//! Dex tool execution, history persistence, steering, and shared turn notes.
//! Completed execution results are applied by the sibling `tool_results`
//! module so dispatch stays separate from transcript/session bookkeeping.

use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::apply_queue_msg;
use crate::agent::compaction::compact_history;
use crate::agent::compaction::verbatim::summary_mode;
use crate::agent::composable::{DexHarness, ToolExecutor};
use crate::agent::state::{wait_cancelled, CancellationSource, ToolState};
use crate::agent::tokens::TokenLedger;
use crate::llm::config::LlmConfig;
use crate::protocol::{ChatMessage, LlmToolCall, QueueMsg, SinkLine, Usage};
use crate::render::format::short_arg;
use crate::runtime::console::{with_console, Console, RESET, TOOL_INPUT_COLOR, TOOL_MUTATION_LOCK};
use crate::runtime::unwind::CatchUnwind;
use crate::session::Session;
use crate::tools::{Policy, ToolFilter, ToolOutcome};

/// Emit a system note on every surface: a transcript line when a sink is
/// attached (TUI / daemon), `eprintln` headless.
pub(crate) async fn system_note(console: &Console, note: &str) {
    note_sink(
        console,
        || SinkLine::System(note.to_string()),
        || format!("[dex] {note}"),
    )
    .await;
}

/// Emit one line on every surface: the sink line when a sink is attached
/// (TUI / daemon), `eprintln` headless (console IO always runs — the
/// headless path must not swallow output). Both arms are lazy: the unused
/// side is never built.
pub(super) async fn note_sink(
    console: &Console,
    line: impl FnOnce() -> SinkLine,
    plain: impl FnOnce() -> String,
) {
    if console.sink().is_some() {
        console.emit_async(line()).await;
    } else {
        with_console(false, || eprintln!("{}", plain()));
    }
}

pub(crate) fn tool_calls_conflict(calls: &[LlmToolCall]) -> bool {
    let mut paths = std::collections::HashSet::new();
    calls.iter().any(|call| {
        // Fail closed: unparseable args serialize the batch rather than
        // risk a concurrent same-file race we couldn't check.
        let Ok(value) = serde_json::from_str::<Value>(&call.function.arguments) else {
            return true;
        };
        // `bash`, and any `write`/`edit` carrying `then_run`, spawn a shell
        // command whose file effects we cannot see from `path` alone. Force
        // the whole batch through the mutation lock rather than fan out
        // several concurrent shells — a case the scheduler never had to
        // consider while these calls were pure file writes.
        let name = call.function.name.as_str();
        if name == "bash" || crate::tools::then_run::carries_then_run(name, &value) {
            return true;
        }
        // Calls without a `path` can't be checked — they never force
        // serialization on their own.
        let Some(path) = value.get("path").and_then(Value::as_str) else {
            return false;
        };
        // Normalize before comparing: `./foo.rs` and `foo.rs` (or an
        // absolute vs relative spelling of one file) must collide, or the
        // fan-out runs two edits against one file concurrently and the
        // second silently clobbers the first.
        !paths.insert(crate::tools::normalize_conflict_path(path))
    })
}

pub(crate) fn persist_pending(
    session: &mut Option<&mut Session>,
    messages: &[ChatMessage],
    cursor: &mut usize,
) -> std::io::Result<()> {
    if let Some(session) = session.as_deref_mut() {
        for message in messages.get(*cursor..).unwrap_or_default() {
            session.append_message(message)?;
        }
        *cursor = messages.len();
    }
    Ok(())
}

/// Re-persist the (compacted) history: atomically replace the session file
/// with header + `clear` + every message after the system prompt.
/// Single-sources the journal invariant that the file mirrors `messages`
/// after any compaction rewrites it (threshold gate, emergency, online
/// boundary). Atomic (`Session::rewrite_messages`): readers never see a
/// torn clear-plus-partial-tail.
pub(crate) fn rewrite_session(
    session: Option<&mut Session>,
    messages: &[ChatMessage],
    persisted_cursor: &mut usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(session) = session {
        session.rewrite_messages(messages)?;
    }
    *persisted_cursor = messages.len();
    Ok(())
}

