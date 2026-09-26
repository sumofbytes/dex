//! Dex turn lifecycle wrapper and host adapter setup for `dex-agent-core`.
//! `daemon::turn` handles HTTP/auth/idempotency/SSE and calls `process_turn`; no HTTP here.

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::state::{CancellationSource, ToolState};
use crate::llm::client::ModelClient;
use crate::llm::config::LlmConfig;
#[cfg(test)]
use crate::llm::transport::sse::Turn;
#[cfg(test)]
use crate::protocol::Role;
use crate::protocol::{ChatMessage, QueueMsg};
#[cfg(test)]
use crate::protocol::{ModelEvent, SinkLine};
use crate::runtime::console::{Console, SpinnerGuard};
use crate::session::Session;
use crate::tools::Policy;
use crate::tools::ToolFilter;
#[cfg(test)]
use tools::record_usage;
#[cfg(test)]
use tools::run_tool_batch;

/// The per-agent capability bundle for [`process_turn`]. One loop serves main agent and children — the
/// bundle decides what each run gets: the main agent passes its steering
/// channels, session, and `filter: None`; a child passes `steering_rx:
/// None`, its own seed messages, its own JSONL session, its own console,
/// and `filter: Some` (allowlist enforced at dispatch). Children never
/// inherit the parent's transcript, steering, session, or cancel token.
pub struct AgentRuntime<'a, C, X> {
    pub config: &'a LlmConfig,
    pub messages: &'a mut Vec<ChatMessage>,
    pub state: &'a mut ToolState,
    pub steering_rx: Option<&'a mut mpsc::Receiver<QueueMsg>>,
    pub steering_accepted_tx: Option<&'a mpsc::Sender<String>>,
    pub session: Option<&'a mut Session>,
    pub client: &'a C,
    pub cancel: &'a X,
    pub console: &'a Console,
    pub filter: Option<&'a ToolFilter>,
    /// The daemon-backed turn context: `Some` for parent turns
    /// inside the daemon — it is what makes `delegate` spawnable — and
    /// for children under the depth cap (one level deeper). At-cap
    /// children and every non-daemon path pass `None` (no delegation).
    pub agent_ctx: Option<Arc<crate::agent::delegate::AgentTurnContext>>,
    /// Turn budget override (a definition's `max_tool_iterations`
    /// feeds the existing budget knob; `None` = the default/env value).
    pub tool_budget: Option<usize>,
    /// Overwritable harness decisions for this turn (`None` = defaults with
    /// `DEX_MAX_TOOL_ITERATIONS` read once at turn setup). Pass a custom
    /// [`DexHarness`](crate::agent::composable::DexHarness) to swap one
    /// piece — catalog, trigger, overflow wording, conflict rule, scorer,
    /// executor, transcript store — without forking the loop.
    pub harness: Option<std::sync::Arc<crate::agent::composable::DexHarness>>,
}

/// Apply one drained queue message to the not-yet-injected `pending` list:
/// `Content` appends, `Recall` removes the newest matching item. Recalls are
/// applied in arrival order, so a recall can only cancel an item that has not
/// been injected yet — one already sent is part of the transcript.
///
/// # Drain points (the steering contract)
///
/// Queued messages are drained only at two points in the agent engine,
/// both *before a model call* — never mid-batch, never between a tool call
/// and its result:
///
/// 1. top of every loop iteration (before compaction + the LLM call), and
/// 2. after an assistant message with no tool calls lands, so a steering
///    message racing the turn's final text still gets injected (loop
///    continues once; the next iteration returns the new final text).
///
/// Everything sent after the last drain of a turn waits for the next turn —
/// the queue never blocks the caller and never grows the current prompt
/// after the request body is built. One drain applies ALL queued messages in
/// arrival order (`inject_steering` loops `try_recv` until empty).
pub fn apply_queue_msg(pending: &mut Vec<String>, msg: QueueMsg) {
    match msg {
        QueueMsg::Content(text) => pending.push(text),
        QueueMsg::Recall(text) => {
            if let Some(pos) = pending.iter().rposition(|item| item == &text) {
                pending.remove(pos);
            }
        }
    }
}

