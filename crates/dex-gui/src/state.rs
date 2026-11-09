//! App state and the pure transitions that fold daemon events into the transcript.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dex_client::protocol::{ApprovalDecision, Question, QuestionAnswer, SessionInfo, StreamEvent};
use rune::{TextEdit, Waker};
use rune_kit::diff::{DiffState, parse};
use tokio::sync::mpsc;

/// UI -> worker commands.
pub enum Cmd {
    NewSession,
    Open(String),
    /// Rename a thread: Return ended an in-place sidebar edit.
    Rename {
        id: String,
        name: String,
    },
    Send {
        cwd: String,
        text: String,
        model: Option<String>,
        permission: Option<String>,
        /// First message of a new thread: work in a fresh git worktree.
        worktree: bool,
    },
    Cancel,
    /// Mid-turn steering: the agent reads it before its next model call.
    Steer(String),
    /// A message that runs as a chained turn after the current one.
    Followup(String),
    /// Take a queued message back (the daemon drops it if it has not injected it yet).
    Recall {
        text: String,
        followup: bool,
    },
    Approve {
        request_id: String,
        decision: ApprovalDecision,
    },
    /// Answer the parked `ask_user` batch (`answers[i]` ↔ its `questions[i]`).
    Answer {
        request_id: String,
        answers: Vec<QuestionAnswer>,
    },
}

/// Worker -> UI messages, drained on the UI thread by `poll`.
pub enum Msg {
    Ready(Info),
    Sessions(Vec<SessionInfo>),
    /// A session just created: it lists before the next refresh catches it.
    Upsert(SessionInfo),
    /// A session was opened (or created): replayed history follows as `Event`s.
    Opened {
        id: String,
        replay: Vec<StreamEvent>,
    },
    Event(StreamEvent),
    /// An event from a running turn, tagged with its thread (which may not be the open one).
    Turn {
        id: String,
        ev: StreamEvent,
    },
    TurnStarted(String),
    Status(String),
    /// The directory the current thread works in changed (a worktree was made).
    Project(String),
    /// Project files (relative) and the working-tree patch from a background `git` scan.
    /// One line of the terminal's running command's output.
    TermLine(String),
    /// The terminal's command ended with this exit code after `secs`.
    TermDone {
        code: i32,
        secs: f64,
    },
    Scanned {
        cwd: String,
        files: Vec<String>,
        patch: String,
    },
}

#[derive(Clone, Default)]
pub struct Info {
    pub provider: String,
    pub model: String,
    pub models: Vec<String>,
    pub permission: String,
    pub cwd: String,
    pub branch: Option<String>,
}

pub struct ToolRun {
    pub id: String,
    pub name: String,
    pub args: String,
    pub done: Option<ToolDone>,
    pub open: bool,
}

pub struct ToolDone {
    pub summary: String,
    pub ok: bool,
    pub preview: Vec<String>,
    pub secs: f64,
}

pub enum Item {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
    },
    Thinking {
        text: String,
        open: bool,
        /// When the first chunk arrived; `secs` is set once something else follows it.
        started: Option<std::time::Instant>,
        secs: Option<u64>,
    },
    Tool(ToolRun),
    Notice(String),
    Error(String),
    Approval {
        request_id: String,
        name: String,
        input: String,
        decided: Option<ApprovalDecision>,
    },
    Question {
        request_id: String,
        questions: Vec<Question>,
        /// Toggled option indices per question (pick order kept): `Choice`
        /// for a single-select slot, `Multi` for a checkbox one.
        picks: Vec<Vec<usize>>,
        /// Set once the `ask_user` tool result lands or the turn fails:
        /// what the slots resolved to, so the picker is no longer live.
        decided: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq)]
pub enum Page {
    Home,
    Thread,
}

pub struct Queued {
    pub text: String,
    pub followup: bool,
}

/// How many command blocks the terminal keeps, and lines kept per block.
pub const TERM_BLOCKS: usize = 100;
pub const TERM_LINES: usize = 2000;
/// Saved terminal commands kept.
pub const TERM_HISTORY: usize = 1000;

/// One command the terminal ran: the command, its output lines, and how it ended.
pub struct TermBlock {
    pub cmd: String,
    pub lines: Vec<String>,
    pub status: TermStatus,
}

pub enum TermStatus {
    Running { started: std::time::Instant },
    Done { code: i32, secs: f64 },
}

#[derive(Clone, Copy, PartialEq)]
pub enum VTab {
    Files,
    Changes,
    Terminal,
}

/// How far the sidebar and the viewer pane stretch when you drag their dividers. Ranges that
/// leave the transcript room in a 1100-point window (DESIGN.md's widths are the starting
/// points: `sidebar::tok::WIDTH` 280 and `viewer::WIDTH` 460).
pub const SIDE_MIN: f32 = 220.0;
pub const SIDE_MAX: f32 = 440.0;
pub const VIEW_MIN: f32 = 340.0;
pub const VIEW_MAX: f32 = 640.0;

pub struct OpenFile {
    pub path: String,
    pub text: String,
}

#[derive(Default)]
pub struct Inbox(pub Mutex<Vec<Msg>>);

/// Why a thread is listed under NEEDS YOU.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NeedKind {
    /// An approval or a question is waiting.
    Waiting,
    /// A background turn finished while the thread was not open.
    Done,
    Failed,
}

pub struct Need {
    pub id: String,
    pub kind: NeedKind,
    /// The prompt for a waiting thread; done and failed rows show the thread title.
    pub label: Option<String>,
}

/// Where the open thread is in its turn: `Idle` -> `Sending` (prompt submitted, daemon has not
/// confirmed) -> `Working` (the thread is in `State::running`) -> `Idle`. `Working` is not stored
/// here: it is membership in `running`, so the two can never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    Idle,
    Sending,
    Working,
}

