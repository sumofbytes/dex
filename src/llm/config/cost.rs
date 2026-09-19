use super::catalog_index::with_catalog_index;
use super::catalog_index::CostRates;
use super::catalog_index::IndexedModel;
use crate::protocol::Provider;

pub(crate) fn cost_rates(entry: &serde_json::Value) -> Option<CostRates> {
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
        output: rate(&["output"]),
    })
}

/// Shared 3-tier model-cost lookup for `usage_cost`: the entry whose `api`
/// matches the configured endpoint wins, then any catalog entry of the
/// configured provider, then any provider at all. Served from the
/// per-generation index — a map lookup plus a scan over one model's entries
/// instead of a full catalog walk.
pub(crate) fn resolve_model_cost(
    model: &str,
    provider_keys: &[String],
    base_url: &str,
) -> Option<CostRates> {
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
    usage: &crate::protocol::Usage,
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
