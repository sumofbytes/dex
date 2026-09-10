//! Sub-agent domain types (plan §4): definitions, instances, results,
//! manager, and the delegation tools.
//!
//! Phases 0–8 are wired: manager, delegate tools, child body, and the
//! Phase 6 turn-boundary drains all have non-test callers. What stays
//! deliberately dormant under one `dead_code` allow is the post-V1 surface
//! (`AgentInstance::parent_id`, `AgentState::Pending`) that Phase 9's final
//! pass trims or keeps with the contract — zero *unintended* allows must
//! remain. Unused imports, by contrast, are allowed per-item on the
//! re-exports that need them.
#![allow(dead_code)]

mod context;
mod definition;
mod instance;
pub(crate) mod manager;
mod result;
pub(crate) mod tools;

#[allow(unused_imports)]
pub(crate) use context::ContextSeed;
#[allow(unused_imports)]
pub(crate) use definition::{
    AgentDefinition, PermissionInherit, DEFAULT_AGENT_TIMEOUT, READ_ONLY_TOOLS,
};
#[allow(unused_imports)]
pub(crate) use instance::{AgentId, AgentInstance, AgentState};
#[allow(unused_imports)]
pub(crate) use manager::{
    AgentEvent, AgentManager, AgentNotice, ProgressReporter, SpawnError, WaitOutcome,
};
#[allow(unused_imports)]
pub(crate) use result::AgentResult;
pub(crate) use tools::{
    delegation_enabled, execute_delegation, is_delegation, set_daemon_linked, status_word,
    AgentTurnContext,
};

use definition::parse_definition;

/// Same env rule as the main schema (`tools_schema`): `git`/`chain` cost
/// prompt tokens and stay hidden unless opted in. A definition never
/// grants what the environment hides — the reviewer degrades to the
/// read-only trio when the var is absent (§19).
fn extra_tools_enabled() -> bool {
    std::env::var("DEX_EXTRA_TOOLS").as_deref() == Ok("1")
}

const EXPLORER_MD: &str = "---\nname: explorer\ndescription: Understand code without modifying it. Give it a question about the codebase; it returns findings in prose with file paths. Read-only: never modifies files or runs commands.\ntools: read, ffgrep, fffind\n---\nYou are an explorer. Answer the task with findings in prose: file paths, relevant snippets, risks. Never modify files or run shell commands — you do not have those tools. If the task needs something outside your tools, say so in your result instead of working around it.\n";

const REVIEWER_MD: &str = "---\nname: reviewer\ndescription: Review a diff or change for correctness and regressions. Give it what changed; it returns findings in prose. Read-only.\ntools: read, ffgrep, fffind, git\n---\nYou are a reviewer. Review the change for correctness, regressions, and missed edge cases; report findings in prose with file paths and line references. Never modify files or run shell commands outside your tools.\n";

const TESTER_MD: &str = "---\nname: tester\ndescription: Investigate and run relevant tests. Give it what to verify; it reports pass/fail plus failures in prose. May run shell commands: they run under a trusted permission policy and are auto-denied otherwise.\ntools: read, ffgrep, fffind, bash\n---\nYou are a tester. Investigate the requested area and run the relevant tests with your shell; report pass/fail plus the failures in prose with file paths. Keep commands read-only in spirit (run tests, do not deploy or delete). If a command is denied, report that instead of working around it.\n";

/// V1a ships three built-ins, zero required configuration (§19). Parsed
/// through the same frontmatter parser user files will use, so the parser
/// stays live. The reviewer degrades to the read-only trio when
/// `DEX_EXTRA_TOOLS` is absent.
pub(crate) fn builtin_definitions() -> Vec<AgentDefinition> {
    let mut defs = [EXPLORER_MD, REVIEWER_MD, TESTER_MD]
        .into_iter()
        .map(|md| parse_definition(md).expect("built-in agent must parse"))
        .collect::<Vec<_>>();
    if !extra_tools_enabled() {
        for def in &mut defs {
            def.tools.remove("git");
            def.tools.remove("chain");
        }
    }
    defs
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

    /// The two env-touching tests below serialize on this: one sets
    /// `DEX_EXTRA_TOOLS` while the other removes it, and parallel threads
    /// would flake each other (same reason as `TEST_SESSIONS_ENV_LOCK`).
    static EXTRA_TOOLS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Save/restore one env var (all `DEX_EXTRA_TOOLS` readers adapt to
    /// either value, so no cross-test lock is needed — just hygiene).
    struct EnvRestore {
        key: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let saved = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, saved }
        }

        fn remove(key: &'static str) -> Self {
            let saved = std::env::var_os(key);
            unsafe { std::env::remove_var(key) };
            Self { key, saved }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.saved {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn builtins_load_with_documented_tool_sets() {
        let _lock = EXTRA_TOOLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::remove("DEX_EXTRA_TOOLS");
        let defs = builtin_definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explorer", "reviewer", "tester"]);
        for def in &defs {
            assert!(!def.description.is_empty(), "{}", def.name);
            assert!(!def.prompt.is_empty(), "{}", def.name);
            assert_eq!(def.model, None, "{} inherits the model", def.name);
            assert_eq!(def.permissions, PermissionInherit::Inherit);
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
        assert_eq!(tools("explorer"), ["fffind", "ffgrep", "read"]);
        // Degraded: no git without the opt-in.
        assert_eq!(tools("reviewer"), ["fffind", "ffgrep", "read"]);
        assert_eq!(tools("tester"), ["bash", "fffind", "ffgrep", "read"]);
    }

    #[test]
    fn reviewer_keeps_git_when_extra_tools_opt_in() {
        let _lock = EXTRA_TOOLS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::set("DEX_EXTRA_TOOLS", "1");
        let defs = builtin_definitions();
        let reviewer = defs.iter().find(|d| d.name == "reviewer").unwrap();
        assert!(reviewer.tools.contains("git"), "{:?}", reviewer.tools);
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
        assert!(!AgentState::Pending.is_terminal());
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
