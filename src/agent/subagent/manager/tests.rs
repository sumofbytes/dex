//! Tests, split out of the module body so it stays implementation.

use super::*;
use std::path::PathBuf;
use tokio::sync::Barrier;

fn test_def(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.to_string(),
        prompt: String::new(),
        model: None,
        tools: ["read".to_string()].into_iter().collect(),
        max_tool_iterations: None,
        timeout: Duration::from_secs(60),
    }
}

fn completed(summary: &str) -> AgentResult {
    AgentResult {
        status: AgentState::Completed,
        summary: summary.to_string(),
        error: None,
        reason: ExitReason::Normal,
        tool_calls: 0,
        resume: None,
        usage: None,
    }
}

fn failed(error: &str) -> AgentResult {
    AgentResult {
        status: AgentState::Failed,
        summary: String::new(),
        error: Some(error.to_string()),
        usage: None,
        reason: ExitReason::Permanent,
        tool_calls: 0,
        resume: None,
    }
}

/// A body that only ends through its token — the shape Phase 7's real
/// runner has. Lets tests hold children live without sleeps.
async fn token_body(
    token: CancellationToken,
    _progress: ProgressReporter,
    _id: AgentId,
) -> AgentResult {
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
}

#[tokio::test(flavor = "current_thread")]
async fn journal_hook_fires_on_every_terminal_path() {
    // §15 V1a: the daemon's hook observes every terminal path — the
    // single choke point means cancel and panic are journaled too.
    // §15 V1b: the same hook now carries the typed events; spawn and
    // terminal completions must both fire through it.
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let capture = seen.clone();
    let mgr = AgentManager::new("sess").with_events(Arc::new(move |event| {
        let text = match event {
            AgentEvent::Spawned { agent_id, .. } => format!("spawned {agent_id}"),
            AgentEvent::Progress { agent_id, .. } => format!("progress {agent_id}"),
            AgentEvent::Completed(notice) => {
                format!(
                    "{} {}",
                    notice.agent_id,
                    super::super::status_word(notice.status)
                )
            }
        };
        capture.lock().unwrap_or_else(|e| e.into_inner()).push(text);
    }));
    let completed = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            completed("findings")
        })
        .unwrap();
    let cancelled_id = mgr
        .spawn(&test_def("tester"), SpawnMeta::fresh(), token_body)
        .unwrap();
    assert!(matches!(
        mgr.wait(&completed, Duration::from_secs(5)).await,
        WaitOutcome::Finished(_)
    ));
    mgr.cancel(&cancelled_id);
    assert!(matches!(
        mgr.wait(&cancelled_id, Duration::from_secs(5)).await,
        WaitOutcome::Finished(_)
    ));
    let entries = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        entries,
        vec![
            "spawned sess-0".to_string(),
            "spawned sess-1".to_string(),
            "sess-0 completed".to_string(),
            "sess-1 cancelled".to_string(),
        ]
    );
    // Finished children leave the live registry (§12 V1b): approvals
    // only ever arrive from a running child, so the label lookup is
    // `None` for a terminal id.
    assert_eq!(mgr.definition_name(&cancelled_id), None);
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_assigns_session_scoped_ids_and_reports_running() {
    let mgr = AgentManager::new("sess");
    // `spawn` never yields before returning, so on a single-threaded
    // runtime the wrapper cannot have run yet: fully deterministic.
    let first = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            completed("findings")
        })
        .unwrap();
    let second = mgr
        .spawn(&test_def("tester"), SpawnMeta::fresh(), |_, _, _| async {
            completed("pass")
        })
        .unwrap();
    assert_eq!(first.to_string(), "sess-0");
    assert_eq!(second.to_string(), "sess-1");
    assert_eq!(mgr.status(&first), Some(AgentState::Running));
    assert_eq!(mgr.active_count(), 2);
    // The §12 V1b approval label resolves from the live registry: a
    // running child's definition name is available the moment a parked
    // approval needs it.
    assert_eq!(mgr.definition_name(&first), Some("explorer".to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn completing_child_files_result_and_notice() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            completed("findings")
        })
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Completed);
            assert_eq!(result.summary, "findings");
            assert_eq!(result.error, None);
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(mgr.status(&id), Some(AgentState::Completed));
    assert_eq!(mgr.active_count(), 0);
    let notices = mgr.drain_notices();
    assert_eq!(
        notices,
        vec![AgentNotice {
            agent_id: id,
            name: "explorer".to_string(),
            status: AgentState::Completed,
            usage: None,
            resumable: false,
        }]
    );
    assert_eq!(mgr.take_overflow(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn failed_result_preserved_with_error() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("tester"), SpawnMeta::fresh(), |_, _, _| async {
            failed("boom")
        })
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Failed);
            assert_eq!(result.error.as_deref(), Some("boom"));
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    let notices = mgr.drain_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].status, AgentState::Failed);
}

