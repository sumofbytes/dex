//! MCP OAuth login for Streamable-HTTP servers: protected-resource
//! metadata (RFC9728) + authorization-server metadata (RFC8414) + dynamic
//! client registration (RFC7591) + PKCE S256 (RFC7636), loopback redirect.
//!
//! `dex mcp login <server>` exchanges a browser approval for tokens stored
//! in `$XDG_DATA_HOME/dex/mcp/<server>.json` (0600, never in sessions or
//! logs). [`super::HttpTransport`] injects a valid token per request and
//! turns a `Bearer` 401 into a login hint; a stored refresh token is tried
//! once before the hint surfaces.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::redact_secrets;

const CALLBACK_TIMEOUT_SECS: u64 = 180;
const DISCOVERY_TIMEOUT_SECS: u64 = 15;
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl OAuthToken {
    fn usable(&self) -> bool {
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
    oauth_dir().join(format!("{}.json", super::sanitize_server_name(server)))
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
    configs: &std::collections::BTreeMap<String, super::McpServerConfig>,
    server: &str,
) -> Option<String> {
    let http = configs
        .get(&super::sanitize_server_name(server))
        .is_some_and(|cfg| cfg.is_http());
    http.then(|| status_line(server))
}

/// Auth statuses for every HTTP server (sorted by name). Loads the config
/// once, then maps over statuses (no N+1 reloads).
pub(crate) fn auth_lines() -> Vec<String> {
    let configs = super::load_server_configs();
    configs
        .keys()
        .filter_map(|name| auth_line_with(&configs, name))
        .collect()
}

// ---------------------------------------------------------------------------
// Small pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// `Bearer resource_metadata="https://…"` out of a `WWW-Authenticate` value.
/// Case-insensitive key, tolerates extra params/whitespace; only http(s).
pub(crate) fn parse_resource_metadata_url(header: &str) -> Option<String> {
    const KEY: &str = "resource_metadata";
    // `to_ascii_lowercase` preserves byte offsets; `to_lowercase` can expand
    // non-ASCII and desync `lower` indexes from `header`/`bytes` indexes.
    let lower = header.to_ascii_lowercase();
    let bytes = header.as_bytes();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(KEY) {
        let mut rest = search + rel + KEY.len();
        while matches!(bytes.get(rest), Some(b' ' | b'\t')) {
            rest += 1;
        }
        if bytes.get(rest) != Some(&b'=') {
            search = rest;
            continue;
        }
        rest += 1;
        while matches!(bytes.get(rest), Some(b' ' | b'\t')) {
            rest += 1;
        }
        if bytes.get(rest) != Some(&b'"') {
            search = rest;
            continue;
        }
        rest += 1;
        let end = header[rest..].find('"')?;
        let url = header[rest..rest + end].trim();
        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(url.to_string());
        }
        search = rest + end + 1;
    }
    None
}

