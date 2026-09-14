//! Per-extension Lua VM: sandbox construction, the `dex.*` host API, and the
//! worker thread that owns the `Lua` state.
//!
//! The worker thread never touches async code. Every host upcall
//! (`dex.tools.call`, `dex.tools.call_original`) crosses to the awaiting task
//! over a per-call channel, and the task answers before the worker finishes:
//! [`Request`] in, [`WorkerMsg`] out — a terminal [`WorkerMsg::Done`] or any
//! number of [`WorkerMsg::HostCall`]s the task answers first (plan §6.4, §7
//! `dex.tools.call`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use mlua::{Error as LuaError, Function, Lua, MultiValue, Table, Value, VmState};
use serde_json::{Map, Value as Json};
use tokio::sync::{mpsc, oneshot};

use super::Manifest;
use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::tools::{resolve_workspace_path, Policy, ShellEvidence, ToolFilter};

/// Default per-call Lua budget (manifest `timeout_secs`, clamped to
/// [`MAX_TOOL_TIMEOUT_SECS`]).
/// Hard ceiling for a manifest `timeout_secs`.
pub(crate) const MAX_TOOL_TIMEOUT_SECS: u64 = 120;
/// Budget for one extension's handlers of a single hook event: hooks are an
/// observing layer, so a wedged hook stalls dispatch at most this long
/// before the fail-open default (§8) skips it.
pub(crate) const HOOK_TIMEOUT_SECS: u64 = 10;
/// Grace for an aborted worker to unwind to `Done` after its in-flight host
/// call is dropped: the worker may be stuck in a `pcall` loop swallowing the
/// abort error, so the task never waits past this.
const ABORT_GRACE: Duration = Duration::from_secs(5);
/// Maximum nesting of host-mediated calls (`dex.tools.call` inside a tool
/// inside a `dex.tools.call` …): unbounded recursion would let two
/// cooperating extensions ping-pong until the deadline.
pub(crate) const MAX_HOSTCALL_DEPTH: u32 = 8;

/// Event names `dex.events.on` accepts. Anything else fails legibly at load
/// rather than silently never firing — including the reserved/omitted Pi
/// surface (`context`, `agent.spawn`, …: plan §7), so Pi-ported extensions
/// learn immediately what has no seam.
pub(crate) const KNOWN_EVENTS: &[&str] = &[
    "tool.before",
    "tool.after",
    "turn.start",
    "turn.end",
    "agent.start",
    "agent.end",
    "session.before_compact",
    "before_agent_start",
];

/// Host context a Lua call runs under: what cancellation, gates, and
/// allowlists a nested `dex.tools.call` / `dex.tools.call_original` inherits
/// from the outer call. Borrowed from the awaiting task — nothing here
/// crosses threads, only the JSON payloads do.
#[derive(Clone, Copy)]
pub(crate) struct HostCtx<'a> {
    pub(crate) cancel: &'a (dyn CancellationSource + Send + Sync),
    pub(crate) policy: &'a Policy,
    pub(crate) filter: Option<&'a ToolFilter>,
}

/// Live routing for `dex.tools.call_original`: only a shadow drive carries
/// one, which is what makes `call_original` outside a shadow impossible by
/// construction (the worker also rejects it — defense in depth).
pub(crate) struct ShadowCtx<'a> {
    pub(crate) shell_out: &'a mut Option<ShellEvidence>,
}

tokio::task_local! {
    static HOSTCALL_DEPTH: Cell<u32>;
}

fn hostcall_depth() -> u32 {
    HOSTCALL_DEPTH.try_with(|d| d.get()).unwrap_or(0)
}

fn set_hostcall_depth(depth: u32) {
    let _ = HOSTCALL_DEPTH.try_with(|d| d.set(depth));
}

/// What the worker is asked to run: a registered tool, one extension's
/// handlers for a hook event, or a shadowed built-in (which additionally arms
/// `dex.tools.call_original` for the duration of the call).
pub(crate) enum CallKind {
    Tool {
        tool: String,
    },
    Event {
        event: String,
    },
    Shadow {
        target: String,
    },
    /// `dex.commands.register` dispatch: run the named command handler.
    Command {
        name: String,
    },
}

