//! MCP (Model Context Protocol) client: use external servers as tools.
//!
//! P0: `tools/list` + `tools/call` over stdio + Streamable HTTP, exposed as
//! `mcp__<server>__<tool>`. P1: `resources/list|read` + `prompts/list|get`
//! via one synthetic `mcp__<server>_read_resource` tool per server.
//!
//! Performance: one shared reqwest client, cached [`ToolDefinition`]s merged
//! synchronously into `tools_schema()` (never blocks the turn loop), fan-out
//! refresh via `JoinSet`, 30s per-call timeout, 32 KiB output clamp. One bad
//! server goes `down` and never breaks the turn.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::core::types::ToolDefinition;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// One entry under `mcp_servers:` in config.yaml.
#[derive(Clone, Debug, Default)]
pub(crate) struct McpServerConfig {
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) headers: BTreeMap<String, String>,
    pub(crate) timeout_secs: u64,
    pub(crate) disabled: bool,
}

impl McpServerConfig {
    pub(crate) fn is_http(&self) -> bool {
        self.url.is_some()
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub(crate) const MCP_OUTPUT_BYTES: usize = 32 * 1024;
pub(crate) const MCP_DESC_LIMIT: usize = 500;

/// `$VAR` / `${VAR}` expansion against the process environment.
pub(crate) fn expand_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'{' {
                if let Some(end) = raw[i + 2..].find('}') {
                    let key = &raw[i + 2..i + 2 + end];
                    out.push_str(&std::env::var(key).unwrap_or_default());
                    i += 3 + end;
                    continue;
                }
            } else {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > i + 1 {
                    out.push_str(&std::env::var(&raw[i + 1..j]).unwrap_or_default());
                    i = j;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Server names become part of a tool name: lowercase + `[a-z0-9_]` only.
pub(crate) fn sanitize_server_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let mut out: String = lower
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push_str("server");
    }
    out
}

/// `my-tool` -> `mcp__myserver__my_tool` (`__` in a tool id becomes `_`).
pub(crate) fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!(
        "mcp__{}__{}",
        sanitize_server_name(server),
        tool.replace("__", "_")
    )
}

/// Inverse of [`mcp_tool_name`]: `(server, tool)`. The synthetic resource
/// reader `mcp__<server>_read_resource` (single underscore) maps to
/// `(server, "\0resource")`. A real tool literally named `read_resource`
/// (`mcp__<server>__read_resource`) wins over the synthetic form.
pub(crate) fn split_mcp_name(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    // Real tools always use the double-underscore separator.
    if let Some(sep) = rest.find("__") {
        let (server, tool) = rest.split_at(sep);
        let tool = &tool[2..];
        if !server.is_empty() && !tool.is_empty() && !tool.contains("__") {
            return Some((server.to_string(), tool.to_string()));
        }
        return None;
    }
    // Synthetic reader uses a single underscore.
    if let Some(server) = rest.strip_suffix("_read_resource") {
        if !server.is_empty() && !server.contains("__") {
            return Some((server.to_string(), "\0resource".to_string()));
        }
    }
    None
}

fn yaml_str(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| v.as_str())
        .map(expand_env)
        .filter(|s| !s.is_empty())
}

fn yaml_str_list(map: &serde_yaml::Mapping, key: &str) -> Vec<String> {
    let Some(v) = map.get(serde_yaml::Value::String(key.to_string())) else {
        return Vec::new();
    };
    match v {
        serde_yaml::Value::Sequence(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(expand_env))
            .collect(),
        serde_yaml::Value::String(s) => vec![expand_env(s)],
        _ => Vec::new(),
    }
}

fn yaml_str_map(map: &serde_yaml::Mapping, key: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(serde_yaml::Value::Mapping(inner)) =
        map.get(serde_yaml::Value::String(key.to_string()))
    else {
        return out;
    };
    for (k, v) in inner {
        if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
            out.insert(k.to_string(), expand_env(v));
        }
    }
    out
}

