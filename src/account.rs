use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use eyre::{eyre, Result};
use rand::Rng;

const LOW_BALANCE_WEI: u128 = 10_000_000_000_000_000; // 0.01 ETH

fn log_pending_tx(label: &str, tx_hash: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true).create(true).open("pending_txs.log")
    {
        let _ = writeln!(f, "[{label}] {tx_hash}");
    }
}

use crate::telegram::TelegramBot;

#[cfg(feature = "gpu")]
use crate::gpu::GpuMiner;

const HASH_CONTRACT_ADDRESS: Address = address!("AC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc");
const EPOCH_BLOCKS: u64 = 100;

sol! {
    #[sol(rpc)]
    contract HashToken {
        function currentDifficulty() external view returns (uint256);
        function totalMints() external view returns (uint256);
        function genesisComplete() external view returns (bool);
        function getChallenge(address miner) external view returns (bytes32);
        function miningState() external view returns (
            uint256 era,
            uint256 reward,
            uint256 difficulty,
            uint256 minted,
            uint256 remaining,
            uint256 epoch,
            uint256 epochBlocksLeft
        );
        function mine(uint256 nonce) external;
        function totalSupply() external view returns (uint256);
    }
}

#[derive(Clone)]
pub struct AccountStats {
    pub label: String,
    pub hashes: u64,
    pub solutions: u64,
    last_update: Instant,
    pub hashrate: f64,
}

impl AccountStats {
    pub fn new(label: String) -> Self {
        AccountStats { label, hashes: 0, solutions: 0, last_update: Instant::now(), hashrate: 0.0 }
    }

    pub fn update_rate(&mut self, new_hashes: u64) {
        let elapsed = self.last_update.elapsed().as_secs_f64().max(0.001);
        self.hashrate = new_hashes as f64 / elapsed;
        self.hashes += new_hashes;
        self.last_update = Instant::now();
    }
}

pub struct AccountConfig {
    pub label: String,
    pub private_key: String,
}

pub struct MiningConfig {
    pub rpc_url: String,
    pub priority_gwei: f64,
    pub max_fee_gwei: f64,
    pub gpu_batch_size: usize,
}

async fn retry<T, E, F, IT>(retries: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> IT,
    IT: std::future::IntoFuture<Output = std::result::Result<T, E>>,
    E: std::fmt::Display,
{
    let mut last_err = eyre!("no attempts");
    for attempt in 0..retries {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = eyre!("{e}");
                if attempt < retries - 1 {
                    tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
                }
            }
        }
    }
    Err(last_err)
}

