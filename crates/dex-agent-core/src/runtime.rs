//! Provider- and host-independent model/tool turn orchestration.

use std::time::Instant;

use dex_ai::{
    CancellationSource, ChatMessage, ModelClient, ModelEvent, StopReason, ToolDefinition, Usage,
};
use tokio::sync::mpsc;

use crate::{apply_model_turn, TokenLedger, ToolRoundBudget, ToolRoundOutcome};

pub type AgentTurnError = Box<dyn std::error::Error + Send + Sync>;

/// Application services used by the reusable turn state machine.
///
/// Hosts supply prompt maintenance, tool execution, persistence, and
/// transcript output. The engine owns model/tool sequencing, normalized
/// response application, cancellation during model calls, and tool budgets.
///
/// A host can expose the engine through a small application wrapper:
///
/// ```
/// use dex_agent_core::{run_turn, AgentHost, AgentTurnError};
/// use dex_ai::{CancellationSource, ChatMessage, ModelClient};
///
/// async fn run_for_host<C, X, H>(
///     model: &C,
///     cancel: &X,
///     history: &mut Vec<ChatMessage>,
///     host: &mut H,
/// ) -> Result<String, AgentTurnError>
/// where
///     C: ModelClient + 'static,
///     X: CancellationSource + Send + Sync + 'static,
///     H: AgentHost,
/// {
///     run_turn(model, cancel, history, host, 64).await
/// }
/// ```
#[allow(async_fn_in_trait)]
pub trait AgentHost: Send {
    /// Persist/inject/compact history before each model request.
    ///
    /// This runs before the engine's cancellation check. When it changes
    /// history, it must keep `ledger` synchronized. Steering intended for the
    /// next request may be injected here.
    async fn before_model(
        &mut self,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError>;

    /// Return the schema surface for the next provider request.
    fn tool_schemas(&self) -> Vec<ToolDefinition>;

    /// Start streaming model events for one request, if the host has a sink.
    /// `finish_model_events` is called after every request, including errors.
    fn start_model_events(&mut self) -> Option<mpsc::Sender<ModelEvent>>;
    /// Close the per-request event channel and finish forwarding buffered events.
    async fn finish_model_events(&mut self);

    async fn on_model_response(
        &mut self,
        usage: Option<Usage>,
        stop_reason: Option<StopReason>,
        elapsed_ms: u64,
    ) -> Result<(), AgentTurnError>;

    /// Return true after successfully handling a recoverable model error.
    /// The engine allows one successful recovery/retry per user turn. Return
    /// false to have the engine return the original model error unchanged.
    async fn recover_model_error(
        &mut self,
        error: &str,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<bool, AgentTurnError>;

    /// Execute the calls and append their results to history and `ledger`.
    async fn execute_tools(
        &mut self,
        calls: &[dex_ai::LlmToolCall],
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError>;

    /// Observe the completed batch outcome after tool results were recorded.
    /// This is called after every batch. On `Warn`, hosts may emit a notice;
    /// on `Exhausted`, append/persist a resume marker before the engine returns
    /// its tool-budget error.
    async fn on_tool_round(
        &mut self,
        outcome: ToolRoundOutcome,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<(), AgentTurnError>;

    /// Inject any steering that arrived during the final model call and persist.
    /// Return true only when new input was injected and the engine should run
    /// another model request.
    async fn finish_response(
        &mut self,
        response: &str,
        messages: &mut Vec<ChatMessage>,
        ledger: &mut TokenLedger,
    ) -> Result<bool, AgentTurnError>;
}

/// Run the model/tool loop for one user turn.
///
/// A tool budget counts completed assistant batches, regardless of how many
/// tool calls each batch contains. `tool_round_limit == 0` allows a final
/// response but stops after the first batch that requests tools.
///
/// Returns the final assistant response or a model, host, cancellation, or
/// tool-budget error.
pub async fn run_turn<C, X, H>(
    client: &C,
    cancel: &X,
    messages: &mut Vec<ChatMessage>,
    host: &mut H,
    tool_round_limit: usize,
) -> Result<String, AgentTurnError>
where
    C: ModelClient + 'static,
    X: CancellationSource + Send + Sync + 'static,
    H: AgentHost,
{
    let mut ledger = TokenLedger::rebuild(messages);
    let mut rounds = ToolRoundBudget::new(tool_round_limit);
    let mut overflow_retried = false;

    loop {
        host.before_model(messages, &mut ledger).await?;
        if cancel.is_cancelled() {
            let _ = cancel.take_cancelled();
            return Err("cancelled by user".into());
        }

        let schemas = host.tool_schemas();
        let events = host.start_model_events();
        let started = Instant::now();
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let result = tokio::select! {
            _ = wait_cancelled(cancel_ref) => Err("cancelled by user".into()),
            result = client.complete(messages, &schemas, events.clone(), cancel_ref) => result,
        };
        drop(events);
        host.finish_model_events().await;
        let turn = match result {
            Ok(turn) => turn,
            Err(error) => {
                let message = error.to_string();
                if matches!(
                    message.as_str(),
                    "interrupted" | "cancelled" | "cancelled by user"
                ) {
                    return Err("cancelled by user".into());
                }
                if !overflow_retried
                    && host
                        .recover_model_error(&message, messages, &mut ledger)
                        .await?
                {
                    overflow_retried = true;
                    continue;
                }
                return Err(message.into());
            }
        };

        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        host.on_model_response(turn.usage, turn.stop_reason, elapsed_ms)
            .await?;
        let applied = apply_model_turn(messages, &mut ledger, turn);
        if let Some(calls) = applied.tool_calls {
            host.execute_tools(&calls, messages, &mut ledger).await?;
            let outcome = rounds.complete_round();
            host.on_tool_round(outcome, messages, &mut ledger).await?;
            if let ToolRoundOutcome::Exhausted { completed, .. } = outcome {
                return Err(format!(
                    "turn budget exhausted after {completed} tool rounds; partial progress preserved — send another prompt to continue"
                )
                .into());
            }
        } else {
            let response = applied.response;
            if !host
                .finish_response(&response, messages, &mut ledger)
                .await?
            {
                return Ok(response);
            }
        }
    }
}

async fn wait_cancelled(cancel: &(dyn CancellationSource + Send + Sync)) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use dex_ai::{FunctionCall, LlmToolCall, Turn};

    use super::*;

    #[derive(Clone)]
    struct MockModel(std::sync::Arc<Mutex<VecDeque<Turn>>>);

    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolDefinition],
            _events: Option<mpsc::Sender<ModelEvent>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            self.0.lock().unwrap().pop_front().ok_or_else(|| {
                Box::new(std::io::Error::other("no queued model response"))
                    as Box<dyn std::error::Error + Send + Sync>
            })
        }
    }

