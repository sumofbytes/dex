use super::super::definition::AgentDefinition;
use super::super::exit::classify_body_error;
use super::super::exit::ExitReason;
use super::super::manager::ProgressReporter;
use super::super::manager::WaitOutcome;
use super::super::model::AgentId;
use super::super::model::AgentResult;
use super::super::model::AgentState;
use super::super::model::AgentUsage;
use super::super::model::ContextSeed;
use super::super::resume::ResumeHandle;
use super::super::resume::ResumeRequest;
use super::super::SpawnMeta;
use super::schema::is_delegation;
use super::schema::status_word;
use super::schema::AgentTurnContext;
use super::schema::DELEGATION_TOOLS;
use super::schema::MAX_AGENT_DEPTH;
use super::schema::MAX_WAIT_SECONDS;
use super::schema::WAIT_SLEEP;
use crate::agent::r#loop::process_turn;
use crate::agent::r#loop::AgentRuntime;
use crate::agent::state::CancellationSource;
use crate::agent::state::ToolState;
use crate::llm::config::LlmConfig;
use crate::protocol::ApprovalRequest;
use crate::protocol::ChatMessage;
use crate::protocol::SinkLine;
use crate::runtime::console::CancellationToken;
use crate::runtime::console::Console;
use crate::session::load_llm_messages_from_session;
use crate::session::Session;
use crate::tools::Policy;
use crate::tools::ToolError;
use crate::tools::ToolFilter;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

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

/// `delegate(agent, task?, file_hints?, model?, resume_from?, instruction?)` —
///
/// fresh: resolve the definition, build the isolated seed from the tool
/// arguments (the parent model writes the task itself; dex never
/// auto-copies transcript, §5), spawn, return immediately. `model` is the
/// same single-knob selection as the main agent (`provider/model`); when
/// present it overrides the definition for this spawn (complexity-based
/// pick), otherwise the child inherits this turn's resolved model (§13).
/// Resume: `resume_from` names a terminal child whose transcript replays as
/// generation + 1 with an interruption nudge (§24.1–§24.3); `task` is
/// then unneeded, and `instruction` (plus `file_hints`) folds into the
/// nudge instead of replacing the original task. A resume without its own
/// `model` keeps the finished generation's model (handle-carried), so a
/// complexity-chosen model survives generations unless overridden.
pub(crate) fn effective_child_model(
    explicit: Option<String>,
    handle_model: Option<&str>,
    def_model: Option<String>,
) -> Option<String> {
    explicit
        .or_else(|| handle_model.map(str::to_string))
        .or(def_model)
}

/// Clone the parent config and apply the effective model override, if any.
/// Single resolution point: dispatch validates here (fail-fast, no Running
/// entry on a bad pick) and hands the resolved config to the child body, so
/// the catalog is read once per spawn instead of twice.
pub(crate) fn resolve_child_config(
    parent: &LlmConfig,
    model: Option<&str>,
) -> Result<LlmConfig, String> {
    let mut config = parent.clone();
    if let Some(model) = model {
        config.apply_model(model, false)?;
    }
    Ok(config)
}

