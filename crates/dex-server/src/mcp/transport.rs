//! MCP JSON-RPC transports: stdio + Streamable HTTP.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Mutex;

use super::config::{McpServerConfig, DEFAULT_TIMEOUT_SECS};
use super::oauth;

// ---------------------------------------------------------------------------
// JSON-RPC + transports
// ---------------------------------------------------------------------------

pub(crate) fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// The `initialize` handshake payload: one source for the protocol version
/// and client info so the stdio and HTTP paths cannot drift apart.
/// (oauth.rs `probe_challenge` still builds its own envelope; that lane owns it.)
pub(crate) fn initialize_params() -> Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "dex", "version": env!("CARGO_PKG_VERSION")},
    })
}

/// Minimal transport surface the manager needs. Real transports speak
/// JSON-RPC 2.0; tests inject fakes. Boxed future (not `async_trait`, which
/// would add a dependency) keeps the trait object-safe on edition 2021.
pub(crate) trait McpTransport: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>;
}

// MCP transports share the process-wide async client: every call site sets
// its own per-request `.timeout()` (default `DEFAULT_TIMEOUT_SECS`), which
// governs — a client-level total here would additionally cap transports the
// user configured with a longer `timeout_secs`.

/// Newline-delimited JSON-RPC over a child process's stdio.
///
/// Requests are serialized through one lock pair (MCP calls are infrequent;
/// one in flight per server is plenty), so no response demux table is
/// needed — write a line, read the reply line, match the id.
pub(crate) struct StdioTransport {
    stdin: Mutex<tokio::process::ChildStdin>,
    stdout: Mutex<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    _child: Mutex<tokio::process::Child>,
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

impl StdioTransport {
    pub(crate) async fn spawn(cfg: &McpServerConfig) -> Result<Self, String> {
        let command = cfg.command.clone().ok_or("missing command")?;
        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&cfg.args).envs(&cfg.env);
        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        // Detach from the user's terminal; never inherit stdio.
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            // New session like tool children: no controlling tty.
            cmd.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {command}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let transport = Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(tokio::io::BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        };
        transport.initialize().await?;
        Ok(transport)
    }

    async fn initialize(&self) -> Result<(), String> {
        let result = self.request("initialize", initialize_params()).await?;
        let _ = result;
        // Fire-and-forget per spec; the server must not reply.
        let notif = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let mut stdin = self.stdin.lock().await;
        use tokio::io::AsyncWriteExt as _;
        let mut line = notif.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl McpTransport for StdioTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(async move {
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            // Lock ordering is fixed (stdin then stdout) so concurrent callers
            // serialize instead of interleaving lines on the pipe.
            let mut stdin = self.stdin.lock().await;
            let mut stdout = self.stdout.lock().await;
            let mut line = rpc_request(id, method, params).to_string();
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())?;
            // Match our id: servers may interleave notifications and
            // server->client requests at any time, and one stray line
            // would otherwise desync every later call by one reply.
            // Bounded so a chatty server can't spin us.
            for _ in 0..32 {
                let mut reply = String::new();
                tokio::time::timeout(
                    Duration::from_secs(DEFAULT_TIMEOUT_SECS),
                    stdout.read_line(&mut reply),
                )
                .await
                .map_err(|_| format!("mcp request {method} timed out"))?
                .map_err(|e| e.to_string())?;
                if reply.trim().is_empty() {
                    return Err("mcp server exited".to_string());
                }
                let v: Value = serde_json::from_str(reply.trim()).map_err(|e| e.to_string())?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    return extract_rpc_result(&v).ok_or("mcp: bad response".to_string());
                }
            }
            Err("mcp: too many interleaved messages".to_string())
        })
    }
}

/// Transport-level failure: `session_gone` means the server forgot our
/// Streamable HTTP session (restart) and the call is worth one re-handshake;
/// `unauthorized` means a 401 worth one token refresh before surfacing.
pub(crate) struct HttpError {
    msg: String,
    session_gone: bool,
    unauthorized: bool,
}

