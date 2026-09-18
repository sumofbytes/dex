//! Learned wire protocols: the single owner of empirical protocol inference.
//!
//! A model that rejects `/responses` but succeeds over `/chat/completions`
//! is remembered (per endpoint+model) so the next turn — and the next run —
//! goes straight to completions. Two layers, one module: an in-memory table
//! (this process — the one-time learning cost is a single failed call per
//! model per run) backed by `learned-apis.json` in the cache dir (survives
//! restarts / one-shot runs). `remember` writes both together, so the layers
//! can never disagree the way two separately-synced caches could.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use crate::protocol::ApiProtocol;
use crate::workspace::{cached_parse, xdg_path, FileCache};

/// Models empirically switched to chat-completions after the responses API
/// rejected them (e.g. glm-5.3-flash on zen/go 500s on `/responses`, 200s on
/// `/chat/completions`). Keyed by (base_url, model).
fn probed_apis() -> &'static Mutex<HashMap<(String, String), ApiProtocol>> {
    static MAP: OnceLock<Mutex<HashMap<(String, String), ApiProtocol>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

fn learned_key(base_url: &str, model: &str) -> (String, String) {
    (base_url.to_string(), model.to_string())
}

/// Persisted side: `XDG_CACHE_HOME/dex/learned-apis.json`,
/// `"<base_url>|<model>"` → protocol name. Only consulted when nothing
/// explicit pins the protocol.
/// ponytail: no expiry — a model that speaks completions keeps working even
/// after the provider adds responses support.
fn learned_apis_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/learned-apis.json")
}

/// Learned protocols, cached process-wide and invalidated by file identity:
/// the parse is served while the content hash is unchanged.
type LearnedApiMap = serde_json::Map<String, serde_json::Value>;

static LEARNED_CACHE: OnceLock<Mutex<Option<FileCache<LearnedApiMap>>>> = OnceLock::new();

fn learned_api_map() -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = learned_apis_path() else {
        return Default::default();
    };
    // A missing or unparseable file is an empty map (and gets cached as
    // one): learning simply starts over.
    cached_parse(&LEARNED_CACHE, &path, |text| {
        Some(
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                &text.unwrap_or_default(),
            )
            .unwrap_or_default(),
        )
    })
    .unwrap_or_default()
}

fn learned_from_file(base_url: &str, model: &str) -> Option<ApiProtocol> {
    learned_api_map()
        .get(format!("{base_url}|{model}").as_str())?
        .as_str()
        .and_then(ApiProtocol::parse)
}

/// Learned protocol for this endpoint+model: the in-memory table first, then
/// the persisted file (a file hit is promoted into memory so later turns in
/// this process skip the file map).
pub(crate) fn lookup(base_url: &str, model: &str) -> Option<ApiProtocol> {
    if let Some(api) = probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(base_url.to_string(), model.to_string()))
        .copied()
    {
        return Some(api);
    }
    let api = learned_from_file(base_url, model)?;
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(learned_key(base_url, model), api);
    Some(api)
}

/// Remember an empirically learned protocol in memory AND on disk together:
/// the next turn in this process and the next run both skip the failed
/// protocol. Best-effort; a lost race between concurrent learners just
/// re-learns.
pub(crate) fn remember(base_url: &str, model: &str, api: ApiProtocol) {
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(learned_key(base_url, model), api);
    let Some(path) = learned_apis_path() else {
        return;
    };
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    map.insert(
        format!("{base_url}|{model}"),
        serde_json::Value::from(api.name()),
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, text);
        // The file changed under us; drop the cached map so the next
        // lookup re-reads instead of serving the pre-write copy.
        if let Some(cache) = LEARNED_CACHE.get() {
            cache.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

/// In-memory-only insert for tests: hermetic (no cache-dir write), so
/// protocol-gate tests never touch the developer's real `learned-apis.json`.
#[cfg(test)]
pub(crate) fn remember_memory(base_url: &str, model: &str, api: ApiProtocol) {
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(learned_key(base_url, model), api);
}

/// Clear the in-memory table (test teardown).
#[cfg(test)]
pub(crate) fn clear_memory() {
    probed_apis()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}
