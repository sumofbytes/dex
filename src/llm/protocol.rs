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
    // Six default tools. `chain` and `git`
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
                description: "Write or overwrite a file (parent directories are created). For surgical changes to existing files, prefer `edit`. Pass then_run to verify the change in the same call — its output comes back in this result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "then_run": { "type": "string", "description": "optional shell command to run in this same call after a successful write (e.g. a build, formatter or test); its output is appended to this result. Skipped, and never reported as if it ran, when the write fails." }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "edit".to_string(),
                description: "Replace text in a file. oldText must match exactly one location — include 2-3 surrounding lines to make it unique, or pass replaceAll for every occurrence. Whitespace-only mismatches are retried line-wise. On failure, read the file and retry with exact text. Pass then_run to verify the change in the same call — its output comes back in this result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string", "description": "exact existing text to replace" },
                        "newText": { "type": "string", "description": "replacement text" },
                        "replaceAll": { "type": "boolean", "description": "replace every occurrence instead of requiring exactly one (default false)" },
                        "then_run": { "type": "string", "description": "optional shell command to run in this same call after a successful edit (e.g. a build, formatter or test); its output is appended to this result. Skipped, and never reported as if it ran, when the edit fails." }
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
"file_offset": { "type": "integer", "description": "file index to resume from when a result ends with a '[... shown, more files unscanned; continue with file_offset ...]' trailer" },
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
    // Sub-agent delegation (§10): background spawn + bounded wait + stop.
    // Registered only in daemon-linked processes with the kill switch unset;
    // OneShot/direct runs reject them at dispatch (no manager to spawn into).
    // Descriptions carry the usage guidance (AGENTS.md: behavior detail
    // lives at the tool decision, not in prompt.rs).
    if crate::agent::subagent::delegation_enabled() {
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "delegate".to_string(),
                description: "Delegate a task to a background sub-agent and return its agent_id immediately — it never blocks this turn. Available agents: explorer (understand code, read-only), reviewer (review a change, read-only), tester (run tests; its shell runs only under a trusted permission policy). The child gets only the task you write plus optional file hints, never this conversation; it runs with its own tool set and reports its final message back. Completions are announced automatically at the next turn boundary — don't poll unless you need the result before continuing.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent": { "type": "string", "description": "agent name: explorer | reviewer | tester" },
                        "task": { "type": "string", "description": "what the child must do, self-contained: findings, file paths, risks; it cannot see this conversation" },
                        "file_hints": { "type": "array", "items": { "type": "string" }, "description": "workspace-relative paths the child should start from" }
                    },
                    "required": ["agent", "task"]
                }),
            },
        });
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "delegate_output".to_string(),
                description: "Fetch a delegated child's result. Returns the terminal result (status, summary, error) as soon as it is done; otherwise the current state plus what it is running now. wait_seconds (0-120, default 0) bounds the wait: 0 polls and returns immediately. The wait returns early if this turn is cancelled; steering sent while waiting is acted on right after it returns. Finished results stay fetchable after their announcement.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "id returned by delegate" },
                        "wait_seconds": { "type": "integer", "description": "how long to wait for completion (0-120, default 0)" }
                    },
                    "required": ["agent_id"]
                }),
            },
        });
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "delegate_stop".to_string(),
                description: "Cancel a running delegated child and return its terminal result (status cancelled). Safe on ids that already finished: it returns their recorded result instead.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "id returned by delegate" }
                    },
                    "required": ["agent_id"]
                }),
            },
        });
    }
    // Online context compaction: the working-plan tool whose
    // completed steps are compaction boundaries. Gated like the extra tools —
    // it costs prompt tokens on every request and only pays off on
    // long-horizon work.
    if crate::agent::online::online_compaction_enabled() {
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "update_plan".to_string(),
                description: "Replace the complete working plan. A newly completed step becomes a safe point where dex may compact context if doing so is economical. Send the complete plan on every call; keep at most one step in_progress and mark finished steps completed; when completing a step, include concise progress evidence when available.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "steps": {
                            "type": "array",
                            "maxItems": 128,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string", "description": "stable step id; reuse only for the same goal" },
                                    "goal": { "type": "string" },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                                },
                                "required": ["id", "goal", "status"],
                                "additionalProperties": false
                            }
                        },
                        "progress": {
                            "type": "object",
                            "properties": {
                                "files_changed": { "type": "array", "items": { "type": "string" } },
                                "verification": { "type": "array", "items": { "type": "string" }, "description": "checks run and their outcome" },
                                "decisions": { "type": "array", "items": { "type": "string" } }
                            }
                        }
                    },
                    "required": ["steps"]
                }),
            },
        });
    }
    // Observation pack recall: the pull-back side of the projection.
    // Gated like the other prompt-token-costing tools — only registered
    // when the packer itself is on, so the schema cost tracks the feature.
    if crate::agent::obs_pack::observation_pack_enabled() {
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "obs_recall".to_string(),
                description: "Read a stored large tool result by observation id and byte offset. Older large tool results in this conversation were replaced with placeholders; recall a paged excerpt from the placeholder's id when you need the original content again. Continue with the returned next_offset.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "observation id from a placeholder" },
                        "offset": { "type": "integer", "description": "byte offset, default 0" }
                    },
                    "required": ["id"]
                }),
            },
        });
    }
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

