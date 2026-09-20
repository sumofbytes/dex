use super::super::slash;
use super::super::status::footer_line;
use super::super::status::truncate_display;
use super::super::style::fg;
use super::super::style::{composer_band, content_width, status_padding};
use super::super::theme;
use super::super::App;
use super::activity::ActivityView;
use super::activity::QueueGroup;
use super::composer::ComposerView;
use super::preview::render_approval_detail;
use super::UiLayout;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::block::Padding;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Clear;
use ratatui::widgets::List;
use ratatui::widgets::ListItem;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use unicode_width::UnicodeWidthStr;

struct FooterView;

impl FooterView {
    fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        let width = content_width(area.width);
        let line = footer_line(app, width);
        // Status row sits on the last screen row: top gutter only. The
        // terminal adds its own dead space below the grid, and the old
        // bottom gutter row read as a hole under the footer.
        f.render_widget(
            Paragraph::new(line).block(Block::default().padding(status_padding())),
            area,
        );
    }
}

pub(crate) struct SlashSuggestionsView;

impl SlashSuggestionsView {
    pub(super) fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        let suggestions = slash::slash_suggestions(app);
        if suggestions.is_empty() || area.height < 3 || area.width < 10 {
            return;
        }
        app.slash_selected = app.slash_selected.min(suggestions.len() - 1);
        let input = app.input.text();
        // A bare `/model ` matches the whole catalog (50+ entries): cap the
        // visible rows so the popup stays a small list above the composer
        // instead of a full-transcript wall, and scroll it with the selection.
        const MAX_VISIBLE: usize = 10;
        let mut visible = suggestions.len().min(MAX_VISIBLE);
        // Copy-safe sheet above the composer: only a `─` rule on top for
        // separation, no box-drawing elsewhere (`╭`/`│ `/`───` per row) so a
        // native terminal selection pastes plain commands (at worst one
        // leading `────` line to drop). No background either: rows carry no
        // fill, so nothing extra to strip. The `> ` marker plus the
        // default fg (vs cyan) is the only selection indicator (skip the
        // 2-wide marker gutter when copying a command, as with any picker
        // affordance). One blank gutter row sits between the header text
        // and the first command so the list breathes instead of butting the
        // header. Sheet chrome: rule + header + blank gutter = 3 rows above
        // the list.
        const SHEET_CHROME_ROWS: u16 = 3;
        let height = (visible as u16 + SHEET_CHROME_ROWS).min(area.y);
        if height < SHEET_CHROME_ROWS + 1 {
            return;
        }
        visible = visible.min((height - SHEET_CHROME_ROWS) as usize);
        if visible == 0 {
            return;
        }
        let max_start = suggestions.len().saturating_sub(visible);
        let start = app
            .slash_selected
            .saturating_sub(visible.saturating_sub(1))
            .min(max_start);
        let window = &suggestions[start..start + visible];
        // Sheet width matches the composer band minus its left gutter: the
        // labels share the composer's text column (glyph + gap to the
        // left). The sheet is inset one extra column (so its text lands on
        // the composer's text column, past the band's rule start), leaving
        // one column of air on each side like the composer's rules.
        // Inside a picker (`/model `, `/provider `, `/resume …`) rows show
        // just the item (`> gpt-5`), not the repeated command (`/model
        // <item>`) — the header already names the picker.
        let band = composer_band(area);
        let width = band
            .width
            .saturating_sub(super::super::style::HORIZONTAL_GUTTER);
        let avail = width.saturating_sub(2) as usize;
        let cmd_col = window
            .iter()
            .map(|(command, _)| UnicodeWidthStr::width(slash::suggestion_label(&input, command)))
            .max()
            .unwrap_or(0)
            .min(48)
            .min(avail.max(1));
        let popup = Rect {
            x: band.x + super::super::style::HORIZONTAL_GUTTER,
            y: area.y - height,
            width,
            height,
        };
        // Marker gutter (2) + command + gap (2); the rest is description.
        // No fills: `Clear` already blanked the sheet, and the top rule is
        // the only border, so rows copy as plain text (at most the leading
        // `────` line, plus the `> ` marker gutter to skip).
        let inner_w = width as usize;
        let desc_w = inner_w.saturating_sub(cmd_col + 4) as u16;
        let items = window
            .iter()
            .enumerate()
            .map(|(offset, (command, description))| {
                let selected = start + offset == app.slash_selected;
                let marker_style = if selected {
                    fg(theme::accent_fg())
                } else {
                    fg(theme::muted_fg())
                };
                let command_style = if selected {
                    // `>` plus the brighter fg mark the selection; unselected
                    // rows stay plain cyan. No bold — chrome stays quiet.
                    fg(theme::surface_fg())
                } else {
                    fg(theme::accent_fg())
                };
                let description_style = if selected {
                    fg(theme::surface_fg())
                } else {
                    fg(theme::secondary_fg())
                };
                let label = slash::suggestion_label(&input, command);
                let cell = truncate_display(label, cmd_col as u16);
                let pad = cmd_col.saturating_sub(UnicodeWidthStr::width(cell.as_str()));
                let mut cell = cell;
                cell.push_str(&" ".repeat(pad + 2));
                let desc = truncate_display(description, desc_w);
                Line::from(vec![
                    Span::styled(if selected { "> " } else { "  " }, marker_style),
                    Span::styled(cell, command_style),
                    Span::styled(desc, description_style),
                ])
            });
        let base = if input.starts_with("/model ") {
            "Models"
        } else if input.starts_with("/provider ") {
            "Providers"
        } else if input.starts_with("/resume") {
            "Sessions"
        } else {
            "Slash commands"
        };
        let header_text = if suggestions.len() > visible {
            format!(
                // Two-space lead matches the `> `/`  ` marker gutter so the
                // header text starts at the same column as the item labels.
                "  {base} {}/{}   ↑↓ navigate · Enter select · Tab complete ",
                app.slash_selected + 1,
                suggestions.len()
            )
        } else {
            format!("  {base}   ↑↓ navigate · Enter select · Tab complete ")
        };
        let header_text = truncate_display(&header_text, width);
        // Single `─` rule across the top is the sheet's only border: it
        // separates the popup from the transcript without side/corner
        // glyphs, and a stray leading `────` line is the only copy artifact.
        // The row at `+ 2` stays cleared (blank gutter) so the first command
        // at `+ 3` doesn't butt the header text at `+ 1`.
        let sheet_row = |dy: u16| Rect {
            x: popup.x,
            y: popup.y + dy,
            width: popup.width,
            height: 1,
        };
        f.render_widget(Clear, popup);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner_w),
                fg(theme::hairline_fg()),
            ))),
            sheet_row(0),
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(header_text, fg(theme::muted_fg())))),
            sheet_row(1),
        );
        f.render_widget(
            List::new(items),
            Rect {
                x: popup.x,
                y: popup.y + SHEET_CHROME_ROWS,
                width: popup.width,
                height: visible as u16,
            },
        );
    }
}

