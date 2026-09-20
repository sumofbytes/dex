//! Central layout chrome for the TUI: the shared gutters, the spacing of
//! every full-width surface, and semantic `Padding` helpers so renderers
//! never hand-roll a `Padding { .. }` or an inset `Rect` per component.
//!
//! Metrics (how much air each surface gets) and `Padding` construction live
//! here; colors live in `theme`.

use ratatui::layout::Rect;
use ratatui::widgets::block::Padding;

/// Horizontal air, in cells, shared by every full-width surface (footer,
/// activity strip, queue strip, slash popup) and the transcript's leading
/// indent, so rendered transcript text and the composer's text column always
/// start on the same cell. The composer's band is inset `HORIZONTAL_GUTTER`
/// from each window edge (see `composer_band`) and adds no inside horizontal
/// padding, so its rules get one column of air while its text column lands on
/// the transcript indent. Tuning it moves the composer cursor and the
/// transcript indent together, which is what keeps them aligned.
pub(crate) const HORIZONTAL_GUTTER: u16 = 1;
/// Vertical air, in rows, between stacked surfaces (transcript ↔ activity,
/// activity ↔ footer). One row above and below; the status row drops its
/// bottom gutter so it sits on the last screen row.
pub(crate) const VERTICAL_GUTTER: u16 = 1;

/// Left air of the transcript's shared grid, in cells. Derived from the
/// gutter so tuning the gutter moves both at once.
pub(crate) const TRANSCRIPT_INDENT: usize = HORIZONTAL_GUTTER as usize;

/// The composer's top/bottom hairline rules: one row each.
pub(crate) const INPUT_BORDER_ROWS: u16 = 2;
/// Blank air rows above and below the composer text, echoed around the
/// submitted prompt in the transcript so both keep the same shape.
pub(crate) const INPUT_PAD_Y: u16 = 1;
/// Prompt glyph shown on the composer's first row and echoed on the first
/// row of the submitted prompt, so your turns read as yours in the
/// transcript. Width counts the trailing space; wrap width and the row-0
/// cursor x are offset by it.
pub(crate) const INPUT_PROMPT: &str = "❯ ";
pub(crate) const INPUT_PROMPT_WIDTH: usize = 2;
/// Rows of status text under the composer (name + mode line).
pub(crate) const STATUS_CONTENT_ROWS: u16 = 1;
/// Minimum text rows the composer shows when empty.
pub(crate) const INPUT_MIN_ROWS: u16 = 3;
/// Blank rows between the composer band and the status row.
pub(crate) const INPUT_STATUS_GUTTER: u16 = 0;
/// Fixed height of the approval overlay.
pub(crate) const APPROVAL_HEIGHT: u16 = 11;
/// Tab stop for display expansion.
pub(crate) const TAB_WIDTH: usize = 8;

/// `Padding` of every full-width surface: `HORIZONTAL_GUTTER` left/right,
/// `VERTICAL_GUTTER` top/bottom. Footer passes `bottom: 0` via
/// `status_padding`.
pub(crate) fn surface_padding() -> Padding {
    Padding {
        left: HORIZONTAL_GUTTER,
        right: HORIZONTAL_GUTTER,
        top: VERTICAL_GUTTER,
        bottom: VERTICAL_GUTTER,
    }
}

/// Footer `Padding`: gutters on top and sides, none on the bottom — the
/// status line sits on the last screen row. The terminal adds its own dead
/// space under the grid, and the dropped row read as a hole under the
/// footer.
pub(crate) fn status_padding() -> Padding {
    Padding {
        left: HORIZONTAL_GUTTER,
        right: HORIZONTAL_GUTTER,
        top: VERTICAL_GUTTER,
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
