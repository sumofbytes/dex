//! Reusable daemon client library: `DaemonClient` speaks the full daemon
//! HTTP+SSE protocol (sessions, chat streaming, approvals, steering, shell)
//! and is the seam any custom UI builds on — the built-in TUI uses exactly
//! this surface. Wire types live in [`crate::protocol`].

pub mod http;
pub(crate) mod runtime;
pub(crate) mod sse;

pub use http::{ChatOptions, DaemonClient};
