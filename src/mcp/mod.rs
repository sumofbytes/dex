//! MCP (Model Context Protocol) client: use external servers as tools.
//!
//! P0: `tools/list` + `tools/call` over stdio + Streamable HTTP, exposed as
//! `mcp__<server>__<tool>`. P1: `resources/list|read` + `prompts/list|get`
//! via one synthetic `mcp__<server>_read_resource` tool per server.
//!
//! Security: server allow/deny filtering, secret redaction in errors, tool
//! allowlist/denylist per server, a schema cap so one chatty server cannot
//! flood the context. See `SECURITY.md` (MCP section) for the threat model.
//!
//! Performance: one shared reqwest client, cached [`ToolDefinition`]s merged
//! synchronously into `tools_schema()` (never blocks the turn loop), fan-out
//! refresh via `JoinSet`, per-server timeout, 32 KiB output clamp. One bad
//! server goes `down` and never breaks the turn.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::protocol::ToolDefinition;

pub(crate) mod config;
pub(crate) mod mapping;
pub(crate) mod oauth;
pub(crate) mod redact;
pub(crate) mod transport;

pub(crate) use config::load_server_configs;
use config::{
    def_belongs_to, insert_cached, mcp_max_tools, sanitize_server_name, split_mcp_name,
    McpServerConfig,
};
#[cfg(test)]
use config::{expand_env, mcp_enabled, mcp_tool_name, parse_mcp_servers};
#[cfg(test)]
use config::{MCP_DESC_LIMIT, MCP_TOOL_NAME_LIMIT};
use mapping::{clamp_output, content_to_text, json_arr, resource_reader_definition, McpTool};
#[cfg(test)]
use redact::redact_line;
use redact::redact_secrets;
use transport::{rpc_error, HttpTransport, McpTransport, StdioTransport};
#[cfg(test)]
use transport::{sse_result_id, sse_scan_buffered_id};

// ---------------------------------------------------------------------------
// Client + manager
// ---------------------------------------------------------------------------

pub(crate) struct McpClient {
    transport: Box<dyn McpTransport>,
    timeout: Duration,
}

impl McpClient {
    fn new(transport: Box<dyn McpTransport>, timeout_secs: u64) -> Self {
        Self {
            transport,
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }

    /// One call with timeout + cancellation. On cancel the in-flight
    /// transport future is dropped (aborting the HTTP request or the stdio
    /// read); the stdio id-matching loop skips the orphaned reply, so the
    /// next call on the same server stays in sync.
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Value, String> {
        let run = tokio::time::timeout(self.timeout, self.transport.request(method, params));
        match cancel {
            Some(c) => {
                tokio::select! {
                    r = run => r.map_err(|_| format!("mcp {method} timed out"))?,
                    _ = wait_cancelled(c) => Err("mcp call cancelled".to_string()),
                }
            }
            None => run.await.map_err(|_| format!("mcp {method} timed out"))?,
        }
    }

    /// Liveness probe for the sweeper. Cheap (`tools/list`), never surfaced.
    pub(crate) async fn ping(&self) -> Result<(), String> {
        self.call("tools/list", serde_json::json!({}), None)
            .await
            .map(|_| ())
    }

    pub(crate) async fn list_tools(
        &self,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Vec<McpTool>, String> {
        let v = self
            .call("tools/list", serde_json::json!({}), cancel)
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(format!("tools/list: {err}"));
        }
        let mut out = Vec::new();
        for t in json_arr(&v, "tools") {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            out.push(McpTool {
                name: name.to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            });
        }
        Ok(out)
    }

    pub(crate) async fn call_tool(
        &self,
        tool: &str,
        args: &Value,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Value, String> {
        let v = self
            .call(
                "tools/call",
                serde_json::json!({"name": tool, "arguments": args}),
                cancel,
            )
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(err);
        }
        Ok(v)
    }

    pub(crate) async fn has_resources(
        &self,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> bool {
        self.call("resources/list", serde_json::json!({}), cancel)
            .await
            .map(|v| v.get("resources").and_then(Value::as_array).is_some())
            .unwrap_or(false)
    }

    pub(crate) async fn read_resource(
        &self,
        uri: &str,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<String, String> {
        let v = self
            .call("resources/read", serde_json::json!({"uri": uri}), cancel)
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(err);
        }
        // `resources/read` returns `contents[]` with inline `text`/`blob`
        // (no `type` discriminator), unlike `tools/call` `content[]`.
        let mut parts = Vec::new();
        for item in json_arr(&v, "contents") {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("data");
                parts.push(format!("[blob omitted: {mime} {} bytes]", blob.len()));
            }
        }
        let mut text = parts.join("\n");
        if text.is_empty() {
            text = "(empty resource)".to_string();
        }
        Ok(clamp_output(text))
    }
}

pub(crate) struct ServerStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) tools: usize,
    pub(crate) error: Option<String>,
}

