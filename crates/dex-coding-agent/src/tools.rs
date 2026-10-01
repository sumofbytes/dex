//! Built-in coding-agent tool definitions and deterministic schema composition.

use dex_ai::{FunctionDef, ToolDefinition};
use serde_json::json;

/// Native schema: everything `tools_schema()` owns itself.
/// Built fresh per call (small: ~9 defs); the MCP + extension slices ride
/// alongside as borrowed `Arc`s.
pub fn builtin_tools(delegation_enabled: bool) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "read".to_string(),
                description: "Read text file contents with line numbers (reference only). Use offset/limit for large files. Batch independent reads with paths:[...] (up to 10) or glob:'src/**/*.rs' (up to 8) in ONE call.".to_string(),
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
                description: "Run a shell command (120s timeout, output truncated to 400 lines/32KB). Prefer read/grep/find over cat/grep/find; use targeted commands (grep -n, tail -N) over dumping files; use background spawn for long-running commands.".to_string(),
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
                description: "Replace exact text in a file: one oldText/newText pair or a batch of disjoint edits[] — every entry is matched against the original file, so merge nearby changes into one entry and never overlap. Each oldText must match exactly one location: include 2-3 surrounding lines for uniqueness or pass replaceAll. Matching tolerates indentation, trailing whitespace, and dash/quote variants; on failure, read the file and retry with exact text.".to_string(),
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
                description: "Fast content search (respects .gitignore). Regex when pattern has metacharacters, plain text otherwise; zero matches are retried as fuzzy. Default returns file paths; content mode gives path:line:text. Keep queries short — one term. Scope with a path ('src/ TODO') or exclude ('TODO !tests/').".to_string(),
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
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "ask_user".to_string(),
                description: "Ask the user a structured question when a decision has 2-4 well-defined options; prefer this over guessing. Ask in plain text for anything open-ended. One call may batch related questions (each gets its own picker screen). If the user dismisses a question, proceed on your own judgment instead of re-asking the same question.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 4,
                            "description": "1-4 related questions, each rendered as its own picker screen",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "question": { "type": "string", "description": "one line, the actual ask" },
                                    "header": { "type": "string", "maxLength": 12, "description": "short picker tab label, e.g. 'Database'" },
                                    "options": {
                                        "type": "array",
                                        "minItems": 2,
                                        "maxItems": 4,
                                        "description": "2-4 labeled choices; a free-text 'Other' row is added implicitly",
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "label": { "type": "string", "maxLength": 20, "description": "what the user selects" },
                                                "description": { "type": "string", "description": "one line shown under the label" }
                                            },
                                            "required": ["label", "description"]
                                        }
                                    },
                                    "multiSelect": { "type": "boolean", "description": "checkbox behavior allowing several options (default false)" },
                                    "default": { "type": "integer", "description": "0-based index into options; preselected in pickers and used by the headless empty-line shortcut" }
                                },
                                "required": ["question", "header", "options"]
                            }
                        }
                    },
                    "required": ["questions"]
                }),
            },
        },
    ];
    // Sub-agent delegation (§10): background spawn + bounded wait + stop.
    // Registered only in daemon-linked processes with the kill switch unset;
    // OneShot/direct runs reject it at dispatch (no manager to spawn into).
    // One tool with an `action` — spawn/wait/stop/list share one definition,
    // so the schema stays small and the four verbs stay discoverable.
    if delegation_enabled {
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "delegate".to_string(),
                description: "Sub-agents (spawn/wait/stop/list). spawn runs a task as a background child (explorer: code understanding; reviewer: review; tester: runs tests) and returns its agent_id. Children never see this conversation — pass a self-contained task + file_hints.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["spawn", "wait", "stop", "list"], "description": "spawn a child, wait for/fetch its result, stop it, or list this session's children" },
                        "agent": { "type": "string", "description": "spawn: agent name — explorer | reviewer | tester" },
                        "task": { "type": "string", "description": "spawn: what the child must do, self-contained: findings, file paths, risks; it cannot see this conversation; required unless resume_from is set" },
                        "file_hints": { "type": "array", "items": { "type": "string" }, "description": "spawn: workspace-relative paths the child should start from" },
                        "model": { "type": "string", "description": "spawn: optional model override (provider/model, same knob as --model); omit to inherit this turn's model" },
                        "resume_from": { "type": "string", "description": "spawn: prior agent_id to resume from its transcript as a new generation" },
                        "instruction": { "type": "string", "description": "spawn: refined instruction folded into the resume continuation note" },
                        "agent_id": { "type": "string", "description": "wait/stop: id returned by a spawn" },
                        "wait_seconds": { "type": "integer", "description": "wait: how long to wait for completion (0-120, default 0)" }
                    },
                    "required": ["action"]
                }),
            },
        });
        tools.push(ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: "background".to_string(),
                description: "Background shell tasks (spawn/output/wait/stop/list). spawn detaches and returns a task id; output pages past a byte cursor; wait blocks up to timeout_secs.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["spawn", "output", "stop", "wait", "list"], "description": "spawn a task, read its output, stop it, wait for exit, or list this session's tasks" },
                        "command": { "type": "string", "description": "spawn: shell command, same grammar as bash" },
                        "id": { "type": "string", "description": "output/stop/wait: task id (task-1)" },
                        "cursor": { "type": "integer", "description": "output/wait: byte offset into total_written from a previous result" },
                        "timeout_secs": { "type": "integer", "description": "wait: how long to wait for exit (0-120, default 30); 0 polls once" }
                    },
                    "required": ["action"]
                }),
            },
        });
    }
    tools
}

