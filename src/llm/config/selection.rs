use super::config_file_path;
use super::invalidate_config_cache;
use super::load_config_str;
use super::provider::setup_guide_error;
use crate::protocol::Provider;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;

use std::env;

/// One `DEX_FOO=…` integer knob
/// unset or unparseable. Single-sources the
/// `env::var(..).ok().and_then(|v| v.parse().ok()).unwrap_or(..)` ladder
/// repeated through `from_env`.
pub(crate) fn env_parse<T: FromStr>(name: &str, default: T) -> T {
    env_parse_opt(name).unwrap_or(default)
}

/// Same env read + parse, but the caller keeps the fallback chain — used
/// where the default is reached only after catalog lookups
/// (`DEX_CONTEXT_WINDOW`).
fn env_parse_opt<T: FromStr>(name: &str) -> Option<T> {
    env::var(name).ok().and_then(|v| v.parse().ok())
}

// Filesystem helpers live in `workspace` (one owner for XDG paths, atomic
// tmp files, and content-identity caching); re-exported here so existing
// `config::...` paths (and tests) keep working.
pub(crate) use crate::workspace::unique_tmp_path;

/// One resolved knob: the value that `from_env` applies and the origin
/// that `doctor` reports. Both sides consume the same resolution, so the
/// precedence rule exists once and `doctor` cannot drift from runtime
/// routing.
pub(crate) struct Resolved<T> {
    pub(crate) value: T,
    pub(crate) origin: &'static str,
}

/// The single selection knob — `--model` > `DEX_MODEL` > file `model:`.
/// There is no built-in default model: nothing set anywhere is a setup
/// error (the guide text), so the daemon never silently rides a hardcoded
/// id that a catalog refresh can retire. The flag is pre-filtered by the
/// caller: `from_env` keeps an empty `--model` as a literal selection
/// (historical behavior), `doctor` treats it as unset.
pub(crate) fn resolve_selection(
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

/// Provider for a selection that names none: `custom` when an explicit
/// `--base-url` names an endpoint (the URL, not a provider entry, is what
/// the user configured — it routes onto `providers.custom.*` so key errors
/// name the right deposit). Empty otherwise: there is no default provider —
/// the selection must carry a `provider/` prefix.
pub(crate) fn provider_without_prefix(base_url_override: Option<&str>) -> String {
    if base_url_override.is_some_and(|u| !u.trim().is_empty()) {
        "custom".to_string()
    } else {
        String::new()
    }
}

/// How a selection string routes: which known-provider prefix it carries
/// and what remains. `split_selection`, `strip_routing_prefixes`, and
/// `apply_model` classify the same `provider/` grammar with different
/// outcomes; this enum is the shared classification. Endpoint prefixes
/// (`go/…`) are deliberately not classified here — endpoints are instance
/// state, so each consumer keeps its own endpoint step.
pub(crate) enum SelectionRoute {
    /// `provider/rest` — a known provider prefix; `rest` may be empty.
    ProviderQualified { provider: String, rest: String },
    /// The whole selection is a bare known provider name (no `/`).
    BareProvider { provider: String },
    /// No known-provider prefix: the whole selection is the model id.
    Model(String),
}

/// Classify one selection string against the known provider names.
pub(crate) fn classify_selection(selection: &str, known: &BTreeSet<String>) -> SelectionRoute {
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

pub(crate) fn split_selection(
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
pub(crate) fn persist_selection(
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
