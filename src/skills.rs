use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::core::types::Skill;

pub(crate) fn parse_skill(path: &Path) -> Option<Skill> {
    let content = fs::read_to_string(path).ok()?;
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
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        eprintln!(
            "[skills] ignoring invalid skill name '{}' in {}",
            name,
            path.display()
        );
        return None;
    }
    Some(Skill {
        name,
        description: description.unwrap_or_default(),
        path: path.to_path_buf(),
    })
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

pub(crate) fn discover_skills(dirs: &[PathBuf]) -> Vec<Skill> {
    // The daemon calls this per chat turn (system prompt) and per
    // `/api/skills`; each call scans 4+ dirs + parses SKILL.md frontmatter.
    // Cache 10s keyed by the dir list — skill edits appear within seconds,
    // and explicit `/skill:<name>` loads bypass via `discover_skills_fresh`.
    static CACHE: OnceLock<Mutex<Option<CachedSkills>>> = OnceLock::new();
    struct CachedSkills {
        key: Vec<PathBuf>,
        at: Instant,
        skills: Vec<Skill>,
    }
    if let Some(hit) = CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|cached| cached.key == dirs && cached.at.elapsed() < Duration::from_secs(10))
    {
        return hit.skills.clone();
    }
    let skills = discover_skills_fresh(dirs);
    CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace(CachedSkills {
            key: dirs.to_vec(),
            at: Instant::now(),
            skills: skills.clone(),
        });
    skills
}

pub(crate) fn discover_skills_fresh(dirs: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() && path.join("SKILL.md").exists() {
                if let Some(skill) = parse_skill(&path.join("SKILL.md")) {
                    if seen.insert(skill.name.clone()) {
                        skills.push(skill);
                    } else {
                        eprintln!(
                            "[skills] ignoring duplicate skill '{}' at {}",
                            skill.name,
                            path.display()
                        );
                    }
                }
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

pub(crate) async fn discover_skills_fresh_async(dirs: &[PathBuf]) -> Vec<Skill> {
    // Async dir scans (`read_dir`, concurrent `SKILL.md` reads): 10s cache
    // lives in `discover_skills`; explicit `/skill` loads bypass it.
    let mut dir_entries: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for dir in dirs {
        let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
            continue;
        };
        let mut subdirs = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            // is_dir via file_type to avoid extra stat; fallback to path check.
            let is_dir = entry
                .file_type()
                .await
                .map(|ft| ft.is_dir())
                .unwrap_or_else(|_| path.is_dir());
            if is_dir {
                subdirs.push(path);
            }
        }
        subdirs.sort();
        dir_entries.push((dir.clone(), subdirs));
    }
    // Concurrent SKILL.md reads under the existing sort + first-wins + dup warning.
    let mut set = tokio::task::JoinSet::new();
    for (_, subdirs) in dir_entries {
        for path in subdirs {
            let skill_path = path.join("SKILL.md");
            // Fast existence check without extra stat storm: attempt read directly.
            set.spawn(async move {
                let content = tokio::fs::read_to_string(&skill_path).await.ok()?;
                // Parse without blocking: frontmatter is tiny, parse inline.
                // Reuse sync parser by writing to temp? Instead parse here (duplicate tiny logic).
                // To reuse helpers and avoid duplication, parse via blocking task for CPU? Frontmatter parse is trivial (<1ms), inline.
                let mut lines = content.lines();
                if lines.next()?.trim() != "---" {
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
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    return None;
                }
                Some(crate::core::types::Skill {
                    name,
                    description: description.unwrap_or_default(),
                    path: skill_path,
                })
            });
        }
    }
    let mut found: Vec<crate::core::types::Skill> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(Some(s)) = r {
            found.push(s);
        }
    }
    // Sort + first-wins dedup to match sync contract (sorted by name, first wins).
    // Since JoinSet completion is nondeterministic, sort by path first for determinism,
    // then by name for output, keeping first path per name.
    found.sort_by(|a, b| a.path.cmp(&b.path));
    let mut seen = std::collections::HashSet::new();
    let mut skills = Vec::new();
    for skill in found {
        if seen.insert(skill.name.clone()) {
            skills.push(skill);
        } else {
            eprintln!(
                "[skills] ignoring duplicate skill '{}' at {}",
                skill.name,
                skill.path.display()
            );
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

#[allow(clippy::type_complexity)]
pub(crate) async fn discover_skills_async(dirs: &[PathBuf]) -> Vec<Skill> {
    // 10s cache keyed by dir list (same as sync); hits are a mutex bump inline,
    // misses run the async scan above (no spawn_blocking needed — dir scans are async).
    static CACHE_ASYNC: std::sync::OnceLock<
        std::sync::Mutex<Option<(Vec<PathBuf>, std::time::Instant, Vec<Skill>)>>,
    > = std::sync::OnceLock::new();
    if let Some(hit) = CACHE_ASYNC
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .filter(|(key, at, _)| key == dirs && at.elapsed() < Duration::from_secs(10))
    {
        return hit.2.clone();
    }
    let skills = discover_skills_fresh_async(dirs).await;
    CACHE_ASYNC
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .replace((dirs.to_vec(), Instant::now(), skills.clone()));
    skills
}

pub(crate) fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Project-level skills
    if let Ok(cwd) = env::current_dir() {
        dirs.push(cwd.join(".dex/skills"));
        dirs.push(cwd.join(".agents/skills"));
    }
    // User-level skills, including the pre-rename `ak` location so existing
    // setups keep working.
    if let Some(cfg) = env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(&cfg).join("dex/skills"));
        dirs.push(PathBuf::from(cfg).join("ak/skills"));
    } else if let Some(home) = env::var_os("HOME") {
        dirs.push(PathBuf::from(&home).join(".config/dex/skills"));
        dirs.push(PathBuf::from(home).join(".config/ak/skills"));
    }
    dirs
}

