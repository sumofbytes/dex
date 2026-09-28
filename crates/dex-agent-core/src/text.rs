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

    // Clamp each window to what the line count actually leaves, so head
    // and tail can never overlap (or double-count) when `total <
    // max_lines` but the byte budget still forces the clamp path.
    let head_budget = (max_lines / 2).min(total);
    let tail_budget = (max_lines - head_budget).min(total - head_budget);
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
    if omitted == 0 {
        // Every line survived the byte budget — keep them all, no marker.
        out.extend(tail);
        return (out.join("\n"), clipped_any);
    }
    out.push(format!("[... {omitted} of {total} lines truncated ...]"));
    out.extend(tail);
    (out.join("\n"), true)
}

pub fn truncate_text(text: &str, max_bytes: usize, max_lines: usize) -> String {
    clamp_lines(text, max_lines, max_bytes)
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Any string over the full Unicode range, so char-boundary handling
    /// (multibyte, emoji, combining marks) is exercised.
    fn any_text(max: usize) -> impl proptest::strategy::Strategy<Value = String> {
        proptest::collection::vec(proptest::char::any(), 0..=max)
            .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        /// Below the budget the clip is the identity.
        #[test]
        fn clip_chars_identity_below_budget(s in any_text(80), max in 0usize..=200) {
            if s.chars().count() <= max {
                prop_assert_eq!(&clip_chars(&s, max), &s);
            }
        }

        /// Clipping is idempotent: re-clipping changes nothing.
        #[test]
        fn clip_chars_is_idempotent(s in any_text(120), max in 0usize..=80) {
            let once = clip_chars(&s, max);
            prop_assert_eq!(clip_chars(&once, max), once);
        }

        /// A cut output carries at most `max` chars plus the `…` marker.
        #[test]
        fn clip_chars_char_budget(s in any_text(120), max in 1usize..=80) {
            let out = clip_chars(&s, max);
            prop_assert!(out.chars().count() <= max + 1);
            prop_assert!(out.ends_with('…') || out == s);
        }

        /// Clamped output never exceeds max_lines + 1 (marker) lines and
        /// carries at most one truncation marker.
        #[test]
        fn clamp_lines_bounds(
            lines in proptest::collection::vec("[^\n\r]{0,40}", 0..=60),
            max_lines in 1usize..=12,
            max_bytes in 1usize..=300,
        ) {
            let text = lines.join("\n");
            let out = clamp_lines(&text, max_lines, max_bytes);
            prop_assert!(out.lines().count() <= max_lines + 1);
            prop_assert!(out.matches(" lines truncated ").count() <= 1);
            // A marker is only emitted when lines were actually dropped.
            prop_assert!(!out.contains("[... 0 of "));
        }
    }
}
