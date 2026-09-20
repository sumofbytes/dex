use super::app::format_tokens;
use super::app::App;
use super::app::TranscriptBlock;
use super::app::INPUT_PROMPT;
use super::app::STREAM_FLUSH_INTERVAL;
use super::app::THINKING_TEXT_CAP;
use super::app::THINKING_TEXT_SLACK;
use super::app::TRANSCRIPT_INDENT;
use super::render;
use super::status;
use super::theme;
use crate::protocol::Role;
use crate::protocol::SinkLine;
use crate::runtime::format_runtime::agent_lifecycle;
use crate::ui::format::short_arg;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashSet;
use std::time::Instant;

pub(crate) fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

pub(crate) fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

/// True when a stored transcript line renders as air: every span is
/// whitespace. Direct-pushed blanks (`Line::default()`) carry no spans;
/// markdown-rendered blank rows carry the 1-space transcript indent.
fn line_is_air(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

pub(crate) fn push_info_line(app: &mut App, line: Line<'static>) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    app.transcript.push(TranscriptBlock::Info {
        stamp: 0,
        line: indent_transcript_line(line),
    });
    move_activity_to_tail(app);
}

pub(crate) fn push_info(app: &mut App, text: String) {
    push_info_line(
        app,
        Line::from(Span::styled(text, Style::default().fg(Color::Cyan))),
    );
}

/// Session-start banner: the "DEX" wordmark with its version in
/// parentheses. One short row, so this fits even the narrowest transcripts
/// without wrapping.
pub(crate) const BANNER: &str = concat!("DEX (v", env!("CARGO_PKG_VERSION"), ")");

/// Push the session-start banner (wordmark plus version). One `Banner` block
/// (not an Info block) so `TranscriptView` treats it as session chrome.
pub(crate) fn push_banner(app: &mut App) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    // Subtle by design: the theme-aware muted foreground instead of a bright
    // accent, so the banner reads as quiet chrome on light/dark terminals.
    let style = Style::default().fg(theme::muted_fg());
    let lines = vec![indent_transcript_line(Line::from(Span::styled(
        BANNER.to_string(),
        style,
    )))];
    app.transcript
        .push(TranscriptBlock::Banner { stamp: 0, lines });
}

/// Wall-clock throttle gate: true once `STREAM_FLUSH_INTERVAL` elapsed
/// since the last flush, so markdown re-parses stay capped when the frame
/// rate changes.
fn stream_flush_due(app: &App) -> bool {
    app.stream_last_flush.elapsed() >= STREAM_FLUSH_INTERVAL
}

fn note_stream_flush(app: &mut App) {
    app.stream_last_flush = Instant::now();
}

/// Index of the last content block, skipping any trailing open
/// turn-activity indicator. All streaming extension logic (assistant
/// coalesce, thinking deltas, tool completion, dimming, close) must use
/// this instead of `transcript.last*()` — the open `Activity` block trails
/// at the tail while a turn runs, so `last` is the spinner, not content.
fn content_tail_idx(app: &App) -> Option<usize> {
    app.transcript
        .iter()
        .rposition(|b| !matches!(b, TranscriptBlock::Activity { settled: None, .. }))
}

fn content_tail_mut(app: &mut App) -> Option<&mut TranscriptBlock> {
    let idx = content_tail_idx(app)?;
    app.transcript.get_mut(idx)
}

/// Re-wrap the open turn-activity block on thinking visibility flips. The
/// spinner hides while a thinking block streams (Working shows only when
/// busy-but-not-thinking), so its cached rows depend on `thinking_open` as
/// well as its stamp — bump it whenever thinking opens or closes.
fn bump_open_activity(app: &mut App) {
    if let Some(pos) = app
        .transcript
        .iter()
        .rposition(|b| matches!(b, TranscriptBlock::Activity { settled: None, .. }))
    {
        app.transcript[pos].bump();
    }
}

