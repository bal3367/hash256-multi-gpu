mod account;
mod telegram;
#[cfg(feature = "gpu")]
mod gpu;

use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex};
use std::time::{Duration, Instant};

use eyre::{eyre, Result};
use serde::Deserialize;

use account::{AccountConfig, AccountStats, MiningConfig};
use telegram::{
    format_accounts, format_stats, format_status,
    main_menu_keyboard, StatEntry, TelegramBot, TgUpdate,
};

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
    let telegram_chat_id_str = cfg.telegram_chat_id
        .or_else(|| std::env::var("TELEGRAM_CHAT_ID").ok())
        .unwrap_or_default();

    let has_telegram = !telegram_token.is_empty() && !telegram_chat_id_str.is_empty();
    let configured_chat_id: i64 = if has_telegram {
        match telegram_chat_id_str.parse() {
            Ok(id) => id,
            Err(_) => {
                eprintln!("❌ telegram_chat_id bukan angka valid: '{telegram_chat_id_str}'");
                eprintln!("   Contoh benar: \"telegram_chat_id\": \"5051864490\"");
                std::process::exit(1);
            }
        }
    } else {
        0
    };
    let telegram = Arc::new(TelegramBot::new(telegram_token, telegram_chat_id_str));

    println!("🔐 HASH Multi-Account GPU Miner v0.3");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("📋 Accounts  : {}", cfg.accounts.len());
    println!("⛽ RPC URL   : {rpc_url}");

    // Init GPU
    #[cfg(feature = "gpu")]
    let (gpu, gpu_name) = {
        match gpu::GpuMiner::new(cfg.gpu_batch_size) {
            Ok(g) => {
                let name = g.device_name().to_string();
                println!("🎮 GPU        : {name}");
                println!("📦 Batch size : {} nonces/dispatch", g.batch_size());
                match g.self_test() {
                    Ok(()) => println!("✅ GPU self-test passed"),
                    Err(e) => eprintln!("⚠️  GPU self-test failed: {e}"),
                }
                (Arc::new(Mutex::new(g)), name)
            }
            Err(e) => {
                eprintln!("⚠️  GPU init failed: {e}");
                eprintln!("   Check that OpenCL drivers are installed.");
                std::process::exit(1);
            }
        }
    };

    #[cfg(not(feature = "gpu"))]
    let gpu_name = "CPU only".to_string();

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

    let session_start = Arc::new(Instant::now());

    // Build per-account stats slots
    let all_stats: Vec<Arc<Mutex<AccountStats>>> = cfg.accounts
        .iter()
        .map(|a| Arc::new(Mutex::new(AccountStats::new(a.label.clone()))))
        .collect();

    // Notify Telegram: start + register bot commands
    if has_telegram {
        let labels: Vec<String> = cfg.accounts.iter().map(|a| a.label.clone()).collect();
        telegram.set_commands().await;
        telegram.notify_start(&labels, &gpu_name).await;
        println!("📬 Telegram bot commands registered");
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
                acc_cfg, mc, stats, tg,
                #[cfg(feature = "gpu")]
                gpu_ref,
                sd,
            )
            .await
        });
        handles.push(handle);
    }

    // Periodic stats reporter
    let stats_reporter = {
        let all_stats = all_stats.clone();
        let shutdown = Arc::clone(&shutdown);
        let interval = cfg.stats_interval_secs;
        let ss = Arc::clone(&session_start);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                if shutdown.load(Ordering::Relaxed) { break; }

                let entries = collect_stats(&all_stats);
                let total_hr: f64 = entries.iter().map(|e| e.hashrate).sum();
                let total_sol: u64 = entries.iter().map(|e| e.solutions).sum();
                let elapsed = ss.elapsed().as_secs_f64();
                println!("\n📊 Stats ({:.0}s elapsed):", elapsed);
                for e in &entries {
                    println!("   {:20} | {:8.2} MH/s | {} solutions",
                        e.label, e.hashrate / 1_000_000.0, e.solutions);
                }
                println!("   Total: {:.2} MH/s | {} solutions\n", total_hr / 1_000_000.0, total_sol);

                // Stats otomatis ke Telegram dinonaktifkan — gunakan /status untuk cek manual
            }
        })
    };

    // Telegram command listener (long-polling)
    let cmd_listener = if has_telegram {
        let tg = Arc::clone(&telegram);
        let all_stats = all_stats.clone();
        let gn = gpu_name.clone();
        let shutdown = Arc::clone(&shutdown);
        let ss = Arc::clone(&session_start);
        Some(tokio::spawn(async move {
            let mut offset = 0i64;
            loop {
                if shutdown.load(Ordering::Relaxed) { break; }
                let updates = tg.poll_updates(offset).await;
                for upd in updates {
                    offset = upd.update_id + 1;
                    handle_update(&tg, &upd, &all_stats, ss.elapsed().as_secs(), configured_chat_id, &gn, &shutdown).await;
                }
            }
        }))
    } else {
        None
    };

    for h in handles {
        let _ = h.await;
    }
    stats_reporter.abort();
    if let Some(t) = cmd_listener { t.abort(); }

    if has_telegram {
        telegram.notify_stopped().await;
    }
    println!("✅ All miners stopped.");
    Ok(())
}

