use crate::ui::fg;
use std::io::{self, IsTerminal};

use super::super::push_banner;
use super::super::push_info;
use super::super::view;
use super::super::App;
use super::super::EnableMouseScroll;
use super::super::TerminalCleanup;
use super::input::find_local_session_file;
use super::input::rebuild_remote_from_messages;
use super::input::replay_remote_events;
use super::osc::OSC_START;
use super::pollers::spawn_events_poller;
use super::pollers::spawn_git_poller;
use super::resume::connection_label;
use super::resume::same_workspace;
use super::state::RemoteApp;
use super::state::WorkerMessage;
use super::state::LAUNCH_START;
use crate::cli::Args;
use crate::client::http::DaemonClient;
use crate::protocol::AgentMode;
use crate::protocol::ApiProtocol;
use crate::protocol::DaemonInfo;
use crate::protocol::PermissionMode;
use crate::protocol::Provider;
use crate::session::Session;
use crossterm::event::DisableMouseCapture;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::PushKeyboardEnhancementFlags;
use crossterm::execute;
use crossterm::terminal::enable_raw_mode;
use crossterm::terminal::EnterAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::Terminal;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

/// Build a display-only config from the daemon's reported runtime info. The
/// client never talks to the model provider itself; this only feeds the
/// status footer and slash-command suggestions.
fn display_config(info: &DaemonInfo) -> crate::llm::config::LlmConfig {
    // An empty daemon provider (no config resolved yet) stays an empty
    // generic name here — display-only, never resolved against a catalog.
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
        permission: PermissionMode::parse(&info.permission).unwrap_or(PermissionMode::Ask),
        verify_command: None,
        extra_headers: Default::default(),
        global_headers: Default::default(),
        provider_entries: Default::default(),
        provider_headers: Default::default(),
        api_pinned: false,
        // Display-only copy never talks to a provider (timeouts inert).
        connect_timeout_secs: 10,
        request_timeout_secs: 300,
    }
}

/// The skills line for the session-start header, or the "no skills" note.
/// Muted like the DEX banner: session-start chrome, not a call to action.
pub(crate) fn skills_header_line(skills: &[crate::protocol::Skill]) -> Line<'static> {
    let muted = fg(crate::render::theme::muted_fg());
    skills_listing_line(skills).unwrap_or_else(|| {
        Line::from(Span::styled(
            "no skills loaded (add .dex/skills/<name>/SKILL.md or ~/.config/dex/skills)"
                .to_string(),
            muted,
        ))
    })
}

/// The session-start skills line: names comma-separated on a single row, all
/// muted like the DEX banner above it. `None` when no skills are loaded.
pub(crate) fn skills_listing_line(skills: &[crate::protocol::Skill]) -> Option<Line<'static>> {
    let (first, rest) = skills.split_first()?;
    let mut names = first.name.clone();
    for skill in rest {
        names.push_str(", ");
        names.push_str(&skill.name);
    }
    let muted = fg(crate::render::theme::muted_fg());
    Some(Line::from(vec![
        Span::styled(format!("skills loaded ({}): ", skills.len()), muted),
        Span::styled(names, muted),
        Span::styled(" · /skill:<name> loads one", muted),
    ]))
}

/// The session-start launch-time line, shown below the skills listing so
/// users can see how fast the TUI was ready to use. Muted so it stays
/// quiet next to the skills line.
pub(crate) fn launch_time_line(elapsed_secs: f64) -> Line<'static> {
    Line::from(vec![Span::styled(
        format!(
            "ready in {}",
            crate::render::format::format_duration(elapsed_secs)
        ),
        fg(crate::render::theme::muted_fg()),
    )])
}

/// Whatever the render/event loop needs after boot: live app state plus the
/// initialized terminal (raw mode entered, alternate screen on) and its restore
/// guard.
pub(crate) struct Boot {
    pub(crate) remote: RemoteApp,
    pub(crate) terminal: Terminal<CrosstermBackend<io::Stdout>>,
    pub(crate) cleanup: TerminalCleanup,
}

