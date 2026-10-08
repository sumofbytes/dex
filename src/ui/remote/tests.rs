//! Tests, split out of the module body so it stays implementation.

use super::*;

#[test]
fn output_rate_needs_output_and_timing() {
    assert_eq!(output_rate(500, Some(1_000)), Some(500.0));
    // Sub-second durations scale up (2k tokens in 400ms -> 5k tok/s).
    assert_eq!(output_rate(2_000, Some(400)), Some(5_000.0));
    // No timing (old daemon / untimed compaction call) hides the rate.
    assert_eq!(output_rate(500, None), None);
    // A zero duration or an empty completion is not a rate.
    assert_eq!(output_rate(500, Some(0)), None);
    assert_eq!(output_rate(0, Some(1_000)), None);
}

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
fn resume_command_matches_invocation_not_url_host() {
    let here = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // Locally owned daemon, same workspace: the id alone is enough.
    assert_eq!(
        resume_command("http://127.0.0.1:4113", "dex-k3m9x2qp7w4n8t5v", &here, true),
        "dex --reattach dex-k3m9x2qp7w4n8t5v"
    );
    // Locally owned daemon but a session from elsewhere: its tools are
    // confined to the daemon cwd, so the command must cd back first.
    assert_eq!(
        resume_command(
            "http://127.0.0.1:4113",
            "dex-k3m9x2qp7w4n8t5v",
            "/srv/other-repo",
            true
        ),
        "cd /srv/other-repo && dex --reattach dex-k3m9x2qp7w4n8t5v"
    );
    // A loopback URL can still be remote (SSH port-forward, container):
    // `connect` means this process does not own that daemon.
    assert_eq!(
        resume_command("http://127.0.0.1:4113", "sess-1", "/srv/other-repo", false),
        "dex connect http://127.0.0.1:4113 --reattach sess-1"
    );
    assert_eq!(
        resume_command(
            "https://agent.example.com",
            "sess-1",
            "/srv/other-repo",
            false
        ),
        "dex connect https://agent.example.com --reattach sess-1"
    );
    // Shell metacharacters (globs, separators, spaces) get quoted.
    assert_eq!(
        resume_command(
            "http://user:pw@host:8420/x?t=1",
            "sess-1",
            "/srv/other-repo",
            false
        ),
        "dex connect 'http://user:pw@host:8420/x?t=1' --reattach sess-1"
    );
    assert_eq!(
        shell_quote("/srv/my repo"),
        "'/srv/my repo'",
        "a cwd with a space must survive one shell round trip"
    );
}