/// Streamable HTTP: POST JSON-RPC, accept `application/json` or SSE stream.
/// A stored OAuth token (`dex mcp login <server>`) is injected per request
/// unless the config already sets `authorization` — so login/logout take
/// effect without a reconnect.
pub(crate) struct HttpTransport {
    server: String,
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpTransport {
    pub fn new(server: &str, cfg: &McpServerConfig) -> Self {
        Self {
            server: server.to_string(),
            url: cfg.url.clone().unwrap_or_default(),
            headers: cfg.headers.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }
}

impl McpTransport for HttpTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(async move { self.request_inner(method, params).await })
    }
}

impl HttpTransport {
    async fn request_inner(&self, method: &str, params: Value) -> Result<Value, String> {
        match self.roundtrip(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if e.session_gone => {
                // Server restarted and forgot the session: drop it,
                // re-handshake per the Streamable HTTP spec, retry once.
                *self.session.lock().await = None;
                let _ = self.roundtrip("initialize", initialize_params()).await;
                self.roundtrip(method, params).await.map_err(|e| e.msg)
            }
            Err(e) if e.unauthorized => {
                // One silent refresh when the stored token is refreshable,
                // then a single retry. A dead refresh token (`invalid_grant`)
                // is dropped so later calls fail fast with the login hint.
                // Transient failures back off 60s so a down AS is not hammered
                // on every tool call.
                if let Some(saved) = oauth::load_token(&self.server) {
                    if saved.refreshable() && !oauth::refresh_backoff_active(&self.server) {
                        match oauth::refresh_access_token(&saved).await {
                            Ok(fresh) => {
                                let _ = oauth::save_token(&self.server, &fresh);
                                oauth::clear_refresh_backoff(&self.server);
                                return self.roundtrip(method, params).await.map_err(|e| e.msg);
                            }
                            Err(e) if e.contains("invalid_grant") => {
                                let _ = oauth::clear_token(&self.server);
                            }
                            Err(_) => {
                                oauth::note_refresh_failure(&self.server);
                            }
                        }
                    }
                }
                Err(e.msg)
            }
            Err(e) => Err(e.msg),
        }
    }

