use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::protocol::{
    ApprovalDecision, ApprovalResponse, ChatRequest, CreateSessionRequest, CreateSessionResponse,
    DaemonInfo, EventsResponse, FollowupRequest, GitInfo, LoadSkillRequest, LoadSkillResponse,
    ReattachResponse, RecallRequest, SessionInfo, ShellRequest, ShellResponse, SkillInfo,
    SteerRequest, StreamEnvelope, StreamEvent,
};

use super::runtime::{block_on, shared_async_client, shared_streaming_client};
pub use super::sse::ChatStream;

/// Per-request overrides forwarded to the daemon with a chat turn.
#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    pub skill_dirs: Vec<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub permission: Option<String>,
    /// Agent mode (`plan`/`manual`/`auto`), the client's session selector.
    /// The daemon prefers it over `permission` and appends the plan
    /// directive in plan mode.
    pub mode: Option<String>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    pub plan: Option<String>,
    /// Custom base system prompt text (resolved client-side from
    /// `--system-prompt` / `--system-prompt-file`).
    pub system_prompt: Option<String>,
    /// Remote `/thinking` choice: `None` uses the daemon default, `Some("")`
    /// is an explicit clear, otherwise the level. Mirrors `ChatRequest`.
    pub thinking_effort: Option<String>,
    /// P10: replay-safe submission key; the daemon dedups identical keys within 60s.
    pub idempotency_key: Option<String>,
}

/// HTTP client for communicating with the dex daemon.
///
/// Async-first: one shared `reqwest::Client` (TLS + pool init once, clones
/// are atomic bumps). Sync methods remain for one-shot CLI + repl; they
/// `block_on` the async implementations so behavior is identical.
#[derive(Clone)]
pub struct DaemonClient {
    base_url: String,
    http: reqwest::Client,
    streaming: reqwest::Client,
    /// Bearer token for the daemon API, when it requires one: `DEX_DAEMON_TOKEN`
    /// wins, else the token file the daemon publishes for non-loopback binds.
    /// Loopback daemons run without a token, so a missing file is not an error.
    token: Option<String>,
    warning: Arc<dyn Fn(&str) + Send + Sync>,
}

