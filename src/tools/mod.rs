#![allow(clippy::doc_lazy_continuation)]
mod audit;
mod edit;
pub(crate) mod error;
mod meta;
pub(crate) mod outcome;
pub(crate) mod plan;
pub(crate) mod policy;
mod read;
pub(crate) mod sandbox;
mod search;
mod shell;
mod write;

// Workspace confinement lives in `sandbox.rs`; re-exported here so existing
// `tools::...` paths keep working.
use audit::audit;
pub(crate) use error::ToolError;
pub(crate) use outcome::{ShellEvidence, ToolOutcome};
pub(crate) use policy::{
    enforce_policy, metadata, metadata_native, PermissionRequirement, ToolFilter,
};
pub(crate) use sandbox::{
    normalize_conflict_path, resolve_workspace_path, workspace_path, workspace_root,
};
// Leaf tool implementations live in per-tool modules; re-exported so
// existing `tools::...` paths keep working.
#[cfg(test)]
use edit::{apply_edit, apply_edit_batch, parse_edit_ops};
use edit::{change_diff_async, tool_edit};
pub(crate) use meta::Policy;
use meta::{tool_chain, tool_git, tool_ls};
#[cfg(test)]
use read::expand_glob_in;
use read::tool_read;
pub(crate) use shell::parse_shell_escape;
#[cfg(test)]
use shell::run_bash_with_limits;
use shell::{run_bash, tool_bash, BASH_CLAMP_BYTES, BASH_CLAMP_LINES};
pub(crate) use write::hash_file;
use write::tool_write;

use self::search::{tool_fffind, tool_ffgrep};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::time::Duration;

use self::plan::{format_plan_snapshot, parse_plan_progress, parse_plan_steps};
use crate::core::format::{clamp_lines_checked, clip_chars};
use crate::runtime::cancel::CancellationSource;

pub(crate) static CONFIGURED_OUTPUT_LIMIT: AtomicUsize = AtomicUsize::new(1_048_576);

pub(crate) fn set_output_limit(limit: usize) {
    if limit > 0 {
        CONFIGURED_OUTPUT_LIMIT.store(limit, Ordering::Relaxed);
    }
}

/// `update_plan`: validate the full plan replacement and echo the snapshot.
/// Pure — the boundary bookkeeping and the compaction decision live in the
/// agent loop ([`crate::agent::online_compaction`]).
fn tool_update_plan(args: &Map<String, Value>) -> Result<String, ToolError> {
    let steps = args.get("steps").ok_or(ToolError::Missing("steps"))?;
    let steps = parse_plan_steps(steps).map_err(ToolError::InvalidArgument)?;
    let progress = parse_plan_progress(args.get("progress")).map_err(ToolError::InvalidArgument)?;
    Ok(format_plan_snapshot(&steps, &progress))
}

fn arg_str(args: &Map<String, Value>, key: &'static str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::NotString(key)),
        None => Err(ToolError::Missing(key)),
    }
}

/// The `then_run` field (SoL-Pi-compatible): the verification command a
/// `write`/`edit` call carries. Only those two tools accept it; `null`, empty and whitespace-only
/// values all mean "no command" so an omitted optional field stays harmless.
/// A present-but-unusable value (object, array, number) is an error rather than
/// a silent no-op: a model guessing another harness's
/// `then_run: {command: …, timeout: …}` shape would otherwise read the
/// mutation's success as its own verification.
fn then_run_command<'a>(
    name: &str,
    args: &'a Map<String, Value>,
) -> Result<Option<&'a str>, ToolError> {
    if !matches!(name, "write" | "edit") {
        return Ok(None);
    }
    match args.get("then_run") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(command)) => Ok(Some(command.trim()).filter(|value| !value.is_empty())),
        Some(_) => Err(ToolError::InvalidArgument(
            "'then_run' must be a shell command string".to_string(),
        )),
    }
}

/// Append the `then_run` observation to a *successful* write/edit result.
/// Returns the enriched text plus the command's exit code (`None` when the
/// command failed to spawn, timed out, or was cancelled) so the caller can
/// record the shell run on the audit trail. Same clamping and shell timeout as
/// `bash`; a non-zero exit or timeout is reported in-band rather than as a tool
/// error, because the mutation itself did succeed and the model needs both
/// facts to decide what to do next.
async fn append_then_run(
    mut text: String,
    command: &str,
    cancel: &(dyn CancellationSource + Send + Sync),
    session: Option<&Path>,
    shell_out: &mut Option<ShellEvidence>,
) -> (String, Option<i32>) {
    let (output, code) = match run_bash(command, cancel).await {
        Ok(result) => result,
        Err(error) => {
            text.push_str(&format!(
                "\n\n[then_run:failed] {}\n(error: {error})",
                clip_command(command)
            ));
            // No exit code and no archive: the command never produced output.
            *shell_out = Some(ShellEvidence {
                archive_id: None,
                exit_code: None,
            });
            return (text, None);
        }
    };
    let marker = match code {
        Some(0) => "succeeded".to_string(),
        Some(code) => format!("failed (exit {code})"),
        None => "failed".to_string(),
    };
    text.push_str(&format!(
        "\n\n[then_run:{marker}] {}\n",
        clip_command(command)
    ));
    let (clamped, was_clamped) = clamp_lines_checked(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES);
    let (clamped, archive_id) =
        crate::agent::evidence_reducer::capture(session, "then_run", &output, clamped, was_clamped);
    *shell_out = Some(ShellEvidence {
        archive_id,
        exit_code: code,
    });
    text.push_str(if clamped.trim().is_empty() {
        "(no output)"
    } else {
        &clamped
    });
    (text, code)
}

/// One-line clip for the `then_run` command echoed in the result marker. The
/// command also rides the tool-input preview, so the marker needs only enough
/// to identify it — not a second full copy of a very long command in context.
fn clip_command(command: &str) -> String {
    clip_chars(&command.replace('\n', " "), 200)
}

/// The session path the evidence reducer archives into (`Some` only for
/// daemon parent turns — the same source the projection and recall read).
fn evidence_session(policy: &Policy) -> Option<PathBuf> {
    policy.agent.as_ref().map(|ctx| ctx.session_path.clone())
}

/// Execute a tool using paths confined to the current workspace.
pub(crate) async fn execute(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let mut shell = None;
    execute_with_shell(name, args, cancel, policy, filter, &mut shell).await
}

/// Like `execute`, but surfaces the out-of-band shell facts (archive id,
/// exit code) the evidence reducer needs without parsing them out of the
/// result text. Only the agent loop's outcome path needs this; every other
/// caller uses `execute`.
pub(crate) async fn execute_with_shell(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
    shell_out: &mut Option<ShellEvidence>,
) -> Result<String, ToolError> {
    // H1 (`tool.before`, plan §8): mutate/deny seam before every gate.
    // Delegation tools skip it — the child allowlist sees the call the
    // parent issued, unmodified by a third party.
    let owned_args: Map<String, Value>;
    let mut hooks_ran = false;
    // Zero-cost when no extension subscribes: the args pass through uncloned.
    let args = if crate::agent::subagent::is_delegation(name)
        || !crate::extensions::has_event_handlers("tool.before")
    {
        args
    } else {
        hooks_ran = true;
        match crate::extensions::apply_before_hooks(name, args, cancel, policy, filter).await {
            crate::extensions::BeforeOutcome::Proceed { args, mutated_by } => {
                for ext in &mutated_by {
                    audit(&format!("{name}[tool.before:{ext}]"), &args, "mutated");
                }
                owned_args = args;
                &owned_args
            }
            crate::extensions::BeforeOutcome::Denied { by, reason } => {
                return Err(ToolError::Denied(format!(
                    "tool.before hook from extension '{by}' denied the call: {reason}"
                )));
            }
        }
    };
    // Resolve `then_run` once — it decides both the permission requirement
    // inside `dispatch_tool` and the follow-up run below, and a malformed
    // value must fail before anything else happens. Only `write`/`edit`
    // accept the field, so for every other tool — delegation and MCP ones
    // included — this is a side-effect-free `Ok(None)`.
    let then_run = match then_run_command(name, args) {
        Ok(command) => command,
        Err(error) => return Err(error),
    };
    let result = dispatch_tool(
        name, args, then_run, cancel, policy, filter, shell_out, true,
    )
    .await;
    // A tool.before hook already saw the args even when a later gate denies
    // the call: record that observation, so the audit trail shows a third
    // party witnessed a call it never got to influence.
    if hooks_ran {
        if let Err(ToolError::Denied(reason)) = &result {
            audit(
                &format!("{name}[tool.before]"),
                args,
                &format!("observed; call denied: {reason}"),
            );
        }
    }
    // Run `then_run` in this same call so the mutation and its
    // verification arrive as one observation, instead of costing a second
    // provider round-trip that re-sends the whole prefix just to learn whether
    // the build passed. A failed mutation skips the command: the edit error is
    // the observation, and a command output must never be handed back as if it
    // had run against the new content. (`then_run` is only ever `Some` for
    // `write`/`edit`, which always take the workspace path above.)
    let result = match (result, then_run) {
        (Ok(text), Some(command)) => {
            let (text, code) = append_then_run(
                text,
                command,
                cancel,
                evidence_session(policy).as_deref(),
                shell_out,
            )
            .await;
            // Audit the shell run separately from the mutation: `DEX_AUDIT=1`
            // must show that a command ran and how it exited, not just a
            // successful `write`/`edit`.
            let mut shell_args = Map::new();
            shell_args.insert("command".into(), Value::String(command.to_string()));
            shell_args.insert("then_run_of".into(), Value::String(name.to_string()));
            let shell_outcome = match code {
                Some(0) => "ok".to_string(),
                Some(code) => format!("exit {code}"),
                None => "no exit status (timeout, cancel, or spawn failure)".to_string(),
            };
            audit("bash", &shell_args, &shell_outcome);
            Ok(text)
        }
        (result, _) => result,
    };
    // One audit row per tool call: every exit path — the delegation denial, a
    // malformed `then_run`, an unknown tool, the allowlist rejections, the
    // policy denial, the provider/tool failure — lands here with exactly the
    // outcome string the per-exit audits used to record.
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(error) => error.to_string(),
    };
    audit(name, args, &outcome);
    result
}