fn parse_server_config(value: &serde_yaml::Value) -> Option<McpServerConfig> {
    // Shorthand: `github: "npx -y server"` means command + args.
    if let Some(s) = value.as_str() {
        let expanded = expand_env(s);
        let mut parts = expanded.split_whitespace();
        let command = parts.next()?.to_string();
        return Some(McpServerConfig {
            command: Some(command),
            args: parts.map(str::to_string).collect(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            ..Default::default()
        });
    }
    let map = value.as_mapping()?;
    let disabled = map
        .get(serde_yaml::Value::String("disabled".to_string()))
        .and_then(serde_yaml::Value::as_bool)
        .unwrap_or(false);
    let timeout_secs = map
        .get(serde_yaml::Value::String("timeout_secs".to_string()))
        .and_then(serde_yaml::Value::as_u64)
        .filter(|t| *t > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    // Shorthand: `github: "npx -y server"` means command + args.
    if let Some(s) = value.as_str() {
        let expanded = expand_env(s);
        let mut parts = expanded.split_whitespace();
        let command = parts.next()?.to_string();
        return Some(McpServerConfig {
            command: Some(command),
            args: parts.map(str::to_string).collect(),
            timeout_secs,
            disabled,
            ..Default::default()
        });
    }
    Some(McpServerConfig {
        command: yaml_str(map, "command"),
        args: yaml_str_list(map, "args"),
        env: yaml_str_map(map, "env"),
        cwd: yaml_str(map, "cwd"),
        url: yaml_str(map, "url"),
        headers: yaml_str_map(map, "headers"),
        timeout_secs,
        disabled,
    })
}

/// Parse the `mcp_servers:` mapping out of a config file value.
pub(crate) fn parse_mcp_servers(root: &serde_yaml::Value) -> BTreeMap<String, McpServerConfig> {
    let mut out = BTreeMap::new();
    let Some(map) = root.as_mapping() else {
        return out;
    };
    let Some(servers) = map
        .get(serde_yaml::Value::String("mcp_servers".to_string()))
        .and_then(|v| v.as_mapping())
    else {
        return out;
    };
    for (name, cfg) in servers {
        let Some(name) = name.as_str() else { continue };
        if name.contains("__") {
            continue; // `__` would collide with the tool-name separator.
        }
        let sanitized = sanitize_server_name(name);
        if let Some(cfg) = parse_server_config(cfg) {
            if !cfg.disabled && (cfg.command.is_some() || cfg.url.is_some()) {
                out.insert(sanitized, cfg);
            }
        }
    }
    out
}

fn config_file_value() -> Option<serde_yaml::Value> {
    let path = if let Some(p) = std::env::var_os("DEX_CONFIG") {
        std::path::PathBuf::from(p)
    } else {
        let dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
            })?;
        dir.join("dex/config.yaml")
    };
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&text).ok()
}

/// Load server configs from config.yaml (`DEX_MCP_SERVERS_JSON` wins for tests).
pub(crate) fn load_server_configs() -> BTreeMap<String, McpServerConfig> {
    if let Ok(json) = std::env::var("DEX_MCP_SERVERS_JSON") {
        if !json.is_empty() {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&json) {
                let mut out = BTreeMap::new();
                for (name, cfg) in &map {
                    let yaml: serde_yaml::Value =
                        serde_yaml::from_str(&cfg.to_string()).unwrap_or(serde_yaml::Value::Null);
                    if let Some(c) = parse_server_config(&yaml) {
                        if !c.disabled && (c.command.is_some() || c.url.is_some()) {
                            out.insert(sanitize_server_name(name), c);
                        }
                    }
                }
                return out;
            }
        }
    }
    config_file_value()
        .as_ref()
        .map(parse_mcp_servers)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// MCP types: tools, content mapping
// ---------------------------------------------------------------------------

