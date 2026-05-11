mod account;
mod telegram;
#[cfg(feature = "gpu")]
mod gpu;

use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use serde::Deserialize;

use account::{AccountConfig, AccountStats, MiningConfig};
use telegram::{StatEntry, TelegramBot};

#[derive(Deserialize)]
struct Config {
    rpc_url: Option<String>,
    accounts: Vec<RawAccount>,
    gpu_batch_size: Option<usize>,
    #[serde(default = "default_priority")]
    priority_gwei: f64,
    #[serde(default = "default_max_fee")]
    max_fee_gwei: f64,
    telegram_token: Option<String>,
    telegram_chat_id: Option<String>,
    #[serde(default = "default_stats_interval")]
    stats_interval_secs: u64,
}

#[derive(Deserialize)]
struct RawAccount {
    label: String,
    private_key: String,
}

fn default_priority() -> f64 { 5.0 }
fn default_max_fee() -> f64 { 100.0 }
fn default_stats_interval() -> u64 { 60 }

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    // Determine config file path (--config path or default accounts.json)
    let args: Vec<String> = std::env::args().collect();
    let config_path = args.windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "accounts.json".to_string());

    if !std::path::Path::new(&config_path).exists() {
        eprintln!("❌ Config file not found: {config_path}");
        eprintln!("   Copy accounts.json.example → accounts.json and fill in your keys.");
        std::process::exit(1);
    }

    let raw = std::fs::read_to_string(&config_path)
        .map_err(|e| eyre!("Cannot read {config_path}: {e}"))?;
    let cfg: Config = serde_json::from_str(&raw)
        .map_err(|e| eyre!("Invalid JSON in {config_path}: {e}"))?;

    if cfg.accounts.is_empty() {
        return Err(eyre!("No accounts configured in {config_path}"));
    }

    let rpc_url = cfg.rpc_url
        .or_else(|| std::env::var("RPC_URL").ok())
        .unwrap_or_else(|| "https://eth.llamarpc.com".to_string());

    let telegram_token = cfg.telegram_token
        .or_else(|| std::env::var("TELEGRAM_TOKEN").ok())
        .unwrap_or_default();
    let telegram_chat_id = cfg.telegram_chat_id
        .or_else(|| std::env::var("TELEGRAM_CHAT_ID").ok())
        .unwrap_or_default();

    let has_telegram = !telegram_token.is_empty() && !telegram_chat_id.is_empty();
    let telegram = Arc::new(TelegramBot::new(telegram_token, telegram_chat_id));

    println!("🔐 HASH Multi-Account GPU Miner v0.2");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📋 Accounts  : {}", cfg.accounts.len());
    println!("⛽ RPC URL   : {rpc_url}");

    // Init GPU
    #[cfg(feature = "gpu")]
    let gpu = {
        match gpu::GpuMiner::new(cfg.gpu_batch_size) {
            Ok(g) => {
                println!("🎮 GPU        : {}", g.device_name());
                println!("📦 Batch size : {} nonces/dispatch", g.batch_size());
                match g.self_test() {
                    Ok(()) => println!("✅ GPU self-test passed"),
                    Err(e) => {
                        eprintln!("❌ GPU self-test failed: {e}");
                        eprintln!("   Falling back to CPU mining");
                    }
                }
                Arc::new(Mutex::new(g))
            }
            Err(e) => {
                eprintln!("⚠️  GPU init failed: {e}");
                eprintln!("   Check that OpenCL drivers are installed.");
                std::process::exit(1);
            }
        }
    };

    let batch_size = cfg.gpu_batch_size.unwrap_or(1 << 22);
    let mining_cfg = Arc::new(MiningConfig {
        rpc_url: rpc_url.clone(),
        priority_gwei: cfg.priority_gwei,
        max_fee_gwei: cfg.max_fee_gwei,
        gpu_batch_size: batch_size,
    });

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            println!("\n🛑 Stopping miners...");
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    // Build per-account stats slots
    let all_stats: Vec<Arc<Mutex<AccountStats>>> = cfg.accounts
        .iter()
        .map(|a| Arc::new(Mutex::new(AccountStats::new(a.label.clone()))))
        .collect();

    // Notify Telegram: start
    if has_telegram {
        let labels: Vec<String> = cfg.accounts.iter().map(|a| a.label.clone()).collect();
        #[cfg(feature = "gpu")]
        let gpu_name = {
            let g = gpu.lock().unwrap();
            g.device_name().to_string()
        };
        #[cfg(not(feature = "gpu"))]
        let gpu_name = "CPU only".to_string();
        telegram.notify_start(&labels, &gpu_name).await;
    }

    // Spawn per-account tasks
    let mut handles = Vec::new();
    for (raw_acc, stats_slot) in cfg.accounts.into_iter().zip(all_stats.iter()) {
        let acc_cfg = AccountConfig { label: raw_acc.label, private_key: raw_acc.private_key };
        let mc = Arc::clone(&mining_cfg);
        let tg = Arc::clone(&telegram);
        let stats = Arc::clone(stats_slot);
        let sd = Arc::clone(&shutdown);

        #[cfg(feature = "gpu")]
        let gpu_ref = Arc::clone(&gpu);

        let handle = tokio::spawn(async move {
            account::run_account(
                acc_cfg,
                mc,
                stats,
                tg,
                #[cfg(feature = "gpu")]
                gpu_ref,
                sd,
            )
            .await
        });
        handles.push(handle);
    }

    // Stats reporter
    let stats_reporter = {
        let all_stats = all_stats.clone();
        let telegram = Arc::clone(&telegram);
        let shutdown = Arc::clone(&shutdown);
        let interval = cfg.stats_interval_secs;
        let session_start = Instant::now();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if shutdown.load(Ordering::Relaxed) { break; }

                let entries: Vec<StatEntry> = all_stats
                    .iter()
                    .map(|s| {
                        let s = s.lock().unwrap();
                        StatEntry { label: s.label.clone(), hashrate: s.hashrate, solutions: s.solutions }
                    })
                    .collect();

                // Console summary
                let total_hr: f64 = entries.iter().map(|e| e.hashrate).sum();
                let total_sol: u64 = entries.iter().map(|e| e.solutions).sum();
                println!("\n📊 Stats ({:.0}s elapsed):", session_start.elapsed().as_secs_f64());
                for e in &entries {
                    println!("   {:20} | {:8.2} MH/s | {} solutions",
                        e.label, e.hashrate / 1_000_000.0, e.solutions);
                }
                println!("   Total: {:.2} MH/s | {} solutions\n", total_hr / 1_000_000.0, total_sol);

                if has_telegram {
                    telegram.notify_stats(&entries, session_start.elapsed().as_secs()).await;
                }
            }
        })
    };

    // Wait for all account tasks
    for h in handles {
        let _ = h.await;
    }
    stats_reporter.abort();

    if has_telegram {
        telegram.notify_stopped().await;
    }
    println!("✅ All miners stopped.");
    Ok(())
}