/// Re-dispatch the shadowed built-in for `dex.tools.call_original`: the
/// same gates as any call (approval included), but never the shadow itself.
/// `then_run` stays with the outer shadow call, which resolves and audits it.
pub(crate) async fn dispatch_original(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
    shell_out: &mut Option<ShellEvidence>,
) -> Result<String, ToolError> {
    dispatch_tool(name, args, None, cancel, policy, filter, shell_out, false).await
}

/// The tool-selection core behind `execute_with_shell`: routes and gates a
/// call, then runs it — with no `audit` calls (the single audit row lives in
/// the caller). The gate order is load-bearing (§11): delegation route first,
/// then the permission requirement, then the child allowlist (a sharper,
/// cheaper rejection than a prompt), then `enforce_policy` — so a denial
/// never triggers a prompt — and only then dispatch.
#[allow(clippy::too_many_arguments)]
async fn dispatch_tool(
    name: &str,
    args: &Map<String, Value>,
    then_run: Option<&str>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
    shell_out: &mut Option<ShellEvidence>,
    resolve_shadow: bool,
) -> Result<String, ToolError> {
    // Delegation tools route first (Phase 5): they are not workspace tools
    // and have no static registry entry. The child allowlist never contains
    // one, so the same availability gate rejects a child's call before any
    // delegation logic runs (§11 — at-cap children carry no delegation tools).
    if crate::agent::subagent::is_delegation(name) {
        if let Some(filter) = filter {
            if !filter.allows(name) {
                return Err(ToolError::Denied(format!(
                    "tool '{name}' is not in {}'s tool allowlist",
                    filter.owner
                )));
            }
        }
        return crate::agent::subagent::execute_delegation(name, args, cancel, policy).await;
    }
    // Inside a shadow re-dispatch (`resolve_shadow == false`) the shadow's
    // Shell row must not raise the gate again — use the native requirement.
    let requirement = match if resolve_shadow {
        metadata(name)
    } else {
        metadata_native(name)
    } {
        // MCP tools are external processes: their `metadata()` row already
        // carries the most restrictive gate (same as shell), so the separate
        // `mcp__` requirement check is gone from here.
        Some(meta) => {
            // A `write`/`edit` carrying `then_run` also runs a shell
            // command, so it must clear the *shell* gate rather than the weaker
            // write gate — `ask-shell` passes file mutations unprompted, which
            // would otherwise make an `edit` with `then_run` a way to run a
            // command unapproved.
            if then_run.is_some() {
                PermissionRequirement::Shell
            } else {
                meta.permission
            }
        }
        None => return Err(ToolError::Unknown(name.to_string())),
    };
    // Availability before approval: a sharper, cheaper rejection naming the
    // agent's allowlist (plan §11 — the model self-corrects, never executes).
    // Checked before `enforce_policy` so a denied tool never prompts.
    if let Some(filter) = filter {
        let error = if !filter.allows(name) {
            Some(ToolError::Denied(format!(
                "tool '{name}' is not in {}'s tool allowlist",
                filter.owner
            )))
        } else if then_run.is_some() && !filter.allows("bash") {
            // `then_run` runs a shell command, so an allowlist that permits
            // `edit` but not `bash` must not become a shell escape hatch.
            Some(ToolError::Denied(format!(
                "'{name}' carries then_run, which needs 'bash' in {}'s tool allowlist",
                filter.owner
            )))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(error);
        }
    }
    if resolve_shadow && crate::extensions::is_shadowed(name) {
        // Same gates as the ext__ row in metadata(): a shadow intercepts a
        // built-in, so it can lie about what the built-in does.
        enforce_policy(name, args, PermissionRequirement::Shell, cancel, policy).await?;
        return crate::extensions::call_shadow_global(
            name, args, cancel, policy, filter, shell_out,
        )
        .await
        .map_err(ToolError::Internal);
    }
    enforce_policy(name, args, requirement, cancel, policy).await?;
    if name.starts_with("mcp__") {
        // `ToolError::Internal` displays as the raw message, so the caller's
        // single audit row records exactly the string audited here before.
        return crate::mcp::call_global(name, args, cancel)
            .await
            .map_err(ToolError::Internal);
    }
    if crate::extensions::is_extension_tool(name) {
        // Same audit contract as MCP: the raw string lands in the one row.
        // (`call_global` normalizes the deprecated `lua__` alias.)
        return crate::extensions::call_global(name, args, cancel, policy, filter)
            .await
            .map_err(ToolError::Internal);
    }
    // fff owns its threads + lock; run inside spawn_blocking (10s grep budget stays).
    if matches!(name, "grep" | "ffgrep" | "find" | "fffind") {
        let name_owned = name.to_string();
        let args_owned = args.clone();
        return tokio::task::spawn_blocking(move || match name_owned.as_str() {
            "grep" | "ffgrep" => tool_ffgrep(&args_owned),
            _ => tool_fffind(&args_owned),
        })
        .await
        .unwrap_or(Err(ToolError::Internal("fff worker panicked".into())));
    }
    match name {
        "read" => tool_read(args).await,
        "bash" => tool_bash(args, cancel, evidence_session(policy).as_deref(), shell_out).await,
        "write" => tool_write(args).await,
        "edit" => tool_edit(args).await,
        "grep" | "ffgrep" | "find" | "fffind" => unreachable!("handled above"),
        "ls" => tool_ls(args).await,
        "git" => tool_git(args, cancel).await,
        "chain" => tool_chain(args, cancel, policy, filter).await,
        "update_plan" => tool_update_plan(args),
        "obs_recall" => tool_obs_recall(args, cancel, policy),
        _ => unreachable!("metadata and dispatch must stay in sync"),
    }
}

pub(crate) fn tool_obs_recall(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
) -> Result<String, ToolError> {
    // Only a daemon parent turn carries a session path (same source the
    // projection reads): OneShot / direct runs / children have none and
    // fail with a clear error rather than silently succeeding with an
    // empty archive.
    let Some(session_path) = policy.agent.as_ref().map(|ctx| ctx.session_path.clone()) else {
        return Err(ToolError::InvalidArgument(
            "obs_recall requires a daemon session (no session archive attached)".to_string(),
        ));
    };
    let _ = cancel;
    crate::agent::obs_pack::tool_obs_recall(&session_path, args)
}

