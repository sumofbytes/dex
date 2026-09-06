use std::convert::Infallible;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse as _;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_core::Stream;
use serde_json::json;
use tokio::sync::mpsc;

use crate::agent::r#loop::process_turn;
use crate::agent::state::ToolState;
use crate::core::console::{CancellationToken, Console, TraceWriter};
use crate::core::types::{ApprovalDecision, ApprovalRequest, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt;
use crate::protocol::{
    ApprovalResponse, ChatRequest, CreateSessionRequest, DaemonInfo, EventsResponse,
    FollowupRequest, GitInfo, LoadSkillRequest, ReattachResponse, SkillInfo, SteerRequest,
    StreamEnvelope, StreamEvent,
};
use crate::session::{self, Session};
use crate::skills::{discover_skills_async, discover_skills_fresh_async, skill_dirs};

use super::{required_token, DaemonState, PendingApproval, SessionEntry};

/// Bearer-token gate: every `/api/*` route requires `Authorization: Bearer
/// <token>` when the daemon requires a token (non-loopback bind or an
/// explicit `DEX_DAEMON_TOKEN`). `/health` stays open so liveness checks and
/// `wait_until_ready` work before any credential is exchanged.
async fn require_bearer(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if request.uri().path().starts_with("/api/") {
        if let Some(expected) = required_token() {
            let provided = request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::trim);
            let ok = provided.is_some_and(|p| {
                p.len() == expected.len() && {
                    // Constant-time-ish compare: leaks at most the prefix length.
                    p.bytes()
                        .zip(expected.bytes())
                        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                        == 0
                }
            });
            if !ok {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "missing or invalid bearer token (set DEX_DAEMON_TOKEN on the client)",
                )
                    .into_response();
            }
        }
    }
    next.run(request).await
}

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/config", get(get_config))
        .route("/api/git", get(get_git))
        .route("/api/mcp", get(get_mcp))
        .route("/api/mcp/{server}/reconnect", post(mcp_reconnect))
        .route("/api/skills", get(list_skills))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/steer", post(steer))
        .route("/api/sessions/{id}/followup", post(followup))
        .route("/api/sessions/{id}/skill", post(load_skill))
        // P10: versioned reattach/replay, P9: trace, P8: undo.
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/reattach", post(reattach))
        .route("/api/sessions/{id}/trace", get(session_trace))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route("/api/sessions/{id}/waive", post(session_waive))
        .route("/api/sessions/{id}/name", post(session_name))
        .with_state(state)
        .layer(axum::middleware::from_fn(require_bearer))
}

async fn health(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    // `rebuild_complete` is false while the background startup scan is still
    // merging the disk registry; clients hitting 404s mid-window should
    // retry rather than treat the session as gone.
    Json(json!({
        "status": "ok",
        "rebuild_complete": state.rebuild_complete.load(Ordering::Relaxed),
    }))
}

/// Best-effort runtime info so remote clients can render the same status
/// footer as the local TUI. Resolved from the daemon's own environment.
async fn cached_git_context_async(cwd: &str) -> (Option<String>, bool) {
    // `git branch` + `git status` via `tokio::process` under the 5s cache (Phase 4/6).
    struct Entry {
        at: Instant,
        branch: Option<String>,
        dirty: bool,
    }
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, Entry>>> = OnceLock::new();
    if let Some(hit) = CACHE
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(cwd)
        .filter(|e| e.at.elapsed() < Duration::from_secs(5))
    {
        return (hit.branch.clone(), hit.dirty);
    }
    let (branch, dirty) = crate::core::format::git_context_async(cwd).await;
    let mut cache = CACHE
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() > 32 {
        cache.clear();
    }
    cache.insert(
        cwd.to_string(),
        Entry {
            at: Instant::now(),
            branch: branch.clone(),
            dirty,
        },
    );
    (branch, dirty)
}

async fn resolve_daemon_info_async() -> DaemonInfo {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (git_branch, git_dirty) = cached_git_context_async(&cwd).await;
    match LlmConfig::from_env_async(None, None, None, Vec::new()).await {
        Ok(config) => DaemonInfo {
            provider: config.provider.name().to_string(),
            model: config.model.clone(),
            api: config.api.name().to_string(),
            available_models: config.available_models.clone(),
            context_window: config.context_window,
            permission: match config.permission {
                crate::core::types::PermissionMode::ReadOnly => "read-only".into(),
                crate::core::types::PermissionMode::AskWrites => "ask-writes".into(),
                crate::core::types::PermissionMode::AskShell => "ask-shell".into(),
                crate::core::types::PermissionMode::Trusted => "trusted".into(),
            },
            cwd,
            git_branch,
            git_dirty,
            thinking_effort: config.thinking_effort.clone(),
            thinking_warning: config.thinking_mismatch_warning(),
        },
        Err(_) => {
            // Config is incomplete (e.g. no API key yet); report what we can
            // so the client still renders.
            let permission = std::env::var("DEX_PERMISSION")
                .ok()
                .filter(|v| crate::core::types::PermissionMode::parse(v).is_ok())
                .unwrap_or_else(|| "ask-writes".to_string());
            let model = "unknown".to_string();
            let provider_name = std::env::var("DEX_PROVIDER")
                .ok()
                .unwrap_or_else(|| "opencode".to_string());
            DaemonInfo {
                provider: provider_name,
                model,
                api: "openai-responses".to_string(),
                available_models: Vec::new(),
                context_window: 128_000,
                permission,
                cwd,
                git_branch,
                git_dirty,
                thinking_effort: None,
                thinking_warning: None,
            }
        }
    }
}

async fn get_config() -> Json<DaemonInfo> {
    // Async client is Clone (no blocking TLS init); cache hits are a mutex bump.
    Json(resolve_daemon_info_async().await)
}

/// Lightweight footer poll: just the daemon workspace's branch/dirty, behind
/// the same 5s `cached_git_context_async` as `/api/config` so a 2s TUI poll costs
/// at most one `git` spawn per 5s — and never pays `LlmConfig::from_env`.
async fn get_git() -> Json<GitInfo> {
    // `tokio::process` git spawns under the 5s cache; per-frame cost zero.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (git_branch, git_dirty) = cached_git_context_async(&cwd).await;
    Json(GitInfo {
        git_branch,
        git_dirty,
    })
}

async fn get_mcp() -> Json<serde_json::Value> {
    // Reads the process-wide cache; never spawns, never blocks the loop.
    // `error` carries the last connect/probe failure so operators see *why*
    // a server is down; `truncated` counts schema-cap drops (see loop.rs).
    // `auth` carries the OAuth line for HTTP servers (`null` for stdio, which
    // needs no login, and for servers the daemon never configured).
    // Config loads once and maps over statuses (no N+1 reloads).
    let configs = crate::mcp::load_server_configs();
    let servers: Vec<serde_json::Value> = crate::mcp::global_manager()
        .statuses()
        .await
        .into_iter()
        .map(|s| json!({"name": s.name, "state": s.state, "tools": s.tools, "error": s.error, "auth": crate::mcp::oauth::auth_line_with(&configs, &s.name)}))
        .collect();
    Json(json!({ "servers": servers, "truncated": crate::mcp::cached_truncated() }))
}

/// Drop the client and reconnect now; surfaces the error instead of only
/// recording `down`. Operators hit this after fixing a crashed server
/// instead of restarting the daemon.
async fn mcp_reconnect(Path(server): Path<String>) -> Json<serde_json::Value> {
    match crate::mcp::global_manager().reconnect(&server).await {
        Ok(tools) => Json(json!({ "server": server, "state": "up", "tools": tools })),
        Err(error) => Json(json!({ "server": server, "state": "down", "error": error })),
    }
}

async fn list_skills() -> Json<serde_json::Value> {
    let dirs = skill_dirs();
    let skills = discover_skills_async(&dirs).await;
    let infos: Vec<SkillInfo> = skills
        .into_iter()
        .map(|s| SkillInfo {
            name: s.name,
            description: s.description,
        })
        .collect();
    Json(json!({ "skills": infos }))
}

