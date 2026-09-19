use std::sync::{Arc, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui_markdown::highlight::{CodeHighlighter, StyleSegment, TreeSitterHighlighter};
use ratatui_markdown::CodeColors;

use super::palette::{fg_rgb, muted_rgb};
use crate::runtime::console::RESET;

const DIM: &str = "\x1b[2m";

/// Render an assistant message to the terminal as markdown, with fenced
/// code blocks highlighted by the same tree-sitter engine the TUI uses —
/// no external `bat` process, so headless and TUI output always agree.
/// Tree-sitter misses (sql, dockerfile, kotlin/groovy) fall back to the
/// generic lexer below instead of dim.
pub(crate) fn print_code_block(lang: &str, body: &str) {
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
/// adapter (`ui::render::highlight_code_block`, which also serves the
/// markdown fence renderer) and the headless ANSI adapter — one palette,
/// one grammar set, no drift.
pub(crate) fn shared_highlighter() -> Arc<TreeSitterHighlighter> {
    static HIGHLIGHTER: OnceLock<Arc<TreeSitterHighlighter>> = OnceLock::new();
    HIGHLIGHTER
        .get_or_init(|| Arc::new(TreeSitterHighlighter::new().with_code_colors(code_colors())))
        .clone()
}

/// Shared highlighter segments, sorted and stripped of `BOLD`: the crate's
/// default theme bolds keywords, which fills code-heavy screens — code
/// tokens keep color only (bold is markdown emphasis, not chrome).
pub(crate) fn highlight_segments(lang: &str, code: &str) -> Vec<StyleSegment> {
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
pub(crate) fn highlight_ansi(lang: &str, code: &str) -> Option<String> {
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
pub(crate) fn fallback_highlight_ansi(lang: &str, code: &str) -> Option<String> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    if !fallback_enabled(lang) {
        return None;
    }
    let segs = fallback_segments(lang, code);
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
pub(crate) fn fallback_code_block(lang: &str, code: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    if !fallback_enabled(lang) {
        return None;
    }
    let segs = fallback_segments(lang, code);
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
pub(crate) fn code_block_spans(
    code: &str,
    segs: &[StyleSegment],
) -> Option<Vec<Vec<Span<'static>>>> {
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

/// Canonical highlight key for a lowercase fence tag or extension.
/// Tree-sitter grammars cover every key except `sql` (dependency conflict —
/// see Cargo.toml) and `dockerfile` (no upstream grammar), which highlight
/// via the generic lexer. Unknown tokens return `""`.
pub(crate) fn canonical_lang(token: &str) -> &'static str {
    match token {
        "rust" | "rs" => "rust",
        "python" | "py" | "pyw" => "python",
        "javascript" | "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "typescript" | "ts" | "mts" | "cts" | "tsx" => "typescript",
        "go" | "golang" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "c++" | "hpp" | "cc" | "hh" | "cxx" => "cpp",
        "csharp" | "c-sharp" | "cs" | "c#" => "csharp",
        "bash" | "sh" | "shell" | "zsh" | "console" | "terminal" | "sh-session"
        | "shell-session" => "bash",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "dart" => "dart",
        "kotlin" | "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "nix" => "nix",
        "php" => "php",
        "ruby" | "rb" => "ruby",
        "scala" => "scala",
        "swift" => "swift",
        "sql" => "sql",
        "dockerfile" | "docker" | "containerfile" => "dockerfile",
        "html" | "htm" => "html",
        "xml" | "svg" | "xsd" => "xml",
        "css" => "css",
        _ => "",
    }
}

/// Highlight language for a file path, covering compiled grammars plus
/// the fallback-only `dockerfile` key and common aliases. Unknown extensions
/// return `""` and callers keep the dim fallback (never guess). The basename
/// is used so dotted directories (`my.dir/file`) never leak into the
/// extension; extensionless `Dockerfile`/`Containerfile` work because the
/// whole file name is the token.
pub(crate) fn lang_from_path(path: &str) -> &'static str {
    let base = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .split(':')
        .next()
        .unwrap_or(path);
    if base.eq_ignore_ascii_case("dockerfile") || base.eq_ignore_ascii_case("containerfile") {
        return "dockerfile";
    }
    let ext = base.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    canonical_lang(&ext)
}

/// Fence info string -> highlight key (`ratatui-markdown::get_lang` only
/// matches exact lowercase tags). Strips our legacy trailing `:`, drops
/// params (`rust ignore`, `js linenums`), lowercases, then canonicalizes
/// through [`canonical_lang`]. Unknown tags pass through raw so `get_lang`
/// can still match its own native aliases; plain-text tags stay empty (dim).
/// Shared by the TUI renderer (`ui::render::split_markdown`) and the headless
/// `print_code_block`, so both paths agree on every fence.
pub(crate) fn normalize_code_lang(info: &str) -> String {
    let token = info
        .trim()
        .trim_end_matches(':')
        .split([' ', '\t', ',', ';', '{', '}'])
        .next()
        .unwrap_or("")
        .trim_end_matches(':');
    let lower = token.to_ascii_lowercase();
    let canon = canonical_lang(&lower);
    if canon.is_empty() {
        lower
    } else {
        canon.to_string()
    }
}

/// Whether the generic lexer may color `lang`. Every canonical key (compiled
/// or `dockerfile`) plus `groovy` (no upstream grammar, but common enough to
/// deserve keywords) is allowed; `text`/`plain` and truly unknown fence tags
/// stay dim instead of guessing.
fn fallback_enabled(lang: &str) -> bool {
    if lang.is_empty() || matches!(lang, "text" | "plain" | "txt") {
        return false;
    }
    if !canonical_lang(lang).is_empty() {
        return true;
    }
    matches!(lang, "groovy")
}

/// Canonical key for fallback comment/keyword lookup. Callers already pass
/// canonical keys; raw aliases (`py`, `js`, …) map through the same table so
/// they still highlight instead of going dim.
fn fallback_key(lang: &str) -> &str {
    let canon = canonical_lang(lang);
    if canon.is_empty() {
        lang
    } else {
        canon
    }
}

/// End of the line starting at `from`: the next `\n` or EOF. An unclosed
/// comment colors to EOL rather than vanishing.
fn scan_to_eol(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    end
}

/// Record a comment run covering `[start, end)`.
fn close_comment(segs: &mut Vec<StyleSegment>, start: usize, end: usize, style: Style) {
    segs.push(StyleSegment { start, end, style });
}

fn fallback_segments(lang: &str, code: &str) -> Vec<StyleSegment> {
    if !fallback_enabled(lang) {
        return Vec::new();
    }
    let lang = fallback_key(lang);
    let colors = code_colors();
    // Color only: bold is reserved for authored/structural emphasis
    // (headings, `**bold**`), not code chrome — unstyled keyword weight
    // keeps fences from filling the screen.
    let kw = Style::default().fg(colors.keyword);
    let string = Style::default().fg(colors.string);
    let number = Style::default().fg(colors.number);
    let comment = Style::default()
        .fg(colors.comment)
        .add_modifier(Modifier::ITALIC);
    let (hash, slash, dash, block) = comment_kinds(lang);
    let html = matches!(lang, "html" | "xml");
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut segs: Vec<StyleSegment> = Vec::new();
    let mut pos = 0;
    let mut block_start: Option<usize> = None;
    let mut html_start: Option<usize> = None;
    // Advance by one char (not one byte) so `pos` stays on a char boundary.
    let char_len_at = |p: usize| code[p..].chars().next().map_or(1, |c| c.len_utf8());
    while pos < len {
        if let Some(s) = block_start {
            if pos + 1 < len && bytes[pos] == b'*' && bytes[pos + 1] == b'/' {
                pos += 2;
                close_comment(&mut segs, s, pos, comment);
                block_start = None;
            } else {
                pos += char_len_at(pos);
                if pos >= len {
                    close_comment(&mut segs, s, len, comment);
                }
            }
            continue;
        }
        if let Some(s) = html_start {
            if pos + 2 < len
                && bytes[pos] == b'-'
                && bytes[pos + 1] == b'-'
                && bytes[pos + 2] == b'>'
            {
                pos += 3;
                close_comment(&mut segs, s, pos, comment);
                html_start = None;
            } else {
                pos += char_len_at(pos);
                if pos >= len {
                    close_comment(&mut segs, s, len, comment);
                }
            }
            continue;
        }
        let b = bytes[pos];
        if block && pos + 1 < len && b == b'/' && bytes[pos + 1] == b'*' {
            block_start = Some(pos);
            pos += 2;
            continue;
        }
        if html
            && pos + 3 < len
            && b == b'<'
            && bytes[pos + 1] == b'!'
            && bytes[pos + 2] == b'-'
            && bytes[pos + 3] == b'-'
        {
            html_start = Some(pos);
            pos += 4;
            continue;
        }
        if slash && pos + 1 < len && b == b'/' && bytes[pos + 1] == b'/' {
            let end = scan_to_eol(bytes, pos + 2);
            close_comment(&mut segs, pos, end, comment);
            pos = end;
            continue;
        }
        if dash && pos + 1 < len && b == b'-' && bytes[pos + 1] == b'-' {
            let end = scan_to_eol(bytes, pos + 2);
            close_comment(&mut segs, pos, end, comment);
            pos = end;
            continue;
        }
        if hash && b == b'#' {
            let end = scan_to_eol(bytes, pos + 1);
            close_comment(&mut segs, pos, end, comment);
            pos = end;
            continue;
        }
        if b == b'"' || b == b'\'' || b == b'`' {
            let quote = b;
            let mut end = pos + 1;
            let mut closed = false;
            while end < len {
                let eb = bytes[end];
                if eb == b'\n' && quote != b'`' {
                    break;
                }
                if eb == b'\\' {
                    end += 1;
                    if end < len {
                        end += char_len_at(end);
                    }
                    continue;
                }
                if lang == "sql"
                    && quote == b'\''
                    && eb == b'\''
                    && end + 1 < len
                    && bytes[end + 1] == b'\''
                {
                    end += 2;
                    continue;
                }
                if eb == quote {
                    end += 1;
                    closed = true;
                    break;
                }
                end += if eb < 0x80 { 1 } else { char_len_at(end) };
            }
            // Unclosed strings still color to EOL/EOF rather than vanishing.
            if end > pos + 1 || closed {
                segs.push(StyleSegment {
                    start: pos,
                    end: end.min(len),
                    style: string,
                });
            }
            pos = end.min(len).max(pos + 1);
            continue;
        }
        if b.is_ascii_digit() {
            let mut end = pos + 1;
            while end < len
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'.' || bytes[end] == b'_')
            {
                end += 1;
            }
            segs.push(StyleSegment {
                start: pos,
                end,
                style: number,
            });
            pos = end;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let mut end = pos + 1;
            while end < len && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let is_kw = code
                .get(pos..end)
                .is_some_and(|w| fallback_is_keyword(lang, w));
            if is_kw {
                segs.push(StyleSegment {
                    start: pos,
                    end,
                    style: kw,
                });
            }
            pos = end;
            continue;
        }
        pos += if b < 0x80 { 1 } else { char_len_at(pos) };
    }
    segs
}

/// Which comment markers the generic lexer honors, as
/// `(hash, slash, dash, block)`. One table so `//`/`--`/`#` rules can't drift.
/// CSS has no `//` comments — only `/* */` (covered by the block rule).
/// `--` is a comment only in SQL/Lua; bash `grep -- -x` must not comment.
fn comment_kinds(lang: &str) -> (bool, bool, bool, bool) {
    let hash = matches!(
        lang,
        "python" | "ruby" | "bash" | "yaml" | "toml" | "nix" | "dockerfile" | "php"
    );
    let slash = matches!(
        lang,
        "rust"
            | "javascript"
            | "typescript"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "csharp"
            | "dart"
            | "kotlin"
            | "scala"
            | "swift"
            | "php"
    );
    let dash = matches!(lang, "sql" | "lua");
    let block = matches!(
        lang,
        "rust"
            | "javascript"
            | "typescript"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "csharp"
            | "dart"
            | "kotlin"
            | "scala"
            | "swift"
            | "php"
            | "css"
            | "sql"
    );
    (hash, slash, dash, block)
}

fn fallback_is_keyword(lang: &str, word: &str) -> bool {
    match lang {
        "rust" => matches!(
            word,
            "as" | "break"
                | "const"
                | "continue"
                | "crate"
                | "else"
                | "enum"
                | "extern"
                | "false"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "true"
                | "type"
                | "unsafe"
                | "use"
                | "where"
                | "while"
                | "async"
                | "await"
                | "dyn"
        ),
        "python" | "ruby" => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "do"
                | "elif"
                | "else"
                | "elsif"
                | "end"
                | "except"
                | "False"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "module"
                | "next"
                | "nil"
                | "None"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "redo"
                | "rescue"
                | "retry"
                | "return"
                | "self"
                | "super"
                | "True"
                | "try"
                | "unless"
                | "until"
                | "while"
                | "with"
                | "yield"
        ),
        "javascript" | "typescript" => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "delete"
                | "else"
                | "export"
                | "extends"
                | "false"
                | "finally"
                | "for"
                | "function"
                | "if"
                | "import"
                | "in"
                | "let"
                | "new"
                | "null"
                | "of"
                | "return"
                | "static"
                | "super"
                | "this"
                | "throw"
                | "true"
                | "try"
                | "typeof"
                | "var"
                | "while"
                | "with"
                | "yield"
        ),
        "go" => matches!(
            word,
            "break"
                | "case"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        "bash" => {
            matches!(
                word,
                "if" | "then"
                    | "else"
                    | "elif"
                    | "fi"
                    | "for"
                    | "while"
                    | "until"
                    | "in"
                    | "do"
                    | "done"
                    | "case"
                    | "esac"
                    | "function"
                    | "select"
                    | "time"
                    | "coproc"
                    | "echo"
                    | "cd"
                    | "pwd"
                    | "export"
                    | "local"
                    | "readonly"
                    | "declare"
                    | "typeset"
                    | "unset"
                    | "alias"
                    | "unalias"
                    | "source"
                    | "exec"
                    | "exit"
                    | "return"
                    | "trap"
                    | "shift"
                    | "true"
                    | "false"
                    | "test"
            )
        }
        "sql" => matches!(
            word.to_ascii_lowercase().as_str(),
            "select"
                | "from"
                | "where"
                | "join"
                | "on"
                | "group"
                | "by"
                | "order"
                | "having"
                | "limit"
                | "offset"
                | "insert"
                | "into"
                | "values"
                | "update"
                | "set"
                | "delete"
                | "create"
                | "table"
                | "alter"
                | "drop"
                | "index"
                | "view"
                | "as"
                | "and"
                | "or"
                | "not"
                | "null"
                | "primary"
                | "key"
                | "foreign"
                | "references"
                | "distinct"
                | "count"
                | "sum"
                | "avg"
                | "min"
                | "max"
                | "union"
                | "all"
                | "inner"
                | "outer"
                | "left"
                | "right"
                | "case"
                | "when"
                | "then"
                | "else"
                | "end"
                | "like"
                | "in"
                | "is"
                | "between"
                | "exists"
        ),
        "dockerfile" => matches!(
            word.to_ascii_lowercase().as_str(),
            "from"
                | "run"
                | "cmd"
                | "label"
                | "maintainer"
                | "expose"
                | "env"
                | "add"
                | "copy"
                | "entrypoint"
                | "volume"
                | "user"
                | "workdir"
                | "arg"
                | "onbuild"
                | "stopsignal"
                | "healthcheck"
                | "shell"
        ),
        // Generic keywords for compiled languages without their own table
        // (plus groovy, which has no grammar). Truly unknown languages stay
        // dim instead of guessing.
        "java" | "c" | "cpp" | "csharp" | "dart" | "kotlin" | "lua" | "nix" | "php" | "scala"
        | "swift" | "html" | "xml" | "css" | "json" | "toml" | "yaml" | "groovy" => matches!(
            word,
            "and"
                | "as"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "def"
                | "do"
                | "else"
                | "end"
                | "false"
                | "fn"
                | "for"
                | "fun"
                | "function"
                | "if"
                | "import"
                | "in"
                | "let"
                | "match"
                | "mut"
                | "new"
                | "null"
                | "object"
                | "or"
                | "pub"
                | "return"
                | "self"
                | "static"
                | "struct"
                | "true"
                | "try"
                | "type"
                | "val"
                | "var"
                | "while"
                | "with"
                | "yield"
        ),
        _ => false,
    }
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

mod tests;
