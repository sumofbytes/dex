use std::fs;
use std::path::Path;

use crate::protocol::Skill;

pub(crate) fn parse_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
    let (name, description) = parse_frontmatter(&content)?;
    if !skill_name_ok(&name, path) {
        return None;
    }
    Some(Skill {
        name,
        description,
        path: path.to_path_buf(),
    })
}

/// Parse a SKILL.md frontmatter block into `(name, description)`; `None`
/// when the file doesn't start with `---` or has no `name:` key. Shared by
/// the sync and async discovery paths so the frontmatter rules can't drift.
pub(crate) fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let mut lines = content.lines();
    let first = lines.next()?;
    if first.trim() != "---" {
        return None;
    }
    let mut name = None;
    let mut description = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(unquote(val.trim()));
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(unquote(val.trim()));
        }
    }
    let name = name?;
    Some((name, description.unwrap_or_default()))
}

/// Name validity shared by both parse paths; an invalid name warns to stderr
/// with the sync path's exact wording (the async path used to drop it) and
/// rejects the skill.
pub(crate) fn skill_name_ok(name: &str, path: &Path) -> bool {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        eprintln!(
            "[skills] ignoring invalid skill name '{}' in {}",
            name,
            path.display()
        );
    }
    ok
}

/// Strip a single layer of surrounding quotes (single or double) from a YAML
/// scalar value, so `description: "Short description"` yields the bare value.
pub(crate) fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    #[test]
    fn unquote_strips_quotes() {
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote("'world'"), "world");
        assert_eq!(unquote("bare"), "bare");
        assert_eq!(unquote("\"a\""), "a"); // trimmed then stripped
        assert_eq!(unquote("\"\""), "");
    }

    #[test]
    fn parse_skill_accepts_valid_frontmatter() {
        let dir = std::env::temp_dir().join(format!("dex-skill-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            "---\nname: my-skill\ndescription: \"Does things\"\n---\nBody",
        )
        .unwrap();
        let skill = parse_skill(&path).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Does things");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_skill_rejects_missing_or_bad_name() {
        let dir = std::env::temp_dir().join(format!("dex-skill-bad-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let bad = dir.join("bad.md");
        fs::write(&bad, "---\nname: bad name with spaces\n---\n").unwrap();
        assert!(parse_skill(&bad).is_none());
        let no_front = dir.join("nofront.md");
        fs::write(&no_front, "no frontmatter").unwrap();
        assert!(parse_skill(&no_front).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