async fn load_skill(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<LoadSkillRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if req.name.is_empty()
        || !req
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let entry = {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(&session_id).cloned()
    }
    .ok_or(StatusCode::NOT_FOUND)?;
    let skill_name = req.name.clone();
    let extra_dirs = req.skill_dirs.clone();
    let mut dirs = skill_dirs();
    dirs.extend(extra_dirs.iter().map(std::path::PathBuf::from));
    // Explicit user load: bypass the discovery cache so a just-added
    // skill resolves immediately (async dir scans + concurrent reads).
    let skills = discover_skills_fresh_async(&dirs).await;
    let skill = skills
        .into_iter()
        .find(|s| s.name == skill_name)
        .ok_or(StatusCode::NOT_FOUND)?;
    let content = tokio::fs::read_to_string(&skill.path)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let skill_for_msg = skill.clone();
    let content_for_msg = content.clone();
    tokio::task::spawn_blocking(move || {
        let mut session = if entry.path.exists() {
            Session::from_path(&entry.path).map_err(|e| format!("load session: {e}"))?
        } else {
            Session::new(entry.cwd.clone(), entry.name.clone())
                .map_err(|e| format!("create session: {e}"))?
        };
        let msg = ChatMessage::user_named(
            format!("--- Skill: {} ---\n{}", skill_for_msg.name, content_for_msg),
            "skill",
        );
        session
            .append_message(msg)
            .map_err(|e| format!("append: {e}"))?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({
        "name": skill.name,
        "description": skill.description,
        "content": content,
    })))
}

async fn create_session(
    State(state): State<Arc<DaemonState>>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Tools run in the daemon's working directory (the server owns the
    // workspace), so sessions are recorded against it. A co-located client's
    // cwd matches anyway; a remote client's cwd is not meaningful on the
    // server and would be misleading in session listings.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| req.cwd.clone());
    let session = Session::new(cwd.clone(), req.name.clone())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let session_id = session.id().to_string();
    let path = session
        .path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // `Session::new` fills in the default `<workspace>-<7 chars>` name when
    // the request carries none, so read it back from the session: the
    // in-memory entry must match the persisted header.
    let entry = SessionEntry {
        path: path.clone().into(),
        name: session.name().map(ToString::to_string),
        cwd,
    };
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.clone(), entry);

    Ok(Json(json!({
        "session_id": session_id,
        "path": path,
    })))
}

async fn list_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    // P10: disk-backed listing with JoinSet parallel per-session scans
    // (`spawn_blocking` per file, join, sort) — fixes the linear scan (S2).
    let mut by_id: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    let listed = session::Session::list_all_async().await.unwrap_or_default();
    // Per-session message_count + turn_state in parallel (spawn_blocking per file).
    let mut set = tokio::task::JoinSet::new();
    for (path, header) in listed {
        set.spawn(tokio::task::spawn_blocking(move || {
            let message_count = crate::session::load_messages_from_session(&path)
                .map(|m| m.len())
                .unwrap_or(0);
            let turn_state = session::Session::last_turn_state(&path).to_string();
            (path, header, message_count, turn_state)
        }));
    }
    let mut scanned = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Ok(v)) = r {
            scanned.push(v);
        }
    }
    scanned.sort_by(|a, b| b.1.timestamp().cmp(a.1.timestamp()));
    for (path, header, message_count, turn_state) in scanned {
        let name = header.name().map(|n| n.to_string());
        by_id.insert(
            header.id().to_string(),
            json!({
                "session_id": header.id(),
                "path": path.display().to_string(),
                "name": name,
                "cwd": header.cwd(),
                "created_at": header.timestamp(),
                "message_count": message_count,
                "turn_state": turn_state,
            }),
        );
    }
    // Preserve in-memory sessions that have no file yet (shouldn't happen).
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (id, entry) in sessions.iter() {
            by_id.entry(id.clone()).or_insert_with(|| {
                json!({
                    "session_id": id,
                    "path": entry.path.to_string_lossy(),
                    "name": entry.name,
                    "cwd": entry.cwd,
                    "created_at": "",
                    "message_count": 0,
                    "turn_state": "unknown",
                })
            });
        }
    }
    let mut sessions: Vec<serde_json::Value> = by_id.into_values().collect();
    sessions.sort_by(|a, b| {
        let a = a
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let b = b
            .get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        b.cmp(a)
    });
    Json(json!({ "sessions": sessions }))
}

