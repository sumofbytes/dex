#![allow(clippy::doc_lazy_continuation)]
mod fff;

use self::fff::{tool_fffind, tool_ffgrep};
use serde_json::{Map, Value};
use similar::TextDiff;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::agent::state::{wait_cancelled, CancellationSource};
use crate::core::console::Console;
use crate::core::format::clamp_lines;
use crate::core::types::{ApprovalDecision, ApprovalRequest, PermissionMode};
use tokio::io::AsyncReadExt as _;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setsid() -> i32;
}

#[cfg(unix)]
const SIGKILL: i32 = 9;
static CONFIGURED_OUTPUT_LIMIT: AtomicUsize = AtomicUsize::new(1_048_576);

/// Model-facing caps. Capture limits (1 MiB shell, 256 KiB read) guard
/// memory; clamp limits guard the context window. Head+tail clamping keeps
/// both the imports/context at the start and the errors/summaries at the end.
const BASH_CLAMP_LINES: usize = 400;
const BASH_CLAMP_BYTES: usize = 32 * 1024;
const READ_MAX_LINES: usize = 2_000;
const READ_MAX_BYTES: usize = 256 * 1024;
/// Multi-file read caps: enough for the "search, then read the hits" pattern
/// in one call, small enough that a fan-out cannot flood the context.
const READ_FANOUT_MAX_FILES: usize = 10;
const READ_FANOUT_GLOB_MAX_FILES: usize = 8;
const READ_FANOUT_PER_FILE_LINES: usize = 200;

pub(crate) fn set_output_limit(limit: usize) {
    if limit > 0 {
        CONFIGURED_OUTPUT_LIMIT.store(limit, Ordering::Relaxed);
    }
}

fn workspace_root() -> Result<PathBuf, ToolError> {
    env::current_dir().map_err(ToolError::Io)
}

/// Resolve a user-provided path within the current workspace. Existing path
/// components are canonicalized so symlinks cannot silently escape it; for a
/// new file, the existing parent is canonicalized instead.
fn workspace_path(raw: &str) -> Result<PathBuf, ToolError> {
    let root = workspace_root()?.canonicalize().map_err(ToolError::Io)?;
    resolve_workspace_path(&root, raw)
}

fn resolve_workspace_path(root: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let resolved = if candidate.exists() {
        candidate.canonicalize().map_err(ToolError::Io)?
    } else {
        let file_name = candidate
            .file_name()
            .ok_or_else(|| ToolError::OutsideWorkspace(candidate.display().to_string()))?;
        let parent = candidate
            .parent()
            .unwrap_or(root)
            .canonicalize()
            .map_err(ToolError::Io)?;
        parent.join(file_name)
    };
    if resolved == root || resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(ToolError::OutsideWorkspace(raw.to_string()))
    }
}

/// Normalize a tool-call `path` argument for same-path conflict detection:
/// `./foo.rs`, `src/../foo.rs` and `foo.rs` are the same file, but the raw
/// strings differ — the turn loop would then fan out two edits to one file
/// concurrently and the second edit would silently clobber the first.
/// Best-effort with known gaps: paths needing FS state we can't resolve
/// (missing parent dirs), case-only differences on case-insensitive FS,
/// and same-file writes via `bash` redirection (no `path` arg) still miss.
/// Unresolvable paths fall back to a lexical clean (no symlink resolution),
/// which still catches `./` and `a/../` spelling differences.
pub(crate) fn normalize_conflict_path(raw: &str) -> String {
    if let Ok(resolved) = workspace_path(raw) {
        return resolved.display().to_string();
    }
    lexical_normalize_fallback(raw)
}

