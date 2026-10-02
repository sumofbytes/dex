use super::super::slash;
use super::super::status::footer_line;
use super::super::status::truncate_display;
use super::super::style::fg;
use super::super::style::{composer_band, content_width, status_padding};
use super::super::App;
use super::activity::ActivityView;
use super::activity::QueueGroup;
use super::composer::ComposerView;
use super::preview::render_approval_detail;
use super::UiLayout;
use crate::render::theme;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
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
        // Sheet width matches the composer band's width, and its left edge
        // is the band's own left edge: the transcript's text column is one
        // cell inside the band, so a sheet inset any further right leaves
        // the first character of every parent row orphaned beside the
        // overlay (DEX-21). Starting on the band clears the whole column
        // and keeps the sheet's own `> ` marker in that first gutter cell,
        // so rows still copy as plain commands with the same two-cell
        // marker lead as before.
        // Inside a picker (`/model `, `/provider `, `/resume …`) rows show
        // just the item (`> gpt-5`), not the repeated command (`/model
        // <item>`) — the header already names the picker.
        let band = composer_band(area);
        let width = band.width;
        let avail = width.saturating_sub(2) as usize;
        let cmd_col = window
            .iter()
            .map(|(command, _)| UnicodeWidthStr::width(slash::suggestion_label(&input, command)))
            .max()
            .unwrap_or(0)
            .min(48)
            .min(avail.max(1));
        let popup = Rect {
            x: band.x,
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
                theme::hairline_style(),
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

/// The `ask_user` wizard overlay: one question at a time (`[n/N]`),
/// options with a cursor, the implicit "Other" row, a Submit row on
/// multiSelect questions, and the summary screen after the last question.
pub(crate) struct QuestionOverlay;

impl QuestionOverlay {
    pub(super) fn render(f: &mut ratatui::Frame, area: Rect, app: &App) {
        let Some(question) = app.pending_questions.first() else {
            return;
        };
        let extra = app.pending_questions.len().saturating_sub(1);
        let agent_prefix = question
            .agent
            .as_deref()
            .map(|agent| format!("{agent} asks: "))
            .unwrap_or_default();
        let width = area
            .width
            .saturating_sub(6)
            .clamp(52, 76)
            .min(area.width.saturating_sub(2));
        let rows: Vec<Line> = if question.summary {
            summary_lines(question)
        } else {
            let Some(current) = question.questions.get(question.current) else {
                return;
            };
            question_lines(question, current)
        };
        let counter = if question.summary {
            "summary".to_string()
        } else {
            format!("[{}/{}]", question.current + 1, question.questions.len())
        };
        let height = (rows.len() as u16 + 6).clamp(9, area.height.saturating_sub(4));
        let popup = centered(area, width, height);
        f.render_widget(Clear, popup);
        let block = Block::default()
            .title(format!(" {}Question {} ", agent_prefix, counter))
            .title_style(fg(theme::accent_fg()))
            .borders(Borders::ALL)
            .border_style(fg(theme::accent_fg()))
            .padding(Padding::new(1, 1, 1, 1))
            .style(Style::default().bg(theme::popup_bg()));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        f.render_widget(Paragraph::new(rows).wrap(Wrap { trim: false }), inner);
        if extra > 0 {
            // Queued batches behind this one.
            let note = Rect {
                y: popup.y + popup.height.saturating_sub(1),
                ..popup
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("+{extra} more question batch(es) waiting"),
                    fg(theme::muted_fg()),
                ))),
                note,
            );
        }
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn question_lines(
    question: &super::super::PendingQuestionUi,
    current: &crate::protocol::Question,
) -> Vec<Line<'static>> {
    let mut rows = vec![
        Line::from(Span::styled(
            current.question.clone(),
            fg(theme::accent_fg()),
        )),
        Line::from(Span::raw(String::new())),
    ];
    for (i, option) in current.options.iter().enumerate() {
        let selected = !current.multi_select && question.selected == i;
        let marker = if current.multi_select {
            if question.toggled[i] {
                "[x] "
            } else {
                "[ ] "
            }
        } else if selected {
            "› "
        } else {
            "  "
        };
        let default = if current.default == Some(i) {
            "  — default"
        } else {
            ""
        };
        // The cursor row inverts the terminal's own colors (max contrast on
        // any theme); a toggled `[x]` gets the accent so multi-select state
        // survives a glance.
        let marker_style = if selected {
            theme::selection_style()
        } else if current.multi_select && question.toggled[i] {
            fg(theme::accent_fg())
        } else {
            fg(theme::muted_fg())
        };
        rows.push(Line::from(vec![
            Span::styled(format!("{marker}{}", i + 1), marker_style),
            Span::raw(" "),
            Span::styled(
                option.label.clone(),
                if selected {
                    theme::selection_style()
                } else {
                    fg(theme::surface_fg())
                },
            ),
            Span::styled(default.to_string(), fg(theme::warn_fg())),
        ]));
        if !option.description.is_empty() {
            rows.push(Line::from(Span::styled(
                format!("     {}", option.description),
                fg(theme::muted_fg()),
            )));
        }
    }
    // The implicit "Other" row (display-only index: never shifts options).
    let other_selected = question.selected == current.options.len();
    if let Some(buffer) = &question.text_entry {
        rows.push(Line::from(Span::styled(
            format!("  other › {buffer}▏"),
            fg(theme::surface_fg()),
        )));
        rows.push(Line::from(Span::styled(
            "  type your answer, Enter to record, Esc to cancel",
            fg(theme::muted_fg()),
        )));
    } else {
        let marker = if other_selected { "› " } else { "  " };
        rows.push(Line::from(Span::styled(
            format!("{marker}{}) other", current.options.len() + 1),
            if other_selected {
                theme::selection_style()
            } else {
                fg(theme::muted_fg())
            },
        )));
    }
    if current.multi_select {
        let submit_selected = question.selected == current.options.len() + 1;
        let marker = if submit_selected { "› " } else { "  " };
        rows.push(Line::from(Span::styled(
            format!("{marker}submit  [Space: toggle]"),
            if submit_selected {
                theme::selection_style()
            } else {
                fg(theme::muted_fg())
            },
        )));
    }
    rows.push(Line::from(Span::raw(String::new())));
    rows.push(Line::from(Span::styled(
        "↑/↓ move · number pick · Enter record · Esc back",
        fg(theme::muted_fg()),
    )));
    rows
}

