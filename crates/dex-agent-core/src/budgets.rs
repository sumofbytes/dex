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

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn budget(threshold: u64, keep: usize, prefix: usize) -> CompactionBudget {
        CompactionBudget {
            token_threshold: threshold,
            keep_recent_messages: keep,
            prefix_messages: prefix,
        }
    }

    proptest! {
        /// Raising any input can never un-trigger compaction.
        #[test]
        fn should_compact_is_monotone(
            threshold in 0u64..=10_000,
            keep in 0usize..=200,
            prefix in 0usize..=100,
            tokens in 0u64..=10_000,
            overhead in 0u64..=10_000,
            count in 0usize..=400,
            d_tokens in 0u64..=10_000,
            d_overhead in 0u64..=10_000,
            d_count in 0usize..=400,
        ) {
            let b = budget(threshold, keep, prefix);
            let lower = b.should_compact(tokens, overhead, count);
            let higher = b.should_compact(
                tokens + d_tokens,
                overhead + d_overhead,
                count + d_count,
            );
            prop_assert!(higher || !lower);
        }

        /// Exact token boundary: triggers one token past the threshold,
        /// never at it.
        #[test]
        fn should_compact_token_boundary(
            threshold in 0u64..=10_000,
            tokens in 0u64..=10_000,
        ) {
            prop_assert_eq!(
                budget(threshold, 0, 0).should_compact(tokens, 0, 0),
                tokens > threshold
            );
        }

        /// Exact message boundary against the count-based fallback.
        #[test]
        fn should_compact_message_boundary(
            threshold in 1u64..=10_000,
            keep in 0usize..=200,
            prefix in 0usize..=100,
            count in 0usize..=400,
        ) {
            prop_assert_eq!(
                budget(threshold, keep, prefix).should_compact(0, 0, count),
                count > prefix + keep
            );
        }

        /// ToolRoundBudget invariants over any run: completed tracks the
        /// round count, at most one warning fires (only below the limit),
        /// and exhaustion is terminal.
        #[test]
        fn tool_round_budget_invariants(limit in 0usize..=60, rounds in 1usize..=120) {
            let mut budget = ToolRoundBudget::new(limit);
            let mut warned = 0usize;
            let mut exhausted = false;
            for round in 1..=rounds {
                match budget.complete_round() {
                    ToolRoundOutcome::Continue { completed, .. } => {
                        prop_assert!(!exhausted);
                        prop_assert_eq!(completed, round);
                        prop_assert!(completed < limit);
                        prop_assert!(completed * 5 < limit * 4 || warned == 1);
                    }
                    ToolRoundOutcome::Warn { completed, .. } => {
                        prop_assert!(!exhausted);
                        prop_assert_eq!(completed, round);
                        warned += 1;
                        prop_assert_eq!(warned, 1);
                        prop_assert!(completed < limit);
                        prop_assert!(completed * 5 >= limit * 4);
                    }
                    ToolRoundOutcome::Exhausted { completed, .. } => {
                        prop_assert_eq!(completed, round);
                        prop_assert!(completed >= limit);
                        exhausted = true;
                    }
                }
                prop_assert_eq!(budget.completed(), round);
            }
            prop_assert!(exhausted || rounds < limit.max(1));
        }
    }
}
