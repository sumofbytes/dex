use super::context::ContextSeed;
use super::definition::AgentDefinition;

/// A short unique id (counter + session slug once the Phase 4 manager owns
/// the counter). Appears in events, session paths, results, approval labels.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AgentId(pub(crate) String);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One execution of a definition. Declarative config (`definition`,
/// `context`) stays separate from runtime state (`state`) — never mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentState {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl AgentState {
    /// Terminal states all yield an `AgentResult` and a completion notice;
    /// only `Running` keeps a registry entry and a task alive — spawns
    /// reject at capacity instead of queueing, so there is no dormant
    /// state to represent.
    pub(crate) fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One run of an [`AgentDefinition`]: what it is (`definition`), what it
/// was asked (`context`), how it's doing (`state`). The Phase 4 manager
/// owns the registry of these plus the task handles, tokens, and
/// timeouts — the instance itself holds no runtime machinery.
#[derive(Clone, Debug)]
pub(crate) struct AgentInstance {
    pub(crate) id: AgentId,
    pub(crate) definition: AgentDefinition,
    pub(crate) context: ContextSeed,
    pub(crate) state: AgentState,
    /// Tool the child is currently running, as reported through its
    /// [`ProgressReporter`](super::manager::ProgressReporter) — the
    /// Phase 6 `progress <tool>` render reads this (plan §15).
    pub(crate) progress: Option<String>,
}
