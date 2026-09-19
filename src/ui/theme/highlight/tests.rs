//! Tests for syntax highlighting.
#![cfg(test)]

use super::*;

#[test]
fn lang_map_covers_compiled_grammars() {
    assert_eq!(lang_from_path("src/main.rs"), "rust");
    assert_eq!(lang_from_path("a.py:12-20"), "python");
    assert_eq!(lang_from_path("*.tsx"), "typescript");
    assert_eq!(lang_from_path("run.SH"), "bash");
    assert_eq!(lang_from_path("2 files"), "");
    assert_eq!(lang_from_path("Makefile"), "");
    // New grammars enabled alongside the highlighter features.
    assert_eq!(lang_from_path("a.cs"), "csharp");
    assert_eq!(lang_from_path("a.dart"), "dart");
    assert_eq!(lang_from_path("a.kt"), "kotlin");
    assert_eq!(lang_from_path("a.lua"), "lua");
    assert_eq!(lang_from_path("a.nix"), "nix");
    assert_eq!(lang_from_path("a.php"), "php");
    assert_eq!(lang_from_path("a.rb"), "ruby");
    assert_eq!(lang_from_path("a.scala"), "scala");
    assert_eq!(lang_from_path("a.swift"), "swift");
    // Compiled grammars (tree-sitter), including sql/html/css/xml.
    assert_eq!(lang_from_path("q.sql"), "sql");
    assert_eq!(lang_from_path("Dockerfile"), "dockerfile");
    assert_eq!(lang_from_path("Containerfile"), "dockerfile");
    assert_eq!(lang_from_path("a.sql:12-20"), "sql");
    assert_eq!(lang_from_path("index.html"), "html");
    assert_eq!(lang_from_path("a.xml"), "xml");
    assert_eq!(lang_from_path("a.css"), "css");
    // Dotted directories never leak into the extension.
    assert_eq!(lang_from_path("my.dir/file"), "");
    assert_eq!(lang_from_path("my.dir/a.rs"), "rust");
    assert_eq!(lang_from_path("my.dir/Dockerfile"), "dockerfile");
}

#[test]
fn fence_tags_and_paths_agree() {
    // Every path extension must canonicalize exactly like the equivalent
    // fence tag, so TUI and preview highlighting never disagree.
    for (path, tag) in [
        ("a.rs", "rs"),
        ("a.py", "py"),
        ("a.js", "mjs"),
        ("a.ts", "mts"),
        ("a.tsx", "tsx"),
        ("a.h", "h"),
        ("a.hpp", "hpp"),
        ("a.json", "jsonc"),
        ("a.yaml", "yml"),
        ("run.sh", "console"),
        ("run.sh", "sh-session"),
        ("a.cs", "c#"),
        ("a.cs", "c-sharp"),
        ("a.kt", "kt"),
        ("a.rb", "rb"),
    ] {
        let from_path = lang_from_path(path);
        assert!(!from_path.is_empty(), "{path}");
        assert_eq!(from_path, canonical_lang(tag), "{path} vs ```{tag}");
    }
    assert_eq!(canonical_lang("dockerfile"), "dockerfile");
    assert_eq!(canonical_lang("sql"), "sql");
    assert_eq!(canonical_lang("xml"), "xml");
    assert_eq!(canonical_lang("html"), "html");
    assert_eq!(canonical_lang("text"), "");
}

