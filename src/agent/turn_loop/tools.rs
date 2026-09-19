//! Tool-execution half of the turn loop: tool-call batching, per-result
//! bookkeeping (cache, transcript), steering injection, and the
//! console/sink note emitters.

use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use super::apply_queue_msg;
use crate::agent::compaction::compact_history;
use crate::agent::jev::summary_mode;
use crate::agent::state::{cache_fingerprint, CancellationSource, ToolState};
use crate::agent::tokens::TokenLedger;
use crate::llm::config::LlmConfig;
use crate::protocol::{ChatMessage, LlmToolCall, QueueMsg, SinkLine, Usage};
use crate::runtime::console::{
    with_console, Console, RESET, TOOL_INPUT_COLOR, TOOL_MUTATION_LOCK, TOOL_OUTPUT_COLOR,
};
use crate::session::Session;
use crate::tools::{execute_outcome, Policy, ToolFilter, ToolOutcome};
use crate::ui::format::{
    model_tool_result, short_arg, tool_preview, tool_preview_body, tool_result_summary,
};

/// Emit a system note on every surface: a transcript line when a sink is
/// attached (TUI / daemon), `eprintln` headless.
pub(super) async fn system_note(console: &Console, note: &str) {
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
        if call.function.name == "bash" || carries_then_run(&value) {
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

/// Whether this call carries an executable `then_run` verification command.
/// Mirrors the resolver in `tools::then_run_command`: only a non-empty string
/// runs a shell, so a null / blank / wrong-typed field stays a plain write
/// that can fan out in parallel.
fn carries_then_run(value: &Value) -> bool {
    value
        .get("then_run")
        .and_then(Value::as_str)
        .is_some_and(|command| !command.trim().is_empty())
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
pub(super) fn rewrite_session(
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

/// Per-turn cap on tool rounds (one round = one assistant batch with tool
/// calls, regardless of how many calls the batch fans out). A model that
/// loops (re-issuing the same failing call in new words, ping-ponging two
/// files) burns unlimited tokens without one; the repeated-call detector
/// only stops *identical* calls. Bounded, preserved partial progress; the
/// user can continue with another prompt. `DEX_MAX_TOOL_ITERATIONS`
/// overrides.
pub(super) fn max_tool_iterations() -> usize {
    std::env::var("DEX_MAX_TOOL_ITERATIONS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
}

/// Phrases meaning "the input no longer fits the context window",
/// matched on a lowercased message by `is_context_overflow`.
const OVERFLOW_PHRASES: &[&str] = &[
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

/// Provider wording for "the input no longer fits the context window".
/// Matched on lowercase; providers phrase it many ways. The generic
/// "reduce the length" only counts with a context/token/prompt/input
/// anchor so unrelated length validations (filenames, etc.) don't trigger
/// a wasteful emergency compaction.
pub(super) fn is_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    OVERFLOW_PHRASES.iter().any(|p| message.contains(*p))
        || (message.contains("reduce the length")
            && (message.contains("context")
                || message.contains("token")
                || message.contains("prompt")
                || message.contains("input")))
}

/// Shared per-call accounting: live context usage in `state`, the sink
/// event the TUI accumulates spend from (carrying the daemon-priced USD
/// cost, so the remote client never re-prices locally), and the
/// session-cumulative USD cost. Also used for compaction summarizer calls,
/// which are billed too. `gen_ms` is the caller-measured wall-clock
/// duration of the LLM call (`None` when untimed, e.g. compaction): it
/// becomes the footer's tokens/s denominator on the client.
pub(super) async fn record_usage(
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
    crate::log!(Debug, "tool {name} {input}");
    let started = Instant::now();
    let outcome = execute_outcome(&name, args, cancel, policy, filter).await;
    crate::log!(
        Debug,
        "tool {name} ok={} in {:?}",
        outcome.ok,
        started.elapsed()
    );
    (name, input, outcome)
}

/// Force up to three compaction rounds regardless of the token threshold —
/// the provider has already said the input is over the real limit, so the
/// estimator's opinion no longer matters. Returns true when history shrank.
pub(super) async fn emergency_compact(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
    cancel: &(dyn CancellationSource + Send + Sync),
    console: &Console,
    ledger: &mut TokenLedger,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut compacted_any = false;
    for _ in 0..3 {
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
        )
        .await
        {
            Ok((true, usage)) => {
                compacted_any = true;
                if let Some(u) = usage {
                    record_usage(config, state, console, u, None).await;
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
    system_note(console, note).await;
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
/// the calls conflict, else fanned out on JoinSet tasks (input-ordered via
/// indexed results + sort; panics surface as tool errors).
pub(super) async fn run_tool_batch<X>(
    calls: &[LlmToolCall],
    cancel: &X,
    policy: &Policy,
    filter: Option<&ToolFilter>,
    console: &Console,
) -> Vec<(String, String, ToolOutcome, Duration)>
where
    X: CancellationSource + Clone + 'static,
{
    if tool_calls_conflict(calls) {
        let _guard = TOOL_MUTATION_LOCK.lock().await;
        let mut out = Vec::new();
        for call in calls {
            let started = Instant::now();
            note_tool_start(console, call).await;
            let (name, input, outcome) = execute_tool_call(
                call,
                cancel as &(dyn CancellationSource + Send + Sync),
                policy,
                filter,
            )
            .await;
            out.push((name, input, outcome, started.elapsed()));
        }
        out
    } else {
        // One `tokio::spawn` per call, awaited in input order: tasks still
        // run concurrently, but each handle stays paired with its own index,
        // so a panicking worker is attributed to its own call instead of
        // landing on a positional guess after a completion-order sort (S3).
        let mut handles = Vec::with_capacity(calls.len());
        for (idx, call) in calls.iter().enumerate() {
            let call = call.clone();
            let cancel = cancel.clone();
            let policy = policy.clone();
            // Owned per task: the future must be 'static, so it
            // cannot hold the turn's borrowed filter — same shape
            // as the per-task policy clone above.
            let filter = filter.cloned();
            // Same for the start-of-call announce below: each task emits
            // its own `ToolInput` (paired later by call id), so the TUI
            // opens every parallel block up front instead of only after
            // the whole batch completes.
            let task_console = console.clone();
            handles.push((
                idx,
                tokio::spawn(async move {
                    let started = Instant::now();
                    note_tool_start(&task_console, &call).await;
                    let (name, input, outcome) =
                        execute_tool_call(&call, &cancel, &policy, filter.as_ref()).await;
                    (name, input, outcome, started.elapsed())
                }),
            ));
        }
        let mut out = Vec::with_capacity(calls.len());
        for (idx, handle) in handles {
            match handle.await {
                Ok((name, input, outcome, elapsed)) => out.push((name, input, outcome, elapsed)),
                Err(_) => {
                    let call = &calls[idx];
                    out.push((
                        call.function.name.clone(),
                        call.function.arguments.clone(),
                        ToolOutcome {
                            text: "Error: tool worker panicked".into(),
                            ok: false,
                            diff: None,
                        },
                        Duration::ZERO,
                    ))
                }
            }
        }
        out
    }
}

/// Everything one tool result in a completed batch touches, bundled so
/// `process_tool_result` stays a plain function instead of a 7-arg one.
/// `'b` is the per-batch scope the mutable state is reborrowed for.
pub(super) struct ToolResultCtx<'a, 'b> {
    pub(super) console: &'a Console,
    pub(super) state: &'b mut ToolState,
    pub(super) messages: &'b mut Vec<ChatMessage>,
    pub(super) session: &'b mut Option<&'a mut Session>,
    pub(super) persisted_cursor: &'b mut usize,
    pub(super) last_tools: &'b mut Vec<String>,
    pub(super) ledger: &'b mut TokenLedger,
}

/// Process one tool result from a completed batch: repeated-call guard,
/// cache, sink emit, and transcript append. Moved verbatim from the inline
/// per-result loop body.
pub(super) async fn process_tool_result(
    ctx: &mut ToolResultCtx<'_, '_>,
    call: &LlmToolCall,
    turn_cwd: &str,
    name: &str,
    input: &str,
    outcome: ToolOutcome,
    elapsed: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let console: &Console = ctx.console;
    let state: &mut ToolState = ctx.state;
    let messages: &mut Vec<ChatMessage> = ctx.messages;
    let session: &mut Option<&mut Session> = ctx.session;
    let persisted_cursor: &mut usize = ctx.persisted_cursor;
    let last_tools: &mut Vec<String> = ctx.last_tools;
    let ledger: &mut TokenLedger = ctx.ledger;
    let cache_key = format!(
        "{}:{}:{}{}",
        turn_cwd,
        name,
        input,
        cache_fingerprint(name, input)
    );
    let succeeded = outcome.ok;
    // Captured before `outcome.text` is moved below: the
    // pre-mutation unified diff for write/edit results.
    let diff = outcome.diff.clone();
    // Occurrences counted AFTER the push: when the ring is full
    // the evicted front entry may itself be a match, so a
    // pre-push count over-counts by one and can trip the
    // `>= 3` guard a call early (regression:
    // repeated_tool_guard_counts_after_ring_eviction). The
    // filter borrows `cache_key`; that borrow ends before the
    // `state.insert` move below.
    if succeeded {
        if last_tools.len() >= 6 {
            last_tools.remove(0);
        }
        last_tools.push(cache_key.clone());
    }
    let repeated_count = last_tools.iter().filter(|k| **k == cache_key).count();
    // No `ToolInput` here: the start of the call was already announced by
    // `note_tool_start` when execution began (serial and parallel paths),
    // so the block is open long before this completion line lands.

    let cacheable = matches!(name, "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls");
    let mut cache_hit = false;
    let mut ok = succeeded;
    let result = if repeated_count >= 3 {
        ok = false;
        "Error: repeated identical tool call; choose a different action or finish.".to_string()
    } else if cacheable && succeeded {
        if let Some(cached) = state.cache.get(&cache_key) {
            cache_hit = true;
            cached.clone()
        } else {
            state.insert(cache_key, outcome.text.clone());
            outcome.text
        }
    } else {
        if matches!(name, "write" | "edit") {
            state.clear();
        }
        outcome.text
    };
    note_sink(
        console,
        || {
            let mut summary = tool_result_summary(name, input, &result, ok, diff.as_deref());
            if cache_hit {
                summary = format!("cached · {summary}");
            }
            let counts_only = matches!(
                name,
                "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "chain"
            );
            let skip_first = !counts_only || !ok;
            let preview = tool_preview(name, ok, diff.as_deref(), &result, skip_first);
            SinkLine::ToolOutput {
                // Pairs with the `ToolInput` announced at execution start:
                // UIs match by id so parallel-batch outputs land on their
                // own open block instead of the transcript tail.
                id: call.id.clone(),
                name: name.to_string(),
                summary,
                success: ok,
                preview,
                duration: elapsed.as_secs_f64(),
            }
        },
        || {
            let body = tool_preview_body(name, ok, diff.as_deref(), &result);
            format!(
                "{}[tool output] {}:\n{}{}",
                TOOL_OUTPUT_COLOR, name, body, RESET
            )
        },
    )
    .await;
    // The tool result lands before any system note so the transcript
    // stays assistant → tool_result → note.
    let mut result_message = ChatMessage::tool_result(call.id.clone(), model_tool_result(&result));
    // Internal-only metadata (`chat_completions_messages` strips `name`
    // from the wire): lets the transcript label which tool produced the
    // result.
    result_message.name = Some(name.to_string());
    messages.push(result_message);
    ledger.push(messages.last().expect("just pushed"));
    persist_pending(session, messages, persisted_cursor)?;
    Ok(())
}
