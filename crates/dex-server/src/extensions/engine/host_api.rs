//! The `dex.*` host API surface handed to each extension sandbox: one table
//! builder per namespace (`tools`, `events`, `log`, `workspace`, `commands`,
//! `state`, `prompt`, `model`, `net`, `json`), plus the JSON<->Lua conversion
//! helpers and the host-upcall bridge. Closures capture worker-side
//! registration state; host routing happens on the task side of the channel.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Error as LuaError, Function, Lua, MultiValue, Table, Value};
use serde_json::Value as Json;

use super::super::Manifest;
use super::{
    host_upcall, json_to_lua, lua_to_json, lua_type_name, lua_value_to_string, valid_segment,
    worker_drive_model, HostOp, WorkerRegistrations, KNOWN_EVENTS, SLOT_INTERFACES,
    WRAPPABLE_SLOTS,
};
use crate::tools::resolve_workspace_path;

/// `dex.tools.*` — registration, host-mediated calls, shadow rails.
pub(super) fn tools_table(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Table {
    let ext_id = manifest.id.clone();
    let tools = lua.create_table().expect("dex.tools table");

    // dex.tools.register({ name, execute, override? })
    {
        let regs = Rc::clone(regs);
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        tools
            .set(
                "register",
                lua.create_function(move |_, spec: Table| {
                    let name: String = spec.get("name").map_err(|_| {
                        LuaError::RuntimeError("tools.register needs a name".into())
                    })?;
                    if !valid_segment(&name) {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' tool name '{name}': use [a-z0-9_-]+, max 64 chars, no `__`"
                        )));
                    }
                    let execute: Function = spec.get("execute").map_err(|_| {
                        LuaError::RuntimeError(format!(
                            "extension '{ext_id}' tool '{name}' needs an execute function"
                        ))
                    })?;
                    let override_shadow: bool = spec.get("override").unwrap_or(false);
                    if override_shadow {
                        if !manifest.has_capability("tools.override") {
                            return Err(LuaError::RuntimeError(format!(
                                "extension '{ext_id}' shadows '{name}' without the tools.override capability"
                            )));
                        }
                        if name.contains("__") {
                            return Err(LuaError::RuntimeError(format!(
                                "extension '{ext_id}' cannot shadow '{name}': only plain built-in names shadow"
                            )));
                        }
                        if manifest.declares_tool(&name) {
                            return Err(LuaError::RuntimeError(format!(
                                "extension '{ext_id}' cannot shadow its own tool '{name}'"
                            )));
                        }
                        let mut regs = regs.borrow_mut();
                        if regs.tools.contains_key(&name) {
                            return Err(LuaError::RuntimeError(format!(
                                "extension '{ext_id}' shadows '{name}' twice"
                            )));
                        }
                        regs.tools.insert(name.clone(), execute);
                        regs.shadows.push(name);
                        return Ok(());
                    }
                    if !manifest.has_capability("tools") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' registers tools without the tools capability"
                        )));
                    }
                    if !manifest.declares_tool(&name) {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' registers undeclared tool '{name}': list it in manifest.yaml tools[]"
                        )));
                    }
                    let mut regs = regs.borrow_mut();
                    if regs.tools.contains_key(&name) {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' registers '{name}' twice"
                        )));
                    }
                    regs.tools.insert(name, execute);
                    Ok(())
                })
                .expect("register fn"),
            )
            .expect("register slot");
    }

    // dex.tools.call_original(ctx, args): only inside a shadow (P1).
    {
        let regs = Rc::clone(regs);
        let ext_id = ext_id.clone();
        tools
            .set(
                "call_original",
                lua.create_function(move |lua, (ctx, args): (Table, Table)| {
                    let _ = ctx;
                    let target = regs.borrow().current_shadow.clone().ok_or_else(|| {
                        LuaError::RuntimeError(format!(
                            "extension '{ext_id}' call_original outside a shadow has no original"
                        ))
                    })?;
                    let args_json =
                        lua_to_json(Value::Table(args)).map_err(LuaError::RuntimeError)?;
                    host_upcall(
                        lua,
                        &ext_id,
                        HostOp::CallOriginal {
                            target,
                            args: args_json,
                        },
                    )
                })
                .expect("call_original fn"),
            )
            .expect("call_original slot");
    }

    // dex.tools.call(name, args): host-mediated tool invocation (P2 rails).
    {
        let ext_id = ext_id.clone();
        tools
            .set(
                "call",
                lua.create_function(move |lua, (name, args): (String, Table)| {
                    let args_json =
                        lua_to_json(Value::Table(args)).map_err(LuaError::RuntimeError)?;
                    host_upcall(
                        lua,
                        &ext_id,
                        HostOp::ToolCall {
                            name,
                            args: args_json,
                        },
                    )
                })
                .expect("tools.call fn"),
            )
            .expect("tools.call slot");
    }

    // dex.tools.list(): the extension tool names (set_active's domain).
    {
        let ext_id = ext_id.clone();
        tools
            .set(
                "list",
                lua.create_function(move |lua, _: ()| host_upcall(lua, &ext_id, HostOp::ToolsList))
                    .expect("tools.list fn"),
            )
            .expect("tools.list slot");
    }

    // dex.tools.set_active(list): restrict the extension schema slice
    // (short own-tool names or full ext__ names — the host resolves);
    // requires the tools.override capability (plan §6.4).
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        tools
            .set(
                "set_active",
                lua.create_function(move |lua, list: Table| {
                    if !manifest.has_capability("tools.override") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' calls tools.set_active without the tools.override capability"
                        )));
                    }
                    let mut names = Vec::new();
                    for value in list.sequence_values::<Value>() {
                        let value = value.map_err(|e| LuaError::RuntimeError(e.to_string()))?;
                        match value {
                            Value::String(name) => names.push(
                                name.to_str()
                                    .map_err(|e| LuaError::RuntimeError(e.to_string()))?
                                    .to_string(),
                            ),
                            other => {
                                return Err(LuaError::RuntimeError(format!(
                                    "dex.tools.set_active: expected string names, got {other:?}"
                                )))
                            }
                        }
                    }
                    host_upcall(
                        lua,
                        &ext_id,
                        HostOp::SetActive {
                            ext: ext_id.clone(),
                            tools: names,
                        },
                    )
                })
                .expect("tools.set_active fn"),
            )
            .expect("tools.set_active slot");
    }

    tools
}

