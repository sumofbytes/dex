/// Lifecycle snapshot of one MCP server for status lines and the UI panels.
/// Plain data so `render` can depend on it without reaching upward into
/// `mcp`.
#[derive(Clone, Debug)]
pub struct ServerStatus {
    pub name: String,
    pub state: String,
    pub tools: usize,
    pub error: Option<String>,
}

pub use dex_runtime::lines::ApprovalRequest;