#[tokio::test(flavor = "current_thread")]
async fn wait_times_out_then_finishes() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            completed("late")
        })
        .unwrap();
    match mgr.wait(&id, Duration::from_millis(50)).await {
        WaitOutcome::Running(state) => assert_eq!(state, AgentState::Running),
        other => panic!("expected Running, got {other:?}"),
    }
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => assert_eq!(result.summary, "late"),
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unknown_id_is_unknown_everywhere() {
    let mgr = AgentManager::new("sess");
    let unknown = AgentId("sess-99".to_string());
    assert_eq!(
        mgr.wait(&unknown, Duration::from_millis(10)).await,
        WaitOutcome::Unknown
    );
    assert_eq!(mgr.status(&unknown), None);
    assert_eq!(mgr.cancel(&unknown), None);
    assert!(mgr.drain_notices().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn cancel_fires_child_token_and_yields_cancelled() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
    assert_eq!(mgr.cancel(&id), Some(AgentState::Running));
    // `cancel` never yields, so on a single-threaded runtime the
    // wrapper cannot have reaped the entry yet: the fired token is
    // observably the child's own.
    let fired = mgr
        .lock()
        .running
        .get(&id)
        .map(|child| child.token.is_cancelled());
    assert_eq!(fired, Some(true));
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Cancelled);
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(mgr.active_count(), 0);
    let notices = mgr.drain_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].status, AgentState::Cancelled);
}

