//! The landing page, as drawn in Paper (`DESIGN.md`, dex / Home v3): a greeting, a stats row,
//! the issue field (the composer of the next thread), and the threads touched in the last two
//! days behind a "Show more".

use chrono::{Local, NaiveDate, Timelike};
use rune::{Cursor, El, ScrollSpec, col, el, row, spacer, text, text_input};
use rune_kit::Kit;

use crate::sidebar::ago;
use crate::state::{Cmd, Page, State, project_name, session_title};
use crate::style::{caps, rule, space, typo};
use crate::theme;

fn local_date(updated_at: &str) -> Option<NaiveDate> {
    chrono::DateTime::parse_from_rfc3339(updated_at)
        .ok()
        .map(|t| t.with_timezone(&Local).date_naive())
}

/// One stat in the strip under the greeting: a colored dot, the number, the label.
fn stat(k: &Kit, n: usize, label: &str, color: rune::Color) -> El {
    row()
        .gap(space::XS)
        .items_center()
        .child(el().size(6.0, 6.0).radius(3.0).fill(color))
        .child(text(n.to_string()).font(k.ty.ui).color(k.c.text_base))
        .child(text(label.to_string()).font(k.ty.ui).color(k.c.text_muted))
}

/// The issue field's card: the input, the mode and model chips, and the Enter hint.
fn issue_card(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let input = text_input(&s.issue, k.ty.body.lh(22.0))
        .key("issue-input")
        .placeholder("Write an issue…", k.c.text_faint)
        .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
        .color(k.c.text_base)
        .on_edit(|s: &mut State| &mut s.issue)
        .on_submit(|s: &mut State, _| s.submit_issue())
        .w_full()
        .px(space::MD)
        .py(space::MD)
        .min_h(44.0);
    let mode = crate::composer_menu::mode(s, k, layer);
    let model = crate::composer_menu::model(s, k, layer);
    col()
        .w_full()
        .padding(space::XS)
        .gap(space::XXS)
        .radius(k.radius.xl)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_base)
        .child(input)
        .child(
            // Chip text lands on the card edge + 12, the input's content line.
            row()
                .w_full()
                .min_w(0.0)
                .gap(space::MD)
                .items_center()
                .pr(space::MD)
                .pb(space::MD)
                .child(crate::composer_menu::project(s, k))
                .child(mode)
                .child(model)
                .child(spacer())
                .child(
                    rune_kit::IconButton::new(&crate::icons::UP)
                        .variant(rune_kit::IconButtonVariant::Submit)
                        .label("Send")
                        .on_press(|s: &mut State| s.submit_issue())
                        .build(k)
                        .key("issue-send"),
                ),
        )
}

pub fn home(s: &State, k: &Kit, inset: f32, layer: &mut Vec<El>) -> El {
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

    let subtitle = match dated.len() {
        0 => "Nothing moved in the last two days. Write a new issue.".to_string(),
        1 => "One thread moved in the last two days. Pick one up or write a new issue.".to_string(),
        n => format!("{n} threads moved in the last two days. Pick one up or write a new issue."),
    };

    // Stats strip: waiting on you, threads running, projects.
    let stats = row()
        .gap(space::XL + space::LG)
        .items_center()
        .pt(space::XL)
        .pb(space::XL)
        .child(stat(k, s.needs.len(), "waiting on you", theme::waiting()))
        .child(stat(k, s.running.len(), "threads running", theme::accent()))
        .child(stat(k, projects.len(), "projects", theme::done()));

    // Recent: threads touched in the last two days, newest first. Six show until "Show more".
    let shown = if s.show_more { dated.len() } else { 6 };
    let mut recent = col().w_full().child(rule(k));
    if dated.is_empty() {
        recent = recent.child(
            text("Write an issue above to see it here.")
                .font(k.ty.body)
                .color(k.c.text_muted)
                .py(20.0),
        );
    }
    for (i, (sess, _)) in dated.iter().take(shown).enumerate() {
        let id = sess.session_id.clone();
        let cwd = sess.cwd.clone();
        let when = ago(&sess.updated_at);
        let line = row()
            .key(format!("recent{i}"))
            .w_full()
            .min_w(0.0)
            .h(56.0)
            .px(10.0)
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
    if !s.show_more && dated.len() > shown {
        let more = text("Show more").font(k.ty.ui).color(k.c.text_muted);
        recent = recent.child(
            k.interactive(more.px(10.0).py(12.0).cursor(Cursor::Pointer))
                .on_press(|s: &mut State| s.show_more = true),
        );
    }

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
                .wrap()
                .w_full()
                .font(k.ty.body)
                .color(k.c.text_muted)
                .pt(space::MD),
        )
        .child(stats)
        .child(issue_card(s, k, layer).mt(space::XXL))
        .child(
            col()
                .w_full()
                .pt(space::XXL)
                .child(caps(k, "RECENT").pb(space::XL))
                .child(recent),
        );

    col()
        .w_full()
        .h_full()
        .child(
            row()
                .w_full()
                .h(48.0)
                .px(space::XL)
                .pl(crate::sidebar::lights_pad(inset, space::XL))
                .items_center()
                .children((!s.sidebar).then(|| crate::sidebar::toggle(k))),
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
