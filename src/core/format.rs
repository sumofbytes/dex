#![allow(dead_code, unused_variables, unused_imports)]
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Per-line character budget: protects against minified/binary-ish content
/// whose single lines would dominate the context window.
const MAX_LINE_CHARS: usize = 2_000;

/// Head+tail output clamp shared by every tool: keeps the first and last
/// lines (errors and summaries live at the end, imports and context at the
/// start) and replaces the middle with a marker carrying the real counts, so
/// the model always knows how much it did not see. Long lines are clipped to
/// [`MAX_LINE_CHARS`].
pub(crate) fn clamp_lines(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let clipped: Vec<String> = text
        .lines()
        .map(|line| {
            let limit = line
                .char_indices()
                .nth(MAX_LINE_CHARS)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            if limit < line.len() {
                format!("{}…", &line[..limit])
            } else {
                line.to_string()
            }
        })
        .collect();
    let total = clipped.len();
    let width = |lines: &[String]| -> usize { lines.iter().map(|l| l.len() + 1).sum::<usize>() };

    let fits = width(&clipped) <= max_bytes.saturating_add(total);
    if total <= max_lines && fits {
        return clipped.join("\n");
    }

    let head_budget = max_lines / 2;
    let tail_budget = max_lines - head_budget;
    let mut head: Vec<String> = Vec::new();
    let mut used = 0usize;
    for line in clipped.iter().take(head_budget) {
        if used + line.len() + 1 > max_bytes * 2 / 3 && !head.is_empty() {
            break;
        }
        used += line.len() + 1;
        head.push(line.clone());
    }
    let mut tail: Vec<String> = Vec::new();
    let mut tail_used = 0usize;
    for line in clipped.iter().rev().take(tail_budget) {
        if tail_used + line.len() + 1 > max_bytes / 3 && !tail.is_empty() {
            break;
        }
        tail_used += line.len() + 1;
        tail.push(line.clone());
    }
    tail.reverse();

    let shown = head.len() + tail.len();
    let omitted = total - shown;
    let mut out = head;
    out.push(format!("[... {omitted} of {total} lines truncated ...]"));
    out.extend(tail);
    out.join("\n")
}

pub(crate) fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    clamp_lines(text, max_lines, max_bytes)
}

pub(crate) fn terminal_preview(text: &str) -> String {
    truncate_text(text, 10 * 1024, 100)
}

/// Rows of informational snippet under a tool outcome line in the REPL
/// transcript. Every tool shares this cap (write/edit's review-oriented diff
/// is the documented exception); overflow folds into a `… +N more lines`
/// tail, so no tool can flood the transcript.
pub(crate) const TRANSCRIPT_PREVIEW_LINES: usize = 6;

/// Compact single-line summary of a tool's arguments for the REPL transcript.
/// Pulls the primary arg (path/command/pattern) instead of dumping raw JSON.
///
/// The transcript word-wraps this row, so the preview is capped generously
/// (a few rows at typical widths) instead of chopped at one line — a silent
/// mid-word cut both loses the command's tail and leaves an awkwardly
/// overflowing row on narrow terminals. `…` marks any remaining cut.
pub(crate) fn short_arg(name: &str, input: &str) -> String {
    let obj = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(|x| x.as_str());
    let primary: Option<String> = match name {
        "chain" => obj
            .as_ref()
            .and_then(|o| o.get("steps"))
            .and_then(Value::as_array)
            .map(|steps| {
                let tools: Vec<&str> = steps
                    .iter()
                    .filter_map(|step| step.get("tool").and_then(Value::as_str))
                    .collect();
                format!("{} steps: {}", steps.len(), tools.join(" → "))
            }),
        "read" => read_short_arg(obj.as_ref()),
        "write" | "edit" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "glob" => get("path")
            .or_else(|| get("file"))
            .or_else(|| get("pattern"))
            .or_else(|| get("glob"))
            .map(str::to_string),
        // The input line already shows the tool name, so only the mode.
        "git" => Some(get("mode").unwrap_or("status").to_string()),
        "bash" => get("command").map(str::to_string),
        _ => None,
    };
    let s = primary
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| input.to_string());
    let s = s.lines().next().unwrap_or(&s).trim();
    let s = strip_ansi(s);
    truncate_cols(&s, ARG_PREVIEW_COLS)
}

