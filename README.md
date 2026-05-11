# HASH Token Multi-Account GPU Miner

Rust miner untuk HASH token di Ethereum — mendukung **10+ akun secara bersamaan** dengan **GPU acceleration (OpenCL)** dan **monitoring via Telegram bot**.

## Fitur

- Multi-akun: jalankan 10+ wallet sekaligus dari satu file konfigurasi
- GPU mining via OpenCL (NVIDIA/AMD) — Keccak-256 kernel yang dioptimasi
- CPU fallback otomatis jika GPU tidak tersedia
- Telegram bot: notifikasi start, solusi ditemukan, stats berkala, error
- Retry otomatis untuk kegagalan RPC
- Verifikasi CPU setelah GPU menemukan solusi (belt-and-braces)

---

## Persyaratan

| Kebutuhan | Keterangan |
|-----------|-----------|
| Rust ≥ 1.75 | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| OpenCL runtime | Ubuntu: `sudo apt install ocl-icd-opencl-dev` |
| Driver GPU NVIDIA | `sudo apt install nvidia-opencl-dev` |
| ETH di setiap wallet | Untuk gas fee — rekomendasi 0.05 ETH/akun |
| RPC endpoint | Gunakan Alchemy/Infura untuk performa lebih stabil |

---

## Instalasi & Setup

### 1. Clone dan masuk ke direktori

```bash
git clone https://github.com/bal3367/hash256-multi-gpu.git
cd hash256-multi-gpu
```

### 2. Buat file konfigurasi

```bash
cp accounts.json.example accounts.json
nano accounts.json
```

Isi `accounts.json` dengan private key dan konfigurasi:

```json
{
  "rpc_url": "https://eth.llamarpc.com",
  "accounts": [
    { "label": "Akun 1", "private_key": "0x..." },
    { "label": "Akun 2", "private_key": "0x..." }
  ],
  "gpu_batch_size": 4194304,
  "priority_gwei": 5.0,
  "max_fee_gwei": 100.0,
  "telegram_token": "TOKEN_DARI_BOTFATHER",
  "telegram_chat_id": "CHAT_ID_KAMU",
  "stats_interval_secs": 60
}
```

> **Keamanan:** `accounts.json` sudah ada di `.gitignore`. Jangan pernah commit file ini.

### 3. Setup Telegram Bot (opsional tapi disarankan)

1. Chat `@BotFather` di Telegram → `/newbot` → ikuti instruksi → salin token
2. Kirim pesan ke bot kamu, lalu buka:
   `https://api.telegram.org/botTOKEN_KAMU/getUpdates`
3. Salin `chat.id` dari respons JSON ke field `telegram_chat_id`

### 4. Install OpenCL (Ubuntu/Debian)

```bash
sudo apt update
sudo apt install ocl-icd-opencl-dev nvidia-opencl-dev -y

# Verifikasi GPU terdeteksi:
clinfo | head -20
```

### 5. Build

```bash
# Dengan GPU support (direkomendasikan)
cargo build --release

# Tanpa GPU (CPU only)
cargo build --release --no-default-features
```

Binary hasil build ada di `target/release/hash-miner-rs`

### 6. Jalankan

```bash
# Gunakan accounts.json di direktori yang sama
./target/release/hash-miner-rs

# Atau tentukan path config sendiri
./target/release/hash-miner-rs --config /path/to/accounts.json

# Jalankan di background dengan screen/tmux
screen -S miner
./target/release/hash-miner-rs
# Ctrl+A, D untuk detach
```

---

## Konfigurasi Detail

| Field | Default | Keterangan |
|-------|---------|-----------|
| `rpc_url` | `https://eth.llamarpc.com` | RPC endpoint Ethereum |
| `accounts` | — | List akun (label + private key) |
| `gpu_batch_size` | `4194304` (4M) | Nonces per GPU dispatch |
| `priority_gwei` | `5.0` | EIP-1559 priority fee (tip untuk miner) |
| `max_fee_gwei` | `100.0` | Batas maksimum gas fee |
| `telegram_token` | — | Token bot dari @BotFather |
| `telegram_chat_id` | — | Chat ID tujuan notifikasi |
| `stats_interval_secs` | `60` | Interval laporan stats ke Telegram |

### Tips GPU Batch Size

| GPU | Batch size yang disarankan |
|-----|--------------------------|
| RTX 3060 / 4060 | `4194304` (4M) |
| RTX 3080 / 4080 | `16777216` (16M) |
| RTX 4090 / PRO 6000 WS | `67108864` (64M) |

---

## Monitoring Telegram

Bot akan mengirim notifikasi:

```
🚀 HASH Miner Started
GPU: NVIDIA RTX PRO 6000 WS
Accounts: 10
  1. Akun 1
  2. Akun 2
  ...

📊 Stats Update (1m 0s)
  Akun 1: 45.2 MH/s | 2 ✅
  Akun 2: 44.8 MH/s | 1 ✅
Total: 452.0 MH/s | 23 solutions

✅ Solution Found!
Account: Akun 3
Nonce: 12345678
TX: 0xabcdef...
Block: 12345678
🔗 https://etherscan.io/tx/0xabcdef...
```

---

## Troubleshooting

**`no OpenCL device found`**
```bash
sudo apt install ocl-icd-opencl-dev nvidia-opencl-dev
clinfo | head -20
```

**`Genesis not complete`**
Mining belum dibuka di kontrak. Cek status di https://hash256.org/mine

**`Transaction reverted`**
- Pastikan ada cukup ETH untuk gas di setiap wallet
- Difficulty mungkin berubah antar epoch; miner akan otomatis restart round

**GPU utilization rendah**
Naikkan `gpu_batch_size` sampai `nvidia-smi` menunjukkan >80% utilization.

---

## Keamanan

- `accounts.json` ada di `.gitignore` — JANGAN di-commit
- Private key hanya dibaca dari file lokal, tidak pernah dikirim ke mana pun
- Disarankan pakai RPC endpoint pribadi (Alchemy/Infura) bukan public endpoint

---

## Kontrak

- Address: `0xAC7b5d06fa1e77D08aea40d46cB7C5923A87A0cc` (Ethereum Mainnet)
- Algoritma: `keccak256(abi.encode(bytes32 challenge, uint256 nonce)) < difficulty`

## License

MIT