/// Chat-completions wire messages: serialized from [`ChatMessage`] minus
/// dex-internal fields. `name` is a local tag (`steering`, `skill`,
/// `summary`, `follow-up`, `agent-notifications`) that the model never needs
/// and strict OpenAI-compatible endpoints reject (`messages[i]: "name" is
/// not supported by this endpoint`). `reasoning_items` are Responses-API
/// blobs the Responses wire replays inside `input` (and the Anthropic wire
/// filters in `assistant_blocks`) — as a chat-completions field they would
/// be garbage. `reasoning_content` stays: it is the model-facing DeepSeek
/// field.
pub(crate) fn chat_completions_messages(messages: &[ChatMessage]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| {
            let mut wire = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
            if let Some(object) = wire.as_object_mut() {
                object.remove("name");
                object.remove("reasoning_items");
            }
            wire
        })
        .collect()
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
        chat_completions_messages, merge_chat_tool_call, response_call_index, response_tool_call,
        responses_input, tools_schema, ChatMessage, FunctionCall, LlmToolCall, StreamToolCall,
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
        // Serializes against tests in other modules that flip the env vars
        // these gates read (online compaction, extra tools).
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = crate::session::EnvGuard(vec![
            (
                crate::agent::online::ONLINE_COMPACTION_ENV,
                std::env::var_os(crate::agent::online::ONLINE_COMPACTION_ENV),
            ),
            ("DEX_EXTRA_TOOLS", std::env::var_os("DEX_EXTRA_TOOLS")),
            (
                "DEX_OBSERVATION_PACK",
                std::env::var_os("DEX_OBSERVATION_PACK"),
            ),
        ]);
        std::env::remove_var("DEX_ONLINE_COMPACTION");
        std::env::remove_var("DEX_EXTRA_TOOLS");
        std::env::remove_var("DEX_OBSERVATION_PACK");
        let schema = tools_schema();
        let names: Vec<_> = schema.iter().map(|t| t.function.name.as_str()).collect();
        let expected: Vec<&str> = vec!["read", "bash", "write", "edit", "grep", "find", "ls"];
        assert_eq!(names, expected);

        // DEX_ONLINE_COMPACTION=1 adds the plan tool.
        std::env::set_var("DEX_ONLINE_COMPACTION", "1");
        let names: Vec<String> = tools_schema()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(names.iter().any(|n| n == "update_plan"), "{names:?}");

        // DEX_OBSERVATION_PACK=1 adds the recall tool.
        std::env::set_var("DEX_OBSERVATION_PACK", "1");
        let names: Vec<String> = tools_schema()
            .iter()
            .map(|t| t.function.name.clone())
            .collect();
        assert!(names.iter().any(|n| n == "obs_recall"), "{names:?}");
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
        let wire = chat_completions_messages(&msgs);
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