/// Drain queued steering messages and inject them as user-role messages,
/// notifying the accepted channel per message. Returns true when any
/// steering was injected (the caller records the horizon correction and
/// persists).
pub(super) async fn inject_steering(
    rx: &mut mpsc::Receiver<QueueMsg>,
    accepted: Option<&mpsc::Sender<String>>,
    messages: &mut Vec<ChatMessage>,
) -> bool {
    let mut drained: Vec<String> = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        apply_queue_msg(&mut drained, msg);
    }
    if drained.is_empty() {
        return false;
    }
    for content in drained {
        if let Some(accepted) = accepted {
            let _ = accepted.send(content.clone()).await;
        }
        messages.push(ChatMessage::user_named(content, "steering"));
    }
    true
}

/// Shared per-call accounting: live context usage in `state`, the sink
/// event the TUI accumulates spend from (carrying the daemon-priced USD
/// cost, so the remote client never re-prices locally), and the
/// session-cumulative USD cost. Also used for compaction summarizer calls,
/// which are billed too. `gen_ms` is the caller-measured wall-clock
/// duration of the LLM call (`None` when untimed, e.g. compaction): it
/// becomes the footer's tokens/s denominator on the client.
pub(crate) async fn record_usage(
    config: &LlmConfig,
    state: &mut ToolState,
    console: &Console,
    u: Usage,
    gen_ms: Option<u64>,
) {
    state.last_usage = Some(u.prompt_tokens);
    state.last_cached = u.cached_tokens;
    // Session-cumulative totals feed the one-shot stderr summary; the TUI
    // accumulates the same Usage sink lines client-side (separate state).
    state.total_usage = state.total_usage.saturating_add(u.prompt_tokens);
    state.total_output = state.total_output.saturating_add(u.completion_tokens);
    let cost =
        crate::llm::config::usage_cost(&config.model, &config.provider, &config.base_url, &u)
            .unwrap_or_else(|| {
                #[allow(clippy::cast_precision_loss)]
                let rate_per_1k = std::env::var("DEX_COST_PER_1K")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.002);
                #[allow(clippy::cast_precision_loss)]
                {
                    (u.prompt_tokens + u.completion_tokens) as f64 * rate_per_1k / 1000.0
                }
            });
    console
        .emit_async(SinkLine::Usage {
            tokens: u.prompt_tokens,
            cached: u.cached_tokens,
            cost,
            output: u.completion_tokens,
            gen_ms,
        })
        .await;
    state.total_cost += cost;
}

pub(super) async fn execute_tool_call(
    call: &LlmToolCall,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
    executor: &dyn ToolExecutor,
) -> (String, String, ToolOutcome) {
    let name = call.function.name.clone();
    let raw_args = call.function.arguments.clone();
    let value: Value = match serde_json::from_str(&raw_args) {
        Ok(value) => value,
        Err(error) => {
            return (
                name,
                raw_args,
                ToolOutcome {
                    text: format!("Error: invalid tool arguments: {}", error),
                    ok: false,
                    diff: None,
                },
            )
        }
    };
    let Some(args) = value.as_object() else {
        return (
            name,
            raw_args,
            ToolOutcome {
                text: "Error: tool arguments must be a JSON object".into(),
                ok: false,
                diff: None,
            },
        );
    };
    let input = serde_json::to_string(args).unwrap_or_default();
    dex_runtime::log!(Debug, "tool {name} {input}");
    let started = Instant::now();
    let outcome = executor
        .execute_outcome(&name, args, cancel, policy, filter)
        .await;
    dex_runtime::log!(
        Debug,
        "tool {name} ok={} in {:?}",
        outcome.ok,
        started.elapsed()
    );
    (name, input, outcome)
}

