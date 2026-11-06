//! dex GUI: a native client for the dex daemon, built on Rune. Embeds the daemon in-process
//! and talks to it over HTTP+SSE via `dex-client`; `DEX_GUI_URL` attaches to an existing one.
//! Headless render: `dex-gui --snap out.png --scene chat`.

mod state;
mod view;
mod worker;

use rune::{AppSpec, Tx, Waker};
use state::{Cmd, Info, Item, Msg, State, ToolDone, ToolRun};
use tokio::sync::mpsc;

fn setup(s: &mut State, wake: Waker) {
    let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
    s.cmds = Some(tx);
    worker::spawn(s.inbox.clone(), wake, rx);
}

fn poll(s: &mut State, _: f64, _: &mut Tx) -> bool {
    let msgs: Vec<Msg> = std::mem::take(&mut *s.inbox.0.lock().unwrap());
    let changed = !msgs.is_empty();
    for m in msgs {
        s.apply(m);
    }
    changed
}

fn sample(s: &mut State) {
    s.status = "ready".into();
    s.info = Info {
        provider: "opencode".into(),
        model: "glm-5.3-flash".into(),
        models: vec!["glm-5.3-flash".into()],
        permission: "trusted".into(),
        cwd: "~/Work/dex".into(),
        branch: Some("develop".into()),
    };
    for (i, t) in [
        "Fix flaky reconnect in the event stream",
        "Sidebar shows stale session titles",
        "Migrate config loader",
        "Add keyboard shortcuts",
    ]
    .iter()
    .enumerate()
    {
        if let Ok(si) = serde_json::from_value(serde_json::json!({
            "session_id": format!("s{i}"), "path": "", "name": t, "cwd": "/", "updated_at": format!("2026-11-0{}", 9 - i), "auto_named": false
        })) {
            s.sessions.push(si);
        }
    }
    s.current = Some("s0".into());
    s.tokens = 64_600;
    s.cost = 1.84;
    s.items.push(Item::User(
        "list the crates in this repo and explain the client split".into(),
    ));
    s.items.push(Item::Tool(ToolRun {
        id: "1".into(),
        name: "ls".into(),
        args: "crates".into(),
        done: Some(ToolDone {
            summary: "11 entries".into(),
            ok: true,
            preview: vec!["dex-acp".into(), "dex-client".into()],
            secs: 0.1,
        }),
        open: true,
    }));
    s.items.push(Item::Assistant {
        text: "## Crates\n\nThere are **11 crates**. The client split is:\n\n- `dex-client` talks HTTP+SSE\n- `dex-protocol` holds the wire types\n\n```rust\nlet c = DaemonClient::new(url)?;\n```\n".into(),
        streaming: false,
    });
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[("chat", sample)],
        setup: Some(setup),
        poll: Some(poll),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        ..AppSpec::new("dex", (1100.0, 760.0), State::default, view::view)
    });
}
