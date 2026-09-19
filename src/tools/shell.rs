//! Shell execution: timeouts, process groups, output capture.

use std::env;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::AsyncReadExt as _;

use crate::runtime::cancel::{wait_cancelled, CancellationSource};
use crate::ui::format::clamp_lines_checked;

use super::then_run::arg_str;
use super::{ShellEvidence, ToolError, CONFIGURED_OUTPUT_LIMIT};

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setsid() -> i32;
}

#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Model-facing caps. Capture limits (1 MiB shell) guard memory; clamp
/// limits guard the context window. Head+tail clamping keeps both the
/// imports/context at the start and the errors/summaries at the end.
pub(crate) const BASH_CLAMP_LINES: usize = 400;
pub(crate) const BASH_CLAMP_BYTES: usize = 32 * 1024;

/// Maximum wall-clock duration for a shell command.
/// Configure with DEX_TOOL_TIMEOUT_SECS (default: 120).
/// Output is capped by DEX_TOOL_OUTPUT_BYTES (default: 1 MiB).
fn shell_timeout() -> Duration {
    env::var("DEX_TOOL_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(120))
}

/// Expose the running binary to shell commands as $DEX_BIN so a script can
/// call tools locally (`"$DEX_BIN" run read path=src/main.rs`) and stitch a
/// whole read-only pipeline in one call — intermediate output stays out of
/// the conversation and only the distilled result reaches the model.
/// ponytail: shell stitching only — if JSON routing in pipelines gets
/// painful, embed rquickjs and expose tools as functions (same $DEX_BIN mechanism).
fn tool_runner_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        env.push(("DEX_BIN".to_string(), exe.display().to_string()));
    }
    // git via bash without --no-pager can invoke delta/bat/less which
    // probe the terminal (OSC 10/11) and race crossterm for the reply;
    // force a non-interactive pager so the child never queries the pts.
    env.push(("GIT_PAGER".to_string(), "cat".to_string()));
    env.push(("PAGER".to_string(), "cat".to_string()));
    env
}

