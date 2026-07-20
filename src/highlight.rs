//! Client-side syntax highlighting: the theme layer over `clew-core`'s
//! tokenizer.
//!
//! The tokenizer (language registry, `HlLine`, `highlight_lines`, `detect`, …)
//! lives in `clew-core` so the headless server can produce the same
//! `(text, style index)` spans. Turning a style index into an actual color is a
//! GUI concern — it depends on the theme — so it stays here. Everything from the
//! core tokenizer is re-exported, so `crate::highlight::*` keeps resolving.

use std::sync::LazyLock;

use iced::Color;

pub use clew_core::highlight::*;

use crate::theme::rgb;

/// Color for a style index, `None` for default foreground.
pub fn style_color(idx: u8) -> Option<Color> {
    static COLORS: LazyLock<Vec<Option<Color>>> = LazyLock::new(|| {
        HIGHLIGHT_NAMES
            .iter()
            .map(|name| capture_color(name))
            .collect()
    });
    COLORS.get(idx as usize).copied().flatten()
}

fn capture_color(name: &str) -> Option<Color> {
    let color = if name.starts_with("comment") {
        rgb(0x5c6370)
    } else if name.starts_with("keyword") {
        rgb(0xc678dd)
    } else if name.starts_with("string.escape") || name.starts_with("escape") {
        rgb(0x56b6c2)
    } else if name.starts_with("string") {
        rgb(0x98c379)
    } else if name.starts_with("number") || name.starts_with("constant") {
        rgb(0xd19a66)
    } else if name.starts_with("function") || name == "constructor" {
        rgb(0x61afef)
    } else if name.starts_with("type") || name == "module" || name == "label" {
        rgb(0xe5c07b)
    } else if name.starts_with("property")
        || name.starts_with("attribute")
        || name.starts_with("variable.builtin")
        || name == "tag"
    {
        rgb(0xe06c75)
    } else if name.starts_with("variable.parameter") {
        rgb(0xd19a66)
    } else if name.starts_with("operator") || name.starts_with("punctuation.special") {
        rgb(0x56b6c2)
    } else if name.starts_with("punctuation") {
        rgb(0x848b98)
    } else {
        return None;
    };
    Some(color)
}
