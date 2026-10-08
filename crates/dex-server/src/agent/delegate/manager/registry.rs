use super::super::exit::ExitReason;
use super::super::model::AgentId;
use super::super::model::AgentInstance;
use super::super::model::AgentResult;
use super::super::model::AgentState;
use super::super::model::AgentUsage;
use super::super::resume::advertised_remaining;
use super::super::resume::resume_note;
use super::super::resume::ResumeHandle;
use super::lifecycle::EventHook;
use super::lifecycle::ProgressReporter;
use super::lifecycle::TaskEventHook;
use crate::protocol::QueueMsg;
use crate::runtime::console::CancellationToken;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Completion announcement queued for the Phase 6 drain site (parent turn
/// end, which renders these into context). Notices are informational only —
/// the retained [`AgentResult`] is the source of truth.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentNotice {
    pub agent_id: AgentId,
    pub name: String,
    pub status: AgentState,
    /// Child token spend, when the body reported any (§18: the lifecycle
    /// line carries it so client-side spend accounting stays honest).
    pub usage: Option<AgentUsage>,
    /// True when the retained result carries a [`ResumeHandle`]: the prose
    /// advertises `delegate(resume_from = …)`. Never part of the
    /// `[agent …] finished …` prefix the TUI matches on (§24.1:
    /// resumability is notice prose only, zero wire change).
    pub resumable: bool,
    /// True when the retained result is a `Completed` child whose handle is
    /// a *continuation* target: the prose advertises `delegate(send)` into
    /// its history instead of a `resume_from` re-entry. Exactly one of
    /// `resumable`/`continuable` is ever true (the manager's `finish` sets
    /// both from the same handle).
    pub continuable: bool,
}

impl AgentNotice {
    /// The §15 V1a lifecycle line: the stable `[agent <name>:<id>] finished
    /// <status>` prefix the TUI matches on, plus the child's token spend
    /// when reported and a resume hint when the result carries a handle.
    /// Cost is shown only when priced (an unpriced model renders no
    /// `$0.0000` noise). Pure; unit-tested.
    pub fn text(&self) -> String {
        let mut text = format!(
            "[agent {}:{}] finished {}",
            self.name,
            self.agent_id,
            super::super::status_word(self.status)
        );
        if let Some(usage) = self.usage {
            text.push_str(&format!(
                " · {} tok",
                crate::protocol::tokens::format_tokens(usage.prompt_tokens + usage.output_tokens)
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
        } else if self.continuable {
            text.push_str(&format!(
                " · continuable with delegate(send, agent_id = \"{0}\")",
                self.agent_id
            ));
        }
        text
    }
}

/// One row of the `delegate` `list` view (§24.3): live or retained.
#[derive(Clone, Debug)]
pub struct ChildInfo {
    pub agent_id: AgentId,
    pub name: String,
    pub state: AgentState,
    pub progress: Option<String>,
    /// `resume_from`-capable ending (interrupted / budgeted, with progress).
    pub resumable: bool,
    /// A `Completed` child whose handle addresses a continuation via
    /// `delegate(send)`.
    pub continuable: bool,
    pub transcript: Option<PathBuf>,
}

/// Bounded-wait outcome for [`AgentManager::wait`]. The result is boxed:
/// it is by far the largest variant and would trip
/// `clippy::large_enum_variant` against the byte-sized `Running`.
#[derive(Debug, PartialEq)]
pub enum WaitOutcome {
    /// Terminal result; also retained for later fetch after notice drain.
    Finished(Box<AgentResult>),
    /// Still live when `timeout` elapsed; carries the last-seen state.
    Running(AgentState),
    /// Unknown id: never spawned here, or its result aged out of retention.
    Unknown,
}

/// `spawn` rejection. `AtCapacity` carries the running list so the caller
/// (model or operator) can wait for or cancel a child instead of
/// guessing; `Closed` means the manager was shut down — no new children,
/// ever.
#[derive(Debug, PartialEq, Eq)]
pub enum SpawnError {
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

/// Which of a child's two queues a `send` message lands in: `Steer` is
/// drained by the child's *running* turn at its next round boundary (the
/// same [`QueueMsg`] channel semantics the main turn drains), `FollowUp`
/// waits for the current turn to end and chains a new one on the same
/// history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendDelivery {
    Steer,
    FollowUp,
}

impl SendDelivery {
    /// The `delegate` `send` `delivery` argument strings; `"steer"` is
    /// the default.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "steer" => Some(Self::Steer),
            "follow_up" => Some(Self::FollowUp),
            _ => None,
        }
    }
}

