use std::collections::HashMap;
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

use crate::core::console::CancellationToken;
use crate::core::types::{ApprovalDecision, ChatMessage, QueueMsg};
use crate::llm::config::LlmConfig;
use crate::protocol::{
    ApprovalResponse, ChatRequest, CreateSessionRequest, DaemonInfo, EventsResponse,
    ExtensionRunRequest, FollowupRequest, GitInfo, LoadSkillRequest, ReattachResponse,
    RecallRequest, SkillInfo, SteerRequest, StreamEnvelope, StreamEvent,
};
use crate::session::{self, Session};
use crate::skills::{discover_skills_async, discover_skills_fresh_async, skill_dirs};

use super::approvals::write_approval_audit;
#[cfg(test)]
use super::lookup::{lookup_entry, persisted_current};
use super::lookup::{lookup_entry_async, session_path};
use super::shell::session_shell;
use super::turn::run_agent_turn;
#[cfg(test)]
use super::turn::{apply_thinking_override, run_turn_inner, TurnChannels};
#[cfg(test)]
use super::PendingApproval;
use super::{lock_map, required_token, DaemonState, SessionEntry};

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
                    // Constant-time-ish compare over the secret bytes.
                    // Length leaks (tokens are fixed-length UUIDs); content
                    // does not short-circuit.
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

/// Per-request log line for `DEX_LOG=info dex serve`:
/// `method path -> status (ms)`. Outermost layer, so rejected requests log too.
/// One info line per handled request. The capture allocates (method + path),
/// so it is skipped entirely when `DEX_LOG` would drop the line. On streaming
/// routes (SSE) the duration is time to response *headers* — the body keeps
/// streaming after this line is written.
async fn log_requests(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !crate::core::logging::enabled(crate::core::logging::Level::Info) {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    crate::log!(
        Info,
        "{} {path} -> {} ({:?})",
        method,
        response.status(),
        started.elapsed()
    );
    response
}

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/config", get(get_config))
        .route("/api/git", get(get_git))
        .route("/api/mcp", get(get_mcp))
        .route("/api/mcp/{server}/reconnect", post(mcp_reconnect))
        .route("/api/extensions", get(get_extensions))
        .route("/api/extensions/reload", post(extensions_reload))
        .route("/api/extensions/run", post(extensions_run))
        .route("/api/skills", get(list_skills))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/steer", post(steer))
        .route("/api/sessions/{id}/followup", post(followup))
        .route("/api/sessions/{id}/recall", post(recall))
        .route("/api/sessions/{id}/skill", post(load_skill))
        // P10: versioned reattach/replay, P9: trace, P8: undo.
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/reattach", post(reattach))
        .route("/api/sessions/{id}/trace", get(session_trace))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route("/api/sessions/{id}/waive", post(session_waive))
        .route("/api/sessions/{id}/name", post(session_name))
        .route("/api/sessions/{id}/shell", post(session_shell))
        .with_state(state)
        .layer(axum::middleware::from_fn(require_bearer))
        .layer(axum::middleware::from_fn(log_requests))
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
    if let Some(hit) = lock_map(CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new())))
        .get(cwd)
        .filter(|e| e.at.elapsed() < Duration::from_secs(5))
    {
        return (hit.branch.clone(), hit.dirty);
    }
    let (branch, dirty) = crate::core::format::git_context_async(cwd).await;
    let mut cache = lock_map(CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new())));
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

