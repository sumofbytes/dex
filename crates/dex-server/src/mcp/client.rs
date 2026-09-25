//! One live MCP server connection: timeout + cancellation over any
//! [`McpTransport`].

use std::time::Duration;

use serde_json::Value;

use crate::agent::state::{wait_cancelled, CancellationSource};

use super::mapping::{clamp_output, json_arr, McpTool};
use super::transport::{rpc_error, McpTransport};

pub struct McpClient {
    transport: Box<dyn McpTransport>,
    timeout: Duration,
}

impl McpClient {
    pub(super) fn new(transport: Box<dyn McpTransport>, timeout_secs: u64) -> Self {
        Self {
            transport,
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }

    /// One call with timeout + cancellation. On cancel the in-flight
    /// transport future is dropped (aborting the HTTP request or the stdio
    /// read); the stdio id-matching loop skips the orphaned reply, so the
    /// next call on the same server stays in sync.
    async fn call(
        &self,
        method: &str,
        params: Value,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Value, String> {
        let run = tokio::time::timeout(self.timeout, self.transport.request(method, params));
        match cancel {
            Some(c) => {
                tokio::select! {
                    r = run => r.map_err(|_| format!("mcp {method} timed out"))?,
                    _ = wait_cancelled(c) => Err("mcp call cancelled".to_string()),
                }
            }
            None => run.await.map_err(|_| format!("mcp {method} timed out"))?,
        }
    }

    /// Liveness probe for the sweeper. Cheap (`tools/list`), never surfaced.
    pub async fn ping(&self) -> Result<(), String> {
        self.call("tools/list", serde_json::json!({}), None)
            .await
            .map(|_| ())
    }

    pub async fn list_tools(
        &self,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Vec<McpTool>, String> {
        let v = self
            .call("tools/list", serde_json::json!({}), cancel)
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(format!("tools/list: {err}"));
        }
        let mut out = Vec::new();
        for t in json_arr(&v, "tools") {
            let name = t.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            out.push(McpTool {
                name: name.to_string(),
                description: t
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            });
        }
        Ok(out)
    }

    pub async fn call_tool(
        &self,
        tool: &str,
        args: &Value,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<Value, String> {
        let v = self
            .call(
                "tools/call",
                serde_json::json!({"name": tool, "arguments": args}),
                cancel,
            )
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(err);
        }
        Ok(v)
    }

    pub async fn has_resources(
        &self,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> bool {
        self.call("resources/list", serde_json::json!({}), cancel)
            .await
            .map(|v| v.get("resources").and_then(Value::as_array).is_some())
            .unwrap_or(false)
    }

    pub async fn read_resource(
        &self,
        uri: &str,
        cancel: Option<&(dyn CancellationSource + Send + Sync)>,
    ) -> Result<String, String> {
        let v = self
            .call("resources/read", serde_json::json!({"uri": uri}), cancel)
            .await?;
        if let Some(err) = rpc_error(&v) {
            return Err(err);
        }
        // `resources/read` returns `contents[]` with inline `text`/`blob`
        // (no `type` discriminator), unlike `tools/call` `content[]`.
        let mut parts = Vec::new();
        for item in json_arr(&v, "contents") {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_string());
            } else if let Some(blob) = item.get("blob").and_then(Value::as_str) {
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("data");
                parts.push(format!("[blob omitted: {mime} {} bytes]", blob.len()));
            }
        }
        let mut text = parts.join("\n");
        if text.is_empty() {
            text = "(empty resource)".to_string();
        }
        Ok(clamp_output(text))
    }
}
