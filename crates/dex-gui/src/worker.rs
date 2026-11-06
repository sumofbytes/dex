//! Background thread: embedded (or remote) daemon plus a tokio loop that executes `Cmd`s and
//! posts `Msg`s for the UI thread.

use std::sync::Arc;

use dex_client::protocol::StreamEvent;
use dex_client::{ChatOptions, DaemonClient};
use rune::Waker;
use tokio::sync::mpsc;

use crate::state::{Cmd, Inbox, Info, Msg};

/// A clonable handle for posting messages from the per-turn tasks.
#[derive(Clone)]
struct Poster(Arc<dyn Fn(Msg) + Send + Sync>);

impl Poster {
    fn post(&self, m: Msg) {
        (self.0)(m)
    }
}

/// Threads older than this are not listed in the sidebar.
const RECENT_DAYS: i64 = 2;

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

pub fn spawn(inbox: Arc<Inbox>, wake: Waker, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio");
        rt.block_on(async move {
            let poster = Poster(Arc::new(move |m: Msg| {
                inbox.0.lock().unwrap().push(m);
                wake();
            }));
            let post = |m: Msg| poster.post(m);
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
            match client.get_config_async().await {
                Ok(c) => post(Msg::Ready(Info {
                    provider: c.provider,
                    model: c.model,
                    models: c.available_models,
                    permission: c.permission,
                    cwd: if c.cwd.is_empty() { cwd.clone() } else { c.cwd },
                    branch: c.git_branch,
                })),
                Err(e) => post(Msg::Status(format!("config: {e}"))),
            }
            refresh(&client, &post).await;

            let mut session: Option<String> = None;
            // Control channels of the turns that are running, by session id.
            let mut turns: std::collections::HashMap<String, mpsc::UnboundedSender<Cmd>> =
                std::collections::HashMap::new();
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    Cmd::NewSession => {
                        session = None;
                        post(Msg::Opened {
                            id: String::new(),
                            replay: Vec::new(),
                        });
                    }
                    Cmd::Open(id) => match replay(&client, &id).await {
                        Ok(events) => {
                            session = Some(id.clone());
                            post(Msg::Opened { id, replay: events });
                        }
                        Err(e) => post(Msg::Status(format!("open failed: {e}"))),
                    },
                    Cmd::Rename { id, name } => {
                        // A rename during a streamed turn is refused (409); the retry is
                        // the next drag into the field.
                        if let Err(e) = client.rename_session_async(&id, &name).await {
                            post(Msg::Status(format!("rename failed: {e}")));
                        }
                        // The sidebar and the thread header show the stored names.
                        refresh(&client, &post).await;
                    }
                    Cmd::Send {
                        cwd,
                        text,
                        model,
                        permission,
                        worktree,
                    } => {
                        let id = match &session {
                            Some(id) => id.clone(),
                            None => {
                                let mut cwd = cwd;
                                if worktree {
                                    match crate::worktree::create(&cwd) {
                                        Ok(p) => {
                                            post(Msg::Project(p.clone()));
                                            cwd = p;
                                        }
                                        Err(e) => post(Msg::Event(StreamEvent::System(format!(
                                            "worktree not created: {e}"
                                        )))),
                                    }
                                }
                                match client.create_session_async(&cwd, None).await {
                                    Ok(r) => {
                                        session = Some(r.session_id.clone());
                                        post(Msg::Opened {
                                            id: r.session_id.clone(),
                                            replay: vec![StreamEvent::UserPrompt {
                                                content: text.clone(),
                                            }],
                                        });
                                        r.session_id
                                    }
                                    Err(e) => {
                                        post(Msg::Event(StreamEvent::TurnFailed {
                                            error: e.to_string(),
                                        }));
                                        continue;
                                    }
                                }
                            }
                        };
                        let options = ChatOptions {
                            model,
                            permission,
                            ..ChatOptions::default()
                        };
                        // The turn runs on its own task, so New/Open (and other threads) stay
                        // usable while it streams.
                        let (tx, mut trx) = mpsc::unbounded_channel();
                        turns.insert(id.clone(), tx);
                        post(Msg::TurnStarted(id.clone()));
                        let (client, poster) = (client.clone(), poster.clone());
                        tokio::spawn(async move {
                            let p = |m: Msg| poster.post(m);
                            run_turn(&client, &id, &text, options, &mut trx, &p).await;
                            refresh(&client, &p).await;
                        });
                    }
                    cmd @ (Cmd::Cancel
                    | Cmd::Approve { .. }
                    | Cmd::Steer(_)
                    | Cmd::Followup(_)
                    | Cmd::Recall { .. }) => {
                        // Controls go to the open thread's turn.
                        if let Some(t) = session.as_ref().and_then(|id| turns.get(id)) {
                            let _ = t.send(cmd);
                        }
                    }
                }
            }
        });
    });
}