/// A tool advertised by an MCP server (before namespacing).
#[derive(Clone, Debug)]
pub(crate) struct McpTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

impl McpTool {
    pub(crate) fn to_definition(&self, server: &str) -> ToolDefinition {
        let mut desc = format!("[{}] {}", server, self.description.trim());
        if desc.len() > MCP_DESC_LIMIT {
            desc.truncate(MCP_DESC_LIMIT);
            desc.push('…');
        }
        let mut params = self.input_schema.clone();
        if !params.is_object() {
            params = serde_json::json!({"type": "object"});
        }
        ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::core::types::FunctionDef {
                name: mcp_tool_name(server, &self.name),
                description: desc,
                parameters: params,
            },
        }
    }
}

/// Synthetic P1 reader: one per server, backed by `resources/*`.
pub(crate) fn resource_reader_definition(server: &str) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: crate::core::types::FunctionDef {
            name: format!("mcp__{server}_read_resource"),
            description: format!(
                "[{server}] Read a resource served by this MCP server (file, doc, schema). Prefer this over shelling out when the server hosts the data."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "uri": { "type": "string", "description": "resource URI from resources/list" } },
                "required": ["uri"]
            }),
        },
    }
}

/// Map a `tools/call` result's `content[]` to display text. Text wins;
/// images/resources degrade to placeholders so the model still sees shape.
pub(crate) fn content_to_text(result: &Value) -> String {
    let mut parts = Vec::new();
    let empty = Vec::new();
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    for item in content {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            "image" | "audio" => {
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("data");
                parts.push(format!("[{kind} omitted: {mime}]"));
            }
            "resource" => {
                let uri = item
                    .get("resource")
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                if let Some(text) = item
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                {
                    parts.push(format!("[resource {uri}]\n{text}"));
                } else {
                    parts.push(format!("[resource omitted: {uri}]"));
                }
            }
            _ => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
        }
    }
    let mut text = parts.join("\n");
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !text.is_empty()
    {
        text = format!("Error: {text}");
    }
    if text.len() > MCP_OUTPUT_BYTES {
        text.truncate(MCP_OUTPUT_BYTES);
        text.push_str("\n[truncated]");
    }
    text
}

// ---------------------------------------------------------------------------
// JSON-RPC + transports
// ---------------------------------------------------------------------------

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Minimal transport surface the manager needs. Real transports speak
/// JSON-RPC 2.0; tests inject fakes. Boxed future (not `async_trait`, which
/// would add a dependency) keeps the trait object-safe on edition 2021.
pub(crate) trait McpTransport: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>;
}

fn shared_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Newline-delimited JSON-RPC over a child process's stdio.
///
/// Requests are serialized through one lock pair (MCP calls are infrequent;
/// one in flight per server is plenty), so no response demux table is
/// needed — write a line, read the reply line, match the id.
pub(crate) struct StdioTransport {
    stdin: Mutex<tokio::process::ChildStdin>,
    stdout: Mutex<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: AtomicU64,
    _child: Mutex<tokio::process::Child>,
}

#[cfg(unix)]
unsafe extern "C" {
    fn setsid() -> i32;
}

impl StdioTransport {
    pub(crate) async fn spawn(cfg: &McpServerConfig) -> Result<Self, String> {
        let command = cfg.command.clone().ok_or("missing command")?;
        let mut cmd = tokio::process::Command::new(&command);
        cmd.args(&cfg.args).envs(&cfg.env);
        if let Some(cwd) = &cfg.cwd {
            cmd.current_dir(cwd);
        }
        // Detach from the user's terminal; never inherit stdio.
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            // New session like tool children: no controlling tty.
            cmd.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn {command}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let transport = Self {
            stdin: Mutex::new(stdin),
            stdout: Mutex::new(tokio::io::BufReader::new(stdout)),
            next_id: AtomicU64::new(1),
            _child: Mutex::new(child),
        };
        transport.initialize().await?;
        Ok(transport)
    }

