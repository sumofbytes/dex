pub(crate) mod server;

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::agent::subagent::{AgentEvent, AgentManager};
use crate::core::console::CancellationToken;
use crate::core::types::{ApprovalDecision, QueueMsg};
use crate::protocol::{StreamEnvelope, StreamEvent};

// ---------------------------------------------------------------------------
// Daemon bearer token
// ---------------------------------------------------------------------------

/// Where the daemon publishes its auto-generated bearer token for clients
/// (`$XDG_DATA_HOME/dex/daemon.token`, 0600).
pub(crate) fn daemon_token_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("dex/daemon.token"))
}

/// Token required by this daemon process, resolved at serve time:
///
/// - `DEX_DAEMON_TOKEN` always wins — the operator chose the credential.
/// - A loopback bind needs no token (the co-located TUI / one-shot client
///   keeps working with zero configuration), unless `DEX_DAEMON_TOKEN` is set.
/// - Any other bind generates one, writes it to `daemon.token` (0600,
///   created atomically so it is never world-readable) and prints it once.
///   Serving a workspace-running agent on a reachable interface with no
///   authentication would hand arbitrary code execution to anyone on the
///   network.
///
/// Single-global file: two daemons on one machine share `daemon.token` —
/// the second overwrites the first. Clients connecting to multiple daemons
/// must pass per-host `DEX_DAEMON_TOKEN` explicitly.
static REQUIRED_TOKEN: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Atomically write `token` to `path` with 0600 (no world-readable window).
fn write_token_file(path: &PathBuf, token: &str) -> bool {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let content = format!("{token}\n");
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut f) => {
                use std::io::Write as _;
                f.write_all(content.as_bytes()).is_ok()
            }
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        match std::fs::write(path, format!("{token}\n")) {
            Ok(()) => true,
            Err(_) => false,
        }
    }
}

pub(crate) fn prepare_daemon_token(addr: &std::net::SocketAddr) {
    let token = std::env::var("DEX_DAEMON_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            if addr.ip().is_loopback() {
                return None;
            }
            let token = uuid::Uuid::new_v4().to_string();
            let published = match daemon_token_file() {
                Some(path) => write_token_file(&path, &token),
                None => false,
            };
            eprintln!(
                "daemon: generated bearer token for {addr} (written to daemon.token: {published})"
            );
            eprintln!("daemon: clients connect with DEX_DAEMON_TOKEN=<token>");
            Some(token)
        });
    *REQUIRED_TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = Some(token);
}

/// The credential this daemon process requires (`None` → unauthenticated).
/// Cloned (not `&'static`) so tests can re-resolve per case without
/// poisoning a process-global `OnceLock`.
pub(crate) fn required_token() -> Option<String> {
    REQUIRED_TOKEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .flatten()
}

