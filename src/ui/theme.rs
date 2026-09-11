//! Theme-aware surface colors for the TUI.
//!
//! Fixed palette slots bypass the terminal's theme (the 256-color gray ramp
//! is never remapped), so neutral grays clash with tinted backgrounds. Surface
//! colors here are instead derived from the terminal's real background and
//! foreground colors, queried once at startup (OSC 11): a surface is the
//! background blended a step toward the foreground, so it keeps the theme's
//! hue and always contrasts with text on it.

use std::sync::{Mutex, OnceLock};

use ratatui::style::Color;

use crate::core::palette::{blend, faint_rgb, fg_rgb, muted_rgb, term_palette};

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

/// BG for overlays: a "raised" surface.
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

/// Foreground for hairline separator rules (sheet top rule): barely visible, far dimmer than readable muted text. Keeps the
/// terminal's hue via the same foreground-toward-background blend.
pub(crate) fn hairline_fg() -> Color {
    match faint_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::DarkGray,
    }
}

/// One selectable user voice: the status-notice name plus the bright shade
/// for dark terminals and the deep shade for light ones. Every accent entry
/// steers clear of the claimed slots — Cyan chrome, LightGreen ok-states,
/// Yellow warnings, Red errors; `plain` is the pre-voice original, the
/// inherited foreground on both themes.
struct Voice {
    name: &'static str,
    dark: Color,
    light: Color,
}

const VOICES: &[Voice] = &[
    Voice {
        name: "magenta",
        dark: Color::LightMagenta,
        light: Color::Magenta,
    },
    Voice {
        name: "sky",
        dark: Color::LightBlue,
        light: Color::Blue,
    },
    Voice {
        name: "peach",
        dark: Color::Rgb(255, 190, 130),
        light: Color::Rgb(176, 92, 24),
    },
    Voice {
        name: "violet",
        dark: Color::Rgb(200, 160, 255),
        light: Color::Rgb(110, 60, 180),
    },
    Voice {
        name: "rose",
        dark: Color::Rgb(255, 150, 180),
        light: Color::Rgb(190, 45, 95),
    },
    Voice {
        name: "amber",
        dark: Color::Rgb(255, 195, 85),
        light: Color::Rgb(150, 95, 5),
    },
    Voice {
        name: "coral",
        dark: Color::Rgb(255, 140, 115),
        light: Color::Rgb(185, 65, 40),
    },
    Voice {
        name: "plain",
        dark: Color::Reset,
        light: Color::Reset,
    },
];

/// Default voice index: `plain`, the pre-voice inherited foreground.
/// A lookup (not a literal) so reordering [`VOICES`] can't silently change
/// the default. The first `Alt+V` press steps into magenta.
fn default_voice() -> usize {
    VOICES
        .iter()
        .position(|voice| voice.name == "plain")
        .unwrap_or(0)
}

/// Active voice index into [`VOICES`]. The event loop is single-threaded,
/// so a plain mutex around the slot is plenty.
static VOICE: OnceLock<Mutex<usize>> = OnceLock::new();

fn voice_idx() -> usize {
    VOICE
        .get_or_init(|| Mutex::new(default_voice()))
        .lock()
        .map(|slot| *slot)
        .unwrap_or(default_voice())
        % VOICES.len()
}

/// Advance to the next voice, returning its name for the status-bar notice.
/// The composer and newly submitted prompts pick it up via [`user_fg`];
/// already-submitted rows keep the voice they were sent in.
pub(crate) fn cycle_voice() -> &'static str {
    let mut slot = VOICE
        .get_or_init(|| Mutex::new(default_voice()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = (*slot + 1) % VOICES.len();
    VOICES[*slot].name
}

/// Signature color for the user's own words, in the composer and in
/// submitted prompts: a rotatable voice, plain (the inherited foreground)
/// by default and instantly separable from the assistant once rotated.
/// Accent hues can't be derived from the queried
/// palette, so each shade is picked per background — bright on dark
/// terminals, deep on light ones — and inherits the default foreground when
/// the theme is unknown rather than risk an unreadable pick. `Alt+V` cycles
/// the voice — magenta → sky → peach → violet → rose → amber → coral →
/// plain — and the default is `plain`, the inherited foreground.
pub(crate) fn user_fg() -> Color {
    let voice = &VOICES[voice_idx()];
    match background() {
        Background::Dark => voice.dark,
        Background::Light => voice.light,
        Background::Unknown => Color::Reset,
    }
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

    static VOICE_SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn user_voice_matches_background() {
        // The default voice is `plain`: the inherited foreground on every
        // theme — never an unreadable pick.
        let _guard = VOICE_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *VOICE
            .get_or_init(|| Mutex::new(default_voice()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = default_voice();
        assert_eq!(user_fg(), Color::Reset);
    }

    #[test]
    fn default_voice_is_plain() {
        // Out of the box the user's words inherit the terminal foreground;
        // the first `Alt+V` press steps into magenta.
        let _guard = VOICE_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let mut slot = VOICE
                .get_or_init(|| Mutex::new(default_voice()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = default_voice();
        }
        assert_eq!(VOICES[voice_idx()].name, "plain");
        assert_eq!(user_fg(), Color::Reset);
        assert_eq!(cycle_voice(), "magenta");
        *VOICE
            .get_or_init(|| Mutex::new(default_voice()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = default_voice();
    }

    #[test]
    fn voices_have_distinct_names_and_theme_shades() {
        let mut names: Vec<_> = VOICES.iter().map(|voice| voice.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            VOICES.len(),
            "each voice needs a distinct status-notice name"
        );
        for voice in VOICES {
            assert!(!voice.name.is_empty());
            if voice.name == "plain" {
                // The pre-voice original: inherited foreground on both themes.
                assert_eq!(voice.dark, Color::Reset);
                assert_eq!(voice.light, Color::Reset);
            } else {
                assert_ne!(
                    voice.dark, voice.light,
                    "voice {} needs distinct dark/light shades",
                    voice.name
                );
            }
        }
    }

    #[test]
    fn voice_rotation_wraps_around() {
        let _guard = VOICE_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = voice_idx();
        let start_fg = user_fg();
        for _ in 0..VOICES.len() {
            let name = cycle_voice();
            assert!(
                VOICES.iter().any(|voice| voice.name == name),
                "cycle_voice returned an unknown voice: {name}"
            );
        }
        assert_eq!(voice_idx(), start, "a full rotation must restore the voice");
        assert_eq!(user_fg(), start_fg);
    }
}