/// Display-column budget for the transcript's tool-input preview row. Long
/// bash commands wrap across rows in the transcript (the TUI word-wraps);
/// this cap only bounds the damage for pathological inputs.
const ARG_PREVIEW_COLS: usize = 320;

/// Truncate to `max_cols` display columns, appending `…` when clipped.
fn truncate_cols(s: &str, max_cols: usize) -> String {
    if UnicodeWidthStr::width(s) <= max_cols {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max_cols.saturating_sub(1) {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Compact read target for the transcript input line, in editor goto style
/// like opencode/pi: bare `path` for a whole-file read, `path:from-to` when
/// paginating, `path:from+` for an open-ended offset, `N files` for fan-out
/// (so `paths:[...]` never dumps raw JSON), or the glob. Raw JSON is never
/// shown.
fn read_short_arg(obj: Option<&serde_json::Map<String, Value>>) -> Option<String> {
    let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(Value::as_str);
    if let Some(paths) = obj.and_then(|o| o.get("paths")).and_then(Value::as_array) {
        let n = paths.len();
        return Some(format!("{} file{}", n, if n == 1 { "" } else { "s" }));
    }
    if let Some(glob) = get("glob") {
        return Some(glob.to_string());
    }
    let path = get("path").or_else(|| get("file"))?;
    let offset = obj
        .and_then(|o| o.get("offset"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    match obj.and_then(|o| o.get("limit")).and_then(Value::as_u64) {
        Some(n) => {
            let end = offset.saturating_add(n.max(1)).saturating_sub(1);
            Some(format!("{path}:{offset}-{end}"))
        }
        None if offset > 1 => Some(format!("{path}:{offset}+")),
        None => Some(path.to_string()),
    }
}

/// First non-empty, trimmed line of a tool result, truncated — a one-line
/// confirmation for the REPL transcript instead of the full output.
pub(crate) fn one_line_summary(text: &str) -> String {
    let stripped = strip_ansi(text);
    let line = stripped
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    let line = line.trim();
    let limit = line
        .char_indices()
        .nth(120)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    line[..limit].to_string()
}

/// Drop ANSI escape sequences (colors, cursor movement) and carriage
/// returns/bells so tool output renders as plain text in the transcript.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' || c == '\x07' {
            continue;
        }
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Consume the CSI sequence up to its final byte (@..~).
            for next in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
        }
        // A lone ESC (or non-CSI sequence) is dropped.
    }
    out
}

/// A few informational lines from a tool result, rendered dim under the
/// one-line summary: enough to see *what* happened without flooding the
/// transcript. Lines are trimmed on the right only, so indentation (tree
/// output, indented code, nested listings) survives; blank lines and ANSI
/// escapes are removed, long lines clipped at 120 chars, and overflow folds
/// into a `… +N more lines` tail. `skip_first` lets callers omit the line
/// the one-line summary already shows.
pub(crate) fn tool_result_preview(text: &str, max_lines: usize, skip_first: bool) -> Vec<String> {
    let mut lines = text
        .lines()
        .map(strip_ansi)
        .map(|line| line.trim_end().to_string())
        .filter(|line| !line.trim().is_empty());
    if skip_first {
        lines.next();
    }
    let mut preview: Vec<String> = lines
        .by_ref()
        .map(|line| {
            let limit = line
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            let mut clipped = line[..limit].to_string();
            if limit < line.len() {
                clipped.push('…');
            }
            clipped
        })
        .take(max_lines)
        .collect();
    let remaining = lines.count();
    if remaining > 0 {
        preview.push(format!(
            "… +{remaining} more line{}",
            if remaining == 1 { "" } else { "s" }
        ));
    }
    preview
}

/// Read snippet à la opencode/pi: the `{:>4}  content` numbered gutter is
/// kept verbatim (trimmed on the right only, so gutter alignment and code
/// indent survive — unlike the generic preview, which trims both sides),
/// `==> file <==` fan-out headers stay as landmarks, and `[... N more
/// lines …]` pagination trailers fold into a clean `… +N more lines` tail
/// instead of leaking into the snippet as content. Tail-capped like other
/// previews.
pub(crate) fn read_preview_lines(text: &str, max: usize) -> Vec<String> {
    let mut preview: Vec<String> = Vec::new();
    let mut elided: usize = 0;
    let mut trailer_more: u64 = 0;
    for line in text.lines() {
        let stripped = strip_ansi(line);
        let trimmed = stripped.trim();
        if let Some(n) = trailer_more_lines(trimmed) {
            trailer_more += n;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if preview.len() < max {
            let clipped = stripped.trim_end();
            let limit = clipped
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(clipped.len());
            if limit < clipped.len() {
                preview.push(format!("{}…", &clipped[..limit]));
            } else {
                preview.push(clipped.to_string());
            }
        } else {
            elided += 1;
        }
    }
    let more = elided + trailer_more as usize;
    if more > 0 {
        preview.push(format!(
            "… +{more} more line{}",
            if more == 1 { "" } else { "s" }
        ));
    }
    preview
}

/// How many unseen lines a `[... …]` trailer line reports, if any: the read
/// pagination note (`[... {n} more lines; continue with offset … ...]`), the
/// byte-budget note (no count), and the head+tail clamp marker
/// (`[... {n} of {m} lines truncated ...]`) all share the `[... {n} …` shape.
fn trailer_more_lines(trimmed: &str) -> Option<u64> {
    if !trimmed.starts_with("[...") {
        return None;
    }
    trimmed
        .strip_prefix("[...")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|first| first.parse::<u64>().ok())
}

/// GitHub-style diff snippet for write/edit results: unified hunks with
/// surrounding context (the `---`/`+++` file headers are dropped — the tool
/// input line already names the path), long lines clipped like other
/// previews, tail-capped so a pathological diff cannot flood the transcript.
/// ponytail: flat row cap; collapse unchanged hunks instead if it bites.
pub(crate) fn diff_preview_lines(diff: &str, max: usize) -> Vec<String> {
    let mut lines: Vec<String> = diff.lines().map(|l| l.trim_end().to_string()).collect();
    // Strip exactly the leading file headers (`--- a/…` / `+++ b/…`); deeper
    // lines that happen to start with `---` are hunk content, not headers.
    if lines.first().is_some_and(|l| l.starts_with("--- ")) {
        lines.remove(0);
    }
    if lines.first().is_some_and(|l| l.starts_with("+++ ")) {
        lines.remove(0);
    }
    let mut clipped: Vec<String> = lines
        .into_iter()
        .map(|line| {
            let limit = line
                .char_indices()
                .nth(120)
                .map(|(i, _)| i)
                .unwrap_or(line.len());
            if limit < line.len() {
                format!("{}…", &line[..limit])
            } else {
                line
            }
        })
        .collect();
    if clipped.len() > max {
        let rest = clipped.len() - max;
        clipped.truncate(max);
        clipped.push(format!("… +{rest} more diff lines"));
    }
    clipped
}

/// Canonical tool-output preview dispatch shared by the TUI transcript and
/// the headless console path: write/edit show the unified diff on success,
/// read shows its numbered snippet, everything else (and every failure)
/// shares the generic preview so the error stays visible.
pub(crate) fn tool_preview(
    name: &str,
    ok: bool,
    diff: Option<&str>,
    result: &str,
    skip_first: bool,
) -> Vec<String> {
    if let Some(diff) = diff {
        if matches!(name, "write" | "edit") && ok {
            return diff_preview_lines(diff, 30);
        }
    }
    if name == "read" && ok {
        return read_preview_lines(result, TRANSCRIPT_PREVIEW_LINES);
    }
    tool_result_preview(result, TRANSCRIPT_PREVIEW_LINES, skip_first)
}

/// Headless one-shot body for the same dispatch: GitHub-style diff snippet
/// for write/edit, terminal-clamped output otherwise.
pub(crate) fn tool_preview_body(name: &str, ok: bool, diff: Option<&str>, result: &str) -> String {
    if let Some(diff) = diff {
        if matches!(name, "write" | "edit") && ok {
            return terminal_preview(diff);
        }
    }
    terminal_preview(result)
}

/// Outcome-first, human-sized result for the TUI transcript: the ✓/✗ glyph
/// and its color already carry success/failure, so the summary leads with
/// Read outcome in opencode/pi style: `lines A-B · M lines (+N more)` for a
/// paginated or truncated read, `F files · M lines` for fan-out, and the plain
/// `M lines` count otherwise. The range comes from the numbered gutter
/// actually shown (truthful under clamping); `[... N more lines …]`
/// pagination trailers feed the `(+N more)` tail instead of inflating the
/// count, and `==> file <==` fan-out headers are counted as files, not lines.
fn read_summary(obj: Option<&serde_json::Map<String, Value>>, text: &str) -> String {
    let offset = obj
        .and_then(|o| o.get("offset"))
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let mut files = 0usize;
    let mut shown = 0usize;
    let mut first: Option<u64> = None;
    let mut last = 0u64;
    let mut more = 0u64;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("==>") {
            files += 1;
            continue;
        }
        if let Some(n) = trailer_more_lines(trimmed) {
            more += n;
            continue;
        }
        if trimmed.starts_with("[...") {
            continue;
        }
        shown += 1;
        if let Some(n) = read_gutter_number(line) {
            if first.is_none() {
                first = Some(n);
            }
            last = n;
        }
    }
    let mut out = if files > 0 {
        format!(
            "{} file{} · {} line{}",
            files,
            if files == 1 { "" } else { "s" },
            shown,
            plural(shown)
        )
    } else if offset > 1 || more > 0 {
        match first {
            Some(a) if a != last => {
                format!("lines {a}-{last} · {} line{}", shown, plural(shown))
            }
            Some(a) => format!("line {a} · {} line{}", shown, plural(shown)),
            None => format!("{} line{}", shown, plural(shown)),
        }
    } else {
        format!("{} line{}", shown, plural(shown))
    };
    if more > 0 {
        out.push_str(&format!(" (+{more} more)"));
    }
    out
}

