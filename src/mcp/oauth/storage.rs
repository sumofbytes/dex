use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) const CALLBACK_TIMEOUT_SECS: u64 = 180;
pub(crate) const DISCOVERY_TIMEOUT_SECS: u64 = 15;
/// Tokens with less than this much lifetime left count as expired.
const EXPIRY_SKEW_SECS: u64 = 60;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OAuthToken {
    pub(crate) access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    /// Unix seconds; `None` = unknown lifetime (use until a 401).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    #[serde(default)]
    pub(crate) token_endpoint: String,
    #[serde(default)]
    pub(crate) client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) client_secret: Option<String>,
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl OAuthToken {
    pub(crate) fn usable(&self) -> bool {
        if self.access_token.is_empty() {
            return false;
        }
        self.expires_at
            .map(|e| now_secs() + EXPIRY_SKEW_SECS < e)
            .unwrap_or(true)
    }

    pub(crate) fn refreshable(&self) -> bool {
        self.refresh_token.as_deref().is_some_and(|r| !r.is_empty())
            && !self.token_endpoint.is_empty()
            && !self.client_id.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Token store
// ---------------------------------------------------------------------------

pub(crate) fn oauth_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("dex/mcp");
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".local/share/dex/mcp"))
        .unwrap_or_else(|| PathBuf::from(".dex/mcp"))
}

pub(crate) fn token_path(server: &str) -> PathBuf {
    // Sanitize here (not just at call sites): `server` becomes a file name,
    // so `../../x` must map to `______x.json` inside `oauth_dir`, never a
    // traversal. Idempotent over already-sanitized names.
    oauth_dir().join(format!(
        "{}.json",
        super::super::sanitize_server_name(server)
    ))
}

/// Any failure (missing/corrupt/empty) is "no token", never an error: auth
/// state must not break the turn loop or status rendering.
pub(crate) fn load_token(server: &str) -> Option<OAuthToken> {
    let text = std::fs::read_to_string(token_path(server)).ok()?;
    let tok: OAuthToken = serde_json::from_str(&text).ok()?;
    if tok.access_token.is_empty() {
        return None;
    }
    Some(tok)
}

/// A stored token with enough lifetime left to use on requests.
pub(crate) fn valid_token(server: &str) -> Option<OAuthToken> {
    load_token(server).filter(|t| t.usable())
}

pub(crate) fn save_token(server: &str, tok: &OAuthToken) -> Result<(), String> {
    let path = token_path(server);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mcp oauth: {e}"))?;
    }
    let text = serde_json::to_string_pretty(tok).map_err(|e| format!("mcp oauth: {e}"))?;
    #[cfg(unix)]
    {
        // Create with 0600 from the start: `fs::write` + `set_permissions`
        // leaves a world-readable window (umask 022). `open` does not chmod
        // an existing file, so force the mode afterwards too.
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("mcp oauth: {e}"))?;
        f.write_all(text.as_bytes())
            .map_err(|e| format!("mcp oauth: {e}"))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, text).map_err(|e| format!("mcp oauth: {e}"))?;
    }
    Ok(())
}

/// True when a token file was removed.
pub(crate) fn clear_token(server: &str) -> bool {
    std::fs::remove_file(token_path(server)).is_ok()
}

fn human_expiry(tok: &OAuthToken) -> String {
    match tok.expires_at {
        None => "logged in (no expiry advertised)".to_string(),
        Some(e) => {
            let now = now_secs();
            if e <= now + EXPIRY_SKEW_SECS {
                "expired — re-run `dex mcp login`".to_string()
            } else {
                let left = e - now;
                let span = if left >= 172_800 {
                    format!("{}d", left / 86_400)
                } else if left >= 7_200 {
                    format!("{}h", left / 3_600)
                } else if left >= 120 {
                    format!("{}m", left / 60)
                } else {
                    format!("{left}s")
                };
                format!("logged in (expires in {span})")
            }
        }
    }
}

pub(crate) fn status_line(server: &str) -> String {
    match load_token(server) {
        None => format!("{server}: not logged in — `dex mcp login {server}`"),
        Some(tok) => format!("{server}: {}", human_expiry(&tok)),
    }
}

/// Auth status for one server, or `None` when there is nothing to log in
/// to: stdio servers carry no OAuth, so `/mcp` and `dex mcp status` skip
/// them instead of crying "not logged in".
pub(crate) fn auth_line_with(
    configs: &std::collections::BTreeMap<String, super::super::McpServerConfig>,
    server: &str,
) -> Option<String> {
    let http = configs
        .get(&super::super::sanitize_server_name(server))
        .is_some_and(|cfg| cfg.is_http());
    http.then(|| status_line(server))
}

/// Auth statuses for every HTTP server (sorted by name). Loads the config
/// once, then maps over statuses (no N+1 reloads).
pub(crate) fn auth_lines() -> Vec<String> {
    let configs = super::super::load_server_configs();
    configs
        .keys()
        .filter_map(|name| auth_line_with(&configs, name))
        .collect()
}

// ---------------------------------------------------------------------------
// Small pure helpers (unit-tested)
// ---------------------------------------------------------------------------