    async fn initialize(&self) -> Result<(), String> {
        let result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "dex", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await?;
        let _ = result;
        // Fire-and-forget per spec; the server must not reply.
        let notif = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let mut stdin = self.stdin.lock().await;
        use tokio::io::AsyncWriteExt as _;
        let mut line = notif.to_string();
        line.push('\n');
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl McpTransport for StdioTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(async move {
            use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            // Lock ordering is fixed (stdin then stdout) so concurrent callers
            // serialize instead of interleaving lines on the pipe.
            let mut stdin = self.stdin.lock().await;
            let mut stdout = self.stdout.lock().await;
            let mut line = rpc_request(id, method, params).to_string();
            line.push('\n');
            stdin
                .write_all(line.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            stdin.flush().await.map_err(|e| e.to_string())?;
            // Match our id: servers may interleave notifications and
            // server->client requests at any time, and one stray line
            // would otherwise desync every later call by one reply.
            // Bounded so a chatty server can't spin us.
            for _ in 0..32 {
                let mut reply = String::new();
                tokio::time::timeout(
                    Duration::from_secs(DEFAULT_TIMEOUT_SECS),
                    stdout.read_line(&mut reply),
                )
                .await
                .map_err(|_| format!("mcp request {method} timed out"))?
                .map_err(|e| e.to_string())?;
                if reply.trim().is_empty() {
                    return Err("mcp server exited".to_string());
                }
                let v: Value = serde_json::from_str(reply.trim()).map_err(|e| e.to_string())?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    return extract_rpc_result(&v).ok_or("mcp: bad response".to_string());
                }
            }
            Err("mcp: too many interleaved messages".to_string())
        })
    }
}

/// Streamable HTTP: POST JSON-RPC, accept `application/json` or SSE stream.
pub(crate) struct HttpTransport {
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
}

impl HttpTransport {
    pub(crate) fn new(cfg: &McpServerConfig) -> Self {
        Self {
            url: cfg.url.clone().unwrap_or_default(),
            headers: cfg.headers.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
        }
    }
}

impl McpTransport for HttpTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + Send + 'a>>
    {
        Box::pin(async move { self.request_inner(method, params).await })
    }
}

impl HttpTransport {
    async fn request_inner(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = rpc_request(1, method, params);
        let mut req = shared_http_client()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .timeout(self.timeout);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.json(&body).send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("mcp http {}: {}", resp.status(), method));
        }
        let text = resp.text().await.map_err(|e| e.to_string())?;
        // Plain JSON wins; otherwise scan SSE `data:` lines for the reply.
        if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
            if v.is_object() {
                if let Some(r) = extract_rpc_result(&v) {
                    return Ok(r);
                }
            }
        }
        for line in text.lines() {
            let data = line.trim().strip_prefix("data:").unwrap_or("").trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data) {
                if let Some(r) = extract_rpc_result(&v) {
                    return Ok(r);
                }
            }
        }
        Err("mcp: no result in http response".to_string())
    }
}

