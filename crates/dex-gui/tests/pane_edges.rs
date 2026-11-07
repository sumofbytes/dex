//! Probe that the pane dividers stretch: the edge markup mirrors src/view.rs (the pinned 8pt
//! strip over the 1pt rule, `on_drag` writing the width through `State`-like helpers), so a
//! simulated drag must move the pane's edge and a double-press must reset it. Run with:
//! cargo test --test pane_edges -- --nocapture

use rune::runtime::ElementInfo;
use rune::{Env, Runtime, el, row};

struct S {
    sidebar_w: f32,
    viewer_w: f32,
}

impl S {
    fn drag_sidebar(&mut self, start: f32, dx: f32) {
        self.sidebar_w = (start + dx).clamp(220.0, 440.0).round();
    }

    fn drag_viewer(&mut self, start: f32, dx: f32) {
        self.viewer_w = (start - dx).clamp(340.0, 640.0).round();
    }
}

/// The window layout of src/view.rs: sidebar, rule, main pane, viewer — with both drag strips
/// mounted as the last children, exactly as in the app.
fn view(s: &S, _: &mut rune::Cx<S>) -> rune::El {
    let side_edge = el()
        .key("sidebar-edge")
        .pin(Some(s.sidebar_w - 3.5), Some(0.0), None, Some(0.0))
        .w(8.0)
        .cursor(rune::Cursor::ResizeColumn)
        .on_drag(|s: &mut S, d: &mut rune::Drag, tx: &mut rune::Tx| {
            tx.instant();
            match d.phase {
                rune::Phase::Down => d.data = s.sidebar_w,
                rune::Phase::Move => s.drag_sidebar(d.data, d.dx),
                rune::Phase::Up if d.cancelled() => s.sidebar_w = d.data,
                _ => {}
            }
        })
        .on_double_press_tx(|s: &mut S, tx: &mut rune::Tx| {
            tx.instant();
            s.sidebar_w = 280.0;
        });
    let viewer_edge = el()
        .key("viewer-edge")
        .pin(None, Some(0.0), Some(s.viewer_w - 3.5), Some(0.0))
        .w(8.0)
        .cursor(rune::Cursor::ResizeColumn)
        .on_drag(|s: &mut S, d: &mut rune::Drag, tx: &mut rune::Tx| {
            tx.instant();
            match d.phase {
                rune::Phase::Down => d.data = s.viewer_w,
                rune::Phase::Move => s.drag_viewer(d.data, d.dx),
                rune::Phase::Up if d.cancelled() => s.viewer_w = d.data,
                _ => {}
            }
        });
    row()
        .key("root")
        .w_full()
        .h_full()
        .child(el().key("sidebar").w(s.sidebar_w).h_full())
        .child(el().key("rule").w(1.0).h_full())
        .child(el().key("main").grow(1.0).min_w(0.0))
        .child(el().key("rule2").w(1.0).h_full())
        .child(el().key("viewer").w(s.viewer_w).h_full())
        .child(viewer_edge)
        .child(side_edge)
}

fn find(rt: &Runtime<S>, key: &str) -> ElementInfo {
    rt.elements()
        .into_iter()
        .find(|e| e.key.as_deref() == Some(key))
        .unwrap()
}

/// The strip's centre, where a pointer presses and drags from. `rect` is `[x, y, w, h]`.
fn centre(info: &ElementInfo) -> (f32, f32) {
    (
        info.rect[0] + info.rect[2] / 2.0,
        info.rect[1] + info.rect[3] / 2.0,
    )
}

