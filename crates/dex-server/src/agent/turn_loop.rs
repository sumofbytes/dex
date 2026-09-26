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
    // `model_selector` slot (spec §11): the first Lua opinion picks the
    // model this turn serves; no opinion — or an unresolvable one, which
    // `apply_model` refuses rather than half-switching — keeps the
    // configured model. Fail-open, like every hook. The resolved override
    // drives three views at once: the wire (`with_served_model`, since the
    // client IS the config in production), the host-side budget/compaction
    // views (`process_turn_scoped`'s `config`), and the extension drive
    // snapshot (`drive_model_for`) — so `dex.model`, `model_select` and
    // compaction thresholds all agree on what was served.
    let selection = {
        let cancel = rt.cancel.clone();
        crate::extensions::query_model_selector_global(rt.config, &cancel).await
    };
    let override_config: Option<std::sync::Arc<crate::llm::config::LlmConfig>> = selection
        .and_then(|selection| {
            let mut config = rt.config.clone();
            match config.apply_model(&selection, false) {
                Ok(_) => Some(std::sync::Arc::new(config)),
                Err(e) => {
                    crate::runtime::notice::warn_once(
                        "ext.model-selector",
                        &format!("model_selector: cannot serve '{selection}': {e}"),
                    );
                    None
                }
            }
        });
    let config = override_config
        .as_ref()
        .map(|a| a.as_ref())
        .unwrap_or(rt.config);
    // `tool_catalog` slot (spec §11): the first Lua opinion narrows the
    // served schemas for this turn. Computed from the default composed
    // assembly before the scope opens, so the filter never filters itself;
    // no opinion — or an error, which fails open — serves the full catalog.
    // Narrowing-only by construction (`CatalogFilter::apply` is a subset op).
    let catalog_schemas = {
        let cancel = rt.cancel.clone();
        crate::extensions::query_tool_catalog_global(
            &{
                use dex_agent_core::ToolCatalog as _;
                crate::agent::composable::ComposedCatalog::dex_default().tool_schemas()
            },
            &cancel,
        )
        .await
    };
    let drive = crate::extensions::drive_model_for(config);
    let fut = process_turn_scoped(rt, config);
    match (override_config.as_ref(), catalog_schemas) {
        // The task-local scope is what the wire sees (`LlmConfig` clients);
        // test mocks and other clients ignore it.
        (Some(_), Some(schemas)) => {
            crate::llm::client::with_served_model(
                override_config.as_ref().expect("checked").clone(),
                crate::agent::composable::with_tool_catalog_override(
                    schemas,
                    crate::extensions::with_drive_model(drive, fut),
                ),
            )
            .await
        }
        (Some(_), None) => {
            crate::llm::client::with_served_model(
                override_config.as_ref().expect("checked").clone(),
                crate::extensions::with_drive_model(drive, fut),
            )
            .await
        }
        (None, Some(schemas)) => {
            crate::agent::composable::with_tool_catalog_override(
                schemas,
                crate::extensions::with_drive_model(drive, fut),
            )
            .await
        }
        (None, None) => crate::extensions::with_drive_model(drive, fut).await,
    }
}

async fn process_turn_scoped<C, X>(
    rt: AgentRuntime<'_, C, X>,
    config: &crate::llm::config::LlmConfig,
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
    let turn_policy = Policy::turn(config.permission, rt.console);
    let cancel = rt.cancel.clone();
    let filter = rt.filter;
    // Model-aware extensions (`dex.model`, provider-native tools) sync on
    // `model_select`: fires when the served `provider/model` changed since
    // the last turn (always on the first), and records the snapshot
    // `dex.model` reads — including per-request daemon overrides the file
    // never sees. Same fail-open contract as the hooks below.
    crate::extensions::fire_model_select_if_changed(config, &cancel, &turn_policy, filter).await;
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
        config: _,
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
    // Runtime observation: `message.sent` fires for the turn's final text
    // (uniform across daemon, children, one-shot, TUI-local — every path
    // runs through here). Truncated preview; failures carry the error.
    {
        let (ok, body) = match &result {
            Ok(text) => (true, text.clone()),
            Err(e) => (false, e.to_string()),
        };
        let (preview, truncated, chars) = crate::extensions::text_preview(&body);
        crate::extensions::fire_lifecycle_event(
            "message.sent",
            serde_json::json!({
                "ok": ok,
                "preview": preview,
                "truncated": truncated,
                "chars": chars,
            }),
            &cancel,
        )
        .await;
    }
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
    // `DEX_MAX_TOOL_ITERATIONS` here, once — never per helper call. The
    // resolver applies `harness.slots:` registry selections on top.
    let harness = harness.unwrap_or_else(crate::agent::registry::resolve_snapshot);
    let tool_round_limit = tool_budget.unwrap_or_else(|| harness.config.max_tool_iterations());
    // `agent_loop` slot (spec §9 / Phase 5): a registered Lua loop replaces
    // the whole `run_turn` orchestration — this driver keeps the host, the
    // token ledger, the tool-round budget, and cancellation; the loop owns
    // iteration. Queried at the same snapshot point as every other slot;
    // `None` (no registered loop) keeps the Rust engine.
    if let Some((ext_id, engine)) = crate::extensions::agent_loop_global().await {
        let started = std::time::Instant::now();
        let result = lua_loop::run_lua_agent_loop(
            engine,
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
            harness,
            tool_round_limit,
        )
        .await;
        // §35 invocation trace: the loop is one long invocation spanning the
        // whole turn — one record, the turn's duration and outcome.
        crate::extensions::trace::record(
            &ext_id,
            "agent_loop",
            started.elapsed(),
            if result.is_ok() { "ok" } else { "error" },
            result.as_ref().err().map(|e| e.to_string()),
        );
        return result;
    }
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
pub(crate) mod lua_loop;
pub(crate) mod tool_results;
pub(crate) mod tools;

#[cfg(test)]
pub mod tests;
