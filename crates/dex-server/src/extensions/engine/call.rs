//! One extension call on the worker thread: drive-model context, the
//! deadline hook, dispatch by [`CallKind`] (tool call / event handlers /
//! command handler), directive merging, and tool-result stringification.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Error as LuaError, Function, Lua, MultiValue, Table, Value, VmState};
use serde_json::{Map, Value as Json};

use super::{
    json_to_lua, lua_to_json, lua_type_name, stringify_json, stringify_tool_result, CallKind,
    WorkerMsg, WorkerRegistrations,
};
use crate::extensions::Manifest;
use tokio::sync::mpsc;

/// Wall-clock backstop for an agent loop (spec §9): model rounds legitimately
/// take minutes, so there is no per-call deadline — the task side sets the
/// abort flag on turn end/cancellation. This backstop only catches a worker
/// nobody is waiting on or cancelled, e.g. a loop that never surfaces.
const LOOP_BACKSTOP: Duration = Duration::from_secs(3600);

// The drive's model context on the worker thread: set by `run_call` for
// the drive's duration, read by the sync `dex.model.*` closures (which
// cannot reach the task-local — the worker is a plain OS thread). Drives
// never interleave on a worker, so one slot is enough.
std::thread_local! {
    static DRIVE_WORKER: RefCell<Option<super::super::DriveModel>> = const { RefCell::new(None) };
}

/// This drive's model context, if the running call carries one.
pub(super) fn worker_drive_model() -> Option<super::super::DriveModel> {
    DRIVE_WORKER.with(|slot| slot.borrow().clone())
}

/// Restores the worker's previous drive context when the drive ends
/// (save/restore keeps nesting honest if a drive ever re-enters).
pub(super) struct WorkerDriveGuard {
    prev: Option<super::super::DriveModel>,
}

impl Drop for WorkerDriveGuard {
    fn drop(&mut self) {
        DRIVE_WORKER.with(|slot| *slot.borrow_mut() = self.prev.take());
    }
}

/// Run one call on the worker: pin the drive's model context, arm the
/// deadline hook, dispatch by kind, and always terminate with exactly one
/// `Done`.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_call(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    dir: &Path,
    kind: CallKind,
    args: Json,
    timeout: Duration,
    call_id: &str,
    model: Option<super::super::DriveModel>,
    tx: &mpsc::UnboundedSender<WorkerMsg>,
) {
    // Pin this drive's model for `dex.model.*` below; the guard restores
    // the previous slot (none, outside nesting) on every exit path.
    let _drive_guard = DRIVE_WORKER.with(|slot| {
        let prev = slot.borrow().clone();
        *slot.borrow_mut() = model;
        WorkerDriveGuard { prev }
    });
    // Deadline + cancellation hook: the task sets `aborted` on timeout (via
    // reply-drop) — but the primary path is the task-side deadline in
    // `drive`, which abandons first. The hook still bounds pure-Lua runaway
    // when the task is waiting on `Done`.
    let abort = Arc::new(AtomicBool::new(false));
    let hook_abort = Arc::clone(&abort);
    let started = Instant::now();
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            if hook_abort.load(Ordering::Relaxed) || started.elapsed() > timeout {
                return Err(LuaError::RuntimeError("extension call timed out".into()));
            }
            Ok(VmState::Continue)
        },
    )
    .expect("hook arm");
    // Route host upcalls from this call to the task side.
    let _ = lua.set_app_data(tx.clone());
    let done = |result: Result<String, String>| {
        lua.remove_app_data::<mpsc::UnboundedSender<WorkerMsg>>();
        lua.remove_hook();
        let _ = tx.send(WorkerMsg::Done(result));
    };
    let ctx = match call_context(lua, manifest, dir, &kind, call_id) {
        Ok(ctx) => ctx,
        Err(e) => {
            done(Err(e));
            return;
        }
    };
    let is_shadow = matches!(kind, CallKind::Shadow { .. });
    let result = match kind {
        CallKind::Tool { tool } | CallKind::Shadow { target: tool } => {
            run_tool_call(lua, manifest, regs, tool, is_shadow, args, &ctx)
        }
        CallKind::Event { event } => run_event_handlers(lua, manifest, regs, event, args, &ctx),
        CallKind::Command { name } => run_command_handler(lua, manifest, regs, name, args, &ctx),
    };
    abort.store(true, Ordering::Relaxed);
    done(result);
}