pub(crate) async fn delegate(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    policy: &Policy,
) -> Result<String, ToolError> {
    if ctx.depth >= MAX_AGENT_DEPTH {
        return Err(ToolError::Denied(format!(
            "delegation depth limit reached (depth {} of max {MAX_AGENT_DEPTH}); do the work in this turn instead of spawning",
            ctx.depth
        )));
    }
    let agent_name = string_arg(args, "agent").ok_or(ToolError::Missing("agent"))?;
    let mut def = super::super::find_definition(&agent_name).map_err(ToolError::InvalidArgument)?;
    let resume_from = string_arg(args, "resume_from");
    let task = string_arg(args, "task");
    if resume_from.is_none() && task.is_none() {
        return Err(ToolError::Missing("task"));
    }
    // Resolve the handle before the model so a resume without its own
    // `model` can inherit the finished generation's pick. On-disk handles
    // predate model tracking (`None`) and fall back to the definition.
    let handle_opt = match &resume_from {
        Some(resume_id) => Some(resolve_resume_handle(ctx, &AgentId(resume_id.clone())).await?),
        None => None,
    };
    let explicit = string_arg(args, "model");
    let base = def.model.clone();
    def.model = effective_child_model(
        explicit,
        handle_opt.as_ref().and_then(|h| h.model.as_deref()),
        base,
    );
    // Fail fast at dispatch: an unresolvable pick returns InvalidArgument
    // without spawning a child the parent must poll to discover the error.
    let child_config: Arc<LlmConfig> = Arc::new(
        resolve_child_config(&ctx.config, def.model.as_deref())
            .map_err(ToolError::InvalidArgument)?,
    );
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
    if let Some(handle) = handle_opt {
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
                    remaining_budget: handle.remaining_budget,
                },
                child_body(ctx.clone(), def.clone(), seed, Some(resume), child_config),
            )
            .map_err(|error| ToolError::Denied(error.to_string()))?;
        if let Some(console) = policy.console.as_ref() {
            console
                .emit_async(SinkLine::System(format!(
                    "[agent {}:{id}] started",
                    def.name
                )))
                .await;
        }
        return Ok(json!({
            "agent_id": id.to_string(),
            "state": "running",
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
                remaining_budget: None,
            },
            child_body(ctx.clone(), def.clone(), seed, None, child_config),
        )
        .map_err(|error| ToolError::Denied(error.to_string()))?;
    if let Some(console) = policy.console.as_ref() {
        console
            .emit_async(SinkLine::System(format!(
                "[agent {}:{id}] started",
                def.name
            )))
            .await;
    }
    Ok(json!({ "agent_id": id.to_string(), "state": "running" }).to_string())
}

