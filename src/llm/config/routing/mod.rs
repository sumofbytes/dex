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

/// Valid keys under the `routing:` mapping (`enabled` plus one per tier,
/// plus one effort key per tier for the `(model, thinking_effort)` tuple).
/// Used for typo hints: an unknown nested key warns instead of silently
/// doing nothing (the typo policy `load_config_file` uses for top level).
const KNOWN_ROUTING_KEYS: &[&str] = &[
    "enabled",
    "fast",
    "balanced",
    "powerful",
    "fast_effort",
    "balanced_effort",
    "powerful_effort",
];

/// Complexity-router switch (`routing.enabled:`) and per-tier env vars.
/// Env beats file, per the standard precedence; tiers name full
/// `provider/model` selections, so catalog/learned-API/`api:` pins keep
/// working through the existing selection path.
pub(crate) const ROUTING_ENV: &str = "DEX_ROUTING";
pub(crate) const ROUTING_FAST_ENV: &str = "DEX_ROUTING_FAST";
pub(crate) const ROUTING_BALANCED_ENV: &str = "DEX_ROUTING_BALANCED";
pub(crate) const ROUTING_POWERFUL_ENV: &str = "DEX_ROUTING_POWERFUL";
/// Per-tier reasoning-effort overrides (`DEX_ROUTING_<TIER>_EFFORT`).
pub(crate) const ROUTING_FAST_EFFORT_ENV: &str = "DEX_ROUTING_FAST_EFFORT";
pub(crate) const ROUTING_BALANCED_EFFORT_ENV: &str = "DEX_ROUTING_BALANCED_EFFORT";
pub(crate) const ROUTING_POWERFUL_EFFORT_ENV: &str = "DEX_ROUTING_POWERFUL_EFFORT";
/// Env var holding a tier's selection (`DEX_ROUTING_FAST`…).
fn routing_tier_env(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => ROUTING_FAST_ENV,
        Tier::Balanced => ROUTING_BALANCED_ENV,
        Tier::Powerful => ROUTING_POWERFUL_ENV,
    }
}

/// Env var holding a tier's reasoning effort (`DEX_ROUTING_FAST_EFFORT`…).
fn routing_effort_env(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => ROUTING_FAST_EFFORT_ENV,
        Tier::Balanced => ROUTING_BALANCED_EFFORT_ENV,
        Tier::Powerful => ROUTING_POWERFUL_EFFORT_ENV,
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

/// Origin wording for a tier's file effort (`config routing.fast_effort:`…).
fn routing_effort_file_origin(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "config routing.fast_effort:",
        Tier::Balanced => "config routing.balanced_effort:",
        Tier::Powerful => "config routing.powerful_effort:",
    }
}