/// Drive the registered agent loop on the worker (spec §9, agent_loop.v1).
/// The loop owns iteration; every engine step is a blocking host upcall the
/// turn driver answers on the task side with the turn's real state. The
/// instruction hook enforces the caller's `abort` flag (no fixed deadline —
/// model rounds take minutes) plus a generous wall-clock backstop. The
/// return value is the loop's result envelope: a string is `{text}`, a
/// table may carry `{text}` and/or `{error}`.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_agent_loop(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    dir: &Path,
    call_id: &str,
    abort: Arc<AtomicBool>,
    model: Option<super::super::DriveModel>,
    tx: &mpsc::UnboundedSender<WorkerMsg>,
) {
    // Same drive-model pin as `run_call`: `dex.model.*` serves this turn.
    let _drive_guard = DRIVE_WORKER.with(|slot| {
        let prev = slot.borrow().clone();
        *slot.borrow_mut() = model;
        WorkerDriveGuard { prev }
    });
    let started = Instant::now();
    let hook_abort = Arc::clone(&abort);
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            if hook_abort.load(Ordering::Relaxed) {
                return Err(LuaError::RuntimeError("agent loop aborted".into()));
            }
            if started.elapsed() > LOOP_BACKSTOP {
                return Err(LuaError::RuntimeError(
                    "agent loop exceeded its wall-clock backstop".into(),
                ));
            }
            Ok(VmState::Continue)
        },
    )
    .expect("hook arm");
    let _ = lua.set_app_data(tx.clone());
    let done = |result: Result<String, String>| {
        lua.remove_app_data::<mpsc::UnboundedSender<WorkerMsg>>();
        lua.remove_hook();
        let _ = tx.send(WorkerMsg::Done(result));
    };
    let (loop_id, run) = match regs.borrow().agent_loop.clone() {
        Some(pair) => pair,
        None => {
            done(Err(
                "agent loop requested but none is registered".to_string()
            ));
            return;
        }
    };
    let ctx = match loop_context(lua, manifest, dir, &loop_id, call_id) {
        Ok(ctx) => ctx,
        Err(e) => {
            done(Err(e));
            return;
        }
    };
    let ev_id = manifest.id_for_error();
    let returned: Value = match run.call((ctx,)) {
        Ok(v) => v,
        Err(e) => {
            done(Err(format!(
                "extension '{ev_id}' agent loop '{loop_id}' failed: {e}"
            )));
            return;
        }
    };
    abort.store(true, Ordering::Relaxed);
    let envelope: Result<Json, String> = match returned {
        Value::String(s) => s
            .to_str()
            .map(|text| {
                Json::Object(
                    [("text".to_string(), Json::String(text.to_string()))]
                        .into_iter()
                        .collect(),
                )
            })
            .map_err(|e| e.to_string()),
        Value::Table(t) => lua_to_json(Value::Table(t)).and_then(|json| {
            let mut out = Map::new();
            match json {
                Json::Object(map) => {
                    if let Some(text) = map.get("text").and_then(|v| v.as_str()) {
                        out.insert("text".to_string(), Json::String(text.to_string()));
                    }
                    if let Some(error) = map.get("error").and_then(|v| v.as_str()) {
                        out.insert("error".to_string(), Json::String(error.to_string()));
                    }
                }
                _ => {
                    return Err(format!(
                        "extension '{ev_id}' agent loop '{loop_id}' must return a string or a table"
                    ));
                }
            }
            if out.is_empty() {
                return Err(format!(
                    "extension '{ev_id}' agent loop '{loop_id}' returned no text/error"
                ));
            }
            Ok(Json::Object(out))
        }),
        other => Err(format!(
            "extension '{ev_id}' agent loop '{loop_id}' must return a string or a table, got {}",
            lua_type_name(&other)
        )),
    };
    done(envelope.map(|envelope| envelope.to_string()));
}