    #[derive(Clone, Default)]
    struct NeverCancel;

    impl CancellationSource for NeverCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn take_cancelled(&self) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct Host {
        executed: Vec<String>,
        rounds: Vec<ToolRoundOutcome>,
    }

    impl AgentHost for Host {
        async fn before_model(
            &mut self,
            _messages: &mut Vec<ChatMessage>,
            _ledger: &mut TokenLedger,
        ) -> Result<(), AgentTurnError> {
            Ok(())
        }

        fn tool_schemas(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }
        fn start_model_events(&mut self) -> Option<mpsc::Sender<ModelEvent>> {
            None
        }
        async fn finish_model_events(&mut self) {}

        async fn on_model_response(
            &mut self,
            _usage: Option<Usage>,
            _stop_reason: Option<StopReason>,
            _elapsed_ms: u64,
        ) -> Result<(), AgentTurnError> {
            Ok(())
        }

        async fn recover_model_error(
            &mut self,
            _error: &str,
            _messages: &mut Vec<ChatMessage>,
            _ledger: &mut TokenLedger,
        ) -> Result<bool, AgentTurnError> {
            Ok(false)
        }

        async fn execute_tools(
            &mut self,
            calls: &[dex_ai::LlmToolCall],
            messages: &mut Vec<ChatMessage>,
            ledger: &mut TokenLedger,
        ) -> Result<(), AgentTurnError> {
            for call in calls {
                self.executed.push(call.function.name.clone());
                messages.push(ChatMessage::tool_result(&call.id, "done"));
                ledger.push(messages.last().expect("tool result was appended"));
            }
            Ok(())
        }

        async fn on_tool_round(
            &mut self,
            outcome: ToolRoundOutcome,
            _messages: &mut Vec<ChatMessage>,
            _ledger: &mut TokenLedger,
        ) -> Result<(), AgentTurnError> {
            self.rounds.push(outcome);
            Ok(())
        }

        async fn finish_response(
            &mut self,
            _response: &str,
            _messages: &mut Vec<ChatMessage>,
            _ledger: &mut TokenLedger,
        ) -> Result<bool, AgentTurnError> {
            Ok(false)
        }
    }

    fn tool_turn() -> Turn {
        Turn {
            message: ChatMessage::assistant_calls(
                None,
                vec![LlmToolCall {
                    id: "call-1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
            usage: None,
            stop_reason: Some(StopReason::ToolUse),
        }
    }

    fn model(turns: Vec<Turn>) -> MockModel {
        MockModel(std::sync::Arc::new(Mutex::new(turns.into())))
    }

    #[tokio::test]
    async fn engine_executes_tool_round_then_returns_final_response() {
        let model = model(vec![
            tool_turn(),
            Turn {
                message: ChatMessage::assistant("finished"),
                usage: None,
                stop_reason: Some(StopReason::Stop),
            },
        ]);
        let cancel = NeverCancel;
        let mut messages = vec![ChatMessage::user("do it")];
        let mut host = Host::default();
        let answer = run_turn(&model, &cancel, &mut messages, &mut host, 5)
            .await
            .unwrap();
        assert_eq!(answer, "finished");
        assert_eq!(host.executed, ["read"]);
        assert!(messages
            .iter()
            .any(|message| message.role == dex_ai::Role::Tool));
        assert_eq!(
            host.rounds,
            [ToolRoundOutcome::Continue {
                completed: 1,
                limit: 5
            }]
        );
    }

    #[tokio::test]
    async fn engine_stops_after_tool_round_budget() {
        let model = model(vec![tool_turn()]);
        let cancel = NeverCancel;
        let mut messages = vec![ChatMessage::user("do it")];
        let mut host = Host::default();
        let error = run_turn(&model, &cancel, &mut messages, &mut host, 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("turn budget exhausted after 1 tool rounds"));
        assert_eq!(
            host.rounds,
            [ToolRoundOutcome::Exhausted {
                completed: 1,
                limit: 1
            }]
        );
    }
}
