//! Server-side composable seams.
//!
//! `dex-agent-core::components` holds host-independent pieces
//! (catalog, trigger, summarizer, overflow, conflicts, pruning, limits).
//! This module holds the dex-application pieces that need server types
//! (`Policy`, `Session`, `Console`, `LlmConfig`):
//!
//! - [`HarnessConfig`] — numeric budgets in one struct (`from_env` is the
//!   single env boundary; library paths take the struct by value).
//! - [`ToolExecutor`] — runs one tool call to a [`ToolOutcome`].
//! - [`BatchConflictPolicy`] — when a batch must serialize.
//! - [`TranscriptStore`] — journal append / rewrite.
//! - [`UsageReporter`] — per-call usage accounting.
//! - [`EventSink`] — transcript lines on every surface.
//! - [`PromptContributor`] — system-prompt sections.
//! - [`DynamicToolSource`] — MCP / extension schema slices.
//! - [`RecoveryPolicy`] — overflow retry budget.
//!
//! Each trait ships a default that preserves current behavior, so
//! overriding one piece never requires forking the turn loop.

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::agent::state::CancellationSource;
use crate::protocol::{ChatMessage, LlmToolCall, ToolDefinition};
use crate::session::Session;
use crate::tools::{Policy, ToolFilter, ToolOutcome};

/// Numeric budgets in one struct. `from_env` reads `DEX_MAX_TOOL_ITERATIONS`
/// once at turn setup; everything else is an explicit field so tests and
/// embedders never touch env.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    pub max_tool_iterations: usize,
    pub max_compaction_attempts: usize,
    pub batch_max_concurrent: usize,
    pub keep_recent_messages: usize,
    pub min_to_summarize: usize,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            max_tool_iterations: 200,
            max_compaction_attempts: 3,
            batch_max_concurrent: 10,
            keep_recent_messages: 12,
            min_to_summarize: 8,
        }
    }
}

impl HarnessConfig {
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

    pub fn with_max_tool_iterations(mut self, n: usize) -> Self {
        self.max_tool_iterations = n;
        self
    }

    pub fn with_max_compaction_attempts(mut self, n: usize) -> Self {
        self.max_compaction_attempts = n;
        self
    }

    pub fn with_batch_max_concurrent(mut self, n: usize) -> Self {
        self.batch_max_concurrent = n.max(1);
        self
    }
}

/// Runs one tool call to an outcome. The default is the workspace-confined
/// dispatcher; override to stub tools in tests or route calls elsewhere.
#[allow(async_fn_in_trait)]
pub trait ToolExecutor: Send + Sync {
    async fn execute_outcome(
        &self,
        name: &str,
        args: &Map<String, Value>,
        cancel: &(dyn CancellationSource + Send + Sync),
        policy: &Policy,
        filter: Option<&ToolFilter>,
    ) -> ToolOutcome;
}

/// Default executor: the real dispatcher.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultToolExecutor;

impl ToolExecutor for DefaultToolExecutor {
    async fn execute_outcome(
        &self,
        name: &str,
        args: &Map<String, Value>,
        cancel: &(dyn CancellationSource + Send + Sync),
        policy: &Policy,
        filter: Option<&ToolFilter>,
    ) -> ToolOutcome {
        crate::tools::execute_outcome(name, args, cancel, policy, filter).await
    }
}

/// When a batch must serialize under the mutation lock. The default mirrors
/// the historic rule (same-`path` collision, `bash`, `then_run`);
/// override for custom serialization without touching the scheduler.
pub trait BatchConflictPolicy: Send + Sync {
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool;
}

/// Default: fail closed on unparseable args, serialize `bash`/`then_run`,
/// collide on normalized `path`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultBatchConflictPolicy;

impl BatchConflictPolicy for DefaultBatchConflictPolicy {
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        super::turn_loop::tools::tool_calls_conflict(calls)
    }
}

/// Closure conflict policy.
pub struct FnBatchConflictPolicy<F>(pub F)
where
    F: Fn(&[LlmToolCall]) -> bool + Send + Sync;

