//! Complexity router (V1): deterministic tier classification + model lookup.
//!
//! Each turn is routed to a model tier by task complexity. V1 is
//! keyword/length heuristics only — no LLM call, no new dependencies, stdlib
//! only — so routing stays pure and unit-testable. An LLM classifier may
//! follow later behind an opt-in env gate (the `DEX_COMPACTION_LLM=1`
//! pattern); deterministic stays the default.
//!
//! Tiers (`low`/`medium`/`high`/`critical` on the wire):
//! * `Low` — typos, single-file reads, trivial Q&A.
//! * `Medium` — normal feature work, single-scope edits.
//! * `High` — multi-file refactors, auth/data paths, ambiguous specs.
//! * `Critical` — migrations, security, irreversible changes.

use std::str::FromStr;

/// Model tier for one turn. Lowercase on the wire (`Display`/`FromStr`
/// round-trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Low,
    Medium,
    High,
    Critical,
}

impl Tier {
    /// All tiers in ascending order (used by config `doctor` rows).
    pub(crate) const ALL: [Tier; 4] = [Tier::Low, Tier::Medium, Tier::High, Tier::Critical];

    /// Config-file/env key fragment for this tier (`low`…`critical`).
    pub(crate) fn key(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Medium => "medium",
            Tier::High => "high",
            Tier::Critical => "critical",
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

impl FromStr for Tier {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Tier::Low),
            "medium" => Ok(Tier::Medium),
            "high" => Ok(Tier::High),
            "critical" => Ok(Tier::Critical),
            _ => Err(format!(
                "unknown tier '{s}'; use low, medium, high or critical"
            )),
        }
    }
}

/// What `classify` sees: prompt length in bytes, history size in estimated
/// tokens, and a 0–9 heuristic count of how tool-heavy the prompt sounds
/// (see [`infer_tool_hints`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskSignal {
    pub(crate) prompt_len: usize,
    pub(crate) history_tokens: u64,
    pub(crate) tool_hints: u8,
}

/// Per-tier model selections (`provider/model` strings). Empty means unset —
/// [`model_for`] falls through to `medium`, then to top-level `model:`.
#[derive(Debug, Clone, Default)]
pub(crate) struct TierMap {
    pub(crate) low: String,
    pub(crate) medium: String,
    pub(crate) high: String,
    pub(crate) critical: String,
}

impl TierMap {
    pub(crate) fn get(&self, tier: Tier) -> &str {
        match tier {
            Tier::Low => &self.low,
            Tier::Medium => &self.medium,
            Tier::High => &self.high,
            Tier::Critical => &self.critical,
        }
    }
}

/// Resolve the model selection for a tier. Full `provider/model` strings ride
/// through untouched, so the existing `split_selection`/`resolve_selection`
/// path (catalog, `learned-apis.json`, `providers.<name>.api:` pins) keeps
/// working. Fallback chain: tier miss → `medium` → top-level `model:`
/// (`fallback`).
pub(crate) fn model_for<'a>(tier: Tier, map: &'a TierMap, fallback: &'a str) -> &'a str {
    let hit = map.get(tier);
    if !hit.is_empty() {
        return hit;
    }
    if tier != Tier::Medium && !map.medium.is_empty() {
        return &map.medium;
    }
    fallback
}

/// Rough "how tool-heavy does this prompt sound" count, saturating at 9.
/// One point per distinct tool-ish keyword; a prompt naming many tools/files
/// usually means multi-step work, which is what escalates a turn to `High`.
/// Heuristic only — `classify` treats it as one signal among three.
pub(crate) fn infer_tool_hints(text: &str) -> u8 {
    const HINTS: &[&str] = &[
        "read",
        "edit",
        "write",
        "bash",
        "run",
        "test",
        "grep",
        "find",
        "file",
        "multi-file",
        "refactor",
        "migrate",
        "auth",
        "schema",
        "migration",
    ];
    let lower = text.to_ascii_lowercase();
    HINTS.iter().filter(|h| lower.contains(**h)).count().min(9) as u8
}

