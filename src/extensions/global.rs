//! Process-global extension manager: sync reads, hooks, drive model, net fetch, commands.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::core::types::ToolDefinition;
use crate::llm::config::ExtensionModelSnapshot;

use super::manager::ExtensionManager;
use super::{has_event_handlers, MAX_NET_RESPONSE_BYTES, MAX_NET_TIMEOUT_MS};
use super::{AfterOutcome, BeforeOutcome, CallKind, CompactAction, HostCtx, HOOK_TIMEOUT_SECS};

// ---------------------------------------------------------------------------
// Global (process-wide) manager: sync reads for schema + dispatch paths
// ---------------------------------------------------------------------------

pub(crate) static GLOBAL: OnceLock<std::sync::Arc<ExtensionManager>> = OnceLock::new();

pub(crate) fn global_manager() -> std::sync::Arc<ExtensionManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = std::sync::Arc::new(ExtensionManager::fresh());
            // Best-effort background load; the schema merges whatever is
            // cached (same contract as MCP). One-shot CLI awaits refresh
            // explicitly instead (see main.rs).
            let clone = std::sync::Arc::clone(&mgr);
            crate::client::http::spawn_task(async move { clone.refresh().await });
            mgr
        })
        .clone()
}

/// Bounded spin on `try_read` (same contract as MCP's `spin_read`): every
/// writer holds its guard for a bare swap, so contention clears within a
/// few yields and the never-block guarantee holds.
pub(crate) fn spin_read<T: Clone>(lock: &tokio::sync::RwLock<T>) -> Option<T> {
    spin_guard(lock).map(|guard| guard.clone())
}

/// Guard-returning spin for maps whose values aren't `Clone` (engines):
/// the caller reads through the guard instead of cloning.
pub(crate) fn spin_guard<T>(
    lock: &tokio::sync::RwLock<T>,
) -> Option<tokio::sync::RwLockReadGuard<'_, T>> {
    for _ in 0..16 {
        if let Ok(guard) = lock.try_read() {
            return Some(guard);
        }
        std::thread::yield_now();
    }
    None
}

/// Cached extension tools for `tools_schema()` — never blocks, never fails.
/// Clones the `Arc`, not the defs.
pub(crate) fn cached_tools() -> Arc<[ToolDefinition]> {
    let Some(m) = GLOBAL.get() else {
        return Arc::new([]);
    };
    let all = spin_read(&m.cached).unwrap_or_else(|| Arc::new([]));
    match m.active.read().expect("active lock").clone() {
        None => all,
        // Filtered here, not at set time: a load-time `set_active` races
        // the cache rebuild, and dropping unknown names there would wedge
        // a fresh process to an empty schema.
        Some(active) => {
            let wanted: HashSet<String> = active.into_iter().collect();
            all.iter()
                .filter(|d| wanted.contains(&d.function.name))
                .cloned()
                .collect::<Vec<_>>()
                .into()
        }
    }
}

/// Token cost of the cached extension schema slice, for the compaction budget.
/// Precomputed at rebuild — a cached load, never a re-serialize. With an
/// `active` slice the filtered set sums the precomputed per-tool costs (no
/// live serialization of the filtered defs). Never blocks the loop;
/// contention spins (see `spin_read`), and a still-contended costs read
/// falls back to the whole-cache total (conservative: compacts earlier,
/// never later).
pub(crate) fn cached_schema_tokens() -> u64 {
    let Some(m) = GLOBAL.get() else {
        return 0;
    };
    if let Some(active) = m.active.read().expect("active lock").clone() {
        let wanted: HashSet<String> = active.into_iter().collect();
        if let Some(costs) = spin_read(&m.cached_costs) {
            // Same sum-then-divide formula as `schema_token_estimate`, on
            // the subset: bit-identical to estimating the sliced defs.
            let mut chars = 0u64;
            let mut count = 0u64;
            for (name, cost) in costs.iter() {
                if wanted.contains(name) {
                    chars += cost;
                    count += 1;
                }
            }
            return chars / 4 + count * crate::agent::tokens::PER_MESSAGE_OVERHEAD;
        }
    }
    spin_read(&m.cached_tokens).unwrap_or_default()
}

