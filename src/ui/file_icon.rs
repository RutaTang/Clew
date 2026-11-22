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
//!
//! Every mark fills the same share of its square, centred on what is drawn
//! of it: a tile's letters are cut from the code font's outlines and set
//! on their own ink (a line of text sat them a point high on the tile),
//! and each glyph is framed to its own drawing (a lock drawn small in its
//! grid read smaller than a gear drawn large), its stroke kept as wide.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use iced::widget::{container, svg};
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
const TEXT: FileMark = mark(Glyph::TextFile, None);

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

/// The share of the icon's square a tile fills.
const TILE_SHARE: f32 = 6.0 / 7.0;

/// The share a glyph's drawing fills at its longest, stroke and all: less
/// than a tile's, as a dark outline out to its edge reads larger than a
/// pale tile of the same size.
const GLYPH_SHARE: f32 = 0.74;

/// The icon of the file named `name`, `size` square; `plain` colours a
/// mark with no hue of its own (as the row's other icons are).
pub(crate) fn file_icon<'a>(name: &str, size: f32, plain: Color) -> Element<'a, Message> {
    let body: Element<'a, Message> = match mark_of(name) {
        FileMark::Glyph { glyph, hue } => {
            framed(glyph, hue.map_or(plain, |h| readable(rgb(h))), size)
        }
        FileMark::Tile { letters, hue } => {
            tile_icon(letters, hue, size).unwrap_or_else(|| framed(Glyph::File, plain, size))
        }
    };
    container(body).center(Length::Fixed(size)).into()
}

/// `glyph` framed to its own drawing, filling [`GLYPH_SHARE`] of `size`.
fn framed<'a>(glyph: Glyph, color: Color, size: f32) -> Element<'a, Message> {
    let [x0, y0, x1, y1] = drawn(glyph);
    // The drawing's longest side, with the stroke round it, fills the
    // share: the frame is the drawing's centre, out to where that falls.
    let geometry = GLYPH_SHARE - glyph::STROKE / 24.0;
    let side = (x1 - x0).max(y1 - y0) / geometry;
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    glyph::icon_framed(glyph, color, size, [cx - side / 2.0, cy - side / 2.0, side])
}

/// Where a mark's glyph is drawn in its 24-unit grid — left, top, right,
/// bottom of its lines, without their stroke.
fn drawn(glyph: Glyph) -> [f32; 4] {
    match glyph {
        Glyph::Lock => [5.0, 4.5, 19.0, 20.0],
        Glyph::Branch => [3.5, 2.5, 20.5, 21.5],
        Glyph::Settings => [2.95, 2.0, 21.05, 22.0],
        Glyph::Braces => [3.0, 3.0, 21.0, 21.0],
        Glyph::Cube => [4.5, 3.8, 19.5, 20.2],
        Glyph::OpenBook => [2.0, 3.0, 22.0, 21.0],
        Glyph::Image => [3.0, 4.0, 21.0, 20.0],
        Glyph::File | Glyph::TextFile => [4.0, 2.0, 20.0, 22.0],
        _ => [0.0, 0.0, 24.0, 24.0],
    }
}

