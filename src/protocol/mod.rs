pub use dex_agent_core::{AgentMode, PermissionMode, Plan};
/// Explicit root re-export from `dex-ai` (no glob: the crate also publishes
/// `transport`/`streaming`/`wire` modules we keep behind `dex_ai::` paths).
pub use dex_ai::{
    ApiProtocol, ChatCompletionsRequest, ChatMessage, FunctionCall, FunctionDef, LlmToolCall,
    ModelEvent, Provider, Role, StopReason, StreamDelta, StreamFunctionCall, StreamOptions,
    StreamToolCall, StreamUsage, ToolDefinition, Usage, BASH_EXCLUDED_NAME,
};
pub use dex_protocol::*;

pub(crate) fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    let mode = PermissionMode::parse(value)?;
    if let Some((key, message)) = PermissionMode::deprecated_alias(value) {
        crate::runtime::notice::warn_once(key, message);
    }
    Ok(mode)
}

pub(crate) mod tokens;

/// Domain vocabulary shared across agent/tools/session/ui and persisted in
/// session JSONL (plan §4 Phase 3: wire-schema items went to `shared.rs`,
/// these stay domain — sessions are the wire).
#[path = "domain.rs"]
pub(crate) mod domain;

pub(crate) use domain::*;

/// Wire schema for chat messages, tool definitions and stream chunk
/// parsing (plan §4 Phase 3: formerly `core/types.rs`).
#[path = "shared.rs"]
pub(crate) mod shared;

pub(crate) use shared::*;
