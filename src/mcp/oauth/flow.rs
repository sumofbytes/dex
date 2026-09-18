use super::super::redact::redact_secrets;
use super::storage::clear_token;
use super::storage::load_token;
use super::storage::now_secs;
use super::storage::save_token;
use super::storage::OAuthToken;
use super::storage::CALLBACK_TIMEOUT_SECS;
use super::storage::DISCOVERY_TIMEOUT_SECS;
use std::collections::BTreeMap;
use std::time::Duration;

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
pub(crate) fn ensure_https_or_loopback(url: &str, context: &str) -> Result<(), String> {
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
pub(crate) fn well_known_resource_url(base: &str) -> String {
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
pub(crate) struct ResourceMetadata {
    pub(crate) authorization_servers: Vec<String>,
    pub(crate) scopes: Vec<String>,
}

pub(crate) async fn fetch_resource_metadata(
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
pub(crate) struct AuthServerMetadata {
    pub(crate) authorization_endpoint: String,
    pub(crate) token_endpoint: String,
    pub(crate) registration_endpoint: Option<String>,
}

pub(crate) async fn fetch_auth_server_metadata(
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

pub(crate) struct ClientRegistration {
    pub(crate) client_id: String,
    pub(crate) client_secret: Option<String>,
}

pub(crate) async fn register_client(
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

pub(crate) fn token_from_response(
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

/// Failure from `post_token_form`: a non-2xx token reply (raw body, so the
/// caller can sniff grant errors like `invalid_grant`) or a transport/decode
/// failure (already formatted with the caller's context).
enum TokenError {
    Status(String),
    Other(String),
}

/// POST a form-encoded grant request to a token endpoint and parse the JSON
/// reply. Shared by the authorization-code exchange and the refresh grant.
async fn post_token_form(
    http: &reqwest::Client,
    token_endpoint: &str,
    pairs: &[(&str, &str)],
    context: &str,
) -> Result<serde_json::Value, TokenError> {
    ensure_https_or_loopback(token_endpoint, "token_endpoint").map_err(TokenError::Other)?;
    let resp = http
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .timeout(Duration::from_secs(DISCOVERY_TIMEOUT_SECS))
        .body(form_body(pairs))
        .send()
        .await
        .map_err(|e| TokenError::Other(format!("mcp oauth: {context}: {e}")))?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(TokenError::Status(text));
    }
    resp.json()
        .await
        .map_err(|e| TokenError::Other(format!("mcp oauth: {context}: {e}")))
}

pub(crate) async fn exchange_code(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthToken, String> {
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
    let v = match post_token_form(http, token_endpoint, &pairs, "code exchange").await {
        Ok(v) => v,
        Err(TokenError::Other(e)) => return Err(e),
        Err(TokenError::Status(text)) => return Err(http_error("code exchange failed", &text)),
    };
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
    let v = match post_token_form(&http, &saved.token_endpoint, &pairs, "refresh").await {
        Ok(v) => v,
        Err(TokenError::Other(e)) => return Err(e),
        Err(TokenError::Status(text)) => {
            if text.contains("invalid_grant") {
                return Err("mcp oauth: refresh rejected (invalid_grant)".to_string());
            }
            return Err(http_error("refresh failed", &text));
        }
    };
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

pub(crate) fn authorize_url(
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
pub(crate) fn parse_callback(req: &str) -> Result<(String, String), String> {
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

pub(crate) async fn wait_for_callback(
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

pub(crate) struct ChallengeProbe {
    pub(crate) challenged: bool,
    pub(crate) resource_metadata: Option<String>,
}

/// One unauthenticated `initialize` to capture the server's
/// `WWW-Authenticate` challenge without failing the turn.
pub(crate) async fn probe_challenge(
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
    let server = super::super::sanitize_server_name(server);
    let configs = super::super::load_server_configs();
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

    // The loopback binds first: its port is part of the redirect URI.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("mcp oauth: loopback bind: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("mcp oauth: loopback addr: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (client_id, client_secret) =
        resolve_client(&cfg, &server, &asm, &redirect_uri, &http).await?;
    let client = ResolvedClient {
        id: client_id,
        secret: client_secret,
    };
    let tok = authorize_and_exchange(
        &http,
        &asm,
        &client,
        &redirect_uri,
        &scope,
        &base,
        &listener,
    )
    .await?;
    save_token(&server, &tok)?;
    match super::super::global_manager().reconnect(&server).await {
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

/// OAuth client credentials resolved for one login attempt.
struct ResolvedClient {
    pub(crate) id: String,
    pub(crate) secret: Option<String>,
}

/// Pick OAuth client credentials: configured ones win, then a saved
/// registration for the same token endpoint (no re-registration every
/// login), else dynamic registration against `asm.registration_endpoint`.
async fn resolve_client(
    cfg: &super::super::McpServerConfig,
    server: &str,
    asm: &AuthServerMetadata,
    redirect_uri: &str,
    http: &reqwest::Client,
) -> Result<(String, Option<String>), String> {
    let known = if let Some(id) = cfg.oauth_client_id.clone() {
        Some((id, cfg.oauth_client_secret.clone()))
    } else {
        load_token(server)
            .filter(|s| !s.client_id.is_empty() && s.token_endpoint == asm.token_endpoint)
            .map(|s| (s.client_id, s.client_secret))
    };
    match known {
        Some(pair) => Ok(pair),
        None => {
            let endpoint = asm.registration_endpoint.as_deref().ok_or(
                "mcp oauth: server needs a pre-registered client (no registration_endpoint); set oauth_client_id/oauth_client_secret in config".to_string(),
            )?;
            let reg = register_client(http, endpoint, redirect_uri).await?;
            Ok((reg.client_id, reg.client_secret))
        }
    }
}

/// Open the browser, wait for the loopback callback (retrying once without
/// `resource` if the AS rejects it with `invalid_target`), then exchange the
/// code for a token.
async fn authorize_and_exchange(
    http: &reqwest::Client,
    asm: &AuthServerMetadata,
    client: &ResolvedClient,
    redirect_uri: &str,
    scope: &str,
    resource_base: &str,
    listener: &tokio::net::TcpListener,
) -> Result<OAuthToken, String> {
    let (verifier, challenge) = pkce_pair();
    // `resource` (RFC8707) is sent first; AS servers that do not understand
    // it fail with `invalid_target` — then retry once without it.
    let mut resource = resource_base.to_string();
    let mut state = random_state();
    let code = loop {
        launch_browser(&authorize_url(
            &asm.authorization_endpoint,
            &client.id,
            redirect_uri,
            scope,
            &resource,
            &challenge,
            &state,
        ));
        match wait_for_callback(listener, &state).await {
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
    exchange_code(
        http,
        &asm.token_endpoint,
        &client.id,
        client.secret.as_deref(),
        &code,
        redirect_uri,
        &verifier,
    )
    .await
}

pub(crate) fn logout(server: &str) -> Result<String, String> {
    let server = super::super::sanitize_server_name(server);
    if clear_token(&server) {
        Ok(format!("logged out of '{server}'"))
    } else {
        Ok(format!("'{server}' was not logged in"))
    }
}
