use super::super::definition::AgentDefinition;
use super::super::exit::transcript_holds_progress;
use super::super::exit::ExhaustKind;
use super::super::exit::ExitReason;
use super::super::model::AgentId;
use super::super::model::AgentInstance;
use super::super::model::AgentResult;
use super::super::model::AgentState;
use super::registry::escalate_handle;
use super::registry::AgentNotice;
use super::registry::BodyFuture;
use super::registry::BoxRun;
use super::registry::ChildInfo;
use super::registry::Inner;
use super::registry::Record;
use super::registry::RunningChild;
use super::registry::SpawnError;
use super::registry::SpawnMeta;
use super::registry::WaitOutcome;
use crate::runtime::console::CancellationToken;
use crate::runtime::unwind::CatchUnwind;
use crate::session::Session;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Max live children per session (plan §7). The next concurrent spawn
/// past this rejects fail-fast with the running list, so the model can
/// wait for or cancel a child instead of guessing.
pub const MAX_CHILDREN: usize = 4;
/// Completion notices retained per session; once full, further completions
/// fold into the [`AgentManager::take_overflow`] counter instead of growing
/// without bound.
pub const MAX_NOTICES: usize = 32;

/// Terminal results retained per session, so the `delegate` tool's
/// `wait` action can fetch them after the notice is drained.
const MAX_RESULTS: usize = 64;
/// `wait` poll quantum: prompt completion delivery without busy-spinning.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// Live progress handle for one child (plan §15: the `progress <tool>`
/// System line). Phase 5's child body reports around each tool call with
/// [`ProgressReporter::set`]; once the child's registry entry is gone the
/// no-ops are harmless. Cheap to clone; the shared `Arc` is the same one
/// the wrapper already holds.
#[derive(Clone)]
pub struct ProgressReporter {
    pub manager: Arc<Mutex<Inner>>,
    pub id: AgentId,
    /// Lifecycle hook snapshotted at spawn (see [`AgentManager::launch`]):
    /// per-line emission is the hot path (one sink line per streamed
    /// token), so it must not take the manager lock per event. The hook is
    /// attached when the manager is built, always before any spawn.
    events: Option<EventHook>,
}

impl ProgressReporter {
    /// Record the tool the child is currently running (e.g. `"bash"`).
    /// Fires the lifecycle hook (§15 V1b) when the tool actually changes,
    /// so one tool call's set/clear pair emits at most one progress event.
    /// Also bumps the registry's spend meter: the body's own sink counter
    /// dies with the body, so a wrapper-synthesized ending (panic, timeout)
    /// reads the meter back for its resume budget (§24.1).
    pub fn set(&self, tool: &str) {
        let mut inner = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        let mut fire = false;
        if let Some(child) = inner.running.get_mut(&self.id) {
            child.calls += 1;
            if child.instance.progress.as_deref() != Some(tool) {
                child.instance.progress = Some(tool.to_string());
                fire = true;
            }
        }
        let hook = inner.events.clone();
        drop(inner);
        if fire {
            if let Some(hook) = hook {
                hook(AgentEvent::Progress {
                    agent_id: self.id.clone(),
                    current_tool: Some(tool.to_string()),
                });
            }
        }
    }

    /// Clear the record when the tool call returns.
    pub fn clear(&self) {
        if let Some(child) = self
            .manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .get_mut(&self.id)
        {
            child.instance.progress = None;
        }
    }

    /// Fire one child transcript line (plan §20 child view) through the
    /// lifecycle hook snapshotted at spawn. A no-op when no hook was
    /// attached (test-built managers, non-daemon callers): the lines are a
    /// UI surface, never agent state.
    pub fn emit_line(&self, name: &str, event: crate::protocol::StreamEvent) {
        if let Some(hook) = &self.events {
            hook(AgentEvent::Line {
                agent_id: self.id.clone(),
                name: name.to_string(),
                event,
            });
        }
    }
}

/// Per-session child-agent registry. Cheap to clone; all clones share one
/// `Mutex<Inner>` so concurrent completions cannot tear registry state
/// (plan §17).
#[derive(Clone)]
pub struct AgentManager {
    pub inner: Arc<Mutex<Inner>>,
}

