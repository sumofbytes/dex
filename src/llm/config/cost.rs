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

/// Compact rate: up to 4 decimals, no trailing zeros (`0.5`, not `0.5000`).
fn fmt_rate(v: f64) -> String {
    let s = format!("{v:.4}");
    format!("${}", s.trim_end_matches('0').trim_end_matches('.'))
}

/// `$in/$out per 1M` when input and output differ, else `$in per 1M`.
/// Missing output bills at the input rate (same rule as `usage_cost`).
pub(crate) fn format_cost_label(cost: &CostRates) -> String {
    let input = fmt_rate(cost.input);
    match cost.output {
        Some(out) if (out - cost.input).abs() > 1e-9 => {
            format!("{input}/{} per 1M", fmt_rate(out))
        }
        _ => format!("{input} per 1M"),
    }
}

/// One picker row's attribution: who serves the id and at what price.
/// Resolved in one catalog-index pass (see `model_hints_for`).
#[cfg_attr(not(any(feature = "tui", test)), allow(dead_code))]
pub(crate) struct ModelHint {
    pub(crate) provider: Option<String>,
    pub(crate) cost: Option<String>,
}

/// Provider + cost per picker row, in one catalog-index pass (the popup
/// runs per keystroke over thousands of ids — one stat + one map walk,
/// never one lookup per row). A qualified `prefix/tail` keeps its prefix
/// as the provider and prices only that provider's own entry (a different
/// provider's cheaper rate here would label the row `p` and bill it at
/// `q`'s price); a bare id attributes the cheapest priced entry (`from`
/// semantics: the row routes at pick time, the cheapest is the floor).
/// `None` fields when the catalog has no entry for the id.
#[cfg_attr(not(any(feature = "tui", test)), allow(dead_code))]
pub(crate) fn model_hints_for(selections: &[String]) -> Vec<ModelHint> {
    with_catalog_index(|index| {
        selections
            .iter()
            .map(|sel| {
                let (prefix, id) = match sel.split_once('/') {
                    Some((p, rest)) if !p.is_empty() && !rest.is_empty() => (Some(p), rest),
                    _ => (None, sel.as_str()),
                };
                let entries = index.by_id.get(id.to_ascii_lowercase().as_str());
                let Some(entries) = entries else {
                    return ModelHint {
                        provider: prefix.map(str::to_string),
                        cost: None,
                    };
                };
                let cheapest = |a: &&IndexedModel, b: &&IndexedModel| {
                    a.cost
                        .as_ref()
                        .map(|c| c.input)
                        .unwrap_or(f64::MAX)
                        .partial_cmp(&b.cost.as_ref().map(|c| c.input).unwrap_or(f64::MAX))
                        .unwrap_or(std::cmp::Ordering::Equal)
                };
                let priced = |e: &&IndexedModel| e.cost.is_some();
                let named = |e: &IndexedModel| (!e.provider.is_empty()).then(|| e.provider.clone());
                match prefix {
                    Some(p) => {
                        // Price only an entry of the NAMED provider;
                        // catalog-key aliases ride the builtin's key list
                        // (`openai-codex` → `codex`/`openai`), an unknown
                        // prefix has no honestly attributable price.
                        let keys: Vec<String> =
                            Provider::parse_known(p, &std::collections::BTreeSet::new())
                                .map(|pr| pr.catalog_keys())
                                .unwrap_or_else(|| vec![p.to_ascii_lowercase()]);
                        let pick = entries
                            .iter()
                            .filter(priced)
                            .find(|e| keys.iter().any(|k| e.provider.eq_ignore_ascii_case(k)));
                        ModelHint {
                            provider: Some(p.to_string()),
                            cost: pick.and_then(|e| e.cost.as_ref().map(format_cost_label)),
                        }
                    }
                    None => {
                        if let Some(e) = entries.iter().filter(priced).min_by(cheapest) {
                            ModelHint {
                                provider: named(e),
                                cost: e.cost.as_ref().map(format_cost_label),
                            }
                        } else {
                            ModelHint {
                                provider: entries.iter().find_map(named),
                                cost: None,
                            }
                        }
                    }
                }
            })
            .collect()
    })
    .unwrap_or_else(|| {
        selections
            .iter()
            .map(|sel| match sel.split_once('/') {
                Some((p, rest)) if !p.is_empty() && !rest.is_empty() => ModelHint {
                    provider: Some(p.to_string()),
                    cost: None,
                },
                _ => ModelHint {
                    provider: None,
                    cost: None,
                },
            })
            .collect()
    })
}

/// One display cost per picker row (batch wrapper over `model_hints_for`).
/// `None` when the catalog has no priced entry for the id.
#[cfg_attr(not(any(feature = "tui", test)), allow(dead_code))]
pub(crate) fn cost_hints_for(selections: &[String]) -> Vec<Option<String>> {
    model_hints_for(selections)
        .into_iter()
        .map(|h| h.cost)
        .collect()
}

/// Single-row convenience for `/model` confirmations.
#[cfg_attr(not(any(feature = "tui", test)), allow(dead_code))]
pub(crate) fn cost_hint_for(selection: &str) -> Option<String> {
    cost_hints_for(std::slice::from_ref(&selection.to_string()))
        .into_iter()
        .next()
        .flatten()
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
