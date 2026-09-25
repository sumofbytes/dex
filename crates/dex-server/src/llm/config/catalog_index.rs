use super::context_index::dex_catalog_cache_path;
use super::context_index::load_dex_catalog;
use super::cost::cost_rates;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::SystemTime;

pub use crate::workspace::fnv_bytes;

/// Per-catalog-generation lookup index (§24/§25): `from_env` runs per daemon
/// chat turn and every model call re-probes the catalog (idle timeout via
/// `reasoning_options_for`, `usage_cost`), but each
/// probe used to walk all providers × models with a lowercase alloc per id.
/// The index walks once per catalog generation (same file-identity
/// invalidation as the catalog parse itself) and serves every probe from
/// maps. Shape quirks are preserved per lookup via the `endpoint_only` /
/// `from_flat` flags: each reader sees exactly the entries the old walk
/// would have visited.
#[derive(Clone, Default)]
pub struct CostRates {
    pub input: f64,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
}

#[derive(Clone, Default)]
pub struct IndexedModel {
    /// Catalog provider key (`""` for the flat `models` shape, which names none).
    pub provider: String,
    /// The provider entry's `api` URL, if any.
    pub api: Option<String>,
    pub context: Option<u64>,
    pub output: Option<u64>,
    /// `Some` iff the entry carries a `cost` object (even an empty one —
    /// the old walk returned the object and defaulted missing rates).
    pub cost: Option<CostRates>,
    pub reasoning_options: Option<Vec<String>>,
    /// From the `providers`-nested shape (catalog.json): visible only to
    /// endpoint routing, like the old walk which consulted that shape solely
    /// in `catalog_endpoint_for_model`.
    pub endpoint_only: bool,
    /// From the flat top-level `models` shape: visible to context/output/
    /// has-model/bare-id lookups, never to cost tiers (the old tier walk
    /// only matched provider entries).
    pub from_flat: bool,
}

pub struct CatalogIndex {
    pub path: std::path::PathBuf,
    pub mtime: SystemTime,
    pub len: u64,
    /// FNV-1a of the catalog text this index was built from. (mtime, len)
    /// alone can't tell a mtime-preserving copy or a same-content rewrite
    /// from real new data; on a metadata miss this hash decides re-parse
    /// vs. refresh-without-reparse (see `with_catalog_index`).
    pub hash: u64,
    /// Lowercased model id → entries in catalog iteration order.
    pub by_id: HashMap<String, Vec<IndexedModel>>,
    pub provider_api: HashMap<String, String>,
    pub provider_env: HashMap<String, Vec<String>>,
    /// Bare model ids with their provider key (top-level providers plus the
    /// flat shape, whose key is `""`); one id may repeat across providers
    /// (each contributes its own endpoint prefix at expansion time).
    pub bare: Vec<(String, String)>,
    /// Fully expanded `available_models` lists by configured-provider set
    /// (capped): concurrent turns with differing sets each hit instead of
    /// thrashing a single-entry cache.
    pub expanded_for: HashMap<BTreeSet<String>, Vec<String>>,
}

static CATALOG_INDEX: OnceLock<Mutex<Option<CatalogIndex>>> = OnceLock::new();

/// Serve `f` from the in-process catalog index only when it is already warm
/// AND still describes the catalog on disk: a mutex lock + one `stat`, never
/// a file read or rebuild. Used for cross-checks (see `ctx_from_index`) that
/// must not turn the KB-slim fast path into a 4MB parse. The metadata check
/// matters: without it a warm index left over from the previous catalog
/// generation would be served (and could rewrite `models.ctx.json` from
/// stale context windows) after `dex update --models`.
pub fn if_catalog_index_warm<T>(f: impl FnOnce(&CatalogIndex) -> T) -> Option<T> {
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
pub fn with_catalog_index<T>(f: impl FnOnce(&CatalogIndex) -> T) -> Option<T> {
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
pub fn with_catalog_index_mut<T>(f: impl FnOnce(&mut CatalogIndex) -> T) -> Option<T> {
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
