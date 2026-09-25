//! Tests, split out of the module body so it stays implementation.

use super::*;
use crate::agent::state::GlobalCancellation;

#[test]
fn delegation_is_one_tool_with_four_actions() {
    assert!(is_delegation(DELEGATION_TOOL));
    assert_eq!(DELEGATION_ACTIONS, ["spawn", "wait", "stop", "list"]);
    assert!(!is_delegation("read"));
    assert!(!is_delegation("mcp__x__y"));
}

#[tokio::test]
async fn delegation_without_a_daemon_context_rejects_cleanly() {
    // OneShot / `dex run` shape: no manager anywhere to spawn into.
    let policy = Policy::trusted();
    let mut args = Map::new();
    for action in DELEGATION_ACTIONS {
        args.insert("action".into(), json!(action));
        let error = execute_delegation(DELEGATION_TOOL, &args, &GlobalCancellation, &policy)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("daemon-backed"),
            "{action}: {error}"
        );
    }
}

#[tokio::test]
async fn children_cannot_delegate_at_the_filter() {
    // §11 at-cap rule: a child filter without delegation tools rejects
    // before any delegation logic runs (the at-cap child shape).
    let filter = ToolFilter::new("explorer", ["read", "grep", "find"]);
    let policy = Policy::trusted();
    let mut args = Map::new();
    args.insert("agent".into(), json!("explorer"));
    args.insert("task".into(), json!("look around"));
    let error = crate::tools::execute(
        "delegate",
        &args,
        &GlobalCancellation,
        &policy,
        Some(&filter),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not in explorer's tool allowlist"),
        "{error}"
    );
}

#[test]
fn seed_task_text_includes_hints_and_parent_summary() {
    let seed = ContextSeed {
        task: "check the build".to_string(),
        file_hints: vec![PathBuf::from("src/main.rs")],
        parent_summary: Some("the diff touches the gate".to_string()),
    };
    let text = seed_task_text(&seed);
    assert!(text.starts_with("check the build"));
    assert!(text.contains("File hints: src/main.rs"));
    assert!(text.contains("Parent context:"));
    assert!(text.contains("the diff touches"));
    let bare = ContextSeed {
        task: "t".to_string(),
        file_hints: Vec::new(),
        parent_summary: None,
    };
    assert_eq!(seed_task_text(&bare), "t");
}