/// The `ctx` handed to an agent loop's `run(ctx)`: identity fields plus the
/// agent_loop.v1 step surface. Every step is a blocking host upcall (the
/// worker never touches async code); JSON in, JSON out.
fn loop_context(
    lua: &Lua,
    manifest: &Manifest,
    dir: &Path,
    loop_id: &str,
    call_id: &str,
) -> Result<Table, String> {
    let ext_id = manifest.id.clone();
    let ctx = lua.create_table().map_err(|e| e.to_string())?;
    ctx.set("extension", ext_id.clone())
        .map_err(|e| e.to_string())?;
    ctx.set("workspace", dir.display().to_string())
        .map_err(|e| e.to_string())?;
    ctx.set("call_id", call_id).map_err(|e| e.to_string())?;
    ctx.set("loop", loop_id).map_err(|e| e.to_string())?;
    ctx.set("interface", "agent_loop.v1")
        .map_err(|e| e.to_string())?;

    let upcall_table = |name: &str, member: &str, f: mlua::Function| -> Result<(), String> {
        let t = lua.create_table().map_err(|e| e.to_string())?;
        t.set(member, f).map_err(|e| e.to_string())?;
        ctx.set(name, t).map_err(|e| e.to_string())
    };

    // ctx.model.call(): one engine round. Reply is the envelope table
    // `{content?, tool_calls? = [{id,name,args}], retry?}` or a Lua error.
    {
        let ext_id = ext_id.clone();
        let f = lua
            .create_function(move |lua, _: ()| {
                let json = super::host_upcall(lua, &ext_id, super::HostOp::LoopModel)?;
                let value: Json = serde_json::from_str(&json)
                    .map_err(|e| LuaError::RuntimeError(format!("bad model reply: {e}")))?;
                json_to_lua(lua, &value).map_err(|e| LuaError::RuntimeError(e.to_string()))
            })
            .map_err(|e| e.to_string())?;
        upcall_table("model", "call", f)?;
    }
    // ctx.tools.execute(): run the last response's pending tool calls
    // through the host (hooks, gates, dispatch) + budget. Reply
    // `{completed, limit}` or `{exhausted, note}`.
    {
        let ext_id = ext_id.clone();
        let f = lua
            .create_function(move |lua, _: ()| {
                let json = super::host_upcall(lua, &ext_id, super::HostOp::LoopTools)?;
                let value: Json = serde_json::from_str(&json)
                    .map_err(|e| LuaError::RuntimeError(format!("bad tools reply: {e}")))?;
                json_to_lua(lua, &value).map_err(|e| LuaError::RuntimeError(e.to_string()))
            })
            .map_err(|e| e.to_string())?;
        upcall_table("tools", "execute", f)?;
    }
    // ctx.finish(response): finish_response — steering + persistence.
    // Reply `{steered = bool}`: true means run another round.
    {
        let ext_id = ext_id.clone();
        let f = lua
            .create_function(move |lua, response: String| {
                let json =
                    super::host_upcall(lua, &ext_id, super::HostOp::LoopFinish { response })?;
                let value: Json = serde_json::from_str(&json)
                    .map_err(|e| LuaError::RuntimeError(format!("bad finish reply: {e}")))?;
                json_to_lua(lua, &value).map_err(|e| LuaError::RuntimeError(e.to_string()))
            })
            .map_err(|e| e.to_string())?;
        ctx.set("finish", f).map_err(|e| e.to_string())?;
    }
    // ctx.cancelled(): is the turn cancelled?
    {
        let ext_id = ext_id.clone();
        let f = lua
            .create_function(move |lua, _: ()| {
                let json = super::host_upcall(lua, &ext_id, super::HostOp::LoopCancelled)?;
                Ok(json == "true")
            })
            .map_err(|e| e.to_string())?;
        ctx.set("cancelled", f).map_err(|e| e.to_string())?;
    }
    // ctx.state(): `{cancelled, rounds, round_limit, messages}`.
    {
        let ext_id = ext_id.clone();
        let f = lua
            .create_function(move |lua, _: ()| {
                let json = super::host_upcall(lua, &ext_id, super::HostOp::LoopState)?;
                let value: Json = serde_json::from_str(&json)
                    .map_err(|e| LuaError::RuntimeError(format!("bad state reply: {e}")))?;
                json_to_lua(lua, &value).map_err(|e| LuaError::RuntimeError(e.to_string()))
            })
            .map_err(|e| e.to_string())?;
        ctx.set("state", f).map_err(|e| e.to_string())?;
    }
    Ok(ctx)
}

