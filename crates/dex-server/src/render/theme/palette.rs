//! Terminal palette primitive: the real background/foreground queried once
//! via OSC 11 and memoized. Lives in core (not ui) so both the TUI theme
//! (`ui::theme`) and the headless highlighter (`core::highlight`) derive
//! colors from a single query instead of each probing the terminal.

use std::sync::OnceLock;

use terminal_colorsaurus::{color_palette, QueryOptions, ThemeMode};

/// The terminal's real colors (`None` when the query fails / not a TTY).
#[derive(Clone, Copy)]
pub struct TermPalette {
    pub dark: bool,
    pub foreground: (u8, u8, u8),
    pub background: (u8, u8, u8),
}

pub fn term_palette() -> Option<&'static TermPalette> {
    static PALETTE: OnceLock<Option<TermPalette>> = OnceLock::new();
    PALETTE
        .get_or_init(|| {
            // Bounded for startup: the default 1s timeout parks TUI
            // first paint on a dead terminal. Fast terminals answer in ms
            // and unsupported ones are detected via DA1 before the timeout,
            // so a slow-but-capable outlier just falls back to `Unknown`
            // (Reset colors) instead of stalling launch. (`QueryOptions` is
            // non-exhaustive, so the timeout is set on the default value.)
            let mut query = QueryOptions::default();
            query.timeout = std::time::Duration::from_millis(100);
            let p = color_palette(query).ok()?;
            Some(TermPalette {
                dark: p.theme_mode() == ThemeMode::Dark,
                background: p.background.scale_to_8bit(),
                foreground: p.foreground.scale_to_8bit(),
            })
        })
        .as_ref()
}

/// Blend `base` toward `toward` by `amount` (0.0 = base, 1.0 = toward).
pub fn blend(base: (u8, u8, u8), toward: (u8, u8, u8), amount: f32) -> (u8, u8, u8) {
    let mix = |b: u8, t: u8| (f32::from(b) + (f32::from(t) - f32::from(b)) * amount).round() as u8;
    (
        mix(base.0, toward.0),
        mix(base.1, toward.1),
        mix(base.2, toward.2),
    )
}

/// The terminal's own default foreground, if known.
pub fn fg_rgb() -> Option<(u8, u8, u8)> {
    term_palette().map(|p| p.foreground)
}

/// Readable dim: foreground blended toward the background, preserving the
/// theme's hue on tinted light/dark terminals (fixed ANSI grays clash).
pub fn muted_rgb() -> Option<(u8, u8, u8)> {
    term_palette().map(|p| {
        let amount = if p.dark { 0.38 } else { 0.42 };
        blend(p.foreground, p.background, amount)
    })
}

/// Perceived luminance, Rec. 709 weights (0..=255 scale).
fn luminance(c: (u8, u8, u8)) -> f32 {
    let (r, g, b) = (f32::from(c.0), f32::from(c.1), f32::from(c.2));
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Hairline edge color with an optional accent tint: the accent hue re-lit
/// to the muted band's luminance, so it reads at the same dimness as the
/// plain [`muted_rgb`] blend but carries the accent's hue instead of the
/// foreground's. A chromatic line at the same luminance as an achromatic
/// one still reads as more present, which keeps the rule an edge — not
/// dialogue, not chrome shouting for attention. `None` (plain voice,
/// unknown theme) falls back to the plain muted blend.
pub fn tinted_rgb(tint: Option<(u8, u8, u8)>) -> Option<(u8, u8, u8)> {
    term_palette().map(|p| {
        let amount = if p.dark { 0.38 } else { 0.42 };
        let base = blend(p.foreground, p.background, amount);
        let Some(t) = tint else { return base };
        // Re-light the accent hue to the muted band's luminance. The factor
        // is bounded so a near-black tint can't explode the channels.
        let src = luminance(t).max(1.0);
        let factor = (luminance(base) / src).clamp(0.25, 6.0);
        let scale = |ch: u8| ((f32::from(ch) * factor).round() as i32).clamp(0, 255) as u8;
        (scale(t.0), scale(t.1), scale(t.2))
    })
}

/// Which side of the light/dark split the terminal background sits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Background {
    Dark,
    Light,
    /// Unknown (query failed or not a TTY); assume dark, the common case.
    Unknown,
}
