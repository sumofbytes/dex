use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use unicode_width::UnicodeWidthStr;

use crate::agent::router::{
    classify_with_reasons, count_history_tool_calls, infer_tool_hints, model_for, TaskSignal, Tier,
    TierMap,
};
use crate::agent::tokens::estimate_tokens;
use crate::core::types::{ApiProtocol, ChatMessage, PermissionMode, Provider};
use crate::llm::auth::load_codex_credentials;

/// One `DEX_FOO=…` integer knob: env value parsed, `default` when the var is
/// unset or unparseable. Single-sources the
/// `env::var(..).ok().and_then(|v| v.parse().ok()).unwrap_or(..)` ladder
/// repeated through `from_env`.
fn env_parse<T: FromStr>(name: &str, default: T) -> T {
    env_parse_opt(name).unwrap_or(default)
}

/// Same env read + parse, but the caller keeps the fallback chain — used
/// where the default is reached only after catalog lookups
/// (`DEX_CONTEXT_WINDOW`).
fn env_parse_opt<T: FromStr>(name: &str) -> Option<T> {
    env::var(name).ok().and_then(|v| v.parse().ok())
}

/// Shared XDG-vs-HOME directory resolution: `$<env_var>/dex/<rel>` when the
/// XDG variable is set, else `$HOME/<home_sub>/dex/<rel>`, else `None` (no
/// `HOME`). Pure re-expression of the layout every config/cache path below
/// uses; the env var is a parameter because the sites use three different
/// ones (`DEX_CONFIG` overrides the whole config path, so its check stays
/// at that call site).
fn xdg_path(env_var: &str, home_sub: &str, rel: &str) -> Option<std::path::PathBuf> {
    if let Some(dir) = env::var_os(env_var) {
        return Some(std::path::PathBuf::from(dir).join(rel));
    }
    env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(home_sub).join(rel))
}

/// File-identity cache entry shared by the config file, learned-apis map,
/// and models.dev catalog: the parse is served while the content hash is
/// unchanged.
struct FileCache<T> {
    path: std::path::PathBuf,
    /// FNV-1a of the file bytes: identity is content, not (mtime, len),
    /// so same-length rewrites within one mtime tick and mtime-preserving
    /// copies still miss. Reads are per call (these files are KBs, the
    /// catalog parse below stays cached); the hit saves the parse.
    hash: u64,
    value: T,
}

fn fnv_bytes(text: &str) -> u64 {
    let mut h = 14695981039346656037u64;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Read `path` and serve `cache`'s stored parse while the content hash is
/// unchanged; on a miss, hand the text to `parse`, storing the result.
/// `parse` gets `None` when the read failed and returns `None` when nothing
/// should be cached (read/parse failure) — the caller decides what that
/// means (empty default vs hard failure). Identity is the content hash
/// rather than (mtime, len): a same-length rewrite inside one mtime tick
/// (FAT/NFS 1–2 s granularity, `cp -p`, checkout preserving mtime) still
/// misses instead of serving stale config/endpoints indefinitely.
/// Poisoned-mutex recovery matches the rest of the daemon: keep the value.
fn cached_parse<T: Clone>(
    cache: &OnceLock<Mutex<Option<FileCache<T>>>>,
    path: &std::path::Path,
    parse: impl FnOnce(Option<String>) -> Option<T>,
) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    let hash = fnv_bytes(&text);
    if let Some(hit) = cache
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.hash == hash)
    {
        return Some(hit.value.clone());
    }
    let value = parse(Some(text))?;
    cache
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(FileCache {
            path: path.to_path_buf(),
            hash,
            value: value.clone(),
        });
    Some(value)
}

/// One resolved knob: the value that `from_env` applies and the origin
/// that `doctor` reports. Both sides consume the same resolution, so the
/// precedence rule exists once and `doctor` cannot drift from runtime
/// routing.
struct Resolved<T> {
    value: T,
    origin: &'static str,
}

/// The single selection knob — `--model` > `DEX_MODEL` > file `model:`.
/// There is no built-in default model: nothing set anywhere is a setup
/// error (the guide text), so the daemon never silently rides a hardcoded
/// id that a catalog refresh can retire. The flag is pre-filtered by the
/// caller: `from_env` keeps an empty `--model` as a literal selection
/// (historical behavior), `doctor` treats it as unset.
fn resolve_selection(
    flag: Option<String>,
    file: &Option<serde_yaml::Value>,
) -> Result<Resolved<String>, String> {
    let dex_model = env::var("DEX_MODEL").ok().filter(|m| !m.trim().is_empty());
    let file_model = load_config_str(file, "model");
    let (value, origin) = if let Some(flag) = flag.clone() {
        (flag, "--model")
    } else if let Some(dex_model) = dex_model.clone() {
        (dex_model, "DEX_MODEL")
    } else if let Some(file_model) = file_model.clone() {
        (file_model, "config model:")
    } else {
        return Err(setup_guide_error());
    };
    Ok(Resolved { value, origin })
}

/// Provider fallback when the selection carries no prefix: `DEX_PROVIDER`
/// (deprecated) > file `active_provider:` (deprecated) > "opencode".
/// Returns doctor's origin wording alongside the name; `from_env` ignores
/// it. The `warn_once` fires here (deduped) so both entry points warn.
fn provider_fallback_with_origin(
    file: &Option<serde_yaml::Value>,
) -> (String, Option<&'static str>) {
    if let Ok(name) = env::var("DEX_PROVIDER") {
        if !name.trim().is_empty() {
            warn_once(
                "env:DEX_PROVIDER",
                "env var DEX_PROVIDER is deprecated — use DEX_MODEL=<provider>/<model> (e.g. DEX_MODEL=openai-codex)",
            );
            return (name, Some("DEX_PROVIDER (deprecated)"));
        }
    }
    match load_provider_name(file) {
        Some(name) => (name, Some("config active_provider: (deprecated)")),
        // `None` origin = nothing the user said; the name only shapes the
        // key-error text (`from_env` errors on the missing model id before
        // any request is built).
        None => ("opencode".to_string(), None),
    }
}

/// Provider for a selection that names none: the fallback chain — unless
/// that chain lands on the builtin default *and* an explicit `--base-url`
/// names an endpoint. Then the URL, not the default provider, is what the
/// user configured: it routes onto `providers.custom.*` so key errors name
/// the right deposit and opencode-specific wiring (key requirement,
/// session headers) never fires for a foreign endpoint. An explicit
/// deprecated pointer (`DEX_PROVIDER` / `active_provider:`) still wins —
/// the user named a provider.
fn provider_without_prefix(
    fallback: (String, Option<&'static str>),
    base_url_override: Option<&str>,
) -> String {
    let custom = fallback.1.is_none() && base_url_override.is_some_and(|u| !u.trim().is_empty());
    if custom {
        "custom".to_string()
    } else {
        fallback.0
    }
}

/// Config file location: `$DEX_CONFIG` > `$XDG_CONFIG_HOME/dex/config.yaml`
/// > `~/.config/dex/config.yaml`.
fn config_file_path() -> Option<std::path::PathBuf> {
    if let Some(p) = env::var_os("DEX_CONFIG") {
        return Some(std::path::PathBuf::from(p));
    }
    xdg_path("XDG_CONFIG_HOME", ".config", "dex/config.yaml")
}

/// Raw config file as YAML. Parsed as an untyped `Value` so unknown keys
/// survive the `/model` write-back. Missing/invalid file → None (env rules).
/// Cached process-wide and invalidated by content hash: `from_env` runs per
/// chat turn on the daemon, and each call was re-reading + re-parsing the file.
static CONFIG_CACHE: OnceLock<Mutex<Option<FileCache<Option<serde_yaml::Value>>>>> =
    OnceLock::new();

/// The parsed config file for other subsystems (MCP servers, extension
/// paths). Cached — same contract as `load_config_file`.
pub(crate) fn config_file_value() -> Option<serde_yaml::Value> {
    load_config_file()
}

fn load_config_file() -> Option<serde_yaml::Value> {
    let path = config_file_path()?;
    cached_parse(&CONFIG_CACHE, &path, |text| {
        // Unknown keys are almost always typos; name them instead of
        // letting a misspelled setting silently do nothing. A typo'd file
        // must not silently disable every user setting — the parse failure
        // is cached as `None` so `warn_once` stays the only report. A read
        // failure caches nothing.
        let text = text?;
        Some(match serde_yaml::from_str::<serde_yaml::Value>(&text) {
            Ok(value) => {
                if let Some(map) = value.as_mapping() {
                    let unknown: Vec<&str> = map
                        .keys()
                        .filter_map(|k| k.as_str())
                        .filter(|k| !KNOWN_FILE_KEYS.contains(k))
                        .collect();
                    if !unknown.is_empty() {
                        warn_once(
                            "config:unknown-keys",
                            &format!(
                                "unknown config key(s) {} in {} — valid keys: {}",
                                unknown.join(", "),
                                path.display(),
                                KNOWN_FILE_KEYS.join(", ")
                            ),
                        );
                    }
                }
                Some(value)
            }
            Err(e) => {
                warn_once(
                    "config:invalid",
                    &format!(
                        "ignoring invalid config {}: {e}\n     valid top-level keys: {}",
                        path.display(),
                        KNOWN_FILE_KEYS.join(", ")
                    ),
                );
                None
            }
        })
    })
    .flatten()
}

fn invalidate_config_cache() {
    if let Some(cache) = CONFIG_CACHE.get() {
        cache.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// Context-window resolution chain — `DEX_CONTEXT_WINDOW` > file
/// `context_window:` > the slim cross-process index > the catalog (which
/// then refreshes the index). No built-in default: a model the catalog
/// doesn't size needs an explicit window, so the miss is an error.
fn resolve_context_window(model: &str, file: &Option<serde_yaml::Value>) -> Result<u64, String> {
    context_window_from_env()
        .or_else(|| load_config_num(file, "context_window"))
        .or_else(|| ctx_from_index(model))
        .or_else(|| {
            load_dex_catalog().and_then(|c| {
                let ctx = catalog_context_window(model);
                ensure_ctx_index(&c);
                ctx
            })
        })
        .ok_or_else(|| {
            format!(
                "no context window for model '{model}' — set 'context_window: <tokens>' in the config or DEX_CONTEXT_WINDOW (see `dex doctor`)"
            )
        })
}

/// `DEX_CONTEXT_WINDOW`, positive integers only — mirrors `load_config_num`:
/// a zero/garbage value is a warning, never a silent zero-width window that
/// trips the compaction threshold on every turn.
fn context_window_from_env() -> Option<u64> {
    let raw = env::var("DEX_CONTEXT_WINDOW").ok()?;
    match raw.trim().parse::<u64>() {
        Ok(n) if n > 0 => Some(n),
        _ => {
            warn_once(
                "env:DEX_CONTEXT_WINDOW",
                "DEX_CONTEXT_WINDOW must be a positive token count — ignoring it",
            );
            None
        }
    }
}

/// Numeric config-file key (`context_window:`); positive integers only —
/// a zero/negative/garbage value is a warning, not a silent miss.
fn load_config_num(file: &Option<serde_yaml::Value>, key: &str) -> Option<u64> {
    let value = file.as_ref()?.get(key)?;
    let num = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
    if num == 0 {
        warn_once(
            "config:context_window",
            "config key 'context_window:' must be a positive token count — ignoring it",
        );
        return None;
    }
    Some(num)
}

fn load_config_str(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    file.as_ref()
        .and_then(|f| f.get(key))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Every top-level config key dex reads (plus the deprecated ones it still
/// honors). Used for typo hints: an unknown key is called out instead of
/// silently doing nothing, and a parse error lists what is valid.
const KNOWN_FILE_KEYS: &[&str] = &[
    "model",
    "providers",
    "context_window",
    "thinking_effort",
    "system_prompt",
    "system_prompt_file",
    "mcp_servers",
    "agent_wake",
    "routing",
    "extensions",
    // Deprecated but still honored for old files:
    "active_provider",
    "provider",
    "base_url",
    "api",
    "headers",
    "http_headers",
];

/// One-time notice, keyed so a notice fires once per process even though
/// the daemon rebuilds config every turn. `id` dedupes; `message` is the
/// full text after the `dex: ` prefix. stderr only — stdout belongs to the
/// client stream, and one line cannot corrupt a TUI the way a per-turn
/// stream could.
pub(crate) fn warn_once(id: &str, message: &str) {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = WARNED.get_or_init(Default::default);
    if seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string())
    {
        eprintln!("dex: {message}");
    }
}

/// The `agent_wake` knob (§10b V1b): when a completion notice is queued
/// while the session is idle and a client is plausibly listening, the
/// daemon runs one wake turn to deliver it. Config `agent_wake:` (default
/// on) with a `DEX_AGENT_WAKE` kill switch — env beats file, per the
/// standard precedence. Returns the value plus its origin for `doctor`.
pub(crate) fn agent_wake_origin() -> (bool, &'static str) {
    if let Ok(raw) = env::var("DEX_AGENT_WAKE") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => return (false, "DEX_AGENT_WAKE"),
            "1" | "true" | "on" | "yes" => return (true, "DEX_AGENT_WAKE"),
            _ => {}
        }
    }
    if let Some(enabled) = load_config_file()
        .as_ref()
        .and_then(|file| file.get("agent_wake"))
        .and_then(|value| value.as_bool())
    {
        return (enabled, "config: agent_wake");
    }
    (true, "built-in default")
}

/// The wake gate read by the scheduler: `true` unless disabled.
pub(crate) fn agent_wake_enabled() -> bool {
    agent_wake_origin().0
}

/// Complexity-router switch (`routing.enabled:`) and per-tier env vars.
/// Env beats file, per the standard precedence; tiers name full
/// `provider/model` selections, so catalog/learned-API/`api:` pins keep
/// working through the existing selection path.
pub(crate) const ROUTING_ENV: &str = "DEX_ROUTING";
pub(crate) const ROUTING_FAST_ENV: &str = "DEX_ROUTING_FAST";
pub(crate) const ROUTING_BALANCED_ENV: &str = "DEX_ROUTING_BALANCED";
pub(crate) const ROUTING_POWERFUL_ENV: &str = "DEX_ROUTING_POWERFUL";
/// Env var holding a tier's selection (`DEX_ROUTING_FAST`…).
fn routing_tier_env(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => ROUTING_FAST_ENV,
        Tier::Balanced => ROUTING_BALANCED_ENV,
        Tier::Powerful => ROUTING_POWERFUL_ENV,
    }
}

/// Origin wording for a tier's file selection (`config routing.fast:`…).
fn routing_file_origin(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "config routing.fast:",
        Tier::Balanced => "config routing.balanced:",
        Tier::Powerful => "config routing.powerful:",
    }
}

/// Per-tier origins for [`RoutingResolution`]: where each raw selection
/// came from. A miss falls back at use time (`model_for`: tier miss →
/// `balanced` → top-level `model:`), so the miss origin only says that.
pub(crate) struct TierOrigins {
    pub(crate) fast: &'static str,
    pub(crate) balanced: &'static str,
    pub(crate) powerful: &'static str,
}

impl TierOrigins {
    fn get(&self, tier: Tier) -> &'static str {
        match tier {
            Tier::Fast => self.fast,
            Tier::Balanced => self.balanced,
            Tier::Powerful => self.powerful,
        }
    }
}

/// The resolved `routing:` knob: the switch plus the raw per-tier
/// selections (empty = unset). `from_env` and `doctor` share this
/// resolution so the origin rows cannot drift from runtime routing.
pub(crate) struct RoutingResolution {
    pub(crate) enabled: bool,
    pub(crate) enabled_origin: &'static str,
    pub(crate) tiers: TierMap,
    pub(crate) tier_origins: TierOrigins,
}

/// One file key's trimmed selection (`None` = missing, empty, or
/// non-string). A present-but-empty or non-string value warns once and
/// falls through like a miss, never a hard error.
fn routing_file_key(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    let value = file
        .as_ref()
        .and_then(|f| f.get("routing"))
        .and_then(|r| r.as_mapping())
        .and_then(|m| m.get(key))?;
    match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s.to_string()),
        None => {
            warn_once(
                &format!("config:routing:{key}"),
                &format!("ignoring routing.{key}: — set a 'provider/model' selection string"),
            );
            None
        }
    }
}

/// One tier's raw selection: `DEX_ROUTING_<TIER>` > file `routing.<tier>:`.
/// Empty/missing at every layer is not an error — `model_for` falls
/// through to `balanced`, then to top-level `model:`.
fn routing_tier_value(
    file: &Option<serde_yaml::Value>,
    tier: Tier,
    miss_origin: &'static str,
) -> (String, &'static str) {
    let env_var = routing_tier_env(tier);
    if let Ok(raw) = env::var(env_var) {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return (trimmed, env_var);
        }
    }
    if let Some(selection) = routing_file_key(file, tier.key()) {
        return (selection, routing_file_origin(tier));
    }
    (String::new(), miss_origin)
}

/// Shared `routing:` resolution: `DEX_ROUTING` > file `routing.enabled:`
/// > off, plus each tier's selection. Default off — routing is opt-in.
pub(crate) fn routing_resolution(file: &Option<serde_yaml::Value>) -> RoutingResolution {
    const BALANCED_MISS: &str = "unset (falls back to model:)";
    const TIER_MISS: &str = "unset (falls back to routing.balanced:, then model:)";
    let mut enabled = false;
    let mut enabled_origin = "built-in default";
    if let Ok(raw) = env::var(ROUTING_ENV) {
        match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => {
                enabled = false;
                enabled_origin = ROUTING_ENV;
            }
            "1" | "true" | "on" | "yes" => {
                enabled = true;
                enabled_origin = ROUTING_ENV;
            }
            _ => warn_once(
                "env:DEX_ROUTING",
                "DEX_ROUTING must be 0/1 (or true/false, on/off, yes/no) — ignoring it",
            ),
        }
    }
    if enabled_origin == "built-in default" {
        let flag = file
            .as_ref()
            .and_then(|f| f.get("routing"))
            .and_then(|r| r.as_mapping())
            .and_then(|m| m.get("enabled"));
        match flag {
            None => {}
            Some(v) => match v.as_bool() {
                Some(b) => {
                    enabled = b;
                    enabled_origin = "config routing.enabled:";
                }
                None => warn_once(
                    "config:routing:enabled",
                    "ignoring routing.enabled: — set true or false",
                ),
            },
        }
    }
    let mut tiers = TierMap::default();
    let mut origins = TierOrigins {
        fast: TIER_MISS,
        balanced: BALANCED_MISS,
        powerful: TIER_MISS,
    };
    for tier in Tier::ALL {
        let miss = if tier == Tier::Balanced {
            BALANCED_MISS
        } else {
            TIER_MISS
        };
        let (value, origin) = routing_tier_value(file, tier, miss);
        match tier {
            Tier::Fast => {
                tiers.fast = value;
                origins.fast = origin;
            }
            Tier::Balanced => {
                tiers.balanced = value;
                origins.balanced = origin;
            }
            Tier::Powerful => {
                tiers.powerful = value;
                origins.powerful = origin;
            }
        }
    }
    RoutingResolution {
        enabled,
        enabled_origin,
        tiers,
        tier_origins: origins,
    }
}

/// One routed turn: classify the prompt and resolve the tier's model
/// through `model_for` (tier miss → `balanced` → top-level `model:`).
/// `None` when routing is off or nothing selects a model (the normal
/// `from_env` setup error then explains itself). `model_override` is
/// `Some` only when the tier resolves away from the current selection,
/// so an unrouted turn rebuilds nothing. Callers with an explicit
/// `--model` / per-request model skip this — the explicit pick wins.
pub(crate) struct RoutedTurn {
    pub(crate) tier: Tier,
    pub(crate) model_override: Option<String>,
    /// Strongest classification signal, for the per-turn log line.
    pub(crate) reason: &'static str,
}

pub(crate) fn route_turn(prompt: &str, history: &[ChatMessage]) -> Option<RoutedTurn> {
    let file = load_config_file();
    let routing = routing_resolution(&file);
    if !routing.enabled {
        return None;
    }
    let selection = resolve_selection(None, &file).ok()?;
    // History doubles as the routing signal: token size plus the real
    // tool-call count, so a deep session weighs in without prompt words.
    // The daemon reuses this same load for the turn itself, so routing
    // sees exactly what the turn will send.
    let signal = TaskSignal {
        prompt_chars: prompt.chars().count(),
        history_tokens: estimate_tokens(history),
        history_tool_calls: count_history_tool_calls(history),
        tool_hints: infer_tool_hints(prompt),
    };
    let decision = classify_with_reasons(&signal, prompt);
    let model = model_for(decision.tier, &routing.tiers, &selection.value);
    Some(RoutedTurn {
        tier: decision.tier,
        model_override: (model != selection.value).then(|| model.to_string()),
        reason: decision.top_reason(),
    })
}

/// What `doctor` shows per tier: the tier's own selection, else
/// `routing.balanced:`, else the top-level selection — the same chain
/// `model_for` applies at runtime, so the row explains the turn's model.
fn routing_tier_display(
    tier: Tier,
    routing: &RoutingResolution,
    selection: Option<&str>,
    selection_source: &str,
) -> (String, String) {
    let value = routing.tiers.get(tier);
    if !value.is_empty() {
        return (
            value.to_string(),
            routing.tier_origins.get(tier).to_string(),
        );
    }
    if tier != Tier::Balanced && !routing.tiers.balanced.is_empty() {
        return (
            routing.tiers.balanced.clone(),
            routing.tier_origins.balanced.to_string(),
        );
    }
    match selection {
        Some(value) => (value.to_string(), selection_source.to_string()),
        None => (
            "(unset)".to_string(),
            "UNCONFIGURED — set 'model: <provider>/<model>'".to_string(),
        ),
    }
}

/// Read a system-prompt file for the env/file layers: a miss warns once and
/// falls through to the next layer (an explicit `--system-prompt-file` miss
/// is instead a hard error at the CLI boundary, so remote daemons never
/// silently run the default when the user named a file).
fn read_prompt_file(path: &str, warn_id: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Some(text),
        Ok(_) => None,
        Err(e) => {
            warn_once(
                warn_id,
                &format!("ignoring unreadable system prompt file '{path}': {e}"),
            );
            None
        }
    }
}

/// Custom base system prompt (DEX-13): replaces the built-in identity/rules;
/// project instructions, extensions and skills are still appended. Subagent
/// children keep their own persona/rules (`child_system_prompt`); `dex serve`
/// ignores CLI flags and falls back to its own env/file layers unless the
/// client forwards per-request text. Precedence mirrors every other knob —
/// explicit per-request/CLI text > env inline > env file > file inline >
/// file file > built-in default (`None`). Inline beats file within a layer;
/// any CLI beats any env beats any file. Empty/whitespace-only values count
/// as unset at every layer and fall through. Returns the text plus its
/// origin for `doctor`.
pub(crate) fn system_prompt_origin(explicit: Option<&str>) -> (Option<String>, &'static str) {
    system_prompt_origin_with_label(explicit, "--system-prompt")
}

/// Same as [`system_prompt_origin`], with the caller-provided label for the
/// explicit layer so `doctor` can report `--system-prompt-file` when the
/// text came from the file flag (runtime callers forward text only and keep
/// the default label — only the text matters there).
pub(crate) fn system_prompt_origin_with_label(
    explicit: Option<&str>,
    explicit_label: &'static str,
) -> (Option<String>, &'static str) {
    if let Some(text) = explicit.filter(|t| !t.trim().is_empty()) {
        return (Some(text.to_string()), explicit_label);
    }
    if let Ok(text) = env::var("DEX_SYSTEM_PROMPT") {
        if !text.trim().is_empty() {
            return (Some(text), "DEX_SYSTEM_PROMPT");
        }
    }
    if let Ok(path) = env::var("DEX_SYSTEM_PROMPT_FILE") {
        if !path.trim().is_empty() {
            if let Some(text) = read_prompt_file(path.trim(), "env:DEX_SYSTEM_PROMPT_FILE") {
                return (Some(text), "DEX_SYSTEM_PROMPT_FILE");
            }
        }
    }
    let file = load_config_file();
    if let Some(text) = load_config_str(&file, "system_prompt").filter(|t| !t.trim().is_empty()) {
        return (Some(text), "config system_prompt:");
    }
    if let Some(path) =
        load_config_str(&file, "system_prompt_file").filter(|p| !p.trim().is_empty())
    {
        if let Some(text) = read_prompt_file(path.trim(), "config:system_prompt_file") {
            return (Some(text), "config system_prompt_file:");
        }
    }
    (None, "built-in default")
}

/// Resolve the client-side CLI pair (`--system-prompt` > `--system-prompt-file`)
/// into the single text override forwarded per-request, plus the flag it came
/// from for `doctor`. The file is read here so a remote daemon never has to
/// see the client's local path; a miss is a hard error instead of a silent
/// default. Whitespace-only inline/file content counts as unset (`None`) and
/// falls through to the env/file layers.
pub(crate) fn resolve_cli_system_prompt(
    inline: Option<String>,
    file: Option<String>,
) -> Result<Option<(String, &'static str)>, String> {
    if let Some(text) = inline.filter(|t| !t.trim().is_empty()) {
        return Ok(Some((text, "--system-prompt")));
    }
    if let Some(path) = file.filter(|p| !p.trim().is_empty()) {
        let path = path.trim().to_string();
        return std::fs::read_to_string(&path)
            .map(|text| {
                if text.trim().is_empty() {
                    None
                } else {
                    Some((text, "--system-prompt-file"))
                }
            })
            .map_err(|e| format!("cannot read --system-prompt-file '{path}': {e}"));
    }
    Ok(None)
}

/// Selection pointer fallback: `active_provider:` is deprecated — the
/// provider now rides inside `model:` as `provider/model` (a legacy
/// `provider:` key is honored with the same warning).
fn load_provider_name(file: &Option<serde_yaml::Value>) -> Option<String> {
    if let Some(name) = load_config_str(file, "active_provider") {
        warn_once(
            "config:active_provider",
            "config key 'active_provider:' is deprecated — name the provider in 'model:' as 'provider/model' (e.g. 'model: zai/glm-5.3-flash')",
        );
        return Some(name);
    }
    let legacy = load_config_str(file, "provider")?;
    warn_once(
        "config:provider",
        "config key 'provider:' is renamed — use 'model: <provider>/<model>' (e.g. 'model: opencode/gpt-5.6-luna')",
    );
    Some(legacy)
}

/// A configured provider (`providers:` map in config.yaml): the deposit
/// place for that provider's API key plus optional overrides. Endpoint,
/// models, pricing, context windows and reasoning options come from the
/// models.dev catalog entry of the same key; the wire protocol is learned
/// empirically unless pinned here. Applies to builtins too
/// (`providers.opencode.api_key` beats the env var).
#[derive(Clone, Default)]
pub(crate) struct ProviderEntry {
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) api: Option<ApiProtocol>,
    /// Extra headers sent only to this provider (gateway auth, routing,
    /// attribution). Same shapes as the global `headers:` key (mapping,
    /// text block, or list); merged under the global/env/CLI extras, which
    /// win on collision.
    pub(crate) headers: BTreeMap<String, String>,
}

fn load_provider_entries(file: &Option<serde_yaml::Value>) -> BTreeMap<String, ProviderEntry> {
    let Some(map) = file
        .as_ref()
        .and_then(|f| f.get("providers"))
        .and_then(|p| p.as_mapping())
    else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (key, value) in map {
        let Some(name) = key
            .as_str()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        let entry = Some(value.clone());
        out.insert(
            name,
            ProviderEntry {
                api_key: load_config_str(&entry, "api_key"),
                base_url: load_config_str(&entry, "base_url"),
                api: load_config_str(&entry, "api")
                    .as_deref()
                    .and_then(ApiProtocol::parse),
                headers: config_headers_map(&entry, "headers"),
            },
        );
    }
    out
}

/// Names of configured generic providers — the extra vocabulary accepted
/// wherever a provider name routes (`/model` + `/provider` prefixes,
/// `parse_known`).
fn known_providers(entries: &BTreeMap<String, ProviderEntry>) -> BTreeSet<String> {
    entries.keys().cloned().collect()
}

/// How a selection string routes: which known-provider prefix it carries
/// and what remains. `split_selection`, `strip_routing_prefixes`, and
/// `apply_model` classify the same `provider/` grammar with different
/// outcomes; this enum is the shared classification. Endpoint prefixes
/// (`go/…`) are deliberately not classified here — endpoints are instance
/// state, so each consumer keeps its own endpoint step.
enum SelectionRoute {
    /// `provider/rest` — a known provider prefix; `rest` may be empty.
    ProviderQualified { provider: String, rest: String },
    /// The whole selection is a bare known provider name (no `/`).
    BareProvider { provider: String },
    /// No known-provider prefix: the whole selection is the model id.
    Model(String),
}

