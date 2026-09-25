//! Deterministic compaction fallback (`DEX_COMPACTION=llm` off): builds the
//! structured checkpoint summary (Goal / Constraints / Progress / Key Decisions /
//! Next Steps / Critical Context) without an LLM call, plus the file-ops
//! extraction shared with the LLM summary path.

use std::collections::HashSet;

use crate::truncate_text;
use dex_ai::{ChatMessage, Role};

// File tracking — read/written/edited sets extracted from tool calls
#[derive(Default)]
pub struct FileOps {
    read: HashSet<String>,
    written: HashSet<String>,
    edited: HashSet<String>,
}

pub fn extract_file_ops_from_message(msg: &ChatMessage, ops: &mut FileOps) {
    let Some(calls) = &msg.tool_calls else {
        return;
    };
    if msg.role != Role::Assistant {
        return;
    }
    for call in calls {
        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            continue;
        };
        match call.function.name.as_str() {
            "read" => {
                ops.read.insert(path.to_string());
            }
            "write" => {
                ops.written.insert(path.to_string());
            }
            "edit" => {
                ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

fn compute_file_lists(ops: &FileOps) -> (Vec<String>, Vec<String>) {
    let mut modified: HashSet<String> = HashSet::new();
    modified.extend(ops.edited.iter().cloned());
    modified.extend(ops.written.iter().cloned());
    let mut read_only: Vec<String> = ops
        .read
        .iter()
        .filter(|p| !modified.contains(*p))
        .cloned()
        .collect();
    let mut modified_files: Vec<String> = modified.into_iter().collect();
    read_only.sort();
    modified_files.sort();
    (read_only, modified_files)
}

fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

/// Deterministic fallback when the LLM summarizer fails or is cancelled.
/// Structured summary: Goal, Constraints, Progress, Key Decisions, Next Steps, Critical Context,
/// plus `<read-files>`/`<modified-files>`. Keeps file ops and verification failures.
pub fn deterministic_summary(
    old: &[ChatMessage],
    turn_prefix: &[ChatMessage],
    previous_summary: Option<&str>,
    file_ops: &FileOps,
) -> String {
    // Extract goal: prefer plan, then previous summary, then first user
    let mut goal: Option<String> = None;
    for msg in old.iter().chain(turn_prefix.iter()) {
        if msg.name.as_deref() == Some("plan") && goal.is_none() {
            if let Some(c) = &msg.content {
                goal = Some(truncate_text(c, 800, 10));
            }
        }
    }
    if goal.is_none() {
        if let Some(prev) = previous_summary {
            goal = Some(truncate_text(prev, 800, 10));
        }
    }
    if goal.is_none() {
        if let Some(first) = old
            .iter()
            .chain(turn_prefix.iter())
            .find(|m| m.role == Role::User && m.name.as_deref() != Some("summary"))
        {
            if let Some(c) = &first.content {
                goal = Some(truncate_text(c, 600, 8));
            }
        }
    }
    let goal = goal.unwrap_or_else(|| {
        format!(
            "Truncated {} earlier messages (no goal extracted).",
            old.len() + turn_prefix.len()
        )
    });

    // Constraints: collect plan constraints verbatim if any
    let mut constraints: Vec<String> = Vec::new();
    for msg in old.iter().chain(turn_prefix.iter()) {
        if msg.name.as_deref() == Some("plan") {
            if let Some(c) = &msg.content {
                // naive: split lines that look like constraints
                for line in c.lines().take(5) {
                    let t = line.trim();
                    if !t.is_empty() && constraints.len() < 5 {
                        constraints.push(truncate_text(t, 300, 4));
                    }
                }
            }
        }
    }

    // Progress Done / In Progress: assistant short decisions
    let mut done: Vec<String> = Vec::new();
    let mut verifies: Vec<String> = Vec::new();
    for msg in old.iter().chain(turn_prefix.iter()) {
        if msg.name.as_deref() == Some("verify") {
            if let Some(c) = &msg.content {
                verifies.push(truncate_text(c, 600, 8));
            }
        }
        if msg.role == Role::Assistant && msg.tool_calls.is_none() {
            if let Some(c) = &msg.content {
                if c.len() < 300 && done.len() < 5 {
                    done.push(truncate_text(c, 400, 4));
                }
            }
        }
    }

    let (read_files, modified_files) = compute_file_lists(file_ops);
    let file_section = format_file_operations(&read_files, &modified_files);

    let mut out = String::new();
    out.push_str("## Goal\n");
    out.push_str(&goal);
    out.push_str("\n\n## Constraints & Preferences\n");
    if constraints.is_empty() {
        out.push_str("- (none)\n");
    } else {
        for c in constraints {
            out.push_str(&format!("- {}\n", c));
        }
    }
    out.push_str("\n## Progress\n### Done\n");
    if done.is_empty() {
        out.push_str("- (none)\n");
    } else {
        for d in &done {
            out.push_str(&format!("- [x] {}\n", d));
        }
    }
    out.push_str("\n### In Progress\n");
    out.push_str("- [ ] (none)\n");
    out.push_str("\n### Blocked\n");
    if verifies.is_empty() {
        out.push_str("- (none)\n");
    } else {
        let tail = verifies
            .iter()
            .rev()
            .take(2)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!("{}\n", tail));
    }
    out.push_str("\n## Key Decisions\n");
    if done.is_empty() {
        out.push_str("- (none)\n");
    } else {
        for d in done.iter().take(3) {
            out.push_str(&format!("- **{}**\n", d));
        }
    }
    out.push_str("\n## Next Steps\n");
    out.push_str("1. Continue from retained context\n");
    out.push_str("\n## Critical Context\n");
    out.push_str("- (none)\n");
    out.push_str(&file_section);
    out.trim().to_string()
}

/// File-operations section for the summary ("**Files read:** …"), empty
/// when nothing was touched. Shared tail of both LLM-summary merge paths.
pub fn attach_file_section(file_ops: &FileOps) -> String {
    let (read_files, modified_files) = compute_file_lists(file_ops);
    format_file_operations(&read_files, &modified_files)
}