pub struct State {
    /// Threads with a turn running (open or not).
    pub running: HashSet<String>,
    /// The open thread submitted a prompt and waits for `TurnStarted` (the `Sending` phase).
    pub sending: bool,
    /// Threads waiting on the user, newest first.
    pub needs: Vec<Need>,
    pub sessions: Vec<SessionInfo>,
    pub sessions_loaded: bool,
    pub current: Option<String>,
    pub items: Vec<Item>,
    pub edit: TextEdit,
    /// The composer text as of the last poll: a change asks for one more frame (see `poll`).
    pub edit_seen: String,
    pub status: String,
    pub info: Info,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub open_select: Option<&'static str>,
    pub tokens: u64,
    pub cost: f64,
    pub sidebar: bool,
    /// The sidebar's width; its divider stretches it.
    pub sidebar_w: f32,
    pub page: Page,
    pub queue: Vec<Queued>,
    pub search: TextEdit,
    pub searching: bool,
    /// The thread being renamed (its sidebar row's title is an editable field).
    pub renaming: Option<String>,
    /// The rename field's text and caret.
    pub rename: TextEdit,
    /// Project (working directory) new threads start in; follows the opened thread.
    pub project: Option<String>,
    pub collapsed: HashSet<String>,
    /// Tool-call groups opened in the transcript, by the index of their first item.
    pub open_work: HashSet<usize>,
    pub show_all: HashSet<String>,
    pub all_projects: bool,
    /// Right-hand viewer pane (files, changes).
    /// Threads and projects removed from the sidebar. Only hidden: the sessions stay on disk.
    pub hidden_threads: HashSet<String>,
    pub hidden_projects: HashSet<String>,
    pub theme_open: bool,
    /// The model menu's search text.
    pub model_query: TextEdit,
    pub theme_query: TextEdit,
    pub use_worktree: bool,
    pub confirm_discard: bool,
    pub viewer: bool,
    /// The viewer pane's width; its divider stretches it.
    pub viewer_w: f32,
    pub vtab: VTab,
    pub files: Vec<String>,
    pub file_filter: TextEdit,
    pub open_file: Option<OpenFile>,
    pub diff: DiffState,
    pub scanned: Option<String>,
    pub term: Vec<TermBlock>,
    pub term_input: TextEdit,
    /// Commands run in the terminal, oldest first, saved in the data directory.
    pub term_history: Vec<String>,
    /// While the input walks history: where it is and the input's text before the walk
    /// started (the text Ctrl+R then searches for).
    pub term_hist: Option<(usize, String)>,
    pub term_cwd: Option<String>,
    pub wake: Option<Waker>,
    pub inbox: Arc<Inbox>,
    pub cmds: Option<mpsc::UnboundedSender<Cmd>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            running: HashSet::new(),
            needs: Vec::new(),
            sessions: Vec::new(),
            sessions_loaded: false,
            current: None,
            items: Vec::new(),
            edit: TextEdit::default(),
            edit_seen: String::new(),
            sending: false,
            status: "starting…".into(),
            info: Info::default(),
            model: None,
            permission: None,
            open_select: None,
            tokens: 0,
            cost: 0.0,
            sidebar: true,
            sidebar_w: crate::sidebar::tok::WIDTH,
            page: Page::Home,
            queue: Vec::new(),
            search: TextEdit::default(),
            searching: false,
            renaming: None,
            rename: TextEdit::default(),
            project: None,
            collapsed: HashSet::new(),
            open_work: HashSet::new(),
            show_all: HashSet::new(),
            all_projects: false,
            hidden_threads: HashSet::new(),
            hidden_projects: HashSet::new(),
            theme_open: false,
            model_query: TextEdit::default(),
            theme_query: TextEdit::default(),
            use_worktree: false,
            confirm_discard: false,
            viewer: false,
            viewer_w: crate::viewer::WIDTH,
            vtab: VTab::Files,
            files: Vec::new(),
            file_filter: TextEdit::default(),
            open_file: None,
            diff: DiffState::new(Vec::new()),
            scanned: None,
            term: Vec::new(),
            term_input: TextEdit::default(),
            term_history: Vec::new(),
            term_hist: None,
            term_cwd: None,
            wake: None,
            inbox: Arc::default(),
            cmds: None,
        }
    }
}

impl State {
    /// Clicking an option of the transcript question at item `i`: single-select
    /// questions replace the pick; `multiSelect` ones toggle it (pick order kept).
    pub fn pick_question(&mut self, i: usize, q: usize, o: usize) {
        let Some(Item::Question {
            picks, questions, ..
        }) = self.items.get_mut(i)
        else {
            return;
        };
        let multi = questions.get(q).is_some_and(|x| x.multi_select);
        if multi {
            let slot = &mut picks[q];
            match slot.iter().position(|&x| x == o) {
                Some(p) => {
                    slot.remove(p);
                }
                None => slot.push(o),
            }
        } else {
            picks[q] = vec![o];
        }
    }

    pub fn send_cmd(&self, c: Cmd) {
        if let Some(tx) = &self.cmds {
            let _ = tx.send(c);
        }
    }

    /// Directory new threads run in and the footer shows.
    /// The open thread's turn phase.
    pub fn turn(&self) -> Turn {
        if self
            .current
            .as_ref()
            .is_some_and(|c| self.running.contains(c))
        {
            Turn::Working
        } else if self.sending {
            Turn::Sending
        } else {
            Turn::Idle
        }
    }

    /// A turn is in flight on the open thread (sending or working).
    pub fn busy(&self) -> bool {
        self.turn() != Turn::Idle
    }

    /// A terminal command is running: the last block has not ended.
    pub fn term_running(&self) -> bool {
        matches!(
            self.term.last().map(|b| &b.status),
            Some(TermStatus::Running { .. })
        )
    }

    pub fn cwd(&self) -> String {
        self.project
            .clone()
            .unwrap_or_else(|| self.info.cwd.clone())
    }

    /// Whether a thread is listed: not hidden itself, nor under a hidden project.
    pub fn is_visible(&self, sess: &SessionInfo) -> bool {
        !self.hidden_threads.contains(&sess.session_id)
            && !self
                .hidden_projects
                .contains(&crate::worktree::project_root(&sess.cwd))
    }

    pub fn hide_thread(&mut self, id: &str) {
        self.hidden_threads.insert(id.to_string());
        self.save_hidden();
    }

    pub fn hide_project(&mut self, root: &str) {
        self.hidden_projects.insert(root.to_string());
        self.save_hidden();
    }

    /// Switch colour scheme and remember it in the data directory.
    pub fn set_theme(&mut self, name: &str) {
        if crate::theme::set(name)
            && let Some(dir) = crate::worktree::data_dir().to_str()
        {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(std::path::Path::new(dir).join("gui-theme"), name);
        }
    }

