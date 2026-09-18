//! The agent-turn engine: queue plumbing, turn setup/teardown, and the
//! full `run_turn_inner` pipeline shared by chat turns and idle wakes.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::r#loop::{apply_queue_msg, process_turn, AgentRuntime};
use crate::agent::state::ToolState;
use crate::agent::subagent::{AgentTurnContext, WaitOutcome};
use crate::core::types::{ApprovalDecision, ApprovalRequest, ChatMessage, QueueMsg, SinkLine};
use crate::llm::config::LlmConfig;
use crate::llm::prompt::system_prompt_with_override_for;
use crate::protocol::{ChatRequest, StreamEnvelope, StreamEvent};
use crate::runtime::console::{CancellationToken, Console, TraceWriter};
use crate::runtime::unwind::CatchUnwind;
use crate::session::{self, Session};
use crate::skills::{discover_skills_async, skill_dirs};

use super::approvals::child_approval_bridge;
use super::lookup::persisted_current;
use super::server::create_queue_pair;
use super::{journal_event, lock_map, DaemonState, PendingApproval};

/// Turn outcome from the chained loop: final response text plus the
/// usage/cached token counts captured with it.
pub(crate) type TurnOutcome =
    Result<(String, Option<u64>, Option<u64>), Box<dyn std::error::Error + Send + Sync>>;

/// Per-turn queue plumbing: the steering/follow-up receivers the agent loop
/// drains, the accepted-notification senders the loop replies on, and the
/// bridge-drain signals the terminal event waits on.
pub(crate) struct TurnChannels {
    steering_rx: mpsc::Receiver<QueueMsg>,
    followup_rx: mpsc::Receiver<QueueMsg>,
    steering_accepted_tx: mpsc::Sender<String>,
    followup_accepted_tx: mpsc::Sender<String>,
    sink_done: tokio::sync::oneshot::Sender<()>,
    approval_done: tokio::sync::oneshot::Sender<()>,
}

impl TurnChannels {
    /// Fresh queues with no live counterpart — for direct `run_turn_inner`
    /// calls (tests) that never receive steering and never wait on bridges.
    #[cfg(test)]
    pub(crate) fn detached() -> Self {
        let (steering_tx, steering_rx) = mpsc::channel::<QueueMsg>(16);
        let (followup_tx, followup_rx) = mpsc::channel::<QueueMsg>(16);
        let (steering_accepted_tx, _) = mpsc::channel::<String>(16);
        let (followup_accepted_tx, _) = mpsc::channel::<String>(16);
        let (sink_done, _) = tokio::sync::oneshot::channel::<()>();
        let (approval_done, _) = tokio::sync::oneshot::channel::<()>();
        drop((steering_tx, followup_tx));
        TurnChannels {
            steering_rx,
            followup_rx,
            steering_accepted_tx,
            followup_accepted_tx,
            sink_done,
            approval_done,
        }
    }
}

/// Forward accepted steers/follow-ups onto the SSE stream so the remote TUI
/// can clear its `pending_*` badge and render the prompt. Journaled so a
/// reattach replay reconstructs the transcript.
pub(crate) fn spawn_accepted_forwarder(
    state: Arc<DaemonState>,
    session_id: String,
    tx: mpsc::Sender<StreamEnvelope>,
    mut rx: mpsc::Receiver<String>,
    event: fn(String) -> StreamEvent,
) {
    tokio::spawn(async move {
        while let Some(content) = rx.recv().await {
            let event = event(content);
            let seq = state.next_seq(&session_id);
            journal_event(&state, &session_id, seq, &event);
            let _ = tx.send(StreamEnvelope { seq, event }).await;
        }
    });
}

