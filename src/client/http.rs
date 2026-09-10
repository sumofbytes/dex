use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::protocol::{
    ApprovalDecision, ApprovalResponse, ChatRequest, CreateSessionRequest, CreateSessionResponse,
    DaemonInfo, EventsResponse, FollowupRequest, GitInfo, LoadSkillRequest, LoadSkillResponse,
    ReattachResponse, SessionInfo, ShellRequest, ShellResponse, SkillInfo, SteerRequest,
    StreamEnvelope, StreamEvent,
};

/// Per-request overrides forwarded to the daemon with a chat turn.
#[derive(Debug, Clone, Default)]
pub(crate) struct ChatOptions {
    pub(crate) skill_dirs: Vec<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) permission: Option<String>,
    pub(crate) headers: Option<std::collections::BTreeMap<String, String>>,
    pub(crate) plan: Option<String>,
    /// P10: replay-safe submission key; the daemon dedups identical keys within 60s.
    pub(crate) idempotency_key: Option<String>,
}

/// Shared tokio runtime for sync callers (one-shot CLI, repl, `dex run`).
/// Small (2 workers): only bridges sync entry points to async I/O.
static SHARED_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn shared_rt() -> &'static tokio::runtime::Runtime {
    SHARED_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("shared runtime")
    })
}

/// Block on an async future from sync code (CLI/one-shot/`dex run` paths).
/// No async CLI plumbing needed per plan §5.
pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    shared_rt().block_on(fut)
}

/// Spawn an async task from sync code (TUI workers) onto the shared runtime.
/// Detached on drop (like threads), so failed launches don't wait.
pub(crate) fn spawn_task<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    shared_rt().spawn(fut)
}

/// HTTP client for communicating with the dex daemon.
///
/// Async-first: one shared `reqwest::Client` (TLS + pool init once, clones
/// are atomic bumps). Sync methods remain for one-shot CLI + repl; they
/// `block_on` the async implementations so behavior is identical.
#[derive(Clone)]
pub(crate) struct DaemonClient {
    base_url: String,
    http: reqwest::Client,
    /// Bearer token for the daemon API, when it requires one: `DEX_DAEMON_TOKEN`
    /// wins, else the token file the daemon publishes for non-loopback binds.
    /// Loopback daemons run without a token, so a missing file is not an error.
    token: Option<String>,
}

