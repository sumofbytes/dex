//! Per-session change ledger (`session_state "changes"`): the single owner
//! of `/undo`. Write/edit tools append a [`ChangeRecord`]; undo pops the
//! newest entry and restores `before` content when the file still matches
//! `after_hash` (no concurrent edit since).

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{load_session_state, Session};

/// One entry in the per-session change ledger.
/// `before`/`after` hold file content (capped) so `/undo` can restore the
/// previous state; hashes always recorded even when content was too big.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct ChangeRecord {
    pub path: String,
    pub tool: String,
    pub before_hash: String,
    pub after_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub timestamp: String,
}

/// Cap for content stored in a change record; larger files record hashes
/// only (undo unavailable for them).
const CHANGE_CONTENT_CAP: usize = 64 * 1024;
/// Keep at most this many change records per session (FIFO).
const CHANGE_RECORD_CAP: usize = 50;

/// Load the change ledger (FIFO, newest last).
pub(crate) fn load_changes(path: &Path) -> Vec<ChangeRecord> {
    load_session_state(path)
        .ok()
        .and_then(|m| m.get("changes").cloned())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Replace the change ledger.
pub(crate) fn save_changes(session: &mut Session, changes: &[ChangeRecord]) -> io::Result<()> {
    let json = serde_json::to_string(changes).map_err(io::Error::other)?;
    session.set_state("changes", &json)
}

/// Record one change (FIFO-capped). Returns the updated ledger.
pub(crate) fn record_change(
    session: &mut Session,
    record: ChangeRecord,
) -> io::Result<Vec<ChangeRecord>> {
    let mut changes = session.path().map(load_changes).unwrap_or_default();
    changes.push(record);
    while changes.len() > CHANGE_RECORD_CAP {
        changes.remove(0);
    }
    save_changes(session, &changes)?;
    Ok(changes)
}

/// Hash that identifies a call's input (tool name + raw arguments) in the
/// effect journal — restart recovery matches intents, not contents.
fn hash_input(tool: &str, raw_args: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut hasher);
    raw_args.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Before-state captured for one write/edit call, for `/undo`.
struct CapturedFile {
    path: String,
    before_hash: String,
    before: Option<String>,
}

/// What [`track_start`] captured for one mutating call; pass it back to
/// [`track_end`] after execution.
pub(crate) struct TrackedCall {
    call_id: String,
    tool: String,
    /// write/edit target snapshot; `None` for bash/MCP/… (no single file
    /// to snapshot) and for creates (nothing to undo back into).
    file: Option<CapturedFile>,
}

/// Snapshot the write/edit target before execution: hash always, content
/// when the file exists and fits the cap. A file that doesn't exist yet
/// yields `None` — undoing a creation would mean deleting it, which the
/// ledger doesn't do.
fn capture_before(raw_args: &str) -> Option<CapturedFile> {
    let path = serde_json::from_str::<serde_json::Value>(raw_args)
        .ok()?
        .get("path")?
        .as_str()?
        .to_string();
    let meta = fs::metadata(&path).ok()?;
    let before = if meta.len() <= CHANGE_CONTENT_CAP as u64 {
        fs::read_to_string(&path).ok()
    } else {
        None
    };
    let before_hash = crate::tools::hash_file(&path);
    Some(CapturedFile {
        path,
        before_hash,
        before,
    })
}

/// Journal durable intent (`effect_start`) for one mutating call and capture
/// its before-state for `/undo`. Call BEFORE executing the tool; feed the
/// returned [`TrackedCall`] to [`track_end`] afterwards. `None` for
/// read-only tools (nothing to recover) and without a persisted session
/// (nowhere to record).
pub(crate) fn track_start(
    session: Option<&mut Session>,
    call_id: &str,
    tool: &str,
    raw_args: &str,
) -> Option<TrackedCall> {
    if !crate::tools::metadata(tool).is_some_and(|m| m.mutating) {
        return None;
    }
    let session = session?;
    let _ = session.effect_start(call_id, tool, &hash_input(tool, raw_args));
    let file = matches!(tool, "write" | "edit")
        .then(|| capture_before(raw_args))
        .flatten();
    Some(TrackedCall {
        call_id: call_id.to_string(),
        tool: tool.to_string(),
        file,
    })
}

/// Close one tracked call AFTER execution: `effect_result` in the journal
/// (so a crash mid-tool leaves the `effect_start` visibly open) and, for
/// write/edit, the post-execution file state in the `/undo` ledger. `None`
/// (an untracked, read-only call) is a no-op.
pub(crate) fn track_end(session: Option<&mut Session>, tracked: Option<TrackedCall>, ok: bool) {
    let Some(tracked) = tracked else { return };
    let Some(session) = session else { return };
    let _ = session.effect_result(&tracked.call_id, ok);
    let Some(file) = tracked.file else { return };
    let after_hash = crate::tools::hash_file(&file.path);
    let after = fs::metadata(&file.path)
        .ok()
        .filter(|m| m.len() <= CHANGE_CONTENT_CAP as u64)
        .and_then(|_| fs::read_to_string(&file.path).ok());
    let record = make_change_record(
        &tracked.tool,
        &file.path,
        file.before.as_deref(),
        after.as_deref(),
        &file.before_hash,
        &after_hash,
    );
    let _ = record_change(session, record);
}

/// Undo the most recent change: the target file must still match
/// `after_hash` (no concurrent edit since), otherwise refuse. Returns a
/// human-readable summary for the caller to surface.
pub fn undo_last_change(session: &mut Session) -> io::Result<String> {
    let Some(path) = session.path().map(|p| p.to_path_buf()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "session is not persisted",
        ));
    };
    let mut changes = load_changes(&path);
    let Some(record) = changes.pop() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no changes to undo",
        ));
    };
    if crate::tools::hash_file(&record.path) != record.after_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} was modified since the change; refusing to undo",
                record.path
            ),
        ));
    }
    let Some(before) = &record.before else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is too large for undo", record.path),
        ));
    };
    fs::write(&record.path, before).map_err(io::Error::other)?;
    save_changes(session, &changes)?;
    Ok(format!(
        "undid {} on {} ({})",
        record.tool, record.path, record.timestamp
    ))
}

/// Build a change record from a completed write/edit.
pub(crate) fn make_change_record(
    tool: &str,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
    before_hash: &str,
    after_hash: &str,
) -> ChangeRecord {
    let cap = |s: &str| (s.len() <= CHANGE_CONTENT_CAP).then(|| s.to_string());
    ChangeRecord {
        path: path.to_string(),
        tool: tool.to_string(),
        before_hash: before_hash.to_string(),
        after_hash: after_hash.to_string(),
        before: before.and_then(cap),
        after: after.and_then(cap),
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}
