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

/// Headless `[tool input]`/`[tool output]` body: same per-line standard as
/// the transcript rows — [`PREVIEW_LINE_COLS`] display columns with `…` on
/// every cut and ANSI escapes stripped — under the head/tail line fold.
/// ANSI is stripped *before* the byte budget so escapes are not counted and
/// a cut landing inside an escape sequence cannot swallow the `…` marker.
pub(crate) fn terminal_preview(text: &str) -> String {
    clamp_lines(&strip_ansi(text), 100, 10 * 1024)
        .lines()
        .map(|line| truncate_cols(line, PREVIEW_LINE_COLS))
        .collect::<Vec<_>>()
        .join("\n")
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
    let s = primary.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        // Unknown tool or unparseable input: collapse whitespace so a
        // pretty-printed JSON blob previews as one compact row instead of
        // just its first line (`{`).
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    });
    let s = s.lines().next().unwrap_or(&s).trim();
    let s = strip_ansi(s);
    truncate_cols(&s, PREVIEW_LINE_COLS)
}

/// Display-column budget shared by every transcript tool row: the input
/// `▸` preview (`short_arg`), the output summary (`one_line_summary`), and
/// every preview flavor (`tool_result_preview`, `read_preview_lines`,
/// `diff_preview_lines`). The TUI word-wraps rows, so one generous cap just
/// bounds the damage for pathological lines; `truncate_cols` measures real
/// display columns (wide chars count 2) and marks every cut with `…`.
const PREVIEW_LINE_COLS: usize = 320;

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

