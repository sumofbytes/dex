use super::context_index::dex_catalog_cache_path;
use super::context_index::load_dex_catalog;
use super::cost::cost_rates;
use crate::workspace::unique_tmp_path;
use crate::workspace::xdg_path;
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
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CostRates {
    pub input: f64,
    pub cache_read: Option<f64>,
    pub output: Option<f64>,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
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

/// Cross-process persisted catalog index (`models.idx.json`, next to the
/// catalog): the derived lookup index serialized to disk so a fresh process
/// skips the 4.5MB parse + index build. Keyed by the catalog's content hash
/// — a restored index is exactly as trustworthy as a rebuild, and a stale
/// one fails the hash check and falls through to the parse (then
/// re-persists).
///
/// Wire format is columnar, not `IndexedModel`-shaped: providers, api URLs,
/// cost triples and reasoning-option lists are interned into a string/row
/// table and each per-model entry is a 7-wide int row. The verbose JSON
/// shape (per-entry field names + repeated provider/api/cost strings) was
/// 2MB and cost ~30ms to deserialize — the dominant cold-start cost — while
/// this form is ~5x smaller and parses in low single-digit ms.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedIndex {
    v: u32,
    hash: u64,
    /// The catalog's identity at persist time: a cold process whose catalog
    /// `stat` still matches can restore without reading + hashing the 4.5MB
    /// file at all.
    cat_mtime: Option<(u64, u32)>,
    cat_len: u64,
    /// Interned strings: model ids, provider keys, api URLs, reasoning
    /// options. Everything else references this by index.
    strs: Vec<String>,
    /// Deduped cost triples.
    costs: Vec<PersistedCost>,
    /// Deduped reasoning-option lists (indices into `strs`).
    ros: Vec<Vec<u32>>,
    /// Lowercased model id → compact entry rows (catalog iteration order).
    by_id: Vec<(String, Vec<[i64; 7]>)>,
    provider_api: Vec<[u32; 2]>,
    provider_env: Vec<(u32, Vec<u32>)>,
    bare: Vec<[u32; 2]>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedCost {
    i: f64,
    cr: Option<f64>,
    o: Option<f64>,
}

/// Row layout for one `IndexedModel`: `[provider, api, context, output,
/// cost, reasoning_options, flags]`. `-1` = none for indices, options and
/// limits; flags bit0 = `endpoint_only`, bit1 = `from_flat`.
const IDX_ROW_PROVIDER: usize = 0;
const IDX_ROW_API: usize = 1;
const IDX_ROW_CONTEXT: usize = 2;
const IDX_ROW_OUTPUT: usize = 3;
const IDX_ROW_COST: usize = 4;
const IDX_ROW_REASONING: usize = 5;
const IDX_ROW_FLAGS: usize = 6;
const IDX_NONE: i64 = -1;
const IDX_FLAG_ENDPOINT_ONLY: i64 = 1;
const IDX_FLAG_FROM_FLAT: i64 = 2;
const PERSISTED_INDEX_VERSION: u32 = 3;

/// Intern `s` into the persisted string table, returning its stable index.
fn intern_str(s: &str, strs: &mut Vec<String>, ids: &mut HashMap<String, u32>) -> u32 {
    if let Some(id) = ids.get(s) {
        return *id;
    }
    let id = strs.len() as u32;
    ids.insert(s.to_string(), id);
    strs.push(s.to_string());
    id
}

pub fn persisted_index_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/models.idx.json")
}

