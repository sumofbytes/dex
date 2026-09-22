// binary), following the `llm::config::tests` precedent.
use super::*;

use crate::client::sse::SseFramer;

#[test]
fn mcp_auth_lines_skips_null_stdio() {
    let body = serde_json::json!({"servers": [
        {"name": "gh", "auth": "gh: logged in"},
        {"name": "local", "auth": null},
        {"name": "nope"},
    ]});
    assert_eq!(mcp_auth_lines(&body), vec!["gh: logged in".to_string()]);
    assert!(mcp_auth_lines(&serde_json::json!({})).is_empty());
}

#[tokio::test]
async fn wait_until_ready_async_errors_after_timeout() {
    // TDD Phase 5: async sleep, not thread::sleep — unreachable daemon
    // must error within the deadline without parking a thread.
    let client = DaemonClient::new("http://127.0.0.1:9").unwrap();
    let start = Instant::now();
    let err = client
        .wait_until_ready_async(Duration::from_millis(120))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("did not become ready"));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn parse_sse_line_skips_keepalives_and_junk() {
    assert!(DaemonClient::parse_sse_line("").is_none());
    assert!(DaemonClient::parse_sse_line("\n").is_none());
    assert!(DaemonClient::parse_sse_line(": comment").is_none());
    assert!(DaemonClient::parse_sse_line("event: message").is_none());
    assert!(DaemonClient::parse_sse_line("data:").is_none());
    assert!(DaemonClient::parse_sse_line("data: ping").is_none());
    assert!(DaemonClient::parse_sse_line("data: not-json").is_none());
    let env = StreamEnvelope {
        seq: 1,
        event: StreamEvent::AssistantText("hi".to_string()),
    };
    let line = format!("data: {}", serde_json::to_string(&env).unwrap());
    assert!(matches!(
        DaemonClient::parse_sse_line(&line),
        Some(StreamEvent::AssistantText(t)) if t == "hi"
    ));
}

/// The framer tracks the highest envelope seq so a reconnect can resume
/// the journal from exactly where the stream died.
#[test]
fn sse_framer_tracks_the_replay_cursor() {
    let env1 = StreamEnvelope {
        seq: 3,
        event: StreamEvent::AssistantText("a".to_string()),
    };
    let env2 = StreamEnvelope {
        seq: 7,
        event: StreamEvent::System("b".to_string()),
    };
    let mut framer = SseFramer::default();
    framer.push_bytes(format!("data: {}\n", serde_json::to_string(&env1).unwrap()).as_bytes());
    framer.push_bytes(format!("data: {}\n", serde_json::to_string(&env2).unwrap()).as_bytes());
    framer.push_bytes(b"data: ping\n");
    framer.push_bytes(b"data: not-json\n");
    assert_eq!(framer.next_seq, 8);
    assert_eq!(framer.pending.len(), 2);
    framer.finish();
    assert_eq!(framer.next_seq, 8);
}

/// The supervision removal deleted `StreamEvent::AgentRecovered`: an
/// old journal row carrying that type still advances the cursor
/// without yielding an event, so replay never stalls on it.
#[test]
fn sse_framer_skips_the_removed_agent_recovered_type() {
    let mut framer = SseFramer::default();
    framer.push_bytes(
        b"data: {\"seq\":9,\"agent_recovered\":{\"agent_id\":\"sess-0\",\"status\":\"resumed\"}}\n",
    );
    assert_eq!(framer.next_seq, 10);
    assert!(framer.pending.is_empty());
}

#[test]
fn sse_framer_reassembles_split_lines_and_trailing_terminal() {
    // One envelope split across TCP chunks reassembles; keep-alives and
    // junk around it are skipped; the terminal envelope without a
    // trailing newline surfaces via `finish()`.
    let assistant = StreamEnvelope {
        seq: 1,
        event: StreamEvent::AssistantText("hello".to_string()),
    };
    let complete = StreamEnvelope {
        seq: 2,
        event: StreamEvent::TurnComplete {
            response: "done".to_string(),
            usage: None,
            cached: None,
        },
    };
    let first = format!("data: {}\n\n", serde_json::to_string(&assistant).unwrap());
    let terminal = format!("data: {}", serde_json::to_string(&complete).unwrap());
    let mut framer = SseFramer::default();
    // Split mid-payload: nothing complete yet (chunks are contiguous on
    // the wire, so the halves arrive back-to-back).
    let (head, tail) = first.as_bytes().split_at(first.len() / 2);
    framer.push_bytes(head);
    assert!(framer.pending.is_empty());
    framer.push_bytes(tail);
    // Junk around real lines is skipped.
    framer.push_bytes(b"data: ping\n\n\ndata: bogus{\n\n");
    assert!(matches!(
        framer.pending.pop_front(),
        Some(StreamEvent::AssistantText(t)) if t == "hello"
    ));
    assert!(framer.pending.is_empty());
    // Trailing terminal without newline is held until `finish()`.
    framer.push_bytes(terminal.as_bytes());
    assert!(framer.pending.is_empty());
    framer.finish();
    assert!(matches!(
        framer.pending.pop_front(),
        Some(StreamEvent::TurnComplete { response, .. }) if response == "done"
    ));
}