/// Worker → task messages on the per-call channel. Exactly one `Done`
/// terminates the stream; any number of `HostCall`s may precede it.
pub(crate) enum WorkerMsg {
    Done(Result<String, String>),
    HostCall {
        op: HostOp,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// A host upcall from Lua, answered by the awaiting task.
pub(crate) enum HostOp {
    /// `dex.tools.call_original(ctx, args)` inside a shadow: re-dispatch the
    /// shadowed built-in with the caller's gates (no shadow re-entry).
    CallOriginal { target: String, args: Json },
    /// `dex.tools.call(name, args)`: a full host-mediated tool invocation —
    /// H1 hooks, gates, dispatch — like any model-issued call.
    ToolCall { name: String, args: Json },
    /// `dex.tools.list()`: the extension tool names (the slice set_active
    /// controls).
    ToolsList,
    /// `dex.tools.set_active(list)`: restrict the extension schema slice.
    SetActive { tools: Vec<String> },
}

/// The chunk's exports: registered tool names, subscribed event names, and
/// shadowed built-in names. Returned by [`ExtensionEngine::load`].
pub(crate) struct ChunkExports {
    pub(crate) tools: Vec<String>,
    pub(crate) events: Vec<String>,
    pub(crate) shadows: Vec<String>,
    /// (name, description) pairs, sorted by name.
    pub(crate) commands: Vec<(String, String)>,
}

enum Request {
    Load {
        source: String,
        reply: oneshot::Sender<Result<ChunkExports, String>>,
    },
    Call {
        kind: CallKind,
        args: Json,
        timeout: Duration,
        call_id: String,
        tx: mpsc::UnboundedSender<WorkerMsg>,
    },
}

/// Mutable registration state shared by the `dex.*` closures. `Rc<RefCell>`
/// (never crossing threads — the worker owns it) because mlua closures are
/// `Fn`, not `FnMut`.
#[derive(Default)]
struct WorkerRegistrations {
    tools: HashMap<String, Function>,
    events: HashMap<String, Vec<Function>>,
    /// `dex.commands.register({ name, description, execute })` handlers.
    commands: HashMap<String, (String, Function)>,
    shadows: Vec<String>,
    /// Shadow target armed for the running call, if any: the only context in
    /// which `dex.tools.call_original` is legal.
    current_shadow: Option<String>,
}

/// One extension's VM. Cloneable handle; the worker thread owns the state.
#[derive(Clone)]
pub(crate) struct ExtensionEngine {
    tx: mpsc::Sender<Request>,
    manifest: Manifest,
}

impl ExtensionEngine {
    pub(crate) fn new(manifest: Manifest, dir: PathBuf) -> std::io::Result<Self> {
        let id = manifest.id.clone();
        let (tx, rx) = mpsc::channel::<Request>(16);
        let worker_manifest = manifest.clone();
        let worker_dir = dir;
        std::thread::Builder::new()
            .name(format!("dex-ext-{}", manifest.id))
            .spawn(move || worker_loop(worker_manifest, worker_dir, rx))
            .map(|_| Self { tx, manifest })
            .map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    format!("extension '{id}' worker spawn failed: {e}"),
                )
            })
    }

    fn stopped(&self) -> String {
        format!(
            "extension '{}' worker stopped",
            self.manifest.id_for_error()
        )
    }

    /// Per-tool-call Lua budget from the manifest, clamped.
    pub(crate) fn tool_timeout(&self, tool: &str) -> Duration {
        Duration::from_secs(
            self.manifest
                .timeout_secs(tool)
                .clamp(1, MAX_TOOL_TIMEOUT_SECS),
        )
    }

    /// Run the extension chunk (`return function(dex) ... end`) and collect
    /// its exports. Registration mistakes fail the whole extension here, so
    /// nothing half-registered ever reaches the schema.
    pub(crate) async fn load(&self, source: String) -> Result<ChunkExports, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Request::Load { source, reply })
            .await
            .map_err(|_| self.stopped())?;

        rx.await.map_err(|_| self.stopped())?
    }

    /// Run a registered tool, shadow, or event handler to completion,
    /// answering host upcalls from the task side. `ToolCall` re-enters the
    /// full pipeline under `host`; `CallOriginal` re-dispatches the shadowed
    /// built-in with the caller's gates — legal only when `shadow` carries
    /// the live shell-evidence slot. The returned string is the tool result
    /// (or the directive envelope JSON, for events).
    pub(crate) async fn drive(
        &self,
        kind: CallKind,
        args: Json,
        timeout: Duration,
        cancel: &(dyn CancellationSource + Send + Sync),
        host: HostCtx<'_>,
        mut shadow: Option<ShadowCtx<'_>>,
    ) -> Result<String, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMsg>();
        let call_id = uuid::Uuid::new_v4().to_string();
        self.tx
            .send(Request::Call {
                kind,
                args,
                timeout,
                call_id,
                tx,
            })
            .await
            .map_err(|_| self.stopped())?;
        let deadline = tokio::time::Instant::now() + timeout;
        // The worker aborts Lua between instructions once the deadline
        // passes, but a worker blocked on a host-call reply executes no
        // instructions — the task-side deadline below is the backstop, and
        // the abort reply (or dropping its sender) is what unblocks it.
        // Set when the task abandons the worker (timeout/cancel): the next
        // `Done` is the worker's unwind noise, and this error is the answer.
        let mut abandoning: Option<String> = None;
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    let Some(msg) = msg else {
                        return Err(self.stopped());
                    };
                    match msg {
                        WorkerMsg::Done(result) => {
                            if let Some(error) = abandoning {
                                return Err(error);
                            }
                            return result;
                        }
                        WorkerMsg::HostCall { op, reply } => {
                            if abandoning.is_some() {
                                continue;
                            }
                            if hostcall_depth() >= MAX_HOSTCALL_DEPTH {
                                let _ = reply.send(Err(format!(
                                    "dex.tools.call depth exceeded ({MAX_HOSTCALL_DEPTH}): refusing nested call"
                                )));
                                continue;
                            }
                            let depth = hostcall_depth();
                            set_hostcall_depth(depth + 1);
                            let outcome = tokio::select! {
                                result = answer_hostcall(op, &host, &mut shadow) => result,
                                _ = tokio::time::sleep_until(deadline) => {
                                    Err(format!("extension '{}' call timed out", self.manifest.id_for_error()))
                                }
                                _ = wait_cancelled(cancel) => {
                                    Err(format!("extension '{}' call cancelled", self.manifest.id_for_error()))
                                }
                            };
                            set_hostcall_depth(depth);
                            match outcome {
                                Err(error) => {
                                    // The nested call died: abort the worker
                                    // the same way a deadline does, and report
                                    // the nested error rather than unwind noise.
                                    let _ = reply.send(Err(error.clone()));
                                    abandoning = Some(error);
                                    abort_wait(&mut rx).await;
                                    return Err(abandoning
                                        .take()
                                        .unwrap_or_else(|| self.stopped()));
                                }
                                ok => {
                                    let _ = reply.send(ok);
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    abort_wait(&mut rx).await;
                    return Err(format!(
                        "extension '{}' call timed out",
                        self.manifest.id_for_error()
                    ));
                }
                _ = wait_cancelled(cancel) => {
                    abort_wait(&mut rx).await;
                    return Err(format!(
                        "extension '{}' call cancelled",
                        self.manifest.id_for_error()
                    ));
                }
            }
        }
    }
}