/// One server's fetched schema slice: raw defs plus the synthetic reader.
/// The caller merges (collision renames need the shared map, so merging
/// stays serial over a sorted server list).
struct ServerDefs {
    tools: Vec<(ToolDefinition, String)>,
    reader: Option<ToolDefinition>,
}

pub(crate) struct McpManager {
    configs: BTreeMap<String, McpServerConfig>,
    clients: RwLock<HashMap<String, Arc<McpClient>>>,
    down: RwLock<HashMap<String, String>>,
    /// Schema cache behind `Arc`: readers clone the `Arc`, never the defs
    /// (each `parameters: Value` re-serializes expensively — see
    /// `cached_schema_tokens`).
    cached_tools: RwLock<Arc<[ToolDefinition]>>,
    cached_names: RwLock<HashMap<String, (String, String)>>,
    cached_truncated: RwLock<usize>,
    /// Token cost of `cached_tools`, precomputed at swap time: the per-turn
    /// budget reads this instead of re-running `schema_token_estimate`
    /// (`parameters.to_string()` per def) on every model call.
    cached_schema_tokens: RwLock<u64>,
}

impl McpManager {
    pub(crate) fn new(configs: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            configs,
            clients: RwLock::new(HashMap::new()),
            down: RwLock::new(HashMap::new()),
            cached_tools: RwLock::new(Arc::new([])),
            cached_names: RwLock::new(HashMap::new()),
            cached_truncated: RwLock::new(0),
            cached_schema_tokens: RwLock::new(0),
        }
    }

    pub(crate) fn from_env() -> Arc<Self> {
        Arc::new(Self::new(load_server_configs()))
    }

    /// Insert a pre-built client (tests / daemon bootstrap).
    #[cfg(test)]
    pub(crate) async fn insert_client(&self, server: &str, client: McpClient) {
        self.clients
            .write()
            .await
            .insert(server.to_string(), Arc::new(client));
    }

    /// Connect all servers concurrently; one failure never fails the batch.
    pub(crate) async fn refresh(self: &Arc<Self>) {
        if self.configs.is_empty() {
            return;
        }
        let mut set = tokio::task::JoinSet::new();
        for (name, cfg) in self.configs.clone() {
            let this = Arc::clone(self);
            set.spawn(async move { this.connect_one(&name, &cfg).await });
        }
        while set.join_next().await.is_some() {}
        self.rebuild_cache().await;
    }

    async fn connect_one(&self, name: &str, cfg: &McpServerConfig) {
        if self.clients.read().await.contains_key(name) {
            return;
        }
        let transport: Result<Box<dyn McpTransport>, String> = if cfg.is_http() {
            Ok(Box::new(HttpTransport::new(name, cfg)))
        } else {
            match StdioTransport::spawn(cfg).await {
                Ok(t) => Ok(Box::new(t)),
                Err(e) => Err(redact_secrets(&e)),
            }
        };
        match transport {
            Ok(t) => {
                let client = Arc::new(McpClient::new(t, cfg.timeout_secs));
                // Probe with tools/list so a dead server goes `down` now,
                // not mid-turn.
                match client.list_tools(None).await {
                    Ok(_) => {
                        self.clients.write().await.insert(name.to_string(), client);
                        self.down.write().await.remove(name);
                    }
                    Err(e) => {
                        self.down
                            .write()
                            .await
                            .insert(name.to_string(), redact_secrets(&e));
                    }
                }
            }
            Err(e) => {
                self.down.write().await.insert(name.to_string(), e);
            }
        }
    }

    async fn rebuild_cache(&self) {
        let clients = self.clients.read().await.clone();
        // Fan out the per-server RPCs: `fetch_server_defs` holds no locks,
        // so one slow server never stalls the rest (previously serial, 2
        // RPCs each with 30s timeouts).
        let mut set = tokio::task::JoinSet::new();
        for (server, client) in &clients {
            // Test clients have no config entry: default allows everything.
            let cfg = self.configs.get(server).cloned().unwrap_or_default();
            let server = server.clone();
            let client = Arc::clone(client);
            set.spawn(async move {
                let defs = Self::fetch_server_defs(&server, &cfg, &client).await;
                (server, defs)
            });
        }
        let mut fetched = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(row) = joined {
                fetched.push(row);
            }
        }
        // Merge single-threaded over a sorted server list: collision renames
        // (`~2` suffixes) are order-dependent, so a fixed order keeps them
        // stable from rebuild to rebuild.
        fetched.sort_by(|a, b| a.0.cmp(&b.0));
        let mut tools = Vec::new();
        let mut names = HashMap::new();
        for (server, defs) in fetched {
            for (def, tool) in defs.tools {
                insert_cached(&mut tools, &mut names, def, &server, &tool);
            }
            if let Some(def) = defs.reader {
                names.insert(
                    def.function.name.clone(),
                    (server.clone(), "\0resource".to_string()),
                );
                tools.push(def);
            }
        }
        self.enforce_cap(&mut tools, &mut names, mcp_max_tools())
            .await;
        self.swap_cache(tools, names).await;
    }

    /// Schema cap: the model pays for the schema on every request, so bound
    /// the merged cache. Sorted by name (client-map order is random), head
    /// kept, dropped count recorded for `/api/mcp`. `max` is a parameter
    /// (not read here) so tests can exercise the cap without mutating the
    /// process-global env table that parallel tests share.
    async fn enforce_cap(
        &self,
        tools: &mut Vec<ToolDefinition>,
        names: &mut HashMap<String, (String, String)>,
        max: usize,
    ) {
        tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        let dropped = tools.len().saturating_sub(max);
        if dropped > 0 {
            tools.truncate(max);
            let kept: std::collections::HashSet<&str> =
                tools.iter().map(|d| d.function.name.as_str()).collect();
            names.retain(|k, _| kept.contains(k.as_str()));
        }
        *self.cached_truncated.write().await = dropped;
    }

    /// One server's schema slice, fetched with no cache locks held: the
    /// `list_tools` + `has_resources` RPCs. Raw defs — the caller merges
    /// (collision renames need the shared map, so merging stays serial).
    /// List one server's tools (+ resource reader), honoring the server's
    /// allow/deny filter.
    async fn fetch_server_defs(
        server: &str,
        cfg: &McpServerConfig,
        client: &Arc<McpClient>,
    ) -> ServerDefs {
        let mut tools = Vec::new();
        let listed = client.list_tools(None).await.unwrap_or_default();
        for tool in &listed {
            if !cfg.tool_allowed(&tool.name) {
                continue;
            }
            tools.push((tool.to_definition(server), tool.name.clone()));
        }
        let reader = if client.has_resources(None).await {
            Some(resource_reader_definition(server))
        } else {
            None
        };
        ServerDefs { tools, reader }
    }

    /// Swap a freshly built cache under the write locks: the only locked
    /// section of the rebuild path — RPCs, cap math, and token math all
    /// happen lock-free on locals first.
    async fn swap_cache(
        &self,
        tools: Vec<ToolDefinition>,
        names: HashMap<String, (String, String)>,
    ) {
        let tokens = crate::agent::tokens::schema_token_estimate(&tools);
        *self.cached_tools.write().await = Arc::from(tools);
        *self.cached_names.write().await = names;
        *self.cached_schema_tokens.write().await = tokens;
    }

    /// Connect one server and merge its tools into the live cache (the batch
    /// `refresh` path rebuilds wholesale; this keeps a lazy connect cheap).
    /// Fetch first, lock only to swap: readers keep serving the previous
    /// cache across the lazy-connect RPCs instead of degrading to an empty
    /// slice under a held write lock.
    async fn connect_and_cache(&self, name: &str, cfg: &McpServerConfig) {
        self.connect_one(name, cfg).await;
        let Some(client) = self.clients.read().await.get(name).cloned() else {
            return;
        };
        let defs = Self::fetch_server_defs(name, cfg, &client).await;
        let current = self.cached_tools.read().await.clone();
        let mut tools: Vec<ToolDefinition> = current
            .iter()
            .filter(|d| !def_belongs_to(&d.function.name, name))
            .cloned()
            .collect();
        let mut names = self.cached_names.read().await.clone();
        names.retain(|_, (server, _)| server != name);
        for (def, tool) in defs.tools {
            insert_cached(&mut tools, &mut names, def, name, &tool);
        }
        if let Some(def) = defs.reader {
            names.insert(
                def.function.name.clone(),
                (name.to_string(), "\0resource".to_string()),
            );
            tools.push(def);
        }
        self.enforce_cap(&mut tools, &mut names, mcp_max_tools())
            .await;
        self.swap_cache(tools, names).await;
    }

    /// Drop the client and reconnect now; surfaces the error instead of only
    /// recording `down`. Backs `POST /api/mcp/{server}/reconnect`.
    pub(crate) async fn reconnect(&self, server: &str) -> Result<usize, String> {
        let cfg = self
            .configs
            .get(server)
            .cloned()
            .ok_or_else(|| format!("unknown mcp server '{server}'"))?;
        self.clients.write().await.remove(server);
        self.down.write().await.remove(server);
        self.connect_and_cache(server, &cfg).await;
        if self.clients.read().await.contains_key(server) {
            let tools = self.cached_tools.read().await;
            Ok(tools
                .iter()
                .filter(|d| def_belongs_to(&d.function.name, server))
                .count())
        } else {
            Err(self
                .down
                .read()
                .await
                .get(server)
                .cloned()
                .unwrap_or_else(|| "not connected".to_string()))
        }
    }

    /// Ping every client concurrently; drop the dead so they go `down`
    /// before the next turn. Runs every 60s on the global manager; never
    /// fails the batch.
    pub(crate) async fn sweep_once(&self) {
        let clients: Vec<(String, Arc<McpClient>)> =
            self.clients.read().await.clone().into_iter().collect();
        let mut set = tokio::task::JoinSet::new();
        for (name, client) in clients {
            set.spawn(async move {
                let dead = client.ping().await.is_err();
                (name, dead)
            });
        }
        let mut dead = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((name, true)) = joined {
                dead.push(name);
            }
        }
        if dead.is_empty() {
            return;
        }
        {
            let mut clients = self.clients.write().await;
            let mut down = self.down.write().await;
            for name in &dead {
                clients.remove(name);
                down.insert(
                    name.clone(),
                    "liveness probe failed; will reconnect on next use".to_string(),
                );
            }
        }
        self.rebuild_cache().await;
    }

    #[cfg(test)]
    pub(crate) async fn tool_definitions(&self) -> Arc<[ToolDefinition]> {
        self.cached_tools.read().await.clone()
    }

    pub(crate) async fn statuses(&self) -> Vec<ServerStatus> {
        let clients = self.clients.read().await;
        let tools = self.cached_tools.read().await;
        let down = self.down.read().await;
        Self::status_list(&self.configs, &clients, &tools, &down)
    }

    fn status_list(
        configs: &BTreeMap<String, McpServerConfig>,
        clients: &HashMap<String, Arc<McpClient>>,
        tools: &[ToolDefinition],
        down: &HashMap<String, String>,
    ) -> Vec<ServerStatus> {
        let mut out: Vec<ServerStatus> = configs
            .keys()
            .map(|name| {
                let count = tools
                    .iter()
                    .filter(|d| def_belongs_to(&d.function.name, name))
                    .count();
                let state = if clients.contains_key(name) {
                    "up"
                } else {
                    "down"
                };
                ServerStatus {
                    name: name.clone(),
                    state: state.to_string(),
                    tools: count,
                    error: down.get(name).cloned(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Dispatch `mcp__<server>__<tool>` (or the `_read_resource` reader).
    /// All errors pass through [`redact_secrets`] — one choke point so
    /// echoed headers/args never reach the model or logs.
    pub(crate) async fn call_tool(
        &self,
        full_name: &str,
        args: &serde_json::Map<String, Value>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<String, String> {
        self.call_tool_inner(full_name, args, cancel)
            .await
            .map_err(|e| redact_secrets(&e))
    }

    async fn call_tool_inner(
        &self,
        full_name: &str,
        args: &serde_json::Map<String, Value>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<String, String> {
        let (server, tool) = split_mcp_name(full_name).ok_or("unknown MCP tool")?;
        // Lazy connect: the background refresh may not have run (or the
        // server was added after boot). Connect on demand so the first call
        // works instead of reporting a phantom `down`.
        if !self.clients.read().await.contains_key(&server) {
            if let Some(cfg) = self.configs.get(&server).cloned() {
                self.connect_and_cache(&server, &cfg).await;
            }
        }
        // Resolve against the cache so renames/fakes stay consistent. The
        // split-name fallback is only for servers without a config entry
        // (test fakes): with a config, `connect_and_cache`/`rebuild_cache`
        // just populated the cache, so a miss means filtered or unknown —
        // falling back would bypass allow/deny.
        let cached = self.cached_names.read().await.get(full_name).cloned();
        let (server, tool) = match cached {
            Some(resolved) => resolved,
            None if self.configs.contains_key(&server) => {
                return Err(format!("unknown MCP tool '{full_name}'"));
            }
            None => (server, tool),
        };
        let clients = self.clients.read().await.get(&server).cloned();
        // Clone the client out of the map and drop the guard before any
        // `.await`: holding the read guard across the tool RPC (up to the
        // 30s timeout) would stall every map writer — reconnect, sweeper
        // drops, lazy-connect inserts — on a hung server. Read-shared
        // otherwise, so concurrent calls never block each other here.
        let Some(client) = clients else {
            let reason = self
                .down
                .read()
                .await
                .get(&server)
                .cloned()
                .unwrap_or_else(|| "not connected".to_string());
            return Err(format!("mcp server '{server}' is down: {reason}"));
        };
        if tool == "\0resource" {
            let uri = args.get("uri").and_then(Value::as_str).unwrap_or("");
            if uri.is_empty() {
                return Err("missing argument 'uri'".to_string());
            }
            return client.read_resource(uri, Some(cancel)).await;
        }
        let result = client
            .call_tool(&tool, &Value::Object(args.clone()), Some(cancel))
            .await?;
        Ok(content_to_text(&result))
    }
}

// ---------------------------------------------------------------------------
// Global (process-wide) manager: sync reads for schema + approval paths
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Arc<McpManager>> = OnceLock::new();

/// Serialize tests that mutate the MCP environment: test threads share one
/// process-global env table, so a set_var window in one test can flip an
/// env read in another (observed: schema-cap test vs concurrent rebuilds).
/// Tokio mutex: guards are held across `.await` (handler calls that read
/// the env), which a std mutex forbids under clippy.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn global_manager() -> Arc<McpManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = McpManager::from_env();
            // Best-effort background connect; schema merges whatever is cached.
            let clone = Arc::clone(&mgr);
            crate::client::http::spawn_task(async move { clone.refresh().await });
            // Liveness sweeper: ping each client every 60s so a server that
            // died mid-session goes `down` before the next turn uses it.
            let sweep = Arc::clone(&mgr);
            crate::client::http::spawn_task(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    sweep.sweep_once().await;
                }
            });
            mgr
        })
        .clone()
}

/// Bounded spin on `try_read`: every writer holds its guard for a bare
/// swap (never across an await), so a contended first try succeeds within
/// a few yields. A single `try_read().unwrap_or_default()` undercounts the
/// compaction budget to 0 under contention and skips a needed compaction —
/// or drops the tools from one request's schema. The spin keeps the
/// never-block contract (bounded yields) while making the fallback
/// ~unreachable.
fn spin_read<T: Clone>(lock: &RwLock<T>) -> Option<T> {
    for _ in 0..16 {
        if let Ok(guard) = lock.try_read() {
            return Some(guard.clone());
        }
        std::thread::yield_now();
    }
    None
}

/// Cached MCP tools for `tools_schema()` — never blocks, never fails.
/// Clones the `Arc`, not the defs.
pub(crate) fn cached_tools() -> Arc<[ToolDefinition]> {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_tools))
        .unwrap_or_else(|| Arc::new([]))
}