/// Classify one turn. Order matters: critical keywords win over high ones,
/// and high keywords win over length signals, so an explicit "migration" is
/// never downgraded by being short. Anything without a signal is `Medium`
/// (normal feature work); only short, history-light, tool-free prompts with
/// a trivial shape reach `Low`.
pub(crate) fn classify(signal: &TaskSignal, text: &str) -> Tier {
    const CRITICAL: &[&str] = &[
        "migrat",
        "security",
        "vulnerab",
        "exploit",
        "privilege",
        "irreversib",
        "data loss",
        "drop table",
        "drop database",
        "delete production",
        "production data",
        "secret rotat",
        "rotate secret",
    ];
    const HIGH: &[&str] = &[
        "refactor",
        "multi-file",
        "multi file",
        "across files",
        "auth",
        "permission",
        "ambiguous",
        "architect",
        "race condition",
        "deadlock",
        "schema",
        "data path",
        "cross-cutting",
        "cross cutting",
    ];
    const LOW: &[&str] = &[
        "typo",
        "what is",
        "what are",
        "what's",
        "explain",
        "summar",
        "read file",
        "show me",
        "where is",
    ];
    let lower = text.to_ascii_lowercase();
    if CRITICAL.iter().any(|h| lower.contains(h)) {
        return Tier::Critical;
    }
    if HIGH.iter().any(|h| lower.contains(h)) {
        return Tier::High;
    }
    if signal.prompt_len > 6000 || signal.history_tokens > 30_000 || signal.tool_hints >= 5 {
        return Tier::High;
    }
    if signal.tool_hints == 0
        && signal.history_tokens < 4000
        && signal.prompt_len < 160
        && LOW.iter().any(|h| lower.contains(h))
    {
        return Tier::Low;
    }
    Tier::Medium
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(len: usize, history: u64, hints: u8) -> TaskSignal {
        TaskSignal {
            prompt_len: len,
            history_tokens: history,
            tool_hints: hints,
        }
    }

    #[test]
    fn tier_strings_round_trip() {
        for tier in Tier::ALL {
            let s = tier.to_string();
            assert_eq!(s.parse::<Tier>().unwrap(), tier);
        }
        assert_eq!("HIGH".parse::<Tier>().unwrap(), Tier::High);
        assert!("urgent".parse::<Tier>().is_err());
    }

    #[test]
    fn model_for_prefers_tier_then_medium_then_fallback() {
        let map = TierMap {
            low: "p/cheap".into(),
            medium: "p/mid".into(),
            ..TierMap::default()
        };
        assert_eq!(model_for(Tier::Low, &map, "p/base"), "p/cheap");
        // Unset high falls back to medium, not the selection.
        assert_eq!(model_for(Tier::High, &map, "p/base"), "p/mid");
        assert_eq!(model_for(Tier::Medium, &map, "p/base"), "p/mid");
        // Nothing set anywhere: the top-level `model:` selection.
        let empty = TierMap::default();
        assert_eq!(model_for(Tier::Critical, &empty, "p/base"), "p/base");
        assert_eq!(model_for(Tier::Medium, &empty, "p/base"), "p/base");
    }

    #[test]
    fn classify_routes_keywords_by_severity() {
        let plain = signal(400, 0, 0);
        assert_eq!(
            classify(&plain, "run the database migration"),
            Tier::Critical
        );
        assert_eq!(
            classify(&plain, "fix the security vulnerability"),
            Tier::Critical
        );
        assert_eq!(classify(&plain, "refactor auth across files"), Tier::High);
        assert_eq!(classify(&plain, "the spec is ambiguous"), Tier::High);
        // Critical wins over high even when both match.
        assert_eq!(
            classify(&plain, "migrate the auth refactor"),
            Tier::Critical
        );
    }

    #[test]
    fn classify_trivial_prompts_are_low_and_normal_work_is_medium() {
        assert_eq!(classify(&signal(12, 0, 0), "fix typo"), Tier::Low);
        assert_eq!(
            classify(&signal(40, 0, 0), "what is this function?"),
            Tier::Low
        );
        assert_eq!(
            classify(&signal(400, 0, 1), "add a retry to the fetch call"),
            Tier::Medium
        );
        // A long low-keyword prompt is still ordinary work, not trivial.
        assert_eq!(
            classify(&signal(500, 0, 0), "explain this module in detail please"),
            Tier::Medium
        );
    }

    #[test]
    fn classify_escalates_on_length_history_or_tool_hints() {
        assert_eq!(classify(&signal(7000, 0, 0), "add a retry"), Tier::High);
        assert_eq!(classify(&signal(400, 40_000, 0), "add a retry"), Tier::High);
        assert_eq!(
            classify(&signal(400, 0, 6), "read edit write bash run test"),
            Tier::High
        );
    }

    #[test]
    fn classify_is_deterministic() {
        let s = signal(400, 1000, 2);
        assert_eq!(
            classify(&s, "refactor the auth middleware"),
            classify(&s, "refactor the auth middleware")
        );
    }

    #[test]
    fn infer_tool_hints_saturates_at_nine() {
        assert_eq!(infer_tool_hints("hello"), 0);
        assert!(infer_tool_hints("read edit write bash run test grep find file") <= 9);
        assert_eq!(
            infer_tool_hints("read read read"),
            1,
            "distinct keywords only"
        );
    }
}
