//! App state and the pure transitions that fold daemon events into the transcript.

use std::sync::{Arc, Mutex};

use dex_client::protocol::{ApprovalDecision, SessionInfo, StreamEvent};
use rune::TextEdit;
use tokio::sync::mpsc;

/// UI -> worker commands.
pub enum Cmd {
    NewSession,
    Open(String),
    Send {
        text: String,
        model: Option<String>,
        permission: Option<String>,
    },
    Cancel,
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

#[derive(Default)]
pub struct Inbox(pub Mutex<Vec<Msg>>);

pub struct State {
    pub sessions: Vec<SessionInfo>,
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
    pub inbox: Arc<Inbox>,
    pub cmds: Option<mpsc::UnboundedSender<Cmd>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            sessions: Vec::new(),
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
            }
            Msg::Status(s) => self.status = s,
            Msg::Opened { id, replay } => {
                self.reset_transcript();
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
            StreamEvent::System(s) => self.items.push(Item::Notice(s)),
            StreamEvent::Error(e) => self.items.push(Item::Error(e)),
            StreamEvent::Usage { tokens, cost, .. } => {
                self.tokens = tokens;
                self.cost += cost;
            }
            StreamEvent::TurnComplete { .. } => {
                self.finish_stream();
                self.busy = false;
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

    pub fn submit(&mut self) {
        let text = self.edit.text.trim().to_string();
        if text.is_empty() || self.busy {
            return;
        }
        self.edit.replace_text("");
        self.items.push(Item::User(text.clone()));
        self.busy = true;
        self.status = "working…".into();
        self.send_cmd(Cmd::Send {
            text,
            model: self.model.clone(),
            permission: self.permission.clone(),
        });
    }
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