/// Classify one selection string against the known provider names.
fn classify_selection(selection: &str, known: &BTreeSet<String>) -> SelectionRoute {
    match selection.split_once('/') {
        Some((prefix, rest)) if Provider::parse_known(prefix, known).is_some() => {
            SelectionRoute::ProviderQualified {
                provider: prefix.trim().to_ascii_lowercase(),
                rest: rest.trim().to_string(),
            }
        }
        _ if Provider::parse_known(selection, known).is_some() => SelectionRoute::BareProvider {
            provider: selection.trim().to_ascii_lowercase(),
        },
        _ => SelectionRoute::Model(selection.to_string()),
    }
}

/// Split a model selection into `(provider, model)`. A leading
/// `provider/` prefix — or the whole selection being a bare provider name —
/// selects the provider; the remainder is the model id (a bare provider
/// keeps the builtin default model). The provider is lowercased, matching
/// `Provider::name()`. `None` provider means "selection names
/// no known provider" (an endpoint prefix like `go/…` or a plain model id).
/// The error for a bare provider pick with no model id (`--model
/// anthropic`, file `model: opencode`) — there is no per-provider built-in
/// default model to fall back to.
fn bare_provider_needs_model(selection: &str, provider: &str) -> String {
    format!(
        "'{selection}' names a provider but no model — set 'model: {provider}/<model>' (see the provider's catalog entry, or `dex doctor`)"
    )
}

fn split_selection(
    selection: &str,
    known: &BTreeSet<String>,
) -> Result<(Option<String>, String), String> {
    match classify_selection(selection, known) {
        SelectionRoute::ProviderQualified { provider, rest } => {
            if rest.is_empty() {
                Err(bare_provider_needs_model(selection, &provider))
            } else {
                Ok((Some(provider.clone()), rest.to_string()))
            }
        }
        SelectionRoute::BareProvider { provider } => {
            Err(bare_provider_needs_model(selection, &provider))
        }
        SelectionRoute::Model(model) => Ok((None, model)),
    }
}

/// Per-catalog-generation lookup index (§24/§25): `from_env` runs per daemon
/// chat turn and every model call re-probes the catalog (idle timeout via
/// `reasoning_options_for`, `usage_cost`, `cache_write_read_ratio`), but each
/// probe used to walk all providers × models with a lowercase alloc per id.
/// The index walks once per catalog generation (same file-identity
/// invalidation as the catalog parse itself) and serves every probe from
/// maps. Shape quirks are preserved per lookup via the `endpoint_only` /
/// `from_flat` flags: each reader sees exactly the entries the old walk
/// would have visited.
#[derive(Clone, Default)]
struct CostRates {
    input: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    output: Option<f64>,
}

#[derive(Clone, Default)]
struct IndexedModel {
    /// Catalog provider key (`""` for the flat `models` shape, which names none).
    provider: String,
    /// The provider entry's `api` URL, if any.
    api: Option<String>,
    context: Option<u64>,
    output: Option<u64>,
    /// `Some` iff the entry carries a `cost` object (even an empty one —
    /// the old walk returned the object and defaulted missing rates).
    cost: Option<CostRates>,
    reasoning_options: Option<Vec<String>>,
    /// From the `providers`-nested shape (catalog.json): visible only to
    /// endpoint routing, like the old walk which consulted that shape solely
    /// in `catalog_endpoint_for_model`.
    endpoint_only: bool,
    /// From the flat top-level `models` shape: visible to context/output/
    /// has-model/bare-id lookups, never to cost tiers (the old tier walk
    /// only matched provider entries).
    from_flat: bool,
}

struct CatalogIndex {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    /// FNV-1a of the catalog text this index was built from. (mtime, len)
    /// alone can't tell a mtime-preserving copy or a same-content rewrite
    /// from real new data; on a metadata miss this hash decides re-parse
    /// vs. refresh-without-reparse (see `with_catalog_index`).
    hash: u64,
    /// Lowercased model id → entries in catalog iteration order.
    by_id: HashMap<String, Vec<IndexedModel>>,
    provider_api: HashMap<String, String>,
    provider_env: HashMap<String, Vec<String>>,
    /// Bare model ids with their provider key (top-level providers plus the
    /// flat shape, whose key is `""`); one id may repeat across providers
    /// (each contributes its own endpoint prefix at expansion time).
    bare: Vec<(String, String)>,
    /// Fully expanded `available_models` lists by configured-provider set
    /// (capped): concurrent turns with differing sets each hit instead of
    /// thrashing a single-entry cache.
    expanded_for: HashMap<BTreeSet<String>, Vec<String>>,
}

static CATALOG_INDEX: OnceLock<Mutex<Option<CatalogIndex>>> = OnceLock::new();

/// Serve `f` from the in-process catalog index only when it is already warm
/// AND still describes the catalog on disk: a mutex lock + one `stat`, never
/// a file read or rebuild. Used for cross-checks (see `ctx_from_index`) that
/// must not turn the KB-slim fast path into a 4MB parse. The metadata check
/// matters: without it a warm index left over from the previous catalog
/// generation would be served (and could rewrite `models.ctx.json` from
/// stale context windows) after `dex update --models`.
fn if_catalog_index_warm<T>(f: impl FnOnce(&CatalogIndex) -> T) -> Option<T> {
    let path = dex_catalog_cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    let guard = CATALOG_INDEX
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|index| index.path == path && index.mtime == mtime && index.len == len)
        .map(f)
}

/// Advertised reasoning-effort values of one model entry (models.dev
/// `reasoning_options`, e.g. glm-5.3-flash: low/high/max).
fn reasoning_values(entry: &serde_json::Value) -> Option<Vec<String>> {
    let options = entry.get("reasoning_options")?.as_array()?;
    let values: Vec<String> = options
        .iter()
        .filter_map(|o| o.get("values"))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    (!values.is_empty()).then_some(values)
}

fn cost_rates(entry: &serde_json::Value) -> Option<CostRates> {
    let cost = entry.get("cost")?;
    let rate = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| cost.get(*name))
            .and_then(|v| v.as_f64())
    };
    Some(CostRates {
        input: rate(&["input"]).unwrap_or(0.0),
        cache_read: rate(&["cache_read", "cacheRead"]),
        cache_write: rate(&["cache_write", "cacheWrite"]),
        output: rate(&["output"]),
    })
}

fn limit_of(entry: &serde_json::Value, key: &str) -> Option<u64> {
    entry
        .get("limit")
        .and_then(|l| l.get(key))
        .and_then(|c| c.as_u64())
}

/// Fold one provider entry's models into the index.
fn index_provider_models(
    index: &mut CatalogIndex,
    prov_key: &str,
    api: Option<String>,
    models: &serde_json::Map<String, serde_json::Value>,
    endpoint_only: bool,
    from_flat: bool,
) {
    for (id, entry) in models {
        index
            .by_id
            .entry(id.to_ascii_lowercase())
            .or_default()
            .push(IndexedModel {
                provider: prov_key.to_string(),
                api: api.clone(),
                context: limit_of(entry, "context"),
                output: limit_of(entry, "output"),
                cost: cost_rates(entry),
                reasoning_options: reasoning_values(entry),
                endpoint_only,
                from_flat,
            });
        if !endpoint_only {
            index.bare.push((id.clone(), prov_key.to_string()));
        }
    }
}

fn build_catalog_index(
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    hash: u64,
    catalog: &serde_json::Value,
) -> CatalogIndex {
    let mut index = CatalogIndex {
        path,
        mtime,
        len,
        hash,
        by_id: HashMap::new(),
        provider_api: HashMap::new(),
        provider_env: HashMap::new(),
        bare: Vec::new(),
        expanded_for: HashMap::new(),
    };
    // Top-level provider entries (api.json shape). The `models`/`providers`
    // keys hold model/provider maps, not provider entries — the old walks
    // found no `models` child in them, so they contribute nothing here.
    if let Some(providers) = catalog.as_object() {
        for (prov_key, entry) in providers {
            if prov_key == "models" || prov_key == "providers" {
                continue;
            }
            let Some(models) = entry.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            if let Some(api) = entry
                .get("api")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
            {
                index.provider_api.insert(prov_key.clone(), api);
            }
            // Same `env`-map reading the old `catalog_env_vars` did (list
            // or map shape); sorted so resolution never depends on key order.
            let mut env_names: Vec<String> = match entry.get("env") {
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_string)
                    .collect(),
                Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
                _ => Vec::new(),
            };
            env_names.sort();
            env_names.dedup();
            if !env_names.is_empty() {
                index.provider_env.insert(prov_key.clone(), env_names);
            }
            let api = index.provider_api.get(prov_key).cloned();
            index_provider_models(&mut index, prov_key, api, models, false, false);
        }
    }
    // Flat `models` shape (catalog.json): bare ids, context/output caps,
    // has-model — but never cost tiers or endpoint routing (no provider).
    if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
        index_provider_models(&mut index, "", None, models, false, true);
    }
    // `providers`-nested shape (catalog.json): endpoint routing only.
    if let Some(nested) = catalog.get("providers").and_then(|p| p.as_object()) {
        for (prov_key, entry) in nested {
            let Some(models) = entry.get("models").and_then(|m| m.as_object()) else {
                continue;
            };
            let api = entry
                .get("api")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            index_provider_models(&mut index, prov_key, api, models, true, false);
        }
    }
    index
}

/// Run `f` against the current catalog index, rebuilding it when the catalog
/// file changed since. `None` when no catalog is cached — every caller falls
/// back exactly as before (config error, silent skip, or default).
/// Run `f` against the current catalog index, rebuilding it when the catalog
/// file changed since. `None` when no catalog is cached — every caller falls
/// back exactly as before (config error, silent skip, or default).
///
/// Hot path stays metadata-only (`fs::metadata`, no read). On a metadata
/// miss, identity is content: the 4MB read + FNV hash turns a
/// mtime-preserving copy or a same-content rewrite into a cheap metadata
/// refresh instead of a full re-parse. A same-length rewrite inside one
/// mtime tick still serves the previous generation until the next metadata
/// change — hashing per call would cost more than the index saves.
fn with_catalog_index<T>(f: impl FnOnce(&CatalogIndex) -> T) -> Option<T> {
    let path = dex_catalog_cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    {
        let guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = guard
            .as_ref()
            .filter(|index| index.path == path && index.mtime == mtime && index.len == len)
        {
            return Some(f(index));
        }
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let hash = fnv_bytes(&text);
    {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = (*guard)
            .as_mut()
            .filter(|index| index.path == path && index.hash == hash)
        {
            // Same bytes under fresh metadata: the parsed index is still
            // valid; just record the identity the next hot-path check sees.
            index.mtime = mtime;
            index.len = len;
            return Some(f(index));
        }
    }
    let catalog = load_dex_catalog()?;
    let fresh = build_catalog_index(path.clone(), mtime, len, hash, &catalog);
    let mut guard = CATALOG_INDEX
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // A concurrent turn may have rebuilt while this one parsed — serve the
    // newest generation either way (same content hash, same conclusions).
    // The path check stays: an `XDG_CACHE_HOME` switch between the two
    // locks must not serve the other cache dir's index as this path's.
    if let Some(index) = guard
        .as_ref()
        .filter(|index| index.path == path && index.hash == hash)
    {
        return Some(f(index));
    }
    *guard = Some(fresh);
    Some(f(guard.as_ref()?))
}

/// Same as [`with_catalog_index`] with a mutable index: the expanded
/// available-models cache lives on the index itself.
fn with_catalog_index_mut<T>(f: impl FnOnce(&mut CatalogIndex) -> T) -> Option<T> {
    let path = dex_catalog_cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fresh = guard
            .as_ref()
            .is_some_and(|index| index.path == path && index.mtime == mtime && index.len == len);
        if fresh {
            return Some(f(guard.as_mut()?));
        }
    }
    // Metadata miss — identity is content, same contract as
    // `with_catalog_index`: same bytes under fresh metadata refresh the
    // recorded identity without a re-parse; different bytes rebuild.
    let text = std::fs::read_to_string(&path).ok()?;
    let hash = fnv_bytes(&text);
    {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = (*guard)
            .as_mut()
            .filter(|index| index.path == path && index.hash == hash)
        {
            index.mtime = mtime;
            index.len = len;
            return Some(f(index));
        }
    }
    // Parsed outside the lock (like `cached_parse`): the 4MB walk never
    // blocks concurrent readers serving the previous generation.
    let catalog = load_dex_catalog()?;
    let fresh_index = build_catalog_index(path.clone(), mtime, len, hash, &catalog);
    let mut guard = CATALOG_INDEX
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let stale = guard
        .as_ref()
        .map(|index| index.path != path || index.hash != hash)
        .unwrap_or(true);
    if stale {
        *guard = Some(fresh_index);
    } else {
        // Same content (another turn rebuilt it while we parsed): refresh
        // the recorded metadata so the hot path hits.
        if let Some(index) = (*guard).as_mut() {
            index.mtime = mtime;
            index.len = len;
        }
    }
    Some(f(guard.as_mut()?))
}

/// Catalog `api` URL for a provider key ("zai" → its serving endpoint).
/// Entries without one (native-API providers like anthropic) are not usable
/// as generic OpenAI-compatible providers — that absence is the gate.
fn catalog_api(key: &str) -> Option<String> {
    with_catalog_index(|index| index.provider_api.get(key).cloned()).flatten()
}

/// Whether `model` names a known model id anywhere in the catalog (either
/// shape). Guards the provider-like hint: native `org/model` ids share
/// their prefix with a provider but are legit model ids.
fn catalog_has_model(model: &str) -> bool {
    with_catalog_index(|index| {
        index
            .by_id
            .get(model.to_ascii_lowercase().as_str())
            .is_some_and(|entries| entries.iter().any(|e| !e.endpoint_only))
    })
    .unwrap_or(false)
}

/// A selection naming an unconfigured models.dev provider — bare (`zai`)
/// or qualified (`zai/glm-…`) — rides the fallback provider as an opaque
/// id and dies later with an API error. Say so, once, with the fix.
/// Returns whether it warned.
fn warn_provider_like_selection(selection: &str, provider_name: &str, served: &[String]) -> bool {
    // Ids the gateway serves and the resolved provider itself aren't mistakes.
    if selection == provider_name || served.iter().any(|m| m.as_str() == selection) {
        return false;
    }
    // For `prefix/rest` the mistake candidate is the prefix; the full id
    // may still be legit (provider-native model ids like
    // `moonshotai/kimi-k2.6`, endpoint routes like `go/…`).
    let (candidate, qualified) = match selection.split_once('/') {
        Some((prefix, rest)) if !prefix.is_empty() && !rest.is_empty() => (prefix, true),
        Some(_) => return false,
        None => (selection, false),
    };
    if candidate == provider_name {
        return false;
    }
    if catalog_api(candidate).is_none() {
        return false;
    }
    if qualified && catalog_has_model(selection) {
        return false;
    }
    let key_env = catalog_env_vars(candidate)
        .first()
        .cloned()
        .unwrap_or_else(|| "<key>".to_string());
    warn_once(
        &format!("hint:provider:{candidate}"),
        &format!(
            "'{selection}' looks like provider '{candidate}', not a model id — it's being sent to \
'{provider_name}' as-is; to use it, add 'providers: {candidate}: {{api_key: <key>}}' to config \
(key env: {key_env})"
        ),
    );
    true
}

/// Every documented API-key env var for a provider (all keys of the catalog
/// `env` map: e.g. ZHIPU_API_KEY, OPENROUTER_API_KEY, …), sorted so the
/// resolution order never depends on JSON key order. Tried in order — a
/// provider documenting several names accepts any of them.
fn catalog_env_vars(key: &str) -> Vec<String> {
    // api.json shape is a list of names; older catalog.json used an object
    // (name → description). Accept both — only the names matter here.
    // Sorted at index time so resolution never depends on JSON key order.
    with_catalog_index(|index| index.provider_env.get(key).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Builtin providers whose canonical key env var is pinned in dex rather
/// than catalog-discovered, so key resolution works cache-less on a fresh
/// install (no `dex update --models` needed first). Mirrored in `doctor`'s
/// key-origin row.
fn pinned_key_env(provider: &Provider) -> Option<&'static str> {
    match provider {
        Provider::OpenCode => Some("OPENCODE_API_KEY"),
        Provider::Anthropic => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Landing base URL when nothing explicit (`--base-url`, top-level
/// `base_url:`) is set: the provider's config entry override, then the
/// builtin landing (native providers), then the catalog `api` URL
/// (generic providers). The config entry beats the built-in landing —
/// config-first, no exceptions.
fn landing_base_url_for(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> Option<String> {
    if let Some(url) = entries
        .get(provider.name())
        .and_then(|e| e.base_url.clone())
    {
        return Some(url);
    }
    match provider {
        Provider::Generic(name) => catalog_api(name),
        other => other.default_base_url().map(str::to_string),
    }
}

/// Endpoint table for `/model` routing: builtin endpoints plus, for generic
/// providers, their single catalog/config endpoint under the provider name.
fn endpoints_for(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> BTreeMap<String, String> {
    let mut out = provider.endpoints();
    if let Provider::Generic(name) = provider {
        if let Some(url) = landing_base_url_for(provider, entries) {
            out.insert(name.clone(), url);
        }
    }
    out
}

/// Everything about the active provider derived in one place from the
/// `providers:` map + models.dev catalog. Callers read the fields instead
/// of re-deriving them, so a new provider-scoped concern (a compat flag,
/// a credential flavor, …) is one field here + one line in
/// `resolve_provider` — never a new `(provider, entries)` parameter pair
/// threaded through callers.
#[derive(Clone, Default)]
pub(crate) struct ResolvedProvider {
    /// Fallback endpoint when nothing explicit is set (builtin default, or
    /// the entry override > catalog `api` URL for generics). `None` means
    /// the provider has no known endpoint — callers fail loudly instead of
    /// pointing requests at "".
    pub(crate) landing: Option<String>,
    /// Named endpoints offered to `/model` routing.
    pub(crate) endpoints: BTreeMap<String, String>,
    /// The entry's `api:` protocol pin, if any.
    pub(crate) api_pin: Option<ApiProtocol>,
    /// The entry's provider-scoped `headers:`.
    pub(crate) headers: BTreeMap<String, String>,
}

/// Derive the active provider's runtime view. Infallible by design: a
/// missing landing is `None` (the caller decides whether an explicit
/// `base_url` already covers it), never a silent empty string.
fn resolve_provider(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> ResolvedProvider {
    let entry = entries.get(provider.name());
    ResolvedProvider {
        landing: landing_base_url_for(provider, entries),
        endpoints: endpoints_for(provider, entries),
        api_pin: entry.and_then(|e| e.api),
        headers: entry.map(|e| e.headers.clone()).unwrap_or_default(),
    }
}

/// Neutral first-run hint when nothing selects a provider+model (no
/// `--model`, `DEX_MODEL`, file `model:`, `DEX_PROVIDER`/`active_provider:`
/// with an id). Names no favorite and no model id — the user picks; model
/// ids come from the provider's catalog entry (or `dex doctor`), not a
/// hardcoded default that can rot.
fn setup_guide_error() -> String {
    let path = config_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/dex/config.yaml".to_string());
    format!(
        "no model configured — set 'model: <provider>/<model>' in the config, then run `dex doctor`:\n\
         \u{20}\u{20}opencode: model: zen/<model-id> + providers.opencode.api_key (or OPENCODE_API_KEY)\n\
         \u{20}\u{20}anthropic: model: anthropic/<model-id> + providers.anthropic.api_key (or ANTHROPIC_API_KEY)\n\
         \u{20}\u{20}custom gateway (Bearer + Anthropic wire): model: gateway/<model-id> + providers.gateway: {{base_url: https://gateway.example/v1, api_key, api: anthropic-messages}}\n\
         \u{20}\u{20}codex: model: openai-codex/<model-id> + run `codex --login` (or CODEX_ACCESS_TOKEN)\n\
         config: {path}"
    )
}

/// Per-provider credentials — the uniform deposit order for every provider
/// except codex (which reads its own credential file):
/// 1. `providers.<name>.api_key` in config.yaml,
/// 2. the provider's own conventional env vars from the catalog `env` map
///    (`OPENCODE_API_KEY`, `ZHIPU_API_KEY`, `OPENROUTER_API_KEY`, …).
///
/// Then a loud error naming the deposit places.
pub(crate) fn resolve_credentials(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    if matches!(provider, Provider::OpenAiCodex) {
        return load_codex_credentials();
    }
    let name = provider.name();
    if let Some(key) = entries
        .get(name)
        .and_then(|e| e.api_key.clone())
        .filter(|k| !k.is_empty())
    {
        return Ok((key, None));
    }
    // The provider's own documented env vars; pinned builtin vars (see
    // `pinned_key_env`) work cache-less — the catalog is the source for
    // every other provider.
    let mut env_names: Vec<String> = catalog_env_vars(name);
    if let Some(pinned) = pinned_key_env(provider) {
        if !env_names.iter().any(|v| v == pinned) {
            env_names.insert(0, pinned.to_string());
        }
    }
    for var in &env_names {
        if let Ok(key) = env::var(var) {
            if !key.trim().is_empty() {
                return Ok((key, None));
            }
        }
    }
    Err(format!(
        "no API key for provider '{name}': set providers.{name}.api_key in config.yaml{}",
        if env_names.is_empty() {
            " or export the provider's key env var (run `dex update --models` to learn its name)"
                .to_string()
        } else {
            format!(" or export {}", env_names.join(", "))
        }
    )
    .into())
}

/// CLI overrides for the extension snapshot below (`--model`, `--base-url`,
/// `--header`): the worker-thread `dex.model` view resolves from
/// file+env, which never sees CLI flags, so `main` deposits them here once
/// at startup. Daemon per-request overrides instead flow through
/// `process_turn`, which records the served snapshot directly.
type CliOverrides = (Option<String>, Option<String>, Vec<String>);

static CLI_OVERRIDES: OnceLock<Mutex<CliOverrides>> = OnceLock::new();

/// Deposit the CLI overrides (idempotent: first call wins, later ones are
/// ignored — startup parses args once).
pub(crate) fn set_cli_model_overrides(
    model: Option<String>,
    base_url: Option<String>,
    headers: Vec<String>,
) {
    let slot = CLI_OVERRIDES.get_or_init(Default::default);
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if guard.0.is_none() && guard.1.is_none() && guard.2.is_empty() {
        *guard = (model, base_url, headers);
    }
}

fn cli_overrides() -> (Option<String>, Option<String>, Vec<String>) {
    CLI_OVERRIDES
        .get()
        .map(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
}

/// The current model as extensions see it: no secrets, safe to clone into
/// Lua tables and the change-detection static. `api` is the wire-protocol
/// name (`ApiProtocol::name`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtensionModelSnapshot {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) api: String,
    pub(crate) base_url: String,
}

impl ExtensionModelSnapshot {
    /// `provider/model` selection id, as `model_select` payloads carry it.
    pub(crate) fn id(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// The current model's credentials for `dex.model.auth()`: raw key plus the
/// endpoint and the merged extra headers, so the extension can place auth
/// per provider family (Bearer header, `x-api-key`, `?key=` query). Never
/// logged — the key lives in Lua memory only.
pub(crate) struct ExtensionModelAuth {
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) headers: BTreeMap<String, String>,
}

/// Shared resolution for the snapshot + auth views: provider, entries,
/// endpoint, protocol, and bare model id. Mirrors `from_env`'s precedence
/// (`--model` > `DEX_MODEL` > file `model:`, explicit base_url pins) and
/// `apply_model`'s endpoint routing — without the reqwest client, context
/// window, or any failure a bare model view must not have.
type ExtensionModelParts = (
    Provider,
    BTreeMap<String, ProviderEntry>,
    String,
    ApiProtocol,
    String,
);

fn extension_model_parts() -> Result<ExtensionModelParts, String> {
    let (cli_model, cli_base_url, _) = cli_overrides();
    let file = load_config_file();
    let entries = load_provider_entries(&file);
    let known = known_providers(&entries);
    let raw_selection = resolve_selection(cli_model.filter(|m| !m.trim().is_empty()), &file)
        .map(|r| r.value)
        .map_err(|e| e.to_string())?;
    let (selection_provider, mut model) =
        split_selection(&raw_selection, &known).map_err(|e| e.to_string())?;
    let provider_name = match selection_provider {
        Some(name) => name,
        None => provider_without_prefix(
            provider_fallback_with_origin(&file),
            cli_base_url.as_deref(),
        ),
    };
    let provider = Provider::parse_known(&provider_name, &known)
        .or_else(|| (provider_name == "custom").then(|| Provider::Generic("custom".to_string())))
        .ok_or_else(|| {
            format!(
                "unsupported provider '{provider_name}'; add it under 'providers:' (e.g. providers.custom: {{base_url: ..., api_key: ...}})"
            )
        })?;
    let resolved = resolve_provider(&provider, &entries);
    let file_base_url = load_config_str(&file, "base_url");
    let explicit = cli_base_url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .or(file_base_url);
    let mut base_url = explicit
        .clone()
        .or(resolved.landing.clone())
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "provider '{}' has no base_url: set base_url under providers: or run `dex update --models`",
                provider.name()
            )
        })?;
    if explicit.is_none() {
        // Same routing as `apply_model` on the fallback endpoint: an
        // `endpoint/id` prefix (or a bare id the catalog serves elsewhere)
        // moves the base_url under the model.
        if let Some((name, rest)) = model.split_once('/') {
            if let Some(url) = resolved.endpoints.get(name) {
                base_url = url.clone();
                model = rest.to_string();
            }
        }
        if let Some(url) = catalog_endpoint_for_model(&model, &resolved.endpoints, &base_url) {
            base_url = url;
        }
    } else if let Some((name, rest)) = model.split_once('/') {
        // Pinned endpoint: prefixes name the target but never move the URL.
        if resolved.endpoints.contains_key(name) {
            model = rest.to_string();
        }
    }
    let (base_api, _) = base_protocol(&provider, resolved.api_pin, &file);
    let api = model_api_from_env(&raw_selection, &model)
        .or_else(|| learned_api(&base_url, &model))
        .unwrap_or(base_api);
    Ok((provider, entries, base_url, api, model))
}

/// `dex.model.current()`: provider, bare model id, wire protocol, endpoint.
/// Fails only when nothing selects a model or the provider has no endpoint
/// (same guidance as `from_env`, never a silent default).
pub(crate) fn extension_model_snapshot() -> Result<ExtensionModelSnapshot, String> {
    let (provider, _, base_url, api, model) = extension_model_parts()?;
    Ok(ExtensionModelSnapshot {
        provider: provider.name().to_string(),
        model,
        api: api.name().to_string(),
        base_url,
    })
}

/// `dex.model.auth()`: key + endpoint + merged extra headers (provider
/// `headers:` under file < env < `--header`; `authorization` dropped — the
/// key travels separately). Errors name the deposit places, never the key.
pub(crate) fn extension_model_auth() -> Result<ExtensionModelAuth, String> {
    let (provider, _, base_url, _, _) = extension_model_parts()?;
    extension_model_auth_for(provider.name(), &base_url)
}

/// The current model's wire protocol, same resolution as
/// `extension_model_snapshot` — so `dex.model.auth()` can also say which
/// API the key+endpoint speak without a served snapshot.
pub(crate) fn extension_model_api() -> Result<String, String> {
    let (_, _, _, api, _) = extension_model_parts()?;
    Ok(api.name().to_string())
}

/// `dex.model.auth(provider)`: credentials for an arbitrary configured
/// provider — the model-independent extension vocabulary (a fallback search
/// calls another provider's endpoint with that provider's own key).
/// Endpoint: the entry's `base_url:` > the catalog landing (builtin) / the
/// catalog `api` URL (generic). Wire: the entry's `api:` pin > provider
/// default. Auth: the standard deposit order.
pub(crate) struct ExtensionProviderAuth {
    pub(crate) auth: ExtensionModelAuth,
    pub(crate) api: String,
}

pub(crate) fn extension_provider_auth(
    provider_name: &str,
) -> Result<ExtensionProviderAuth, String> {
    let file = load_config_file();
    let entries = load_provider_entries(&file);
    let known = known_providers(&entries);
    let provider = Provider::parse_known(provider_name, &known).ok_or_else(|| {
        format!(
            "unsupported provider '{provider_name}'; add it under 'providers:' (e.g. providers.{provider_name}: {{base_url: ..., api_key: ...}})"
        )
    })?;
    let resolved = resolve_provider(&provider, &entries);
    let base_url = resolved
        .landing
        .clone()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "provider '{}' has no endpoint: set base_url under providers.{} or run `dex update --models`",
                provider.name(),
                provider.name()
            )
        })?;
    let auth = extension_model_auth_for(provider_name, &base_url)?;
    let (api, _) = base_protocol(&provider, resolved.api_pin, &file);
    Ok(ExtensionProviderAuth {
        auth,
        api: api.name().to_string(),
    })
}

/// A provider with resolvable credentials and a usable endpoint — the
/// `net.providers` vocabulary: `dex.net.fetch` may target these endpoints
/// (each with its own key) when the manifest declares `net.providers`;
/// everything else stays confined to the model's own endpoint.
pub(crate) struct ConfiguredProviderEndpoint {
    pub(crate) provider: String,
    pub(crate) base_url: String,
}

/// Every configured provider that could actually authenticate: builtins
/// count when their key deposits resolve, generics with a file entry count
/// when theirs do (file key or the catalog env var). Sorted by provider
/// spelling (alias spellings dedupe to one canonical entry below). Shared
/// by the `net.providers` fetch allowlist and `dex.model.providers()`.
pub(crate) fn extension_configured_providers() -> Vec<ConfiguredProviderEndpoint> {
    let file = load_config_file();
    let entries = load_provider_entries(&file);
    let known = known_providers(&entries);
    let mut names: BTreeSet<String> = entries.keys().cloned().collect();
    // Builtins need no file entry — the single list lives on `Provider`.
    names.extend(Provider::BUILTINS.iter().map(ToString::to_string));
    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for name in names {
        let Some(provider) = Provider::parse_known(&name, &known) else {
            continue;
        };
        // Alias spellings ("codex"/"openai-codex") resolve to one provider.
        let canonical = provider.name().to_string();
        if !seen.insert(canonical.clone()) {
            continue;
        }
        if resolve_credentials(&provider, &entries).is_err() {
            continue;
        }
        if let Some(base_url) = resolve_provider(&provider, &entries)
            .landing
            .filter(|u| !u.trim().is_empty())
        {
            out.push(ConfiguredProviderEndpoint {
                provider: canonical,
                base_url,
            });
        }
    }
    out
}

/// Auth for an explicit provider + endpoint: what the worker calls with the
/// served snapshot when a turn recorded one (a per-request override the
/// file never sees — the key still resolves from the configured deposits).
/// Unknown names become generic providers so a served custom endpoint keeps
/// working without a file entry.
pub(crate) fn extension_model_auth_for(
    provider_name: &str,
    base_url: &str,
) -> Result<ExtensionModelAuth, String> {
    let (_, _, cli_headers) = cli_overrides();
    let file = load_config_file();
    let entries = load_provider_entries(&file);
    let known = known_providers(&entries);
    let provider = Provider::parse_known(provider_name, &known)
        .unwrap_or_else(|| Provider::Generic(provider_name.to_string()));
    let (api_key, _) = resolve_credentials(&provider, &entries).map_err(|e| e.to_string())?;
    let mut headers = load_config_headers(&file);
    // Provider-scoped entries beat the global table per key (AGENTS.md
    // precedence): overwrite, don't `or_insert`.
    if let Some(entry) = entries.get(provider.name()) {
        for (name, value) in &entry.headers {
            headers.insert(name.clone(), value.clone());
        }
    }
    for (name, value) in custom_headers_from_env() {
        insert_extra_header(&mut headers, &name, &value);
    }
    for raw in &cli_headers {
        insert_parsed_headers(&mut headers, raw);
    }
    headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
    Ok(ExtensionModelAuth {
        api_key,
        base_url: base_url.to_string(),
        headers,
    })
}

/// Persisted wire protocols learned empirically at runtime
/// (`XDG_CACHE_HOME/dex/learned-apis.json`): models that rejected
/// `/responses` and succeeded over `/chat/completions`. Keyed
/// `"<base_url>|<model>"`. Only consulted when nothing explicit pins the
/// protocol. ponytail: no expiry — a model that speaks completions keeps
/// working even after the provider adds responses support.
fn learned_apis_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/learned-apis.json")
}

/// Learned wire protocols, cached process-wide and invalidated by file
/// identity. `from_env` consulted this file on every turn (one read + parse
/// per turn); hits are now a mutex bump.
type LearnedApiMap = serde_json::Map<String, serde_json::Value>;

static LEARNED_CACHE: OnceLock<Mutex<Option<FileCache<LearnedApiMap>>>> = OnceLock::new();

fn learned_api_map() -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = learned_apis_path() else {
        return Default::default();
    };
    // A missing or unparseable file is an empty map (and gets cached as
    // one): learning simply starts over.
    cached_parse(&LEARNED_CACHE, &path, |text| {
        Some(
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                &text.unwrap_or_default(),
            )
            .unwrap_or_default(),
        )
    })
    .unwrap_or_default()
}

