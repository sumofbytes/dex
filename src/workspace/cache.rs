//! Content-identity file cache: read a file, re-parse only when bytes changed.

use std::sync::{Mutex, OnceLock};

/// File-identity cache entry shared by the config file, learned-apis map,
/// and models.dev catalog.
pub(crate) struct FileCache<T> {
    pub(crate) path: std::path::PathBuf,
    pub(crate) hash: u64,
    pub(crate) value: T,
}

pub(crate) fn fnv_bytes(text: &str) -> u64 {
    let mut h = 14695981039346656037u64;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(1099511628211);
    }
    h
}

pub(crate) fn cached_parse<T: Clone>(
    cache: &OnceLock<Mutex<Option<FileCache<T>>>>,
    path: &std::path::Path,
    parse: impl FnOnce(Option<String>) -> Option<T>,
) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    let hash = fnv_bytes(&text);
    if let Some(hit) = cache
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.hash == hash)
    {
        return Some(hit.value.clone());
    }
    let value = parse(Some(text))?;
    cache
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(FileCache {
            path: path.to_path_buf(),
            hash,
            value: value.clone(),
        });
    Some(value)
}
