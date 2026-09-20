use super::input::InputField;
use super::selection::b64;
use super::selection::Selection;
use super::selection::SetClipboard;
use super::slash;
use crate::agent::state::ToolState;
use crate::llm::config::LlmConfig;
use crate::session::Session;
use crossterm::Command;
use ratatui::layout::Rect;
use ratatui::text::Line;
use std::cell::Cell;
use std::fmt;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

/// Raised-surface colors are resolved in `ui/theme.rs` from the terminal's
/// own palette / detected background, so they follow the terminal theme.
/// Max markdown re-parse rate while streaming (`markdown_lines` +
/// tree-sitter runs once per window, not once per token). Wall-clock, not
/// tick-based, so it stays constant when the frame rate changes.
pub(crate) const STREAM_FLUSH_INTERVAL: Duration = Duration::from_millis(120);

/// Cap for stored streamed-thinking text (§29): the expanded (Ctrl+T) wrap
/// is linear per flush, so unbounded growth made long reasoning quadratic
/// over the turn. The oldest reasoning drops; the collapsed indicator is
/// unaffected.
pub(crate) const THINKING_TEXT_CAP: usize = 32 * 1024;
/// Cut back to the cap only once past cap + slack, so the O(n) head drain
/// is amortized over many deltas instead of paid per delta while over cap.
pub(crate) const THINKING_TEXT_SLACK: usize = 4 * 1024;

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
        /// The LLM tool-call id from `ToolInput`, pairing this block with
        /// its `ToolOutput`. Empty for legacy inputs (session rebuild of
        /// tool messages predates ids only when `tool_call_id` is absent);
        /// empty-id outputs attach to the tail block as before.
        tool_id: String,
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
    /// Pre-rendered block for the session-start DEX banner. One block, so it
    /// renders without the blank gap line `TranscriptView` inserts between
    /// blocks.
    Banner {
        stamp: u64,
        lines: Vec<Line<'static>>,
    },
}