/// A language's `letters` on a tile of its colour, `size` square: the
/// letters' outlines set on the middle of their own ink. `None` if the
/// code font has none to give.
fn tile_icon<'a>(letters: &'static str, hue: u32, size: f32) -> Option<Element<'a, Message>> {
    let cut = outlines(letters)?;
    let alpha = if theme::is_light() { 0.14 } else { 0.24 };
    // Read on the tile, the wash over the panel, not on the panel.
    let ink = readable_on(rgb(hue), theme::mix(theme::bg_panel(), rgb(hue), alpha));
    // Whole pixels, for edges as sharp as the glyphs' beside it.
    let side = (size * TILE_SHARE).round();
    // Fewer letters, larger: one fills the tile as two do; none wider
    // than the tile leaves room for.
    let em = side
        * match letters.chars().count() {
            1 => 0.72,
            2 => 0.6,
            _ => 0.48,
        };
    let scale = (em / cut.em).min(side * 0.8 / cut.width);
    let (x, y) = (
        (side - cut.width * scale) / 2.0,
        (side - cut.height * scale) / 2.0,
    );
    // The letters alone in the image, opaque; the tile is the box's own
    // fill, which blends with whatever row it is on (the image's renderer
    // greys a translucent fill).
    let doc = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {side} {side}'>\
         <path transform='translate({x} {y}) scale({scale})' d='{d}' fill='{ink}'/></svg>",
        d = cut.path,
        ink = hex(ink),
    );
    let wash = Color {
        a: alpha,
        ..rgb(hue)
    };
    Some(
        container(
            svg(svg::Handle::from_memory(doc.into_bytes()))
                .width(side)
                .height(side),
        )
        .width(side)
        .height(side)
        .style(move |_| container::Style {
            background: Some(wash.into()),
            border: iced::Border {
                radius: (side * 0.22).into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into(),
    )
}

/// A label's letters as one outline, in the code font's units: its path
/// set with its ink's top left at the origin, y down.
struct Cut {
    path: String,
    width: f32,
    height: f32,
    /// The font's em, in the same units.
    em: f32,
}

/// Each label's cut, or that the font had none to give.
type Cuts = Mutex<HashMap<&'static str, Option<Arc<Cut>>>>;

/// `letters` cut from JetBrains Mono Bold, once per label.
fn outlines(letters: &'static str) -> Option<Arc<Cut>> {
    static CUTS: OnceLock<Cuts> = OnceLock::new();
    let cuts = CUTS.get_or_init(Default::default);
    let mut cuts = cuts.lock().unwrap_or_else(|e| e.into_inner());
    cuts.entry(letters)
        .or_insert_with(|| cut(letters).map(Arc::new))
        .clone()
}

fn cut(letters: &str) -> Option<Cut> {
    use ab_glyph::{Font as _, OutlineCurve, Point};
    let font =
        ab_glyph::FontRef::try_from_slice(crate::fonts::ALL[crate::fonts::CODE_BOLD_FACE]).ok()?;
    // Each segment of every letter, set along the line, y down.
    let mut curves: Vec<Vec<Point>> = Vec::new();
    let mut pen = 0.0;
    for ch in letters.chars() {
        let id = font.glyph_id(ch);
        if id.0 == 0 {
            return None;
        }
        if let Some(outline) = font.outline(id) {
            let at = |p: Point| Point {
                x: p.x + pen,
                y: -p.y,
            };
            for curve in outline.curves {
                curves.push(match curve {
                    OutlineCurve::Line(a, b) => vec![at(a), at(b)],
                    OutlineCurve::Quad(a, b, c) => vec![at(a), at(b), at(c)],
                    OutlineCurve::Cubic(a, b, c, d) => vec![at(a), at(b), at(c), at(d)],
                });
            }
        }
        pen += font.h_advance_unscaled(id);
    }
    // The ink's bounds: each curve walked, not its control points, which
    // stand off a round letter's edge.
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for c in &curves {
        for step in 0..=16 {
            let p = point_on(c, step as f32 / 16.0);
            (x0, y0, x1, y1) = (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y));
        }
    }
    if curves.is_empty() || x1 <= x0 || y1 <= y0 {
        return None;
    }
    let mut path = String::new();
    let mut last: Option<Point> = None;
    let f = |p: &Point| format!("{:.1} {:.1}", p.x - x0, p.y - y0);
    for c in &curves {
        if last.is_none_or(|l| (l.x - c[0].x).abs() > 0.01 || (l.y - c[0].y).abs() > 0.01) {
            if last.is_some() {
                path.push('Z');
            }
            path.push_str(&format!("M{}", f(&c[0])));
        }
        match c.len() {
            2 => path.push_str(&format!("L{}", f(&c[1]))),
            3 => path.push_str(&format!("Q{} {}", f(&c[1]), f(&c[2]))),
            _ => path.push_str(&format!("C{} {} {}", f(&c[1]), f(&c[2]), f(&c[3]))),
        }
        last = c.last().copied();
    }
    path.push('Z');
    Some(Cut {
        path,
        width: x1 - x0,
        height: y1 - y0,
        em: font.units_per_em().unwrap_or(1000.0),
    })
}

/// The point at `t` along a line or Bézier curve through `c`.
fn point_on(c: &[ab_glyph::Point], t: f32) -> ab_glyph::Point {
    let mut ps = c.to_vec();
    while ps.len() > 1 {
        ps = ps
            .windows(2)
            .map(|w| ab_glyph::Point {
                x: w[0].x + (w[1].x - w[0].x) * t,
                y: w[0].y + (w[1].y - w[0].y) * t,
            })
            .collect();
    }
    ps[0]
}

/// `#rrggbb` of `c`.
fn hex(c: Color) -> String {
    let [r, g, b, _] = c.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
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
        assert_eq!(glyph("LICENSE"), Some(Glyph::TextFile));
        assert_eq!(glyph("notes.txt"), Some(Glyph::TextFile));
        // Nothing known: the plain page — a dotfile's dot is not an
        // extension, nor is an unknown one.
        assert_eq!(mark_of("data.xyz"), PLAIN);
        assert_eq!(mark_of(".bashrc"), PLAIN);
        assert_eq!(mark_of("rs"), PLAIN);
    }

    /// Every mark is drawn to its frame: each tile's letters cut from the
    /// code font, to fit the tile, and each glyph framed to its drawing.
    #[test]
    fn every_mark_is_drawn_to_its_frame() {
        let exts = [
            "rs", "py", "ipynb", "ts", "js", "go", "c", "h", "cpp", "hpp", "java", "kt", "scala",
            "groovy", "cs", "fs", "php", "rb", "swift", "m", "zig", "lean", "hs", "cabal", "erl",
            "ex", "sql", "lua", "dart", "r", "jl", "ml", "clj", "elm", "nim", "v", "pl", "vue",
            "svelte", "astro", "sh", "ps1", "bat", "html", "xml", "css", "scss", "less", "md",
            "rst", "txt", "json", "toml", "lock", "proto", "graphql", "wasm", "csv", "pdf", "png",
        ];
        let names = [".gitignore", "Dockerfile", "README", "Makefile", "data.xyz"];
        let all = exts
            .iter()
            .map(|e| format!("a.{e}"))
            .chain(names.iter().map(|n| n.to_string()));
        for name in all {
            match mark_of(&name) {
                FileMark::Tile { letters, .. } => {
                    let cut = outlines(letters).unwrap_or_else(|| panic!("{name}: {letters} cut"));
                    assert!(cut.width > 0.0 && cut.height > 0.0, "{name}");
                    assert!(
                        cut.path.starts_with('M') && cut.path.ends_with('Z'),
                        "{name}"
                    );
                }
                FileMark::Glyph { glyph, .. } => {
                    assert_ne!(
                        drawn(glyph),
                        [0.0, 0.0, 24.0, 24.0],
                        "{name}: {glyph:?} framed"
                    );
                }
            }
        }
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
