//! Theme picker: a searchable list of every colour scheme, opened from the header or Ctrl+,.

use rune::{El, ScrollSpec, col, text, text_input};
use rune_kit::shell::SidebarRow;
use rune_kit::{Dialog, Kit};

use crate::state::State;

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
        list = list.child(
            SidebarRow::new(*name)
                .selected(*name == current)
                .on_select(move |s: &mut State, _| {
                    s.set_theme(&pick);
                    s.theme_open = false;
                })
                .build(k)
                .key(format!("theme:{name}")),
        );
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
