use super::super::App;
use crate::client::http::ChatOptions;
use crate::client::http::DaemonClient;
use crate::protocol::StreamEvent;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use tokio::sync::mpsc;

/// Process start for the `ready in …` session-start line. Marked at `main()`
/// entry so the duration covers the full cold start the
/// `perf(daemon,llm): cut TUI cold start ~780ms to ~160ms` commit optimized
/// (daemon spawn + session-registry scan + models.dev catalog parse +
/// config/session/skills fetch), not just the TUI half after daemon boot.
pub(crate) static LAUNCH_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn mark_launch_start() {
    LAUNCH_START.get_or_init(Instant::now);
}

/// Messages flowing from per-turn worker / background tasks into the UI loop.
/// The worker side is async (tasks), the UI loop stays sync crossterm; only the
/// worker side is async (Phase 5 bridge).
pub(crate) enum WorkerMessage {
    /// A stream event from the daemon.
    Stream(StreamEvent),
    /// The turn's final journal cursor (V1b): the idle events poller resumes
    /// past everything the turn's own SSE already delivered.
    Cursor(u64),
    /// The SSE stream closed; carries a transport error if any.
    Finished(Option<String>),
    /// Background git poll result (off UI thread, every 2s).
    Git(crate::protocol::GitInfo),
    /// A direct shell run (`!`/`!!` prefix) finished on the daemon. Rendered
    /// as a `bash` tool block; the daemon saved it to session history (`!`
    /// feeds the next turn, `!!` stays out of the model context).
    Shell {
        command: String,
        output: String,
        success: bool,
        duration: f64,
        excluded: bool,
    },
}

/// Client-server TUI: renders the exact same `App` view as the local engine,
/// but every turn is executed by the daemon and streamed back over SSE.
pub(crate) struct RemoteApp {
    pub(crate) app: App,
    pub(crate) client: DaemonClient,
    pub(crate) session_id: String,
    pub(crate) options: ChatOptions,
    pub(crate) worker_tx: mpsc::Sender<WorkerMessage>,
    pub(crate) worker_rx: mpsc::Receiver<WorkerMessage>,
    /// A `!`/`!!` shell run is in flight on the worker (one bash at a
    /// time — a second is refused until this one finishes). Independent of
    /// `app.busy`: a shell may overlap an agent turn.
    pub(crate) shell_running: bool,
    /// A cancel for the in-flight shell was already requested (second
    /// Ctrl+C force-quits instead of re-sending, mirroring the turn path).
    pub(crate) shell_cancel_requested: bool,
    /// Shared with the active worker so approvals arriving after a cancel
    /// request are denied instead of parking the turn on the overlay.
    pub(crate) cancel_flag: Arc<AtomicBool>,
    /// Last left press (time, transcript cell, consecutive-click count) for
    /// double-/triple-click detection; the count caps at 3.
    pub(crate) last_click: Option<(Instant, (usize, usize), u8)>,
    /// Shared with the idle events poller: true while a child agent may be
    /// live (§15: the client polls `GET /events?since=` once it knows
    /// children can be running) and while a turn is streaming (the poller
    /// pauses so live SSE rows are never duplicated).
    pub(crate) live_children: Arc<AtomicBool>,
    pub(crate) busy_poll: Arc<AtomicBool>,
    /// Highest journal seq delivered to the UI (V1b): the idle poller's
    /// resume cursor, shared with it. The turn worker publishes its SSE
    /// cursor as it streams; the poller advances it while idle, so rows
    /// a live turn already rendered are never re-fetched.
    pub(crate) events_cursor: Arc<AtomicU64>,
}