/// Join `raw` against the workspace root and clean `.`/`..` lexically
/// without touching the FS. Catches `./newdir/f` vs `newdir/f` when the
/// parent doesn't exist yet (canonicalize would fail).
fn lexical_normalize_fallback(raw: &str) -> String {
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        let root = workspace_root()
            .ok()
            .and_then(|r| r.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        root.join(raw_path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        use std::path::Component as C;
        match comp {
            C::CurDir => {}
            C::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        raw.to_string()
    } else {
        out.display().to_string()
    }
}

#[derive(Debug)]
pub(crate) enum ToolError {
    Missing(&'static str),
    NotString(&'static str),
    InvalidArgument(String),
    Io(io::Error),
    EditNotUnique(usize),
    OutsideWorkspace(String),
    /// A write/edit supplied an `expected_hash` that no longer matches the
    /// file on disk (someone else changed it since the model's last read).
    /// The caller must re-read and retry — the write is not applied.
    StaleFile {
        path: String,
        expected: String,
        actual: String,
    },
    /// A shell command ran but signalled failure (non-zero exit, killed, or
    /// timed out). `code` is `None` when the process never exited on its own.
    /// Carries the combined output so partial results still reach the model.
    Shell {
        output: String,
        code: Option<i32>,
    },
    /// An internal tool-engine failure (not a bad invocation, not a shell
    /// exit): e.g. the fff index failed to initialize.
    Internal(String),
    /// The permission policy refused the call before it ran: a read-only
    /// rejection, a user deny, an unanswered approval (approver gone), or
    /// approval needed with no channel to ask on. Never inferred from tool
    /// output — decided by the Phase 0 gate in `execute`.
    Denied(String),
    Unknown(String),
}

/// A tool result together with whether the call actually succeeded. Success
/// is decided where the exit status is known — never inferred from the
/// output text, which may legitimately contain markers like `[exit 1]`.
/// `diff` carries the display-only git diff for write/edit, captured before
/// the file was mutated; it never reaches the model.
#[derive(Clone, Debug)]
pub(crate) struct ToolOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
    pub(crate) diff: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolMetadata {
    pub read_only: bool,
    pub mutating: bool,
    pub idempotent: bool,
    pub requires_shell: bool,
    pub permission: PermissionRequirement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionRequirement {
    Read,
    Write,
    Shell,
}

pub(crate) fn metadata(name: &str) -> Option<ToolMetadata> {
    Some(match name {
        "read" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: false,
            permission: PermissionRequirement::Read,
        },
        // fff tools run in-process; only git shells out.
        "grep" | "ffgrep" | "find" | "fffind" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: false,
            permission: PermissionRequirement::Read,
        },
        "ls" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: false,
            permission: PermissionRequirement::Read,
        },
        "git" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
        "chain" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
        "bash" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        "write" | "edit" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: false,
            permission: PermissionRequirement::Write,
        },
        // MCP tools are external processes: most restrictive gate (`ask`
        // unless trusted), same as shell. Resolved dynamically so cached
        // server tools don't need a static entry each.
        _ if name.starts_with("mcp__") => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        _ => return None,
    })
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(k) => write!(f, "missing argument '{}'", k),
            Self::NotString(k) => write!(f, "argument '{}' must be a string", k),
            Self::InvalidArgument(message) => write!(f, "{}", message),
            Self::Io(e) => write!(f, "io error: {}", e),
            Self::EditNotUnique(n) => write!(
                f,
                "oldText matches {n} locations; include more surrounding lines to make it unique, or pass replaceAll: true"
            ),
            Self::OutsideWorkspace(path) => write!(f, "path is outside the workspace: {}", path),
            Self::StaleFile {
                path,
                expected,
                actual,
            } => write!(
                f,
                "file changed since it was read (expected_hash mismatch: expected {expected}, file is {actual}) — re-read {} and retry; concurrent edit wins, your write was not applied",
                path
            ),
            Self::Shell { output, code } => match code {
                Some(code) => write!(f, "{output}\n[exit {code}]"),
                None => write!(f, "{output}"),
            },
            Self::Internal(e) => write!(f, "{e}"),
            Self::Denied(message) => write!(f, "permission denied: {message}"),
            Self::Unknown(t) => write!(f, "unknown tool '{}'", t),
        }
    }
}

fn arg_str(args: &Map<String, Value>, key: &'static str) -> Result<String, ToolError> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::NotString(key)),
        None => Err(ToolError::Missing(key)),
    }
}

fn audit(name: &str, args: &Map<String, Value>, outcome: &str) {
    // Audit writes are sync open+write per
    // tool call which blocks the loop thread on fs. Gate behind DEX_AUDIT=1
    // for strict auditing, otherwise skip (session.jsonl already journals).
    if std::env::var("DEX_AUDIT").as_deref() != Ok("1") {
        return;
    }
    let Some(base) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let path = base.join("dex/audit.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "cwd": env::current_dir().ok().map(|p| p.display().to_string()),
        "tool": name,
        "args": args,
        "outcome": outcome,
    });
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        // One write syscall per record: parallel tool executions append to
        // this file concurrently, and a multi-syscall formatted write would
        // interleave mid-record.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}

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

async fn run_bash(
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

async fn run_bash_with_limits(
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

/// `read` returns line-numbered content (right-aligned number + two-space gap
/// + tab-expanded content per .editorconfig/language) so subsequent `edit`
/// oldText anchors are cheap to construct. Default: first 2000 lines, capped
/// by a byte budget; paginate with offset/limit. Binary files are refused
/// rather than dumped into the context window.
///
/// Round-trip reduction: `paths` (explicit list) or `glob` (pattern fan-out)
/// read several files in ONE call — the "search, then read what it found"
/// chain collapses into a single tool call. Per-file errors are isolated and
/// the call succeeds when at least one file is readable.
async fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        return fanout_read(parse_path_list(paths)?, args).await;
    }
    if let Some(glob) = args.get("glob").and_then(Value::as_str) {
        return fanout_read(expand_glob(glob).await?, args).await;
    }
    let path = workspace_path(&arg_str(args, "path")?)?;
    let (body, _more) =
        read_file_numbered(&path, read_offset(args), read_limit(args, READ_MAX_LINES)).await?;
    Ok(body)
}

fn read_offset(args: &Map<String, Value>) -> usize {
    args.get("offset")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(1)
}

fn read_limit(args: &Map<String, Value>, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(default)
}

