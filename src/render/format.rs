pub(crate) use dex_agent_core::{clamp_lines, clip_chars, truncate_text};
#[cfg(test)]
pub(crate) use dex_agent_core::{clamp_lines_checked, MAX_LINE_CHARS};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
        "read" => read_short_arg(obj.as_ref()),
        "write" | "edit" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "glob" => get("path")
            .or_else(|| get("file"))
            .or_else(|| get("pattern"))
            .or_else(|| get("glob"))
            .map(str::to_string),
        // The input line already shows the tool name, so only the mode.
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
        _ => match one_line_summary(text) {
            first if first.is_empty() => "(no output)".to_string(),
            first => first,
        },
    }
}

pub(super) fn plural(n: usize) -> &'static str {
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
    // Limits must match the journal-side copy in
    // `dex_session::store::model_tool_result` (and vice versa) so replayed
    // history and rendered output agree on what got truncated.
    truncate_text(text, 50 * 1024, 2_000)
}

#[cfg(test)]
mod tests;

mod mcp;

// `/mcp` sheet (TUI) is the only runtime consumer.
#[cfg_attr(not(feature = "tui"), allow(unused_imports))]
pub(crate) use mcp::render_mcp_panel;

mod approval;

#[cfg(all(test, feature = "tui"))]
pub(crate) use approval::approval_risk;
#[cfg(feature = "tui")]
pub(crate) use approval::{
    approval_details, approval_risk_with_then_run, approval_summary, approval_title,
    approval_title_with_then_run, input_has_then_run,
};
#[cfg(not(feature = "tui"))]
// `input_has_then_run` only feeds the TUI approval title/risk helpers.
#[cfg_attr(not(feature = "tui"), allow(unused_imports))]
pub(crate) use approval::{approval_details, approval_summary, approval_title, input_has_then_run};
