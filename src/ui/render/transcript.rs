use super::super::style::INPUT_PROMPT_WIDTH;
use super::super::transcript_indent;
use super::super::App;
use super::super::Selection;
use super::super::WrappedBlock;
use super::super::TAB_WIDTH;
use super::super::TRANSCRIPT_INDENT;
use super::thinking::activity_display_lines;
use super::thinking::activity_indicator_line;
use super::thinking::extend_thinking_rows;
use super::thinking::thinking_display_lines;
use super::thinking::thinking_indicator_line;
use super::thinking::wrap_thinking_full;
use super::thinking::ThinkingWrap;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthChar;

pub(crate) struct TranscriptView;

/// Wrapped rows for a transcript block at `width`. Thinking and activity
/// blocks are cached in their settled form — collapsed indicator, duration
/// summary or full dim text when expanded — so the live dots stay a
/// per-frame overlay and never trigger a re-wrap themselves. An open
/// turn-activity block wraps to zero rows while a thinking block streams:
/// Working shows only when busy-but-not-thinking, so the transcript never
/// stacks two live spinners.
/// Submitted prompts read as the composer's echo: the same `❯ ` glyph and
/// the same top/bottom air (`INPUT_PAD_Y`) as the live composer, on the
/// terminal's own background, so a sent prompt keeps the height and shape
/// it had while typed. Stored lines stay unpadded (width-dependent fill
/// happens here at wrap time, keeping `wrap_line_display`'s indent logic
/// intact); each wrapped row is padded out to the full width, with blank
/// rows above and below the content.
fn paint_surface_row(mut row: Line<'static>, width: usize, bg: Color) -> Line<'static> {
    // `Line` renders each span as `line.style.patch(span.style)`, so one
    // line-level bg covers every span that doesn't set its own — no need to
    // stomp span styles (which would clobber future span-level bgs).
    row.style.bg = Some(bg);
    let w = row.width();
    if w < width {
        row.spans
            .push(Span::styled(" ".repeat(width - w), Style::default().bg(bg)));
    }
    row
}

fn surface_pad_row(width: usize, bg: Color) -> Line<'static> {
    paint_surface_row(Line::default(), width, bg)
}

/// Wrap + pad a surface's stored lines: every row padded out to the full
/// width in `bg`, with `pad` blank air rows above and below the content.
/// `first_row_overhang` narrows the first row (the echoed prompt glyph);
/// see `wrap_line_display`.
/// Single source for the user-prompt and tool-step arms of `wrap_block`,
/// which differ only in pad count and overhang.
fn surface_rows(
    lines: impl IntoIterator<Item = Line<'static>>,
    width: u16,
    pad: usize,
    first_row_overhang: usize,
) -> Vec<Line<'static>> {
    let bg = Color::Reset;
    let w = width.max(1) as usize;
    let mut rows: Vec<Line<'static>> = lines
        .into_iter()
        .flat_map(|l| wrap_line_display(&l, width, first_row_overhang))
        .map(|r| paint_surface_row(r, w, bg))
        .collect();
    for _ in 0..pad {
        rows.insert(0, surface_pad_row(w, bg));
        rows.push(surface_pad_row(w, bg));
    }
    rows
}

pub(crate) fn wrap_block(
    block: &super::super::TranscriptBlock,
    width: u16,
    show_thinking: bool,
    thinking_open: bool,
) -> Vec<Line<'static>> {
    match block {
        super::super::TranscriptBlock::User { lines, .. } => {
            // No band: the prompt keeps the terminal's own background and is
            // framed by the composer's hairline rules, not a shaded strip.
            // `INPUT_PAD_Y` air matches the live composer's shape, and the
            // first row wraps `INPUT_PROMPT_WIDTH` narrower for the echoed
            // glyph — the same wrap the live composer applies, so typed and
            // submitted prompts reflow identically.
            surface_rows(
                lines.clone(),
                width,
                super::super::INPUT_PAD_Y as usize,
                INPUT_PROMPT_WIDTH,
            )
        }
        super::super::TranscriptBlock::Tool { .. } => {
            // Each tool step carries its own air row top/bottom so the
            // content clears the edge; gaps between steps stay terminal bg.
            surface_rows(block.lines().into_iter().cloned(), width, 1, 0)
        }
        super::super::TranscriptBlock::Thinking { text, elapsed, .. } => {
            if show_thinking {
                thinking_display_lines(text, true, false, None, 0, width)
            } else {
                thinking_display_lines(text, false, false, *elapsed, 0, width)
            }
        }
        super::super::TranscriptBlock::Activity { settled, .. } => {
            if settled.is_none() && thinking_open {
                Vec::new()
            } else {
                activity_display_lines(settled.as_deref(), 0, width)
            }
        }
        _ => block
            .lines()
            .into_iter()
            .flat_map(|l| wrap_line_display(l, width, 0))
            .collect(),
    }
}

