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
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;

use crate::core::console::CancellationToken;
use crate::core::unwind::CatchUnwind;
use crate::session::Session;

use super::context::ContextSeed;
use super::definition::AgentDefinition;
use super::exit::{
    advertised_remaining, decide, exit_reason_word, recover_mode_word, resume_note,
    transcript_holds_progress, Action, ExhaustKind, ExitReason, Intensity, RecoverMode,
    ResumeHandle, ResumeRequest, SupervisionSpec,
};
use super::instance::{AgentId, AgentInstance, AgentState};
use super::result::{AgentResult, AgentUsage};

/// Max live children per session (plan §7). The next concurrent spawn
/// past this queues (§24.5) instead of rejecting; only queue overflow
/// rejects, with the running list.
pub(crate) const MAX_CHILDREN: usize = 4;
/// Max queued spawn requests per session (§24.5). Over-cap spawns queue
/// FIFO and start when a sibling goes terminal (or the breaker clears);
/// overflow beyond this bound rejects with the queue-full error.
pub(crate) const MAX_PENDING: usize = 8;
/// Completion notices retained per session; once full, further completions
/// fold into the [`AgentManager::take_overflow`] counter instead of growing
/// without bound.
pub(crate) const MAX_NOTICES: usize = 32;

/// Recovery-ledger memory bound (§24.5): entries are never pruned by
/// whoever finished last (each definition counts its own window at
/// decision time), so the deque is only capped. Recoveries are
/// window-rate-limited; the cap exists for pathological catalog-scale
/// churn, not for correctness.
pub(crate) const MAX_RECOVERY_LEDGER: usize = 4096;
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
    /// Recovery lineage for escalation notices (§24.4): oldest-first
    /// mode words ("resumed after timed out"), oldest first — the
    /// escalation record. Empty for first attempts — no suffix then.
    pub(crate) history: Vec<String>,
    /// True when this notice was queued for the parent (§24.5): the
    /// idle wake's fire condition keys on it, so a silent recovery's
    /// `Completed` hook does not spawn a wake task that would find an
    /// empty notice queue. Not serialized — journaling renders the same
    /// `text()` either way.
    pub(crate) queued: bool,
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
        if !self.history.is_empty() {
            text.push_str(&format!(
                " · after {} recoveries: {}",
                self.history.len(),
                self.history.join("; ")
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

/// Bounded-wait outcome for [`AgentManager::wait`]. The result is boxed:
/// it is by far the largest variant and would trip
/// `clippy::large_enum_variant` against the byte-sized `Running`.
#[derive(Debug, PartialEq)]
pub(crate) enum WaitOutcome {
    /// Terminal result; also retained for later fetch after notice drain.
    Finished(Box<AgentResult>),
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
    /// The spawn QUEUE is full (`MAX_PENDING`), not the child cap: the
    /// message names what is actually full, so the model can act (wait
    /// for or cancel a queued child) instead of guessing.
    QueueFull {
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
            Self::QueueFull { limit, running } => {
                let list = running
                    .iter()
                    .map(|(id, name)| format!("{id} ({name})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "spawn queue full ({limit} queued, {} running): {list}; \
                     wait for or cancel a queued child before spawning",
                    running.len()
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
    /// Also bumps the registry's spend meter: the body's own sink counter
    /// dies with the body, so a wrapper-synthesized ending (panic, timeout)
    /// reads the meter back for its resume budget (§24.1).
    pub(crate) fn set(&self, tool: &str) {
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
/// so no lifecycle transition can bypass either encoding. `Clone` for the
/// queued-drain path: settle hooks fire before a recovery relaunch, and
/// both stages reference the same event list.
#[derive(Clone)]
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
    /// Phase 12 automatic recovery (§24.4): the supervisor re-entered a
    /// recoverable child as `agent_id` (attempt number, 1-based). Fires
    /// beside the superseded generation's `Completed` and the new one's
    /// `Spawned` — never instead of them.
    Recovered {
        agent_id: AgentId,
        name: String,
        attempt: u32,
        mode: RecoverMode,
        reason: ExitReason,
    },
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
    /// Recovery-action timestamps for the intensity ledger (§24.5):
    /// supervisor-scoped (all children share it), pruned by each
    /// definition's window at decision time.
    recoveries: VecDeque<Instant>,
    /// Phase 13 spawn queue (§24.5): FIFO, drained when a sibling goes
    /// terminal or the breaker clears. Ids are pre-allocated at enqueue,
    /// so a queued child is addressable (`delegate_output` → "queued",
    /// `delegate_stop` → synthesized `Cancelled`).
    pending: VecDeque<PendingChild>,
    /// Circuit breaker (§24.5): set when an intensity-exhausted recovery
    /// escalates — spawns queue until it expires (the escalating
    /// definition's window). `None` = clear; drain-on-expiry.
    open_until: Option<Instant>,
    /// One drain timer per manager (the breaker's time-based half).
    drain_scheduled: bool,
}

/// A queued spawn request (§24.5): everything [`AgentManager::launch`]
/// needs, held until a slot frees.
struct PendingChild {
    id: AgentId,
    def: AgentDefinition,
    seed: ContextSeed,
    meta: SpawnMeta,
    run: BoxRun,
}

/// Everything a recovery needs, cloned out of the registry entry before
/// it is removed: definition, seed, spawn coordinates, lineage, factory.
struct ReEntry {
    def: AgentDefinition,
    seed: ContextSeed,
    meta: SpawnMeta,
    factory: ChildFactory,
    resume: Option<ResumeRequest>,
    name: String,
    attempt: u32,
    mode: RecoverMode,
    reason: ExitReason,
    /// The superseded generation's silent notice, fired only once the
    /// relaunch succeeds (a cap-race failure escalates with its own
    /// notice instead — one completion per id).
    superseded_notice: AgentNotice,
    fallback: Fallback,
}

/// Recomputed escalation inputs, for when the relaunch loses a cap race
/// after the entry is gone.
struct Fallback {
    transcript: Option<PathBuf>,
    generation: u32,
    allowance: Option<usize>,
    lineage: Vec<String>,
    reason: ExitReason,
    tool_calls: u32,
}

/// Named snapshot of the registry entry at `finish` time. `None` only
/// for an id that was never registered (defensive — wrappers always
/// register first); then the result settles handle-free.
struct Record {
    generation: u32,
    transcript: Option<PathBuf>,
    spec: SupervisionSpec,
    lineage: Vec<String>,
    parent_session: Option<PathBuf>,
    retry: Option<ChildFactory>,
    seed: ContextSeed,
    def: AgentDefinition,
    /// Registry-side spend meter (§24.1), surviving body death.
    calls: u32,
    /// The tool-round budget the child was launched under (§24.5):
    /// the resume's remaining budget, else the definition's cap;
    /// `None` = uncapped. Recoveries subtract the reconciled spend
    /// so the meter carries across the lineage.
    allowance: Option<usize>,
}

/// The escalation handle for a recorded child, transcript-gated (§24.1):
/// only a child whose file is known can advertise a re-entry. Shared by
/// the settle escalation arm and the cap-race fallback.
fn escalate_handle(
    record: &Record,
    id: &AgentId,
    reason: ExitReason,
    tool_calls: u32,
) -> Option<ResumeHandle> {
    let transcript = record.transcript.clone()?;
    let remaining = advertised_remaining(record.allowance, tool_calls as usize);
    Some(ResumeHandle {
        agent_id: id.clone(),
        transcript,
        generation: record.generation,
        remaining_budget: remaining,
        note: resume_note(reason, tool_calls),
        history: record.lineage.clone(),
    })
}

impl Inner {
    /// Queue a spawn request. Capacity is checked here so the spawn and
    /// recovery-fallback paths share the same overflow semantics.
    fn enqueue(&mut self, pending: PendingChild) -> Result<(), SpawnError> {
        if self.pending.len() >= MAX_PENDING {
            return Err(self.queue_full());
        }
        self.pending.push_back(pending);
        Ok(())
    }

    /// The queue-overflow error: names the queue, not the child cap.
    fn queue_full(&self) -> SpawnError {
        let running = self
            .running
            .iter()
            .map(|(id, child)| (id.clone(), child.instance.definition.name.clone()))
            .collect();
        SpawnError::QueueFull {
            limit: MAX_PENDING,
            running,
        }
    }
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
    /// The parent session file, to derive the next generation's
    /// transcript without the original spawn context (§24.3).
    parent_session: Option<PathBuf>,
    /// Phase 12 re-entry factory. `None` (tests, manual-only callers)
    /// escalates immediately — today's behavior.
    retry: Option<ChildFactory>,
    /// Automatic-recovery lineage, oldest first (§24.4 escalation
    /// record). Extended per recovery, read at escalation.
    lineage: Vec<String>,
    /// Registry-side spend meter (§24.1): bumped by `ProgressReporter::set`
    /// per tool call, so a wrapper-synthesized ending (panic, timeout)
    /// still reports the spend its body made. Reconciled against the
    /// body's own count at `finish` (`max` — both count the same events,
    /// the registry meter just survives the body's death).
    calls: u32,
    /// The tool-round budget this child was launched under (§24.5 spend
    /// meter): the resume's remaining budget, else the definition's cap;
    /// `None` = uncapped. Recoveries subtract the dying generation's
    /// spend so the meter carries across the lineage.
    allowance: Option<usize>,
}

/// Builds one attempt's body (§24.3: re-entry is spawn). The factory —
/// not the manager — owns parent-turn context; the manager owns when.
/// `resume` is `None` for fresh attempts (generation 0 and `Fresh`
/// recoveries).
pub(crate) type ChildFactory = Arc<
    dyn Fn(
            CancellationToken,
            ProgressReporter,
            AgentId,
            Option<ResumeRequest>,
        ) -> Pin<Box<dyn Future<Output = AgentResult> + Send>>
        + Send
        + Sync,
>;

/// One attempt's built future.
type BodyFuture = Pin<Box<dyn Future<Output = AgentResult> + Send>>;

/// Attempt-0 bodies (FnOnce) and recovery bodies (factory-built) funnel
/// through the shared launch path in this shape.
type BoxRun = Box<dyn FnOnce(CancellationToken, ProgressReporter, AgentId) -> BodyFuture + Send>;

/// What `spawn` needs beyond definition + seed (§24.3 + §24.5): which
/// generation this child is, where its transcript derives from, who it
/// was resumed from, how it may re-enter, and the lineage so far.
/// Fresh spawns use `SpawnMeta::fresh()`. `Clone`-only: the retry
/// factory is a `dyn Fn` and deliberately opaque to `Debug`.
#[derive(Clone)]
pub(crate) struct SpawnMeta {
    pub(crate) generation: u32,
    pub(crate) parent_session: Option<PathBuf>,
    /// The original child id for resume generations (§24.3 lineage).
    /// `None` for fresh spawns.
    pub(crate) parent_id: Option<AgentId>,
    /// Phase 12 re-entry factory. `None` escalates immediately.
    pub(crate) retry: Option<ChildFactory>,
    /// Recovery lineage inherited by the next generation (§24.4).
    /// Empty for fresh spawns.
    pub(crate) lineage: Vec<String>,
    /// The tool-round budget the body runs under: the resume's remaining
    /// budget, else the definition's cap (§24.5). `None` for uncapped
    /// definitions. Mirrored into `RunningChild::allowance` so
    /// recoveries can carry spend across generations.
    pub(crate) remaining_budget: Option<usize>,
}

impl SpawnMeta {
    pub(crate) fn fresh() -> Self {
        Self {
            generation: 0,
            parent_session: None,
            parent_id: None,
            retry: None,
            lineage: Vec::new(),
            remaining_budget: None,
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
                recoveries: VecDeque::new(),
                pending: VecDeque::new(),
                open_until: None,
                drain_scheduled: false,
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
    /// checks. Over-cap spawns and spawns under an open breaker queue as
    /// `Pending` (§24.5) — an observable state (`delegate` reports
    /// "queued", `delegate_stop` cancels the queue entry) — and launch
    /// when a slot frees.
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
        let run: BoxRun = Box::new(move |token, progress, id| {
            let future: BodyFuture = Box::pin(run(token, progress, id));
            future
        });
        self.launch(def, seed, meta, run)
    }

    /// Shared launch path (§24.3: re-entry is spawn): cap check, id
    /// allocation, transcript derivation, registry insert, the
    /// cancel/timeout wrapper, handle recording, and the `Spawned` hook.
    /// Attempt 0 arrives via `run`, recoveries via the retry factory —
    /// both funnel here, so there is exactly one spawn implementation.
    fn launch(
        &self,
        def: &AgentDefinition,
        seed: ContextSeed,
        meta: SpawnMeta,
        run: BoxRun,
    ) -> Result<AgentId, SpawnError> {
        let id = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SpawnError::Closed);
            }
            // §24.5: over-cap spawns and spawns under an open breaker
            // queue FIFO instead of rejecting; only queue overflow still
            // rejects (with the running list, as the cap used to).
            if inner.running.len() >= MAX_CHILDREN || inner.open_until.is_some() {
                // Capacity is checked BEFORE the id is allocated (§24.5):
                // an overflowed queue must not burn a counter slot.
                if inner.pending.len() >= MAX_PENDING {
                    return Err(inner.queue_full());
                }
                let id = Self::next_id(&mut inner);
                inner.pending.push_back(PendingChild {
                    id: id.clone(),
                    def: def.clone(),
                    seed,
                    meta,
                    run,
                });
                drop(inner);
                self.schedule_drain();
                return Ok(id);
            }
            Self::next_id(&mut inner)
        };
        self.launch_with_id(def, seed, meta, run, id)
    }

    /// Register + wrapper + `Spawned` hook for an already-allocated id.
    /// `launch` and the queue drain both land here (via [`Self::register`]
    /// + [`Self::spawn_wrapper`]).
    fn launch_with_id(
        &self,
        def: &AgentDefinition,
        seed: ContextSeed,
        meta: SpawnMeta,
        run: BoxRun,
        id: AgentId,
    ) -> Result<AgentId, SpawnError> {
        let (token, timeout) = {
            let mut inner = self.lock();
            if inner.closed {
                return Err(SpawnError::Closed);
            }
            if inner.running.len() >= MAX_CHILDREN {
                // A racing sibling (or the drain) filled the last slot
                // between `launch`'s check and this one. The queue exists
                // to absorb exactly this, and the id is already
                // allocated — requeue instead of reporting a spurious
                // "at child capacity" to the model. A full queue (or an
                // open breaker) still rejects; the burn of one counter
                // slot is the cost of the rare race.
                let pending = PendingChild {
                    id: id.clone(),
                    def: def.clone(),
                    seed,
                    meta,
                    run,
                };
                inner.enqueue(pending)?;
                drop(inner);
                self.schedule_drain();
                return Ok(id);
            }
            Self::register(&mut inner, def, seed, &meta, &id)
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

    /// Lock-scope registration shared by `launch_with_id` and the queue
    /// drain: derive the transcript path (§24.3), insert the
    /// `RunningChild`, return the fresh token + timeout. Capacity is the
    /// caller's check — both check it under the same lock they register
    /// in, so a drain can never lose its slot to a racing spawn.
    fn register(
        inner: &mut Inner,
        def: &AgentDefinition,
        seed: ContextSeed,
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
                    id: id.clone(),
                    definition: def.clone(),
                    parent_id: meta.parent_id.clone(),
                    context: seed,
                    state: AgentState::Running,
                    progress: None,
                },
                token: token.clone(),
                handle: None,
                transcript,
                generation: meta.generation,
                parent_session: meta.parent_session.clone(),
                retry: meta.retry.clone(),
                lineage: meta.lineage.clone(),
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
    }

    /// Session-scoped monotonic id allocation (shared by the launch and
    /// queue paths so ids stay ordered by request time).
    fn next_id(inner: &mut Inner) -> AgentId {
        let id = AgentId(format!("{}-{}", inner.session, inner.next_counter));
        inner.next_counter += 1;
        id
    }

    /// Launch queued children while a slot is free and the breaker is
    /// clear (§24.5). Called from `finish` (a terminal freed a slot) and
    /// from the drain timer (the breaker expired). Pop *and* register in
    /// one lock scope — a racing spawn can never steal the slot between
    /// the check and the insert.
    fn drain_queued(&self) {
        loop {
            let registered = {
                let mut inner = self.lock();
                if inner.closed {
                    inner.pending.clear();
                    inner.open_until = None;
                    return;
                }
                // Window expiry clears the breaker (the drain's time half).
                if let Some(until) = inner.open_until {
                    if Instant::now() >= until {
                        inner.open_until = None;
                    }
                }
                if inner.running.len() >= MAX_CHILDREN || inner.open_until.is_some() {
                    return;
                }
                let Some(pending) = inner.pending.pop_front() else {
                    return;
                };
                let PendingChild {
                    id,
                    def,
                    seed,
                    meta,
                    run,
                } = pending;
                let (token, timeout) = Self::register(&mut inner, &def, seed, &meta, &id);
                (id, def, timeout, token, run)
            };
            let (id, def, timeout, token, run) = registered;
            let name = def.name.clone();
            self.spawn_wrapper(id.clone(), name.clone(), timeout, token, run);
            // §15 V1b: the typed spawn event fires after registration.
            let hook = self.lock().events.clone();
            if let Some(hook) = hook {
                hook(AgentEvent::Spawned { agent_id: id, name });
            }
        }
    }

    /// The breaker's time-based half: one timer per manager, woken at
    /// `open_until` (or immediately for a cap-only queue) to drain.
    /// Re-arms itself while the breaker stays open; a check-and-clear
    /// under the lock keeps the flag and the queue in sync, so no
    /// enqueue is ever left without a timer or a terminal to drain it.
    fn schedule_drain(&self) {
        let already = {
            let mut inner = self.lock();
            if inner.drain_scheduled {
                true
            } else {
                inner.drain_scheduled = true;
                false
            }
        };
        if already {
            return;
        }
        let manager = self.clone();
        tokio::spawn(async move {
            loop {
                let wait = manager
                    .lock()
                    .open_until
                    .map(|until| until.saturating_duration_since(Instant::now()))
                    .unwrap_or(Duration::ZERO);
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                manager.drain_queued();
                let stop = {
                    let mut inner = manager.lock();
                    let stop = inner.pending.is_empty() || inner.open_until.is_none();
                    if stop {
                        inner.drain_scheduled = false;
                    }
                    stop
                };
                if stop {
                    break;
                }
            }
        });
    }

    /// Stamp one terminal result through retention, notices, the journal
    /// hook, and registry removal. Single choke point: completion, failure,
    /// cancel, timeout, and panic all land here, so no terminal path can
    /// orphan an entry or skip its lifecycle line.
    ///
    /// The outcome comes from [`decide`]: `Settle` retains + notifies as
    /// today; `Escalate` additionally advertises the resume handle with
    /// the recovery history; `Recover` retains silently — no parent
    /// notice; the terminal notice fires only on policy exhaustion
    /// (§24.4) — and re-enters the child as generation + 1 after this
    /// returns. Recovery launches outside the lock (`launch` locks
    /// internally); a cap race there falls back to escalation so the
    /// child is never lost silently.
    fn finish(&self, id: &AgentId, name: &str, mut result: AgentResult) {
        let mut reenter: Option<ReEntry> = None;
        let mut hooks: Vec<AgentEvent> = Vec::new();
        // Copy out before `result` moves into retention below.
        let reason = result.reason;
        {
            let mut inner = self.lock();
            if inner.results.len() >= MAX_RESULTS {
                if let Some(oldest) = inner.result_order.pop_front() {
                    inner.results.remove(&oldest);
                    inner.names.remove(&oldest);
                }
            }
            // §24.1: bodies arrive pre-classified (`result.reason`);
            // wrapper-synthesized endings were classified at their arm.
            // Nothing here reads prose. `remaining_budget` needs the
            // definition's cap, which rides in the registry entry: a
            // timeout keeps the full meter (spend unknown), a budget
            // exhaustion keeps the honest remainder.
            // Named snapshot of the registry entry. `None` only for an
            // id that was never registered (defensive — wrappers always
            // register first); then the result settles handle-free.
            let record: Option<Record> = inner.running.get(id).map(|child| Record {
                generation: child.generation,
                transcript: child.transcript.clone(),
                allowance: child.allowance,
                spec: child.instance.definition.supervision,
                lineage: child.lineage.clone(),
                parent_session: child.parent_session.clone(),
                retry: child.retry.clone(),
                seed: child.instance.context.clone(),
                def: child.instance.definition.clone(),
                calls: child.calls,
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
            let spec = record
                .as_ref()
                .map(|record| record.spec)
                .unwrap_or_default();
            // Intensity ledger (§24.5): supervisor-scoped. Each
            // definition counts its own window at decision time —
            // entries are never pruned by whoever finished last, so a
            // long-window definition's history survives its
            // short-window siblings (the deque cap is only a memory
            // bound).
            let now = Instant::now();
            let in_window = inner
                .recoveries
                .iter()
                .filter(|at| now.duration_since(**at) <= spec.window)
                .count();
            // Without a factory there is nothing to re-enter with:
            // escalate exactly as the manual-only path would.
            let retry = record.as_ref().and_then(|record| record.retry.clone());
            // The lineage's leftover tool rounds: what this generation
            // was launched with minus what it spent — spend carries
            // across generations, so a budget-exhausted lineage cannot
            // reset its meter by dying and re-entering. `None` =
            // uncapped.
            let next_allowance = record
                .as_ref()
                .and_then(|record| record.allowance)
                .map(|allowance| allowance.saturating_sub(tool_calls as usize));
            let mut action = decide(
                &spec,
                result.reason,
                Intensity {
                    in_window,
                    lineage_recoveries: record
                        .as_ref()
                        .map(|record| record.lineage.len())
                        .unwrap_or(0),
                    lineage_budget: next_allowance,
                },
                progress_made,
            );
            if matches!(action, Action::Recover { .. }) && retry.is_none() {
                action = Action::Escalate;
            }
            // Retain + queue + hook inputs, shared by the settle paths.
            // Recovery retains silently and queues nothing.
            let retain = |inner: &mut Inner,
                          id: &AgentId,
                          name: &str,
                          result: AgentResult,
                          history: Vec<String>|
             -> AgentNotice {
                let status = result.status;
                let resumable = result.resume.is_some();
                inner.result_order.push_back(id.clone());
                inner.names.insert(id.clone(), name.to_string());
                inner.results.insert(id.clone(), result);
                AgentNotice {
                    agent_id: id.clone(),
                    name: name.to_string(),
                    status,
                    // Copied from the retained result, so the notice can
                    // never disagree with the record the parent later
                    // fetches.
                    usage: inner.results.get(id).and_then(|result| result.usage),
                    resumable,
                    history,
                    // Set by the queue step below: only a queued notice
                    // may wake the session.
                    queued: false,
                }
            };
            let queue = |inner: &mut Inner, mut notice: AgentNotice| {
                notice.queued = true;
                if inner.notices.len() >= MAX_NOTICES {
                    inner.overflowed += 1;
                } else {
                    inner.notices.push_back(notice);
                }
            };
            match action {
                Action::Settle | Action::Escalate => {
                    let lineage = record
                        .as_ref()
                        .map(|record| record.lineage.clone())
                        .unwrap_or_default();
                    if action == Action::Escalate {
                        let handle = record
                            .as_ref()
                            .and_then(|record| escalate_handle(record, id, reason, tool_calls));
                        if let Some(handle) = handle {
                            result.resume = Some(handle);
                        }
                        // §24.5 breaker: an intensity-exhausted *recovery*
                        // escalation means the environment is failing
                        // faster than policy allows re-entry — further
                        // spawns queue until this window expires
                        // (`launch`), and a recovery that finds no room
                        // escalates. `never`-mode escalations are
                        // ordinary endings, not breaker material. The
                        // trigger counts the same window `decide` just
                        // measured — the ledger is never pruned by
                        // whoever finished last.
                        if spec.recover != RecoverMode::Never && in_window >= spec.max as usize {
                            let until = now + spec.window;
                            inner.open_until = Some(match inner.open_until {
                                Some(existing) if existing > until => existing,
                                _ => until,
                            });
                        }
                    }
                    let notice = retain(&mut inner, id, name, result, lineage);
                    queue(&mut inner, notice.clone());
                    inner.running.remove(id);
                    hooks.push(AgentEvent::Completed(notice));
                }
                Action::Recover { mode } => {
                    let record = record.expect("recoveries only fire for registered children");
                    let factory = retry.expect("recoveries only fire with a factory");
                    inner.recoveries.push_back(now);
                    while inner.recoveries.len() > MAX_RECOVERY_LEDGER {
                        inner.recoveries.pop_front();
                    }
                    let line = format!(
                        "{} after {}",
                        recover_mode_word(mode),
                        exit_reason_word(reason)
                    );
                    let mut next_lineage = record.lineage.clone();
                    next_lineage.push(line);
                    // Retain silently: no handle (the lineage continues in
                    // the next generation), no queue entry. The superseded
                    // generation's `Completed` fires only once the
                    // relaunch succeeds — on a cap-race failure the
                    // escalated notice is the generation's sole
                    // completion, not a duplicate.
                    let notice = retain(&mut inner, id, name, result, Vec::new());
                    inner.running.remove(id);
                    let next_generation = record.generation + 1;
                    // Both modes re-enter through the same resume
                    // plumbing; the mode tells the body whether to replay
                    // (`Resume`) or run the original seed with a full
                    // meter under the generation-suffixed file the
                    // registry already points at (`Fresh`).
                    let resume = Some(ResumeRequest {
                        mode,
                        handle: ResumeHandle {
                            agent_id: id.clone(),
                            transcript: record.transcript.clone().unwrap_or_default(),
                            generation: record.generation,
                            remaining_budget: if mode == RecoverMode::Resume {
                                next_allowance
                            } else {
                                // Fresh runs the definition's full meter.
                                None
                            },
                            note: if mode == RecoverMode::Resume {
                                resume_note(reason, tool_calls)
                            } else {
                                String::new()
                            },
                            history: next_lineage.clone(),
                        },
                        instruction: None,
                        // The seed task already carries the hints; the
                        // nudge would only duplicate them.
                        file_hints: Vec::new(),
                    });
                    reenter = Some(ReEntry {
                        meta: SpawnMeta {
                            generation: next_generation,
                            parent_session: record.parent_session.clone(),
                            parent_id: Some(id.clone()),
                            retry: Some(factory.clone()),
                            lineage: next_lineage.clone(),
                            remaining_budget: next_allowance,
                        },
                        factory,
                        resume,
                        def: record.def.clone(),
                        seed: record.seed.clone(),
                        name: name.to_string(),
                        attempt: next_generation + 1,
                        mode,
                        reason,
                        superseded_notice: notice,
                        fallback: Fallback {
                            transcript: record.transcript.clone(),
                            generation: record.generation,
                            allowance: record.allowance,
                            lineage: record.lineage.clone(),
                            reason,
                            tool_calls,
                        },
                    });
                }
            }
        }
        // Settle hooks fire BEFORE the relaunch, so the journal order is
        // old-`Completed` → `Recovered`/new-`Spawned` — the lifecycle
        // reads forward in time (§24.4). The superseded generation's
        // silent notice is not in here: it fires with the relaunch
        // outcome below, so a failed relaunch never double-completes.
        {
            let hook = self.lock().events.clone();
            if let Some(hook) = hook {
                for event in &hooks {
                    hook(event.clone());
                }
            }
        }
        let mut requeued = false;
        if let Some(reenter) = reenter {
            // The fallback paths still need the factory + resume after
            // `build` consumes its copies — clone up front.
            let factory = reenter.factory.clone();
            let resume = reenter.resume.clone();
            let build: BoxRun =
                Box::new(move |token, progress, new_id| (factory)(token, progress, new_id, resume));
            match self.launch(
                &reenter.def,
                reenter.seed.clone(),
                reenter.meta.clone(),
                build,
            ) {
                Ok(new_id) => {
                    // The superseded generation completes before the
                    // `Recovered` event so the journal reads forward in
                    // time; a failed relaunch escalates instead and
                    // never double-completes the id.
                    let hook = self.lock().events.clone();
                    if let Some(hook) = hook {
                        hook(AgentEvent::Completed(reenter.superseded_notice));
                        hook(AgentEvent::Recovered {
                            agent_id: new_id,
                            name: reenter.name,
                            attempt: reenter.attempt,
                            mode: reenter.mode,
                            reason: reenter.reason,
                        });
                    }
                }
                Err(SpawnError::AtCapacity { .. }) => {
                    // Cap race between removal and relaunch: queue the
                    // recovery like any spawn (§24.5) — it launches when a
                    // sibling frees its slot. Breaker open or queue full →
                    // escalate: recovery is exactly what the breaker gates,
                    // and an overflowed queue must not lose the child
                    // silently either. One lock scope does probe + allocate
                    // + enqueue, so no other actor can slip between the
                    // check and the push.
                    // (The race itself is not deterministically testable —
                    // the dying child frees its own slot first, so hitting
                    // AtCapacity here needs a racing spawn between the
                    // lock scopes.)
                    let outcome = {
                        let mut inner = self.lock();
                        if inner.open_until.is_some() || inner.pending.len() >= MAX_PENDING {
                            None
                        } else {
                            let new_id = Self::next_id(&mut inner);
                            let retry_factory = reenter.factory.clone();
                            let retry_resume = reenter.resume.clone();
                            inner.pending.push_back(PendingChild {
                                id: new_id.clone(),
                                def: reenter.def.clone(),
                                seed: reenter.seed.clone(),
                                meta: SpawnMeta {
                                    generation: reenter.meta.generation,
                                    parent_session: reenter.meta.parent_session.clone(),
                                    // The dying generation is the lineage parent.
                                    parent_id: Some(id.clone()),
                                    retry: reenter.meta.retry.clone(),
                                    lineage: reenter.meta.lineage.clone(),
                                    remaining_budget: reenter.meta.remaining_budget,
                                },
                                run: Box::new(move |token, progress, new_id| {
                                    (retry_factory)(token, progress, new_id, retry_resume)
                                }),
                            });
                            Some((
                                new_id.clone(),
                                AgentEvent::Recovered {
                                    agent_id: new_id,
                                    name: reenter.name.clone(),
                                    attempt: reenter.attempt,
                                    mode: reenter.mode,
                                    reason: reenter.reason,
                                },
                            ))
                        }
                    };
                    match outcome {
                        Some((_new_id, event)) => {
                            let hook = self.lock().events.clone();
                            if let Some(hook) = hook {
                                // The superseded generation's completion
                                // rides the requeue: its chip drops now,
                                // the queued recovery spawns later.
                                hook(AgentEvent::Completed(reenter.superseded_notice.clone()));
                                hook(event);
                            }
                            self.schedule_drain();
                            requeued = true;
                        }
                        None => self.escalate_fallback(id, &reenter),
                    }
                }
                Err(_) => {
                    // Closed registry: escalate the retained result —
                    // attach the handle so a later `delegate_list` (or a
                    // resumed session's scan) can still re-enter it.
                    self.escalate_fallback(id, &reenter);
                }
            }
        }
        // A terminal freed a slot: let the queue start its next child.
        self.drain_queued();
        _ = requeued;
    }

    /// Cap-race escalation (§24.4): attach the resume handle to the
    /// retained result and notify the parent — the recovery lost its
    /// race, so the model decides instead.
    fn escalate_fallback(&self, id: &AgentId, reenter: &ReEntry) {
        let fallback = &reenter.fallback;
        let notice = {
            let mut inner = self.lock();
            if let Some(result) = inner.results.get_mut(id) {
                if let Some(transcript) = fallback.transcript.clone() {
                    let remaining =
                        advertised_remaining(fallback.allowance, fallback.tool_calls as usize);
                    result.resume = Some(ResumeHandle {
                        agent_id: id.clone(),
                        transcript,
                        generation: fallback.generation,
                        remaining_budget: remaining,
                        note: resume_note(fallback.reason, fallback.tool_calls),
                        history: fallback.lineage.clone(),
                    });
                }
            }
            // Reconcile the meter here too: the fallback can fire for a
            // body that spent calls before its relaunch lost the race.
            AgentNotice {
                agent_id: id.clone(),
                name: reenter.name.clone(),
                status: inner
                    .results
                    .get(id)
                    .map(|result| result.status)
                    .unwrap_or(AgentState::Failed),
                usage: inner.results.get(id).and_then(|result| result.usage),
                resumable: inner
                    .results
                    .get(id)
                    .and_then(|result| result.resume.as_ref())
                    .is_some(),
                history: fallback.lineage.clone(),
                // The parent is being notified of a lost recovery — it may
                // act on it, so the wake is legitimate. The notice is
                // QUEUED, not journal-only: a `queued: true` notice that
                // never enters the queue schedules a wake that no-ops at
                // `has_notices()` and the escalation never reaches the
                // model's context.
                queued: true,
            }
        };
        {
            let mut inner = self.lock();
            if inner.notices.len() >= MAX_NOTICES {
                inner.overflowed += 1;
            } else {
                inner.notices.push_back(notice.clone());
            }
        }
        let hook = self.lock().events.clone();
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
                    return WaitOutcome::Finished(Box::new(result.clone()));
                }
                if let Some(child) = inner.running.get(id) {
                    if tokio::time::Instant::now() >= deadline {
                        return WaitOutcome::Running(child.instance.state);
                    }
                } else if inner.pending.iter().any(|pending| &pending.id == id) {
                    // §24.5: queued children report as Pending — the
                    // dormant state, observable at last.
                    if tokio::time::Instant::now() >= deadline {
                        return WaitOutcome::Running(AgentState::Pending);
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
        // Queued children (§24.5): visible so `delegate_list` explains
        // what is waiting and why. No transcript: the child has not run —
        // the file appears when the queue drains and it launches.
        out.extend(inner.pending.iter().map(|pending| ChildInfo {
            agent_id: pending.id.clone(),
            name: pending.def.name.clone(),
            state: AgentState::Pending,
            progress: None,
            resumable: false,
            transcript: None,
        }));
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

    /// The registry parent of a live child (§24.3 generations): the
    /// original id for resume generations, `None` for fresh children —
    /// and for unknown or finished ids, which carry no live lineage.
    pub(crate) fn parent_of(&self, id: &AgentId) -> Option<AgentId> {
        self.lock()
            .running
            .get(id)
            .and_then(|child| child.instance.parent_id.clone())
    }

    pub(crate) fn status(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            return Some(child.instance.state);
        }
        if inner.pending.iter().any(|pending| &pending.id == id) {
            return Some(AgentState::Pending);
        }
        inner.results.get(id).map(|result| result.status)
    }

    /// True while the id sits in the spawn queue (§24.5): `delegate` uses
    /// it right after a spawn to report "queued" instead of "started".
    pub(crate) fn is_queued(&self, id: &AgentId) -> bool {
        self.lock().pending.iter().any(|pending| &pending.id == id)
    }

    /// Signal the child's token. Returns the last-seen state (`None` for
    /// unknown ids); the `Cancelled` result itself lands via `finish` once
    /// the wrapper observes the token. Signalling a finished id is a
    /// harmless no-op lookup. A *queued* id never started: it is removed
    /// from the queue with a synthesized `Cancelled` result so
    /// `delegate_stop`'s contract holds verbatim.
    pub(crate) fn cancel(&self, id: &AgentId) -> Option<AgentState> {
        let (notice, event) = {
            let mut inner = self.lock();
            if let Some(child) = inner.running.get(id) {
                child.token.cancel();
                return Some(child.instance.state);
            }
            let queued_index = inner.pending.iter().position(|pending| &pending.id == id);
            let Some(index) = queued_index else {
                return inner.results.get(id).map(|result| result.status);
            };
            let pending = inner.pending.remove(index);
            let pending = pending.expect("position() matched an entry");
            let result = AgentResult {
                status: AgentState::Cancelled,
                summary: String::new(),
                error: Some("cancelled before start (was queued)".to_string()),
                usage: None,
                reason: ExitReason::ShutDown,
                tool_calls: 0,
                resume: None,
            };
            let status = result.status;
            // Retention bound, same discipline as `finish`: a queued-id
            // cancel files a real result and must not inflate the cache
            // past `MAX_RESULTS`.
            if inner.results.len() >= MAX_RESULTS {
                if let Some(oldest) = inner.result_order.pop_front() {
                    inner.results.remove(&oldest);
                    inner.names.remove(&oldest);
                }
            }
            inner.result_order.push_back(id.clone());
            inner.names.insert(id.clone(), pending.def.name.clone());
            inner.results.insert(id.clone(), result);
            let notice = AgentNotice {
                agent_id: id.clone(),
                name: pending.def.name.clone(),
                status,
                usage: None,
                resumable: false,
                history: Vec::new(),
                // A never-started child's cancel is a real parent-visible
                // event (delegate_stop or the reaper did it).
                queued: true,
            };
            if inner.notices.len() >= MAX_NOTICES {
                inner.overflowed += 1;
            } else {
                inner.notices.push_back(notice.clone());
            }
            let event = AgentEvent::Completed(notice.clone());
            (notice, event)
        };
        // Hoisted like every other hook site: firing the hook inside the
        // `if let` scrutinee would hold the registry guard through the
        // hook's journal append + SSE broadcast (edition 2021 scrutinee
        // temporaries outlive the block).
        let hook = self.lock().events.clone();
        if let Some(hook) = hook {
            hook(event);
        }
        Some(notice.status)
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
    /// reaches zero. Queued children (§24.5) never started — they don't
    /// count toward the cap.
    pub(crate) fn active_count(&self) -> usize {
        self.lock().running.len()
    }

    /// Ids the reaper (§24.5) may ShutDown: live children plus queued
    /// requests. Queued cancels are handled in `cancel` (synthesized
    /// result, no task to join).
    pub(crate) fn cancellable_ids(&self) -> Vec<AgentId> {
        let inner = self.lock();
        let mut ids: Vec<AgentId> = inner.running.keys().cloned().collect();
        ids.extend(inner.pending.iter().map(|pending| pending.id.clone()));
        ids
    }

    /// Cancel every live child, join their tasks, and close the manager.
    /// After this returns, every child has funneled through `finish`
    /// (results + notices retained), `active_count` is zero, and `spawn`
    /// rejects: a joined handle means its wrapper already stored and
    /// removed its entry, and a stale clone cannot respawn into this
    /// registry. Queued (never-started) children drop silently — no
    /// transcript, no task, no spend to report.
    pub(crate) async fn shutdown(&self) {
        let (handles, events) = {
            let mut inner = self.lock();
            inner.closed = true;
            // Queued children never started, but the parent holds their
            // ids: file a synthesized `Cancelled` result + notice per
            // entry, so the final notice drain explains them instead of
            // the ids reading `Unknown` forever (same shape as `cancel`
            // on a queued id).
            let mut events = Vec::new();
            for pending in std::mem::take(&mut inner.pending) {
                if inner.results.len() >= MAX_RESULTS {
                    if let Some(oldest) = inner.result_order.pop_front() {
                        inner.results.remove(&oldest);
                        inner.names.remove(&oldest);
                    }
                }
                let result = AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: Some("cancelled before start (manager shut down)".to_string()),
                    usage: None,
                    reason: ExitReason::ShutDown,
                    tool_calls: 0,
                    resume: None,
                };
                let notice = AgentNotice {
                    agent_id: pending.id.clone(),
                    name: pending.def.name.clone(),
                    status: AgentState::Cancelled,
                    usage: None,
                    resumable: false,
                    history: Vec::new(),
                    queued: true,
                };
                inner.result_order.push_back(pending.id.clone());
                inner
                    .names
                    .insert(pending.id.clone(), pending.def.name.clone());
                inner.results.insert(pending.id.clone(), result);
                if inner.notices.len() >= MAX_NOTICES {
                    inner.overflowed += 1;
                } else {
                    inner.notices.push_back(notice.clone());
                }
                events.push(AgentEvent::Completed(notice));
            }
            inner.open_until = None;
            for child in inner.running.values() {
                child.token.cancel();
            }
            (
                inner
                    .running
                    .values_mut()
                    .filter_map(|child| child.handle.take())
                    .collect::<Vec<JoinHandle<AgentResult>>>(),
                events,
            )
        };
        let hook = self.lock().events.clone();
        if let Some(hook) = hook {
            for event in events {
                hook(event);
            }
        }
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
            supervision: SupervisionSpec::default(),
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
                AgentEvent::Recovered {
                    agent_id, attempt, ..
                } => {
                    format!("recovered {agent_id} attempt {attempt}")
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
                history: Vec::new(),
                queued: true,
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
    async fn over_cap_queues_then_a_terminal_frees_a_slot() {
        // §24.5: over-cap spawns queue instead of rejecting — the old
        // AtCapacity contract moved to queue overflow
        // (`queue_overflow_rejects_with_capacity`).
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
        // The 5th request queues with a pre-allocated id.
        let queued = mgr
            .spawn(
                &test_def("one-too-many"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert_eq!(mgr.status(&queued), Some(AgentState::Pending));
        // Freeing one slot drains the queue: the queued child launches
        // and reaches its terminal through the same wrapper.
        mgr.cancel(&ids[0]);
        match mgr.wait(&ids[0], Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Cancelled),
            other => panic!("expected Finished, got {other:?}"),
        }
        // The queue drained: the child launched (still parked on its own
        // token, so it reports Running now, not Pending) — cancel it and
        // join it through the same wrapper.
        assert_eq!(mgr.status(&queued), Some(AgentState::Running));
        assert!(!mgr.is_queued(&queued));
        mgr.cancel(&queued);
        match mgr.wait(&queued, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.status(&queued), Some(AgentState::Cancelled));
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
            history: Vec::new(),
            queued: true,
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
            history: Vec::new(),
            queued: false,
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
            history: Vec::new(),
            queued: false,
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
            history: vec!["resumed after timed out".to_string()],
            queued: true,
        };
        assert_eq!(
            resumable.text(),
            "[agent explorer:sess-3] finished timed out · resumable with delegate(resume_from = \"sess-3\") · after 1 recoveries: resumed after timed out"
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
                    parent_id: None,
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: None,
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
    async fn escalate_clamps_exhausted_meter_to_full_cap() {
        // tool_calls == cap: a Some(0) handle would promise "write your
        // final summary without tools" while the loop still runs one
        // post-hoc round and then hard-fails — the handle advertises None
        // (the definition's full cap) instead; partial spend keeps the
        // honest remainder.
        let dir = PathBuf::from("/tmp/dex-supervision-clamp");
        let mgr = AgentManager::new("sess");
        let mut def = test_def("explorer");
        def.max_tool_iterations = Some(2);
        let spent_all = mgr
            .spawn(
                &def,
                test_seed(),
                SpawnMeta {
                    generation: 0,
                    parent_session: Some(dir.join("sess.jsonl")),
                    parent_id: None,
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: None,
                },
                |_, _, _| async {
                    AgentResult {
                        status: AgentState::TimedOut,
                        summary: "partial".to_string(),
                        error: Some("timed out after 600s".to_string()),
                        usage: None,
                        reason: ExitReason::Exhausted(crate::agent::subagent::ExhaustKind::Timeout),
                        tool_calls: 2,
                        resume: None,
                    }
                },
            )
            .unwrap();
        let spent_one = mgr
            .spawn(
                &def,
                test_seed(),
                SpawnMeta {
                    generation: 0,
                    parent_session: Some(dir.join("sess.jsonl")),
                    parent_id: None,
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: None,
                },
                |_, _, _| async {
                    AgentResult {
                        status: AgentState::TimedOut,
                        summary: "partial".to_string(),
                        error: Some("timed out after 600s".to_string()),
                        usage: None,
                        reason: ExitReason::Exhausted(crate::agent::subagent::ExhaustKind::Timeout),
                        tool_calls: 1,
                        resume: None,
                    }
                },
            )
            .unwrap();
        for (id, expected) in [(&spent_all, None), (&spent_one, Some(1))] {
            match mgr.wait(id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(result) => {
                    let handle = result.resume.expect("escalated with progress");
                    assert_eq!(handle.remaining_budget, expected);
                }
                other => panic!("expected Finished, got {other:?}"),
            }
        }
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
    async fn resume_generation_links_parent() {
        // §24.3 lineage: a resume generation names its original; fresh
        // spawns have no parent.
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert_eq!(mgr.parent_of(&first), None);
        let second = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta {
                    generation: 1,
                    parent_session: None,
                    parent_id: Some(first.clone()),
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: None,
                },
                token_body,
            )
            .unwrap();
        assert_eq!(mgr.parent_of(&second), Some(first));
        assert_eq!(mgr.parent_of(&AgentId("sess-9".to_string())), None);
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn over_cap_spawn_queues_then_drains_on_terminal() {
        // §24.5: the 5th spawn queues (Pending — the dormant state wakes
        // up) instead of rejecting; a sibling's terminal starts it.
        let mgr = AgentManager::new("sess");
        let mut running = Vec::new();
        for _ in 0..MAX_CHILDREN {
            running.push(
                mgr.spawn(
                    &test_def("explorer"),
                    test_seed(),
                    SpawnMeta::fresh(),
                    token_body,
                )
                .unwrap(),
            );
        }
        let queued = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("from the queue") },
            )
            .unwrap();
        assert_eq!(mgr.status(&queued), Some(AgentState::Pending));
        assert_eq!(mgr.active_count(), MAX_CHILDREN);
        assert!(mgr.is_queued(&queued));
        // `delegate_output`-shaped wait reports the queue, not Unknown.
        match mgr.wait(&queued, Duration::ZERO).await {
            WaitOutcome::Running(AgentState::Pending) => {}
            other => panic!("expected Running(Pending), got {other:?}"),
        }
        // A sibling goes terminal: the queued child starts.
        mgr.cancel(&running[0]);
        match mgr.wait(&running[0], Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        match mgr.wait(&queued, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert_eq!(result.summary, "from the queue");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.status(&queued), Some(AgentState::Completed));
        // Two notices: the cancelled sibling and the drained child.
        assert_eq!(mgr.drain_notices().len(), 2);
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_child_stop_cancels_without_launching() {
        // `delegate_stop` on a queued id: no process exists, so the cancel
        // synthesizes the terminal result and the queue drops the entry.
        let mgr = AgentManager::new("sess");
        for _ in 0..MAX_CHILDREN {
            mgr.spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        }
        let queued = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("never runs") },
            )
            .unwrap();
        assert_eq!(mgr.cancel(&queued), Some(AgentState::Cancelled));
        assert_eq!(mgr.active_count(), MAX_CHILDREN);
        match mgr.wait(&queued, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled);
                assert!(result.error.as_deref().unwrap().contains("before start"));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].agent_id, queued);
        assert_eq!(notices[0].status, AgentState::Cancelled);
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_overflow_rejects_with_capacity() {
        // 4 running + 8 queued: the 13th request still rejects — the
        // queue is bounded, unbounded queues would grow without bound.
        let mgr = AgentManager::new("sess");
        for _ in 0..MAX_CHILDREN {
            mgr.spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        }
        for _ in 0..MAX_PENDING {
            mgr.spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        }
        let error = mgr
            .spawn(
                &test_def("one-too-many"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap_err();
        assert!(matches!(
            error,
            SpawnError::QueueFull {
                limit: MAX_PENDING,
                ..
            }
        ));
        // The message names the queue (8 queued, 4 running), not a false
        // child-capacity claim.
        assert!(
            error
                .to_string()
                .contains("spawn queue full (8 queued, 4 running)"),
            "{}",
            error
        );
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queue_drains_fifo_across_terminals() {
        // §24.5: queued children launch in enqueue order, one per freed
        // slot — a terminal sibling drains the OLDEST queued entry first.
        let spawned: Arc<Mutex<Vec<AgentId>>> = Arc::new(Mutex::new(Vec::new()));
        let hook_spawned = spawned.clone();
        let mgr = AgentManager::new("sess").with_events(Arc::new(move |event| {
            if let AgentEvent::Spawned { agent_id, .. } = event {
                hook_spawned.lock().unwrap().push(agent_id);
            }
        }));
        // Fill the cap with children that only end through their token.
        let mut running = Vec::new();
        for _ in 0..MAX_CHILDREN {
            running.push(
                mgr.spawn(
                    &test_def("explorer"),
                    test_seed(),
                    SpawnMeta::fresh(),
                    token_body,
                )
                .unwrap(),
            );
        }
        let first_queued = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        let second_queued = mgr
            .spawn(
                &test_def("reviewer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert!(mgr.is_queued(&first_queued));
        assert!(mgr.is_queued(&second_queued));
        // Free one slot: the FIRST queued entry launches.
        mgr.cancel(&running[0]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !spawned.lock().unwrap().contains(&first_queued) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "first queued child never launched"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!spawned.lock().unwrap().contains(&second_queued));
        // Free another: the SECOND queued entry launches, still in order.
        mgr.cancel(&running[1]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !spawned.lock().unwrap().contains(&second_queued) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "second queued child never launched"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn queued_cancel_respects_the_retention_bound() {
        // Filing a queued cancel's synthesized result must not grow the
        // cache past `MAX_RESULTS` — the oldest retained result evicts,
        // exactly like `finish`.
        let mgr = AgentManager::new("sess");
        for i in 0..MAX_RESULTS {
            let id = mgr
                .spawn(
                    &test_def("explorer"),
                    test_seed(),
                    SpawnMeta::fresh(),
                    |_, _, _| async move { completed("done") },
                )
                .unwrap();
            match mgr.wait(&id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(_) => {}
                other => panic!("expected Finished for {i}, got {other:?}"),
            }
        }
        // Hold the cap and queue one child behind it.
        let mut running = Vec::new();
        for _ in 0..MAX_CHILDREN {
            running.push(
                mgr.spawn(
                    &test_def("explorer"),
                    test_seed(),
                    SpawnMeta::fresh(),
                    token_body,
                )
                .unwrap(),
            );
        }
        let queued = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert!(mgr.is_queued(&queued));
        mgr.cancel(&queued);
        // The cancel's result is present; the oldest result was evicted
        // to make room, so the bound held.
        match mgr.wait(&queued, Duration::ZERO).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        match mgr.wait(&running[0], Duration::ZERO).await {
            WaitOutcome::Running(state) => assert_eq!(state, AgentState::Running),
            WaitOutcome::Finished(_) => panic!("running child must stay live"),
            WaitOutcome::Unknown => panic!("running child must stay visible"),
        }
        let evicted = AgentId("sess-0".to_string());
        assert_eq!(
            mgr.wait(&evicted, Duration::ZERO).await,
            WaitOutcome::Unknown
        );
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_files_terminal_results_for_queued_children() {
        // A queued id outlives the queue: shutdown files a synthesized
        // `Cancelled` + notice per entry instead of leaving the ids to
        // read `Unknown` forever.
        let mgr = AgentManager::new("sess");
        for _ in 0..MAX_CHILDREN {
            mgr.spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        }
        let queued = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                SpawnMeta::fresh(),
                token_body,
            )
            .unwrap();
        assert!(mgr.is_queued(&queued));
        mgr.shutdown().await;
        match mgr.wait(&queued, Duration::ZERO).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled);
                assert_eq!(result.reason, ExitReason::ShutDown);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let notices = mgr.drain_notices();
        assert!(
            notices
                .iter()
                .any(|notice| notice.agent_id == queued && notice.queued),
            "{:?}",
            notices
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn escalate_fallback_queues_its_notice() {
        // Regression: the cap-race fallback marks its notice
        // `queued: true` — it must actually enter the notice queue, or
        // the idle wake it schedules no-ops at `has_notices()` and the
        // escalation never reaches the model's context.
        let dir = PathBuf::from("/tmp/dex-p13-fallback");
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta {
                    generation: 0,
                    parent_session: Some(dir.join("sess.jsonl")),
                    parent_id: None,
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: Some(5),
                },
                |_, _, _| async { exhausted_body() },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        let _ = mgr.drain_notices();
        let factory: ChildFactory =
            Arc::new(|_, _, _, _| Box::pin(async { completed("x") }) as BodyFuture);
        let reenter = ReEntry {
            def: test_def("explorer"),
            seed: test_seed(),
            meta: SpawnMeta::fresh(),
            factory,
            resume: None,
            name: "explorer".to_string(),
            attempt: 2,
            mode: RecoverMode::Resume,
            reason: ExitReason::Exhausted(ExhaustKind::Timeout),
            superseded_notice: AgentNotice {
                agent_id: id.clone(),
                name: "explorer".to_string(),
                status: AgentState::TimedOut,
                usage: None,
                resumable: false,
                history: Vec::new(),
                queued: false,
            },
            fallback: Fallback {
                transcript: Some(dir.join("agents").join("sess-0-explorer.jsonl")),
                generation: 0,
                allowance: Some(5),
                lineage: Vec::new(),
                reason: ExitReason::Exhausted(ExhaustKind::Timeout),
                tool_calls: 3,
            },
        };
        mgr.escalate_fallback(&id, &reenter);
        assert!(mgr.has_notices(), "queued flag without a queue entry");
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].queued);
        assert!(
            notices[0].resumable,
            "the retained result now carries a handle"
        );
        // The handle's meter is honest: allowance 5 − spend 3.
        match mgr.wait(&id, Duration::ZERO).await {
            WaitOutcome::Finished(result) => {
                let handle = result.resume.expect("fallback attaches the handle");
                assert_eq!(handle.remaining_budget, Some(2));
                assert_eq!(handle.generation, 0);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn breaker_open_queues_spawns_until_window_expiry() {
        // §24.5: an intensity-exhausted recovery escalation opens the
        // breaker; fresh spawns queue until the window expires and the
        // drain timer starts them. Uses a 1 s window so the expiry is
        // observable, not a 60 s wall-clock wait.
        let mut def = test_def("tester");
        def.supervision = SupervisionSpec {
            recover: RecoverMode::Resume,
            max: 1,
            window: Duration::from_secs(1),
        };
        // Attempt 0 + generation 1 both exhaust; the second death finds
        // the ledger full (max 1) → escalates AND opens the breaker.
        let factory: ChildFactory = Arc::new(|_, _, _, _| {
            Box::pin(async { exhausted_body() })
                as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(&def, test_seed(), retry_meta(factory), |_, _, _| async {
                exhausted_body()
            })
            .unwrap();
        match mgr.wait(&first, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        // The recovery (generation 1) also ran and exhausted; its
        // escalation found the ledger at max → breaker open.
        // (One recovery + one escalation = 2 generations total.)
        // A fresh spawn now queues instead of launching.
        let queued = mgr
            .spawn(
                &test_def("explorer"),
                test_seed(),
                SpawnMeta::fresh(),
                |_, _, _| async { completed("after the breaker") },
            )
            .unwrap();
        assert_eq!(mgr.status(&queued), Some(AgentState::Pending));
        // The window expires; the drain timer starts the child.
        match mgr.wait(&queued, Duration::from_secs(10)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert_eq!(result.summary, "after the breaker");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wrapper_synthesized_ending_reconciles_the_spend_meter() {
        // §24.1 (review fix): a body that panics mid-turn loses its own
        // sink counter, but the registry meter (`ProgressReporter::set`)
        // survives it — the resume budget must reconcile to cap − spend,
        // not the full cap.
        let dir = PathBuf::from("/tmp/dex-supervision-meter");
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("sess-0-tester.jsonl"),
            "{\"entry_type\":\"session\"}\n{\"entry_type\":\"turn_start\"}\n",
        )
        .unwrap();
        let mut def = test_def("tester");
        def.max_tool_iterations = Some(10);
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &def,
                test_seed(),
                SpawnMeta {
                    generation: 0,
                    parent_session: Some(dir.join("sess.jsonl")),
                    parent_id: None,
                    retry: None,
                    lineage: Vec::new(),
                    remaining_budget: None,
                },
                |token, progress, _| async move {
                    progress.set("read");
                    progress.set("bash");
                    // Body reports no count — it never got to synthesize.
                    assert!(!token.is_cancelled());
                    panic!("boom mid-turn");
                },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.reason, ExitReason::Transient);
                // The advertised budget reconciles against the registry
                // meter: 10 − 2, not the full 10.
                let handle = result.resume.expect("transient + progress advertises");
                assert_eq!(handle.remaining_budget, Some(8));
                assert!(handle.note.contains("2 tool calls"), "{}", handle.note);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A body result with a definition's supervision fields set.
    fn recover_def(mode: RecoverMode) -> AgentDefinition {
        let mut def = test_def("tester");
        def.supervision = SupervisionSpec {
            recover: mode,
            max: 2,
            window: Duration::from_secs(60),
        };
        def
    }

    fn retry_meta(factory: ChildFactory) -> SpawnMeta {
        SpawnMeta {
            generation: 0,
            parent_session: Some(PathBuf::from("/tmp/dex-p12")),
            parent_id: None,
            retry: Some(factory),
            lineage: Vec::new(),
            remaining_budget: None,
        }
    }

    /// A terminal body result classified `Exhausted(Timeout)` with spend —
    /// the shape a timed-out-then-synthesized child would carry, without
    /// waiting out a real timeout.
    fn exhausted_body() -> AgentResult {
        AgentResult {
            status: AgentState::TimedOut,
            summary: "partial".to_string(),
            error: Some("timed out".to_string()),
            usage: None,
            reason: ExitReason::Exhausted(ExhaustKind::Timeout),
            tool_calls: 3,
            resume: None,
        }
    }

    /// Second-spawn ids observed through the event hook (Recovered carries
    /// the new id; Spawned does too — either wakes the waiter).
    async fn wait_for_generation(seen: &Arc<Mutex<Vec<AgentId>>>, skip: &AgentId) -> AgentId {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(id) = seen.lock().unwrap().iter().find(|id| *id != skip).cloned() {
                return id;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("recovery generation never spawned");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_mode_reenters_transient_deaths_with_history() {
        // §24.4 + §24.8: `recover: resume` re-enters after an
        // Exhausted death within intensity. The superseded generation
        // retains silently (no handle, no notice); the factory receives
        // the replay request; the hook fires Recovered; the escalation
        // record travels in the lineage.
        let spawns: Arc<Mutex<Vec<AgentId>>> = Arc::new(Mutex::new(Vec::new()));
        let recovered: Arc<Mutex<Vec<(AgentId, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_resume: Arc<Mutex<Vec<Option<ResumeRequest>>>> = Arc::new(Mutex::new(Vec::new()));
        let hook_spawns = spawns.clone();
        let hook_recovered = recovered.clone();
        let mgr = AgentManager::new("sess").with_events(Arc::new(move |event| match event {
            AgentEvent::Spawned { agent_id, .. } => {
                hook_spawns.lock().unwrap().push(agent_id);
            }
            AgentEvent::Recovered {
                agent_id, attempt, ..
            } => {
                hook_recovered.lock().unwrap().push((agent_id, attempt));
            }
            AgentEvent::Progress { .. } | AgentEvent::Completed(_) => {}
        }));
        let factory_seen = seen_resume.clone();
        // The recovery body parks on this gate so the lineage assertion
        // below observes it while still Running (an instantly-ready body
        // would already be terminal — `parent_of` reads live children).
        let gate = Arc::new(tokio::sync::Notify::new());
        let factory_gate = gate.clone();
        let factory: ChildFactory = Arc::new(move |_, _, _, resume| {
            factory_seen.lock().unwrap().push(resume.clone());
            let gate = factory_gate.clone();
            Box::pin(async move {
                gate.notified().await;
                AgentResult {
                    status: AgentState::Completed,
                    summary: "recovered work".to_string(),
                    error: None,
                    usage: None,
                    reason: ExitReason::Normal,
                    tool_calls: 1,
                    resume: None,
                }
            }) as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let first = mgr
            .spawn(
                &recover_def(RecoverMode::Resume),
                test_seed(),
                retry_meta(factory),
                |_, _, _| async { exhausted_body() },
            )
            .unwrap();
        // The superseded generation settles silently: retained, but with
        // no handle and no notice of its own.
        match mgr.wait(&first, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert!(result.resume.is_none());
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let second = wait_for_generation(&spawns, &first).await;
        // Lineage while the generation is still live (parked on the gate).
        assert_eq!(mgr.parent_of(&second), Some(first.clone()));
        // Release the parked body and let it complete.
        gate.notify_one();
        match mgr.wait(&second, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert!(result.resume.is_none());
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        // The factory ran once, with a replay request: derived
        // transcript, full meter (test defs are uncapped), and the
        // lineage line describing this recovery.
        let (count, transcript_ok, generation, history) = {
            let calls = seen_resume.lock().unwrap();
            (
                calls.len(),
                calls[0].as_ref().is_some_and(|request| {
                    request
                        .handle
                        .transcript
                        .ends_with("agents/sess-0-tester.jsonl")
                }),
                calls[0].as_ref().map(|request| request.handle.generation),
                calls[0]
                    .as_ref()
                    .map(|request| request.handle.history.clone()),
            )
        };
        assert_eq!(count, 1);
        assert!(transcript_ok);
        assert_eq!(generation, Some(0));
        assert_eq!(history, Some(vec!["resumed after timed out".to_string()]));
        // Exactly one parent notice — the new generation's completion.
        // The Recovered hook fired with attempt 2.
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].agent_id, second);
        let (recovered_len, recovered_target, recovered_attempt) = {
            let recovered = recovered.lock().unwrap();
            (
                recovered.len(),
                recovered.first().map(|(id, _)| id.clone()),
                recovered.first().map(|(_, attempt)| *attempt),
            )
        };
        assert_eq!(recovered_len, 1);
        assert_eq!(recovered_target, Some(second.clone()));
        assert_eq!(recovered_attempt, Some(2));
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fresh_mode_reruns_with_an_empty_transcript() {
        // `recover: fresh` re-enters through the resume plumbing without
        // replaying: a generation-only request (the body then writes the
        // `.g<N>` file the registry derived at spawn time — registry and
        // file can never disagree), full meter, no nudge.
        let seen_resume: Arc<Mutex<Vec<Option<ResumeRequest>>>> = Arc::new(Mutex::new(Vec::new()));
        let factory_seen = seen_resume.clone();
        let factory: ChildFactory = Arc::new(move |_, _, _, resume| {
            factory_seen.lock().unwrap().push(resume.clone());
            let request = resume.expect("fresh mode rides the spawn path");
            assert_eq!(request.mode, RecoverMode::Fresh);
            assert_eq!(request.instruction, None);
            assert!(request.file_hints.is_empty());
            assert_eq!(request.handle.remaining_budget, None);
            Box::pin(async move { completed("redone") })
                as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(
                &recover_def(RecoverMode::Fresh),
                test_seed(),
                retry_meta(factory),
                |_, _, _| async { exhausted_body() },
            )
            .unwrap();
        match mgr.wait(&first, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        // The request names the generation the registry points at: the
        // body derives `.g1` from it (`handle.generation + 1`).
        {
            let calls = seen_resume.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].as_ref().map(|request| request.handle.generation),
                Some(0)
            );
        }
        assert_eq!(mgr.drain_notices().len(), 1);
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fresh_lineage_stops_at_the_recovery_cap() {
        // A lineage that exhausts on every attempt must stop: the window
        // never binds here (max 100, all deaths land inside it), so the
        // per-lineage cap is what ends the treadmill —
        // MAX_LINEAGE_RECOVERIES re-entries, then escalate.
        let calls = Arc::new(Mutex::new(0usize));
        let factory_calls = calls.clone();
        let factory: ChildFactory = Arc::new(move |_, _, _, _resume| {
            *factory_calls.lock().unwrap() += 1;
            Box::pin(async move { exhausted_body() })
                as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let mut def = recover_def(RecoverMode::Fresh);
        def.supervision.max = 100;
        def.max_tool_iterations = Some(4);
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(&def, test_seed(), retry_meta(factory), |_, _, _| async {
                exhausted_body()
            })
            .unwrap();
        match mgr.wait(&first, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        // The chain settles: poll until the escalated notice lands.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let notices = loop {
            let notices = mgr.drain_notices();
            if !notices.is_empty() {
                break notices;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "lineage never escalated at the cap"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(notices.len(), 1);
        assert!(notices[0].resumable, "escalation advertises a handle");
        assert_eq!(
            notices[0].history.len(),
            crate::agent::subagent::exit::MAX_LINEAGE_RECOVERIES,
            "{:?}",
            notices[0].history
        );
        assert_eq!(
            *calls.lock().unwrap(),
            crate::agent::subagent::exit::MAX_LINEAGE_RECOVERIES
        );
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intensity_exhaustion_escalates_with_history() {
        // max 1: the first death recovers, the second escalates with the
        // lineage verbatim in the notice.
        let mut def = test_def("tester");
        def.supervision = SupervisionSpec {
            recover: RecoverMode::Resume,
            max: 1,
            window: Duration::from_secs(60),
        };
        let calls = Arc::new(Mutex::new(0usize));
        let factory_calls = calls.clone();
        let factory: ChildFactory = Arc::new(move |_, _, _, _resume| {
            *factory_calls.lock().unwrap() += 1;
            Box::pin(async move { exhausted_body() })
                as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(&def, test_seed(), retry_meta(factory), |_, _, _| async {
                exhausted_body()
            })
            .unwrap();
        match mgr.wait(&first, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        // Only one factory call: the second death exhausted the ledger.
        assert_eq!(*calls.lock().unwrap(), 1);
        // The escalated generation advertises with history; it is the
        // only notice in the queue.
        let notices = mgr.drain_notices();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].resumable);
        assert_eq!(
            notices[0].history,
            vec!["resumed after timed out".to_string()]
        );
        assert!(notices[0].text().contains("after 1 recoveries"));
        mgr.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn never_recovers_escalates_without_calling_the_factory() {
        // `recover: never` (the default) never re-enters, even with a
        // factory installed: today's behavior, byte-identical.
        let calls = Arc::new(Mutex::new(0usize));
        let factory_calls = calls.clone();
        let factory: ChildFactory = Arc::new(move |_, _, _, _| {
            *factory_calls.lock().unwrap() += 1;
            Box::pin(async move { completed("unreachable") })
                as Pin<Box<dyn Future<Output = AgentResult> + Send>>
        });
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(
                &test_def("tester"),
                test_seed(),
                retry_meta(factory),
                |_, _, _| async { exhausted_body() },
            )
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::TimedOut);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(*calls.lock().unwrap(), 0);
        assert_eq!(mgr.drain_notices().len(), 1);
        mgr.shutdown().await;
    }
}
