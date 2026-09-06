#![allow(dead_code)]

use std::fmt;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crossterm::Command;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::agent::state::ToolState;
use crate::core::types::{Role, SinkLine};
use crate::llm::config::LlmConfig;
use crate::session::Session;

mod input;
mod remote;
mod render;
mod slash;
mod status;
mod theme;
mod wrapping;

pub(crate) use remote::{mark_launch_start, run_ratatui_repl_with_remote};
pub(crate) use render::view;

use input::InputField;

const VERTICAL_GUTTER: u16 = 1;
const HORIZONTAL_GUTTER: u16 = 1;
const TRANSCRIPT_INDENT: usize = HORIZONTAL_GUTTER as usize;
const INPUT_BORDER_ROWS: u16 = 0;
const INPUT_PAD_Y: u16 = 1;
const STATUS_CONTENT_ROWS: u16 = 1;
const INPUT_MIN_ROWS: u16 = 3;
const INPUT_STATUS_GUTTER: u16 = 0;
const APPROVAL_HEIGHT: u16 = 11;
pub(super) const TAB_WIDTH: usize = 8;

/// Raised-surface colors are resolved in `ui/theme.rs` from the terminal's
/// own palette / detected background, so they follow the terminal theme.
/// Max markdown re-parse rate while streaming (`markdown_lines` +
/// tree-sitter runs once per window, not once per token). Wall-clock, not
/// tick-based, so it stays constant when the frame rate changes.
const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(120);

/// A semantic transcript block. Gaps between blocks are **not** stored;
/// they are inserted by `TranscriptView::render` (`ui/render.rs`) as a
/// single blank `Line` between any two blocks. This makes gutter handling
/// canonical and removes the need for ad-hoc `push_transcript_gap` /
/// `in_assistant_stream` bookkeeping at every call site.
#[derive(Debug)]
pub(crate) enum TranscriptBlock {
    User {
        stamp: u64,
        lines: Vec<Line<'static>>,
    },
    Assistant {
        stamp: u64,
        lines: Vec<Line<'static>>,
    },
    /// Streamed model reasoning, stored raw. Rendered by `TranscriptView` as
    /// a one-line preview (collapsed) or full dim text (expanded via
    /// Ctrl+T); not routed through `lines()`.
    Thinking {
        stamp: u64,
        text: String,
        /// When the first delta landed; the settled duration is measured
        /// from here so the closed line can read "Thought for 4s".
        started: Instant,
        /// Set when the block closes; `None` while streaming.
        elapsed: Option<Duration>,
    },
    /// Live turn activity: pushed when a turn starts and kept at the
    /// transcript tail (moved on every busy-time append) so the animated
    /// "● Working" indicator trails the newest block; on turn end it
    /// settles into the "Worked for …" summary line.
    Activity {
        stamp: u64,
        started: Instant,
        /// Set on turn end; `None` while the turn runs.
        settled: Option<String>,
    },
    Tool {
        stamp: u64,
        input: Line<'static>,
        output: Option<Line<'static>>,
        preview: Vec<Line<'static>>,
        /// Short arg as emitted by `ToolInput` (e.g. `src/main.rs:1-20`).
        /// Stored — not parsed back out of the rendered `input` line — so
        /// `ToolOutput` can pick the preview language for syntax
        /// highlighting without span scraping.
        tool_arg: String,
    },
    System {
        stamp: u64,
        line: Line<'static>,
    },
    Error {
        stamp: u64,
        line: Line<'static>,
    },
    Info {
        stamp: u64,
        line: Line<'static>,
    },
    /// Multi-line pre-rendered block (the session-start DEX art). One block,
    /// so its rows render back-to-back — separate blocks would each get a
    /// blank gap line from `TranscriptView` and shred the art.
    Banner {
        stamp: u64,
        lines: Vec<Line<'static>>,
    },
}

impl TranscriptBlock {
    /// Content stamp: bumped on every mutation after construction so the
    /// per-block display cache (`App.wrapped_cache`) can tell a changed block
    /// from a stable one without re-wrapping the whole transcript.
    fn stamp(&self) -> u64 {
        match self {
            Self::User { stamp, .. }
            | Self::Assistant { stamp, .. }
            | Self::Thinking { stamp, .. }
            | Self::Activity { stamp, .. }
            | Self::Tool { stamp, .. }
            | Self::System { stamp, .. }
            | Self::Error { stamp, .. }
            | Self::Info { stamp, .. }
            | Self::Banner { stamp, .. } => *stamp,
        }
    }

    fn bump(&mut self) {
        let stamp = match self {
            Self::User { stamp, .. }
            | Self::Assistant { stamp, .. }
            | Self::Thinking { stamp, .. }
            | Self::Activity { stamp, .. }
            | Self::Tool { stamp, .. }
            | Self::System { stamp, .. }
            | Self::Error { stamp, .. }
            | Self::Info { stamp, .. }
            | Self::Banner { stamp, .. } => stamp,
        };
        *stamp = stamp.wrapping_add(1);
    }
}

/// Per-block wrapped display rows, parallel to `transcript`. Blocks are
/// append-only, so a streaming flush re-wraps only the blocks whose stamp
/// changed instead of the whole transcript.
pub(crate) struct WrappedBlock {
    /// Block stamp the rows were wrapped at; `u64::MAX` = not wrapped yet.
    stamp: u64,
    rows: Vec<Line<'static>>,
}

impl TranscriptBlock {
    /// Lines that belong to this block, in display order.
    pub(crate) fn lines(&self) -> Vec<&Line<'static>> {
        match self {
            TranscriptBlock::User { lines, .. } => lines.iter().collect(),
            TranscriptBlock::Assistant { lines, .. } => lines.iter().collect(),
            TranscriptBlock::Thinking { .. } => vec![],
            TranscriptBlock::Activity { .. } => vec![],
            TranscriptBlock::Tool {
                input,
                output,
                preview,
                ..
            } => {
                let mut out = Vec::with_capacity(1 + output.is_some() as usize + preview.len());
                out.push(input);
                if let Some(o) = output {
                    out.push(o);
                }
                out.extend(preview.iter());
                out
            }
            TranscriptBlock::System { line, .. } => vec![line],
            TranscriptBlock::Error { line, .. } => vec![line],
            TranscriptBlock::Info { line, .. } => vec![line],
            TranscriptBlock::Banner { lines, .. } => lines.iter().collect(),
        }
    }
}

