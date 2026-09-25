//! In-process transcript/approval vocabulary shared by the console, the
//! daemon and every UI. Plain data (no ANSI, no serde): UIs apply their own
//! styling; wire serialization happens in `dex-protocol`.
//!
//! These types live in `dex-runtime` (not the server crate) because
//! `runtime::console::Console` carries them, and the console is shared by
//! the server and the thin client. The server's `protocol` module
//! re-exports them so historical `protocol::SinkLine` /
//! `protocol::ApprovalRequest` paths keep working.

use dex_agent_core::Plan;
use dex_protocol::ApprovalDecision;

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
    Plan(Plan),
}

/// One tool/permission approval, routed to whichever surface can answer it
/// (headless console prompt, TUI modal, remote client).
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub name: String,
    pub input: String,
    /// Set when the requester is a background child agent (plan §12 V1b):
    /// children outlive the parent turn, so turn-end teardown must not deny
    /// their parked approvals. `None` for the parent turn's own tools.
    pub agent_id: Option<String>,
    /// The child's definition name for the labeled prompt (V1b): rendered
    /// as "explorer wants to run bash: …". `None` for the parent's own.
    pub agent: Option<String>,
    pub response: tokio::sync::mpsc::Sender<ApprovalDecision>,
}
