//! Local token-file lookup used by the default client constructor.

pub(crate) fn client_daemon_token() -> Option<String> {
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
