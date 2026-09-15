use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::agent::compaction::{
    compact_history, effective_tokens, estimate_tokens, KEEP_RECENT_MESSAGES,
};
use crate::agent::online_compaction::{
    cache_debt_for_ratio, decide_compaction, online_compaction_enabled, post_compaction_reminder,
    CompactionEconomics, DEFAULT_COMPACTION_ECONOMICS, NATIVE_SUMMARY_TOKEN_ESTIMATE,
};
use crate::agent::state::{cache_fingerprint, wait_cancelled, CancellationSource, ToolState};
use crate::core::console::{
    with_console, Console, SpinnerGuard, RESET, TOOL_INPUT_COLOR, TOOL_MUTATION_LOCK,
    TOOL_OUTPUT_COLOR,
};
use crate::core::format::{
    model_tool_result, short_arg, tool_preview, tool_preview_body, tool_result_summary,
};
use crate::core::types::{ChatMessage, LlmToolCall, QueueMsg, Role, SinkLine, StopReason, Usage};
use crate::llm::client::ModelClient;
use crate::llm::config::LlmConfig;
use crate::llm::stream::Turn;
use crate::session::Session;
use crate::tools::{execute_outcome, Policy, ToolFilter, ToolOutcome};

/// Emit a system note on every surface: a transcript line when a sink is
/// attached (TUI / daemon), `eprintln` headless.
async fn system_note(console: &Console, note: &str) {
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
async fn note_sink(
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

/// Re-persist the (compacted) history: clear the session file and rewrite
/// every message after the system prompt. Single-sources the journal
/// invariant that the file mirrors `messages` after any compaction
/// rewrites it (threshold gate, emergency, online boundary).
fn rewrite_session(
    session: Option<&mut Session>,
    messages: &[ChatMessage],
    persisted_cursor: &mut usize,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(session) = session {
        session.clear_messages()?;
        for message in messages.iter().skip(1) {
            session.append_message(message)?;
        }
    }
    *persisted_cursor = messages.len();
    Ok(())
}

/// Drain queued steering messages and inject them as user-role messages,
/// notifying the accepted channel per message. Returns true when any
/// steering was injected (the caller records the horizon correction and
/// persists).
async fn inject_steering(
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
fn max_tool_iterations() -> usize {
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
fn is_context_overflow(message: &str) -> bool {
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
/// Tokens a compaction can actually archive: the transcript minus the
/// system message (never summarized) and the larger of the keep-recent
/// token window and the `KEEP_RECENT_MESSAGES` message floor that
/// `compact_history` enforces. Bounds the economics' saving estimate to
/// what a cut can really remove.
fn archivable_tokens(messages: &[ChatMessage], config: &LlmConfig) -> u64 {
    let transcript = estimate_tokens(messages.get(1..).unwrap_or(&[]));
    let recent = estimate_tokens(
        messages
            .get(messages.len().saturating_sub(KEEP_RECENT_MESSAGES)..)
            .unwrap_or(&[]),
    );
    transcript.saturating_sub(recent.max(config.keep_recent_tokens()))
}

async fn record_usage(
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

async fn execute_tool_call(
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
                    shell: None,
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
                shell: None,
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
async fn emergency_compact(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
    cancel: &(dyn CancellationSource + Send + Sync),
    console: &Console,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut compacted_any = false;
    for _ in 0..3 {
        match compact_history(config, messages, cancel, true).await {
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
    system_note(console, note).await;
    Ok(compacted_any)
}

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
/// the (projected) context exceeds the token threshold or — without online
/// compaction — the message-count cap, at most three attempts. Threshold
/// compactions carry their cache re-write as debt the boundary economics
/// repay (math moved verbatim from the original inline block).
#[allow(clippy::too_many_arguments)]
async fn compaction_gate(
    config: &LlmConfig,
    console: &Console,
    messages: &mut Vec<ChatMessage>,
    projected: &[ChatMessage],
    ephemerals: &[Option<String>],
    cancel: &(dyn CancellationSource + Send + Sync),
    mut session: Option<&mut Session>,
    persisted_cursor: &mut usize,
    state: &mut ToolState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut compaction_attempts = 0;
    while compaction_attempts < 3 {
        let eff = effective_tokens(projected, ephemerals, true);
        let need_by_tokens = eff > config.compaction_threshold();
        // The message-count fallback is a global cap — exactly what the
        // online compaction economics replace. With the experiment on,
        // the count cap is dropped: short turns compact at plan
        // boundaries when economical, and the token threshold stays as
        // window protection.
        let need_by_count =
            !online_compaction_enabled() && messages.len() > 1 + KEEP_RECENT_MESSAGES;
        if !need_by_tokens && !need_by_count {
            break;
        }
        // Measure the re-write cost and the archivable slice before
        // `compact_history` rewrites `messages`.
        let online =
            online_compaction_enabled().then(|| (eff, archivable_tokens(messages, config)));
        match compact_history(config, messages, cancel, false).await {
            Ok((true, compacted)) => {
                compaction_attempts += 1;
                // Summarizer calls are billed like any other; account
                // them so the status-bar spend includes compaction.
                if let Some(u) = compacted {
                    record_usage(config, state, console, u, None).await;
                }
                rewrite_session(session.as_deref_mut(), messages, persisted_cursor)?;
                if let Some((write, archive)) = online {
                    // A threshold compaction bypasses the boundary
                    // economics, but the state must still see it:
                    // pressure samples reset, the re-write is carried
                    // as debt the next boundary repays, and the plan
                    // survives (the model was not asked to re-plan).
                    let (debt, repayment) =
                        cache_debt_for_ratio(write, archive, Some(config.cache_write_read_ratio()));
                    state
                        .online_compaction
                        .record_threshold_compaction(debt, repayment);
                }
                continue;
            }
            Ok((false, _)) => break,
            Err(e) if e.contains("cancelled") => return Err(e.into()),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Announce one tool call the moment it starts executing — not when it
/// finishes — so every surface shows the in-progress call: the TUI opens
/// the tool block, the daemon forwards `tool_call` for remote TUIs,
/// headless prints `[tool input]`, and child-agent progress labels update.
/// The preview arg is the raw call JSON; `short_arg` reduces it the same
/// way the completion path does once the normalized input is known, so
/// the two lines agree. The matching `ToolOutput` (same `id`) completes
/// the block later, even when parallel batches interleave.
async fn note_tool_start(console: &Console, call: &LlmToolCall) {
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
async fn run_tool_batch<X>(
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
                            shell: None,
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
/// `process_tool_result` stays a plain function instead of an 11-arg one.
/// `'a` is the turn-wide borrow (config, policy, cancel, ephemerals); `'b`
/// is the per-batch scope the mutable state is reborrowed for.
struct ToolResultCtx<'a, 'b> {
    config: &'a LlmConfig,
    console: &'a Console,
    state: &'b mut ToolState,
    policy: &'a Policy,
    messages: &'b mut Vec<ChatMessage>,
    session: &'b mut Option<&'a mut Session>,
    persisted_cursor: &'b mut usize,
    cancel: &'a (dyn CancellationSource + Send + Sync),
    ephemerals: &'b [Option<String>],
    online_boundary_handled: &'b mut bool,
    last_tools: &'b mut Vec<String>,
}

/// Process one tool result from a completed batch: repeated-call guard,
/// cache, evidence reducer, plan-boundary bookkeeping, sink emit, transcript
/// append, and (at most one per turn) the online compaction decision. Moved
/// verbatim from the inline per-result loop body.
async fn process_tool_result(
    ctx: &mut ToolResultCtx<'_, '_>,
    call: &LlmToolCall,
    turn_cwd: &str,
    name: &str,
    input: &str,
    outcome: ToolOutcome,
    elapsed: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config: &LlmConfig = ctx.config;
    let console: &Console = ctx.console;
    let policy: &Policy = ctx.policy;
    let cancel: &(dyn CancellationSource + Send + Sync) = ctx.cancel;
    let ephemerals: &[Option<String>] = ctx.ephemerals;
    let state: &mut ToolState = ctx.state;
    let messages: &mut Vec<ChatMessage> = ctx.messages;
    let session: &mut Option<&mut Session> = ctx.session;
    let persisted_cursor: &mut usize = ctx.persisted_cursor;
    let online_boundary_handled: &mut bool = ctx.online_boundary_handled;
    let last_tools: &mut Vec<String> = ctx.last_tools;
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
    let mut result = if repeated_count >= 3 {
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
    // Evidence-preserving reducer: delegate the first read of a
    // large build/test log to the configured reducer model and
    // verify every quoted line byte for byte against the archived
    // raw output. Any uncheckable receipt falls open: the raw
    // (clamped) result is kept untouched and the observation pack
    // handles it.
    let processed = crate::agent::evidence_reducer::process(
        config,
        policy
            .agent
            .as_ref()
            .map(|ctx| ctx.session_path.clone())
            .as_deref(),
        cancel,
        crate::agent::evidence_reducer::ToolResultView {
            call_id: call.id.as_str(),
            tool_name: name,
            input_json: input,
            result_text: &result,
            ok: succeeded,
            shell: outcome.shell.as_ref(),
        },
    )
    .await;
    // The reducer call's spend is real even when its receipt is
    // rejected: fold it into the session totals and the usage
    // stream, priced at the model that actually ran.
    let crate::agent::evidence_reducer::Processed {
        reduction,
        usage,
        pricing,
    } = processed;
    if let Some(usage) = usage {
        record_usage(
            pricing.as_ref().unwrap_or(config),
            state,
            console,
            usage,
            None,
        )
        .await;
        state.dirty = true;
    }
    if let Some(reduced) = reduction {
        result = reduced.receipt;
        system_note(
            console,
            &format!(
                "evidence reducer: {} -> {} (verified)",
                crate::agent::evidence_reducer::format_bytes(reduced.source_bytes),
                crate::agent::evidence_reducer::format_bytes(reduced.receipt_bytes),
            ),
        )
        .await;
    }
    // Online context compaction: a completed plan step is a boundary —
    // a safe point where history can be compacted if the economics say
    // the cache re-write pays for itself before the work ends. The
    // boundary bookkeeping runs *before* the sink emit so plan-hygiene
    // advice is part of the `result` the user sees; the compaction
    // decision itself runs after the tool result is appended so the
    // transcript keeps assistant → tool_result → reminder order (pi's
    // reference aborts the turn instead; dex compacts inline, and the
    // ordering must stay wire-valid). At most one boundary per turn is
    // evaluated.
    let boundary = crate::agent::online_compaction::capture_plan_update(
        &mut state.online_compaction,
        name,
        ok,
        input,
        &mut result,
    );
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
    // The tool result lands before any boundary reminder so the
    // transcript stays assistant → tool_result → reminder (see
    // the boundary note above).
    let mut result_message = ChatMessage::tool_result(call.id.clone(), model_tool_result(&result));
    // Internal-only metadata (`chat_completions_messages` strips `name`
    // from the wire): lets the observation pack label placeholders with the
    // producing tool and exempt `obs_recall` read-backs from re-packing.
    result_message.name = Some(name.to_string());
    messages.push(result_message);
    persist_pending(session, messages, persisted_cursor)?;
    if let Some(steps) = boundary {
        state.online_compaction.record_boundary(steps);
        if !*online_boundary_handled {
            *online_boundary_handled = true;
            let context_tokens = effective_tokens(messages, ephemerals, true);
            // The reference pins windowReserveTokens at a fixed
            // 16 KiB, independent of the host's compaction reserve;
            // dex's default reserve_tokens is also 16_384, and
            // following dex's configured edge keeps window
            // protection consistent with the pre-call compaction
            // threshold.
            let economics = CompactionEconomics {
                window_reserve_tokens: config.reserve_tokens,
                ..DEFAULT_COMPACTION_ECONOMICS
            };
            let decision = decide_compaction(
                context_tokens,
                // Archivable slice: what a cut can actually
                // remove (see `archivable_tokens`) — the system
                // message and the keep-recent window are never
                // archived, and the ephemeral preamble + tool
                // schema are re-sent on every request.
                archivable_tokens(messages, config),
                NATIVE_SUMMARY_TOKEN_ESTIMATE,
                context_tokens,
                &state.online_compaction,
                Some(config.context_window),
                Some(config.cache_write_read_ratio()),
                &economics,
            );
            crate::log!(
                Debug,
                "online compaction boundary: {} (write {}, archive {})",
                decision.reason,
                decision.write_tokens,
                decision.archive_tokens
            );
            if decision.compact {
                match compact_history(config, messages, cancel, false).await {
                    Ok((true, usage)) => {
                        if let Some(u) = usage {
                            record_usage(config, state, console, u, None).await;
                        }
                        rewrite_session(session.as_deref_mut(), messages, persisted_cursor)?;
                        // The compaction forces the retained prefix to
                        // be re-written at cache-write price on the next
                        // request; carry that as debt the following
                        // boundaries must repay before another compaction
                        // is economical.
                        let (debt, repayment) = decision.cache_debt();
                        // The reminder lists the remaining goals — build
                        // it before record_compaction clears the plan.
                        let reminder = post_compaction_reminder(
                            &state.online_compaction.plan,
                            &state.online_compaction.progress,
                        );
                        state.online_compaction.record_compaction(debt, repayment);
                        messages.push(ChatMessage::user_named(reminder, "compact"));
                        persist_pending(session, messages, persisted_cursor)?;
                        // The boundary fired silently before; a
                        // system line is the only user-visible
                        // proof the economics paid out.
                        system_note(
                            console,
                            &format!(
                                "online compaction: history compacted at plan boundary (~{} tokens archived)",
                                decision.archive_tokens
                            ),
                        )
                        .await;
                    }
                    // Below the summarize floor (e.g. a boundary
                    // right after the previous compaction) or a
                    // summarizer failure: nothing was cut, so no
                    // cache debt is carried — log why anyway.
                    Ok((false, _)) => {
                        crate::log!(Debug,
                            "online compaction boundary declined: history below the summarize floor"
                        );
                    }
                    Err(e) => {
                        crate::log!(Debug, "online compaction boundary failed: {e}");
                    }
                }
            }
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
            Some(first) if first.role == crate::core::types::Role::System => {
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
            if first.role == crate::core::types::Role::System {
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
    // Online compaction evaluates at most one plan boundary per turn (the
    // reference sets `pendingBoundary` on the first completed step and never
    // overwrites it); later completions in the same turn only feed the
    // horizon sample.
    let mut online_boundary_handled = false;
    // Phase 0 gate context: every tool call this turn runs under the
    // turn's permission mode + approval channel. One policy for the whole
    // turn so same-turn allow-for-session records are shared. The daemon
    // context (Phase 5) rides along for the delegation tools.
    let mut policy = Policy::turn(config.permission, console);
    policy.agent = agent_ctx;

    loop {
        persist_pending(&mut session, messages, &mut persisted_cursor)?;
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
        }
        if let Some(rx) = steering_rx.as_mut() {
            // A steer redirects the work: the horizon learned from completed
            // boundaries no longer describes the remaining effort.
            let injected = inject_steering(rx, steering_accepted_tx, messages).await;
            if injected {
                state.online_compaction.record_correction();
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
        // Observation pack projection: the provider-bound view replaces
        // stale large tool results with placeholders. Built fresh from
        // the intact history on every request; the stored session never
        // changes. Token accounting (compaction threshold, online
        // sampling) reads this view too — archived payloads must not
        // pressure the window estimate after their grace period expires.
        let obs_session = policy.agent.as_ref().map(|ctx| ctx.session_path.clone());
        let projected = crate::agent::obs_pack::project_messages(
            &state.obs_projection,
            obs_session.as_deref(),
            messages,
        );
        // Placeholder takeovers are otherwise invisible — the stored history
        // never changes — so surface each first takeover to the user.
        for note in state.obs_projection.take_notes() {
            system_note(console, &note).await;
        }
        compaction_gate(
            config,
            console,
            messages,
            &projected,
            &ephemerals,
            cancellation,
            session.as_deref_mut(),
            &mut persisted_cursor,
            state,
        )
        .await?;

        // Online compaction bookkeeping: sample the context size of
        // every provider request — the growth rate and the
        // per-boundary request counts feed the compaction economics.
        // Sampled on the projected view: only bytes actually sent count.
        crate::agent::online_compaction::sample_request(
            &mut state.online_compaction,
            effective_tokens(&projected, &ephemerals, true),
        );

        // Async LLM call with prompt cancel: `select!(cancelled, complete)`
        // wakes within ~10ms. No message `to_vec` clone beyond what the call
        // needs and no parked thread (S6 resource win).
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let call_started = std::time::Instant::now();
        let turn: Turn = tokio::select! {
            _ = wait_cancelled(cancel_ref) => {
                return Err("cancelled by user".into());
            }
            r = client.complete(&projected, true, console.sink().cloned(), cancel_ref) => match r {
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
                    config,
                    console,
                    state: &mut *state,
                    policy: &policy,
                    messages: &mut *messages,
                    session: &mut session,
                    persisted_cursor: &mut persisted_cursor,
                    cancel: cancellation,
                    ephemerals: &ephemerals,
                    online_boundary_handled: &mut online_boundary_handled,
                    last_tools: &mut last_tools,
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
            // dead_code lints, so a hand-written literal could forget one);
            // the projection is reset because a resume re-derives it from
            // scratch.
            if state.dirty {
                let mut to_save = state.clone();
                to_save.obs_projection = crate::agent::obs_pack::ProjectionState::new();
                to_save.save_async().await;
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
            if let Some(rx) = steering_rx.as_mut() {
                let injected = inject_steering(rx, steering_accepted_tx, messages).await;
                if injected {
                    state.online_compaction.record_correction();
                    state.last_usage = last_usage;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes tests that run `process_turn`: every turn reads
    /// `DEX_MAX_TOOL_ITERATIONS` at entry, and the budget test parks the
    /// var at "2" across its await — any concurrent `process_turn` would
    /// exhaust early. An async mutex because the guard must span awaits
    /// (clippy's `await_holding_lock` rejects the std one here).
    pub(crate) static TEST_TURN_ENV_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());
    use crate::agent::state::{CancellationSource, ToolState};
    use crate::core::types::{ApiProtocol, ChatMessage, PermissionMode, Provider};
    use crate::llm::client::ModelClient;

    #[derive(Clone)]
    struct MockModel;

    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Turn {
                message: ChatMessage::assistant("hello from mock"),
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[derive(Clone)]
    struct NeverCancel;

    impl CancellationSource for NeverCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn take_cancelled(&self) -> bool {
            false
        }
    }

    fn test_config() -> LlmConfig {
        LlmConfig {
            provider: Provider::OpenCode,
            api_key: String::new(),
            base_url: String::new(),
            model: "mock".into(),
            available_models: vec!["mock".into()],
            endpoints: Default::default(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: PermissionMode::Trusted,
            verify_command: None,
            extra_headers: Default::default(),
            client: reqwest::Client::new(),
            provider_entries: Default::default(),
            provider_headers: Default::default(),
            api_pinned: false,
        }
    }

    /// Session-cumulative totals accumulate across calls with saturating
    /// adds; zero-prompt usage still counts its output tokens (the one-shot
    /// spend summary gates on both totals).
    #[tokio::test]
    async fn record_usage_accumulates_session_totals() {
        let config = test_config();
        let mut state = ToolState::default();
        record_usage(
            &config,
            &mut state,
            &Console::none(),
            Usage {
                prompt_tokens: 10,
                completion_tokens: 4,
                cached_tokens: None,
            },
            None,
        )
        .await;
        record_usage(
            &config,
            &mut state,
            &Console::none(),
            Usage {
                prompt_tokens: 0,
                completion_tokens: 7,
                cached_tokens: None,
            },
            None,
        )
        .await;
        assert_eq!(state.total_usage, 10);
        assert_eq!(state.total_output, 11);
        assert_eq!(state.last_usage, Some(0));
    }

    /// `turn.start`/`turn.end` fire around the turn; a `turn.end` hook can
    /// act through the host (`dex.tools.call` runs gated, here trusted).
    #[tokio::test]
    async fn turn_lifecycle_hooks_fire_around_the_turn() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
            .lock()
            .await;
        // Workspace-confined: the hook writes a relative path in the test
        // process's cwd (the crate root).
        let marker = std::path::PathBuf::from(format!(
            "dex-turn-end-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let marker_str = marker.display().to_string();
        let lua = format!(
            "return function(dex)\n  dex.events.on(\"turn.end\", function(ctx, ev)\n    if not ev.ok then return end\n    dex.tools.call(\"write\", {{ path = \"{}\", content = \"done\" }})\n  end)\nend\n",
            marker_str
        );
        let manifest = "manifest_version: 1\nid: turn-hook\nversion: 0.1.0\ncapabilities: []\n";
        let root = crate::extensions::tests::fixture_exts(&[("turn-hook", manifest, &lua)]);
        crate::extensions::global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &MockModel,
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(
            marker.is_file(),
            "turn.end hook must have written the marker"
        );
        std::fs::remove_file(&marker).ok();
        std::fs::remove_dir_all(&root).ok();
        // Drop the fixture: the process-global manager would otherwise keep
        // the turn.end hook writing a marker file on every later turn in
        // this test process.
        crate::extensions::global_manager().reset_for_tests().await;
    }

    /// `before_agent_start` appends to the System prompt for the turn's
    /// model requests and is restored when the turn ends (per-turn scope).
    #[tokio::test]
    async fn before_agent_start_appends_system_prompt_and_restores() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
            .lock()
            .await;
        let lua = "return function(dex)\n  dex.events.on(\"before_agent_start\", function(ctx, ev)\n    return { append = \"PROMPT-MARKER\" }\n  end)\nend\n";
        let manifest = "manifest_version: 1\nid: promptmark\nversion: 0.1.0\ncapabilities: []\n";
        let root = crate::extensions::tests::fixture_exts(&[("promptmark", manifest, lua)]);
        crate::extensions::global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;

        // Recording client: capture the first request's system message.
        #[derive(Clone)]
        struct CaptureModel(Arc<std::sync::Mutex<Option<String>>>);
        impl ModelClient for CaptureModel {
            async fn complete(
                &self,
                messages: &[ChatMessage],
                _with_tools: bool,
                _sink: Option<mpsc::Sender<SinkLine>>,
                _cancel: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                let first = messages.first().map(|m| m.content.clone());
                *self.0.lock().unwrap() = first.flatten();
                Ok(Turn {
                    message: ChatMessage::assistant("done"),
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 0,
                        cached_tokens: None,
                    }),
                    stop_reason: None,
                })
            }
        }

        let config = test_config();
        let system_text = "base persona".to_string();
        let mut messages = vec![ChatMessage::system(&system_text)];
        let captured = Arc::new(std::sync::Mutex::new(None::<String>));
        let mut state = ToolState::default();
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &CaptureModel(captured.clone()),
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        assert!(result.is_ok(), "{result:?}");
        // The request the model saw carried the appendix...
        let seen = captured.lock().unwrap().clone().unwrap_or_default();
        assert!(
            seen.contains("PROMPT-MARKER") && seen.contains("base persona"),
            "system prompt must carry the appendix during the turn: {seen:?}"
        );
        // ...and the session's System message is restored afterwards.
        assert_eq!(
            messages.first().and_then(|m| m.content.clone()).as_deref(),
            Some("base persona"),
            "appendix must not persist past the turn"
        );
        std::fs::remove_dir_all(&root).ok();
        crate::extensions::global_manager().reset_for_tests().await;
    }

    #[tokio::test]
    async fn process_turn_completes_with_injected_client() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &MockModel,
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello from mock");
        assert!(messages
            .iter()
            .any(|m| m.content.as_deref() == Some("hello from mock")));
    }

    #[derive(Clone)]
    struct ToolThenAnswer {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolThenAnswer {
        fn new() -> Self {
            Self {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl ModelClient for ToolThenAnswer {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let message = if round == 0 {
                ChatMessage::assistant_calls(
                    None,
                    vec![crate::core::types::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"echo line-one; echo line-two; echo line-three; echo line-four"}"#.into(),
                        },
                    }],
                )
            } else {
                ChatMessage::assistant("done")
            };
            Ok(Turn {
                message,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn tool_result_streams_summary_preview_and_success() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let (sink_tx, mut sink_rx) = mpsc::channel(32);
        let (approval_tx, _approval_rx) = mpsc::channel(16);
        let _ = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &ToolThenAnswer::new(),
            cancel: &NeverCancel,
            console: &crate::core::console::Console::daemon(sink_tx, approval_tx),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        let mut events = Vec::new();
        while let Ok(e) = sink_rx.try_recv() {
            events.push(e);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, SinkLine::ToolInput { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, SinkLine::ToolOutput { .. })));
        // The start announce precedes the completion line, and both carry
        // the call id so the UI can pair them (the DEX-10 case: the input
        // must be visible while the tool still runs, not after).
        let input_pos = events
            .iter()
            .position(|e| matches!(e, SinkLine::ToolInput { .. }))
            .expect("tool input announced");
        let output_pos = events
            .iter()
            .position(|e| matches!(e, SinkLine::ToolOutput { .. }))
            .expect("tool output emitted");
        assert!(
            input_pos < output_pos,
            "tool input must precede its output: {events:?}"
        );
        let input_id = events.iter().find_map(|e| match e {
            SinkLine::ToolInput { id, .. } => Some(id.clone()),
            _ => None,
        });
        let output_id = events.iter().find_map(|e| match e {
            SinkLine::ToolOutput { id, .. } => Some(id.clone()),
            _ => None,
        });
        assert_eq!(input_id.as_deref(), Some("call-1"));
        assert_eq!(input_id, output_id);
    }

    /// Online context compaction end-to-end: a completed plan step is a
    /// boundary, and once the horizon (requests per boundary × remaining
    /// steps) covers the cache re-write breakeven, the history compacts and
    /// the re-plan reminder lands in the transcript.
    #[derive(Clone)]
    struct PlanThenCompact {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        big_path: String,
    }

    impl ModelClient for PlanThenCompact {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let call = |name: &str, args: String, id: &str| {
                ChatMessage::assistant_calls(
                    None,
                    vec![crate::core::types::LlmToolCall {
                        id: id.into(),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: name.into(),
                            arguments: args,
                        },
                    }],
                )
            };
            let message = match round {
                // Filler rounds before and after the big read: the
                // summarizer needs a multi-message span to cut (the
                // min-messages guard), and the rounds build the
                // requests-per-boundary sample.
                r if (0..10).contains(&r) => {
                    call("ls", "{}".to_string(), &format!("call-ls-{r}"))
                }
                10 => call(
                    "read",
                    format!(r#"{{"path":"{}"}}"#, self.big_path),
                    "call-read",
                ),
                r if (11..30).contains(&r) => {
                    call("ls", "{}".to_string(), &format!("call-ls-{r}"))
                }
                30 => call(
                    "update_plan",
                    r#"{"steps":[{"id":"1","goal":"read the big file","status":"completed"},{"id":"2","goal":"second","status":"pending"},{"id":"3","goal":"third","status":"pending"}]}"#
                        .to_string(),
                    "call-plan",
                ),
                _ => ChatMessage::assistant("done"),
            };
            Ok(Turn {
                message,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn online_compaction_compacts_at_an_economical_boundary() {
        // Shared env lock + panic-safe restore: the var is read by other
        // test modules in this binary (e.g. the protocol schema test), so
        // unsynchronized set/remove flakes those assertions.
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let _env = crate::session::EnvGuard(vec![(
            crate::agent::online_compaction::ONLINE_COMPACTION_ENV,
            std::env::var_os(crate::agent::online_compaction::ONLINE_COMPACTION_ENV),
        )]);
        std::env::set_var(crate::agent::online_compaction::ONLINE_COMPACTION_ENV, "1");
        // The read tool clamps a single file to ~64 KiB (~16k tokens), so
        // shrink keep_recent below that: the archive then clears the
        // breakeven at the first demonstrated boundary (31 requests, 2
        // steps remaining → horizon 63 vs breakeven ≈ 16).
        let mut config = test_config();
        config.keep_recent_tokens = 4_000;
        // ~200 KB of read output: far beyond keep_recent_tokens, so the
        // archive is large enough for the economics to fire at the first
        // demonstrated boundary (31 requests, 2 steps remaining).
        let big_path = "target/dex-online-compact-test.txt";
        let marker = "ONLINE-COMPACT-MARKER-7f3a";
        let big = format!(
            "{}\n",
            (0..2000)
                .map(|i| format!("line {i:0>4} {}", "x".repeat(90)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        std::fs::write(big_path, big).unwrap();

        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &PlanThenCompact {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                big_path: big_path.to_string(),
            },
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: Some(64),
        })
        .await;
        // Remove the fixture before asserting so a failed assert doesn't
        // leak it into `target/`.
        let _ = std::fs::remove_file(big_path);

        assert!(result.is_ok(), "{result:?}");
        assert_eq!(result.as_deref().unwrap(), "done");
        // The boundary compacted: exactly one compaction recorded, the
        // re-plan reminder is in the transcript, and the big read output is
        // gone from the context.
        assert_eq!(state.online_compaction.native_compaction_count(), 1);
        assert!(messages.iter().any(|m| {
            m.name.as_deref() == Some("compact")
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("call update_plan"))
        }));
        assert!(!messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains(marker))));
        // The remaining plan steps ride along on the reminder.
        let reminder = messages
            .iter()
            .find(|m| m.name.as_deref() == Some("compact"))
            .unwrap();
        assert!(reminder.content.as_deref().unwrap().contains("- second"));
    }

    #[tokio::test]
    async fn conflict_serialize_still_serializes_same_path_edits() {
        // TDD Phase 2: same-path edits must take the serialize path.
        let calls = vec![
            crate::core::types::LlmToolCall {
                id: "a".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "edit".into(),
                    arguments: r#"{"path":"same.rs"}"#.into(),
                },
            },
            crate::core::types::LlmToolCall {
                id: "b".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "edit".into(),
                    arguments: r#"{"path":"same.rs"}"#.into(),
                },
            },
        ];
        assert!(tool_calls_conflict(&calls));
        let different = vec![
            crate::core::types::LlmToolCall {
                id: "a".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            },
            crate::core::types::LlmToolCall {
                id: "b".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"b.rs"}"#.into(),
                },
            },
        ];
        assert!(!tool_calls_conflict(&different));
    }

    #[tokio::test]
    async fn parallel_batch_preserves_input_order() {
        // Distinct-path reads take the parallel fan-out; results must come
        // back input-ordered so the caller can zip them with the original
        // calls — a slow first worker must never shift attribution onto the
        // second call's result.
        use crate::tools::Policy;
        let call = |id: &str, path: &str| crate::core::types::LlmToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: crate::core::types::FunctionCall {
                name: "read".into(),
                arguments: format!(r#"{{"path":"{path}"}}"#),
            },
        };
        let calls = vec![
            call("a", "definitely-not-here-a.rs"),
            call("b", "definitely-not-here-b.rs"),
        ];
        assert!(!tool_calls_conflict(&calls));
        let (sink_tx, mut sink_rx) = mpsc::channel(32);
        let (approval_tx, _approval_rx) = mpsc::channel(16);
        let console = crate::core::console::Console::daemon(sink_tx, approval_tx);
        let results =
            run_tool_batch(&calls, &NeverCancel, &Policy::trusted(), None, &console).await;
        assert_eq!(results.len(), 2);
        assert!(
            results[0].1.contains("definitely-not-here-a.rs"),
            "first result keeps first call's input"
        );
        assert!(
            results[1].1.contains("definitely-not-here-b.rs"),
            "second result keeps second call's input"
        );
        // Parallel fan-out announces every call up front, each tagged with
        // its own id so the UI can open both blocks before either finishes.
        let mut announced = Vec::new();
        while let Ok(e) = sink_rx.try_recv() {
            if let SinkLine::ToolInput { id, .. } = e {
                announced.push(id);
            }
        }
        announced.sort();
        assert_eq!(announced, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn then_run_forces_serialization() {
        // A `write`/`edit` carrying `then_run` runs a shell command, so the
        // batch must serialize even across distinct paths — otherwise N edits
        // fan out N concurrent shells.
        let call = |name: &str, args: &str| crate::core::types::LlmToolCall {
            id: name.into(),
            call_type: "function".into(),
            function: crate::core::types::FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        };
        assert!(tool_calls_conflict(&[
            call("edit", r#"{"path":"a.rs","then_run":"cargo test"}"#),
            call("edit", r#"{"path":"b.rs"}"#),
        ]));
        // A bare `bash` likewise serializes the batch.
        assert!(tool_calls_conflict(&[call("bash", r#"{"command":"ls"}"#)]));
        // A blank `then_run` is not a command: the writes stay parallel.
        assert!(!tool_calls_conflict(&[
            call("edit", r#"{"path":"a.rs","then_run":"  "}"#),
            call("edit", r#"{"path":"b.rs"}"#),
        ]));
    }

    #[test]
    fn overflow_wording_is_recognized() {
        for msg in [
            "API error: This model's maximum context length is 8192 tokens",
            "input length exceeds context window",
            "your prompt is too long",
            "400 Too many tokens in request",
            "context size exceeds limit",
            "prompt too long for context",
            "token limit exceeded",
            "please reduce the length of the context",
        ] {
            assert!(is_context_overflow(msg), "{msg}");
        }
        assert!(!is_context_overflow("API error: invalid api key"));
        assert!(!is_context_overflow("stream idle for over 90s"));
        // Generic length validation without a context anchor must not
        // trigger a wasteful emergency compaction.
        assert!(!is_context_overflow("reduce the length of your filename"));
    }

    #[test]
    fn queue_recall_removes_newest_match_only() {
        let mut pending = Vec::new();
        apply_queue_msg(&mut pending, QueueMsg::Content("a".into()));
        apply_queue_msg(&mut pending, QueueMsg::Content("a".into()));
        apply_queue_msg(&mut pending, QueueMsg::Content("b".into()));
        // Newest matching item goes first; the older duplicate stays.
        apply_queue_msg(&mut pending, QueueMsg::Recall("a".into()));
        assert_eq!(pending, vec!["a".to_string(), "b".to_string()]);
        // A recall with no queued match is a no-op (already injected).
        apply_queue_msg(&mut pending, QueueMsg::Recall("zzz".into()));
        assert_eq!(pending, vec!["a".to_string(), "b".to_string()]);
        // A recall that arrives after its item (separate drain) still works.
        apply_queue_msg(&mut pending, QueueMsg::Recall("a".into()));
        assert_eq!(pending, vec!["b".to_string()]);
    }

    /// A model that stalls in a tool-call loop is cut off by the per-turn
    /// budget instead of burning unbounded tokens; the transcript stays
    /// coherent (every call has its result).
    #[tokio::test]
    async fn turn_budget_stops_an_endless_tool_loop() {
        #[derive(Clone)]
        struct AlwaysTool;
        impl ModelClient for AlwaysTool {
            async fn complete(
                &self,
                _m: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                Ok(Turn {
                    message: ChatMessage::assistant_calls(
                        Some("thinking about it".to_string()),
                        vec![crate::core::types::LlmToolCall {
                            id: format!("c{}", uuid::Uuid::new_v4().simple()),
                            call_type: "function".into(),
                            function: crate::core::types::FunctionCall {
                                name: "read".into(),
                                arguments: r#"{"path":"README.md"}"#.into(),
                            },
                        }],
                    ),
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 0,
                        cached_tokens: None,
                    }),
                    stop_reason: None,
                })
            }
        }
        // Serialize with every other process_turn test: while this test's
        // DEX_MAX_TOOL_ITERATIONS=2 is live across the process_turn await,
        // any concurrent process_turn would read it and exhaust early.
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let prev = std::env::var("DEX_MAX_TOOL_ITERATIONS").ok();
        std::env::set_var("DEX_MAX_TOOL_ITERATIONS", "2");
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let err = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &AlwaysTool,
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await
        .unwrap_err()
        .to_string();
        match prev {
            Some(v) => std::env::set_var("DEX_MAX_TOOL_ITERATIONS", v),
            None => std::env::remove_var("DEX_MAX_TOOL_ITERATIONS"),
        }
        assert!(err.contains("turn budget exhausted"), "{err}");
        // Transcript coherence: every assistant batch has its tool result.
        let unanswered = messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| m.tool_calls.as_ref().map(|c| c.len()).unwrap_or_default())
            .sum::<usize>();
        let answered = messages.iter().filter(|m| m.role == Role::Tool).count();
        assert_eq!(
            answered, unanswered,
            "budget stop must leave a coherent transcript"
        );
    }

    /// When the provider rejects the request because the history no longer
    /// fits, the loop emergency-compacts and retries once instead of failing
    /// the turn.
    #[tokio::test]
    async fn context_overflow_compacts_and_retries_once() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        #[derive(Clone)]
        struct OverflowThenOk {
            round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ModelClient for OverflowThenOk {
            async fn complete(
                &self,
                messages: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if round == 0 {
                    return Err("API error: maximum context length exceeded".into());
                }
                // After compaction the history must actually have shrunk.
                assert!(
                    messages.len() < 20,
                    "compaction must cut history before the retry"
                );
                Ok(Turn {
                    message: ChatMessage::assistant("recovered"),
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 0,
                        cached_tokens: None,
                    }),
                    stop_reason: None,
                })
            }
        }
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        messages.push(ChatMessage::user("goal: fix the flaky test"));
        // Enough medium-size turns that compaction has something to cut.
        for i in 0..20 {
            messages.push(ChatMessage::user(format!("u{i}: {}", "x".repeat(300))));
            messages.push(ChatMessage::assistant(format!("a{i}: {}", "y".repeat(300))));
        }
        let mut state = ToolState::default();
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &OverflowThenOk {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        assert!(
            result.is_ok(),
            "overflow must compact and retry: {result:?}"
        );
        // Exactly one summary entry, and the failed prompt round did not
        // leave the transcript wedged.
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.name.as_deref() == Some("summary"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn cancel_during_llm_call_unwinds_promptly() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        // TDD Phase 2: select!(cancelled, complete) — no 50ms poll quantum.
        #[derive(Clone)]
        struct Hanging;
        impl ModelClient for Hanging {
            async fn complete(
                &self,
                _m: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                unreachable!()
            }
        }
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let cancel = crate::core::console::CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel2.cancel();
        });
        let start = std::time::Instant::now();
        let err = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &Hanging,
            cancel: &cancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled by user");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must preempt hanging LLM without 50ms quanta pile-up"
        );
    }

    #[tokio::test]
    async fn cancel_during_tool_io_suppresses_result_fanout() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        // Cancel lands while the bash tool runs: the per-tool "cancelled"
        // error is shutdown noise, not model input — the turn unwinds
        // without persisting a tool result the model never saw.
        #[derive(Clone)]
        struct SleepOnce {
            round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ModelClient for SleepOnce {
            async fn complete(
                &self,
                _m: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let message = if round == 0 {
                    ChatMessage::assistant_calls(
                        None,
                        vec![crate::core::types::LlmToolCall {
                            id: "call-1".into(),
                            call_type: "function".into(),
                            function: crate::core::types::FunctionCall {
                                name: "bash".into(),
                                arguments: r#"{"command":"sleep 30"}"#.into(),
                            },
                        }],
                    )
                } else {
                    ChatMessage::assistant("done")
                };
                Ok(Turn {
                    message,
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 0,
                        cached_tokens: None,
                    }),
                    stop_reason: None,
                })
            }
        }
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let cancel = crate::core::console::CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel2.cancel();
        });
        let start = std::time::Instant::now();
        let err = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &SleepOnce {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            cancel: &cancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled by user");
        assert_eq!(messages.len(), 2, "only system + assistant call persist");
        assert!(
            !messages.iter().any(|m| m.role == Role::Tool),
            "cancelled tool errors must not reach the transcript"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must preempt tool IO without waiting out the command"
        );
    }

    #[derive(Clone)]
    struct RepeatedGuardScript {
        commands: Vec<&'static str>,
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RepeatedGuardScript {
        fn new(commands: Vec<&'static str>) -> Self {
            Self {
                commands,
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl ModelClient for RepeatedGuardScript {
        async fn complete(
            &self,
            _m: &[ChatMessage],
            _w: bool,
            _s: Option<mpsc::Sender<SinkLine>>,
            _c: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let message = if round < self.commands.len() {
                ChatMessage::assistant_calls(
                    None,
                    vec![crate::core::types::LlmToolCall {
                        id: format!("call-{round}"),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: "bash".into(),
                            arguments: format!(r#"{{"command":"{}"}}"#, self.commands[round]),
                        },
                    }],
                )
            } else {
                ChatMessage::assistant("done")
            };
            Ok(Turn {
                message,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn repeated_tool_guard_counts_after_ring_eviction() {
        // k, a, b, c, k, d, k: the ring is full (6) when the 7th identical
        // call arrives, and the evicted front entry is itself a match.
        // Counting after the push+eviction reads 2 — under the threshold.
        // (A pre-push count would read 3 and feed the model a spurious
        // "repeated identical tool call" error on this call.)
        // Lock so the budget test's env window can't overlap this run.
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let guard_err = "Error: repeated identical tool call; choose a different action or finish.";
        let _ = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &RepeatedGuardScript::new(vec![
                "echo repeat-probe",
                "echo other-a",
                "echo other-b",
                "echo other-c",
                "echo repeat-probe",
                "echo other-d",
                "echo repeat-probe",
            ]),
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        let guard_hits = messages
            .iter()
            .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(guard_err))
            .count();
        assert_eq!(
            guard_hits, 0,
            "guard must not fire: ring eviction removed a match before counting"
        );
    }

    #[tokio::test]
    async fn repeated_tool_guard_fires_on_third_consecutive_call() {
        // Positive control: three identical calls in a row must trip the
        // guard on the third one only.
        // Lock so the budget test's env window can't overlap this run.
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let guard_err = "Error: repeated identical tool call; choose a different action or finish.";
        let _ = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &RepeatedGuardScript::new(vec!["echo probe", "echo probe", "echo probe"]),
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: None,
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        let guard_hits = messages
            .iter()
            .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(guard_err))
            .count();
        assert_eq!(
            guard_hits, 1,
            "exactly the third identical call trips: {guard_hits}"
        );
        let last_tool = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool)
            .and_then(|m| m.content.as_deref());
        assert_eq!(
            last_tool,
            Some(guard_err),
            "the guard error must land on the third call's result"
        );
    }

    /// Phase 2 exit: a child runs the same loop with its own seed, no
    /// steering, and a filter — the allowlist is enforced at dispatch
    /// (denied `bash` fails closed as a tool result) while the allowed
    /// `read` runs, and the history is the child's own.
    #[derive(Clone)]
    struct FilterProbe {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FilterProbe {
        fn new() -> Self {
            Self {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl ModelClient for FilterProbe {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let message = if round == 0 {
                ChatMessage::assistant_calls(
                    None,
                    vec![
                        crate::core::types::LlmToolCall {
                            id: "call-1".into(),
                            call_type: "function".into(),
                            function: crate::core::types::FunctionCall {
                                name: "read".into(),
                                arguments: r#"{"path":"Cargo.toml"}"#.into(),
                            },
                        },
                        crate::core::types::LlmToolCall {
                            id: "call-2".into(),
                            call_type: "function".into(),
                            function: crate::core::types::FunctionCall {
                                name: "bash".into(),
                                arguments: r#"{"command":"echo hi"}"#.into(),
                            },
                        },
                    ],
                )
            } else {
                ChatMessage::assistant("done")
            };
            Ok(Turn {
                message,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn filtered_child_run_enforces_allowlist_and_keeps_own_history() {
        let _lock = TEST_TURN_ENV_LOCK.lock().await;
        let config = test_config();
        let mut messages = vec![ChatMessage::system("child seed: explore only")];
        let mut state = ToolState::default();
        let filter = ToolFilter::new("explorer", ["read", "grep", "find"]);
        let result = process_turn(AgentRuntime {
            config: &config,
            messages: &mut messages,
            state: &mut state,
            steering_rx: None,
            steering_accepted_tx: None,
            session: None,
            client: &FilterProbe::new(),
            cancel: &NeverCancel,
            console: &crate::core::console::Console::none(),
            filter: Some(&filter),
            agent_ctx: None,
            tool_budget: None,
        })
        .await;
        assert_eq!(result.unwrap(), "done");
        let tool_text: String = messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            tool_text.contains("dex"),
            "allowed read must run: {tool_text}"
        );
        assert!(
            tool_text.contains("not in explorer's tool allowlist"),
            "denied bash must fail closed with the policy reason: {tool_text}"
        );
        // The child's history is its own seed plus this turn — no parent
        // transcript is ever inherited.
        assert_eq!(
            messages.first().and_then(|m| m.content.as_deref()),
            Some("child seed: explore only")
        );
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.contains("parent transcript"))),
            "child must never see the parent transcript"
        );
    }
}