/// Serialized persisted-index payload for one catalog generation, carrying
/// the catalog's identity (`cat_mtime`/`cat_len`) so a cold process can
/// restore from a bare `stat` — the `hash` covers the case where the
/// metadata changed but the content didn't. The runtime `path` stays
/// caller-supplied: a restore adopts whatever the catalog's `stat` says now.
pub fn persisted_index_text(
    hash: u64,
    catalog: &serde_json::Value,
    cat_mtime: Option<SystemTime>,
    cat_len: u64,
) -> Option<String> {
    let built = build_catalog_index(
        std::path::PathBuf::new(),
        SystemTime::UNIX_EPOCH,
        0,
        hash,
        catalog,
    );
    let mut strs: Vec<String> = Vec::new();
    let mut str_ids: HashMap<String, u32> = HashMap::new();
    let mut costs: Vec<PersistedCost> = Vec::new();
    let mut cost_ids: HashMap<(u64, Option<u64>, Option<u64>), u32> = HashMap::new();
    let mut ros: Vec<Vec<u32>> = Vec::new();
    let mut ro_ids: HashMap<Vec<u32>, u32> = HashMap::new();

    let mut by_id: Vec<(String, Vec<[i64; 7]>)> = Vec::with_capacity(built.by_id.len());
    for (id, entries) in &built.by_id {
        let mut rows: Vec<[i64; 7]> = Vec::with_capacity(entries.len());
        for e in entries {
            let cost_id = match &e.cost {
                Some(c) => {
                    let key = (
                        c.input.to_bits(),
                        c.cache_read.map(f64::to_bits),
                        c.output.map(f64::to_bits),
                    );
                    *cost_ids.entry(key).or_insert_with(|| {
                        costs.push(PersistedCost {
                            i: c.input,
                            cr: c.cache_read,
                            o: c.output,
                        });
                        (costs.len() - 1) as u32
                    }) as i64
                }
                None => IDX_NONE,
            };
            let ro_id = match &e.reasoning_options {
                Some(options) => {
                    let values: Vec<u32> = options
                        .iter()
                        .map(|o| intern_str(o, &mut strs, &mut str_ids))
                        .collect();
                    *ro_ids.entry(values).or_insert_with(|| {
                        ros.push(
                            options
                                .iter()
                                .map(|o| intern_str(o, &mut strs, &mut str_ids))
                                .collect(),
                        );
                        (ros.len() - 1) as u32
                    }) as i64
                }
                None => IDX_NONE,
            };
            rows.push([
                intern_str(&e.provider, &mut strs, &mut str_ids) as i64,
                e.api
                    .as_deref()
                    .map(|api| intern_str(api, &mut strs, &mut str_ids) as i64)
                    .unwrap_or(IDX_NONE),
                e.context.map(|c| c as i64).unwrap_or(IDX_NONE),
                e.output.map(|c| c as i64).unwrap_or(IDX_NONE),
                cost_id,
                ro_id,
                (e.endpoint_only as i64) | ((e.from_flat as i64) << 1),
            ]);
        }
        by_id.push((id.clone(), rows));
    }
    let mut provider_api: Vec<[u32; 2]> = Vec::with_capacity(built.provider_api.len());
    let mut provider_env: Vec<(u32, Vec<u32>)> = Vec::with_capacity(built.provider_env.len());
    // Sorted for a deterministic file (the maps above iterate unordered).
    for (prov, api) in built.provider_api.iter().collect::<BTreeSet<_>>() {
        provider_api.push([
            intern_str(prov, &mut strs, &mut str_ids),
            intern_str(api, &mut strs, &mut str_ids),
        ]);
    }
    for (prov, envs) in built.provider_env.iter().collect::<BTreeSet<_>>() {
        provider_env.push((
            intern_str(prov, &mut strs, &mut str_ids),
            envs.iter()
                .map(|env| intern_str(env, &mut strs, &mut str_ids))
                .collect(),
        ));
    }
    let bare: Vec<[u32; 2]> = built
        .bare
        .iter()
        .map(|(id, prov)| {
            [
                intern_str(id, &mut strs, &mut str_ids),
                intern_str(prov, &mut strs, &mut str_ids),
            ]
        })
        .collect();
    let persisted = PersistedIndex {
        v: PERSISTED_INDEX_VERSION,
        hash,
        cat_mtime: cat_mtime.and_then(system_time_pair),
        cat_len,
        strs,
        costs,
        ros,
        by_id,
        provider_api,
        provider_env,
        bare,
    };
    serde_json::to_string(&persisted).ok()
}

/// Resolve a persisted index for catalog content `hash` back into runtime
/// shape. `None` on any parse failure, unknown version or out-of-range
/// reference — the caller falls through to the full catalog parse.
struct ExpandedIndex {
    hash: u64,
    by_id: HashMap<String, Vec<IndexedModel>>,
    provider_api: HashMap<String, String>,
    provider_env: HashMap<String, Vec<String>>,
    bare: Vec<(String, String)>,
}

