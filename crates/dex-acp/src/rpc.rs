//! Newline-delimited JSON-RPC 2.0 plumbing: a serialized writer task, plus
//! agent->client requests that await the client's response.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub struct Rpc {
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
}

impl Rpc {
    /// Spawn the writer task. One task owns the writer so concurrent
    /// prompts never interleave partial lines.
    pub fn new<W: AsyncWrite + Unpin + Send + 'static>(mut writer: W) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err()
                {
                    break;
                }
            }
        });
        Self {
            tx,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    fn send(&self, message: Value) {
        let _ = self.tx.send(message.to_string());
    }

    pub fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn respond(&self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub fn respond_err(&self, id: Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": code, "message": message},
        }));
    }

    /// Send a request to the client and wait for its `result`. `None` when
    /// the client answers with an error or goes away.
    pub async fn request(&self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().ok()?.insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.ok()
    }

    /// Route a client response to the waiting `request`. An error response
    /// drops the sender so the waiter sees `None`.
    pub fn deliver(&self, id: &Value, message: &Value) {
        let Some(id) = id.as_u64() else { return };
        let Some(waiter) = self.pending.lock().ok().and_then(|mut p| p.remove(&id)) else {
            return;
        };
        if message.get("error").is_none() {
            let _ = waiter.send(message["result"].clone());
        }
    }

    /// Fail every outstanding request (client disconnected).
    pub fn close(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
    }
}