/// Token cost of the cached MCP schema slice, for the compaction budget.
/// Precomputed at cache-swap time — a cached load, never a re-serialize.
/// Never blocks the loop; contention spins (see `spin_read`) instead of
/// returning 0 and skipping a needed compaction.
pub(crate) fn cached_schema_tokens() -> u64 {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_schema_tokens))
        .unwrap_or_default()
}

/// Tools dropped from the schema by the cap (0 when everything fits).
pub(crate) fn cached_truncated() -> usize {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_truncated))
        .unwrap_or_default()
}

/// Ephemeral MCP status line for the turn-loop compaction budget: priced,
/// never stored. `None` when the manager was never initialized (no MCP
/// tools in the schema then either) or a lock is contended — the budget
/// probe must never block the loop or spawn the background refresh.
pub(crate) fn ephemeral_line() -> Option<String> {
    let mgr = GLOBAL.get()?;
    // Zero-config managers have no line to print (the cached-status path
    // below still yields `Some(vec![])` there); keep this gate here only.
    if mgr.configs.is_empty() {
        return None;
    }
    try_snapshot(mgr).and_then(|st| crate::ui::format::mcp_status_line(&st))
}

pub(crate) async fn call_global(
    name: &str,
    args: &serde_json::Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, String> {
    global_manager().call_tool(name, args, cancel).await
}

/// Sync snapshot of per-server status for the `/mcp` slash command: never
/// blocks, never initializes the manager (a slash handler must not spawn
/// the background refresh). `None` when uninitialized or contended — the
/// caller renders that as "unavailable" rather than an empty server list,
/// which would wrongly imply no MCP is configured.
pub(crate) fn cached_statuses() -> Option<Vec<ServerStatus>> {
    let mgr = GLOBAL.get()?;
    try_snapshot(mgr)
}

/// Lock-free snapshot of `status_list` via `try_read`: `None` when a lock is
/// contended — callers must treat that as "unavailable", never as an empty
/// server list (which would wrongly imply no MCP is configured).
fn try_snapshot(mgr: &McpManager) -> Option<Vec<ServerStatus>> {
    let clients = spin_read(&mgr.clients)?;
    let tools = spin_read(&mgr.cached_tools)?;
    let down = spin_read(&mgr.down)?;
    Some(McpManager::status_list(
        &mgr.configs,
        &clients,
        &tools,
        &down,
    ))
}

// ---------------------------------------------------------------------------
// Tests (TDD: naming, config, mapping, manager isolation)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
