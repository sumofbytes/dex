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

/// Byte length of a message's replayed payload (content, tool calls,
/// reasoning, framing) — the shared body of the token estimator, used by
/// `estimate_tokens` and the pi-style cut-point walk.
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
        + message
            .reasoning_items
            .as_ref()
            .map_or(0, |items| items.iter().map(|v| v.to_string().len()).sum());
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

/// Effective prompt size = persistent history + ephemeral preamble + tool schema.
pub(crate) fn effective_tokens(
    messages: &[ChatMessage],
    ephemeral: &[Option<String>],
    with_tools: bool,
) -> u64 {
    estimate_tokens(messages)
        + estimate_ephemeral_tokens(ephemeral)
        + if with_tools {
            // Native schema is a flat estimate; the MCP slice is live —
            // without it the compaction threshold ignores the per-request
            // schema cost that actually fills the window.
            TOOL_SCHEMA_TOKENS + crate::mcp::cached_schema_tokens()
        } else {
            0
        }
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
