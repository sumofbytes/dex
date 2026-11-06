//! Theme picker: a searchable list of every colour scheme, opened from the header or Ctrl+,.

use rune::{El, ScrollSpec, col, el, row, text, text_input};
use rune_kit::{Dialog, Kit};

use crate::state::State;

/// Three squares: the scheme's background, text and accent.
fn swatch(bg: rune::Color, fg: rune::Color, accent: rune::Color) -> El {
    row()
        .gap(0.0)
        .radius(4.0)
        .child(el().size(10.0, 14.0).fill(bg))
        .child(el().size(10.0, 14.0).fill(fg))
        .child(el().size(10.0, 14.0).fill(accent))
}

/// Rows drawn at once; refine the search for the rest.
const MAX_ROWS: usize = 80;

pub fn picker(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if !s.theme_open {
        return;
    }
    let q = s.theme_query.text.to_lowercase();
    let current = crate::theme::name();
    let matches: Vec<&str> = crate::theme::names()
        .into_iter()
        .filter(|n| q.is_empty() || n.to_lowercase().contains(&q))
        .collect();
    let mut list = col().w_full().gap(2.0);
    for name in matches.iter().take(MAX_ROWS) {
        let pick = name.to_string();
        let on = *name == current;
        let (bg, fg, accent) = crate::theme::swatch(name);
        let line = row()
            .key(format!("theme:{name}"))
            .w_full()
            .h(36.0)
            .px(10.0)
            .gap(12.0)
            .items_center()
            .radius(k.radius.md)
            .fill(if on {
                k.c.surface_raised
            } else {
                rune::Color::TRANSPARENT
            })
            .child(swatch(bg, fg, accent))
            .child(
                text(name.to_string())
                    .font(k.ty.ui)
                    .color(if on { k.c.text_base } else { k.c.text_weak })
                    .truncate()
                    .grow(1.0)
                    .min_w(0.0),
            )
            .child(if on {
                rune::icon(&crate::icons::CHECK, 14.0).color(k.c.text_base)
            } else {
                el().size(14.0, 14.0)
            });
        list = list.child(k.interactive(line).on_press(move |s: &mut State| {
            s.set_theme(&pick);
            s.theme_open = false;
        }));
    }
    if matches.is_empty() {
        list = list.child(
            text("No scheme matches")
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    } else if matches.len() > MAX_ROWS {
        list = list.child(
            text(format!(
                "{} more — refine the search",
                matches.len() - MAX_ROWS
            ))
            .font(k.ty.ui)
            .color(k.c.text_muted),
        );
    }
    let body = col()
        .w_full()
        .gap(8.0)
        .child(
            text_input(&s.theme_query, k.ty.ui.lh(20.0))
                .key("theme-search")
                .placeholder("Search themes…", k.c.text_faint)
                .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
                .color(k.c.text_base)
                .single_line()
                .on_edit(|s: &mut State| &mut s.theme_query)
                .w_full()
                .px(10.0)
                .py(6.0),
        )
        .child(
            col()
                .w_full()
                .h(300.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        );
    Dialog::new("theme", "Theme")
        .open(true)
        .width(380.0)
        .fit()
        .child(body)
        .on_close(|s: &mut State| s.theme_open = false)
        .build(k, layer);
}