/// Force up to the harness recovery budget of compaction rounds regardless
/// of the token threshold — the provider has already said the input is over
/// the real limit, so the estimator's opinion no longer matters. Returns
/// true when history shrank.
pub(super) async fn emergency_compact(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
    cancel: &(dyn CancellationSource + Send + Sync),
    console: &Console,
    ledger: &mut TokenLedger,
    harness: &DexHarness,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut compacted_any = false;
    for _ in 0..harness.recovery_attempts() {
        // Emergency cuts follow the threshold knob (`DEX_COMPACTION`):
        // one parse selects both the prune and the fallback summarizer.
        let summarizer = summary_mode();
        match compact_history(
            config,
            messages,
            cancel,
            true,
            summarizer.prunes_jev(),
            summarizer,
            harness,
        )
        .await
        {
            Ok((true, usage)) => {
                compacted_any = true;
                if let Some(u) = usage {
                    harness.usage.record(config, state, console, u, None).await;
                }
            }
            _ => break,
        }
    }
    let note = if compacted_any {
        "context overflow: compacted history and retrying the model call once"
    } else {
        "context overflow: nothing compactable; the input (likely one message or tool result) is too large"
    };
    // Compaction may have rewritten history: re-measure once instead of
    // tracking per-round deltas on this rare path.
    *ledger = TokenLedger::rebuild(messages);
    harness.events.system_note(console, note).await;
    Ok(compacted_any)
}

/// Announce one tool call the moment it starts executing — not when it
/// finishes — so every surface shows the in-progress call: the TUI opens
/// the tool block, the daemon forwards `tool_call` for remote TUIs,
/// headless prints `[tool input]`, and child-agent progress labels update.
/// The preview arg is the raw call JSON; `short_arg` reduces it the same
/// way the completion path does once the normalized input is known, so
/// the two lines agree. The matching `ToolOutput` (same `id`) completes
/// the block later, even when parallel batches interleave.
pub(super) async fn note_tool_start(console: &Console, call: &LlmToolCall) {
    let name = call.function.name.clone();
    let args = call.function.arguments.clone();
    let short = short_arg(&name, &args);
    note_sink(
        console,
        || SinkLine::ToolInput {
            id: call.id.clone(),
            input: format!("{name} {short}"),
        },
        || format!("{TOOL_INPUT_COLOR}[tool input] {name} {short}{RESET}",),
    )
    .await;
}

