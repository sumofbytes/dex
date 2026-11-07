//! Colour schemes: dex's warm default plus every scheme bundled from Po (`themes/schemes.txt`,
//! copied from Po). Each scheme gives three seeds (background, text, accent) that `rune-kit`
//! derives the whole UI palette from, as Po does for its terminal colours.

use std::sync::{OnceLock, RwLock};

use rune::Color;
use rune::color::hex;
use rune_kit::{Kit, PaletteSeed};

/// Po's scheme list: `@@ <name>` lines, each followed by `key = value` lines.
const BUNDLE: &str = include_str!("../themes/schemes.txt");

/// dex's own warm dark theme, the default.
pub const DEFAULT: &str = "dex";

#[derive(Clone, Copy)]
struct Active {
    kit: &'static Kit,
    accent: Color,
    on_accent: Color,
}

fn active() -> &'static RwLock<(String, Active)> {
    static ACTIVE: OnceLock<RwLock<(String, Active)>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new((DEFAULT.into(), build_dex())))
}

fn c(s: &str) -> Color {
    hex(s).unwrap_or(Color::WHITE)
}

fn build_dex() -> Active {
    let (bg, fg, accent) = (c("#342d2d"), c("#f4eeee"), c("#f38d70"));
    Active {
        kit: leak(bg, fg, accent),
        accent,
        on_accent: c("#2a1f1c"),
    }
}

/// A scheme's three seeds (background, text, accent) for a swatch in the picker.
pub fn swatch(name: &str) -> (Color, Color, Color) {
    if name == DEFAULT {
        return (c("#342d2d"), c("#f4eeee"), c("#f38d70"));
    }
    seeds(name).unwrap_or((c("#342d2d"), c("#f4eeee"), c("#f38d70")))
}

/// A scheme's seeds from its section of the bundle. `accent` is its blue, as Po's primary.
fn seeds(name: &str) -> Option<(Color, Color, Color)> {
    let body = BUNDLE.split("@@ ").skip(1).find_map(|sec| {
        let (title, body) = sec.split_once('\n')?;
        title.trim().eq_ignore_ascii_case(name).then_some(body)
    })?;
    let (mut bg, mut fg, mut blue) = (None, None, None);
    for line in body.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "background" => bg = hex(value),
            "foreground" => fg = hex(value),
            "palette" => {
                if let Some((i, color)) = value.split_once('=')
                    && i.trim() == "4"
                {
                    blue = hex(color.trim());
                }
            }
            _ => {}
        }
    }
    let (bg, fg) = (bg?, fg?);
    Some((bg, fg, blue.unwrap_or(fg)))
}

/// A scheme's seeds from its section of the bundle, ready to build a kit.
fn build(name: &str) -> Option<Active> {
    let (bg, fg, accent) = seeds(name)?;
    // Ink on the accent: whichever of background and text contrasts more with it.
    let on_accent = if (bg.luminance() - accent.luminance()).abs()
        >= (fg.luminance() - accent.luminance()).abs()
    {
        bg
    } else {
        fg
    };
    Some(Active {
        kit: leak(bg, fg, accent),
        accent,
        on_accent,
    })
}

/// Kits are built once per switch and kept for the life of the process (a handful at most).
/// Rune's scale uses 11, 13 and odd line heights; the app's is whole and even: 12 (small,
/// captions, mono, UI rows), 14 (body and reading text), with even line heights. Rune's own components follow.
fn even_type(k: &mut Kit) {
    use rune::TextStyle;
    let ui = TextStyle::new(12.0, 400.0).lh(18.0);
    let body = TextStyle::new(14.0, 400.0).lh(22.0);
    k.ty.ui = ui;
    k.ty.ui_medium = TextStyle {
        weight: 500.0,
        ..ui
    };
    k.ty.ui_compact = ui.lh(16.0);
    k.ty.small = TextStyle::new(12.0, 400.0).lh(16.0);
    k.ty.small_medium = TextStyle::new(12.0, 500.0).lh(16.0);
    k.ty.body = body;
    k.ty.body_medium = TextStyle {
        weight: 500.0,
        ..body
    };
    k.ty.text_small = ui;
    k.ty.text_small_medium = TextStyle {
        weight: 500.0,
        ..ui
    };
    k.ty.text_xsmall = TextStyle::new(12.0, 400.0).lh(18.0);
    k.ty.text_xsmall_medium = TextStyle::new(12.0, 500.0).lh(18.0);
}

fn leak(bg: Color, fg: Color, accent: Color) -> &'static Kit {
    let mut kit = Kit::from_palette(PaletteSeed { bg, fg, accent });
    even_type(&mut kit);
    Box::leak(Box::new(kit))
}

/// Every scheme's name: dex first, then Po's bundle in its order.
pub fn names() -> Vec<&'static str> {
    std::iter::once(DEFAULT)
        .chain(
            BUNDLE
                .lines()
                .filter_map(|l| l.strip_prefix("@@ "))
                .map(str::trim),
        )
        .collect()
}

/// The active scheme's name.
pub fn name() -> String {
    active().read().unwrap().0.clone()
}

/// Switch to scheme `name`. Returns false (and changes nothing) for an unknown name.
pub fn set(name: &str) -> bool {
    let next = if name == DEFAULT {
        Some(build_dex())
    } else {
        build(name)
    };
    let Some(next) = next else { return false };
    *active().write().unwrap() = (name.to_string(), next);
    true
}

pub fn kit() -> &'static Kit {
    active().read().unwrap().1.kit
}

pub fn accent() -> Color {
    active().read().unwrap().1.accent
}

/// The accent button's ink on `accent()`.
pub fn on_accent() -> Color {
    active().read().unwrap().1.on_accent
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
/// The tool name label in the transcript, a cool blue beside the warm palette.
pub fn tool_label() -> Color {
    c("#8fb4ff")
}

pub fn waiting() -> Color {
    c("#f2c87a")
}
pub fn done() -> Color {
    c("#8fd694")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_scheme_builds() {
        let names = names();
        assert!(names.len() > 400);
        for n in names {
            assert!(build(n).is_some() || n == DEFAULT, "{n} did not build");
        }
    }
}