#[test]
fn resume_hint_puts_command_on_own_line() {
    let cmd = "dex --reattach sess-1";
    // Redirected stderr stays plain so each line stays greppable.
    assert_eq!(
        format_resume_hint(cmd, false),
        "\nTo resume this session:\ndex --reattach sess-1"
    );
    // Label-only dim: the command stays bright for copy-paste, and no
    // trailing SGR leaks into the shell's next prompt.
    assert_eq!(
        format_resume_hint(cmd, true),
        format!("\n{DIM}To resume this session:{RESET}\n{cmd}")
    );
    assert!(format_resume_hint(cmd, true).ends_with(cmd));
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

#[test]
fn local_session_file_resolves_by_id_when_files_are_shared() {
    // `/resume` resolves the daemon sid (an id, not an index/path), so the
    // local lookup must handle ids — `Session::resume` only handles
    // index/path and silently missed, leaving a blank terminal.
    let _guard = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-resume-{}", std::process::id()));
    let _env =
        crate::test_env::EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    std::env::set_var("XDG_DATA_HOME", &dir);
    let mut s = crate::session::Session::new("/tmp/dex-resume-cwd".into(), None).unwrap();
    s.append_message(&crate::protocol::ChatMessage::user("hi".to_string()))
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
fn premature_close_reports_transport_failure() {
    assert_eq!(
        premature_close_error(false),
        Some("connection closed before turn completed".to_string())
    );
    assert_eq!(premature_close_error(true), None);
}

#[tokio::test]
async fn closed_decision_channel_resolves_to_deny() {
    // V1b: each queued approval carries its own decision channel; the
    // TUI going away must resolve to Deny instead of stranding the
    // parked approval (the overlay's resolve path owns the sender).
    // `spawn_approval_poster` maps the `None` from a closed channel to
    // `Deny`; here we pin the channel semantics it relies on.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ApprovalDecision>(1);
    drop(tx);
    assert!(
        rx.recv().await.is_none(),
        "no sender: the poster maps None to Deny"
    );
}

fn key(code: KeyCode, mods: KeyModifiers) -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
    KeyEvent {
        code,
        modifiers: mods,
        kind: KeyEventKind::Press,
        state: KeyEventState::empty(),
    }
}

fn ctrl_c() -> crossterm::event::KeyEvent {
    key(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn ctrl_d() -> crossterm::event::KeyEvent {
    key(KeyCode::Char('d'), KeyModifiers::CONTROL)
}

fn esc_key() -> crossterm::event::KeyEvent {
    key(KeyCode::Esc, KeyModifiers::empty())
}

/// Minimal `RemoteApp` for `handle_key` tests. The client points at a
/// dead port so `request_cancel`'s POST fails fast (connection refused)
/// instead of hanging.
fn test_remote() -> RemoteApp {
    let mut app = App::scratch();
    app.remote_mode = true;
    app.config.model = "test".into();
    app.config.available_models = vec!["test".into()];
    let (worker_tx, worker_rx) = mpsc::channel::<WorkerMessage>(16);
    RemoteApp {
        app,
        client: DaemonClient::new("http://127.0.0.1:9").expect("test client builds"),
        session_id: "test".to_string(),
        options: ChatOptions::default(),
        mode: AgentMode::Manual,
        ceiling: PermissionMode::Trusted,
        worker_tx,
        worker_rx,
        cancel_flag: Arc::new(AtomicBool::new(false)),
        last_click: None,
        shell_running: false,
        shell_cancel_requested: false,
        live_children: Arc::new(AtomicBool::new(false)),
        busy_poll: Arc::new(AtomicBool::new(false)),
        events_cursor: Arc::new(AtomicU64::new(0)),
    }
}

#[test]
fn ctrl_c_clears_draft_before_armed_quit() {
    let mut remote = test_remote();
    remote.app.input.insert_paste("hello");
    handle_key(&mut remote, ctrl_c());
    assert!(!remote.app.quit, "draft Ctrl+C must not quit");
    assert!(
        remote.app.input.text().is_empty(),
        "first press clears the draft"
    );
    assert!(
        remote.app.last_ctrl_c.is_none(),
        "clearing disarms the pending quit"
    );
    // Empty composer now arms; the second press quits.
    handle_key(&mut remote, ctrl_c());
    assert!(!remote.app.quit);
    assert!(remote.app.last_ctrl_c.is_some());
    handle_key(&mut remote, ctrl_c());
    assert!(remote.app.quit);
}

#[test]
fn ctrl_c_clears_whitespace_only_draft() {
    let mut remote = test_remote();
    remote.app.input.insert_paste("   ");
    handle_key(&mut remote, ctrl_c());
    assert!(!remote.app.quit);
    assert!(remote.app.input.text().is_empty());
}

#[test]
fn alt_v_cycles_the_voice_without_touching_the_draft() {
    // The voice slot is process-global, so serialize against the theme
    // tests that assert on it.
    let _guard = crate::render::theme::VOICE_SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut remote = test_remote();
    remote.app.input.insert_paste("draft");
    handle_key(
        &mut remote,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::ALT,
        ),
    );
    let (notice, _) = remote.app.notice.clone().expect("Alt+V shows a notice");
    assert!(notice.starts_with("voice: "), "unexpected notice: {notice}");
    assert_eq!(
        remote.app.input.text(),
        "draft",
        "Alt+V must not edit the draft"
    );
}

#[test]
fn busy_ctrl_c_needs_three_presses_to_force_quit() {
    let mut remote = test_remote();
    remote.app.busy = true;
    handle_key(&mut remote, ctrl_c());
    assert!(remote.app.cancel_requested, "first press cancels");
    assert!(!remote.app.quit);
    handle_key(&mut remote, ctrl_c());
    assert!(
        !remote.app.quit,
        "second press must not force-quit a slow turn"
    );
    handle_key(&mut remote, ctrl_c());
    assert!(remote.app.quit, "third press force-quits a stuck turn");
}

#[test]
fn recall_candidate_prefers_newest_steer_then_followup() {
    let mut remote = test_remote();
    assert!(recall_candidate(&remote.app).is_none(), "empty queue");
    remote.app.pending_followups.push("follow".into());
    remote.app.pending_steering.push("steer".into());
    // Steers are recalled before follow-ups regardless of queue order.
    let (text, followup) = recall_candidate(&remote.app).expect("candidate");
    assert_eq!(text, "steer");
    assert!(!followup);
    remote.app.pending_steering.pop();
    let (text, followup) = recall_candidate(&remote.app).expect("candidate");
    assert_eq!(text, "follow");
    assert!(followup);
}

#[test]
fn recall_queued_failure_keeps_queue_and_draft() {
    let mut remote = test_remote();
    remote.app.busy = true;
    remote.app.pending_steering.push("queued msg".into());
    remote.app.input = crate::ui::input::InputField::from_text("draft text");
    // The fixture's client points at a dead port: the POST fails, so the
    // badge must stay queued and the draft must be untouched.
    recall_queued(&mut remote);
    assert_eq!(remote.app.pending_steering, vec!["queued msg".to_string()]);
    assert_eq!(remote.app.input.text(), "draft text");
    assert!(!remote.app.transcript.is_empty(), "failure is reported");
}

#[test]
fn esc_repress_while_cancelling_stays_put() {
    let mut remote = test_remote();
    remote.app.busy = true;
    remote.app.cancel_requested = true;
    handle_key(&mut remote, esc_key());
    assert!(!remote.app.quit);
    assert!(remote.app.cancel_requested);
}

#[test]
fn ctrl_d_quits_only_on_empty_idle_composer() {
    let mut remote = test_remote();
    handle_key(&mut remote, ctrl_d());
    assert!(remote.app.quit, "empty idle Ctrl+D quits");

    let mut remote = test_remote();
    remote.app.input.insert_paste("draft");
    handle_key(&mut remote, ctrl_d());
    assert!(!remote.app.quit, "draft Ctrl+D must not quit");
    assert_eq!(remote.app.input.text(), "draft");

    let mut remote = test_remote();
    remote.app.busy = true;
    handle_key(&mut remote, ctrl_d());
    assert!(!remote.app.quit, "busy Ctrl+D must not quit the turn");
}

#[test]
fn up_arrow_walks_history_through_recalled_slash_entries() {
    // A recalled "/clear" must not resurrect the slash popup and trap
    // Up/Down: the walk continues to older entries, Esc keeps the
    // recalled line instead of discarding it, and Down returns to the
    // newest entry and then the live draft.
    let mut remote = test_remote();
    remote.app.history_push("/clear".into());
    remote.app.history_push("plain draft".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "plain draft");
    assert!(!popup_open(&remote.app), "no popup while walking");

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "/clear");
    assert!(
        !popup_open(&remote.app),
        "recalled slash entry must not trap Up in the popup"
    );

    handle_key(&mut remote, esc_key());
    assert_eq!(
        remote.app.input.text(),
        "/clear",
        "Esc mid-walk keeps the recalled line"
    );

    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "plain draft");
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "",
        "Down past newest restores draft"
    );
    assert_eq!(remote.app.history_index, None);
}

