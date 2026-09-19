pub(crate) mod approvals;
pub(crate) mod auth;
pub(crate) mod lookup;
pub(crate) mod server;
pub(crate) mod shell;
pub(crate) mod turn;
pub(crate) mod wake;

// Bearer-token auth lives in `auth.rs`; re-exported here so existing
// `daemon::...` paths keep working.
#[cfg(test)]
pub(crate) use auth::reset_daemon_token_for_tests;
pub(crate) use auth::{daemon_token_file, prepare_daemon_token, required_token};

pub(crate) mod serve;
pub(crate) mod state;
#[cfg(test)]
mod tests;
pub(crate) use serve::run_daemon;
pub(crate) use state::{
    journal_event, lock_map, DaemonState, PendingApproval, SessionEntry, NEGATIVE_TTL,
};
