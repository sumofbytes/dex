//! Phase 5 — the three delegation tools plus the child body that runs the
//! standard turn loop (plan §5, §8, §10): `delegate` spawns through the
//! per-session [`AgentManager`] and returns the id immediately, `delegate_output`
//! is the bounded poll-wait (≤120 s, ~250 ms sleeps, early exit on completion
//! or parent cancel — never `select!` on steering, which is `&mut`-borrowed by
//! the parent loop and unreachable from a tool call), `delegate_stop` cancels.
//!
//! The child body is the same `process_turn` runtime with an isolated bundle:
//! its own config clone (model override via the one-knob path, §13), its own
//! JSONL session (§16), its own console (a child-local sink; no approval
//! channel — background children cannot prompt, §12 V1a detached auto-deny),
//! the definition's tool allowlist enforced at dispatch (§11), and no
//! steering. Depth 1 is enforced twice: the child filter never contains a
//! delegation tool, and a child turn's bundle carries no daemon context.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use crate::agent::r#loop::{process_turn, AgentRuntime};
use crate::agent::state::{CancellationSource, ToolState};
use crate::core::console::{CancellationToken, Console};
use crate::core::types::{ApprovalRequest, ChatMessage, SinkLine};
use crate::llm::config::LlmConfig;
use crate::session::{load_llm_messages_from_session, Session};
use crate::tools::{Policy, ToolError, ToolFilter};

use super::context::ContextSeed;
use super::definition::AgentDefinition;
use super::exit::{classify_body_error, ExitReason, RecoverMode, ResumeHandle, ResumeRequest};
use super::instance::{AgentId, AgentState};
use super::manager::{AgentManager, ChildFactory, ProgressReporter, WaitOutcome};
use super::result::{AgentResult, AgentUsage};
use super::SpawnMeta;

/// The four model-facing delegation tools (§10, §24.3). Background is the
/// only spawn mode — no `run_in_background` flag to forget.
pub(crate) const DELEGATION_TOOLS: [&str; 4] = [
    "delegate",
    "delegate_output",
    "delegate_stop",
    "delegate_list",
];

/// `delegate_output`'s wait ceiling (§10.2): a bounded poll-wait, never an
/// unbounded block.
pub(crate) const MAX_WAIT_SECONDS: u64 = 120;

/// Sleep quantum of the `delegate_output` wait loop: steering sent during a
/// wait is acted on at most one interval after the wait returns (§10.2 — the
/// documented latency, not a claimed interrupt that cannot exist).
const WAIT_SLEEP: Duration = Duration::from_millis(250);

pub(crate) fn is_delegation(name: &str) -> bool {
    DELEGATION_TOOLS.contains(&name)
}

/// Lowercase status word for the §15 lifecycle lines and tool results
/// (`finished completed`, `finished timed out`).
pub(crate) fn status_word(state: AgentState) -> &'static str {
    match state {
        // One word for `Pending` everywhere (`delegate`/`delegate_output`
        // say "queued" too) — the registry state and the queue state are
        // the same fact.
        AgentState::Pending => "queued",
        AgentState::Running => "running",
        AgentState::Completed => "completed",
        AgentState::Failed => "failed",
        AgentState::Cancelled => "cancelled",
        AgentState::TimedOut => "timed out",
    }
}

/// Set once at startup from the resolved [`crate::cli::Mode`]: only
/// daemon-backed processes (Serve, and Default which spawns one) link the
/// delegation tools into the schema. OneShot/no-daemon modes unregister
/// them at registration time (§10 — not a prompt hack); dispatch rejects
/// them anyway (no manager to spawn into).
static DAEMON_LINKED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_daemon_linked(linked: bool) {
    DAEMON_LINKED.store(linked, Ordering::Relaxed);
}

/// Schema-level gate (§19 kill switch + §10 OneShot rule). Dispatch checks
/// the live turn context too — registration and enforcement stay separate.
pub(crate) fn delegation_enabled() -> bool {
    DAEMON_LINKED.load(Ordering::Relaxed) && std::env::var("DEX_SUBAGENTS").as_deref() != Ok("0")
}

/// The daemon-backed turn context a `delegate` call needs (the thin-client
/// side of the manager, §10.1). Built once per parent turn in
/// `run_turn_inner`; `None`-shaped absence is what makes delegation
/// impossible in OneShot/direct tool runs. Children get no context of their
/// own, so a child turn has nothing to delegate with (depth 1 at dispatch).
pub(crate) struct AgentTurnContext {
    pub(crate) session_id: String,
    /// The parent session's JSONL — the child file lands beside it (§16).
    pub(crate) session_path: PathBuf,
    /// The parent's resolved workspace (the child inherits it, §5).
    pub(crate) cwd: String,
    /// The parent's resolved model config; the child clones it and applies
    /// its definition's model override, if any (§13).
    pub(crate) config: Arc<LlmConfig>,
    pub(crate) manager: AgentManager,
    /// Snapshot of the parent session's "allow for session" keys (same
    /// scope as [`Console::approval_key`]) — children outlive the turn that
    /// spawned them, so the set they inherit is seeded at spawn (§12; the
    /// live-consulted variant is V1b).
    pub(crate) session_approvals: HashSet<String>,
    /// §12 V1b: the per-parent-turn channel that routes a child's
    /// [`ApprovalRequest`] into the daemon's `pending_approvals` as a
    /// labeled prompt (the parent turn's own tools use their own channel).
    /// `None` outside the daemon: the child console then carries a closed
    /// channel and `enforce_policy` fails closed (V1a detached auto-deny).
    pub(crate) child_approvals: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
    /// §12 V1b: live "allow for session" lookup against the daemon's map,
    /// so a decision granted after a child spawned still applies to it.
    /// `None` outside the daemon.
    pub(crate) live_approvals: Option<crate::core::console::LiveApprovalCheck>,
}

/// Dispatch the delegation tools from [`crate::tools::execute`]. The
/// allowlist gate already ran (a child calling any delegation tool was
/// rejected there, §11).
pub(crate) async fn execute_delegation(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
) -> Result<String, ToolError> {
    let Some(ctx) = policy.agent.clone() else {
        return Err(ToolError::Denied(format!(
            "'{name}' spawns children in the daemon; it is unavailable without a \
             daemon-backed turn (one-shot and direct tool runs have no manager)"
        )));
    };
    match name {
        "delegate" => delegate(&ctx, args, policy).await,
        "delegate_output" => delegate_output(&ctx, args, cancel).await,
        "delegate_stop" => delegate_stop(&ctx, args).await,
        "delegate_list" => delegate_list(&ctx).await,
        _ => unreachable!("is_delegation and dispatch stay in sync"),
    }
}

fn string_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|value| !value.trim().is_empty())
}