/// `dex.events.on(event, fn)` — subscribe to a known event.
pub(super) fn events_table(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Table {
    let ext_id = manifest.id.clone();
    let events = lua.create_table().expect("dex.events table");

    // dex.events.on(event, fn)
    {
        let regs = Rc::clone(regs);
        let ext_id = ext_id.clone();
        events
            .set(
                "on",
                lua.create_function(move |_, (event, handler): (String, Function)| {
                    if !KNOWN_EVENTS.contains(&event.as_str()) {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' subscribes to unknown event '{event}'"
                        )));
                    }
                    regs.borrow_mut()
                        .events
                        .entry(event)
                        .or_default()
                        .push(handler);
                    Ok(())
                })
                .expect("events.on fn"),
            )
            .expect("events.on slot");
    }

    events
}

/// `dex.wrap(slot, fn)` — middleware over an enveloped slot's handler chain
/// (spec §13). `fn(next, ctx)` receives the rest of the chain as `next(ev)`
/// and the event payload as `ctx`; its return becomes the envelope the host
/// reads. Registration order is composition order, outermost first.
/// Wrapping an unknown or non-wrappable slot fails legibly at load.
pub(super) fn wrap_slot(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Result<Function, LuaError> {
    let ext_id = manifest.id.clone();
    let regs = Rc::clone(regs);
    lua.create_function(move |_, (slot, mw): (String, Function)| {
        if !WRAPPABLE_SLOTS.contains(&slot.as_str()) {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' wraps '{slot}': not a middleware-wrappable slot (wrappable: {})",
                WRAPPABLE_SLOTS.join(", ")
            )));
        }
        regs.borrow_mut().wraps.entry(slot).or_default().push(mw);
        Ok(())
    })
}