pub(crate) struct BottomPane;

impl BottomPane {
    pub(super) fn render(
        f: &mut ratatui::Frame,
        layout: &UiLayout,
        app: &mut App,
        input_lines: Vec<Line<'static>>,
        input_cursor: (u16, u16, u16),
        queue: &[QueueGroup],
    ) {
        if layout.activity.height > 0 {
            ActivityView::render(f, layout.activity, queue);
        }
        if layout.input.height > 0 {
            ComposerView::render(f, layout.input, app, input_lines, input_cursor);
        } else {
            drop((input_lines, input_cursor));
        }
        if layout.footer.height > 0 {
            FooterView::render(f, layout.footer, app);
        }
    }
}

pub(crate) struct ApprovalOverlay;

impl ApprovalOverlay {
    pub(super) fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        let Some(approval) = app.pending_approvals.first() else {
            return;
        };
        // V1b: child agents label their prompts and can queue several.
        let extra = app.pending_approvals.len().saturating_sub(1);
        let agent_prefix = approval
            .agent
            .as_deref()
            .map(|agent| format!("{agent} wants to "))
            .unwrap_or_default();
        // — centered modal, clean readable command —
        // Parsed once at enqueue (§29); never re-parse per frame.
        let title = &approval.title;
        let summary = &approval.summary;
        let details = &approval.details;
        let (risk_label, risk_color) = (approval.risk_label, approval.risk_color);
        // width clamped so modal feels floating, not full-bleed; height grows with details
        let width = area
            .width
            .saturating_sub(6)
            .clamp(52, 76)
            .min(area.width.saturating_sub(2));
        let detail_rows = details.len() as u16;
        // header 2 + gap 1 + details + gap 1 + options 3 + hint 1 + borders(2) + padding(2) = 12+details
        let needed = detail_rows.saturating_add(12).clamp(13, 22);
        let height = needed.min(area.height.saturating_sub(4)).max(13);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let popup = Rect {
            x,
            y,
            width,
            height,
        };
        f.render_widget(Clear, popup);
        let block = Block::default()
            .title(format!(" {} — {} ", title, approval.name))
            .title_style(fg(theme::warn_fg()))
            .borders(Borders::ALL)
            .border_style(fg(theme::warn_fg()))
            .padding(Padding::new(1, 1, 1, 1))
            .style(Style::default().bg(theme::popup_bg()));
        let inner = block.inner(popup);
        f.render_widget(block, popup);

