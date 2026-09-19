//! Complexity router (V1): deterministic tier classification + model lookup.
//!
//! Each turn is routed to a model tier by task complexity. V1 is
//! stem/weight scoring only — no LLM call, no new dependencies, stdlib
//! only — so routing stays pure and unit-testable. An LLM classifier may
//! follow later behind an opt-in env gate (the `DEX_COMPACTION=llm`
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
//! How labels become tiers: the prompt is tokenized once, each token is
//! stem-normalized ([`norm_token`]), and stems score against small weighted
//! tables — one concept is one stem, not six inflected strings, so unseen
//! inflections (`migrated`, `architectural`) still hit while `author` never
//! matches `auth` (token equality/prefix, never substring search). Weights
//! corroborate: a lone `auth` (+2) stays `Balanced`; `auth` + `refactor`
//! reaches `Powerful`. True multi-word concepts (`drop table`,
//! `race condition`) stay literal phrases, matched whole-word. Structural
//! signals — file paths named in the prompt, code fences, real tool-call
//! counts from session history (not tool words in the prompt) — add their
//! own weight. Every hit records a `reason`, so a decision explains itself
//! (the joined `reasons` list, journaled per turn by the caller).

use std::str::FromStr;

use crate::protocol::ChatMessage;

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
            "fast" => Ok(Tier::Fast),
            "balanced" => Ok(Tier::Balanced),
            "powerful" => Ok(Tier::Powerful),
            _ => Err(format!(
                "unknown tier '{s}'; use fast, balanced or powerful"
            )),
        }
    }
}

/// What `classify` sees: prompt size in characters (not bytes — multibyte
/// text must not escalate on byte length), history size in estimated tokens,
/// real tool-call activity from the loaded session history, and a 0–9
/// heuristic count of how tool-heavy the prompt sounds (see
/// [`infer_tool_hints`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskSignal {
    pub(crate) prompt_chars: usize,
    pub(crate) history_tokens: u64,
    pub(crate) history_tool_calls: u32,
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

/// Which layer [`model_for`] resolves from: the tier's own selection, the
/// `balanced` fallback, or the top-level `model:` selection. [`model_for`]
/// and config's `routing_tier_display` share this so the doctor origin can
/// never drift from the runtime fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelForSource {
    Tier,
    Balanced,
    Fallback,
}

pub(crate) fn model_for_source(tier: Tier, map: &TierMap) -> ModelForSource {
    if !map.get(tier).is_empty() {
        ModelForSource::Tier
    } else if tier != Tier::Balanced && !map.balanced.is_empty() {
        ModelForSource::Balanced
    } else {
        ModelForSource::Fallback
    }
}

/// Resolve the model selection for a tier. Full `provider/model` strings ride
/// through untouched, so the existing `split_selection`/`resolve_selection`
/// path (catalog, `learned-apis.json`, `providers.<name>.api:` pins) keeps
/// working. Fallback chain: tier miss → `balanced` → top-level `model:`
/// (`fallback`).
pub(crate) fn model_for<'a>(tier: Tier, map: &'a TierMap, fallback: &'a str) -> &'a str {
    match model_for_source(tier, map) {
        ModelForSource::Tier => map.get(tier),
        ModelForSource::Balanced => &map.balanced,
        ModelForSource::Fallback => fallback,
    }
}

/// One classified turn: the tier, its corroborated score, and why.
/// `reasons` is empty for a featureless prompt (ordinary work); entries are
/// in classification order (powerful stems/phrases, then fast ones, then
/// structure). Callers log the whole list joined — the first entry alone
/// may not name the heaviest driver (see [`classify_with_reasons`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) tier: Tier,
    pub(crate) score: i32,
    pub(crate) reasons: Vec<&'static str>,
}

impl Decision {
    fn new(tier: Tier, score: i32, reasons: Vec<&'static str>) -> Self {
        Self {
            tier,
            score,
            reasons,
        }
    }
}

