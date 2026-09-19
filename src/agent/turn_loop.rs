//! Agent turn state machine (`process_turn`/`AgentRuntime`): owns the model↔tool loop.
//! Boundary: `daemon::turn` is the HTTP handler (auth, idempotency, SSE emit) and calls into here; no HTTP here.

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::compaction::{compact_history, KEEP_RECENT_MESSAGES};
use crate::agent::jev::summary_mode;
use crate::agent::state::{wait_cancelled, CancellationSource, ToolState};
use crate::agent::tokens::{estimate_ephemeral_tokens, schema_budget_tokens, TokenLedger};
use crate::llm::client::ModelClient;
use crate::llm::config::LlmConfig;
use crate::llm::transport::sse::Turn;
use crate::protocol::{ChatMessage, QueueMsg, Role, SinkLine, StopReason};
use crate::runtime::console::{Console, SpinnerGuard, RESET, TOOL_OUTPUT_COLOR};
use crate::session::Session;
use crate::tools::{Policy, ToolFilter};

/// The per-agent capability bundle for [`process_turn`] (Phase 2 runtime
/// extraction; plan §8). One loop serves main agent and children — the
/// bundle decides what each run gets: the main agent passes its steering
/// channels, session, and `filter: None`; a child passes `steering_rx:
/// None`, its own seed messages, its own JSONL session, its own console,
/// and `filter: Some` (allowlist enforced at dispatch). Children never
/// inherit the parent's transcript, steering, session, or cancel token.
pub(crate) struct AgentRuntime<'a, C, X> {
    pub(crate) config: &'a LlmConfig,
    pub(crate) messages: &'a mut Vec<ChatMessage>,
    pub(crate) state: &'a mut ToolState,
    pub(crate) steering_rx: Option<&'a mut mpsc::Receiver<QueueMsg>>,
    pub(crate) steering_accepted_tx: Option<&'a mpsc::Sender<String>>,
    pub(crate) session: Option<&'a mut Session>,
    pub(crate) client: &'a C,
    pub(crate) cancel: &'a X,
    pub(crate) console: &'a Console,
    pub(crate) filter: Option<&'a ToolFilter>,
    /// The daemon-backed turn context (Phase 5): `Some` for parent turns
    /// inside the daemon — it is what makes `delegate` spawnable — and
    /// for children under the depth cap (one level deeper). At-cap
    /// children and every non-daemon path pass `None` (no delegation).
    pub(crate) agent_ctx: Option<Arc<crate::agent::subagent::AgentTurnContext>>,
    /// Turn budget override (plan §4: a definition's `max_tool_iterations`
    /// feeds the existing budget knob; `None` = the default/env value).
    pub(crate) tool_budget: Option<usize>,
}

