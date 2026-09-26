//! Process-global extension manager: sync reads, hooks, drive model, net fetch, commands.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::llm::config::ExtensionModelSnapshot;
use crate::protocol::ToolDefinition;

use super::appendix::{full_tool_name, normalize_tool_name};
use super::manager::ExtensionManager;
use super::{AfterOutcome, BeforeOutcome, CallKind, CompactAction, HostCtx, HOOK_TIMEOUT_SECS};
use super::{MAX_NET_RESPONSE_BYTES, MAX_NET_TIMEOUT_MS};

// ---------------------------------------------------------------------------
// Global (process-wide) manager: sync reads for schema + dispatch paths
// ---------------------------------------------------------------------------

pub static GLOBAL: OnceLock<std::sync::Arc<ExtensionManager>> = OnceLock::new();

pub fn global_manager() -> std::sync::Arc<ExtensionManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = std::sync::Arc::new(ExtensionManager::fresh());
            // Best-effort background load; the schema merges whatever is
            // cached (same contract as MCP). One-shot CLI awaits refresh
            // explicitly instead (see main.rs).
            let clone = std::sync::Arc::clone(&mgr);
            crate::runtime::http::spawn_task(async move { clone.refresh().await });
            mgr
        })
        .clone()
}

/// Bounded spin on `try_read` (same contract as MCP's `spin_read`): every
/// writer holds its guard for a bare swap, so contention clears within a
/// few yields and the never-block guarantee holds.
pub fn spin_read<T: Clone>(lock: &tokio::sync::RwLock<T>) -> Option<T> {
    spin_guard(lock).map(|guard| guard.clone())
}

