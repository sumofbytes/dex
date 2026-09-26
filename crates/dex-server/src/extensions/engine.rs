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
use std::path::PathBuf;
use std::rc::Rc;

use std::time::{Duration, Instant};

use mlua::{Error as LuaError, Function, Lua, MultiValue, Table, Value, VmState};
use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot};

use super::Manifest;
use super::MAX_TOOL_TIMEOUT_SECS;
use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::tools::{Policy, ToolFilter};

/// Budget for one extension's handlers of a single hook event: hooks are an
/// observing layer, so a wedged hook stalls dispatch at most this long
/// before the fail-open default (§8) skips it.
pub const HOOK_TIMEOUT_SECS: u64 = 10;
/// Budget for the load-time setup run (chunk + registration): a looping
/// setup function must not hang the refresh — and one-shot startup blocks
/// on it (`main.rs`), so an unbounded load would hang the whole process.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(HOOK_TIMEOUT_SECS);
/// Slow-hook warning threshold: a hook call slower than this logs a warning
/// (perf doc §9). Hooks run inline on the dispatch path, so anything near
/// the 10s hook timeout is per-turn latency; 500ms flags the offender early.
pub const SLOW_HOOK_WARN: Duration = Duration::from_millis(500);
/// Per-VM memory ceiling: without it `string.rep`/table growth OOM-aborts
/// the whole daemon instead of failing one extension call.
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Grace for an aborted worker to unwind to `Done` after its in-flight host
/// call is dropped: the worker may be stuck in a `pcall` loop swallowing the
/// abort error, so the task never waits past this.
const ABORT_GRACE: Duration = Duration::from_secs(5);
/// Maximum nesting of host-mediated calls (`dex.tools.call` inside a tool
/// inside a `dex.tools.call` …): unbounded recursion would let two
/// cooperating extensions ping-pong until the deadline.
pub const MAX_HOSTCALL_DEPTH: u32 = 8;

/// Event names `dex.events.on` accepts. Anything else fails legibly at load
/// rather than silently never firing — including the reserved/omitted Pi
/// surface (`context`, `agent.spawn`, …: plan §7), so Pi-ported extensions
/// learn immediately what has no seam.
pub const KNOWN_EVENTS: &[&str] = &[
    "tool.before",
    "tool.after",
    "turn.start",
    "turn.end",
    "agent.start",
    "agent.end",
    "session.before_compact",
    "before_agent_start",
    "model_select",
    "harness.overflow",
    "harness.conflict",
];

