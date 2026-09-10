use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use unicode_width::UnicodeWidthStr;

use crate::core::types::{ApiProtocol, PermissionMode, Provider};
use crate::llm::auth::load_codex_credentials;
use crate::llm::provider::{DEFAULT_CONTEXT_WINDOW, DEFAULT_MODEL};

/// Config file location: `$DEX_CONFIG` > `$XDG_CONFIG_HOME/dex/config.yaml`
/// > `~/.config/dex/config.yaml`.
fn config_file_path() -> Option<std::path::PathBuf> {
    if let Some(p) = env::var_os("DEX_CONFIG") {
        return Some(std::path::PathBuf::from(p));
    }
    let dir = env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(dir.join("dex/config.yaml"))
}

/// Raw config file as YAML. Parsed as an untyped `Value` so unknown keys
/// survive the `/model` write-back. Missing/invalid file → None (env rules).
/// Cached process-wide and invalidated by file identity (path + mtime +
/// length): `from_env` runs per chat turn on the daemon, and each call was
/// re-reading + re-parsing the file.
struct CachedConfigFile {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    value: Option<serde_yaml::Value>,
}

static CONFIG_CACHE: OnceLock<Mutex<Option<CachedConfigFile>>> = OnceLock::new();

fn load_config_file() -> Option<serde_yaml::Value> {
    let path = config_file_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = CONFIG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return hit.value.clone();
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value = match serde_yaml::from_str::<serde_yaml::Value>(&text) {
        Ok(value) => {
            // Unknown keys are almost always typos; name them instead of
            // letting a misspelled setting silently do nothing.
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
            // A typo'd file must not silently disable every user setting.
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
    };
    CONFIG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedConfigFile {
            path,
            mtime,
            len,
            value: value.clone(),
        });
    value
}

