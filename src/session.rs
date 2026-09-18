#![allow(dead_code, unused_variables, unused_imports)]
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::types::{ChatMessage, Role};

const SESSION_VERSION: u32 = 1;

/// Serializes tests that redirect XDG_DATA_HOME (it decides where ALL
/// sessions live, including other tests' fixtures).
#[cfg(test)]
pub(crate) static TEST_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Panic-safe env restore for tests: saved on construction, reverted on drop
/// even when the test panics, so a failed test can't leak vars into others
/// running in the same process. Take it while holding TEST_SESSIONS_ENV_LOCK.
#[cfg(test)]
pub(crate) struct EnvGuard(pub Vec<(&'static str, Option<std::ffi::OsString>)>);

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.0.drain(..) {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct SessionHeader {
    #[serde(rename = "type")]
    entry_type: String,
    version: u32,
    id: String,
    timestamp: String,
    cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl SessionHeader {
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn cwd(&self) -> &str {
        &self.cwd
    }
    pub(crate) fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

/// Borrowed view of a message for append-time serialization: the journal
/// line is built from references, so appending never clones the message.
/// Reads parse entries back through `serde_json::Value`, not this struct.
#[derive(Serialize)]
struct SessionMessageEntry<'a> {
    #[serde(rename = "type")]
    entry_type: &'a str,
    id: &'a str,
    timestamp: &'a str,
    #[serde(flatten)]
    message: &'a ChatMessage,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct SessionInfoEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    name: String,
}

#[derive(Serialize)]
struct SessionClearEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
}

#[derive(Serialize)]
struct SessionEventEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    /// Complexity-router tier that chose this turn's model (`turn_start`
    /// only, routing enabled). Absent otherwise, so unrouted journals
    /// stay byte-identical to before.
    #[serde(skip_serializing_if = "Option::is_none")]
    tier: Option<String>,
}

/// Durable record of one side effect: intent (before execution) and outcome
/// (after). Written around every tool execution so a restart can reconcile
/// what happened vs. what completed (P8 journal).
#[derive(Serialize)]
struct SessionEffectEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    tool_call_id: String,
    name: String,
    input_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<bool>,
}

/// One entry in the per-session change ledger (`session_state "changes"`).
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

#[derive(Serialize)]
struct SessionStateEntry {
    #[serde(rename = "type")]
    entry_type: String,
    id: String,
    timestamp: String,
    key: String,
    value: String,
}

#[derive(Debug)]
pub(crate) struct Session {
    header: SessionHeader,
    path: Option<PathBuf>,
    /// Reused append handle for the main JSONL, opened lazily on first write;
    /// one write syscall per line, same as the old open-append-write-close —
    /// the cache only removes the per-append open/close.
    journal: Option<File>,
    /// Same for the events journal: the hot path, one line per stream delta.
    events_journal: Option<File>,
}

/// Shared directory scan behind every session listing (SES-1): each direct
/// `*.jsonl` file under `dir` whose first line parses as a session header.
/// Unreadable directories are skipped and per-file failures (open, empty,
/// bad JSON) drop the entry — only the header is needed, and session files
/// grow large, so listings stream just the first line. Callers apply
/// `sort_newest_first` for the canonical newest-first order.
fn scan_jsonl_dir(dir: &Path) -> Vec<(PathBuf, SessionHeader)> {
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
fn sort_newest_first(sessions: &mut [(PathBuf, SessionHeader)]) {
    sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
}

/// Stream a file line by line, handing each line (newline included) to `f`
/// (SES-2). Owns the read loop shared by the journal/state scanners:
/// `Interrupted` (EINTR) retries instead of ending the scan — treating it
/// as EOF silently truncates a mid-journal read — and any other read error
/// stops the scan like EOF, so the helper's `Err` is the open failure only.
/// Per-scanner work (substring prefilters, JSON parsing) stays in `f`.
fn for_each_line(path: &Path, mut f: impl FnMut(&str)) -> io::Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            // EINTR: retry, never treat as EOF — that would silently
            // truncate the scan mid-file.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Torn/invalid read: stop the scan (matches `load_events`).
            Err(_) => return Ok(()),
            Ok(_) => {}
        }
        f(&line);
    }
}

// ---------------------------------------------------------------------------
// Append-only journal caches (perf doc §§11–12)
// ---------------------------------------------------------------------------
//
// Production writers only ever append — `clear_messages` writes a `clear`
// marker the loader folds, and no delete/reset endpoint exists — so a
// snapshot stays exact as long as every mutation flows through
// `append_line` (session journal) / `append_event` (events journal) below,
// which refresh the snapshot's identity after each write. The one exception
// is `rewrite_messages` (post-compaction): it replaces the file atomically
// (temp + rename) and publishes the fresh snapshot fused with its identity
// under one lock. Identity is `(mtime, len)`: any out-of-band rewrite (a
// test's `fs::write`, a hand edit) misses and re-parses from disk, and a
// missing file evicts. Paths are unique per session and entries are
// FIFO-capped, so a long-lived daemon can't accumulate dead sessions.

/// `(mtime, len)` identity for a journal snapshot.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileId {
    mtime: SystemTime,
    len: u64,
}

fn file_id(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().and_then(|m| {
        m.modified().ok().map(|mtime| FileId {
            mtime,
            len: m.len(),
        })
    })
}

/// FIFO-capped process-global cache keyed by journal path.
struct PathCache<V> {
    map: HashMap<PathBuf, V>,
    order: VecDeque<PathBuf>,
}

impl<V> PathCache<V> {
    const CAP: usize = 32;

    fn insert(&mut self, path: &Path, value: V) {
        if !self.map.contains_key(path) {
            self.order.push_back(path.to_path_buf());
            while self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
            // Eviction above skips keys removed out of band; compact the
            // queue if it still overflows with stale keys.
            if self.order.len() > Self::CAP * 2 {
                self.order.retain(|p| self.map.contains_key(p));
            }
        }
        self.map.insert(path.to_path_buf(), value);
    }

    fn get(&self, path: &Path) -> Option<&V> {
        self.map.get(path)
    }

    fn get_mut(&mut self, path: &Path) -> Option<&mut V> {
        self.map.get_mut(path)
    }

    fn evict(&mut self, path: &Path) {
        self.map.remove(path);
    }
}

/// Durable-journal mode, read per call: `env::var` on the journal hot path
/// is a few microseconds per appended line (lines are per-message, not per
/// frame), while a `OnceLock` would cache the first read forever and poison
/// runtime flips plus every recovery test after the first append.
fn durable_journal() -> bool {
    env::var("DEX_DURABLE").as_deref() == Ok("1")
}

/// Parsed session history by journal path (perf doc §11): the per-turn
/// daemon rebuild and the TUI's transcript loads share it within a process.
/// Stored PRE-repair — every load (hit or miss) runs
/// `repair_dangling_tool_calls` on its own copy, exactly like a fresh parse.
type HistorySnapshot = (FileId, Vec<ChatMessage>);

