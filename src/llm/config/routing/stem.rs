//! Word-stem matching primitives for tier classification: tokenize,
//! stem-normalize, and score weighted hit tables. Pure string logic —
//! no routing tables, no decisions.

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
