//! Session registry lookup: the single owner of "id → live entry".
//! In-memory registry first, one-shot disk fallback (filename probe, then
//! `list_all` for renamed/legacy files), post-rebuild negative caching.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::http::StatusCode;

use crate::session;

use super::{lock_map, DaemonState, SessionEntry};

/// Registry lookup with a one-shot disk fallback: the startup rebuild runs
/// in the background, so an id missing from the registry may simply not have
/// been scanned yet. A disk hit is registered (live entries win over the
/// later rebuild merge via `or_insert`) so subsequent lookups stay in-memory.
/// A post-rebuild disk miss is negative-cached (perf doc §27), so a typo'd
/// id walks the workspace once, not once per request. Negative entries
/// expire after `NEGATIVE_TTL` so out-of-band creates become visible.
pub(crate) fn missing_hit(state: &Arc<DaemonState>, session_id: &str) -> bool {
    if !state.rebuild_complete.load(Ordering::Relaxed) {
        return false;
    }
    let mut missing = lock_map(&state.missing_sessions);
    match missing.get(session_id) {
        Some(at) if at.elapsed() < crate::daemon::NEGATIVE_TTL => true,
        Some(_) => {
            missing.remove(session_id);
            false
        }
        None => false,
    }
}

pub(crate) fn lookup_entry(state: &Arc<DaemonState>, session_id: &str) -> Option<SessionEntry> {
    if let Some(entry) = lock_map(&state.sessions).get(session_id).cloned() {
        return Some(entry);
    }
    // Stale-negative self-heal + typo-storm guard (§27): the filename probe
    // is readdir-only (no header opens), so a negative hit re-probes cheaply
    // — an out-of-band create (CLI/TUI writing the file directly, which
    // never clears this set) becomes visible on the next request instead of
    // after `NEGATIVE_TTL`, while a true typo still avoids the full
    // `list_all` open+parse walk on every request. The probe doubles as the
    // fast path below (perf doc §1).
    let probed = session::Session::find_by_id_filename(session_id);
    if missing_hit(state, session_id) && probed.is_none() {
        return None;
    }
    lock_map(&state.missing_sessions).remove(session_id);
    // Fast path first (perf doc §1): the session id is the JSONL filename,
    // so a filename match + one header read replaces the workspace-wide
    // open+parse of every session. The `list_all` scan below only serves
    // renamed/legacy files whose stem no longer names the id.
    if let Some(path) = probed {
        // Exact id only: `find_by_id_filename` also resolves unique prefixes
        // (a CLI convenience). Accepting one here would register — and cache
        // — a *different* session under the requested key, so every later
        // request on that key mutates the wrong journal. Prefix selectors
        // stay a client-side (`/resume`) affordance.
        if let Some(entry_session) = session::Session::from_path(&path)
            .ok()
            .filter(|s| s.id() == session_id)
        {
            let entry = SessionEntry {
                path: path.clone(),
                name: entry_session.name().map(ToOwned::to_owned),
                cwd: entry_session.cwd().to_string(),
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
            };
            lock_map(&state.sessions).insert(session_id.to_string(), entry.clone());
            return Some(entry);
        }
    }
    let found = session::Session::list_all()
        .unwrap_or_default()
        .into_iter()
        .find(|(_, header)| header.id() == session_id);
    let Some((path, header)) = found else {
        // Post-rebuild a miss is stable (new ids are server-minted on an
        // explicit registration path that clears this set): remember it.
        if state.rebuild_complete.load(Ordering::Relaxed) {
            lock_map(&state.missing_sessions)
                .insert(session_id.to_string(), std::time::Instant::now());
        }
        return None;
    };
    // An out-of-band create after a negative hit must clear the stale miss.
    lock_map(&state.missing_sessions).remove(session_id);
    let entry = SessionEntry {
        path: path.clone(),
        name: header.name().map(ToOwned::to_owned),
        cwd: header.cwd().to_string(),
        model: None,
        wake_provider: None,
        wake_base_url: None,
        plan_persisted: None,
        model_persisted: None,
    };
    lock_map(&state.sessions).insert(session_id.to_string(), entry.clone());
    Some(entry)
}

/// True when an identical daemon-persisted session-state value is still
/// current (perf doc §28): never written, changed since, or written but the
/// file moved under us — the mtime guard, because the co-located TUI can
/// write the same file directly, so an entry-only comparison could skip a
/// needed restore. A missing file also forces the write (same as before).
/// `==` on `SystemTime` is brittle on coarse-granularity filesystems
/// (1 s FAT/NFS ticks: a same-tick out-of-band write compares equal and is
/// missed) — accepted: sessions live on local disks (ns ext4/APFS), and a
/// miss only re-appends an identical value, never a wrong one.
pub(crate) fn persisted_current<T: PartialEq>(
    persisted: &Option<(T, std::time::SystemTime)>,
    value: &T,
    path: &std::path::Path,
) -> bool {
    let Some((prev, at)) = persisted else {
        return false;
    };
    prev == value
        && std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|mtime| mtime == *at)
}

/// Async registry lookup with the disk fallback off the executor (perf doc
/// §27): `Session::list_all` walks the workspace, so the scan +
/// registration run in `spawn_blocking`. In-memory hits (and post-rebuild
/// negative hits) stay inline — only a true registry miss pays the hop.
pub(crate) async fn lookup_entry_async(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<SessionEntry, StatusCode> {
    if let Some(entry) = lock_map(&state.sessions).get(session_id).cloned() {
        return Ok(entry);
    }
    if missing_hit(state, session_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let state = Arc::clone(state);
    let session_id = session_id.to_string();
    // A JoinError (panic/cancel) is an internal failure, not an absent
    // session: surfacing 500 instead of folding into `None` → 404.
    tokio::task::spawn_blocking(move || lookup_entry(&state, &session_id))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)
}

/// Resolve a session file path from the registry (with disk fallback), or 404.
pub(crate) async fn session_path(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    let path = lookup_entry_async(state, session_id).await?.path;
    if path.exists() {
        Ok(path)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
