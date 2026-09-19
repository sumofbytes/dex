//! Workspace inspection (ls), git, and tool chaining.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::protocol::PermissionMode;
use crate::runtime::cancel::CancellationSource;
use crate::runtime::console::Console;
use crate::ui::format::clamp_lines;

use super::read::{READ_FANOUT_MAX_FILES, READ_MAX_BYTES, READ_MAX_LINES};
use super::shell::{run_bash, BASH_CLAMP_BYTES, BASH_CLAMP_LINES};
use super::{execute, metadata, workspace_path, PermissionRequirement, ToolError, ToolFilter};

pub(crate) async fn tool_ls(args: &Map<String, Value>) -> Result<String, ToolError> {
    let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let path = workspace_path(raw)?;
    let meta = tokio::fs::metadata(&path).await.map_err(ToolError::Io)?;
    if !meta.is_dir() {
        return Ok(path.display().to_string());
    }
    let mut dir = tokio::fs::read_dir(&path).await.map_err(ToolError::Io)?;
    let mut entries: Vec<String> = Vec::new();
    while let Some(entry) = dir.next_entry().await.map_err(ToolError::Io)? {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        // is_dir via file_type to avoid extra metadata call; fallback to path check.
        let is_dir = entry
            .file_type()
            .await
            .map(|ft| ft.is_dir())
            .unwrap_or_else(|_| p.is_dir());
        if is_dir {
            entries.push(format!("{name}/"));
        } else {
            entries.push(name);
        }
    }
    entries.sort();
    if entries.is_empty() {
        return Ok("(empty)".to_string());
    }
    // Clamp to avoid flooding context.
    let limited = clamp_lines(&entries.join("\n"), 500, 32 * 1024);
    Ok(limited)
}

pub(crate) async fn tool_git(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("status");
    if !matches!(mode, "status" | "diff") {
        return Err(ToolError::NotString("mode (status or diff)"));
    }
    // Routed through run_bash so git shares the shell timeout, cancellation,
    // capture limits, and clamping instead of running unbounded.
    let (output, code) = run_bash(&format!("git --no-pager {mode}"), cancel).await?;
    match code {
        Some(0) | Some(1) => Ok(clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES)),
        code => Err(ToolError::Shell {
            output: clamp_lines(&output, BASH_CLAMP_LINES, BASH_CLAMP_BYTES),
            code,
        }),
    }
}

/// Maximum steps in one chain: enough for search → read → search → read,
/// small enough to stay predictable.
const CHAIN_MAX_STEPS: usize = 4;

/// A bounded, read-only chain executed in ONE LLM round trip — dex's scoped
/// take on programmatic tool calling. The model declares steps; routing
/// between steps is mechanical (`from` a search step, `take: "paths"` into a
/// read fan-out), never semantic: the model cannot branch or transform
/// mid-chain, and mutation/shell tools are refused. On a step failure the
/// earlier steps' outputs ship with the error, so the round trip still
/// carries information.
pub(crate) async fn tool_chain(
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(ToolError::Missing("steps"))?;
    if steps.len() < 2 {
        return Err(ToolError::InvalidArgument(
            "chain needs at least 2 steps; a single tool call does not need a chain".to_string(),
        ));
    }
    if steps.len() > CHAIN_MAX_STEPS {
        return Err(ToolError::InvalidArgument(format!(
            "chain supports at most {CHAIN_MAX_STEPS} steps (got {})",
            steps.len()
        )));
    }

    let mut completed: Vec<(String, String)> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        match run_chain_step(step, index, &completed, cancel, policy, filter).await {
            Ok(pair) => completed.push(pair),
            Err(error) => {
                let mut text = render_chain_steps(&completed);
                text.push_str(&format!("\n--- step {index} failed ---\nError: {error}\n"));
                return Err(ToolError::Shell {
                    output: clamp_lines(&text, READ_MAX_LINES, READ_MAX_BYTES),
                    code: None,
                });
            }
        }
    }
    Ok(clamp_lines(
        &render_chain_steps(&completed),
        READ_MAX_LINES,
        READ_MAX_BYTES,
    ))
}

fn render_chain_steps(steps: &[(String, String)]) -> String {
    let mut out = String::new();
    for (index, (tool, output)) in steps.iter().enumerate() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("--- step {index}: {tool} ---\n{output}"));
    }
    out
}