/// Auth lines from a `GET /api/mcp` body: the non-null `auth` fields in
/// server order. `null` means stdio (no login possible) — skipped, so
/// `/mcp` never nags about servers that can't take a login.
/// Lenient list extraction shared by the list-style endpoints: rows that
/// fail to deserialize are skipped rather than failing the whole call.
fn lenient_array<T: serde::de::DeserializeOwned>(value: &serde_json::Value, key: &str) -> Vec<T> {
    value[key]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

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
        Self::with_token(base_url, crate::auth::client_daemon_token())
    }

    /// Create a client with an explicitly supplied daemon token. This is the
    /// reusable constructor for embedders that manage credentials themselves;
    /// `None` means send unauthenticated requests.
    pub fn with_token(
        base_url: &str,
        token: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::with_token_and_clients(
            base_url,
            token,
            shared_async_client(),
            shared_streaming_client(),
        )
    }

    /// Create a client with caller-owned HTTP pools. Hosts can preserve their
    /// user-agent, proxy, timeout, or telemetry policy without coupling this
    /// crate to the host runtime.
    pub fn with_token_and_clients(
        base_url: &str,
        token: Option<String>,
        http: reqwest::Client,
        streaming: reqwest::Client,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            http,
            streaming,
            token: token
                .map(|token| token.trim().to_string())
                .filter(|t| !t.is_empty()),
            warning: Arc::new(|message| eprintln!("dex-client: {message}")),
        })
    }

    /// Set a host logging callback for non-fatal client warnings.
    pub fn with_warning_handler(mut self, handler: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.warning = Arc::new(handler);
        self
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

    /// Loaded extension summaries from the daemon (`GET /api/extensions`).
    pub async fn extensions_status_async(
        &self,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let body = self
            .http
            .get(format!("{}/api/extensions", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        Ok(body)
    }

    /// Ask the daemon to rescan its extensions (`POST /api/extensions/reload`).
    pub async fn extensions_reload_async(
        &self,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let body = self
            .http
            .post(format!("{}/api/extensions/reload", self.base_url))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        Ok(body)
    }

    /// Run one registered extension slash command on the daemon
    /// (`POST /api/extensions/run`). The daemon resolves `name` against its
    /// own manager; unknown names are a 404 so the caller can fall back to
    /// "unknown command". Run failures arrive as 200 with an `error` field.
    pub async fn extensions_run_async(
        &self,
        name: &str,
        arg: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
        let body = self
            .http
            .post(format!("{}/api/extensions/run", self.base_url))
            .headers(self.api_headers())
            .json(&crate::protocol::ExtensionRunRequest {
                name: name.to_string(),
                arg: arg.to_string(),
            })
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        Ok(body)
    }

    pub fn extensions_run(
        &self,
        name: &str,
        arg: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        block_on(self.extensions_run_async(name, arg))
            .map_err(|e| -> Box<dyn std::error::Error> { e })
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
    /// `{base}/api/sessions/{sid}/{tail}` — every per-session endpoint.
    fn session_url(&self, session_id: &str, tail: &str) -> String {
        format!("{}/api/sessions/{}/{}", self.base_url, session_id, tail)
    }

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

        let sessions: Vec<SessionInfo> = lenient_array(&resp, "sessions");

        Ok(sessions)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionInfo>, Box<dyn std::error::Error>> {
        block_on(self.list_sessions_async())
    }

    /// Parse one raw SSE line into a `StreamEvent`. Pure (no I/O, no runtime
    /// interaction) so it is safe to call from any context and trivial to
    /// unit-test. Returns `None` for keep-alives (`ping`), blanks, non-`data:`
    /// lines, and unparsable payloads (skipped, matching prior behavior).
    #[cfg(test)]
    pub(crate) fn parse_sse_line(line: &str) -> Option<StreamEvent> {
        Some(crate::sse::SseFramer::parse_envelope(line)?.event)
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
        let url = self.session_url(session_id, "chat");
        // The chat SSE stream lives as long as the turn (often minutes);
        // the shared client's total timeout would kill it mid-turn.
        let mut builder = self.streaming.post(&url).headers(self.api_headers());
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
                mode: options.mode,
                headers: options.headers,
                plan: options.plan,
                system_prompt: options.system_prompt,
                thinking_effort: options.thinking_effort,
            })
            .send()
            .await
            .map_err(|e| e.to_string())?
            .error_for_status()
            .map_err(|e| e.to_string())?;
        Ok(ChatStream::new(response))
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
            .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        while let Some(item) = stream.next_event().await {
            let event = item.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            if let StreamEvent::ApprovalRequired { ref request_id, .. } = &event {
                let request_id = request_id.clone();
                let decision = on_event(event).unwrap_or(ApprovalDecision::Deny);
                if let Err(e) = self.approve_async(session_id, &request_id, decision).await {
                    (self.warning)(&format!("daemon approval delivery failed: {e}"));
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
            .post(self.session_url(session_id, "approve"))
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
            .post(self.session_url(session_id, "cancel"))
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
        let skills: Vec<SkillInfo> = lenient_array(&resp, "skills");
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

    /// Replay journaled stream events from `since` on (`since` is the next
    /// seq to serve, inclusive — chain with the returned `next_seq`).
    /// Lenient per row (V1b fallback): an event type this client does not know is skipped
    /// while `next_seq` still advances past it, so a replay never stalls on
    /// a newer daemon's events.
    pub async fn events_async(
        &self,
        session_id: &str,
        since: u64,
    ) -> Result<EventsResponse, Box<dyn std::error::Error>> {
        // Explicit page limit: old daemons ignore it (their unbounded reply
        // still drains via the `next_seq` no-progress backup in the replay
        // loop), new ones bound the page.
        let resp = self
            .http
            .get(format!(
                "{}?since={}&limit={}",
                self.session_url(session_id, "events"),
                since,
                crate::protocol::EVENTS_PAGE_LIMIT,
            ))
            .headers(self.api_headers())
            .send()
            .await?
            .error_for_status()?;
        let text = resp.text().await?;
        let value: serde_json::Value = serde_json::from_str(&text)?;
        let next_seq = value
            .get("next_seq")
            .and_then(|v| v.as_u64())
            .unwrap_or(since);
        let events: Vec<StreamEnvelope> = lenient_array(&value, "events");
        Ok(EventsResponse { events, next_seq })
    }

    pub fn events(
        &self,
        session_id: &str,
        since: u64,
    ) -> Result<EventsResponse, Box<dyn std::error::Error>> {
        block_on(self.events_async(session_id, since))
    }

    /// Fetch the redacted trace rows for a session (P9). The UI never renders
    /// the raw trace rows because the journal already carries user events.
    pub async fn trace_async(
        &self,
        session_id: &str,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let resp: serde_json::Value = self
            .http
            .get(self.session_url(session_id, "trace"))
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
            .post(self.session_url(session_id, "undo"))
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
            .post(self.session_url(session_id, "waive"))
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
            .post(self.session_url(session_id, "name"))
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
            .post(self.session_url(session_id, "shell"))
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
            .map_err(|e| -> Box<dyn std::error::Error> { e })
    }

    /// Enqueue a steering message into an active turn (mid-turn injection).
    pub async fn steer_async(
        &self,
        session_id: &str,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(self.session_url(session_id, "steer"))
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
            .post(self.session_url(session_id, "followup"))
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

    /// Recall a queued steering/follow-up message the daemon has not injected
    /// yet, so the caller can edit and resubmit it (Alt+Up in the TUI).
    pub async fn recall_async(
        &self,
        session_id: &str,
        content: &str,
        followup: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.http
            .post(self.session_url(session_id, "recall"))
            .headers(self.api_headers())
            .json(&RecallRequest {
                content: content.to_string(),
                followup,
            })
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub fn recall(
        &self,
        session_id: &str,
        content: &str,
        followup: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        block_on(self.recall_async(session_id, content, followup))
    }
}

// `pub(crate)`: `client/repl.rs` tests reuse `spawn_daemon_sync` (same test

#[cfg(test)]
pub(crate) mod tests;