/// The TUI application state. Rendering lives in `ui/render.rs` (`view`);
/// turn execution lives either in the local engine or, in client-server
/// mode, in `ui/remote.rs` which drives the same state from daemon events.
pub(crate) struct App {
    pub(crate) transcript: Vec<TranscriptBlock>,
    pub(crate) input: InputField,
    pub(crate) config: LlmConfig,
    pub(crate) messages: Vec<crate::core::types::ChatMessage>,
    pub(crate) tool_state: ToolState,
    pub(crate) session: Session,
    pub(crate) skills: Vec<crate::core::types::Skill>,
    pub(crate) turn_start: usize,
    pub(crate) cwd: String,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_dirty: bool,
    pub(crate) steering_rx: Option<mpsc::Receiver<String>>,
    pub(crate) followup_rx: Option<mpsc::Receiver<String>>,
    pub(crate) pending_steering: Vec<String>,
    pub(crate) pending_followups: Vec<String>,
    pub(crate) cancel_requested: bool,
    pub(crate) approval_rx: Option<mpsc::Receiver<crate::core::types::ApprovalRequest>>,
    pub(crate) pending_approval: Option<PendingApproval>,
    pub(crate) busy: bool,
    pub(crate) autoscroll: bool,
    pub(crate) scroll: u16,
    pub(crate) tick: u16,
    pub(crate) quit: bool,
    pub(crate) last_ctrl_c: Option<Instant>,
    pub(crate) history: Vec<String>,
    pub(crate) history_index: Option<usize>,
    pub(crate) history_draft: String,
    pub(crate) slash_selected: usize,
    /// How this TUI reached its agent engine, e.g. "[L] 127.0.0.1" (local
    /// loopback) or "[R] daemon.internal" (remote). Pinned to the right edge
    /// of the status bar; the only other session-start block is the DEX art.
    pub(crate) connection: Option<String>,
    /// Whether the tail `Assistant` block is still open for streaming
    /// coalescence. Tracked so an initial transcript block (e.g. in tests)
    /// does not merge with the first streamed assistant turn; gaps remain
    /// canonical between blocks.
    pub(crate) assistant_open: bool,
    /// Whether streamed thinking blocks render in full (Ctrl+T) or as a
    /// one-line preview.
    pub(crate) show_thinking: bool,
    /// Whether the tail `Thinking` block is still streaming deltas. Drives
    /// the collapsed indicator's dot animation; it closes (settles) as soon
    /// as any non-thinking line arrives or the turn ends.
    pub(crate) thinking_open: bool,
    pub(crate) plan: crate::core::types::Plan,
    /// Assistant deltas buffered between throttle windows. `markdown_lines`
    /// (term-md + tree-sitter) runs on the buffer at most once per
    /// `STREAM_FLUSH_INTERVAL` instead of once per token; `flush_assistant`
    /// drains it into the tail `Assistant` block.
    pub(crate) assistant_pending: String,
    /// Markdown gap state for the open assistant block. Survives throttled
    /// `flush_assistant` clears of `assistant_pending` so a heading/list at a
    /// window edge keeps its top air; reset on every fresh assistant block.
    pub(crate) assistant_gap: crate::core::markdown::GapState,
    /// Last wall-clock markdown/thinking flush; gates `append_sink_line`
    /// throttling so the re-parse rate is frame-rate independent.
    pub(crate) stream_last_flush: Instant,
    /// Wrapped rows per transcript block, parallel to `transcript`. Kept in
    /// sync (and `display_cache` rebuilt) by `TranscriptView::render` only
    /// when a block's stamp changes, a block is added, or the width changes.
    pub(crate) wrapped_cache: Vec<WrappedBlock>,
    /// Terminal width the `wrapped_cache` rows were wrapped to.
    pub(crate) wrapped_width: u16,
    /// Concatenated wrapped rows with block-gap separators; sliced to the
    /// visible window each frame so a full-transcript clone never happens.
    pub(crate) display_cache: Vec<Line<'static>>,
    /// Transcript rect from the last rendered frame; mouse events are
    /// translated through it into `display_cache` row space.
    pub(crate) transcript_area: Option<Rect>,
    /// Live mouse drag selection (raw anchor/end cell in display space).
    pub(crate) selection: Option<Selection>,
    /// Transient status-bar notice, e.g. copy confirmation. Expires after
    /// [`NOTICE_LIFETIME`]; the event loop redraws once on expiry.
    pub(crate) notice: Option<(String, Instant)>,
}

impl App {
    pub(crate) fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.history_draft = self.input.text();
        }
        let idx = self
            .history_index
            .map(|i| (i + 1).min(self.history.len() - 1))
            .unwrap_or(0);
        self.history_index = Some(idx);
        self.input = InputField::from_text(&self.history[self.history.len() - 1 - idx]);
    }

    pub(crate) fn history_down(&mut self) {
        match self.history_index {
            None => {}
            Some(0) => {
                self.history_index = None;
                self.input = InputField::from_text(&self.history_draft);
            }
            Some(idx) => {
                let new_idx = idx - 1;
                self.history_index = Some(new_idx);
                self.input = InputField::from_text(&self.history[self.history.len() - 1 - new_idx]);
            }
        }
    }

    pub(crate) fn history_push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Copy mouse-selected transcript text to the system clipboard via
    /// OSC 52 and flash a status-bar notice. Empty selections are skipped;
    /// oversized ones are refused rather than truncated.
    pub(crate) fn copy_selection(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        const MAX_COPY_BYTES: usize = 100_000;
        let bytes = text.as_bytes();
        if bytes.len() > MAX_COPY_BYTES {
            self.notice = Some(("selection too large to copy".into(), Instant::now()));
            return;
        }
        match crossterm::execute!(std::io::stdout(), SetClipboard(b64(bytes))) {
            Ok(()) => {
                self.notice = Some((
                    format!("copied {} chars", text.chars().count()),
                    Instant::now(),
                ))
            }
            Err(_) => self.notice = Some(("copy failed".into(), Instant::now())),
        }
    }

    /// Clear an expired notice, returning true so the caller redraws the
    /// status line.
    pub(crate) fn tick_notice(&mut self) -> bool {
        match &self.notice {
            Some((_, at)) if at.elapsed() >= NOTICE_LIFETIME => {
                self.notice = None;
                true
            }
            _ => false,
        }
    }
}

pub(crate) struct PendingApproval {
    pub(crate) name: String,
    pub(crate) input: String,
    pub(crate) response: tokio::sync::mpsc::Sender<crate::core::types::ApprovalDecision>,
    pub(crate) selected: usize,
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// DECSET 1000 + 1002 + 1006: report mouse events using the SGR encoding, so
/// wheel scrolling arrives as real `Event::Mouse` input instead of the
/// terminal synthesizing Up/Down arrow presses (DECSET 1007). 1002 adds
/// button-event motion tracking: drag events flow only while a button is
/// held — exactly what transcript drag-select needs — while unpressed
/// pointer movement stays silent, so no event flood. Left press/drag/release
/// drives in-app selection with OSC 52 copy; holding Shift (Option in
/// iTerm2) still bypasses mouse reporting for native selection.
pub(crate) struct EnableMouseScroll;

impl Command for EnableMouseScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// How long a status-bar notice (e.g. copy confirmation) stays visible.
pub(crate) const NOTICE_LIFETIME: Duration = Duration::from_secs(2);

/// Mouse drag selection over the transcript, in `display_cache` (row, col)
/// cell space. `anchor`/`end` are the raw press/release points; `norm()`
/// orders them for highlight and copy. `sticky` selections (double-click
/// word picks, triple-click line picks) survive mouse-up so the highlight
/// stays visible until the next click. `whole_line` selections cover full
/// rows: `anchor` is the press point and `end` (moved by dragging) only
/// contributes its row — copy and highlight take every row between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: (usize, usize),
    pub(crate) end: (usize, usize),
    pub(crate) sticky: bool,
    pub(crate) whole_line: bool,
}

impl Selection {
    /// Anchor/end ordered top-left → bottom-right.
    pub(crate) fn norm(&self) -> ((usize, usize), (usize, usize)) {
        if (self.anchor.0, self.anchor.1) <= (self.end.0, self.end.1) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    /// True when the press never moved: a click clears, it doesn't copy.
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor == self.end
    }
}

