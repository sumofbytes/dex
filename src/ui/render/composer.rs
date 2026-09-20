use super::super::style::fg;
use super::super::style::{composer_band, INPUT_PROMPT, INPUT_PROMPT_WIDTH};
use super::super::theme;
use super::super::wrapping::wrap_line;
use super::super::App;
use super::super::InputField;
use super::input_block;
use ratatui::layout::Rect;
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
        let band = composer_band(area);
        f.render_widget(Clear, band);
        // Bare text on the terminal background between the two hairline rules
        // (see `input_block`); the busy state dims the text.
        let input_style = if app.busy || !app.pending_approvals.is_empty() {
            fg(theme::muted_fg())
        } else {
            fg(theme::surface_fg())
        };
        let block = input_block();
        let inner = block.inner(band);
        let content_rows = inner.height;
        let scroll = (cursor.0 + 1).saturating_sub(content_rows);
        let paragraph = Paragraph::new(lines)
            .style(input_style)
            .scroll((scroll, 0))
            .block(block);
        f.render_widget(paragraph, band);

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
    // The `❯ ` glyph marks the first row as the typing edge; it is echoed
    // on the submitted prompt's first row (see `render_user_prompt`), so
    // typed and sent share one shape. The first logical line wraps
    // `INPUT_PROMPT_WIDTH` narrower and the glyph's own width carries the
    // cursor/indent, so everything after the glyph sits on the shared text
    // column and nothing clips the band's right edge.
    // The user's words carry the signature voice color while typing, so
    // typed and submitted prompts match. The whole composer falls back to
    // muted while busy or awaiting approval.
    let text_fg = if dim {
        theme::muted_fg()
    } else {
        theme::user_fg()
    };
    // The glyph wears the same voice color as the words — one voice per
    // prompt row.
    let prompt_span = Span::styled(INPUT_PROMPT.to_string(), fg(text_fg));
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur_row: u16 = 0;
    let mut cur_x: u16 = 0;
    for (li, line) in input.lines.iter().enumerate() {
        // The first visual row starts after the glyph, so it wraps
        // `INPUT_PROMPT_WIDTH` narrower; every later row uses the full
        // width.
        let first = lines.is_empty();
        let wrap_w = if first {
            w.saturating_sub(INPUT_PROMPT_WIDTH)
        } else {
            w
        };
        let (segs, seg_idx, x) = wrap_line(line, wrap_w, input.col);
        for (si, seg) in segs.iter().enumerate() {
            let mut spans = Vec::new();
            if first && si == 0 {
                spans.push(prompt_span.clone());
            }
            spans.push(Span::styled(seg.clone(), fg(text_fg)));
            lines.push(Line::from(spans));
        }
        if li == input.row {
            cur_row += seg_idx;
            cur_x = if first && seg_idx == 0 {
                INPUT_PROMPT_WIDTH as u16 + x
            } else {
                x
            };
        } else if li < input.row {
            cur_row += segs.len() as u16;
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(vec![
            prompt_span,
            Span::styled(String::new(), fg(text_fg)),
        ]));
        if input.row == 0 {
            cur_x = INPUT_PROMPT_WIDTH as u16;
        }
    }
    (lines, (cur_row, cur_x, cur_row))
}