async fn refresh(client: &DaemonClient, post: &impl Fn(Msg)) {
    if let Ok(mut list) = client.list_sessions_async().await {
        // Test runs and scratch work leave thousands of throwaway-directory sessions; they
        // would each become a sidebar project.
        // Only threads touched in the last two days are listed.
        let cutoff = chrono::Utc::now() - chrono::Duration::days(RECENT_DAYS);
        let tmp = std::env::temp_dir();
        list.retain(|x| {
            chrono::DateTime::parse_from_rfc3339(&x.updated_at).is_ok_and(|t| t >= cutoff)
                && x.message_count > 0
                && !x.cwd.is_empty()
                && !std::path::Path::new(&x.cwd).starts_with(&tmp)
                && !x.cwd.starts_with("/tmp/")
        });
        post(Msg::Sessions(list));
    }
}

async fn replay(client: &DaemonClient, id: &str) -> Result<Vec<StreamEvent>, String> {
    client.reattach_async(id).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut since = 0u64;
    loop {
        let page = client
            .events_async(id, since)
            .await
            .map_err(|e| e.to_string())?;
        let progressed = page.next_seq > since;
        out.extend(page.events.into_iter().map(|e| e.event));
        if !progressed {
            break;
        }
        since = page.next_seq;
        if out.len() > 50_000 {
            break;
        }
    }
    Ok(out)
}

async fn run_turn(
    client: &DaemonClient,
    id: &str,
    prompt: &str,
    options: ChatOptions,
    rx: &mut mpsc::UnboundedReceiver<Cmd>,
    post: &impl Fn(Msg),
) {
    let mut stream = match client.chat_stream(id, prompt, options).await {
        Ok(s) => s,
        Err(e) => {
            return post(Msg::Turn {
                id: id.to_string(),
                ev: StreamEvent::TurnFailed {
                    error: e.to_string(),
                },
            });
        }
    };
    // Follow-ups chain more turns on this same stream, so a TurnComplete only ends it when none
    // are waiting.
    let mut followups = 0usize;
    loop {
        tokio::select! {
            item = stream.next_event() => match item {
                Some(Ok(ev)) => {
                    if matches!(ev, StreamEvent::FollowupAccepted { .. }) {
                        followups = followups.saturating_sub(1);
                    }
                    let done = match &ev {
                        StreamEvent::TurnComplete { .. } => followups == 0,
                        StreamEvent::TurnFailed { .. } => true,
                        _ => false,
                    };
                    post(Msg::Turn { id: id.to_string(), ev });
                    if done { return; }
                }
                Some(Err(e)) => return post(Msg::Turn { id: id.to_string(), ev: StreamEvent::TurnFailed { error: e } }),
                None => return post(Msg::Turn { id: id.to_string(), ev: StreamEvent::TurnComplete { response: String::new(), usage: None, cached: None } }),
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Approve { request_id, decision }) => {
                    let _ = client.approve_async(id, &request_id, decision).await;
                }
                Some(Cmd::Cancel) => { let _ = client.cancel_async(id).await; }
                Some(Cmd::Steer(text)) => { let _ = client.steer_async(id, &text).await; }
                Some(Cmd::Followup(text)) => {
                    followups += 1;
                    if client.followup_async(id, &text).await.is_err() {
                        followups -= 1;
                    }
                }
                  Some(Cmd::Recall { text, followup }) => {
                      if client.recall_async(id, &text, followup).await.is_ok() && followup {
                          followups = followups.saturating_sub(1);
                      }
                  }
                  Some(Cmd::Rename { id: sid, name }) => {
                      if client.rename_session_async(&sid, &name).await.is_ok() {
                          refresh(client, post).await;
                      }
                  }
                Some(_) => {}
                None => return,
            },
        }
    }
}