fn extract_rpc_result(v: &Value) -> Option<Value> {
    if let Some(err) = v.get("error") {
        return Some(serde_json::json!({"__mcp_error": err}));
    }
    v.get("result").cloned()
}

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

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        tokio::time::timeout(self.timeout, self.transport.request(method, params))
            .await
            .map_err(|_| format!("mcp {method} timed out"))?
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<McpTool>, String> {
        let v = self.call("tools/list", serde_json::json!({})).await?;
        if let Some(err) = v.get("__mcp_error") {
            return Err(format!("tools/list: {err}"));
        }
        let mut out = Vec::new();
        for t in v
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
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

    pub(crate) async fn call_tool(&self, tool: &str, args: &Value) -> Result<Value, String> {
        let v = self
            .call(
                "tools/call",
                serde_json::json!({"name": tool, "arguments": args}),
            )
            .await?;
        if let Some(err) = v.get("__mcp_error") {
            return Err(format!("{err}"));
        }
        Ok(v)
    }

    pub(crate) async fn has_resources(&self) -> bool {
        self.call("resources/list", serde_json::json!({}))
            .await
            .map(|v| v.get("resources").and_then(Value::as_array).is_some())
            .unwrap_or(false)
    }

    pub(crate) async fn read_resource(&self, uri: &str) -> Result<String, String> {
        let v = self
            .call("resources/read", serde_json::json!({"uri": uri}))
            .await?;
        if let Some(err) = v.get("__mcp_error") {
            return Err(format!("{err}"));
        }
        // `resources/read` returns `contents[]` with inline `text`/`blob`
        // (no `type` discriminator), unlike `tools/call` `content[]`.
        let mut parts = Vec::new();
        for item in v
            .get("contents")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
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
        if text.len() > MCP_OUTPUT_BYTES {
            text.truncate(MCP_OUTPUT_BYTES);
            text.push_str("\n[truncated]");
        }
        Ok(text)
    }
}

pub(crate) struct ServerStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) tools: usize,
}

pub(crate) struct McpManager {
    configs: BTreeMap<String, McpServerConfig>,
    clients: RwLock<HashMap<String, Arc<McpClient>>>,
    down: RwLock<HashMap<String, String>>,
    cached_tools: RwLock<Vec<ToolDefinition>>,
    cached_names: RwLock<HashMap<String, (String, String)>>,
}