/// Drain the buffered assistant deltas into the tail `Assistant` block,
/// rendering the markdown once for the whole buffered chunk. Call before
/// anything reads the transcript or pushes a non-assistant block, so pending
/// text always lands in the block that was streaming it.
pub(crate) fn flush_assistant(app: &mut App) {
    if app.assistant_pending.is_empty() {
        return;
    }
    note_stream_flush(app);
    let new_lines: Vec<Line<'static>> = render::markdown_lines(app.assistant_pending.trim_end())
        .into_iter()
        .map(indent_transcript_line)
        .collect();
    app.assistant_pending.clear();
    if app.assistant_open {
        if let Some(TranscriptBlock::Assistant { lines, stamp }) = content_tail_mut(app) {
            lines.extend(new_lines);
            *stamp = stamp.wrapping_add(1);
            return;
        }
    }
    app.transcript.push(TranscriptBlock::Assistant {
        stamp: 0,
        lines: new_lines,
    });
    app.assistant_open = true;
    // New block landed after the trailing spinner; re-trail it so the
    // "● Working" indicator stays last during a turn. No-op when idle.
    move_activity_to_tail(app);
}

/// Drop the oldest stored thinking past [`THINKING_TEXT_CAP`] (§29). The cut
/// shifts bytes, so every wrapped block's stamp resets — the incremental
/// tail-wrap state rebuilds from scratch on the next frame instead of
/// grafting onto shifted rows. Every over-cap `Thinking` block is trimmed
/// (not just the tail — a multi-tool turn leaves older closed blocks
/// unbounded otherwise), and a cut inserts a `[truncated]` marker so the
/// expanded view never silently shows partial reasoning.
fn trim_thinking_head(app: &mut App) {
    const MARKER: &str = "[truncated]…\n";
    let mut cut = false;
    for block in &mut app.transcript {
        let TranscriptBlock::Thinking { text, .. } = block else {
            continue;
        };
        if text.len() <= THINKING_TEXT_CAP {
            continue;
        }
        let target = text.len().saturating_sub(THINKING_TEXT_CAP);
        let mut at = target;
        while at < text.len() && !text.is_char_boundary(at) {
            at += 1;
        }
        if at > 0 {
            text.drain(..at);
            if !text.starts_with("[truncated]") {
                text.insert_str(0, MARKER);
            }
            cut = true;
        }
    }
    if cut {
        for wb in &mut app.wrapped_cache {
            wb.stamp = u64::MAX;
        }
    }
}

/// `SinkLine::Assistant`: buffer streaming deltas — markdown runs once per
/// throttle window, not once per line. Each sink line is one complete markdown
/// line, so rejoin buffered lines with '\n' to keep paragraph structure.
/// Normalize blank lines around block-level markdown so dense model output
/// still renders with air between sections. Gap state survives throttled
/// flushes (which clear `assistant_pending`): without it a heading/list at a
/// window edge lost its top air while the bottom air survived via the trailing
/// blank. A blank line inside the open message keeps the tail block so the
/// inter-block gutter stays canonical (blank runs collapse to one air row).
fn append_assistant(app: &mut App, s: String) {
    if s.trim().is_empty() {
        if app.assistant_open {
            flush_assistant(app);
            if let Some(TranscriptBlock::Assistant { lines, stamp }) = content_tail_mut(app) {
                if !lines.last().is_some_and(line_is_air) {
                    lines.push(Line::default());
                    *stamp = stamp.wrapping_add(1);
                }
            }
            app.assistant_gap.note_blank();
        }
        return;
    }
    if !app.assistant_open && app.assistant_pending.is_empty() {
        app.assistant_gap.reset();
    }
    let chunk = app.assistant_gap.normalize(&s);
    if !app.assistant_pending.is_empty() && !app.assistant_pending.ends_with('\n') {
        app.assistant_pending.push('\n');
    }
    app.assistant_pending.push_str(&chunk);
    // Tables need header + delimiter + rows in one render window: markdown is
    // re-parsed per flush, so a table split across windows would fall back to
    // raw paragraphs. Hold the flush while the buffer ends on a table line;
    // prose, a blank line or the next non-assistant sink line releases it.
    let last_line = app
        .assistant_pending
        .trim_end()
        .rsplit('\n')
        .next()
        .unwrap_or("");
    let holding_table = crate::ui::theme::markdown::is_table_line(last_line);
    if stream_flush_due(app) && !holding_table {
        flush_assistant(app);
    }
}

