//! The composer's option pickers, as drawn in Paper (`DESIGN.md`, "Composer options"): a
//! mode list with a one-line description per mode, and a searchable model list grouped by
//! provider. Both open upward from their chip.

use rune::{El, ScrollSpec, TextStyle, col, el, row, text, text_input};
use rune_kit::{Kit, Placement, Popover};

use crate::icons;
use crate::state::State;

/// The permission modes in display order: (value sent to the daemon, name, description).
pub const MODES: [(&str, &str, &str); 3] = [
    ("read-only", "Read-only", "Reads only, changes nothing"),
    ("ask", "Ask", "Asks before changes"),
    ("trusted", "Trusted", "Runs tools without asking"),
];

/// Models shown before the list asks the user to filter.
const MAX_MODELS: usize = 12;

fn caps(k: &Kit, label: &str) -> El {
    text(label.to_string())
        .font(TextStyle {
            size: 11.0,
            tracking: 0.9,
            ..k.ty.ui_medium
        })
        .color(k.c.text_faint)
}

/// A chip in the composer: an optional icon, a label, faint detail, and a chevron.
fn chip(k: &Kit, icon: Option<&'static rune::Icon>, label: &str, detail: &str, key: &str) -> El {
    let mut r = row()
        .key(key.to_string())
        .h(28.0)
        .px(8.0)
        .gap(6.0)
        .items_center()
        .radius(k.radius.md);
    if let Some(ic) = icon {
        r = r.child(rune::icon(ic, 12.0).color(k.c.text_muted));
    }
    r = r.child(
        text(label.to_string())
            .font(k.ty.ui_medium)
            .color(k.c.text_muted),
    );
    if !detail.is_empty() {
        r = r.child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    r.child(rune::icon(&icons::DOWN, 12.0).color(k.c.text_faint))
}

/// One row of a menu: a name with an optional description, and a check on the current one.
fn menu_row(k: &Kit, key: String, name: &str, desc: &str, detail: &str, on: bool) -> El {
    let mut label = col().grow(1.0).min_w(0.0).gap(2.0);
    label = label.child(
        text(name.to_string())
            .font(TextStyle {
                size: 13.0,
                ..k.ty.ui_medium
            })
            .color(k.c.text_base)
            .truncate(),
    );
    if !desc.is_empty() {
        label = label.child(
            text(desc.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    let mut r = row()
        .key(key)
        .w_full()
        .min_h(34.0)
        .px(10.0)
        .py(6.0)
        .gap(10.0)
        .items_center()
        .radius(k.radius.md)
        .fill(if on {
            k.c.surface_raised
        } else {
            rune::Color::TRANSPARENT
        })
        .child(label);
    if !detail.is_empty() {
        r = r.child(
            text(detail.to_string())
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    r.child(if on {
        rune::icon(&icons::CHECK, 14.0).color(k.c.text_base)
    } else {
        el().size(14.0, 14.0)
    })
}

/// The mode chip and its menu.
pub fn mode(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let current = s.effective_permission();
    let name = MODES
        .iter()
        .find(|(v, _, _)| *v == current)
        .map(|(_, n, _)| *n)
        .unwrap_or("Trusted");
    let open = s.open_select == Some("mode");
    let trigger = k
        .interactive(chip(k, Some(&icons::SLIDERS), name, "", "mode-chip"))
        .on_press(|s: &mut State| {
            s.open_select = if s.open_select == Some("mode") {
                None
            } else {
                Some("mode")
            };
        });
    let mut list = col().w_full().gap(2.0);
    for (value, label, desc) in MODES {
        let pick = value.to_string();
        let item = k
            .interactive(menu_row(
                k,
                format!("mode:{value}"),
                label,
                desc,
                "",
                current == value,
            ))
            .on_press(move |s: &mut State| {
                s.permission = Some(pick.clone());
                s.open_select = None;
            });
        list = list.child(item);
    }
    let body = col()
        .w(240.0)
        .gap(4.0)
        .child(caps(k, "MODE").px(10.0).pt(6.0).pb(4.0))
        .child(list);
    Popover::new("mode-menu")
        .trigger(trigger)
        .open(open)
        .placement(Placement::Top)
        .padding(6.0)
        .on_close(|s: &mut State| s.open_select = None)
        .child(body)
        .build(k, layer)
}

/// The model chip and its searchable menu.
pub fn model(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let current = s.effective_model();
    let provider = s.info.provider.clone();
    let open = s.open_select == Some("model");
    let trigger = k
        .interactive(chip(k, None, &current, &provider, "model-chip"))
        .on_press(|s: &mut State| {
            s.open_select = if s.open_select == Some("model") {
                None
            } else {
                Some("model")
            };
        });

    let query = s.model_query.text.trim().to_lowercase();
    let models: Vec<String> = if s.info.models.is_empty() {
        vec![current.clone()]
    } else {
        s.info.models.clone()
    };
    let matched: Vec<&String> = models
        .iter()
        .filter(|m| query.is_empty() || m.to_lowercase().contains(&query))
        .collect();

    let search = text_input(&s.model_query, k.ty.ui.lh(20.0))
        .key("model-search")
        .placeholder("Search models", k.c.text_faint)
        .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
        .color(k.c.text_base)
        .single_line()
        .on_edit(|s: &mut State| &mut s.model_query)
        .w_full()
        .px(10.0)
        .py(6.0);

    let mut list = col().w_full().gap(2.0);
    for m in matched.iter().take(MAX_MODELS) {
        let pick = (*m).clone();
        let on = **m == current;
        let item = k
            .interactive(menu_row(k, format!("model:{m}"), m, "", &provider, on))
            .on_press(move |s: &mut State| {
                s.model = Some(pick.clone());
                s.open_select = None;
            });
        list = list.child(item);
    }
    if matched.is_empty() {
        list = list.child(
            text("No model matches")
                .font(k.ty.small)
                .color(k.c.text_faint)
                .px(10.0)
                .py(6.0),
        );
    } else if matched.len() > MAX_MODELS {
        list = list.child(
            text(format!(
                "{} more · keep typing to filter",
                matched.len() - MAX_MODELS
            ))
            .font(k.ty.small)
            .color(k.c.text_faint)
            .px(10.0)
            .py(6.0),
        );
    }

    let body = col()
        .w(280.0)
        .gap(4.0)
        .child(search)
        .child(caps(k, &provider.to_uppercase()).px(10.0).pt(6.0).pb(2.0))
        .child(
            col()
                .w_full()
                .max_h(260.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(list),
        );
    Popover::new("model-menu")
        .trigger(trigger)
        .open(open)
        .placement(Placement::Top)
        .padding(6.0)
        .on_close(|s: &mut State| s.open_select = None)
        .child(body)
        .build(k, layer)
}

#[allow(dead_code)]
fn _unused(_: &El) {}