/// Execute one batch of tool calls: serialized under the mutation lock when
/// the harness conflict detector fires, else fanned out on JoinSet tasks
/// (bounded to the harness batch-concurrency permits, aborted promptly on
/// cancel; input-ordered via indexed slots, panics surface as tool errors).
pub(super) async fn run_tool_batch<X>(
    calls: &[LlmToolCall],
    cancel: &X,
    policy: &Policy,
    filter: Option<&ToolFilter>,
    console: &Console,
    harness: &DexHarness,
) -> Vec<(String, String, ToolOutcome, Duration)>
where
    X: CancellationSource + Clone + 'static,
{
    // Runtime override: one `harness.conflict` round-trip per batch when a Lua
    // extension subscribes, else the Rust detector (zero-cost default).
    let conflicts = if crate::extensions::has_event_handlers("harness.conflict") {
        crate::extensions::query_harness_conflict(
            calls,
            cancel as &(dyn CancellationSource + Send + Sync),
        )
        .await
        .unwrap_or_else(|| harness.conflicts(calls))
    } else {
        harness.conflicts(calls)
    };
    if conflicts {
        let _guard = TOOL_MUTATION_LOCK.lock().await;
        let mut out = Vec::new();
        for call in calls {
            if cancel.is_cancelled() {
                out.push((
                    call.function.name.clone(),
                    call.function.arguments.clone(),
                    ToolOutcome {
                        text: "Error: cancelled by user".into(),
                        ok: false,
                        diff: None,
                    },
                    Duration::ZERO,
                ));
                continue;
            }
            let started = Instant::now();
            note_tool_start(console, call).await;
            let (name, input, outcome) = execute_tool_call(
                call,
                cancel as &(dyn CancellationSource + Send + Sync),
                policy,
                filter,
                &*harness.executor,
            )
            .await;
            out.push((name, input, outcome, started.elapsed()));
        }
        out
    } else {
        // Bounded fan-out on a JoinSet: tasks still run concurrently (total
        // ~max, not sum), each result carries its own index so the transcript
        // stays in input order. A semaphore caps fd/thread pressure no matter
        // how many calls the model packed into one batch; `select!` on
        // `wait_cancelled` aborts the stragglers instead of waiting for the
        // slowest tool after Ctrl+C. The bound comes from the harness so the
        // fan-out is overwritable without forking the scheduler.
        let batch_max = harness.config.batch_max_concurrent();
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(batch_max));
        let executor = harness.executor.clone();
        let mut set = tokio::task::JoinSet::new();
        for (idx, call) in calls.iter().enumerate() {
            let call = call.clone();
            let cancel = cancel.clone();
            let policy = policy.clone();
            // Owned per task: the future must be 'static, so it
            // cannot hold the turn's borrowed filter — same shape
            // as the per-task policy clone above.
            let filter = filter.cloned();
            // Each task announces its own start (paired later by call
            // id), but only as semaphore permits free up: ~max blocks
            // open at once, in permit order rather than input order.
            let task_console = console.clone();
            let sem = sem.clone();
            let executor = executor.clone();
            set.spawn(async move {
                // The semaphore is never closed, but fail closed with
                // the index intact rather than run unpermitted.
                let Ok(_permit) = sem.acquire_owned().await else {
                    return (
                        idx,
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                        ToolOutcome {
                            text: "Error: tool worker missing".into(),
                            ok: false,
                            diff: None,
                        },
                        Duration::ZERO,
                    );
                };
                let started = Instant::now();
                note_tool_start(&task_console, &call).await;
                // `CatchUnwind` keeps the index on the panic path: a
                // panicking worker reports against its own call instead
                // of landing on a positional guess (JoinSet's JoinError
                // carries no task payload).
                let work = async {
                    let (name, input, outcome) =
                        execute_tool_call(&call, &cancel, &policy, filter.as_ref(), &*executor)
                            .await;
                    (name, input, outcome, started.elapsed())
                };
                match CatchUnwind::new(Box::pin(work), "tool worker panicked").await {
                    Ok((name, input, outcome, elapsed)) => (idx, name, input, outcome, elapsed),
                    Err(text) => (
                        idx,
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                        ToolOutcome {
                            text: format!("Error: {text}"),
                            ok: false,
                            diff: None,
                        },
                        Duration::ZERO,
                    ),
                }
            });
        }
        let mut slots: Vec<Option<(String, String, ToolOutcome, Duration)>> =
            Vec::with_capacity(calls.len());
        slots.resize_with(calls.len(), || None);
        let mut remaining = calls.len();
        let cancel_ref = cancel as &(dyn CancellationSource + Send + Sync);
        while remaining > 0 {
            tokio::select! {
                res = set.join_next() => {
                    match res {
                        Some(Ok((idx, name, input, outcome, elapsed))) => {
                            slots[idx] = Some((name, input, outcome, elapsed));
                            remaining -= 1;
                        }
                        Some(Err(_)) => {
                            // Only panics outside the `CatchUnwind`
                            // wrapper reach here (permit acquire, start
                            // announce): attribute to the first empty
                            // slot. Panics are rare, count stays exact.
                            if let Some(hole) = slots.iter().position(|s| s.is_none()) {
                                let call = &calls[hole];
                                slots[hole] = Some((
                                    call.function.name.clone(),
                                    call.function.arguments.clone(),
                                    ToolOutcome {
                                        text: "Error: tool worker panicked".into(),
                                        ok: false,
                                        diff: None,
                                    },
                                    Duration::ZERO,
                                ));
                            }
                            remaining -= 1;
                        }
                        None => break,
                    }
                }
                _ = wait_cancelled(cancel_ref) => {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    for (i, slot) in slots.iter_mut().enumerate() {
                        if slot.is_none() {
                            let call = &calls[i];
                            *slot = Some((
                                call.function.name.clone(),
                                call.function.arguments.clone(),
                                ToolOutcome {
                                    text: "Error: cancelled by user".into(),
                                    ok: false,
                                    diff: None,
                                },
                                Duration::ZERO,
                            ));
                        }
                    }
                    break;
                }
            }
        }
        slots
            .into_iter()
            .enumerate()
            .map(|(i, s)| {
                s.unwrap_or_else(|| {
                    let call = &calls[i];
                    (
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                        ToolOutcome {
                            text: "Error: tool worker missing".into(),
                            ok: false,
                            diff: None,
                        },
                        Duration::ZERO,
                    )
                })
            })
            .collect()
    }
}
