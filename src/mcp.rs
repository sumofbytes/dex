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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Mutex, RwLock};

use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::core::types::ToolDefinition;

pub(crate) mod oauth;

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
    /// Tool allowlist: when non-empty, only these server-side tool names are
    /// exposed. Denylist wins over allowlist.
    pub(crate) allow: Vec<String>,
    /// Tool denylist: these server-side tool names are never exposed.
    pub(crate) deny: Vec<String>,
    /// Pre-registered OAuth client (`dex mcp login` skips dynamic
    /// registration when set). Refresh reuses the saved client otherwise.
    pub(crate) oauth_client_id: Option<String>,
    pub(crate) oauth_client_secret: Option<String>,
    /// OAuth scope for the authorize request (metadata default otherwise).
    pub(crate) oauth_scope: Option<String>,
}

impl McpServerConfig {
    pub(crate) fn is_http(&self) -> bool {
        self.url.is_some()
    }

    /// Allowlist/denylist gate on the server-side (pre-namespace) tool name.
    pub(crate) fn tool_allowed(&self, tool: &str) -> bool {
        if self.deny.iter().any(|d| d == tool) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|a| a == tool)
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub(crate) const MCP_OUTPUT_BYTES: usize = 32 * 1024;
pub(crate) const MCP_DESC_LIMIT: usize = 500;
/// Namespaced tool names are clamped so a hostile server cannot push a
/// multi-KB name into the schema on every request.
pub(crate) const MCP_TOOL_NAME_LIMIT: usize = 128;

/// Kill switch: `DEX_MCP=0|off|false|no` (or `DEX_NO_MCP=1`) disables every
/// MCP server. Default on.
pub(crate) fn mcp_enabled() -> bool {
    match std::env::var("DEX_MCP")
        .unwrap_or_default()
        .trim()
        .to_lowercase()
        .as_str()
    {
        "0" | "off" | "false" | "no" => false,
        _ => std::env::var("DEX_NO_MCP")
            .map(|v| v != "1")
            .unwrap_or(true),
    }
}

/// Schema cap: `DEX_MCP_MAX_TOOLS`, default 200.
pub(crate) fn mcp_max_tools() -> usize {
    std::env::var("DEX_MCP_MAX_TOOLS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
}

/// `$VAR` / `${VAR}` expansion against the process environment.
/// Missing variables are an error (fail-closed): silently substituting `""`
/// would turn a missing API key into an unauthenticated request.
pub(crate) fn expand_env(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'{' {
                if let Some(end) = raw[i + 2..].find('}') {
                    let key = &raw[i + 2..i + 2 + end];
                    match std::env::var(key) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            return Err(format!("mcp config: env var ${{{key}}} is not set"));
                        }
                    }
                    i += 3 + end;
                    continue;
                }
            } else {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    j += 1;
                }
                if j > i + 1 {
                    let key = &raw[i + 1..j];
                    match std::env::var(key) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            return Err(format!("mcp config: env var ${key} is not set"));
                        }
                    }
                    i = j;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Ok(out)
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
/// Clamped to [`MCP_TOOL_NAME_LIMIT`] chars so a hostile server cannot push
/// a multi-KB name into the schema on every request.
pub(crate) fn mcp_tool_name(server: &str, tool: &str) -> String {
    let mut name = format!(
        "mcp__{}__{}",
        sanitize_server_name(server),
        tool.replace("__", "_")
    );
    if name.len() > MCP_TOOL_NAME_LIMIT {
        name.truncate(MCP_TOOL_NAME_LIMIT);
    }
    name
}

