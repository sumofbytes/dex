#![allow(dead_code)]

use unicode_width::UnicodeWidthChar;

/// Greedily wrap an input line and locate the cursor in the resulting rows.
/// `col` and the returned cursor position use bytes and display cells
/// respectively, matching the input editor and ratatui.
const TAB_WIDTH: usize = 8;

pub(super) fn wrap_line(line: &str, width: usize, col: usize) -> (Vec<String>, u16, u16) {
    let width = width.max(1);
    // Clamp to a char boundary too: a byte col valid on one row can sit
    // inside a multi-byte char on this one (cross-row cursor moves), and
    // `line[start..col]` below would panic mid-render.
    let mut col = col.min(line.len());
    while col > 0 && !line.is_char_boundary(col) {
        col -= 1;
    }
    let mut segments = Vec::new();
    let mut start = 0;
    let mut row_width = 0;
    // Byte range of the last whitespace char that can serve as a wrap
    // point. The break whitespace itself is consumed (standard word-wrap):
    // it renders on neither row, so a trailing space never wraps into a
    // leading space on the next visual line — matching the submitted
    // prompt, which drops the break space when wrapping.
    let mut last_space: Option<(usize, usize)> = None;

    // Walk char-boundary pairs directly; no materialized bounds vector.
    let mut chars = line.char_indices().peekable();
    while let Some((begin, ch)) = chars.next() {
        let end = chars.peek().map_or(line.len(), |&(index, _)| index);
        let char_width = if ch == '\t' {
            TAB_WIDTH - (row_width % TAB_WIDTH)
        } else if ch.is_control() {
            0
        } else {
            ch.width().unwrap_or(0).max(1)
        };

        if ch.is_control() && ch != '\t' {
            // Drop other C0 controls entirely (\r, BEL, etc.) — they would
            // otherwise desync the model vs terminal. Tabs are kept with
            // tabstop-aware width above.
            if begin < col {
                // col points into original line; dropping a control before
                // it doesn't affect display column, so no cursor adjustment
                // needed beyond not counting its width.
            }
            continue;
        }
        if ch.is_whitespace() {
            last_space = Some((begin, end));
        }
        if row_width + char_width > width && begin > start {
            // A whitespace char that itself overflows is collapsed: break
            // before it and skip it, so the next row never starts with a
            // stray space. Otherwise wrap at the last whitespace inside the
            // row and consume it; only hard-break when no break point exists.
            if ch.is_whitespace() && end > begin && last_space == Some((begin, end)) {
                segments.push((start, begin));
                start = end;
                row_width = 0;
                last_space = None;
                continue;
            }
            if let Some((space_begin, space_end)) =
                last_space.filter(|(_, e)| *e > start && *e <= begin)
            {
                segments.push((start, space_begin));
                start = space_end;
                row_width = {
                    let mut w = 0;
                    for c in line[start..begin].chars() {
                        if c == '\t' {
                            w += TAB_WIDTH - (w % TAB_WIDTH);
                        } else if !c.is_control() {
                            w += c.width().unwrap_or(0).max(1);
                        }
                    }
                    w
                };
            } else {
                segments.push((start, begin));
                start = begin;
                row_width = 0;
            }
            last_space = None;
        }
        row_width += char_width;
    }
    segments.push((start, line.len()));

    let strings: Vec<String> = segments
        .iter()
        .map(|&(start, end)| {
            let raw = &line[start..end];
            let mut out = String::with_capacity(raw.len());
            let mut col = 0usize;
            for c in raw.chars() {
                if c == '\t' {
                    let spaces = TAB_WIDTH - (col % TAB_WIDTH);
                    out.push_str(&" ".repeat(spaces));
                    col += spaces;
                } else if c.is_control() {
                    // drop C0 controls (\r, BEL, etc.) – width 0, never rendered
                } else {
                    out.push(c);
                    col += c.width().unwrap_or(0).max(1);
                }
            }
            out
        })
        .collect();

    let mut cursor_segment = segments.len().saturating_sub(1) as u16;
    let mut cursor_x = 0;
    for (index, &(start, end)) in segments.iter().enumerate() {
        let is_last = index + 1 == segments.len();
        // Segments no longer tile the line: the consumed break whitespace
        // leaves a gap (`prev_end` .. `next_start`). A cursor before the gap
        // belongs at the end of the previous row; a cursor after it matches
        // the next row's start. Contiguous boundaries belong to the next row.
        let inside = col >= start && col < end;
        let at_gap_end = col == end && !is_last && segments[index + 1].0 > col;
        if !(inside || (col == end && is_last) || at_gap_end) {
            continue;
        }
        cursor_segment = index as u16;
        let mut w = 0;
        for c in line[start..col].chars() {
            if c == '\t' {
                w += TAB_WIDTH - (w % TAB_WIDTH);
            } else if !c.is_control() {
                w += c.width().unwrap_or(0).max(1);
            }
        }
        cursor_x = w as u16;
        break;
    }
    (strings, cursor_segment, cursor_x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn wraps_at_whitespace_and_tracks_cursor() {
        let (lines, row, column) = wrap_line("one two", 5, "one two".len());
        assert_eq!(lines, vec!["one", "two"]);
        assert_eq!((row, column), (1, 3));
    }

    #[test]
    fn uses_display_width_for_unicode() {
        let (lines, row, column) = wrap_line("ab界d", 3, "ab界".len());
        assert_eq!(lines, vec!["ab", "界d"]);
        assert_eq!((row, column), (1, 2));
        assert_eq!(UnicodeWidthStr::width(lines[1].as_str()), 3);
    }

    #[test]
    fn cursor_col_inside_multibyte_char_snaps_back() {
        // A byte col landing inside a wide char (stale cross-row offset from
        // the editor) must not slice mid-char; the cursor snaps to the
        // char's left boundary instead of panicking the frame.
        let (lines, row, column) = wrap_line("ab\u{7AC7}def", 80, 4);
        assert_eq!(lines, vec!["ab\u{7AC7}def"]);
        assert_eq!((row, column), (0, 2));
    }

    #[test]
    fn narrow_width_still_makes_progress() {
        let (lines, _, _) = wrap_line("abc", 0, 0);
        assert_eq!(lines, vec!["a", "b", "c"]);
    }

    #[test]
    fn wrap_point_on_whitespace_does_not_invert_range() {
        // Width boundary lands exactly on a space: previously this produced
        // line[3..2] and panicked. Greedy wrapping keeps making progress,
        // and the break space is consumed so no row starts with a space.
        let (lines, _, _) = wrap_line("In one", 2, 0);
        assert_eq!(lines, vec!["In", "on", "e"]);
    }

    #[test]
    fn overflowing_space_collapses_instead_of_leading_next_row() {
        // Typing a space at the end of a full row must not indent the next
        // visual line: the space is consumed, matching the submitted prompt.
        let (lines, row, column) = wrap_line("ab cd", 2, "ab ".len());
        assert_eq!(lines, vec!["ab", "cd"]);
        // Cursor sat right after the consumed space: start of the next row.
        assert_eq!((row, column), (1, 0));
        // Cursor right before the consumed space: end of the previous row.
        let (lines, row, column) = wrap_line("ab cd", 2, 2);
        assert_eq!(lines, vec!["ab", "cd"]);
        assert_eq!((row, column), (0, 2));
    }

    #[test]
    fn trailing_space_at_wrap_edge_produces_empty_not_spaced_row() {
        // Full row + trailing space: the space collapses into an empty
        // continuation row (cursor on the next line at x=0), not a row
        // containing a single leading space.
        let (lines, row, column) = wrap_line("ab ", 2, "ab ".len());
        assert_eq!(lines, vec!["ab", ""]);
        assert_eq!((row, column), (1, 0));
    }
}