/// Run one agent turn and push numbered `StreamEnvelope`s into `tx`. Async:
/// spawned via `tokio::spawn`, bridges are tasks with `send().await`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_agent_turn(
    state: Arc<DaemonState>,
    session_id: String,
    req: ChatRequest,
    cancel: CancellationToken,
    tx: mpsc::Sender<StreamEnvelope>,
    idempotency_key: Option<String>,
    request_hash: u64,
    steering_rx: Option<mpsc::Receiver<QueueMsg>>,
    followup_rx: Option<mpsc::Receiver<QueueMsg>>,
) {
    // Use a guard so active_turns/cancel_tokens/pending approvals/steering
    // are cleaned even when run_turn_inner panics inside the spawned task
    // (`CatchUnwind` below still delivers `TurnFailed` in that case).
    struct TurnGuard {
        state: Arc<DaemonState>,
        session_id: String,
        cancel: CancellationToken,
        stream_tx: mpsc::Sender<StreamEnvelope>,
    }
    impl Drop for TurnGuard {
        fn drop(&mut self) {
            // Deny the parent turn's still-pending approvals so blocked agent
            // threads wake up promptly. Child-agent approvals are skipped —
            // see `take_session_pendings` (§12 V1b): the child outlives the
            // parent turn and must stay answerable.
            for sender in self.state.take_session_pendings(&self.session_id) {
                let _ = sender.try_send(ApprovalDecision::Deny);
            }
            // Tear down only the entries THIS turn registered: an idle wake
            // stolen by a user chat POST must not remove the user turn's
            // fresh registration (identity-checked via the cancel token).
            let owned = {
                let mut tokens = lock_map(&self.state.cancel_tokens);
                let owned = tokens
                    .get(&self.session_id)
                    .is_some_and(|token| token.same_token(&self.cancel));
                if owned {
                    tokens.remove(&self.session_id);
                }
                owned
            };
            if owned {
                lock_map(&self.state.active_turns).remove(&self.session_id);
                for map in [&self.state.steering_txs, &self.state.followup_txs] {
                    lock_map(map).remove(&self.session_id);
                }
                self.state
                    .unregister_stream(&self.session_id, &self.stream_tx);
            }
        }
    }
    let _guard = TurnGuard {
        state: state.clone(),
        session_id: session_id.clone(),
        cancel: cancel.clone(),
        stream_tx: tx.clone(),
    };
    // Journaled agent events (child lifecycle, labeled approvals, wake
    // turns) also ride this turn's live stream while it lasts (V1b).
    state.register_stream(&session_id, &tx);
    // Steering / follow-up channels for this turn (mirrors the old in-memory
    // `event.rs` submit path). `POST /steer` and `POST /followup` push into
    // these; the agent loop consumes them between iterations / chained turns.
    // When `chat` pre-creates the queues to avoid a race, reuse them; else
    // (direct `run_agent_turn` calls, e.g. tests) create them here.
    let (steering_rx, followup_rx) = match (steering_rx, followup_rx) {
        (Some(sr), Some(fr)) => (sr, fr),
        _ => create_queue_pair(&state, &session_id),
    };
    let (steering_accepted_tx, steering_accepted_rx) = mpsc::channel::<String>(16);
    let (followup_accepted_tx, followup_accepted_rx) = mpsc::channel::<String>(16);
    spawn_accepted_forwarder(
        state.clone(),
        session_id.clone(),
        tx.clone(),
        steering_accepted_rx,
        |content| StreamEvent::SteeringAccepted { content },
    );
    spawn_accepted_forwarder(
        state.clone(),
        session_id.clone(),
        tx.clone(),
        followup_accepted_rx,
        |content| StreamEvent::FollowupAccepted { content },
    );
    // Drain signals for the sink/approval bridges: both run concurrently
    // with this task and may still hold lines they received before the
    // console dropped. The terminal event must be the last one on the wire
    // (and the highest seq in the journal), or clients settle the turn
    // while straggler AssistantText/ToolResult events still arrive.
    let (sink_done_tx, sink_done_rx) = tokio::sync::oneshot::channel::<()>();
    let (approval_done_tx, approval_done_rx) = tokio::sync::oneshot::channel::<()>();
    let channels = TurnChannels {
        steering_rx,
        followup_rx,
        steering_accepted_tx,
        followup_accepted_tx,
        sink_done: sink_done_tx,
        approval_done: approval_done_tx,
    };
    let result: Result<(String, Option<u64>, Option<u64>), String> = match CatchUnwind::new(
        Box::pin(run_turn_inner(
            &state,
            &session_id,
            &req,
            &cancel,
            &tx,
            channels,
        )),
        "turn panicked",
    )
    .await
    {
        Ok(inner) => inner,
        Err(panicked) => Err(panicked),
    };
    // Drop the guard now before sending the terminal event so a new turn can
    // be accepted promptly; drop ordering handles pending approvals/active turns.
    drop(_guard);
    let _ = sink_done_rx.await;
    let _ = approval_done_rx.await;

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
    // Clone the path first: the guard must die with this statement —
    // `journal_event` re-locks `sessions`, and a guard held across the
    // `if let` body would self-deadlock (std Mutex is not reentrant).
    let turn_path = lock_map(&state.sessions)
        .get(&session_id)
        .map(|e| e.path.clone());
    if let Some(path) = turn_path {
        journal_event(&state, &session_id, seq, &env.event);
        // Durable turn_failed marker for runs that did not finish normally.
        if matches!(&env.event, StreamEvent::TurnFailed { .. })
            && crate::session::Session::last_turn_state(&path) == "interrupted"
        {
            if let Ok(mut journal) = Session::from_path(&path) {
                let _ = journal.turn_event("turn_failed");
            }
        }
    }
    if let Some(key) = idempotency_key {
        state.idempotency_record(&key, &session_id, request_hash, serialized);
    }
    let _ = tx.send(env).await;
}

