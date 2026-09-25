use super::super::append_sink_line;
use super::super::bump_thinking_stamps;
use super::super::deny_all_approvals;
use super::super::push_info;
use super::super::render_user_prompt;
use super::super::resolve_approval;
use super::super::scroll_transcript;
use super::super::slash::complete_slash;
use super::super::slash::dismiss_slash;
use super::super::slash::expand_bare_command;
use super::super::slash::popup_open;
use super::super::slash::slash_suggestions;
use super::super::slash::EXPAND_ON_ENTER;
use super::super::start_activity;
use super::super::App;
use super::commands::handle_remote_slash;
use super::pollers::premature_close_error;
use super::pollers::refresh_git_async;
use super::state::RemoteApp;
use super::state::WorkerMessage;
use super::worker::try_reconnect;
use super::worker::Reconnect;
use crate::protocol::AgentMode;
use crate::protocol::ApprovalDecision;
use crate::protocol::SinkLine;
use crate::protocol::StreamEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

/// Mid-turn reconnect attempts (with 0.5s/1s/2s backoff) before a transport
/// failure is surfaced to the user instead of retried.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;

fn composer_at_end(app: &App) -> bool {
    let row = app.input.row.min(app.input.lines.len().saturating_sub(1));
    row + 1 >= app.input.lines.len() && app.input.col >= app.input.lines[row].len()
}

/// Keys in the generic composer arm that mutate the buffer (and so close a
/// history walk, turning the recalled line into a fresh draft). Cursor-only
/// keys (`Left`/`Right`/`Home`/`End`/intra-`Up`/`Down`) return false so they
/// keep walking. Mirrors `InputField::handle_key`'s mutation set: plain
/// chars, `Ctrl+J` newline, `Enter` newline (`Shift+Enter` reaches the
/// generic arm), `Backspace`/`Delete`/`Tab`. Alt/Ctrl-modified chars are
/// dropped by input and must not detach.
fn history_detaching_key(key: crossterm::event::KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            !c.is_control()
        }
        KeyCode::Char('j')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            true
        }
        KeyCode::Enter | KeyCode::Backspace | KeyCode::Delete | KeyCode::Tab => true,
        _ => false,
    }
}

/// Bracketed paste shared by the event loop: a paste that changes the buffer
/// detaches the history walk like typing does, so the pasted edit becomes
/// the fresh draft instead of being discarded by the next `Up`.
pub(crate) fn handle_paste(app: &mut App, s: &str) {
    if app.history_index.is_some() {
        let before = app.input.text();
        app.input.insert_paste(s);
        if app.input.text() != before {
            app.history_index = None;
        }
    } else {
        app.input.insert_paste(s);
    }
    // A paste can narrow the popup list like typing does.
    app.slash_selected = 0;
}