fn parse_path_list(paths: &[Value]) -> Result<Vec<PathBuf>, ToolError> {
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(
            "paths must not be empty".to_string(),
        ));
    }
    if paths.len() > READ_FANOUT_MAX_FILES {
        return Err(ToolError::InvalidArgument(format!(
            "paths accepts at most {READ_FANOUT_MAX_FILES} files per call (got {}); split into batches",
            paths.len()
        )));
    }
    paths
        .iter()
        .map(|value| {
            value.as_str().map(workspace_path).unwrap_or_else(|| {
                Err(ToolError::InvalidArgument(
                    "paths entries must be strings".to_string(),
                ))
            })
        })
        .collect()
}

/// Line-numbered content for one file, within the read budgets. Also returns
/// how many lines were omitted past the end (for pagination notes).
async fn read_file_numbered(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    // Budgeted read: `take(MAX+1)` caps the read at the byte budget + one
    // probe byte, so a multi-GB log never lands in memory whole. The extra
    // byte distinguishes over-budget from exact-fit.
    let file = tokio::fs::File::open(path).await.map_err(ToolError::Io)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(READ_MAX_BYTES + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .await
        .map_err(ToolError::Io)?;
    let over_budget = bytes.len() > READ_MAX_BYTES;
    bytes.truncate(READ_MAX_BYTES);
    if bytes.contains(&0) {
        return Err(ToolError::InvalidArgument(format!(
            "binary file; use `bash` with a targeted command such as `strings` or `hexdump` on {}",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if offset > total {
        return Err(ToolError::InvalidArgument(format!(
            "offset {offset} is past the end; {} has {total} lines",
            path.display()
        )));
    }
    let numbered: Vec<String> = lines
        .iter()
        .skip(offset - 1)
        .take(limit)
        .enumerate()
        .map(|(index, line)| format!("{:>4}  {}", offset + index, line))
        .collect();
    let mut out = clamp_lines(&numbered.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    let shown = numbered.len();
    let more = total.saturating_sub(offset - 1 + shown);
    if more > 0 && limit >= READ_MAX_LINES {
        out.push_str(&format!(
            "\n[... {more} more lines; continue with offset {} ...]",
            offset + shown
        ));
    }
    if over_budget {
        out.push_str("\n[... file exceeds the read byte budget; use offset/limit ...]");
    }
    Ok((out, more))
}

/// Multi-file read: `==> path <==` sections (grep-style), per-file limits,
/// isolated per-file errors, one shared byte budget.
#[allow(clippy::type_complexity)]
async fn fanout_read(paths: Vec<PathBuf>, args: &Map<String, Value>) -> Result<String, ToolError> {
    // Concurrent reads (S1 latency win: 10x5ms serial ~50ms -> ~5-10ms).
    // JoinSet + semaphore: at most 10 files read concurrently no matter
    // how the input caps evolve; join + sort restores input order, budget
    // is enforced on join.
    let per_file = read_limit(args, READ_FANOUT_PER_FILE_LINES);
    let offset = read_offset(args);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(10));
    let mut set = tokio::task::JoinSet::new();
    for (idx, path) in paths.iter().enumerate() {
        let path = path.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let res = read_file_numbered(&path, offset, per_file).await;
            (idx, path.display().to_string(), res)
        });
    }
    let mut joined: Vec<(usize, String, Result<(String, usize), ToolError>)> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(v) = r {
            joined.push(v);
        }
    }
    joined.sort_by_key(|(idx, _, _)| *idx);
    let mut sections: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (_, display, res) in joined {
        match res {
            Ok((body, _)) => {
                used += body.len();
                sections.push(format!("==> {display} <==\n{body}"));
            }
            Err(error) => errors.push(format!("==> {display}: error: {error}")),
        }
        if used > READ_MAX_BYTES {
            sections.push("[... read budget reached; remaining files skipped ...]".to_string());
            break;
        }
    }
    if sections.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "no file could be read: {}",
            errors.join("; ")
        )));
    }
    let mut out = clamp_lines(&sections.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    if !errors.is_empty() {
        out.push('\n');
        out.push_str(&errors.join("\n"));
    }
    Ok(out)
}

/// Expand a glob into workspace paths. Patterns with `/` match full paths
/// (`src/tools/*.rs`); bare patterns match basenames anywhere (`*.rs`).
async fn expand_glob(glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    let glob = glob.trim().trim_start_matches("./").to_string();
    if glob.is_empty() || !glob.contains(['*', '?']) {
        return Err(ToolError::InvalidArgument(
            "glob must contain wildcard characters (* or ?); use `path` for a single file"
                .to_string(),
        ));
    }
    let command = if glob.contains('/') {
        format!("find . \\( -name .git -o -name target -o -name node_modules \\) -prune -o -path './{glob}' -print")
    } else {
        format!("find . \\( -name .git -o -name target -o -name node_modules \\) -prune -o -name '{glob}' -print")
    };
    let (output, code) = run_bash(&command, &crate::agent::state::GlobalCancellation).await?;
    if !matches!(code, Some(0) | Some(1)) {
        return Err(ToolError::Shell { output, code });
    }
    let mut paths: Vec<PathBuf> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| workspace_path(line).ok())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    if paths.len() > READ_FANOUT_GLOB_MAX_FILES {
        paths.truncate(READ_FANOUT_GLOB_MAX_FILES);
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "glob '{glob}' matched no files"
        )));
    }
    Ok(paths)
}

