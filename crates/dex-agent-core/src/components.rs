//! Overwritable harness pieces.
//!
//! `AgentHost` is the full turn-loop seam, but implementing all eight
//! methods just to tweak one behavior is heavy. These traits split the
//! most commonly customized decisions into small, single-purpose units:
//!
//! - [`ToolCatalog`] — which schemas the model sees.
//! - [`CompactionTrigger`] — when history should be compacted.
//! - [`Summarizer`] — how compacted history is summarized.
//! - [`OverflowDetector`] — when a model error means "context too big".
//! - [`ConflictDetector`] — when a tool batch must run serialized.
//! - [`PruneScorer`] — which compacted tool pairs survive verbatim.
//! - [`HarnessLimits`] — numeric budgets (tool rounds, compaction attempts,
//!   batch fan-out) in one struct instead of scattered env reads.
//!
//! Each trait ships with closure adapters (`FnCatalog`, `FnTrigger`,
//! `FnSummarizer`), static test doubles (`StaticCatalog`,
//! `NeverCompact`, `AlwaysCompact`, `StaticSummarizer`), and a
//! [`Harness`] builder that bundles the three plus the tool-round limit.
//! Overriding one piece is one `with_*` call:
//!
//! ```rust
//! use dex_agent_core::{CompactionBudget, Harness};
//!
//! let harness = Harness::default()
//!     .with_tool_round_limit(32)
//!     .with_compaction_budget(CompactionBudget {
//!         token_threshold: 50_000,
//!         keep_recent_messages: 12,
//!         prefix_messages: 1,
//!     })
//!     .with_summarizer_fn(|old, _prefix, _prev, _ops| {
//!         format!("custom summary over {} messages", old.len())
//!     });
//! ```

use std::sync::Arc;

use dex_ai::{ChatMessage, LlmToolCall, ToolDefinition};

use crate::{deterministic_summary, CompactionBudget, FileOps};

/// Which tool schemas the model sees on the next request.
///
/// Implement this to add, remove, or rewrite tools without touching the
/// turn engine. See `dex-coding-agent::ToolRegistry` for a ready-made
/// catalog with `register` / `override_tool` / `remove` helpers.
pub trait ToolCatalog: Send + Sync {
    fn tool_schemas(&self) -> Vec<ToolDefinition>;
}

impl ToolCatalog for () {
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }
}

impl ToolCatalog for Vec<ToolDefinition> {
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        self.clone()
    }
}

/// Fixed schema list. Clone to share across turns.
#[derive(Clone, Default)]
pub struct StaticCatalog(pub Vec<ToolDefinition>);

impl StaticCatalog {
    pub fn new(tools: Vec<ToolDefinition>) -> Self {
        Self(tools)
    }
}

impl ToolCatalog for StaticCatalog {
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        self.0.clone()
    }
}

/// Closure catalog for one-off overrides and tests.
pub struct FnCatalog<F>(pub F)
where
    F: Fn() -> Vec<ToolDefinition> + Send + Sync;

impl<F> ToolCatalog for FnCatalog<F>
where
    F: Fn() -> Vec<ToolDefinition> + Send + Sync,
{
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        (self.0)()
    }
}

/// When stored history should be compacted before a model call.
pub trait CompactionTrigger: Send + Sync {
    fn should_compact(
        &self,
        stored_tokens: u64,
        ephemeral_overhead: u64,
        message_count: usize,
    ) -> bool;
}

impl CompactionTrigger for CompactionBudget {
    fn should_compact(
        &self,
        stored_tokens: u64,
        ephemeral_overhead: u64,
        message_count: usize,
    ) -> bool {
        (*self).should_compact(stored_tokens, ephemeral_overhead, message_count)
    }
}

/// Never compact. Useful for tests and for hosts that manage history
/// themselves.
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCompact;

impl CompactionTrigger for NeverCompact {
    fn should_compact(&self, _stored: u64, _overhead: u64, _count: usize) -> bool {
        false
    }
}

/// Always compact. Useful for exercising the summarizer path in tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlwaysCompact;

impl CompactionTrigger for AlwaysCompact {
    fn should_compact(&self, _stored: u64, _overhead: u64, _count: usize) -> bool {
        true
    }
}

/// Closure trigger for one-off thresholds and tests.
pub struct FnTrigger<F>(pub F)
where
    F: Fn(u64, u64, usize) -> bool + Send + Sync;

impl<F> CompactionTrigger for FnTrigger<F>
where
    F: Fn(u64, u64, usize) -> bool + Send + Sync,
{
    fn should_compact(&self, stored: u64, overhead: u64, count: usize) -> bool {
        (self.0)(stored, overhead, count)
    }
}

