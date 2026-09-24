//! Session discovery: finding sessions on disk. Owns the directory
//! scan (`scan_jsonl_dir`), canonical order (`sort_newest_first`), and the
//! listing entry points (`list`, `list_all`, `find_by_id_filename`,
//! `resume`). Thin wrappers on `Session` in `mod.rs` keep the
//! `Session::...` paths working.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::{Session, SessionHeader};

/// Shared directory scan behind every session listing (SES-1): each direct
/// `*.jsonl` file under `dir` whose first line parses as a session header.
/// Unreadable directories are skipped and per-file failures (open, empty,
/// bad JSON) drop the entry — only the header is needed, and session files
/// grow large, so listings stream just the first line. Callers apply
/// `sort_newest_first` for the canonical newest-first order.
pub fn scan_jsonl_dir(dir: &Path) -> Vec<(PathBuf, SessionHeader)> {
    let mut sessions = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                if let Some(first) = read_first_line(&path) {
                    if let Ok(header) = serde_json::from_str::<SessionHeader>(first.trim_end()) {
                        sessions.push((path, header));
                    }
                }
            }
        }
    }
    sessions
}

/// Canonical listing order: header timestamp, newest first.
pub fn sort_newest_first(sessions: &mut [(PathBuf, SessionHeader)]) {
    sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
}

/// Read only the first line of a file: session listings only ever need the
/// header, and session files grow with the message history.
fn read_first_line(path: &Path) -> Option<String> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut first = String::new();
    match reader.read_line(&mut first) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(first),
    }
}

// Session pickers (`/resume` sheet, remote UI) are the only runtime
// callers; tests exercise them directly.

pub fn list(cwd: &str) -> io::Result<Vec<(PathBuf, SessionHeader)>> {
    let dir = Session::session_dir().join(Session::cwd_slug(cwd));
    let mut sessions = scan_jsonl_dir(&dir);
    sort_newest_first(&mut sessions);
    Ok(sessions)
}

// Session pickers (`/resume` sheet, remote UI) are the only runtime
// callers; tests exercise them directly.

pub fn list_dir_mtime(cwd: &str) -> Option<SystemTime> {
    std::fs::metadata(Session::session_dir().join(Session::cwd_slug(cwd)))
        .ok()?
        .modified()
        .ok()
}

pub fn list_all() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
    let base = Session::session_dir();
    let mut sessions = Vec::new();
    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            sessions.extend(scan_jsonl_dir(&dir));
        }
    }
    sort_newest_first(&mut sessions);
    Ok(sessions)
}

pub fn find_by_id_filename(sid: &str) -> Option<PathBuf> {
    let q = sid.to_ascii_lowercase();
    let mut exact: Option<PathBuf> = None;
    let mut prefixed: Vec<PathBuf> = Vec::new();
    if let Ok(slugs) = fs::read_dir(Session::session_dir()) {
        for slug in slugs.flatten() {
            let dir = slug.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&dir) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if stem == sid {
                    exact = Some(path);
                    break;
                }
                let lower = stem.to_ascii_lowercase();
                if lower.starts_with(&q) || q.starts_with(&lower) {
                    prefixed.push(path);
                }
            }
            if exact.is_some() {
                break;
            }
        }
    }
    // An exact filename with a foreign header (a stale same-named file)
    // must not shadow the real session: confirm before returning, else
    // keep scanning like the legacy path would.
    if let Some(path) = exact {
        if Session::from_path(&path).is_ok_and(|s| s.id() == sid) {
            return Some(path);
        }
    }
    // Deterministic on colliding prefixes: filesystem order is
    // unspecified, so sort before confirming.
    prefixed.sort_unstable();
    prefixed.into_iter().find(|path| {
        Session::from_path(path).is_ok_and(|s| {
            let id = s.id().to_ascii_lowercase();
            id.starts_with(&q) || q.starts_with(&id)
        })
    })
}

pub async fn list_all_async() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
    let base = Session::session_dir();
    let mut dirs = Vec::new();
    if let Ok(mut rd) = tokio::fs::read_dir(&base).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let dir = entry.path();
            // Prefer async file_type; fallback to sync is_dir for races.
            let is_dir = entry
                .file_type()
                .await
                .map(|ft| ft.is_dir())
                .unwrap_or_else(|_| dir.is_dir());
            if is_dir {
                dirs.push(dir);
            }
        }
    }
    let mut set = tokio::task::JoinSet::new();
    for dir in dirs {
        set.spawn(tokio::task::spawn_blocking(move || scan_jsonl_dir(&dir)));
    }
    let mut sessions = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Ok(mut v)) = r {
            sessions.append(&mut v);
        }
    }
    sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
    Ok(sessions)
}
