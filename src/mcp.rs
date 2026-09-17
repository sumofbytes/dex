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
use crate::core::types::ToolDefinition;

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
    try_snapshot(mgr).and_then(|st| crate::core::format::mcp_status_line(&st))
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
mod tests {
    use super::*;

    struct FakeTransport {
        tools: Vec<McpTool>,
        resources: bool,
        fail: bool,
    }

    #[cfg(test)]
    impl McpTransport for FakeTransport {
        fn request<'a>(
            &'a self,
            method: &'a str,
            _params: Value,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
        {
            Box::pin(async move {
                if self.fail {
                    return Err("boom".to_string());
                }
                match method {
                    "tools/list" => Ok(
                        serde_json::json!({"tools": self.tools.iter().map(|t| serde_json::json!({"name": t.name, "description": t.description, "inputSchema": t.input_schema})).collect::<Vec<_>>() }),
                    ),
                    "tools/call" => {
                        Ok(serde_json::json!({"content": [{"type": "text", "text": "fake-ok"}]}))
                    }
                    "resources/list" => {
                        if self.resources {
                            Ok(serde_json::json!({"resources": [{"uri": "doc://a"}]}))
                        } else {
                            Ok(serde_json::json!({}))
                        }
                    }
                    "resources/read" => {
                        Ok(serde_json::json!({"contents": [{"uri": "doc://a", "text": "hello"}]}))
                    }
                    _ => Ok(serde_json::json!({})),
                }
            })
        }
    }

    #[test]
    fn sse_stream_decode_returns_first_result_with_early_exit() {
        // §20: notifications and blanks scan past; a result split across
        // chunks still matches once its newline arrives.
        let mut buf = Vec::new();
        let scan = |buf: &mut Vec<u8>, chunk: &[u8]| {
            buf.extend_from_slice(chunk);
            sse_scan_buffered_id(buf, 1)
        };
        assert!(scan(&mut buf, b": keep-alive\n\n").is_none());
        assert!(scan(
            &mut buf,
            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"progress\"}\n"
        )
        .is_none());
        assert!(scan(&mut buf, b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"res").is_none());
        let r = scan(&mut buf, b"ult\":{\"ok\":true}}\n").expect("split result matches");
        assert_eq!(r, serde_json::json!({"ok": true}));
        // Error envelopes match too; [DONE] and blanks don't.
        assert!(sse_result_id("data: [DONE]", 1).is_none());
        assert!(sse_result_id("", 1).is_none());
        assert!(sse_result_id(": comment", 1).is_none());
        let e =
            sse_result_id("data: {\"id\":1,\"error\":{\"code\":-1}}", 1).expect("error matches");
        assert!(e.get("__mcp_error").is_some());
        // Id-less envelopes are never this call's result: under concurrent
        // calls on one transport an id-less broadcast carrying `result`
        // would otherwise be stolen and misattributed to whoever scans first.
        assert!(sse_result_id("data: {\"error\":{\"code\":-1}}", 1).is_none());
        assert!(sse_result_id("data: {\"result\":{\"ok\":true}}", 1).is_none());
    }

    fn fake_client(tools: Vec<McpTool>, resources: bool, fail: bool) -> McpClient {
        McpClient::new(
            Box::new(FakeTransport {
                tools,
                resources,
                fail,
            }),
            5,
        )
    }

    static NO_CANCEL: crate::agent::state::GlobalCancellation =
        crate::agent::state::GlobalCancellation;

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.to_string(),
            description: "d".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn tool_names_roundtrip() {
        assert_eq!(mcp_tool_name("GitHub", "my-tool"), "mcp__github__my-tool");
        assert_eq!(
            split_mcp_name("mcp__github__search"),
            Some(("github".to_string(), "search".to_string()))
        );
        assert_eq!(
            split_mcp_name("mcp__gh_read_resource"),
            Some(("gh".to_string(), "\0resource".to_string()))
        );
        assert!(split_mcp_name("read").is_none());
        assert!(split_mcp_name("mcp__a__b__c").is_none());
    }

    #[test]
    fn server_names_are_sanitized() {
        assert_eq!(sanitize_server_name("GitHub-Prod"), "github_prod");
        assert_eq!(sanitize_server_name("a b.c"), "a_b_c");
    }