/// Insert a namespaced tool into the cache, renaming on collision
/// (`…__tool`, `…__tool~2`, …). Two servers (or one hostile server)
/// advertising the same name must not silently shadow each other.
fn insert_cached(
    tools: &mut Vec<ToolDefinition>,
    names: &mut HashMap<String, (String, String)>,
    mut def: ToolDefinition,
    server: &str,
    tool: &str,
) {
    let mut candidate = def.function.name.clone();
    let mut n = 2;
    while names.contains_key(&candidate) {
        candidate = format!("{}~{n}", def.function.name);
        n += 1;
    }
    // `~` keeps the `mcp__` prefix (dispatch still routes) while staying
    // out of the `__` separator grammar; resolution is cache-first so the
    // suffix never reaches the server.
    if candidate != def.function.name {
        def.function.name = candidate.clone();
        def.function.description = format!("{} [renamed: collision]", def.function.description);
    }
    names.insert(candidate, (server.to_string(), tool.to_string()));
    tools.push(def);
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

fn yaml_str(map: &serde_yaml::Mapping, key: &str) -> Result<Option<String>, String> {
    map.get(serde_yaml::Value::String(key.to_string()))
        .and_then(|v| v.as_str())
        .map(|s| expand_env(s).map(|e| (!e.is_empty()).then_some(e)))
        .transpose()
        .map(|o| o.flatten())
}

fn yaml_str_list(map: &serde_yaml::Mapping, key: &str) -> Result<Vec<String>, String> {
    let Some(v) = map.get(serde_yaml::Value::String(key.to_string())) else {
        return Ok(Vec::new());
    };
    match v {
        serde_yaml::Value::Sequence(items) => items
            .iter()
            .filter_map(|i| i.as_str())
            .map(expand_env)
            .collect(),
        serde_yaml::Value::String(s) => Ok(vec![expand_env(s)?]),
        _ => Ok(Vec::new()),
    }
}

fn yaml_str_map(map: &serde_yaml::Mapping, key: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let Some(serde_yaml::Value::Mapping(inner)) =
        map.get(serde_yaml::Value::String(key.to_string()))
    else {
        return Ok(out);
    };
    for (k, v) in inner {
        if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
            out.insert(k.to_string(), expand_env(v)?);
        }
    }
    Ok(out)
}