/// How compacted history is summarized.
///
/// The default ([`DeterministicSummarizer`]) is the offline structured
/// checkpoint used when no LLM summarizer is configured. Replace it to
/// plug in an LLM, verbatim-prune, or domain-specific summary.
pub trait Summarizer: Send + Sync {
    fn summarize(
        &self,
        old: &[ChatMessage],
        turn_prefix: &[ChatMessage],
        previous_summary: Option<&str>,
        file_ops: &FileOps,
    ) -> String;
}

/// Offline structured checkpoint (goal / progress / decisions / files).
#[derive(Clone, Copy, Debug, Default)]
pub struct DeterministicSummarizer;

impl Summarizer for DeterministicSummarizer {
    fn summarize(
        &self,
        old: &[ChatMessage],
        turn_prefix: &[ChatMessage],
        previous_summary: Option<&str>,
        file_ops: &FileOps,
    ) -> String {
        deterministic_summary(old, turn_prefix, previous_summary, file_ops)
    }
}

/// Fixed string, regardless of input. Useful for tests.
#[derive(Clone, Debug)]
pub struct StaticSummarizer(pub String);

impl StaticSummarizer {
    pub fn new(summary: impl Into<String>) -> Self {
        Self(summary.into())
    }
}

impl Summarizer for StaticSummarizer {
    fn summarize(
        &self,
        _old: &[ChatMessage],
        _prefix: &[ChatMessage],
        _previous: Option<&str>,
        _ops: &FileOps,
    ) -> String {
        self.0.clone()
    }
}

/// Closure summarizer for one-off overrides and tests.
pub struct FnSummarizer<F>(pub F)
where
    F: Fn(&[ChatMessage], &[ChatMessage], Option<&str>, &FileOps) -> String + Send + Sync;

impl<F> Summarizer for FnSummarizer<F>
where
    F: Fn(&[ChatMessage], &[ChatMessage], Option<&str>, &FileOps) -> String + Send + Sync,
{
    fn summarize(
        &self,
        old: &[ChatMessage],
        prefix: &[ChatMessage],
        previous: Option<&str>,
        ops: &FileOps,
    ) -> String {
        (self.0)(old, prefix, previous, ops)
    }
}

/// Bundled, overwritable harness decisions.
///
/// Hosts can hold a `Harness` and delegate their `AgentHost` methods to
/// it, so users override behavior by swapping one field instead of
/// reimplementing the host.
pub struct Harness {
    catalog: Arc<dyn ToolCatalog>,
    trigger: Arc<dyn CompactionTrigger>,
    summarizer: Arc<dyn Summarizer>,
    tool_round_limit: usize,
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            catalog: Arc::new(()),
            trigger: Arc::new(CompactionBudget {
                token_threshold: 180_000,
                keep_recent_messages: 12,
                prefix_messages: 1,
            }),
            summarizer: Arc::new(DeterministicSummarizer),
            tool_round_limit: 200,
        }
    }
}

impl Harness {
    pub fn new(
        catalog: Arc<dyn ToolCatalog>,
        trigger: Arc<dyn CompactionTrigger>,
        summarizer: Arc<dyn Summarizer>,
        tool_round_limit: usize,
    ) -> Self {
        Self {
            catalog,
            trigger,
            summarizer,
            tool_round_limit,
        }
    }

    pub fn with_catalog(mut self, catalog: impl ToolCatalog + 'static) -> Self {
        self.catalog = Arc::new(catalog);
        self
    }

    pub fn with_catalog_arc(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalog = catalog;
        self
    }

    pub fn with_trigger(mut self, trigger: impl CompactionTrigger + 'static) -> Self {
        self.trigger = Arc::new(trigger);
        self
    }

    pub fn with_trigger_arc(mut self, trigger: Arc<dyn CompactionTrigger>) -> Self {
        self.trigger = trigger;
        self
    }

    /// Shortcut for the common case: replace the trigger with a budget.
    pub fn with_compaction_budget(self, budget: CompactionBudget) -> Self {
        self.with_trigger(budget)
    }

    /// Shortcut for disabling the proactive trigger.
    pub fn without_compaction(self) -> Self {
        self.with_trigger(NeverCompact)
    }

    pub fn with_summarizer(mut self, summarizer: impl Summarizer + 'static) -> Self {
        self.summarizer = Arc::new(summarizer);
        self
    }

    pub fn with_summarizer_arc(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = summarizer;
        self
    }

    /// Closure shortcut: `.with_summarizer_fn(|old, prefix, prev, ops| …)`.
    pub fn with_summarizer_fn(
        self,
        f: impl Fn(&[ChatMessage], &[ChatMessage], Option<&str>, &FileOps) -> String
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.with_summarizer(FnSummarizer(f))
    }

