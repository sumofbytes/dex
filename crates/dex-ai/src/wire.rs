//! Provider wire-format message mapping shared by dex and other clients.
use crate::{
    ChatMessage, FunctionCall, LlmToolCall, Role, StreamToolCall, ToolDefinition, WireMessage,
};
use serde_json::{json, Value};

/// Upper bound on tool calls merged from one provider stream. Both merge
/// helpers below grow the vec from a provider-controlled `index`, so a
/// malicious or buggy provider sending `index: usize::MAX` would otherwise
/// allocate until OOM. Real turns issue at most a few dozen calls.
const MAX_MERGED_TOOL_CALLS: usize = 1024;

/// Returns `true` when the delta was dropped (index at or past the cap) so
/// callers can surface the truncation instead of silently losing calls.
pub fn merge_chat_tool_call(calls: &mut Vec<LlmToolCall>, delta: StreamToolCall) -> bool {
    if delta.index >= MAX_MERGED_TOOL_CALLS {
        return true;
    }
    while calls.len() <= delta.index {
        calls.push(LlmToolCall {
            id: String::new(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: String::new(),
                arguments: String::new(),
            },
        });
    }
    let call = &mut calls[delta.index];
    if let Some(id) = delta.id {
        call.id = id;
    }
    if let Some(function) = delta.function {
        if let Some(name) = function.name {
            call.function.name.push_str(&name);
        }
        if let Some(arguments) = function.arguments {
            call.function.arguments.push_str(&arguments);
        }
    }
    false
}

/// Chat-completions wire messages: borrowed [`WireMessage`] views, serialized
/// straight to bytes by reqwest — no `Value` middleman (perf doc §8). `name`
/// is a local tag (`steering`, `skill`, `summary`, `follow-up`,
/// `agent-notifications`) that the model never needs and strict
/// OpenAI-compatible endpoints reject (`messages[i]: "name" is not supported
/// by this endpoint`). `reasoning_items` are Responses-API blobs the
/// Responses wire replays inside `input` (and the Anthropic wire filters in
/// `assistant_blocks`) — as a chat-completions field they would be garbage.
/// `reasoning_content` stays: it is the model-facing DeepSeek field.
pub fn chat_completions_messages(messages: &[ChatMessage]) -> Vec<WireMessage<'_>> {
    messages.iter().map(ChatMessage::wire).collect()
}

/// Prompt-cache breakpoints for a chat-completions body bound for a Claude
/// model behind an OpenAI-compatible gateway (OpenRouter and friends pass
/// `cache_control` content parts through to Anthropic; without them a
/// Claude model on this wire never caches). Marks the system message (the
/// stable prefix) and the newest message with text content (the
/// conversation so far). The tail marker follows the conversation rather
/// than the last user prompt so a long tool loop never outruns Anthropic's
/// ~20-block lookback from the newest breakpoint. String content becomes
/// one text part carrying the marker; empty or non-string content is left
/// alone.
pub fn mark_chat_cache_breakpoints(messages: &mut [Value]) {
    let system = messages
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("system"));
    let tail = messages.iter().rposition(|m| {
        m.get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| !text.is_empty())
    });
    for index in [system, tail].into_iter().flatten() {
        let message = &mut messages[index];
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            message["content"] = json!([{
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" },
            }]);
        }
    }
}

pub fn responses_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                if let Some(content) = &message.content {
                    instructions.push(content.clone());
                }
            }
            Role::Tool => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "output": message.content_str(),
                }));
            }
            Role::Assistant => {
                // Replay the model's own reasoning items first: with store:false
                // the request is stateless, and the model only keeps its
                // reasoning thread if we hand it back.
                input.extend(message.reasoning_items.iter().flatten().cloned());
                if let Some(content) = &message.content {
                    if !content.is_empty() {
                        input.push(json!({ "role": "assistant", "content": content }));
                    }
                }
                for call in message.tool_calls.as_deref().unwrap_or_default() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function.name,
                        "arguments": call.function.arguments,
                    }));
                }
            }
            Role::User => {
                input.push(json!({
                    "role": "user",
                    "content": message.content_str(),
                }));
            }
        }
    }
    (
        if instructions.is_empty() {
            None
        } else {
            Some(instructions.join("\n\n"))
        },
        input,
    )
}

pub fn responses_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    wire_tools(tools, |tool| {
        json!({
            "type": "function",
            "name": tool.function.name,
            "description": tool.function.description,
            "parameters": tool.function.parameters,
        })
    })
}

/// Native + MCP + extension schemas mapped to one wire shape: native order
/// is fixed, the MCP + extension tail is sorted by name so refresh
/// completion order can't reorder the schema (deterministic bytes;
/// adding/removing a tool still shifts the tail). Borrowed slices: no
/// merged-schema copy on the wire path. Shared by the OpenAI
/// (`responses_tools`) and Anthropic (`anthropic_tools`) wire shapes — the
/// only difference is the per-tool mapping.
pub fn wire_tools(tools: &[ToolDefinition], map: impl Fn(&ToolDefinition) -> Value) -> Vec<Value> {
    tools.iter().map(map).collect()
}

/// Returns `true` when the item was dropped (index at or past the cap).
pub fn response_tool_call(calls: &mut Vec<LlmToolCall>, index: usize, item: &Value) -> bool {
    if index >= MAX_MERGED_TOOL_CALLS {
        return true;
    }
    while calls.len() <= index {
        calls.push(LlmToolCall {
            id: String::new(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: String::new(),
                arguments: String::new(),
            },
        });
    }
    let call = &mut calls[index];
    if let Some(id) = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
    {
        call.id = id.to_string();
    }
    if let Some(name) = item.get("name").and_then(Value::as_str) {
        call.function.name = name.to_string();
    }
    if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
        call.function.arguments = arguments.to_string();
    }
    false
}

pub fn response_call_index(calls: &[LlmToolCall], index: usize, item: &Value) -> usize {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| calls.iter().position(|call| call.id == id))
        .unwrap_or(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_cache_tail_follows_the_conversation_not_the_last_user() {
        let mut messages = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": null, "tool_calls": []}),
            json!({"role": "tool", "tool_call_id": "t", "content": "result"}),
            json!({"role": "assistant", "content": ""}),
        ];
        mark_chat_cache_breakpoints(&mut messages);
        assert_eq!(
            messages[0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        // Newest message with text: the tool result, not the user prompt.
        assert_eq!(messages[3]["content"][0]["text"], "result");
        assert_eq!(
            messages[3]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert_eq!(messages[1]["content"], "go");
        assert_eq!(messages[4]["content"], "");
    }
}
