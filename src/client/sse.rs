//! Daemon↔TUI SSE transport: byte framing (`SseFramer`) + the in-flight
//! response wrapper (`ChatStream`). Pure buffer logic plus one
//! `response.chunk().await` read loop — no runtime, no endpoints, no display.

use crate::protocol::{StreamEnvelope, StreamEvent};

/// Byte framing for SSE `data:` lines. Pure buffer logic (no I/O, no runtime)
/// shared by `ChatStream`; split lines across TCP chunks are reassembled,
/// keep-alives and junk skipped, trailing partial line held for `finish()`.
/// Tracks the next journal `seq` to serve (inclusive cursor): 0 until the
/// first envelope, then `max(seq) + 1` — so "nothing delivered yet" (resume
/// from 0, serving seq 0) is distinct from "delivered seq 0" (resume from 1).
#[derive(Default)]
pub(crate) struct SseFramer {
    buf: Vec<u8>,
    pub(crate) pending: std::collections::VecDeque<StreamEvent>,
    pub(crate) next_seq: u64,
}

impl SseFramer {
    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.ingest(&String::from_utf8_lossy(&line));
        }
    }

    pub(crate) fn finish(&mut self) {
        // Trailing buffered line without newline (terminal envelope).
        if !self.buf.is_empty() {
            let tail = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            self.ingest(&tail);
        }
    }

    /// Wire-tolerant ingest (P10 + V1b fallback): a `data:` payload that
    /// parses as an envelope is queued; one carrying a `seq` this client
    /// does not have a variant for (older daemon, or a newer daemon's event
    /// type) is skipped — but its `seq` still advances the cursor, so a
    /// reconnect's replay never re-fetches the same range forever.
    fn ingest(&mut self, line: &str) {
        let Some(data) = Self::data_payload(line) else {
            return;
        };
        match serde_json::from_str::<StreamEnvelope>(data) {
            Ok(env) => {
                self.next_seq = self.next_seq.max(env.seq.saturating_add(1));
                self.pending.push_back(env.event);
            }
            Err(_) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(seq) = value.get("seq").and_then(|v| v.as_u64()) {
                        self.next_seq = self.next_seq.max(seq.saturating_add(1));
                    }
                }
            }
        }
    }

    /// Parse one raw SSE line into a full envelope (event + journal seq).
    /// Pure (no I/O, no runtime) so it is safe from any context and trivial
    /// to unit-test. Returns `None` for keep-alives (`ping`), blanks,
    /// non-`data:` lines, and unparsable payloads. Lenient consumers should
    /// prefer [`SseFramer::ingest`], which keeps the cursor moving past
    /// unknown event types.
    #[cfg(test)]
    pub(crate) fn parse_envelope(line: &str) -> Option<StreamEnvelope> {
        let data = Self::data_payload(line)?;
        serde_json::from_str::<StreamEnvelope>(data).ok()
    }

    /// Strip the SSE framing: `None` for blanks, non-`data:` lines, and
    /// keep-alives.
    fn data_payload(line: &str) -> Option<&str> {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        let data = trimmed.strip_prefix("data:")?.trim();
        if data.is_empty() || data == "ping" {
            return None;
        }
        Some(data)
    }
}

/// One in-flight SSE response with its framing buffer. `next_event()` returns
/// `None` on clean EOF and `Some(Err)` on transport failure, so callers can
/// distinguish "turn completed" from "connection died".
pub struct ChatStream {
    response: reqwest::Response,
    framer: SseFramer,
    eof: bool,
}

impl ChatStream {
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            framer: SseFramer::default(),
            eof: false,
        }
    }

    /// Next journal seq to serve (inclusive resume cursor) — 0 until the
    /// first envelope, then highest delivered + 1.
    pub fn last_seq(&self) -> u64 {
        self.framer.next_seq
    }

    pub async fn next_event(&mut self) -> Option<Result<StreamEvent, String>> {
        loop {
            if let Some(event) = self.framer.pending.pop_front() {
                return Some(Ok(event));
            }
            if self.eof {
                return None;
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self.framer.push_bytes(&bytes),
                Ok(None) => {
                    self.eof = true;
                    self.framer.finish();
                }
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }
}
