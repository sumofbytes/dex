//! Background shell tasks: `task` with `action` —
//! spawn/output/stop/wait/list (spec Rev 3). Runs on the per-session
//! [`AgentManager`], so tool calls (which hold a manager, not `DaemonState`)
//! can spawn, poll, and stop without extra plumbing.

use super::super::manager::AgentManager;
use super::schema::AgentTurnContext;
use crate::agent::state::CancellationSource;
use crate::daemon::tasks::pure;
use crate::daemon::tasks::TaskStatus;
use crate::tools::policy::{enforce_policy, PermissionRequirement};
use crate::tools::ToolError;
use crate::tools::ToolFilter;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt as _;

pub const TASK_TOOL: &str = "task";
pub const BACKGROUND_ACTIONS: [&str; 5] = ["spawn", "output", "stop", "wait", "list"];
const MAX_WAIT_SECONDS: u64 = 120;
/// Grace window between SIGTERM and SIGKILL in `stop` / manager shutdown.
const SIGTERM_GRACE: Duration = Duration::from_secs(2);

pub fn is_task(name: &str) -> bool {
    name == TASK_TOOL
}

fn output_cap() -> usize {
    std::env::var("DEX_TOOL_OUTPUT_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|b| *b > 0)
        .unwrap_or_else(|| crate::tools::CONFIGURED_OUTPUT_LIMIT.load(Ordering::Relaxed))
}

fn string_arg(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|v| !v.trim().is_empty())
}

fn cursor_arg(args: &Map<String, Value>) -> Result<Option<u64>, ToolError> {
    match args.get("cursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            ToolError::InvalidArgument("cursor must be a non-negative integer".to_string())
        }),
        Some(_) => Err(ToolError::InvalidArgument(
            "cursor must be a non-negative integer".to_string(),
        )),
    }
}

fn timeout_arg(args: &Map<String, Value>) -> Result<u64, ToolError> {
    let secs = match args.get("timeout_secs") {
        None | Some(Value::Null) => 30,
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
            ToolError::InvalidArgument("timeout_secs must be a non-negative integer".to_string())
        })?,
        Some(_) => {
            return Err(ToolError::InvalidArgument(
                "timeout_secs must be a non-negative integer".to_string(),
            ));
        }
    };
    Ok(secs.min(MAX_WAIT_SECONDS))
}

/// Dispatch `task` from [`crate::tools::execute`]. The allowlist gate
/// already ran for children. Per-action permission gate (spec Rev 3):
/// `spawn`/`stop` need `Shell`, `output`/`wait`/`list` need `Read`.
pub async fn execute_task(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let Some(ctx) = policy.agent.clone() else {
        return Err(ToolError::Denied(format!(
            "'{name}' spawns children in the daemon; it is unavailable without a \
             daemon-backed turn (one-shot and direct tool runs have no manager)"
        )));
    };
    if let Some(filter) = filter {
        if !filter.allows(name) {
            return Err(ToolError::Denied(format!(
                "tool '{name}' is not in {}'s tool allowlist",
                filter.owner
            )));
        }
    }
    let Some(action) = args.get("action").and_then(Value::as_str) else {
        return Err(ToolError::Missing("action"));
    };
    // Per-action gate before anything runs, so a denial never prompts twice.
    let requirement = match action {
        "spawn" | "stop" => PermissionRequirement::Shell,
        "output" | "wait" | "list" => PermissionRequirement::Read,
        _ => {
            return Err(ToolError::InvalidArgument(format!(
                "action must be one of {} (got '{action}')",
                BACKGROUND_ACTIONS.join(" | ")
            )));
        }
    };
    enforce_policy(name, args, requirement, cancel, policy, filter).await?;
    match action {
        "spawn" => bg_spawn(&ctx, args).await,
        "output" => bg_output(&ctx, args).await,
        "stop" => bg_stop(&ctx, args).await,
        "wait" => bg_wait(&ctx, args, cancel).await,
        "list" => bg_list(&ctx).await,
        _ => unreachable!(),
    }
}