fn learned_api(base_url: &str, model: &str) -> Option<ApiProtocol> {
    learned_api_map()
        .get(format!("{base_url}|{model}").as_str())?
        .as_str()
        .and_then(ApiProtocol::parse)
}

/// Best-effort write; a lost race between concurrent learners just re-learns.
pub(crate) fn remember_learned_api(base_url: &str, model: &str, api: ApiProtocol) {
    let Some(path) = learned_apis_path() else {
        return;
    };
    let mut map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    map.insert(
        format!("{base_url}|{model}"),
        serde_json::Value::from(api.name()),
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(path, text);
        // The file changed under us; drop the cached map so the next
        // lookup re-reads instead of serving the pre-write copy.
        if let Some(cache) = LEARNED_CACHE.get() {
            cache.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
}

/// Reasoning-effort options the selected model advertises (models.dev
/// `reasoning_options`, e.g. glm-5.3-flash: low/high/max). Shown in `/model`
/// replies so the thinking knob is discoverable per model; `None` when the
/// catalog has no entry or advertises no effort options.
pub(crate) fn reasoning_options_for(model: &str) -> Option<Vec<String>> {
    // First catalog entry in iteration order wins — the old provider×model
    // walk returned the first provider containing the id, even when that
    // entry advertised no effort options.
    with_catalog_index(|index| {
        index
            .by_id
            .get(model.to_ascii_lowercase().as_str())?
            .iter()
            .find(|e| !e.endpoint_only)
            .and_then(|e| e.reasoning_options.clone())
    })
    .flatten()
}

/// Per-model reasoning effort chosen via `/thinking`
/// (`XDG_CACHE_HOME/dex/thinking-effort.json`): `"<base_url>|<model>"` →
/// effort. Wins over `DEX_THINKING_EFFORT` (a stored choice is more specific
/// than a global). `None` clears the entry.
/// ponytail: read-through, no process cache — the file holds a handful of
/// entries; add file-identity caching like `learned-apis.json` if it grows.
fn thinking_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/thinking-effort.json")
}

fn thinking_map() -> serde_json::Map<String, serde_json::Value> {
    thinking_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_thinking_map(map: &serde_json::Map<String, serde_json::Value>) {
    let Some(path) = thinking_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(map) {
        // Atomic (unique tmp + rename): the daemon re-reads this file per
        // turn; a direct write can hand it torn JSON that then sticks as a
        // cached parse failure until the next write.
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

pub(crate) fn stored_thinking_effort(base_url: &str, model: &str) -> Option<String> {
    thinking_map()
        .get(format!("{base_url}|{model}").as_str())?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Remember (`Some`) or clear (`None`) the `/thinking` choice for one
/// endpoint+model. Best-effort.
pub(crate) fn remember_thinking_effort(base_url: &str, model: &str, effort: Option<&str>) {
    let mut map = thinking_map();
    let key = format!("{base_url}|{model}");
    match effort.filter(|e| !e.is_empty()) {
        Some(effort) => {
            map.insert(key, serde_json::Value::from(effort));
        }
        None => {
            map.remove(&key);
        }
    }
    write_thinking_map(&map);
}

/// Validate a `/thinking` pick against the model's advertised options: the
/// catalog's own casing on match, the raw pick for unknown models (a stale
/// catalog shouldn't block), or the valid options when rejected.
pub(crate) fn validate_thinking_effort(model: &str, pick: &str) -> Result<String, Vec<String>> {
    match reasoning_options_for(model) {
        Some(options) => options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(pick))
            .cloned()
            .ok_or(options),
        None => Ok(pick.to_string()),
    }
}

/// Persist a `/model`/`/provider` selection as the new default under the
/// single canonical key: `model: <endpoint|provider>/<model>`. The prefix
/// is re-derived on load, never stored twice: `endpoint/model` when the
/// current URL is a named endpoint (exact routing survives without a
/// `base_url:`), else `provider/model`. The now-redundant
/// `active_provider:`, legacy `provider:`, and top-level `base_url:` keys
/// are removed so files converge on the one-knob schema — a stale
/// top-level `base_url:` would fight the stored prefix by pinning the old
/// endpoint. Everything else (`api_key`, `api`, `headers`, comments
/// excepted) is preserved verbatim. Best-effort: a read-only or missing
/// file silently skips the write.
/// ponytail: serde_yaml drops comments on write-back; restructure the file
/// if round-tripping comments ever matters.
fn persist_selection(
    selection: &str,
    provider: &Provider,
    base_url: &str,
    endpoints: &BTreeMap<String, String>,
) {
    let Some(path) = config_file_path() else {
        return;
    };
    let mut root: serde_yaml::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_yaml::from_str(&text).ok())
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    if let Some(map) = root.as_mapping_mut() {
        let prefix = endpoints
            .iter()
            .find(|(_, url)| url.as_str() == base_url)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| provider.name().to_string());
        map.insert(
            serde_yaml::Value::from("model"),
            serde_yaml::Value::from(format!("{prefix}/{selection}")),
        );
        for key in ["active_provider", "provider", "base_url"] {
            map.remove(serde_yaml::Value::from(key));
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_yaml::to_string(&root) {
        // Atomic (unique tmp + rename): a co-located reader caches the file
        // by content hash — a direct write can hand it torn YAML, which
        // then sticks as a parse error until the next write.
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        invalidate_config_cache();
    }
}

fn dex_catalog_cache_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/models.dev.json")
}

/// True when no cached models.dev catalog exists yet (fresh install). The
/// daemon fetches it in the background on first start so context windows and
/// `/model` autocomplete work without a manual `dex update --models`.
pub(crate) fn catalog_cache_missing() -> bool {
    dex_catalog_cache_path()
        .map(|p| std::fs::metadata(&p).map(|m| m.len() == 0).unwrap_or(true))
        .unwrap_or(true)
}

/// Slim cross-process context index (`models.ctx.json`): `lowercased model
/// id → context window`. The full catalog is 4+ MB, so every fresh process
/// paid a full read + parse (~180ms) just to look up one model. The index is
/// KBs; warm launches (and every daemon turn) hit it and skip the catalog.
/// Written by `refresh_models_cache` and lazily rebuilt whenever the catalog
/// is newer than the index.
fn dex_ctx_index_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/models.ctx.json")
}

fn ctx_from_index(model: &str) -> Option<u64> {
    // KB-sized file: one small read + parse instead of the 4MB catalog.
    let path = dex_ctx_index_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).ok()?;
    let from_index = map
        .get(model.to_ascii_lowercase().as_str())
        .and_then(|v| v.as_u64())
        .filter(|ctx| *ctx > 0)?;
    // Cross-check against a warm in-process catalog index (a lock + one
    // lookup, never a file read — `if_catalog_index_warm`): the KB file can
    // lag the catalog it was derived from (upstream re-sized a model; the
    // catalog was rewritten between the index write and this read). A
    // mismatch prefers the catalog and rewrites the whole file from the
    // warm index, so one wrong answer self-corrects without paying a cold
    // 4MB parse on the hot path.
    let from_catalog = if_catalog_index_warm(|index| {
        index
            .by_id
            .get(model.to_ascii_lowercase().as_str())
            .and_then(|entries| indexed_context(entries))
    })
    .flatten();
    match from_catalog {
        Some(ctx) if ctx != from_index => {
            if_catalog_index_warm(|index| write_ctx_index(&ctx_map_from_index(index)));
            Some(ctx)
        }
        _ => Some(from_index),
    }
}

/// The context window every reader agrees on: the first non-`endpoint_only`
/// entry in catalog iteration order with a positive `context`. Shared by the
/// slim-index reader (`ctx_from_index`), its repair table
/// (`ctx_map_from_index`) and `build_ctx_map`, so a cold process and a warm
/// one can't disagree and rewrite `models.ctx.json` back and forth.
fn indexed_context(entries: &[IndexedModel]) -> Option<u64> {
    entries
        .iter()
        .filter(|e| !e.endpoint_only)
        .find_map(|e| e.context.filter(|c| *c > 0))
}

/// The full model→context table from the warm catalog index — the repair
/// `ctx_from_index` writes when the slim file disagrees with the catalog,
/// instead of re-parsing the 4MB catalog to rebuild it.
fn ctx_map_from_index(index: &CatalogIndex) -> BTreeMap<String, u64> {
    index
        .by_id
        .iter()
        .filter_map(|(id, entries)| indexed_context(entries).map(|ctx| (id.clone(), ctx)))
        .collect()
}

/// Collect every known `model id → context` pair from the catalog (both the
/// `api.json` providers shape and the `catalog.json` models shape) so the
/// slim index answers without the 4MB parse.
fn build_ctx_map(catalog: &serde_json::Value) -> BTreeMap<String, u64> {
    fn context_of(entry: &serde_json::Value) -> Option<u64> {
        entry
            .get("limit")
            .and_then(|l| l.get("context"))
            .and_then(|c| c.as_u64())
            .filter(|ctx| *ctx > 0)
    }
    let mut map = BTreeMap::new();
    if let Some(providers) = catalog.as_object() {
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                for (id, m) in models {
                    if let Some(ctx) = context_of(m) {
                        map.entry(id.to_ascii_lowercase()).or_insert(ctx);
                    }
                }
            }
        }
    }
    if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
        for (id, m) in models {
            if let Some(ctx) = context_of(m) {
                map.entry(id.to_ascii_lowercase()).or_insert(ctx);
            }
        }
    }
    map
}

/// Unique tmp path for an atomic write: PID plus a per-call counter, so
/// concurrent writers (daemon fetch vs `dex update --models`, or two
/// in-process rebuilds) never share a tmp file — a shared name lets one
/// writer's rename publish another writer's half-written bytes, which is
/// exactly the torn state the rename was meant to prevent. Readers ignore
/// tmp files, so a crashed write just litters one stale file.
fn unique_tmp_path(path: &std::path::Path) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    path.with_extension(format!(
        "json.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Best-effort index write; failures just mean the next launch re-parses.
/// Atomic (unique tmp file + rename) so a concurrent `refresh` never leaves
/// a torn `models.ctx.json` for a reader mid-turn; a stale reader just falls
/// back to the full catalog parse on JSON error.
fn write_ctx_index(map: &BTreeMap<String, u64>) {
    let Some(path) = dex_ctx_index_path() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(map) {
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Rebuild the slim index when it is missing or older than the catalog.
/// Runs only on the slow path (right after the full catalog parse), so warm
/// launches never pay for it.
fn ensure_ctx_index(catalog: &serde_json::Value) {
    let (Some(index_path), Some(catalog_path)) = (dex_ctx_index_path(), dex_catalog_cache_path())
    else {
        return;
    };
    let catalog_mtime = std::fs::metadata(&catalog_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let index_mtime = std::fs::metadata(&index_path)
        .ok()
        .and_then(|m| m.modified().ok());
    let stale = match (catalog_mtime, index_mtime) {
        (Some(c), Some(i)) => c > i,
        _ => true,
    };
    if stale {
        write_ctx_index(&build_ctx_map(catalog));
    }
}

/// Parsed `models.dev.json` catalog, cached process-wide and invalidated by
/// file identity (path + mtime + length). The catalog is 4+ MB and was
/// re-parsed on every `LlmConfig::from_env` — i.e. on each TUI launch (via
/// `/api/config`) and each chat turn (~180ms a pop). Shared through an `Arc`
/// so cache hits are an atomic bump, not a deep clone of the whole tree.
static CATALOG_CACHE: OnceLock<Mutex<Option<FileCache<std::sync::Arc<serde_json::Value>>>>> =
    OnceLock::new();

fn load_dex_catalog() -> Option<std::sync::Arc<serde_json::Value>> {
    let path = dex_catalog_cache_path()?;
    // A corrupt catalog file is not cached: the next `dex update --models`
    // may fix it, and a stale-but-valid copy must never mask a rewrite.
    cached_parse(&CATALOG_CACHE, &path, |text| {
        serde_json::from_str(&text?).ok().map(std::sync::Arc::new)
    })
}

/// models.dev catalog `limit.<key>` for `model` (`context` = window,
/// `output` = generation cap). Matches either cache shape — api.json
/// (per-provider models) or catalog.json (flat models map) — case-
/// insensitively.
/// models.dev catalog `limit.<key>` for `model` (`context` = window,
/// `output` = generation cap). Matches either cache shape — api.json
/// (per-provider models) or catalog.json (flat models map) — case-
/// insensitively. First catalog entry in iteration order wins, as before.
/// A zero `limit` is no limit, so it is filtered like the slim index does.
fn catalog_limit(model: &str, key: &str) -> Option<u64> {
    with_catalog_index(|index| {
        index
            .by_id
            .get(model.to_ascii_lowercase().as_str())?
            .iter()
            .filter(|e| !e.endpoint_only)
            .find_map(|e| match key {
                "context" => e.context,
                "output" => e.output,
                _ => None,
            })
    })
    .flatten()
    .filter(|v| *v > 0)
}

fn catalog_context_window(model: &str) -> Option<u64> {
    catalog_limit(model, "context")
}

/// models.dev `limit.output` for the model — the generation cap wires that
/// must declare one up front (Anthropic `max_tokens`) clamp against. Served
/// from the per-generation index, so safe on hot paths.
pub(crate) fn catalog_output_limit_for(model: &str) -> Option<u64> {
    catalog_limit(model, "output")
}

/// Endpoint URL serving `model` per the models.dev catalog, for bare model
/// picks (`/model kimi-k2.6` with no `endpoint/` prefix). The catalog entry's
/// `api` URL is matched against the provider's endpoint table, so a catalog
/// rename can't silently misroute. Returns `None` (keep the current URL)
/// when the current endpoint already serves the model, the model is unknown,
/// or the current URL is custom (not a known endpoint — explicit wins).
/// ponytail: linear scan of a cached 4MB parse; runs on `/model` switches
/// and on every config rebuild (`from_env` runs per daemon chat turn) —
/// cheap behind the cached parse + endpoint guard.
fn catalog_endpoint_for_model(
    model: &str,
    endpoints: &BTreeMap<String, String>,
    current_base_url: &str,
) -> Option<String> {
    if !endpoints.values().any(|url| url == current_base_url) {
        return None;
    }
    // The index `by_id` vec holds exactly the serving entries (top-level
    // providers, then the `providers`-nested shape) in catalog iteration
    // order — the same sequence the old walk filtered with `serves`.
    with_catalog_index(|index| {
        let entries = index.by_id.get(model.to_ascii_lowercase().as_str())?;
        let mut fallback = None;
        for entry in entries {
            let Some(url) = entry.api.as_deref().filter(|u| !u.is_empty()) else {
                continue;
            };
            if !endpoints.values().any(|known| known == url) {
                continue;
            }
            if url == current_base_url {
                return None;
            }
            if fallback.is_none() {
                fallback = Some(url.to_string());
            }
        }
        fallback
    })
    .flatten()
}

/// Shared 3-tier model-cost lookup for `usage_cost` and
/// `cache_write_read_ratio`: the entry whose `api` matches the configured
/// endpoint wins, then any catalog entry of the configured provider, then
/// any provider at all. Served from the per-generation index — a map lookup
/// plus a scan over one model's entries instead of a full catalog walk.
fn resolve_model_cost(model: &str, provider_keys: &[String], base_url: &str) -> Option<CostRates> {
    let base = base_url.trim_end_matches('/');
    with_catalog_index(|index| {
        let entries = index.by_id.get(model.to_ascii_lowercase().as_str())?;
        // Priced top-level provider entries only: the flat `models` shape
        // and the `providers`-nested shape never fed the old tier walk.
        let priced = |e: &&IndexedModel| !e.endpoint_only && !e.from_flat && e.cost.is_some();
        entries
            .iter()
            .filter(priced)
            .find(|e| {
                e.api
                    .as_deref()
                    .is_some_and(|api| api.trim_end_matches('/') == base)
            })
            .or_else(|| {
                entries
                    .iter()
                    .filter(priced)
                    .find(|e| provider_keys.iter().any(|k| k == &e.provider))
            })
            .or_else(|| entries.iter().find(priced))
            .and_then(|e| e.cost.clone())
    })
    .flatten()
}

/// Cost for one LLM call, using models.dev pricing when available. The same
/// model id is listed by many resellers at different prices, so the entry
/// whose `api` matches the configured endpoint wins, then any catalog entry
/// of the configured provider, then any provider. `input`/`cache_read`/
/// `output` are USD per 1M tokens in the catalog; cache-write tokens are not
/// reported by the OpenAI-compatible endpoints dex speaks, so there is no
/// cache-write term. Cache hits and a missing output rate both fall back to
/// the full input price.
pub(crate) fn usage_cost(
    model: &str,
    provider: &Provider,
    base_url: &str,
    usage: &crate::core::types::Usage,
) -> Option<f64> {
    let keys = provider.catalog_keys();
    let cost = resolve_model_cost(model, &keys, base_url)?;
    let input_rate = cost.input;
    let cache_read_rate = cost.cache_read.unwrap_or(input_rate);
    let output_rate = cost.output.unwrap_or(input_rate);
    let cached = usage.cached_tokens.unwrap_or(0).min(usage.prompt_tokens);
    let fresh = usage.prompt_tokens.saturating_sub(cached);
    #[allow(clippy::cast_precision_loss)]
    let total = fresh as f64 * input_rate / 1_000_000.0
        + cached as f64 * cache_read_rate / 1_000_000.0
        + usage.completion_tokens as f64 * output_rate / 1_000_000.0;
    Some(total)
}

fn load_dex_models_cache() -> Option<Vec<String>> {
    // dex cache is models.dev api.json — expose bare ids plus endpoint-qualified
    // variants (`zen/<id>`, `go/<id>`) so a pick names the endpoint it targets;
    // the prefixes are exactly the names `apply_model` routes on. The bare
    // (id, provider) pairs come from the per-generation index (no catalog
    // walk); the fully expanded list is cached per configured-provider set,
    // so repeat `from_env` calls clone one vec instead of re-sorting.
    let configured: BTreeSet<String> = load_provider_entries(&load_config_file())
        .into_keys()
        .collect();
    with_catalog_index_mut(|index| {
        if let Some(ids) = index.expanded_for.get(&configured) {
            return Some(ids.clone());
        }
        if index.bare.is_empty() {
            return None;
        }
        let mut ids: Vec<String> = Vec::with_capacity(index.bare.len() * 2);
        // Endpoint-qualified prefixes: builtins map to their named
        // endpoints, configured generic providers to their own name. Flat-
        // shape ids (empty provider key) ride bare, as before.
        for (id, prov_key) in index.bare.iter() {
            ids.push(id.clone());
            let dex_prefix: Option<&str> = match prov_key.as_str() {
                "opencode" => Some("zen"),
                "opencode-go" => Some("go"),
                "openai-codex" | "codex" => Some("openai-codex"),
                other => configured.contains(other).then_some(other),
            };
            if let Some(prefix) = dex_prefix.filter(|p| *p != id.as_str()) {
                ids.push(format!("{prefix}/{id}"));
            }
        }
        if ids.is_empty() {
            return None;
        }
        ids.sort();
        ids.dedup();
        if index.expanded_for.len() >= 4 {
            index.expanded_for.clear();
        }
        index.expanded_for.insert(configured, ids.clone());
        Some(ids)
    })
    .flatten()
}

/// Refresh the dex models cache via models.dev.
/// Fetches https://models.dev/api.json (no auth) and caches to
/// XDG_CACHE_HOME/dex/models.dev.json. Next startup uses it for contextWindow
/// and autocomplete without network. Falls back to opencode /models if needed.
pub(crate) async fn refresh_models_cache_async() -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .user_agent(crate::client::http::USER_AGENT)
        // 30s total: api.json is a ~4MB body; the old 10s cap failed on
        // normal slow links while curl (no timeout) succeeded.
        .timeout(Duration::from_secs(30))
        .build()?;
    // Primary: models.dev catalog (provider-agnostic, no auth, has limit.context)
    let mut fetched = false;
    let mut last_err = String::from("no fetch attempted");
    for url in [
        "https://models.dev/api.json",
        "https://models.dev/catalog.json",
    ] {
        let outcome = async {
            let resp = client
                .get(url)
                .send()
                .await
                .map_err(|e| crate::llm::client::error_chain_message(&e))?
                .error_for_status()
                .map_err(|e| crate::llm::client::error_chain_message(&e))?;
            let text = resp
                .text()
                .await
                .map_err(|e| crate::llm::client::error_chain_message(&e))?;
            if serde_json::from_str::<serde_json::Value>(&text).is_err() {
                return Err("response is not valid JSON".to_string());
            }
            let Some(path) = dex_catalog_cache_path() else {
                return Err("no cache dir (set HOME or XDG_CACHE_HOME)".to_string());
            };
            if let Some(parent) = path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            // tmp + rename with a per-call unique tmp name (see
            // `unique_tmp_path`): a reader in another process, or a
            // concurrent daemon/update fetch, must never observe a torn
            // catalog.
            let tmp = unique_tmp_path(&path);
            tokio::fs::write(&tmp, text)
                .await
                .map_err(|e| crate::llm::client::error_chain_message(&e))?;
            tokio::fs::rename(&tmp, &path)
                .await
                .map_err(|e| crate::llm::client::error_chain_message(&e))?;
            println!("cached models.dev {} to {}", url, path.display());
            Ok(())
        }
        .await;
        match outcome {
            Ok(()) => {
                fetched = true;
                break;
            }
            Err(e) => last_err = format!("{url}: {e}"),
        }
    }
    if fetched {
        // Refresh the slim context index too so the next launch skips the
        // 4MB catalog parse.
        if let Some(catalog) = load_dex_catalog() {
            write_ctx_index(&build_ctx_map(&catalog));
        }
        return Ok(());
    }
    Err(format!("could not fetch models.dev catalog: {last_err}").into())
}

/// Sync wrapper for CLI paths that stay sync (`dex update --models`):
/// blocks on the shared runtime handle.
pub(crate) fn refresh_models_cache() -> Result<(), Box<dyn std::error::Error>> {
    crate::client::http::block_on(refresh_models_cache_async())
}

/// Verification opt-in shared by the daemon bootstrap and the one-shot CLI:
/// an explicit `verify_command` always wins; `DEX_VERIFY=1` auto-detects a
/// standard test command from project manifests via
/// [`detect_verify_command`]. Called at the daemon/one-shot boundary (NOT
/// inside the shared agent loop), so a test harness with a real workspace
/// CWD cannot accidentally re-run the project's own test suite mid-turn.
pub(crate) fn apply_verify_optin(config: &mut LlmConfig) {
    if config.verify_command.is_none() && env::var("DEX_VERIFY").as_deref() == Ok("1") {
        config.verify_command = detect_verify_command();
    }
}

/// Auto-detect a verification command from workspace manifests: the first
/// recognized one (Cargo.toml, go.mod, package.json) selects the standard
/// test command.
pub(crate) fn detect_verify_command() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("Cargo.toml").exists() {
        return Some("cargo test".into());
    }
    if cwd.join("go.mod").exists() {
        return Some("go test ./...".into());
    }
    if cwd.join("package.json").exists() {
        return Some("npm test".into());
    }
    None
}

/// Dex-owned per-model table (`DEX_MODEL_APIS="id=api,..."`, e.g.
/// `"kimi-k2.6=openai-completions"`); full `endpoint/id` selection first,
/// then the bare id. None keeps the current/global api.
pub(crate) fn model_api_from_env(selection: &str, bare_id: &str) -> Option<ApiProtocol> {
    let raw = env::var("DEX_MODEL_APIS").ok()?;
    let mut bare = None;
    for entry in raw.split(',') {
        let Some((id, name)) = entry.split_once('=') else {
            continue;
        };
        // Full `endpoint/id` selection wins regardless of entry order.
        if id.trim() == selection {
            if let Some(api) = ApiProtocol::parse(name.trim()) {
                return Some(api);
            }
        } else if bare.is_none() && id.trim() == bare_id {
            bare = ApiProtocol::parse(name.trim());
        }
    }
    bare
}

pub(crate) fn permission_from_env() -> Result<PermissionMode, Box<dyn std::error::Error>> {
    let value = env::var("DEX_PERMISSION")
        .ok()
        .unwrap_or_else(|| "trusted".to_string());
    PermissionMode::parse(&value).map_err(Into::into)
}

/// Parse one custom-header value into `name -> value` pairs.
///
/// Accepts a JSON object (`{"X-Foo":"bar"}` — `headers` map /
/// `http_headers` shape) or `Name: Value` / `Name=Value` pairs separated by
/// commas or newlines (the `ANTHROPIC_CUSTOM_HEADERS` shape).
/// Entries without a name, without a separator, or with an empty value are
/// skipped; later entries win on duplicate names (case-insensitive, last
/// casing wins). `authorization` is dropped (the api key owns it) and a
/// `{...}` value that isn't a JSON object falls back to pair parsing instead
/// of silently yielding nothing.
pub(crate) fn parse_headers_str(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return out;
    }
    if trimmed.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            for (key, value) in map {
                let name = key.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let val = match &value {
                    serde_json::Value::String(s) => s.trim().to_string(),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.to_string(),
                    _ => continue,
                };
                if val.is_empty() {
                    continue;
                }
                insert_extra_header(&mut out, &name, &val);
            }
            return out;
        }
        // Not a JSON object (`{bad`, a JSON array, …) — fall through to
        // pair parsing instead of silently dropping everything. Outer braces
        // are stripped so `{X-Foo: bar}` still yields `X-Foo`.
    }
    let mut body = trimmed;
    if body.starts_with('{') {
        body = body
            .strip_prefix('{')
            .unwrap_or(body)
            .strip_suffix('}')
            .unwrap_or(body)
            .trim();
        if body.is_empty() {
            return out;
        }
    }
    // Newline-separated values may themselves contain commas, so only split
    // on commas when the value is a single line.
    let pieces: Vec<&str> = if body.contains('\n') {
        body.split('\n').collect()
    } else {
        body.split(',').collect()
    };
    for piece in pieces {
        let piece = piece.trim().trim_end_matches(',').trim();
        if piece.is_empty() {
            continue;
        }
        let split = piece.split_once(':').or_else(|| piece.split_once('='));
        let Some((name, value)) = split else {
            continue;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        insert_extra_header(&mut out, &name, &value);
    }
    out
}

/// Scalar YAML value as a header value string (`"abc"`, `42`, `true`).
/// Empty strings yield `None` so blank entries are skipped.
fn yaml_scalar_str(v: &serde_yaml::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_f64().map(|n| n.to_string()))
        .or_else(|| v.as_bool().map(|b| b.to_string()))
        .filter(|s| !s.is_empty())
}