pub(crate) async fn run_bash(
    command: &str,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<(String, Option<i32>), ToolError> {
    let timeout = shell_timeout();
    let max_bytes = env::var("DEX_TOOL_OUTPUT_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or_else(|| CONFIGURED_OUTPUT_LIMIT.load(Ordering::Relaxed));
    run_bash_with_limits(command, timeout, max_bytes, cancel).await
}

async fn read_limited_async<R>(mut reader: R, limit: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buf = [0u8; 8192];
    loop {
        let want = (limit - bytes.len()).min(buf.len());
        if want == 0 {
            break;
        }
        match reader.read(&mut buf[..want]).await {
            Ok(0) | Err(_) => return (bytes, false),
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
        if bytes.len() >= limit {
            break;
        }
    }
    // Limit hit before EOF → truncated (sync version does an extra read to
    // distinguish exact-limit vs over-limit; here reaching the limit without
    // EOF is sufficient — exact-limit false-positives are harmless truncation
    // markers, not data loss).
    (bytes, true)
}

pub(crate) async fn run_bash_with_limits(
    command: &str,
    timeout: Duration,
    max_bytes: usize,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<(String, Option<i32>), ToolError> {
    // Async shell: `tokio::process` + async pipe drain (tasks, not threads) +
    // `select!(wait, timeout, cancelled)` — no reader threads, no 25ms poll
    // quantum (S5). Process-group kill preserved.
    let mut child = shell_command(command).spawn().map_err(ToolError::Io)?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    // Async drain tasks (replace the 2 reader threads).
    let out_h = tokio::spawn(read_limited_async(stdout, max_bytes.saturating_add(1)));
    let err_h = tokio::spawn(read_limited_async(stderr, max_bytes.saturating_add(1)));

    let status = tokio::select! {
        st = child.wait() => st.map_err(ToolError::Io)?,
        _ = tokio::time::sleep(timeout) => {
            kill_process_group_async(pid, &mut child).await;
            let _ = child.wait().await;
            let _ = out_h.await;
            let _ = err_h.await;
            return Ok((
                format!(
                    "Error: shell command timed out after {} seconds",
                    timeout.as_secs()
                ),
                None,
            ));
        }
        _ = wait_cancelled(cancel) => {
            kill_process_group_async(pid, &mut child).await;
            let _ = child.wait().await;
            let _ = out_h.await;
            let _ = err_h.await;
            return Ok(("Error: shell command cancelled".to_string(), None));
        }
    };
    let (mut stdout, stdout_truncated) = out_h.await.unwrap_or_default();
    let (mut stderr, stderr_truncated) = err_h.await.unwrap_or_default();
    // Exact-limit vs over-limit: read up to max+1 to detect over (matches sync `take(MAX+1)`).
    let stdout_truncated = stdout_truncated || stdout.len() > max_bytes;
    let stderr_truncated = stderr_truncated || stderr.len() > max_bytes;
    stdout.truncate(max_bytes);
    stderr.truncate(max_bytes);
    let mut result = String::from_utf8_lossy(&stdout).into_owned();
    if stdout_truncated {
        result.push_str("\n[... output exceeded capture limit ...]");
    }
    let stderr_str = String::from_utf8_lossy(&stderr).into_owned();
    if !stderr_str.trim().is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str("--- stderr ---\n");
        result.push_str(&stderr_str);
        if stderr_truncated {
            result.push_str("\n[... stderr exceeded capture limit ...]");
        }
    }
    Ok((result, status.code()))
}

async fn kill_process_group_async(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            let _ = kill(-(pid as i32), SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.kill().await;
}

/// Split a `!`/`!!` shell escape into `(command, exclude_from_context)`.
/// `!cmd` feeds the next turn; `!!cmd` stays out of the LLM context. Returns
/// `None` when the line isn't a shell escape — including a bare `!`/`!!`,
/// which falls through to the agent instead of erroring. Everything
/// after the prefix is the command, newlines included.
pub(crate) fn parse_shell_escape(line: &str) -> Option<(String, bool)> {
    let (rest, excluded) = match line.strip_prefix("!!") {
        Some(rest) => (rest, true),
        None => (line.strip_prefix('!')?, false),
    };
    let command = rest.trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some((command, excluded))
    }
}

/// Spawn the workspace shell. Unix gets `sh -c` in a fresh session so tool
/// children can never write to or race the user's terminal for input: a child
/// that probes the terminal (e.g. `cargo test` running the theme tests →
/// OSC 10/11 on /dev/tty) would otherwise send queries to the TUI's pts and
/// race crossterm for the reply — the TUI can end up with half a color report
/// typed into the composer.
/// SAFETY: runs in the forked child before exec; it is not yet a process
/// group leader, so setsid() succeeds.
#[cfg(unix)]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut builder = tokio::process::Command::new("sh");
    builder
        .arg("-c")
        .arg(command)
        .envs(tool_runner_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    unsafe {
        builder.pre_exec(|| {
            let _ = setsid();
            Ok(())
        });
    }
    builder
}

#[cfg(not(unix))]
fn shell_command(command: &str) -> tokio::process::Command {
    let mut builder = tokio::process::Command::new("cmd");
    builder
        .arg("/C")
        .arg(command)
        .envs(tool_runner_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    builder
}
pub(crate) async fn tool_bash(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    session: Option<&Path>,
    shell_out: &mut Option<ShellEvidence>,
) -> Result<String, ToolError> {
    let (output, code) = run_bash(&arg_str(args, "command")?, cancel).await?;
    let (clamped, was_clamped) = clamp_lines_checked(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES);
    let (clamped, archive_id) =
        crate::agent::evidence_reducer::capture(session, "bash", &output, clamped, was_clamped);
    *shell_out = Some(ShellEvidence {
        archive_id,
        exit_code: code,
    });
    match code {
        Some(0) => Ok(clamped),
        code => Err(ToolError::Shell {
            output: clamped,
            code,
        }),
    }
}
