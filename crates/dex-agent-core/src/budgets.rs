//! Turn and context budget policies shared by agent runtimes.

/// Context thresholds used to decide whether history should be compacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionBudget {
    /// Token threshold including ephemeral prompt and tool-schema overhead.
    pub token_threshold: u64,
    /// Number of recent messages retained as a count-based fallback.
    pub keep_recent_messages: usize,
    /// Number of non-compacted prefix messages excluded from the recent window.
    pub prefix_messages: usize,
}

impl CompactionBudget {
    /// Returns whether the current history crosses either compaction threshold.
    pub fn should_compact(
        self,
        stored_tokens: u64,
        ephemeral_overhead: u64,
        message_count: usize,
    ) -> bool {
        stored_tokens.saturating_add(ephemeral_overhead) > self.token_threshold
            || message_count
                > self
                    .prefix_messages
                    .saturating_add(self.keep_recent_messages)
    }
}

/// Outcome after completing a batch of tool calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolRoundOutcome {
    Continue { completed: usize, limit: usize },
    Warn { completed: usize, limit: usize },
    Exhausted { completed: usize, limit: usize },
}

/// Wording shared by the transcript marker and the returned error when a
/// turn's tool-round budget is exhausted (host pushes the marker, the engine
/// returns the error — one source so they can't drift).
pub fn tool_budget_exhausted_note(completed: usize) -> String {
    format!(
        "turn budget exhausted after {completed} tool rounds; partial progress preserved — send another prompt to continue"
    )
}

/// Counts completed tool batches and emits a single near-limit warning.
#[derive(Clone, Copy, Debug)]
pub struct ToolRoundBudget {
    limit: usize,
    completed: usize,
    warned: bool,
}

impl ToolRoundBudget {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            completed: 0,
            warned: false,
        }
    }

    /// Record one completed tool batch. A fan-out batch counts as one round.
    pub fn complete_round(&mut self) -> ToolRoundOutcome {
        self.completed = self.completed.saturating_add(1);
        if self.completed >= self.limit {
            return ToolRoundOutcome::Exhausted {
                completed: self.completed,
                limit: self.limit,
            };
        }
        if !self.warned && self.completed.saturating_mul(5) >= self.limit.saturating_mul(4) {
            self.warned = true;
            return ToolRoundOutcome::Warn {
                completed: self.completed,
                limit: self.limit,
            };
        }
        ToolRoundOutcome::Continue {
            completed: self.completed,
            limit: self.limit,
        }
    }

    /// Completed tool batches — read-only view for a loop driver that
    /// reports state to a Lua agent loop instead of owning the loop.
    pub fn completed(&self) -> usize {
        self.completed
    }

    /// Configured round limit — read-only view, as [`completed`](Self::completed).
    pub fn limit(&self) -> usize {
        self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_uses_token_and_message_thresholds() {
        let budget = CompactionBudget {
            token_threshold: 100,
            keep_recent_messages: 12,
            prefix_messages: 1,
        };
        assert!(!budget.should_compact(80, 20, 13));
        assert!(budget.should_compact(81, 20, 13));
        assert!(budget.should_compact(80, 20, 14));
    }

    #[test]
    fn tool_budget_warns_once_then_exhausts() {
        let mut budget = ToolRoundBudget::new(5);
        assert_eq!(
            budget.complete_round(),
            ToolRoundOutcome::Continue {
                completed: 1,
                limit: 5
            }
        );
        assert_eq!(
            budget.complete_round(),
            ToolRoundOutcome::Continue {
                completed: 2,
                limit: 5
            }
        );
        assert_eq!(
            budget.complete_round(),
            ToolRoundOutcome::Continue {
                completed: 3,
                limit: 5
            }
        );
        assert_eq!(
            budget.complete_round(),
            ToolRoundOutcome::Warn {
                completed: 4,
                limit: 5
            }
        );
        assert_eq!(
            budget.complete_round(),
            ToolRoundOutcome::Exhausted {
                completed: 5,
                limit: 5
            }
        );
    }

    #[test]
    fn zero_round_budget_exhausts_on_first_tool_batch() {
        assert_eq!(
            ToolRoundBudget::new(0).complete_round(),
            ToolRoundOutcome::Exhausted {
                completed: 1,
                limit: 0
            }
        );
    }
}