fn sse_data(event: &StreamEvent, seq: u64) -> String {
    let env = StreamEnvelope {
        seq,
        event: event.clone(),
    };
    format!("data: {}\n\n", serde_json::to_string(&env).unwrap())
}

/// Minimal mock daemon: streams `chat_body` for every chat POST and
/// records approval decisions. No new deps (axum is already one).
async fn mock_chat_server(
    chat_body: String,
    chat_status: u16,
    approvals: std::sync::Arc<std::sync::Mutex<Vec<ApprovalDecision>>>,
) -> String {
    use axum::{routing::post, Json, Router};
    let app = Router::new()
        .route(
            "/api/sessions/{id}/chat",
            post(move || {
                let body = chat_body.clone();
                async move {
                    (
                        axum::http::StatusCode::from_u16(chat_status).unwrap(),
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        body,
                    )
                }
            }),
        )
        .route(
            "/api/sessions/{id}/approve",
            post(move |Json(req): Json<ApprovalResponse>| {
                let approvals = approvals.clone();
                async move {
                    approvals.lock().unwrap().push(req.decision);
                    Json(serde_json::json!({"status": "ok"}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn chat_stream_delivers_events_and_posts_approval() {
    // Regression for the stuck-`Working ...` hang: the TUI worker drives
    // this stream with `send().await` / `recv().await` only. The old
    // worker called `blocking_send` / `blocking_recv` inside `block_on`,
    // which panics ("Cannot block the current thread from within a
    // runtime"), so no event ever reached the transcript.
    let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let complete = StreamEnvelope {
        seq: 3,
        event: StreamEvent::TurnComplete {
            response: "done".to_string(),
            usage: None,
            cached: None,
        },
    };
    let body = format!(
        "\ndata: ping\n\ndata: not-json\n\n{}{}data: {}",
        sse_data(&StreamEvent::AssistantText("hello".to_string()), 1),
        sse_data(
            &StreamEvent::ApprovalRequired {
                request_id: "r1".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                agent: None,
            },
            2
        ),
        serde_json::to_string(&complete).unwrap(), // no trailing newline
    );
    let base = mock_chat_server(body, 200, approvals.clone()).await;
    let client = DaemonClient::new(&base).unwrap();
    let fut = async {
        let mut stream = client
            .chat_stream("s1", "hi", ChatOptions::default())
            .await
            .expect("POST must succeed");
        let mut kinds = Vec::new();
        while let Some(item) = stream.next_event().await {
            let event = item.expect("transport must not fail");
            if let StreamEvent::ApprovalRequired { request_id, .. } = &event {
                client
                    .approve_async("s1", request_id, ApprovalDecision::AllowOnce)
                    .await
                    .expect("approve POST must succeed");
            }
            kinds.push(match &event {
                StreamEvent::AssistantText(_) => "text",
                StreamEvent::ApprovalRequired { .. } => "approval",
                StreamEvent::TurnComplete { .. } => "complete",
                _ => "other",
            });
        }
        kinds
    };
    let kinds = tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("stream must terminate, not hang");
    assert_eq!(kinds, vec!["text", "approval", "complete"]);
    assert_eq!(
        *approvals.lock().unwrap(),
        vec![ApprovalDecision::AllowOnce]
    );
}

#[tokio::test]
async fn chat_stream_surfaces_transport_error() {
    let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let base = mock_chat_server(String::new(), 500, approvals).await;
    let client = DaemonClient::new(&base).unwrap();
    let err = match client.chat_stream("s1", "hi", ChatOptions::default()).await {
        Ok(_) => panic!("500 must fail"),
        Err(e) => e,
    };
    assert!(!err.to_string().is_empty());
}

#[tokio::test]
async fn chat_async_callback_path_still_forwards_and_approves() {
    // The sync-callback API (one-shot CLI, repl, e2e) shares the same
    // `ChatStream` framing; approvals resolve via the callback return.
    let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let body = format!(
        "{}{}",
        sse_data(&StreamEvent::AssistantText("hi".to_string()), 1),
        sse_data(
            &StreamEvent::ApprovalRequired {
                request_id: "r9".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                agent: None,
            },
            2
        )
    );
    let base = mock_chat_server(body, 200, approvals.clone()).await;
    let client = DaemonClient::new(&base).unwrap();
    let mut seen = Vec::new();
    client
        .chat_async("s1", "hi", ChatOptions::default(), &mut |event| {
            let decision = matches!(event, StreamEvent::ApprovalRequired { .. })
                .then_some(ApprovalDecision::Deny);
            seen.push(matches!(event, StreamEvent::ApprovalRequired { .. }));
            decision
        })
        .await
        .expect("chat must succeed");
    assert_eq!(seen, vec![false, true]);
    assert_eq!(*approvals.lock().unwrap(), vec![ApprovalDecision::Deny]);
}

#[tokio::test]
async fn extensions_run_forwards_name_and_surfaces_404() {
    use axum::{routing::post, Json, Router};
    let app = Router::new()
        .route(
            "/api/extensions/run",
            post(
                |Json(req): Json<crate::protocol::ExtensionRunRequest>| async move {
                    if req.name == "known" {
                        Json(serde_json::json!({"extension": "ext1", "output": "ok"}))
                    } else {
                        // Mirror the daemon: unknown names are 404.
                        Json(serde_json::json!({"error": "not found"}))
                    }
                },
            ),
        )
        .route(
            "/api/extensions/run-404",
            post(|| async { (axum::http::StatusCode::NOT_FOUND, "nope") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = DaemonClient::new(&base).unwrap();
    let body = client.extensions_run_async("known", "arg").await.unwrap();
    assert_eq!(body["output"].as_str(), Some("ok"));
    // A real 404 surfaces as an error containing 404 so the remote TUI
    // can fall back to "unknown command".
    let err = client
        .http
        .post(format!("{base}/api/extensions/run-404"))
        .headers(client.api_headers())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();
    assert!(err.to_string().contains("404"));
}

#[tokio::test]
async fn chat_stream_carries_thinking_override() {
    use axum::{routing::post, Json, Router};
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<crate::protocol::ChatRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen.clone();
    let app = Router::new().route(
        "/api/sessions/{id}/chat",
        post(move |Json(req): Json<crate::protocol::ChatRequest>| {
            let seen = seen_clone.clone();
            async move {
                seen.lock().unwrap().push(req);
                (
                    axum::http::StatusCode::from_u16(200).unwrap(),
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    String::new(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = DaemonClient::new(&base).unwrap();
    let options = ChatOptions {
        thinking_effort: Some("high".into()),
        ..Default::default()
    };
    let mut stream = client.chat_stream("s1", "hi", options).await.unwrap();
    while stream.next_event().await.is_some() {}
    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].thinking_effort.as_deref(), Some("high"));
}

/// Spawn a router on a loopback port from sync code; returns the base
/// URL. Shared with the `client/repl.rs` tests.
pub(crate) fn spawn_daemon_sync(app: axum::Router) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            tx.send(listener.local_addr().unwrap().to_string()).ok();
            axum::serve(listener, app).await.unwrap();
        });
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map(|addr| format!("http://{addr}"))
        .expect("daemon address")
}

/// Drive the real daemon through every `DaemonClient` method over real
/// HTTP: config/git/skills/mcp reads, session lifecycle (create, rename,
/// waive, undo, reattach, events, trace), shell runs, queue errors on an
/// idle session, approval 404s, and an SSE chat turn against a fake
/// chat-completions provider. Runs fully synchronously: the sync
/// wrappers `block_on` the async bodies, so both are exercised.
#[test]
fn client_end_to_end_hits_every_endpoint() {
    let _guard = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    const DONE_SSE: &str =
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";
    let fake_llm = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(|| async {
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(DONE_SSE.to_string()))
                .unwrap()
        }),
    );
    let llm_base = spawn_daemon_sync(fake_llm);
    let daemon_base = spawn_daemon_sync(crate::daemon::server::router(std::sync::Arc::new(
        crate::daemon::DaemonState::new(),
    )));

    let data_dir = std::env::temp_dir().join(format!("dex-http-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "DEX_CONFIG",
        "OPENCODE_API_KEY",
        "DEX_PERMISSION",
        "DEX_VERIFY",
        "DEX_MODELS",
        "DEX_MODEL_APIS",
        "DEX_CONTEXT_WINDOW",
        "DEX_THINKING_EFFORT",
    ]
    .iter()
    .map(|k| (*k, std::env::var_os(k)))
    .collect();
    let _env = crate::session::EnvGuard(saved);
    std::env::set_var("XDG_DATA_HOME", &data_dir);
    std::env::set_var("XDG_CACHE_HOME", data_dir.join("cache"));
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
            data_dir.join("config.yaml"),
            format!("model: opencode/test-model\ncontext_window: 100000\nbase_url: {llm_base}\napi: openai-completions\nproviders:\n  opencode:\n    api_key: test-key\n"),
        )
        .unwrap();
    std::env::set_var("DEX_CONFIG", data_dir.join("config.yaml"));
    std::env::set_var("OPENCODE_API_KEY", "test-key");
    std::env::set_var("DEX_PERMISSION", "ask-writes");
    std::env::set_var("DEX_VERIFY", "false");
    for v in [
        "DEX_MODELS",
        "DEX_MODEL_APIS",
        "DEX_CONTEXT_WINDOW",
        "DEX_THINKING_EFFORT",
    ] {
        std::env::remove_var(v);
    }

    let client = DaemonClient::new(&daemon_base).unwrap();
    client.wait_until_ready(Duration::from_secs(10)).unwrap();

    // Meta reads.
    let info = client.get_config().unwrap();
    assert_eq!(info.provider, "opencode");
    let _git = client.get_git().unwrap();
    assert!(client.list_skills().is_ok());
    let mcp = client.mcp_status().unwrap();
    assert!(mcp["servers"].is_array());

    // Session lifecycle.
    let session_id = client
        .create_session("/tmp/dex-http-e2e-cwd", Some("e2e"))
        .unwrap()
        .session_id;
    let sessions = client.list_sessions().unwrap();
    assert!(
        sessions.iter().any(|s| s.session_id == session_id),
        "created session is listed: {sessions:?}"
    );
    client.rename_session(&session_id, "renamed").unwrap();

    // Session reads + write-side endpoints.
    assert_eq!(client.reattach(&session_id).unwrap().session_id, session_id);
    let events = client.events(&session_id, 0).unwrap();
    assert!(events.events.is_empty(), "nothing journaled yet");
    assert!(client.trace(&session_id).unwrap().is_empty());
    assert!(client.undo(&session_id).is_err(), "no change to undo");
    client.waive(&session_id, "flaky env").unwrap();

    // Queues on an idle session are a 409: nothing is running.
    assert!(client.steer(&session_id, "mid").is_err());
    assert!(client.followup(&session_id, "later").is_err());
    assert!(client.recall(&session_id, "typo", false).is_err());
    // Approvals without a pending request are a 404.
    assert!(client
        .approve(&session_id, "missing", ApprovalDecision::Deny)
        .is_err());
    // Cancel is always a no-op-shaped Ok.
    client.cancel(&session_id).unwrap();

    // A failed skill load surfaces as an error (unknown name).
    assert!(client
        .load_skill(&session_id, "no-such-skill", &[])
        .is_err());

    // `!` runs through the client too.
    let shell = client.shell(&session_id, "echo http-e2e", false).unwrap();
    assert!(shell.success && shell.output.contains("http-e2e"));

    // SSE chat against the fake provider (async-native transport, driven
    // through the shared runtime).
    let mut stream = crate::runtime::http::block_on(client.chat_stream(
        &session_id,
        "hi",
        ChatOptions::default(),
    ))
    .unwrap();
    let mut last = None;
    while let Some(result) = crate::runtime::http::block_on(stream.next_event()) {
        last = Some(result.expect("stream event"));
    }
    assert!(
        matches!(last, Some(StreamEvent::TurnComplete { .. })),
        "stream ends with the terminal event: {last:?}"
    );
    assert!(stream.last_seq() > 0, "the cursor advances with the stream");
    // The callback transport reports the same outcome.
    crate::runtime::http::block_on(client.chat_async(
        &session_id,
        "again",
        ChatOptions::default(),
        &mut |_| None,
    ))
    .unwrap();

    // Journaled events now replay past the cursor.
    let events = client.events(&session_id, 0).unwrap();
    assert!(
        !events.events.is_empty() && events.next_seq > 0,
        "the chat turn journaled events: {}",
        events.events.len()
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}
