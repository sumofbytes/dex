use crate::protocol::ToolDefinition;
pub(crate) use dex_ai::wire::{chat_completions_messages, responses_input, responses_tools};
#[cfg(test)]
use dex_ai::wire::{merge_chat_tool_call, response_call_index, response_tool_call};
#[cfg(test)]
use serde_json::Value;
use std::sync::Arc;

#[cfg(test)]
use dex_coding_agent::sort_tool_defs_by_name;

#[cfg(test)]
fn sort_wire_tools_by_name(tail: &mut [Value]) {
    tail.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
}

pub fn tools_schema() -> Vec<ToolDefinition> {
    // One merged copy per request: `ChatRequest.tools` owns its vec. Callers
    // that serialize straight to `Value` use `tools_schema_parts` and skip
    // even this copy. Native order is fixed; the MCP + extension tail is
    // sorted by name so refresh completion order can't reorder the schema
    // (deterministic bytes; adding/removing a tool still shifts the tail).
    let (tools, mcp, ext) = tools_schema_parts();
    dex_coding_agent::merge_tool_schemas(tools, &mcp, &ext)
}

/// Borrowed schema slices for wire serialization without the merged copy:
/// (native, mcp, extensions). Same content as `tools_schema()`.
pub(crate) fn tools_schema_parts() -> (
    Vec<ToolDefinition>,
    Arc<[ToolDefinition]>,
    Arc<[ToolDefinition]>,
) {
    let tools = native_tools();
    // MCP + extension tools merge from the background-refreshed caches:
    // sync, never blocks the turn loop. Empty until the first refresh
    // lands. The caches hand out `Arc` slices — the caller clones the
    // `Arc`, not the defs.
    let mcp = crate::mcp::cached_tools();
    let ext = crate::extensions::cached_tools();
    (tools, mcp, ext)
}

/// Native schema is coding-agent behavior; the host decides whether delegation is available.
fn native_tools() -> Vec<ToolDefinition> {
    dex_coding_agent::builtin_tools(crate::agent::delegate::delegation_enabled())
}

#[cfg(test)]
mod tests {
    use super::{
        chat_completions_messages, merge_chat_tool_call, response_call_index, response_tool_call,
        responses_input,
    };
    use crate::protocol::{
        ChatMessage, FunctionCall, LlmToolCall, StreamFunctionCall, StreamToolCall,
    };
    use serde_json::json;

