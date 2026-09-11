#![allow(dead_code, unused_variables, unused_imports)]
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
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
    counter: u64,
    /// Reused append handle for the main JSONL, opened lazily on first write;
    /// one write syscall per line, same as the old open-append-write-close —
    /// the cache only removes the per-append open/close.
    journal: Option<File>,
    /// Same for the events journal: the hot path, one line per stream delta.
    events_journal: Option<File>,
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
        format!("{}-{}", slug, Self::random_suffix_7())
    }

    /// 7 random `[a-z0-9]` chars sourced from a v4 UUID (OS RNG, already a
    /// dependency): 36^7 combinations, no coordination needed.
    fn random_suffix_7() -> String {
        const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        bytes[..7]
            .iter()
            .map(|b| ALPHABET[usize::from(*b % 36)] as char)
            .collect()
    }

    pub(crate) fn new(cwd: String, name: Option<String>) -> io::Result<Self> {
        // Unnamed sessions default to `<workspace>-<7 chars>`; an explicit
        // `--name`/`/name` (or `Some` from the daemon request) always wins.
        let name = match name {
            Some(n) if !n.is_empty() => Some(n),
            _ => Some(Self::default_session_name(&cwd)),
        };
        let id = format!("{}_{}", Self::now_ms(), uuid4());
        let dir = Self::session_dir().join(Self::cwd_slug(&cwd));
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", id));
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: id.clone(),
            timestamp: Self::now_iso(),
            cwd,
            name,
        };
        let line = serde_json::to_string(&header).map_err(io::Error::other)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", line)?;
        let mut session = Self {
            header,
            path: Some(path),
            counter: 0,
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

    pub(crate) fn from_path(path: &Path) -> io::Result<Self> {
        // Stream instead of read_to_string: session files grow with the
        // message history and only the header plus a line count are needed.
        let mut reader = BufReader::new(File::open(path)?);
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
        // Count the lines after the header without materializing the file.
        let mut counter = 0u64;
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf)? > 0 {
            counter += 1;
            buf.clear();
        }
        Ok(Self {
            header,
            path: Some(path.to_path_buf()),
            counter,
            journal: None,
            events_journal: None,
        })
    }

    /// Child-agent JSONL (plan §16): `agents/<agent_id>-<name>.jsonl`
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
    ) -> io::Result<Self> {
        let dir = parent_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("agents");
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{agent_id}-{name}.jsonl"));
        let header = SessionHeader {
            entry_type: "session".to_string(),
            version: SESSION_VERSION,
            id: format!("{agent_id}-{name}"),
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
        // run's record readable, and continuing the line counter keeps
        // entry ids unique across the seam.
        let mut counter = 0u64;
        if path.exists() {
            let mut reader = BufReader::new(File::open(&path)?);
            let mut buf = Vec::new();
            while reader.read_until(b'\n', &mut buf)? > 0 {
                counter += 1;
                buf.clear();
            }
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", line)?;
        Ok(Self {
            header,
            path: Some(path),
            counter,
            journal: None,
            events_journal: None,
        })
    }

    pub(crate) fn in_memory(cwd: String) -> Self {
        Self {
            header: SessionHeader {
                entry_type: "session".to_string(),
                version: SESSION_VERSION,
                id: format!("{}_{}", Self::now_ms(), uuid4()),
                timestamp: Self::now_iso(),
                cwd,
                name: None,
            },
            path: None,
            counter: 0,
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
        let mut sessions = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    // Only the header is needed; session files grow large.
                    if let Some(first) = read_first_line(&path) {
                        if let Ok(header) = serde_json::from_str::<SessionHeader>(first.trim_end())
                        {
                            sessions.push((path, header));
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(sessions)
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
                if let Ok(files) = fs::read_dir(&dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            // Only the header is needed; session files grow large.
                            if let Some(first) = read_first_line(&path) {
                                if let Ok(header) =
                                    serde_json::from_str::<SessionHeader>(first.trim_end())
                                {
                                    sessions.push((path, header));
                                }
                            }
                        }
                    }
                }
            }
        }
        sessions.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(sessions)
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
        let dir = parent_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("agents");
        let mut children = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    if let Some(first) = read_first_line(&path) {
                        if let Ok(header) = serde_json::from_str::<SessionHeader>(first.trim_end())
                        {
                            let turn_state = Self::last_turn_state(&path);
                            children.push((path, header, turn_state));
                        }
                    }
                }
            }
        }
        children.sort_by(|a, b| b.1.timestamp.cmp(&a.1.timestamp));
        Ok(children)
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
        self.append_line(&SessionMessageEntry {
            entry_type: "message",
            id: &id,
            timestamp: &timestamp,
            message,
        })
    }

    pub(crate) fn clear_messages(&mut self) -> io::Result<()> {
        let entry = SessionClearEntry {
            entry_type: "clear".into(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
        };
        self.append_line(&entry)
    }

    pub(crate) fn turn_event(&mut self, event: &str) -> io::Result<()> {
        let entry = SessionEventEntry {
            entry_type: event.to_string(),
            id: self.next_id(),
            timestamp: Self::now_iso(),
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
        let durable = std::env::var("DEX_DURABLE").as_deref() == Ok("1")
            || line.contains("\"type\":\"turn_")
            || line.contains("\"type\":\"effect_");
        if durable {
            file.sync_data()?;
        }
        Ok(())
    }

    fn next_id(&mut self) -> String {
        self.counter += 1;
        format!("{:x}", self.counter)
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
    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    }
    pub(crate) fn id(&self) -> &str {
        &self.header.id
    }
    pub(crate) fn name(&self) -> Option<&str> {
        self.header.name.as_deref()
    }
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
    pub(crate) fn count(&self) -> u64 {
        self.counter
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
        writeln!(
            file,
            "{{\"seq\":{seq},\"ts\":\"{ts}\",\"payload\":{payload}}}"
        )?;
        // Event journal is replayable but not critical for crash recovery —
        // sync only for terminal events or when DEX_DURABLE=1. (The old sniff
        // grepped for the Rust variant names, which never appear in the
        // serialized `type` field, so it never fired without DEX_DURABLE.)
        let durable = std::env::var("DEX_DURABLE").as_deref() == Ok("1")
            || payload.contains("\"type\":\"turn_complete\"")
            || payload.contains("\"type\":\"turn_failed\"");
        if durable {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Replay stream events with `seq > since`, in order. `path` is the
    /// SESSION file; the journal lives at `<session>.events.jsonl`.
    pub(crate) fn load_events(path: &Path, since: u64) -> io::Result<Vec<(u64, String)>> {
        let events_path = path.with_extension("events.jsonl");
        // Stream line by line like `max_event_seq` — the journal is the hot
        // file (one line per stream delta) and replay drops everything with
        // `seq <= since`, so don't slurp it into memory first.
        let file = File::open(&events_path)?;
        let mut out = Vec::new();
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                // EINTR: retry, never treat as EOF — that would silently
                // truncate the replay mid-journal.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Torn/invalid line: skip it (matches `max_event_seq`).
                Err(_) => break,
                Ok(_) => {}
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
                continue;
            };
            if seq <= since {
                continue;
            }
            if let Some(payload) = value.get("payload") {
                out.push((seq, payload.to_string()));
            }
        }
        Ok(out)
    }

    /// Highest event seq recorded for a session (`None` when no seq is
    /// journaled yet — distinct from a journal holding exactly seq 0).
    pub(crate) fn max_event_seq(path: &Path) -> Option<u64> {
        let events_path = path.with_extension("events.jsonl");
        // Stream line by line; the journal is the hot file (one line per
        // stream delta) and replay only needs the max seq, not the text.
        let Ok(file) = File::open(&events_path) else {
            return None;
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut max: Option<u64> = None;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // append_event writes {"seq":N,...}; skip parsing other shapes.
            if !line.contains("\"seq\":") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(line.trim_end()) {
                if let Some(seq) = v.get("seq").and_then(Value::as_u64) {
                    max = Some(max.map_or(seq, |m| m.max(seq)));
                }
            }
        }
        max
    }

    /// Terminal state of the most recent turn: "complete", "failed", or
    /// "interrupted" when a `turn_start` has no terminal entry after it.
    pub(crate) fn last_turn_state(path: &Path) -> &'static str {
        // Stream line by line; sessions hold thousands of non-marker entries.
        let Ok(file) = File::open(path) else {
            return "unknown";
        };
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut state = "none";
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // Turn entries serialize as {"type":"turn_*",...}; skip parsing
            // everything else.
            if !line.contains("\"type\":\"turn_") {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
                continue;
            };
            match value.get("type").and_then(Value::as_str) {
                Some("turn_start") => state = "interrupted",
                Some("turn_complete") => state = "complete",
                Some("turn_failed") => state = "failed",
                _ => {}
            }
        }
        state
    }
}

fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
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
    // Stream line by line: session files grow with history and only the
    // post-`clear` tail is kept.
    let mut reader = BufReader::new(File::open(path)?);
    let mut messages = Vec::new();
    let mut line = String::new();
    let mut line_no = 1; // header; matches the old skip(1) numbering
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
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
    repair_dangling_tool_calls(&mut messages);
    Ok(messages)
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
            set.spawn(tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                if let Ok(files) = fs::read_dir(&dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                            if let Some(first) = read_first_line(&path) {
                                if let Ok(header) =
                                    serde_json::from_str::<SessionHeader>(first.trim_end())
                                {
                                    out.push((path, header));
                                }
                            }
                        }
                    }
                }
                out
            }));
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
    let mut reader = BufReader::new(File::open(path)?);
    let mut state = std::collections::HashMap::new();
    let mut line = String::new();
    reader.read_line(&mut line)?; // header
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        // set_state writes {"type":"session_state",...}; quotes inside state
        // values are JSON-escaped, so this substring can only be the marker.
        if !line.contains("\"type\":\"session_state\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue;
        };
        if let (Some(key), Some(val)) = (
            value.get("key").and_then(Value::as_str),
            value.get("value").and_then(Value::as_str),
        ) {
            state.insert(key.into(), val.into());
        }
    }
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
        let after = Session::load_events(&path, 1).unwrap();
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
        let _ = std::fs::remove_file(&path);

        let s = Session::new("/tmp/dex-name-explicit".into(), Some("mine".into())).unwrap();
        assert_eq!(s.name(), Some("mine"));
        let path = s.path().unwrap().to_path_buf();
        let _ = std::fs::remove_file(&path);
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
            Session::child(&parent_path, "/tmp/dex-child-parent", "p-0", "explorer").unwrap();
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
            Session::child(&parent_path, "/tmp/dex-children-list", "c-0", "explorer").unwrap();
        done.turn_event("turn_start").unwrap();
        done.turn_event("turn_complete").unwrap();
        let mut hung =
            Session::child(&parent_path, "/tmp/dex-children-list", "c-1", "tester").unwrap();
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
            Session::child(&parent_path, "/tmp/dex-ignore-child", "p-1", "tester").unwrap();
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