fn history_cache() -> &'static Mutex<PathCache<HistorySnapshot>> {
    static CACHE: OnceLock<Mutex<PathCache<HistorySnapshot>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(PathCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Snapshot lookup: cloned vec on identity match; a missing file evicts.
/// Identity is `(mtime, len)`: sufficient on filesystems with
/// nanosecond mtime granularity (ext4/APFS/NTFS — any same-length
/// rewrite changes mtime and misses). Coarse-granularity filesystems
/// (1 s, e.g. FAT) can false-hit on a same-length rewrite within one
/// tick — accepted: sessions live on local disks, and production
/// writers only append (len always grows).
fn history_cache_get(path: &Path) -> Option<Vec<ChatMessage>> {
    let Some(id) = file_id(path) else {
        history_cache()
            .lock()
            .expect("history cache lock")
            .evict(path);
        return None;
    };
    let cache = history_cache().lock().expect("history cache lock");
    match cache.get(path) {
        Some((cached_id, messages)) if *cached_id == id => Some(messages.clone()),
        _ => None,
    }
}

fn history_cache_put(path: &Path, id: FileId, messages: Vec<ChatMessage>) {
    history_cache()
        .lock()
        .expect("history cache lock")
        .insert(path, (id, messages));
}

/// Refresh the history snapshot's identity after one of our own appends.
/// The message vector itself is extended by `append_message` /
/// `clear_messages` — the only message-shape writers — while every other
/// entry type (turn markers, effects, state) only moves the identity.
///
/// `appended` is the byte count this handle just wrote. The bump is only
/// valid when the file grew by exactly that much since the cached parse:
/// a second process (TUI + spawned daemon, or a hand edit) appending a
/// message row in between would otherwise make the entry claim a length it
/// never parsed, serving a history silently missing those rows — and every
/// later marker append would keep re-bumping it. Foreign growth evicts.
fn history_cache_touch(path: &Path, appended: u64) {
    let Some(id) = file_id(path) else { return };
    let mut cache = history_cache().lock().expect("history cache lock");
    let foreign = match cache.get_mut(path) {
        Some(entry) if entry.0.len.saturating_add(appended) == id.len => {
            entry.0 = id;
            false
        }
        Some(_) => true,
        None => false,
    };
    if foreign {
        cache.evict(path);
    }
}

/// Steady-state events cursor (perf doc §12): per events-journal path, the
/// exact parsed-end identity, highest seq served, and byte-offset
/// checkpoints (first seq per 64 KiB chunk) so a poll seeks past
/// already-served rows. The idle 2 s poll with no new rows is then one
/// `stat` and no file open.
struct EventsCursor {
    id: FileId,
    max_seq: Option<u64>,
    checkpoints: Vec<(u64, u64)>,
    /// The scan that published this entry reached EOF. A page-limited scan
    /// (§1) stops early, so the fast path must not treat its `max_seq` as
    /// the file tip — the next poll re-scans from its checkpoint instead.
    drained: bool,
}

/// Byte spacing of events checkpoints: a poll seeks to the newest chunk at
/// or before its cursor and parses only the tail.
const EVENTS_CHECKPOINT_BYTES: u64 = 64 * 1024;
/// Checkpoint count cap per journal (perf-only: ancient cursors scan more).
const EVENTS_CHECKPOINT_CAP: usize = 4096;
/// Rows served per events-journal page (§1): the startup replay loops pages
/// with a paint between instead of slurping a giant journal in one HTTP
/// round trip, and the idle poller self-paces on reconnect backlogs.
/// Absent `?limit=` means this (old clients keep working, now bounded).
pub(crate) const EVENTS_PAGE_LIMIT: usize = 1000;

fn events_cache() -> &'static Mutex<PathCache<EventsCursor>> {
    static CACHE: OnceLock<Mutex<PathCache<EventsCursor>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(PathCache {
            map: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

/// Refresh the events cursor after one of our own appends (perf doc §12):
/// the common case keeps a poll from ever re-scanning. `written` is the byte
/// count this handle just appended; a foreign writer (another handle/task)
/// interleaving a row between our write and this stat would make the recorded
/// tip and checkpoints describe bytes we didn't write — a tip below the
/// foreign row would then starve a poller parked at that row. Detect it by
/// the length delta and drop the cursor rather than publish it.
fn events_cache_touched(events_path: &Path, seq: u64, written: u64) {
    let Some(id) = file_id(events_path) else {
        return;
    };
    let mut cache = events_cache().lock().expect("events cache lock");
    let foreign = match cache.get_mut(events_path) {
        Some(entry) if entry.id.len.saturating_add(written) == id.len => {
            let prev_len = entry.id.len;
            entry.id = id;
            entry.max_seq = Some(entry.max_seq.map_or(seq, |m| m.max(seq)));
            let base = entry.checkpoints.last().map(|&(_, off)| off).unwrap_or(0);
            if id.len.saturating_sub(base) >= EVENTS_CHECKPOINT_BYTES {
                entry.checkpoints.push((seq, prev_len));
                if entry.checkpoints.len() > EVENTS_CHECKPOINT_CAP {
                    let excess = entry.checkpoints.len() - EVENTS_CHECKPOINT_CAP;
                    entry.checkpoints.drain(..excess);
                }
            }
            false
        }
        Some(_) => true,
        None => false,
    };
    if foreign {
        cache.evict(events_path);
    }
}

/// One events-journal row: `seq` drives the replay cursor, `payload`
/// borrows the raw JSON — no parse → re-serialize round trip on the hot
/// poll path (perf doc §12).
#[derive(Deserialize)]
struct EventRow<'a> {
    seq: u64,
    #[serde(borrow)]
    payload: &'a RawValue,
}

/// Scan the events journal from the newest checkpoint at or before `since`,
/// returning `(rows with seq >= since, highest seq in the file)`. When
/// `collect` is false payloads are skipped (the `max_event_seq` path).
type EventsScan = (Vec<(u64, String)>, Option<u64>);

/// Newest checkpoint at or before `since`: the kept checkpoint list, the seek
/// offset, and the seq that offset should resume at. The file only grows in
/// production, but a checkpoint is verified against its row before use, so an
/// out-of-band rewrite falls back to a full scan instead of serving garbage.
fn checkpoint_resume(
    events_path: &Path,
    meta_len: u64,
    since: u64,
) -> (Vec<(u64, u64)>, u64, Option<u64>) {
    let cache = events_cache().lock().expect("events cache lock");
    match cache.get(events_path) {
        Some(entry) if meta_len >= entry.id.len => {
            let kept: Vec<(u64, u64)> = entry
                .checkpoints
                .iter()
                .copied()
                .filter(|&(_, off)| off <= meta_len)
                .collect();
            match kept.iter().rev().find(|&&(seq, _)| seq <= since).copied() {
                Some((seq, off)) => (kept, off, Some(seq)),
                None => (Vec::new(), 0, None),
            }
        }
        _ => (Vec::new(), 0, None),
    }
}

/// Highest seq already cached below the resume point, so the fresh max covers
/// the whole file, not just the scanned tail.
fn cached_max_seq(events_path: &Path, seek_to: u64) -> Option<u64> {
    if seek_to == 0 {
        return None;
    }
    events_cache()
        .lock()
        .expect("events cache lock")
        .get(events_path)
        .and_then(|e| e.max_seq)
}

fn scan_events(
    events_path: &Path,
    since: u64,
    collect: bool,
    limit: usize,
) -> io::Result<EventsScan> {
    // A zero page serves nothing: without this the `out.len() >= limit`
    // check below runs only after the first push and returns one row.
    if collect && limit == 0 {
        return Ok((Vec::new(), None));
    }
    // Cursor is the next seq to serve (inclusive): initial 0 serves seq 0,
    // and `next_seq = max + 1` resumes without loss or duplication.
    // Fast path: the journal is byte-identical to a previous scan and the
    // cursor is past everything served — the idle poll. No file open.
    // Only a drained scan publishes a servable tip (a page-limited scan
    // stops early, so its `max_seq` is not the file end — §1).
    if let Some(id) = file_id(events_path) {
        let cache = events_cache().lock().expect("events cache lock");
        if let Some(entry) = cache.get(events_path) {
            if entry.id == id && entry.drained && entry.max_seq.is_none_or(|m| since > m) {
                return Ok((Vec::new(), entry.max_seq));
            }
        }
    }
    let meta_len = fs::metadata(events_path)?.len();
    // Resume from the newest checkpoint at or before the cursor.
    let (mut checkpoints, mut seek_to, seek_seq) = checkpoint_resume(events_path, meta_len, since);
    let mut file = File::open(events_path)?;
    // Checkpoint beyond EOF (a shrink raced the stat): full scan instead.
    if seek_to > meta_len {
        seek_to = 0;
        checkpoints.clear();
    }
    if seek_to > 0 {
        file.seek(SeekFrom::Start(seek_to))?;
        let probe = {
            let mut probe_reader = BufReader::new(&file);
            let mut probe = String::new();
            match probe_reader.read_line(&mut probe) {
                Ok(_) => serde_json::from_str::<EventRow<'_>>(&probe)
                    .ok()
                    .map(|r| r.seq),
                Err(_) => None,
            }
        };
        if probe != seek_seq {
            seek_to = 0;
            checkpoints.clear();
        }
        file.seek(SeekFrom::Start(seek_to))?;
    }
    // Checkpoints from before the resume point would interleave with the
    // fresh ones appended during the scan, leaving the vector unsorted and
    // breaking `iter().rev().find(...)` and `events_cache_touched`'s
    // `last()` base. A checkpoint at or before the resume point is kept.
    checkpoints.retain(|&(_, off)| off <= seek_to);
    let mut reader = BufReader::new(file);
    let mut max: Option<u64> = cached_max_seq(events_path, seek_to);
    let mut next_chunk_at = seek_to.saturating_add(EVENTS_CHECKPOINT_BYTES);
    let mut out = Vec::new();
    let mut off = seek_to;
    let mut line = String::new();
    let mut drained = false;
    loop {
        line.clear();
        let n = match reader.read_line(&mut line) {
            Ok(0) => {
                drained = true;
                break;
            }
            Ok(n) => n,
            // Torn read: stop like EOF (matches `for_each_line`), but the
            // end wasn't reached — don't publish a servable tip.
            Err(_) => break,
        };
        let row_start = off;
        off += n as u64;
        let Ok(row) = serde_json::from_str::<EventRow<'_>>(line.trim_end()) else {
            continue;
        };
        max = Some(max.map_or(row.seq, |m| m.max(row.seq)));
        if row_start >= next_chunk_at {
            checkpoints.push((row.seq, row_start));
            next_chunk_at = row_start.saturating_add(EVENTS_CHECKPOINT_BYTES);
        }
        if collect && row.seq >= since {
            out.push((row.seq, row.payload.get().to_owned()));
            // Page-limited serving (§1): stop after `limit` served rows.
            // `max`/checkpoints cover exactly the served prefix, so the
            // next page resumes from its checkpoint; `drained` stays false
            // so the fast path can't mistake this tip for EOF.
            if out.len() >= limit {
                break;
            }
        }
    }
    if let Some(id) = file_id(events_path) {
        // Publish only when the identity still describes what was parsed:
        // an append racing past the scan end would make `max` stale, and
        // the next poll then re-scans from its checkpoint instead.
        if id.len == off {
            if checkpoints.len() > EVENTS_CHECKPOINT_CAP {
                let excess = checkpoints.len() - EVENTS_CHECKPOINT_CAP;
                checkpoints.drain(..excess);
            }
            events_cache().lock().expect("events cache lock").insert(
                events_path,
                EventsCursor {
                    id,
                    max_seq: max,
                    checkpoints,
                    drained,
                },
            );
        }
    }
    Ok((out, max))
}

impl Session {
    fn session_dir() -> PathBuf {
        if let Some(dir) = env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("dex/sessions");
        }
        env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/dex/sessions"))
            .unwrap_or_else(|| PathBuf::from(".dex/sessions"))
    }

    fn cwd_slug(cwd: &str) -> String {
        let mut hash = 2166136261u64;
        for byte in cwd.as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(16777619);
        }
        format!("{}-{:016x}", cwd.replace(['/', '\\'], "-"), hash)
    }

    /// Default session name: `<workspace>-<7 chars>`, e.g. `dex-k3m9x2q`.
    /// The workspace part is the lowercased cwd basename with anything
    /// outside `[a-z0-9]` folded to `-`; the suffix is 7 k8s-style
    /// `[a-z0-9]` chars, unique per session (see `Session::new`).
    pub(crate) fn default_session_name(cwd: &str) -> String {
        format!("{}-{}", Self::workspace_slug(cwd), Self::random_suffix(7))
    }

    /// The cwd basename, lowercased, anything outside `[a-z0-9]` folded to `-`,
    /// runs collapsed and trimmed, capped at 32 chars, `session` when nothing
    /// survives. Shared by the session name and id so both carry the workspace
    /// *name* — the basename, not the path, so `/srv/dex` and `~/dex` agree.
    fn workspace_slug(cwd: &str) -> String {
        let base = std::path::Path::new(cwd)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let mut slug: String = base
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        while slug.contains("--") {
            slug = slug.replace("--", "-");
        }
        slug = slug.trim_matches('-').to_string();
        if slug.is_empty() {
            slug = "session".to_string();
        }
        // ASCII-only by construction, so byte truncation is char-safe.
        if slug.len() > 32 {
            slug.truncate(32);
            slug = slug.trim_end_matches('-').to_string();
            if slug.is_empty() {
                slug = "session".to_string();
            }
        }
        slug
    }

    /// `n` random `[a-z0-9]` chars (`n <= 16`) sourced from a v4 UUID (OS RNG,
    /// already a dependency). `% 36` per byte is mildly biased and a v4 UUID
    /// pins the version/variant bits (bytes 6 and 8), so 16 chars is ~78 bits
    /// rather than 36^16 — still far past collision-free for one sessions dir.
    fn random_suffix(n: usize) -> String {
        const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        debug_assert!(n <= 16, "a v4 UUID only carries 16 bytes");
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        bytes[..n]
            .iter()
            .map(|b| ALPHABET[usize::from(*b % 36)] as char)
            .collect()
    }

    /// Session id: `<workspace>-<16 k8s-style [a-z0-9] chars>`, e.g.
    /// `dex-k3m9x2qp7w4n8t5v`. The workspace prefix is the workspace *name*
    /// (basename, not path), so an id shown without its header hints where it
    /// came from — the quit-time resume hint, daemon logs. Uniqueness still
    /// rests on the 16 random chars, which must cover every sessions directory
    /// at once because the id is both the JSONL filename and the daemon
    /// registry key. Creation order comes from the header `timestamp`, not the
    /// id.
    fn new_id(cwd: &str) -> String {
        format!("{}-{}", Self::workspace_slug(cwd), Self::random_suffix(16))
    }

    pub(crate) fn new(cwd: String, name: Option<String>) -> io::Result<Self> {
        // Unnamed sessions default to `<workspace>-<7 chars>`; an explicit
        // `--name`/`/name` (or `Some` from the daemon request) always wins.
        let name = match name {
            Some(n) if !n.is_empty() => Some(n),
            _ => Some(Self::default_session_name(&cwd)),
        };
        let dir = Self::session_dir().join(Self::cwd_slug(&cwd));
        fs::create_dir_all(&dir)?;
        // The id is both the filename and the daemon registry key, so
        // uniqueness rests on its 16 random chars (see `new_id`). `create_new`
        // refuses a name that is already taken instead of appending a second
        // header to someone else's session; a collision just draws a new id.
        let mut attempts = 0;
        let (id, path, mut file) = loop {
            attempts += 1;
            let id = Self::new_id(&cwd);
            let path = dir.join(format!("{}.jsonl", id));
            match fs::OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(&path)
            {
                Ok(file) => break (id, path, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => continue,
                Err(e) => return Err(e),
            }
        };
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: id.clone(),
            timestamp: Self::now_iso(),
            cwd,
            name,
        };
        let line = serde_json::to_string(&header).map_err(io::Error::other)?;
        writeln!(file, "{}", line)?;
        let mut session = Self {
            header,
            path: Some(path),
            journal: None,
            events_journal: None,
        };
        let skills = crate::skills::discover_skills(&crate::skills::skill_dirs());
        session.record_skills(&skills);
        Ok(session)
    }

    /// Record the skills loaded when the session starts as a `session_state`
    /// entry (key `skills`), so the JSONL documents which skills the system
    /// prompt advertised at session start. Failures are swallowed: a bad
    /// skills record must not fail session creation.
    fn record_skills(&mut self, skills: &[crate::core::types::Skill]) {
        if skills.is_empty() {
            return;
        }
        let value = skills_state_value(skills);
        let _ = self.set_state("skills", &value);
    }

    /// Read + validate the session header, returning it with the handle
    /// positioned just past it for callers that keep scanning (`from_path`).
    fn read_header(reader: &mut BufReader<File>) -> io::Result<SessionHeader> {
        let mut first = String::new();
        if reader.read_line(&mut first)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "empty session file",
            ));
        }
        let header: SessionHeader = serde_json::from_str(first.trim_end()).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad session header: {}", e),
            )
        })?;
        if header.entry_type != "session" || header.id.is_empty() || header.cwd.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid session metadata",
            ));
        }
        if header.version != SESSION_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported session version {}", header.version),
            ));
        }
        Ok(header)
    }

    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        // Header only: entry ids are random (see `next_id`), so opening never
        // needs the old line-count scan — O(1) no matter the history size
        // (perf doc §§22/28).
        let mut reader = BufReader::new(File::open(path)?);
        let header = Self::read_header(&mut reader)?;
        Ok(Self {
            header,
            path: Some(path.to_path_buf()),
            journal: None,
            events_journal: None,
        })
    }

    /// Events-only opener for the journal hot path (perf doc §22):
    /// `journal_event` appends stream events dozens–hundreds of times per
    /// turn. Same header-only open as `from_path`; the separate name documents
    /// that only `append_event` (whose seq comes from the daemon) ever runs
    /// on this handle.
    pub(crate) fn from_path_for_events(path: &Path) -> io::Result<Self> {
        Self::from_path(path)
    }

    /// Derive a child run's transcript path without touching the disk
    /// (§24.3): generation 0 keeps the V1 `agents/<agent_id>-<name>.jsonl`
    /// scheme; resume generations append `.g<N>` so a resume never
    /// clobbers its parent. The manager and the child body both derive
    /// through here, so registry and file agree by construction.
    pub(crate) fn child_path(
        parent_path: &Path,
        agent_id: &str,
        name: &str,
        generation: u32,
    ) -> PathBuf {
        let dir = parent_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("agents");
        if generation == 0 {
            dir.join(format!("{agent_id}-{name}.jsonl"))
        } else {
            dir.join(format!("{agent_id}-{name}.g{generation}.jsonl"))
        }
    }

    /// Child-agent JSONL (plan §16): `agents/<agent_id>-<name>[.g<N>].jsonl`
    /// beside the parent session file, with the same header and marker
    /// discipline (`turn_start`/`turn_complete`/`turn_failed` — a crash
    /// loses at most the in-flight event). The file lives outside the
    /// parent transcript: `list_all`/`list` read only direct `.jsonl`
    /// files in each session directory and the loaders take explicit
    /// paths, so `agents/*` is never ingested into the parent history.
    pub(crate) fn child(
        parent_path: &Path,
        cwd: &str,
        agent_id: &str,
        name: &str,
        generation: u32,
    ) -> io::Result<Self> {
        let path = Self::child_path(parent_path, agent_id, name, generation);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: if generation == 0 {
                format!("{agent_id}-{name}")
            } else {
                format!("{agent_id}-{name}.g{generation}")
            },
            timestamp: Self::now_iso(),
            cwd: cwd.to_string(),
            name: Some(format!(
                "{name} (child of {})",
                parent_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("session")
            )),
        };
        let line = serde_json::to_string(&header).map_err(io::Error::other)?;
        // The agent id is session-scoped and the manager counter restarts
        // after a daemon restart, so a resumed session can collide with a
        // prior run's file: append (never truncate) keeps the interrupted
        // run's record readable, and random entry ids (see `next_id`) stay
        // unique across the seam without a line-count scan.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", line)?;
        Ok(Self {
            header,
            path: Some(path),
            journal: None,
            events_journal: None,
        })
    }

    pub(crate) fn in_memory(cwd: String) -> Self {
        Self {
            header: SessionHeader {
                entry_type: "session".to_string(),
                version: SESSION_VERSION,
                id: Self::new_id(&cwd),
                timestamp: Self::now_iso(),
                cwd,
                name: None,
            },
            path: None,
            journal: None,
            events_journal: None,
        }
    }

    pub(crate) fn open_or_continue(
        cwd: String,
        session_path: Option<&Path>,
        no_session: bool,
    ) -> io::Result<Self> {
        if no_session {
            return Ok(Self::in_memory(cwd));
        }
        if let Some(path) = session_path {
            if path.exists() {
                return Self::from_path(path);
            }
        }
        Self::new(cwd, None)
    }

    pub(crate) fn list(cwd: &str) -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let dir = Self::session_dir().join(Self::cwd_slug(cwd));
        let mut sessions = scan_jsonl_dir(&dir);
        sort_newest_first(&mut sessions);
        Ok(sessions)
    }

    /// mtime of one workspace's sessions dir, for the slash-popup cache key
    /// (perf doc §29): one stat instead of a readdir + header parses per
    /// frame while `/resume ...` sits in the composer.
    pub(crate) fn list_dir_mtime(cwd: &str) -> Option<SystemTime> {
        std::fs::metadata(Self::session_dir().join(Self::cwd_slug(cwd)))
            .ok()?
            .modified()
            .ok()
    }

    /// List every persisted session across all workspaces (registry rebuild
    /// and disk-backed `GET /api/sessions`).
    pub(crate) fn list_all() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let base = Self::session_dir();
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

    /// Fast id→path lookup (perf doc §1): the session id is the JSONL
    /// filename, so match by file name across workspace dirs without
    /// opening every file — `list_all` opens + header-parses each one just
    /// to locate a single session. Each filename hit is confirmed by one
    /// header-only `from_path` read; anything unconfirmed (renamed stems,
    /// legacy files) falls through to the `list_all` scan at the caller.
    pub(crate) fn find_by_id_filename(sid: &str) -> Option<PathBuf> {
        let q = sid.to_ascii_lowercase();
        let mut exact: Option<PathBuf> = None;
        let mut prefixed: Vec<PathBuf> = Vec::new();
        if let Ok(slugs) = fs::read_dir(Self::session_dir()) {
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
            if Self::from_path(&path).is_ok_and(|s| s.id() == sid) {
                return Some(path);
            }
        }
        // Deterministic on colliding prefixes: filesystem order is
        // unspecified, so sort before confirming.
        prefixed.sort_unstable();
        prefixed.into_iter().find(|path| {
            Self::from_path(path).is_ok_and(|s| {
                let id = s.id().to_ascii_lowercase();
                id.starts_with(&q) || q.starts_with(&id)
            })
        })
    }

    /// List one session's child runs (§16): every JSONL under the session's
    /// `agents/` directory, each with its header and last turn state —
    /// `"interrupted"` is a `turn_start` with no terminal marker (a crashed
    /// or daemon-restart-killed child). Deliberately separate from
    /// `list`/`list_all`, whose loaders must keep excluding `agents/*`.
    /// Sorted by header timestamp, newest first, like the other listings.
    pub(crate) fn list_children(
        parent_path: &Path,
    ) -> io::Result<Vec<(PathBuf, SessionHeader, &'static str)>> {
        let dir = Self::agents_dir(parent_path);
        let mut children = scan_jsonl_dir(&dir);
        sort_newest_first(&mut children);
        // Enrich with the turn state after the shared scan/sort: the state
        // is derived per path, so the newest-first order is unaffected.
        Ok(children
            .into_iter()
            .map(|(path, header)| {
                let turn_state = Self::last_turn_state(&path);
                (path, header, turn_state)
            })
            .collect())
    }

    /// Directory holding a session's child-agent transcripts (`agents/`
    /// beside the session file). Single spelling shared by `list_children`
    /// and the §31 listing pre-scan.
    pub(crate) fn agents_dir(parent_path: &Path) -> PathBuf {
        parent_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("agents")
    }

    /// Listing helper (perf doc §31): child-run counts for one agents dir —
    /// total runs plus interrupted ones. One header-only dir scan plus one
    /// turn-state scan per child; `list_sessions` calls it once per distinct
    /// dir instead of once per session file.
    pub(crate) fn count_children(dir: &Path) -> (usize, usize) {
        let children = scan_jsonl_dir(dir);
        let interrupted = children
            .iter()
            .filter(|(path, _)| Self::last_turn_state(path) == "interrupted")
            .count();
        (children.len(), interrupted)
    }

    pub(crate) fn resume(cwd: &str, selector: &str) -> io::Result<Self> {
        let sessions = Self::list(cwd)?;
        let path = if let Ok(index) = selector.parse::<usize>() {
            sessions.get(index).map(|(path, _)| path.clone())
        } else {
            let candidate = PathBuf::from(selector);
            sessions
                .iter()
                .find(|(path, _)| {
                    path == &candidate
                        || path.file_name().and_then(|n| n.to_str()) == Some(selector)
                })
                .map(|(path, _)| path.clone())
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session not found"))?;
        Self::from_path(&path)
    }

    pub(crate) fn set_name(&mut self, name: String) -> io::Result<()> {
        self.header.name = Some(name.clone());
        let entry = SessionInfoEntry {
            entry_type: "session_info".to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            name,
        };
        self.append_line(&entry)
    }

    pub(crate) fn append_message(&mut self, message: &ChatMessage) -> io::Result<()> {
        let id = self.next_id();
        let timestamp = Self::now_iso();
        self.append_line_inner(
            &SessionMessageEntry {
                entry_type: "message",
                id: &id,
                timestamp: &timestamp,
                message,
            },
            false,
        )?;
        // Invalidate the parsed-history snapshot (perf doc §11) instead of
        // fusing write+stat+push: two `Session` handles (daemon turn + `!`
        // shell, or concurrent appends) interleave write;stat;push so the
        // second stat sees both rows while its vec holds one — publishing
        // a new FileId with a stale/misordered tail. Eviction forces the
        // next load to rescan (one scan per turn, never per frame).
        if let Some(path) = self.path.as_deref() {
            history_cache()
                .lock()
                .expect("history cache lock")
                .evict(path);
        }
        Ok(())
    }

    pub(crate) fn clear_messages(&mut self) -> io::Result<()> {
        let entry = SessionClearEntry {
            entry_type: "clear".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
        };
        self.append_line_inner(&entry, false)?;
        // Same invalidate-not-push as `append_message` (see above).
        if let Some(path) = self.path.as_deref() {
            history_cache()
                .lock()
                .expect("history cache lock")
                .evict(path);
        }
        Ok(())
    }

    /// Atomically replace the journal's message tail after compaction:
    /// header + `clear` + every message after the system prompt go to a
    /// temp file in the same directory, fsync, then `rename` over the
    /// journal. The old clear+N-appends sequence left a crash window
    /// mid-rewrite — a `clear` plus a partial tail permanently dropped the
    /// pre-compaction history. A rename is atomic: readers see the old or
    /// the new history, never a torn one. The append handle reopens (it
    /// pointed at the renamed-away inode) and the snapshot publishes fused
    /// with its new identity, mirroring the loader (System role skipped).
    /// In-memory sessions (no path) are a no-op, like the appends were.
    pub(crate) fn rewrite_messages(&mut self, messages: &[ChatMessage]) -> io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        // Non-`.jsonl` suffix: the session listing scans `*.jsonl`, so a
        // half-written temp never appears as a phantom session.
        let tmp = path.with_extension(format!("rewrite-{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let header_line = serde_json::to_string(&self.header).map_err(io::Error::other)?;
        let mut tmp_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        writeln!(tmp_file, "{header_line}")?;
        let clear = SessionClearEntry {
            entry_type: "clear".into(),
            id: Self::random_suffix(8),
            timestamp: Self::now_iso(),
        };
        writeln!(
            tmp_file,
            "{}",
            serde_json::to_string(&clear).map_err(io::Error::other)?
        )?;
        // Preserve the non-message tail: `session_state` rows (plan, model,
        // skills, verify, last_error, and the change ledger `/undo` reads)
        // are not part of `messages`, but the append path kept them and the
        // loaders read the whole file last-write-wins. Reload the surviving
        // values before the rename and re-emit them, or every compaction
        // silently empties `/undo` and drops the persisted plan.
        let preserved = load_session_state(&path).unwrap_or_default();
        for message in messages.iter().skip(1) {
            let entry = SessionMessageEntry {
                entry_type: "message",
                id: &Self::random_suffix(8),
                timestamp: &Self::now_iso(),
                message,
            };
            writeln!(
                tmp_file,
                "{}",
                serde_json::to_string(&entry).map_err(io::Error::other)?
            )?;
        }
        let mut preserved: Vec<(String, String)> = preserved.into_iter().collect();
        preserved.sort();
        for (key, value) in preserved {
            let state_entry = SessionStateEntry {
                entry_type: "session_state".into(),
                id: Self::random_suffix(8),
                timestamp: Self::now_iso(),
                key,
                value,
            };
            writeln!(
                tmp_file,
                "{}",
                serde_json::to_string(&state_entry).map_err(io::Error::other)?
            )?;
        }
        tmp_file.flush()?;
        tmp_file.sync_data()?;
        drop(tmp_file);
        fs::rename(&tmp, &path)?;
        // Best-effort dir fsync so the rename itself survives a crash.
        if let Some(parent) = path.parent() {
            if let Ok(dir) = File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        self.journal = Some(Self::open_append(&path)?);
        if let Some(id) = file_id(&path) {
            let kept: Vec<ChatMessage> = messages
                .iter()
                .skip(1)
                .filter(|m| m.role != Role::System)
                .cloned()
                .collect();
            history_cache()
                .lock()
                .expect("history cache lock")
                .insert(&path, (id, kept));
        }
        Ok(())
    }

    pub(crate) fn turn_event(&mut self, event: &str) -> io::Result<()> {
        self.turn_event_with_tier(event, None)
    }

    /// `turn_start` carrying the complexity-router tier that chose the
    /// turn's model (`None` = routing off or an explicit model pick).
    /// The tier rides the existing event — no new log — and recovery
    /// still keys on the `turn_start` type alone, so a crash loses at
    /// most the in-flight event.
    pub(crate) fn turn_event_with_tier(
        &mut self,
        event: &str,
        tier: Option<&str>,
    ) -> io::Result<()> {
        let entry = SessionEventEntry {
            entry_type: event.to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            tier: tier.map(str::to_string),
        };
        self.append_line(&entry)
    }

    /// Durable side-effect intent: recorded BEFORE the tool executes so a
    /// restart can see effects that started but never completed.
    pub(crate) fn effect_start(
        &mut self,
        tool_call_id: &str,
        name: &str,
        input_hash: &str,
    ) -> io::Result<()> {
        let entry = SessionEffectEntry {
            entry_type: "effect_start".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            tool_call_id: tool_call_id.into(),
            name: name.into(),
            input_hash: input_hash.into(),
            ok: None,
        };
        self.append_line(&entry)
    }

    /// Durable side-effect outcome: recorded AFTER the tool executed.
    pub(crate) fn effect_result(&mut self, tool_call_id: &str, ok: bool) -> io::Result<()> {
        let entry = SessionEffectEntry {
            entry_type: "effect_result".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            tool_call_id: tool_call_id.into(),
            name: String::new(),
            input_hash: String::new(),
            ok: Some(ok),
        };
        self.append_line(&entry)
    }

    pub(crate) fn set_state(&mut self, key: &str, value: &str) -> io::Result<()> {
        let entry = SessionStateEntry {
            entry_type: "session_state".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
            key: key.into(),
            value: value.into(),
        };
        self.append_line(&entry)
    }

    /// Append handle used by both journals: create-on-demand, always append.
    fn open_append(path: &Path) -> io::Result<File> {
        fs::OpenOptions::new().create(true).append(true).open(path)
    }

    fn append_line<T: Serialize>(&mut self, entry: &T) -> io::Result<()> {
        self.append_line_inner(entry, true)
    }

    fn append_line_inner<T: Serialize>(&mut self, entry: &T, touch: bool) -> io::Result<()> {
        // Path is only needed to open the handle — avoid allocating per line.
        if self.journal.is_none() {
            let Some(path) = self.path.as_deref() else {
                return Ok(());
            };
            self.journal = Some(Self::open_append(path)?);
        }
        let line = serde_json::to_string(entry).map_err(io::Error::other)?;
        let file = self.journal.as_mut().expect("journal handle set above");
        writeln!(file, "{line}")?;
        // Don't fsync every line — rely on the OS buffer + periodic flush.
        // Sync only when
        // durability matters (turn boundaries / effect journal) or when
        // DEX_DURABLE=1 is set for strict recovery testing.
        // `clear` is rare (compaction rewrites atomically now; `/clear` is
        // user intent) — sync it so the fold point itself is durable.
        let durable = durable_journal()
            || line.contains("\"type\":\"turn_")
            || line.contains("\"type\":\"effect_")
            || line.contains("\"type\":\"clear\"");
        if durable {
            file.sync_data()?;
        }
        // Refresh the history snapshot's identity: our own append never
        // invalidates it (see `history_cache_touch`). Message/clear writers
        // pass `touch: false` and fuse the refresh with their vector update
        // under one lock instead.
        if touch {
            if let Some(path) = self.path.as_deref() {
                history_cache_touch(path, line.len() as u64 + 1);
            }
        }
        Ok(())
    }

    /// Opaque per-line entry id. Random rather than counted: nothing ever
    /// reads these back (no correlation, no ordering — creation order comes
    /// from file position), so uniqueness is the only requirement, and a
    /// counter would force every open to scan the file first (perf doc §28).
    /// 8 k8s-style chars (~41 bits) against the UUID-backed `random_suffix`
    /// the session-id scheme already trusts for the stronger filename
    /// uniqueness.
    fn next_id(&self) -> String {
        Self::random_suffix(8)
    }
    fn now_iso() -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        chrono::DateTime::from_timestamp(secs as i64, 0)
            .unwrap_or_default()
            .to_rfc3339()
    }
    pub(crate) fn id(&self) -> &str {
        &self.header.id
    }
    /// The workspace the session was recorded from (its header cwd). The tool
    /// workspace is the daemon's cwd, so the two differ after a cross-directory
    /// reattach.
    pub(crate) fn cwd(&self) -> &str {
        &self.header.cwd
    }
    pub(crate) fn name(&self) -> Option<&str> {
        self.header.name.as_deref()
    }
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    /// Display helper for `/session`: number of recorded turns
    /// (`turn_start` markers). A rare explicit user command, so a
    /// streaming scan is fine — and it matches the label better than the
    /// old journal-line counter did.
    pub(crate) fn count_turns(&self) -> usize {
        let Some(path) = self.path.as_deref() else {
            return 0;
        };
        let mut turns = 0usize;
        let _ = for_each_line(path, |line| {
            if line.contains("\"type\":\"turn_start\"") {
                turns += 1;
            }
        });
        turns
    }
    pub(crate) fn display_name(&self) -> String {
        self.name().unwrap_or(self.id()).to_string()
    }

    /// Path of the per-session SSE event journal (`<id>.events.jsonl`).
    pub(crate) fn events_path(&self) -> Option<PathBuf> {
        self.path.as_ref().map(|p| p.with_extension("events.jsonl"))
    }

    /// Append one numbered stream event to the event journal. `seq` is
    /// assigned by the daemon (monotonic per session, seeded from disk on
    /// restart); the journal is what `/api/sessions/{id}/events?since=` replays.
    pub(crate) fn append_event(&mut self, seq: u64, payload: &str) -> io::Result<()> {
        // events_path() allocates (with_extension) — only compute it when the
        // handle needs to be opened, not once per streamed delta.
        if self.events_journal.is_none() {
            let Some(path) = self.events_path() else {
                return Ok(());
            };
            self.events_journal = Some(Self::open_append(&path)?);
        }
        let file = self
            .events_journal
            .as_mut()
            .expect("events journal handle set above");
        // The payload comes straight from serde_json::to_string, so it is
        // already valid JSON: the envelope is composed in place instead of
        // parse -> json! -> re-serialize per streamed delta. An empty payload
        // (serialization failed upstream) becomes an empty JSON string so the
        // line stays parseable for replay.
        let payload = if payload.is_empty() { "\"\"" } else { payload };
        let ts = Self::now_iso();
        let line = format!("{{\"seq\":{seq},\"ts\":\"{ts}\",\"payload\":{payload}}}");
        writeln!(file, "{line}")?;
        // Event journal is replayable but not critical for crash recovery —
        // sync only for terminal events or when DEX_DURABLE=1. (The old sniff
        // grepped for the Rust variant names, which never appear in the
        // serialized `type` field, so it never fired without DEX_DURABLE.)
        let durable = durable_journal()
            || payload.contains("\"type\":\"turn_complete\"")
            || payload.contains("\"type\":\"turn_failed\"");
        if durable {
            file.sync_data()?;
        }
        if let Some(events_path) = self.events_path() {
            events_cache_touched(&events_path, seq, line.len() as u64 + 1);
        }
        Ok(())
    }

    /// Replay stream events with `seq >= since`, in order (`since` is the
    /// next seq to serve, inclusive — `next_seq` chains without loss or
    /// duplication). `path` is the
    /// SESSION file; the journal lives at `<session>.events.jsonl`.
    /// Served from the steady-state cursor when the journal hasn't grown
    /// (one `stat`, no file open — perf doc §12), otherwise scanned from
    /// the newest checkpoint at or before `since`.
    pub(crate) fn load_events(
        path: &Path,
        since: u64,
        limit: usize,
    ) -> io::Result<Vec<(u64, String)>> {
        let events_path = path.with_extension("events.jsonl");
        Ok(scan_events(&events_path, since, true, limit)?.0)
    }

    /// Highest event seq recorded for a session (`None` when no seq is
    /// journaled yet — distinct from a journal holding exactly seq 0).
    /// Served from the cursor without opening the file when the journal
    /// hasn't grown (perf doc §12).
    pub(crate) fn max_event_seq(path: &Path) -> Option<u64> {
        let events_path = path.with_extension("events.jsonl");
        // The tip query must see the whole file (a limit here would corrupt
        // seq seeding) — only serving scans page (§1).
        scan_events(&events_path, u64::MAX, false, usize::MAX)
            .ok()
            .and_then(|(_, max)| max)
    }

    /// Terminal state of the most recent turn: "complete", "failed", or
    /// "interrupted" when a `turn_start` has no terminal entry after it.
    pub(crate) fn last_turn_state(path: &Path) -> &'static str {
        // Streamed via `for_each_line`; sessions hold thousands of
        // non-marker entries. Open failure still reads as "unknown"; the
        // helper stops mid-scan on read errors like EOF.
        let mut state = "none";
        let scan = for_each_line(path, |line| {
            // Turn entries serialize as {"type":"turn_*",...}; skip parsing
            // everything else.
            if !line.contains("\"type\":\"turn_") {
                return;
            }
            let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
                return;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("turn_start") => state = "interrupted",
                Some("turn_complete") => state = "complete",
                Some("turn_failed") => state = "failed",
                _ => {}
            }
        });
        if scan.is_err() {
            return "unknown";
        }
        state
    }

    /// Single-pass listing summary (perf doc §31): `(message_count,
    /// turn_state)` with `load_messages_from_session` / `last_turn_state`
    /// semantics — one open, one scan. Lines that can carry neither (effect,
    /// state and header entries) skip parsing via a compact-spelling
    /// substring prefilter; anything with a `type` key that misses the
    /// prefilter (non-compact spacing) is classified by parsing, so the
    /// count stays exact. `message_count` matches a full load
    /// (malformed lines excluded the same way, `clear` folds, System role
    /// skipped); open failure is an `Err` (callers map it to `unknown` / 0
    /// as today). Stays quiet on malformed lines — unlike the loader, this
    /// runs per file per listing.
    pub(crate) fn scan_summary(path: &Path) -> io::Result<(usize, String)> {
        let mut reader = BufReader::new(File::open(path)?);
        let mut count = 0usize;
        let mut state = "none";
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let text = line.trim_end();
            if text.is_empty() {
                continue;
            }
            // 0 = message, 1 = clear, 2 = turn marker.
            let kind: Option<u8> = if text.contains("\"type\":\"message\"") {
                Some(0)
            } else if text.contains("\"type\":\"clear\"") {
                Some(1)
            } else if text.contains("\"type\":\"turn_") {
                Some(2)
            } else if text.contains("\"type\"") {
                match serde_json::from_str::<Value>(text)
                    .ok()
                    .and_then(|v| v.get("type").and_then(Value::as_str).map(str::to_owned))
                {
                    Some(s) if s == "message" => Some(0),
                    Some(s) if s == "clear" => Some(1),
                    Some(s) if s.starts_with("turn_") => Some(2),
                    _ => None,
                }
            } else {
                None
            };
            match kind {
                // Same exclusion rules as the loader: unparsable lines and
                // the System role (kept out of the transcript) don't count.
                Some(0) => {
                    if let Ok(value) = serde_json::from_str::<Value>(text) {
                        match serde_json::from_value::<ChatMessage>(value) {
                            Ok(msg) if msg.role != Role::System => count += 1,
                            _ => {}
                        }
                    }
                }
                Some(1) => count = 0,
                Some(2) => {
                    if let Ok(value) = serde_json::from_str::<Value>(text) {
                        match value.get("type").and_then(Value::as_str) {
                            Some("turn_start") => state = "interrupted",
                            Some("turn_complete") => state = "complete",
                            Some("turn_failed") => state = "failed",
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Ok((count, state.to_string()))
    }
}

/// Serialize discovered skills for the session-start `skills` state entry:
/// a JSON array of `{name, description, path}` objects.
fn skills_state_value(skills: &[crate::core::types::Skill]) -> String {
    let entries: Vec<serde_json::Value> = skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "path": s.path.display().to_string(),
            })
        })
        .collect();
    serde_json::to_string(&entries).unwrap_or_default()
}