/// `SinkLine::Thinking`: append the delta to the open thinking block (or open
/// one), trimming the head past the cap. Throttled re-wraps: reasoning arrives
/// token-by-token and the block settles on close, which bumps unconditionally.
fn append_thinking(app: &mut App, s: String) {
    if s.is_empty() {
        return;
    }
    app.assistant_open = false;
    // (Gate read before the mutable borrow; clock reset after it.)
    let due = stream_flush_due(app);
    let mut over_cap = false;
    if let Some(TranscriptBlock::Thinking { text, stamp, .. }) = content_tail_mut(app) {
        text.push_str(&s);
        over_cap = text.len() > THINKING_TEXT_CAP + THINKING_TEXT_SLACK;
        if due {
            *stamp = stamp.wrapping_add(1);
        }
    } else {
        app.transcript.push(TranscriptBlock::Thinking {
            stamp: 0,
            text: s,
            started: Instant::now(),
            elapsed: None,
        });
    }
    if over_cap {
        trim_thinking_head(app);
    }
    if due {
        note_stream_flush(app);
    }
    if !app.thinking_open {
        // Spinner hides while thinking streams; re-wrap it away.
        bump_open_activity(app);
    }
    app.thinking_open = true;
}

/// `SinkLine::ToolInput`: start a tool block (glyph line).
fn append_tool_input(app: &mut App, id: String, input: String) {
    dim_intermediate_assistant_block(app);
    app.assistant_open = false;
    let mut it = input.splitn(2, ' ');
    let name = it.next().unwrap_or("").to_string();
    let arg = it.next().unwrap_or("").to_string();
    let line = render::render_tool_input(&name, &arg);
    app.transcript.push(TranscriptBlock::Tool {
        stamp: 0,
        input: line,
        output: None,
        preview: Vec::new(),
        tool_arg: arg,
        tool_id: id,
    });
}

