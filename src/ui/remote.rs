use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
    KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;

use crate::cli::Args;
use crate::client::http::{ChatOptions, DaemonClient};
use crate::core::types::{
    ApiProtocol, ApprovalDecision as CoreApprovalDecision, PermissionMode, Provider, SinkLine,
};
use crate::protocol::{ApprovalDecision as ProtocolApprovalDecision, DaemonInfo, StreamEvent};
use crate::session::Session;

use super::slash::{
    complete_slash, dismiss_slash, expand_bare_command, handle_slash, reset_session_state,
    slash_suggestions, EXPAND_ON_ENTER,
};
use super::{
    append_sink_line, bump_thinking_stamps, close_thinking, flush_assistant, line_selection_text,
    line_width, mouse_display_cell, push_banner, push_info, push_info_line, render_user_prompt,
    resolve_approval, scroll_transcript, selection_text, view, word_bounds, App, EnableMouseScroll,
    PendingApproval, Selection, TerminalCleanup,
};

/// Process start for the `ready in …` session-start line. Marked at `main()`
/// entry so the duration covers the full cold start the
/// `perf(daemon,llm): cut TUI cold start ~780ms to ~160ms` commit optimized
/// (daemon spawn + session-registry scan + models.dev catalog parse +
/// config/session/skills fetch), not just the TUI half after daemon boot.
static LAUNCH_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn mark_launch_start() {
    LAUNCH_START.get_or_init(Instant::now);
}

/// Messages flowing from per-turn worker / background tasks into the UI loop.
/// The worker side is async (tasks), the UI loop stays sync crossterm; only the
/// worker side is async (Phase 5 bridge).
enum WorkerMessage {
    /// A stream event from the daemon.
    Stream(StreamEvent),
    /// The SSE stream closed; carries a transport error if any.
    Finished(Option<String>),
    /// Background git poll result (off UI thread, every 2s).
    Git(crate::protocol::GitInfo),
}

/// Client-server TUI: renders the exact same `App` view as the local engine,
/// but every turn is executed by the daemon and streamed back over SSE.
struct RemoteApp {
    app: App,
    client: DaemonClient,
    session_id: String,
    options: ChatOptions,
    worker_tx: mpsc::Sender<WorkerMessage>,
    worker_rx: mpsc::Receiver<WorkerMessage>,
    /// Paired with the current turn's worker; approval overlays resolve
    /// through it.
    decision_tx: mpsc::Sender<CoreApprovalDecision>,
    /// Shared with the active worker so approvals arriving after a cancel
    /// request are denied instead of parking the turn on the overlay.
    cancel_flag: Arc<AtomicBool>,
    /// Last left press (time, transcript cell, consecutive-click count) for
    /// double-/triple-click detection; the count caps at 3.
    last_click: Option<(Instant, (usize, usize), u8)>,
}

/// How often an idle TUI re-polls `GET /api/git` for the footer. The daemon
/// caches branch/dirty for 5s, so a 2s poll costs at most one `git` spawn
/// per 5s — cheap enough to catch an external `git checkout` within seconds
/// without the per-frame (~10-30ms) cost of `git branch + git status`.
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

fn apply_git_info(remote: &mut RemoteApp, info: &crate::protocol::GitInfo) -> bool {
    let changed =
        remote.app.git_branch != info.git_branch || remote.app.git_dirty != info.git_dirty;
    if changed {
        remote.app.git_branch = info.git_branch.clone();
        remote.app.git_dirty = info.git_dirty;
    }
    changed
}

/// Background git poll off the UI thread (Phase 5): task polls every 2s via
/// `get_git_async`, pushes into the worker channel; UI loop only applies.
/// Daemon 5s cache stays; per-frame cost zero even when busy.
fn spawn_git_poller(client: DaemonClient, tx: tokio::sync::mpsc::Sender<WorkerMessage>) {
    crate::client::http::spawn_task(async move {
        loop {
            tokio::time::sleep(GIT_REFRESH_INTERVAL).await;
            match client.get_git_async().await {
                Ok(info) => {
                    let _ = tx.send(WorkerMessage::Git(info)).await;
                }
                Err(_) => continue,
            }
        }
    });
}

/// One-off footer refresh without blocking the UI thread: fetches
/// `GET /api/git` on the shared runtime and forwards the result through the
/// worker channel (daemon 5s cache keeps it cheap). Used at turn end so
/// tool mutations show up immediately.
fn refresh_git_async(client: DaemonClient, tx: mpsc::Sender<WorkerMessage>) {
    crate::client::http::spawn_task(async move {
        if let Ok(info) = client.get_git_async().await {
            let _ = tx.send(WorkerMessage::Git(info)).await;
        }
    });
}

/// Maps a UI overlay decision to the wire protocol. Pure so approval routing
/// is unit-testable without a daemon or TUI.
fn map_approval_decision(decision: CoreApprovalDecision) -> ProtocolApprovalDecision {
    match decision {
        CoreApprovalDecision::Once => ProtocolApprovalDecision::AllowOnce,
        CoreApprovalDecision::Session => ProtocolApprovalDecision::AllowSession,
        CoreApprovalDecision::Deny => ProtocolApprovalDecision::Deny,
    }
}

/// Next approval decision for the worker task. Auto-denies when a cancel was
/// requested (so the turn unwinds without parking on the overlay); otherwise
/// awaits the overlay. A closed channel means the turn went away — deny.
async fn next_worker_decision(
    cancel_flag: &AtomicBool,
    decision_rx: &mut mpsc::Receiver<CoreApprovalDecision>,
) -> ProtocolApprovalDecision {
    if cancel_flag.load(Ordering::SeqCst) {
        return ProtocolApprovalDecision::Deny;
    }
    map_approval_decision(
        decision_rx
            .recv()
            .await
            .unwrap_or(CoreApprovalDecision::Deny),
    )
}

/// Terminal-close classification for the worker task. The daemon always ends
/// a turn with `TurnComplete` / `TurnFailed`; closing without one is a
/// transport failure and must surface as an error, not silent success.
fn premature_close_error(saw_terminal: bool) -> Option<String> {
    (!saw_terminal).then(|| "connection closed before turn completed".to_string())
}

/// Build a display-only config from the daemon's reported runtime info. The
/// client never talks to the model provider itself; this only feeds the
/// status footer and slash-command suggestions.
fn display_config(info: &DaemonInfo) -> crate::llm::config::LlmConfig {
    let provider = Provider::from_display(&info.provider);
    crate::llm::config::LlmConfig {
        provider: provider.clone(),
        api_key: String::new(),
        base_url: String::new(),
        model: info.model.clone(),
        available_models: if info.available_models.is_empty() {
            vec![info.model.clone()]
        } else {
            info.available_models.clone()
        },
        // Mirror the daemon's endpoint table so `/model endpoint/id`
        // strips and displays exactly what the daemon will route.
        // (The display copy never talks to a provider itself.)
        endpoints: provider.endpoints(),
        api: ApiProtocol::parse(&info.api).unwrap_or(ApiProtocol::Responses),
        account_id: None,
        thinking_effort: info.thinking_effort.clone(),
        context_window: info.context_window,
        reserve_tokens: 16_384,
        keep_recent_tokens: 20_000,
        permission: PermissionMode::parse(&info.permission).unwrap_or(PermissionMode::AskWrites),
        verify_command: None,
        extra_headers: Default::default(),
        provider_entries: Default::default(),
        provider_headers: Default::default(),
        api_pinned: false,
        // Display-only copy never talks to a provider; share the
        // process-wide client instead of initializing TLS + pool.
        client: crate::client::http::shared_async_client(),
    }
}

/// Show which skills the daemon has loaded. Used at session start and after
/// `/new`, so the user can see what `/skill:<name>` can load.
fn push_skills_listing(app: &mut App) {
    match skills_listing_line(&app.skills) {
        Some(line) => push_info_line(app, line),
        None => push_info(
            app,
            "no skills loaded (add .dex/skills/<name>/SKILL.md or ~/.config/dex/skills)"
                .to_string(),
        ),
    }
}

/// The session-start skills line: names comma-separated on a single row, in
/// the terminal's own foreground (`theme::surface_fg`, resolved from the real
/// palette so it follows the active theme) with the count header and hint
/// quiet. `None` when no skills are loaded.
fn skills_listing_line(skills: &[crate::core::types::Skill]) -> Option<Line<'static>> {
    let (first, rest) = skills.split_first()?;
    let mut names = first.name.clone();
    for skill in rest {
        names.push_str(", ");
        names.push_str(&skill.name);
    }
    Some(Line::from(vec![
        Span::styled(
            format!("skills loaded ({}): ", skills.len()),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(names, Style::default().fg(super::theme::surface_fg())),
        Span::styled(
            " · /skill:<name> loads one",
            Style::default().fg(super::theme::muted_fg()),
        ),
    ]))
}

/// The session-start launch-time line, shown below the skills listing so
/// users can see how fast the TUI was ready to use. Muted so it stays
/// quiet next to the skills line.
fn launch_time_line(elapsed_secs: f64) -> Line<'static> {
    Line::from(vec![Span::styled(
        format!(
            "ready in {}",
            crate::core::format::format_duration(elapsed_secs)
        ),
        Style::default().fg(super::theme::muted_fg()),
    )])
}

