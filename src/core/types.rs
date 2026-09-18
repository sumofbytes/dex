//! Dissolving facade (plan §4 Phase 3): the wire/shared vocabulary moved to
//! `protocol::{domain,shared}`; `Skill`/`Plan` (domain types persisted in
//! sessions) live in `protocol/domain.rs`. Delete once no `crate::core::types`
//! references remain.
pub(crate) use crate::protocol::shared::{
    ApiProtocol, ApprovalDecision, ApprovalRequest, BASH_EXCLUDED_NAME, ChatMessage, ChatRequest,
    FunctionCall, FunctionDef, LlmToolCall, PermissionMode, Provider, QueueMsg, Role, SinkLine,
    StopReason, StreamChunk, StreamChoice, StreamDelta, StreamFunctionCall, StreamOptions,
    StreamToolCall, StreamUsage, ToolDefinition, Usage, WireMessage,
};
pub(crate) use crate::protocol::domain::{Plan, Skill};
