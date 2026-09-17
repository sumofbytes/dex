//! MCP tool/content mapping to agent tool definitions.

use crate::core::types::ToolDefinition;
use serde_json::Value;

use super::config::{mcp_tool_name, resource_reader_name, MCP_DESC_LIMIT, MCP_OUTPUT_BYTES};

// ---------------------------------------------------------------------------
// MCP types: tools, content mapping
// ---------------------------------------------------------------------------

/// A tool advertised by an MCP server (before namespacing).
#[derive(Clone, Debug)]
pub(crate) struct McpTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

impl McpTool {
    pub(crate) fn to_definition(&self, server: &str) -> ToolDefinition {
        let mut desc = format!("[{}] {}", server, self.description.trim());
        if desc.len() > MCP_DESC_LIMIT {
            desc.truncate(MCP_DESC_LIMIT);
            desc.push('…');
        }
        let mut params = self.input_schema.clone();
        if !params.is_object() {
            params = serde_json::json!({"type": "object"});
        }
        ToolDefinition {
            tool_type: "function".to_string(),
            function: crate::core::types::FunctionDef {
                name: mcp_tool_name(server, &self.name),
                description: desc,
                parameters: params,
            },
        }
    }
}

/// Synthetic P1 reader: one per server, backed by `resources/*`.
pub(crate) fn resource_reader_definition(server: &str) -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: crate::core::types::FunctionDef {
            name: resource_reader_name(server),
            description: format!(
                "[{server}] Read a resource served by this MCP server (file, doc, schema). Prefer this over shelling out when the server hosts the data."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "uri": { "type": "string", "description": "resource URI from resources/list" } },
                "required": ["uri"]
            }),
        },
    }
}

/// `v[key]` as a slice, empty when missing or not an array — the
/// `.and_then(as_array).unwrap_or(&[])` ladder needs a local empty to borrow;
/// this does not.
pub(crate) fn json_arr<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    match v.get(key).and_then(Value::as_array) {
        Some(items) => items.as_slice(),
        None => &[],
    }
}

/// Bound any server-produced text to [`MCP_OUTPUT_BYTES`] with a marker.
pub(crate) fn clamp_output(mut text: String) -> String {
    if text.len() > MCP_OUTPUT_BYTES {
        text.truncate(MCP_OUTPUT_BYTES);
        text.push_str("\n[truncated]");
    }
    text
}

/// Map a `tools/call` result's `content[]` to display text. Text wins;
/// images/resources degrade to placeholders so the model still sees shape.
pub(crate) fn content_to_text(result: &Value) -> String {
    let mut parts = Vec::new();
    let content = json_arr(result, "content");
    for item in content {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            "image" | "audio" => {
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("data");
                parts.push(format!("[{kind} omitted: {mime}]"));
            }
            "resource" => {
                let uri = item
                    .get("resource")
                    .and_then(|r| r.get("uri"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                if let Some(text) = item
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(Value::as_str)
                {
                    parts.push(format!("[resource {uri}]\n{text}"));
                } else {
                    parts.push(format!("[resource omitted: {uri}]"));
                }
            }
            _ => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
        }
    }
    let mut text = parts.join("\n");
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && !text.is_empty()
    {
        text = format!("Error: {text}");
    }
    clamp_output(text)
}
