//! `@path` mentions: the composer's `@` popup inserts workspace paths into the
//! prompt; at turn start the daemon appends what they point at (file contents,
//! folder listings) so the model doesn't spend a round trip on `read`/`ls`.
//! The journaled prompt stays as typed — only the model-bound message grows.

use serde_json::{Map, Value};

use super::meta::tool_ls;
use super::read::tool_read;
use super::tool_path;

/// Mentions expanded per prompt; extras stay as plain text.
const MAX_MENTIONS: usize = 10;
/// Shared byte budget for every expanded body in one turn.
const MAX_TOTAL_BYTES: usize = 256 * 1024;

/// Paths mentioned as `@path` or `@"path with spaces"`, in order, deduped.
/// An `@` only counts at the start or after whitespace, so emails and
/// `foo@bar` stay text.
pub(crate) fn parse_mentions(prompt: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut prev_ws = true;
    let mut rest = prompt;
    while let Some(c) = rest.chars().next() {
        if c == '@' && prev_ws {
            let after = &rest[1..];
            let (token, used) = match after.strip_prefix('"') {
                Some(quoted) => match quoted.find('"') {
                    Some(end) => (&quoted[..end], end + 2),
                    None => ("", 0),
                },
                None => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    (&after[..end], end)
                }
            };
            if !token.is_empty() && !out.iter().any(|p| p == token) {
                out.push(token.to_string());
            }
            rest = &rest[1 + used..];
            prev_ws = false;
            continue;
        }
        prev_ws = c.is_whitespace();
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Resolve one mention: trailing sentence punctuation is dropped when the
/// literal token doesn't exist (`look at @src/main.rs.`).
fn resolve(token: &str) -> Option<(String, std::path::PathBuf)> {
    let mut candidate = token;
    loop {
        if let Ok(path) = tool_path(candidate) {
            if path.exists() {
                return Some((candidate.to_string(), path));
            }
        }
        match candidate.strip_suffix(|c: char| ",.;:!?)]}'\"".contains(c)) {
            Some(shorter) if !shorter.is_empty() => candidate = shorter,
            _ => return None,
        }
    }
}

/// `prompt` plus a block per existing mentioned path; unchanged when nothing
/// resolves (`@alice` is just text). `used` counts expanded bytes already
/// spent this turn and is advanced by what this call adds.
pub(crate) async fn expand_mentions(prompt: &str, used: &mut usize) -> String {
    let mut blocks: Vec<String> = Vec::new();
    let mut budget = MAX_TOTAL_BYTES.saturating_sub(*used);
    for token in parse_mentions(prompt).iter().take(MAX_MENTIONS) {
        let Some((shown, path)) = resolve(token) else {
            continue;
        };
        // Regular files and folders only: FIFOs and devices (`/dev/zero`) would
        // block or flood the read.
        let is_dir = path.is_dir();
        if !is_dir && !path.is_file() {
            continue;
        }
        if budget == 0 {
            blocks.push("[further mentions skipped: mention budget exhausted; use `read`]".into());
            break;
        }
        let mut args = Map::new();
        args.insert("path".into(), Value::String(shown.clone()));
        let body = if is_dir {
            tool_ls(&args).await
        } else {
            tool_read(&args).await
        };
        let tag = if is_dir { "directory" } else { "file" };
        let mut body = match body {
            Ok(body) => body,
            Err(e) => format!("[could not read: {e}]"),
        };
        if body.len() > budget {
            let mut cut = budget;
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            body.truncate(cut);
            body.push_str("\n[... truncated: mention budget exhausted; use `read` ...]");
        }
        budget = budget.saturating_sub(body.len());
        *used += body.len();
        // Keep file text from closing the wrapper early.
        let body = body
            .replace("</file>", "<\\/file>")
            .replace("</directory>", "<\\/directory>");
        let shown = shown.replace('"', "&quot;");
        blocks.push(format!("<{tag} path=\"{shown}\">\n{body}\n</{tag}>"));
    }
    if blocks.is_empty() {
        return prompt.to_string();
    }
    format!(
        "{prompt}{}{}",
        crate::protocol::MENTION_MARKER,
        blocks.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_whitespace_led_mentions() {
        assert_eq!(
            parse_mentions("see @src/a.rs and @\"my dir/b c.txt\", mail me@x.com @src/a.rs"),
            vec!["src/a.rs", "my dir/b c.txt"]
        );
    }

    #[tokio::test]
    async fn expands_files_and_folders_and_skips_missing() {
        let dir = std::env::temp_dir().join(format!("dex-mentions-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "x").unwrap();
        let (a, sub) = (dir.join("a.txt"), dir.join("sub"));
        let prompt = format!("read @{}. and @{} then @nobody", a.display(), sub.display());
        let out = expand_mentions(&prompt, &mut 0).await;
        assert!(out.starts_with(&prompt));
        assert_eq!(crate::protocol::strip_mention_block(&out), prompt);
        assert!(out.contains("<file path=\""), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("<directory path=\""), "{out}");
        assert!(out.contains("b.txt"), "{out}");
        assert_eq!(
            expand_mentions("no mentions @nobody", &mut 0).await,
            "no mentions @nobody"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
