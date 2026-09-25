//! TUI highlight adapter: the shared tree-sitter engine, style resolution
//! into `ratatui` `Span`s, and the headless ANSI printers used by the console
//! stream printer. Tone colors come from [`super::palette`]; the pure
//! language map and fallback lexer live in [`super::lang`].

use std::sync::{Arc, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui_markdown::highlight::{CodeHighlighter, StyleSegment, TreeSitterHighlighter};
use ratatui_markdown::CodeColors;

use super::lang::{fallback_enabled, fallback_segments, normalize_code_lang, Tone};
use super::palette::{fg_rgb, muted_rgb};
use crate::runtime::console::RESET;

const DIM: &str = "\x1b[2m";

impl Tone {
    /// Terminal color for a tone: content hues stay ANSI (the terminal
    /// remaps them per theme); comments stay muted.
    pub fn color(self) -> Color {
        match self {
            Tone::Keyword => Color::Magenta,
            Tone::String => Color::Green,
            Tone::Number => Color::Yellow,
            Tone::Comment => match muted_rgb() {
                Some((r, g, b)) => Color::Rgb(r, g, b),
                None => Color::DarkGray,
            },
        }
    }
}

pub fn print_code_block(lang: &str, body: &str) {
    let tag = normalize_code_lang(lang);
    if let Some(colored) =
        highlight_ansi(&tag, body).or_else(|| fallback_highlight_ansi(&tag, body))
    {
        print!("{colored}");
    } else {
        print!("{DIM}{body}{RESET}");
    }
}

/// Theme-aware code palette shared by the TUI (`ui::render`) and headless
/// renderers: prose neutrals follow the terminal's real foreground (readable
/// on light + tinted themes, where fixed `White`/`DarkGray` slots vanish or
/// clash); semantic hues stay ANSI so the terminal remaps them.
pub fn code_colors() -> CodeColors {
    let variable = match fg_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Reset,
    };
    let muted = match muted_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Gray,
    };
    CodeColors::builder()
        .variable(variable)
        .comment(muted)
        .punctuation(muted)
        .build()
}

/// The single tree-sitter parser for the process, shared by the TUI spans
/// adapter (`ui::render::highlight_code_block`, which also serves the
/// markdown fence renderer) and the headless ANSI adapter — one palette,
/// one grammar set, no drift.
pub fn shared_highlighter() -> Arc<TreeSitterHighlighter> {
    static HIGHLIGHTER: OnceLock<Arc<TreeSitterHighlighter>> = OnceLock::new();
    HIGHLIGHTER
        .get_or_init(|| Arc::new(TreeSitterHighlighter::new().with_code_colors(code_colors())))
        .clone()
}

/// Shared highlighter segments, sorted and stripped of `BOLD`: the crate's
/// default theme bolds keywords, which fills code-heavy screens — code
/// tokens keep color only (bold is markdown emphasis, not chrome).
pub fn highlight_segments(lang: &str, code: &str) -> Vec<StyleSegment> {
    let mut segs = shared_highlighter().highlight(lang, code);
    segs.sort_by_key(|s| (s.start, s.end));
    for seg in &mut segs {
        seg.style = seg.style.remove_modifier(Modifier::BOLD);
    }
    segs
}

/// Highlight a snippet to an ANSI-escaped string in ONE tree-sitter pass.
/// Returns `None` when the language is unknown or yields nothing — callers
/// try the generic lexer next, then dim.
pub fn highlight_ansi(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    let segs = highlight_segments(lang, code);
    if segs.is_empty() {
        return None;
    }
    Some(ansi_from_segments(code, &segs))
}

/// Generic fallback lexer for tree-sitter misses (`sql`, `dockerfile`,
/// kotlin — whose upstream query panics — groovy, and any compiled language
/// whose grammar yields nothing): single-pass, dependency-free, byte-safe. Emits
/// keyword / string / number / comment runs using the same palette as
/// tree-sitter so headless and TUI output agree. `text`/`plain` and truly
/// unknown languages stay `None` (dim) instead of guessing.
pub fn fallback_highlight_ansi(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    if !fallback_enabled(lang) {
        return None;
    }
    let segs = tone_segments(fallback_segments(lang, code));
    if segs.is_empty() {
        return None;
    }
    Some(ansi_from_segments(code, &segs))
}