fn summary_lines(question: &super::super::PendingQuestionUi) -> Vec<Line<'static>> {
    let mut rows = vec![
        Line::from(Span::styled("Your answers", fg(theme::accent_fg()))),
        Line::from(Span::raw(String::new())),
    ];
    for (i, q) in question.questions.iter().enumerate() {
        let answer = match &question.answers[i] {
            Some(crate::protocol::QuestionAnswer::Choice(idx)) => q
                .options
                .get(*idx)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "—".to_string()),
            Some(crate::protocol::QuestionAnswer::Multi(idxs)) => idxs
                .iter()
                .filter_map(|idx| q.options.get(*idx))
                .map(|o| o.label.clone())
                .collect::<Vec<_>>()
                .join(", "),
            Some(crate::protocol::QuestionAnswer::Text(text)) => text.clone(),
            _ => "—".to_string(),
        };
        rows.push(Line::from(vec![
            Span::styled(format!("{}: ", q.header), fg(theme::muted_fg())),
            Span::styled(answer, fg(theme::surface_fg())),
        ]));
    }
    rows.push(Line::from(Span::raw(String::new())));
    rows.push(Line::from(Span::styled(
        "Enter submit · Esc back",
        fg(theme::muted_fg()),
    )));
    rows
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
                // The cursor row inverts the terminal's own colors —
                // readable on any theme; fixed `Black` on `Yellow` broke
                // when themes remapped those slots toward the background.
                let row_style = if sel {
                    theme::selection_style()
                } else {
                    Style::default()
                        .fg(theme::surface_fg())
                        .bg(theme::popup_bg())
                };
                let hint_style = if sel {
                    theme::selection_style()
                } else {
                    Style::default().fg(theme::muted_fg()).bg(theme::popup_bg())
                };
                let marker = if sel { "› " } else { "  " };
                ListItem::new(Line::from(vec![
                    // Numbered like the question wizard: 1/2/3 resolve
                    // directly, arrows just move the cursor.
                    Span::styled(format!("{}{}. {}", marker, idx + 1, label), row_style),
                    Span::styled(format!("  [{}]  ", key), hint_style),
                    Span::styled(*hint, hint_style),
                ]))
                .style(row_style)
            })
            .collect();
        f.render_widget(List::new(items), chunks[4]);
        f.render_widget(
            Paragraph::new("↑↓ navigate · Enter confirm · Esc deny · y / s / n or 1 / 2 / 3 quick")
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
