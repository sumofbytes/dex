//! Views: sidebar, transcript, composer, footer. Pure functions of `State`.

use dex_client::protocol::ApprovalDecision;
use rune::{Cx, El, ScrollSpec, TextStyle, col, el, row, spacer, text, text_input};
use rune_kit::markdown::Markdown;
use rune_kit::shell::{RowStatus, Sidebar, SidebarRow};
use rune_kit::{Button, ButtonVariant, Collapsible, IconButton, IconButtonVariant, Kit, Spinner};
use rune_theme_opencode::icons;

use crate::state::{Cmd, Item, Page, State, VTab, project_name, session_title};
use dex_client::protocol::SessionInfo;

const COLUMN: f32 = 760.0;
const SIDEBAR_W: f32 = 280.0;

pub fn view(s: &State, _cx: &mut Cx<State>) -> El {
    let k = crate::theme::kit();
    let mut layer = Vec::new();
    let page = match s.page {
        Page::Home => crate::home::home(s, k),
        Page::Thread => main_pane(s, k, &mut layer),
    };
    let body = if s.sidebar {
        row()
            .w_full()
            .h_full()
            .fill(k.c.page)
            .child(sidebar(s, k).w(SIDEBAR_W))
            .child(el().w(1.0).h_full().fill(k.c.border_base))
            .child(page.grow(1.0).min_w(0.0))
            .children(viewer_col(s, k))
    } else {
        row().w_full().h_full().fill(k.c.page).child(page.grow(1.0))
    };
    crate::theme_picker::picker(s, k, &mut layer);
    rune_kit::root(body, layer)
}

/// The viewer pane with its divider, on thread pages only.
fn viewer_col(s: &State, k: &Kit) -> Vec<El> {
    if !(s.viewer && s.page == Page::Thread) {
        return Vec::new();
    }
    vec![
        el().w(1.0).h_full().fill(k.c.border_base),
        crate::viewer::viewer(s, k),
    ]
}

const PER_PROJECT: usize = 5;
const PROJECTS: usize = 12;