// Prompt-side usage rules (pi-style description/guideline split): the schema
// `description` keeps call-time mechanics at each tool decision; these
// behavioral rules ride the system prompt once instead of in every schema blob.
pub fn native_tool_guidelines(delegation_enabled: bool) -> Vec<&'static str> {
    if !delegation_enabled {
        return Vec::new();
    }
    vec![
        "Sub-agents (delegate) never see this conversation — write self-contained tasks, file_hints give starting paths",
        "Delegate spawn returns immediately and completions announce at the next turn boundary — wait only when the result gates the next step",
        "Pass delegate resume_from on a finished child to continue from its transcript; inherit the turn's model unless the task needs a different trade-off",
        "Background tasks announce at the next turn boundary too — poll output/wait only when the result gates the next step",
    ]
}

/// Merge optional host-provided tools after native tools in stable name order.
pub fn merge_tool_schemas(
    mut native: Vec<ToolDefinition>,
    mcp: &[ToolDefinition],
    extensions: &[ToolDefinition],
) -> Vec<ToolDefinition> {
    let mut tail: Vec<ToolDefinition> = mcp.iter().chain(extensions).cloned().collect();
    sort_tool_defs_by_name(&mut tail);
    native.extend(tail);
    native
}

/// Sort dynamic tools by name to keep provider request schemas deterministic.
pub fn sort_tool_defs_by_name(tail: &mut [ToolDefinition]) {
    tail.sort_by(|a, b| a.function.name.cmp(&b.function.name));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: String::new(),
                parameters: serde_json::json!({}),
            },
        }
    }

    #[test]
    fn builtins_keep_fixed_order_and_gate_delegation() {
        let without_delegation = builtin_tools(false);
        let names: Vec<_> = without_delegation
            .iter()
            .map(|definition| definition.function.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["read", "bash", "write", "edit", "grep", "find", "ls", "ask_user"]
        );

        let with_delegation = builtin_tools(true);
        assert_eq!(with_delegation.len(), without_delegation.len() + 2);
        assert_eq!(
            with_delegation[with_delegation.len() - 2].function.name,
            "delegate"
        );
        assert_eq!(with_delegation.last().unwrap().function.name, "background");
    }

    #[test]
    fn dynamic_tools_merge_in_stable_sorted_tail() {
        let merged = merge_tool_schemas(
            vec![tool("read")],
            &[tool("mcp__z__tool"), tool("mcp__a__tool")],
            &[tool("ext__x__tool")],
        );
        let names: Vec<_> = merged
            .iter()
            .map(|definition| definition.function.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["read", "ext__x__tool", "mcp__a__tool", "mcp__z__tool"]
        );
    }
}