/// Remote `/thinking` override: `None` keeps the daemon default, `Some("")`
/// is an explicit clear (unset), otherwise the level. This is what makes a
/// remote choice stick — without it the daemon would use its own file/env
/// and silently ignore the client's display. Pure so the override is
/// unit-testable without a turn.
pub(crate) fn apply_thinking_override(config: &mut LlmConfig, effort: Option<&str>) {
    match effort {
        None => {}
        Some("") => config.thinking_effort = None,
        Some(e) => config.thinking_effort = Some(e.to_string()),
    }
}

/// Deduped write-through of one session state key: skip when `cached` still
/// holds this value and the file is untouched since (the mtime guard — the
/// co-located TUI can write the same file directly, so an entry-only
/// comparison could skip a needed restore). Skipping also stops duplicate rows
/// accumulating for every `load_session_state` scan to walk. Returns true when
/// a write happened, so the caller can re-stat and refresh its cached slot
/// once any follow-up writes have landed.
fn persist_if_stale<V: PartialEq>(
    session: &mut Session,
    path: &std::path::Path,
    cached: Option<(V, std::time::SystemTime)>,
    key: &str,
    value: &str,
    new: &V,
) -> Result<bool, String> {
    if persisted_current(&cached, new, path) {
        return Ok(false);
    }
    session
        .set_state(key, value)
        .map_err(|e| format!("failed to persist {key}: {e}"))?;
    Ok(true)
}

