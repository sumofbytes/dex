//! dex GUI: a native client for the dex daemon, built on Rune. Embeds the daemon in-process
//! and talks to it over HTTP+SSE via `dex-client`; `DEX_GUI_URL` attaches to an existing one.
//! Headless render: `dex-gui --snap out.png --scene chat`.

mod composer_menu;
mod home;
mod icons;
mod state;
mod theme;
mod theme_picker;
mod view;
mod viewer;
mod worker;
mod worktree;

use rune::{AppSpec, KeyEvent, Tx, Waker};
use state::{Cmd, Info, Item, Msg, State, ToolDone, ToolRun};
use tokio::sync::mpsc;

fn setup(s: &mut State, wake: Waker) {
    let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
    s.cmds = Some(tx);
    s.load_hidden();
    s.load_theme();
    s.wake = Some(wake.clone());
    worker::spawn(s.inbox.clone(), wake, rx);
}

/// Shortcuts, written with Ctrl (Cmd on macOS) as in Po: B sidebar, N new thread, O every tool
/// call's details, T all thinking.
fn on_key(s: &mut State, e: &KeyEvent, _: &mut Tx) -> bool {
    if e.is_command('b') {
        s.sidebar = !s.sidebar;
    } else if e.is_command('n') {
        s.page = state::Page::Thread;
        s.send_cmd(Cmd::NewSession);
    } else if e.is_command('e') {
        s.show_viewer(state::VTab::Files);
    } else if e.is_command('g') {
        s.show_viewer(state::VTab::Changes);
    } else if e.is_command('j')
        || matches!(&e.key, rune::Key::Text(t) if t == "`" && (e.mods.cmd || e.mods.ctrl))
    {
        s.page = state::Page::Thread;
        s.show_viewer(state::VTab::Terminal);
    } else if e.is_command(',') {
        s.theme_open = true;
    } else if e.is_command('o') {
        s.toggle_tools();
    } else if e.is_command('t') {
        s.toggle_thinking();
    } else {
        return false;
    }
    true
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
    s.page = state::Page::Thread;
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
    s.page = state::Page::Home;
    s.sessions.clear();
    if let Ok(path) = std::env::var("DEX_GUI_SESSIONS")
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        let rows = v.get("sessions").cloned().unwrap_or(v);
        s.sessions = serde_json::from_value(rows).unwrap_or_default();
    }
}

/// A transcript taller than the window, to check scrolling.
fn long(s: &mut State) {
    sample(s);
    for i in 0..30 {
        s.items.push(Item::User(format!("follow-up question {i}")));
        s.items.push(Item::Assistant {
            text: format!("Answer {i}: the quick brown fox jumps over the lazy dog, twice, to make the line wrap across the column."),
            streaming: false,
        });
    }
}

/// Mid-turn: thinking, a steering message and a follow-up queued.
fn busy(s: &mut State) {
    sample(s);
    s.busy = true;
    s.status = "working…".into();
    s.items.push(Item::Thinking {
        text: "Checking how the client handles reconnects…".into(),
        open: true,
    });
    s.queue.push(state::Queued {
        text: "also add a regression test".into(),
        followup: false,
    });
    s.queue.push(state::Queued {
        text: "then update the docs".into(),
        followup: true,
    });
}

/// Viewer pane over this repository: file preview, or the changes tab.
fn viewer_scene(s: &mut State, tab: state::VTab) {
    sample(s);
    s.project = Some(std::env::var("DEX_GUI_REPO").unwrap_or_else(|_| "/home/aks/Work/dex".into()));
    s.apply(state::scan(s.cwd()));
    s.viewer = true;
    s.vtab = tab;
}

fn files_scene(s: &mut State) {
    viewer_scene(s, state::VTab::Files);
    s.open_path("crates/dex-gui/src/viewer.rs");
}

fn term_scene(s: &mut State) {
    viewer_scene(s, state::VTab::Terminal);
    s.term = vec![
        state::TermBlock {
            cmd: "git log --oneline -3".into(),
            lines: vec![
                "9cdca96 feat(gui): replicate the Paper design v2".into(),
                "cb34053 fix(gui): align the footer with the composer".into(),
                "03ba94e docs(gui): proposed composer option menus".into(),
            ],
            status: state::TermStatus::Done { code: 0, secs: 0.1 },
        },
        state::TermBlock {
            cmd: "cargo test viewer".into(),
            lines: vec!["error[E0433]: cannot find `theme` in this scope".into()],
            status: state::TermStatus::Done { code: 1, secs: 2.4 },
        },
    ];
}

fn wt_scene(s: &mut State) {
    sample(s);
    s.project = std::env::var("DEX_GUI_WT").ok();
}

/// Applies without saving, so renders never change the user's own scheme.
fn apply_theme_only(name: &str) {
    theme::set(name);
}

/// Theme picker open; `DEX_GUI_THEME` picks the scheme behind it.
fn themepick_scene(s: &mut State) {
    sample(s);
    if let Ok(name) = std::env::var("DEX_GUI_THEME") {
        apply_theme_only(&name);
    }
    s.theme_open = true;
}

/// Composer options: the model menu open over a list of models.
fn menus_scene(s: &mut State) {
    sample(s);
    s.info.models = (0..14).map(|i| format!("model-{i}")).collect();
    s.info.models.insert(0, "glm-5.3-flash".into());
    s.model = Some("glm-5.3-flash".into());
    s.open_select = Some("model");
}

/// Composer options: the mode menu open.
fn modemenu_scene(s: &mut State) {
    sample(s);
    s.open_select = Some("mode");
}

fn changes_scene(s: &mut State) {
    viewer_scene(s, state::VTab::Changes);
}

fn home_scene(s: &mut State) {
    sample(s);
    s.page = state::Page::Home;
}

/// Where the detached GUI's output goes: `$XDG_CACHE_HOME/dex/gui.log` (default `~/.cache`).
fn log_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    let dir = base.join("dex");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("gui.log"))
}

/// Like `code .`: relaunch ourselves in the background and return to the shell. Skipped with
/// any argument (`-f`, `--snap`, … stay in the foreground), with `DEX_GUI_FOREGROUND`, and in
/// the relaunched child itself.
fn detach() {
    use std::process::{Command, Stdio};
    if std::env::args().len() > 1
        || std::env::var_os("DEX_GUI_DETACHED").is_some()
        || std::env::var_os("DEX_GUI_FOREGROUND").is_some()
    {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let (out, err) = match log_path().and_then(|p| {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()?;
        Some((f.try_clone().ok()?, f))
    }) {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let mut cmd = Command::new(exe);
    cmd.env("DEX_GUI_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // own group: a closed terminal's SIGHUP/Ctrl-C do not reach it
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008 | 0x0000_0200); // DETACHED_PROCESS | NEW_PROCESS_GROUP
    }
    if cmd.spawn().is_ok() {
        std::process::exit(0);
    }
}

fn main() {
    detach();
    rune::run(AppSpec::<State> {
        scenes: &[
            ("chat", sample),
            ("home", home_scene),
            ("busy", busy),
            ("many", many),
            ("long", long),
            ("real", real),
            ("files", files_scene),
            ("changes", changes_scene),
            ("term", term_scene),
            ("wt", wt_scene),
            ("themepick", themepick_scene),
            ("menus", menus_scene),
            ("modemenu", modemenu_scene),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        ..AppSpec::new("dex", (1100.0, 760.0), State::default, view::view)
    });
}