/// Success of [`AgentManager::send`](super::lifecycle::AgentManager::send):
/// which queue actually took the message (the recorded answer to "will the
/// child see it mid-turn or next turn?").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// Injected into the running turn; consumed at the next drain point.
    Steered,
    /// Parked on the follow-up queue; a new turn runs it when this turn ends.
    Queued,
}

/// `send` rejection. `Unknown` names the ids the manager does know so the
/// caller's error can point at the right child without another round trip;
/// `NotRunning` means the id is known but terminal (the tool layer can then
/// try the continue-from-completed path); `Full` is a full mailbox.
#[derive(Debug)]
pub enum SendError {
    Unknown { known: Vec<(AgentId, String)> },
    NotRunning,
    Full,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown { known } => {
                let list = known
                    .iter()
                    .map(|(id, name)| format!("{id} ({name})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "unknown agent id: delegate action=list shows this session's children (known: {list})")
            }
            Self::NotRunning => {
                write!(f, "agent is not running; its result is retained")
            }
            Self::Full => {
                write!(f, "queue is full; wait (action=wait) before sending more")
            }
        }
    }
}

impl std::error::Error for SendError {}

/// Capacity of each per-child queue (same widening margin the main turn's
/// steering queue uses): a hand-send burst parks instead of wedging the
/// parent, and past it `send` fails loudly.
pub const QUEUE_CAPACITY: usize = 16;

/// The manager-owned input channels handed to a child body at launch:
/// `steering_rx` is the same `mpsc::Receiver<QueueMsg>` shape the main turn
/// drains before every model call, `followup_rx` chains new turns when the
/// current one ends. The matching senders live in the registry entry, so
/// `AgentManager::send` never races the body's lifetime by value alone.
pub struct ChildQueues {
    pub steering_rx: mpsc::Receiver<QueueMsg>,
    pub followup_rx: mpsc::Receiver<QueueMsg>,
}

pub struct Inner {
    pub session: String,
    pub next_counter: u64,
    /// Set by `shutdown`: after it, `spawn` rejects. A stale `AgentManager`
    /// clone (e.g. held by an in-flight tool call) cannot then orphan a
    /// child into a registry nobody will ever join (plan §14).
    pub closed: bool,
    /// Set by the daemon (`AgentManager::with_events`): invoked on every
    /// terminal path with the completion notice, so child lifecycle lines
    /// (§15 V1a `[agent <name>:<id>] finished <status>`) are journaled at
    /// completion time even while no turn is live. `None` for test-built
    /// managers.
    pub events: Option<EventHook>,
    pub running: HashMap<AgentId, RunningChild>,
    pub results: HashMap<AgentId, AgentResult>,
    /// Display names for retained results (results carry no name — the
    /// parent's `delegate` `list` action needs one). Pruned with `results`.
    pub names: HashMap<AgentId, String>,
    /// Insertion order of `results`, for oldest-first eviction.
    pub result_order: VecDeque<AgentId>,
    pub notices: VecDeque<AgentNotice>,
    /// Completions dropped because `notices` was full.
    pub overflowed: usize,
    /// Background shell-task registry (spec Rev 3): per-session, ids
    /// `task-1`…, reachable via the daemon's per-session manager so tool
    /// calls (which hold a manager, not `DaemonState`) can poll/stop.
    pub bg: crate::daemon::tasks::TaskRegistry,
    /// Background-task completion notices, sibling to `notices` (never
    /// conflated so agent drains never misrender task rows).
    pub bg_notices: VecDeque<crate::daemon::tasks::TaskNotice>,
    /// Lifecycle hook for task events (journal + broadcast + wake),
    /// attached by the daemon beside `events`; `None` for test managers.
    pub bg_events: Option<TaskEventHook>,
    /// Per-task status broadcast: `bg_finish` publishes the terminal
    /// status here so `wait`/`stop` can await changes instead of polling.
    pub bg_watch: HashMap<String, tokio::sync::watch::Sender<crate::daemon::tasks::TaskStatus>>,
    /// Drain-task handles, aborted on stop/teardown (the drain owns the
    /// `Child`; the registry keeps only the pid).
    pub bg_handles: HashMap<String, JoinHandle<()>>,
}