    /// Closure shortcut: `.with_catalog_fn(|| …)`.
    pub fn with_catalog_fn(
        self,
        f: impl Fn() -> Vec<ToolDefinition> + Send + Sync + 'static,
    ) -> Self {
        self.with_catalog(FnCatalog(f))
    }

    /// Closure shortcut: `.with_trigger_fn(|stored, overhead, count| …)`.
    pub fn with_trigger_fn(
        self,
        f: impl Fn(u64, u64, usize) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.with_trigger(FnTrigger(f))
    }

    pub fn with_tool_round_limit(mut self, limit: usize) -> Self {
        self.tool_round_limit = limit;
        self
    }

    pub fn tool_schemas(&self) -> Vec<ToolDefinition> {
        self.catalog.tool_schemas()
    }

    pub fn should_compact(&self, stored: u64, overhead: u64, count: usize) -> bool {
        self.trigger.should_compact(stored, overhead, count)
    }

    pub fn summarize(
        &self,
        old: &[ChatMessage],
        prefix: &[ChatMessage],
        previous: Option<&str>,
        ops: &FileOps,
    ) -> String {
        self.summarizer.summarize(old, prefix, previous, ops)
    }

    pub fn tool_round_limit(&self) -> usize {
        self.tool_round_limit
    }
}

/// Detects provider "context too big" errors.
///
/// The default matches the wording list previously inline in the turn loop;
/// override to add provider-specific phrases without forking the host.
pub trait OverflowDetector: Send + Sync {
    fn is_overflow(&self, message: &str) -> bool;
}

/// Substring list behind [`DefaultOverflowDetector`].
pub const OVERFLOW_PHRASES: &[&str] = &[
    "context length",
    "context_length",
    "maximum context",
    "context window",
    "context size",
    "context too large",
    "input length",
    "input is too long",
    "prompt is too long",
    "prompt too long",
    "too many tokens",
    "token limit",
];

/// Default wording match (case-insensitive) with the anchored
/// "reduce the length" rule.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultOverflowDetector;

impl OverflowDetector for DefaultOverflowDetector {
    fn is_overflow(&self, message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        OVERFLOW_PHRASES.iter().any(|p| message.contains(*p))
            || (message.contains("reduce the length")
                && (message.contains("context")
                    || message.contains("token")
                    || message.contains("prompt")
                    || message.contains("input")))
    }
}

/// Closure overflow detector.
pub struct FnOverflowDetector<F>(pub F)
where
    F: Fn(&str) -> bool + Send + Sync;

impl<F> OverflowDetector for FnOverflowDetector<F>
where
    F: Fn(&str) -> bool + Send + Sync,
{
    fn is_overflow(&self, message: &str) -> bool {
        (self.0)(message)
    }
}

/// Decides whether a tool batch must run serialized under the mutation
/// lock instead of fanning out.
///
/// The default (`SerializeAll`) is fail-closed and always serializes; hosts
/// with path-level conflict knowledge (same-file writes, `bash`,
/// `then_run`) provide a finer detector.
pub trait ConflictDetector: Send + Sync {
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool;
}

/// Fail-closed default: serialize every multi-call batch.
#[derive(Clone, Copy, Debug, Default)]
pub struct SerializeAll;

impl ConflictDetector for SerializeAll {
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        calls.len() > 1
    }
}

/// Never serialize: always fan out (only for tools known to be side-effect
/// free and independent).
#[derive(Clone, Copy, Debug, Default)]
pub struct NeverConflict;

impl ConflictDetector for NeverConflict {
    fn conflicts(&self, _calls: &[LlmToolCall]) -> bool {
        false
    }
}

/// Closure conflict detector.
pub struct FnConflictDetector<F>(pub F)
where
    F: Fn(&[LlmToolCall]) -> bool + Send + Sync;

impl<F> ConflictDetector for FnConflictDetector<F>
where
    F: Fn(&[LlmToolCall]) -> bool + Send + Sync,
{
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        (self.0)(calls)
    }
}

/// Verdict for one tool_use → tool_result pair during verbatim pruning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairVerdict {
    pub keep_call: bool,
    pub keep_result: bool,
}

/// Scores one compacted pair without touching the transcript, so the rule
/// is unit-testable and replaceable without forking the prune walk.
pub trait PruneScorer: Send + Sync {
    fn decide(
        &self,
        tool: &str,
        result_chars: usize,
        rerunnable: bool,
        is_error: bool,
    ) -> PairVerdict;
}

/// Size thresholds behind [`DefaultPruneScorer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneThresholds {
    pub keep_below_chars: usize,
    pub truncate_above_chars: usize,
    pub drop_above_chars: usize,
}

impl Default for PruneThresholds {
    fn default() -> Self {
        Self {
            keep_below_chars: 500,
            truncate_above_chars: 2_000,
            drop_above_chars: 10_000,
        }
    }
}