/// Dispatch `ext__<ext>__<tool>`. All errors are plain strings; the caller
/// maps to `ToolError::Internal` for the single audit row.
pub(crate) async fn call_global(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> Result<String, String> {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().call(name, args, &host).await
}

/// `tool.before` chain for the dispatch path. Zero-cost when no extension
/// subscribes: the args pass through uncloned.
pub(crate) async fn apply_before_hooks(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> BeforeOutcome {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    if !has_event_handlers("tool.before") {
        return BeforeOutcome::Proceed {
            args: args.clone(),
            mutated_by: Vec::new(),
        };
    }
    global_manager().apply_before_hooks(tool, args, &host).await
}

/// `tool.after` chain for the outcome path. Zero-cost without subscribers.
pub(crate) async fn apply_after_hooks(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    text: &str,
    ok: bool,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> AfterOutcome {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    if !has_event_handlers("tool.after") {
        return AfterOutcome {
            text: text.to_string(),
            ok,
        };
    }
    global_manager()
        .apply_after_hooks(tool, args, text, ok, &host)
        .await
}

/// Fire-and-forget lifecycle event for the turn loop. Zero-cost without
/// subscribers.
pub(crate) async fn fire_event_global(
    event: &str,
    payload: serde_json::Value,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) {
    if !has_event_handlers(event) {
        return;
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().fire_event(event, payload, &host).await;
}

/// `before_agent_start` for `process_turn`: appends to the system prompt
/// for this turn (see the manager method for merge/fail-open semantics).
pub(crate) async fn apply_before_agent_start(
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> Vec<String> {
    if !has_event_handlers("before_agent_start") {
        return Vec::new();
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().apply_before_agent_start(&host).await
}

/// Per-turn model drive context: the served snapshot + routing-affinity
/// headers for THIS turn. `process_turn` scopes it over the whole turn (see
/// `with_drive_model`); `drive()` snapshots it into each worker call and
/// `served_model_snapshot()` / `net_fetch` prefer it over the globals below.
/// Without it two concurrent turns (daemon sessions, or a subagent child
/// turn nested in its parent) would serve each other's model through the
/// process-wide fallback.
#[derive(Clone, Debug, Default)]
pub(crate) struct DriveModel {
    pub(crate) snapshot: Option<ExtensionModelSnapshot>,
    pub(crate) routing: BTreeMap<String, String>,
}

tokio::task_local! {
    static DRIVE_MODEL: std::cell::RefCell<DriveModel>;
}

/// Build this turn's drive context from its served config (no event, no
/// recording — `fire_model_select_if_changed` still owns those).
pub(crate) fn drive_model_for(config: &crate::llm::config::LlmConfig) -> DriveModel {
    DriveModel {
        snapshot: Some(served_snapshot_for(config)),
        routing: harvest_routing_headers(&config_header_layers(config)),
    }
}

/// Run `fut` with this turn's drive context visible to extension drives.
/// Nested scopes (a subagent child turn inside its parent) shadow and then
/// restore the outer turn's context.
pub(crate) async fn with_drive_model<Fut, T>(model: DriveModel, fut: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    DRIVE_MODEL.scope(std::cell::RefCell::new(model), fut).await
}

/// This turn's drive context, or an empty one outside any turn (one-shot
/// `dex run`, load-time reads — those fall back to the globals / file+env).
pub(crate) fn current_drive_model() -> DriveModel {
    DRIVE_MODEL
        .try_with(|slot| slot.borrow().clone())
        .unwrap_or_default()
}

/// Snapshot + id for a served config. Shared by the per-turn drive context
/// and the change-detection recorder below so the two never drift.
pub(crate) fn served_snapshot_for(
    config: &crate::llm::config::LlmConfig,
) -> ExtensionModelSnapshot {
    ExtensionModelSnapshot {
        provider: config.provider.name().to_string(),
        model: config.model.clone(),
        api: config.api.name().to_string(),
        base_url: config.base_url.clone(),
    }
}

/// Served-model snapshot recorded per turn (see `fire_model_select_if_changed`):
/// the daemon's per-request config — which the file never sees — wins over
/// file+env for `dex.model.current()/auth()` once a turn has served. Secrets
/// never land here (auth re-resolves the key from the deposits each call).
/// Fallback only: inside a turn the task-local drive context (which also
/// covers nested child turns) wins — this static exists for drives outside
/// any turn, where file+env resolution is process-global anyway.
pub(crate) static LAST_MODEL: Mutex<Option<ExtensionModelSnapshot>> = Mutex::new(None);

/// Routing-affinity headers recorded per turn (see
/// `fire_model_select_if_changed`): the `x-opencode-*` pair dex itself
/// injects into `extra_headers` for Console Go routing (the endpoint
/// rejects requests without them: `MissingSessionID`). `dex.net.fetch`
/// re-attaches them so extension calls to the same endpoint route like
/// dex's own — Lua-explicit headers always win. Fallback only, like
/// [`LAST_MODEL`]: inside a turn the drive context carries them.
pub(crate) static LAST_ROUTING_HEADERS: Mutex<BTreeMap<String, String>> =
    Mutex::new(BTreeMap::new());

/// The routing-affinity subset of a turn's resolved headers: only the
/// `x-opencode-*` pair, canonicalized to lowercase names. Layers are scanned
/// in wire precedence (global file < provider file < env/CLI), so a later
/// layer wins per key — a file-level pin must reach Lua's `dex.net.fetch`,
/// not just the injected per-turn values.
pub(crate) fn harvest_routing_headers(
    layers: &[&BTreeMap<String, String>],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for layer in layers {
        for (name, value) in *layer {
            if name.eq_ignore_ascii_case("x-opencode-session") {
                out.insert("x-opencode-session".to_string(), value.clone());
            } else if name.eq_ignore_ascii_case("x-opencode-client") {
                out.insert("x-opencode-client".to_string(), value.clone());
            }
        }
    }
    out
}

/// The three header layers of a resolved config, in wire precedence.
pub(crate) fn config_header_layers(
    config: &crate::llm::config::LlmConfig,
) -> [&BTreeMap<String, String>; 3] {
    [
        &config.global_headers,
        &config.provider_headers,
        &config.extra_headers,
    ]
}

/// Merge the recorded routing headers under Lua-explicit ones
/// (case-insensitive): an extension talking to its own model endpoint
/// routes like dex's own requests, but any per-call header still wins.
pub(crate) fn with_routing_headers(
    headers: &[(String, String)],
    routing: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = headers.to_vec();
    'next: for (name, value) in routing {
        for (existing, _) in headers {
            if existing.eq_ignore_ascii_case(name) {
                continue 'next;
            }
        }
        merged.push((name.clone(), value.clone()));
    }
    merged
}

/// The served snapshot: this turn's drive context wins when inside a turn
/// (per-turn scoping keeps concurrent sessions and nested child turns on
/// their own model); outside any turn, the last recorded turn, if any.
pub(crate) fn served_model_snapshot() -> Option<ExtensionModelSnapshot> {
    let drive = current_drive_model();
    if drive.snapshot.is_some() {
        drive.snapshot
    } else {
        LAST_MODEL.lock().ok().and_then(|guard| guard.clone())
    }
}

/// This turn's routing-affinity headers (empty when the turn carries none —
/// a turn without affinity headers must not inherit a stale session id);
/// outside any turn, the last recorded turn's.
pub(crate) fn current_routing_headers() -> BTreeMap<String, String> {
    let drive = current_drive_model();
    if drive.snapshot.is_some() {
        drive.routing
    } else {
        // Fail-open: a poisoned lock drops the affinity headers, never the call.
        LAST_ROUTING_HEADERS
            .lock()
            .ok()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

/// `model_select` for `process_turn`: record the served snapshot every turn
/// (even without subscribers — it backs `dex.model`), and fire the event
/// only when the `provider/model` id changed since the last turn (first turn
/// always fires, with `previous: null`). Fail-open like `fire_event` — a
/// broken handler logs and the turn proceeds. Nested `dex.tools.call` from
/// a handler inherits this turn's policy.
pub(crate) async fn fire_model_select_if_changed(
    config: &crate::llm::config::LlmConfig,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) {
    let snapshot = served_snapshot_for(config);
    let id = snapshot.id();
    let previous = {
        let mut guard = LAST_MODEL.lock().expect("served model lock");
        let prev = guard.clone().map(|s| s.id());
        *guard = Some(snapshot);
        prev
    };
    // Same turn, same routing: the affinity headers dex injected into this
    // turn's `extra_headers` ride along so `dex.net.fetch` serves the same
    // endpoint without tripping `MissingSessionID`.
    {
        let mut guard = LAST_ROUTING_HEADERS.lock().expect("routing headers lock");
        *guard = harvest_routing_headers(&config_header_layers(config));
    }
    if previous.as_deref() == Some(id.as_str()) || !has_event_handlers("model_select") {
        return;
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager()
        .fire_event(
            "model_select",
            serde_json::json!({ "model": id, "previous": previous }),
            &host,
        )
        .await;
}

/// `dex.net.fetch` client: like the shared streaming client but never
/// follows redirects. Confinement is checked against the requested URL, so
/// a 302 to another origin must surface as a 3xx value — never a followed
/// cross-origin fetch the check never saw.
static NET_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn net_client() -> reqwest::Client {
    NET_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(crate::client::http::USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// `dex.net.fetch` backing call (task side — the worker never touches the
/// network). Confined to the served model's own endpoint: scheme+host+port
/// must match its `base_url`, anything else is a loud error — and redirects
/// are never followed, so a 3xx surfaces as a value instead of escaping the
/// check. When the extension's manifest declares `net.providers`, the
/// allowlist widens to the configured provider endpoints (each fetched with
/// that provider's own key) — every allowed origin still comes from the
/// user's own config, never an arbitrary host. This turn's routing-affinity
/// headers ride along under Lua-explicit ones, so calls to a Console Go
/// endpoint route like dex's own. Non-2xx is a value
/// (`{status, headers, body}`), never an error. Errors never carry headers,
/// bodies, or URL queries (a `?key=` parameter would leak the key into
/// logs).
pub(crate) async fn net_fetch(
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    timeout_ms: u64,
    allow_configured_providers: bool,
) -> Result<String, String> {
    use reqwest::header::{HeaderName, HeaderValue};
    let snapshot = served_model_snapshot()
        .or_else(|| crate::llm::config::extension_model_snapshot().ok())
        .ok_or_else(|| "dex.net.fetch: no model configured".to_string())?;
    let base = reqwest::Url::parse(&snapshot.base_url)
        .map_err(|_| "dex.net.fetch: configured base_url is invalid".to_string())?;
    let parsed =
        reqwest::Url::parse(&url).map_err(|_| format!("dex.net.fetch: invalid url '{url}'"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "dex.net.fetch: only http(s) urls are allowed, got '{}'",
            parsed.scheme()
        ));
    }
    // Origin allowlist: the model's own endpoint always; the configured
    // provider endpoints when `net.providers` is declared. All of them
    // come from the user's own deposits — the fetch can reach another
    // provider, never an arbitrary host.
    let mut origins = vec![net_origin(&base)];
    if allow_configured_providers {
        for entry in crate::llm::config::extension_configured_providers() {
            if let Ok(endpoint) = reqwest::Url::parse(&entry.base_url) {
                origins.push(net_origin(&endpoint));
            }
        }
    }
    if !origins.iter().any(|origin| *origin == net_origin(&parsed)) {
        return Err(format!(
            "dex.net.fetch: url '{}' is outside the model endpoint '{}' and configured provider endpoints",
            net_display_url(&parsed),
            base.host_str().unwrap_or_default()
        ));
    }
    let method_name = method.to_ascii_uppercase();
    if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"].contains(&method_name.as_str()) {
        return Err(format!("dex.net.fetch: unsupported method '{method}'"));
    }
    if body.as_ref().is_some_and(|b| b.len() > 1024 * 1024) {
        return Err("dex.net.fetch: request body too large (max 1 MiB)".to_string());
    }
    let timeout = Duration::from_millis(timeout_ms.clamp(1_000, MAX_NET_TIMEOUT_MS));
    let client = net_client();
    let http_method = reqwest::Method::from_bytes(method_name.as_bytes())
        .map_err(|_| format!("dex.net.fetch: unsupported method '{method}'"))?;
    let mut request = client.request(http_method, parsed.clone()).timeout(timeout);
    let routing = current_routing_headers();
    for (name, value) in &with_routing_headers(&headers, &routing) {
        let lower = name.to_ascii_lowercase();
        if lower == "host" || lower == "content-length" {
            return Err(format!("dex.net.fetch: header '{name}' is host-controlled"));
        }
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("dex.net.fetch: invalid header name '{name}'"))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| format!("dex.net.fetch: invalid header value for '{name}'"))?;
        request = request.header(header_name, header_value);
    }
    if let Some(text) = body {
        request = request.body(text);
    }
    let display = net_display_url(&parsed);
    let response = request.send().await.map_err(|e| {
        format!(
            "dex.net.fetch {display} failed: {}",
            if e.is_timeout() {
                "timed out"
            } else if e.is_connect() {
                "connection failed"
            } else {
                "request failed"
            }
        )
    })?;
    let status = response.status().as_u16();
    let mut response_headers = BTreeMap::new();
    for (name, value) in response.headers() {
        let text = value.to_str().unwrap_or("<unprintable>").to_string();
        response_headers
            .entry(name.to_string())
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&text);
            })
            .or_insert(text);
    }
    let mut response_body = Vec::new();
    let mut stream = response;
    loop {
        match stream.chunk().await {
            Ok(None) => break,
            Ok(Some(chunk)) => {
                response_body.extend_from_slice(&chunk);
                if response_body.len() > MAX_NET_RESPONSE_BYTES {
                    return Err("dex.net.fetch: response too large (max 4 MiB)".to_string());
                }
            }
            Err(_) => return Err("dex.net.fetch: failed reading response body".to_string()),
        }
    }
    serde_json::to_string(&serde_json::json!({
        "status": status,
        "headers": response_headers,
        "body": String::from_utf8_lossy(&response_body),
    }))
    .map_err(|e| e.to_string())
}

/// Error-safe URL display: origin + path only — the query may carry the
/// provider key (`?key=…`), so it never reaches an error string.
fn net_display_url(url: &reqwest::Url) -> String {
    format!(
        "{}://{}{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.path()
    )
}

/// Scheme+host+port — the confinement identity for `net_fetch`.
fn net_origin(url: &reqwest::Url) -> (String, String, Option<u16>) {
    (
        url.scheme().to_string(),
        url.host_str().unwrap_or_default().to_string(),
        url.port_or_known_default(),
    )
}

/// `session.before_compact` for `compact_history`. The host runs without a
/// turn policy at this seam, so nested `dex.tools.call` upcalls inherit a
/// read-only policy — a compaction hook cannot mutate anything.
pub(crate) async fn apply_before_compact(
    emergency: bool,
    message_count: usize,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> CompactAction {
    if !has_event_handlers("session.before_compact") {
        return CompactAction {
            cancel: false,
            instructions: Vec::new(),
            summary: None,
        };
    }
    let policy = crate::tools::Policy::turn(
        crate::core::types::PermissionMode::ReadOnly,
        &crate::core::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .apply_before_compact(emergency, message_count, &host)
        .await
}

/// All registered extension slash commands: (extension id, name,
/// description), in load order.
pub(crate) fn command_list() -> Vec<(String, String, String)> {
    GLOBAL
        .get()
        .and_then(|m| {
            spin_guard(&m.engines).map(|engines| {
                engines
                    .values()
                    .flat_map(|e| {
                        e.commands
                            .iter()
                            .map(|(name, description)| {
                                (e.manifest.id.clone(), name.clone(), description.clone())
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Dispatch an extension slash command. Fire-and-forget from the TUI; the
/// result lands in the log.
pub(crate) async fn run_command_global(
    ext_id: &str,
    name: &str,
    arg: &str,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<String, String> {
    let policy = crate::tools::Policy::turn(
        crate::core::types::PermissionMode::ReadOnly,
        &crate::core::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    let manager = global_manager();
    let engine = {
        let engines = manager.engines.read().await;
        if let Some(e) = engines.get(ext_id) {
            e.engine.clone()
        } else {
            // Lazy self-heal like `call`: boot the addressed extension when
            // a command races the background refresh.
            drop(engines);
            manager
                .ensure_loaded(ext_id)
                .await
                .map_err(|_| format!("extension '{ext_id}' is not loaded"))?;
            let engines = manager.engines.read().await;
            engines
                .get(ext_id)
                .map(|e| e.engine.clone())
                .ok_or_else(|| format!("extension '{ext_id}' is not loaded"))?
        }
    };
    engine
        .drive(
            CallKind::Command {
                name: name.to_string(),
            },
            serde_json::Value::String(arg.to_string()),
            std::time::Duration::from_secs(HOOK_TIMEOUT_SECS),
            cancel,
            host,
            None,
        )
        .await
}
