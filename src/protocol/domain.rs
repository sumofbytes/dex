use crate::protocol::ApprovalDecision;
/// Lifecycle snapshot of one MCP server for status lines and the UI panels.
/// Plain data so `render` can depend on it without reaching upward into
/// `mcp`.
#[derive(Clone, Debug)]
pub(crate) struct ServerStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) tools: usize,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalRequest {
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