fn invalidate_config_cache() {
    if let Some(cache) = CONFIG_CACHE.get() {
        cache.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
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
    "thinking_effort",
    "mcp_servers",
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
fn warn_once(id: &str, message: &str) {
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

/// Split a model selection into `(provider, model)`. A leading
/// `provider/` prefix — or the whole selection being a bare provider name —
/// selects the provider; the remainder is the model id (a bare provider
/// keeps the builtin default model). The provider is lowercased, matching
/// `Provider::name()`. `None` provider means "selection names
/// no known provider" (an endpoint prefix like `go/…` or a plain model id).
fn split_selection(selection: &str, known: &BTreeSet<String>) -> (Option<String>, String) {
    // A bare provider pick gets that provider's own default model
    // (`anthropic` → a claude model; OpenAI-compatible providers share the
    // builtin default).
    let default_model = |name: &str| {
        Provider::parse_known(name, known)
            .map(|provider| provider.default_model().to_string())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string())
    };
    match selection.split_once('/') {
        Some((prefix, rest)) if Provider::parse_known(prefix, known).is_some() => (
            Some(prefix.trim().to_ascii_lowercase()),
            if rest.is_empty() {
                default_model(prefix)
            } else {
                rest.to_string()
            },
        ),
        _ if Provider::parse_known(selection, known).is_some() => (
            Some(selection.trim().to_ascii_lowercase()),
            default_model(selection),
        ),
        _ => (None, selection.to_string()),
    }
}

/// Provider fallback when the model selection names none: `DEX_PROVIDER`
/// (deprecated) > `active_provider:` (deprecated) > builtin default.
fn env_provider_fallback(file: &Option<serde_yaml::Value>) -> String {
    if let Ok(name) = env::var("DEX_PROVIDER") {
        if !name.trim().is_empty() {
            warn_once(
                "env:DEX_PROVIDER",
                "env var DEX_PROVIDER is deprecated — use DEX_MODEL=<provider>/<model> (e.g. DEX_MODEL=openai-codex)",
            );
            return name;
        }
    }
    load_provider_name(file).unwrap_or_else(|| "opencode".to_string())
}

/// Catalog `api` URL for a provider key ("zai" → its serving endpoint).
/// Entries without one (native-API providers like anthropic) are not usable
/// as generic OpenAI-compatible providers — that absence is the gate.
fn catalog_api(key: &str, catalog: &serde_json::Value) -> Option<String> {
    catalog
        .get(key)
        .and_then(|e| e.get("api"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Whether `model` names a known model id anywhere in the catalog (either
/// shape). Guards the provider-like hint: native `org/model` ids share
/// their prefix with a provider but are legit model ids.
fn catalog_has_model(model: &str, catalog: &serde_json::Value) -> bool {
    let needle = model.to_ascii_lowercase();
    if let Some(providers) = catalog.as_object() {
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                if models.keys().any(|id| id.to_ascii_lowercase() == needle) {
                    return true;
                }
            }
        }
    }
    catalog
        .get("models")
        .and_then(|m| m.as_object())
        .is_some_and(|models| models.keys().any(|id| id.to_ascii_lowercase() == needle))
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
    let Some(catalog) = load_dex_catalog() else {
        return false;
    };
    if catalog_api(candidate, &catalog).is_none() {
        return false;
    }
    if qualified && catalog_has_model(selection, &catalog) {
        return false;
    }
    let key_env = catalog_env_vars(candidate, &catalog)
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
fn catalog_env_vars(key: &str, catalog: &serde_json::Value) -> Vec<String> {
    // api.json shape is a list of names; older catalog.json used an object
    // (name → description). Accept both — only the names matter here.
    let mut out = match catalog.get(key).and_then(|e| e.get("env")) {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    };
    out.sort();
    out.dedup();
    out
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

/// Landing base URL when nothing explicit is set: the builtin default, or
/// for a generic provider its config entry override > catalog `api` URL.
fn landing_base_url_for(
    provider: &Provider,
    entries: &BTreeMap<String, ProviderEntry>,
) -> Option<String> {
    match provider {
        Provider::Generic(name) => entries
            .get(name)
            .and_then(|e| e.base_url.clone())
            .or_else(|| load_dex_catalog().and_then(|c| catalog_api(name, &c))),
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

/// Neutral first-run hint when nothing selects a provider (no `--model`,
/// `DEX_MODEL`, file `model:`, `DEX_PROVIDER`/`active_provider:`) and the
/// builtin default has no key. Names no favorite — the user picks.
fn setup_guide_error() -> String {
    let path = config_file_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/dex/config.yaml".to_string());
    // Model ids ride the providers' own defaults so the catalog can move
    // without this text rotting; the gateway shape mirrors the README.
    let anthropic_model = Provider::Anthropic.default_model();
    let opencode_model = Provider::OpenCode.default_model();
    format!(
        "no provider configured — pick one, then run `dex doctor`:\n\
         \u{20}\u{20}anthropic: model: anthropic/{anthropic_model} + providers.anthropic.api_key (or ANTHROPIC_API_KEY)\n\
         \u{20}\u{20}custom gateway (Bearer + Anthropic wire): model: gateway/<model-id> + providers.gateway: {{base_url: https://gateway.example/v1, api_key, api: anthropic-messages}}\n\
         \u{20}\u{20}opencode: model: zen/{opencode_model} + providers.opencode.api_key (or OPENCODE_API_KEY)\n\
         \u{20}\u{20}codex: model: openai-codex + run `codex --login` (or CODEX_ACCESS_TOKEN)\n\
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
    let mut env_names: Vec<String> = load_dex_catalog()
        .map(|c| catalog_env_vars(name, &c))
        .unwrap_or_default();
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

/// Persisted wire protocols learned empirically at runtime
/// (`XDG_CACHE_HOME/dex/learned-apis.json`): models that rejected
/// `/responses` and succeeded over `/chat/completions`. Keyed
/// `"<base_url>|<model>"`. Only consulted when nothing explicit pins the
/// protocol. ponytail: no expiry — a model that speaks completions keeps
/// working even after the provider adds responses support.
fn learned_apis_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/learned-apis.json"));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/dex/learned-apis.json"))
}

/// Learned wire protocols, cached process-wide and invalidated by file
/// identity. `from_env` consulted this file on every turn (one read + parse
/// per turn); hits are now a mutex bump.
struct CachedLearnedApis {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    map: serde_json::Map<String, serde_json::Value>,
}

static LEARNED_CACHE: OnceLock<Mutex<Option<CachedLearnedApis>>> = OnceLock::new();

fn learned_api_map() -> serde_json::Map<String, serde_json::Value> {
    let Some(path) = learned_apis_path() else {
        return Default::default();
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return Default::default();
    };
    let (Ok(mtime), len) = (meta.modified(), meta.len()) else {
        return Default::default();
    };
    if let Some(hit) = LEARNED_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return hit.map.clone();
    }
    let map: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    LEARNED_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedLearnedApis {
            path,
            mtime,
            len,
            map: map.clone(),
        });
    map
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
    let catalog = load_dex_catalog()?;
    let needle = model.to_ascii_lowercase();
    let entry = catalog.as_object()?.values().find_map(|provider| {
        provider
            .get("models")
            .and_then(|m| m.as_object())
            .and_then(|models| {
                models
                    .iter()
                    .find(|(id, _)| id.to_ascii_lowercase() == needle)
                    .map(|(_, v)| v)
            })
    })?;
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

/// Per-model reasoning effort chosen via `/thinking`
/// (`XDG_CACHE_HOME/dex/thinking-effort.json`): `"<base_url>|<model>"` →
/// effort. Wins over `DEX_THINKING_EFFORT` (a stored choice is more specific
/// than a global). `None` clears the entry.
/// ponytail: read-through, no process cache — the file holds a handful of
/// entries; add file-identity caching like `learned-apis.json` if it grows.
fn thinking_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/thinking-effort.json"));
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".cache/dex/thinking-effort.json"))
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
        let _ = std::fs::write(path, text);
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
        let _ = std::fs::write(path, text);
        invalidate_config_cache();
    }
}

fn dex_catalog_cache_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/models.dev.json"));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/dex/models.dev.json"))
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
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(dir).join("dex/models.ctx.json"));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache/dex/models.ctx.json"))
}