fn agent_id_arg(args: &Map<String, Value>) -> Result<AgentId, ToolError> {
    string_arg(args, "agent_id")
        .map(AgentId)
        .ok_or(ToolError::Missing("agent_id"))
}

/// `delegate(agent, task?, file_hints?, resume_from?, instruction?)` —
/// fresh: resolve the definition, build the isolated seed from the tool
/// arguments (the parent model writes the task itself; dex never
/// auto-copies transcript, §5), spawn, return immediately. Resume:
/// `resume_from` names a terminal child whose transcript replays as
/// generation + 1 with an interruption nudge (§24.1–§24.3); `task` is
/// then unneeded, and `instruction` (plus `file_hints`) folds into the
/// nudge instead of replacing the original task.
async fn delegate(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    policy: &Policy,
) -> Result<String, ToolError> {
    let agent_name = string_arg(args, "agent").ok_or(ToolError::Missing("agent"))?;
    let def = super::find_definition(&agent_name).map_err(ToolError::InvalidArgument)?;
    let resume_from = string_arg(args, "resume_from");
    let task = string_arg(args, "task");
    if resume_from.is_none() && task.is_none() {
        return Err(ToolError::Missing("task"));
    }
    let file_hints = args
        .get("file_hints")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .filter(|hint| !hint.trim().is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(resume_id) = resume_from {
        let handle = resolve_resume_handle(ctx, &AgentId(resume_id)).await?;
        let instruction = string_arg(args, "instruction");
        let seed = ContextSeed {
            // Unused on the resume path (messages replay from the
            // transcript), but the spawn still files one: record why.
            task: format!(
                "resume {} generation {}",
                handle.agent_id,
                handle.generation + 1
            ),
            file_hints: file_hints.clone(),
            parent_summary: None,
        };
        let generation = handle.generation + 1;
        let resume = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: handle.clone(),
            instruction,
            file_hints,
        };
        let id = ctx
            .manager
            .spawn(
                &def,
                seed.clone(),
                SpawnMeta {
                    generation,
                    parent_session: Some(ctx.session_path.clone()),
                    parent_id: Some(handle.agent_id.clone()),
                    // No auto-retry on a hand-steered generation: the
                    // model just took ownership of recovery, so a
                    // further death escalates back to it instead of
                    // retrying behind its back.
                    retry: None,
                    lineage: handle.history.clone(),
                    remaining_budget: handle.remaining_budget,
                },
                child_body(ctx.clone(), def.clone(), seed, Some(resume)),
            )
            .map_err(|error| ToolError::Denied(error.to_string()))?;
        // §24.5: under the cap or an open breaker the spawn queues — the
        // lifecycle line and the tool result say so instead of "started".
        let queued = ctx.manager.is_queued(&id);
        if let Some(console) = policy.console.as_ref() {
            let line = if queued {
                format!("[agent {}:{id}] queued (waiting for a slot)", def.name)
            } else {
                format!("[agent {}:{id}] started", def.name)
            };
            console.emit_async(SinkLine::System(line)).await;
        }
        let state = if queued { "queued" } else { "running" };
        return Ok(json!({
            "agent_id": id.to_string(),
            "state": state,
            "resumed_from": handle.agent_id.to_string(),
            "generation": generation,
        })
        .to_string());
    }
    let seed = ContextSeed {
        task: task.ok_or(ToolError::Missing("task"))?,
        file_hints,
        parent_summary: None,
    };
    let id = ctx
        .manager
        .spawn(
            &def,
            seed.clone(),
            SpawnMeta {
                generation: 0,
                parent_session: Some(ctx.session_path.clone()),
                parent_id: None,
                // Auto-recovery factory: the definition's
                // `supervision.recover` decides whether the manager
                // ever calls it past generation 0 (`Never` escalates
                // immediately — today's behavior).
                retry: Some(child_factory(ctx.clone(), def.clone(), seed.clone())),
                lineage: Vec::new(),
                remaining_budget: None,
            },
            child_body(ctx.clone(), def.clone(), seed, None),
        )
        .map_err(|error| ToolError::Denied(error.to_string()))?;
    // §24.5: under the cap or an open breaker the spawn queues — the
    // lifecycle line and the tool result say so instead of "started".
    let queued = ctx.manager.is_queued(&id);
    if let Some(console) = policy.console.as_ref() {
        let line = if queued {
            format!("[agent {}:{id}] queued (waiting for a slot)", def.name)
        } else {
            format!("[agent {}:{id}] started", def.name)
        };
        console.emit_async(SinkLine::System(line)).await;
    }
    let state = if queued { "queued" } else { "running" };
    Ok(json!({ "agent_id": id.to_string(), "state": state }).to_string())
}

/// `delegate_output(agent_id, wait_seconds?)` — bounded poll-wait (§10.2):
/// returns the terminal result immediately, else polls with short sleeps,
/// checking the parent turn's cancel token between sleeps so a cancelled
/// parent never wedges on the wait (the child keeps running, §14).
async fn delegate_output(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    let id = agent_id_arg(args)?;
    let wait_seconds = args
        .get("wait_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(MAX_WAIT_SECONDS);
    let deadline = Instant::now() + Duration::from_secs(wait_seconds);
    loop {
        match ctx.manager.wait(&id, Duration::ZERO).await {
            WaitOutcome::Finished(result) => return Ok(result_json(&id, &result)),
            WaitOutcome::Unknown => {
                return Err(ToolError::InvalidArgument(format!(
                    "unknown agent id '{id}': never spawned in this session, \
                     or its result aged out of retention"
                )));
            }
            WaitOutcome::Running(state) => {
                if cancel.is_cancelled() || Instant::now() >= deadline {
                    return Ok(running_json(&id, state, ctx.manager.progress(&id)));
                }
            }
        }
        tokio::time::sleep(WAIT_SLEEP).await;
    }
}

/// `delegate_stop(agent_id)` — signal the child's token; the wrapper funnels
/// the `Cancelled` result through the same finish path as every other
/// terminal state (§14).
async fn delegate_stop(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
) -> Result<String, ToolError> {
    let id = agent_id_arg(args)?;
    if ctx.manager.cancel(&id).is_none() {
        return Err(ToolError::InvalidArgument(format!(
            "unknown agent id '{id}': never spawned in this session, \
             or its result aged out of retention"
        )));
    }
    match ctx.manager.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => Ok(result_json(&id, &result)),
        // A body that ignores its token still ends via the wrapper's
        // timeout; report the in-flight state instead of blocking.
        WaitOutcome::Running(_) | WaitOutcome::Unknown => Ok(json!({
            "agent_id": id.to_string(),
            "state": "cancelling"
        })
        .to_string()),
    }
}

