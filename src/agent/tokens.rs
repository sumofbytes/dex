use crate::core::types::ChatMessage;

/// Token overhead per message (role, formatting, turn boundary).
pub(crate) const PER_MESSAGE_OVERHEAD: u64 = 12;
/// Rough cost of the tool definitions sent with every request.
const TOOL_SCHEMA_TOKENS: u64 = 3200;

pub(crate) fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    // Content + tool call payload + name/role + replayed reasoning
    // (reasoning_items blobs and reasoning_content are re-sent verbatim next
    // request, so they count toward the window).
    let chars: usize = messages.iter().map(message_char_len).sum();
    // ~4 chars per token plus per-message overhead.
    (chars as u64) / 4 + (messages.len() as u64 * PER_MESSAGE_OVERHEAD)
}

/// Estimate tokens for the ephemeral preamble that is injected at call-time
/// but not stored in `messages`.
pub(crate) fn estimate_ephemeral_tokens(parts: &[Option<String>]) -> u64 {
    let chars: usize = parts
        .iter()
        .filter_map(|p| p.as_ref())
        .map(|s| s.len())
        .sum();
    (chars as u64) / 4 + (parts.len() as u64 * PER_MESSAGE_OVERHEAD)
}

/// Byte-counting sink for `serde_json::to_writer`: measures the serialized
/// length without materializing the item text.
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Byte length of a message's replayed payload (content, tool calls,
/// reasoning, framing) — the shared body of the token estimator, used by
/// `estimate_tokens` and the cut-point walk.
pub(crate) fn message_char_len(message: &ChatMessage) -> usize {
    let mut len = message.content.as_deref().map_or(0, str::len)
        + message.tool_calls.as_ref().map_or(0, |calls| {
            calls
                .iter()
                .map(|call| {
                    call.function.arguments.len() + call.function.name.len() + call.id.len()
                })
                .sum()
        })
        + message.reasoning_content.as_deref().map_or(0, str::len)
        + message.reasoning_items.as_ref().map_or(0, |items| {
            items
                .iter()
                .map(|item| {
                    let mut counter = ByteCounter(0);
                    let _ = serde_json::to_writer(&mut counter, item);
                    counter.0
                })
                .sum()
        });
    // Role and name framing
    len += message.role.as_str().len();
    if let Some(name) = &message.name {
        len += name.len();
    }
    if let Some(tid) = &message.tool_call_id {
        len += tid.len();
    }
    len
}

/// Running token-accounting total over the turn's stored history. The turn
/// loop used to call `effective_tokens` (a full transcript walk that
/// re-serializes every `reasoning_items` blob) 3–4× per model call. The
/// ledger measures each message once — on push, or on rebuild after
/// compaction rewrites history — and serves O(1) totals in between.
/// The ledger must mirror `messages` exactly: push every appended message,
/// rebuild after any rewrite (`compact_history`).
#[derive(Default)]
pub(crate) struct TokenLedger {
    /// Per-message char lengths, parallel to `messages`.
    lens: Vec<usize>,
    /// Sum of `lens`.
    chars: u64,
}

impl TokenLedger {
    pub(crate) fn rebuild(messages: &[ChatMessage]) -> Self {
        let lens: Vec<usize> = messages.iter().map(message_char_len).collect();
        #[allow(clippy::cast_possible_truncation)]
        let chars = lens.iter().sum::<usize>() as u64;
        Self { lens, chars }
    }

    pub(crate) fn push(&mut self, message: &ChatMessage) {
        let len = message_char_len(message);
        self.lens.push(len);
        self.chars += len as u64;
    }

    /// Token estimate over the stored history: the same formula as
    /// `estimate_tokens` over the same messages, read off cached lengths.
    pub(crate) fn stored_tokens(&self) -> u64 {
        self.chars / 4 + (self.lens.len() as u64 * PER_MESSAGE_OVERHEAD)
    }

