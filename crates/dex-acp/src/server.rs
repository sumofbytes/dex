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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite};
use tokio::sync::Notify;

type RpcError = (i64, String);

struct Session {
    mode: Mutex<String>,
    cancelled: AtomicBool,
    cancel_signal: Notify,
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
                "loadSession": false,
                "promptCapabilities": {"image": false, "audio": false, "embeddedContext": true},
                "mcpCapabilities": {"http": false, "sse": false},
            },
            "agentInfo": {"name": "dex", "title": "dex", "version": ctx.version},
            "authMethods": [],
        })),
        "session/new" => new_session(ctx, &params).await,
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
    let cwd = params["cwd"].as_str().filter(|c| !c.is_empty()).ok_or((
        rpc::INVALID_PARAMS,
        "session/new requires an absolute `cwd`".to_string(),
    ))?;
    let created = ctx
        .client
        .create_session_async(cwd, None)
        .await
        .map_err(|e| (rpc::INTERNAL_ERROR, e.to_string()))?;
    let mode = ctx
        .options
        .mode
        .clone()
        .filter(|m| map::MODES.iter().any(|(id, ..)| id == m))
        .unwrap_or_else(|| "auto".to_string());
    let session = Arc::new(Session {
        mode: Mutex::new(mode.clone()),
        cancelled: AtomicBool::new(false),
        cancel_signal: Notify::new(),
    });
    if let Ok(mut sessions) = ctx.sessions.lock() {
        sessions.insert(created.session_id.clone(), session);
    }
    Ok(json!({"sessionId": created.session_id, "modes": map::modes_state(&mode)}))
}

async fn prompt(ctx: &Arc<Ctx>, params: &Value) -> Result<Value, RpcError> {
    let (session_id, session) = ctx.session(params)?;
    let blocks = params["prompt"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let text = map::prompt_text(blocks);
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
    while let Some(item) = stream.next_event().await {
        let event = item.map_err(|e| (rpc::INTERNAL_ERROR, e))?;
        match &event {
            StreamEvent::ApprovalRequired {
                request_id,
                name,
                input,
                ..
            } => {
                let decision = if session.cancelled.load(Ordering::SeqCst) {
                    dex_client::protocol::ApprovalDecision::Deny
                } else {
                    let request = ctx.rpc.request(
                        "session/request_permission",
                        json!({
                            "sessionId": session_id,
                            "toolCall": {
                                "toolCallId": ids.pending_for(name).unwrap_or(request_id),
                                "title": permission_title(name, input),
                                "kind": map::tool_kind(name),
                            },
                            "options": map::permission_options(),
                        }),
                    );
                    tokio::select! {
                        reply = request => reply
                            .map(|r| map::decision_from_outcome(&r))
                            .unwrap_or(dex_client::protocol::ApprovalDecision::Deny),
                        _ = session.cancel_signal.notified() =>
                            dex_client::protocol::ApprovalDecision::Deny,
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
fn permission_title(name: &str, input: &str) -> String {
    let one_line: String = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = one_line.chars().take(120).collect();
    if short.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn roundtrip(frames: &[Value]) -> Vec<Value> {
        let (mut client_w, server_r) = duplex(64 * 1024);
        let (server_w, client_r) = duplex(64 * 1024);
        let daemon = DaemonClient::with_token("http://127.0.0.1:1", None).unwrap();
        let task = tokio::spawn(serve(
            BufReader::new(server_r),
            server_w,
            daemon,
            ChatOptions::default(),
            "9.9.9",
        ));
        for frame in frames {
            client_w
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(client_w);
        let mut out = Vec::new();
        let mut lines = BufReader::new(client_r).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            out.push(serde_json::from_str(&line).unwrap());
        }
        task.await.unwrap().unwrap();
        out
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
}