/// Assemble a resume generation's conversation (§24.3): the definition's
/// system prompt first (re-derived — the transcript never journals it and
/// the loader drops `Role::System` lines), the prior generation's
/// messages verbatim, then the interruption nudge. Returns what the LLM
/// sees and what the journal records; the journal mirrors the fresh path
/// (no system line), so re-resuming a resumed generation re-derives the
/// prompt again instead of duplicating it.
fn resume_conversation(
    def: &AgentDefinition,
    replayed: Vec<ChatMessage>,
    request: &ResumeRequest,
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    let mut in_memory = vec![ChatMessage::system(child_system_prompt(def))];
    in_memory.extend(replayed);
    let nudge = ChatMessage::user_named(resume_nudge(request), "resume");
    in_memory.push(nudge.clone());
    let journal = in_memory[1..].to_vec();
    debug_assert!(!journal.is_empty());
    (in_memory, journal)
}

/// The interruption nudge appended to a replayed transcript (§24.3):
/// why the prior generation stopped, what changed, and the meter left.
fn resume_nudge(request: &ResumeRequest) -> String {
    let mut nudge = format!(
        "You were interrupted: {}. Continue from the transcript above — \
         do not redo completed work, pick up where it stopped.",
        request.handle.note
    );
    if let Some(instruction) = request
        .instruction
        .as_deref()
        .filter(|instruction| !instruction.trim().is_empty())
    {
        nudge.push_str(&format!("\n\nAdditional instruction: {instruction}"));
    }
    if !request.file_hints.is_empty() {
        let hints = request
            .file_hints
            .iter()
            .map(|hint| hint.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        nudge.push_str(&format!("\n\nFile hints: {hints}"));
    }
    match request.handle.remaining_budget {
        Some(0) => nudge.push_str(
            "\n\nNo further tool calls remain: write your final summary now without tools.",
        ),
        Some(left) => nudge.push_str(&format!(
            "\n\nYou have at most {left} further tool calls before the turn ends."
        )),
        None => {}
    }
    nudge
}

/// Generation suffix of a child transcript file name
/// (`<id>-<name>[.g<N>].jsonl`): the trailing `.g<N>` when the remainder
/// still holds an id (which always contains `-`), else 0. Greedy on the
/// last `.g<digits>` — a definition literally named `*.g1` at generation
/// 0 is indistinguishable from generation 1, and the `-` guard keeps
/// id-less stems at 0.
fn parse_generation(file_name: &str) -> u32 {
    let stem = file_name.strip_suffix(".jsonl").unwrap_or(file_name);
    let Some((base, digits)) = stem.rsplit_once(".g") else {
        return 0;
    };
    if !base.contains('-') || digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return 0;
    }
    digits.parse().unwrap_or(0)
}

/// Resolve `delegate(resume_from = <id>)` to a [`ResumeHandle`] (§24.3):
/// a retained terminal result that advertised one, else an interrupted
/// on-disk run from a daemon-restart-killed child
/// (`Session::list_children`). A live child is rejected — it needs
/// `delegate_output`, not a second generation.
async fn resolve_resume_handle(
    ctx: &Arc<AgentTurnContext>,
    id: &AgentId,
) -> Result<ResumeHandle, ToolError> {
    match ctx.manager.wait(id, Duration::ZERO).await {
        WaitOutcome::Finished(result) => {
            if let Some(handle) = result.resume {
                return Ok(handle);
            }
            return Err(ToolError::InvalidArgument(format!(
                "agent '{id}' ended {} and is not resumable; delegate_list shows resumable children",
                status_word(result.status)
            )));
        }
        WaitOutcome::Running(_) => {
            return Err(ToolError::InvalidArgument(format!(
                "agent '{id}' is still running: use delegate_output to wait for it, \
                 or delegate_stop first and then resume the terminal result"
            )));
        }
        WaitOutcome::Unknown => {}
    }
    let children = Session::list_children(&ctx.session_path)
        .map_err(|error| ToolError::InvalidArgument(format!("unknown agent id '{id}': {error}")))?;
    let prefix = format!("{id}-");
    for (path, _header, turn_state) in children {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // `resume_from` takes either the bare agent id (`sess-9`, what
        // live and retained rows advertise) or what `delegate_list` prints
        // for on-disk rows — the transcript file stem (`sess-9-explorer`,
        // `sess-9-explorer.g2`), which has no live registry to translate
        // it back.
        let by_id = name.starts_with(&prefix);
        let by_stem = name.strip_suffix(".jsonl") == Some(id.0.as_str());
        if !by_id && !by_stem {
            continue;
        }
        if turn_state != "interrupted" {
            return Err(ToolError::InvalidArgument(format!(
                "agent '{id}' has a transcript but its last turn is '{turn_state}', not interrupted"
            )));
        }
        let generation = parse_generation(name);
        return Ok(ResumeHandle {
            agent_id: id.clone(),
            transcript: path,
            generation,
            // Spend died with the daemon: the resume runs the full cap.
            // No lineage survives either — the daemon took it.
            remaining_budget: None,
            note: "interrupted (daemon restart or crash); prior spend unknown".to_string(),
            history: Vec::new(),
        });
    }
    Err(ToolError::InvalidArgument(format!(
        "unknown agent id '{id}': never spawned in this session, or its \
         result aged out of retention; delegate_list shows live, retained, \
         and interrupted children"
    )))
}