async fn tool_bash(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    let (output, code) = run_bash(&arg_str(args, "command")?, cancel).await?;
    match code {
        Some(0) => Ok(clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES)),
        code => Err(ToolError::Shell {
            output: clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES),
            code,
        }),
    }
}

async fn tool_write(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let content = arg_str(args, "content")?;
    check_expected_hash(args, &path).await?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(ToolError::Io)?;
    }
    let replaced = tokio::fs::metadata(&path).await.ok().map(|meta| meta.len());
    atomic_write(&path, &content).await?;
    Ok(match replaced {
        Some(old_bytes) => format!(
            "wrote {} (replaced {old_bytes} bytes with {})",
            path.display(),
            content.len()
        ),
        None => format!("wrote {}", path.display()),
    })
}

/// Durability for `write`/`edit`: content lands in a sibling temp file (same
/// directory, so the rename stays on one filesystem), is fsynced, then is
/// atomically renamed over the target. A crash mid-write can never leave a
/// truncated or half-edited file behind — readers see either the old or the
/// new content, never a mixture. The plain `tokio::fs::write` this replaces
/// truncates in place: a power loss during the write corrupts the file the
/// agent is editing.
async fn atomic_write(path: &Path, content: &str) -> Result<(), ToolError> {
    use tokio::io::AsyncWriteExt as _;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".dex-write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    // Preserve the existing file mode (e.g. executable scripts): rename
    // replaces the inode, so re-apply after the swap. New files keep the
    // default umask mode.
    let orig_permissions = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(ToolError::Io)?;
        file.write_all(content.as_bytes())
            .await
            .map_err(ToolError::Io)?;
        file.sync_all().await.map_err(ToolError::Io)?;
        drop(file);
        // Windows rename() refuses to clobber an existing destination.
        #[cfg(windows)]
        let _ = tokio::fs::remove_file(path).await;
        tokio::fs::rename(&tmp, path).await.map_err(ToolError::Io)?;
        if let Some(permissions) = orig_permissions {
            let _ = tokio::fs::set_permissions(path, permissions).await;
        }
        // Best-effort: flush the directory entry so the rename itself is
        // durable; failure here (e.g. exotic filesystems) is not fatal — the
        // file content is already in place.
        if let Ok(handle) = tokio::fs::File::open(dir).await {
            let _ = handle.sync_all().await;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

/// When a write/edit carries `expected_hash`, reject if the file on disk no
/// longer matches (stale read → 409 semantics). Absent file hashes to the
/// empty-string sentinel.
async fn check_expected_hash(args: &Map<String, Value>, path: &Path) -> Result<(), ToolError> {
    let Some(expected) = args.get("expected_hash").and_then(Value::as_str) else {
        return Ok(());
    };
    if expected.is_empty() {
        return Ok(());
    }
    let actual = hash_file_async(&path.display().to_string()).await;
    if actual != expected {
        return Err(ToolError::StaleFile {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// FNV-1a 64-bit hex hash of a file's bytes. An absent file hashes as empty
/// content (the before-hash of a `write` creating a new file).
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hash = 2166136261u64;
    for b in bytes {
        hash = (hash ^ u64::from(*b)).wrapping_mul(16777619);
    }
    format!("{:016x}", hash)
}

pub(crate) fn hash_file(path: &str) -> String {
    hash_bytes(&fs::read(path).unwrap_or_default())
}

async fn hash_file_async(path: &str) -> String {
    hash_bytes(&tokio::fs::read(path).await.unwrap_or_default())
}

async fn tool_edit(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let old = arg_str(args, "oldText")?;
    let new = arg_str(args, "newText")?;
    let replace_all = args
        .get("replaceAll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if old.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText must not be empty; use `write` to create files".to_string(),
        ));
    }
    if old == new {
        return Err(ToolError::InvalidArgument(
            "oldText and newText are identical; nothing to edit".to_string(),
        ));
    }
    check_expected_hash(args, &path).await?;
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(ToolError::Io)?;
    let (updated, note) = apply_edit(&content, &old, &new, replace_all)?;
    atomic_write(&path, &updated).await?;
    Ok(format!("edited {}{note}", path.display()))
}

/// Git-style unified diff (4 context lines, `--- a/…` / `+++ b/…` headers,
/// `/dev/null` for new files) of a pending write/edit, shown in the
/// transcript before approval and under the tool result. Returns None when
/// the file is missing or the change is empty.
fn build_diff(raw_path: &str, before: Option<&str>, after: &str) -> Option<String> {
    let diff = TextDiff::from_lines(before.unwrap_or(""), after);
    let (old_header, new_header) = match &before {
        Some(_) => (format!("a/{raw_path}"), format!("b/{raw_path}")),
        None => ("/dev/null".to_string(), format!("b/{raw_path}")),
    };
    let out = diff
        .unified_diff()
        .context_radius(4)
        .header(&old_header, &new_header)
        .to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn diff_after(name: &str, args: &Map<String, Value>, before: Option<&str>) -> Option<String> {
    match name {
        "write" => arg_str(args, "content").ok(),
        "edit" => {
            let old = arg_str(args, "oldText").ok()?;
            let new = arg_str(args, "newText").ok()?;
            let replace_all = args
                .get("replaceAll")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            apply_edit(before.unwrap_or(""), &old, &new, replace_all)
                .ok()
                .map(|(updated, _)| updated)
        }
        _ => None,
    }
}

async fn change_diff_async(name: &str, args: &Map<String, Value>) -> Option<String> {
    let raw_path = arg_str(args, "path").ok()?;
    let path = workspace_path(&raw_path).ok()?;
    let before = tokio::fs::read_to_string(&path).await.ok();
    let after = diff_after(name, args, before.as_deref())?;
    build_diff(&raw_path, before.as_deref(), &after)
}

fn apply_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, String), ToolError> {
    let count = content.matches(old).count();
    if count == 1 || (replace_all && count > 1) {
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        let idx = content.find(old).unwrap_or(0);
        let start_line = content[..idx].matches('\n').count() + 1;
        let end_line = start_line + old.lines().count().saturating_sub(1);
        let span = if start_line == end_line {
            format!("line {start_line}")
        } else {
            format!("lines {start_line}-{end_line}")
        };
        let note = if count > 1 {
            format!(" ({span}, {count} occurrences)")
        } else {
            format!(" ({span})")
        };
        return Ok((updated, note));
    }
    if count > 1 {
        return Err(ToolError::EditNotUnique(count));
    }

    // Exact match failed: retry with a whitespace-insensitive line-window
    // comparison (fuzzy fallback). Handles the common case of
    // the model reproducing content with different indentation or trailing
    // whitespace. Whole lines are replaced, so the match must cover them.
    let old_lines: Vec<&str> = old.lines().collect();
    let window = old_lines.len();
    let content_lines: Vec<&str> = content.lines().collect();
    let matches_at: Vec<usize> = (0..content_lines.len().saturating_sub(window - 1))
        .filter(|&start| {
            content_lines[start..start + window]
                .iter()
                .zip(&old_lines)
                .all(|(c, o)| c.trim() == o.trim())
        })
        .collect();
    if matches_at.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText not found; read the file to confirm the exact text (whitespace must match)"
                .to_string(),
        ));
    }
    if matches_at.len() > 1 && !replace_all {
        return Err(ToolError::EditNotUnique(matches_at.len()));
    }

    // Replace windows from the end so earlier indices stay valid. Each
    // replacement line inherits the indentation of the old line it replaces
    // when it carries none of its own — models frequently resend matched
    // text without the file's leading whitespace.
    let mut updated_lines: Vec<String> = content_lines.iter().map(|s| s.to_string()).collect();
    for &start in matches_at.iter().rev() {
        let replacement: Vec<String> = new
            .lines()
            .enumerate()
            .map(|(index, line)| {
                let old_indent = content_lines
                    .get(start + index)
                    .map(|old_line| &old_line[..old_line.len() - old_line.trim_start().len()])
                    .unwrap_or_default();
                if !old_indent.is_empty() && !line.is_empty() && line.trim_start() == line {
                    format!("{old_indent}{line}")
                } else {
                    line.to_string()
                }
            })
            .collect();
        updated_lines.splice(start..start + window, replacement);
    }
    let mut updated = updated_lines.join("\n");
    if content.ends_with('\n') {
        updated.push('\n');
    }
    let note = if matches_at.len() == 1 {
        format!(
            " (lines {}-{}, whitespace-insensitive)",
            matches_at[0] + 1,
            matches_at[0] + window
        )
    } else {
        format!(" ({} sites, whitespace-insensitive)", matches_at.len())
    };
    Ok((updated, note))
}

