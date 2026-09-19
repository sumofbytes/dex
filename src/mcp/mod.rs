//! MCP (Model Context Protocol) client: use external servers as tools.
//!
//! P0: `tools/list` + `tools/call` over stdio + Streamable HTTP, exposed as
//! `mcp__<server>__<tool>`. P1: `resources/list|read` + `prompts/list|get`
//! via one synthetic `mcp__<server>_read_resource` tool per server.
//!
//! Security: server allow/deny filtering, secret redaction in errors, tool
//! allowlist/denylist per server, a schema cap so one chatty server cannot
//! flood the context. See `SECURITY.md` (MCP section) for the threat model.
//!
//! Performance: one shared reqwest client, cached [`ToolDefinition`]s merged
//! synchronously into `tools_schema()` (never blocks the turn loop), fan-out
//! refresh via `JoinSet`, per-server timeout, 32 KiB output clamp. One bad
//! server goes `down` and never breaks the turn.

pub(crate) mod client;
pub(crate) mod config;
pub(crate) mod global;
pub(crate) mod manager;
pub(crate) mod mapping;
pub(crate) mod oauth;
pub(crate) mod redact;
pub(crate) mod transport;

#[cfg(test)]
pub(crate) use config::load_server_configs;
#[cfg(test)]
pub(crate) use config::{expand_env, mcp_enabled, mcp_tool_name, parse_mcp_servers};
#[cfg(test)]
pub(crate) use config::{MCP_DESC_LIMIT, MCP_TOOL_NAME_LIMIT};
#[cfg(test)]
pub(crate) use global::status_line;
pub(crate) use global::{
    cached_schema_tokens, cached_statuses, cached_tools, cached_truncated, call_global,
    ephemeral_line, global_manager,
};
pub(crate) use manager::ServerStatus;
#[cfg(test)]
pub(crate) use redact::{redact_line, redact_secrets};
#[cfg(test)]
pub(crate) use transport::{sse_result_id, sse_scan_buffered_id};

#[cfg(test)]
pub(crate) use global::TEST_ENV_LOCK;

#[cfg(test)]
mod tests;
