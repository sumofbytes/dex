use super::super::status::truncate_display;
use super::super::style::surface_padding;
use super::super::App;
use super::QueueMetrics;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;

pub(crate) struct ActivityView;

/// Queued items shown in the strip before the `+N more` tail.
const QUEUE_MAX_ITEMS: usize = 3;
/// Rows rendered per queued item — badge row plus continuation rows, with
/// overflow collapsed into a `…` row. Without the cap a large paste would
/// grow the strip past the terminal and collapse the bottom pane into
/// `compute_layout`'s transcript-only fallback.
pub(crate) const QUEUE_MAX_ITEM_ROWS: usize = 4;

/// One group of the pending-queue strip: a queued item's rows, or the
/// `+N more` tail when `badge` is `None`.
pub(crate) struct QueueGroup {
    pub(crate) badge: Option<&'static str>,
    pub(crate) lines: Vec<String>,
}

/// The pending queue as drawn: badge row plus one row per continuation
/// line per item (long items collapse into a `…` row), then the `+N more`
/// tail. Single source of truth for `pending_queue_metrics` (sizing) and
/// `ActivityView::render` (drawing) — the two must agree or multiline
/// submissions clip.
pub(crate) fn queue_groups(app: &App) -> Vec<QueueGroup> {
    let total = app.pending_steering.len() + app.pending_followups.len();
    let mut groups: Vec<QueueGroup> = app
        .pending_steering
        .iter()
        .map(|p| (true, p))
        .chain(app.pending_followups.iter().map(|p| (false, p)))
        .take(QUEUE_MAX_ITEMS)
        .map(|(is_steer, pending)| {
            let mut lines: Vec<String> = pending
                .lines()
                .take(QUEUE_MAX_ITEM_ROWS)
                .map(str::to_string)
                .collect();
            if lines.is_empty() {
                // Empty queued text still gets its badge row.
                lines.push(String::new());
            }
            // Overflow probe without the old `lines().count()` full walk:
            // one more row tells all (§29). Exact for any
            // `QUEUE_MAX_ITEM_ROWS >= 1` (a row past the window exists iff
            // the item overflows it).
            if pending.lines().nth(QUEUE_MAX_ITEM_ROWS).is_some() {
                // Never the badge row: an overflowing item keeps at least
                // one continuation line before the collapse.
                *lines.last_mut().expect("non-empty") = "…".into();
            }
            QueueGroup {
                badge: Some(if is_steer { "steer" } else { "follow-up" }),
                lines,
            }
        })
        .collect();
    if total > QUEUE_MAX_ITEMS {
        groups.push(QueueGroup {
            badge: None,
            lines: vec![format!("+{} more queued", total - QUEUE_MAX_ITEMS)],
        });
    }
    groups
}

/// Items and content rows the strip renders, straight from `queue_groups`
/// so sizing can't drift from the drawing.
pub(crate) fn queue_metrics_of(groups: &[QueueGroup]) -> QueueMetrics {
    QueueMetrics {
        items: u16::try_from(groups.len()).unwrap_or(u16::MAX),
        rows: u16::try_from(groups.iter().map(|g| g.lines.len()).sum::<usize>())
            .unwrap_or(u16::MAX),
    }
}

impl ActivityView {
    /// The queue groups come from `view` (built once per frame, §29) —
    /// rebuilding them here doubled the per-frame queue cost.
    pub(super) fn render(f: &mut ratatui::Frame, area: Rect, groups: &[QueueGroup]) {
        // Always clear the rect first: ratatui only repaints cells the
        // widget writes, so a shorter line (e.g. fewer queued-steer
        // badges) would otherwise leave trailing chars from the previous
        // frame.
        f.render_widget(Clear, area);
        // The busy "● Working" spinner and the "worked for …" summary now
        // live in the transcript as the turn-activity block; this strip
        // only carries the pending steer/follow-up queue.
        if groups.is_empty() {
            return;
        }
        let content_width = super::super::style::content_width(area.width);
        let style = Style::default().fg(Color::Yellow);
        // Blank separator between groups only — a group's continuation
        // rows sit directly under their badge.
        let mut rows: Vec<Line<'static>> = Vec::new();
        for group in groups {
            if !rows.is_empty() {
                rows.push(Line::from(String::new()));
            }
            let mut lines = group.lines.iter();
            let first = lines.next().map(String::as_str).unwrap_or("");
            let text = match group.badge {
                Some(badge) => format!("{badge} · {first}"),
                None => first.to_string(),
            };
            rows.push(Line::from(Span::styled(
                truncate_display(&text, content_width),
                style,
            )));
            rows.extend(lines.map(|rest| {
                Line::from(Span::styled(truncate_display(rest, content_width), style))
            }));
        }
        f.render_widget(
            Paragraph::new(rows).block(Block::default().padding(surface_padding())),
            area,
        );
    }
}
