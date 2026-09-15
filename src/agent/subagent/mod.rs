//! Sub-agent domain types (plan §4): definitions, instances, results,
//! manager, and the delegation tools.
//!
//! The manager owns lifecycle mechanics only — registry, spawn cap,
//! cancel, timeout, completion notices, and resume handles for manual
//! re-entry. There is no automatic recovery, no spawn queue, and no
//! idle reaper: over-cap spawns reject, and the model re-enters dead
//! children by hand with `delegate(resume_from)`. The blanket
//! `dead_code` allow stays only for genuinely optional surface;
//! everything else is dead-code free.
#![allow(dead_code)]

mod context;
mod definition;
mod exit;
mod instance;
pub(crate) mod manager;
mod result;
pub(crate) mod tools;

#[allow(unused_imports)]
pub(crate) use context::ContextSeed;
#[allow(unused_imports)]
pub(crate) use definition::{AgentDefinition, DEFAULT_AGENT_TIMEOUT, READ_ONLY_TOOLS};
#[allow(unused_imports)]
pub(crate) use exit::{
    classify_body_error, resume_note, transcript_holds_progress, ExhaustKind, ExitReason,
    ResumeHandle, ResumeRequest,
};
#[allow(unused_imports)]
pub(crate) use instance::{AgentId, AgentInstance, AgentState};
#[allow(unused_imports)]
pub(crate) use manager::{
    AgentEvent, AgentManager, AgentNotice, ChildInfo, ProgressReporter, SpawnError, SpawnMeta,
    WaitOutcome,
};
#[allow(unused_imports)]
pub(crate) use result::AgentResult;
pub(crate) use tools::{
    delegation_enabled, execute_delegation, is_delegation, set_daemon_linked, status_word,
    AgentTurnContext,
};

use definition::parse_definition;

const EXPLORER_MD: &str = "---\nname: explorer\ndescription: Understand code without modifying it. Give it a question about the codebase; it returns findings in prose with file paths. Read-only: never modifies files or runs commands.\ntools: read, grep, find\n---\nYou are an explorer. Answer the task with findings in prose: file paths, relevant snippets, risks. Never modify files or run shell commands — you do not have those tools. If the task needs something outside your tools, say so in your result instead of working around it.\n";

const REVIEWER_MD: &str = "---\nname: reviewer\ndescription: Review a diff or change for correctness and regressions. Give it what changed; it returns findings in prose. Read-only.\ntools: read, grep, find, git\n---\nYou are a reviewer. Review the change for correctness, regressions, and missed edge cases; report findings in prose with file paths and line references. Never modify files or run shell commands outside your tools.\n";

const TESTER_MD: &str = "---\nname: tester\ndescription: Investigate and run relevant tests. Give it what to verify; it reports pass/fail plus failures in prose. May run shell commands: they run under a trusted permission policy and are auto-denied otherwise.\ntools: read, grep, find, bash\n---\nYou are a tester. Investigate the requested area and run the relevant tests with your shell; report pass/fail plus the failures in prose with file paths. Keep commands read-only in spirit (run tests, do not deploy or delete). If a command is denied, report that instead of working around it.\n";

/// V1a ships three built-ins, zero required configuration (§19). Parsed
/// through the same frontmatter parser user files will use, so the parser
/// stays live. Child allowlists are independent of the parent prompt-token
/// hiding (`DEX_EXTRA_TOOLS`): the reviewer keeps `git` even when the
/// parent schema hides it.
pub(crate) fn builtin_definitions() -> Vec<AgentDefinition> {
    [EXPLORER_MD, REVIEWER_MD, TESTER_MD]
        .into_iter()
        .map(|md| parse_definition(md).expect("built-in agent must parse"))
        .collect::<Vec<_>>()
}

/// Resolve a definition by name. Unknown names fail with the available
/// agent list — clean rejection, no fallback (§10.1).
pub(crate) fn find_definition(name: &str) -> Result<AgentDefinition, String> {
    let defs = builtin_definitions();
    defs.into_iter().find(|d| d.name == name).ok_or_else(|| {
        let available = builtin_definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown agent '{name}'; available agents: {available}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_load_with_documented_tool_sets() {
        let defs = builtin_definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explorer", "reviewer", "tester"]);
        for def in &defs {
            assert!(!def.description.is_empty(), "{}", def.name);
            assert!(!def.prompt.is_empty(), "{}", def.name);
            assert_eq!(def.model, None, "{} inherits the model", def.name);
        }
        let tools = |n: &str| {
            defs.iter()
                .find(|d| d.name == n)
                .unwrap()
                .tools
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(tools("explorer"), ["find", "grep", "read"]);
        // The reviewer keeps `git` regardless of the parent's
        // `DEX_EXTRA_TOOLS` prompt-token hiding (DEX-9).
        assert_eq!(tools("reviewer"), ["find", "git", "grep", "read"]);
        assert_eq!(tools("tester"), ["bash", "find", "grep", "read"]);
    }

    #[test]
    fn unknown_names_reject_with_the_available_list() {
        let err = find_definition("oracle").unwrap_err();
        assert!(err.contains("unknown agent 'oracle'"), "{err}");
        for name in ["explorer", "reviewer", "tester"] {
            assert!(err.contains(name), "{err}");
        }
        assert!(find_definition("explorer").is_ok());
    }

    #[test]
    fn terminal_states_cover_every_ending() {
        assert!(!AgentState::Running.is_terminal());
        for state in [
            AgentState::Completed,
            AgentState::Failed,
            AgentState::Cancelled,
            AgentState::TimedOut,
        ] {
            assert!(state.is_terminal(), "{state:?}");
        }
    }
}
