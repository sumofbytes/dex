//! Shared styling: named type roles and the small pieces several views repeat. Views pick a
//! role here instead of `k.ty.*` or a literal size, so the scale in DESIGN.md lives in one place.
//! (Rune's own `small` is 11px, not the design's 13px: never use it directly.)

use rune::{Color, El, el, text};
use rune_kit::Kit;

/// The height of a window's title area: the chat pane's header and the sidebar's brand band
/// (`view.rs` and `sidebar.rs`) are both this tall, so their hairlines sit at the same level.
pub const TITLEBAR: f32 = 48.0;

/// Type roles, as `TextStyle`s. Sizes and line heights are from DESIGN.md.
pub mod typo {
    use rune::TextStyle;
    use rune_kit::Kit;

    /// Assistant reply and user message: 14 / 22.
    pub fn reply(k: &Kit) -> TextStyle {
        k.ty.body
    }
    /// Reply headings (level 1..=3): 18 / 16 / 14, semibold, 24 line.
    pub fn heading(k: &Kit, size: f32) -> TextStyle {
        TextStyle {
            size,
            weight: 600.0,
            line_height: 24.0,
            ..reply(k)
        }
    }
    /// Quiet transcript rows (tool group, thought, notices, queued labels): 12 / 18.
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
    /// Uppercase section and project labels: 10, weight 600, tracked.
    pub fn caps(k: &Kit) -> TextStyle {
        TextStyle {
            size: 10.0,
            tracking: 1.0,
            weight: 600.0,
            ..k.ty.ui_medium
        }
    }
    /// Times and shortcut hints: mono 12 / 16.
    pub fn time(k: &Kit) -> TextStyle {
        k.ty.mono.lh(16.0)
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
}

/// Spacing scale, in points: gaps and paddings use these names, never raw numbers. Odd one-offs
/// (a 14 or 48 for a specific layout) stay literal on purpose.
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 10.0;
    pub const XL: f32 = 12.0;
    pub const XXL: f32 = 16.0;
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

/// A structural pane divider (`border_base`): the same ink as the sidebar and pane edges, one
/// step stronger than the in-card `rule`.
pub fn edge(k: &Kit) -> El {
    el().w_full().h(1.0).fill(k.c.border_base)
}

/// A filled circle of `size` points (project colours, status).
pub fn dot(color: Color, size: f32) -> El {
    el().size(size, size).radius(size / 2.0).fill(color)
}