fn call_context(
    lua: &Lua,
    manifest: &Manifest,
    dir: &Path,
    kind: &CallKind,
    call_id: &str,
) -> Result<Table, String> {
    let ctx = lua.create_table().map_err(|e| e.to_string())?;
    ctx.set("extension", manifest.id.clone())
        .map_err(|e| e.to_string())?;
    ctx.set("workspace", dir.display().to_string())
        .map_err(|e| e.to_string())?;
    ctx.set("call_id", call_id).map_err(|e| e.to_string())?;
    if let CallKind::Tool { tool } | CallKind::Shadow { target: tool } = kind {
        ctx.set("tool", tool.clone()).map_err(|e| e.to_string())?;
    }
    Ok(ctx)
}

fn run_tool_call(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    tool: String,
    is_shadow: bool,
    args: Json,
    ctx: &Table,
) -> Result<String, String> {
    let func = regs.borrow().tools.get(&tool).cloned().ok_or_else(|| {
        format!(
            "extension '{}' has no tool '{tool}'",
            manifest.id_for_error()
        )
    })?;
    if is_shadow {
        regs.borrow_mut().current_shadow = Some(tool.clone());
    }
    let args_table = json_to_lua(lua, &args).map_err(|e| e.to_string())?;
    let args_table: Table = match args_table {
        Value::Table(t) => t,
        _ => {
            if is_shadow {
                regs.borrow_mut().current_shadow = None;
            }
            return Err(format!(
                "extension '{}' tool '{tool}': args must be a JSON object",
                manifest.id_for_error()
            ));
        }
    };
    let returned: MultiValue = func.call((ctx.clone(), args_table)).map_err(|e| {
        if is_shadow {
            regs.borrow_mut().current_shadow = None;
        }
        format!(
            "extension '{}' tool '{tool}' failed: {e}",
            manifest.id_for_error()
        )
    })?;
    if is_shadow {
        regs.borrow_mut().current_shadow = None;
    }
    stringify_tool_result(returned, manifest.id_for_error(), &tool)
}

