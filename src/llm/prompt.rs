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

/// Base system prompt, pi-style: identity plus imperative working rules.
/// Tool-behavior detail lives in the tool descriptions (src/llm/protocol.rs),
/// where the model sees it at each tool decision — never duplicated here.
pub(crate) fn system_prompt(skills: &[Skill]) -> String {
    let mut prompt = concat!(
        "You are a coding agent. Use read, ls, grep, find, edit, write, bash to get the job done and report the result.",
        //
        "\n\nWorking rules:\n",
        "- Batch independent reads/searches into ONE parallel call. Don't do one file per turn.\n",
        "- Read before edit; edit with exact oldText; verify with build/tests.\n",
        "- Don't repeat tool calls — once you have enough context, act.\n",
        "\n\nAnswering: be concise, lead with the result, show file paths clearly.",
    )
    .to_string();
    if let Some(ctx) = project_context() {
        prompt.push_str("\n\n--- Project instructions ---\n");
        prompt.push_str(&ctx);
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
}
