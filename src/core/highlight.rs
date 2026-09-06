use std::sync::{Arc, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui_markdown::highlight::{CodeHighlighter, TreeSitterHighlighter};
use ratatui_markdown::CodeColors;

use super::lang::canonical_lang;
use super::palette::{fg_rgb, muted_rgb};
use crate::core::console::RESET;

const DIM: &str = "\x1b[2m";

/// Render an assistant message to the terminal as markdown, with fenced
/// code blocks highlighted by the same tree-sitter engine the TUI uses —
/// no external `bat` process, so headless and TUI output always agree.
pub(crate) fn print_code_block(lang: &str, body: &str) {
    let lower = lang.to_ascii_lowercase();
    let canon = canonical_lang(&lower);
    let tag = if canon.is_empty() {
        lower.as_str()
    } else {
        canon
    };
    match highlight_ansi(tag, body) {
        Some(colored) => print!("{colored}"),
        // Unknown language: dim fallback like the TUI, never guess.
        None => print!("{DIM}{body}{RESET}"),
    }
}

/// Theme-aware code palette shared by the TUI (`ui::render`) and headless
/// renderers: prose neutrals follow the terminal's real foreground (readable
/// on light + tinted themes, where fixed `White`/`DarkGray` slots vanish or
/// clash); semantic hues stay ANSI so the terminal remaps them.
pub(crate) fn code_colors() -> CodeColors {
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
/// adapter (`ui::render::highlight_code_block`) and the headless ANSI
/// adapter below — one palette, one grammar set, no drift.
pub(crate) fn shared_highlighter() -> Arc<TreeSitterHighlighter> {
    static HIGHLIGHTER: OnceLock<Arc<TreeSitterHighlighter>> = OnceLock::new();
    HIGHLIGHTER
        .get_or_init(|| Arc::new(TreeSitterHighlighter::new().with_code_colors(code_colors())))
        .clone()
}

/// Highlight a snippet to an ANSI-escaped string in ONE tree-sitter pass.
/// Returns `None` when the language is unknown or yields nothing — callers
/// keep the dim fallback, so highlighting never regresses to plain.
pub(crate) fn highlight_ansi(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    let mut segs = shared_highlighter().highlight(lang, code);
    if segs.is_empty() {
        return None;
    }
    segs.sort_by_key(|s| (s.start, s.end));
    let mut out = String::with_capacity(code.len() + code.len() / 4);
    let mut pos = 0;
    for seg in &segs {
        // Byte-safe: a bad range falls back to plain rather than panicking.
        let start = seg.start.min(code.len());
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
    Some(out)
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
pub(crate) fn print_markdown_text(line: &str) {
    println!("{}", render_markdown_line(line));
}

pub(crate) fn render_markdown_line(line: &str) -> String {
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
fn render_inline(text: &str) -> String {
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
mod tests {
    use super::*;

    /// Strip SGR sequences so highlighting tests assert structure, not the
    /// exact palette (which follows the terminal theme).
    fn strip_sgr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn highlight_returns_none_for_unknown_or_empty() {
        assert!(highlight_ansi("groovy", "def x = 1\n").is_none());
        assert!(highlight_ansi("", "let x = 1;\n").is_none());
        assert!(highlight_ansi("rust", "").is_none());
    }

    #[test]
    fn highlight_preserves_source_bytes_and_colors_tokens() {
        let code = "fn main() { let x = 42; // comment\n}\n";
        let out = highlight_ansi("rust", code).expect("rust must highlight");
        // No bytes lost or reordered; styles are annotations only.
        assert_eq!(strip_sgr(&out), code);
        // Keyword (`fn`, bold) and comment (italic) carry SGR runs.
        assert!(out.contains("\x1b["), "no SGR emitted: {out:?}");
        let bold = out.split("fn").next().expect("keyword must survive");
        assert!(
            bold.rsplit("\x1b[")
                .next()
                .is_some_and(|sgr| sgr.starts_with('1')),
            "keyword not bold: {out:?}"
        );
    }

    #[test]
    fn highlight_covers_new_grammars() {
        // Every newly enabled grammar must yield segments, not the fallback.
        for (lang, code) in [
            ("csharp", "class A { void M() {} }\n"),
            ("dart", "void main() { var x = 1; }\n"),
            ("lua", "local x = 1 -- hi\n"),
            ("nix", "{ pkgs }: pkgs.hello\n"),
            ("php", "<?php echo $x; ?>\n"),
            ("ruby", "def foo # hi\nend\n"),
            ("scala", "object A { val x = 1 }\n"),
            ("swift", "func f() { let x = 1 }\n"),
        ] {
            let out = highlight_ansi(lang, code).expect("{lang} must highlight");
            assert_eq!(strip_sgr(&out), code, "{lang} lost bytes");
            assert!(out.contains("\x1b["), "{lang} emitted no SGR");
        }
    }

    #[test]
    fn highlight_kotlin_degrades_gracefully() {
        // `highlight-lang-kotlin` is disabled (see Cargo.toml): the grammar is
        // unknown to the highlighter, so kotlin dims instead of panicking.
        assert!(highlight_ansi("kotlin", "fun main() { val x = 1 }\n").is_none());
    }

    #[test]
    fn bash_highlights_without_commenting_flags() {
        // `--` flags and `$VAR` must survive as source bytes (the old hand
        // lexer special-cased these; tree-sitter parses them properly).
        let code = "grep -- -x foo # comment\necho $HOME\n";
        let out = highlight_ansi("bash", code).expect("bash must highlight");
        assert_eq!(strip_sgr(&out), code);
    }

    #[test]
    fn markdown_render_headings_bullets() {
        // Industry standard (glow/mdcat): `#` markers are stripped, H1 gets
        // underline, H2+ bold; bullets collapse to `•`.
        assert_eq!(render_markdown_line("# Title"), "\x1b[1;4mTitle\x1b[0m");
        assert_eq!(render_markdown_line("## Sub"), "\x1b[1mSub\x1b[0m");
        assert_eq!(render_markdown_line("### Deep"), "\x1b[1mDeep\x1b[0m");
        assert_eq!(render_markdown_line("- item"), "\x1b[2m•\x1b[0m item");
        assert_eq!(render_markdown_line("+ plus"), "\x1b[2m•\x1b[0m plus");
        assert_eq!(
            render_markdown_line("  - indented"),
            "\x1b[2m•\x1b[0m indented"
        );
        assert_eq!(render_markdown_line("1. first"), "\x1b[2m1. \x1b[0mfirst");
        assert_eq!(render_markdown_line("- [x] done"), "\x1b[2m☑\x1b[0m done");
        assert_eq!(render_markdown_line("---"), "\x1b[2m───\x1b[0m");
        assert_eq!(render_markdown_line("***"), "\x1b[2m───\x1b[0m");
        assert!(render_markdown_line("> quote").contains('│'));
        assert_eq!(render_markdown_line("plain text"), "plain text");
    }

    #[test]
    fn markdown_render_inline_styles() {
        assert_eq!(render_inline("a `code` b"), "a \x1b[0;36mcode\x1b[0m b");
        assert_eq!(render_inline("**bold** tail"), "\x1b[1mbold\x1b[0m tail");
        assert_eq!(render_inline("*it* ok"), "\x1b[3mit\x1b[0m ok");
        assert_eq!(
            render_inline("[text](http://x)"),
            "text\x1b[2m(http://x)\x1b[0m"
        );
        // Unclosed delimiters stay literal.
        assert_eq!(render_inline("**unclosed"), "**unclosed");
        assert_eq!(render_inline("[just](bracket"), "[just](bracket");
        assert_eq!(
            render_inline("`unclosed code"),
            "\x1b[0;36munclosed code\x1b[0m"
        );
    }
}
