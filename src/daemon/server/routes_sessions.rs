use super::super::lock_map;
use super::super::lookup::lookup_entry_async;
use super::super::lookup::session_path;
use super::super::DaemonState;
use super::super::SessionEntry;
use super::routes_chat::try_claim_slot;
use crate::protocol::ChatMessage;
use crate::protocol::CreateSessionRequest;
use crate::protocol::EventsResponse;
use crate::protocol::ReattachResponse;
use crate::protocol::StreamEnvelope;
use crate::protocol::StreamEvent;
use crate::session;
use crate::session::Session;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub(crate) async fn create_session(
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

pub(crate) async fn list_sessions(
    State(state): State<Arc<DaemonState>>,
) -> Json<serde_json::Value> {
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
// wake may still hold the slot while it unwinds; poll instead of blocking
// the handler on a condvar.
pub(crate) async fn steal_wake_and_claim(state: &Arc<DaemonState>, session_id: &str) -> bool {
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
pub(crate) async fn session_events(
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
pub(crate) async fn reattach(
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
pub(crate) async fn session_trace(
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
pub(crate) async fn session_undo(
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
pub(crate) async fn session_waive(
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
pub(crate) async fn session_name(
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
