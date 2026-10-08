use super::super::definition::AgentDefinition;
use super::super::exit::classify_body_error;
use super::super::exit::ExitReason;
use super::super::manager::ChildQueues;
use super::super::manager::ProgressReporter;
use super::super::manager::SendDelivery;
use super::super::manager::SendError;
use super::super::manager::SendOutcome;
use super::super::manager::WaitOutcome;
use super::super::model::AgentId;
use super::super::model::AgentResult;
use super::super::model::AgentState;
use super::super::model::ContextSeed;
use super::super::resume::ResumeHandle;
use super::super::resume::ResumeRequest;
use super::super::SpawnMeta;
use super::schema::is_delegation;
use super::schema::status_word;
use super::schema::AgentTurnContext;
use super::schema::DELEGATION_ACTIONS;
use super::schema::DELEGATION_TOOL;
use super::schema::MAX_AGENT_DEPTH;
use super::schema::MAX_WAIT_SECONDS;
use super::schema::WAIT_SLEEP;
use crate::agent::state::CancellationSource;
use crate::agent::state::ToolState;
use crate::agent::turn_loop::process_turn;
use crate::agent::turn_loop::AgentRuntime;
use crate::llm::config::LlmConfig;
use crate::protocol::ApprovalRequest;
use crate::protocol::ChatMessage;
use crate::protocol::SinkLine;
use crate::protocol::StreamEvent;
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