pub(crate) fn run_ratatui_repl_with_remote(args: &Args, daemon_url: &str) -> std::io::Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(std::io::Error::other(
            "interactive UI requires a terminal (TTY); use `dex connect <url> \"prompt\"` for one-shot",
        ));
    }
    let launch_start = *LAUNCH_START.get_or_init(Instant::now);
    OSC_START.get_or_init(Instant::now);

    let client = DaemonClient::new(daemon_url)
        .map_err(|e| std::io::Error::other(format!("failed to connect to daemon: {e}")))?;
    client
        .wait_until_ready(Duration::from_secs(10))
        .map_err(|e| std::io::Error::other(format!("daemon not ready: {e}")))?;

    // The daemon owns the model/provider/permission and the workspace; mirror
    // its state so the UI shows what turns will actually use.
    let info = client
        .get_config()
        .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;

    // Session create and skills fetch are independent (`create_session`
    // only needs `info.cwd` above), so overlap them: `tokio::join!` on the
    // shared runtime (one RTT + one daemon-side scan off the critical path).
    // Detach-on-error preserved: skills task is spawned, session awaited first;
    // on session failure we return without awaiting skills (handle drop detaches).
    let skills_client = client.clone();
    let skills_handle = crate::client::http::spawn_task(async move {
        skills_client.list_skills_async().await.unwrap_or_default()
    });
    // Sessions default to `<workspace>-<7 chars>` (k8s-style); an explicit
    // `--name` wins. Generated client-side so the local placeholder shows the
    // same name the daemon persists.
    let session_name = args
        .session_name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| Session::default_session_name(&info.cwd));
    let session_result: Result<(String, bool), String> = if let Some(reattach) = &args.reattach {
        // P10: attach to an existing persisted session on the daemon and get
        // the replay cursor, instead of creating a fresh one.
        client
            .reattach(reattach)
            .map(|resp| (resp.session_id, true))
            .map_err(|e| format!("failed to reattach session: {e}"))
    } else {
        client
            .create_session(&info.cwd, Some(&session_name))
            .map(|resp| (resp.session_id, false))
            .map_err(|e| format!("failed to create session: {e}"))
    };
    let (session_id, is_reattach) = match session_result {
        // Dropping the JoinHandle detaches the skills task; error path never waits.
        Err(e) => return Err(std::io::Error::other(e)),
        Ok(ok) => (ok.0, ok.1),
    };
    let daemon_skills = crate::client::http::block_on(skills_handle).unwrap_or_default();
    // Skills live on the daemon (its workspace); a stale list is harmless —
    // the load call re-discovers on the daemon side.
    let tui_skills: Vec<crate::core::types::Skill> = daemon_skills
        .into_iter()
        .map(|info| crate::core::types::Skill {
            name: info.name,
            description: info.description,
            path: std::path::PathBuf::from(""),
        })
        .collect();

    // Per-request overrides so client flags keep working in remote mode.
    let options = ChatOptions {
        skill_dirs: args
            .skill_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        permission: args.permission.map(|mode| match mode {
            PermissionMode::ReadOnly => "read-only".to_string(),
            PermissionMode::AskWrites => "ask-writes".to_string(),
            PermissionMode::AskShell => "ask-shell".to_string(),
            PermissionMode::Trusted => "trusted".to_string(),
        }),
        headers: if args.headers.is_empty() {
            None
        } else {
            let mut merged = std::collections::BTreeMap::new();
            for raw in &args.headers {
                for (k, v) in crate::llm::config::parse_headers_str(raw) {
                    crate::llm::config::insert_extra_header(&mut merged, &k, &v);
                }
            }
            Some(merged)
        },
        plan: None,
        idempotency_key: None,
    };

    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>(256);
    let (decision_tx, _decision_rx) = mpsc::channel::<CoreApprovalDecision>(16);
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let app = App {
        transcript: Vec::new(),
        input: crate::ui::input::InputField::new(),
        config: display_config(&info),
        messages: Vec::new(),
        tool_state: crate::agent::state::ToolState::default(),
        session: Session::in_memory(info.cwd.clone()),
        plan: crate::core::types::Plan::default(),
        skills: tui_skills,
        turn_start: 0,
        cwd: info.cwd.clone(),
        git_branch: info.git_branch.clone(),
        git_dirty: info.git_dirty,
        turn_started: None,
        last_activity: None,
        steering_rx: None,
        followup_rx: None,
        pending_steering: Vec::new(),
        pending_followups: Vec::new(),
        cancel_requested: false,
        approval_rx: None,
        pending_approval: None,
        busy: false,
        autoscroll: true,
        scroll: 0,
        tick: 0,
        quit: false,
        last_ctrl_c: None,
        history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
        slash_selected: 0,
        connection: Some(connection_label(daemon_url)),
        assistant_open: false,
        show_thinking: false,
        thinking_open: false,
        assistant_pending: String::new(),
        assistant_gap: crate::core::markdown::GapState::new(),
        stream_last_flush: Instant::now(),
        wrapped_cache: Vec::new(),
        wrapped_width: 0,
        display_cache: Vec::new(),
        transcript_area: None,
        selection: None,
        notice: None,
    };

    let mut remote = RemoteApp {
        app,
        client: client.clone(),
        session_id: session_id.clone(),
        options,
        worker_tx: worker_tx.clone(),
        worker_rx,
        decision_tx,
        cancel_flag,
        last_click: None,
    };
    // Background git poll off the UI thread (Phase 5): task polls every 2s,
    // pushes into the worker channel; UI loop only applies. Daemon 5s cache
    // stays; per-frame cost zero even when busy.
    spawn_git_poller(client.clone(), worker_tx);
    if !is_reattach {
        // Keep the local placeholder's display name in sync with the daemon
        // record (a reattach overwrites `app.session` from disk below).
        remote.app.session.set_name(session_name).ok();
    }

    // P10: reconstruct the transcript for a reattached session. Prefer the
    // persisted JSONL messages (complete, includes user prompts the events
    // journal never records); fall back to the events journal when the file
    // isn't shared (true remote). Idempotent replays skip stale approvals
    // (parked approvals die with their turn on the daemon).
    if is_reattach {
        let local = find_local_session_file(&remote.session_id);
        let mut rebuilt = false;
        if let Some(p) = local.as_deref() {
            if let Ok(s) = Session::from_path(p) {
                remote.app.session = s;
            }
            if let Some(p) = local.as_deref() {
                rebuilt = rebuild_remote_from_messages(&mut remote, p);
            }
        }
        if !rebuilt {
            let sid = remote.session_id.clone();
            replay_remote_events(&mut remote, &sid);
        }
        push_info(
            &mut remote.app,
            format!("reattached to session {session_id}"),
        );
    }

    // Detect the terminal background before raw mode / the alternate screen
    // take over; surface colors (including the skills listing below) are
    // resolved from this once.
    super::theme::detect_background();

    // Session-start view: the DEX art, then the skills the daemon discovered,
    // then how fast the TUI was ready to use.
    push_banner(&mut remote.app);
    push_skills_listing(&mut remote.app);
    push_info_line(
        &mut remote.app,
        launch_time_line(launch_start.elapsed().as_secs_f64()),
    );
    // A mismatched `thinking_effort:` (config.yaml names a level the model
    // doesn't advertise) used to `eprintln!` from the daemon thread here —
    // mid OSC theme query / alternate screen — corrupting the display and
    // leaking into the composer. It now arrives as data and renders as a
    // transcript line inside the TUI.
    if let Some(warning) = info.thinking_warning.clone() {
        push_info(&mut remote.app, format!("dex: {warning}"));
    }

    enable_raw_mode()?;
    // No startup drain here: a blind deadline cuts OSC reply bursts in half
    // and leaks the tail (sans lead-in) into the composer. Late replies —
    // from the theme query above or from anything else querying this tty,
    // at any time — are swallowed whole by `strip_osc_report` in the event
    // loop below.
    let _cleanup = TerminalCleanup;
    let mut stdout = io::stdout();
    // Wheel reporting (DECSET 1000 + SGR 1006): scroll events arrive as real
    // `Event::Mouse` input instead of the terminal synthesizing Up/Down arrow
    // presses (DECSET 1007), so the wheel always scrolls the transcript and
    // plain Up/Down always edit the composer. Clicks are ignored; hold Shift
    // (Option in iTerm2) for native drag-select/copy.
    execute!(
        stdout,
        DisableMouseCapture,
        EnterAlternateScreen,
        EnableMouseScroll,
        // DECSET 2004: the terminal wraps pastes in `ESC[200~ … ESC[201~` so
        // crossterm delivers them as one `Event::Paste`. Without it a paste is
        // typed through as individual keys and every embedded newline arrives
        // as a real Enter — submitting the first line of a multi-line paste.
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut run = || -> std::io::Result<()> {
        // Events consumed by the OSC-report lookahead, replayed on the next
        // iterations of the loop.
        let mut pending: VecDeque<Event> = VecDeque::new();
        // ponytail: draw on change, not on a 60 fps heartbeat — every frame
        // rebuilds the whole view + ratatui buffer diff (unicode widths per
        // cell), the top CPU cost in the flamegraph. Draw on state change
        // (worker message / input event); while busy also redraw at animation
        // rate for the thinking/working dots, capped well below 60 fps.
        let mut dirty = true;
        let mut last_busy_draw = Instant::now();
        loop {
            // Drain worker messages: the transcript updates live while the
            // turn streams in on the worker thread.
            let mut streamed = false;
            loop {
                match remote.worker_rx.try_recv() {
                    Ok(WorkerMessage::Git(info)) => {
                        if apply_git_info(&mut remote, &info) {
                            dirty = true;
                        }
                    }
                    Ok(WorkerMessage::Stream(event)) => {
                        streamed = true;
                        if remote.app.cancel_requested {
                            match &event {
                                StreamEvent::TurnComplete { .. }
                                | StreamEvent::TurnFailed { .. } => {
                                    handle_stream_event(&mut remote, event);
                                }
                                _ => {}
                            }
                            continue;
                        }
                        handle_stream_event(&mut remote, event);
                    }
                    Ok(WorkerMessage::Finished(error)) => {
                        streamed = true;
                        finish_turn(&mut remote, error);
                    }
                    Err(_) => break,
                }
            }

            let busy = remote.app.busy;
            // An expired status notice needs one more frame to disappear.
            if remote.app.tick_notice() {
                dirty = true;
            }
            // Footer branch/dirty arrives via the background git task (no per-frame
            // `git` or HTTP on the UI thread, even when busy).
            // Animation heartbeat while a turn runs: at most ~8 fps, and
            // streaming/input draws reset the clock so they don't double up.
            let now = Instant::now();
            let anim_due = busy && now.duration_since(last_busy_draw) >= Duration::from_millis(120);
            if dirty || streamed || anim_due {
                // Advance the animation frame so the dots + status update.
                remote.app.tick = remote.app.tick.wrapping_add(1);
                terminal.draw(|f| view(f, &mut remote.app))?;
                dirty = false;
                if busy {
                    last_busy_draw = Instant::now();
                }
            }

            let next = if let Some(event) = pending.pop_front() {
                event
            } else if event::poll(Duration::from_millis(if busy { 100 } else { 250 }))? {
                event::read()?
            } else {
                continue;
            };

            // Drop OSC 10/11 color reports mis-parsed as keystrokes before
            // they can type themselves into the composer.
            let Some(next) = strip_osc_report(next, &mut pending)? else {
                continue;
            };

            match next {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut remote, key);
                }
                Event::Mouse(mouse) => handle_mouse(&mut remote, mouse),
                Event::Paste(s) => {
                    remote.app.input.insert_paste(&s);
                    // A paste can narrow the popup list like typing does.
                    remote.app.slash_selected = 0;
                }
                Event::Resize(..) => {} // frame recomputed each draw
                _ => {}
            }

            dirty = true;

            if remote.app.quit {
                break;
            }
        }
        Ok(())
    };

    let res = run();
    // Always restore the terminal, even if the loop returned early via `?`.
    disable_raw_mode().ok();
    let _ = execute!(
        io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    res
}

