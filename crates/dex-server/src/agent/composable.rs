//! Server-side composable seams.
//!
//! `dex-agent-core::components` holds host-independent pieces
//! (catalog, trigger, summarizer, overflow, conflicts, pruning, limits).
//! This module holds the dex-application pieces that need server types
//! (`Policy`, `Session`, `Console`, `LlmConfig`), plus [`DexHarness`]: one
//! owned bundle the turn loop threads through every decision point, so
//! overriding one piece never requires forking the turn loop.
//!
//! - [`HarnessConfig`] — numeric budgets + cut window in one struct. It
//!   wraps the core shapes ([`HarnessLimits`], [`CutConfig`]) instead of
//!   copying their fields; `from_env` is the single env boundary.
//! - [`ToolExecutor`] — runs one tool call to a [`ToolOutcome`].
//! - [`PathConflictDetector`] — the path-aware [`ConflictDetector`]
//!   behind the scheduler (same-`path` collision, `bash`, `then_run`).
//! - [`TranscriptStore`] — journal append / rewrite (stateless: the session
//!   is passed per call, so the store can live on the harness).
//! - [`UsageReporter`] — per-call usage accounting.
//! - [`EventSink`] — transcript lines on every surface.
//! - [`PromptContributor`] — system-prompt sections.
//! - [`DynamicToolSource`] / [`ComposedCatalog`] — MCP / extension schema
//!   slices merged behind the default [`ToolCatalog`].
//! - [`RecoveryPolicy`] — overflow retry budget.
//!
//! Each trait ships a default that preserves current behavior, and
//! [`DexHarness`] threads the chosen impls through `DexTurnHost`,
//! `run_tool_batch`, `emergency_compact`, `compaction_gate`,
//! `compact_history`, and `prune_span`.

use std::sync::Arc;

use dex_agent_core::{
    CompactionBudget, CompactionTrigger, ConflictDetector, CutConfig, DefaultOverflowDetector,
    DefaultPruneScorer, HarnessLimits, OverflowDetector, PruneScorer, Summarizer, ToolCatalog,
};
use dex_coding_agent::{ApprovalPolicy, ResultPolicy};
use serde_json::{Map, Value};

use crate::agent::state::CancellationSource;
use crate::protocol::{ChatMessage, LlmToolCall, ToolDefinition};
use crate::session::Session;
use crate::tools::{Policy, ToolFilter, ToolOutcome};

/// Numeric budgets + cut window in one struct.
///
/// This wraps the core shapes instead of duplicating their fields:
/// [`HarnessLimits`] owns the numeric budgets, [`CutConfig`] the cut-point
/// window. `from_env` reads `DEX_MAX_TOOL_ITERATIONS` once at turn setup;
/// everything else is an explicit field so tests and embedders never touch
/// env. The keep-recent *token* count always comes from [`LlmConfig`]
/// (see [`cut_for`](Self::cut_for)), which honors `DEX_KEEP_RECENT_TOKENS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HarnessConfig {
    pub limits: HarnessLimits,
    pub cut: CutConfig,
}

impl HarnessConfig {
    pub fn from_env() -> Self {
        Self {
            limits: HarnessLimits::from_env(),
            cut: CutConfig::default(),
        }
    }