/// `SinkLine::ToolOutput`: complete the paired tool block (or synthesize one
/// on replay). Returns true when it completed an open block — the caller then
/// skips the tail-move, matching the historic early return.
fn append_tool_output(app: &mut App, sl: SinkLine) -> bool {
    let SinkLine::ToolOutput {
        id,
        name,
        summary,
        success,
        preview,
        duration,
    } = sl
    else {
        return false;
    };
    // The glyph line above already names the tool; the └ line leads with the
    // outcome (glyph + summary) and trails timing in dim.
    let failed = !success;
    let color = if failed {
        Color::LightRed
    } else {
        Color::LightGreen
    };
    let mut spans = vec![
        Span::styled("└ ", Style::default().fg(color)),
        Span::styled(if failed { "✗ " } else { "✓ " }, Style::default().fg(color)),
        Span::styled(summary, Style::default().fg(color)),
    ];
    if duration > 0.0 {
        spans.push(Span::styled(
            format!(" · {}", crate::ui::format::format_duration(duration)),
            Style::default().fg(theme::muted_fg()),
        ));
    }
    let output = indent_transcript_line(Line::from(spans));
    // Pair with the open block for this call: parallel batches interleave
    // inputs/outputs, so the tail is not necessarily ours. Empty id = legacy
    // (session rebuild, old journals, shell blocks): keep the old tail
    // behavior. (Content tail: the open Activity spinner may sit at the
    // transcript tail while busy; Activity blocks never match the scan below,
    // so they stay transparent in both paths.)
    let open_idx = if id.is_empty() {
        content_tail_idx(app).filter(|&i| {
            matches!(
                app.transcript.get(i),
                Some(TranscriptBlock::Tool { output: None, .. })
            )
        })
    } else {
        app.transcript.iter().rposition(
            |b| matches!(b, TranscriptBlock::Tool { tool_id, output: None, .. } if tool_id == &id),
        )
    };
    // write/edit previews are a git diff: color like git does. read previews
    // keep the numbered gutter dim and highlight the code by extension (one
    // tree-sitter pass per file section); anything unhighlightable stays dim.
    let preview_lines: Vec<Line<'static>> = if matches!(name.as_str(), "write" | "edit") {
        preview
            .iter()
            .map(|line| {
                let style = if line.starts_with('+') && !line.starts_with("+++") {
                    Style::default().fg(Color::LightGreen)
                } else if line.starts_with('-') && !line.starts_with("---") {
                    Style::default().fg(Color::LightRed)
                } else if line.starts_with("@@") {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(theme::tool_preview_fg())
                };
                indent_transcript_line(Line::from(Span::styled(format!("  {line}"), style)))
            })
            .collect()
    } else if matches!(name.as_str(), "read") && success {
        // Language from the stored `ToolInput` arg (first token is the path:
        // `src/main.rs:1-20`, a glob, or `N files`). `==> file <==` fan-out
        // headers inside re-target per section in `render_read_preview`. Read
        // from the block this output will complete (not the tail: parallel
        // batches interleave, so the tail may be another call's block).
        let arg_path = open_idx
            .and_then(|i| match app.transcript.get(i) {
                Some(TranscriptBlock::Tool { tool_arg, .. }) => Some(tool_arg.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let path = arg_path.split_whitespace().next().unwrap_or("");
        let path = path.split(':').next().unwrap_or(path);
        render::render_read_preview(&preview, crate::ui::theme::highlight::lang_from_path(path))
    } else if success && matches!(name.as_str(), "grep" | "ffgrep") {
        // Content-mode hits are `path:line:code` rows: keep the gutter dim,
        // highlight the code by path extension (same engine and dim fallback
        // as read previews).
        render::render_search_preview(&preview)
    } else {
        preview
            .iter()
            .map(|line| {
                indent_transcript_line(Line::from(Span::styled(
                    format!("  {line}"),
                    Style::default().fg(theme::tool_preview_fg()),
                )))
            })
            .collect()
    };
    app.assistant_open = false;
    // Complete the tool block started by ToolInput if it is still open.
    if let Some(i) = open_idx {
        if let Some(TranscriptBlock::Tool {
            output: out,
            preview: prev,
            stamp,
            ..
        }) = app.transcript.get_mut(i)
        {
            if out.is_none() {
                *out = Some(output);
                *prev = preview_lines;
                *stamp = stamp.wrapping_add(1);
                // A mid-transcript completion shifts every display row below
                // it: drop a live selection reaching past it rather than
                // highlight/copy shifted rows.
                drop_shifted_selection(app, i);
                return true;
            }
        }
    }
    // Fallback: no open ToolInput (e.g. replay); synthesize a block. No arg is
    // known here, so previews stay dim — the replay path
    // (`rebuild_transcript`) always emits `ToolInput` first, which carries the
    // arg for highlighting.
    app.transcript.push(TranscriptBlock::Tool {
        stamp: 0,
        input: indent_transcript_line(Line::from(Span::styled(
            "▸ tool",
            Style::default().fg(Color::Yellow),
        ))),
        output: Some(output),
        preview: preview_lines,
        tool_arg: String::new(),
        tool_id: id,
    });
    false
}

/// `SinkLine::System`: a muted system note; child-agent lifecycle lines
/// (`[agent <name>:<id>] started|finished …`) get their own bold green glyph so
/// a delegation pops out of the muted notes, like per-tool glyphs do.
fn append_system(app: &mut App, s: String) {
    app.assistant_open = false;
    let line = match agent_lifecycle(&s) {
        Some((glyph, _)) => indent_transcript_line(Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(Color::Green)),
            Span::styled(s, Style::default().fg(Color::Green)),
        ])),
        None => indent_transcript_line(Line::from(vec![
            Span::styled("· ", Style::default().fg(theme::muted_fg())),
            Span::styled(s, Style::default().fg(theme::muted_fg())),
        ])),
    };
    app.transcript
        .push(TranscriptBlock::System { stamp: 0, line });
}

fn append_error(app: &mut App, s: String) {
    app.assistant_open = false;
    app.transcript.push(TranscriptBlock::Error {
        stamp: 0,
        line: indent_transcript_line(Line::from(vec![
            Span::styled("! ", Style::default().fg(Color::Red)),
            Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
        ])),
    });
}