pub(crate) async fn run_turn_inner(
    state: &Arc<DaemonState>,
    session_id: &str,
    req: &ChatRequest,
    cancel: &CancellationToken,
    tx: &mpsc::Sender<StreamEnvelope>,
    mut channels: TurnChannels,
) -> Result<(String, Option<u64>, Option<u64>), String> {
    let entry = {
        let sessions = lock_map(&state.sessions);
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
        let canonical = if plan_json.is_empty() {
            crate::core::types::Plan::default().to_json()
        } else {
            let plan: crate::core::types::Plan = serde_json::from_str(plan_json)
                .map_err(|e| format!("invalid plan JSON from client: {e}"))?;
            plan.to_json()
        };
        // §28: the client re-sends its plan on later turns; skip the append
        // when the daemon already persisted exactly this value and nobody
        // else touched the file since.
        let persisted = lock_map(&state.sessions)
            .get(session_id)
            .and_then(|e| e.plan_persisted.clone());
        if persist_if_stale(
            &mut session,
            &entry.path,
            persisted,
            "plan",
            &canonical,
            &canonical,
        )? {
            let at = std::fs::metadata(&entry.path).and_then(|m| m.modified());
            if let (Ok(at), Some(slot)) = (at, lock_map(&state.sessions).get_mut(session_id)) {
                slot.plan_persisted = Some((canonical, at));
            }
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
    // Complexity-router signal: history tokens come from the same
    // model-bound load the turn rebuilds below, so routing sees what the
    // turn will send. Loaded before the config build so the routed model
    // rides the single `from_env_async` — no second build.
    let history = if let Some(path) = session.path().map(|p| p.to_path_buf()) {
        tokio::task::spawn_blocking(move || {
            session::load_llm_messages_from_session(&path).unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    // Complexity router (V1): an explicit per-request model always wins;
    // otherwise the classified tier resolves through routing.balanced: →
    // top-level model:.
    let explicit_model = req.model.clone().filter(|v| !v.is_empty());
    let routed = if explicit_model.is_none() {
        // The history load above doubles as the routing signal (token size
        // plus real tool-call counts) and is reused for the turn below, so
        // routing sees exactly what the turn will send at no extra load.
        crate::llm::config::route_turn(&req.prompt, &history)
    } else {
        None
    };
    let routed_tier = routed.as_ref().map(|r| r.tier.to_string());
    let routed_why = routed.as_ref().map(|r| r.reason_label());
    let model_override = explicit_model.or_else(|| routed.and_then(|r| r.model_override));
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
        model_override,
        perm_override,
        Vec::new(),
    )
    .await
    .map_err(|e| format!("failed to build config: {e}"))?;
    apply_thinking_override(&mut config, req.thinking_effort.as_deref());
    // Console Go routing requires `x-opencode-session`.
    // Auto-fill from the dex session id; explicit per-request headers
    // below still win on collision.
    crate::llm::config::apply_opencode_session_headers(&mut config, session_id);
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
    // Persist provider/model overrides so /resume restores the same provider/base_url without env.
    // §28: skip the two appends when this exact pair is already persisted
    // and untouched since (same mtime guard as the plan above).
    if let Some(raw) = &req.model {
        if !raw.is_empty() {
            let provider_name = config.provider.name().to_string();
            let persisted = lock_map(&state.sessions)
                .get(session_id)
                .and_then(|e| e.model_persisted.clone());
            let value = (raw.clone(), provider_name.clone());
            if persist_if_stale(&mut session, &entry.path, persisted, "model", raw, &value)? {
                session
                    .set_state("provider", &provider_name)
                    .map_err(|e| format!("failed to persist provider: {e}"))?;
                let at = std::fs::metadata(&entry.path).and_then(|m| m.modified());
                if let (Ok(at), Some(slot)) = (at, lock_map(&state.sessions).get_mut(session_id)) {
                    slot.model_persisted = Some((value, at));
                    // The idle-wake path reads `wake_*` with no turn running:
                    // a persist-only `/provider` switch must refresh them
                    // here, not just at the next turn start below, or the
                    // wake runs on the old endpoint.
                    slot.wake_provider = Some(provider_name.clone());
                    slot.wake_base_url = if config.base_url.trim().is_empty() {
                        None
                    } else {
                        Some(config.base_url.clone())
                    };
                }
            }
        }
    }
    // Stash the turn's model on the registry entry (perf doc §30): the
    // idle-wake path reuses it instead of re-scanning the session file.
    // Every turn refreshes it (not just overrides), so the entry tracks
    // `/model` switches made anywhere. Provider + base URL ride along: a
    // bare model id alone would re-resolve on the default provider.
    if let Some(entry) = lock_map(&state.sessions).get_mut(session_id) {
        entry.model = Some(config.model.clone());
        entry.wake_provider = Some(config.provider.name().to_string());
        entry.wake_base_url = if config.base_url.trim().is_empty() {
            None
        } else {
            Some(config.base_url.clone())
        };
    }
    // Verification is opt-in (DEX_VERIFY / config verify_command). No
    // auto-detect by default — auto-running
    // `cargo test` after every edit is the biggest loop tax.
    // Set DEX_VERIFY or config verify_command, or DEX_VERIFY=1 with a manifest,
    // to re-enable: `DEX_VERIFY=1` or explicit `verify_command` in config.
    crate::llm::config::apply_verify_optin(&mut config);

    // §12 V1b: the child-approval bridge consumes a child's requests,
    // parks them labeled in the session's pending_approvals, and denies
    // them after a five-minute silence. The live-approvals closure keeps
    // children in sync with "allow for session" decisions granted after
    // they spawned.
    let (child_approval_tx, child_approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    {
        let state = state.clone();
        let session_id = session_id.to_string();
        let manager = state.manager_for(&session_id);
        tokio::spawn(child_approval_bridge(
            state,
            session_id.clone(),
            manager,
            child_approval_rx,
        ));
    }
    let live_approvals = {
        let state = state.clone();
        let sid = session_id.to_string();
        Arc::new(move |key: &str| {
            lock_map(&state.session_approvals)
                .get(&sid)
                .is_some_and(|approved| approved.contains(key))
        })
    };

    // The delegation context (Phase 5): built once per parent turn — the
    // manager handle, the parent session path/cwd, and the resolved config
    // the child inherits (cloning its own per definition, §13).
    let agent_ctx = Arc::new(AgentTurnContext {
        depth: 0,
        session_id: session_id.to_string(),
        session_path: entry.path.clone(),
        cwd: entry.cwd.clone(),
        config: Arc::new(config.clone()),
        manager: state.manager_for(session_id),
        session_approvals: lock_map(&state.session_approvals)
            .get(session_id)
            .cloned()
            .unwrap_or_default(),
        child_approvals: Some(child_approval_tx),
        live_approvals: Some(live_approvals),
    });

    // Skills are resolved on the daemon (its filesystem is the workspace).
    // Async dir scans + concurrent reads (Phase 6).
    let mut dirs = skill_dirs();
    dirs.extend(req.skill_dirs.iter().map(std::path::PathBuf::from));
    let skills = discover_skills_async(&dirs).await;

    // Rebuild the conversation: system prompt + persisted history + prompt.
    // History load via `spawn_blocking` (full-history scan, fast) — Phase 4/6.
    // The model-bound load drops `!!` shell runs (saved + shown, never sent
    // to the LLM); the transcript rebuild keeps them.
    let mut messages: Vec<ChatMessage> = Vec::new();
    messages.push(ChatMessage::system(system_prompt_with_override_for(
        &skills,
        req.system_prompt.as_deref(),
        Some(std::path::Path::new(&entry.cwd)),
    )));
    // History was loaded up front for the routing signal; reuse it here
    // so the turn sends exactly what routing saw.
    messages.extend(history);
    let user_message = ChatMessage::user(req.prompt.clone());
    // §10b V1a: completion notices queued while no turn was live drain at
    // the next real turn boundary — the start of this one. They ride the
    // LLM context as a user-role message, never the steering channel.
    drain_agent_notices(state, session_id, &mut session, &mut messages).await?;
    // Durable journal (P8): a turn only exists once turn_start is recorded,
    // and an io::Error here fails the turn instead of being swallowed.
    session
        .turn_event_with_tier("turn_start", routed_tier.as_deref())
        .map_err(|e| format!("failed to record turn_start: {e}"))?;
    session
        .append_message(&user_message)
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
    // Surface the routed tier in the transcript: daemon users get no other
    // signal that the model changed under them (the tier is also journaled
    // on `turn_start`).
    if let Some(tier) = routed_tier.as_deref() {
        let why = routed_why.as_deref().unwrap_or("ordinary work");
        console.emit(SinkLine::System(format!(
            "routing → {tier} ({why}; model {})",
            config.model
        )));
    }
    // Restore “allow for session” approvals that survived from prior turns
    // (previously the per-turn Console dropped them).
    if let Some(set) = lock_map(&state.session_approvals).get(session_id).cloned() {
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
        let sid = session_id.to_string();
        let cancel = cancel.clone();
        let sink_done = channels.sink_done;
        tokio::spawn(async move {
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
                    SinkLine::ToolInput { id, input } => {
                        let mut parts = input.splitn(2, ' ');
                        let name = parts.next().unwrap_or_default().to_string();
                        let args = parts.next().unwrap_or_default().to_string();
                        StreamEvent::ToolCall {
                            name,
                            args: serde_json::Value::String(args),
                            id,
                        }
                    }
                    SinkLine::ToolOutput {
                        id,
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
                        id,
                    },
                    SinkLine::System(text) => StreamEvent::System(text),
                    SinkLine::Error(text) => StreamEvent::Error(text),
                    SinkLine::Usage {
                        tokens,
                        cached,
                        cost,
                        output,
                        gen_ms,
                    } => StreamEvent::Usage {
                        tokens,
                        cached,
                        cost,
                        output,
                        gen_ms,
                    },
                    SinkLine::Plan(plan) => StreamEvent::Plan {
                        goal: plan.goal,
                        steps: plan.steps,
                        constraints: plan.constraints,
                        acceptance: plan.acceptance,
                    },
                };
                let seq = state.next_seq(&sid);
                journal_event(&state, &sid, seq, &event);
                let _ = stream_tx.send(StreamEnvelope { seq, event }).await;
            }
            // Drained (or cancelled): run_agent_turn may now emit the
            // terminal event — it must stay the last one on the wire.
            let _ = sink_done.send(());
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
        let approval_done = channels.approval_done;
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
                let agent = request.agent.clone();
                let parked = PendingApproval {
                    session_id: session_id.clone(),
                    response: request.response,
                    name: request.name.clone(),
                    input: request.input.clone(),
                    agent_id: request.agent_id,
                    agent: agent.clone(),
                };
                let replaced =
                    lock_map(&state.pending_approvals).insert(request_id.clone(), parked);
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
                            agent,
                        },
                    })
                    .await;
            }
            // Channel closed: run_agent_turn may now emit the terminal event.
            let _ = approval_done.send(());
        });
    }

    let mut tool_state = ToolState::load_async().await;
    // Outer loop for follow-up chaining (mirrors old local `event.rs` loop):
    // `process_turn` consumes steering mid-turn; follow-ups are drained after
    // each successful turn and chained without a new HTTP request. The loop
    // value IS the turn result — no assigned-then-broken bookkeeping.
    let turn_result: TurnOutcome = loop {
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut tool_state,
            steering_rx: Some(&mut channels.steering_rx),
            steering_accepted_tx: Some(&channels.steering_accepted_tx),
            session: Some(&mut session),
            client: &config,
            cancel,
            console: &console,
            // Main agent: unfiltered (children pass Some via the manager);
            // the delegation context is live, so `delegate` can spawn.
            filter: None,
            agent_ctx: Some(agent_ctx.clone()),
            tool_budget: None,
        })
        .await;
        match result {
            Ok(resp) => {
                // Mid-turn completions drain at this boundary too — the
                // same seam follow-ups chain through (§10b V1a). With no
                // follow-up to chain, the persisted notice message still
                // reaches the model: the next turn reloads it from the
                // session history.
                drain_agent_notices(state, session_id, &mut session, &mut messages).await?;
                // Drain follow-ups queued while this turn ran (`Recall`
                // cancels one that has not been chained yet).
                let mut followups: Vec<String> = Vec::new();
                while let Ok(msg) = channels.followup_rx.try_recv() {
                    apply_queue_msg(&mut followups, msg);
                }
                if followups.is_empty() {
                    break Ok((resp, tool_state.last_usage, tool_state.last_cached));
                }
                for content in followups {
                    let _ = channels.followup_accepted_tx.send(content.clone()).await;
                    let msg = ChatMessage::user_named(content.clone(), "follow-up");
                    session
                        .append_message(&msg)
                        .map_err(|e| format!("failed to persist followup: {e}"))?;
                    messages.push(msg);
                }
                if cancel.is_cancelled() {
                    break Err("cancelled by user".into());
                }
                // chained follow-up: loop and run another turn with the same
                // session/messages/tool_state but without new turn_start marker
                // (the followup is already persisted).
            }
            Err(e) => break Err(e),
        }
    };
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
    turn_result.map_err(|e| e.to_string())
}

