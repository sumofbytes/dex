//! Shared line-level markdown semantics for every renderer.
//!
//! Single home for the classifiers, gap rule and language map used by the
//! TUI transcript (`ui/render.rs`), the headless console
//! (`core/highlight.rs`, `llm/stream.rs`) and the streaming throttle
//! (`ui.rs`). One definition — previously three duplicated copies drifted
//! (headings kept `#` in one path, `+` bullets and `[X]` tasks parsed in
//! another, table detection loose in a third).
//!
//! Rule basis: CommonMark + markdownlint MD022 (blanks around headings),
//! MD031 (blanks around fences), MD032 (blanks around lists), MD035
//! (hr style) and MD058 (blanks around tables) — the same rule
//! glow/mdcat/opencode/Claude Code follow: exactly one blank line before
//! AND after each heading, fence, list block, table, rule and quote; tight
//! inside lists, tables and quotes; single blanks between paragraphs.
//!
//! Contract: classifiers take a `trim_start`-ed line (callers trim once).

/// Fenced code marker (info string after the backticks is allowed).
pub(crate) fn is_fence(t: &str) -> bool {
    t.starts_with("```")
}

/// ATX heading body for H1-H6 (`# ` … `###### `). The `#` markers never
/// render literally (glow/mdcat style); H4-H6 have no dedicated block and
/// render as H3, like upstream parsers.
pub(crate) fn heading_text(t: &str) -> Option<&str> {
    let hashes = t.len() - t.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    t[hashes..]
        .strip_prefix(' ')
        .or(t[hashes..].strip_prefix('\t'))
}

pub(crate) fn heading_level(t: &str) -> usize {
    t.len() - t.trim_start_matches('#').len()
}

pub(crate) fn is_heading(t: &str) -> bool {
    heading_text(t).is_some()
}

/// Horizontal rule: 3+ of `-`/`*`/`_` (CommonMark / MD035). `***` and `___`
/// are rules, not emphasis, when alone on a line.
pub(crate) fn is_hr(t: &str) -> bool {
    let s: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() < 3 {
        return false;
    }
    let c = s.chars().next().unwrap();
    matches!(c, '-' | '*' | '_') && s.chars().all(|x| x == c)
}

/// Blockquote body for `>` with or without a following space (CommonMark
/// allows `>quote`). A bare `>` is a blank quote line.
pub(crate) fn blockquote_text(t: &str) -> Option<&str> {
    let rest = t.strip_prefix('>')?;
    Some(
        rest.strip_prefix(' ')
            .or(rest.strip_prefix('\t'))
            .unwrap_or(rest),
    )
}

pub(crate) fn is_blockquote(t: &str) -> bool {
    t.starts_with('>')
}

/// Task item body + checked flag for `- [ ]`, `- [x]`/`- [X]` (and the
/// `*`/`+` variants), like GitHub / opencode.
pub(crate) fn task_text(t: &str) -> Option<(&str, bool)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            if let Some(text) = rest.strip_prefix("[ ] ") {
                return Some((text, false));
            }
            if let Some(text) = rest.strip_prefix("[x] ").or(rest.strip_prefix("[X] ")) {
                return Some((text, true));
            }
        }
    }
    None
}

/// Ordered-list marker (`1. `, `12) `) — models often butt these against
/// prose without a blank line. CommonMark allows 1-9 digits.
pub(crate) fn is_ordered_item(t: &str) -> bool {
    let digits = t.len() - t.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if !(1..=9).contains(&digits) {
        return false;
    }
    let rest = &t[digits..];
    rest.starts_with(". ") || rest.starts_with(") ")
}

/// List continuity class: consecutive items of the same class stay tight
/// (blank around the list block, never inside it).
pub(crate) fn list_kind(t: &str) -> u8 {
    if t.starts_with("- ") || t.starts_with("* ") || t.starts_with("+ ") {
        1
    } else if is_ordered_item(t) {
        2
    } else {
        0
    }
}

/// Any block opening: headings, fences, rules, quotes, bullets and tasks.
/// Ordered items and table starts are handled by callers (`is_ordered_item`,
/// table continuity) so tight-inside grouping stays exact.
pub(crate) fn is_block_start(t: &str) -> bool {
    t.starts_with("```")
        || heading_text(t).is_some()
        || is_hr(t)
        || is_blockquote(t)
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || task_text(t).is_some()
}

