#[cfg(test)]
use super::status::footer_line;
#[cfg(test)]
use super::status::truncate_display;
use super::style::content_width as input_content_width;
use super::style::fg;
#[cfg(test)]
use super::style::TRANSCRIPT_INDENT;
use super::App;
#[cfg(test)]
use super::InputField;
#[cfg(test)]
use super::Selection;
#[cfg(test)]
use super::TAB_WIDTH;
use crate::render::theme;
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
mod tool;
mod transcript;
pub(super) use activity::{queue_groups, queue_metrics_of};
pub(crate) use bottom::{approval_panel_rows, BottomPane, QuestionOverlay, SlashSuggestionsView};
pub(super) use composer::render_input;
#[cfg(test)]
pub(super) use markdown::markdown_lines;
pub(super) use markdown::markdown_lines_at;
#[cfg(test)]
pub(crate) use markdown::split_markdown;
pub(super) use markdown::DEFAULT_TABLE_WIDTH;
pub(super) use preview::{render_read_preview, render_search_preview, render_tool_arg};
pub(super) use thinking::format_elapsed;
pub(super) use transcript::display_offset;
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
pub(crate) use transcript::{apply_selection, rebuild_display_cache, wrap_block, SEL_BG};

pub(super) fn input_block() -> Block<'static> {
    // Borderless sides, hairline rules top and bottom: the composer is a
    // band on the terminal's own background (no surface fill — the
    // transcript's user band has none either), framed by two `─` rules in
    // the shared `hairline_style()` so it reads as an edge, not a box. The
    // band is already inset one column from the window edges
    // (`composer_band`), which supplies both the rules' air and the text
    // column, so the block adds no horizontal padding and the caret and
    // the transcript's leading indent land on the same cell. No vertical
    // padding: the empty composer is one text row between the two rules
    // and grows only as the input wraps.
    Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(theme::hairline_style())
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