/// RFC3986 percent-encoding for query/form components.
pub(crate) fn percent_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if UNRESERVED.contains(b) {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub(crate) fn percent_decode(s: &str) -> String {
    // RFC3986: only `%XX` decodes. A literal `+` stays `+` — mapping it to
    // space (form-encoding) corrupts authorization codes in the loopback
    // callback query.
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE pair: 32 OS-random bytes (two v4 UUIDs — `uuid` is already a dep)
/// as a 43-char verifier, plus its S256 challenge.
pub(crate) fn pkce_pair() -> (String, String) {
    let a = uuid::Uuid::new_v4();
    let b = uuid::Uuid::new_v4();
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(a.as_bytes());
    raw[16..].copy_from_slice(b.as_bytes());
    let verifier = base64_url_no_pad(&raw);
    let challenge = code_challenge(&verifier);
    (verifier, challenge)
}

/// RFC7636 §4.2 test-vector target: `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`.
pub(crate) fn code_challenge(verifier: &str) -> String {
    use sha2::Digest as _;
    base64_url_no_pad(&sha2::Sha256::digest(verifier.as_bytes()))
}

fn random_state() -> String {
    base64_url_no_pad(uuid::Uuid::new_v4().as_bytes())
}

/// True when `header` carries a `Bearer` WWW-Authenticate challenge.
/// Matches the scheme token only (`Bearer`, `Bearer realm=…`,
/// `Basic…, Bearer …`), not substrings like `Bearertoken`.
pub(crate) fn is_bearer_challenge(header: &str) -> bool {
    let lower = header.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("bearer") {
        let abs = i + rel;
        let prev_ok = abs == 0 || matches!(bytes[abs - 1], b' ' | b'\t' | b',' | b'"');
        let next = bytes.get(abs + 6).copied();
        let next_ok = matches!(next, None | Some(b' ' | b'\t' | b',' | b'"'));
        if prev_ok && next_ok {
            return true;
        }
        i = abs + 6;
        if i >= lower.len() {
            break;
        }
    }
    false
}

fn host_is_loopback(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("http://127.0.0.1")
        || lower.starts_with("http://localhost")
        || lower.starts_with("http://[::1]")
        || lower.starts_with("https://127.0.0.1")
        || lower.starts_with("https://localhost")
        || lower.starts_with("https://[::1]")
}

/// Discovery/token URLs must be https, except loopback http for local dev.
/// Blocks credential + metadata fetch over cleartext to a remote host.
fn ensure_https_or_loopback(url: &str, context: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://") || host_is_loopback(url) {
        return Ok(());
    }
    Err(format!(
        "mcp oauth {context}: refusing non-https url (loopback http only)"
    ))
}

/// Negative cache for failed refreshes: one failing AS must not be hammered
/// on every tool call. `true` while a failure was recorded within 60s.
fn refresh_fail_table(
) -> &'static std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static FAIL_AT: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    FAIL_AT.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

pub(crate) fn refresh_backoff_active(server: &str) -> bool {
    let mut guard = refresh_fail_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(at) = guard.get(server) {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return true;
        }
        guard.remove(server);
    }
    false
}

pub(crate) fn note_refresh_failure(server: &str) {
    refresh_fail_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(server.to_string(), std::time::Instant::now());
}

pub(crate) fn clear_refresh_backoff(server: &str) {
    refresh_fail_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(server);
}

/// `https://host:port/path` → `(origin, path)` without a URL parser.
/// Query/fragment are stripped so they cannot leak into a derived
/// `/.well-known/…` URL (`https://h.example?x` → origin `https://h.example`).
pub(crate) fn split_origin(url: &str) -> (String, String) {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let rest = clean.split_once("://").map(|(_, r)| r).unwrap_or(clean);
    match rest.find('/') {
        Some(i) => {
            let origin = clean[..clean.len() - rest.len() + i].to_string();
            (origin, rest[i..].to_string())
        }
        None => (clean.to_string(), String::new()),
    }
}

/// RFC9728 §3.1 fallback: `/.well-known/oauth-protected-resource` + path.
fn well_known_resource_url(base: &str) -> String {
    let (origin, path) = split_origin(base.trim_end_matches('/'));
    format!("{origin}/.well-known/oauth-protected-resource{path}")
}

// ---------------------------------------------------------------------------
// Discovery + registration + token HTTP (form bodies are hand-encoded so no
// new reqwest features are needed)
// ---------------------------------------------------------------------------

fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn http_error(context: &str, text: &str) -> String {
    let short: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect();
    redact_secrets(&format!("mcp oauth {context}: {short}"))
}

#[derive(Debug)]
struct ResourceMetadata {
    authorization_servers: Vec<String>,
    scopes: Vec<String>,
}

async fn fetch_resource_metadata(
    http: &reqwest::Client,
    url: &str,
) -> Result<ResourceMetadata, String> {
    ensure_https_or_loopback(url, "resource metadata")?;
    let resp = http
        .get(url)
        .header("accept", "application/json")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("mcp oauth: resource metadata: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "mcp oauth: resource metadata: http {}",
            resp.status()
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("mcp oauth: resource metadata: {e}"))?;
    let authorization_servers: Vec<String> = v
        .get("authorization_servers")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if authorization_servers.is_empty() {
        return Err("mcp oauth: resource metadata lists no authorization_servers".to_string());
    }
    let scopes = v
        .get("scopes_supported")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ResourceMetadata {
        authorization_servers,
        scopes,
    })
}

