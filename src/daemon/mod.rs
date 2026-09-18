pub(crate) mod approvals;
pub(crate) mod auth;
pub(crate) mod lookup;
pub(crate) mod server;
pub(crate) mod shell;
pub(crate) mod turn;
pub(crate) mod wake;

// Bearer-token auth lives in `auth.rs`; re-exported here so existing
// `daemon::...` paths keep working.
#[cfg(test)]
pub(crate) use auth::reset_daemon_token_for_tests;
pub(crate) use auth::{daemon_token_file, prepare_daemon_token, required_token};

use std::net::TcpListener;
use std::time::Duration;

pub(crate) mod state;
pub(crate) use state::{
    journal_event, lock_map, DaemonState, PendingApproval, SessionEntry, NEGATIVE_TTL,
};

/// Start the daemon HTTP server on an already-bound listener.
pub(crate) async fn run_daemon(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    // MCP bootstrap: connects servers in the background and merges their
    // tools into the schema cache (plus the 60s liveness sweeper). Without
    // this the manager stays uninitialized and `mcp__*` tools never exist.
    crate::mcp::global_manager();
    // Extension bootstrap: loads Lua extensions in the background and merges
    // their tools into the schema cache (plan §11 P0).
    crate::extensions::global_manager();
    let state = std::sync::Arc::new(DaemonState::new());
    // Rebuild in-memory state from the persisted JSONL on a background thread:
    // scanning every session (headers, event seqs, turn state) costs ~0.5s
    // with a few thousand sessions and would delay /health and TUI first
    // paint. Fresh sessions use uuid ids so they never collide with rebuilt
    // ones; `seed_seq` takes the max so a racing turn can't rewind a counter.
    // A panicking rebuild must not fail silent (the registry would stay
    // partial behind an `ok` health check): log it, and `rebuild_complete`
    // in `/health` stays false.
    let warm = state.clone();
    tokio::spawn(async move {
        warm.rebuild_async().await;
    });

    // Fresh installs have no models.dev catalog until `dex update --models`
    // runs, which silently degrades context windows and `/model` autocomplete.
    // Best-effort background fetch on first start; never blocks or fails the
    // daemon. One retry after 5 minutes covers a laptop waking offline, since
    // a daemon can outlive the outage; `dex update --models` always works too.
    if crate::llm::config::catalog_cache_missing() {
        tokio::spawn(async {
            // The error type is Box<dyn Error>, which is not Send: report it
            // and drop it before any further await so the future stays Send.
            let failed = match crate::llm::config::refresh_models_cache_async().await {
                Ok(()) => false,
                Err(e) => {
                    eprintln!("note: models.dev catalog fetch failed ({e}); retrying in 5 minutes");
                    true
                }
            };
            if failed {
                tokio::time::sleep(Duration::from_secs(5 * 60)).await;
                if let Err(e) = crate::llm::config::refresh_models_cache_async().await {
                    eprintln!(
                        "note: models.dev catalog still missing ({e}); run `dex update --models`"
                    );
                }
            }
        });
    }

    let app = server::router(state.clone());

    // Note: no startup announcement here. The headless `dex serve` caller
    // prints one; the embedded (`dex` default) daemon shares the process
    // with the TUI and must stay silent — anything printed before the
    // alt-screen is entered lingers in scrollback after quit and reads as
    // if a daemon were still listening.

    // tokio refuses blocking fds; the std listener must be non-blocking
    // before registration.
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;

    // Headless exit path: ctrl-C stops serving, cancels + joins every
    // session's children (§14: no orphaned tokio task survives this exit),
    // then exits. A spawned watcher — not `with_graceful_shutdown` — so a
    // still-connected SSE client cannot hold the process open while it
    // drains; once children are joined nothing is lost by exiting hard.
    {
        let state_for_exit = state.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                state_for_exit.shutdown_agents().await;
                std::process::exit(0);
            }
        });
    }

    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::subagent::{
        AgentDefinition, AgentResult, AgentState, ContextSeed, ExitReason, ProgressReporter,
        SpawnMeta, WaitOutcome,
    };
    use crate::runtime::console::CancellationToken;

    fn agent_test_parts(name: &str) -> (AgentDefinition, ContextSeed) {
        let mut def = crate::agent::subagent::builtin_definitions()
            .into_iter()
            .next()
            .expect("built-in agents");
        def.name = name.to_string();
        let seed = ContextSeed {
            task: "do the thing".to_string(),
            file_hints: Vec::new(),
            parent_summary: None,
        };
        (def, seed)
    }

    async fn done_body(
        _token: CancellationToken,
        _progress: ProgressReporter,
        _id: crate::agent::subagent::AgentId,
    ) -> AgentResult {
        AgentResult {
            status: AgentState::Completed,
            summary: "done".to_string(),
            error: None,
            usage: None,
            reason: ExitReason::Normal,
            tool_calls: 0,
            resume: None,
        }
    }

    async fn cancel_body(
        token: CancellationToken,
        _progress: ProgressReporter,
        _id: crate::agent::subagent::AgentId,
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

    #[test]
    fn idempotency_key_replays_same_turn_and_rejects_different_request() {
        let state = DaemonState::new();
        state.idempotency_record("key-1", "sess-1", 42, "{\"seq\":9}".into());
        // Same session + same request hash: replay.
        assert_eq!(
            state.idempotent_replay("key-1", "sess-1", 42),
            Some("{\"seq\":9}".to_string())
        );
        // Different request hash under the same key: do NOT replay (a key
        // cannot launder a different prompt).
        assert_eq!(state.idempotent_replay("key-1", "sess-1", 43), None);
        // Different session under the same key: no replay.
        assert_eq!(state.idempotent_replay("key-1", "sess-2", 42), None);
        // Unknown key: no replay.
        assert_eq!(state.idempotent_replay("nope", "sess-1", 42), None);
    }

    #[test]
    fn event_seq_allocates_monotonically_per_session() {
        let state = DaemonState::new();
        assert_eq!(state.next_seq("a"), 0);
        assert_eq!(state.next_seq("a"), 1);
        assert_eq!(state.next_seq("b"), 0);
        assert_eq!(state.next_seq("a"), 2);
    }

    #[test]
    fn wake_slot_holds_one_wake_per_session_and_frees_on_cancel() {
        // §10b V1b: one wake at a time — a second claim loses; the steal
        // path frees the slot and cancels the loser's token.
        let state = DaemonState::new();
        let first = state.claim_wake("s").expect("first claim wins");
        assert!(state.claim_wake("s").is_none(), "one at a time");
        assert!(state.claim_wake("other").is_some(), "sessions are separate");
        let stolen = state.cancel_wake("s").expect("steal finds the wake");
        assert!(
            stolen.is_cancelled(),
            "a stolen wake must stop; the claim returns the same token"
        );
        assert!(first.is_cancelled(), "a stolen wake must stop");
        assert!(state.cancel_wake("s").is_none(), "already removed");
        assert!(state.claim_wake("s").is_some(), "slot freed");
    }

    #[test]
    fn client_seen_presence_needs_a_recent_journal_read() {
        // §10b V1b presence gate: no read → no audience; a read inside the
        // window counts; a read older than the window does not.
        let state = DaemonState::new();
        assert!(!state.client_seen_fresh("s", std::time::Duration::from_secs(30)));
        state.touch_client_seen("s");
        assert!(state.client_seen_fresh("s", std::time::Duration::from_secs(30)));
        assert!(!state.client_seen_fresh("s", std::time::Duration::ZERO));
    }

    #[tokio::test]
    async fn take_agent_pendings_takes_only_session_children() {
        // The counterpart of `take_session_pendings`: parent approvals stay
        // (their turn owns them); children leave so their parked prompts
        // deny when the session goes away.
        use crate::core::types::ApprovalDecision;
        let state = DaemonState::new();
        let (tx_parent, mut rx_parent) = tokio::sync::mpsc::channel(1);
        let (tx_child, mut rx_child) = tokio::sync::mpsc::channel(1);
        let (tx_other, mut rx_other) = tokio::sync::mpsc::channel(1);
        {
            let mut pending = state.pending_approvals.lock().unwrap();
            pending.insert(
                "p".into(),
                PendingApproval {
                    session_id: "s".into(),
                    response: tx_parent,
                    name: "write".into(),
                    input: "{}".into(),
                    agent_id: None,
                    agent: None,
                },
            );
            pending.insert(
                "c".into(),
                PendingApproval {
                    session_id: "s".into(),
                    response: tx_child,
                    name: "bash".into(),
                    input: "{}".into(),
                    agent_id: Some("s-0".into()),
                    agent: Some("tester".into()),
                },
            );
            pending.insert(
                "o".into(),
                PendingApproval {
                    session_id: "other".into(),
                    response: tx_other,
                    name: "bash".into(),
                    input: "{}".into(),
                    agent_id: Some("other-0".into()),
                    agent: Some("tester".into()),
                },
            );
        }

        let taken = state.take_agent_pendings(Some("s"));
        assert_eq!(taken.len(), 1, "only the session's child approval");
        let _ = taken[0].send(ApprovalDecision::Deny).await;
        assert_eq!(rx_child.try_recv().ok(), Some(ApprovalDecision::Deny));
        {
            let pending = state.pending_approvals.lock().unwrap();
            assert!(pending.contains_key("p"), "parent approval is turn-owned");
            assert!(pending.contains_key("o"), "other session untouched");
        }
        let all = state.take_agent_pendings(None);
        assert_eq!(all.len(), 1, "shutdown sweeps the remaining child");
        assert!(rx_parent.try_recv().is_err());
        assert!(rx_other.try_recv().is_err());
    }

    #[test]
    fn fresh_state_has_no_agent_managers() {
        // Restart-empty: no child registries until first delegate.
        let state = std::sync::Arc::new(DaemonState::new());
        assert!(lock_map(&state.agents).is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn manager_for_is_shared_per_session_and_isolated_across() {
        let state = std::sync::Arc::new(DaemonState::new());
        let (def, seed) = agent_test_parts("explorer");
        // A child spawned through one handle is visible through another
        // handle for the same session: clones share one registry.
        let id = state
            .manager_for("s1")
            .spawn(&def, seed, SpawnMeta::fresh(), done_body)
            .unwrap();
        match state
            .manager_for("s1")
            .wait(&id, std::time::Duration::from_secs(5))
            .await
        {
            WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Completed),
            other => panic!("expected Finished, got {other:?}"),
        }
        // Other sessions are isolated: unknown id, fresh counter.
        assert_eq!(state.manager_for("s2").status(&id), None);
        let (def2, seed2) = agent_test_parts("explorer");
        let other = state
            .manager_for("s2")
            .spawn(&def2, seed2, SpawnMeta::fresh(), done_body)
            .unwrap();
        assert_eq!(other.to_string(), "s2-0");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remove_session_agents_cancels_children_and_drops_manager() {
        let state = std::sync::Arc::new(DaemonState::new());
        let manager = state.manager_for("s1");
        let (def, seed) = agent_test_parts("explorer");
        let id = manager
            .spawn(&def, seed, SpawnMeta::fresh(), cancel_body)
            .unwrap();
        assert_eq!(manager.active_count(), 1);
        state.remove_session_agents("s1").await;
        // The pre-removal handle still sees the removed child (shared
        // registry), now terminal through the Cancelled path.
        match manager.wait(&id, std::time::Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled)
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(manager.active_count(), 0);
        // A fresh lookup starts empty; removing an unknown session is a no-op.
        assert_eq!(state.manager_for("s1").active_count(), 0);
        state.remove_session_agents("missing").await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_agents_joins_every_session() {
        let state = Arc::new(DaemonState::new());
        let first = state.manager_for("s1");
        let second = state.manager_for("s2");
        let (def1, seed1) = agent_test_parts("explorer");
        let (def2, seed2) = agent_test_parts("tester");
        first
            .spawn(&def1, seed1, SpawnMeta::fresh(), cancel_body)
            .unwrap();
        second
            .spawn(&def2, seed2, SpawnMeta::fresh(), cancel_body)
            .unwrap();
        state.shutdown_agents().await;
        assert_eq!(first.active_count(), 0);
        assert_eq!(second.active_count(), 0);
        assert!(lock_map(&state.agents).is_empty());
    }

    #[test]
    fn event_seq_is_seeded_from_disk_after_restart() {
        // Touches the shared sessions dir; serialize against env-redirecting tests.
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        // Simulate a prior run: a session with events already journaled.
        let mut s = crate::session::Session::new("/tmp/dex-seq-test".into(), None).unwrap();
        s.append_event(0, "{\"type\":\"system\",\"data\":\"x\"}")
            .unwrap();
        s.append_event(1, "{\"type\":\"system\",\"data\":\"y\"}")
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let state = DaemonState::new();
        state.seed_seq(s.id(), &path);
        // The next allocation continues after the journal, not from zero.
        assert_eq!(state.next_seq(s.id()), 2);
        let _ = std::fs::remove_file(&path);
    }

    /// Redirect `XDG_DATA_HOME` to a pid-unique dir for the guard's lifetime.
    /// `rebuild_async` scans the whole sessions dir; without this, concurrent
    /// test binaries (same fixed /tmp session names) mark each other's live
    /// sessions as failed. Additive test infra — no assertion changes.
    /// Caller MUST hold `TEST_SESSIONS_ENV_LOCK` (env is process-global);
    /// every user of this helper does.
    struct HermeticXdg(Option<std::ffi::OsString>, std::path::PathBuf);
    impl Drop for HermeticXdg {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.1);
        }
    }
    fn hermetic_xdg() -> HermeticXdg {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dex-daemon-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let prev = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("XDG_DATA_HOME", &dir);
        HermeticXdg(prev, dir)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_marks_interrupted_turns_failed_and_registers_sessions() {
        // Hermetic sessions dir: rebuild_async scans everything under
        // XDG_DATA_HOME, so concurrent test binaries must not see each other.
        let _env_guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let _xdg_guard = hermetic_xdg();
        // A session killed mid-turn: turn_start with no terminal entry.
        let mut s = crate::session::Session::new("/tmp/dex-rebuild-test".into(), None).unwrap();
        let id = s.id().to_string();
        s.turn_event("turn_start").unwrap();
        assert_eq!(
            crate::session::Session::last_turn_state(s.path().unwrap()),
            "interrupted"
        );
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        state.rebuild_async().await;
        // The session is in the registry after restart.
        {
            let sessions = state.sessions.lock().unwrap();
            assert!(
                sessions.contains_key(&id),
                "registry must be rebuilt from disk"
            );
        }
        // The interrupted turn is now durably failed.
        assert_eq!(crate::session::Session::last_turn_state(&path), "failed");
        assert!(state.rebuild_complete.load(Ordering::Relaxed));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn event_seq_seed_advances_past_a_single_seq_zero() {
        // `max_event_seq` is None when empty but Some(0) for a journal
        // holding exactly seq 0; the seed must not reuse seq 0.
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let mut s = crate::session::Session::new("/tmp/dex-seq-zero-test".into(), None).unwrap();
        s.append_event(0, "{\"type\":\"system\",\"data\":\"x\"}")
            .unwrap();
        let path = s.path().unwrap().to_path_buf();
        let state = DaemonState::new();
        state.seed_seq(s.id(), &path);
        assert_eq!(state.next_seq(s.id()), 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("events.jsonl"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_skips_failed_marking_for_live_turns() {
        // A reattach + chat racing the background rebuild owns the journal:
        // stamping `turn_failed` under its live `turn_start` would corrupt it.
        // Hermetic sessions dir — see rebuild_marks_interrupted_turns_failed.
        let _env_guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let _xdg_guard = hermetic_xdg();
        let mut s =
            crate::session::Session::new("/tmp/dex-rebuild-live-test".into(), None).unwrap();
        let id = s.id().to_string();
        s.turn_event("turn_start").unwrap();
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        state.active_turns.lock().unwrap().insert(id.clone());
        state.rebuild_async().await;
        // Still registered, but the live turn is untouched.
        assert!(state.sessions.lock().unwrap().contains_key(&id));
        assert_eq!(
            crate::session::Session::last_turn_state(&path),
            "interrupted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn rebuild_registry_merge_keeps_live_entries() {
        // Sessions claimed (reattached/created) mid-rebuild win over disk via
        // `or_insert` — the rebuild must not clobber them.
        // Hermetic sessions dir — see rebuild_marks_interrupted_turns_failed.
        let _env_guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let _xdg_guard = hermetic_xdg();
        let s = crate::session::Session::new("/tmp/dex-rebuild-wins-test".into(), None).unwrap();
        let id = s.id().to_string();
        let path = s.path().unwrap().to_path_buf();
        drop(s);

        let state = DaemonState::new();
        let live = SessionEntry {
            path: std::path::PathBuf::from("/tmp/dex-live-wins-marker"),
            name: None,
            cwd: "/tmp".into(),
            model: None,
            wake_provider: None,
            wake_base_url: None,
            plan_persisted: None,
            model_persisted: None,
        };
        state.sessions.lock().unwrap().insert(id.clone(), live);
        state.rebuild_async().await;
        assert_eq!(
            state.sessions.lock().unwrap().get(&id).unwrap().path,
            std::path::PathBuf::from("/tmp/dex-live-wins-marker")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A non-loopback bind without an explicit token generates one, publishes
    /// it to `daemon.token` (0600, atomically) and the server then requires
    /// it on `/api/*` while `/health` stays open. Panic-safe env restore via
    /// Drop guards; global token is re-resolvable per case (no `OnceLock`
    /// poisoning).
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn bearer_token_generated_for_non_loopback_bind_and_enforced() {
        struct Restore {
            xdg: Option<std::ffi::OsString>,
            token: Option<std::ffi::OsString>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.xdg.take() {
                    Some(v) => std::env::set_var("XDG_DATA_HOME", v),
                    None => std::env::remove_var("XDG_DATA_HOME"),
                }
                match self.token.take() {
                    Some(v) => std::env::set_var("DEX_DAEMON_TOKEN", v),
                    None => std::env::remove_var("DEX_DAEMON_TOKEN"),
                }
                reset_daemon_token_for_tests();
            }
        }
        let _guard = lock_map(&crate::session::TEST_SESSIONS_ENV_LOCK);
        let _restore = Restore {
            xdg: std::env::var_os("XDG_DATA_HOME"),
            token: std::env::var_os("DEX_DAEMON_TOKEN"),
        };
        let dir = std::env::temp_dir().join(format!("dex-token-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &dir);
        std::env::remove_var("DEX_DAEMON_TOKEN");
        // Loopback needs no token; explicit env wins even on loopback.
        let loopback: std::net::SocketAddr = "127.0.0.1:8420".parse().unwrap();
        prepare_daemon_token(&loopback);
        assert_eq!(required_token(), None, "loopback stays unauthenticated");
        std::env::set_var("DEX_DAEMON_TOKEN", "env-wins");
        prepare_daemon_token(&loopback);
        assert_eq!(
            required_token().as_deref(),
            Some("env-wins"),
            "explicit DEX_DAEMON_TOKEN wins even on loopback"
        );
        std::env::remove_var("DEX_DAEMON_TOKEN");
        let addr: std::net::SocketAddr = "203.0.113.7:8420".parse().unwrap();
        prepare_daemon_token(&addr);
        let required = required_token();
        let token = required.expect("non-loopback bind must generate a token");
        let token_path = dir.join("dex/daemon.token");
        let written = std::fs::read_to_string(&token_path)
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(token, written, "client must read the same credential");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&token_path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must not be group/world readable"
            );
        }

        // Serve one route and check enforcement.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let app = server::router(std::sync::Arc::new(DaemonState::new()));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let http = crate::client::http::shared_async_client();
        // /health stays open (liveness before any credential).
        let health = http
            .get(format!("http://{bound}/health"))
            .send()
            .await
            .unwrap();
        assert!(health.status().is_success());
        // /api without the token: 401 (before the handler runs).
        let denied = http
            .post(format!("http://{bound}/api/sessions"))
            .json(&serde_json::json!({"cwd": "/tmp"}))
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), reqwest::StatusCode::UNAUTHORIZED);
        // With the token: accepted (session created).
        let allowed = http
            .post(format!("http://{bound}/api/sessions"))
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(&serde_json::json!({"cwd": "/tmp", "name": "tok-test"}))
            .send()
            .await
            .unwrap();
        assert!(allowed.status().is_success(), "{}", allowed.status());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