#[test]
fn enter_on_recalled_slash_entry_submits_it() {
    // Enter on a recalled "/clear" executes it exactly like typed input
    // (no popup completion in the way): one history entry, no duplicate,
    // walk closed, composer emptied.
    let mut remote = test_remote();
    remote.app.history_push("/clear".into());
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(
        remote.app.history.last().map(String::as_str),
        Some("/clear")
    );
    assert_eq!(remote.app.history.len(), 1, "no duplicate entry");
    assert!(remote.app.input.text().is_empty());
    assert_eq!(remote.app.history_index, None, "walk closed after submit");
}

#[test]
fn alt_up_reaches_recall_while_popup_is_open() {
    // A queued slash command in the composer opens the popup; Alt+Up
    // must still reach the recall arm instead of being swallowed by the
    // popup's highlight navigation (plain Up keeps moving the popup).
    let mut remote = test_remote();
    remote.app.input = crate::ui::input::InputField::from_text("/cl");
    remote.app.slash_selected = 1;
    remote.app.pending_steering.push("queued steering".into());
    assert!(popup_open(&remote.app), "slash draft opens the popup");

    // The fixture's client points at a dead port, so the recall POST
    // fails and reports it — reaching that report proves Alt+Up got
    // past the popup to the recall arm; queue and highlight untouched.
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(
        remote.app.pending_steering,
        vec!["queued steering".to_string()],
        "Alt+Up reached the recall arm"
    );
    assert_eq!(remote.app.input.text(), "/cl");
    assert_eq!(remote.app.slash_selected, 1, "popup did not move it");
    assert!(
        !remote.app.transcript.is_empty(),
        "recall failure was reported"
    );

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.slash_selected, 0, "plain Up moves the popup");
}

#[test]
fn up_at_end_of_recalled_multiline_walks_older() {
    // Browsing: the recall parks the cursor at the end, so a second `Up`
    // without moving must keep walking instead of stepping intra-line.
    let mut remote = test_remote();
    remote.app.history_push("oldest".into());
    remote.app.history_push("line1\nline2\nline3".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "line1\nline2\nline3");
    assert_eq!(remote.app.history_index, Some(0));

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "oldest",
        "Up at end of recalled entry walks older"
    );
}

#[test]
fn left_then_up_moves_within_recalled_multiline_prompt() {
    // `Left` proves the user is editing the recalled prompt: the next
    // `Up` must move within it, and the walk only resumes at row 0.
    let mut remote = test_remote();
    remote.app.history_push("oldest".into());
    remote.app.history_push("line1\nline2\nline3".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Left, KeyModifiers::empty()));
    assert_eq!(remote.app.history_index, Some(0), "cursor move keeps walk");

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "line1\nline2\nline3",
        "Left+Up stays inside the recalled prompt"
    );
    assert_eq!(remote.app.input.row, 1);
    assert_eq!(remote.app.history_index, Some(0));

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.row, 0);
    assert_eq!(remote.app.input.text(), "line1\nline2\nline3");

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "oldest",
        "Up at row 0 resumes the walk"
    );
}

#[test]
fn down_moves_within_recalled_multiline_prompt() {
    // `Down` from a middle row of a recalled entry steps intra-line
    // instead of jumping to a newer entry; the last row walks.
    let mut remote = test_remote();
    remote.app.history_push("oldest".into());
    remote.app.history_push("line1\nline2\nline3".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Left, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.row, 0);

    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(remote.app.input.row, 1);
    assert_eq!(remote.app.input.text(), "line1\nline2\nline3");
    assert_eq!(remote.app.history_index, Some(0));

    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(remote.app.input.row, 2);
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "",
        "Down on the last row leaves the walk to the draft"
    );
    assert_eq!(remote.app.history_index, None);
}