/// Boot half of the remote REPL: connect, fan out the config/session/skills
/// fetches, build the app, replay a reattach, and prime the terminal.
pub(crate) fn bootstrap(
    args: &Args,
    daemon_url: &str,
    daemon_is_local: bool,
) -> std::io::Result<Boot> {
    if !std::io::stdout().is_terminal() {
        return Err(std::io::Error::other(
            "interactive UI requires a terminal (TTY); use `dex connect <url> \"prompt\"` for one-shot",
        ));
    }
    // The TUI owns the terminal from here on: move runtime logs to the file
    // sink so DEX_LOG output can't garble the alt screen. The notice (if any)
    // prints while the terminal is still the normal screen.
    if let Some(notice) = crate::runtime::logging::redirect_to_file() {
        eprintln!("{notice}");
    }
    let launch_start = *LAUNCH_START.get_or_init(Instant::now);
    OSC_START.get_or_init(Instant::now);

    let client = DaemonClient::new(daemon_url)
        .map_err(|e| std::io::Error::other(format!("failed to connect to daemon: {e}")))?;
    // A freshly-spawned local daemon already passed readiness inside
    // `start_daemon_background`; polling again is a wasted RTT. A remote
    // daemon (`dex connect`) may still be booting, so keep the wait there.
    if !daemon_is_local {
        client
            .wait_until_ready(Duration::from_secs(10))
            .map_err(|e| std::io::Error::other(format!("daemon not ready: {e}")))?;
    }

    // Boot fan-out: `get_config` (config + git), the session op (one small
    // write), and `list_skills` (daemon-side dir scan) are independent —
    // except a *remote* default session name, which is minted from the daemon
    // workspace in `info.cwd`. With `--name`/`--reattach` — or a local
    // default, where the spawned daemon inherits our cwd — nothing is needed
    // from `info`, so all three fly together; otherwise `get_config` still
    // overlaps the skills scan. (`create_session` carries a cwd the server
    // ignores in favor of its own, so a local placeholder is fine there.)
    // Detach-on-error preserved: the spawned tasks are awaited only on the
    // success path; an early return drops their handles (detaches).
    let skills_client = client.clone();
    let skills_handle = crate::runtime::http::spawn_task(async move {
        skills_client.list_skills_async().await.unwrap_or_default()
    });
    // §2: warm the one-time OSC 11 palette query alongside the session RTT
    // instead of serially before first paint. The `block_on` before the
    // skills listing (the first consumer of colors) is instant when the
    // probe finished in flight.
    let palette_handle = crate::runtime::http::spawn_task(async move {
        crate::render::theme::detect_background();
    });
    // The daemon owns the model/provider/permission and the workspace; mirror
    // its state so the UI shows what turns will actually use.
    let config_client = client.clone();
    let info_handle = crate::runtime::http::spawn_task(async move {
        // `Box<dyn Error>` is not `Send`; stringify across the spawn boundary.
        config_client
            .get_config_async()
            .await
            .map_err(|e| e.to_string())
    });
    // Sessions default to `<workspace>-<7 chars>` (k8s-style); an explicit
    // `--name` wins. Generated client-side so the local placeholder shows the
    // same name the daemon persists.
    let explicit_name = args.session_name.clone().filter(|n| !n.is_empty());
    let local_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (session_id, is_reattach, info, session_name) = if let Some(reattach) = &args.reattach {
        // P10: attach to an existing persisted session on the daemon and get
        // the replay cursor, instead of creating a fresh one.
        let resp = client
            .reattach(reattach)
            .map_err(|e| std::io::Error::other(format!("failed to reattach session: {e}")))?;
        let info = crate::runtime::http::block_on(info_handle)
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;
        (resp.session_id, true, info, None)
    } else if let Some(name) = explicit_name.as_deref() {
        let resp = client
            .create_session(&local_cwd, Some(name))
            .map_err(|e| std::io::Error::other(format!("failed to create session: {e}")))?;
        let info = crate::runtime::http::block_on(info_handle)
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;
        (resp.session_id, false, info, Some(name.to_string()))
    } else if daemon_is_local && !local_cwd.is_empty() {
        // Local default: the spawned daemon inherits our cwd, so mint the
        // name locally and overlap all three (same as the explicit-name path
        // above); the server ignores the carried cwd anyway.
        let session_name = Session::default_session_name(&local_cwd);
        let resp = client
            .create_session(&local_cwd, Some(&session_name))
            .map_err(|e| std::io::Error::other(format!("failed to create session: {e}")))?;
        let info = crate::runtime::http::block_on(info_handle)
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;
        (resp.session_id, false, info, Some(session_name))
    } else {
        // Remote default: needs the daemon workspace first; the skills scan
        // above still overlaps this fetch.
        let info = crate::runtime::http::block_on(info_handle)
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?
            .map_err(|e| std::io::Error::other(format!("failed to read daemon config: {e}")))?;
        let session_name = Session::default_session_name(&info.cwd);
        let resp = client
            .create_session(&info.cwd, Some(&session_name))
            .map_err(|e| std::io::Error::other(format!("failed to create session: {e}")))?;
        (resp.session_id, false, info, Some(session_name))
    };
    // §3: the skills future flies with the boot fan-out (spawned above) but
    // is collected after replay below — neither App construction nor replay
    // needs skills, only the session-start listing does.

    // Per-request overrides so client flags keep working in remote mode.
    let mut options = crate::chat_options_from_args(args);
    // Seed the mode from an explicit client `--permission` (a stricter
    // per-run choice), else from the daemon's reported ceiling. Clamp to
    // the ceiling, which a client may only go stricter than.
    let explicit = args.permission.or_else(|| {
        std::env::var("DEX_PERMISSION")
            .ok()
            .and_then(|v| PermissionMode::parse(&v).ok())
    });
    let ceiling = PermissionMode::parse(&info.permission).unwrap_or(PermissionMode::Ask);
    let mode = seed_mode(explicit, ceiling);
    options.mode = Some(mode.as_str().to_string());
    // `permission` still rides along (derived) so an older daemon that
    // ignores `mode` restricts identically.
    options.permission = Some(mode.permission().as_str().to_string());

    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>(256);
    let cancel_flag = Arc::new(AtomicBool::new(false));

    let app = App {
        remote_mode: true,
        transcript: Vec::new(),
        input: crate::ui::input::InputField::new(),
        config: display_config(&info),
        messages: Vec::new(),
        tool_state: crate::agent::state::ToolState::default(),
        session: Session::in_memory(info.cwd.clone()),
        plan: crate::protocol::Plan::default(),
        skills: Vec::new(),
        turn_start: 0,
        cwd: info.cwd.clone(),
        git_branch: info.git_branch.clone(),
        git_dirty: info.git_dirty,
        steering_rx: None,
        followup_rx: None,
        pending_steering: Vec::new(),
        pending_followups: Vec::new(),
        cancel_requested: false,
        cancel_presses: 0,
        approval_rx: None,
        pending_approvals: Vec::new(),
        agents: Vec::new(),
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
        daemon_url: Some(daemon_url.to_string()),
        assistant_open: false,
        show_thinking: false,
        thinking_open: false,
        assistant_pending: String::new(),
        assistant_gap: crate::render::theme::markdown::GapState::new(),
        stream_last_flush: Instant::now(),
        wrapped_cache: Vec::new(),
        wrapped_width: 0,
        display_cache: Vec::new(),
        transcript_area: None,
        selection: None,
        notice: None,
        status_tokens_cache: std::cell::Cell::new((0, 0, 0)),
        slash_cache: std::cell::RefCell::new(None),
    };

    // Shared poller gates (§15 V1b): children live → poll the journal;
    // turn streaming → pause (the turn's own SSE carries those rows).
    let live_children = Arc::new(AtomicBool::new(false));
    let busy_poll = Arc::new(AtomicBool::new(false));
    // Shared journal cursor: the turn worker publishes (fetch_max) the seq
    // its SSE stream has delivered; the idle poller resumes from it.
    let events_cursor = Arc::new(AtomicU64::new(0));
    let mut remote = RemoteApp {
        app,
        client: client.clone(),
        session_id: session_id.clone(),
        options,
        mode,
        ceiling,
        worker_tx: worker_tx.clone(),
        worker_rx,
        cancel_flag,
        last_click: None,
        shell_running: false,
        shell_cancel_requested: false,
        live_children: live_children.clone(),
        busy_poll: busy_poll.clone(),
        events_cursor: events_cursor.clone(),
    };
    // Background git poll off the UI thread (Phase 5): task polls every 2s,
    // pushes into the worker channel; UI loop only applies. Daemon 5s cache
    // stays; per-frame cost zero even when busy.
    spawn_git_poller(client.clone(), worker_tx.clone());
    // Idle events poll (§15 V1b + §12 V1b): once the session may have child
    // agents, poll the journal so lifecycle lines, labeled approvals, and
    // wake turns surface without waiting for a user turn.
    spawn_events_poller(
        client.clone(),
        remote.session_id.clone(),
        worker_tx,
        live_children.clone(),
        busy_poll.clone(),
        events_cursor,
    );
    if let Some(name) = session_name {
        // Keep the local placeholder's display name in sync with the daemon
        // record (a reattach overwrites `app.session` from disk below, so it
        // carries no name here).
        remote.app.session.set_name(name).ok();
    }

    // §1: first paint before replay. The terminal comes up on an empty frame
    // here; the reattach replay below then streams history in with a paint
    // per chunk/page. Time-to-first-paint no longer includes the full
    // history render.
    // Detect the terminal background before raw mode / the alternate screen
    // take over; surface colors (including the skills listing below) are
    // resolved from this once. The probe flew with the boot fan-out (§2);
    // this is instant when it finished alongside the session RTT. The probe
    // warms a memoized query; a panic in it must not take down startup.
    // (Moved up with the terminal init: the palette must resolve before raw
    // mode, and first paint precedes replay now.)
    let _ = crate::runtime::http::block_on(palette_handle);
    enable_raw_mode()?;
    // No startup drain here: a blind deadline cuts OSC reply bursts in half
    // and leaks the tail (sans lead-in) into the composer. Late replies —
    // from the theme query above or from anything else querying this tty,
    // at any time — are swallowed whole by `strip_osc_report` in the event
    // loop below.
    let cleanup = TerminalCleanup;
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
        EnableBracketedPaste,
        // Kitty keyboard protocol (disambiguate only): supporting terminals
        // then report Shift+Enter as `Enter + SHIFT` (`CSI 13;2 u`) instead of
        // the same bare `\r` as Enter, so the composer can tell "newline"
        // from "submit" (see `handle_key`: Enter without SHIFT submits,
        // everything else falls through to the composer). Terminals without
        // support ignore the sequence; Ctrl+J (`InputField::handle_key`)
        // stays the universal fallback. Popped by `TerminalCleanup`.
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    // First paint: empty transcript + composer. History streams in below.
    terminal.draw(|f| view(f, &mut remote.app))?;

    // P10: reconstruct the transcript for a reattached session. Prefer the
    // persisted JSONL messages (complete, includes user prompts the events
    // journal never records); fall back to the events journal when the file
    // isn't shared (true remote). Idempotent replays skip stale approvals
    // (parked approvals die with their turn on the daemon).
    if is_reattach {
        // §4: hold the idle poller paused across the replay — a drain past
        // the first 2s tick would otherwise serve rows the replay hasn't
        // reached yet and duplicate them in the transcript. (The event loop
        // recomputes this flag every iteration, so boot is the only window
        // it covers.)
        remote.busy_poll.store(true, Ordering::SeqCst);
        // Progressive paints (§1): each replay chunk/page draws, so history
        // streams into the already-painted frame. Best-effort: the event
        // loop's draws remain authoritative.
        let mut paint = |remote: &mut RemoteApp| {
            let _ = terminal.draw(|f| view(f, &mut remote.app));
        };
        let local = find_local_session_file(&remote.session_id);
        // Restore the journaled mode from the last `turn_start`: a user in
        // `plan` who reconnects must not land back in the ceiling's mode.
        // Falls back to the seeded selector for legacy journals / true
        // remote (no local file).
        if let Some(p) = local.as_deref() {
            if let Some(mode) = Session::last_turn_mode(p).and_then(|m| AgentMode::parse(&m).ok()) {
                // Clamp to the (possibly new) ceiling: the daemon may have
                // restarted stricter since the mode was journaled.
                if mode.permission().permissiveness() <= remote.ceiling.permissiveness() {
                    remote.mode = mode;
                    remote.options.mode = Some(mode.as_str().to_string());
                    remote.options.permission = Some(mode.permission().as_str().to_string());
                }
            }
        }
        let mut rebuilt = false;
        if let Some(p) = local.as_deref() {
            if let Ok(s) = Session::from_path(p) {
                remote.app.session = s;
            }
            if let Some(p) = local.as_deref() {
                rebuilt = rebuild_remote_from_messages(&mut remote, p, &mut paint);
            }
            if rebuilt {
                // The drain path seeds the cursor per page; the local path
                // seeds it from the local journal tip (no HTTP round trip —
                // the file is right here). Cursor is the next seq to serve
                // (inclusive), so seed max + 1. Either way the poller resumes
                // past replayed rows without its own reattach scan.
                let next = Session::max_event_seq(p).map_or(0, |m| m.saturating_add(1));
                remote.events_cursor.fetch_max(next, Ordering::SeqCst);
            }
        }
        if !rebuilt {
            let sid = remote.session_id.clone();
            replay_remote_events(&mut remote, &sid, &mut paint);
        }
        remote.busy_poll.store(false, Ordering::SeqCst);
        push_info(
            &mut remote.app,
            format!("reattached to session {session_id}"),
        );
        // The daemon resolves the tool workspace from its own cwd, not the
        // session header (`create_session` records the daemon cwd for exactly
        // that reason). Reattaching across directories therefore replays this
        // session while `read`/`write`/`bash` land in *this* tree — say so
        // instead of surprising them mid-turn.
        let session_cwd = remote.app.session.cwd().to_string();
        let workspace_cwd = remote.app.cwd.clone();
        if !same_workspace(&session_cwd, &workspace_cwd) {
            push_info(
                &mut remote.app,
                format!(
                    "dex: this session came from {session_cwd}; tools run in {workspace_cwd} \
                     (the daemon workspace) — `cd {session_cwd}` to reattach there"
                ),
            );
        }
    }

    // §3: collect the skills future here — after replay, before the
    // session-start listing (its only consumer) — so a slow daemon dir scan
    // never delays replay or first paint.
    let daemon_skills = crate::runtime::http::block_on(skills_handle).unwrap_or_default();
    // Skills live on the daemon (its workspace); a stale list is harmless —
    // the load call re-discovers on the daemon side.
    remote.app.skills = daemon_skills
        .into_iter()
        .map(|info| crate::protocol::Skill {
            name: info.name,
            description: info.description,
            path: std::path::PathBuf::from(""),
        })
        .collect();

    // Session-start header: one Banner block holding the DEX wordmark, the
    // skills the daemon discovered, and the ready time — contiguous rows, no
    // inter-block gap air between them.
    let header = vec![
        skills_header_line(&remote.app.skills),
        launch_time_line(launch_start.elapsed().as_secs_f64()),
    ];
    push_banner(&mut remote.app, header);
    // Lazy-auth empty state (the pi/opencode pattern): the daemon boots
    // without a config, so say so once here instead of failing the first
    // turn with a bare config error.
    if info.provider.is_empty() {
        push_info(
            &mut remote.app,
            "no model configured — copy a sample from examples/config.yaml, \
             set 'model: <provider>/<model>', then run `dex doctor`"
                .to_string(),
        );
    }
    // A mismatched `thinking_effort:` (config.yaml names a level the model
    // doesn't advertise) used to `eprintln!` from the daemon thread here —
    // mid OSC theme query / alternate screen — corrupting the display and
    // leaking into the composer. It now arrives as data and renders as a
    // transcript line inside the TUI.
    if let Some(warning) = info.thinking_warning.clone() {
        push_info(&mut remote.app, format!("dex: {warning}"));
    }

    Ok(Boot {
        remote,
        terminal,
        cleanup,
    })
}

