//! One-time process notices: `warn_once` dedupes by id so a notice fires
//! once per process even though config rebuilds per turn. stderr only —
//! stdout belongs to the client stream, and one line cannot corrupt a TUI
//! the way a per-turn stream could. Lives in the kernel because every
//! layer (protocol deprecation ladders, config, extensions) reports
//! deprecated or ignored input through it.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// `id` dedupes; `message` is the full text after the `dex: ` prefix.
pub fn warn_once(id: &str, message: &str) {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = WARNED.get_or_init(Default::default);
    if seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string())
    {
        eprintln!("dex: {message}");
    }
}
