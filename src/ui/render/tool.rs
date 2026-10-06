//! Tool steps: the call (`✓ read src/a.rs:1-40`) with its outcome (`41 lines`)
//! on the row beneath. Output previews stay folded for successful
//! calls (Ctrl+O unfolds them); failures and write/edit diffs always show.

use super::super::style::fg;
use super::super::style::TRANSCRIPT_INDENT;
use super::super::ToolResult;
use super::transcript::wrap_line_display;
use crate::render::theme;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// Diff rows a collapsed write/edit shows before folding.
const COLLAPSED_DIFF_ROWS: usize = 10;
/// Calls faster than this don't print a duration; it is noise.
const SHOW_DURATION_SECS: f64 = 1.0;

/// The outcome text for a step row: the daemon's summary minus the `failed`
/// prefix the `✗` glyph already says (`failed (exit 2) · boom` → `exit 2 ·
/// boom`), plus the duration when it is worth reading.
pub(crate) fn outcome_text(result: &ToolResult) -> String {
    let mut text = result.summary.clone();
    if !result.ok {
        if let Some(rest) = text.strip_prefix("failed · ") {
            text = rest.to_string();
        } else if let Some(rest) = text.strip_prefix("failed (") {
            text = rest.replacen(')', "", 1);
        }
    }
    if result.duration >= SHOW_DURATION_SECS {
        let secs = crate::render::format::format_duration(result.duration);
        if text.is_empty() {
            text = secs;
        } else {
            text = format!("{text} · {secs}");
        }
    }
    text
}

/// Whether a finished call's preview rows show without Ctrl+O.
fn preview_shown(name: &str, ok: bool, expand: bool) -> bool {
    expand || !ok || matches!(name, "write" | "edit")
}

pub(crate) fn tool_rows(
    name: &str,
    arg: &Line<'static>,
    result: Option<&ToolResult>,
    preview: &[Line<'static>],
    width: u16,
    expand: bool,
) -> Vec<Line<'static>> {
    let (glyph, color) = match result {
        None => ("◌", theme::muted_fg()),
        Some(r) if r.ok => ("✓", theme::success_fg()),
        Some(_) => ("✗", theme::failure_fg()),
    };
    let mut spans = vec![
        Span::raw(" ".repeat(TRANSCRIPT_INDENT)),
        Span::styled(glyph, fg(color)),
        Span::raw(" "),
        Span::styled(name.to_string(), fg(theme::secondary_fg())),
    ];
    if !arg.spans.is_empty() {
        spans.push(Span::raw(" "));
        spans.extend(arg.spans.iter().cloned());
    }
    let mut rows = wrap_line_display(&Line::from(spans), width, 0);
    if let Some(result) = result {
        let text = outcome_text(result);
        if !text.is_empty() {
            let style = if result.ok {
                fg(theme::muted_fg())
            } else {
                fg(theme::failure_fg())
            };
            // Always its own row under the call, aligned with the tool name.
            let line = Line::from(vec![
                Span::raw(" ".repeat(TRANSCRIPT_INDENT)),
                Span::styled(format!("  {text}"), style),
            ]);
            rows.extend(wrap_line_display(&line, width, 0));
        }
    }
    if let Some(result) = result {
        if preview_shown(name, result.ok, expand) {
            let cap = if !expand && result.ok {
                COLLAPSED_DIFF_ROWS
            } else {
                usize::MAX
            };
            for line in preview.iter().take(cap) {
                rows.extend(wrap_line_display(line, width, 0));
            }
            if preview.len() > cap {
                let more = Line::from(vec![
                    Span::raw(" ".repeat(TRANSCRIPT_INDENT)),
                    Span::styled(
                        format!("  … +{} more diff lines · Ctrl+O", preview.len() - cap),
                        Style::default().fg(theme::muted_fg()),
                    ),
                ]);
                rows.push(more);
            }
        }
    }
    rows
}