#[test]
fn typing_on_recalled_entry_detaches_into_fresh_draft() {
    // Editing a recalled line closes the walk: the edit is preserved and
    // the next `Up` saves it as the draft instead of discarding it.
    let mut remote = test_remote();
    remote.app.history_push("old".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "old");
    handle_key(&mut remote, key(KeyCode::Char('!'), KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "old!");
    assert_eq!(remote.app.history_index, None, "typing detaches the walk");

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "old");
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    assert_eq!(
        remote.app.input.text(),
        "old!",
        "edited recall is kept as the draft"
    );
}

#[test]
fn noop_backspace_keeps_history_walk() {
    // A `Backspace` that changes nothing (start of buffer) must not
    // detach: the walk position survives.
    let mut remote = test_remote();
    remote.app.history_push("oldest".into());
    remote.app.history_push("newer".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "oldest");
    handle_key(&mut remote, key(KeyCode::Home, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Backspace, KeyModifiers::empty()));
    assert_eq!(remote.app.input.text(), "oldest");
    assert_eq!(
        remote.app.history_index,
        Some(1),
        "no-op edit keeps the walk"
    );
}

#[test]
fn paste_on_recalled_entry_detaches_walk() {
    // A paste that changes the buffer detaches like typing; an empty
    // paste leaves the walk alone.
    let mut remote = test_remote();
    remote.app.history_push("old".into());

    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_paste(&mut remote.app, "!");
    assert_eq!(remote.app.input.text(), "old!");
    assert_eq!(remote.app.history_index, None);

    let mut remote = test_remote();
    remote.app.history_push("old".into());
    handle_key(&mut remote, key(KeyCode::Up, KeyModifiers::empty()));
    handle_paste(&mut remote.app, "");
    assert_eq!(
        remote.app.history_index,
        Some(0),
        "empty paste keeps the walk"
    );
}

