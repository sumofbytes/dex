//! Headless model/system-facing text: the shared formats for messages that
//! are *not* presentation — persisted tool context, child-agent lifecycle
//! markers, and status lines read by the daemon, the one-shot CLI, and the
//! budget probe. Lives in `runtime/` (not `ui/format`) so `daemon/`,
//! `tools/`, and `mcp/` can use it without a `ui` dependency.

/// Model-facing text for a `!`/`!!` shell run:
/// the persisted message the next turn reads. The output is already clamped
/// for the context window by the bash tool. Single owner for the daemon
/// (`POST /shell`) and local one-shot paths so the two can't diverge.
pub fn bash_context_text(
    command: &str,
    output: &str,
    success: bool,
    code: Option<i32>,
    cancelled: bool,
) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.trim().is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if !success {
        match code {
            Some(code) => text.push_str(&format!("\n\nCommand exited with code {code}")),
            None => text.push_str("\n\nCommand failed"),
        }
    }
    text
}

/// Child-agent lifecycle system lines (`[agent <name>:<id>] started|finished
/// …`, formatted in `delegate/tools.rs` / `delegate/manager.rs`) get their own
/// spawn/terminal marker so a delegation pops out of the muted system notes,
/// like the per-tool glyphs do. One owner for the TUI and the headless REPL —
/// the format lives in two emit sites, so the parser must not be duplicated.
/// Returns the marker (`◈` spawn / `◇` terminal) and the text after the
/// `[agent ` prefix.
pub fn agent_lifecycle(s: &str) -> Option<(&'static str, &str)> {
    let rest = s.strip_prefix("[agent ")?;
    let marker = if rest.contains(" finished ") {
        "◇"
    } else {
        "◈"
    };
    Some((marker, rest))
}

/// Git branch + dirty flag for a working directory, for status displays.
/// Sync twin kept only as the async path's reference implementation under
/// test (`git_context_async` must produce identical results).
#[allow(dead_code)] // kept for dependent crates: sync twin of git_context_async
pub fn git_context(cwd: &str) -> (Option<String>, bool) {
    use std::process::Command;
    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && Command::new("git")
            .args(["-C", cwd, "status", "--porcelain"])
            .stdin(std::process::Stdio::null())
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}

pub async fn git_context_async(cwd: &str) -> (Option<String>, bool) {
    use tokio::process::Command as AsyncCommand;
    // `branch` and `status` are independent spawns (~5-30ms each): run them
    // together instead of serially. The `branch.is_some()` guard stays on the
    // *result* — outside a repo `status` prints to stderr, so stdout is empty
    // anyway — at the cost of one wasted spawn in non-repos.
    let mut branch_cmd = AsyncCommand::new("git");
    branch_cmd
        .args(["-C", cwd, "branch", "--show-current"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let mut status_cmd = AsyncCommand::new("git");
    status_cmd
        .args(["-C", cwd, "status", "--porcelain"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let (branch_out, status_out) = tokio::join!(branch_cmd.output(), status_cmd.output());
    let branch = branch_out
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && status_out
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_lifecycle_markers() {
        // The two emit formats (spawn in `delegate/tools.rs`, terminal in
        // `delegate/manager.rs`) must both parse — a wording change here
        // downgrades the lines to muted system notes.
        let (marker, rest) = agent_lifecycle("[agent explorer:sess-1] started").unwrap();
        assert_eq!(marker, "◈");
        assert_eq!(rest, "explorer:sess-1] started");
        let (marker, rest) =
            agent_lifecycle("[agent explorer:sess-1] finished completed · 3 tok").unwrap();
        assert_eq!(marker, "◇");
        assert_eq!(rest, "explorer:sess-1] finished completed · 3 tok");
        // Non-lifecycle system notes stay muted.
        assert!(agent_lifecycle("note: bash result truncated to 45 KiB").is_none());
        assert!(agent_lifecycle("").is_none());
    }

    #[tokio::test]
    async fn git_context_async_matches_sync() {
        // TDD Phase 6: tokio::process git spawns under cache, same branch/dirty.
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let sync_res = git_context(&cwd);
        let async_res = git_context_async(&cwd).await;
        assert_eq!(sync_res, async_res);
    }
}
