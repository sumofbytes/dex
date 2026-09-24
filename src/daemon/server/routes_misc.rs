use super::super::lock_map;
use super::super::lookup::lookup_entry_async;
use super::super::required_token;
use super::super::DaemonState;
use crate::llm::config::LlmConfig;
use crate::protocol::ChatMessage;
use crate::protocol::DaemonInfo;
use crate::protocol::ExtensionRunRequest;
use crate::protocol::GitInfo;
use crate::protocol::LoadSkillRequest;
use crate::protocol::SkillInfo;
use crate::session::Session;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use dex_skills::discover_skills_async;
use dex_skills::discover_skills_fresh_async;
use dex_skills::skill_dirs;
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// Bearer-token gate: every `/api/*` route requires `Authorization: Bearer
/// <token>` when the daemon requires a token (non-loopback bind or an
/// explicit `DEX_DAEMON_TOKEN`). `/health` stays open so liveness checks and
/// `wait_until_ready` work before any credential is exchanged.
pub(crate) async fn require_bearer(
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
pub(crate) async fn log_requests(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if !crate::runtime::logging::enabled(crate::runtime::logging::Level::Info) {
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

pub(crate) async fn health(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
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
    let (branch, dirty) = crate::runtime::format_runtime::git_context_async(cwd).await;
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

/// `/api/config` shape when the daemon has no usable config yet: empty model
/// list and provider name (no built-in default — display-only, see
/// `display_config`). The live-config arm overwrites the derived fields.
fn default_daemon_info(
    cwd: String,
    git_branch: Option<String>,
    git_dirty: bool,
    permission: String,
) -> DaemonInfo {
    DaemonInfo {
        provider: String::new(),
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

async fn resolve_daemon_info_async(ceiling: crate::protocol::PermissionMode) -> DaemonInfo {
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
            let mut info =
                default_daemon_info(cwd, git_branch, git_dirty, ceiling.as_str().to_string());
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
            // so the client still renders. The ceiling is the daemon's own
            // resolved value, not a fresh env read (§3.1).
            default_daemon_info(cwd, git_branch, git_dirty, ceiling.as_str().to_string())
        }
    }
}

pub(crate) async fn get_config(State(state): State<Arc<DaemonState>>) -> Json<DaemonInfo> {
    // Async client is Clone (no blocking TLS init); cache hits are a mutex bump.
    Json(resolve_daemon_info_async(state.ceiling).await)
}

/// Lightweight footer poll: just the daemon workspace's branch/dirty, behind
/// the same 5s `cached_git_context_async` as `/api/config` so a 2s TUI poll costs
/// at most one `git` spawn per 5s — and never pays `LlmConfig::from_env`.
pub(crate) async fn get_git() -> Json<GitInfo> {
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

pub(crate) async fn get_mcp() -> Json<serde_json::Value> {
    // Reads the process-wide cache; never spawns, never blocks the loop.
    // `error` carries the last connect/probe failure so operators see *why*
    // a server is down; `truncated` counts schema-cap drops (see loop.rs).
    // `auth` carries the OAuth line for HTTP servers (`null` for stdio, which
    // needs no login, and for servers the daemon never configured).
    // Config loads once and maps over statuses (no N+1 reloads).
    let configs = crate::mcp::config::load_server_configs();
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
pub(crate) async fn mcp_reconnect(Path(server): Path<String>) -> Json<serde_json::Value> {
    match crate::mcp::global_manager().reconnect(&server).await {
        Ok(tools) => Json(json!({ "server": server, "state": "up", "tools": tools })),
        Err(error) => Json(json!({ "server": server, "state": "down", "error": error })),
    }
}

/// Loaded extension summaries for remote clients (`/extensions` in a
/// connected TUI reads this, never the client process's own manager — the
/// daemon is the process that dispatches `ext__*` tools and hooks).
pub(crate) async fn get_extensions() -> Json<serde_json::Value> {
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
pub(crate) async fn extensions_reload() -> Json<serde_json::Value> {
    crate::extensions::global_manager().reload().await;
    get_extensions().await
}

/// Run one registered extension slash command on the daemon process — the
/// remote TUI's `/<ext-cmd>` lands here, never on the client's local copy
/// (which owns neither the workspace nor the dispatching manager). Unknown
/// names are 404 so the caller falls back to "unknown command"; handler
/// failures are 200 with an `error` field so the Lua message survives.
pub(crate) async fn extensions_run(
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

pub(crate) async fn list_skills() -> Json<serde_json::Value> {
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

pub(crate) async fn load_skill(
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