async fn bg_spawn(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
) -> Result<String, ToolError> {
    let command = string_arg(args, "command").ok_or(ToolError::Missing("command"))?;
    let cwd = PathBuf::from(ctx.cwd.clone());
    if command.trim().is_empty() {
        return Err(ToolError::Missing("command"));
    }
    let id = ctx
        .manager
        .bg_spawn(command.clone(), cwd.clone())
        .map_err(ToolError::Denied)?;
    // Build the child with the foreground shell builder (workspace cwd,
    // `$DEX_BIN`/pager env, fresh session on unix).
    let mut cmd = crate::tools::shell::shell_command(&command);
    cmd.current_dir(&cwd);
    let mut child = cmd.spawn().map_err(|e| {
        ctx.manager
            .bg_finish(&id, TaskStatus::SpawnFailed(e.to_string()), None);
        ToolError::Io(e)
    })?;
    let pid = child.id();
    if let Some(pid) = pid {
        ctx.manager.bg_set_pid(&id, pid);
    } else {
        ctx.manager.bg_finish(
            &id,
            TaskStatus::SpawnFailed("spawn produced no pid".to_string()),
            None,
        );
        return Err(ToolError::Internal(
            "background spawn produced no pid".to_string(),
        ));
    }
    let (mut stdout, mut stderr) = (
        child.stdout.take().expect("stdout was piped"),
        child.stderr.take().expect("stderr was piped"),
    );
    let manager = ctx.manager.clone();
    let task_id = id.clone();
    let cap = output_cap();
    let handle = tokio::spawn(async move {
        drain_task(manager, task_id, child, &mut stdout, &mut stderr, cap).await;
    });
    ctx.manager.bg_set_handle(&id, handle);
    Ok(format!(
        "started background task {id}: {command}\n\
         poll with background(action: \"output\", id: \"{id}\"); it will also be\n\
         announced when it finishes"
    ))
}

/// The drain task: owns the `Child`, appends both pipes to the tail buffer,
/// emits coalesced live chunks, and records the terminal status.
async fn drain_task(
    manager: AgentManager,
    id: String,
    mut child: tokio::process::Child,
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    cap: usize,
) {
    let mut buf_out = [0u8; 8192];
    let mut buf_err = [0u8; 8192];
    let mut last_emit = Instant::now();
    let mut pending: Vec<u8> = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    // Join the exit with pipe EOF: poll the child while draining pipes.
    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            biased;
            status = child.wait() => {
                // Drain whatever remains, then record. `drain_rest` feeds
                // the tail bytes through `pending` so the final live chunk
                // still broadcasts (fast tasks would otherwise vanish from
                // the TUI log — the buffer had them, the stream didn't).
                drain_rest(stdout, stderr, &manager, &id, &mut pending, cap).await;
                flush_pending(&manager, &id, &mut pending);
                let (status_word, code) = match status {
                    Ok(st) => status_from_exit(&st),
                    Err(e) => (TaskStatus::WaitFailed(e.to_string()), None),
                };
                // `stop` may have already recorded Killed; keep first write.
                if manager.bg_is_running(&id).unwrap_or(false) {
                    manager.bg_finish(&id, status_word, code);
                }
                let _ = child.kill().await;
                return;
            }
            n = stdout.read(&mut buf_out), if !stdout_done => {
                match n {
                    Ok(0) => stdout_done = true,
                    Ok(n) => {
                        manager.bg_append(&id, &buf_out[..n], cap);
                        pending.extend_from_slice(&buf_out[..n]);
                    }
                    Err(_) => stdout_done = true,
                }
            }
            n = stderr.read(&mut buf_err), if !stderr_done => {
                match n {
                    Ok(0) => stderr_done = true,
                    Ok(n) => {
                        manager.bg_append(&id, &buf_err[..n], cap);
                        pending.extend_from_slice(&buf_err[..n]);
                    }
                    Err(_) => stderr_done = true,
                }
            }
        }
        if (!pending.is_empty() && pending.len() >= 8192)
            || last_emit.elapsed() >= Duration::from_secs(1)
        {
            flush_pending(&manager, &id, &mut pending);
            last_emit = Instant::now();
        }
        if manager.bg_is_running(&id).is_none() {
            // Evicted or torn down: stop draining, ensure the child dies.
            crate::tools::shell::kill_process_group_pid(child.id().unwrap_or(0));
            let _ = child.kill().await;
            return;
        }
    }
    flush_pending(&manager, &id, &mut pending);
    match child.wait().await {
        Ok(st) => {
            let (status_word, code) = status_from_exit(&st);
            if manager.bg_is_running(&id).unwrap_or(false) {
                manager.bg_finish(&id, status_word, code);
            }
        }
        Err(e) => {
            if manager.bg_is_running(&id).unwrap_or(false) {
                manager.bg_finish(&id, TaskStatus::WaitFailed(e.to_string()), None);
            }
        }
    }
}