/// Answer one host upcall from Lua. `ToolCall` is a full host-mediated
/// invocation — H1 hooks, allowlist, gates, dispatch — exactly like a
/// model-issued call. Boxed: this is the cycle's cut point
/// (`execute_with_shell` → dispatch → `call_global` → `drive` → here →
/// `execute` → `execute_with_shell`), and unboxed it would recurse in type.
async fn answer_hostcall(
    op: HostOp,
    host: &HostCtx<'_>,
    shadow: &mut Option<ShadowCtx<'_>>,
) -> Result<String, String> {
    match op {
        HostOp::ToolCall { name, args } => {
            let Some(args) = args.as_object() else {
                return Err(format!("dex.tools.call '{name}': args must be an object"));
            };
            Box::pin(crate::tools::execute(
                &name,
                args,
                host.cancel,
                host.policy,
                host.filter,
            ))
            .await
            .map_err(|e| e.to_string())
        }
        HostOp::ToolsList => {
            Ok(serde_json::to_string(&crate::extensions::tools_list())
                .map_err(|e| e.to_string())?)
        }
        HostOp::SetActive { tools } => {
            crate::extensions::set_active_global(tools).await;
            Ok("null".to_string())
        }
        HostOp::CallOriginal { target, args } => {
            let Some(slot) = shadow.as_mut() else {
                return Err("call_original outside a shadow has no original".to_string());
            };
            let Some(args) = args.as_object() else {
                return Err("call_original args must be an object".to_string());
            };
            // Same boxing: dispatch re-enters the pipeline above.
            Box::pin(crate::tools::dispatch_original(
                &target,
                args,
                host.cancel,
                host.policy,
                host.filter,
                slot.shell_out,
            ))
            .await
            .map_err(|e| e.to_string())
        }
    }
}