/// `delegate_list()` — the introspection tool (§24.3): live children with
/// their progress label, retained terminal results with resumability, and
/// interrupted on-disk runs a daemon restart left behind. Read-only and
/// idempotent; the post-compaction answer to "what children do I have?".
async fn delegate_list(ctx: &Arc<AgentTurnContext>) -> Result<String, ToolError> {
    let snapshot = ctx.manager.snapshot();
    let mut known: HashSet<String> = snapshot
        .iter()
        .filter_map(|child| child.transcript.as_ref())
        .map(|path| path.display().to_string())
        .collect();
    let mut children: Vec<Value> = snapshot
        .iter()
        .map(|child| {
            json!({
                "agent_id": child.agent_id.to_string(),
                "name": child.name,
                "state": status_word(child.state),
                "progress": child.progress,
                "resumable": child.resumable,
            })
        })
        .collect();
    // On-disk runs the registry no longer knows: anything already listed
    // (same transcript) skips, the rest report with their turn state.
    if let Ok(disk) = Session::list_children(&ctx.session_path) {
        for (path, header, turn_state) in disk {
            if !known.insert(path.display().to_string()) {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string();
            children.push(json!({
                "agent_id": stem,
                "name": header.name().unwrap_or_default(),
                "state": turn_state,
                "progress": Value::Null,
                "resumable": turn_state == "interrupted",
                "transcript": path.display().to_string(),
            }));
        }
    }
    children.sort_by(|a, b| {
        a.get("agent_id")
            .and_then(Value::as_str)
            .cmp(&b.get("agent_id").and_then(Value::as_str))
    });
    Ok(json!({ "children": children }).to_string())
}

fn result_json(id: &AgentId, result: &AgentResult) -> String {
    json!({
        "agent_id": id.to_string(),
        "status": status_word(result.status),
        "summary": result.summary,
        "error": result.error,
        "resumable": result.resume.is_some(),
    })
    .to_string()
}

fn running_json(id: &AgentId, state: AgentState, progress: Option<String>) -> String {
    // Tool-JSON wording (not the SSE wire): a queued child (§24.5) reads
    // "queued" — it has an id and a slot request, but no live process.
    let state = match state {
        AgentState::Pending => "queued",
        _ => "running",
    };
    match progress {
        Some(tool) => json!({ "agent_id": id.to_string(), "state": state, "progress": tool }),
        None => json!({ "agent_id": id.to_string(), "state": state }),
    }
    .to_string()
}

/// Build the resumed conversation (§24.3): persona re-applied first (the
/// journal never stores the System role — `load_messages_from_session`
/// skips it — so the persona comes from the same definition every
/// generation, no drift), the prior generation's messages verbatim, the
/// interruption nudge last. `Err` = the replay came back empty (crash
/// between `turn_start` and the first `append_message`, or an unreadable
/// journal): a resume with no history would run the degenerate seed task
/// ("resume <id> generation N"), which is worse than failing loudly — the
/// parent re-delegates with a fresh task instead.
fn resume_messages(
    def: &AgentDefinition,
    request: &ResumeRequest,
) -> Result<Vec<ChatMessage>, String> {
    let replayed = load_llm_messages_from_session(&request.handle.transcript).unwrap_or_default();
    if replayed.is_empty() {
        return Err(format!(
            "transcript {} has no replayable messages; re-delegate with a fresh task instead",
            request.handle.transcript.display()
        ));
    }
    let mut messages = vec![ChatMessage::system(child_system_prompt(def))];
    messages.extend(replayed.iter().cloned());
    messages.push(ChatMessage::user_named(resume_nudge(request), "resume"));
    Ok(messages)
}

/// The child's task prompt (§5): the parent model's own words, plus optional
/// file hints and parent context. The transcript is never copied.
fn seed_task_text(seed: &ContextSeed) -> String {
    let mut text = seed.task.clone();
    if !seed.file_hints.is_empty() {
        let hints = seed
            .file_hints
            .iter()
            .map(|hint| hint.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("\n\nFile hints: {hints}"));
    }
    if let Some(summary) = seed
        .parent_summary
        .as_deref()
        .filter(|summary| !summary.trim().is_empty())
    {
        text.push_str(&format!("\n\nParent context:\n{summary}"));
    }
    text
}

/// Child system prompt (§5): the definition's persona plus the standard
/// structure the main prompt builds (working rules, `project_context()`),
/// with the child's restricted tool list rendered instead of the main
/// agent's. Skills are main-agent surface; children get none.
pub(crate) fn child_system_prompt(def: &AgentDefinition) -> String {
    let tools = def.tools.iter().cloned().collect::<Vec<_>>().join(", ");
    let mut prompt = format!(
        "{persona}\n\n\
         You are a background sub-agent. The main agent delegated this task and \
         will read your final message as the result — report findings in prose \
         with file paths. Your tools: {tools} (anything else is rejected at \
         dispatch; do not try to work around a missing tool).\n\n\
         Working rules:\n\
         - Batch independent reads/searches into ONE parallel call. Don't do one file per turn.\n\
         - Read before edit; edit with exact oldText; verify with build/tests.\n\
         - Don't repeat tool calls — once you have enough context, act.\n\n\
         Answering: be concise, lead with the result, show file paths clearly.",
        persona = def.prompt,
        tools = tools,
    );
    if let Some(context) = crate::llm::prompt::project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&context);
    }
    prompt
}

/// The child body handed to [`AgentManager::spawn`]: one standard
/// `process_turn` run with an isolated bundle — no parent transcript, no
/// steering, no parent session, no parent cancel token (§8). Concrete
/// (boxed) so the spawn signature never resolves through an async opaque:
/// `child_run` calls `process_turn`, and a non-boxed closure return would
/// make the two functions' opaque types cycle.
pub(crate) type ChildBody = Box<
    dyn FnOnce(
            CancellationToken,
            ProgressReporter,
            AgentId,
        ) -> Pin<Box<dyn Future<Output = AgentResult> + Send>>
        + Send,
>;

/// A re-entry factory (§24.3): the same body builder the manager calls
/// per attempt, with the attempt's resume. `delegate` passes one always;
/// the definition's `supervision.recover` decides whether the manager
/// ever calls it past generation 0.
pub(crate) fn child_factory(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
) -> ChildFactory {
    Arc::new(move |token, progress, id, resume| {
        Box::pin(child_run(
            ctx.clone(),
            def.clone(),
            seed.clone(),
            token,
            progress,
            id,
            resume,
        )) as Pin<Box<dyn Future<Output = AgentResult> + Send>>
    })
}

pub(crate) fn child_body(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
    resume: Option<ResumeRequest>,
) -> ChildBody {
    Box::new(move |token, progress, id| {
        Box::pin(child_run(ctx, def, seed, token, progress, id, resume))
    })
}