/// Bottom-pane layout. `approval_rows > 0` swaps the composer for the approval
/// panel (same slot, `approval_rows` tall); the transcript keeps at least a
/// few rows either way.
pub(super) fn compute_layout(
    area: Rect,
    input_rows: u16,
    queue: QueueMetrics,
    approval_rows: u16,
) -> Option<UiLayout> {
    let mut activity_h = activity_height(queue.items, queue.rows);
    let footer_height = super::INPUT_STATUS_GUTTER + status_height();
    // Too short for the full bottom pane: drop the queue strip before
    // touching the composer. Queued text survives in app state and
    // reappears once space returns; a vanished composer leaves the agent
    // uncontrollable.
    if area.height < minimum_view_height(activity_h, 0) {
        activity_h = 0;
    }
    // Below composer + footer minimums nothing fits: transcript-only.
    if area.height < minimum_view_height(activity_h, 0) {
        return Some(UiLayout {
            transcript: area,
            activity: Rect::new(area.x, area.y, area.width, 0),
            input: Rect::new(area.x, area.y, area.width, 0),
            footer: Rect::new(area.x, area.y, area.width, 0),
        });
    }

    let wanted = if approval_rows > 0 {
        approval_rows.max(super::INPUT_MIN_ROWS)
    } else {
        input_outer_height(input_rows).clamp(super::INPUT_MIN_ROWS, 8)
    };
    // An approval outranks the transcript: deciding blind is worse than a
    // squeezed history.
    let min_transcript = if approval_rows > 0 {
        1
    } else {
        MIN_TRANSCRIPT_ROWS
    };
    let input_h = wanted.min(
        area.height
            .saturating_sub(activity_h + footer_height + min_transcript),
    );
    let input_h = input_h
        .max(super::INPUT_MIN_ROWS.min(area.height.saturating_sub(activity_h + footer_height)));
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

/// Transcript rows the bottom pane may not squeeze out.
const MIN_TRANSCRIPT_ROWS: u16 = 3;

/// Plan §20 child view: the child log rendered inside the parent's
/// transcript window — a one-row title (definition name + id + key hints)
/// on top, the child log below. Rendered through [`TranscriptView::render`]
/// so child blocks wrap, style, scroll, and autoscroll exactly like the
/// parent transcript's. The composer, queue strip, and footer stay visible
/// (the session stays controllable), and the approval overlay draws over
/// this in [`view`].
fn render_child_view(f: &mut ratatui::Frame, area: Rect, app: &mut App, idx: usize) {
    let (title, live) = {
        let log = &app.child_logs[idx];
        let live = app.agents.iter().any(|chip| chip.id == log.id);
        (format!("\u{27e1} {} \u{b7} {}", log.name, log.id), live)
    };
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    let title_line = ratatui::text::Line::from(vec![
        ratatui::text::Span::styled(title, fg(theme::accent_fg())),
        ratatui::text::Span::styled(
            if live {
                "  \u{25cf} running"
            } else {
                "  \u{25cb} done"
            },
            fg(if live {
                theme::success_fg()
            } else {
                theme::muted_fg()
            }),
        ),
        ratatui::text::Span::styled(
            "   Esc close \u{b7} Ctrl+A next \u{b7} PgUp/PgDn scroll",
            fg(theme::muted_fg()),
        ),
    ]);
    f.render_widget(ratatui::widgets::Paragraph::new(title_line), chunks[0]);
    TranscriptView::render(f, chunks[1], &mut app.child_logs[idx].app);
}

/// Split the agents/tasks activity strip off the bottom row, under the
/// footer, while there is something to show and room to spare: the strip
/// is the first thing shed on a short terminal (before the queue strip and
/// the composer). Returns the remaining area for the regular layout.
fn split_activity_strip(f: &mut ratatui::Frame, area: Rect, app: &App) -> Rect {
    let Some(line) = super::status::activity_line(app, input_content_width(area.width)) else {
        return area;
    };
    if area.height <= minimum_view_height(0, 0) + MIN_TRANSCRIPT_ROWS {
        return area;
    }
    let strip = Rect::new(area.x, area.bottom() - 1, area.width, 1);
    f.render_widget(
        ratatui::widgets::Paragraph::new(line)
            .block(Block::default().padding(super::style::status_padding())),
        strip,
    );
    Rect::new(area.x, area.y, area.width, area.height - 1)
}

/// Background task output view (Ctrl+B / `/tasks <id>`): a title row (id,
/// command, state, key hints) over the log's plain lines, bottom-anchored
/// so new output stays in sight unless scrolled up. Lines are clipped, not
/// wrapped — they are raw process output, often wide tables/progress bars.
fn render_task_view(f: &mut ratatui::Frame, area: Rect, app: &mut App, idx: usize) {
    use ratatui::text::{Line, Span};
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
    let body_h = chunks[1].height as usize;
    let log = &app.task_logs[idx];
    let running = app.tasks.iter().any(|t| t.id == log.id && !t.done);
    let max_scroll = log.lines.len().saturating_sub(body_h);
    app.task_scroll = app.task_scroll.min(max_scroll);
    let end = log.lines.len() - app.task_scroll;
    let start = end.saturating_sub(body_h);
    let width = input_content_width(area.width);
    let title = Line::from(vec![
        Span::styled(format!("\u{27f3} {}", log.id), fg(theme::accent_fg())),
        Span::styled(
            format!(
                " \u{b7} {}",
                super::status::truncate_display(&log.command, 40)
            ),
            fg(theme::muted_fg()),
        ),
        if running {
            Span::styled("  \u{25cf} running", fg(theme::success_fg()))
        } else {
            Span::styled("  \u{25cb} done", fg(theme::muted_fg()))
        },
        Span::styled(
            "   Esc close \u{b7} Ctrl+B next \u{b7} PgUp/PgDn scroll",
            fg(theme::muted_fg()),
        ),
    ]);
    let body: Vec<Line> = log.lines[start..end]
        .iter()
        .map(|line| Line::raw(super::status::truncate_display(line, width)))
        .collect();
    let block = Block::default().padding(super::style::status_padding());
    f.render_widget(
        ratatui::widgets::Paragraph::new(title).block(block.clone()),
        chunks[0],
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(body).block(block),
        chunks[1],
    );
}

pub(crate) fn view(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    // Ratatui only repaints cells the widget touches; without a full clear,
    // a shorter line (e.g. fewer queued-steer badges, or a shrunken input)
    // would leave trailing chars from the previous frame.
    f.render_widget(Clear, area);
    let area = split_activity_strip(f, area, app);
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
    // A pending approval takes the composer's slot (inline, transcript stays
    // visible); the `ask_user` wizard is still a centered modal.
    let approval_rows = approval_panel_rows(app, area.width);
    let layout =
        compute_layout(area, input_rows, queue, approval_rows).expect("layout always exists");
    // Plan §20 child view: the open child transcript replaces only the
    // parent's transcript window (title bar + child log); the composer and
    // footer stay so the session remains controllable. The approval
    // overlay below renders on top of it.
    if let Some(idx) = app
        .child_view
        .as_ref()
        .and_then(|id| app.child_logs.iter().position(|log| &log.id == id))
    {
        render_child_view(f, layout.transcript, app, idx);
    } else if let Some(idx) = app
        .task_view
        .as_ref()
        .and_then(|id| app.task_logs.iter().position(|log| &log.id == id))
    {
        render_task_view(f, layout.transcript, app, idx);
    } else {
        TranscriptView::render(f, layout.transcript, app);
    }
    BottomPane::render(f, &layout, app, input_lines, input_cursor, &groups);
    if !app.pending_questions.is_empty() {
        QuestionOverlay::render(f, area, app);
    }
    if app.pending_approvals.is_empty() {
        SlashSuggestionsView::render(f, layout.input, app);
    }
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