/// V1b typed child-agent lifecycle event (plan §15): fired through the
/// daemon's event hook at spawn, on every progress change, and on every
/// terminal path — the same single choke points the V1a `System` lines use,
/// so no lifecycle transition can bypass either encoding.
#[derive(Clone)]
pub enum AgentEvent {
    Spawned {
        agent_id: AgentId,
        name: String,
    },
    Progress {
        agent_id: AgentId,
        current_tool: Option<String>,
    },
    Completed(AgentNotice),
    /// One child transcript line (plan §20 child view): already mapped onto
    /// the wire shapes the parent turn streams, fired from the child body's
    /// sink consumer through [`ProgressReporter::emit_line`]. Journaled and
    /// broadcast through the same hook as the other variants.
    Line {
        agent_id: AgentId,
        name: String,
        event: crate::protocol::StreamEvent,
    },
}

/// The daemon-supplied lifecycle hook: typed events (§15 V1b) journaled and
/// broadcast beside the V1a `System` lines.
pub type EventHook = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Background shell-task lifecycle event (spec Rev 3): fired through the
/// daemon's task hook at spawn, per output coalesce, and on every terminal
/// path. The daemon journals + broadcasts Started/Finished, broadcasts
/// Output live-only, and schedules the idle wake on finish.
#[derive(Clone)]
pub enum TaskEvent {
    Started {
        id: String,
        command: String,
    },
    Output {
        id: String,
        chunk: String,
    },
    Finished {
        notice: crate::daemon::tasks::TaskNotice,
        status: crate::daemon::tasks::TaskStatus,
        exit_code: Option<i32>,
        duration: f64,
    },
}

/// Daemon-supplied task hook (sibling to [`EventHook`]).
pub type TaskEventHook = Arc<dyn Fn(TaskEvent) + Send + Sync>;

