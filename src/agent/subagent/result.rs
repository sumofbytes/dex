use super::instance::AgentState;

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentResult {
    /// Terminal status: `Completed` | `Failed` | `Cancelled` | `TimedOut`.
    pub(crate) status: AgentState,
    /// The child's final assistant message, verbatim (or synthesized).
    pub(crate) summary: String,
    /// Actionable message for non-`Completed` endings.
    pub(crate) error: Option<String>,
}
