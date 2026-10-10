//! ACP agent: request routing and the prompt-turn loop.

use crate::map::{self, ToolIds};
use crate::rpc::{self, Rpc};
use dex_client::protocol::{QuestionAnswer, StreamEvent};
use dex_client::{ChatOptions, DaemonClient};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::sync::Notify;

type RpcError = (i64, String);

/// How long a cancelled prompt waits for the daemon to wind the turn down
/// before returning `cancelled` anyway.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

struct Session {
    mode: Mutex<String>,
    cancelled: AtomicBool,
    cancel_signal: Notify,
    /// A prompt turn is in flight; overlapping prompts are rejected.
    busy: AtomicBool,
}

struct Ctx {
    rpc: Rpc,
    client: DaemonClient,
    options: ChatOptions,
    version: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

impl Ctx {
    fn session(&self, params: &Value) -> Result<(String, Arc<Session>), RpcError> {
        let id = params["sessionId"].as_str().unwrap_or_default();
        self.sessions
            .lock()
            .ok()
            .and_then(|s| s.get(id).cloned())
            .map(|s| (id.to_string(), s))
            .ok_or_else(|| (rpc::INVALID_PARAMS, format!("unknown session '{id}'")))
    }

    /// Register a fresh ACP session state for a daemon session id and seed
    /// its mode. Used by both `session/new` and `session/load`.
    fn register(&self, id: String, mode: &str) -> Arc<Session> {
        let session = Arc::new(Session {
            mode: Mutex::new(mode.to_string()),
            cancelled: AtomicBool::new(false),
            cancel_signal: Notify::new(),
            busy: AtomicBool::new(false),
        });
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.insert(id, session.clone());
        }
        session
    }
}

/// Session mode seed: the flag-provided mode when it names a known ACP
/// mode, else `auto` (dex's trusted default: run tools without asking).
fn seeded_mode(options: &ChatOptions) -> String {
    options
        .mode
        .clone()
        .filter(|m| map::MODES.iter().any(|(id, ..)| id == m))
        .unwrap_or_else(|| "auto".to_string())
}

/// Serve ACP on the process's stdin/stdout. Stdout carries protocol frames
/// only; diagnostics must go to stderr.
pub async fn serve_stdio(
    client: DaemonClient,
    options: ChatOptions,
    version: &str,
) -> io::Result<()> {
    serve(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        client,
        options,
        version,
    )
    .await
}

/// Serve ACP over any line-oriented reader/writer pair until the reader
/// closes. `options` seeds every turn (model, base URL, headers, ...); the
/// ACP session mode overrides its `mode`.
pub async fn serve<R, W>(
    reader: R,
    writer: W,
    client: DaemonClient,
    options: ChatOptions,
    version: &str,
) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let ctx = Arc::new(Ctx {
        rpc: Rpc::new(writer),
        client,
        options,
        version: version.to_string(),
        sessions: Mutex::new(HashMap::new()),
    });
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            ctx.rpc.respond_err(Value::Null, -32700, "parse error");
            continue;
        };
        let id = message.get("id").cloned();
        match (message["method"].as_str(), id) {
            // Response to one of our requests.
            (None, Some(id)) => ctx.rpc.deliver(&id, &message),
            (Some(method), Some(id)) => {
                let (ctx, method) = (ctx.clone(), method.to_string());
                let params = message["params"].clone();
                tokio::spawn(async move {
                    match handle_request(&ctx, &method, params).await {
                        Ok(result) => ctx.rpc.respond(id, result),
                        Err((code, msg)) => ctx.rpc.respond_err(id, code, &msg),
                    }
                });
            }
            (Some(method), None) => handle_notification(&ctx, method, &message["params"]),
            (None, None) => {}
        }
    }
    ctx.rpc.close();
    Ok(())
}

fn handle_notification(ctx: &Arc<Ctx>, method: &str, params: &Value) {
    if method != "session/cancel" {
        return;
    }
    let Ok((id, session)) = ctx.session(params) else {
        return;
    };
    session.cancelled.store(true, Ordering::SeqCst);
    session.cancel_signal.notify_waiters();
    let client = ctx.client.clone();
    tokio::spawn(async move {
        let _ = client.cancel_async(&id).await;
    });
}

