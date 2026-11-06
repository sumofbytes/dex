//! Spike: a native dex GUI on Rune. Embeds the dex daemon in-process and talks to it over
//! HTTP+SSE via `dex-client`; `DEX_GUI_URL` attaches to an existing daemon instead.
//! Headless render: `cargo run -- --snap out.png --scene chat`.

use std::sync::{Arc, Mutex, OnceLock};

use dex_client::protocol::{ApprovalDecision, StreamEvent};
use dex_client::{ChatOptions, DaemonClient};
use rune::tok::{self, type_};
use rune::{AppSpec, Cx, El, ScrollSpec, TextEdit, Tx, Waker, col, el, row, text, text_input};
use tokio::sync::mpsc;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    User,
    Assistant,
    Tool,
    Error,
}

struct Line {
    kind: Kind,
    text: String,
}

#[derive(Default)]
struct Inbox {
    events: Vec<Msg>,
}

enum Msg {
    Event(StreamEvent),
    Status(String),
}

struct State {
    edit: TextEdit,
    lines: Vec<Line>,
    busy: bool,
    status: String,
    inbox: Arc<Mutex<Inbox>>,
    prompts: Option<mpsc::UnboundedSender<String>>,
}

impl Default for State {
    fn default() -> Self {
        State {
            edit: TextEdit::default(),
            lines: Vec::new(),
            busy: false,
            status: "connecting…".into(),
            inbox: Arc::default(),
            prompts: None,
        }
    }
}

static INBOX: OnceLock<Arc<Mutex<Inbox>>> = OnceLock::new();

fn push_line(s: &mut State, kind: Kind, text: String) {
    // Assistant deltas arrive as markdown lines; coalesce into one block per turn.
    if kind == Kind::Assistant
        && let Some(last) = s.lines.last_mut()
        && last.kind == Kind::Assistant
    {
        last.text.push_str(&text);
        return;
    }
    s.lines.push(Line { kind, text });
}

fn apply(s: &mut State, msg: Msg) {
    match msg {
        Msg::Status(t) => s.status = t,
        Msg::Event(ev) => match ev {
            StreamEvent::AssistantText(t) => push_line(s, Kind::Assistant, t),
            StreamEvent::ToolCall { name, args, .. } => {
                let a = args.to_string();
                let a: String = a.chars().take(160).collect();
                push_line(s, Kind::Tool, format!("▸ {name} {a}"));
            }
            StreamEvent::ToolResult {
                name,
                summary,
                success,
                ..
            } => {
                let mark = if success { "✓" } else { "✗" };
                push_line(s, Kind::Tool, format!("{mark} {name}: {summary}"));
            }
            StreamEvent::ApprovalRequired { name, .. } => {
                push_line(
                    s,
                    Kind::Error,
                    format!("denied {name}: approvals are not wired in the spike"),
                );
            }
            StreamEvent::TurnComplete { .. } => {
                s.busy = false;
                s.status = "ready".into();
            }
            StreamEvent::TurnFailed { error } => {
                s.busy = false;
                s.status = "ready".into();
                push_line(s, Kind::Error, error);
            }
            _ => {}
        },
    }
}

fn submit(s: &mut State, _: &mut Tx) {
    let prompt = s.edit.text.trim().to_string();
    if prompt.is_empty() || s.busy {
        return;
    }
    s.edit.replace_text("");
    s.lines.push(Line {
        kind: Kind::User,
        text: prompt.clone(),
    });
    s.busy = true;
    s.status = "working…".into();
    if let Some(tx) = &s.prompts {
        let _ = tx.send(prompt);
    }
}

fn bubble(l: &Line) -> El {
    let t = tok::theme(true);
    let (color, mono) = match l.kind {
        Kind::User => (t.text, false),
        Kind::Assistant => (t.text, false),
        Kind::Tool => (t.text_secondary, true),
        Kind::Error => (tok::accent(1, true), false),
    };
    let _ = mono;
    let body = text(l.text.clone()).font(type_::BODY).color(color);
    let b = col().w_full().padding(10.0).radius(8.0).child(body);
    if l.kind == Kind::User {
        b.fill(t.control_off)
    } else {
        b
    }
}

fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(true);
    let mut list = col().w_full().gap(8.0).padding(12.0);
    for (i, l) in s.lines.iter().enumerate() {
        list = list.child(bubble(l).key(format!("line{i}")));
    }
    let input = text_input(&s.edit, type_::BODY.lh(22.0))
        .key("prompt")
        .grow(1.0)
        .on_edit(|s: &mut State| &mut s.edit)
        .on_submit(submit);
    let bar = row()
        .w_full()
        .padding(10.0)
        .gap(8.0)
        .fill(t.control_off)
        .radius(8.0)
        .child(input);
    let status = text(s.status.clone())
        .font(type_::BODY)
        .color(t.text_secondary);
    col()
        .w_full()
        .h_full()
        .fill(t.bg)
        .padding(12.0)
        .gap(8.0)
        .child(
            list.scroll(ScrollSpec { stick_bottom: true })
                .grow(1.0)
                .min_h(0.0),
        )
        .child(bar)
        .child(el().child(status))
}

/// Run the dex daemon on a background thread of this process, on a pre-bound loopback port.
fn start_embedded_daemon() -> std::io::Result<std::net::SocketAddr> {
    dex_server::agent::delegate::set_daemon_linked(true);
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio");
        rt.block_on(async {
            if let Err(e) = dex_server::daemon::run_daemon(listener).await {
                eprintln!("daemon error: {e}");
            }
        });
    });
    Ok(addr)
}

fn setup(s: &mut State, wake: Waker) {
    let inbox = s.inbox.clone();
    let _ = INBOX.set(inbox.clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    s.prompts = Some(tx);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio");
        rt.block_on(async move {
            let post = |m: Msg| {
                inbox.lock().unwrap().events.push(m);
                wake();
            };
            // DEX_GUI_URL attaches to an existing daemon; otherwise embed one in this process.
            let url = match std::env::var("DEX_GUI_URL") {
                Ok(url) => url,
                Err(_) => match start_embedded_daemon() {
                    Ok(addr) => format!("http://{addr}"),
                    Err(e) => return post(Msg::Status(format!("embedded daemon failed: {e}"))),
                },
            };
            let client = match DaemonClient::new(&url) {
                Ok(c) => c,
                Err(e) => return post(Msg::Status(format!("bad url: {e}"))),
            };
            if let Err(e) = client
                .wait_until_ready_async(std::time::Duration::from_secs(15))
                .await
            {
                return post(Msg::Status(format!("daemon not ready: {e}")));
            }
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let session = match client.create_session_async(&cwd, Some("dex-gui")).await {
                Ok(r) => r.session_id,
                Err(e) => return post(Msg::Status(format!("no daemon at {url}: {e}"))),
            };
            post(Msg::Status("ready".into()));
            while let Some(prompt) = rx.recv().await {
                let mut stream = match client
                    .chat_stream(&session, &prompt, ChatOptions::default())
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        post(Msg::Event(StreamEvent::TurnFailed {
                            error: e.to_string(),
                        }));
                        continue;
                    }
                };
                while let Some(item) = stream.next_event().await {
                    match item {
                        Ok(ev) => {
                            if let StreamEvent::ApprovalRequired { request_id, .. } = &ev {
                                let _ = client
                                    .approve_async(&session, request_id, ApprovalDecision::Deny)
                                    .await;
                            }
                            let done = matches!(
                                ev,
                                StreamEvent::TurnComplete { .. } | StreamEvent::TurnFailed { .. }
                            );
                            post(Msg::Event(ev));
                            if done {
                                break;
                            }
                        }
                        Err(e) => {
                            post(Msg::Event(StreamEvent::TurnFailed { error: e }));
                            break;
                        }
                    }
                }
            }
        });
    });
}

fn poll(s: &mut State, _: f64, _: &mut Tx) -> bool {
    let msgs: Vec<Msg> = std::mem::take(&mut s.inbox.lock().unwrap().events);
    let changed = !msgs.is_empty();
    for m in msgs {
        apply(s, m);
    }
    changed
}

fn sample(s: &mut State) {
    s.status = "ready".into();
    s.lines.push(Line {
        kind: Kind::User,
        text: "list the crates in this repo".into(),
    });
    s.lines.push(Line {
        kind: Kind::Tool,
        text: "▸ ls {\"path\":\"crates\"}".into(),
    });
    s.lines.push(Line {
        kind: Kind::Tool,
        text: "✓ ls: 11 entries".into(),
    });
    s.lines.push(Line {
        kind: Kind::Assistant,
        text: "There are 11 crates, including dex-client, dex-server and dex-acp.".into(),
    });
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[("chat", sample)],
        setup: Some(setup),
        poll: Some(poll),
        ..AppSpec::new("dex", (900.0, 640.0), State::default, view)
    });
}