/// First non-empty, trimmed line of a tool result, truncated to the shared
/// [`PREVIEW_LINE_COLS`] display-column budget with a visible `…` on cut —
/// the same standard as the tool-input preview row.
pub(crate) fn one_line_summary(text: &str) -> String {
    let stripped = strip_ansi(text);
    let line = stripped
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("");
    truncate_cols(line.trim(), PREVIEW_LINE_COLS)
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
/// escapes are removed, long lines clipped to [`PREVIEW_LINE_COLS`] display
/// columns, and overflow folds into a `… +N more lines` tail. `skip_first`
/// lets callers omit the line the one-line summary already shows.
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
        .map(|line| truncate_cols(&line, PREVIEW_LINE_COLS))
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
            preview.push(truncate_cols(stripped.trim_end(), PREVIEW_LINE_COLS));
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
        .map(|line| truncate_cols(&line, PREVIEW_LINE_COLS))
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
/// for write/edit, per-line [`PREVIEW_LINE_COLS`]-clamped output otherwise
/// (via [`terminal_preview`], same standard as the transcript rows).
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

/// Ephemeral MCP status line for the compaction budget (`agent::loop`
/// counts it via the `ephemerals` preamble without storing it in the
/// transcript). `None` when no servers are configured so the budget is
/// unaffected. The schema already tells the model which tools exist; this
/// names what it is *not* seeing — down servers and schema-cap drops.
pub(crate) fn mcp_status_line(statuses: &[crate::mcp::ServerStatus]) -> Option<String> {
    if statuses.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = statuses
        .iter()
        .map(|s| {
            if s.state.as_str() == "up" {
                format!("{} ({} tools)", s.name, s.tools)
            } else {
                format!("{} (down)", s.name)
            }
        })
        .collect();
    let dropped = crate::mcp::cached_truncated();
    if dropped > 0 {
        parts.push(format!("{dropped} tools omitted (schema cap)"));
    }
    Some(format!("MCP servers: {}", parts.join(", ")))
}

/// Multi-line `/mcp` panel in Claude Code style: one header with totals,
/// then per server a `✓ name — N tools` line (with each tool + its one-line
/// description indented beneath) or a `✗ name — down: <reason>` line.
/// `tools` is the cached schema slice; only `mcp__<server>__*` entries
/// (plus the synthetic `_read_resource` reader) belong to a server.
/// `truncated` is the schema-cap drop count (`GET /api/mcp` carries it for
/// remote clients whose own process counter is always zero). Pure function
/// over snapshots so both TUIs share the render.
pub(crate) fn render_mcp_panel(
    statuses: &[crate::mcp::ServerStatus],
    tools: &[crate::core::types::ToolDefinition],
    truncated: usize,
) -> Vec<String> {
    if statuses.is_empty() {
        return vec!["no MCP servers configured.".to_string()];
    }
    let up = statuses.iter().filter(|s| s.state.as_str() == "up").count();
    let total_tools: usize = statuses.iter().map(|s| s.tools).sum();
    let mut lines = vec![format!(
        "MCP servers ({} connected, {} down · {} tools):",
        up,
        statuses.len() - up,
        total_tools
    )];
    for server in statuses {
        if server.state.as_str() == "up" {
            lines.push(format!(
                "✓ {} — {} tool{}",
                server.name,
                server.tools,
                if server.tools == 1 { "" } else { "s" }
            ));
            let mut names: Vec<(&str, &str)> = tools
                .iter()
                .filter_map(|t| {
                    short_mcp_tool(&server.name, &t.function.name)
                        .map(|short| (short, t.function.description.as_str()))
                })
                .collect();
            names.sort();
            for (short, desc) in names {
                let one_line = desc
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(format!("  · {short} — {}", truncate_chars(&one_line, 100)));
            }
        } else {
            let reason = server
                .error
                .as_deref()
                .map(|e| {
                    e.lines()
                        .next()
                        .unwrap_or("unknown error")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| "not connected".to_string());
            lines.push(format!(
                "✗ {} — down: {}",
                server.name,
                truncate_chars(&reason, 160)
            ));
        }
    }
    if truncated > 0 {
        lines.push(format!(
            "… and {truncated} more tool{} hidden by the schema cap (DEX_MCP_MAX_TOOLS).",
            if truncated == 1 { "" } else { "s" }
        ));
    }
    lines
}

/// Strip the `mcp__<server>__` prefix (or the single-underscore synthetic
/// `_read_resource` reader) down to the bare tool name. `None` when the
/// tool belongs to a different server.
fn short_mcp_tool<'a>(server: &'a str, full: &'a str) -> Option<&'a str> {
    let prefix = format!("mcp__{server}__");
    if let Some(short) = full.strip_prefix(&prefix) {
        return Some(short);
    }
    full.strip_prefix(&format!("mcp__{server}_"))
}

/// Char-boundary-safe truncation with an ellipsis marker.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    format!("{}…", &text[..end])
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

    /// Display columns of a string (wide chars count 2) — the unit of the
    /// shared [`PREVIEW_LINE_COLS`] budget.
    fn display_cols(s: &str) -> usize {
        UnicodeWidthStr::width(s)
    }

    /// The budget's canonical cut of `l`-filler: budget-1 columns of content
    /// plus the 1-column `…` marker — what any 400+-column line must clip to.
    fn clipped_at_budget() -> String {
        format!("{}…", "l".repeat(PREVIEW_LINE_COLS - 1))
    }

    #[test]
    fn tool_rows_share_one_display_column_budget() {
        // The standard: every transcript tool row — the input preview
        // (`short_arg`), the output summary (`one_line_summary`), and every
        // preview flavor — is bounded by the same budget, measured in
        // display columns (wide chars count 2), with every cut marked `…`.
        let long = "l".repeat(400); // 400 display columns
        let clipped = clipped_at_budget();
        assert_eq!(display_cols(&clipped), PREVIEW_LINE_COLS);
        assert!(clipped.ends_with('…'), "cut must be marked");

        // Input row and output rows obey the same budget, byte-for-byte.
        assert_eq!(
            short_arg("bash", &format!(r#"{{"command":"{long}"}}"#)),
            clipped,
            "input row"
        );
        assert_eq!(one_line_summary(&long), clipped, "summary row");
        assert_eq!(
            tool_result_preview(&long, TRANSCRIPT_PREVIEW_LINES, false)[0],
            clipped,
            "generic preview"
        );
        // Prefixes (diff marker, read gutter) count against the same budget.
        let budgeted = |prefix: &str| {
            format!(
                "{prefix}{}…",
                "l".repeat(PREVIEW_LINE_COLS - 1 - prefix.chars().count())
            )
        };
        assert_eq!(
            diff_preview_lines(&format!("-{long}"), 6)[0],
            budgeted("-"),
            "diff preview keeps its marker prefix"
        );
        assert_eq!(
            read_preview_lines(&format!("   1  {long}"), 6)[0],
            budgeted("   1  "),
            "read preview keeps its gutter"
        );
    }

    #[test]
    fn headless_rows_share_one_display_column_budget() {
        // The headless one-shot path (`[tool input]` / `[tool output]` on
        // stderr) must adhere to the same per-line standard as the
        // transcript rows: [`PREVIEW_LINE_COLS`] display columns, wide chars
        // count 2, every cut marked `…`, ANSI escapes stripped.
        let long = "l".repeat(400); // 400 display columns
        let clipped = clipped_at_budget();

        assert_eq!(terminal_preview(&long), clipped, "output body");
        // Every line obeys the budget independently.
        assert_eq!(
            terminal_preview(&format!("{long}\n{long}")),
            format!("{clipped}\n{clipped}"),
            "per line"
        );
        // ANSI bytes are stripped, not counted and not leaked to the terminal.
        let ansi = format!("\x1b[31m{long}\x1b[0m");
        assert_eq!(terminal_preview(&ansi), clipped);
        // Wide chars spend 2 columns each: 200 CJK chars (400 columns) clip.
        let wide = "界".repeat(200);
        let wc = terminal_preview(&wide);
        assert!(display_cols(&wc) <= PREVIEW_LINE_COLS && wc.ends_with('…'));

        // A [`MAX_LINE_CHARS`] cut landing mid-escape-sequence must not
        // swallow the `…` marker: escapes are stripped before the budget is
        // applied, so this 5000-column line still previews as the budgeted
        // cut instead of an empty row.
        let giant = format!("\x1b[{}m", "1".repeat(MAX_LINE_CHARS - 2));
        let raw = format!("{giant}{}", "l".repeat(5_000));
        assert_eq!(
            terminal_preview(&raw),
            clipped,
            "escape cut keeps the marker"
        );

        // The dispatch wrapper and the input row agree with the same budget.
        assert_eq!(tool_preview_body("bash", true, None, &long), clipped);
        assert_eq!(
            short_arg("bash", &format!(r#"{{"command":"{long}"}}"#)),
            clipped,
            "headless input row uses the same short_arg preview as the TUI"
        );
    }

    #[test]
    fn short_arg_fallback_collapses_whitespace() {
        // Unknown tool or unparseable input: the raw fallback is
        // whitespace-collapsed so a pretty-printed JSON blob previews as one
        // compact row instead of just its first line (`{`).
        let pretty = "{\n  \"weird\": \"value\"\n}";
        assert_eq!(
            short_arg("unknown_tool", pretty),
            "{ \"weird\": \"value\" }"
        );
        // Plain multi-line fallback is compacted the same way.
        assert_eq!(short_arg("unknown_tool", "a\n  b\n  c"), "a b c");
    }

    #[test]
    fn preview_budget_counts_display_columns_not_chars() {
        // Wide chars spend 2 columns each: 160 CJK chars are 320 columns and
        // must fit the 320-column budget untouched (the old char-count clip
        // at 120 "chars" cut them and left 240+ column rows).
        let fits = "界".repeat(160);
        assert_eq!(UnicodeWidthStr::width(fits.as_str()), PREVIEW_LINE_COLS);
        assert_eq!(
            one_line_summary(&fits),
            fits,
            "no cut at exactly the budget"
        );
        assert_eq!(
            tool_result_preview(&fits, 6, false),
            vec![fits],
            "preview agrees with the summary row"
        );
        // 200 CJK chars (400 columns) clip to the budget with a marker.
        let wide = "界".repeat(200);
        let clipped = one_line_summary(&wide);
        assert!(clipped.ends_with('…'));
        assert!(UnicodeWidthStr::width(clipped.as_str()) <= PREVIEW_LINE_COLS);
        assert_eq!(clipped.chars().count(), 160, "159 wide chars + ellipsis");
    }

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
        let text = format!("\x1b[1;32m{}\x1b[0m", "x".repeat(400));
        let preview = tool_result_preview(&text, 1, false);
        assert_eq!(preview.len(), 1);
        assert!(preview[0].starts_with("xxx"));
        assert!(preview[0].ends_with('…'));
        // The shared transcript budget, in display columns like every other
        // tool row (see tool_rows_share_one_display_column_budget).
        assert!(UnicodeWidthStr::width(preview[0].as_str()) <= PREVIEW_LINE_COLS);
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
        let long = format!("+{}", "x".repeat(400));
        let diff = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n{long}\n+two\n+three\n");
        let preview = diff_preview_lines(&diff, 2);
        assert_eq!(preview.len(), 3);
        assert!(preview[0].starts_with("@@"));
        assert!(preview[1].ends_with('…'));
        assert!(UnicodeWidthStr::width(preview[1].as_str()) <= PREVIEW_LINE_COLS);
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

    #[test]
    fn mcp_status_line_empty_when_no_servers() {
        assert_eq!(mcp_status_line(&[]), None);
    }

    #[test]
    fn mcp_status_line_names_up_and_down_servers() {
        use crate::mcp::ServerStatus;
        let line = mcp_status_line(&[
            ServerStatus {
                name: "gh".to_string(),
                state: "up".to_string(),
                tools: 3,
                error: None,
            },
            ServerStatus {
                name: "db".to_string(),
                state: "down".to_string(),
                tools: 0,
                error: Some("refused".to_string()),
            },
        ])
        .expect("non-empty statuses produce a line");
        assert!(line.contains("gh (3 tools)"), "{line}");
        assert!(line.contains("db (down)"), "{line}");
    }

    #[test]
    fn render_mcp_panel_lists_tools_and_down_servers() {
        use crate::core::types::{FunctionDef, ToolDefinition};
        use crate::mcp::ServerStatus;
        let tool = |name: &str, description: &str| ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDef {
                name: name.to_string(),
                description: description.to_string(),
                parameters: serde_json::json!({}),
            },
        };
        let lines = render_mcp_panel(
            &[
                ServerStatus {
                    name: "gh".to_string(),
                    state: "up".to_string(),
                    tools: 2,
                    error: None,
                },
                ServerStatus {
                    name: "db".to_string(),
                    state: "down".to_string(),
                    tools: 0,
                    error: Some("spawn sqlite-mcp: No such file\nmore".to_string()),
                },
            ],
            &[
                tool("mcp__gh__search", "Search issues\nand pull requests."),
                tool("mcp__gh_read_resource", "Read a resource by URI."),
                tool("mcp__other__x", "Belongs to another server."),
            ],
            0,
        );
        assert_eq!(lines[0], "MCP servers (1 connected, 1 down · 2 tools):");
        assert_eq!(lines[1], "✓ gh — 2 tools");
        // Prefix stripped, description collapsed to its first line.
        assert!(lines.contains(&"  · read_resource — Read a resource by URI.".to_string()));
        assert!(lines.contains(&"  · search — Search issues".to_string()));
        assert!(!lines.iter().any(|l| l.contains("other")));
        // Down server shows the first error line only.
        assert_eq!(
            lines.last().unwrap(),
            "✗ db — down: spawn sqlite-mcp: No such file"
        );
    }

    #[test]
    fn render_mcp_panel_empty_and_truncated() {
        let lines = render_mcp_panel(&[], &[], 0);
        assert_eq!(lines, vec!["no MCP servers configured."]);
        use crate::mcp::ServerStatus;
        let lines = render_mcp_panel(
            &[ServerStatus {
                name: "gh".to_string(),
                state: "up".to_string(),
                tools: 1,
                error: None,
            }],
            &[],
            3,
        );
        assert!(lines
            .last()
            .unwrap()
            .contains("3 more tools hidden by the schema cap"));
    }
}
