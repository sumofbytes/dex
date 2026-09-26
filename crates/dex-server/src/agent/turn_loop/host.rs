//! Dex application adapters for the reusable dex-agent-core turn engine.
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::tool_results::prepare_tool_result;
use super::tools::{
    emergency_compact, inject_steering, is_context_overflow, note_sink, persist_pending,
    record_usage, rewrite_session, run_tool_batch, system_note,
};
use crate::agent::compaction::verbatim::summary_mode;
use crate::agent::compaction::{compact_history, KEEP_RECENT_MESSAGES};
use crate::agent::state::{CancellationSource, ToolState};
use crate::agent::tokens::{estimate_ephemeral_tokens, schema_budget_tokens, TokenLedger};
use crate::llm::config::LlmConfig;
use crate::protocol::{ChatMessage, ModelEvent, QueueMsg, SinkLine, StopReason};
use crate::render::format::{
    model_tool_result, tool_preview, tool_preview_body, tool_result_summary,
};
use crate::runtime::console::{Console, RESET, TOOL_OUTPUT_COLOR};
use crate::session::changes::{track_end, track_start, TrackedCall};
use crate::session::Session;
use crate::tools::{Policy, ToolFilter};
use dex_agent_core::{
    tool_budget_exhausted_note, AgentHost, AgentTurnError, CompactionBudget, ToolRoundOutcome,
};

async fn forward_model_events(
    mut events: mpsc::Receiver<ModelEvent>,
    sink: mpsc::Sender<SinkLine>,
) {
    while let Some(event) = events.recv().await {
        let line = match event {
            ModelEvent::Assistant(text) => SinkLine::Assistant(text),
            ModelEvent::Thinking(text) => SinkLine::Thinking(text),
            ModelEvent::System(text) => SinkLine::System(text),
        };
        if sink.send(line).await.is_err() {
            break;
        }
    }
}

