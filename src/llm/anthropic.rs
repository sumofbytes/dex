//! Anthropic Messages wire protocol (`anthropic-messages`). This module is
//! the request side: history → `POST /v1/messages` body. The response side
//! is the SSE parser in `stream::AnthropicParser`, and auth is
//! `provider::AuthScheme::Anthropic` — the same one-module-per-protocol
//! split the OpenAI pair (`protocol.rs` + `stream.rs` parsers) uses, so a
//! wire detail is only ever touched in one place.

use serde_json::{json, Value};

use crate::core::types::{ChatMessage, LlmToolCall, Role};
use crate::llm::config::LlmConfig;
use crate::llm::protocol::tools_schema;

/// `anthropic-version` header value pinned by `AuthScheme::Anthropic`.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Required `max_tokens` cap. Deliberately a constant, not a knob: it only
/// bounds output. The models.dev catalog's `limit.output` clamps it tighter
/// when the model caps lower (see `max_tokens_and_budget`); a stale catalog
/// fails with a named 400, not a hang.
const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// Headroom the thinking budget must keep below `max_tokens`.
const THINKING_HEADROOM: u64 = 4096;

/// Messages request path. Native base (`https://api.anthropic.com`) gets
/// `/v1/messages`; a base already ending in `/v1` (gateways that mirror the
/// official path layout) gets `/messages` so the version segment never
/// doubles.
pub(crate) fn messages_url(base_url: &str) -> String {
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
fn thinking_budget(effort: &str) -> u64 {
    match effort.trim().to_ascii_lowercase().as_str() {
        "minimal" => 1024,
        "low" => 4096,
        "high" => 16_384,
        "xhigh" => 32_768,
        _ => 8192,
    }
}

/// Request body for `POST /v1/messages`. `with_tools` false keeps `tools`
/// off entirely (compaction/summary calls stay plain-text).
///
/// Prompt caching is always on: breakpoints mark the ends of the three
/// reusable prefixes (system, tool schemas, conversation-so-far) so each
/// turn reuses the previous turn's cached prefix instead of re-reading it.
pub(crate) fn messages_body(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
) -> Value {
    let (system, mut msgs) = messages_input(messages);
    if let Some(last) = msgs.last_mut() {
        mark_cacheable(last);
    }
    let (max_tokens, budget) = max_tokens_and_budget(config);
    let mut body = json!({
        "model": config.model,
        "max_tokens": max_tokens,
        "messages": msgs,
        "stream": true,
    });
    if let Some(system) = system {
        body["system"] = json!([{ "type": "text", "text": system, "cache_control": cache_mark() }]);
    }
    if with_tools {
        let mut tools = anthropic_tools();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = cache_mark();
        }
        body["tools"] = json!(tools);
    }
    if let Some(budget) = budget {
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    body
}

/// `max_tokens` plus the optional thinking budget. `DEFAULT_MAX_TOKENS`
/// stands unless the catalog knows a tighter `limit.output` for the model;
/// the thinking budget adds headroom, and on capped models shrinks toward
/// the API floor (1024) so it stays strictly below `max_tokens`.
fn max_tokens_and_budget(config: &LlmConfig) -> (u64, Option<u64>) {
    let cap = crate::llm::config::catalog_output_limit_for(&config.model);
    let budget = config.thinking_effort.as_deref().map(thinking_budget);
    let budget = match (budget, cap) {
        (Some(b), Some(cap)) if b + THINKING_HEADROOM > cap => {
            Some(b.min(cap.saturating_sub(THINKING_HEADROOM)).max(1024))
        }
        (budget, _) => budget,
    };
    let desired = DEFAULT_MAX_TOKENS.max(budget.unwrap_or(0) + THINKING_HEADROOM);
    let max_tokens = cap.map_or(desired, |cap| desired.min(cap));
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
pub(crate) fn messages_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
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
fn tool_use_block(call: &LlmToolCall) -> Value {
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
/// OpenAI `function.parameters` wrapper).
pub(crate) fn anthropic_tools() -> Vec<Value> {
    tools_schema()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.function.name,
                "description": tool.function.description,
                "input_schema": tool.function.parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::FunctionCall;

    #[test]
    fn messages_url_appends_version_segment_once() {
        assert_eq!(
            messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        // Gateways that already include the version segment.
        assert_eq!(
            messages_url("https://proxy.example/v1/"),
            "https://proxy.example/v1/messages"
        );
        assert_eq!(
            messages_url("https://proxy.example/anthropic"),
            "https://proxy.example/anthropic/v1/messages"
        );
    }

    #[test]
    fn messages_input_splits_system_and_merges_tool_results() {
        let msgs = vec![
            ChatMessage::system("sys1"),
            ChatMessage::system("sys2"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_calls(
                None,
                vec![
                    LlmToolCall {
                        id: "t1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "read".into(),
                            arguments: r#"{"path":"a.rs"}"#.into(),
                        },
                    },
                    LlmToolCall {
                        id: "t2".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    },
                ],
            ),
            ChatMessage::tool_result("t1", "out1"),
            ChatMessage::tool_result("t2", "out2"),
        ];
        let (system, input) = messages_input(&msgs);
        assert_eq!(system.unwrap(), "sys1\n\nsys2");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        // Consecutive tool results merge into ONE user message so roles
        // keep alternating.
        assert_eq!(input[2]["role"], "user");
        let results = input[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "t1");
        assert_eq!(results[1]["tool_use_id"], "t2");
        // tool_use arguments arrive as a parsed object, not a string.
        let calls = input[1]["content"].as_array().unwrap();
        assert_eq!(calls[0]["type"], "tool_use");
        assert_eq!(calls[0]["input"]["path"], "a.rs");
    }

    #[test]
    fn messages_input_replays_signed_thinking_before_text() {
        let mut msg = ChatMessage::assistant_calls(
            Some("answer".into()),
            vec![LlmToolCall {
                id: "t1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        // One replayable thinking block plus a foreign responses-API item
        // that must NOT leak into the Anthropic request.
        msg.reasoning_items = Some(vec![
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig1"}),
            json!({"type": "reasoning", "encrypted_content": "blob"}),
        ]);
        let (_, input) = messages_input(&[msg]);
        let blocks = input[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["signature"], "sig1");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "answer");
        assert_eq!(blocks[2]["type"], "tool_use");
    }

    #[test]
    fn tool_use_unparsable_arguments_degrade_to_empty_object() {
        let call = LlmToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "write".into(),
                arguments: "not json".into(),
            },
        };
        let block = tool_use_block(&call);
        assert_eq!(block["input"], json!({}));
        // A JSON array is not an object either.
        let call = LlmToolCall {
            id: "t1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "write".into(),
                arguments: "[1,2]".into(),
            },
        };
        assert_eq!(tool_use_block(&call)["input"], json!({}));
    }

    /// Points `XDG_CACHE_HOME` at a scratch dir (seeded with a models.dev
    /// catalog) for the test's duration, so catalog lookups behind
    /// `messages_body` don't read the machine's real cache.
    struct HermeticCatalog {
        _lock: std::sync::MutexGuard<'static, ()>,
        dir: std::path::PathBuf,
        prev: Option<std::ffi::OsString>,
    }

    impl HermeticCatalog {
        fn set(name: &str, catalog: &str) -> Self {
            let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir()
                .join(format!("dex-test-catalog-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("dex")).unwrap();
            std::fs::write(dir.join("dex/models.dev.json"), catalog).unwrap();
            let prev = std::env::var_os("XDG_CACHE_HOME");
            std::env::set_var("XDG_CACHE_HOME", &dir);
            Self { _lock, dir, prev }
        }

        fn empty(name: &str) -> Self {
            Self::set(name, "{}")
        }
    }

    impl Drop for HermeticCatalog {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("XDG_CACHE_HOME", v),
                None => std::env::remove_var("XDG_CACHE_HOME"),
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn messages_body_shape_and_thinking_budgets() {
        // Empty catalog: `limit.output` lookups miss, so the constants below
        // hold regardless of the machine's real models.dev cache.
        let _catalog = HermeticCatalog::empty("body-shape");
        let mut config = crate::llm::config::tests::test_cfg();
        config.model = "claude-sonnet-4-5".into();
        config.thinking_effort = None;
        let body = messages_body(&config, &[ChatMessage::user("hi")], false);
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["max_tokens"], 16_384);
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        // The conversation-so-far breakpoint marks the last message.
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
        assert!(body.get("thinking").is_none());

        // Thinking enabled: budget maps from the effort knob and sits
        // strictly below max_tokens.
        config.thinking_effort = Some("low".into());
        let body = messages_body(&config, &[ChatMessage::user("hi")], true);
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        assert_eq!(body["max_tokens"], 16_384);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "read");
        assert!(tools[0].get("input_schema").is_some());
        assert!(tools[0].get("function").is_none());
        // Tool-schema breakpoint marks the last definition.
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

        config.thinking_effort = Some("xhigh".into());
        let body = messages_body(&config, &[ChatMessage::user("hi")], false);
        assert_eq!(body["thinking"]["budget_tokens"], 32_768);
        assert_eq!(body["max_tokens"], 36_864);

        // Unknown / non-OpenAI-vocabulary picks land on the middle bucket.
        config.thinking_effort = Some("mystery".into());
        let body = messages_body(&config, &[ChatMessage::user("hi")], false);
        assert_eq!(body["thinking"]["budget_tokens"], 8192);

        // System content lands in the top-level `system` block array with
        // its own cache breakpoint.
        let body = messages_body(
            &config,
            &[ChatMessage::system("be brief"), ChatMessage::user("hi")],
            false,
        );
        assert_eq!(body["system"][0]["text"], "be brief");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn messages_body_clamps_max_tokens_to_catalog_output_limit() {
        let _catalog = HermeticCatalog::set(
            "output-clamp",
            r#"{"anthropic":{"models":{"claude-haiku-mini":{"limit":{"context":200000,"output":8192}}}}}"#,
        );
        let mut config = crate::llm::config::tests::test_cfg();
        config.model = "claude-haiku-mini".into();

        // No thinking: the cap replaces the constant outright.
        config.thinking_effort = None;
        let body = messages_body(&config, &[ChatMessage::user("hi")], false);
        assert_eq!(body["max_tokens"], 8_192);

        // Thinking: the budget shrinks so it stays strictly below the cap.
        config.thinking_effort = Some("xhigh".into());
        let body = messages_body(&config, &[ChatMessage::user("hi")], false);
        assert_eq!(body["max_tokens"], 8_192);
        assert_eq!(body["thinking"]["budget_tokens"], 4_096);
    }

    #[test]
    fn messages_body_marks_cache_breakpoints_outside_thinking_blocks() {
        let _catalog = HermeticCatalog::empty("cache-marks");
        let mut msg = ChatMessage::assistant_calls(
            None,
            vec![LlmToolCall {
                id: "t1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        msg.reasoning_items = Some(vec![
            json!({"type": "thinking", "thinking": "hmm", "signature": "sig1"}),
        ]);
        let config = crate::llm::config::tests::test_cfg();
        let body = messages_body(&config, &[msg], false);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        // The marker lands on the tool_use block; the thinking block —
        // which can't carry cache_control — stays untouched.
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(blocks[0].get("cache_control").is_none());
    }
}
