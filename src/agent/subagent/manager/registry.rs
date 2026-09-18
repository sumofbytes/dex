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
use crate::runtime::console::CancellationToken;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::task::JoinHandle;

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
            super::super::status_word(self.status)
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
/// (model or operator) can wait for or cancel a child instead of
/// guessing; `Closed` means the manager was shut down — no new children,
/// ever.
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
pub(crate) struct Inner {
    pub(crate) session: String,
    pub(crate) next_counter: u64,
    /// Set by `shutdown`: after it, `spawn` rejects. A stale `AgentManager`
    /// clone (e.g. held by an in-flight tool call) cannot then orphan a
    /// child into a registry nobody will ever join (plan §14).
    pub(crate) closed: bool,
    /// Set by the daemon (`AgentManager::with_events`): invoked on every
    /// terminal path with the completion notice, so child lifecycle lines
    /// (§15 V1a `[agent <name>:<id>] finished <status>`) are journaled at
    /// completion time even while no turn is live. `None` for test-built
    /// managers.
    pub(crate) events: Option<EventHook>,
    pub(crate) running: HashMap<AgentId, RunningChild>,
    pub(crate) results: HashMap<AgentId, AgentResult>,
    /// Display names for retained results (results carry no name — the
    /// parent's `delegate_list` needs one). Pruned with `results`.
    pub(crate) names: HashMap<AgentId, String>,
    /// Insertion order of `results`, for oldest-first eviction.
    pub(crate) result_order: VecDeque<AgentId>,
    pub(crate) notices: VecDeque<AgentNotice>,
    /// Completions dropped because `notices` was full.
    pub(crate) overflowed: usize,
}

/// Named snapshot of the registry entry at `finish` time. `None` only
/// for an id that was never registered (defensive — wrappers always
/// register first); then the result settles handle-free.
pub(crate) struct Record {
    pub(crate) generation: u32,
    pub(crate) transcript: Option<PathBuf>,
    /// Registry-side spend meter (§24.1), surviving body death.
    pub(crate) calls: u32,
    /// The tool-round budget this child was launched under: the resume's
    /// remaining budget, else the definition's cap; `None` = uncapped.
    pub(crate) allowance: Option<usize>,
    /// Effective model override at spawn (`def.model`); carried into the
    /// resume handle so a generation without its own `model` keeps the
    /// complexity-chosen model instead of resetting to the parent.
    pub(crate) model: Option<String>,
}

/// The resume handle for a recorded child, transcript-gated (§24.1):
/// only a child whose file is known can advertise a re-entry.
pub(crate) fn escalate_handle(
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
        model: record.model.clone(),
        note: resume_note(reason, tool_calls),
    })
}

pub(crate) struct RunningChild {
    pub(crate) instance: AgentInstance,
    pub(crate) token: CancellationToken,
    pub(crate) handle: Option<JoinHandle<AgentResult>>,
    /// Derived transcript path (§16 + §24.3 generations), when the spawn
    /// knew the parent session file. `None` for test-built managers —
    /// then no resume handle is ever advertised.
    pub(crate) transcript: Option<PathBuf>,
    /// The generation this child runs as (0 = fresh). Resume spawns +1.
    pub(crate) generation: u32,
    /// Registry-side spend meter (§24.1): bumped by `ProgressReporter::set`
    /// per tool call, so a wrapper-synthesized ending (panic, timeout)
    /// still reports the spend its body made. Reconciled against the
    /// body's own count at `finish` (`max` — both count the same events,
    /// the registry meter just survives the body's death).
    pub(crate) calls: u32,
    /// The tool-round budget this child was launched under: the resume's
    /// remaining budget, else the definition's cap; `None` = uncapped.
    pub(crate) allowance: Option<usize>,
}

/// One attempt's built future.
pub(crate) type BodyFuture = Pin<Box<dyn Future<Output = AgentResult> + Send>>;

/// Spawn bodies (FnOnce) funnel through the shared launch path in this shape.
pub(crate) type BoxRun =
    Box<dyn FnOnce(CancellationToken, ProgressReporter, AgentId) -> BodyFuture + Send>;

/// What `spawn` needs beyond definition + seed (§24.3): which generation
/// this child is, where its transcript derives from, and the tool-round
/// budget the body runs under. Fresh spawns use `SpawnMeta::fresh()`.
#[derive(Clone)]
pub(crate) struct SpawnMeta {
    pub(crate) generation: u32,
    pub(crate) parent_session: Option<PathBuf>,
    /// The tool-round budget the body runs under: the resume's remaining
    /// budget, else the definition's cap. `None` for uncapped
    /// definitions. Mirrored into `RunningChild::allowance` so the
    /// advertised resume budget reconciles spend.
    pub(crate) remaining_budget: Option<usize>,
}

impl SpawnMeta {
    pub(crate) fn fresh() -> Self {
        Self {
            generation: 0,
            parent_session: None,
            remaining_budget: None,
        }
    }
}