/// Apply one drained queue message to the not-yet-injected `pending` list:
/// `Content` appends, `Recall` removes the newest matching item. Recalls are
/// applied in arrival order, so a recall can only cancel an item that has not
/// been injected yet — one already sent is part of the transcript.
pub(crate) fn apply_queue_msg(pending: &mut Vec<String>, msg: QueueMsg) {
    match msg {
        QueueMsg::Content(text) => pending.push(text),
        QueueMsg::Recall(text) => {
            if let Some(pos) = pending.iter().rposition(|item| item == &text) {
                pending.remove(pos);
            }
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
    while compaction_attempts < 3 {
        let eff = ledger.stored_tokens() + budget_overhead;
        let need_by_tokens = eff > config.compaction_threshold();
        let need_by_count = messages.len() > 1 + KEEP_RECENT_MESSAGES;
        if !need_by_tokens && !need_by_count {
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
            Err(e) if e.contains("cancelled") => return Err(e.into()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub(crate) async fn process_turn<C, X>(
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
    } = rt;
    let result = process_turn_inner(ProcessTurnArgs {
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

/// The unbundled runtime `process_turn_inner` works on: same fields as
/// [`AgentRuntime`], destructured once in `process_turn` so the turn wrapper
/// keeps mutable access to `messages` after the call (lifecycle-hook
/// restoration).
struct ProcessTurnArgs<'a, C, X> {
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
    agent_ctx: Option<Arc<crate::agent::subagent::AgentTurnContext>>,
    tool_budget: Option<usize>,
}

#[allow(clippy::too_many_arguments)]
async fn process_turn_inner<C, X>(
    ProcessTurnArgs {
        config,
        messages,
        state,
        mut steering_rx,
        steering_accepted_tx,
        mut session,
        client,
        cancel,
        console,
        filter,
        agent_ctx,
        tool_budget,
    }: ProcessTurnArgs<'_, C, X>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    C: ModelClient + 'static,
    X: CancellationSource + Clone + 'static,
{
    let _working = SpinnerGuard::start(console, "Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;
    let cancellation = cancel;
    let mut persisted_cursor = messages.len();
    let tool_budget = tool_budget.unwrap_or_else(max_tool_iterations);
    let mut tool_iterations = 0usize;
    // One context-overflow retry per turn: after an emergency compaction the
    // model call is re-issued exactly once; a second overflow is a real
    // failure (single message too large), not something more slicing fixes.
    let mut overflow_retried = false;
    let mut budget_warned = false;
    // Phase 0 gate context: every tool call this turn runs under the
    // turn's permission mode + approval channel. One policy for the whole
    // turn so same-turn allow-for-session records are shared. The daemon
    // context (Phase 5) rides along for the delegation tools.
    let mut policy = Policy::turn(config.permission, console);
    policy.agent = agent_ctx;
    // Running token total over the stored history: one full walk per turn
    // (here); appended messages update it, compaction rebuilds it. Every
    // per-iteration budget below reads this instead of re-walking history.
    let mut ledger = TokenLedger::rebuild(messages);

    loop {
        persist_pending(&mut session, messages, &mut persisted_cursor)?;
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
        }
        if let Some(rx) = steering_rx.as_mut() {
            let injected = inject_steering(rx, steering_accepted_tx, messages).await;
            if injected {
                // Steering appends user messages outside the tracked pushes.
                ledger = TokenLedger::rebuild(messages);
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
            }
        }

        // Proactive compaction BEFORE model call
        // Ephemeral MCP status line: priced in the budget below but never
        // stored in `messages` (call-time preamble, not transcript).
        // Sync snapshot, never initializes the manager: no MCP tools are
        // in the schema before bootstrap either, so the budget stays exact
        // and a budget probe never spawns the background refresh.
        let ephemerals = [crate::mcp::ephemeral_line()];
        // Stored-budget overhead for the gate: the gate re-derives
        // ledger + overhead per attempt (see `compaction_gate`).
        let budget_overhead = estimate_ephemeral_tokens(&ephemerals) + schema_budget_tokens();
        compaction_gate(
            config,
            console,
            messages,
            budget_overhead,
            cancellation,
            session.as_deref_mut(),
            &mut persisted_cursor,
            state,
            &mut ledger,
        )
        .await?;

        // Async LLM call with prompt cancel: `select!(cancelled, complete)`
        // wakes within ~10ms.
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let call_started = std::time::Instant::now();
        let wire: &[ChatMessage] = messages;
        let turn: Turn = tokio::select! {
            _ = wait_cancelled(cancel_ref) => {
                return Err("cancelled by user".into());
            }
            r = client.complete(wire, true, console.sink().cloned(), cancel_ref) => match r {
                Ok(result) => result,
                Err(e) => {
                    let msg = e.to_string();
                    if msg == "interrupted" || msg == "cancelled" {
                        return Err("cancelled by user".into());
                    }
                    // The provider rejected the request because the input no
                    // longer fits: emergency-compact and re-issue once
                    // instead of failing the whole turn. The proactive
                    // compaction above runs on an estimate; real provider
                    // limits (tool schemas, a huge single tool result) can
                    // still overshoot it.
                      if !overflow_retried && is_context_overflow(&msg) {
                          overflow_retried = true;
                          match emergency_compact(
                              config,
                              messages,
                              state,
                              cancellation,
                              console,
                              &mut ledger,
                          )
                          .await
                          {
                            Ok(true) => {
                                rewrite_session(
                                    session.as_deref_mut(),
                                    messages,
                                    &mut persisted_cursor,
                                )?;
                                continue;
                            }
                            _ => return Err(msg.into()),
                        }
                    }
                    return Err(msg.into());
                }
            },
        };
        // Whole-call wall clock (connect + first token + stream): the honest
        // denominator for the footer's output tokens/s rate.
        let gen_ms = u64::try_from(call_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(u) = turn.usage {
            last_usage = Some(u.prompt_tokens);
            record_usage(config, state, console, u, Some(gen_ms)).await;
        }
        // The provider cut the reply off mid-generation (output-token limit
        // or a content filter): whatever landed is likely incomplete. Say so
        // instead of silently keeping a truncated reply as if it were complete.
        let truncation = match turn.stop_reason {
            Some(StopReason::Length) => {
                Some("model output hit the output-token limit and may be truncated")
            }
            Some(StopReason::ContentFilter) => {
                Some("model output was cut off by a content filter and may be incomplete")
            }
            _ => None,
        };
        if let Some(note) = truncation {
            note_sink(
                console,
                || SinkLine::System(note.to_string()),
                || format!("[dex] {note}"),
            )
            .await;
        }
        let mut message = turn.message;

        if let Some(calls) = message.tool_calls.take() {
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
                reasoning_items: message.reasoning_items,
                reasoning_content: message.reasoning_content,
            });
            ledger.push(messages.last().expect("just pushed"));

            let results = run_tool_batch(&calls, cancel, &policy, filter, console).await;

            // Cancel landed during tool IO: the per-tool "cancelled"
            // errors above are shutdown noise, not model input. Suppress
            // the fan-out and unwind — the turn was going to abort at the
            // next loop-top check anyway, and skipping the persist keeps
            // a transcript the model never saw out of the session.
            if cancellation.is_cancelled() {
                let _ = cancellation.take_cancelled();
                // Every call already announced its start, so close each
                // open block explicitly — otherwise the TUI spinner (and
                // any remote transcript) lingers on calls that will never
                // complete.
                for call in &calls {
                    let name = call.function.name.clone();
                    note_sink(
                        console,
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
                return Err("cancelled by user".into());
            }

            // Hoisted: one getcwd per iteration, not per tool result.
            let turn_cwd = std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            {
                let mut ctx = ToolResultCtx {
                    console,
                    state: &mut *state,
                    messages: &mut *messages,
                    session: &mut session,
                    persisted_cursor: &mut persisted_cursor,
                    last_tools: &mut last_tools,
                    ledger: &mut ledger,
                };
                for (call, (name, input, outcome, elapsed)) in calls.iter().zip(results) {
                    process_tool_result(&mut ctx, call, &turn_cwd, &name, &input, outcome, elapsed)
                        .await?;
                }
            }
            // Per-turn tool budget: a model that churns without converging
            // (rephrasing the same failing call, ping-ponging two files)
            // burns unbounded tokens; the repeated-call detector only stops
            // bit-identical repeats. The completed batch above is persisted
            // first, so the transcript stays coherent for the next prompt.
            // Counts batches (rounds), not individual calls: one fan-out of
            // N parallel calls is one round.
            tool_iterations += 1;
            if tool_iterations >= tool_budget {
                let note = format!(
                    "turn budget exhausted after {tool_iterations} tool rounds; partial progress preserved — send another prompt to continue"
                );
                // No sink line here: the Err below surfaces the note exactly
                // once on every surface (`agent error:` headless, TurnFailed
                // in the daemon transcript and events journal).
                // Leave a transcript marker so the resume shows why the
                // turn stopped (User-role + name tag, like steering/summary).
                messages.push(ChatMessage::user_named(note.clone(), "budget"));
                ledger.push(messages.last().expect("just pushed"));
                let _ = persist_pending(&mut session, messages, &mut persisted_cursor);
                return Err(note.into());
            }
            if !budget_warned && tool_iterations * 5 >= tool_budget * 4 {
                budget_warned = true;
                let note = format!("{tool_iterations}/{tool_budget} tool rounds used this turn");
                system_note(console, &note).await;
            }
            // Write-through persist (best-effort, tiny JSON): awaited so a
            // process exit right after the turn can't lose it — a detached
            // spawn would be dropped on shutdown before it ever ran. Clone
            // keeps the saved field list compiler-enforced (state.rs disables
            // dead_code lints, so a hand-written literal could forget one).
            if state.dirty {
                state.save_async().await;
                state.dirty = false;
            }
        } else {
            let text = message.content.unwrap_or_default();
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: Some(text.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning_items: message.reasoning_items,
                reasoning_content: message.reasoning_content,
            });
            ledger.push(messages.last().expect("just pushed"));
            if let Some(rx) = steering_rx.as_mut() {
                let injected = inject_steering(rx, steering_accepted_tx, messages).await;
                if injected {
                    state.last_usage = last_usage;
                    // Steering appended outside the tracked pushes.
                    ledger = TokenLedger::rebuild(messages);
                    continue;
                }
            }
            state.last_usage = last_usage;
            persist_pending(&mut session, messages, &mut persisted_cursor)?;
            return Ok(text);
        }
    }
    // The `loop` above never breaks — every path returns or continues — so
    // this expression is unreachable; kept for exhaustiveness (AGT-2), the
    // failure wording stays documented at the end of the turn pipeline.
    #[allow(unreachable_code)]
    Err("turn did not complete after many tool iterations; partial progress preserved.".into())
}

mod tools;

use tools::{
    emergency_compact, inject_steering, is_context_overflow, max_tool_iterations, note_sink,
    persist_pending, process_tool_result, record_usage, rewrite_session, run_tool_batch,
    system_note, ToolResultCtx,
};

#[cfg(test)]
pub(crate) mod tests;