#[cfg(test)]
pub(crate) fn reset_daemon_token_for_tests() {
    *REQUIRED_TOKEN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// A pending tool execution awaiting the client's approval decision.
pub(crate) struct PendingApproval {
    pub(crate) session_id: String,
    pub(crate) response: tokio::sync::mpsc::Sender<ApprovalDecision>,
    pub(crate) name: String,
    pub(crate) input: String,
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
    session_id: String,
    /// Hash of the request body so a key cannot replay a different prompt.
    request_hash: u64,
    /// Serialized terminal `StreamEvent` (`TurnComplete`/`TurnFailed`).
    terminal: String,
    at: Instant,
}

/// Shared state for the daemon. Plain mutexes are fine here: every critical
/// section is short and never holds the lock across an `.await`.
pub(crate) struct DaemonState {
    pub sessions: Mutex<HashMap<String, SessionEntry>>,
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
    pub cancel_tokens: Mutex<HashMap<String, CancellationToken>>,
    /// Per-session cancellation tokens for in-flight `!` shell runs (Esc
    /// cancels a running bash). POST /cancel signals these too; the
    /// entry is removed when the run finishes. At most one run per session
    /// (a second `POST /shell` while one is registered is 409).
    pub shell_tokens: Mutex<HashMap<String, CancellationToken>>,
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
    pub idempotency: Mutex<HashMap<String, IdempotentTurn>>,
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
    pub agents: Mutex<HashMap<String, AgentManager>>,
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
    pub wakes: Mutex<HashMap<String, CancellationToken>>,
    /// Last time a client read this session's event journal (V1b presence,
    /// §10b): every `GET /events` refreshes it. A wake fires only when a
    /// client is plausibly listening.
    pub last_client_seen: Mutex<HashMap<String, Instant>>,
}

/// 60-second window during which an `Idempotency-Key` replays its recorded
/// turn instead of running it again.
const IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct SessionEntry {
    pub path: PathBuf,
    pub name: Option<String>,
    pub cwd: String,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
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
        }
    }

    /// Register a live SSE stream for a session (V1b): journaled agent
    /// events are pushed to it while it lasts.
    pub(crate) fn register_stream(&self, session_id: &str, tx: &mpsc::Sender<StreamEnvelope>) {
        self.active_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .push(tx.clone());
    }

    /// Drop one stream registration (identity-matched, so a turn's teardown
    /// cannot remove a newer turn's registration).
    pub(crate) fn unregister_stream(&self, session_id: &str, tx: &mpsc::Sender<StreamEnvelope>) {
        self.active_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .retain(|existing| !existing.same_channel(tx));
    }

    /// Push one journal event to every attached stream, best effort: the
    /// journal is the source of truth; the push is a latency nicety and a
    /// full/closed channel is harmless.
    pub(crate) fn broadcast_event(&self, session_id: &str, env: &StreamEnvelope) {
        let senders = self
            .active_streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.try_send(env.clone());
        }
    }

    /// Presence heartbeat (V1b): a client read this session's journal now.
    pub(crate) fn touch_client_seen(&self, session_id: &str) {
        self.last_client_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), Instant::now());
    }

    /// A client read the journal within `window` — plausibly an audience.
    pub(crate) fn client_seen_fresh(&self, session_id: &str, window: Duration) -> bool {
        self.last_client_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .is_some_and(|seen| seen.elapsed() < window)
    }

    /// Claim the idle-wake slot for a session. `None` when a wake is
    /// already live — one at a time per session (plan §10b).
    pub(crate) fn claim_wake(&self, session_id: &str) -> Option<CancellationToken> {
        let token = CancellationToken::new();
        let mut wakes = self.wakes.lock().unwrap_or_else(|e| e.into_inner());
        if wakes.contains_key(session_id) {
            return None;
        }
        wakes.insert(session_id.to_string(), token.clone());
        Some(token)
    }

    /// Steal (cancel) a session's idle wake — the user chat POST wins.
    pub(crate) fn cancel_wake(&self, session_id: &str) -> Option<CancellationToken> {
        let token = self
            .wakes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)?;
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
        self.pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, p| {
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
        self.pending_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, p| {
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
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
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
        let manager = self
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(manager) = manager {
            manager.shutdown().await;
        }
    }

    /// Cancel + join every session's children and drop all managers.
    /// Daemon shutdown hook, wired to the ctrl-C exit path in `run_daemon`:
    /// after this returns no child task is live and every manager is closed.
    pub(crate) async fn shutdown_agents(&self) {
        let managers: Vec<AgentManager> = {
            let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
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
        let key = crate::core::console::Console::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(&key))
    }

    pub(crate) fn record_session_approval(&self, session_id: &str, name: &str, input: &str) {
        let key = crate::core::console::Console::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(key);
    }

    /// Check for a replayable turn under `Idempotency-Key`. Falls through
    /// (None) when the key is unknown, stale, or names a different session or
    /// request hash.
    fn idempotent_replay(&self, key: &str, session_id: &str, request_hash: u64) -> Option<String> {
        let mut map = self.idempotency.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| t.at.elapsed() < IDEMPOTENCY_WINDOW);
        let turn = map.get(key)?;
        if turn.session_id != session_id || turn.request_hash != request_hash {
            return None;
        }
        Some(turn.terminal.clone())
    }

    /// Record a completed turn for `Idempotency-Key` dedup.
    fn idempotency_record(&self, key: &str, session_id: &str, request_hash: u64, terminal: String) {
        self.idempotency
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
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
    fn next_seq(&self, session_id: &str) -> u64 {
        let mut map = self.event_seqs.lock().unwrap_or_else(|e| e.into_inner());
        let next = map.entry(session_id.to_string()).or_insert(0);
        let seq = *next;
        *next += 1;
        seq
    }

    /// Allocate a consecutive call/result seq pair under one lock hold so no
    /// concurrent turn can land between the two (shell tool-block replay
    /// stays adjacent).
    fn next_seq_pair(&self, session_id: &str) -> (u64, u64) {
        let mut map = self.event_seqs.lock().unwrap_or_else(|e| e.into_inner());
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
    fn seed_seq(&self, session_id: &str, path: &std::path::Path) {
        let next = crate::session::Session::max_event_seq(path).map_or(0, |max| max + 1);
        self.event_seqs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
            self.event_seqs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(id.clone())
                .and_modify(|s| *s = (*s).max(seq_next))
                .or_insert(seq_next);
            if is_interrupted {
                interrupted.push((id.clone(), path.clone()));
            }
            entries.push((id, SessionEntry { path, name, cwd }));
        }
        for (id, path) in &interrupted {
            let live = self
                .active_turns
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(id);
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
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        for (id, entry) in entries {
            sessions.entry(id).or_insert(entry);
        }
        self.rebuild_complete.store(true, Ordering::Relaxed);
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
fn journal_agent_event(state: &Arc<DaemonState>, session_id: &str, event: AgentEvent) {
    let path = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(session_id)
        .map(|entry| entry.path.clone());
    let Some(path) = path else {
        return;
    };
    let Ok(mut journal) = crate::session::Session::from_path(&path) else {
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
                status: crate::agent::subagent::status_word(notice.status).to_string(),
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
    if matches!(event, AgentEvent::Completed(_)) {
        crate::daemon::server::schedule_idle_wake(state.clone(), session_id.to_string());
    }
}

/// Start the daemon HTTP server on an already-bound listener.
pub(crate) async fn run_daemon(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    // MCP bootstrap: connects servers in the background and merges their
    // tools into the schema cache (plus the 60s liveness sweeper). Without
    // this the manager stays uninitialized and `mcp__*` tools never exist.
    crate::mcp::global_manager();
    let state = std::sync::Arc::new(DaemonState::new());
    // Rebuild in-memory state from the persisted JSONL on a background thread:
    // scanning every session (headers, event seqs, turn state) costs ~0.5s
    // with a few thousand sessions and would delay /health and TUI first
    // paint. Fresh sessions use uuid ids so they never collide with rebuilt
    // ones; `seed_seq` takes the max so a racing turn can't rewind a counter.
    // A panicking rebuild must not fail silent (the registry would stay
    // partial behind an `ok` health check): log it, and `rebuild_complete`
    // in `/health` stays false.
    let warm = state.clone();
    tokio::spawn(async move {
        warm.rebuild_async().await;
    });

    // Fresh installs have no models.dev catalog until `dex update --models`
    // runs, which silently degrades context windows and `/model` autocomplete.
    // Best-effort background fetch on first start; never blocks or fails the
    // daemon. One retry after 5 minutes covers a laptop waking offline, since
    // a daemon can outlive the outage; `dex update --models` always works too.
    if crate::llm::config::catalog_cache_missing() {
        tokio::spawn(async {
            // The error type is Box<dyn Error>, which is not Send: report it
            // and drop it before any further await so the future stays Send.
            let failed = match crate::llm::config::refresh_models_cache_async().await {
                Ok(()) => false,
                Err(e) => {
                    eprintln!("note: models.dev catalog fetch failed ({e}); retrying in 5 minutes");
                    true
                }
            };
            if failed {
                tokio::time::sleep(Duration::from_secs(5 * 60)).await;
                if let Err(e) = crate::llm::config::refresh_models_cache_async().await {
                    eprintln!(
                        "note: models.dev catalog still missing ({e}); run `dex update --models`"
                    );
                }
            }
        });
    }

    let app = server::router(state.clone());

    // Note: no startup announcement here. The headless `dex serve` caller
    // prints one; the embedded (`dex` default) daemon shares the process
    // with the TUI and must stay silent — anything printed before the
    // alt-screen is entered lingers in scrollback after quit and reads as
    // if a daemon were still listening.

    // tokio refuses blocking fds; the std listener must be non-blocking
    // before registration.
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;

    // Headless exit path: ctrl-C stops serving, cancels + joins every
    // session's children (§14: no orphaned tokio task survives this exit),
    // then exits. A spawned watcher — not `with_graceful_shutdown` — so a
    // still-connected SSE client cannot hold the process open while it
    // drains; once children are joined nothing is lost by exiting hard.
    {
        let state_for_exit = state.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                state_for_exit.shutdown_agents().await;
                std::process::exit(0);
            }
        });
    }

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::{
        AgentDefinition, AgentResult, AgentState, ContextSeed, ProgressReporter, WaitOutcome,
    };

    fn agent_test_parts(name: &str) -> (AgentDefinition, ContextSeed) {
        let mut def = crate::agent::subagent::builtin_definitions()
            .into_iter()
            .next()
            .expect("built-in agents");
        def.name = name.to_string();
        let seed = ContextSeed {
            task: "do the thing".to_string(),
            file_hints: Vec::new(),
            parent_summary: None,
        };
        (def, seed)
    }

    async fn done_body(
        _token: CancellationToken,
        _progress: ProgressReporter,
        _id: crate::agent::subagent::AgentId,
    ) -> AgentResult {
        AgentResult {
            status: AgentState::Completed,
            summary: "done".to_string(),
            error: None,
            usage: None,
        }
    }

    async fn cancel_body(
        token: CancellationToken,
        _progress: ProgressReporter,
        _id: crate::agent::subagent::AgentId,
    ) -> AgentResult {
        token.cancelled().await;
        AgentResult {
            status: AgentState::Cancelled,
            summary: String::new(),
            error: Some("child saw cancel".to_string()),
            usage: None,
        }
    }

    #[test]
    fn idempotency_key_replays_same_turn_and_rejects_different_request() {
        let state = DaemonState::new();
        state.idempotency_record("key-1", "sess-1", 42, "{\"seq\":9}".into());
        // Same session + same request hash: replay.
        assert_eq!(
            state.idempotent_replay("key-1", "sess-1", 42),
            Some("{\"seq\":9}".to_string())
        );
        // Different request hash under the same key: do NOT replay (a key
        // cannot launder a different prompt).
        assert_eq!(state.idempotent_replay("key-1", "sess-1", 43), None);
        // Different session under the same key: no replay.
        assert_eq!(state.idempotent_replay("key-1", "sess-2", 42), None);
        // Unknown key: no replay.
        assert_eq!(state.idempotent_replay("nope", "sess-1", 42), None);
    }

    #[test]
    fn event_seq_allocates_monotonically_per_session() {
        let state = DaemonState::new();
        assert_eq!(state.next_seq("a"), 0);
        assert_eq!(state.next_seq("a"), 1);
        assert_eq!(state.next_seq("b"), 0);
        assert_eq!(state.next_seq("a"), 2);
    }

    #[test]
    fn wake_slot_holds_one_wake_per_session_and_frees_on_cancel() {
        // §10b V1b: one wake at a time — a second claim loses; the steal
        // path frees the slot and cancels the loser's token.
        let state = DaemonState::new();
        let first = state.claim_wake("s").expect("first claim wins");
        assert!(state.claim_wake("s").is_none(), "one at a time");
        assert!(state.claim_wake("other").is_some(), "sessions are separate");
        let stolen = state.cancel_wake("s").expect("steal finds the wake");
        assert!(
            stolen.is_cancelled(),
            "a stolen wake must stop; the claim returns the same token"
        );
        assert!(first.is_cancelled(), "a stolen wake must stop");
        assert!(state.cancel_wake("s").is_none(), "already removed");
        assert!(state.claim_wake("s").is_some(), "slot freed");
    }

    #[test]
    fn client_seen_presence_needs_a_recent_journal_read() {
        // §10b V1b presence gate: no read → no audience; a read inside the
        // window counts; a read older than the window does not.
        let state = DaemonState::new();
        assert!(!state.client_seen_fresh("s", std::time::Duration::from_secs(30)));
        state.touch_client_seen("s");
        assert!(state.client_seen_fresh("s", std::time::Duration::from_secs(30)));
        assert!(!state.client_seen_fresh("s", std::time::Duration::ZERO));
    }

    #[tokio::test]
    async fn take_agent_pendings_takes_only_session_children() {
        // The counterpart of `take_session_pendings`: parent approvals stay
        // (their turn owns them); children leave so their parked prompts
        // deny when the session goes away.
        use crate::core::types::ApprovalDecision;
        let state = DaemonState::new();
        let (tx_parent, mut rx_parent) = tokio::sync::mpsc::channel(1);
        let (tx_child, mut rx_child) = tokio::sync::mpsc::channel(1);
        let (tx_other, mut rx_other) = tokio::sync::mpsc::channel(1);
        {
            let mut pending = state.pending_approvals.lock().unwrap();
            pending.insert(
                "p".into(),
                PendingApproval {
                    session_id: "s".into(),
                    response: tx_parent,
                    name: "write".into(),
                    input: "{}".into(),
                    agent_id: None,
                    agent: None,
                },
            );
            pending.insert(
                "c".into(),
                PendingApproval {
                    session_id: "s".into(),
                    response: tx_child,
                    name: "bash".into(),
                    input: "{}".into(),
                    agent_id: Some("s-0".into()),
                    agent: Some("tester".into()),
                },
            );
            pending.insert(
                "o".into(),
                PendingApproval {
                    session_id: "other".into(),
                    response: tx_other,
                    name: "bash".into(),
                    input: "{}".into(),
                    agent_id: Some("other-0".into()),
                    agent: Some("tester".into()),
                },
            );
        }

        let taken = state.take_agent_pendings(Some("s"));
        assert_eq!(taken.len(), 1, "only the session's child approval");
        let _ = taken[0].send(ApprovalDecision::Deny).await;
        assert_eq!(rx_child.try_recv().ok(), Some(ApprovalDecision::Deny));
        {
            let pending = state.pending_approvals.lock().unwrap();
            assert!(pending.contains_key("p"), "parent approval is turn-owned");
            assert!(pending.contains_key("o"), "other session untouched");
        }
        let all = state.take_agent_pendings(None);
        assert_eq!(all.len(), 1, "shutdown sweeps the remaining child");
        assert!(rx_parent.try_recv().is_err());
        assert!(rx_other.try_recv().is_err());
    }

    #[test]
    fn fresh_state_has_no_agent_managers() {
        // Restart-empty: no child registries until first delegate.
        let state = std::sync::Arc::new(DaemonState::new());
        assert!(state
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_for_is_shared_per_session_and_isolated_across() {
        let state = std::sync::Arc::new(DaemonState::new());
        let (def, seed) = agent_test_parts("explorer");
        // A child spawned through one handle is visible through another
        // handle for the same session: clones share one registry.
        let id = state
            .manager_for("s1")
            .spawn(&def, seed, done_body)
            .unwrap();
        match state
            .manager_for("s1")
            .wait(&id, std::time::Duration::from_secs(5))
            .await
        {
            WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Completed),
            other => panic!("expected Finished, got {other:?}"),
        }
        // Other sessions are isolated: unknown id, fresh counter.
        assert_eq!(state.manager_for("s2").status(&id), None);
        let (def2, seed2) = agent_test_parts("explorer");
        let other = state
            .manager_for("s2")
            .spawn(&def2, seed2, done_body)
            .unwrap();
        assert_eq!(other.to_string(), "s2-0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_session_agents_cancels_children_and_drops_manager() {
        let state = std::sync::Arc::new(DaemonState::new());
        let manager = state.manager_for("s1");
        let (def, seed) = agent_test_parts("explorer");
        let id = manager.spawn(&def, seed, cancel_body).unwrap();
        assert_eq!(manager.active_count(), 1);
        state.remove_session_agents("s1").await;
        // The pre-removal handle still sees the reaped child (shared
        // registry), now terminal through the Cancelled path.
        match manager.wait(&id, std::time::Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(manager.active_count(), 0);
        // A fresh lookup starts empty; removing an unknown session is a no-op.
        assert_eq!(state.manager_for("s1").active_count(), 0);
        state.remove_session_agents("missing").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_agents_joins_every_session() {
        let state = Arc::new(DaemonState::new());
        let first = state.manager_for("s1");
        let second = state.manager_for("s2");
        let (def1, seed1) = agent_test_parts("explorer");
        let (def2, seed2) = agent_test_parts("tester");
        first.spawn(&def1, seed1, cancel_body).unwrap();
        second.spawn(&def2, seed2, cancel_body).unwrap();
        state.shutdown_agents().await;
        assert_eq!(first.active_count(), 0);
        assert_eq!(second.active_count(), 0);
        assert!(state
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
    }

    #[test]
    fn event_seq_is_seeded_from_disk_after_restart() {
        // Touches the shared sessions dir; serialize against env-redirecting tests.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Simulate a prior run: a session with events already journaled.
        let mut s = crate::session::Session::new("/tmp/dex-seq-test".into(), None).unwrap();
        s.append_event(0, "{\"type\":\"system\",\"data\":\"x\"}")
            .unwrap();
        s.append_event(1, "{\"type\":\"system\",\"data\":\"y\"}")
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let state = DaemonState::new();
        state.seed_seq(s.id(), &path);
        // The next allocation continues after the journal, not from zero.
        assert_eq!(state.next_seq(s.id()), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_marks_interrupted_turns_failed_and_registers_sessions() {
        // Depends on where the sessions dir resolves; serialize against tests
        // that redirect XDG_DATA_HOME.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A session killed mid-turn: turn_start with no terminal entry.
        let mut s = crate::session::Session::new("/tmp/dex-rebuild-test".into(), None).unwrap();
        let id = s.id().to_string();
        s.turn_event("turn_start").unwrap();
        assert_eq!(
            crate::session::Session::last_turn_state(s.path().unwrap()),
            "interrupted"
        );
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        state.rebuild_async().await;
        // The session is in the registry after restart.
        {
            let sessions = state.sessions.lock().unwrap();
            assert!(
                sessions.contains_key(&id),
                "registry must be rebuilt from disk"
            );
        }
        // The interrupted turn is now durably failed.
        assert_eq!(crate::session::Session::last_turn_state(&path), "failed");
        assert!(state.rebuild_complete.load(Ordering::Relaxed));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn event_seq_seed_advances_past_a_single_seq_zero() {
        // `max_event_seq` is None when empty but Some(0) for a journal
        // holding exactly seq 0; the seed must not reuse seq 0.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s = crate::session::Session::new("/tmp/dex-seq-zero-test".into(), None).unwrap();
        s.append_event(0, "{\"type\":\"system\",\"data\":\"x\"}")
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let state = DaemonState::new();
        state.seed_seq(s.id(), &path);
        assert_eq!(state.next_seq(s.id()), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_skips_failed_marking_for_live_turns() {
        // A reattach + chat racing the background rebuild owns the journal:
        // stamping `turn_failed` under its live `turn_start` would corrupt it.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut s =
            crate::session::Session::new("/tmp/dex-rebuild-live-test".into(), None).unwrap();
        let id = s.id().to_string();
        s.turn_event("turn_start").unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        state.active_turns.lock().unwrap().insert(id.clone());
        state.rebuild_async().await;
        // Still registered, but the live turn is untouched.
        assert!(state.sessions.lock().unwrap().contains_key(&id));
        assert_eq!(
            crate::session::Session::last_turn_state(&path),
            "interrupted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_registry_merge_keeps_live_entries() {
        // Sessions claimed (reattached/created) mid-rebuild win over disk via
        // `or_insert` — the rebuild must not clobber them.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let s = crate::session::Session::new("/tmp/dex-rebuild-wins-test".into(), None).unwrap();
        let id = s.id().to_string();
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        let live = SessionEntry {
            path: std::path::PathBuf::from("/tmp/dex-live-wins-marker"),
            name: None,
            cwd: "/tmp".into(),
        };
        state.sessions.lock().unwrap().insert(id.clone(), live);
        state.rebuild_async().await;
        assert_eq!(
            state.sessions.lock().unwrap().get(&id).unwrap().path,
            std::path::PathBuf::from("/tmp/dex-live-wins-marker")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A non-loopback bind without an explicit token generates one, publishes
    /// it to `daemon.token` (0600, atomically) and the server then requires
    /// it on `/api/*` while `/health` stays open. Panic-safe env restore via
    /// Drop guards; global token is re-resolvable per case (no `OnceLock`
    /// poisoning).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bearer_token_generated_for_non_loopback_bind_and_enforced() {
        struct Restore {
            xdg: Option<std::ffi::OsString>,
            token: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.xdg.take() {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
                match self.token.take() {
                    Some(v) => std::env::set_var("DEX_DAEMON_TOKEN", v),
                    None => std::env::remove_var("DEX_DAEMON_TOKEN"),
                }
                reset_daemon_token_for_tests();
            }
        }
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _restore = Restore {
            xdg: std::env::var_os("XDG_DATA_HOME"),
            token: std::env::var_os("DEX_DAEMON_TOKEN"),
        };
        let dir = std::env::temp_dir().join(format!("dex-token-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        std::env::remove_var("DEX_DAEMON_TOKEN");
        // Loopback needs no token; explicit env wins even on loopback.
        let loopback: std::net::SocketAddr = "127.0.0.1:8420".parse().unwrap();
        prepare_daemon_token(&loopback);
        assert_eq!(required_token(), None, "loopback stays unauthenticated");
        std::env::set_var("DEX_DAEMON_TOKEN", "env-wins");
        prepare_daemon_token(&loopback);
        assert_eq!(
            required_token().as_deref(),
            Some("env-wins"),
            "explicit DEX_DAEMON_TOKEN wins even on loopback"
        );
        std::env::remove_var("DEX_DAEMON_TOKEN");
        let addr: std::net::SocketAddr = "203.0.113.7:8420".parse().unwrap();
        prepare_daemon_token(&addr);
        let required = required_token();
        let token = required.expect("non-loopback bind must generate a token");
        let token_path = dir.join("dex/daemon.token");
        let written = std::fs::read_to_string(&token_path)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(token, written, "client must read the same credential");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must not be group/world readable"
            );
        }

        // Serve one route and check enforcement.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let app = server::router(std::sync::Arc::new(DaemonState::new()));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let http = reqwest::Client::new();
        // /health stays open (liveness before any credential).
        let health = http
            .get(format!("http://{bound}/health"))
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        // /api without the token: 401 (before the handler runs).
        let denied = http
            .post(format!("http://{bound}/api/sessions"))
            .json(&serde_json::json!({"cwd": "/tmp"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        // With the token: accepted (session created).
        let allowed = http
            .post(format!("http://{bound}/api/sessions"))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&serde_json::json!({"cwd": "/tmp", "name": "tok-test"}))
            .send()
            .await
            .unwrap();
        assert!(allowed.status().is_success(), "{}", allowed.status());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