pub(crate) fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let mut out = String::new();
    out.push_str("\n\nAvailable skills:\n");
    for skill in skills {
        out.push_str(&format!("- {}: {}\n", skill.name, skill.description));
    }
    out.push_str("\nTo use a skill, type /skill:<name> or ask about it.\n");
    out
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

    #[test]
    fn discover_skills_sorts_and_dedups() {
        let base = std::env::temp_dir().join(format!("dex-discover-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        fs::create_dir_all(a.join("skill-a")).unwrap();
        fs::create_dir_all(b.join("skill-a")).unwrap(); // duplicate name
        fs::create_dir_all(b.join("skill-b")).unwrap();
        fs::write(
            a.join("skill-a/SKILL.md"),
            "---\nname: alpha\ndescription: first\n---\n",
        )
        .unwrap();
        fs::write(
            b.join("skill-a/SKILL.md"),
            "---\nname: alpha\ndescription: dup\n---\n",
        )
        .unwrap();
        fs::write(
            b.join("skill-b/SKILL.md"),
            "---\nname: beta\ndescription: second\n---\n",
        )
        .unwrap();
        let skills = discover_skills(&[a, b.clone()]);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[1].name, "beta");
        // first wins on duplicate
        assert_eq!(skills[0].description, "first");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn format_skills_for_prompt_contains_names() {
        let skills = vec![Skill {
            name: "x".into(),
            description: "does x".into(),
            path: PathBuf::from("/tmp"),
        }];
        let out = format_skills_for_prompt(&skills);
        assert!(out.contains("x: does x"));
        assert!(out.contains("/skill:"));
    }

    #[tokio::test]
    async fn discover_async_matches_sync_and_dedups() {
        // TDD Phase 6: async dir scans + concurrent reads, same sort + first-wins + dup warning.
        let base = std::env::temp_dir().join(format!("dex-discover-async-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        let _ = tokio::fs::create_dir_all(a.join("s1")).await;
        let _ = tokio::fs::create_dir_all(b.join("s1")).await;
        tokio::fs::write(
            a.join("s1/SKILL.md"),
            "---\nname: dup\ndescription: a\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            b.join("s1/SKILL.md"),
            "---\nname: dup\ndescription: b\n---\n",
        )
        .await
        .unwrap();
        let dirs = vec![a, b];
        let sync_res = discover_skills_fresh(&dirs);
        let async_res = discover_skills_fresh_async(&dirs).await;
        assert_eq!(sync_res.len(), async_res.len());
        assert_eq!(sync_res[0].name, async_res[0].name);
        // First wins (a before b)
        assert_eq!(async_res[0].description, "a");
        let _ = tokio::fs::remove_dir_all(&base).await;
    }
}