/// Keep `wrapped_cache` parallel to the transcript: width changes clear both
/// caches, a shorter transcript (reset/resume) drops stale entries, appended
/// blocks start unwrapped. Every transcript mutation must extend/truncate the
/// cache alongside (or clear both, like `reset_session_state`): a missed site
/// serves stale rows with no other signal, so the drift guard lives here.
fn sync_wrapped_cache(app: &mut App, area: Rect, mark: &mut impl FnMut(usize)) {
    if app.wrapped_width != area.width {
        app.wrapped_cache.clear();
        app.display_cache.clear();
        app.wrapped_width = area.width;
        // Every row is re-wrapped at the new width, so the old selection's row
        // coordinates (and the text they'd copy) are gone too.
        app.selection = None;
        mark(0);
    }
    if app.wrapped_cache.len() > app.transcript.len() {
        app.wrapped_cache.truncate(app.transcript.len());
        // Selection rows refer to the old cache; drop them rather than
        // highlight or copy rows that no longer exist.
        app.selection = None;
        mark(app.transcript.len());
    }
    while app.wrapped_cache.len() < app.transcript.len() {
        mark(app.wrapped_cache.len());
        app.wrapped_cache.push(WrappedBlock {
            stamp: u64::MAX,
            rows: Vec::new(),
            src_len: 0,
            open_len: 0,
            open_rows: 0,
            expanded: false,
        });
    }
    // The tail-append path only ever pushes, so this holds on entry to the
    // wrap loop.
    debug_assert_eq!(
        app.wrapped_cache.len(),
        app.transcript.len(),
        "wrapped_cache drifted from transcript — new mutation site missed the parallel cache"
    );
}

/// Re-wrap every block whose content stamp changed — usually only the tail —
/// and report each via `mark`. Expanded thinking re-wraps only the appended
/// tail (§29): stored text is append-only below the cap, so rows before the
/// last source line are final. A head-cut, reset, or rebuild resets stamps and
/// takes the full wrap.
fn wrap_dirty_blocks(app: &mut App, area: Rect, mark: &mut impl FnMut(usize)) {
    for (idx, block) in app.transcript.iter().enumerate() {
        if app.wrapped_cache[idx].stamp == block.stamp() {
            continue;
        }
        if let super::super::TranscriptBlock::Thinking { text, .. } = block {
            if app.show_thinking {
                let stamp = block.stamp();
                let width = area.width;
                let wb = &mut app.wrapped_cache[idx];
                if wb.expanded {
                    let state = ThinkingWrap {
                        src_len: wb.src_len,
                        open_len: wb.open_len,
                        open_rows: wb.open_rows,
                    };
                    if let Some(next) = extend_thinking_rows(&mut wb.rows, state, text, width) {
                        wb.stamp = stamp;
                        wb.src_len = next.src_len;
                        wb.open_len = next.open_len;
                        wb.open_rows = next.open_rows;
                        mark(idx);
                        continue;
                    }
                }
                let (rows, state) = wrap_thinking_full(text, width);
                *wb = WrappedBlock {
                    stamp,
                    rows,
                    src_len: state.src_len,
                    open_len: state.open_len,
                    open_rows: state.open_rows,
                    expanded: true,
                };
                mark(idx);
                continue;
            }
        }
        let rows = wrap_block(block, area.width, app.show_thinking, app.thinking_open);
        app.wrapped_cache[idx] = WrappedBlock {
            stamp: block.stamp(),
            rows,
            src_len: 0,
            open_len: 0,
            open_rows: 0,
            expanded: false,
        };
        mark(idx);
    }
}

/// Re-extend `display_cache` from the first dirty block: truncate to that
/// block's start offset (gap separators + wrapped-row counts — length
/// arithmetic, no clones), then re-extend from there. Unchanged leading blocks
/// keep byte-identical rows, so the offsets line up; this runs only on content
/// or width changes, never for scroll. Tool steps carry their own air
/// rows, so every gap between blocks stays blank terminal bg.
fn rebuild_display_cache(app: &mut App, first_dirty: Option<usize>) {
    let Some(dirty) = first_dirty else {
        return;
    };
    let mut start = 0usize;
    for (idx, wb) in app.wrapped_cache.iter().enumerate().take(dirty) {
        if idx > 0 && !wb.rows.is_empty() {
            start += 1;
        }
        start += wb.rows.len();
    }
    app.display_cache.truncate(start);
    for (idx, wb) in app.wrapped_cache.iter().enumerate().skip(dirty) {
        if idx > 0 && !wb.rows.is_empty() {
            app.display_cache.push(Line::default());
        }
        app.display_cache.extend(wb.rows.iter().cloned());
    }
}

