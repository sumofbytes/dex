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
use crate::session::Session;
use crate::tools::{Policy, ToolError, ToolFilter};

use super::context::ContextSeed;
use super::definition::AgentDefinition;
use super::instance::{AgentId, AgentState};
use super::manager::{AgentManager, ProgressReporter, WaitOutcome};
use super::result::AgentResult;

/// The three model-facing delegation tools (§10). Background is the only
/// spawn mode — no `run_in_background` flag to forget.
pub(crate) const DELEGATION_TOOLS: [&str; 3] = ["delegate", "delegate_output", "delegate_stop"];

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
        AgentState::Pending => "pending",
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

/// `delegate(agent, task, file_hints?)` — resolve the definition, build the
/// isolated seed from the tool arguments (the parent model writes the task
/// itself; dex never auto-copies transcript, §5), spawn, return immediately.
async fn delegate(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    policy: &Policy,
) -> Result<String, ToolError> {
    let agent_name = string_arg(args, "agent").ok_or(ToolError::Missing("agent"))?;
    let task = string_arg(args, "task").ok_or(ToolError::Missing("task"))?;
    let def = super::find_definition(&agent_name).map_err(ToolError::InvalidArgument)?;
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
    let seed = ContextSeed {
        task,
        file_hints,
        parent_summary: None,
    };
    let id = ctx
        .manager
        .spawn(
            &def,
            seed.clone(),
            child_body(ctx.clone(), def.clone(), seed),
        )
        .map_err(|error| ToolError::Denied(error.to_string()))?;
    // §15 V1a: the started line rides the parent turn's journal (already
    // streaming); the finished line is journaled by the manager's hook on
    // every terminal path, whenever the child actually ends.
    if let Some(console) = policy.console.as_ref() {
        console
            .emit_async(SinkLine::System(format!(
                "[agent {}:{}] started",
                def.name, id
            )))
            .await;
    }
    Ok(json!({ "agent_id": id.to_string(), "state": "running" }).to_string())
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
            WaitOutcome::Running(_) => {
                if cancel.is_cancelled() || Instant::now() >= deadline {
                    return Ok(running_json(&id, ctx.manager.progress(&id)));
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

fn result_json(id: &AgentId, result: &AgentResult) -> String {
    json!({
        "agent_id": id.to_string(),
        "status": status_word(result.status),
        "summary": result.summary,
        "error": result.error,
    })
    .to_string()
}

fn running_json(id: &AgentId, progress: Option<String>) -> String {
    match progress {
        Some(tool) => json!({ "agent_id": id.to_string(), "state": "running", "progress": tool }),
        None => json!({ "agent_id": id.to_string(), "state": "running" }),
    }
    .to_string()
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

pub(crate) fn child_body(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
) -> ChildBody {
    Box::new(move |token, progress, id| Box::pin(child_run(ctx, def, seed, token, progress, id)))
}

async fn child_run(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
    token: CancellationToken,
    progress: ProgressReporter,
    id: AgentId,
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
            };
        }
    }
    // Child JSONL (§16): its own file beside the parent's, same marker
    // discipline (`turn_start`/`turn_complete`/`turn_failed`), so a crash
    // loses at most the in-flight event. A disk failure fails the child,
    // never the parent turn.
    let mut session = match Session::child(&ctx.session_path, &ctx.cwd, &id.0, &def.name) {
        Ok(session) => session,
        Err(error) => {
            return AgentResult {
                status: AgentState::Failed,
                summary: String::new(),
                error: Some(format!("child session could not be created: {error}")),
            };
        }
    };
    let user_message = ChatMessage::user(seed_task_text(&seed));
    let _ = session.turn_event("turn_start");
    let _ = session.append_message(&user_message);
    let mut messages = vec![ChatMessage::system(child_system_prompt(&def)), user_message];

    // Child console: a child-local sink that captures the last assistant
    // text (the §6 synthesized partial summary) and NO live approval
    // channel — background children cannot prompt for approval (§12 V1a
    // detached auto-deny: the denial is recorded as a failed tool result).
    // "Allow for session" approvals granted in the parent session carry
    // over, so a user-approved exact command still runs.
    let (sink_tx, mut sink_rx) = mpsc::channel::<SinkLine>(256);
    let (approval_tx, _dropped) = mpsc::channel::<ApprovalRequest>(1);
    drop(_dropped);
    let console = Console::new(sink_tx, approval_tx);
    console.seed_session_approvals(ctx.session_approvals.clone());
    let last_assistant = Arc::new(Mutex::new(None::<String>));
    let capture = last_assistant.clone();
    // The child's sink lines drive two things: the §6 partial-summary
    // capture (last assistant text) and the §15 progress label (the tool
    // the child is currently running, read by `delegate_output`).
    tokio::spawn(async move {
        while let Some(line) = sink_rx.recv().await {
            match line {
                SinkLine::Assistant(text) => {
                    *capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
                }
                SinkLine::ToolInput(preview) => {
                    // Preview is "<name> <short-args>" (loop.rs emit shape).
                    let name = preview.split(' ').next().unwrap_or_default();
                    progress.set(name);
                }
                SinkLine::ToolOutput { .. } => progress.clear(),
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
        tool_budget: def.max_tool_iterations.map(|n| n as usize),
    })
    .await;
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
    match result {
        // Completed guarantees a non-empty summary (§6): an empty final
        // message is not a usable result.
        Ok(text) if !text.trim().is_empty() => AgentResult {
            status: AgentState::Completed,
            summary: text,
            error: None,
        },
        Ok(_) => AgentResult {
            status: AgentState::Failed,
            summary: partial,
            error: Some("child ended without a final message".to_string()),
        },
        Err(error) => AgentResult {
            status: AgentState::Failed,
            summary: partial,
            error: Some(error.to_string()),
        },
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
}