/// File key fragment for a tier's effort (`fast_effort`…).
fn effort_key(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast_effort",
        Tier::Balanced => "balanced_effort",
        Tier::Powerful => "powerful_effort",
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
/// selections (empty = unset) and the per-tier reasoning efforts
/// (empty = unset, keep the turn's `thinking_effort:`). `from_env` and
/// `doctor` share this resolution so the origin rows cannot drift from
/// runtime routing.
pub(crate) struct RoutingResolution {
    pub(crate) enabled: bool,
    pub(crate) enabled_origin: &'static str,
    pub(crate) tiers: TierMap,
    pub(crate) tier_origins: TierOrigins,
    pub(crate) efforts: TierMap,
    pub(crate) effort_origins: TierOrigins,
}

/// One file key's trimmed selection (`None` = missing, empty, or
/// non-string). A present-but-empty or non-string value warns once and
/// falls through like a miss, never a hard error.
fn routing_file_key(file: &Option<serde_yaml::Value>, key: &str) -> Option<String> {
    routing_file_key_with_hint(file, key, "set a 'provider/model' selection string")
}

/// Same as [`routing_file_key`] with a caller-chosen hint for the warning.
fn routing_file_key_with_hint(
    file: &Option<serde_yaml::Value>,
    key: &str,
    hint: &str,
) -> Option<String> {
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
                &format!("ignoring routing.{key}: — {hint}"),
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

/// One tier's raw effort: `DEX_ROUTING_<TIER>_EFFORT` > file
/// `routing.<tier>_effort:`. Empty/missing at every layer is not an error —
/// `effort_for` falls through to `balanced` effort, then to no override
/// (the turn keeps its `thinking_effort:`).
fn routing_effort_value(
    file: &Option<serde_yaml::Value>,
    tier: Tier,
    miss_origin: &'static str,
) -> (String, &'static str) {
    let env_var = routing_effort_env(tier);
    if let Ok(raw) = env::var(env_var) {
        let trimmed = raw.trim().to_string();
        if !trimmed.is_empty() {
            return (trimmed, env_var);
        }
    }
    if let Some(effort) =
        routing_file_key_with_hint(file, effort_key(tier), "set a reasoning effort string")
    {
        return (effort, routing_effort_file_origin(tier));
    }
    (String::new(), miss_origin)
}

/// Shared `routing:` resolution: `DEX_ROUTING` > file `routing.enabled:`
/// > off, plus each tier's selection and effort. Default off — routing is opt-in.
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
    const EFFORT_BALANCED_MISS: &str = "unset (keeps thinking_effort:)";
    const EFFORT_TIER_MISS: &str =
        "unset (falls back to routing.balanced_effort:, then keeps thinking_effort:)";
    let mut efforts = TierMap::default();
    let mut effort_origins = TierOrigins {
        fast: EFFORT_TIER_MISS,
        balanced: EFFORT_BALANCED_MISS,
        powerful: EFFORT_TIER_MISS,
    };
    for tier in Tier::ALL {
        let miss = if tier == Tier::Balanced {
            EFFORT_BALANCED_MISS
        } else {
            EFFORT_TIER_MISS
        };
        let (value, origin) = routing_effort_value(file, tier, miss);
        match tier {
            Tier::Fast => {
                efforts.fast = value;
                effort_origins.fast = origin;
            }
            Tier::Balanced => {
                efforts.balanced = value;
                effort_origins.balanced = origin;
            }
            Tier::Powerful => {
                efforts.powerful = value;
                effort_origins.powerful = origin;
            }
        }
    }
    RoutingResolution {
        enabled,
        enabled_origin,
        tiers,
        tier_origins: origins,
        efforts,
        effort_origins,
    }
}

/// One routed turn: classify the prompt and resolve the tier's `(model,
/// thinking_effort)` tuple — the model through `model_for` (tier miss →
/// `balanced` → top-level `model:`), the effort through `effort_for`
/// (tier miss → `balanced` effort → no override, keep `thinking_effort:`).
/// `None` when routing is off or nothing selects a model (the normal
/// `from_env` setup error then explains itself). `model_override` is
/// `Some` only when the tier resolves away from the current selection,
/// so an unrouted turn rebuilds nothing. `effort_override` is `Some` only
/// when the tier names an effort. Callers with an explicit `--model` /
/// per-request model skip this — the explicit pick wins; an explicit
/// per-request thinking effort likewise wins over the routed effort.
pub(crate) struct RoutedTurn {
    pub(crate) tier: Tier,
    pub(crate) model_override: Option<String>,
    pub(crate) effort_override: Option<String>,
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
        &routing.efforts,
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

/// Resolve the reasoning effort for a tier: tier miss → `balanced`
/// effort → unset (empty). Unlike [`model_for`] there is no top-level
/// fallback here — unset means no override, the turn keeps whatever
/// `refresh_thinking_effort` resolved (`stored` > env > file).
pub(crate) fn effort_for(tier: Tier, map: &TierMap) -> &str {
    match effort_for_source(tier, map) {
        ModelForSource::Tier => map.get(tier),
        ModelForSource::Balanced => &map.balanced,
        ModelForSource::Fallback => "",
    }
}

/// Which layer [`effort_for`] resolves from; shared with [`model_for_source`]
/// so `doctor` origins mirror the runtime chain.
pub(crate) fn effort_for_source(tier: Tier, map: &TierMap) -> ModelForSource {
    model_for_source(tier, map)
}

/// Shared tail: resolve the tier's model through `model_for` (tier miss →
/// `balanced` → top-level `model:`) and its effort through `effort_for`
/// (tier miss → `balanced` effort → no override), overriding each only on change.
fn finish_route(
    selection_value: &str,
    tiers: &TierMap,
    efforts: &TierMap,
    tier: Tier,
    reasons: Vec<&'static str>,
) -> RoutedTurn {
    let model = model_for(tier, tiers, selection_value);
    let effort = effort_for(tier, efforts);
    RoutedTurn {
        tier,
        model_override: (model != selection_value).then(|| model.to_string()),
        effort_override: (!effort.is_empty()).then(|| effort.to_string()),
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

/// What `doctor` shows per tier effort: the tier's effective effort through
/// [`effort_for`] (tier miss → `balanced` effort → unset, keep
/// `thinking_effort:`) plus where it came from.
pub(crate) fn routing_effort_display(tier: Tier, routing: &RoutingResolution) -> (String, String) {
    let effort = effort_for(tier, &routing.efforts);
    let value = if effort.is_empty() {
        "(unset — keeps thinking_effort:)".to_string()
    } else {
        effort.to_string()
    };
    let origin = match effort_for_source(tier, &routing.efforts) {
        ModelForSource::Tier => routing.effort_origins.get(tier),
        ModelForSource::Balanced => routing.effort_origins.balanced,
        ModelForSource::Fallback => routing.effort_origins.get(tier),
    };
    (value, origin.to_string())
}
