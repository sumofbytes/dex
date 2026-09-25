//! Dex server side: the daemon (axum + SSE), the agent turn engine and its
//! provider clients, workspace-confined tools, MCP integration, Lua
//! extensions, session persistence glue, and headless rendering.
//!
//! Everything an embedder needs to run a dex daemon lives here behind a
//! public API: [`daemon`] for the HTTP+SSE server, [`llm`] for provider
//! configuration, [`mcp`] / [`extensions`] for integration state, and
//! [`session`] for the JSONL session composition layer. The `dex` binary is
//! a thin client over this API: its TUI talks to the daemon through
//! `dex-client` over HTTP+SSE (or an in-process loopback socket).

/// Historical `crate::agent::` paths for the turn-loop lifecycle wrapper.
/// Internal: was `pub(crate)` before the crate split; `#[doc(hidden)]` is an
/// interim signal until the embedder surface is curated.
#[doc(hidden)]
pub mod agent;
#[doc(hidden)]
pub mod auth;
pub mod daemon;
pub mod extensions;
pub mod llm;
pub mod mcp;
pub mod protocol;
/// Historical `crate::render::` paths for the headless format/theme.
/// Internal: was `pub(crate)` before the crate split.
#[doc(hidden)]
pub mod render;
pub mod session;
#[doc(hidden)]
pub mod telemetry;
/// Historical `crate::tools::` paths for the workspace-confined tools.
/// Internal: was `pub(crate)` before the crate split.
#[doc(hidden)]
pub mod tools;
#[doc(hidden)]
pub mod workspace;

/// Process-global runtime lives in the standalone `dex-runtime` crate; the
/// `crate::runtime::` paths used throughout the server keep working via
/// this re-export.
pub use dex_runtime as runtime;

/// Shared test helpers (`crate::test_env` in test modules below).
#[cfg(test)]
pub use dex_runtime::test_env;
