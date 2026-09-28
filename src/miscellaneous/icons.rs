//! File-type icons for the tree: a Nerd Font (devicons/seti) glyph plus a
//! per-language colour. The icon font is embedded and registered at startup
//! (see `main`), and referenced elsewhere via [`ICON_FONT`].

use iced::{Color, Font};

use crate::theme::{self, rgb};

/// Raw bytes of the embedded icon font (Symbols Nerd Font Mono, OFL/MIT).
pub const FONT_BYTES: &[u8] = include_bytes!("../../assets/SymbolsNerdFontMono-Regular.ttf");

/// The icon font family, for rendering the glyphs below.
pub const ICON_FONT: Font = Font::with_name("Symbols Nerd Font Mono");

/// A closed / open folder glyph and its colour.
pub fn folder_icon(open: bool) -> (char, Color) {
    let glyph = if open { '\u{f07c}' } else { '\u{f07b}' };
    (glyph, theme::dim())
}

/// The icon (glyph + colour) for a file, chosen by well-known filename first,
/// then by extension. Falls back to a neutral document glyph. The colour
/// follows the active theme (see [`shade_for`]).
pub fn file_icon(name: &str) -> (char, Color) {
    let (glyph, hue) = icon_of(name);
    let color = match hue {
        Some(hue) => shaded(hue),
        None => theme::dim(),
    };
    (glyph, color)
}

/// [`shade_for`] of `hue` in the active theme, remembered per theme: it runs
/// for every file row of every tree on every `view`, and finding the pull is
/// up to twenty steps of contrast math against each of the surfaces an icon
/// is drawn on ([`icon_surfaces`]).
fn shaded(hue: u32) -> Color {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static SHADES: RefCell<HashMap<(usize, u32), Color>> = RefCell::new(HashMap::new());
    }
    let theme = theme::active_index();
    SHADES.with(|shades| {
        *shades.borrow_mut().entry((theme, hue)).or_insert_with(|| {
            let def = &theme::THEMES[theme];
            shade_for(rgb(hue), def.is_light, &def.palette)
        })
    })
}

/// The surfaces a file icon is drawn on: the editor, the panels, a floating
/// dialog (the finder), and a tree or list row hovered or selected (the
/// current file's row, the finder's highlighted result).
fn icon_surfaces(palette: &theme::Palette) -> [Color; 5] {
    [
        palette.bg,
        palette.bg_panel,
        palette.elevated,
        palette.bg_hover,
        palette.selected,
    ]
}

/// A file type's brand hue as drawn on `palette`'s theme: pulled toward the
/// theme's foreground just far enough to clear [`MIN_ICON_CONTRAST`] against
/// every surface an icon is drawn on ([`icon_surfaces`]). On a light theme
/// it always moves at least [`LIGHT_THEME_PULL`] — the pale brand hues
/// (yellow, sky blue) all but vanished there — and then further only where
/// that theme's surfaces demand it, in small steps, so the hue stays as
/// recognisable as the theme allows.
pub(crate) fn shade_for(brand: Color, light: bool, palette: &theme::Palette) -> Color {
    let surfaces = icon_surfaces(palette);
    let legible = |c: Color| {
        surfaces
            .iter()
            .all(|&surface| theme::contrast(c, surface) >= MIN_ICON_CONTRAST)
    };
    let mut pull = if light { LIGHT_THEME_PULL } else { 0.0 };
    loop {
        let drawn = theme::mix(brand, palette.fg, pull);
        if legible(drawn) || pull >= 1.0 {
            return drawn;
        }
        pull = (pull + PULL_STEP).min(1.0);
    }
}

/// WCAG's minimum contrast for a graphical object that carries meaning.
const MIN_ICON_CONTRAST: f32 = 3.0;

/// The least a brand hue moves toward the foreground on a light theme, for a
/// consistent look across hues.
const LIGHT_THEME_PULL: f32 = 0.45;

/// How much further a hue that still falls short moves per step.
const PULL_STEP: f32 = 0.05;