/// Dispatch the `delegate` tool from [`crate::tools::execute`]. The
/// allowlist gate already ran (a child calling it was rejected there, §11).
/// One tool, five actions: spawn/wait/stop/list/send route on
/// `args["action"]`.
pub async fn execute_delegation(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let Some(ctx) = policy.agent.clone() else {
        return Err(ToolError::Denied(format!(
            "'{name}' spawns children in the daemon; it is unavailable without a \
             daemon-backed turn (one-shot and direct tool runs have no manager)"
        )));
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("spawn");
    match action {
        "spawn" => delegate(&ctx, args, cancel, policy, filter).await,
        "wait" => delegate_output(&ctx, args, cancel).await,
        "stop" => delegate_stop(&ctx, args).await,
        "list" => delegate_list(&ctx).await,
        "send" => delegate_send(&ctx, args, policy).await,
        other => Err(ToolError::InvalidArgument(format!(
            "action must be one of {} (got '{other}')",
            DELEGATION_ACTIONS.join(" | ")
        ))),
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
/// present it overrides the definition for this spawn (explicit
/// pick), otherwise the child inherits this turn's resolved model (§13).
/// Resume: `resume_from` names a terminal child whose transcript replays as
/// generation + 1 with an interruption nudge (§24.1–§24.3); `task` is
/// then unneeded, and `instruction` (plus `file_hints`) folds into the
/// nudge instead of replacing the original task. A resume without its own
/// `model` keeps the finished generation's model (handle-carried), so a
/// per-spawn model survives generations unless overridden.
pub fn effective_child_model(
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
pub fn resolve_child_config(parent: &LlmConfig, model: Option<&str>) -> Result<LlmConfig, String> {
    let mut config = parent.clone();
    if let Some(model) = model {
        config.apply_model(model, false)?;
    }
    Ok(config)
}

pub async fn delegate(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    if ctx.depth >= MAX_AGENT_DEPTH {
        return Err(ToolError::Denied(format!(
            "delegation depth limit reached (depth {} of max {MAX_AGENT_DEPTH}); do the work in this turn instead of spawning",
            ctx.depth
        )));
    }
    let requested = string_arg(args, "agent").ok_or(ToolError::Missing("agent"))?;
    let task = string_arg(args, "task");
    let resume_from = string_arg(args, "resume_from");
    if resume_from.is_none() && task.is_none() {
        return Err(ToolError::Missing("task"));
    }
    // Supervisor routing: one `supervisor.route` round-trip when a Lua
    // extension subscribes, else the requested definition (zero-cost
    // default). A deny fails attributed; a redirect to an unknown
    // definition falls back to the requested agent with a loud log — a
    // hook typo must not brick delegation.
    let agent_name = if crate::extensions::has_event_handlers("supervisor.route") {
        let action = crate::extensions::query_supervisor_route(
            &requested,
            task.as_deref(),
            cancel,
            policy,
            filter,
        )
        .await;
        if let Some((by, reason)) = action.deny {
            let why = if reason.trim().is_empty() {
                "denied by extension".to_string()
            } else {
                reason
            };
            return Err(ToolError::Denied(format!(
                "supervisor.route hook from extension '{by}' denied spawn of '{requested}': {why}"
            )));
        }
        match action.agent {
            Some(target) if super::super::find_definition(&target).is_ok() => target,
            Some(target) => {
                eprintln!(
                    "dex: [extensions] supervisor.route redirected to unknown agent '{target}' — spawning '{requested}' instead"
                );
                requested
            }
            None => requested,
        }
    } else {
        requested
    };
    let mut def = super::super::find_definition(&agent_name).map_err(ToolError::InvalidArgument)?;
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
pub async fn delegate_output(
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

/// `delegate_send(agent_id, message, delivery?)` — deliver one message to
/// an existing child (spec G1/G2). On a live child, `steer` (default) is
/// injected before the child's next model call (the same drain points the
/// main turn uses; a steer racing the child's final answer chains as a new
/// turn), `follow_up` queues a new turn after the current one ends — a
/// parent polling `action=wait` observes either within one 250 ms
/// [`WAIT_SLEEP`] quantum. On a retained `Completed` child, `send` continues
/// it (spec G2): the next generation replays the transcript, appends the
/// message as a follow-up, and runs a full tool budget under the parent's
/// *current* approvals (live-consulted, not the spawn-time snapshot). It
/// occupies a [`MAX_CHILDREN`] slot like any spawn.
pub async fn delegate_send(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    policy: &Policy,
) -> Result<String, ToolError> {
    let id = agent_id_arg(args)?;
    let message = string_arg(args, "message").ok_or(ToolError::Missing("message"))?;
    let delivery_raw = args
        .get("delivery")
        .and_then(Value::as_str)
        .unwrap_or("steer");
    let delivery = SendDelivery::parse(delivery_raw).ok_or_else(|| {
        ToolError::InvalidArgument(format!(
            "delivery must be 'steer' or 'follow_up' (got '{delivery_raw}')"
        ))
    })?;
    match ctx.manager.send(&id, message.clone(), delivery) {
        Ok(outcome) => Ok(match outcome {
            SendOutcome::Steered => json!({
                "agent_id": id.to_string(),
                "accepted": "steer",
                "note": "injected at the child's next round boundary"
            })
            .to_string(),
            SendOutcome::Queued => json!({
                "agent_id": id.to_string(),
                "accepted": "follow_up",
                "note": "runs as the child's next turn after this one ends"
            })
            .to_string(),
        }),
        Err(SendError::Full) => Err(ToolError::InvalidArgument(format!(
            "cannot send to agent '{id}': the delivery queue is full; wait for it \
             to drain before sending more"
        ))),
        Err(error @ SendError::Unknown { .. }) => {
            Err(ToolError::InvalidArgument(error.to_string()))
        }
        Err(SendError::NotRunning) => {
            let result = match ctx.manager.wait(&id, Duration::ZERO).await {
                WaitOutcome::Finished(result) => result,
                WaitOutcome::Running(_) => {
                    return Err(ToolError::InvalidArgument(format!(
                        "agent '{id}' is wrapping up and no longer accepts messages; \
                         once it finishes, send again to continue it"
                    )));
                }
                WaitOutcome::Unknown => {
                    return Err(ToolError::InvalidArgument(format!(
                        "agent '{id}' ended and its result aged out of retention; \
                         delegate action=list shows this session's children"
                    )));
                }
            };
            let Some(handle) = result.resume.as_ref().filter(|handle| handle.continuable) else {
                // Spec table: failures/cancels/timeouts answer to
                // `resume_from`, not to a send.
                let hint = if result.resume.is_some() {
                    format!("it re-enters with delegate(resume_from = \"{id}\") instead")
                } else {
                    "only resumable endings answer to delegate(resume_from = …); \
                     delegate action=list shows which children are continuable"
                        .to_string()
                };
                return Err(ToolError::InvalidArgument(format!(
                    "agent '{id}' ended {} and cannot be continued with send; {hint}",
                    status_word(result.status)
                )));
            };
            // Continuation (spec G2): same transcript, one generation on,
            // the message is the follow-up instruction. Model: the handle's
            // per-spawn pick is re-applied unless this send overrides it —
            // `send` carries no `model` argument, so the handle wins.
            let name = ctx
                .manager
                .snapshot()
                .iter()
                .find(|child| child.agent_id == id)
                .map(|child| child.name.clone())
                .unwrap_or_default();
            let mut def =
                super::super::find_definition(&name).map_err(ToolError::InvalidArgument)?;
            let base = def.model.clone();
            def.model = effective_child_model(None, handle.model.as_deref(), base);
            let child_config: Arc<LlmConfig> = Arc::new(
                resolve_child_config(&ctx.config, def.model.as_deref())
                    .map_err(ToolError::InvalidArgument)?,
            );
            let generation = handle.generation + 1;
            let mut continue_handle = handle.clone();
            // A continuation always runs a full budget (the definition cap).
            continue_handle.remaining_budget = None;
            let resume = ResumeRequest {
                handle: continue_handle,
                instruction: Some(message),
                file_hints: Vec::new(),
            };
            let seed = ContextSeed {
                // Unused on the resume path (messages replay from the
                // transcript), but the spawn still files one: record why.
                task: format!("continue {id} generation {generation}"),
                file_hints: Vec::new(),
                parent_summary: None,
            };
            let new_id = ctx
                .manager
                .spawn(
                    &def,
                    SpawnMeta {
                        generation,
                        parent_session: Some(ctx.session_path.clone()),
                        remaining_budget: None,
                    },
                    child_body(ctx.clone(), def.clone(), seed, Some(resume), child_config),
                )
                .map_err(|error| ToolError::Denied(error.to_string()))?;
            if let Some(console) = policy.console.as_ref() {
                console
                    .emit_async(SinkLine::System(format!(
                        "[agent {}:{new_id}] started",
                        def.name
                    )))
                    .await;
            }
            Ok(json!({
                "agent_id": new_id.to_string(),
                "state": "running",
                "continued_from": id.to_string(),
                "generation": generation,
            })
            .to_string())
        }
    }
}

/// Assemble a resume generation's conversation (§24.3): the definition's
/// system prompt first (re-derived — the transcript never journals it and
/// the loader drops `Role::System` lines), the prior generation's
/// messages verbatim, then the interruption nudge. Returns what the LLM
/// sees and what the journal records; the journal mirrors the fresh path
/// (no system line), so re-resuming a resumed generation re-derives the
/// prompt again instead of duplicating it.
/// Test-only: the live resume path inlines this assembly in `child_body`
/// (turn_event ordering differs). Kept as the reference the test asserts.
#[cfg(test)]
pub fn resume_conversation(
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
/// A continuation (`send` into a finished child, spec G2) says follow-up
/// instead — the child did not fail, the parent asked for more work.
pub fn resume_nudge(request: &ResumeRequest) -> String {
    if request.handle.continuable {
        let mut nudge = format!(
            "You already reported your final result ({}). The parent has a \
             follow-up: work from the transcript above — nothing needs repeating \
             unless the follow-up asks for it.",
            request.handle.note
        );
        if let Some(instruction) = request
            .instruction
            .as_deref()
            .filter(|instruction| !instruction.trim().is_empty())
        {
            nudge.push_str(&format!("\n\nFollow-up: {instruction}"));
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
        return nudge;
    }
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
pub fn parse_generation(file_name: &str) -> u32 {
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
/// a `delegate` `wait`, not a second generation.
pub async fn resolve_resume_handle(
    ctx: &Arc<AgentTurnContext>,
    id: &AgentId,
) -> Result<ResumeHandle, ToolError> {
    match ctx.manager.wait(id, Duration::ZERO).await {
        WaitOutcome::Finished(result) => {
            // A retained Completed child advertises a *continuation* handle
            // (spec G2): `resume_from` is for recoverable endings — a
            // finished child is followed up with `delegate send` instead.
            if let Some(handle) = result.resume.as_ref().filter(|h| !h.continuable) {
                return Ok(handle.clone());
            }
            if result
                .resume
                .as_ref()
                .is_some_and(|handle| handle.continuable)
            {
                return Err(ToolError::InvalidArgument(format!(
                    "agent '{id}' ended completed: continue it with delegate \
                     send (message = what to do next), not a resume"
                )));
            }
            return Err(ToolError::InvalidArgument(format!(
                "agent '{id}' ended {} and is not resumable; delegate action=list shows resumable children",
                status_word(result.status)
            )));
        }
        WaitOutcome::Running(_) => {
            return Err(ToolError::InvalidArgument(format!(
                "agent '{id}' is still running: use delegate action=wait for it, \
                 or delegate action=stop first and then resume the terminal result"
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
            // On-disk runs are re-entry targets, not continuation targets.
            continuable: false,
        });
    }
    Err(ToolError::InvalidArgument(format!(
        "unknown agent id '{id}': never spawned in this session, or its \
         result aged out of retention; delegate action=list shows live, retained, \
         and interrupted children"
    )))
}

/// `delegate_list()` — the introspection tool (§24.3): live children with
/// their progress label, retained terminal results with resumability, and
/// interrupted on-disk runs a daemon restart left behind. Read-only and
/// idempotent; the post-compaction answer to "what children do I have?".
pub async fn delegate_list(ctx: &Arc<AgentTurnContext>) -> Result<String, ToolError> {
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
                "continuable": child.continuable,
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
/// ("`resume <id> generation N`"), which is worse than failing loudly — the
/// parent re-delegates with a fresh task instead.
pub fn resume_messages(
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
    // A continuation labels its nudge `follow-up`; a re-entry after an
    // interruption says `resume` (the child reads the author on the wire).
    let label = if request.handle.continuable {
        "follow-up"
    } else {
        "resume"
    };
    let mut messages = vec![ChatMessage::system(child_system_prompt(def))];
    messages.extend(replayed.iter().cloned());
    messages.push(ChatMessage::user_named(resume_nudge(request), label));
    Ok(messages)
}

/// The child's task prompt (§5): the parent model's own words, plus optional
/// file hints and parent context. The transcript is never copied.
pub fn seed_task_text(seed: &ContextSeed) -> String {
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
pub fn child_system_prompt(def: &AgentDefinition) -> String {
    let tools = def.tools.iter().cloned().collect::<Vec<_>>().join(", ");
    let mut prompt = format!(
        "{persona}\n\n\
         You are a background sub-agent. The main agent delegated this task and \
         will read your final message as the result — report findings in prose \
         with file paths. Your tools: {tools} (anything else is rejected at \
         dispatch; do not try to work around a missing tool). Your persona\n\
         above states what your tools may touch — honor it. Rules that \
         mention editing apply only when you have the tools for it.\n\
         \n\
         Working rules:\n\
         - Inspect before editing; use repository evidence over assumptions.\n\
         - Make minimal changes consistent with existing code.\n\
         - Batch independent tool calls; avoid redundant work.\n\
         - Verify changes with relevant tests, builds, or checks.\n\
         - Diagnose and fix failures within the task's scope.\n\
         - Do not ask questions answerable from the transcript.\n\
         - Do not stop until the requested outcome is implemented and \
           reasonably verified.\n\n\
         Answering: report the result, verification, and relevant \
         limitations concisely in your final message.",
        persona = def.prompt,
        tools = tools,
    );
    if let Some(context) = crate::llm::prompt::project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&context);
    }
    prompt
}

/// Route one child console line onto the wire (plan §20 child view),
/// coalescing the token-voluminous delta kinds exactly like the parent
/// turn's sink pump (`daemon/turn.rs`): providers emit one sink line per
/// thinking token, and each event costs a seq bump, a journal append, and
/// an SSE write. Thinking deltas join verbatim; Assistant events are
/// complete markdown lines and join with `'\n'`. A non-coalescing event
/// flushes the held tail first, so line order never changes.
fn forward_coalesced(
    event: StreamEvent,
    coalescing: bool,
    pending: &mut Option<StreamEvent>,
    emit: &mut dyn FnMut(StreamEvent),
) {
    if !coalescing {
        if let Some(held) = pending.take() {
            emit(held);
        }
        emit(event);
        return;
    }
    match (pending, event) {
        (Some(StreamEvent::Thinking(buf)), StreamEvent::Thinking(text)) => {
            buf.push_str(&text);
        }
        (Some(StreamEvent::AssistantText(buf)), StreamEvent::AssistantText(text)) => {
            buf.push('\n');
            buf.push_str(&text);
        }
        (held, event) => {
            if let Some(held) = held.take() {
                emit(held);
            }
            *held = Some(event);
        }
    }
}

/// The child body handed to [`AgentManager::spawn`]: one or more standard
/// `process_turn` runs with an isolated bundle — no parent transcript, no
/// parent session, no parent cancel token (§8). Unlike before spec G1, the
/// child carries its own steering/follow-up queues: `steer` lands inside
/// the running turn, `follow_up` (and a steer that raced the final answer)
/// chains another turn on the same history once the current one ends.
/// Concrete (boxed) so the spawn signature never resolves through an async
/// opaque: `child_run` calls `process_turn`, and a non-boxed closure return
/// would make the two functions' opaque types cycle.
pub type ChildBody = Box<
    dyn FnOnce(
            CancellationToken,
            ProgressReporter,
            AgentId,
            ChildQueues,
        ) -> Pin<Box<dyn Future<Output = AgentResult> + Send>>
        + Send,
>;

pub fn child_body(
    ctx: Arc<AgentTurnContext>,
    def: AgentDefinition,
    seed: ContextSeed,
    resume: Option<ResumeRequest>,
    child_config: Arc<LlmConfig>,
) -> ChildBody {
    Box::new(move |token, progress, id, queues| {
        Box::pin(child_run(
            ctx,
            def,
            seed,
            token,
            progress,
            id,
            resume,
            child_config,
            queues,
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
    queues: ChildQueues,
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
        .with_questions(ctx.child_questions.clone())
        .with_live_approvals(ctx.live_approvals.clone())
        .with_agent(id.0.clone(), def.name.clone());
    console.seed_session_approvals(ctx.session_approvals.clone());
    let last_assistant = Arc::new(Mutex::new(None::<String>));
    let capture = last_assistant.clone();
    // Clones for the accepted-echo forwarders and the final usage read
    // must exist before the consumer spawn moves `progress` into the sink
    // loop.
    let meter = progress.clone();
    let echo_steer = progress.clone();
    let echo_follow = progress.clone();
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
    // child is currently running, read by the `wait` action), and the §18
    // usage tally (each `record_usage` emission folds into the registry's
    // spend meter — the only tally, so a synthesized ending still reports
    // its spend, spec G3). Under the daemon they also stream the live
    // child transcript (plan §20 child view): each console line maps onto
    // the same `StreamEvent` shape the parent turn streams (turn.rs's sink
    // pump), with the same Thinking coalescing — providers emit one sink
    // line per token, and each event costs a seq bump, a journal append,
    // and an SSE write. `Usage` and `Plan` stay local: the meter lives in
    // the registry and the child never owns a plan.
    let child_name = def.name.clone();
    let sink_drained = progress.expect_sink_drain();
    let consumer = tokio::spawn(async move {
        let mut pending: Option<StreamEvent> = None;
        let mut emit = |event: StreamEvent| progress.emit_line(&child_name, event);
        while let Some(line) = sink_rx.recv().await {
            match line {
                SinkLine::Assistant(text) => {
                    *capture.lock().unwrap_or_else(|e| e.into_inner()) = Some(text.clone());
                    forward_coalesced(
                        StreamEvent::AssistantText(text),
                        true,
                        &mut pending,
                        &mut emit,
                    );
                }
                SinkLine::Thinking(text) => {
                    forward_coalesced(StreamEvent::Thinking(text), true, &mut pending, &mut emit)
                }
                SinkLine::ToolInput { id, input } => {
                    // Preview is "<name> <short-args>" (loop.rs emit shape).
                    let name = input.split(' ').next().unwrap_or_default();
                    progress.set(name);
                    *open_in.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    *tally_calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
                    let mut parts = input.splitn(2, ' ');
                    let tool = parts.next().unwrap_or_default().to_string();
                    let args = parts.next().unwrap_or_default().to_string();
                    forward_coalesced(
                        StreamEvent::ToolCall {
                            name: tool,
                            args: serde_json::Value::String(args),
                            id,
                        },
                        false,
                        &mut pending,
                        &mut emit,
                    );
                }
                SinkLine::ToolOutput {
                    id,
                    name,
                    summary,
                    success,
                    preview,
                    duration,
                } => {
                    let mut open = open_out.lock().unwrap_or_else(|e| e.into_inner());
                    *open = open.saturating_sub(1);
                    if *open == 0 {
                        progress.clear();
                    }
                    forward_coalesced(
                        StreamEvent::ToolResult {
                            name,
                            summary,
                            success,
                            preview,
                            duration,
                            id,
                        },
                        false,
                        &mut pending,
                        &mut emit,
                    );
                }
                SinkLine::Usage {
                    tokens,
                    output,
                    cost,
                    ..
                } => {
                    // Straight into the registry's spend meter: it outlives
                    // the body, so the wrapper can attach spend to a
                    // synthesized result (spec G3).
                    progress.absorb_usage(tokens, output, cost);
                }
                SinkLine::System(text) => {
                    forward_coalesced(StreamEvent::System(text), false, &mut pending, &mut emit)
                }
                SinkLine::Error(text) => {
                    forward_coalesced(StreamEvent::Error(text), false, &mut pending, &mut emit)
                }
                _ => {}
            }
        }
        // Channel closed: flush the coalesced tail.
        if let Some(event) = pending.take() {
            emit(event);
        }
        sink_drained.notify_one();
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
    // `ask_user` is always available to children (like `ls`, it never
    // prompts): a child blocked on a decision must be able to ask, and its
    // question rides the same labeled prompt path as its approvals.
    allowed.insert("ask_user".to_string());
    // Session tasks are shared: any agent in the session can poll/stop them.
    allowed.insert(crate::agent::delegate::TASK_TOOL.to_string());
    if may_delegate {
        allowed.insert(DELEGATION_TOOL.to_string());
    }
    let filter = ToolFilter {
        owner: def.name.clone(),
        allowed,
    };
    // The child's daemon context for one more level, when allowed. Shares
    // the session coordinates, manager, and approval bridges — only the
    // depth advances. The config is the child's *resolved* model (parent
    // model plus definition / per-spawn `model` override), so a grandchild
    // without its own override inherits the per-spawn model rather
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
            child_questions: ctx.child_questions.clone(),
            live_approvals: ctx.live_approvals.clone(),
        }))
    } else {
        None
    };
    // Agent lifecycle hooks (plan §7 P2): fired around each child turn with
    // the child's own policy, so a nested `dex.tools.call` from a hook is
    // gated exactly like a model-issued child call. A panicked or timed-out
    // body never reaches the end event — the registry result is the record
    // for those (spawn_wrapper owns the abnormal exits).
    // Scoped so the hook-host policy (which clones the console, keeping the
    // child sink open) drops before `drop(console)` below — otherwise the
    // consumer await deadlocks on a channel that never closes.
    let ChildQueues {
        steering_rx: mut steer_rx,
        followup_rx: mut follow_rx,
    } = queues;
    // Acceptance echoes (the child's counterpart of the daemon's main-turn
    // forwarder): a consumed steer or a chained follow-up shows in the
    // child's live transcript view with the same event the parent uses.
    let (steer_accepted_tx, steer_accepted_rx) = mpsc::channel::<String>(16);
    let (followup_accepted_tx, followup_accepted_rx) = mpsc::channel::<String>(16);
    spawn_accepted_forwarder(echo_steer, def.name.clone(), steer_accepted_rx, |c| {
        StreamEvent::SteeringAccepted { content: c }
    });
    spawn_accepted_forwarder(echo_follow, def.name.clone(), followup_accepted_rx, |c| {
        StreamEvent::FollowupAccepted { content: c }
    });
    // §8 + spec G1: one `process_turn` per turn; when the turn ends with a
    // follow-up parked (or a steer that raced the final answer), the same
    // history chains another turn — journal markers stay per-turn (§16).
    // Drain points inside `process_turn` are the main turn's: top of every
    // loop iteration and after an assistant message without tool calls.
    let mut first_turn = true;
    let mut last_text = String::new();
    let ending: TurnEnd = loop {
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
                steering_rx: Some(&mut steer_rx),
                steering_accepted_tx: Some(&steer_accepted_tx),
                session: Some(&mut session),
                client: &config,
                cancel: &token,
                console: &console,
                filter: Some(&filter),
                agent_ctx: child_ctx.clone(),
                // Resume honors the remaining meter (§24.2) on the first turn
                // only; a continuation, a fresh spawn and every chained turn
                // run the definition cap (the wall-clock timeout bounds the
                // whole chain).
                tool_budget: if first_turn {
                    resume
                        .as_ref()
                        .and_then(|request| request.handle.remaining_budget)
                } else {
                    None
                }
                .or_else(|| def.max_tool_iterations.map(|n| n as usize)),
                harness: None,
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
        first_turn = false;
        let _ = session.turn_event(if result.is_ok() {
            "turn_complete"
        } else {
            "turn_failed"
        });
        match result {
            Err(error) => break TurnEnd::Failed(error.to_string()),
            Ok(text) => {
                // Post-turn drain: a steered message that raced the child's
                // final answer (past `process_turn`'s last drain point) and
                // every queued follow-up chain one more turn; arrival order
                // is preserved, recalls remove only not-yet-injected text.
                let mut queued: Vec<(String, &'static str)> = Vec::new();
                drain_inboxes(
                    &mut steer_rx,
                    &mut follow_rx,
                    &steer_accepted_tx,
                    &followup_accepted_tx,
                    &mut queued,
                )
                .await;
                if queued.is_empty() {
                    // Stop accepting under the manager lock, then drain once
                    // more: a send either landed before the flip (caught
                    // here) or is refused, never accepted and lost.
                    meter.set_accepting(false);
                    drain_inboxes(
                        &mut steer_rx,
                        &mut follow_rx,
                        &steer_accepted_tx,
                        &followup_accepted_tx,
                        &mut queued,
                    )
                    .await;
                    if queued.is_empty() {
                        // Completed guarantees a non-empty summary (§6): an
                        // empty final message is not a usable result.
                        // A chained turn that ends empty keeps the previous
                        // turn's answer rather than discarding good work.
                        let text = if text.trim().is_empty() {
                            std::mem::take(&mut last_text)
                        } else {
                            text
                        };
                        if text.trim().is_empty() {
                            break TurnEnd::EmptyFinal;
                        } else {
                            break TurnEnd::Completed(text);
                        }
                    }
                    meter.set_accepting(true);
                }
                if !text.trim().is_empty() {
                    last_text = text;
                }
                let _ = session.turn_event("turn_start");
                for (content, kind) in queued {
                    let message = ChatMessage::user_named(content, kind);
                    let _ = session.append_message(&message);
                    messages.push(message);
                }
            }
        }
    };
    // Dropping the console closes the child sink, so the consumer drains
    // every buffered line and exits — awaiting it makes the captured
    // summary and §18 usage deterministic instead of racing the last
    // sends. (`SinkLine::Usage` is emitted per LLM call, after the final
    // assistant text.)
    drop(console);
    let _ = consumer.await;
    let partial = last_assistant
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
    // The registry meter is the only tally (spec G3): the sink is fully
    // drained here, so the snapshot is the total across every turn.
    let usage = meter.usage_snapshot().reported();
    let tool_calls = *calls.lock().unwrap_or_else(|e| e.into_inner());
    match ending {
        TurnEnd::Completed(text) => AgentResult {
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
        TurnEnd::EmptyFinal => AgentResult {
            status: AgentState::Failed,
            summary: partial,
            error: Some("child ended without a final message".to_string()),
            usage,
            reason: ExitReason::Permanent,
            tool_calls,
            resume: None,
        },
        TurnEnd::Failed(message) => AgentResult {
            status: AgentState::Failed,
            summary: partial,
            error: Some(message.clone()),
            usage,
            reason: classify_body_error(&message),
            tool_calls,
            resume: None,
        },
    }
}

/// How the turn chain ended inside `child_run` — the shell for the
/// [`AgentResult`] the body synthesizes after its sink has drained.
enum TurnEnd {
    Completed(String),
    EmptyFinal,
    Failed(String),
}

/// Move everything parked in the child's steering and follow-up inboxes into
/// `queued` (arrival order per queue, steering first), echoing each accepted
/// content message into the child's live transcript.
async fn drain_inboxes(
    steer_rx: &mut mpsc::Receiver<crate::protocol::QueueMsg>,
    follow_rx: &mut mpsc::Receiver<crate::protocol::QueueMsg>,
    steer_accepted_tx: &mpsc::Sender<String>,
    followup_accepted_tx: &mpsc::Sender<String>,
    queued: &mut Vec<(String, &'static str)>,
) {
    while let Ok(msg) = steer_rx.try_recv() {
        if let crate::protocol::QueueMsg::Content(content) = &msg {
            let _ = steer_accepted_tx.send(content.clone()).await;
        }
        enqueue(queued, msg, "steering");
    }
    while let Ok(msg) = follow_rx.try_recv() {
        if let crate::protocol::QueueMsg::Content(content) = &msg {
            let _ = followup_accepted_tx.send(content.clone()).await;
        }
        enqueue(queued, msg, "follow-up");
    }
}

/// Apply one drained queue message to the not-yet-appended list, keeping
/// each message's journal kind ("steering" vs "follow-up") alongside it.
/// Recall semantics match `apply_queue_msg`: only a queued item can be
/// recalled; one already pushed into the conversation is history.
fn enqueue(
    pending: &mut Vec<(String, &'static str)>,
    msg: crate::protocol::QueueMsg,
    kind: &'static str,
) {
    match msg {
        crate::protocol::QueueMsg::Content(text) => pending.push((text, kind)),
        crate::protocol::QueueMsg::Recall(text) => {
            if let Some(pos) = pending.iter().rposition(|(item, _)| *item == text) {
                pending.remove(pos);
            }
        }
    }
}

/// Echo consumed sends into the child's live transcript view, the child
/// counterpart of the main daemon turn's accepted forwarder.
fn spawn_accepted_forwarder(
    progress: ProgressReporter,
    name: String,
    mut rx: mpsc::Receiver<String>,
    wrap: fn(String) -> StreamEvent,
) {
    tokio::spawn(async move {
        while let Some(content) = rx.recv().await {
            progress.emit_line(&name, wrap(content));
        }
    });
}