impl TranscriptView {
    pub(crate) fn render(f: &mut ratatui::Frame, area: Rect, app: &mut App) {
        // Remember where the transcript lives so mouse events can be
        // translated into display rows between frames.
        app.transcript_area = Some(area);
        // Clear the transcript area first: without this a shorter frame (e.g. after
        // a long wrapped line scrolls out, or after a resize that re-wraps to fewer
        // rows) would leave trailing cells from the previous Paragraph. The top-level
        // Clear in `view` covers the whole screen once per frame, but Paragraph only
        // writes its own cells — any row that was previously occupied and is now empty
        // would otherwise persist as a ghost until the next full clear (resize).
        f.render_widget(Clear, area);
        let visible = area.height as usize;
        // ponytail: per-block wrap cache — transcript blocks are append-only,
        // so a streaming flush re-wraps only the blocks whose content stamp
        // changed (usually the tail) instead of the whole transcript. Scroll
        // and resize reuse cached rows; only the visible window is cloned
        // into the paragraph each draw.
        // First block whose display rows may have changed (perf doc §29):
        // the concat below re-extends from here instead of re-cloning the
        // whole transcript per streaming flush.
        let mut first_dirty: Option<usize> = None;
        let mut mark = |idx: usize| {
            first_dirty = Some(first_dirty.map_or(idx, |first| first.min(idx)));
        };
        sync_wrapped_cache(app, area, &mut mark);
        wrap_dirty_blocks(app, area, &mut mark);
        rebuild_display_cache(app, first_dirty);
        // The open thinking / activity rows animate: their lines are overlaid
        // on the rendered window (not written back into `display_cache`), so
        // a later `first_dirty` re-extend from `wrapped_cache` can't resurrect
        // a stale spinner tick. Both animated blocks sit at the transcript
        // tail — the open activity block is the tail block while busy, and the
        // open thinking block is the last content block (any non-thinking sink
        // line closes it) — so their display rows derive from the tail
        // instead of walking the cache.
        let mut thinking_row: Option<usize> = None;
        let mut activity_row: Option<usize> = None;
        if (app.thinking_open && !app.show_thinking) || app.busy {
            if let Some(tail) = app.transcript.len().checked_sub(1) {
                let tail_open = matches!(
                    &app.transcript[tail],
                    super::super::TranscriptBlock::Activity { settled: None, .. }
                );
                let tail_rows = if tail_open {
                    app.wrapped_cache[tail].rows.len()
                } else {
                    0
                };
                if tail_open && tail_rows > 0 {
                    // Tail block: no separator after it, so its last row is
                    // the last display row.
                    activity_row = Some(app.display_cache.len() - 1);
                }
                if app.thinking_open && !app.show_thinking {
                    if let Some(idx) = tail.checked_sub(usize::from(tail_open)) {
                        if matches!(
                            &app.transcript[idx],
                            super::super::TranscriptBlock::Thinking { .. }
                        ) {
                            let rows = app.wrapped_cache[idx].rows.len();
                            if rows > 0 {
                                // Rows after the thinking block: only the
                                // open activity (0 rows while thinking
                                // streams) plus its 1-row separator when
                                // non-empty.
                                let sep = usize::from(tail_rows > 0);
                                thinking_row = Some(app.display_cache.len() - tail_rows - sep - 1);
                            }
                        }
                    }
                }
            }
        }
        let total = app.display_cache.len();
        let max_scroll = (total.saturating_sub(visible)) as u16;
        if app.autoscroll {
            app.scroll = max_scroll;
        } else {
            app.scroll = app.scroll.min(max_scroll);
            if app.scroll >= max_scroll {
                app.autoscroll = true;
            }
        }

        // No `.wrap(Wrap)` here: the display cache is already pre-wrapped to
        // `area.width` by `wrap_line_display`, and ratatui 0.29's WordWrapper
        // emits a phantom empty row before any all-whitespace line that is
        // exactly `area.width` wide, shifting every row below it down by one.
        // ponytail: clone only the visible window; a full-transcript clone
        // per frame was the remaining O(N) term once wrapping was cached.
        let mut window: Vec<Line<'static>> = app
            .display_cache
            .iter()
            .skip(app.scroll as usize)
            .take(visible)
            .cloned()
            .collect();
        // Animated overlay on the window copy (O(1)): `display_cache` keeps
        // the un-animated rows so cache re-extends never resurrect a stale tick.
        let scroll = app.scroll as usize;
        if let Some(row) = thinking_row {
            if row >= scroll && row - scroll < window.len() {
                window[row - scroll] = thinking_indicator_line(true, None, app.tick, area.width);
            }
        }
        if let Some(row) = activity_row {
            if row >= scroll && row - scroll < window.len() {
                window[row - scroll] = activity_indicator_line(app.tick, area.width);
            }
        }
        if let Some(sel) = app.selection {
            apply_selection(&mut window, app.scroll as usize, sel, area.width);
        }
        let transcript = Paragraph::new(window).style(Style::default().fg(Color::Gray));
        f.render_widget(transcript, area);
    }
}