/// `dex.activate_profile(name)` / `dex.profile()` — harness-profile
/// activation and inspection (spec §30). Activation is transactional at
/// resolution time: the name is stored now; when the next turn snapshot
/// resolves, an unknown name warns and applies nothing. `nil` deactivates.
/// Returns the previously active name.
pub(super) fn profile_api(lua: &Lua) -> Result<(Function, Function), LuaError> {
    let activate = lua.create_function(|_, name: Option<String>| {
        let previous = crate::agent::registry::active_profile();
        match name {
            Some(n) if !n.trim().is_empty() => {
                crate::agent::registry::set_active_profile(Some(n));
            }
            _ => crate::agent::registry::set_active_profile(None),
        }
        Ok(previous)
    })?;
    let current = lua.create_function(|_, ()| Ok(crate::agent::registry::active_profile()))?;
    Ok((activate, current))
}

/// `dex.fallback(slot, fn)` — backup opinion for an enveloped slot (spec
/// §34): consulted when the slot's whole chain (handlers + middleware)
/// produced no opinion, before the Rust default. Same slot family as
/// `dex.wrap`; unknown slots fail legibly at load.
pub(super) fn fallback_slot(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Result<Function, LuaError> {
    let ext_id = manifest.id.clone();
    let regs = Rc::clone(regs);
    lua.create_function(move |_, (slot, backup): (String, Function)| {
        if !WRAPPABLE_SLOTS.contains(&slot.as_str()) {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' sets a fallback for '{slot}': not a wrappable slot (wrappable: {})",
                WRAPPABLE_SLOTS.join(", ")
            )));
        }
        regs.borrow_mut().fallbacks.insert(slot, backup);
        Ok(())
    })
}

/// `dex.replace(slot, spec)` — component replacement (spec §10/§12). The
/// only replaceable slot is `agent_loop` (spec §9, the deliberately-last
/// milestone): `spec = { id, interface? = "agent_loop.v1", run }` where
/// `run(ctx)` owns the whole turn loop. Gated on the `agent_loop`
/// capability — replacing the orchestration is the widest surface an
/// extension can hold — and one loop per extension (re-running setup twice
/// is a bug, not a re-register).
pub(super) fn replace_slot(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Result<Function, LuaError> {
    let ext_id = manifest.id.clone();
    let manifest = manifest.clone();
    let regs = Rc::clone(regs);
    lua.create_function(move |_, (slot, spec): (String, Table)| {
        if slot != "agent_loop" {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' replaces '{slot}': not a replaceable slot (replaceable: agent_loop)"
            )));
        }
        if !manifest.has_capability("agent_loop") {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' replaces the agent loop without the agent_loop capability"
            )));
        }
        let id: String = spec
            .get("id")
            .map_err(|_| LuaError::RuntimeError("replace needs an id".into()))?;
        if !valid_segment(&id) {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' agent_loop id '{id}': use [a-z0-9_-]+, max 64 chars, no `__`"
            )));
        }
        let interface: Option<String> = spec.get("interface").unwrap_or(None);
        if interface.as_deref().is_some_and(|i| i != "agent_loop.v1") {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' agent_loop interface {interface:?}: want \"agent_loop.v1\""
            )));
        }
        let run: Function = spec.get("run").map_err(|_| {
            LuaError::RuntimeError(format!(
                "extension '{ext_id}' agent_loop '{id}' needs a run function"
            ))
        })?;
        let mut regs = regs.borrow_mut();
        if regs.agent_loop.is_some() {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' registers agent_loop twice"
            )));
        }
        regs.agent_loop = Some((id, run));
        Ok(())
    })
}