    async fn roundtrip(&self, method: &str, params: Value) -> Result<Value, HttpError> {
        let fail = |msg: String| HttpError {
            msg,
            session_gone: false,
            unauthorized: false,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = rpc_request(id, method, params);
        let mut req = crate::runtime::http::shared_async_client()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .timeout(self.timeout);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if !self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            if let Some(tok) = oauth::valid_token(&self.server) {
                req = req.header("authorization", format!("Bearer {}", tok.access_token));
            }
        }
        let had_session = self.session.lock().await.clone();
        if let Some(s) = had_session.clone() {
            req = req.header("mcp-session-id", s);
        }
        let mut resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().await = Some(s.to_string());
        }
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND && had_session.is_some() {
            return Err(HttpError {
                msg: format!("mcp http 404 (session expired): {method}"),
                session_gone: true,
                unauthorized: false,
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let bearer = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .is_some_and(oauth::is_bearer_challenge);
            let mut msg = format!("mcp http {status}: {method}");
            if bearer {
                msg.push_str(&format!(
                    " (OAuth required — run 'dex mcp login {}')",
                    self.server
                ));
            }
            return Err(HttpError {
                msg,
                session_gone: false,
                unauthorized: true,
            });
        }
        if !status.is_success() {
            return Err(fail(format!("mcp http {status}: {method}")));
        }
        // Stream-decode (§20): the server may hold the SSE stream open after
        // the result (server-initiated messages) — waiting for the full body
        // then meant hanging until the call timeout even though the reply
        // had arrived. Decode `data:` lines incrementally and return on the
        // envelope carrying this call's `id` with a result/error (same
        // "first wins" rule as the old full-body scan, now id-matched like
        // `StdioTransport` so a notification carrying `result` can't
        // early-exit). A non-SSE plain-JSON body falls back to a whole-body
        // parse at EOF. `buf` is capped: a malicious infinite `:keep-alive`
        // stream can't OOM before the result arrives.
        let mut buf: Vec<u8> = Vec::new();
        // The non-SSE fallback parses the whole body, but the scan above
        // drains every completed line out of `buf` (including non-`data:`
        // ones). Keep the raw bytes separately so a plain-JSON reply that
        // ends in a newline (very common: `json.NewEncoder`, `print`) is
        // still parseable at EOF instead of looking empty. Head-capped: an
        // RPC reply is small, and a bigger SSE body is never JSON anyway.
        let mut raw: Vec<u8> = Vec::new();
        const SSE_BUF_CAP: usize = 1024 * 1024;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if raw.len() < SSE_BUF_CAP {
                        let room = SSE_BUF_CAP - raw.len();
                        raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
                    }
                    buf.extend_from_slice(&chunk);
                    if let Some(r) = sse_scan_buffered_id(&mut buf, id) {
                        // Early exit drops `resp`, closing the stream: the
                        // server's post-result broadcasts lose one listener.
                        // Correctness first (was: hang to timeout); the next
                        // call re-establishes the stream.
                        return Ok(r);
                    }
                    // After the scan `buf` holds only the unterminated tail:
                    // progress/`:keep-alive` spam can't OOM the call, and a
                    // single line bigger than the cap could never be parsed,
                    // so fail loudly instead of silently dropping it.
                    if buf.len() > SSE_BUF_CAP {
                        return Err(fail(format!(
                            "mcp http response line exceeded the {SSE_BUF_CAP}-byte framing cap"
                        )));
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(fail(e.to_string())),
            }
        }
        // Trailing line without a newline (SSE), then the plain-JSON
        // fallback for non-SSE responses — parsed from `raw`, since the
        // scan consumed the completed lines above.
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf);
            if let Some(r) = sse_result_id(&line, id) {
                return Ok(r);
            }
        }
        let text = String::from_utf8_lossy(&raw);
        if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
            if v.get("id").is_none_or(|v| v.as_u64() == Some(id)) {
                if let Some(r) = extract_rpc_result(&v) {
                    return Ok(r);
                }
            }
        }
        Err(fail("mcp: no result in http response".to_string()))
    }
}

/// One SSE `data:` line → RPC result/error for `id`, if it carries one.
/// Progress notifications (no `result`/`error`, or a different `id`) return
/// `None` so the scan continues. An envelope without an `id` is NEVER this
/// call's result — under concurrent calls on one transport an id-less
/// broadcast notification carrying `result` would otherwise be stolen and
/// misattributed to whoever scans first. Servers that omit `id` are served
/// by the non-multiplexed stdio path, not here.
pub(crate) fn sse_result_id(line: &str, id: u64) -> Option<Value> {
    let data = line.trim().strip_prefix("data:").unwrap_or("").trim();
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(data).ok()?;
    if v.get("id").and_then(|v| v.as_u64()) != Some(id) {
        return None;
    }
    extract_rpc_result(&v)
}

/// Scan newly completed lines in `buf` for this call's result (§20): a
/// `data:` line split across chunks stays buffered until its newline
/// arrives. Completed non-result lines are dropped; the partial tail stays.
pub(crate) fn sse_scan_buffered_id(buf: &mut Vec<u8>, id: u64) -> Option<Value> {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        if let Some(r) = sse_result_id(&String::from_utf8_lossy(&line), id) {
            return Some(r);
        }
    }
    None
}

pub(crate) fn extract_rpc_result(v: &Value) -> Option<Value> {
    if let Some(err) = v.get("error") {
        return Some(serde_json::json!({"__mcp_error": err}));
    }
    v.get("result").cloned()
}

/// The [`extract_rpc_result`] error sentinel: the server's message when the
/// reply was a JSON-RPC error. One check so a reply path can't dodge it.
pub(crate) fn rpc_error(v: &Value) -> Option<String> {
    v.get("__mcp_error").map(|e| e.to_string())
}
