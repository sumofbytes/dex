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
    let samples = [
        (
            "Fix flaky reconnect in the event stream",
            "/home/aks/Work/dex",
        ),
        ("Sidebar shows stale session titles", "/home/aks/Work/dex"),
        ("Migrate config loader", "/home/aks/Work/dex"),
        ("Explain the tile cache eviction", "/home/aks/Work/lumen"),
        (
            "Retry uploads after a dropped connection",
            "/home/aks/Work/lumen",
        ),
    ];
    for (i, (t, cwd)) in samples.iter().enumerate() {
        if let Ok(si) = serde_json::from_value(serde_json::json!({
            "session_id": format!("s{i}"), "path": "", "name": t, "cwd": cwd,
            "updated_at": format!("2026-11-0{}", 9 - i), "auto_named": false, "message_count": 4
        })) {
            s.sessions.push(si);
        }
    }
    s.sessions_loaded = true;
    s.project = Some("/home/aks/Work/dex".into());
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

/// Stress scene: a large session history like a long-lived daemon's.
fn many(s: &mut State) {
    sample(s);
    s.sessions.clear();
    for i in 0..4700 {
        let cwd = [
            "/home/aks/Work/dex",
            "/home/aks/Work/po",
            "/home/aks/Work/exsa",
        ][i % 3];
        if let Ok(si) = serde_json::from_value(serde_json::json!({
            "session_id": format!("m{i}"), "path": "", "name": format!("thread {i}"), "cwd": cwd,
            "updated_at": format!("2026-11-{:02}", i % 28), "auto_named": false, "message_count": 3
        })) {
            s.sessions.push(si);
        }
    }
}

/// Debug scene: sessions from the JSON file named by DEX_GUI_SESSIONS (a saved `/api/sessions`).
fn real(s: &mut State) {
    sample(s);
    s.sessions.clear();
    if let Ok(path) = std::env::var("DEX_GUI_SESSIONS")
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let rows = v.get("sessions").cloned().unwrap_or(v);
        s.sessions = serde_json::from_value(rows).unwrap_or_default();
    }
}

fn main() {
    rune::run(AppSpec::<State> {
        scenes: &[("chat", sample), ("many", many), ("real", real)],
        setup: Some(setup),
        poll: Some(poll),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        ..AppSpec::new("dex", (1100.0, 760.0), State::default, view::view)
    });
}
