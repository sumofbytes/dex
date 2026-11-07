//! Probes how the composer's text field grows to fit its wrapped lines, headlessly.
//! Run with: cargo test --test composer_grow -- --nocapture

use rune::runtime::ElementInfo;
use rune::{Env, Key, KeyEvent, Mods, Runtime, TextEdit, TextStyle, col, text_input};

struct S {
    draft: TextEdit,
    grow: bool,
}

/// Mirrors the composer field in src/view.rs (min_h/max_h on the field, no auto_grow) and,
/// with `s.grow`, the rune-blessed `auto_grow` variant with the same field padding.
fn view(s: &S, _: &mut rune::Cx<S>) -> rune::El {
    let field = text_input(&s.draft, TextStyle::new(14.0, 400.0).lh(22.0))
        .key("composer-input")
        .on_edit(|s: &mut S| &mut s.draft)
        .w_full();
    let field = field.px(8.0).py(8.0);
    let field = if s.grow {
        field
    } else {
        field.min_h(44.0).max_h(220.0)
    };
    use rune::row;
    let input = if s.grow {
        rune::auto_grow(field, 2, 10)
    } else {
        field
    };
    let card = col()
        .w_full()
        .max_w(760.0)
        .padding(4.0)
        .gap(2.0)
        .child(input)
        .child(row().pb(8.0).child(rune::spacer()));
    let stack = col().w_full().max_w(760.0).gap(6.0).child(card);
    row()
        .key("composer")
        .w_full()
        .justify_center()
        .px(24.0)
        .pb(8.0)
        .child(stack.grow(1.0))
}

fn find(rt: &Runtime<S>, key: &str) -> ElementInfo {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap()
}

fn dump(tag: &str, rt: &Runtime<S>, t: &str) {
    let input = find(rt, "composer-input");
    let caret = input.caret.expect("caret");
    println!(
        "{tag} {t} h={:.1} target={:.1} caret_y={:.1} len={} text={:?}",
        input.rect[3],
        input.target[3],
        caret[1],
        input.text.as_deref().map_or(0, str::len),
        input.text.as_deref().map_or("", |s| &s[..s.len().min(30)])
    );
}

fn grow_probe(grow: bool) {
    let mut draft = TextEdit::new("");
    draft.focused = true;
    let mut rt = Runtime::new(
        S { draft, grow },
        view,
        (1400.0, 800.0),
        Env::new(true, 2.0),
    );
    for _ in 0..10 {
        rt.frame(1.0 / 60.0);
    }
    println!(
        "--- grow = {grow}, field={}",
        if grow {
            "auto_grow (springs)"
        } else {
            "min_h/max_h (instant)"
        }
    );
    for n in 1..=12 {
        rt.key(KeyEvent {
            key: Key::Text("abcdefghij abcdefghi ".into()),
            mods: Mods::default(),
        });
        rt.frame(1.0 / 60.0);
        dump("commit ", &rt, &format!("n={n}"));
        rt.frame(1.0 / 60.0);
        dump("1 later", &rt, &format!("n={n}"));
    }
}

#[test]
fn probe() {
    grow_probe(false);
    grow_probe(true);
}
