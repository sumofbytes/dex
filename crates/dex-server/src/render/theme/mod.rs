//! Theme: terminal palette, surface colors, markdown line semantics and the
//! tree-sitter code highlighter shared by the TUI renderers and the headless
//! stream printer (`llm::transport::sse::turn`). Single home so the TUI and
//! the console agree on colors and markdown spacing rules.
//!
//! `palette.rs` holds the OSC-11 terminal query primitive; the surface
//! color functions live here (moved verbatim from the old `ui/theme.rs`).

pub mod lang;
// Pure line semantics (headings, fences, tables, gaps, language map): no
// terminal dependency, used by the headless stream printer too.
pub mod markdown;
// OSC-11 terminal palette query + RGB blends: terminal-only, no ratatui.
// Both the TUI surface colors and the headless ANSI printers derive from it.
pub mod palette;
// Tree-sitter highlighter + ratatui style resolution: the `StyleSegment`
// engine and `Color` mapping are TUI-only (ratatui types), so the module is
// gated. The headless stream printer gets twin printers below — same markdown
// line semantics and the generic lexer — so a `--no-default-features` binary
// reads like the TUI fallback path.
#[cfg(feature = "tui")]
pub mod highlight;

#[cfg(feature = "tui")]
pub use highlight::{print_code_block, print_markdown_text};
#[cfg(not(feature = "tui"))]
mod headless_printers {
    // Headless twins of the TUI-gated printers: same line semantics via
    // [`super::markdown`] + the generic lexer in [`super::lang`], no
    // tree-sitter/ratatui. Dim fences and ANSI markdown, same as the TUI
    // fallback path, so a `--no-default-features` binary reads like the TUI
    // on a terminal without a highlighter.
    use super::lang::{fallback_enabled, fallback_segments, normalize_code_lang, Tone};
    use crate::runtime::console::RESET;

    const DIM: &str = "\x1b[2m";

    pub fn print_code_block(lang: &str, body: &str) {
        let tag = normalize_code_lang(lang);
        if fallback_enabled(&tag) {
            let mut out = String::new();
            let mut pos = 0usize;
            for (start, end, tone) in fallback_segments(&tag, body) {
                let s = start.max(pos);
                if s > pos {
                    out.push_str(body.get(pos..s).unwrap_or(""));
                }
                if end > s {
                    if let Some(slice) = body.get(s..end) {
                        let code = match tone {
                            Tone::Keyword => "35",
                            Tone::String => "32",
                            Tone::Number => "33",
                            Tone::Comment => "90",
                        };
                        out.push_str("\x1b[");
                        out.push_str(code);
                        out.push('m');
                        out.push_str(slice);
                        out.push_str(RESET);
                    }
                    pos = pos.max(end);
                }
            }
            if pos < body.len() {
                out.push_str(body.get(pos..).unwrap_or(""));
            }
            print!("{out}");
            return;
        }
        print!("{DIM}{body}{RESET}");
    }

    pub fn print_markdown_text(line: &str) {
        println!("{}", super::render_markdown_line_headless(line));
    }
}
#[cfg(not(feature = "tui"))]
pub use headless_printers::{print_code_block, print_markdown_text};

/// Headless markdown line renderer: the pure line semantics in
/// [`markdown`] plus fixed ANSI styling (headings bold, quotes/rules dim,
/// inline code cyan) — no ratatui, mirrors the TUI fallback output.
#[cfg(not(feature = "tui"))]
fn render_markdown_line_headless(line: &str) -> String {
    use crate::render::theme::markdown as md;
    let trimmed = line.trim_start();
    if let Some(body) = md::heading_text(trimmed) {
        if md::heading_level(trimmed) == 1 {
            return format!("\x1b[1;4m{}\x1b[0m", render_inline_ansi(body));
        }
        return format!("\x1b[1m{}\x1b[0m", render_inline_ansi(body));
    }
    if md::is_hr(trimmed) {
        return "\x1b[2m───\x1b[0m".to_string();
    }
    if let Some(body) = md::blockquote_text(trimmed) {
        return format!(
            "\x1b[2m│\x1b[0m \x1b[3m{}\x1b[0m",
            render_inline_ansi(body.trim_start())
        );
    }
    if let Some((body, checked)) = md::task_text(trimmed) {
        let glyph = if checked { "☑" } else { "☐" };
        return format!("\x1b[2m{glyph}\x1b[0m {}", render_inline_ansi(body));
    }
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or(trimmed.strip_prefix("* "))
        .or(trimmed.strip_prefix("+ "))
    {
        return format!("\x1b[2m•\x1b[0m {}", render_inline_ansi(rest));
    }
    render_inline_ansi(trimmed)
}