impl<F> BatchConflictPolicy for FnBatchConflictPolicy<F>
where
    F: Fn(&[LlmToolCall]) -> bool + Send + Sync,
{
    fn conflicts(&self, calls: &[LlmToolCall]) -> bool {
        (self.0)(calls)
    }
}

/// Journal append / rewrite behind the `persisted_cursor` invariant.
/// The default mirrors `persist_pending` / `rewrite_session`; override for
/// in-memory transcripts in tests.
pub trait TranscriptStore: Send {
    fn append_pending(
        &mut self,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> std::io::Result<()>;
    fn rewrite(
        &mut self,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// Default store: the JSONL session when present, cursor-only otherwise.
pub struct SessionTranscriptStore<'a> {
    pub session: Option<&'a mut Session>,
}

impl TranscriptStore for SessionTranscriptStore<'_> {
    fn append_pending(
        &mut self,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> std::io::Result<()> {
        super::turn_loop::tools::persist_pending(&mut self.session, messages, cursor)
    }

    fn rewrite(
        &mut self,
        messages: &[ChatMessage],
        cursor: &mut usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        super::turn_loop::tools::rewrite_session(self.session.as_deref_mut(), messages, cursor)
    }
}

/// Per-call usage accounting (live totals, sink event, cumulative cost).
/// Override to route spend to a custom meter.
#[allow(async_fn_in_trait)]
pub trait UsageReporter: Send + Sync {
    async fn record(
        &self,
        config: &crate::llm::config::LlmConfig,
        state: &mut crate::agent::state::ToolState,
        console: &crate::runtime::console::Console,
        usage: crate::protocol::Usage,
        gen_ms: Option<u64>,
    );
}

/// Default reporter: the shared `record_usage` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultUsageReporter;

impl UsageReporter for DefaultUsageReporter {
    async fn record(
        &self,
        config: &crate::llm::config::LlmConfig,
        state: &mut crate::agent::state::ToolState,
        console: &crate::runtime::console::Console,
        usage: crate::protocol::Usage,
        gen_ms: Option<u64>,
    ) {
        super::turn_loop::tools::record_usage(config, state, console, usage, gen_ms).await;
    }
}

/// Transcript lines on every surface (sink when attached, stderr headless).
/// Override to fan events to a custom UI.
#[allow(async_fn_in_trait)]
pub trait EventSink: Send + Sync {
    async fn system_note(&self, console: &crate::runtime::console::Console, note: &str);
}

/// Default sink: the shared `system_note` helper.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultEventSink;

impl EventSink for DefaultEventSink {
    async fn system_note(&self, console: &crate::runtime::console::Console, note: &str) {
        super::turn_loop::tools::system_note(console, note).await;
    }
}

/// One system-prompt section. Chain contributors to replace the fixed
/// base + project + extensions + skills assembly without forking `prompt.rs`.
pub trait PromptContributor: Send + Sync {
    fn append(&self, prompt: &mut String);
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

/// Overflow retry budget: how many emergency compactions per turn.
/// Override to disable the retry or allow deeper cuts.
pub trait RecoveryPolicy: Send + Sync {
    fn max_attempts(&self) -> usize;
}

/// Default: three attempts, matching the historic loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultRecoveryPolicy;

impl RecoveryPolicy for DefaultRecoveryPolicy {
    fn max_attempts(&self) -> usize {
        HarnessConfig::default().max_compaction_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_historic_consts() {
        let cfg = HarnessConfig::default();
        assert_eq!(cfg.max_tool_iterations, 200);
        assert_eq!(cfg.max_compaction_attempts, 3);
        assert_eq!(cfg.batch_max_concurrent, 10);
        assert_eq!(cfg.keep_recent_messages, 12);
    }

    #[test]
    fn config_builder_overrides_one_field() {
        let cfg = HarnessConfig::default().with_batch_max_concurrent(2);
        assert_eq!(cfg.batch_max_concurrent, 2);
        assert_eq!(cfg.max_tool_iterations, 200);
    }
}
