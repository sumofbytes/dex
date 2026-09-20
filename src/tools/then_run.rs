//! `then_run` support: the verification command a `write`/`edit` call
//! carries, run after a successful mutation with the same clamping and
//! shell timeout as `bash`.

use serde_json::{Map, Value};

use crate::runtime::cancel::CancellationSource;
use crate::ui::format::{clamp_lines, clip_chars};

use super::error::ToolError;
use super::shell::{run_bash, BASH_CLAMP_BYTES, BASH_CLAMP_LINES};

/// Whether a parsed tool-call argument object carries an executable
/// `then_run` verification command. Single-sources the resolver in
/// `tools::then_run_command` (only a non-empty string runs a shell, only
/// `write`/`edit` accept the field) so the batch-conflict check here can
/// never drift from what dispatch actually runs.
pub(crate) fn carries_then_run(name: &str, value: &Value) -> bool {
    then_run_command(name, value.as_object().unwrap_or(&Map::new()))
        .unwrap_or(None)
        .is_some()
}

/// The `then_run` field (SoL-Pi-compatible): the verification command a
/// `write`/`edit` call carries. Only those two tools accept it; `null`, empty and whitespace-only
/// values all mean "no command" so an omitted optional field stays harmless.
/// A present-but-unusable value (object, array, number) is an error rather than
/// a silent no-op: a model guessing another harness's
/// `then_run: {command: …, timeout: …}` shape would otherwise read the
/// mutation's success as its own verification.
pub(crate) fn then_run_command<'a>(
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
pub(super) async fn append_then_run(
    mut text: String,
    command: &str,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> (String, Option<i32>) {
    let (output, code) = match run_bash(command, cancel).await {
        Ok(result) => result,
        Err(error) => {
            text.push_str(&format!(
                "\n\n[then_run:failed] {}\n(error: {error})",
                clip_command(command)
            ));
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
    let clamped = clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES);
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
