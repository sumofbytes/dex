//! MCP server configuration: parsing, names, gating.

use std::collections::{BTreeMap, HashMap};

use crate::protocol::ToolDefinition;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// One entry under `mcp_servers:` in config.yaml.
#[derive(Clone, Debug, Default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub disabled: bool,
    /// Tool allowlist: when non-empty, only these server-side tool names are
    /// exposed. Denylist wins over allowlist.
    pub allow: Vec<String>,
    /// Tool denylist: these server-side tool names are never exposed.
    pub deny: Vec<String>,
    /// Pre-registered OAuth client (`dex mcp login` skips dynamic
    /// registration when set). Refresh reuses the saved client otherwise.
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    /// OAuth scope for the authorize request (metadata default otherwise).
    pub oauth_scope: Option<String>,
}

impl McpServerConfig {
    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }

    /// Allowlist/denylist gate on the server-side (pre-namespace) tool name.
    pub fn tool_allowed(&self, tool: &str) -> bool {
        if self.deny.iter().any(|d| d == tool) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|a| a == tool)
    }
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MCP_OUTPUT_BYTES: usize = 32 * 1024;
pub const MCP_DESC_LIMIT: usize = 500;
/// Namespaced tool names are clamped so a hostile server cannot push a
/// multi-KB name into the schema on every request.
pub const MCP_TOOL_NAME_LIMIT: usize = 128;

/// Kill switch: `DEX_MCP=0|off|false|no` (or `DEX_NO_MCP=1`) disables every
/// MCP server. Default on.
pub fn mcp_enabled() -> bool {
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
pub fn mcp_max_tools() -> usize {
    std::env::var("DEX_MCP_MAX_TOOLS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(200)
}

/// `$VAR` / `${VAR}` expansion against the process environment.
/// Missing variables are an error (fail-closed): silently substituting `""`
/// would turn a missing API key into an unauthenticated request.
pub fn expand_env(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    // Walk chars, not bytes: `bytes[i] as char` turned each byte of a
    // multi-byte char into its own Latin-1 scalar (mojibake: `café$X` ->
    // `cafÃ©<value>`). `i` is a char index; `start` keeps the byte offset of
    // the current char for slicing `raw` (only ASCII can be a `$`/`${`
    // marker, so char boundaries never split one).
    let chars: Vec<(usize, char)> = raw.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if c == '$' && i + 1 < chars.len() {
            if chars[i + 1].1 == '{' {
                if let Some(end) = raw[start + 2..].find('}') {
                    let key = &raw[start + 2..start + 2 + end];
                    match std::env::var(key) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            return Err(format!("mcp config: env var ${{{key}}} is not set"));
                        }
                    }
                    // `$` + `{` + the key's chars + `}` are all consumed.
                    i += 3 + key.chars().count();
                    continue;
                }
            } else {
                let mut j = i + 1;
                while j < chars.len() && (chars[j].1.is_ascii_alphanumeric() || chars[j].1 == '_') {
                    j += 1;
                }
                if j > i + 1 {
                    let end = chars.get(j).map_or(raw.len(), |&(b, _)| b);
                    let key = &raw[start + 1..end];
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
        out.push(c);
        i += 1;
    }
    Ok(out)
}

/// Server names become part of a tool name: lowercase + `[a-z0-9_]` only.
pub fn sanitize_server_name(name: &str) -> String {
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
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
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
pub fn insert_cached(
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
pub fn split_mcp_name(name: &str) -> Option<(String, String)> {
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

/// Synthetic P1 reader tool name: `mcp__<server>_read_resource` (single
/// underscore, outside the `__` separator grammar). Single source.
pub fn resource_reader_name(server: &str) -> String {
    format!("mcp__{server}_read_resource")
}

/// Does a cached definition's name belong to `server`? Either the synthetic
/// reader or one of its real tools under `mcp__<server>__`.
pub fn def_belongs_to(def_name: &str, server: &str) -> bool {
    def_name == resource_reader_name(server) || def_name.starts_with(&format!("mcp__{server}__"))
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
pub fn parse_mcp_servers(root: &serde_yaml::Value) -> BTreeMap<String, McpServerConfig> {
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
        // YAML warnings carry the sanitized name; `insert_server` derives the
        // key from `display`, so pass it already-sanitized here.
        insert_server(&mut out, &sanitize_server_name(name), cfg);
    }
    out
}

/// Parse one `mcp_servers:` entry into `out`, keyed by the sanitized name.
/// Warnings use `display` verbatim — the YAML path passes the sanitized name,
/// the JSON env path the raw one (the warning text is the only difference).
fn insert_server(
    out: &mut BTreeMap<String, McpServerConfig>,
    display: &str,
    raw: &serde_yaml::Value,
) {
    match parse_server_config(raw) {
        Ok(cfg) if active_config(&cfg) => {
            out.insert(sanitize_server_name(display), cfg);
        }
        Ok(_) => {}
        Err(e) => eprintln!("dex: mcp server '{display}' ignored: {e}"),
    }
}

/// Load server configs from config.yaml (`DEX_MCP_SERVERS_JSON` wins for tests).
pub fn load_server_configs() -> BTreeMap<String, McpServerConfig> {
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
                    // JSON warnings carry the raw name (differs from YAML).
                    insert_server(&mut out, name, &yaml);
                }
                return out;
            }
        }
    }
    // Shared cached parse (perf doc §30): the old shadow reader re-read +
    // re-parsed config.yaml on every call — same paths, same outcome for
    // valid files (invalid files yield no servers either way, plus a
    // one-time warning from the cached loader).
    crate::llm::config::config_file_value()
        .as_ref()
        .map(parse_mcp_servers)
        .unwrap_or_default()
}
