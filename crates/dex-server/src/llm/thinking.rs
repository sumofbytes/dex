//! Per-model reasoning effort chosen via `/thinking`: the single owner of
//! the `thinking-effort.json` cache (`"<base_url>|<model>"` → effort).
//! A stored choice wins over `DEX_THINKING_EFFORT` (more specific than
//! a global). `None` clears the entry.
//!
//! ponytail: read-through, no process cache — the file holds a handful of
//! entries; add file-identity caching like `learned-apis.json` if it grows.

use crate::workspace::{unique_tmp_path, xdg_path};

fn thinking_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/thinking-effort.json")
}

fn thinking_map() -> serde_json::Map<String, serde_json::Value> {
    thinking_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

// `/thinking` (TUI) is the only runtime caller; tests exercise it directly.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
fn write_thinking_map(map: &serde_json::Map<String, serde_json::Value>) {
    let Some(path) = thinking_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(map) {
        // Atomic (unique tmp + rename): the daemon re-reads this file per
        // turn; a direct write can hand it torn JSON that then sticks as a
        // cached parse failure until the next write.
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub fn stored_thinking_effort(base_url: &str, model: &str) -> Option<String> {
    thinking_map()
        .get(format!("{base_url}|{model}").as_str())?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Remember (`Some`) or clear (`None`) the `/thinking` choice for one
/// endpoint+model. Best-effort.
// `/thinking` (TUI) is the only runtime caller; tests exercise it directly.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn remember_thinking_effort(base_url: &str, model: &str, effort: Option<&str>) {
    let mut map = thinking_map();
    let key = format!("{base_url}|{model}");
    match effort.filter(|e| !e.is_empty()) {
        Some(effort) => {
            map.insert(key, serde_json::Value::from(effort));
        }
        None => {
            map.remove(&key);
        }
    }
    write_thinking_map(&map);
}
