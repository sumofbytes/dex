// binary), following the `llm::config::tests` precedent.
use super::*;

use crate::sse::SseFramer;
use axum::response::IntoResponse;

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
        "{}{}{}",
        sse_data(&StreamEvent::AssistantText("hi".to_string()), 1),
        sse_data(
            &StreamEvent::ApprovalRequired {
                request_id: "r9".to_string(),
                name: "bash".to_string(),
                input: "{}".to_string(),
                agent: None,
            },
            2
        ),
        sse_data(
            &StreamEvent::TurnComplete {
                response: String::new(),
                usage: None,
                cached: None,
            },
            3
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
    assert_eq!(seen, vec![false, true, false]);
    assert_eq!(*approvals.lock().unwrap(), vec![ApprovalDecision::Deny]);
}

#[tokio::test]
async fn chat_async_reports_premature_close_without_terminal() {
    // A stream that closes without TurnComplete/TurnFailed is a transport
    // failure: callers must not record the turn as done.
    let approvals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let body = sse_data(&StreamEvent::AssistantText("hi".to_string()), 1);
    let base = mock_chat_server(body, 200, approvals).await;
    let client = DaemonClient::new(&base).unwrap();
    let err = client
        .chat_async("s1", "hi", ChatOptions::default(), &mut |_| None)
        .await;
    assert!(
        err.is_err(),
        "a close without a terminal event must be an error: {err:?}"
    );
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

fn spawn_stub(app: axum::Router) -> String {
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
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .map(|addr| format!("http://{addr}"))
        .expect("stub address")
}

#[test]
fn error_chain_message_walks_sources() {
    // reqwest-style: outer Display names the context, the cause lives in
    // `source()`. `to_string()` alone drops it; the chain keeps it.
    let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
    let chained = std::io::Error::new(std::io::ErrorKind::TimedOut, io);
    let msg = error_chain_message(&chained);
    assert!(msg.contains("connection refused"), "chain kept: {msg}");
}

#[test]
fn default_constructor_resolves_token_and_sync_wrappers() {
    const TOKEN: &str = "crate-e2e-token";
    std::env::set_var("DEX_DAEMON_TOKEN", TOKEN);
    let require_bearer = axum::routing::get(|headers: axum::http::HeaderMap| async move {
        if headers.get("authorization").and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {TOKEN}"))
        {
            return axum::http::StatusCode::UNAUTHORIZED.into_response();
        }
        axum::Json(serde_json::json!({
            "provider": "stub",
            "model": "stub-model",
            "available_models": ["stub-model"],
            "context_window": 128000,
            "permission": "trusted",
            "cwd": "/",
            "git_branch": null,
            "git_dirty": false,
        }))
        .into_response()
    });
    let app = axum::Router::new()
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route("/api/config", require_bearer);
    let base = spawn_stub(app);

    // `new` exercises the crate-default path end to end: local token lookup
    // (env var), the crate's own HTTP pools, and the default stderr warnings.
    let client = DaemonClient::new(&base).unwrap();
    client
        .wait_until_ready(std::time::Duration::from_secs(5))
        .unwrap();
    let info = client.get_config().unwrap();
    assert_eq!(info.model, "stub-model");
    assert_eq!(info.provider, "stub");
    std::env::remove_var("DEX_DAEMON_TOKEN");

    // File fallback: `$XDG_DATA_HOME/dex/daemon.token` trims whitespace.
    let data = std::env::temp_dir().join(format!("dex-client-e2e-{}", std::process::id()));
    std::fs::create_dir_all(data.join("dex")).unwrap();
    std::fs::write(data.join("dex/daemon.token"), "file-token\n").unwrap();
    let saved_xdg = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_DATA_HOME", &data);
    assert_eq!(
        crate::auth::client_daemon_token().as_deref(),
        Some("file-token")
    );
    match saved_xdg {
        Some(v) => std::env::set_var("XDG_DATA_HOME", v),
        None => std::env::remove_var("XDG_DATA_HOME"),
    }
    assert!(std::fs::remove_dir_all(&data).is_ok());
}

#[test]
fn approval_delivery_failure_warns_through_warning_handler() {
    const SSE: &str = concat!(
        "data: {\"seq\":0,\"type\":\"approval_required\",\"data\":",
        "{\"request_id\":\"r1\",\"name\":\"bash\",\"input\":\"ls\"}}\n\n",
        "data: {\"seq\":1,\"type\":\"turn_complete\",\"data\":{\"response\":\"done\"}}\n\n",
    );
    let app = axum::Router::new()
        .route(
            "/api/sessions/s1/chat",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    SSE,
                )
            }),
        )
        .route(
            "/api/sessions/s1/approve",
            axum::routing::post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
    let base = spawn_stub(app);

    let warnings: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let sink = warnings.clone();
    let client = DaemonClient::with_token(&base, None)
        .unwrap()
        .with_warning_handler(move |message| sink.lock().unwrap().push(message.to_string()));

    let mut on_event = |event: StreamEvent| match &event {
        StreamEvent::ApprovalRequired { .. } => Some(crate::protocol::ApprovalDecision::AllowOnce),
        _ => None,
    };
    client
        .chat("s1", "hi", ChatOptions::default(), &mut on_event)
        .unwrap();
    let captured = warnings.lock().unwrap();
    assert!(
        captured
            .iter()
            .any(|w| w.starts_with("daemon approval delivery failed:")),
        "warning captured: {captured:?}"
    );
}
