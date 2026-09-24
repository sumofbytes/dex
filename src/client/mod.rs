//! Compatibility exports for the standalone HTTP/SSE daemon client crate.
//!
//! Note: the re-exported `runtime` and the crate's own defaults (`DaemonClient::new`
//! token lookup, pool configuration, stderr warnings) are the *crate's* defaults,
//! not the app kernel's. In-process Dex always goes through `http::DaemonClient`,
//! which injects the app runtime, pools, credential lookup, and structured logging.

pub mod http;
pub use dex_client::{protocol, runtime, sse};
pub use http::{ChatOptions, DaemonClient};

#[cfg(test)]
pub(crate) mod e2e_tests;