/// Line number from read's `{:>4}  content` gutter; `None` for `==>`
/// headers, `[...` trailers, and unnumbered text. The two-space gap is
/// required so a code line that merely starts with digits is never mistaken
/// for a gutter.
fn read_gutter_number(line: &str) -> Option<u64> {
    let rest = line.trim_start();
    let digits_len = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    if !rest[digits_len..].starts_with("  ") {
        return None;
    }
    rest[..digits_len].parse().ok()
}

/// what actually happened (counts, diffstats, first output line). Failure
/// keeps the `failed ·` prefix: greppable, and it explains *why*. The tool
/// input JSON is consulted for write/edit so the summary can show a diffstat
/// without the caller doing extra IO.
pub(crate) fn tool_result_summary(name: &str, input: &str, text: &str, ok: bool) -> String {
    if !ok {
        return format!("failed · {}", one_line_summary(text));
    }
    let obj = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(|x| x.as_str());
    let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    match name {
        "read" => read_summary(obj.as_ref(), text),
        "grep" | "ffgrep" => {
            // Files mode (the default) lists paths, not matches.
            let files_mode = obj
                .as_ref()
                .and_then(|o| o.get("output_mode"))
                .and_then(Value::as_str)
                .unwrap_or("files")
                == "files";
            if files_mode {
                format!(
                    "{} file{} matched",
                    lines,
                    if lines == 1 { "" } else { "s" }
                )
            } else if lines == 1 {
                "1 match".to_string()
            } else {
                format!("{lines} matches")
            }
        }
        "find" | "fffind" => format!("{} entr{}", lines, if lines == 1 { "y" } else { "ies" }),
        "ls" => format!("{} entr{}", lines, if lines == 1 { "y" } else { "ies" }),
        "bash" => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
        "write" => {
            let written = get("content").unwrap_or_default().lines().count();
            format!("{} line{} written", written, plural(written))
        }
        "edit" => {
            let removed = get("oldText").unwrap_or_default().lines().count();
            let added = get("newText").unwrap_or_default().lines().count();
            format!("+{added} −{removed}")
        }
        "chain" => {
            let steps = obj
                .as_ref()
                .and_then(|o| o.get("steps"))
                .and_then(Value::as_array)
                .map(|steps| steps.len())
                .unwrap_or(0);
            // Each fan-out read emits a `==> path <==` section header.
            let files = text.matches("==> ").count();
            format!(
                "{steps} steps · {files} file{} · {lines} line{}",
                if files == 1 { "" } else { "s" },
                plural(lines)
            )
        }
        _ => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Compact wall-clock label for a tool result: millis below 1s,
/// sub-second precision below 10s, whole seconds below a minute, then minutes.
pub(crate) fn format_duration(secs: f64) -> String {
    if secs < 0.0 {
        return String::new();
    }
    if secs < 1.0 {
        format!("{}ms", (secs * 1000.0).round() as u64)
    } else if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{:.0}s", secs.round())
    } else {
        let total = secs.round() as u64;
        format!("{}m {:02}s", total / 60, total % 60)
    }
}

pub(crate) fn model_tool_result(text: &str) -> String {
    truncate_text(text, 50 * 1024, 2_000)
}

/// Human-readable approval helpers — keep tool JSON out of the user's face.
/// `approval_title` / `approval_summary` / `approval_details` turn raw
/// `{"path":…}` / `{"command":…}` payloads into the short, scannable
/// lines the overlay and CLI prompt show. No filesystem IO, pure formatting.
/// ponytail: one place for all tool-to-human mapping; add a tool → add a branch.
pub(crate) fn approval_title(name: &str) -> &'static str {
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

pub(crate) fn approval_risk(name: &str) -> (&'static str, ratatui::style::Color) {
    use ratatui::style::Color;
    match name {
        "bash" => ("high", Color::LightRed),
        "write" | "edit" => ("medium", Color::Yellow),
        _ => ("low", Color::LightGreen),
    }
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
            format!(
                "{} · {} line{}",
                path,
                lines,
                if lines == 1 { "" } else { "s" }
            )
        }
        "edit" => {
            let path = get("path").unwrap_or("(unknown path)");
            let old = get("oldText").unwrap_or("").lines().count();
            let new = get("newText").unwrap_or("").lines().count();
            format!("{} · -{} +{}", path, old, new)
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

pub(crate) fn approval_details(name: &str, input: &str) -> Vec<String> {
    let v = serde_json::from_str::<Value>(input).ok();
    let obj = v.as_ref().and_then(|v| v.as_object());
    let get = |k: &str| obj.and_then(|o| o.get(k)).and_then(|x| x.as_str());
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
                        let limit = line
                            .char_indices()
                            .nth(88)
                            .map(|(idx, _)| idx)
                            .unwrap_or(line.len());
                        let clipped = if limit < line.len() {
                            format!("{}…", &line[..limit])
                        } else {
                            line.to_string()
                        };
                        out.push(format!("  {}", clipped));
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
            if let Some(content) = get("content") {
                let lines = content.lines().count();
                let bytes = content.len();
                out.push(format!("{} lines · {} bytes", lines, bytes));
                if lines > 0 {
                    out.push("content:".to_string());
                    for (i, line) in content.lines().take(4).enumerate() {
                        let limit = line
                            .char_indices()
                            .nth(72)
                            .map(|(idx, _)| idx)
                            .unwrap_or(line.len());
                        let clipped = if limit < line.len() {
                            format!("{}…", &line[..limit])
                        } else {
                            line.to_string()
                        };
                        out.push(format!("  {:>3} │ {}", i + 1, clipped));
                    }
                    if lines > 4 {
                        out.push(format!("  … +{} more lines", lines - 4));
                    }
                }
            }
            if out.is_empty() {
                vec![input.to_string()]
            } else {
                out
            }
        }
        "edit" => {
            let mut out = Vec::new();
            if let Some(path) = get("path") {
                out.push(format!("path: {}", path));
            }
            if let (Some(old), Some(new)) = (get("oldText"), get("newText")) {
                out.push(format!(
                    "replace {} lines → {} lines",
                    old.lines().count(),
                    new.lines().count()
                ));
                let old_preview: Vec<&str> = old.lines().take(3).collect();
                let new_preview: Vec<&str> = new.lines().take(3).collect();
                if !old_preview.is_empty() {
                    out.push("  − old:".to_string());
                    for l in old_preview {
                        let limit = l
                            .char_indices()
                            .nth(68)
                            .map(|(idx, _)| idx)
                            .unwrap_or(l.len());
                        out.push(format!(
                            "    {}",
                            if limit < l.len() {
                                format!("{}…", &l[..limit])
                            } else {
                                l.to_string()
                            }
                        ));
                    }
                }
                if !new_preview.is_empty() {
                    out.push("  + new:".to_string());
                    for l in new_preview {
                        let limit = l
                            .char_indices()
                            .nth(68)
                            .map(|(idx, _)| idx)
                            .unwrap_or(l.len());
                        out.push(format!(
                            "    {}",
                            if limit < l.len() {
                                format!("{}…", &l[..limit])
                            } else {
                                l.to_string()
                            }
                        ));
                    }
                }
            }
            if out.is_empty() {
                vec![input.to_string()]
            } else {
                out
            }
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
            if out.is_empty() {
                vec![input.to_string()]
            } else {
                out
            }
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

/// Git branch + dirty flag for a working directory, for status displays.
pub(crate) fn git_context(cwd: &str) -> (Option<String>, bool) {
    use std::process::Command;
    let branch = Command::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && Command::new("git")
            .args(["-C", cwd, "status", "--porcelain"])
            .stdin(std::process::Stdio::null())
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}

pub(crate) async fn git_context_async(cwd: &str) -> (Option<String>, bool) {
    use tokio::process::Command as AsyncCommand;
    let branch = AsyncCommand::new("git")
        .args(["-C", cwd, "branch", "--show-current"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = if branch.is_some() {
        AsyncCommand::new("git")
            .args(["-C", cwd, "status", "--porcelain"])
            .stdin(std::process::Stdio::null())
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .output()
            .await
            .ok()
            .is_some_and(|output| !output.stdout.is_empty())
    } else {
        false
    };
    (branch, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_outcome_first_without_ok_prefix() {
        // A successful read whose content happens to contain the shell
        // failure marker must not be reported as failed.
        assert_eq!(
            tool_result_summary(
                "read",
                "{}",
                "use serde_json::{Map, Value};\nresult.push_str(\"[exit 1]\");\n",
                true
            ),
            "2 lines"
        );
        // A genuinely failed tool reports failure with its first output line.
        assert_eq!(
            tool_result_summary("bash", "{}", "ls: no such file\n[exit 2]", false),
            "failed · ls: no such file"
        );
        // grep defaults to files mode: the summary counts files, not matches.
        assert_eq!(
            tool_result_summary("grep", "{}", "", true),
            "0 files matched"
        );
        assert_eq!(
            tool_result_summary(
                "grep",
                r#"{"output_mode":"content"}"#,
                "src/a.rs:1:hit",
                true
            ),
            "1 match"
        );
        // Empty successful output says so instead of a bare "ok".
        assert_eq!(tool_result_summary("bash", "{}", "", true), "(no output)");
        assert_eq!(
            tool_result_summary("bash", "{}", "hello world\nrest", true),
            "hello world"
        );
    }

    #[test]
    fn mutation_summaries_show_diffstats_from_input() {
        let write_input = r#"{"path":"src/x.rs","content":"a\nb\nc\n"}"#;
        assert_eq!(
            tool_result_summary("write", write_input, "wrote src/x.rs", true),
            "3 lines written"
        );
        let single = r#"{"path":"src/x.rs","content":"only"}"#;
        assert_eq!(
            tool_result_summary("write", single, "wrote src/x.rs", true),
            "1 line written"
        );
        let edit_input = r#"{"path":"src/x.rs","oldText":"a\nb","newText":"x\ny\nz"}"#;
        assert_eq!(
            tool_result_summary("edit", edit_input, "edited src/x.rs", true),
            "+3 −2"
        );
    }

    #[test]
    fn duration_formats_for_each_scale() {
        assert_eq!(format_duration(0.042), "42ms");
        assert_eq!(format_duration(0.42), "420ms");
        assert_eq!(format_duration(2.14), "2.1s");
        assert_eq!(format_duration(9.96), "10.0s");
        assert_eq!(format_duration(41.96), "42s");
        assert_eq!(format_duration(65.0), "1m 05s");
        assert_eq!(format_duration(125.4), "2m 05s");
        assert_eq!(format_duration(-1.0), "");
    }

    #[test]
    fn clamp_keeps_head_and_tail_with_real_counts() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let clamped = clamp_lines(text.trim_end(), 10, 1 << 20);
        assert!(clamped.starts_with("line 1\n"), "{clamped}");
        assert!(clamped.ends_with("line 100"), "{clamped}");
        assert!(
            clamped.contains("[... 90 of 100 lines truncated ...]"),
            "{clamped}"
        );

        // Within the limit the text passes through untouched.
        assert_eq!(clamp_lines("a\nb\nc", 10, 1024), "a\nb\nc");

        // Pathologically long lines are clipped, not kept whole.
        let long_line = "x".repeat(5000);
        let clamped = clamp_lines(&long_line, 10, 1 << 20);
        assert!(clamped.ends_with('…'));
        assert!(clamped.len() < 2100, "{}", clamped.len());

        // The byte budget bounds the result even below the line limit.
        let text: String = (1..=50)
            .map(|n| format!("{n} {}\n", "y".repeat(500)))
            .collect();
        let clamped = clamp_lines(text.trim_end(), 100, 4096);
        assert!(clamped.contains("lines truncated"), "{clamped}");
        assert!(clamped.len() < 8192, "{}", clamped.len());
    }

    #[test]
    fn preview_shows_meaningful_lines_with_more_tail() {
        let text = "\nfirst\n\nsecond\nthird\nfourth\n";
        assert_eq!(
            tool_result_preview(text, 3, false),
            vec!["first", "second", "third", "… +1 more line"]
        );
    }

    #[test]
    fn preview_can_skip_the_line_already_in_the_summary() {
        let text = "Error: boom\nat src/main.rs:1\nat src/main.rs:2\n";
        assert_eq!(
            tool_result_preview(text, 3, true),
            vec!["at src/main.rs:1", "at src/main.rs:2"]
        );
    }

    #[test]
    fn preview_strips_ansi_and_clips_long_lines() {
        let text = format!("\x1b[1;32m{}\x1b[0m", "x".repeat(200));
        let preview = tool_result_preview(&text, 1, false);
        assert_eq!(preview.len(), 1);
        assert!(preview[0].starts_with("xxx"));
        assert!(preview[0].ends_with('…'));
        assert!(preview[0].chars().count() <= 121);
        assert!(!preview[0].contains('\x1b'));
    }

    #[test]
    fn preview_of_empty_output_is_empty() {
        assert!(tool_result_preview("", 3, false).is_empty());
        assert!(tool_result_preview("\n \n", 3, true).is_empty());
    }

    #[test]
    fn summary_and_arg_strip_escapes_and_carriage_returns() {
        assert_eq!(one_line_summary("\x1b[31mboom\x1b[0m\r\nnext"), "boom");
        assert_eq!(one_line_summary("progress\r\r\x07done"), "progressdone");
        // Fallback path: unparseable input JSON is sanitized too.
        assert_eq!(short_arg("x", "'\x1b[1mevil\x1b[0m\r"), "'evil");
    }

    #[test]
    fn short_arg_keeps_full_command_and_marks_any_cut() {
        // A long bash command must not be silently chopped at one line: the
        // transcript wraps the row, so only pathological length is capped —
        // with a visible `…`, never a silent cut.
        let cmd = format!("echo {}", "a".repeat(500));
        let arg = short_arg("bash", &format!(r#"{{"command":"{cmd}"}}"#));
        assert_eq!(arg, format!("echo {}…", "a".repeat(314)));
        // Ordinary-length commands survive whole.
        assert_eq!(
            short_arg("bash", r#"{"command":"cargo test"}"#),
            "cargo test"
        );
    }

    #[test]
    fn diff_preview_is_github_style_hunks_without_file_headers() {
        let diff = "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,5 +1,6 @@\n ctx\n-old\n+new\n ctx2\n";
        let preview = diff_preview_lines(diff, 30);
        // File headers are dropped (the tool input line already names the
        // path); the hunk header and +/-/context lines remain.
        assert!(!preview.iter().any(|l| l.starts_with("--- ")));
        assert!(!preview.iter().any(|l| l.starts_with("+++ ")));
        assert!(preview.iter().any(|l| l.starts_with("@@")));
        assert!(preview.iter().any(|l| l == "-old"));
        assert!(preview.iter().any(|l| l == "+new"));
        assert!(preview.iter().any(|l| l == " ctx"));
    }

    #[test]
    fn diff_preview_clips_long_lines_and_caps_the_tail() {
        let long = format!("+{}", "x".repeat(200));
        let diff = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n{long}\n+two\n+three\n");
        let preview = diff_preview_lines(&diff, 2);
        assert_eq!(preview.len(), 3);
        assert!(preview[0].starts_with("@@"));
        assert!(preview[1].ends_with('…'));
        assert!(preview[1].chars().count() <= 121);
        assert_eq!(preview[2], "… +2 more diff lines");
    }

    #[test]
    fn preview_preserves_indentation_for_all_tools() {
        // Tree output, indented code, nested listings: the snippet must not
        // flatten what the tool actually printed.
        let text = "src/\n  main.rs\n    mod deep\n";
        assert_eq!(
            tool_result_preview(text, 6, false),
            vec!["src/", "  main.rs", "    mod deep"]
        );
    }

    #[test]
    fn short_arg_covers_every_tool_without_raw_json() {
        assert_eq!(
            short_arg("find", r#"{"pattern": "TODO", "path": "."}"#),
            "."
        );
        assert_eq!(short_arg("ffgrep", r#"{"pattern": "TODO"}"#), "TODO");
        assert_eq!(short_arg("fffind", r#"{"pattern": "*.rs"}"#), "*.rs");
        assert_eq!(short_arg("git", r#"{"mode": "diff"}"#), "diff");
        assert_eq!(short_arg("git", "{}"), "status");
    }

    #[test]
    fn summaries_cover_the_ff_aliases() {
        assert_eq!(
            tool_result_summary("ffgrep", "{}", "a.rs\nb.rs\n", true),
            "2 files matched"
        );
        assert_eq!(
            tool_result_summary("fffind", "{}", "a.rs\n", true),
            "1 entry"
        );
    }

    #[tokio::test]
    async fn git_context_async_matches_sync() {
        // TDD Phase 6: tokio::process git spawns under cache, same branch/dirty.
        let cwd = std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let sync_res = super::git_context(&cwd);
        let async_res = super::git_context_async(&cwd).await;
        assert_eq!(sync_res, async_res);
    }
}