/// Route a streamed console line into the transcript with the same styling
/// the local engine uses, so remote and local turns look identical.
/// Each `SinkLine` maps to one `TranscriptBlock` (or an extension of the
/// tail `Assistant` block while streaming). No empty gap `Line`s are stored;
/// `TranscriptView` inserts a single blank `Line` between any two blocks.
pub(crate) fn append_sink_line(app: &mut App, sl: SinkLine) {
    // Anything other than a thinking delta closes the open thinking block.
    // Non-assistant lines first drain the pending assistant buffer so it
    // lands in the block that was streaming it; assistant deltas drain in
    // their own arm.
    if !matches!(sl, SinkLine::Assistant(_)) {
        flush_assistant(app);
    }
    if !matches!(sl, SinkLine::Thinking(_)) {
        close_thinking(app);
    }
    match sl {
        SinkLine::Assistant(s) => append_assistant(app, s),
        SinkLine::Thinking(s) => append_thinking(app, s),
        SinkLine::ToolInput { id, input } => append_tool_input(app, id, input),
        SinkLine::ToolOutput { .. } => {
            if append_tool_output(app, sl) {
                return;
            }
        }
        SinkLine::System(s) => append_system(app, s),
        // Usage updates flow into the status bar via StreamEvent::Usage in
        // the remote handler, not into the transcript.
        SinkLine::Usage { .. } => {}
        SinkLine::Plan(plan) => {
            app.plan = plan;
        }
        SinkLine::Error(s) => append_error(app, s),
    }
    // ponytail: sticky autoscroll — don't force true on every append;
    // TranscriptView snaps only when already at bottom, so manual scroll
    // during streaming stays put instead of snapping back each chunk.
    move_activity_to_tail(app);
}

/// Close the streaming thinking block, if any: stops the collapsed
/// indicator's dot animation. The per-block display cache holds the settled
/// "◌ Thinking ..." row already (the dots are a per-frame overlay), but the
/// stamp bump forces a re-wrap so an expanded (Ctrl+T) block shows any text
/// that arrived since the last throttled bump.
pub(crate) fn close_thinking(app: &mut App) {
    if app.thinking_open {
        app.thinking_open = false;
        // Working reappears once thinking settles; re-wrap the spinner.
        bump_open_activity(app);
        // Stamp bump forces a re-wrap so the settled row shows the elapsed
        // "Thought for …" (and an expanded Ctrl+T block shows any text that
        // arrived since the last throttled bump). Content tail: the open
        // Activity spinner may sit at the transcript tail while busy.
        if let Some(TranscriptBlock::Thinking {
            started,
            elapsed,
            stamp,
            ..
        }) = content_tail_mut(app)
        {
            *elapsed = Some(started.elapsed());
            *stamp = stamp.wrapping_add(1);
        }
    }
}

/// Push the live turn-activity block: an animated "● Working" indicator
/// that trails the latest transcript block while the turn runs and settles
/// into the "Worked for …" summary when it ends.
pub(crate) fn start_activity(app: &mut App) {
    if app
        .transcript
        .iter()
        .any(|b| matches!(b, TranscriptBlock::Activity { settled: None, .. }))
    {
        return;
    }
    app.transcript.push(TranscriptBlock::Activity {
        stamp: 0,
        started: Instant::now(),
        settled: None,
    });
    app.autoscroll = true;
}

/// Clear the selection when rows at or below block `pos` no longer map to
/// the same display rows (the block moved past them). A selection entirely
/// above `pos` keeps pointing at unchanged rows, so it survives the
/// streaming appends that re-trail the activity spinner.
fn drop_shifted_selection(app: &mut App, pos: usize) {
    // Start row of block `pos` in display space: cached rows plus the
    // 1-row separator before each non-empty block after the first.
    let first_shifted: usize = app
        .wrapped_cache
        .iter()
        .take(pos)
        .enumerate()
        .map(|(j, wb)| wb.rows.len() + usize::from(j > 0 && !wb.rows.is_empty()))
        .sum();
    if let Some(sel) = app.selection {
        if sel.norm().1 .0 >= first_shifted {
            app.selection = None;
        }
    }
}

/// Move the open turn-activity block to the transcript tail so the animated
/// "● Working" indicator always sits under the newest block. Called after
/// every busy-time append; no-op without a running turn (settled blocks
/// from finished turns stay where they settled).
pub(crate) fn move_activity_to_tail(app: &mut App) {
    if !app.busy {
        return;
    }
    if matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ) {
        return;
    }
    let Some(pos) = app
        .transcript
        .iter()
        .rposition(|b| matches!(b, TranscriptBlock::Activity { settled: None, .. }))
    else {
        return;
    };
    let block = app.transcript.remove(pos);
    app.transcript.push(block);
    // Wrapped rows after `pos` shifted up a slot; drop their (tail few)
    // cache entries — TranscriptView re-wraps them.
    app.wrapped_cache.truncate(pos);
    drop_shifted_selection(app, pos);
}

