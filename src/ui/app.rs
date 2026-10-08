use super::input::InputField;
use super::selection::b64;
use super::selection::Selection;
use super::selection::SetClipboard;
use super::slash;
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
/// they are inserted by `TranscriptView::render` (`ui/render.rs`) as
/// `BLOCK_GAP_ROWS` blank `Line`s between any two blocks. This makes gutter handling
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
        settled: Option<Settled>,
    },
    Tool {
        stamp: u64,
        /// Tool name (`bash`, `read`, `mcp__srv__tool`): heads the step row.
        name: String,
        /// The call's argument, highlighted, without indent/glyph/name — the
        /// step row is composed at wrap time (the outcome goes on the row beneath).
        input: Line<'static>,
        started: Instant,
        /// Outcome once the call finishes; `None` while it runs.
        result: Option<ToolResult>,
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
    /// One slash-command/notice output: every row of it is one block, so a
    /// list (sessions, help) reads as a list instead of gap-separated rows.
    Info {
        stamp: u64,
        lines: Vec<Line<'static>>,
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

/// How a turn ended; picks the settled summary's wording and color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnTone {
    Done,
    Cancelled,
    Failed,
}

/// The settled turn summary row.
#[derive(Debug, Clone)]
pub(crate) struct Settled {
    pub(crate) text: String,
    pub(crate) tone: TurnTone,
}

/// A finished tool call: outcome glyph/color come from `ok`, the outcome
/// text from `summary`.
#[derive(Debug, Clone)]
pub(crate) struct ToolResult {
    pub(crate) ok: bool,
    pub(crate) summary: String,
    /// Seconds the daemon measured for the call; 0 when unknown.
    pub(crate) duration: f64,
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
            TranscriptBlock::Tool { input, preview, .. } => {
                let mut out = Vec::with_capacity(1 + preview.len());
                out.push(input);
                out.extend(preview.iter());
                out
            }
            TranscriptBlock::System { line, .. } => vec![line],
            TranscriptBlock::Error { line, .. } => vec![line],
            TranscriptBlock::Info { lines, .. } => lines.iter().collect(),
            TranscriptBlock::Banner { lines, .. } => lines.iter().collect(),
        }
    }
}

/// The TUI application state. Rendering lives in `ui/render.rs` (`view`);
/// turn execution lives either in the local engine or, in client-server
/// mode, in `ui/remote.rs` which drives the same state from daemon events.
pub(crate) struct App {
    /// Remote (client-server) TUI: the daemon owns model/provider selection
    /// (routing, credentials, config-file persistence), so `/model` only
    /// updates the local display and the raw selection is forwarded — the
    /// client never writes the config file back (its default provider would
    /// silently repoint the shared config).
    pub(crate) remote_mode: bool,
    pub(crate) transcript: Vec<TranscriptBlock>,
    pub(crate) input: InputField,
    pub(crate) config: LlmConfig,
    pub(crate) messages: Vec<crate::protocol::ChatMessage>,
    pub(crate) tool_state: super::UsageState,
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
    /// Waiting `ask_user` batches, oldest first. The overlay resolves the
    /// front; each entry POSTs its own answer set.
    pub(crate) pending_questions: Vec<PendingQuestionUi>,
    /// Live child agents (V1b typed events, §15).
    pub(crate) agents: Vec<AgentChip>,
    /// Per-child transcript logs (plan §20 child view): scratch `App`s keyed
    /// by agent id, reused purely for their transcript machinery (block
    /// building + wrap caches), so child lines render with the identical
    /// block semantics as the parent transcript. Logs survive the child's
    /// completion; the block cap bounds memory for runaway children.
    pub(crate) child_logs: Vec<ChildLog>,
    /// The open child transcript view: the agent id being shown in the
    /// parent's transcript window (composer and footer stay visible),
    /// `None` while the parent transcript is showing.
    pub(crate) child_view: Option<String>,
    /// Background shell tasks (spec Rev 3): status-bar chips flipped at
    /// `TaskFinished`, plus capped output logs fed by `TaskOutput`.
    pub(crate) tasks: Vec<TaskChip>,
    pub(crate) task_logs: Vec<TaskLog>,
    /// The open task output view (Ctrl+B / `/tasks <id>`): the task id shown
    /// in the transcript window, like `child_view` for agents.
    pub(crate) task_view: Option<String>,
    /// Lines scrolled up from the task log's tail; 0 follows new output.
    pub(crate) task_scroll: usize,
    /// Agents/tasks that finished this turn, named in the activity strip
    /// until [`DONE_FADE`] passes, then folded into `done_tally`. Both reset
    /// when the next turn starts.
    pub(crate) recent_done: Vec<(String, Instant)>,
    pub(crate) done_tally: usize,
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
    /// Whether successful tool calls show their output preview (Ctrl+O);
    /// collapsed (outcome row only) by default. Failures and write/edit
    /// diffs always show theirs.
    pub(crate) expand_tools: bool,
    /// Daemon's estimate of what a fresh session's first turn carries,
    /// shown by `/session`.
    pub(crate) base_context: Vec<crate::protocol::BaseContextPart>,
    /// `tool_state.total_output` when the current turn started, so the
    /// settled summary reports tokens generated *this turn*.
    pub(crate) turn_out_base: u64,
    /// Tool steps in the transcript when the current turn started, so the
    /// settled summary counts this turn's tools (steers add `User` blocks
    /// mid-turn, so the last prompt is no boundary).
    pub(crate) turn_tools_base: usize,
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
    pub(crate) assistant_gap: crate::render::theme::markdown::GapState,
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
    /// Derived from [`App::scratch`] — the one canonical all-defaults `App`
    /// literal — so a new field is initialized in exactly one place.
    pub(crate) fn test_app() -> App {
        let mut app = App::scratch();
        app.config.model = "test".into();
        app.config.available_models = vec!["test".into()];
        app
    }
}