async fn tool_ls(args: &Map<String, Value>) -> Result<String, ToolError> {
    let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = workspace_path(raw)?;
    let meta = tokio::fs::metadata(&path).await.map_err(ToolError::Io)?;
    if !meta.is_dir() {
        return Ok(path.display().to_string());
    }
    let mut dir = tokio::fs::read_dir(&path).await.map_err(ToolError::Io)?;
    let mut entries: Vec<String> = Vec::new();
    while let Some(entry) = dir.next_entry().await.map_err(ToolError::Io)? {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // is_dir via file_type to avoid extra metadata call; fallback to path check.
        let is_dir = entry
            .file_type()
            .await
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|_| p.is_dir());
        if is_dir {
            entries.push(format!("{name}/"));
        } else {
            entries.push(name);
        }
    }
    entries.sort();
    if entries.is_empty() {
        return Ok("(empty)".to_string());
    }
    // Clamp to avoid flooding context.
    let limited = clamp_lines(&entries.join("\n"), 500, 32 * 1024);
    Ok(limited)
}

async fn tool_git(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("status");
    if !matches!(mode, "status" | "diff") {
        return Err(ToolError::NotString("mode (status or diff)"));
    }
    // Routed through run_bash so git shares the shell timeout, cancellation,
    // capture limits, and clamping instead of running unbounded.
    let (output, code) = run_bash(&format!("git --no-pager {mode}"), cancel).await?;
    match code {
        Some(0) | Some(1) => Ok(clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES)),
        code => Err(ToolError::Shell {
            output: clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES),
            code,
        }),
    }
}