pub async fn process_turn<C, X>(
    rt: AgentRuntime<'_, C, X>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    C: ModelClient + 'static,
    X: CancellationSource + Clone + 'static,
{
    // Pin this turn's model drive context for the whole turn: extension
    // drives read it instead of the process-wide fallback, so concurrent
    // turns (daemon sessions) and nested child turns each serve their own
    // model. The recorder below still updates the fallback for out-of-turn
    // drives, and the `model_select` event still fires on change.
    let drive = crate::extensions::drive_model_for(rt.config);
    crate::extensions::with_drive_model(drive, process_turn_scoped(rt)).await
}

async fn process_turn_scoped<C, X>(
    rt: AgentRuntime<'_, C, X>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    C: ModelClient + 'static,
    X: CancellationSource + Clone + 'static,
{
    // Lifecycle hooks (plan §7): `before_agent_start` may append to the
    // system prompt for this turn (read-only influence, Pi's prompt
    // customizer); `turn.start` before anything runs; `turn.end` on every
    // exit path with the outcome. Fire-and-forget — these events carry no
    // directive the host acts on. The hook host gets this turn's policy, so
    // a nested `dex.tools.call` from a hook is gated exactly like a
    // model-issued one.
    let turn_policy = Policy::turn(rt.config.permission, rt.console);
    let cancel = rt.cancel.clone();
    let filter = rt.filter;
    // Model-aware extensions (`dex.model`, provider-native tools) sync on
    // `model_select`: fires when the served `provider/model` changed since
    // the last turn (always on the first), and records the snapshot
    // `dex.model` reads — including per-request daemon overrides the file
    // never sees. Same fail-open contract as the hooks below.
    crate::extensions::fire_model_select_if_changed(rt.config, &cancel, &turn_policy, filter).await;
    // `before_agent_start` system-prompt append (plan §7 P2): applied to the
    // leading System message for the duration of the turn, restored before
    // the result leaves — the journal never stores System role messages, so
    // nothing persists into later turns.
    let appends = crate::extensions::apply_before_agent_start(&cancel, &turn_policy, filter).await;
    // Some(original) = the appendix was applied and must be restored.
    let saved_system: Option<Option<String>> = if appends.is_empty() {
        None
    } else {
        let appendix = appends.join("\n\n");
        match rt.messages.first_mut() {
            Some(first) if first.role == crate::protocol::Role::System => {
                let original = first.content.clone();
                first.content = Some(format!(
                    "{}\n\n--- Extensions ---\n{}",
                    original.clone().unwrap_or_default(),
                    appendix
                ));
                Some(original)
            }
            _ => None,
        }
    };
    crate::extensions::fire_event_global(
        "turn.start",
        serde_json::json!({}),
        &cancel,
        &turn_policy,
        filter,
    )
    .await;
    // Destructure so `messages` survives the call: the appendix restore below
    // needs it back, and inner's returns are many (a drop guard cannot hold a
    // second &mut).
    let AgentRuntime {
        config,
        messages,
        state,
        steering_rx,
        steering_accepted_tx,
        session,
        client,
        cancel: cancel_ref,
        console,
        filter,
        agent_ctx,
        tool_budget,
        harness,
    } = rt;
    let result = run_agent_engine(DexHostSetup {
        config,
        messages: &mut *messages,
        state,
        steering_rx,
        steering_accepted_tx,
        session,
        client,
        cancel: cancel_ref,
        console,
        filter,
        agent_ctx,
        tool_budget,
        harness,
    })
    .await;
    // Restore the System message the appendix rode on: per-turn scope.
    if let Some(original) = saved_system {
        if let Some(first) = messages.first_mut() {
            if first.role == crate::protocol::Role::System {
                first.content = original;
            }
        }
    }
    let payload = match &result {
        Ok(_) => serde_json::json!({ "ok": true }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    };
    crate::extensions::fire_event_global("turn.end", payload, &cancel, &turn_policy, filter).await;
    result
}

/// Dex host construction inputs, kept bundled while the lifecycle wrapper
/// retains its mutable access to history for restoring the prompt appendix.
struct DexHostSetup<'a, C, X> {
    config: &'a LlmConfig,
    messages: &'a mut Vec<ChatMessage>,
    state: &'a mut ToolState,
    steering_rx: Option<&'a mut mpsc::Receiver<QueueMsg>>,
    steering_accepted_tx: Option<&'a mpsc::Sender<String>>,
    session: Option<&'a mut Session>,
    client: &'a C,
    cancel: &'a X,
    console: &'a Console,
    filter: Option<&'a ToolFilter>,
    agent_ctx: Option<Arc<crate::agent::delegate::AgentTurnContext>>,
    tool_budget: Option<usize>,
    harness: Option<Arc<crate::agent::composable::DexHarness>>,
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_engine<C, X>(
    DexHostSetup {
        config,
        messages,
        state,
        steering_rx,
        steering_accepted_tx,
        session,
        client,
        cancel,
        console,
        filter,
        agent_ctx,
        tool_budget,
        harness,
    }: DexHostSetup<'_, C, X>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    C: ModelClient + 'static,
    X: CancellationSource + Clone + 'static,
{
    let _working = SpinnerGuard::start(console, "Working");
    // Single env boundary for the turn: defaults read
    // `DEX_MAX_TOOL_ITERATIONS` here, once — never per helper call.
    let harness =
        harness.unwrap_or_else(|| Arc::new(crate::agent::composable::DexHarness::from_env()));
    let tool_round_limit = tool_budget.unwrap_or_else(|| harness.config.max_tool_iterations());
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
    dex_agent_core::run_turn(client, cancel, messages, &mut host, tool_round_limit).await
}

pub(crate) mod host;
pub(crate) mod tool_results;
pub(crate) mod tools;

#[cfg(test)]
pub mod tests;
