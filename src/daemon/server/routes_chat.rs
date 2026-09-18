use super::super::approvals::write_approval_audit;
use super::super::lock_map;
use super::super::lookup::lookup_entry_async;
use super::super::turn::run_agent_turn;
use super::super::DaemonState;
use super::routes_sessions::steal_wake_and_claim;
use crate::core::types::ApprovalDecision;
use crate::core::types::QueueMsg;
use crate::protocol::ApprovalResponse;
use crate::protocol::ChatRequest;
use crate::protocol::FollowupRequest;
use crate::protocol::RecallRequest;
use crate::protocol::SteerRequest;
use crate::protocol::StreamEnvelope;
use crate::runtime::console::CancellationToken;
use axum::extract::Path;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::sse::Sse;
use axum::Json;
use futures_core::Stream;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) async fn chat(
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

pub(crate) async fn approve(
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

pub(crate) async fn cancel(
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

pub(crate) async fn steer(
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

pub(crate) async fn followup(
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
pub(crate) async fn recall(
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
pub(crate) fn try_claim_slot(state: &Arc<DaemonState>, session_id: &str) -> bool {
    let mut active = lock_map(&state.active_turns);
    if active.contains(session_id) {
        return false;
    }
    active.insert(session_id.to_string());
    true
}

// Chat wins over an idle wake (§10b V1b): cancel the wake, then take the
// turn slot. Without a wake, a second chat 409s immediately. With one, the
