#[cfg(test)]
use super::status::footer_line;
#[cfg(test)]
use super::status::truncate_display;
use super::style::content_width as input_content_width;
use super::style::fg;
#[cfg(test)]
use super::style::TRANSCRIPT_INDENT;
use super::theme;
use super::App;
#[cfg(test)]
use super::InputField;
#[cfg(test)]
use super::Selection;
#[cfg(test)]
use super::TAB_WIDTH;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::Color;
#[cfg(test)]
use ratatui::text::Line;
#[cfg(test)]
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
#[cfg(test)]
use ratatui::widgets::Paragraph;
#[cfg(test)]
use ratatui_markdown::markdown::MarkdownBlock;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

mod activity;
mod bottom;
mod composer;
mod markdown;
mod preview;
mod thinking;
mod transcript;
pub(super) use activity::{queue_groups, queue_metrics_of};
pub(crate) use bottom::{ApprovalOverlay, BottomPane, SlashSuggestionsView};
pub(super) use composer::render_input;
pub(super) use markdown::markdown_lines;
#[cfg(test)]
pub(crate) use markdown::split_markdown;
pub(super) use preview::{render_read_preview, render_search_preview, render_tool_input};
pub(super) use thinking::format_elapsed;
#[cfg(test)]
pub(crate) use transcript::wrap_line_display;
pub(super) use transcript::TranscriptView;

// Test-only helpers from the children (rendered-view unit tests below).
#[cfg(test)]
pub(crate) use activity::QUEUE_MAX_ITEM_ROWS;
#[cfg(test)]
pub(crate) use markdown::highlight_code_block;
#[cfg(test)]
pub(crate) use preview::render_approval_detail;
#[cfg(test)]
pub(crate) use thinking::{
    extend_thinking_rows, thinking_display_lines, thinking_indicator_text, wrap_thinking_full,
};
#[cfg(test)]
pub(crate) use transcript::{apply_selection, wrap_block, SEL_BG};

pub(super) fn input_block() -> Block<'static> {
    // Borderless sides, hairline rules top and bottom: the composer is a
    // band on the terminal's own background (no surface fill — the
    // transcript's user band has none either), framed by two `─` rules in
    // the shared `hairline_fg()` so it reads as an edge, not a box. The
    // band is already inset one column from the window edges
    // (`composer_band`), which supplies both the rules' air and the text
    // column, so the block adds no horizontal padding and the caret and
    // the transcript's leading indent land on the same cell. No vertical
    // padding: the empty composer is one text row between the two rules
    // and grows only as the input wraps.
    Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(fg(theme::hairline_fg()))
}

pub(super) fn input_outer_height(content_rows: u16) -> u16 {
    content_rows + super::INPUT_BORDER_ROWS
}

pub(super) fn activity_height(item_count: u16, line_count: u16) -> u16 {
    if item_count == 0 {
        return 0;
    }
    // One content row per line (a multiline queued item spans several rows)
    // plus one blank separator between items; no gutters — the strip sits
    // flush between the transcript and the composer's top rule.
    line_count.saturating_add(item_count.saturating_sub(1))
}

/// Footer chunk: no gutter above the status row — it hugs the composer's
/// bottom hairline (the old gutter row read as a dead gap between them) and
/// the status line sits on the last screen row.
pub(super) fn status_height() -> u16 {
    super::STATUS_CONTENT_ROWS
}

pub(super) fn minimum_view_height(activity_h: u16, approval_h: u16) -> u16 {
    activity_h + approval_h + status_height() + super::INPUT_MIN_ROWS + super::INPUT_STATUS_GUTTER
}

pub(super) struct UiLayout {
    pub(super) transcript: Rect,
    pub(super) activity: Rect,
    pub(super) input: Rect,
    pub(super) footer: Rect,
}

/// Items and content rows the pending-queue strip renders, straight from
/// `queue_groups` so sizing agrees with the drawing.
#[derive(Clone, Copy)]
pub(crate) struct QueueMetrics {
    pub(super) items: u16,
    pub(super) rows: u16,
}