/// Named snapshot of the registry entry at `finish` time. `None` only
/// for an id that was never registered (defensive — wrappers always
/// register first); then the result settles handle-free.
pub struct Record {
    pub generation: u32,
    pub transcript: Option<PathBuf>,
    /// Registry-side spend meter (§24.1), surviving body death.
    pub calls: u32,
    /// Registry-side token spend snapshot at `finish` time (spec G3):
    /// results that carry none fold this in so synthesized endings report
    /// what their LLM calls cost.
    pub usage: AgentUsage,
    /// The tool-round budget this child was launched under: the resume's
    /// remaining budget, else the definition's cap; `None` = uncapped.
    pub allowance: Option<usize>,
    /// Effective model override at spawn (`def.model`); carried into the
    /// resume handle so a generation without its own `model` keeps the
    /// per-spawn model instead of resetting to the parent.
    pub model: Option<String>,
}

/// The resume handle for a recorded child, transcript-gated (§24.1):
/// only a child whose file is known can advertise a re-entry. A `Completed`
/// child (`ExitReason::Normal`) advertises a *continuation* handle instead:
/// it re-enters via `delegate(send)` with a fresh budget and the parent's
/// current approvals, not via `resume_from`.
pub fn escalate_handle(
    record: &Record,
    id: &AgentId,
    reason: ExitReason,
    tool_calls: u32,
) -> Option<ResumeHandle> {
    let transcript = record.transcript.clone()?;
    let continuable = matches!(reason, ExitReason::Normal);
    let remaining = if continuable {
        // A continuation is a new task over kept history: it always runs
        // the full tool budget, never the parent generation's leftover.
        None
    } else {
        advertised_remaining(record.allowance, tool_calls as usize)
    };
    Some(ResumeHandle {
        agent_id: id.clone(),
        transcript,
        generation: record.generation,
        remaining_budget: remaining,
        model: record.model.clone(),
        note: resume_note(reason, tool_calls),
        continuable,
    })
}

pub struct RunningChild {
    pub instance: AgentInstance,
    pub token: CancellationToken,
    pub handle: Option<JoinHandle<AgentResult>>,
    /// `steer` inbox: `AgentManager::send` tries into this; the body's
    /// turn loop drains it at round boundaries (spec G1).
    pub steering_tx: mpsc::Sender<QueueMsg>,
    /// `follow_up` inbox: drained after the current turn ends, before the
    /// child is retired.
    pub followup_tx: mpsc::Sender<QueueMsg>,
    /// Registry-side token spend (spec G3): the body's sink consumer folds
    /// every `SinkLine::Usage` in here, so the wrapper can attach spend to
    /// synthesized results (panic, timeout, cancel) whose body never got
    /// to build a result. Mirrors the `calls` meter, which lives here for
    /// the same reason.
    pub usage: AgentUsage,
    /// Derived transcript path (§16 + §24.3 generations), when the spawn
    /// knew the parent session file. `None` for test-built managers —
    /// then no resume handle is ever advertised.
    pub transcript: Option<PathBuf>,
    /// The generation this child runs as (0 = fresh). Resume spawns +1.
    pub generation: u32,
    /// Registry-side spend meter (§24.1): bumped by `ProgressReporter::set`
    /// per tool call, so a wrapper-synthesized ending (panic, timeout)
    /// still reports the spend its body made. Reconciled against the
    /// body's own count at `finish` (`max` — both count the same events,
    /// the registry meter just survives the body's death).
    pub calls: u32,
    /// The tool-round budget this child was launched under: the resume's
    /// remaining budget, else the definition's cap; `None` = uncapped.
    pub allowance: Option<usize>,
}

/// One attempt's built future.
pub type BodyFuture = Pin<Box<dyn Future<Output = AgentResult> + Send>>;

/// Spawn bodies (FnOnce) funnel through the shared launch path in this
/// shape: token, reporter, id, and the per-child steering queues.
pub type BoxRun =
    Box<dyn FnOnce(CancellationToken, ProgressReporter, AgentId, ChildQueues) -> BodyFuture + Send>;

/// What `spawn` needs beyond definition + seed (§24.3): which generation
/// this child is, where its transcript derives from, and the tool-round
/// budget the body runs under. Fresh spawns use `SpawnMeta::fresh()`.
#[derive(Clone)]
pub struct SpawnMeta {
    pub generation: u32,
    pub parent_session: Option<PathBuf>,
    /// The tool-round budget the body runs under: the resume's remaining
    /// budget, else the definition's cap. `None` for uncapped
    /// definitions. Mirrored into `RunningChild::allowance` so the
    /// advertised resume budget reconciles spend.
    pub remaining_budget: Option<usize>,
}

#[cfg(test)]
impl SpawnMeta {
    pub fn fresh() -> Self {
        Self {
            generation: 0,
            parent_session: None,
            remaining_budget: None,
        }
    }
}
