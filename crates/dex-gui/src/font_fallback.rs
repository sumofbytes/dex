//! The mono slot's families, extended with installed fonts that actually draw Nerd Font icons.
//!
//! Rune looks for private-use glyphs only among the families a slot names — never as a system
//! fallback (`needs_no_font` deliberately skips U+E000–F8FF). Shell output (starship's prompt,
//! `eza --icons`) is full of them, so the slot must name a family that has them. The static list
//! covers the common cases (`ui-monospace` is the user's own monospace on every platform — Rune
//! expands it via `fc-match`/SF Mono/Cascadia); this scan adds whatever icon-capable family is
//! installed under any other name. Probing is exact: a font is kept only if its charmap maps the
//! probe codepoints, so name-based keyword filtering can be loose.

use fontique::{Collection, CollectionOptions, SourceCache, SourceCacheOptions};

/// The mono slot families we ship, before the discovered ones are appended.
const BASE: &str = "IBM Plex Mono, ui-monospace, GeistMono NF, Symbols Nerd Font Mono";

/// Codepoints the shell streams draw as icons, from two Nerd Font ranges (powerline, Font
/// Awesome). A font must map both to count — that means a complete Nerd Font, the only kind
/// that renders the whole icon set `ls`/prompt aliases emit.
const PROBES: [char; 2] = ['\u{E0B0}', '\u{F07B}'];

/// How many discovered families to name at most — a CSS list shouldn't grow unbounded.
const MAX_NAMES: usize = 3;

/// Lowercase fragments that mark a family worth probing first; anything else is scanned only
/// when these find nothing.
const KEYWORDS: [&str; 5] = ["nerd", "nf", "symbol", "icon", "powerline"];

/// The mono slot families for this machine, as a process-lifetime string (`AppSpec::families`
/// takes `&'static str`; the theme kit leaks the same way).
pub fn mono_families() -> &'static str {
    let extra = discovered();
    if extra.is_empty() {
        return BASE;
    }
    Box::leak(format!("{BASE}, {extra}").into_boxed_str())
}

/// Installed families whose fonts map the probe codepoints, most scan-order first, deduped
/// against [`BASE`].
fn discovered() -> String {
    let mut collection = Collection::new(CollectionOptions {
        system_fonts: true,
        ..CollectionOptions::default()
    });
    collection.load_system_fonts();
    let ids: Vec<fontique::FamilyId> = collection.family_ids().collect();
    let names: Vec<(String, fontique::FamilyId)> = ids
        .into_iter()
        .filter_map(|id| collection.family_name(id).map(|n| (n.to_owned(), id)))
        .collect();
    let base = BASE
        .split(", ")
        .map(|f| f.to_lowercase())
        .collect::<Vec<_>>();

    let (likely, rest): (Vec<_>, Vec<_>) = names.into_iter().partition(|(name, _)| {
        let name = name.to_lowercase();
        KEYWORDS.iter().any(|kw| name.contains(kw))
    });
    let mut found: Vec<String> = Vec::new();
    for (batch, thorough) in [(&likely, false), (&rest, true)] {
        for (name, id) in batch {
            let lowered = name.to_lowercase();
            if base.contains(&lowered) || found.iter().any(|f| f.eq_ignore_ascii_case(name)) {
                continue;
            }
            if covers_probes(&mut collection, *id) {
                found.push(name.clone());
                if found.len() == MAX_NAMES {
                    break;
                }
            }
        }
        // Found in the keyword pass: the expensive full scan is unnecessary.
        if !found.is_empty() && !thorough {
            break;
        }
    }
    found.join(", ")
}

/// Whether any face of the family maps every probe codepoint.
fn covers_probes(collection: &mut Collection, id: fontique::FamilyId) -> bool {
    let Some(family) = collection.family(id) else {
        return false;
    };
    let mut source_cache = SourceCache::new(SourceCacheOptions::default());
    family.fonts().iter().any(|font| {
        let Some(blob) = font.load(Some(&mut source_cache)) else {
            return false;
        };
        font.charmap_index()
            .charmap(blob.data())
            .is_some_and(|charmap| PROBES.iter().all(|c| charmap.map(*c).is_some()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_families_keeps_the_base_prefix_and_yields_clean_names() {
        let t0 = std::time::Instant::now();
        let families = mono_families();
        let scan = t0.elapsed();
        assert!(families == BASE || families.strip_prefix(&format!("{BASE}, ")).is_some());
        if let Some(extra) = families.strip_prefix(&format!("{BASE}, ")) {
            for name in extra.split(", ") {
                assert!(
                    !name.is_empty() && !name.contains(','),
                    "bad family name: {name}"
                );
            }
        }
        println!("mono_families = {families}\nscan took {scan:.1?}");
    }
}