    #[test]
    fn merge_chat_tool_call_assembles_fragmented_deltas() {
        let mut calls = Vec::new();
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 0,
                id: Some("call_1".into()),
                function: Some(StreamFunctionCall {
                    name: Some("read".into()),
                    arguments: Some("{\"path\":\"".into()),
                }),
            },
        );
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 0,
                id: None,
                function: Some(StreamFunctionCall {
                    name: None,
                    arguments: Some("a.rs\"}".into()),
                }),
            },
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    }

    #[test]
    fn merge_chat_tool_call_grows_sparse_indices() {
        let mut calls = Vec::new();
        merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 2,
                id: Some("c".into()),
                function: Some(StreamFunctionCall {
                    name: Some("bash".into()),
                    arguments: Some("{}".into()),
                }),
            },
        );
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].id, "c");
        assert!(calls[0].id.is_empty());
    }

    /// Indices at/after the merge cap are dropped and reported, not
    /// silently swallowed — the caller warns so a truncating provider
    /// stream is debuggable.
    #[test]
    fn merge_chat_tool_call_reports_drops_past_cap() {
        let mut calls = Vec::new();
        let dropped = merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 1024,
                id: Some("late".into()),
                function: None,
            },
        );
        assert!(dropped);
        assert!(calls.is_empty());
        // In-bounds deltas still merge and report false.
        let dropped = merge_chat_tool_call(
            &mut calls,
            StreamToolCall {
                index: 0,
                id: Some("a".into()),
                function: None,
            },
        );
        assert!(!dropped);
        assert_eq!(calls[0].id, "a");
    }

    #[test]
    fn responses_input_splits_system_and_tool_output() {
        let msgs = vec![
            ChatMessage::system("sys1"),
            ChatMessage::system("sys2"),
            ChatMessage::user("hi"),
            ChatMessage::tool_result("call_1", "out"),
        ];
        let (instructions, input) = responses_input(&msgs);
        assert_eq!(instructions.unwrap(), "sys1\n\nsys2");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    /// Strict OpenAI-compatible endpoints reject dex's internal `name` tag
    /// (`messages[i]: "name" is not supported by this endpoint`) and stray
    /// `reasoning_items` from sessions that started on the Responses wire.
    #[test]
    fn chat_completions_messages_strips_internal_fields() {
        let mut tagged = ChatMessage::user_named("hi", "steering");
        tagged.reasoning_items = Some(vec![json!({"type": "reasoning", "content": "x"})]);
        let msgs = vec![
            ChatMessage::system("sys"),
            tagged,
            ChatMessage::tool_result("call_1", "out"),
        ];
        let wire: Vec<serde_json::Value> = chat_completions_messages(&msgs)
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(wire[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(wire[1], json!({"role": "user", "content": "hi"}));
        assert_eq!(
            wire[2],
            json!({"role": "tool", "content": "out", "tool_call_id": "call_1"})
        );
    }

    #[test]
    fn responses_input_encodes_assistant_tool_calls() {
        let msgs = vec![ChatMessage::assistant_calls(
            Some("thinking".into()),
            vec![LlmToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        )];
        let (instructions, input) = responses_input(&msgs);
        assert!(instructions.is_none());
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "c1");
    }

    /// Reasoning items stored on an assistant message are replayed verbatim
    /// and first, so a stateless request resumes the model's reasoning
    /// thread instead of making it re-reason.
    #[test]
    fn responses_input_replays_reasoning_items_before_content() {
        let mut msg = ChatMessage::assistant_calls(
            Some("narration".into()),
            vec![LlmToolCall {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        );
        msg.reasoning_items = Some(vec![json!({
            "type": "reasoning",
            "id": "r1",
            "summary": [],
            "encrypted_content": "blob1"
        })]);
        let msgs = vec![msg];
        let (_, input) = responses_input(&msgs);
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "blob1");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
    }

    /// Prompt-cache stability: extending the history only appends to these
    /// wire views (no message merging on this path), never rewrites earlier
    /// entries — the provider caches the prefix, and any byte change in it
    /// forces a full re-read.
    #[test]
    fn wire_views_are_append_only() {
        let mut base = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("hi"),
            ChatMessage::tool_result("call_1", "out"),
        ];
        let (instr1, input1) = responses_input(&base);
        let wire1: Vec<serde_json::Value> = chat_completions_messages(&base)
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        base.push(ChatMessage::user("follow-up"));
        let (instr2, input2) = responses_input(&base);
        let wire2: Vec<serde_json::Value> = chat_completions_messages(&base)
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        assert_eq!(instr1, instr2);
        assert_eq!(input2.len(), input1.len() + 1);
        assert_eq!(&input2[..input1.len()], &input1[..]);
        assert_eq!(wire2.len(), wire1.len() + 1);
        assert_eq!(&wire2[..wire1.len()], &wire1[..]);
    }

    #[test]
    fn wire_tools_serialize_deterministically() {
        // Consecutive serializations agree exactly: background MCP/extension
        // refreshes landing between calls must not reorder the schema.
        let tools = super::tools_schema();
        let a = super::responses_tools(&tools);
        let b = super::responses_tools(&tools);
        assert_eq!(a, b);
        let c = super::tools_schema();
        let d = super::tools_schema();
        let names_c: Vec<&str> = c.iter().map(|t| t.function.name.as_str()).collect();
        let names_d: Vec<&str> = d.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names_c, names_d);
        // Native head order is fixed (MCP/extension tail sorts behind it).
        assert!(names_c.starts_with(&["read", "bash", "write", "edit", "grep", "find", "ls"]));
    }

    #[test]
    fn wire_tool_tail_sort_is_order_independent() {
        // The production sort helpers must yield identical bytes regardless
        // of cache fill order — not just agree across consecutive calls with
        // unchanged caches.
        use crate::protocol::{FunctionDef, ToolDefinition};
        fn def(name: &str) -> ToolDefinition {
            ToolDefinition {
                tool_type: "function".to_string(),
                function: FunctionDef {
                    name: name.to_string(),
                    description: String::new(),
                    parameters: json!({}),
                },
            }
        }
        let mut fwd = vec![def("mcp__b__x"), def("ext__a__y"), def("mcp__a__z")];
        let mut rev = fwd.clone();
        rev.reverse();
        super::sort_tool_defs_by_name(&mut fwd);
        super::sort_tool_defs_by_name(&mut rev);
        let names_fwd: Vec<&str> = fwd.iter().map(|t| t.function.name.as_str()).collect();
        let names_rev: Vec<&str> = rev.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names_fwd, names_rev);
        assert_eq!(names_fwd, vec!["ext__a__y", "mcp__a__z", "mcp__b__x"]);
        // Wire JSON form: missing names sort first, never panic.
        let mut a = vec![json!({"name": "b"}), json!({}), json!({"name": "a"})];
        let mut b = a.clone();
        b.reverse();
        super::sort_wire_tools_by_name(&mut a);
        super::sort_wire_tools_by_name(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn response_tool_call_and_index_resolve_by_id() {
        let mut calls = vec![LlmToolCall {
            id: "a".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "old".into(),
                arguments: String::new(),
            },
        }];
        // Resolve existing id to index 0 even when suggested index is 5.
        let idx = response_call_index(&calls, 5, &json!({"call_id":"a"}));
        assert_eq!(idx, 0);
        // Unknown id falls back to suggested index.
        assert_eq!(
            response_call_index(&calls, 5, &json!({"call_id":"miss"})),
            5
        );
        // Append a new call via response_tool_call.
        response_tool_call(
            &mut calls,
            1,
            &json!({"call_id":"b","name":"write","arguments":"{}"}),
        );
        assert_eq!(calls[1].id, "b");
        assert_eq!(calls[1].function.name, "write");
    }
}