/// `dex.use(slot, impl)` — component selection with registration-time
/// validation (spec §10/§28): the slot must be a known component slot, and
/// `impl` is either the implementation function directly or a
/// `{ id?, interface?, run }` spec whose `interface` must name the slot's
/// current version (`SLOT_INTERFACES`) — a component written against a
/// different envelope fails at load instead of misreading payloads.
/// Registered handlers join the slot's chain exactly like
/// `dex.events.on(slot, …)` (fail-open contract unchanged, §33); the
/// validated extra surface is the point. Requires the `harness`
/// capability. `agent_loop` is not selectable here — `dex.replace` owns it.
pub(super) fn use_slot(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Result<Function, LuaError> {
    let ext_id = manifest.id.clone();
    let manifest = manifest.clone();
    let regs = Rc::clone(regs);
    lua.create_function(move |_, (slot, component): (String, Value)| {
        if !manifest.has_capability("harness") {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' uses slot '{slot}' without the harness capability"
            )));
        }
        if slot == "agent_loop" {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}': use dex.replace(\"agent_loop\", …) for the agent loop"
            )));
        }
        let Some((_, interface)) = SLOT_INTERFACES.iter().find(|(s, _)| *s == slot) else {
            return Err(LuaError::RuntimeError(format!(
                "extension '{ext_id}' uses unknown slot '{slot}' (usable: {})",
                SLOT_INTERFACES
                    .iter()
                    .map(|(s, _)| *s)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let run = match component {
            // Bare function: current interface implied.
            Value::Function(run) => run,
            Value::Table(spec) => {
                if let Some(id) = spec.get::<Option<String>>("id")? {
                    if !valid_segment(&id) {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' {slot} component id '{id}': use [a-z0-9_-]+, max 64 chars, no `__`"
                        )));
                    }
                }
                let declared: Option<String> = spec.get("interface")?;
                if let Some(declared) = declared {
                    if declared != *interface {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' {slot} interface {declared:?}: want {interface:?}"
                        )));
                    }
                }
                spec.get("run").map_err(|_| {
                    LuaError::RuntimeError(format!(
                        "extension '{ext_id}' {slot} component needs a run function"
                    ))
                })?
            }
            other => {
                return Err(LuaError::RuntimeError(format!(
                    "extension '{ext_id}' dex.use('{slot}', …): expected a function or spec table, got {}",
                    lua_type_name(&other)
                )))
            }
        };
        let mut regs = regs.borrow_mut();
        regs.events.entry(slot.clone()).or_default().push(run);
        if !regs.uses.contains(&slot) {
            regs.uses.push(slot);
        }
        Ok(())
    })
}

/// `dex.log.*` — daemon log + journal lines, prefixed `lua[<ext>]`.
pub(super) fn log_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let log = lua.create_table().expect("dex.log table");

    // dex.log.*: daemon log + journal, prefixed lua[<ext>].
    for level in ["debug", "info", "warn", "error"] {
        let ext_id = ext_id.clone();
        let level = level.to_string();
        log.set(
            level.clone(),
            lua.create_function(move |_, msg: MultiValue| {
                let mut parts = Vec::new();
                for value in msg {
                    parts.push(lua_value_to_string(&value));
                }
                eprintln!("lua[{ext_id}] {}: {}", level, parts.join("\t"));
                Ok(())
            })
            .expect("log fn"),
        )
        .expect("log slot");
    }

    log
}

/// `dex.workspace.read(path)` / `dex.workspace.exists(path)`: confined,
/// capability-gated reads (§5.2).
pub(super) fn workspace_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let workspace = lua.create_table().expect("dex.workspace table");

    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        workspace
            .set(
                "read",
                lua.create_function(move |_, path: String| {
                    if !manifest.has_capability("workspace.read") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' reads workspace without the workspace.read capability"
                        )));
                    }
                    let root = crate::tools::workspace_root().map_err(|e| {
                        LuaError::RuntimeError(format!("extension '{ext_id}': {e}"))
                    })?;
                    let full = resolve_workspace_path(&root, &path).map_err(|e| {
                        LuaError::RuntimeError(format!("extension '{ext_id}': {e}"))
                    })?;
                    std::fs::read_to_string(&full).map_err(|e| {
                        LuaError::RuntimeError(format!(
                            "extension '{ext_id}' read '{path}': {e}"
                        ))
                    })
                })
                .expect("workspace.read fn"),
            )
            .expect("workspace.read slot");
    }
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        workspace
            .set(
                "exists",
                lua.create_function(move |_, path: String| {
                    if !manifest.has_capability("workspace.read") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' stats workspace without the workspace.read capability"
                        )));
                    }
                    let Ok(root) = crate::tools::workspace_root() else {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}': no workspace"
                        )));
                    };
                    Ok(resolve_workspace_path(&root, &path).is_ok_and(|full| full.exists()))
                })
                .expect("workspace.exists fn"),
            )
            .expect("workspace.exists slot");
    }

    workspace
}