    #[test]
    fn env_vars_expand() {
        unsafe { std::env::set_var("DEX_MCP_TEST_X", "hello") };
        assert_eq!(expand_env("$DEX_MCP_TEST_X/world").unwrap(), "hello/world");
        assert_eq!(expand_env("${DEX_MCP_TEST_X}!").unwrap(), "hello!");
        // Fail-closed: a missing variable is an error, never silent `""`
        // (which would turn a missing key into an unauthenticated request).
        assert!(expand_env("$DEX_MCP_TEST_MISSING!").is_err());
    }

    #[test]
    fn env_vars_expand_non_ascii() {
        unsafe { std::env::set_var("DEX_MCP_TEST_UNICODE", "héllo") };
        // Multi-byte chars adjacent to an expansion must pass through
        // unchanged; `bytes[i] as char` used to mojibake `é` into `Ã©`.
        assert_eq!(
            expand_env("café $DEX_MCP_TEST_UNICODE bar").unwrap(),
            "café héllo bar"
        );
        assert_eq!(
            expand_env("café${DEX_MCP_TEST_UNICODE}!").unwrap(),
            "caféhéllo!"
        );
        // Fail-closed missing-var error still fires next to multi-byte text.
        assert_eq!(
            expand_env("café $DEX_MCP_TEST_MISSING bar").unwrap_err(),
            "mcp config: env var $DEX_MCP_TEST_MISSING is not set"
        );
    }