impl App {
    /// The canonical all-defaults `App` literal: a bare scratch `App` used
    /// only as transcript machinery for a child log (plan §20 child view;
    /// tests derive [`App::test_app`] from it), so every non-transcript
    /// field stays at its default and new fields initialize in one place.
    pub(crate) fn scratch() -> App {
        App {
            remote_mode: false,
            transcript: Vec::new(),
            input: crate::ui::input::InputField::new(),
            config: crate::llm::config::LlmConfig {
                provider: crate::protocol::Provider::Anthropic,
                api_key: String::new(),
                base_url: String::new(),
                model: String::new(),
                available_models: Vec::new(),
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
            tool_state: crate::ui::UsageState::default(),
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
            pending_questions: Vec::new(),
            agents: Vec::new(),
            child_logs: Vec::new(),
            child_view: None,
            tasks: Vec::new(),
            task_logs: Vec::new(),
            task_view: None,
            task_scroll: 0,
            recent_done: Vec::new(),
            done_tally: 0,
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
            expand_tools: false,
            base_context: Vec::new(),
            turn_out_base: 0,
            turn_tools_base: 0,
            thinking_open: false,
            plan: crate::protocol::Plan::default(),
            assistant_pending: String::new(),
            assistant_gap: crate::render::theme::markdown::GapState::new(),
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

    /// Fold finished agents/tasks older than [`DONE_FADE`] into the tally,
    /// returning true so the caller redraws the activity strip.
    pub(crate) fn tick_done_fade(&mut self) -> bool {
        let before = self.recent_done.len();
        self.recent_done.retain(|(_, at)| at.elapsed() < DONE_FADE);
        let faded = before - self.recent_done.len();
        self.done_tally += faded;
        faded > 0
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
        let has_then_run = crate::render::format::input_has_then_run(&input);
        let title = crate::render::format::approval_title_with_then_run(&name, has_then_run);
        let summary = crate::render::format::approval_summary(&name, &input);
        let details = crate::render::format::approval_details(&name, &input);
        let (risk_label, risk_color) =
            crate::render::format::approval_risk_with_then_run(&name, has_then_run);
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

/// One pending `ask_user` batch with its wizard state. The overlay owns
/// every key while pending; one sender resolves the whole batch
/// (`answers[i]` ↔ `questions[i]`, unfilled slots submit as `Dismiss`).
pub(crate) struct PendingQuestionUi {
    pub(crate) questions: Vec<crate::protocol::Question>,
    /// Answers recorded so far, filled positionally by the wizard.
    pub(crate) answers: Vec<Option<crate::protocol::QuestionAnswer>>,
    /// Wizard index: which question is showing.
    pub(crate) current: usize,
    /// Selected row of the visible question's menu: the options, then the
    /// implicit "Other" row, then (multiSelect only) the Submit row.
    pub(crate) selected: usize,
    /// Toggle state for the visible question when `multi_select`.
    pub(crate) toggled: Vec<bool>,
    /// Free-text buffer while the "Other" row is being answered.
    pub(crate) text_entry: Option<String>,
    /// True when the summary screen (all questions answered) is showing.
    pub(crate) summary: bool,
    /// The child agent's definition name ("explorer asks: …").
    pub(crate) agent: Option<String>,
    pub(crate) response: tokio::sync::mpsc::Sender<Vec<crate::protocol::QuestionAnswer>>,
}

impl PendingQuestionUi {
    pub(crate) fn new(
        questions: Vec<crate::protocol::Question>,
        agent: Option<String>,
        response: tokio::sync::mpsc::Sender<Vec<crate::protocol::QuestionAnswer>>,
    ) -> Self {
        let toggled = questions
            .first()
            .map(|q| vec![false; q.options.len()])
            .unwrap_or_default();
        Self {
            answers: vec![None; questions.len()],
            current: 0,
            selected: 0,
            toggled,
            text_entry: None,
            summary: false,
            agent,
            questions,
            response,
        }
    }

    /// Row count of the visible question's menu (options + Other [+ Submit]).
    pub(crate) fn menu_rows(&self) -> usize {
        self.questions
            .get(self.current)
            .map(|q| q.options.len() + 1 + usize::from(q.multi_select))
            .unwrap_or(0)
    }

    /// Reset the per-question selection state for the question now showing.
    pub(crate) fn reset_selection(&mut self) {
        self.selected = 0;
        self.text_entry = None;
        self.toggled = self
            .questions
            .get(self.current)
            .map(|q| vec![false; q.options.len()])
            .unwrap_or_default();
    }

    /// Record the current question's answer and advance; on the last
    /// question this shows the summary screen instead of submitting —
    /// Enter there sends the whole batch.
    pub(crate) fn record_and_advance(&mut self, answer: crate::protocol::QuestionAnswer) {
        if let Some(slot) = self.answers.get_mut(self.current) {
            *slot = Some(answer);
        }
        self.text_entry = None;
        if self.current + 1 < self.questions.len() {
            self.current += 1;
            self.reset_selection();
        } else {
            self.summary = true;
        }
    }

    /// The batch's answers, unfilled slots as `Dismiss`.
    pub(crate) fn collected(&self) -> Vec<crate::protocol::QuestionAnswer> {
        self.answers
            .iter()
            .map(|slot| {
                slot.clone()
                    .unwrap_or(crate::protocol::QuestionAnswer::Dismiss)
            })
            .collect()
    }
}

/// One background shell task, from the typed task lifecycle events (spec
/// Rev 3): the status-bar chip. Entries arrive at `TaskStarted` and flip to
/// done at `TaskFinished` (chips persist like agent chips cap discipline).
#[derive(Clone, Debug)]
pub(crate) struct TaskChip {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) done: bool,
}

/// One background task's output log: id + command plus capped raw lines fed
/// by live `TaskOutput` events. Read-only in v1; stopping is the model's job.
pub(crate) struct TaskLog {
    pub(crate) id: String,
    pub(crate) command: String,
    pub(crate) lines: Vec<String>,
}

/// Line cap per task log: a runaway task must not grow client memory.
pub(crate) const TASK_LOG_MAX_LINES: usize = 2000;

/// Cap on retained task logs (mirrors `CHILD_LOG_MAX_LOGS` order).
pub(crate) const TASK_LOG_MAX_LOGS: usize = 16;

/// One live child agent, from the V1b typed lifecycle events (§15): the
/// status-bar chip. Entries arrive at `AgentSpawned`, update on
/// `AgentProgress`, and drop at `AgentCompleted`.
#[derive(Clone, Debug)]
pub(crate) struct AgentChip {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) tool: Option<String>,
}

/// One child agent's transcript log (plan §20 child view): the definition
/// name for the view title plus a scratch [`App`] carrying the transcript
/// blocks and their wrap caches. `App::new`-free — the scratch is built by
/// [`App::scratch`] so every field stays initialized exactly once.
pub(crate) struct ChildLog {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) app: App,
}

/// Block cap per child log: a runaway child must not grow the client's
/// memory without bound. Past the cap the oldest quarter is dropped and the
/// wrap caches cleared (recomputed lazily on the next render).
pub(crate) const CHILD_LOG_MAX_BLOCKS: usize = 2000;

/// Cap on the number of retained child logs: each is a full scratch `App`
/// with caches, so a long session spawning many children must not grow
/// without bound either. Past the cap the oldest log is dropped (recreated
/// empty if that child is still streaming).
pub(crate) const CHILD_LOG_MAX_LOGS: usize = 16;

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

/// How long a finished agent/task stays named in the activity strip before
/// it collapses into the `✓N done` tally.
pub(crate) const DONE_FADE: Duration = Duration::from_secs(10);