/// Heuristic stand-in for the two live questions (call still matters?
/// result still needed verbatim?).
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultPruneScorer {
    pub thresholds: PruneThresholds,
}

impl PruneScorer for DefaultPruneScorer {
    fn decide(
        &self,
        _tool: &str,
        result_chars: usize,
        rerunnable: bool,
        is_error: bool,
    ) -> PairVerdict {
        if result_chars < self.thresholds.keep_below_chars {
            return PairVerdict {
                keep_call: true,
                keep_result: true,
            };
        }
        if result_chars > self.thresholds.drop_above_chars {
            if rerunnable && !is_error {
                return PairVerdict {
                    keep_call: false,
                    keep_result: false,
                };
            }
            return PairVerdict {
                keep_call: true,
                keep_result: false,
            };
        }
        if result_chars > self.thresholds.truncate_above_chars {
            return PairVerdict {
                keep_call: true,
                keep_result: false,
            };
        }
        PairVerdict {
            keep_call: true,
            keep_result: true,
        }
    }
}

/// Closure prune scorer.
pub struct FnPruneScorer<F>(pub F)
where
    F: Fn(&str, usize, bool, bool) -> PairVerdict + Send + Sync;

impl<F> PruneScorer for FnPruneScorer<F>
where
    F: Fn(&str, usize, bool, bool) -> PairVerdict + Send + Sync,
{
    fn decide(
        &self,
        tool: &str,
        result_chars: usize,
        rerunnable: bool,
        is_error: bool,
    ) -> PairVerdict {
        (self.0)(tool, result_chars, rerunnable, is_error)
    }
}

/// Cut-point window for compaction: token-based keep-recent plus a
/// message-count fallback. Replaces the scattered `KEEP_RECENT_*` consts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutConfig {
    pub keep_recent_tokens: u64,
    pub min_keep_messages: usize,
    pub min_to_summarize: usize,
}

impl Default for CutConfig {
    fn default() -> Self {
        Self {
            keep_recent_tokens: 20_000,
            min_keep_messages: 12,
            min_to_summarize: 8,
        }
    }
}

impl CutConfig {
    pub fn emergency(&self) -> Self {
        Self {
            keep_recent_tokens: self.keep_recent_tokens.saturating_div(4),
            min_keep_messages: 4,
            min_to_summarize: 1,
        }
    }
}

/// Numeric budgets in one place. `from_env` is the single env boundary;
/// library paths take `HarnessLimits` by value instead of reading env.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessLimits {
    pub max_tool_iterations: usize,
    pub max_compaction_attempts: usize,
    pub batch_max_concurrent: usize,
    pub min_reduction_ratio: u32,
}

impl Default for HarnessLimits {
    fn default() -> Self {
        Self {
            max_tool_iterations: 200,
            max_compaction_attempts: 3,
            batch_max_concurrent: 10,
            min_reduction_ratio: 25,
        }
    }
}

impl HarnessLimits {
    /// Read `DEX_MAX_TOOL_ITERATIONS`; every other limit stays default.
    /// Hosts call this once at turn setup and pass the value down.
    pub fn from_env() -> Self {
        let max_tool_iterations = std::env::var("DEX_MAX_TOOL_ITERATIONS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(200);
        Self {
            max_tool_iterations,
            ..Self::default()
        }
    }

    pub fn reduction_ratio(&self) -> f64 {
        f64::from(self.min_reduction_ratio) / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_ai::FunctionDef;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn catalog_override_replaces_schemas() {
        let harness = Harness::default().with_catalog(StaticCatalog::new(vec![tool("custom")]));
        assert_eq!(harness.tool_schemas().len(), 1);
        assert_eq!(harness.tool_schemas()[0].function.name, "custom");
    }

    #[test]
    fn trigger_override_replaces_budget() {
        let harness = Harness::default().without_compaction();
        assert!(!harness.should_compact(u64::MAX, u64::MAX, usize::MAX));

        let harness = Harness::default().with_trigger(AlwaysCompact);
        assert!(harness.should_compact(0, 0, 0));
    }

    #[test]
    fn summarizer_override_replaces_default() {
        let harness = Harness::default().with_summarizer(StaticSummarizer::new("fixed checkpoint"));
        let ops = FileOps::default();
        assert_eq!(harness.summarize(&[], &[], None, &ops), "fixed checkpoint");

        let harness =
            Harness::default().with_summarizer_fn(|old, _, _, _| format!("{} messages", old.len()));
        let old = vec![ChatMessage::user("a"), ChatMessage::user("b")];
        assert_eq!(harness.summarize(&old, &[], None, &ops), "2 messages");
    }
}
