use super::catalog_query::endpoints_for;
use super::catalog_query::landing_base_url_for;
use super::config_file_path;
use super::headers::config_headers_map;
use super::load_config_str;
use super::warn_once;
use crate::core::types::ApiProtocol;
use crate::core::types::Provider;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::OnceLock;

use std::env;

/// Selection pointer fallback: `active_provider:` is deprecated — the
/// provider now rides inside `model:` as `provider/model` (a legacy
/// `provider:` key is honored with the same warning).
pub(crate) fn load_provider_name(file: &Option<serde_yaml::Value>) -> Option<String> {
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

pub(crate) fn load_provider_entries(
    file: &Option<serde_yaml::Value>,
) -> BTreeMap<String, ProviderEntry> {
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
pub(crate) fn known_providers(entries: &BTreeMap<String, ProviderEntry>) -> BTreeSet<String> {
    entries.keys().cloned().collect()
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
pub(crate) fn resolve_provider(
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
pub(crate) fn setup_guide_error() -> String {
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

pub(crate) fn cli_overrides() -> (Option<String>, Option<String>, Vec<String>) {
    CLI_OVERRIDES
        .get()
        .map(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .unwrap_or_default()
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