/// After an abort, drain until `Done` (the worker's unwind) or the grace
/// period, whichever comes first. The caller reports its own error
/// regardless — including when a `pcall` loop swallows the abort and `Done`
/// never arrives.
async fn abort_wait(rx: &mut mpsc::UnboundedReceiver<WorkerMsg>) {
    let _ = tokio::time::timeout(ABORT_GRACE, async {
        while let Some(msg) = rx.recv().await {
            if matches!(msg, WorkerMsg::Done(_)) {
                break;
            }
        }
    })
    .await;
}

fn worker_loop(manifest: Manifest, dir: PathBuf, mut rx: mpsc::Receiver<Request>) {
    let lua = Lua::new();

    strip_sandbox(&lua);
    let regs = Rc::new(RefCell::new(WorkerRegistrations::default()));
    let dex = build_dex_table(&lua, &manifest, &regs);
    // The worker exits when the last engine handle drops: `run()` returning
    // `None` is shutdown, never an error surface.

    while let Some(req) = rx.blocking_recv() {
        match req {
            Request::Load { source, reply } => {
                let _ = reply.send(run_load(&lua, &dex, &manifest, &regs, &source));
            }
            Request::Call {
                kind,
                args,
                timeout,
                call_id,
                tx,
            } => {
                run_call(
                    &lua, &manifest, &regs, &dir, kind, args, timeout, &call_id, &tx,
                );
            }
        }
    }
}

/// Strip the process-touching surface before the chunk runs: `os`, `io`,
/// `package`/`require`, the file loaders, the chunk compiler (`load` would
/// let the chunk compile code outside host control), and `debug`
/// (introspection into host closures). `collectgarbage` is harmless and
/// stays. The chunk is loaded by the host via `Lua::load`, never by an
/// extension-chosen path.
fn strip_sandbox(lua: &Lua) {
    let globals = lua.globals();
    for name in [
        "os",
        "io",
        "package",
        "require",
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "debug",
    ] {
        let _ = globals.set(name, Value::Nil);
    }
}

fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn run_load(
    lua: &Lua,
    dex: &Table,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    source: &str,
) -> Result<ChunkExports, String> {
    // The chunk must evaluate to the setup function; calling it runs
    // registration (its own return is ignored — exports come from `regs`).

    let chunk: Function = lua
        .load(source)
        .set_name(format!("extension '{}'", manifest.id_for_error()))
        .eval()
        .map_err(|e| {
            format!(
                "extension '{}' must return function(dex): {e}",
                manifest.id_for_error()
            )
        })?;

    let _: MultiValue = chunk
        .call(dex.clone())
        .map_err(|e| format!("extension '{}' setup failed: {e}", manifest.id_for_error()))?;

    let regs = regs.borrow();

    let mut commands: Vec<(String, String)> = regs
        .commands
        .iter()
        .map(|(name, (description, _))| (name.clone(), description.clone()))
        .collect();
    commands.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ChunkExports {
        tools: regs.tools.keys().cloned().collect(),
        events: regs.events.keys().cloned().collect(),
        shadows: regs.shadows.clone(),
        commands,
    })
}

