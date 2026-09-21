use serde_json::{Map, Value};
use std::env;
use std::fs;
use std::io::Write;

pub(crate) fn audit(name: &str, args: &Map<String, Value>, outcome: &str) {
    // Audit writes are sync open+write per
    // tool call which blocks the loop thread on fs. Gate behind DEX_AUDIT=1
    // for strict auditing, otherwise skip (session.jsonl already journals).
    if std::env::var("DEX_AUDIT").as_deref() != Ok("1") {
        return;
    }
    let Some(base) = crate::runtime::logging::data_home() else {
        return;
    };
    let path = base.join("dex/audit.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "cwd": env::current_dir().ok().map(|p| p.display().to_string()),
        "tool": name,
        "args": args,
        "outcome": outcome,
    });
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        // One write syscall per record: parallel tool executions append to
        // this file concurrently, and a multi-syscall formatted write would
        // interleave mid-record.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}