pub async fn run_account(
    cfg: AccountConfig,
    mining_cfg: Arc<MiningConfig>,
    stats_slot: Arc<Mutex<AccountStats>>,
    telegram: Arc<TelegramBot>,
    #[cfg(feature = "gpu")] gpu: Arc<Mutex<GpuMiner>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    let label = cfg.label.clone();

    let key = cfg.private_key.trim().trim_start_matches("0x");
    if key.len() != 64 {
        eprintln!("❌ [{label}] invalid private key length");
        telegram.notify_error(&label, "Invalid private key length").await;
        return;
    }
    let signer: PrivateKeySigner = match key.parse() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ [{label}] key parse: {e}");
            telegram.notify_error(&label, &e.to_string()).await;
            return;
        }
    };
    let miner_address = signer.address();
    let wallet = EthereumWallet::from(signer);

    let provider = match mining_cfg.rpc_url.parse() {
        Ok(url) => ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(url),
        Err(e) => {
            eprintln!("❌ [{label}] RPC URL: {e}");
            telegram.notify_error(&label, &e.to_string()).await;
            return;
        }
    };

    let contract = HashToken::new(HASH_CONTRACT_ADDRESS, provider.clone());

    // Check genesis
    match retry(3, || async { contract.genesisComplete().call().await }).await {
        Ok(g) if !g._0 => {
            eprintln!("❌ [{label}] Genesis not complete");
            telegram.notify_error(&label, "Genesis not complete — mining closed").await;
            return;
        }
        Err(e) => eprintln!("⚠️ [{label}] genesisComplete: {e}"),
        _ => {}
    }

    println!("✅ [{label}] Ready | {miner_address}");

    let batch_size = mining_cfg.gpu_batch_size as u64;
    let mut pending_hashes: u64 = 0;

    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) { break; }

        let block_num = match retry(3, || provider.get_block_number()).await {
            Ok(n) => n,
            Err(e) => {
                eprintln!("❌ [{label}] block_number: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        let epoch = block_num / EPOCH_BLOCKS;

        let challenge = match retry(3, || async { contract.getChallenge(miner_address).call().await }).await {
            Ok(v) => v._0,
            Err(e) => {
                eprintln!("❌ [{label}] getChallenge: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let difficulty = match retry(3, || async { contract.currentDifficulty().call().await }).await {
            Ok(v) => v._0,
            Err(e) => {
                eprintln!("❌ [{label}] difficulty: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        // Balance check — warn via Telegram if below 0.01 ETH
        match provider.get_balance(miner_address).await {
            Ok(bal) => {
                let bal_u128: u128 = bal.try_into().unwrap_or(u128::MAX);
                if bal_u128 < LOW_BALANCE_WEI {
                    let msg = format!("Saldo rendah: {:.5} ETH — top up segera!", bal_u128 as f64 / 1e18);
                    eprintln!("⚠️ [{label}] {msg}");
                    telegram.notify_error(&label, &msg).await;
                }
            }
            Err(e) => eprintln!("⚠️ [{label}] balance check: {e}"),
        }

        println!("⛏️  [{label}] epoch={epoch}");

        let mut nonce_cursor: u64 = rand::thread_rng().gen();
        let mut found_nonce: Option<u64> = None;
        let mut batch_count: u64 = 0;

        'mining: loop {
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) { break; }

            // Check epoch every 32 GPU batches
            if batch_count > 0 && batch_count % 32 == 0 {
                if let Ok(bn) = provider.get_block_number().await {
                    if bn / EPOCH_BLOCKS != epoch {
                        println!("🔄 [{label}] epoch changed, restarting");
                        break 'mining;
                    }
                }
            }

            #[cfg(feature = "gpu")]
            {
                let gpu_clone = Arc::clone(&gpu);
                let ch = challenge;
                let diff = difficulty;
                let nc = nonce_cursor;

                let batch_result = tokio::task::spawn_blocking(move || {
                    let g = gpu_clone.lock().unwrap();
                    g.mine_batch(ch, diff, nc)
                })
                .await;

                pending_hashes += batch_size;
                batch_count += 1;
                nonce_cursor = nonce_cursor.wrapping_add(batch_size);

                match batch_result {
                    Ok(Ok(Some(n))) => { found_nonce = Some(n); break 'mining; }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => eprintln!("⚠️ [{label}] GPU error: {e}"),
                    Err(e) => eprintln!("⚠️ [{label}] spawn_blocking: {e}"),
                }
            }

            #[cfg(not(feature = "gpu"))]
            {
                use alloy::primitives::keccak256;
                let cpu_batch = 4096u64;
                for i in 0..cpu_batch {
                    let n = nonce_cursor.wrapping_add(i);
                    let mut buf = [0u8; 64];
                    buf[..32].copy_from_slice(challenge.as_slice());
                    buf[32..].copy_from_slice(&U256::from(n).to_be_bytes::<32>());
                    let h = keccak256(buf);
                    if U256::from_be_bytes::<32>(h.0) < difficulty {
                        found_nonce = Some(n);
                        break;
                    }
                }
                pending_hashes += cpu_batch;
                batch_count += 1;
                nonce_cursor = nonce_cursor.wrapping_add(cpu_batch);
                if found_nonce.is_some() { break 'mining; }
                tokio::task::yield_now().await;
            }

            // Flush stats every 4 batches
            if pending_hashes >= batch_size * 4 {
                let mut s = stats_slot.lock().unwrap();
                s.update_rate(pending_hashes);
                pending_hashes = 0;
            }
        }

        if pending_hashes > 0 {
            let mut s = stats_slot.lock().unwrap();
            s.update_rate(pending_hashes);
            pending_hashes = 0;
        }

        if shutdown.load(std::sync::atomic::Ordering::Relaxed) { break; }

        let Some(nonce) = found_nonce else { continue };

        println!("🎉 [{label}] Found nonce={nonce}");

        // Dynamic gas estimation — query current network gas price, cap at config max
        let config_priority = (mining_cfg.priority_gwei * 1e9) as u128;
        let config_max      = (mining_cfg.max_fee_gwei  * 1e9) as u128;
        let (priority_wei, max_fee_wei) = match provider.get_gas_price().await {
            Ok(current_price) => {
                // max_fee = current_price * 2 gives 1-2 block headroom for base fee changes
                let max_fee = (current_price * 2 + config_priority)
                    .min(config_max)
                    .max(config_priority + 1_000_000_000); // always at least priority + 1 gwei
                println!("⛽ [{label}] gas price={:.1} gwei → maxFee={:.1} gwei",
                    current_price as f64 / 1e9, max_fee as f64 / 1e9);
                (config_priority, max_fee)
            }
            Err(_) => {
                eprintln!("⚠️ [{label}] gas price fetch failed, using config defaults");
                (config_priority, config_max)
            }
        };

        // ── Submit TX ────────────────────────────────────────────────────────
        match contract
            .mine(U256::from(nonce))
            .max_priority_fee_per_gas(priority_wei)
            .max_fee_per_gas(max_fee_wei)
            .send()
            .await
        {
            Ok(pending) => {
                let tx_hash = *pending.tx_hash();
                println!("📋 [{label}] TX: {tx_hash}");
                match pending.with_required_confirmations(1).get_receipt().await {
                    Ok(receipt) if receipt.status() => {
                        let block = receipt.block_number.unwrap_or_default();
                        println!("✅ [{label}] Confirmed block {block}");
                        stats_slot.lock().unwrap().solutions += 1;
                        telegram.notify_solution(&label, &nonce.to_string(), &tx_hash.to_string(), block).await;
                    }
                    Ok(_) => {
                        // Revert — retry once with gas bumped 50%
                        eprintln!("❌ [{label}] TX reverted — retry +50% gas");
                        let retry_priority = priority_wei * 3 / 2;
                        let retry_max = (max_fee_wei * 3 / 2).min(config_max * 2);
                        match contract
                            .mine(U256::from(nonce))
                            .max_priority_fee_per_gas(retry_priority)
                            .max_fee_per_gas(retry_max)
                            .send()
                            .await
                        {
                            Ok(retry_pending) => {
                                let retry_hash = *retry_pending.tx_hash();
                                println!("📋 [{label}] Retry TX: {retry_hash}");
                                match retry_pending.with_required_confirmations(1).get_receipt().await {
                                    Ok(r) if r.status() => {
                                        let block = r.block_number.unwrap_or_default();
                                        println!("✅ [{label}] Retry confirmed block {block}");
                                        stats_slot.lock().unwrap().solutions += 1;
                                        telegram.notify_solution(&label, &nonce.to_string(), &retry_hash.to_string(), block).await;
                                    }
                                    Ok(_) => {
                                        eprintln!("❌ [{label}] Retry also reverted");
                                        telegram.notify_error(&label, "TX reverted 2x — epoch mungkin sudah berubah").await;
                                    }
                                    Err(e) => {
                                        eprintln!("❌ [{label}] Retry receipt: {e}");
                                        log_pending_tx(&label, &retry_hash.to_string());
                                        telegram.notify_error(&label, &format!("Retry receipt timeout. TX: {retry_hash}")).await;
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("❌ [{label}] Retry send: {e}");
                                telegram.notify_error(&label, &format!("Retry TX send: {e}")).await;
                            }
                        }
                    }
                    Err(e) => {
                        // Receipt timeout — save TX hash for manual recovery
                        eprintln!("❌ [{label}] Receipt timeout: {e}");
                        log_pending_tx(&label, &tx_hash.to_string());
                        telegram.notify_error(&label, &format!(
                            "Receipt timeout. TX disimpan di pending_txs.log\nTX: {tx_hash}"
                        )).await;
                    }
                }
            }
            Err(e) => {
                eprintln!("❌ [{label}] TX send: {e}");
                telegram.notify_error(&label, &format!("TX send gagal: {e}")).await;
            }
        }
    }
}
