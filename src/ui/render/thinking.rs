use super::super::status::truncate_display;
use super::super::theme;
use super::transcript::wrap_line_display;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::time::Duration;

/// Render a streamed thinking block: collapsed = a single dim indicator
/// that animates "◌ Thinking .." while the block streams and settles at
/// "Thought for 4s" (or "◌ Thinking ..." when no span was measured) once it
/// closes; expanded (Ctrl+T) = the full text, dim.
/// One expanded-thinking source line: dim + transcript-indented, exactly as
/// the expanded arm of `thinking_display_lines` builds it.
fn thinking_line(s: &str) -> Line<'static> {
    let style = Style::default().fg(theme::muted_fg());
    super::super::indent_transcript_line(Line::from(Span::styled(s.to_string(), style)))
}

pub(crate) fn thinking_display_lines(
    text: &str,
    expanded: bool,
    thinking_open: bool,
    elapsed: Option<Duration>,
    tick: u16,
    width: u16,
) -> Vec<Line<'static>> {
    if expanded {
        return text
            .lines()
            .flat_map(|l| wrap_line_display(&thinking_line(l), width))
            .collect();
    }
    vec![thinking_indicator_line(thinking_open, elapsed, tick, width)]
}

/// Incremental cursor for an expanded thinking block's wrapped rows (§29):
/// bytes of `text` already reflected in the cached rows, of which the last
/// source line (`open_len` bytes → `open_rows` rows) may still be open.
#[derive(Clone, Copy)]
pub(crate) struct ThinkingWrap {
    pub(crate) src_len: usize,
    pub(crate) open_len: usize,
    pub(crate) open_rows: usize,
}

/// Full wrap of expanded thinking text plus the incremental cursor
/// describing it. The still-open line is the text after the last newline
/// (empty when the text ends with one — it contributes no rows).
pub(crate) fn wrap_thinking_full(text: &str, width: u16) -> (Vec<Line<'static>>, ThinkingWrap) {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut head_rows = 0usize;
    for l in text.lines() {
        head_rows = rows.len();
        rows.extend(wrap_line_display(&thinking_line(l), width));
    }
    let (open_len, open_rows) = if text.ends_with('\n') || text.is_empty() {
        (0, 0)
    } else {
        (
            text.rsplit('\n').next().map_or(0, str::len),
            rows.len() - head_rows,
        )
    };
    (
        rows,
        ThinkingWrap {
            src_len: text.len(),
            open_len,
            open_rows,
        },
    )
}

/// Extend cached expanded-thinking rows with newly appended text (§29).
/// Everything before the previously open source line is final, so only the
/// open line plus the tail re-wrap. Returns `None` when the cache can't be
/// reused — shorter text (head-cut, reset, rebuild) or rows built collapsed
/// — and the caller must fully re-wrap.
pub(crate) fn extend_thinking_rows(
    rows: &mut Vec<Line<'static>>,
    state: ThinkingWrap,
    text: &str,
    width: u16,
) -> Option<ThinkingWrap> {
    if state.src_len > text.len() {
        return None;
    }
    let start = state.src_len.saturating_sub(state.open_len);
    let tail = text.get(start..)?;
    rows.truncate(rows.len().saturating_sub(state.open_rows));
    let (mut tail_rows, tail_state) = wrap_thinking_full(tail, width);
    rows.append(&mut tail_rows);
    Some(ThinkingWrap {
        src_len: text.len(),
        open_len: tail_state.open_len,
        open_rows: tail_state.open_rows,
    })
}

/// The collapsed thinking indicator's text: while the block streams, the
/// dot count cycles 1→3 every other animation frame (~0.24s at the ~8 fps
/// busy heartbeat) — deliberately slower than the stream flush so the dots
/// read as a calm pulse; once the block closes it settles at "Thought for
/// <elapsed>", or falls back to the static dots when no span was measured.
pub(crate) fn thinking_indicator_text(
    thinking_open: bool,
    elapsed: Option<Duration>,
    tick: u16,
) -> String {
    if thinking_open {
        return format!("◌ Thinking {}", dots_for_tick(tick));
    }
    match elapsed {
        Some(elapsed) => format!("Thought for {}", format_elapsed(elapsed)),
        None => "◌ Thinking ...".to_string(),
    }
}

/// "4s" under a minute; minutes + seconds above.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        let mins = secs / 60;
        let rest = secs % 60;
        format!("{mins}m {rest}s")
    }
}

/// Shared dot cadence: cycles 1→3 every other animation frame (~0.24s at
/// the ~8 fps busy heartbeat) — deliberately slower than the stream flush
/// so the dots read as a calm pulse. Used by both the thinking and the
/// turn-activity indicators.
fn dots_for_tick(tick: u16) -> &'static str {
    match (tick / 2) % 3 {
        0 => ".",
        1 => "..",
        _ => "...",
    }
}

pub(crate) fn thinking_indicator_line(
    thinking_open: bool,
    elapsed: Option<Duration>,
    tick: u16,
    width: u16,
) -> Line<'static> {
    super::super::indent_transcript_line(Line::from(Span::styled(
        truncate_display(
            &thinking_indicator_text(thinking_open, elapsed, tick),
            width,
        ),
        Style::default().fg(theme::muted_fg()),
    )))
}

/// The collapsed turn-activity block: "● Working .." with the shared dot
/// cadence while the turn runs (animated by the per-frame overlay), then
/// the green "Worked for 12s · 4.2k tokens" summary once it settles.
pub(crate) fn activity_display_lines(
    settled: Option<&str>,
    tick: u16,
    width: u16,
) -> Vec<Line<'static>> {
    match settled {
        Some(summary) => vec![super::super::indent_transcript_line(Line::from(
            Span::styled(
                truncate_display(summary, width),
                Style::default().fg(Color::LightGreen),
            ),
        ))],
        None => vec![activity_indicator_line(tick, width)],
    }
}

pub(crate) fn activity_indicator_line(tick: u16, width: u16) -> Line<'static> {
    super::super::indent_transcript_line(Line::from(Span::styled(
        truncate_display(&format!("● Working {}", dots_for_tick(tick)), width),
        Style::default().fg(theme::muted_fg()),
    )))
}
