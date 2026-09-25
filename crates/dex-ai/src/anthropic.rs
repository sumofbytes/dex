//! Anthropic Messages wire protocol (`anthropic-messages`). This module is
//! the request side: history → `POST /v1/messages` body. The response side
//! is the SSE parser in `sse::AnthropicParser`, and auth is
//! `provider::AuthScheme::Anthropic` — the same one-module-per-protocol
//! split the OpenAI pair (`protocol.rs` + `stream.rs` parsers) uses, so a
//! wire detail is only ever touched in one place.

use serde_json::{json, Value};

use crate::wire::wire_tools;
use crate::{ChatMessage, LlmToolCall, Role, ToolDefinition};

/// Required default `max_tokens` cap. The host may pass a smaller resolved
/// model output limit to [`messages_body`]; catalog lookup remains outside
/// this crate.
const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// Headroom the thinking budget must keep below `max_tokens`.
const THINKING_HEADROOM: u64 = 4096;

/// Messages request path. Native base (`https://api.anthropic.com`) gets
/// `/v1/messages`; a base already ending in `/v1` (gateways that mirror the
/// official path layout) gets `/messages` so the version segment never
/// doubles.
pub fn messages_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// `thinking_effort` knob → Anthropic `budget_tokens`. Anthropic has no
/// effort names, so the OpenAI-style vocabulary maps onto budgets (the
/// API floor is 1024). Unknown picks get the middle bucket.
pub fn thinking_budget(effort: &str) -> u64 {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => 1024,
        "low" => 4096,
        "high" => 16_384,
        "xhigh" => 32_768,
        _ => 8192,
    }
}

/// Request body for `POST /v1/messages`. An empty tool slice keeps `tools`
/// off entirely (compaction/summary calls stay plain-text).
///
/// Prompt caching is always on: breakpoints mark the ends of the three
/// reusable prefixes (system, tool schemas, conversation-so-far) so each
/// turn reuses the previous turn's cached prefix instead of re-reading it.
pub fn messages_body(
    model: &str,
    max_output_tokens: Option<u64>,
    thinking_effort: Option<&str>,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
) -> Value {
    let (system, mut msgs) = messages_input(messages);
    if let Some(last) = msgs.last_mut() {
        mark_cacheable(last);
    }
    let (max_tokens, budget) = max_tokens_and_budget(max_output_tokens, thinking_effort);
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": msgs,
        "stream": true,
    });
    if let Some(system) = system {
        body["system"] = json!([{ "type": "text", "text": system, "cache_control": cache_mark() }]);
    }
    if !tools.is_empty() {
        let mut wire_tools = anthropic_tools(tools);
        if let Some(last) = wire_tools.last_mut() {
            last["cache_control"] = cache_mark();
        }
        body["tools"] = json!(wire_tools);
    }
    if let Some(budget) = budget {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    body
}

/// `max_tokens` plus the optional thinking budget. `DEFAULT_MAX_TOKENS`
/// stands unless the host supplies a tighter `max_output_tokens`; the
/// thinking budget adds headroom and shrinks on capped models to stay below
/// `max_tokens`.
pub fn max_tokens_and_budget(
    max_output_tokens: Option<u64>,
    thinking_effort: Option<&str>,
) -> (u64, Option<u64>) {
    let budget = thinking_effort.map(thinking_budget);
    let budget = match (budget, max_output_tokens) {
        (Some(b), Some(cap)) if b + THINKING_HEADROOM > cap => {
            Some(b.min(cap.saturating_sub(THINKING_HEADROOM)).max(1024))
        }
        (budget, _) => budget,
    };
    let desired = DEFAULT_MAX_TOKENS.max(budget.unwrap_or(0) + THINKING_HEADROOM);
    let max_tokens = max_output_tokens.map_or(desired, |cap| desired.min(cap));
    (max_tokens, budget)
}

/// Prompt-caching marker: the prefix ending at the marked block is cached
/// server-side (a few breakpoints per request are allowed).
fn cache_mark() -> Value {
    json!({ "type": "ephemeral" })
}