/// Seed the TUI's agent mode for a launch: the explicit client
/// `--permission`/`DEX_PERMISSION` wins, else the mode derives from the
/// daemon's ceiling (`trusted` → `auto`, `ask` → `manual`, `read-only` →
/// `plan`). The result is clamped to the ceiling, which a client may only go
/// stricter than.
pub(crate) fn seed_mode(explicit: Option<PermissionMode>, ceiling: PermissionMode) -> AgentMode {
    let wanted = explicit.unwrap_or(ceiling);
    if wanted.permissiveness() > ceiling.permissiveness() {
        AgentMode::from_permission(ceiling)
    } else {
        AgentMode::from_permission(wanted)
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    #[test]
    fn trusted_ceiling_seeds_auto() {
        assert_eq!(seed_mode(None, PermissionMode::Trusted), AgentMode::Auto);
    }

    #[test]
    fn ask_ceiling_seeds_manual_and_read_only_seeds_plan() {
        assert_eq!(seed_mode(None, PermissionMode::Ask), AgentMode::Manual);
        assert_eq!(seed_mode(None, PermissionMode::ReadOnly), AgentMode::Plan);
    }

    #[test]
    fn explicit_permission_seeds_its_mode() {
        // A stock (trusted) daemon explicitly asked for `ask` stays manual.
        assert_eq!(
            seed_mode(Some(PermissionMode::Ask), PermissionMode::Trusted),
            AgentMode::Manual
        );
    }

    #[test]
    fn explicit_permission_is_clamped_to_the_ceiling() {
        // An `ask` ceiling clamps an explicit `trusted` client choice down
        // to `manual` — the ceiling always wins.
        assert_eq!(
            seed_mode(Some(PermissionMode::Trusted), PermissionMode::Ask),
            AgentMode::Manual
        );
        assert_eq!(
            seed_mode(Some(PermissionMode::Trusted), PermissionMode::ReadOnly),
            AgentMode::Plan
        );
    }
}
