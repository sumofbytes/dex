//! Phase 4 — per-session child-agent lifecycle: registry, spawn cap,
//! bounded wait, cancel, timeout, and completion notices.
//!
//! The manager owns *mechanics only*: id allocation, the spawn cap,
//! cancellation tokens, task handles, result retention, and the notice
//! queue. It never builds prompts, touches the model, or interprets
//! results — the caller supplies the child body at [`AgentManager::spawn`]
//! (Phase 5's delegate tool builds it from the parent turn; Phase 7 adds
//! tool filtering and the model override). Tests inject mock bodies,
//! which keeps every lifecycle path hermetic.
//!
//! Mechanics the wrapper enforces here so no terminal path can orphan a
//! registry entry (plan §14): body panics funnel a synthesized `Failed`
//! result through `finish`; a run outliving its definition's `timeout`
//! ends `TimedOut`; and after `shutdown` the manager is closed, so a
//! stale clone cannot orphan a child into a registry nobody will join.
//!
//! Note on the plan (§7, §14): the sketch shows `spawn(def, seed)` with the
//! child body implied. The body arrives as an argument instead, because the
//! manager must not contain model logic and the only code that can build
//! the child future (parent-turn config, client, tools, policy) lives with
//! the caller. Same seam, one parameter wider.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::core::console::CancellationToken;
use crate::core::unwind::CatchUnwind;
use crate::session::Session;

use super::context::ContextSeed;
use super::definition::AgentDefinition;
use super::exit::{
    on_exit, resume_note, transcript_holds_progress, ExhaustKind, ExitReason, OnExit, ResumeHandle,
};
use super::instance::{AgentId, AgentInstance, AgentState};
use super::result::{AgentResult, AgentUsage};

/// Max live children per session (plan §7). The 5th concurrent spawn is
/// rejected with the running list so the caller can wait on or cancel one
/// instead of fanning out.
pub(crate) const MAX_CHILDREN: usize = 4;
/// Completion notices retained per session; once full, further completions
/// fold into the [`AgentManager::take_overflow`] counter instead of growing
/// without bound.
pub(crate) const MAX_NOTICES: usize = 32;
/// Terminal results retained per session, so `wait` (and Phase 5's
/// `delegate_output`) can fetch them after the notice is drained.
const MAX_RESULTS: usize = 64;
/// `wait` poll quantum: prompt completion delivery without busy-spinning.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// Completion announcement queued for the Phase 6 drain site (parent turn
/// end, which renders these into context). Notices are informational only —
/// the retained [`AgentResult`] is the source of truth.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentNotice {
    pub(crate) agent_id: AgentId,
    pub(crate) name: String,
    pub(crate) status: AgentState,
    /// Child token spend, when the body reported any (§18: the lifecycle
    /// line carries it so client-side spend accounting stays honest).
    pub(crate) usage: Option<AgentUsage>,
    /// True when the retained result carries a [`ResumeHandle`]: the prose
    /// advertises `delegate(resume_from = …)`. Never part of the
    /// `[agent …] finished …` prefix the TUI matches on (§24.1:
    /// resumability is notice prose only, zero wire change).
    pub(crate) resumable: bool,
}

impl AgentNotice {
    /// The §15 V1a lifecycle line: the stable `[agent <name>:<id>] finished
    /// <status>` prefix the TUI matches on, plus the child's token spend
    /// when reported and a resume hint when the result carries a handle.
    /// Cost is shown only when priced (an unpriced model renders no
    /// `$0.0000` noise). Pure; unit-tested.
    pub(crate) fn text(&self) -> String {
        let mut text = format!(
            "[agent {}:{}] finished {}",
            self.name,
            self.agent_id,
            super::status_word(self.status)
        );
        if let Some(usage) = self.usage {
            text.push_str(&format!(
                " · {} tok",
                crate::ui::format_tokens(usage.prompt_tokens + usage.output_tokens)
            ));
            if usage.cost_usd > 0.0 {
                text.push_str(&format!(" · ${:.4}", usage.cost_usd));
            }
        }
        if self.resumable {
            text.push_str(&format!(
                " · resumable with delegate(resume_from = \"{0}\")",
                self.agent_id
            ));
        }
        text
    }
}

/// One row of the `delegate_list` view (§24.3): live or retained.
#[derive(Clone, Debug)]
pub(crate) struct ChildInfo {
    pub(crate) agent_id: AgentId,
    pub(crate) name: String,
    pub(crate) state: AgentState,
    pub(crate) progress: Option<String>,
    pub(crate) resumable: bool,
    pub(crate) transcript: Option<PathBuf>,
}