/// Window between two left presses on the same transcript cell that counts
/// as a repeated click (double = word select, triple = line select).
const MULTI_CLICK: Duration = Duration::from_millis(500);

/// Mouse routing: wheel scrolls the transcript; left press/drag/release
/// selects transcript text with a visible highlight and copies it to the
/// system clipboard (OSC 52) on release. A click clears; Shift+drag still
/// bypasses mouse reporting for native selection. Repeated presses on the
/// same cell within `MULTI_CLICK` select more: double-click the word under
/// it, triple-click the whole line (and dragging a line select extends it
/// by whole rows). Both keep the highlight until the next click.
fn handle_mouse(remote: &mut RemoteApp, m: event::MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollUp => scroll_transcript(&mut remote.app, -3),
        MouseEventKind::ScrollDown => scroll_transcript(&mut remote.app, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            // Pressing outside the transcript (composer, activity line)
            // clears any live selection instead of starting a new one.
            let cell = mouse_display_cell(
                remote.app.scroll,
                remote.app.transcript_area,
                remote.app.display_cache.len(),
                &m,
            );
            let clicks = match (remote.last_click, cell) {
                (Some((at, last, n)), Some(c)) if at.elapsed() <= MULTI_CLICK && last == c => {
                    (n + 1).min(3)
                }
                _ => 1,
            };
            remote.app.selection = match cell {
                Some((row, col)) if clicks == 2 => word_bounds(&remote.app.display_cache[row], col)
                    .map(|(c0, c1)| Selection {
                        anchor: (row, c0),
                        end: (row, c1),
                        sticky: true,
                        whole_line: false,
                    }),
                Some((row, _)) if clicks == 3 => Some(Selection {
                    anchor: (row, 0),
                    end: (row, line_width(&remote.app.display_cache[row])),
                    sticky: true,
                    whole_line: true,
                }),
                Some(cell) => Some(Selection {
                    anchor: cell,
                    end: cell,
                    sticky: false,
                    whole_line: false,
                }),
                None => None,
            };
            remote.last_click = cell.map(|c| (Instant::now(), c, clicks));
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(sel) = remote.app.selection.as_mut() {
                if let Some(cell) = mouse_display_cell(
                    remote.app.scroll,
                    remote.app.transcript_area,
                    remote.app.display_cache.len(),
                    &m,
                ) {
                    if sel.whole_line {
                        // Line selects extend by whole rows; the anchor row
                        // (the press point) stays put, xterm-style.
                        sel.end = (cell.0, line_width(&remote.app.display_cache[cell.0]));
                    } else {
                        sel.end = cell;
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // Sticky selections (double-click word picks, triple-click line
            // picks) stay highlighted after release until the next press;
            // drag selections clear.
            let sticky = remote.app.selection.is_some_and(|s| s.sticky);
            if let Some(sel) = remote.app.selection {
                if !sel.is_empty() {
                    let text = if sel.whole_line {
                        let r0 = sel.anchor.0.min(sel.end.0);
                        let r1 = sel.anchor.0.max(sel.end.0);
                        line_selection_text(&remote.app.display_cache, r0, r1)
                    } else {
                        let ((r0, c0), (r1, c1)) = sel.norm();
                        selection_text(&remote.app.display_cache, (r0, c0), (r1, c1))
                    };
                    remote.app.copy_selection(&text);
                }
            }
            if !sticky {
                remote.app.selection = None;
            }
        }
        _ => {}
    }
}

fn handle_stream_event(remote: &mut RemoteApp, event: StreamEvent) {
    match event {
        StreamEvent::AssistantText(text) => {
            append_sink_line(&mut remote.app, SinkLine::Assistant(text));
        }
        StreamEvent::Thinking(text) => {
            append_sink_line(&mut remote.app, SinkLine::Thinking(text));
        }
        StreamEvent::ToolCall { name, args } => {
            let preview = args.as_str().unwrap_or_default().to_string();
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolInput(format!("{name} {preview}")),
            );
        }
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            duration,
        } => {
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolOutput {
                    name,
                    summary,
                    success,
                    preview,
                    duration,
                },
            );
        }
        StreamEvent::SteeringAccepted { content } => {
            if let Some(pos) = remote
                .app
                .pending_steering
                .iter()
                .position(|c| c == &content)
            {
                remote.app.pending_steering.remove(pos);
            } else if !remote.app.pending_steering.is_empty() {
                // Fallback: content may have been trimmed differently; pop oldest.
                remote.app.pending_steering.remove(0);
            }
            render_user_prompt(&mut remote.app, &content);
        }
        StreamEvent::FollowupAccepted { content } => {
            if let Some(pos) = remote
                .app
                .pending_followups
                .iter()
                .position(|c| c == &content)
            {
                remote.app.pending_followups.remove(pos);
            } else if !remote.app.pending_followups.is_empty() {
                remote.app.pending_followups.remove(0);
            }
            render_user_prompt(&mut remote.app, &content);
        }
        StreamEvent::ApprovalRequired { name, input, .. } => {
            // Invariant: the daemon parks at most one approval per turn
            // (agent thread blocks until it is resolved), so overwriting would
            // drop the prior sender. If it happens, deny the stale one.
            if let Some(stale) = remote.app.pending_approval.take() {
                let _ = stale.response.try_send(CoreApprovalDecision::Deny);
            }
            remote.app.pending_approval = Some(PendingApproval {
                name,
                input,
                response: remote.decision_tx.clone(),
                selected: 0,
            });
        }
        StreamEvent::TurnComplete { usage, cached, .. } => {
            if let Some(usage) = usage {
                remote.app.tool_state.last_usage = Some(usage);
            }
            remote.app.tool_state.last_cached = cached;
        }
        StreamEvent::Usage {
            tokens,
            cached,
            cost,
            output,
        } => {
            // Live context usage: emitted by the daemon after every LLM call
            // so the status bar updates mid-turn, not just at completion.
            remote.app.tool_state.last_usage = Some(tokens);
            remote.app.tool_state.last_cached = cached;
            // Cumulative spend across turns, priced once daemon-side
            // (catalog or DEX_COST_PER_1K) so client and daemon agree.
            // TurnComplete.usage repeats the final call's count, so only
            // Usage events accumulate.
            remote.app.tool_state.total_usage =
                remote.app.tool_state.total_usage.saturating_add(tokens);
            remote.app.tool_state.total_output =
                remote.app.tool_state.total_output.saturating_add(output);
            remote.app.tool_state.total_cost += cost;
        }
        StreamEvent::TurnFailed { error } => {
            append_sink_line(&mut remote.app, SinkLine::Error(error));
        }
        StreamEvent::System(msg) => {
            append_sink_line(&mut remote.app, SinkLine::System(msg));
        }
        StreamEvent::Error(msg) => {
            append_sink_line(&mut remote.app, SinkLine::Error(msg));
        }
        StreamEvent::Plan {
            goal,
            steps,
            constraints,
            acceptance,
        } => {
            remote.app.plan = crate::core::types::Plan {
                goal,
                steps,
                constraints,
                acceptance,
            };
        }
    }
}

