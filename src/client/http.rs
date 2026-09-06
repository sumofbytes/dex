use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::protocol::{
    ApprovalDecision, ApprovalResponse, ChatRequest, CreateSessionRequest, CreateSessionResponse,
    DaemonInfo, EventsResponse, FollowupRequest, GitInfo, LoadSkillRequest, LoadSkillResponse,
    ReattachResponse, SessionInfo, SkillInfo, SteerRequest, StreamEnvelope, StreamEvent,
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
}

/// Process-wide shared async client. `Client::new()` initializes a TLS
/// backend + connection pool (~tens of ms); the TUI used to build one per
/// `DaemonClient` plus one per display config. Clones are an atomic bump.
///
/// Timeout note: 10s connect / 300s per-read. `wait_until_ready` overrides
/// to 2s per poll.
static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub(crate) fn shared_async_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Back-compat alias during migration: was `reqwest::blocking::Client`.
pub(crate) fn shared_blocking_client() -> reqwest::Client {
    shared_async_client()
}

impl DaemonClient {
    pub fn new(base_url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self {
            base_url,
            http: shared_async_client(),
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
        let url = format!("{}/api/sessions/{}/chat", self.base_url, session_id);

        let mut builder = self.http.post(&url).headers(self.api_headers());
        if let Some(key) = &options.idempotency_key {
            builder = builder.header("idempotency-key", key);
        }
        let mut response = builder
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
            .await?
            .error_for_status()?;

        let mut buf: Vec<u8> = Vec::new();
        #[allow(clippy::while_let_loop)]
        while let Some(bytes) = response.chunk().await? {
            buf.extend_from_slice(&bytes);
            // Split on newlines; keep trailing partial line buffered.
            loop {
                let Some(pos) = buf.iter().position(|&b| b == b'\n') else {
                    break;
                };
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let trimmed = String::from_utf8_lossy(&line);
                let trimmed = trimmed.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                let Some(data) = trimmed.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "ping" {
                    continue;
                }
                let event = match serde_json::from_str::<StreamEnvelope>(data) {
                    Ok(env) => env.event,
                    Err(_) => continue,
                };
                if let StreamEvent::ApprovalRequired { ref request_id, .. } = &event {
                    let request_id = request_id.clone();
                    let decision = on_event(event).unwrap_or(ApprovalDecision::Deny);
                    if let Err(e) = self.approve_async(session_id, &request_id, decision).await {
                        crate::llm::client::provider_log(
                            "approval_delivery_failed",
                            &e.to_string(),
                        );
                    }
                    continue;
                }
                on_event(event);
            }
        }
        // Trailing buffered line without newline (terminal envelope).
        if !buf.is_empty() {
            let trimmed = String::from_utf8_lossy(&buf);
            let trimmed = trimmed.trim_end();
            if let Some(data) = trimmed.strip_prefix("data:") {
                let data = data.trim();
                if !data.is_empty() && data != "ping" {
                    if let Ok(env) = serde_json::from_str::<StreamEnvelope>(data) {
                        let event = env.event;
                        if let StreamEvent::ApprovalRequired { ref request_id, .. } = &event {
                            let request_id = request_id.clone();
                            let decision = on_event(event).unwrap_or(ApprovalDecision::Deny);
                            let _ = self.approve_async(session_id, &request_id, decision).await;
                        } else {
                            on_event(event);
                        }
                    }
                }
            }
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
}