/// Credential a client presents to a dex daemon. Same resolution as
/// `daemon::daemon_token_file`; kept client-side so `dex connect` and the
/// in-process TUI need no setup beyond copying the file's contents.
fn client_daemon_token() -> Option<String> {
    if let Ok(t) = std::env::var("DEX_DAEMON_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    crate::daemon::daemon_token_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Process-wide shared async client. `Client::new()` initializes a TLS
/// backend + connection pool (~tens of ms); the TUI used to build one per
/// `DaemonClient` plus one per display config. Clones are an atomic bump.
///
/// Timeout note: 10s connect / 300s total. `wait_until_ready` overrides
/// to 2s per poll.
/// `User-Agent` sent on every outbound HTTP request (provider generations,
/// catalog fetches, daemon/TUI traffic). Both reference agents identify
/// themselves on the wire; a missing UA reads as bot traffic to some
/// gateways. A per-request `User-Agent` header still overrides this default.
pub(crate) const USER_AGENT: &str = concat!("dex/", env!("CARGO_PKG_VERSION"));
static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub(crate) fn shared_async_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Process-wide shared client for long-lived SSE streams (TUI↔daemon chat,
/// daemon↔provider generations). Connect timeout only: reqwest's total
/// `.timeout()` covers the whole streaming body, so any value here kills
/// turns/generations that run longer than it (`error decoding response body`
/// at exactly N seconds). A stalled stream ends via server keep-alive/EOF
/// or user cancel instead.
static STREAMING_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Socket keepalive interval for long-lived SSE streams. One place so the
/// shared streaming client and the explicit-timeout client stay in sync.
pub(crate) const TCP_KEEPALIVE_SECS: u64 = 60;

pub(crate) fn shared_streaming_client() -> reqwest::Client {
    STREAMING_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                // Socket-level keepalives: periodic probes let a silently
                // dropped connection (dead middlebox, hung peer) surface at
                // the TCP layer instead of parking indefinitely. Detection
                // still takes a few missed probes — the app-level idle
                // watchdog bounds application silence. Healthy-but-slow
                // providers are unaffected — keepalive ACKs carry no body
                // bytes, so the SSE idle timer still governs silence.
                .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Byte framing for SSE `data:` lines. Pure buffer logic (no I/O, no runtime)
/// shared by `ChatStream`; split lines across TCP chunks are reassembled,
/// keep-alives and junk skipped, trailing partial line held for `finish()`.
/// Also tracks the highest envelope `seq` seen, so a caller can resume after
/// a dropped connection by replaying the journal from that cursor.
#[derive(Default)]
struct SseFramer {
    buf: Vec<u8>,
    pending: std::collections::VecDeque<StreamEvent>,
    last_seq: u64,
}

impl SseFramer {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(env) = Self::parse_envelope(&String::from_utf8_lossy(&line)) {
                self.last_seq = self.last_seq.max(env.seq);
                self.pending.push_back(env.event);
            }
        }
    }

    fn finish(&mut self) {
        // Trailing buffered line without newline (terminal envelope).
        if !self.buf.is_empty() {
            let tail = String::from_utf8_lossy(&self.buf).into_owned();
            self.buf.clear();
            if let Some(env) = Self::parse_envelope(&tail) {
                self.last_seq = self.last_seq.max(env.seq);
                self.pending.push_back(env.event);
            }
        }
    }

    /// Parse one raw SSE line into a full envelope (event + journal seq).
    /// Pure (no I/O, no runtime) so it is safe from any context and trivial
    /// to unit-test. Returns `None` for keep-alives (`ping`), blanks,
    /// non-`data:` lines, and unparsable payloads.
    fn parse_envelope(line: &str) -> Option<StreamEnvelope> {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            return None;
        }
        let data = trimmed.strip_prefix("data:")?.trim();
        if data.is_empty() || data == "ping" {
            return None;
        }
        serde_json::from_str::<StreamEnvelope>(data).ok()
    }
}

/// One in-flight SSE response with its framing buffer. `next_event()` returns
/// `None` on clean EOF and `Some(Err)` on transport failure, so callers can
/// distinguish "turn completed" from "connection died".
pub(crate) struct ChatStream {
    response: reqwest::Response,
    framer: SseFramer,
    eof: bool,
}

impl ChatStream {
    /// Highest journal seq delivered so far — the resume cursor for a
    /// reattach after the connection drops mid-turn.
    pub(crate) fn last_seq(&self) -> u64 {
        self.framer.last_seq
    }

    pub(crate) async fn next_event(&mut self) -> Option<Result<StreamEvent, String>> {
        loop {
            if let Some(event) = self.framer.pending.pop_front() {
                return Some(Ok(event));
            }
            if self.eof {
                return None;
            }
            match self.response.chunk().await {
                Ok(Some(bytes)) => self.framer.push_bytes(&bytes),
                Ok(None) => {
                    self.eof = true;
                    self.framer.finish();
                }
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }
}

/// Auth lines from a `GET /api/mcp` body: the non-null `auth` fields in
/// server order. `null` means stdio (no login possible) — skipped, so
/// `/mcp` never nags about servers that can't take a login.
pub fn mcp_auth_lines(body: &serde_json::Value) -> Vec<String> {
    body["servers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v["auth"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

impl DaemonClient {
    pub fn new(base_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            http: shared_async_client(),
            token: client_daemon_token(),
        })
    }

    /// Wait until `GET /health` answers (the daemon may still be booting).
    /// Returns an error if it never becomes ready within `timeout`.
    /// Async: `sleep().await` instead of `thread::sleep(50ms)` (S6).
    pub async fn wait_until_ready_async(
        &self,
        timeout: Duration,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .http
                .get(format!("{}/health", self.base_url))
                .timeout(Duration::from_secs(2))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(()),
                _ if Instant::now() >= deadline => {
                    return Err(format!("daemon at {} did not become ready", self.base_url).into())
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    pub fn wait_until_ready(&self, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.wait_until_ready_async(timeout))
    }

    /// Fetch the daemon's runtime info (model, provider, workspace, git).
    pub async fn get_config_async(&self) -> Result<DaemonInfo, Box<dyn std::error::Error>> {
        let info = self
            .http
            .get(format!("{}/api/config", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<DaemonInfo>()
            .await?;
        Ok(info)
    }

    pub fn get_config(&self) -> Result<DaemonInfo, Box<dyn std::error::Error>> {
        block_on(self.get_config_async())
    }

    /// Poll the daemon workspace's branch/dirty for the status footer.
    /// Short timeout + best-effort: a slow daemon must not hitch the TUI,
    /// the next interval simply retries.
    pub async fn get_git_async(&self) -> Result<GitInfo, Box<dyn std::error::Error + Send + Sync>> {
        let info = self
            .http
            .get(format!("{}/api/git", self.base_url))
            .headers(self.api_headers())
            .timeout(Duration::from_secs(2))
            .send()
            .await?
            .error_for_status()?
            .json::<GitInfo>()
            .await?;
        Ok(info)
    }

    pub fn get_git(&self) -> Result<GitInfo, Box<dyn std::error::Error>> {
        block_on(self.get_git_async()).map_err(|e| -> Box<dyn std::error::Error> { e })
    }

    /// Versioned-protocol headers (P10): declared on every `/api/*` request
    /// so the daemon can reject a mismatch. Health checks stay header-free.
    fn api_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(value) = reqwest::header::HeaderValue::from_str("application/vnd.dex.v1+json") {
            headers.insert(reqwest::header::ACCEPT, value);
        }
        if let Ok(value) = reqwest::header::HeaderValue::from_str("1") {
            headers.insert("x-dex-protocol", value);
        }
        if let Some(token) = &self.token {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
        headers
    }

    /// Create a new session on the daemon.
    pub async fn create_session_async(
        &self,
        cwd: &str,
        name: Option<&str>,
    ) -> Result<CreateSessionResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!("{}/api/sessions", self.base_url))
            .headers(self.api_headers())
            .json(&CreateSessionRequest {
                cwd: cwd.to_string(),
                name: name.map(String::from),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<CreateSessionResponse>()
            .await?;
        Ok(resp)
    }

    pub fn create_session(
        &self,
        cwd: &str,
        name: Option<&str>,
    ) -> Result<CreateSessionResponse, Box<dyn std::error::Error>> {
        block_on(self.create_session_async(cwd, name))
    }

    /// List all sessions on the daemon (P10: disk-backed, survives restarts).
    pub async fn list_sessions_async(
        &self,
    ) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/sessions", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let sessions = resp["sessions"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        Ok(sessions)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        block_on(self.list_sessions_async())
    }

    /// Parse one raw SSE line into a `StreamEvent`. Pure (no I/O, no runtime
    /// interaction) so it is safe to call from any context and trivial to
    /// unit-test. Returns `None` for keep-alives (`ping`), blanks, non-`data:`
    /// lines, and unparsable payloads (skipped, matching prior behavior).
    pub(crate) fn parse_sse_line(line: &str) -> Option<StreamEvent> {
        Some(SseFramer::parse_envelope(line)?.event)
    }

    /// Async-native SSE transport: POSTs the chat request and yields parsed
    /// `StreamEvent`s via `next_event().await`. Carries no approval
    /// side-effects — the caller forwards events and POSTs decisions itself
    /// (the TUI worker maps UI decisions and calls `approve_async`). Single
    /// `.chunk().await` read loop, no threads, backpressured by the caller.
    pub async fn chat_stream(
        &self,
        session_id: &str,
        prompt: &str,
        options: ChatOptions,
    ) -> Result<ChatStream, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/sessions/{}/chat", self.base_url, session_id);
        // The chat SSE stream lives as long as the turn (often minutes);
        // the shared client's total timeout would kill it mid-turn.
        let mut builder = shared_streaming_client()
            .post(&url)
            .headers(self.api_headers());
        if let Some(key) = &options.idempotency_key {
            builder = builder.header("idempotency-key", key);
        }
        let response = builder
            .json(&ChatRequest {
                prompt: prompt.to_string(),
                skill_dirs: options.skill_dirs,
                base_url: options.base_url,
                model: options.model,
                permission: options.permission,
                headers: options.headers,
                plan: options.plan,
            })
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        Ok(ChatStream {
            response,
            framer: SseFramer::default(),
            eof: false,
        })
    }

    /// Async SSE chat: drives `chat_async` stream, invoking `on_event`
    /// synchronously per event. Approval decisions `await` the daemon
    /// confirm POST instead of blocking a thread (S6).
    pub async fn chat_async(
        &self,
        session_id: &str,
        prompt: &str,
        options: ChatOptions,
        on_event: &mut dyn FnMut(StreamEvent) -> Option<ApprovalDecision>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Sync-callback path (one-shot CLI, repl, e2e tests): the callback
        // never touches the runtime, so driving the shared `ChatStream`
        // framing here keeps one parser for both callers.
        let mut stream = self
            .chat_stream(session_id, prompt, options)
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        while let Some(item) = stream.next_event().await {
            let event = item.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            if let StreamEvent::ApprovalRequired { ref request_id, .. } = &event {
                let request_id = request_id.clone();
                let decision = on_event(event).unwrap_or(ApprovalDecision::Deny);
                if let Err(e) = self.approve_async(session_id, &request_id, decision).await {
                    crate::llm::client::provider_log(
                        "approval_delivery_failed",
                        &crate::llm::client::error_chain_message(&*e),
                    );
                }
                continue;
            }
            on_event(event);
        }
        Ok(())
    }

    /// Submit a chat prompt and process `StreamEvent`s as they arrive.
    ///
    /// `on_event` is called synchronously for every event in stream order.
    /// When an `ApprovalRequired` event is received, its return value is the
    /// user's decision; a `None` default denies the tool.
    ///
    /// Returns only after the stream closes (turn complete/failed/disconnect).
    pub fn chat(
        &self,
        session_id: &str,
        prompt: &str,
        options: ChatOptions,
        on_event: &mut dyn FnMut(StreamEvent) -> Option<ApprovalDecision>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.chat_async(session_id, prompt, options, on_event))
    }

    /// Send an approval decision for a pending tool execution.
    pub async fn approve_async(
        &self,
        session_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/approve",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&ApprovalResponse {
                request_id: request_id.to_string(),
                decision,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn approve(
        &self,
        session_id: &str,
        request_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.approve_async(session_id, request_id, decision))
    }

    /// Cancel the active turn for a session.
    pub async fn cancel_async(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/cancel",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn cancel(&self, session_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.cancel_async(session_id))
    }

    /// List skills discovered on the daemon (its workspace).
    pub async fn list_skills_async(
        &self,
    ) -> Result<Vec<SkillInfo>, Box<dyn std::error::Error + Send + Sync>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/skills", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let skills = resp["skills"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(skills)
    }

    pub fn list_skills(&self) -> Result<Vec<SkillInfo>, Box<dyn std::error::Error>> {
        block_on(self.list_skills_async()).map_err(|e| -> Box<dyn std::error::Error> { e })
    }

    /// MCP server status from the daemon (`GET /api/mcp`): per-server
    /// name/state/tool-count/error/auth plus the schema-cap drop count.
    /// Returned raw — the caller renders it with `render_mcp_panel` and
    /// prints `mcp_auth_lines` after it.
    pub async fn mcp_status_async(
        &self,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let resp: serde_json::Value = self
            .http
            .get(format!("{}/api/mcp", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    pub fn mcp_status(&self) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        block_on(self.mcp_status_async()).map_err(|e| -> Box<dyn std::error::Error> { e })
    }

    /// Load a skill by name into the daemon's session history.
    pub async fn load_skill_async(
        &self,
        session_id: &str,
        name: &str,
        skill_dirs: &[String],
    ) -> Result<LoadSkillResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/skill",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&LoadSkillRequest {
                name: name.to_string(),
                skill_dirs: skill_dirs.to_vec(),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<LoadSkillResponse>()
            .await?;
        Ok(resp)
    }

    pub fn load_skill(
        &self,
        session_id: &str,
        name: &str,
        skill_dirs: &[String],
    ) -> Result<LoadSkillResponse, Box<dyn std::error::Error>> {
        block_on(self.load_skill_async(session_id, name, skill_dirs))
    }

    /// Re-register a persisted session and get the replay cursor (P10).
    pub async fn reattach_async(
        &self,
        session_id: &str,
    ) -> Result<ReattachResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/reattach",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<ReattachResponse>()
            .await?;
        Ok(resp)
    }

    pub fn reattach(
        &self,
        session_id: &str,
    ) -> Result<ReattachResponse, Box<dyn std::error::Error>> {
        block_on(self.reattach_async(session_id))
    }

    /// Replay journaled stream events after `since` (P10).
    pub async fn events_async(
        &self,
        session_id: &str,
        since: u64,
    ) -> Result<EventsResponse, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .get(format!(
                "{}/api/sessions/{}/events?since={}",
                self.base_url, session_id, since
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<EventsResponse>()
            .await?;
        Ok(resp)
    }

    pub fn events(
        &self,
        session_id: &str,
        since: u64,
    ) -> Result<EventsResponse, Box<dyn std::error::Error>> {
        block_on(self.events_async(session_id, since))
    }

    /// Fetch the redacted trace rows for a session (P9).
    pub async fn trace_async(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(format!(
                "{}/api/sessions/{}/trace",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp["trace"].as_array().cloned().unwrap_or_default())
    }

    pub fn trace(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        block_on(self.trace_async(session_id))
    }

    /// Undo the last recorded change; Ok(true) when an undo happened.
    pub async fn undo_async(&self, session_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/undo",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        Ok(resp["status"].as_str() == Some("ok"))
    }

    pub fn undo(&self, session_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        block_on(self.undo_async(session_id))
    }

    /// Record a `waived` verification disposition with a reason (P9).
    pub async fn waive_async(
        &self,
        session_id: &str,
        reason: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/waive",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&serde_json::json!({ "reason": reason }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn waive(&self, session_id: &str, reason: &str) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.waive_async(session_id, reason))
    }

    /// Rename a session on the daemon.
    pub async fn rename_session_async(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/name",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn rename_session(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.rename_session_async(session_id, name))
    }

    /// Run a shell command directly on the daemon (`!`/`!!` prefix in the
    /// TUI). `exclude_from_context` (`!!`): saved to history and shown,
    /// but never sent to the LLM.
    pub async fn shell_async(
        &self,
        session_id: &str,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<ShellResponse, Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .http
            .post(format!(
                "{}/api/sessions/{}/shell",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&ShellRequest {
                command: command.to_string(),
                exclude_from_context,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<ShellResponse>()
            .await?;
        Ok(resp)
    }

    pub fn shell(
        &self,
        session_id: &str,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<ShellResponse, Box<dyn std::error::Error>> {
        block_on(self.shell_async(session_id, command, exclude_from_context))
            .map_err(|e| e.to_string().into())
    }

    /// Enqueue a steering message into an active turn (mid-turn injection).
    pub async fn steer_async(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/steer",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&SteerRequest {
                content: content.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn steer(&self, session_id: &str, content: &str) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.steer_async(session_id, content))
    }

    /// Enqueue a follow-up message (runs as a chained turn after the current
    /// one completes, Alt+Enter in the TUI).
    pub async fn followup_async(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(format!(
                "{}/api/sessions/{}/followup",
                self.base_url, session_id
            ))
            .headers(self.api_headers())
            .json(&FollowupRequest {
                content: content.to_string(),
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn followup(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.followup_async(session_id, content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_auth_lines_skips_null_stdio() {
        let body = serde_json::json!({"servers": [
            {"name": "gh", "auth": "gh: logged in"},
            {"name": "local", "auth": null},
            {"name": "nope"},
        ]});
        assert_eq!(mcp_auth_lines(&body), vec!["gh: logged in".to_string()]);
        assert!(mcp_auth_lines(&serde_json::json!({})).is_empty());
    }

    #[tokio::test]
    async fn wait_until_ready_async_errors_after_timeout() {
        // TDD Phase 5: async sleep, not thread::sleep — unreachable daemon
        // must error within the deadline without parking a thread.
        let client = DaemonClient::new("http://127.0.0.1:9").unwrap();
        let start = Instant::now();
        let err = client
            .wait_until_ready_async(Duration::from_millis(120))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not become ready"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn parse_sse_line_skips_keepalives_and_junk() {
        assert!(DaemonClient::parse_sse_line("").is_none());
        assert!(DaemonClient::parse_sse_line("\n").is_none());
        assert!(DaemonClient::parse_sse_line(": comment").is_none());
        assert!(DaemonClient::parse_sse_line("event: message").is_none());
        assert!(DaemonClient::parse_sse_line("data:").is_none());
        assert!(DaemonClient::parse_sse_line("data: ping").is_none());
        assert!(DaemonClient::parse_sse_line("data: not-json").is_none());
        let env = StreamEnvelope {
            seq: 1,
            event: StreamEvent::AssistantText("hi".to_string()),
        };
        let line = format!("data: {}", serde_json::to_string(&env).unwrap());
        assert!(matches!(
            DaemonClient::parse_sse_line(&line),
            Some(StreamEvent::AssistantText(t)) if t == "hi"
        ));
    }

    /// The framer tracks the highest envelope seq so a reconnect can resume
    /// the journal from exactly where the stream died.
    #[test]
    fn sse_framer_tracks_the_replay_cursor() {
        let env1 = StreamEnvelope {
            seq: 3,
            event: StreamEvent::AssistantText("a".to_string()),
        };
        let env2 = StreamEnvelope {
            seq: 7,
            event: StreamEvent::System("b".to_string()),
        };
        let mut framer = SseFramer::default();
        framer.push_bytes(format!("data: {}\n", serde_json::to_string(&env1).unwrap()).as_bytes());
        framer.push_bytes(format!("data: {}\n", serde_json::to_string(&env2).unwrap()).as_bytes());
        framer.push_bytes(b"data: ping\n");
        framer.push_bytes(b"data: not-json\n");
        assert_eq!(framer.last_seq, 7);
        assert_eq!(framer.pending.len(), 2);
        framer.finish();
        assert_eq!(framer.last_seq, 7);
    }

    #[test]
    fn sse_framer_reassembles_split_lines_and_trailing_terminal() {
        // One envelope split across TCP chunks reassembles; keep-alives and
        // junk around it are skipped; the terminal envelope without a
        // trailing newline surfaces via `finish()`.
        let assistant = StreamEnvelope {
            seq: 1,
            event: StreamEvent::AssistantText("hello".to_string()),
        };
        let complete = StreamEnvelope {
            seq: 2,
            event: StreamEvent::TurnComplete {
                response: "done".to_string(),
                usage: None,
                cached: None,
            },
        };
        let first = format!("data: {}\n\n", serde_json::to_string(&assistant).unwrap());
        let terminal = format!("data: {}", serde_json::to_string(&complete).unwrap());
        let mut framer = SseFramer::default();
        // Split mid-payload: nothing complete yet (chunks are contiguous on
        // the wire, so the halves arrive back-to-back).
        let (head, tail) = first.as_bytes().split_at(first.len() / 2);
        framer.push_bytes(head);
        assert!(framer.pending.is_empty());
        framer.push_bytes(tail);
        // Junk around real lines is skipped.
        framer.push_bytes(b"data: ping\n\n\ndata: bogus{\n\n");
        assert!(matches!(
            framer.pending.pop_front(),
            Some(StreamEvent::AssistantText(t)) if t == "hello"
        ));
        assert!(framer.pending.is_empty());
        // Trailing terminal without newline is held until `finish()`.
        framer.push_bytes(terminal.as_bytes());
        assert!(framer.pending.is_empty());
        framer.finish();
        assert!(matches!(
            framer.pending.pop_front(),
            Some(StreamEvent::TurnComplete { response, .. }) if response == "done"
        ));
    }

    fn sse_data(event: &StreamEvent, seq: u64) -> String {
        let env = StreamEnvelope {
            seq,
            event: event.clone(),
        };
        format!("data: {}\n\n", serde_json::to_string(&env).unwrap())
    }

    /// Minimal mock daemon: streams `chat_body` for every chat POST and
    /// records approval decisions. No new deps (axum is already one).
    async fn mock_chat_server(
        chat_body: String,
        chat_status: u16,
        approvals: std::sync::Arc<std::sync::Mutex<Vec<ApprovalDecision>>>,
    ) -> String {
        use axum::{routing::post, Json, Router};
        let app = Router::new()
            .route(
                "/api/sessions/{id}/chat",
                post(move || {
                    let body = chat_body.clone();
                    async move {
                        (
                            axum::http::StatusCode::from_u16(chat_status).unwrap(),
                            [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            body,
                        )
                    }
                }),
            )
            .route(
                "/api/sessions/{id}/approve",
                post(move |Json(req): Json<ApprovalResponse>| {
                    let approvals = approvals.clone();
                    async move {
                        approvals.lock().unwrap().push(req.decision);
                        Json(serde_json::json!({"status": "ok"}))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn chat_stream_delivers_events_and_posts_approval() {
        // Regression for the stuck-`Working ...` hang: the TUI worker drives
        // this stream with `send().await` / `recv().await` only. The old
        // worker called `blocking_send` / `blocking_recv` inside `block_on`,
        // which panics ("Cannot block the current thread from within a
        // runtime"), so no event ever reached the transcript.
        let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let complete = StreamEnvelope {
            seq: 3,
            event: StreamEvent::TurnComplete {
                response: "done".to_string(),
                usage: None,
                cached: None,
            },
        };
        let body = format!(
            "\ndata: ping\n\ndata: not-json\n\n{}{}data: {}",
            sse_data(&StreamEvent::AssistantText("hello".to_string()), 1),
            sse_data(
                &StreamEvent::ApprovalRequired {
                    request_id: "r1".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                },
                2
            ),
            serde_json::to_string(&complete).unwrap(), // no trailing newline
        );
        let base = mock_chat_server(body, 200, approvals.clone()).await;
        let client = DaemonClient::new(&base).unwrap();
        let fut = async {
            let mut stream = client
                .chat_stream("s1", "hi", ChatOptions::default())
                .await
                .expect("POST must succeed");
            let mut kinds = Vec::new();
            while let Some(item) = stream.next_event().await {
                let event = item.expect("transport must not fail");
                if let StreamEvent::ApprovalRequired { request_id, .. } = &event {
                    client
                        .approve_async("s1", request_id, ApprovalDecision::AllowOnce)
                        .await
                        .expect("approve POST must succeed");
                }
                kinds.push(match &event {
                    StreamEvent::AssistantText(_) => "text",
                    StreamEvent::ApprovalRequired { .. } => "approval",
                    StreamEvent::TurnComplete { .. } => "complete",
                    _ => "other",
                });
            }
            kinds
        };
        let kinds = tokio::time::timeout(Duration::from_secs(10), fut)
            .await
            .expect("stream must terminate, not hang");
        assert_eq!(kinds, vec!["text", "approval", "complete"]);
        assert_eq!(
            *approvals.lock().unwrap(),
            vec![ApprovalDecision::AllowOnce]
        );
    }

    #[tokio::test]
    async fn chat_stream_surfaces_transport_error() {
        let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let base = mock_chat_server(String::new(), 500, approvals).await;
        let client = DaemonClient::new(&base).unwrap();
        let err = match client.chat_stream("s1", "hi", ChatOptions::default()).await {
            Ok(_) => panic!("500 must fail"),
            Err(e) => e,
        };
        assert!(!err.to_string().is_empty());
    }

    #[tokio::test]
    async fn chat_async_callback_path_still_forwards_and_approves() {
        // The sync-callback API (one-shot CLI, repl, e2e) shares the same
        // `ChatStream` framing; approvals resolve via the callback return.
        let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let body = format!(
            "{}{}",
            sse_data(&StreamEvent::AssistantText("hi".to_string()), 1),
            sse_data(
                &StreamEvent::ApprovalRequired {
                    request_id: "r9".to_string(),
                    name: "bash".to_string(),
                    input: "{}".to_string(),
                },
                2
            )
        );
        let base = mock_chat_server(body, 200, approvals.clone()).await;
        let client = DaemonClient::new(&base).unwrap();
        let mut seen = Vec::new();
        client
            .chat_async("s1", "hi", ChatOptions::default(), &mut |event| {
                let decision = matches!(event, StreamEvent::ApprovalRequired { .. })
                    .then_some(ApprovalDecision::Deny);
                seen.push(matches!(event, StreamEvent::ApprovalRequired { .. }));
                decision
            })
            .await
            .expect("chat must succeed");
        assert_eq!(seen, vec![false, true]);
        assert_eq!(*approvals.lock().unwrap(), vec![ApprovalDecision::Deny]);
    }
}