pub(crate) fn load_messages_from_session(path: &Path) -> io::Result<Vec<ChatMessage>> {
    // Snapshot hit: byte-identical to a previous parse — no file IO at all.
    if let Some(mut messages) = history_cache_get(path) {
        repair_dangling_tool_calls(&mut messages);
        return Ok(messages);
    }
    let (mut messages, _) = scan_history(path)?;
    repair_dangling_tool_calls(&mut messages);
    Ok(messages)
}

/// Same single scan as [`load_messages_from_session`] plus the stored plan
/// (perf doc §1): reattach used to re-scan the whole file for the plan
/// right after loading messages. On a snapshot hit this degrades to today's
/// two passes (the snapshot stores messages only); the cold path — the one
/// that blocks first paint — pays one.
pub(crate) fn load_messages_and_plan(
    path: &Path,
) -> io::Result<(Vec<ChatMessage>, crate::core::types::Plan)> {
    if let Some(mut messages) = history_cache_get(path) {
        repair_dangling_tool_calls(&mut messages);
        return Ok((messages, load_plan(path)));
    }
    let (mut messages, plan) = scan_history(path)?;
    repair_dangling_tool_calls(&mut messages);
    Ok((
        messages,
        plan.map(|s| crate::core::types::Plan::from_json(&s))
            .unwrap_or_default(),
    ))
}