/// Build the `dex.*` table once per worker. Closures capture the worker-side
/// registration state plus (for upcalls) nothing task-specific: host routing
/// happens on the task side of the channel.
fn build_dex_table(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Table {
    let ext_id = manifest.id.clone();
    let dex = lua.create_table().expect("dex table");
    let tools = lua.create_table().expect("dex.tools table");
    let events = lua.create_table().expect("dex.events table");
    let log = lua.create_table().expect("dex.log table");
    let workspace = lua.create_table().expect("dex.workspace table");

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
                            "extension '{ext_id}' tool name '{name}': use [a-z0-9_-]+, max 64 chars"
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

    // dex.tools.set_active(list): restrict the extension schema slice;
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
                    host_upcall(lua, &ext_id, HostOp::SetActive { tools: names })
                })
                .expect("tools.set_active fn"),
            )
            .expect("tools.set_active slot");
    }

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

    // dex.workspace.read(path) / dex.workspace.exists(path): confined,
    // capability-gated reads (§5.2).
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

    // dex.commands.register({ name, description, execute }): a slash
    // command. Name must be a bare word (no spaces — it is the slash word).
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
    dex.set("commands", commands).expect("dex.commands");

    // dex.state.get/set: per-extension JSON key/value store (plan §7 P3;
    // minimal persistence: one JSON file per extension, write-through).
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
    dex.set("state", state).expect("dex.state");

    // dex.prompt: read-only system-prompt influence (plan §7). `append`
    // contributes load-time text to the base system prompt; `get` reads the
    // composed prompt (no skills — those are session-scoped).
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
    dex.set("prompt", prompt).expect("dex.prompt");
    dex.set("tools", tools).expect("dex.tools");
    dex.set("events", events).expect("dex.events");
    dex.set("log", log).expect("dex.log");
    dex.set("workspace", workspace).expect("dex.workspace");
    dex
}

/// Send a host upcall to the awaiting task and block the worker for the
/// answer. The task always drains the channel until `Done`, so `send` only
/// fails after a task-side abandon — which already reports its own error.
fn host_upcall(lua: &Lua, ext_id: &str, op: HostOp) -> Result<String, LuaError> {
    let tx: mpsc::UnboundedSender<WorkerMsg> = lua
        .app_data_ref::<mpsc::UnboundedSender<WorkerMsg>>()
        .ok_or_else(|| {
            LuaError::RuntimeError(format!("extension '{ext_id}' host call outside a call"))
        })?
        .clone();
    let (reply, rx) = oneshot::channel();
    tx.send(WorkerMsg::HostCall { op, reply })
        .map_err(|_| LuaError::RuntimeError(format!("extension '{ext_id}' host unreachable")))?;
    rx.blocking_recv()
        .map_err(|_| LuaError::RuntimeError(format!("extension '{ext_id}' host call abandoned")))?
        .map_err(LuaError::RuntimeError)
}

/// Run one call on the worker: arm the deadline hook, dispatch by kind, and
/// always terminate with exactly one `Done`.
#[allow(clippy::too_many_arguments)]
fn run_call(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
    dir: &Path,
    kind: CallKind,
    args: Json,
    timeout: Duration,
    call_id: &str,
    tx: &mpsc::UnboundedSender<WorkerMsg>,
) {
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

fn lua_type_name(value: &Value) -> &'static str {
    match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "boolean",
        Value::LightUserData(_) => "userdata",
        Value::Integer(_) => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Table(_) => "table",
        Value::Function(_) => "function",
        Value::Thread(_) => "thread",
        Value::UserData(_) => "userdata",
        Value::Error(_) => "error",
        Value::Other(_) => "other",
    }
}

fn stringify_json(value: &Value) -> Result<Json, String> {
    match value {
        Value::String(s) => Ok(Json::String(
            s.to_str().map_err(|e| e.to_string())?.to_string(),
        )),
        Value::Integer(i) => Ok(Json::Number((*i).into())),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(Json::Number)
            .ok_or_else(|| "non-finite number".to_string()),
        Value::Boolean(b) => Ok(Json::Bool(*b)),
        Value::Nil => Ok(Json::Null),
        Value::Table(_) => lua_to_json(value.clone()),
        other => Err(format!("cannot pass {} to host", lua_type_name(other))),
    }
}

