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
#[cfg(test)]
const CHANGE_CONTENT_CAP: usize = 64 * 1024;
/// Keep at most this many change records per session (FIFO).
#[cfg(test)]
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
/// Production code never appends to the ledger yet (undo entries are
/// seeded by tests against the `/undo` route); test-only until a
/// write/edit tool wires it up.
#[cfg(test)]
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

/// Undo the most recent change: the target file must still match
/// `after_hash` (no concurrent edit since), otherwise refuse. Returns a
/// human-readable summary for the caller to surface.
pub(crate) fn undo_last_change(session: &mut Session) -> io::Result<String> {
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
#[cfg(test)]
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
