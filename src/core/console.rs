#![allow(dead_code, unused_variables, unused_imports)]
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tokio::sync::{mpsc, Notify};

use crate::agent::state::CancellationSource;

use crate::core::types::{ApprovalRequest, SinkLine};

pub(crate) static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigint(_: i32) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Unix: `sigaction` (not `signal`) WITHOUT SA_RESTART, so a SIGINT
/// arriving while blocked in read(2) returns EINTR instead of restarting
/// the syscall — the flag is then observed promptly.
#[cfg(unix)]
pub(crate) fn install_sigint_handler() {
    // Minimal libc binding so we don't need the libc crate.
    #[repr(C)]
    struct SigAction {
        handler: extern "C" fn(i32),
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    unsafe extern "C" {
        fn sigaction(signum: i32, act: *const SigAction, old: *mut SigAction) -> i32;
    }
    let action = SigAction {
        handler: handle_sigint,
        mask: [0; 16],
        flags: 0, // no SA_RESTART => read() returns EINTR
        restorer: 0,
    };
    unsafe {
        sigaction(2, &action, std::ptr::null_mut());
    }
}

/// Windows: CRT `signal()` for SIGINT. The handler runs on a console
/// control thread; the flag is consumed by `take_interrupt()` polling.
#[cfg(windows)]
pub(crate) fn install_sigint_handler() {
    unsafe extern "C" {
        fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    }
    unsafe {
        signal(2, handle_sigint);
    }
}

/// Returns true once per interrupt (consumes the flag).
pub(crate) fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Sticky interrupt state: true once Ctrl+C has been pressed, and stays true
/// until the flag is consumed via `take_interrupt`. Use for poll-based loops
/// that check cancellation every iteration.
pub(crate) fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// A cancellation signal scoped to a single agent turn. Unlike the
/// process-global `CANCEL_REQUESTED` flag, a token is per-session, idempotent,
/// and not sticky: once dropped the turn it cancelled is finished and a new
/// turn gets a fresh, un-cancelled token. This prevents one client's Cancel
/// from leaking into another session, and prevents a stale cancellation from
/// spuriously aborting a later turn.
#[derive(Clone)]
pub(crate) struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Signal cancellation for the turn this token belongs to.
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// True until `reset` is called; safe to poll from worker threads.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Consumes the cancellation: returns true if it was set and resets it,
    /// mirroring `take_interrupt`.
    pub(crate) fn take_cancelled(&self) -> bool {
        self.cancelled.swap(false, Ordering::SeqCst)
    }

    /// Async wait for cancellation: resolves immediately when already
    /// cancelled, otherwise when `cancel()` fires. Powers
    /// `tokio::select!` in async LLM/SSE/tool/daemon paths (instant cancel
    /// instead of 25-50ms poll quanta).
    pub(crate) async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationSource for CancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
    fn take_cancelled(&self) -> bool {
        self.take_cancelled()
    }
}

pub(crate) const RESET: &str = "\x1b[0m";

pub(crate) const DIM: &str = "\x1b[2m";

pub(crate) const TOOL_INPUT_COLOR: &str = "\x1b[1;33m";

pub(crate) const TOOL_OUTPUT_COLOR: &str = "\x1b[0;34m";

pub(crate) const AGENT_COLOR: &str = "\x1b[1;32m";

pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

pub(crate) static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) static TOOL_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) static SPINNER_RUNNING: AtomicBool = AtomicBool::new(false);

pub(crate) static SPINNER_DRAWN: AtomicBool = AtomicBool::new(false);

/// Bundles the output sinks used by a single agent turn. Previously these were
/// process-global statics (CONSOLE_SINK / APPROVAL_SINK / SESSION_APPROVALS);
/// threading a `Console` through `process_turn` removes the singleton that the
/// REPL worker thread used to read via crate-level globals.
pub(crate) struct Console {
    sink: Option<mpsc::Sender<SinkLine>>,
    approval: Option<mpsc::Sender<ApprovalRequest>>,
    session_approvals: Mutex<Option<HashSet<String>>>,
    /// When true, the daemon handles approvals remotely via SSE instead of
    /// prompting on stdin. The approval channel is still used to send requests;
    /// a separate mechanism resolves them when the client POSTs back.
    pub(crate) remote_approval: bool,
    /// Redacted per-turn observability journal (P9). Optional; the daemon
    /// opens one per turn (`<session>.trace.jsonl`, `0600`), local paths skip it.
    trace: Option<TraceWriter>,
}

/// Redacted event span appended to a per-turn `trace.jsonl` (P9).
/// Field meanings are fixed; no prompts, tool args, or secrets are ever
/// written — only hashes and counters, so a trace is safe to ship to cost
/// tooling.
#[derive(Clone)]
pub(crate) struct TraceWriter {
    file: Arc<Mutex<std::fs::File>>,
}