/// Settle the open turn-activity block: move it to the transcript tail and
/// swap the animated "● Working" indicator for the turn's summary —
/// "Worked for 12s · 4.2k tokens". Duration is measured from the block's
/// start, so it spans the whole turn (thinking included). Token count is
/// read only once a block to settle exists, so replayed turns that never
/// saw a turn start skip the estimate entirely.
pub(crate) fn settle_activity(app: &mut App) {
    let Some(pos) = app
        .transcript
        .iter()
        .rposition(|b| matches!(b, TranscriptBlock::Activity { settled: None, .. }))
    else {
        return;
    };
    let started = match &app.transcript[pos] {
        TranscriptBlock::Activity { started, .. } => *started,
        _ => unreachable!(),
    };
    let tokens = status::status_tokens(app);
    app.transcript.remove(pos);
    app.transcript.push(TranscriptBlock::Activity {
        stamp: 0,
        started,
        settled: Some(format!(
            "Worked for {} · {} tokens",
            render::format_elapsed(started.elapsed()),
            format_tokens(tokens),
        )),
    });
    app.wrapped_cache.truncate(pos);
    drop_shifted_selection(app, pos);
}

/// Ctrl+T toggles expanded thinking. The wrapped rows of a thinking block
/// depend on that flag, so stamp-bump every thinking block to force a re-wrap.
pub(crate) fn bump_thinking_stamps(app: &mut App) {
    for block in &mut app.transcript {
        if matches!(block, TranscriptBlock::Thinking { .. }) {
            block.bump();
        }
    }
}

fn dim_intermediate_assistant_block(app: &mut App) {
    if let Some(TranscriptBlock::Assistant { lines, stamp }) = content_tail_mut(app) {
        for line in lines.iter_mut() {
            for span in &mut line.spans {
                span.style = span.style.fg(theme::muted_fg());
            }
        }
        *stamp = stamp.wrapping_add(1);
    }
}

/// Send the user's approval decision for the front pending approval, then
/// reveal the next queued one (V1b: child agents can park several).
pub(crate) fn resolve_approval(app: &mut App, decision: crate::protocol::ApprovalDecision) {
    if !app.pending_approvals.is_empty() {
        let approval = app.pending_approvals.remove(0);
        let _ = approval.response.try_send(decision);
    }
}

/// Deny every queued approval (cancel/quit path): the agent threads unwind
/// instead of waiting on prompts nobody will answer.
pub(crate) fn deny_all_approvals(app: &mut App) {
    for approval in app.pending_approvals.drain(..) {
        let _ = approval
            .response
            .try_send(crate::protocol::ApprovalDecision::Deny);
    }
}

pub(crate) fn scroll_transcript(app: &mut App, delta: i32) {
    app.autoscroll = false;
    app.scroll = (app.scroll as i32)
        .saturating_add(delta)
        .clamp(0, u16::MAX as i32) as u16;
}

/// Render the user's submitted prompt with the shared transcript grid.
/// The words keep the user's signature voice color from the composer, and
/// the composer's `❯ ` glyph is echoed on the first row, so your turns
/// keep the shape they had while typed.
/// No empty gap `Line`s are stored; gutter is inserted by `TranscriptView`.
pub(crate) fn render_user_prompt(app: &mut App, line: &str) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    // No background is stored here: `wrap_block` pads every row to the
    // full width plus `INPUT_PAD_Y` air at wrap time.
    let user_style = Style::default().fg(theme::user_fg());
    let mut block_lines = Vec::new();
    for (i, sub) in line.split('\n').enumerate() {
        let mut l = Line::from(Span::styled(sub.to_string(), user_style));
        if i == 0 {
            // The glyph is echoed on the first row so submitted and typed
            // share one shape.
            l.spans
                .insert(0, Span::styled(INPUT_PROMPT.to_string(), user_style));
        }
        block_lines.push(indent_transcript_line(l));
    }
    app.transcript.push(TranscriptBlock::User {
        stamp: 0,
        lines: block_lines,
    });
    move_activity_to_tail(app);
    app.autoscroll = true;
}

