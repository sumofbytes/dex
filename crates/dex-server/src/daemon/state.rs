use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::agent::delegate::{AgentEvent, AgentManager};
use crate::protocol::{ApprovalDecision, PermissionMode, QueueMsg};
use crate::protocol::{StreamEnvelope, StreamEvent};
use crate::runtime::console::CancellationToken;

/// Poison-recovery lock for every daemon state mutex: a panicking thread
/// must not take the daemon down — grab the (possibly poisoned) guard and
/// carry on, same as the previous inline
/// `.lock().unwrap_or_else(|e| e.into_inner())` spelling at every former site.
pub(crate) fn lock_map<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A pending tool execution awaiting the client's approval decision.
pub struct PendingApproval {
    pub session_id: String,
    pub response: tokio::sync::mpsc::Sender<ApprovalDecision>,
    pub name: String,
    pub input: String,
    /// Set when the requester is a background child agent (plan §12 V1b):
    /// the child outlives the parent turn, so turn-end teardown and parent
    /// cancel must not deny its approval — it stays parked and answerable.
    pub(crate) agent_id: Option<String>,
    /// The child's definition name for the labeled prompt (V1b, §12):
    /// rendered "explorer wants to run bash: …". `None` for the parent
    /// turn's own tools.
    pub(crate) agent: Option<String>,
}

/// A completed chat turn kept for `Idempotency-Key` dedup (P10): replaying
/// the same key within the window returns the recorded terminal event instead
/// of running the turn again (no duplicate effects).
#[allow(dead_code)]
pub(crate) struct IdempotentTurn {
    pub(crate) session_id: String,
    /// Hash of the request body so a key cannot replay a different prompt.
    pub(crate) request_hash: u64,
    /// Serialized terminal `StreamEvent` (`TurnComplete`/`TurnFailed`).
    pub(crate) terminal: String,
    pub(crate) at: Instant,
}

