//! The Lua agent loop (spec §9 / Phase 5, `agent_loop.v1`): a registered
//! extension loop (`dex.replace("agent_loop", { run = fn })`) owns the turn's
//! iteration; this driver owns the turn's real state and answers the loop's
//! step upcalls on the task side.
//!
//! The split mirrors the extension engine's worker/task design: the Lua VM
//! lives on the extension's worker thread and blocks on each step reply;
//! this task side performs each step against the same `DexTurnHost`, token
//! ledger, and tool-round budget the Rust `run_turn` engine uses — so every
//! invariant (normalized history transitions, persistence, cancellation,
//! streaming, budget) is kept by construction. The default loop, expressed
//! in the Lua surface, is:
//!
//! ```lua
//! while true do
//!   local r = ctx.model.call()
//!   if r.tool_calls and #r.tool_calls > 0 then
//!     ctx.tools.execute()
//!   else
//!     local s = ctx.finish(r.content or "")
//!     if not s.steered then return r.content or "" end
//!   end
//! end
//! ```
//!
//! The worker has no per-call deadline (model rounds take minutes): the
//! driver sets the shared `abort` flag on every exit path, the instruction
//! hook enforces it between Lua instructions, and cancellation is checked
//! both between steps and inside the model request (`tokio::select!`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dex_agent_core::{
    apply_model_turn, tool_budget_exhausted_note, AgentHost, TokenLedger, ToolRoundBudget,
    ToolRoundOutcome,
};
use tokio::sync::mpsc;

use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::extensions::{ExtensionEngine, HostOp, WorkerMsg};
use crate::llm::client::ModelClient;
use crate::protocol::ChatMessage;

use super::host;
use crate::llm::config::LlmConfig;

/// Same boxed error type the Rust engine path returns.
type AgentTurnError = Box<dyn std::error::Error + Send + Sync>;

/// One answered step: a JSON envelope back to the loop, or a failure that
/// ends the turn (the error reaches the loop for its own diagnostics, then
/// the turn fails with the same message).
enum Outcome {
    Step(String),
    Fatal(String),
}

/// Reap a worker after the turn is over: drain until its unwind `Done` or a
/// short grace period. The caller reports its own outcome regardless (same
/// contract as the engine's `abort_wait`).
async fn reap_worker(rx: &mut mpsc::UnboundedReceiver<WorkerMsg>) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(msg) = rx.recv().await {
            if matches!(msg, WorkerMsg::Done(_)) {
                break;
            }
        }
    })
    .await;
}