#[derive(Debug)]
struct AuthServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

async fn fetch_auth_server_metadata(
    http: &reqwest::Client,
    issuer: &str,
) -> Result<AuthServerMetadata, String> {
    ensure_https_or_loopback(issuer, "issuer")?;
    let issuer = issuer.trim_end_matches('/');
    let (origin, path) = split_origin(issuer);
    let path = path.trim_end_matches('/');
    let mut candidates = vec![format!("{issuer}/.well-known/oauth-authorization-server")];
    if !path.is_empty() {
        candidates.push(format!(
            "{origin}/.well-known/oauth-authorization-server{path}"
        ));
    }
    candidates.push(format!("{issuer}/.well-known/openid-configuration"));
    let mut last = "no candidate fetched".to_string();
    for url in &candidates {
        let resp = match http
            .get(url)
            .header("accept", "application/json")
            .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        if !resp.status().is_success() {
            last = format!("http {}", resp.status());
            continue;
        }
        let v: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                last = e.to_string();
                continue;
            }
        };
        let authorization_endpoint = v
            .get("authorization_endpoint")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        let token_endpoint = v
            .get("token_endpoint")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if authorization_endpoint.is_empty() || token_endpoint.is_empty() {
            last = "metadata missing authorization/token endpoints".to_string();
            continue;
        }
        if ensure_https_or_loopback(&authorization_endpoint, "authorization_endpoint").is_err()
            || ensure_https_or_loopback(&token_endpoint, "token_endpoint").is_err()
        {
            last = "metadata endpoints must be https (loopback http only)".to_string();
            continue;
        }
        return Ok(AuthServerMetadata {
            authorization_endpoint,
            token_endpoint,
            registration_endpoint: v
                .get("registration_endpoint")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        });
    }
    Err(redact_secrets(&format!(
        "mcp oauth: authorization-server metadata not found ({last})"
    )))
}

struct ClientRegistration {
    client_id: String,
    client_secret: Option<String>,
}