impl DaemonInfo {
    /// `/api/config` shape when the daemon has no usable config yet:
    /// provider/model from env fallbacks, empty model list. The live-config
    /// arm overwrites the derived fields on top of this.
    fn default_for(
        cwd: String,
        git_branch: Option<String>,
        git_dirty: bool,
        permission: String,
    ) -> Self {
        Self {
            provider: std::env::var("DEX_PROVIDER")
                .ok()
                .unwrap_or_else(|| "opencode".to_string()),
            model: "unknown".to_string(),
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

async fn resolve_daemon_info_async() -> DaemonInfo {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Git spawns and config resolution (file + catalog-index reads) are
    // independent — overlap them; both sit on TUI first-paint's critical path.
    let ((git_branch, git_dirty), config) = tokio::join!(
        cached_git_context_async(&cwd),
        LlmConfig::from_env_async(None, None, None, Vec::new())
    );
    match config {
        Ok(config) => {
            let mut info = DaemonInfo::default_for(
                cwd,
                git_branch,
                git_dirty,
                config.permission.as_str().to_string(),
            );
            info.thinking_warning = config.thinking_mismatch_warning();
            info.provider = config.provider.name().to_string();
            info.api = config.api.name().to_string();
            info.model = config.model;
            info.available_models = config.available_models;
            info.context_window = config.context_window;
            info.thinking_effort = config.thinking_effort;
            info
        }
        Err(_) => {
            // Config is incomplete (e.g. no API key yet); report what we can
            // so the client still renders.
            let permission = std::env::var("DEX_PERMISSION")
                .ok()
                .filter(|v| crate::core::types::PermissionMode::parse(v).is_ok())
                .unwrap_or_else(|| "ask-writes".to_string());
            DaemonInfo::default_for(cwd, git_branch, git_dirty, permission)
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

/// Loaded extension summaries for remote clients (`/extensions` in a
/// connected TUI reads this, never the client process's own manager — the
/// daemon is the process that dispatches `ext__*` tools and hooks).
async fn get_extensions() -> Json<serde_json::Value> {
    let extensions: Vec<serde_json::Value> = crate::extensions::loaded_summaries()
        .into_iter()
        .map(|(id, version, tools, events)| {
            json!({
                "id": id,
                "version": version,
                "tools": tools.len(),
                "tool_names": tools,
                "events": events,
            })
        })
        .collect();
    Json(json!({ "extensions": extensions }))
}

/// Explicit reload (plan §9 P2): the daemon rescans, unloads extensions
/// that vanished or lost consent, and reports the new set. The remote TUI's
/// `/extensions reload` lands here — the client-side reload cannot reach
/// the process that dispatches.
async fn extensions_reload() -> Json<serde_json::Value> {
    crate::extensions::global_manager().reload().await;
    get_extensions().await
}

/// Run one registered extension slash command on the daemon process — the
/// remote TUI's `/<ext-cmd>` lands here, never on the client's local copy
/// (which owns neither the workspace nor the dispatching manager). Unknown
/// names are 404 so the caller falls back to "unknown command"; handler
/// failures are 200 with an `error` field so the Lua message survives.
async fn extensions_run(
    Json(req): Json<ExtensionRunRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (ext_id, arg) = crate::extensions::command_list()
        .into_iter()
        .find(|(_, n, _)| *n == name)
        .map(|(ext, _, _)| (ext, req.arg.clone()))
        .ok_or(StatusCode::NOT_FOUND)?;
    let cancel = crate::agent::state::GlobalCancellation;
    match crate::extensions::run_command_global(&ext_id, &name, &arg, &cancel).await {
        Ok(output) => Ok(Json(json!({ "extension": ext_id, "output": output }))),
        Err(error) => Ok(Json(json!({ "extension": ext_id, "error": error }))),
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
    let entry = lookup_entry_async(&state, &session_id).await?;
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
            .append_message(&msg)
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
        model: None,
        wake_provider: None,
        wake_base_url: None,
        plan_persisted: None,
        model_persisted: None,
    };
    lock_map(&state.sessions).insert(session_id.clone(), entry);
    // A re-created id (server-minted, so effectively never) must not stick
    // in the §27 negative cache.
    lock_map(&state.missing_sessions).remove(session_id.as_str());

    Ok(Json(json!({
        "session_id": session_id,
        "path": path,
    })))
}

async fn list_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    // P10: disk-backed listing with JoinSet parallel per-session scans
    // (`spawn_blocking` per file, join, sort) — fixes the linear scan (S2).
    let listed = session::Session::list_all_async().await.unwrap_or_default();
    // §31: every same-workspace session shares one agents dir — count each
    // distinct dir once (header-only scan + per-child turn states, off the
    // axum worker) instead of once per session file.
    let mut child_dirs: Vec<std::path::PathBuf> = Vec::new();
    for (path, _) in &listed {
        let dir = session::Session::agents_dir(path);
        if !child_dirs.contains(&dir) {
            child_dirs.push(dir);
        }
    }
    let child_counts: HashMap<std::path::PathBuf, (usize, usize)> =
        tokio::task::spawn_blocking(move || {
            child_dirs
                .into_iter()
                .map(|dir| {
                    let counts = session::Session::count_children(&dir);
                    (dir, counts)
                })
                .collect()
        })
        .await
        .unwrap_or_default();
    let child_counts = std::sync::Arc::new(child_counts);
    // Per-session (message_count, turn_state) in parallel (spawn_blocking per file).
    let mut set = tokio::task::JoinSet::new();
    for (path, header) in listed {
        let child_counts = child_counts.clone();
        set.spawn(tokio::task::spawn_blocking(move || {
            // §31: one fused open+scan per file — no full message parse for
            // the count, no second pass for the turn state.
            let (message_count, turn_state) =
                crate::session::Session::scan_summary(&path).unwrap_or((0, "unknown".to_string()));
            // §16/Phase 8: child runs surface in the listing (the resume
            // picker shows them); loaders keep excluding `agents/*`.
            let dir = session::Session::agents_dir(&path);
            let (child_agents, interrupted_children) =
                child_counts.get(&dir).copied().unwrap_or((0, 0));
            (
                path,
                header,
                message_count,
                turn_state,
                child_agents,
                interrupted_children,
            )
        }));
    }
    let mut scanned = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Ok(v)) = r {
            scanned.push(v);
        }
    }
    // One sort over typed rows: timestamp desc, session id tie-break so the
    // order is deterministic. The in-memory no-file fallback (sort key ""
    // sorts last) joins the same list before the single sort pass.
    let mut rows: Vec<(String, String, serde_json::Value)> = scanned
        .into_iter()
        .map(
            |(path, header, message_count, turn_state, child_agents, interrupted_children)| {
                let name = header.name().map(|n| n.to_string());
                (
                    header.timestamp().to_string(),
                    header.id().to_string(),
                    json!({
                        "session_id": header.id(),
                        "path": path.display().to_string(),
                        "name": name,
                        "cwd": header.cwd(),
                        "created_at": header.timestamp(),
                        "message_count": message_count,
                        "turn_state": turn_state,
                        "child_agents": child_agents,
                        "interrupted_children": interrupted_children,
                    }),
                )
            },
        )
        .collect();
    // Preserve in-memory sessions that have no file yet (shouldn't happen);
    // disk rows win when an id is present in both.
    {
        let sessions = lock_map(&state.sessions);
        for (id, entry) in sessions.iter() {
            if rows
                .iter()
                .any(|(_, row_id, _)| row_id.as_str() == id.as_str())
            {
                continue;
            }
            rows.push((
                String::new(),
                id.clone(),
                json!({
                    "session_id": id,
                    "path": entry.path.to_string_lossy(),
                    "name": entry.name,
                    "cwd": entry.cwd,
                    "created_at": "",
                    "message_count": 0,
                    "turn_state": "unknown",
                    "child_agents": 0,
                    "interrupted_children": 0,
                }),
            ));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let sessions: Vec<serde_json::Value> = rows.into_iter().map(|(_, _, row)| row).collect();
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
    // Idempotency replay is routed through the SAME channel/stream as a live
    // turn (single return type below): the recorded terminal envelope is just
    // pushed and the stream closes. The request hash backs both idempotency
    // paths (replay lookup here, record after the turn) and neither runs
    // without a key — compute it only then, not on every chat.
    let request_hash: Option<u64> = idempotency_key.as_deref().map(|_| {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(&req).unwrap_or_default().hash(&mut h);
        h.finish()
    });
    let replay_envelope: Option<StreamEnvelope> = idempotency_key
        .as_deref()
        .and_then(|key| state.idempotent_replay(key, &session_id, request_hash?))
        .and_then(|terminal| serde_json::from_str::<StreamEnvelope>(&terminal).ok());
    // Reject concurrent turns on the same session up front so the
    // append-only session log stays consistent. A replay must not hold the
    // active-turn slot, so it is checked before registration.
    // Disk fallback: the session may predate the background rebuild scan.
    if lookup_entry_async(&state, &session_id).await.is_err() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Pre-create per-turn channels so `POST /steer` / `POST /followup`
    // have a target as soon as the turn is registered (avoids a race where
    // the client sends steering in the gap between `active_turns` insert and
    // the `spawn_blocking` thread creating its channels).
    let mut steering_rx_opt: Option<mpsc::Receiver<QueueMsg>> = None;
    let mut followup_rx_opt: Option<mpsc::Receiver<QueueMsg>> = None;
    let mut cancel_for_turn: Option<CancellationToken> = None;
    if replay_envelope.is_none() {
        // Chat wins over an idle wake (§10b V1b): steal the wake, then take
        // the turn slot. Without a wake, a second chat 409s immediately.
        if !steal_wake_and_claim(&state, &session_id).await {
            return Err(StatusCode::CONFLICT);
        }

        // Register a fresh per-turn cancellation token before spawning so a
        // /cancel arriving during turn setup is still observed. The agent loop
        // and the stream reader poll it; it never leaks across sessions or
        // later turns.
        let cancel = CancellationToken::new();
        lock_map(&state.cancel_tokens).insert(session_id.clone(), cancel.clone());
        cancel_for_turn = Some(cancel);
        // Steering / follow-up queues for this turn (mirrors old local
        // `event.rs` channels). Insert now so the HTTP handlers can push
        // immediately.
        let (steering_rx, followup_rx) = create_queue_pair(&state, &session_id);
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
                request_hash.unwrap_or(0),
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

/// Create a turn's steering/follow-up queue pair and register the senders
/// so the HTTP handlers (`/steer`, `/followup`) can push immediately.
pub(crate) fn create_queue_pair(
    state: &DaemonState,
    session_id: &str,
) -> (mpsc::Receiver<QueueMsg>, mpsc::Receiver<QueueMsg>) {
    let (steering_tx, steering_rx) = mpsc::channel::<QueueMsg>(16);
    let (followup_tx, followup_rx) = mpsc::channel::<QueueMsg>(16);
    lock_map(&state.steering_txs).insert(session_id.to_string(), steering_tx);
    lock_map(&state.followup_txs).insert(session_id.to_string(), followup_tx);
    (steering_rx, followup_rx)
}

async fn approve(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<ApprovalResponse>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pending = lock_map(&state.pending_approvals).remove(&req.request_id);

    match pending {
        Some(pending) if pending.session_id == session_id => {
            let decision = ApprovalDecision::from(req.decision);
            if decision == ApprovalDecision::Session {
                // Persist for the whole session so next turns skip the overlay
                state.record_session_approval(&session_id, &pending.name, &pending.input);
            }
            // Audit: best-effort, redacted input hash, actor, request_id.
            // A child-agent prompt records its label so the trail names the
            // requester (§12 V1b).
            write_approval_audit(
                &session_id,
                &req.request_id,
                &pending.name,
                &pending.input,
                decision.as_str(),
                "remote",
                pending.agent.as_deref(),
            );
            let _ = pending.response.send(decision).await;
            Ok(Json(json!({ "status": "ok" })))
        }
        Some(pending) => {
            // Restore on cross-session attempt so the legitimate session can
            // still resolve it.
            lock_map(&state.pending_approvals).insert(req.request_id, pending);
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
    // running for the session, so there is nothing to cancel. Cloned under
    // the lock, cancelled outside it — never hold the mutex across the call.
    if let Some(token) = lock_map(&state.cancel_tokens).get(&session_id).cloned() {
        token.cancel();
    }
    // Same for an in-flight `!` shell run (Esc cancels it).
    // One Esc cancels whatever is running; a turn and a shell overlap only
    // when the user explicitly started both.
    if let Some(token) = lock_map(&state.shell_tokens).get(&session_id).cloned() {
        token.cancel();
    }

    // Deny the parent turn's approvals still pending for this session so
    // agent tasks blocked on them wake up promptly. Child-agent approvals
    // are skipped — see `take_session_pendings` (§12 V1b).
    for sender in state.take_session_pendings(&session_id) {
        let _ = sender.send(ApprovalDecision::Deny).await;
    }

    Json(json!({ "status": "ok" }))
}

/// Select the per-turn queue for `session_id`: steering unless `followup`.
/// `None` when the live turn holds no queue yet — the caller surfaces that
/// as a 409 (nothing to steer/follow/recall against).
fn queue_tx(
    state: &DaemonState,
    session_id: &str,
    followup: bool,
) -> Option<mpsc::Sender<QueueMsg>> {
    let map = if followup {
        lock_map(&state.followup_txs)
    } else {
        lock_map(&state.steering_txs)
    };
    map.get(session_id).cloned()
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
    if lookup_entry_async(&state, &session_id).await.is_err() {
        return Err(StatusCode::NOT_FOUND);
    }
    let tx = queue_tx(&state, &session_id, false).ok_or(StatusCode::CONFLICT)?;
    tx.send(QueueMsg::Content(content))
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
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
    if lookup_entry_async(&state, &session_id).await.is_err() {
        return Err(StatusCode::NOT_FOUND);
    }
    let tx = queue_tx(&state, &session_id, true).ok_or(StatusCode::CONFLICT)?;
    tx.send(QueueMsg::Content(content))
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// `POST /api/sessions/{id}/recall` — cancel a queued steering/follow-up
/// message the daemon has not injected yet, so the client can edit it. The
/// recall rides the same per-turn queue as the item, so it is a no-op when the
/// item was already accepted at a model boundary (it is then part of the
/// transcript and re-rendered by `SteeringAccepted`/`FollowupAccepted`).
async fn recall(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<RecallRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let content = req.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if lookup_entry_async(&state, &session_id).await.is_err() {
        return Err(StatusCode::NOT_FOUND);
    }
    let tx = queue_tx(&state, &session_id, req.followup).ok_or(StatusCode::CONFLICT)?;
    tx.send(QueueMsg::Recall(content))
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    Ok(Json(json!({ "status": "ok" })))
}

/// Take the session's active-turn slot; `false` when a live turn holds it.
/// The registry guard dies inside this fn — it never spans an await.
fn try_claim_slot(state: &Arc<DaemonState>, session_id: &str) -> bool {
    let mut active = lock_map(&state.active_turns);
    if active.contains(session_id) {
        return false;
    }
    active.insert(session_id.to_string());
    true
}

// Chat wins over an idle wake (§10b V1b): cancel the wake, then take the
// turn slot. Without a wake, a second chat 409s immediately. With one, the
// wake may still hold the slot while it unwinds; poll instead of blocking
// the handler on a condvar.
async fn steal_wake_and_claim(state: &Arc<DaemonState>, session_id: &str) -> bool {
    let had_wake = state.cancel_wake(session_id).is_some();
    for _ in 0..250 {
        if try_claim_slot(state, session_id) {
            return true;
        }
        if !had_wake {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `POST /api/sessions/{id}/events` handler.
async fn session_events(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<EventsResponse>, StatusCode> {
    let path = session_path(&state, &session_id).await?;
    // Presence heartbeat (§10b V1b): every journal read counts as a client
    // listening; the idle wake fires only while this stays fresh.
    state.touch_client_seen(&session_id);
    let since = params
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // Page-limited serving (§1): giant journals stream as bounded pages the
    // replay loop drains with a paint between, instead of one huge slurp.
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(crate::session::EVENTS_PAGE_LIMIT);
    let events = tokio::task::spawn_blocking(move || {
        let mut events = Vec::new();
        let mut last_raw: Option<u64> = None;
        for (seq, payload) in Session::load_events(&path, since, limit).unwrap_or_default() {
            last_raw = Some(last_raw.map_or(seq, |m: u64| m.max(seq)));
            // Unknown event types are skipped for the payload (the client's
            // fallback rule) but still advance the cursor — otherwise a
            // poller would re-fetch the same range forever.
            if let Ok(event) = serde_json::from_str::<StreamEvent>(&payload) {
                events.push(StreamEnvelope { seq, event });
            }
        }
        // next_seq follows the raw journal rows (even unknown types), so the
        // client cursor keeps moving; with no rows at/after `since` the cursor
        // stays put so a later row can never be skipped. Cursor is the next
        // seq to serve (inclusive): `since=0` serves seq 0.
        let next_seq = last_raw.map_or(since, |m| m.saturating_add(1));
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
    let entry = lookup_entry_async(&state, &session_id).await?;
    if !entry.path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    state.seed_seq(&session_id, &entry.path);
    // Prune stale idempotency-recorded seq: reattach hands the client the
    // cursor to resume from (next seq to serve: max + 1, 0 when empty).
    let seq = Session::max_event_seq(&entry.path).map_or(0, |m| m.saturating_add(1));
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
    let path = session_path(&state, &session_id).await?;
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
    let path = session_path(&state, &session_id).await?;
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
    let path = session_path(&state, &session_id).await?;
    let reason = reason.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut session = Session::from_path(&path)?;
        // A waive is a recorded, user-authored message the model sees next.
        session.append_message(&ChatMessage::user_named(
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
    let path = session_path(&state, &session_id).await?;
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
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
            },
        );
        (state, id)
    }

    #[tokio::test]
    async fn shell_validates_runs_and_persists() {
        use crate::protocol::ShellRequest;
        let shell = |command: &str, excluded: bool| ShellRequest {
            command: command.into(),
            exclude_from_context: excluded,
        };
        let state = Arc::new(DaemonState::new());
        // Unknown session -> 404 (with a real command).
        let r = session_shell(
            State(state.clone()),
            Path("nope".into()),
            Json(shell("echo hi", false)),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
        // Empty command -> 400 before the session lookup.
        let r = session_shell(
            State(state.clone()),
            Path("nope".into()),
            Json(shell("   ", false)),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        // Hermetic session file: unique temp path, so parallel runs never
        // collide (no fixed name under target/).
        let path = std::env::temp_dir().join(format!(
            "dex-shell-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
              &path,
              "{\"type\":\"session\",\"version\":1,\"id\":\"test-shell-1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/dex-test-cwd\"}\n",
          )
          .unwrap();
        let (state, id) = state_with_session(&path);
        // A registered in-flight run makes a second one 409 (one bash
        // at a time; Esc cancels the first).
        state
            .shell_tokens
            .lock()
            .unwrap()
            .insert(id.clone(), crate::core::console::CancellationToken::new());
        let r = session_shell(
            State(state.clone()),
            Path(id.clone()),
            Json(shell("echo hi", false)),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
        state.shell_tokens.lock().unwrap().remove(&id);
        let r = session_shell(
            State(state.clone()),
            Path(id.clone()),
            Json(shell("echo hi", false)),
        )
        .await;
        let body = r.expect("shell run").0;
        assert!(body.success);
        assert!(body.output.contains("hi"));
        // The slot frees when the run finishes (back-to-back `!` works).
        assert!(!state.shell_tokens.lock().unwrap().contains_key(&id));
        // A failing command reports success=false with the exit marker.
        let r = session_shell(
            State(state.clone()),
            Path(id.clone()),
            Json(shell("exit 3", false)),
        )
        .await;
        let body = r.expect("shell run").0;
        assert!(!body.success);
        assert!(body.output.contains("[exit 3]"));
        // Runs persist to history and feed the next turn.
        let loaded = crate::session::load_messages_from_session(&path).unwrap();
        assert!(
            loaded
                .iter()
                .any(|m| m.content_str().contains("Ran `echo hi`")),
            "shell run persisted: {:?}",
            loaded
                .iter()
                .map(|m| m.content_str().to_string())
                .collect::<Vec<_>>(),
        );
        // `!!` persists too but stays out of the model-bound history.
        let r = session_shell(
            State(state.clone()),
            Path(id.clone()),
            Json(shell("echo secret", true)),
        )
        .await;
        assert!(r.expect("shell run").0.success);
        let loaded = crate::session::load_messages_from_session(&path).unwrap();
        assert!(loaded
            .iter()
            .any(|m| m.is_context_excluded() && m.content_str().contains("secret")),);
        let llm = crate::session::load_llm_messages_from_session(&path).unwrap();
        assert!(llm
            .iter()
            .any(|m| m.content_str().contains("Ran `echo hi`")));
        assert!(!llm.iter().any(|m| m.content_str().contains("secret")));
        // /cancel signals an in-flight shell run too (Esc cancels it).
        let token = crate::core::console::CancellationToken::new();
        state
            .shell_tokens
            .lock()
            .unwrap()
            .insert(id.clone(), token.clone());
        let _ = cancel(State(state.clone()), Path(id)).await;
        assert!(token.is_cancelled());
        let _ = std::fs::remove_file(&path);
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
            system_prompt: None,
            thinking_effort: None,
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
    #[allow(clippy::await_holding_lock)] // env must stay redirected across the reloads
    async fn extensions_endpoints_report_shape_and_reload() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
            .lock()
            .await;
        // Hermetic: the manager must not see the developer's real installs.
        let keys: [(&'static str, bool); 2] = [("XDG_CONFIG_HOME", true), ("XDG_DATA_HOME", true)];
        let saved = crate::session::EnvGuard(
            keys.iter()
                .map(|(k, _)| (*k, std::env::var_os(k)))
                .collect(),
        );
        let root = std::env::temp_dir().join(format!("dex-ext-api-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        // Fixture user-scope extension lands on disk via the XDG config dir.
        let ext_dir = root.join("config/dex/extensions/apix");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.yaml"),
            "manifest_version: 1\nid: apix\nversion: 0.1.0\ncapabilities: []\n",
        )
        .unwrap();
        std::fs::write(
            ext_dir.join("extension.lua"),
            "return function(dex)\n  dex.events.on(\"turn.start\", function(ctx, ev) end)\nend\n",
        )
        .unwrap();

        // Status before any load: shape present, fixture not loaded.
        let body = get_extensions().await.0;
        assert!(body.get("extensions").and_then(|v| v.as_array()).is_some());

        // Reload picks the fixture up and reports it loaded.
        let body = extensions_reload().await.0;
        let loaded: Vec<&serde_json::Value> = body["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["id"] == "apix")
            .collect();
        assert_eq!(loaded.len(), 1, "reload must load the fixture: {body}");
        assert_eq!(loaded[0]["tools"].as_u64(), Some(0));
        assert_eq!(
            loaded[0]["events"].as_array().and_then(|a| a.first()),
            Some(&serde_json::json!("turn.start"))
        );

        // Remove the fixture from disk: reload unloads it (reconcile).
        std::fs::remove_dir_all(&ext_dir).unwrap();
        let body = extensions_reload().await.0;
        assert!(
            !body["extensions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["id"] == "apix"),
            "vanished extension must unload on reload: {body}"
        );
        crate::extensions::global_manager().reset_for_tests().await;
        std::fs::remove_dir_all(&root).ok();
        drop(saved);
    }

    #[test]
    fn thinking_override_sets_clears_and_keeps_default() {
        // Pure override applied per turn: None keeps, "" clears, else sets.
        let mut config = LlmConfig {
            provider: crate::core::types::Provider::OpenCode,
            api_key: String::new(),
            base_url: String::new(),
            model: "m".into(),
            available_models: Vec::new(),
            endpoints: Default::default(),
            api: crate::core::types::ApiProtocol::Responses,
            account_id: None,
            thinking_effort: Some("low".into()),
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: crate::core::types::PermissionMode::Trusted,
            verify_command: None,
            extra_headers: Default::default(),
            global_headers: Default::default(),
            provider_entries: Default::default(),
            provider_headers: Default::default(),
            api_pinned: false,
            connect_timeout_secs: 10,
            request_timeout_secs: 300,
        };
        apply_thinking_override(&mut config, None);
        assert_eq!(config.thinking_effort.as_deref(), Some("low"));
        apply_thinking_override(&mut config, Some(""));
        assert!(config.thinking_effort.is_none());
        apply_thinking_override(&mut config, Some("high"));
        assert_eq!(config.thinking_effort.as_deref(), Some("high"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env lock guards the manager reset below
    async fn extensions_run_rejects_unknown_and_empty() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Empty name is a bad request, unknown names are 404 so the remote
        // TUI can fall back to "unknown command".
        let r = extensions_run(Json(ExtensionRunRequest {
            name: String::new(),
            arg: String::new(),
        }))
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        let r = extensions_run(Json(ExtensionRunRequest {
            name: "no-such-ext-command-xyz".into(),
            arg: String::new(),
        }))
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));
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
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
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
            State(state.clone()),
            Path("s".into()),
            Json(FollowupRequest {
                content: "x".into(),
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
        // recall mirrors steer/followup: empty -> 400, no active queue -> 409.
        let r = recall(
            State(state.clone()),
            Path("s".into()),
            Json(RecallRequest {
                content: "  ".into(),
                followup: false,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::BAD_REQUEST)));
        let r = recall(
            State(state.clone()),
            Path("s".into()),
            Json(RecallRequest {
                content: "x".into(),
                followup: false,
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::CONFLICT)));
        // With an active queue the recall lands as `Recall` on that channel.
        let (tx, mut rx) = mpsc::channel::<QueueMsg>(4);
        state.steering_txs.lock().unwrap().insert("s".into(), tx);
        let r = recall(
            State(state.clone()),
            Path("s".into()),
            Json(RecallRequest {
                content: "typo".into(),
                followup: false,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert!(matches!(rx.try_recv(), Ok(QueueMsg::Recall(c)) if c == "typo"));
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
    async fn steal_wake_and_claim_wins_over_a_live_wake() {
        // Chat-wins rule (§10b V1b): a user POST steals the idle wake — the
        // wake's token is cancelled and the slot lands on the user turn.
        let state = Arc::new(DaemonState::new());
        state.claim_wake("s-a").expect("seed a live wake");
        assert!(steal_wake_and_claim(&state, "s-a").await);
        assert!(
            state.cancel_wake("s-a").is_none(),
            "the stolen wake is gone"
        );
        assert!(
            state.active_turns.lock().unwrap().contains("s-a"),
            "the user turn holds the slot"
        );
    }

    #[tokio::test]
    async fn steal_wake_and_claim_yields_to_a_live_turn() {
        // No wake to steal and a turn holds the slot: the chat POST must
        // lose here too — it retries, it never force-breaks a live turn.
        let state = Arc::new(DaemonState::new());
        state.active_turns.lock().unwrap().insert("s-a".to_string());
        assert!(!steal_wake_and_claim(&state, "s-a").await);
        assert!(
            state.active_turns.lock().unwrap().contains("s-a"),
            "the live turn keeps the slot"
        );
    }

    #[tokio::test]
    async fn steal_wake_and_claim_claims_a_free_slot() {
        let state = Arc::new(DaemonState::new());
        assert!(steal_wake_and_claim(&state, "s-a").await);
        assert!(
            state.active_turns.lock().unwrap().contains("s-a"),
            "the slot is claimed"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn approve_unknown_request_is_404_and_cross_session_is_restored() {
        // Resolving the deny below writes an audit row from the ambient
        // XDG_DATA_HOME; hold the env lock so the row can't land in a test
        // that is concurrently redirecting it.
        let _lock = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
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
                agent_id: None,
                agent: None,
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
                    agent_id: None,
                    agent: None,
                },
            );
            pending.insert(
                "r-b".into(),
                PendingApproval {
                    session_id: "s-b".into(),
                    response: tx_b,
                    name: "write".into(),
                    input: "{}".into(),
                    agent_id: None,
                    agent: None,
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

    #[test]
    fn persisted_state_skip_logic() {
        // §28: identical + untouched → skip; anything else → write.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "dex-persisted-current-{}-{}.jsonl",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        // Never written → must write.
        assert!(!persisted_current(&None, &"v".to_string(), &path));
        std::fs::write(&path, "{}\n").unwrap();
        let at = std::fs::metadata(&path).unwrap().modified().unwrap();
        let persisted = Some(("v".to_string(), at));
        // Identical + untouched → skip the append.
        assert!(persisted_current(&persisted, &"v".to_string(), &path));
        // Changed value → write.
        assert!(!persisted_current(&persisted, &"w".to_string(), &path));
        // Same value but a stale mtime (someone rewrote the file) → write.
        assert!(!persisted_current(
            &Some(("v".to_string(), std::time::UNIX_EPOCH)),
            &"v".to_string(),
            &path
        ));
        // Missing file → write (same as before).
        std::fs::remove_file(&path).unwrap();
        assert!(!persisted_current(&persisted, &"v".to_string(), &path));
    }

    #[tokio::test]
    async fn cancel_leaves_child_agent_approvals_pending() {
        let state = Arc::new(DaemonState::new());
        let (tx_child, mut rx_child) = mpsc::channel(1);
        state.pending_approvals.lock().unwrap().insert(
            "r-child".into(),
            PendingApproval {
                session_id: "s-a".into(),
                response: tx_child,
                name: "write".into(),
                input: "{}".into(),
                agent_id: Some("s-a-0".into()),
                agent: Some("explorer".into()),
            },
        );

        let _ = cancel(State(state.clone()), Path("s-a".into())).await;

        // A background child outlives the parent turn (§12 V1b): its
        // approval stays parked and answerable instead of being denied with
        // the turn — denying it would strand a still-running child.
        assert!(
            state
                .pending_approvals
                .lock()
                .unwrap()
                .contains_key("r-child"),
            "child approval must survive parent cancel"
        );
        assert!(
            rx_child.try_recv().is_err(),
            "child approval must not be resolved by parent cancel"
        );
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
        // `since` is the next seq to serve (inclusive): seq 0 replays.
        let r = session_events(State(state.clone()), Path(id.clone()), Query(params))
            .await
            .unwrap();
        assert_eq!(r.events.len(), 2);
        assert_eq!(r.events[0].seq, 0);
        assert_eq!(r.events[1].seq, 1);
        assert_eq!(r.next_seq, 2);
        assert!(matches!(r.events[0].event, StreamEvent::System(ref s) if s == "one"));
        assert!(matches!(r.events[1].event, StreamEvent::System(ref s) if s == "two"));

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "2".to_string());
        let r = session_events(State(state), Path(id), Query(params))
            .await
            .unwrap();
        assert!(r.events.is_empty(), "fully consumed cursor replays nothing");
        assert_eq!(r.next_seq, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded test runtime; guard is intentional
    async fn create_session_registers_and_lists_from_disk() {
        // Redirects where ALL sessions live; serialize against other tests
        // that read/write the sessions dir.
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
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
    // ---- coverage fixes (docs/test-coverage.md §1) ----

    #[tokio::test]
    async fn steer_and_followup_land_on_the_turn_queue() {
        let state = Arc::new(DaemonState::new());
        state.sessions.lock().unwrap().insert(
            "s".into(),
            SessionEntry {
                path: "/tmp/does-not-exist.jsonl".into(),
                name: None,
                cwd: "/tmp".into(),
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
            },
        );
        let (stx, mut srx) = mpsc::channel::<QueueMsg>(4);
        let (ftx, mut frx) = mpsc::channel::<QueueMsg>(4);
        state.steering_txs.lock().unwrap().insert("s".into(), stx);
        state.followup_txs.lock().unwrap().insert("s".into(), ftx);
        // Content is trimmed before it lands on the queue.
        let r = steer(
            State(state.clone()),
            Path("s".into()),
            Json(SteerRequest {
                content: "  mid-turn note  ".into(),
            }),
        )
        .await;
        assert!(r.is_ok());
        assert!(
            matches!(srx.try_recv(), Ok(QueueMsg::Content(c)) if c == "mid-turn note"),
            "steer must land on the steering queue"
        );
        let r = followup(
            State(state),
            Path("s".into()),
            Json(FollowupRequest {
                content: "next turn".into(),
            }),
        )
        .await;
        assert!(r.is_ok());
        assert!(
            matches!(frx.try_recv(), Ok(QueueMsg::Content(c)) if c == "next turn"),
            "followup must land on the followup queue"
        );
    }

    #[tokio::test]
    async fn recall_reaches_the_followup_queue_too() {
        let state = Arc::new(DaemonState::new());
        state.sessions.lock().unwrap().insert(
            "s".into(),
            SessionEntry {
                path: "/tmp/does-not-exist.jsonl".into(),
                name: None,
                cwd: "/tmp".into(),
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
            },
        );
        let (tx, mut rx) = mpsc::channel::<QueueMsg>(4);
        state.followup_txs.lock().unwrap().insert("s".into(), tx);
        let r = recall(
            State(state),
            Path("s".into()),
            Json(RecallRequest {
                content: "typo".into(),
                followup: true,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert!(matches!(rx.try_recv(), Ok(QueueMsg::Recall(c)) if c == "typo"));
    }

    #[tokio::test]
    async fn cancel_cancels_the_turn_and_shell_tokens() {
        let state = Arc::new(DaemonState::new());
        let turn = CancellationToken::new();
        let shell = CancellationToken::new();
        state
            .cancel_tokens
            .lock()
            .unwrap()
            .insert("s".into(), turn.clone());
        state
            .shell_tokens
            .lock()
            .unwrap()
            .insert("s".into(), shell.clone());
        let _ = cancel(State(state), Path("s".into())).await;
        assert!(turn.is_cancelled(), "Esc must unwind the in-flight turn");
        assert!(shell.is_cancelled(), "Esc must cancel an in-flight `!` run");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
    async fn approve_allow_session_records_approval_and_writes_audit() {
        let _lock = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let data_dir = std::env::temp_dir().join(format!("dex-srv-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
                .into_iter()
                .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        let state = Arc::new(DaemonState::new());
        // Parent turn approves for the whole session: recorded so later turns
        // skip the overlay, and audited with actor "remote".
        let (tx, mut rx) = mpsc::channel(1);
        state.pending_approvals.lock().unwrap().insert(
            "req-s".into(),
            PendingApproval {
                session_id: "s-a".into(),
                response: tx,
                name: "bash".into(),
                input: "{}".into(),
                agent_id: None,
                agent: None,
            },
        );
        let r = approve(
            State(state.clone()),
            Path("s-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-s".into(),
                decision: crate::protocol::ApprovalDecision::AllowSession,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(rx.try_recv().ok(), Some(ApprovalDecision::Session));
        assert!(
            state.is_session_approved("s-a", "bash", "{}"),
            "AllowSession must persist for the session"
        );
        let audit_path = data_dir.join("dex/audit.jsonl");
        let text = std::fs::read_to_string(&audit_path).expect("audit row written");
        let row: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
        assert_eq!(row["session_id"], "s-a");
        assert_eq!(row["request_id"], "req-s");
        assert_eq!(row["tool"], "bash");
        assert_eq!(row["decision"], "session");
        assert_eq!(row["actor"], "remote");
        assert!(row["agent"].is_null());
        assert!(
            row["input_hash"].as_str().is_some_and(|h| h.len() == 16),
            "input is redacted to a hash: {row}"
        );

        // A child-agent decision carries the child's label into the audit
        // row (§12 V1b) and does NOT record a session approval.
        let (tx, mut rx_child) = mpsc::channel(1);
        state.pending_approvals.lock().unwrap().insert(
            "req-c".into(),
            PendingApproval {
                session_id: "s-a".into(),
                response: tx,
                name: "write".into(),
                input: r#"{"path":"x"}"#.into(),
                agent_id: Some("s-a-0".into()),
                agent: Some("explorer".into()),
            },
        );
        let r = approve(
            State(state.clone()),
            Path("s-a".into()),
            Json(crate::protocol::ApprovalResponse {
                request_id: "req-c".into(),
                decision: crate::protocol::ApprovalDecision::Deny,
            }),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(rx_child.try_recv().ok(), Some(ApprovalDecision::Deny));
        let text = std::fs::read_to_string(&audit_path).unwrap();
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2, "one row per resolution: {text}");
        assert_eq!(rows[1]["agent"], "explorer");
        assert_eq!(rows[1]["decision"], "deny");
        assert!(
            !state.is_session_approved("s-a", "write", r#"{"path":"x"}"#),
            "Deny must not grant anything"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn load_skill_finds_fresh_skills_and_persists_content() {
        let root = std::env::temp_dir().join(format!(
            "dex-srv-skill-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        let dir = root.join("skills");
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        let content = "---\nname: demo\ndescription: \"Test skill\"\n---\nBody";
        std::fs::write(dir.join("demo/SKILL.md"), content).unwrap();
        let path = std::env::temp_dir().join(format!(
            "dex-skill-load-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-skill-1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/dex-test-cwd\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&path);

        // Unknown name -> 404 even though the session exists.
        let r = load_skill(
            State(state.clone()),
            Path(id.clone()),
            Json(LoadSkillRequest {
                name: "missing".into(),
                skill_dirs: vec![dir.to_string_lossy().into_owned()],
            }),
        )
        .await;
        assert!(matches!(r, Err(StatusCode::NOT_FOUND)));

        // Explicit loads bypass the discovery cache, so a just-added skill
        // resolves immediately.
        let r = load_skill(
            State(state),
            Path(id),
            Json(LoadSkillRequest {
                name: "demo".into(),
                skill_dirs: vec![dir.to_string_lossy().into_owned()],
            }),
        )
        .await;
        let body = r.expect("skill loads").0;
        assert_eq!(body["name"], "demo");
        assert_eq!(body["description"], "Test skill");
        assert_eq!(body["content"], content);
        // The skill is persisted as a named message for the next turn.
        let loaded = crate::session::load_messages_from_session(&path).unwrap();
        assert!(
            loaded
                .iter()
                .any(|m| m.content_str().contains("--- Skill: demo ---")),
            "skill message persisted: {:?}",
            loaded.iter().map(|m| m.content_str()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn session_events_skip_unknown_types_but_advance_cursor() {
        let dir = std::env::temp_dir().join(format!("dex-srv-events-2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "test-events-2";
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            r#"{"type":"session","version":1,"id":"test-events-2","timestamp":"t","cwd":"/tmp/x"}
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{id}.events.jsonl")),
            r#"{"seq":0,"payload":{"type":"system","data":"one"}}
{"seq":1,"payload":{"type":"yet_unknown_kind","data":"skip me"}}
{"seq":2,"payload":{"type":"agent_recovered","agent_id":"sess-0","attempt":1,"mode":"resume","reason":"interrupted"}}
this line is torn and not json
{"seq":3,"payload":{"type":"system","data":"three"}}
"#,
        )
        .unwrap();
        let (state, id) = state_with_session(&path);

        let mut params = std::collections::HashMap::new();
        params.insert("since".to_string(), "0".to_string());
        let r = session_events(State(state), Path(id), Query(params))
            .await
            .unwrap();
        // Unknown event types are skipped for the payload but still advance
        // the cursor, and torn lines are dropped. (`since` is the inclusive
        // next-seq-to-serve cursor, so `since=0` serves seq 0 too.
        // `agent_recovered` is the wire type the supervision removal
        // deleted — old journals carrying it replay cleanly.)
        assert_eq!(r.events.len(), 2, "{:?}", r.events);
        assert_eq!(r.events[0].seq, 0);
        assert!(matches!(r.events[0].event, StreamEvent::System(ref s) if s == "one"));
        assert_eq!(r.events[1].seq, 3);
        assert!(matches!(r.events[1].event, StreamEvent::System(ref s) if s == "three"));
        assert_eq!(r.next_seq, 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_events_without_journal_replay_nothing() {
        let path = std::env::temp_dir().join(format!(
            "dex-srv-nojournal-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-nojournal\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&path);
        // No `since` param defaults to 0; no journal file replays nothing and
        // leaves the cursor at 0.
        let r = session_events(
            State(state),
            Path(id),
            Query(std::collections::HashMap::new()),
        )
        .await
        .unwrap();
        assert!(r.events.is_empty());
        assert_eq!(r.next_seq, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // single-threaded runtime; env must stay redirected
    async fn lookup_entry_disk_fallback_and_reattach_seed_from_disk() {
        let _lock = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let data_dir =
            std::env::temp_dir().join(format!("dex-srv-fallback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
                .into_iter()
                .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        let dir = data_dir.join("dex/sessions/fb");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fb-1.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"fb-1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp/fb-cwd\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("fb-1.events.jsonl"),
            r#"{"seq":0,"payload":{"type":"system","data":"a"}}
{"seq":1,"payload":{"type":"system","data":"b"}}
{"seq":2,"payload":{"type":"system","data":"c"}}
{"seq":3,"payload":{"type":"system","data":"d"}}
"#,
        )
        .unwrap();

        let state = Arc::new(DaemonState::new());
        // The startup rebuild runs in the background, so a registry miss
        // falls back to disk and registers the hit for later lookups.
        let entry = lookup_entry(&state, "fb-1").expect("session found on disk");
        assert!(entry.path.exists());
        assert_eq!(entry.cwd, "/tmp/fb-cwd");
        assert!(
            state.sessions.lock().unwrap().contains_key("fb-1"),
            "disk hit must register in memory"
        );
        assert!(lookup_entry(&state, "fb-2").is_none(), "unknown stays None");

        // §27: post-rebuild misses negative-cache instead of re-walking
        // the workspace on every probe.
        state.rebuild_complete.store(true, Ordering::Relaxed);
        assert!(lookup_entry(&state, "fb-2").is_none());
        assert!(lock_map(&state.missing_sessions).contains_key("fb-2"));

        // Reattach resolves the same disk fallback and returns the replay
        // cursor: the next seq to serve (max + 1, 0 when empty).
        let r = reattach(State(state.clone()), Path("fb-1".into()))
            .await
            .expect("reattach");
        assert_eq!(r.session_id, "fb-1");
        assert_eq!(r.seq, 4);
        assert!(matches!(
            reattach(State(state), Path("fb-2".into())).await,
            Err(StatusCode::NOT_FOUND)
        ));

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn session_trace_reads_rows_and_skips_torn_lines() {
        let dir = std::env::temp_dir().join(format!("dex-srv-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = "test-trace-1";
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-trace-1\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{id}.trace.jsonl")),
            r#"{"tool":"bash","ok":true}
not json
{"tool":"write","ok":false}
"#,
        )
        .unwrap();
        let (state, id) = state_with_session(&path);
        let Json(value) = session_trace(State(state.clone()), Path(id.clone()))
            .await
            .unwrap();
        let rows = value["trace"].as_array().expect("trace array");
        assert_eq!(rows.len(), 2, "torn lines are skipped: {rows:?}");
        assert_eq!(rows[0]["tool"], "bash");
        assert_eq!(rows[1]["tool"], "write");

        // A session without a trace journal reports an empty trace.
        let bare = std::env::temp_dir().join(format!(
            "dex-srv-notrace-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
            &bare,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-trace-2\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&bare);
        let Json(value) = session_trace(State(state), Path(id)).await.unwrap();
        assert!(value["trace"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&bare);
    }

    #[tokio::test]
    async fn session_undo_restores_and_refuses_conflicts() {
        let dir = std::env::temp_dir().join(format!(
            "dex-srv-undo-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("file.txt");
        std::fs::write(&target, "before").unwrap();
        let before_hash = crate::tools::hash_file(&target.to_string_lossy());
        std::fs::write(&target, "after").unwrap();
        let after_hash = crate::tools::hash_file(&target.to_string_lossy());
        let path = dir.join("test-undo-1.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-undo-1\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&path);
        {
            let mut session = Session::from_path(&path).unwrap();
            crate::session::record_change(
                &mut session,
                crate::session::make_change_record(
                    "write",
                    &target.to_string_lossy(),
                    Some("before"),
                    Some("after"),
                    &before_hash,
                    &after_hash,
                ),
            )
            .unwrap();
        }

        // Undo reverts the file and reports what it did.
        let Json(body) = session_undo(State(state.clone()), Path(id.clone()))
            .await
            .unwrap();
        assert_eq!(body["status"], "ok");
        assert!(
            body["message"].as_str().unwrap().contains("undid write"),
            "{}",
            body["message"]
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "before");

        // Ledger empty now -> 404 (nothing left to undo).
        assert!(matches!(
            session_undo(State(state.clone()), Path(id.clone())).await,
            Err(StatusCode::NOT_FOUND)
        ));

        // File moved on since the change -> 409, no silent revert.
        std::fs::write(&target, "two").unwrap();
        let after2 = crate::tools::hash_file(&target.to_string_lossy());
        {
            let mut session = Session::from_path(&path).unwrap();
            crate::session::record_change(
                &mut session,
                crate::session::make_change_record(
                    "edit",
                    &target.to_string_lossy(),
                    Some("before"),
                    Some("two"),
                    &before_hash,
                    &after2,
                ),
            )
            .unwrap();
        }
        std::fs::write(&target, "modified since").unwrap();
        assert!(matches!(
            session_undo(State(state), Path(id)).await,
            Err(StatusCode::CONFLICT)
        ));
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "modified since",
            "conflicting undo must not touch the file"
        );

        // Unknown session -> 404.
        assert!(matches!(
            session_undo(State(Arc::new(DaemonState::new())), Path("nope".into())).await,
            Err(StatusCode::NOT_FOUND)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_waive_requires_reason_and_records_disposition() {
        let path = std::env::temp_dir().join(format!(
            "dex-srv-waive-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-waive-1\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&path);
        // P9: waived requires a reason; missing or blank is a 400.
        for body in [json!({}), json!({"reason": "   "})] {
            let r = session_waive(State(state.clone()), Path(id.clone()), Json(body.clone())).await;
            assert!(
                matches!(r, Err(StatusCode::BAD_REQUEST)),
                "expected 400 for {body}"
            );
        }
        let r = session_waive(
            State(state),
            Path(id),
            Json(json!({"reason": "  flaky env " })),
        )
        .await
        .unwrap();
        assert_eq!(r["status"], "ok");
        // The reason is recorded as a user-authored message the model sees,
        // plus a verify-state disposition.
        let loaded = crate::session::load_messages_from_session(&path).unwrap();
        assert!(
            loaded
                .iter()
                .any(|m| m.content_str() == "[verify waived] flaky env"),
            "waive message persisted: {:?}",
            loaded.iter().map(|m| m.content_str()).collect::<Vec<_>>()
        );
        let state_map = crate::session::load_session_state(&path).unwrap();
        let verify = state_map.get("verify").expect("verify state written");
        assert!(verify.contains("\"disposition\":\"waived\""), "{verify}");
        assert!(verify.contains("flaky env"), "{verify}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn session_name_validates_and_renames() {
        let path = std::env::temp_dir().join(format!(
            "dex-srv-name-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"test-name-1\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        let (state, id) = state_with_session(&path);
        // Missing or blank names are a 400.
        for body in [json!({}), json!({"name": "   "})] {
            let r = session_name(State(state.clone()), Path(id.clone()), Json(body.clone())).await;
            assert!(
                matches!(r, Err(StatusCode::BAD_REQUEST)),
                "expected 400 for {body}"
            );
        }
        // Unknown session -> 404 before any rename.
        assert!(matches!(
            session_name(
                State(Arc::new(DaemonState::new())),
                Path("nope".into()),
                Json(json!({"name": "x"}))
            )
            .await,
            Err(StatusCode::NOT_FOUND)
        ));
        // Names are trimmed before they are recorded.
        let r = session_name(
            State(state),
            Path(id),
            Json(json!({"name": "  Fancy Name  "})),
        )
        .await
        .unwrap();
        assert_eq!(r["status"], "ok");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines()
                .any(|l| l.contains("\"type\":\"session_info\"") && l.contains("Fancy Name")),
            "rename persisted: {text}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn health_and_mcp_meta_endpoints_return_shapes() {
        let state = Arc::new(DaemonState::new());
        let Json(h) = health(State(state)).await;
        assert_eq!(h["status"], "ok");
        // A fresh daemon has no background rebuild in flight.
        assert_eq!(h["rebuild_complete"], false);

        let Json(mcp) = get_mcp().await;
        assert!(mcp["servers"].is_array());
        assert!(mcp["truncated"].is_u64());

        // Reconnecting an unconfigured server surfaces the error (state down)
        // instead of only recording `down`.
        let Json(re) = mcp_reconnect(Path("no-such-server".into())).await;
        assert_eq!(re["server"], "no-such-server");
        assert_eq!(re["state"], "down");
        assert!(re["error"].is_string());

        let Json(skills) = list_skills().await;
        assert!(skills["skills"].is_array());
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
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);

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
                model: None,
                wake_provider: None,
                wake_base_url: None,
                plan_persisted: None,
                model_persisted: None,
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
            system_prompt: None,
            thinking_effort: None,
        };

        // 1. Client escalating to trusted against a read-only daemon: rejected.
        let err = run_turn_inner(
            &state,
            &id,
            &mk_req(Some("trusted"), None),
            &cancel,
            &tx,
            TurnChannels::detached(),
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
            TurnChannels::detached(),
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
            TurnChannels::detached(),
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
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);

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
            format!("active_provider: opencode\nmodel: opencode/test-model\ncontext_window: 100000\nbase_url: {llm_base}\napi: openai-completions\n"),
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

        // ask-writes is enforced at dispatch: exactly one prompt for the
        // write (previously nothing ever sent on the approval channel, so
        // the write ran unprompted despite DEX_PERMISSION=ask-writes).
        let approvals: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                crate::protocol::StreamEvent::ApprovalRequired { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(approvals, vec!["write".to_string()], "{events:?}");

        // The client denied it: the tool fails closed and touches nothing.
        assert!(
            events.iter().any(|e| matches!(e,
                crate::protocol::StreamEvent::ToolResult { name, success, .. }
                if name == "write" && !*success)),
            "denied write must surface as failed ToolResult: {events:?}"
        );
        assert!(
            !std::path::Path::new("evil.txt").exists(),
            "denied write must not touch disk"
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

    /// Full delegation loop over real HTTP: the parent's model calls
    /// `delegate`, the child runs its OWN turn (own session, own history,
    /// same fake provider — requests are dispatched on the persona in the
    /// system prompt, so response order never races), and the child's
    /// completion notice drains into the NEXT user chat's turn context
    /// (§10b V1a). The child JSONL lands under `agents/` (§16).
    /// Agent lifecycle hooks fire around a real child run: the fixture
    /// extension records `agent.start`/`agent.end` through `dex.state`
    /// (policy-free) while a real delegation drives `child_run`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env must stay redirected for the whole turn
    async fn agent_lifecycle_hooks_fire_around_a_child_run() {
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
            .lock()
            .await;

        const DELEGATE_SSE: &str = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"delegate","arguments":"{\"agent\":\"explorer\",\"task\":\"find the gate\"}"}}]}}]}"#,
            "\n\ndata: [DONE]\n\n"
        );
        const PARENT_DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"delegated\"}}]}\n\ndata: [DONE]\n\n";
        const CHILD_DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"found it\"}}]}\n\ndata: [DONE]\n\n";

        let parent_calls = Arc::new(AtomicUsize::new(0));
        let fake_llm = Router::new()
            .route(
                "/chat/completions",
                post(
                    move |AxumState(state): AxumState<Arc<AtomicUsize>>, body: String| async move {
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        let system = parsed["messages"][0]["content"]
                            .as_str()
                            .unwrap_or_default();
                        let sse = if system.contains("You are an explorer") {
                            CHILD_DONE_SSE
                        } else {
                            let n = state.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                DELEGATE_SSE
                            } else {
                                PARENT_DONE_SSE
                            }
                        };
                        axum::http::Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(Body::from(sse.to_string()))
                            .unwrap()
                    },
                ),
            )
            .with_state(parent_calls.clone());
        let llm_base = spawn_app(fake_llm).await;
        let daemon_state = Arc::new(DaemonState::new());
        let daemon_base = spawn_app(router(daemon_state.clone())).await;

        let data_dir = std::env::temp_dir().join(format!("dex-aghook-{}", std::process::id()));
        let saved: Vec<(&'static str, Option<std::ffi::OsString>)> = [
            "XDG_CONFIG_HOME",
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
        std::env::set_var("XDG_CONFIG_HOME", data_dir.join("config"));
        std::fs::create_dir_all(data_dir.join("config/dex/extensions/aghook")).unwrap();
        std::fs::write(
            data_dir.join("config/dex/extensions/aghook/manifest.yaml"),
            "manifest_version: 1\nid: aghook\nversion: 0.1.0\ncapabilities: []\n",
        )
        .unwrap();
        std::fs::write(
            data_dir.join("config/dex/extensions/aghook/extension.lua"),
            concat!(
                "return function(dex)\n",
                "  dex.events.on(\"agent.start\", function(ctx, ev)\n",
                "    dex.state.set(\"started\", ev.agent)\n",
                "  end)\n",
                "  dex.events.on(\"agent.end\", function(ctx, ev)\n",
                "    dex.state.set(\"ended\", tostring(ev.ok))\n",
                "  end)\n",
                "end\n"
            ),
        )
        .unwrap();
        std::fs::write(
            data_dir.join("config.yaml"),
            format!("active_provider: opencode\nmodel: opencode/test-model\ncontext_window: 100000\nbase_url: {llm_base}\napi: openai-completions\n"),
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
        std::env::set_var("DEX_PERMISSION", "trusted");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_VERIFY", "true");
        for v in [
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }
        // The fixture loads into the same process-global manager the daemon
        // turns dispatch through.
        crate::extensions::global_manager().reload().await;

        let base = daemon_base.clone();
        let (session_id, chat_result) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&base).unwrap();
            client.wait_until_ready(Duration::from_secs(10)).unwrap();
            let session_id = client
                .create_session("/tmp/dex-aghook-cwd", Some("aghook"))
                .unwrap()
                .session_id;
            let r = client
                .chat(
                    &session_id,
                    "go explore",
                    ChatOptions::default(),
                    &mut |_event| None,
                )
                .map_err(|e| e.to_string());
            (session_id, r)
        })
        .await
        .unwrap();
        chat_result.unwrap();

        // Wait for the child's terminal state, then read the hook state file.
        // (The child session dir is under agents/; the hook state file is the
        // extension's own record — independent of journal timing.)
        let state_file = data_dir.join("dex/extensions/state/aghook.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        let state;
        loop {
            if let Ok(text) = std::fs::read_to_string(&state_file) {
                if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(&text) {
                    if map.contains_key("ended") {
                        state = map;
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(
                Instant::now() < deadline,
                "agent lifecycle state never written"
            );
        }
        assert_eq!(state.get("started"), Some(&serde_json::json!("explorer")));
        assert_eq!(state.get("ended"), Some(&serde_json::json!("true")));
        let _ = session_id;
        crate::extensions::global_manager().reset_for_tests().await;
        std::fs::remove_dir_all(&data_dir).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env must stay redirected for the whole turn
    async fn delegate_runs_child_and_notice_drains_next_turn() {
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);

        const DELEGATE_SSE: &str = concat!(
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"delegate","arguments":"{\"agent\":\"explorer\",\"task\":\"find where the gate lives\"}"}}]}}]}"#,
            "\n\ndata: [DONE]\n\n"
        );
        const PARENT_DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"delegated\"}}]}\n\ndata: [DONE]\n\n";
        const CHILD_DONE_SSE: &str = concat!(
            r#"data: {"choices":[{"delta":{"content":"the gate lives in src/tools"}}]}"#,
            // §18: the child's own per-call usage — its record_usage prices
            // it and the completion notice carries the tokens, so client-side
            // spend accounting stays honest.
            "\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":300}}",
            "\n\ndata: [DONE]\n\n"
        );

        // Provider calls: parent requests count up (first = delegate call);
        // child requests are recognized by the persona in their system
        // prompt. Every request body is captured for the drain assertion.
        let parent_calls = Arc::new(AtomicUsize::new(0));
        let requests: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let fake_llm = Router::new().route(
            "/chat/completions",
            post(
                move |AxumState(state): AxumState<Arc<AtomicUsize>>, body: String| {
                    let seen = seen.clone();
                    async move {
                        let parsed: serde_json::Value =
                            serde_json::from_str(&body).unwrap_or_default();
                        lock_map(&seen).push(parsed.clone());
                        let system = parsed["messages"][0]["content"]
                            .as_str()
                            .unwrap_or_default();
                        let sse = if system.contains("You are an explorer") {
                            CHILD_DONE_SSE
                        } else {
                            let n = state.fetch_add(1, Ordering::SeqCst);
                            if n == 0 {
                                DELEGATE_SSE
                            } else {
                                PARENT_DONE_SSE
                            }
                        };
                        axum::http::Response::builder()
                            .status(200)
                            .header("content-type", "text/event-stream")
                            .body(Body::from(sse))
                            .unwrap()
                    }
                },
            ),
        );
        // Axum requires typed state; attach the counter (already Arc'd).
        let fake_llm = fake_llm.with_state(parent_calls.clone());
        let llm_base = spawn_app(fake_llm).await;
        let daemon_state = Arc::new(DaemonState::new());
        let daemon_base = spawn_app(router(daemon_state.clone())).await;

        // Deterministic daemon environment: LLM pointed at the fake provider.
        let data_dir = std::env::temp_dir().join(format!("dex-deleg-{}", std::process::id()));
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
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            data_dir.join("config.yaml"),
            format!("active_provider: opencode\nmodel: opencode/test-model\ncontext_window: 100000\nbase_url: {llm_base}\napi: openai-completions\n"),
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
        std::env::set_var("DEX_PERMISSION", "ask-writes");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_VERIFY", "true");
        for v in [
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }

        // Chat 1: the parent delegates and finishes its own turn.
        let base1 = daemon_base.clone();
        let (session_id, events, chat_result) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&base1).unwrap();
            client.wait_until_ready(Duration::from_secs(10)).unwrap();
            let session_id = client
                .create_session("/tmp/dex-deleg-cwd", Some("deleg"))
                .unwrap()
                .session_id;
            let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
            let r = client
                .chat(
                    &session_id,
                    "go explore",
                    ChatOptions::default(),
                    &mut |event| {
                        events.push(event);
                        None
                    },
                )
                .map_err(|e| e.to_string());
            (session_id, events, r)
        })
        .await
        .unwrap();
        chat_result.unwrap();

        // The parent's turn saw the spawn and finished.
        assert!(
            events.iter().any(|e| matches!(e,
                crate::protocol::StreamEvent::System(text) if text.starts_with("[agent explorer:"))),
            "started line must be journaled: {events:?}"
        );
        match events.iter().find_map(|e| match e {
            crate::protocol::StreamEvent::TurnComplete { response, .. } => Some(response.clone()),
            _ => None,
        }) {
            Some(response) => assert_eq!(response, "delegated"),
            None => panic!("expected TurnComplete in {events:?}"),
        }
        // The child id comes from the started line.
        let agent_id = events
            .iter()
            .find_map(|e| match e {
                crate::protocol::StreamEvent::System(text)
                    if text.starts_with("[agent explorer:") =>
                {
                    Some(
                        text.trim_start_matches("[agent explorer:")
                            .trim_end_matches("] started")
                            .to_string(),
                    )
                }
                _ => None,
            })
            .expect("started line");

        // Wait for the child to reach its terminal state (deterministic:
        // the notice must be queued before the next chat drains it).
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if daemon_state
                .manager_for(&session_id)
                .status(&crate::agent::subagent::AgentId(agent_id.clone()))
                == Some(crate::agent::subagent::AgentState::Completed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(Instant::now() < deadline, "child never completed");
        }

        // Chat 2: the notice drains into this turn's LLM context.
        let (second_events, second_result) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&daemon_base).unwrap();
            let mut events: Vec<crate::protocol::StreamEvent> = Vec::new();
            let r = client
                .chat(
                    &session_id,
                    "did it finish?",
                    ChatOptions::default(),
                    &mut |event| {
                        events.push(event);
                        None
                    },
                )
                .map_err(|e| e.to_string());
            (events, r)
        })
        .await
        .unwrap();
        second_result.unwrap();
        match second_events.iter().find_map(|e| match e {
            crate::protocol::StreamEvent::TurnComplete { response, .. } => Some(response.clone()),
            _ => None,
        }) {
            Some(response) => assert_eq!(response, "delegated"),
            None => panic!("expected TurnComplete in {second_events:?}"),
        }

        // The second chat's LLM request carried the notice: the status line
        // plus the child's summary (what the parent consumes, §6).
        let bodies = lock_map(&requests).clone();
        let last = bodies.last().unwrap();
        let notice_messages: Vec<&serde_json::Value> = last["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains("finished completed"))
            })
            .collect();
        assert_eq!(notice_messages.len(), 1, "{last:?}");
        let notice = notice_messages[0]["content"].as_str().unwrap();
        assert!(
            notice.contains("[agent explorer:"),
            "status line prefix: {notice}"
        );
        // §18: the child's own spend rides the notice (deduped by seq on
        // replay, like every lifecycle line).
        assert!(
            notice.contains("finished completed · 1.5k tok"),
            "usage suffix: {notice}"
        );
        assert!(
            notice.contains("the gate lives in src/tools"),
            "child summary delivered: {notice}"
        );

        // §16: the child's own JSONL beside the parent's, with markers.
        let sessions_base = data_dir.join("dex/sessions");
        let mut child_files = Vec::new();
        let mut stack = vec![sessions_base];
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with("-explorer.jsonl"))
                    {
                        child_files.push(path);
                    }
                }
            }
        }
        assert_eq!(child_files.len(), 1, "one child JSONL under agents/");
        let child_text = std::fs::read_to_string(&child_files[0]).unwrap();
        assert!(
            child_text.contains("\"type\":\"turn_start\""),
            "{child_text}"
        );
        assert!(
            child_text.contains("\"type\":\"turn_complete\""),
            "{child_text}"
        );
        assert!(
            child_text.contains("find where the gate lives"),
            "child transcript carries the task: {child_text}"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }
    // ---- coverage fixes (docs/test-coverage.md §1) ----

    /// Bearer gate over real HTTP: `/health` stays open, every `/api/*` route
    /// requires `Authorization: Bearer <token>` when the daemon requires a
    /// token, and `/api/config` renders resolved daemon info on success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env + token global must stay pinned
    async fn bearer_gate_blocks_api_routes_and_config_reports_info() {
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let data_dir = std::env::temp_dir().join(format!("dex-e2e-bearer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        std::fs::create_dir_all(&data_dir).unwrap();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_PERMISSION",
            "DEX_DAEMON_TOKEN",
            "DEX_LOG",
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ]
        .iter()
        .map(|k| (*k, std::env::var_os(k)))
        .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        std::env::set_var("XDG_CACHE_HOME", data_dir.join("cache"));
        std::fs::write(
            data_dir.join("config.yaml"),
            "active_provider: opencode\napi: openai-completions\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_PERMISSION", "ask-writes");
        std::env::set_var("DEX_DAEMON_TOKEN", "s3cret-token");
        for v in [
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }
        // Exercise the per-request log line too (`DEX_LOG=info dex serve`).
        std::env::set_var("DEX_LOG", "info");
        crate::core::logging::init();

        std::env::set_var("DEX_DAEMON_TOKEN", "s3cret-token");
        crate::daemon::prepare_daemon_token(&"127.0.0.1:9".parse().unwrap());
        // Reset the pinned token even when an assertion panics: the global is
        // process-wide and would 401 every later daemon test in the binary.
        struct TokenReset;
        impl Drop for TokenReset {
            fn drop(&mut self) {
                crate::daemon::reset_daemon_token_for_tests();
            }
        }
        let _token_reset = TokenReset;
        assert_eq!(
            required_token().as_deref(),
            Some("s3cret-token"),
            "loopback bind with explicit DEX_DAEMON_TOKEN requires it"
        );

        let base = spawn_app(router(Arc::new(DaemonState::new()))).await;
        let http = crate::client::http::shared_async_client();
        let url = |path: &str| format!("{base}{path}");

        // Liveness stays open without credentials.
        let status = http.get(url("/health")).send().await.unwrap().status();
        assert_eq!(status, 200);

        // API routes without / with a wrong token -> 401.
        for auth in [None, Some("Bearer wrong")] {
            let mut r = http.get(url("/api/config"));
            if let Some(auth) = auth {
                r = r.header("authorization", auth);
            }
            let resp = r.send().await.unwrap();
            assert_eq!(resp.status(), 401, "auth {auth:?} must be rejected");
        }

        // The right token gets through; /api/config reports resolved info.
        let resp = http
            .get(url("/api/config"))
            .header("authorization", "Bearer s3cret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let info: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(info["provider"], "opencode");
        assert_eq!(info["permission"], "ask-writes");
        assert_eq!(
            info["cwd"],
            std::env::current_dir()
                .unwrap()
                .to_string_lossy()
                .to_string()
        );

        // Incomplete config still renders (best-effort info, no error).
        std::env::set_var("DEX_PROVIDER", "definitely-not-a-provider");
        let resp = http
            .get(url("/api/config"))
            .header("authorization", "Bearer s3cret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let info: serde_json::Value = resp.json().await.unwrap();
        assert!(info.get("provider").is_some());

        // Restore the log subscriber from the original env (the token global
        // is reset by the drop guard above; the env guard restores the
        // variables it pinned).
        drop(_env);
        crate::core::logging::init();
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    /// P10 idempotency over real HTTP: the same `Idempotency-Key` replays the
    /// recorded terminal event instead of re-running the turn (the model is
    /// NOT called a second time).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)] // env must stay redirected for the whole turn
    async fn idempotent_chat_replays_the_recorded_terminal_event() {
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        const DONE_SSE: &str =
            "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\ndata: [DONE]\n\n";
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fake_llm = Router::new().route(
            "/chat/completions",
            post(move |AxumState(_): AxumState<Arc<AtomicUsize>>| {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(Body::from(DONE_SSE.to_string()))
                        .unwrap()
                }
            }),
        );
        let fake_llm = fake_llm.with_state(calls.clone());
        let llm_base = spawn_app(fake_llm).await;
        let daemon_base = spawn_app(router(Arc::new(DaemonState::new()))).await;

        let data_dir = std::env::temp_dir().join(format!("dex-e2e-idem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_PERMISSION",
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
        std::env::set_var("XDG_CACHE_HOME", data_dir.join("cache"));
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            data_dir.join("config.yaml"),
            format!("active_provider: opencode\nmodel: opencode/test-model\ncontext_window: 100000\nbase_url: {llm_base}\napi: openai-completions\n"),
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_PERMISSION", "ask-writes");
        std::env::set_var("DEX_VERIFY", "false");
        for v in [
            "DEX_MODELS",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(v);
        }

        let calls_in = calls.clone();
        let (calls_after_first, replay_events) = tokio::task::spawn_blocking(move || {
            let client = DaemonClient::new(&daemon_base).unwrap();
            client.wait_until_ready(Duration::from_secs(10)).unwrap();
            let session_id = client
                .create_session("/tmp/dex-e2e-cwd", Some("idem"))
                .unwrap()
                .session_id;
            // First turn runs for real.
            let mut first = Vec::new();
            client
                .chat(
                    &session_id,
                    "hello",
                    ChatOptions {
                        idempotency_key: Some("key-1".into()),
                        ..Default::default()
                    },
                    &mut |event| {
                        first.push(event);
                        None
                    },
                )
                .unwrap();
            let calls_after_first = calls_in.load(Ordering::SeqCst);
            // Same key + same request body: replay, no second model call.
            let mut second = Vec::new();
            client
                .chat(
                    &session_id,
                    "hello",
                    ChatOptions {
                        idempotency_key: Some("key-1".into()),
                        ..Default::default()
                    },
                    &mut |event| {
                        second.push(event);
                        None
                    },
                )
                .unwrap();
            (calls_after_first, second)
        })
        .await
        .unwrap();

        assert!(calls_after_first >= 1, "the first turn runs for real");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_after_first,
            "the replay must not call the model again"
        );
        assert_eq!(
            replay_events.len(),
            1,
            "replay emits exactly the recorded terminal event: {replay_events:?}"
        );
        match &replay_events[0] {
            crate::protocol::StreamEvent::TurnComplete { response, .. } => {
                assert_eq!(response, "all done");
            }
            other => panic!("expected TurnComplete replay, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&data_dir);
    }
}

#[cfg(test)]
mod async_parallel_tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn list_sessions_joins_parallel_and_sorts() {
        // TDD Phase 4 (S2): JoinSet per-file scans, join, sort — same as sequential, ~50ms not ~500ms.
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
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
                    model: None,
                    wake_provider: None,
                    wake_base_url: None,
                    plan_persisted: None,
                    model_persisted: None,
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
