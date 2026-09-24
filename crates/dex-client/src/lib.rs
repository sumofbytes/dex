//! Standalone HTTP/SSE client for a dex daemon.
//!
//! This crate depends on the shared wire contract and its own HTTP/runtime
//! defaults. Applications can use `DaemonClient::with_token` when they own
//! credential lookup themselves.

mod auth;
pub mod http;
pub mod protocol {
    pub use dex_protocol::*;
}
pub mod runtime;
pub mod sse;

pub use http::{ChatOptions, DaemonClient};
