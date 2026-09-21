//! Daemon bearer-token auth: the single owner of the daemon credential.
//!
//! Resolution: `DEX_DAEMON_TOKEN` always wins; a loopback bind needs no
//! token; any other bind generates one, publishes it to `daemon.token`
//! (0600, atomically), and prints it once. Serving a workspace-running
//! agent on a reachable interface with no authentication would hand
//! arbitrary code execution to anyone on the network.
//!
//! Single-global file: two daemons on one machine share `daemon.token` —
//! the second overwrites the first. Clients connecting to multiple daemons
//! must pass per-host `DEX_DAEMON_TOKEN` explicitly.

use std::path::PathBuf;
use std::sync::Mutex;

use super::lock_map;

/// Where the daemon publishes its auto-generated bearer token for clients
/// (`$XDG_DATA_HOME/dex/daemon.token`, 0600).
pub(crate) fn daemon_token_file() -> Option<PathBuf> {
    crate::runtime::logging::data_home().map(|base| base.join("dex/daemon.token"))
}

static REQUIRED_TOKEN: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Atomically write `token` to `path` with 0600 (no world-readable window).
fn write_token_file(path: &PathBuf, token: &str) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let content = format!("{token}\n");
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(content.as_bytes()).is_ok()
            }
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        match std::fs::write(path, format!("{token}\n")) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

pub(crate) fn prepare_daemon_token(addr: &std::net::SocketAddr) {
    let token = std::env::var("DEX_DAEMON_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            if addr.ip().is_loopback() {
                return None;
            }
            let token = uuid::Uuid::new_v4().to_string();
            let published = match daemon_token_file() {
                Some(path) => write_token_file(&path, &token),
                None => false,
            };
            eprintln!(
                "daemon: generated bearer token for {addr} (written to daemon.token: {published})"
            );
            eprintln!("daemon: clients connect with DEX_DAEMON_TOKEN=<token>");
            Some(token)
        });
    *lock_map(&REQUIRED_TOKEN) = Some(token);
}

/// The credential this daemon process requires (`None` → unauthenticated).
/// Cloned (not `&'static`) so tests can re-resolve per case without
/// poisoning a process-global `OnceLock`.
pub(crate) fn required_token() -> Option<String> {
    lock_map(&REQUIRED_TOKEN).clone().flatten()
}

#[cfg(test)]
pub(crate) fn reset_daemon_token_for_tests() {
    *lock_map(&REQUIRED_TOKEN) = None;
}
