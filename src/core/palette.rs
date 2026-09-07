//! Terminal palette primitive: the real background/foreground queried once
//! via OSC 11 and memoized. Lives in core (not ui) so both the TUI theme
//! (`ui::theme`) and the headless highlighter (`core::highlight`) derive
//! colors from a single query instead of each probing the terminal.

use std::sync::OnceLock;

use terminal_colorsaurus::{color_palette, QueryOptions, ThemeMode};

/// The terminal's real colors (`None` when the query fails / not a TTY).
pub(crate) struct TermPalette {
    pub(crate) dark: bool,
    pub(crate) foreground: (u8, u8, u8),
    pub(crate) background: (u8, u8, u8),
}

pub(crate) fn term_palette() -> Option<&'static TermPalette> {
    static PALETTE: OnceLock<Option<TermPalette>> = OnceLock::new();
    PALETTE
        .get_or_init(|| {
            let p = color_palette(QueryOptions::default()).ok()?;
            Some(TermPalette {
                dark: p.theme_mode() == ThemeMode::Dark,
                background: p.background.scale_to_8bit(),
                foreground: p.foreground.scale_to_8bit(),
            })
        })
        .as_ref()
}

/// Blend `base` toward `toward` by `amount` (0.0 = base, 1.0 = toward).
pub(crate) fn blend(base: (u8, u8, u8), toward: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let mix = |b: u8, t: u8| (f32::from(b) + (f32::from(t) - f32::from(b)) * amount).round() as u8;
    (
        mix(base.0, toward.0),
        mix(base.1, toward.1),
        mix(base.2, toward.2),
    )
}

/// The terminal's own default foreground, if known.
pub(crate) fn fg_rgb() -> Option<(u8, u8, u8)> {
    term_palette().map(|p| p.foreground)
}

/// Readable dim: foreground blended toward the background, preserving the
/// theme's hue on tinted light/dark terminals (fixed ANSI grays clash).
pub(crate) fn muted_rgb() -> Option<(u8, u8, u8)> {
    term_palette().map(|p| {
        let amount = if p.dark { 0.38 } else { 0.42 };
        blend(p.foreground, p.background, amount)
    })
}
