//! Turn-scoped token budgeting that reaches upward into `mcp`/`extensions`
//! for the live tool-schema slices. Pure estimation lives one layer down in
//! [`crate::protocol::tokens`].

// Re-exports so callers keep one path (`agent::tokens::...`). A few are
// only consumed by the TUI (`format_tokens`) or tests, hence the allow.
#[allow(unused_imports)]
pub(crate) use crate::protocol::tokens::{
    estimate_ephemeral_tokens, estimate_tokens, format_tokens, message_char_len, schema_chars,
    schema_token_estimate, TokenLedger, PER_MESSAGE_OVERHEAD,
};

/// Rough cost of the tool definitions sent with every request.
const TOOL_SCHEMA_TOKENS: u64 = 3200;

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
