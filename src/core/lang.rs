//! Single source of truth for tree-sitter language keys: fence info tags
//! (`ui::render`) and file extensions (`lang_from_path`) canonicalize
//! through one table, so both paths always agree. Keys are the primary
//! `ratatui-markdown::get_lang` names (which also accepts most of these
//! aliases natively); unknown tokens return `""` and callers keep the dim
//! fallback instead of guessing.

/// Canonical tree-sitter key for a lowercase fence tag or extension.
/// Unknown tokens return `""`.
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
        "csharp" | "cs" | "c#" => "csharp",
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
        _ => "",
    }
}

/// Tree-sitter language for a file path, covering the grammars compiled in
/// via Cargo features plus common aliases. Unknown extensions return `""`
/// and callers keep the dim fallback (never guess).
pub(crate) fn lang_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = ext.split(':').next().unwrap_or(&ext);
    canonical_lang(ext)
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
            ("a.kt", "kt"),
            ("a.rb", "rb"),
        ] {
            let from_path = lang_from_path(path);
            assert!(!from_path.is_empty(), "{path}");
            assert_eq!(from_path, canonical_lang(tag), "{path} vs ```{tag}");
        }
        assert_eq!(canonical_lang("dockerfile"), "");
        assert_eq!(canonical_lang("text"), "");
    }
}
