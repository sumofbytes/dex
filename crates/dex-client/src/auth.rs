//! Local token lookup used by the crate-default `DaemonClient::new`.
//!
//! Embedding hosts that manage credentials themselves should prefer
//! `DaemonClient::with_token` and never call this.

/// Resolve the daemon credential for the crate-default constructor: explicit
/// `DEX_DAEMON_TOKEN` first, then the token file the daemon publishes at
/// `$XDG_DATA_HOME/dex/daemon.token` (0600).
pub fn client_daemon_token() -> Option<String> {
    if let Ok(token) = std::env::var("DEX_DAEMON_TOKEN") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })?;
    std::fs::read_to_string(data_home.join("dex/daemon.token"))
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_env_var_falls_through_to_token_file() {
        // Empty/whitespace `DEX_DAEMON_TOKEN` must not shadow a real token
        // file; the env check trims and rejects empties before the file read.
        let data = std::env::temp_dir().join(format!("dex-client-auth-{}", std::process::id()));
        std::fs::create_dir_all(data.join("dex")).unwrap();
        std::fs::write(data.join("dex/daemon.token"), "  file-token \n").unwrap();
        let saved_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("DEX_DAEMON_TOKEN", "   ");
        std::env::set_var("XDG_DATA_HOME", &data);
        assert_eq!(client_daemon_token().as_deref(), Some("file-token"));
        std::env::remove_var("DEX_DAEMON_TOKEN");
        match saved_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        assert!(std::fs::remove_dir_all(&data).is_ok());
    }
}