    /// Apply the scheme saved last time, if any.
    pub fn load_theme(&self) {
        let path = crate::worktree::data_dir().join("gui-theme");
        if let Ok(name) = std::fs::read_to_string(path) {
            crate::theme::set(name.trim());
        }
    }

    /// Read the hidden set from the data directory, if it was saved before.
    pub fn load_hidden(&mut self) {
        let Some(text) = hidden_file().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let set = |key: &str| -> HashSet<String> {
            v.get(key)
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        self.hidden_threads = set("threads");
        self.hidden_projects = set("projects");
    }

    fn save_hidden(&self) {
        let Some(path) = hidden_file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let v = serde_json::json!({
            "threads": self.hidden_threads,
            "projects": self.hidden_projects,
        });
        let _ = std::fs::write(path, v.to_string());
    }

    /// Double-click on a thread row: its title becomes an editable field.
    pub fn start_rename(&mut self, id: &str) {
        let title = self
            .sessions
            .iter()
            .find(|s| s.session_id == id)
            .map(session_title)
            .unwrap_or_default();
        self.rename = TextEdit::new(title);
        self.rename.anchor = 0; // caret at the end: the whole title is selected
        self.rename.focused = true;
        self.renaming = Some(id.to_string());
    }

    /// Return ended the edit: send the name to the daemon (an empty name keeps the old one).
    pub fn commit_rename(&mut self) {
        self.rename.focused = false;
        if let Some(id) = self.renaming.take() {
            let name = self.rename.text.trim().to_string();
            if !name.is_empty() {
                self.send_cmd(Cmd::Rename { id, name });
            }
        }
    }

    /// Escape put the row back: keep the old title.
    pub fn cancel_rename(&mut self) {
        self.rename.focused = false;
        self.renaming = None;
    }

    pub fn effective_model(&self) -> String {
        self.model
            .clone()
            .unwrap_or_else(|| self.info.model.clone())
    }

    pub fn effective_permission(&self) -> String {
        self.permission
            .clone()
            .unwrap_or_else(|| self.info.permission.clone())
    }

    pub fn title(&self) -> String {
        self.sessions
            .iter()
            .find(|s| Some(&s.session_id) == self.current.as_ref())
            .map(session_title)
            .unwrap_or_else(|| "New session".into())
    }

    fn reset_transcript(&mut self) {
        self.items.clear();
        self.open_work.clear();
        self.tokens = 0;
        self.cost = 0.0;
        self.sending = false;
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Ready(info) => {
                self.info = info;
                self.status = "ready".into();
            }
            Msg::Sessions(list) => {
                let mut list = list;
                // A thread that is running or open must not vanish because the daemon's listing
                // does not include it yet (nothing journaled).
                for old in &self.sessions {
                    let id = &old.session_id;
                    let live = self.running.contains(id) || self.current.as_deref() == Some(id);
                    if live && !list.iter().any(|x| x.session_id == *id) {
                        list.push(old.clone());
                    }
                }
                list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                self.sessions = list;
                self.sessions_loaded = true;
            }
            Msg::Upsert(sess) => {
                self.sessions.retain(|x| x.session_id != sess.session_id);
                self.sessions.insert(0, sess);
                self.sessions
                    .sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            }
            Msg::Status(s) => self.status = s,
            Msg::Project(p) => self.project = Some(p),
            Msg::TermLine(line) => {
                if let Some(b) = self.term.last_mut() {
                    b.lines.push(strip_ansi(line.trim_end_matches('\r')));
                    let n = b.lines.len();
                    if n > TERM_LINES {
                        b.lines.drain(..n - TERM_LINES);
                    }
                }
            }
            Msg::TermDone { code, secs } => {
                if let Some(b) = self.term.last_mut() {
                    b.status = TermStatus::Done { code, secs };
                }
            }
            Msg::Scanned { cwd, files, patch } => {
                if cwd == self.cwd() {
                    self.files = files;
                    self.diff.set_files(parse(&patch));
                    self.scanned = Some(cwd);
                }
            }
            Msg::Opened { id, replay } => {
                // Leaving a thread that still has an unanswered approval keeps it in NEEDS YOU.
                let old = self.current.clone().filter(|o| !o.is_empty() && *o != id);
                let unanswered = self.items.iter().any(|i| {
                    matches!(
                        i,
                        Item::Approval { decided: None, .. } | Item::Question { decided: None, .. }
                    )
                });
                if let Some(old) = old
                    && unanswered
                    && self.running.contains(&old)
                {
                    self.need(old, NeedKind::Waiting, Some("Waiting for approval".into()));
                }
                self.needs.retain(|n| n.id != id);
                self.reset_transcript();
                self.queue.clear();
                self.page = Page::Thread;
                self.current = Some(id);
                for ev in replay {
                    self.apply_event(ev);
                }
                self.finish_stream();
            }
            Msg::Event(ev) => self.apply_event(ev),
            Msg::TurnStarted(id) => {
                self.needs.retain(|n| n.id != id);
                // For a brand-new thread `Opened` came first and saw nothing running yet.
                if self.current.as_deref() == Some(id.as_str()) {
                    self.sending = false;
                }
                self.running.insert(id);
            }
            Msg::Turn { id, ev } => self.apply_turn(id, ev),
        }
    }

    /// Adds (or replaces) a NEEDS YOU row, newest first.
    fn need(&mut self, id: String, kind: NeedKind, label: Option<String>) {
        self.needs.retain(|n| n.id != id);
        self.needs.insert(0, Need { id, kind, label });
    }

    /// An event from a running turn: the open thread folds it into the transcript, any other
    /// thread only updates its sidebar state.
    fn apply_turn(&mut self, id: String, ev: StreamEvent) {
        if self.current.as_deref() == Some(id.as_str()) {
            // The turn ends here unless a queued follow-up chains another one on the same stream.
            let ends = match &ev {
                StreamEvent::TurnComplete { .. } => !self.queue.iter().any(|q| q.followup),
                StreamEvent::TurnFailed { .. } => true,
                _ => false,
            };
            self.apply_event(ev);
            if ends {
                self.running.remove(&id);
            }
            return;
        }
        match ev {
            StreamEvent::ApprovalRequired { name, .. } => {
                self.need(id, NeedKind::Waiting, Some(format!("Allow {name}?")));
            }
            StreamEvent::QuestionRequired { questions, .. } => {
                let q = questions.first().map(|q| q.question.clone());
                self.need(id, NeedKind::Waiting, q);
            }
            StreamEvent::TurnComplete { .. } => {
                self.running.remove(&id);
                self.need(id, NeedKind::Done, None);
            }
            StreamEvent::TurnFailed { .. } => {
                self.running.remove(&id);
                self.need(id, NeedKind::Failed, None);
            }
            _ => {}
        }
    }

