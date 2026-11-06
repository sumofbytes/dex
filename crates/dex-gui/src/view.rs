//! Views: sidebar, transcript, composer, footer. Pure functions of `State`.

use dex_client::protocol::ApprovalDecision;
use rune::{Cx, El, ScrollSpec, col, row, spacer, text, text_input};
use rune_kit::markdown::Markdown;
use rune_kit::shell::{RowStatus, Sidebar, SidebarRow, SidebarSection};
use rune_kit::{
    Button, ButtonVariant, Collapsible, IconButton, IconButtonVariant, Kit, Select, Spinner,
};
use rune_theme_opencode::icons;

use crate::state::{Cmd, Item, State, project_name, session_title};
use dex_client::protocol::SessionInfo;

const MODES: [&str; 3] = ["read-only", "ask", "trusted"];
const COLUMN: f32 = 760.0;
const SIDEBAR_W: f32 = 280.0;

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    let k = rune_theme_opencode::kit(cx.system_dark());
    let mut layer = Vec::new();
    let main = main_pane(s, k, &mut layer);
    let body = if s.sidebar {
        row()
            .w_full()
            .h_full()
            .fill(k.c.page)
            .child(sidebar(s, k).w(SIDEBAR_W))
            .child(main.grow(1.0).min_w(0.0))
    } else {
        row().w_full().h_full().fill(k.c.page).child(main.grow(1.0))
    };
    rune_kit::root(body, layer)
}

const PER_PROJECT: usize = 8;
const PROJECTS: usize = 12;

fn sidebar(s: &State, k: &Kit) -> El {
    let new = Button::new("New thread")
        .variant(ButtonVariant::Ghost)
        .icon(&icons::EDIT)
        .on_press(|s: &mut State| {
            s.send_cmd(Cmd::NewSession);
        })
        .build(k)
        .key("new-thread");
    let mut sb = Sidebar::new()
        .label("Projects")
        .header_padding(14.0, 10.0)
        .header(new);

    // Projects: the active one first, then by most recent thread. The daemon lists every
    // directory's threads; empty ones are skipped.
    let current = s.cwd();
    let mut projects: Vec<(String, Vec<&SessionInfo>)> = Vec::new();
    for sess in s
        .sessions
        .iter()
        .filter(|x| x.message_count > 0 && !x.cwd.is_empty())
    {
        match projects.iter_mut().find(|(c, _)| *c == sess.cwd) {
            Some((_, v)) => v.push(sess),
            None => projects.push((sess.cwd.clone(), vec![sess])),
        }
    }
    if !projects.iter().any(|(c, _)| *c == current) && !current.is_empty() {
        projects.insert(0, (current.clone(), Vec::new()));
    } else if let Some(i) = projects.iter().position(|(c, _)| *c == current) {
        let p = projects.remove(i);
        projects.insert(0, p);
    }

    let hidden = projects.len().saturating_sub(PROJECTS);
    let take = if s.all_projects {
        projects.len()
    } else {
        projects.len().min(PROJECTS)
    };
    for (pi, (cwd, threads)) in projects.iter().take(take).enumerate() {
        let collapsed = s.collapsed.contains(cwd);
        let all = s.show_all.contains(cwd);
        let add = {
            let cwd = cwd.clone();
            IconButton::new(&icons::PLUS)
                .variant(IconButtonVariant::Ghost)
                .label("New thread in project")
                .on_press(move |s: &mut State| {
                    s.project = Some(cwd.clone());
                    s.send_cmd(Cmd::NewSession);
                })
                .build(k)
                .key("add")
        };
        let toggle_cwd = cwd.clone();
        let mut sec = SidebarSection::new(project_name(cwd))
            .key(format!("proj{pi}"))
            .collapsed(collapsed)
            .count(threads.len())
            .action(add)
            .on_toggle(move |s: &mut State, _| {
                if !s.collapsed.remove(&toggle_cwd) {
                    s.collapsed.insert(toggle_cwd.clone());
                }
            });
        let shown = if all {
            threads.len()
        } else {
            threads.len().min(PER_PROJECT)
        };
        for (ti, sess) in threads.iter().take(shown).enumerate() {
            let id = sess.session_id.clone();
            let open_cwd = sess.cwd.clone();
            let selected = s.current.as_deref() == Some(sess.session_id.as_str());
            let mut r = SidebarRow::new(session_title(sess))
                .selected(selected)
                .on_select(move |s: &mut State, _| {
                    s.project = Some(open_cwd.clone());
                    s.send_cmd(Cmd::Open(id.clone()));
                });
            if selected && s.busy {
                r = r.status(RowStatus::Busy);
            }
            sec = sec.row(r.build(k).key(format!("t{pi}-{ti}")));
        }
        if threads.len() > PER_PROJECT {
            let more_cwd = cwd.clone();
            let label = if all {
                "Show less".to_string()
            } else {
                format!("Show {} more", threads.len() - PER_PROJECT)
            };
            sec = sec.row(
                Button::new(label)
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut State| {
                        if !s.show_all.remove(&more_cwd) {
                            s.show_all.insert(more_cwd.clone());
                        }
                    })
                    .build(k)
                    .key(format!("more{pi}")),
            );
        }
        sb = sb.section(sec);
    }
    if hidden > 0 {
        let label = if s.all_projects {
            "Show fewer projects".to_string()
        } else {
            format!("Show {hidden} more projects")
        };
        sb = sb.child(
            Button::new(label)
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut State| s.all_projects = !s.all_projects)
                .build(k)
                .key("more-projects"),
        );
    }
    if !s.sessions_loaded {
        sb = sb.child(
            text("Loading threads…")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(12.0)
                .key("loading"),
        );
    }
    sb.build(k).key("sidebar-root")
}

