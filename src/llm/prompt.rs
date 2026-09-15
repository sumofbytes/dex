use std::env;
use std::fs;
use std::path::PathBuf;

use crate::core::types::Skill;
use crate::skills::format_skills_for_prompt;

/// Nearest project file walking up from the cwd (`AGENTS.md` wins, else
/// `CLAUDE.md`), returned as a path so callers can stat before reading.
fn project_file() -> Option<PathBuf> {
    let mut dir = env::current_dir().ok()?;
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

pub(crate) fn project_context() -> Option<String> {
    let path = project_file()?;
    fs::read_to_string(path).ok()
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
    if let Some(ctx) = project_context() {
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
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Hermetic: ignore any developer shell override for this assertion.
        let prev = std::env::var_os("DEX_SYSTEM_PROMPT");
        let prev_file = std::env::var_os("DEX_SYSTEM_PROMPT_FILE");
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
        match prev {
            Some(v) => std::env::set_var("DEX_SYSTEM_PROMPT", v),
            None => std::env::remove_var("DEX_SYSTEM_PROMPT"),
        }
        match prev_file {
            Some(v) => std::env::set_var("DEX_SYSTEM_PROMPT_FILE", v),
            None => std::env::remove_var("DEX_SYSTEM_PROMPT_FILE"),
        }
    }
}