    #[test]
    fn config_parses_stdio_and_http() {
        unsafe { std::env::set_var("DEX_MCP_TEST_X", "hello") };
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
mcp_servers:
  gh:
    command: npx
    args: ["-y", "server"]
    env: {TOK: $DEX_MCP_TEST_X}
    allow: [search]
    deny: [exec]
  short: "uvx server --foo"
  web:
    url: https://example.com/mcp
    headers: {Authorization: Bearer x}
    timeout_secs: 5
  off:
    command: foo
    disabled: true
  empty: {}
"#,
        )
        .unwrap();
        let map = parse_mcp_servers(&yaml);
        assert!(map.contains_key("gh"));
        assert_eq!(map["gh"].args, vec!["-y", "server"]);
        assert_eq!(map["gh"].env.get("TOK").map(String::as_str), Some("hello"));
        assert_eq!(map["gh"].allow, vec!["search"]);
        assert_eq!(map["gh"].deny, vec!["exec"]);
        assert!(map["gh"].tool_allowed("search"));
        assert!(!map["gh"].tool_allowed("exec"));
        assert!(!map["gh"].tool_allowed("other"));
        assert_eq!(map["short"].command.as_deref(), Some("uvx"));
        assert_eq!(map["short"].args, vec!["server", "--foo"]);
        assert!(map.contains_key("web"));
        assert!(map["web"].is_http());
        assert_eq!(map["web"].timeout_secs, 5);
        assert!(!map.contains_key("off"));
        assert!(!map.contains_key("empty"));
    }

    #[test]
    fn bad_entries_are_skipped_not_fatal() {
        unsafe { std::env::remove_var("DEX_MCP_TEST_MISSING_2") };
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
mcp_servers:
  broken:
    command: foo
    env: {TOK: $DEX_MCP_TEST_MISSING_2}
  also_broken: 42
  good:
    command: bar
"#,
        )
        .unwrap();
        let map = parse_mcp_servers(&yaml);
        assert!(!map.contains_key("broken"));
        assert!(!map.contains_key("also_broken"));
        assert!(map.contains_key("good"));
    }

    #[test]
    fn kill_switch_disables_mcp() {
        let _env = TEST_ENV_LOCK.blocking_lock();
        let prev = std::env::var("DEX_MCP").ok();
        unsafe { std::env::set_var("DEX_MCP", "0") };
        assert!(!mcp_enabled());
        assert!(load_server_configs().is_empty());
        unsafe {
            match prev {
                Some(v) => std::env::set_var("DEX_MCP", v),
                None => std::env::remove_var("DEX_MCP"),
            }
        }
        assert!(mcp_enabled());
    }

    #[test]
    fn secrets_are_redacted() {
        let err = "mcp http 401: tools/call\nAuthorization: Bearer abc123\nx-api-key=zzz";
        let out = redact_secrets(err);
        assert!(!out.contains("abc123"), "{out}");
        assert!(!out.contains("zzz"), "{out}");
        assert!(out.contains("Authorization: [redacted]"), "{out}");
        assert!(out.contains("x-api-key=[redacted]"), "{out}");
        assert_eq!(redact_secrets("plain boom"), "plain boom");
    }

    #[test]
    fn redaction_is_non_ascii_safe() {
        // Multi-byte chars around the marker survive redaction untouched.
        let out = redact_line("café: authorization: Bearer sk-café123 rest");
        assert_eq!(out, "café: authorization: [redacted]");
        // `to_lowercase` expands some chars (`İ` -> i + U+0307) and desynced
        // the byte offsets applied to `line`; ASCII markers must stay aligned.
        let out = redact_line("İ: authorization: Bearer sk-secret");
        assert_eq!(out, "İ: authorization: [redacted]");
    }

    #[test]
    fn tool_names_are_clamped() {
        assert!(mcp_tool_name("srv", &"x".repeat(500)).len() <= MCP_TOOL_NAME_LIMIT);
    }

    #[test]
    fn allow_deny_gate_tools() {
        let cfg = McpServerConfig {
            deny: vec!["rm".to_string()],
            ..Default::default()
        };
        assert!(!cfg.tool_allowed("rm"));
        assert!(cfg.tool_allowed("ls"));
        // Deny wins over allow.
        let cfg = McpServerConfig {
            allow: vec!["ls".to_string()],
            deny: vec!["ls".to_string()],
            ..Default::default()
        };
        assert!(!cfg.tool_allowed("ls"));
        assert!(!cfg.tool_allowed("other"));
    }

    #[test]
    fn tool_definition_clamps_and_fixes_schema() {
        let t = McpTool {
            name: "search".to_string(),
            description: "x".repeat(2000),
            input_schema: Value::Null,
        };
        let def = t.to_definition("gh");
        assert_eq!(def.function.name, "mcp__gh__search");
        assert!(def.function.description.len() <= MCP_DESC_LIMIT + 3);
        assert!(def.function.parameters.is_object());
    }

    #[test]
    fn content_mapping_covers_shapes() {
        let v = serde_json::json!({"content": [
            {"type": "text", "text": "hi"},
            {"type": "image", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "f://a", "text": "body"}},
        ]});
        let text = content_to_text(&v);
        assert!(text.contains("hi"));
        assert!(text.contains("omitted"));
        assert!(text.contains("body"));
        let err =
            serde_json::json!({"content": [{"type": "text", "text": "nope"}], "isError": true});
        assert!(content_to_text(&err).starts_with("Error:"));
    }

    #[tokio::test]
    async fn manager_lists_calls_and_isolates_failures() {
        let mgr = Arc::new(McpManager::new(BTreeMap::new()));
        mgr.insert_client(
            "good",
            fake_client(
                vec![McpTool {
                    name: "search".to_string(),
                    description: "s".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
                true,
                false,
            ),
        )
        .await;
        mgr.insert_client("bad", fake_client(vec![], false, true))
            .await;
        mgr.rebuild_cache().await;
        let defs = mgr.tool_definitions().await;
        // 1 real tool + 1 synthetic resource reader from `good`; `bad` adds none.
        assert_eq!(defs.len(), 2);
        let mut args = serde_json::Map::new();
        let out = mgr
            .call_tool("mcp__good__search", &args, &NO_CANCEL)
            .await
            .unwrap();
        assert_eq!(out, "fake-ok");
        args.insert("uri".to_string(), Value::String("doc://a".to_string()));
        let res = mgr
            .call_tool("mcp__good_read_resource", &args, &NO_CANCEL)
            .await
            .unwrap();
        assert!(res.contains("hello"));
        // Unknown server reports down, never panics.
        assert!(mgr
            .call_tool("mcp__nope__x", &args, &NO_CANCEL)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn name_collisions_rename_instead_of_shadow() {
        let mgr = Arc::new(McpManager::new(BTreeMap::new()));
        for srv in ["one", "two"] {
            mgr.insert_client(srv, fake_client(vec![tool("same")], false, false))
                .await;
        }
        mgr.rebuild_cache().await;
        let defs = mgr.tool_definitions().await;
        assert_eq!(defs.len(), 2);
        let names: Vec<String> = defs.iter().map(|d| d.function.name.clone()).collect();
        // Both survive; the loser keeps the `mcp__` prefix with a `~2`
        // suffix and still dispatches through the cache.
        assert_eq!(names.iter().filter(|n| *n == "mcp__one__same").count(), 1);
        assert_eq!(names.iter().filter(|n| *n == "mcp__two__same").count(), 1);
        let args = serde_json::Map::new();
        for n in &names {
            assert_eq!(
                mgr.call_tool(n, &args, &NO_CANCEL).await.unwrap(),
                "fake-ok"
            );
        }
    }

    #[tokio::test]
    async fn allow_deny_filter_schema() {
        let mut configs = BTreeMap::new();
        configs.insert(
            "s".to_string(),
            McpServerConfig {
                deny: vec!["nope".to_string()],
                ..Default::default()
            },
        );
        let mgr = Arc::new(McpManager::new(configs));
        mgr.insert_client(
            "s",
            fake_client(vec![tool("ok"), tool("nope")], false, false),
        )
        .await;
        mgr.rebuild_cache().await;
        let defs = mgr.tool_definitions().await;
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "mcp__s__ok");
        let args = serde_json::Map::new();
        assert!(mgr
            .call_tool("mcp__s__nope", &args, &NO_CANCEL)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn schema_cap_drops_and_counts() {
        // Direct `enforce_cap` with an explicit max: no env mutation, so no
        // cross-test race on the shared env table (see TEST_ENV_LOCK).
        let mgr = McpManager::new(BTreeMap::new());
        let mut tools = vec![tool("b").to_definition("s"), tool("a").to_definition("s")];
        let mut names = HashMap::new();
        names.insert("mcp__s__a".to_string(), ("s".to_string(), "a".to_string()));
        names.insert("mcp__s__b".to_string(), ("s".to_string(), "b".to_string()));
        mgr.enforce_cap(&mut tools, &mut names, 1).await;
        // Sorted by name, head kept.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "mcp__s__a");
        assert_eq!(*mgr.cached_truncated.read().await, 1);
    }

    #[tokio::test]
    async fn reconnect_smoke_unknown_server_errors() {
        // No transport involved: an unknown name fails before any spawn.
        let mgr = McpManager::new(BTreeMap::new());
        let err = mgr.reconnect("nope").await.unwrap_err();
        assert!(err.contains("unknown mcp server"), "{err}");
    }

    #[tokio::test]
    async fn statuses_carry_down_reason() {
        let mut configs = BTreeMap::new();
        configs.insert(
            "bad".to_string(),
            McpServerConfig {
                command: Some("dex-definitely-missing-binary".to_string()),
                timeout_secs: 5,
                ..Default::default()
            },
        );
        let mgr = McpManager::new(configs);
        assert!(mgr.reconnect("bad").await.is_err());
        assert!(mgr.reconnect("ghost").await.is_err());
        let st = mgr.statuses().await;
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].state, "down");
        assert!(
            st[0].error.as_deref().unwrap_or("").contains("spawn"),
            "{:?}",
            st[0].error
        );
    }

    #[tokio::test]
    async fn sweep_drops_dead_clients() {
        let mgr = Arc::new(McpManager::new(BTreeMap::new()));
        mgr.insert_client("dead", fake_client(vec![], false, true))
            .await;
        mgr.sweep_once().await;
        assert!(!mgr.clients.read().await.contains_key("dead"));
        assert!(mgr.down.read().await.contains_key("dead"));
    }

    #[tokio::test]
    async fn statuses_report_down_without_clients() {
        let mut configs = BTreeMap::new();
        configs.insert("a".to_string(), McpServerConfig::default());
        let mgr = McpManager::new(configs);
        let st = mgr.statuses().await;
        assert_eq!(st.len(), 1);
        assert_eq!(st[0].state, "down");
    }
}