impl McpManager {
    pub(crate) fn new(configs: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            configs,
            clients: RwLock::new(HashMap::new()),
            down: RwLock::new(HashMap::new()),
            cached_tools: RwLock::new(Vec::new()),
            cached_names: RwLock::new(HashMap::new()),
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
            Ok(Box::new(HttpTransport::new(cfg)))
        } else {
            match StdioTransport::spawn(cfg).await {
                Ok(t) => Ok(Box::new(t)),
                Err(e) => Err(e),
            }
        };
        match transport {
            Ok(t) => {
                let client = Arc::new(McpClient::new(t, cfg.timeout_secs));
                // Probe with tools/list so a dead server goes `down` now,
                // not mid-turn.
                match client.list_tools().await {
                    Ok(_) => {
                        self.clients.write().await.insert(name.to_string(), client);
                        self.down.write().await.remove(name);
                    }
                    Err(e) => {
                        self.down.write().await.insert(name.to_string(), e);
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
        let mut tools = Vec::new();
        let mut names = HashMap::new();
        for (server, client) in &clients {
            Self::cache_server_into(server, client, &mut tools, &mut names).await;
        }
        *self.cached_tools.write().await = tools;
        *self.cached_names.write().await = names;
    }

    /// List one server's tools (+ resource reader) into the given sinks.
    async fn cache_server_into(
        server: &str,
        client: &Arc<McpClient>,
        tools: &mut Vec<ToolDefinition>,
        names: &mut HashMap<String, (String, String)>,
    ) {
        let listed = client.list_tools().await.unwrap_or_default();
        for t in &listed {
            let def = t.to_definition(server);
            names.insert(
                def.function.name.clone(),
                (server.to_string(), t.name.clone()),
            );
            tools.push(def);
        }
        if client.has_resources().await {
            let def = resource_reader_definition(server);
            names.insert(
                def.function.name.clone(),
                (server.to_string(), "\0resource".to_string()),
            );
            tools.push(def);
        }
    }

    /// Connect one server and merge its tools into the live cache (the batch
    /// `refresh` path rebuilds wholesale; this keeps a lazy connect cheap).
    async fn connect_and_cache(&self, name: &str, cfg: &McpServerConfig) {
        self.connect_one(name, cfg).await;
        let clients = self.clients.read().await.clone();
        let Some(client) = clients.get(name) else {
            return;
        };
        let mut tools = self.cached_tools.write().await;
        let mut names = self.cached_names.write().await;
        tools.retain(|d| {
            d.function.name != format!("mcp__{name}_read_resource")
                && !d.function.name.starts_with(&format!("mcp__{name}__"))
        });
        names.retain(|_, (server, _)| server != name);
        Self::cache_server_into(name, client, &mut tools, &mut names).await;
    }

    #[cfg(test)]
    pub(crate) async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.cached_tools.read().await.clone()
    }

    pub(crate) async fn statuses(&self) -> Vec<ServerStatus> {
        let clients = self.clients.read().await;
        let tools = self.cached_tools.read().await;
        let mut out: Vec<ServerStatus> = self
            .configs
            .keys()
            .map(|name| {
                let prefix = format!("mcp__{name}");
                let count = tools
                    .iter()
                    .filter(|d| {
                        d.function.name == format!("{prefix}_read_resource")
                            || d.function.name.starts_with(&format!("{prefix}__"))
                    })
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
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Dispatch `mcp__<server>__<tool>` (or the `_read_resource` reader).
    pub(crate) async fn call_tool(
        &self,
        full_name: &str,
        args: &serde_json::Map<String, Value>,
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
        // Resolve against the cache so renames/fakes stay consistent; fall
        // back to the split name for lazily-connected servers.
        let (server, tool) = self
            .cached_names
            .read()
            .await
            .get(full_name)
            .cloned()
            .unwrap_or((server, tool));
        let clients = self.clients.read().await;
        let Some(client) = clients.get(&server) else {
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
            return client.read_resource(uri).await;
        }
        let result = client
            .call_tool(&tool, &Value::Object(args.clone()))
            .await?;
        Ok(content_to_text(&result))
    }
}

// ---------------------------------------------------------------------------
// Global (process-wide) manager: sync reads for schema + approval paths
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<Arc<McpManager>> = OnceLock::new();

pub(crate) fn global_manager() -> Arc<McpManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = McpManager::from_env();
            // Best-effort background connect; schema merges whatever is cached.
            let clone = Arc::clone(&mgr);
            crate::client::http::spawn_task(async move { clone.refresh().await });
            mgr
        })
        .clone()
}

/// Cached MCP tools for `tools_schema()` — never blocks, never fails.
pub(crate) fn cached_tools() -> Vec<ToolDefinition> {
    GLOBAL
        .get()
        .and_then(|m| m.cached_tools.try_read().ok().map(|t| t.clone()))
        .unwrap_or_default()
}

pub(crate) async fn call_global(
    name: &str,
    args: &serde_json::Map<String, Value>,
) -> Result<String, String> {
    global_manager().call_tool(name, args).await
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
        assert_eq!(expand_env("$DEX_MCP_TEST_X/world"), "hello/world");
        assert_eq!(expand_env("${DEX_MCP_TEST_X}!"), "hello!");
        assert_eq!(expand_env("$DEX_MCP_TEST_MISSING!"), "!");
    }

    #[test]
    fn config_parses_stdio_and_http() {
        let yaml: serde_yaml::Value = serde_yaml::from_str(
            r#"
mcp_servers:
  gh:
    command: npx
    args: ["-y", "server"]
    env: {TOK: $DEX_MCP_TEST_X}
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
        assert!(map.contains_key("web"));
        assert!(map["web"].is_http());
        assert_eq!(map["web"].timeout_secs, 5);
        assert!(!map.contains_key("off"));
        assert!(!map.contains_key("empty"));
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
        let out = mgr.call_tool("mcp__good__search", &args).await.unwrap();
        assert_eq!(out, "fake-ok");
        args.insert("uri".to_string(), Value::String("doc://a".to_string()));
        let res = mgr
            .call_tool("mcp__good_read_resource", &args)
            .await
            .unwrap();
        assert!(res.contains("hello"));
        // Unknown server reports down, never panics.
        assert!(mgr.call_tool("mcp__nope__x", &args).await.is_err());
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