/// Run one extension's handlers for an event in registration order, threading
/// the event table through. Returns the directive envelope as JSON:
/// `{args, deny, reason, content, is_error, append, instructions, cancel,
/// summary}` — only the keys a handler set are present. A deny
/// short-circuits the remaining handlers.
fn run_event_handlers(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    event: String,
    args: Json,
    ctx: &Table,
) -> Result<String, String> {
    let handlers = regs
        .borrow()
        .events
        .get(&event)
        .cloned()
        .unwrap_or_default();
    let wraps = regs.borrow().wraps.get(&event).cloned().unwrap_or_default();
    let ev_id = manifest.id_for_error().to_string();
    let ev_table: Table = match json_to_lua(lua, &args).map_err(|e| e.to_string())? {
        Value::Table(t) => t,
        _ => {
            return Err(format!(
                "extension '{ev_id}' event '{event}': payload must be a JSON object"
            ));
        }
    };
    // A stable handle for the `dex.fallback` backup call: the main chain
    // consumes the table (and middleware may hand it its own mutations —
    // the fallback sees the original payload).
    let ev_for_fallback = ev_table.clone();

    // The chain bottom: run the handler fold over `ev`, returning the
    // envelope as a Lua table so middleware can observe and rewrite it.
    let run_fold = {
        let ev_id = ev_id.clone();
        let event = event.clone();
        let ctx = ctx.clone();
        let handlers = handlers.clone();
        move |lua: &Lua, ev: Table| -> Result<Table, String> {
            let mut envelope = fold_handlers(&ctx, &handlers, ev.clone(), &ev_id, &event)?;
            envelope = fold_ev_mutations(ev, envelope, &ev_id, &event)?;
            match json_to_lua(lua, &Json::Object(envelope)) {
                Ok(Value::Table(t)) => Ok(t),
                _ => Err(format!(
                    "extension '{ev_id}' event '{event}': cannot build envelope"
                )),
            }
        }
    };

    let envelope = if wraps.is_empty() {
        let mut envelope = fold_handlers(ctx, &handlers, ev_table.clone(), &ev_id, &event)?;
        envelope = fold_ev_mutations(ev_table, envelope, &ev_id, &event)?;
        run_slot_fallback(lua, regs, &event, ctx, &ev_for_fallback, envelope, &ev_id)?
    } else {
        // Middleware chain, registration order outermost first: fold the
        // layers around the handler fold in reverse. `next(ev)` is
        // re-entrant (a retry middleware may call it repeatedly); a `nil`
        // return is a pass-through. A string return is the convenient
        // `{content = ...}` form.
        let ev_id_mw = ev_id.clone();
        let event_mw = event.clone();
        let mut next: Function = lua
            .create_function(move |lua, (ev,): (Table,)| {
                run_fold(lua, ev).map_err(LuaError::RuntimeError)
            })
            .map_err(|e| e.to_string())?;
        for mw in wraps.iter().rev() {
            let inner = next.clone();
            let mw = mw.clone();
            next = lua
                .create_function(move |lua, (ev,): (Table,)| -> Result<Value, mlua::Error> {
                    let returned: Value = mw.call((inner.clone(), ev.clone()))?;
                    match returned {
                        Value::Nil => inner.call((ev,)),
                        Value::String(s) => {
                            let t = lua.create_table()?;
                            t.set("content", s)?;
                            Ok(Value::Table(t))
                        }
                        other => Ok(other),
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        let returned: Value = next.call((ev_table,)).map_err(|e| {
            format!("extension '{ev_id_mw}' event '{event_mw}' middleware failed: {e}")
        })?;
        let envelope: Map<String, Json> = match returned {
            Value::Table(t) => match lua_to_json(Value::Table(t))
                .map_err(|e| format!("extension '{ev_id}' event '{event}': {e}"))?
            {
                Json::Object(map) => map,
                _ => {
                    return Err(format!(
                        "extension '{ev_id}' event '{event}': middleware must return an object"
                    ));
                }
            },
            Value::String(s) => {
                let mut map = Map::new();
                map.insert(
                    "content".to_string(),
                    Json::String(s.to_str().map_err(|e| e.to_string())?.to_string()),
                );
                map
            }
            Value::Nil => Map::new(),
            other => {
                return Err(format!(
                    "extension '{ev_id}' event '{event}': middleware must return nil, a string, or a table, got {}",
                    lua_type_name(&other)
                ));
            }
        };
        run_slot_fallback(lua, regs, &event, ctx, &ev_for_fallback, envelope, &ev_id)?
    };
    serde_json::to_string(&Json::Object(envelope)).map_err(|e| e.to_string())
}

/// `dex.fallback` (spec §34): when the slot's whole chain produced no
/// opinion (no directive key set — every handler no-opped, errored, or
/// there were none), run the registered backup before the Rust default. An
/// opinion from the chain is never second-guessed.
fn run_slot_fallback(
    _lua: &Lua,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    event: &str,
    ctx: &Table,
    ev_table: &Table,
    mut envelope: Map<String, Json>,
    ev_id: &str,
) -> Result<Map<String, Json>, String> {
    let fallback = regs.borrow().fallbacks.get(event).cloned();
    let no_opinion = !envelope
        .keys()
        .any(|k| DIRECTIVE_KEYS.contains(&k.as_str()));
    if no_opinion {
        if let Some(backup) = fallback {
            let returned: Value = backup
                .call((ctx.clone(), ev_table.clone()))
                .map_err(|e| format!("extension '{ev_id}' event '{event}' fallback failed: {e}"))?;
            merge_directive(returned, &mut envelope, ev_id, event)?;
        }
    }
    Ok(envelope)
}

/// Fold the handler chain over `ev` into a directive envelope: handlers run
/// in registration order, each seeing the same event table; a deny
/// short-circuits the rest.
fn fold_handlers(
    ctx: &Table,
    handlers: &[Function],
    ev_table: Table,
    ev_id: &str,
    event: &str,
) -> Result<Map<String, Json>, String> {
    let mut envelope = Map::new();
    for handler in handlers {
        let returned: Value = handler
            .call((ctx.clone(), ev_table.clone()))
            .map_err(|e| format!("extension '{ev_id}' event '{event}' failed: {e}"))?;
        merge_directive(returned, &mut envelope, ev_id, event)?;
        if envelope.get("deny").and_then(|v| v.as_bool()) == Some(true) {
            break;
        }
    }
    Ok(envelope)
}

/// Fold every serializable `ev` mutation back into the envelope (explicit
/// handler returns take precedence): this is what makes `ev.args.command = …`
/// (tool.before) and `ev.content = …` / `ev.is_error = …` (tool.after) reach
/// the host even when the handler returns nil.
fn fold_ev_mutations(
    ev_table: Table,
    mut envelope: Map<String, Json>,
    ev_id: &str,
    event: &str,
) -> Result<Map<String, Json>, String> {
    for pair in ev_table.pairs::<Value, Value>() {
        let (key, value) = match pair {
            Ok(kv) => kv,
            Err(_) => continue,
        };
        let Value::String(key) = key else { continue };
        let key = key.to_str().map_err(|e| e.to_string())?.to_string();
        if value == Value::Nil || envelope.contains_key(&key) {
            continue;
        }
        let json = lua_to_json(value).map_err(|e| {
            format!("extension '{ev_id}' event '{event}': cannot serialize ev.{key}: {e}")
        })?;
        envelope.insert(key, json);
    }
    Ok(envelope)
}

/// Run one registered command handler with the rest-of-line payload.
fn run_command_handler(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    name: String,
    args: Json,
    ctx: &Table,
) -> Result<String, String> {
    let ev_id = manifest.id_for_error();
    let execute = regs
        .borrow()
        .commands
        .get(&name)
        .map(|(_, f)| f.clone())
        .ok_or_else(|| format!("extension '{ev_id}' has no command '{name}'"))?;
    let payload = json_to_lua(lua, &args).map_err(|e| e.to_string())?;
    let returned: Value = execute
        .call((ctx.clone(), payload))
        .map_err(|e| format!("extension '{ev_id}' command '{name}' failed: {e}"))?;
    match returned {
        Value::Nil => Ok("null".to_string()),
        value => lua_to_json(value).map(|json| json.to_string()),
    }
}

/// Directive-envelope keys the host reads from event handlers (the
/// whitelist `merge_directive` folds; anything else on a handler's return
/// is ignored). The ev-mutation fold uses the same set for precedence.
const DIRECTIVE_KEYS: &[&str] = &[
    "deny",
    "reason",
    "content",
    "is_error",
    "append",
    "instructions",
    "cancel",
    "summary",
    "overflow",
    "conflicts",
    "compact",
    "decision",
    "redirect",
    "model",
    "keep",
    "drop",
];

/// Fold one handler's return into the directive envelope: nil continues the
/// chain, a string is result content (the convenient `tool.after` form), a
/// table sets directive keys. Anything else is a fail-open/fail-closed
/// decision for the manager — reported here as an error.
fn merge_directive(
    returned: Value,
    envelope: &mut Map<String, Json>,
    ev_id: &str,
    event: &str,
) -> Result<(), String> {
    match returned {
        Value::Nil => Ok(()),
        Value::String(s) => {
            envelope.insert(
                "content".to_string(),
                Json::String(s.to_str().map_err(|e| e.to_string())?.to_string()),
            );
            Ok(())
        }
        Value::Table(t) => {
            for &key in DIRECTIVE_KEYS {
                let value: Value = t.get(key).map_err(|e| e.to_string())?;
                if !matches!(value, Value::Nil) {
                    let json = match key {
                        "content" | "append" | "instructions" | "summary" | "reason"
                        | "agent" | "model" => {
                            match value {
                                Value::String(s) => Json::String(
                                    s.to_str().map_err(|e| e.to_string())?.to_string(),
                                ),
                                other => stringify_json(&other)?,
                            }
                        }
                        _ => lua_to_json(value)?,
                    };
                    envelope.insert(key.to_string(), json);
                }
            }
            Ok(())
        }
        other => Err(format!(
            "extension '{ev_id}' event '{event}': handler must return nil, a string, or a table, got {}",
            lua_type_name(&other)
        )),
    }
}