/// Insert one header with case-insensitive "later wins" semantics: a later
/// `x-foo` replaces an earlier `X-Foo` (last casing wins). Empty names/values
/// and `authorization` (the api key owns that) are skipped so a bad entry
/// can never poison the map — send-time filtering remains as defense in depth.
pub(crate) fn insert_extra_header(out: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return;
    }
    if name.eq_ignore_ascii_case("authorization") {
        return;
    }
    if let Some(existing) = out.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned() {
        if existing != name {
            out.remove(&existing);
        }
    }
    out.insert(name.to_string(), value.to_string());
}

/// Console Go routing affinity: the zen/go endpoint rejects requests without
/// `x-opencode-session` (`MissingSessionID`): gated to the opencode provider or an opencode.ai
/// base URL, filled from the dex session id. Keys already present (any
/// casing) are left alone — including in the two file layers, which merge
/// BELOW `extra_headers` on the wire — so explicit user headers always win
/// regardless of call order.
pub(crate) fn apply_opencode_session_headers(config: &mut LlmConfig, session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let base_url = config.base_url.as_str();
    let is_opencode = matches!(config.provider, Provider::OpenCode)
        || reqwest::Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .is_some_and(|h| h.eq_ignore_ascii_case("opencode.ai"));
    if !is_opencode {
        return;
    }
    // A name pinned in ANY layer — file (`global_headers`/`provider_headers`)
    // or env/CLI/per-request (`extra_headers`) — suppresses the auto-fill:
    // the function writes into `extra_headers`, which merges last, so a
    // file-level user header would otherwise lose to it. Resolve both
    // predicates before mutating (they borrow `config` immutably).
    let taken = |name: &str| {
        config
            .global_headers
            .keys()
            .chain(config.provider_headers.keys())
            .chain(config.extra_headers.keys())
            .any(|k| k.eq_ignore_ascii_case(name))
    };
    let session_taken = taken("x-opencode-session");
    let client_taken = taken("x-opencode-client");
    if !session_taken {
        config
            .extra_headers
            .insert("x-opencode-session".to_string(), session_id.to_string());
    }
    if !client_taken {
        config
            .extra_headers
            .insert("x-opencode-client".to_string(), "dex".to_string());
    }
}

fn merge_config_headers_map(out: &mut BTreeMap<String, String>, map: &serde_yaml::Mapping) {
    for (k, v) in map {
        let name = k.as_str().unwrap_or_default();
        if let Some(val) = yaml_scalar_str(v) {
            insert_extra_header(out, name, &val);
        }
    }
}

/// Custom headers from one config file key. Accepts a mapping,
/// a text-header block (`"X-Foo: bar\nX-Baz: qux"`, same syntax as
/// the env vars / `--header`), or a list mixing both. Later entries win.
/// Parse a raw header string ("K: V" pairs or a JSON object) and merge it
/// into `out` under the standard precedence rules. One funnel for every
/// `parse_headers_str` call site.
fn insert_parsed_headers(out: &mut BTreeMap<String, String>, raw: &str) {
    for (k, v) in parse_headers_str(raw) {
        insert_extra_header(out, &k, &v);
    }
}

/// Merge one YAML header value — a mapping or a raw "K: V" text — into
/// `out`. Other scalar shapes are ignored, as before.
fn merge_one(out: &mut BTreeMap<String, String>, value: &serde_yaml::Value) {
    if let Some(map) = value.as_mapping() {
        merge_config_headers_map(out, map);
    } else if let Some(text) = value.as_str() {
        insert_parsed_headers(out, text);
    }
}

fn config_headers_map(file: &Option<serde_yaml::Value>, key: &str) -> BTreeMap<String, String> {
    let Some(value) = file.as_ref().and_then(|f| f.get(key)) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(items) = value.as_sequence() {
        for item in items {
            merge_one(&mut out, item);
        }
    } else {
        merge_one(&mut out, value);
    }
    out
}

/// Custom headers from the config file. `headers:` wins per-key over
/// `http_headers:` when both set the same name. Both keys are
/// deprecated top-level mirrors of provider state — still global, with a
/// one-time pointer at the canonical spots.
fn load_config_headers(file: &Option<serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut out = config_headers_map(file, "http_headers");
    let file_headers = config_headers_map(file, "headers");
    if out.is_empty() && file_headers.is_empty() {
        return out;
    }
    if !out.is_empty() {
        warn_once(
            "config:http_headers",
            "top-level config key 'http_headers:' is deprecated — use 'headers:' under the provider's entry in 'providers:' (provider-scoped) or DEX_HEADERS (global)",
        );
    }
    if !file_headers.is_empty() {
        warn_once(
            "config:headers",
            "top-level config key 'headers:' is deprecated — use 'headers:' under the provider's entry in 'providers:' (provider-scoped) or DEX_HEADERS (global)",
        );
    }
    for (k, v) in file_headers {
        insert_extra_header(&mut out, &k, &v);
    }
    out
}

/// Custom headers from the environment. Later sources win per-key:
/// `ANTHROPIC_CUSTOM_HEADERS` (claude) < `OPENAI_HEADERS` < `DEX_HEADERS`.
/// The first two are deprecated aliases — honored, with a one-time notice
/// pointing at `DEX_HEADERS`.
pub(crate) fn custom_headers_from_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ["ANTHROPIC_CUSTOM_HEADERS", "OPENAI_HEADERS", "DEX_HEADERS"] {
        if let Ok(raw) = env::var(key) {
            if key != "DEX_HEADERS" && !raw.trim().is_empty() {
                warn_once(
                    key,
                    &format!("env var {key} is deprecated — use DEX_HEADERS (same syntax)"),
                );
            }
            insert_parsed_headers(&mut out, &raw);
        }
    }
    out
}

#[derive(Clone)]
pub(crate) struct LlmConfig {
    pub(crate) provider: Provider,
    pub(crate) api_key: String,
    pub(crate) base_url: String,
    pub(crate) model: String,
    pub(crate) available_models: Vec<String>,
    pub(crate) endpoints: BTreeMap<String, String>,
    /// Configured generic providers (`providers:` map): the deposit place for
    /// per-provider keys plus optional base_url/`api:` overrides.
    pub(crate) provider_entries: BTreeMap<String, ProviderEntry>,
    /// The active provider's scoped `headers:`, refreshed on every provider
    /// Switch. Provider-scoped beats the global file table per key; env
    /// and `--header` extras still win over both.
    pub(crate) provider_headers: BTreeMap<String, String>,
    pub(crate) api: ApiProtocol,
    /// True when the user pinned the wire protocol globally: a top-level
    /// file `api:` or the active provider's entry `api:`. Baked once in
    /// `from_env` so hot paths (`streaming`, `client`) never re-read the
    /// file. Per-model `DEX_MODEL_APIS` entries don't count — they decide
    /// per model, but empirical fallback stays available for unlisted models.
    pub(crate) api_pinned: bool,
    pub(crate) account_id: Option<String>,
    pub(crate) thinking_effort: Option<String>,
    /// Model context window size in tokens (used for compaction + status bar).
    /// Per-model (models.dev catalog) unless overridden by DEX_CONTEXT_WINDOW / file.
    pub(crate) context_window: u64,
    pub(crate) reserve_tokens: u64,
    pub(crate) keep_recent_tokens: u64,
    pub(crate) permission: PermissionMode,
    pub(crate) verify_command: Option<String>,
    /// Extra HTTP headers sent on every provider request (gateway auth,
    /// routing, attribution). env (`ANTHROPIC_CUSTOM_HEADERS`/`OPENAI_HEADERS`/
    /// `DEX_HEADERS`) < `--header` / per-request overrides — these beat both
    /// file layers on the wire (see `merged_headers`). The global file
    /// `headers:`/`http_headers:` table lives in `global_headers`. Never
    /// carries `authorization` (the api key owns that) — dropped at send time.
    pub(crate) extra_headers: BTreeMap<String, String>,
    /// Global config-file `headers:`/`http_headers:` (the lowest precedence
    /// layer: provider-scoped file headers and env/CLI extras both beat it,
    /// per key). Split from `extra_headers` so the wire merge in
    /// `merged_headers` can order the three layers exactly like
    /// `extension_model_auth_for` does.
    pub(crate) global_headers: BTreeMap<String, String>,
    pub(crate) client: reqwest::Client,
}

/// Base wire protocol + baked pin from the provider entry's `api:` pin and
/// the global file `api:`. Single derivation shared by `from_env` and every
/// provider switch; per-model overrides (`DEX_MODEL_APIS`, learned) apply
/// on top via `resolve_model_api`. Native providers carry a built-in
/// default (`anthropic` → anthropic-messages); OpenAI-compatible ones
/// fall back to the responses default with the empirical completions
/// fallback.
fn base_protocol(
    provider: &Provider,
    api_pin: Option<ApiProtocol>,
    file: &Option<serde_yaml::Value>,
) -> (ApiProtocol, bool) {
    let file_pin = load_config_str(file, "api");
    let api = api_pin
        .or(file_pin.as_deref().and_then(ApiProtocol::parse))
        .or_else(|| provider.default_api())
        .unwrap_or(ApiProtocol::Responses);
    (api, file_pin.is_some() || api_pin.is_some())
}

/// Nothing anywhere names a provider/model/endpoint: the "no provider
/// configured" guide is the honest error then, not "opencode is broken".
/// Shares `from_env`'s own check so the setup error and resolution agree.
fn selection_is_unconfigured(
    model_override: Option<&str>,
    base_url_override: Option<&str>,
    file: &Option<serde_yaml::Value>,
) -> bool {
    model_override.map(|m| m.trim().is_empty()).unwrap_or(true)
        && env::var("DEX_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .is_none()
        && load_config_str(file, "model").is_none()
        && env::var("DEX_PROVIDER")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_none()
        && load_provider_name(file).is_none()
        // `--base-url` is a setup pointer too: the user configured an endpoint,
        // so a missing key names providers.custom, not the generic guide.
        && base_url_override.is_none()
}

/// A provider name as configured: a known provider, or the URL-backed
/// `custom` pseudo-provider (a bare `--base-url` route lands there).
fn provider_from_name(name: &str, known: &BTreeSet<String>) -> Option<Provider> {
    Provider::parse_known(name, known)
        .or_else(|| (name == "custom").then(|| Provider::Generic("custom".to_string())))
}

/// The `provider`/`model` pair for the selection. When nothing selected a
/// model, still resolve the provider the same way `from_env` would (deprecated
/// pointer > the `--base-url` custom route > the opencode landing default) so
/// the key/endpoint error names the right deposit; the missing-model guide
/// comes last.
fn selection_provider_and_model(
    selection: &Result<Resolved<String>, String>,
    known: &BTreeSet<String>,
    file: &Option<serde_yaml::Value>,
    base_url_override: Option<&str>,
    provider_entries: &BTreeMap<String, ProviderEntry>,
    using_builtin_default: bool,
) -> Result<(Option<String>, String), Box<dyn std::error::Error>> {
    let Ok(resolved) = selection else {
        let guide = selection.as_ref().err().expect("selection is Err");
        let (fallback, origin) = provider_fallback_with_origin(file);
        let name = provider_without_prefix((fallback, origin), base_url_override);
        let provider = provider_from_name(&name, known).ok_or_else(|| guide.clone())?;
        let key_err = resolve_credentials(&provider, provider_entries)
            .err()
            .map(|e| {
                if using_builtin_default {
                    guide.clone()
                } else {
                    e.to_string()
                }
            });
        return Err(key_err.unwrap_or_else(|| guide.clone()).into());
    };
    let (provider, model) = split_selection(&resolved.value, known)?;
    Ok((provider, model))
}

/// `DEX_MODELS` (comma list) as the starting served-model list.
fn env_models() -> Vec<String> {
    env::var("DEX_MODELS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Streaming generations must not have a total request timeout: reqwest's
/// `.timeout()` covers the whole SSE body, killing long generations with
/// `error decoding response body`. The default shares the process-wide
/// streaming client (connect timeout only); an explicit
/// DEX_HTTP_REQUEST_TIMEOUT_SECS still builds a bounded backstop client.
fn http_client(connect_secs: u64, request_secs: u64) -> Result<reqwest::Client, reqwest::Error> {
    if connect_secs == 10 && request_secs == 300 {
        Ok(crate::client::http::shared_streaming_client())
    } else {
        reqwest::Client::builder()
            .user_agent(crate::client::http::USER_AGENT)
            .connect_timeout(Duration::from_secs(connect_secs))
            .timeout(Duration::from_secs(request_secs))
            // Same dead-socket detection as the shared streaming client.
            .tcp_keepalive(Duration::from_secs(crate::client::http::TCP_KEEPALIVE_SECS))
            .build()
    }
}

impl LlmConfig {
    pub(crate) fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
        header_overrides: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        crate::tools::set_output_limit(env_parse("DEX_TOOL_OUTPUT_BYTES", 1_048_576));
        let permission = match permission_override {
            Some(mode) => mode,
            None => permission_from_env()?,
        };
        let file = load_config_file();
        // Custom headers, three layers so the wire merge can order them like
        // `extension_model_auth_for` (AGENTS.md precedence): global file table
        // < provider-scoped < env < CLI. `extra_headers` carries env+CLI (and
        // later per-request overrides); the file's global table rides in
        // `global_headers`.
        let global_headers = load_config_headers(&file);
        let mut extra_headers = BTreeMap::new();
        for (k, v) in custom_headers_from_env() {
            insert_extra_header(&mut extra_headers, &k, &v);
        }
        for raw in header_overrides {
            insert_parsed_headers(&mut extra_headers, raw);
        }
        let provider_entries = load_provider_entries(&file);
        let known = known_providers(&provider_entries);
        // Untouched builtin default (no flag/env/file pointer anywhere):
        // a missing key then means "nothing configured", not "opencode
        // is broken" — the error guides setup instead of endorsing one
        // provider.
        let using_builtin_default = selection_is_unconfigured(
            model_override.as_deref(),
            base_url_override.as_deref(),
            &file,
        );
        // One selection knob names provider *and* model: `provider/model`
        // (`endpoint/model` or a bare provider name work too). Precedence:
        // `--model` > `DEX_MODEL` > file `model:` — nothing set anywhere is
        // a setup error, not a silent builtin default. When the selection
        // carries no provider, `DEX_PROVIDER` / `active_provider:` (both
        // deprecated) still pick one. Model resolution is deferred to just
        // before the build: credential/endpoint problems are reported
        // first, they are the more actionable fix.
        let selection = resolve_selection(model_override, &file);
        let (selection_provider, model) = selection_provider_and_model(
            &selection,
            &known,
            &file,
            base_url_override.as_deref(),
            &provider_entries,
            using_builtin_default,
        )?;
        let provider_name = match selection_provider.as_deref() {
            Some(name) => name.to_string(),
            None => provider_without_prefix(
                provider_fallback_with_origin(&file),
                base_url_override.as_deref(),
            ),
        };
        // `custom` from `provider_without_prefix` is URL-backed even when
        // no `providers.custom:` entry exists yet; parse_known already
        // covers the configured case, this catches the bare `--base-url`
        // route so the key error names the right deposit.
        let provider = provider_from_name(&provider_name, &known).ok_or_else(|| {
            format!(
                "unsupported provider '{provider_name}'; use opencode, openai-codex, anthropic, or add it under 'providers:' (e.g. providers.custom: {{base_url: ..., api_key: ...}})"
            )
        })?;
        let mut available_models = env_models();
        let file_base_url = load_config_str(&file, "base_url");
        if file_base_url.is_some() {
            warn_once(
                "config:base_url",
                "top-level config key 'base_url:' is deprecated — set 'base_url:' under the provider's entry in 'providers:'",
            );
        }
        // An explicit base_url (`--base-url`, file `base_url:`) pins the
        // endpoint — routing may not silently rewire what the user set.
        // Only the fallback default (nothing set anywhere) routes.
        let explicit_base_url = base_url_override.is_some() || file_base_url.is_some();
        // One derivation for everything provider-scoped; an explicit
        // base_url (CLI/file) pins the endpoint and covers a missing
        // landing, otherwise the landing is required — fail loudly instead
        // of pointing requests at "".
        let resolved = resolve_provider(&provider, &provider_entries);
        let base_url = base_url_override
            .or(file_base_url)
            .filter(|v| !v.is_empty())
            .or(resolved.landing.clone())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                format!(
                    "provider '{}' has no base_url: set base_url under providers: or run `dex update --models`",
                    provider.name()
                )
            })?;
        // Wire protocol default: the active provider's `api:` entry pin,
        // else the global file `api:`, else responses. A typo'd file `api:`
        // fails fast instead of silently defaulting.
        if let Some(name) = load_config_str(&file, "api") {
            warn_once(
                "config:api",
                "top-level config key 'api:' is deprecated — set 'api:' under the provider's entry in 'providers:'",
            );
            ApiProtocol::parse(&name).ok_or_else(|| {
                format!("unsupported api '{name}'; use openai-completions, openai-responses or anthropic-messages")
            })?;
        }
        // Baked once: hot paths read the fields instead of re-reading the
        // file on every request.
        let (api, api_pinned) = base_protocol(&provider, resolved.api_pin, &file);
        // The model carries its own wire protocol; the global `api` above is
        // only the default. Otherwise the `apply_model` call below resolves
        // (full `endpoint/id` key, then bare id, then learned fallback).
        let (api_key, account_id) =
            resolve_credentials(&provider, &provider_entries).map_err(|e| {
                if using_builtin_default {
                    setup_guide_error().into()
                } else {
                    e
                }
            })?;

        // Reserve 16384, keep 20000 tokens recent (not 12 messages)
        let reserve_tokens = env_parse("DEX_RESERVE_TOKENS", 16_384);
        let keep_recent_tokens = env_parse("DEX_KEEP_RECENT_TOKENS", 20_000);
        let connect_secs: u64 = env_parse("DEX_HTTP_CONNECT_TIMEOUT_SECS", 10);
        let request_secs: u64 = env_parse("DEX_HTTP_REQUEST_TIMEOUT_SECS", 300);
        let client = http_client(connect_secs, request_secs)?;
        // Dex standalone: no network at startup — models come from config/DEX_MODELS
        // or `dex update --models` cache (XDG_DATA_HOME/dex/models.json). Removed
        // live /models fetch (was 5s+ blocking per endpoint).
        // Dex own cache — bootstraps with just current model if missing.
        if available_models.is_empty() {
            if let Some(cached) = load_dex_models_cache() {
                available_models = cached;
            }
        }
        // Before the model is force-inserted below (so the served list stays
        // honest): a bare provider-like name is about to ride this provider
        // as a model id.
        warn_provider_like_selection(&model, &provider_name, &available_models);
        if !available_models.iter().any(|candidate| candidate == &model) {
            available_models.insert(0, model.clone());
        }
        let mut this = Self {
            provider,
            api_key,
            base_url,
            model: model.clone(),
            available_models,
            api,
            account_id,
            thinking_effort: None, // resolved below once model+endpoint are final
            context_window: 0,     // resolved below once model+endpoint are final
            reserve_tokens,
            keep_recent_tokens,
            verify_command: env::var("DEX_VERIFY").ok(),
            permission,
            extra_headers,
            global_headers,
            client,
            endpoints: resolved.endpoints,
            provider_entries,
            provider_headers: resolved.headers,
            api_pinned,
        };
        // A prefixed model override (--model go/foo or the daemon's
        // per-request model) routes to its endpoint; bare ids keep the
        // resolved base_url. An explicit base_url (--base-url, file
        // `base_url:`) pins the endpoint: prefixes are naming only and
        // stripped, never re-routed.
        if !explicit_base_url {
            this.apply_model(&model, false)?;
        } else {
            // The pinned endpoint only fixes routing, never the protocol:
            // resolve from the original selection first (stripping below
            // drops an `endpoint/id` prefix a full-key table entry may
            // name), so a per-model pin and the learned fallback still
            // apply instead of silently falling back to the global default.
            let selection = model.clone();
            this.strip_routing_prefixes();
            if let Some(api) = this.resolve_model_api(&selection) {
                this.api = api;
            }
        }
        // Context window, resolved on the final (routed/stripped) model id:
        // DEX_CONTEXT_WINDOW > file `context_window:` > the slim
        // cross-process index > the catalog. No built-in default — a model
        // nothing sizes is a config error, not a silent 128k guess.
        this.context_window = resolve_context_window(&this.model, &file)?;
        this.refresh_thinking_effort();
        Ok(this)
    }

    /// Async entry for the daemon turn: same precedence/errors as `from_env`,
    /// but off the runtime worker. Cache hits are a mutex bump (inline);
    /// on miss the 4MB catalog parse runs in `spawn_blocking`.
    pub(crate) async fn from_env_async(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
        header_overrides: Vec<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        tokio::task::spawn_blocking(move || {
            Self::from_env(
                base_url_override,
                model_override,
                permission_override,
                &header_overrides,
            )
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.to_string().into() })?
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
    }

    /// Wire protocol for a fresh model selection: an explicit per-model
    /// table entry wins, otherwise a previously learned fallback for this
    /// `(base_url, model)`, otherwise the configured default is kept
    /// (`None`).
    fn resolve_model_api(&self, selection: &str) -> Option<ApiProtocol> {
        if let Some(api) = model_api_from_env(selection, &self.model) {
            return Some(api);
        }
        learned_api(&self.base_url, &self.model)
    }

    /// Apply a `/model` selection. `provider/model` switches provider (and its
    /// base_url) when `provider` parses as a known provider; `endpoint/model`
    /// routes to the named endpoint's base_url; a bare id keeps the current
    /// endpoint when it serves the model (per the models.dev catalog) and
    /// moves to the serving endpoint otherwise — so the pick, not the user,
    /// owns the base_url. The wire protocol follows the same way
    /// (`DEX_MODEL_APIS`, then learned, then the global default) with the
    /// empirical responses→completions fallback as the last resort.
    /// Strip provider/endpoint prefixes without side effects: no provider
    /// switch, no credential lookup, no `base_url` change. Used on load when
    /// the endpoint is explicitly pinned — the pin wins, so a stored
    /// `provider/…` or `endpoint/…` selection names its target but must not
    /// move the pinned URL. Unknown prefixes stay part of the model id.
    fn strip_routing_prefixes(&mut self) {
        let mut sel = self.model.clone();
        if let SelectionRoute::ProviderQualified { rest, .. } =
            classify_selection(&sel, &known_providers(&self.provider_entries))
        {
            sel = rest;
        }
        if let Some((name, rest)) = sel.split_once('/') {
            if self.endpoints.contains_key(name) {
                sel = rest.to_string();
            }
        }
        self.model = sel;
    }

    /// Provider prefix is stripped first, then endpoint routing runs on the
    /// remainder. Returns the endpoint name when an endpoint route was
    /// taken, or an error when a provider-qualified pick names a provider
    /// with no credentials/endpoint — state is left untouched then, so one
    /// provider's key can never leak to another's endpoint. With `persist`,
    /// the stripped id is written back to the config file (it becomes the
    /// new default).
    pub(crate) fn apply_model(
        &mut self,
        selection: &str,
        persist: bool,
    ) -> Result<Option<String>, String> {
        let prev_model = self.model.clone();
        let prev_provider = self.provider.clone();
        // Provider-qualified: "opencode/gpt-..." or "openai-codex/gpt-..."
        // switches provider (and its base_url) without env. A bare provider
        // name ("/model opencode") switches provider and keeps the model —
        // it names a provider, not a model id.
        let sel: String;
        let mut keep_model = false;
        let known = known_providers(&self.provider_entries);
        let route_provider = match classify_selection(selection, &known) {
            SelectionRoute::ProviderQualified { provider, rest } => {
                if rest.is_empty() {
                    // Original kept the full selection visible to the
                    // endpoint-routing step below.
                    keep_model = true;
                    sel = selection.to_string();
                } else {
                    sel = rest;
                }
                Some(provider)
            }
            SelectionRoute::BareProvider { provider } => {
                // A bare provider keeps the current model.
                keep_model = true;
                sel = self.model.clone();
                Some(provider)
            }
            SelectionRoute::Model(model) => {
                sel = model;
                None
            }
        };
        let new_provider = route_provider.and_then(|name| Provider::parse_known(&name, &known));
        if let Some(new_provider) = new_provider {
            if new_provider != self.provider {
                // Resolve everything before mutating: a half-switched
                // config (new provider, old key/URL/headers) is worse
                // than no switch at all. Generic providers land on
                // their config entry / catalog URL.
                let (key, account) = resolve_credentials(&new_provider, &self.provider_entries)
                    .map_err(|e| e.to_string())?;
                let resolved = resolve_provider(&new_provider, &self.provider_entries);
                self.set_provider(new_provider, key, account, resolved)?;
            }
        }
        let mut result = None;
        let mut routed = false;
        if let Some((name, rest)) = sel.split_once('/') {
            if let Some(url) = self.endpoints.get(name).cloned() {
                self.base_url = url;
                self.model = rest.to_string();
                result = Some(name.to_string());
                routed = true;
            }
        }
        if !routed && !keep_model {
            // Bare id (possibly with provider-native slashes like
            // `moonshotai/kimi-k2.6`): stay unless the catalog shows the
            // model lives on another known endpoint.
            self.model = sel.to_string();
            warn_provider_like_selection(&sel, self.provider.name(), &self.available_models);
            if let Some(url) =
                catalog_endpoint_for_model(&self.model, &self.endpoints, &self.base_url)
            {
                result = self
                    .endpoints
                    .iter()
                    .find_map(|(name, known)| (*known == url).then(|| name.clone()));
                self.base_url = url;
            }
        }
        // The model carries its wire protocol; the global `api` is the
        // fallback and the global pin keeps it.
        if let Some(api) = self.resolve_model_api(selection) {
            self.api = api;
        }
        if persist && (self.model != prev_model || self.provider != prev_provider) {
            persist_selection(&self.model, &self.provider, &self.base_url, &self.endpoints);
        }
        // Refresh from the models.dev catalog unless the env or the file
        // pinned the window; without a catalog hit the previous value (a
        // configured key or the prior model's resolution) stands.
        if env::var("DEX_CONTEXT_WINDOW").is_err()
            && load_config_num(&load_config_file(), "context_window").is_none()
        {
            if let Some(ctx) = catalog_context_window(&self.model) {
                self.context_window = ctx;
            }
        }
        // Effort follows the final model+endpoint: stored `/thinking`
        // choice, else `DEX_THINKING_EFFORT`.
        self.refresh_thinking_effort();
        Ok(result)
    }

    /// Trigger compaction when prompt exceeds this many tokens.
    /// contextWindow - reserveTokens (16384) leaves room for reply.
    pub(crate) fn compaction_threshold(&self) -> u64 {
        self.context_window.saturating_sub(self.reserve_tokens)
    }

    /// Cache-write / cache-read price ratio for the configured model, from
    /// the models.dev catalog when it prices the model. Feeds the online
    /// compaction economics. Billing follows `usage_cost`: cache reads (and
    /// writes) with no catalog rate bill at the plain input rate, so the
    /// ratio is `write_rate / read_rate` — an explicit write surcharge
    /// (Anthropic-style), `input / cache_read` when writes bill at the
    /// plain input rate (the industry norm), and 1.0 when the provider
    /// prices no caching at all (reads bill at input price, so re-writing
    /// after a compaction costs the same as reading it). The measured
    /// cross-provider fallback only covers models the catalog doesn't
    /// price.
    pub(crate) fn cache_write_read_ratio(&self) -> f64 {
        const FALLBACK: f64 = crate::agent::online_compaction::DEFAULT_CACHE_WRITE_READ_RATIO;
        let keys = self.provider.catalog_keys();
        let Some(cost) = resolve_model_cost(&self.model, &keys, &self.base_url) else {
            return FALLBACK;
        };
        let input_rate = (cost.input > 0.0).then_some(cost.input);
        // Same unbilled-rate assumptions as `usage_cost`: a missing rate
        // bills at the input price, so `write / read` covers every pricing
        // shape the catalog actually carries.
        let read_rate = cost.cache_read.or(input_rate);
        let write_rate = cost.cache_write.or(input_rate);
        match (write_rate, read_rate) {
            (Some(write), Some(read)) if write > 0.0 && read > 0.0 => write / read,
            _ => FALLBACK,
        }
    }

    pub(crate) fn keep_recent_tokens(&self) -> u64 {
        self.keep_recent_tokens
    }

    /// Reasoning effort for the current model: a stored `/thinking` choice
    /// wins, then `DEX_THINKING_EFFORT`, then file `thinking_effort:`, else
    /// unset. Never prints: this runs on daemon threads while the TUI owns
    /// the terminal (alternate screen + raw mode + OSC theme query), where
    /// an `eprintln!` corrupts the display and leaks into the composer.
    /// Callers surface `thinking_mismatch_warning()` through the transcript
    /// (`DaemonInfo.thinking_warning`) or stderr when no TUI is active.
    pub(crate) fn refresh_thinking_effort(&mut self) {
        let effort = stored_thinking_effort(&self.base_url, &self.model)
            .or_else(|| {
                env::var("DEX_THINKING_EFFORT")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .or_else(|| load_config_str(&load_config_file(), "thinking_effort"));
        self.thinking_effort = effort;
    }

    /// Warning when the effective effort isn't advertised for the model
    /// (catalog scan skipped when no effort is set). Pure data, no I/O, so
    /// daemon threads stay silent on the TUI's terminal; the caller decides
    /// where it surfaces (transcript vs. stderr).
    pub(crate) fn thinking_mismatch_warning(&self) -> Option<String> {
        let effort = self.thinking_effort.as_ref()?;
        let options = reasoning_options_for(&self.model)?;
        if options.iter().any(|o| o == effort) {
            return None;
        }
        Some(format!(
            "thinking effort '{effort}' not advertised for model '{}' (options: {}); the API may reject it",
            self.model,
            options.join(", ")
        ))
    }

    /// Base wire protocol + baked pin from an entry pin, re-reading the
    /// global file `api:` (cached). Per-model overrides apply on top.
    fn refresh_protocol(&mut self, api_pin: Option<ApiProtocol>) {
        let (api, pinned) = base_protocol(&self.provider, api_pin, &load_config_file());
        self.api = api;
        self.api_pinned = pinned;
    }

    /// Activate a fully-resolved provider bundle: endpoint, protocol base +
    /// pin, and scoped headers move together, so a switch can never leave a
    /// half-moved config behind. Errors (state untouched) when the provider
    /// has no known endpoint instead of pointing requests at "".
    fn set_provider(
        &mut self,
        provider: Provider,
        api_key: String,
        account_id: Option<String>,
        resolved: ResolvedProvider,
    ) -> Result<(), String> {
        let ResolvedProvider {
            landing,
            endpoints,
            api_pin,
            headers,
        } = resolved;
        let landing = landing.ok_or_else(|| {
            format!(
                "provider '{}' has no base_url: set base_url under providers: or run `dex update --models`",
                provider.name()
            )
        })?;
        self.provider = provider;
        self.api_key = api_key;
        self.account_id = account_id;
        self.base_url = landing;
        self.endpoints = endpoints;
        self.provider_headers = headers;
        self.refresh_protocol(api_pin);
        Ok(())
    }

    pub(crate) fn switch_provider(
        &mut self,
        provider: &Provider,
        persist: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (api_key, account_id) = resolve_credentials(provider, &self.provider_entries)?;
        let resolved = resolve_provider(provider, &self.provider_entries);
        self.set_provider(provider.clone(), api_key, account_id, resolved)
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        // Keep the current model's own protocol when the global `api` is not
        // explicitly pinned.
        let current = self.model.clone();
        if let Some(api) = self.resolve_model_api(&current) {
            self.api = api;
        }
        // The endpoint moved under the model: re-resolve the thinking knob
        // for the new endpoint+model too.
        self.refresh_thinking_effort();
        if persist {
            persist_selection(&self.model, &self.provider, &self.base_url, &self.endpoints);
        }
        Ok(())
    }
}

/// One `dex doctor` row: three fixed columns — key (KEY_COLS), value
/// (VALUE_COLS), origin. Widths count display columns (CJK chars render 2
/// wide), so padded values still line up. A value that overflows its column
/// wraps: the value prints in full on its own line and the origin hangs at the
/// origin column, so long paths never run into the origin text.
fn row(out: &mut String, key: &str, value: &str, source: &str) {
    const KEY_COLS: usize = 18;
    const VALUE_COLS: usize = 46;
    // A key wider than its column would collapse the padding and shift every
    // origin column: fail in debug builds instead.
    debug_assert!(
        UnicodeWidthStr::width(key) <= KEY_COLS,
        "doctor key {key:?} exceeds its {KEY_COLS}-column field"
    );
    let origin_indent = " ".repeat(KEY_COLS + VALUE_COLS);
    let fits = UnicodeWidthStr::width(value) <= VALUE_COLS;
    let mut origin_lines = source.split('\n');
    let inline = if fits {
        origin_lines.next().unwrap_or_default()
    } else {
        ""
    };
    if inline.is_empty() {
        out.push_str(&format!("{key:<KEY_COLS$}{value}\n"));
    } else {
        let pad = " ".repeat(VALUE_COLS - UnicodeWidthStr::width(value));
        out.push_str(&format!("{key:<KEY_COLS$}{value}{pad}{inline}\n"));
    }
    for line in origin_lines {
        if !line.is_empty() {
            out.push_str(&format!("{origin_indent}{line}\n"));
        }
    }
}

fn permission_name(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "read-only",
        PermissionMode::AskWrites => "ask-writes",
        PermissionMode::AskShell => "ask-shell",
        PermissionMode::Trusted => "trusted",
    }
}

/// Everything `doctor`'s provider section needs: the resolved selection, the
/// live build (when it succeeded), and the raw config sources it explains.
struct ProviderDoctor<'a> {
    file: &'a Option<serde_yaml::Value>,
    provider_entries: &'a BTreeMap<String, ProviderEntry>,
    known: &'a BTreeSet<String>,
    provider_name: &'a str,
    provider_source: &'a str,
    selection_source: &'a str,
    has_selection: bool,
    routing_selection: Option<&'a str>,
    routing_selection_source: &'a str,
    pre_model: &'a str,
    cfg_result: &'a Result<LlmConfig, Box<dyn std::error::Error>>,
    flag_base_url: Option<&'a str>,
    permission_override: Option<PermissionMode>,
    header_overrides: &'a [String],
    custom_route: bool,
}

