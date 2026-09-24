//! Dex adapter for Anthropic Messages request construction. Catalog lookup stays
//! in dex; protocol formatting lives in `dex-ai`.
#[cfg(test)]
use crate::protocol::LlmToolCall;
use crate::protocol::{ChatMessage, ToolDefinition};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;

pub(crate) use dex_ai::anthropic::messages_url;
#[cfg(test)]
use dex_ai::anthropic::{messages_input, tool_use_block};

pub(crate) fn messages_body(
    model: &str,
    thinking_effort: Option<&str>,
    messages: &[ChatMessage],
    tools: &[ToolDefinition],
) -> Value {
    dex_ai::anthropic::messages_body(
        model,
        crate::llm::config::catalog_output_limit_for(model),
        thinking_effort,
        messages,
        tools,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FunctionCall;

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
    fn messages_input_is_append_only_for_cache_stability() {
        // Prompt-cache stability: appending a new user turn only appends to
        // the wire view, never rewrites earlier entries. The provider caches
        // the prefix; any byte change in it forces a full re-read.
        let mut base = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::assistant_calls(
                None,
                vec![LlmToolCall {
                    id: "t1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
            ),
            ChatMessage::tool_result("t1", "out1"),
        ];
        let (sys1, input1) = messages_input(&base);
        base.push(ChatMessage::user("follow-up"));
        let (sys2, input2) = messages_input(&base);
        assert_eq!(sys1, sys2);
        assert_eq!(input2.len(), input1.len() + 1);
        assert_eq!(&input2[..input1.len()], &input1[..]);
        // Known exception: appending another consecutive tool_result merges
        // into the trailing user message (roles must alternate), rewriting
        // the last wire entry instead of appending.
        base.pop();
        let (_, before) = messages_input(&base);
        base.push(ChatMessage::tool_result("t2", "out2"));
        let (_, after) = messages_input(&base);
        assert_eq!(after.len(), before.len());
        assert_ne!(after[before.len() - 1], before[before.len() - 1]);
    }

    #[test]
    fn messages_body_keeps_system_and_tools_stable_across_appends() {
        let _catalog = HermeticCatalog::empty("prefix-stable");
        let mut base = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let tools = crate::llm::protocol::tools_schema();
        let body1 = messages_body("m-r", None, &base, &tools);
        base.push(ChatMessage::user("follow-up"));
        let body2 = messages_body("m-r", None, &base, &tools);
        assert_eq!(body1["system"], body2["system"]);
        assert_eq!(body1["tools"], body2["tools"]);
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
        let model = "claude-sonnet-4-5";
        let body = messages_body(model, None, &[ChatMessage::user("hi")], &[]);
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
        let tools = crate::llm::protocol::tools_schema();
        let body = messages_body(model, Some("low"), &[ChatMessage::user("hi")], &tools);
        assert_eq!(body["thinking"]["budget_tokens"], 4096);
        assert_eq!(body["max_tokens"], 16_384);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], "read");
        assert!(tools[0].get("input_schema").is_some());
        assert!(tools[0].get("function").is_none());
        // Tool-schema breakpoint marks the last definition.
        assert_eq!(tools.last().unwrap()["cache_control"]["type"], "ephemeral");

        let body = messages_body(model, Some("xhigh"), &[ChatMessage::user("hi")], &[]);
        assert_eq!(body["thinking"]["budget_tokens"], 32_768);
        assert_eq!(body["max_tokens"], 36_864);

        // Unknown / non-OpenAI-vocabulary picks land on the middle bucket.
        let body = messages_body(model, Some("mystery"), &[ChatMessage::user("hi")], &[]);
        assert_eq!(body["thinking"]["budget_tokens"], 8192);

        // System content lands in the top-level `system` block array with
        // its own cache breakpoint.
        let body = messages_body(
            model,
            Some("mystery"),
            &[ChatMessage::system("be brief"), ChatMessage::user("hi")],
            &[],
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
        let model = "claude-haiku-mini";

        // No thinking: the cap replaces the constant outright.
        let body = messages_body(model, None, &[ChatMessage::user("hi")], &[]);
        assert_eq!(body["max_tokens"], 8_192);

        // Thinking: the budget shrinks so it stays strictly below the cap.
        let body = messages_body(model, Some("xhigh"), &[ChatMessage::user("hi")], &[]);
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
        let body = messages_body("m-r", None, &[msg], &[]);
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        // The marker lands on the tool_use block; the thinking block —
        // which can't carry cache_control — stays untouched.
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(blocks[0].get("cache_control").is_none());
    }
}