/// Drain queued child completion notices into ONE user-role message
/// ("agent-notifications") prepended to the next turn's context (§10b V1a:
/// results land only at real turn boundaries — never mid-turn, never via
/// the steering channel). The retained [`AgentResult`](crate::agent::subagent::AgentResult)
/// is the source of truth: the child's final text is what the parent
/// consumes.
async fn drain_agent_notices(
    state: &Arc<DaemonState>,
    session_id: &str,
    session: &mut Session,
    messages: &mut Vec<ChatMessage>,
) -> Result<bool, String> {
    let manager = state.manager_for(session_id);
    let notices = manager.drain_notices();
    let overflow = manager.take_overflow();
    if notices.is_empty() && overflow == 0 {
        return Ok(false);
    }
    let mut text = String::new();
    for notice in notices {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&notice.text());
        if let WaitOutcome::Finished(result) = manager.wait(&notice.agent_id, Duration::ZERO).await
        {
            if !result.summary.trim().is_empty() {
                text.push_str("\n\n");
                text.push_str(&result.summary);
            }
            if let Some(error) = result.error {
                text.push_str(&format!("\nError: {error}"));
            }
        }
    }
    if overflow > 0 {
        text.push_str(&format!(
            "\n\n{overflow} more children finished earlier than this notice could \
             carry; their results are retained — use delegate_output with their ids."
        ));
    }
    let message = ChatMessage::user_named(text, "agent-notifications");
    session
        .append_message(&message)
        .map_err(|e| format!("failed to persist agent notice: {e}"))?;
    messages.push(message);
    Ok(true)
}