fn lua_value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s
            .to_str()
            .map(|b| b.to_string())
            .unwrap_or_else(|_| "<invalid utf-8>".to_string()),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_string(),
        Value::Table(_) => match lua_to_json(value.clone()) {
            Ok(json) => json.to_string(),
            Err(_) => "<unserializable>".to_string(),
        },
        other => format!("<{}>", lua_type_name(other)),
    }
}

/// Tool results are display strings: scalars stringify, tables serialize as
/// JSON, anything else (functions, threads) is a registration-time-shaped
/// error at call time.
fn stringify_tool_result(returned: MultiValue, ev_id: &str, tool: &str) -> Result<String, String> {
    let first = returned.into_iter().next().unwrap_or(Value::Nil);
    match first {
        Value::String(s) => s.to_str().map(|b| b.to_string()).map_err(|e| e.to_string()),
        Value::Integer(i) => Ok(i.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Boolean(b) => Ok(b.to_string()),
        Value::Nil => Ok(String::new()),
        Value::Table(_) => serde_json::to_string(
            &lua_to_json(first).map_err(|e| format!("extension '{ev_id}' tool '{tool}': {e}"))?,
        )
        .map_err(|e| e.to_string()),
        other => Err(format!(
            "extension '{ev_id}' tool '{tool}' returned {}",
            lua_type_name(&other)
        )),
    }
}

/// Host JSON → Lua: objects to string-keyed tables, arrays to 1-based
/// tables. Non-string object keys stringify (JSON round-trips are
/// string-keyed at the top; nested numbers become `"1"`-style keys).
fn json_to_lua(lua: &Lua, value: &Json) -> Result<Value, LuaError> {
    match value {
        Json::Null => Ok(Value::Nil),
        Json::Bool(b) => Ok(Value::Boolean(*b)),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Ok(Value::Nil)
            }
        }
        Json::String(s) => Ok(Value::String(lua.create_string(s)?)),
        Json::Array(items) => {
            let table = lua.create_table()?;
            for (index, item) in items.iter().enumerate() {
                table.set(index + 1, json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(table))
        }
        Json::Object(map) => {
            let table = lua.create_table()?;
            for (key, item) in map {
                table.set(key.clone(), json_to_lua(lua, item)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

/// Lua → host JSON. Tables are arrays iff non-empty with exactly the integer
/// keys `1..=n`; empty tables become `{}` (args-shaped; a nested empty array
/// degrades — hooks mutating exotic shapes re-check the host side).
/// Functions, threads, and userdata cannot cross and are an error, never a
/// silent drop.
fn lua_to_json(value: Value) -> Result<Json, String> {
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(b) => Ok(Json::Bool(b)),
        Value::Integer(i) => Ok(Json::Number(i.into())),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(Json::Number)
            .ok_or_else(|| "non-finite number cannot cross to host".to_string()),
        Value::String(s) => Ok(Json::String(
            s.to_str().map_err(|e| e.to_string())?.to_string(),
        )),
        Value::Table(t) => {
            let mut pairs: Vec<(Value, Value)> = t
                .pairs()
                .collect::<Result<_, _>>()
                .map_err(|e| e.to_string())?;
            if pairs.is_empty() {
                return Ok(Json::Object(Map::new()));
            }
            pairs.sort_by(|a, b| {
                let ai = a.0.as_integer();
                let bi = b.0.as_integer();
                ai.cmp(&bi)
            });
            let is_array = pairs
                .iter()
                .enumerate()
                .all(|(index, (key, _))| key.as_integer() == Some(index as i64 + 1));
            if is_array {
                let mut items = Vec::with_capacity(pairs.len());
                for (_, item) in pairs {
                    items.push(lua_to_json(item)?);
                }
                Ok(Json::Array(items))
            } else {
                let mut map = Map::with_capacity(pairs.len());
                for (key, item) in pairs {
                    let key = match key {
                        Value::String(s) => s.to_str().map_err(|e| e.to_string())?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        Value::Boolean(b) => b.to_string(),
                        other => {
                            return Err(format!(
                                "cannot use {} as an object key",
                                lua_type_name(&other)
                            ));
                        }
                    };
                    map.insert(key, lua_to_json(item)?);
                }
                Ok(Json::Object(map))
            }
        }
        other => Err(format!("cannot pass {} to host", lua_type_name(&other))),
    }
}