/// Find the local JSONL for a daemon session id when files are shared
/// (default co-located daemon). Matches exact id first, then id prefix,
/// then file-stem — `Session::resume` only handles index/path, so an id
/// lookup through it silently misses and left `/resume` with no file to
/// rebuild from (blank terminal).
fn find_local_session_file(sid: &str) -> Option<std::path::PathBuf> {
    let all = Session::list_all().unwrap_or_default();
    if let Some((p, _)) = all.iter().find(|(_, h)| h.id() == sid) {
        return Some(p.clone());
    }
    let q = sid.to_ascii_lowercase();
    if let Some((p, _)) = all.iter().find(|(_, h)| {
        h.id().to_ascii_lowercase().starts_with(&q) || q.starts_with(&h.id().to_ascii_lowercase())
    }) {
        return Some(p.clone());
    }
    all.into_iter()
        .find(|(p, _)| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem == sid || stem.starts_with(sid))
        })
        .map(|(p, _)| p)
}

/// Rebuild the transcript from the persisted JSONL messages (complete:
/// includes the user prompts the events journal never records). Returns
/// true when anything was rendered. Mirrors the local `/resume` path.
fn rebuild_remote_from_messages(remote: &mut RemoteApp, path: &std::path::Path) -> bool {
    let Ok(loaded) = crate::session::load_messages_from_session(path) else {
        return false;
    };
    if loaded.is_empty() {
        return false;
    }
    let system = remote
        .app
        .messages
        .first()
        .cloned()
        .unwrap_or(crate::core::types::ChatMessage::system(String::new()));
    remote.app.messages.clear();
    remote.app.messages.push(system);
    remote.app.messages.extend(loaded);
    super::rebuild_transcript(&mut remote.app);
    if let Some(p) = remote.app.session.path() {
        let plan = crate::session::load_plan(p);
        if !plan.is_empty() {
            remote.app.plan = plan;
        }
    } else {
        let plan = crate::session::load_plan(path);
        if !plan.is_empty() {
            remote.app.plan = plan;
        }
    }
    true
}

/// Replay the daemon's events journal into the transcript (best effort when
/// no local file is available, e.g. true remote). Skips parked approvals;
/// flushes the throttled assistant buffer so the replay is visible.
fn replay_remote_events(remote: &mut RemoteApp, session_id: &str) {
    let mut since = 0u64;
    let mut batches = 0;
    loop {
        match remote.client.events(session_id, since) {
            Ok(resp) => {
                if resp.events.is_empty() {
                    break;
                }
                for env in resp.events {
                    if !matches!(env.event, StreamEvent::ApprovalRequired { .. }) {
                        handle_stream_event(remote, env.event);
                    }
                }
                since = resp.next_seq;
                batches += 1;
                if batches > 10_000 {
                    break;
                }
            }
            Err(e) => {
                push_info(&mut remote.app, format!("replay failed: {e}"));
                break;
            }
        }
    }
    flush_assistant(&mut remote.app);
    close_thinking(&mut remote.app);
    remote.app.autoscroll = true;
    remote.app.scroll = 0;
}

fn finish_turn(remote: &mut RemoteApp, error: Option<String>) {
    if let Some(error) = error {
        append_sink_line(&mut remote.app, SinkLine::Error(error));
    }
    let app = &mut remote.app;
    // Drain any assistant deltas still in the throttle buffer so the final
    // text is in the transcript before the turn is torn down.
    flush_assistant(app);
    // If the turn was cancelled, restore queued steering/follow-ups to the
    // composer so the user can retry (mirrors old local `event.rs` logic).
    if app.cancel_requested {
        let mut restored = Vec::new();
        restored.append(&mut app.pending_steering);
        restored.append(&mut app.pending_followups);
        if !restored.is_empty() {
            app.input = crate::ui::input::InputField::from_text(&restored.join("\n"));
        }
    }
    app.busy = false;
    app.cancel_requested = false;
    // A turn can end right after thinking (cancel, failure before any text);
    // settle the indicator instead of leaving the dots animating forever.
    close_thinking(app);
    remote.cancel_flag.store(false, Ordering::SeqCst);
    if let Some(started) = app.turn_started.take() {
        let tokens = app
            .tool_state
            .last_usage
            .unwrap_or_else(|| crate::agent::compaction::estimate_tokens(&app.messages));
        app.last_activity = Some(format!(
            "worked for {:.1}s · {} tokens",
            started.elapsed().as_secs_f64(),
            super::format_tokens(tokens)
        ));
    }
    // Tools (bash/git/write/edit) may have switched branches or dirtied the
    // tree mid-turn; refresh the footer now rather than waiting for the next
    // background poll. Async so the UI thread never blocks on HTTP (the
    // daemon's 5s git cache keeps it cheap).
    refresh_git_async(remote.client.clone(), remote.worker_tx.clone());
}

/// How long to hold a suspicious char run while waiting for the rest of an
/// OSC color report before giving up and replaying it as real input.
/// Short poll for the next key in the burst, total window for a chunked
/// terminal reply (some ttys split `\x1b]11;rgb:…\x07` across writes).
const OSC_LOOKAHEAD: Duration = Duration::from_millis(35);
const OSC_TOTAL_TIMEOUT: Duration = Duration::from_millis(150);

/// TUI startup instant, so swallowed reports can be attributed: uptime near
/// zero means our own startup theme query (reply arrived late); a large
/// uptime means something else queried this tty mid-session.
static OSC_START: OnceLock<Instant> = OnceLock::new();

/// XDG cache directory (`$XDG_CACHE_HOME`, else `$HOME/.cache`); the macOS
/// layout matches the `dirs` crate. Replaces the `dirs` dep (one call site).
pub(crate) fn cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Caches"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
            })
    }
}

/// Best-effort diagnostic journal of OSC runs caught by `strip_osc_report`,
/// appended to `~/.cache/dex/osc.log`. The reply carries no sender identity,
/// so the log records when + how late + what; failures are ignored.
fn log_osc(kind: &str, body: &str) {
    use std::io::Write;
    let Some(dir) = cache_dir() else { return };
    let path = dir.join("dex/osc.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let up = OSC_START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let body: String = body.chars().take(80).collect();
    let _ = writeln!(f, "{ts} uptime={up:.1}s {kind} body={body}");
}

/// Swallow OSC 10/11 color reports that crossterm mis-parses as keystrokes.
/// Crossterm has no OSC parsing: `\x1b]11;rgb:0505/1818/2e2e` + BEL arrives
/// as `Alt+']'`, then one plain-char key per body byte, then `Ctrl+G` (BEL)
/// or `Alt+'\'` (ST). Such reports come from the startup theme query and
/// from anything else querying this tty (the terminal itself does), at any
/// time; unfiltered they type `10;rgb:f6f6/dcdc/acac…` into the composer.
/// Returns `None` when the run was swallowed; anything that doesn't fit the
/// report shape is replayed untouched, so real input is never dropped —
/// worst case it is delayed by one look-ahead window.
fn strip_osc_report(ev: Event, pending: &mut VecDeque<Event>) -> std::io::Result<Option<Event>> {
    let lead_in = |e: &Event| {
        matches!(
            e,
            Event::Key(k) if k.code == KeyCode::Char(']') && k.modifiers == KeyModifiers::ALT
        )
    };
    if !lead_in(&ev) {
        return Ok(Some(ev));
    }
    let terminator = |k: &crossterm::event::KeyEvent| {
        (k.code == KeyCode::Char('g') && k.modifiers == KeyModifiers::CONTROL) // BEL
            || (k.code == KeyCode::Char('\\') && k.modifiers == KeyModifiers::ALT)
        // ST
    };

    // Consume the run: plain chars accumulate into `body`, until a report
    // terminator, the next report's lead-in, any other key, or a gap.
    // A chunked tty can split `\x1b]11;rgb:…\x07` across writes with a short
    // gap; if the prefix so far could still become a valid report, keep
    // waiting up to the total window instead of replaying the fragment.
    let mut run: Vec<Event> = vec![ev];
    let mut body = String::new();
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= OSC_TOTAL_TIMEOUT {
            break;
        }
        let remaining = OSC_TOTAL_TIMEOUT - elapsed;
        let short = remaining.min(OSC_LOOKAHEAD);
        let next = match pending.pop_front() {
            Some(e) => e,
            None if event::poll(short)? => event::read()?,
            None => {
                if is_osc_prefix(&body) && !body.is_empty() {
                    continue;
                }
                break;
            }
        };
        let (char_of, ends) = match &next {
            Event::Key(k) if k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT => {
                match k.code {
                    KeyCode::Char(c) => (Some(c), false),
                    _ => (None, true),
                }
            }
            Event::Key(k) if terminator(k) => (None, true),
            e if lead_in(e) => (None, true),
            _ => (None, true),
        };
        if let Some(c) = char_of {
            body.push(c);
        }
        run.push(next);
        if ends {
            break;
        }
    }

    // A back-to-back next report begins with its own lead-in; reclassify it.
    let next_lead_in = if run.len() > 1 && lead_in(run.last().unwrap()) {
        run.pop()
    } else {
        None
    };

    if is_osc_report(&body) {
        log_osc("swallowed", &body);
        if let Some(lead) = next_lead_in {
            pending.push_front(lead);
        }
        return Ok(None);
    }
    // Not a report after all (or the burst was split beyond the look-ahead —
    // ponytail: 25ms; a tty that chunks reply writes slower than that would
    // leak the tail): replay everything in order. The lead-in itself is
    // returned for dispatch (Alt+']' inserts nothing) so a replayed run can
    // never re-enter this filter and loop.
    log_osc("replayed", &body);
    let lead_in_ev = run.remove(0);
    for e in run.into_iter().rev() {
        pending.push_front(e);
    }
    Ok(Some(lead_in_ev))
}