impl TranscriptBlock {
    /// Content stamp: bumped on every mutation after construction so the
    /// per-block display cache (`App.wrapped_cache`) can tell a changed block
    /// from a stable one without re-wrapping the whole transcript.
    pub(crate) fn stamp(&self) -> u64 {
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

    pub(crate) fn bump(&mut self) {
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
    pub(crate) stamp: u64,
    pub(crate) rows: Vec<Line<'static>>,
    /// Incremental expanded-thinking state (§29): bytes of thinking `text`
    /// already reflected in `rows` (`src_len`), of which the last source
    /// line (`open_len` bytes → `open_rows` rows) may still be open. Only
    /// meaningful when `expanded`; stored text is append-only below the
    /// cap, so rows before the open line are final and only the appended
    /// tail re-wraps per flush.
    pub(crate) src_len: usize,
    pub(crate) open_len: usize,
    pub(crate) open_rows: usize,
    /// `rows` were built by the expanded-thinking path. Ctrl+T bumps stamps
    /// (`bump_thinking_stamps`), so a mode flip never incrementally extends
    /// rows built for the other mode — but the flag is the guard that makes
    /// that airtight.
    pub(crate) expanded: bool,
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
    pub(crate) messages: Vec<crate::protocol::ChatMessage>,
    pub(crate) tool_state: ToolState,
    pub(crate) session: Session,
    pub(crate) skills: Vec<crate::protocol::Skill>,
    pub(crate) turn_start: usize,
    pub(crate) cwd: String,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_dirty: bool,
    pub(crate) steering_rx: Option<mpsc::Receiver<String>>,
    pub(crate) followup_rx: Option<mpsc::Receiver<String>>,
    pub(crate) pending_steering: Vec<String>,
    pub(crate) pending_followups: Vec<String>,
    pub(crate) cancel_requested: bool,
    /// Busy Ctrl+C presses since the current cancel started; the third press
    /// force-quits (stuck daemon). Reset wherever `cancel_requested` resets.
    pub(crate) cancel_presses: u8,
    pub(crate) approval_rx: Option<mpsc::Receiver<crate::protocol::ApprovalRequest>>,
    /// Waiting approval prompts, oldest first (V1b): child agents can park
    /// several at once and they outlive the parent turn, so the old
    /// single-slot overwrite-deny invariant is a queue now. The overlay
    /// renders the front; a decision pops it and reveals the next.
    pub(crate) pending_approvals: Vec<PendingApproval>,
    /// Live child agents (V1b typed events, §15).
    pub(crate) agents: Vec<AgentChip>,
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
    /// of the status bar; the only other session-start block is the DEX banner.
    pub(crate) connection: Option<String>,
    /// Base URL of the backing daemon, when this TUI is remote: extension
    /// status/reload must reach the process that dispatches (`/extensions`
    /// in a remote TUI goes to the daemon API, not the client's manager).
    /// `None` for local/loopback turns, which own their manager.
    pub(crate) daemon_url: Option<String>,
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
    pub(crate) plan: crate::protocol::Plan,
    /// Assistant deltas buffered between throttle windows. `markdown_lines`
    /// (term-md + tree-sitter) runs on the buffer at most once per
    /// `STREAM_FLUSH_INTERVAL` instead of once per token; `flush_assistant`
    /// drains it into the tail `Assistant` block.
    pub(crate) assistant_pending: String,
    /// Markdown gap state for the open assistant block. Survives throttled
    /// `flush_assistant` clears of `assistant_pending` so a heading/list at a
    /// window edge keeps its top air; reset on every fresh assistant block.
    pub(crate) assistant_gap: crate::ui::theme::markdown::GapState,
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
    /// Cached fallback context estimate for the status bar (perf doc §29):
    /// `status::status_tokens()` serves it while the history fingerprint
    /// (count + per-message role/length/head-tail samples) is unchanged,
    /// so frames (keystrokes, SSE batches, 8fps busy ticks) never re-walk
    /// the transcript until the first provider-reported `Usage` arrives.
    pub(crate) status_tokens_cache: Cell<(usize, u64, u64)>,
    /// Cached slash-popup listing, keyed per input change (§29) — see
    /// `slash::SlashCache`. `RefCell` (not a plain field) so the
    /// `&App` render path can memoize without signature ripples; never
    /// borrowed reentrantly (the compute path never calls back in).
    pub(crate) slash_cache: std::cell::RefCell<Option<slash::SlashCache>>,
}

#[cfg(test)]
impl App {
    /// Shared test constructor: a hermetic in-memory session, no connection,
    /// no skills, model "test". ui.rs and ui/slash.rs tests both build on it.
    pub(crate) fn test_app() -> App {
        App {
            transcript: Vec::new(),
            input: crate::ui::input::InputField::new(),
            config: crate::llm::config::LlmConfig {
                provider: crate::protocol::Provider::OpenCode,
                api_key: String::new(),
                base_url: String::new(),
                model: "test".into(),
                available_models: vec!["test".into()],
                endpoints: Default::default(),
                api: crate::protocol::ApiProtocol::Responses,
                account_id: None,
                thinking_effort: None,
                context_window: 128_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
                permission: crate::protocol::PermissionMode::Trusted,
                verify_command: None,
                extra_headers: Default::default(),
                global_headers: Default::default(),
                provider_entries: Default::default(),
                provider_headers: Default::default(),
                api_pinned: false,
                connect_timeout_secs: 10,
                request_timeout_secs: 300,
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
            cancel_presses: 0,
            approval_rx: None,
            pending_approvals: Vec::new(),
            agents: Vec::new(),
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
            daemon_url: None,
            assistant_open: false,
            show_thinking: false,
            thinking_open: false,
            plan: crate::protocol::Plan::default(),
            assistant_pending: String::new(),
            assistant_gap: crate::ui::theme::markdown::GapState::new(),
            stream_last_flush: Instant::now(),
            wrapped_cache: Vec::new(),
            wrapped_width: 0,
            display_cache: Vec::new(),
            transcript_area: None,
            selection: None,
            notice: None,
            status_tokens_cache: Cell::new((0, 0, 0)),
            slash_cache: std::cell::RefCell::new(None),
        }
    }
}

impl App {
    pub(crate) fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        self.slash_selected = 0;
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
        self.slash_selected = 0;
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
    pub(crate) response: tokio::sync::mpsc::Sender<crate::protocol::ApprovalDecision>,
    pub(crate) selected: usize,
    /// The child agent's definition name (V1b, §12): the prompt renders
    /// labeled ("explorer wants to run bash: …").
    pub(crate) agent: Option<String>,
    /// Render-ready copy, parsed once at enqueue (§29): the overlay used to
    /// re-parse the same `input` JSON 3–4× per frame while pending.
    pub(crate) title: &'static str,
    pub(crate) summary: String,
    pub(crate) details: Vec<String>,
    pub(crate) risk_label: &'static str,
    pub(crate) risk_color: ratatui::style::Color,
}

impl PendingApproval {
    pub(crate) fn new(
        name: String,
        input: String,
        response: tokio::sync::mpsc::Sender<crate::protocol::ApprovalDecision>,
        agent: Option<String>,
    ) -> Self {
        let has_then_run = crate::ui::format::input_has_then_run(&input);
        let title = crate::ui::format::approval_title_with_then_run(&name, has_then_run);
        let summary = crate::ui::format::approval_summary(&name, &input);
        let details = crate::ui::format::approval_details(&name, &input);
        let (risk_label, risk_color) =
            crate::ui::format::approval_risk_with_then_run(&name, has_then_run);
        Self {
            name,
            response,
            selected: 0,
            agent,
            title,
            summary,
            details,
            risk_label,
            risk_color,
        }
    }
}

/// One live child agent, from the V1b typed lifecycle events (§15): the
/// status-bar chip. Entries arrive at `AgentSpawned`, update on
/// `AgentProgress`, and drop at `AgentCompleted`.
#[derive(Clone, Debug)]
pub(crate) struct AgentChip {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) tool: Option<String>,
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    match tokens {
        // A trailing ".0" is wasted width in the status bar: 12.0k -> 12k.
        t if t >= 1_000_000 => format!("{:.1}M", t as f64 / 1_000_000.0).replace(".0M", "M"),
        t if t >= 1_000 => format!("{:.1}k", t as f64 / 1_000.0).replace(".0k", "k"),
        t => t.to_string(),
    }
}

pub(crate) struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        use crossterm::event::{
            DisableBracketedPaste, DisableMouseCapture, PopKeyboardEnhancementFlags,
        };
        let _ = crossterm::execute!(
            std::io::stdout(),
            // Undoes the Kitty disambiguate push from the TUI setup; harmless
            // on terminals that never supported it.
            PopKeyboardEnhancementFlags,
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
