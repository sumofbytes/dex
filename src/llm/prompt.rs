use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::core::types::Skill;
use crate::skills::format_skills_for_prompt;

/// Nearest project file walking up from `dir` (`AGENTS.md` wins, else
/// `CLAUDE.md`), returned as a path so callers can stat before reading.
fn project_file_from(dir: &Path) -> Option<PathBuf> {
    let mut dir = dir.to_path_buf();
    loop {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// `project_context` result, cached process-wide and invalidated by file
/// identity (perf doc §11): the daemon rebuilds the system prompt every
/// turn, and each rebuild was re-walking ancestors + re-reading AGENTS.md.
/// A missing project file is NOT cached (a walk is stat-only; caching the
/// absence would hide a file created mid-process). Keyed by (cwd, path) so
/// a multi-session daemon serving several workspaces doesn't cross-contaminate.
static PROJECT_CONTEXT_CACHE: OnceLock<Mutex<HashMap<(PathBuf, PathBuf), ProjectContextCache>>> =
    OnceLock::new();

struct ProjectContextCache {
    mtime: SystemTime,
    len: u64,
    content: Option<String>,
}

fn project_cache() -> &'static Mutex<HashMap<(PathBuf, PathBuf), ProjectContextCache>> {
    PROJECT_CONTEXT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn project_context() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    project_context_for(&cwd)
}

/// Same as [`project_context`] but rooted at `dir`: daemon turns pass the
/// session workspace so multi-session daemons don't serve the daemon cwd's
/// file to every session.
pub(crate) fn project_context_for(dir: &Path) -> Option<String> {
    let cwd = dir.to_path_buf();
    let path = project_file_from(&cwd)?;
    // stat → read → stat: a writer racing the read leaves mismatched
    // identities, and the torn bytes are served once WITHOUT caching —
    // caching post-read bytes under a pre-read identity (or vice versa)
    // would pin stale project instructions until the *next* change.
    let meta_before = fs::metadata(&path).ok()?;
    let (mtime_b, len_b) = (meta_before.modified().ok()?, meta_before.len());
    let content = fs::read_to_string(&path).ok();
    let meta_after = fs::metadata(&path).ok()?;
    let (mtime, len) = (meta_after.modified().ok()?, meta_after.len());
    if mtime_b != mtime || len_b != len {
        return content;
    }
    let mut cache = project_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(&(cwd.clone(), path.clone())) {
        if hit.mtime == mtime && hit.len == len {
            return hit.content.clone();
        }
    }
    cache.insert(
        (cwd, path),
        ProjectContextCache {
            mtime,
            len,
            content: content.clone(),
        },
    );
    // Bounded-cap: project files are few, but a daemon serving many
    // workspaces must not grow without bound. Eviction is arbitrary
    // (`HashMap` order), not FIFO — the cap is a safety net, and any live
    // entry re-caches on next use.
    if cache.len() > 64 {
        if let Some(k) = cache.keys().next().cloned() {
            cache.remove(&k);
        }
    }
    content
}

/// Base system prompt: identity plus imperative working rules.
/// Tool-behavior detail lives in the tool descriptions (src/llm/protocol.rs),
/// where the model sees it at each tool decision — never duplicated here.
///
/// A custom `system_prompt:` / `DEX_SYSTEM_PROMPT` / `--system-prompt` (or its
/// `*_file` variant) replaces only this base; project instructions,
/// extensions and skills are still appended.
pub(crate) fn system_prompt(skills: &[Skill]) -> String {
    system_prompt_with_override(skills, None)
}

/// Same as [`system_prompt`], with an explicit per-request/CLI text override
/// (already resolved client-side, so remote daemons need no file access).
/// `Some` replaces the built-in base; `None` falls back to the env/file
/// layers via `system_prompt_origin`.
pub(crate) fn system_prompt_with_override(skills: &[Skill], explicit: Option<&str>) -> String {
    system_prompt_with_override_for(skills, explicit, None)
}

/// Same as [`system_prompt_with_override`] but rooted at `cwd` for the
/// project-instructions lookup: daemon turns pass the session workspace so
/// multi-session daemons don't serve the daemon cwd's file to every session.
pub(crate) fn system_prompt_with_override_for(
    skills: &[Skill],
    explicit: Option<&str>,
    cwd: Option<&Path>,
) -> String {
    let (custom, _) = crate::llm::config::system_prompt_origin(explicit);
    let mut prompt = custom.unwrap_or_else(|| {
        concat!(
            "You are a coding agent. Use read, ls, grep, find, edit, write, bash to get the job done and report the result.",
            //
            "\n\nWorking rules:\n",
            "- Batch independent reads/searches into ONE parallel call. Don't do one file per turn.\n",
            "- Read before edit; edit with exact oldText; verify with build/tests.\n",
            "- Don't repeat tool calls — once you have enough context, act.\n",
            "\n\nAnswering: be concise, lead with the result, show file paths clearly.",
        )
        .to_string()
    });
    let ctx = match cwd {
        Some(dir) => project_context_for(dir),
        None => project_context(),
    };
    if let Some(ctx) = ctx {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&ctx);
    }
    // Load-time extension contributions (`dex.prompt.append`): read-only
    // influence, no gate interaction (plan §7).
    let appendix = crate::extensions::prompt_appendix();
    if !appendix.is_empty() {
        prompt.push_str("\n\n--- Extensions ---\n");
        prompt.push_str(&appendix);
    }
    if !skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(skills));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_lines_have_no_ragged_indentation() {
        // Only the base prompt — appended AGENTS.md/skills may indent freely.
        let base = system_prompt(&[])
            .split("\n\n--- Project instructions ---\n")
            .next()
            .unwrap()
            .to_string();
        assert!(!base.contains("--- Project instructions"));
        for line in base.lines() {
            assert_eq!(line, line.trim_start(), "ragged prompt line: {line:?}");
        }
    }

    #[test]
    fn base_prompt_covers_answer_and_tool_rules() {
        let base = system_prompt(&[]);
        for needle in ["Working rules:", "Answering:"] {
            assert!(base.contains(needle), "missing section: {needle}");
        }
    }

    #[test]
    fn base_prompt_does_not_duplicate_tool_schema_detail() {
        // Pagination/chain/stitching guidance lives in tool descriptions, not here.
        let base = system_prompt(&[])
            .split("\n\n--- Project instructions ---\n")
            .next()
            .unwrap()
            .to_string();
        for needle in [
            "offset/limit",
            "from/take",
            "`chain`",
            "$DEX_BIN",
            "output_mode",
        ] {
            assert!(
                !base.contains(needle),
                "tool-schema detail leaked: {needle}"
            );
        }
    }

    #[test]
    fn custom_base_replaces_builtin_but_keeps_skills_appendix() {
        struct EnvRestore {
            vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
        }
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                for (key, prev) in self.vars.drain(..) {
                    match prev {
                        Some(v) => std::env::set_var(key, v),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Hermetic: ignore any developer shell/config override for this assertion.
        let _restore = EnvRestore {
            vars: [
                "DEX_CONFIG",
                "DEX_SYSTEM_PROMPT",
                "DEX_SYSTEM_PROMPT_FILE",
                "XDG_CACHE_HOME",
            ]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect(),
        };
        std::env::set_var(
            "DEX_CONFIG",
            std::env::temp_dir().join(format!("dex-prompt-test-{}", std::process::id())),
        );
        std::env::set_var(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("dex-prompt-cache-{}", std::process::id())),
        );
        std::env::remove_var("DEX_SYSTEM_PROMPT");
        std::env::remove_var("DEX_SYSTEM_PROMPT_FILE");
        let custom = system_prompt_with_override(&[], Some("You are a pirate."));
        assert!(custom.starts_with("You are a pirate."), "{custom}");
        assert!(!custom.contains("Batch independent reads"), "{custom}");
        let skills = vec![crate::core::types::Skill {
            name: "s".to_string(),
            description: "d".to_string(),
            path: std::path::PathBuf::from("/tmp/s/SKILL.md"),
        }];
        let with_skills = system_prompt_with_override(&skills, Some("You are a pirate."));
        assert!(
            with_skills.starts_with("You are a pirate."),
            "{with_skills}"
        );
        assert!(with_skills.contains("s"), "{with_skills}");
    }
}