/// One ANSI pass for tree-sitter and fallback segments alike. Byte-safe: a
/// bad range falls back to plain rather than panicking, and segments are
/// clipped to `pos` so overlaps never duplicate bytes.
fn ansi_from_segments(code: &str, segs: &[StyleSegment]) -> String {
    let mut out = String::with_capacity(code.len() + code.len() / 4);
    let mut pos = 0;
    for seg in segs {
        let start = seg.start.min(code.len()).max(pos);
        let end = seg.end.min(code.len());
        if start > pos {
            out.push_str(code.get(pos..start).unwrap_or(""));
        }
        if end > start {
            if let Some(slice) = code.get(start..end) {
                push_styled(&mut out, &seg.style, slice);
            }
            pos = pos.max(end);
        }
    }
    if pos < code.len() {
        out.push_str(code.get(pos..).unwrap_or(""));
    }
    out
}

/// TUI twin of [`fallback_highlight_ansi`]: the same segments split into
/// per-line spans so multi-line block comments keep their style across rows.
/// Returns `None` when nothing is colorable (callers keep dim).
pub fn fallback_code_block(lang: &str, code: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    if !fallback_enabled(lang) {
        return None;
    }
    let segs = tone_segments(fallback_segments(lang, code));
    if segs.is_empty() {
        return None;
    }
    code_block_spans(code, &segs)
}

/// Split `segs` into per-line spans so multi-line constructs (block
/// comments, triple-quoted strings) keep their style across rows. Shared by
/// the tree-sitter path (`ui::render::highlight_code_block`) and the
/// fallback above. Returns `None` on a bad split so callers keep dim rather
/// than rendering half-highlighted rows.
pub fn code_block_spans(code: &str, segs: &[StyleSegment]) -> Option<Vec<Vec<Span<'static>>>> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for line in code.split('\n') {
        ranges.push((start, start + line.len()));
        start += line.len() + 1;
    }
    let mut out: Vec<Vec<Span<'static>>> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        let mut spans = Vec::new();
        let mut pos = lo;
        for seg in segs.iter() {
            if seg.end <= lo || seg.start >= hi || seg.end <= seg.start {
                continue;
            }
            let s = seg.start.max(lo);
            let e = seg.end.min(hi);
            if s < pos {
                continue;
            }
            let gap = code.get(pos..s)?;
            if !gap.is_empty() {
                spans.push(Span::raw(gap.to_string()));
            }
            let text = code.get(s..e)?;
            if text.is_empty() {
                continue;
            }
            spans.push(Span::styled(text.to_string(), seg.style));
            pos = e;
        }
        let tail = code.get(pos..hi)?;
        if !tail.is_empty() {
            spans.push(Span::raw(tail.to_string()));
        }
        out.push(spans);
    }
    Some(out)
}

/// Convert fallback `(start, end, tone)` triples into `StyleSegment`s with
/// the resolved tone colors — the single Tone→Style mapping.
fn tone_segments(triples: Vec<(usize, usize, Tone)>) -> Vec<StyleSegment> {
    triples
        .into_iter()
        .map(|(start, end, tone)| StyleSegment {
            start,
            end,
            style: Style::default().fg(tone.color()),
        })
        .collect()
}

/// Append `text` wrapped in its style's SGR codes; unstyled text is appended
/// raw. Each styled run is self-contained (`...RESET`), so runs never bleed
/// into each other across newlines.
fn push_styled(out: &mut String, style: &Style, text: &str) {
    let sgr = style_to_sgr(style);
    if sgr.is_empty() {
        out.push_str(text);
    } else {
        out.push_str(&sgr);
        out.push_str(text);
        out.push_str(RESET);
    }
}

