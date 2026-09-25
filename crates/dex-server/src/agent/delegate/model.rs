//! Child-model types: the declarative input (`ContextSeed`), the run
//! handle (`AgentInstance` + `AgentId` + `AgentState`), and the outcome
//! (`AgentResult` + `AgentUsage`). One module — the three are always
//! imported together and no boundary runs between them.

use std::path::PathBuf;

use super::definition::AgentDefinition;
use super::exit::ExitReason;
use super::resume::ResumeHandle;

/// The isolated input a child is given (plan §5). The parent's transcript
/// is never copied — whatever the child needs arrives here, written by the
/// parent model into the `delegate` call.
#[derive(Clone, Debug)]
pub struct ContextSeed {
    /// Required: what the child must do, in the parent model's own words.
    pub task: String,
    /// Optional workspace-relative file hints.
    pub file_hints: Vec<PathBuf>,
    /// Optional model-written background the child can't get otherwise.
    pub parent_summary: Option<String>,
}

/// A short unique id (counter + session slug once the Phase 4 manager owns
/// the counter). Appears in events, session paths, results, approval labels.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AgentId(pub String);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One execution of a definition. Declarative config (`definition`,
/// `context`) stays separate from runtime state (`state`) — never mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
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
    #[cfg(test)]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One run of an [`AgentDefinition`]: what it is (`definition`), what it
/// was asked (`context`), how it's doing (`state`). The Phase 4 manager
/// owns the registry of these plus the task handles, tokens, and
/// timeouts — the instance itself holds no runtime machinery.
#[derive(Clone, Debug)]
pub struct AgentInstance {
    pub definition: AgentDefinition,
    pub state: AgentState,
    /// Tool the child is currently running, as reported through its
    /// [`ProgressReporter`](super::manager::ProgressReporter) — the
    /// Phase 6 `progress <tool>` render reads this (plan §15).
    pub progress: Option<String>,
}

/// Child token spend (§18), accumulated from the child's own `record_usage`
/// emissions — the same per-call usage the shared turn loop already prices.
/// `cost_usd` carries the daemon-priced amount, so the lifecycle line never
/// re-prices client-side. Tokens default to zero, cost to zero: a child that
/// never reached an LLM call has nothing to report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentUsage {
    pub prompt_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl AgentUsage {
    /// Fold one provider-reported call into the totals (saturating, so a
    /// pathological count cannot wrap).
    pub fn absorb(&mut self, tokens: u64, output: u64, cost: f64) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(tokens);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cost_usd += cost;
    }

    /// `Some` only when at least one call was reported, so results without
    /// LLM activity carry `None` instead of a zero row.
    pub fn reported(self) -> Option<Self> {
        (self.prompt_tokens + self.output_tokens > 0).then_some(self)
    }
}

/// What the parent consumes: the child's final assistant message plus a
/// status — the shape pi, Claude Code, and Amp's oracle all ship. No
/// forced JSON across heterogeneous providers; findings, paths, and risks
/// live in the prose, and a child that wants to hand over structured data
/// points at files the parent reads with its own tools.
///
/// Synthesis when the child ends without a final message (budget
/// exhaustion, mid-turn cancel, mid-LLM-call timeout) is the Phase 7
/// manager's job: `summary` = last partial text if any else `""`, `error`
/// always set. Every terminal state yields one; only `Completed`
/// guarantees a non-empty `summary`.
///
/// `usage` is `None` for the wrapper-synthesized endings (panic, timeout,
/// cancel-before-return), where the body never produced a result to carry
/// it — the child JSONL still holds any spend up to the drop.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentResult {
    /// Terminal status: `Completed` | `Failed` | `Cancelled` | `TimedOut`.
    pub status: AgentState,
    /// The child's final assistant message, verbatim (or synthesized).
    pub summary: String,
    /// Actionable message for non-`Completed` endings.
    pub error: Option<String>,
    /// Token spend the child itself reported, when it made LLM calls.
    pub usage: Option<AgentUsage>,
    /// §24.1 classification — never surfaced to the model; the policy
    /// machine's input, set by the body (or the wrapper arm) and read by
    /// the manager's `finish`.
    pub reason: ExitReason,
    /// Sink-counted tool invocations. The body runs a single
    /// `process_turn`, so this is the spend meter resume budgets from.
    pub tool_calls: u32,
    /// Set only by the manager's `finish` — never by the child body.
    /// `Some` ⇔ Transient/Exhausted *and* progress made.
    pub resume: Option<ResumeHandle>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_cover_every_ending() {
        assert!(!AgentState::Running.is_terminal());
        for state in [
            AgentState::Completed,
            AgentState::Failed,
            AgentState::Cancelled,
            AgentState::TimedOut,
        ] {
            assert!(state.is_terminal(), "{state:?}");
        }
    }
}