/// The glyph and, for a file type with a colour of its own, its brand hue;
/// `None` for the neutral types, which take the theme's dim colour.
fn icon_of(name: &str) -> (char, Option<u32>) {
    let lower = name.to_ascii_lowercase();

    // Whole-name matches for the files that deserve a distinctive look.
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return ('\u{e7a8}', Some(0xdea584)),
        "package.json" | "package-lock.json" => return ('\u{e718}', Some(0x8cc84b)),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return ('\u{e702}', Some(0xf14e32)),
        "dockerfile" => return ('\u{f308}', Some(0x2496ed)),
        "makefile" => return ('\u{e673}', None),
        "readme.md" | "readme" => return ('\u{e73e}', Some(0x9db4d0)),
        "license" | "license.md" | "license.txt" => return ('\u{f0219}', None),
        _ => {}
    }

    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext {
        "rs" => ('\u{e7a8}', Some(0xdea584)),
        "py" | "pyi" => ('\u{e606}', Some(0x5b9bd5)),
        "js" | "mjs" | "cjs" => ('\u{e74e}', Some(0xe5c07b)),
        "ts" => ('\u{e628}', Some(0x519aba)),
        "tsx" | "jsx" => ('\u{e7ba}', Some(0x61dafb)),
        "go" => ('\u{e627}', Some(0x4fc3dc)),
        "dart" => ('\u{e798}', Some(0x40a0d8)),
        "c" | "h" => ('\u{e61e}', Some(0x6ea8e0)),
        "cpp" | "cc" | "cxx" | "hpp" => ('\u{e61d}', Some(0xe06c9a)),
        "rb" => ('\u{e739}', Some(0xd06a58)),
        "java" => ('\u{e738}', Some(0xe0784b)),
        "kt" | "kts" => ('\u{e634}', Some(0xb07be0)),
        "swift" => ('\u{e755}', Some(0xf0803c)),
        "html" | "htm" => ('\u{e736}', Some(0xe06c4b)),
        "css" => ('\u{e749}', Some(0x61afef)),
        "scss" | "sass" => ('\u{e749}', Some(0xcf6f9c)),
        "json" | "jsonc" => ('\u{e60b}', Some(0xe5c07b)),
        "toml" => ('\u{e6b2}', None),
        "yaml" | "yml" => ('\u{e6a8}', None),
        "md" | "markdown" => ('\u{e73e}', Some(0x9db4d0)),
        "sh" | "bash" | "zsh" | "fish" => ('\u{e795}', Some(0x98c379)),
        "lock" => ('\u{f023}', None),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => ('\u{f1c5}', Some(0xb07be0)),
        "svg" => ('\u{f1c5}', Some(0xe0a03c)),
        "pdf" => ('\u{f1c1}', Some(0xe0574b)),
        "txt" | "log" => ('\u{f15c}', None),
        _ => ('\u{f15b}', None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2-11: the icon colours follow the theme. The neutral types take the
    /// theme's own dim colour, and every brand hue, as drawn, clears the 3:1
    /// non-text contrast against every surface an icon sits on — the selected
    /// and hovered rows included — in EVERY shipped theme: the pale hues
    /// (yellow JavaScript, sky-blue Markdown) did not on a light one while
    /// they were fixed colours, a fixed pull tuned on one light theme left
    /// four hues at 2.6–3.0 on gruvbox-light and paper-light, and the
    /// selected row (the current file's) was not checked at all.
    #[test]
    fn every_icon_stays_legible_on_every_theme() {
        let names = [
            "Cargo.toml",
            "package.json",
            ".gitignore",
            "Dockerfile",
            "README.md",
            "a.rs",
            "a.py",
            "a.js",
            "a.ts",
            "a.tsx",
            "a.go",
            "a.dart",
            "a.c",
            "a.cpp",
            "a.rb",
            "a.java",
            "a.kt",
            "a.swift",
            "a.html",
            "a.css",
            "a.scss",
            "a.json",
            "a.md",
            "a.sh",
            "a.png",
            "a.svg",
            "a.pdf",
        ];
        for theme in theme::THEMES {
            let p = &theme.palette;
            for name in names {
                let hue = icon_of(name).1.expect("a type with a colour of its own");
                let drawn = shade_for(rgb(hue), theme.is_light, p);
                for (surface, bg) in [
                    ("editor", p.bg),
                    ("panel", p.bg_panel),
                    ("dialog", p.elevated),
                    ("hovered row", p.bg_hover),
                    ("selected row", p.selected),
                ] {
                    let ratio = theme::contrast(drawn, bg);
                    assert!(
                        ratio >= MIN_ICON_CONTRAST,
                        "{name} on {}'s {surface}: contrast {ratio:.2}",
                        theme.id
                    );
                }
            }
        }
        // The pull is the least that works: a hue already legible on a dark
        // theme is drawn as is, and on a light one moves the baseline only.
        let one_dark = theme::THEMES.iter().find(|t| t.id == "one-dark").unwrap();
        let rust = rgb(0xdea584);
        assert_eq!(shade_for(rust, false, &one_dark.palette), rust);
        let one_light = theme::THEMES.iter().find(|t| t.id == "one-light").unwrap();
        let p = &one_light.palette;
        assert_eq!(
            shade_for(rgb(0x5b9bd5), true, p),
            theme::mix(rgb(0x5b9bd5), p.fg, LIGHT_THEME_PULL)
        );
        for name in [
            "Makefile", "LICENSE", "a.toml", "a.yml", "a.lock", "a.txt", "a.xyz",
        ] {
            assert_eq!(icon_of(name).1, None, "{name} is a neutral type");
        }
    }
}