    fn finish_stream(&mut self) {
        for it in &mut self.items {
            if let Item::Assistant { streaming, .. } = it {
                *streaming = false;
            }
        }
    }

    /// Stamps the trailing thought with how long it took, once something else follows it.
    fn finish_thinking(&mut self) {
        if let Some(Item::Thinking {
            started: Some(t),
            secs: secs @ None,
            ..
        }) = self.items.last_mut()
        {
            *secs = Some(t.elapsed().as_secs());
        }
    }

    pub fn apply_event(&mut self, ev: StreamEvent) {
        if !matches!(ev, StreamEvent::Thinking(_)) {
            self.finish_thinking();
        }
        match ev {
            StreamEvent::UserPrompt { content } => self.items.push(Item::User(content)),
            StreamEvent::AssistantText(t) => {
                if let Some(Item::Assistant { text, streaming }) = self.items.last_mut() {
                    text.push_str(&t);
                    *streaming = true;
                } else {
                    self.items.push(Item::Assistant {
                        text: t,
                        streaming: true,
                    });
                }
            }
            StreamEvent::Thinking(t) => {
                if let Some(Item::Thinking { text, .. }) = self.items.last_mut() {
                    text.push_str(&t);
                } else {
                    self.items.push(Item::Thinking {
                        text: t,
                        open: false,
                        started: Some(std::time::Instant::now()),
                        secs: None,
                    });
                }
            }
            StreamEvent::ToolCall { name, args, id } => {
                self.finish_stream();
                self.items.push(Item::Tool(ToolRun {
                    id,
                    name,
                    args: args_line(&args),
                    done: None,
                    open: false,
                }));
            }
            StreamEvent::ToolResult {
                name,
                summary,
                success,
                preview,
                duration,
                id,
            } => {
                let is_ask = name == "ask_user" && !summary.is_empty();
                let ask_summary = summary.clone();
                let done = ToolDone {
                    summary,
                    ok: success,
                    preview,
                    secs: duration,
                };
                let slot = self.items.iter_mut().rev().find_map(|it| match it {
                    Item::Tool(t)
                        if t.done.is_none()
                            && (t.id == id || (id.is_empty() && t.name == name)) =>
                    {
                        Some(t)
                    }
                    _ => None,
                });
                match slot {
                    Some(t) => t.done = Some(done),
                    None => self.items.push(Item::Tool(ToolRun {
                        id,
                        name,
                        args: String::new(),
                        done: Some(done),
                        open: false,
                    })),
                }
                if is_ask
                    && let Some(Item::Question { decided, .. }) =
                        self.items.iter_mut().rev().find_map(|it| match it {
                            Item::Question { decided, .. } if decided.is_none() => Some(it),
                            _ => None,
                        })
                {
                    *decided = Some(ask_summary);
                }
            }
            StreamEvent::ApprovalRequired {
                request_id,
                name,
                input,
                ..
            } => {
                self.items.push(Item::Approval {
                    request_id,
                    name,
                    input,
                    decided: None,
                });
            }
            StreamEvent::QuestionRequired {
                request_id,
                questions,
                ..
            } => {
                let picks = vec![Vec::new(); questions.len()];
                self.items.push(Item::Question {
                    request_id,
                    questions,
                    picks,
                    decided: None,
                });
            }
            StreamEvent::SteeringAccepted { content }
            | StreamEvent::FollowupAccepted { content } => {
                let t = content.trim();
                if let Some(i) = self
                    .queue
                    .iter()
                    .position(|q| q.text == content || q.text.trim() == t)
                {
                    self.queue.remove(i);
                }
                self.items.push(Item::User(content));
            }
            StreamEvent::System(s) => self.items.push(Item::Notice(s)),
            StreamEvent::Error(e) => self.items.push(Item::Error(e)),
            StreamEvent::Usage { tokens, cost, .. } => {
                self.tokens = tokens;
                self.cost += cost;
            }
            StreamEvent::TurnComplete { .. } => {
                self.finish_stream();
                self.sending = false;
                self.status = "ready".into();
            }
            StreamEvent::TurnFailed { error } => {
                self.finish_stream();
                self.sending = false;
                self.status = "ready".into();
                // A failed turn's parked `ask_user` never produces a tool
                // result: close its unanswered pickers too.
                for it in &mut self.items {
                    if let Item::Question { decided, .. } = it
                        && decided.is_none()
                    {
                        *decided = Some("No answer".into());
                    }
                }
                self.items.push(Item::Error(error));
            }
            _ => {}
        }
    }

    /// Drag the sidebar's edge `dx` from where the pointer pressed. The drag strip reverts to
    /// `start` itself when the gesture is cancelled (Escape, the window losing focus).
    pub fn drag_sidebar(&mut self, start: f32, dx: f32) {
        self.sidebar_w = (start + dx).clamp(SIDE_MIN, SIDE_MAX).round();
    }

    /// Same for the viewer pane's edge, which widens as the pointer moves left (`-dx`).
    pub fn drag_viewer(&mut self, start: f32, dx: f32) {
        self.viewer_w = (start - dx).clamp(VIEW_MIN, VIEW_MAX).round();
    }

    /// Show the viewer on `tab` (or hide it when it already shows that tab) and rescan.
    pub fn show_viewer(&mut self, tab: VTab) {
        if self.viewer && self.vtab == tab {
            self.viewer = false;
            return;
        }
        self.viewer = true;
        self.vtab = tab;
        self.rescan();
    }

    /// List the project's files and its uncommitted changes on a background thread.
    pub fn rescan(&self) {
        let (cwd, inbox, wake) = (self.cwd(), self.inbox.clone(), self.wake.clone());
        std::thread::spawn(move || {
            inbox.0.lock().unwrap().push(scan(cwd));
            if let Some(w) = wake {
                w();
            }
        });
    }

