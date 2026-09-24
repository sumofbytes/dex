//! Reusable daemon library: run the agent daemon inside any process. The
//! HTTP+SSE edge ([`server::router`]) and the bootstrap ([`serve::run_daemon`])
//! are public so an embedder can serve the same API the built-in TUI talks to
//! and put their own UI on top.

pub(crate) mod approvals;
pub(crate) mod auth;
pub(crate) mod lookup;
pub mod server;
pub(crate) mod shell;
pub(crate) mod turn;
pub(crate) mod wake;

// Bearer-token auth lives in `auth.rs`; re-exported here so existing
// `daemon::...` paths keep working.
#[cfg(test)]
pub(crate) use auth::reset_daemon_token_for_tests;
pub(crate) use auth::{prepare_daemon_token, required_token};

pub mod serve;
pub(crate) mod state;
#[cfg(test)]
mod tests;
pub use serve::run_daemon;
pub(crate) use state::{journal_event, lock_map, SessionEntry, NEGATIVE_TTL};
pub use state::{DaemonState, PendingApproval};