async fn run_chain_step(
    step: &Value,
    index: usize,
    completed: &[(String, String)],
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<(String, String), ToolError> {
    let invalid = |message: String| ToolError::InvalidArgument(format!("step {index}: {message}"));
    let obj = step
        .as_object()
        .ok_or_else(|| invalid("must be an object".to_string()))?;
    let tool = obj
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("is missing 'tool'".to_string()))?;
    let meta = metadata(tool).ok_or_else(|| invalid(format!("unknown tool '{tool}'")))?;
    if meta.permission != PermissionRequirement::Read {
        return Err(invalid(format!(
            "chain is read-only; '{tool}' must run as its own approved call"
        )));
    }

    let mut step_args = match obj.get("args") {
        Some(Value::Object(map)) => map.clone(),
        None => Map::new(),
        Some(_) => return Err(invalid("'args' must be an object".to_string())),
    };

    if let Some(from) = obj.get("from") {
        let from = from
            .as_u64()
            .map(|n| n as usize)
            .ok_or_else(|| invalid("'from' must be the index of an earlier step".to_string()))?;
        if from >= index {
            return Err(invalid("'from' must reference an earlier step".to_string()));
        }
        if obj.get("take").and_then(Value::as_str) != Some("paths") {
            return Err(invalid(
                "'take' must be \"paths\" (routes a search step's matched files into read)"
                    .to_string(),
            ));
        }
        if tool != "read" {
            return Err(invalid("'from' routing requires the read tool".to_string()));
        }
        let (source_tool, source_output) = &completed[from];
        if !matches!(
            source_tool.as_str(),
            "grep" | "ffgrep" | "find" | "fffind" | "chain"
        ) {
            return Err(invalid(format!(
                "'from' step {from} is '{source_tool}', which produces no file paths; use grep (files mode) or find"
            )));
        }
        let max_files = obj
            .get("max_files")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, READ_FANOUT_MAX_FILES))
            .unwrap_or(5);
        let paths = extract_search_paths(source_output, max_files, from)?;
        step_args.remove("path");
        step_args.insert(
            "paths".to_string(),
            Value::Array(
                paths
                    .into_iter()
                    .map(|path| Value::String(path.display().to_string()))
                    .collect(),
            ),
        );
    }

    let output = Box::pin(execute(tool, &step_args, cancel, policy, filter)).await?;
    Ok((tool.to_string(), output))
}

/// Pull file paths out of a search step's output (grep files-mode lines,
/// find output) and resolve them within the workspace.
fn extract_search_paths(
    output: &str,
    max_files: usize,
    from: usize,
) -> Result<Vec<PathBuf>, ToolError> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        // Skip truncation markers and chain section labels.
        if line.is_empty() || line.starts_with('[') || line.starts_with("---") {
            continue;
        }
        if let Ok(path) = workspace_path(line) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        if paths.len() >= max_files {
            break;
        }
    }
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "step {from} output contained no resolvable file paths; widen the search or raise head_limit"
        )));
    }
    Ok(paths)
}

/// The permission context a tool call runs under (Phase 0 gate). `mode`
/// is the turn's `PermissionMode` (from `LlmConfig::permission`);
/// `console` carries the approval channel plus the live session-approval
/// set. Owned (not borrowed) so concurrent fan-out tasks can each hold a
/// copy; the console clone shares the turn's live session-approval set, so
/// build one `Policy` per turn and reuse it for the whole turn — same-turn
/// allow-for-session records are then visible to every later call.
/// `Policy::trusted()` (no console) preserves the old behavior for explicit
/// user-invoked paths (`dex run`, `--tool`, the `!` escape): the `!` itself
/// is the approval there.
#[derive(Clone)]
pub(crate) struct Policy {
    pub(crate) mode: PermissionMode,
    pub(crate) console: Option<Console>,
    /// The daemon-backed turn context (Phase 5): `Some` only for parent
    /// turns inside the daemon. It is what makes the delegation tools
    /// spawnable; children and every non-daemon path carry `None`, so a
    /// `delegate` call from either is rejected at dispatch (§11, no
    /// recursion — the depth cap in code, not in the prompt).
    pub(crate) agent: Option<Arc<crate::agent::subagent::AgentTurnContext>>,
}