/// Maximum steps in one chain: enough for search → read → search → read,
/// small enough to stay predictable.
const CHAIN_MAX_STEPS: usize = 4;

/// A bounded, read-only chain executed in ONE LLM round trip — dex's scoped
/// take on programmatic tool calling. The model declares steps; routing
/// between steps is mechanical (`from` a search step, `take: "paths"` into a
/// read fan-out), never semantic: the model cannot branch or transform
/// mid-chain, and mutation/shell tools are refused. On a step failure the
/// earlier steps' outputs ship with the error, so the round trip still
/// carries information.
async fn tool_chain(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(ToolError::Missing("steps"))?;
    if steps.len() < 2 {
        return Err(ToolError::InvalidArgument(
            "chain needs at least 2 steps; a single tool call does not need a chain".to_string(),
        ));
    }
    if steps.len() > CHAIN_MAX_STEPS {
        return Err(ToolError::InvalidArgument(format!(
            "chain supports at most {CHAIN_MAX_STEPS} steps (got {})",
            steps.len()
        )));
    }

    let mut completed: Vec<(String, String)> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match run_chain_step(step, index, &completed, cancel, policy, filter).await {
            Ok(pair) => completed.push(pair),
            Err(error) => {
                let mut text = render_chain_steps(&completed);
                text.push_str(&format!("\n--- step {index} failed ---\nError: {error}\n"));
                return Err(ToolError::Shell {
                    output: clamp_lines(&text, READ_MAX_LINES, READ_MAX_BYTES),
                    code: None,
                });
            }
        }
    }
    Ok(clamp_lines(
        &render_chain_steps(&completed),
        READ_MAX_LINES,
        READ_MAX_BYTES,
    ))
}

fn render_chain_steps(steps: &[(String, String)]) -> String {
    let mut out = String::new();
    for (index, (tool, output)) in steps.iter().enumerate() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("--- step {index}: {tool} ---\n{output}"));
    }
    out
}

async fn run_chain_step(
    step: &Value,
    index: usize,
    completed: &[(String, String)],
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<(String, String), ToolError> {
    let invalid = |message: String| ToolError::InvalidArgument(format!("step {index}: {message}"));
    let obj = step
        .as_object()
        .ok_or_else(|| invalid("must be an object".to_string()))?;
    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("is missing 'tool'".to_string()))?;
    let meta = metadata(tool).ok_or_else(|| invalid(format!("unknown tool '{tool}'")))?;
    if meta.permission != PermissionRequirement::Read {
        return Err(invalid(format!(
            "chain is read-only; '{tool}' must run as its own approved call"
        )));
    }

    let mut step_args = match obj.get("args") {
        Some(Value::Object(map)) => map.clone(),
        None => Map::new(),
        Some(_) => return Err(invalid("'args' must be an object".to_string())),
    };

    if let Some(from) = obj.get("from") {
        let from = from
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| invalid("'from' must be the index of an earlier step".to_string()))?;
        if from >= index {
            return Err(invalid("'from' must reference an earlier step".to_string()));
        }
        if obj.get("take").and_then(Value::as_str) != Some("paths") {
            return Err(invalid(
                "'take' must be \"paths\" (routes a search step's matched files into read)"
                    .to_string(),
            ));
        }
        if tool != "read" {
            return Err(invalid("'from' routing requires the read tool".to_string()));
        }
        let (source_tool, source_output) = &completed[from];
        if !matches!(
            source_tool.as_str(),
            "grep" | "ffgrep" | "find" | "fffind" | "chain"
        ) {
            return Err(invalid(format!(
                "'from' step {from} is '{source_tool}', which produces no file paths; use grep (files mode) or find"
            )));
        }
        let max_files = obj
            .get("max_files")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, READ_FANOUT_MAX_FILES))
            .unwrap_or(5);
        let paths = extract_search_paths(source_output, max_files, from)?;
        step_args.remove("path");
        step_args.insert(
            "paths".to_string(),
            Value::Array(
                paths
                    .into_iter()
                    .map(|path| Value::String(path.display().to_string()))
                    .collect(),
            ),
        );
    }

    let output = Box::pin(execute(tool, &step_args, cancel, policy, filter)).await?;
    Ok((tool.to_string(), output))
}

