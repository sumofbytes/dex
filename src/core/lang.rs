//! Single source of truth for highlight language keys: fence info tags
//! (`ui::render`) and file extensions (`lang_from_path`) canonicalize
//! through one table, so both paths always agree. Keys are the primary
//! `ratatui-markdown::get_lang` names (which also accepts most of these
//! aliases natively) plus fallback-only keys (`sql`, `dockerfile`, `html`,
//! `css`) served by the generic lexer in `core::highlight`; unknown tokens
//! return `""` and callers keep the dim fallback instead of guessing.

/// Canonical highlight key for a lowercase fence tag or extension.
/// Tree-sitter grammars cover most keys; `sql`/`dockerfile`/`html`/`css`
/// are fallback-only (no compiled grammar) and highlight via the generic
/// lexer. Unknown tokens return `""`.
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
        "html" | "htm" | "xml" => "html",
        "css" => "css",
        _ => "",
    }
}

/// Highlight language for a file path, covering compiled grammars plus
/// fallback-only keys and common aliases. Unknown extensions return `""`
/// and callers keep the dim fallback (never guess). Extensionless
/// `Dockerfile`/`Containerfile` work because the whole file name is the
/// token.
pub(crate) fn lang_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = ext.split(':').next().unwrap_or(&ext);
    canonical_lang(ext)
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

#[cfg(test)]
mod tests {
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
        // Fallback-only keys served by the generic lexer (no compiled grammar).
        assert_eq!(lang_from_path("q.sql"), "sql");
        assert_eq!(lang_from_path("Dockerfile"), "dockerfile");
        assert_eq!(lang_from_path("a.sql:12-20"), "sql");
        assert_eq!(lang_from_path("index.html"), "html");
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
}
