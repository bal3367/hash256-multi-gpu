use std::time::Duration;

#[derive(Clone)]
pub struct TelegramBot {
    token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl TelegramBot {
    pub fn new(token: impl Into<String>, chat_id: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        TelegramBot { token: token.into(), chat_id: chat_id.into(), client }
    }

    pub async fn send(&self, text: &str) {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });
        if let Err(e) = self.client.post(&url).json(&body).send().await {
            eprintln!("[Telegram] send failed: {e}");
        }
    }

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
             {accounts_list}",
            account_labels.len()
        ))
        .await;
    }

    pub async fn notify_solution(
        &self,
        label: &str,
        nonce: &str,
        tx_hash: &str,
        block: u64,
    ) {
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
        if entries.is_empty() {
            return;
        }
        let rows: Vec<String> = entries
            .iter()
            .map(|e| {
                format!(
                    "  `{}`: {:.1} MH/s | {} ✅",
                    e.label,
                    e.hashrate / 1_000_000.0,
                    e.solutions
                )
            })
            .collect();
        let total_hr: f64 = entries.iter().map(|e| e.hashrate).sum();
        let total_sol: u64 = entries.iter().map(|e| e.solutions).sum();
        let mins = elapsed_secs / 60;
        let secs = elapsed_secs % 60;
        self.send(&format!(
            "📊 *Stats Update* ({}m {}s)\n\
             {}\n\
             ─────────────────────\n\
             Total: `{:.1} MH/s` | `{}` solutions",
            mins,
            secs,
            rows.join("\n"),
            total_hr / 1_000_000.0,
            total_sol
        ))
        .await;
    }

    pub async fn notify_error(&self, label: &str, error: &str) {
        self.send(&format!("❌ *Error* — `{label}`\n`{error}`")).await;
    }

    pub async fn notify_stopped(&self) {
        self.send("🛑 *Miner stopped*").await;
    }
}

pub struct StatEntry {
    pub label: String,
    pub hashrate: f64,
    pub solutions: u64,
}