pub(crate) fn handle_key(remote: &mut RemoteApp, key: crossterm::event::KeyEvent) {
    // Copied before the `app` borrow: a `!` shell run is independent of
    // `busy` but cancels the same way (Esc cancels it).
    let shell_running = remote.shell_running;
    let shell_cancel_requested = remote.shell_cancel_requested;

    // Approval overlay takes precedence: the worker is blocked until a
    // decision arrives.
    if handle_approval_key(remote, key) {
        return;
    }
    let app = &mut remote.app;

    // Idle double Ctrl+C guard: any non-Ctrl+C key cancels the pending quit.
    let is_ctrl_c =
        matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL);
    if !is_ctrl_c {
        app.last_ctrl_c = None;
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            handle_ctrl_c(remote, shell_running, shell_cancel_requested);
        }
        KeyCode::Char('d')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !app.busy
                && !shell_running
                && app.input.text().trim().is_empty() =>
        {
            // EOF-quit on an empty composer; a draft or a live
            // turn/shell falls through to the composer below.
            app.quit = true;
        }
        KeyCode::Esc if app.busy || shell_running => {
            request_cancel(remote);
        }
        KeyCode::Char('t') if key.modifiers == KeyModifiers::CONTROL => {
            app.show_thinking = !app.show_thinking;
            bump_thinking_stamps(app);
        }
        // Alt+V: cycle the user voice color. Global chrome like Ctrl+T
        // above, so it sits before the slash-popup arm and works with the
        // popup open; already-submitted rows keep the voice they were
        // sent in, the composer and new prompts use the new one.
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => {
            let name = crate::render::theme::cycle_voice();
            app.notice = Some((format!("voice: {name}"), Instant::now()));
        }
        // Shift+Tab cycle: plan → manual → auto → plan, clamped to the
        // daemon ceiling. `BackTab` is crossterm's mapping for `ESC[Z` and
        // Kitty's `CSI 9;2u`; Shift+Tab is also accepted for terminals that
        // forward the raw pair. Placed after the approval early-return (inert
        // while an approval is parked) and before the popup arm so it works
        // with the slash popup open; the composer never sees BackTab.
        KeyCode::BackTab | KeyCode::Tab
            if key.modifiers.contains(KeyModifiers::SHIFT)
                || matches!(key.code, KeyCode::BackTab) =>
        {
            cycle_mode(remote);
        }
        // Alt+Up while working: pull the newest queued message back into the
        // composer to edit it. Hoisted above the slash-popup arm so the
        // popup's highlight navigation can't swallow the "Alt+Up again for
        // more" affordance. Best-effort — an item already accepted at a
        // model boundary is gone from the queue and renders as a transcript
        // block instead.
        KeyCode::Up
            if key.modifiers.contains(KeyModifiers::ALT)
                && (!app.pending_steering.is_empty() || !app.pending_followups.is_empty()) =>
        {
            recall_queued(remote);
        }
        _ if popup_open(app) => handle_popup_key(remote, key),
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
        KeyCode::Up => handle_up_key(app, key),
        KeyCode::Down => handle_down_key(app, key),
        _ => handle_composer_key(app, key),
    }
}

/// Shift+Tab: advance the mode one stop, clamped to the daemon ceiling. A
/// clamped cycle is a no-op that reports why instead of silently wrapping
/// past the boundary.
fn cycle_mode(remote: &mut RemoteApp) {
    let wanted = remote.mode.next();
    let clamp = AgentMode::from_permission(remote.ceiling);
    if wanted.permission().permissiveness() > clamp.permission().permissiveness() {
        remote.app.notice = Some((
            format!(
                "mode: {} (ceiling {} — raise with --permission/DEX_PERMISSION)",
                wanted.label(),
                remote.ceiling.as_str()
            ),
            Instant::now(),
        ));
        return;
    }
    apply_mode(remote, wanted);
    remote.app.notice = Some((format!("mode: {}", wanted.label()), Instant::now()));
}

/// Set the mode and keep every derived surface in sync: the session value,
/// the display config (approval overlay), and the per-request `options`
/// (both `mode` and the derived `permission`, so older daemons restrict
/// identically). Takes effect on the next submit; an in-flight turn keeps
/// the mode it started with.
fn apply_mode(remote: &mut RemoteApp, mode: AgentMode) {
    remote.mode = mode;
    remote.app.config.permission = mode.permission();
    remote.options.mode = Some(mode.as_str().to_string());
    remote.options.permission = Some(mode.permission().as_str().to_string());
}

/// Approval overlay: the worker is blocked until a decision arrives, so this
/// owns every key while an approval is pending. Returns true when one was
/// showing (the caller then returns without touching the composer).
fn handle_approval_key(remote: &mut RemoteApp, key: crossterm::event::KeyEvent) -> bool {
    if remote.app.pending_approvals.is_empty() {
        return false;
    }
    let app = &mut remote.app;
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Deny the pending approval and cancel the turn; another Ctrl+C
            // once idle quits.
            resolve_approval(app, ApprovalDecision::Deny);
            request_cancel(remote);
        }
        KeyCode::Up | KeyCode::Left => {
            if let Some(approval) = app.pending_approvals.first_mut() {
                approval.selected = approval.selected.saturating_sub(1);
            }
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Tab => {
            if let Some(approval) = app.pending_approvals.first_mut() {
                approval.selected = (approval.selected + 1).min(2);
            }
        }
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            resolve_approval(app, ApprovalDecision::AllowOnce);
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            resolve_approval(app, ApprovalDecision::AllowSession);
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            resolve_approval(app, ApprovalDecision::Deny);
        }
        KeyCode::Enter => {
            let decision = app
                .pending_approvals
                .first()
                .map(|approval| match approval.selected {
                    0 => ApprovalDecision::AllowOnce,
                    1 => ApprovalDecision::AllowSession,
                    _ => ApprovalDecision::Deny,
                });
            if let Some(decision) = decision {
                resolve_approval(app, decision);
            }
        }
        _ => {}
    }
    true
}