/// `dex.commands.register({ name, description, execute })`: a slash command.
/// The name must be a bare word (no spaces — it is the slash word).
pub(super) fn commands_table(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Table {
    let ext_id = manifest.id.clone();
    let commands = lua.create_table().expect("dex.commands table");
    {
        let regs = Rc::clone(regs);
        let ext_id = ext_id.clone();
        commands
            .set(
                "register",
                lua.create_function(move |_, spec: Table| {
                    let name: String = spec.get("name").map_err(|_| {
                        LuaError::RuntimeError("commands.register needs a name".into())
                    })?;
                    if name.is_empty()
                        || !name
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' registers invalid command name '{name}' (use [a-z0-9-])"
                        )));
                    }
                    let execute: Function = spec.get("execute").map_err(|_| {
                        LuaError::RuntimeError(format!(
                            "extension '{ext_id}' command '{name}' needs an execute function"
                        ))
                    })?;
                    let description: String = spec.get("description").unwrap_or_default();
                    regs.borrow_mut()
                        .commands
                        .insert(name, (description, execute));
                    Ok(())
                })
                .expect("commands.register fn"),
            )
            .expect("commands.register slot");
    }
    commands
}

/// `dex.state.get/set`: per-extension JSON key/value store (plan §7 P3;
/// minimal persistence: one JSON file per extension, write-through).
pub(super) fn state_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let state = lua.create_table().expect("dex.state table");
    {
        let ext_id = ext_id.clone();
        state
            .set(
                "get",
                lua.create_function(move |lua, key: String| {
                    match crate::extensions::state_get(&ext_id, &key) {
                        Some(json) => json_to_lua(lua, &json),
                        None => Ok(Value::Nil),
                    }
                })
                .expect("state.get fn"),
            )
            .expect("state.get slot");
    }
    {
        let ext_id = ext_id.clone();
        state
            .set(
                "set",
                lua.create_function(move |_lua, (key, value): (String, Value)| {
                    if value == Value::Nil {
                        return Err(LuaError::RuntimeError(
                            "dex.state.set: value cannot be nil (delete is not supported)"
                                .to_string(),
                        ));
                    }
                    let json = lua_to_json(value).map_err(LuaError::RuntimeError)?;
                    crate::extensions::state_set(&ext_id, key, json);
                    Ok(())
                })
                .expect("state.set fn"),
            )
            .expect("state.set slot");
    }
    state
}

/// `dex.prompt`: read-only system-prompt influence (plan §7). `append`
/// contributes load-time text to the base system prompt; `get` reads the
/// composed prompt (no skills — those are session-scoped).
pub(super) fn prompt_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let prompt = lua.create_table().expect("dex.prompt table");
    {
        let ext_id = ext_id.clone();
        prompt
            .set(
                "append",
                lua.create_function(move |_, text: String| {
                    crate::extensions::push_prompt_appendix(&ext_id, text);
                    Ok(())
                })
                .expect("prompt.append fn"),
            )
            .expect("prompt.append slot");
    }
    prompt
        .set(
            "get_system_prompt",
            lua.create_function(|_, _: ()| Ok(crate::llm::prompt::system_prompt(&[])))
                .expect("prompt.get fn"),
        )
        .expect("prompt.get slot");
    prompt
}

