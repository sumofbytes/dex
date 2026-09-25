pub use dex_agent_core::{AgentMode, PermissionMode, Plan};
/// Explicit root re-export from `dex-ai` (no glob: the crate also publishes
/// `transport`/`streaming`/`wire` modules we keep behind `dex_ai::` paths).
pub use dex_ai::{
    ApiProtocol, ChatCompletionsRequest, ChatMessage, FunctionCall, FunctionDef, LlmToolCall,
    ModelEvent, Provider, Role, StopReason, StreamDelta, StreamFunctionCall, StreamOptions,
    StreamToolCall, StreamUsage, ToolDefinition, Usage, BASH_EXCLUDED_NAME,
};
pub use dex_protocol::*;

pub fn parse_permission_mode(value: &str) -> Result<PermissionMode, String> {
    let mode = PermissionMode::parse(value)?;
    if let Some((key, message)) = PermissionMode::deprecated_alias(value) {
        crate::runtime::notice::warn_once(key, message);
    }
    Ok(mode)
}

pub mod tokens;

/// Domain vocabulary shared across agent/tools/session/ui and persisted in
/// session JSONL (wire-schema items live in `shared.rs`).
#[path = "domain.rs"]
pub mod domain;

/// Skill records come from the standalone discovery crate; the daemon wire
/// maps them to `SkillInfo` on the way out.
pub use dex_skills::Skill;
pub use domain::*;

/// Wire schema for chat messages, tool definitions and stream chunk
/// parsing.
#[path = "shared.rs"]
pub mod shared;

pub use shared::*;