/// Drain both pipes to EOF after the child exited, appending to the tail
/// buffer and feeding `pending` so the caller emits one final live chunk.
async fn drain_rest(
    stdout: &mut tokio::process::ChildStdout,
    stderr: &mut tokio::process::ChildStderr,
    manager: &AgentManager,
    id: &str,
    pending: &mut Vec<u8>,
    cap: usize,
) {
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                manager.bg_append(id, &buf[..n], cap);
                pending.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                manager.bg_append(id, &buf[..n], cap);
                pending.extend_from_slice(&buf[..n]);
            }
            Err(_) => break,
        }
    }
}

/// Exit status → (TaskStatus, exit_code). Signal death keeps the Unix
/// signal number so notices/`wait` can say which signal ended it.
fn status_from_exit(status: &std::process::ExitStatus) -> (TaskStatus, Option<i32>) {
    match status.code() {
        Some(0) => (TaskStatus::Exited(0), Some(0)),
        Some(c) => (TaskStatus::Exited(c), Some(c)),
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                (TaskStatus::Killed(status.signal()), None)
            }
            #[cfg(not(unix))]
            {
                (TaskStatus::Killed(None), None)
            }
        }
    }
}

fn flush_pending(manager: &AgentManager, id: &str, pending: &mut Vec<u8>) {
    if pending.is_empty() {
        return;
    }
    let chunk = pure::decode_lossy(pending);
    pending.clear();
    manager.bg_emit_output(id, chunk);
}

/// Shared `output`/`wait` rendering for the new byte range.
fn render_output(
    manager: &AgentManager,
    id: &str,
    cursor: Option<u64>,
) -> Result<String, ToolError> {
    let (slice, bytes, status_word, running) = manager
        .bg_read(id, cursor)
        .map_err(ToolError::InvalidArgument)?;
    let text = pure::decode_lossy(&bytes);
    let clamped = pure::clamp_new(&text);
    // Duration line: manager has no started_at accessor; use status only.
    // Keep the header stable: `task-1: running` / `task-1: exit 0`.
    let header = if running {
        format!("{id}: running")
    } else {
        format!("{id}: finished, {status_word}")
    };
    let mut out = format!("{header}\n[cursor {} → {}]\n", slice.start, slice.end);
    if slice.truncated > 0 {
        out.push_str(&format!(
            "[output truncated — {} bytes dropped]\n",
            slice.truncated
        ));
    }
    if slice.clamped {
        out.push_str("[cursor clamped]\n");
    }
    out.push_str(&clamped);
    Ok(out)
}

async fn bg_output(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
) -> Result<String, ToolError> {
    let id = string_arg(args, "id").ok_or(ToolError::Missing("id"))?;
    let cursor = cursor_arg(args)?;
    render_output(&ctx.manager, &id, cursor)
}