impl PersistedIndex {
    fn expand(self) -> Option<ExpandedIndex> {
        if self.v != PERSISTED_INDEX_VERSION {
            return None;
        }
        let st = |i: i64| -> Option<String> { self.strs.get(usize::try_from(i).ok()?).cloned() };
        let mut by_id: HashMap<String, Vec<IndexedModel>> =
            HashMap::with_capacity(self.by_id.len());
        for (id, rows) in self.by_id {
            let mut entries: Vec<IndexedModel> = Vec::with_capacity(rows.len());
            for row in rows {
                let flags = row[IDX_ROW_FLAGS];
                entries.push(IndexedModel {
                    provider: st(row[IDX_ROW_PROVIDER])?,
                    api: match row[IDX_ROW_API] {
                        IDX_NONE => None,
                        i => Some(st(i)?),
                    },
                    context: match row[IDX_ROW_CONTEXT] {
                        IDX_NONE => None,
                        i => Some(u64::try_from(i).ok()?),
                    },
                    output: match row[IDX_ROW_OUTPUT] {
                        IDX_NONE => None,
                        i => Some(u64::try_from(i).ok()?),
                    },
                    cost: match row[IDX_ROW_COST] {
                        IDX_NONE => None,
                        i => self.costs.get(usize::try_from(i).ok()?).map(|c| CostRates {
                            input: c.i,
                            cache_read: c.cr,
                            output: c.o,
                        }),
                    },
                    reasoning_options: match row[IDX_ROW_REASONING] {
                        IDX_NONE => None,
                        i => {
                            let options = self.ros.get(usize::try_from(i).ok()?)?;
                            let mut values = Vec::with_capacity(options.len());
                            for o in options {
                                values.push(st(i64::from(*o))?);
                            }
                            Some(values)
                        }
                    },
                    endpoint_only: flags & IDX_FLAG_ENDPOINT_ONLY != 0,
                    from_flat: flags & IDX_FLAG_FROM_FLAT != 0,
                });
            }
            by_id.insert(id, entries);
        }
        let mut provider_api: HashMap<String, String> =
            HashMap::with_capacity(self.provider_api.len());
        for row in self.provider_api {
            provider_api.insert(st(i64::from(row[0]))?, st(i64::from(row[1]))?);
        }
        let mut provider_env: HashMap<String, Vec<String>> =
            HashMap::with_capacity(self.provider_env.len());
        for (prov, envs) in self.provider_env {
            let envs: Option<Vec<String>> = envs.iter().map(|e| st(i64::from(*e))).collect();
            provider_env.insert(st(i64::from(prov))?, envs?);
        }
        let mut bare: Vec<(String, String)> = Vec::with_capacity(self.bare.len());
        for row in self.bare {
            bare.push((st(i64::from(row[0]))?, st(i64::from(row[1]))?));
        }
        Some(ExpandedIndex {
            hash: self.hash,
            by_id,
            provider_api,
            provider_env,
            bare,
        })
    }
}

/// Restore a persisted index for catalog content `hash`. `None` on any
/// parse failure or hash mismatch — the caller falls through to the full
/// catalog parse, which re-persists a current copy in the background.
pub fn restore_persisted_index(
    path: &std::path::Path,
    hash: u64,
    mtime: SystemTime,
    len: u64,
) -> Option<CatalogIndex> {
    let text = std::fs::read_to_string(persisted_index_path()?).ok()?;
    restore_persisted_index_text(path, hash, mtime, len, &text)
}

fn restore_persisted_index_text(
    path: &std::path::Path,
    hash: u64,
    mtime: SystemTime,
    len: u64,
    text: &str,
) -> Option<CatalogIndex> {
    let p: PersistedIndex = serde_json::from_str(text).ok()?;
    let expanded = p.expand()?;
    if expanded.hash != hash {
        return None;
    }
    Some(CatalogIndex {
        path: path.to_path_buf(),
        mtime,
        len,
        hash,
        by_id: expanded.by_id,
        provider_api: expanded.provider_api,
        provider_env: expanded.provider_env,
        bare: expanded.bare,
        expanded_for: HashMap::new(),
    })
}

