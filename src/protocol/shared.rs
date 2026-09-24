//! Shared message/role/approval/permission vocabulary (from the old
//! core/types.rs;
//! dissolving in Phase 3 — see protocol/shared.rs for the split map).

/// A single streamed line destined for the UI transcript. Plain text (no
/// ANSI) so the UI applies its own styling. Ratatui-agnostic.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SinkLine {
    Assistant(String),
    /// Incremental model reasoning ("thinking") delta. UIs render a collapsed
    /// one-line preview and can expand the full text on demand.
    Thinking(String),
    /// A tool call started. Emitted when execution begins — not when it
    /// finishes — so surfaces can show the call while it runs; the matching
    /// `ToolOutput` carries the same `id`. `id` is empty when unknown
    /// (session-file rebuild, old journals); outputs with an empty id
    /// attach to the tail block as before.
    ToolInput {
        id: String,
        input: String,
    },
    ToolOutput {
        /// Pairs with the `ToolInput` emitted when this call started.
        /// Empty for legacy inputs (rebuild/old journals): the output then
        /// attaches to the tail block as before.
        id: String,
        name: String,
        summary: String,
        /// Whether the tool call succeeded; rendered as ✓/✗ by the UIs.
        success: bool,
        /// A few informational output lines shown dim under the summary.
        preview: Vec<String>,
        /// Wall-clock seconds the tool took; 0 when unknown.
        duration: f64,
    },
    System(String),
    Error(String),
    /// Prompt tokens reported by the provider after each LLM call, so the
    /// status bar can track context usage live instead of once per turn.
    /// `cached` is the provider-reported cached-token subset, when reported;
    /// `cost` is the USD cost of the call as priced by the daemon; `output`
    /// is the completion-token count for the same call; `gen_ms` is the
    /// wall-clock duration the caller measured for the whole LLM call, when
    /// known (the denominator for the footer's output tokens/s rate).
    Usage {
        tokens: u64,
        cached: Option<u64>,
        cost: f64,
        output: u64,
        gen_ms: Option<u64>,
    },
    Plan(crate::protocol::Plan),
}

/// One message on a per-turn steering or follow-up queue. `Content` enqueues a
/// not-yet-delivered item; `Recall` cancels a queued item (matched by content)
/// so the client can pull it back into the composer and edit it. The consumer
/// applies recalls in arrival order, so a recall only cancels an item that has
/// not yet been injected into the conversation — an already-injected item is
/// part of the transcript and cannot be pulled back.
#[derive(Debug)]
pub enum QueueMsg {
    Content(String),
    Recall(String),
}