#[test]
fn dragging_the_edges_stretches_both_panes() {
    let mut rt = Runtime::new(
        S {
            sidebar_w: 280.0,
            viewer_w: 460.0,
        },
        view,
        (1100.0, 760.0),
        Env::new(true, 2.0),
    );
    for _ in 0..4 {
        rt.frame(1.0 / 60.0);
    }

    // Left edge: the strip straddles the rule next to the sidebar's edge, and a drag right
    // widens the pane.
    let (x, y) = centre(&find(&rt, "sidebar-edge"));
    assert!(
        (275.0..=285.0).contains(&x),
        "sidebar strip must straddle the rule at x≈281, got {x}"
    );
    assert_eq!(find(&rt, "sidebar").rect[2], 280.0);
    rt.pointer_down(x, y);
    rt.pointer_move(x + 80.0, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x + 80.0, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(find(&rt, "sidebar").rect[2], 360.0);
    let (x, _) = centre(&find(&rt, "sidebar-edge"));
    assert!(
        (355.0..=365.0).contains(&x),
        "strip follows the rule to x≈360.5, got {x}"
    );

    // Right edge: the strip straddles the rule at the viewer pane's left, and a drag left
    // widens the viewer.
    let (x, y) = centre(&find(&rt, "viewer-edge"));
    assert!(
        (635.0..=645.0).contains(&x),
        "viewer strip must straddle the rule at x≈639.5, got {x}"
    );
    rt.pointer_down(x, y);
    rt.pointer_move(x - 100.0, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x - 100.0, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(find(&rt, "viewer").rect[0], 540.0);
    let (x, _) = centre(&find(&rt, "viewer-edge"));
    assert!(
        (535.0..=545.0).contains(&x),
        "viewer strip follows to x≈539.5, got {x}"
    );

    // The two stretched panes leave room for the main pane between them.
    let main = find(&rt, "main").rect;
    assert_eq!(
        [main[0], main[0] + main[2]],
        [361.0, 539.0],
        "main pane sits between the stretched edges"
    );
}

/// A double-press on a stretched strip jumps back to the default width.
#[test]
fn double_press_resets_the_width() {
    let mut rt = Runtime::new(
        S {
            sidebar_w: 280.0,
            viewer_w: 460.0,
        },
        view,
        (1100.0, 760.0),
        Env::new(true, 2.0),
    );
    for _ in 0..4 {
        rt.frame(1.0 / 60.0);
    }

    // Stretch it far, then double-press the strip: the second press inside the click interval
    // resets to 280. (Within one press the drag already runs; only the press count matters.)
    let (x, y) = centre(&find(&rt, "sidebar-edge"));
    rt.pointer_down(x, y);
    rt.pointer_move(x + 160.0, y);
    rt.frame(1.0 / 60.0);
    rt.pointer_up(x + 160.0, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(find(&rt, "sidebar").rect[2], 440.0);

    let (x, y) = centre(&find(&rt, "sidebar-edge"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(0.1);
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(
        find(&rt, "sidebar").rect[2],
        280.0,
        "double-click reset the width"
    );
}

/// Escape mid-drag cancels the gesture and the pane keeps its old width (the app's strips
/// re-run `Up if cancelled` on the same handlers).
#[test]
fn escape_reverts_a_drag() {
    let mut rt = Runtime::new(
        S {
            sidebar_w: 280.0,
            viewer_w: 460.0,
        },
        view,
        (1100.0, 760.0),
        Env::new(true, 2.0),
    );
    for _ in 0..4 {
        rt.frame(1.0 / 60.0);
    }

    // Cancel mid-drag: the strip re-runs `Up if cancelled` on the next handler and restores.
    let (x, y) = centre(&find(&rt, "sidebar-edge"));
    rt.pointer_down(x, y);
    rt.pointer_move(x + 120.0, y);
    rt.frame(1.0 / 60.0);
    rt.key(rune::KeyEvent {
        key: rune::Key::Escape,
        mods: rune::Mods::default(),
    });
    rt.pointer_up(x + 120.0, y);
    rt.frame(1.0 / 60.0);
    assert_eq!(
        find(&rt, "sidebar").rect[2],
        280.0,
        "a cancelled drag puts the width back"
    );
    assert_eq!(
        find(&rt, "viewer").rect[0],
        640.0,
        "the viewer pane was untouched"
    );
}