/// Shared state for the daemon. Plain mutexes are fine here: every critical
/// section is short and never holds the lock across an `.await`.
pub struct DaemonState {
    pub sessions: Mutex<HashMap<String, SessionEntry>>,
    /// The daemon's permission ceiling, resolved **once** at construction
    /// from `--permission` / `DEX_PERMISSION` (one default, `trusted`).
    /// Turn clamps and `/api/config` both read it, so a later env change
    /// cannot split the two. Only a long-lived `dex serve` clamp can bite;
    /// the embedded daemon inherits the same env, so its clamp is a no-op.
    pub(crate) ceiling: PermissionMode,
    /// Pending approval requests keyed by request ID (as sent to the client
    /// in the `ApprovalRequired` stream event). The sender resolves the
    /// blocking `approve_tool` call inside the agent loop.
    pub pending_approvals: Mutex<HashMap<String, PendingApproval>>,
    /// Sessions with a turn currently in flight; one turn at a time per
    /// session keeps the append-only session log consistent.
    pub active_turns: Mutex<HashSet<String>>,
    /// Per-session cancellation tokens for in-flight turns. POST /cancel
    /// signals the token so this turn unwinds without touching other
    /// sessions; the entry is removed when the turn finishes.
    pub(crate) cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// Per-session cancellation tokens for in-flight `!` shell runs (Esc
    /// cancels a running bash). POST /cancel signals these too; the
    /// entry is removed when the run finishes. At most one run per session
    /// (a second `POST /shell` while one is registered is 409).
    pub(crate) shell_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// Per-session steering queue: `POST /steer` pushes a `Content` into the
    /// turn's `steering_rx` (consumed inside `process_turn` between
    /// iterations); `POST /recall` pushes a `Recall` that cancels a not-yet-
    /// injected item.
    pub steering_txs: Mutex<HashMap<String, mpsc::Sender<QueueMsg>>>,
    /// Per-session follow-up queue: `POST /followup` pushes a `Content` into a
    /// turn's outer loop (`run_agent_turn`) which chains a new `process_turn`
    /// iteration without a new HTTP request; `POST /recall` pushes a `Recall`.
    pub followup_txs: Mutex<HashMap<String, mpsc::Sender<QueueMsg>>>,
    /// Per-session next event sequence number (P10) for the SSE journal,
    /// seeded from disk on startup so replays stay consistent across restarts.
    pub event_seqs: Mutex<HashMap<String, u64>>,
    /// `Idempotency-Key` → completed turn, for 60s dedup (P10).
    pub(crate) idempotency: Mutex<HashMap<String, IdempotentTurn>>,
    /// Persisted “allow for session” approvals, keyed by `name:hash` (same
    /// scope as `Console::approval_key`). Lives on the daemon so a decision
    /// survives across turns; previously `Console` was per-turn and lost it.
    pub session_approvals: Mutex<HashMap<String, HashSet<String>>>,
    /// Per-session child-agent managers (Phase 4 lifecycle: spawn cap,
    /// cancel, completion notices). Lazily created by `manager_for`;
    /// `shutdown_agents` joins everything on the ctrl-C exit, and
    /// `remove_session_agents` drops a session's manager once session
    /// delete/reset endpoints exist. Managers are closed on shutdown, so
    /// stale clones cannot respawn children into a dropped registry.
    pub(crate) agents: Mutex<HashMap<String, AgentManager>>,
    /// Set once the background startup rebuild has merged the disk registry.
    /// Surfaced via `/health` so operators can tell a partial registry apart
    /// from an empty one.
    pub rebuild_complete: AtomicBool,
    /// Live SSE streams per session (V1b): lifecycle events that are
    /// journaled outside a turn (child agents, wake turns) are also pushed
    /// to any attached client's turn stream, so the TUI sees them live
    /// instead of waiting for its next poll.
    pub active_streams: Mutex<HashMap<String, Vec<mpsc::Sender<StreamEnvelope>>>>,
    /// Per-session idle wake turn tokens (V1b, plan §10b). A user chat POST
    /// steals the wake: "chat wins, wake skips" — a user-visible 409 must
    /// never lose a race with a background notice.
    pub(crate) wakes: Mutex<HashMap<String, CancellationToken>>,
    /// Last time a client read this session's event journal (V1b presence,
    /// §10b): every `GET /events` refreshes it. A wake fires only when a
    /// client is plausibly listening.
    pub last_client_seen: Mutex<HashMap<String, Instant>>,
    /// Session ids proven absent from disk (perf doc §27): a typo'd id costs
    /// one full `list_all` walk, then hits this instead of re-walking per
    /// request. Populated only after the startup rebuild completes (during
    /// the rebuild window a miss may simply be unscanned-yet, so the disk
    /// fallback always runs); cleared on explicit registration. Entries
    /// expire after `NEGATIVE_TTL` so a session created out-of-band (CLI/TUI
    /// direct file) after a miss becomes visible without a restart.
    pub missing_sessions: Mutex<HashMap<String, Instant>>,
}

/// Negative-cache TTL for absent session ids (see `missing_sessions`).
pub(crate) const NEGATIVE_TTL: Duration = Duration::from_secs(60);

/// 60-second window during which an `Idempotency-Key` replays its recorded
/// turn instead of running it again.
pub(crate) const IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SessionEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cwd: String,
    /// Model the session's last turn used, stashed at turn start (perf doc
    /// §30): the idle-wake path reuses it instead of re-scanning the
    /// session file for the stored `model` state. `None` until the first
    /// turn (registry rebuilds only read headers) — the wake falls back to
    /// the file scan then.
    pub model: Option<String>,
    /// Provider + base URL the stashed model resolved to: a bare model id
    /// alone re-resolves on the default provider/endpoint, so the wake
    /// would run on the wrong endpoint after a `/provider` switch or a
    /// custom `--base-url` turn. `None` until the first turn, like `model`.
    pub wake_provider: Option<String>,
    pub wake_base_url: Option<String>,
    /// Last `plan` JSON this daemon persisted + the session file's mtime
    /// right after the write (perf doc §28): a re-sent identical plan
    /// skips the append when the mtime proves nobody else touched the file
    /// since (the co-located TUI can write the same file directly, so an
    /// entry-only comparison could skip a needed restore).
    pub plan_persisted: Option<(String, std::time::SystemTime)>,
    /// Same for the `model`/`provider` pair: raw client model string +
    /// resolved provider name + file mtime after the write.
    pub model_persisted: Option<((String, String), std::time::SystemTime)>,
}

