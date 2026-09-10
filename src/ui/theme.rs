//! Theme-aware surface colors for the TUI.
//!
//! Fixed palette slots bypass the terminal's theme (the 256-color gray ramp
//! is never remapped), so neutral grays clash with tinted backgrounds. Surface
//! colors here are instead derived from the terminal's real background and
//! foreground colors, queried once at startup (OSC 11): a surface is the
//! background blended a step toward the foreground, so it keeps the theme's
//! hue and always contrasts with text on it.

use ratatui::style::Color;

use crate::core::palette::{blend, fg_rgb, muted_rgb, term_palette};

/// Which side of the light/dark split the terminal background sits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Background {
    Dark,
    Light,
    /// Unknown (query failed or not a TTY); assume dark, the common case.
    Unknown,
}

/// A raised surface: the terminal's actual background lifted a step toward
/// its foreground, so the hue matches the active theme. `amount` controls how
/// far the surface sits above the background (larger = more prominent).
fn raised(amount: f32) -> Color {
    match term_palette() {
        Some(p) => {
            let (r, g, b) = blend(p.background, p.foreground, amount);
            Color::Rgb(r, g, b)
        }
        // No theme information at all: leave the background untouched so the
        // surface always blends with whatever the terminal paints.
        None => Color::Reset,
    }
}

/// BG for the composer, submitted prompts, and overlays: a "raised" surface.
pub(crate) fn surface_bg() -> Color {
    match background() {
        Background::Dark => raised(0.10),
        Background::Light => raised(0.06),
        Background::Unknown => Color::Reset,
    }
}

/// Slightly stronger surface for popups so they read as floating above the UI.
pub(crate) fn popup_bg() -> Color {
    match background() {
        Background::Dark => raised(0.18),
        Background::Light => raised(0.12),
        Background::Unknown => Color::Reset,
    }
}

/// Foreground for prominent text on the composer/surface: the terminal's own
/// default foreground, so it always contrasts with the background and with
/// the surfaces derived from it. Fixed ANSI slots (`Color::Black` /
/// `Color::White`) must not be used here: they are palette entries that
/// themes routinely remap toward the background tint (e.g. Gruvbox Light sets
/// color 0 to the cream background), which renders text unreadable. When the
/// theme is unknown the default foreground is inherited (`Reset`) instead.
pub(crate) fn surface_fg() -> Color {
    match fg_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Reset,
    }
}

/// Foreground for secondary text on a raised surface (descriptions, hints):
/// quiet, but still readable in both theme modes. Uses the same blended
/// foreground guideline as tool input/previews so it keeps the terminal's
/// hue and stays readable on tinted backgrounds instead of a fixed ANSI gray.
pub(crate) fn secondary_fg() -> Color {
    tool_muted_fg()
}

/// Foreground for de-emphasised text on the composer/surface.
/// Uses the same blended-foreground guideline as tool input/previews so
/// de-emphasised text keeps the terminal's hue and stays readable on
/// tinted light/dark backgrounds instead of a fixed low-contrast gray.
pub(crate) fn muted_fg() -> Color {
    tool_muted_fg()
}

/// Core readable color for secondary tool text (args + previews).
/// Derived from the terminal's actual foreground blended toward its
/// background so it keeps the theme's hue and contrasts on both light
/// and dark tinted backgrounds. Fixed ANSI grays (`DarkGray`/`Gray`)
/// bypass the palette and clash with tinted themes.
fn tool_muted_fg() -> Color {
    // Blend foreground toward background just enough to read as detail
    // rather than dialogue, while preserving contrast.
    match muted_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Gray,
    }
}

/// Foreground for tool output previews: dimmed but still readable.
pub(crate) fn tool_preview_fg() -> Color {
    tool_muted_fg()
}

/// Foreground for tool input arguments: same readable dim as previews.
/// Separate semantic alias so call sites read intention while sharing a
/// single tunable (`tool_muted_fg`). Keeps both in sync modularly.
pub(crate) fn tool_input_fg() -> Color {
    tool_muted_fg()
}

fn background() -> Background {
    match term_palette() {
        Some(p) if p.dark => Background::Dark,
        Some(_) => Background::Light,
        None => Background::Unknown,
    }
}

/// Warm the one-time OSC 11 query. Called eagerly at TUI startup (before raw
/// mode) so later color lookups are pure memo hits.
pub(super) fn detect_background() -> Background {
    background()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_are_consistent_with_background() {
        // Whatever the detected background, surfaces must resolve without
        // panicking and stay on-theme: Reset when the theme is unknown, or
        // RGB derived from the queried palette.
        for color in [surface_bg(), popup_bg()] {
            match color {
                Color::Reset => {}
                Color::Rgb(..) if background() != Background::Unknown => {}
                other => panic!("color leaks theme: {other:?}"),
            }
        }
        // With a known theme the surface foreground is the terminal's own
        // default foreground, never a fixed ANSI slot the theme may remap.
        if background() != Background::Unknown {
            assert!(matches!(surface_fg(), Color::Rgb(..)));
        }
        // Unknown theme must inherit the terminal fg (Reset), never a fixed
        // color that could match the surface it sits on.
        if background() == Background::Unknown {
            assert_eq!(surface_fg(), Color::Reset);
            assert_eq!(surface_bg(), Color::Reset);
        }
    }
}
