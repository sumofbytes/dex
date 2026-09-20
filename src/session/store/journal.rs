//! Session event journal: the append-only `events.jsonl` sidecar that
//! records `turn_start`/`turn_complete`/`turn_failed` markers and streamed
//! daemon events, plus the cheap single-pass summary scan used by listing.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use serde_json::Value;

use crate::protocol::{ChatMessage, Role};

use super::Session;

impl Session {
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
        let durable = super::durable_journal()
            || payload.contains("\"type\":\"turn_complete\"")
            || payload.contains("\"type\":\"turn_failed\"");
        if durable {
            file.sync_data()?;
        }
        if let Some(events_path) = self.events_path() {
            super::super::events::events_cache_touched(&events_path, seq, line.len() as u64 + 1);
        }
        Ok(())
    }

    /// Replay stream events with `seq >= since` — see `events::load_events`.
    pub(crate) fn load_events(
        path: &Path,
        since: u64,
        limit: usize,
    ) -> io::Result<Vec<(u64, String)>> {
        super::super::events::load_events(path, since, limit)
    }

    /// Highest event seq recorded for a session — see `events::max_event_seq`.
    pub(crate) fn max_event_seq(path: &Path) -> Option<u64> {
        super::super::events::max_event_seq(path)
    }

    /// Terminal state of the most recent turn: "complete", "failed", or
    /// "interrupted" when a `turn_start` has no terminal entry after it.
    pub(crate) fn last_turn_state(path: &Path) -> &'static str {
        // Streamed via `for_each_line`; sessions hold thousands of
        // non-marker entries. Open failure still reads as "unknown"; the
        // helper stops mid-scan on read errors like EOF.
        let mut state = "none";
        let scan = super::for_each_line(path, |line| {
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

    /// Agent mode recorded on the most recent `turn_start` (see
    /// `turn_event_with_mode`): the client's last selector, restored on
    /// reattach. `None` for legacy journals and subagent turns.
    pub(crate) fn last_turn_mode(path: &Path) -> Option<String> {
        let mut mode: Option<String> = None;
        let scan = super::for_each_line(path, |line| {
            if !line.contains("\"type\":\"turn_start\"") {
                return;
            }
            let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
                return;
            };
            if let Some(m) = value.get("mode").and_then(Value::as_str) {
                mode = Some(m.to_string());
            }
        });
        if scan.is_err() {
            return None;
        }
        mode
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
