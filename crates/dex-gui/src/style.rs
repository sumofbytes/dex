//! Shared styling: named type roles and the small pieces several views repeat. Views pick a
//! role here instead of `k.ty.*` or a literal size, so the scale in DESIGN.md lives in one place.
//! (Rune's own `small` is 11px, not the design's 13px: never use it directly.)

use rune::{Color, El, el, text};
use rune_kit::Kit;

/// Type roles, as `TextStyle`s. Sizes and line heights are from DESIGN.md.
pub mod typo {
    use rune::TextStyle;
    use rune_kit::Kit;

    /// Assistant reply: 16 / 24.
    pub fn reply(k: &Kit) -> TextStyle {
        TextStyle {
            size: 16.0,
            line_height: 24.0,
            ..k.ty.body
        }
    }
    /// Reply headings (level 1..=3): 20 / 17 / 16, semibold, 26 line.
    pub fn heading(k: &Kit, size: f32) -> TextStyle {
        TextStyle {
            size,
            weight: 600.0,
            line_height: 26.0,
            ..reply(k)
        }
    }
    /// A model thought's text: 15 / 22.
    pub fn thought(k: &Kit) -> TextStyle {
        TextStyle {
            size: 15.0,
            ..k.ty.ui.lh(22.0)
        }
    }
    /// Quiet transcript rows (tool group, thought, notices, queued labels): 13 / 20.
    pub fn meta(k: &Kit) -> TextStyle {
        k.ty.ui
    }
    /// Helper and secondary text (hints, descriptions, status): 12 / 18.
    pub fn caption(k: &Kit) -> TextStyle {
        TextStyle {
            size: 12.0,
            ..k.ty.ui.lh(18.0)
        }
    }
    /// Uppercase section and project labels: 11, weight 500, tracked.
    pub fn caps(k: &Kit) -> TextStyle {
        TextStyle {
            size: 11.0,
            tracking: 0.9,
            ..k.ty.ui_medium
        }
    }
    /// Times and shortcut hints: mono 11.
    pub fn time(k: &Kit) -> TextStyle {
        TextStyle {
            size: 11.0,
            ..k.ty.mono
        }
        .lh(14.0)
    }
    /// The footer line under the composer: mono 12.
    pub fn footer(k: &Kit) -> TextStyle {
        k.ty.mono
    }
    /// Home greeting.
    pub fn greeting(k: &Kit) -> TextStyle {
        TextStyle {
            size: 56.0,
            tracking: -1.5,
            ..k.ty.body_medium
        }
        .lh(68.0)
    }
    /// Home stat numbers.
    pub fn stat(k: &Kit) -> TextStyle {
        TextStyle {
            size: 40.0,
            ..k.ty.body_medium
        }
    }
}

/// A small uppercase label in the faint ink.
pub fn caps(k: &Kit, label: &str) -> El {
    text(label.to_string())
        .font(typo::caps(k))
        .color(k.c.text_faint)
}

/// A 1px hairline, drawn as an element: `El` has no per-side borders.
pub fn rule(k: &Kit) -> El {
    el().w_full().h(1.0).fill(k.c.border_muted)
}

/// A filled circle of `size` points (project colours, status).
pub fn dot(color: Color, size: f32) -> El {
    el().size(size, size).radius(size / 2.0).fill(color)
}