/// "now", "5m", "9h", "2d": how long ago a thread was touched.
pub fn ago(updated_at: &str) -> String {
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(updated_at) else {
        return String::new();
    };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc))
        .num_seconds()
        .max(0);
    match secs {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn nav_row(
    k: &Kit,
    label: &str,
    icon: &'static rune::Icon,
    selected: bool,
    key: &str,
    f: impl Fn(&mut State) + 'static,
) -> El {
    SidebarRow::new(label)
        .icon(icon)
        .selected(selected)
        .height(32.0)
        .on_select(move |s: &mut State, _| f(s))
        .build(k)
        .key(key.to_string())
}

fn thread_row(s: &State, k: &Kit, sess: &SessionInfo, key: String, with_project: bool) -> El {
    let id = sess.session_id.clone();
    let hide_id = sess.session_id.clone();
    let open_cwd = sess.cwd.clone();
    let selected = s.page == Page::Thread && s.current.as_deref() == Some(sess.session_id.as_str());
    let mut r = SidebarRow::new(session_title(sess))
        .selected(selected)
        .on_select(move |s: &mut State, _| {
            s.project = Some(open_cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        });
    let waiting = s
        .items
        .iter()
        .any(|i| matches!(i, Item::Approval { decided: None, .. }));
    // The running thread keeps its spinner even when another thread (or Home) is in view.
    let running = s.current.as_deref() == Some(sess.session_id.as_str());
    if running && waiting {
        r = r.status(RowStatus::Attention);
    } else if running && s.busy {
        r = r.status(RowStatus::Busy);
    }
    let age = ago(&sess.updated_at);
    r = if with_project {
        r.detail(format!("{} · {age}", project_name(&sess.cwd)))
    } else {
        r.detail(age)
    };
    // Hover reveals an × that removes the thread from the sidebar.
    r = r.on_close(move |s: &mut State, _| s.hide_thread(&hide_id));
    r.height(32.0).build(k).key(key)
}

fn sidebar(s: &State, k: &Kit) -> El {
    let brand = text("dex")
        .font(k.ty.body_medium)
        .color(k.c.text_base)
        .px(10.0)
        .key("brand");
    let mut sb = Sidebar::new()
        .label("Navigation")
        .header_padding(12.0, 8.0)
        .header(brand)
        .header(nav_row(
            k,
            "Home",
            &crate::icons::HOME,
            s.page == Page::Home,
            "nav-home",
            |s| {
                s.page = Page::Home;
            },
        ))
        .header(nav_row(
            k,
            "New",
            &crate::icons::PLUS,
            false,
            "nav-new",
            |s| {
                s.page = Page::Thread;
                s.send_cmd(Cmd::NewSession);
            },
        ))
        .header(nav_row(
            k,
            "Search",
            &crate::icons::SEARCH,
            s.searching,
            "nav-search",
            |s| {
                s.searching = !s.searching;
                if !s.searching {
                    s.search.replace_text("");
                }
            },
        ));
    if s.searching {
        sb = sb.header(
            text_input(&s.search, k.ty.ui.lh(20.0))
                .key("search-input")
                .placeholder("Search threads…", k.c.text_faint)
                .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
                .color(k.c.text_base)
                .single_line()
                .on_edit(|s: &mut State| &mut s.search)
                .w_full()
                .px(10.0)
                .py(6.0),
        );
    }

    let query = s.search.text.trim().to_lowercase();
    if s.searching && !query.is_empty() {
        for (i, sess) in s
            .sessions
            .iter()
            .filter(|x| session_title(x).to_lowercase().contains(&query))
            .take(30)
            .enumerate()
        {
            sb = sb.child(thread_row(s, k, sess, format!("found{i}"), true));
        }
        return sb.build(k).key("sidebar-root");
    }

    sb = sb.child(
        row()
            .key("projects-label")
            .w_full()
            .px(10.0)
            .pt(14.0)
            .pb(4.0)
            .child(crate::home::caps(k, "PROJECTS")),
    );

    // Projects: the active one first, then by most recent thread. The daemon lists every
    // directory's threads; empty ones are skipped.
    let current = crate::worktree::project_root(&s.cwd());
    let mut projects: Vec<(String, Vec<&SessionInfo>)> = Vec::new();
    for sess in s
        .sessions
        .iter()
        .filter(|x| x.message_count > 0 && !x.cwd.is_empty() && s.is_visible(x))
    {
        let root = crate::worktree::project_root(&sess.cwd);
        match projects.iter_mut().find(|(c, _)| *c == root) {
            Some((_, v)) => v.push(sess),
            None => projects.push((root, vec![sess])),
        }
    }
    if let Some(i) = projects.iter().position(|(c, _)| *c == current) {
        let p = projects.remove(i);
        projects.insert(0, p);
    } else if !current.is_empty() {
        projects.insert(0, (current.clone(), Vec::new()));
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
        let name = project_name(cwd);
        let color = crate::theme::project_color(&name);
        let add = {
            let cwd = cwd.clone();
            IconButton::new(&icons::PLUS)
                .variant(IconButtonVariant::Ghost)
                .label("New thread in project")
                .on_press(move |s: &mut State| {
                    s.project = Some(cwd.clone());
                    s.page = Page::Thread;
                    s.send_cmd(Cmd::NewSession);
                })
                .build(k)
                .key("add")
        };
        let toggle_cwd = cwd.clone();
        let chevron = if collapsed {
            k.glyph.triangle_right
        } else {
            k.glyph.triangle_down
        };
        let hide_cwd = cwd.clone();
        let header = k
            .interactive(
                row()
                    .key(format!("proj{pi}"))
                    .hover_group()
                    .w_full()
                    .h(34.0)
                    .px(10.0)
                    .gap(6.0)
                    .items_center()
                    .radius(k.radius.md)
                    .child(
                        // A project is a small caps label with its colour dot, not a row.
                        row()
                            .gap(6.0)
                            .items_center()
                            .grow(1.0)
                            .min_w(0.0)
                            .child(el().w(6.0).h(6.0).radius(3.0).fill(color))
                            .child(
                                text(name.to_uppercase())
                                    .font(rune::TextStyle {
                                        size: 11.0,
                                        tracking: 0.9,
                                        ..k.ty.ui_medium
                                    })
                                    .color(k.c.text_faint)
                                    .truncate()
                                    .grow(1.0)
                                    .min_w(0.0),
                            ),
                    )
                    .child(add)
                    .child(
                        IconButton::new(&icons::CLOSE)
                            .variant(IconButtonVariant::GhostMuted)
                            .label("Remove project from sidebar")
                            .on_press(move |s: &mut State| s.hide_project(&hide_cwd))
                            .build(k)
                            .key(format!("hide{pi}"))
                            .group_opacity(0.0, 1.0),
                    )
                    .child(rune::icon(chevron, 12.0).color(color)),
            )
            .on_press(move |s: &mut State| {
                if !s.collapsed.remove(&toggle_cwd) {
                    s.collapsed.insert(toggle_cwd.clone());
                }
            });
        sb = sb.child(header);
        if collapsed {
            continue;
        }
        let shown = if all {
            threads.len()
        } else {
            threads.len().min(PER_PROJECT)
        };
        for (ti, sess) in threads.iter().take(shown).enumerate() {
            // Threads sit indented under their project label.
            sb = sb.child(row().w_full().pl(14.0).child(thread_row(
                s,
                k,
                sess,
                format!("t{pi}-{ti}"),
                false,
            )));
        }
        if threads.len() > PER_PROJECT {
            let more_cwd = cwd.clone();
            let label = if all {
                "Show less".to_string()
            } else {
                format!("Show {} more", threads.len() - PER_PROJECT)
            };
            sb = sb.child(
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
        .fill(k.c.page)
        .child(header(s, k))
        .child(transcript(s, k))
        .child(composer(s, k, layer))
        .child(footer(s, k));
    card.grow(1.0).min_h(0.0)
}

fn header(s: &State, k: &Kit) -> El {
    let toggle = IconButton::new(&crate::icons::PANEL)
        .variant(IconButtonVariant::Ghost)
        .label("Toggle sidebar")
        .on_press(|s: &mut State| s.sidebar = !s.sidebar)
        .build(k)
        .key("toggle-sidebar");
    let title = text(s.title())
        .font(k.ty.ui_medium)
        .color(k.c.text_base)
        .truncate();
    let wt = crate::worktree::info(&s.cwd());
    let mut stats = row().gap(8.0).items_center();
    if let Some(w) = &wt {
        stats = stats
            .child(text(w.branch.clone()).font(k.ty.mono).color(k.c.text_muted))
            .child(
                Button::new("Merge")
                    .variant(ButtonVariant::Submit)
                    .on_press(|s: &mut State| s.finish_worktree(true))
                    .build(k)
                    .key("wt-merge"),
            )
            .child(if s.confirm_discard {
                Button::new("Discard work?")
                    .variant(ButtonVariant::Danger)
                    .on_press(|s: &mut State| s.finish_worktree(false))
                    .build(k)
                    .key("wt-discard-yes")
            } else {
                Button::new("Discard")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut State| s.confirm_discard = true)
                    .build(k)
                    .key("wt-discard")
            });
    } else if s.current.is_none() {
        stats = stats.child(
            Button::new("Worktree")
                .variant(if s.use_worktree {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Ghost
                })
                .on_press(|s: &mut State| s.use_worktree = !s.use_worktree)
                .build(k)
                .key("wt-toggle"),
        );
    }
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
        .child(
            IconButton::new(&crate::icons::SLIDERS)
                .variant(IconButtonVariant::Ghost)
                .label("Theme (Ctrl+,)")
                .on_press(|s: &mut State| s.theme_open = true)
                .build(k)
                .key("open-theme"),
        )
        .child(
            IconButton::new(&crate::icons::TERMINAL)
                .variant(IconButtonVariant::Ghost)
                .label("Terminal (Ctrl+`)")
                .on_press(|s: &mut State| s.show_viewer(VTab::Terminal))
                .build(k)
                .key("open-term"),
        )
        .child(
            IconButton::new(&crate::icons::FOLDER)
                .variant(IconButtonVariant::Ghost)
                .label("Files (Ctrl+E)")
                .on_press(|s: &mut State| s.show_viewer(VTab::Files))
                .build(k)
                .key("open-files"),
        )
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
    // Items breathe: 22 points between them, and a turn's user message starts a new group.
    let mut list = col().w_full().max_w(COLUMN).gap(22.0).py(24.0).px(20.0);
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
        let live = s.busy && i + 1 == s.items.len();
        let mut el = item(i, it, k, live).key(format!("item{i}"));
        if i > 0 && matches!(it, Item::User(_)) {
            el = col().w_full().gap(0.0).child(el.pt(10.0));
        }
        list = list.child(el);
    }
    // The viewport must be a column: a row would stretch its child to the viewport's height,
    // so the content would never overflow and nothing could scroll.
    col()
        .key("transcript")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .child(row().w_full().justify_center().child(list.grow(1.0)))
}

fn item(i: usize, it: &Item, k: &Kit, live: bool) -> El {
    match it {
        Item::User(t) => row().w_full().justify_end().child(
            col()
                .max_w(560.0)
                .px(16.0)
                .py(12.0)
                .radius(k.radius.xl)
                .fill(k.c.layer_02)
                .child(text(t.clone()).font(k.ty.body).color(k.c.text_base).wrap()),
        ),
        Item::Assistant { text: t, streaming } => Markdown::new(t.clone())
            .streaming(*streaming)
            .build(k)
            .w_full(),
        Item::Thinking { text: t, open } => {
            Collapsible::new(if live { "Thinking" } else { "Thought" }, *open)
                .child(text(t.clone()).font(k.ty.ui).color(k.c.text_muted))
                .on_toggle(move |s: &mut State| {
                    if let Some(Item::Thinking { open, .. }) = s.items.get_mut(i) {
                        *open = !*open;
                    }
                })
                .build(k)
        }
        Item::Tool(t) => {
            // A compact row, as in the transcript design: the tool's name, its arguments and the
            // duration. Its output follows dimmed under a `↳`: the first line when closed, all of it
            // when opened.
            let failed = matches!(&t.done, Some(d) if !d.ok);
            let secs = match &t.done {
                Some(d) => format!("{:.1}s", d.secs),
                None => String::new(),
            };
            let label = row()
                .gap(0.0)
                .child(
                    text(t.name.clone())
                        .font(k.ty.mono)
                        .color(crate::theme::tool_label()),
                )
                .child(text(":").font(k.ty.mono).color(crate::theme::tool_label()));
            let head = row()
                .key(format!("tool{i}"))
                .w_full()
                .h(24.0)
                .gap(6.0)
                .items_center()
                .child(label)
                .child(
                    text(t.args.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_muted)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(text(secs).font(k.ty.mono).color(if failed {
                    k.c.danger.fg
                } else {
                    k.c.text_faint
                }));
            let mut c = col().w_full().gap(2.0).child(k.interactive(head).on_press(
                move |s: &mut State| {
                    if let Some(Item::Tool(t)) = s.items.get_mut(i) {
                        t.open = !t.open;
                    }
                },
            ));
            if let Some(d) = &t.done {
                let ink = if failed {
                    k.c.danger.fg
                } else {
                    k.c.text_faint
                };
                let lines: Vec<&String> = if t.open {
                    std::iter::once(&d.summary)
                        .chain(d.preview.iter())
                        .collect()
                } else {
                    d.preview.iter().take(1).collect()
                };
                for (n, l) in lines.into_iter().enumerate() {
                    let arrow = if n == 0 { "↳" } else { " " };
                    c = c.child(
                        row()
                            .w_full()
                            .gap(8.0)
                            .pl(4.0)
                            .child(text(arrow).font(k.ty.mono).color(k.c.text_faint))
                            .child(
                                text(l.clone())
                                    .font(k.ty.mono)
                                    .color(ink)
                                    .truncate()
                                    .grow(1.0)
                                    .min_w(0.0),
                            ),
                    );
                }
            }
            c
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
        .on_input_key(|s: &mut State, e: &rune::KeyEvent, _| {
            // Alt+Enter queues a follow-up that runs after the current turn.
            if matches!(e.key, rune::Key::Enter) && e.mods.alt {
                s.queue_followup();
                return true;
            }
            false
        })
        .w_full()
        .min_h(44.0)
        .max_h(220.0);

    let mode = crate::composer_menu::mode(s, k, layer);
    let model = crate::composer_menu::model(s, k, layer);

    let action = if s.busy {
        IconButton::new(&icons::STOP)
            .variant(IconButtonVariant::Ghost)
            .label("Stop")
            .on_press(|s: &mut State| s.send_cmd(Cmd::Cancel))
            .build(k)
    } else {
        IconButton::new(&crate::icons::UP)
            .variant(IconButtonVariant::Submit)
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
    let mut stack = col().w_full().max_w(COLUMN).gap(6.0);
    for (i, q) in s.queue.iter().enumerate() {
        let label = if q.followup { "Queued" } else { "Steering" };
        stack = stack.child(
            row()
                .key(format!("queued{i}"))
                .w_full()
                .px(12.0)
                .py(4.0)
                .gap(8.0)
                .items_center()
                .radius(k.radius.lg)
                .border(1.0, k.c.border_muted)
                .child(text(label).font(k.ty.small).color(k.c.text_faint))
                .child(
                    text(q.text.clone())
                        .font(k.ty.ui)
                        .color(k.c.text_muted)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    IconButton::new(&icons::CLOSE)
                        .variant(IconButtonVariant::Ghost)
                        .label("Take back")
                        .on_press(move |s: &mut State| s.recall(i))
                        .build(k)
                        .key("recall"),
                ),
        );
    }
    let stack = stack.child(card);
    row()
        .w_full()
        .justify_center()
        .px(16.0)
        .pb(8.0)
        .child(stack.grow(1.0))
}

/// The line under the composer: the project path and branch on the left, the agent's status
/// on the right. It sits in the same centred column as the composer, so their edges line up.
fn footer(s: &State, k: &Kit) -> El {
    let meta = TextStyle {
        size: 12.0,
        ..k.ty.mono
    };
    let mut left = row().grow(1.0).min_w(0.0).gap(6.0).items_center().child(
        text(s.cwd())
            .font(meta)
            .color(k.c.text_faint)
            .truncate()
            .shrink(),
    );
    if let Some(b) = &s.info.branch {
        left = left
            .child(text("·").font(k.ty.small).color(k.c.border_strong))
            .child(rune::icon(&crate::icons::BRANCH, 12.0).color(k.c.text_faint))
            .child(text(b.clone()).font(meta).color(k.c.text_faint).truncate());
    }
    let ready = s.status == "ready";
    let dot = if ready {
        crate::theme::done()
    } else {
        crate::theme::waiting()
    };
    let status = row()
        .gap(6.0)
        .items_center()
        .child(el().size(6.0, 6.0).radius(3.0).fill(dot))
        .child(
            text(format!("{} · {}", s.info.provider, s.status))
                .font(TextStyle {
                    size: 12.0,
                    ..k.ty.small
                })
                .color(k.c.text_faint),
        );
    row().w_full().justify_center().child(
        row()
            .w_full()
            .max_w(COLUMN)
            .h(36.0)
            .px(16.0)
            .pb(8.0)
            .gap(8.0)
            .items_center()
            .child(left)
            .child(status),
    )
}