/// How the engine is reached: loopback daemons are "local", everything else
/// is reported by host. Used by the status bar instead of a startup banner.
pub(crate) fn connection_label(daemon_url: &str) -> String {
    let authority = daemon_url
        .split_once("://")
        .map_or(daemon_url, |(_, rest)| rest);
    // Authority = [userinfo@]host[:port]; stop at the first path/query char.
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    // Bracketed IPv6 literals: everything through `]` is the host, the rest
    // (if any) is the port. Otherwise a trailing `:digits` is a port.
    let host = if let Some(close) = authority.find(']') {
        &authority[..=close]
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => authority,
        }
    };
    if is_loopback(host) {
        format!("[L] {host}")
    } else {
        format!("[R] {host}")
    }
}

fn is_loopback(host: &str) -> bool {
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let octets: Vec<_> = bare.split('.').collect();
    octets.len() == 4 && octets[0] == "127" && octets[1..].iter().all(|o| o.parse::<u8>().is_ok())
}

/// Body grammar of an OSC 10/11 color report: `10;rgb:` / `11;rgb:` plus at
/// least three `/`-separated hex components (16-bit or truncated).
fn is_osc_report(body: &str) -> bool {
    let rest = body
        .strip_prefix("10;rgb:")
        .or_else(|| body.strip_prefix("11;rgb:"))
        .unwrap_or("");
    !rest.is_empty()
        && rest.split('/').count() >= 3
        && rest.chars().all(|c| c.is_ascii_hexdigit() || c == '/')
}

fn is_osc_prefix(body: &str) -> bool {
    if body.is_empty() {
        return true;
    }
    if "10;rgb:".starts_with(body) || "11;rgb:".starts_with(body) {
        return true;
    }
    if let Some(rest) = body
        .strip_prefix("10;rgb:")
        .or_else(|| body.strip_prefix("11;rgb:"))
    {
        return rest.chars().all(|c| c.is_ascii_hexdigit() || c == '/');
    }
    matches!(body, "1" | "10" | "11" | "10;" | "11;")
}

fn handle_key(remote: &mut RemoteApp, key: crossterm::event::KeyEvent) {
    let app = &mut remote.app;

    // Approval overlay takes precedence: the worker is blocked until a
    // decision arrives.
    if app.pending_approval.is_some() {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                // Deny the pending approval and cancel the turn; another
                // Ctrl+C once idle quits.
                resolve_approval(app, CoreApprovalDecision::Deny);
                request_cancel(remote);
            }
            KeyCode::Up | KeyCode::Left => {
                if let Some(approval) = app.pending_approval.as_mut() {
                    approval.selected = approval.selected.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
                if let Some(approval) = app.pending_approval.as_mut() {
                    approval.selected = (approval.selected + 1).min(2);
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                resolve_approval(app, CoreApprovalDecision::Once);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                resolve_approval(app, CoreApprovalDecision::Session);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_approval(app, CoreApprovalDecision::Deny);
            }
            KeyCode::Enter => {
                let decision =
                    app.pending_approval
                        .as_ref()
                        .map(|approval| match approval.selected {
                            0 => CoreApprovalDecision::Once,
                            1 => CoreApprovalDecision::Session,
                            _ => CoreApprovalDecision::Deny,
                        });
                if let Some(decision) = decision {
                    resolve_approval(app, decision);
                }
            }
            _ => {}
        }
        return;
    }

    // Idle double Ctrl+C guard: any non-Ctrl+C key cancels the pending quit.
    let is_ctrl_c =
        matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        app.last_ctrl_c = None;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.busy {
                if app.cancel_requested {
                    // Second Ctrl+C while a cancel is already in flight: the
                    // daemon is stuck, force-quit rather than stay trapped.
                    app.quit = true;
                } else {
                    request_cancel(remote);
                }
            } else {
                // Double Ctrl+C to exit when idle (avoid accidental quit).
                const DOUBLE_WINDOW: Duration = Duration::from_secs(2);
                let now = Instant::now();
                let should_quit = app
                    .last_ctrl_c
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_WINDOW);
                if should_quit {
                    app.quit = true;
                } else {
                    app.last_ctrl_c = Some(now);
                    push_info(app, "Press Ctrl+C again to exit".to_string());
                }
            }
        }
        KeyCode::Esc if app.busy => {
            request_cancel(remote);
        }
        KeyCode::Char('t') if key.modifiers == KeyModifiers::CONTROL => {
            app.show_thinking = !app.show_thinking;
            bump_thinking_stamps(app);
        }
        _ if !app.busy && !slash_suggestions(app).is_empty() => match key.code {
            KeyCode::Esc => {
                // Discard the drafted slash command and close the popup
                // without completing anything (busy+Esc still cancels).
                dismiss_slash(app);
            }
            KeyCode::Up => {
                app.slash_selected = app.slash_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let last = slash_suggestions(app).len().saturating_sub(1);
                app.slash_selected = (app.slash_selected + 1).min(last);
            }
            KeyCode::Tab => {
                if !expand_bare_command(app) {
                    complete_slash(app);
                }
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                // Bare picker command (`/model`, `/provider`, `/resume`):
                // first Enter expands to `"<cmd> "` and shows the popup
                // instead of submitting the bare form (which would only
                // print info into the transcript).
                if expand_bare_command(app) {
                    return;
                }
                // Command-name completion without an argument yet (`/mod` →
                // `/model `): complete but don't submit while the result is
                // still a bare picker command. Argument-less commands
                // (`/clear`) still submit immediately, and argument
                // completions (`/model foo`, `/resume 0`) complete + submit
                // the highlighted choice as before.
                let before = app.input.text();
                if !before.contains(' ') && !before.contains('\n') {
                    let before_bare = before.trim().to_string();
                    if complete_slash(app) {
                        let bare = app.input.text().trim().to_string();
                        if EXPAND_ON_ENTER.contains(&bare.as_str()) && before_bare != bare {
                            app.slash_selected = 0;
                            return;
                        }
                    }
                } else {
                    complete_slash(app);
                }
                let is_followup = key.modifiers.contains(KeyModifiers::ALT);
                submit_prompt(remote, is_followup);
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete => {
                // Typing narrows the popup list: feed the keystroke to the
                // composer and jump back to the top match so the highlight
                // never strands past the filtered results (e.g. `/` + `r`
                // lands on `/resume` instead of a stale arrow position).
                app.input.handle_key(key);
                app.slash_selected = 0;
            }
            _ => app.input.handle_key(key),
        },
        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
            let is_followup = key.modifiers.contains(KeyModifiers::ALT);
            submit_prompt(remote, is_followup);
        }
        KeyCode::PageUp => {
            scroll_transcript(app, -20);
        }
        KeyCode::PageDown => {
            scroll_transcript(app, 20);
        }
        KeyCode::Up => {
            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                scroll_transcript(app, -1);
            } else if app.history_index.is_some()
                || app.input.lines.len() <= 1
                || app.input.row == 0
            {
                app.history_up();
            } else {
                app.input.handle_key(key);
            }
        }
        KeyCode::Down => {
            if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
                scroll_transcript(app, 1);
            } else if app.history_index.is_some()
                || app.input.lines.len() <= 1
                || app.input.row + 1 >= app.input.lines.len()
            {
                app.history_down();
            } else {
                app.input.handle_key(key);
            }
        }
        _ => {
            app.input.handle_key(key);
        }
    }
}

fn request_cancel(remote: &mut RemoteApp) {
    let app = &mut remote.app;
    if !app.busy {
        return;
    }
    app.cancel_requested = true;
    remote.cancel_flag.store(true, Ordering::SeqCst);
    // If an approval is blocking the turn, deny it first so the agent thread
    // can unwind.
    if app.pending_approval.take().is_some() {
        let _ = remote.decision_tx.try_send(CoreApprovalDecision::Deny);
    }
    match remote.client.cancel(&remote.session_id) {
        Ok(()) => push_info(app, "cancelling...".to_string()),
        Err(e) => push_info(app, format!("cancel failed: {e}")),
    }
}

