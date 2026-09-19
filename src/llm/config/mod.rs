use crate::protocol::ApiProtocol;
use crate::protocol::PermissionMode;
use crate::protocol::Provider;
pub(crate) use crate::workspace::{cached_parse, xdg_path, FileCache};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

mod catalog_index;
mod catalog_query;
mod cost;
mod ctx_index;
mod doctor;
mod extension_model;
mod headers;
mod permission;
mod prompt_source;
mod provider;
mod routing;
mod selection;
/// Builtin providers whose canonical key env var is pinned in dex rather
/// than catalog-discovered — see `llm::auth` (single owner of key
/// resolution); re-exported so existing `config::...` paths keep working.
pub(crate) use super::auth::{pinned_key_env, resolve_credentials};
/// Per-model reasoning effort — see `llm::thinking` (single owner of
/// `thinking-effort.json`); re-exported so existing `config::...` paths
/// keep working.
pub(crate) use super::thinking::{remember_thinking_effort, stored_thinking_effort};
#[cfg(test)]
pub(crate) use crate::workspace::unique_tmp_path;
pub(crate) use catalog_query::{
    catalog_env_vars, load_dex_models_cache, reasoning_options_for, refresh_models_cache,
    refresh_models_cache_async, validate_thinking_effort, warn_provider_like_selection,
};
#[cfg(test)]
pub(crate) use cost::resolve_model_cost;
pub(crate) use cost::usage_cost;
#[cfg(test)]
pub(crate) use ctx_index::{build_ctx_map, write_ctx_index};
pub(crate) use ctx_index::{
    catalog_cache_missing, catalog_context_window, catalog_endpoint_for_model,
    catalog_output_limit_for, ctx_from_index, ensure_ctx_index, load_dex_catalog,
};
pub(crate) use doctor::doctor;
pub(crate) use extension_model::{
    extension_configured_providers, extension_model_api, extension_model_auth,
    extension_model_auth_for, extension_model_snapshot, extension_provider_auth,
    ExtensionModelSnapshot,
};
pub(crate) use headers::{
    apply_opencode_session_headers, custom_headers_from_env, insert_extra_header,
    insert_parsed_headers, load_config_headers, merge_header_layers, parse_headers_str,
};
pub(crate) use permission::permission_from_env;
pub(crate) use prompt_source::{resolve_cli_system_prompt, system_prompt_origin};
pub(crate) use provider::{
    known_providers, load_provider_entries, load_provider_name, model_api_from_env,
    resolve_provider, set_cli_model_overrides, setup_guide_error, ProviderEntry, ResolvedProvider,
};
pub(crate) use routing::route_turn;
#[cfg(test)]
pub(crate) use routing::routing_resolution;
pub(crate) use selection::{
    classify_selection, env_parse, persist_selection, provider_fallback_with_origin,
    provider_without_prefix, resolve_selection, split_selection, Resolved, SelectionRoute,
};

/// Config file location: `$DEX_CONFIG` > `$XDG_CONFIG_HOME/dex/config.yaml`
/// > `~/.config/dex/config.yaml`.
pub(crate) fn config_file_path() -> Option<std::path::PathBuf> {
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

pub(crate) fn load_config_file() -> Option<serde_yaml::Value> {
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

pub(crate) fn invalidate_config_cache() {
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
pub(crate) fn context_window_from_env() -> Option<u64> {
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
pub(crate) fn load_config_num(file: &Option<serde_yaml::Value>, key: &str) -> Option<u64> {
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

pub(crate) fn load_config_str(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
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
    /// Timeout knobs, not a client: the wire goes through
    /// [`LlmConfig::http_client`], which shares the process-wide streaming
    /// client on defaults and builds a bounded one only when
    /// `DEX_HTTP_*_TIMEOUT_SECS` overrides say otherwise. Carrying a
    /// `reqwest::Client` here duplicated the shared pool per config.
    pub(crate) connect_timeout_secs: u64,
    pub(crate) request_timeout_secs: u64,
}

/// Base wire protocol + baked pin from the provider entry's `api:` pin and
/// the global file `api:`. Single derivation shared by `from_env` and every
/// provider switch; per-model overrides (`DEX_MODEL_APIS`, learned) apply
/// on top via `resolve_model_api`. Native providers carry a built-in
/// default (`anthropic` → anthropic-messages); OpenAI-compatible ones
/// fall back to the responses default with the empirical completions
/// fallback.
pub(crate) fn base_protocol(
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
impl LlmConfig {
    /// HTTP client for provider generations: the process-wide streaming
    /// client (connect timeout only — a total timeout would kill long
    /// generations) unless explicit `DEX_HTTP_*_TIMEOUT_SECS` overrides
    /// demand a bounded backstop client. Timeouts ride the config so the
    /// shared pool isn't duplicated per turn.
    pub(crate) fn http_client(&self) -> reqwest::Client {
        if self.connect_timeout_secs == 10 && self.request_timeout_secs == 300 {
            crate::client::http::shared_streaming_client()
        } else {
            reqwest::Client::builder()
                .user_agent(crate::client::http::USER_AGENT)
                .connect_timeout(Duration::from_secs(self.connect_timeout_secs))
                .timeout(Duration::from_secs(self.request_timeout_secs))
                // Same dead-socket detection as the shared streaming client.
                .tcp_keepalive(Duration::from_secs(crate::client::http::TCP_KEEPALIVE_SECS))
                .build()
                .unwrap_or_else(|_| crate::client::http::shared_streaming_client())
        }
    }

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
        // Custom headers, three layers merged by `merge_header_layers`
        // (AGENTS.md precedence): global file table
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
        let connect_timeout_secs: u64 = env_parse("DEX_HTTP_CONNECT_TIMEOUT_SECS", 10);
        let request_timeout_secs: u64 = env_parse("DEX_HTTP_REQUEST_TIMEOUT_SECS", 300);
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
            connect_timeout_secs,
            request_timeout_secs,
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
        crate::llm::learned::lookup(&self.base_url, &self.model)
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
#[cfg(test)]
pub(crate) mod tests;