impl AgentManager {
    pub fn new(session: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                session: session.to_string(),
                next_counter: 0,
                closed: false,
                events: None,
                running: HashMap::new(),
                results: HashMap::new(),
                names: HashMap::new(),
                result_order: VecDeque::new(),
                notices: VecDeque::new(),
                overflowed: 0,
                bg: crate::daemon::tasks::TaskRegistry::new(),
                bg_notices: VecDeque::new(),
                bg_events: None,
                bg_handles: HashMap::new(),
            })),
        }
    }

    /// Attach the lifecycle event hook (§15 V1a `System` lines + §15 V1b
    /// typed variants). The daemon builds managers with it; test-built
    /// managers leave it `None`.
    pub fn with_events(self, hook: EventHook) -> Self {
        self.lock().events = Some(hook);
        self
    }

    /// Attach the background-task hook (spec Rev 3). Chain after
    /// `with_events` in `DaemonState::manager_for`.
    pub fn with_task_events(self, hook: TaskEventHook) -> Self {
        self.lock().bg_events = Some(hook);
        self
    }

    /// The definition name behind a live child id (§12 V1b approval
    /// labels). `None` for unknown ids and finished children — approvals
    /// only ever arrive from a running child.
    pub fn definition_name(&self, id: &AgentId) -> Option<String> {
        self.lock()
            .running
            .get(id)
            .map(|child| child.instance.definition.name.clone())
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Spawn a child agent.
    ///
    /// `run` receives the child's [`CancellationToken`], a [`ProgressReporter`]
    /// scoped to it, and its allocated [`AgentId`] (session paths and results
    /// need it, plan §4), and produces the terminal [`AgentResult`]; results
    /// are filed under the allocated id, so bodies cannot misattribute. Every
    /// terminal path — completion, failure, cancel, timeout, panic — funnels
    /// through result retention, the notice queue, the journal hook, and
    /// registry removal.
    ///
    /// The wrapper enforces the definition's `timeout` (default
    /// [`DEFAULT_AGENT_TIMEOUT`]): a run that outlasts it is dropped and
    /// ended `TimedOut` with a synthesized partial-status error. A
    /// panicking body becomes `Failed` instead of aborting the spawned task
    /// with its registry entry still live (plan §14).
    ///
    /// The instance starts `Running`: the task is spawned before this
    /// returns, so there is no observable dormant state.
    ///
    /// Never blocks: rejects synchronously when the manager is closed
    /// (post-`shutdown`) or already at [`MAX_CHILDREN`] live children
    /// ([`SpawnError::AtCapacity`], carrying the running list).
    ///
    /// `meta` carries the generation and the parent session file the
    /// transcript derives from (§24.3).
    pub fn spawn<F, Fut>(
        &self,
        def: &AgentDefinition,
        meta: SpawnMeta,
        run: F,
    ) -> Result<AgentId, SpawnError>
    where
        F: FnOnce(CancellationToken, ProgressReporter, AgentId) -> Fut + Send + 'static,
        Fut: Future<Output = AgentResult> + Send + 'static,
    {
        let run: BoxRun = Box::new(move |token, progress, id| {
            let future: BodyFuture = Box::pin(run(token, progress, id));
            future
        });
        self.launch(def, meta, run)
    }

    /// Cap check, id allocation, transcript derivation, registry insert,
    /// the cancel/timeout wrapper, handle recording, and the `Spawned`
    /// hook — one spawn implementation. Check and registration share a
    /// single lock scope, so a racing spawn can never steal the slot
    /// between the cap check and the insert.
    fn launch(
        &self,
        def: &AgentDefinition,
        meta: SpawnMeta,
        run: BoxRun,
    ) -> Result<AgentId, SpawnError> {
        let id = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SpawnError::Closed);
            }
            if inner.running.len() >= MAX_CHILDREN {
                return Err(Self::at_capacity(&inner));
            }
            Self::next_id(&mut inner)
        };
        let (token, timeout) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SpawnError::Closed);
            }
            // A racing sibling filled the last slot between the check and
            // this registration: fail fast rather than queue — the caller
            // (model or operator) waits for or cancels a child and
            // retries, with the running list naming whom.
            if inner.running.len() >= MAX_CHILDREN {
                return Err(Self::at_capacity(&inner));
            }
            Self::register(&mut inner, def, &meta, &id)
        };
        self.spawn_wrapper(id.clone(), def.name.clone(), timeout, token, run);
        // §15 V1b: the typed spawn event fires after registration, so the
        // journal order can never reference an unregistered id.
        let hook = self.lock().events.clone();
        if let Some(hook) = hook {
            hook(AgentEvent::Spawned {
                agent_id: id.clone(),
                name: def.name.clone(),
            });
        }
        Ok(id)
    }

    /// The fail-fast capacity error: names the cap and who holds it.
    fn at_capacity(inner: &Inner) -> SpawnError {
        SpawnError::AtCapacity {
            limit: MAX_CHILDREN,
            running: inner
                .running
                .iter()
                .map(|(id, child)| (id.clone(), child.instance.definition.name.clone()))
                .collect(),
        }
    }

    /// Lock-scope registration: derive the transcript path (§24.3),
    /// insert the `RunningChild`, return the fresh token + timeout.
    fn register(
        inner: &mut Inner,
        def: &AgentDefinition,
        meta: &SpawnMeta,
        id: &AgentId,
    ) -> (CancellationToken, Duration) {
        // §24.3: the transcript path derives here, once, from the same
        // helper the child body writes through — registry and file can
        // never disagree. Generations share the id; the filename
        // carries the suffix, so a resume never clobbers its parent.
        let transcript = meta
            .parent_session
            .as_deref()
            .map(|parent| Session::child_path(parent, &id.0, &def.name, meta.generation));
        let token = CancellationToken::new();
        inner.running.insert(
            id.clone(),
            RunningChild {
                instance: AgentInstance {
                    definition: def.clone(),
                    state: AgentState::Running,
                    progress: None,
                },
                token: token.clone(),
                handle: None,
                transcript,
                generation: meta.generation,
                calls: 0,
                allowance: meta
                    .remaining_budget
                    .or(def.max_tool_iterations.map(|cap| cap as usize)),
            },
        );
        (token, def.timeout)
    }

    /// The cancel/timeout wrapper + handle recording, lock-free (§14).
    fn spawn_wrapper(
        &self,
        id: AgentId,
        name: String,
        timeout: Duration,
        token: CancellationToken,
        run: BoxRun,
    ) {
        let manager = self.clone();
        let task_id = id.clone();
        // Snapshot the hook before the task spawns: the child body's
        // per-line emissions (`ProgressReporter::emit_line`) then never
        // touch the manager lock (see `ProgressReporter::events`).
        let events = manager.lock().events.clone();
        let handle = tokio::spawn(async move {
            // Cancel wins over a body that ignores its token, and the body
            // gets the definition's `timeout` to finish (plan §14): a run
            // outliving it is dropped and ended `TimedOut`. Either way the
            // wrapper funnels exactly one result through `finish`.
            let result = tokio::select! {
                result = tokio::time::timeout(
                    timeout,
                    CatchUnwind::new(
                        Box::pin(run(
                            token.clone(),
                            ProgressReporter {
                                manager: manager.inner.clone(),
                                id: task_id.clone(),
                                events,
                            },
                            task_id.clone(),
                        )),
                        "child panicked",
                    ),
                ) => match result {
                    Ok(Ok(result)) => result,
                    Ok(Err(panicked)) => AgentResult {
                        status: AgentState::Failed,
                        summary: String::new(),
                        error: Some(panicked),
                        usage: None,
                        reason: ExitReason::Transient,
                        tool_calls: 0,
                        resume: None,
                    },
                    Err(_elapsed) => AgentResult {
                        status: AgentState::TimedOut,
                        summary: String::new(),
                        error: Some(format!("timed out after {}s", timeout.as_secs())),
                        usage: None,
                        reason: ExitReason::Exhausted(ExhaustKind::Timeout),
                        tool_calls: 0,
                        resume: None,
                    },
                },
                () = token.cancelled() => AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: Some("cancelled".to_string()),
                    usage: None,
                    reason: ExitReason::ShutDown,
                    tool_calls: 0,
                    resume: None,
                },
            };
            manager.finish(&task_id, &name, result.clone());
            result
        });

        // The wrapper may already have finished (and removed its entry) for
        // an immediately-ready body; only record the handle if still live.
        if let Some(child) = self.lock().running.get_mut(&id) {
            child.handle = Some(handle);
        }
    }

    /// Session-scoped monotonic id allocation, ordered by request time.
    fn next_id(inner: &mut Inner) -> AgentId {
        let id = AgentId(format!("{}-{}", inner.session, inner.next_counter));
        inner.next_counter += 1;
        id
    }

    /// Stamp one terminal result through retention, notices, the journal
    /// hook, and registry removal. Single choke point: completion, failure,
    /// cancel, timeout, and panic all land here, so no terminal path can
    /// orphan an entry or skip its lifecycle line.
    ///
    /// Recoverable endings (interrupted, timed out, or budget-exhausted)
    /// with progress on disk additionally advertise the resume handle for
    /// a manual `delegate(resume_from)`; everything else settles
    /// handle-free. There is no automatic re-entry.
    fn finish(&self, id: &AgentId, name: &str, mut result: AgentResult) {
        // Copy out before `result` moves into retention below.
        let reason = result.reason;
        let notice = {
            let mut inner = self.lock();
            if inner.results.len() >= MAX_RESULTS {
                if let Some(oldest) = inner.result_order.pop_front() {
                    inner.results.remove(&oldest);
                    inner.names.remove(&oldest);
                }
            }
            // §24.1: bodies arrive pre-classified (`result.reason`);
            // wrapper-synthesized endings were classified at their arm.
            // Nothing here reads prose. Named snapshot of the registry
            // entry: `None` only for an id that was never registered
            // (defensive — wrappers always register first); then the
            // result settles handle-free.
            let record: Option<Record> = inner.running.get(id).map(|child| Record {
                generation: child.generation,
                transcript: child.transcript.clone(),
                allowance: child.allowance,
                calls: child.calls,
                model: child.instance.definition.model.clone(),
            });
            // Spend reconciliation (§24.1): the body's own count is
            // authoritative while it lives; the registry meter survives a
            // panicking or timed-out body. Both count the same `set()`
            // events, so `max` is exact either way.
            let tool_calls = result
                .tool_calls
                .max(record.as_ref().map(|record| record.calls).unwrap_or(0));
            let progress_made = tool_calls > 0
                || !result.summary.trim().is_empty()
                || record
                    .as_ref()
                    .and_then(|record| record.transcript.as_deref())
                    .is_some_and(transcript_holds_progress);
            // A recoverable ending with progress advertises the handle
            // for a manual re-entry; terminal-by-intent endings
            // (finished, cancelled, failed without progress) settle
            // handle-free.
            if matches!(reason, ExitReason::Transient | ExitReason::Exhausted(_)) && progress_made {
                let handle = record
                    .as_ref()
                    .and_then(|record| escalate_handle(record, id, reason, tool_calls));
                if let Some(handle) = handle {
                    result.resume = Some(handle);
                }
            }
            let status = result.status;
            let resumable = result.resume.is_some();
            inner.result_order.push_back(id.clone());
            inner.names.insert(id.clone(), name.to_string());
            inner.results.insert(id.clone(), result);
            let notice = AgentNotice {
                agent_id: id.clone(),
                name: name.to_string(),
                status,
                // Copied from the retained result, so the notice can
                // never disagree with the record the parent later
                // fetches.
                usage: inner.results.get(id).and_then(|result| result.usage),
                resumable,
            };
            if inner.notices.len() >= MAX_NOTICES {
                inner.overflowed += 1;
            } else {
                inner.notices.push_back(notice.clone());
            }
            inner.running.remove(id);
            notice
        };
        let hook = self.lock().events.clone();
        if let Some(hook) = hook {
            hook(AgentEvent::Completed(notice));
        }
    }

    /// Bounded wait: the retained result if terminal, the last-seen state
    /// if `timeout` elapses first, `Unknown` for an id this manager never
    /// spawned (or whose result aged out of retention).
    pub async fn wait(&self, id: &AgentId, timeout: Duration) -> WaitOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                if let Some(result) = inner.results.get(id) {
                    return WaitOutcome::Finished(Box::new(result.clone()));
                }
                if let Some(child) = inner.running.get(id) {
                    if tokio::time::Instant::now() >= deadline {
                        return WaitOutcome::Running(child.instance.state);
                    }
                } else {
                    return WaitOutcome::Unknown;
                }
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Current lifecycle state, live or terminal. `None` for unknown ids.
    /// Cancel-surfaced states arrive here once the wrapper funnels them
    /// through `finish` — `cancel` itself only signals the token.
    /// Point-in-time view for the `delegate` `list` action (§24.3): live children
    /// with their progress label, plus retained terminal results with
    /// their resumability. Sorted by id for a stable render.
    pub fn snapshot(&self) -> Vec<ChildInfo> {
        let inner = self.lock();
        let mut out: Vec<ChildInfo> = inner
            .running
            .iter()
            .map(|(id, child)| ChildInfo {
                agent_id: id.clone(),
                name: child.instance.definition.name.clone(),
                state: AgentState::Running,
                progress: child.instance.progress.clone(),
                resumable: false,
                transcript: child.transcript.clone(),
            })
            .collect();
        out.extend(inner.results.iter().map(|(id, result)| {
            ChildInfo {
                agent_id: id.clone(),
                name: inner.names.get(id).cloned().unwrap_or_default(),
                state: result.status,
                progress: None,
                resumable: result.resume.is_some(),
                transcript: result
                    .resume
                    .as_ref()
                    .map(|handle| handle.transcript.clone()),
            }
        }));
        out.sort_by(|a, b| a.agent_id.0.cmp(&b.agent_id.0));
        out
    }

    /// Test-only: the parent observes cancellation through the child's
    /// exit result, not by polling state.
    #[cfg(test)]
    pub fn status(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            return Some(child.instance.state);
        }
        inner.results.get(id).map(|result| result.status)
    }

    /// Signal the child's token. Returns the last-seen state (`None` for
    /// unknown ids); the `Cancelled` result itself lands via `finish` once
    /// the wrapper observes the token. Signalling a finished id is a
    /// harmless no-op lookup.
    pub fn cancel(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            child.token.cancel();
            return Some(child.instance.state);
        }
        inner.results.get(id).map(|result| result.status)
    }

    pub fn progress(&self, id: &AgentId) -> Option<String> {
        self.lock()
            .running
            .get(id)
            .and_then(|child| child.instance.progress.clone())
    }

    /// Drain queued completion notices, oldest first. Results stay
    /// retained — draining never loses fetchability.
    pub fn drain_notices(&self) -> Vec<AgentNotice> {
        self.lock().notices.drain(..).collect()
    }

    /// Completions dropped while the notice queue was full. Resets to zero;
    /// the Phase 6 drain site renders a non-zero take as "N more children
    /// finished — ask for specifics".
    pub fn take_overflow(&self) -> usize {
        std::mem::take(&mut self.lock().overflowed)
    }

    /// Any queued completion notices (the idle wake's fire condition,
    /// §10b V1b — a peek, not a drain).
    pub fn has_notices(&self) -> bool {
        !self.lock().notices.is_empty()
    }

    /// Live children. The daemon shutdown path (§17) joins until this
    /// reaches zero.
    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.lock().running.len()
    }

    /// Cancel every live child, join their tasks, and close the manager.
    /// After this returns, every child has funneled through `finish`
    /// (results + notices retained), `active_count` is zero, and `spawn`
    /// rejects: a joined handle means its wrapper already stored and
    /// removed its entry, and a stale clone cannot respawn into this
    /// registry.
    pub async fn shutdown(&self) {
        let handles = {
            let mut inner = self.lock();
            inner.closed = true;
            for child in inner.running.values() {
                child.token.cancel();
            }
            inner
                .running
                .values_mut()
                .filter_map(|child| child.handle.take())
                .collect::<Vec<JoinHandle<AgentResult>>>()
        };
        for handle in handles {
            let _ = handle.await;
        }
        self.shutdown_bg_tasks().await;
    }

    // ---- Background shell tasks (spec Rev 3) ----

    /// Insert a new running background task; fires the Started hook outside
    /// the lock. `Err` past the 8-running cap.
    pub fn bg_spawn(&self, command: String, cwd: std::path::PathBuf) -> Result<String, String> {
        let (id, hook) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err("agent manager is shut down; no new children can start".to_string());
            }
            let id = inner.bg.spawn(command.clone(), cwd)?;
            let hook = inner.bg_events.clone();
            (id, hook)
        };
        if let Some(hook) = hook {
            hook(TaskEvent::Started {
                id: id.clone(),
                command,
            });
        }
        Ok(id)
    }

    pub fn bg_set_pid(&self, id: &str, pid: u32) {
        if let Some(task) = self.lock().bg.get_mut(id) {
            task.pid = Some(pid);
        }
    }

    pub fn bg_set_handle(&self, id: &str, handle: JoinHandle<()>) {
        self.lock().bg_handles.insert(id.to_string(), handle);
    }

    /// Append output bytes; returns the new virtual length. Pure accounting
    /// via `daemon::tasks::pure::push_bytes`.
    pub fn bg_append(&self, id: &str, chunk: &[u8], cap: usize) -> Option<u64> {
        let mut inner = self.lock();
        let task = inner.bg.get_mut(id)?;
        crate::daemon::tasks::pure::push_bytes(
            &mut task.buf,
            &mut task.total_written,
            &mut task.dropped_prefix,
            chunk,
            cap,
        );
        Some(task.total_written)
    }

    /// Fire one coalesced live-output chunk (broadcast-only).
    pub fn bg_emit_output(&self, id: &str, chunk: String) {
        let hook = self.lock().bg_events.clone();
        if let Some(hook) = hook {
            hook(TaskEvent::Output {
                id: id.to_string(),
                chunk,
            });
        }
    }

    /// Mark terminal, queue the notice, fire the Finished hook outside the
    /// lock. Returns the notice for the caller (wake scheduling reuses it).
    pub fn bg_finish(
        &self,
        id: &str,
        status: crate::daemon::tasks::TaskStatus,
        exit_code: Option<i32>,
    ) -> Option<crate::daemon::tasks::TaskNotice> {
        let (notice, hook, duration) = {
            let mut inner = self.lock();
            let task = inner.bg.get_mut(id)?;
            if !task.is_running() {
                return None;
            }
            let duration = task.started_at.elapsed().as_secs_f64();
            // Tail for the notice: last 512 bytes decoded lossy.
            let tail_len = task.buf.len().min(512);
            let tail_bytes: Vec<u8> = task
                .buf
                .iter()
                .skip(task.buf.len() - tail_len)
                .copied()
                .collect();
            let tail = crate::daemon::tasks::pure::decode_lossy(&tail_bytes);
            let word = status.word();
            let notice = crate::daemon::tasks::TaskNotice {
                id: id.to_string(),
                command: task.command.clone(),
                status: word,
                tail,
            };
            inner.bg.finish(id, status.clone());
            inner.bg_notices.push_back(notice.clone());
            inner.bg_handles.remove(id);
            (notice, inner.bg_events.clone(), duration)
        };
        if let Some(hook) = hook {
            hook(TaskEvent::Finished {
                notice: notice.clone(),
                status,
                exit_code,
                duration,
            });
        }
        Some(notice)
    }

    /// Read the new byte range for `output`/`wait`: resolved virtual slice
    /// plus a copy of the buffered bytes in `[start, end)`.
    pub fn bg_read(
        &self,
        id: &str,
        cursor: Option<u64>,
    ) -> Result<(crate::daemon::tasks::pure::Slice, Vec<u8>, String, bool), String> {
        let inner = self.lock();
        let task = inner
            .bg
            .get(id)
            .ok_or_else(|| format!("unknown background task '{id}': never spawned in this session, or its result aged out of retention"))?;
        let slice = crate::daemon::tasks::pure::slice_range(
            task.total_written,
            task.dropped_prefix,
            task.buf.len(),
            cursor,
        );
        // Map virtual offsets back into the buffer: buffer covers
        // `[total - buf.len(), total)`.
        let buf_start = task.total_written.saturating_sub(task.buf.len() as u64);
        let from = slice.start.saturating_sub(buf_start) as usize;
        let to = slice.end.saturating_sub(buf_start) as usize;
        let bytes: Vec<u8> = task
            .buf
            .iter()
            .skip(from)
            .take(to.saturating_sub(from))
            .copied()
            .collect();
        Ok((slice, bytes, task.status.word(), task.is_running()))
    }

    /// Pid + abort handle for `stop`/teardown. The drain observes the death
    /// and records `Killed` via `bg_finish`.
    pub fn bg_take_handle(&self, id: &str) -> Option<JoinHandle<()>> {
        self.lock().bg_handles.remove(id)
    }

    pub fn bg_pid(&self, id: &str) -> Option<u32> {
        self.lock().bg.get(id)?.pid
    }

    pub fn bg_status_word(&self, id: &str) -> Option<String> {
        self.lock().bg.get(id).map(|t| t.status.word())
    }

    pub fn bg_is_running(&self, id: &str) -> Option<bool> {
        self.lock().bg.get(id).map(|t| t.is_running())
    }

    /// One line per task, running first then id order (for `list`).
    pub fn bg_snapshot(&self) -> Vec<(String, String, String)> {
        let inner = self.lock();
        // TaskRegistry has no iterator; rebuild via finished_order is
        // private — snapshot through a debug-free path: ids are
        // `task-1..=counter`. Probe sequentially (counter is small).
        let mut out = Vec::new();
        let mut n = 1u64;
        loop {
            let id = format!("task-{n}");
            match inner.bg.get(&id) {
                Some(task) => {
                    let elapsed = task
                        .ended_at
                        .unwrap_or_else(std::time::Instant::now)
                        .saturating_duration_since(task.started_at);
                    let age = if elapsed.as_secs() >= 60 {
                        format!("{}m", elapsed.as_secs() / 60)
                    } else {
                        format!("{}s", elapsed.as_secs())
                    };
                    out.push((id, task.status.word(), format!("{age}  {}", task.command)));
                }
                None => {
                    // Ids are dense from 1..=counter minus evicted finished;
                    // stop probing well past any live id.
                    if n > inner.bg.task_count_hint() {
                        break;
                    }
                }
            }
            n += 1;
            if n > 4096 {
                break;
            }
        }
        out.sort_by(|a, b| {
            let arun = a.1 == "running";
            let brun = b.1 == "running";
            brun.cmp(&arun).then(a.0.cmp(&b.0))
        });
        out
    }

    pub fn has_task_notices(&self) -> bool {
        !self.lock().bg_notices.is_empty()
    }

    /// Any queued notices at all (agents or tasks) — the idle wake's fire
    /// condition.
    pub fn has_any_notices(&self) -> bool {
        let inner = self.lock();
        !inner.notices.is_empty() || !inner.bg_notices.is_empty()
    }

    pub fn drain_task_notices(&self) -> Vec<crate::daemon::tasks::TaskNotice> {
        self.lock().bg_notices.drain(..).collect()
    }

    /// Kill all running tasks (pid-group) and join their drains. Daemon
    /// shutdown / session-delete path; turn cancel never calls this.
    pub async fn shutdown_bg_tasks(&self) {
        let (pids, handles): (Vec<u32>, Vec<JoinHandle<()>>) = {
            let mut inner = self.lock();
            let pids: Vec<u32> = inner
                .bg_handles
                .keys()
                .filter_map(|id| inner.bg.get(id)?.pid)
                .collect();
            let handles: Vec<JoinHandle<()>> = inner.bg_handles.drain().map(|(_, h)| h).collect();
            (pids, handles)
        };
        for pid in pids {
            crate::tools::shell::kill_process_group_pid(pid);
        }
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }
}
