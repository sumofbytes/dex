//! App state and the pure transitions that fold daemon events into the transcript.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use dex_client::protocol::{ApprovalDecision, SessionInfo, StreamEvent};
use rune::{TextEdit, Waker};
use rune_kit::diff::{DiffState, parse};
use tokio::sync::mpsc;

/// UI -> worker commands.
pub enum Cmd {
    NewSession,
    Open(String),
    Send {
        cwd: String,
        text: String,
        model: Option<String>,
        permission: Option<String>,
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
}

/// Worker -> UI messages, drained on the UI thread by `poll`.
pub enum Msg {
    Ready(Info),
    Sessions(Vec<SessionInfo>),
    /// A session was opened (or created): replayed history follows as `Event`s.
    Opened {
        id: String,
        replay: Vec<StreamEvent>,
    },
    Event(StreamEvent),
    Status(String),
    /// Project files (relative) and the working-tree patch from a background `git` scan.
    /// Output of the terminal's running command (`None`: it exited).
    Term(Option<String>),
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

#[derive(Clone, Copy, PartialEq)]
pub enum VTab {
    Files,
    Changes,
    Terminal,
}

pub struct OpenFile {
    pub path: String,
    pub text: String,
}

#[derive(Default)]
pub struct Inbox(pub Mutex<Vec<Msg>>);

pub struct State {
    pub sessions: Vec<SessionInfo>,
    pub sessions_loaded: bool,
    pub current: Option<String>,
    pub items: Vec<Item>,
    pub edit: TextEdit,
    pub busy: bool,
    pub status: String,
    pub info: Info,
    pub model: Option<String>,
    pub permission: Option<String>,
    pub open_select: Option<&'static str>,
    pub tokens: u64,
    pub cost: f64,
    pub sidebar: bool,
    pub page: Page,
    pub queue: Vec<Queued>,
    pub search: TextEdit,
    pub searching: bool,
    /// Project (working directory) new threads start in; follows the opened thread.
    pub project: Option<String>,
    pub collapsed: HashSet<String>,
    pub show_all: HashSet<String>,
    pub all_projects: bool,
    /// Right-hand viewer pane (files, changes).
    pub viewer: bool,
    pub vtab: VTab,
    pub files: Vec<String>,
    pub file_filter: TextEdit,
    pub open_file: Option<OpenFile>,
    pub diff: DiffState,
    pub scanned: Option<String>,
    pub term: Vec<String>,
    pub term_input: TextEdit,
    pub term_cwd: Option<String>,
    pub term_running: bool,
    pub wake: Option<Waker>,
    pub inbox: Arc<Inbox>,
    pub cmds: Option<mpsc::UnboundedSender<Cmd>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            sessions: Vec::new(),
            sessions_loaded: false,
            current: None,
            items: Vec::new(),
            edit: TextEdit::default(),
            busy: false,
            status: "starting…".into(),
            info: Info::default(),
            model: None,
            permission: None,
            open_select: None,
            tokens: 0,
            cost: 0.0,
            sidebar: true,
            page: Page::Home,
            queue: Vec::new(),
            search: TextEdit::default(),
            searching: false,
            project: None,
            collapsed: HashSet::new(),
            show_all: HashSet::new(),
            all_projects: false,
            viewer: false,
            vtab: VTab::Files,
            files: Vec::new(),
            file_filter: TextEdit::default(),
            open_file: None,
            diff: DiffState::new(Vec::new()),
            scanned: None,
            term: Vec::new(),
            term_input: TextEdit::default(),
            term_cwd: None,
            term_running: false,
            wake: None,
            inbox: Arc::default(),
            cmds: None,
        }
    }
}

impl State {
    pub fn send_cmd(&self, c: Cmd) {
        if let Some(tx) = &self.cmds {
            let _ = tx.send(c);
        }
    }

    /// Directory new threads run in and the footer shows.
    pub fn cwd(&self) -> String {
        self.project
            .clone()
            .unwrap_or_else(|| self.info.cwd.clone())
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
        self.tokens = 0;
        self.cost = 0.0;
        self.busy = false;
    }