/// One weighted label: the stem its word family shares, its score weight,
/// and the human reason recorded when it hits. Positive stems escalate,
/// negative ones mark trivial work; nothing reaches `Powerful` on a single
/// +1/+2 alone — escalation needs corroboration (or a +3).
const POWERFUL_STEMS: &[(&str, i32, &str)] = &[
    ("migrat", 3, "migration work"),
    ("vulner", 3, "known vulnerability"),
    ("exploit", 3, "exploit"),
    ("secur", 2, "security-sensitive"),
    ("auth", 2, "auth"),
    ("authent", 2, "authentication"),
    ("authoriz", 2, "authorization"),
    ("permiss", 2, "permissions"),
    ("privileg", 2, "privilege escalation"),
    ("refactor", 2, "refactor"),
    ("deadlock", 2, "concurrency hazard"),
    ("irrevers", 2, "irreversible change"),
    ("architect", 1, "architecture"),
    ("ambigu", 1, "ambiguous scope"),
    ("schema", 1, "schema change"),
];

/// Irreversible or wide-blast-radius concepts that only exist as phrases.
/// Kept minimal on purpose: if it is one word, it belongs in
/// [`POWERFUL_STEMS`].
const POWERFUL_PHRASES: &[(&str, i32, &str)] = &[
    ("data loss", 3, "data-loss risk"),
    ("drop table", 3, "destructive SQL"),
    ("drop database", 3, "destructive SQL"),
    ("delete production", 3, "production delete"),
    ("production data", 3, "production data"),
    ("secret rotation", 2, "secret rotation"),
    ("secret rotations", 2, "secret rotation"),
    ("rotate secret", 2, "secret rotation"),
    ("race condition", 3, "concurrency hazard"),
    ("race conditions", 3, "concurrency hazard"),
    ("across files", 2, "multi-file scope"),
    ("multi file", 2, "multi-file scope"),
    ("cross cutting", 1, "cross-cutting change"),
    ("data path", 1, "data-path change"),
];

/// Trivial-work markers: negative weights that pull toward `Fast`.
/// `read file`/`show me`, typo fixes, and summary/explanation requests (-2)
/// mark trivial work on their own; question words (-1) need the
/// trivial-question shape below to get there.
const FAST_STEMS: &[(&str, i32, &str)] = &[
    ("typo", -2, "typo"),
    ("summar", -2, "summary request"),
    ("explain", -2, "explanation request"),
];

const FAST_PHRASES: &[(&str, i32, &str)] = &[
    ("read file", -2, "single-file read"),
    ("show me", -2, "single-file read"),
    ("what is", -1, "trivial Q&A"),
    ("what are", -1, "trivial Q&A"),
    ("where is", -1, "trivial Q&A"),
];

/// Closed-class tool verbs for [`infer_tool_hints`], in normalized form
/// (see [`norm_token`]: `reads`→`read`, `running`→`run`). One entry per
/// verb; entries with aliases count once no matter how many inflections
/// appear — `write`/`writes` normalize to `write` but `writing` to `writ`,
/// so the family shares one entry and `write writing` is one verb, not two.
/// Scope words (`refactor`, `auth`, …) score in the stem tables instead —
/// counting them here too would double-count one mention into escalation.
const TOOL_VERBS: &[&[&str]] = &[
    &["read"],
    &["edit"],
    &["writ", "write"],
    &["bash"],
    &["run"],
    &["test"],
    &["grep"],
    &["find"],
    &["file"],
];

/// Rough "how tool-heavy does this prompt sound" count, saturating at 9:
/// one point per distinct tool verb above. Heuristic only — `classify`
/// treats 5+ as one strong signal among several, and real session activity
/// (`history_tool_calls`) outranks it.
pub(crate) fn infer_tool_hints(text: &str) -> u8 {
    let tokens = tokenize(text);
    TOOL_VERBS
        .iter()
        .filter(|aliases| aliases.iter().any(|stem| tokens.iter().any(|t| t == *stem)))
        .count()
        .min(9) as u8
}

/// Real tool activity behind the turn: every assistant `tool_calls` entry in
/// the loaded session history. Ground truth for "multi-step session" —
/// unlike prompt words, the user cannot sway it by naming tools.
pub(crate) fn count_history_tool_calls(history: &[ChatMessage]) -> u32 {
    history
        .iter()
        .filter_map(|m| m.tool_calls.as_ref())
        .map(|calls| calls.len() as u32)
        .fold(0u32, u32::saturating_add)
}

/// How many file paths the prompt names: whitespace chunks that are
/// `a/b`-shaped or carry a 2–5-letter extension (`Cargo.toml`,
/// `src/main.rs`). Caps at 9 — ten paths are not wiser than nine.
fn count_path_refs(text: &str) -> usize {
    text.split_whitespace()
        .filter(|chunk| is_path_chunk(chunk))
        .take(9)
        .count()
}

