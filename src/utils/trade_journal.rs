use chrono::Utc;
use serde_json::{json, Value};
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::Path;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct TradeJournal {
    path: String,
    slot_name: String,
}

impl TradeJournal {
    pub fn new(slot_name: &str) -> Self {
        let path = std::env::var("TRADE_LOG_FILE")
            .unwrap_or_else(|_| "logs/trade_events.jsonl".to_string());
        Self {
            path,
            slot_name: slot_name.to_string(),
        }
    }

    pub fn log_event(&self, event: &str, market: &str, payload: Value) {
        let line = json!({
            "ts": Utc::now().to_rfc3339(),
            "slot": self.slot_name,
            "event": event,
            "market": market,
            "data": payload,
        });
        self.append_jsonl(&line);
    }

    fn append_jsonl(&self, value: &Value) {
        let path = Path::new(&self.path);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = create_dir_all(parent) {
                    warn!(error = %e, path = %self.path, "Failed to create trade log directory");
                    return;
                }
            }
        }

        let mut file = match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => file,
            Err(e) => {
                warn!(error = %e, path = %self.path, "Failed to open trade log file");
                return;
            }
        };

        if let Err(e) = writeln!(file, "{}", value) {
            warn!(error = %e, path = %self.path, "Failed to append trade log line");
        }
    }
}