async fn handle_request(ctx: &Arc<Ctx>, method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": map::PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false},
            },
            "agentInfo": {"name": "dex", "title": "dex", "version": ctx.version},
            "authMethods": [],
        })),
        "session/new" => new_session(ctx, &params).await,
        "session/load" => load_session(ctx, &params).await,
        "session/set_mode" => {
            let (_, session) = ctx.session(&params)?;
            let mode = params["modeId"].as_str().unwrap_or_default();
            if !map::MODES.iter().any(|(id, ..)| *id == mode) {
                return Err((rpc::INVALID_PARAMS, format!("unknown mode '{mode}'")));
            }
            if let Ok(mut current) = session.mode.lock() {
                *current = mode.to_string();
            }
            Ok(json!({}))
        }
        "session/prompt" => prompt(ctx, &params).await,
        other => Err((rpc::METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

async fn new_session(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, RpcError> {
    let cwd = params["cwd"]
        .as_str()
        .filter(|c| std::path::Path::new(c).is_absolute())
        .ok_or((
            rpc::INVALID_PARAMS,
            "session/new requires an absolute `cwd`".to_string(),
        ))?;
    let created = ctx
        .client
        .create_session_async(cwd, None)
        .await
        .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
    let mode = seeded_mode(&ctx.options);
    ctx.register(created.session_id.clone(), &mode);
    Ok(json!({"sessionId": created.session_id, "modes": map::modes_state(&mode)}))
}

/// `session/load`: take over an existing daemon session and stream its
/// journaled transcript to the client as `session/update` notifications
/// before responding (`loadSession` capability).
async fn load_session(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, RpcError> {
    let id = params["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or((
            rpc::INVALID_PARAMS,
            "session/load requires `sessionId`".to_string(),
        ))?;
    // Existence probe first so an unknown id stays `INVALID_PARAMS` instead
    // of a transport error: the plain list names every session the daemon
    // can serve (from memory or disk).
    let known = ctx
        .client
        .list_sessions_async()
        .await
        .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?
        .iter()
        .any(|s| s.session_id == id);
    if !known {
        return Err((rpc::INVALID_PARAMS, format!("unknown session '{id}'")));
    }
    ctx.client
        .reattach_async(id)
        .await
        .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
    let mode = seeded_mode(&ctx.options);
    ctx.register(id.to_string(), &mode);
    replay_updates(ctx, id).await;
    Ok(json!({"modes": map::modes_state(&mode)}))
}

/// Stream the journal under a loaded session as `session/update`
/// notifications, mapped exactly like live delivery. Pages drain to the end
/// of the journal (already cursor-stamped by `reattach`); a page that fails
/// or never advances the cursor stops the drain — the session stays usable
/// either way, and later turns stream live again.
async fn replay_updates(ctx: &Arc<Ctx>, session_id: &str) {
    let mut ids = ToolIds::default();
    let mut cursor = 0;
    loop {
        let page = ctx.client.events_async(session_id, cursor).await;
        let Ok(page) = page else {
            return;
        };
        if page.next_seq <= cursor || page.events.is_empty() {
            return;
        }
        for envelope in &page.events {
            if let Some(update) = map::update_for(&envelope.event, &mut ids) {
                ctx.rpc.notify(
                    "session/update",
                    json!({"sessionId": session_id, "update": update}),
                );
            }
        }
        cursor = page.next_seq;
    }
}

async fn prompt(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, RpcError> {
    let (session_id, session) = ctx.session(params)?;
    let blocks = params["prompt"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let text = map::prompt_text(blocks);
    if session.busy.swap(true, Ordering::SeqCst) {
        return Err((
            rpc::INVALID_PARAMS,
            "a prompt is already running in this session".to_string(),
        ));
    }
    let _busy = BusyGuard(&session.busy);
    session.cancelled.store(false, Ordering::SeqCst);

    let mut options = ctx.options.clone();
    options.mode = session.mode.lock().ok().map(|m| m.clone());
    let mut stream = ctx
        .client
        .chat_stream(&session_id, &text, options)
        .await
        .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;

    let update = |u: Value| {
        ctx.rpc.notify(
            "session/update",
            json!({"sessionId": session_id, "update": u}),
        )
    };
    let mut ids = ToolIds::default();
    let mut failure: Option<String> = None;
    let mut terminal = false;
    loop {
        // Register for cancellation before checking the flag so a cancel
        // landing in between is not lost (`notify_waiters` stores no permit).
        let notified = session.cancel_signal.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let next = if session.cancelled.load(Ordering::SeqCst) {
            tokio::time::timeout(CANCEL_GRACE, stream.next_event())
                .await
                .unwrap_or(None)
        } else {
            tokio::select! {
                item = stream.next_event() => item,
                _ = &mut notified => tokio::time::timeout(CANCEL_GRACE, stream.next_event())
                    .await
                    .unwrap_or(None),
            }
        };
        let Some(item) = next else { break };
        let event = item.map_err(|e| (rpc::INTERNAL_ERROR, e))?;
        match &event {
            StreamEvent::ApprovalRequired {
                request_id,
                name,
                input,
                agent,
            } => {
                let decision = if session.cancelled.load(Ordering::SeqCst) {
                    dex_client::protocol::ApprovalDecision::Deny
                } else {
                    // A child agent's call is not in this turn's open set:
                    // don't pair it with a same-named parent call.
                    let tool_call_id = match agent {
                        Some(_) => request_id.as_str(),
                        None => ids.pending_for(name).unwrap_or(request_id),
                    };
                    let request = ctx.rpc.request(
                        "session/request_permission",
                        json!({
                            "sessionId": session_id,
                            "toolCall": {
                                "toolCallId": tool_call_id,
                                "title": permission_title(agent.as_deref(), name, input),
                                "kind": map::tool_kind(name),
                            },
                            "options": map::permission_options(),
                        }),
                    );
                    tokio::select! {
                        reply = request => reply
                            .map(|r| map::decision_from_outcome(&r))
                            .unwrap_or(dex_client::protocol::ApprovalDecision::Deny),
                        _ = &mut notified => dex_client::protocol::ApprovalDecision::Deny,
                    }
                };
                let _ = ctx
                    .client
                    .approve_async(&session_id, request_id, decision)
                    .await;
            }
            // ACP has no structured-question surface: dismiss so the turn
            // never hangs on a prompt nobody can answer.
            StreamEvent::QuestionRequired {
                request_id,
                questions,
                ..
            } => {
                let answers = vec![QuestionAnswer::Dismiss; questions.len()];
                let _ = ctx
                    .client
                    .answer_async(&session_id, request_id, answers)
                    .await;
            }
            StreamEvent::TurnComplete { .. } => terminal = true,
            StreamEvent::TurnFailed { error } => {
                terminal = true;
                failure = Some(error.clone());
            }
            other => {
                if let Some(u) = map::update_for(other, &mut ids) {
                    update(u);
                }
            }
        }
        if terminal {
            break;
        }
    }

    if session.cancelled.load(Ordering::SeqCst) {
        return Ok(json!({"stopReason": "cancelled"}));
    }
    match (terminal, failure) {
        (_, Some(error)) => Err((rpc::INTERNAL_ERROR, error)),
        (true, None) => Ok(json!({"stopReason": "end_turn"})),
        (false, None) => Err((
            rpc::INTERNAL_ERROR,
            "connection closed before the turn completed".to_string(),
        )),
    }
}

/// Approval title: tool name plus a short one-line view of its input.
fn permission_title(agent: Option<&str>, name: &str, input: &str) -> String {
    let one_line: String = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = one_line.chars().take(120).collect();
    let who = agent.map(|a| format!("[{a}] ")).unwrap_or_default();
    if short.is_empty() {
        format!("{who}{name}")
    } else {
        format!("{who}{name}: {short}")
    }
}

/// Clears a session's `busy` flag when the prompt ends, however it ends.
struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Minimal stdio-roundtrip harness over tokio duplex pipes.
    async fn roundtrip_with(daemon: &str, frames: &[Value]) -> Vec<Value> {
        let (mut client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);
        let daemon = DaemonClient::with_token(daemon, None).unwrap();
        let task = tokio::spawn(serve(
            BufReader::new(server_r),
            server_w,
            daemon,
            ChatOptions::default(),
            "9.9.9",
        ));
        let mut out = Vec::new();
        let mut lines = BufReader::new(client_r).lines();
        for frame in frames {
            client_w
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
            // Requests are handled concurrently, so like a real client wait
            // for each response before sending the next frame.
            if let (Some(id), true) = (frame.get("id"), frame.get("method").is_some()) {
                while let Some(line) = lines.next_line().await.unwrap() {
                    let message: Value = serde_json::from_str(&line).unwrap();
                    let answered = message.get("method").is_none() && message["id"] == *id;
                    out.push(message);
                    if answered {
                        break;
                    }
                }
            }
        }
        drop(client_w);
        while let Some(line) = lines.next_line().await.unwrap() {
            out.push(serde_json::from_str(&line).unwrap());
        }
        task.await.unwrap().unwrap();
        out
    }

    async fn roundtrip(frames: &[Value]) -> Vec<Value> {
        roundtrip_with("http://127.0.0.1:1", frames).await
    }

    /// Tiny HTTP daemon for the load paths: `reqwest` dispatches one
    /// connection per request and accepts hand-rolled `Connection: close`
    /// replies, so a raw tokio listener is enough. Routes match the request
    /// target by prefix; unmatched targets get `404 {}`.
    async fn mock_daemon(routes: &[(&str, u16, &str)]) -> String {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: Vec<(String, u16, String)> = routes
            .iter()
            .map(|(p, s, b)| (p.to_string(), *s, b.to_string()))
            .collect();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let Some(target) = request_target(&mut socket).await else {
                        return;
                    };
                    let (status, body) = routes
                        .iter()
                        .filter(|(path, ..)| target.starts_with(path))
                        // Longest match wins: `/api/sessions` also prefixes
                        // every per-session route.
                        .max_by_key(|(path, ..)| path.len())
                        .map(|(_, s, b)| (*s, b.clone()))
                        .unwrap_or((404, "{}".to_string()));
                    let phrase = if status == 200 { "OK" } else { "Not Found" };
                    let reply = format!(
                        "HTTP/1.1 {status} {phrase}\r\n\
                         Content-Type: application/json\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(reply.as_bytes()).await.ok();
                    socket.shutdown().await.ok();
                });
            }
        });
        format!("http://{addr}")
    }

    /// First line's request target of one HTTP/1.1 request (`api?query`),
    /// read byte-wise until the end of the headers.
    async fn request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
        use tokio::io::AsyncReadExt;
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match socket.read(&mut byte).await {
                Ok(0) | Err(_) => return None,
                _ => head.push(byte[0]),
            }
        }
        String::from_utf8_lossy(&head)
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1).map(str::to_string))
    }

    #[tokio::test]
    async fn initialize_and_error_paths() {
        let out = roundtrip(&[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
            json!({"jsonrpc":"2.0","id":2,"method":"nope"}),
            json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"x","prompt":[]}}),
            json!({"jsonrpc":"2.0","id":4,"method":"session/new","params":{}}),
        ])
        .await;
        let by_id = |id: u64| out.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(by_id(1)["result"]["protocolVersion"], 1);
        assert_eq!(by_id(1)["result"]["agentInfo"]["name"], "dex");
        assert_eq!(by_id(1)["result"]["agentInfo"]["version"], "9.9.9");
        assert_eq!(by_id(2)["error"]["code"], rpc::METHOD_NOT_FOUND);
        assert_eq!(by_id(3)["error"]["code"], rpc::INVALID_PARAMS);
        assert_eq!(by_id(4)["error"]["code"], rpc::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn rejects_relative_cwd() {
        let out = roundtrip(&[
            json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"rel/dir"}}),
        ])
        .await;
        assert_eq!(out[0]["error"]["code"], rpc::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn session_load_replays_journal_then_accepts_prompts() {
        let daemon = mock_daemon(&[
            (
                "/api/sessions",
                200,
                r#"{"sessions":[{"session_id":"s-1","path":"/w","name":null,"cwd":"/w"}]}"#,
            ),
            ("/api/sessions/s-1/reattach", 200, r#"{"session_id":"s-1","seq":3}"#),
            (
                "/api/sessions/s-1/events?since=0",
                200,
                r#"{"events":[{"seq":0,"type":"assistant_text","data":"hi"},{"seq":1,"type":"tool_call","data":{"name":"bash","args":"ls","id":"c1"}}],"next_seq":2}"#,
            ),
            (
                "/api/sessions/s-1/events?since=2",
                200,
                r#"{"events":[{"seq":2,"type":"steering_accepted","data":{"content":"use bash"}}],"next_seq":3}"#,
            ),
        ])
        .await;
        let out = roundtrip_with(
            &daemon,
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
                json!({
                    "jsonrpc":"2.0","id":2,"method":"session/load",
                    "params":{"sessionId":"s-1","cwd":"/w","mcpServers":[]}
                }),
                json!({
                    "jsonrpc":"2.0","id":3,"method":"session/prompt",
                    "params":{"sessionId":"s-1","prompt":[{"type":"text","text":"x"}]}
                }),
            ],
        )
        .await;
        let updates: Vec<&Value> = out
            .iter()
            .filter(|m| m["method"] == "session/update")
            .collect();
        assert!(!updates.is_empty(), "out: {out:?}");
        let update = |n: usize| updates[n]["params"]["update"].clone();
        // The journal replays in order, before the `session/load` response.
        let u0 = update(0);
        assert_eq!(u0["sessionUpdate"], "agent_message_chunk");
        assert_eq!(u0["content"]["text"], "hi");
        let u1 = update(1);
        assert_eq!(u1["sessionUpdate"], "tool_call");
        assert_eq!(u1["toolCallId"], "c1");
        let u2 = update(2);
        assert_eq!(u2["sessionUpdate"], "user_message_chunk");
        assert_eq!(u2["content"]["text"], "use bash");

        let by_id = |id: u64| out.iter().find(|m| m["id"] == id).unwrap();
        assert_eq!(by_id(1)["result"]["agentCapabilities"]["loadSession"], true);
        assert_eq!(by_id(2)["result"]["modes"]["currentModeId"], "auto");
        // The loaded session is registered: its prompt reaches the daemon
        // (the mock never completes the turn) instead of `unknown session`.
        let err = &by_id(3)["error"];
        assert_eq!(err["code"], rpc::INTERNAL_ERROR, "{}", err);
        assert!(
            !err["message"].to_string().contains("unknown session"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn session_load_unknown_id_is_invalid_params() {
        let daemon = mock_daemon(&[("/api/sessions", 200, r#"{"sessions":[]}"#)]).await;
        let out = roundtrip_with(
            &daemon,
            &[json!({
                "jsonrpc":"2.0","id":1,"method":"session/load",
                "params":{"sessionId":"nope","cwd":"/w","mcpServers":[]}
            })],
        )
        .await;
        assert_eq!(out[0]["error"]["code"], rpc::INVALID_PARAMS);
        let msg = out[0]["error"]["message"].as_str().unwrap();
        assert!(msg.contains("unknown session 'nope'"), "{}: {msg}", out[0]);
    }

    #[test]
    fn permission_title_labels_child_agents() {
        assert_eq!(
            permission_title(
                None, "bash", "ls  -la
"
            ),
            "bash: ls -la"
        );
        assert_eq!(
            permission_title(Some("explorer"), "bash", ""),
            "[explorer] bash"
        );
    }
}