fn submit_prompt(remote: &mut RemoteApp, is_followup: bool) {
    let line = remote.app.input.text().trim().to_string();
    if line.is_empty() {
        return;
    }
    // While busy, queue steering (mid-turn) or follow-up (chained turn) to
    // the daemon instead of starting a new turn. The daemon's `steering_rx` /
    // `followup_rx` (consumed inside `process_turn` and the outer follow-up
    // loop) mirrors the old in-memory `event.rs::submit(is_followup)` path.
    if remote.app.busy {
        if is_followup {
            remote.app.pending_followups.push(line.clone());
        } else {
            remote.app.pending_steering.push(line.clone());
        }
        let history_line = line.clone();
        remote.app.history_push(history_line);
        remote.app.input.reset();
        let client = remote.client.clone();
        let sid = remote.session_id.clone();
        let result = if is_followup {
            client.followup(&sid, &line)
        } else {
            client.steer(&sid, &line)
        };
        if let Err(e) = result {
            // No active turn to queue into (409) or network error: restore to
            // pending badge error and keep input for retry.
            if is_followup {
                remote.app.pending_followups.retain(|c| c != &line);
            } else {
                remote.app.pending_steering.retain(|c| c != &line);
            }
            let msg = e.to_string();
            if msg.contains("409") || msg.contains("CONFLICT") {
                push_info(
                    &mut remote.app,
                    format!(
                        "no active turn to queue {} into (turn may have just finished); sent as new prompt on next Enter",
                        if is_followup { "follow-up" } else { "steer" }
                    ),
                );
                remote.app.input = crate::ui::input::InputField::from_text(&line);
            } else {
                push_info(
                    &mut remote.app,
                    format!(
                        "failed to queue {}: {e}",
                        if is_followup { "follow-up" } else { "steer" }
                    ),
                );
            }
        }
        // On success the daemon will emit `SteeringAccepted` / `FollowupAccepted`
        // which clears `pending_*` and renders the prompt. Keep the badge
        // visible until then.
        return;
    }

    if line.starts_with('/') && !line.contains('\n') {
        remote.app.history_push(line.clone());
        remote.app.input.reset();
        if handle_remote_slash(remote, &line) {
            remote.app.quit = true;
        }
        return;
    }

    remote.app.history_push(line.clone());
    remote.app.input.reset();

    // Render the user prompt with the shared transcript grid.
    render_user_prompt(&mut remote.app, &line);

    let app = &mut remote.app;
    app.busy = true;
    app.cancel_requested = false;
    app.last_activity = None;
    app.turn_started = Some(std::time::Instant::now());
    remote.cancel_flag.store(false, Ordering::SeqCst);

    // Spawn the worker as an async task: it drives the daemon's SSE stream
    // with `send().await` / `recv().await` (never `blocking_send` /
    // `blocking_recv`, which panic inside a runtime). Approval requests park
    // the task until the overlay resolves them via the decision channel.
    let client = remote.client.clone();
    let session_id = remote.session_id.clone();
    let mut options = remote.options.clone();
    if !remote.app.plan.is_empty() && options.plan.is_none() {
        options.plan = Some(remote.app.plan.to_json());
    }
    let prompt = line;
    let event_tx = remote.worker_tx.clone();
    let mut decision_rx = remote.take_decision_receiver();
    let cancel_flag = remote.cancel_flag.clone();

    crate::client::http::spawn_task(async move {
        let mut stream = match client.chat_stream(&session_id, &prompt, options).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = event_tx
                    .send(WorkerMessage::Finished(Some(e.to_string())))
                    .await;
                return;
            }
        };
        let mut saw_terminal = false;
        loop {
            match stream.next_event().await {
                Some(Ok(event)) => {
                    let request_id = match &event {
                        StreamEvent::ApprovalRequired { request_id, .. } => {
                            Some(request_id.clone())
                        }
                        StreamEvent::TurnComplete { .. } | StreamEvent::TurnFailed { .. } => {
                            saw_terminal = true;
                            None
                        }
                        _ => None,
                    };
                    // Backpressured: awaits UI drain instead of dropping when
                    // the transcript bursts faster than the 8fps redraw.
                    if event_tx.send(WorkerMessage::Stream(event)).await.is_err() {
                        break;
                    }
                    if let Some(request_id) = request_id {
                        // After a cancel request, deny automatically so the
                        // turn can unwind without user interaction.
                        let decision = next_worker_decision(&cancel_flag, &mut decision_rx).await;
                        if let Err(e) = client
                            .approve_async(&session_id, &request_id, decision)
                            .await
                        {
                            crate::llm::client::provider_log(
                                "approval_delivery_failed",
                                &e.to_string(),
                            );
                        }
                    }
                }
                Some(Err(e)) => {
                    let _ = event_tx.send(WorkerMessage::Finished(Some(e))).await;
                    return;
                }
                None => {
                    // The daemon always closes with TurnComplete/TurnFailed;
                    // a close without one is a transport failure, not success.
                    let error = premature_close_error(saw_terminal);
                    let _ = event_tx.send(WorkerMessage::Finished(error)).await;
                    return;
                }
            }
        }
    });
}

impl RemoteApp {
    /// Swap in a fresh decision channel per turn; the worker for this turn
    /// owns the old receiver.
    fn take_decision_receiver(&mut self) -> mpsc::Receiver<CoreApprovalDecision> {
        let (tx, rx) = mpsc::channel(16);
        self.decision_tx = tx;
        rx
    }
}