    pub fn with_limits(mut self, limits: HarnessLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_cut(mut self, cut: CutConfig) -> Self {
        self.cut = cut;
        self
    }

    pub fn with_max_tool_iterations(mut self, n: usize) -> Self {
        self.limits.max_tool_iterations = n;
        self
    }

    pub fn with_max_compaction_attempts(mut self, n: usize) -> Self {
        self.limits.max_compaction_attempts = n;
        self
    }

    pub fn with_batch_max_concurrent(mut self, n: usize) -> Self {
        self.limits.batch_max_concurrent = n.max(1);
        self
    }

    pub fn max_tool_iterations(&self) -> usize {
        self.limits.max_tool_iterations
    }

    pub fn max_compaction_attempts(&self) -> usize {
        self.limits.max_compaction_attempts
    }

    pub fn batch_max_concurrent(&self) -> usize {
        self.limits.batch_max_concurrent
    }

    pub fn keep_recent_messages(&self) -> usize {
        self.cut.min_keep_messages
    }

    pub fn min_to_summarize(&self) -> usize {
        self.cut.min_to_summarize
    }

    /// Effective cut window for a turn: message counts from this config,
    /// token count from [`LlmConfig`] (the `DEX_KEEP_RECENT_TOKENS` knob).
    pub fn cut_for(&self, config: &crate::llm::config::LlmConfig) -> CutConfig {
        CutConfig {
            keep_recent_tokens: config.keep_recent_tokens(),
            min_keep_messages: self.cut.min_keep_messages,
            min_to_summarize: self.cut.min_to_summarize,
        }
    }
}

/// Runs one tool call to an outcome. The default is the workspace-confined
/// dispatcher; override to stub tools in tests or route calls elsewhere.
///
/// Boxed-future style (not `async fn`) so the trait stays object-safe:
/// hosts hold `Arc<dyn ToolExecutor>`.
pub trait ToolExecutor: Send + Sync {
    fn execute_outcome<'a>(
        &'a self,
        name: &'a str,
        args: &'a Map<String, Value>,
        cancel: &'a (dyn CancellationSource + Send + Sync),
        policy: &'a Policy,
        filter: Option<&'a ToolFilter>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send + 'a>>;
}

/// Default executor: the real dispatcher.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultToolExecutor;

impl ToolExecutor for DefaultToolExecutor {
    fn execute_outcome<'a>(
        &'a self,
        name: &'a str,
        args: &'a Map<String, Value>,
        cancel: &'a (dyn CancellationSource + Send + Sync),
        policy: &'a Policy,
        filter: Option<&'a ToolFilter>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolOutcome> + Send + 'a>> {
        Box::pin(crate::tools::execute_outcome(
            name, args, cancel, policy, filter,
        ))
    }
}

/// Path-aware [`ConflictDetector`] behind the batch scheduler: fail closed
/// on unparseable args, serialize `bash` / `then_run`, collide on
/// normalized `path`. This is the production default; the core
/// `SerializeAll` is the stricter fail-closed alternative and
/// `NeverConflict` the escape hatch for known side-effect-free batches.
#[derive(Clone, Copy, Debug, Default)]
pub struct PathConflictDetector;

impl ConflictDetector for PathConflictDetector {
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        super::turn_loop::tools::tool_calls_conflict(calls)
    }
}

