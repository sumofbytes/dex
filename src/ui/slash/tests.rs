//! Tests, split out of the module body so it stays implementation.

use super::*;
use crate::protocol::Skill;

fn new_app() -> App {
    App::test_app()
}

/// Info lines pushed by slash handlers (`push_info` appends `Info`
/// transcript blocks; grab their plain text for assertions).
fn info_texts(app: &App) -> Vec<String> {
    app.transcript
        .iter()
        .filter_map(|b| match b {
            crate::ui::TranscriptBlock::Info { line, .. } => Some(
                line.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect()
}

fn has_info(app: &App, needle: &str) -> bool {
    info_texts(app).iter().any(|t| t.contains(needle))
}

fn type_input(app: &mut App, text: &str) {
    app.input = InputField::from_text(text);
    app.slash_selected = 0;
}

#[test]
fn suggestions_cache_serves_repeated_renders() {
    // The popup renders per frame; recomputation happens per input
    // change (plus model/skill/session data changes), not per frame.
    let mut app = new_app();
    type_input(&mut app, "/model ");
    let first = slash_suggestions(&app);
    assert_eq!(first.len(), 1, "test config serves one model");
    assert!(
        app.slash_cache.borrow().is_some(),
        "popup memoizes per input"
    );
    // Same input: served from the cache, same listing.
    assert_eq!(slash_suggestions(&app), first);
    // Narrower query recomputes (and can only shrink the listing).
    type_input(&mut app, "/model t");
    assert_eq!(slash_suggestions(&app), first);
    type_input(&mut app, "/model z");
    assert!(slash_suggestions(&app).is_empty());
    // A model switch invalidates: the key tracks the current model.
    app.config.model = "other".to_string();
    type_input(&mut app, "/model ");
    let _ = slash_suggestions(&app);
    assert_eq!(
        app.slash_cache.borrow().as_ref().unwrap().key.model,
        "other"
    );
}

#[test]
fn suggestions_only_for_bare_slash_prefixes() {
    let mut app = new_app();
    type_input(&mut app, "");
    assert!(slash_suggestions(&app).is_empty(), "no popup without input");
    type_input(&mut app, "hello");
    assert!(
        slash_suggestions(&app).is_empty(),
        "no popup for plain text"
    );

    type_input(&mut app, "a\nb");
    assert!(
        slash_suggestions(&app).is_empty(),
        "no popup across a newline"
    );

    app.busy = true;
    type_input(&mut app, "/cle");
    assert!(
        slash_suggestions(&app).is_empty(),
        "no popup while a turn runs"
    );
    app.busy = false;

    type_input(&mut app, "/cle");
    let got = slash_suggestions(&app);
    assert_eq!(
        got,
        vec![(
            "/clear".to_string(),
            "Clear conversation history".to_string()
        )]
    );

    type_input(&mut app, "/mod");
    let got = slash_suggestions(&app);
    // `/mode` sorts before `/model` in COMMANDS, so `/mod` offers both.
    assert_eq!(got[0].0, "/mode");
    assert!(got.iter().any(|(cmd, _)| cmd == "/model"), "{got:?}");
    // Skills are suggested as /skill:<name> entries.
    app.skills.push(Skill {
        name: "demo".into(),
        description: "d".into(),
        path: "/tmp/demo/SKILL.md".into(),
    });
    type_input(&mut app, "/sk");
    let got = slash_suggestions(&app);
    assert_eq!(got[0].0, "/skill:demo");

    // Argument form without a known picker -> nothing.
    type_input(&mut app, "/nope arg");
    assert!(slash_suggestions(&app).is_empty());
}

#[test]
fn no_popup_while_walking_history() {
    // Up/Down must keep walking history when the composer holds a line
    // recalled from it: a "/clear" entry must not resurrect the popup
    // and trap the walk (the remote key arms fall through when it's
    // closed, so Up/Down reach the generic history handling).
    let mut app = new_app();
    type_input(&mut app, "/clear");
    assert!(popup_open(&app), "popup while freshly typing");

    app.history_index = Some(0);
    assert!(
        !popup_open(&app),
        "recalled slash line must not open the popup"
    );

    // Even a fresh command draft stays popup-free mid-walk; resuming
    // normal typing (walk closed) gets the popup back.
    type_input(&mut app, "/mo");
    assert!(slash_suggestions(&app).is_empty(), "no popup mid-walk");
    app.history_index = None;
    assert!(popup_open(&app), "fresh typing gets the popup back");
}

#[test]
fn model_and_provider_pickers_filter_and_mark_current() {
    let mut app = new_app();
    app.config.available_models = vec!["test".into(), "prov/other".into()];
    type_input(&mut app, "/model te");
    let got = slash_suggestions(&app);
    // "test" matches by prefix and is the current model; "prov/other"
    // matches by its tail ("other" does not start with "te" -> filtered).
    assert_eq!(
        got,
        vec![("/model test".to_string(), "Current model".to_string())]
    );

    type_input(&mut app, "/model other");
    let got = slash_suggestions(&app);
    assert_eq!(
        got,
        vec![("/model prov/other".to_string(), "prov".to_string())],
        "tail match shows the provider as the row hint"
    );

    type_input(&mut app, "/provider op");
    let got = slash_suggestions(&app);
    assert_eq!(
        got,
        vec![
            (
                "/provider opencode".to_string(),
                "Current provider".to_string()
            ),
            ("/provider openai-codex".to_string(), String::new()),
        ]
    );

    type_input(&mut app, "/provider zzz");
    assert!(slash_suggestions(&app).is_empty());
}

#[test]
fn resume_picker_lists_other_sessions() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let data_dir = std::env::temp_dir().join(format!("dex-slash-resume-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
            .into_iter()
            .collect();
    let _env = crate::session::EnvGuard(saved);
    std::env::set_var("XDG_DATA_HOME", &data_dir);
    // The session file stays on disk after the handle drops — `Session`
    // has no `Drop` impl and appends are synchronous writes.
    Session::new("/tmp".to_string(), Some("other".to_string())).unwrap();

    let mut app = new_app();
    type_input(&mut app, "/resume");
    let got = slash_suggestions(&app);
    assert_eq!(got.len(), 1, "the current session must be hidden");
    assert_eq!(got[0].0, "/resume 0");
    assert!(got[0].1.contains("other"), "row shows the session name");

    // Query filters by name.
    type_input(&mut app, "/resume oth");
    assert_eq!(slash_suggestions(&app).len(), 1);
    type_input(&mut app, "/resume zzz");
    assert!(slash_suggestions(&app).is_empty());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn picker_rows_strip_the_command_prefix() {
    assert_eq!(suggestion_label("/model te", "/model test"), "test");
    assert_eq!(
        suggestion_label("/provider o", "/provider opencode"),
        "opencode"
    );
    assert_eq!(suggestion_label("/resume", "/resume 0"), "0");
    assert_eq!(
        suggestion_label("/cle", "/clear"),
        "/clear",
        "outside a picker the command itself is the label"
    );
}

#[test]
fn complete_slash_expands_the_selected_suggestion() {
    let mut app = new_app();
    type_input(&mut app, "/cle");
    assert!(complete_slash(&mut app));
    assert_eq!(app.input.text(), "/clear ");
    assert_eq!(app.slash_selected, 0);

    // Already-complete text is left alone (no stacked spaces).
    type_input(&mut app, "/clear ");
    app.slash_selected = 0;
    assert!(!complete_slash(&mut app));
    assert_eq!(app.input.text(), "/clear ");

    // Nothing to complete (no suggestions).
    type_input(&mut app, "plain text");
    assert!(!complete_slash(&mut app));
}

#[test]
fn dismiss_slash_clears_the_draft_only_when_open() {
    let mut app = new_app();
    type_input(&mut app, "plain");
    assert!(!dismiss_slash(&mut app));
    assert_eq!(app.input.text(), "plain", "nothing cleared when closed");

    type_input(&mut app, "/cle");
    assert!(dismiss_slash(&mut app));
    assert_eq!(app.input.text(), "", "the draft is what closes the popup");
    assert_eq!(app.slash_selected, 0);
}

#[test]
fn bare_picker_commands_expand_on_enter() {
    let mut app = new_app();
    for cmd in EXPAND_ON_ENTER {
        type_input(&mut app, cmd);
        assert!(expand_bare_command(&mut app), "{cmd} expands");
        assert_eq!(app.input.text(), format!("{cmd} "));
        assert_eq!(app.slash_selected, 0);
    }
    // Already has an argument / newline / non-picker -> untouched.
    type_input(&mut app, "/model gpt");
    assert!(!expand_bare_command(&mut app));
    assert_eq!(app.input.text(), "/model gpt");
    type_input(&mut app, "/resume 0");
    assert!(!expand_bare_command(&mut app));
    type_input(&mut app, "/clear");
    assert!(!expand_bare_command(&mut app));
    type_input(&mut app, "a\nb");
    assert!(!expand_bare_command(&mut app));
}

#[test]
fn reset_session_state_clears_conversation_scope_only() {
    let mut app = new_app();
    app.messages.push(ChatMessage::user("m1"));
    app.messages.push(ChatMessage::user("m2"));
    app.pending_steering.push("s".into());
    app.pending_followups.push("f".into());
    app.cancel_requested = true;
    app.cancel_presses = 1;
    app.tool_state.total_usage = 99;
    app.tool_state.total_cost = 1.5;
    app.tool_state.verify_dirty = true;
    app.plan = crate::protocol::Plan {
        goal: Some("g".into()),
        steps: vec![("step".into(), false)],
        ..Default::default()
    };
    app.messages.clear();
    app.messages.push(ChatMessage::system("sys"));
    app.messages.push(ChatMessage::user("u"));
    // A pending approval is denied on reset so the agent loop unblocks.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    app.pending_approvals.push(crate::ui::PendingApproval::new(
        "bash".into(),
        "{}".into(),
        tx,
        None,
    ));

    reset_session_state(&mut app);

    // The system prompt (first message) survives; the rest is dropped.
    assert_eq!(app.messages.len(), 1);
    assert!(app.messages[0].content_str().contains("sys"));
    assert!(app.pending_steering.is_empty() && app.pending_followups.is_empty());
    assert!(!app.cancel_requested && app.cancel_presses == 0);
    assert_eq!(app.tool_state.total_usage, 0);
    assert_eq!(app.tool_state.total_cost, 0.0);
    assert!(!app.tool_state.verify_dirty);
    assert!(app.plan.is_empty());
    assert!(app.transcript.is_empty());
    assert!(app.pending_approvals.is_empty());
    assert_eq!(
        rx.try_recv().ok(),
        Some(crate::protocol::ApprovalDecision::Deny),
        "pending approvals are denied, not dropped silently"
    );
}

#[test]
fn handle_slash_basic_commands() {
    // /quit returns true (quit request), everything else false.
    let mut app = new_app();
    assert!(handle_slash(&mut app, "/quit"));

    let mut app = new_app();
    app.messages.push(ChatMessage::system("sys"));
    assert!(!handle_slash(&mut app, "/session"));
    assert!(
        has_info(&app, "session: "),
        "{}",
        info_texts(&app).join(" | ")
    );
    assert!(has_info(&app, "turns: 0"));

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/permissions"));
    assert!(has_info(&app, "permissions is now /mode"));
    assert!(has_info(&app, "mode: auto"));
    assert!(has_info(&app, "permission: trusted (derived from mode)"));

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/no-such-command"));
    assert!(has_info(&app, "unknown command: /no-such-command"));
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/help"));
    assert!(has_info(&app, "commands: /quit"));
    assert!(has_info(&app, "prefix: !<command>"));
}

#[test]
fn handle_slash_mode_shows_and_sets() {
    // Bare: report the mode and its derived permission (the default
    // `trusted` maps to `auto`).
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/mode"));
    assert!(has_info(&app, "mode: auto"));
    assert!(has_info(&app, "permission: trusted (derived from mode)"));

    // An argument switches the mode, which drives `config.permission`.
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/mode plan"));
    assert!(has_info(&app, "mode: plan (permission: read-only)"));
    assert_eq!(
        app.config.permission,
        crate::protocol::PermissionMode::ReadOnly
    );

    // An unknown mode reports the parser error and changes nothing.
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/mode nope"));
    assert!(has_info(&app, "invalid mode 'nope'"));
    assert_eq!(
        app.config.permission,
        crate::protocol::PermissionMode::Trusted
    );
}

#[test]
fn documented_commands_parse_and_share_one_help_list() {
    // Every documented command must be recognized by the shared parser: a
    // phantom row (documented but unimplemented, like the old `/goal`)
    // classifies as `Unknown` and fails here.
    for spec in COMMANDS {
        assert_ne!(
            parse(spec.command),
            SlashCommand::Unknown,
            "{} is documented but not parsed",
            spec.command
        );
    }

    // Both entry points render the command list from the one table, so the
    // local and remote help text can no longer drift.
    let expected = commands_help_line();
    assert!(expected.starts_with("commands: /quit"), "{expected}");
    for spec in COMMANDS {
        if !spec.usage.is_empty() {
            assert!(
                expected.contains(spec.usage),
                "help omits {}: {expected}",
                spec.usage
            );
        }
    }

    let mut local = new_app();
    handle_slash(&mut local, "/help");
    assert!(
        info_texts(&local).iter().any(|t| t.contains(&expected)),
        "local help: {}",
        info_texts(&local).join(" | ")
    );

    let mut remote = new_app();
    cmd_help(&mut remote, true);
    assert!(
        info_texts(&remote).iter().any(|t| t.contains(&expected)),
        "remote help: {}",
        info_texts(&remote).join(" | ")
    );
}

#[test]
fn handle_slash_clear_refuses_while_busy_and_clears_when_idle() {
    let mut app = new_app();
    app.messages.push(ChatMessage::system("sys"));
    app.messages.push(ChatMessage::user("keep-me-out"));
    app.busy = true;
    assert!(!handle_slash(&mut app, "/clear"));
    assert!(has_info(&app, "cannot clear while a turn is running"));
    assert_eq!(app.messages.len(), 2, "busy /clear changes nothing");

    app.busy = false;
    assert!(!handle_slash(&mut app, "/clear"));
    assert!(has_info(&app, "history cleared"));
    assert_eq!(app.messages.len(), 1, "the system prompt survives");
}

#[test]
fn handle_slash_mcp_variants() {
    // Pin the process-global manager (`OnceLock`, never unset): without
    // this the snapshot races whichever parallel test initializes the
    // manager first. A concurrent refresh can still win a `try_read`
    // against the snapshot, so the expectation is derived from the same
    // global before/after the handler and asserted only while both
    // observations agree (a flip means the outcome was genuinely
    // ambiguous — skip rather than guess).
    crate::mcp::global_manager();
    let mut app = new_app();
    let before = crate::mcp::cached_statuses().is_some();
    assert!(!handle_slash(&mut app, "/mcp"));
    let after = crate::mcp::cached_statuses().is_some();
    let texts = info_texts(&app).join(" | ");
    if before && after {
        assert!(
            !texts.contains("MCP status unavailable"),
            "initialized manager renders the panel: {texts}"
        );
    } else if !before && !after {
        assert!(
            texts.contains("MCP status unavailable"),
            "no manager, no panel: {texts}"
        );
    }

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/mcp help"));
    assert!(has_info(&app, "dex mcp login"));

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/mcp bogus"));
    assert!(has_info(&app, "usage: /mcp [help]"));
}

#[test]
fn handle_slash_name_sets_session_name() {
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/name  Fancy  "));
    assert!(has_info(&app, "session name: Fancy"));
    assert_eq!(app.session.name(), Some("Fancy"));

    // Blank name is ignored (no info, no rename).
    let mut app = new_app();
    let before = info_texts(&app).len();
    assert!(!handle_slash(&mut app, "/name   "));
    assert_eq!(info_texts(&app).len(), before);
}

#[test]
fn handle_slash_skill_load_and_miss() {
    let root = std::env::temp_dir().join(format!("dex-slash-skill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("demo")).unwrap();
    std::fs::write(root.join("demo/SKILL.md"), "body").unwrap();

    let mut app = new_app();
    app.skills.push(Skill {
        name: "demo".into(),
        description: "d".into(),
        path: root.join("demo/SKILL.md"),
    });
    assert!(!handle_slash(&mut app, "/skill:demo"));
    assert!(has_info(&app, "loaded skill: demo"));
    assert_eq!(
        app.messages.last().unwrap().name.as_deref(),
        Some("skill"),
        "the skill lands as a named user message"
    );
    assert!(app.messages.last().unwrap().content_str().contains("body"));

    let mut app = new_app();
    app.skills.push(Skill {
        name: "demo".into(),
        description: "d".into(),
        path: root.join("demo/SKILL.md"),
    });
    assert!(!handle_slash(&mut app, "/skill:missing"));
    assert!(has_info(&app, "skill not found: missing"));
    assert!(has_info(&app, "available skills:"));
    assert!(has_info(&app, "  - demo"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn handle_slash_thinking_show_set_and_clear() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let cache = std::env::temp_dir().join(format!("dex-slash-think-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache);
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
        (
            "DEX_THINKING_EFFORT",
            std::env::var_os("DEX_THINKING_EFFORT"),
        ),
        ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
    ]
    .into_iter()
    .collect();
    let _env = crate::session::EnvGuard(saved);
    std::env::set_var("XDG_CACHE_HOME", &cache);
    std::env::remove_var("DEX_THINKING_EFFORT");
    // No user config: a file `thinking_effort:` must not leak into the
    // refresh after `/thinking clear`.
    std::env::set_var("DEX_CONFIG", cache.join("no-such-config.yaml"));
    // A minimal models.dev cache entry advertising effort options, so the
    // validation branches are reachable without the network.
    std::fs::create_dir_all(cache.join("dex")).unwrap();
    std::fs::write(
            cache.join("dex/models.dev.json"),
            r#"{"openai":{"models":{"fake-reasoner":{"reasoning_options":[{"values":["low","high"]}]}}}}"#,
        )
        .unwrap();

    let mut app = new_app();
    app.config.model = "fake-reasoner".into();
    assert!(!handle_slash(&mut app, "/thinking"));
    assert!(has_info(
        &app,
        "thinking effort: unset (options: low, high)"
    ));

    // A level the model does not advertise is rejected with the options.
    let mut app = new_app();
    app.config.model = "fake-reasoner".into();
    assert!(!handle_slash(&mut app, "/thinking bogus"));
    assert!(
        has_info(
            &app,
            "unknown thinking effort 'bogus' for fake-reasoner (options: low, high)"
        ),
        "{}",
        info_texts(&app).join(" | ")
    );
    assert!(app.config.thinking_effort.is_none());

    // A valid level is remembered and shown for the model.
    let mut app = new_app();
    app.config.model = "fake-reasoner".into();
    assert!(!handle_slash(&mut app, "/thinking high"));
    assert!(
        has_info(&app, "thinking effort: high for fake-reasoner"),
        "{}",
        info_texts(&app).join(" | ")
    );
    assert_eq!(app.config.thinking_effort.as_deref(), Some("high"));

    // A blank argument prints usage.
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/thinking  "));
    assert!(has_info(&app, "usage: /thinking <level>|clear"));

    // "clear" resets to unset even when an effort was set.
    let mut app = new_app();
    app.config.model = "fake-reasoner".into();
    app.config.thinking_effort = Some("high".into());
    assert!(!handle_slash(&mut app, "/thinking clear"));
    assert!(
        has_info(&app, "thinking effort cleared"),
        "{}",
        info_texts(&app).join(" | ")
    );
    assert!(app.config.thinking_effort.is_none());
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn handle_slash_waive_records_reason() {
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/waive   "));
    assert!(has_info(&app, "usage: /waive <reason>"));

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/waive flaky env"));
    assert!(has_info(&app, "verification waived"));
    assert_eq!(
        app.messages.last().unwrap().name.as_deref(),
        Some("waive"),
        "the reason lands as a named user message"
    );
    assert!(app
        .messages
        .last()
        .unwrap()
        .content_str()
        .contains("[verify waived] flaky env"));
}

#[test]
fn handle_slash_undo_reports_in_memory_sessions() {
    // in-memory sessions have no change ledger: the error surfaces.
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/undo"));
    assert!(has_info(&app, "undo: "));
}

#[test]
fn handle_slash_provider_known_and_unknown() {
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/provider nope"));
    assert!(has_info(&app, "unknown provider: nope"));

    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/provider opencode"));
    assert!(has_info(&app, "provider already selected: opencode"));
}

#[test]
fn handle_slash_model_switch_and_persist_is_hermetic() {
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-slash-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
        ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
        ("DEX_CONTEXT_WINDOW", std::env::var_os("DEX_CONTEXT_WINDOW")),
    ]
    .into_iter()
    .collect();
    let _env = crate::session::EnvGuard(saved);
    std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    std::env::remove_var("DEX_CONTEXT_WINDOW");

    let mut app = new_app();
    // A persisted session file so `set_state("model", ...)` has a home.
    let session_path = dir.join("model-test.jsonl");
    std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"model-test\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
    app.session = Session::from_path(&session_path).unwrap();
    assert!(!handle_slash(&mut app, "/model beta-model"));
    assert!(
        has_info(&app, "switched to model: beta-model"),
        "{}",
        info_texts(&app).join(" | ")
    );
    assert_eq!(app.config.model, "beta-model");
    assert!(
        app.config
            .available_models
            .contains(&"beta-model".to_string()),
        "the picked model joins the catalog list"
    );
    assert_eq!(
        crate::session::load_session_state(&session_path)
            .unwrap()
            .get("model"),
        Some(&"beta-model".to_string()),
        "/model persists the choice into session state"
    );

    // A blank argument changes nothing.
    let mut app = new_app();
    assert!(!handle_slash(&mut app, "/model  "));
    assert_eq!(app.config.model, "test");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn apply_session_state_restores_plan_and_reports_model_errors() {
    let mut app = new_app();
    // No session path -> nothing happens.
    apply_session_state(&mut app, None);

    let dir = std::env::temp_dir().join(format!("dex-slash-state-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state-test.jsonl");
    std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"state-test\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
    {
        let mut session = Session::from_path(&path).unwrap();
        session
            .set_state(
                "plan",
                &crate::protocol::Plan {
                    goal: Some("build it".into()),
                    steps: vec![("one".into(), true), ("two".into(), false)],
                    ..Default::default()
                }
                .to_json(),
            )
            .unwrap();
        session.set_state("model", "not-a-model").unwrap();
    }

    apply_session_state(&mut app, Some(&path));
    let texts = info_texts(&app).join(" | ");
    assert!(texts.contains("restored plan: 1 / 2 steps"), "{texts}");
    assert!(texts.contains("restored goal: build it"), "{texts}");
    assert_eq!(app.plan.steps.len(), 2);
    // apply_model does not validate against the catalog: the stored id is
    // adopted as-is and reported.
    assert!(texts.contains("restored model: not-a-model"), "{texts}");
    assert_eq!(app.config.model, "not-a-model");
    let _ = std::fs::remove_dir_all(&dir);
}
