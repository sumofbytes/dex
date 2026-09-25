use super::resolve_credentials;

use super::base_protocol;
use super::context_index::catalog_endpoint_for_model;
use super::headers::custom_headers_from_env;
use super::headers::insert_parsed_headers;
use super::headers::load_config_headers;
use super::headers::merge_header_layers;
use super::load_config_file;
use super::load_config_str;
use super::provider::cli_overrides;
use super::provider::known_providers;
use super::provider::load_provider_entries;
use super::provider::model_api_from_env;
use super::provider::resolve_provider;
use super::provider::unrouted_selection_error;
use super::provider::ProviderEntry;
use super::selection::provider_without_prefix;
use super::selection::resolve_selection;
use super::selection::split_selection;
use crate::protocol::ApiProtocol;
use crate::protocol::Provider;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// The current model as extensions see it: no secrets, safe to clone into
/// Lua tables and the change-detection static. `api` is the wire-protocol
/// name (`ApiProtocol::name`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionModelSnapshot {
    pub provider: String,
    pub model: String,
    pub api: String,
    pub base_url: String,
}

impl ExtensionModelSnapshot {
    /// `provider/model` selection id, as `model_select` payloads carry it.
    pub fn id(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

/// The current model's credentials for `dex.model.auth()`: raw key plus the
/// endpoint and the merged extra headers, so the extension can place auth
/// per provider family (Bearer header, `x-api-key`, `?key=` query). Never
/// logged — the key lives in Lua memory only.
pub struct ExtensionModelAuth {
    pub api_key: String,
    pub base_url: String,
    pub headers: BTreeMap<String, String>,
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
        None => provider_without_prefix(cli_base_url.as_deref()),
    };
    if provider_name.is_empty() {
        // The selection exists but resolved no provider (bare id,
        // unconfigured `prefix/rest`, retired `zen/…`): name it and the
        // fix — "unsupported provider ''" (or "no model configured")
        // would be a lie.
        return Err(unrouted_selection_error(&raw_selection, &known));
    }
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
        .or_else(|| crate::llm::learned::lookup(&base_url, &model))
        .unwrap_or(base_api);
    Ok((provider, entries, base_url, api, model))
}

/// `dex.model.current()`: provider, bare model id, wire protocol, endpoint.
/// Fails only when nothing selects a model or the provider has no endpoint
/// (same guidance as `from_env`, never a silent default).
pub fn extension_model_snapshot() -> Result<ExtensionModelSnapshot, String> {
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
pub fn extension_model_auth() -> Result<ExtensionModelAuth, String> {
    let (provider, _, base_url, _, _) = extension_model_parts()?;
    extension_model_auth_for(provider.name(), &base_url)
}

/// The current model's wire protocol, same resolution as
/// `extension_model_snapshot` — so `dex.model.auth()` can also say which
/// API the key+endpoint speak without a served snapshot.
pub fn extension_model_api() -> Result<String, String> {
    let (_, _, _, api, _) = extension_model_parts()?;
    Ok(api.name().to_string())
}

/// `dex.model.auth(provider)`: credentials for an arbitrary configured
/// provider — the model-independent extension vocabulary (a fallback search
/// calls another provider's endpoint with that provider's own key).
/// Endpoint: the entry's `base_url:` > the catalog landing (builtin) / the
/// catalog `api` URL (generic). Wire: the entry's `api:` pin > provider
/// default. Auth: the standard deposit order.
pub struct ExtensionProviderAuth {
    pub auth: ExtensionModelAuth,
    pub api: String,
}

pub fn extension_provider_auth(provider_name: &str) -> Result<ExtensionProviderAuth, String> {
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
pub struct ConfiguredProviderEndpoint {
    pub provider: String,
    pub base_url: String,
}

/// Every configured provider that could actually authenticate: builtins
/// count when their key deposits resolve, generics with a file entry count
/// when theirs do (file key or the catalog env var). Sorted by provider
/// spelling (alias spellings dedupe to one canonical entry below). Shared
/// by the `net.providers` fetch allowlist and `dex.model.providers()`.
pub fn extension_configured_providers() -> Vec<ConfiguredProviderEndpoint> {
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
pub fn extension_model_auth_for(
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
    // Same merge as the wire path — one function, so the Lua view and the
    // request headers can never drift apart.
    let global = load_config_headers(&file);
    let scoped = entries
        .get(provider.name())
        .map(|entry| entry.headers.clone())
        .unwrap_or_default();
    let mut extra = custom_headers_from_env();
    for raw in &cli_headers {
        insert_parsed_headers(&mut extra, raw);
    }
    let headers = merge_header_layers(&global, &scoped, &extra);
    Ok(ExtensionModelAuth {
        api_key,
        base_url: base_url.to_string(),
        headers,
    })
}