#[test]
fn remote_thinking_sets_override_without_touching_client_file() {
    // Regression: remote `/thinking` wrote the client's
    // `thinking-effort.json` (keyed `|model`) while the daemon turn used
    // its own config — a silent no-op. Now display + per-turn override
    // move together and no local file write happens here.
    let mut remote = test_remote();
    assert!(remote.options.thinking_effort.is_none());
    handle_remote_slash(&mut remote, "/thinking high");
    assert_eq!(remote.app.config.thinking_effort.as_deref(), Some("high"));
    assert_eq!(
        remote.options.thinking_effort.as_deref(),
        Some("high"),
        "next turn must carry the choice to the daemon"
    );
    handle_remote_slash(&mut remote, "/thinking clear");
    assert!(remote.app.config.thinking_effort.is_none());
    assert_eq!(
        remote.options.thinking_effort.as_deref(),
        Some(""),
        "clear is an explicit unset override, not no-override"
    );
    handle_remote_slash(&mut remote, "/thinking");
    let text: String = remote
        .app
        .transcript
        .iter()
        .filter_map(|b| match b {
            crate::ui::TranscriptBlock::Info { lines, .. } => Some(
                lines
                    .iter()
                    .flat_map(|l| l.spans.iter())
                    .map(|s| s.content.to_string())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(text.contains("unset"), "show reflects the clear: {text}");
}

#[test]
fn remote_model_switch_drops_stale_thinking_override() {
    // A level pinned for the old model must not leak onto the new one.
    let mut remote = test_remote();
    handle_remote_slash(&mut remote, "/thinking high");
    assert!(remote.options.thinking_effort.is_some());
    handle_remote_slash(&mut remote, "/model anthropic/other-model");
    assert!(
        remote.options.thinking_effort.is_none(),
        "model switch clears the old override so the daemon default applies"
    );
    assert!(remote.options.model.is_some(), "model still forwards");
}

#[test]
fn remote_model_switch_does_not_write_config_file_back() {
    // The daemon owns the real selection; the client's `/model` resolve is
    // display-only. Without this, a remote `/model <bare-id>` persisted the
    // client's view into the shared config file — silently repointing the
    // default model (e.g. `opencode/x` to a bare id under whatever provider
    // the client happened to resolve).
    use crate::test_env::TEST_SESSIONS_ENV_LOCK;
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "dex-remote-model-nofile-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _guard = crate::test_env::EnvGuard(vec![
        ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
        ("XDG_CONFIG_HOME", std::env::var_os("XDG_CONFIG_HOME")),
    ]);
    std::env::set_var("XDG_CONFIG_HOME", &dir);
    std::env::remove_var("DEX_CONFIG");

    let mut remote = test_remote();
    remote.app.config.model = "opencode/other-model".into();
    handle_remote_slash(&mut remote, "/model anthropic/other-model");
    assert!(remote.options.model.is_some(), "override still forwards");
    assert_eq!(
        remote.app.config.model, "other-model",
        "display reflects the picked id without client-side resolution"
    );
    assert!(
        remote
            .app
            .config
            .available_models
            .iter()
            .any(|c| c == "other-model"),
        "picked id stays completable in the input"
    );

    // No config file may have been created for the write-back.
    let cfg = dir.join("dex/config.yaml");
    assert!(!cfg.exists(), "remote /model wrote back the config file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remote_extension_split_and_builtin_gate() {
    assert_eq!(
        split_remote_extension("/deploy prod"),
        Some(("deploy".into(), "prod".into()))
    );
    assert_eq!(
        split_remote_extension("/deploy"),
        Some(("deploy".into(), String::new()))
    );
    assert!(split_remote_extension("/a-command!").is_none());
    assert!(split_remote_extension("plain").is_none());
    assert!(is_builtin_command("quit"));
    assert!(is_builtin_command("model"));
    assert!(!is_builtin_command("deploy"));
    // Built-ins never reach the daemon: Unknown stays unknown locally.
    let mut remote = test_remote();
    assert!(!remote_unknown_or_extension(&mut remote, "/quit x"));
}

#[test]
fn every_documented_command_is_handled_remotely() {
    // The remote entry point answers some commands itself and defers the
    // rest to the shared `handle_slash`; either way no documented command
    // may reach the local "unknown command" arm. Uses the bare command
    // name (the table's `[<m>]`-style usages are not literal invocations).
    fn info_text(remote: &RemoteApp) -> String {
        remote
            .app
            .transcript
            .iter()
            .filter_map(|b| match b {
                crate::ui::TranscriptBlock::Info { lines, .. } => Some(
                    lines
                        .iter()
                        .flat_map(|l| l.spans.iter())
                        .map(|s| s.content.to_string())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
    for spec in crate::ui::slash::COMMANDS {
        let name = spec
            .command
            .split_whitespace()
            .next()
            .unwrap_or(spec.command);
        let mut remote = test_remote();
        let quit = handle_remote_slash(&mut remote, name);
        assert_eq!(quit, name == "/quit", "{name} quit flag");
        let text = info_text(&remote);
        assert!(
            !text.contains("unknown command"),
            "{name} fell through to the local unknown-command arm: {text}"
        );
    }
}

// --- Agent mode cycling (Shift+Tab) -----------------------------------------

fn back_tab() -> crossterm::event::KeyEvent {
    key(KeyCode::BackTab, KeyModifiers::SHIFT)
}

fn shift_tab() -> crossterm::event::KeyEvent {
    key(KeyCode::Tab, KeyModifiers::SHIFT)
}

#[test]
fn back_tab_cycles_plan_manual_auto() {
    let mut remote = test_remote();
    remote.mode = AgentMode::Plan;
    remote.app.config.permission = AgentMode::Plan.permission();

    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Manual);
    assert_eq!(remote.app.config.permission, PermissionMode::Ask);
    assert_eq!(remote.options.mode.as_deref(), Some("manual"));
    assert_eq!(remote.options.permission.as_deref(), Some("ask"));

    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Auto);
    assert_eq!(remote.app.config.permission, PermissionMode::Trusted);

    // auto → plan wraps.
    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Plan);
    assert_eq!(remote.app.config.permission, PermissionMode::ReadOnly);
    assert_eq!(remote.options.permission.as_deref(), Some("read-only"));

    // A notice names the mode for two seconds.
    let (text, _) = remote.app.notice.clone().expect("a notice is pushed");
    assert_eq!(text, "mode: plan");
}

#[test]
fn shift_tab_cycles_too_for_terminals_forwarding_the_pair() {
    let mut remote = test_remote();
    remote.mode = AgentMode::Plan;
    handle_key(&mut remote, shift_tab());
    assert_eq!(remote.mode, AgentMode::Manual);
}

#[test]
fn back_tab_clamps_at_the_daemon_ceiling() {
    // The daemon allows `ask` (manual); auto must be unreachable and the
    // cycle must report why instead of silently wrapping past the boundary.
    let mut remote = test_remote();
    remote.mode = AgentMode::Manual;
    remote.ceiling = PermissionMode::Ask;
    remote.app.config.permission = PermissionMode::Ask;

    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Manual, "auto is clamped away");
    assert_eq!(remote.app.config.permission, PermissionMode::Ask);
    let (text, _) = remote
        .app
        .notice
        .clone()
        .expect("a notice explains the clamp");
    assert!(text.contains("ceiling"), "{text}");
    assert!(text.contains("ask"), "{text}");
}

#[test]
fn approval_number_keys_resolve_directly() {
    // 1/2/3 mirror the question wizard: resolve immediately without
    // moving the cursor first (1=allow once, 2=allow session, 3=deny).
    let mut remote = test_remote();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    remote
        .app
        .pending_approvals
        .push(crate::ui::PendingApproval::new(
            "bash".into(),
            "{}".into(),
            tx,
            None,
        ));
    handle_key(&mut remote, key(KeyCode::Char('2'), KeyModifiers::empty()));
    assert!(remote.app.pending_approvals.is_empty(), "resolved");
    assert_eq!(
        rx.try_recv().expect("decision sent"),
        crate::protocol::ApprovalDecision::AllowSession
    );
}

#[test]
fn finish_turn_drains_parked_approvals_and_questions() {
    // The daemon can resolve or tear down parked approvals/questions
    // without the turn tail telling us (external resolution, session
    // teardown); a finished turn must not leave their panels parked on the
    // composer slot forever. Deny/dismiss so a still-blocked daemon side
    // unwinds instead of waiting on a prompt nobody answers.
    let mut remote = test_remote();
    let (atx, mut arx) = tokio::sync::mpsc::channel(1);
    remote
        .app
        .pending_approvals
        .push(crate::ui::PendingApproval::new(
            "bash".into(),
            "{}".into(),
            atx,
            None,
        ));
    let mut qrx = park_question(&mut remote, vec![one_question()]);
    remote.app.busy = true;

    super::input::finish_turn(&mut remote, None);

    assert!(remote.app.pending_approvals.is_empty(), "approval drained");
    assert!(remote.app.pending_questions.is_empty(), "questions drained");
    assert!(!remote.app.busy);
    assert_eq!(
        arx.try_recv().ok(),
        Some(crate::protocol::ApprovalDecision::Deny),
        "unwind a still-blocked daemon side"
    );
    assert_eq!(
        qrx.try_recv().ok(),
        Some(vec![crate::protocol::QuestionAnswer::Dismiss]),
        "dismiss a still-blocked daemon side"
    );
}

#[test]
fn question_other_number_opens_text_entry() {
    // The "other" row renders as `3)` (2 options); its number opens the
    // free-text entry like Enter on it, and stays unsubmitted.
    let mut remote = test_remote();
    let mut rx = park_question(&mut remote, vec![one_question()]);
    handle_key(&mut remote, key(KeyCode::Char('3'), KeyModifiers::empty()));
    assert!(
        remote.app.pending_questions[0].text_entry.is_some(),
        "other's number opens text entry"
    );
    assert!(rx.try_recv().is_err(), "entry open, batch unsubmitted");
}

fn one_question() -> crate::protocol::Question {
    crate::protocol::Question {
        question: "Which database?".into(),
        header: "Database".into(),
        options: vec![
            crate::protocol::QuestionOption {
                label: "postgres".into(),
                description: "default".into(),
            },
            crate::protocol::QuestionOption {
                label: "sqlite".into(),
                description: "embedded".into(),
            },
        ],
        multi_select: false,
        default: None,
    }
}

fn park_question(
    remote: &mut RemoteApp,
    questions: Vec<crate::protocol::Question>,
) -> tokio::sync::mpsc::Receiver<Vec<crate::protocol::QuestionAnswer>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    remote
        .app
        .pending_questions
        .push(crate::ui::PendingQuestionUi::new(questions, None, tx));
    rx
}

#[test]
fn question_wizard_number_and_enter_record_then_advance() {
    let questions = vec![one_question(), one_question()];
    let mut remote = test_remote();
    let mut rx = park_question(&mut remote, questions);
    // Number 2 picks sqlite; the wizard advances to question 2, never
    // submitting the batch.
    handle_key(&mut remote, key(KeyCode::Char('2'), KeyModifiers::empty()));
    assert_eq!(remote.app.pending_questions.len(), 1);
    let front = &remote.app.pending_questions[0];
    assert_eq!(front.current, 1, "advanced to the second question");
    assert_eq!(
        front.answers[0],
        Some(crate::protocol::QuestionAnswer::Choice(1))
    );
    assert!(
        rx.try_recv().is_err(),
        "recording an answer must not submit the batch"
    );
    // Enter on question 2 records it (selected row 0) and shows the
    // summary; Enter there submits the whole batch.
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(remote.app.pending_questions[0].summary, "summary shows");
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(remote.app.pending_questions.is_empty(), "submitted");
    assert_eq!(
        rx.try_recv().expect("batch sent"),
        vec![
            crate::protocol::QuestionAnswer::Choice(1),
            crate::protocol::QuestionAnswer::Choice(0),
        ]
    );
}

#[test]
fn question_wizard_multi_toggle_submit_and_esc_backout() {
    let mut q = one_question();
    q.multi_select = true;
    q.options.push(crate::protocol::QuestionOption {
        label: "cache".into(),
        description: String::new(),
    });
    let mut remote = test_remote();
    let mut rx = park_question(&mut remote, vec![q, one_question()]);
    // Toggle option 1 twice via Space: net off. Then toggle via Enter.
    handle_key(&mut remote, key(KeyCode::Char(' '), KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Char(' '), KeyModifiers::empty()));
    assert!(!remote.app.pending_questions[0].toggled[0]);
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(remote.app.pending_questions[0].toggled[0]);
    // Move to the Submit row: 3 options + Other + Submit → index 4.
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    let front = &remote.app.pending_questions[0];
    assert_eq!(
        front.answers[0],
        Some(crate::protocol::QuestionAnswer::Multi(vec![0])),
        "submit collects the toggles"
    );
    // Esc backs out one question: from the second back to the first; the
    // backed-out (second) question stays unanswered.
    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    let front = &remote.app.pending_questions[0];
    assert_eq!(front.current, 0);
    assert_eq!(
        front.answers[1], None,
        "backed-out question stays unanswered"
    );
    // Re-answer the first via its Submit row (backing out cleared the
    // toggles, so it submits as Dismiss), record the second, and submit
    // from the summary.
    for _ in 0..4 {
        handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    }
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(
        rx.try_recv().expect("batch sent"),
        vec![
            crate::protocol::QuestionAnswer::Dismiss,
            crate::protocol::QuestionAnswer::Choice(0)
        ]
    );
}

#[test]
fn question_wizard_esc_on_first_dismisses_and_other_takes_text() {
    let mut remote = test_remote();
    let mut rx = park_question(&mut remote, vec![one_question()]);
    // Select the Other row (index = options.len() = 2) and press Enter:
    // free-text entry opens, Esc cancels it, Enter records the text.
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Down, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(remote.app.pending_questions[0].text_entry.is_some());
    handle_key(&mut remote, key(KeyCode::Char('s'), KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Char('q'), KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(remote.app.pending_questions[0].summary, "summary shows");
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert!(
        remote.app.pending_questions.is_empty(),
        "single question submitted"
    );
    assert_eq!(
        rx.try_recv().expect("batch sent"),
        vec![crate::protocol::QuestionAnswer::Text("sq".into())]
    );

    // Esc on the first question dismisses it and advances (never strands).
    let mut remote = test_remote();
    let mut rx = park_question(&mut remote, vec![one_question(), one_question()]);
    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    let front = &remote.app.pending_questions[0];
    assert_eq!(front.current, 1);
    assert_eq!(
        front.answers[0],
        Some(crate::protocol::QuestionAnswer::Dismiss)
    );
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    handle_key(&mut remote, key(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(
        rx.try_recv().expect("batch sent"),
        vec![
            crate::protocol::QuestionAnswer::Dismiss,
            crate::protocol::QuestionAnswer::Choice(0)
        ]
    );
}

#[test]
fn back_tab_is_inert_while_an_approval_is_parked() {
    let mut remote = test_remote();
    remote.mode = AgentMode::Manual;
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    remote
        .app
        .pending_approvals
        .push(crate::ui::PendingApproval::new(
            "bash".into(),
            "{}".into(),
            tx,
            None,
        ));
    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Manual, "approval owns the keys");
}

#[test]
fn back_tab_works_with_the_slash_popup_open() {
    // It is global chrome like Alt+V: with the popup open it still cycles
    // and does not dismiss the draft.
    let mut remote = test_remote();
    remote.mode = AgentMode::Plan;
    remote.app.config.permission = PermissionMode::ReadOnly;
    remote.app.input.insert_paste("/mod");
    handle_key(&mut remote, back_tab());
    assert_eq!(remote.mode, AgentMode::Manual);
    assert_eq!(remote.app.input.text(), "/mod", "the draft survives");
}

#[test]
fn plain_tab_still_completes_in_the_slash_popup() {
    // Only Shift+Tab is the mode cycle; a bare Tab must keep completing the
    // highlighted slash suggestion.
    let mut remote = test_remote();
    remote.mode = AgentMode::Plan;
    remote.app.config.permission = PermissionMode::ReadOnly;
    remote.app.input.insert_paste("/cle");
    assert!(crate::ui::slash::popup_open(&remote.app));
    handle_key(&mut remote, key(KeyCode::Tab, KeyModifiers::empty()));
    assert_eq!(remote.mode, AgentMode::Plan, "Tab must not cycle the mode");
    assert_eq!(remote.app.input.text(), "/clear ");
}

#[test]
fn agent_lines_buffer_into_child_log_and_leave_parent_alone() {
    let mut remote = test_remote();
    let before = remote.app.transcript.len();
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::AgentLine {
            agent_id: "a1".into(),
            name: "explorer".into(),
            event: Box::new(crate::protocol::StreamEvent::AssistantText(
                "hello from child".into(),
            )),
        },
    );
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::AgentLine {
            agent_id: "a1".into(),
            name: "explorer".into(),
            event: Box::new(crate::protocol::StreamEvent::ToolResult {
                name: "bash".into(),
                summary: "ls".into(),
                success: true,
                preview: vec![],
                duration: 0.0,
                id: "t1".into(),
            }),
        },
    );
    assert_eq!(remote.app.transcript.len(), before, "parent untouched");
    assert_eq!(remote.app.child_logs.len(), 1);
    let log = &remote.app.child_logs[0];
    assert_eq!(log.name, "explorer");
    assert_eq!(log.app.transcript.len(), 2, "assistant + tool blocks");
    assert!(
        matches!(
            log.app.transcript[0],
            crate::ui::TranscriptBlock::Assistant { .. }
        ),
        "same block semantics as the parent transcript"
    );
}

#[test]
fn task_events_drive_chips_and_capped_logs() {
    let mut remote = test_remote();
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::TaskStarted {
            id: "task-1".into(),
            command: "npm run dev".into(),
        },
    );
    assert_eq!(remote.app.tasks.len(), 1);
    assert!(!remote.app.tasks[0].done);
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::TaskOutput {
            id: "task-1".into(),
            chunk: "listening\non :3000".into(),
        },
    );
    assert_eq!(remote.app.task_logs.len(), 1);
    assert_eq!(remote.app.task_logs[0].lines, vec!["listening", "on :3000"]);
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::TaskFinished {
            id: "task-1".into(),
            status: "exit 0".into(),
            exit_code: Some(0),
            duration: 1.0,
        },
    );
    assert!(remote.app.tasks[0].done);
    assert_eq!(remote.app.recent_done.len(), 1);
    let strip: String = crate::ui::status::activity_line(&remote.app, 80)
        .expect("a just-finished task shows in the strip")
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(strip.contains("✓ task-1"), "{strip}");
}

