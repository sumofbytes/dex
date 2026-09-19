#![allow(clippy::doc_lazy_continuation)]
mod audit;
mod dispatch;
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
pub(crate) use dispatch::{dispatch_original, execute, execute_outcome, execute_sync};
pub(crate) use error::ToolError;
pub(crate) use outcome::{ShellEvidence, ToolOutcome};
pub(crate) use policy::{metadata, PermissionRequirement, ToolFilter};
pub(crate) use sandbox::{
    normalize_conflict_path, resolve_workspace_path, workspace_path, workspace_root,
};
// Leaf tool implementations live in per-tool modules; re-exported so
// existing `tools::...` paths keep working.
#[cfg(test)]
use edit::{apply_edit, apply_edit_batch, change_diff_async, parse_edit_ops};
pub(crate) use meta::Policy;
#[cfg(test)]
use read::expand_glob_in;
pub(crate) use shell::parse_shell_escape;
#[cfg(test)]
use shell::run_bash_with_limits;
use shell::{run_bash, BASH_CLAMP_BYTES, BASH_CLAMP_LINES};
pub(crate) use write::hash_file;

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::time::Duration;

use self::plan::{format_plan_snapshot, parse_plan_progress, parse_plan_steps};
use crate::runtime::cancel::CancellationSource;
use crate::ui::format::{clamp_lines_checked, clip_chars};

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

#[cfg(test)]
mod tests;
