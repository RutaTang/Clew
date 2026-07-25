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

use crate::theme::{self, rgb};

/// Color for a style index, `None` for default foreground. Reads the active
/// theme so the same token gets its One Dark or One Light color.
pub fn style_color(idx: u8) -> Option<Color> {
    static DARK: LazyLock<Vec<Option<Color>>> = LazyLock::new(|| build_table(&DARK_TOKENS));
    static LIGHT: LazyLock<Vec<Option<Color>>> = LazyLock::new(|| build_table(&LIGHT_TOKENS));
    let table = if theme::is_light() { &*LIGHT } else { &*DARK };
    table.get(idx as usize).copied().flatten()
}

fn build_table(tokens: &TokenColors) -> Vec<Option<Color>> {
    HIGHLIGHT_NAMES
        .iter()
        .map(|name| capture_color(name, tokens))
        .collect()
}

/// One color per token category. Paired dark/light instances keep the two
/// schemes in lockstep — add a category here and both themes must fill it.
struct TokenColors {
    comment: u32,
    keyword: u32,
    escape: u32,
    string: u32,
    number: u32,
    function: u32,
    type_: u32,
    property: u32,
    parameter: u32,
    operator: u32,
    punctuation: u32,
}

/// One Dark — the original token colors.
const DARK_TOKENS: TokenColors = TokenColors {
    comment: 0x5c6370,
    keyword: 0xc678dd,
    escape: 0x56b6c2,
    string: 0x98c379,
    number: 0xd19a66,
    function: 0x61afef,
    type_: 0xe5c07b,
    property: 0xe06c75,
    parameter: 0xd19a66,
    operator: 0x56b6c2,
    punctuation: 0x848b98,
};

/// One Light — the paired light token colors (Atom One Light).
const LIGHT_TOKENS: TokenColors = TokenColors {
    comment: 0xa0a1a7,
    keyword: 0xa626a4,
    escape: 0x0184bc,
    string: 0x50a14f,
    number: 0x986801,
    function: 0x4078f2,
    type_: 0xc18401,
    property: 0xe45649,
    parameter: 0x986801,
    operator: 0x0184bc,
    punctuation: 0x6a6f7a,
};

fn capture_color(name: &str, c: &TokenColors) -> Option<Color> {
    let hex = if name.starts_with("comment") {
        c.comment
    } else if name.starts_with("keyword") {
        c.keyword
    } else if name.starts_with("string.escape") || name.starts_with("escape") {
        c.escape
    } else if name.starts_with("string") {
        c.string
    } else if name.starts_with("number") || name.starts_with("constant") {
        c.number
    } else if name.starts_with("function") || name == "constructor" {
        c.function
    } else if name.starts_with("type") || name == "module" || name == "label" {
        c.type_
    } else if name.starts_with("property")
        || name.starts_with("attribute")
        || name.starts_with("variable.builtin")
        || name == "tag"
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
