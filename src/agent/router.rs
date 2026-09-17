//! Complexity router (V1): deterministic tier classification + model lookup.
//!
//! Each turn is routed to a model tier by task complexity. V1 is
//! keyword/length heuristics only — no LLM call, no new dependencies, stdlib
//! only — so routing stays pure and unit-testable. An LLM classifier may
//! follow later behind an opt-in env gate (the `DEX_COMPACTION_LLM=1`
//! pattern); deterministic stays the default.
//!
//! This lives in core rather than as a Lua extension: the tier picks the
//! model the turn's `LlmConfig` is rebuilt with *before* the first LLM call,
//! and extensions only see the resolved snapshot — they cannot swap models.
//!
//! Tiers (`fast`/`balanced`/`powerful` on the wire) mirror the vendors'
//! three capability buckets — OpenAI `nano`/`Luna` ↔ `mini`/`Terra` ↔
//! flagship/`Sol`, Anthropic `Haiku` ↔ `Sonnet` ↔ `Opus`:
//! * `Fast` — typos, single-file reads, trivial Q&A.
//! * `Balanced` — normal feature work, single-scope edits. Also the default
//!   and the fallback tier.
//! * `Powerful` — multi-file refactors, auth/data paths, ambiguous specs,
//!   migrations, security, irreversible changes.
//!
//! Matching is word-boundary aware (see [`word_hit`]): `auth` must not fire
//! on `author`, `read` must not fire on `already`.

use std::str::FromStr;

/// Model tier for one turn. Lowercase on the wire (`Display`/`FromStr`
/// round-trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Fast,
    Balanced,
    Powerful,
}

impl Tier {
    /// All tiers in ascending order (used by config `doctor` rows).
    pub(crate) const ALL: [Tier; 3] = [Tier::Fast, Tier::Balanced, Tier::Powerful];

