//! Sidebar: layout tokens, small reusable pieces (nav row, project header, thread row, "show
//! more" link) and the assembly. Every size and gap comes from `tok`, so rows stay aligned.

use dex_client::protocol::SessionInfo;
use rune::{El, row, spacer, text, text_input};
use rune_kit::shell::{RowStatus, Sidebar, SidebarRow};
use rune_kit::{Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, Spinner};
use rune_theme_opencode::icons as kit_icons;

use crate::icons;
use crate::state::{Cmd, Item, Page, State, project_name, session_title};
use crate::style::{caps, dot, space, typo};

/// Layout tokens, in points. One row height, one inset, one gap: rows never drift apart.
pub mod tok {
    pub const WIDTH: f32 = 280.0;
    /// Every row: nav, project header, thread.
    pub const ROW_H: f32 = 32.0;
    /// Space between the sidebar edge and a row's content.
    pub const INSET: f32 = super::space::LG;
    /// Gap between items inside a row.
    pub const GAP: f32 = super::space::SM;
    /// Fixed, right-aligned column for a thread's time, just left of the slot.
    pub const TIME_W: f32 = 24.0;
    /// The slot before a project's name: holds the dot, or a spinner while it runs.
    pub const LEAD: f32 = 8.0;
    /// Project dot diameter.
    pub const DOT: f32 = 6.0;
    /// Threads sit under their project's chevron + dot.
    pub const INDENT: f32 = 14.0;
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

/// A navigation row: icon + label.
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
        .height(tok::ROW_H)
        .on_select(move |s: &mut State, _| f(s))
        .build(k)
        .key(key.to_string())
}

/// A thread row: title, status, and a right-aligned time (or `project · time` in search).
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
        r.trailing(time_col(k, age))
    };
    // Hover reveals an × that removes the thread from the sidebar.
    r = r.on_close(move |s: &mut State, _| s.hide_thread(&hide_id));
    r.height(tok::ROW_H).build(k).key(key)
}

/// A thread's time: a fixed-width column, text pushed to its right edge, so `2h` and `1d` align.
fn time_col(k: &Kit, age: String) -> El {
    row()
        .key("time")
        .w(tok::TIME_W)
        .child(spacer().grow(1.0))
        .child(text(age).font(typo::time(k)).color(k.c.text_faint))
}

/// What a project header needs: pressing anywhere on it toggles `collapsed`.
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
    // One fixed slot, so the name never shifts between the dot and the spinner.
    let lead = row()
        .size(tok::LEAD, tok::LEAD)
        .center()
        .child(if p.running {
            Spinner::new().size(tok::LEAD).build(k)
        } else {
            dot(p.color, tok::DOT)
        });
    k.interactive(
        row()
            .key(p.key.clone())
            .hover_group()
            .w_full()
            .h(tok::ROW_H)
            .px(tok::INSET)
            .gap(tok::GAP)
            .items_center()
            .radius(k.radius.md)
            .child(
                row()
                    .gap(space::XS)
                    .items_center()
                    .grow(1.0)
                    .min_w(0.0)
                    .child(lead)
                    .child(
                        text(p.name.to_uppercase())
                            .font(typo::caps(k))
                            .color(k.c.text_faint)
                            .truncate()
                            .grow(1.0)
                            .min_w(0.0),
                    ),
            )
            .child(
                IconButton::new(&kit_icons::CLOSE)
                    .control_size(ControlSize::Small)
                    .variant(IconButtonVariant::GhostMuted)
                    .label("Remove project from sidebar")
                    .on_press(move |s: &mut State| s.hide_project(&hide))
                    .build(k)
                    .key(format!("{}-hide", p.key))
                    .group_opacity(0.0, 1.0),
            )
            .child(
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
            ),
    )
    .on_press(move |s: &mut State| {
        if !s.collapsed.remove(&toggle) {
            s.collapsed.insert(toggle.clone());
        }
    })
}

/// A quiet text button under a list ("Show 2 more").
fn more_link(k: &Kit, key: String, label: String, f: impl Fn(&mut State) + 'static) -> El {
    Button::new(label)
        .variant(ButtonVariant::Ghost)
        .on_press(f)
        .build(k)
        .key(key)
}

/// Threads grouped by project root: the active project first, then by most recent thread.
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
        .px(tok::INSET)
        .key("brand");
    let mut sb = Sidebar::new()
        .label("Navigation")
        .header_padding(12.0, 8.0)
        .header(brand)
        .header(nav_row(
            k,
            "Home",
            &icons::HOME,
            s.page == Page::Home,
            "nav-home",
            |s| {
                s.page = Page::Home;
            },
        ))
        .header(nav_row(k, "New", &icons::PLUS, false, "nav-new", |s| {
            s.page = Page::Thread;
            s.send_cmd(Cmd::NewSession);
        }))
        .header(nav_row(
            k,
            "Search",
            &icons::SEARCH,
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
                .px(tok::INSET)
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

    sb = sb.child(
        row()
            .key("projects-label")
            .w_full()
            .px(tok::INSET)
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
                running: s.busy
                    && threads
                        .iter()
                        .any(|t| s.current.as_deref() == Some(t.session_id.as_str())),
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
            sb = sb.child(row().w_full().pl(tok::INDENT).child(thread_row(
                s,
                k,
                sess,
                format!("t{pi}-{ti}"),
                false,
            )));
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