async fn register_client(
    http: &reqwest::Client,
    endpoint: &str,
    redirect_uri: &str,
) -> Result<ClientRegistration, String> {
    ensure_https_or_loopback(endpoint, "registration_endpoint")?;
    let body = serde_json::json!({
        "redirect_uris": [redirect_uri],
        "client_name": "dex",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = http
        .post(endpoint)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("mcp oauth: client registration: {e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error("client registration failed", &text));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("mcp oauth: client registration: {e}"))?;
    let client_id = v
        .get("client_id")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if client_id.is_empty() {
        return Err("mcp oauth: registration returned no client_id".to_string());
    }
    Ok(ClientRegistration {
        client_id,
        client_secret: v
            .get("client_secret")
            .and_then(|s| s.as_str())
            .map(str::to_string),
    })
}

fn token_from_response(
    v: &serde_json::Value,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<String>,
    keep_refresh: Option<String>,
) -> Result<OAuthToken, String> {
    let access_token = v
        .get("access_token")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return Err(http_error(
            "token response has no access_token",
            &v.to_string(),
        ));
    }
    let expires_at = v
        .get("expires_in")
        .and_then(|e| e.as_u64().or_else(|| e.as_str()?.parse::<u64>().ok()))
        .map(|s| now_secs() + s);
    Ok(OAuthToken {
        access_token,
        refresh_token: v
            .get("refresh_token")
            .and_then(|s| s.as_str())
            .map(str::to_string)
            .or(keep_refresh),
        expires_at,
        token_endpoint: token_endpoint.to_string(),
        client_id: client_id.to_string(),
        client_secret,
    })
}

async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthToken, String> {
    ensure_https_or_loopback(token_endpoint, "token_endpoint")?;
    let mut pairs = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    let secret;
    if let Some(s) = client_secret {
        secret = s.to_string();
        pairs.push(("client_secret", secret.as_str()));
    }
    let resp = http
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .body(form_body(&pairs))
        .send()
        .await
        .map_err(|e| format!("mcp oauth: code exchange: {e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(http_error("code exchange failed", &text));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("mcp oauth: code exchange: {e}"))?;
    token_from_response(
        &v,
        token_endpoint,
        client_id,
        client_secret.map(str::to_string),
        None,
    )
}

/// One silent refresh. `invalid_grant` means the stored token is dead: the
/// caller drops the file so later requests fail fast with the login hint.
pub(crate) async fn refresh_access_token(saved: &OAuthToken) -> Result<OAuthToken, String> {
    if !saved.refreshable() {
        return Err("mcp oauth: stored token is not refreshable (log in again)".to_string());
    }
    ensure_https_or_loopback(&saved.token_endpoint, "token_endpoint")?;
    let http = crate::client::http::shared_async_client();
    let refresh = saved.refresh_token.clone().unwrap_or_default();
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", saved.client_id.as_str()),
    ];
    let secret;
    if let Some(s) = saved.client_secret.as_deref() {
        secret = s.to_string();
        pairs.push(("client_secret", secret.as_str()));
    }
    let resp = http
        .post(&saved.token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .body(form_body(&pairs))
        .send()
        .await
        .map_err(|e| format!("mcp oauth: refresh: {e}"))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        if text.contains("invalid_grant") {
            return Err("mcp oauth: refresh rejected (invalid_grant)".to_string());
        }
        return Err(http_error("refresh failed", &text));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("mcp oauth: refresh: {e}"))?;
    token_from_response(
        &v,
        &saved.token_endpoint,
        &saved.client_id,
        saved.client_secret.clone(),
        saved.refresh_token.clone(),
    )
}

// ---------------------------------------------------------------------------
// Browser + loopback callback
// ---------------------------------------------------------------------------

fn authorize_url(
    endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    resource: &str,
    challenge: &str,
    state: &str,
) -> String {
    let mut url = format!(
        "{endpoint}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        percent_encode(client_id),
        percent_encode(redirect_uri),
        percent_encode(challenge),
        percent_encode(state),
    );
    if !scope.is_empty() {
        url.push_str(&format!("&scope={}", percent_encode(scope)));
    }
    if !resource.is_empty() {
        url.push_str(&format!("&resource={}", percent_encode(resource)));
    }
    url
}

fn launch_browser(url: &str) {
    eprintln!("dex: authorize this MCP server in your browser:\n  {url}");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Parse the loopback `GET …?code=…&state=…` (or `?error=…`) request line.
/// The error code is preserved (`authorization denied (invalid_target): …`)
/// so callers can retry without `resource` when the AS rejects it.
fn parse_callback(req: &str) -> Result<(String, String), String> {
    let line = req.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err("unexpected browser callback".to_string());
    }
    let target = parts.next().ok_or("unexpected browser callback")?;
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = None;
    let mut state = String::new();
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(percent_decode(v)),
            "state" => state = percent_decode(v),
            "error" => {
                let desc = query
                    .split('&')
                    .find_map(|p| {
                        p.strip_prefix("error_description=")
                            .or_else(|| p.strip_prefix("error_uri="))
                    })
                    .map(percent_decode)
                    .unwrap_or_else(|| percent_decode(v));
                let code_name = percent_decode(v);
                if desc == code_name || desc.is_empty() {
                    return Err(format!("authorization denied ({code_name})"));
                }
                return Err(format!("authorization denied ({code_name}): {desc}"));
            }
            _ => {}
        }
    }
    Ok((code.ok_or("authorization server returned no code")?, state))
}

async fn wait_for_callback(
    listener: &tokio::net::TcpListener,
    state: &str,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let (mut sock, _) = tokio::time::timeout(
        Duration::from_secs(CALLBACK_TIMEOUT_SECS),
        listener.accept(),
    )
    .await
    .map_err(|_| {
        "timed out waiting for the browser — approve the request, then re-run `dex mcp login`"
            .to_string()
    })?
    .map_err(|e| format!("loopback accept: {e}"))?;
    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(30), sock.read(&mut buf))
        .await
        .map_err(|_| "browser callback read timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let reply = |status: &str, body: &str| {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    match parse_callback(&String::from_utf8_lossy(&buf[..n])) {
        Ok((code, got)) if got == state && !code.is_empty() => {
            let body = "dex: login complete — return to the terminal.";
            let _ = sock.write_all(reply("200 OK", body).as_bytes()).await;
            Ok(code)
        }
        Ok(_) => {
            let body = "dex: state mismatch — login aborted; re-run `dex mcp login`.";
            let _ = sock
                .write_all(reply("400 Bad Request", body).as_bytes())
                .await;
            Err("state mismatch — login aborted (possible CSRF)".to_string())
        }
        Err(e) => {
            let body = "dex: login failed — return to the terminal.";
            let _ = sock
                .write_all(reply("400 Bad Request", body).as_bytes())
                .await;
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Public flows
// ---------------------------------------------------------------------------

struct ChallengeProbe {
    challenged: bool,
    resource_metadata: Option<String>,
}

/// One unauthenticated `initialize` to capture the server's
/// `WWW-Authenticate` challenge without failing the turn.
async fn probe_challenge(
    http: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
) -> ChallengeProbe {
    let idle = ChallengeProbe {
        challenged: false,
        resource_metadata: None,
    };
    let mut req = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS));
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "dex", "version": env!("CARGO_PKG_VERSION")},
        },
    });
    let Ok(resp) = req.json(&body).send().await else {
        return idle;
    };
    // 401 always means auth; some servers answer 400 + `WWW-Authenticate`
    // instead (MCP spec drift) — treat that as challenged too, but only when
    // the header actually carries a Bearer challenge.
    let status = resp.status();
    let www = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if status != reqwest::StatusCode::UNAUTHORIZED
        && (status != reqwest::StatusCode::BAD_REQUEST || !is_bearer_challenge(&www))
    {
        return idle;
    }
    let resource_metadata = parse_resource_metadata_url(&www);
    ChallengeProbe {
        challenged: true,
        resource_metadata,
    }
}

