use crate::client::{ChatOptions, DaemonClient};
use crate::protocol::{ApprovalDecision, StreamEvent};
use std::time::Duration;

fn spawn_daemon_sync(app: axum::Router) -> String {
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