#[tokio::test(flavor = "current_thread")]
async fn over_cap_spawn_rejects_fail_fast() {
    // Over-cap spawns reject immediately with the running list — no
    // queue, no drain: the model waits for or cancels a child and
    // retries explicitly.
    let mgr = AgentManager::new("sess");
    let mut ids = Vec::new();
    for _ in 0..MAX_CHILDREN {
        ids.push(
            mgr.spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
                .unwrap(),
        );
    }
    match mgr.spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body) {
        Err(SpawnError::AtCapacity { limit, running }) => {
            assert_eq!(limit, MAX_CHILDREN);
            assert_eq!(running.len(), MAX_CHILDREN);
        }
        other => panic!("expected AtCapacity, got {other:?}"),
    }
    // A terminal frees a slot for an explicit retry — nothing drains
    // on its own.
    mgr.cancel(&ids[0]);
    assert!(matches!(
        mgr.wait(&ids[0], Duration::from_secs(5)).await,
        WaitOutcome::Finished(_)
    ));
    mgr.spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
    mgr.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn notices_bound_and_overflow_folds() {
    let mgr = AgentManager::new("sess");
    for _ in 0..(MAX_NOTICES + 3) {
        let id = mgr
            .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
                completed("done")
            })
            .unwrap();
        assert!(matches!(
            mgr.wait(&id, Duration::from_secs(5)).await,
            WaitOutcome::Finished(_)
        ));
    }
    let notices = mgr.drain_notices();
    assert_eq!(notices.len(), MAX_NOTICES);
    // FIFO: the retained notices are the first completions.
    assert_eq!(notices[0].agent_id.to_string(), "sess-0");
    assert_eq!(mgr.take_overflow(), 3);
    assert_eq!(mgr.take_overflow(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn results_survive_notice_drain() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            completed("durable")
        })
        .unwrap();
    assert!(matches!(
        mgr.wait(&id, Duration::from_secs(5)).await,
        WaitOutcome::Finished(_)
    ));
    assert_eq!(mgr.drain_notices().len(), 1);
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => assert_eq!(result.summary, "durable"),
        other => panic!("expected retained Finished, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_cancels_and_joins_children() {
    let mgr = AgentManager::new("sess");
    let first = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
    let second = mgr
        .spawn(&test_def("tester"), SpawnMeta::fresh(), token_body)
        .unwrap();
    mgr.shutdown().await;
    assert_eq!(mgr.active_count(), 0);
    for id in [&first, &second] {
        match mgr.wait(id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }
    assert_eq!(mgr.drain_notices().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_completions_stay_consistent() {
    let mgr = AgentManager::new("sess");
    let gate = std::sync::Arc::new(Barrier::new(MAX_CHILDREN));
    let mut ids = Vec::new();
    for n in 0..MAX_CHILDREN {
        let gate = gate.clone();
        ids.push(
            mgr.spawn(
                &test_def(&format!("agent-{n}")),
                SpawnMeta::fresh(),
                |_, _, _| async move {
                    gate.wait().await;
                    completed("through the gate")
                },
            )
            .unwrap(),
        );
    }
    for id in &ids {
        match mgr.wait(id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }
    assert_eq!(mgr.active_count(), 0);
    assert_eq!(mgr.drain_notices().len(), MAX_CHILDREN);
    assert_eq!(mgr.take_overflow(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn panicking_body_fails_without_orphaning_the_entry() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            panic!("body exploded")
        })
        .unwrap();
    // The wrapper catches the panic and funnels a synthesized `Failed`
    // through `finish` — no entry left Running, no slot leaked (§14).
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Failed);
            assert_eq!(result.error.as_deref(), Some("child panicked"));
            assert_eq!(result.summary, "");
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(mgr.status(&id), Some(AgentState::Failed));
    assert_eq!(mgr.active_count(), 0);
    assert_eq!(mgr.drain_notices().len(), 1);
    // Cleanup ran: a fresh spawn still works.
    mgr.spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn child_timeout_ends_timed_out() {
    let mgr = AgentManager::new("sess");
    let mut def = test_def("explorer");
    def.timeout = Duration::from_millis(50);
    let id = mgr
        .spawn(&def, SpawnMeta::fresh(), |_, _, _| async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            completed("never")
        })
        .unwrap();
    // The wrapper enforces the definition's timeout (§14): the hung body
    // is dropped and the run ends TimedOut with a synthesized error.
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::TimedOut);
            assert!(result.error.unwrap().contains("timed out"));
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(mgr.active_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_after_shutdown_rejects_closed() {
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
    mgr.shutdown().await;
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Cancelled)
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    // A stale clone (an in-flight tool call holds one) cannot respawn a
    // child nobody will ever join.
    assert!(matches!(
        mgr.spawn(&test_def("x"), SpawnMeta::fresh(), token_body),
        Err(SpawnError::Closed)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn progress_reports_current_tool_and_clears() {
    let mgr = AgentManager::new("sess");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let id = mgr
        .spawn(
            &test_def("explorer"),
            SpawnMeta::fresh(),
            move |_, progress, _| {
                async move {
                    progress.set("bash");
                    let _ = rx.await; // hold the "tool call" until observed
                    progress.clear();
                    completed("done")
                }
            },
        )
        .unwrap();
    // Current-thread runtime: the wrapper has not run yet.
    assert_eq!(mgr.progress(&id), None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(mgr.progress(&id), Some("bash".to_string()));
    let _ = tx.send(());
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => assert_eq!(result.summary, "done"),
        other => panic!("expected Finished, got {other:?}"),
    }
    // Finished children carry no live progress.
    assert_eq!(mgr.progress(&id), None);
}

#[tokio::test(flavor = "current_thread")]
async fn one_child_failing_does_not_disturb_a_sibling() {
    // §22-H: results map to their own ids and a sibling's run is
    // independent of a failure — no shared-state corruption, no
    // cascade.
    let mgr = AgentManager::new("sess");
    let doomed = mgr
        .spawn(&test_def("doomed"), SpawnMeta::fresh(), |_, _, _| async {
            failed("boom")
        })
        .unwrap();
    let sibling = mgr
        .spawn(&test_def("sibling"), SpawnMeta::fresh(), |_, _, _| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            completed("fine")
        })
        .unwrap();
    match mgr.wait(&doomed, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Failed);
            assert_eq!(result.error.as_deref(), Some("boom"));
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    match mgr.wait(&sibling, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Completed);
            assert_eq!(result.summary, "fine");
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    assert_eq!(mgr.active_count(), 0);
    let notices = mgr.drain_notices();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0].status, AgentState::Failed);
    assert_eq!(notices[1].status, AgentState::Completed);
}

#[test]
fn notice_text_carries_usage_and_hides_unpriced_cost() {
    // §18: the lifecycle line carries the child's own spend, deduped
    // by seq on replay like every other lifecycle line.
    let notice = AgentNotice {
        agent_id: AgentId("sess-3".to_string()),
        name: "explorer".to_string(),
        status: AgentState::Completed,
        usage: Some(AgentUsage {
            prompt_tokens: 1_200,
            output_tokens: 300,
            cost_usd: 0.0312,
        }),
        resumable: false,
    };
    assert_eq!(
        notice.text(),
        "[agent explorer:sess-3] finished completed · 1.5k tok · $0.0312"
    );
    // Unpriced model: tokens yes, no `$0.0000` noise.
    let unpriced = AgentNotice {
        agent_id: AgentId("sess-3".to_string()),
        name: "explorer".to_string(),
        status: AgentState::Completed,
        usage: Some(AgentUsage {
            prompt_tokens: 1,
            output_tokens: 2,
            cost_usd: 0.0,
        }),
        resumable: false,
    };
    assert_eq!(
        unpriced.text(),
        "[agent explorer:sess-3] finished completed · 3 tok"
    );
    // Wrapper-synthesized endings carry no usage row at all.
    let bare = AgentNotice {
        agent_id: AgentId("sess-3".to_string()),
        name: "explorer".to_string(),
        status: AgentState::Completed,
        usage: None,
        resumable: false,
    };
    assert_eq!(bare.text(), "[agent explorer:sess-3] finished completed");
    // A resumable result advertises the re-entry — same prefix the TUI
    // matches on, suffix only.
    let resumable = AgentNotice {
        agent_id: AgentId("sess-3".to_string()),
        name: "explorer".to_string(),
        status: AgentState::TimedOut,
        usage: None,
        resumable: true,
    };
    assert_eq!(
            resumable.text(),
            "[agent explorer:sess-3] finished timed out · resumable with delegate(resume_from = \"sess-3\")"
        );
}

#[tokio::test(flavor = "current_thread")]
async fn transient_wrapper_death_advertises_a_resume_handle() {
    // §24.1: a panicking body is `Transient`; with the transcript on
    // disk holding progress, `finish` attaches the handle at its
    // single choke point — the panic arm itself stays dumb.
    let dir = PathBuf::from("/tmp/dex-supervision-resume");
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    // The first spawn in a fresh `sess` manager allocates `sess-0`.
    std::fs::write(
        agents.join("sess-0-explorer.jsonl"),
        "{\"entry_type\":\"session\"}\n{\"entry_type\":\"turn_start\"}\n",
    )
    .unwrap();
    let mgr = AgentManager::new("sess");
    // The first spawn allocates `sess-0`, matching the fixture above.
    let id = mgr
        .spawn(
            &test_def("explorer"),
            SpawnMeta {
                generation: 0,
                parent_session: Some(dir.join("sess.jsonl")),
                remaining_budget: None,
            },
            |_, _, _| async { panic!("boom") },
        )
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Failed);
            assert_eq!(result.reason, ExitReason::Transient);
            let handle = result.resume.expect("transient + progress advertises");
            assert_eq!(handle.agent_id, id);
            assert_eq!(handle.generation, 0);
            assert!(handle.note.contains("interrupted"), "{}", handle.note);
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    let notices = mgr.drain_notices();
    assert!(notices.iter().any(|notice| notice.resumable));
    assert!(notices[0].text().contains("resume_from"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn escalate_clamps_exhausted_meter_to_full_cap() {
    // tool_calls == cap: a Some(0) handle would promise "write your
    // final summary without tools" while the loop still runs one
    // post-hoc round and then hard-fails — the handle advertises None
    // (the definition's full cap) instead; partial spend keeps the
    // honest remainder.
    let dir = PathBuf::from("/tmp/dex-supervision-clamp");
    let mgr = AgentManager::new("sess");
    let mut def = test_def("explorer");
    def.max_tool_iterations = Some(2);
    let spent_all = mgr
        .spawn(
            &def,
            SpawnMeta {
                generation: 0,
                parent_session: Some(dir.join("sess.jsonl")),
                remaining_budget: None,
            },
            |_, _, _| async {
                AgentResult {
                    status: AgentState::TimedOut,
                    summary: "partial".to_string(),
                    error: Some("timed out after 600s".to_string()),
                    usage: None,
                    reason: ExitReason::Exhausted(crate::agent::subagent::ExhaustKind::Timeout),
                    tool_calls: 2,
                    resume: None,
                }
            },
        )
        .unwrap();
    let spent_one = mgr
        .spawn(
            &def,
            SpawnMeta {
                generation: 0,
                parent_session: Some(dir.join("sess.jsonl")),
                remaining_budget: None,
            },
            |_, _, _| async {
                AgentResult {
                    status: AgentState::TimedOut,
                    summary: "partial".to_string(),
                    error: Some("timed out after 600s".to_string()),
                    usage: None,
                    reason: ExitReason::Exhausted(crate::agent::subagent::ExhaustKind::Timeout),
                    tool_calls: 1,
                    resume: None,
                }
            },
        )
        .unwrap();
    for (id, expected) in [(&spent_all, None), (&spent_one, Some(1))] {
        match mgr.wait(id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                let handle = result.resume.expect("escalated with progress");
                assert_eq!(handle.remaining_budget, expected);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn normal_completion_advertises_no_handle() {
    // `Normal` never advertises a handle, even with progress.
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), |_, _, _| async {
            AgentResult {
                status: AgentState::Completed,
                summary: "done".to_string(),
                error: None,
                usage: None,
                reason: ExitReason::Normal,
                tool_calls: 7,
                resume: None,
            }
        })
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => assert!(result.resume.is_none()),
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn resume_shaped_spawn_completes() {
    // The resume path in `tools.rs` re-spawns with generation + 1:
    // the manager accepts that shape like any other spawn (lineage
    // itself rides the `resumed_from` field of the `delegate`
    // response, not the registry).
    let mgr = AgentManager::new("sess");
    let first = mgr
        .spawn(&test_def("explorer"), SpawnMeta::fresh(), token_body)
        .unwrap();
    let second = mgr
        .spawn(
            &test_def("explorer"),
            SpawnMeta {
                generation: 1,
                parent_session: None,
                remaining_budget: None,
            },
            token_body,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(mgr.status(&second), Some(AgentState::Running));
    mgr.shutdown().await;
    match mgr.wait(&second, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Cancelled),
        other => panic!("expected Finished, got {other:?}"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn wrapper_synthesized_ending_reconciles_the_spend_meter() {
    // §24.1 (review fix): a body that panics mid-turn loses its own
    // sink counter, but the registry meter (`ProgressReporter::set`)
    // survives it — the resume budget must reconcile to cap − spend,
    // not the full cap.
    let dir = PathBuf::from("/tmp/dex-supervision-meter");
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("sess-0-tester.jsonl"),
        "{\"entry_type\":\"session\"}\n{\"entry_type\":\"turn_start\"}\n",
    )
    .unwrap();
    let mut def = test_def("tester");
    def.max_tool_iterations = Some(10);
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(
            &def,
            SpawnMeta {
                generation: 0,
                parent_session: Some(dir.join("sess.jsonl")),
                remaining_budget: None,
            },
            |token, progress, _| async move {
                progress.set("read");
                progress.set("bash");
                // Body reports no count — it never got to synthesize.
                assert!(!token.is_cancelled());
                panic!("boom mid-turn");
            },
        )
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            assert_eq!(result.status, AgentState::Failed);
            assert_eq!(result.reason, ExitReason::Transient);
            // The advertised budget reconciles against the registry
            // meter: 10 − 2, not the full 10.
            let handle = result.resume.expect("transient + progress advertises");
            assert_eq!(handle.remaining_budget, Some(8));
            assert!(handle.note.contains("2 tool calls"), "{}", handle.note);
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn escalated_handle_carries_def_model_for_resume_inheritance() {
    // A generation spawned with `def.model` advertises it on the handle,
    // so a resume without its own `model` keeps the complexity-chosen
    // model instead of resetting to the parent. `None` stays `None`.
    let dir = PathBuf::from("/tmp/dex-supervision-handle-model");
    let agents = dir.join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("sess-0-tester.jsonl"),
        "{\"entry_type\":\"session\"}\n{\"entry_type\":\"turn_start\"}\n",
    )
    .unwrap();
    let mut with_model = test_def("tester");
    with_model.model = Some("opencode/m-cheap".to_string());
    let mgr = AgentManager::new("sess");
    let id = mgr
        .spawn(
            &with_model,
            SpawnMeta {
                generation: 0,
                parent_session: Some(dir.join("sess.jsonl")),
                remaining_budget: None,
            },
            |_, _, _| async { panic!("boom") },
        )
        .unwrap();
    match mgr.wait(&id, Duration::from_secs(5)).await {
        WaitOutcome::Finished(result) => {
            let handle = result.resume.expect("transient + progress advertises");
            assert_eq!(handle.model.as_deref(), Some("opencode/m-cheap"));
        }
        other => panic!("expected Finished, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_lists_live_and_retained() {
    // `delegate_list`'s source: one live child, one retained result.
    let mgr = AgentManager::new("sess");
    let live = mgr
        .spawn(
            &test_def("explorer"),
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
    let done = mgr
        .spawn(&test_def("tester"), SpawnMeta::fresh(), |_, _, _| async {
            completed("ok")
        })
        .unwrap();
    match mgr.wait(&done, Duration::from_secs(5)).await {
        WaitOutcome::Finished(_) => {}
        other => panic!("expected Finished, got {other:?}"),
    }
    let snapshot = mgr.snapshot();
    assert_eq!(snapshot.len(), 2);
    let live_row = snapshot.iter().find(|row| row.agent_id == live).unwrap();
    assert_eq!(live_row.state, AgentState::Running);
    assert!(!live_row.resumable);
    let done_row = snapshot.iter().find(|row| row.agent_id == done).unwrap();
    assert_eq!(done_row.state, AgentState::Completed);
    assert_eq!(done_row.name, "tester");
    assert!(!done_row.resumable);
    mgr.shutdown().await;
}