/// Full interactive login: discover → client → browser → exchange → save →
/// reconnect. Prints the authorize URL (and tries to open it) for headless
/// terminals.
pub(crate) async fn login(server: &str) -> Result<String, String> {
    let server = super::sanitize_server_name(server);
    let configs = super::load_server_configs();
    let cfg = configs
        .get(&server)
        .cloned()
        .ok_or_else(|| format!("unknown mcp server '{server}' (see /mcp)"))?;
    let base = cfg.url.clone().ok_or_else(|| {
        format!("mcp server '{server}' is stdio: OAuth login is only for HTTP servers")
    })?;
    let http = crate::client::http::shared_async_client();

    let probe = probe_challenge(&http, &base, &cfg.headers).await;
    if !probe.challenged {
        return Ok(format!(
            "'{server}' answered without OAuth — nothing to log in to"
        ));
    }
    let rm_url = probe
        .resource_metadata
        .unwrap_or_else(|| well_known_resource_url(&base));
    let rm = fetch_resource_metadata(&http, &rm_url).await?;
    let issuer = rm
        .authorization_servers
        .first()
        .cloned()
        .ok_or("mcp oauth: resource metadata lists no authorization_servers".to_string())?;
    let scope = cfg
        .oauth_scope
        .clone()
        .unwrap_or_else(|| rm.scopes.join(" "));
    let asm = fetch_auth_server_metadata(&http, &issuer).await?;

    // Client: configured credentials win, then a saved registration for the
    // same token endpoint (no re-registration every login), else register.
    // The loopback binds first: its port is part of the redirect URI.
    let known = if let Some(id) = cfg.oauth_client_id.clone() {
        Some((id, cfg.oauth_client_secret.clone()))
    } else {
        load_token(&server)
            .filter(|s| !s.client_id.is_empty() && s.token_endpoint == asm.token_endpoint)
            .map(|s| (s.client_id, s.client_secret))
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("mcp oauth: loopback bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("mcp oauth: loopback addr: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (client_id, client_secret) = match known {
        Some(pair) => pair,
        None => {
            let endpoint = asm.registration_endpoint.as_deref().ok_or(
                "mcp oauth: server needs a pre-registered client (no registration_endpoint); set oauth_client_id/oauth_client_secret in config".to_string(),
            )?;
            let reg = register_client(&http, endpoint, &redirect_uri).await?;
            (reg.client_id, reg.client_secret)
        }
    };

    let (verifier, challenge) = pkce_pair();
    // `resource` (RFC8707) is sent first; AS servers that do not understand
    // it fail with `invalid_target` — then retry once without it.
    let mut resource = base.clone();
    let mut state = random_state();
    let code = loop {
        launch_browser(&authorize_url(
            &asm.authorization_endpoint,
            &client_id,
            &redirect_uri,
            &scope,
            &resource,
            &challenge,
            &state,
        ));
        match wait_for_callback(&listener, &state).await {
            Ok(code) => break code,
            Err(e) if e.contains("invalid_target") && !resource.is_empty() => {
                eprintln!("dex: server rejected `resource`; retrying without it…");
                resource.clear();
                state = random_state();
                continue;
            }
            Err(e) => return Err(e),
        }
    };
    let tok = exchange_code(
        &http,
        &asm.token_endpoint,
        &client_id,
        client_secret.as_deref(),
        &code,
        &redirect_uri,
        &verifier,
    )
    .await?;
    save_token(&server, &tok)?;
    match super::global_manager().reconnect(&server).await {
        Ok(n) => Ok(format!(
            "logged in to '{server}' ({n} tool{}); token stored, never logged",
            if n == 1 { "" } else { "s" }
        )),
        Err(e) => Ok(format!(
            "token saved for '{server}', but reconnect failed: {}",
            redact_secrets(&e)
        )),
    }
}

pub(crate) fn logout(server: &str) -> Result<String, String> {
    let server = super::sanitize_server_name(server);
    if clear_token(&server) {
        Ok(format!("logged out of '{server}'"))
    } else {
        Ok(format!("'{server}' was not logged in"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_vector() {
        // The RFC's printed Appendix B challenge drops the final char (42
        // chars cannot encode a 32-byte digest); the true value ends `-cM`.
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 43);
        assert_eq!(code_challenge(&verifier), challenge);
    }

    #[test]
    fn resource_metadata_url_parses() {
        assert_eq!(
            parse_resource_metadata_url(
                r#"Bearer resource_metadata="https://auth.example.com/.well-known/oauth-protected-resource""#
            )
            .as_deref(),
            Some("https://auth.example.com/.well-known/oauth-protected-resource")
        );
        // Extra params, odd spacing/case.
        assert_eq!(
            parse_resource_metadata_url(
                r#"Bearer error="invalid_token", Resource_Metadata = "https://a.example/x", scope="mcp""#
            )
            .as_deref(),
            Some("https://a.example/x")
        );
        assert_eq!(parse_resource_metadata_url("Bearer"), None);
        assert_eq!(parse_resource_metadata_url("Basic realm=\"x\""), None);
        // Non-URL values are ignored, not trusted.
        assert_eq!(
            parse_resource_metadata_url(r#"Bearer resource_metadata="not-a-url""#),
            None
        );
    }

    #[test]
    fn url_codec_roundtrips() {
        assert_eq!(
            percent_encode("http://127.0.0.1:9/cb?a=b c"),
            "http%3A%2F%2F127.0.0.1%3A9%2Fcb%3Fa%3Db%20c"
        );
        // RFC3986: `+` is literal, only `%XX` decodes (form `+`→space used
        // to corrupt codes in the callback query).
        assert_eq!(percent_decode("a%20b+c%2F"), "a b+c/");
        assert_eq!(
            percent_decode(percent_encode("code/x+y=z&").as_str()),
            "code/x+y=z&"
        );
    }

    #[test]
    fn bearer_challenge_is_scheme_not_substring() {
        assert!(is_bearer_challenge("Bearer"));
        assert!(is_bearer_challenge(
            r#"Bearer resource_metadata="https://a/x""#
        ));
        assert!(is_bearer_challenge(
            r#"Basic realm="x", Bearer error="invalid_token""#
        ));
        assert!(!is_bearer_challenge("Basic realm=\"x\""));
        assert!(!is_bearer_challenge("Bearertoken xyz"));
        assert!(!is_bearer_challenge(""));
    }

    #[test]
    fn https_required_except_loopback() {
        assert!(ensure_https_or_loopback("https://a.example/token", "t").is_ok());
        assert!(ensure_https_or_loopback("http://127.0.0.1:8080/x", "t").is_ok());
        assert!(ensure_https_or_loopback("http://localhost:9/x", "t").is_ok());
        assert!(ensure_https_or_loopback("http://a.example/token", "t").is_err());
        assert!(ensure_https_or_loopback("http://a.example/token", "issuer").is_err());
    }

    #[test]
    fn token_path_never_traverses() {
        let p = token_path("../../etc/passwd");
        assert_eq!(
            p.file_name().and_then(|n| n.to_str()),
            Some("______etc_passwd.json")
        );
        assert!(p.parent().is_some_and(|d| d.ends_with("dex/mcp")));
    }

    #[test]
    fn origin_splits() {
        assert_eq!(
            split_origin("https://h.example:8443/a/b"),
            ("https://h.example:8443".to_string(), "/a/b".to_string())
        );
        assert_eq!(
            split_origin("https://h.example"),
            ("https://h.example".to_string(), String::new())
        );
        // Query/fragment never leak into derived well-known URLs.
        assert_eq!(
            split_origin("https://h.example/mcp?x=1#frag"),
            ("https://h.example".to_string(), "/mcp".to_string())
        );
        assert_eq!(
            well_known_resource_url("https://h.example/mcp"),
            "https://h.example/.well-known/oauth-protected-resource/mcp"
        );
        assert_eq!(
            well_known_resource_url("https://h.example/mcp?x=1"),
            "https://h.example/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn callback_parses_code_and_rejects_denial() {
        let (code, state) =
            parse_callback("GET /callback?code=abc%201&state=s7 HTTP/1.1\r\nHost: x\r\n").unwrap();
        assert_eq!((code.as_str(), state.as_str()), ("abc 1", "s7"));
        // `+` in a code is literal (RFC3986), not a space.
        let (code, _) = parse_callback("GET /callback?code=a%2Bb+c&state=s HTTP/1.1\r\n").unwrap();
        assert_eq!(code, "a+b+c");
        // Error code is preserved so login can retry without `resource` on
        // `invalid_target`.
        let err =
            parse_callback("GET /callback?error=invalid_target&error_description=nope HTTP/1.1")
                .unwrap_err();
        assert!(err.contains("invalid_target"), "{err}");
        assert!(parse_callback("GET /callback?error=access_denied HTTP/1.1").is_err());
        assert!(parse_callback("POST /callback?code=x HTTP/1.1").is_err());
    }

    #[test]
    fn token_store_roundtrips_hermetic() {
        let _lock = super::super::TEST_ENV_LOCK.blocking_lock();
        let prev = std::env::var_os("XDG_DATA_HOME");
        let dir = std::env::temp_dir().join(format!("dex-oauth-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        let tok = OAuthToken {
            access_token: "a".to_string(),
            refresh_token: Some("r".to_string()),
            expires_at: Some(now_secs() + 3600),
            token_endpoint: "https://a.example/token".to_string(),
            client_id: "c".to_string(),
            client_secret: None,
        };
        assert!(save_token("roundtrip", &tok).is_ok());
        assert!(valid_token("roundtrip").is_some());
        assert!(status_line("roundtrip").contains("logged in"));
        assert!(clear_token("roundtrip"));
        assert!(load_token("roundtrip").is_none());
        assert!(status_line("roundtrip").contains("not logged in"));
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_tokens_are_not_valid() {
        let tok = OAuthToken {
            access_token: "a".to_string(),
            refresh_token: None,
            expires_at: Some(now_secs() - 10),
            token_endpoint: String::new(),
            client_id: String::new(),
            client_secret: None,
        };
        assert!(!tok.usable());
        assert!(!tok.refreshable());
    }
}