/// Headless inline styling: `` `code` `` (cyan), `**bold**`, `*italic*`,
/// `[text](url)` — the ANSI twin of the TUI-gated `highlight::render_inline`.
#[cfg(not(feature = "tui"))]
fn render_inline_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text;
    while let Some(i) = rest.find('`') {
        out.push_str(&rest[..i].replace("**", ""));
        rest = &rest[i + 1..];
        match rest.find('`') {
            Some(end) => {
                out.push_str("\x1b[0;36m");
                out.push_str(&rest[..end]);
                out.push_str("\x1b[0m");
                rest = &rest[end + 1..];
            }
            None => {
                out.push('`');
                break;
            }
        }
    }
    out.push_str(rest);
    out
}
#[cfg(feature = "tui")]
use std::sync::{Mutex, OnceLock};

#[cfg(feature = "tui")]
use ratatui::style::Color;

#[cfg(feature = "tui")]
use self::palette::{blend, faint_rgb, fg_rgb, muted_rgb, term_palette, Background};

/// A raised surface: the terminal's actual background lifted a step toward
/// its foreground, so the hue matches the active theme. `amount` controls how
/// far the surface sits above the background (larger = more prominent).
#[cfg(feature = "tui")]
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

/// BG for popup surfaces so they read as floating above the UI.
#[cfg(feature = "tui")]
pub fn popup_bg() -> Color {
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
#[cfg(feature = "tui")]
pub fn surface_fg() -> Color {
    match fg_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Reset,
    }
}

/// Foreground for secondary text on a raised surface (descriptions, hints):
/// quiet, but still readable in both theme modes. Uses the same blended
/// foreground guideline as tool input/previews so it keeps the terminal's
/// hue and stays readable on tinted backgrounds instead of a fixed ANSI gray.
#[cfg(feature = "tui")]
pub fn secondary_fg() -> Color {
    tool_muted_fg()
}

/// Foreground for de-emphasised text on the composer/surface.
/// Uses the same blended-foreground guideline as tool input/previews so
/// de-emphasised text keeps the terminal's hue and stays readable on
/// tinted light/dark backgrounds instead of a fixed low-contrast gray.
#[cfg(feature = "tui")]
pub fn muted_fg() -> Color {
    tool_muted_fg()
}