/// Where the resolved `base_url` came from: an explicit pin wins, then catalog
/// routing, then the provider entry, then the built-in/catalog default.
fn base_url_source(
    flag_base_url: Option<&str>,
    file_base_url: Option<&str>,
    live: Option<&LlmConfig>,
    landing: Option<&str>,
    base_url: &str,
    entry: Option<&ProviderEntry>,
    provider: &Provider,
) -> String {
    if flag_base_url.is_some() {
        "--base-url (pins endpoint)".to_string()
    } else if file_base_url.is_some() {
        "config base_url: (deprecated)".to_string()
    } else if live.is_some() && landing != Some(base_url) {
        match live.and_then(|c| {
            c.endpoints
                .iter()
                .find(|(_, url)| url.as_str() == base_url)
                .map(|(name, _)| name.clone())
        }) {
            Some(name) => format!("endpoint {name} (models.dev routing)"),
            None => "models.dev catalog routing".to_string(),
        }
    } else if entry.and_then(|e| e.base_url.clone()).is_some() {
        "config providers.<name>.base_url".to_string()
    } else if matches!(
        provider,
        Provider::OpenCode | Provider::OpenAiCodex | Provider::Anthropic
    ) {
        "built-in default".to_string()
    } else {
        "models.dev catalog".to_string()
    }
}

/// Where the API key resolves from — the deposit place, never the value.
fn api_key_source(provider: &Provider, entry: Option<&ProviderEntry>) -> String {
    if matches!(provider, Provider::OpenAiCodex) {
        if env::var_os("CODEX_ACCESS_TOKEN").is_some() {
            "CODEX_ACCESS_TOKEN".to_string()
        } else {
            let home = env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex")))
                .unwrap_or_default();
            format!("{} (codex auth)", home.join("auth.json").display())
        }
    } else if entry
        .and_then(|e| e.api_key.clone())
        .filter(|k| !k.is_empty())
        .is_some()
    {
        format!("config providers.{}.api_key", provider.name())
    } else {
        let mut names = catalog_env_vars(provider.name());
        // Mirror `resolve_credentials`: pinned builtin vars resolve cache-less,
        // ahead of any catalog `env` discovery.
        if let Some(pinned) = pinned_key_env(provider) {
            if !names.iter().any(|v| v == pinned) {
                names.insert(0, pinned.to_string());
            }
        }
        match names
            .iter()
            .find(|n| env::var(n).map(|v| !v.trim().is_empty()).unwrap_or(false))
        {
            Some(name) => format!("{name} (environment)"),
            None => "MISSING — set providers.<name>.api_key or the provider's env var".to_string(),
        }
    }
}

/// The wire protocol and where it came from. Lookup keys mirror `from_env`:
/// the post-split remainder first, then the stripped model.
fn protocol_source(
    entry: Option<&ProviderEntry>,
    file: &Option<serde_yaml::Value>,
    model_api: Option<ApiProtocol>,
    base_url: &str,
    model: &str,
    provider: &Provider,
) -> (String, &'static str) {
    if let Some(api) = entry.and_then(|e| e.api) {
        (api.name().to_string(), "config providers.<name>.api")
    } else if let Some(name) = load_config_str(file, "api") {
        (
            ApiProtocol::parse(&name)
                .map(|a| a.name().to_string())
                .unwrap_or_else(|| format!("INVALID '{name}'")),
            "config api: (deprecated)",
        )
    } else if let Some(api) = model_api {
        (api.name().to_string(), "DEX_MODEL_APIS")
    } else if let Some(api) = learned_api(base_url, model) {
        (api.name().to_string(), "learned (learned-apis.json)")
    } else if let Some(api) = provider.default_api() {
        (api.name().to_string(), "built-in provider default")
    } else {
        (
            "openai-responses".to_string(),
            "default (auto-fallback to completions)",
        )
    }
}

/// Context window and its origin: env > file > index > catalog (no built-in
/// default — a model nothing sizes is a config error, not a 128k guess).
fn context_source(file: &Option<serde_yaml::Value>, model: &str) -> (String, &'static str) {
    if let Some(v) = context_window_from_env() {
        (v.to_string(), "DEX_CONTEXT_WINDOW")
    } else if let Some(ctx) = load_config_num(file, "context_window") {
        (ctx.to_string(), "config context_window:")
    } else if let Some(ctx) = ctx_from_index(model) {
        (ctx.to_string(), "cached context index")
    } else {
        match catalog_context_window(model) {
            Some(ctx) => (ctx.to_string(), "models.dev catalog"),
            None => (
                "UNKNOWN".to_string(),
                "no catalog entry for this model — set context_window: or DEX_CONTEXT_WINDOW",
            ),
        }
    }
}

/// Thinking-effort and its origin: stored choice > env > file > model default.
fn thinking_source(
    file: &Option<serde_yaml::Value>,
    base_url: &str,
    model: &str,
) -> (String, &'static str) {
    if let Some(e) = stored_thinking_effort(base_url, model) {
        (e, "stored /thinking choice")
    } else if let Some(e) = env::var("DEX_THINKING_EFFORT")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        (e, "DEX_THINKING_EFFORT")
    } else if let Some(e) = load_config_str(file, "thinking_effort") {
        (e, "config thinking_effort:")
    } else {
        ("(unset)".to_string(), "model default")
    }
}

/// The provider/model/endpoint/key/protocol rows. Values come from the live
/// build when it succeeds; otherwise the derived values still explain the
/// setup (e.g. missing key). Origins mirror `from_env` exactly.
fn provider_section(out: &mut String, d: &ProviderDoctor<'_>) {
    let provider_opt = Provider::parse_known(d.provider_name, d.known).or_else(|| {
        d.custom_route
            .then(|| Provider::Generic("custom".to_string()))
    });
    let Some(provider) = provider_opt else {
        row(
            out,
            "provider",
            d.provider_name,
            "UNSUPPORTED — use opencode, openai-codex, anthropic, or add it under 'providers:'",
        );
        return;
    };
    let live = d
        .cfg_result
        .as_ref()
        .ok()
        .filter(|c| c.provider == provider);
    row(out, "provider", provider.name(), d.provider_source);
    let model = live.map(|c| c.model.clone()).unwrap_or_else(|| {
        if d.pre_model.is_empty() {
            "(unset)".to_string()
        } else {
            d.pre_model.to_string()
        }
    });
    // An unset model despite a selection is a bare provider pick: name the
    // selection as origin and why the row is empty (the resolve row carries
    // the fix).
    let model_source = if d.has_selection && d.pre_model.is_empty() {
        format!("{} (no model id)", d.selection_source)
    } else {
        d.selection_source.to_string()
    };
    row(out, "model", &model, &model_source);

    let resolved = resolve_provider(&provider, d.provider_entries);
    let entry = d.provider_entries.get(provider.name());
    let file_base_url = load_config_str(d.file, "base_url");
    let derived_base = d
        .flag_base_url
        .map(str::to_string)
        .or(file_base_url.clone())
        .or(resolved.landing.clone())
        .unwrap_or_default();
    let base_url = live.map(|c| c.base_url.clone()).unwrap_or(derived_base);
    let base_source = base_url_source(
        d.flag_base_url,
        file_base_url.as_deref(),
        live,
        resolved.landing.as_deref(),
        &base_url,
        entry,
        &provider,
    );
    row(out, "base_url", &base_url, &base_source);

    // Credentials — name the deposit place, never the value.
    let key_source = api_key_source(&provider, entry);
    row(out, "api key", "(hidden)", &key_source);

    // Wire protocol and its origin. Lookup keys mirror `from_env`: the
    // post-split remainder first, then the stripped model.
    let model_api = model_api_from_env(d.pre_model, &model);
    let (chain_api, api_source) =
        protocol_source(entry, d.file, model_api, &base_url, &model, &provider);
    let api = live.map(|c| c.api.name().to_string()).unwrap_or(chain_api);
    row(out, "protocol", &api, api_source);
    let (chain_ctx, ctx_source) = context_source(d.file, &model);
    let ctx = live
        .map(|c| c.context_window.to_string())
        .unwrap_or(chain_ctx);
    row(out, "context", &format!("{ctx} tokens"), ctx_source);

    // Experiment rows (online compaction, obs pack, evidence reducer): owned
    // by each experiment module and rendered through the experiment registry
    // — config.rs never names a gate or its env var. Order follows the
    // registry.
    for r in crate::agent::experiments::doctor_rows(crate::agent::experiments::DoctorCtx {
        live,
        model: &model,
    }) {
        row(out, r.label, &r.value, &r.source);
    }

    let (chain_effort, effort_source) = thinking_source(d.file, &base_url, &model);
    let effort = live
        .and_then(|c| c.thinking_effort.clone())
        .unwrap_or(chain_effort);
    row(out, "thinking", &effort, effort_source);

    let (perm, perm_source) = match live.map(|c| c.permission) {
        Some(mode) => {
            let source = if d.permission_override.is_some() {
                "--permission"
            } else if env::var("DEX_PERMISSION")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_some()
            {
                "DEX_PERMISSION"
            } else {
                "built-in default"
            };
            (permission_name(mode).to_string(), source.to_string())
        }
        // The build failed (e.g. missing key): still show the origin.
        None => match env::var("DEX_PERMISSION")
            .ok()
            .filter(|v| PermissionMode::parse(v).is_ok())
        {
            Some(v) => (v, "DEX_PERMISSION".to_string()),
            None => ("trusted".to_string(), "built-in default".to_string()),
        },
    };
    row(out, "permission", &perm, &perm_source);
    let (wake, wake_source) = agent_wake_origin();
    row(
        out,
        "agent wake",
        if wake { "on" } else { "off" },
        wake_source,
    );
    // Complexity router (V1): one row per value — the switch plus each
    // tier's resolved selection (tier miss → routing.balanced: →
    // top-level model:), sharing `from_env`'s resolution.
    let routing = routing_resolution(d.file);
    row(
        out,
        "routing",
        if routing.enabled { "on" } else { "off" },
        routing.enabled_origin,
    );
    for tier in Tier::ALL {
        let (value, origin) = routing_tier_display(
            tier,
            &routing,
            d.routing_selection,
            d.routing_selection_source,
        );
        row(out, &format!("routing {}", tier.key()), &value, &origin);
    }

    // Headers: count per layer, sources joined.
    let mut header_count = 0;
    let mut header_sources: Vec<&str> = Vec::new();
    let http_headers = config_headers_map(d.file, "http_headers");
    let file_headers = config_headers_map(d.file, "headers");
    let entry_headers = entry.map(|e| e.headers.clone()).unwrap_or_default();
    let env_headers = custom_headers_from_env();
    let mut cli_headers = BTreeMap::new();
    for raw in d.header_overrides {
        insert_parsed_headers(&mut cli_headers, raw);
    }
    for (headers, source) in [
        (&http_headers, "config http_headers: (deprecated)"),
        (&file_headers, "config headers: (deprecated)"),
        (&entry_headers, "config providers.<name>.headers"),
        (&env_headers, "DEX_HEADERS"),
        (&cli_headers, "--header"),
    ] {
        header_count += headers.len();
        if !headers.is_empty() {
            header_sources.push(source);
        }
    }
    row(
        out,
        "headers",
        &header_count.to_string(),
        &if header_sources.is_empty() {
            "none".to_string()
        } else {
            header_sources.join(" + ")
        },
    );

    if !resolved.endpoints.is_empty() {
        let names: Vec<&str> = resolved.endpoints.keys().map(String::as_str).collect();
        row(
            out,
            "endpoints",
            &names.join(", "),
            "available to /model routing",
        );
    }
    if !d.provider_entries.is_empty() {
        let names: Vec<&str> = d.provider_entries.keys().map(String::as_str).collect();
        row(
            out,
            "providers",
            &names.join(", "),
            "configured in providers:",
        );
    }
}

/// `doctor`'s view of the selection: (provider prefix, pre-routing model id,
/// bare-provider-pick flag). A bare provider pick (`--model anthropic`) or
/// provider-only prefix (`model: zai/`) is not a model id — `split_selection`
/// rejects both, so surface the provider on the provider row and leave the
/// model row unset instead of reading the provider name as a model id (the
/// resolve row carries the fix).
fn doctor_selection_parts(
    selection: Option<&str>,
    known: &BTreeSet<String>,
) -> (Option<String>, String, bool) {
    let Some(selection) = selection else {
        return (None, String::new(), false);
    };
    match split_selection(selection, known) {
        Ok((provider, model)) => (provider, model, false),
        Err(_) => match classify_selection(selection, known) {
            SelectionRoute::BareProvider { provider } => (Some(provider), String::new(), true),
            SelectionRoute::ProviderQualified { provider, .. } => {
                (Some(provider), String::new(), false)
            }
            // Not a provider shape: echo the raw selection as the model id
            // (endpoint prefixes, gateway model ids).
            SelectionRoute::Model(_) => (None, selection.to_string(), false),
        },
    }
}

/// Provider-independent `doctor` rows: system prompt, discovered extensions,
/// extra extension dirs, and the final build status.
fn tail_rows(
    out: &mut String,
    system_prompt_override: &Option<(String, &'static str)>,
    cfg_result: &Result<LlmConfig, Box<dyn std::error::Error>>,
) {
    // Base system prompt override (DEX-13): provider-independent, so it prints
    // even when the provider row is unsupported. Value is a char count — the
    // full text would flood doctor — with the same origin
    // `system_prompt_origin` reports at runtime.
    let (text, label) = match system_prompt_override {
        Some((text, label)) => (Some(text.as_str()), *label),
        None => (None, "--system-prompt"),
    };
    let (custom, origin) = system_prompt_origin_with_label(text, label);
    let value = match custom {
        Some(text) => format!("custom ({} chars)", text.chars().count()),
        None => "default".to_string(),
    };
    row(out, "system prompt", &value, origin);
    // Lua extensions: disk discovery + consent state (deterministic — the
    // load-state detail lives in `dex extensions list`).
    let discovered = crate::extensions::discovered_extensions();
    if discovered.is_empty() {
        row(out, "extensions", "none", "cwd/.dex, XDG config dirs");
    } else {
        for (id, version, scope, state) in &discovered {
            row(
                out,
                "extensions",
                &format!("{id} {version}"),
                &format!("{scope}, {state}"),
            );
        }
    }
    // Extra dirs from config/env (origin per the precedence rules).
    let ext_paths = crate::extensions::config_extension_paths();
    if !ext_paths.is_empty() {
        let origin = if std::env::var("DEX_EXTENSIONS_PATHS")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            "DEX_EXTENSIONS_PATHS (environment)"
        } else {
            "extensions.paths (config)"
        };
        row(
            out,
            "ext paths",
            &ext_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            origin,
        );
    }
    out.push('\n');
    match cfg_result {
        Ok(_) => row(out, "resolve", "OK", "config builds cleanly"),
        Err(e) => row(out, "resolve", "ERROR", &e.to_string()),
    }
}