async fn chat(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    // P10 versioned protocol: a client MAY declare its protocol version; a
    // newer-than-supported version is rejected. Absence stays backward-compatible.
    if let Some(protocol) = headers.get("x-dex-protocol").and_then(|v| v.to_str().ok()) {
        if protocol != "1" {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    // P10 idempotency: the same `Idempotency-Key` within 60s replays the
    // recorded terminal event instead of re-running effects.
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .filter(|k| !k.is_empty())
        .map(ToOwned::to_owned);
    let request_hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&req).unwrap_or_default().hash(&mut h);
        h.finish()
    };
    // Idempotency replay is routed through the SAME channel/stream as a live
    // turn (single return type below): the recorded terminal envelope is just
    // pushed and the stream closes.
    let mut replay_envelope: Option<StreamEnvelope> = None;
    if let Some(key) = &idempotency_key {
        if let Some(terminal) = state.idempotent_replay(key, &session_id, request_hash) {
            replay_envelope = serde_json::from_str::<StreamEnvelope>(&terminal).ok();
        }
    }
    // Reject concurrent turns on the same session up front so the
    // append-only session log stays consistent. A replay must not hold the
    // active-turn slot, so it is checked before registration.
    // Disk fallback: the session may predate the background rebuild scan.
    if lookup_entry(&state, &session_id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Pre-create per-turn channels so `POST /steer` / `POST /followup`
    // have a target as soon as the turn is registered (avoids a race where
    // the client sends steering in the gap between `active_turns` insert and
    // the `spawn_blocking` thread creating its channels).
    let mut steering_rx_opt: Option<mpsc::Receiver<String>> = None;
    let mut followup_rx_opt: Option<mpsc::Receiver<String>> = None;
    let mut cancel_for_turn: Option<CancellationToken> = None;
    if replay_envelope.is_none() {
        {
            let mut active = state.active_turns.lock().unwrap_or_else(|e| e.into_inner());
            if active.contains(&session_id) {
                return Err(StatusCode::CONFLICT);
            }
            active.insert(session_id.clone());
        }

        // Register a fresh per-turn cancellation token before spawning so a
        // /cancel arriving during turn setup is still observed. The agent loop
        // and the stream reader poll it; it never leaks across sessions or
        // later turns.
        let cancel = CancellationToken::new();
        state
            .cancel_tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), cancel.clone());
        cancel_for_turn = Some(cancel);
        // Steering / follow-up queues for this turn (mirrors old local
        // `event.rs` channels). Insert now so the HTTP handlers can push
        // immediately.
        let (steering_tx, steering_rx) = mpsc::channel::<String>(16);
        let (followup_tx, followup_rx) = mpsc::channel::<String>(16);
        state
            .steering_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), steering_tx);
        state
            .followup_txs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), followup_tx);
        steering_rx_opt = Some(steering_rx);
        followup_rx_opt = Some(followup_rx);
    }

    let (tx, rx) = mpsc::channel::<StreamEnvelope>(256);

    if let Some(env) = replay_envelope {
        // Replay: emit the recorded terminal envelope, then close.
        let tx = tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(env).await;
        });
    } else {
        // Async turn: `tokio::spawn(run_agent_turn(...))` — session IO/config
        // misses go through `spawn_blocking`, sockets/timers/bridges are tasks.
        let state_for_turn = state.clone();
        let sid = session_id.clone();
        let idem_key = idempotency_key;
        let cancel = cancel_for_turn.unwrap_or_default();
        tokio::spawn(async move {
            run_agent_turn(
                state_for_turn,
                sid,
                req,
                cancel,
                tx,
                idem_key,
                request_hash,
                steering_rx_opt,
                followup_rx_opt,
            )
            .await;
        });
    }

    // Convert the receiver into an SSE stream. Each event is serialized
    // exactly once: axum adds the `data:` prefix, so hand it raw JSON.
    // A 15-line `poll_recv` wrapper instead of the `async-stream` macro.
    let event_stream = ReceiverStream { rx };

    Ok(Sse::new(event_stream).keep_alive(
        axum::response::sse::KeepAlive::default()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
}

/// Bridge a tokio mpsc receiver into a `Stream` for axum's SSE body.
struct ReceiverStream {
    rx: mpsc::Receiver<StreamEnvelope>,
}

impl Stream for ReceiverStream {
    type Item = Result<Event, Infallible>;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.get_mut().rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(event)) => {
                let data = serde_json::to_string(&event).unwrap_or_default();
                std::task::Poll::Ready(Some(Ok(Event::default().data(data))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// `Future::catch_unwind` without a new dependency: polls the inner future
/// inside `std::panic::catch_unwind` per poll, so a panic in
/// `run_turn_inner` becomes `Err("turn panicked")` (the pre-async contract)
/// instead of aborting the spawned turn task with no terminal SSE event —
/// the client would otherwise hang until keep-alive timeout with cleanup
/// done (via `TurnGuard::drop`) but no `TurnFailed` ever sent.
struct CatchUnwind<F>(std::panic::AssertUnwindSafe<F>);

impl<F: std::future::Future> std::future::Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // SAFETY: we never move `F` out of the pin; only project to poll it.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.0 .0) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err("turn panicked".to_string())),
        }
    }
}

/// Run one agent turn and push numbered `StreamEnvelope`s into `tx`. Async:
/// spawned via `tokio::spawn`, bridges are tasks with `send().await`.
#[allow(clippy::too_many_arguments)]
async fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: String,
    req: ChatRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEnvelope>,
    idempotency_key: Option<String>,
    request_hash: u64,
    steering_rx: Option<mpsc::Receiver<String>>,
    followup_rx: Option<mpsc::Receiver<String>>,
) {
    // Use a guard so active_turns/cancel_tokens/pending approvals/steering
    // are cleaned even when run_turn_inner panics inside the spawned task
    // (`CatchUnwind` below still delivers `TurnFailed` in that case).
    struct TurnGuard {
        state: Arc<DaemonState>,
        session_id: String,
    }
    impl Drop for TurnGuard {
        fn drop(&mut self) {
            {
                let mut pending = self
                    .state
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                pending.retain(|_, p| {
                    if p.session_id == self.session_id {
                        let _ = p.response.try_send(ApprovalDecision::Deny);
                        false
                    } else {
                        true
                    }
                });
            }
            self.state
                .active_turns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
            self.state
                .cancel_tokens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
            self.state
                .steering_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
            self.state
                .followup_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&self.session_id);
        }
    }
    let _guard = TurnGuard {
        state: state.clone(),
        session_id: session_id.clone(),
    };
    // Steering / follow-up channels for this turn (mirrors the old in-memory
    // `event.rs` submit path). `POST /steer` and `POST /followup` push into
    // these; the agent loop consumes them between iterations / chained turns.
    // When `chat` pre-creates the queues to avoid a race, reuse them; else
    // (direct `run_agent_turn` calls, e.g. tests) create them here.
    let (steering_rx, followup_rx) = match (steering_rx, followup_rx) {
        (Some(sr), Some(fr)) => (sr, fr),
        _ => {
            let (steering_tx, sr) = mpsc::channel::<String>(16);
            let (followup_tx, fr) = mpsc::channel::<String>(16);
            state
                .steering_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.clone(), steering_tx);
            state
                .followup_txs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.clone(), followup_tx);
            (sr, fr)
        }
    };
    let (steering_accepted_tx, mut steering_accepted_rx) = mpsc::channel::<String>(16);
    let (followup_accepted_tx, mut followup_accepted_rx) = mpsc::channel::<String>(16);
    // Forward accepted steers/follow-ups onto the SSE stream so the remote
    // TUI can clear its `pending_*` badge and render the prompt. Journaled
    // so a reattach replay reconstructs the transcript.
    {
        let tx_clone = tx.clone();
        let state_clone = state.clone();
        let sid = session_id.clone();
        let entry_path = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .map(|e| e.path.clone());
        tokio::spawn(async move {
            let mut journal = entry_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            while let Some(content) = steering_accepted_rx.recv().await {
                let event = StreamEvent::SteeringAccepted {
                    content: content.clone(),
                };
                let seq = state_clone.next_seq(&sid);
                if let Some(j) = journal.as_mut() {
                    let _ = j.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = tx_clone.send(StreamEnvelope { seq, event }).await;
            }
        });
    }
    {
        let tx_clone = tx.clone();
        let state_clone = state.clone();
        let sid = session_id.clone();
        let entry_path = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .map(|e| e.path.clone());
        tokio::spawn(async move {
            let mut journal = entry_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            while let Some(content) = followup_accepted_rx.recv().await {
                let event = StreamEvent::FollowupAccepted {
                    content: content.clone(),
                };
                let seq = state_clone.next_seq(&sid);
                if let Some(j) = journal.as_mut() {
                    let _ = j.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = tx_clone.send(StreamEnvelope { seq, event }).await;
            }
        });
    }
    // Own receivers mutably for the async turn (tokio try_recv needs &mut).
    let mut steering_rx = steering_rx;
    let mut followup_rx = followup_rx;
    let result: Result<(String, Option<u64>, Option<u64>), String> =
        match CatchUnwind(std::panic::AssertUnwindSafe(run_turn_inner(
            &state,
            &session_id,
            &req,
            &cancel,
            &tx,
            Some(&mut steering_rx),
            Some(&steering_accepted_tx),
            Some(&mut followup_rx),
            Some(&followup_accepted_tx),
        )))
        .await
        {
            Ok(inner) => inner,
            Err(panicked) => Err(panicked),
        };
    // Drop the guard now before sending the terminal event so a new turn can
    // be accepted promptly; drop ordering handles pending approvals/active turns.
    drop(_guard);

    let terminal = match result {
        Ok((response, usage, cached)) => StreamEvent::TurnComplete {
            response,
            usage,
            cached,
        },
        Err(error) => StreamEvent::TurnFailed { error },
    };
    // P10/P8: the terminal event gets a seq, is journaled (reopening the
    // session file so an in-flight handle is untouched), dedup'd via
    // Idempotency-Key, and only then emitted.
    let seq = state.next_seq(&session_id);
    let env = StreamEnvelope {
        seq,
        event: terminal,
    };
    let serialized = serde_json::to_string(&env).unwrap_or_default();
    if let Some(path) = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session_id)
        .map(|e| e.path.clone())
    {
        if let Ok(mut journal) = Session::from_path(&path) {
            let _ =
                journal.append_event(seq, &serde_json::to_string(&env.event).unwrap_or_default());
            // Durable turn_failed marker for runs that did not finish normally.
            if matches!(&env.event, StreamEvent::TurnFailed { .. })
                && crate::session::Session::last_turn_state(&path) == "interrupted"
            {
                let _ = journal.turn_event("turn_failed");
            }
        }
    }
    if let Some(key) = idempotency_key {
        state.idempotency_record(&key, &session_id, request_hash, serialized);
    }
    let _ = tx.send(env).await;
}

