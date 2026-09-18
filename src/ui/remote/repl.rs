use super::super::view;
use super::boot::bootstrap;
use super::boot::Boot;
use super::input::finish_turn;
use super::input::handle_mouse;
use super::input::handle_stream_event;
use super::keys::finish_shell_command;
use super::keys::handle_key;
use super::keys::handle_paste;
use super::osc::strip_osc_report;
use super::pollers::apply_git_info;
use super::resume::print_resume_hint;
use super::state::WorkerMessage;
use crate::cli::Args;
use crate::protocol::StreamEvent;
use crossterm::event;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableMouseCapture;
use crossterm::event::Event;
use crossterm::event::KeyEventKind;
use crossterm::execute;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::LeaveAlternateScreen;
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

/// `daemon_is_local` says whether this process owns the daemon it talks to
/// (`Mode::Default`) rather than connecting to one it does not (`Mode::Connect`).
/// It decides the quit-time resume command — see `resume_command`.
///
/// Render/event-loop half of the remote REPL: `bootstrap` owns terminal setup,
/// this owns the draw/poll loop and the restore + resume hint on the way out.
pub(crate) fn run_ratatui_repl_with_remote(
    args: &Args,
    daemon_url: &str,
    daemon_is_local: bool,
) -> std::io::Result<()> {
    let Boot {
        mut remote,
        mut terminal,
        cleanup,
    } = bootstrap(args, daemon_url, daemon_is_local)?;
    // Report lifecycle state to the enclosing Herdr pane, if any.
    let mut herdr = super::super::herdr::Reporter::new();
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
                    Ok(WorkerMessage::Cursor(seq)) => {
                        remote.events_cursor.fetch_max(seq, Ordering::SeqCst);
                    }
                    Ok(WorkerMessage::Finished(error)) => {
                        streamed = true;
                        finish_turn(&mut remote, error);
                    }
                    Ok(WorkerMessage::Shell {
                        command,
                        output,
                        success,
                        duration,
                        excluded,
                    }) => {
                        streamed = true;
                        remote.shell_running = false;
                        remote.shell_cancel_requested = false;
                        remote.app.cancel_presses = 0;
                        finish_shell_command(
                            &mut remote,
                            &command,
                            &output,
                            success,
                            duration,
                            excluded,
                        );
                    }
                    Err(_) => break,
                }
            }

            // A `!` shell run animates like a turn even with no agent turn in
            // flight (otherwise a long shell looks frozen).
            let busy = remote.app.busy || remote.shell_running;
            // The idle poller pauses while anything streams (the turn's own
            // SSE carries those rows) and resumes at the published cursor.
            remote.busy_poll.store(busy, Ordering::SeqCst);
            // Cheap when unchanged (outside Herdr it is a no-op).
            herdr.sync(
                busy,
                remote
                    .app
                    .pending_approvals
                    .first()
                    .map(|a| a.name.as_str()),
            );
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
                    handle_paste(&mut remote.app, &s);
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
    // Always release, even if the loop returned early via `?` (draw/poll
    // error) — otherwise a stale agent row lingers in the Herdr sidebar.
    herdr.release();
    // Always restore the terminal, even if the loop returned early via `?`.
    disable_raw_mode().ok();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    );
    // Drop `TerminalCleanup` here rather than at the end of the function. Its
    // Drop writes `CSI ?1049l` / the Kitty pop, and `CSI ?1049l` also *restores
    // the cursor* to where it sat before the TUI (xterm's `srm_OPT_ALTBUF_CURSOR`,
    // followed by DECRC). Left to the end, that lands after the resume hint, the
    // shell redraws its prompt on the restored line, and the prompt overwrites
    // the hint's prefix — leaving just the tail (the session id) visible.
    // Drop runs on every path, so the early returns above stay covered. The
    // Kitty disambiguate pop stays there (not in the `execute!` above): the
    // flags are a push/pop stack, so popping in both places would unbalance a
    // terminal we don't own (e.g. nested in another Kitty-aware app).
    drop(cleanup);
    // ratatui's `Terminal` Drop re-shows the cursor; nothing may follow the hint.
    drop(terminal);
    // The session outlives the TUI (the daemon persisted it), so hand the user
    // the exact command to come back instead of making them hunt `/resume`.
    // Printed on every quit, reattach included: the id and the daemon URL are
    // what the user needs, and they are not always the ones they typed. Nothing
    // may be written to the terminal after this point.
    print_resume_hint(
        daemon_url,
        &remote.session_id,
        remote.app.session.cwd(),
        daemon_is_local,
    );
    res
}