/// Run one turn through a registered Lua agent loop. The loop drives the
/// same [`AgentHost`] the Rust `run_turn` engine would, so persistence,
/// steering, compaction, streaming, and budgets are unchanged; only the
/// sequence decisions move into Lua.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_lua_agent_loop<C, X>(
    engine: ExtensionEngine,
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut crate::agent::state::ToolState,
    steering_rx: Option<&mut mpsc::Receiver<crate::protocol::QueueMsg>>,
    steering_accepted_tx: Option<&mpsc::Sender<String>>,
    session: Option<&mut crate::session::Session>,
    client: &C,
    cancel: &X,
    console: &crate::runtime::console::Console,
    filter: Option<&crate::tools::ToolFilter>,
    agent_ctx: Option<Arc<crate::agent::delegate::AgentTurnContext>>,
    harness: Arc<crate::agent::composable::DexHarness>,
    tool_round_limit: usize,
) -> Result<String, AgentTurnError>
where
    C: ModelClient + 'static,
    X: CancellationSource + Clone + 'static,
{
    let mut host = host::make_host(
        config,
        state,
        steering_rx,
        steering_accepted_tx,
        session,
        cancel,
        console,
        filter,
        agent_ctx,
        messages.len(),
        harness,
    );
    // The engine's per-turn state, driven step-by-step from Lua.
    let mut ledger = TokenLedger::rebuild(messages);
    let mut rounds = ToolRoundBudget::new(tool_round_limit);
    let mut overflow_retried = false;
    // Tool calls of the last applied response, awaiting `ctx.tools.execute()`.
    let mut pending: Option<Vec<dex_ai::LlmToolCall>> = None;

    let abort = Arc::new(AtomicBool::new(false));
    let mut rx = match engine.agent_loop_request(Arc::clone(&abort)).await {
        Ok(rx) => rx,
        Err(e) => return Err(e.into()),
    };

    loop {
        let msg = tokio::select! {
            msg = rx.recv() => msg,
            _ = wait_cancelled(cancel) => {
                abort.store(true, Ordering::Relaxed);
                reap_worker(&mut rx).await;
                return Err("cancelled by user".into());
            }
        };
        let Some(msg) = msg else {
            return Err("extension agent loop worker stopped".into());
        };
        let WorkerMsg::HostCall { op, reply } = msg else {
            // Terminal `Done`: the loop's result envelope.
            let WorkerMsg::Done(result) = msg else {
                unreachable!("a worker sends exactly one terminal Done");
            };
            abort.store(true, Ordering::Relaxed);
            return map_loop_result(result);
        };

        let outcome = match op {
            HostOp::LoopModel => {
                loop_model_step(
                    &mut host,
                    client,
                    cancel,
                    messages,
                    &mut ledger,
                    &mut overflow_retried,
                    &mut pending,
                )
                .await
            }
            HostOp::LoopTools => {
                loop_tools_step(
                    &mut host,
                    messages,
                    &mut ledger,
                    &mut rounds,
                    pending.take(),
                )
                .await
            }
            HostOp::LoopFinish { response } => {
                match host.finish_response(&response, messages, &mut ledger).await {
                    Ok(true) => Outcome::Step(r#"{"steered":true}"#.to_string()),
                    Ok(false) => Outcome::Step(r#"{"steered":false}"#.to_string()),
                    Err(e) => Outcome::Fatal(e.to_string()),
                }
            }
            HostOp::LoopState => Outcome::Step(
                serde_json::json!({
                    "cancelled": cancel.is_cancelled(),
                    "rounds": rounds.completed(),
                    "round_limit": rounds.limit(),
                    "messages": messages.len(),
                })
                .to_string(),
            ),
            HostOp::LoopCancelled => Outcome::Step(cancel.is_cancelled().to_string()),
            // Generic `dex.*` upcalls (`dex.tools.call`, `dex.net.fetch`,
            // ...) ride the same channel inside a loop drive: answer them
            // through the engine's host-call path under this turn's policy,
            // so a loop's tool access is gated exactly like a model's.
            op => {
                let policy = crate::tools::Policy::turn(config.permission, console);
                let ctx = crate::extensions::HostCtx {
                    cancel,
                    policy: &policy,
                    filter,
                };
                match crate::extensions::answer_hostcall(op, &ctx).await {
                    Ok(reply) => Outcome::Step(reply),
                    Err(e) => Outcome::Fatal(e),
                }
            }
        };
        match outcome {
            Outcome::Step(step_reply) => {
                let _ = reply.send(Ok(step_reply));
            }
            Outcome::Fatal(error) => {
                let _ = reply.send(Err(error.clone()));
                abort.store(true, Ordering::Relaxed);
                reap_worker(&mut rx).await;
                return Err(error.into());
            }
        }
    }
}

/// Map the loop's terminal result envelope: `{text}` is the turn response,
/// `{error}` (or a Lua failure) fails the turn with that message.
fn map_loop_result(result: Result<String, String>) -> Result<String, AgentTurnError> {
    match result {
        Ok(envelope) => {
            let parsed: serde_json::Value = serde_json::from_str(&envelope)
                .map_err(|e| format!("agent loop reply is not JSON: {e}"))?;
            if let Some(error) = parsed.get("error").and_then(|v| v.as_str()) {
                return Err(error.to_string().into());
            }
            Ok(parsed
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string())
        }
        Err(error) => Err(error.into()),
    }
}

/// One engine round, exactly the default loop's sequence: `before_model`,
/// cancellation check, streaming model request under `select!` against
/// cancellation, `finish_model_events`, error recovery (one retry per turn,
/// retried internally so the loop sees only the eventual outcome),
/// `on_model_response`, and the normalized history transition. The applied
/// tool calls are parked for [`HostOp::LoopTools`].
#[allow(clippy::too_many_arguments)]
async fn loop_model_step<C, X, H>(
    host: &mut H,
    client: &C,
    cancel: &X,
    messages: &mut Vec<ChatMessage>,
    ledger: &mut TokenLedger,
    overflow_retried: &mut bool,
    pending: &mut Option<Vec<dex_ai::LlmToolCall>>,
) -> Outcome
where
    C: ModelClient,
    X: CancellationSource + Sync,
    H: AgentHost,
{
    if let Err(e) = host.before_model(messages, ledger).await {
        return Outcome::Fatal(e.to_string());
    }
    if cancel.is_cancelled() {
        let _ = cancel.take_cancelled();
        return Outcome::Fatal("cancelled by user".to_string());
    }
    loop {
        let schemas = host.tool_schemas();
        let events = host.start_model_events();
        let started = Instant::now();
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let result = tokio::select! {
            _ = wait_cancelled(cancel_ref) => Err("cancelled by user".to_string()),
            result = client.complete(messages, &schemas, events.clone(), cancel_ref) => {
                result.map_err(|e| e.to_string())
            }
        };
        drop(events);
        host.finish_model_events().await;
        let turn = match result {
            Ok(turn) => turn,
            Err(message) => {
                if matches!(
                    message.as_str(),
                    "interrupted" | "cancelled" | "cancelled by user"
                ) {
                    return Outcome::Fatal("cancelled by user".to_string());
                }
                let recovered = match host.recover_model_error(&message, messages, ledger).await {
                    Ok(recovered) => recovered,
                    Err(e) => return Outcome::Fatal(e.to_string()),
                };
                if !*overflow_retried && recovered {
                    *overflow_retried = true;
                    continue;
                }
                return Outcome::Fatal(message);
            }
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Err(e) = host
            .on_model_response(turn.usage, turn.stop_reason, elapsed_ms)
            .await
        {
            return Outcome::Fatal(e.to_string());
        }
        let applied = apply_model_turn(messages, ledger, turn);
        let calls = applied.tool_calls.clone();
        *pending = calls;
        return Outcome::Step(
            serde_json::json!({
                "content": if applied.response.is_empty() { None } else { Some(applied.response) },
                "tool_calls": applied.tool_calls.map(|calls| {
                    calls.iter().map(|c| serde_json::json!({
                        "id": c.id,
                        "name": c.function.name,
                        "args": serde_json::from_str::<serde_json::Value>(&c.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default())),
                    })).collect::<Vec<_>>()
                }),
            })
            .to_string(),
        );
    }
}

/// Execute the parked tool calls through the host (hooks, gates, dispatch —
/// everything a model-issued batch goes through), complete the budget round,
/// and report the outcome. Budget exhaustion fails the turn with the same
/// note the Rust engine returns.
async fn loop_tools_step<H>(
    host: &mut H,
    messages: &mut Vec<ChatMessage>,
    ledger: &mut TokenLedger,
    rounds: &mut ToolRoundBudget,
    calls: Option<Vec<dex_ai::LlmToolCall>>,
) -> Outcome
where
    H: AgentHost,
{
    let Some(calls) = calls else {
        return Outcome::Fatal(
            "ctx.tools.execute() with no pending tool calls: call ctx.model.call() first"
                .to_string(),
        );
    };
    if let Err(e) = host.execute_tools(&calls, messages, ledger).await {
        return Outcome::Fatal(e.to_string());
    }
    let outcome = rounds.complete_round();
    if let Err(e) = host.on_tool_round(outcome, messages, ledger).await {
        return Outcome::Fatal(e.to_string());
    }
    if let ToolRoundOutcome::Exhausted { completed, .. } = outcome {
        return Outcome::Fatal(tool_budget_exhausted_note(completed));
    }
    match outcome {
        ToolRoundOutcome::Continue { completed, limit } => {
            Outcome::Step(serde_json::json!({ "completed": completed, "limit": limit }).to_string())
        }
        ToolRoundOutcome::Warn { completed, limit } => Outcome::Step(
            serde_json::json!({ "completed": completed, "limit": limit, "warned": true })
                .to_string(),
        ),
        ToolRoundOutcome::Exhausted { .. } => unreachable!("handled above"),
    }
}