pub(crate) const SEL_BG: Color = Color::Indexed(24);

/// True when a display line has no visible text (empty or whitespace-only).
/// Such an end row has no (visible) text range to highlight, so the caller
/// pads it to a bar to keep its inclusion in the selection visible.
fn is_blank_line(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .flat_map(|s| s.content.chars())
        .all(|c| c.is_whitespace())
}

/// Paint the mouse selection onto the visible window rows. Fully covered
/// rows become a solid bar (style patch + padding to the area width); the
/// first row of a multi-line drag (normed top, regardless of drag direction)
/// highlights its text range plus the trailing margin out to the edge
/// (continuation cue, no line-style patch so the prefix before the anchor
/// stays plain); the end row — and any single-row selection — highlights
/// only the selected cell range, end cell inclusive, never padding, so a
/// drag ending on the last char stays distinct from a whole-line pick.
/// Whole-line (triple-click) selections paint every covered row as a solid
/// bar. When a row's text already fills the area width there is no margin
/// left, so a last-char drag on that row reads the same as a whole-line
/// pick — unavoidable with a single highlight style.
pub(crate) fn apply_selection(
    window: &mut [Line<'static>],
    scroll: usize,
    sel: Selection,
    width: u16,
) {
    let ((r0, c0), (r1, c1)) = sel.norm();
    let hl = Style::default().bg(SEL_BG);
    let single = r0 == r1;
    for (i, line) in window.iter_mut().enumerate() {
        let row = scroll + i;
        if row < r0 || row > r1 {
            continue;
        }
        let inner = row > r0 && row < r1;
        let line_sel = sel.whole_line && row >= r0 && row <= r1;
        if inner || line_sel {
            line.style = line.style.patch(hl);
            pad_row(line, width, hl);
            continue;
        }
        let (from, to) = if single {
            (c0, c1 + 1)
        } else if row == r0 {
            (c0, usize::MAX)
        } else {
            (0, c1 + 1)
        };
        style_row_range(line, from, to, hl);
        if !single && row == r0 {
            // First (top) row continues onto the next rows: extend the highlight
            // through the trailing margin. No line-style patch — that would
            // also paint the unselected prefix before the anchor.
            pad_row(line, width, hl);
        } else if !single && row == r1 && is_blank_line(line) {
            // A blank or whitespace-only last row has no (visible) text range
            // to highlight; pad so its inclusion in the selection stays visible.
            pad_row(line, width, hl);
        }
    }
}

/// Highlight cell range `[from, to)` of a row, splitting spans as needed.
fn style_row_range(line: &mut Line<'static>, from: usize, to: usize, hl: Style) {
    if from >= to || line.spans.is_empty() {
        return;
    }
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut pos = 0usize;
    for span in std::mem::take(&mut line.spans) {
        let graphemes: Vec<(String, Style)> = span
            .styled_graphemes(Style::default())
            .map(|g| (g.symbol.to_string(), g.style))
            .collect();
        let start = pos;
        pos += graphemes.len();
        if pos <= from || start >= to {
            out.push(span);
            continue;
        }
        for (i, (symbol, style)) in graphemes.into_iter().enumerate() {
            if start + i >= from && start + i < to {
                out.push(Span::styled(symbol, style.patch(hl)));
            } else {
                out.push(Span::styled(symbol, style));
            }
        }
    }
    line.spans = out;
}

/// Pad a row with highlighted spaces so a fully covered row reads as a solid
/// selection bar out to the right edge.
fn pad_row(line: &mut Line<'static>, width: u16, hl: Style) {
    let w = width as usize;
    let line_w = line.width();
    if line_w < w {
        line.spans.push(Span::styled(" ".repeat(w - line_w), hl));
    }
}

/// Wrap a stored transcript line to `width` display cells, keeping the
/// leading transcript indent (re-applied per row), expanding tabs, and
/// dropping C0 controls. `first_row_overhang` carves that many leading
/// cells (the echoed `❯ ` glyph) out of the wrapped body and re-applies
/// them to the first row only — exactly like the indent — so the body's
/// row-0 wrap width is reduced by the glyph once, matching the live
/// composer's narrower row-0 wrap; callers without a glyph pass 0.
pub(crate) fn wrap_line_display(
    line: &Line<'static>,
    width: u16,
    first_row_overhang: usize,
) -> Vec<Line<'static>> {
    let w = width.max(1) as usize;
    let output_indent = line.spans.first().is_some_and(|span| {
        span.content.as_ref() == transcript_indent() && span.style.bg.is_none()
    });
    let indent_width = if output_indent {
        TRANSCRIPT_INDENT.min(w)
    } else {
        0
    };

    #[derive(Clone)]
    struct Unit {
        text: String,
        style: Style,
        width: usize,
    }

    // Drop the whole leading indent span (TRANSCRIPT_INDENT cells), not just
    // one grapheme — the indent is re-added per wrapped row below.
    let graphemes = line
        .styled_graphemes(Style::default())
        .skip(if output_indent { indent_width } else { 0 });
    // Keep tabs as separate units for tabstop-aware expansion; drop other C0.
    let mut raw: Vec<(String, Style, bool)> = Vec::new();
    for sg in graphemes {
        if sg.symbol == "\t" {
            raw.push(("\t".to_string(), sg.style, true));
        } else if sg.symbol.chars().all(|c| c.is_control()) {
            continue;
        } else if sg.symbol.chars().any(|c| c.is_control()) {
            let filtered: String = sg.symbol.chars().filter(|c| !c.is_control()).collect();
            if filtered.is_empty() {
                continue;
            }
            raw.push((filtered, sg.style, false));
        } else {
            raw.push((sg.symbol.to_string(), sg.style, false));
        }
    }

    // Carve off the echoed glyph — the first `first_row_overhang` body
    // cells after the indent. Like the indent, it is re-applied per row
    // (row 0 only), so it must not also count inside the wrapped body;
    // otherwise row 0 wraps a glyph narrower than the composer's.
    let mut glyph: Vec<(String, Style)> = Vec::new();
    let mut glyph_width = 0usize;
    for _ in 0..first_row_overhang {
        let Some((symbol, _, _)) = raw.first() else {
            break;
        };
        let width = symbol
            .chars()
            .map(|c| c.width().unwrap_or(0))
            .sum::<usize>()
            .max(1);
        let (text, style, _) = raw.remove(0);
        glyph_width += width;
        glyph.push((text, style));
        if glyph_width >= first_row_overhang {
            break;
        }
    }

    let mut rows: Vec<Vec<Unit>> = Vec::new();
    let mut row = Vec::new();
    // Row 0 starts already carrying the transcript indent plus the echoed
    // glyph, so its wrap width is reduced by both.
    let mut row_width = (indent_width + glyph_width).min(w);
    let mut last_space: Option<usize> = None;
    for (symbol, style, is_tab) in raw {
        // Tab width is relative to the current column (row_width).
        let mut text = symbol.clone();
        let mut width = if is_tab {
            TAB_WIDTH - (row_width % TAB_WIDTH)
        } else {
            symbol
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
                .max(1)
        };
        let mut whitespace = is_tab || symbol.chars().all(char::is_whitespace);
        if is_tab {
            text = " ".repeat(width);
        }
        if row_width + width > w && !row.is_empty() {
            if let Some(space) = last_space {
                let remainder = row.split_off(space + 1);
                row.truncate(space);
                rows.push(row);
                row = remainder;
            } else {
                rows.push(row);
                row = Vec::new();
            }
            // Only the first wrapped row is narrowed; continuations get the
            // full width (minus the indent), like the composer.
            row_width = indent_width + row.iter().map(|u: &Unit| u.width).sum::<usize>();
            last_space = None;
            if is_tab {
                width = TAB_WIDTH - (row_width % TAB_WIDTH);
                text = " ".repeat(width);
                whitespace = true;
            }
        }
        if whitespace {
            last_space = Some(row.len());
        }
        row_width += width;
        row.push(Unit { text, style, width });
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(row);
    }

    let out: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = Vec::new();
            if output_indent {
                spans.push(Span::raw(" ".repeat(indent_width)));
            }
            if i == 0 {
                spans.extend(
                    glyph
                        .iter()
                        .map(|(text, style)| Span::styled(text.clone(), *style)),
                );
            }
            spans.extend(row.into_iter().map(|u| Span::styled(u.text, u.style)));
            Line::from(spans)
        })
        .collect();
    out
}