/// Bounded-wait outcome for [`AgentManager::wait`].
#[derive(Debug, PartialEq)]
pub(crate) enum WaitOutcome {
    /// Terminal result; also retained for later fetch after notice drain.
    Finished(AgentResult),
    /// Still live when `timeout` elapsed; carries the last-seen state.
    Running(AgentState),
    /// Unknown id: never spawned here, or its result aged out of retention.
    Unknown,
}

/// `spawn` rejection. `AtCapacity` carries the running list so the caller
/// (model or operator) can pick a waiter instead of guessing; `Closed`
/// means the manager was shut down — no new children, ever.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpawnError {
    AtCapacity {
        limit: usize,
        running: Vec<(AgentId, String)>,
    },
    Closed,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtCapacity { limit, running } => {
                let list = running
                    .iter()
                    .map(|(id, name)| format!("{id} ({name})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "at child capacity ({limit} running): {list}; \
                     wait for or cancel one before spawning"
                )
            }
            Self::Closed => {
                write!(f, "agent manager is shut down; no new children can start")
            }
        }
    }
}

impl std::error::Error for SpawnError {}

/// Live progress handle for one child (plan §15: the `progress <tool>`
/// System line). Phase 5's child body reports around each tool call with
/// [`ProgressReporter::set`]; once the child's registry entry is gone the
/// no-ops are harmless. Cheap to clone; the shared `Arc` is the same one
/// the wrapper already holds.
#[derive(Clone)]
pub(crate) struct ProgressReporter {
    manager: Arc<Mutex<Inner>>,
    id: AgentId,
}

