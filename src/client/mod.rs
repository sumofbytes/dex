//! Compatibility exports for the standalone HTTP/SSE daemon client crate.

pub mod http;
pub use dex_client::{protocol, runtime, sse};
pub use http::{ChatOptions, DaemonClient};

#[cfg(test)]
mod e2e_tests;
