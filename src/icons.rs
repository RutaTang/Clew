//! File-type icons for the tree: a Nerd Font (devicons/seti) glyph plus a
//! per-language colour. The icon font is embedded and registered at startup
//! (see `main`), and referenced elsewhere via [`ICON_FONT`].

use iced::{Color, Font};

use crate::theme::rgb;

/// Raw bytes of the embedded icon font (Symbols Nerd Font Mono, OFL/MIT).
pub const FONT_BYTES: &[u8] = include_bytes!("../assets/SymbolsNerdFontMono-Regular.ttf");

/// The icon font family, for rendering the glyphs below.
pub const ICON_FONT: Font = Font::with_name("Symbols Nerd Font Mono");

/// A closed / open folder glyph and its colour.
pub fn folder_icon(open: bool) -> (char, Color) {
    let color = rgb(0x7c8598);
    if open { ('\u{f07c}', color) } else { ('\u{f07b}', color) }
}

/// The icon (glyph + colour) for a file, chosen by well-known filename first,
/// then by extension. Falls back to a neutral document glyph.
pub fn file_icon(name: &str) -> (char, Color) {
    let lower = name.to_ascii_lowercase();

    // Whole-name matches for the files that deserve a distinctive look.
    match lower.as_str() {
        "cargo.toml" | "cargo.lock" => return ('\u{e7a8}', rgb(0xdea584)),
        "package.json" | "package-lock.json" => return ('\u{e718}', rgb(0x8cc84b)),
        ".gitignore" | ".gitattributes" | ".gitmodules" => return ('\u{e702}', rgb(0xf14e32)),
        "dockerfile" => return ('\u{f308}', rgb(0x2496ed)),
        "makefile" => return ('\u{e673}', rgb(0x9aa5b5)),
        "readme.md" | "readme" => return ('\u{e73e}', rgb(0x9db4d0)),
        "license" | "license.md" | "license.txt" => return ('\u{f0219}', rgb(0x9aa5b5)),
        _ => {}
    }

    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => ('\u{e7a8}', rgb(0xdea584)),
        "py" | "pyi" => ('\u{e606}', rgb(0x5b9bd5)),
        "js" | "mjs" | "cjs" => ('\u{e74e}', rgb(0xe5c07b)),
        "ts" => ('\u{e628}', rgb(0x519aba)),
        "tsx" | "jsx" => ('\u{e7ba}', rgb(0x61dafb)),
        "go" => ('\u{e627}', rgb(0x4fc3dc)),
        "dart" => ('\u{e798}', rgb(0x40a0d8)),
        "c" | "h" => ('\u{e61e}', rgb(0x6ea8e0)),
        "cpp" | "cc" | "cxx" | "hpp" => ('\u{e61d}', rgb(0xe06c9a)),
        "rb" => ('\u{e739}', rgb(0xd06a58)),
        "java" => ('\u{e738}', rgb(0xe0784b)),
        "kt" | "kts" => ('\u{e634}', rgb(0xb07be0)),
        "swift" => ('\u{e755}', rgb(0xf0803c)),
        "html" | "htm" => ('\u{e736}', rgb(0xe06c4b)),
        "css" => ('\u{e749}', rgb(0x61afef)),
        "scss" | "sass" => ('\u{e749}', rgb(0xcf6f9c)),
        "json" | "jsonc" => ('\u{e60b}', rgb(0xe5c07b)),
        "toml" => ('\u{e6b2}', rgb(0x9aa5b5)),
        "yaml" | "yml" => ('\u{e6a8}', rgb(0x9aa5b5)),
        "md" | "markdown" => ('\u{e73e}', rgb(0x9db4d0)),
        "sh" | "bash" | "zsh" | "fish" => ('\u{e795}', rgb(0x98c379)),
        "lock" => ('\u{f023}', rgb(0x8a94a6)),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" => ('\u{f1c5}', rgb(0xb07be0)),
        "svg" => ('\u{f1c5}', rgb(0xe0a03c)),
        "pdf" => ('\u{f1c1}', rgb(0xe0574b)),
        "txt" | "log" => ('\u{f15c}', rgb(0x8a94a6)),
        _ => ('\u{f15b}', rgb(0x6b7280)),
    }
}