#[test]
fn child_view_toggles_and_cycles_with_keys() {
    let mut remote = test_remote();
    for id in ["a1", "a2"] {
        input::handle_stream_event(
            &mut remote,
            crate::protocol::StreamEvent::AgentLine {
                agent_id: id.into(),
                name: "explorer".into(),
                event: Box::new(crate::protocol::StreamEvent::System("working".into())),
            },
        );
    }
    assert!(remote.app.child_view.is_none());
    // Ctrl+A opens the first log.
    handle_key(&mut remote, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.child_view.as_deref(), Some("a1"));
    // Ctrl+A cycles to the second; Esc closes; Ctrl+A opens again.
    handle_key(&mut remote, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.child_view.as_deref(), Some("a2"));
    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(remote.app.child_view.is_none());
    handle_key(&mut remote, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.child_view.as_deref(), Some("a1"));
    // Scrolling drives the child log's own scroll state, not the parent's.
    handle_key(&mut remote, key(KeyCode::PageUp, KeyModifiers::empty()));
    assert!(!remote.app.child_logs[0].app.autoscroll);
    assert_eq!(remote.app.scroll, 0);
}

#[test]
fn task_view_opens_cycles_and_scrolls_with_keys() {
    let mut remote = test_remote();
    handle_key(&mut remote, key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(remote.app.task_view.is_none());
    assert!(remote.app.notice.is_some(), "no tasks: a notice explains");
    for id in ["task-1", "task-2"] {
        input::handle_stream_event(
            &mut remote,
            crate::protocol::StreamEvent::TaskStarted {
                id: id.into(),
                command: "make".into(),
            },
        );
    }
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::TaskOutput {
            id: "task-1".into(),
            chunk: "\x1b[31merror\x1b[0m: boom".into(),
        },
    );
    assert_eq!(remote.app.task_logs[0].lines, vec!["error: boom"]);
    input::handle_stream_event(
        &mut remote,
        crate::protocol::StreamEvent::TaskFinished {
            id: "task-2".into(),
            status: "exit 0".into(),
            exit_code: Some(0),
            duration: 1.0,
        },
    );
    // Ctrl+B prefers the newest running task over a newer finished one.
    handle_key(&mut remote, key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.task_view.as_deref(), Some("task-1"));
    // Scroll keys move the task log, not the parent transcript.
    handle_key(&mut remote, key(KeyCode::PageUp, KeyModifiers::empty()));
    assert_eq!(remote.app.task_scroll, 20);
    assert_eq!(remote.app.scroll, 0);
    // Ctrl+B cycles, then closes after the last log.
    handle_key(&mut remote, key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.task_view.as_deref(), Some("task-2"));
    assert_eq!(remote.app.task_scroll, 0);
    handle_key(&mut remote, key(KeyCode::Char('b'), KeyModifiers::CONTROL));
    assert!(remote.app.task_view.is_none());
    // `/tasks <id>` opens a specific log; Esc closes it.
    crate::ui::slash::handle_slash(&mut remote.app, "/tasks task-2");
    assert_eq!(remote.app.task_view.as_deref(), Some("task-2"));
    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(remote.app.task_view.is_none());
}