impl DaemonState {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            // `unwrap_or(Ask)`: a malformed `DEX_PERMISSION` must fail the
            // gate closed, never open (the turn still dies at config
            // resolution, but the ceiling itself stays conservative).
            ceiling: crate::llm::config::permission_from_env().unwrap_or(PermissionMode::Ask),
            pending_approvals: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashSet::new()),
            cancel_tokens: Mutex::new(HashMap::new()),
            shell_tokens: Mutex::new(HashMap::new()),
            steering_txs: Mutex::new(HashMap::new()),
            followup_txs: Mutex::new(HashMap::new()),
            event_seqs: Mutex::new(HashMap::new()),
            idempotency: Mutex::new(HashMap::new()),
            session_approvals: Mutex::new(HashMap::new()),
            agents: Mutex::new(HashMap::new()),
            rebuild_complete: AtomicBool::new(false),
            active_streams: Mutex::new(HashMap::new()),
            wakes: Mutex::new(HashMap::new()),
            last_client_seen: Mutex::new(HashMap::new()),
            missing_sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Register a live SSE stream for a session (V1b): journaled agent
    /// events are pushed to it while it lasts.
    pub(crate) fn register_stream(&self, session_id: &str, tx: &mpsc::Sender<StreamEnvelope>) {
        lock_map(&self.active_streams)
            .entry(session_id.to_string())
            .or_default()
            .push(tx.clone());
    }

    /// Drop one stream registration (identity-matched, so a turn's teardown
    /// cannot remove a newer turn's registration).
    pub(crate) fn unregister_stream(&self, session_id: &str, tx: &mpsc::Sender<StreamEnvelope>) {
        lock_map(&self.active_streams)
            .entry(session_id.to_string())
            .or_default()
            .retain(|existing| !existing.same_channel(tx));
    }

    /// Push one journal event to every attached stream, best effort: the
    /// journal is the source of truth; the push is a latency nicety. One
    /// struct clone per stream — no per-receiver serialization here (each
    /// SSE body serializes once in its own `poll_next`, unavoidable with
    /// per-stream backpressure; sessions typically have one stream). A
    /// closed receiver is pruned so dead streams don't accumulate; a full
    /// one is dropped (its next poll backfills from the journal).
    pub(crate) fn broadcast_event(&self, session_id: &str, env: &StreamEnvelope) {
        let senders = lock_map(&self.active_streams)
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        for tx in senders {
            match tx.try_send(env.clone()) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    self.unregister_stream(session_id, &tx);
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
            }
        }
    }

    /// Presence heartbeat (V1b): a client read this session's journal now.
    pub(crate) fn touch_client_seen(&self, session_id: &str) {
        lock_map(&self.last_client_seen).insert(session_id.to_string(), Instant::now());
    }

    /// A client read the journal within `window` — plausibly an audience.
    pub(crate) fn client_seen_fresh(&self, session_id: &str, window: Duration) -> bool {
        lock_map(&self.last_client_seen)
            .get(session_id)
            .is_some_and(|seen| seen.elapsed() < window)
    }

    /// Claim the idle-wake slot for a session. `None` when a wake is
    /// already live — one at a time per session (plan §10b).
    pub(crate) fn claim_wake(&self, session_id: &str) -> Option<CancellationToken> {
        let token = CancellationToken::new();
        let mut wakes = lock_map(&self.wakes);
        if wakes.contains_key(session_id) {
            return None;
        }
        wakes.insert(session_id.to_string(), token.clone());
        Some(token)
    }

    /// Steal (cancel) a session's idle wake — the user chat POST wins.
    pub(crate) fn cancel_wake(&self, session_id: &str) -> Option<CancellationToken> {
        let token = lock_map(&self.wakes).remove(session_id)?;
        token.cancel();
        Some(token)
    }

    /// Collect (and remove) still-pending child-agent approvals for a
    /// session so cancel/shutdown paths can deny them. The counterpart of
    /// `take_session_pendings`, which deliberately skips these.
    pub(crate) fn take_agent_pendings(
        &self,
        session_id: Option<&str>,
    ) -> Vec<tokio::sync::mpsc::Sender<ApprovalDecision>> {
        let mut out = Vec::new();
        lock_map(&self.pending_approvals).retain(|_, p| {
            if p.agent_id.is_some() && session_id.is_none_or(|sid| p.session_id == sid) {
                out.push(p.response.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Collect (and remove) the parent turn's still-pending approvals for a
    /// session so the caller can deny them without holding the lock across
    /// an await. Child-agent approvals (`agent_id` set, plan §12 V1b) are
    /// skipped: a background child outlives the parent turn, and denying its
    /// approval would strand a still-running child with no way to proceed.
    pub(crate) fn take_session_pendings(
        &self,
        session_id: &str,
    ) -> Vec<tokio::sync::mpsc::Sender<ApprovalDecision>> {
        let mut out = Vec::new();
        lock_map(&self.pending_approvals).retain(|_, p| {
            if p.agent_id.is_none() && p.session_id == session_id {
                out.push(p.response.clone());
                false
            } else {
                true
            }
        });
        out
    }

    /// Lazily create (or return) the child-agent manager for a session.
    /// Clones share one registry, so any handle sees every child. The
    /// terminal-path journal hook (§15 V1a) closes over this state, making
    /// a state → manager → hook cycle; `shutdown_agents` and
    /// `remove_session_agents` take the managers out of the map, which
    /// drops the hooks and breaks it — nothing leaks.
    /// Phase 5's delegate tool is the first caller.
    pub(crate) fn manager_for(self: &Arc<Self>, session_id: &str) -> AgentManager {
        let mut agents = lock_map(&self.agents);
        agents
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let state = Arc::clone(self);
                let sid = session_id.to_string();
                AgentManager::new(session_id).with_events(Arc::new(move |event| {
                    journal_agent_event(&state, &sid, event);
                }))
            })
            .clone()
    }

    /// Cancel + join a session's children, then drop its manager, so no
    /// orphaned child task survives its session. `shutdown` marks the
    /// manager closed, so stale clones cannot respawn into the dropped
    /// registry. Session delete/reset hook — wired to those endpoints when
    /// they land (Phase 5/6); until then it stays `dead_code`.
    #[allow(dead_code)]
    pub(crate) async fn remove_session_agents(&self, session_id: &str) {
        // The children are about to be cancelled: deny their still-parked
        // approvals so a blocked tool call wakes and unwinds instead of
        // waiting on a prompt nobody will answer (§12 V1b timeout rule).
        for sender in self.take_agent_pendings(Some(session_id)) {
            let _ = sender.send(ApprovalDecision::Deny).await;
        }
        let manager = lock_map(&self.agents).remove(session_id);
        if let Some(manager) = manager {
            manager.shutdown().await;
        }
    }

    /// Cancel + join every session's children and drop all managers.
    /// Daemon shutdown hook, wired to the ctrl-C exit path in `run_daemon`:
    /// after this returns no child task is live and every manager is closed.
    pub(crate) async fn shutdown_agents(&self) {
        let managers: Vec<AgentManager> = {
            let mut agents = lock_map(&self.agents);
            std::mem::take(&mut *agents).into_values().collect()
        };
        for manager in managers {
            manager.shutdown().await;
        }
        for sender in self.take_agent_pendings(None) {
            let _ = sender.send(ApprovalDecision::Deny).await;
        }
    }

    #[allow(dead_code)]
    /// Scoped “allow for session” check — mirrors `Console::approval_key`
    /// so daemon and console agree on scope. `write`/`edit` → path, `bash` →
    /// command, else full input hash.
    pub(crate) fn is_session_approved(&self, session_id: &str, name: &str, input: &str) -> bool {
        let key = crate::runtime::console::Console::approval_key(name, input);
        lock_map(&self.session_approvals)
            .get(session_id)
            .is_some_and(|set| set.contains(&key))
    }

    pub(crate) fn record_session_approval(&self, session_id: &str, name: &str, input: &str) {
        let key = crate::runtime::console::Console::approval_key(name, input);
        lock_map(&self.session_approvals)
            .entry(session_id.to_string())
            .or_default()
            .insert(key);
    }

    /// Check for a replayable turn under `Idempotency-Key`. Falls through
    /// (None) when the key is unknown, stale, or names a different session or
    /// request hash.
    pub(crate) fn idempotent_replay(
        &self,
        key: &str,
        session_id: &str,
        request_hash: u64,
    ) -> Option<String> {
        let mut map = lock_map(&self.idempotency);
        map.retain(|_, t| t.at.elapsed() < IDEMPOTENCY_WINDOW);
        let turn = map.get(key)?;
        if turn.session_id != session_id || turn.request_hash != request_hash {
            return None;
        }
        Some(turn.terminal.clone())
    }

    /// Record a completed turn for `Idempotency-Key` dedup.
    pub(crate) fn idempotency_record(
        &self,
        key: &str,
        session_id: &str,
        request_hash: u64,
        terminal: String,
    ) {
        lock_map(&self.idempotency).insert(
            key.to_string(),
            IdempotentTurn {
                session_id: session_id.to_string(),
                request_hash,
                terminal,
                at: Instant::now(),
            },
        );
    }

    /// Allocate the next event seq for a session.
    pub(crate) fn next_seq(&self, session_id: &str) -> u64 {
        let mut map = lock_map(&self.event_seqs);
        let next = map.entry(session_id.to_string()).or_insert(0);
        let seq = *next;
        *next += 1;
        seq
    }

    /// Allocate a consecutive call/result seq pair under one lock hold so no
    /// concurrent turn can land between the two (shell tool-block replay
    /// stays adjacent).
    pub(crate) fn next_seq_pair(&self, session_id: &str) -> (u64, u64) {
        let mut map = lock_map(&self.event_seqs);
        let next = map.entry(session_id.to_string()).or_insert(0);
        let first = *next;
        *next += 2;
        (first, first + 1)
    }

    /// Seed `event_seqs` for a session from its persisted journal: the next
    /// allocation continues after the highest journaled seq (0 when the
    /// journal holds no seq yet — `None`, not a journal holding seq 0).
    /// Takes the max with any live counter: the startup rebuild now runs in
    /// the background, so a turn may have allocated seqs before this seeds.
    pub(crate) fn seed_seq(&self, session_id: &str, path: &std::path::Path) {
        let next = crate::session::Session::max_event_seq(path).map_or(0, |max| max + 1);
        lock_map(&self.event_seqs)
            .entry(session_id.to_string())
            .and_modify(|seq| *seq = (*seq).max(next))
            .or_insert(next);
    }

    /// Rebuild the session registry from disk (`Session::list_all`) after a
    /// daemon restart, seed per-session event cursors, and mark turns that
    /// were interrupted by the crash (`turn_start` with no terminal entry) as
    /// `turn_failed` so a reattaching client sees the truth instead of a ghost.
    ///
    /// Runs on a background task at startup: per-session scans run
    /// concurrently in a `JoinSet` (`spawn_blocking` per file, join, sort),
    /// fixing the linear scan; the registry lock is held only for the final
    /// insert. Entries use `or_insert` so sessions created while the rebuild
    /// was in flight win over their (nonexistent) disk state.
    #[allow(clippy::type_complexity)]
    /// Async parallel rebuild: per-session scans in a `JoinSet`
    /// (`spawn_blocking` per file, join, sort). Used by the background task.
    pub(crate) async fn rebuild_async(&self) {
        let listed = tokio::task::spawn_blocking(crate::session::Session::list_all)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();
        let mut set = tokio::task::JoinSet::new();
        for (path, header) in listed {
            let id = header.id().to_string();
            let name = header.name().map(ToOwned::to_owned);
            let cwd = header.cwd().to_string();
            let path_c = path.clone();
            set.spawn(tokio::task::spawn_blocking(move || {
                let seq_next = crate::session::Session::max_event_seq(&path_c).map_or(0, |m| m + 1);
                let interrupted =
                    crate::session::Session::last_turn_state(&path_c) == "interrupted";
                (id, path_c, name, cwd, seq_next, interrupted)
            }));
        }
        let mut scanned: Vec<(String, PathBuf, Option<String>, String, u64, bool)> = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(Ok(v)) = joined {
                scanned.push(v);
            }
        }
        scanned.sort_by(|a, b| a.0.cmp(&b.0));
        let mut entries = Vec::new();
        let mut interrupted = Vec::new();
        for (id, path, name, cwd, seq_next, is_interrupted) in scanned {
            lock_map(&self.event_seqs)
                .entry(id.clone())
                .and_modify(|s| *s = (*s).max(seq_next))
                .or_insert(seq_next);
            if is_interrupted {
                interrupted.push((id.clone(), path.clone()));
            }
            entries.push((
                id,
                SessionEntry {
                    path,
                    name,
                    cwd,
                    model: None,
                    wake_provider: None,
                    wake_base_url: None,
                    plan_persisted: None,
                    model_persisted: None,
                },
            ));
        }
        for (id, path) in &interrupted {
            let live = lock_map(&self.active_turns).contains(id);
            if live {
                continue;
            }
            if let Ok(mut s) = crate::session::Session::from_path(path) {
                let _ = s.turn_event("turn_failed").and_then(|_| {
                    s.set_state(
                        "last_error",
                        "turn interrupted by daemon restart; effects may be partial — review before continuing",
                    )
                });
            }
        }
        let mut sessions = lock_map(&self.sessions);
        for (id, entry) in entries {
            sessions.entry(id).or_insert(entry);
        }
        self.rebuild_complete.store(true, Ordering::Relaxed);
    }
}

/// Append one numbered stream event to the session's `.events.jsonl`
/// journal (P10). Best effort like every journal write here: a missing
/// registry entry or unreadable session file silently skips the append —
/// the live SSE stream is the primary delivery path, the journal only
/// feeds `?since=` replay.
pub(crate) fn journal_event(state: &DaemonState, session_id: &str, seq: u64, event: &StreamEvent) {
    let path = lock_map(&state.sessions)
        .get(session_id)
        .map(|entry| entry.path.clone());
    let Some(path) = path else {
        return;
    };
    if let Ok(mut journal) = crate::session::Session::from_path_for_events(&path) {
        let _ = journal.append_event(seq, &serde_json::to_string(event).unwrap_or_default());
    }
}

/// Journal one child lifecycle line (§15 V1a): a `System` event with the
/// stable `[agent <name>:<id>]` prefix the TUI matches on. Called from the
/// manager's terminal path — every ending (completed/failed/cancelled/
/// timed out/panic) lands here at completion time, even while no turn is
/// live, so a client's `?since=` poll picks it up without a turn.
/// §15 V1a + V1b: journal one event per typed lifecycle event, fired from
/// the manager's single choke points. Completions keep their V1a `System`
/// line (old clients render it) and add the typed variant (new clients read
/// fields); both are broadcast to any attached live stream so a TUI mid-turn
/// sees child lifecycle live. A completion also schedules the idle wake
/// turn (§10b V1b).
pub(crate) fn journal_agent_event(state: &Arc<DaemonState>, session_id: &str, event: AgentEvent) {
    let path = lock_map(&state.sessions)
        .get(session_id)
        .map(|entry| entry.path.clone());
    let Some(path) = path else {
        return;
    };
    let Ok(mut journal) = crate::session::Session::from_path_for_events(&path) else {
        return;
    };
    let (typed, system_line) = match &event {
        AgentEvent::Spawned { agent_id, name } => (
            StreamEvent::AgentSpawned {
                agent_id: agent_id.to_string(),
                name: name.clone(),
            },
            None,
        ),
        AgentEvent::Progress {
            agent_id,
            current_tool,
        } => (
            StreamEvent::AgentProgress {
                agent_id: agent_id.to_string(),
                state: "running".to_string(),
                current_tool: current_tool.clone(),
            },
            None,
        ),
        AgentEvent::Completed(notice) => (
            StreamEvent::AgentCompleted {
                agent_id: notice.agent_id.to_string(),
                status: crate::agent::delegate::status_word(notice.status).to_string(),
            },
            Some(notice.text()),
        ),
    };
    // The V1a line first, so a replay renders the transcript line before it
    // consumes the typed variant.
    if let Some(line) = system_line {
        let seq = state.next_seq(session_id);
        let env = StreamEnvelope {
            seq,
            event: StreamEvent::System(line),
        };
        let _ = journal.append_event(seq, &serde_json::to_string(&env.event).unwrap_or_default());
        state.broadcast_event(session_id, &env);
    }
    let seq = state.next_seq(session_id);
    let env = StreamEnvelope { seq, event: typed };
    let _ = journal.append_event(seq, &serde_json::to_string(&env.event).unwrap_or_default());
    state.broadcast_event(session_id, &env);
    if matches!(&event, AgentEvent::Completed(_)) {
        crate::daemon::wake::schedule_idle_wake(state.clone(), session_id.to_string());
    }
}
