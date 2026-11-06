//! A warm dark theme in Po's palette (a brown-grey surface with a salmon accent), derived from
//! three seed colours through `rune-kit`, plus per-project name colours.

use std::sync::OnceLock;

use rune::Color;
use rune::color::hex;
use rune_kit::{Kit, PaletteSeed};

fn c(s: &str) -> Color {
    hex(s).unwrap_or(Color::WHITE)
}

pub fn kit() -> &'static Kit {
    static KIT: OnceLock<Kit> = OnceLock::new();
    KIT.get_or_init(|| {
        Kit::from_palette(PaletteSeed {
            bg: c("#342d2d"),
            fg: c("#f4eeee"),
            accent: c("#f38d70"),
        })
    })
}

pub fn accent() -> Color {
    c("#f38d70")
}

/// The accent button's ink on `accent()`.
pub fn on_accent() -> Color {
    c("#2a1f1c")
}

/// A stable colour for a project name, from a small warm palette.
pub fn project_color(name: &str) -> Color {
    const PALETTE: [&str; 6] = [
        "#f38d70", "#8fd694", "#a89cf0", "#f2c87a", "#74d4c4", "#f08fb0",
    ];
    let h = name
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(b as u32));
    c(PALETTE[h as usize % PALETTE.len()])
}

/// Status colours for thread marks.
pub fn waiting() -> Color {
    c("#f2c87a")
}
pub fn done() -> Color {
    c("#8fd694")
}