impl TraceWriter {
    /// Open (append) a trace file with `0600` permissions on unix.
    pub(crate) fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        let file = trace_file(path)?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub(crate) fn record(&self, span: serde_json::Value) {
        use std::io::Write;
        if let Ok(mut file) = self.file.lock() {
            let mut line = span.to_string();
            line.push('\n');
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

/// Unix: create/append with `0600` so traces stay private to the user.
#[cfg(unix)]
fn trace_file(path: std::path::PathBuf) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// No permission bits to set; plain create/append.
#[cfg(not(unix))]
fn trace_file(path: std::path::PathBuf) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

impl Clone for Console {
    fn clone(&self) -> Self {
        Self {
            sink: self.sink.clone(),
            approval: self.approval.clone(),
            session_approvals: Mutex::new(
                self.session_approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
            ),
            remote_approval: self.remote_approval,
            trace: self.trace.clone(),
        }
    }
}

impl Console {
    #[allow(dead_code)]
    pub(crate) fn new(
        sink: mpsc::Sender<SinkLine>,
        approval: mpsc::Sender<ApprovalRequest>,
    ) -> Self {
        Self {
            sink: Some(sink),
            approval: Some(approval),
            session_approvals: Mutex::new(None),
            remote_approval: false,
            trace: None,
        }
    }

    /// A console with no sinks: streamed output prints directly to the terminal
    /// (used by the one-shot CLI path).
    pub(crate) fn none() -> Self {
        Self {
            sink: None,
            approval: None,
            session_approvals: Mutex::new(None),
            remote_approval: false,
            trace: None,
        }
    }

    /// Create a console for the daemon that handles approvals remotely.
    pub(crate) fn daemon(
        sink: mpsc::Sender<SinkLine>,
        approval: mpsc::Sender<ApprovalRequest>,
    ) -> Self {
        Self {
            sink: Some(sink),
            approval: Some(approval),
            session_approvals: Mutex::new(None),
            remote_approval: true,
            trace: None,
        }
    }

    /// Attach a trace journal (P9). Returns a clone (the same file).
    pub(crate) fn with_trace(mut self, trace: Option<TraceWriter>) -> Self {
        self.trace = trace;
        self
    }

    /// Record one span, if a trace journal is attached. Field names are the
    /// contract; see `TraceWriter`.
    pub(crate) fn trace_span(&self, mut span: serde_json::Value) {
        if let Some(trace) = &self.trace {
            if let Some(obj) = span.as_object_mut() {
                obj.insert(
                    "ts".into(),
                    serde_json::json!(chrono::Utc::now().to_rfc3339()),
                );
            }
            trace.record(span);
        }
    }

    /// Seed the per-console session-approval set from the daemon's
    /// persisted map (so “allow for session” survives across turns).
    pub(crate) fn seed_session_approvals(&self, set: std::collections::HashSet<String>) {
        let mut guard = self
            .session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = guard.get_or_insert_with(std::collections::HashSet::new);
        entry.extend(set);
    }

    pub(crate) fn sink(&self) -> Option<&mpsc::Sender<SinkLine>> {
        self.sink.as_ref()
    }

    pub(crate) fn approval(&self) -> Option<&mpsc::Sender<ApprovalRequest>> {
        self.approval.as_ref()
    }

    /// Sync emit for legacy sync callers (printer, tool summaries): never
    /// blocks, drops when full like the old `.send().ok()`.
    pub(crate) fn emit(&self, line: SinkLine) {
        if let Some(sink) = &self.sink {
            let _ = sink.try_send(line);
        }
    }

    /// Async emit for async turn/SSE/tool paths: back-pressured `send().await`.
    pub(crate) async fn emit_async(&self, line: SinkLine) {
        if let Some(sink) = &self.sink {
            let _ = sink.send(line).await;
        }
    }

    pub(crate) fn approval_key(name: &str, input: &str) -> String {
        // Scope approvals: write/edit -> path, bash -> command, else full input hash.
        let relevant = if matches!(name, "write" | "edit") {
            serde_json::from_str::<serde_json::Value>(input)
                .ok()
                .and_then(|v| {
                    v.get("path")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| input.to_string())
        } else if name == "bash" {
            serde_json::from_str::<serde_json::Value>(input)
                .ok()
                .and_then(|v| {
                    v.get("command")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or_else(|| input.to_string())
        } else {
            input.to_string()
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        relevant.hash(&mut hasher);
        format!("{}:{:016x}", name, hasher.finish())
    }

    pub(crate) fn session_approved(&self, name: &str, input: &str) -> bool {
        let key = Self::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|approved| approved.contains(&key))
    }

    pub(crate) fn record_session_approval(&self, name: &str, input: &str) {
        let key = Self::approval_key(name, input);
        self.session_approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashSet::new)
            .insert(key);
    }
}

/// Erase the drawn spinner frame, if any. Caller holds CONSOLE_LOCK.
pub(crate) fn erase_spinner_frame() {
    if SPINNER_DRAWN.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\r\x1b[2K");
        let _ = out.flush();
    }
}

/// Run `f` with the spinner suspended so output never interleaves with frames.
/// `ui_mode` is true when streamed output is routed through a sink instead of
/// the terminal (e.g. the ratatui REPL), in which case console IO is skipped.
pub(crate) fn with_console(ui_mode: bool, f: impl FnOnce()) {
    if ui_mode {
        return; // UI mode: output is routed through the sink; skip console IO
    }
    let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    erase_spinner_frame();
    f()
}

/// Animates `<label> ...` frames until dropped (no-op when stdout is piped).
pub(crate) struct SpinnerGuard {
    #[allow(dead_code)]
    pub(crate) worker: Option<thread::JoinHandle<()>>,
}

impl SpinnerGuard {
    pub(crate) fn start(console: &Console, label: &str) -> Self {
        if console.sink().is_some() {
            return Self { worker: None }; // UI mode shows "working…" in the status bar
        }
        if !io::stdout().is_terminal() {
            return Self { worker: None };
        }
        SPINNER_RUNNING.store(true, Ordering::SeqCst);
        let label = label.to_string();
        let worker = thread::spawn(move || {
            let mut out = io::stdout();
            for frame in SPINNER_FRAMES.iter().cycle() {
                if !SPINNER_RUNNING.load(Ordering::SeqCst) {
                    break;
                }
                {
                    let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                    if !SPINNER_RUNNING.load(Ordering::SeqCst) {
                        break;
                    }
                    let _ = write!(out, "\r\x1b[2K{}{frame}{RESET} {label} ...", AGENT_COLOR);
                    let _ = out.flush();
                    SPINNER_DRAWN.store(true, Ordering::SeqCst);
                }
                thread::sleep(Duration::from_millis(80));
            }
        });
        Self {
            worker: Some(worker),
        }
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        if self.worker.take().is_some() {
            // Final erase under the lock: after RUNNING flips false no new
            // frame can appear, and the worker exits on its next tick
            // without blocking turn teardown.
            let _lock = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            SPINNER_RUNNING.store(false, Ordering::SeqCst);
            erase_spinner_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_token_is_per_turn_not_global() {
        let a = CancellationToken::new();
        let b = a.clone();
        assert!(!a.is_cancelled());
        a.cancel();
        assert!(a.is_cancelled());
        assert!(b.is_cancelled()); // clone shares state
        assert!(a.take_cancelled());
        assert!(!a.is_cancelled());
        // fresh token is independent
        let c = CancellationToken::new();
        assert!(!c.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_token_cancelled_resolves_without_poll() {
        // TDD: stalled stream + cancel() must resolve instantly, not on the
        // next SSE line / 50ms poll quantum.
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        // Already-cancelled resolves without waiting.
        tokio::time::timeout(Duration::from_millis(10), token.cancelled())
            .await
            .expect("cancelled() must resolve instantly when already cancelled");
        // Notified waiter resolves when cancel() fires from another task.
        let token2 = CancellationToken::new();
        let waiter = token2.clone();
        let handle = tokio::spawn(async move { waiter.cancelled().await });
        tokio::time::sleep(Duration::from_millis(5)).await;
        token2.cancel();
        tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("waiter must wake on cancel()")
            .unwrap();
    }

    #[test]
    fn approval_key_scopes_by_path_or_command() {
        let k1 = Console::approval_key("write", r#"{"path":"a.txt","content":"hi"}"#);
        let k2 = Console::approval_key("write", r#"{"path":"a.txt","content":"other"}"#);
        let k3 = Console::approval_key("write", r#"{"path":"b.txt","content":"hi"}"#);
        assert_eq!(k1, k2, "same path should share key regardless of content");
        assert_ne!(k1, k3);

        let kb1 = Console::approval_key("bash", r#"{"command":"rm -rf /"}"#);
        let kb2 = Console::approval_key("bash", r#"{"command":"rm -rf /"}"#);
        let kb3 = Console::approval_key("bash", r#"{"command":"ls"}"#);
        assert_eq!(kb1, kb2);
        assert_ne!(kb1, kb3);
    }

    #[test]
    fn session_approvals_are_scoped_and_recorded() {
        let (tx, _rx) = mpsc::channel(16);
        let (atx, _arx) = mpsc::channel(16);
        let console = Console::new(tx, atx);
        let input = r#"{"path":"foo.rs","content":"x"}"#;
        assert!(!console.session_approved("write", input));
        console.record_session_approval("write", input);
        assert!(console.session_approved("write", input));
        // different path not approved
        assert!(!console.session_approved("write", r#"{"path":"bar.rs"}"#));
    }

    #[test]
    fn console_daemon_sets_remote_flag() {
        let (tx, _rx) = mpsc::channel(16);
        let (atx, _arx) = mpsc::channel(16);
        let c = Console::daemon(tx, atx);
        assert!(c.remote_approval);
        assert!(c.sink().is_some());
    }
}