/// Host context a Lua call runs under: what cancellation, gates, and
/// allowlists a nested `dex.tools.call` / `dex.tools.call_original` inherits
/// from the outer call. Borrowed from the awaiting task — nothing here
/// crosses threads, only the JSON payloads do.
#[derive(Clone, Copy)]
pub struct HostCtx<'a> {
    pub cancel: &'a (dyn CancellationSource + Send + Sync),
    pub policy: &'a Policy,
    pub filter: Option<&'a ToolFilter>,
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
pub enum CallKind {
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
pub enum WorkerMsg {
    Done(Result<String, String>),
    HostCall {
        op: HostOp,
        reply: oneshot::Sender<Result<String, String>>,
    },
}

/// A host upcall from Lua, answered by the awaiting task.
pub enum HostOp {
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
    /// Carries the caller id so short (own-extension) names resolve to full
    /// `ext__<ext>__<tool>` names host-side — extension code never spells the
    /// prefix.
    SetActive { ext: String, tools: Vec<String> },
    /// `dex.net.fetch(spec)`: one HTTP request confined to the current
    /// model's own endpoint (the task side checks the origin) — plus the
    /// configured provider endpoints when the manifest declares
    /// `net.providers`. The worker never touches the network; the awaiting
    /// task performs the request.
    NetFetch {
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
        timeout_ms: u64,
        allow_providers: bool,
    },
}

/// The chunk's exports: registered tool names, subscribed event names, and
/// shadowed built-in names. Returned by [`ExtensionEngine::load`].
pub struct ChunkExports {
    pub tools: Vec<String>,
    pub events: Vec<String>,
    pub shadows: Vec<String>,
    /// (name, description) pairs, sorted by name.
    pub commands: Vec<(String, String)>,
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
        /// This turn's drive context (snapshot + routing headers), read from
        /// the task-local at `drive()` time. The worker pins it for the
        /// drive's duration so `dex.model.*` on the worker thread serves this
        /// call's model even when another turn records a newer one mid-call.
        model: Option<super::DriveModel>,
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
pub struct ExtensionEngine {
    tx: mpsc::Sender<Request>,
    manifest: Manifest,
}

impl ExtensionEngine {
    pub fn new(manifest: Manifest, dir: PathBuf) -> std::io::Result<Self> {
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
    pub fn tool_timeout(&self, tool: &str) -> Duration {
        Duration::from_secs(
            self.manifest
                .timeout_secs(tool)
                .clamp(1, MAX_TOOL_TIMEOUT_SECS),
        )
    }

    /// Run the extension chunk (`return function(dex) ... end`) and collect
    /// its exports. Registration mistakes fail the whole extension here, so
    /// nothing half-registered ever reaches the schema.
    pub async fn load(&self, source: String) -> Result<ChunkExports, String> {
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
    /// built-in with the caller's gates. The returned string is the tool result
    /// (or the directive envelope JSON, for events).
    pub async fn drive(
        &self,
        kind: CallKind,
        args: Json,
        timeout: Duration,
        cancel: &(dyn CancellationSource + Send + Sync),
        host: HostCtx<'_>,
    ) -> Result<String, String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerMsg>();
        let call_id = uuid::Uuid::new_v4().to_string();
        // Snapshot the turn's drive context now: by the time the worker runs
        // this call, a concurrent turn may have recorded a newer snapshot
        // into the process-wide fallback.
        let drive_model = {
            let model = crate::extensions::current_drive_model();
            if model.snapshot.is_some() {
                Some(model)
            } else {
                None
            }
        };
        self.tx
            .send(Request::Call {
                kind,
                args,
                timeout,
                call_id,
                model: drive_model,
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
                                result = answer_hostcall(op, &host) => result,
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
/// (`execute` → dispatch → `call_global` → `drive` → here →
/// `execute`), and unboxed it would recurse in type.
async fn answer_hostcall(op: HostOp, host: &HostCtx<'_>) -> Result<String, String> {
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
        HostOp::SetActive { ext, tools } => {
            crate::extensions::set_active_global(&ext, tools).await;
            Ok("null".to_string())
        }
        HostOp::NetFetch {
            url,
            method,
            headers,
            body,
            timeout_ms,
            allow_providers,
        } => {
            crate::extensions::net_fetch(url, method, headers, body, timeout_ms, allow_providers)
                .await
        }
        HostOp::CallOriginal { target, args } => {
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

    // Cap the VM's memory: an allocation past the ceiling raises a Lua
    // memory error (caught like any runtime error) instead of OOM-killing
    // the whole process.
    lua.set_memory_limit(LUA_MEMORY_LIMIT)
        .expect("memory limit");
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
                model,
                tx,
            } => {
                run_call(
                    &lua, &manifest, &regs, &dir, kind, args, timeout, &call_id, model, &tx,
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

pub(super) fn valid_segment(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains("__")
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
    // The load-time budget: same instruction-count hook as `run_call`, so a
    // looping setup function fails the extension instead of hanging the
    // refresh (and one-shot startup, which blocks on it).
    let started = Instant::now();
    lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            if started.elapsed() > LOAD_TIMEOUT {
                return Err(LuaError::RuntimeError("extension load timed out".into()));
            }
            Ok(VmState::Continue)
        },
    )
    .expect("hook arm");
    let result = run_load_inner(lua, dex, manifest, regs, source);
    lua.remove_hook();
    result
}

fn run_load_inner(
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

/// Build the `dex.*` table once per worker: one sub-table per namespace, each
/// assembled by its own builder so no single function registers the whole API.
/// Closures capture the worker-side registration state plus (for upcalls)
/// nothing task-specific: host routing happens on the task side of the channel.
fn build_dex_table(
    lua: &Lua,
    manifest: &Manifest,
    regs: &Rc<RefCell<WorkerRegistrations>>,
) -> Table {
    let dex = lua.create_table().expect("dex table");
    dex.set("tools", host_api::tools_table(lua, manifest, regs))
        .expect("dex.tools");
    dex.set("events", host_api::events_table(lua, manifest, regs))
        .expect("dex.events");
    dex.set("log", host_api::log_table(lua, manifest))
        .expect("dex.log");
    dex.set("workspace", host_api::workspace_table(lua, manifest))
        .expect("dex.workspace");
    dex.set("commands", host_api::commands_table(lua, manifest, regs))
        .expect("dex.commands");
    dex.set("state", host_api::state_table(lua, manifest))
        .expect("dex.state");
    dex.set("prompt", host_api::prompt_table(lua, manifest))
        .expect("dex.prompt");
    dex.set("model", host_api::model_table(lua, manifest))
        .expect("dex.model");
    dex.set("net", host_api::net_table(lua, manifest))
        .expect("dex.net");
    dex.set("json", host_api::json_table(lua))
        .expect("dex.json");
    dex
}

mod call;
mod host_api;
mod lua_json;

/// Send a host upcall to the awaiting task and block the worker for the
/// answer. The task always drains the channel until `Done`, so `send` only
/// fails after a task-side abandon — which already reports its own error.
pub(super) fn host_upcall(lua: &Lua, ext_id: &str, op: HostOp) -> Result<String, LuaError> {
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

use call::{run_call, worker_drive_model};
use lua_json::{
    json_to_lua, lua_to_json, lua_type_name, lua_value_to_string, stringify_json,
    stringify_tool_result,
};