/// Expand a click at char index `col` on a display line to the enclosing
/// word: the maximal run of non-whitespace chars. `None` past the line's
/// text or when the click lands on whitespace.
pub(crate) fn word_bounds(line: &Line<'static>, col: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    Some((start, end))
}

/// Char count of a display line: the extent a whole-line (triple-click)
/// selection covers on that row.
pub(crate) fn line_width(line: &Line<'static>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

/// OSC 52 clipboard set: `ESC ] 52 ; c ; <base64> ST`. Honored by xterm,
/// alacritty, kitty, foot, wezterm, Windows Terminal, and tmux (with
/// `set-clipboard on`); unsupported terminals ignore it silently — the
/// highlight stays visible, so Shift+drag native selection remains the
/// fallback.
pub(crate) struct SetClipboard(pub(crate) String);

impl Command for SetClipboard {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]52;c;{}\x1b\\", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Standard base64 with padding, enough for OSC 52 payloads; no dependency.
fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Translate a mouse event into `display_cache` (row, col) space: the event
/// cell must land inside the transcript area on a real row. `None` outside.
pub(crate) fn mouse_display_cell(
    scroll: u16,
    area: Option<Rect>,
    rows: usize,
    m: &crossterm::event::MouseEvent,
) -> Option<(usize, usize)> {
    let area = area?;
    if m.column < area.x
        || m.row < area.y
        || m.column >= area.x + area.width
        || m.row >= area.y + area.height
    {
        return None;
    }
    let row = scroll as usize + (m.row - area.y) as usize;
    if row >= rows {
        return None;
    }
    Some((row, (m.column - area.x) as usize))
}

/// Plain text of a normalized selection: full rows join with newlines, the
/// anchor/end rows are sliced to the selected columns. Copies what's on
/// screen (pre-wrapped), matching what native terminal selection would hand
/// over.
pub(crate) fn selection_text(
    rows: &[Line<'static>],
    (r0, c0): (usize, usize),
    (r1, c1): (usize, usize),
) -> String {
    let text =
        |l: &Line<'static>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    if r0 == r1 {
        return rows
            .get(r0)
            .map(|l| {
                text(l)
                    .chars()
                    .skip(c0)
                    .take(c1.saturating_sub(c0))
                    .collect()
            })
            .unwrap_or_default();
    }
    let mut out: Vec<String> = Vec::new();
    if let Some(l) = rows.get(r0) {
        out.push(text(l).chars().skip(c0).collect());
    }
    for line in rows.get(r0 + 1..).into_iter().flatten().take(r1 - r0 - 1) {
        out.push(text(line));
    }
    if let Some(l) = rows.get(r1) {
        out.push(text(l).chars().take(c1).collect());
    }
    out.join("\n")
}

/// Copy text for whole-line (triple-click) selections: every row from
/// `r0..=r1` in full, joined by newlines — no column clipping, so all
/// covered lines are copied whole regardless of length.
pub(crate) fn line_selection_text(rows: &[Line<'static>], r0: usize, r1: usize) -> String {
    (r0..=r1.min(rows.len().saturating_sub(1)))
        .filter_map(|r| rows.get(r))
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn transcript_indent() -> String {
    " ".repeat(TRANSCRIPT_INDENT)
}

fn indent_transcript_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(transcript_indent()));
    line
}

/// True when a stored transcript line renders as air: every span is
/// whitespace. Direct-pushed blanks (`Line::default()`) carry no spans;
/// markdown-rendered blank rows carry the 1-space transcript indent.
fn line_is_air(line: &Line) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

pub(super) fn push_info_line(app: &mut App, line: Line<'static>) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    app.transcript.push(TranscriptBlock::Info {
        stamp: 0,
        line: indent_transcript_line(line),
    });
    move_activity_to_tail(app);
}

pub(super) fn push_info(app: &mut App, text: String) {
    push_info_line(
        app,
        Line::from(Span::styled(text, Style::default().fg(Color::Cyan))),
    );
}

/// Session-start ASCII art ("DEX"), pushed as the transcript's first block.
const DEX_ART: &str = "\
██████╗ ███████╗██╗  ██╗
██╔══██╗██╔════╝╚██╗██╔╝
██║  ██║█████╗   ╚███╔╝
██║  ██║██╔══╝   ██╔██╗
██████╔╝███████╗██╔╝ ██╗
╚═════╝ ╚══════╝╚═╝  ╚═╝";

/// Push the session-start DEX art. One `Banner` block (not per-row Info
/// blocks) so `TranscriptView` renders the rows back-to-back without the
/// blank gap it inserts between blocks.
pub(super) fn push_banner(app: &mut App) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    let lines = DEX_ART
        .lines()
        .map(|row| {
            indent_transcript_line(Line::from(Span::styled(
                row.trim_end().to_string(),
                Style::default().fg(Color::Cyan),
            )))
        })
        .collect();
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