// ── Collect stats snapshot ────────────────────────────────────────────────

fn collect_stats(all_stats: &[Arc<Mutex<AccountStats>>]) -> Vec<StatEntry> {
    all_stats.iter().map(|s| {
        let s = s.lock().unwrap();
        StatEntry { label: s.label.clone(), hashrate: s.hashrate, solutions: s.solutions }
    }).collect()
}

// ── Telegram update dispatcher ────────────────────────────────────────────

async fn handle_update(
    tg: &TelegramBot,
    upd: &TgUpdate,
    all_stats: &[Arc<Mutex<AccountStats>>],
    elapsed_secs: u64,
    configured_chat_id: i64,
    gpu_name: &str,
    shutdown: &Arc<AtomicBool>,
) {
    // Extract chat_id and text from message or callback_query
    let (chat_id, text, callback_id) = if let Some(msg) = &upd.message {
        (msg.chat.id, msg.text.as_deref().unwrap_or("").to_string(), None)
    } else if let Some(cb) = &upd.callback_query {
        let cid = cb.from.id;
        let data = cb.data.as_deref().unwrap_or("").to_string();
        (cid, data, Some(cb.id.as_str()))
    } else {
        return;
    };

    // Security: only respond to configured chat
    if chat_id != configured_chat_id {
        return;
    }

    // Answer callback to clear button loading spinner
    if let Some(cb_id) = callback_id {
        tg.answer_callback(cb_id).await;
    }

    let cmd = text.trim().split_whitespace().next().unwrap_or("");
    let cmd = cmd.trim_start_matches('/');
    // Strip @BotName suffix (e.g. "/status@MyBot" → "status")
    let cmd = cmd.split('@').next().unwrap_or(cmd);

    let entries = collect_stats(all_stats);

    match cmd {
        "start" | "help" => {
            let text = format!(
                "🚀 *HASH Multi-Account Miner*\n\
                 GPU: `{gpu_name}`\n\
                 Pilih perintah di bawah atau ketik langsung:\n\n\
                 /status — status ringkas\n\
                 /accounts — daftar semua akun\n\
                 /stats — stats lengkap\n\
                 /stop — hentikan miner\n\
                 /help — menu ini"
            );
            tg.send_to(chat_id, &text, Some(main_menu_keyboard())).await;
        }
        "status" | "cmd_status" => {
            let text = format_status(&entries, elapsed_secs, gpu_name);
            tg.send_to(chat_id, &text, Some(main_menu_keyboard())).await;
        }
        "accounts" | "cmd_accounts" => {
            let text = format_accounts(&entries);
            tg.send_to(chat_id, &text, Some(main_menu_keyboard())).await;
        }
        "stats" | "cmd_stats" => {
            let text = format_stats(&entries, elapsed_secs);
            tg.send_to(chat_id, &text, Some(main_menu_keyboard())).await;
        }
        "stop" | "cmd_stop" => {
            tg.send_to(chat_id, "🛑 *Menghentikan semua miner...*\nTunggu beberapa detik.", None).await;
            shutdown.store(true, Ordering::Relaxed);
        }
        _ => {
            tg.send_to(chat_id, "❓ Perintah tidak dikenal. Ketik /help untuk daftar perintah.", None).await;
        }
    }
}
