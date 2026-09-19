//! Approval-surface rendering: titles, risk coloring, one-line summaries,
//! and the per-tool detail lines shown in the permission dialog.

use serde_json::Value;

use super::{clip_chars, short_arg};

pub(crate) fn approval_title(name: &str, input: &str) -> &'static str {
    approval_title_with_then_run(name, input_has_then_run(input))
}

/// [`approval_title`] with a precomputed `then_run` flag: [`PendingApproval::new`]
/// parses the input once and shares the flag across title/risk instead of
/// parsing 2× (plus summary/details = 4× per enqueue).
pub(crate) fn approval_title_with_then_run(name: &str, has_then_run: bool) -> &'static str {
    // A `then_run` makes a file write a shell command too; say so in the
    // title rather than presenting it as a plain write.
    if matches!(name, "write" | "edit") && has_then_run {
        return "File change + shell verification";
    }
    match name {
        "bash" => "Run shell command",
        "write" => "Create / overwrite file",
        "edit" => "Edit file",
        "read" => "Read file",
        "grep" | "ffgrep" => "Search contents",
        "ls" => "List directory",
        "find" | "fffind" => "Find files",
        "git" => "Git",
        "chain" => "Chained read",
        _ => "Run tool",
    }
}

#[cfg(test)]
pub(crate) fn approval_risk(name: &str, input: &str) -> (&'static str, ratatui::style::Color) {
    approval_risk_with_then_run(name, input_has_then_run(input))
}

/// [`approval_risk`] with a precomputed flag (see [`approval_title_with_then_run`]).
pub(crate) fn approval_risk_with_then_run(
    name: &str,
    has_then_run: bool,
) -> (&'static str, ratatui::style::Color) {
    use ratatui::style::Color;
    // A `then_run` turns a file mutation into a shell command; the approver
    // must see the same "high" risk as a bare `bash`.
    if matches!(name, "write" | "edit") && has_then_run {
        return ("high", Color::LightRed);
    }
    match name {
        "bash" => ("high", Color::LightRed),
        "write" | "edit" => ("medium", Color::Yellow),
        _ => ("low", Color::LightGreen),
    }
}

/// Whether the raw approval input carries a non-empty `then_run` command —
/// the same field `then_run_suffix` renders in the summary.
pub(crate) fn input_has_then_run(input: &str) -> bool {
    serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().map(|obj| then_run_of(Some(obj)).is_some()))
        .unwrap_or(false)
}

pub(crate) fn approval_summary(name: &str, input: &str) -> String {
    let v = serde_json::from_str::<Value>(input).ok();
    let obj = v.as_ref().and_then(|v| v.as_object());
    let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(|x| x.as_str());
    match name {
        "bash" => get("command")
            .map(|c| c.lines().next().unwrap_or(c).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(no command)".to_string()),
        "write" => {
            let path = get("path").unwrap_or("(unknown path)");
            let content = get("content").unwrap_or("");
            let lines = content.lines().count();
            let plural = if lines == 1 { "" } else { "s" };
            format!("{path} · {lines} line{plural}{}", then_run_suffix(obj))
        }
        "edit" => {
            let path = get("path").unwrap_or("(unknown path)");
            let old = get("oldText").unwrap_or("").lines().count();
            let new = get("newText").unwrap_or("").lines().count();
            format!("{} · -{} +{}{}", path, old, new, then_run_suffix(obj))
        }
        "read" => {
            if let Some(paths) = obj.and_then(|o| o.get("paths")).and_then(|v| v.as_array()) {
                format!("{} files", paths.len())
            } else if let Some(glob) = get("glob") {
                format!("glob: {}", glob)
            } else {
                get("path")
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "(no path)".to_string())
            }
        }
        "grep" | "ffgrep" => get("pattern")
            .map(|p| format!("search: {}", p))
            .unwrap_or_else(|| "(no pattern)".to_string()),
        "find" | "fffind" => get("pattern")
            .map(|p| format!("find: {}", p))
            .unwrap_or_else(|| "(no pattern)".to_string()),
        "ls" => get("path")
            .map(|p| format!("ls: {}", p))
            .unwrap_or_else(|| "ls: .".to_string()),
        "git" => get("mode")
            .map(|m| format!("git {}", m))
            .unwrap_or_else(|| "git".to_string()),
        "chain" => obj
            .and_then(|o| o.get("steps"))
            .and_then(|v| v.as_array())
            .map(|steps| {
                let tools: Vec<&str> = steps
                    .iter()
                    .filter_map(|s| s.get("tool").and_then(|v| v.as_str()))
                    .collect();
                format!("{} steps: {}", steps.len(), tools.join(" → "))
            })
            .unwrap_or_else(|| "chain".to_string()),
        _ => short_arg(name, input),
    }
}

/// A `write`/`edit` carrying `then_run` also runs a shell
/// command. The approver must see that the file change is not all that will
/// happen, so every approval surface carries the command. Clipped to one line —
/// the prompt is a glance, not the transcript.
fn then_run_of(obj: Option<&serde_json::Map<String, Value>>) -> Option<String> {
    let command = obj?.get("then_run")?.as_str()?.replace('\n', " ");
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some(clip_chars(command, 120))
}

