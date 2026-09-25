//! Workspace inspection (ls).

use std::sync::Arc;

use serde_json::{Map, Value};

use crate::protocol::PermissionMode;
use crate::render::format::clamp_lines;
use crate::runtime::console::Console;

use super::workspace_path;
use super::ToolError;

pub async fn tool_ls(args: &Map<String, Value>) -> Result<String, ToolError> {
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
pub struct Policy {
    pub mode: PermissionMode,
    pub console: Option<Console>,
    /// The daemon-backed turn context (Phase 5): `Some` only for parent
    /// turns inside the daemon. It is what makes the delegation tools
    /// spawnable; children and every non-daemon path carry `None`, so a
    /// `delegate` call from either is rejected at dispatch (§11, no
    /// recursion — the depth cap in code, not in the prompt).
    pub agent: Option<Arc<crate::agent::delegate::AgentTurnContext>>,
}
