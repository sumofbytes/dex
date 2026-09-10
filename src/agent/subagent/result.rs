use super::instance::AgentState;

/// Child token spend (§18), accumulated from the child's own `record_usage`
/// emissions — the same per-call usage the shared turn loop already prices.
/// `cost_usd` carries the daemon-priced amount, so the lifecycle line never
/// re-prices client-side. Tokens default to zero, cost to zero: a child that
/// never reached an LLM call has nothing to report.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AgentUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cost_usd: f64,
}

impl AgentUsage {
    /// Fold one provider-reported call into the totals (saturating, so a
    /// pathological count cannot wrap).
    pub(crate) fn absorb(&mut self, tokens: u64, output: u64, cost: f64) {
        self.prompt_tokens = self.prompt_tokens.saturating_add(tokens);
        self.output_tokens = self.output_tokens.saturating_add(output);
        self.cost_usd += cost;
    }

    /// `Some` only when at least one call was reported, so results without
    /// LLM activity carry `None` instead of a zero row.
    pub(crate) fn reported(self) -> Option<Self> {
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
pub(crate) struct AgentResult {
    /// Terminal status: `Completed` | `Failed` | `Cancelled` | `TimedOut`.
    pub(crate) status: AgentState,
    /// The child's final assistant message, verbatim (or synthesized).
    pub(crate) summary: String,
    /// Actionable message for non-`Completed` endings.
    pub(crate) error: Option<String>,
    /// Token spend the child itself reported, when it made LLM calls.
    pub(crate) usage: Option<AgentUsage>,
}
