//! Sidebar, on one grid (see DESIGN.md): a 16 lead column (nav icon, project dot or spinner, a
//! running thread's spinner), a text column (labels, project names, titles) and a trailing edge
//! (a 20 slot for + and ×, then the time column). Every row is built from the same chrome, so
//! their positions cannot drift. All sizes come from `tok`.

use dex_client::protocol::SessionInfo;
use rune::{El, Key, row, spacer, text, text_input};
use rune_kit::shell::Sidebar;
use rune_kit::{ControlSize, IconButton, IconButtonVariant, Kit, Spinner};
use rune_theme_opencode::icons as kit_icons;

use crate::icons;
use crate::state::{Cmd, Item, NeedKind, Page, State, project_name, session_title};
use crate::style::{caps, dot, space, typo};

/// Layout tokens, in points.
pub mod tok {
    pub const WIDTH: f32 = 280.0;
    /// Space between the sidebar edge and a row.
    pub const MARGIN: f32 = 12.0;
    /// Space between a row's edge and its content: content starts at MARGIN + PAD = 22.
    pub const PAD: f32 = 10.0;
    /// Every row: nav, project header, thread.
    pub const ROW_H: f32 = 32.0;
    /// The lead column: nav icon, project dot / spinner, thread spinner (centred in it).
    pub const LEAD: f32 = 16.0;
    /// Between the lead column and the text column (text starts at 22 + 16 + 8 = 46).
    pub const LEAD_GAP: f32 = 8.0;
    /// The trailing slot: + on a project, × on hover.
    pub const SLOT: f32 = 20.0;
    pub const DOT: f32 = 6.0;
    pub const SPINNER: f32 = 8.0;
    pub const PER_PROJECT: usize = 5;
    pub const PROJECTS: usize = 12;
}

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

/// The chrome every row shares: 32 tall, 10 padding, lead column then text column, hover and
/// (when selected) the pressed fill.
fn shell(k: &Kit, key: String, selected: bool) -> El {
    let r = k.interactive(
        row()
            .key(key)
            .hover_group()
            .w_full()
            .h(tok::ROW_H)
            .px(tok::PAD)
            .gap(tok::LEAD_GAP)
            .items_center()
            .radius(k.radius.md),
    );
    if selected { r.fill(k.c.pressed) } else { r }
}

/// A fixed `size` x `size` box centring its content.
fn boxed(size: f32, child: El) -> El {
    row().size(size, size).center().child(child)
}

fn lead(child: El) -> El {
    boxed(tok::LEAD, child)
}

/// An empty lead column, so text lines up when a row has no icon.
fn lead_gap() -> El {
    row().size(tok::LEAD, tok::LEAD)
}

fn spinner(k: &Kit) -> El {
    Spinner::new().size(tok::SPINNER).build(k)
}

