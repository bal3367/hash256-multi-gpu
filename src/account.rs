use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use eyre::{eyre, Result};
use rand::Rng;

use crate::telegram::{StatEntry, TelegramBot};

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

async fn retry<T, E, F, Fut>(retries: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
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
    match retry(3, || contract.genesisComplete().call()).await {
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

        let challenge = match retry(3, || contract.getChallenge(miner_address).call()).await {
            Ok(v) => v._0,
            Err(e) => {
                eprintln!("❌ [{label}] getChallenge: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let difficulty = match retry(3, || contract.currentDifficulty().call()).await {
            Ok(v) => v._0,
            Err(e) => {
                eprintln!("❌ [{label}] difficulty: {e}");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

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

        let priority_wei = (mining_cfg.priority_gwei * 1e9) as u128;
        let max_fee_wei  = (mining_cfg.max_fee_gwei  * 1e9) as u128;

        let tx = contract
            .mine(U256::from(nonce))
            .max_priority_fee_per_gas(priority_wei)
            .max_fee_per_gas(max_fee_wei);

        match tx.send().await {
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
                        eprintln!("❌ [{label}] TX reverted");
                        telegram.notify_error(&label, "Transaction reverted").await;
                    }
                    Err(e) => {
                        eprintln!("❌ [{label}] receipt: {e}");
                        telegram.notify_error(&label, &format!("Receipt: {e}")).await;
                    }
                }
            }
            Err(e) => {
                eprintln!("❌ [{label}] send: {e}");
                telegram.notify_error(&label, &format!("TX send: {e}")).await;
            }
        }
    }
}