/// Drain the buffered assistant deltas into the tail `Assistant` block,
/// rendering the markdown once for the whole buffered chunk. Call before
/// anything reads the transcript or pushes a non-assistant block, so pending
/// text always lands in the block that was streaming it.
fn flush_assistant(app: &mut App) {
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
        if let Some(TranscriptBlock::Assistant { lines, stamp }) = app.transcript.last_mut() {
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
}

/// Route a streamed console line into the transcript with the same styling
/// the local engine uses, so remote and local turns look identical.
/// Each `SinkLine` maps to one `TranscriptBlock` (or an extension of the
/// tail `Assistant` block while streaming). No empty gap `Line`s are stored;
/// `TranscriptView` inserts a single blank `Line` between any two blocks.
pub(super) fn append_sink_line(app: &mut App, sl: SinkLine) {
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
        SinkLine::Assistant(s) => {
            if s.trim().is_empty() {
                // Blank inside the current assistant message (e.g. streaming
                // blank line between paragraphs). Keep it inside the tail
                // Assistant block so the inter-block gutter remains canonical.
                // Blank runs collapse to one air row (CommonMark/pi/codex):
                // the model often emits several, and each used to push its
                // own `Line::default()`.
                if app.assistant_open {
                    flush_assistant(app);
                    if let Some(TranscriptBlock::Assistant { lines, stamp }) =
                        app.transcript.last_mut()
                    {
                        if !lines.last().is_some_and(line_is_air) {
                            lines.push(Line::default());
                            *stamp = stamp.wrapping_add(1);
                        }
                    }
                    app.assistant_gap.note_blank();
                }
                return;
            }
            // ponytail: buffer deltas — markdown (term-md + tree-sitter) runs
            // at most once per throttle window instead of once per line. Each
            // sink line is one complete markdown line (stream.rs trims the
            // trailing newline), so rejoin buffered lines with '\n' to keep
            // paragraph structure across the throttle window. Normalize blank
            // lines around block-level markdown so dense model output still
            // renders with air between sections. Gap state survives throttled
            // flushes (which clear `assistant_pending`): without it a
            // heading/list at a window edge lost its top air while the bottom
            // air survived via the trailing blank.
            if !app.assistant_open && app.assistant_pending.is_empty() {
                app.assistant_gap.reset();
            }
            let chunk = app.assistant_gap.normalize(&s);
            if !app.assistant_pending.is_empty() && !app.assistant_pending.ends_with('\n') {
                app.assistant_pending.push('\n');
            }
            app.assistant_pending.push_str(&chunk);
            // Tables need header + delimiter + rows in one render window:
            // markdown is re-parsed per flush, so a table split across
            // windows would fall back to raw paragraphs. Hold the flush
            // while the buffer ends on a table line; prose, a blank line or
            // the next non-assistant sink line releases the whole table.
            let last_line = app
                .assistant_pending
                .trim_end()
                .rsplit('\n')
                .next()
                .unwrap_or("");
            let holding_table = crate::core::markdown::is_table_line(last_line);
            if stream_flush_due(app) && !holding_table {
                flush_assistant(app);
            }
        }
        SinkLine::Thinking(s) => {
            if s.is_empty() {
                return;
            }
            app.assistant_open = false;
            // ponytail: throttle re-wraps — reasoning arrives token-by-
            // token; the block settles on close, which bumps unconditionally.
            // (Gate read before the mutable borrow; clock reset after it.)
            let due = stream_flush_due(app);
            if let Some(TranscriptBlock::Thinking { text, stamp, .. }) = app.transcript.last_mut() {
                text.push_str(&s);
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
            if due {
                note_stream_flush(app);
            }
            app.thinking_open = true;
        }
        SinkLine::ToolInput(s) => {
            dim_intermediate_assistant_block(app);
            app.assistant_open = false;
            let mut it = s.splitn(2, ' ');
            let name = it.next().unwrap_or("").to_string();
            let arg = it.next().unwrap_or("").to_string();
            let input = render::render_tool_input(&name, &arg);
            app.transcript.push(TranscriptBlock::Tool {
                stamp: 0,
                input,
                output: None,
                preview: Vec::new(),
                tool_arg: arg,
            });
        }
        SinkLine::ToolOutput {
            name,
            summary,
            success,
            preview,
            duration,
        } => {
            // The ▸ line above already names the tool; the └ line leads with
            // the outcome (glyph + summary) and trails timing in dim.
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
                    format!(" · {}", crate::core::format::format_duration(duration)),
                    Style::default().fg(theme::muted_fg()),
                ));
            }
            let output = indent_transcript_line(Line::from(spans));
            // write/edit previews are a git diff: color like git does. read
            // previews keep the numbered gutter dim and highlight the code
            // by extension (opencode/Claude Code style, one tree-sitter
            // pass per file section); anything unhighlightable stays dim.
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
                // Language from the stored `ToolInput` arg (first token is
                // the path: `src/main.rs:1-20`, a glob, or `N files`).
                // `==> file <==` fan-out headers inside re-target per
                // section in `render_read_preview`.
                let arg_path = app
                    .transcript
                    .last()
                    .and_then(|b| match b {
                        TranscriptBlock::Tool {
                            output: None,
                            tool_arg,
                            ..
                        } => Some(tool_arg.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let path = arg_path.split_whitespace().next().unwrap_or("");
                let path = path.split(':').next().unwrap_or(path);
                render::render_read_preview(&preview, crate::core::lang::lang_from_path(path))
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
            if let Some(TranscriptBlock::Tool {
                output: out,
                preview: prev,
                stamp,
                ..
            }) = app.transcript.last_mut()
            {
                if out.is_none() {
                    *out = Some(output);
                    *prev = preview_lines;
                    *stamp = stamp.wrapping_add(1);
                    return;
                }
            }
            // Fallback: no open ToolInput (e.g. replay); synthesize a block.
            // No arg is known here, so previews stay dim — the replay path
            // (`rebuild_transcript`) always emits `ToolInput` first, which
            // carries the arg for highlighting.
            app.transcript.push(TranscriptBlock::Tool {
                stamp: 0,
                input: indent_transcript_line(Line::from(Span::styled(
                    "▸ tool",
                    Style::default().fg(Color::Yellow),
                ))),
                output: Some(output),
                preview: preview_lines,
                tool_arg: String::new(),
            });
        }
        SinkLine::System(s) => {
            app.assistant_open = false;
            app.transcript.push(TranscriptBlock::System {
                stamp: 0,
                line: indent_transcript_line(Line::from(vec![
                    Span::styled("· ", Style::default().fg(theme::muted_fg())),
                    Span::styled(s, Style::default().fg(theme::muted_fg())),
                ])),
            });
        }
        // Usage updates flow into the status bar via StreamEvent::Usage in
        // the remote handler, not into the transcript.
        SinkLine::Usage { .. } => {}
        SinkLine::Plan(plan) => {
            app.plan = plan;
        }
        SinkLine::Error(s) => {
            app.assistant_open = false;
            app.transcript.push(TranscriptBlock::Error {
                stamp: 0,
                line: indent_transcript_line(Line::from(vec![
                    Span::styled("! ", Style::default().fg(Color::Red)),
                    Span::styled(format!("error: {s}"), Style::default().fg(Color::Red)),
                ])),
            });
        }
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
pub(super) fn close_thinking(app: &mut App) {
    if app.thinking_open {
        app.thinking_open = false;
        // Stamp bump forces a re-wrap so the settled row shows the elapsed
        // "Thought for …" (and an expanded Ctrl+T block shows any text that
        // arrived since the last throttled bump).
        if let Some(TranscriptBlock::Thinking {
            started,
            elapsed,
            stamp,
            ..
        }) = app.transcript.last_mut()
        {
            *elapsed = Some(started.elapsed());
            *stamp = stamp.wrapping_add(1);
        }
    }
}

/// Push the live turn-activity block: an animated "● Working" indicator
/// that trails the latest transcript block while the turn runs and settles
/// into the "Worked for …" summary when it ends.
pub(super) fn start_activity(app: &mut App) {
    app.transcript.push(TranscriptBlock::Activity {
        stamp: 0,
        started: Instant::now(),
        settled: None,
    });
    app.autoscroll = true;
}

/// Move the open turn-activity block to the transcript tail so the animated
/// "● Working" indicator always sits under the newest block. Called after
/// every busy-time append; no-op without a running turn (settled blocks
/// from finished turns stay where they settled).
fn move_activity_to_tail(app: &mut App) {
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
    if pos + 1 == app.transcript.len() {
        return;
    }
    let block = app.transcript.remove(pos);
    app.transcript.push(block);
    // Wrapped rows after `pos` shifted up a slot; drop their (tail few)
    // cache entries — TranscriptView re-wraps them.
    app.wrapped_cache.truncate(pos);
    app.selection = None;
}

/// Settle the open turn-activity block: move it to the transcript tail and
/// swap the animated "● Working" indicator for the turn's summary —
/// "Worked for 12.3s · 4.2k tokens". Duration is measured from the block's
/// start, so it spans the whole turn (thinking included). No-op when no
/// turn ran (e.g. a replay that never saw the turn start).
pub(super) fn settle_activity(app: &mut App, tokens: u64) {
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
    app.transcript.remove(pos);
    app.transcript.push(TranscriptBlock::Activity {
        stamp: 0,
        started,
        settled: Some(format!(
            "Worked for {:.1}s · {} tokens",
            started.elapsed().as_secs_f64(),
            format_tokens(tokens),
        )),
    });
    app.wrapped_cache.truncate(pos);
    app.selection = None;
}

/// Ctrl+T toggles expanded thinking. The wrapped rows of a thinking block
/// depend on that flag, so stamp-bump every thinking block to force a re-wrap.
pub(super) fn bump_thinking_stamps(app: &mut App) {
    for block in &mut app.transcript {
        if matches!(block, TranscriptBlock::Thinking { .. }) {
            block.bump();
        }
    }
}

fn dim_intermediate_assistant_block(app: &mut App) {
    if let Some(TranscriptBlock::Assistant { lines, stamp }) = app.transcript.last_mut() {
        for line in lines.iter_mut() {
            for span in &mut line.spans {
                span.style = span.style.fg(theme::muted_fg());
            }
        }
        *stamp = stamp.wrapping_add(1);
    }
}

/// Send the user's approval decision for the pending tool execution.
pub(super) fn resolve_approval(app: &mut App, decision: crate::core::types::ApprovalDecision) {
    if let Some(approval) = app.pending_approval.take() {
        let _ = approval.response.try_send(decision);
    }
}

pub(super) fn scroll_transcript(app: &mut App, delta: i32) {
    app.autoscroll = false;
    app.scroll = (app.scroll as i32)
        .saturating_add(delta)
        .clamp(0, u16::MAX as i32) as u16;
}

/// Render the user's submitted prompt with the shared transcript grid.
/// No empty gap `Line`s are stored; gutter is inserted by `TranscriptView`.
pub(super) fn render_user_prompt(app: &mut App, line: &str) {
    flush_assistant(app);
    close_thinking(app);
    app.assistant_open = false;
    let user_bg = Style::default()
        .fg(theme::surface_fg())
        .bg(theme::surface_bg());
    let horizontal_pad = " ".repeat(TRANSCRIPT_INDENT);
    let edge_pad = Span::styled(" ", user_bg);
    let mut block_lines = Vec::new();
    block_lines.push(Line::from(edge_pad.clone()));
    for sub in line.split('\n') {
        block_lines.push(Line::from(vec![
            Span::styled(horizontal_pad.clone(), user_bg),
            Span::styled(sub.to_string(), user_bg),
            edge_pad.clone(),
        ]));
    }
    block_lines.push(Line::from(edge_pad));
    app.transcript.push(TranscriptBlock::User {
        stamp: 0,
        lines: block_lines,
    });
    move_activity_to_tail(app);
    app.autoscroll = true;
}

pub(crate) fn rebuild_transcript(app: &mut App) {
    app.transcript.clear();
    app.assistant_pending.clear();
    app.assistant_gap.reset();
    app.assistant_open = false;
    app.thinking_open = false;
    let msgs = app.messages.clone();
    for msg in msgs.iter().skip(1) {
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
                            crate::core::types::SinkLine::Assistant(content.clone()),
                        );
                    }
                }
                if let Some(calls) = &msg.tool_calls {
                    for tc in calls {
                        let input = format!("{} {}", tc.function.name, tc.function.arguments);
                        append_sink_line(app, crate::core::types::SinkLine::ToolInput(input));
                    }
                }
            }
            Role::Tool => {
                let name = msg.name.clone().unwrap_or_else(|| "tool".to_string());
                let content = msg.content.clone().unwrap_or_default();
                let mut lines = content.lines();
                let summary = lines.next().unwrap_or("").to_string();
                let preview: Vec<String> = lines.take(6).map(|s| s.to_string()).collect();
                append_sink_line(
                    app,
                    crate::core::types::SinkLine::ToolOutput {
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
    // The last replayed message may be assistant text still sitting in the
    // delta buffer; drain it so the rebuilt transcript is complete.
    flush_assistant(app);
    app.autoscroll = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_app() -> App {
        App {
            transcript: Vec::new(),
            input: crate::ui::input::InputField::new(),
            config: crate::llm::config::LlmConfig {
                provider: crate::core::types::Provider::OpenCode,
                api_key: String::new(),
                base_url: String::new(),
                model: "test".into(),
                available_models: vec!["test".into()],
                endpoints: Default::default(),
                api: crate::core::types::ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
                permission: crate::core::types::PermissionMode::Trusted,
                verify_command: None,
                extra_headers: Default::default(),
                provider_entries: Default::default(),
                provider_headers: Default::default(),
                api_pinned: false,
                client: reqwest::Client::new(),
            },
            messages: Vec::new(),
            tool_state: crate::agent::state::ToolState::default(),
            session: crate::session::Session::in_memory("/tmp".into()),
            skills: Vec::new(),
            turn_start: 0,
            cwd: "/tmp".into(),
            git_branch: None,
            git_dirty: false,
            steering_rx: None,
            followup_rx: None,
            pending_steering: Vec::new(),
            pending_followups: Vec::new(),
            cancel_requested: false,
            approval_rx: None,
            pending_approval: None,
            busy: false,
            autoscroll: true,
            scroll: 0,
            tick: 0,
            quit: false,
            last_ctrl_c: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            slash_selected: 0,
            connection: None,
            assistant_open: false,
            show_thinking: false,
            thinking_open: false,
            plan: crate::core::types::Plan::default(),
            assistant_pending: String::new(),
            assistant_gap: crate::core::markdown::GapState::new(),
            stream_last_flush: Instant::now(),
            wrapped_cache: Vec::new(),
            wrapped_width: 0,
            display_cache: Vec::new(),
            transcript_area: None,
            selection: None,
            notice: None,
        }
    }

    #[test]
    fn b64_matches_known_vectors() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn selection_norm_orders_cells() {
        let s = Selection {
            anchor: (5, 2),
            end: (3, 7),
            sticky: false,
            whole_line: false,
        };
        assert_eq!(s.norm(), ((3, 7), (5, 2)));
        let s = Selection {
            anchor: (2, 9),
            end: (2, 1),
            sticky: false,
            whole_line: false,
        };
        assert_eq!(s.norm(), ((2, 1), (2, 9)));
        assert!(Selection {
            anchor: (1, 1),
            end: (1, 1),
            sticky: false,
            whole_line: false,
        }
        .is_empty());
        assert!(!Selection {
            anchor: (1, 1),
            end: (1, 2),
            sticky: false,
            whole_line: false,
        }
        .is_empty());
        // Whole-line selects order by row; cols carry no meaning downstream.
        let s = Selection {
            anchor: (5, 0),
            end: (2, 4),
            sticky: true,
            whole_line: true,
        };
        assert_eq!(s.norm(), ((2, 4), (5, 0)));
    }

    #[test]
    fn line_width_counts_chars_across_spans() {
        let spans = Line::from(vec![Span::from("run "), Span::from("cargo")]);
        assert_eq!(line_width(&spans), 9);
        assert_eq!(line_width(&Line::default()), 0);
    }

    #[test]
    fn line_selection_text_takes_full_rows() {
        let rows = vec![
            Line::from("alpha"),
            Line::default(),
            Line::from(Span::styled("gamma!", Style::default().fg(Color::Cyan))),
        ];
        // Every covered row in full, blank row included.
        assert_eq!(line_selection_text(&rows, 0, 2), "alpha\n\ngamma!");
        // Single row.
        assert_eq!(line_selection_text(&rows, 1, 1), "");
        // End past the cache is clamped.
        assert_eq!(line_selection_text(&rows, 2, 9), "gamma!");
    }

    #[test]
    fn word_bounds_selects_enclosing_word() {
        let line = Line::from("run cargo test --all-targets");
        // Click inside "cargo" → whole word.
        assert_eq!(word_bounds(&line, 5), Some((4, 9)));
        assert_eq!(word_bounds(&line, 4), Some((4, 9)));
        assert_eq!(word_bounds(&line, 8), Some((4, 9)));
        // Word at line start/end ("--all-targets" spans 15..28).
        assert_eq!(word_bounds(&line, 1), Some((0, 3)));
        assert_eq!(word_bounds(&line, 27), Some((15, 28)));
        // Whitespace click or past end → no selection.
        assert_eq!(word_bounds(&line, 3), None);
        assert_eq!(word_bounds(&line, 28), None);
        // Spans are joined before scanning.
        let spans = Line::from(vec![Span::from("run "), Span::from("cargo")]);
        assert_eq!(word_bounds(&spans, 6), Some((4, 9)));
    }

    #[test]
    fn selection_text_slices_rows() {
        let rows = vec![
            Line::from("alpha"),
            Line::default(),
            Line::from(Span::styled("beta", Style::default().fg(Color::Cyan))),
        ];
        // Across rows, through the blank separator.
        assert_eq!(selection_text(&rows, (0, 1), (2, 2)), "lpha\n\nbe");
        // Single row, single range.
        assert_eq!(selection_text(&rows, (2, 1), (2, 3)), "et");
        // Anchor past the end of the line yields nothing.
        assert_eq!(selection_text(&rows, (0, 90), (0, 95)), "");
    }

    #[test]
    fn mouse_cell_maps_through_area() {
        use crossterm::event::{self, KeyModifiers};
        let area = Rect::new(0, 5, 40, 10);
        let m = |col: u16, row: u16| event::MouseEvent {
            kind: event::MouseEventKind::Down(event::MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        };
        // Inside: display row = scroll + offset, col = screen col.
        assert_eq!(
            mouse_display_cell(7, Some(area), 100, &m(3, 6)),
            Some((8, 3))
        );
        // Above the transcript area: ignored.
        assert_eq!(mouse_display_cell(7, Some(area), 100, &m(3, 4)), None);
        // Below the last cached row: ignored.
        assert_eq!(mouse_display_cell(97, Some(area), 100, &m(3, 9)), None);
        // Right edge is exclusive.
        assert_eq!(mouse_display_cell(0, Some(area), 100, &m(40, 5)), None);
        // No area recorded yet: ignored.
        assert_eq!(mouse_display_cell(0, None, 100, &m(0, 0)), None);
    }

    #[test]
    fn banner_is_one_block_of_six_art_rows() {
        // One Banner block, not per-row Info blocks: TranscriptView inserts a
        // blank gap line between blocks, which would shred the art apart.
        let mut app = test_app();
        push_banner(&mut app);
        assert_eq!(app.transcript.len(), 1);
        let lines = app.transcript[0].lines();
        assert_eq!(lines.len(), 6);
        let art = [
            "██████╗ ███████╗██╗  ██╗",
            "██╔══██╗██╔════╝╚██╗██╔╝",
            "██║  ██║█████╗   ╚███╔╝",
            "██║  ██║██╔══╝   ██╔██╗",
            "██████╔╝███████╗██╔╝ ██╗",
            "╚═════╝ ╚══════╝╚═╝  ╚═╝",
        ];
        for (line, row) in lines.iter().zip(art) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(text, format!(" {row}"), "indent + art row, in order");
        }
        // Art rows carry the info color (the indent span is unstyled).
        assert!(lines.iter().all(|l| l
            .spans
            .last()
            .is_some_and(|s| s.style.fg == Some(Color::Cyan))));
    }

    #[test]
    fn thinking_stream_coalesces_and_closes_on_assistant_text() {
        let mut app = test_app();
        // Throttle window open, so every delta bumps the version.
        app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
        for chunk in ["Let me ", "think."] {
            append_sink_line(
                &mut app,
                crate::core::types::SinkLine::Thinking(chunk.into()),
            );
        }
        assert!(app.thinking_open, "streaming deltas keep the block open");
        // The thinking bumps reset the shared throttle clock; reopen the
        // window so the assistant delta renders immediately.
        app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("done".into()),
        );
        assert!(
            !app.thinking_open,
            "assistant text closes the thinking block"
        );
        assert_eq!(app.transcript.len(), 2);
        assert!(matches!(
            &app.transcript[0],
            TranscriptBlock::Thinking { text, .. } if text == "Let me think."
        ));
        assert!(matches!(
            &app.transcript[1],
            TranscriptBlock::Assistant { .. }
        ));
        assert!(app.assistant_open);
    }

    #[test]
    fn thinking_only_deltas_stay_in_one_block() {
        let mut app = test_app();
        for chunk in ["a", "b", "c"] {
            append_sink_line(
                &mut app,
                crate::core::types::SinkLine::Thinking(chunk.into()),
            );
        }
        assert_eq!(app.transcript.len(), 1);
        assert!(
            matches!(&app.transcript[0], TranscriptBlock::Thinking { text, .. } if text == "abc")
        );
        assert!(app.thinking_open);
    }

    #[test]
    fn assistant_lines_buffer_until_the_throttle_window() {
        let mut app = test_app();
        // Inside a throttle window: just flushed, so nothing renders yet.
        app.stream_last_flush = Instant::now();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("hello".into()),
        );
        assert_eq!(app.assistant_pending, "hello\n");
        assert!(app.transcript.is_empty(), "nothing renders mid-window");

        // Window elapsed: the next delta flushes.
        app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("world".into()),
        );
        assert!(app.assistant_pending.is_empty());
        assert!(app.assistant_open);
        // Two complete markdown lines inside one window keep their line
        // break (the daemon coalescer joins with '\n' as well).
        let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
            panic!("expected assistant block");
        };
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("world"), "{text}");
        assert!(
            !text.contains("helloworld"),
            "line break lost while buffering: {text}"
        );

        // A non-assistant line drains the buffer before its own block lands.
        // Fresh window again so the tail buffers instead of flushing.
        app.stream_last_flush = Instant::now();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("tail".into()),
        );
        assert_eq!(app.assistant_pending, "tail\n");
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("bash echo".into()),
        );
        assert!(app.assistant_pending.is_empty());
        assert_eq!(app.transcript.len(), 2);
        let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
            panic!("expected assistant block");
        };
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("tail"), "buffered tail flushed first: {text}");
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptBlock::Tool { .. })
        ));
    }

    #[test]
    fn heading_keeps_top_air_across_flush_windows() {
        // A heading at a throttle edge used to butt against the flushed prose
        // (no top air) while the bottom air survived via the trailing blank.
        let mut app = test_app();
        let due = || Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
        app.stream_last_flush = due();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("intro text".into()),
        );
        assert!(app.assistant_open);
        app.stream_last_flush = due();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::Assistant("## Heading".into()),
        );
        let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
            panic!("expected assistant block");
        };
        let row = |l: &Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let rows: Vec<String> = lines.iter().map(row).collect();
        assert_eq!(rows.len(), 3, "top air missing across flush: {rows:?}");
        assert!(rows[0].contains("intro text"), "{rows:?}");
        assert!(rows[1].trim().is_empty(), "{rows:?}");
        assert!(rows[2].contains("Heading"), "{rows:?}");
        assert!(!rows[2].contains('#'), "{rows:?}");
    }

    #[test]
    fn assistant_blank_runs_collapse_to_one_air_row() {
        // Streaming blank lines between paragraphs collapse to a single air
        // row (CommonMark/pi/codex); each used to push its own blank `Line`.
        let mut app = test_app();
        let due = || Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
        for line in ["para one", "", "", "", "para two"] {
            app.stream_last_flush = due();
            append_sink_line(
                &mut app,
                crate::core::types::SinkLine::Assistant(line.into()),
            );
        }
        flush_assistant(&mut app);
        let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
            panic!("expected assistant block");
        };
        let rows: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(rows.len(), 3, "blank run not collapsed: {rows:?}");
        assert!(rows[0].contains("para one"), "{rows:?}");
        assert!(rows[1].trim().is_empty(), "{rows:?}");
        assert!(rows[2].contains("para two"), "{rows:?}");
    }

    #[test]
    fn thinking_closes_on_user_prompt() {
        // Steering-style interleave: a user prompt lands mid-turn.
        let mut app = test_app();
        append_sink_line(&mut app, crate::core::types::SinkLine::Thinking("h".into()));
        assert!(app.thinking_open);
        render_user_prompt(&mut app, "steer");
        assert!(!app.thinking_open);
    }

    #[test]
    fn thinking_close_records_elapsed_duration() {
        let mut app = test_app();
        append_sink_line(&mut app, crate::core::types::SinkLine::Thinking("h".into()));
        assert!(matches!(
            &app.transcript[0],
            TranscriptBlock::Thinking { elapsed: None, .. }
        ));
        close_thinking(&mut app);
        assert!(matches!(
            &app.transcript[0],
            TranscriptBlock::Thinking {
                elapsed: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn activity_block_trails_the_tail_then_settles() {
        let mut app = test_app();
        app.busy = true;
        start_activity(&mut app);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptBlock::Activity { settled: None, .. })
        ));
        // A tool block lands after it: the indicator must move below it.
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("bash cargo test".into()),
        );
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptBlock::Activity { settled: None, .. })
        ));
        assert!(matches!(
            app.transcript[app.transcript.len() - 2],
            TranscriptBlock::Tool { .. }
        ));
        settle_activity(&mut app, 4200);
        match app.transcript.last() {
            Some(TranscriptBlock::Activity {
                settled: Some(summary),
                ..
            }) => {
                assert!(summary.contains("Worked for"), "{summary}");
                assert!(summary.contains("4.2k tokens"), "{summary}");
            }
            other => panic!("expected settled activity block, got {other:?}"),
        }
        // Settling again (e.g. a duplicate finish event) is a no-op.
        settle_activity(&mut app, 1);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptBlock::Activity { .. })
        ));
        assert_eq!(app.transcript.len(), 2);
    }

    #[test]
    fn indent_transcript_line_adds_gutter() {
        let line = Line::from("test");
        let indented = indent_transcript_line(line);
        assert!(indented.spans[0].content.as_ref() == " ");
    }

    #[test]
    fn git_context_returns_empty_on_non_repo() {
        let (branch, dirty) = crate::core::format::git_context("/tmp/not-a-repo-12345");
        assert!(branch.is_none());
        assert!(!dirty);
    }

    #[test]
    fn tool_preview_lines_are_indented_and_dimmed() {
        // Preview formatting is exercised through the Tool block produced by
        // `append_sink_line`; the stored preview lines must be indented and
        // dimmed exactly as before.
        let mut app = test_app();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("bash echo hi".into()),
        );
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolOutput {
                name: "bash".into(),
                summary: "v ok".into(),
                success: true,
                preview: vec!["src/main.rs".into(), "… +3 more lines".into()],
                duration: 0.0,
            },
        );
        let TranscriptBlock::Tool { preview, .. } = &app.transcript[0] else {
            panic!("expected Tool block");
        };
        assert_eq!(preview.len(), 2);
        for line in preview {
            assert!(line.spans.len() == 2); // indent gutter + content
            assert_eq!(line.spans[1].style.fg, Some(theme::tool_preview_fg()));
        }
        assert!(preview[0].spans[1].content.as_ref() == "  src/main.rs");
        assert!(preview[1].spans[1].content.as_ref() == "  … +3 more lines");
    }

    #[test]
    fn read_preview_highlights_code_and_keeps_gutter_dim() {
        // Industry standard (opencode/Claude Code): read snippets highlight
        // by extension; the `{:>4}  ` gutter stays dim for alignment.
        let mut app = test_app();
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolInput("read src/main.rs:1-2".into()),
        );
        append_sink_line(
            &mut app,
            crate::core::types::SinkLine::ToolOutput {
                name: "read".into(),
                summary: "v 2 lines".into(),
                success: true,
                preview: vec!["   1  fn main() {}".into(), "… +1 more lines".into()],
                duration: 0.0,
            },
        );
        let TranscriptBlock::Tool { preview, .. } = &app.transcript[0] else {
            panic!("expected Tool block");
        };
        assert_eq!(preview.len(), 2);
        // Code row: indent + dim gutter + at least one highlighted span.
        // spans[0] is the transcript indent (unstyled), spans[1] the gutter.
        let gutter: String = preview[0].spans[1]
            .content
            .as_ref()
            .chars()
            .chain(
                preview[0]
                    .spans
                    .get(2)
                    .map(|s| s.content.as_ref())
                    .unwrap_or("")
                    .chars(),
            )
            .collect();
        assert!(gutter.contains('1'), "{gutter}");
        assert_eq!(preview[0].spans[1].style.fg, Some(theme::tool_preview_fg()));
        assert!(
            preview[0].spans[2..]
                .iter()
                .any(|s| s.style.fg != Some(theme::tool_preview_fg())),
            "code should highlight, got {:?}",
            preview[0]
        );
        // Tail row stays dim (spans[0] is the unstyled indent gutter).
        assert!(preview[1].spans[1..]
            .iter()
            .all(|s| s.style.fg == Some(theme::tool_preview_fg())));
    }

    /// Write a minimal persisted session JSONL (same entry shapes
    /// `Session::new`/`set_state` produce) so `apply_session_state` can be
    /// exercised without touching the real session directory.
    fn write_session_file(state_lines: &[&str]) -> PathBuf {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let mut h = DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        let tid = h.finish();
        let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "dex-apply-state-{}-{}-{}-{}.jsonl",
            std::process::id(),
            tid,
            nanos,
            nonce
        ));
        let mut lines = vec![r#"{"type":"session","version":1,"id":"statetest","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#.to_string()];
        lines.extend(state_lines.iter().map(|l| l.to_string()));
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn apply_session_state_restores_model_from_session_file() {
        let path = write_session_file(&[
            r#"{"type":"session_state","id":"1","timestamp":"2020-01-01T00:00:01Z","key":"model","value":"restored-model"}"#,
        ]);

        let mut app = test_app();
        crate::ui::slash::apply_session_state(&mut app, Some(&path));

        assert_eq!(app.config.model, "restored-model");
        assert!(app
            .config
            .available_models
            .contains(&"restored-model".to_string()));
    }

    #[test]
    fn apply_session_state_keeps_config_when_file_has_no_state_entries() {
        let path = write_session_file(&[]);

        let mut app = test_app();
        crate::ui::slash::apply_session_state(&mut app, Some(&path));

        assert_eq!(app.config.model, "test");
    }

    #[test]
    fn apply_session_state_is_noop_without_a_session_path() {
        let mut app = test_app();
        crate::ui::slash::apply_session_state(&mut app, None);

        assert_eq!(app.config.model, "test");
    }

    #[test]
    fn reset_session_state_clears_per_session_state() {
        let mut app = test_app();
        app.messages
            .push(crate::core::types::ChatMessage::user("hi"));
        app.transcript.push(TranscriptBlock::Info {
            stamp: 0,
            line: Line::from("old"),
        });
        app.tool_state.total_usage = 42;
        app.tool_state.total_cost = 1.5;
        app.tool_state.last_usage = Some(7);
        app.pending_steering.push("steer".into());
        app.pending_followups.push("follow".into());
        app.plan.steps.push(("step".into(), false));
        app.turn_start = 3;
        app.transcript.push(TranscriptBlock::Activity {
            stamp: 0,
            started: Instant::now(),
            settled: None,
        });
        app.scroll = 9;
        app.autoscroll = false;

        crate::ui::slash::reset_session_state(&mut app);

        assert!(app.messages.len() <= 1);
        assert!(app.transcript.is_empty());
        assert!(!app.assistant_open);
        assert!(app.autoscroll);
        assert_eq!(app.scroll, 0);
        assert_eq!(app.tool_state.total_usage, 0);
        assert_eq!(app.tool_state.total_cost, 0.0);
        assert!(app.tool_state.last_usage.is_none());
        assert!(app.pending_steering.is_empty());
        assert!(app.pending_followups.is_empty());
        assert!(app.plan.is_empty());
        assert_eq!(app.turn_start, 0);
        assert!(!app
            .transcript
            .iter()
            .any(|b| matches!(b, TranscriptBlock::Activity { .. })));
    }

    #[test]
    fn slash_new_refuses_while_busy() {
        let mut app = test_app();
        app.busy = true;
        crate::ui::slash::handle_slash(&mut app, "/new");
        assert!(
            app.transcript
                .iter()
                  .any(|b| matches!(b, TranscriptBlock::Info { line, .. } if line.spans.iter().any(|s| s.content.contains("turn is running"))))
        );
    }

    #[test]
    fn enter_expands_bare_picker_commands_instead_of_submitting() {
        // `/model`, `/provider`, `/resume` open a popup picker: Enter on the
        // bare form must expand to `"<cmd> "` (popup stays open) rather than
        // submit and print info into the transcript.
        for cmd in ["/model", "/provider", "/resume"] {
            let mut app = test_app();
            app.input = crate::ui::input::InputField::from_text(cmd);
            app.slash_selected = 5;
            assert!(
                crate::ui::slash::expand_bare_command(&mut app),
                "{cmd} must expand"
            );
            assert_eq!(app.input.text(), format!("{cmd} "));
            assert_eq!(app.slash_selected, 0);
        }
    }

    #[test]
    fn enter_expansion_leaves_other_input_untouched() {
        // Argument-less commands (`/clear`), partial prefixes (`/mod`), and
        // inputs that already carry an argument keep the old path: the Enter
        // handler's completion step (not the bare-command expansion) owns
        // them.
        for input in ["/clear", "/mod", "/model foo", "/resume 0", "hello"] {
            let mut app = test_app();
            app.input = crate::ui::input::InputField::from_text(input);
            assert!(
                !crate::ui::slash::expand_bare_command(&mut app),
                "{input} must not expand"
            );
            assert_eq!(app.input.text(), input);
        }
    }

    #[test]
    fn command_prefix_completes_to_bare_picker_form() {
        // `/mod` + completion → `/model `: the Enter handler sees a bare
        // picker form in `EXPAND_ON_ENTER` and holds the submit so the popup
        // stays open for the actual choice.
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text("/mod");
        assert!(crate::ui::slash::complete_slash(&mut app));
        assert_eq!(app.input.text(), "/model ");
        assert!(crate::ui::slash::EXPAND_ON_ENTER.contains(&"/model"));
    }

    #[test]
    fn picker_rows_show_items_not_repeated_commands() {
        // Inside a picker the popup rows show just the item (`gpt-5`), not
        // the repeated command (`/model gpt-5`); outside a picker the
        // command itself stays the label.
        let label = crate::ui::slash::suggestion_label;
        assert_eq!(label("/model ", "/model gpt-5"), "gpt-5");
        assert_eq!(label("/model gp", "/model gpt-5"), "gpt-5");
        assert_eq!(label("/provider ", "/provider opencode"), "opencode");
        assert_eq!(label("/resume ", "/resume 0"), "0");
        assert_eq!(label("/resume", "/resume 2"), "2");
        assert_eq!(label("/resume old", "/resume 2"), "2");
        assert_eq!(label("/", "/model"), "/model");
        assert_eq!(label("/cl", "/clear"), "/clear");
    }

    #[test]
    fn esc_dismiss_discards_draft_and_closes_popup() {
        // Esc on an open popup discards the drafted slash command (which is
        // what closes the popup — it is derived from the input) and resets
        // the highlight, without submitting anything.
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text("/mod");
        app.slash_selected = 2;
        assert!(crate::ui::slash::dismiss_slash(&mut app));
        assert_eq!(app.input.text(), "");
        assert_eq!(app.slash_selected, 0);
        assert!(crate::ui::slash::slash_suggestions(&app).is_empty());
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn slash_typing_narrows_to_matching_command() {
        // `/` lists every command; typing `r` narrows to `/resume` — the
        // popup filters on the input text, arrows only move the highlight.
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text("/");
        let all = crate::ui::slash::slash_suggestions(&app);
        assert!(all.len() > 1);
        app.input = crate::ui::input::InputField::from_text("/r");
        let filtered = crate::ui::slash::slash_suggestions(&app);
        assert_eq!(
            filtered.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
            vec!["/resume"]
        );
    }

    #[test]
    fn resume_lists_sessions_regardless_of_content() {
        // `/resume` picks by index/time: a header-only session (no messages
        // yet) is listed too — resuming it just shows an empty transcript.
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-resume-empty-{}", std::process::id()));
        let _env =
            crate::session::EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
        std::env::set_var("XDG_DATA_HOME", &dir);
        let session =
            crate::session::Session::new("/tmp".into(), Some("empty-one".into())).unwrap();
        let name = session.name().unwrap().to_string();
        drop(session);
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text("/resume ");
        let suggestions = crate::ui::slash::slash_suggestions(&app);
        assert!(
            suggestions.iter().any(|(_, desc)| desc.contains(&name)),
            "header-only session must be listed: {suggestions:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_dismiss_reports_when_no_popup_open() {
        // Plain text (or empty input) shows no popup: nothing to discard.
        for input in ["hello", ""] {
            let mut app = test_app();
            app.input = crate::ui::input::InputField::from_text(input);
            assert!(!crate::ui::slash::dismiss_slash(&mut app));
            assert_eq!(app.input.text(), input);
        }
    }
}
