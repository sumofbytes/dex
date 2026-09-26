//! One extension call on the worker thread: drive-model context, the
//! deadline hook, dispatch by [`CallKind`] (tool call / event handlers /
//! command handler), directive merging, and tool-result stringification.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Error as LuaError, Lua, MultiValue, Table, Value, VmState};
use serde_json::{Map, Value as Json};

use super::{
    json_to_lua, lua_to_json, lua_type_name, stringify_json, stringify_tool_result, CallKind,
    WorkerMsg, WorkerRegistrations,
};
use crate::extensions::Manifest;
use tokio::sync::mpsc;

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
    let ev_id = manifest.id_for_error();
    let ev_table: Table = match json_to_lua(lua, &args).map_err(|e| e.to_string())? {
        Value::Table(t) => t,
        _ => {
            return Err(format!(
                "extension '{ev_id}' event '{event}': payload must be a JSON object"
            ));
        }
    };
    let mut envelope = Map::new();
    for handler in &handlers {
        let returned: Value = handler
            .call((ctx.clone(), ev_table.clone()))
            .map_err(|e| format!("extension '{ev_id}' event '{event}' failed: {e}"))?;
        merge_directive(returned, &mut envelope, ev_id, &event)?;
        if envelope.get("deny").and_then(|v| v.as_bool()) == Some(true) {
            break;
        }
    }
    // Mutations to the event table are directives too: fold every
    // serializable ev key back into the envelope, with explicit handler
    // returns taking precedence. This is what makes `ev.args.command = …`
    // (tool.before) and `ev.content = …` / `ev.is_error = …` (tool.after)
    // reach the host even when the handler returns nil.
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
    serde_json::to_string(&Json::Object(envelope)).map_err(|e| e.to_string())
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
            for key in [
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
            ] {
                let value: Value = t.get(key).map_err(|e| e.to_string())?;
                if !matches!(value, Value::Nil) {
                    let json = match key {
                        "content" | "append" | "instructions" | "summary" | "reason" => {
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