/// One whitespace chunk looks like a path: `a/b`-shaped or `base.ext`
/// with a 2–5-letter alphanumeric extension. Surrounding punctuation
/// (backticks, quotes, brackets) is trimmed first; overlong chunks are
/// never paths.
fn is_path_chunk(chunk: &str) -> bool {
    let w = chunk.trim_matches(|c: char| "`\"'()[],;:.!?<>".contains(c) || c.is_whitespace());
    if w.len() < 3 || w.len() > 120 {
        return false;
    }
    // Bare English slash-conjunctions (`and/or`) are prose, not paths.
    if w.eq_ignore_ascii_case("and/or") {
        return false;
    }
    if w.contains('/') && w.chars().any(|c| c.is_ascii_alphanumeric()) {
        return true;
    }
    match w.rfind('.') {
        Some(i) => {
            let (base, ext) = (&w[..i], &w[i + 1..]);
            (2..=5).contains(&ext.len())
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
                && base.chars().any(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// Trivial-question shape: opens with an interrogative and either asks
/// outright (`?`) or is short. Catches `what is this?`, `where is X defined`
/// without needing the words themselves to score.
fn is_trivial_question(text: &str) -> bool {
    const OPENERS: &[&str] = &[
        "what", "where", "how", "why", "when", "which", "who", "is", "are", "can", "does", "do",
        "did", "will", "would", "should",
    ];
    let trimmed = text.trim_start().to_ascii_lowercase();
    let first: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if !OPENERS.contains(&first.as_str()) {
        return false;
    }
    text.trim_end().ends_with('?') || text.chars().count() < 160
}

/// Score thresholds: +3 corroborates into `Powerful` (one +3 stem/phrase, or
/// two +2s, or structure piling onto a +2); -2 with a short prompt reaches
/// `Fast`. Everything between is ordinary work — the wide `Balanced`
/// middle is intentional, not a fall-through.
const POWERFUL_SCORE: i32 = 3;
const FAST_SCORE: i32 = -2;
/// `Fast` stays short: a pasted dump mentioning a typo is still real work.
const FAST_MAX_CHARS: usize = 400;

/// Structure: what the turn looks like, not what it says — named file
/// paths, code fences, real session activity, and the legacy decisive
/// signals (enormous prompt/session/tool count). Push order here is the
/// reason order callers see, so it stays exactly as classified.
fn score_structure(signal: &TaskSignal, text: &str, push: &mut impl FnMut(i32, &'static str)) {
    match count_path_refs(text) {
        n if n >= 3 => push(2, "names 3+ files"),
        2 => push(1, "names 2 files"),
        _ => {}
    }
    if text.contains("```") {
        push(1, "includes code");
    }
    // Real session activity outranks prompt words: a session already ten
    // tool calls deep is multi-step work whatever this prompt names.
    match signal.history_tool_calls {
        n if n >= 10 => push(2, "busy session history"),
        n if n >= 4 => push(1, "active session history"),
        _ => {}
    }
    // Legacy decisive signals, kept whole: an enormous prompt, session, or
    // tool count escalates on its own, with the reason saying so.
    if signal.tool_hints >= 5 {
        push(3, "tool-heavy prompt");
    }
    if signal.prompt_chars > 6000 {
        push(3, "very long prompt");
    }
    if signal.history_tokens > 30_000 {
        push(3, "very large session");
    }
    if is_trivial_question(text) {
        push(-1, "trivial Q&A shape");
    }
}

/// Classify one turn with reasons. Single tokenize, then additive weights —
/// no early `OR → Powerful`: `fix typo in auth` nets to zero (`Balanced`)
/// instead of escalating on one word, and `explain architecture` stays put
/// (`explain` −1, `architecture` +1).
pub(crate) fn classify_with_reasons(signal: &TaskSignal, text: &str) -> Decision {
    let tokens = tokenize(text);
    // Hyphens fold to spaces so `multi-file` and `multi file` (likewise
    // `cross-cutting`) match one phrase entry.
    let lower = text.to_ascii_lowercase().replace('-', " ");
    let mut score = 0i32;
    let mut reasons: Vec<&'static str> = Vec::new();
    let mut reason_weights: Vec<i32> = Vec::new();
    // Reasons dedupe by string with max-weight-wins: one concept, one vote
    // (`drop table` + `drop database` is a single destructive-SQL signal,
    // +3 once, not twice; `deadlock` +2 under `race condition` +3 keeps +3).
    let mut push = |weight: i32, reason: &'static str| {
        if let Some(i) = reasons.iter().position(|r| *r == reason) {
            if weight > reason_weights[i] {
                score += weight - reason_weights[i];
                reason_weights[i] = weight;
            }
        } else {
            score += weight;
            reasons.push(reason);
            reason_weights.push(weight);
        }
    };

    score_hits(POWERFUL_STEMS, &mut push, |stem| stem_hit(&tokens, stem));
    score_hits(POWERFUL_PHRASES, &mut push, |phrase| {
        word_hit(&lower, phrase)
    });
    score_hits(FAST_STEMS, &mut push, |stem| stem_hit(&tokens, stem));
    score_hits(FAST_PHRASES, &mut push, |phrase| word_hit(&lower, phrase));

    // Structure: what the turn looks like, not what it says.
    score_structure(signal, text, &mut push);

    // Reason order is classification order: powerful stems/phrases score
    // before fast ones, structure last — so the first entry usually names
    // what drove the tier, but a later +3 piling onto an earlier +1 keeps
    // the earlier entry first (pinned by
    // `first_reason_is_first_hit_in_classification_order`); callers log the
    // whole list, so no driver is lost.
    if score >= POWERFUL_SCORE {
        Decision::new(Tier::Powerful, score, reasons)
    } else if score <= FAST_SCORE && signal.prompt_chars < FAST_MAX_CHARS {
        Decision::new(Tier::Fast, score, reasons)
    } else {
        Decision::new(Tier::Balanced, score, reasons)
    }
}

// --- stem: word-stem matching primitives (folded from router/stem.rs) ---

// Word-stem matching primitives for tier classification: tokenize,
// stem-normalize, and score weighted hit tables. Pure string logic —
// no routing tables, no decisions.

/// Stem-normalize one lowercased alphanumeric token through three small
/// steps (plural → verb ending → doubled consonant) so one table stem
/// covers its inflections — `migrates`/`migrating` → `migrat`,
/// `vulnerabilities` → `vulnerability`, `running` → `run`.
/// Nominalizations (`-ion`/`-ation`) are deliberately left alone: `migration`
/// already starts with `migrat`, while `authorization` must not fold into
/// `author`, so each family lists the stem its forms share as a prefix
/// (see `stem_hit`).
pub(crate) fn norm_token(token: &str) -> String {
    let s = token.to_ascii_lowercase();
    let s = strip_plural(s);
    let s = strip_verb_ending(s);
    collapse_double_consonant(s)
}

/// Strip regular plural endings (`-ies→y`/`-es`/`-s`); the length and
/// `ss`/`us` guards keep short words, `class`, and `status` intact.
pub(crate) fn strip_plural(mut s: String) -> String {
    if s.len() > 4 && s.ends_with("ies") {
        s.truncate(s.len() - 3);
        s.push('y');
    } else if s.len() > 4 && s.ends_with("es") && !s.ends_with("sses") {
        s.truncate(s.len() - 2);
    } else if s.len() > 3 && s.ends_with('s') && !s.ends_with("ss") && !s.ends_with("us") {
        s.truncate(s.len() - 1);
    }
    s
}

/// Strip regular verb endings (`-ing`/`-ed`); length guards keep short
/// words (`red`, `sing`) intact.
pub(crate) fn strip_verb_ending(mut s: String) -> String {
    if s.len() > 5 && s.ends_with("ing") {
        s.truncate(s.len() - 3);
    } else if s.len() > 4 && s.ends_with("ed") {
        s.truncate(s.len() - 2);
    }
    s
}

/// Collapse a doubled trailing consonant left by the steps above:
/// `running` → `runn` → `run`.
pub(crate) fn collapse_double_consonant(mut s: String) -> String {
    let b = s.as_bytes();
    if s.len() > 3 && b[s.len() - 1] == b[s.len() - 2] && b[s.len() - 1].is_ascii_alphabetic() {
        s.truncate(s.len() - 1);
    }
    s
}

/// Split text into normalized tokens in one pass. Non-alphanumeric bytes
/// (including `_`, `-`, `.`, `/`) are boundaries, so `multi-file` and
/// `multi file` tokenize identically and `author`/`already`/`credits` can
/// never match `auth`/`read`/`edit` — substring false positives are
/// impossible by construction.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            tokens.push(norm_token(&current));
            current.clear();
        }
    }
    if !current.is_empty() {
        tokens.push(norm_token(&current));
    }
    tokens
}

/// Stem hit: exact equality, or prefix when the stem is long enough to be
/// distinctive (`migrat` matches `migrate`/`migration`/`migrated`; short
/// stems like `auth` stay exact so `author` never hits).
pub(crate) fn stem_hit(tokens: &[String], stem: &str) -> bool {
    tokens
        .iter()
        .any(|t| t == stem || (stem.len() >= 5 && t.starts_with(stem)))
}

/// ASCII word character for phrase-boundary checks.
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whole-phrase hit on already-lowercased text: the phrase needs a non-word
/// character (or string edge) on both sides. Only true multi-word concepts
/// live here — everything single-word scores through stems instead.
pub(crate) fn word_hit(text: &str, needle: &str) -> bool {
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

/// Score one weighted table: each hit adds its weight with its reason
/// (deduped by the caller). One helper covers all four tables — callers
/// pass [`stem_hit`] over tokens or [`word_hit`] over lowercased text.
pub(crate) fn score_hits(
    table: &[(&str, i32, &'static str)],
    push: &mut impl FnMut(i32, &'static str),
    mut is_hit: impl FnMut(&str) -> bool,
) {
    for (key, weight, reason) in table {
        if is_hit(key) {
            push(*weight, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(signal: &TaskSignal, text: &str) -> Tier {
        classify_with_reasons(signal, text).tier
    }

    fn signal(chars: usize, history: u64, history_calls: u32, hints: u8) -> TaskSignal {
        TaskSignal {
            prompt_chars: chars,
            history_tokens: history,
            history_tool_calls: history_calls,
            tool_hints: hints,
        }
    }

    fn plain(text: &str) -> (TaskSignal, u8) {
        let hints = infer_tool_hints(text);
        (signal(text.chars().count(), 0, 0, hints), hints)
    }

    fn tier_of(text: &str) -> Tier {
        let (s, _) = plain(text);
        classify(&s, text)
    }

    #[test]
    fn tier_strings_round_trip() {
        for tier in Tier::ALL {
            let s = tier.to_string();
            assert_eq!(s.parse::<Tier>().unwrap(), tier);
        }
        assert_eq!("POWERFUL".parse::<Tier>().unwrap(), Tier::Powerful);
        assert!("urgent".parse::<Tier>().is_err());
        // No four-tier aliases: the shipped knob is fast/balanced/powerful.
        assert!("low".parse::<Tier>().is_err());
        assert!("medium".parse::<Tier>().is_err());
        assert!("high".parse::<Tier>().is_err());
        assert!("critical".parse::<Tier>().is_err());
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

    /// Golden corpus: prompt → tier. The word cloud lives or dies here —
    /// add the prompt when a label changes, not another inflection list.
    #[test]
    fn corpus_routes_representative_prompts() {
        let cases: &[(&str, Tier)] = &[
            // Fast: typos, single-file reads, trivial Q&A.
            ("fix typo", Tier::Fast),
            ("fix all typos in README", Tier::Fast),
            ("what is this function?", Tier::Fast),
            ("where is the config loader?", Tier::Fast),
            ("read file Cargo.toml", Tier::Fast),
            ("show me the file", Tier::Fast),
            ("summarize yesterday's standup notes", Tier::Fast),
            // Balanced: ordinary work, including prompts that name one
            // heavy word without corroboration.
            ("add a retry to the fetch call", Tier::Balanced),
            ("the spec is ambiguous", Tier::Balanced),
            ("explain architecture of this file", Tier::Balanced),
            ("fix typo in auth middleware", Tier::Balanced),
            ("where is the schema defined?", Tier::Balanced),
            ("the author already reviewed the credits", Tier::Balanced),
            ("read the latest test run", Tier::Balanced),
            // Powerful: corroborated scope, security, irreversible work.
            ("run the database migration", Tier::Powerful),
            ("migrated the sessions table, now backfill", Tier::Powerful),
            ("fix the security vulnerability", Tier::Powerful),
            ("refactor auth across files", Tier::Powerful),
            ("ambiguous spec, refactor the auth layer", Tier::Powerful),
            ("authenticate the user before the migration", Tier::Powerful),
            ("drop table sessions", Tier::Powerful),
            ("watch for race conditions in the pool", Tier::Powerful),
            ("rotate secrets after the exploit", Tier::Powerful),
        ];
        for (prompt, expected) in cases {
            let (s, _) = plain(prompt);
            assert_eq!(classify(&s, prompt), *expected, "{prompt}");
        }
    }

    #[test]
    fn single_heavy_word_no_longer_escalates_alone() {
        // The old boolean-OR escalated any of these on its own; weights
        // need corroboration now.
        for prompt in [
            "the spec is ambiguous",
            "explain architecture of this file",
            "where is the schema defined?",
            "fix typo in auth middleware",
        ] {
            assert_eq!(tier_of(prompt), Tier::Balanced, "{prompt}");
        }
        // …while corroborated pairs still escalate.
        assert_eq!(tier_of("refactor the auth middleware"), Tier::Powerful);
        assert_eq!(tier_of("migrate auth to the new schema"), Tier::Powerful);
    }

    #[test]
    fn tokenizing_kills_substring_false_positives() {
        // `auth` in author, `read` in already, `edit` in credits, `test` in
        // latest: tokens never match, so ordinary prose stays `Balanced`.
        assert_eq!(
            tier_of("the author already reviewed the credits"),
            Tier::Balanced
        );
        assert_eq!(infer_tool_hints("I already reviewed the credits"), 0);
        assert_eq!(tier_of("read the latest test run"), Tier::Balanced);
        assert_eq!(infer_tool_hints("read the latest test run"), 3);
    }

    #[test]
    fn stems_cover_unseen_inflections() {
        // Never in any list, still classified: stemming generalizes.
        assert_eq!(tier_of("migrated the sessions table"), Tier::Powerful);
        assert_eq!(tier_of("migrating to the new store"), Tier::Powerful);
        assert_eq!(
            tier_of("architectural overview of the pool",),
            Tier::Balanced
        );
        assert_eq!(tier_of("refactored the loader a little"), Tier::Balanced);
        assert_eq!(tier_of("summarise the thread",), Tier::Fast);
    }

    #[test]
    fn trivial_prompts_are_fast_and_normal_work_is_balanced() {
        assert_eq!(tier_of("fix typo"), Tier::Fast);
        assert_eq!(tier_of("what is this function?"), Tier::Fast);
        assert_eq!(tier_of("add a retry to the fetch call"), Tier::Balanced);
        // A long explanation request is still ordinary work, not trivial:
        // score −1 never reaches Fast whatever the length.
        let long = format!("explain this module in detail please\n{}", "x".repeat(500));
        let s = signal(long.chars().count(), 0, 0, infer_tool_hints(&long));
        assert_eq!(classify(&s, &long), Tier::Balanced);
    }

    #[test]
    fn busy_session_history_escalates_without_prompt_words() {
        // Ten real tool calls behind a bland prompt: multi-step session.
        let s = signal(30, 2000, 12, 0);
        let d = classify_with_reasons(&s, "continue from where we left off");
        assert_eq!(d.tier, Tier::Balanced);
        // …corroborated by one scope word it crosses into Powerful.
        let d = classify_with_reasons(&s, "continue the refactor");
        assert_eq!(d.tier, Tier::Powerful);
        assert!(d.reasons.contains(&"busy session history"));
    }

    #[test]
    fn decisions_explain_themselves() {
        let (s, _) = plain("refactor auth across files");
        let d = classify_with_reasons(&s, "refactor auth across files");
        assert_eq!(d.tier, Tier::Powerful);
        assert!(d.score >= 3, "score {}", d.score);
        assert!(d.reasons.contains(&"refactor"));
        assert!(d.reasons.contains(&"auth"));
        assert!(!d.reasons.is_empty());

        let (s, _) = plain("add a retry to the fetch call");
        let d = classify_with_reasons(&s, "add a retry to the fetch call");
        assert_eq!(d.tier, Tier::Balanced);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn classify_escalates_on_legacy_decisive_signals() {
        let long = format!("add a retry {}", "x".repeat(7000));
        let s = signal(long.chars().count(), 0, 0, 0);
        let d = classify_with_reasons(&s, &long);
        assert_eq!(d.tier, Tier::Powerful);
        assert!(d.reasons.contains(&"very long prompt"));
        assert_eq!(
            classify(&signal(400, 40_000, 0, 0), "add a retry"),
            Tier::Powerful
        );
        assert_eq!(
            classify(&signal(400, 0, 0, 6), "read edit write bash run test"),
            Tier::Powerful
        );
    }

    #[test]
    fn classify_is_deterministic() {
        let (s, _) = plain("refactor the auth middleware");
        assert_eq!(
            classify(&s, "refactor the auth middleware"),
            classify(&s, "refactor the auth middleware")
        );
    }

    #[test]
    fn infer_tool_hints_counts_distinct_tool_verbs() {
        assert_eq!(infer_tool_hints("hello"), 0);
        assert_eq!(
            infer_tool_hints("read edit write bash run test grep find file"),
            9
        );
        // Scope words score elsewhere — hints stay tool verbs only.
        assert_eq!(infer_tool_hints("refactor migrate auth schema"), 0);
        assert_eq!(infer_tool_hints("read read read"), 1);
    }

    #[test]
    fn history_tool_calls_count_real_calls() {
        use crate::protocol::{FunctionCall, LlmToolCall, Role};
        let call = || LlmToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        let history = vec![
            ChatMessage {
                role: Role::User,
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning_items: None,
                reasoning_content: None,
            },
            ChatMessage {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![call(), call()]),
                tool_call_id: None,
                name: None,
                reasoning_items: None,
                reasoning_content: None,
            },
        ];
        assert_eq!(count_history_tool_calls(&history), 2);
        assert_eq!(count_history_tool_calls(&[]), 0);
    }

    #[test]
    fn first_reason_is_first_hit_in_classification_order() {
        // A +2 stem hit plus a later +3 structure signal keeps the stem
        // first in `reasons` — order is classification order, not weight —
        // while the log label joins the whole list, so the +3 still shows.
        let long = format!("refactor the loader {}", "x".repeat(7000));
        let s = signal(long.chars().count(), 0, 0, infer_tool_hints(&long));
        let d = classify_with_reasons(&s, &long);
        assert_eq!(d.tier, Tier::Powerful);
        assert_eq!(d.reasons.first(), Some(&"refactor"));
        assert!(d.reasons.contains(&"very long prompt"));
    }

    #[test]
    fn duplicate_reasons_count_once() {
        // One concept, one vote: two destructive-SQL phrases add +3 once.
        let prompt = "drop table sessions and drop database prod";
        let (s, _) = plain(prompt);
        let d = classify_with_reasons(&s, prompt);
        assert_eq!(d.tier, Tier::Powerful);
        assert_eq!(d.score, 3);
        assert_eq!(d.reasons, vec!["destructive SQL"]);
    }

    #[test]
    fn duplicate_reasons_keep_max_weight() {
        // `deadlock` (+2) and `race condition` (+3) share one reason: one
        // vote, max weight wins.
        let prompt = "deadlock in the pool, looks like a race condition";
        let (s, _) = plain(prompt);
        let d = classify_with_reasons(&s, prompt);
        assert_eq!(d.tier, Tier::Powerful);
        assert_eq!(d.score, 3);
        assert_eq!(d.reasons, vec!["concurrency hazard"]);
    }

    #[test]
    fn slash_conjunctions_are_not_paths() {
        assert!(!is_path_chunk("and/or"));
        assert!(!is_path_chunk("`and/or`,"));
        // Real slash paths still count.
        assert!(is_path_chunk("src/main.rs"));
        assert!(is_path_chunk("a/b"));
        let (s, _) = plain("use this and/or that approach");
        assert_eq!(
            classify(&s, "use this and/or that approach"),
            Tier::Balanced
        );
        // Boundary: two real paths plus `and/or` are two paths (+1), not
        // three (+2) — miscounting would escalate below.
        assert_eq!(count_path_refs("src/main.rs src/lib.rs and/or"), 2);
        // `auth` (+2) plus one file point stays Balanced; with `and/or`
        // miscounted as a second path it would reach Powerful.
        let (s, _) = plain("auth src/main.rs and/or");
        assert_eq!(classify(&s, "auth src/main.rs and/or"), Tier::Balanced);
    }

    #[test]
    fn write_inflections_count_as_one_verb() {
        // `write`/`writes`→`write` but `writing`→`writ`: one shared entry,
        // one vote. (`write writes` would pass even without the alias —
        // both normalize to `write` — so the regression case is
        // `write writing`.)
        assert_eq!(infer_tool_hints("write it"), 1);
        assert_eq!(infer_tool_hints("writes it"), 1);
        assert_eq!(infer_tool_hints("writing it"), 1);
        assert_eq!(infer_tool_hints("write writing"), 1);
    }
}
