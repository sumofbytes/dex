use crossterm::Command;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::fmt;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Mouse drag selection over the transcript, in `display_cache` (row, col)
/// cell space. `anchor`/`end` are the raw press/release points and BOTH
/// cells they land on are covered (native terminal convention: releasing
/// on a char selects it); `norm()` orders them for highlight and copy.
/// `sticky` selections (double-click
/// word picks, triple-click line picks) survive mouse-up so the highlight
/// stays visible until the next click. `whole_line` selections cover full
/// rows: `anchor` is the press point and `end` (moved by dragging) only
/// contributes its row — copy and highlight take every row between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: (usize, usize),
    pub(crate) end: (usize, usize),
    pub(crate) sticky: bool,
    pub(crate) whole_line: bool,
}

impl Selection {
    /// Anchor/end ordered top-left → bottom-right.
    pub(crate) fn norm(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.end.0, self.end.1) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// True when the press never moved (anchor == end): a plain click
    /// clears instead of copying. Sticky picks may still cover a single
    /// cell (one-char word, one-char line) and must copy.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.end
    }
}

/// Expand a click at char index `col` on a display line to the enclosing
/// word: the maximal run of non-whitespace chars, as inclusive char
/// indices. `None` past the line's text or when the click lands on
/// whitespace.
pub(crate) fn word_bounds(line: &Line<'static>, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !chars[end + 1].is_whitespace() {
        end += 1;
    }
    Some((start, end))
}

/// Char count of a display line; see [`last_col`] for the last covered cell
/// of a whole-line (triple-click) pick on that row.
pub(crate) fn line_width(line: &Line<'static>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// Inclusive cell index of a line's final char; `(row, last_col(row))` is the
/// end coordinate of a whole-line (triple-click) pick on that row. Empty lines
/// pick cell 0.
pub(crate) fn last_col(line: &Line<'static>) -> usize {
    line_width(line).saturating_sub(1)
}

/// OSC 52 clipboard set: `ESC ] 52 ; c ; <base64> ST`. Honored by xterm,
/// alacritty, kitty, foot, wezterm, Windows Terminal, and tmux (with
/// `set-clipboard on`); unsupported terminals ignore it silently — the
/// highlight stays visible, so Shift+drag native selection remains the
/// fallback.
pub(crate) struct SetClipboard(pub(crate) String);

impl Command for SetClipboard {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]52;c;{}\x1b\\", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Standard base64 with padding, enough for OSC 52 payloads; no dependency.
pub(crate) fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Translate a mouse event into `display_cache` (row, col) space: the event
/// cell must land inside the transcript area on a real row. `None` outside.
pub(crate) fn mouse_display_cell(
    scroll: u16,
    area: Option<Rect>,
    rows: usize,
    m: &crossterm::event::MouseEvent,
) -> Option<(usize, usize)> {
    let area = area?;
    if m.column < area.x
        || m.row < area.y
        || m.column >= area.x + area.width
        || m.row >= area.y + area.height
    {
        return None;
    }
    let row = scroll as usize + (m.row - area.y) as usize;
    if row >= rows {
        return None;
    }
    Some((row, (m.column - area.x) as usize))
}

/// Column range of a leading fenced-code border prefix on a rendered
/// transcript row: `(start, end)`, or `(0, 0)` when the row carries none.
/// The border arrives as its own `│ ` span (code boxes and blockquotes,
/// right after the whitespace indent); table rows use a bare `│` span and
/// never match, so their pipes survive copies.
fn leading_border_range(line: &Line<'static>) -> (usize, usize) {
    let mut start = 0usize;
    for span in &line.spans {
        let text: &str = span.content.as_ref();
        if !text.is_empty() && text.chars().all(char::is_whitespace) {
            start += UnicodeWidthStr::width(text);
            continue;
        }
        return if text == "│ " {
            (start, start + UnicodeWidthStr::width(text))
        } else {
            (0, 0)
        };
    }
    (0, 0)
}

/// Row text for copies: the `│ ` border span is display furniture, not
/// content, so multi-line copies of a snippet come out without the bar.
fn copy_text(line: &Line<'static>) -> String {
    let (start, end) = leading_border_range(line);
    if start == end {
        // Surface-band rows are padded to the full width with `surface_bg()`
        // spaces so the submitted prompt reads as the composer's echo;
        // strip that display fill so copies stay clean.
        return line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for span in &line.spans {
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col < start || col >= end {
                out.push(ch);
            }
            col += w;
        }
    }
    out
}

/// Map a display column through a stripped leading border: columns before
/// it keep their index, columns inside clamp to its start, columns after it
/// shift left by its width. Monotonic, so `(shift(c0), shift(c1))` stays a
/// valid range — callers pass the start cell and the *exclusive* end bound
/// (for an inclusive release cell `c1`, that's `c1 + 1`).
fn border_shift(col: usize, (start, end): (usize, usize)) -> usize {
    if col <= start {
        col
    } else if col >= end {
        col - (end - start)
    } else {
        start
    }
}

/// Plain text of a normalized selection: full rows join with newlines, the
/// anchor/end rows are sliced to the selected columns — both endpoint cells
/// inclusive, so releasing on a char selects it. Copies what's on
/// screen (pre-wrapped), matching what native terminal selection would hand
/// over, minus border furniture.
pub(crate) fn selection_text(
    rows: &[Line<'static>],
    (r0, c0): (usize, usize),
    (r1, c1): (usize, usize),
) -> String {
    if r0 == r1 {
        return rows
            .get(r0)
            .map(|l| {
                let range = leading_border_range(l);
                let text = copy_text(l);
                let from = border_shift(c0, range);
                // `c1` is the release cell (inclusive); `border_shift`
                // speaks the old exclusive dialect, so hand it `c1 + 1`.
                let to = border_shift(c1 + 1, range);
                text.chars()
                    .skip(from)
                    .take(to.saturating_sub(from))
                    .collect()
            })
            .unwrap_or_default();
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(l) = rows.get(r0) {
        let from = border_shift(c0, leading_border_range(l));
        out.push(copy_text(l).chars().skip(from).collect());
    }
    for line in rows.get(r0 + 1..).into_iter().flatten().take(r1 - r0 - 1) {
        out.push(copy_text(line));
    }
    if let Some(l) = rows.get(r1) {
        let to = border_shift(c1 + 1, leading_border_range(l));
        out.push(copy_text(l).chars().take(to).collect());
    }
    out.join("\n")
}

/// Copy text for whole-line (triple-click) selections: every row from
/// `r0..=r1` in full (border furniture stripped), joined by newlines — no
/// column clipping, so all covered lines are copied whole regardless of
/// length.
pub(crate) fn line_selection_text(rows: &[Line<'static>], r0: usize, r1: usize) -> String {
    (r0..=r1.min(rows.len().saturating_sub(1)))
        .filter_map(|r| rows.get(r))
        .map(copy_text)
        .collect::<Vec<_>>()
        .join("\n")
}
