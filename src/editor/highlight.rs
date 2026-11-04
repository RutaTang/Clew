//! Client-side syntax highlighting: the theme layer over `clew-core`'s
//! tokenizer.
//!
//! The tokenizer (language registry, `HlLine`, `highlight_lines`, `detect`, …)
//! lives in `clew-core` so the headless server can produce the same
//! `(text, style index)` spans. Turning a style index into an actual color is a
//! GUI concern — it depends on the theme — so it stays here. Everything from the
//! core tokenizer is re-exported, so `crate::highlight::*` keeps resolving.
//!
//! Two token palettes are kept — One Dark and One Light — and `style_color`
//! returns from whichever the active theme selects, so highlighting follows the
//! light/dark switch just like the chrome does.

use std::sync::LazyLock;

use iced::Color;

pub use clew_core::highlight::*;

use crate::theme::{self, TokenColors, rgb};

/// Color for a style index, `None` for default foreground. Reads the active
/// theme so the same token takes that theme's token color. One precomputed
/// table per theme (built once), indexed by the active theme.
pub fn style_color(idx: u8) -> Option<Color> {
    static TABLES: LazyLock<Vec<Vec<Option<Color>>>> = LazyLock::new(|| {
        theme::THEMES
            .iter()
            .map(|t| build_table(&t.tokens))
            .collect()
    });
    TABLES
        .get(theme::active_index())
        .and_then(|table| table.get(idx as usize).copied().flatten())
}

/// The code face a style index is set in, where it is not the regular one:
/// a function where it is defined reads in bold, as the design sets it.
pub fn style_font(idx: u8) -> Option<iced::Font> {
    static DEFINITION: LazyLock<Option<u8>> = LazyLock::new(|| {
        HIGHLIGHT_NAMES
            .iter()
            .position(|n| *n == "function.definition")
            .and_then(|i| u8::try_from(i).ok())
    });
    (Some(idx) == *DEFINITION).then_some(crate::fonts::CODE_BOLD)
}

fn build_table(tokens: &TokenColors) -> Vec<Option<Color>> {
    HIGHLIGHT_NAMES
        .iter()
        .map(|name| capture_color(name, tokens))
        .collect()
}

fn capture_color(name: &str, c: &TokenColors) -> Option<Color> {
    let hex = if name == "function.definition" {
        c.definition.unwrap_or(c.function)
    } else if name == "function.macro" {
        c.macro_.unwrap_or(c.function)
    } else if name == "module" {
        c.module.unwrap_or(c.type_)
    } else if name.starts_with("attribute") {
        c.attribute.unwrap_or(c.property)
    } else if name.starts_with("comment") {
        c.comment
    } else if name.starts_with("keyword") {
        c.keyword
    } else if name.starts_with("string.escape") || name.starts_with("escape") {
        c.escape
    } else if name.starts_with("string") {
        c.string
    } else if name.starts_with("number") || name.starts_with("constant") {
        c.number
    } else if name.starts_with("function") {
        c.function
    } else if name.starts_with("type") || name == "constructor" || name == "label" {
        c.type_
    } else if name.starts_with("property") || name.starts_with("variable.builtin") || name == "tag"
    {
        c.property
    } else if name.starts_with("variable.parameter") {
        c.parameter
    } else if name.starts_with("operator") || name.starts_with("punctuation.special") {
        c.operator
    } else if name.starts_with("punctuation") {
        c.punctuation
    } else {
        return None;
    };
    Some(rgb(hex))
}
