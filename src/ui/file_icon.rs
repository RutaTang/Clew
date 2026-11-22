//! A file's icon, by what the file is: where a list names files — the file
//! tree, its filter, Go to File, search results — each says at a glance
//! what kind of file it is, as an editor's tree does.
//!
//! Source files are marked with their language's short name on a tile of
//! its colour (RS, PY, TS…): original, legible at 14 px, and the same shape
//! for every language, however many there are. Files that are not source
//! are drawn with one of Clew's own line glyphs — braces for JSON, a gear
//! for settings, a picture for an image, a lock for a lockfile — and any
//! other file with the plain page it had.
//!
//! The colours are each language's well-known hue, its letters kept
//! readable on the panel in light and dark themes.

use iced::widget::{container, text};
use iced::{Color, Element, Length};

use crate::Message;
use crate::glyph::{self, Glyph};
use crate::theme;

/// What a file is drawn as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileMark {
    /// A language's letters on a tile of its colour.
    Tile { letters: &'static str, hue: u32 },
    /// One of Clew's glyphs, in a hue of its own or in the row's plain
    /// colour.
    Glyph { glyph: Glyph, hue: Option<u32> },
}

/// The plain page: a file Clew has no mark for.
const PLAIN: FileMark = FileMark::Glyph {
    glyph: Glyph::File,
    hue: None,
};

const fn tile(letters: &'static str, hue: u32) -> FileMark {
    FileMark::Tile { letters, hue }
}

const fn mark(glyph: Glyph, hue: Option<u32>) -> FileMark {
    FileMark::Glyph { glyph, hue }
}

/// Settings and build files: a gear, in the row's colour.
const SETTINGS: FileMark = mark(Glyph::Settings, None);
/// Text that is read, not run.
const TEXT: FileMark = mark(Glyph::TextLines, None);

/// The mark of the file named `name` (its last path part).
pub(crate) fn mark_of(name: &str) -> FileMark {
    let lower = name.to_ascii_lowercase();
    // Whole names first: what a file is called can say more than its
    // extension (`Cargo.lock`, `Dockerfile`, `.gitignore`).
    if let Some(mark) = by_name(&lower) {
        return mark;
    }
    match lower.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => by_extension(ext).unwrap_or(PLAIN),
        _ => PLAIN,
    }
}

fn by_name(name: &str) -> Option<FileMark> {
    Some(match name {
        "cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "poetry.lock"
        | "gemfile.lock" | "composer.lock" | "go.sum" | "flake.lock" | "uv.lock"
        | "pipfile.lock" | "bun.lockb" | "mix.lock" | "pubspec.lock" | "podfile.lock" => {
            mark(Glyph::Lock, None)
        }
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" | ".mailmap" => {
            mark(Glyph::Branch, Some(0xF05032))
        }
        "dockerfile"
        | "containerfile"
        | ".dockerignore"
        | "docker-compose.yml"
        | "docker-compose.yaml"
        | "compose.yml"
        | "compose.yaml" => mark(Glyph::Cube, Some(0x2496ED)),
        "makefile" | "gnumakefile" | "justfile" | "cmakelists.txt" | "meson.build"
        | "build.gradle" | "build.gradle.kts" | "settings.gradle" | "rakefile" | "gemfile"
        | "pipfile" | "procfile" | "vagrantfile" | ".editorconfig" | ".env" => SETTINGS,
        _ if name.starts_with(".env.") => SETTINGS,
        _ if name.starts_with("readme") => mark(Glyph::OpenBook, Some(0x3B82C4)),
        _ if name.starts_with("license")
            || name.starts_with("licence")
            || name.starts_with("copying")
            || name.starts_with("notice") =>
        {
            TEXT
        }
        _ if name.starts_with("dockerfile.") || name.ends_with(".dockerfile") => {
            mark(Glyph::Cube, Some(0x2496ED))
        }
        _ if name.ends_with(".lock") => mark(Glyph::Lock, None),
        _ => return None,
    })
}