/// Pull file paths out of a search step's output (grep files-mode lines,
/// find output) and resolve them within the workspace.
fn extract_search_paths(
    output: &str,
    max_files: usize,
    from: usize,
) -> Result<Vec<PathBuf>, ToolError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Skip truncation markers and chain section labels.
        if line.is_empty() || line.starts_with('[') || line.starts_with("---") {
            continue;
        }
        if let Ok(path) = workspace_path(line) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if paths.len() >= max_files {
            break;
        }
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "step {from} output contained no resolvable file paths; widen the search or raise head_limit"
        )));
    }
    Ok(paths)
}

/// The permission context a tool call runs under (Phase 0 gate). `mode`
/// is the turn's `PermissionMode` (from `LlmConfig::permission`);
/// `console` carries the approval channel plus the live session-approval
/// set. Owned (not borrowed) so concurrent fan-out tasks can each hold a
/// copy; the console clone shares the turn's live session-approval set, so
/// build one `Policy` per turn and reuse it for the whole turn — same-turn
/// allow-for-session records are then visible to every later call.
/// `Policy::trusted()` (no console) preserves the old behavior for explicit
/// user-invoked paths (`dex run`, `--tool`, the `!` escape): the `!` itself
/// is the approval there.
#[derive(Clone)]
pub(crate) struct Policy {
    pub(crate) mode: PermissionMode,
    pub(crate) console: Option<Console>,
}

impl Policy {
    pub(crate) fn trusted() -> Self {
        Self {
            mode: PermissionMode::Trusted,
            console: None,
        }
    }

    pub(crate) fn turn(mode: PermissionMode, console: &Console) -> Self {
        Self {
            mode,
            console: Some(console.clone()),
        }
    }
}

/// Phase 0 approval gate: dispatch consults the turn's policy before any
/// tool runs. Reads always pass; `trusted` passes everything; `ask-shell`
/// passes file mutations (only shell prompts); everything else mutating
/// parks an `ApprovalRequest` on the console's approval channel and blocks
/// for the verdict, with session approvals short-circuiting first.
/// Denial, cancellation, and no-channel surface as `ToolError::Denied` —
/// the loop records it as a failed tool result, and the post-fan-out
/// cancellation check still unwinds a turn cancelled mid-prompt.
async fn enforce_policy(
    name: &str,
    args: &Map<String, Value>,
    requirement: PermissionRequirement,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
) -> Result<(), ToolError> {
    let needs_approval = match (requirement, policy.mode) {
        (PermissionRequirement::Read, _) => false,
        (_, PermissionMode::Trusted) => false,
        // ask-shell permits reads and file mutations; only shell prompts.
        (PermissionRequirement::Write, PermissionMode::AskShell) => false,
        _ => true,
    };
    if !needs_approval {
        return Ok(());
    }
    if policy.mode == PermissionMode::ReadOnly {
        return Err(ToolError::Denied(format!(
            "{name} is not allowed in read-only mode"
        )));
    }
    let Some(console) = policy.console.as_ref() else {
        return Err(ToolError::Denied(format!(
            "{name} needs approval ({} mode) but no approval channel is attached",
            policy.mode.as_str()
        )));
    };
    let input = serde_json::Value::Object(args.clone()).to_string();
    if console.session_approved(name, &input) {
        return Ok(());
    }
    let Some(sender) = console.approval() else {
        return Err(ToolError::Denied(format!(
            "{name} needs approval but no approval channel is attached"
        )));
    };
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel(1);
    sender
        .send(ApprovalRequest {
            name: name.to_string(),
            input: input.clone(),
            agent_id: None,
            response: response_tx,
        })
        .await
        .map_err(|_| {
            ToolError::Denied(format!(
                "{name} needs approval but the approval channel is closed"
            ))
        })?;
    let decision = tokio::select! {
        () = wait_cancelled(cancel) => {
            return Err(ToolError::Denied(
                "cancelled while awaiting approval".to_string(),
            ));
        }
        verdict = response_rx.recv() => verdict,
    };
    match decision {
        Some(ApprovalDecision::Once) => Ok(()),
        Some(ApprovalDecision::Session) => {
            // Same-turn repeats skip the prompt via the turn policy's
            // console copy; the daemon's /approve handler persists to its
            // session map for later turns (the console is per-turn).
            console.record_session_approval(name, &input);
            Ok(())
        }
        Some(ApprovalDecision::Deny) => {
            Err(ToolError::Denied(format!("{name} denied by approver")))
        }
        None => Err(ToolError::Denied(format!(
            "{name} approval went unanswered; treating as denied"
        ))),
    }
}

/// Allowlist enforced at dispatch (Phase 2 runtime extraction; plan §11).
/// `owner` names the agent the set belongs to and appears in denial errors
/// so the model can self-correct; `allowed` holds exact tool names plus
/// `prefix*` wildcards (`mcp__gh__*` covers one server; `mcp__*` covers all
/// MCP tools). A bare `*` would allow everything — never put it in a child
/// set. `None` (no filter) preserves the parent's unfiltered behavior at
/// every existing call site.
#[derive(Clone, Debug)]
pub(crate) struct ToolFilter {
    pub(crate) owner: String,
    pub(crate) allowed: BTreeSet<String>,
}