/// Journal append / rewrite behind the `persisted_cursor` invariant.
/// Stateless by design — the session is passed per call — so the store can
/// live on the harness instead of borrowing the host's session. The default
/// mirrors `persist_pending` / `rewrite_session`; override for in-memory
/// transcripts in tests.
pub trait TranscriptStore: Send + Sync {
    fn append_pending(
        &self,
        session: &mut Option<&mut Session>,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> std::io::Result<()>;
    fn rewrite(
        &self,
        session: Option<&mut Session>,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Default store: the JSONL session when present, cursor-only otherwise.
#[derive(Clone, Copy, Debug, Default)]
pub struct SessionTranscriptStore;

impl TranscriptStore for SessionTranscriptStore {
    fn append_pending(
        &self,
        session: &mut Option<&mut Session>,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> std::io::Result<()> {
        super::turn_loop::tools::persist_pending(session, messages, cursor)
    }

    fn rewrite(
        &self,
        session: Option<&mut Session>,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        super::turn_loop::tools::rewrite_session(session, messages, cursor)
    }
}

/// Per-call usage accounting (live totals, sink event, cumulative cost).
/// Override to route spend to a custom meter. Boxed-future style so the
/// trait stays object-safe: hosts hold `Arc<dyn UsageReporter>`.
pub trait UsageReporter: Send + Sync {
    fn record<'a>(
        &'a self,
        config: &'a crate::llm::config::LlmConfig,
        state: &'a mut crate::agent::state::ToolState,
        console: &'a crate::runtime::console::Console,
        usage: crate::protocol::Usage,
        gen_ms: Option<u64>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Default reporter: the shared `record_usage` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultUsageReporter;

impl UsageReporter for DefaultUsageReporter {
    fn record<'a>(
        &'a self,
        config: &'a crate::llm::config::LlmConfig,
        state: &'a mut crate::agent::state::ToolState,
        console: &'a crate::runtime::console::Console,
        usage: crate::protocol::Usage,
        gen_ms: Option<u64>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(super::turn_loop::tools::record_usage(
            config, state, console, usage, gen_ms,
        ))
    }
}

/// Transcript lines on every surface (sink when attached, stderr headless).
/// Override to fan events to a custom UI. Boxed-future style so the trait
/// stays object-safe: hosts hold `Arc<dyn EventSink>`.
pub trait EventSink: Send + Sync {
    fn system_note<'a>(
        &'a self,
        console: &'a crate::runtime::console::Console,
        note: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Default sink: the shared `system_note` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultEventSink;

impl EventSink for DefaultEventSink {
    fn system_note<'a>(
        &'a self,
        console: &'a crate::runtime::console::Console,
        note: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(super::turn_loop::tools::system_note(console, note))
    }
}

/// One system-prompt section. Chain contributors to replace the fixed
/// base + project + extensions + skills assembly without forking `prompt.rs`;
/// see `system_prompt_with_chain` for the assembly entry point.
pub trait PromptContributor: Send + Sync {
    fn append(&self, prompt: &mut String);
}

/// One static `header + body` section.
pub struct StaticSection {
    pub header: String,
    pub body: String,
}

impl StaticSection {
    pub fn new(header: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            body: body.into(),
        }
    }
}

impl PromptContributor for StaticSection {
    fn append(&self, prompt: &mut String) {
        prompt.push_str(&self.header);
        prompt.push_str(&self.body);
    }
}

/// Ordered chain; each contributor appends in registration order.
#[derive(Default)]
pub struct ChainContributor {
    parts: Vec<Arc<dyn PromptContributor>>,
}

impl ChainContributor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(mut self, part: impl PromptContributor + 'static) -> Self {
        self.parts.push(Arc::new(part));
        self
    }

    pub fn push_arc(mut self, part: Arc<dyn PromptContributor>) -> Self {
        self.parts.push(part);
        self
    }

    pub fn build(&self, mut base: String) -> String {
        for part in &self.parts {
            part.append(&mut base);
        }
        base
    }
}

/// One dynamic schema slice (MCP servers, extensions). Override to serve
/// custom toolsets; the merge stays name-sorted downstream.
pub trait DynamicToolSource: Send + Sync {
    fn tools(&self) -> Arc<[ToolDefinition]>;
}

/// Default MCP source: the background-refreshed cache.
#[derive(Clone, Copy, Debug, Default)]
pub struct McpToolSource;

impl DynamicToolSource for McpToolSource {
    fn tools(&self) -> Arc<[ToolDefinition]> {
        crate::mcp::cached_tools()
    }
}

/// Default extension source: the background-refreshed cache.
#[derive(Clone, Copy, Debug, Default)]
pub struct ExtensionToolSource;

impl DynamicToolSource for ExtensionToolSource {
    fn tools(&self) -> Arc<[ToolDefinition]> {
        crate::extensions::cached_tools()
    }
}

/// Default [`ToolCatalog`]: native schemas first (fixed order), dynamic
/// sources merged into a name-sorted tail so provider request bytes stay
/// deterministic. Byte-identical to `tools_schema()` with the default
/// sources; swap sources to serve custom toolsets without forking the
/// schema assembly.
#[derive(Default)]
pub struct ComposedCatalog {
    pub sources: Vec<Arc<dyn DynamicToolSource>>,
}

impl ComposedCatalog {
    pub fn dex_default() -> Self {
        Self {
            sources: vec![Arc::new(McpToolSource), Arc::new(ExtensionToolSource)],
        }
    }

    pub fn with_sources(sources: Vec<Arc<dyn DynamicToolSource>>) -> Self {
        Self { sources }
    }
}

impl ToolCatalog for ComposedCatalog {
    fn tool_schemas(&self) -> Vec<ToolDefinition> {
        let (native, _, _) = crate::llm::tool_descriptions::tools_schema_parts();
        let mut tail = Vec::new();
        for source in &self.sources {
            tail.extend(source.tools().iter().cloned());
        }
        dex_coding_agent::merge_tool_schemas(native, &tail, &[])
    }
}

/// Overflow retry budget: how many emergency compactions per turn.
/// Override to disable the retry or allow deeper cuts.
pub trait RecoveryPolicy: Send + Sync {
    fn max_attempts(&self) -> usize;
}

/// Default: the configured compaction-attempt budget.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultRecoveryPolicy;

impl RecoveryPolicy for DefaultRecoveryPolicy {
    fn max_attempts(&self) -> usize {
        HarnessLimits::default().max_compaction_attempts
    }
}

/// One owned bundle threading every overwritable turn-loop decision.
///
/// `DexTurnHost` holds an `Arc<DexHarness>` and every helper
/// (`compaction_gate`, `emergency_compact`, `run_tool_batch`,
/// `compact_history`, `prune_span`, `prepare_tool_result`) takes the pieces
/// it needs from it. `None` optionals select the config-derived behavior
/// (compaction budget from `LlmConfig`, deterministic summary, default
/// approval rows); providing them overrides exactly one decision.
#[derive(Clone)]
pub struct DexHarness {
    pub config: HarnessConfig,
    pub catalog: Arc<dyn ToolCatalog>,
    pub trigger: Option<Arc<dyn CompactionTrigger>>,
    pub summarizer: Option<Arc<dyn Summarizer>>,
    pub overflow: Arc<dyn OverflowDetector>,
    pub conflict: Arc<dyn ConflictDetector>,
    pub scorer: Arc<dyn PruneScorer>,
    pub result_policy: ResultPolicy,
    pub approval: Option<Arc<dyn ApprovalPolicy>>,
    pub executor: Arc<dyn ToolExecutor>,
    pub transcript: Arc<dyn TranscriptStore>,
    pub usage: Arc<dyn UsageReporter>,
    pub events: Arc<dyn EventSink>,
    pub recovery: Arc<dyn RecoveryPolicy>,
}

impl Default for DexHarness {
    fn default() -> Self {
        Self {
            config: HarnessConfig::default(),
            catalog: Arc::new(ComposedCatalog::dex_default()),
            trigger: None,
            summarizer: None,
            overflow: Arc::new(DefaultOverflowDetector),
            conflict: Arc::new(PathConflictDetector),
            scorer: Arc::new(DefaultPruneScorer::default()),
            result_policy: ResultPolicy::default(),
            approval: None,
            executor: Arc::new(DefaultToolExecutor),
            transcript: Arc::new(SessionTranscriptStore),
            usage: Arc::new(DefaultUsageReporter),
            events: Arc::new(DefaultEventSink),
            recovery: Arc::new(DefaultRecoveryPolicy),
        }
    }
}

impl DexHarness {
    /// Defaults with `DEX_MAX_TOOL_ITERATIONS` read once. Turn setup calls
    /// this when the caller passes no harness, so the env knob keeps its
    /// historic read-at-entry timing.
    pub fn from_env() -> Self {
        Self {
            config: HarnessConfig::from_env(),
            ..Self::default()
        }
    }

    pub fn with_config(mut self, config: HarnessConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_catalog(mut self, catalog: impl ToolCatalog + 'static) -> Self {
        self.catalog = Arc::new(catalog);
        self
    }

    pub fn with_catalog_arc(mut self, catalog: Arc<dyn ToolCatalog>) -> Self {
        self.catalog = catalog;
        self
    }

    /// Closure shortcut: `.with_catalog_fn(|| …)`.
    pub fn with_catalog_fn(
        self,
        f: impl Fn() -> Vec<ToolDefinition> + Send + Sync + 'static,
    ) -> Self {
        self.with_catalog(dex_agent_core::FnCatalog(f))
    }

    /// Serve the default native + dynamic assembly from custom sources.
    pub fn with_dynamic_sources(mut self, sources: Vec<Arc<dyn DynamicToolSource>>) -> Self {
        self.catalog = Arc::new(ComposedCatalog::with_sources(sources));
        self
    }

    pub fn with_trigger(mut self, trigger: impl CompactionTrigger + 'static) -> Self {
        self.trigger = Some(Arc::new(trigger));
        self
    }

    pub fn with_trigger_arc(mut self, trigger: Arc<dyn CompactionTrigger>) -> Self {
        self.trigger = Some(trigger);
        self
    }

    pub fn with_summarizer(mut self, summarizer: impl Summarizer + 'static) -> Self {
        self.summarizer = Some(Arc::new(summarizer));
        self
    }

    pub fn with_summarizer_arc(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    pub fn with_overflow(mut self, overflow: impl OverflowDetector + 'static) -> Self {
        self.overflow = Arc::new(overflow);
        self
    }

    pub fn with_overflow_arc(mut self, overflow: Arc<dyn OverflowDetector>) -> Self {
        self.overflow = overflow;
        self
    }

    /// Closure shortcut: `.with_overflow_fn(|message| …)`.
    pub fn with_overflow_fn(self, f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.with_overflow(dex_agent_core::FnOverflowDetector(f))
    }

    pub fn with_conflict(mut self, conflict: impl ConflictDetector + 'static) -> Self {
        self.conflict = Arc::new(conflict);
        self
    }

    pub fn with_conflict_arc(mut self, conflict: Arc<dyn ConflictDetector>) -> Self {
        self.conflict = conflict;
        self
    }

    /// Closure shortcut: `.with_conflict_fn(|calls| …)`.
    pub fn with_conflict_fn(
        self,
        f: impl Fn(&[LlmToolCall]) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.with_conflict(dex_agent_core::FnConflictDetector(f))
    }

    pub fn with_scorer(mut self, scorer: impl PruneScorer + 'static) -> Self {
        self.scorer = Arc::new(scorer);
        self
    }

    pub fn with_scorer_arc(mut self, scorer: Arc<dyn PruneScorer>) -> Self {
        self.scorer = scorer;
        self
    }

    pub fn with_result_policy(mut self, policy: ResultPolicy) -> Self {
        self.result_policy = policy;
        self
    }

    pub fn with_approval(mut self, approval: impl ApprovalPolicy + 'static) -> Self {
        self.approval = Some(Arc::new(approval));
        self
    }

    pub fn with_approval_arc(mut self, approval: Arc<dyn ApprovalPolicy>) -> Self {
        self.approval = Some(approval);
        self
    }

    pub fn with_executor(mut self, executor: impl ToolExecutor + 'static) -> Self {
        self.executor = Arc::new(executor);
        self
    }

    pub fn with_executor_arc(mut self, executor: Arc<dyn ToolExecutor>) -> Self {
        self.executor = executor;
        self
    }

    pub fn with_transcript(mut self, store: impl TranscriptStore + 'static) -> Self {
        self.transcript = Arc::new(store);
        self
    }

    pub fn with_transcript_arc(mut self, store: Arc<dyn TranscriptStore>) -> Self {
        self.transcript = store;
        self
    }

    pub fn with_usage(mut self, reporter: impl UsageReporter + 'static) -> Self {
        self.usage = Arc::new(reporter);
        self
    }

    pub fn with_usage_arc(mut self, reporter: Arc<dyn UsageReporter>) -> Self {
        self.usage = reporter;
        self
    }

    pub fn with_events(mut self, sink: impl EventSink + 'static) -> Self {
        self.events = Arc::new(sink);
        self
    }

    pub fn with_events_arc(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.events = sink;
        self
    }

    pub fn with_recovery(mut self, recovery: impl RecoveryPolicy + 'static) -> Self {
        self.recovery = Arc::new(recovery);
        self
    }

    pub fn with_recovery_arc(mut self, recovery: Arc<dyn RecoveryPolicy>) -> Self {
        self.recovery = recovery;
        self
    }

    pub fn tool_schemas(&self) -> Vec<ToolDefinition> {
        self.catalog.tool_schemas()
    }

    /// Proactive compaction gate: custom trigger when set, otherwise the
    /// config-derived [`CompactionBudget`] (current behavior).
    pub fn should_compact(
        &self,
        config: &crate::llm::config::LlmConfig,
        stored_tokens: u64,
        ephemeral_overhead: u64,
        message_count: usize,
    ) -> bool {
        if let Some(trigger) = &self.trigger {
            return trigger.should_compact(stored_tokens, ephemeral_overhead, message_count);
        }
        CompactionBudget {
            token_threshold: config.compaction_threshold(),
            keep_recent_messages: crate::agent::compaction::KEEP_RECENT_MESSAGES,
            prefix_messages: 1,
        }
        .should_compact(stored_tokens, ephemeral_overhead, message_count)
    }

    pub fn is_overflow(&self, message: &str) -> bool {
        self.overflow.is_overflow(message)
    }

    pub fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        self.conflict.conflicts(calls)
    }

    pub fn recovery_attempts(&self) -> usize {
        self.recovery.max_attempts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_agent_core::{NeverCompact, StaticSummarizer};

    #[test]
    fn config_defaults_match_historic_consts() {
        let cfg = HarnessConfig::default();
        assert_eq!(cfg.max_tool_iterations(), 200);
        assert_eq!(cfg.max_compaction_attempts(), 3);
        assert_eq!(cfg.batch_max_concurrent(), 10);
        assert_eq!(cfg.keep_recent_messages(), 12);
        assert_eq!(cfg.min_to_summarize(), 8);
        assert_eq!(cfg.limits, HarnessLimits::default());
        assert_eq!(cfg.cut, CutConfig::default());
    }

    #[test]
    fn config_builder_overrides_one_field() {
        let cfg = HarnessConfig::default().with_batch_max_concurrent(2);
        assert_eq!(cfg.batch_max_concurrent(), 2);
        assert_eq!(cfg.max_tool_iterations(), 200);
    }

    #[test]
    fn cut_for_takes_tokens_from_llm_config() {
        let config = crate::llm::config::tests::test_cfg();
        let cut = HarnessConfig::default().cut_for(&config);
        assert_eq!(cut.keep_recent_tokens, config.keep_recent_tokens());
        assert_eq!(cut.min_keep_messages, 12);
    }

    #[test]
    fn harness_overrides_flow_through() {
        let harness = DexHarness::default()
            .with_trigger(NeverCompact)
            .with_summarizer(StaticSummarizer::new("fixed"))
            .with_result_policy(ResultPolicy::default().without_repeat_guard());
        let config = crate::llm::config::tests::test_cfg();
        assert!(!harness.should_compact(&config, u64::MAX, u64::MAX, usize::MAX));
        assert_eq!(harness.result_policy.repeat_limit, usize::MAX);
        // Defaults still fire: overflow wording and path conflicts.
        assert!(harness.is_overflow("maximum context length exceeded"));
        assert!(!harness.is_overflow("invalid api key"));
    }

    #[test]
    fn composed_catalog_matches_tools_schema() {
        let names = |tools: Vec<ToolDefinition>| {
            tools
                .into_iter()
                .map(|t| t.function.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(DexHarness::default().tool_schemas()),
            names(crate::llm::tool_descriptions::tools_schema())
        );
    }
}