    /// Read the saved terminal command history, if there was one.
    pub fn load_term_history(&mut self) {
        let Some(path) = term_history_file() else {
            return;
        };
        if let Ok(text) = std::fs::read_to_string(path) {
            self.term_history = text.lines().map(str::to_string).collect();
        }
    }

    /// Remember a command in the history (adjacent repeats dropped) and save it.
    pub fn push_term_history(&mut self, cmd: &str) {
        if self.term_history.last().map(String::as_str) == Some(cmd) {
            return;
        }
        self.term_history.push(cmd.to_string());
        let n = self.term_history.len();
        if n > TERM_HISTORY {
            self.term_history.drain(..n - TERM_HISTORY);
        }
        let Some(path) = term_history_file() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, self.term_history.join("\n"));
    }

    /// Put history entry `i` in the input, caret at the end.
    fn recall_term(&mut self, i: usize) {
        if let Some(t) = self.term_history.get(i) {
            self.term_input.replace_text(t.clone());
        }
    }

    /// Up: one step back through the history, from the input's own text.
    pub fn term_hist_up(&mut self) {
        if self.term_history.is_empty() {
            return;
        }
        let hist = self.term_hist.take();
        let (mut pos, draft) = match hist {
            Some((p, d)) => (p, d),
            None => (self.term_history.len(), self.term_input.text.clone()),
        };
        if pos > 0 {
            pos -= 1;
            let t = self.term_history[pos].clone();
            self.term_input.replace_text(t);
        }
        self.term_hist = Some((pos, draft));
    }

    /// Down: forward through the history, past the newest back to the unsent text.
    pub fn term_hist_down(&mut self) {
        let Some((pos, draft)) = self.term_hist.take() else {
            return;
        };
        match self.term_history.get(pos + 1) {
            Some(t) => {
                let t = t.clone();
                self.term_hist = Some((pos + 1, draft));
                self.term_input.replace_text(t);
            }
            None => {
                self.term_input.replace_text(draft); // past the newest: back to the unsent draft
            }
        }
    }

    /// Ctrl+R: the most recent command containing the field's text — the text the walk started
    /// from, once a match is shown. Again finds the next older match; an empty field behaves
    /// like Up.
    pub fn term_hist_search(&mut self) {
        let (pos, query) = match &self.term_hist {
            Some((p, q)) => (*p, q.clone()),
            None if self.term_input.text.trim().is_empty() => {
                self.term_hist_up();
                return;
            }
            None => (self.term_history.len(), self.term_input.text.clone()),
        };
        if let Some(i) = (0..pos)
            .rev()
            .find(|&i| self.term_history[i].contains(&query))
        {
            self.term_hist = Some((i, query));
            self.recall_term(i);
        }
    }

    /// Run the terminal input as a shell command in the project directory, streaming its output
    /// into a new block. Not a PTY: interactive programs (editors, pagers) do not work.
    pub fn run_term(&mut self) {
        let cmd = self.term_input.text.trim().to_string();
        if cmd.is_empty() || self.term_running() {
            return;
        }
        self.term_hist = None;
        self.push_term_history(&cmd);
        self.term_input.replace_text("");
        let dir = self.term_cwd.clone().unwrap_or_else(|| self.cwd());
        if self.term.len() >= TERM_BLOCKS {
            self.term.remove(0);
        }
        if let Some(arg) = cmd.strip_prefix("cd ").or((cmd == "cd").then_some("~")) {
            let arg = arg.trim();
            let home = std::env::var("HOME").unwrap_or_default();
            let target = match arg.strip_prefix('~') {
                Some(rest) => format!("{home}{rest}"),
                None => arg.to_string(),
            };
            let (lines, code) = match std::path::Path::new(&dir).join(target).canonicalize() {
                Ok(p) if p.is_dir() => {
                    self.term_cwd = Some(p.display().to_string());
                    (Vec::new(), 0)
                }
                _ => (vec![format!("cd: {arg}: no such directory")], 1),
            };
            self.term.push(TermBlock {
                cmd,
                lines,
                status: TermStatus::Done { code, secs: 0.0 },
            });
            return;
        }
        self.term.push(TermBlock {
            cmd: cmd.clone(),
            lines: Vec::new(),
            status: TermStatus::Running {
                started: std::time::Instant::now(),
            },
        });
        let (inbox, wake) = (self.inbox.clone(), self.wake.clone());
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
            let started = std::time::Instant::now();
            let post = |m: Msg| {
                inbox.0.lock().unwrap().push(m);
                if let Some(w) = &wake {
                    w();
                }
            };
            #[cfg(unix)]
            let mut c = {
                let mut c = Command::new("sh");
                c.arg("-c").arg(format!("exec 2>&1; {cmd}"));
                c
            };
            #[cfg(windows)]
            let mut c = {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(format!("{cmd} 2>&1"));
                c
            };
            let child = c
                .current_dir(&dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            match child {
                Ok(mut ch) => {
                    if let Some(out) = ch.stdout.take() {
                        for l in BufReader::new(out).lines().map_while(Result::ok) {
                            post(Msg::TermLine(l));
                        }
                    }
                    let code = ch.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
                    post(Msg::TermDone {
                        code,
                        secs: started.elapsed().as_secs_f64(),
                    });
                }
                Err(e) => {
                    post(Msg::TermLine(format!("failed to run: {e}")));
                    post(Msg::TermDone {
                        code: -1,
                        secs: 0.0,
                    });
                }
            }
        });
    }

    /// Forget the terminal's history, unless a command is still running.
    pub fn clear_term(&mut self) {
        if !self.term_running() {
            self.term.clear();
        }
    }

    /// Merge (or discard) the current thread's worktree and return to the main project.
    pub fn finish_worktree(&mut self, merge: bool) {
        self.confirm_discard = false;
        let Some(w) = crate::worktree::info(&self.cwd()) else {
            return;
        };
        let r = if merge {
            crate::worktree::merge(&w)
        } else {
            crate::worktree::discard(&w)
        };
        match r {
            Ok(m) => {
                self.project = Some(w.repo);
                self.current = None;
                self.items.clear();
                self.send_cmd(Cmd::NewSession);
                self.items.push(Item::Notice(m));
                self.rescan();
            }
            Err(e) => self.items.push(Item::Error(e)),
        }
    }

    pub fn open_path(&mut self, rel: &str) {
        let full = std::path::Path::new(&self.cwd()).join(rel);
        let text = match std::fs::read(&full) {
            Ok(b) if b.len() > 512 * 1024 => "File too large to preview.".into(),
            Ok(b) => String::from_utf8(b).unwrap_or_else(|_| "Binary file.".into()),
            Err(e) => format!("Cannot read: {e}"),
        };
        self.open_file = Some(OpenFile {
            path: rel.to_string(),
            text,
        });
        self.viewer = true;
        self.vtab = VTab::Files;
    }

    /// Toggle every tool call's details open (or all closed when they already are).
    pub fn toggle_tools(&mut self) {
        let open = self
            .items
            .iter()
            .any(|i| matches!(i, Item::Tool(t) if !t.open));
        for it in &mut self.items {
            if let Item::Tool(t) = it {
                t.open = open;
            }
        }
        // Open or close every work block along with the rows.
        self.open_work.clear();
        if open {
            self.open_work.extend(work_run_starts(&self.items));
        }
    }

    pub fn toggle_thinking(&mut self) {
        let open = self
            .items
            .iter()
            .any(|i| matches!(i, Item::Thinking { open: false, .. }));
        for it in &mut self.items {
            if let Item::Thinking { open: o, .. } = it {
                *o = open;
            }
        }
    }

    /// Alt+Enter: queue the message to run after the current turn.
    pub fn queue_followup(&mut self) {
        let text = self.edit.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if !self.busy() {
            return self.submit();
        }
        self.edit.replace_text("");
        self.queue.push(Queued {
            text: text.clone(),
            followup: true,
        });
        self.send_cmd(Cmd::Followup(text));
    }

    /// Take queued message `i` back into the input to edit it.
    pub fn recall(&mut self, i: usize) {
        if i >= self.queue.len() {
            return;
        }
        let q = self.queue.remove(i);
        self.send_cmd(Cmd::Recall {
            text: q.text.clone(),
            followup: q.followup,
        });
        self.edit.replace_text(q.text);
    }

    pub fn submit(&mut self) {
        let text = self.edit.text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.busy() {
            // Enter mid-turn steers: the agent reads it before its next model call.
            self.edit.replace_text("");
            self.queue.push(Queued {
                text: text.clone(),
                followup: false,
            });
            self.send_cmd(Cmd::Steer(text));
            return;
        }
        self.page = Page::Thread;
        self.edit.replace_text("");
        self.items.push(Item::User(text.clone()));
        self.sending = true;
        self.status = "working…".into();
        self.send_cmd(Cmd::Send {
            cwd: self.cwd(),
            text,
            model: self.model.clone(),
            permission: self.permission.clone(),
            worktree: self.use_worktree && self.current.is_none(),
        });
    }
}