/// Ctrl+C: cancel a running turn/shell (with the force-quit guard), clear a
/// drafted prompt, or arm the double-press quit when idle.
fn handle_ctrl_c(remote: &mut RemoteApp, shell_running: bool, shell_cancel_requested: bool) {
    let app = &mut remote.app;
    if app.busy || shell_running {
        if app.cancel_requested || shell_cancel_requested {
            // Cancel already in flight: stay idempotent. Only the third press
            // force-quits (stuck daemon/shell) — a slow turn must never die to
            // an impatient double-tap.
            app.cancel_presses = app.cancel_presses.saturating_add(1);
            if app.cancel_presses >= 3 {
                app.quit = true;
            } else {
                push_info(
                    app,
                    "still cancelling... (Ctrl+C again to force quit)".to_string(),
                );
            }
        } else {
            request_cancel(remote);
            remote.app.cancel_presses = 1;
        }
    } else if !app.input.text().is_empty() {
        // First press with a drafted prompt just clears the composer;
        // quitting needs an empty line.
        app.input.reset();
        app.slash_selected = 0;
        app.last_ctrl_c = None;
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

/// Slash-command popup: owns Up/Down/Tab/Enter/Esc and feeds typing through to
/// the composer so the highlight tracks the filtered list.
fn handle_popup_key(remote: &mut RemoteApp, key: crossterm::event::KeyEvent) {
    let app = &mut remote.app;
    match key.code {
        KeyCode::Esc => {
            // Discard the drafted slash command and close the popup without
            // completing anything (busy+Esc still cancels).
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
            // Bare picker command (`/model`, `/provider`, `/resume`): first
            // Enter expands to `"<cmd> "` and shows the popup instead of
            // submitting the bare form (which would only print info into the
            // transcript). Same expansion applies to a bare picker command
            // recalled from history — the recalled line is the text, so expand
            // it here too.
            if expand_bare_command(app) {
                return;
            }
            // Command-name completion without an argument yet (`/mod` →
            // `/model `): complete but don't submit while the result is still a
            // bare picker command. Argument-less commands (`/clear`) still
            // submit immediately, and argument completions (`/model foo`,
            // `/resume 0`) complete + submit the highlighted choice as before.
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
            // composer and jump back to the top match so the highlight never
            // strands past the filtered results (e.g. `/` + `r` lands on
            // `/resume` instead of a stale arrow position).
            app.input.handle_key(key);
            app.slash_selected = 0;
        }
        _ => app.input.handle_key(key),
    }
}

/// Up: scroll while busy/Shift, walk history at a single-line composer, else
/// move the composer cursor.
fn handle_up_key(app: &mut App, key: crossterm::event::KeyEvent) {
    if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
        scroll_transcript(app, -1);
    } else if app.input.lines.len() <= 1
        || (app.history_index.is_some() && composer_at_end(app))
        || app.input.row == 0
    {
        app.history_up();
    } else {
        app.input.handle_key(key);
    }
}

/// Down: scroll while busy/Shift, walk history at the last line, else move the
/// composer cursor.
fn handle_down_key(app: &mut App, key: crossterm::event::KeyEvent) {
    if app.busy || key.modifiers.contains(KeyModifiers::SHIFT) {
        scroll_transcript(app, 1);
    } else if app.input.lines.len() <= 1 || app.input.row + 1 >= app.input.lines.len() {
        app.history_down();
    } else {
        app.input.handle_key(key);
    }
}

/// Default composer path: a mutating keystroke turns a recalled line into a
/// fresh draft (next Up saves the edited text); cursor-only keys keep the walk
/// so Left + Up moves within the recalled prompt. The text comparison keeps
/// no-op Backspace/Delete on the walk instead of detaching for an unchanged
/// buffer.
fn handle_composer_key(app: &mut App, key: crossterm::event::KeyEvent) {
    if app.history_index.is_some() && history_detaching_key(key) {
        let before = app.input.text();
        app.input.handle_key(key);
        if app.input.text() != before {
            app.history_index = None;
        }
    } else {
        app.input.handle_key(key);
    }
}

fn request_cancel(remote: &mut RemoteApp) {
    let turn_running = remote.app.busy;
    if !turn_running && !remote.shell_running {
        return;
    }
    // A cancel is already unwinding everything running (double Esc / overlay
    // re-press): don't re-POST or spam the transcript.
    let turn_done = !turn_running || remote.app.cancel_requested;
    let shell_done = !remote.shell_running || remote.shell_cancel_requested;
    if turn_done && shell_done {
        return;
    }
    if turn_running && !remote.app.cancel_requested {
        remote.app.cancel_requested = true;
        remote.cancel_flag.store(true, Ordering::SeqCst);
        // Deny every queued approval so the blocked agent threads unwind
        // (child agents may have parked several).
        deny_all_approvals(&mut remote.app);
    }
    if remote.shell_running && !remote.shell_cancel_requested {
        remote.shell_cancel_requested = true;
    }
    // One Esc cancels whatever is running: the daemon signals the turn
    // token and the shell token alike (a turn and a shell overlap only
    // when the user explicitly started both).
    match remote.client.cancel(&remote.session_id) {
        Ok(()) => push_info(&mut remote.app, "cancelling...".to_string()),
        Err(e) => push_info(&mut remote.app, format!("cancel failed: {e}")),
    }
}

/// Run a `!`/`!!` shell escape on the daemon: no agent turn, no
/// approval — the `!` itself is the approval. Renders the typed line now;
/// the `bash` tool block lands when the worker answers. The daemon saves
/// the run to session history: `!` feeds the next turn, `!!` stays out of
/// the model context.
fn run_shell_command(remote: &mut RemoteApp, line: String, command: String, excluded: bool) {
    render_user_prompt(&mut remote.app, &line);
    remote.shell_running = true;
    remote.shell_cancel_requested = false;
    remote.app.cancel_presses = 0;
    let client = remote.client.clone();
    let sid = remote.session_id.clone();
    let tx = remote.worker_tx.clone();
    crate::runtime::http::spawn_task(async move {
        let start = Instant::now();
        match client.shell_async(&sid, &command, excluded).await {
            Ok(resp) => {
                let _ = tx
                    .send(WorkerMessage::Shell {
                        command,
                        output: resp.output,
                        success: resp.success,
                        duration: start.elapsed().as_secs_f64(),
                        excluded,
                    })
                    .await;
            }
            Err(error) => {
                let _ = tx
                    .send(WorkerMessage::Shell {
                        command,
                        output: format!("Error: {error}"),
                        success: false,
                        duration: start.elapsed().as_secs_f64(),
                        excluded,
                    })
                    .await;
            }
        }
    });
}

/// Render a finished `!`/`!!` shell run as one self-contained `bash` tool
/// block. Pushed back-to-back (input then output) so no open block lingers
/// while the command runs; the pair shares a unique id so concurrent runs
/// can't steal each other's half even if the two pairs interleave.
pub(crate) fn finish_shell_command(
    remote: &mut RemoteApp,
    command: &str,
    output: &str,
    success: bool,
    duration: f64,
    excluded: bool,
) {
    static SHELL_BLOCK_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let block_id = format!(
        "shell-{}",
        SHELL_BLOCK_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let input = serde_json::json!({"command": command}).to_string();
    let short = crate::render::format::short_arg("bash", &input);
    append_sink_line(
        &mut remote.app,
        SinkLine::ToolInput {
            id: block_id.clone(),
            input: format!("bash {short}"),
        },
    );
    let mut summary =
        crate::render::format::tool_result_summary("bash", &input, output, success, None);
    if excluded {
        summary.push_str(" · excluded from context");
    }
    let preview = crate::render::format::tool_preview(
        "bash",
        success,
        None,
        output,
        crate::render::format::preview_skips_first_line("bash", success, output),
    );
    append_sink_line(
        &mut remote.app,
        SinkLine::ToolOutput {
            id: block_id,
            name: "bash".into(),
            summary,
            success,
            preview,
            duration,
        },
    );
    // The command may have switched branches or dirtied the tree.
    refresh_git_async(remote.client.clone(), remote.worker_tx.clone());
}

/// Alt+Up while a turn is running: pull the newest queued steering message
/// (falling back to a follow-up) back into the composer and tell the daemon to
/// drop its queued copy, so the user can edit before it is injected.
/// Best-effort: a message already accepted at a model boundary is gone from
/// the queue and renders as a transcript block instead. The local queue is
/// only touched after the daemon accepts the recall, so a failed call leaves
/// badges and draft untouched.
pub(crate) fn recall_queued(remote: &mut RemoteApp) {
    let Some((text, followup)) = recall_candidate(&remote.app) else {
        return;
    };
    let client = remote.client.clone();
    let sid = remote.session_id.clone();
    if let Err(e) = client.recall(&sid, &text, followup) {
        push_info(
            &mut remote.app,
            format!("could not recall queued message: {e}"),
        );
        return;
    }
    if followup {
        remote.app.pending_followups.pop();
    } else {
        remote.app.pending_steering.pop();
    }
    // Append to any draft so typed text is never lost; one item per press.
    let app = &mut remote.app;
    let mut draft = app.input.text();
    if !draft.trim().is_empty() {
        draft.push('\n');
    }
    draft.push_str(&text);
    app.input = crate::ui::input::InputField::from_text(&draft);
    app.slash_selected = 0;
    push_info(
        app,
        format!(
            "recalled queued {} for editing (Alt+Up again for more)",
            if followup { "follow-up" } else { "steer" }
        ),
    );
}

/// Newest queued message to recall: steers first, then follow-ups (matching
/// the badge order). Peeks instead of popping so a failed recall can leave
/// the queue intact.
pub(crate) fn recall_candidate(app: &App) -> Option<(String, bool)> {
    if let Some(text) = app.pending_steering.last() {
        return Some((text.clone(), false));
    }
    app.pending_followups
        .last()
        .map(|text| (text.clone(), true))
}

fn submit_prompt(remote: &mut RemoteApp, is_followup: bool) {
    let line = remote.app.input.text().trim().to_string();
    if line.is_empty() {
        return;
    }
    // `!`/`!!` shell escape first (before the busy queue): it never touches
    // the agent loop, so there is no turn to steer — and it may run
    // alongside one (only one shell at a time per session; Esc cancels it).
    // A bare `!`/`!!` falls through to the agent.
    if let Some((command, excluded)) = crate::tools::parse_shell_escape(&line) {
        remote.app.history_push(line.clone());
        remote.app.input.reset();
        if remote.shell_running {
            push_info(
                &mut remote.app,
                "A bash command is already running. Press Esc to cancel it first.".to_string(),
            );
        } else {
            run_shell_command(remote, line, command, excluded);
        }
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
    app.cancel_presses = 0;
    // Live "● Working" indicator inside the transcript; settled to the
    // "Worked for …" summary by `finish_turn`.
    start_activity(app);
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
    // P10 idempotency: a unique key per submission. If the connection drops
    // mid-turn the worker re-POSTs with the SAME key — a COMPLETED turn
    // replays its recorded terminal event instead of re-running effects.
    // A turn that died mid-way (no terminal recorded) re-executes on
    // retry; only completed turns dedup. A still-running turn answers 409.
    options.idempotency_key = Some(uuid::Uuid::new_v4().to_string());
    let prompt = line;
    let event_tx = remote.worker_tx.clone();

    crate::runtime::http::spawn_task(async move {
        let mut saw_terminal = false;
        // Journal cursor: highest seq delivered to the UI; a reconnect
        // replays only what came after it.
        let mut last_seq = 0u64;
        // Reconnect attempts per turn, with backoff between them — a
        // daemon restart or network blip gets a few chances to settle
        // before the failure is surfaced as real. Retries are safe: each
        // re-POST reuses the same idempotency key.
        let mut reconnect_attempts = 0u32;
        let mut stream = match client
            .chat_stream(&session_id, &prompt, options.clone())
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                let _ = event_tx
                    .send(WorkerMessage::Finished(Some(e.to_string())))
                    .await;
                return;
            }
        };
        loop {
            match stream.next_event().await {
                Some(Ok(event)) => {
                    if matches!(
                        &event,
                        StreamEvent::TurnComplete { .. } | StreamEvent::TurnFailed { .. }
                    ) {
                        saw_terminal = true;
                    }
                    last_seq = last_seq.max(stream.last_seq());
                    // Backpressured: awaits UI drain instead of dropping when
                    // the transcript bursts faster than the 8fps redraw.
                    if event_tx.send(WorkerMessage::Stream(event)).await.is_err() {
                        return;
                    }
                }
                // A transport failure (`Some(Err)`) or a close without a
                // terminal event (`None`): one reattach attempt replays the
                // journal from our cursor and re-POSTs with the same
                // idempotency key (completed turns dedup; mid-way deaths
                // re-execute; still-running turns get 409).
                outcome => {
                    let transport_error = match outcome {
                        Some(Err(e)) => Some(e),
                        None => None,
                        // First arm handles every `Some(Ok)`; treat a stray
                        // as no error rather than panicking.
                        Some(Ok(_)) => None,
                    };
                    if saw_terminal {
                        // The daemon always closes right after the terminal
                        // event; a clean close here is success.
                        let _ = event_tx.send(WorkerMessage::Cursor(last_seq)).await;
                        let _ = event_tx.send(WorkerMessage::Finished(None)).await;
                        return;
                    }
                    if reconnect_attempts < MAX_RECONNECT_ATTEMPTS {
                        reconnect_attempts += 1;
                        tokio::time::sleep(Duration::from_millis(
                            500 * (1 << (reconnect_attempts - 1)),
                        ))
                        .await;
                        match try_reconnect(
                            &client,
                            &session_id,
                            &prompt,
                            &options,
                            &mut last_seq,
                            &event_tx,
                        )
                        .await
                        {
                            Reconnect::Resumed(new_stream) => {
                                let _ = event_tx
                                    .send(WorkerMessage::Stream(StreamEvent::System(
                                        "connection lost mid-turn; reattached and replaying the missed events"
                                            .into(),
                                    )))
                                    .await;
                                stream = new_stream;
                                continue;
                            }
                            Reconnect::Terminal => {
                                let _ = event_tx.send(WorkerMessage::Cursor(last_seq)).await;
                                let _ = event_tx.send(WorkerMessage::Finished(None)).await;
                                return;
                            }
                            Reconnect::Failed(msg) => {
                                let _ = event_tx.send(WorkerMessage::Cursor(last_seq)).await;
                                let _ = event_tx.send(WorkerMessage::Finished(Some(msg))).await;
                                return;
                            }
                        }
                    }
                    // Recovery exhausted: surface the real transport failure
                    // (the daemon always closes with TurnComplete/TurnFailed,
                    // so a close without one is a transport failure too).
                    let _ = event_tx.send(WorkerMessage::Cursor(last_seq)).await;
                    let _ = event_tx
                        .send(WorkerMessage::Finished(
                            transport_error.or_else(|| premature_close_error(false)),
                        ))
                        .await;
                    return;
                }
            }
        }
    });
}
