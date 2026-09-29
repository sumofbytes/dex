//! In-process transcript/approval vocabulary shared by the console, the
//! daemon and every UI. Plain data (no ANSI, no serde): UIs apply their own
//! styling; wire serialization happens in `dex-protocol`.
//!
//! These types live here (not `dex-protocol`, which stays wire-only)
//! because they are the in-process vocabulary that flows across the host
//! boundary (emitted by the server-side host adapter, carried by the
//! console), and they share `Plan` with the rest of the agent vocabulary.

use crate::Plan;
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

/// A pending `ask_user` batch, routed to whichever surface can answer it —
/// the in-process counterpart of [`ApprovalRequest`] with a variable
/// option list instead of a fixed decision enum. Like approvals it rides
/// a dedicated channel on the `Console` (not the transcript sink), and
/// the daemon-side bridge parks it, journals it, and broadcasts the wire
/// event. One sender resolves the whole batch — partial answers never
/// cross the host boundary; UIs buffer per-question state and submit
/// once. Teardown semantics match approvals: `agent_id` set means the
/// requester is a background child that outlives the parent turn, so
/// turn-end teardown must not dismiss its question.
#[derive(Clone, Debug)]
pub struct QuestionRequest {
    pub questions: Vec<dex_protocol::Question>,
    pub agent_id: Option<String>,
    /// The child's definition name for the labeled prompt ("explorer
    /// asks: …"). `None` for the parent turn's own questions.
    pub agent: Option<String>,
    pub response: tokio::sync::mpsc::Sender<Vec<dex_protocol::QuestionAnswer>>,
}

/// One tool/permission approval, routed to whichever surface can answer it
/// (headless console prompt, TUI modal, remote client).
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub name: String,
    pub input: String,
    /// Set when the requester is a background child agent:
    /// children outlive the parent turn, so turn-end teardown must not deny
    /// their parked approvals. `None` for the parent turn's own tools.
    pub agent_id: Option<String>,
    /// The child's definition name for the labeled prompt: rendered
    /// as "explorer wants to run bash: …". `None` for the parent's own.
    pub agent: Option<String>,
    pub response: tokio::sync::mpsc::Sender<ApprovalDecision>,
}
