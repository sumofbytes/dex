//! dex GUI: a native client for the dex daemon, built on Rune. Embeds the daemon in-process
//! and talks to it over HTTP+SSE via `dex-client`; `DEX_GUI_URL` attaches to an existing one.
//! Headless render: `dex-gui --snap out.png --scene chat`. Linked into `dex` behind its `gui` feature.

mod composer_menu;
mod home;
mod icons;
mod sidebar;
mod state;
mod style;
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
    s.load_term_history();
    s.load_theme();
    s.wake = Some(wake.clone());
    worker::spawn(s.inbox.clone(), wake, rx);
}

/// Shortcuts, written with Ctrl (Cmd on macOS) as in Po: B sidebar, N new thread, O every tool
/// call's details, T all thinking, Esc stops the running turn.
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
    } else if e.is_command('`')
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
    } else if matches!(e.key, rune::Key::Escape)
        && s.busy()
        && s.renaming.is_none()
        && !s.theme_open
    {
        // Esc stops the running turn (the composer's Stop button does the same).
        s.send_cmd(Cmd::Cancel);
    } else {
        return false;
    }
    true
}

fn poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    // A line that wraps is laid out one commit before the field's height catches up, and an idle
    // window draws no further frame, so the new line sits flush against the chips until the
    // next keystroke. Ask for one more commit after each edit.
    let edited = s.edit.text != s.edit_seen;
    if edited {
        s.edit_seen = s.edit.text.clone();
        tx.wake_after(std::time::Duration::ZERO);
    }
    let msgs: Vec<Msg> = std::mem::take(&mut *s.inbox.0.lock().unwrap());
    let changed = edited || !msgs.is_empty();
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
            "updated_at": (chrono::Utc::now() - chrono::Duration::hours([2, 9, 26, 3, 50][i])).to_rfc3339(), "auto_named": false, "message_count": 4
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

/// The sidebar with NEEDS YOU rows and a thread running in the background.
fn needs_scene(s: &mut State) {
    sample(s);
    s.current = Some("s0".into());
    let n = |id: &str, kind, label: Option<&str>| state::Need {
        id: id.into(),
        kind,
        label: label.map(Into::into),
    };
    s.needs = vec![
        n(
            "s1",
            state::NeedKind::Waiting,
            Some("Allow bash: cargo test?"),
        ),
        n("s2", state::NeedKind::Done, None),
        n("s4", state::NeedKind::Failed, None),
    ];
    s.running.insert("s3".into());
}

/// Mid-turn: thinking, a steering message and a follow-up queued.
fn busy(s: &mut State) {
    sample(s);
    s.sending = true;
    s.status = "working…".into();
    s.items.push(Item::Thinking {
        text: "Checking how the client handles reconnects…".into(),
        open: true,
        started: None,
        secs: None,
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

/// A finished thought, collapsed or open, then tools and the reply (`DEX_GUI_OPEN=1` opens it).
fn thought_scene(s: &mut State) {
    sample(s);
    let open = std::env::var_os("DEX_GUI_OPEN").is_some();
    s.items.clear();
    s.items.push(Item::User(
        "can you read me readme and tell me if we are good".into(),
    ));
    let thought = |secs| {
        Item::Thinking {
        text: "This isn't the dex repo at all. I'm in a different project, dex-gui, a spike GUI client.".into(),
        open: false,
        started: None,
        secs: Some(secs),
    }
    };
    let tool = |name: &str, args: &str, secs: f64, out: &str| {
        Item::Tool(ToolRun {
            id: name.into(),
            name: name.into(),
            args: args.into(),
            done: Some(ToolDone {
                summary: out.into(),
                ok: true,
                preview: vec![out.into()],
                secs,
            }),
            open: false,
        })
    };
    s.items.push(thought(4));
    s.items.push(tool("ls", "crates", 0.1, "11 entries"));
    s.items
        .push(tool("bash", "cargo build", 2.4, "Finished dev profile"));
    s.items.push(thought(2));
    s.items.push(tool("read", "src/view.rs", 0.0, "560 lines"));
    s.items.push(tool("grep", "tool_group", 0.1, "3 matches"));
    s.items.push(Item::Assistant {
        text: "Turns out I'm not in the dex repo. I'm in a separate spike project called dex-gui.\n\n- `Cargo.toml`: a standalone workspace\n- `src/`: main.rs, state.rs\n\nWhich would you like?".into(),
        streaming: false,
    });
    if open {
        s.open_work.insert(1);
    }
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

/// A sidebar thread renamed in place: the title is an editable field (double-click rename).
fn rename_scene(s: &mut State) {
    sample(s);
    s.start_rename("s1");
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

/// How long the launcher watches the relaunched GUI for an early crash (no GPU/Vulkan driver,
/// no window system) before handing the shell back.
const START_GRACE: std::time::Duration = std::time::Duration::from_millis(2500);

/// Like `code .`: relaunch ourselves in the background and return to the shell. Skipped with
/// any argument (`-f`, `--snap`, … stay in the foreground), with `DEX_GUI_FOREGROUND`, and in
/// the relaunched child itself. Returns `false` when the child died within [`START_GRACE`] of a
/// failed start, so the caller can fall back instead of leaving the user with nothing.
fn detach() -> bool {
    use std::process::{Command, Stdio};
    if std::env::args().len() > 1
        || std::env::var_os("DEX_GUI_DETACHED").is_some()
        || std::env::var_os("DEX_GUI_FOREGROUND").is_some()
    {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return true;
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
    let Ok(mut child) = cmd.spawn() else {
        return true; // could not relaunch: run in this process instead
    };
    let deadline = std::time::Instant::now() + START_GRACE;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                eprintln!(
                    "dex: the GUI failed to start ({status}); details in {}",
                    log_path().map_or_else(|| "its log".into(), |p| p.display().to_string())
                );
                return false;
            }
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    std::process::exit(0);
}

/// Runs the GUI. `false` means it could not start and the caller should fall back (to the TUI).
pub fn run() -> bool {
    if !detach() {
        return false;
    }
    rune::run(AppSpec::<State> {
        scenes: &[
            ("chat", sample),
            ("home", home_scene),
            ("rename", rename_scene),
            ("busy", busy),
            ("needs", needs_scene),
            ("thought", thought_scene),
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
    true
}