/// Proactive compaction gate, run before every model call: compact while
/// the stored context exceeds the token threshold or the message-count cap,
/// at most three attempts.
/// The budget is re-derived from the ledger after every cut: re-checking a
/// stale pre-cut number forces up to three compactions even when the first
/// already fit. The gate reads the stored history, not a projection: it
/// guards the window AND the journal, so the stored history can't grow
/// unbounded while the gate defers.
#[allow(clippy::too_many_arguments)]
async fn compaction_gate(
    config: &LlmConfig,
    console: &Console,
    messages: &mut Vec<ChatMessage>,
    // Ephemeral + schema overhead for this iteration (call-time preamble +
    // tool schemas, never stored): the gate adds the ledger's stored total
    // fresh each attempt.
    budget_overhead: u64,
    cancel: &(dyn CancellationSource + Send + Sync),
    mut session: Option<&mut Session>,
    persisted_cursor: &mut usize,
    state: &mut ToolState,
    ledger: &mut TokenLedger,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut compaction_attempts = 0;
    let max_attempts = crate::agent::composable::HarnessConfig::default().max_compaction_attempts;
    while compaction_attempts < max_attempts {
        if !(CompactionBudget {
            token_threshold: config.compaction_threshold(),
            keep_recent_messages: KEEP_RECENT_MESSAGES,
            prefix_messages: 1,
        })
        .should_compact(ledger.stored_tokens(), budget_overhead, messages.len())
        {
            break;
        }
        // Threshold cuts follow the threshold knob (`DEX_COMPACTION`):
        // one parse selects both the prune and the fallback summarizer.
        let summarizer = summary_mode();
        match compact_history(
            config,
            messages,
            cancel,
            false,
            summarizer.prunes_jev(),
            summarizer,
        )
        .await
        {
            Ok((true, compacted)) => {
                compaction_attempts += 1;
                // History was rewritten: re-measure once for the next attempt.
                *ledger = TokenLedger::rebuild(messages);
                // Summarizer calls are billed like any other; account
                // them so the status-bar spend includes compaction.
                if let Some(u) = compacted {
                    record_usage(config, state, console, u, None).await;
                }
                rewrite_session(session.as_deref_mut(), messages, persisted_cursor)?;
                continue;
            }
            Ok((false, _)) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(super) struct DexTurnHost<'a, X> {
    pub(super) config: &'a LlmConfig,
    pub(super) state: &'a mut ToolState,
    pub(super) steering_rx: Option<&'a mut mpsc::Receiver<QueueMsg>>,
    pub(super) steering_accepted_tx: Option<&'a mpsc::Sender<String>>,
    pub(super) session: Option<&'a mut Session>,
    pub(super) cancel: &'a X,
    pub(super) console: &'a Console,
    pub(super) filter: Option<&'a ToolFilter>,
    pub(super) policy: Policy,
    pub(super) last_tools: Vec<String>,
    pub(super) last_usage: Option<u64>,
    pub(super) persisted_cursor: usize,
    event_forwarder: Option<JoinHandle<()>>,
}

impl<X: CancellationSource + Clone + Send + Sync + 'static> AgentHost for DexTurnHost<'_, X> {
    async fn before_model(
        &mut self,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError> {
        persist_pending(&mut self.session, messages, &mut self.persisted_cursor)?;
        if self.cancel.is_cancelled() {
            let _ = self.cancel.take_cancelled();
            return Err("cancelled by user".into());
        }
        if let Some(rx) = self.steering_rx.as_mut() {
            if inject_steering(rx, self.steering_accepted_tx, messages).await {
                *ledger = TokenLedger::rebuild(messages);
                persist_pending(&mut self.session, messages, &mut self.persisted_cursor)?;
            }
        }

        let ephemerals = [crate::mcp::ephemeral_line()];
        let overhead = estimate_ephemeral_tokens(&ephemerals) + schema_budget_tokens();
        compaction_gate(
            self.config,
            self.console,
            messages,
            overhead,
            self.cancel,
            self.session.as_deref_mut(),
            &mut self.persisted_cursor,
            self.state,
            ledger,
        )
        .await?;
        Ok(())
    }

    fn tool_schemas(&self) -> Vec<dex_ai::ToolDefinition> {
        crate::llm::tool_descriptions::tools_schema()
    }

    fn start_model_events(&mut self) -> Option<mpsc::Sender<ModelEvent>> {
        let sink = self.console.sink().cloned()?;
        let (sender, receiver) = mpsc::channel(256);
        self.event_forwarder = Some(tokio::spawn(forward_model_events(receiver, sink)));
        Some(sender)
    }

    async fn finish_model_events(&mut self) {
        if let Some(forwarder) = self.event_forwarder.take() {
            let _ = forwarder.await;
        }
    }

    async fn on_model_response(
        &mut self,
        usage: Option<dex_ai::Usage>,
        stop_reason: Option<StopReason>,
        elapsed_ms: u64,
    ) -> Result<(), AgentTurnError> {
        if let Some(usage) = usage {
            self.last_usage = Some(usage.prompt_tokens);
            record_usage(
                self.config,
                self.state,
                self.console,
                usage,
                Some(elapsed_ms),
            )
            .await;
        }
        let note = match stop_reason {
            Some(StopReason::Length) => {
                Some("model output hit the output-token limit and may be truncated")
            }
            Some(StopReason::ContentFilter) => {
                Some("model output was cut off by a content filter and may be incomplete")
            }
            _ => None,
        };
        if let Some(note) = note {
            note_sink(
                self.console,
                || SinkLine::System(note.to_string()),
                || format!("[dex] {note}"),
            )
            .await;
        }
        Ok(())
    }

    async fn recover_model_error(
        &mut self,
        error: &str,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<bool, AgentTurnError> {
        if !is_context_overflow(error) {
            return Ok(false);
        }
        match emergency_compact(
            self.config,
            messages,
            self.state,
            self.cancel,
            self.console,
            ledger,
        )
        .await
        {
            Ok(true) => {
                rewrite_session(
                    self.session.as_deref_mut(),
                    messages,
                    &mut self.persisted_cursor,
                )?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn execute_tools(
        &mut self,
        calls: &[dex_ai::LlmToolCall],
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError> {
        // Durable intent + `/undo` before-state, recorded BEFORE execution:
        // a crash mid-tool leaves an `effect_start` without its
        // `effect_result`, which is exactly the restart-recovery signal.
        let tracked: Vec<Option<TrackedCall>> = calls
            .iter()
            .map(|call| {
                track_start(
                    self.session.as_deref_mut(),
                    &call.id,
                    &call.function.name,
                    &call.function.arguments,
                )
            })
            .collect();
        let results =
            run_tool_batch(calls, self.cancel, &self.policy, self.filter, self.console).await;
        if self.cancel.is_cancelled() {
            let _ = self.cancel.take_cancelled();
            for call in calls {
                let name = call.function.name.clone();
                note_sink(
                    self.console,
                    || SinkLine::ToolOutput {
                        id: call.id.clone(),
                        name: name.clone(),
                        summary: "cancelled by user".to_string(),
                        success: false,
                        preview: Vec::new(),
                        duration: 0.0,
                    },
                    || {
                        format!(
                            "{}[tool output] {}:\ncancelled by user{}",
                            TOOL_OUTPUT_COLOR, name, RESET
                        )
                    },
                )
                .await;
            }
            // Cancelled calls close their journal intents with their real
            // outcome: completed calls keep their result (their file changes
            // still land in the undo ledger), never-ran ones record failed.
            for ((_, _, outcome, _), tracked) in results.into_iter().zip(tracked) {
                track_end(self.session.as_deref_mut(), tracked, outcome.ok);
            }
            return Err("cancelled by user".into());
        }

        let turn_cwd = std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        for ((call, (name, input, outcome, elapsed)), tracked) in
            calls.iter().zip(results).zip(tracked)
        {
            let result = prepare_tool_result(
                self.state,
                &mut self.last_tools,
                &turn_cwd,
                &name,
                &input,
                outcome,
            );
            let succeeded = result.ok;
            track_end(self.session.as_deref_mut(), tracked, succeeded);
            note_sink(
                self.console,
                || {
                    let mut summary = tool_result_summary(
                        &name,
                        &input,
                        &result.text,
                        succeeded,
                        result.diff.as_deref(),
                    );
                    if result.cache_hit {
                        summary = format!("cached · {summary}");
                    }
                    let preview =
                        tool_preview(&name, succeeded, result.diff.as_deref(), &result.text);
                    SinkLine::ToolOutput {
                        id: call.id.clone(),
                        name: name.clone(),
                        summary,
                        success: succeeded,
                        preview,
                        duration: elapsed.as_secs_f64(),
                    }
                },
                || {
                    let body =
                        tool_preview_body(&name, succeeded, result.diff.as_deref(), &result.text);
                    format!(
                        "{}[tool output] {}:\n{}{}",
                        TOOL_OUTPUT_COLOR, name, body, RESET
                    )
                },
            )
            .await;

            let mut result_message =
                ChatMessage::tool_result(call.id.clone(), model_tool_result(&result.text));
            result_message.name = Some(name);
            messages.push(result_message);
            ledger.push(messages.last().expect("just pushed"));
            persist_pending(&mut self.session, messages, &mut self.persisted_cursor)?;
        }
        Ok(())
    }

    async fn on_tool_round(
        &mut self,
        outcome: ToolRoundOutcome,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError> {
        match outcome {
            ToolRoundOutcome::Exhausted { completed, .. } => {
                let note = tool_budget_exhausted_note(completed);
                messages.push(ChatMessage::user_named(&note, "budget"));
                ledger.push(messages.last().expect("just pushed"));
                let _ = persist_pending(&mut self.session, messages, &mut self.persisted_cursor);
                return Ok(());
            }
            ToolRoundOutcome::Warn { completed, limit } => {
                system_note(
                    self.console,
                    &format!("{completed}/{limit} tool rounds used this turn"),
                )
                .await;
            }
            ToolRoundOutcome::Continue { .. } => {}
        }
        if self.state.dirty {
            self.state.save_async().await;
            self.state.dirty = false;
        }
        Ok(())
    }

    async fn finish_response(
        &mut self,
        _response: &str,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<bool, AgentTurnError> {
        if let Some(rx) = self.steering_rx.as_mut() {
            if inject_steering(rx, self.steering_accepted_tx, messages).await {
                self.state.last_usage = self.last_usage;
                *ledger = TokenLedger::rebuild(messages);
                return Ok(true);
            }
        }
        self.state.last_usage = self.last_usage;
        persist_pending(&mut self.session, messages, &mut self.persisted_cursor)?;
        Ok(false)
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn make_host<'a, X: CancellationSource + Clone + 'static>(
    config: &'a LlmConfig,
    state: &'a mut ToolState,
    steering_rx: Option<&'a mut mpsc::Receiver<QueueMsg>>,
    steering_accepted_tx: Option<&'a mpsc::Sender<String>>,
    session: Option<&'a mut Session>,
    cancel: &'a X,
    console: &'a Console,
    filter: Option<&'a ToolFilter>,
    agent_ctx: Option<Arc<crate::agent::delegate::AgentTurnContext>>,
    messages_len: usize,
) -> DexTurnHost<'a, X> {
    let mut policy = Policy::turn(config.permission, console);
    policy.agent = agent_ctx;
    let last_usage = state.last_usage;
    DexTurnHost {
        config,
        state,
        steering_rx,
        steering_accepted_tx,
        session,
        cancel,
        console,
        filter,
        policy,
        last_tools: Vec::new(),
        last_usage,
        persisted_cursor: messages_len,
        event_forwarder: None,
    }
}