/// `delegate_output(agent_id, wait_seconds?)` — bounded poll-wait (§10.2):
/// returns the terminal result immediately, else polls with short sleeps,
/// checking the parent turn's cancel token between sleeps so a cancelled
/// parent never wedges on the wait (the child keeps running, §14).
pub(crate) async fn delegate_output(
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

/// Assemble a resume generation's conversation (§24.3): the definition's
/// system prompt first (re-derived — the transcript never journals it and
/// the loader drops `Role::System` lines), the prior generation's
/// messages verbatim, then the interruption nudge. Returns what the LLM
/// sees and what the journal records; the journal mirrors the fresh path
/// (no system line), so re-resuming a resumed generation re-derives the
/// prompt again instead of duplicating it.
pub(crate) fn resume_conversation(
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
pub(crate) fn resume_nudge(request: &ResumeRequest) -> String {
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
pub(crate) fn parse_generation(file_name: &str) -> u32 {
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
pub(crate) async fn resolve_resume_handle(
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
            remaining_budget: None,
            // Predates model tracking: the resume falls back to the definition.
            model: None,
            note: "interrupted (daemon restart or crash); prior spend unknown".to_string(),
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
pub(crate) async fn delegate_list(ctx: &Arc<AgentTurnContext>) -> Result<String, ToolError> {
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

fn running_json(id: &AgentId, progress: Option<String>) -> String {
    match progress {
        Some(tool) => json!({ "agent_id": id.to_string(), "state": "running", "progress": tool }),
        None => json!({ "agent_id": id.to_string(), "state": "running" }),
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
pub(crate) fn resume_messages(
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
pub(crate) fn seed_task_text(seed: &ContextSeed) -> String {
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
    resume: Option<ResumeRequest>,
    child_config: Arc<LlmConfig>,
) -> ChildBody {
    Box::new(move |token, progress, id| {
        Box::pin(child_run(
            ctx,
            def,
            seed,
            token,
            progress,
            id,
            resume,
            child_config,
        ))
    })
}

#[allow(clippy::too_many_arguments)]
async fn child_run(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
    token: CancellationToken,
    progress: ProgressReporter,
    id: AgentId,
    resume: Option<ResumeRequest>,
    child_config: Arc<LlmConfig>,
) -> AgentResult {
    // §13: `child_config` is the dispatch-resolved model (parent plus
    // definition / per-spawn `model` override, validated once in
    // `delegate`), so the body never re-resolves and the catalog is read
    // once per spawn.
    let config = (*child_config).clone();
    // Child JSONL (§16): its own file beside the parent's, same marker
    // discipline (`turn_start`/`turn_complete`/`turn_failed`), so a crash
    // loses at most the in-flight event. Resume generations append `.g<N>`
    // (§24.3) so a resume never clobbers its parent. A disk failure
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
    // loudly — the parent can re-delegate with a fresh task instead.
    let mut messages: Vec<ChatMessage> = match &resume {
        Some(request) => {
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
                if message.role != crate::protocol::Role::System {
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
    // Open-call depth for the §15 progress label: parallel batches overlap,
    // so the label clears only when the last open call returns — clearing
    // on the first completion would drop the label while siblings run.
    let open_calls = Arc::new(Mutex::new(0u32));
    let open_in = open_calls.clone();
    let open_out = open_calls.clone();
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
                SinkLine::ToolInput { input, .. } => {
                    // Preview is "<name> <short-args>" (loop.rs emit shape).
                    let name = input.split(' ').next().unwrap_or_default();
                    progress.set(name);
                    *open_in.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    *tally_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                }
                SinkLine::ToolOutput { .. } => {
                    let mut open = open_out.lock().unwrap_or_else(|e| e.into_inner());
                    *open = open.saturating_sub(1);
                    if *open == 0 {
                        progress.clear();
                    }
                }
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
    // §11: the allowlist is the definition's own set, plus the delegation
    // tools when this child may delegate further (depth + 1 under the cap).
    // At the cap the filter strips them and the turn carries no daemon
    // context, so a further `delegate` rejects twice — allowlist first,
    // dispatch second.
    let child_depth = ctx.depth + 1;
    let may_delegate = child_depth < MAX_AGENT_DEPTH;
    let mut allowed: std::collections::BTreeSet<String> = def
        .tools
        .iter()
        .filter(|tool| !is_delegation(tool))
        .cloned()
        .collect();
    if may_delegate {
        allowed.extend(DELEGATION_TOOLS.iter().map(|t| t.to_string()));
    }
    let filter = ToolFilter {
        owner: def.name.clone(),
        allowed,
    };
    // The child's daemon context for one more level, when allowed. Shares
    // the session coordinates, manager, and approval bridges — only the
    // depth advances. The config is the child's *resolved* model (parent
    // model plus definition / per-spawn `model` override), so a grandchild
    // without its own override inherits the complexity-chosen model rather
    // than skipping back to the root.
    let child_ctx: Option<Arc<AgentTurnContext>> = if may_delegate {
        Some(Arc::new(AgentTurnContext {
            depth: child_depth,
            session_id: ctx.session_id.clone(),
            session_path: ctx.session_path.clone(),
            cwd: ctx.cwd.clone(),
            config: Arc::new(config.clone()),
            manager: ctx.manager.clone(),
            session_approvals: ctx.session_approvals.clone(),
            child_approvals: ctx.child_approvals.clone(),
            live_approvals: ctx.live_approvals.clone(),
        }))
    } else {
        None
    };
    // Agent lifecycle hooks (plan §7 P2): fired around the child's run with
    // the child's own policy, so a nested `dex.tools.call` from a hook is
    // gated exactly like a model-issued child call. A panicked or timed-out
    // body never reaches the end event — the registry result is the record
    // for those (spawn_wrapper owns the abnormal exits).
    // Scoped so the hook-host policy (which clones the console, keeping the
    // child sink open) drops before `drop(console)` below — otherwise the
    // consumer await deadlocks on a channel that never closes.
    let result = {
        let agent_policy = Policy::turn(config.permission, &console);
        crate::extensions::fire_event_global(
            "agent.start",
            serde_json::json!({ "agent": def.name, "id": id.0 }),
            &token,
            &agent_policy,
            Some(&filter),
        )
        .await;
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
            agent_ctx: child_ctx,
            // Resume honors the remaining meter (§24.2). `None` (unlimited
            // or unknown spend) falls back to the definition's cap.
            tool_budget: resume
                .as_ref()
                .and_then(|request| request.handle.remaining_budget)
                .or_else(|| def.max_tool_iterations.map(|n| n as usize)),
        })
        .await;
        let payload = match &result {
            Ok(_) => serde_json::json!({ "agent": def.name, "ok": true }),
            Err(e) => {
                serde_json::json!({ "agent": def.name, "ok": false, "error": e.to_string() })
            }
        };
        // Fired while the child's console is still open: a hook prompt (ask
        // modes) routes through the child's approval bridge like any tool
        // call.
        crate::extensions::fire_event_global(
            "agent.end",
            payload,
            &token,
            &agent_policy,
            Some(&filter),
        )
        .await;
        result
    };
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
            // (§24.1: one choke point for resume handles).
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