/// Mark the last block that can carry a cache breakpoint — thinking blocks
/// can't, so the newest replayable text/tool block wins.
fn mark_cacheable(message: &mut Value) {
    let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(block) = blocks.iter_mut().rev().find(|block| {
        !matches!(
            block.get("type").and_then(Value::as_str),
            Some("thinking") | Some("redacted_thinking")
        )
    }) else {
        return;
    };
    block["cache_control"] = cache_mark();
}

/// History → `messages` array, with system messages pulled out into the
/// top-level `system` string (mirror of `protocol::responses_input`).
/// Anthropic quirks encoded here:
/// - roles must alternate, so consecutive tool results merge into ONE user
///   message carrying one `tool_result` block each;
/// - an assistant turn is a block array: replayed `thinking` blocks first
///   (only signature-carrying ones — the API rejects unsigned thinking),
///   then text, then `tool_use` with arguments parsed into an object.
pub fn messages_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
    let mut system = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                if let Some(content) = &message.content {
                    system.push(content.clone());
                }
            }
            Role::Tool => {
                pending_tool_results.push(tool_result_block(message));
            }
            Role::Assistant => {
                if !pending_tool_results.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": std::mem::take(&mut pending_tool_results),
                    }));
                }
                let blocks = assistant_blocks(message);
                if !blocks.is_empty() {
                    out.push(json!({ "role": "assistant", "content": blocks }));
                }
            }
            Role::User => {
                if !pending_tool_results.is_empty() {
                    out.push(json!({
                        "role": "user",
                        "content": std::mem::take(&mut pending_tool_results),
                    }));
                }
                out.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": message.content_str() }],
                }));
            }
        }
    }
    if !pending_tool_results.is_empty() {
        out.push(json!({
            "role": "user",
            "content": pending_tool_results,
        }));
    }
    (
        if system.is_empty() {
            None
        } else {
            Some(system.join("\n\n"))
        },
        out,
    )
}

/// Replayed thinking blocks first (signature check keeps foreign
/// `reasoning_items` — e.g. sessions that started on an OpenAI protocol —
/// from being sent as garbage blocks), then text, then tool_use.
fn assistant_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut blocks: Vec<Value> = message
        .reasoning_items
        .iter()
        .flatten()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("thinking") | Some("redacted_thinking")
            )
        })
        .cloned()
        .collect();
    if let Some(content) = &message.content {
        if !content.is_empty() {
            blocks.push(json!({ "type": "text", "text": content }));
        }
    }
    for call in message.tool_calls.as_deref().unwrap_or_default() {
        blocks.push(tool_use_block(call));
    }
    blocks
}

/// OpenAI-shaped tool call → Anthropic `tool_use` block. `input` must be a
/// JSON object; unparsable arguments degrade to `{}` rather than failing
/// the whole request.
pub fn tool_use_block(call: &LlmToolCall) -> Value {
    let input = serde_json::from_str::<Value>(&call.function.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.function.name,
        "input": input,
    })
}

fn tool_result_block(message: &ChatMessage) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
        "content": message.content_str(),
    })
}

/// Shared tool schemas → Anthropic shape (`input_schema` instead of the
/// OpenAI `function.parameters` wrapper). Merge + tail-sort via `wire_tools`;
/// only the per-tool
/// mapping differs per wire shape.
pub fn anthropic_tools(tools: &[ToolDefinition]) -> Vec<Value> {
    wire_tools(tools, |tool| {
        json!({
            "name": tool.function.name,
            "description": tool.function.description,
            "input_schema": tool.function.parameters,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::messages_body;
    use crate::ChatMessage;

    #[test]
    fn request_body_applies_host_output_limit_to_thinking_budget() {
        let body = messages_body(
            "claude-haiku-mini",
            Some(8_192),
            Some("xhigh"),
            &[ChatMessage::user("hi")],
            &[],
        );

        assert_eq!(body["max_tokens"], 8_192);
        assert_eq!(body["thinking"]["budget_tokens"], 4_096);
    }
}