fn hidden_file() -> Option<std::path::PathBuf> {
    Some(crate::worktree::data_dir().join("gui-hidden.json"))
}

/// Where the terminal's command history is saved.
fn term_history_file() -> Option<std::path::PathBuf> {
    Some(crate::worktree::data_dir().join("gui-term-history"))
}

/// The index of the first item of each run of consecutive tool calls.
pub fn work_run_starts(items: &[Item]) -> Vec<usize> {
    (0..items.len())
        .filter(|&i| is_work(&items[i]) && (i == 0 || !is_work(&items[i - 1])))
        .collect()
}

/// A thought or a tool call: what folds into a "Worked" block.
pub fn is_work(it: &Item) -> bool {
    matches!(it, Item::Thinking { .. } | Item::Tool(_))
}

/// A transcript question's POST payload: `answers[i]` ↔ its `questions[i]`;
/// an untouched slot dismisses.
pub fn question_answers(questions: &[Question], picks: &[Vec<usize>]) -> Vec<QuestionAnswer> {
    questions
        .iter()
        .zip(picks)
        .map(|(q, pick)| match pick.as_slice() {
            [only, ..] if !q.multi_select => QuestionAnswer::Choice(*only),
            [] => QuestionAnswer::Dismiss,
            _ => QuestionAnswer::Multi(pick.clone()),
        })
        .collect()
}

/// What a run of work items did: `(thoughts, commands, seconds)`. Seconds add the finished
/// thoughts' and tool calls' own durations.
pub fn work_summary(items: &[Item]) -> (usize, usize, f64) {
    let (mut thoughts, mut commands, mut secs) = (0, 0, 0.0);
    for it in items {
        match it {
            Item::Thinking { secs: t, .. } => {
                thoughts += 1;
                secs += t.unwrap_or(0) as f64;
            }
            Item::Tool(t) => {
                commands += 1;
                secs += t.done.as_ref().map_or(0.0, |d| d.secs);
            }
            _ => {}
        }
    }
    (thoughts, commands, secs)
}

/// A project's display name: the last path component.
pub fn project_name(cwd: &str) -> String {
    let t = cwd.trim_end_matches('/');
    t.rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("/")
        .to_string()
}

pub fn session_title(s: &SessionInfo) -> String {
    // A chosen name wins over the first prompt, but a generated one never
    // shows: once the original prompt is out of the transcript (`clear` then
    // a compaction summary, which is never a title), falling back to the
    // generated name would put `dex-gui-<rand>` strings in the sidebar.
    let t = s
        .name
        .clone()
        .filter(|n| !n.is_empty() && !s.auto_named)
        .or_else(|| s.first_prompt.clone())
        .unwrap_or_else(|| "Untitled".into());
    t.lines()
        .next()
        .unwrap_or("Untitled")
        .chars()
        .take(80)
        .collect()
}

