use serde_json::{json, Value};

use crate::core::types::{
    ChatMessage, FunctionCall, FunctionDef, LlmToolCall, Role, StreamToolCall, ToolDefinition,
};

pub(crate) fn merge_chat_tool_call(calls: &mut Vec<LlmToolCall>, delta: StreamToolCall) {
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
}

pub(crate) fn tools_schema() -> Vec<ToolDefinition> {
    // Industry harnesses (pi, claude) ship 4-6 tools. `chain` and `git`
    // cost ~800 prompt tokens per request and are rarely used — `read`
    // fan-out + parallel calls cover the same, and `git` is reachable via
    // `bash "git ..."`. Gate them behind DEX_EXTRA_TOOLS=1 for compat.
    let extra = std::env::var("DEX_EXTRA_TOOLS").as_deref() == Ok("1");
    let mut tools = vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "read".to_string(),
                description: "Read file contents with line numbers. Use offset/limit for large files. Batch independent reads with paths:[...] or glob:'src/**/*.rs' (up to 10 files) in ONE call.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "relative path to a single file" },
                        "paths": { "type": "array", "items": { "type": "string" }, "description": "several files to read in one call (max 10)" },
                        "glob": { "type": "string", "description": "glob fan-out, e.g. 'src/tools/*.rs' or '*.rs' (max 8 files, sorted)" },
                        "offset": { "type": "integer", "description": "1-based line to start from (default 1)" },
                        "limit": { "type": "integer", "description": "maximum lines per file (default 2000 single-file, 200 multi-file)" }
                    },
                    "required": []
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "bash".to_string(),
                description: "Run a shell command. Output is capped. Prefer read/grep/find over cat/grep/find; use targeted commands (grep -n, tail -N) over dumping files.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string", "description": "shell command to run" } },
                    "required": ["command"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "write".to_string(),
                description: "Write or overwrite a file (parent directories are created). For surgical changes to existing files, prefer `edit`.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "edit".to_string(),
                description: "Replace text in a file. oldText must match exactly one location — include 2-3 surrounding lines to make it unique, or pass replaceAll for every occurrence. Whitespace-only mismatches are retried line-wise. On failure, read the file and retry with exact text.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string", "description": "exact existing text to replace" },
                        "newText": { "type": "string", "description": "replacement text" },
                        "replaceAll": { "type": "boolean", "description": "replace every occurrence instead of requiring exactly one (default false)" }
                    },
                    "required": ["path", "oldText", "newText"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "grep".to_string(),
                description: "Fast content search (respects .gitignore). Regex when pattern has metacharacters, plain text otherwise; zero matches are retried as fuzzy. Default returns file paths; content mode gives path:line:text. Keep queries short — one term. Use path prefix 'src/ TODO' or exclude 'TODO !test/'.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "text or regex to search; may include path prefixes ('src/') and excludes ('!tests/')" },
                        "output_mode": { "type": "string", "enum": ["files", "content"], "description": "files (default): paths only; content: path:line:text" },
                        "head_limit": { "type": "integer", "description": "maximum results (default 50)" },
                        "context": { "type": "integer", "description": "lines of context around each match in content mode (0-10, default 0)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "find".to_string(),
                description: "Fuzzy file search (respects .gitignore, typo-tolerant). Matches workspace-relative paths; supports globs '**/*.rs'. Keep queries short — 1-2 terms.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "fuzzy path query such as 'main', 'tools fff', or 'src/**/*.rs'; never empty or '*' alone" },
                        "limit": { "type": "integer", "description": "maximum paths returned (default 20)" }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "ls".to_string(),
                description: "List files and directories. Shows entries in the given path (default '.'). Use to explore project structure.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "directory to list (default '.')" }
                    },
                    "required": []
                }),
            },
        },
    ];
    if extra {
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "git".to_string(),
                description: "Inspect repository status or diff (read-only).".to_string(),
                parameters: json!({"type":"object","properties":{"mode":{"type":"string","enum":["status","diff"]}}}),
            },
        });
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "chain".to_string(),
                description: "Run a bounded read-only sequence in ONE round trip: a search step (grep files-mode or find) followed by read steps that consume the matched files via from/take. Use when later steps depend on earlier output; for independent calls, batch them as parallel calls instead. Mutating and shell tools are not allowed in chains.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "maxItems": 4,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string", "description": "read, grep, find, or git" },
                                    "args": { "type": "object", "description": "arguments passed to that tool" },
                                    "from": { "type": "integer", "description": "index of an earlier step whose matched files this read consumes" },
                                    "take": { "type": "string", "enum": ["paths"], "description": "route the referenced step's file paths into this read" },
                                    "max_files": { "type": "integer", "description": "cap on files read when routing from a search step (default 5, max 10)" }
                                },
                                "required": ["tool"]
                            }
                        }
                    },
                    "required": ["steps"]
                }),
            },
        });
    }
    // MCP tools merge from the background-refreshed cache: sync, never
    // blocks the turn loop. Empty until the first refresh lands.
    tools.extend(crate::mcp::cached_tools());
    tools
}

pub(crate) fn responses_input(messages: &[ChatMessage]) -> (Option<String>, Vec<Value>) {
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

pub(crate) fn responses_tools() -> Vec<Value> {
    tools_schema()
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": tool.function.parameters,
            })
        })
        .collect()
}

pub(crate) fn response_tool_call(calls: &mut Vec<LlmToolCall>, index: usize, item: &Value) {
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
}

pub(crate) fn response_call_index(calls: &[LlmToolCall], index: usize, item: &Value) -> usize {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .and_then(|id| calls.iter().position(|call| call.id == id))
        .unwrap_or(index)
}

#[cfg(test)]
mod tests {
    use super::{
        merge_chat_tool_call, response_call_index, response_tool_call, responses_input,
        tools_schema, ChatMessage, FunctionCall, LlmToolCall, StreamToolCall,
    };
    use crate::core::types::StreamFunctionCall;
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

    #[test]
    fn tools_schema_contains_all_tools() {
        let schema = tools_schema();
        let names: Vec<_> = schema.iter().map(|t| t.function.name.as_str()).collect();
        // Default is 6 tools (pi parity); DEX_EXTRA_TOOLS=1 adds git+chain
        if std::env::var("DEX_EXTRA_TOOLS").as_deref() == Ok("1") {
            assert_eq!(
                names,
                ["read", "bash", "write", "edit", "grep", "find", "ls", "git", "chain"]
            );
        } else {
            assert_eq!(
                names,
                ["read", "bash", "write", "edit", "grep", "find", "ls"]
            );
        }
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