impl ToolFilter {
    // Test-only until the Phase 4 manager builds child filters from
    // definitions (same precedent as DaemonState::is_session_approved).
    #[allow(dead_code)]
    pub(crate) fn new(
        owner: impl Into<String>,
        allowed: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            owner: owner.into(),
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }

    /// Exact name match, or a `prefix*` wildcard entry covering it.
    pub(crate) fn allows(&self, name: &str) -> bool {
        if self.allowed.contains(name) {
            return true;
        }
        self.allowed
            .iter()
            .filter_map(|entry| entry.strip_suffix('*'))
            .any(|prefix| name.starts_with(prefix))
    }
}

/// Execute a tool using paths confined to the current workspace.
pub(crate) async fn execute(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let requirement = if name.starts_with("mcp__") {
        // MCP tools are external processes: most restrictive gate, same as
        // shell. Resolved here so the gate below covers them too.
        PermissionRequirement::Shell
    } else if let Some(meta) = metadata(name) {
        meta.permission
    } else {
        let error = ToolError::Unknown(name.to_string());
        audit(name, args, &error.to_string());
        return Err(error);
    };
    // Availability before approval: a sharper, cheaper rejection naming the
    // agent's allowlist (plan §11 — the model self-corrects, never executes).
    // Checked before `enforce_policy` so a denied tool never prompts.
    if let Some(filter) = filter {
        if !filter.allows(name) {
            let error = ToolError::Denied(format!(
                "tool '{name}' is not in {}'s tool allowlist",
                filter.owner
            ));
            audit(name, args, &error.to_string());
            return Err(error);
        }
    }
    if let Err(error) = enforce_policy(name, args, requirement, cancel, policy).await {
        audit(name, args, &error.to_string());
        return Err(error);
    }
    if name.starts_with("mcp__") {
        let result = crate::mcp::call_global(name, args, cancel).await;
        let outcome = match &result {
            Ok(_) => "ok".to_string(),
            Err(e) => e.clone(),
        };
        audit(name, args, &outcome);
        return result.map_err(ToolError::Internal);
    }
    // fff owns its threads + lock; run inside spawn_blocking (10s grep budget stays).
    if matches!(name, "grep" | "ffgrep" | "find" | "fffind") {
        let name_owned = name.to_string();
        let args_owned = args.clone();
        let res = tokio::task::spawn_blocking(move || match name_owned.as_str() {
            "grep" | "ffgrep" => tool_ffgrep(&args_owned),
            _ => tool_fffind(&args_owned),
        })
        .await
        .unwrap_or(Err(ToolError::Internal("fff worker panicked".into())));
        let outcome = match &res {
            Ok(_) => "ok".to_string(),
            Err(e) => e.to_string(),
        };
        audit(name, args, &outcome);
        return res;
    }
    let result = match name {
        "read" => tool_read(args).await,
        "bash" => tool_bash(args, cancel).await,
        "write" => tool_write(args).await,
        "edit" => tool_edit(args).await,
        "grep" | "ffgrep" | "find" | "fffind" => unreachable!("handled above"),
        "ls" => tool_ls(args).await,
        "git" => tool_git(args, cancel).await,
        "chain" => tool_chain(args, cancel, policy, filter).await,
        _ => unreachable!("metadata and dispatch must stay in sync"),
    };
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(error) => error.to_string(),
    };
    audit(name, args, &outcome);
    result
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
    // model, only the transcript preview via `ToolOutcome::diff`.
    let pending_diff = if matches!(name, "write" | "edit") {
        change_diff_async(name, args).await
    } else {
        None
    };
    match execute(name, args, cancel, policy, filter).await {
        Ok(out) => ToolOutcome {
            text: out,
            ok: true,
            diff: pending_diff,
        },
        Err(e) => ToolOutcome {
            text: format!("Error: {}", e),
            ok: false,
            diff: None,
        },
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
    use crate::agent::state::GlobalCancellation;
    use serde_json::json;
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
        let cwd = std::env::current_dir().unwrap();
        fs::create_dir_all(cwd.join("target")).unwrap();
        let rel = "target/dex-atomic-write-test.txt";
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
        let strays: Vec<_> = fs::read_dir(cwd.join("target"))
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
        let _ = fs::remove_file(&full);
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
    async fn temporary_workspace_paths_are_confined() {
        let root = std::env::temp_dir().join(format!("dex-workspace-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        assert!(resolve_workspace_path(&root, "inside.txt")
            .unwrap()
            .starts_with(&root));
        assert!(matches!(
            resolve_workspace_path(&root, "../outside.txt"),
            Err(ToolError::OutsideWorkspace(_))
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
        super::fff::rescan();

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
        super::fff::rescan();

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
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("one.rs"), format!("{needle} in one\n")).unwrap();
        fs::write(root.join("two.rs"), "nothing here\n").unwrap();
        super::fff::rescan();

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
        super::fff::rescan();
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
        use crate::core::console::CancellationToken;
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
        use crate::core::console::CancellationToken;
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
    use crate::core::console::Console;
    use crate::core::types::{ApprovalDecision, PermissionMode};

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
        use crate::core::console::CancellationToken;
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