/// One-line summary of a tool call's arguments for its collapsed row.
fn args_line(args: &serde_json::Value) -> String {
    let s = match args {
        serde_json::Value::Object(m) => {
            let pick = ["command", "path", "pattern", "query", "file_path"]
                .iter()
                .find_map(|k| m.get(*k).and_then(|v| v.as_str()));
            match pick {
                Some(v) => v.to_string(),
                None => args.to_string(),
            }
        }
        other => other.to_string(),
    };
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' {
            if it.peek() == Some(&'[') {
                it.next();
                for n in it.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `git` listing of a project's files and its working-tree patch against HEAD.
pub fn scan(cwd: String) -> Msg {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let files = git(&["ls-files", "--cached", "--others", "--exclude-standard"])
        .lines()
        .map(str::to_string)
        .collect();
    let patch = git(&["diff", "HEAD", "--no-color"]);
    Msg::Scanned { cwd, files, patch }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_thread_is_busy_after_its_turn_starts() {
        let mut s = State::default();
        s.apply(Msg::Opened {
            id: "n".into(),
            replay: Vec::new(),
        });
        assert!(!s.busy());
        s.apply(Msg::TurnStarted("n".into()));
        assert!(s.busy() && s.running.contains("n"));
        s.apply(Msg::Turn {
            id: "n".into(),
            ev: StreamEvent::TurnComplete {
                response: String::new(),
                usage: None,
                cached: None,
            },
        });
        assert!(!s.busy() && !s.running.contains("n"));
    }

    #[test]
    fn refresh_keeps_running_threads() {
        let mut s = State::default();
        let sess: SessionInfo = serde_json::from_value(serde_json::json!({
            "session_id": "new", "path": "", "cwd": "/w", "created_at": "", "updated_at": "",
            "auto_named": false, "message_count": 0
        }))
        .unwrap();
        s.apply(Msg::Upsert(sess));
        s.running.insert("new".into());
        s.apply(Msg::Sessions(Vec::new()));
        assert_eq!(s.sessions.len(), 1);
        s.running.clear();
        s.apply(Msg::Sessions(Vec::new()));
        assert!(s.sessions.is_empty());
    }

    #[test]
    fn background_turns_land_in_needs_you() {
        let mut s = State {
            current: Some("a".into()),
            ..State::default()
        };
        s.running.insert("b".into());
        s.apply(Msg::Turn {
            id: "b".into(),
            ev: StreamEvent::ApprovalRequired {
                request_id: "r".into(),
                name: "bash".into(),
                input: String::new(),
                agent: None,
            },
        });
        assert!(s.needs[0].kind == NeedKind::Waiting && s.running.contains("b"));
        s.apply(Msg::Turn {
            id: "b".into(),
            ev: StreamEvent::TurnComplete {
                response: String::new(),
                usage: None,
                cached: None,
            },
        });
        assert!(s.needs.len() == 1 && s.needs[0].kind == NeedKind::Done);
        assert!(!s.running.contains("b"));
        // Opening the thread clears its row.
        s.apply(Msg::Opened {
            id: "b".into(),
            replay: Vec::new(),
        });
        assert!(s.needs.is_empty());
    }

    #[test]
    fn generated_names_never_reach_the_sidebar() {
        let sess = |name: Option<&str>, auto: bool, prompt: Option<&str>| {
            serde_json::from_value::<SessionInfo>(serde_json::json!({
                "session_id": "s", "path": "", "cwd": "/x", "updated_at": "",
                "name": name, "auto_named": auto,
                "first_prompt": prompt, "message_count": 1
            }))
            .unwrap()
        };
        // A generated name with no prompt in the transcript shows Untitled,
        // not `dex-gui-<rand>`.
        assert_eq!(
            session_title(&sess(Some("dex-gui-k3m9x2q"), true, None)),
            "Untitled"
        );
        // A chosen name and the first prompt still win, in that order.
        assert_eq!(
            session_title(&sess(Some("Fancy"), false, Some("hi there"))),
            "Fancy"
        );
        assert_eq!(
            session_title(&sess(Some("dex-gui-k3m9x2q"), true, Some("hi there"))),
            "hi there"
        );
        assert_eq!(session_title(&sess(None, false, None)), "Untitled");
    }

    #[test]
    fn pane_widths_follow_the_pointer() {
        let mut s = State::default();
        assert_eq!(s.sidebar_w, crate::sidebar::tok::WIDTH);
        assert_eq!(s.viewer_w, crate::viewer::WIDTH);
        s.drag_sidebar(s.sidebar_w, 60.0);
        assert_eq!(s.sidebar_w, 340.0);
        s.drag_sidebar(s.sidebar_w, 400.0); // clamped at the stretch limit
        assert_eq!(s.sidebar_w, SIDE_MAX);
        s.drag_sidebar(s.sidebar_w, -600.0); // clamped at the collapse limit
        assert_eq!(s.sidebar_w, SIDE_MIN);
        s.drag_viewer(s.viewer_w, -80.0); // the pointer moves left: the pane widens
        assert_eq!(s.viewer_w, 540.0);
        s.drag_viewer(s.viewer_w, -600.0); // clamped at the stretch limit
        assert_eq!(s.viewer_w, VIEW_MAX);
    }

    #[test]
    fn tool_calls_group_into_runs() {
        let tool = || {
            Item::Tool(ToolRun {
                id: String::new(),
                name: "bash".into(),
                args: String::new(),
                done: None,
                open: false,
            })
        };
        let items = vec![
            Item::User("a".into()),
            Item::Thinking {
                text: "t".into(),
                open: false,
                started: None,
                secs: Some(2),
            },
            tool(),
            tool(),
            Item::Assistant {
                text: "b".into(),
                streaming: false,
            },
            tool(),
        ];
        assert_eq!(work_run_starts(&items), vec![1, 5]);
        assert_eq!(work_summary(&items[1..4]), (1, 2, 2.0));
    }

    #[test]
    fn a_new_thread_lists_before_the_next_refresh() {
        let sess = |id: &str, updated: &str| -> SessionInfo {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "path": "", "cwd": "/x" , "updated_at": updated,
                "message_count": 1, "auto_named": false
            }))
            .unwrap()
        };
        let mut s = State::default();
        s.apply(Msg::Sessions(vec![sess("old", "2026-11-05T00:00:00Z")]));
        s.apply(Msg::Upsert(sess("new", "2026-11-07T00:00:00Z")));
        assert_eq!(s.sessions[0].session_id, "new");
        assert!(s.sessions.iter().any(|x| x.session_id == "old"));
        // A full listing replaces the placeholder with real values.
        s.apply(Msg::Sessions(vec![sess("old", "2026-11-05T00:00:00Z")]));
        assert!(!s.sessions.iter().any(|x| x.session_id == "new"));
    }

    #[test]
    fn hidden_threads_and_projects_are_not_visible() {
        let sess = |id: &str, cwd: &str| -> SessionInfo {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "path": "", "cwd": cwd, "updated_at": "2026-11-06T00:00:00Z",
                "auto_named": false, "message_count": 1
            }))
            .unwrap()
        };
        let mut s = State::default();
        let a = sess("a", "/x/one");
        let b = sess("b", "/x/two");
        assert!(s.is_visible(&a) && s.is_visible(&b));
        s.hidden_threads.insert("a".into());
        assert!(!s.is_visible(&a) && s.is_visible(&b));
        s.hidden_projects.insert("/x/two".into());
        assert!(!s.is_visible(&b));
    }

    #[test]
    fn terminal_runs_a_command_and_streams_output() {
        let mut s = State {
            project: Some(std::env::temp_dir().display().to_string()),
            ..State::default()
        };
        s.term_input.replace_text("echo hi; echo err >&2");
        s.run_term();
        assert!(s.term_running());
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let msgs: Vec<Msg> = std::mem::take(&mut *s.inbox.0.lock().unwrap());
            for m in msgs {
                s.apply(m);
            }
            if !s.term_running() {
                break;
            }
        }
        assert!(!s.term_running());
        assert_eq!(s.term.len(), 1);
        assert_eq!(s.term[0].lines, ["hi", "err"]);
        assert!(matches!(s.term[0].status, TermStatus::Done { code: 0, .. }));
    }

    #[test]
    fn terminal_input_walks_history() {
        let mut s = State::default();
        s.push_term_history("echo a");
        s.push_term_history("echo a"); // adjacent repeat dropped
        s.push_term_history("echo b");
        assert_eq!(s.term_history, ["echo a", "echo b"]);
        s.term_input.replace_text("echo c");
        s.term_hist_up();
        assert_eq!(s.term_input.text, "echo b");
        s.term_hist_up();
        assert_eq!(s.term_input.text, "echo a");
        s.term_hist_up(); // oldest entry: no further step
        assert_eq!(s.term_input.text, "echo a");
        s.term_hist_down();
        assert_eq!(s.term_input.text, "echo b");
        s.term_hist_down(); // past the newest: back to the unsent draft
        assert_eq!(s.term_input.text, "echo c");
    }

    #[test]
    fn terminal_input_searches_history() {
        let mut s = State::default();
        for c in ["cargo build", "cargo test", "git status"] {
            s.push_term_history(c);
        }
        s.term_input.replace_text("test");
        s.term_hist_search();
        assert_eq!(s.term_input.text, "cargo test");
        s.term_hist_search(); // no older match containing "test": the field keeps it
        assert_eq!(s.term_input.text, "cargo test");
        s.term_hist_search(); // the query is the pre-search text, not the shown command
        assert_eq!(s.term_input.text, "cargo test");
    }

    #[test]
    fn terminal_search_walks_older_matches() {
        let mut s = State::default();
        for c in ["cargo build", "cargo test one", "cargo test two"] {
            s.push_term_history(c);
        }
        s.term_input.replace_text("test");
        s.term_hist_search();
        assert_eq!(s.term_input.text, "cargo test two");
        s.term_hist_search();
        assert_eq!(s.term_input.text, "cargo test one");
    }
}