/// A GFM pipe-table line: `| a | b |`, `a | b | c`, `|---|:---:|`.
pub(crate) fn is_table_line(t: &str) -> bool {
    let t = t.trim();
    if t.is_empty() || !t.contains('|') {
        return false;
    }
    if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
        return true;
    }
    let pipe_count = t.chars().filter(|&c| c == '|').count();
    if pipe_count < 2 {
        return false;
    }
    let non_sep = t
        .chars()
        .filter(|c| !matches!(c, '|' | '-' | ':' | ' '))
        .count();
    if non_sep > 0 {
        return true;
    }
    let sep_chars: Vec<char> = t.chars().filter(|c| !matches!(c, '|' | ' ')).collect();
    !sep_chars.is_empty() && sep_chars.iter().all(|c| *c == '-' || *c == ':')
}

/// A GFM delimiter row: only `|`, `-`, `:` and spaces with at least one dash.
pub(crate) fn is_table_delimiter(t: &str) -> bool {
    let t = t.trim();
    !t.is_empty() && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// A table starts at line `i` when the line is a table line and the next
/// line is a delimiter row (GFM requires the header + separator pair).
pub(crate) fn is_table_start(lines: &[&str], i: usize) -> bool {
    is_table_line(lines[i].trim())
        && lines
            .get(i + 1)
            .is_some_and(|n| is_table_delimiter(n.trim()))
}

/// Trailing air: prose after one of these needs exactly one blank line
/// (MD022/MD032/MD058). Plain prose needs none (a soft break stays joined).
pub(crate) fn block_leaves_air(t: &str, is_table: bool) -> bool {
    is_table
        || is_heading(t)
        || is_hr(t)
        || is_blockquote(t)
        || list_kind(t) > 0
        || task_text(t).is_some()
}

/// Tight-inside continuity: same-list items, consecutive table rows and
/// consecutive quote lines never get air between them.
pub(crate) fn is_continuation(prev: &str, prev_table: bool, curr: &str, curr_table: bool) -> bool {
    if curr_table && prev_table {
        return true;
    }
    let kt = list_kind(curr);
    if kt > 0 && kt == list_kind(prev) {
        return true;
    }
    // Task items share the `-`/`*`/`+` prefix, so `list_kind` already covers
    // `- [ ]` → `- [ ]`; the explicit check keeps `* [x]` → `- [x]` style
    // switches reading as one list.
    if task_text(curr).is_some() && task_text(prev).is_some() {
        return true;
    }
    if is_blockquote(curr) && is_blockquote(prev) {
        return true;
    }
    false
}

/// Convenience for line-at-a-time renderers (headless console): trims once
/// and derives the table flags from the same strict GFM test the TUI uses.
pub(crate) fn is_continuation_lines(prev: &str, curr: &str) -> bool {
    let p = prev.trim_start();
    let c = curr.trim_start();
    is_continuation(p, is_table_line(p), c, is_table_line(c))
}

/// Whether headless `curr` wants a blank line before it: never doubled,
/// never between continuation rows. Callers only invoke outside fences.
/// Table gaps need outer pipes (or a delimiter row): bare `a | b | c`
/// prose stays gapless until a delimiter confirms a real table in the TUI
/// path (`is_table_start`); models emit outer pipes in practice.
pub(crate) fn is_table_gap_line(t: &str) -> bool {
    let t = t.trim();
    if !is_table_line(t) {
        return false;
    }
    t.starts_with('|') || t.ends_with('|') || is_table_delimiter(t)
}

pub(crate) fn needs_gap_before(prev_empty: bool, curr: &str) -> bool {
    if prev_empty || curr.trim().is_empty() {
        return false;
    }
    let t = curr.trim_start();
    is_fence(t)
        || heading_text(t).is_some()
        || is_hr(t)
        || t.starts_with('>')
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("+ ")
        || task_text(t).is_some()
        || is_ordered_item(t)
        || is_table_gap_line(t)
}

/// Insert blank lines where the model butts block-level markdown against
/// surrounding text, so dense output still renders with air between
/// sections. `pending` is the assistant text already buffered for the
/// streaming seam (or empty for whole-message replay). Never inserts inside
/// code fences; existing blank lines are never doubled.
pub(crate) fn normalize_gaps(pending: &str, s: &str) -> String {
    let trimmed = pending.strip_suffix('\n').unwrap_or(pending);
    let mut prev: &str = trimmed.rsplit('\n').next().unwrap_or("").trim_start();
    let mut prev_table = is_table_line(prev);
    let mut air = block_leaves_air(prev, prev_table);
    let mut fenced = trimmed
        .lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count()
        % 2
        == 1;
    let mut out = String::new();
    for line in s.lines() {
        let t = line.trim_start();
        if is_fence(t) {
            if !fenced && !prev.is_empty() {
                out.push('\n');
            }
            fenced = !fenced;
            if !fenced {
                // Closing fence leaves air like any block (MD031).
                air = true;
                prev_table = false;
                // Point `prev` at something non-empty/block so the next
                // prose line sees the air. `t` borrows `line` (ends this
                // iteration), so use a static fence marker instead.
                prev = "```";
            }
        } else if fenced {
            out.push_str(line);
            out.push('\n');
            continue;
        } else {
            let curr_table = is_table_line(t);
            if !prev.is_empty()
                && !t.is_empty()
                && !is_continuation(prev, prev_table, t, curr_table)
            {
                let starts_block = is_block_start(t)
                    || is_ordered_item(t)
                    || (is_table_gap_line(t) && !prev_table);
                if air || starts_block {
                    out.push('\n');
                }
            }
            prev = t;
            prev_table = curr_table;
            air = block_leaves_air(t, curr_table);
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifiers_cover_the_industry_surface() {
        assert_eq!(heading_text("# T"), Some("T"));
        assert_eq!(heading_text("###### D"), Some("D"));
        assert!(heading_text("####### T").is_none());
        assert!(heading_text("#NoSpace").is_none());
        assert_eq!(heading_level("### T"), 3);
        assert!(is_hr("---") && is_hr("***") && is_hr("___") && is_hr("- - -"));
        assert!(!is_hr("--") && !is_hr("-*-"));
        assert_eq!(blockquote_text(">q"), Some("q"));
        assert_eq!(blockquote_text("> q"), Some("q"));
        assert_eq!(blockquote_text(">"), Some(""));
        assert!(is_blockquote("> q"));
        assert_eq!(task_text("- [X] d"), Some(("d", true)));
        assert_eq!(task_text("+ [ ] t"), Some(("t", false)));
        assert!(task_text("- [?] x").is_none());
        assert!(is_ordered_item("1. a") && is_ordered_item("12) b"));
        assert!(is_ordered_item("1234. a"));
        assert!(!is_ordered_item("1234567890. a"));
        assert!(!is_ordered_item("a. b"));
        assert_eq!(list_kind("+ a"), 1);
        assert_eq!(list_kind("2. a"), 2);
        assert!(is_block_start("+ a") && is_block_start("> q") && is_block_start("***"));
        assert!(is_fence("```rust"));
    }

    #[test]
    fn gaps_match_markdownlint_blanks_around_blocks() {
        assert_eq!(
            normalize_gaps("", "text\n## H\n- a\n- b\n1. x"),
            "text\n\n## H\n\n- a\n- b\n\n1. x\n"
        );
        assert_eq!(
            normalize_gaps("", "text\n***\nmore"),
            "text\n\n***\n\nmore\n"
        );
        assert_eq!(
            normalize_gaps("", "text\n> q\nmore"),
            "text\n\n> q\n\nmore\n"
        );
        assert_eq!(normalize_gaps("", "> a\n> b\n"), "> a\n> b\n");
        assert_eq!(
            normalize_gaps("", "text\n+ a\n+ b\nmore"),
            "text\n\n+ a\n+ b\n\nmore\n"
        );
        assert_eq!(normalize_gaps("", "- [X] d\nmore"), "- [X] d\n\nmore\n");
        // Existing blanks are never doubled; fences are untouched inside.
        assert_eq!(
            normalize_gaps("", "## H\n\n- a\n\ntail"),
            "## H\n\n- a\n\ntail\n"
        );
        assert_eq!(
            normalize_gaps("", "```rust\n# not a heading\n```\ntext"),
            "```rust\n# not a heading\n```\n\ntext\n"
        );
        // Streaming seams: heading/list/table at the window edge.
        assert_eq!(normalize_gaps("## H\n", "- a"), "\n- a\n");
        assert_eq!(
            normalize_gaps("| a | b |\n|---|---|\n", "| 1 | 2 |"),
            "| 1 | 2 |\n"
        );
        assert_eq!(
            normalize_gaps("```rust\nlet x = 1;\n", "# done"),
            "# done\n"
        );
    }

    #[test]
    fn table_gaps_need_outer_pipes() {
        // Bare `a | b | c` shell prose stays gapless; outer-pipe tables gap.
        assert!(!is_table_gap_line("a | b | c"));
        assert!(is_table_gap_line("| a | b |"));
        assert!(!needs_gap_before(false, "a | b | c"));
        assert!(needs_gap_before(false, "| a | b |"));
        assert_eq!(normalize_gaps("", "text\na | b | c"), "text\na | b | c\n");
    }
}
