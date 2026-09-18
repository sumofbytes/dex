use serde_json::{json, Value};
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;

use crate::core::types::{
    ChatMessage, FunctionCall, FunctionDef, LlmToolCall, Role, StreamToolCall, ToolDefinition,
    WireMessage,
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

pub(crate) fn sort_tool_defs_by_name(tail: &mut [ToolDefinition]) {
    tail.sort_by(|a, b| a.function.name.cmp(&b.function.name));
}

pub(crate) fn sort_wire_tools_by_name(tail: &mut [Value]) {
    tail.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
}

pub(crate) fn tools_schema() -> Vec<ToolDefinition> {
    // One merged copy per request: `ChatRequest.tools` owns its vec. Callers
    // that serialize straight to `Value` use `tools_schema_parts` and skip
    // even this copy. Native order is fixed; the MCP + extension tail is
    // sorted by name so refresh completion order can't reorder the schema
    // (deterministic bytes; adding/removing a tool still shifts the tail).
    let (mut tools, mcp, ext) = tools_schema_parts();
    let mut tail: Vec<ToolDefinition> = mcp.iter().cloned().chain(ext.iter().cloned()).collect();
    sort_tool_defs_by_name(&mut tail);
    tools.extend(tail);
    tools
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

/// Native + experiment schema: everything `tools_schema()` owns itself.
/// Built fresh per call (small: ~11 defs); the MCP + extension slices ride
/// alongside as borrowed `Arc`s.
fn native_tools() -> Vec<ToolDefinition> {
    // Six default tools. `chain` and `git`
    // cost ~800 prompt tokens per request and are rarely used — `read`
    // fan-out + parallel calls cover the same, and `git` is reachable via
    // `bash "git ..."`. Gate them behind DEX_EXTRA_TOOLS=1 for compat.
    let extra = extra_tools_enabled();
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
                description: "Replace text in a file: one oldText/newText pair, or a batch of disjoint replacements via edits[] (every entry is matched against the original file — merge nearby changes into one entry, never overlap). Each oldText must match exactly one location — include 2-3 surrounding lines to make it unique, or pass replaceAll for every occurrence. Matching ignores indentation, trailing whitespace, and quote/dash variants. On failure, read the file and retry with exact text. Pass then_run to verify the change in the same call — its output comes back in this result.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string", "description": "exact existing text to replace" },
                        "newText": { "type": "string", "description": "replacement text" },
                        "edits": { "type": "array", "description": "batch of disjoint {oldText, newText} replacements applied in one call; pass either this or oldText/newText, not both", "items": { "type": "object", "properties": { "oldText": { "type": "string" }, "newText": { "type": "string" } }, "required": ["oldText", "newText"] } },
                        "replaceAll": { "type": "boolean", "description": "replace every occurrence instead of requiring exactly one (default false)" },
                        "then_run": { "type": "string", "description": "optional shell command to run in this same call after a successful edit (e.g. a build, formatter or test); its output is appended to this result. Skipped, and never reported as if it ran, when the edit fails." }
                    },
                    "required": ["path"]
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
                  description: "Delegate a task to a background sub-agent and return its agent_id immediately — it never blocks this turn. Available agents: explorer (understand code, read-only), reviewer (review a change, read-only), tester (run tests; its shell runs only under a trusted permission policy). The child gets only the task you write plus optional file hints, never this conversation; it runs with its own tool set and reports its final message back. Completions are announced automatically at the next turn boundary — don't poll unless you need the result before continuing. Pass resume_from (a prior agent_id) to continue a resumable child from its transcript as a new generation — task is then optional and instruction plus file_hints fold into the continuation note; delegate_list shows resumable children. Omit model to inherit this turn's model (on resume, to keep the prior generation's model); pass provider/model only when the task's complexity needs a different trade-off (stronger for hard reasoning, cheaper for simple lookups).".to_string(),
                  parameters: json!({
                      "type": "object",
                      "properties": {
                          "agent": { "type": "string", "description": "agent name: explorer | reviewer | tester" },
                          "task": { "type": "string", "description": "what the child must do, self-contained: findings, file paths, risks; it cannot see this conversation; required unless resume_from is set" },
                          "file_hints": { "type": "array", "items": { "type": "string" }, "description": "workspace-relative paths the child should start from" },
                          "model": { "type": "string", "description": "optional model override (provider/model, same knob as --model); omit to inherit this turn's model" },
                          "resume_from": { "type": "string", "description": "prior agent_id to resume from its transcript as a new generation" },
                          "instruction": { "type": "string", "description": "refined instruction folded into the resume continuation note" }
                      },
                      "required": ["agent"]
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
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "delegate_list".to_string(),
                description: "List this session's sub-agent children: live ones with their progress, finished ones with status and resumability, and interrupted on-disk runs a daemon restart left behind. Read-only. Use it when spawn-result lines scrolled away or after compaction.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
            },
        });
    }
    // Experiment tools (the online compaction plan tool, the observation
    // pack recall tool): owned by each experiment module and registered
    // through the experiment registry — protocol.rs never names a gate.
    tools.extend(crate::agent::experiments::tool_definitions());
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
    tools
}

/// `DEX_EXTRA_TOOLS` flag, cached process-wide: `tools_schema()` runs per
/// model call and the env lookup is pure overhead after boot. Tests bypass
/// the cache — they flip the var mid-process and expect the schema to
/// follow (see the experiments schema test).
fn extra_tools_enabled() -> bool {
    #[cfg(test)]
    {
        std::env::var("DEX_EXTRA_TOOLS").as_deref() == Ok("1")
    }
    #[cfg(not(test))]
    {
        static EXTRA_TOOLS: OnceLock<bool> = OnceLock::new();
        *EXTRA_TOOLS.get_or_init(|| std::env::var("DEX_EXTRA_TOOLS").as_deref() == Ok("1"))
    }
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
pub(crate) fn chat_completions_messages(messages: &[ChatMessage]) -> Vec<WireMessage<'_>> {
    messages.iter().map(ChatMessage::wire).collect()
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
    wire_tools(|tool| {
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
pub(crate) fn wire_tools(map: impl Fn(&ToolDefinition) -> Value) -> Vec<Value> {
    let (native, mcp, ext) = tools_schema_parts();
    let mut out: Vec<Value> = native.iter().map(&map).collect();
    let mut tail: Vec<Value> = mcp.iter().chain(ext.iter()).map(map).collect();
    sort_wire_tools_by_name(&mut tail);
    out.extend(tail);
    out
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
        responses_input, ChatMessage, FunctionCall, LlmToolCall, StreamToolCall,
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
        let a = super::responses_tools();
        let b = super::responses_tools();
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
        use crate::core::types::{FunctionDef, ToolDefinition};
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
