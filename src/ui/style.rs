//! Central layout chrome for the TUI: the shared gutters, the spacing of
//! every full-width surface, and semantic `Padding` helpers so renderers
//! never hand-roll a `Padding { .. }` or an inset `Rect` per component.
//!
//! Metrics (how much air each surface gets) and `Padding` construction live
//! here; colors live in `theme`.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::block::Padding;

/// The one style constructor: `style::fg(theme::muted_fg())` instead of
/// `Style::default().fg(..)`. Everything a renderer styles as plain colored
/// text goes through here, so `Style::default()` appears exactly once in the
/// UI — here — and a future default modifier (or bg sweep) is a one-line
/// change instead of a sed across the renderers.
pub(crate) fn fg(color: ratatui::style::Color) -> Style {
    Style::default().fg(color)
}

/// Horizontal air, in cells, shared by every full-width surface (footer,
/// activity strip, queue strip, slash popup) and the transcript's leading
/// indent, so rendered transcript text and the composer's text column always
/// start on the same cell. The composer's band is inset `HORIZONTAL_GUTTER`
/// from each window edge (see `composer_band`) and adds no inside horizontal
/// padding, so its rules get one column of air while its text column lands on
/// the transcript indent. Tuning it moves the composer cursor and the
/// transcript indent together, which is what keeps them aligned.
pub(crate) const HORIZONTAL_GUTTER: u16 = 1;
// `VERTICAL_GUTTER` was removed: no stacked surface carries vertical air
// between rows any more — the only vertical air is `BLOCK_GAP_ROWS` between
// transcript blocks, inserted by `rebuild_display_cache`.

/// Left air of the transcript's shared grid, in cells. Derived from the
/// gutter so tuning the gutter moves both at once.
pub(crate) const TRANSCRIPT_INDENT: usize = HORIZONTAL_GUTTER as usize;

/// Blank rows `TranscriptView::render` inserts between any two transcript
/// blocks (`rebuild_display_cache`); the sole source of inter-block spacing
/// — blocks carry no baked air of their own.
pub(crate) const BLOCK_GAP_ROWS: usize = 2;

/// The composer's top/bottom hairline rules: one row each.
pub(crate) const INPUT_BORDER_ROWS: u16 = 2;
/// Blank air rows above and below the composer text, echoed around the
/// submitted prompt in the transcript so both keep the same shape. The
/// live composer gets this from `composer.rs`'s air rows, not from
/// `input_block` (which adds no vertical padding).
pub(crate) const INPUT_PAD_Y: u16 = 1;
/// Prompt glyph shown on the composer's first row and echoed on the first
/// row of the submitted prompt, so your turns read as yours in the
/// transcript. Width counts the trailing space; wrap width and the row-0
/// cursor x are offset by it.
pub(crate) const INPUT_PROMPT: &str = "❯ ";
pub(crate) const INPUT_PROMPT_WIDTH: usize = 2;
/// Rows of status text under the composer (name + mode line).
pub(crate) const STATUS_CONTENT_ROWS: u16 = 1;
/// Minimum *outer* height of the composer band: one text row plus the
/// two hairline rules (`INPUT_BORDER_ROWS`). The composer grows beyond
/// this as the input wraps, up to the 8-row cap in `compute_layout`.
pub(crate) const INPUT_MIN_ROWS: u16 = 3;
/// Blank rows between the composer band and the status row.
pub(crate) const INPUT_STATUS_GUTTER: u16 = 0;
/// Fixed height of the approval overlay.
pub(crate) const APPROVAL_HEIGHT: u16 = 11;
/// Tab stop for display expansion.
pub(crate) const TAB_WIDTH: usize = 8;

/// Footer `Padding`: side gutters only. The status line hugs the composer's
/// bottom rule and the last screen row — both vertical gutters were dropped:
/// the top one read as a dead gap under the input band, the bottom one as a
/// hole under the footer.
pub(crate) fn status_padding() -> Padding {
    Padding {
        left: HORIZONTAL_GUTTER,
        right: HORIZONTAL_GUTTER,
        top: 0,
        bottom: 0,
    }
}

/// The composer band inset one `HORIZONTAL_GUTTER` column from each window
/// edge, so its top/bottom rules never touch the screen border. The inset
/// *is* the text column: `input_block` adds no horizontal padding, so the
/// band's inner left edge sits on the shared transcript indent and its inner
/// width equals `input_content_width` of the full area.
pub(crate) fn composer_band(area: Rect) -> Rect {
    Rect {
        x: area.x + HORIZONTAL_GUTTER,
        y: area.y,
        width: area.width.saturating_sub(HORIZONTAL_GUTTER * 2),
        height: area.height,
    }
}

/// Inner text width of a full-width surface of `width` columns: the gutter
/// inset from each side.
pub(crate) fn content_width(width: u16) -> u16 {
    width.saturating_sub(HORIZONTAL_GUTTER * 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn prompt_glyph_width_matches_its_constant() {
        // `INPUT_PROMPT_WIDTH` offsets the composer's row-0 wrap width and
        // the echoed prompt's first-row overhang; if the glyph changes
        // width and this drifts, typed and sent prompts reflow differently.
        assert_eq!(INPUT_PROMPT.width(), INPUT_PROMPT_WIDTH);
    }
}
