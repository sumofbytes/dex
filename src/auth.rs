//! Daemon-side bearer token location (the client-side credential lookup is
//! `dex_client::client_daemon_token` in the standalone crate).

use std::path::PathBuf;

/// Where the daemon publishes its auto-generated bearer token for clients
/// (`$XDG_DATA_HOME/dex/daemon.token`, 0600).
pub(crate) fn daemon_token_file() -> Option<PathBuf> {
    crate::runtime::logging::data_home().map(|base| base.join("dex/daemon.token"))
}
