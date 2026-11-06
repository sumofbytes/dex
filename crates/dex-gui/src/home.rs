//! The landing page: a greeting, counts, the threads touched today, and a start card.

use chrono::{Datelike, Local, NaiveDate, Timelike};
use rune::{El, ScrollSpec, col, el, row, text};
use rune_kit::{IconButton, IconButtonVariant, Kit};
use rune_theme_opencode::icons;

use crate::state::{Cmd, Page, State, project_name, session_title};
use crate::theme;
use crate::view::ago;

fn local_date(updated_at: &str) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .ok()
        .map(|t| t.with_timezone(&Local).date_naive())
}

fn card(k: &Kit) -> El {
    col()
        .padding(18.0)
        .gap(10.0)
        .radius(k.radius.xl)
        .border(1.0, k.c.border_base)
}

fn stat(k: &Kit, n: usize, label: &str, color: rune::Color) -> El {
    col()
        .grow(1.0)
        .min_w(130.0)
        .padding(16.0)
        .gap(2.0)
        .radius(k.radius.xl)
        .border(1.0, k.c.border_base)
        .child(
            text(n.to_string())
                .font(k.ty.body_medium.lh(32.0))
                .color(color),
        )
        .child(text(label.to_string()).font(k.ty.ui).color(k.c.text_muted))
}

fn pill(k: &Kit, label: &str, primary: bool, key: &str, f: impl Fn(&mut State) + 'static) -> El {
    let (fill, ink) = if primary {
        (theme::accent(), theme::on_accent())
    } else {
        (rune::Color::TRANSPARENT, k.c.text_base)
    };
    let b = row()
        .key(key.to_string())
        .px(18.0)
        .py(9.0)
        .radius(999.0)
        .fill(fill)
        .child(text(label.to_string()).font(k.ty.ui_medium).color(ink));
    let b = if primary {
        b
    } else {
        b.border(1.0, k.c.border_strong)
    };
    k.interactive(b).on_press(f)
}

pub fn home(s: &State, k: &Kit) -> El {
    let now = Local::now();
    let today = now.date_naive();
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
    let today_threads: Vec<_> = dated
        .iter()
        .filter(|(_, d)| *d == Some(today))
        .map(|(x, _)| *x)
        .collect();
    let mut projects: Vec<&str> = s
        .sessions
        .iter()
        .filter(|x| s.is_visible(x))
        .map(|x| x.cwd.as_str())
        .collect();
    projects.sort_unstable();
    projects.dedup();
    let running = usize::from(s.busy);

    let toggle = IconButton::new(&icons::SIDEBAR)
        .variant(IconButtonVariant::Ghost)
        .label("Toggle sidebar")
        .on_press(|s: &mut State| s.sidebar = !s.sidebar)
        .build(k)
        .key("home-toggle");

    let stats = row()
        .w_full()
        .gap(12.0)
        .flex_wrap()
        .child(stat(k, today_threads.len(), "Today", k.c.text_base))
        .child(stat(k, dated.len(), "Last 2 days", k.c.text_muted))
        .child(stat(k, running, "In progress", theme::waiting()))
        .child(stat(k, projects.len(), "Projects", theme::done()));

    let mut today_card = card(k)
        .grow(1.0)
        .min_w(320.0)
        .child(text("Today").font(k.ty.ui_medium).color(k.c.text_base));
    if today_threads.is_empty() {
        today_card = today_card.child(
            text(
                "Nothing yet today. Pick up an older thread from the sidebar, or start a new one.",
            )
            .font(k.ty.ui)
            .color(k.c.text_muted)
            .wrap(),
        );
    }
    for (i, sess) in today_threads.iter().take(5).enumerate() {
        let id = sess.session_id.clone();
        let cwd = sess.cwd.clone();
        let line = row()
            .key(format!("today{i}"))
            .w_full()
            .min_w(0.0)
            .min_w(0.0)
            .px(8.0)
            .py(6.0)
            .gap(8.0)
            .radius(k.radius.md)
            .items_center()
            .child(
                text(session_title(sess))
                    .font(k.ty.ui)
                    .color(k.c.text_base)
                    .truncate()
                    .grow(1.0)
                    .shrink()
                    .min_w(0.0),
            )
            .child(
                text(format!(
                    "{} · {}",
                    project_name(&sess.cwd),
                    ago(&sess.updated_at)
                ))
                .font(k.ty.small)
                .color(k.c.text_faint),
            );
        today_card = today_card.child(k.interactive(line).on_press(move |s: &mut State| {
            s.project = Some(cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        }));
    }

    let continue_cwd = s
        .sessions
        .iter()
        .find(|x| s.is_visible(x))
        .map(|x| (x.session_id.clone(), x.cwd.clone()));
    let mut start = row()
        .gap(10.0)
        .child(pill(k, "New thread", true, "start-new", |s| {
            s.page = Page::Thread;
            s.send_cmd(Cmd::NewSession);
        }));
    if let Some((id, cwd)) = continue_cwd {
        start = start.child(pill(k, "Continue last", false, "start-last", move |s| {
            s.project = Some(cwd.clone());
            s.page = Page::Thread;
            s.send_cmd(Cmd::Open(id.clone()));
        }));
    }
    let start_card = card(k)
        .child(text("Start").font(k.ty.ui_medium).color(k.c.text_base))
        .child(start);

    let content = col()
        .w_full()
        .max_w(860.0)
        .padding(28.0)
        .gap(14.0)
        .child(
            text(now.format("%A, %B %-d").to_string())
                .font(k.ty.ui)
                .color(k.c.text_muted),
        )
        .child(
            text(greeting.to_string())
                .font(
                    rune::TextStyle {
                        size: 40.0,
                        ..k.ty.body_medium
                    }
                    .lh(46.0),
                )
                .color(k.c.text_base),
        )
        .child(
            text("A clear view of what matters today.")
                .font(k.ty.body)
                .color(k.c.text_muted),
        )
        .child(stats)
        .child(
            row()
                .w_full()
                .gap(12.0)
                .items_start()
                .flex_wrap()
                .child(today_card),
        )
        .child(start_card);

    col()
        .w_full()
        .h_full()
        .child(row().w_full().h(48.0).px(12.0).items_center().child(toggle))
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