/// Streaming history scan shared by the message loaders: message/clear
/// folding plus the last stored `plan` value (last write wins, like
/// `load_session_state`). Returns pre-repair messages; the caller repairs
/// its own copy so snapshot hits stay byte-identical to fresh parses.
fn scan_history(path: &Path) -> io::Result<(Vec<ChatMessage>, Option<String>)> {
    // Stream line by line: session files grow with history and only the
    // post-`clear` tail is kept.
    let mut reader = BufReader::new(File::open(path)?);
    let mut messages = Vec::new();
    let mut plan: Option<String> = None;
    let mut line = String::new();
    let mut line_no = 1; // header; matches the old skip(1) numbering
    let mut consumed = 0u64;
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        consumed += n as u64;
        line_no += 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line.trim_end()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[session] skipping bad line {} in {}: {}",
                    line_no,
                    path.display(),
                    e
                );
                continue;
            }
        };
        // The plan rides the same scan for free: this line is already
        // parsed as `Value` above, so two map lookups capture it — no
        // second full pass like the old `load_plan`-after-`load_messages`.
        if value.get("type").and_then(Value::as_str) == Some("session_state")
            && value.get("key").and_then(Value::as_str) == Some("plan")
        {
            if let Some(text) = value.get("value").and_then(Value::as_str) {
                plan = Some(text.to_string());
            }
        }
        if value.get("type").and_then(Value::as_str) == Some("clear") {
            messages.clear();
        } else if value.get("type").and_then(Value::as_str) == Some("message") {
            match serde_json::from_value::<ChatMessage>(value) {
                Ok(msg) if msg.role != Role::System => messages.push(msg),
                Ok(_) => {}
                Err(e) => eprintln!(
                    "[session] skipping unparseable message at line {} in {}: {}",
                    line_no,
                    path.display(),
                    e
                ),
            }
        }
    }
    // Cache the pre-repair snapshot when the file didn't grow mid-parse —
    // an append racing the scan would make the identity stale, so that
    // parse stays correct but uncached.
    if let Some(id) = file_id(path) {
        if id.len == consumed {
            history_cache_put(path, id, messages.clone());
        }
    }
    Ok((messages, plan))
}