#[test]
fn fence_aliases_canonicalize_to_compiled_grammars() {
    // Shell/file-extension aliases must hit the compiled grammars instead
    // of the dim fallback; shared by the TUI renderer and the headless
    // `print_code_block`, so both paths agree on every fence.
    assert_eq!(normalize_code_lang("c-sharp"), "csharp");
    assert_eq!(normalize_code_lang("mjs"), "javascript");
    assert_eq!(normalize_code_lang("jsx"), "javascript");
    assert_eq!(normalize_code_lang("mts"), "typescript");
    assert_eq!(normalize_code_lang("cts"), "typescript");
    assert_eq!(normalize_code_lang("jsonc"), "json");
    assert_eq!(normalize_code_lang("pyw"), "python");
    assert_eq!(normalize_code_lang("hpp"), "cpp");
    assert_eq!(normalize_code_lang("console"), "bash");
    assert_eq!(normalize_code_lang("terminal"), "bash");
    assert_eq!(normalize_code_lang("rs"), "rust");
    // Params and legacy colons still strip.
    assert_eq!(normalize_code_lang("rust ignore"), "rust");
    assert_eq!(normalize_code_lang("bash:"), "bash");
}

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
    // Keyword (`fn`) and comment (italic) carry SGR runs; the keyword
    // is colored but never bold (bold is markdown emphasis, not chrome).
    assert!(out.contains("\x1b["), "no SGR emitted: {out:?}");
    // `style_to_sgr` joins codes (italic first, then fg), so a run
    // starting with `3;` is exactly italic+fg — a bare `\x1b[3` would
    // also match plain fg runs (`\x1b[35m`), which say nothing about
    // italics.
    assert!(out.contains("\x1b[3;"), "comment not italic: {out:?}");
    let kw = out.split("fn").next().expect("keyword must survive");
    assert!(
        kw.rsplit("\x1b[")
            .next()
            .is_some_and(|sgr| !sgr.is_empty() && !sgr.starts_with('1')),
        "keyword bold or unstyled: {out:?}"
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
        ("html", "<div class=\"a\">hi</div>\n"),
        ("css", ".a { color: red; }\n"),
        ("xml", "<note><to>a</to></note>\n"),
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

#[test]
fn fallback_covers_dockerfile_and_uncompiled() {
    // Tree-sitter misses these (sql has no usable grammar per Cargo.toml,
    // dockerfile has none, kotlin's upstream query panics so it stays
    // disabled, groovy has none); the generic lexer must color them.
    // html/css/xml now have real grammars (see
    // highlight_covers_new_grammars) and don't need it.
    for (lang, code) in [
        ("dockerfile", "FROM rust:1 AS b\nRUN cargo build\n"),
        ("sql", "SELECT a FROM t WHERE x = 1 -- hi\n"),
        ("sql", "select a from t where x = 'it''s'\n"),
        ("groovy", "def x = 42 // hi\n"),
        ("kotlin", "fun main() { val x = 1 }\n"),
    ] {
        let out = fallback_highlight_ansi(lang, code).expect("{lang} must fallback");
        assert_eq!(strip_sgr(&out), code, "{lang} lost bytes");
        assert!(out.contains("\x1b["), "{lang} emitted no SGR");
    }
    // sql/dockerfile/kotlin/groovy really are tree-sitter misses.
    for (lang, code) in [
        ("sql", "SELECT a FROM t\n"),
        ("dockerfile", "FROM rust:1 AS b\n"),
        ("groovy", "def x = 1\n"),
        ("kotlin", "fun main() { val x = 1 }\n"),
    ] {
        assert!(
            highlight_ansi(lang, code).is_none(),
            "{lang} unexpectedly tree-sitter"
        );
    }
}

#[test]
fn fallback_sql_keywords_case_insensitive() {
    let lower = fallback_highlight_ansi("sql", "select a from t\n").expect("lower");
    let upper = fallback_highlight_ansi("sql", "SELECT a FROM t\n").expect("upper");
    assert!(lower.contains("\x1b["));
    assert!(upper.contains("\x1b["));
    assert_eq!(strip_sgr(&lower), "select a from t\n");
    assert_eq!(strip_sgr(&upper), "SELECT a FROM t\n");
}

#[test]
fn fallback_keeps_bash_flags_uncommented() {
    // `--` is a comment only in sql/lua: flags alone must not count as a
    // comment (no false color), while a real `#` comment still colors.
    assert!(fallback_highlight_ansi("bash", "grep -- -x foo\n").is_none());
    let code = "grep -- -x foo # hi\n";
    let out = fallback_highlight_ansi("bash", code).expect("bash comment");
    assert_eq!(strip_sgr(&out), code);
    let sql = fallback_highlight_ansi("sql", "select 1 -- hi\n").expect("sql");
    assert_eq!(strip_sgr(&sql), "select 1 -- hi\n");
    assert!(sql.contains("\x1b["));
}

#[test]
fn fallback_block_comment_spans_lines_and_stays_byte_safe() {
    let code = "/* multi\nline 日本語 */\nlet x = 1\n";
    let out = fallback_highlight_ansi("rust", code).expect("block comment");
    assert_eq!(strip_sgr(&out), code);
    assert!(out.contains("\x1b["));
    let rows = fallback_code_block("rust", code).expect("spans");
    assert_eq!(rows.len(), 4);
    let text: String = rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|s| s.content.as_ref().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("multi"));
    // Unclosed strings/blocks never panic and preserve bytes.
    let evil = "let s = \"unclosed\n/* unclosed\n";
    let out = fallback_highlight_ansi("rust", evil).expect("unclosed");
    assert_eq!(strip_sgr(&out), evil);
    let uni = "let x = \"日本語\" # コメント\n";
    let out = fallback_highlight_ansi("python", uni).expect("unicode");
    assert_eq!(strip_sgr(&out), uni);
}

#[test]
fn fallback_plain_text_stays_dim() {
    assert!(fallback_highlight_ansi("text", "select 1\n").is_none());
    assert!(fallback_highlight_ansi("plain", "select 1\n").is_none());
    assert!(fallback_highlight_ansi("", "select 1\n").is_none());
    assert!(fallback_highlight_ansi("sql", "").is_none());
    // No colorable tokens: no keywords/numbers/strings/comments.
    assert!(fallback_highlight_ansi("groovy", "plain\n").is_none());
    assert!(fallback_code_block("text", "select 1\n").is_none());
    // Truly unknown fence tags stay dim instead of guessing keywords.
    assert!(fallback_highlight_ansi("definitely-not-a-lang", "def x = 42\n").is_none());
    assert!(fallback_highlight_ansi("definitely-not-a-lang", "plain 42\n").is_none());
    assert!(fallback_code_block("definitely-not-a-lang", "def x = 42\n").is_none());
    // Raw aliases still resolve through the canonical table.
    let out = fallback_highlight_ansi("py", "def foo:\n    pass\n").expect("py alias");
    assert!(out.contains("\x1b["));
}

#[test]
fn fallback_css_has_no_slash_comments() {
    // CSS only has `/* */`: a `//` line must not count as a comment.
    assert!(fallback_highlight_ansi("css", "// hi\n").is_none());
    let out = fallback_highlight_ansi("css", "/* hi */\n").expect("block comment");
    assert!(out.contains("\x1b["));
    assert_eq!(strip_sgr(&out), "/* hi */\n");
}

#[test]
fn highlight_pipeline_falls_back_for_dockerfile() {
    // The two-stage wiring `ui::render::highlight_code_block`
    // implements: a tree-sitter hit passes through, a miss (dockerfile
    // has no usable grammar) falls back to the generic lexer, and truly
    // unknown tags stay empty (dim).
    assert!(highlight_ansi("rust", "fn main() {}\n").is_some());
    assert!(highlight_ansi("dockerfile", "FROM rust:1\n").is_none());
    assert!(fallback_code_block("dockerfile", "FROM rust:1\n").is_some());
    assert!(fallback_code_block("definitely-not-a-lang", "def x = 42\n").is_none());
    assert!(fallback_code_block("text", "select 1\n").is_none());
}
