//! Pure fence-tag / file-path language mapping and the tone-tagged generic
//! fallback lexer. No terminal dependency: both the TUI highlighter
//! (`theme::highlight`) and the headless ANSI printer consume these and
//! resolve [`Tone`] to their own color representation.

/// Semantic token class emitted by the fallback lexer; resolved to a terminal
/// color by the consumer (TUI spans or headless SGR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Keyword,
    String,
    Number,
    Comment,
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
pub fn lang_from_path(path: &str) -> &'static str {
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
pub fn normalize_code_lang(info: &str) -> String {
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
pub(crate) fn fallback_enabled(lang: &str) -> bool {
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
fn close_comment(segs: &mut Vec<(usize, usize, Tone)>, start: usize, end: usize, tone: Tone) {
    segs.push((start, end, tone));
}

/// Fallback lexer: keyword / string / number / comment runs as
/// `(start, end, tone)` triples over the source. Byte- and char-boundary-safe.
/// Colors are resolved once at the terminal edge (`theme::highlight` or the
/// headless printer) so TUI and console never drift.
pub(crate) fn fallback_segments(lang: &str, code: &str) -> Vec<(usize, usize, Tone)> {
    if !fallback_enabled(lang) {
        return Vec::new();
    }
    let lang = fallback_key(lang);
    // Color only: bold/italic are reserved for authored emphasis, not code
    // chrome — unstyled keyword weight keeps fences from filling the screen.
    let kw = Tone::Keyword;
    let string = Tone::String;
    let number = Tone::Number;
    let comment = Tone::Comment;
    let (hash, slash, dash, block) = comment_kinds(lang);
    let html = matches!(lang, "html" | "xml");
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut segs: Vec<(usize, usize, Tone)> = Vec::new();
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
                segs.push((pos, end.min(len), string));
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
            segs.push((pos, end, number));
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
                segs.push((pos, end, kw));
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