fn by_extension(ext: &str) -> Option<FileMark> {
    Some(match ext {
        // Languages Clew reads, then others often met in a project.
        "rs" => tile("RS", 0xD2632A),
        "py" | "pyi" | "pyw" | "pyx" => tile("PY", 0x3776AB),
        "ipynb" => tile("NB", 0xF37626),
        "ts" | "mts" | "cts" | "tsx" => tile("TS", 0x3178C6),
        "js" | "mjs" | "cjs" | "jsx" => tile("JS", 0xE8C21A),
        "go" => tile("GO", 0x00ADD8),
        "c" => tile("C", 0x5C7CBA),
        "h" => tile("H", 0x8E7CC3),
        "cpp" | "cc" | "cxx" | "c++" | "ipp" | "tpp" => tile("C++", 0xF34B7D),
        "hpp" | "hh" | "hxx" | "h++" => tile("H", 0xF34B7D),
        "java" => tile("J", 0xB07219),
        "kt" | "kts" => tile("KT", 0xA97BFF),
        "scala" | "sc" => tile("SC", 0xDC322F),
        "groovy" | "gradle" => tile("GR", 0x4298B8),
        "cs" => tile("C#", 0x178600),
        "fs" | "fsx" | "fsi" => tile("F#", 0x378BBA),
        "php" => tile("PHP", 0x777BB4),
        "rb" | "erb" | "rake" | "gemspec" => tile("RB", 0xCC342D),
        "swift" => tile("SW", 0xF05138),
        "m" | "mm" => tile("OC", 0x438EFF),
        "zig" | "zon" => tile("Z", 0xEC915C),
        "lean" => tile("LN", 0x4E70B8),
        "hs" | "lhs" => tile("HS", 0x5E5086),
        "cabal" => tile("HS", 0x5E5086),
        "erl" | "hrl" => tile("ER", 0xB83998),
        "ex" | "exs" | "heex" => tile("EX", 0x6E4A7E),
        "sql" => tile("SQL", 0xE38C00),
        "lua" => tile("LU", 0x000080),
        "dart" => tile("DT", 0x00B4AB),
        "r" => tile("R", 0x198CE7),
        "jl" => tile("JL", 0xA270BA),
        "ml" | "mli" => tile("ML", 0xEF7A08),
        "clj" | "cljs" | "cljc" | "edn" => tile("CL", 0xDB5855),
        "elm" => tile("EL", 0x60B5CC),
        "nim" => tile("NI", 0xFFC200),
        "v" | "sv" | "vhd" | "vhdl" => tile("HD", 0x848BF3),
        "pl" | "pm" => tile("PL", 0x0298C3),
        "vue" => tile("V", 0x41B883),
        "svelte" => tile("S", 0xFF3E00),
        "astro" => tile("A", 0xFF5A03),
        "sh" | "bash" | "zsh" | "fish" | "ksh" => tile("SH", 0x4EAA25),
        "ps1" | "psm1" | "psd1" => tile("PS", 0x2B5797),
        "bat" | "cmd" => tile("BAT", 0xC1F12E),
        "html" | "htm" | "xhtml" => tile("<>", 0xE34C26),
        "xml" | "xsd" | "xsl" | "xslt" | "plist" | "csproj" | "fsproj" | "vcxproj" => {
            tile("<>", 0x0060AC)
        }
        "css" => tile("#", 0x663399),
        "scss" | "sass" => tile("#", 0xC6538C),
        "less" => tile("#", 0x1D365D),
        "md" | "markdown" | "mdx" | "mdown" => tile("M↓", 0x519ABA),
        "rst" | "adoc" | "asciidoc" | "org" | "tex" => TEXT,
        "txt" | "log" | "text" => TEXT,
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" | "geojson" => {
            mark(Glyph::Braces, Some(0xCBA200))
        }
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "config" | "properties" | "env"
        | "editorconfig" | "sln" | "mk" | "cmake" | "nix" | "tf" | "hcl" => SETTINGS,
        "lock" => mark(Glyph::Lock, None),
        "proto" => tile("PB", 0x4285F4),
        "graphql" | "gql" => tile("GQ", 0xE10098),
        "wasm" | "wat" => tile("WA", 0x654FF0),
        "csv" | "tsv" => tile("CSV", 0x2E8B57),
        "pdf" => tile("PDF", 0xE5252A),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "icns" | "tif" | "tiff"
        | "avif" | "heic" | "svg" => mark(Glyph::Image, Some(0x9A6FD6)),
        _ => return None,
    })
}