async fn bg_stop(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
) -> Result<String, ToolError> {
    let id = string_arg(args, "id").ok_or(ToolError::Missing("id"))?;
    let running = ctx.manager.bg_is_running(&id).ok_or_else(|| {
        ToolError::InvalidArgument(format!(
            "unknown background task '{id}': never spawned in this session, or its result aged out of retention"
        ))
    })?;
    if !running {
        let word = ctx
            .manager
            .bg_status_word(&id)
            .unwrap_or_else(|| "finished".to_string());
        return Ok(format!("{id}: already {word}"));
    }
    if let Some(pid) = ctx.manager.bg_pid(&id) {
        // Two-phase stop: SIGTERM the group first, then give it a grace
        // window to exit on its own before SIGKILL (SIGKILL denies
        // handlers, temp cleanup, and pipe flush).
        crate::tools::shell::sigterm_process_group_pid(pid);
        if let Some(mut rx) = ctx.manager.bg_watch_rx(&id) {
            let deadline = Instant::now() + SIGTERM_GRACE;
            while matches!(ctx.manager.bg_is_running(&id), Some(true)) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match tokio::time::timeout(deadline - now, rx.changed()).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
        }
        if ctx.manager.bg_is_running(&id).unwrap_or(false) {
            if let Some(pid) = ctx.manager.bg_pid(&id) {
                crate::tools::shell::kill_process_group_pid(pid);
            }
        }
    }
    // Give the drain a beat to observe the death and record Killed; then
    // record synchronously so `stop` never leaves a zombie row if the drain
    // was already gone. `bg_finish` is first-write-wins (guarded by
    // `is_running`), so a racing drain can't double-notice.
    let manager = ctx.manager.clone();
    let stop_id = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if manager.bg_is_running(&stop_id).unwrap_or(false) {
            manager.bg_finish(&stop_id, TaskStatus::Killed(None), None);
        }
    });
    #[cfg(unix)]
    return Ok(format!(
        "{id}: stopped (SIGTERM to process group; SIGKILL after grace if ignored)"
    ));
    #[cfg(not(unix))]
    return Ok(format!("{id}: stopped"));
}

async fn bg_wait(
    ctx: &Arc<AgentTurnContext>,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    let id = string_arg(args, "id").ok_or(ToolError::Missing("id"))?;
    if ctx.manager.bg_read(&id, None).is_err() {
        return Err(ToolError::InvalidArgument(format!(
            "unknown background task '{id}': never spawned in this session, or its result aged out of retention"
        )));
    }
    let timeout = timeout_arg(args)?;
    let cursor = cursor_arg(args)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    // Event-driven wait: subscribe once to the task's status channel and
    // block on `changed` — no polling loop, wakes as soon as the drain
    // records the terminal status.
    let mut rx = match ctx.manager.bg_watch_rx(&id) {
        Some(rx) => rx,
        None => {
            // Already finished (or aged out): one-shot read decides.
            return match ctx.manager.bg_is_running(&id) {
                Some(true) => {
                    Err(ToolError::Internal("task status channel closed".into()))
                }
                Some(false) => render_output(&ctx.manager, &id, cursor),
                None => Err(ToolError::InvalidArgument(format!(
                    "unknown background task '{id}': never spawned in this session, or its result aged out of retention"
                ))),
            };
        }
    };
    loop {
        match ctx.manager.bg_is_running(&id) {
            None => {
                return Err(ToolError::InvalidArgument(format!(
                    "unknown background task '{id}': never spawned in this session, or its result aged out of retention"
                )));
            }
            Some(false) => return render_output(&ctx.manager, &id, cursor),
            Some(true) => {
                let now = Instant::now();
                if cancel.is_cancelled() || now >= deadline {
                    let mut out = render_output(&ctx.manager, &id, cursor)?;
                    out = format!("still running after {timeout}s\n{out}");
                    return Ok(out);
                }
                let remaining = deadline - now;
                // Race the status change against the deadline and the
                // cancellation waiter; either one wins and the loop
                // re-checks the authoritative registry state.
                tokio::select! {
                    changed = rx.changed() => {
                        if changed.is_err() {
                            // Sender dropped: the drain either finished the
                            // task or the row was evicted — re-read decides.
                            continue;
                        }
                    }
                    _ = tokio::time::sleep(remaining) => {
                        continue;
                    }
                    _ = crate::agent::state::wait_cancelled(cancel) => {
                        let mut out = render_output(&ctx.manager, &id, cursor)?;
                        out = format!("wait cancelled\n{out}");
                        return Ok(out);
                    }
                }
            }
        }
    }
}