    /// Config-file/env key fragment for this tier (`fast`…`powerful`).
    pub(crate) fn key(self) -> &'static str {
        match self {
            Tier::Fast => "fast",
            Tier::Balanced => "balanced",
            Tier::Powerful => "powerful",
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
            "fast" | "low" => Ok(Tier::Fast),
            "balanced" | "medium" => Ok(Tier::Balanced),
            // `high` and `critical` merged into one bucket (see `classify`).
            "powerful" | "high" | "critical" => Ok(Tier::Powerful),
            _ => Err(format!(
                "unknown tier '{s}'; use fast, balanced or powerful"
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
/// [`model_for`] falls through to `balanced`, then to top-level `model:`.
#[derive(Debug, Clone, Default)]
pub(crate) struct TierMap {
    pub(crate) fast: String,
    pub(crate) balanced: String,
    pub(crate) powerful: String,
}

impl TierMap {
    pub(crate) fn get(&self, tier: Tier) -> &str {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Balanced => &self.balanced,
            Tier::Powerful => &self.powerful,
        }
    }
}

/// Resolve the model selection for a tier. Full `provider/model` strings ride
/// through untouched, so the existing `split_selection`/`resolve_selection`
/// path (catalog, `learned-apis.json`, `providers.<name>.api:` pins) keeps
/// working. Fallback chain: tier miss → `balanced` → top-level `model:`
/// (`fallback`).
pub(crate) fn model_for<'a>(tier: Tier, map: &'a TierMap, fallback: &'a str) -> &'a str {
    let hit = map.get(tier);
    if !hit.is_empty() {
        return hit;
    }
    if tier != Tier::Balanced && !map.balanced.is_empty() {
        return &map.balanced;
    }
    fallback
}

/// ASCII word character for boundary checks.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whole-word/phrase hit: every occurrence of `needle` needs a non-word
/// character (or string edge) on both sides. Both sides must already be
/// lowercased. Keeps `auth` from firing on `author` and `read` from firing
/// on `already`.
fn word_hit(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    text.match_indices(needle).any(|(i, _)| {
        let before_ok = !matches!(text[..i].chars().next_back(), Some(c) if is_word_char(c));
        let after_ok =
            !matches!(text[i + needle.len()..].chars().next(), Some(c) if is_word_char(c));
        before_ok && after_ok
    })
}

/// Rough "how tool-heavy does this prompt sound" count, saturating at 9.
/// One point per distinct tool-ish keyword (inflections listed explicitly so
/// matching stays whole-word); a prompt naming many tools/files usually
/// means multi-step work, which is what escalates a turn to `Powerful`.
/// Heuristic only — `classify` treats it as one signal among three.
pub(crate) fn infer_tool_hints(text: &str) -> u8 {
    const HINTS: &[&str] = &[
        "read",
        "reads",
        "reading",
        "edit",
        "edits",
        "editing",
        "write",
        "writes",
        "writing",
        "bash",
        "run",
        "runs",
        "running",
        "test",
        "tests",
        "testing",
        "grep",
        "find",
        "file",
        "files",
        "multi-file",
        "refactor",
        "refactors",
        "refactoring",
        "migrate",
        "migrates",
        "migrating",
        "migration",
        "migrations",
        "auth",
        "authenticate",
        "authenticates",
        "authenticated",
        "authenticating",
        "authentication",
        "authorization",
        "schema",
        "schemas",
    ];
    let lower = text.to_ascii_lowercase();
    HINTS.iter().filter(|h| word_hit(&lower, h)).count().min(9) as u8
}

/// Classify one turn. Order matters: powerful keywords win over length
/// signals, so an explicit "migration" is never downgraded by being short.
/// Anything without a signal is `Balanced` (normal feature work); only
/// short, history-light prompts with a trivial shape reach `Fast`.
pub(crate) fn classify(signal: &TaskSignal, text: &str) -> Tier {
    const POWERFUL: &[&str] = &[
        // Irreversible / security-sensitive.
        "migrate",
        "migrates",
        "migrating",
        "migration",
        "migrations",
        "security",
        "vulnerability",
        "vulnerabilities",
        "vulnerable",
        "exploit",
        "exploits",
        "privilege",
        "privileges",
        "irreversible",
        "irreversibly",
        "data loss",
        "drop table",
        "drop database",
        "delete production",
        "production data",
        "secret rotation",
        "secret rotations",
        "secret rotate",
        "rotate secret",
        // Hard multi-scope work.
        "refactor",
        "refactors",
        "refactoring",
        "multi-file",
        "multi file",
        "across files",
        "auth",
        "authenticate",
        "authenticates",
        "authenticated",
        "authenticating",
        "authentication",
        "authorization",
        "permission",
        "permissions",
        "ambiguous",
        "ambiguity",
        "architect",
        "architecture",
        "race condition",
        "deadlock",
        "deadlocks",
        "schema",
        "schemas",
        "data path",
        "cross-cutting",
        "cross cutting",
    ];
    const FAST: &[&str] = &[
        "typo",
        "typos",
        "what is",
        "what are",
        "what's",
        "explain",
        "explains",
        "summary",
        "summaries",
        "summarize",
        "summarise",
        "read file",
        "show me",
        "where is",
    ];
    let lower = text.to_ascii_lowercase();
    if POWERFUL.iter().any(|h| word_hit(&lower, h)) {
        return Tier::Powerful;
    }
    if signal.prompt_len > 6000 || signal.history_tokens > 30_000 || signal.tool_hints >= 5 {
        return Tier::Powerful;
    }
    // `tool_hints <= 2`, not `== 0`: trivial reads name their own tools —
    // "read file Cargo.toml" already hints `read` + `file` — and the
    // powerful checks above already ran, so a couple of hints here cannot
    // hide hard work.
    if signal.tool_hints <= 2
        && signal.history_tokens < 4000
        && signal.prompt_len < 160
        && FAST.iter().any(|h| word_hit(&lower, h))
    {
        return Tier::Fast;
    }
    Tier::Balanced
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
        assert_eq!("POWERFUL".parse::<Tier>().unwrap(), Tier::Powerful);
        assert!("urgent".parse::<Tier>().is_err());
    }

    #[test]
    fn deprecated_tier_names_parse_as_aliases() {
        assert_eq!("low".parse::<Tier>().unwrap(), Tier::Fast);
        assert_eq!("medium".parse::<Tier>().unwrap(), Tier::Balanced);
        assert_eq!("high".parse::<Tier>().unwrap(), Tier::Powerful);
        assert_eq!("critical".parse::<Tier>().unwrap(), Tier::Powerful);
    }

    #[test]
    fn model_for_prefers_tier_then_balanced_then_fallback() {
        let map = TierMap {
            fast: "p/cheap".into(),
            balanced: "p/mid".into(),
            ..TierMap::default()
        };
        assert_eq!(model_for(Tier::Fast, &map, "p/base"), "p/cheap");
        // Unset powerful falls back to balanced, not the selection.
        assert_eq!(model_for(Tier::Powerful, &map, "p/base"), "p/mid");
        assert_eq!(model_for(Tier::Balanced, &map, "p/base"), "p/mid");
        // Nothing set anywhere: the top-level `model:` selection.
        let empty = TierMap::default();
        assert_eq!(model_for(Tier::Powerful, &empty, "p/base"), "p/base");
        assert_eq!(model_for(Tier::Balanced, &empty, "p/base"), "p/base");
    }

    #[test]
    fn classify_routes_keywords_by_severity() {
        let plain = signal(400, 0, 0);
        assert_eq!(
            classify(&plain, "run the database migration"),
            Tier::Powerful
        );
        assert_eq!(
            classify(&plain, "fix the security vulnerability"),
            Tier::Powerful
        );
        assert_eq!(
            classify(&plain, "refactor auth across files"),
            Tier::Powerful
        );
        assert_eq!(classify(&plain, "the spec is ambiguous"), Tier::Powerful);
        assert_eq!(
            classify(&plain, "authenticate the user before the migration"),
            Tier::Powerful
        );
    }

    #[test]
    fn classify_ignores_substring_matches() {
        // `auth` in author, `read` in already, `edit` in credits, `test` in
        // latest: none of these is the tool/keyword, so ordinary prose stays
        // `Balanced`.
        let plain = signal(400, 0, 0);
        assert_eq!(
            classify(&plain, "the author already reviewed the credits"),
            Tier::Balanced
        );
        assert_eq!(infer_tool_hints("I already reviewed the credits"), 0);
        assert_eq!(
            classify(&plain, "read the latest test run"),
            Tier::Balanced,
            "`latest` must not count as an extra `test` hint, and two hints are not enough to escalate"
        );
        assert_eq!(infer_tool_hints("read the latest test run"), 3);
    }

    #[test]
    fn classify_trivial_prompts_are_fast_and_normal_work_is_balanced() {
        assert_eq!(classify(&signal(12, 0, 0), "fix typo"), Tier::Fast);
        assert_eq!(
            classify(&signal(40, 0, 0), "what is this function?"),
            Tier::Fast
        );
        assert_eq!(
            classify(&signal(400, 0, 1), "add a retry to the fetch call"),
            Tier::Balanced
        );
        // A long fast-keyword prompt is still ordinary work, not trivial.
        assert_eq!(
            classify(&signal(500, 0, 0), "explain this module in detail please"),
            Tier::Balanced
        );
    }

    #[test]
    fn classify_trivial_reads_reach_fast_despite_their_own_hints() {
        // `route_turn` feeds `infer_tool_hints(prompt)` back in, so "read
        // file …" always carries `read` + `file` hints — the fast gate must
        // tolerate them or the single-file-read shape is dead.
        for text in ["read file Cargo.toml", "show me the file"] {
            let hints = infer_tool_hints(text);
            assert!(hints <= 2, "{text} hints {hints}");
            let s = signal(text.len(), 0, hints);
            assert_eq!(classify(&s, text), Tier::Fast, "{text}");
        }
    }

    #[test]
    fn classify_escalates_on_length_history_or_tool_hints() {
        assert_eq!(classify(&signal(7000, 0, 0), "add a retry"), Tier::Powerful);
        assert_eq!(
            classify(&signal(400, 40_000, 0), "add a retry"),
            Tier::Powerful
        );
        assert_eq!(
            classify(&signal(400, 0, 6), "read edit write bash run test"),
            Tier::Powerful
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
        assert_eq!(
            infer_tool_hints("read edit write bash run test grep find file"),
            9
        );
        assert_eq!(
            infer_tool_hints(
                "read edit write bash run test grep find file refactor migrate auth schema"
            ),
            9,
            "saturates, never exceeds nine"
        );
        assert_eq!(
            infer_tool_hints("read read read"),
            1,
            "distinct keywords only"
        );
    }
}
