//! Agent turn state machine (`process_turn`/`AgentRuntime`): owns the model↔tool loop.
//! Boundary: `daemon::turn` is the HTTP handler (auth, idempotency, SSE emit) and calls into here; no HTTP here.

use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::agent::compaction::{compact_history, KEEP_RECENT_MESSAGES};
use crate::agent::online_compaction::{
    cache_debt_for_ratio, decide_compaction, online_compaction_enabled, post_compaction_reminder,
    CompactionEconomics, DEFAULT_COMPACTION_ECONOMICS, NATIVE_SUMMARY_TOKEN_ESTIMATE,
};
use crate::agent::state::{cache_fingerprint, wait_cancelled, CancellationSource, ToolState};
use crate::agent::tokens::{
    estimate_ephemeral_tokens, estimate_tokens, schema_budget_tokens, TokenLedger,
};
use crate::llm::client::ModelClient;
use crate::llm::config::LlmConfig;
use crate::llm::transport::sse::Turn;
use crate::protocol::{ChatMessage, LlmToolCall, QueueMsg, Role, SinkLine, StopReason, Usage};
use crate::runtime::console::{
    with_console, Console, SpinnerGuard, RESET, TOOL_INPUT_COLOR, TOOL_MUTATION_LOCK,
    TOOL_OUTPUT_COLOR,
};
use crate::session::Session;
use crate::tools::{execute_outcome, Policy, ToolFilter, ToolOutcome};
use crate::ui::format::{
    model_tool_result, short_arg, tool_preview, tool_preview_body, tool_result_summary,
};

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

/// Re-persist the (compacted) history: atomically replace the session file
/// with header + `clear` + every message after the system prompt.
/// Single-sources the journal invariant that the file mirrors `messages`
/// after any compaction rewrites it (threshold gate, emergency, online
/// boundary). Atomic (`Session::rewrite_messages`): readers never see a
/// torn clear-plus-partial-tail.
fn rewrite_session(
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
    ledger: &mut TokenLedger,
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
    // Compaction may have rewritten history: re-measure once instead of
    // tracking per-round deltas on this rare path.
    *ledger = TokenLedger::rebuild(messages);
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
/// the stored context exceeds the token threshold or — without online
/// compaction — the message-count cap, at most three attempts. Threshold
/// compactions carry their cache re-write as debt the boundary economics
/// repay (math moved verbatim from the original inline block).
/// The budget is re-derived from the ledger after every cut: re-checking a
/// stale pre-cut number forces up to three compactions even when the first
/// already fit. Stored (not projected): the gate guards the window AND the
/// journal — a projected-only reading defers while the stored history grows
/// unbounded — matching the boundary economics and the pre-pack behavior.
/// The per-request sampler keeps the projected number (bytes actually sent).
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
        // `compact_history` rewrites `messages` — both read off the ledger,
        // O(keep-recent) instead of full transcript walks.
        let online = online_compaction_enabled().then(|| {
            (
                eff,
                ledger.archivable_tokens(KEEP_RECENT_MESSAGES, config.keep_recent_tokens()),
            )
        });
        match compact_history(config, messages, cancel, false).await {
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
    ledger: &'b mut TokenLedger,
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
    ledger.push(messages.last().expect("just pushed"));
    persist_pending(session, messages, persisted_cursor)?;
    if let Some(steps) = boundary {
        state.online_compaction.record_boundary(steps);
        if !*online_boundary_handled {
            *online_boundary_handled = true;
            // Ledger + preamble + schema: the same budget the pre-call gate
            // enforces, without re-walking history (`messages` here is the
            // stored history, matching the gate's pack-off reading; with the
            // pack on the gate reads the smaller projected view, so this is
            // the conservative side).
            let context_tokens = ledger.stored_tokens()
                + estimate_ephemeral_tokens(ephemerals)
                + schema_budget_tokens();
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
                // remove (see `TokenLedger::archivable_tokens`) — the system
                // message and the keep-recent window are never
                // archived, and the ephemeral preamble + tool
                // schema are re-sent on every request.
                ledger.archivable_tokens(KEEP_RECENT_MESSAGES, config.keep_recent_tokens()),
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
                        // History was rewritten: re-measure before the
                        // reminder push so the ledger mirrors `messages`.
                        *ledger = TokenLedger::rebuild(messages);
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
                        ledger.push(messages.last().expect("just pushed"));
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
            // A steer redirects the work: the horizon learned from completed
            // boundaries no longer describes the remaining effort.
            let injected = inject_steering(rx, steering_accepted_tx, messages).await;
            if injected {
                state.online_compaction.record_correction();
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
        // ledger + overhead per attempt (see `compaction_gate`); the
        // sampler below keeps the projected `eff` (bytes actually sent).
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

        // Observation pack projection: the provider-bound view replaces
        // stale large tool results with placeholders. Built fresh from the
        // intact history on every request — and only AFTER the gate, so a
        // compaction that rewrote `messages` this iteration is what the
        // projection (and the sampler below) sees. Building it before the
        // gate sent the pre-compaction history once the pack was on. The
        // stored session never changes. Token accounting (online sampling)
        // reads this view too — archived payloads must not pressure the
        // window estimate after their grace period expires.
        let obs_session = policy.agent.as_ref().map(|ctx| ctx.session_path.clone());
        // Owned projection only when the pack is on: when off the
        // provider-bound view IS `messages` (borrowed at the call site).
        // Held after the gate's `&mut` so no borrow freezes `messages`.
        let projected_owned: Option<Vec<ChatMessage>> =
            if crate::agent::obs_pack::observation_pack_enabled() {
                Some(
                    crate::agent::obs_pack::project_messages(
                        &state.obs_projection,
                        obs_session.as_deref(),
                        messages,
                    )
                    .into_owned(),
                )
            } else {
                None
            };
        // Placeholder takeovers are otherwise invisible — the stored history
        // never changes — so surface each first takeover to the user.
        for note in state.obs_projection.take_notes() {
            system_note(console, &note).await;
        }
        // Projected budget for the sampler below: with the pack off the
        // ledger total is exact (the wire view is history); with it on, one
        // walk over the projected view replaces the 3–4 full walks the loop
        // used to pay.
        let eff = match &projected_owned {
            Some(projected) => estimate_tokens(projected),
            None => ledger.stored_tokens(),
        } + budget_overhead;

        // Online compaction bookkeeping: sample the context size of
        // every provider request — the growth rate and the
        // per-boundary request counts feed the compaction economics.
        // Sampled on the projected view: only bytes actually sent count.
        crate::agent::online_compaction::sample_request(&mut state.online_compaction, eff);

        // Async LLM call with prompt cancel: `select!(cancelled, complete)`
        // wakes within ~10ms. The wire view borrows history when the pack is
        // off (no `to_vec` clone) and the owned projection otherwise.
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let call_started = std::time::Instant::now();
        let wire: &[ChatMessage] = projected_owned.as_deref().unwrap_or(messages.as_slice());
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
            ledger.push(messages.last().expect("just pushed"));
            if let Some(rx) = steering_rx.as_mut() {
                let injected = inject_steering(rx, steering_accepted_tx, messages).await;
                if injected {
                    state.online_compaction.record_correction();
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

#[cfg(test)]
pub(crate) mod tests;
