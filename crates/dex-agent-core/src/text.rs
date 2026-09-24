/// Per-line character budget: protects against minified/binary-ish content
/// whose single lines would dominate the context window.
pub const MAX_LINE_CHARS: usize = 2_000;

/// Clip a string to at most `max_chars` *characters* — char-boundary-safe,
/// appending a single `…` when anything was cut. The shared body of every
/// char-count clip. This counts Unicode scalar values rather than display
/// columns, which are handled by a presentation layer.
pub fn clip_chars(s: &str, max_chars: usize) -> String {
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
pub fn clamp_lines(text: &str, max_lines: usize, max_bytes: usize) -> String {
    clamp_lines_checked(text, max_lines, max_bytes).0
}

/// `clamp_lines` plus an explicit "the clamp cut bytes" flag. Trailing
/// newline normalization is not a cut — the capture path must not archive a
/// result, and hang a marker on it, merely because its raw form ends in a
/// newline and the join drops that newline.
pub fn clamp_lines_checked(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
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

pub fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    clamp_lines(text, max_lines, max_bytes)
}
