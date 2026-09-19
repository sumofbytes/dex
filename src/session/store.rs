use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::protocol::{ChatMessage, Role};

use super::discovery;
use super::header::{
    file_id, FileId, PathCache, SessionClearEntry, SessionEventEntry, SessionHeader,
    SessionInfoEntry, SessionMessageEntry, SessionStateEntry, SESSION_VERSION,
};

#[cfg(test)]
use super::header::SessionEffectEntry;
#[cfg(test)]
use super::{undo_last_change, EnvGuard, TEST_SESSIONS_ENV_LOCK};

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

impl Session {
    pub(crate) fn session_dir() -> PathBuf {
        if let Some(dir) = env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("dex/sessions");
        }
        env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/dex/sessions"))
            .unwrap_or_else(|| PathBuf::from(".dex/sessions"))
    }

    pub(crate) fn cwd_slug(cwd: &str) -> String {
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
    fn record_skills(&mut self, skills: &[crate::protocol::Skill]) {
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
        discovery::list(cwd)
    }

    /// mtime of one workspace's sessions dir, for the slash-popup cache key
    /// (perf doc §29): one stat instead of a readdir + header parses per
    /// frame while `/resume ...` sits in the composer.
    pub(crate) fn list_dir_mtime(cwd: &str) -> Option<SystemTime> {
        discovery::list_dir_mtime(cwd)
    }

    /// List every persisted session across all workspaces (registry rebuild
    /// and disk-backed `GET /api/sessions`).
    pub(crate) fn list_all() -> io::Result<Vec<(PathBuf, SessionHeader)>> {
        discovery::list_all()
    }

    /// Fast id→path lookup (perf doc §1): the session id is the JSONL
    /// filename, so match by file name across workspace dirs without
    /// opening every file — `list_all` opens + header-parses each one just
    /// to locate a single session. Each filename hit is confirmed by one
    /// header-only `from_path` read; anything unconfirmed (renamed stems,
    /// legacy files) falls through to the `list_all` scan at the caller.
    pub(crate) fn find_by_id_filename(sid: &str) -> Option<PathBuf> {
        discovery::find_by_id_filename(sid)
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
        let mut children = discovery::scan_jsonl_dir(&dir);
        discovery::sort_newest_first(&mut children);
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
        let children = discovery::scan_jsonl_dir(dir);
        let interrupted = children
            .iter()
            .filter(|(path, _)| Self::last_turn_state(path) == "interrupted")
            .count();
        (children.len(), interrupted)
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
    #[cfg(test)]
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
    #[cfg(test)]
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
        // durability matters (turn boundaries) or when DEX_DURABLE=1 is
        // set for strict recovery testing.
        // `effect_start`/`effect_result` are the restart-recovery intent
        // records (written BEFORE the tool executes) — sync them so a crash
        // can't leave an executed tool unrecorded. `clear` is rare
        // (compaction rewrites atomically now; `/clear` is user intent) —
        // sync it so the fold point itself is durable.
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
}

/// Serialize discovered skills for the session-start `skills` state entry:
/// a JSON array of `{name, description, path}` objects.
fn skills_state_value(skills: &[crate::protocol::Skill]) -> String {
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
) -> io::Result<(Vec<ChatMessage>, crate::protocol::Plan)> {
    if let Some(mut messages) = history_cache_get(path) {
        repair_dangling_tool_calls(&mut messages);
        return Ok((messages, load_plan(path)));
    }
    let (mut messages, plan) = scan_history(path)?;
    repair_dangling_tool_calls(&mut messages);
    Ok((
        messages,
        plan.map(|s| crate::protocol::Plan::from_json(&s))
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
                crate::ui::format::model_tool_result(
                    "Error: tool result missing — the agent exited before it was recorded; the call may have executed. Verify the effect on disk before retrying.",
                ),
            ),
        );
    }
}

/// Async full-history load: streaming file, fast — short `spawn_blocking`.
/// No async production consumer yet (the TUI replays via SSE); test-only
/// until one lands.
#[cfg(test)]
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
        discovery::list_all_async().await
    }
}

pub(crate) fn load_plan(path: &Path) -> crate::protocol::Plan {
    load_session_state(path)
        .ok()
        .and_then(|m| m.get("plan").cloned())
        .map(|s| crate::protocol::Plan::from_json(&s))
        .unwrap_or_default()
}

#[allow(dead_code)]
pub(crate) fn save_plan(session: &mut Session, plan: &crate::protocol::Plan) -> io::Result<()> {
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

#[cfg(test)]
mod tests;

mod journal;
