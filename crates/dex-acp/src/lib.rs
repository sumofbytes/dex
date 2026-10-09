//! Agent Client Protocol (ACP) adapter for a dex daemon.
//!
//! Speaks newline-delimited JSON-RPC 2.0 over a reader/writer pair (stdio
//! for `dex acp`) and translates it onto the daemon's HTTP+SSE API through
//! [`dex_client`]. The adapter owns no agent logic: every ACP session is a
//! daemon session, every `session/prompt` is one chat turn, and permission
//! prompts are the daemon's approvals. Any other front end (a web UI, say)
//! can sit on the same daemon API without going through this crate.

mod map;
mod rpc;
mod server;

pub use server::{serve, serve_stdio};
