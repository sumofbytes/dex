//! The per-server manager: connection lifecycle, schema cache, status.
//!
//! One [`McpManager`] owns every configured server: connect (batch refresh
//! or lazy), rebuild the merged tool cache under the schema cap, sweep dead
//! clients, dispatch `mcp__<server>__<tool>` calls. All cache reads are
//! sync-friendly via bounded `try_read` spins (see `spin_read`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::agent::state::CancellationSource;
use crate::protocol::{ServerStatus, ToolDefinition};

use super::client::McpClient;
use super::config::{
    def_belongs_to, insert_cached, load_server_configs, mcp_max_tools, split_mcp_name,
    McpServerConfig,
};
use super::mapping::{content_to_text, resource_reader_definition};
use super::redact::redact_secrets;
use super::transport::{HttpTransport, McpTransport, StdioTransport};

/// One server's fetched schema slice: raw defs plus the synthetic reader.
/// The caller merges (collision renames need the shared map, so merging
/// stays serial over a sorted server list).
struct ServerDefs {
    tools: Vec<(ToolDefinition, String)>,
    reader: Option<ToolDefinition>,
}

/// One server's connection state. A server is absent from the map until a
/// connect is attempted; from then on it is exactly `Up` or `Down`, never
/// both, so the two can't drift apart as two parallel maps could.
///
/// ```text
/// (absent) --connect ok--> Up --probe/ping fails--> Down(reason)
///     |                     ^                          |
///     +--connect fails--> Down <--connect ok-----------+   reconnect: -> (absent) -> connect
/// ```
#[derive(Clone)]
pub(super) enum Conn {
    Up(Arc<McpClient>),
    Down(String),
}

pub struct McpManager {
    pub(super) configs: BTreeMap<String, McpServerConfig>,
    pub(super) conns: RwLock<HashMap<String, Conn>>,
    /// Schema cache behind `Arc`: readers clone the `Arc`, never the defs
    /// (each `parameters: Value` re-serializes expensively — see
    /// `cached_schema_tokens`).
    pub(super) cached_tools: RwLock<Arc<[ToolDefinition]>>,
    pub(super) cached_names: RwLock<HashMap<String, (String, String)>>,
    pub(super) cached_truncated: RwLock<usize>,
    /// Token cost of `cached_tools`, precomputed at swap time: the per-turn
    /// budget reads this instead of re-running `schema_token_estimate`
    /// (`parameters.to_string()` per def) on every model call.
    pub(super) cached_schema_tokens: RwLock<u64>,
}