async fn child_run(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
    token: CancellationToken,
    progress: ProgressReporter,
    id: AgentId,
    resume: Option<ResumeRequest>,
) -> AgentResult {
    // §13: the definition's model override resolves through the existing
    // one-knob path; `None` inherits the parent's resolved model.
    let mut config = (*ctx.config).clone();
    if let Some(model) = def.model.as_deref() {
        if let Err(error) = config.apply_model(model, false) {
            return AgentResult {
                status: AgentState::Failed,
                summary: String::new(),
                error: Some(format!("agent model '{model}' failed to resolve: {error}")),
                usage: None,
                reason: ExitReason::Permanent,
                tool_calls: 0,
                resume: None,
            };
        }
    }
    // Child JSONL (§16): its own file beside the parent's, same marker
    // discipline (`turn_start`/`turn_complete`/`turn_failed`), so a crash
    // loses at most the in-flight event. Resume generations append `.g<N>`
    // (§24.3) so a resume never clobbers its parent — including `Fresh`
    // recoveries, which re-enter through the same plumbing so the file
    // they write is the file the registry points at. A disk failure
    // fails the child, never the parent turn.
    let generation = resume
        .as_ref()
        .map(|request| request.handle.generation + 1)
        .unwrap_or(0);
    let mut session =
        match Session::child(&ctx.session_path, &ctx.cwd, &id.0, &def.name, generation) {
            Ok(session) => session,
            Err(error) => {
                return AgentResult {
                    status: AgentState::Failed,
                    summary: String::new(),
                    error: Some(format!("child session could not be created: {error}")),
                    usage: None,
                    reason: ExitReason::Permanent,
                    tool_calls: 0,
                    resume: None,
                };
            }
        };
    // Fresh: system prompt + the parent-written task. Resume: replay the
    // prior generation's transcript, then append the interruption nudge as
    // a named user message. The journal never stores the System role
    // (`load_messages_from_session` skips it), so the persona prompt is
    // re-applied from the definition on every generation — same definition,
    // no drift. A replay that comes back EMPTY (crash between `turn_start`
    // and the first `append_message`, or an unreadable journal) is a
    // Permanent failure: a resume with no history would run the degenerate
    // seed task ("resume <id> generation N"), which is worse than failing
    // loudly — the parent can re-delegate with a fresh task instead. A
    // `Fresh` recovery rides the same request but replays nothing: same
    // seed, full meter, generation-suffixed file the registry points at.
    let mut messages: Vec<ChatMessage> = match &resume {
        Some(request) if request.mode == RecoverMode::Resume => {
            let messages = match resume_messages(&def, request) {
                Ok(messages) => messages,
                Err(error) => {
                    return AgentResult {
                        status: AgentState::Failed,
                        summary: String::new(),
                        error: Some(error),
                        usage: None,
                        reason: ExitReason::Permanent,
                        tool_calls: 0,
                        resume: None,
                    };
                }
            };
            let _ = session.turn_event("turn_start");
            // The persona (System role) is rebuilt per generation, never
            // journaled — same rule the fresh path follows.
            for message in &messages {
                if message.role != crate::core::types::Role::System {
                    let _ = session.append_message(message);
                }
            }
            messages
        }
        _ => {
            let user_message = ChatMessage::user(seed_task_text(&seed));
            let _ = session.turn_event("turn_start");
            let _ = session.append_message(&user_message);
            vec![ChatMessage::system(child_system_prompt(&def)), user_message]
        }
    };

    // Child console: a child-local sink that captures the last assistant
    // text (the §6 synthesized partial summary) and, under the daemon, a
    // live approval channel routed through the parent turn's child-approval
    // bridge (§12 V1b labeled prompts: the request parks in the session's
    // pending_approvals with the child's name, and a 5-minute timeout
    // denies it if nobody answers). "Allow for session" approvals granted
    // in the parent session carry over — seeded from the spawn-time
    // snapshot and consulted live, so a decision granted after the spawn
    // still applies. Outside the daemon the channel stays closed and
    // `enforce_policy` fails closed (V1a detached auto-deny: the denial is
    // recorded as a failed tool result).
    let (sink_tx, mut sink_rx) = mpsc::channel::<SinkLine>(256);
    let approval_tx = match &ctx.child_approvals {
        Some(bridge) => bridge.clone(),
        None => {
            let (approval_tx, _dropped) = mpsc::channel::<ApprovalRequest>(1);
            drop(_dropped);
            approval_tx
        }
    };
    let console = Console::new(sink_tx, approval_tx)
        .with_live_approvals(ctx.live_approvals.clone())
        .with_agent(id.0.clone(), def.name.clone());
    console.seed_session_approvals(ctx.session_approvals.clone());
    let last_assistant = Arc::new(Mutex::new(None::<String>));
    let usage = Arc::new(Mutex::new(AgentUsage::default()));
    let capture = last_assistant.clone();
    let tally = usage.clone();
    // §24.1 spend meter: the body runs a single `process_turn`, so
    // sink-counted tool invocations are the budget resume carries over
    // (calls, not rounds — conservative when the child fanned out).
    let calls = Arc::new(Mutex::new(0u32));
    let tally_calls = calls.clone();
    // The child's sink lines drive three things: the §6 partial-summary
    // capture (last assistant text), the §15 progress label (the tool the
    // child is currently running, read by `delegate_output`), and the §18
    // usage tally (each `record_usage` emission folds into the result).
    let consumer = tokio::spawn(async move {
        while let Some(line) = sink_rx.recv().await {
            match line {
                SinkLine::Assistant(text) => {
                    *capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
                }
                SinkLine::ToolInput(preview) => {
                    // Preview is "<name> <short-args>" (loop.rs emit shape).
                    let name = preview.split(' ').next().unwrap_or_default();
                    progress.set(name);
                    *tally_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
                SinkLine::ToolOutput { .. } => progress.clear(),
                SinkLine::Usage {
                    tokens,
                    output,
                    cost,
                    ..
                } => {
                    tally
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .absorb(tokens, output, cost);
                }
                _ => {}
            }
        }
    });

    let mut tool_state = ToolState::load_async().await;
    // §11: the allowlist is the definition's own set; delegation tool names
    // are stripped defensively — dispatch rejects them regardless, but a
    // filter entry can never grant them (the prompt is not a boundary).
    let filter = ToolFilter {
        owner: def.name.clone(),
        allowed: def
            .tools
            .iter()
            .filter(|tool| !is_delegation(tool))
            .cloned()
            .collect(),
    };
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut tool_state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: Some(&mut session),
        client: &config,
        cancel: &token,
        console: &console,
        filter: Some(&filter),
        // Depth 1 at dispatch: children carry no daemon context, so every
        // delegation tool call from a child is rejected (§11/§20).
        agent_ctx: None,
        // Resume honors the remaining meter (§24.2). `None` (unlimited
        // or unknown spend) falls back to the definition's cap.
        tool_budget: resume
            .as_ref()
            .and_then(|request| request.handle.remaining_budget)
            .or_else(|| def.max_tool_iterations.map(|n| n as usize)),
    })
    .await;
    // Dropping the console closes the child sink, so the consumer drains
    // every buffered line and exits — awaiting it makes the captured
    // summary and §18 usage deterministic instead of racing the last
    // sends. (`SinkLine::Usage` is emitted per LLM call, after the final
    // assistant text.)
    drop(console);
    let _ = consumer.await;
    let _ = session.turn_event(if result.is_ok() {
        "turn_complete"
    } else {
        "turn_failed"
    });
    let partial = last_assistant
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    let usage = usage.lock().unwrap_or_else(|e| e.into_inner()).reported();
    let tool_calls = *calls.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        // Completed guarantees a non-empty summary (§6): an empty final
        // message is not a usable result.
        Ok(text) if !text.trim().is_empty() => AgentResult {
            status: AgentState::Completed,
            summary: text,
            error: None,
            usage,
            // The body classifies; only the manager's `finish` advertises
            // (§24.1: one decision point, via `on_exit`).
            reason: ExitReason::Normal,
            tool_calls,
            resume: None,
        },
        Ok(_) => AgentResult {
            status: AgentState::Failed,
            summary: partial,
            error: Some("child ended without a final message".to_string()),
            usage,
            reason: ExitReason::Permanent,
            tool_calls,
            resume: None,
        },
        Err(error) => {
            let message = error.to_string();
            AgentResult {
                status: AgentState::Failed,
                summary: partial,
                error: Some(message.clone()),
                usage,
                reason: classify_body_error(&message),
                tool_calls,
                resume: None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::GlobalCancellation;

    #[test]
    fn delegation_names_are_recognized() {
        for name in DELEGATION_TOOLS {
            assert!(is_delegation(name), "{name}");
        }
        assert!(!is_delegation("read"));
        assert!(!is_delegation("mcp__x__y"));
    }

    #[tokio::test]
    async fn delegation_without_a_daemon_context_rejects_cleanly() {
        // OneShot / `dex run` shape: no manager anywhere to spawn into.
        let policy = Policy::trusted();
        let args = Map::new();
        for name in DELEGATION_TOOLS {
            let error = execute_delegation(name, &args, &GlobalCancellation, &policy)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("daemon-backed"),
                "{name}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn children_cannot_delegate_at_the_filter() {
        // §11 depth-1 rule at dispatch: the child allowlist never contains a
        // delegation tool, so the standard availability gate rejects the
        // call before any delegation logic runs.
        let filter = ToolFilter::new("explorer", ["read", "ffgrep", "fffind"]);
        let policy = Policy::trusted();
        let mut args = Map::new();
        args.insert("agent".into(), json!("explorer"));
        args.insert("task".into(), json!("look around"));
        let error = crate::tools::execute(
            "delegate",
            &args,
            &GlobalCancellation,
            &policy,
            Some(&filter),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not in explorer's tool allowlist"),
            "{error}"
        );
    }

    #[test]
    fn seed_task_text_includes_hints_and_parent_summary() {
        let seed = ContextSeed {
            task: "check the build".to_string(),
            file_hints: vec![PathBuf::from("src/main.rs")],
            parent_summary: Some("the diff touches the gate".to_string()),
        };
        let text = seed_task_text(&seed);
        assert!(text.starts_with("check the build"));
        assert!(text.contains("File hints: src/main.rs"));
        assert!(text.contains("Parent context:"));
        assert!(text.contains("the diff touches"));
        let bare = ContextSeed {
            task: "t".to_string(),
            file_hints: Vec::new(),
            parent_summary: None,
        };
        assert_eq!(seed_task_text(&bare), "t");
    }

    #[test]
    fn child_system_prompt_carries_persona_tools_and_project_context() {
        let def = super::super::find_definition("explorer").unwrap();
        let prompt = child_system_prompt(&def);
        assert!(prompt.starts_with(&def.prompt), "persona leads");
        // BTreeSet iteration is sorted.
        assert!(
            prompt.contains("Your tools: fffind, ffgrep, read"),
            "{prompt}"
        );
        // project_context() appends the repo's own instructions when present
        // (this repo has one); the marker matches the main prompt's shape.
        if crate::llm::prompt::project_context().is_some() {
            assert!(prompt.contains("--- Project instructions ---"), "{prompt}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn parent_cancel_ends_delegate_output_without_touching_children() {
        // §22-I: the parent's cancel token ends the `delegate_output` wait
        // early (the deadline is minutes away, so an instant return proves
        // the token drove it) — and the child keeps running untouched.
        let manager = AgentManager::new("sess");
        let ctx = Arc::new(AgentTurnContext {
            session_id: "sess".to_string(),
            // No real child body runs here, so the parent path is never touched.
            session_path: PathBuf::new(),
            cwd: String::new(),
            config: Arc::new(crate::llm::config::tests::test_cfg()),
            manager: manager.clone(),
            session_approvals: HashSet::new(),
            child_approvals: None,
            live_approvals: None,
        });
        let id = manager
            .spawn(
                &super::super::builtin_definitions()
                    .into_iter()
                    .next()
                    .unwrap(),
                ContextSeed {
                    task: "do the thing".to_string(),
                    file_hints: Vec::new(),
                    parent_summary: None,
                },
                SpawnMeta::fresh(),
                // Ends only through its own token: parent cancel must not
                // reach it.
                |token, _progress, _id| async move {
                    token.cancelled().await;
                    AgentResult {
                        status: AgentState::Cancelled,
                        summary: String::new(),
                        error: Some("child saw cancel".to_string()),
                        usage: None,
                        reason: ExitReason::ShutDown,
                        tool_calls: 0,
                        resume: None,
                    }
                },
            )
            .unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let mut args = Map::new();
        args.insert("agent_id".into(), json!(id.to_string()));
        let wait =
            tokio::time::timeout(Duration::from_secs(5), delegate_output(&ctx, &args, &token))
                .await;
        match wait {
            Ok(Ok(out)) => {
                let value: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(value["state"], "running");
            }
            other => panic!("expected a fast running report, got {other:?}"),
        }
        // Untouched: still live, still Running, still its own cancel token.
        assert_eq!(manager.active_count(), 1);
        assert_eq!(manager.status(&id), Some(AgentState::Running));
        // Cleanup: cancel + join so no task outlives the test.
        manager.shutdown().await;
        assert_eq!(manager.active_count(), 0);
    }

    #[test]
    fn parse_generation_reads_trailing_suffix_only() {
        assert_eq!(parse_generation("sess-0-explorer.jsonl"), 0);
        assert_eq!(parse_generation("sess-0-explorer.g1.jsonl"), 1);
        assert_eq!(parse_generation("sess-0-explorer.g12.jsonl"), 12);
        // No id guard (`-`): an id-less `.g1`-shaped stem stays gen 0.
        assert_eq!(parse_generation("my.g1.jsonl"), 0);
        // Greedy on the last suffix: a definition literally named `my.g1`
        // at generation 0 is indistinguishable from generation 1.
        assert_eq!(parse_generation("sess-0-my.g1.jsonl"), 1);
        assert_eq!(parse_generation("sess-0-explorer.gx.jsonl"), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_messages_reapplies_persona_and_appends_nudge() {
        // §24.3 (review fix): the journal never stores the System role, so
        // `resume_messages` must re-apply the persona from the definition —
        // a resumed child runs with the same persona it started with.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = PathBuf::from("/tmp/dex-supervision-resume-msgs");
        let parent_path = dir.join("sess.jsonl");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let mut child = Session::child(
            &parent_path,
            "/tmp/dex-supervision-resume-msgs",
            "sess-2",
            "tester",
            0,
        )
        .unwrap();
        let _ = child.turn_event("turn_start");
        child
            .append_message(&ChatMessage::user("check the build"))
            .unwrap();
        child
            .append_message(&ChatMessage::assistant("found three risks"))
            .unwrap();
        drop(child);
        let def = super::super::builtin_definitions()
            .into_iter()
            .find(|def| def.name == "tester")
            .unwrap();
        let request = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: ResumeHandle {
                agent_id: AgentId("sess-2".to_string()),
                transcript: Session::child_path(&parent_path, "sess-2", "tester", 0),
                generation: 0,
                remaining_budget: Some(7),
                note: "timed out after 3 tool calls; continue from the transcript".to_string(),
                history: Vec::new(),
            },
            instruction: Some("skip the build".to_string()),
            file_hints: Vec::new(),
        };
        let messages = resume_messages(&def, &request).unwrap();
        // Persona leads, from the definition — not from the journal.
        assert!(
            messages[0]
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with(&def.prompt)),
            "first message must be the persona: {:?}",
            messages[0].content
        );
        // Replay is verbatim and in order.
        assert_eq!(messages[1].content.as_deref(), Some("check the build"));
        assert_eq!(messages[2].content.as_deref(), Some("found three risks"));
        // The nudge lands last, carrying the instruction.
        let last = messages.last().unwrap();
        assert_eq!(last.name.as_deref(), Some("resume"));
        let nudge = last.content.as_deref().unwrap();
        assert!(nudge.contains("timed out after 3 tool calls"), "{nudge}");
        assert!(nudge.contains("skip the build"), "{nudge}");
        assert!(nudge.contains("at most 7 further tool calls"), "{nudge}");
        // The persona never journals (System role skipped on replay too).
        let reloaded = crate::session::load_messages_from_session(&Session::child_path(
            &parent_path,
            "sess-2",
            "tester",
            0,
        ))
        .unwrap();
        assert!(reloaded
            .iter()
            .all(|m| m.role != crate::core::types::Role::System));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resume_messages_rejects_an_empty_transcript_loudly() {
        // §24.3 review fix: an empty replay (turn_start without any message
        // line) must NOT fall back to the degenerate seed task — it fails
        // Permanent so the parent re-delegates with a fresh task.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = PathBuf::from("/tmp/dex-supervision-resume-empty");
        let parent_path = dir.join("sess.jsonl");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let mut child = Session::child(
            &parent_path,
            "/tmp/dex-supervision-resume-empty",
            "sess-3",
            "tester",
            0,
        )
        .unwrap();
        let _ = child.turn_event("turn_start");
        drop(child);
        let def = super::super::builtin_definitions()
            .into_iter()
            .find(|def| def.name == "tester")
            .unwrap();
        let request = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: ResumeHandle {
                agent_id: AgentId("sess-3".to_string()),
                transcript: Session::child_path(&parent_path, "sess-3", "tester", 0),
                generation: 0,
                remaining_budget: None,
                note: "interrupted".to_string(),
                history: Vec::new(),
            },
            instruction: None,
            file_hints: Vec::new(),
        };
        let error = resume_messages(&def, &request).unwrap_err();
        assert!(error.contains("no replayable messages"), "{error}");
        assert!(error.contains("re-delegate"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_nudge_carries_reason_instruction_and_budget() {
        let request = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: ResumeHandle {
                agent_id: AgentId("sess-0".to_string()),
                transcript: PathBuf::from("/tmp/x.jsonl"),
                generation: 2,
                remaining_budget: Some(5),
                note: "turn budget exhausted after 50 tool calls; continue from the transcript"
                    .to_string(),
                history: Vec::new(),
            },
            instruction: Some("skip the build".to_string()),
            file_hints: vec![PathBuf::from("src/main.rs")],
        };
        let nudge = resume_nudge(&request);
        assert!(nudge.contains("turn budget exhausted"), "{nudge}");
        assert!(nudge.contains("skip the build"), "{nudge}");
        assert!(nudge.contains("src/main.rs"), "{nudge}");
        assert!(nudge.contains("at most 5 further tool calls"), "{nudge}");
        let spent = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: ResumeHandle {
                remaining_budget: Some(0),
                ..request.handle.clone()
            },
            instruction: None,
            file_hints: Vec::new(),
        };
        assert!(
            resume_nudge(&spent).contains("No further tool calls remain"),
            "{}",
            resume_nudge(&spent)
        );
    }

    #[test]
    fn resume_conversation_prepends_system_prompt_and_journals_without_it() {
        // The transcript never journals the system prompt and the loader
        // drops `Role::System` lines — the resume must re-derive it, or
        // the generation runs with no persona and no tool rules.
        let def = super::super::builtin_definitions()
            .into_iter()
            .next()
            .unwrap();
        let replayed = vec![
            ChatMessage::user("do the thing"),
            ChatMessage::assistant("on it"),
        ];
        let request = ResumeRequest {
            mode: RecoverMode::Resume,
            handle: ResumeHandle {
                agent_id: AgentId("sess-0".to_string()),
                transcript: PathBuf::from("/tmp/x.jsonl"),
                generation: 0,
                remaining_budget: Some(5),
                note: "timed out".to_string(),
                history: Vec::new(),
            },
            instruction: Some("skip the build".to_string()),
            file_hints: Vec::new(),
        };
        let (in_memory, journal) = resume_conversation(&def, replayed, &request);
        assert_eq!(in_memory.len(), 4);
        assert_eq!(in_memory[0].role, crate::core::types::Role::System);
        assert!(!in_memory[0]
            .content
            .as_deref()
            .unwrap_or_default()
            .is_empty());
        assert_eq!(in_memory[1].content.as_deref(), Some("do the thing"));
        assert_eq!(in_memory[2].content.as_deref(), Some("on it"));
        assert_eq!(in_memory[3].name.as_deref(), Some("resume"));
        assert!(
            in_memory[3]
                .content
                .as_deref()
                .unwrap_or_default()
                .contains("skip the build"),
            "{}",
            in_memory[3].content.as_deref().unwrap_or_default()
        );
        // The journal mirrors the fresh path: no system line, nudge last.
        assert_eq!(journal.len(), 3);
        assert_ne!(journal[0].role, crate::core::types::Role::System);
        assert_eq!(journal[2].name.as_deref(), Some("resume"));
    }

    fn resume_test_ctx(manager: AgentManager, session_path: PathBuf) -> Arc<AgentTurnContext> {
        Arc::new(AgentTurnContext {
            session_id: "sess".to_string(),
            session_path,
            cwd: String::new(),
            config: Arc::new(crate::llm::config::tests::test_cfg()),
            manager,
            session_approvals: HashSet::new(),
            child_approvals: None,
            live_approvals: None,
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_resume_handle_prefers_retained_then_rejects_live() {
        // A retained terminal result that advertised a handle wins without
        // touching the disk.
        let manager = AgentManager::new("sess");
        let dir = PathBuf::from("/tmp/dex-supervision-resolve");
        let ctx = resume_test_ctx(manager.clone(), dir.join("sess.jsonl"));
        let transcript = dir.join("agents").join("sess-0-tester.jsonl");
        let held = transcript.clone();
        let id = manager
            .spawn(
                &super::super::builtin_definitions()
                    .into_iter()
                    .next()
                    .unwrap(),
                ContextSeed {
                    task: "do the thing".to_string(),
                    file_hints: Vec::new(),
                    parent_summary: None,
                },
                SpawnMeta::fresh(),
                move |_, _, _| {
                    let transcript = held.clone();
                    async move {
                        AgentResult {
                            status: AgentState::TimedOut,
                            summary: "partial".to_string(),
                            error: Some("timed out after 600s".to_string()),
                            usage: None,
                            reason: ExitReason::Exhausted(
                                crate::agent::subagent::ExhaustKind::Timeout,
                            ),
                            tool_calls: 4,
                            resume: Some(ResumeHandle {
                                agent_id: AgentId("sess-0".to_string()),
                                transcript,
                                generation: 0,
                                remaining_budget: Some(46),
                                note: "timed out after 4 tool calls; continue from the transcript"
                                    .to_string(),
                                history: Vec::new(),
                            }),
                        }
                    }
                },
            )
            .unwrap();
        // The body returns immediately, but the spawned task still has to
        // be polled once: join it through a bounded wait before resolving.
        match manager.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        let handle = resolve_resume_handle(&ctx, &id).await.unwrap();
        assert_eq!(handle.remaining_budget, Some(46));
        assert_eq!(handle.generation, 0);
        // A live child rejects: it needs output/wait, not a new generation.
        let live = manager
            .spawn(
                &super::super::builtin_definitions()
                    .into_iter()
                    .next()
                    .unwrap(),
                ContextSeed {
                    task: "hang".to_string(),
                    file_hints: Vec::new(),
                    parent_summary: None,
                },
                SpawnMeta::fresh(),
                |token, _, _| async move {
                    token.cancelled().await;
                    AgentResult {
                        status: AgentState::Cancelled,
                        summary: String::new(),
                        error: None,
                        usage: None,
                        reason: ExitReason::ShutDown,
                        tool_calls: 0,
                        resume: None,
                    }
                },
            )
            .unwrap();
        let error = resolve_resume_handle(&ctx, &live).await.unwrap_err();
        assert!(error.to_string().contains("still running"), "{error}");
        // Unknown id with no disk: clean rejection pointing at the list.
        let error = resolve_resume_handle(&ctx, &AgentId("sess-9".to_string()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("delegate_list"), "{error}");
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolve_resume_handle_finds_interrupted_runs_on_disk() {
        // A daemon-restart-killed child: header + turn_start, no terminal
        // marker. The registry never saw this manager, so resolution is
        // purely the on-disk scan.
        let dir = PathBuf::from("/tmp/dex-supervision-resolve-disk");
        let parent_path = dir.join("sess.jsonl");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let mut child = Session::child(
            &parent_path,
            "/tmp/dex-supervision-resolve-disk",
            "sess-4",
            "explorer",
            0,
        )
        .unwrap();
        let _ = child.turn_event("turn_start");
        drop(child);
        let mut gen2 = Session::child(
            &parent_path,
            "/tmp/dex-supervision-resolve-disk",
            "sess-5",
            "tester",
            2,
        )
        .unwrap();
        let _ = gen2.turn_event("turn_start");
        drop(gen2);
        let manager = AgentManager::new("sess");
        let ctx = resume_test_ctx(manager, parent_path);
        let handle = resolve_resume_handle(&ctx, &AgentId("sess-4".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.generation, 0);
        assert_eq!(handle.remaining_budget, None);
        assert!(handle.transcript.ends_with("agents/sess-4-explorer.jsonl"));
        let handle = resolve_resume_handle(&ctx, &AgentId("sess-5".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.generation, 2);
        // `delegate_list` prints on-disk rows under their transcript stem;
        // the advertised id must round-trip back into a handle.
        let handle = resolve_resume_handle(&ctx, &AgentId("sess-4-explorer".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.generation, 0);
        assert!(handle.transcript.ends_with("agents/sess-4-explorer.jsonl"));
        let handle = resolve_resume_handle(&ctx, &AgentId("sess-5-tester.g2".to_string()))
            .await
            .unwrap();
        assert_eq!(handle.generation, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delegate_list_reports_live_retained_and_disk() {
        let dir = PathBuf::from("/tmp/dex-supervision-list");
        let parent_path = dir.join("sess.jsonl");
        std::fs::create_dir_all(dir.join("agents")).unwrap();
        let manager = AgentManager::new("sess");
        let ctx = resume_test_ctx(manager.clone(), parent_path.clone());
        let live = manager
            .spawn(
                &super::super::builtin_definitions()
                    .into_iter()
                    .next()
                    .unwrap(),
                ContextSeed {
                    task: "hang".to_string(),
                    file_hints: Vec::new(),
                    parent_summary: None,
                },
                SpawnMeta::fresh(),
                |token, _, _| async move {
                    token.cancelled().await;
                    AgentResult {
                        status: AgentState::Cancelled,
                        summary: String::new(),
                        error: None,
                        usage: None,
                        reason: ExitReason::ShutDown,
                        tool_calls: 0,
                        resume: None,
                    }
                },
            )
            .unwrap();
        let done = manager
            .spawn(
                &super::super::builtin_definitions()
                    .into_iter()
                    .next()
                    .unwrap(),
                ContextSeed {
                    task: "finish".to_string(),
                    file_hints: Vec::new(),
                    parent_summary: None,
                },
                SpawnMeta::fresh(),
                |_, _, _| async {
                    AgentResult {
                        status: AgentState::Completed,
                        summary: "ok".to_string(),
                        error: None,
                        usage: None,
                        reason: ExitReason::Normal,
                        tool_calls: 1,
                        resume: None,
                    }
                },
            )
            .unwrap();
        match manager.wait(&done, Duration::from_secs(5)).await {
            WaitOutcome::Finished(_) => {}
            other => panic!("expected Finished, got {other:?}"),
        }
        let mut disk = Session::child(
            &parent_path,
            "/tmp/dex-supervision-list",
            "sess-9",
            "explorer",
            0,
        )
        .unwrap();
        let _ = disk.turn_event("turn_start");
        drop(disk);
        let out = delegate_list(&ctx).await.unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        let children = value["children"].as_array().unwrap();
        assert_eq!(children.len(), 3, "{out}");
        let row = |agent_id: &str| {
            children
                .iter()
                .find(|row| row["agent_id"] == agent_id)
                .unwrap_or_else(|| panic!("missing row {agent_id}: {out}"))
        };
        assert_eq!(row(&live.to_string())["state"], "running");
        assert_eq!(row(&live.to_string())["resumable"], false);
        assert_eq!(row(&done.to_string())["state"], "completed");
        assert_eq!(row(&done.to_string())["resumable"], false);
        assert_eq!(row("sess-9-explorer")["state"], "interrupted");
        assert_eq!(row("sess-9-explorer")["resumable"], true);
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