/// `dex.model.current()/auth([provider])/providers()`: the current model +
/// its credentials — or, with an explicit provider, that provider's
/// credentials — so a model-aware extension (provider-native search, …) can
/// reuse endpoints and keys instead of configuring its own, and enumerate
/// the configured fallback vocabulary. Reads file+env on the worker (sync,
/// no secrets cross into logs); the daemon records the served snapshot per
/// turn, which wins when set. Gated on the `model` capability like
/// `workspace.read` — and the explicit-provider form additionally needs
/// `net.providers` (a bare `model` extension may read only the current
/// model's key; every other provider's key stays out of its reach).
pub(super) fn model_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let model = lua.create_table().expect("dex.model table");
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        model
            .set(
                "current",
                lua.create_function(move |lua, _: ()| {
                    if !manifest.has_capability("model") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' reads the model without the model capability"
                        )));
                    }
                    // The drive's pinned model wins when this call carries one
                    // (this turn's model, even under concurrent turns); then
                    // the task-side fallback (a per-request override the file
                    // never sees); otherwise resolve from file+env.
                    let current = match worker_drive_model()
                        .and_then(|drive| drive.snapshot)
                        .or_else(crate::extensions::served_model_snapshot)
                    {
                        Some(served) => served,
                        None => crate::llm::config::extension_model_snapshot()
                            .map_err(LuaError::RuntimeError)?,
                    };
                    let id = current.id();
                    let table = lua.create_table()?;
                    table.set("provider", current.provider)?;
                    table.set("model", current.model)?;
                    table.set("id", id)?;
                    table.set("api", current.api)?;
                    table.set("base_url", current.base_url)?;
                    Ok(table)
                })
                .expect("model.current fn"),
            )
            .expect("model.current slot");
    }
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        model
            .set(
                "auth",
                lua.create_function(move |lua, provider: Option<String>| {
                    if !manifest.has_capability("model") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' reads model auth without the model capability"
                        )));
                    }
                    // No argument: the current model — the drive's pinned
                    // model wins when this call carries one (this turn's
                    // model, even under concurrent turns); then the task-side
                    // fallback (a per-request override the file never sees);
                    // the key still resolves from the configured deposits.
                      // With an explicit provider: that provider's configured
                      // deposits instead — the model-independent vocabulary a
                      // fallback search rides on. Gated on `net.providers`:
                      // cross-provider keys are as sensitive as the fetches
                      // they enable, so a bare `model` extension sees only
                      // the current model's key.
                      let (auth, api) =
                          match provider.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                              Some(name) => {
                                  if !manifest.has_capability("net.providers") {
                                      return Err(LuaError::RuntimeError(format!(
                                          "extension '{ext_id}' reads another provider's auth without the net.providers capability"
                                      )));
                                  }
                                  let resolved = crate::llm::config::extension_provider_auth(name)
                                      .map_err(LuaError::RuntimeError)?;
                                  (resolved.auth, resolved.api)
                              }
                            None => {
                                match worker_drive_model()
                                    .and_then(|drive| drive.snapshot)
                                    .or_else(crate::extensions::served_model_snapshot)
                                {
                                    Some(served) => (
                                        crate::llm::config::extension_model_auth_for(
                                            &served.provider,
                                            &served.base_url,
                                        )
                                        .map_err(LuaError::RuntimeError)?,
                                        served.api,
                                    ),
                                    None => (
                                        crate::llm::config::extension_model_auth()
                                            .map_err(LuaError::RuntimeError)?,
                                        crate::llm::config::extension_model_api()
                                            .map_err(LuaError::RuntimeError)?,
                                    ),
                                }
                            }
                        };
                    let table = lua.create_table()?;
                    table.set("api_key", auth.api_key)?;
                    table.set("base_url", auth.base_url)?;
                    table.set("api", api)?;
                    let headers = lua.create_table()?;
                    for (name, value) in &auth.headers {
                        headers.set(name.clone(), value.clone())?;
                    }
                    table.set("headers", headers)?;
                    Ok(table)
                })
                .expect("model.auth fn"),
            )
            .expect("model.auth slot");
    }
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        model
            .set(
                "providers",
                lua.create_function(move |lua, _: ()| {
                    if !manifest.has_capability("model") {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' lists providers without the model capability"
                        )));
                    }
                    // Configured providers with resolvable credentials — the
                    // fallback vocabulary. Names + endpoints only, never keys.
                    let list = lua.create_table()?;
                    for (index, entry) in crate::llm::config::extension_configured_providers()
                        .into_iter()
                        .enumerate()
                    {
                        let item = lua.create_table()?;
                        item.set("provider", entry.provider)?;
                        item.set("base_url", entry.base_url)?;
                        list.set(index + 1, item)?;
                    }
                    Ok(list)
                })
                .expect("model.providers fn"),
            )
            .expect("model.providers slot");
    }
    model
}