fn style_to_sgr(style: &Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if style.add_modifier.contains(Modifier::BOLD) {
        codes.push("1".to_string());
    }
    if style.add_modifier.contains(Modifier::DIM) {
        codes.push("2".to_string());
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        codes.push("3".to_string());
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        codes.push("4".to_string());
    }
    if let Some(fg) = style.fg.and_then(fg_sgr) {
        codes.push(fg);
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// `ratatui::Color` -> SGR foreground params. `Reset` inherits the terminal
/// default (the whole point of the theme-aware palette); RGB uses truecolor
/// so headless output matches the TUI exactly.
fn fg_sgr(color: Color) -> Option<String> {
    let code = match color {
        Color::Reset => return None,
        Color::Black => "30",
        Color::Red => "31",
        Color::Green => "32",
        Color::Yellow => "33",
        Color::Blue => "34",
        Color::Magenta => "35",
        Color::Cyan => "36",
        Color::Gray => "37",
        Color::DarkGray => "90",
        Color::LightRed => "91",
        Color::LightGreen => "92",
        Color::LightYellow => "93",
        Color::LightBlue => "94",
        Color::LightMagenta => "95",
        Color::LightCyan => "96",
        Color::White => "97",
        Color::Rgb(r, g, b) => return Some(format!("38;2;{r};{g};{b}")),
        Color::Indexed(n) => return Some(format!("38;5;{n}")),
    };
    Some(code.to_string())
}

/// Render one line of assistant markdown prose to the terminal. Replaces the
/// `termimad` dependency: headings (stripped `#`, bold like glow/mdcat),
/// bullets (`-`/`*`/`+` → `•`), ordered (`1. ` kept), tasks (`☐`/`☑` like the
/// TUI), quotes (`│`), rules (`───`), plus inline code/bold/italic/links.
pub fn print_markdown_text(line: &str) {
    println!("{}", render_markdown_line(line));
}

pub fn render_markdown_line(line: &str) -> String {
    use super::markdown as md;
    let trimmed = line.trim_start();
    if let Some(body) = md::heading_text(trimmed) {
        // Strip the `#` markers (glow/mdcat style); H1 gets underline.
        if md::heading_level(trimmed) == 1 {
            return format!("\x1b[1;4m{}\x1b[0m", render_inline(body));
        }
        return format!("\x1b[1m{}\x1b[0m", render_inline(body));
    }
    if md::is_hr(trimmed) {
        return "\x1b[2m───\x1b[0m".to_string();
    }
    if let Some(body) = md::blockquote_text(trimmed) {
        return format!(
            "\x1b[2m│\x1b[0m \x1b[3m{}\x1b[0m",
            render_inline(body.trim_start())
        );
    }
    if let Some((body, checked)) = md::task_text(trimmed) {
        let box_glyph = if checked { "☑" } else { "☐" };
        return format!("\x1b[2m{box_glyph}\x1b[0m {}", render_inline(body));
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or(trimmed.strip_prefix("* "))
        .or(trimmed.strip_prefix("+ "))
    {
        return format!("\x1b[2m•\x1b[0m {}", render_inline(rest));
    }
    if md::is_ordered_item(trimmed) {
        // Keep the number (industry standard); style the body only.
        let digits = trimmed.len()
            - trimmed
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .len();
        let (num, sep_body) = trimmed.split_at(digits);
        let body = sep_body
            .strip_prefix(". ")
            .or(sep_body.strip_prefix(") "))
            .unwrap_or(sep_body);
        let sep = if sep_body.starts_with(". ") {
            ". "
        } else {
            ") "
        };
        return format!("\x1b[2m{num}{sep}\x1b[0m{}", render_inline(body));
    }
    render_inline(trimmed)
}

/// Inline styling: `` `code` ``, `**bold**`, `*italic*`, `[text](url)`.
pub fn render_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 16);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            let end = (i + 1..chars.len())
                .find(|&j| chars[j] == '`')
                .unwrap_or(chars.len());
            out.push_str("\x1b[0;36m");
            out.extend(&chars[i + 1..end]);
            out.push_str("\x1b[0m");
            i = end + 1;
        } else if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_from(&chars, i + 2, "**") {
                out.push_str("\x1b[1m");
                out.extend(&chars[i + 2..end]);
                out.push_str("\x1b[0m");
                i = end + 2;
            } else {
                out.push_str("**");
                i += 2;
            }
        } else if c == '[' {
            // [text](url) -> text (url dimmed); unmatched brackets stay literal.
            if let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == ']') {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = (close + 2..chars.len()).find(|&j| chars[j] == ')') {
                        out.extend(&chars[i + 1..close]);
                        out.push_str("\x1b[2m(");
                        out.extend(&chars[close + 2..end]);
                        out.push_str(")\x1b[0m");
                        i = end + 1;
                        continue;
                    }
                }
            }
            out.push('[');
            i += 1;
        } else if c == '*' {
            if let Some(end) = (i + 1..chars.len()).find(|&j| chars[j] == '*') {
                out.push_str("\x1b[3m");
                out.extend(&chars[i + 1..end]);
                out.push_str("\x1b[0m");
                i = end + 1;
            } else {
                out.push('*');
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn find_from(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i..].starts_with(&pat.chars().collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests;
