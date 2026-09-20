use super::load_config_file;
use super::selection::resolve_selection;
use super::warn_once;
pub(crate) mod classify;

use self::classify::classify_with_reasons;
use self::classify::count_history_tool_calls;
use self::classify::infer_tool_hints;
use self::classify::model_for;
use self::classify::model_for_source;
use self::classify::ModelForSource;
use self::classify::TaskSignal;
use self::classify::Tier;
use self::classify::TierMap;
use crate::agent::tokens::estimate_tokens;
use crate::protocol::ChatMessage;

use std::env;

/// Valid keys under the `routing:` mapping (`enabled` plus one per tier).
/// Used for typo hints: an unknown nested key warns instead of silently
/// doing nothing (the typo policy `load_config_file` uses for top level).
const KNOWN_ROUTING_KEYS: &[&str] = &["enabled", "fast", "balanced", "powerful"];

/// Complexity-router switch (`routing.enabled:`) and per-tier env vars.
/// Env beats file, per the standard precedence; tiers name full
/// `provider/model` selections, so catalog/learned-API/`api:` pins keep
/// working through the existing selection path.
pub(crate) const ROUTING_ENV: &str = "DEX_ROUTING";
pub(crate) const ROUTING_FAST_ENV: &str = "DEX_ROUTING_FAST";
pub(crate) const ROUTING_BALANCED_ENV: &str = "DEX_ROUTING_BALANCED";
pub(crate) const ROUTING_POWERFUL_ENV: &str = "DEX_ROUTING_POWERFUL";
/// Env var holding a tier's selection (`DEX_ROUTING_FAST`…).
fn routing_tier_env(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => ROUTING_FAST_ENV,
        Tier::Balanced => ROUTING_BALANCED_ENV,
        Tier::Powerful => ROUTING_POWERFUL_ENV,
    }
}

/// Origin wording for a tier's file selection (`config routing.fast:`…).
fn routing_file_origin(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "config routing.fast:",
        Tier::Balanced => "config routing.balanced:",
        Tier::Powerful => "config routing.powerful:",
    }
}

/// Per-tier origins for [`RoutingResolution`]: where each raw selection
/// came from. A miss falls back at use time (`model_for`: tier miss →
/// `balanced` → top-level `model:`), so the miss origin only says that.
pub(crate) struct TierOrigins {
    pub(crate) fast: &'static str,
    pub(crate) balanced: &'static str,
    pub(crate) powerful: &'static str,
}

impl TierOrigins {
    pub(crate) fn get(&self, tier: Tier) -> &'static str {
        match tier {
            Tier::Fast => self.fast,
            Tier::Balanced => self.balanced,
            Tier::Powerful => self.powerful,
        }
    }
}

/// The resolved `routing:` knob: the switch plus the raw per-tier
/// selections (empty = unset). `from_env` and `doctor` share this
/// resolution so the origin rows cannot drift from runtime routing.
pub(crate) struct RoutingResolution {
    pub(crate) enabled: bool,
    pub(crate) enabled_origin: &'static str,
    pub(crate) tiers: TierMap,
    pub(crate) tier_origins: TierOrigins,
}

/// One file key's trimmed selection (`None` = missing, empty, or
/// non-string). A present-but-empty or non-string value warns once and
/// falls through like a miss, never a hard error.
fn routing_file_key(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    let value = file
        .as_ref()
        .and_then(|f| f.get("routing"))
        .and_then(|r| r.as_mapping())
        .and_then(|m| m.get(key))?;
    match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s.to_string()),
        None => {
            warn_once(
                &format!("config:routing:{key}"),
                &format!("ignoring routing.{key}: — set a 'provider/model' selection string"),
            );
            None
        }
    }
}

/// One tier's raw selection: `DEX_ROUTING_<TIER>` > file `routing.<tier>:`.
/// Empty/missing at every layer is not an error — `model_for` falls
/// through to `balanced`, then to top-level `model:`.
fn routing_tier_value(
    file: &Option<serde_yaml::Value>,
    tier: Tier,
    miss_origin: &'static str,
) -> (String, &'static str) {
    let env_var = routing_tier_env(tier);
    if let Ok(raw) = env::var(env_var) {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return (trimmed, env_var);
        }
    }
    if let Some(selection) = routing_file_key(file, tier.key()) {
        return (selection, routing_file_origin(tier));
    }
    (String::new(), miss_origin)
}

