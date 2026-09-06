use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::core::console::RESET;

/// Render an assistant message to the terminal as markdown, with
/// fenced code blocks highlighted via `bat` when available.
pub(crate) fn print_code_block(lang: &str, body: &str) {
    // Try `bat` first (supports language tags + line numbers + theme).
    let bat = ["bat", "batcat"]
        .iter()
        .find_map(|b| which(b).ok().map(|p| (b.to_string(), p)));
    if let Some((bin, path)) = bat {
        let mut cmd = Command::new(&path);
        cmd.args([
            "--color=always",
            "--style=plain,header=fault",
            "--paging=never",
        ]);
        if !lang.is_empty() {
            cmd.args(["-l", lang]);
        }
        if let Ok(mut child) = cmd.arg("-").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(body.as_bytes());
            }
            drop(child.stdin.take());
            if let Ok(out) = child.wait_with_output() {
                if out.status.success() {
                    print!("{}", String::from_utf8_lossy(&out.stdout));
                    return;
                }
            }
        }
        let _ = bin;
    }
    // Fallback: use a small lexer so terminals without bat still get useful
    // syntax colours (strings/comments are consumed before keywords).
    print_ansi_highlighted_code(lang, body);
}

pub(crate) fn print_ansi_highlighted_code(lang: &str, body: &str) {
    const KEYWORD: &str = "\x1b[1;35m";
    const STRING: &str = "\x1b[0;32m";
    const NUMBER: &str = "\x1b[0;33m";
    const COMMENT: &str = "\x1b[0;90m";
    const PUNCT: &str = "\x1b[0;36m";
    let lang = lang.to_ascii_lowercase();
    let hash_comments = matches!(
        lang.as_str(),
        "python" | "py" | "ruby" | "rb" | "bash" | "sh" | "yaml" | "yml" | "toml" | "perl"
    );
    let keywords = match lang.as_str() {
        "rust" | "rs" => "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn",
        "python" | "py" => "and as assert async await break class continue def del elif else except False finally for from global if import in is lambda None not or pass raise return True try while with yield",
        "javascript" | "js" | "typescript" | "ts" => "as async await break case catch class const continue default delete else export extends false finally for function if import in let new null of return static super this throw true try typeof var while with yield",
        "go" | "golang" => "break case const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var",
        _ => "class const def else false fn for function if import let match mut new null pub return static struct true try type while async await",
    };
    let is_keyword = |word: &str| keywords.split_whitespace().any(|k| k == word);

    for line in body.split_inclusive('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if (c == '/' && i + 1 < chars.len() && chars[i + 1] == '/')
                || (c == '#' && hash_comments)
                || (c == '-' && i + 1 < chars.len() && chars[i + 1] == '-')
            {
                print!(
                    "{}{}{}",
                    COMMENT,
                    chars[i..].iter().collect::<String>(),
                    RESET
                );
                break;
            } else if matches!(c, '\"' | '\'' | '`') {
                let quote = c;
                let start = i;
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\\' {
                        i += 2;
                        continue;
                    }
                    let closed = chars[i] == quote;
                    i += 1;
                    if closed {
                        break;
                    }
                }
                print!(
                    "{}{}{}",
                    STRING,
                    chars[start..i.min(chars.len())].iter().collect::<String>(),
                    RESET
                );
            } else if c.is_ascii_digit() {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '.' | '_'))
                {
                    i += 1;
                }
                print!(
                    "{}{}{}",
                    NUMBER,
                    chars[start..i].iter().collect::<String>(),
                    RESET
                );
            } else if c.is_ascii_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if is_keyword(&word) {
                    print!("{}{}{}", KEYWORD, word, RESET);
                } else {
                    print!("{}", word);
                }
            } else {
                i += 1;
                if "{}[]()<>;:,.=+-*/%!&|?".contains(c) {
                    print!("{}{}{}", PUNCT, c, RESET);
                } else {
                    print!("{}", c);
                }
            }
        }
    }
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

pub(crate) fn which(bin: &str) -> Result<PathBuf, io::Error> {
    let path_var = env::var("PATH").unwrap_or_default();
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", bin),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_existing_binary_and_fails_for_missing() {
        // sh should exist on any unix test image
        assert!(which("sh").is_ok());
        assert!(which("definitely-not-a-real-binary-dex-test-12345").is_err());
    }

    #[test]
    fn highlight_does_not_panic_on_various_langs() {
        // Just ensure no panic; output goes to stdout and is not asserted.
        print_ansi_highlighted_code("rust", "fn main() { let x = 42; // comment\n}");
        print_ansi_highlighted_code("python", "def foo(): # hi\n    return \"str\"\n");
        print_ansi_highlighted_code("unknown", "some plain text 123");
        print_ansi_highlighted_code("", "");
    }

    #[test]
    fn highlight_handles_hash_comments_per_lang() {
        // hash-comments branch for python, plain for rust
        print_ansi_highlighted_code("python", "# comment\nx = 1");
        print_ansi_highlighted_code("rust", "# not a comment in rust\nlet x = 1;");
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