impl McpManager {
    pub fn new(configs: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            configs,
            conns: RwLock::new(HashMap::new()),
            cached_tools: RwLock::new(Arc::new([])),
            cached_names: RwLock::new(HashMap::new()),
            cached_truncated: RwLock::new(0),
            cached_schema_tokens: RwLock::new(0),
        }
    }

    pub fn from_env() -> Arc<Self> {
        Arc::new(Self::new(load_server_configs()))
    }

    /// Insert a pre-built client (tests / daemon bootstrap).
    #[cfg(test)]
    pub async fn insert_client(&self, server: &str, client: McpClient) {
        self.conns
            .write()
            .await
            .insert(server.to_string(), Conn::Up(Arc::new(client)));
    }

    /// The live client for `name`, if it is `Up`.
    async fn client(&self, name: &str) -> Option<Arc<McpClient>> {
        match self.conns.read().await.get(name) {
            Some(Conn::Up(c)) => Some(Arc::clone(c)),
            _ => None,
        }
    }

    /// Snapshot of every `Up` client.
    async fn up_clients(&self) -> HashMap<String, Arc<McpClient>> {
        self.conns
            .read()
            .await
            .iter()
            .filter_map(|(n, c)| match c {
                Conn::Up(c) => Some((n.clone(), Arc::clone(c))),
                Conn::Down(_) => None,
            })
            .collect()
    }

    async fn down_reason(&self, name: &str) -> String {
        match self.conns.read().await.get(name) {
            Some(Conn::Down(reason)) => reason.clone(),
            _ => "not connected".to_string(),
        }
    }

    async fn mark_down(&self, name: &str, reason: String) {
        self.conns
            .write()
            .await
            .insert(name.to_string(), Conn::Down(reason));
    }

    /// Connect all servers concurrently; one failure never fails the batch.
    pub async fn refresh(self: &Arc<Self>) {
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
        if self.client(name).await.is_some() {
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
                        self.conns
                            .write()
                            .await
                            .insert(name.to_string(), Conn::Up(client));
                    }
                    Err(e) => self.mark_down(name, redact_secrets(&e)).await,
                }
            }
            Err(e) => self.mark_down(name, e).await,
        }
    }

    pub(super) async fn rebuild_cache(&self) {
        let clients = self.up_clients().await;
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
    pub(super) async fn enforce_cap(
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
        let Some(client) = self.client(name).await else {
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
    pub async fn reconnect(&self, server: &str) -> Result<usize, String> {
        let cfg = self
            .configs
            .get(server)
            .cloned()
            .ok_or_else(|| format!("unknown mcp server '{server}'"))?;
        self.conns.write().await.remove(server);
        self.connect_and_cache(server, &cfg).await;
        if self.client(server).await.is_some() {
            let tools = self.cached_tools.read().await;
            Ok(tools
                .iter()
                .filter(|d| def_belongs_to(&d.function.name, server))
                .count())
        } else {
            Err(self.down_reason(server).await)
        }
    }

    /// Ping every client concurrently; drop the dead so they go `down`
    /// before the next turn. Runs every 60s on the global manager; never
    /// fails the batch.
    pub async fn sweep_once(&self) {
        let clients: Vec<(String, Arc<McpClient>)> = self.up_clients().await.into_iter().collect();
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
            let mut conns = self.conns.write().await;
            for name in &dead {
                conns.insert(
                    name.clone(),
                    Conn::Down("liveness probe failed; will reconnect on next use".to_string()),
                );
            }
        }
        self.rebuild_cache().await;
    }

    #[cfg(test)]
    pub async fn tool_definitions(&self) -> Arc<[ToolDefinition]> {
        self.cached_tools.read().await.clone()
    }

    pub async fn statuses(&self) -> Vec<ServerStatus> {
        let conns = self.conns.read().await;
        let tools = self.cached_tools.read().await;
        Self::status_list(&self.configs, &conns, &tools)
    }

    pub(super) fn status_list(
        configs: &BTreeMap<String, McpServerConfig>,
        conns: &HashMap<String, Conn>,
        tools: &[ToolDefinition],
    ) -> Vec<ServerStatus> {
        let mut out: Vec<ServerStatus> = configs
            .keys()
            .map(|name| {
                let count = tools
                    .iter()
                    .filter(|d| def_belongs_to(&d.function.name, name))
                    .count();
                let state = if matches!(conns.get(name), Some(Conn::Up(_))) {
                    "up"
                } else {
                    "down"
                };
                ServerStatus {
                    name: name.clone(),
                    state: state.to_string(),
                    tools: count,
                    error: match conns.get(name) {
                        Some(Conn::Down(reason)) => Some(reason.clone()),
                        _ => None,
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Dispatch `mcp__<server>__<tool>` (or the `_read_resource` reader).
    /// All errors pass through [`redact_secrets`] — one choke point so
    /// echoed headers/args never reach the model or logs.
    pub async fn call_tool(
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
        if self.client(&server).await.is_none() {
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
        let clients = self.client(&server).await;
        // Clone the client out of the map and drop the guard before any
        // `.await`: holding the read guard across the tool RPC (up to the
        // 30s timeout) would stall every map writer — reconnect, sweeper
        // drops, lazy-connect inserts — on a hung server. Read-shared
        // otherwise, so concurrent calls never block each other here.
        let Some(client) = clients else {
            let reason = self.down_reason(&server).await;
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
