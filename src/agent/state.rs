#![allow(dead_code, unused_variables, unused_imports)]
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

pub(crate) trait CancellationSource: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn take_cancelled(&self) -> bool;
}

/// Process-global cancellation (Ctrl+C) used by the non-TUI paths
/// (`dex "prompt"` and `dex --tool`). The TUI/daemon paths use a per-session
/// `CancellationToken` instead, so a cancel never leaks across sessions.
#[derive(Clone)]
pub(crate) struct GlobalCancellation;

impl CancellationSource for GlobalCancellation {
    fn is_cancelled(&self) -> bool {
        crate::core::console::is_interrupted()
    }
    fn take_cancelled(&self) -> bool {
        crate::core::console::take_interrupt()
    }
}

pub(crate) const CACHE_FILE_NAME: &str = "dex-tool-cache.json";

pub(crate) fn cache_file_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(dir).join(CACHE_FILE_NAME));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join(CACHE_FILE_NAME))
}

pub(crate) fn cache_fingerprint(name: &str, input: &str) -> String {
    let mut fingerprint = String::new();
    if matches!(name, "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls") {
        if let Ok(args) = serde_json::from_str::<Value>(input) {
            if let Some(path) = args.get("path").and_then(Value::as_str) {
                if let Ok(meta) = fs::metadata(path) {
                    fingerprint = format!(
                        ":{}:{}",
                        meta.len(),
                        meta.modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos())
                            .unwrap_or_default()
                    );
                }
            }
        }
    }
    fingerprint
}

#[derive(Default)]
pub(crate) struct ToolState {
    pub(crate) cache: HashMap<String, String>,
    pub(crate) dirty: bool,
    /// Last API-reported prompt token count for the main conversation.
    pub(crate) last_usage: Option<u64>,
    /// Provider-reported cached-token subset of the last call's prompt, when
    /// the provider reports cache detail. Display-only.
    pub(crate) last_cached: Option<u64>,
    /// Session-cumulative prompt tokens across every LLM call, kept in the
    /// TUI process (in-memory only, resets on restart). The status bar shows
    /// `last_usage` as live context utilization and this as the spend figure.
    pub(crate) total_usage: u64,
    /// Session-cumulative completion (output) tokens across every LLM call,
    /// fed by the same `Usage` events as `total_usage`. In-memory only.
    pub(crate) total_output: u64,
    /// Session-cumulative cost in USD, mirroring pi's `usageTotals.cost`.
    /// Accumulated per `Usage` event from provider pricing (catalog) or
    /// `DEX_COST_PER_1K` fallback. In-memory only, like `total_usage`.
    pub(crate) total_cost: f64,
    pub(crate) verify_dirty: bool,
}

impl ToolState {
    pub(crate) fn load() -> Self {
        let mut state = Self::default();
        // Cached tool output can contain source code or secrets. Keep caching
        // opt-in until a caller explicitly requests it.
        if env::var("DEX_TOOL_CACHE").as_deref() != Ok("1") {
            return state;
        }
        if let Some(path) = cache_file_path() {
            if let Ok(contents) = fs::read_to_string(&path) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&contents) {
                    state.cache = map;
                }
            }
        }
        state
    }

    pub(crate) fn insert(&mut self, key: String, value: String) {
        self.cache.insert(key, value);
        self.dirty = true;
    }

    pub(crate) fn clear(&mut self) {
        if !self.cache.is_empty() {
            self.cache.clear();
            self.dirty = true;
        }
    }

    /// Persist the cache to disk (best-effort; failures are ignored).
    pub(crate) fn save(&self) {
        if !self.dirty || env::var("DEX_TOOL_CACHE").as_deref() != Ok("1") {
            return;
        }
        if let Some(path) = cache_file_path() {
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Ok(json) = serde_json::to_string(&self.cache) {
                let _ = fs::write(&path, json);
            }
        }
    }

    /// Async load: cache hits are cheap; file read goes through async fs
    /// (Phase 6). Best-effort, never fails the turn.
    pub(crate) async fn load_async() -> Self {
        let mut state = Self::default();
        if env::var("DEX_TOOL_CACHE").as_deref() != Ok("1") {
            return state;
        }
        if let Some(path) = cache_file_path() {
            if let Ok(contents) = tokio::fs::read_to_string(&path).await {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&contents) {
                    state.cache = map;
                }
            }
        }
        state
    }

    /// Async save: spawned so turn teardown never waits on it (today it
    /// already does not fail the turn; keep that).
    pub(crate) async fn save_async(&self) {
        if !self.dirty || env::var("DEX_TOOL_CACHE").as_deref() != Ok("1") {
            return;
        }
        if let Some(path) = cache_file_path() {
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            if let Ok(json) = serde_json::to_string(&self.cache) {
                let _ = tokio::fs::write(&path, json).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_file_path_resolves_xdg_then_home() {
        let prev_xdg = env::var_os("XDG_CACHE_HOME");
        let prev_home = env::var_os("HOME");

        env::set_var("XDG_CACHE_HOME", "/tmp/xdg-cache-test");
        assert_eq!(
            cache_file_path().unwrap(),
            PathBuf::from("/tmp/xdg-cache-test/dex-tool-cache.json")
        );

        env::remove_var("XDG_CACHE_HOME");
        env::set_var("HOME", "/tmp/fakehome2");
        assert_eq!(
            cache_file_path().unwrap(),
            PathBuf::from("/tmp/fakehome2/.cache/dex-tool-cache.json")
        );

        match prev_xdg {
            Some(v) => env::set_var("XDG_CACHE_HOME", v),
            None => env::remove_var("XDG_CACHE_HOME"),
        }
        match prev_home {
            Some(v) => env::set_var("HOME", v),
            None => env::remove_var("HOME"),
        }
    }

    #[test]
    fn cache_fingerprint_is_stable_for_missing_file() {
        // No real file -> no fingerprint suffix
        let fp = cache_fingerprint("read", r#"{"path":"/tmp/definitely-missing-dex-12345"}"#);
        assert_eq!(fp, "");
    }

    #[test]
    fn tool_state_insert_marks_dirty_and_clear_resets() {
        let mut s = ToolState::default();
        assert!(!s.dirty);
        s.insert("k".into(), "v".into());
        assert!(s.dirty);
        assert_eq!(s.cache.get("k").unwrap(), "v");
        s.clear();
        assert!(s.cache.is_empty());
        // second clear when already empty does not dirty again (still dirty from first clear, but not extra)
        let dirty_before = s.dirty;
        s.clear();
        assert_eq!(s.dirty, dirty_before);
    }

    #[test]
    fn tool_state_load_is_empty_when_cache_disabled() {
        // default env has DEX_TOOL_CACHE != "1"
        let prev = env::var_os("DEX_TOOL_CACHE");
        env::remove_var("DEX_TOOL_CACHE");
        let s = ToolState::load();
        assert!(s.cache.is_empty());
        match prev {
            Some(v) => env::set_var("DEX_TOOL_CACHE", v),
            None => env::remove_var("DEX_TOOL_CACHE"),
        }
    }
}