/// `dex.net.fetch(spec)`: one HTTP request confined to the model's own
/// endpoint (scheme+host+port must match `dex.model.auth().base_url`),
/// widened to the configured provider endpoints when the manifest declares
/// `net.providers`. Non-2xx is a value (`{status, headers, body}`), not a
/// Lua error. Gated on the `net` capability (which itself requires
/// `model`).
pub(super) fn net_table(lua: &Lua, manifest: &Manifest) -> Table {
    let ext_id = manifest.id.clone();
    let net = lua.create_table().expect("dex.net table");
    {
        let ext_id = ext_id.clone();
        let manifest = manifest.clone();
        net.set(
            "fetch",
            lua.create_function(move |lua, spec: Table| {
                if !manifest.has_capability("net") {
                    return Err(LuaError::RuntimeError(format!(
                        "extension '{ext_id}' fetches without the net capability"
                    )));
                }
                let url: String = spec.get("url").map_err(|_| {
                    LuaError::RuntimeError(format!(
                        "extension '{ext_id}' dex.net.fetch needs a url"
                    ))
                })?;
                let method = match spec.get::<Value>("method").unwrap_or(Value::Nil) {
                    Value::Nil => "GET".to_string(),
                    Value::String(s) => s
                        .to_str()
                        .map_err(|e| LuaError::RuntimeError(e.to_string()))?
                        .to_string(),
                    other => {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' dex.net.fetch method must be a string, got {}",
                            lua_type_name(&other)
                        )))
                    }
                };
                let timeout_ms = match spec.get::<Value>("timeout_ms").unwrap_or(Value::Nil) {
                    Value::Nil => 30_000,
                    Value::Integer(n) if n > 0 => n as u64,
                    Value::Number(n) if n > 0.0 => n as u64,
                    other => {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' dex.net.fetch timeout_ms must be a positive number, got {}",
                            lua_type_name(&other)
                        )))
                    }
                };
                let body: Option<String> = spec.get("body").map_err(|_| {
                    LuaError::RuntimeError(format!(
                        "extension '{ext_id}' dex.net.fetch body must be a string"
                    ))
                })?;
                let headers_value: Value = spec.get("headers").unwrap_or(Value::Nil);
                let mut headers = Vec::new();
                match headers_value {
                    Value::Nil => {}
                    Value::Table(heads) => {
                        for pair in heads.pairs::<String, String>() {
                            let (name, value) = pair.map_err(|e| {
                                LuaError::RuntimeError(format!(
                                    "extension '{ext_id}' dex.net.fetch headers must be string pairs: {e}"
                                ))
                            })?;
                            headers.push((name, value));
                        }
                    }
                    other => {
                        return Err(LuaError::RuntimeError(format!(
                            "extension '{ext_id}' dex.net.fetch headers must be a table, got {}",
                            lua_type_name(&other)
                        )))
                    }
                }
                let json = host_upcall(
                    lua,
                    &ext_id,
                    HostOp::NetFetch {
                        url,
                        method,
                        headers,
                        body,
                        timeout_ms,
                        allow_providers: manifest.has_capability("net.providers"),
                    },
                )?;
                let value: Json = serde_json::from_str(&json).map_err(|e| {
                    LuaError::RuntimeError(format!("extension '{ext_id}' bad fetch reply: {e}"))
                })?;
                json_to_lua(lua, &value)
            })
            .expect("net.fetch fn"),
        )
        .expect("net.fetch slot");
    }
    net
}

/// `dex.json.encode/decode`: table<->JSON string for request bodies and
/// response parsing. Pure data transform, no capability gate.
pub(super) fn json_table(lua: &Lua) -> Table {
    let json = lua.create_table().expect("dex.json table");
    json.set(
        "encode",
        lua.create_function(|_, value: Value| {
            lua_to_json(value)
                .map(|json| json.to_string())
                .map_err(LuaError::RuntimeError)
        })
        .expect("json.encode fn"),
    )
    .expect("json.encode slot");
    json.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let value: Json = serde_json::from_str(&text)
                .map_err(|e| LuaError::RuntimeError(format!("dex.json.decode: {e}")))?;
            json_to_lua(lua, &value)
        })
        .expect("json.decode fn"),
    )
    .expect("json.decode slot");
    json
}