/// `dex doctor`: print the resolved provider/model/endpoint/protocol/key
/// configuration and where each value came from — `git config
/// --show-origin` for the LLM wiring. Read-only, no network, no daemon;
/// answers "why is dex using X?" without archaeology. Takes the same
/// overrides as `from_env` so flags (`--model`, `--base-url`,
/// `--permission`, `--header`, `--system-prompt` / `--system-prompt-file`
/// resolved text) are reflected, not silently dropped.
pub(crate) fn doctor(
    base_url_override: Option<String>,
    model_override: Option<String>,
    permission_override: Option<PermissionMode>,
    header_overrides: &[String],
    system_prompt_override: Option<(String, &'static str)>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("dex {}\n\n", env!("CARGO_PKG_VERSION")));

    match config_file_path() {
        Some(path) => {
            let state = if load_config_file().is_some() {
                "valid"
            } else {
                "missing or invalid — ignored (env/defaults still apply)"
            };
            row(&mut out, "config", &path.display().to_string(), state);
        }
        None => row(&mut out, "config", "-", "no config file (defaults only)"),
    }
    match dex_catalog_cache_path() {
        Some(path) => match std::fs::metadata(&path) {
            Ok(meta) => {
                let size = format!("{} KB", meta.len() / 1024);
                row(&mut out, "catalog", &path.display().to_string(), &size);
            }
            Err(_) => row(
                &mut out,
                "catalog",
                &path.display().to_string(),
                "missing — run `dex update --models`",
            ),
        },
        None => row(&mut out, "catalog", "-", "unavailable (no HOME/XDG)"),
    }
    out.push('\n');

    let file = load_config_file();
    let provider_entries = load_provider_entries(&file);
    let known = known_providers(&provider_entries);

    // Selection + origin, mirroring `from_env` precedence:
    // `--model` > `DEX_MODEL` > file `model:` — nothing set anywhere is
    // reported as unconfigured, not silently defaulted.
    let flag_model = model_override.clone().filter(|m| !m.trim().is_empty());
    let flag_base_url = base_url_override.clone().filter(|u| !u.trim().is_empty());
    let selection_resolved = resolve_selection(flag_model.clone(), &file);
    let selection = selection_resolved.as_ref().ok().map(|r| r.value.clone());
    let selection_source = match &selection_resolved {
        Ok(r) => r.origin.to_string(),
        // Short form here: the resolve row prints the full setup guide.
        Err(_) => "UNCONFIGURED — set 'model: <provider>/<model>'".to_string(),
    };
    let (selection_provider, pre_model, bare_pick) =
        doctor_selection_parts(selection.as_deref(), &known);
    // Provider fallback shares `from_env`'s chain (and its deprecation
    // warning, deduped) so the origin row cannot drift from routing.
    let (fallback_provider, fallback_origin) = provider_fallback_with_origin(&file);
    let provider_name = selection_provider.clone().unwrap_or_else(|| {
        provider_without_prefix(
            (fallback_provider.clone(), fallback_origin),
            flag_base_url.as_deref(),
        )
    });
    let provider_source = match selection_provider {
        // A bare pick carries no prefix; the selection origin says it all.
        Some(_) if bare_pick => selection_source.clone(),
        Some(_) => format!("{selection_source} prefix"),
        // Same routing as `from_env`: an explicit --base-url with no
        // selection prefix lands on providers.custom, not the builtin.
        None if provider_name == "custom" => "--base-url (pins endpoint)".to_string(),
        None => fallback_origin.unwrap_or("built-in default").to_string(),
    };
    // The real build, once: rows below take values from it so `doctor`
    // agrees with runtime routing (catalog endpoint moves, prefix
    // stripping, per-model/learned protocol). Origins stay informational.
    let cfg_result = LlmConfig::from_env(
        flag_base_url.clone(),
        flag_model.clone(),
        permission_override,
        header_overrides,
    );
    // The `--base-url` route can land on `custom` before the entry exists;
    // it is a real (user-URL-backed) provider then, not a typo.
    provider_section(
        &mut out,
        &ProviderDoctor {
            file: &file,
            provider_entries: &provider_entries,
            known: &known,
            provider_name: &provider_name,
            provider_source: &provider_source,
            selection_source: &selection_source,
            has_selection: selection.is_some(),
            pre_model: &pre_model,
            cfg_result: &cfg_result,
            flag_base_url: flag_base_url.as_deref(),
            permission_override,
            header_overrides,
            routing_selection: selection.as_deref(),
            routing_selection_source: &selection_source,
            custom_route: selection_provider.is_none() && provider_name == "custom",
        },
    );
    tail_rows(&mut out, &system_prompt_override, &cfg_result);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{
        apply_verify_optin, build_ctx_map, catalog_cache_missing, detect_verify_command, doctor,
        load_config_file, load_dex_models_cache, load_provider_entries, model_api_from_env,
        persist_selection, reasoning_options_for, remember_learned_api, remember_thinking_effort,
        stored_thinking_effort, unique_tmp_path, usage_cost, validate_thinking_effort,
        warn_provider_like_selection, write_ctx_index, ApiProtocol, LlmConfig, PermissionMode,
        Provider, ProviderEntry,
    };
    use crate::core::types::Usage;
    use std::{collections::BTreeSet, env};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn apply_model_routes_prefixed_selection_to_endpoint() {
        let mut cfg = test_cfg();
        cfg.model = "gpt-5.6-luna".into();
        // Bare id keeps the current base_url.
        assert_eq!(cfg.apply_model("gpt-5.6-luna", false).unwrap(), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // Prefixed id routes to the endpoint and strips the prefix.
        assert_eq!(
            cfg.apply_model("go/kimi-k2", false).unwrap().as_deref(),
            Some("go")
        );
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Provider-native slashes in model ids survive under an endpoint.
        assert_eq!(
            cfg.apply_model("go/moonshotai/kimi-k2", false)
                .unwrap()
                .as_deref(),
            Some("go")
        );
        assert_eq!(cfg.model, "moonshotai/kimi-k2");
        // Unknown prefix is a plain model id.
        assert_eq!(cfg.apply_model("unknown/m", false).unwrap(), None);
        assert_eq!(cfg.model, "unknown/m");
    }

    /// Synthetic models.dev catalog: the same id priced differently per
    /// provider, with the endpoint-exact entry NOT first alphabetically.
    fn write_cost_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "aaa-reseller": {
                    "api": "https://aaa.example/v1",
                    "models": {
                        "m-1": { "cost": { "input": 2.0 } },
                        "m-2": { "cost": { "input": 4.0 } }
                    }
                },
                "opencode": {
                    "api": "https://zen.example/v1",
                    "models": {
                        "m-1": { "cost": { "input": 0.5 } },
                        "moonshotai/kimi-k2": { "cost": { "input": 0.5 } }
                    }
                },
                "moonshotai": {
                    "api": "https://moonshot.example/v1",
                    "models": { "kimi-k2": { "cost": { "input": 1.0 } } }
                },
                "opencode-go": {
                    "api": "https://go.example/v1",
                    "models": {
                        "m-1": { "cost": { "input": 0.25, "cache_read": 0.0625, "output": 2.0 } }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn usage_cost_prefers_endpoint_then_provider_pricing() {
        // Serializes process-env redirection against other tests.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_cost_catalog(&dir);
        let prev_cache = env::var_os("XDG_CACHE_HOME");
        env::set_var("XDG_CACHE_HOME", &dir);
        let usage = |prompt: u64, completion: u64, cached: Option<u64>| Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
        };

        // Endpoint-exact match wins even though "aaa-reseller" sorts first.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://go.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(0.25)
        );
        // Fresh + cached + output are each billed at their own rate:
        // 0.6*0.25 + 0.4*0.0625 + 100k*2.0/1M.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://go.example/v1",
                &usage(1_000_000, 100_000, Some(400_000))
            ),
            Some(0.375)
        );
        // No endpoint match: the configured provider's catalog keys win over
        // the global scan (0.5, not aaa-reseller's 2.0).
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://unrelated.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(0.5)
        );
        // A catalog entry without an output rate bills output at the input
        // rate: 1M*0.5 + 1M*0.5.
        assert_eq!(
            usage_cost(
                "m-1",
                &Provider::OpenCode,
                "https://unrelated.example/v1",
                &usage(1_000_000, 1_000_000, None)
            ),
            Some(1.0)
        );
        // Model only listed by another provider: global fallback still prices it.
        assert_eq!(
            usage_cost(
                "m-2",
                &Provider::OpenAiCodex,
                "https://x.example/v1",
                &usage(1_000_000, 0, None)
            ),
            Some(4.0)
        );

        match prev_cache {
            Some(v) => env::set_var("XDG_CACHE_HOME", v),
            None => env::remove_var("XDG_CACHE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_model_cost_ignores_trailing_slash() {
        // Serializes process-env redirection against other tests. Hermetic
        // catalog via `XDG_CACHE_HOME` — the lookup is index-backed, so the
        // catalog arrives as a file, not a `Value`.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-cost-slash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "go": {
                    "api": "https://go.example/v1",
                    "models": {"m-1": {"cost": {"input": 1.0}}}
                }
            })
            .to_string(),
        )
        .unwrap();
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        // A pinned `base_url` with a trailing slash still hits the
        // endpoint-exact tier instead of falling through to reseller pricing.
        let cost = super::resolve_model_cost("m-1", &["go".to_string()], "https://go.example/v1/");
        assert_eq!(cost.map(|c| c.input), Some(1.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cache-write/read ratio resolution feeding the online compaction
    /// economics: explicit write surcharge wins, unpriced writes derive
    /// from input/cache_read (writes bill at the plain input rate), and no
    /// cache pricing at all falls back to the measured cross-provider
    /// default. Hermetic catalog via `XDG_CACHE_HOME`.
    #[test]
    fn cache_write_read_ratio_resolves_write_unpriced_and_fallback() {
        // Serializes process-env redirection against other tests.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-ratio-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                // Anthropic-style: explicit write surcharge.
                "anthropic-like": {
                    "models": {
                        "surcharge": { "cost": { "input": 3.0, "cache_write": 3.75, "cache_read": 0.3, "output": 15.0 } }
                    }
                },
                // The norm: cache_read priced, writes unpriced (input rate).
                "plain": {
                    "models": {
                        "flat": { "cost": { "input": 1.25, "cache_read": 0.125, "output": 10.0 } }
                    }
                },
                // No cache pricing at all: reads bill at the input rate.
                "opaque": {
                    "models": {
                        "nada": { "cost": { "input": 2.0, "output": 8.0 } }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let prev_cache = env::var_os("XDG_CACHE_HOME");
        env::set_var("XDG_CACHE_HOME", &dir);
        let mut cfg = test_cfg();
        // No model match at all → measured fallback.
        assert!((cfg.cache_write_read_ratio() - 5.0).abs() < 1e-9);
        cfg.model = "flat".into();
        // Writes at the plain input rate: 1.25 / 0.125 = 10.
        assert!((cfg.cache_write_read_ratio() - 10.0).abs() < 1e-9);
        cfg.model = "surcharge".into();
        // Explicit surcharge: 3.75 / 0.3 = 12.5.
        assert!((cfg.cache_write_read_ratio() - 12.5).abs() < 1e-9);
        cfg.model = "nada".into();
        // No cache rates: both bill at input (like `usage_cost`), so a
        // re-write after compaction costs exactly what a read costs → 1.0,
        // and the economics see no surcharge to amortize.
        assert!((cfg.cache_write_read_ratio() - 1.0).abs() < 1e-9);
        match prev_cache {
            Some(v) => env::set_var("XDG_CACHE_HOME", v),
            None => env::remove_var("XDG_CACHE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    pub(crate) fn test_cfg() -> LlmConfig {
        LlmConfig {
            provider: Provider::OpenCode,
            api_key: "k".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            model: "m-r".into(),
            available_models: Vec::new(),
            endpoints: [
                ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
                (
                    "go".to_string(),
                    "https://opencode.ai/zen/go/v1".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            api: ApiProtocol::Responses,
            api_pinned: false,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: PermissionMode::AskWrites,
            verify_command: None,
            extra_headers: Default::default(),
            global_headers: Default::default(),
            client: reqwest::Client::new(),
            provider_entries: Default::default(),
            provider_headers: Default::default(),
        }
    }

    /// Save/restore process env around tests that redirect dex env vars.
    struct EnvRestore {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn take(keys: &[&'static str]) -> Self {
            Self {
                vars: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, prev) in self.vars.drain(..) {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// DEX_MODEL_APIS table with `DEX_API` unpinned so resolution runs.
    fn model_apis_env(table: &str) -> EnvRestore {
        let guard = EnvRestore::take(&["DEX_MODEL_APIS"]);
        std::env::set_var("DEX_MODEL_APIS", table);
        guard
    }

    #[test]
    fn apply_model_follows_model_apis() {
        // Serializes process-env redirection (DEX_MODEL_APIS/DEX_API)
        // against daemon tests holding the same guard.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = model_apis_env("m-c=openai-completions");
        let mut cfg = test_cfg();
        cfg.apply_model("m-c", false).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        // Unknown ids keep the current protocol.
        cfg.apply_model("m-r", false).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    }

    #[test]
    fn apply_model_full_selection_key_wins() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = model_apis_env("m-x=openai-completions,go/m-x=openai-responses");
        let mut cfg = test_cfg();
        cfg.apply_model("go/m-x", false).unwrap();
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "m-x");
        assert_eq!(cfg.api, ApiProtocol::Responses);
        cfg.apply_model("m-x", false).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
    }

    #[test]
    fn bare_unconfigured_catalog_provider_resolves_and_warns() {
        // A selection naming an unconfigured catalog provider ("zai",
        // bare or `zai/model`) rides the fallback provider as an opaque
        // id: resolution succeeds and a one-time hint points at the fix.
        // Already-served and known native ids are not mistakes.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-providerlike-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_cost_catalog(&dir);
        let _cache = EnvRestore::take(&["XDG_CACHE_HOME"]);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let mut cfg = test_cfg();
        // Unconfigured provider key: warns, resolution keeps zen and sends
        // the name as a model id.
        assert!(warn_provider_like_selection(
            "aaa-reseller",
            "opencode",
            &["m-1".to_string()]
        ));
        cfg.apply_model("aaa-reseller", false).unwrap();
        assert_eq!(cfg.model, "aaa-reseller");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // A model id the gateway serves is fine.
        assert!(!warn_provider_like_selection(
            "m-1",
            "opencode",
            &["m-1".to_string()]
        ));
        // A qualified pick off an unconfigured provider warns too — it
        // rides the fallback as an opaque id just like the bare name.
        assert!(warn_provider_like_selection(
            "aaa-reseller/m-2",
            "opencode",
            &[]
        ));
        // A known native `org/model` id sharing its prefix with a provider
        // is legit — no warning. Unknown prefixes and the resolved
        // provider itself are exempt too.
        assert!(!warn_provider_like_selection(
            "moonshotai/kimi-k2",
            "opencode",
            &[]
        ));
        assert!(!warn_provider_like_selection(
            "unknown/m-1",
            "opencode",
            &[]
        ));
        assert!(!warn_provider_like_selection("opencode", "opencode", &[]));
    }

    #[test]
    fn dex_model_bare_provider_name_resolves_as_model_id() {
        // DEX_MODEL=aaa-reseller (unconfigured catalog provider) is not a
        // provider switch: it rides the default provider as a model id and
        // the config builds — the hint warns instead of erroring.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-bareprov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_cost_catalog(&dir.join("cache"));
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            "XDG_CACHE_HOME",
            "DEX_CONTEXT_WINDOW",
        ]);
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::remove_var("DEX_MODEL");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
        let cfg = LlmConfig::from_env(None, Some("aaa-reseller".to_string()), None, &[]).unwrap();
        assert_eq!(cfg.model, "aaa-reseller");
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_api_from_env_ignores_malformed_entries() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_MODEL_APIS"]);
        std::env::remove_var("DEX_MODEL_APIS");
        assert_eq!(
            model_api_from_env("m-c", "m-c"),
            None,
            "unset table resolves nothing"
        );
        std::env::set_var("DEX_MODEL_APIS", "junk-no-equals,m-c=bogus-api,m-c=chat");
        assert_eq!(
            model_api_from_env("m-c", "m-c"),
            Some(ApiProtocol::ChatCompletions),
            "first parseable match wins"
        );
    }

    #[test]
    fn provider_parse_accepts_aliases() {
        let known: BTreeSet<String> = ["zai".to_string()].into_iter().collect();
        let assert_known = |name: &str| Provider::parse_known(name, &known).unwrap();
        assert_eq!(assert_known("opencode"), Provider::OpenCode);
        assert_eq!(assert_known("codex"), Provider::OpenAiCodex);
        assert_eq!(assert_known("openai-codex"), Provider::OpenAiCodex);
        assert_eq!(assert_known("zai"), Provider::Generic("zai".to_string()));
        assert!(Provider::parse_known("unknown", &known).is_none());
        // No "openai" alias: a generic `providers.openai` entry is the way
        // to point that name at an OpenAI-compatible endpoint.
        assert!(Provider::parse_known("openai", &known).is_none());
        assert_eq!(
            Provider::OpenCode.default_base_url(),
            Some("https://opencode.ai/zen/v1")
        );
        assert_eq!(
            Provider::OpenAiCodex.default_base_url(),
            Some("https://chatgpt.com/backend-api/codex")
        );
    }

    #[test]
    fn selection_trims_whitespace_around_provider_and_model() {
        let known: BTreeSet<String> = ["zai".to_string()].into_iter().collect();
        // The provider side already trims via `parse_known`; a stray space
        // on the model side must not survive into the catalog lookup.
        assert_eq!(
            super::split_selection("zai/ glm-x", &known),
            Ok((Some("zai".to_string()), "glm-x".to_string()))
        );
        assert_eq!(
            super::split_selection(" zai / glm-x ", &known),
            Ok((Some("zai".to_string()), "glm-x".to_string()))
        );
    }

    #[test]
    fn apply_model_switches_provider_and_sets_base_url_without_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = EnvRestore::take(&["OPENCODE_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID"]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("CODEX_ACCESS_TOKEN", "codex-tok");
        let mut cfg = test_cfg();
        // starts as OpenCode @ zen
        assert_eq!(cfg.provider, Provider::OpenCode);
        // Switch to codex via provider-qualified model — no env base_url required
        cfg.apply_model("openai-codex/gpt-5.6-luna", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenAiCodex);
        assert_eq!(
            cfg.base_url,
            Provider::OpenAiCodex.default_base_url().unwrap()
        );
        assert_eq!(cfg.model, "gpt-5.6-luna");
        // Switch back via the provider prefix
        cfg.apply_model("opencode/gpt-4o", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
        assert_eq!(cfg.model, "gpt-4o");
        // Provider + endpoint: opencode/go/kimi -> go endpoint
        cfg.apply_model("opencode/go/kimi-k2", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Bare endpoint still works without provider prefix
        cfg = test_cfg();
        cfg.apply_model("go/kimi-k2", false).unwrap();
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.model, "kimi-k2");
        // Bare provider name switches provider and keeps the model.
        cfg = test_cfg();
        cfg.apply_model("gpt-4o", false).unwrap();
        cfg.apply_model("openai-codex", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenAiCodex);
        assert_eq!(
            cfg.base_url,
            Provider::OpenAiCodex.default_base_url().unwrap()
        );
        assert_eq!(cfg.model, "gpt-4o");
        cfg.apply_model("opencode", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.model, "gpt-4o");
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
    }

    #[test]
    fn endpoints_always_available_for_opencode_without_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = EnvRestore::take(&[
            "OPENCODE_API_KEY",
            "DEX_PROVIDER",
            "DEX_MODELS",
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_CONTEXT_WINDOW",
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key2");
        std::env::remove_var("DEX_MODELS");
        std::env::set_var("DEX_PROVIDER", "opencode");
        std::env::set_var("DEX_MODEL", "opencode/m");
        std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
        // No config file: point DEX_CONFIG at a missing path.
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-missing-{}", std::process::id())),
        );
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
        assert!(cfg.endpoints.contains_key("go"));
        assert!(cfg.endpoints.contains_key("zen"));
    }

    #[test]
    fn custom_headers_parse_json_and_pairs() {
        use super::parse_headers_str;
        let json = parse_headers_str(r#"{"X-Gateway-Key":"abc","X-Empty":"","num":42}"#);
        assert_eq!(json.get("X-Gateway-Key").map(String::as_str), Some("abc"));
        assert_eq!(json.get("num").map(String::as_str), Some("42"));
        assert!(!json.contains_key("X-Empty"), "empty values are skipped");
        assert!(parse_headers_str("  ").is_empty());

        let pairs = parse_headers_str("X-Foo: bar, X-Baz=qux");
        assert_eq!(pairs.get("X-Foo").map(String::as_str), Some("bar"));
        assert_eq!(pairs.get("X-Baz").map(String::as_str), Some("qux"));

        // Newline-separated pairs may contain commas in values.
        let multi = parse_headers_str("X-A: one, two\nX-B: three");
        assert_eq!(multi.get("X-A").map(String::as_str), Some("one, two"));
        assert_eq!(multi.get("X-B").map(String::as_str), Some("three"));

        // Malformed entries are skipped, later duplicates win.
        let messy = parse_headers_str("no-separator, : novalue, X-K: 1, X-K: 2");
        assert_eq!(messy.len(), 1);
        assert_eq!(messy.get("X-K").map(String::as_str), Some("2"));

        // Case-insensitive duplicates collapse (last casing/value wins).
        let ci = parse_headers_str("X-Foo: 1, x-foo: 2");
        assert_eq!(ci.len(), 1);
        assert_eq!(ci.get("x-foo").map(String::as_str), Some("2"));

        // `authorization` never lands in the map (api key owns it).
        assert!(parse_headers_str("Authorization: hacked").is_empty());
        assert!(parse_headers_str(r#"{"authorization":"hacked"}"#).is_empty());

        // A `{...}` value that isn't a JSON object falls back to pairs.
        let fb = parse_headers_str("{bad json");
        assert!(fb.is_empty(), "no separator means no pairs either");
        let fb = parse_headers_str("{X-Foo: bar}");
        assert_eq!(fb.get("X-Foo").map(String::as_str), Some("bar"));
    }

    #[test]
    fn custom_headers_layer_file_env_cli() {
        use std::collections::BTreeMap;

        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = EnvRestore::take(&[
            "OPENCODE_API_KEY",
            "DEX_PROVIDER",
            "DEX_MODELS",
            "DEX_CONFIG",
            "DEX_HEADERS",
            "OPENAI_HEADERS",
            "ANTHROPIC_CUSTOM_HEADERS",
        ]);
        // Config file: `headers:` wins per-key over `http_headers:`.
        let dir = std::env::temp_dir().join(format!("dex-headers-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("config.yaml");
        std::fs::write(
            &cfg_path,
            "active_provider: opencode\nmodel: m-h\ncontext_window: 1000\nhttp_headers:\n  X-File: file\n  X-Shared: http\nheaders:\n  X-Shared: file\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", &cfg_path);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::remove_var("DEX_MODELS");
        std::env::set_var("DEX_HEADERS", "X-Env: env");
        std::env::set_var("OPENAI_HEADERS", "X-Env2: openai");
        std::env::set_var("ANTHROPIC_CUSTOM_HEADERS", "X-Env3: env3");
        let cfg = LlmConfig::from_env(
            None,
            None,
            None,
            &["X-Cli: cli".to_string(), "X-Shared: cli".to_string()],
        )
        .unwrap();
        assert_eq!(cfg.model, "m-h");
        // File layers land in `global_headers` (lowest precedence on the
        // wire); `http_headers:` loses to `headers:` per key inside that map.
        assert_eq!(
            cfg.global_headers.get("X-File").map(String::as_str),
            Some("file")
        );
        assert_eq!(
            cfg.global_headers.get("X-Shared").map(String::as_str),
            Some("file")
        );
        // Env / CLI land in `extra_headers` — above both file layers.
        assert_eq!(
            cfg.extra_headers.get("X-Env").map(String::as_str),
            Some("env")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Env2").map(String::as_str),
            Some("openai")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Env3").map(String::as_str),
            Some("env3")
        );
        assert_eq!(
            cfg.extra_headers.get("X-Cli").map(String::as_str),
            Some("cli")
        );
        assert!(!cfg.extra_headers.contains_key("X-File"));
        // The wire merge orders the layers: CLI wins over both config-file
        // spellings, file keys survive where nothing above them speaks.
        let merged: BTreeMap<String, String> = crate::llm::client::merged_headers(&cfg)
            .into_iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(merged.get("x-shared").map(String::as_str), Some("cli"));
        assert_eq!(merged.get("x-file").map(String::as_str), Some("file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_headers_config_text_and_list_shapes() {
        use super::load_config_headers;
        // Text-header block (same syntax as env vars / `--header`).
        let file: Option<serde_yaml::Value> =
            Some(serde_yaml::from_str("headers: \"X-A: one\\nX-B: two, three\"\n").unwrap());
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-A").map(String::as_str), Some("one"));
        assert_eq!(out.get("X-B").map(String::as_str), Some("two, three"));
        // List mixing text entries and one-key maps; later entries win.
        let file: Option<serde_yaml::Value> = Some(
            serde_yaml::from_str("http_headers:\n  - \"X-A: 1\"\n  - X-A: 2\n  - X-C: 3\n")
                .unwrap(),
        );
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-A").map(String::as_str), Some("2"));
        assert_eq!(out.get("X-C").map(String::as_str), Some("3"));
        // Non-string scalars and blank entries are skipped.
        let file: Option<serde_yaml::Value> =
            Some(serde_yaml::from_str("headers:\n  X-N: 42\n  X-E: \"\"\n").unwrap());
        let out = load_config_headers(&file);
        assert_eq!(out.get("X-N").map(String::as_str), Some("42"));
        assert!(!out.contains_key("X-E"));
    }

    #[test]
    fn opencode_session_headers_gated_and_explicit_wins() {
        use super::apply_opencode_session_headers;
        // Opencode provider (zen or go endpoint) + session id → both headers.
        let mut cfg = test_cfg();
        apply_opencode_session_headers(&mut cfg, "sess-1");
        assert_eq!(
            cfg.extra_headers
                .get("x-opencode-session")
                .map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(
            cfg.extra_headers
                .get("x-opencode-client")
                .map(String::as_str),
            Some("dex")
        );
        // Generic provider on another host → nothing.
        let mut cfg = test_cfg();
        cfg.provider = Provider::Generic("other".to_string());
        cfg.base_url = "https://other.example/v1".into();
        apply_opencode_session_headers(&mut cfg, "sess-1");
        assert!(cfg.extra_headers.is_empty());
        // Generic provider pointed at opencode.ai → headers (host fallback).
        let mut cfg = test_cfg();
        cfg.provider = Provider::Generic("proxy".to_string());
        apply_opencode_session_headers(&mut cfg, "sess-1");
        assert_eq!(cfg.extra_headers.len(), 2);
        // Empty session id → nothing, even for opencode.
        let mut cfg = test_cfg();
        apply_opencode_session_headers(&mut cfg, "  ");
        assert!(cfg.extra_headers.is_empty());
        // Explicit user header wins (any casing); client header still fills.
        let mut cfg = test_cfg();
        cfg.extra_headers
            .insert("X-Opencode-Session".to_string(), "mine".to_string());
        apply_opencode_session_headers(&mut cfg, "sess-1");
        assert_eq!(
            cfg.extra_headers
                .get("X-Opencode-Session")
                .map(String::as_str),
            Some("mine")
        );
        assert_eq!(
            cfg.extra_headers
                .get("x-opencode-client")
                .map(String::as_str),
            Some("dex")
        );
        // A FILE-layer pin must suppress the auto-fill too: `extra_headers`
        // merges after both file layers, so injecting here would silently
        // override the user's config-file header.
        let mut cfg = test_cfg();
        cfg.provider_headers
            .insert("x-opencode-session".to_string(), "file".to_string());
        apply_opencode_session_headers(&mut cfg, "sess-1");
        assert!(!cfg.extra_headers.contains_key("x-opencode-session"));
        assert_eq!(
            cfg.extra_headers
                .get("x-opencode-client")
                .map(String::as_str),
            Some("dex")
        );
    }

    #[test]
    fn permission_parse_and_ordering() {
        assert_eq!(
            PermissionMode::parse("read-only").unwrap(),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::parse("readonly").unwrap(),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            PermissionMode::parse("ask_writes").unwrap(),
            PermissionMode::AskWrites
        );
        assert_eq!(
            PermissionMode::parse("trusted").unwrap(),
            PermissionMode::Trusted
        );
        assert!(PermissionMode::parse("nope").is_err());
        assert!(
            PermissionMode::ReadOnly.permissiveness() < PermissionMode::Trusted.permissiveness()
        );
    }

    /// An explicit `verify_command` is never overwritten — even with the
    /// opt-in env set. The auto-detect branch itself is cwd-dependent and
    /// covered by `detect_verify_command_selects_by_manifest`.
    #[test]
    fn apply_verify_optin_explicit_command_wins() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut config = test_cfg();
        config.verify_command = Some("make check".into());
        apply_verify_optin(&mut config);
        assert_eq!(config.verify_command.as_deref(), Some("make check"));
        let _env = EnvRestore::take(&["DEX_VERIFY"]);
        std::env::set_var("DEX_VERIFY", "1");
        apply_verify_optin(&mut config);
        assert_eq!(config.verify_command.as_deref(), Some("make check"));
    }

    #[test]
    fn detect_verify_command_selects_by_manifest() {
        // Isolated temp dir without manifests -> None
        let prev = std::env::current_dir().unwrap();
        let tmp = std::env::temp_dir().join(format!("dex-verify-none-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_current_dir(&tmp).unwrap();
        assert_eq!(detect_verify_command(), None);
        // Cargo.toml -> cargo test
        std::fs::write(tmp.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("cargo test"));
        let _ = std::fs::remove_file(tmp.join("Cargo.toml"));
        std::fs::write(tmp.join("go.mod"), "module x").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("go test ./..."));
        let _ = std::fs::remove_file(tmp.join("go.mod"));
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        assert_eq!(detect_verify_command().as_deref(), Some("npm test"));
        // Restore the cwd *before* deleting the temp dir: this runs
        // concurrently with other tests, and a deleted process cwd makes
        // `current_dir()` return None for them.
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn catalog_cache_missing_reports_fresh_installs() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-catalog-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let path = dir.join("dex/models.dev.json");
        // No file (fresh install) or a zero-length file (torn write): missing.
        assert!(catalog_cache_missing());
        std::fs::write(&path, "").unwrap();
        assert!(catalog_cache_missing());
        // Any non-empty catalog counts as present.
        std::fs::write(&path, "{}").unwrap();
        assert!(!catalog_cache_missing());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_ctx_index_writes_leave_no_torn_file() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        // tmp paths are unique per call even within one process — a shared
        // name lets one writer rename another's half-written tmp file.
        let base = std::path::PathBuf::from("/tmp/dex");
        assert_ne!(unique_tmp_path(&base), unique_tmp_path(&base));
        let dir = std::env::temp_dir().join(format!("dex-ctx-index-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        // Four concurrent writers on one destination. Maps are large enough
        // that a write spans syscalls, so a shared tmp name could publish
        // another writer's half-written bytes.
        let mk_map = |ctx: u64| -> std::collections::BTreeMap<String, u64> {
            (0..20_000u32)
                .map(|i| (format!("model-{i}"), ctx))
                .collect()
        };
        for _ in 0..4 {
            std::thread::scope(|s| {
                for ctx in [1000u64, 2000, 3000, 4000] {
                    let map = mk_map(ctx);
                    s.spawn(move || write_ctx_index(&map));
                }
            });
            let path = dir.join("dex/models.ctx.json");
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: std::collections::BTreeMap<String, u64> =
                serde_json::from_str(&text).expect("index must always parse");
            assert_eq!(parsed.len(), 20_000);
            assert!(
                [1000, 2000, 3000, 4000].contains(parsed.values().next().unwrap()),
                "torn or foreign index contents"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_default_missing_key_suggests_setup() {
        // Nothing selects a provider (no flag/env/file pointer): the
        // missing-key error must guide setup, not endorse opencode.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "ANTHROPIC_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-nokey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        // Point at a path that does not exist: no file `model:`.
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        for key in [
            "DEX_MODEL",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            std::env::remove_var(key);
        }
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected setup-guide error"),
        };
        assert!(err.contains("no model configured"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("gateway"), "{err}");
        assert!(err.contains("openai-codex"), "{err}");
        assert!(!err.contains("no API key for provider"), "{err}");
        // The same guide surfaces in `dex doctor`'s resolve row.
        let report = doctor(None, None, None, &[], None);
        assert!(report.contains("no model configured"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_base_url_without_selection_routes_to_custom_provider() {
        // `--base-url` + no provider pointer anywhere must NOT ride the
        // builtin default: key errors name providers.custom.api_key, and
        // no OPENCODE_API_KEY is demanded for a foreign endpoint.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-custom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        for key in [
            "DEX_MODEL",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "ANTHROPIC_API_KEY",
        ] {
            std::env::remove_var(key);
        }
        let err = match LlmConfig::from_env(
            Some("http://localhost:11434/v1".to_string()),
            None,
            None,
            &[],
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error for providers.custom"),
        };
        assert!(err.contains("providers.custom.api_key"), "{err}");
        assert!(!err.contains("opencode.api_key"), "{err}");
        assert!(!err.contains("OPENCODE_API_KEY"), "{err}");
        // An explicit deprecated provider pointer beats the custom route:
        // the user named a provider.
        std::env::set_var("DEX_PROVIDER", "opencode");
        let err = match LlmConfig::from_env(
            Some("http://localhost:11434/v1".to_string()),
            None,
            None,
            &[],
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error for providers.opencode"),
        };
        assert!(err.contains("providers.opencode.api_key"), "{err}");
        // With providers.custom.api_key set, resolution succeeds on the
        // pinned URL with the default model.
        std::env::remove_var("DEX_PROVIDER");
        std::fs::write(
            dir.join("config.yaml"),
            "providers:\n  custom:\n    api_key: kk\n",
        )
        .unwrap();
        // No builtin default model exists: the build demands an explicit
        // model even with the key and URL in place.
        let err = match LlmConfig::from_env(
            Some("http://localhost:11434/v1".to_string()),
            None,
            None,
            &[],
        ) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected no-model-configured error"),
        };
        assert!(err.contains("no model configured"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_names_custom_provider_for_base_url_only_setup() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-doc-custom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        for key in ["DEX_PROVIDER", "OPENCODE_API_KEY", "DEX_MODEL"] {
            std::env::remove_var(key);
        }
        let out = doctor(
            Some("http://localhost:11434/v1".to_string()),
            None,
            None,
            &[],
            None,
        );
        let prow = out.lines().find(|l| l.starts_with("provider ")).unwrap();
        assert!(prow.contains("custom"), "{prow}");
        assert!(prow.contains("--base-url"), "{prow}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn setup_guide_lists_builtin_default_first() {
        let guide = super::setup_guide_error();
        let opencode = guide.find("opencode: model:").unwrap();
        let anthropic = guide.find("anthropic: model:").unwrap();
        let codex = guide.find("codex: model:").unwrap();
        assert!(opencode < anthropic && anthropic < codex, "{guide}");
    }

    #[test]
    fn opencode_key_resolves_entry_then_own_env_var() {
        // Deposit order: providers.opencode.api_key > OPENCODE_API_KEY
        // (opencode's gateway key).
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-okey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::remove_var("OPENCODE_API_KEY");
        // The provider's own env var is the shell path.
        std::env::set_var("OPENCODE_API_KEY", "canonical");
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "canonical"
        );
        // The scoped deposit place wins over the env var.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    api_key: deposited\n",
        )
        .unwrap();
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "deposited"
        );
        // Missing everywhere: the error points at the canonical names.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\n",
        )
        .unwrap();
        std::env::remove_var("OPENCODE_API_KEY");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error"),
        };
        assert!(err.contains("providers.opencode.api_key"), "{err}");
        assert!(err.contains("OPENCODE_API_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn anthropic_key_resolves_cacheless_via_pinned_env_var() {
        // The catalog cache is empty (`{}`) — ANTHROPIC_API_KEY must still
        // resolve because builtin native providers pin their canonical var
        // (see `pinned_key_env`). Deposit order matches opencode:
        // providers.anthropic.api_key > ANTHROPIC_API_KEY.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "ANTHROPIC_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-akey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: anthropic\nmodel: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
        )
        .unwrap();
        // Cache-less: the pinned var is the shell path; the bare provider
        // pick gets a model of its own family and the native wire.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api_key, "sk-ant");
        assert_eq!(cfg.model, "claude-sonnet-4-5");
        assert_eq!(cfg.api, ApiProtocol::Anthropic);
        // The scoped deposit place wins over the env var.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: anthropic\nmodel: anthropic/claude-sonnet-4-5\ncontext_window: 1000\nproviders:\n  anthropic:\n    api_key: deposited\n",
        )
        .unwrap();
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "deposited"
        );
        // The primary knob (`model: anthropic`) picks the family default too.
        std::fs::write(
            dir.join("config.yaml"),
            "model: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
        )
        .unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-5");
        assert_eq!(cfg.api, ApiProtocol::Anthropic);
        // Missing everywhere: the error points at the canonical names.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: anthropic\nmodel: anthropic/claude-sonnet-4-5\ncontext_window: 1000\n",
        )
        .unwrap();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error"),
        };
        assert!(err.contains("providers.anthropic.api_key"), "{err}");
        assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_file_defaults_apply() {
        // Model, endpoint and protocol come from `--model`/`--base-url`,
        // the file, or builtins — never env vars (those are provider
        // properties, not agent globals).
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_CONTEXT_WINDOW",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-filecfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "active_provider: opencode\nbase_url: https://file.example/v1\nmodel: file-model\ncontext_window: 1000\napi: openai-completions\ncustom_key: keep-me\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", &path);
        std::env::set_var("OPENCODE_API_KEY", "env-key");
        // File model + file base_url + file api apply; a `--model`
        // override beats the file.
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "file-model");
        assert_eq!(cfg.base_url, "https://file.example/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let cfg = LlmConfig::from_env(None, Some("flag-model".to_string()), None, &[]).unwrap();
        assert_eq!(cfg.model, "flag-model");
        // Write-back: one canonical `model: <endpoint>/<id>` key; the
        // redundant `active_provider:`/`base_url:` keys are dropped; unknown
        // keys survive.
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        cfg.apply_model("go/new-model", true).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model: go/new-model"), "{text}");
        assert!(
            !text
                .lines()
                .any(|l| { l.starts_with("active_provider:") || l.starts_with("base_url:") }),
            "redundant keys removed: {text}"
        );
        assert!(text.contains("api: openai-completions"), "{text}");
        assert!(text.contains("custom_key: keep-me"));
        // Learned protocol: remembered to the cache, picked up on the next
        // config build (nothing explicit pins this model's protocol).
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let cache = dir.join("cache");
        std::env::set_var("XDG_CACHE_HOME", &cache);
        remember_learned_api(
            "https://file.example/v1",
            "file-model",
            ApiProtocol::ChatCompletions,
        );
        assert!(cache.join("dex/learned-apis.json").exists());
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthetic catalog where each model lives on exactly one endpoint,
    /// so bare picks must move `base_url` without any prefix knowledge.
    fn write_routing_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "opencode": {
                    "api": "https://opencode.ai/zen/v1",
                    "models": { "m-zen-only": { "limit": { "context": 1 } } }
                },
                "opencode-go": {
                    "api": "https://go.example/v1",
                    "models": { "m-go-only": { "limit": { "context": 1 } } }
                },
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn bare_model_auto_routes_to_serving_endpoint() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME", "DEX_MODEL_APIS"]);
        let dir = std::env::temp_dir().join(format!("dex-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::remove_var("DEX_MODEL_APIS");
        let mut cfg = test_cfg();
        cfg.endpoints
            .insert("go".to_string(), "https://go.example/v1".to_string());
        // Bare pick living on another endpoint moves base_url by itself.
        assert_eq!(
            cfg.apply_model("m-go-only", false).unwrap().as_deref(),
            Some("go")
        );
        assert_eq!(cfg.base_url, "https://go.example/v1");
        assert_eq!(cfg.model, "m-go-only");
        // Back to a zen-only model.
        assert_eq!(
            cfg.apply_model("m-zen-only", false).unwrap().as_deref(),
            Some("zen")
        );
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // A model the current endpoint serves never moves — dual-served ids
        // must not ping-pong between endpoints.
        assert_eq!(cfg.apply_model("m-zen-only", false).unwrap(), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // Unknown models and custom URLs stay put.
        assert_eq!(cfg.apply_model("m-unknown", false).unwrap(), None);
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        cfg.base_url = "https://custom.example/v1".to_string();
        assert_eq!(cfg.apply_model("m-go-only", false).unwrap(), None);
        assert_eq!(cfg.base_url, "https://custom.example/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_base_url_wins_over_catalog_routing() {
        // An explicit pin holds even when it names a known endpoint: a
        // go-only model stays on the pinned URL instead of being silently
        // rerouted to the serving endpoint. Pins are `--base-url` and file
        // `base_url:` only — endpoint properties, not env globals.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m-zen\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        // `--base-url` pin with a `--model` override.
        let cfg = LlmConfig::from_env(
            Some("https://opencode.ai/zen/v1".to_string()),
            Some("m-go-only".to_string()),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(cfg.model, "m-go-only");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        // File `base_url:` pin with a file model.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nbase_url: https://opencode.ai/zen/v1\nmodel: m-go-only\n",
        )
        .unwrap();
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-go-only");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Synthetic catalog with a third-party OpenAI-compatible provider
    /// ("zai") so the generic provider layer can be exercised end to end.
    fn write_generic_catalog(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "zai": {
                    "api": "https://api.zai.example/v4",
                    "env": { "ZAI_TEST_KEY": "z.ai api key" },
                    "models": {
                        "glm-x": {
                            "limit": { "context": 1 },
                            "reasoning_options": [
                                { "type": "effort", "values": ["low", "high"] }
                            ]
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    fn generic_config(dir: &std::path::Path, providers_yaml: &str) {
        std::fs::write(
            dir.join("config.yaml"),
            format!("active_provider: zai\nmodel: zai/glm-x\n{providers_yaml}"),
        )
        .unwrap();
    }

    #[test]
    fn generic_provider_resolves_endpoint_key_and_routing() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
            "ZAI_TEST_KEY",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-generic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        for key in [
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "ZAI_TEST_KEY",
        ] {
            std::env::remove_var(key);
        }
        // Endpoint + key from the deposit place; catalog supplies the URL.
        // (`--model` stands in for the removed model env override.)
        let cfg = LlmConfig::from_env(None, Some("glm-x".to_string()), None, &[]).unwrap();
        assert_eq!(cfg.provider.name(), "zai");
        assert_eq!(cfg.base_url, "https://api.zai.example/v4");
        assert_eq!(cfg.api_key, "zsk-deposit");
        assert_eq!(
            cfg.endpoints.get("zai").map(String::as_str),
            Some("https://api.zai.example/v4")
        );
        // `/model zai/glm-x` is a no-op (already there); unknown prefixes
        // must still stay plain model ids.
        let mut cfg = cfg;
        assert!(cfg.apply_model("unknown/m", false).unwrap().is_none());
        assert_eq!(cfg.model, "unknown/m");
        // Key falls back to the provider's own conventional env var.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: zai\nmodel: zai/glm-x\nproviders:\n  zai: {}\n",
        )
        .unwrap();
        std::env::set_var("ZAI_TEST_KEY", "zsk-from-env");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api_key, "zsk-from-env");
        // No key anywhere: the error names both deposit places.
        std::env::remove_var("ZAI_TEST_KEY");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error"),
        };
        assert!(err.contains("providers.zai.api_key"), "{err}");
        assert!(err.contains("ZAI_TEST_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generic_provider_switch_and_completion_ids() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-gswitch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        generic_config(&dir, "providers:\n  zai:\n    api_key: zsk-deposit\n");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::remove_var("DEX_PROVIDER");
        // Completion list offers the configured provider's qualified ids.
        let ids = load_dex_models_cache().unwrap();
        assert!(ids.contains(&"glm-x".to_string()));
        assert!(ids.contains(&"zai/glm-x".to_string()));
        // Provider-qualified pick switches to the generic provider.
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(cfg.apply_model("zai/glm-x", false).unwrap().is_none()); // already on zai
        assert_eq!(cfg.model, "glm-x");
        // From opencode, the same pick switches provider AND endpoint.
        let mut cfg = test_cfg();
        cfg.provider_entries = load_provider_entries(&load_config_file());
        assert_eq!(cfg.apply_model("zai/glm-x", false).unwrap(), None);
        assert_eq!(cfg.provider.name(), "zai");
        assert_eq!(cfg.base_url, "https://api.zai.example/v4");
        // Thinking options come from the catalog for the selected model.
        assert_eq!(
            reasoning_options_for("glm-x"),
            Some(vec!["low".to_string(), "high".to_string()])
        );
        assert_eq!(reasoning_options_for("m-unknown"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn models_cache_offers_endpoint_qualified_ids() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-mlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_routing_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let ids = load_dex_models_cache().unwrap();
        // Bare ids for every catalog model, plus endpoint-qualified variants
        // so a pick can name its endpoint explicitly (`zen/…` vs `go/…`).
        assert!(ids.contains(&"m-zen-only".to_string()));
        assert!(ids.contains(&"zen/m-zen-only".to_string()));
        assert!(ids.contains(&"m-go-only".to_string()));
        assert!(ids.contains(&"go/m-go-only".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prefixed_selection_restores_protocol_on_restart() {
        // A `model: go/<id>` in the config file must honor a bare-id
        // `DEX_MODEL_APIS` entry: the full selection key is tried first,
        // then the stripped id.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-restart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: go/m-z9\ncontext_window: 1000\n",
        )
        .unwrap();
        // Empty catalog dir: no routing interference, unknown model stays.
        std::fs::create_dir_all(dir.join("cache/dex")).unwrap();
        std::fs::write(dir.join("cache/dex/models.dev.json"), "{}").unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_MODEL_APIS", "m-z9=openai-completions");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-z9");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persisted_selection_reloads_stripped() {
        // Persisted `model:` must reload onto the same endpoint without the
        // raw prefix: `from_env` treats a file `base_url:` as an explicit
        // pin and skips routing, so a stored `go/<id>` would otherwise be
        // sent to the API verbatim after restart.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-persist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Catalog whose serving URLs are exactly the builtin endpoints.
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "opencode": {
                    "api": "https://opencode.ai/zen/v1",
                    "models": { "m-zen": { "limit": { "context": 1 } } }
                },
                "opencode-go": {
                    "api": "https://opencode.ai/zen/go/v1",
                    "models": { "m-go": { "limit": { "context": 1 } } }
                },
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m-zen\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        cfg.apply_model("go/m-go", true).unwrap();
        assert_eq!(cfg.model, "m-go");
        // Restart with the persisted file (now carrying a `base_url:`, i.e.
        // the explicit-pin path): the id stays stripped, the endpoint holds.
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-go");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        // And back to a zen model, bare this time.
        let mut cfg = cfg;
        cfg.apply_model("m-zen", true).unwrap();
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-zen");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_base_url_still_resolves_per_model_protocol() {
        // A file `base_url:` pins routing only: the per-model table and the
        // learned fallback must still decide the wire protocol. Otherwise a
        // persisted `/model go/<id>` (which always writes `base_url:`)
        // retries `/responses` on every restart and a `DEX_MODEL_APIS` pin
        // is ignored for the request yet blocks the fallback — the exact
        // error loop in the glm-5.3-flash report.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-pin-proto-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cache/dex")).unwrap();
        std::fs::write(dir.join("cache/dex/models.dev.json"), "{}").unwrap();
        // Persisted state after `/model go/m-go`: stripped id + pinned URL.
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nbase_url: https://opencode.ai/zen/go/v1\nmodel: m-go\ncontext_window: 1000\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in ["DEX_PROVIDER", "DEX_MODELS", "DEX_CONTEXT_WINDOW"] {
            std::env::remove_var(key);
        }
        // Per-model table entry wins over the responses default.
        std::env::set_var("DEX_MODEL_APIS", "m-go=openai-completions");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "m-go");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        // Without the table, the learned fallback decides instead.
        std::env::remove_var("DEX_MODEL_APIS");
        remember_learned_api(
            "https://opencode.ai/zen/go/v1",
            "m-go",
            ApiProtocol::ChatCompletions,
        );
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_model_refuses_provider_switch_without_credentials_or_endpoint() {
        // A provider-qualified pick is atomic: without credentials or a
        // known endpoint the switch is refused and the old key/URL stay
        // put — never one provider's key against another's endpoint.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "OPENCODE_API_KEY",
            "CODEX_ACCESS_TOKEN",
            "CODEX_ACCOUNT_ID",
            "CODEX_HOME",
            "XDG_CACHE_HOME",
        ]);
        for key in ["OPENCODE_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID"] {
            std::env::remove_var(key);
        }
        // Hermetic XDG/CODEX_HOME: no credential file, no catalog.
        let dir = std::env::temp_dir().join(format!("dex-refuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("CODEX_HOME", dir.join("codex-home"));
        let mut cfg = test_cfg();
        assert!(cfg.apply_model("openai-codex/gpt-x", false).is_err());
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(cfg.model, "m-r");
        // Configured generic provider with neither key nor endpoint.
        cfg.provider_entries
            .insert("zai".to_string(), ProviderEntry::default());
        assert!(cfg.apply_model("zai/glm-x", false).is_err());
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.api_key, "k");
        assert_eq!(cfg.base_url, "https://opencode.ai/zen/v1");
        assert_eq!(cfg.model, "m-r");
        // Key deposited but no endpoint anywhere: same refusal.
        cfg.provider_entries.insert(
            "zai-keyed".to_string(),
            ProviderEntry {
                api_key: Some("zsk-x".to_string()),
                ..Default::default()
            },
        );
        assert!(cfg.apply_model("zai-keyed/glm-x", false).is_err());
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.api_key, "k");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_env_vars_tries_every_documented_name() {
        // A provider documenting several key env vars accepts any of them —
        // not just the first key in the catalog `env` map.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
            "ZAI_A_KEY",
            "ZAI_B_KEY",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-multienv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(
            dir.join("dex/models.dev.json"),
            serde_json::json!({
                "zai": {
                    "api": "https://api.zai.example/v4",
                    "env": { "ZAI_B_KEY": "second", "ZAI_A_KEY": "first" },
                    "models": { "glm-x": { "limit": { "context": 1 } } }
                }
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: zai\nmodel: zai/glm-x\nproviders:\n  zai: {}\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        for key in [
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "ZAI_A_KEY",
            "ZAI_B_KEY",
        ] {
            std::env::remove_var(key);
        }
        // Only the non-first documented name is set.
        std::env::set_var("ZAI_A_KEY", "a-key");
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "a-key"
        );
        // Neither set: the error names every documented name.
        std::env::remove_var("ZAI_A_KEY");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected missing-key error"),
        };
        assert!(err.contains("ZAI_A_KEY"), "{err}");
        assert!(err.contains("ZAI_B_KEY"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_provider_key_still_selects() {
        // Pre-rename files used `provider:` for the selection pointer;
        // they keep loading (with a one-time stderr warning), and the next
        // write-back migrates the pointer to `active_provider:`.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-legacyprov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "provider: opencode\nmodel: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    api_key: deposited\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", &path);
        std::env::remove_var("OPENCODE_API_KEY");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.api_key, "deposited");
        // Write-back stores one canonical key and drops the legacy ones.
        let endpoints: std::collections::BTreeMap<String, String> = [
            ("zen".to_string(), "https://opencode.ai/zen/v1".to_string()),
            (
                "go".to_string(),
                "https://opencode.ai/zen/go/v1".to_string(),
            ),
        ]
        .into_iter()
        .collect();
        persist_selection(
            "m",
            &Provider::OpenCode,
            "https://opencode.ai/zen/v1",
            &endpoints,
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model: zen/m"), "{text}");
        assert!(
            !text.lines().any(|l| {
                l.starts_with("provider:")
                    || l.starts_with("active_provider:")
                    || l.starts_with("base_url:")
            }),
            "legacy keys removed: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn api_pin_bakes_into_config() {
        // The global protocol pin is computed once in `from_env` (file or
        // provider entry) so hot paths never re-read the file.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-pinbake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\n",
        )
        .unwrap();
        assert!(
            !LlmConfig::from_env(None, None, None, &[])
                .unwrap()
                .api_pinned
        );
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\napi: openai-completions\n",
        )
        .unwrap();
        assert!(
            LlmConfig::from_env(None, None, None, &[])
                .unwrap()
                .api_pinned
        );
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\nproviders:\n  opencode:\n    api: openai-completions\n",
        )
        .unwrap();
        assert!(
            LlmConfig::from_env(None, None, None, &[])
                .unwrap()
                .api_pinned
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolved_provider_bundles_entry_overrides() {
        // One derivation feeds every consumer: entry `base_url:` beats the
        // catalog URL, entry `api:` pins (and bakes `api_pinned`), entry
        // `headers:` land in `provider_headers` — and all three refresh on
        // a provider switch.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-resolved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: zai\nmodel: zai/glm-x\nproviders:\n  zai:\n    api_key: zsk-deposit\n    base_url: https://custom.zai.example/v1\n    api: openai-completions\n    headers:\n      X-Prov: prov\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ] {
            std::env::remove_var(key);
        }
        let mut cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.base_url, "https://custom.zai.example/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        assert!(cfg.api_pinned);
        assert_eq!(
            cfg.provider_headers.get("X-Prov").map(String::as_str),
            Some("prov")
        );
        // Switching providers refreshes the whole bundle, not just the URL:
        // endpoint, protocol base + pin, and scoped headers.
        cfg.apply_model("opencode/glm-x", false).unwrap();
        assert_eq!(cfg.provider, Provider::OpenCode);
        assert_eq!(cfg.base_url, Provider::OpenCode.default_base_url().unwrap());
        assert_eq!(cfg.api, ApiProtocol::Responses);
        assert!(!cfg.api_pinned);
        assert!(cfg.provider_headers.is_empty());
        // And back via `switch_provider`: pin, endpoint and headers return.
        cfg.switch_provider(&Provider::Generic("zai".into()), false)
            .unwrap();
        assert_eq!(cfg.base_url, "https://custom.zai.example/v1");
        assert_eq!(cfg.api, ApiProtocol::ChatCompletions);
        assert!(cfg.api_pinned);
        assert_eq!(
            cfg.provider_headers.get("X-Prov").map(String::as_str),
            Some("prov")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctx_map_covers_both_catalog_shapes_lowercased() {
        let catalog = serde_json::json!({
            "opencode": { "models": {
                "GPT-5": { "limit": { "context": 128000 } },
                "zero": { "limit": { "context": 0 } },
                "nodoc": {},
            } },
            "models": {
                "Custom-X": { "limit": { "context": 200000 } },
            },
        });
        let map = build_ctx_map(&catalog);
        assert_eq!(map.get("gpt-5"), Some(&128000));
        assert_eq!(map.get("custom-x"), Some(&200000));
        assert!(!map.contains_key("zero"));
        assert!(!map.contains_key("nodoc"));
    }

    #[test]
    fn thinking_effort_resolves_stored_then_env() {
        // Precedence: stored `/thinking` choice > `DEX_THINKING_EFFORT` >
        // unset; clearing falls back down the chain.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_THINKING_EFFORT",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
            "XDG_CONFIG_HOME",
            "DEX_CONFIG",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-think-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        // refresh_thinking_effort also falls back to the user config file;
        // a machine with `thinking_effort:` set there would fail the first
        // assertion, so point the config lookup at the empty temp dir too.
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::remove_var("DEX_CONFIG");
        std::env::remove_var("DEX_THINKING_EFFORT");
        let mut cfg = test_cfg();
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort, None);
        std::env::set_var("DEX_THINKING_EFFORT", "medium");
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
        remember_thinking_effort(&cfg.base_url, &cfg.model, Some("low"));
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
        assert_eq!(
            stored_thinking_effort(&cfg.base_url, &cfg.model).as_deref(),
            Some("low")
        );
        remember_thinking_effort(&cfg.base_url, &cfg.model, None);
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
        std::env::remove_var("DEX_THINKING_EFFORT");
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_pick_validates_against_advertised_options() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-thinkval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        // Case-insensitive pick, catalog casing back.
        assert_eq!(
            validate_thinking_effort("glm-x", "HIGH"),
            Ok("high".to_string())
        );
        // Rejected pick returns the valid options.
        assert_eq!(
            validate_thinking_effort("glm-x", "ultra"),
            Err(vec!["low".to_string(), "high".to_string()])
        );
        // Unknown model: accepted raw, a stale catalog never blocks.
        assert_eq!(
            validate_thinking_effort("m-unknown", "whatever"),
            Ok("whatever".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_mismatch_warning_is_data_not_stderr() {
        // The mismatch hint must be returnable data (transcript line), never
        // an `eprintln!` from config code: the daemon shares the TUI's
        // terminal, where stderr corrupts the alternate screen / OSC query.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-thinkwarn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_generic_catalog(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let mut cfg = test_cfg();
        cfg.model = "glm-x".into();
        cfg.thinking_effort = None;
        assert!(cfg.thinking_mismatch_warning().is_none());
        cfg.thinking_effort = Some("high".into());
        assert!(cfg.thinking_mismatch_warning().is_none());
        cfg.thinking_effort = Some("ultra".into());
        let warning = cfg.thinking_mismatch_warning().expect("mismatch warns");
        assert!(
            warning.contains("ultra") && warning.contains("low, high"),
            "{warning}"
        );
        cfg.model = "m-unknown".into();
        assert!(cfg.thinking_mismatch_warning().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_model_swaps_effort_with_the_model() {
        // A model switch moves the thinking knob to the new model's stored
        // choice instead of leaking the old one.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_THINKING_EFFORT",
            "DEX_CONTEXT_WINDOW",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-thinkswap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::remove_var("DEX_THINKING_EFFORT");
        let mut cfg = test_cfg();
        remember_thinking_effort(&cfg.base_url, "m-r", Some("low"));
        remember_thinking_effort(&cfg.base_url, "m-new", Some("high"));
        cfg.refresh_thinking_effort();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
        cfg.apply_model("m-new", false).unwrap();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("high"));
        std::env::set_var("DEX_THINKING_EFFORT", "medium");
        cfg.apply_model("m-bare", false).unwrap();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thinking_effort_reads_file_key_under_env() {
        // File `thinking_effort:` is the default under a stored choice and
        // the env var: stored > env > file > unset.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-thinkfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        std::fs::write(dir.join("dex/models.dev.json"), "{}").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nmodel: opencode/m\ncontext_window: 1000\nthinking_effort: low\n",
        )
        .unwrap();
        std::env::set_var("XDG_CACHE_HOME", &dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        for key in [
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "DEX_THINKING_EFFORT",
        ] {
            std::env::remove_var(key);
        }
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("low"));
        std::env::set_var("DEX_THINKING_EFFORT", "medium");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("medium"));
        remember_thinking_effort(&cfg.base_url, &cfg.model, Some("high"));
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.thinking_effort.as_deref(), Some("high"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `model: <provider>/<model>` (and `DEX_MODEL`) is the single selection
    /// knob: the prefix picks the provider, the rest the model.
    #[test]
    fn selection_prefix_and_dex_model_pick_provider_and_model() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-selection-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dex")).unwrap();
        let _cache = EnvRestore::take(&["XDG_CACHE_HOME"]);
        std::env::set_var("XDG_CACHE_HOME", &dir); // hermetic: no real catalog
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            "DEX_MODEL_APIS",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
        ]);
        let path = dir.join("config.yaml");
        std::fs::write(
            &path,
            "providers:\n  zai:\n    api_key: zk\n    base_url: https://zai.example/v1\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", &path);

        // File `model: zai/glm-5.3`
        std::fs::write(
            &path,
            "providers:\n  zai:\n    api_key: zk\n    base_url: https://zai.example/v1\nmodel: zai/glm-5.3\ncontext_window: 1000\n",
        )
        .unwrap();
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
        assert_eq!(cfg.model, "glm-5.3");
        assert_eq!(cfg.base_url, "https://zai.example/v1");
        assert_eq!(cfg.api_key, "zk");

        // `DEX_MODEL` beats the file without touching it (its window rides
        // the env pin).
        std::env::set_var("DEX_CONTEXT_WINDOW", "1000");
        std::env::set_var("DEX_MODEL", "zai/kimi-k2");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
        assert_eq!(cfg.model, "kimi-k2");

        // A bare provider name no longer implies a default model: it is
        // rejected, naming the `provider/<model>` form to use.
        std::env::set_var("DEX_MODEL", "zai");
        let err = match LlmConfig::from_env(None, None, None, &[]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("bare provider name must not resolve"),
        };
        assert!(err.contains("'zai' names a provider but no model"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_reports_selection_and_origins() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            crate::agent::obs_pack::OBSERVATION_PACK_ENV,
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::remove_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV);
        // Point at a missing file so the host config can't color the output.
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-missing-doctor-{}", std::process::id())),
        );
        let out = doctor(None, None, None, &[], None);
        assert!(out.contains("provider"), "{out}");
        assert!(out.contains("model"), "{out}");
        assert!(out.contains("OPENCODE_API_KEY"), "{out}");
        assert!(out.contains("built-in default"), "{out}");
        assert!(out.contains("resolve"), "{out}");
        // Observation pack row: default off, origin named.
        let obs = out
            .lines()
            .find(|l| l.starts_with("obs pack "))
            .expect("obs pack row");
        assert!(obs.contains("off"), "{obs}");
        assert!(obs.contains("built-in default"), "{obs}");
    }

    /// A bare provider pick (`DEX_MODEL=anthropic`) surfaces as the provider
    /// row, never as a model id: the model row is unset (the selection named
    /// as its origin) and the resolve row carries the fix.
    #[test]
    fn doctor_shows_bare_provider_pick_as_provider() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "OPENCODE_API_KEY"]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_MODEL", "anthropic");
        // Point at a missing file so the host config can't color the output.
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-bare-doctor-{}", std::process::id())),
        );
        let out = doctor(None, None, None, &[], None);
        let prov = out
            .lines()
            .find(|l| l.starts_with("provider "))
            .expect("provider row");
        assert!(prov.contains("anthropic"), "{prov}");
        assert!(prov.contains("DEX_MODEL"), "{prov}");
        let model = out
            .lines()
            .find(|l| l.starts_with("model "))
            .expect("model row");
        assert!(model.contains("(unset)"), "{model}");
        assert!(!model.contains("anthropic"), "{model}");
    }

    /// The obs pack row reflects the gate on and names the env var as its
    /// origin.
    #[test]
    fn doctor_reports_obs_pack_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            crate::agent::obs_pack::OBSERVATION_PACK_ENV,
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-obs-doctor-{}", std::process::id())),
        );
        std::env::set_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV, "1");
        let out = doctor(None, None, None, &[], None);
        let obs = out
            .lines()
            .find(|l| l.starts_with("obs pack "))
            .expect("obs pack row");
        assert!(obs.contains("on"), "{obs}");
        assert!(
            obs.contains(crate::agent::obs_pack::OBSERVATION_PACK_ENV),
            "{obs}"
        );
    }

    /// The evidence reducer row only appears behind its gate, reports the
    /// pack gate it depends on, and names the reducer model env var as the
    /// model origin when it is set.
    #[test]
    fn doctor_reports_evidence_reducer_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            crate::agent::evidence_reducer::GATE_ENV,
            crate::agent::obs_pack::OBSERVATION_PACK_ENV,
            crate::agent::evidence_reducer::MODEL_ENV,
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-evidence-doctor-{}", std::process::id())),
        );
        // Gate off: no row at all.
        std::env::remove_var(crate::agent::evidence_reducer::GATE_ENV);
        std::env::remove_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV);
        std::env::remove_var(crate::agent::evidence_reducer::MODEL_ENV);
        let out = doctor(None, None, None, &[], None);
        assert!(
            !out.lines().any(|l| l.starts_with("evidence reducer")),
            "gate off must not produce a row: {out}"
        );
        // Gate on but the pack off: the row explains what is missing.
        std::env::set_var(crate::agent::evidence_reducer::GATE_ENV, "1");
        let out = doctor(None, None, None, &[], None);
        let row = out
            .lines()
            .find(|l| l.starts_with("evidence reducer"))
            .expect("evidence reducer row");
        assert!(
            row.contains(&format!(
                "needs {}=1",
                crate::agent::obs_pack::OBSERVATION_PACK_ENV
            )),
            "{row}"
        );
        assert!(row.contains("main model"), "{row}");
        // Pack on: fully enabled, and an explicit reducer model is named
        // with its env origin.
        std::env::set_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV, "1");
        std::env::set_var(
            crate::agent::evidence_reducer::MODEL_ENV,
            "openrouter/z-ai/glm-4.5-air",
        );
        let out = doctor(None, None, None, &[], None);
        let row = out
            .lines()
            .find(|l| l.starts_with("evidence reducer"))
            .expect("evidence reducer row");
        assert!(row.contains("on ·"), "{row}");
        assert!(row.contains("openrouter/z-ai/glm-4.5-air"), "{row}");
        assert!(
            row.contains(crate::agent::evidence_reducer::MODEL_ENV),
            "{row}"
        );
    }

    /// Rows whose value overflows the value column wrap instead of
    /// colliding with the origin text; the origin hangs at the origin
    /// column (display columns 18+46 = 64).
    #[test]
    fn doctor_wraps_overlong_value_and_hangs_origin() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
        ]);
        // Missing file, so the config row prints the path + a source note.
        std::env::set_var(
            "DEX_CONFIG",
            format!(
                "{}/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/config.yaml",
                std::env::temp_dir().display()
            ),
        );
        let out = doctor(None, None, None, &[], None);
        let mut lines = out.lines();
        while let Some(line) = lines.next() {
            if !line.starts_with("config ") {
                continue;
            }
            // The whole path is the only text on its own line.
            assert!(
                line.trim_end().ends_with("config.yaml"),
                "value keeps its line: {line:?}"
            );
            // The origin starts at the origin column on the next line.
            let origin = lines.next().expect("origin line");
            assert_eq!(
                origin.find("missing or invalid"),
                Some(18 + 46),
                "origin hangs at the origin column: {origin:?}"
            );
            return;
        }
        panic!("no config row in:\n{out}");
    }

    /// Padding counts display columns: a CJK path is 31 chars but only 45
    /// columns wide, so the origin still lands on column 64.
    #[test]
    fn doctor_pads_by_display_width() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
        ]);
        // "/tmp/" (5 cols) + 14 CJK chars (28 cols) + "/config.yaml" (12 cols) = 45.
        std::env::set_var(
            "DEX_CONFIG",
            format!("/tmp/{}/config.yaml", "配置文件配置文件配置文件配置"),
        );
        let out = doctor(None, None, None, &[], None);
        let line = out
            .lines()
            .find(|l| l.starts_with("config "))
            .expect("config row");
        let at = line
            .find("missing or invalid")
            .expect("origin inline on the value line");
        assert_eq!(
            UnicodeWidthStr::width(&line[..at]),
            18 + 46,
            "origin starts at display column 64: {line:?}"
        );
    }

    /// Byte-for-byte `doctor` output under a fully hermetic scenario: no
    /// config file, no catalog caches, no dex env vars, one provider key.
    /// This is the PR-18 motion gate — any refactor of the shared
    /// resolution must reproduce this output exactly. Fixed paths (no pid)
    /// keep the snapshot stable across runs and machines.
    #[test]
    fn doctor_output_is_byte_stable() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "DEX_PROVIDER",
            "DEX_MODEL",
            "DEX_MODELS",
            "DEX_CONTEXT_WINDOW",
            "DEX_RESERVE_TOKENS",
            "DEX_KEEP_RECENT_TOKENS",
            "DEX_THINKING_EFFORT",
            "DEX_SYSTEM_PROMPT",
            "DEX_SYSTEM_PROMPT_FILE",
            "DEX_PERMISSION",
            "DEX_HEADERS",
            crate::agent::online_compaction::ONLINE_COMPACTION_ENV,
            crate::agent::obs_pack::OBSERVATION_PACK_ENV,
            crate::agent::evidence_reducer::GATE_ENV,
            crate::agent::evidence_reducer::MODEL_ENV,
            "DEX_AGENT_WAKE",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
            "ANTHROPIC_CUSTOM_HEADERS",
            "OPENAI_HEADERS",
            "OPENCODE_API_KEY",
            "XDG_CONFIG_HOME",
            "DEX_EXTENSIONS_PATHS",
        ]);
        std::env::remove_var("DEX_EXTENSIONS_PATHS");
        // `EnvRestore::take` saves-and-restores; the toggles that flip doctor
        // rows must be cleared outright, so a developer shell with the
        // experiment gates set does not drift the byte-stable output.
        std::env::remove_var(crate::agent::online_compaction::ONLINE_COMPACTION_ENV);
        std::env::remove_var(crate::agent::obs_pack::OBSERVATION_PACK_ENV);
        std::env::remove_var(crate::agent::evidence_reducer::GATE_ENV);
        std::env::remove_var(crate::agent::evidence_reducer::MODEL_ENV);
        std::env::remove_var("DEX_SYSTEM_PROMPT");
        std::env::remove_var("DEX_SYSTEM_PROMPT_FILE");
        std::env::remove_var("DEX_ROUTING");
        std::env::remove_var("DEX_ROUTING_FAST");
        std::env::remove_var("DEX_ROUTING_BALANCED");
        std::env::remove_var("DEX_ROUTING_POWERFUL");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var("DEX_CONFIG", "/tmp/dex-doctor-snapshot/missing.yaml");
        std::env::set_var("XDG_CACHE_HOME", "/tmp/dex-doctor-snapshot/cache");
        std::env::set_var("XDG_DATA_HOME", "/tmp/dex-doctor-snapshot/data");
        // Hermetic extension discovery too: the extensions row reads the
        // XDG config dir, which must not see the developer's real installs.
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/dex-doctor-snapshot/config");
        let out = doctor(None, None, None, &[], None);
        // The online row is built from the experiment module's own env
        // const so config.rs never names the gate; the padding is derived
        // from the value width (origin column = 18+46) instead of
        // hand-counted spaces, so an env rename tracks cleanly.
        let online_value = format!(
            "off (set {}=1)",
            crate::agent::online_compaction::ONLINE_COMPACTION_ENV
        );
        let online_row = format!(
            "online compaction {online_value}{}built-in default (off)\n",
            " ".repeat(46 - online_value.chars().count())
        );
        let expected = format!(
            "{}{}{}",
            concat!(
                concat!("dex ", env!("CARGO_PKG_VERSION"), "\n"),
                "\n",
                "config            /tmp/dex-doctor-snapshot/missing.yaml         missing or invalid — ignored (env/defaults still apply)\n",
                "catalog           /tmp/dex-doctor-snapshot/cache/dex/models.dev.json\n",
                "                                                                missing — run `dex update --models`\n",
                "\n",
                "provider          opencode                                      built-in default\n",
                "model             (unset)                                       UNCONFIGURED — set 'model: <provider>/<model>'\n",
                "base_url          https://opencode.ai/zen/v1                    built-in default\n",
                "api key           (hidden)                                      OPENCODE_API_KEY (environment)\n",
                "protocol          openai-responses                              default (auto-fallback to completions)\n",
                "context           UNKNOWN tokens                                no catalog entry for this model — set context_window: or DEX_CONTEXT_WINDOW\n",
            ),
            online_row,
            concat!(
                "obs pack          off                                           built-in default (off)\n",
                "thinking          (unset)                                       model default\n",
                "permission        trusted                                       built-in default\n",
                "agent wake        on                                            built-in default\n",
                "routing           off                                           built-in default\n",
                "routing fast      (unset)                                       UNCONFIGURED — set 'model: <provider>/<model>'\n",
                "routing balanced  (unset)                                       UNCONFIGURED — set 'model: <provider>/<model>'\n",
                "routing powerful  (unset)                                       UNCONFIGURED — set 'model: <provider>/<model>'\n",
                "headers           0                                             none\n",
                "endpoints         go, zen                                       available to /model routing\n",
                "system prompt     default                                       built-in default\n",
                "extensions        none                                          cwd/.dex, XDG config dirs\n",
                "\n",
                "resolve           ERROR                                         no model configured — set 'model: <provider>/<model>' in the config, then run `dex doctor`:\n",
                "                                                                  opencode: model: zen/<model-id> + providers.opencode.api_key (or OPENCODE_API_KEY)\n",
                "                                                                  anthropic: model: anthropic/<model-id> + providers.anthropic.api_key (or ANTHROPIC_API_KEY)\n",
                "                                                                  custom gateway (Bearer + Anthropic wire): model: gateway/<model-id> + providers.gateway: {base_url: https://gateway.example/v1, api_key, api: anthropic-messages}\n",
                "                                                                  codex: model: openai-codex/<model-id> + run `codex --login` (or CODEX_ACCESS_TOKEN)\n",
                "                                                                config: /tmp/dex-doctor-snapshot/missing.yaml\n",
            )
        );
        assert_eq!(out, expected, "doctor output drifted");
    }

    /// Complexity router: off with unset tiers by default; file `routing:`
    /// parses the switch plus per-tier selections; env beats file per tier;
    /// an unknown tier key and a non-string tier warn and fall through
    /// instead of erroring (the typo policy `load_config_file` uses).
    #[test]
    fn routing_resolution_reads_switch_tiers_and_env() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
            "XDG_CACHE_HOME",
        ]);
        for key in [
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
        ] {
            std::env::remove_var(key);
        }
        // Default: off, every tier unset.
        let r = super::routing_resolution(&None);
        assert!(!r.enabled);
        assert_eq!(r.enabled_origin, "built-in default");
        assert!(r.tiers.fast.is_empty());
        assert!(r.tiers.balanced.is_empty());
        assert!(r.tiers.powerful.is_empty());
        // File switch + tiers; env wins one tier; garbage warns through.
        let dir = std::env::temp_dir().join(format!("dex-routing-res-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\nrouting:\n  enabled: true\n  fast: myprov/cheap\n  balanced: myprov/mid\n  powerful: 7\n  bogus: myprov/nope\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("DEX_ROUTING_POWERFUL", "myprov/fast");
        let file = super::load_config_file();
        let r = super::routing_resolution(&file);
        assert!(r.enabled);
        assert_eq!(r.enabled_origin, "config routing.enabled:");
        assert_eq!(r.tiers.fast, "myprov/cheap");
        assert_eq!(r.tier_origins.fast, "config routing.fast:");
        assert_eq!(r.tiers.balanced, "myprov/mid");
        // Non-string file tier ignored; env fills the gap with env origin.
        assert_eq!(r.tiers.powerful, "myprov/fast");
        assert_eq!(r.tier_origins.powerful, "DEX_ROUTING_POWERFUL");
        std::env::remove_var("DEX_ROUTING_POWERFUL");
        // Unknown tier keys are ignored: only fast/balanced/powerful exist.
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\nrouting:\n  enabled: true\n  low: myprov/cheap\n  medium: myprov/mid\n",
        )
        .unwrap();
        let file = super::load_config_file();
        let r = super::routing_resolution(&file);
        assert!(r.tiers.fast.is_empty());
        assert!(r.tiers.balanced.is_empty());
        assert!(r.tiers.powerful.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `route_turn`: `None` when routing is off; otherwise the classified
    /// tier resolves through `routing.balanced:` → `model:`, overriding only
    /// when the tier names a different selection (no pointless rebuilds).
    #[test]
    fn route_turn_classifies_and_overrides_only_on_change() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
            "XDG_CACHE_HOME",
        ]);
        for key in [
            "DEX_MODEL",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
        ] {
            std::env::remove_var(key);
        }
        let dir = std::env::temp_dir().join(format!("dex-routing-turn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\nrouting:\n  enabled: true\n  fast: myprov/cheap\n  balanced: myprov/m-7\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        // Trivial prompt → fast tier → override to the cheap model.
        let routed = super::route_turn("fix typo", &[]).expect("routing on");
        assert_eq!(routed.tier, crate::agent::router::Tier::Fast);
        assert_eq!(routed.model_override.as_deref(), Some("myprov/cheap"));
        assert!(!routed.reason.is_empty());
        // Ordinary work with history behind it → balanced, which already is
        // the selection → no override, the config never rebuilds.
        let history = vec![crate::core::types::ChatMessage::user("x".repeat(20_000))];
        let routed =
            super::route_turn("add a retry to the fetch call", &history).expect("routing on");
        assert_eq!(routed.tier, crate::agent::router::Tier::Balanced);
        assert_eq!(routed.model_override, None);
        // Migration work escalates; unset powerful falls back to balanced.
        let routed = super::route_turn("run the database migration", &[]).expect("routing on");
        assert_eq!(routed.tier, crate::agent::router::Tier::Powerful);
        assert_eq!(routed.model_override, None);
        // Routing off → None even for a powerful-shaped prompt.
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n",
        )
        .unwrap();
        assert!(super::route_turn("run the database migration", &[]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `dex doctor` shows the routing switch plus each tier's resolved
    /// model and origin — an unset tier displays the `balanced` fallback it
    /// would actually use at runtime.
    #[test]
    fn doctor_shows_routing_tiers_with_origins() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
            "OPENCODE_API_KEY",
            "XDG_CACHE_HOME",
        ]);
        for key in [
            "DEX_MODEL",
            "DEX_ROUTING",
            "DEX_ROUTING_FAST",
            "DEX_ROUTING_BALANCED",
            "DEX_ROUTING_POWERFUL",
        ] {
            std::env::remove_var(key);
        }
        let dir = std::env::temp_dir().join(format!("dex-routing-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\nrouting:\n  enabled: true\n  fast: myprov/cheap\n  balanced: myprov/mid\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        let out = super::doctor(None, None, None, &[], None);
        let routing: Vec<&str> = out.lines().filter(|l| l.starts_with("routing")).collect();
        assert_eq!(routing.len(), 4, "{out}");
        assert!(routing[0].contains("on"), "{}", routing[0]);
        assert!(
            routing[0].contains("config routing.enabled:"),
            "{}",
            routing[0]
        );
        assert!(routing[1].contains("myprov/cheap"), "{}", routing[1]);
        assert!(
            routing[1].contains("config routing.fast:"),
            "{}",
            routing[1]
        );
        // Unset powerful shows the balanced fallback with balanced's origin —
        // the same chain `model_for` applies at runtime.
        assert!(routing[3].contains("myprov/mid"), "{}", routing[3]);
        assert!(
            routing[3].contains("config routing.balanced:"),
            "{}",
            routing[3]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_precedence_is_cli_env_file_default() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_SYSTEM_PROMPT",
            "DEX_SYSTEM_PROMPT_FILE",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-sprompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prompt_file = dir.join("prompt.md");
        std::fs::write(&prompt_file, "file prompt").unwrap();
        let env_file = dir.join("env.md");
        std::fs::write(&env_file, "env file prompt").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: opencode/m\ncontext_window: 1000\nsystem_prompt: file inline\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
            std::env::remove_var(key);
        }
        // File inline wins over nothing.
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text.as_deref(), Some("file inline"));
        assert_eq!(origin, "config system_prompt:");
        // Env inline beats file.
        std::env::set_var("DEX_SYSTEM_PROMPT", "env inline");
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text.as_deref(), Some("env inline"));
        assert_eq!(origin, "DEX_SYSTEM_PROMPT");
        // Explicit CLI text beats env.
        let (text, origin) = super::system_prompt_origin(Some("cli text"));
        assert_eq!(text.as_deref(), Some("cli text"));
        assert_eq!(origin, "--system-prompt");
        std::env::remove_var("DEX_SYSTEM_PROMPT");
        // Env file beats file inline; file `system_prompt_file:` is the last
        // layer before the default.
        std::env::set_var("DEX_SYSTEM_PROMPT_FILE", &env_file);
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text.as_deref(), Some("env file prompt"));
        assert_eq!(origin, "DEX_SYSTEM_PROMPT_FILE");
        std::env::remove_var("DEX_SYSTEM_PROMPT_FILE");
        // Within the file layer, inline beats file.
        std::fs::write(
            dir.join("config.yaml"),
            format!(
                "model: opencode/m\ncontext_window: 1000\nsystem_prompt: inline wins\nsystem_prompt_file: {}\n",
                prompt_file.display()
            ),
        )
        .unwrap();
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text.as_deref(), Some("inline wins"));
        assert_eq!(origin, "config system_prompt:");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_prompt_file_layer_reads_absolute_path() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_SYSTEM_PROMPT",
            "DEX_SYSTEM_PROMPT_FILE",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-spfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prompt_file = dir.join("prompt.md");
        std::fs::write(&prompt_file, "from file layer").unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            format!(
                "model: opencode/m\ncontext_window: 1000\nsystem_prompt_file: {}\n",
                prompt_file.display()
            ),
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
            std::env::remove_var(key);
        }
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text.as_deref(), Some("from file layer"));
        assert_eq!(origin, "config system_prompt_file:");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_cli_system_prompt_prefers_inline_and_errors_on_missing_file() {
        let dir = std::env::temp_dir().join(format!("dex-clicfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prompt_file = dir.join("p.md");
        std::fs::write(&prompt_file, "cli file text").unwrap();
        // Inline wins when both are set.
        assert_eq!(
            super::resolve_cli_system_prompt(
                Some("inline".to_string()),
                Some(prompt_file.display().to_string()),
            )
            .unwrap(),
            Some(("inline".to_string(), "--system-prompt"))
        );
        // File content is returned verbatim with the file-flag origin.
        assert_eq!(
            super::resolve_cli_system_prompt(None, Some(prompt_file.display().to_string()))
                .unwrap(),
            Some(("cli file text".to_string(), "--system-prompt-file"))
        );
        // Whitespace-only file content counts as unset and falls through.
        std::fs::write(dir.join("blank.md"), "   \n").unwrap();
        assert_eq!(
            super::resolve_cli_system_prompt(
                None,
                Some(dir.join("blank.md").display().to_string())
            )
            .unwrap(),
            None
        );
        // Missing file is a hard error, never a silent default.
        assert!(super::resolve_cli_system_prompt(
            None,
            Some(dir.join("missing.md").display().to_string())
        )
        .is_err());
        assert_eq!(super::resolve_cli_system_prompt(None, None).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_system_prompt_row_names_origin() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "OPENCODE_API_KEY",
            "DEX_SYSTEM_PROMPT",
            "DEX_SYSTEM_PROMPT_FILE",
            "XDG_CACHE_HOME",
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-spdoc-{}", std::process::id())),
        );
        std::env::set_var(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("dex-spdoc-cache-{}", std::process::id())),
        );
        for key in ["DEX_MODEL", "DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
            std::env::remove_var(key);
        }
        let out = doctor(None, None, None, &[], None);
        let prow = out
            .lines()
            .find(|l| l.starts_with("system prompt "))
            .expect("system prompt row");
        assert!(prow.contains("default"), "{prow}");
        assert!(prow.contains("built-in default"), "{prow}");
        std::env::set_var("DEX_SYSTEM_PROMPT", "custom base");
        let out = doctor(None, None, None, &[], None);
        let prow = out
            .lines()
            .find(|l| l.starts_with("system prompt "))
            .expect("system prompt row");
        assert!(prow.contains("custom (11 chars)"), "{prow}");
        assert!(prow.contains("DEX_SYSTEM_PROMPT"), "{prow}");
        // Explicit CLI text wins and reports as a flag.
        let out = doctor(
            None,
            None,
            None,
            &[],
            Some(("cli".to_string(), "--system-prompt")),
        );
        let prow = out
            .lines()
            .find(|l| l.starts_with("system prompt "))
            .expect("system prompt row");
        assert!(prow.contains("--system-prompt"), "{prow}");
        // File-flag text reports its own origin.
        let out = doctor(
            None,
            None,
            None,
            &[],
            Some(("cli".to_string(), "--system-prompt-file")),
        );
        let prow = out
            .lines()
            .find(|l| l.starts_with("system prompt "))
            .expect("system prompt row");
        assert!(prow.contains("--system-prompt-file"), "{prow}");
    }

    #[test]
    fn whitespace_file_inline_system_prompt_falls_through_to_default() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_SYSTEM_PROMPT",
            "DEX_SYSTEM_PROMPT_FILE",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-spblank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: opencode/m\ncontext_window: 1000\nsystem_prompt: '   '\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_SYSTEM_PROMPT", "DEX_SYSTEM_PROMPT_FILE"] {
            std::env::remove_var(key);
        }
        let (text, origin) = super::system_prompt_origin(None);
        assert_eq!(text, None);
        assert_eq!(origin, "built-in default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Hermetic config for the extension snapshot tests: a generic provider
    /// with its own endpoint + key, so resolution stays cache-less (no
    /// catalog, no env keys).
    fn write_extmodel_config(dir: &std::path::Path) {
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n",
        )
        .unwrap();
    }

    #[test]
    fn extension_model_snapshot_resolves_selection_and_endpoint() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-extmodel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_extmodel_config(&dir);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_MODEL", "DEX_PROVIDER", "DEX_MODEL_APIS"] {
            std::env::remove_var(key);
        }
        let snap = super::extension_model_snapshot().unwrap();
        assert_eq!(snap.provider, "myprov");
        assert_eq!(snap.model, "m-7");
        assert_eq!(snap.base_url, "https://myprov.example/v1");
        assert_eq!(snap.api, "openai-responses");
        assert_eq!(snap.id(), "myprov/m-7");
        let auth = super::extension_model_auth().unwrap();
        assert_eq!(auth.api_key, "k-123");
        assert_eq!(auth.base_url, "https://myprov.example/v1");
        assert!(auth
            .headers
            .keys()
            .all(|k| !k.eq_ignore_ascii_case("authorization")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_model_snapshot_errors_without_selection() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard =
            EnvRestore::take(&["DEX_CONFIG", "DEX_MODEL", "DEX_PROVIDER", "XDG_CACHE_HOME"]);
        let dir = std::env::temp_dir().join(format!("dex-extmodel-no-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "context_window: 1000\n").unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_MODEL", "DEX_PROVIDER"] {
            std::env::remove_var(key);
        }
        let err = super::extension_model_snapshot().unwrap_err();
        assert!(err.contains("no model configured"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_model_auth_merges_headers_and_drops_authorization() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_PROVIDER",
            "DEX_HEADERS",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-extmodel-h-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nheaders:\n  X-A: global\n  authorization: Bearer file-key\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n    headers:\n      X-A: scoped\n      X-B: scoped\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_MODEL", "DEX_PROVIDER", "DEX_HEADERS"] {
            std::env::remove_var(key);
        }
        let auth = super::extension_model_auth().unwrap();
        // Provider-scoped entries beat the global table per key (AGENTS.md
        // precedence); `authorization` never travels with the headers (the
        // key goes separately).
        assert_eq!(auth.headers.get("X-A").map(String::as_str), Some("scoped"));
        assert_eq!(auth.headers.get("X-B").map(String::as_str), Some("scoped"));
        assert!(auth
            .headers
            .keys()
            .all(|k| !k.eq_ignore_ascii_case("authorization")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_provider_names_stay_in_sync_with_parse_known() {
        // `extension_configured_providers` seeds discovery from this list; every
        // entry must resolve without any file entry, or builtins silently drop
        // out of `net.providers` / `dex.model.providers()`.
        let known = std::collections::BTreeSet::new();
        for name in crate::core::types::Provider::BUILTINS {
            assert!(
                crate::core::types::Provider::parse_known(name, &known).is_some(),
                "BUILTINS entry '{name}' must parse without any file entry"
            );
        }
        // Alias spellings land on one canonical provider.
        assert_eq!(
            crate::core::types::Provider::parse_known("codex", &known)
                .map(|p| p.name().to_string()),
            crate::core::types::Provider::parse_known("openai-codex", &known)
                .map(|p| p.name().to_string()),
        );
    }

    #[test]
    fn extension_provider_auth_resolves_explicit_provider() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-provauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n  anthropic:\n    base_url: https://anthropic.example\n    api_key: k-ant\n    api: anthropic-messages\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_MODEL", "DEX_PROVIDER", "DEX_MODEL_APIS"] {
            std::env::remove_var(key);
        }
        // A configured provider with its own endpoint, key and wire pin.
        let resolved = super::extension_provider_auth("anthropic").unwrap();
        assert_eq!(resolved.auth.api_key, "k-ant");
        assert_eq!(resolved.auth.base_url, "https://anthropic.example");
        assert_eq!(resolved.api, "anthropic-messages");
        // Unknown provider: the error names the deposit place.
        let unknown = match super::extension_provider_auth("ghostprov") {
            Err(e) => e,
            Ok(_) => panic!("ghostprov has no deposits and must not resolve"),
        };
        assert!(
            unknown.contains("add it under 'providers:'"),
            "got: {unknown}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_configured_providers_lists_resolvable_endpoints() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&[
            "DEX_CONFIG",
            "DEX_MODEL",
            "DEX_PROVIDER",
            "DEX_MODEL_APIS",
            "XDG_CACHE_HOME",
        ]);
        let dir = std::env::temp_dir().join(format!("dex-provlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.yaml"),
            "model: myprov/m-7\nproviders:\n  myprov:\n    base_url: https://myprov.example/v1\n    api_key: k-123\n  nokey:\n    base_url: https://nokey.example\n",
        )
        .unwrap();
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        for key in ["DEX_MODEL", "DEX_PROVIDER", "DEX_MODEL_APIS"] {
            std::env::remove_var(key);
        }
        let providers: Vec<String> = super::extension_configured_providers()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        // `myprov` has key + endpoint; `nokey` has no key deposit and is
        // skipped — the list only names providers that could actually
        // authenticate. (Builtins appear when this machine holds their
        // credentials, so the assertion is membership, not equality.)
        assert!(
            providers.contains(&"myprov".to_string()),
            "got: {providers:?}"
        );
        assert!(
            !providers.contains(&"nokey".to_string()),
            "got: {providers:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