fn main_pane(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let card = col()
        .key("main")
        .h_full()
        .fill(k.c.bg_base)
        .radius(k.radius.lg)
        .border(1.0, k.c.border_muted)
        .child(header(s, k))
        .child(transcript(s, k))
        .child(composer(s, k, layer))
        .child(footer(s, k));
    col().h_full().padding(8.0).child(card.grow(1.0).min_h(0.0))
}

fn header(s: &State, k: &Kit) -> El {
    let toggle = IconButton::new(&icons::SIDEBAR)
        .variant(IconButtonVariant::Ghost)
        .label("Toggle sidebar")
        .on_press(|s: &mut State| s.sidebar = !s.sidebar)
        .build(k)
        .key("toggle-sidebar");
    let title = text(s.title())
        .font(k.ty.ui_medium)
        .color(k.c.text_base)
        .truncate();
    let mut stats = row().gap(8.0).items_center();
    if s.busy {
        stats = stats.child(Spinner::new().build(k));
    }
    if s.tokens > 0 {
        stats = stats.child(
            text(format!("{}  ${:.2}", human(s.tokens), s.cost))
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    }
    row()
        .w_full()
        .h(48.0)
        .px(12.0)
        .gap(8.0)
        .items_center()
        .child(toggle)
        .child(title.grow(1.0).min_w(0.0))
        .child(stats)
}

fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn transcript(s: &State, k: &Kit) -> El {
    let mut list = col().w_full().max_w(COLUMN).gap(14.0).py(16.0).px(16.0);
    if s.items.is_empty() {
        list = list.child(
            col()
                .w_full()
                .py(120.0)
                .items_center()
                .gap(6.0)
                .child(
                    text("What should we work on?")
                        .font(k.ty.body_medium)
                        .color(k.c.text_base),
                )
                .child(text(s.cwd()).font(k.ty.ui).color(k.c.text_muted)),
        );
    }
    for (i, it) in s.items.iter().enumerate() {
        list = list.child(item(i, it, k).key(format!("item{i}")));
    }
    let centered = row().w_full().justify_center().child(list.grow(1.0));
    centered
        .key("transcript")
        .scroll(ScrollSpec { stick_bottom: true })
        .grow(1.0)
        .min_h(0.0)
        .w_full()
}

fn item(i: usize, it: &Item, k: &Kit) -> El {
    match it {
        Item::User(t) => row().w_full().justify_end().child(
            col()
                .max_w(560.0)
                .px(14.0)
                .py(10.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_02)
                .child(text(t.clone()).font(k.ty.body).color(k.c.text_base)),
        ),
        Item::Assistant { text: t, streaming } => Markdown::new(t.clone())
            .streaming(*streaming)
            .build(k)
            .w_full(),
        Item::Thinking { text: t, open } => Collapsible::new("Thinking", *open)
            .child(text(t.clone()).font(k.ty.ui).color(k.c.text_muted))
            .on_toggle(move |s: &mut State| {
                if let Some(Item::Thinking { open, .. }) = s.items.get_mut(i) {
                    *open = !*open;
                }
            })
            .build(k),
        Item::Tool(t) => {
            let mark = match &t.done {
                None => "…",
                Some(d) if d.ok => "✓",
                Some(_) => "✗",
            };
            let secs = match &t.done {
                Some(d) if d.secs >= 1.0 => format!("  {:.1}s", d.secs),
                _ => String::new(),
            };
            let title = format!("{mark} {}  {}{secs}", t.name, t.args);
            let mut c = Collapsible::new(title, t.open).on_toggle(move |s: &mut State| {
                if let Some(Item::Tool(t)) = s.items.get_mut(i) {
                    t.open = !t.open;
                }
            });
            if let Some(d) = &t.done {
                let mut body = col().gap(2.0).px(8.0).py(4.0);
                body = body.child(text(d.summary.clone()).font(k.ty.ui).color(k.c.text_muted));
                for l in &d.preview {
                    body = body.child(text(l.clone()).font(k.ty.mono).color(k.c.text_base));
                }
                c = c.child(body);
            }
            c.build(k)
        }
        Item::Notice(t) => text(t.clone()).font(k.ty.small).color(k.c.text_faint),
        Item::Error(t) => col()
            .w_full()
            .px(12.0)
            .py(8.0)
            .radius(k.radius.md)
            .fill(k.c.danger.bg)
            .child(text(t.clone()).font(k.ty.ui).color(k.c.danger.fg)),
        Item::Approval {
            request_id,
            name,
            input,
            decided,
        } => {
            let head = text(format!("Allow {name}?"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base);
            let detail: String = input.chars().take(400).collect();
            let mut c = col()
                .w_full()
                .px(14.0)
                .py(12.0)
                .gap(8.0)
                .radius(k.radius.lg)
                .fill(k.c.layer_02)
                .border(1.0, k.c.border_base)
                .child(head)
                .child(text(detail).font(k.ty.mono).color(k.c.text_muted));
            c = match decided {
                Some(d) => c.child(
                    text(match d {
                        ApprovalDecision::Deny => "Denied",
                        ApprovalDecision::AllowOnce => "Allowed once",
                        ApprovalDecision::AllowSession => "Allowed for session",
                    })
                    .font(k.ty.small)
                    .color(k.c.text_faint),
                ),
                None => {
                    let btn = |label: &'static str, v: ButtonVariant, d: ApprovalDecision| {
                        let rid = request_id.clone();
                        Button::new(label)
                            .variant(v)
                            .on_press(move |s: &mut State| {
                                if let Some(Item::Approval { decided, .. }) = s.items.get_mut(i) {
                                    *decided = Some(d);
                                }
                                s.send_cmd(Cmd::Approve {
                                    request_id: rid.clone(),
                                    decision: d,
                                });
                            })
                            .build(k)
                            .key(label)
                    };
                    c.child(
                        row()
                            .gap(8.0)
                            .child(btn(
                                "Allow once",
                                ButtonVariant::Contrast,
                                ApprovalDecision::AllowOnce,
                            ))
                            .child(btn(
                                "Allow for session",
                                ButtonVariant::Neutral,
                                ApprovalDecision::AllowSession,
                            ))
                            .child(btn("Deny", ButtonVariant::Ghost, ApprovalDecision::Deny)),
                    )
                }
            };
            c
        }
    }
}

fn composer(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let input = text_input(&s.edit, k.ty.body.lh(22.0))
        .key("composer-input")
        .placeholder("Ask anything…", k.c.text_faint)
        .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
        .color(k.c.text_base)
        .on_edit(|s: &mut State| &mut s.edit)
        .on_submit(|s: &mut State, _| s.submit())
        .w_full()
        .min_h(44.0)
        .max_h(220.0);

    let mode_cur = s.effective_permission();
    let mode = Select::new("mode", &MODES)
        .current(Some(&mode_cur))
        .width(120.0)
        .open(s.open_select == Some("mode"))
        .on_toggle(|s: &mut State| {
            s.open_select = if s.open_select == Some("mode") {
                None
            } else {
                Some("mode")
            }
        })
        .on_close(|s: &mut State| s.open_select = None)
        .on_choose(|s: &mut State, v: &str, _| {
            s.permission = Some(v.to_string());
            s.open_select = None;
        })
        .build(k, layer);

    let models = if s.info.models.is_empty() {
        vec![s.effective_model()]
    } else {
        s.info.models.clone()
    };
    let model_cur = s.effective_model();
    let model = Select::new("model", &models)
        .current(Some(&model_cur))
        .width(260.0)
        .open(s.open_select == Some("model"))
        .on_toggle(|s: &mut State| {
            s.open_select = if s.open_select == Some("model") {
                None
            } else {
                Some("model")
            }
        })
        .on_close(|s: &mut State| s.open_select = None)
        .on_choose(|s: &mut State, v: &str, _| {
            s.model = Some(v.to_string());
            s.open_select = None;
        })
        .build(k, layer);

    let action = if s.busy {
        IconButton::new(&icons::STOP)
            .variant(IconButtonVariant::Ghost)
            .label("Stop")
            .on_press(|s: &mut State| s.send_cmd(Cmd::Cancel))
            .build(k)
    } else {
        IconButton::new(&icons::ARROW_UP)
            .variant(IconButtonVariant::Contrast)
            .label("Send")
            .on_press(|s: &mut State| s.submit())
            .build(k)
    }
    .key("send");

    let card = col()
        .w_full()
        .max_w(COLUMN)
        .padding(12.0)
        .gap(8.0)
        .radius(k.radius.xl)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_base)
        .child(input)
        .child(
            row()
                .gap(8.0)
                .items_center()
                .child(mode)
                .child(model)
                .child(spacer())
                .child(action),
        );
    row()
        .w_full()
        .justify_center()
        .px(16.0)
        .pb(8.0)
        .child(card.grow(1.0))
}

fn footer(s: &State, k: &Kit) -> El {
    let branch = s
        .info
        .branch
        .clone()
        .map(|b| format!("  ⎇ {b}"))
        .unwrap_or_default();
    row()
        .w_full()
        .h(28.0)
        .px(14.0)
        .items_center()
        .child(
            text(format!("{}{branch}", s.cwd()))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(
            text(format!("{} · {}", s.info.provider, s.status))
                .font(k.ty.small)
                .color(k.c.text_faint),
        )
}