/// The colour `0xRRGGBB` is.
fn rgb(hex: u32) -> Color {
    Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `hue`, made readable on `back`: drawn toward the text colour until it
/// stands out as a small mark must — a pale yellow on a light theme
/// darkened, a deep navy on a dark one lightened.
fn readable_on(hue: Color, back: Color) -> Color {
    let toward = theme::fg();
    let mut t = 0.0;
    let mut ink = hue;
    while theme::contrast(ink, back) < MIN_CONTRAST && t < 1.0 {
        t += 0.1;
        ink = theme::mix(hue, toward, t);
    }
    ink
}

/// `hue`, made readable on the panel.
pub(crate) fn readable(hue: Color) -> Color {
    readable_on(hue, theme::bg_panel())
}

/// How far a mark's colour stands from what is under it: between WCAG's for
/// graphics (3) and for small text (4.5) — a tile's letters are both.
const MIN_CONTRAST: f32 = 4.0;

/// The tile's share of the icon's square: a filled tile reads larger than
/// a stroked glyph of the same box.
const TILE_SHARE: f32 = 0.93;

/// The icon of the file named `name`, `size` square; `plain` colours a
/// mark with no hue of its own (as the row's other icons are).
pub(crate) fn file_icon<'a>(name: &str, size: f32, plain: Color) -> Element<'a, Message> {
    let body: Element<'a, Message> = match mark_of(name) {
        FileMark::Glyph { glyph, hue } => {
            glyph::icon(glyph, hue.map_or(plain, |h| readable(rgb(h))), size)
        }
        FileMark::Tile { letters, hue } => {
            let alpha = if theme::is_light() { 0.14 } else { 0.24 };
            let wash = Color {
                a: alpha,
                ..rgb(hue)
            };
            // Read on the tile, the wash over the panel, not on the panel.
            let ink = readable_on(rgb(hue), theme::mix(theme::bg_panel(), rgb(hue), alpha));
            let side = (size * TILE_SHARE).round();
            // Fewer letters, larger: one fills the tile as two do.
            let letter = side
                * match letters.chars().count() {
                    1 => 0.72,
                    2 => 0.6,
                    _ => 0.48,
                };
            container(
                text(letters)
                    .font(crate::fonts::CODE_BOLD)
                    .size(letter)
                    .line_height(1.0)
                    .color(ink),
            )
            .center(Length::Fixed(side))
            .style(move |_| container::Style {
                background: Some(wash.into()),
                border: iced::Border {
                    radius: (side * 0.22).into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
        }
    };
    container(body).center(Length::Fixed(size)).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each file is marked by what it is: its language's letters, a glyph
    /// for what is not source, its whole name before its extension, and
    /// the plain page for anything else.
    #[test]
    fn files_are_marked_by_what_they_are() {
        let letters = |name: &str| match mark_of(name) {
            FileMark::Tile { letters, .. } => Some(letters),
            FileMark::Glyph { .. } => None,
        };
        let glyph = |name: &str| match mark_of(name) {
            FileMark::Glyph { glyph, .. } => Some(glyph),
            FileMark::Tile { .. } => None,
        };
        assert_eq!(letters("lib.rs"), Some("RS"));
        assert_eq!(letters("main.py"), Some("PY"));
        assert_eq!(letters("App.TSX"), Some("TS"), "any case");
        assert_eq!(letters("index.mjs"), Some("JS"));
        assert_eq!(letters("server.go"), Some("GO"));
        assert_eq!(letters("engine.cpp"), Some("C++"));
        assert_eq!(letters("Proof.lean"), Some("LN"));
        assert_eq!(letters("README.md"), None, "README is a book first");
        assert_eq!(letters("guide.md"), Some("M↓"));
        assert_eq!(glyph("README.md"), Some(Glyph::OpenBook));
        assert_eq!(glyph("Cargo.lock"), Some(Glyph::Lock));
        assert_eq!(glyph("yarn.lock"), Some(Glyph::Lock));
        assert_eq!(
            glyph("package-lock.json"),
            Some(Glyph::Lock),
            "a lockfile, not JSON"
        );
        assert_eq!(glyph("package.json"), Some(Glyph::Braces));
        assert_eq!(glyph(".gitignore"), Some(Glyph::Branch));
        assert_eq!(glyph("Dockerfile"), Some(Glyph::Cube));
        assert_eq!(glyph("Cargo.toml"), Some(Glyph::Settings));
        assert_eq!(glyph("Makefile"), Some(Glyph::Settings));
        assert_eq!(glyph(".env.local"), Some(Glyph::Settings));
        assert_eq!(glyph("logo.PNG"), Some(Glyph::Image));
        assert_eq!(glyph("LICENSE"), Some(Glyph::TextLines));
        assert_eq!(glyph("notes.txt"), Some(Glyph::TextLines));
        // Nothing known: the plain page — a dotfile's dot is not an
        // extension, nor is an unknown one.
        assert_eq!(mark_of("data.xyz"), PLAIN);
        assert_eq!(mark_of(".bashrc"), PLAIN);
        assert_eq!(mark_of("rs"), PLAIN);
    }

    /// A tile's letters stand out from the tile under them, and a glyph's
    /// hue from the panel, in a light theme and a dark one.
    #[test]
    fn marks_are_readable_in_light_and_dark() {
        let names = [
            "a.rs",
            "a.py",
            "a.ts",
            "a.js",
            "a.go",
            "a.c",
            "a.h",
            "a.cpp",
            "a.java",
            "a.kt",
            "a.cs",
            "a.php",
            "a.rb",
            "a.swift",
            "a.zig",
            "a.lean",
            "a.hs",
            "a.erl",
            "a.sql",
            "a.lua",
            "a.dart",
            "a.sh",
            "a.bat",
            "a.html",
            "a.css",
            "a.less",
            "a.md",
            "a.csv",
            "a.pdf",
            "a.nim",
            "a.json",
            "a.png",
            ".gitignore",
            "Dockerfile",
            "README",
        ];
        let was = theme::is_light();
        for light in [true, false] {
            crate::theme::set_light(light);
            for name in names {
                let (ink, back) = match mark_of(name) {
                    FileMark::Tile { hue, .. } => {
                        let alpha = if light { 0.14 } else { 0.24 };
                        let back = theme::mix(theme::bg_panel(), rgb(hue), alpha);
                        (readable_on(rgb(hue), back), back)
                    }
                    FileMark::Glyph { hue: Some(hue), .. } => {
                        (readable(rgb(hue)), theme::bg_panel())
                    }
                    FileMark::Glyph { hue: None, .. } => continue,
                };
                let contrast = theme::contrast(ink, back);
                assert!(
                    contrast >= MIN_CONTRAST - 0.01,
                    "{name} (light {light}): {contrast:.2}"
                );
            }
        }
        crate::theme::set_light(was);
    }
}