impl ProgressReporter {
    /// Record the tool the child is currently running (e.g. `"bash"`).
    /// Fires the lifecycle hook (§15 V1b) when the tool actually changes,
    /// so one tool call's set/clear pair emits at most one progress event.
    pub(crate) fn set(&self, tool: &str) {
        let mut inner = self.manager.lock().unwrap_or_else(|e| e.into_inner());
        let mut fire = false;
        if let Some(child) = inner.running.get_mut(&self.id) {
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
    pub(crate) fn clear(&self) {
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
}

/// Per-session child-agent registry. Cheap to clone; all clones share one
/// `Mutex<Inner>` so concurrent completions cannot tear registry state
/// (plan §17).
#[derive(Clone)]
pub(crate) struct AgentManager {
    inner: Arc<Mutex<Inner>>,
}

/// V1b typed child-agent lifecycle event (plan §15): fired through the
/// daemon's event hook at spawn, on every progress change, and on every
/// terminal path — the same single choke points the V1a `System` lines use,
/// so no lifecycle transition can bypass either encoding.
pub(crate) enum AgentEvent {
    Spawned {
        agent_id: AgentId,
        name: String,
    },
    Progress {
        agent_id: AgentId,
        current_tool: Option<String>,
    },
    Completed(AgentNotice),
}

/// The daemon-supplied lifecycle hook: typed events (§15 V1b) journaled and
/// broadcast beside the V1a `System` lines.
type EventHook = Arc<dyn Fn(AgentEvent) + Send + Sync>;

struct Inner {
    session: String,
    next_counter: u64,
    /// Set by `shutdown`: after it, `spawn` rejects. A stale `AgentManager`
    /// clone (e.g. held by an in-flight tool call) cannot then orphan a
    /// child into a registry nobody will ever join (plan §14).
    closed: bool,
    /// Set by the daemon (`AgentManager::with_events`): invoked on every
    /// terminal path with the completion notice, so child lifecycle lines
    /// (§15 V1a `[agent <name>:<id>] finished <status>`) are journaled at
    /// completion time even while no turn is live. `None` for test-built
    /// managers.
    events: Option<EventHook>,
    running: HashMap<AgentId, RunningChild>,
    results: HashMap<AgentId, AgentResult>,
    /// Display names for retained results (results carry no name — the
    /// parent's `delegate_list` needs one). Pruned with `results`.
    names: HashMap<AgentId, String>,
    /// Insertion order of `results`, for oldest-first eviction.
    result_order: VecDeque<AgentId>,
    notices: VecDeque<AgentNotice>,
    /// Completions dropped because `notices` was full.
    overflowed: usize,
}

struct RunningChild {
    instance: AgentInstance,
    token: CancellationToken,
    handle: Option<JoinHandle<AgentResult>>,
    /// Derived transcript path (§16 + §24.3 generations), when the spawn
    /// knew the parent session file. `None` for test-built managers —
    /// then no resume handle is ever advertised.
    transcript: Option<PathBuf>,
    /// The generation this child runs as (0 = fresh). Resume spawns +1.
    generation: u32,
}

/// What `spawn` needs beyond definition + seed (§24.3): which generation
/// this child is, and the parent session file its transcript derives
/// from. Fresh spawns use `SpawnMeta::fresh()` (generation 0, no
/// transcript tracking — unit tests never touch the filesystem).
#[derive(Clone, Debug)]
pub(crate) struct SpawnMeta {
    pub(crate) generation: u32,
    pub(crate) parent_session: Option<PathBuf>,
}

impl SpawnMeta {
    pub(crate) fn fresh() -> Self {
        Self {
            generation: 0,
            parent_session: None,
        }
    }
}

impl AgentManager {
    pub(crate) fn new(session: &str) -> Self {
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
            })),
        }
    }

    /// Attach the lifecycle event hook (§15 V1a `System` lines + §15 V1b
    /// typed variants). The daemon builds managers with it; test-built
    /// managers leave it `None`.
    pub(crate) fn with_events(self, hook: EventHook) -> Self {
        self.lock().events = Some(hook);
        self
    }

    /// The definition name behind a live child id (§12 V1b approval
    /// labels). `None` for unknown ids and finished children — approvals
    /// only ever arrive from a running child.
    pub(crate) fn definition_name(&self, id: &AgentId) -> Option<String> {
        self.lock()
            .running
            .get(id)
            .map(|child| child.instance.definition.name.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
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
    /// returns, so `Pending` is unobservable and would only race status
    /// checks. (`Pending` stays for future queued spawns.)
    ///
    /// Never blocks: rejects synchronously when the manager is closed
    /// (post-`shutdown`) or already at [`MAX_CHILDREN`] live children.
    ///
    /// `meta` carries the generation and the parent session file the
    /// transcript derives from (§24.3).
    pub(crate) fn spawn<F, Fut>(
        &self,
        def: &AgentDefinition,
        seed: ContextSeed,
        meta: SpawnMeta,
        run: F,
    ) -> Result<AgentId, SpawnError>
    where
        F: FnOnce(CancellationToken, ProgressReporter, AgentId) -> Fut + Send + 'static,
        Fut: Future<Output = AgentResult> + Send + 'static,
    {
        let (id, token, timeout) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SpawnError::Closed);
            }
            if inner.running.len() >= MAX_CHILDREN {
                let running = inner
                    .running
                    .iter()
                    .map(|(id, child)| (id.clone(), child.instance.definition.name.clone()))
                    .collect();
                return Err(SpawnError::AtCapacity {
                    limit: MAX_CHILDREN,
                    running,
                });
            }
            let id = AgentId(format!("{}-{}", inner.session, inner.next_counter));
            inner.next_counter += 1;
            let token = CancellationToken::new();
            // §24.3: the transcript path derives here, once, from the same
            // helper the child body writes through — registry and file can
            // never disagree. Generations share the id; the filename
            // carries the suffix, so a resume never clobbers its parent.
            let transcript = meta
                .parent_session
                .as_deref()
                .map(|parent| Session::child_path(parent, &id.0, &def.name, meta.generation));
            inner.running.insert(
                id.clone(),
                RunningChild {
                    instance: AgentInstance {
                        id: id.clone(),
                        definition: def.clone(),
                        parent_id: None,
                        context: seed,
                        state: AgentState::Running,
                        progress: None,
                    },
                    token: token.clone(),
                    handle: None,
                    transcript,
                    generation: meta.generation,
                },
            );
            (id, token, def.timeout)
        };

        let manager = self.clone();
        let name = def.name.clone();
        let spawn_name = name.clone();
        let task_id = id.clone();
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
        // §15 V1b: the typed spawn event fires after registration, so the
        // journal order can never reference an unregistered id.
        let hook = self.lock().events.clone();
        if let Some(hook) = hook {
            hook(AgentEvent::Spawned {
                agent_id: id.clone(),
                name: spawn_name,
            });
        }
        Ok(id)
    }

    /// Stamp one terminal result through retention, notices, the journal
    /// hook, and registry removal. Single choke point: completion, failure,
    /// cancel, timeout, and panic all land here, so no terminal path can
    /// orphan an entry or skip its lifecycle line.
    fn finish(&self, id: &AgentId, name: &str, mut result: AgentResult) {
        let mut inner = self.lock();
        if inner.results.len() >= MAX_RESULTS {
            if let Some(oldest) = inner.result_order.pop_front() {
                inner.results.remove(&oldest);
                inner.names.remove(&oldest);
            }
        }
        // §24.1 + §24.4: the single decision point. Bodies arrive
        // pre-classified (`result.reason`); wrapper-synthesized endings
        // were classified at their arm. Nothing here reads prose.
        // `remaining_budget` needs the definition's cap, which rides in
        // the registry entry: a timeout keeps the full meter (spend
        // unknown), a budget exhaustion keeps the honest remainder.
        let record = inner.running.get(id).map(|child| {
            (
                child.generation,
                child.transcript.clone(),
                child.instance.definition.max_tool_iterations,
            )
        });
        let progress_made = result.tool_calls > 0
            || !result.summary.trim().is_empty()
            || record
                .as_ref()
                .and_then(|(_, transcript, _)| transcript.as_deref())
                .is_some_and(transcript_holds_progress);
        if on_exit(result.reason, progress_made) == OnExit::EscalateWithResume {
            if let Some((generation, Some(transcript), budget)) = record {
                let remaining =
                    budget.map(|cap| (cap as usize).saturating_sub(result.tool_calls as usize));
                result.resume = Some(ResumeHandle {
                    agent_id: id.clone(),
                    transcript,
                    generation,
                    remaining_budget: remaining,
                    note: resume_note(result.reason, result.tool_calls),
                });
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
            // Copied from the retained result, so the notice can never
            // disagree with the record the parent later fetches.
            usage: inner.results.get(id).and_then(|result| result.usage),
            resumable,
        };
        if inner.notices.len() >= MAX_NOTICES {
            inner.overflowed += 1;
        } else {
            inner.notices.push_back(notice.clone());
        }
        inner.running.remove(id);
        // Outside the lock: the hook journals through the daemon's own seq
        // mutex, and its file IO must never block registry access.
        let hook = inner.events.clone();
        drop(inner);
        if let Some(hook) = hook {
            hook(AgentEvent::Completed(notice));
        }
    }

    /// Bounded wait: the retained result if terminal, the last-seen state
    /// if `timeout` elapses first, `Unknown` for an id this manager never
    /// spawned (or whose result aged out of retention).
    pub(crate) async fn wait(&self, id: &AgentId, timeout: Duration) -> WaitOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                if let Some(result) = inner.results.get(id) {
                    return WaitOutcome::Finished(result.clone());
                }
                match inner.running.get(id) {
                    Some(child) => {
                        if tokio::time::Instant::now() >= deadline {
                            return WaitOutcome::Running(child.instance.state);
                        }
                    }
                    None => return WaitOutcome::Unknown,
                }
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Current lifecycle state, live or terminal. `None` for unknown ids.
    /// Cancel-surfaced states arrive here once the wrapper funnels them
    /// through `finish` — `cancel` itself only signals the token.
    /// Point-in-time view for `delegate_list` (§24.3): live children
    /// with their progress label, plus retained terminal results with
    /// their resumability. Sorted by id for a stable render.
    pub(crate) fn snapshot(&self) -> Vec<ChildInfo> {
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

    pub(crate) fn status(&self, id: &AgentId) -> Option<AgentState> {
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
    pub(crate) fn cancel(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            child.token.cancel();
            return Some(child.instance.state);
        }
        inner.results.get(id).map(|result| result.status)
    }

    /// Tool the child last reported through its [`ProgressReporter`], if
    /// any. `None` for unknown ids (never spawned, finished, or evicted).
    pub(crate) fn progress(&self, id: &AgentId) -> Option<String> {
        self.lock()
            .running
            .get(id)
            .and_then(|child| child.instance.progress.clone())
    }

    /// Drain queued completion notices, oldest first. Results stay
    /// retained — draining never loses fetchability.
    pub(crate) fn drain_notices(&self) -> Vec<AgentNotice> {
        self.lock().notices.drain(..).collect()
    }

    /// Completions dropped while the notice queue was full. Resets to zero;
    /// the Phase 6 drain site renders a non-zero take as "N more children
    /// finished — ask for specifics".
    pub(crate) fn take_overflow(&self) -> usize {
        std::mem::take(&mut self.lock().overflowed)
    }

    /// Any queued completion notices (the idle wake's fire condition,
    /// §10b V1b — a peek, not a drain).
    pub(crate) fn has_notices(&self) -> bool {
        !self.lock().notices.is_empty()
    }

    /// Live children. The daemon shutdown path (§17) joins until this
    /// reaches zero.
    pub(crate) fn active_count(&self) -> usize {
        self.lock().running.len()
    }

    /// Cancel every live child, join their tasks, and close the manager.
    /// After this returns, every child has funneled through `finish`
    /// (results + notices retained), `active_count` is zero, and `spawn`
    /// rejects: a joined handle means its wrapper already stored and
    /// removed its entry, and a stale clone cannot respawn into this
    /// registry.
    pub(crate) async fn shutdown(&self) {
        let handles: Vec<JoinHandle<AgentResult>> = {
            let mut inner = self.lock();
            inner.closed = true;
            for child in inner.running.values() {
                child.token.cancel();
            }
            inner
                .running
                .values_mut()
                .filter_map(|child| child.handle.take())
                .collect()
        };
        for handle in handles {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::definition::PermissionInherit;
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::Barrier;

    fn test_def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: format!("{name} test agent"),
            prompt: String::new(),
            model: None,
            tools: ["read".to_string()].into_iter().collect(),
            permissions: PermissionInherit::Inherit,
            max_tool_iterations: None,
            timeout: Duration::from_secs(60),
        }
    }

    fn test_seed() -> ContextSeed {
        ContextSeed {
            task: "do the thing".to_string(),
            file_hints: vec![PathBuf::from("src/main.rs")],
            parent_summary: None,
        }
    }

    fn completed(summary: &str) -> AgentResult {
        AgentResult {
            status: AgentState::Completed,
            summary: summary.to_string(),
            error: None,
            reason: ExitReason::Normal,
            tool_calls: 0,
            resume: None,
            usage: None,
        }
    }

    fn failed(error: &str) -> AgentResult {
        AgentResult {
            status: AgentState::Failed,
            summary: String::new(),
            error: Some(error.to_string()),
            usage: None,
            reason: ExitReason::Permanent,
            tool_calls: 0,
            resume: None,
        }
    }

    /// A body that only ends through its token — the shape Phase 7's real
    /// runner has. Lets tests hold children live without sleeps.
    async fn token_body(
        token: CancellationToken,
        _progress: ProgressReporter,
        _id: AgentId,
    ) -> AgentResult {
        token.cancelled().await;
        AgentResult {
            status: AgentState::Cancelled,
            summary: String::new(),
            error: Some("child saw cancel".to_string()),
            usage: None,
            reason: ExitReason::ShutDown,
            tool_calls: 0,
            resume: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn journal_hook_fires_on_every_terminal_path() {
        // §15 V1a: the daemon's hook observes every terminal path — the
        // single choke point means cancel and panic are journaled too.
        // §15 V1b: the same hook now carries the typed events; spawn and
        // terminal completions must both fire through it.
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let capture = seen.clone();
        let mgr = AgentManager::new("sess").with_events(Arc::new(move |event| {
            let text = match event {
                AgentEvent::Spawned { agent_id, .. } => format!("spawned {agent_id}"),
                AgentEvent::Progress { agent_id, .. } => format!("progress {agent_id}"),
                AgentEvent::Completed(notice) => {
                    format!(
                        "{} {}",
                        notice.agent_id,
                        super::super::status_word(notice.status)
                    )
                }
            };
            capture.lock().unwrap_or_else(|e| e.into_inner()).push(text);
        }));
        let completed = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("findings") },
            )
            .unwrap();
        let cancelled_id = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert!(matches!(
            mgr.wait(&completed, Duration::from_secs(5)).await,
            WaitOutcome::Finished(_)
        ));
        mgr.cancel(&cancelled_id);
        assert!(matches!(
            mgr.wait(&cancelled_id, Duration::from_secs(5)).await,
            WaitOutcome::Finished(_)
        ));
        let entries = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(
            entries,
            vec![
                "spawned sess-0".to_string(),
                "spawned sess-1".to_string(),
                "sess-0 completed".to_string(),
                "sess-1 cancelled".to_string(),
            ]
        );
        // Finished children leave the live registry (§12 V1b): approvals
        // only ever arrive from a running child, so the label lookup is
        // `None` for a terminal id.
        assert_eq!(mgr.definition_name(&cancelled_id), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_assigns_session_scoped_ids_and_reports_running() {
        let mgr = AgentManager::new("sess");
        // `spawn` never yields before returning, so on a single-threaded
        // runtime the wrapper cannot have run yet: fully deterministic.
        let first = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("findings") },
            )
            .unwrap();
        let second = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("pass") },
            )
            .unwrap();
        assert_eq!(first.to_string(), "sess-0");
        assert_eq!(second.to_string(), "sess-1");
        assert_eq!(mgr.status(&first), Some(AgentState::Running));
        assert_eq!(mgr.active_count(), 2);
        // The §12 V1b approval label resolves from the live registry: a
        // running child's definition name is available the moment a parked
        // approval needs it.
        assert_eq!(mgr.definition_name(&first), Some("explorer".to_string()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completing_child_files_result_and_notice() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("findings") },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert_eq!(result.summary, "findings");
                assert_eq!(result.error, None);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.status(&id), Some(AgentState::Completed));
        assert_eq!(mgr.active_count(), 0);
        let notices = mgr.drain_notices();
        assert_eq!(
            notices,
            vec![AgentNotice {
                agent_id: id,
                name: "explorer".to_string(),
                status: AgentState::Completed,
                usage: None,
                resumable: false,
            }]
        );
        assert_eq!(mgr.take_overflow(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_result_preserved_with_error() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { failed("boom") },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.error.as_deref(), Some("boom"));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, AgentState::Failed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_times_out_then_finishes() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    completed("late")
                },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_millis(50)).await {
            WaitOutcome::Running(state) => assert_eq!(state, AgentState::Running),
            other => panic!("expected Running, got {other:?}"),
        }
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.summary, "late"),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_id_is_unknown_everywhere() {
        let mgr = AgentManager::new("sess");
        let unknown = AgentId("sess-99".to_string());
        assert_eq!(
            mgr.wait(&unknown, Duration::from_millis(10)).await,
            WaitOutcome::Unknown
        );
        assert_eq!(mgr.status(&unknown), None);
        assert_eq!(mgr.cancel(&unknown), None);
        assert!(mgr.drain_notices().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_fires_child_token_and_yields_cancelled() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert_eq!(mgr.cancel(&id), Some(AgentState::Running));
        // `cancel` never yields, so on a single-threaded runtime the
        // wrapper cannot have reaped the entry yet: the fired token is
        // observably the child's own.
        let fired = mgr
            .lock()
            .running
            .get(&id)
            .map(|child| child.token.is_cancelled());
        assert_eq!(fired, Some(true));
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.active_count(), 0);
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, AgentState::Cancelled);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn at_capacity_rejects_with_running_list_then_frees() {
        let mgr = AgentManager::new("sess");
        let mut ids = Vec::new();
        for n in 0..MAX_CHILDREN {
            ids.push(
                mgr.spawn(
                    &test_def(&format!("agent-{n}")),
                    test_seed(),
                    SpawnMeta::fresh(),
                    token_body,
                )
                .unwrap(),
            );
        }
        match mgr.spawn(
            &test_def("one-too-many"),
            test_seed(),
            SpawnMeta::fresh(),
            token_body,
        ) {
            Err(SpawnError::AtCapacity { limit, running }) => {
                assert_eq!(limit, MAX_CHILDREN);
                assert_eq!(running.len(), MAX_CHILDREN);
                let listed: Vec<AgentId> = running.into_iter().map(|(id, _)| id).collect();
                for id in &ids {
                    assert!(listed.contains(id), "missing {id} in rejection");
                }
            }
            other => panic!("expected AtCapacity, got {other:?}"),
        }
        assert!(mgr
            .spawn(&test_def("x"), test_seed(), SpawnMeta::fresh(), token_body)
            .unwrap_err()
            .to_string()
            .contains(&ids[0].to_string()));
        // Freeing one slot unblocks spawn.
        mgr.cancel(&ids[0]);
        match mgr.wait(&ids[0], Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Cancelled),
            other => panic!("expected Finished, got {other:?}"),
        }
        mgr.spawn(
            &test_def("fits-now"),
            test_seed(),
            SpawnMeta::fresh(),
            token_body,
        )
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notices_bound_and_overflow_folds() {
        let mgr = AgentManager::new("sess");
        for _ in 0..(MAX_NOTICES + 3) {
            let id = mgr
                .spawn(
                    &test_def("explorer"),
                    test_seed(),
                    SpawnMeta::fresh(),
                    |_, _, _| async { completed("done") },
                )
                .unwrap();
            assert!(matches!(
                mgr.wait(&id, Duration::from_secs(5)).await,
                WaitOutcome::Finished(_)
            ));
        }
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), MAX_NOTICES);
        // FIFO: the retained notices are the first completions.
        assert_eq!(notices[0].agent_id.to_string(), "sess-0");
        assert_eq!(mgr.take_overflow(), 3);
        assert_eq!(mgr.take_overflow(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn results_survive_notice_drain() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("durable") },
            )
            .unwrap();
        assert!(matches!(
            mgr.wait(&id, Duration::from_secs(5)).await,
            WaitOutcome::Finished(_)
        ));
        assert_eq!(mgr.drain_notices().len(), 1);
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.summary, "durable"),
            other => panic!("expected retained Finished, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_and_joins_children() {
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        let second = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        mgr.shutdown().await;
        assert_eq!(mgr.active_count(), 0);
        for id in [&first, &second] {
            match mgr.wait(id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(result) => {
                    assert_eq!(result.status, AgentState::Cancelled)
                }
                other => panic!("expected Finished, got {other:?}"),
            }
        }
        assert_eq!(mgr.drain_notices().len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_completions_stay_consistent() {
        let mgr = AgentManager::new("sess");
        let gate = std::sync::Arc::new(Barrier::new(MAX_CHILDREN));
        let mut ids = Vec::new();
        for n in 0..MAX_CHILDREN {
            let gate = gate.clone();
            ids.push(
                mgr.spawn(
                    &test_def(&format!("agent-{n}")),
                    test_seed(),
                    SpawnMeta::fresh(),
                    |_, _, _| async move {
                        gate.wait().await;
                        completed("through the gate")
                    },
                )
                .unwrap(),
            );
        }
        for id in &ids {
            match mgr.wait(id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(result) => {
                    assert_eq!(result.status, AgentState::Completed)
                }
                other => panic!("expected Finished, got {other:?}"),
            }
        }
        assert_eq!(mgr.active_count(), 0);
        assert_eq!(mgr.drain_notices().len(), MAX_CHILDREN);
        assert_eq!(mgr.take_overflow(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panicking_body_fails_without_orphaning_the_entry() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { panic!("body exploded") },
            )
            .unwrap();
        // The wrapper catches the panic and funnels a synthesized `Failed`
        // through `finish` — no entry left Running, no slot leaked (§14).
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.error.as_deref(), Some("child panicked"));
                assert_eq!(result.summary, "");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.status(&id), Some(AgentState::Failed));
        assert_eq!(mgr.active_count(), 0);
        assert_eq!(mgr.drain_notices().len(), 1);
        // Cleanup ran: a fresh spawn still works.
        mgr.spawn(
            &test_def("explorer"),
            test_seed(),
            SpawnMeta::fresh(),
            token_body,
        )
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn child_timeout_ends_timed_out() {
        let mgr = AgentManager::new("sess");
        let mut def = test_def("explorer");
        def.timeout = Duration::from_millis(50);
        let id = mgr
            .spawn(&def, test_seed(), SpawnMeta::fresh(), |_, _, _| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                completed("never")
            })
            .unwrap();
        // The wrapper enforces the definition's timeout (§14): the hung body
        // is dropped and the run ends TimedOut with a synthesized error.
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::TimedOut);
                assert!(result.error.unwrap().contains("timed out"));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.active_count(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_after_shutdown_rejects_closed() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        mgr.shutdown().await;
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        // A stale clone (an in-flight tool call holds one) cannot respawn a
        // child nobody will ever join.
        assert!(matches!(
            mgr.spawn(&test_def("x"), test_seed(), SpawnMeta::fresh(), token_body),
            Err(SpawnError::Closed)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_reports_current_tool_and_clears() {
        let mgr = AgentManager::new("sess");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                move |_, progress, _| {
                    async move {
                        progress.set("bash");
                        let _ = rx.await; // hold the "tool call" until observed
                        progress.clear();
                        completed("done")
                    }
                },
            )
            .unwrap();
        // Current-thread runtime: the wrapper has not run yet.
        assert_eq!(mgr.progress(&id), None);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(mgr.progress(&id), Some("bash".to_string()));
        let _ = tx.send(());
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.summary, "done"),
            other => panic!("expected Finished, got {other:?}"),
        }
        // Finished children carry no live progress.
        assert_eq!(mgr.progress(&id), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_child_failing_does_not_disturb_a_sibling() {
        // §22-H: results map to their own ids and a sibling's run is
        // independent of a failure — no shared-state corruption, no
        // cascade.
        let mgr = AgentManager::new("sess");
        let doomed = mgr
            .spawn(
                &test_def("doomed"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { failed("boom") },
            )
            .unwrap();
        let sibling = mgr
            .spawn(
                &test_def("sibling"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    completed("fine")
                },
            )
            .unwrap();
        match mgr.wait(&doomed, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.error.as_deref(), Some("boom"));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        match mgr.wait(&sibling, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert_eq!(result.summary, "fine");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.active_count(), 0);
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].status, AgentState::Failed);
        assert_eq!(notices[1].status, AgentState::Completed);
    }

    #[test]
    fn notice_text_carries_usage_and_hides_unpriced_cost() {
        // §18: the lifecycle line carries the child's own spend, deduped
        // by seq on replay like every other lifecycle line.
        let notice = AgentNotice {
            agent_id: AgentId("sess-3".to_string()),
            name: "explorer".to_string(),
            status: AgentState::Completed,
            usage: Some(AgentUsage {
                prompt_tokens: 1_200,
                output_tokens: 300,
                cost_usd: 0.0312,
            }),
            resumable: false,
        };
        assert_eq!(
            notice.text(),
            "[agent explorer:sess-3] finished completed · 1.5k tok · $0.0312"
        );
        // Unpriced model: tokens yes, no `$0.0000` noise.
        let unpriced = AgentNotice {
            agent_id: AgentId("sess-3".to_string()),
            name: "explorer".to_string(),
            status: AgentState::Completed,
            usage: Some(AgentUsage {
                prompt_tokens: 1,
                output_tokens: 2,
                cost_usd: 0.0,
            }),
            resumable: false,
        };
        assert_eq!(
            unpriced.text(),
            "[agent explorer:sess-3] finished completed · 3 tok"
        );
        // Wrapper-synthesized endings carry no usage row at all.
        let bare = AgentNotice {
            agent_id: AgentId("sess-3".to_string()),
            name: "explorer".to_string(),
            status: AgentState::Completed,
            usage: None,
            resumable: false,
        };
        assert_eq!(bare.text(), "[agent explorer:sess-3] finished completed");
        // A resumable result advertises the re-entry — same prefix the TUI
        // matches on, suffix only.
        let resumable = AgentNotice {
            agent_id: AgentId("sess-3".to_string()),
            name: "explorer".to_string(),
            status: AgentState::TimedOut,
            usage: None,
            resumable: true,
        };
        assert_eq!(
            resumable.text(),
            "[agent explorer:sess-3] finished timed out · resumable with delegate(resume_from = \"sess-3\")"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transient_wrapper_death_advertises_a_resume_handle() {
        // §24.1: a panicking body is `Transient`; with the transcript on
        // disk holding progress, `finish` attaches the handle through the
        // single `on_exit` point — the panic arm itself stays dumb.
        let dir = PathBuf::from("/tmp/dex-supervision-resume");
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        // The first spawn in a fresh `sess` manager allocates `sess-0`.
        std::fs::write(
            agents.join("sess-0-explorer.jsonl"),
            "{\"entry_type\":\"session\"}\n{\"entry_type\":\"turn_start\"}\n",
        )
        .unwrap();
        let mgr = AgentManager::new("sess");
        // The first spawn allocates `sess-0`, matching the fixture above.
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta {
                    generation: 0,
                    parent_session: Some(dir.join("sess.jsonl")),
                },
                |_, _, _| async { panic!("boom") },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.reason, ExitReason::Transient);
                let handle = result.resume.expect("transient + progress advertises");
                assert_eq!(handle.agent_id, id);
                assert_eq!(handle.generation, 0);
                assert!(handle.note.contains("interrupted"), "{}", handle.note);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let notices = mgr.drain_notices();
        assert!(notices.iter().any(|notice| notice.resumable));
        assert!(notices[0].text().contains("resume_from"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn normal_completion_advertises_no_handle() {
        // `Normal` always drops through `on_exit`, even with progress.
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async {
                    AgentResult {
                        status: AgentState::Completed,
                        summary: "done".to_string(),
                        error: None,
                        usage: None,
                        reason: ExitReason::Normal,
                        tool_calls: 7,
                        resume: None,
                    }
                },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert!(result.resume.is_none()),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn snapshot_lists_live_and_retained() {
        // `delegate_list`'s source: one live child, one retained result.
        let mgr = AgentManager::new("sess");
        let live = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |token, _, _| async move {
                    token.cancelled().await;
                    AgentResult {
                        status: AgentState::Cancelled,
                        summary: String::new(),
                        error: None,
                        usage: None,
                        reason: ExitReason::ShutDown,
                        tool_calls: 0,
                        resume: None,
                    }
                },
            )
            .unwrap();
        let done = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("ok") },
            )
            .unwrap();
        match mgr.wait(&done, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        let snapshot = mgr.snapshot();
        assert_eq!(snapshot.len(), 2);
        let live_row = snapshot.iter().find(|row| row.agent_id == live).unwrap();
        assert_eq!(live_row.state, AgentState::Running);
        assert!(!live_row.resumable);
        let done_row = snapshot.iter().find(|row| row.agent_id == done).unwrap();
        assert_eq!(done_row.state, AgentState::Completed);
        assert_eq!(done_row.name, "tester");
        assert!(!done_row.resumable);
        mgr.shutdown().await;
    }
}
