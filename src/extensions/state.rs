//! `dex.state`: per-extension key/value store (JSON file per extension).

use super::discovery::data_extensions_dir;
use std::collections::BTreeMap;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// dex.state: per-extension key/value store (JSON file per extension)
// ---------------------------------------------------------------------------

pub(crate) static STATE: std::sync::Mutex<BTreeMap<String, BTreeMap<String, serde_json::Value>>> =
    std::sync::Mutex::new(BTreeMap::new());

fn state_file(ext_id: &str) -> PathBuf {
    data_extensions_dir()
        .join("state")
        .join(format!("{ext_id}.json"))
}

/// One JSON file per extension, loaded lazily, written through on every
/// mutation. Values are JSON — small state only (flags, counters), not
/// documents.
fn load_state_file(ext_id: &str) -> BTreeMap<String, serde_json::Value> {
    std::fs::read_to_string(state_file(ext_id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn with_state<R>(ext_id: &str, f: impl FnOnce(&mut BTreeMap<String, serde_json::Value>) -> R) -> R {
    let mut guard = STATE.lock().expect("state lock");
    let map = guard
        .entry(ext_id.to_string())
        .or_insert_with(|| load_state_file(ext_id));
    let result = f(map);
    let _ = std::fs::create_dir_all(state_file(ext_id).parent().expect("parent"));
    let _ = std::fs::write(
        state_file(ext_id),
        serde_json::to_string_pretty(map).unwrap_or_default(),
    );
    result
}

/// Read-only variant: no file rewrite on a pure `get`.
fn with_state_read<R>(
    ext_id: &str,
    f: impl FnOnce(&BTreeMap<String, serde_json::Value>) -> R,
) -> R {
    let mut guard = STATE.lock().expect("state lock");
    let map = guard
        .entry(ext_id.to_string())
        .or_insert_with(|| load_state_file(ext_id));
    f(map)
}

/// `dex.state.get(key)` — JSON value or nil.
pub(crate) fn state_get(ext_id: &str, key: &str) -> Option<serde_json::Value> {
    with_state_read(ext_id, |map| map.get(key).cloned())
}

/// `dex.state.set(key, value)` — write-through to the extension's state file.
pub(crate) fn state_set(ext_id: &str, key: String, value: serde_json::Value) {
    with_state(ext_id, |map| {
        map.insert(key, value);
    });
}
