#![allow(dead_code, unused_variables, unused_imports)]
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Per-line character budget: protects against minified/binary-ish content
/// whose single lines would dominate the context window.
const MAX_LINE_CHARS: usize = 2_000;

/// Clip a string to at most `max_chars` *characters* — char-boundary-safe,
/// appending a single `…` when anything was cut. The shared body of every
/// char-count clip in the display layer (not display columns — see
/// [`truncate_cols`]).
pub(crate) fn clip_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}…", &s[..end])
}

/// Head+tail output clamp shared by every tool: keeps the first and last
/// lines (errors and summaries live at the end, imports and context at the
/// start) and replaces the middle with a marker carrying the real counts, so
/// the model always knows how much it did not see. Long lines are clipped to
/// [`MAX_LINE_CHARS`].
pub(crate) fn clamp_lines(text: &str, max_lines: usize, max_bytes: usize) -> String {
    clamp_lines_checked(text, max_lines, max_bytes).0
}

/// `clamp_lines` plus an explicit "the clamp cut bytes" flag. Trailing
/// newline normalization is not a cut — the capture path must not archive a
/// result, and hang a marker on it, merely because its raw form ends in a
/// newline and the join drops that newline.
pub(crate) fn clamp_lines_checked(
    text: &str,
    max_lines: usize,
    max_bytes: usize,
) -> (String, bool) {
    let clipped: Vec<String> = text
        .lines()
        .map(|line| clip_chars(line, MAX_LINE_CHARS))
        .collect();
    // Byte length lies here: `clip_chars` output can be a few bytes longer
    // than the input (an exactly-max_chars line gains `…`), so compare the
    // strings themselves.
    let clipped_any = text
        .lines()
        .zip(clipped.iter())
        .any(|(line, out)| out.as_str() != line);
    let total = clipped.len();
    let width = |lines: &[String]| -> usize { lines.iter().map(|l| l.len() + 1).sum::<usize>() };

    let fits = width(&clipped) <= max_bytes.saturating_add(total);
    if total <= max_lines && fits {
        return (clipped.join("\n"), clipped_any);
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
    (out.join("\n"), true)
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

/// Compact read target for the transcript input line, in editor goto style:
/// bare `path` for a whole-file read, `path:from-to` when
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
            continue;
        }
        if chars.peek() == Some(&']') {
            chars.next();
            // Consume the OSC payload (hyperlinks, window titles) up to its
            // terminator: BEL, or the two-byte ST (ESC \). Dropping only the
            // ESC would leak the payload text into previews.
            loop {
                match chars.next() {
                    None | Some('\x07') => break,
                    Some('\x1b') => {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                    _ => {}
                }
            }
            continue;
        }
        // A lone ESC (or non-CSI/OSC sequence) is dropped.
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

/// Read snippet: the `{:>4}  content` numbered gutter is
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
/// Read outcome: `lines A-B · M lines (+N more)` for a
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
/// without the caller doing extra IO; `edit` prefers the real unified diff
/// (`diff`) when the caller has one, so `replaceAll` replication counts.
pub(crate) fn tool_result_summary(
    name: &str,
    input: &str,
    text: &str,
    ok: bool,
    diff: Option<&str>,
) -> String {
    if !ok {
        return format!("failed · {}", one_line_summary(text));
    }
    let obj = serde_json::from_str::<Value>(input)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let get = |k: &str| obj.as_ref().and_then(|o| o.get(k)).and_then(|x| x.as_str());
    match name {
        "read" => read_summary(obj.as_ref(), text),
        "grep" | "ffgrep" => {
            // Files mode (the default) lists paths, not matches; content
            // mode prints `path:123:text` hits alongside `path:123-context`
            // context rows, so only match-shaped rows count — and the
            // `[... more exist ...]` truncation trailer is never a hit.
            let files_mode = obj
                .as_ref()
                .and_then(|o| o.get("output_mode"))
                .and_then(Value::as_str)
                .unwrap_or("files")
                == "files";
            let hits = if files_mode {
                text.lines()
                    .filter(|line| {
                        let line = line.trim_start();
                        !line.is_empty()
                            && !line.starts_with("[...")
                            // Fuzzy fallback header (`0 exact matches…`):
                            // prose, not a path.
                            && !line.starts_with("0 exact matches for '")
                    })
                    .count()
            } else {
                text.lines()
                    .filter(|line| grep_match_line(line.trim_start()))
                    .count()
            };
            if files_mode {
                format!("{} file{} matched", hits, if hits == 1 { "" } else { "s" })
            } else if hits == 1 {
                "1 match".to_string()
            } else {
                format!("{hits} matches")
            }
        }
        "find" | "fffind" | "ls" => {
            // Entry rows plus trailers: the picker's `[... N of M paths
            // matched …]` and the ls clamp `[... N of M lines truncated …]`
            // are truncation notes, not entries — fold their count into a
            // `(+N more)` tail like read does.
            let mut entries = 0usize;
            let mut more = 0u64;
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(n) = trailer_more_lines(trimmed) {
                    more += n;
                    continue;
                }
                if trimmed.starts_with("[...") {
                    continue; // trailer without a count
                }
                if trimmed == "0 matches." || trimmed == "(empty)" {
                    continue; // picker/ls empty-result prose, not an entry
                }
                entries += 1;
            }
            let mut out = if entries == 0 {
                "empty".to_string()
            } else {
                format!("{} entr{}", entries, if entries == 1 { "y" } else { "ies" })
            };
            if more > 0 {
                out.push_str(&format!(" (+{more} more)"));
            }
            out
        }
        "bash" => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
        "write" => {
            let written = get("content").unwrap_or_default().lines().count();
            format!(
                "{} line{} written{}",
                written,
                plural(written),
                then_run_tail(text)
            )
        }
        "edit" => {
            // Diffstat from the actual unified diff when available — it
            // counts replicated hunks too, so a `replaceAll` edit reports
            // every occurrence, not just the input's one spelling.
            let (added, removed) = diff_stat(diff).unwrap_or_else(|| {
                (
                    get("newText").unwrap_or_default().lines().count(),
                    get("oldText").unwrap_or_default().lines().count(),
                )
            });
            format!("+{added} −{removed}{}", then_run_tail(text))
        }
        "chain" => {
            let steps = obj
                .as_ref()
                .and_then(|o| o.get("steps"))
                .and_then(Value::as_array)
                .map(|steps| steps.len())
                .unwrap_or(0);
            // Each fan-out read emits a `==> path <==` section header. Step
            // outputs are clamped, so their trailers are truncation notes —
            // excluded from the line count and folded into `(+N more)`.
            let files = text.matches("==> ").count();
            let mut lines = 0usize;
            let mut more = 0u64;
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(n) = trailer_more_lines(trimmed) {
                    more += n;
                    continue;
                }
                if trimmed.starts_with("[...") {
                    continue;
                }
                lines += 1;
            }
            let mut out = format!(
                "{steps} steps · {files} file{} · {lines} line{}",
                if files == 1 { "" } else { "s" },
                plural(lines)
            );
            if more > 0 {
                out.push_str(&format!(" (+{more} more)"));
            }
            out
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

/// The ` · then_run: …` tail for a `write`/`edit` summary whose result carries
/// a verification marker. Display-only: `ToolOutcome::ok` still means the
/// mutation landed, so this keeps the verdict ("succeeded" / "failed (exit N)")
/// in view instead of a bare green "N lines written" that implies the check
/// passed. The marker is emitted verbatim by `tools::append_then_run`, never
/// inferred from arbitrary output.
fn then_run_tail(text: &str) -> String {
    match then_run_verdict(text) {
        Some(verdict) => format!(" · then_run: {verdict}"),
        None => String::new(),
    }
}

/// The verdict inside the last `[then_run:…]` marker in `text`, if any.
fn then_run_verdict(text: &str) -> Option<String> {
    let start = text.rfind("\n\n[then_run:")? + 2;
    let body = text[start..].strip_prefix("[then_run:")?;
    let end = body.find(']')?;
    let verdict = body[..end].trim();
    (!verdict.is_empty()).then(|| verdict.to_string())
}

/// Truthful diffstat from the unified diff: counts `+`/`-` hunk lines, so a
/// `replaceAll` edit reports every replicated occurrence. `None` when no
/// diff is available (callers fall back to the input's newText/oldText
/// estimate).
fn diff_stat(diff: Option<&str>) -> Option<(usize, usize)> {
    let diff = diff?;
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    Some((added, removed))
}

/// A grep content-mode row is a match when shaped `path:123:text`; context
/// rows print `path:123-text`, and fff's fuzzy fallback prints `  12: code`
/// under a bare path header (matches too). Trailers start `[...`.
fn grep_match_line(line: &str) -> bool {
    if line.starts_with("[...") {
        return false;
    }
    // Fuzzy fallback row: leading `12: code`.
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && line[digits..].starts_with(": ") {
        return true;
    }
    // Exact hit: `path:123:text` (context is `path:123-text`).
    let Some((_, after_path)) = line.split_once(':') else {
        return false;
    };
    let d = after_path
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .count();
    d > 0 && after_path[d..].starts_with(':')
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

/// Child-agent lifecycle system lines (`[agent <name>:<id>] started|finished
/// …`, formatted in `subagent/tools.rs` / `subagent/manager.rs`) get their own
/// spawn/terminal marker so a delegation pops out of the muted system notes,
/// like the per-tool glyphs do. One owner for the TUI and the headless REPL —
/// the format lives in two emit sites, so the parser must not be duplicated.
/// Returns the marker (`◈` spawn / `◇` terminal) and the text after the
/// `[agent ` prefix.
pub(crate) fn agent_lifecycle(s: &str) -> Option<(&'static str, &str)> {
    let rest = s.strip_prefix("[agent ")?;
    let marker = if rest.contains(" finished ") {
        "◇"
    } else {
        "◈"
    };
    Some((marker, rest))
}

/// Model-facing text for a `!`/`!!` shell run:
/// the persisted message the next turn reads. The output is already clamped
/// for the context window by the bash tool. Single owner for the daemon
/// (`POST /shell`) and local one-shot paths so the two can't diverge.
pub(crate) fn bash_context_text(
    command: &str,
    output: &str,
    success: bool,
    code: Option<i32>,
    cancelled: bool,
) -> String {
    let mut text = format!("Ran `{command}`\n");
    if output.trim().is_empty() {
        text.push_str("(no output)");
    } else {
        text.push_str("```\n");
        text.push_str(output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("```");
    }
    if cancelled {
        text.push_str("\n\n(command cancelled)");
    } else if !success {
        match code {
            Some(code) => text.push_str(&format!("\n\nCommand exited with code {code}")),
            None => text.push_str("\n\nCommand failed"),
        }
    }
    text
}

/// Human-readable approval helpers — keep tool JSON out of the user's face.
/// `approval_title` / `approval_summary` / `approval_details` turn raw
/// `{"path":…}` / `{"command":…}` payloads into the short, scannable
/// lines the overlay and CLI prompt show. No filesystem IO, pure formatting.
/// ponytail: one place for all tool-to-human mapping; add a tool → add a branch.
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
    // `branch` and `status` are independent spawns (~5-30ms each): run them
    // together instead of serially. The `branch.is_some()` guard stays on the
    // *result* — outside a repo `status` prints to stderr, so stdout is empty
    // anyway — at the cost of one wasted spawn in non-repos.
    let mut branch_cmd = AsyncCommand::new("git");
    branch_cmd
        .args(["-C", cwd, "branch", "--show-current"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let mut status_cmd = AsyncCommand::new("git");
    status_cmd
        .args(["-C", cwd, "status", "--porcelain"])
        .stdin(std::process::Stdio::null())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let (branch_out, status_out) = tokio::join!(branch_cmd.output(), status_cmd.output());
    let branch = branch_out
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|branch| !branch.is_empty());
    let dirty = branch.is_some()
        && status_out
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
    (branch, dirty)
}

#[cfg(test)]
mod tests;

mod mcp;

pub(crate) use mcp::{mcp_status_line, render_mcp_panel};