/// Guard-returning spin for maps whose values aren't `Clone` (engines):
/// the caller reads through the guard instead of cloning.
pub fn spin_guard<T>(lock: &tokio::sync::RwLock<T>) -> Option<tokio::sync::RwLockReadGuard<'_, T>> {
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
pub fn cached_tools() -> Arc<[ToolDefinition]> {
    let Some(m) = GLOBAL.get() else {
        return Arc::new([]);
    };
    let all = spin_read(&m.cached).unwrap_or_else(|| Arc::new([]));
    match m
        .active
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
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
pub fn cached_schema_tokens() -> u64 {
    let Some(m) = GLOBAL.get() else {
        return 0;
    };
    if let Some(active) = m
        .active
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
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
pub async fn call_global(
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
pub async fn apply_before_hooks(
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
pub async fn apply_after_hooks(
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
pub async fn fire_event_global(
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
pub async fn apply_before_agent_start(
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
pub struct DriveModel {
    pub snapshot: Option<ExtensionModelSnapshot>,
    pub routing: BTreeMap<String, String>,
}

tokio::task_local! {
    static DRIVE_MODEL: std::cell::RefCell<DriveModel>;
}

/// Build this turn's drive context from its served config (no event, no
/// recording — `fire_model_select_if_changed` still owns those).
pub fn drive_model_for(config: &crate::llm::config::LlmConfig) -> DriveModel {
    DriveModel {
        snapshot: Some(served_snapshot_for(config)),
        routing: harvest_routing_headers(&config_header_layers(config)),
    }
}

/// Run `fut` with this turn's drive context visible to extension drives.
/// Nested scopes (a subagent child turn inside its parent) shadow and then
/// restore the outer turn's context.
pub async fn with_drive_model<Fut, T>(model: DriveModel, fut: Fut) -> T
where
    Fut: std::future::Future<Output = T>,
{
    DRIVE_MODEL.scope(std::cell::RefCell::new(model), fut).await
}

/// This turn's drive context, or an empty one outside any turn (one-shot
/// `dex run`, load-time reads — those fall back to the globals / file+env).
pub fn current_drive_model() -> DriveModel {
    DRIVE_MODEL
        .try_with(|slot| slot.borrow().clone())
        .unwrap_or_default()
}

/// Snapshot + id for a served config. Shared by the per-turn drive context
/// and the change-detection recorder below so the two never drift.
pub fn served_snapshot_for(config: &crate::llm::config::LlmConfig) -> ExtensionModelSnapshot {
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
pub static LAST_MODEL: Mutex<Option<ExtensionModelSnapshot>> = Mutex::new(None);

/// Routing-affinity headers recorded per turn (see
/// `fire_model_select_if_changed`): the `x-opencode-*` pair dex itself
/// injects into `extra_headers` for Console Go routing (the endpoint
/// rejects requests without them: `MissingSessionID`). `dex.net.fetch`
/// re-attaches them so extension calls to the same endpoint route like
/// dex's own — Lua-explicit headers always win. Fallback only, like
/// [`LAST_MODEL`]: inside a turn the drive context carries them.
pub static LAST_ROUTING_HEADERS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

/// The routing-affinity subset of a turn's resolved headers: only the
/// `x-opencode-*` pair, canonicalized to lowercase names. Layers are scanned
/// in wire precedence (global file < provider file < env/CLI), so a later
/// layer wins per key — a file-level pin must reach Lua's `dex.net.fetch`,
/// not just the injected per-turn values.
pub fn harvest_routing_headers(layers: &[&BTreeMap<String, String>]) -> BTreeMap<String, String> {
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
pub fn config_header_layers(
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
pub fn with_routing_headers(
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
pub fn served_model_snapshot() -> Option<ExtensionModelSnapshot> {
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
pub fn current_routing_headers() -> BTreeMap<String, String> {
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
pub async fn fire_model_select_if_changed(
    config: &crate::llm::config::LlmConfig,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) {
    let snapshot = served_snapshot_for(config);
    let id = snapshot.id();
    let previous = {
        let mut guard = LAST_MODEL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = guard.clone().map(|s| s.id());
        *guard = Some(snapshot);
        prev
    };
    // Same turn, same routing: the affinity headers dex injected into this
    // turn's `extra_headers` ride along so `dex.net.fetch` serves the same
    // endpoint without tripping `MissingSessionID`.
    {
        let mut guard = LAST_ROUTING_HEADERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
                .user_agent(crate::runtime::http::USER_AGENT)
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
pub async fn net_fetch(
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

/// `supervisor.route` for the spawn path: merged redirect/deny action, or
/// the default (no opinion) when unsubscribed. Carries the turn's policy +
/// filter so nested calls stay gated like model-issued ones.
pub async fn query_supervisor_route(
    agent: &str,
    task: Option<&str>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> super::SupervisorAction {
    if !has_event_handlers("supervisor.route") {
        return super::SupervisorAction::default();
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager()
        .query_supervisor_route(agent, task, &host)
        .await
}

/// Truncated text preview for observe-only lifecycle payloads: prompts and
/// responses can be pastes large enough to wedge the 64 MiB Lua VM, so hooks
/// get the head plus honest counts, never the whole body.
pub fn text_preview(text: &str) -> (String, bool, usize) {
    const MAX_CHARS: usize = 2000;
    let chars = text.chars().count();
    if chars <= MAX_CHARS {
        (text.to_string(), false, chars)
    } else {
        (text.chars().take(MAX_CHARS).collect(), true, chars)
    }
}

/// Observe-only lifecycle fire: `message.received`, `message.sent`,
/// `session.created`, `session.loaded`. Zero-cost without subscribers;
/// read-only nested policy; payloads carry truncated previews.
pub async fn fire_lifecycle_event(
    event: &'static str,
    payload: serde_json::Value,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) {
    if !has_event_handlers(event) {
        return;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager().fire_event(event, payload, &host).await;
}

/// `permission.request` for the approval gate: first explicit
/// `{decision = "allow"|"deny"}` wins, `None` (unsubscribed or no opinion)
/// means the normal approval flow. Carries the turn's policy + filter so a
/// nested `dex.tools.call` is gated exactly like a model-issued one.
pub async fn query_permission_request(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    requirement: &str,
    mode: &str,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> Option<(super::PermissionDecision, String, String)> {
    if !has_event_handlers("permission.request") {
        return None;
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager()
        .query_permission(tool, args, requirement, mode, &host)
        .await
}

/// `llm.before` for `before_model`: appends persisted as a user-role note
/// before the compaction gate (see the manager method). Zero-cost without
/// subscribers. Read-only host: decision hooks cannot mutate.
pub async fn apply_llm_before(
    messages: usize,
    stored_tokens: u64,
    overhead: u64,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Vec<String> {
    if !has_event_handlers("llm.before") {
        return Vec::new();
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .apply_llm_before(
            &host,
            serde_json::json!({
                "messages": messages,
                "stored_tokens": stored_tokens,
                "overhead": overhead,
            }),
        )
        .await
}

/// `harness.overflow` for the recovery path: first non-nil `{overflow}`
/// wins, `None` (unsubscribed or no opinion) means the Rust default.
/// Read-only host: decision hooks cannot mutate.
pub async fn query_harness_overflow(
    message: &str,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<bool> {
    if !has_event_handlers("harness.overflow") {
        return None;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .query_harness_overflow(message, &host)
        .await
}

/// `harness.conflict` for the batch scheduler: first non-nil `{conflicts}`
/// wins for the whole batch (one Lua round-trip per batch, never per pair),
/// `None` means the Rust conflict detector. Read-only host.
pub async fn query_harness_conflict(
    calls: &[crate::protocol::LlmToolCall],
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<bool> {
    if !has_event_handlers("harness.conflict") {
        return None;
    }
    let payload = calls
        .iter()
        .map(|c| serde_json::json!({"name": c.function.name, "args": c.function.arguments}))
        .collect::<Vec<_>>();
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .query_harness_conflict(&serde_json::Value::Array(payload), &host)
        .await
}

/// `harness.compact` for the turn loop's compaction gate: first non-`nil`
/// `{compact}` wins, `None` (unsubscribed, error, or no opinion) means the
/// Rust trigger decides. Read-only host.
pub async fn query_harness_compact(
    stored_tokens: u64,
    ephemeral_overhead: u64,
    message_count: usize,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<bool> {
    if !has_event_handlers("harness.compact") {
        return None;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .query_harness_compact(stored_tokens, ephemeral_overhead, message_count, &host)
        .await
}

/// `model_selector` for `process_turn`: first non-empty `{model = "..."}`
/// (or a bare string return) picks the model this turn serves; `None`
/// (unsubscribed, error, or no opinion) keeps the configured model.
/// Read-only host — choosing a model must not require tool access. The
/// payload names the configured and last served model ids, never secrets.
pub async fn query_model_selector_global(
    config: &crate::llm::config::LlmConfig,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<String> {
    if !has_event_handlers("model_selector") {
        return None;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    let previous = LAST_MODEL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|s| s.id());
    global_manager()
        .query_model_selector(
            &served_snapshot_for(config).id(),
            previous.as_deref(),
            &host,
        )
        .await
}

/// The agent-loop slot (spec §9): first loaded extension that registered a
/// loop via `dex.replace("agent_loop", …)`, or `None` for the Rust
/// `run_turn` default. Queried once per turn at the snapshot point.
pub async fn agent_loop_global() -> Option<(String, crate::extensions::ExtensionEngine)> {
    global_manager().agent_loop().await
}

/// `tool_catalog` for the turn's schema assembly: the first `{keep}/{drop}`
/// opinion narrows the served schemas; `None` (unsubscribed, error, no
/// opinion) keeps the full catalog. Read-only host — filtering schemas must
/// not require tool access.
pub async fn query_tool_catalog_global(
    schemas: &[crate::protocol::ToolDefinition],
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<Vec<crate::protocol::ToolDefinition>> {
    if !has_event_handlers("tool_catalog") {
        return None;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager().query_tool_catalog(schemas, &host).await
}

/// `harness.summarize` for compaction: first non-empty `{summary}` wins;
/// `None` (unsubscribed, error, or no opinion) means the Rust summarizer.
/// Read-only host — a summarizer cannot mutate anything.
pub async fn query_harness_summarize(
    conversation: &str,
    previous_summary: Option<&str>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Option<String> {
    if !has_event_handlers("harness.summarize") {
        return None;
    }
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .query_harness_summarize(conversation, previous_summary, &host)
        .await
}

/// `session.before_compact` for `compact_history`. The host runs without a
/// turn policy at this seam, so nested `dex.tools.call` upcalls inherit a
/// read-only policy — a compaction hook cannot mutate anything.
pub async fn apply_before_compact(
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
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
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
pub fn command_list() -> Vec<(String, String, String)> {
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
pub async fn run_command_global(
    ext_id: &str,
    name: &str,
    arg: &str,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<String, String> {
    let policy = crate::tools::Policy::turn(
        crate::protocol::PermissionMode::ReadOnly,
        &crate::runtime::console::Console::none(),
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
        )
        .await
}

/// `dex.tools.list()`: the extension tool names (full `ext__` names).
/// Spins under contention like every other sync cache read (see
/// `global::spin_read`) instead of failing open to an empty list.
pub fn tools_list() -> Vec<String> {
    GLOBAL
        .get()
        .and_then(|m| {
            spin_read(&m.cached).map(|t| t.iter().map(|d| d.function.name.clone()).collect())
        })
        .unwrap_or_default()
}

/// `dex.tools.set_active(list)`: persist the schema slice; an empty list
/// means "no extension tools". Short (own-extension) names resolve to full
/// `ext__<ext>__<tool>` names here so extension code never spells the
/// prefix; full names pass through, and unknown names filter out at read
/// time (see `set_active`).
pub async fn set_active_global(ext: &str, tools: Vec<String>) {
    let tools = tools.iter().map(|t| resolve_active_name(ext, t)).collect();
    if let Some(m) = GLOBAL.get() {
        m.set_active(tools).await;
    }
}

/// Resolve one `set_active` entry to its full name: already-full `ext__`
/// names pass through, legacy `lua__` names normalize to `ext__` (so the
/// read-time filter against canonical cache names still matches), and
/// anything else names the caller's own tool.
pub fn resolve_active_name(ext: &str, name: &str) -> String {
    if name.starts_with("ext__") {
        name.to_string()
    } else if name.starts_with("lua__") {
        normalize_tool_name(name)
    } else {
        full_tool_name(ext, name)
    }
}

/// Dispatch a shadowed built-in through its shadow (plan §6.4 step 4).
pub async fn call_shadow_global(
    target: &str,
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
    global_manager().call_shadow(target, args, &host).await
}

/// Per-extension status for `dex extensions list` / `/extensions`: (id,
/// version, tools, events) for every loaded engine. Sync snapshot, never
/// blocks (same contract as `cached_tools`).
pub fn loaded_summaries() -> Vec<(String, String, Vec<String>, Vec<String>)> {
    GLOBAL
        .get()
        .and_then(|m| {
            spin_guard(&m.engines).map(|engines| {
                engines
                    .values()
                    .map(|e| {
                        (
                            e.manifest.id.clone(),
                            e.manifest.version.clone(),
                            e.tools.clone(),
                            e.events.clone(),
                        )
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Sync shadow check for `metadata()`: a shadowed built-in is Shell-gated.
/// Never blocks — empty until the first refresh lands (same as the schema).
pub fn is_shadowed(name: &str) -> bool {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.shadowed).map(|s| s.contains(name)))
        .unwrap_or(false)
}

/// Does any loaded extension subscribe to `event`? Fast path so the common
/// no-hooks turn skips arg cloning + JSON round-trips entirely.
pub fn has_event_handlers(event: &str) -> bool {
    GLOBAL
        .get()
        .and_then(|m| {
            spin_guard(&m.engines).map(|e| {
                e.values()
                    .any(|ext| ext.events.contains(&event.to_string()))
            })
        })
        .unwrap_or(false)
}
