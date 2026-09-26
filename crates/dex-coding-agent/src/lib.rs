//! Dex coding-agent behavior that can be reused independently of its host.
//!
//! The crate owns the built-in tool catalog and stable schema composition.
//! Hosts decide which optional tools are available and provide dynamic MCP
//! and extension definitions; execution, approvals, and UI remain host-owned.

mod registry;
mod tool_policy;
mod tool_results;
mod tools;

pub use registry::{ToolEntry, ToolRegistry};

pub use tool_policy::{
    native_tool_metadata, needs_approval, ApprovalPolicy, DefaultApprovalPolicy, FnApprovalPolicy,
    PermissionRequirement, ToolMetadata,
};
pub use tool_results::{
    normalize_tool_result, normalize_tool_result_with, NormalizedToolResult, ResultPolicy,
    ToolExecutionResult,
};
pub use tools::{builtin_tools, merge_tool_schemas, sort_tool_defs_by_name};