/// Slash commands for remote mode. Locally-answered commands are handled
/// here; everything else defers to the shared `slash` module. Returns true
/// when the app should quit.
fn handle_remote_slash(remote: &mut RemoteApp, line: &str) -> bool {
    match line {
        "/quit" => return true,
        "/clear" | "/new" => {
            if remote.app.busy {
                let what = if line == "/clear" {
                    "clear history"
                } else {
                    "start a new session"
                };
                push_info(
                    &mut remote.app,
                    format!("cannot {what} while a turn is running."),
                );
            } else {
                let label = if line == "/clear" {
                    "history cleared."
                } else {
                    "new session started."
                };
                let cwd = remote.app.cwd.clone();
                let name = Session::default_session_name(&cwd);
                match remote.client.create_session(&cwd, Some(&name)) {
                    Ok(session) => {
                        remote.session_id = session.session_id;
                        reset_session_state(&mut remote.app);
                        remote.options.plan = None;
                        let mut fresh = Session::in_memory(cwd);
                        fresh.set_name(name).ok();
                        remote.app.session = fresh;
                        push_info(&mut remote.app, label.to_string());
                        push_skills_listing(&mut remote.app);
                    }
                    Err(e) => {
                        push_info(&mut remote.app, format!("could not start new session: {e}"))
                    }
                }
            }
        }
        "/session" => {
            let id = remote.session_id.clone();
            push_info(&mut remote.app, format!("session: {id} (on daemon)"));
        }
        "/undo" => {
            let sid = remote.session_id.clone();
            match remote.client.undo(&sid) {
                Ok(true) => push_info(&mut remote.app, "last change undone.".to_string()),
                Ok(false) | Err(_) => push_info(
                    &mut remote.app,
                    "nothing to undo (or undo refused).".to_string(),
                ),
            }
        }
        _ if line.starts_with("/mcp ") => {
            // Same grammar as local `/mcp`: only `help` is valid; anything
            // else is usage (login itself runs in the CLI on the daemon host).
            match line["/mcp ".len()..].trim() {
                "help" => {
                    push_info(
                        &mut remote.app,
                        "/mcp shows server status including auth.".to_string(),
                    );
                    push_info(
                        &mut remote.app,
                        "MCP OAuth runs in the CLI: `dex mcp login <server>` (on the daemon host when remote).".to_string(),
                    );
                }
                _ => push_info(&mut remote.app, "usage: /mcp [help]".to_string()),
            }
        }
        "/mcp" => {
            // Explicit arm (not the `handle_slash` fallthrough below): the
            // daemon owns the MCP connections, so status must come from
            // `/api/mcp` — the client process's own manager was never
            // bootstrapped and would render an empty list.
            match remote.client.mcp_status() {
                Ok(body) => {
                    let statuses: Vec<crate::mcp::ServerStatus> = body["servers"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|v| crate::mcp::ServerStatus {
                                    name: v["name"].as_str().unwrap_or("?").to_string(),
                                    state: v["state"].as_str().unwrap_or("down").to_string(),
                                    tools: v["tools"].as_u64().unwrap_or(0) as usize,
                                    error: v["error"].as_str().map(|s| s.to_string()),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // No per-tool detail over the wire yet: headers + errors.
                    let truncated = body["truncated"].as_u64().unwrap_or(0) as usize;
                    for line in crate::core::format::render_mcp_panel(&statuses, &[], truncated) {
                        push_info(&mut remote.app, line);
                    }
                    // Auth rides the same body (`auth`, null for stdio) so a
                    // remote TUI never needs the daemon host's token files.
                    for line in crate::client::http::mcp_auth_lines(&body) {
                        push_info(&mut remote.app, line);
                    }
                }
                Err(e) => push_info(&mut remote.app, format!("could not fetch MCP status: {e}")),
            }
        }
        "/help" => {
            push_info(
                &mut remote.app,
                "commands: /quit /clear /new /session /undo /mcp /waive <reason> /permissions /model [<m>] /skill:<name> /goal <text> /plan [add|done|clear] /constraint [add|clear] /accept [add|done|clear]"
                    .to_string(),
            );
            push_info(
                &mut remote.app,
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/wheel scroll · Ctrl+T thinking"
                    .to_string(),
            );
            push_info(
                &mut remote.app,
                "mouse: drag, double/triple-click to select and copy · wheel scrolls".to_string(),
            );
            push_info(
                &mut remote.app,
                "while working: Enter queues steer · Alt+Enter queues follow-up · Esc/Ctrl+C cancels and restores queued input"
                    .to_string(),
            );
        }
        _ if line.starts_with("/skill:") => {
            let name = line["/skill:".len()..].trim().to_string();
            if name.is_empty() {
                push_info(&mut remote.app, "usage: /skill:<name>".to_string());
            } else {
                let sid = remote.session_id.clone();
                let dirs = remote.options.skill_dirs.clone();
                let res = remote.client.load_skill(&sid, &name, &dirs);
                match res {
                    Ok(resp) => {
                        push_info(&mut remote.app, format!("loaded skill: {}", resp.name));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("404") {
                            push_info(&mut remote.app, format!("skill not found: {name}"));
                            let needs_refresh = remote.app.skills.is_empty();
                            if needs_refresh {
                                if let Ok(fresh) = remote.client.list_skills() {
                                    remote.app.skills = fresh
                                        .into_iter()
                                        .map(|info| crate::core::types::Skill {
                                            name: info.name,
                                            description: info.description,
                                            path: std::path::PathBuf::from(""),
                                        })
                                        .collect();
                                }
                            }
                            let names: Vec<String> =
                                remote.app.skills.iter().map(|s| s.name.clone()).collect();
                            if !names.is_empty() {
                                push_info(&mut remote.app, "available skills:".to_string());
                                for n in names {
                                    push_info(&mut remote.app, format!("  - {n}"));
                                }
                            }
                        } else {
                            push_info(
                                &mut remote.app,
                                format!("could not load skill '{name}': {e}"),
                            );
                        }
                    }
                }
            }
        }
        _ if line.starts_with("/waive ") => {
            let reason = line["/waive ".len()..].trim().to_string();
            if reason.is_empty() {
                push_info(&mut remote.app, "usage: /waive <reason>".to_string());
            } else {
                let sid = remote.session_id.clone();
                match remote.client.waive(&sid, &reason) {
                    Ok(()) => push_info(
                        &mut remote.app,
                        "verification waived (recorded for this session).".to_string(),
                    ),
                    Err(e) => push_info(
                        &mut remote.app,
                        format!("could not waive verification: {e}"),
                    ),
                }
            }
        }
        _ if line.starts_with("/name ") => {
            let name = line["/name ".len()..].trim().to_string();
            if name.is_empty() {
                push_info(&mut remote.app, "usage: /name <name>".to_string());
            } else {
                let sid = remote.session_id.clone();
                match remote.client.rename_session(&sid, &name) {
                    Ok(()) => push_info(&mut remote.app, format!("session name: {name}")),
                    Err(e) => push_info(&mut remote.app, format!("could not rename: {e}")),
                }
            }
        }
        l if l.starts_with("/provider ") => {
            push_info(
                &mut remote.app,
                "provider is configured on the daemon host; not switchable from a remote client"
                    .to_string(),
            );
        }
        "/resume" => {
            // Prefer daemon listing (works over network); fall back to local files for offline.
            let daemon_sessions = remote.client.list_sessions().ok();
            if let Some(mut sessions) = daemon_sessions {
                sessions.retain(|s| {
                    s.cwd == remote.app.cwd
                        && s.session_id != remote.session_id
                        && s.message_count > 0
                });
                if sessions.is_empty() {
                    push_info(&mut remote.app, "no sessions found.".to_string());
                } else {
                    push_info(&mut remote.app, "sessions:".to_string());
                    for (i, s) in sessions.iter().enumerate() {
                        let name = s.name.as_deref().unwrap_or("(unnamed)");
                        push_info(
                            &mut remote.app,
                            format!("  {}: {} ({})", i, name, s.session_id),
                        );
                    }
                    push_info(
                        &mut remote.app,
                        "use /resume <index|id> to resume (reattaches on daemon)".to_string(),
                    );
                }
            } else {
                let sessions = Session::list(&remote.app.cwd).unwrap_or_default();
                let filtered: Vec<_> = sessions
                    .into_iter()
                    .filter(|(path, header)| {
                        if header.id() == remote.app.session.id() {
                            return false;
                        }
                        matches!(
                            crate::session::load_messages_from_session(path),
                            Ok(msgs) if !msgs.is_empty()
                        )
                    })
                    .collect();
                if filtered.is_empty() {
                    push_info(&mut remote.app, "no sessions found.".to_string());
                } else {
                    push_info(&mut remote.app, "sessions:".to_string());
                    for (i, (path, header)) in filtered.iter().enumerate() {
                        let name = header.name().unwrap_or("(unnamed)");
                        push_info(
                            &mut remote.app,
                            format!("  {}: {} ({})", i, name, path.display()),
                        );
                    }
                    push_info(
                        &mut remote.app,
                        "use /resume <index|id> to resume (reattaches on daemon)".to_string(),
                    );
                }
            }
        }
        _ if line.starts_with("/resume ") => {
            if remote.app.busy {
                push_info(
                    &mut remote.app,
                    "cannot resume a session while a turn is running.".to_string(),
                );
                return false;
            }
            let selector = line["/resume ".len()..].trim().to_string();
            // Resolve selector to a daemon session_id + server-side path: index
            // or id prefix. Filter the same way as the listing so indices line
            // up. The daemon path doubles as the local file when co-located.
            let resolved: Option<(String, Option<String>)> = remote
                .client
                .list_sessions()
                .ok()
                .and_then(|mut sessions| {
                    sessions.retain(|s| {
                        s.cwd == remote.app.cwd
                            && s.session_id != remote.session_id
                            && s.message_count > 0
                    });
                    if let Ok(idx) = selector.parse::<usize>() {
                        return sessions
                            .get(idx)
                            .map(|s| (s.session_id.clone(), Some(s.path.clone())));
                    }
                    // prefix or exact id/name match
                    let q = selector.to_ascii_lowercase();
                    sessions
                        .iter()
                        .find(|s| {
                            s.session_id.to_ascii_lowercase().starts_with(&q)
                                || s.name
                                    .as_deref()
                                    .unwrap_or("")
                                    .to_ascii_lowercase()
                                    .contains(&q)
                        })
                        .map(|s| (s.session_id.clone(), Some(s.path.clone())))
                })
                .or_else(|| {
                    let sessions = Session::list(&remote.app.cwd).unwrap_or_default();
                    let filtered: Vec<_> = sessions
                        .into_iter()
                        .filter(|(path, header)| {
                            if header.id() == remote.app.session.id() {
                                return false;
                            }
                            matches!(
                                crate::session::load_messages_from_session(path),
                                Ok(msgs) if !msgs.is_empty()
                            )
                        })
                        .collect();
                    if let Ok(idx) = selector.parse::<usize>() {
                        return filtered.get(idx).and_then(|(p, _)| {
                            // Resolve id from file header for local fallback.
                            crate::session::Session::from_path(p)
                                .ok()
                                .map(|s| (s.id().to_string(), Some(p.display().to_string())))
                        });
                    }
                    let q = selector.to_ascii_lowercase();
                    filtered
                        .iter()
                        .find(|(p, h)| {
                            h.id().to_ascii_lowercase().starts_with(&q)
                                || h.name().unwrap_or("").to_ascii_lowercase().contains(&q)
                                || p.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("")
                                    .to_ascii_lowercase()
                                    .starts_with(&q)
                        })
                        .and_then(|(p, _)| crate::session::Session::from_path(p).ok())
                        .map(|s| {
                            let path = s.path().map(|p| p.display().to_string());
                            (s.id().to_string(), path)
                        })
                });
            let Some((sid, daemon_path)) = resolved else {
                push_info(
                    &mut remote.app,
                    format!("could not resume session: {selector} not found"),
                );
                return false;
            };
            // Local JSONL when files are shared (default co-located daemon):
            // daemon path first, then an id lookup (Session::resume only
            // handles index/path, so an id through it silently misses).
            let local_path: Option<std::path::PathBuf> = daemon_path
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_file())
                .or_else(|| find_local_session_file(&sid));
            // Reattach before touching the transcript: a failed reattach must
            // leave the current view intact instead of blanking it.
            match remote.client.reattach(&sid) {
                Ok(resp) => {
                    remote.session_id = resp.session_id.clone();
                    // Full per-session reset (transcript, usage, plan, scroll,
                    // pending steering/approvals) so the old conversation
                    // doesn't leak into the resumed one.
                    reset_session_state(&mut remote.app);
                    remote.options.plan = None;
                    if let Some(p) = local_path.as_deref() {
                        if let Ok(s) = Session::from_path(p) {
                            remote.app.session = s;
                        }
                    }
                    // Prefer JSONL messages (complete, includes user prompts
                    // the events journal never records); replay events only
                    // when no local file is available (true remote).
                    let mut rebuilt = false;
                    if let Some(p) = local_path.as_deref() {
                        rebuilt = rebuild_remote_from_messages(remote, p);
                    }
                    if !rebuilt {
                        let sid = remote.session_id.clone();
                        replay_remote_events(remote, &sid);
                        if remote.app.transcript.is_empty()
                            && remote.app.assistant_pending.is_empty()
                        {
                            push_info(
                                &mut remote.app,
                                "resumed session has no replayable history.".to_string(),
                            );
                        }
                    }
                    push_info(
                        &mut remote.app,
                        format!("resumed session: {}", remote.session_id),
                    );
                }
                Err(e) => push_info(&mut remote.app, format!("could not reattach session: {e}")),
            }
        }
        _ => {
            let had_model = remote.app.config.model.clone();
            let had_permission = remote.app.config.permission;
            let had_plan = remote.app.plan.clone();
            // Raw `/model <selection>` argument: the daemon routes endpoint
            // prefixes (`go/…`) against its own endpoint table, so it must
            // see the un-stripped selection — the client's `config.model` is
            // already resolved to the bare id.
            let raw_model = line.strip_prefix("/model ").map(|s| s.trim().to_string());
            let quit = handle_slash(&mut remote.app, line);
            if remote.app.config.model != had_model {
                remote.options.model = Some(match raw_model {
                    Some(raw) if !raw.is_empty() => raw,
                    _ => remote.app.config.model.clone(),
                });
            }
            if remote.app.config.permission != had_permission {
                remote.options.permission = Some(match remote.app.config.permission {
                    PermissionMode::ReadOnly => "read-only".to_string(),
                    PermissionMode::AskWrites => "ask-writes".to_string(),
                    PermissionMode::AskShell => "ask-shell".to_string(),
                    PermissionMode::Trusted => "trusted".to_string(),
                });
            }
            if remote.app.plan != had_plan {
                if remote.app.plan.is_empty() {
                    remote.options.plan = Some(String::new());
                } else {
                    remote.options.plan = Some(remote.app.plan.to_json());
                }
            }
            return quit;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_labels_classify_host() {
        assert_eq!(connection_label("http://127.0.0.1:4113"), "[L] 127.0.0.1");
        assert_eq!(connection_label("http://localhost:4113"), "[L] localhost");
        assert_eq!(connection_label("http://[::1]:4113"), "[L] [::1]");
        assert_eq!(
            connection_label("daemon.internal:4113"),
            "[R] daemon.internal"
        );
        assert_eq!(
            connection_label("https://agent.example.com/api"),
            "[R] agent.example.com"
        );
    }

    #[test]
    fn recognizes_leaked_color_reports() {
        // The exact bodies observed typing themselves into the composer.
        assert!(is_osc_report("10;rgb:f6f6/dcdc/acac"));
        assert!(is_osc_report("11;rgb:0505/1818/2e2e"));
        assert!(is_osc_report("10;rgb:05/18/2e"));
        // Not reports: replayed as real input.
        assert!(!is_osc_report("hello"));
        assert!(!is_osc_report(""));
        assert!(!is_osc_report("10;rgb:"));
        assert!(!is_osc_report("12;rgb:0505/1818/2e2e"));
        assert!(!is_osc_report("11;rgb:zzzz/1818/2e2e"));
        assert!(!is_osc_report("10;rgb:0505/1818"));
    }

    fn skill(name: &str) -> crate::core::types::Skill {
        crate::core::types::Skill {
            name: name.to_string(),
            description: String::new(),
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn skills_listing_is_one_comma_separated_line() {
        let line = skills_listing_line(&[skill("a"), skill("b"), skill("c")])
            .expect("non-empty skills produce a line");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "skills loaded (3): a, b, c · /skill:<name> loads one");
        // Skill names use the terminal's theme foreground (Reset when the
        // theme is unknown), never a fixed ANSI slot the theme may remap.
        let fg = line.spans[1].style.fg.expect("names span carries a fg");
        assert!(
            matches!(fg, Color::Reset | Color::Rgb(..)),
            "names must follow the theme, got {fg:?}"
        );
        assert!(skills_listing_line(&[]).is_none());
    }

    #[test]
    fn launch_time_line_shows_ready_duration() {
        let line = launch_time_line(1.23);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "ready in 1.2s");
    }

    fn row_width(line: &ratatui::text::Line<'_>) -> usize {
        use unicode_width::UnicodeWidthStr;
        line.spans.iter().map(|s| s.content.width()).sum()
    }

    #[test]
    fn skills_listing_wraps_within_terminal_width() {
        let skills: Vec<_> = (0..8)
            .map(|i| skill(&format!("very-long-skill-name-{i:02}")))
            .collect();
        let line = skills_listing_line(&skills).expect("non-empty skills produce a line");
        let width = 40u16;
        let rows = crate::ui::render::wrap_line_display(&line, width);
        assert!(rows.len() > 1, "long listing must wrap into multiple rows");
        for row in &rows {
            assert!(
                row_width(row) <= width as usize,
                "row width {} exceeds {width}",
                row_width(row)
            );
        }
        // Wrapping splits at grapheme level; only the whitespace at each
        // break point is consumed (standard word-wrap), so the rows rejoin
        // to the original line modulo whitespace — nothing dropped or
        // duplicated.
        let strip_ws = |text: &str| {
            text.chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>()
        };
        let joined: String = rows
            .iter()
            .flat_map(|r| r.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        let original: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(strip_ws(&joined), strip_ws(&original));
        // Theme fg survives onto continuation rows.
        let fg = line.spans[1].style.fg.expect("names span carries a fg");
        assert!(
            rows.iter()
                .skip(1)
                .flat_map(|r| r.spans.iter())
                .any(|s| s.style.fg == Some(fg) && s.content != " "),
            "continuation rows must keep the names' theme fg"
        );
    }

    #[test]
    fn skills_listing_hard_breaks_overlong_single_name() {
        // One unbroken word (no whitespace) wider than the terminal must be
        // hard-split rather than overflow.
        let line = skills_listing_line(&[skill(&"x".repeat(120))]).expect("one skill");
        let rows = crate::ui::render::wrap_line_display(&line, 40);
        assert!(rows.len() > 1, "overlong single name must hard-break");
        for row in &rows {
            assert!(row_width(row) <= 40, "row overflow: {}", row_width(row));
        }
    }

    #[test]
    fn local_session_file_resolves_by_id_when_files_are_shared() {
        // `/resume` resolves the daemon sid (an id, not an index/path), so the
        // local lookup must handle ids — `Session::resume` only handles
        // index/path and silently missed, leaving a blank terminal.
        let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-resume-{}", std::process::id()));
        let _env =
            crate::session::EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        std::env::set_var("XDG_DATA_HOME", &dir);
        let mut s = crate::session::Session::new("/tmp/dex-resume-cwd".into(), None).unwrap();
        s.append_message(crate::core::types::ChatMessage::user("hi".to_string()))
            .unwrap();
        let id = s.id().to_string();
        let path = s.path().unwrap().to_path_buf();
        drop(s);
        assert_eq!(find_local_session_file(&id), Some(path.clone()));
        assert_eq!(
            find_local_session_file(&id[..8.min(id.len())]),
            Some(path.clone())
        );
        assert!(find_local_session_file("no-such-session").is_none());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("events.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_poller_spawns_without_blocking_ui() {
        // TDD Phase 5: background task polls off UI thread (2s interval, daemon 5s cache);
        // spawn returns immediately (no per-frame `git` or HTTP on UI thread).
        let client = DaemonClient::new("http://127.0.0.1:9").unwrap();
        let (wtx, _wrx) = tokio::sync::mpsc::channel::<WorkerMessage>(8);
        let start = std::time::Instant::now();
        spawn_git_poller(client, wtx);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "poller spawn must not block UI thread"
        );
        assert_eq!(GIT_REFRESH_INTERVAL, std::time::Duration::from_secs(2));
    }

    #[test]
    fn approval_decision_mapping_covers_all_variants() {
        assert!(matches!(
            map_approval_decision(CoreApprovalDecision::Once),
            ProtocolApprovalDecision::AllowOnce
        ));
        assert!(matches!(
            map_approval_decision(CoreApprovalDecision::Session),
            ProtocolApprovalDecision::AllowSession
        ));
        assert!(matches!(
            map_approval_decision(CoreApprovalDecision::Deny),
            ProtocolApprovalDecision::Deny
        ));
    }

    #[test]
    fn premature_close_reports_transport_failure() {
        assert_eq!(
            premature_close_error(false),
            Some("connection closed before turn completed".to_string())
        );
        assert_eq!(premature_close_error(true), None);
    }

    #[tokio::test]
    async fn worker_decision_auto_denies_after_cancel_without_waiting() {
        // Regression: approvals arriving after a cancel request must deny
        // immediately instead of parking the turn on the overlay. The channel
        // stays empty here — if the worker waited, the timeout fires.
        let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<CoreApprovalDecision>(16);
        let decision = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_worker_decision(&cancel_flag, &mut rx),
        )
        .await
        .expect("cancel path must not wait on the overlay");
        assert!(matches!(decision, ProtocolApprovalDecision::Deny));
    }

    #[tokio::test]
    async fn worker_decision_forwards_overlay_and_defaults_closed_to_deny() {
        let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CoreApprovalDecision>(16);
        tx.send(CoreApprovalDecision::Session).await.unwrap();
        let decision = next_worker_decision(&cancel_flag, &mut rx).await;
        assert!(matches!(decision, ProtocolApprovalDecision::AllowSession));
        // Turn went away (sender dropped): deny rather than hang.
        drop(tx);
        let decision = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_worker_decision(&cancel_flag, &mut rx),
        )
        .await
        .expect("closed channel must resolve, not hang");
        assert!(matches!(decision, ProtocolApprovalDecision::Deny));
    }
}