pub(super) fn compute_layout(
    area: Rect,
    input_rows: u16,
    queue: QueueMetrics,
    approval_pending: bool,
) -> Option<UiLayout> {
    let mut activity_h = activity_height(queue.items, queue.rows);
    let mut approval_h = if approval_pending {
        super::APPROVAL_HEIGHT
    } else {
        0
    };
    let footer_height = super::INPUT_STATUS_GUTTER + status_height();
    // Too short for the full bottom pane: drop the queue strip and the
    // reserved approval band before touching the composer. Queued text
    // survives in app state and reappears once space returns; a vanished
    // composer leaves the agent uncontrollable.
    if area.height < minimum_view_height(activity_h, approval_h) {
        activity_h = 0;
        approval_h = 0;
    }
    // Below composer + footer minimums nothing fits: transcript-only.
    if area.height < minimum_view_height(activity_h, approval_h) {
        return Some(UiLayout {
            transcript: area,
            activity: Rect::new(area.x, area.y, area.width, 0),
            input: Rect::new(area.x, area.y, area.width, 0),
            footer: Rect::new(area.x, area.y, area.width, 0),
        });
    }

    let input_h = input_outer_height(input_rows)
        .clamp(super::INPUT_MIN_ROWS, 8)
        .min(
            area.height
                .saturating_sub(activity_h + approval_h + footer_height),
        );
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(activity_h),
        Constraint::Length(input_h),
        Constraint::Length(super::INPUT_STATUS_GUTTER),
        Constraint::Length(status_height()),
    ])
    .split(area);

    Some(UiLayout {
        transcript: chunks[0],
        activity: chunks[1],
        input: chunks[2],
        footer: chunks[4],
    })
}

pub(crate) fn view(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Ratatui only repaints cells the widget touches; without a full clear,
    // a shorter line (e.g. fewer queued-steer badges, or a shrunken input)
    // would leave trailing chars from the previous frame.
    f.render_widget(Clear, area);
    // ponytail: wrap the composer once — the rows size the layout and
    // render it, so don't pay `render_input` twice per frame.
    let (input_lines, input_cursor) = render_input(
        &app.input,
        input_content_width(area.width),
        app.busy || !app.pending_approvals.is_empty(),
    );
    let input_rows = input_lines.len() as u16;
    // The busy "● Working" status lives in the transcript (turn-activity
    // block); this strip only sizes for the pending queue. The groups are
    // built once per frame here (§29) and shared with the strip drawing
    // below, instead of rebuilt in both sizing and drawing.
    let groups = queue_groups(app);
    let queue = queue_metrics_of(&groups);
    // Approval is a centered modal, not a bottom-pane split — don't reserve
    // APPROVAL_HEIGHT in the main layout; it would shrink the transcript for
    // no reason and push the composer up.
    let layout = compute_layout(area, input_rows, queue, false).expect("layout always exists");

    TranscriptView::render(f, layout.transcript, app);
    BottomPane::render(f, &layout, app, input_lines, input_cursor, &groups);
    if !app.pending_approvals.is_empty() {
        ApprovalOverlay::render(f, area, app);
    }
    SlashSuggestionsView::render(f, layout.input, app);
}

#[test]
fn composer_text_carries_user_voice() {
    // Typed words wear the signature color so typed and submitted prompts
    // match; while dim (busy/approvals) the text mutes.
    let input = InputField::from_text("hello");
    let (active, _) = render_input(&input, 40, false);
    let text: String = active[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text, "❯ hello");
    assert!(active[0]
        .spans
        .iter()
        .all(|s| s.style.fg == Some(theme::user_fg())));
    let (dimmed, _) = render_input(&input, 40, true);
    assert!(dimmed[0]
        .spans
        .iter()
        .all(|s| s.style.fg == Some(theme::muted_fg())));
}
#[cfg(test)]
pub(crate) mod tests;