/// Execute a tool, reporting success explicitly. Callers must not re-derive
/// success from the output text: tool output can legitimately contain
/// strings like `[exit 1]` (shell markers appear in source files and logs).
pub(crate) async fn execute_outcome(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> ToolOutcome {
    // Capture the unified diff BEFORE `execute` mutates the file; the
    // before-image is gone afterwards. Display-only: it never reaches the
    // model, only the transcript preview via `ToolOutcome::diff`. A `then_run`
    // (e.g. a formatter) can touch the file again afterwards; the
    // preview stays the mutation's diff, which is the change the model asked
    // for and asked to see.
    let pending_diff = if matches!(name, "write" | "edit") {
        change_diff_async(name, args).await
    } else {
        None
    };
    let mut shell = None;
    let (mut text, mut ok) =
        match execute_with_shell(name, args, cancel, policy, filter, &mut shell).await {
            Ok(out) => (out, true),
            Err(e) => (format!("Error: {}", e), false),
        };
    // H2 middleware: `tool.after` may rewrite the result. Fail-open — a hook
    // error keeps the host result (the manager already logs it).
    let after =
        crate::extensions::apply_after_hooks(name, args, &text, ok, cancel, policy, filter).await;
    text = after.text;
    ok = after.ok;
    ToolOutcome {
        text,
        ok,
        diff: pending_diff,
        shell,
    }
}

/// Sync wrappers for `dex run <tool>` / `dex --tool` raw paths (no async CLI
/// plumbing needed per plan §5). Explicit user invocations run trusted —
/// the command itself is the approval — so no policy parameter; unfiltered
/// too (explicit invocations run the full toolset, never a child allowlist).
pub(crate) fn execute_sync(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    crate::client::http::block_on(execute(name, args, cancel, &Policy::trusted(), None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::cancel::GlobalCancellation;
    use serde_json::json;

    #[test]
    fn shell_escape_splits_command() {
        assert_eq!(
            parse_shell_escape("!ls -la"),
            Some(("ls -la".to_string(), false))
        );
        assert_eq!(
            parse_shell_escape("!  echo hi  "),
            Some(("echo hi".to_string(), false))
        );
        assert_eq!(
            parse_shell_escape("!!cargo test"),
            Some(("cargo test".to_string(), true))
        );
        assert_eq!(
            parse_shell_escape("!!  echo hi  "),
            Some(("echo hi".to_string(), true))
        );
        // Multiline scripts run whole.
        assert_eq!(
            parse_shell_escape("!echo a\necho b"),
            Some(("echo a\necho b".to_string(), false))
        );
        // Bare `!`/`!!` fall through to the agent (usage, not a run).
        assert_eq!(parse_shell_escape("!"), None);
        assert_eq!(parse_shell_escape("!   "), None);
        assert_eq!(parse_shell_escape("!!"), None);
        // Ordinary prompts and slash commands are not shell escapes.
        assert_eq!(parse_shell_escape("hello"), None);
        assert_eq!(parse_shell_escape("/model foo"), None);
        assert_eq!(parse_shell_escape(""), None);
    }
    #[tokio::test]
    async fn bash_exposes_the_binary_for_local_stitching() {
        let (output, code) = run_bash_with_limits(
            "printf '%s' \"$DEX_BIN\"",
            Duration::from_secs(5),
            4096,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert_eq!(code, Some(0));
        assert_eq!(
            output,
            std::env::current_exe().unwrap().display().to_string()
        );
    }
    #[tokio::test]
    async fn bash_children_have_no_controlling_tty() {
        // A tool child must never share the user's terminal: a child that
        // probes it (e.g. cargo test → theme query → OSC 10/11 on /dev/tty)
        // would write to and race the TUI's crossterm for the same pts, and
        // half a color report could end up typed into the composer.
        let (output, code) = run_bash_with_limits(
            "if cat </dev/tty >/dev/null 2>&1; then echo HAS_TTY; else echo NO_TTY; fi",
            Duration::from_secs(5),
            4096,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert_eq!(code, Some(0));
        assert!(
            output.contains("NO_TTY"),
            "tool child still has a controlling tty: {output}"
        );
    }

    #[tokio::test]
    async fn metadata_classifies_tools() {
        assert!(metadata("read").unwrap().read_only);
        assert!(metadata("write").unwrap().mutating);
        assert!(!metadata("bash").unwrap().idempotent);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn expand_glob_lists_pruned_files_only() {
        // Hermetic fixture for the `find -prune` semantics glob expansion
        // relies on: pruned dirs never match, symlinked dirs are listed but
        // never descended, outside-workspace symlinks are rejected.
        let dir = std::env::temp_dir().join(format!(
            "dex-glob-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["sub/nested", "target", ".git", "node_modules"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        for f in [
            "a.rs",
            "sub/b.rs",
            "sub/c.txt",
            "sub/nested/g.rs",
            "target/d.rs",
            ".git/e.rs",
            "node_modules/f.rs",
            "x",
        ] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        std::os::unix::fs::symlink("sub", dir.join("linksub")).unwrap();
        let outside = dir.with_extension("outside");
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("leak")).unwrap();
        let names = |paths: Vec<std::path::PathBuf>| -> Vec<String> {
            paths
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect()
        };
        // Bare pattern: basenames anywhere, pruned dirs excluded.
        assert_eq!(
            names(expand_glob_in(&dir, "*.rs").await.unwrap()),
            ["a.rs", "b.rs", "g.rs"]
        );
        // Path pattern: `*` spans `/`, like `find -path`.
        assert_eq!(
            names(expand_glob_in(&dir, "sub/*.rs").await.unwrap()),
            ["b.rs", "g.rs"]
        );
        // No double-descent through the symlinked dir: each file once.
        assert_eq!(
            names(expand_glob_in(&dir, "sub/*").await.unwrap()),
            ["b.rs", "c.txt", "g.rs"]
        );
        // `?` matches the single-char file; the `.` root never surfaces.
        assert_eq!(names(expand_glob_in(&dir, "?").await.unwrap()), ["x"]);
        // Outside-workspace symlink stays rejected; dirs stay files-only.
        assert_eq!(
            names(expand_glob_in(&dir, "*").await.unwrap()),
            ["a.rs", "b.rs", "c.txt", "g.rs", "x"]
        );
        assert!(expand_glob_in(&dir, "*.nope").await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn tool_arguments_are_validated() {
        let args = Map::new();
        assert!(matches!(
            execute("read", &args, &GlobalCancellation, &Policy::trusted(), None).await,
            Err(ToolError::Missing("path"))
        ));
        let mut args = Map::new();
        args.insert("path".into(), Value::Bool(true));
        assert!(matches!(
            execute("read", &args, &GlobalCancellation, &Policy::trusted(), None).await,
            Err(ToolError::NotString("path"))
        ));
    }

    #[tokio::test]
    async fn fffind_rejects_unbounded_patterns() {
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("*".into()));
        assert!(matches!(
            execute(
                "fffind",
                &args,
                &GlobalCancellation,
                &Policy::trusted(),
                None
            )
            .await,
            Err(ToolError::InvalidArgument(_))
        ));
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("".into()));
        assert!(matches!(
            execute(
                "ffgrep",
                &args,
                &GlobalCancellation,
                &Policy::trusted(),
                None
            )
            .await,
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn write_and_edit_leave_no_temp_files_and_round_trip() {
        // Own subdirectory: other tests write into `target/` concurrently, and
        // their in-flight `.dex-write-*` temp files would race this scan.
        let dir = "target/dex-atomic-write-test";
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join(dir)).unwrap();
        let rel = "target/dex-atomic-write-test/file.txt";
        let full = cwd.join(rel);
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("first\n".into()));
        assert!(execute(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None
        )
        .await
        .is_ok());
        // Content round-trips and no `.dex-write-*` temp file survives.
        assert_eq!(fs::read_to_string(&full).unwrap(), "first\n");
        args.insert("oldText".into(), Value::String("first".into()));
        args.insert("newText".into(), Value::String("second".into()));
        assert!(
            execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
                .await
                .is_ok()
        );
        assert_eq!(fs::read_to_string(&full).unwrap(), "second\n");
        let strays: Vec<_> = fs::read_dir(cwd.join(dir))
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".dex-write-"))
            })
            .collect();
        assert!(
            strays.is_empty(),
            "temp files must be renamed away, not left"
        );
        let _ = fs::remove_dir_all(cwd.join(dir));
    }

    /// `then_run` runs after a successful mutation and its output
    /// arrives in the same tool result, so the model learns the verification
    /// outcome without a second round-trip that re-sends the whole prefix.
    #[cfg(unix)]
    #[tokio::test]
    async fn then_run_streams_verification_into_the_same_result() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-test.txt";
        let full = cwd.join(rel);
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("verified\n".into()));
        args.insert("then_run".into(), Value::String(format!("cat {rel}")));
        let out = execute(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("wrote"), "{out}");
        assert!(
            out.contains("[then_run:succeeded] cat target/dex-then-run-test.txt"),
            "{out}"
        );
        // The command saw the new content: it runs after the write, not before.
        assert!(out.trim_end().ends_with("verified"), "{out}");
        let _ = fs::remove_file(&full);
    }

    /// A failing command is not a failed tool call: the write landed, and the
    /// model needs both facts — the mutation and the exit status — not an
    /// `Error:` that hides which half of the call did what.
    #[cfg(unix)]
    #[tokio::test]
    async fn then_run_failure_is_reported_in_band() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-fail.txt";
        let full = cwd.join(rel);
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("x\n".into()));
        args.insert("then_run".into(), Value::String("echo boom; exit 3".into()));
        let out = execute(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .expect("the write succeeded; the command's exit must not fail the call");
        assert!(out.contains("[then_run:failed (exit 3)]"), "{out}");
        assert!(out.contains("boom"), "{out}");
        let _ = fs::remove_file(&full);
    }

    /// A failed mutation never runs the command: a stale verification output
    /// must not reach the model attached to a change that never landed.
    #[tokio::test]
    async fn failed_mutation_skips_then_run() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-skip.txt";
        let full = cwd.join(rel);
        fs::write(&full, "present\n").unwrap();
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("absent\n".into()));
        args.insert("newText".into(), Value::String("never\n".into()));
        args.insert(
            "then_run".into(),
            Value::String("echo ran > target/dex-then-run-skip-ran.txt".into()),
        );
        let error = execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .expect_err("oldText is absent");
        assert!(matches!(error, ToolError::InvalidArgument(_)), "{error}");
        assert!(
            !cwd.join("target/dex-then-run-skip-ran.txt").exists(),
            "the command must not run when the mutation failed"
        );
        assert_eq!(fs::read_to_string(&full).unwrap(), "present\n");
        let _ = fs::remove_file(&full);
    }

    /// `then_run` must clear the *shell* gate, not the write gate:
    /// `ask-shell` passes file mutations unprompted, so otherwise an `edit` with
    /// `then_run` would be a way to run a command with no approval at all. No console is
    /// attached here, so a shell requirement surfaces as a denial.
    #[tokio::test]
    async fn then_run_needs_the_shell_gate() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-gate.txt";
        let full = cwd.join(rel);
        fs::write(&full, "before\n").unwrap();
        let policy = Policy {
            mode: PermissionMode::AskShell,
            console: None,
            agent: None,
        };
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("before".into()));
        args.insert("newText".into(), Value::String("after".into()));
        assert!(
            execute("edit", &args, &GlobalCancellation, &policy, None)
                .await
                .is_ok(),
            "a plain mutation is exactly what ask-shell permits"
        );
        args.insert("then_run".into(), Value::String("echo hi".into()));
        let error = execute("edit", &args, &GlobalCancellation, &policy, None)
            .await
            .expect_err("a then_run shell command is not covered by the write gate");
        assert!(matches!(error, ToolError::Denied(_)), "{error}");
        assert_eq!(
            fs::read_to_string(&full).unwrap(),
            "after\n",
            "the denial must land before the mutation, not after it"
        );
        let _ = fs::remove_file(&full);
    }

    /// Only write/edit take `then_run`; an unusable value fails loudly rather
    /// than doing nothing while the model reads the mutation as verified.
    #[test]
    fn then_run_command_scope() {
        let mut args = Map::new();
        args.insert("then_run".into(), Value::String("   ".into()));
        assert_eq!(then_run_command("write", &args).unwrap(), None);
        args.insert("then_run".into(), Value::String(" cargo test ".into()));
        assert_eq!(
            then_run_command("write", &args).unwrap(),
            Some("cargo test")
        );
        assert_eq!(then_run_command("edit", &args).unwrap(), Some("cargo test"));
        // Other tools ignore it entirely: the field is not theirs.
        assert_eq!(then_run_command("bash", &args).unwrap(), None);
        assert_eq!(then_run_command("read", &args).unwrap(), None);
        // `null` is "absent" — some clients serialize omitted optionals that way.
        args.insert("then_run".into(), Value::Null);
        assert_eq!(then_run_command("write", &args).unwrap(), None);
        // A structured value is a caller mistake, not a request to skip.
        args.insert("then_run".into(), json!({ "command": "cargo test" }));
        assert!(matches!(
            then_run_command("write", &args),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    /// An agent allowlisting `edit` but not `bash` must not gain shell through
    /// the `then_run` command: the same boundary the permission gate enforces,
    /// one layer down at the child's tool set.
    #[tokio::test]
    async fn then_run_command_respects_a_child_tool_allowlist() {
        let filter = ToolFilter {
            owner: "child".to_string(),
            allowed: BTreeSet::from(["edit".to_string()]),
        };
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-filter.txt";
        let full = cwd.join(rel);
        fs::write(&full, "before\n").unwrap();
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("before".into()));
        args.insert("newText".into(), Value::String("after".into()));
        args.insert("then_run".into(), Value::String("echo hi".into()));
        let error = execute(
            "edit",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .expect_err("bash is not in the child's allowlist");
        assert!(matches!(error, ToolError::Denied(_)), "{error}");
        assert!(error.to_string().contains("bash"), "{error}");
        assert_eq!(fs::read_to_string(&full).unwrap(), "before\n");
        // The same call without `then_run` is exactly what the filter allows.
        args.remove("then_run");
        assert!(
            execute(
                "edit",
                &args,
                &GlobalCancellation,
                &Policy::trusted(),
                Some(&filter)
            )
            .await
            .is_ok(),
            "a plain edit is within the child's tools"
        );
        let _ = fs::remove_file(&full);
    }

    /// A malformed `then_run` fails before the mutation runs, so the model can
    /// never mistake an unusable field for a check that came back clean.
    #[tokio::test]
    async fn malformed_then_run_does_not_mutate() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-then-run-malformed.txt";
        let full = cwd.join(rel);
        let _ = fs::remove_file(&full);
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("x\n".into()));
        args.insert("then_run".into(), json!({ "command": "cargo test" }));
        let error = execute(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .expect_err("an object then_run is rejected");
        assert!(matches!(error, ToolError::InvalidArgument(_)), "{error}");
        assert!(!full.exists(), "the write must not have happened");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_executable_bit() {
        use std::os::unix::fs::PermissionsExt as _;
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-atomic-mode-test.sh";
        let full = cwd.join(rel);
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("#!/bin/sh\n".into()));
        assert!(execute(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None
        )
        .await
        .is_ok());
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
        args.insert("oldText".into(), Value::String("#!/bin/sh".into()));
        args.insert("newText".into(), Value::String("#!/bin/sh\necho hi".into()));
        assert!(
            execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
                .await
                .is_ok()
        );
        let mode = std::fs::metadata(&full).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "edit must not strip +x");
        let _ = fs::remove_file(&full);
    }

    #[test]
    fn conflict_paths_normalize_spellings() {
        // `./target` and `target` are the same directory; an absolute and a
        // relative spelling of one file must collide too.
        let cwd = std::env::current_dir().unwrap();
        let rel = "./target";
        assert_eq!(
            normalize_conflict_path(rel),
            normalize_conflict_path("target")
        );
        let abs = cwd.join("target").display().to_string();
        assert_eq!(
            normalize_conflict_path(&abs),
            normalize_conflict_path("target")
        );
        // Lexical fallback: new files in not-yet-existing dirs still
        // collide across `./` spellings without touching the FS.
        assert_eq!(
            normalize_conflict_path("./newdir-dex-test/f.rs"),
            normalize_conflict_path("newdir-dex-test/f.rs")
        );
        assert_eq!(
            normalize_conflict_path("a/../newdir-dex-test/f.rs"),
            normalize_conflict_path("newdir-dex-test/f.rs")
        );
        // Outside-workspace absolute paths clean lexically instead of
        // crashing the check.
        assert_eq!(
            normalize_conflict_path("/definitely/not/here"),
            "/definitely/not/here"
        );
    }

    #[tokio::test]
    async fn edit_requires_exactly_one_match() {
        assert!(matches!(
            apply_edit("a a", "a", "b", false),
            Err(ToolError::EditNotUnique(2))
        ));
        let (updated, _) = apply_edit("a", "a", "b", false).unwrap();
        assert_eq!(updated, "b");
        assert!(matches!(
            apply_edit("a", "x", "b", false),
            Err(ToolError::InvalidArgument(_))
        ));
    }

    #[tokio::test]
    async fn edit_replace_all_replaces_every_occurrence() {
        let (updated, note) = apply_edit("a b a", "a", "c", true).unwrap();
        assert_eq!(updated, "c b c");
        assert!(note.contains("2 occurrences"), "{note}");
        // Without replaceAll the duplicate is an error, not a silent partial.
        assert!(apply_edit("a b a", "a", "c", false).is_err());
    }

    #[tokio::test]
    async fn write_edit_require_expected_hash_and_reject_stale() {
        // Real workspace file under target/ (inside cwd, cleaned up).
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let path = cwd.join("target/dex-stale-test.txt");
        fs::write(&path, "v1\n").unwrap();
        let rel = "target/dex-stale-test.txt";
        let h = hash_file(&path.display().to_string());

        // Correct expected_hash: edit applies.
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("v1".into()));
        args.insert("newText".into(), Value::String("v2".into()));
        args.insert("expected_hash".into(), Value::String(h.clone()));
        assert!(
            execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
                .await
                .is_ok()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

        // Stale expected_hash: rejected, file untouched.
        let mut stale = Map::new();
        stale.insert("path".into(), Value::String(rel.into()));
        stale.insert("oldText".into(), Value::String("v2".into()));
        stale.insert("newText".into(), Value::String("v3".into()));
        stale.insert("expected_hash".into(), Value::String("deadbeef".into()));
        assert!(matches!(
            execute(
                "edit",
                &stale,
                &GlobalCancellation,
                &Policy::trusted(),
                None
            )
            .await,
            Err(ToolError::StaleFile { .. })
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

        // hash_file is deterministic.
        assert_eq!(
            hash_file(&path.display().to_string()),
            hash_file(&path.display().to_string())
        );
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn change_diff_shows_unified_diff_for_write_and_edit() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let path = cwd.join("target/dex-preview-test.txt");
        fs::write(&path, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n").unwrap();
        let rel = "target/dex-preview-test.txt";
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("l5\n".into()));
        args.insert("newText".into(), Value::String("L5\nL5b\n".into()));
        let diff = change_diff_async("edit", &args).await.unwrap();
        assert!(diff.contains("--- a/target/dex-preview-test.txt"), "{diff}");
        assert!(diff.contains("+++ b/target/dex-preview-test.txt"), "{diff}");
        assert!(diff.contains("@@"), "{diff}");
        assert!(diff.contains("-l5"), "{diff}");
        assert!(diff.contains("+L5b"), "{diff}");
        // Context lines surround the change (4 radius) and are untouched.
        assert!(diff.lines().any(|l| l == " l4"), "{diff}");
        assert!(diff.lines().any(|l| l == " l9"), "{diff}");
        // Lines beyond the 4-line context radius stay outside the hunks.
        assert!(!diff.contains(" l10\n"), "{diff}");
        let _ = fs::remove_file(&path);
    }

    #[tokio::test]
    async fn change_diff_new_file_uses_dev_null_header() {
        let cwd = std::env::current_dir().unwrap();
        let rel = "target/dex-preview-new.txt";
        let _ = fs::remove_file(cwd.join(rel));
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("content".into(), Value::String("hello\n".into()));
        let diff = change_diff_async("write", &args).await.unwrap();
        assert!(diff.contains("--- /dev/null"), "{diff}");
        assert!(diff.contains("+++ b/target/dex-preview-new.txt"), "{diff}");
        assert!(diff.contains("+hello"), "{diff}");
    }

    #[tokio::test]
    async fn execute_outcome_carries_diff_for_edit() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-outcome-diff-test.txt";
        fs::write(cwd.join(rel), "a\nb\nc\n").unwrap();
        let mut args = Map::new();
        args.insert("path".into(), Value::String(rel.into()));
        args.insert("oldText".into(), Value::String("b\n".into()));
        args.insert("newText".into(), Value::String("B\n".into()));
        let outcome =
            execute_outcome("edit", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(outcome.ok, "{}", outcome.text);
        let diff = outcome.diff.expect("edit outcome carries a diff");
        assert!(diff.contains("-b"), "{diff}");
        assert!(diff.contains("+B"), "{diff}");
        let _ = fs::remove_file(cwd.join(rel));
    }

    #[tokio::test]
    async fn edit_fuzzy_fallback_handles_whitespace_drift() {
        // oldText with different indentation still matches exactly one
        // line-window and replaces whole lines.
        let content = "fn main() {\n    let x = 1;\n    println!(x);\n}\n";
        let old = "let x = 1;\nprintln!(x);";
        let new = "let y = 2;\nprintln!(y);";
        let (updated, note) = apply_edit(content, old, new, false).unwrap();
        assert!(updated.contains("let y = 2;"), "{updated}");
        assert!(updated.contains("    println!(y);"), "{updated}");
        assert!(note.contains("whitespace-insensitive"), "{note}");
        // Ambiguous fuzzy matches are rejected, not guessed.
        assert!(apply_edit("a\nb\na\nb", "a\nb", "c", false).is_err());
        // A genuinely absent match reports actionable guidance.
        let err = apply_edit("x", "nope", "c", false).unwrap_err();
        assert!(err.to_string().contains("read the file"), "{err}");
    }

    #[tokio::test]
    async fn edit_fuzzy_fallback_folds_lookalike_characters() {
        // Smart quotes, em-dashes, and non-breaking spaces in the file match
        // their ASCII equivalents in oldText; unchanged lines keep original bytes.
        let content = "let a = \u{2018}hi\u{2019};\n// a \u{2014} b\nx =\u{00A0}1;\n";
        let (updated, note) = apply_edit(
            content,
            "let a = 'hi';\n// a - b",
            "let a = 'yo';\n// a - b!",
            false,
        )
        .unwrap();
        assert!(updated.contains("let a = 'yo';"), "{updated}");
        assert!(updated.contains("// a - b!"), "{updated}");
        assert!(updated.contains("x =\u{00A0}1;\n"), "{updated}");
        assert!(note.contains("whitespace-insensitive"), "{note}");
    }

    #[tokio::test]
    async fn edit_fuzzy_fallback_preserves_crlf_outside_the_window() {
        // LF oldText matches a CRLF file's line window (with an indentation
        // and quote drift forcing the fuzzy path); the edit writes LF for
        // the replaced line but leaves surrounding CRLF bytes alone.
        let content = "a\r\n    say \u{2018}hi\u{2019}  \r\nc\r\n";
        let (updated, _) = apply_edit(content, "say 'hi'", "say 'yo'", false).unwrap();
        assert_eq!(updated, "a\r\n    say 'yo'\nc\r\n", "{updated:?}");
    }

    #[tokio::test]
    async fn edit_batch_applies_disjoint_edits_in_one_call() {
        let content = "alpha\nbeta\ngamma\ndelta\n";
        let ops = vec![
            ("alpha".to_string(), "ALPHA".to_string()),
            ("gamma\ndelta".to_string(), "GAMMA\nDELTA".to_string()),
        ];
        let (updated, note) = apply_edit_batch(content, &ops, false).unwrap();
        assert_eq!(updated, "ALPHA\nbeta\nGAMMA\nDELTA\n", "{updated:?}");
        assert!(note.contains("2 edits"), "{note}");
        assert!(note.contains("line 1"), "{note}");
        assert!(note.contains("lines 3-4"), "{note}");
    }

    #[tokio::test]
    async fn edit_batch_note_counts_fan_out_sites() {
        // replaceAll inside a batch: the note must not claim "2 edits" while
        // listing three spans — the site count is stated separately.
        let content = "a\nb\nb\n";
        let ops = vec![
            ("a".to_string(), "A".to_string()),
            ("b".to_string(), "B".to_string()),
        ];
        let (updated, note) = apply_edit_batch(content, &ops, true).unwrap();
        assert_eq!(updated, "A\nB\nB\n", "{updated:?}");
        assert!(note.contains("2 edits, 3 sites"), "{note}");
    }

    #[tokio::test]
    async fn edit_batch_rejects_non_object_entries_by_name() {
        // `edits: ["foo"]` used to report "missing oldText"; name the actual
        // shape problem so the model can self-correct.
        let mut args = Map::new();
        args.insert(
            "edits".into(),
            Value::Array(vec![Value::String("foo".into())]),
        );
        let error = parse_edit_ops(&args).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("edits[0] must be an object with oldText and newText"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn edit_batch_noop_error_names_the_entry() {
        let mut args = Map::new();
        args.insert(
            "edits".into(),
            Value::Array(vec![
                serde_json::json!({"oldText": "a", "newText": "A"}),
                serde_json::json!({"oldText": "same", "newText": "same"}),
            ]),
        );
        let error = parse_edit_ops(&args).unwrap_err();
        assert!(
            error.to_string().contains("edits[1]") && error.to_string().contains("identical"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn edit_batch_rejects_overlapping_entries() {
        let content = "one\ntwo\nthree\n";
        let ops = vec![
            ("one\ntwo".to_string(), "1\n2".to_string()),
            ("two\nthree".to_string(), "2\n3".to_string()),
        ];
        let err = apply_edit_batch(content, &ops, false).unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
        // No partial application: the error leaves the file untouched.
    }

    #[tokio::test]
    async fn edit_batch_arg_shapes_are_validated() {
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-edit-batch-shapes.txt";
        fs::write(cwd.join(rel), "aaa\nbbb\nccc\n").unwrap();
        let run = |args: Map<String, Value>| async move {
            execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None).await
        };
        // Mixing the two shapes fails loudly.
        let mut mixed = Map::new();
        mixed.insert("path".into(), Value::String(rel.into()));
        mixed.insert("oldText".into(), Value::String("aaa".into()));
        mixed.insert("newText".into(), Value::String("A".into()));
        mixed.insert(
            "edits".into(),
            Value::Array(vec![serde_json::json!({"oldText": "bbb", "newText": "B"})]),
        );
        assert!(run(mixed).await.is_err());
        // A disjoint batch applies end to end through `execute`.
        let mut batched = Map::new();
        batched.insert("path".into(), Value::String(rel.into()));
        batched.insert(
            "edits".into(),
            Value::Array(vec![
                serde_json::json!({"oldText": "aaa", "newText": "A"}),
                serde_json::json!({"oldText": "ccc", "newText": "C"}),
            ]),
        );
        let out = run(batched).await.expect("disjoint batch applies");
        assert!(out.contains("2 edits"), "{out}");
        assert_eq!(fs::read_to_string(cwd.join(rel)).unwrap(), "A\nbbb\nC\n");
        // replaceAll fans out across a batch too: every entry's matches
        // are collected up front and overlap-checked before anything applies.
        let mut batched_all = Map::new();
        batched_all.insert("path".into(), Value::String(rel.into()));
        batched_all.insert(
            "edits".into(),
            Value::Array(vec![serde_json::json!({"oldText": "b", "newText": "B"})]),
        );
        batched_all.insert("replaceAll".into(), Value::Bool(true));
        run(batched_all).await.expect("batch replaceAll applies");
        assert_eq!(fs::read_to_string(cwd.join(rel)).unwrap(), "A\nBBB\nC\n");
        let _ = fs::remove_file(cwd.join(rel));
    }

    #[tokio::test]
    async fn temporary_workspace_paths_are_confined() {
        let root = std::env::temp_dir().join(format!("dex-workspace-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert!(resolve_workspace_path(&root, "inside.txt")
            .unwrap()
            .starts_with(&root));
        assert!(matches!(
            resolve_workspace_path(&root, "../outside.txt"),
            Err(crate::workspace::WorkspaceError::OutsideWorkspace(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shell_timeout_terminates_long_running_command() {
        let (result, code) = run_bash_with_limits(
            "sleep 1",
            Duration::from_millis(10),
            1024,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert!(result.contains("timed out"));
        assert_eq!(code, None);
    }

    #[tokio::test]
    async fn shell_exit_code_is_reported_separately_from_output() {
        let (output, code) = run_bash_with_limits(
            "echo partial-results; exit 3",
            Duration::from_secs(5),
            1024,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert_eq!(code, Some(3));
        assert_eq!(output, "partial-results\n");
    }

    #[tokio::test]
    async fn stderr_is_labeled_and_capture_limit_is_marked() {
        let (output, _) = run_bash_with_limits(
            "echo out; echo err 1>&2",
            Duration::from_secs(5),
            1024,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert!(output.contains("out\n"), "{output:?}");
        assert!(output.contains("--- stderr ---\nerr"), "{output:?}");

        let (clipped, _) =
            run_bash_with_limits("seq 1 100", Duration::from_secs(5), 16, &GlobalCancellation)
                .await
                .unwrap();
        assert!(clipped.contains("capture limit"), "{clipped:?}");
    }

    #[tokio::test]
    async fn read_is_line_numbered_and_paginates() {
        // Fixtures live under target/ so the workspace path confinement
        // accepts them (and the directory is already ignored).
        let root = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("dex-read-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.txt");
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let mut args = Map::new();
        args.insert("path".into(), Value::String(path.display().to_string()));
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(outcome.ok, "{}", outcome.text);
        // Line numbers are now right-aligned with two spaces (no raw tab) and file
        // tabs are expanded per tab_width, so the separator is stable.
        assert_eq!(
            outcome.text,
            "   1  one\n   2  two\n   3  three\n   4  four"
        );

        args.insert("offset".into(), Value::Number(2.into()));
        args.insert("limit".into(), Value::Number(1.into()));
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert_eq!(outcome.text, "   2  two");

        args.insert("offset".into(), Value::Number(9.into()));
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(!outcome.ok, "offset past end must fail: {}", outcome.text);

        // Binary content is refused instead of dumped into the context.
        fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        let mut args = Map::new();
        args.insert(
            "path".into(),
            Value::String(root.join("blob.bin").display().to_string()),
        );
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("binary file"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn read_fanout_reads_many_files_in_one_call() {
        // Fixtures live at the workspace root (not target/) because search
        // and glob rules deliberately exclude target/.
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fanout-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "alpha\n").unwrap();
        fs::write(root.join("b.txt"), "beta\n").unwrap();

        // Explicit list; a missing file is isolated, not fatal.
        let mut args = Map::new();
        args.insert(
            "paths".into(),
            Value::Array(vec![
                Value::String(root.join("a.txt").display().to_string()),
                Value::String(root.join("missing.txt").display().to_string()),
                Value::String(root.join("b.txt").display().to_string()),
            ]),
        );
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("==> "), "{}", outcome.text);
        assert!(outcome.text.contains("   1  alpha"), "{}", outcome.text);
        assert!(outcome.text.contains("   1  beta"), "{}", outcome.text);
        assert!(outcome.text.contains("error:"), "{}", outcome.text);

        // Glob fan-out, sorted, capped.
        let mut args = Map::new();
        args.insert("glob".into(), Value::String("*.txt".into()));
        let outcome =
            execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("a.txt"), "{}", outcome.text);
        assert!(outcome.text.contains("b.txt"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ffgrep_truncation_trailer_counts_shown_and_how_to_continue() {
        // Truncation must read like read's trailer: what was shown, that more
        // was left unscanned, and the exact next-page argument.
        let pid = std::process::id();
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fff-page-{pid}"));
        fs::create_dir_all(&root).unwrap();
        let needle = format!("PAGETOKEN_{pid}");
        for f in 0..6 {
            let body: String = (0..20).map(|_| format!("{needle}\n")).collect();
            fs::write(root.join(format!("f{f}.rs")), body).unwrap();
        }
        super::search::rescan();

        // Files mode: 3 of 6 files shown, next page starts at file_offset 3.
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle.clone()));
        args.insert("head_limit".into(), Value::Number(3.into()));
        let outcome = execute_outcome(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("3 files shown"), "{}", outcome.text);
        assert!(
            outcome
                .text
                .contains("continue with file_offset 3 or raise head_limit"),
            "{}",
            outcome.text
        );

        // Content mode resumes from that offset: pages 4 and 5, each capped
        // at 10 matches per file, under the default head_limit of 50.
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle.clone()));
        args.insert("output_mode".into(), Value::String("content".into()));
        args.insert("file_offset".into(), Value::Number(4.into()));
        let outcome = execute_outcome(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("every file hit the 10-match cap"),
            "{}",
            outcome.text
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ffgrep_context_returns_surrounding_lines() {
        // Fixture at the workspace root (not target/): fff respects
        // .gitignore, so ignored fixture dirs are invisible to it.
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fff-ctx-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let needle = format!("CTXNEEDLE_{}", std::process::id());
        fs::write(
            root.join("code.rs"),
            format!("top\nbefore\n{needle} here\nafter\nbottom\n"),
        )
        .unwrap();
        super::search::rescan();

        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle.clone()));
        args.insert("output_mode".into(), Value::String("content".into()));
        args.insert("context".into(), Value::Number(1.into()));
        let outcome = execute_outcome(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("before"), "{}", outcome.text);
        assert!(outcome.text.contains("after"), "{}", outcome.text);
        assert!(!outcome.text.contains("top"), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ffgrep_fuzzy_fallback_recovers_typos() {
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-fff-typo-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("thing.rs"),
            "struct UserAccountController { field: u32 }\n",
        )
        .unwrap();
        super::search::rescan();

        // Exact query misses (the token has a transposed 'lr'), fuzzy retry hits.
        // Assembled at runtime so the query text does not appear verbatim in
        // this source file — the exact search would hit this file otherwise.
        let mut args = Map::new();
        args.insert(
            "pattern".into(),
            Value::String(format!("UserAccountControlel{}", "r")),
        );
        args.insert("output_mode".into(), Value::String("content".into()));
        let outcome = execute_outcome(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("approximate"), "{}", outcome.text);
        assert!(
            outcome.text.contains("UserAccountController"),
            "{}",
            outcome.text
        );
        // Detail rows get their own lines, not glued to the path
        // (`thing.rs  1: …` would break the summary counter and preview).
        assert!(!outcome.text.contains("thing.rs  "), "{}", outcome.text);

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn chain_runs_search_then_reads_matched_files_in_one_call() {
        // Fixtures live at the workspace root (not target/): fff respects
        // .gitignore, so ignored fixture dirs are invisible to it.
        let needle = format!("TARGET_{}", "TOKEN");
        let root = std::env::current_dir()
            .unwrap()
            .join(format!("dex-chain-fx-{}", std::process::id()));
        // A previous run that panicked mid-assert leaked its fixture dir
        // (cleanup below only runs on the happy path); sweep those first.
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with("dex-chain-fx-") {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.rs"), format!("{needle} in one\n")).unwrap();
        fs::write(root.join("two.rs"), "nothing here\n").unwrap();
        super::search::rescan();

        let args = json!({
            "steps": [
                {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"limit": 10}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("--- step 0: ffgrep ---"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains("--- step 1: read ---"),
            "{}",
            outcome.text
        );
        assert!(
            outcome.text.contains(&format!("{needle} in one")),
            "{}",
            outcome.text
        );

        // Mutation and shell tools are refused inside chains.
        let args = json!({
            "steps": [
                {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
                {"tool": "bash", "args": {"command": "echo hi"}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("read-only"), "{}", outcome.text);

        // `from` must reference an earlier step.
        let args = json!({
            "steps": [
                {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
                {"tool": "read", "from": 1, "take": "paths"}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(outcome.text.contains("earlier step"), "{}", outcome.text);

        // A failing second step still ships the first step's output.
        let args = json!({
            "steps": [
                {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
                {"tool": "read", "from": 0, "take": "paths", "args": {"offset": 99}}
            ]
        })
        .as_object()
        .unwrap()
        .clone();
        let outcome = execute_outcome(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(!outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("step 0: ffgrep") && outcome.text.contains("step 1 failed"),
            "{}",
            outcome.text
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ffgrep_without_matches_is_success() {
        // Assembled at runtime so the needle does not appear in this source
        // file (the test greps the crate it lives in). Gibberish so the
        // fuzzy fallback has nothing approximate to land on either.
        let needle = format!("zxq{}wvut", std::process::id());
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String(needle));
        let outcome = execute_outcome(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(
            outcome.ok,
            "no matches (exact or fuzzy) must be ok: {}",
            outcome.text
        );
        assert!(
            outcome.text.contains("0 matches."),
            "no matches must report zero: {:?}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn fffind_finds_paths_fuzzily() {
        super::search::rescan();
        let mut args = Map::new();
        args.insert("pattern".into(), Value::String("tools mod".into()));
        let outcome = execute_outcome(
            "fffind",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            outcome.text.contains("src/tools/mod.rs"),
            "{}",
            outcome.text
        );
    }

    #[tokio::test]
    async fn failed_shell_command_keeps_output_and_exit_marker() {
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo boom; exit 2".into()));
        let outcome =
            execute_outcome("bash", &args, &GlobalCancellation, &Policy::trusted(), None).await;
        assert!(!outcome.ok);
        assert!(outcome.text.contains("boom"));
        assert!(outcome.text.contains("[exit 2]"));
    }

    #[tokio::test]
    async fn fanout_read_is_concurrent_ordered_and_isolated() {
        // TDD Phase 3 (S1): ≤10 files concurrent, join + sort to input order, budget on join, per-file errors isolated.
        let cwd = std::env::current_dir().unwrap();
        let dir = cwd.join("target/dex-fanout-test");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let mut rels = Vec::new();
        for i in 0..5 {
            let rel = format!("target/dex-fanout-test/f{i}.txt");
            tokio::fs::write(cwd.join(&rel), format!("content-{i}\nline2\n"))
                .await
                .unwrap();
            rels.push(rel);
        }
        // One missing file among good ones: isolated, call still succeeds.
        rels.push("target/dex-fanout-test/missing-xyz.txt".to_string());
        let mut args = Map::new();
        args.insert(
            "paths".to_string(),
            serde_json::Value::Array(
                rels.iter()
                    .map(|r| serde_json::Value::String(r.clone()))
                    .collect(),
            ),
        );
        // Use workspace_path resolution via execute (paths confined).
        let out = execute("read", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .unwrap();
        // All good files present, in input order (==> path <== sections sorted by input, not completion).
        let mut last_pos = 0;
        for i in 0..5 {
            let marker = format!("f{i}.txt");
            let pos = out.find(&marker).expect("each file present");
            assert!(pos >= last_pos, "fan-out must preserve input order");
            last_pos = pos;
            assert!(out.contains(&format!("content-{i}")));
        }
        assert!(
            out.contains("missing-xyz"),
            "per-file errors isolated, not fatal"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn bash_timeout_kills_and_reports_exactly() {
        // TDD Phase 3 (S5): timeout exact, not 25ms-quantized; [exit N] reporting preserved.
        let (out, code) = run_bash_with_limits(
            "sleep 5; echo never",
            Duration::from_millis(80),
            4096,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert_eq!(code, None);
        assert!(
            out.contains("timed out after 0 seconds") || out.contains("timed out"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn bash_cancel_is_prompt_not_poll_quantized() {
        // TDD Phase 3: cancel via select!, not 25ms poll.
        use crate::runtime::console::CancellationToken;
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            t2.cancel();
        });
        let start = std::time::Instant::now();
        let (out, code) =
            run_bash_with_limits("sleep 5; echo never", Duration::from_secs(10), 4096, &token)
                .await
                .unwrap();
        assert_eq!(code, None);
        assert!(out.contains("cancelled"), "{out}");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancel must preempt sleep without 25ms quanta pile-up"
        );
    }

    #[tokio::test]
    async fn cancelled_tool_reports_error_outcome_for_loop_suppression() {
        // Producer side of the cancel-during-IO contract: a fired token
        // turns the tool into ok:false, so the loop neither caches nor
        // replays it — and `execute_outcome` never derives success from text.
        use crate::runtime::console::CancellationToken;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut args = serde_json::Map::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("sleep 30".to_string()),
        );
        let outcome = execute_outcome("bash", &args, &cancel, &Policy::trusted(), None).await;
        assert!(!outcome.ok);
        assert!(outcome.text.contains("cancelled"), "{}", outcome.text);
    }

    #[tokio::test]
    async fn bash_exit_code_and_stderr_label_preserved() {
        let (out, code) = run_bash_with_limits(
            "echo out; echo err >&2; exit 3",
            Duration::from_secs(5),
            4096,
            &GlobalCancellation,
        )
        .await
        .unwrap();
        assert_eq!(code, Some(3));
        // tool_bash maps non-zero to Shell error with clamp + [exit N] via Display; direct run returns output + code.
        assert!(out.contains("out"));
        assert!(out.contains("--- stderr ---"));
        assert!(out.contains("err"));
        // Clamp preserved via tool_bash
        let mut args = Map::new();
        args.insert(
            "command".into(),
            serde_json::Value::String("echo hi".into()),
        );
        let ok = execute("bash", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .unwrap();
        assert!(ok.contains("hi"));
    }

    #[tokio::test]
    async fn read_pagination_and_binary_refusal() {
        let cwd = std::env::current_dir().unwrap();
        let rel = "target/dex-read-pag-test.txt";
        let content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        tokio::fs::create_dir_all(cwd.join("target")).await.unwrap();
        tokio::fs::write(cwd.join(rel), &content).await.unwrap();
        let mut args = Map::new();
        args.insert("path".into(), serde_json::Value::String(rel.into()));
        args.insert("offset".into(), serde_json::Value::from(3u64));
        args.insert("limit".into(), serde_json::Value::from(2u64));
        let out = execute("read", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .unwrap();
        assert!(out.contains("3"), "{out}");
        assert!(out.contains("line3") && out.contains("line4"));
        assert!(!out.contains("line5"));
        // Binary refused, not dumped
        let bin_rel = "target/dex-read-bin-test.bin";
        tokio::fs::write(cwd.join(bin_rel), vec![0u8, 1, 2, 3])
            .await
            .unwrap();
        let mut bargs = Map::new();
        bargs.insert("path".into(), serde_json::Value::String(bin_rel.into()));
        let err = execute(
            "read",
            &bargs,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("binary"), "{err}");
        let _ = tokio::fs::remove_file(cwd.join(rel)).await;
        let _ = tokio::fs::remove_file(cwd.join(bin_rel)).await;
    }

    #[tokio::test]
    async fn chain_refuses_mutating_steps() {
        let mut args = Map::new();
        args.insert(
            "steps".into(),
            serde_json::Value::Array(vec![
                serde_json::json!({"tool": "read", "args": {"path": "Cargo.toml"}}),
                serde_json::json!({"tool": "bash", "args": {"command": "echo hi"}}),
            ]),
        );
        let err = execute(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    }

    // Phase 0 gate tests: dispatch consults the turn policy, parks an
    // approval prompt for mutating calls under ask modes, and blocks for
    // the verdict. Files land under `target/` (unique per test) with
    // best-effort cleanup, mirroring the existing tests in this module.
    use crate::core::types::{ApprovalDecision, PermissionMode};
    use crate::runtime::console::Console;

    fn phase0_console() -> (
        Console,
        tokio::sync::mpsc::Receiver<crate::core::types::ApprovalRequest>,
    ) {
        let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel::<crate::core::types::SinkLine>(16);
        let (approval_tx, approval_rx) =
            tokio::sync::mpsc::channel::<crate::core::types::ApprovalRequest>(16);
        (Console::new(sink_tx, approval_tx), approval_rx)
    }

    fn phase0_write_args(path: &str) -> Map<String, Value> {
        let mut args = Map::new();
        args.insert("path".into(), Value::String(path.into()));
        args.insert("content".into(), Value::String("phase0\n".into()));
        args
    }

    #[tokio::test]
    async fn ask_writes_parks_write_and_blocks_until_allowed() {
        let rel = "target/phase0-allow.txt";
        let _ = fs::remove_file(rel);
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskWrites, &console);
        let args = phase0_write_args(rel);
        let expected_input = Value::Object(args.clone()).to_string();
        let mut handle = tokio::spawn(async move {
            execute_outcome("write", &args, &GlobalCancellation, &policy, None).await
        });
        // The call blocks: no outcome before the verdict …
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut handle)
                .await
                .is_err(),
            "write must block for approval under ask-writes"
        );
        // … and exactly one prompt is parked.
        let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("approval prompt must arrive")
            .expect("approval channel must stay open");
        assert_eq!(request.name, "write");
        assert_eq!(request.input, expected_input);
        request
            .response
            .send(ApprovalDecision::Once)
            .await
            .expect("agent must still be waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("verdict must unblock the call")
            .expect("worker panicked");
        assert!(outcome.ok, "{}", outcome.text);
        assert_eq!(fs::read_to_string(rel).unwrap(), "phase0\n");
        assert!(
            approval_rx.try_recv().is_err(),
            "exactly one prompt must be parked"
        );
        let _ = fs::remove_file(rel);
    }

    #[tokio::test]
    async fn ask_writes_deny_blocks_the_write() {
        let rel = "target/phase0-deny.txt";
        let _ = fs::remove_file(rel);
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskWrites, &console);
        let args = phase0_write_args(rel);
        let handle = tokio::spawn(async move {
            execute_outcome("write", &args, &GlobalCancellation, &policy, None).await
        });
        let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("approval prompt must arrive")
            .expect("approval channel must stay open");
        request
            .response
            .send(ApprovalDecision::Deny)
            .await
            .expect("agent must still be waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("verdict must unblock the call")
            .expect("worker panicked");
        assert!(!outcome.ok);
        assert!(outcome.text.contains("denied"), "{}", outcome.text);
        assert!(!std::path::Path::new(rel).exists(), "deny must not write");
    }

    #[tokio::test]
    async fn ask_writes_allow_session_skips_the_second_prompt() {
        let rel = "target/phase0-session.txt";
        let _ = fs::remove_file(rel);
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskWrites, &console);
        // First identical call prompts; deny it to release the worker.
        let args = phase0_write_args(rel);
        let policy2 = policy.clone();
        let first = tokio::spawn(async move {
            execute_outcome("write", &args, &GlobalCancellation, &policy2, None).await
        });
        let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("approval prompt must arrive")
            .expect("approval channel must stay open");
        request
            .response
            .send(ApprovalDecision::Deny)
            .await
            .expect("agent must still be waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(10), first)
            .await
            .expect("verdict must unblock the call")
            .expect("worker panicked");
        assert!(!outcome.ok);
        assert!(!std::path::Path::new(rel).exists(), "deny must not write");
        // Second identical call prompts again; allow for session.
        let args2 = phase0_write_args(rel);
        let policy3 = policy.clone();
        let second = tokio::spawn(async move {
            execute_outcome("write", &args2, &GlobalCancellation, &policy3, None).await
        });
        let request2 = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("second prompt must arrive")
            .expect("approval channel must stay open");
        request2
            .response
            .send(ApprovalDecision::Session)
            .await
            .expect("agent must still be waiting");
        let outcome2 = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("verdict must unblock the call")
            .expect("worker panicked");
        assert!(outcome2.ok, "{}", outcome2.text);
        // Same-turn repeat: no new prompt, straight through.
        let args3 = phase0_write_args(rel);
        let outcome3 = tokio::time::timeout(
            Duration::from_secs(10),
            execute_outcome("write", &args3, &GlobalCancellation, &policy, None),
        )
        .await
        .expect("session-approved call must not block");
        assert!(outcome3.ok, "{}", outcome3.text);
        assert!(
            approval_rx.try_recv().is_err(),
            "no further prompt after allow-for-session"
        );
        let _ = fs::remove_file(rel);
    }

    #[tokio::test]
    async fn read_only_rejects_write_without_prompt() {
        let rel = "target/phase0-readonly.txt";
        let _ = fs::remove_file(rel);
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::ReadOnly, &console);
        let args = phase0_write_args(rel);
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            execute_outcome("write", &args, &GlobalCancellation, &policy, None),
        )
        .await
        .expect("read-only rejection must not block");
        assert!(!outcome.ok);
        assert!(outcome.text.contains("read-only"), "{}", outcome.text);
        assert!(
            approval_rx.try_recv().is_err(),
            "read-only must reject, never prompt"
        );
        assert!(!std::path::Path::new(rel).exists());
    }

    #[tokio::test]
    async fn trusted_runs_mutations_with_no_channel() {
        let rel = "target/phase0-trusted.txt";
        let _ = fs::remove_file(rel);
        let args = phase0_write_args(rel);
        let outcome = execute_outcome(
            "write",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await;
        assert!(outcome.ok, "{}", outcome.text);
        assert_eq!(fs::read_to_string(rel).unwrap(), "phase0\n");
        let _ = fs::remove_file(rel);
    }

    #[tokio::test]
    async fn ask_shell_permits_write_but_prompts_shell() {
        // File mutations need no prompt under ask-shell …
        let rel = "target/phase0-askshell.txt";
        let _ = fs::remove_file(rel);
        let policy = Policy::turn(PermissionMode::AskShell, &Console::none());
        let args = phase0_write_args(rel);
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            execute_outcome("write", &args, &GlobalCancellation, &policy, None),
        )
        .await
        .expect("ask-shell write must not block");
        assert!(outcome.ok, "{}", outcome.text);
        let _ = fs::remove_file(rel);
        // … while shell still parks exactly one prompt.
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskShell, &console);
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo phase0".into()));
        let handle = tokio::spawn(async move {
            execute_outcome("bash", &args, &GlobalCancellation, &policy, None).await
        });
        let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("shell prompt must arrive")
            .expect("approval channel must stay open");
        assert_eq!(request.name, "bash");
        request
            .response
            .send(ApprovalDecision::Once)
            .await
            .expect("agent must still be waiting");
        let outcome = tokio::time::timeout(Duration::from_secs(15), handle)
            .await
            .expect("verdict must unblock the call")
            .expect("worker panicked");
        assert!(outcome.ok, "{}", outcome.text);
        assert!(
            approval_rx.try_recv().is_err(),
            "exactly one prompt must be parked"
        );
    }

    #[tokio::test]
    async fn approval_wait_unwinds_on_cancel() {
        use crate::runtime::console::CancellationToken;
        let rel = "target/phase0-cancel.txt";
        let _ = fs::remove_file(rel);
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskWrites, &console);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        let args = phase0_write_args(rel);
        let handle =
            tokio::spawn(
                async move { execute_outcome("write", &args, &cancel2, &policy, None).await },
            );
        // Wait for the parked prompt, then cancel instead of answering.
        let _request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .expect("approval prompt must arrive")
            .expect("approval channel must stay open");
        cancel.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
            .await
            .expect("cancel must unblock the parked call")
            .expect("worker panicked");
        assert!(!outcome.ok);
        assert!(outcome.text.contains("cancelled"), "{}", outcome.text);
        assert!(!std::path::Path::new(rel).exists());
    }

    // Phase 2 (runtime extraction): the explicit ToolFilter allowlist,
    // enforced at dispatch. `None` preserves the parent path everywhere.
    #[test]
    fn tool_filter_matches_exact_and_wildcard_only() {
        let filter = ToolFilter::new("explorer", ["read", "ffgrep", "mcp__gh__*"]);
        assert!(filter.allows("read"));
        assert!(filter.allows("ffgrep"));
        assert!(filter.allows("mcp__gh__search"));
        assert!(!filter.allows("bash"));
        assert!(!filter.allows("mcp__other__tool"));
        assert!(!filter.allows("read_all"), "no accidental prefix match");
        assert!(!filter.allows("delegate"));
    }

    #[tokio::test]
    async fn filtered_out_tool_is_rejected_with_allowlist_error() {
        let filter = ToolFilter::new("explorer", ["read"]);
        let mut args = Map::new();
        args.insert("command".into(), Value::String("echo hi".into()));
        let err = execute(
            "bash",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("explorer"), "{err}");
        assert!(err.to_string().contains("allowlist"), "{err}");
        // An allowed tool still runs under the same filter.
        let mut args = Map::new();
        args.insert("path".into(), Value::String("Cargo.toml".into()));
        let out = execute(
            "read",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .unwrap();
        assert!(out.contains("dex"), "{out}");
    }

    #[tokio::test]
    async fn filtered_out_tool_rejects_before_approval_without_prompt() {
        // Filter runs before policy: a denied tool must fail closed with an
        // allowlist error and never park an approval prompt.
        let (console, mut approval_rx) = phase0_console();
        let policy = Policy::turn(PermissionMode::AskWrites, &console);
        let filter = ToolFilter::new("explorer", ["read"]);
        let args = phase0_write_args("target/phase0-filter-no-prompt.txt");
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            execute_outcome("write", &args, &GlobalCancellation, &policy, Some(&filter)),
        )
        .await
        .expect("filtered-out rejection must not block");
        assert!(!outcome.ok);
        assert!(outcome.text.contains("allowlist"), "{}", outcome.text);
        assert!(
            approval_rx.try_recv().is_err(),
            "filtered-out tool must reject, never prompt"
        );
        assert!(!std::path::Path::new("target/phase0-filter-no-prompt.txt").exists());
    }

    #[tokio::test]
    async fn unknown_tool_stays_unknown_under_filter() {
        let filter = ToolFilter::new("explorer", ["read"]);
        let args = Map::new();
        let err = execute(
            "nope",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown tool"), "{err}");
    }

    #[tokio::test]
    async fn chain_steps_run_under_the_same_filter() {
        // `chain` itself is allowed; its `read` step is not (chain's own
        // read-only gate rejects non-read steps before dispatch, so probe
        // the filter with a read step instead).
        let filter = ToolFilter::new("explorer", ["ffgrep", "chain"]);
        let mut args = Map::new();
        args.insert(
            "steps".into(),
            serde_json::Value::Array(vec![
                serde_json::json!({"tool": "ffgrep", "args": {"pattern": "dex"}}),
                serde_json::json!({"tool": "read", "args": {"path": "Cargo.toml"}}),
            ]),
        );
        let err = execute(
            "chain",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("allowlist"), "{err}");
    }
}