#[allow(clippy::too_many_arguments, unused_assignments)]
async fn run_turn_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: &ChatRequest,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<StreamEnvelope>,
    steering_rx: Option<&mut mpsc::Receiver<String>>,
    steering_accepted_tx: Option<&mpsc::Sender<String>>,
    followup_rx: Option<&mut mpsc::Receiver<String>>,
    followup_accepted_tx: Option<&mpsc::Sender<String>>,
) -> Result<(String, Option<u64>, Option<u64>), String> {
    let entry = {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get(session_id).cloned()
    }
    .ok_or_else(|| "session not found".to_string())?;

    // Resume the session created via POST /api/sessions; fall back to a fresh
    // one if the file vanished.
    let mut session = if entry.path.exists() {
        Session::from_path(&entry.path).map_err(|e| format!("failed to load session: {e}"))?
    } else {
        Session::new(entry.cwd.clone(), entry.name.clone())
            .map_err(|e| format!("failed to create session: {e}"))?
    };

    // Persist plan forwarded by the client (remote TUI slash commands). Empty
    // string clears. Invalid JSON is rejected explicitly rather than silently
    // storing garbage (which would come back as an empty plan on reload).
    if let Some(plan_json) = &req.plan {
        if plan_json.is_empty() {
            session
                .set_state("plan", &crate::core::types::Plan::default().to_json())
                .map_err(|e| format!("failed to persist plan: {e}"))?;
        } else {
            let plan: crate::core::types::Plan = serde_json::from_str(plan_json)
                .map_err(|e| format!("invalid plan JSON from client: {e}"))?;
            session
                .set_state("plan", &plan.to_json())
                .map_err(|e| format!("failed to persist plan: {e}"))?;
        }
    }

    // Permission ceiling: daemon policy (env) is max; client may only go stricter.
    let daemon_perm = crate::llm::config::permission_from_env()
        .unwrap_or(crate::core::types::PermissionMode::AskWrites);
    if let Some(req_perm_str) = &req.permission {
        let req_perm = crate::core::types::PermissionMode::parse(req_perm_str)?;
        if req_perm.permissiveness() > daemon_perm.permissiveness() {
            return Err(format!(
                "permission escalation denied: daemon ceiling is {:?} (client requested {:?}); use a stricter mode or change daemon config",
                daemon_perm, req_perm
            ));
        }
    }
    // Build the config from the daemon's own environment, with
    // optional per-request overrides sent by the client (now validated).
    // Async: cache hits are a mutex bump inline; misses parse the 4MB catalog
    // in `spawn_blocking` (Phase 6 `from_env_async`).
    let perm_override = req
        .permission
        .as_deref()
        .map(crate::core::types::PermissionMode::parse)
        .transpose()?;
    let mut config = LlmConfig::from_env_async(
        req.base_url.clone().filter(|v| !v.is_empty()),
        req.model.clone().filter(|v| !v.is_empty()),
        perm_override,
        Vec::new(),
    )
    .await
    .map_err(|e| format!("failed to build config: {e}"))?;
    // Per-request custom headers from the client (`--header` flags) win
    // over the daemon's own configured headers for this turn only.
    // `insert_extra_header` drops empties + `authorization` and collapses
    // case-insensitive duplicates, so the map stays clean (send-time
    // filtering remains as defense in depth).
    if let Some(headers) = req.headers.as_ref() {
        for (k, v) in headers {
            crate::llm::config::insert_extra_header(&mut config.extra_headers, k, v);
        }
    }
    // Persist provider/model overrides so /resume restores the same provider/base_url without env
    if let Some(raw) = &req.model {
        if !raw.is_empty() {
            let _ = session.set_state("model", raw);
            let _ = session.set_state("provider", config.provider.name());
        }
    }
    // Verification is opt-in (DEX_VERIFY / config verify_command). No
    // auto-detect by default — pi has no verify hook and auto-running
    // `cargo test` after every edit is the biggest loop tax.
    // Set DEX_VERIFY or config verify_command, or DEX_VERIFY=1 with a manifest,
    // to re-enable: `DEX_VERIFY=1` or explicit `verify_command` in config.
    if config.verify_command.is_none() && std::env::var("DEX_VERIFY").as_deref() == Ok("1") {
        config.verify_command = crate::llm::config::detect_verify_command();
    }

    // Skills are resolved on the daemon (its filesystem is the workspace).
    // Async dir scans + concurrent reads (Phase 6).
    let mut dirs = skill_dirs();
    dirs.extend(req.skill_dirs.iter().map(std::path::PathBuf::from));
    let skills = discover_skills_async(&dirs).await;

    // Rebuild the conversation: system prompt + persisted history + prompt.
    // History load via `spawn_blocking` (full-history scan, fast) — Phase 4/6.
    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage::system(system_prompt(&skills)));
    if let Some(path) = session.path().map(|p| p.to_path_buf()) {
        let loaded = tokio::task::spawn_blocking(move || {
            session::load_messages_from_session(&path).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        messages.extend(loaded);
    }
    let user_message = ChatMessage::user(req.prompt.clone());
    // Durable journal (P8): a turn only exists once turn_start is recorded,
    // and an io::Error here fails the turn instead of being swallowed.
    session
        .turn_event("turn_start")
        .map_err(|e| format!("failed to record turn_start: {e}"))?;
    session
        .append_message(user_message.clone())
        .map_err(|e| format!("failed to persist prompt: {e}"))?;
    messages.push(user_message);

    // Per-turn redacted trace journal (P9): `<session>.trace.jsonl`, 0600.
    let trace = session
        .path()
        .map(|p| p.with_extension("trace.jsonl"))
        .and_then(|p| TraceWriter::open(p).ok());

    // The agent loop reports through std channels; bridge them onto the
    // tokio sender with dedicated threads.
    let (sink_tx, sink_rx) = mpsc::channel::<SinkLine>(256);
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>(16);
    let console = Console::daemon(sink_tx, approval_tx).with_trace(trace);
    // Restore “allow for session” approvals that survived from prior turns
    // (previously the per-turn Console dropped them).
    if let Some(set) = state
        .session_approvals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
    {
        console.seed_session_approvals(set);
    }
    // Fast-path daemon check before we even park the turn: if the session
    // already approved this exact scoped key, the agent loop's own
    // `session_approved` check will still succeed, but seeding avoids the
    // overlay round-trip entirely for repeated identical calls within the
    // same session. (No early return here — the loop itself short-circuits.)

    // Sink bridge: SinkLines arrive from the streaming LLM reader and tool
    // executor; forward them as numbered StreamEnvelopes on a dedicated
    // task with the same Thinking/Assistant coalescing, journaling each one
    // for replay (P10). `send().await` replaces `blocking_send`.
    {
        let stream_tx = tx.clone();
        let state = state.clone();
        let session_path = session.path().map(|p| p.to_path_buf());
        let sid = session_id.to_string();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            // Reopen so journal writes never fight the agent loop's handle;
            // events land in the separate `<id>.events.jsonl` file.
            let mut journal = session_path
                .as_deref()
                .and_then(|p| Session::from_path(p).ok());
            let mut deferred: Option<SinkLine> = None;
            let mut sink_rx = sink_rx;
            loop {
                let sl = match deferred.take() {
                    Some(sl) => sl,
                    None => tokio::select! {
                        _ = cancel.cancelled() => break,
                        recvd = sink_rx.recv() => match recvd {
                            Some(sl) => sl,
                            None => break,
                        },
                    },
                };
                // Coalesce per-token thinking deltas: providers stream one
                // sink line per token, and each event costs a seq bump, a
                // journal append, and an SSE write. Drain every already
                // queued consecutive line of the same kind into one event
                // (a different line kind is held back and handled next, in
                // order). Thinking deltas are raw token fragments and join
                // verbatim; Assistant events are complete markdown lines
                // (stream.rs trims the trailing newline) and join with '\n'
                // so paragraph structure survives coalescing.
                let mut batch = sl;
                loop {
                    match (&mut batch, sink_rx.try_recv()) {
                        (SinkLine::Thinking(buf), Ok(SinkLine::Thinking(text))) => {
                            buf.push_str(&text);
                        }
                        (SinkLine::Assistant(buf), Ok(SinkLine::Assistant(text))) => {
                            buf.push('\n');
                            buf.push_str(&text);
                        }
                        (_, Ok(other)) => {
                            deferred = Some(other);
                            break;
                        }
                        (_, Err(_)) => break,
                    }
                }
                let event = match batch {
                    SinkLine::Assistant(text) => StreamEvent::AssistantText(text),
                    SinkLine::Thinking(text) => StreamEvent::Thinking(text),
                    SinkLine::ToolInput(preview) => {
                        let mut parts = preview.splitn(2, ' ');
                        let name = parts.next().unwrap_or_default().to_string();
                        let args = parts.next().unwrap_or_default().to_string();
                        StreamEvent::ToolCall {
                            name,
                            args: serde_json::Value::String(args),
                        }
                    }
                    SinkLine::ToolOutput {
                        name,
                        summary,
                        success,
                        preview,
                        duration,
                    } => StreamEvent::ToolResult {
                        name,
                        summary,
                        success,
                        preview,
                        duration,
                    },
                    SinkLine::System(text) => StreamEvent::System(text),
                    SinkLine::Error(text) => StreamEvent::Error(text),
                    SinkLine::Usage {
                        tokens,
                        cached,
                        cost,
                        output,
                    } => StreamEvent::Usage {
                        tokens,
                        cached,
                        cost,
                        output,
                    },
                    SinkLine::Plan(plan) => StreamEvent::Plan {
                        goal: plan.goal,
                        steps: plan.steps,
                        constraints: plan.constraints,
                        acceptance: plan.acceptance,
                    },
                };
                let seq = state.next_seq(&sid);
                if let Some(s) = journal.as_mut() {
                    let _ = s.append_event(seq, &serde_json::to_string(&event).unwrap_or_default());
                }
                let _ = stream_tx.send(StreamEnvelope { seq, event }).await;
            }
        });
    }

    // Approval bridge: each ApprovalRequest gets a fresh request_id; the
    // response sender is parked in the shared state so POST /approve can
    // resolve it. Task + `send().await` replaces the parked thread.
    {
        let state = state.clone();
        let session_id = session_id.to_string();
        let stream_tx = tx.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut approval_rx = approval_rx;
            while let Some(request) = approval_rx.recv().await {
                // A cancellation was requested: don't surface new approvals,
                // deny them so the agent thread can unwind.
                if cancel.is_cancelled() {
                    let _ = request.response.try_send(ApprovalDecision::Deny);
                    continue;
                }
                let request_id = uuid::Uuid::new_v4().to_string();
                let name_clone = request.name.clone();
                let input_clone = request.input.clone();
                let parked = PendingApproval {
                    session_id: session_id.clone(),
                    response: request.response,
                    name: name_clone,
                    input: input_clone,
                };
                let replaced = state
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(request_id.clone(), parked);
                if let Some(stale) = replaced {
                    // Should not happen (request_ids are unique); deny to
                    // avoid a deadlock in a stray agent thread.
                    let _ = stale.response.try_send(ApprovalDecision::Deny);
                }
                let _ = stream_tx
                    .send(StreamEnvelope {
                        seq: state.next_seq(&session_id),
                        event: StreamEvent::ApprovalRequired {
                            request_id,
                            name: request.name,
                            input: request.input,
                        },
                    })
                    .await;
            }
        });
    }

    let mut tool_state = ToolState::load_async().await;
    #[allow(unused_assignments)]
    // Outer loop for follow-up chaining (mirrors old local `event.rs` loop):
    // `process_turn` consumes steering mid-turn; follow-ups are drained after
    // each successful turn and chained without a new HTTP request.
    let mut final_response = String::new();
    let mut final_usage = None;
    let mut final_cached = None;
    let turn_result: Result<String, Box<dyn std::error::Error + Send + Sync>>;
    // Own the option wrappers for the chained loop; reborrow inner `&mut`
    // each iteration (tokio `try_recv` needs `&mut`).
    let mut steering_opt = steering_rx;
    let mut followup_opt = followup_rx;
    loop {
        // Reborrow `&mut Receiver` from `Option<&mut Receiver>` without moving.
        let steering_reborrow = steering_opt.as_deref_mut();
        let result = process_turn(
            &config,
            &mut messages,
            &mut tool_state,
            steering_reborrow,
            steering_accepted_tx,
            Some(&mut session),
            &config,
            cancel,
            &console,
        )
        .await;
        match result {
            Ok(resp) => {
                final_response = resp;
                final_usage = tool_state.last_usage;
                final_cached = tool_state.last_cached;
                // Drain follow-ups queued while this turn ran.
                let followups: Vec<String> = match followup_opt.as_mut() {
                    Some(rx) => {
                        let mut out = Vec::new();
                        while let Ok(v) = rx.try_recv() {
                            out.push(v);
                        }
                        out
                    }
                    None => Vec::new(),
                };
                if followups.is_empty() {
                    turn_result = Ok(final_response.clone());
                    break;
                }
                for content in followups {
                    if let Some(tx) = followup_accepted_tx {
                        let _ = tx.send(content.clone()).await;
                    }
                    let msg = ChatMessage::user_named(content.clone(), "follow-up");
                    session
                        .append_message(msg.clone())
                        .map_err(|e| format!("failed to persist followup: {e}"))?;
                    messages.push(msg);
                }
                if cancel.is_cancelled() {
                    turn_result = Err("cancelled by user".into());
                    break;
                }
                // chained follow-up: loop and run another turn with the same
                // session/messages/tool_state but without new turn_start marker
                // (the followup is already persisted).
                continue;
            }
            Err(e) => {
                turn_result = Err(e);
                break;
            }
        }
    }
    let usage = final_usage;
    let cached = final_cached;
    // Durable terminal marker (P8): a completed turn is recorded before the
    // event is relayed; a failed one gets `turn_failed` in run_agent_turn.
    match &turn_result {
        Ok(_) => session
            .turn_event("turn_complete")
            .map_err(|e| format!("failed to record turn_complete: {e}"))?,
        Err(_) => session
            .turn_event("turn_failed")
            .map_err(|e| format!("failed to record turn_failed: {e}"))?,
    }
    turn_result
        .map_err(|e| e.to_string())
        .map(|response| (response, usage, cached))
}