/// Model-bound history: the full journal minus `!!` shell runs (saved
/// to history and shown in the TUI, but never sent to the LLM). Transcript
/// rebuilds keep the unfiltered [`load_messages_from_session`] so `!!`
/// stays visible there.
pub(crate) fn load_llm_messages_from_session(path: &Path) -> io::Result<Vec<ChatMessage>> {
    Ok(load_messages_from_session(path)?
        .into_iter()
        .filter(|m| !m.is_context_excluded())
        .collect())
}

/// Repair a transcript that ends mid-batch: an assistant tool call whose
/// result never landed (a crash between the assistant message and the tool
/// result, or a truncated journal line). Providers reject a `tool_calls`
/// batch without its results, so a resume would fail the very next request
/// until the dangling call was hand-edited out. Synthesize a deterministic
/// placeholder instead: the model sees an honest failure marker and can
/// re-issue the call after checking what actually happened.
///
/// Placeholders are inserted immediately after their assistant message (in
/// call order), not appended at the end, so causality survives even for a
/// historic middle-batch gap. In-memory only — the journal file keeps its
/// bytes, so every load re-synthesizes deterministically instead of
/// accumulating duplicates.
fn repair_dangling_tool_calls(messages: &mut Vec<ChatMessage>) {
    use std::collections::HashSet;
    let answered: HashSet<&str> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    // (assistant_idx, call_id) for calls with no result anywhere.
    let mut dangling: Vec<(usize, String)> = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        if msg.role != Role::Assistant {
            continue;
        }
        if let Some(calls) = &msg.tool_calls {
            for c in calls {
                if !answered.contains(c.id.as_str()) {
                    dangling.push((idx, c.id.clone()));
                }
            }
        }
    }
    // Insert in reverse so earlier indices stay valid; same-index inserts
    // iterate reversed to keep the original call order after the assistant.
    for (assistant_idx, id) in dangling.into_iter().rev() {
        let pos = (assistant_idx + 1).min(messages.len());
        messages.insert(
            pos,
            ChatMessage::tool_result(
                id,
                crate::core::format::model_tool_result(
                    "Error: tool result missing — the agent exited before it was recorded; the call may have executed. Verify the effect on disk before retrying.",
                ),
            ),
        );
    }
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

async fn read_first_line_async(path: PathBuf) -> Option<String> {
    use tokio::io::AsyncBufReadExt as _;
    let file = tokio::fs::File::open(&path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut first = String::new();
    match reader.read_line(&mut first).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(first),
    }
}

/// Async full-history load: streaming file, fast — short `spawn_blocking`.
pub(crate) async fn load_messages_from_session_async(
    path: PathBuf,
) -> io::Result<Vec<ChatMessage>> {
    tokio::task::spawn_blocking(move || load_messages_from_session(&path))
        .await
        .map_err(io::Error::other)?
}

impl Session {
    /// Async `list_all`: `JoinSet` (`spawn_blocking` per file, join, sort) —
    /// fixes the linear scan (S2 cold-start 50x10ms ~500ms → ~50ms parallel).
    pub(crate) async fn list_all_async() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        let base = Self::session_dir();
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
}

pub(crate) fn load_plan(path: &Path) -> crate::core::types::Plan {
    load_session_state(path)
        .ok()
        .and_then(|m| m.get("plan").cloned())
        .map(|s| crate::core::types::Plan::from_json(&s))
        .unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) fn save_plan(session: &mut Session, plan: &crate::core::types::Plan) -> io::Result<()> {
    session.set_state("plan", &plan.to_json())
}