fn child_line(id: &str) -> crate::protocol::StreamEvent {
    crate::protocol::StreamEvent::AgentLine {
        agent_id: id.into(),
        name: "explorer".into(),
        event: Box::new(crate::protocol::StreamEvent::System("working".into())),
    }
}

#[test]
fn esc_closes_child_view_without_denying_a_parked_approval() {
    // Regression: the approval handler ran before the child-view modal, so
    // with an approval pending and the view open, Esc denied the approval
    // instead of closing the view (and y/n/s resolved it invisibly). Now
    // the overlay draws over the view and Esc only closes it.
    let mut remote = test_remote();
    input::handle_stream_event(&mut remote, child_line("a1"));
    handle_key(&mut remote, key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert_eq!(remote.app.child_view.as_deref(), Some("a1"));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    remote
        .app
        .pending_approvals
        .push(crate::ui::PendingApproval::new(
            "bash".into(),
            "{}".into(),
            tx,
            None,
        ));

    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(remote.app.child_view.is_none(), "Esc closes the view");
    assert_eq!(remote.app.pending_approvals.len(), 1, "approval survives");

    // And with the view closed, Esc denies again as before.
    handle_key(&mut remote, key(KeyCode::Esc, KeyModifiers::empty()));
    assert!(remote.app.pending_approvals.is_empty());
    assert_eq!(
        rx.try_recv().expect("decision sent"),
        crate::protocol::ApprovalDecision::Deny
    );
}

#[test]
fn child_log_count_is_capped_and_view_closes_on_dropped_log() {
    // Each log is a scratch `App` with caches, so a many-spawn session
    // must not accumulate them unbounded: past the cap the oldest log
    // drops, and an open view onto it closes instead of dangling.
    let mut remote = test_remote();
    for i in 0..crate::ui::CHILD_LOG_MAX_LOGS {
        input::handle_stream_event(&mut remote, child_line(&format!("a{i}")));
    }
    assert_eq!(remote.app.child_logs.len(), crate::ui::CHILD_LOG_MAX_LOGS);
    remote.app.child_view = Some("a0".into());
    input::handle_stream_event(&mut remote, child_line("new"));
    assert_eq!(remote.app.child_logs.len(), crate::ui::CHILD_LOG_MAX_LOGS);
    assert_eq!(remote.app.child_logs[0].id, "a1", "oldest dropped");
    assert!(remote.app.child_view.is_none(), "view closed");
}
