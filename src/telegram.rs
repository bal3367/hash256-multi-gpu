use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

// ── Telegram API types ────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TgResponse<T> {
    result: Vec<T>,
}

#[derive(Deserialize)]
pub struct TgUpdate {
    pub update_id: i64,
    pub message: Option<TgMessage>,
    pub callback_query: Option<TgCallback>,
}

#[derive(Deserialize)]
pub struct TgMessage {
    pub chat: TgChat,
    pub text: Option<String>,
}

#[derive(Deserialize)]
pub struct TgCallback {
    pub id: String,
    pub from: TgChat,
    pub data: Option<String>,
}

#[derive(Deserialize)]
pub struct TgChat {
    pub id: i64,
}

// ── TelegramBot ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TelegramBot {
    pub token: String,
    pub chat_id: String,
    client: reqwest::Client,
    poll_client: reqwest::Client,
}

impl TelegramBot {
    pub fn new(token: impl Into<String>, chat_id: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        // Long-poll timeout must exceed Telegram's 30 s server-side wait
        let poll_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(40))
            .build()
            .unwrap_or_default();
        TelegramBot { token: token.into(), chat_id: chat_id.into(), client, poll_client }
    }

    fn api(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    // ── Push to configured chat ───────────────────────────────────────────

    pub async fn send(&self, text: &str) {
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });
        if let Err(e) = self.client.post(self.api("sendMessage")).json(&body).send().await {
            eprintln!("[Telegram] send failed: {e}");
        }
    }

    // ── Send to any chat_id (for command replies) ─────────────────────────

    pub async fn send_to(&self, chat_id: i64, text: &str, keyboard: Option<Value>) {
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });
        if let Some(kb) = keyboard {
            body["reply_markup"] = kb;
        }
        if let Err(e) = self.client.post(self.api("sendMessage")).json(&body).send().await {
            eprintln!("[Telegram] send_to failed: {e}");
        }
    }

    // ── Register bot commands (shows in Telegram / menu) ──────────────────

    pub async fn set_commands(&self) {
        let body = serde_json::json!({
            "commands": [
                {"command": "start",    "description": "Menu utama"},
                {"command": "status",   "description": "Status ringkas miner"},
                {"command": "accounts", "description": "Daftar semua akun & hashrate"},
                {"command": "stats",    "description": "Stats lengkap semua akun"},
                {"command": "help",     "description": "Bantuan & daftar perintah"},
            ]
        });
        if let Err(e) = self.client.post(self.api("setMyCommands")).json(&body).send().await {
            eprintln!("[Telegram] setMyCommands failed: {e}");
        }
    }

    // ── Long-poll for new updates ─────────────────────────────────────────

    pub async fn poll_updates(&self, offset: i64) -> Vec<TgUpdate> {
        let body = serde_json::json!({
            "offset": offset,
            "timeout": 30,
            "allowed_updates": ["message", "callback_query"]
        });
        match self.poll_client
            .post(self.api("getUpdates"))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => resp.json::<TgResponse<TgUpdate>>().await
                .map(|r| r.result)
                .unwrap_or_default(),
            Err(_) => vec![],
        }
    }

    // ── Answer callback query (clears loading spinner on button) ──────────

    pub async fn answer_callback(&self, callback_id: &str) {
        let body = serde_json::json!({"callback_query_id": callback_id});
        let _ = self.client.post(self.api("answerCallbackQuery")).json(&body).send().await;
    }

    // ── Notification helpers ──────────────────────────────────────────────

    pub async fn notify_start(&self, account_labels: &[String], gpu_name: &str) {
        let accounts_list = account_labels
            .iter()
            .enumerate()
            .map(|(i, l)| format!("  {}. {}", i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        self.send(&format!(
            "🚀 *HASH Miner Started*\n\
             GPU: `{gpu_name}`\n\
             Accounts: {}\n\n\
             {accounts_list}\n\n\
             Ketik /help untuk daftar perintah.",
            account_labels.len()
        ))
        .await;
    }

    pub async fn notify_solution(&self, label: &str, nonce: &str, tx_hash: &str, block: u64) {
        self.send(&format!(
            "✅ *Solution Found!*\n\
             Account: `{label}`\n\
             Nonce: `{nonce}`\n\
             TX: `{tx_hash}`\n\
             Block: `{block}`\n\
             🔗 https://etherscan.io/tx/{tx_hash}"
        ))
        .await;
    }

    pub async fn notify_stats(&self, entries: &[StatEntry], elapsed_secs: u64) {
        if entries.is_empty() { return; }
        let text = format_stats(entries, elapsed_secs);
        self.send(&text).await;
    }

    pub async fn notify_error(&self, label: &str, error: &str) {
        self.send(&format!("❌ *Error* — `{label}`\n`{error}`")).await;
    }

    pub async fn notify_stopped(&self) {
        self.send("🛑 *Miner stopped*").await;
    }
}

// ── Keyboard builders ─────────────────────────────────────────────────────

pub fn main_menu_keyboard() -> Value {
    serde_json::json!({
        "inline_keyboard": [
            [
                {"text": "📊 Status",       "callback_data": "cmd_status"},
                {"text": "👥 Accounts",     "callback_data": "cmd_accounts"},
            ],
            [
                {"text": "📈 Stats Lengkap","callback_data": "cmd_stats"},
                {"text": "🔄 Refresh",      "callback_data": "cmd_status"},
            ]
        ]
    })
}

// ── Shared stats formatter (used by both push and command replies) ─────────

pub fn format_stats(entries: &[StatEntry], elapsed_secs: u64) -> String {
    let rows: Vec<String> = entries
        .iter()
        .map(|e| format!("  `{}`: {:.1} MH/s | {} ✅", e.label, e.hashrate / 1_000_000.0, e.solutions))
        .collect();
    let total_hr: f64 = entries.iter().map(|e| e.hashrate).sum();
    let total_sol: u64 = entries.iter().map(|e| e.solutions).sum();
    let mins = elapsed_secs / 60;
    let secs = elapsed_secs % 60;
    format!(
        "📊 *Stats Update* ({}m {}s)\n{}\n─────────────────────\nTotal: `{:.1} MH/s` | `{}` solutions",
        mins, secs,
        rows.join("\n"),
        total_hr / 1_000_000.0,
        total_sol
    )
}

pub fn format_status(entries: &[StatEntry], elapsed_secs: u64, gpu_name: &str) -> String {
    let total_hr: f64 = entries.iter().map(|e| e.hashrate).sum();
    let total_sol: u64 = entries.iter().map(|e| e.solutions).sum();
    let h = elapsed_secs / 3600;
    let m = (elapsed_secs % 3600) / 60;
    let s = elapsed_secs % 60;
    format!(
        "📊 *Status Miner*\n\
         ⏱ Uptime: `{h}j {m}m {s}s`\n\
         🎮 GPU: `{gpu_name}`\n\
         👥 Akun aktif: `{}/{}`\n\
         ⚡ Total: `{:.1} MH/s`\n\
         ✅ Total solusi: `{}`",
        entries.len(), entries.len(),
        total_hr / 1_000_000.0,
        total_sol
    )
}

pub fn format_accounts(entries: &[StatEntry]) -> String {
    if entries.is_empty() {
        return "👥 *Accounts*\nBelum ada akun aktif.".to_string();
    }
    let rows: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!("{}. `{}` | {:.1} MH/s | {} ✅",
                i + 1, e.label, e.hashrate / 1_000_000.0, e.solutions)
        })
        .collect();
    format!("👥 *Daftar Akun* ({} aktif)\n\n{}", entries.len(), rows.join("\n"))
}

// ── StatEntry (public, used by main.rs) ──────────────────────────────────

pub struct StatEntry {
    pub label: String,
    pub hashrate: f64,
    pub solutions: u64,
}
