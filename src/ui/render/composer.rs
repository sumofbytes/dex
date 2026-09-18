use super::super::theme;
use super::super::wrapping::wrap_line;
use super::super::App;
use super::super::InputField;
use super::input_block;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;

pub(crate) struct ComposerView;

impl ComposerView {
    // `input_lines`/`cursor` are rendered once per frame in `view` (they
    // also size the layout); re-wrapping here doubled the composer cost.
    pub(super) fn render(
        f: &mut ratatui::Frame,
        area: Rect,
        app: &mut App,
        lines: Vec<Line<'static>>,
        cursor: (u16, u16, u16),
    ) {
        f.render_widget(Clear, area);
        // One subtle `surface_bg()` band behind the text (see `input_block`);
        // the busy state dims the text with it.
        let input_style = if app.busy || !app.pending_approvals.is_empty() {
            Style::default().fg(theme::muted_fg())
        } else {
            Style::default().fg(theme::surface_fg())
        };
        let block = input_block();
        let inner = block.inner(area);
        let content_rows = inner.height;
        let scroll = (cursor.0 + 1).saturating_sub(content_rows);
        let paragraph = Paragraph::new(lines)
            .style(input_style)
            .scroll((scroll, 0))
            .block(block);
        f.render_widget(paragraph, area);

        // The composer owns keyboard focus whenever no modal is up —
        // including while the agent works, since typing + Enter queues a
        // steering message. The dim busy style signals the state; only the
        // approval modal (which consumes keys) hides the cursor.
        if app.pending_approvals.is_empty() {
            let cur_y = cursor.2.saturating_sub(scroll);
            f.set_cursor_position((inner.x + cursor.1, inner.y + cur_y));
        }
    }
}

pub(crate) fn render_input(
    input: &InputField,
    width: u16,
    dim: bool,
) -> (Vec<Line<'static>>, (u16, u16, u16)) {
    let w = width.max(1) as usize;
    // No prompt glyph: bare text on the shared transcript margin is the
    // visual cue that the row below the transcript is where you type, and
    // every visual row gets the full width.
    // The user's words carry the signature voice color while typing, so
    // typed and submitted prompts match. The whole composer falls back to
    // muted while busy or awaiting approval.
    let text_fg = if dim {
        theme::muted_fg()
    } else {
        theme::user_fg()
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur_row: u16 = 0;
    let mut cur_x: u16 = 0;
    for (li, line) in input.lines.iter().enumerate() {
        let (segs, seg_idx, x) = wrap_line(line, w, input.col);
        for seg in segs {
            lines.push(Line::from(Span::styled(seg, Style::default().fg(text_fg))));
        }
        if li == input.row {
            cur_row += seg_idx;
            cur_x = x;
        } else if li < input.row {
            cur_row += wrap_line(line, w, line.len()).0.len() as u16;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            String::new(),
            Style::default().fg(text_fg),
        )));
    }
    (lines, (cur_row, cur_x, cur_row))
}