pub(crate) fn rebuild_transcript(app: &mut App) {
    app.transcript.clear();
    // See `reset_session_state`: stamps restart at 0, so cached rows must go.
    app.wrapped_cache.clear();
    app.display_cache.clear();
    app.selection = None;
    app.assistant_pending.clear();
    app.assistant_gap.reset();
    app.assistant_open = false;
    app.thinking_open = false;
    // The message vec moves out instead of cloning: none of the render
    // paths below touch `app.messages` (streaming lands in transcript
    // blocks), so the restore at the end is exact (§1).
    let msgs = std::mem::take(&mut app.messages);
    // Tool ids with an open `Tool` block, maintained as blocks are emitted
    // (§1): the transcript starts empty, so an id is open exactly when this
    // loop emitted its `ToolInput` without the matching `ToolOutput` yet —
    // no linear block scan per tool message (was O(tools × blocks)).
    let mut opened: HashSet<&str> = HashSet::new();
    if msgs.len() > 1 {
        // Skip the leading system message (never rendered).
        render_message_slice(app, &msgs[1..], &mut opened);
    }
    app.messages = msgs;
    // The last replayed message may be assistant text still sitting in the
    // delta buffer; drain it so the rebuilt transcript is complete.
    flush_assistant(app);
    app.autoscroll = true;
}

/// Render one slice of session messages into transcript blocks (§1, excludes
/// the leading system message): the startup replay renders head→tail in
/// chunks with a paint between (progressive backfill), threading `opened`
/// across chunks so the final transcript is byte-identical to one-shot
/// `rebuild_transcript`. No flushing here — the caller flushes once at the
/// end, so assistant text spanning a chunk seam coalesces exactly like
/// one-shot.
pub(crate) fn render_message_slice<'m>(
    app: &mut App,
    msgs: &'m [crate::protocol::ChatMessage],
    opened: &mut HashSet<&'m str>,
) {
    for msg in msgs {
        match msg.role {
            Role::User => {
                if let Some(content) = &msg.content {
                    if !content.trim().is_empty() {
                        render_user_prompt(app, content);
                    }
                }
            }
            Role::Assistant => {
                if let Some(content) = &msg.content {
                    if !content.trim().is_empty() {
                        append_sink_line(
                            app,
                            crate::protocol::SinkLine::Assistant(content.clone()),
                        );
                    }
                }
                if let Some(calls) = &msg.tool_calls {
                    for tc in calls {
                        opened.insert(tc.id.as_str());
                        let input = format!(
                            "{} {}",
                            tc.function.name,
                            short_arg(&tc.function.name, &tc.function.arguments)
                        );
                        append_sink_line(
                            app,
                            crate::protocol::SinkLine::ToolInput {
                                id: tc.id.clone(),
                                input,
                            },
                        );
                    }
                }
            }
            Role::Tool => {
                let name = msg.name.clone().unwrap_or_else(|| "tool".to_string());
                let content = msg.content.clone().unwrap_or_default();
                let mut lines = content.lines();
                let summary = lines.next().unwrap_or("").to_string();
                let preview: Vec<String> = lines.take(6).map(|s| s.to_string()).collect();
                // Replay has no ToolInput (args live in the assistant call,
                // not the tool message). When the assistant call above
                // already opened this id's block, emit only the output so
                // it pairs with that block; otherwise emit a bare input so
                // the output attaches to a real tool block instead of the
                // synthesized `▸ tool` fallback. Search previews highlight
                // from the inline `path:line:` gutters, so they work
                // without an arg. Both carry the stored call id so the
                // output pairs with this block even when replayed messages
                // interleave.
                let tool_id = msg.tool_call_id.as_deref().unwrap_or_default();
                let already_open = !tool_id.is_empty() && opened.contains(tool_id);
                if already_open {
                    opened.remove(tool_id);
                } else {
                    append_sink_line(
                        app,
                        crate::protocol::SinkLine::ToolInput {
                            id: tool_id.to_string(),
                            input: name.clone(),
                        },
                    );
                }
                append_sink_line(
                    app,
                    crate::protocol::SinkLine::ToolOutput {
                        id: tool_id.to_string(),
                        name,
                        summary,
                        success: true,
                        preview,
                        duration: 0.0,
                    },
                );
            }
            Role::System => {}
        }
    }
}