/// A navigation row: icon in the lead column, label in the text column, optional hint.
fn nav_row(
    k: &Kit,
    label: &str,
    icon: &'static rune::Icon,
    hint: Option<&str>,
    selected: bool,
    key: &str,
    f: impl Fn(&mut State) + 'static,
) -> El {
    let (ink, glyph) = if selected {
        (k.c.text_base, k.c.icon_base)
    } else {
        (k.c.text_muted, k.c.icon_muted)
    };
    let mut r = shell(k, key.to_string(), selected)
        .child(lead(rune::icon(icon, 16.0).color(glyph)))
        .child(
            text(label.to_string())
                .font(k.ty.ui_medium)
                .color(ink)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
    if let Some(h) = hint {
        r = r.child(
            text(h.to_string())
                .font(typo::time(k))
                .color(k.c.text_faint),
        );
    }
    r.on_press(move |s: &mut State| f(s))
}

/// A thread row: running spinner (or attention dot) in the lead column, title in the text
/// column, × (hide) in the slot on hover, time right-aligned at the row's trailing edge (the
/// same edge the `+` on a project header sits on). In search results the time is
/// `project · age`.
fn thread_row(s: &State, k: &Kit, sess: &SessionInfo, key: String, with_project: bool) -> El {
    thread_row_as(s, k, sess, key, with_project, None, None)
}

/// A thread row, optionally with its own title (a NEEDS YOU prompt) and lead mark.
fn thread_row_as(
    s: &State,
    k: &Kit,
    sess: &SessionInfo,
    key: String,
    with_project: bool,
    title: Option<String>,
    status: Option<NeedKind>,
) -> El {
    let id = sess.session_id.clone();
    let hide_id = sess.session_id.clone();
    let open_cwd = sess.cwd.clone();
    let selected = s.page == Page::Thread && s.current.as_deref() == Some(sess.session_id.as_str());
    let is_open = s.current.as_deref() == Some(sess.session_id.as_str());
    let waiting = if is_open {
        s.items
            .iter()
            .any(|i| matches!(i, Item::Approval { decided: None, .. }))
    } else {
        s.needs
            .iter()
            .any(|n| n.id == sess.session_id && n.kind == NeedKind::Waiting)
    };
    // A running thread keeps its spinner even when it is in the background.
    let running = s.running.contains(&sess.session_id) || (is_open && s.busy);
    let mark = if let Some(kind) = status {
        let color = match kind {
            NeedKind::Waiting => crate::theme::waiting(),
            NeedKind::Done => crate::theme::done(),
            NeedKind::Failed => k.c.danger.fg,
        };
        lead(dot(color, tok::DOT))
    } else if waiting {
        lead(dot(crate::theme::waiting(), tok::DOT))
    } else if running {
        lead(spinner(k))
    } else {
        lead_gap()
    };
    let age = ago(&sess.updated_at);
    let time = if with_project {
        text(format!("{} · {age}", project_name(&sess.cwd)))
    } else {
        // Right-aligned in the trailing slot, flush with the `+` a project header ends with.
        row()
            .w(tok::SLOT)
            .child(spacer().grow(1.0))
            .child(text(age).font(typo::time(k)).color(k.c.text_faint))
    };
    // A renaming row shows a text field instead of its title, bound to the state's edit.
    let edit = (s.renaming.as_deref() == Some(sess.session_id.as_str())).then(|| s.rename.clone());
    thread_chrome(
        k, key, selected, mark, sess, title, id, open_cwd, hide_id, edit, time,
    )
}

/// A thread row's chrome. `edit` (`Some` while this row is being renamed) swaps the title text
/// for an editable field: Return commits, Escape cancels, a press elsewhere ends it.
#[allow(clippy::too_many_arguments)]
fn thread_chrome(
    k: &Kit,
    key: String,
    selected: bool,
    mark: El,
    sess: &SessionInfo,
    title: Option<String>,
    id: String,
    open_cwd: String,
    hide_id: String,
    edit: Option<rune::TextEdit>,
    time: El,
) -> El {
    let ink = if selected {
        k.c.text_base
    } else {
        k.c.text_muted
    };
    let font = if selected { k.ty.ui_medium } else { k.ty.ui };
    let rename_id = id.clone();
    let title = match edit {
        Some(edit) => text_input(&edit, font)
            .single_line()
            .key(format!("{key}-rename"))
            .grow(1.0)
            .shrink()
            .min_w(0.0)
            .color(k.c.text_base)
            .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
            .on_edit(|s: &mut State| &mut s.rename)
            .on_submit(|s: &mut State, _| s.commit_rename())
            .on_input_key(|s: &mut State, ev, _| {
                if ev.key == Key::Escape {
                    s.cancel_rename();
                    true
                } else {
                    false
                }
            }),
        None => text(title.unwrap_or_else(|| session_title(sess)))
            .font(font)
            .color(ink)
            .truncate()
            .grow(1.0)
            .min_w(0.0),
    };
    shell(k, key.clone(), selected)
        .child(mark)
        .child(title)
        .child(boxed(
            tok::SLOT,
            IconButton::new(&kit_icons::CLOSE)
                .control_size(ControlSize::Small)
                .variant(IconButtonVariant::GhostMuted)
                .label("Hide thread from sidebar")
                .on_press(move |s: &mut State| s.hide_thread(&hide_id))
                .build(k)
                .key(format!("{key}-hide"))
                .group_opacity(0.0, 1.0),
        ))
        .child(time)
        .on_press(move |s: &mut State| {
            // A press during a rename ends the edit (Return would too); don't jump threads.
            if s.renaming.is_some() {
                s.commit_rename();
                return;
            }
            s.project = Some(open_cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        })
        .on_double_press(move |s: &mut State| {
            if s.renaming.is_none() {
                s.start_rename(&rename_id);
            }
        })
}

/// What a project header needs: pressing anywhere on it toggles collapse.
struct ProjectHeader {
    key: String,
    name: String,
    color: rune::Color,
    /// A thread in this project is running: its dot becomes a spinner.
    running: bool,
    cwd: String,
}

/// `[dot|spinner] NAME ........ [×] [+]`: no chevron, the whole row toggles; × shows on hover.
fn project_header(k: &Kit, p: ProjectHeader) -> El {
    let (toggle, add, hide) = (p.cwd.clone(), p.cwd.clone(), p.cwd.clone());
    let mark = if p.running {
        spinner(k)
    } else {
        dot(p.color, tok::DOT)
    };
    shell(k, p.key.clone(), false)
        .child(lead(mark))
        .child(
            text(p.name.to_uppercase())
                .font(typo::caps(k))
                .color(k.c.text_faint)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        )
        .child(boxed(
            tok::SLOT,
            IconButton::new(&kit_icons::CLOSE)
                .control_size(ControlSize::Small)
                .variant(IconButtonVariant::GhostMuted)
                .label("Remove project from sidebar")
                .on_press(move |s: &mut State| s.hide_project(&hide))
                .build(k)
                .key(format!("{}-hide", p.key))
                .group_opacity(0.0, 1.0),
        ))
        .child(boxed(
            tok::SLOT,
            IconButton::new(&kit_icons::PLUS)
                .control_size(ControlSize::Small)
                .variant(IconButtonVariant::Ghost)
                .label("New thread in project")
                .on_press(move |s: &mut State| {
                    s.project = Some(add.clone());
                    s.page = Page::Thread;
                    s.send_cmd(Cmd::NewSession);
                })
                .build(k)
                .key(format!("{}-add", p.key)),
        ))
        .on_press(move |s: &mut State| {
            if !s.collapsed.remove(&toggle) {
                s.collapsed.insert(toggle.clone());
            }
        })
}

/// A quiet row under a list ("Show 2 more"), its text in the text column.
fn more_link(k: &Kit, key: String, label: String, f: impl Fn(&mut State) + 'static) -> El {
    shell(k, key, false)
        .child(lead_gap())
        .child(text(label).font(typo::meta(k)).color(k.c.text_faint))
        .on_press(move |s: &mut State| f(s))
}

fn projects(s: &State) -> Vec<(String, Vec<&SessionInfo>)> {
    let current = crate::worktree::project_root(&s.cwd());
    let mut out: Vec<(String, Vec<&SessionInfo>)> = Vec::new();
    for sess in s
        .sessions
        .iter()
        .filter(|x| x.message_count > 0 && !x.cwd.is_empty() && s.is_visible(x))
    {
        let root = crate::worktree::project_root(&sess.cwd);
        match out.iter_mut().find(|(c, _)| *c == root) {
            Some((_, v)) => v.push(sess),
            None => out.push((root, vec![sess])),
        }
    }
    if let Some(i) = out.iter().position(|(c, _)| *c == current) {
        let p = out.remove(i);
        out.insert(0, p);
    } else if !current.is_empty() {
        out.insert(0, (current, Vec::new()));
    }
    out
}

pub fn sidebar(s: &State, k: &Kit) -> El {
    let brand = text("dex")
        .font(k.ty.body_medium)
        .color(k.c.text_base)
        .px(tok::PAD)
        .key("brand");
    let mut sb = Sidebar::new()
        .label("Navigation")
        .inset(tok::MARGIN)
        .header_padding(12.0, 8.0)
        .header(brand)
        .header(nav_row(
            k,
            "Home",
            &icons::HOME,
            None,
            s.page == Page::Home,
            "nav-home",
            |s| {
                s.page = Page::Home;
            },
        ))
        .header(nav_row(
            k,
            "New thread",
            &icons::PLUS,
            Some("Ctrl N"),
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
            &icons::SEARCH,
            None,
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
                .px(tok::PAD)
                .py(space::SM),
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

    // Threads waiting on the user (approval, question) or finished/failed in the background.
    let needs: Vec<_> = s
        .needs
        .iter()
        .filter_map(|n| {
            s.sessions
                .iter()
                .find(|x| x.session_id == n.id)
                .map(|x| (n, x))
        })
        .collect();
    if !needs.is_empty() {
        sb = sb.child(
            row()
                .key("needs-label")
                .w_full()
                .px(tok::PAD)
                .pt(14.0)
                .pb(space::XS)
                .child(caps(k, "NEEDS YOU")),
        );
        for (i, (n, sess)) in needs.into_iter().enumerate() {
            let title = n.label.clone();
            sb = sb.child(thread_row_as(
                s,
                k,
                sess,
                format!("need{i}"),
                false,
                title,
                Some(n.kind),
            ));
        }
    }

    sb = sb.child(
        row()
            .key("projects-label")
            .w_full()
            .px(tok::PAD)
            .pt(14.0)
            .pb(space::XS)
            .child(caps(k, "PROJECTS")),
    );

    let projects = projects(s);
    let hidden = projects.len().saturating_sub(tok::PROJECTS);
    let take = if s.all_projects {
        projects.len()
    } else {
        projects.len().min(tok::PROJECTS)
    };
    for (pi, (cwd, threads)) in projects.iter().take(take).enumerate() {
        let collapsed = s.collapsed.contains(cwd);
        let name = project_name(cwd);
        sb = sb.child(project_header(
            k,
            ProjectHeader {
                key: format!("proj{pi}"),
                color: crate::theme::project_color(&name),
                name,
                running: threads.iter().any(|t| {
                    s.running.contains(&t.session_id)
                        || (s.busy && s.current.as_deref() == Some(t.session_id.as_str()))
                }),
                cwd: cwd.clone(),
            },
        ));
        if collapsed {
            continue;
        }
        let all = s.show_all.contains(cwd);
        let shown = if all {
            threads.len()
        } else {
            threads.len().min(tok::PER_PROJECT)
        };
        for (ti, sess) in threads.iter().take(shown).enumerate() {
            sb = sb.child(thread_row(s, k, sess, format!("t{pi}-{ti}"), false));
        }
        if threads.len() > tok::PER_PROJECT {
            let more_cwd = cwd.clone();
            let label = if all {
                "Show less".to_string()
            } else {
                format!("Show {} more", threads.len() - tok::PER_PROJECT)
            };
            sb = sb.child(more_link(k, format!("more{pi}"), label, move |s| {
                if !s.show_all.remove(&more_cwd) {
                    s.show_all.insert(more_cwd.clone());
                }
            }));
        }
    }
    if hidden > 0 {
        let label = if s.all_projects {
            "Show fewer projects".to_string()
        } else {
            format!("Show {hidden} more projects")
        };
        sb = sb.child(more_link(k, "more-projects".into(), label, |s| {
            s.all_projects = !s.all_projects
        }));
    }
    if !s.sessions_loaded {
        sb = sb.child(
            text("Loading threads…")
                .font(typo::caption(k))
                .color(k.c.text_faint)
                .px(space::XL)
                .key("loading"),
        );
    }
    sb.build(k).key("sidebar-root")
}