/// `SystemTime` as a JSON-able `(secs, nanos)` pair (sub-second mtime
/// granularity matters: the catalog is rewritten in place by
/// `dex update --models`).
fn system_time_pair(t: SystemTime) -> Option<(u64, u32)> {
    let d = t.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some((d.as_secs(), d.subsec_nanos()))
}

/// Fast-path restore: when the persisted index still names the catalog's
/// current `(mtime, len)`, adopt it without reading + hashing the 4.5MB
/// catalog. `None` on any mismatch or parse failure — the caller falls
/// through to the content-hash path, then to the full parse.
pub(super) fn restore_persisted_index_meta(
    path: &std::path::Path,
    mtime: SystemTime,
    len: u64,
) -> Option<CatalogIndex> {
    let text = std::fs::read_to_string(persisted_index_path()?).ok()?;
    let p: PersistedIndex = serde_json::from_str(&text).ok()?;
    if p.cat_len != len || p.cat_mtime != system_time_pair(mtime) {
        return None;
    }
    let expanded = p.expand()?;
    Some(CatalogIndex {
        path: path.to_path_buf(),
        mtime,
        len,
        hash: expanded.hash,
        by_id: expanded.by_id,
        provider_api: expanded.provider_api,
        provider_env: expanded.provider_env,
        bare: expanded.bare,
        expanded_for: HashMap::new(),
    })
}

/// Serialize + write the persisted index on a background thread — a ~0.6MB
/// serialize must not sit on the cold critical path. Runs at most once per
/// catalog generation (only after a full rebuild; a successful restore
/// means the file is already current). Atomic rename, like the ctx index,
/// so a concurrent writer never leaves a torn file behind.
fn spawn_persist_index(
    hash: u64,
    catalog: std::sync::Arc<serde_json::Value>,
    cat_mtime: Option<SystemTime>,
    cat_len: u64,
) {
    std::thread::spawn(move || {
        let Some(text) = persisted_index_text(hash, &catalog, cat_mtime, cat_len) else {
            return;
        };
        let Some(path) = persisted_index_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = unique_tmp_path(&path);
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    });
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
    // Cold process: if the persisted index still names this catalog's
    // (mtime, len), restore it without reading + hashing the 4MB file.
    if let Some(restored) = restore_persisted_index_meta(&path, mtime, len) {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(restored);
        return Some(f(guard.as_ref()?));
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
    {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = (*guard)
            .as_mut()
            .filter(|index| index.path == path && index.hash == hash)
        {
            // Raced with a concurrent rebuild of the same generation —
            // serve it, just refresh the identity metadata.
            index.mtime = mtime;
            index.len = len;
            return Some(f(index));
        }
        if let Some(restored) = restore_persisted_index(&path, hash, mtime, len) {
            *guard = Some(restored);
            return Some(f(guard.as_ref()?));
        }
    }
    let catalog = load_dex_catalog()?;
    let fresh = build_catalog_index(path.clone(), mtime, len, hash, &catalog);
    spawn_persist_index(hash, std::sync::Arc::clone(&catalog), Some(mtime), len);
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
    // Before reading the 4MB file: a persisted index that still names this
    // catalog's (mtime, len) restores without the read + hash.
    if let Some(restored) = restore_persisted_index_meta(&path, mtime, len) {
        let mut guard = CATALOG_INDEX
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *guard = Some(restored);
        return Some(f(guard.as_mut()?));
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
            index.mtime = mtime;
            index.len = len;
            return Some(f(index));
        }
    }
    // Cold process: try the persisted index before the 4MB parse (same
    // contract as `with_catalog_index` above).
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
        if let Some(restored) = restore_persisted_index(&path, hash, mtime, len) {
            *guard = Some(restored);
            return Some(f(guard.as_mut()?));
        }
    }
    // Parsed outside the lock (like `cached_parse`): the 4MB walk never
    // blocks concurrent readers serving the previous generation.
    let catalog = load_dex_catalog()?;
    let fresh_index = build_catalog_index(path.clone(), mtime, len, hash, &catalog);
    spawn_persist_index(hash, std::sync::Arc::clone(&catalog), Some(mtime), len);
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
