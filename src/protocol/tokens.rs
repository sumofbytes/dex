use super::ChatMessage;

/// Token overhead per message (role, formatting, turn boundary).
pub(crate) const PER_MESSAGE_OVERHEAD: u64 = 12;

#[cfg_attr(not(any(feature = "tui", test)), allow(dead_code))]
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
                .map(|item| serde_json::to_string(item).map_or(0, |s| s.len()))
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
}

/// Token cost of the live MCP tool-schema slice (`mcp.rs:cached_tools`).
/// Same ~4-chars-per-token heuristic as [`estimate_tokens`]: namespaced
/// name + description + serialized parameters per definition.
pub(crate) fn schema_token_estimate(defs: &[super::ToolDefinition]) -> u64 {
    (schema_chars(defs) as u64) / 4 + (defs.len() as u64 * PER_MESSAGE_OVERHEAD)
}

/// Raw char count behind [`schema_token_estimate`]: per-tool shares for the
/// extension `active`-slice budget, which replicates the sum-then-divide
/// formula on the subset so whole-cache and sliced totals stay bit-identical.
pub(crate) fn schema_chars(defs: &[super::ToolDefinition]) -> usize {
    defs.iter()
        .map(|d| {
            d.function.name.len()
                + d.function.description.len()
                + d.function.parameters.to_string().len()
        })
        .sum()
}

/// Human token count for status lines: `941`, `12.3k`, `1.2M`. Shared by the
/// TUI status bar and the subagent lifecycle line so both renderers agree.
pub(crate) fn format_tokens(tokens: u64) -> String {
    match tokens {
        // A trailing ".0" is wasted width in the status bar: 12.0k -> 12k.
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0).replace(".0M", "M"),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0).replace(".0k", "k"),
        t => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::ChatMessage;
    use super::*;

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
}