        // inside: header (title+summary), label, details, spacer, options, hint
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(detail_rows.min(inner.height.saturating_sub(7)).max(1)),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
        ])
        .split(inner);

        let header_line = Line::from(vec![
            Span::styled(title.to_string(), fg(theme::accent_fg())),
            Span::styled("  ·  ", fg(theme::muted_fg())),
            Span::styled(format!("{} risk", risk_label), fg(risk_color)),
            Span::styled(format!("  ·  {}", approval.name), fg(theme::muted_fg())),
        ]);
        let sub = Line::from(Span::styled(summary.clone(), fg(theme::surface_fg())));
        f.render_widget(
            Paragraph::new(vec![header_line, sub]).wrap(Wrap { trim: false }),
            chunks[0],
        );
        let wants = format!("The {agent_prefix}agent wants to run:");
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(wants, fg(theme::muted_fg())))),
            chunks[1],
        );
        // Queued behind this one (V1b): child agents can park several.
        if extra > 0 {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("+{extra} more approval(s) waiting"),
                    fg(theme::warn_fg()),
                ))),
                chunks[5],
            );
        }
        let detail_lines: Vec<Line> = details
            .iter()
            .map(|d| render_approval_detail(&approval.name, d))
            .collect();
        f.render_widget(
            Paragraph::new(detail_lines).wrap(Wrap { trim: false }),
            chunks[2],
        );

        let labels = [
            ("Allow once", "y", "just this time"),
            ("Allow for session", "s", "remember"),
            ("Deny", "n", "block"),
        ];
        let items: Vec<ListItem> = labels
            .iter()
            .enumerate()
            .map(|(idx, (label, key, hint))| {
                let sel = approval.selected == idx;
                let style = if sel {
                    // Black-on-yellow inversion is emphasis enough; no bold.
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::default()
                        .fg(theme::surface_fg())
                        .bg(theme::popup_bg())
                };
                let marker = if sel { "› " } else { "  " };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{}{}", marker, label), style),
                    Span::styled(
                        format!("  [{}]  ", key),
                        if sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(theme::muted_fg()).bg(theme::popup_bg())
                        },
                    ),
                    Span::styled(
                        *hint,
                        if sel {
                            Style::default().fg(Color::Black).bg(Color::Yellow)
                        } else {
                            Style::default().fg(theme::muted_fg()).bg(theme::popup_bg())
                        },
                    ),
                ]))
                .style(style)
            })
            .collect();
        f.render_widget(List::new(items), chunks[4]);
        f.render_widget(
            Paragraph::new("↑↓ navigate · Enter confirm · Esc deny · y / s / n quick")
                .style(
                    Style::default()
                        .fg(theme::muted_fg())
                        .add_modifier(Modifier::ITALIC),
                )
                .alignment(ratatui::layout::Alignment::Center),
            chunks[5],
        );
    }
}
