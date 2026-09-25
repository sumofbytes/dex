//! Dex thin client: TUI + CLI over the daemon API.
//!
//! The server side (daemon, agent turn engine, provider clients, tools,
//! MCP, extensions, sessions, headless rendering) lives in the standalone
//! `dex-server` crate; this crate is a thin
//! client that talks to it over HTTP+SSE via [`client`] (or an in-process
//! loopback socket for `dex`'s default mode). It re-exports the server API
//! so `dex serve` and embedders can bootstrap a daemon from the binary.

pub mod app;
pub mod cli;
pub mod client;
#[cfg(feature = "tui")]
mod ui;

pub use app::run;

// Compat re-exports: the server side moved to the `dex-server` crate, and
// the historical `crate::<module>::` paths keep working through these.
pub use dex_runtime as runtime;
pub use dex_server::{
    agent, daemon, extensions, llm, mcp, protocol, render, session, telemetry, tools,
};

/// Shared test helpers for this crate's tests (`crate::test_env`).
#[cfg(test)]
pub use dex_runtime::test_env;
