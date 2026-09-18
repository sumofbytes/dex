use super::catalog_index::with_catalog_index;
use super::catalog_index::with_catalog_index_mut;
use super::ctx_index::build_ctx_map;
use super::ctx_index::dex_catalog_cache_path;
use super::ctx_index::load_dex_catalog;
use super::ctx_index::write_ctx_index;
use super::load_config_file;
use super::provider::load_provider_entries;
use super::provider::ProviderEntry;
use super::warn_once;
use crate::core::types::Provider;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

pub(crate) use crate::workspace::unique_tmp_path;

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
pub(crate) fn warn_provider_like_selection(
    selection: &str,
    provider_name: &str,
    served: &[String],
) -> bool {
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
pub(crate) fn catalog_env_vars(key: &str) -> Vec<String> {
    // api.json shape is a list of names; older catalog.json used an object
    // (name → description). Accept both — only the names matter here.
    // Sorted at index time so resolution never depends on JSON key order.
    with_catalog_index(|index| index.provider_env.get(key).cloned().unwrap_or_default())
        .unwrap_or_default()
}

/// Landing base URL when nothing explicit (`--base-url`, top-level
/// `base_url:`) is set: the provider's config entry override, then the
/// builtin landing (native providers), then the catalog `api` URL
/// (generic providers). The config entry beats the built-in landing —
/// config-first, no exceptions.
pub(crate) fn landing_base_url_for(
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
pub(crate) fn endpoints_for(
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

pub(crate) fn load_dex_models_cache() -> Option<Vec<String>> {
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
    // Shared client (pool reuse): the 30s total rides per-request — api.json
    // is a ~4MB body, and the old 10s cap failed on normal slow links while
    // curl (no timeout) succeeded.
    let client = crate::client::http::shared_async_client();
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
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| crate::llm::http::error_chain_message(&e))?
                .error_for_status()
                .map_err(|e| crate::llm::http::error_chain_message(&e))?;
            let text = resp
                .text()
                .await
                .map_err(|e| crate::llm::http::error_chain_message(&e))?;
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
                .map_err(|e| crate::llm::http::error_chain_message(&e))?;
            tokio::fs::rename(&tmp, &path)
                .await
                .map_err(|e| crate::llm::http::error_chain_message(&e))?;
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