async fn bg_list(ctx: &Arc<AgentTurnContext>) -> Result<String, ToolError> {
    let rows = ctx.manager.bg_snapshot();
    if rows.is_empty() {
        return Ok("no background tasks".to_string());
    }
    Ok(rows
        .iter()
        .map(|(id, status, rest)| format!("{id}  {status}  {rest}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::GlobalCancellation;
    use crate::tools::Policy;

    fn test_ctx(cwd: &str) -> Arc<AgentTurnContext> {
        Arc::new(AgentTurnContext {
            depth: 0,
            session_id: "sess".to_string(),
            session_path: PathBuf::new(),
            cwd: cwd.to_string(),
            config: Arc::new(crate::llm::config::tests::test_cfg()),
            manager: AgentManager::new("sess"),
            session_approvals: Default::default(),
            child_approvals: None,
            child_questions: None,
            live_approvals: None,
        })
    }

    fn policy_for(ctx: &Arc<AgentTurnContext>) -> Policy {
        Policy {
            mode: crate::protocol::PermissionMode::Trusted,
            console: None,
            agent: Some(ctx.clone()),
            approval: None,
        }
    }

    fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn background_is_one_tool_with_five_actions() {
        assert!(is_task(TASK_TOOL));
        assert_eq!(
            BACKGROUND_ACTIONS,
            ["spawn", "output", "stop", "wait", "list"]
        );
        assert!(!is_task("bash"));
    }

    #[tokio::test]
    async fn background_without_a_daemon_context_rejects_cleanly() {
        let policy = Policy::trusted();
        for action in BACKGROUND_ACTIONS {
            let a = args(&[("action", Value::String(action.to_string()))]);
            let error = execute_task(TASK_TOOL, &a, &GlobalCancellation, &policy, None)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("daemon-backed"),
                "{action}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn background_missing_action_errors_cleanly() {
        let ctx = test_ctx("/tmp");
        let policy = policy_for(&ctx);
        let error = execute_task(TASK_TOOL, &Map::new(), &GlobalCancellation, &policy, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("action"), "{error}");
        ctx.manager.shutdown().await;
    }

    #[tokio::test]
    async fn background_unknown_id_errors_cleanly() {
        let ctx = test_ctx("/tmp");
        let policy = policy_for(&ctx);
        for action in ["output", "stop", "wait"] {
            let a = args(&[
                ("action", Value::String(action.to_string())),
                ("id", Value::String("task-99".to_string())),
            ]);
            let error = execute_task(TASK_TOOL, &a, &GlobalCancellation, &policy, None)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("task-99"), "{action}: {error}");
        }
        ctx.manager.shutdown().await;
    }

    #[tokio::test]
    async fn background_echo_round_trip() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            ("command", Value::String("echo hello-bg".to_string())),
        ]);
        let out = execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(out.contains("task-1"), "{out}");
        // Wait up to 10s for the echo to exit, then read.
        let wait = args(&[
            ("action", Value::String("wait".to_string())),
            ("id", Value::String("task-1".to_string())),
            ("timeout_secs", Value::from(10)),
        ]);
        let waited = execute_task(TASK_TOOL, &wait, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(waited.contains("hello-bg"), "{waited}");
        assert!(waited.contains("exit 0"), "{waited}");
        let list = args(&[("action", Value::String("list".to_string()))]);
        let listed = execute_task(TASK_TOOL, &list, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(listed.contains("task-1"), "{listed}");
        ctx.manager.shutdown().await;
    }

    #[tokio::test]
    async fn background_stop_kills_sleep() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            ("command", Value::String("sleep 30".to_string())),
        ]);
        execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        let stop = args(&[
            ("action", Value::String("stop".to_string())),
            ("id", Value::String("task-1".to_string())),
        ]);
        let stopped = execute_task(TASK_TOOL, &stop, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(stopped.contains("stopped"), "{stopped}");
        // The drain observes the SIGKILL shortly after.
        let wait = args(&[
            ("action", Value::String("wait".to_string())),
            ("id", Value::String("task-1".to_string())),
            ("timeout_secs", Value::from(10)),
        ]);
        let waited = execute_task(TASK_TOOL, &wait, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(waited.contains("killed"), "{waited}");
        ctx.manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_sends_sigterm_first_and_wellbehaved_task_exits_clean() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            (
                "command",
                Value::String(
                    "bash -c \"trap 'echo term-received; exit 0' TERM; sleep 60 & wait\""
                        .to_string(),
                ),
            ),
        ]);
        execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        // Let the trap install before signaling.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stop = args(&[
            ("action", Value::String("stop".to_string())),
            ("id", Value::String("task-1".to_string())),
        ]);
        execute_task(TASK_TOOL, &stop, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        let wait = args(&[
            ("action", Value::String("wait".to_string())),
            ("id", Value::String("task-1".to_string())),
            ("timeout_secs", Value::from(10)),
        ]);
        let waited = execute_task(TASK_TOOL, &wait, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(waited.contains("exit 0"), "{waited}");
        assert!(waited.contains("term-received"), "{waited}");
        ctx.manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_escalates_to_sigkill_when_sigterm_is_ignored() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            (
                "command",
                Value::String(
                    "bash -c \"trap '' TERM; while true; do sleep 0.2; done\"".to_string(),
                ),
            ),
        ]);
        execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let stop = args(&[
            ("action", Value::String("stop".to_string())),
            ("id", Value::String("task-1".to_string())),
        ]);
        execute_task(TASK_TOOL, &stop, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        let wait = args(&[
            ("action", Value::String("wait".to_string())),
            ("id", Value::String("task-1".to_string())),
            ("timeout_secs", Value::from(15)),
        ]);
        let waited = execute_task(TASK_TOOL, &wait, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(waited.contains("killed (SIGKILL)"), "{waited}");
        ctx.manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_death_is_recorded_with_the_signal() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            (
                "command",
                Value::String("bash -c 'kill -TERM $$'".to_string()),
            ),
        ]);
        execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        let wait = args(&[
            ("action", Value::String("wait".to_string())),
            ("id", Value::String("task-1".to_string())),
            ("timeout_secs", Value::from(10)),
        ]);
        let waited = execute_task(TASK_TOOL, &wait, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        assert!(waited.contains("killed (SIGTERM)"), "{waited}");
        ctx.manager.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_running_task_and_records_terminal() {
        let dir = std::env::temp_dir();
        let ctx = test_ctx(dir.to_str().unwrap());
        let policy = policy_for(&ctx);
        let spawn = args(&[
            ("action", Value::String("spawn".to_string())),
            ("command", Value::String("sleep 60".to_string())),
        ]);
        execute_task(TASK_TOOL, &spawn, &GlobalCancellation, &policy, None)
            .await
            .unwrap();
        ctx.manager.shutdown().await;
        // Row is terminal (SIGTERM from the graceful phase), not "running".
        assert_eq!(ctx.manager.bg_is_running("task-1"), Some(false));
        let status = ctx.manager.bg_status_word("task-1").unwrap_or_default();
        assert!(
            status.contains("killed"),
            "expected killed word, got {status}"
        );
    }
}
