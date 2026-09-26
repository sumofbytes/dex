use super::catalog_index::if_catalog_index_warm;
use super::catalog_index::with_catalog_index;
use super::catalog_index::CatalogIndex;
use super::catalog_index::IndexedModel;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::OnceLock;

pub use crate::workspace::{cached_parse, unique_tmp_path, xdg_path, FileCache};

pub fn dex_catalog_cache_path() -> Option<std::path::PathBuf> {
    xdg_path("XDG_CACHE_HOME", ".cache", "dex/models.dev.json")
}

/// True when no cached models.dev catalog exists yet (fresh install). The
/// daemon fetches it in the background on first start so context windows and
/// `/model` autocomplete work without a manual `dex update --models`.
pub fn catalog_cache_missing() -> bool {
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

/// The parsed `models.ctx.json` map, cached process-wide and invalidated by
/// file identity (path + content hash) via `cached_parse` — same contract as
/// the catalog parse. Without this, every `ctx_from_index` call (i.e. every
/// `LlmConfig::from_env`, so every daemon turn and `/api/config`) re-read and
/// re-parsed the file even though it only ever changes via an atomic rewrite.
type CtxMapCache = FileCache<std::sync::Arc<BTreeMap<String, u64>>>;
static CTX_MAP_CACHE: OnceLock<Mutex<Option<CtxMapCache>>> = OnceLock::new();

pub fn ctx_from_index(model: &str) -> Option<u64> {
    // KB-sized file: one small read + parse instead of the 4MB catalog.
    let path = dex_ctx_index_path()?;
    let map = cached_parse(&CTX_MAP_CACHE, &path, |text| {
        let raw: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(text.as_deref()?).ok()?;
        let mut map = BTreeMap::new();
        for (id, value) in raw {
            if let Some(ctx) = value.as_u64().filter(|ctx| *ctx > 0) {
                map.insert(id, ctx);
            }
        }
        Some(std::sync::Arc::new(map))
    })?;
    let from_index = map.get(model.to_ascii_lowercase().as_str()).copied()?;
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
pub fn build_ctx_map(catalog: &serde_json::Value) -> BTreeMap<String, u64> {
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

/// Best-effort index write; failures just mean the next launch re-parses.
/// Atomic (unique tmp file + rename) so a concurrent `refresh` never leaves
/// a torn `models.ctx.json` for a reader mid-turn; a stale reader just falls
/// back to the full catalog parse on JSON error.
pub fn write_ctx_index(map: &BTreeMap<String, u64>) {
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
pub fn ensure_ctx_index(catalog: &serde_json::Value) {
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

pub fn load_dex_catalog() -> Option<std::sync::Arc<serde_json::Value>> {
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

pub fn catalog_context_window(model: &str) -> Option<u64> {
    catalog_limit(model, "context")
}

/// models.dev `limit.output` for the model — the generation cap wires that
/// must declare one up front (Anthropic `max_tokens`) clamp against. Served
/// from the per-generation index, so safe on hot paths.
pub fn catalog_output_limit_for(model: &str) -> Option<u64> {
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
pub fn catalog_endpoint_for_model(
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