/// Shared `routing:` resolution: `DEX_ROUTING` > file `routing.enabled:`
/// > off, plus each tier's selection. Default off — routing is opt-in.
pub(crate) fn routing_resolution(file: &Option<serde_yaml::Value>) -> RoutingResolution {
    const BALANCED_MISS: &str = "unset (falls back to model:)";
    const TIER_MISS: &str = "unset (falls back to routing.balanced:, then model:)";
    // Unknown `routing.*` keys are almost always typos (`routing.fsat:`);
    // name them instead of silently ignoring them.
    if let Some(map) = file
        .as_ref()
        .and_then(|f| f.get("routing"))
        .and_then(|r| r.as_mapping())
    {
        let unknown: Vec<&str> = map
            .keys()
            .filter_map(|k| k.as_str())
            .filter(|k| !KNOWN_ROUTING_KEYS.contains(k))
            .collect();
        if !unknown.is_empty() {
            warn_once(
                "config:routing:unknown-keys",
                &format!(
                    "unknown routing key(s) {} — valid keys: {}",
                    unknown.join(", "),
                    KNOWN_ROUTING_KEYS.join(", ")
                ),
            );
        }
    }
    let mut enabled = false;
    let mut enabled_origin = "built-in default";
    // Empty counts as unset at every layer and falls through silently; a
    // non-empty but unparseable value warns and likewise falls through to
    // the file (never a hard error — the typo policy `load_config_file`
    // uses).
    if let Ok(raw) = env::var(ROUTING_ENV) {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "0" | "false" | "off" | "no" => {
                enabled = false;
                enabled_origin = ROUTING_ENV;
            }
            "1" | "true" | "on" | "yes" => {
                enabled = true;
                enabled_origin = ROUTING_ENV;
            }
            _ => warn_once(
                "env:DEX_ROUTING",
                "DEX_ROUTING must be 0/1 (or true/false, on/off, yes/no) — ignoring it",
            ),
        }
    }
    if enabled_origin == "built-in default" {
        let flag = file
            .as_ref()
            .and_then(|f| f.get("routing"))
            .and_then(|r| r.as_mapping())
            .and_then(|m| m.get("enabled"));
        match flag {
            None => {}
            Some(v) => match v.as_bool() {
                Some(b) => {
                    enabled = b;
                    enabled_origin = "config routing.enabled:";
                }
                None => warn_once(
                    "config:routing:enabled",
                    "ignoring routing.enabled: — set true or false",
                ),
            },
        }
    }
    let mut tiers = TierMap::default();
    let mut origins = TierOrigins {
        fast: TIER_MISS,
        balanced: BALANCED_MISS,
        powerful: TIER_MISS,
    };
    for tier in Tier::ALL {
        let miss = if tier == Tier::Balanced {
            BALANCED_MISS
        } else {
            TIER_MISS
        };
        let (value, origin) = routing_tier_value(file, tier, miss);
        match tier {
            Tier::Fast => {
                tiers.fast = value;
                origins.fast = origin;
            }
            Tier::Balanced => {
                tiers.balanced = value;
                origins.balanced = origin;
            }
            Tier::Powerful => {
                tiers.powerful = value;
                origins.powerful = origin;
            }
        }
    }
    RoutingResolution {
        enabled,
        enabled_origin,
        tiers,
        tier_origins: origins,
    }
}

/// One routed turn: classify the prompt and resolve the tier's model
/// through `model_for` (tier miss → `balanced` → top-level `model:`).
/// `None` when routing is off or nothing selects a model (the normal
/// `from_env` setup error then explains itself). `model_override` is
/// `Some` only when the tier resolves away from the current selection,
/// so an unrouted turn rebuilds nothing. Callers with an explicit
/// `--model` / per-request model skip this — the explicit pick wins.
pub(crate) struct RoutedTurn {
    pub(crate) tier: Tier,
    pub(crate) model_override: Option<String>,
    /// Classification hits in classification order, for the per-turn log
    /// line (see [`reason_label`][Self::reason_label]).
    pub(crate) reasons: Vec<&'static str>,
}

impl RoutedTurn {
    /// Log label: every classification hit joined, or `"ordinary work"`
    /// when nothing fired — the first hit alone may not name the heaviest
    /// driver, so the line carries the whole list.
    pub(crate) fn reason_label(&self) -> String {
        if self.reasons.is_empty() {
            "ordinary work".to_string()
        } else {
            self.reasons.join(", ")
        }
    }
}

pub(crate) fn route_turn(prompt: &str, history: &[ChatMessage]) -> Option<RoutedTurn> {
    let file = load_config_file();
    let routing = routing_resolution(&file);
    if !routing.enabled {
        return None;
    }
    let selection = resolve_selection(None, &file).ok()?;
    let signal = routing_signal(prompt, history);
    let decision = classify_with_reasons(&signal, prompt);
    Some(finish_route(
        &selection.value,
        &routing.tiers,
        decision.tier,
        decision.reasons,
    ))
}

/// The shared routing signal: prompt size plus history token size and the
/// real tool-call count, so a deep session weighs in without prompt words.
/// The daemon reuses the turn's own history load, so routing sees exactly
/// what the turn will send.
fn routing_signal(prompt: &str, history: &[ChatMessage]) -> TaskSignal {
    TaskSignal {
        prompt_chars: prompt.chars().count(),
        history_tokens: estimate_tokens(history),
        history_tool_calls: count_history_tool_calls(history),
        tool_hints: infer_tool_hints(prompt),
    }
}

/// Shared tail: resolve the tier's model through `model_for` (tier miss →
/// `balanced` → top-level `model:`), overriding only on change.
fn finish_route(
    selection_value: &str,
    tiers: &TierMap,
    tier: Tier,
    reasons: Vec<&'static str>,
) -> RoutedTurn {
    let model = model_for(tier, tiers, selection_value);
    RoutedTurn {
        tier,
        model_override: (model != selection_value).then(|| model.to_string()),
        reasons,
    }
}

/// What `doctor` shows per tier: the tier's effective selection through
/// [`model_for`] (tier miss → `balanced` → top-level `model:`) plus where
/// that selection came from, so the row explains the turn's model.
pub(crate) fn routing_tier_display(
    tier: Tier,
    routing: &RoutingResolution,
    selection: Option<&str>,
    selection_source: &str,
) -> (String, String) {
    let fallback = selection.unwrap_or("(unset)");
    let value = model_for(tier, &routing.tiers, fallback).to_string();
    // The origin mirrors `model_for` through the shared `model_for_source`,
    // so the row always names the layer the turn's model actually came from.
    let origin = match model_for_source(tier, &routing.tiers) {
        ModelForSource::Tier => routing.tier_origins.get(tier),
        ModelForSource::Balanced => routing.tier_origins.balanced,
        ModelForSource::Fallback => match selection {
            Some(_) => selection_source,
            None => "UNCONFIGURED — set 'model: <provider>/<model>'",
        },
    };
    (value, origin.to_string())
}