#[test]
fn questions_fold_pick_and_answer() {
    use dex_client::protocol::{Question, QuestionOption};
    let q = |multi: bool| Question {
        question: "Which database?".into(),
        header: "Database".into(),
        options: vec![
            QuestionOption {
                label: "postgres".into(),
                description: "default".into(),
            },
            QuestionOption {
                label: "sqlite".into(),
                description: "".into(),
            },
        ],
        multi_select: multi,
        default: None,
    };
    let questions = vec![q(false), q(true)];
    let mut s = State::default();
    s.apply(Msg::Event(StreamEvent::QuestionRequired {
        request_id: "q1".into(),
        questions: questions.clone(),
        agent: None,
    }));
    let Some(Item::Question { picks, .. }) = s.items.last() else {
        panic!("question item");
    };
    assert_eq!(picks, &[Vec::<usize>::new(), Vec::new()]);
    // Single-select replaces; checkbox questions toggle in pick order.
    s.pick_question(0, 0, 1);
    s.pick_question(0, 0, 0);
    s.pick_question(0, 1, 1);
    s.pick_question(0, 1, 2);
    s.pick_question(0, 1, 1);
    s.pick_question(0, 1, 2);
    assert_eq!(
        crate::state::question_answers(&questions, &[vec![0], vec![1, 2]]),
        vec![QuestionAnswer::Choice(0), QuestionAnswer::Multi(vec![1, 2]),]
    );
    // An untouched slot dismisses.
    assert_eq!(
        crate::state::question_answers(&questions, &[vec![], vec![]]),
        vec![QuestionAnswer::Dismiss, QuestionAnswer::Dismiss]
    );
}

#[test]
fn question_result_and_failure_close_the_picker() {
    let mut s = State::default();
    let park = |s: &mut State, id: &str| {
        s.apply(Msg::Event(StreamEvent::QuestionRequired {
            request_id: id.into(),
            questions: vec![dex_client::protocol::Question {
                question: "Why?".into(),
                header: "Why".into(),
                options: vec![
                    dex_client::protocol::QuestionOption {
                        label: "a".into(),
                        description: "".into(),
                    },
                    dex_client::protocol::QuestionOption {
                        label: "b".into(),
                        description: "".into(),
                    },
                ],
                multi_select: false,
                default: None,
            }],
            agent: None,
        }));
    };
    park(&mut s, "q1");
    s.apply(Msg::Event(StreamEvent::ToolResult {
        name: "ask_user".into(),
        summary: "Why: b (user's choice)".into(),
        success: true,
        preview: vec![],
        duration: 0.0,
        id: "1".into(),
    }));
    assert!(matches!(
        s.items.iter().find(|it| matches!(it, Item::Question { .. })),
        Some(Item::Question { decided: Some(d), .. }) if d.contains("b (user's choice)")
    ));
    // A failed turn closes every still-open picker.
    park(&mut s, "q2");
    s.apply(Msg::Event(StreamEvent::TurnFailed {
        error: "boom".into(),
    }));
    let closed: Vec<_> = s
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Question { decided, .. } => Some(decided.is_some()),
            _ => None,
        })
        .collect();
    assert_eq!(closed, [true, true]);
}