fn parse_server_config(value: &serde_yaml::Value) -> Result<McpServerConfig, String> {
    // Shorthand: `github: "npx -y server"` means command + args.
    if let Some(s) = value.as_str() {
        let expanded = expand_env(s)?;
        let mut parts = expanded.split_whitespace();
        let Some(command) = parts.next().map(str::to_string) else {
            return Err("mcp config: empty command shorthand".to_string());
        };
        return Ok(McpServerConfig {
            command: Some(command),
            args: parts.map(str::to_string).collect(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            ..Default::default()
        });
    }
    let Some(map) = value.as_mapping() else {
        return Err("mcp config: server entry must be a string or mapping".to_string());
    };
    let disabled = map
        .get(serde_yaml::Value::String("disabled".to_string()))
        .and_then(serde_yaml::Value::as_bool)
        .unwrap_or(false);
    let timeout_secs = map
        .get(serde_yaml::Value::String("timeout_secs".to_string()))
        .and_then(serde_yaml::Value::as_u64)
        .filter(|t| *t > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Ok(McpServerConfig {
        command: yaml_str(map, "command")?,
        args: yaml_str_list(map, "args")?,
        env: yaml_str_map(map, "env")?,
        cwd: yaml_str(map, "cwd")?,
        url: yaml_str(map, "url")?,
        headers: yaml_str_map(map, "headers")?,
        timeout_secs,
        disabled,
        allow: yaml_str_list(map, "allow")?,
        deny: yaml_str_list(map, "deny")?,
        oauth_client_id: yaml_str(map, "oauth_client_id")?,
        oauth_client_secret: yaml_str(map, "oauth_client_secret")?,
        oauth_scope: yaml_str(map, "oauth_scope")?,
    })
}

/// A config is active when not disabled and actually runnable (a
/// http+command entry is stdio — stdio wins, matching `connect_one`).
fn active_config(cfg: &McpServerConfig) -> bool {
    !cfg.disabled && (cfg.command.is_some() || cfg.url.is_some())
}

/// Parse the `mcp_servers:` mapping out of a config file value. Entries that
/// fail (unset env var, wrong shape) are skipped with a stderr warning — one
/// bad entry must never break the whole config.
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
            // `__` would collide with the tool-name separator.
            eprintln!("dex: mcp server '{name}' ignored: '__' is reserved");
            continue;
        }
        let sanitized = sanitize_server_name(name);
        match parse_server_config(cfg) {
            Ok(cfg) if active_config(&cfg) => {
                out.insert(sanitized, cfg);
            }
            Ok(_) => {}
            Err(e) => eprintln!("dex: mcp server '{sanitized}' ignored: {e}"),
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
    if !mcp_enabled() {
        return BTreeMap::new();
    }
    if let Ok(json) = std::env::var("DEX_MCP_SERVERS_JSON") {
        if !json.is_empty() {
            if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&json) {
                let mut out = BTreeMap::new();
                for (name, cfg) in &map {
                    if name.contains("__") {
                        eprintln!("dex: mcp server '{name}' ignored: '__' is reserved");
                        continue;
                    }
                    let yaml: serde_yaml::Value =
                        serde_yaml::from_str(&cfg.to_string()).unwrap_or(serde_yaml::Value::Null);
                    match parse_server_config(&yaml) {
                        Ok(c) if active_config(&c) => {
                            out.insert(sanitize_server_name(name), c);
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("dex: mcp server '{name}' ignored: {e}"),
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
// Error hygiene: servers and proxies echo our headers/args back in failures.
// Redact `key: value` / `key=value` secrets at the manager boundary so they
// never reach turn output, logs, or the model.
// ---------------------------------------------------------------------------

const SECRET_MARKERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "bearer",
    "api-key",
    "apikey",
    "secret",
    "passwd",
    "password",
    "cookie",
    "set-cookie",
];

/// Scrub secret values from an error string, keeping `Key: [redacted]` shape.
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

fn redact_line(line: &str) -> String {
    let lower = line.to_lowercase();
    let mut start: Option<usize> = None;
    for marker in SECRET_MARKERS {
        let mut search = 0;
        while let Some(rel) = lower[search..].find(marker) {
            let abs = search + rel;
            search = abs + marker.len();
            let mut rest = abs + marker.len();
            while matches!(line.as_bytes().get(rest), Some(b' ' | b'\t' | b'\r')) {
                rest += 1;
            }
            // `Authorization: Bearer x`, `api_key=abc`, or a bare `Bearer x`.
            let is_bearer = *marker == "bearer"
                && line
                    .as_bytes()
                    .get(rest)
                    .is_some_and(|b| !b.is_ascii_whitespace());
            if matches!(line.as_bytes().get(rest), Some(b':') | Some(b'=')) || is_bearer {
                if !is_bearer {
                    rest += 1;
                    while matches!(line.as_bytes().get(rest), Some(b' ' | b'\t' | b'\r')) {
                        rest += 1;
                    }
                }
                start = Some(start.map_or(rest, |prev: usize| prev.min(rest)));
                break;
            }
        }
    }
    match start {
        Some(s) => {
            let end = line.find('\n').unwrap_or(line.len());
            format!("{}[redacted]{}", &line[..s], &line[end..])
        }
        None => line.to_string(),
    }
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
            .user_agent(crate::client::http::USER_AGENT)
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

/// Transport-level failure: `session_gone` means the server forgot our
/// Streamable HTTP session (restart) and the call is worth one re-handshake;
/// `unauthorized` means a 401 worth one token refresh before surfacing.
struct HttpError {
    msg: String,
    session_gone: bool,
    unauthorized: bool,
}

/// Streamable HTTP: POST JSON-RPC, accept `application/json` or SSE stream.
/// A stored OAuth token (`dex mcp login <server>`) is injected per request
/// unless the config already sets `authorization` — so login/logout take
/// effect without a reconnect.
pub(crate) struct HttpTransport {
    server: String,
    url: String,
    headers: BTreeMap<String, String>,
    timeout: Duration,
    session: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpTransport {
    pub(crate) fn new(server: &str, cfg: &McpServerConfig) -> Self {
        Self {
            server: server.to_string(),
            url: cfg.url.clone().unwrap_or_default(),
            headers: cfg.headers.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs.max(1)),
            session: Mutex::new(None),
            next_id: AtomicU64::new(1),
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
        match self.roundtrip(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if e.session_gone => {
                // Server restarted and forgot the session: drop it,
                // re-handshake per the Streamable HTTP spec, retry once.
                *self.session.lock().await = None;
                let _ = self
                    .roundtrip(
                        "initialize",
                        serde_json::json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "clientInfo": {"name": "dex", "version": env!("CARGO_PKG_VERSION")},
                        }),
                    )
                    .await;
                self.roundtrip(method, params).await.map_err(|e| e.msg)
            }
            Err(e) if e.unauthorized => {
                // One silent refresh when the stored token is refreshable,
                // then a single retry. A dead refresh token (`invalid_grant`)
                // is dropped so later calls fail fast with the login hint.
                // Transient failures back off 60s so a down AS is not hammered
                // on every tool call.
                if let Some(saved) = oauth::load_token(&self.server) {
                    if saved.refreshable() && !oauth::refresh_backoff_active(&self.server) {
                        match oauth::refresh_access_token(&saved).await {
                            Ok(fresh) => {
                                let _ = oauth::save_token(&self.server, &fresh);
                                oauth::clear_refresh_backoff(&self.server);
                                return self.roundtrip(method, params).await.map_err(|e| e.msg);
                            }
                            Err(e) if e.contains("invalid_grant") => {
                                let _ = oauth::clear_token(&self.server);
                            }
                            Err(_) => {
                                oauth::note_refresh_failure(&self.server);
                            }
                        }
                    }
                }
                Err(e.msg)
            }
            Err(e) => Err(e.msg),
        }
    }

    async fn roundtrip(&self, method: &str, params: Value) -> Result<Value, HttpError> {
        let fail = |msg: String| HttpError {
            msg,
            session_gone: false,
            unauthorized: false,
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = rpc_request(id, method, params);
        let mut req = shared_http_client()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .timeout(self.timeout);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        if !self
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            if let Some(tok) = oauth::valid_token(&self.server) {
                req = req.header("authorization", format!("Bearer {}", tok.access_token));
            }
        }
        let had_session = self.session.lock().await.clone();
        if let Some(s) = had_session.clone() {
            req = req.header("mcp-session-id", s);
        }
        let resp = req
            .json(&body)
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        if let Some(s) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().await = Some(s.to_string());
        }
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND && had_session.is_some() {
            return Err(HttpError {
                msg: format!("mcp http 404 (session expired): {method}"),
                session_gone: true,
                unauthorized: false,
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            let bearer = resp
                .headers()
                .get("www-authenticate")
                .and_then(|v| v.to_str().ok())
                .is_some_and(oauth::is_bearer_challenge);
            let mut msg = format!("mcp http {status}: {method}");
            if bearer {
                msg.push_str(&format!(
                    " (OAuth required — run 'dex mcp login {}')",
                    self.server
                ));
            }
            return Err(HttpError {
                msg,
                session_gone: false,
                unauthorized: true,
            });
        }
        if !status.is_success() {
            return Err(fail(format!("mcp http {status}: {method}")));
        }
        let text = resp.text().await.map_err(|e| fail(e.to_string()))?;
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
        Err(fail("mcp: no result in http response".to_string()))
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
        if let Some(err) = v.get("__mcp_error") {
            return Err(format!("{err}"));
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
    pub(crate) error: Option<String>,
}

pub(crate) struct McpManager {
    configs: BTreeMap<String, McpServerConfig>,
    clients: RwLock<HashMap<String, Arc<McpClient>>>,
    down: RwLock<HashMap<String, String>>,
    cached_tools: RwLock<Vec<ToolDefinition>>,
    cached_names: RwLock<HashMap<String, (String, String)>>,
    cached_truncated: RwLock<usize>,
}

impl McpManager {
    pub(crate) fn new(configs: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            configs,
            clients: RwLock::new(HashMap::new()),
            down: RwLock::new(HashMap::new()),
            cached_tools: RwLock::new(Vec::new()),
            cached_names: RwLock::new(HashMap::new()),
            cached_truncated: RwLock::new(0),
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
        let mut tools = Vec::new();
        let mut names = HashMap::new();
        for (server, client) in &clients {
            // Test clients have no config entry: default allows everything.
            let cfg = self.configs.get(server).cloned().unwrap_or_default();
            Self::cache_server_into(server, &cfg, client, &mut tools, &mut names).await;
        }
        self.enforce_cap(&mut tools, &mut names, mcp_max_tools())
            .await;
        *self.cached_tools.write().await = tools;
        *self.cached_names.write().await = names;
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

    /// List one server's tools (+ resource reader) into the given sinks,
    /// honoring the server's allow/deny filter.
    async fn cache_server_into(
        server: &str,
        cfg: &McpServerConfig,
        client: &Arc<McpClient>,
        tools: &mut Vec<ToolDefinition>,
        names: &mut HashMap<String, (String, String)>,
    ) {
        let listed = client.list_tools(None).await.unwrap_or_default();
        for tool in &listed {
            if !cfg.tool_allowed(&tool.name) {
                continue;
            }
            insert_cached(tools, names, tool.to_definition(server), server, &tool.name);
        }
        if client.has_resources(None).await {
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
        Self::cache_server_into(name, cfg, client, &mut tools, &mut names).await;
        self.enforce_cap(&mut tools, &mut names, mcp_max_tools())
            .await;
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
            let prefix = format!("mcp__{server}");
            Ok(tools
                .iter()
                .filter(|d| {
                    d.function.name == format!("{prefix}_read_resource")
                        || d.function.name.starts_with(&format!("{prefix}__"))
                })
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

    /// Ping every client; drop the dead so they go `down` before the next
    /// turn. Runs every 60s on the global manager; never fails the batch.
    pub(crate) async fn sweep_once(&self) {
        let clients: Vec<(String, Arc<McpClient>)> =
            self.clients.read().await.clone().into_iter().collect();
        let mut changed = false;
        for (name, client) in &clients {
            if client.ping().await.is_err() {
                self.clients.write().await.remove(name);
                self.down.write().await.insert(
                    name.clone(),
                    "liveness probe failed; will reconnect on next use".to_string(),
                );
                changed = true;
            }
        }
        if changed {
            self.rebuild_cache().await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn tool_definitions(&self) -> Vec<ToolDefinition> {
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

/// Cached MCP tools for `tools_schema()` — never blocks, never fails.
pub(crate) fn cached_tools() -> Vec<ToolDefinition> {
    GLOBAL
        .get()
        .and_then(|m| m.cached_tools.try_read().ok().map(|t| t.clone()))
        .unwrap_or_default()
}

/// Token cost of the cached MCP schema slice, for the compaction budget.
pub(crate) fn cached_schema_tokens() -> u64 {
    GLOBAL
        .get()
        .and_then(|m| {
            m.cached_tools
                .try_read()
                .ok()
                .map(|t| crate::agent::tokens::schema_token_estimate(&t))
        })
        .unwrap_or_default()
}

/// Tools dropped from the schema by the cap (0 when everything fits).
pub(crate) fn cached_truncated() -> usize {
    GLOBAL
        .get()
        .and_then(|m| m.cached_truncated.try_read().ok().map(|n| *n))
        .unwrap_or_default()
}

/// Ephemeral MCP status line for the turn-loop compaction budget: priced,
/// never stored. `None` when the manager was never initialized (no MCP
/// tools in the schema then either) or a lock is contended — the budget
/// probe must never block the loop or spawn the background refresh.
pub(crate) fn ephemeral_line() -> Option<String> {
    let mgr = GLOBAL.get()?;
    if mgr.configs.is_empty() {
        return None;
    }
    let clients = mgr.clients.try_read().ok()?;
    let tools = mgr.cached_tools.try_read().ok()?;
    let down = mgr.down.try_read().ok()?;
    crate::core::format::mcp_status_line(&McpManager::status_list(
        &mgr.configs,
        &clients,
        &tools,
        &down,
    ))
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
    let clients = mgr.clients.try_read().ok()?;
    let tools = mgr.cached_tools.try_read().ok()?;
    let down = mgr.down.try_read().ok()?;
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