    pub fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Ready(info) => {
                self.info = info;
                self.status = "ready".into();
            }
            Msg::Sessions(list) => {
                let mut list = list;
                list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                self.sessions = list;
                self.sessions_loaded = true;
            }
            Msg::Status(s) => self.status = s,
            Msg::Term(Some(chunk)) => {
                for l in chunk.split('\n') {
                    self.term.push(strip_ansi(l.trim_end_matches('\r')));
                }
                let n = self.term.len();
                if n > 5000 {
                    self.term.drain(..n - 5000);
                }
            }
            Msg::Term(None) => self.term_running = false,
            Msg::Scanned { cwd, files, patch } => {
                if cwd == self.cwd() {
                    self.files = files;
                    self.diff.set_files(parse(&patch));
                    self.scanned = Some(cwd);
                }
            }
            Msg::Opened { id, replay } => {
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
        }
    }

    fn finish_stream(&mut self) {
        for it in &mut self.items {
            if let Item::Assistant { streaming, .. } = it {
                *streaming = false;
            }
        }
    }

    pub fn apply_event(&mut self, ev: StreamEvent) {
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
                // A queued follow-up chains another turn on the same stream.
                self.busy = self.queue.iter().any(|q| q.followup);
                self.status = "ready".into();
            }
            StreamEvent::TurnFailed { error } => {
                self.finish_stream();
                self.busy = false;
                self.status = "ready".into();
                self.items.push(Item::Error(error));
            }
            _ => {}
        }
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

    /// Run the terminal input as a shell command in the project directory, streaming its output.
    /// Not a PTY: interactive programs (editors, pagers) do not work.
    pub fn run_term(&mut self) {
        let cmd = self.term_input.text.trim().to_string();
        if cmd.is_empty() || self.term_running {
            return;
        }
        self.term_input.replace_text("");
        let dir = self.term_cwd.clone().unwrap_or_else(|| self.cwd());
        self.term.push(format!("$ {cmd}"));
        if let Some(arg) = cmd.strip_prefix("cd ").or((cmd == "cd").then_some("~")) {
            let arg = arg.trim();
            let home = std::env::var("HOME").unwrap_or_default();
            let target = match arg.strip_prefix('~') {
                Some(rest) => format!("{home}{rest}"),
                None => arg.to_string(),
            };
            match std::path::Path::new(&dir).join(target).canonicalize() {
                Ok(p) if p.is_dir() => self.term_cwd = Some(p.display().to_string()),
                _ => self.term.push(format!("cd: {arg}: no such directory")),
            }
            return;
        }
        self.term_running = true;
        let (inbox, wake) = (self.inbox.clone(), self.wake.clone());
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            use std::process::{Command, Stdio};
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
                            post(Msg::Term(Some(l)));
                        }
                    }
                    let _ = ch.wait();
                }
                Err(e) => post(Msg::Term(Some(format!("failed to run: {e}")))),
            }
            post(Msg::Term(None));
        });
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
        if !self.busy {
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
        if self.busy {
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
        self.busy = true;
        self.status = "working…".into();
        self.send_cmd(Cmd::Send {
            cwd: self.cwd(),
            text,
            model: self.model.clone(),
            permission: self.permission.clone(),
        });
    }
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
    let t = s
        .name
        .clone()
        .filter(|n| !n.is_empty() && !s.auto_named)
        .or_else(|| s.first_prompt.clone())
        .or_else(|| s.name.clone())
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
    fn terminal_runs_a_command_and_streams_output() {
        let mut s = State::default();
        s.project = Some(std::env::temp_dir().display().to_string());
        s.term_input.replace_text("echo hi; echo err >&2");
        s.run_term();
        assert!(s.term_running);
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let msgs: Vec<Msg> = std::mem::take(&mut *s.inbox.0.lock().unwrap());
            for m in msgs {
                s.apply(m);
            }
            if !s.term_running {
                break;
            }
        }
        assert!(!s.term_running);
        assert_eq!(s.term, ["$ echo hi; echo err >&2", "hi", "err"]);
    }
}