    /// Tokens a compaction can actually archive: the transcript minus the
    /// system message (never summarized) and the larger of the keep-recent
    /// token window and the `keep_recent_messages` message floor. Same
    /// formula as the old `archivable_tokens` walk, read off the cached
    /// lengths — O(`keep_recent_messages`) instead of O(history).
    pub(crate) fn archivable_tokens(
        &self,
        keep_recent_messages: usize,
        keep_recent_tokens: u64,
    ) -> u64 {
        let n = self.lens.len();
        if n <= 1 {
            return 0;
        }
        let transcript_chars = self.chars.saturating_sub(self.lens[0] as u64);
        let transcript = transcript_chars / 4 + ((n - 1) as u64 * PER_MESSAGE_OVERHEAD);
        let from = n.saturating_sub(keep_recent_messages);
        #[allow(clippy::cast_possible_truncation)]
        let recent_chars = self.lens[from..].iter().sum::<usize>() as u64;
        let recent = recent_chars / 4 + ((n - from) as u64 * PER_MESSAGE_OVERHEAD);
        transcript.saturating_sub(recent.max(keep_recent_tokens))
    }
}

/// Per-request tool-schema budget for the compaction threshold: the native
/// flat estimate plus the live MCP + extension slices. The live slices are
/// precomputed at refresh time (never re-serialized here), so this is two
/// cached loads — safe to call per model request.
pub(crate) fn schema_budget_tokens() -> u64 {
    // Without the live slices the compaction threshold ignores the
    // per-request schema cost that actually fills the window.
    TOOL_SCHEMA_TOKENS
        + crate::mcp::cached_schema_tokens()
        + crate::extensions::cached_schema_tokens()
}

/// Token cost of the live MCP tool-schema slice (`mcp.rs:cached_tools`).
/// Same ~4-chars-per-token heuristic as [`estimate_tokens`]: namespaced
/// name + description + serialized parameters per definition.
pub(crate) fn schema_token_estimate(defs: &[crate::core::types::ToolDefinition]) -> u64 {
    let chars: usize = defs
        .iter()
        .map(|d| {
            d.function.name.len()
                + d.function.description.len()
                + d.function.parameters.to_string().len()
        })
        .sum();
    (chars as u64) / 4 + (defs.len() as u64 * PER_MESSAGE_OVERHEAD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::ChatMessage;

    fn sample_history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("you are dex"),
            ChatMessage::user("hello"),
            ChatMessage::assistant("hi there"),
            ChatMessage::user("do the thing"),
        ]
    }

    #[test]
    fn ledger_total_matches_walk_estimate() {
        let messages = sample_history();
        let ledger = TokenLedger::rebuild(&messages);
        assert_eq!(ledger.stored_tokens(), estimate_tokens(&messages));
        // Incremental pushes agree with a fresh rebuild.
        let mut incremental = TokenLedger::default();
        for message in &messages {
            incremental.push(message);
        }
        assert_eq!(incremental.stored_tokens(), ledger.stored_tokens());
        assert_eq!(incremental.stored_tokens(), estimate_tokens(&messages));
    }

    #[test]
    fn ledger_archivable_matches_slice_formula() {
        // The retired `archivable_tokens` walk: estimate(messages[1..]) minus
        // the larger of the last-K estimate and the token floor.
        let messages = sample_history();
        let ledger = TokenLedger::rebuild(&messages);
        let keep = 2usize;
        let floor = 50u64;
        let transcript = estimate_tokens(&messages[1..]);
        let recent = estimate_tokens(&messages[messages.len().saturating_sub(keep)..]);
        assert_eq!(
            ledger.archivable_tokens(keep, floor),
            transcript.saturating_sub(recent.max(floor))
        );
        // Degenerate histories archive nothing.
        assert_eq!(TokenLedger::rebuild(&[]).archivable_tokens(keep, floor), 0);
        assert_eq!(
            TokenLedger::rebuild(&messages[..1]).archivable_tokens(keep, floor),
            0
        );
    }
}