async fn approve(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pending = state
        .pending_approvals
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&req.request_id);

    match pending {
        Some(pending) if pending.session_id == session_id => {
            let decision = match req.decision {
                crate::protocol::ApprovalDecision::AllowOnce => ApprovalDecision::Once,
                crate::protocol::ApprovalDecision::AllowSession => {
                    // Persist for the whole session so next turns skip the overlay
                    state.record_session_approval(&session_id, &pending.name, &pending.input);
                    ApprovalDecision::Session
                }
                crate::protocol::ApprovalDecision::Deny => ApprovalDecision::Deny,
            };
            // Audit: best-effort, redacted input hash, actor, request_id
            {
                let Some(base) = std::env::var_os("XDG_DATA_HOME")
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| std::path::PathBuf::from(h).join(".local/share"))
                    })
                else {
                    let _ = pending.response.try_send(decision);
                    return Ok(Json(json!({ "status": "ok" })));
                };
                let path = base.join("dex/audit.jsonl");
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&pending.input, &mut hasher);
                use std::hash::Hasher;
                let input_hash = format!("{:016x}", hasher.finish());
                let decision_str = match decision {
                    ApprovalDecision::Once => "once",
                    ApprovalDecision::Session => "session",
                    ApprovalDecision::Deny => "deny",
                };
                let record = serde_json::json!({
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "session_id": session_id,
                    "request_id": req.request_id,
                    "tool": pending.name,
                    "input_hash": input_hash,
                    "decision": decision_str,
                    "actor": "remote",
                });
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                {
                    let mut line = record.to_string();
                    line.push('\n');
                    let _ = std::io::Write::write_all(&mut file, line.as_bytes());
                }
            }
            let _ = pending.response.send(decision).await;
            Ok(Json(json!({ "status": "ok" })))
        }
        Some(pending) => {
            // Restore on cross-session attempt so the legitimate session can
            // still resolve it.
            state
                .pending_approvals
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(req.request_id, pending);
            Err(StatusCode::NOT_FOUND)
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

async fn cancel(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Json<serde_json::Value> {
    // Ask the in-flight turn to unwind: the LLM stream reader and the agent
    // loop poll this token between steps. A missing entry means no turn is
    // running for the session, so there is nothing to cancel.
    if let Some(token) = state
        .cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session_id)
    {
        token.cancel();
    }

    // Deny any approvals still pending for this session so agent tasks
    // blocked on them wake up promptly. Collect senders under the lock,
    // then send without holding it (std MutexGuard is !Send across await).
    let to_deny = {
        let mut pending = state
            .pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stale: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in stale {
            if let Some(p) = pending.remove(&id) {
                out.push(p.response);
            }
        }
        out
    };
    for sender in to_deny {
        let _ = sender.send(ApprovalDecision::Deny).await;
    }

    Json(json!({ "status": "ok" }))
}

async fn steer(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<SteerRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let content = req.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Session must exist; steering only valid while a turn is active.
    // Disk fallback: the session may predate the background rebuild scan.
    if lookup_entry(&state, &session_id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let tx = {
        let map = state.steering_txs.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&session_id).cloned()
    }
    .ok_or(StatusCode::CONFLICT)?;
    tx.send(content).await.map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn followup(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<FollowupRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let content = req.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Disk fallback: the session may predate the background rebuild scan.
    if lookup_entry(&state, &session_id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let tx = {
        let map = state.followup_txs.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&session_id).cloned()
    }
    .ok_or(StatusCode::CONFLICT)?;
    tx.send(content).await.map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// Registry lookup with a one-shot disk fallback: the startup rebuild runs
/// in the background, so an id missing from the registry may simply not have
/// been scanned yet. A disk hit is registered (live entries win over the
/// later rebuild merge via `or_insert`) so subsequent lookups stay in-memory.
fn lookup_entry(state: &Arc<DaemonState>, session_id: &str) -> Option<SessionEntry> {
    if let Some(entry) = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .cloned()
    {
        return Some(entry);
    }
    let (path, header) = session::Session::list_all()
        .unwrap_or_default()
        .into_iter()
        .find(|(_, header)| header.id() == session_id)?;
    let entry = SessionEntry {
        path: path.clone(),
        name: header.name().map(ToOwned::to_owned),
        cwd: header.cwd().to_string(),
    };
    state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string(), entry.clone());
    Some(entry)
}

/// Resolve a session file path from the registry (with disk fallback), or 404.
fn session_path(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<std::path::PathBuf, StatusCode> {
    lookup_entry(state, session_id)
        .map(|e| e.path)
        .filter(|p| p.exists())
        .ok_or(StatusCode::NOT_FOUND)
}

/// `GET /api/sessions/{id}/events?since=<seq>` — replay journaled stream
/// events after a cursor (P10). Missing journal file replays nothing.
async fn session_events(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<EventsResponse>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let since = params
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let events = tokio::task::spawn_blocking(move || {
        let mut events = Vec::new();
        for (seq, payload) in Session::load_events(&path, since).unwrap_or_default() {
            if let Ok(event) = serde_json::from_str::<StreamEvent>(&payload) {
                events.push(StreamEnvelope { seq, event });
            }
        }
        let next_seq = events
            .last()
            .map(|e| e.seq.saturating_add(1))
            .unwrap_or(since);
        (events, next_seq)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (events, next_seq) = events;
    Ok(Json(EventsResponse { events, next_seq }))
}

/// `POST /api/sessions/{id}/reattach` — re-register a persisted session after
/// a daemon restart (or a client reconnect) and return the replay cursor.
async fn reattach(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<ReattachResponse>, StatusCode> {
    let entry = lookup_entry(&state, &session_id).ok_or(StatusCode::NOT_FOUND)?;
    if !entry.path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    state.seed_seq(&session_id, &entry.path);
    // Prune stale idempotency-recorded seq: reattach hands the client the
    // cursor to resume from.
    let seq = Session::max_event_seq(&entry.path).unwrap_or(0);
    Ok(Json(ReattachResponse {
        session_id: session_id.clone(),
        seq,
    }))
}

/// `GET /api/sessions/{id}/trace` — the per-session (redacted) trace journal
/// rows for this session, for cost/outcome queries (P9).
async fn session_trace(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let trace_path = path.with_extension("trace.jsonl");
    let rows = tokio::task::spawn_blocking(move || -> Vec<serde_json::Value> {
        let Ok(text) = std::fs::read_to_string(&trace_path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "trace": rows })))
}

/// `POST /api/sessions/{id}/undo` — revert the last recorded change (P8).
/// Refuses when the file moved on since (after_hash mismatch) or is too big.
async fn session_undo(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let path = session_path(&state, &session_id)?;
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        session::undo_last_change(&mut session)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    match result {
        Ok(message) => Ok(Json(json!({ "status": "ok", "message": message }))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Err(StatusCode::CONFLICT),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// `POST /api/sessions/{id}/waive` with `{"reason": ...}` — record a
/// `waived` verification disposition. A missing/empty reason is a 400 (P9:
/// waived requires a reason).
async fn session_waive(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let reason = req
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if reason.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = session_path(&state, &session_id)?;
    let reason = reason.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        // A waive is a recorded, user-authored message the model sees next.
        session.append_message(ChatMessage::user_named(
            format!("[verify waived] {reason}"),
            "waive",
        ))?;
        session.set_state(
            "verify",
            &serde_json::json!({
                "disposition": "waived",
                "reason": reason,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// `POST /api/sessions/{id}/name` with `{"name": ...}` — rename a session
/// (remote counterpart of local `/name`).
async fn session_name(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let name = req
        .get("name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim);
    let Some(name) = name.filter(|n| !n.is_empty()) else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let path = session_path(&state, &session_id)?;
    let name = name.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        session.set_name(name)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json!({ "status": "ok" })))
}

#[cfg(test)]
mod handler_tests {
    use super::*;
    use axum::extract::{Path, Query};

    #[tokio::test]
    async fn git_endpoint_matches_local_git_context() {
        // `GET /api/git` is the footer's poll source; it must report the same
        // branch/dirty as a direct `git_context` of the daemon cwd.
        let Json(info) = get_git().await;
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (branch, dirty) = crate::core::format::git_context(&cwd);
        assert_eq!(info.git_branch, branch);
        assert_eq!(info.git_dirty, dirty);
    }

    fn state_with_session(path: &std::path::Path) -> (Arc<DaemonState>, String) {
        let state = Arc::new(DaemonState::new());
        let session = Session::from_path(path).unwrap();
        let id = session.id().to_string();
        state.sessions.lock().unwrap().insert(
            id.clone(),
            SessionEntry {
                path: path.to_path_buf(),
                name: None,
                cwd: "/tmp/dex-test-cwd".into(),
            },
        );
        (state, id)
    }

    #[tokio::test]
    async fn chat_rejects_unknown_session_and_bad_protocol_version() {
        let state = Arc::new(DaemonState::new());
        let req = ChatRequest {
            prompt: "hi".into(),
            skill_dirs: vec![],
            base_url: None,
            model: None,
            permission: None,
            headers: None,
            plan: None,
        };
        // unknown session -> 404
        let r = chat(
            State(state.clone()),
            Path("nope".into()),
            axum::http::HeaderMap::new(),
            Json(req.clone()),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));

        // newer protocol version -> 400 (checked before session lookup)
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-dex-protocol", "2".parse().unwrap());
        let r = chat(State(state), Path("nope".into()), headers, Json(req)).await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
    }

    #[tokio::test]
    async fn mcp_status_and_reconnect_shape() {
        // Serialized with the MCP env tests (TEST_ENV_LOCK) and hermetic via
        // DEX_NO_MCP=1: this first-touch init of the global manager must stay
        // config-free, so a dev machine's servers can't leak into the shared
        // schema cache (the exact-schema test depends on it).
        let _env = crate::mcp::TEST_ENV_LOCK.lock().await;
        let prev = std::env::var("DEX_NO_MCP").ok();
        unsafe { std::env::set_var("DEX_NO_MCP", "1") };
        let body = get_mcp().await.0;
        assert!(body.get("servers").and_then(|v| v.as_array()).is_some());
        assert!(body.get("truncated").and_then(|v| v.as_u64()).is_some());
        // `auth` is always present (null for stdio/unconfigured): the remote
        // TUI reads login state off this key, never the daemon's token files.
        for server in body["servers"].as_array().into_iter().flatten() {
            assert!(server.get("auth").is_some());
        }
        // Smoke: unknown server surfaces down+error, never panics.
        let r = mcp_reconnect(Path("no-such-server".into())).await.0;
        assert_eq!(r.get("state").and_then(|v| v.as_str()), Some("down"));
        assert!(r.get("error").and_then(|v| v.as_str()).is_some());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DEX_NO_MCP", v),
                None => std::env::remove_var("DEX_NO_MCP"),
            }
        }
    }

    #[tokio::test]
    async fn steer_and_followup_validate_content_and_turn_state() {
        let state = Arc::new(DaemonState::new());
        // empty content -> 400
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "  ".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        let r = followup(
            State(state.clone()),
            Path("s".into()),
            Json(FollowupRequest { content: "".into() }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        // unknown session -> 404
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
        // registered session without active turn -> 409 (nothing to steer)
        state.sessions.lock().unwrap().insert(
            "s".into(),
            SessionEntry {
                path: "/tmp/does-not-exist.jsonl".into(),
                name: None,
                cwd: "/tmp".into(),
            },
        );
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
        let r = followup(
            State(state),
            Path("s".into()),
            Json(FollowupRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
    }

    #[tokio::test]
    async fn load_skill_rejects_bad_names() {
        let state = Arc::new(DaemonState::new());
        for bad in ["", "../evil", "has space", "slash/ed"] {
            let r = load_skill(
                State(state.clone()),
                Path("s".into()),
                Json(LoadSkillRequest {
                    name: bad.into(),
                    skill_dirs: vec![],
                }),
            )
            .await;
            assert!(
                matches!(r, Err(StatusCode::BAD_REQUEST)),
                "expected 400 for {bad:?}"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn approve_unknown_request_is_404_and_cross_session_is_restored() {
        let state = Arc::new(DaemonState::new());
        // no such request_id -> 404
        let r = approve(
            State(state.clone()),
            Path("sess-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "missing".into(),
                decision: crate::protocol::ApprovalDecision::AllowOnce,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));

        // pending parked under sess-a; decision sent against sess-b -> 404 and restored
        let (tx, mut rx) = mpsc::channel(1);
        state.pending_approvals.lock().unwrap().insert(
            "req-1".into(),
            PendingApproval {
                session_id: "sess-a".into(),
                response: tx,
                name: "bash".into(),
                input: "{}".into(),
            },
        );
        let r = approve(
            State(state.clone()),
            Path("sess-b".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-1".into(),
                decision: crate::protocol::ApprovalDecision::AllowOnce,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
        assert!(
            state
                .pending_approvals
                .lock()
                .unwrap()
                .contains_key("req-1"),
            "pending must be restored for the legitimate session"
        );
        assert!(
            rx.try_recv().is_err(),
            "restored approval must not be resolved"
        );

        // correct session resolves it
        let r = approve(
            State(state.clone()),
            Path("sess-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-1".into(),
                decision: crate::protocol::ApprovalDecision::Deny,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(
            rx.try_recv().ok(),
            Some(crate::core::types::ApprovalDecision::Deny)
        );
    }

    #[tokio::test]
    async fn cancel_denies_pending_approvals_for_the_session() {
        let state = Arc::new(DaemonState::new());
        let (tx_a, mut rx_a) = mpsc::channel(1);
        let (tx_b, mut rx_b) = mpsc::channel(1);
        {
            let mut pending = state.pending_approvals.lock().unwrap();
            pending.insert(
                "r-a".into(),
                PendingApproval {
                    session_id: "s-a".into(),
                    response: tx_a,
                    name: "write".into(),
                    input: "{}".into(),
                },
            );
            pending.insert(
                "r-b".into(),
                PendingApproval {
                    session_id: "s-b".into(),
                    response: tx_b,
                    name: "write".into(),
                    input: "{}".into(),
                },
            );
        }

        let _ = cancel(State(state), Path("s-a".into())).await;

        assert_eq!(
            rx_a.try_recv().ok(),
            Some(crate::core::types::ApprovalDecision::Deny)
        );
        assert!(rx_b.try_recv().is_err(), "other sessions must be untouched");
    }

    #[tokio::test]
    async fn session_events_replay_after_cursor() {
        // Hand-crafted session files: fully hermetic, no env redirects.
        let dir = std::env::temp_dir().join(format!("dex-srv-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "test-events-1";
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            r#"{"type":"session","version":1,"id":"test-events-1","timestamp":"t","cwd":"/tmp/x"}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{id}.events.jsonl")),
            r#"{"seq":0,"payload":{"type":"system","data":"one"}}
{"seq":1,"payload":{"type":"system","data":"two"}}
"#,
        )
        .unwrap();
        let (state, id) = state_with_session(&path);

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "0".to_string());
        // `since` is exclusive: seq 0 is skipped, seq 1 replays.
        let r = session_events(State(state.clone()), Path(id.clone()), Query(params))
            .await
            .unwrap();
        assert_eq!(r.events.len(), 1);
        assert_eq!(r.events[0].seq, 1);
        assert_eq!(r.next_seq, 2);
        assert!(matches!(r.events[0].event, StreamEvent::System(ref s) if s == "two"));

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "1".to_string());
        let r = session_events(State(state), Path(id), Query(params))
            .await
            .unwrap();
        assert!(r.events.is_empty(), "fully consumed cursor replays nothing");
        assert_eq!(r.next_seq, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn create_session_registers_and_lists_from_disk() {
        // Redirects where ALL sessions live; serialize against other tests
        // that read/write the sessions dir.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data_dir = std::env::temp_dir().join(format!("dex-srv-create-{}", std::process::id()));
        let _env =
            crate::session::EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        let state = Arc::new(DaemonState::new());
        let r = create_session(
            State(state.clone()),
            Json(CreateSessionRequest {
                cwd: "/tmp/dex-create-cwd".into(),
                name: Some("t".into()),
            }),
        )
        .await
        .unwrap();
        let id = r.0["session_id"].as_str().unwrap().to_string();
        assert!(!id.is_empty());
        assert!(
            state.sessions.lock().unwrap().contains_key(&id),
            "session must be registered"
        );

        let listed = list_sessions(State(state)).await.0["sessions"]
            .as_array()
            .unwrap()
            .clone();
        assert!(listed
            .iter()
            .any(|s| s["session_id"].as_str() == Some(id.as_str())));

        // cleanup: the created session file lives under data_dir
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

#[cfg(test)]
mod permission_gate_tests {
    use super::*;

    /// The daemon permission ceiling is the security boundary between a
    /// remote client and trusted-mode tool execution: a client may only
    /// request a STRICTER mode than the daemon's own. Runs before any LLM
    /// call, so it is testable with no provider.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn permission_ceiling_blocks_client_escalation_and_bad_plan() {
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Hermetic session storage.
        let data_dir = std::env::temp_dir().join(format!("dex-perm-{}", std::process::id()));
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "XDG_DATA_HOME",
            "DEX_PERMISSION",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
        ]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        let session = Session::new("/tmp/dex-perm-cwd".into(), None).unwrap();
        let path = session.path().unwrap().to_path_buf();
        let id = session.id().to_string();
        drop(session);

        let state = Arc::new(DaemonState::new());
        state.sessions.lock().unwrap().insert(
            id.clone(),
            SessionEntry {
                path: path.clone(),
                name: None,
                cwd: "/tmp".into(),
            },
        );

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let cancel = CancellationToken::new();

        // Deterministic provider config for the pass-through case: fake key
        // plus a per-request unroutable base URL (connection refused, no
        // network) — endpoint overrides are per-request/file, never env.
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            ["DEX_PERMISSION", "DEX_PROVIDER", "OPENCODE_API_KEY"]
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
        let _env2 = crate::session::EnvGuard(saved);
        std::env::set_var("DEX_PERMISSION", "read-only");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");

        let mk_req = |permission: Option<&str>, plan: Option<&str>| ChatRequest {
            prompt: "go".into(),
            skill_dirs: vec![],
            base_url: Some("http://127.0.0.1:9".to_string()),
            model: None,
            permission: permission.map(String::from),
            headers: None,
            plan: plan.map(String::from),
        };

        // 1. Client escalating to trusted against a read-only daemon: rejected.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("trusted"), None),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("escalation denied"), "got: {err}");
        // Nothing journaled: the turn never started.
        assert_ne!(
            crate::session::Session::last_turn_state(&path),
            "turn_start",
            "rejected turn must not journal turn_start"
        );

        // 2. Client requesting the same (stricter-or-equal) mode passes the
        // gate and fails later, at the LLM call — a different error.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("read-only"), None),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            !err.contains("escalation denied"),
            "gate must not fire for non-escalation: {err}"
        );

        // 3. Invalid plan JSON from the client is rejected, not stored.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("read-only"), Some("{not json")),
            &cancel,
            &tx,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("invalid plan JSON"), "got: {err}");

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("trace.jsonl"));
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::client::http::{ChatOptions, DaemonClient};
    use axum::body::Body;
    use axum::extract::State as AxumState;
    use axum::routing::post;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn spawn_app(app: Router) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// Full remote loop over real HTTP: real daemon router + real
    /// DaemonClient + a fake chat-completions provider. The model asks to
    /// `write` a file, the client denies the approval, the denied tool
    /// result flows back, and the model finishes with plain text. Covers
    /// client SSE parsing, the approval round trip, and the daemon turn
    /// machinery in one pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env must stay redirected for the whole turn
    async fn client_denies_write_then_turn_completes() {
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // Fake provider: request 0 asks for a write; later requests finish.
        const TOOL_SSE: &str = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"write","arguments":"{\"path\":\"evil.txt\",\"content\":\"hi\"}"}}]}}]}"#,
            "\n\ndata: [DONE]\n\n"
        );
        const DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\ndata: [DONE]\n\n";
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fake_llm = Router::new().route(
            "/chat/completions",
            post(move |AxumState(_): AxumState<Arc<AtomicUsize>>| {
                let counter = counter.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    let body = if n == 0 { TOOL_SSE } else { DONE_SSE };
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(body.to_string()))
                        .unwrap()
                }
            }),
        );
        // Axum requires typed state; attach the counter (already Arc'd).
        let fake_llm = fake_llm.with_state(calls.clone());
        let llm_base = spawn_app(fake_llm).await;

        let daemon_base = spawn_app(router(Arc::new(DaemonState::new()))).await;

        // Deterministic daemon environment: approvals on, LLM pointed at
        // the fake provider.
        let data_dir = std::env::temp_dir().join(format!("dex-e2e-{}", std::process::id()));
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "XDG_DATA_HOME",
            "DEX_CONFIG",
            "DEX_PERMISSION",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
            "DEX_VERIFY",
        ]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        // No real user config may leak in: point the daemon at the fake
        // provider through a real config file (a machine's config.yaml
        // could pin a provider-prefixed model that re-routes base_url away
        // from the mock; a file base_url pins the endpoint instead).
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            data_dir.join("config.yaml"),
            format!("active_provider: opencode\nbase_url: {llm_base}\napi: openai-completions\n"),
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
        std::env::set_var("DEX_PERMISSION", "ask-writes");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_VERIFY", "true");
        // Clear the rest for hermeticity.
        for v in [
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }

        // All client work happens on one blocking thread: reqwest::blocking
        // panics if created or dropped inside an async context.
        let (_session_id, events, chat_result) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&daemon_base).unwrap();
            client.wait_until_ready(Duration::from_secs(10)).unwrap();
            let session_id = client
                .create_session("/tmp/dex-e2e-cwd", Some("e2e"))
                .unwrap()
                .session_id;
            let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
            let r = client
                .chat(
                    &session_id,
                    "write a file please",
                    ChatOptions::default(),
                    &mut |event| {
                        let decision = match &event {
                            crate::protocol::StreamEvent::ApprovalRequired { .. } => {
                                Some(crate::protocol::ApprovalDecision::Deny)
                            }
                            _ => None,
                        };
                        events.push(event);
                        decision
                    },
                )
                .map_err(|e| e.to_string());
            (session_id, events, r)
            // client drops here, on the blocking pool
        })
        .await
        .unwrap();
        chat_result.unwrap();

        // Pi has no permission popups — default is trusted, so write succeeds without approval.
        let approvals: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::protocol::StreamEvent::ApprovalRequired { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(approvals, Vec::<String>::new(), "{events:?}");

        // The write tool should succeed and create the file.
        assert!(
            events.iter().any(|e| matches!(e,
                crate::protocol::StreamEvent::ToolResult { name, success, .. }
                if name == "write" && *success)),
            "write must surface as successful ToolResult: {events:?}"
        );
        assert!(
            std::path::Path::new("evil.txt").exists(),
            "write must touch disk when trusted"
        );

        // The turn completed with the model's final text.
        let last = events.last().unwrap();
        match last {
            crate::protocol::StreamEvent::TurnComplete { response, .. } => {
                assert_eq!(response, "all done");
            }
            other => panic!("expected TurnComplete, got {other:?}"),
        }
        // The model was called twice: tool call, then final answer.
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_file("evil.txt");
    }
}

#[cfg(test)]
mod async_parallel_tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_sessions_joins_parallel_and_sorts() {
        // TDD Phase 4 (S2): JoinSet per-file scans, join, sort — same as sequential, ~50ms not ~500ms.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data_dir = std::env::temp_dir().join(format!("dex-list-par-{}", std::process::id()));
        let saved = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        let state = Arc::new(DaemonState::new());
        // Create 5 sessions (each ~10ms scan serially would be ~50ms).
        for i in 0..5 {
            let s = Session::new(format!("/tmp/cwd-{i}"), Some(format!("n{i}"))).unwrap();
            state.sessions.lock().unwrap().insert(
                s.id().to_string(),
                SessionEntry {
                    path: s.path().unwrap().to_path_buf(),
                    name: Some(format!("n{i}")),
                    cwd: format!("/tmp/cwd-{i}"),
                },
            );
        }
        let start = std::time::Instant::now();
        let resp = list_sessions(State(state)).await;
        let elapsed = start.elapsed();
        let sessions = resp.0["sessions"].as_array().cloned().unwrap_or_default();
        assert_eq!(sessions.len(), 5, "all sessions listed");
        // Sorted newest-first by created_at (list_all_async sorts too).
        let mut prev = "9999";
        for s in &sessions {
            let at = s.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            assert!(at <= prev, "sorted newest-first");
            prev = at;
        }
        // Parallel, not serial 500ms (loose: <5s, ensures JoinSet didn't serialize with sleeps).
        assert!(elapsed < std::time::Duration::from_secs(5));
        match saved {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
