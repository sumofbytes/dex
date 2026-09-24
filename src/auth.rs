//! Daemon-side bearer token location and management helpers.

use std::path::PathBuf;

/// Where the daemon publishes its auto-generated bearer token for clients
/// (`$XDG_DATA_HOME/dex/daemon.token`, 0600).
pub(crate) fn daemon_token_file() -> Option<PathBuf> {
    crate::runtime::logging::data_home().map(|base| base.join("dex/daemon.token"))
}

/// Resolve the daemon credential for the in-process Dex client: explicit
/// environment value first, then the token file published by a non-loopback
/// daemon.
pub(crate) fn client_daemon_token() -> Option<String> {
    if let Ok(token) = std::env::var("DEX_DAEMON_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    daemon_token_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}
