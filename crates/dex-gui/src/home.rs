//! The landing page, as drawn in Paper (`DESIGN.md`, dex / Home v2): a greeting, the threads
//! touched in the last two days, a column of counts, and the start actions.

use chrono::{Local, NaiveDate, Timelike};
use rune::{El, ScrollSpec, col, el, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit};

use crate::icons;
use crate::sidebar::ago;
use crate::state::{Cmd, Page, State, project_name, session_title};
use crate::style::{caps, rule, space, typo};
use crate::theme;

fn local_date(updated_at: &str) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .ok()
        .map(|t| t.with_timezone(&Local).date_naive())
}

fn stat(k: &Kit, n: usize, label: &str, color: rune::Color) -> El {
    col()
        .w_full()
        .gap(space::SM)
        .py(18.0)
        .child(text(n.to_string()).font(typo::stat(k)).color(color))
        .child(
            text(label.to_string())
                .font(typo::caption(k))
                .color(k.c.text_faint),
        )
        .child(rule(k).mt(18.0))
}

fn action(k: &Kit, label: &str, primary: bool, key: &str, f: impl Fn(&mut State) + 'static) -> El {
    let (fill, ink) = if primary {
        (theme::accent(), theme::on_accent())
    } else {
        (rune::Color::TRANSPARENT, k.c.text_base)
    };
    let mut b = row()
        .key(key.to_string())
        .px(space::XXL)
        .py(space::LG)
        .gap(space::MD)
        .items_center()
        .radius(k.radius.md)
        .fill(fill);
    if primary {
        b = b.child(rune::icon(&icons::PLUS, 16.0).color(ink));
    } else {
        b = b.border(1.0, k.c.border_strong);
    }
    b = b.child(text(label.to_string()).font(k.ty.ui_medium).color(ink));
    k.interactive(b).on_press(f)
}

pub fn home(s: &State, k: &Kit) -> El {
    let now = Local::now();
    let greeting = match now.hour() {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    };

    let dated: Vec<(&dex_client::protocol::SessionInfo, Option<NaiveDate>)> = s
        .sessions
        .iter()
        .filter(|x| s.is_visible(x))
        .map(|x| (x, local_date(&x.updated_at)))
        .collect();
    let mut projects: Vec<&str> = s
        .sessions
        .iter()
        .filter(|x| s.is_visible(x))
        .map(|x| x.cwd.as_str())
        .collect();
    projects.sort_unstable();
    projects.dedup();
    let running = usize::from(s.busy());

    let toggle = IconButton::new(&icons::PANEL)
        .variant(IconButtonVariant::Ghost)
        .label("Toggle sidebar")
        .on_press(|s: &mut State| s.sidebar = !s.sidebar)
        .build(k)
        .key("home-toggle");

    let subtitle = match dated.len() {
        0 => "Nothing moved in the last two days.".to_string(),
        1 => "One thread moved in the last two days. Pick up where you left off.".to_string(),
        n => format!("{n} threads moved in the last two days. Pick up where you left off."),
    };

    // Recent: threads touched in the last two days, newest first.
    let mut recent = col().w_full().child(rule(k));
    if dated.is_empty() {
        recent = recent.child(
            text("Start a thread to see it here.")
                .font(k.ty.body)
                .color(k.c.text_muted)
                .py(20.0),
        );
    }
    for (i, (sess, _)) in dated.iter().take(6).enumerate() {
        let id = sess.session_id.clone();
        let cwd = sess.cwd.clone();
        let when = ago(&sess.updated_at);
        let line = row()
            .key(format!("recent{i}"))
            .w_full()
            .min_w(0.0)
            .h(56.0)
            .px(space::LG)
            .gap(space::XL)
            .items_center()
            .child(
                el().size(6.0, 6.0)
                    .radius(3.0)
                    .fill(theme::project_color(&project_name(&sess.cwd))),
            )
            .child(
                text(session_title(sess))
                    .font(k.ty.body_medium)
                    .color(k.c.text_base)
                    .truncate()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0),
            )
            .child(
                text(project_name(&sess.cwd).to_uppercase())
                    .font(typo::caps(k))
                    .color(k.c.text_faint),
            )
            .child(text(when).font(typo::time(k)).color(k.c.text_faint));
        recent = recent.child(k.interactive(line).on_press(move |s: &mut State| {
            s.project = Some(cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        }));
        recent = recent.child(rule(k));
    }

    let week = col()
        .w(200.0)
        .child(caps(k, "THIS WEEK").pb(space::XL))
        .child(
            col()
                .w_full()
                .child(rule(k))
                .child(stat(
                    k,
                    dated.len(),
                    "threads active in two days",
                    k.c.text_base,
                ))
                .child(stat(k, projects.len(), "projects", k.c.text_base))
                .child(stat(k, running, "running now", theme::waiting())),
        );

    let continue_cwd = s
        .sessions
        .iter()
        .find(|x| s.is_visible(x))
        .map(|x| (x.session_id.clone(), x.cwd.clone()));
    let mut actions = row().w_full().gap(space::LG).items_center().child(action(
        k,
        "New thread",
        true,
        "start-new",
        |s| {
            s.page = Page::Thread;
            s.send_cmd(Cmd::NewSession);
        },
    ));
    if let Some((id, cwd)) = continue_cwd {
        actions = actions.child(action(k, "Continue last", false, "start-last", move |s| {
            s.project = Some(cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        }));
    }
    actions = actions.child(text("Ctrl N").font(typo::time(k)).color(k.c.text_faint));

    let content = col()
        .w_full()
        .max_w(820.0)
        .padding(56.0)
        .gap(0.0)
        .child(caps(
            k,
            &now.format("%A, %B %-d").to_string().to_uppercase(),
        ))
        .child(
            text(greeting.to_string())
                .font(typo::greeting(k))
                .color(k.c.text_base)
                .pt(14.0),
        )
        .child(
            text(subtitle)
                .font(k.ty.body)
                .color(k.c.text_muted)
                .pt(space::MD),
        )
        .child(
            row()
                .w_full()
                .gap(64.0)
                .items_start()
                .pt(56.0)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .child(caps(k, "RECENT").pb(space::XL))
                        .child(recent),
                )
                .child(week),
        )
        .child(el().grow(1.0))
        .child(actions.pt(40.0));

    col()
        .w_full()
        .h_full()
        .child(
            row()
                .w_full()
                .h(48.0)
                .px(space::XL)
                .items_center()
                .child(toggle),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(row().w_full().justify_center().child(content.grow(1.0))),
        )
}