fn ctx_from_index(model: &str) -> Option<u64> {
    // KB-sized file: one small read + parse instead of the 4MB catalog.
    let path = dex_ctx_index_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text).ok()?;
    map.get(model.to_ascii_lowercase().as_str())?
        .as_u64()
        .filter(|ctx| *ctx > 0)
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
                        map.insert(id.to_ascii_lowercase(), ctx);
                    }
                }
            }
        }
    }
    if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
        for (id, m) in models {
            if let Some(ctx) = context_of(m) {
                map.insert(id.to_ascii_lowercase(), ctx);
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
struct CachedCatalog {
    path: std::path::PathBuf,
    mtime: SystemTime,
    len: u64,
    value: std::sync::Arc<serde_json::Value>,
}

static CATALOG_CACHE: OnceLock<Mutex<Option<CachedCatalog>>> = OnceLock::new();

fn load_dex_catalog() -> Option<std::sync::Arc<serde_json::Value>> {
    let path = dex_catalog_cache_path()?;
    let meta = std::fs::metadata(&path).ok()?;
    let (mtime, len) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = CATALOG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.path == path && cached.mtime == mtime && cached.len == len)
    {
        return Some(hit.value.clone());
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let value: std::sync::Arc<serde_json::Value> =
        std::sync::Arc::new(serde_json::from_str(&text).ok()?);
    CATALOG_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedCatalog {
            path,
            mtime,
            len,
            value: value.clone(),
        });
    Some(value)
}

/// models.dev catalog `limit.<key>` for `model` (`context` = window,
/// `output` = generation cap). Matches either cache shape — api.json
/// (per-provider models) or catalog.json (flat models map) — case-
/// insensitively.
fn catalog_limit(model: &str, catalog: &serde_json::Value, key: &str) -> Option<u64> {
    let needle = model.to_ascii_lowercase();
    // catalog is api.json (providers) or catalog.json (models+providers) — try both shapes
    if let Some(providers) = catalog.as_object() {
        let lookup = |models: &serde_json::Map<String, serde_json::Value>| {
            models
                .get(needle.as_str())
                .or_else(|| {
                    // fallback case-insensitive scan
                    models
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == needle)
                        .map(|(_, v)| v)
                })
                .and_then(|m| m.get("limit"))
                .and_then(|l| l.get(key))
                .and_then(|c| c.as_u64())
        };
        // api.json shape: { "opencode": { models: { "id": { limit:{context} } } } }
        for (_prov, entry) in providers {
            if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                if let Some(v) = lookup(models) {
                    return Some(v);
                }
            }
        }
        // catalog.json shape: { models: { "id": { limit } }, providers: { } }
        if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
            if let Some(v) = lookup(models) {
                return Some(v);
            }
        }
    }
    None
}

fn catalog_context_window(model: &str, catalog: &serde_json::Value) -> Option<u64> {
    catalog_limit(model, catalog, "context")
}

