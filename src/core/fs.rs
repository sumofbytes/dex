//! Small filesystem helpers shared across config, catalog, and learned
//! state: XDG path resolution, atomic tmp paths, and a content-identity
//! file cache. The single owner of "read a file, but only re-parse when
//! the bytes actually changed".

use std::env;
use std::sync::{Mutex, OnceLock};

/// Resolve `rel` under `$env_var`, falling back to `$HOME/home_sub/rel.
/// `env_var` is a parameter because callers use different ones (`DEX_CONFIG`
/// overrides the whole config path, so its check stays at that call site).
pub(crate) fn xdg_path(env_var: &str, home_sub: &str, rel: &str) -> Option<std::path::PathBuf> {
    if let Some(dir) = env::var_os(env_var) {
        return Some(std::path::PathBuf::from(dir).join(rel));
    }
    env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(home_sub).join(rel))
}

/// File-identity cache entry shared by the config file, learned-apis map,
/// and models.dev catalog: the parse is served while the content hash is
/// unchanged.
pub(crate) struct FileCache<T> {
    path: std::path::PathBuf,
    /// FNV-1a of the file bytes: identity is content, not (mtime, len),
    /// so same-length rewrites within one mtime tick and mtime-preserving
    /// copies still miss. Reads are per call (these files are KBs, the
    /// catalog parse below stays cached); the hit saves the parse.
    hash: u64,
    value: T,
}

pub(crate) fn fnv_bytes(text: &str) -> u64 {
    let mut h = 14695981039346656037u64;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Read `path` and serve `cache`'s stored parse while the content hash is
/// unchanged; on a miss, hand the text to `parse`, storing the result.
/// `parse` gets `None` when the read failed and returns `None` when nothing
/// should be cached (read/parse failure) — the caller decides what that
/// means (empty default vs hard failure). Identity is the content hash
/// rather than (mtime, len): a same-length rewrite inside one mtime tick
/// (FAT/NFS 1–2 s granularity, `cp -p`, checkout preserving mtime) still
/// misses instead of serving stale config/endpoints indefinitely.
/// Poisoned-mutex recovery matches the rest of the daemon: keep the value.
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

/// Unique tmp path for an atomic write: PID plus a per-call counter, so
/// concurrent writers (daemon fetch vs `dex update --models`, or two
/// in-process rebuilds) never share a tmp file — a shared name lets one
/// writer's rename publish another writer's half-written bytes, which is
/// exactly the torn state the rename was meant to prevent. Readers ignore
/// tmp files, so a crashed write just litters one stale file.
pub(crate) fn unique_tmp_path(path: &std::path::Path) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}