#[test]
fn child_system_prompt_carries_persona_tools_and_project_context() {
    let def = super::super::find_definition("explorer").unwrap();
    let prompt = child_system_prompt(&def);
    assert!(prompt.starts_with(&def.prompt), "persona leads");
    // BTreeSet iteration is sorted.
    assert!(prompt.contains("Your tools: find, grep, read"), "{prompt}");
    // project_context() appends the repo's own instructions when present
    // (this repo has one); the marker matches the main prompt's shape.
    if crate::llm::prompt::project_context().is_some() {
        assert!(prompt.contains("--- Project instructions ---"), "{prompt}");
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // single-threaded test runtime; env must stay redirected across the spawn
async fn delegate_rejects_unresolvable_model_before_spawning() {
    // Complexity-based override fails fast: an unresolvable `model`
    // returns InvalidArgument without spawning a child (no Running
    // entry the parent must poll to discover the error).
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = [
        "OPENCODE_API_KEY",
        "CODEX_ACCESS_TOKEN",
        "CODEX_ACCOUNT_ID",
        "CODEX_HOME",
        "XDG_CACHE_HOME",
    ]
    .iter()
    .map(|key| (*key, std::env::var_os(key)))
    .collect::<Vec<_>>();
    for key in ["OPENCODE_API_KEY", "CODEX_ACCESS_TOKEN", "CODEX_ACCOUNT_ID"] {
        std::env::remove_var(key);
    }
    let dir = std::env::temp_dir().join(format!("dex-delegate-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("XDG_CACHE_HOME", &dir);
    std::env::set_var("CODEX_HOME", dir.join("codex-home"));
    let manager = AgentManager::new("sess");
    let ctx = Arc::new(AgentTurnContext {
        depth: 0,
        session_id: "sess".to_string(),
        session_path: PathBuf::new(),
        cwd: String::new(),
        config: Arc::new(crate::llm::config::tests::test_cfg()),
        manager: manager.clone(),
        session_approvals: HashSet::new(),
        child_approvals: None,
        live_approvals: None,
    });
    let mut args = Map::new();
    args.insert("agent".into(), json!("explorer"));
    args.insert("task".into(), json!("look around"));
    args.insert("model".into(), json!("openai-codex/gpt-x"));
    let error = delegate(&ctx, &args, &Policy::trusted()).await.unwrap_err();
    assert!(
        error.to_string().contains("openai-codex")
            || error.to_string().contains("credentials")
            || error.to_string().contains("endpoint"),
        "{error}"
    );
    assert_eq!(manager.active_count(), 0);
    manager.shutdown().await;
    for (key, prev) in saved {
        match prev {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn effective_child_model_prefers_explicit_then_handle_then_def() {
    // Per-spawn beats file-frontmatter; a resume without its own pick
    // keeps the finished generation's model; otherwise the definition
    // (frontmatter or `None` = inherit) stands.
    assert_eq!(
        effective_child_model(
            Some("opencode/m-strong".to_string()),
            Some("opencode/m-cheap"),
            Some("opencode/m-front".to_string()),
        )
        .as_deref(),
        Some("opencode/m-strong")
    );
    assert_eq!(
        effective_child_model(
            None,
            Some("opencode/m-cheap"),
            Some("opencode/m-front".to_string()),
        )
        .as_deref(),
        Some("opencode/m-cheap")
    );
    assert_eq!(
        effective_child_model(None, None, Some("opencode/m-front".to_string()),).as_deref(),
        Some("opencode/m-front")
    );
    assert_eq!(effective_child_model(None, None, None), None);
}

#[test]
fn resolve_child_config_applies_override_once_and_inherits_on_none() {
    // Success path the failure test above doesn't cover: a same-provider
    // bare id resolves without credentials, `None` clones the parent,
    // and the resolved config is what the child (and grandchild) runs.
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-delegate-model-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let prev_cache = std::env::var_os("XDG_CACHE_HOME");
    std::env::set_var("XDG_CACHE_HOME", &dir);
    let parent = crate::llm::config::tests::test_cfg();
    let inherited = resolve_child_config(&parent, None).unwrap();
    assert_eq!(inherited.model, parent.model);
    assert_eq!(inherited.base_url, parent.base_url);
    let cheap = resolve_child_config(&parent, Some("m-c")).unwrap();
    assert_eq!(cheap.model, "m-c");
    // Same provider, so the endpoint stays put; only the id moves.
    assert_eq!(cheap.base_url, parent.base_url);
    match prev_cache {
        Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
        None => std::env::remove_var("XDG_CACHE_HOME"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn parent_cancel_ends_delegate_output_without_touching_children() {
    // §22-I: the parent's cancel token ends the `delegate_output` wait
    // early (the deadline is minutes away, so an instant return proves
    // the token drove it) — and the child keeps running untouched.
    let manager = AgentManager::new("sess");
    let ctx = Arc::new(AgentTurnContext {
        depth: 0,
        session_id: "sess".to_string(),
        // No real child body runs here, so the parent path is never touched.
        session_path: PathBuf::new(),
        cwd: String::new(),
        config: Arc::new(crate::llm::config::tests::test_cfg()),
        manager: manager.clone(),
        session_approvals: HashSet::new(),
        child_approvals: None,
        live_approvals: None,
    });
    let id = manager
        .spawn(
            &super::super::builtin_definitions()
                .into_iter()
                .next()
                .unwrap(),
            SpawnMeta::fresh(),
            // Ends only through its own token: parent cancel must not
            // reach it.
            |token, _progress, _id| async move {
                token.cancelled().await;
                AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: Some("child saw cancel".to_string()),
                    usage: None,
                    reason: ExitReason::ShutDown,
                    tool_calls: 0,
                    resume: None,
                }
            },
        )
        .unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let mut args = Map::new();
    args.insert("agent_id".into(), json!(id.to_string()));
    let wait =
        tokio::time::timeout(Duration::from_secs(5), delegate_output(&ctx, &args, &token)).await;
    match wait {
        Ok(Ok(out)) => {
            let value: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(value["state"], "running");
        }
        other => panic!("expected a fast running report, got {other:?}"),
    }
    // Untouched: still live, still Running, still its own cancel token.
    assert_eq!(manager.active_count(), 1);
    assert_eq!(manager.status(&id), Some(AgentState::Running));
    // Cleanup: cancel + join so no task outlives the test.
    manager.shutdown().await;
    assert_eq!(manager.active_count(), 0);
}

#[test]
fn parse_generation_reads_trailing_suffix_only() {
    assert_eq!(parse_generation("sess-0-explorer.jsonl"), 0);
    assert_eq!(parse_generation("sess-0-explorer.g1.jsonl"), 1);
    assert_eq!(parse_generation("sess-0-explorer.g12.jsonl"), 12);
    // No id guard (`-`): an id-less `.g1`-shaped stem stays gen 0.
    assert_eq!(parse_generation("my.g1.jsonl"), 0);
    // Greedy on the last suffix: a definition literally named `my.g1`
    // at generation 0 is indistinguishable from generation 1.
    assert_eq!(parse_generation("sess-0-my.g1.jsonl"), 1);
    assert_eq!(parse_generation("sess-0-explorer.gx.jsonl"), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn resume_messages_reapplies_persona_and_appends_nudge() {
    // §24.3 (review fix): the journal never stores the System role, so
    // `resume_messages` must re-apply the persona from the definition —
    // a resumed child runs with the same persona it started with.
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = PathBuf::from("/tmp/dex-supervision-resume-msgs");
    let parent_path = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    let mut child = Session::child(
        &parent_path,
        "/tmp/dex-supervision-resume-msgs",
        "sess-2",
        "tester",
        0,
    )
    .unwrap();
    let _ = child.turn_event("turn_start");
    child
        .append_message(&ChatMessage::user("check the build"))
        .unwrap();
    child
        .append_message(&ChatMessage::assistant("found three risks"))
        .unwrap();
    drop(child);
    let def = super::super::builtin_definitions()
        .into_iter()
        .find(|def| def.name == "tester")
        .unwrap();
    let request = ResumeRequest {
        handle: ResumeHandle {
            agent_id: AgentId("sess-2".to_string()),
            transcript: Session::child_path(&parent_path, "sess-2", "tester", 0),
            generation: 0,
            remaining_budget: Some(7),
            model: None,
            note: "timed out after 3 tool calls; continue from the transcript".to_string(),
        },
        instruction: Some("skip the build".to_string()),
        file_hints: Vec::new(),
    };
    let messages = resume_messages(&def, &request).unwrap();
    // Persona leads, from the definition — not from the journal.
    assert!(
        messages[0]
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(&def.prompt)),
        "first message must be the persona: {:?}",
        messages[0].content
    );
    // Replay is verbatim and in order.
    assert_eq!(messages[1].content.as_deref(), Some("check the build"));
    assert_eq!(messages[2].content.as_deref(), Some("found three risks"));
    // The nudge lands last, carrying the instruction.
    let last = messages.last().unwrap();
    assert_eq!(last.name.as_deref(), Some("resume"));
    let nudge = last.content.as_deref().unwrap();
    assert!(nudge.contains("timed out after 3 tool calls"), "{nudge}");
    assert!(nudge.contains("skip the build"), "{nudge}");
    assert!(nudge.contains("at most 7 further tool calls"), "{nudge}");
    // The persona never journals (System role skipped on replay too).
    let reloaded = crate::session::load_messages_from_session(&Session::child_path(
        &parent_path,
        "sess-2",
        "tester",
        0,
    ))
    .unwrap();
    assert!(reloaded
        .iter()
        .all(|m| m.role != crate::protocol::Role::System));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn resume_messages_rejects_an_empty_transcript_loudly() {
    // §24.3 review fix: an empty replay (turn_start without any message
    // line) must NOT fall back to the degenerate seed task — it fails
    // Permanent so the parent re-delegates with a fresh task.
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = PathBuf::from("/tmp/dex-supervision-resume-empty");
    let parent_path = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    let mut child = Session::child(
        &parent_path,
        "/tmp/dex-supervision-resume-empty",
        "sess-3",
        "tester",
        0,
    )
    .unwrap();
    let _ = child.turn_event("turn_start");
    drop(child);
    let def = super::super::builtin_definitions()
        .into_iter()
        .find(|def| def.name == "tester")
        .unwrap();
    let request = ResumeRequest {
        handle: ResumeHandle {
            agent_id: AgentId("sess-3".to_string()),
            transcript: Session::child_path(&parent_path, "sess-3", "tester", 0),
            generation: 0,
            remaining_budget: None,
            model: None,
            note: "interrupted".to_string(),
        },
        instruction: None,
        file_hints: Vec::new(),
    };
    let error = resume_messages(&def, &request).unwrap_err();
    assert!(error.contains("no replayable messages"), "{error}");
    assert!(error.contains("re-delegate"), "{error}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resume_nudge_carries_reason_instruction_and_budget() {
    let request = ResumeRequest {
        handle: ResumeHandle {
            agent_id: AgentId("sess-0".to_string()),
            transcript: PathBuf::from("/tmp/x.jsonl"),
            generation: 2,
            remaining_budget: Some(5),
            model: None,
            note: "turn budget exhausted after 50 tool calls; continue from the transcript"
                .to_string(),
        },
        instruction: Some("skip the build".to_string()),
        file_hints: vec![PathBuf::from("src/main.rs")],
    };
    let nudge = resume_nudge(&request);
    assert!(nudge.contains("turn budget exhausted"), "{nudge}");
    assert!(nudge.contains("skip the build"), "{nudge}");
    assert!(nudge.contains("src/main.rs"), "{nudge}");
    assert!(nudge.contains("at most 5 further tool calls"), "{nudge}");
    let spent = ResumeRequest {
        handle: ResumeHandle {
            remaining_budget: Some(0),
            ..request.handle.clone()
        },
        instruction: None,
        file_hints: Vec::new(),
    };
    assert!(
        resume_nudge(&spent).contains("No further tool calls remain"),
        "{}",
        resume_nudge(&spent)
    );
}

#[test]
fn resume_conversation_prepends_system_prompt_and_journals_without_it() {
    // The transcript never journals the system prompt and the loader
    // drops `Role::System` lines — the resume must re-derive it, or
    // the generation runs with no persona and no tool rules.
    let def = super::super::builtin_definitions()
        .into_iter()
        .next()
        .unwrap();
    let replayed = vec![
        ChatMessage::user("do the thing"),
        ChatMessage::assistant("on it"),
    ];
    let request = ResumeRequest {
        handle: ResumeHandle {
            agent_id: AgentId("sess-0".to_string()),
            transcript: PathBuf::from("/tmp/x.jsonl"),
            generation: 0,
            remaining_budget: Some(5),
            model: None,
            note: "timed out".to_string(),
        },
        instruction: Some("skip the build".to_string()),
        file_hints: Vec::new(),
    };
    let (in_memory, journal) = super::exec::resume_conversation(&def, replayed, &request);
    assert_eq!(in_memory.len(), 4);
    assert_eq!(in_memory[0].role, crate::protocol::Role::System);
    assert!(!in_memory[0]
        .content
        .as_deref()
        .unwrap_or_default()
        .is_empty());
    assert_eq!(in_memory[1].content.as_deref(), Some("do the thing"));
    assert_eq!(in_memory[2].content.as_deref(), Some("on it"));
    assert_eq!(in_memory[3].name.as_deref(), Some("resume"));
    assert!(
        in_memory[3]
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("skip the build"),
        "{}",
        in_memory[3].content.as_deref().unwrap_or_default()
    );
    // The journal mirrors the fresh path: no system line, nudge last.
    assert_eq!(journal.len(), 3);
    assert_ne!(journal[0].role, crate::protocol::Role::System);
    assert_eq!(journal[2].name.as_deref(), Some("resume"));
}

fn resume_test_ctx(manager: AgentManager, session_path: PathBuf) -> Arc<AgentTurnContext> {
    Arc::new(AgentTurnContext {
        depth: 0,
        session_id: "sess".to_string(),
        session_path,
        cwd: String::new(),
        config: Arc::new(crate::llm::config::tests::test_cfg()),
        manager,
        session_approvals: HashSet::new(),
        child_approvals: None,
        live_approvals: None,
    })
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_resume_handle_prefers_retained_then_rejects_live() {
    // A retained terminal result that advertised a handle wins without
    // touching the disk.
    let manager = AgentManager::new("sess");
    let dir = PathBuf::from("/tmp/dex-supervision-resolve");
    let ctx = resume_test_ctx(manager.clone(), dir.join("sess.jsonl"));
    let transcript = dir.join("agents").join("sess-0-tester.jsonl");
    let held = transcript.clone();
    let id = manager
        .spawn(
            &super::super::builtin_definitions()
                .into_iter()
                .next()
                .unwrap(),
            SpawnMeta::fresh(),
            move |_, _, _| {
                let transcript = held.clone();
                async move {
                    AgentResult {
                        status: AgentState::TimedOut,
                        summary: "partial".to_string(),
                        error: Some("timed out after 600s".to_string()),
                        usage: None,
                        reason: ExitReason::Exhausted(crate::agent::delegate::ExhaustKind::Timeout),
                        tool_calls: 4,
                        resume: Some(ResumeHandle {
                            agent_id: AgentId("sess-0".to_string()),
                            transcript,
                            generation: 0,
                            remaining_budget: Some(46),
                            model: None,
                            note: "timed out after 4 tool calls; continue from the transcript"
                                .to_string(),
                        }),
                    }
                }
            },
        )
        .unwrap();
    // The body returns immediately, but the spawned task still has to
    // be polled once: join it through a bounded wait before resolving.
    match manager.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(_) => {}
        other => panic!("expected Finished, got {other:?}"),
    }
    let handle = resolve_resume_handle(&ctx, &id).await.unwrap();
    assert_eq!(handle.remaining_budget, Some(46));
    assert_eq!(handle.generation, 0);
    // A live child rejects: it needs output/wait, not a new generation.
    let live = manager
        .spawn(
            &super::super::builtin_definitions()
                .into_iter()
                .next()
                .unwrap(),
            SpawnMeta::fresh(),
            |token, _, _| async move {
                token.cancelled().await;
                AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: None,
                    usage: None,
                    reason: ExitReason::ShutDown,
                    tool_calls: 0,
                    resume: None,
                }
            },
        )
        .unwrap();
    let error = resolve_resume_handle(&ctx, &live).await.unwrap_err();
    assert!(error.to_string().contains("still running"), "{error}");
    // Unknown id with no disk: clean rejection pointing at the list.
    let error = resolve_resume_handle(&ctx, &AgentId("sess-9".to_string()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("action=list"), "{error}");
    manager.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_resume_handle_finds_interrupted_runs_on_disk() {
    // A daemon-restart-killed child: header + turn_start, no terminal
    // marker. The registry never saw this manager, so resolution is
    // purely the on-disk scan.
    let dir = PathBuf::from("/tmp/dex-supervision-resolve-disk");
    let parent_path = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    let mut child = Session::child(
        &parent_path,
        "/tmp/dex-supervision-resolve-disk",
        "sess-4",
        "explorer",
        0,
    )
    .unwrap();
    let _ = child.turn_event("turn_start");
    drop(child);
    let mut gen2 = Session::child(
        &parent_path,
        "/tmp/dex-supervision-resolve-disk",
        "sess-5",
        "tester",
        2,
    )
    .unwrap();
    let _ = gen2.turn_event("turn_start");
    drop(gen2);
    let manager = AgentManager::new("sess");
    let ctx = resume_test_ctx(manager, parent_path);
    let handle = resolve_resume_handle(&ctx, &AgentId("sess-4".to_string()))
        .await
        .unwrap();
    assert_eq!(handle.generation, 0);
    assert_eq!(handle.remaining_budget, None);
    assert!(handle.transcript.ends_with("agents/sess-4-explorer.jsonl"));
    let handle = resolve_resume_handle(&ctx, &AgentId("sess-5".to_string()))
        .await
        .unwrap();
    assert_eq!(handle.generation, 2);
    // `delegate_list` prints on-disk rows under their transcript stem;
    // the advertised id must round-trip back into a handle.
    let handle = resolve_resume_handle(&ctx, &AgentId("sess-4-explorer".to_string()))
        .await
        .unwrap();
    assert_eq!(handle.generation, 0);
    assert!(handle.transcript.ends_with("agents/sess-4-explorer.jsonl"));
    let handle = resolve_resume_handle(&ctx, &AgentId("sess-5-tester.g2".to_string()))
        .await
        .unwrap();
    assert_eq!(handle.generation, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn delegate_list_reports_live_retained_and_disk() {
    let dir = PathBuf::from("/tmp/dex-supervision-list");
    let parent_path = dir.join("sess.jsonl");
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    let manager = AgentManager::new("sess");
    let ctx = resume_test_ctx(manager.clone(), parent_path.clone());
    let live = manager
        .spawn(
            &super::super::builtin_definitions()
                .into_iter()
                .next()
                .unwrap(),
            SpawnMeta::fresh(),
            |token, _, _| async move {
                token.cancelled().await;
                AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: None,
                    usage: None,
                    reason: ExitReason::ShutDown,
                    tool_calls: 0,
                    resume: None,
                }
            },
        )
        .unwrap();
    let done = manager
        .spawn(
            &super::super::builtin_definitions()
                .into_iter()
                .next()
                .unwrap(),
            SpawnMeta::fresh(),
            |_, _, _| async {
                AgentResult {
                    status: AgentState::Completed,
                    summary: "ok".to_string(),
                    error: None,
                    usage: None,
                    reason: ExitReason::Normal,
                    tool_calls: 1,
                    resume: None,
                }
            },
        )
        .unwrap();
    match manager.wait(&done, Duration::from_secs(5)).await {
        WaitOutcome::Finished(_) => {}
        other => panic!("expected Finished, got {other:?}"),
    }
    let mut disk = Session::child(
        &parent_path,
        "/tmp/dex-supervision-list",
        "sess-9",
        "explorer",
        0,
    )
    .unwrap();
    let _ = disk.turn_event("turn_start");
    drop(disk);
    let out = delegate_list(&ctx).await.unwrap();
    let value: Value = serde_json::from_str(&out).unwrap();
    let children = value["children"].as_array().unwrap();
    assert_eq!(children.len(), 3, "{out}");
    let row = |agent_id: &str| {
        children
            .iter()
            .find(|row| row["agent_id"] == agent_id)
            .unwrap_or_else(|| panic!("missing row {agent_id}: {out}"))
    };
    assert_eq!(row(&live.to_string())["state"], "running");
    assert_eq!(row(&live.to_string())["resumable"], false);
    assert_eq!(row(&done.to_string())["state"], "completed");
    assert_eq!(row(&done.to_string())["resumable"], false);
    assert_eq!(row("sess-9-explorer")["state"], "interrupted");
    assert_eq!(row("sess-9-explorer")["resumable"], true);
    manager.shutdown().await;
    let _ = std::fs::remove_dir_all(&dir);
}