/// Foreground for hairline separator rules (the sheet top rule): barely
/// visible, far dimmer than readable muted text. Keeps the terminal's hue via
/// the same foreground-toward-background blend.
#[cfg(feature = "tui")]
pub fn hairline_fg() -> Color {
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
#[cfg(feature = "tui")]
struct Voice {
    name: &'static str,
    dark: Color,
    light: Color,
}

#[cfg(feature = "tui")]
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
#[cfg(feature = "tui")]
fn default_voice() -> usize {
    VOICES
        .iter()
        .position(|voice| voice.name == "plain")
        .unwrap_or(0)
}

/// Active voice index into [`VOICES`]. The event loop is single-threaded,
/// so a plain mutex around the slot is plenty.
#[cfg(feature = "tui")]
static VOICE: OnceLock<Mutex<usize>> = OnceLock::new();

/// Serializes tests that read or rotate the process-global voice slot (the
/// theme tests below and the `Alt+V` keybinding test in `ui/remote.rs`).
#[allow(dead_code)] // test helper shared with the `dex` client tests
pub static VOICE_SERIAL: Mutex<()> = Mutex::new(());

#[cfg(feature = "tui")]
fn voice_idx() -> usize {
    *VOICE
        .get_or_init(|| Mutex::new(default_voice()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Advance to the next voice, returning its name for the status-bar notice.
/// The composer and newly submitted prompts pick it up via [`user_fg`];
/// already-submitted rows keep the voice they were sent in.
#[cfg(feature = "tui")]
pub fn cycle_voice() -> &'static str {
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
#[cfg(feature = "tui")]
pub fn user_fg() -> Color {
    voice_shade(&VOICES[voice_idx()], background())
}

/// One voice's shade on a detected background: the bright accent on dark
/// terminals, the deep one on light, and the inherited foreground when the
/// theme is unknown rather than risk an unreadable pick.
#[cfg(feature = "tui")]
fn voice_shade(voice: &Voice, background: Background) -> Color {
    match background {
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
#[cfg(feature = "tui")]
fn tool_muted_fg() -> Color {
    // Blend foreground toward background just enough to read as detail
    // rather than dialogue, while preserving contrast.
    match muted_rgb() {
        Some((r, g, b)) => Color::Rgb(r, g, b),
        None => Color::Gray,
    }
}

/// Foreground for tool output previews: dimmed but still readable.
#[cfg(feature = "tui")]
pub fn tool_preview_fg() -> Color {
    tool_muted_fg()
}

/// Foreground for tool input arguments: same readable dim as previews.
/// Separate semantic alias so call sites read intention while sharing a
/// single tunable (`tool_muted_fg`). Keeps both in sync modularly.
#[cfg(feature = "tui")]
pub fn tool_input_fg() -> Color {
    tool_muted_fg()
}

/// Semantic ANSI accents — Cyan / Yellow / Green / Red. Terminal themes remap
/// these slots to their own palette, so the TUI picks up the active theme's
/// hue instead of a fixed RGB. Two scales share each slot:
///
/// - structural chrome (identity, navigation, pending items) uses the **base**
///   color: Cyan/Yellow,
/// - content outcomes (tool results, ok/fail tails) use the **bright** scale:
///   LightGreen/LightRed, which reads as data rather than as UI.
///
/// Strong `Green`/`Red` are reserved for affirmative state (mode: auto,
/// clean-notice, agents) and the error block. This block is the only place
/// raw `Color::Cyan`/`Yellow`/`Green`/`Red` constants should appear; named
/// roles below are what renderers call.
///
/// Identity / navigation chrome: cwd + branch, mode: plan, info notes, the
/// slash-sheet marker, diff `@@` hunks.
#[cfg(feature = "tui")]
pub fn accent_fg() -> Color {
    Color::Cyan
}

/// Attention: pending items (queued steers, warnings, the `▲ more above`
/// hint), the mode: manual chip, tool-glyph headings.
#[cfg(feature = "tui")]
pub fn warn_fg() -> Color {
    Color::Yellow
}

/// Affirmative state: mode: auto, the transient saved notice, live agents.
#[cfg(feature = "tui")]
pub fn ok_fg() -> Color {
    Color::Green
}

/// Errors: the transcript's `! error: …` block. Tool failures use
/// `failure_fg` (bright scale) so a failed call reads as content outcome,
/// not a UI alarm.
#[cfg(feature = "tui")]
pub fn error_fg() -> Color {
    Color::Red
}

/// Content outcome — success (tool `✓` tails, settled turn summary, clean
/// branch).
#[cfg(feature = "tui")]
pub fn success_fg() -> Color {
    Color::LightGreen
}

/// Content outcome — failure (tool `✗` tails, failed diffs, context past
/// the compaction trigger).
#[cfg(feature = "tui")]
pub fn failure_fg() -> Color {
    Color::LightRed
}

#[cfg(feature = "tui")]
fn background() -> Background {
    match term_palette() {
        Some(p) if p.dark => Background::Dark,
        Some(_) => Background::Light,
        None => Background::Unknown,
    }
}

/// Warm the one-time OSC 11 query. Called eagerly at TUI startup (before raw
/// mode) so later color lookups are pure memo hits.
#[cfg(feature = "tui")]
pub fn detect_background() -> Background {
    background()
}

#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;

    #[test]
    fn surfaces_are_consistent_with_background() {
        // Whatever the detected background, surfaces must resolve without
        // panicking and stay on-theme: Reset when the theme is unknown, or
        // RGB derived from the queried palette.
        for color in [popup_bg()] {
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
        }
    }

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
    fn voice_shade_picks_a_shade_per_background() {
        // Every voice resolves per detected background: bright on dark, deep
        // on light, inherited foreground when the terminal never answered.
        for voice in VOICES {
            assert_eq!(voice_shade(voice, Background::Dark), voice.dark);
            assert_eq!(voice_shade(voice, Background::Light), voice.light);
            assert_eq!(voice_shade(voice, Background::Unknown), Color::Reset);
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