/// The `· then: …` tail `approval_summary` appends for a `write`/`edit`
/// carrying `then_run`.
fn then_run_suffix(obj: Option<&serde_json::Map<String, Value>>) -> String {
    match then_run_of(obj) {
        Some(command) => format!(" · then: {command}"),
        None => String::new(),
    }
}

pub(crate) fn approval_details(name: &str, input: &str) -> Vec<String> {
    let v = serde_json::from_str::<Value>(input).ok();
    let obj = v.as_ref().and_then(|v| v.as_object());
    let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(|x| x.as_str());
    // An arm that assembled no structured detail rows falls back to the raw
    // input line.
    let fallback = |out: Vec<String>| {
        if out.is_empty() {
            vec![input.to_string()]
        } else {
            out
        }
    };
    match name {
        "bash" => {
            if let Some(cmd) = get("command") {
                let cmd = cmd.trim();
                if cmd.len() <= 120 && !cmd.contains('\n') {
                    vec![format!("$ {}", cmd)]
                } else {
                    let mut out = vec!["$ ".to_string()];
                    for (i, line) in cmd.lines().enumerate() {
                        if i >= 6 {
                            out.push(format!("  … +{} more lines", cmd.lines().count() - i));
                            break;
                        }
                        let line = line.trim_end();
                        out.push(format!("  {}", clip_chars(line, 88)));
                    }
                    out
                }
            } else {
                vec![input.to_string()]
            }
        }
        "write" => {
            let mut out = Vec::new();
            if let Some(path) = get("path") {
                out.push(format!("path: {}", path));
            }
            if let Some(command) = then_run_of(obj) {
                out.push(format!("then: $ {command}"));
            }
            if let Some(content) = get("content") {
                let lines = content.lines().count();
                let bytes = content.len();
                out.push(format!("{} lines · {} bytes", lines, bytes));
                if lines > 0 {
                    out.push("content:".to_string());
                    for (i, line) in content.lines().take(4).enumerate() {
                        out.push(format!("  {:>3} │ {}", i + 1, clip_chars(line, 72)));
                    }
                    if lines > 4 {
                        out.push(format!("  … +{} more lines", lines - 4));
                    }
                }
            }
            fallback(out)
        }
        "edit" => {
            let mut out = Vec::new();
            if let Some(path) = get("path") {
                out.push(format!("path: {}", path));
            }
            if let Some(command) = then_run_of(obj) {
                out.push(format!("then: $ {command}"));
            }
            if let (Some(old), Some(new)) = (get("oldText"), get("newText")) {
                out.push(format!(
                    "replace {} lines → {} lines",
                    old.lines().count(),
                    new.lines().count()
                ));
                // The old/new previews are verbatim twins: same 3-line
                // take, same 68-char clip, same indent — only the label and
                // the source differ.
                let push_preview = |out: &mut Vec<String>, label: &str, text: &str| {
                    let preview: Vec<&str> = text.lines().take(3).collect();
                    if !preview.is_empty() {
                        out.push(label.to_string());
                        for l in preview {
                            out.push(format!("    {}", clip_chars(l, 68)));
                        }
                    }
                };
                push_preview(&mut out, "  − old:", old);
                push_preview(&mut out, "  + new:", new);
            }
            fallback(out)
        }
        "read" => {
            if let Some(paths) = obj.and_then(|o| o.get("paths")).and_then(|v| v.as_array()) {
                let mut out = vec![format!("{} files:", paths.len())];
                for p in paths.iter().take(6).filter_map(|v| v.as_str()) {
                    out.push(format!("  • {}", p));
                }
                if paths.len() > 6 {
                    out.push(format!("  … +{} more", paths.len() - 6));
                }
                out
            } else if let Some(glob) = get("glob") {
                vec![format!("glob: {}", glob)]
            } else if let Some(path) = get("path") {
                let mut out = vec![format!("path: {}", path)];
                if let Some(off) = obj.and_then(|o| o.get("offset")).and_then(|v| v.as_u64()) {
                    out.push(format!("offset: {}", off));
                }
                if let Some(lim) = obj.and_then(|o| o.get("limit")).and_then(|v| v.as_u64()) {
                    out.push(format!("limit: {}", lim));
                }
                out
            } else {
                vec![input.to_string()]
            }
        }
        "grep" | "ffgrep" => {
            let mut out = Vec::new();
            if let Some(pat) = get("pattern") {
                out.push(format!("pattern: {}", pat));
            }
            if let Some(mode) = get("output_mode") {
                out.push(format!("mode: {}", mode));
            }
            fallback(out)
        }
        "find" | "fffind" => {
            if let Some(pat) = get("pattern") {
                vec![format!("pattern: {}", pat)]
            } else {
                vec![input.to_string()]
            }
        }
        "ls" => {
            if let Some(path) = get("path") {
                vec![format!("path: {}", path)]
            } else {
                vec!["path: .".to_string()]
            }
        }
        "git" => {
            if let Some(mode) = get("mode") {
                vec![format!("git {}", mode)]
            } else {
                vec![input.to_string()]
            }
        }
        _ => vec![short_arg(name, input)],
    }
}