pub(crate) fn load_session_state(
    path: &Path,
) -> io::Result<std::collections::HashMap<String, String>> {
    let mut state = std::collections::HashMap::new();
    let mut header = true;
    for_each_line(path, |line| {
        // Line 1 is the session header, not a state entry.
        if header {
            header = false;
            return;
        }
        // set_state writes {"type":"session_state",...}; quotes inside state
        // values are JSON-escaped, so this substring can only be the marker.
        if !line.contains("\"type\":\"session_state\"") {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        if let (Some(key), Some(val)) = (
            value.get("key").and_then(Value::as_str),
            value.get("value").and_then(Value::as_str),
        ) {
            state.insert(key.into(), val.into());
        }
    })?;
    Ok(state)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn unique_path(prefix: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let mut h = DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        let tid = h.finish();
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "{}-{}-{}-{}-{}.jsonl",
            prefix,
            std::process::id(),
            tid,
            nanos,
            nonce
        ))
    }

    #[test]
    fn clear_marker_removes_messages_during_recovery() {
        let path = unique_path("dex-session-test");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let message = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"old"}"#;
        let clear = r#"{"type":"clear","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", header, message, clear)).unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        // A clear followed by new messages loads again.
        fs::write(
            &path,
            format!("{}\n{}\n{}\n{}\n", header, message, clear, message),
        )
        .unwrap();
        assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
        // System-only sessions hold no displayable content.
        let system = r#"{"type":"message","id":"3","timestamp":"2020-01-01T00:00:00Z","role":"system","content":"note"}"#;
        fs::write(&path, format!("{}\n{}\n", header, system)).unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_state_last_write_wins_and_ignores_other_entries() {
        let path = unique_path("dex-session-state");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let first = r#"{"type":"session_state","id":"1","timestamp":"2020-01-01T00:00:01Z","key":"model","value":"old-model"}"#;
        let message = r#"{"type":"message","id":"2","timestamp":"2020-01-01T00:00:02Z","role":"user","content":"hi"}"#;
        let second = r#"{"type":"session_state","id":"3","timestamp":"2020-01-01T00:00:03Z","key":"model","value":"new-model"}"#;
        let provider = r#"{"type":"session_state","id":"4","timestamp":"2020-01-01T00:00:04Z","key":"provider","value":"openai-codex"}"#;
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                header, first, message, second, provider
            ),
        )
        .unwrap();
        let state = load_session_state(&path).unwrap();
        assert_eq!(state.get("model").map(String::as_str), Some("new-model"));
        assert_eq!(
            state.get("provider").map(String::as_str),
            Some("openai-codex")
        );
        assert_eq!(state.len(), 2);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn session_state_missing_file_is_an_error() {
        let path = unique_path("dex-session-state-missing");
        let _ = fs::remove_file(&path);
        assert!(load_session_state(&path).is_err());
    }

    /// Reasoning replay fields must survive the JSONL journal: the
    /// flattened `ChatMessage` (session.rs `SessionMessageEntry`) round-trips
    /// `reasoning_items` + `reasoning_content` so a resumed session keeps
    /// its reasoning thread instead of re-reasoning from scratch.
    #[test]
    fn message_round_trip_preserves_reasoning_replay_fields() {
        let path = unique_path("dex-session-reasoning");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let mut msg = serde_json::to_value(ChatMessage {
            role: Role::Assistant,
            content: Some("done".to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_items: Some(vec![serde_json::json!({
                "type": "reasoning",
                "id": "r1",
                "encrypted_content": "blob1",
            })]),
            reasoning_content: Some("step 1".to_string()),
        })
        .unwrap();
        msg["type"] = serde_json::json!("message");
        msg["id"] = serde_json::json!("1");
        msg["timestamp"] = serde_json::json!("2020-01-01T00:00:00Z");
        fs::write(&path, format!("{}\n{}\n", header, msg)).unwrap();
        let loaded = load_messages_from_session(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].reasoning_items.as_ref().unwrap()[0]["encrypted_content"],
            "blob1"
        );
        assert_eq!(loaded[0].reasoning_content.as_deref(), Some("step 1"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn llm_history_drops_excluded_shell_runs_but_transcript_keeps_them() {
        // `!!`: saved to history and shown in the TUI, never sent to
        // the LLM. The transcript rebuild uses the full load; the
        // model-bound load filters.
        use crate::core::types::BASH_EXCLUDED_NAME;
        let path = unique_path("dex-session-shell-exclude");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut session = Session::from_path(&path).unwrap();
        session
            .append_message(&ChatMessage::user("Ran `echo hi`\n```\nhi\n```"))
            .unwrap();
        session
            .append_message(&ChatMessage::user_named(
                "Ran `echo secret`\n```\nsecret\n```",
                BASH_EXCLUDED_NAME,
            ))
            .unwrap();
        drop(session);
        let full = load_messages_from_session(&path).unwrap();
        assert_eq!(full.len(), 2);
        let llm = load_llm_messages_from_session(&path).unwrap();
        assert_eq!(llm.len(), 1);
        assert!(llm[0].content_str().contains("echo hi"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn events_journal_replays_after_seq_cursor() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-events-test".into(), None).unwrap();
        s.append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
            .unwrap();
        s.append_event(1, r#"{"type":"assistant_text","data":"b"}"#)
            .unwrap();
        s.append_event(2, r#"{"type":"turn_complete","data":{"response":"done"}}"#)
            .unwrap();
        s.append_event(3, r#"{"type":"assistant_text","data":"c"}"#)
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let after = Session::load_events(&path, 2, usize::MAX).unwrap();
        let seqs: Vec<u64> = after.iter().map(|(seq, _)| *seq).collect();
        let texts: Vec<String> = after
            .iter()
            .map(|(_, p)| serde_json::from_str::<Value>(p).unwrap()["data"].to_string())
            .collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(texts.len(), 2);
        assert_eq!(Session::max_event_seq(&path), Some(3));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[test]
    fn history_cache_serves_appends_without_rescan() {
        let path = unique_path("dex-history-cache");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        s.append_message(&ChatMessage::user("one")).unwrap();
        // Miss parses; hit serves the snapshot.
        assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
        assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
        // The write funnel extends the snapshot: still no re-parse.
        s.append_message(&ChatMessage::user("two")).unwrap();
        let two = load_messages_from_session(&path).unwrap();
        assert_eq!(two.len(), 2);
        assert!(two[1].content_str().contains("two"));
        // `clear` folds the snapshot like the loader folds the file.
        s.clear_messages().unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        s.append_message(&ChatMessage::user("three")).unwrap();
        assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
        // An out-of-band rewrite misses and re-parses.
        fs::write(&path, format!("{header}\n")).unwrap();
        assert!(load_messages_from_session(&path).unwrap().is_empty());
        // A missing file evicts instead of serving stale rows.
        fs::remove_file(&path).unwrap();
        assert!(load_messages_from_session(&path).is_err());
        assert!(history_cache_get(&path).is_none());
    }

    #[test]
    fn history_cache_hit_repairs_dangling_tool_calls_idempotently() {
        use crate::core::types::{FunctionCall, LlmToolCall};
        let path = unique_path("dex-history-cache-repair");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        // Assistant tool call whose result never landed (cancelled turn).
        let dangling = ChatMessage::assistant_calls(
            Some("calling".into()),
            vec![LlmToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        s.append_message(&dangling).unwrap();
        // Every load synthesizes the placeholder exactly once — the cached
        // hit must match a fresh parse, not accumulate duplicates.
        let first = load_messages_from_session(&path).unwrap();
        let second = load_messages_from_session(&path).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn history_cache_touch_evicts_on_foreign_append() {
        let path = unique_path("dex-history-foreign");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        s.append_message(&ChatMessage::user("one")).unwrap();
        assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
        // A second process appends a message row behind our back...
        let foreign = ChatMessage::user("foreign");
        let entry = SessionMessageEntry {
            entry_type: "message",
            id: "foreign",
            timestamp: "2020-01-01T00:00:01Z",
            message: &foreign,
        };
        let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        drop(f);
        // ...then our own turn marker touches the snapshot: the length delta
        // no longer matches, so it must evict rather than publish a vector
        // that silently omits the foreign row.
        s.turn_event("turn_start").unwrap();
        let loaded = load_messages_from_session(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|m| m.content_str().contains("foreign")));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rewrite_messages_preserves_session_state() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-rewrite-state".into(), None).unwrap();
        s.set_state("plan", "the plan").unwrap();
        s.set_state("changes", "[]").unwrap();
        let path = s.path().unwrap().to_path_buf();
        // The turn loop's `messages` carry the system prompt at index 0; the
        // rewrite skips it and re-emits the rest.
        let messages = vec![ChatMessage::system("sys"), ChatMessage::user("keep")];
        s.rewrite_messages(&messages).unwrap();
        let state = load_session_state(&path).unwrap();
        assert_eq!(state.get("plan").map(String::as_str), Some("the plan"));
        assert_eq!(state.get("changes").map(String::as_str), Some("[]"));
        let after = load_messages_from_session(&path).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].content_str().contains("keep"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn events_page_limit_zero_serves_nothing() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-events-limit0".into(), None).unwrap();
        s.append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        assert!(Session::load_events(&path, 0, 0).unwrap().is_empty());
        // A nonzero page still serves.
        assert_eq!(Session::load_events(&path, 0, 1).unwrap().len(), 1);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[test]
    fn events_poll_skips_unchanged_journal_and_seeks_checkpoints() {
        let path = unique_path("dex-events-cache");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        for seq in 0..4u64 {
            s.append_event(
                seq,
                &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
            )
            .unwrap();
        }
        // Miss parses; the idle re-poll serves nothing without file IO.
        // Cursor is the next seq to serve (inclusive): `since=0` serves seq 0.
        assert_eq!(Session::load_events(&path, 0, usize::MAX).unwrap().len(), 4);
        assert!(Session::load_events(&path, 4, usize::MAX)
            .unwrap()
            .is_empty());
        // A behind cursor re-scans and still gets every row exactly once.
        let behind = Session::load_events(&path, 1, usize::MAX).unwrap();
        assert_eq!(
            behind.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Appends extend the cursor: only the tail is served.
        s.append_event(4, r#"{"type":"assistant_text","data":"4"}"#)
            .unwrap();
        let tail = Session::load_events(&path, 4, usize::MAX).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].0, 4);
        assert_eq!(Session::max_event_seq(&path), Some(4));
        // Grow past one checkpoint and seek to it: same tail, no full scan
        // by construction (checkpoints anchor the resume offset).
        for seq in 5..1600u64 {
            s.append_event(
                seq,
                &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
            )
            .unwrap();
        }
        {
            let cache = events_cache().lock().expect("events cache lock");
            assert!(!cache
                .get(&path.with_extension("events.jsonl"))
                .unwrap()
                .checkpoints
                .is_empty());
        }
        let tail = Session::load_events(&path, 1591, usize::MAX).unwrap();
        assert_eq!(tail.len(), 9);
        assert_eq!(tail[0].0, 1591);
        assert_eq!(Session::max_event_seq(&path), Some(1599));
        // Out-of-band truncation falls back to a full scan, never garbage:
        // the 64-byte stump holds no complete row.
        let events_path = path.with_extension("events.jsonl");
        let stump = fs::read(&events_path).unwrap()[..64].to_vec();
        fs::write(&events_path, stump).unwrap();
        assert!(Session::load_events(&path, 0, usize::MAX)
            .unwrap()
            .is_empty());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(events_path);
    }

    #[test]
    fn events_journal_pages_with_exactly_once_delivery() {
        // §1: page-limited serving splits the journal across pages; chaining
        // pages by last served seq delivers every row exactly once, and the
        // drained tail re-arms the one-stat idle fast path.
        let path = unique_path("dex-events-pages");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        for seq in 0..7u64 {
            s.append_event(
                seq,
                &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
            )
            .unwrap();
        }
        // Cursor semantics are inclusive (`seq >= since`): `since` is the
        // next seq to serve, so pages chain with `last + 1` (the daemon's
        // `next_seq`). Every row lands exactly once, including seq 0.
        let mut since = 0u64;
        let mut got = Vec::new();
        for _ in 0..10 {
            let page = Session::load_events(&path, since, 3).unwrap();
            if page.is_empty() {
                break;
            }
            since = page.last().unwrap().0 + 1;
            got.extend(page.into_iter().map(|(seq, _)| seq));
        }
        assert_eq!(got, vec![0, 1, 2, 3, 4, 5, 6]);
        // Drained: the idle re-poll serves nothing (fast path, no file open).
        assert!(Session::load_events(&path, since, 3).unwrap().is_empty());
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[test]
    fn events_opener_skips_line_count_scan() {
        let path = unique_path("dex-events-opener");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        fs::write(&path, format!("{header}\n")).unwrap();
        let mut s = Session::from_path(&path).unwrap();
        s.append_message(&ChatMessage::user("history")).unwrap();
        // Header-only open: no scan, counter unused — the handle only ever
        // appends events.
        let mut journal = Session::from_path_for_events(&path).unwrap();
        journal
            .append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
            .unwrap();
        // The row landed (max sees seq 0); replay serves it to a `since=0`
        // poll under the inclusive cursor semantics.
        assert_eq!(Session::max_event_seq(&path), Some(0));
        journal
            .append_event(1, r#"{"type":"assistant_text","data":"b"}"#)
            .unwrap();
        let rows = Session::load_events(&path, 0, usize::MAX).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 0);
        assert_eq!(rows[1].0, 1);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[test]
    fn scan_summary_matches_full_load() {
        let path = unique_path("dex-scan-summary");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let user = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"hi"}"#;
        let system = r#"{"type":"message","id":"2","timestamp":"2020-01-01T00:00:00Z","role":"system","content":"note"}"#;
        let bad = r#"{"type":"message","id":"bad","role":"mystery"}"#;
        let start = r#"{"type":"turn_start","id":"3","timestamp":"2020-01-01T00:00:00Z"}"#;
        let done = r#"{"type":"turn_complete","id":"4","timestamp":"2020-01-01T00:00:00Z"}"#;
        let state = r#"{"type":"session_state","id":"5","timestamp":"2020-01-01T00:00:00Z","key":"k","value":"v"}"#;
        // Non-compact spacing exercises the parse-and-classify fallback.
        let spaced = r#"{ "type" : "message" , "id" : "6" , "role" : "user" , "content" : "sp" }"#;
        fs::write(
            &path,
            format!("{header}\n{user}\n{system}\n{bad}\n{start}\n{done}\n{state}\n{spaced}\n"),
        )
        .unwrap();
        let loaded = load_messages_from_session(&path).unwrap();
        let (count, turn) = Session::scan_summary(&path).unwrap();
        assert_eq!(count, loaded.len());
        assert_eq!(count, 2);
        assert_eq!(turn, "complete");
        // `clear` folds the count like the loader folds the vec.
        let clear = r#"{"type":"clear","id":"7","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{header}\n{user}\n{clear}\n{spaced}\n")).unwrap();
        let (count, _) = Session::scan_summary(&path).unwrap();
        assert_eq!(count, load_messages_from_session(&path).unwrap().len());
        assert_eq!(count, 1);
        // Missing file is an error (callers map to 0/unknown).
        let _ = fs::remove_file(&path);
        assert!(Session::scan_summary(&path).is_err());
    }

    #[test]
    fn last_turn_state_tracks_terminal_entries() {
        let path = unique_path("dex-turn-state");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let start = r#"{"type":"turn_start","id":"1","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n", header, start)).unwrap();
        assert_eq!(Session::last_turn_state(&path), "interrupted");
        // Append a terminal entry and it flips.
        let done = r#"{"type":"turn_complete","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
        fs::write(&path, format!("{}\n{}\n{}\n", header, start, done)).unwrap();
        assert_eq!(Session::last_turn_state(&path), "complete");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn find_by_id_filename_resolves_exact_and_prefix() {
        // Sessions live under XDG_DATA_HOME: redirect + serialize against
        // tests doing the same. Two sessions prove the lookup discriminates
        // by filename instead of returning the first header parsed.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        let dir = std::env::temp_dir().join(format!("dex-find-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_DATA_HOME", &dir);
        let a = Session::new("/tmp/dex-find-a".into(), None).unwrap();
        let b = Session::new("/tmp/dex-find-b".into(), None).unwrap();
        let (ida, pa) = (a.id().to_string(), a.path().unwrap().to_path_buf());
        let (idb, pb) = (b.id().to_string(), b.path().unwrap().to_path_buf());
        assert_ne!(ida, idb);
        assert_eq!(Session::find_by_id_filename(&ida), Some(pa.clone()));
        assert_eq!(Session::find_by_id_filename(&idb), Some(pb.clone()));
        // Near-full prefix: the 8-char workspace slug collides across
        // sessions, so abbreviate inside the random suffix instead.
        assert_eq!(
            Session::find_by_id_filename(&ida[..ida.len() - 1]),
            Some(pa.clone())
        );
        assert_eq!(Session::find_by_id_filename("no-such-session"), None);
        drop(a);
        drop(b);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_messages_and_plan_matches_separate_loads() {
        let path = unique_path("dex-messages-plan");
        let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
        let user = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"hi"}"#;
        let want = crate::core::types::Plan {
            goal: Some("g".into()),
            steps: vec![("s".into(), false)],
            constraints: Vec::new(),
            acceptance: Vec::new(),
        };
        let state = format!(
            r#"{{"type":"session_state","id":"2","timestamp":"2020-01-01T00:00:00Z","key":"plan","value":{}}}"#,
            serde_json::to_string(&want.to_json()).unwrap()
        );
        fs::write(&path, format!("{header}\n{user}\n{state}\n")).unwrap();
        let (messages, plan) = load_messages_and_plan(&path).unwrap();
        // Same messages as the standalone loader, same plan as the
        // standalone second pass — from one scan.
        assert_eq!(
            serde_json::to_string(&messages).unwrap(),
            serde_json::to_string(&load_messages_from_session(&path).unwrap()).unwrap()
        );
        assert_eq!(plan, load_plan(&path));
        assert_eq!(plan, want);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn change_ledger_records_then_undo_restores() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-undo-test".into(), None).unwrap();
        let work = s.path().unwrap().parent().unwrap().join("work.txt");
        fs::write(&work, b"before\n").unwrap();
        let h_before = crate::tools::hash_file(&work.display().to_string());
        fs::write(&work, b"after\n").unwrap();
        let h_after = crate::tools::hash_file(&work.display().to_string());
        record_change(
            &mut s,
            make_change_record(
                "write",
                &work.display().to_string(),
                Some("before\n"),
                Some("after\n"),
                &h_before,
                &h_after,
            ),
        )
        .unwrap();
        // File is currently "after" — matches after_hash, so undo applies.
        let changes = load_changes(s.path().unwrap());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].after_hash, h_after);
        let msg = undo_last_change(&mut s).unwrap();
        assert!(msg.contains("undid write"));
        assert_eq!(fs::read_to_string(&work).unwrap(), "before\n");
        assert!(load_changes(s.path().unwrap()).is_empty());
        let _ = fs::remove_file(&work);
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn undo_refuses_when_file_moved_on() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-undo-concurrent".into(), None).unwrap();
        let work = s.path().unwrap().parent().unwrap().join("c.txt");
        fs::write(&work, b"v1\n").unwrap();
        let h = crate::tools::hash_file(&work.display().to_string());
        record_change(
            &mut s,
            make_change_record(
                "write",
                &work.display().to_string(),
                Some("v1\n"),
                Some("v2\n"),
                &h,
                &h,
            ),
        )
        .unwrap();
        // Rewrite the file afterwards but keep the same hash (hash is of
        // content; simulate a concurrent edit changing it):
        // A concurrent edit changes the content -> new hash -> refuse.
        fs::write(&work, b"vX\n").unwrap();
        let err = undo_last_change(&mut s).unwrap_err();
        assert!(err.to_string().contains("refusing to undo"));
        let _ = fs::remove_file(&work);
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn effect_journal_records_intent_and_outcome() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-effect-test".into(), None).unwrap();
        s.effect_start("call-1", "edit", "abc").unwrap();
        s.turn_event("turn_start").unwrap();
        s.effect_result("call-1", true).unwrap();
        s.turn_event("turn_complete").unwrap();
        let text = fs::read_to_string(s.path().unwrap()).unwrap();
        assert!(text.contains("effect_start"));
        assert!(text.contains("call-1"));
        assert!(text.contains("effect_result"));
        assert!(text.contains("\"ok\":true"));
        assert!(Session::last_turn_state(s.path().unwrap()) == "complete");
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    /// The routed tier rides the `turn_start` marker (`None` serializes to
    /// nothing, so unrouted journals stay byte-identical), and recovery
    /// still keys on the marker type alone.
    #[test]
    fn turn_start_carries_routing_tier() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = Session::new("/tmp/dex-tier-test".into(), None).unwrap();
        s.turn_event_with_tier("turn_start", Some("powerful"))
            .unwrap();
        s.turn_event("turn_complete").unwrap();
        let text = fs::read_to_string(s.path().unwrap()).unwrap();
        assert!(text.contains(r#""type":"turn_start""#), "{text}");
        assert!(text.contains(r#""tier":"powerful""#), "{text}");
        assert!(Session::last_turn_state(s.path().unwrap()) == "complete");
        if let Some(p) = s.path() {
            let _ = fs::remove_file(p);
        }
    }

    #[test]
    fn record_skills_writes_session_state_entry() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let skills = vec![crate::core::types::Skill {
            name: "demo".into(),
            description: "does demo things".into(),
            path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
        }];
        let mut s = Session::new("/tmp/dex-skills-test".into(), None).unwrap();
        s.record_skills(&skills);
        let path = s.path().unwrap().to_path_buf();
        let state = load_session_state(&path).unwrap();
        let recorded: serde_json::Value =
            serde_json::from_str(state.get("skills").unwrap()).unwrap();
        assert_eq!(recorded[0]["name"], "demo");
        assert_eq!(recorded[0]["description"], "does demo things");
        assert_eq!(recorded[0]["path"], "/tmp/demo/SKILL.md");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn default_session_name_is_workspace_plus_k8s_suffix() {
        let name = Session::default_session_name("/home/user/dex");
        let (base, suffix) = name.rsplit_once('-').unwrap();
        assert_eq!(base, "dex");
        assert_eq!(suffix.len(), 7);
        assert!(suffix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        // Sanitizes: punctuation/folds collapse to one `-`, lowercased.
        assert!(Session::default_session_name("/home/user/My Project!").starts_with("my-project-"));
        // Degenerate cwds fall back to `session`.
        assert!(Session::default_session_name("/").starts_with("session-"));
        assert!(Session::default_session_name("").starts_with("session-"));
        // Unique per call.
        assert_ne!(
            Session::default_session_name("/home/user/dex"),
            Session::default_session_name("/home/user/dex")
        );
    }

    #[test]
    fn new_session_defaults_name_but_keeps_explicit() {
        // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let s = Session::new("/tmp/dex-name-default".into(), None).unwrap();
        let name = s.name().unwrap().to_string();
        assert!(name.starts_with("dex-name-default-"), "got: {name}");
        assert_eq!(name.rsplit_once('-').unwrap().1.len(), 7);
        // The generated name is persisted in the on-disk header.
        let path = s.path().unwrap().to_path_buf();
        let raw = std::fs::read_to_string(&path).unwrap();
        let header: SessionHeader = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(header.name(), Some(name.as_str()));
        // The id is `<workspace>-<16 chars>`: self-describing so the resume
        // hint and daemon logs say which workspace *name* a session belongs to.
        let id = s.id().to_string();
        let (id_slug, id_suffix) = id.rsplit_once('-').unwrap();
        assert_eq!(id_slug, "dex-name-default", "got: {id}");
        assert_eq!(id_suffix.len(), 16, "got: {id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "got: {id}"
        );
        // The same slug rules drive the name's workspace half, so id and name
        // agree on the workspace even though the suffixes are drawn separately.
        assert_eq!(id_slug, name.rsplit_once('-').unwrap().0);
        assert!(Session::new_id("/").starts_with("session-"));
        assert!(Session::new_id("").starts_with("session-"));
        assert_eq!(path.file_stem().and_then(|s| s.to_str()), Some(id.as_str()));
        let _ = std::fs::remove_file(&path);

        let s = Session::new("/tmp/dex-name-explicit".into(), Some("mine".into())).unwrap();
        assert_eq!(s.name(), Some("mine"));
        let path = s.path().unwrap().to_path_buf();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn session_ids_are_unique_within_a_workspace() {
        // The id is both the JSONL filename and the daemon registry key, and
        // the old epoch prefix is gone: distinctness rests entirely on the 16
        // random chars, so two sessions in one workspace must never collide.
        let ids: std::collections::HashSet<String> = (0..256)
            .map(|_| Session::new_id("/tmp/dex-unique-workspace"))
            .collect();
        assert_eq!(ids.len(), 256);
        assert_ne!(
            Session::new_id("/tmp/dex-unique-workspace"),
            Session::new_id("/tmp/dex-unique-workspace")
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn session_async_readers_match_sync() {
        // TDD Phase 6: async file streams / spawn_blocking, same wire format, same undo ledger.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-sess-async-{}", std::process::id()));
        let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut s = Session::new("/tmp/async-cwd".into(), None).unwrap();
        let msg = ChatMessage::user("hello async");
        s.append_message(&msg).unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        let sync_msgs = load_messages_from_session(&path).unwrap();
        let async_msgs = load_messages_from_session_async(path.clone())
            .await
            .unwrap();
        assert_eq!(sync_msgs.len(), async_msgs.len());
        assert_eq!(sync_msgs[0].content, async_msgs[0].content);
        // list_all_async matches list_all (JoinSet, join, sort)
        let sync_list = Session::list_all().unwrap();
        let async_list = Session::list_all_async().await.unwrap();
        assert_eq!(sync_list.len(), async_list.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dangling_tool_call_is_synthesized_on_load() {
        // A crash between the assistant call and its result must not wedge
        // the next resume: providers reject an unanswered tool_call batch.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        let dir = std::env::temp_dir().join(format!("dex-sess-repair-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut s = Session::new("/tmp/dex-repair-cwd".into(), None).unwrap();
        s.append_message(&ChatMessage::user("goal")).unwrap();
        s.append_message(&ChatMessage::assistant_calls(
            None,
            vec![crate::core::types::LlmToolCall {
                id: "call-1".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "edit".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }],
        ))
        .unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        let messages = load_messages_from_session(&path).unwrap();
        assert_eq!(
            messages.len(),
            3,
            "assistant call must get a synthesized result"
        );
        let last = messages.last().unwrap();
        assert_eq!(last.role, crate::core::types::Role::Tool);
        assert_eq!(last.tool_call_id.as_deref(), Some("call-1"));
        assert!(
            last.content
                .as_deref()
                .unwrap()
                .contains("tool result missing"),
            "synthesized result must say what happened: {:?}",
            last.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_tool_batch_loads_without_synthesis() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        let dir = std::env::temp_dir().join(format!("dex-sess-clean-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut s = Session::new("/tmp/dex-clean-cwd".into(), None).unwrap();
        s.append_message(&ChatMessage::assistant_calls(
            None,
            vec![crate::core::types::LlmToolCall {
                id: "call-1".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        ))
        .unwrap();
        s.append_message(&ChatMessage::tool_result("call-1", "contents"))
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        let messages = load_messages_from_session(&path).unwrap();
        assert_eq!(messages.len(), 2, "no placeholder for a complete batch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dangling_middle_batch_repairs_in_place_not_at_end() {
        // A historic middle-batch gap gets its placeholder right after its
        // assistant message, preserving causality for the model.
        let mut messages = vec![
            ChatMessage::user("goal"),
            ChatMessage::assistant_calls(
                None,
                vec![crate::core::types::LlmToolCall {
                    id: "mid-1".into(),
                    call_type: "function".into(),
                    function: crate::core::types::FunctionCall {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
            ChatMessage::user("follow-up"),
        ];
        super::repair_dangling_tool_calls(&mut messages);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2].role, crate::core::types::Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("mid-1"));
        assert_eq!(messages[3].content.as_deref(), Some("follow-up"));
    }

    #[test]
    fn child_session_writes_its_own_file_with_markers() {
        // §16: the child's JSONL lands in `agents/` beside the parent file
        // with the same turn-marker discipline.
        let parent = Session::new("/tmp/dex-child-parent".into(), None).unwrap();
        let parent_path = parent.path().unwrap().to_path_buf();
        let mut child =
            Session::child(&parent_path, "/tmp/dex-child-parent", "p-0", "explorer", 0).unwrap();
        let child_path = child.path().unwrap().to_path_buf();
        assert_eq!(
            child_path.parent().unwrap(),
            parent_path.parent().unwrap().join("agents")
        );
        assert_eq!(
            child_path.file_name().and_then(|s| s.to_str()),
            Some("p-0-explorer.jsonl")
        );
        child.turn_event("turn_start").unwrap();
        child
            .append_message(&ChatMessage::user("child task"))
            .unwrap();
        child.turn_event("turn_complete").unwrap();
        // Marker discipline holds: a completed turn is not "interrupted".
        assert_eq!(Session::last_turn_state(&child_path), "complete");
        let messages = load_messages_from_session(&child_path).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content.as_deref(), Some("child task"));
        let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
    }

    #[test]
    fn list_children_reports_runs_and_interrupted_state() {
        // Phase 8 exit: resume shows children and interrupted runs — every
        // run under `agents/` with its last turn state, `"interrupted"`
        // being a `turn_start` with no terminal marker (crashed or
        // daemon-restart-killed child). Loaders still ignore the directory.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parent = Session::new("/tmp/dex-children-list".into(), None).unwrap();
        let parent_path = parent.path().unwrap().to_path_buf();
        drop(parent);
        let mut done =
            Session::child(&parent_path, "/tmp/dex-children-list", "c-0", "explorer", 0).unwrap();
        done.turn_event("turn_start").unwrap();
        done.turn_event("turn_complete").unwrap();
        let mut hung =
            Session::child(&parent_path, "/tmp/dex-children-list", "c-1", "tester", 0).unwrap();
        hung.turn_event("turn_start").unwrap();
        // A crash before a terminal marker leaves the run interrupted.
        drop(done);
        drop(hung);
        let runs = Session::list_children(&parent_path).unwrap();
        assert_eq!(runs.len(), 2);
        let state_of = |prefix: &str| {
            runs.iter()
                .find(|(_, header, _)| header.id().starts_with(prefix))
                .map(|(.., state)| *state)
        };
        assert_eq!(state_of("c-0"), Some("complete"));
        assert_eq!(state_of("c-1"), Some("interrupted"));
        // Headers keep the parent linkage (§22-N: parent/child recorded).
        assert!(runs
            .iter()
            .all(|(_, header, _)| header.name().is_some_and(|n| n.contains("(child of "))));
        let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
    }

    #[test]
    fn child_generations_never_collide() {
        // §24.3: generation 0 keeps the V1 filename; resume generations
        // append `.g<N>` so a resume never clobbers its parent.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parent = Session::new("/tmp/dex-supervision-gen".into(), None).unwrap();
        let parent_path = parent.path().unwrap().to_path_buf();
        drop(parent);
        let first = Session::child(
            &parent_path,
            "/tmp/dex-supervision-gen",
            "s-0",
            "explorer",
            0,
        )
        .unwrap();
        let second = Session::child(
            &parent_path,
            "/tmp/dex-supervision-gen",
            "s-0",
            "explorer",
            1,
        )
        .unwrap();
        let first_path = first.path().unwrap().to_path_buf();
        let second_path = second.path().unwrap().to_path_buf();
        assert_ne!(first_path, second_path);
        assert!(first_path.ends_with("agents/s-0-explorer.jsonl"));
        assert!(second_path.ends_with("agents/s-0-explorer.g1.jsonl"));
        assert_eq!(first.id(), "s-0-explorer");
        assert_eq!(second.id(), "s-0-explorer.g1");
        drop(first);
        drop(second);
        assert_eq!(
            Session::child_path(&parent_path, "s-0", "explorer", 0),
            first_path
        );
        assert_eq!(
            Session::child_path(&parent_path, "s-0", "explorer", 1),
            second_path
        );
        let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
    }

    #[test]
    fn parent_listing_and_loader_ignore_child_sessions() {
        // §16 hard rule: `agents/*` never enters the parent transcript or
        // the session registry. Serialization: Session::new writes into the
        // shared sessions dir.
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let parent = Session::new("/tmp/dex-ignore-child".into(), None).unwrap();
        let parent_path = parent.path().unwrap().to_path_buf();
        let mut child =
            Session::child(&parent_path, "/tmp/dex-ignore-child", "p-1", "tester", 0).unwrap();
        let child_path = child.path().unwrap().to_path_buf();
        child
            .append_message(&ChatMessage::user("child-only content"))
            .unwrap();
        // Listing reads only direct .jsonl files in each session directory.
        let listed = Session::list_all().unwrap();
        assert!(
            listed.iter().all(|(path, _)| path != &child_path),
            "child session must not be listed as a session"
        );
        assert!(
            listed.iter().any(|(path, _)| path == &parent_path),
            "the parent session itself stays listed"
        );
        // And the loaders take explicit paths: the parent's history has no
        // child content.
        let parent_messages = load_llm_messages_from_session(&parent_path).unwrap();
        assert!(parent_messages
            .iter()
            .all(|m| m.content.as_deref() != Some("child-only content")));
        let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
    }
}