/// models.dev `limit.output` for the model — the generation cap wires that
/// must declare one up front (Anthropic `max_tokens`) clamp against. Reads
/// the cached catalog parse, so safe on hot paths.
pub(crate) fn catalog_output_limit_for(model: &str) -> Option<u64> {
    load_dex_catalog().and_then(|c| catalog_limit(model, &c, "output"))
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
    catalog: &serde_json::Value,
    model: &str,
    endpoints: &BTreeMap<String, String>,
    current_base_url: &str,
) -> Option<String> {
    if !endpoints.values().any(|url| url == current_base_url) {
        return None;
    }
    let needle = model.to_ascii_lowercase();
    let serves = |entry: &serde_json::Value| {
        entry
            .get("models")
            .and_then(|m| m.as_object())
            .is_some_and(|models| models.keys().any(|id| id.to_ascii_lowercase() == needle))
    };
    let mut fallback = None;
    // api.json shape nests providers at the top level; catalog.json shape
    // nests them under `providers` (a top-level `models` dict has no
    // `models` child per entry, so it is skipped by `serves`).
    let mut entries: Vec<&serde_json::Value> = Vec::new();
    if let Some(obj) = catalog.as_object() {
        entries.extend(obj.values());
    }
    if let Some(obj) = catalog.get("providers").and_then(|p| p.as_object()) {
        entries.extend(obj.values());
    }
    for entry in entries {
        let url = entry.get("api").and_then(|v| v.as_str()).unwrap_or("");
        if !endpoints.values().any(|known| known == url) || !serves(entry) {
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
}

/// First `cost` object for `needle` among catalog providers accepted by
/// `pick`, in the catalog's provider order.
fn catalog_cost<'a>(
    catalog: &'a serde_json::Value,
    needle: &str,
    pick: impl Fn(&str, &serde_json::Value) -> bool,
) -> Option<&'a serde_json::Value> {
    let providers = catalog.as_object()?;
    for (key, entry) in providers {
        if !pick(key, entry) {
            continue;
        }
        if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
            if let Some(m) = models.get(needle).or_else(|| {
                models
                    .iter()
                    .find(|(k, _)| k.to_ascii_lowercase() == needle)
                    .map(|(_, v)| v)
            }) {
                if let Some(c) = m.get("cost") {
                    return Some(c);
                }
            }
        }
    }
    None
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
    let catalog = load_dex_catalog()?;
    let needle = model.to_ascii_lowercase();
    let keys = provider.catalog_keys();
    let cost_val = catalog_cost(&catalog, &needle, |_, entry| {
        entry.get("api").and_then(|v| v.as_str()) == Some(base_url)
    })
    .or_else(|| catalog_cost(&catalog, &needle, |key, _| keys.iter().any(|k| k == key)))
    .or_else(|| catalog_cost(&catalog, &needle, |_, _| true))?;
    let input_rate = cost_val
        .get("input")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let cache_read_rate = cost_val
        .get("cache_read")
        .or_else(|| cost_val.get("cacheRead"))
        .and_then(|v| v.as_f64())
        .unwrap_or(input_rate);
    let output_rate = cost_val
        .get("output")
        .and_then(|v| v.as_f64())
        .unwrap_or(input_rate);
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
    // the prefixes are exactly the names `apply_model` routes on.
    if let Some(catalog) = load_dex_catalog() {
        if let Some(providers) = catalog.as_object() {
            let mut ids: Vec<String> = Vec::new();
            // Endpoint-qualified prefixes: builtins map to their named
            // endpoints, configured generic providers to their own name.
            let configured: BTreeSet<String> = load_provider_entries(&load_config_file())
                .into_keys()
                .collect();
            for (prov_key, entry) in providers {
                if let Some(models) = entry.get("models").and_then(|m| m.as_object()) {
                    let dex_prefix: Option<String> = match prov_key.as_str() {
                        "opencode" => Some("zen".to_string()),
                        "opencode-go" => Some("go".to_string()),
                        "openai-codex" | "codex" => Some("openai-codex".to_string()),
                        other => configured.contains(other).then(|| other.to_string()),
                    };
                    for id in models.keys() {
                        ids.push(id.clone());
                        if let Some(prefix) = &dex_prefix {
                            if prefix != id.as_str() {
                                ids.push(format!("{prefix}/{id}"));
                            }
                        }
                    }
                }
            }
            if !ids.is_empty() {
                ids.sort();
                ids.dedup();
                return Some(ids);
            }
        }
        if let Some(models) = catalog.get("models").and_then(|m| m.as_object()) {
            let ids: Vec<String> = models.keys().cloned().collect();
            if !ids.is_empty() {
                return Some(ids);
            }
        }
    }
    None
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
/// casing) are left alone, so explicit user headers always win regardless
/// of call order.
pub(crate) fn apply_opencode_session_headers(
    out: &mut BTreeMap<String, String>,
    provider: &Provider,
    base_url: &str,
    session_id: &str,
) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let is_opencode = matches!(provider, Provider::OpenCode)
        || reqwest::Url::parse(base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .is_some_and(|h| h.eq_ignore_ascii_case("opencode.ai"));
    if !is_opencode {
        return;
    }
    for (name, value) in [
        ("x-opencode-session", session_id),
        ("x-opencode-client", "dex"),
    ] {
        if !out.keys().any(|k| k.eq_ignore_ascii_case(name)) {
            out.insert(name.to_string(), value.to_string());
        }
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
fn config_headers_map(file: &Option<serde_yaml::Value>, key: &str) -> BTreeMap<String, String> {
    let Some(value) = file.as_ref().and_then(|f| f.get(key)) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(map) = value.as_mapping() {
        merge_config_headers_map(&mut out, map);
    } else if let Some(text) = value.as_str() {
        for (k, v) in parse_headers_str(text) {
            insert_extra_header(&mut out, &k, &v);
        }
    } else if let Some(items) = value.as_sequence() {
        for item in items {
            if let Some(map) = item.as_mapping() {
                merge_config_headers_map(&mut out, map);
            } else if let Some(text) = item.as_str() {
                for (k, v) in parse_headers_str(text) {
                    insert_extra_header(&mut out, &k, &v);
                }
            }
        }
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
            for (k, v) in parse_headers_str(&raw) {
                insert_extra_header(&mut out, &k, &v);
            }
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
    /// switch. Applied under the global/env/CLI extras, which win.
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
    /// routing, attribution). Config `headers:`/`http_headers:` < env
    /// (`ANTHROPIC_CUSTOM_HEADERS`/`OPENAI_HEADERS`/`DEX_HEADERS`) <
    /// `--header` / per-request overrides. Never carries `authorization`
    /// (the api key owns that) — it is dropped at send time.
    pub(crate) extra_headers: BTreeMap<String, String>,
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

impl LlmConfig {
    pub(crate) fn from_env(
        base_url_override: Option<String>,
        model_override: Option<String>,
        permission_override: Option<PermissionMode>,
        header_overrides: &[String],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        crate::tools::set_output_limit(
            env::var("DEX_TOOL_OUTPUT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1_048_576),
        );
        let permission = match permission_override {
            Some(mode) => mode,
            None => permission_from_env()?,
        };
        let file = load_config_file();
        // Custom provider headers: config file < env < CLI flags.
        let mut extra_headers = load_config_headers(&file);
        for (k, v) in custom_headers_from_env() {
            insert_extra_header(&mut extra_headers, &k, &v);
        }
        for raw in header_overrides {
            for (k, v) in parse_headers_str(raw) {
                insert_extra_header(&mut extra_headers, &k, &v);
            }
        }
        let provider_entries = load_provider_entries(&file);
        let known = known_providers(&provider_entries);
        // Untouched builtin default (no flag/env/file pointer anywhere):
        // a missing key then means "nothing configured", not "opencode
        // is broken" — the error guides setup instead of endorsing one
        // provider.
        let using_builtin_default = model_override
            .as_ref()
            .map(|m| m.trim().is_empty())
            .unwrap_or(true)
            && env::var("DEX_MODEL")
                .ok()
                .filter(|m| !m.trim().is_empty())
                .is_none()
            && load_config_str(&file, "model").is_none()
            && env::var("DEX_PROVIDER")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .is_none()
            && load_provider_name(&file).is_none();
        // One selection knob names provider *and* model: `provider/model`
        // (`endpoint/model` or a bare provider name work too). Precedence:
        // `--model` > `DEX_MODEL` > file `model:` > builtin default. When
        // the selection carries no provider, `DEX_PROVIDER` /
        // `active_provider:` (both deprecated) still pick one.
        let selection = model_override
            .or_else(|| env::var("DEX_MODEL").ok().filter(|m| !m.trim().is_empty()))
            .or_else(|| load_config_str(&file, "model"))
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let (selection_provider, mut model) = split_selection(&selection, &known);
        let provider_name = match selection_provider.as_deref() {
            Some(name) => name.to_string(),
            None => env_provider_fallback(&file),
        };
        let provider = Provider::parse_known(&provider_name, &known).ok_or_else(|| {
            format!(
                "unsupported provider '{provider_name}'; use opencode, openai-codex, anthropic or a providers: entry"
            )
        })?;
        // A provider from the deprecated fallback (`DEX_PROVIDER` /
        // `active_provider:`) with the untouched builtin default model gets
        // one of its own family — `active_provider: anthropic` must not send
        // `gpt-5.6-luna` to the Messages API.
        if selection_provider.is_none() && model == DEFAULT_MODEL {
            model = provider.default_model().to_string();
        }
        let mut available_models = env::var("DEX_MODELS")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
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
        let context_window = env::var("DEX_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse().ok())
            // Slim cross-process index first (KBs); the 4MB catalog parse
            // is the cold-path fallback, which then refreshes the index.
            .or_else(|| ctx_from_index(&model))
            .or_else(|| {
                load_dex_catalog().and_then(|c| {
                    let ctx = catalog_context_window(&model, &c);
                    ensure_ctx_index(&c);
                    ctx
                })
            })
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        // Reserve 16384, keep 20000 tokens recent (not 12 messages)
        let reserve_tokens = env::var("DEX_RESERVE_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(16_384);
        let keep_recent_tokens = env::var("DEX_KEEP_RECENT_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let connect_secs: u64 = env::var("DEX_HTTP_CONNECT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let request_secs: u64 = env::var("DEX_HTTP_REQUEST_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        // Streaming generations must not have a total request timeout:
        // reqwest's `.timeout()` covers the whole SSE body, killing long
        // generations with `error decoding response body`. The default path
        // shares the process-wide streaming client (connect timeout only);
        // an explicit DEX_HTTP_REQUEST_TIMEOUT_SECS still builds a bounded
        // client for those who want a backstop.
        let client = if connect_secs == 10 && request_secs == 300 {
            crate::client::http::shared_streaming_client()
        } else {
            reqwest::Client::builder()
                .user_agent(crate::client::http::USER_AGENT)
                .connect_timeout(Duration::from_secs(connect_secs))
                .timeout(Duration::from_secs(request_secs))
                .build()?
        };
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
            context_window,
            reserve_tokens,
            keep_recent_tokens,
            verify_command: env::var("DEX_VERIFY").ok(),
            permission,
            extra_headers,
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
            this.refresh_thinking_effort();
        }
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
        if let Some((prefix, rest)) = sel.split_once('/') {
            let known = known_providers(&self.provider_entries);
            if Provider::parse_known(prefix, &known).is_some() {
                sel = rest.to_string();
            }
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
        let mut sel = selection;
        let mut keep_model = false;
        let known = known_providers(&self.provider_entries);
        let prefix_provider = match selection.split_once('/') {
            Some((prefix, _)) => Provider::parse_known(prefix, &known),
            None => Provider::parse_known(selection, &known),
        };
        if let Some(new_provider) = prefix_provider {
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
            match selection.split_once('/') {
                Some((_, rest)) if !rest.is_empty() => sel = rest,
                _ => keep_model = true,
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
            warn_provider_like_selection(sel, self.provider.name(), &self.available_models);
            if let Some(catalog) = load_dex_catalog() {
                if let Some(url) = catalog_endpoint_for_model(
                    &catalog,
                    &self.model,
                    &self.endpoints,
                    &self.base_url,
                ) {
                    result = self
                        .endpoints
                        .iter()
                        .find_map(|(name, known)| (*known == url).then(|| name.clone()));
                    self.base_url = url;
                }
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
        // Dex standalone: contextWindow from models.dev catalog > provider default; refresh unless env pinned it.
        if env::var("DEX_CONTEXT_WINDOW").is_err() {
            if let Some(catalog) = load_dex_catalog() {
                if let Some(ctx) = catalog_context_window(&self.model, &catalog) {
                    self.context_window = ctx;
                } else {
                    self.context_window = DEFAULT_CONTEXT_WINDOW;
                }
            } else {
                self.context_window = DEFAULT_CONTEXT_WINDOW;
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

/// `dex doctor`: print the resolved provider/model/endpoint/protocol/key
/// configuration and where each value came from — `git config
/// --show-origin` for the LLM wiring. Read-only, no network, no daemon;
/// answers "why is dex using X?" without archaeology. Takes the same
/// overrides as `from_env` so flags (`--model`, `--base-url`,
/// `--permission`, `--header`) are reflected, not silently dropped.
pub(crate) fn doctor(
    base_url_override: Option<String>,
    model_override: Option<String>,
    permission_override: Option<PermissionMode>,
    header_overrides: &[String],
) -> String {
    fn row(out: &mut String, key: &str, value: &str, source: &str) {
        // Three fixed columns — key (KEY_COLS), value (VALUE_COLS), origin. Widths count
        // display columns (CJK chars render 2 wide), so padded values still
        // line up. A value that overflows its column wraps: the value
        // prints in full on its own line and the origin hangs at the origin
        // column, so long paths never run into the origin text.
        const KEY_COLS: usize = 11;
        const VALUE_COLS: usize = 46;
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
    // `--model` > `DEX_MODEL` > file `model:` > builtin default.
    let flag_model = model_override.clone().filter(|m| !m.trim().is_empty());
    let flag_base_url = base_url_override.clone().filter(|u| !u.trim().is_empty());
    let dex_model = env::var("DEX_MODEL").ok().filter(|m| !m.trim().is_empty());
    let file_model = load_config_str(&file, "model");
    let selection = flag_model
        .clone()
        .or(dex_model.clone())
        .or(file_model.clone())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let selection_source = if flag_model.is_some() {
        "--model"
    } else if dex_model.is_some() {
        "DEX_MODEL"
    } else if file_model.is_some() {
        "config model:"
    } else {
        "built-in default"
    };
    let (selection_provider, pre_model) = split_selection(&selection, &known);
    let file_provider = load_provider_name(&file);
    let env_provider = env::var("DEX_PROVIDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let provider_name = selection_provider
        .clone()
        .or(env_provider.clone())
        .or(file_provider.clone())
        .unwrap_or_else(|| "opencode".to_string());
    let provider_source = match selection_provider {
        Some(_) => format!("{selection_source} prefix"),
        None if env_provider.is_some() => "DEX_PROVIDER (deprecated)".to_string(),
        None if file_provider.is_some() => "config active_provider: (deprecated)".to_string(),
        None => "built-in default".to_string(),
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
    match Provider::parse_known(&provider_name, &known) {
        None => row(
            &mut out,
            "provider",
            &provider_name,
            "UNSUPPORTED — use opencode, openai-codex, anthropic or a providers: entry",
        ),
        Some(provider) => {
            // Values come from the live build when it succeeds; otherwise
            // the derived values still explain the setup (e.g. missing key).
            let live = cfg_result.as_ref().ok().filter(|c| c.provider == provider);
            row(&mut out, "provider", provider.name(), &provider_source);
            let model = live.map(|c| c.model.clone()).unwrap_or(pre_model.clone());
            row(&mut out, "model", &model, selection_source);

            let resolved = resolve_provider(&provider, &provider_entries);
            let entry = provider_entries.get(provider.name());
            let file_base_url = load_config_str(&file, "base_url");
            let derived_base = flag_base_url
                .clone()
                .or(file_base_url.clone())
                .or(resolved.landing.clone())
                .unwrap_or_default();
            let base_url = live.map(|c| c.base_url.clone()).unwrap_or(derived_base);
            let base_source = if flag_base_url.is_some() {
                "--base-url (pins endpoint)".to_string()
            } else if file_base_url.is_some() {
                "config base_url: (deprecated)".to_string()
            } else if live.is_some() && resolved.landing.as_deref() != Some(base_url.as_str()) {
                match live.and_then(|c| {
                    c.endpoints
                        .iter()
                        .find(|(_, url)| *url == &base_url)
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
            };
            row(&mut out, "base_url", &base_url, &base_source);

            // Credentials — name the deposit place, never the value.
            let key_source = if matches!(provider, Provider::OpenAiCodex) {
                if env::var_os("CODEX_ACCESS_TOKEN").is_some() {
                    "CODEX_ACCESS_TOKEN".to_string()
                } else {
                    let home = env::var_os("CODEX_HOME")
                        .map(std::path::PathBuf::from)
                        .or_else(|| {
                            env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".codex"))
                        })
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
                let mut names = load_dex_catalog()
                    .map(|c| catalog_env_vars(provider.name(), &c))
                    .unwrap_or_default();
                // Mirror `resolve_credentials`: pinned builtin vars resolve
                // cache-less, ahead of any catalog `env` discovery.
                if let Some(pinned) = pinned_key_env(&provider) {
                    if !names.iter().any(|v| v == pinned) {
                        names.insert(0, pinned.to_string());
                    }
                }
                match names
                    .iter()
                    .find(|n| env::var(n).map(|v| !v.trim().is_empty()).unwrap_or(false))
                {
                    Some(name) => format!("{name} (environment)"),
                    None => "MISSING — set providers.<name>.api_key or the provider's env var"
                        .to_string(),
                }
            };
            row(&mut out, "api key", "(hidden)", &key_source);

            // Wire protocol and its origin. Lookup keys mirror `from_env`:
            // the post-split remainder first, then the stripped model.
            let model_api = model_api_from_env(&pre_model, &model);
            let (chain_api, api_source) = if let Some(api) = entry.and_then(|e| e.api) {
                (api.name().to_string(), "config providers.<name>.api")
            } else if let Some(name) = load_config_str(&file, "api") {
                (
                    ApiProtocol::parse(&name)
                        .map(|a| a.name().to_string())
                        .unwrap_or_else(|| format!("INVALID '{name}'")),
                    "config api: (deprecated)",
                )
            } else if let Some(api) = model_api {
                (api.name().to_string(), "DEX_MODEL_APIS")
            } else if let Some(api) = learned_api(&base_url, &model) {
                (api.name().to_string(), "learned (learned-apis.json)")
            } else if let Some(api) = provider.default_api() {
                (api.name().to_string(), "built-in provider default")
            } else {
                (
                    "openai-responses".to_string(),
                    "default (auto-fallback to completions)",
                )
            };
            let api = live.map(|c| c.api.name().to_string()).unwrap_or(chain_api);
            row(&mut out, "protocol", &api, api_source);
            let (chain_ctx, ctx_source) = if let Ok(v) = env::var("DEX_CONTEXT_WINDOW") {
                (v, "DEX_CONTEXT_WINDOW".to_string())
            } else if let Some(ctx) = ctx_from_index(&model) {
                (ctx.to_string(), "cached context index".to_string())
            } else {
                match load_dex_catalog().and_then(|c| catalog_context_window(&model, &c)) {
                    Some(ctx) => (ctx.to_string(), "models.dev catalog".to_string()),
                    None => (
                        DEFAULT_CONTEXT_WINDOW.to_string(),
                        "built-in default".to_string(),
                    ),
                }
            };
            let ctx = live
                .map(|c| c.context_window.to_string())
                .unwrap_or(chain_ctx);
            row(&mut out, "context", &format!("{ctx} tokens"), &ctx_source);

            let (chain_effort, effort_source) =
                if let Some(e) = stored_thinking_effort(&base_url, &model) {
                    (e, "stored /thinking choice".to_string())
                } else if let Some(e) = env::var("DEX_THINKING_EFFORT")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                {
                    (e, "DEX_THINKING_EFFORT".to_string())
                } else if let Some(e) = load_config_str(&file, "thinking_effort") {
                    (e, "config thinking_effort:".to_string())
                } else {
                    ("(unset)".to_string(), "model default".to_string())
                };
            let effort = live
                .and_then(|c| c.thinking_effort.clone())
                .unwrap_or(chain_effort);
            row(&mut out, "thinking", &effort, &effort_source);

            let (perm, perm_source) = match live.map(|c| c.permission) {
                Some(mode) => {
                    let source = if permission_override.is_some() {
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
            row(&mut out, "permission", &perm, &perm_source);

            // Headers: count per layer, sources joined.
            let mut header_count = 0;
            let mut header_sources: Vec<&str> = Vec::new();
            let http_headers = config_headers_map(&file, "http_headers");
            let file_headers = config_headers_map(&file, "headers");
            let entry_headers = entry.map(|e| e.headers.clone()).unwrap_or_default();
            let env_headers = custom_headers_from_env();
            let mut cli_headers = BTreeMap::new();
            for raw in header_overrides {
                for (k, v) in parse_headers_str(raw) {
                    insert_extra_header(&mut cli_headers, &k, &v);
                }
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
                &mut out,
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
                    &mut out,
                    "endpoints",
                    &names.join(", "),
                    "available to /model routing",
                );
            }
            if !provider_entries.is_empty() {
                let names: Vec<&str> = provider_entries.keys().map(String::as_str).collect();
                row(
                    &mut out,
                    "providers",
                    &names.join(", "),
                    "configured in providers:",
                );
            }
        }
    }
    out.push('\n');
    match &cfg_result {
        Ok(_) => row(&mut out, "resolve", "OK", "config builds cleanly"),
        Err(e) => row(&mut out, "resolve", "ERROR", &e.to_string()),
    }
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
        ]);
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::remove_var("DEX_MODEL");
        std::env::set_var("OPENCODE_API_KEY", "test-key");
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
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key2");
        std::env::remove_var("DEX_MODELS");
        std::env::set_var("DEX_PROVIDER", "opencode");
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
            "active_provider: opencode\nmodel: m-h\nhttp_headers:\n  X-File: file\n  X-Shared: http\nheaders:\n  X-Shared: file\n",
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
        assert_eq!(
            cfg.extra_headers.get("X-File").map(String::as_str),
            Some("file")
        );
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
        // CLI wins over both config-file spellings.
        assert_eq!(
            cfg.extra_headers.get("X-Shared").map(String::as_str),
            Some("cli")
        );
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
        use std::collections::BTreeMap;
        // Opencode provider (zen or go endpoint) + session id → both headers.
        let mut out = BTreeMap::new();
        apply_opencode_session_headers(
            &mut out,
            &Provider::OpenCode,
            "https://opencode.ai/zen/go/v1",
            "sess-1",
        );
        assert_eq!(
            out.get("x-opencode-session").map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(
            out.get("x-opencode-client").map(String::as_str),
            Some("dex")
        );
        // Generic provider on another host → nothing.
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        apply_opencode_session_headers(
            &mut out,
            &Provider::Generic("other".to_string()),
            "https://other.example/v1",
            "sess-1",
        );
        assert!(out.is_empty());
        // Generic provider pointed at opencode.ai → headers (host fallback).
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        apply_opencode_session_headers(
            &mut out,
            &Provider::Generic("proxy".to_string()),
            "https://opencode.ai/zen/v1",
            "sess-1",
        );
        assert_eq!(out.len(), 2);
        // Empty session id → nothing, even for opencode.
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        apply_opencode_session_headers(
            &mut out,
            &Provider::OpenCode,
            "https://opencode.ai/zen/v1",
            "  ",
        );
        assert!(out.is_empty());
        // Explicit user header wins (any casing); client header still fills.
        let mut out: BTreeMap<String, String> =
            BTreeMap::from([("X-Opencode-Session".to_string(), "mine".to_string())]);
        apply_opencode_session_headers(
            &mut out,
            &Provider::OpenCode,
            "https://opencode.ai/zen/v1",
            "sess-1",
        );
        assert_eq!(
            out.get("X-Opencode-Session").map(String::as_str),
            Some("mine")
        );
        assert_eq!(
            out.get("x-opencode-client").map(String::as_str),
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
        assert!(err.contains("no provider configured"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("gateway"), "{err}");
        assert!(err.contains("openai-codex"), "{err}");
        assert!(err.contains(crate::llm::provider::DEFAULT_MODEL), "{err}");
        assert!(!err.contains("no API key for provider"), "{err}");
        // The same guide surfaces in `dex doctor`'s resolve row.
        let report = doctor(None, None, None, &[]);
        assert!(report.contains("no provider configured"), "{report}");
        let _ = std::fs::remove_dir_all(&dir);
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
            "active_provider: opencode\nproviders:\n  opencode:\n    api_key: deposited\n",
        )
        .unwrap();
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "deposited"
        );
        // Missing everywhere: the error points at the canonical names.
        std::fs::write(dir.join("config.yaml"), "active_provider: opencode\n").unwrap();
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
        std::fs::write(dir.join("config.yaml"), "active_provider: anthropic\n").unwrap();
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
            "active_provider: anthropic\nproviders:\n  anthropic:\n    api_key: deposited\n",
        )
        .unwrap();
        assert_eq!(
            LlmConfig::from_env(None, None, None, &[]).unwrap().api_key,
            "deposited"
        );
        // The primary knob (`model: anthropic`) picks the family default too.
        std::fs::write(dir.join("config.yaml"), "model: anthropic\n").unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert_eq!(cfg.model, "claude-sonnet-4-5");
        assert_eq!(cfg.api, ApiProtocol::Anthropic);
        // Missing everywhere: the error points at the canonical names.
        std::fs::write(dir.join("config.yaml"), "active_provider: anthropic\n").unwrap();
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
            "active_provider: opencode\nbase_url: https://file.example/v1\nmodel: file-model\napi: openai-completions\ncustom_key: keep-me\n",
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
        std::fs::write(dir.join("config.yaml"), "active_provider: opencode\n").unwrap();
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
            format!("active_provider: zai\n{providers_yaml}"),
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
            "active_provider: zai\nproviders:\n  zai: {}\n",
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
            "active_provider: opencode\nmodel: go/m-z9\n",
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
        std::fs::write(dir.join("config.yaml"), "active_provider: opencode\n").unwrap();
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
            "active_provider: opencode\nbase_url: https://opencode.ai/zen/go/v1\nmodel: m-go\n",
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
            "active_provider: zai\nproviders:\n  zai: {}\n",
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
            "provider: opencode\nproviders:\n  opencode:\n    api_key: deposited\n",
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
        std::fs::write(dir.join("config.yaml"), "active_provider: opencode\n").unwrap();
        assert!(
            !LlmConfig::from_env(None, None, None, &[])
                .unwrap()
                .api_pinned
        );
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\napi: openai-completions\n",
        )
        .unwrap();
        assert!(
            LlmConfig::from_env(None, None, None, &[])
                .unwrap()
                .api_pinned
        );
        std::fs::write(
            dir.join("config.yaml"),
            "active_provider: opencode\nproviders:\n  opencode:\n    api: openai-completions\n",
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
            "active_provider: zai\nproviders:\n  zai:\n    api_key: zsk-deposit\n    base_url: https://custom.zai.example/v1\n    api: openai-completions\n    headers:\n      X-Prov: prov\n",
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
            "active_provider: opencode\nthinking_effort: low\n",
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
            "providers:\n  zai:\n    api_key: zk\n    base_url: https://zai.example/v1\nmodel: zai/glm-5.3\n",
        )
        .unwrap();
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
        assert_eq!(cfg.model, "glm-5.3");
        assert_eq!(cfg.base_url, "https://zai.example/v1");
        assert_eq!(cfg.api_key, "zk");

        // `DEX_MODEL` beats the file without touching it.
        std::env::set_var("DEX_MODEL", "zai/kimi-k2");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
        assert_eq!(cfg.model, "kimi-k2");

        // A bare provider name selects the provider with the default model.
        std::env::set_var("DEX_MODEL", "zai");
        let cfg = LlmConfig::from_env(None, None, None, &[]).unwrap();
        assert!(matches!(cfg.provider, Provider::Generic(ref n) if n == "zai"));
        assert_eq!(cfg.model, "gpt-5.6-luna");
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
        ]);
        std::env::set_var("OPENCODE_API_KEY", "test-key");
        // Point at a missing file so the host config can't color the output.
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-missing-doctor-{}", std::process::id())),
        );
        let out = doctor(None, None, None, &[]);
        assert!(out.contains("provider"), "{out}");
        assert!(out.contains("model"), "{out}");
        assert!(out.contains("OPENCODE_API_KEY"), "{out}");
        assert!(out.contains("built-in default"), "{out}");
        assert!(out.contains("resolve"), "{out}");
    }

    /// Rows whose value overflows the value column wrap instead of
    /// colliding with the origin text; the origin hangs at the origin
    /// column (display columns 11+46 = 57).
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
        let out = doctor(None, None, None, &[]);
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
                Some(11 + 46),
                "origin hangs at the origin column: {origin:?}"
            );
            return;
        }
        panic!("no config row in:\n{out}");
    }

    /// Padding counts display columns: a CJK path is 31 chars but only 45
    /// columns wide, so the origin still lands on column 57.
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
        let out = doctor(None, None, None, &[]);
        let line = out
            .lines()
            .find(|l| l.starts_with("config "))
            .expect("config row");
        let at = line
            .find("missing or invalid")
            .expect("origin inline on the value line");
        assert_eq!(
            UnicodeWidthStr::width(&line[..at]),
            11 + 46,
            "origin starts at display column 57: {line:?}"
        );
    }
}
