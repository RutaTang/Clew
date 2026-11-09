//! Color palettes (Clew Dark / Clew Light by default) and reusable widget
//! styles.
//!
//! Every color the chrome draws with resolves at runtime from the [`active`]
//! palette, chosen by a global mode flag ([`set_light`]). The flag is app-wide
//! and read on the UI thread, so a plain atomic (no lock, no unsafe) is enough:
//! flip it and request a redraw and every window repaints in the new theme.
//! Colors are exposed as accessor functions (`theme::bg()`, `theme::fg()`, …)
//! rather than consts precisely so they can follow that flag.

use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use iced::widget::{button, checkbox, container, pick_list, progress_bar, scrollable, text_input};
use iced::{Border, Color, Theme};

/// Build a `Color` from a `0xRRGGBB` literal.
pub const fn rgb(hex: u32) -> Color {
    Color {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

pub const fn with_alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

/// `Color` → `#rrggbb` (for embedding in SVG that resvg will rasterize).
pub fn hex(c: Color) -> String {
    let u = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", u(c.r), u(c.g), u(c.b))
}

/// The full set of semantic UI colors for one theme. One per entry in
/// [`THEMES`]; [`active`] returns whichever the mode + selection resolve to.
pub struct Palette {
    pub bg: Color,        // editor background
    pub bg_panel: Color,  // sidebar / toolbar background
    pub bg_hover: Color,  // subtle hover wash
    pub selected: Color,  // selected list row (accent-tinted)
    pub fg: Color,        // primary text
    pub fg_bright: Color, // emphasised / selected text
    pub fg_muted: Color,  // secondary text, section labels
    pub dim: Color,       // tertiary / disabled
    pub accent: Color,    // links, active accent
    pub warn: Color,      // errors / failure counts
    pub border: Color,    // hard borders around elevated surfaces
    pub hairline: Color,  // 1px dividers
    // Colors used only inside the widget-style helpers below.
    pub elevated: Color,     // floating modal panel background
    pub on_accent: Color,    // text on an accent-filled button
    pub accent_hover: Color, // accent button, hovered/pressed
    pub scrollbar: Color,    // overlay scrollbar thumb
    // Semantic status colors mirrored into the iced palette.
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    // Editor accents.
    pub info: Color,      // hint / info (cyan)
    pub find: Color,      // search-match highlight base
    pub selection: Color, // character selection background
}

/// One color per broad syntax category; the highlighter maps tree-sitter capture
/// names onto these. Paired per theme so the palette and syntax stay in step.
pub struct TokenColors {
    pub comment: u32,
    pub keyword: u32,
    pub escape: u32,
    pub string: u32,
    pub number: u32,
    pub function: u32,
    pub type_: u32,
    pub property: u32,
    pub parameter: u32,
    pub operator: u32,
    pub punctuation: u32,
    /// Code that is none of the above; `None` is the interface's text colour.
    pub plain: Option<u32>,
    /// A function where it is defined; `None` is [`Self::function`].
    pub definition: Option<u32>,
    /// The module path before a name (`std::path::`); `None` is
    /// [`Self::type_`].
    pub module: Option<u32>,
    /// An attribute, whole; `None` is [`Self::property`].
    pub attribute: Option<u32>,
    /// A macro; `None` is [`Self::function`].
    pub macro_: Option<u32>,
}

/// A complete theme: its chrome [`Palette`] plus its syntax [`TokenColors`],
/// tagged light or dark. Every field is const, so the whole [`THEMES`] table is
/// a compile-time constant that [`active`] indexes into.
pub struct ThemeDef {
    pub id: &'static str,   // stable id used in config.toml
    pub name: &'static str, // display name
    pub is_light: bool,
    pub palette: Palette,
    pub tokens: TokenColors,
}

// ---- Clew Dark / Clew Light (the defaults) --------------------------------
//
// Clew's own look: neutral greys that stay out of the code's way, one
// restrained indigo that marks only what is current (the caret's line, the
// focused field, links), and a syntax palette tuned to read for hours.

const CLEW_DARK_PAL: Palette = Palette {
    bg: rgb(0x18181b),
    bg_panel: rgb(0x121214),
    bg_hover: rgb(0x222226),
    selected: rgb(0x26262b),
    fg: rgb(0xededef),
    fg_bright: rgb(0xfafafa),
    fg_muted: rgb(0xa1a1aa),
    dim: rgb(0x84848c),
    accent: rgb(0x8e8cf6),
    warn: rgb(0xf87171),
    border: rgb(0x2e2e33),
    hairline: rgb(0x242428),
    elevated: rgb(0x1c1c1f),
    on_accent: rgb(0x0b0b0f),
    accent_hover: rgb(0xa5a3f8),
    scrollbar: rgb(0x8e8e96),
    success: rgb(0x4ade80),
    warning: rgb(0xfbbf24),
    danger: rgb(0xf87171),
    info: rgb(0x7dd3fc),
    find: rgb(0xfbbf24),
    selection: rgb(0x33335a),
};
// The design's palette: what is not a keyword, a name defined or called, a
// type, a string, a module path, an attribute or a comment is plain.
const CLEW_DARK_TOK: TokenColors = TokenColors {
    comment: 0x8a8f98,
    keyword: 0xe08ab7,
    escape: 0x7fd1c7,
    string: 0xb5d98a,
    number: 0xf5a97f,
    function: 0x93b4ff,
    type_: 0xf2c27d,
    property: 0x7fd1c7,
    parameter: 0xd4d4d8,
    operator: 0xd4d4d8,
    punctuation: 0xd4d4d8,
    plain: Some(0xd4d4d8),
    definition: Some(0x7fd1c7),
    module: Some(0x8b8b94),
    attribute: Some(0xc8a2f0),
    macro_: Some(0xc8a2f0),
};

const CLEW_LIGHT_PAL: Palette = Palette {
    bg: rgb(0xffffff),
    bg_panel: rgb(0xf4f4f5),
    bg_hover: rgb(0xebebee),
    selected: rgb(0xe6e6ea),
    fg: rgb(0x18181b),
    fg_bright: rgb(0x09090b),
    fg_muted: rgb(0x52525b),
    dim: rgb(0x71717a),
    accent: rgb(0x5b58d9),
    warn: rgb(0xdc2626),
    border: rgb(0xd4d4d8),
    hairline: rgb(0xe4e4e7),
    elevated: rgb(0xffffff),
    on_accent: rgb(0xffffff),
    accent_hover: rgb(0x4b48c9),
    scrollbar: rgb(0xa1a1aa),
    success: rgb(0x117f3a),
    warning: rgb(0xb45309),
    danger: rgb(0xd52222),
    info: rgb(0x0273ae),
    find: rgb(0xf59e0b),
    selection: rgb(0xdcdbfb),
};
const CLEW_LIGHT_TOK: TokenColors = TokenColors {
    comment: 0x6b7078,
    keyword: 0xb4246e,
    escape: 0x0f766e,
    string: 0x3f7d1c,
    number: 0xb45309,
    function: 0x2b50c0,
    type_: 0x9a5b00,
    property: 0x0f766e,
    parameter: 0x27272a,
    operator: 0x27272a,
    punctuation: 0x27272a,
    plain: Some(0x27272a),
    definition: Some(0x0f766e),
    module: Some(0x71717a),
    attribute: Some(0x7c3aad),
    macro_: Some(0x7c3aad),
};

// ---- One Dark / One Light -------------------------------------------------

const ONE_DARK_PAL: Palette = Palette {
    bg: rgb(0x282c34),
    bg_panel: rgb(0x21252b),
    bg_hover: rgb(0x2d323c),
    selected: rgb(0x2f3a4c),
    fg: rgb(0xabb2bf),
    fg_bright: rgb(0xdfe4ec),
    fg_muted: rgb(0x828b9c),
    dim: rgb(0x5c6370),
    accent: rgb(0x61afef),
    warn: rgb(0xe06c75),
    border: rgb(0x181a1f),
    hairline: rgb(0x30353f),
    elevated: rgb(0x2c313a),
    on_accent: rgb(0x1b1d23),
    accent_hover: rgb(0x539bd4),
    scrollbar: rgb(0x8a94a6),
    success: rgb(0x98c379),
    warning: rgb(0xe5c07b),
    danger: rgb(0xe2747d),
    info: rgb(0x56b6c2),
    find: rgb(0xe5c07b),
    selection: rgb(0x2d3a55),
};
const ONE_DARK_TOK: TokenColors = TokenColors {
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
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const ONE_LIGHT_PAL: Palette = Palette {
    bg: rgb(0xfafafa),
    bg_panel: rgb(0xeff1f3),
    bg_hover: rgb(0xe4e7ea),
    selected: rgb(0xd3e3fb),
    fg: rgb(0x383a42),
    fg_bright: rgb(0x1b1d23),
    fg_muted: rgb(0x6b7280),
    dim: rgb(0x9ca3af),
    accent: rgb(0x1f60f0),
    warn: rgb(0xd6372c),
    border: rgb(0xcfd3d9),
    hairline: rgb(0xe6e8ec),
    elevated: rgb(0xffffff),
    on_accent: rgb(0xffffff),
    accent_hover: rgb(0x3467d6),
    scrollbar: rgb(0x9096a1),
    success: rgb(0x3c783b),
    warning: rgb(0x985f00),
    danger: rgb(0xcc2d1e),
    info: rgb(0x0172a3),
    find: rgb(0xf5b400),
    selection: rgb(0xbcd8fb),
};
const ONE_LIGHT_TOK: TokenColors = TokenColors {
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
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

// ---- Gruvbox Soft ---------------------------------------------------------

const GRUVBOX_DARK_PAL: Palette = Palette {
    bg: rgb(0x32302f),
    bg_panel: rgb(0x282828),
    bg_hover: rgb(0x3c3836),
    selected: rgb(0x45403d),
    fg: rgb(0xebdbb2),
    fg_bright: rgb(0xfbf1c7),
    fg_muted: rgb(0xa89984),
    dim: rgb(0x7c6f64),
    accent: rgb(0x83a598),
    warn: rgb(0xfb4934),
    border: rgb(0x1d2021),
    hairline: rgb(0x3c3836),
    elevated: rgb(0x3c3836),
    on_accent: rgb(0x1d2021),
    accent_hover: rgb(0x9fbdb0),
    scrollbar: rgb(0x7c6f64),
    success: rgb(0xb8bb26),
    warning: rgb(0xfabd2f),
    danger: rgb(0xfc6b5a),
    info: rgb(0x8ec07c),
    find: rgb(0xfabd2f),
    selection: rgb(0x504945),
};
const GRUVBOX_DARK_TOK: TokenColors = TokenColors {
    comment: 0x928374,
    keyword: 0xfb4934,
    escape: 0xfe8019,
    string: 0xb8bb26,
    number: 0xd3869b,
    function: 0x8ec07c,
    type_: 0xfabd2f,
    property: 0x83a598,
    parameter: 0xfe8019,
    operator: 0xfe8019,
    punctuation: 0xa89984,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const GRUVBOX_LIGHT_PAL: Palette = Palette {
    bg: rgb(0xf2e5bc),
    bg_panel: rgb(0xebdbb2),
    bg_hover: rgb(0xe8d9a8),
    selected: rgb(0xdcc9a0),
    fg: rgb(0x3c3836),
    fg_bright: rgb(0x282828),
    fg_muted: rgb(0x7c6f64),
    dim: rgb(0xa89984),
    accent: rgb(0x076678),
    warn: rgb(0x9d0006),
    border: rgb(0xd5c4a1),
    hairline: rgb(0xe0d0a4),
    elevated: rgb(0xfbf1c7),
    on_accent: rgb(0xfbf1c7),
    accent_hover: rgb(0x05505f),
    scrollbar: rgb(0xa89984),
    success: rgb(0x64600c),
    warning: rgb(0x82550e),
    danger: rgb(0x9d0006),
    info: rgb(0x38694b),
    find: rgb(0xd79921),
    selection: rgb(0xd5c4a1),
};
const GRUVBOX_LIGHT_TOK: TokenColors = TokenColors {
    comment: 0x928374,
    keyword: 0x9d0006,
    escape: 0xaf3a03,
    string: 0x79740e,
    number: 0x8f3f71,
    function: 0x427b58,
    type_: 0xb57614,
    property: 0x076678,
    parameter: 0xaf3a03,
    operator: 0xaf3a03,
    punctuation: 0x7c6f64,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

// ---- Paper ----------------------------------------------------------------
// A print aesthetic: warm paper grounds, sepia ink text, and desaturated
// "colored-ink" syntax — low contrast and warm, for reading code like a page.

const PAPER_LIGHT_PAL: Palette = Palette {
    bg: rgb(0xf4ecd8),
    bg_panel: rgb(0xece3cc),
    bg_hover: rgb(0xe7ddc4),
    selected: rgb(0xe4d7ad),
    fg: rgb(0x4a4436),
    fg_bright: rgb(0x2e2a20),
    fg_muted: rgb(0x857a60),
    dim: rgb(0xa99f86),
    accent: rgb(0x37669a),
    warn: rgb(0xa03e3e),
    border: rgb(0xd8ccae),
    hairline: rgb(0xe4dabf),
    elevated: rgb(0xfaf4e2),
    on_accent: rgb(0xf4ecd8),
    accent_hover: rgb(0x305d8c),
    scrollbar: rgb(0xb3a988),
    success: rgb(0x4c6b3c),
    warning: rgb(0x815c25),
    danger: rgb(0xa03e3e),
    info: rgb(0x416c63),
    find: rgb(0xd9a441),
    selection: rgb(0xe4d7ad),
};
const PAPER_LIGHT_TOK: TokenColors = TokenColors {
    comment: 0x9a8f75,
    keyword: 0xa03e3e,
    escape: 0x4a7a70,
    string: 0x4c6b3c,
    number: 0xa5762f,
    function: 0x3b6ea5,
    type_: 0x8a6d3b,
    property: 0x9d5b3c,
    parameter: 0xa5762f,
    operator: 0x4a7a70,
    punctuation: 0x857a60,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const PAPER_DARK_PAL: Palette = Palette {
    bg: rgb(0x262019),
    bg_panel: rgb(0x1f1a14),
    bg_hover: rgb(0x302921),
    selected: rgb(0x3a3025),
    fg: rgb(0xd9cba8),
    fg_bright: rgb(0xf0e6cb),
    fg_muted: rgb(0x9d8f70),
    dim: rgb(0x766a54),
    accent: rgb(0x7fa0b0),
    warn: rgb(0xcf6b5c),
    border: rgb(0x17130e),
    hairline: rgb(0x322a20),
    elevated: rgb(0x302921),
    on_accent: rgb(0x1f1a14),
    accent_hover: rgb(0x96b4c2),
    scrollbar: rgb(0x6a5f4a),
    success: rgb(0xa3b072),
    warning: rgb(0xd9a441),
    danger: rgb(0xcf6b5c),
    info: rgb(0x83a89a),
    find: rgb(0xd9a441),
    selection: rgb(0x3b3228),
};
const PAPER_DARK_TOK: TokenColors = TokenColors {
    comment: 0x8a7d60,
    keyword: 0xcf6b5c,
    escape: 0x83a89a,
    string: 0xa3b072,
    number: 0xd9a441,
    function: 0x7fa0b0,
    type_: 0xd0a860,
    property: 0xcf8a5c,
    parameter: 0xd9a441,
    operator: 0x83a89a,
    punctuation: 0x9d8f70,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

// ---- Cyberpunk ------------------------------------------------------------
// Neon on a deep midnight-purple void: hot magenta, electric cyan, acid yellow.
// Dark only — a light "neon" scheme would defeat the glow.

const CYBERPUNK_PAL: Palette = Palette {
    bg: rgb(0x0f0b1e),
    bg_panel: rgb(0x0a0816),
    bg_hover: rgb(0x1a1530),
    selected: rgb(0x2a1f47),
    fg: rgb(0xe5e0ff),
    fg_bright: rgb(0xffffff),
    fg_muted: rgb(0x8b84b8),
    dim: rgb(0x565080),
    accent: rgb(0x00f0ff),
    warn: rgb(0xff2e6c),
    border: rgb(0x05030f),
    hairline: rgb(0x1c1633),
    elevated: rgb(0x16112b),
    on_accent: rgb(0x0a0816),
    accent_hover: rgb(0x5cf7ff),
    scrollbar: rgb(0x4a4080),
    success: rgb(0x00ff9f),
    warning: rgb(0xfcee0a),
    danger: rgb(0xff2e6c),
    info: rgb(0x00f0ff),
    find: rgb(0xfcee0a),
    selection: rgb(0x2a1f47),
};
const CYBERPUNK_TOK: TokenColors = TokenColors {
    comment: 0x565080,
    keyword: 0xff2e97,
    escape: 0x00ffcc,
    string: 0x5cff9d,
    number: 0xb47dff,
    function: 0x00f0ff,
    type_: 0xfcee0a,
    property: 0xff9f43,
    parameter: 0xb47dff,
    operator: 0x00ffcc,
    punctuation: 0x8b84b8,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

// ---- Nord, Dracula, Tokyo Night, Catppuccin, Rosé Pine, Solarized ---------
//
// Well-loved open palettes, each with its own syntax colours. Where a
// palette's status colours (success, warning, danger, info) or accent read
// below 4.5:1 on its surfaces, they are drawn a little toward the ink — the
// rest is the palette as its authors made it.

const NORD_PAL: Palette = Palette {
    bg: rgb(0x2e3440),
    bg_panel: rgb(0x272c36),
    bg_hover: rgb(0x353c4a),
    selected: rgb(0x3b4252),
    fg: rgb(0xd8dee9),
    fg_bright: rgb(0xeceff4),
    fg_muted: rgb(0xaab3c3),
    dim: rgb(0x8590a6),
    accent: rgb(0x88c0d0),
    warn: rgb(0xd18d94),
    border: rgb(0x3b4252),
    hairline: rgb(0x343a46),
    elevated: rgb(0x323846),
    on_accent: rgb(0x2e3440),
    accent_hover: rgb(0x8fbcbb),
    scrollbar: rgb(0x4c566a),
    success: rgb(0xa3be8c),
    warning: rgb(0xebcb8b),
    danger: rgb(0xd18d94),
    info: rgb(0x8fbcbb),
    find: rgb(0xebcb8b),
    selection: rgb(0x434c5e),
};
const NORD_TOK: TokenColors = TokenColors {
    comment: 0x6c7891,
    keyword: 0x81a1c1,
    escape: 0xebcb8b,
    string: 0xa3be8c,
    number: 0xb48ead,
    function: 0x88c0d0,
    type_: 0x8fbcbb,
    property: 0xd8dee9,
    parameter: 0xd8dee9,
    operator: 0x81a1c1,
    punctuation: 0x9aa5b8,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const DRACULA_PAL: Palette = Palette {
    bg: rgb(0x282a36),
    bg_panel: rgb(0x21222c),
    bg_hover: rgb(0x313341),
    selected: rgb(0x3a3c4e),
    fg: rgb(0xf8f8f2),
    fg_bright: rgb(0xffffff),
    fg_muted: rgb(0xb9bccc),
    dim: rgb(0x858aa8),
    accent: rgb(0xbd93f9),
    warn: rgb(0xff6e6e),
    border: rgb(0x343746),
    hairline: rgb(0x2f3140),
    elevated: rgb(0x2d2f3d),
    on_accent: rgb(0x282a36),
    accent_hover: rgb(0xcaa8fa),
    scrollbar: rgb(0x44475a),
    success: rgb(0x50fa7b),
    warning: rgb(0xffb86c),
    danger: rgb(0xff6e6e),
    info: rgb(0x8be9fd),
    find: rgb(0xf1fa8c),
    selection: rgb(0x44475a),
};
const DRACULA_TOK: TokenColors = TokenColors {
    comment: 0x6f7cb0,
    keyword: 0xff79c6,
    escape: 0xff79c6,
    string: 0xf1fa8c,
    number: 0xbd93f9,
    function: 0x50fa7b,
    type_: 0x8be9fd,
    property: 0x8be9fd,
    parameter: 0xffb86c,
    operator: 0xff79c6,
    punctuation: 0xd6d6cf,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const TOKYO_NIGHT_PAL: Palette = Palette {
    bg: rgb(0x1a1b26),
    bg_panel: rgb(0x16161e),
    bg_hover: rgb(0x20222e),
    selected: rgb(0x292e42),
    fg: rgb(0xc0caf5),
    fg_bright: rgb(0xe0e6ff),
    fg_muted: rgb(0xa9b1d6),
    dim: rgb(0x737aa2),
    accent: rgb(0x7aa2f7),
    warn: rgb(0xf7768e),
    border: rgb(0x292e42),
    hairline: rgb(0x232433),
    elevated: rgb(0x1f2335),
    on_accent: rgb(0x1a1b26),
    accent_hover: rgb(0x8cb0fa),
    scrollbar: rgb(0x3b4261),
    success: rgb(0x9ece6a),
    warning: rgb(0xe0af68),
    danger: rgb(0xf7768e),
    info: rgb(0x7dcfff),
    find: rgb(0xe0af68),
    selection: rgb(0x283457),
};
const TOKYO_NIGHT_TOK: TokenColors = TokenColors {
    comment: 0x5f6896,
    keyword: 0xbb9af7,
    escape: 0x89ddff,
    string: 0x9ece6a,
    number: 0xff9e64,
    function: 0x7aa2f7,
    type_: 0x2ac3de,
    property: 0x73daca,
    parameter: 0xe0af68,
    operator: 0x89ddff,
    punctuation: 0x9aa5ce,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const CATPPUCCIN_MOCHA_PAL: Palette = Palette {
    bg: rgb(0x1e1e2e),
    bg_panel: rgb(0x181825),
    bg_hover: rgb(0x262637),
    selected: rgb(0x313244),
    fg: rgb(0xcdd6f4),
    fg_bright: rgb(0xeef1fd),
    fg_muted: rgb(0xa6adc8),
    dim: rgb(0x7f849c),
    accent: rgb(0xcba6f7),
    warn: rgb(0xf38ba8),
    border: rgb(0x313244),
    hairline: rgb(0x29293b),
    elevated: rgb(0x24243a),
    on_accent: rgb(0x1e1e2e),
    accent_hover: rgb(0xd6b9f9),
    scrollbar: rgb(0x45475a),
    success: rgb(0xa6e3a1),
    warning: rgb(0xfab387),
    danger: rgb(0xf38ba8),
    info: rgb(0x89dceb),
    find: rgb(0xf9e2af),
    selection: rgb(0x45475a),
};
const CATPPUCCIN_MOCHA_TOK: TokenColors = TokenColors {
    comment: 0x7f849c,
    keyword: 0xcba6f7,
    escape: 0xf5c2e7,
    string: 0xa6e3a1,
    number: 0xfab387,
    function: 0x89b4fa,
    type_: 0xf9e2af,
    property: 0xb4befe,
    parameter: 0xeba0ac,
    operator: 0x89dceb,
    punctuation: 0x9399b2,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const ROSE_PINE_PAL: Palette = Palette {
    bg: rgb(0x191724),
    bg_panel: rgb(0x14121e),
    bg_hover: rgb(0x1f1d2e),
    selected: rgb(0x26233a),
    fg: rgb(0xe0def4),
    fg_bright: rgb(0xf5f3ff),
    fg_muted: rgb(0x908caa),
    dim: rgb(0x6e6a86),
    accent: rgb(0xc4a7e7),
    warn: rgb(0xeb6f92),
    border: rgb(0x26233a),
    hairline: rgb(0x211f30),
    elevated: rgb(0x1f1d2e),
    on_accent: rgb(0x191724),
    accent_hover: rgb(0xd0b9ec),
    scrollbar: rgb(0x403d52),
    success: rgb(0x9ccfd8),
    warning: rgb(0xf6c177),
    danger: rgb(0xeb6f92),
    info: rgb(0x9ccfd8),
    find: rgb(0xf6c177),
    selection: rgb(0x2f2c45),
};
const ROSE_PINE_TOK: TokenColors = TokenColors {
    comment: 0x6e6a86,
    keyword: 0x3e8fb0,
    escape: 0xeb6f92,
    string: 0xf6c177,
    number: 0xebbcba,
    function: 0xebbcba,
    type_: 0x9ccfd8,
    property: 0xc4a7e7,
    parameter: 0xc4a7e7,
    operator: 0x908caa,
    punctuation: 0x908caa,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const SOLARIZED_DARK_PAL: Palette = Palette {
    bg: rgb(0x002b36),
    bg_panel: rgb(0x00222b),
    bg_hover: rgb(0x073642),
    selected: rgb(0x0b3d4a),
    fg: rgb(0xa1adad),
    fg_bright: rgb(0xeee8d5),
    fg_muted: rgb(0x839496),
    dim: rgb(0x657b83),
    accent: rgb(0x3c97d6),
    warn: rgb(0xe56765),
    border: rgb(0x0b3d4a),
    hairline: rgb(0x063340),
    elevated: rgb(0x063541),
    on_accent: rgb(0x002b36),
    accent_hover: rgb(0x3d9be0),
    scrollbar: rgb(0x29505a),
    success: rgb(0x859900),
    warning: rgb(0xb58900),
    danger: rgb(0xe56765),
    info: rgb(0x2aa198),
    find: rgb(0xb58900),
    selection: rgb(0x0e4655),
};
const SOLARIZED_DARK_TOK: TokenColors = TokenColors {
    comment: 0x5f7880,
    keyword: 0x859900,
    escape: 0xcb4b16,
    string: 0x2aa198,
    number: 0xd33682,
    function: 0x268bd2,
    type_: 0xb58900,
    property: 0x6c71c4,
    parameter: 0xcb4b16,
    operator: 0x859900,
    punctuation: 0x839496,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const SOLARIZED_LIGHT_PAL: Palette = Palette {
    bg: rgb(0xfdf6e3),
    bg_panel: rgb(0xf5eedb),
    bg_hover: rgb(0xeee8d5),
    selected: rgb(0xe6dfc8),
    fg: rgb(0x52676e),
    fg_bright: rgb(0x073642),
    fg_muted: rgb(0x657b83),
    dim: rgb(0x93a1a1),
    accent: rgb(0x1e6fa8),
    warn: rgb(0xca2e2b),
    border: rgb(0xe3dcc6),
    hairline: rgb(0xece5cf),
    elevated: rgb(0xfffaee),
    on_accent: rgb(0xfdf6e3),
    accent_hover: rgb(0x1e74b0),
    scrollbar: rgb(0xc7c0a9),
    success: rgb(0x627100),
    warning: rgb(0x866500),
    danger: rgb(0xca2e2b),
    info: rgb(0x1f7770),
    find: rgb(0xe8c547),
    selection: rgb(0xeee8d5),
};
const SOLARIZED_LIGHT_TOK: TokenColors = TokenColors {
    comment: 0x93a1a1,
    keyword: 0x859900,
    escape: 0xcb4b16,
    string: 0x2aa198,
    number: 0xd33682,
    function: 0x268bd2,
    type_: 0xb58900,
    property: 0x6c71c4,
    parameter: 0xcb4b16,
    operator: 0x859900,
    punctuation: 0x657b83,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const CATPPUCCIN_LATTE_PAL: Palette = Palette {
    bg: rgb(0xeff1f5),
    bg_panel: rgb(0xe6e9ef),
    bg_hover: rgb(0xe3e6ed),
    selected: rgb(0xdce0e8),
    fg: rgb(0x4c4f69),
    fg_bright: rgb(0x303446),
    fg_muted: rgb(0x6c6f85),
    dim: rgb(0x9ca0b0),
    accent: rgb(0x8538ea),
    warn: rgb(0xce0f38),
    border: rgb(0xccd0da),
    hairline: rgb(0xdce0e8),
    elevated: rgb(0xf9fafc),
    on_accent: rgb(0xeff1f5),
    accent_hover: rgb(0x7a2ee0),
    scrollbar: rgb(0xacb0be),
    success: rgb(0x2f7620),
    warning: rgb(0x8f5b13),
    danger: rgb(0xce0f38),
    info: rgb(0x036d97),
    find: rgb(0xdf8e1d),
    selection: rgb(0xccd0da),
};
const CATPPUCCIN_LATTE_TOK: TokenColors = TokenColors {
    comment: 0x8c8fa1,
    keyword: 0x8839ef,
    escape: 0xea76cb,
    string: 0x40a02b,
    number: 0xfe640b,
    function: 0x1e66f5,
    type_: 0xdf8e1d,
    property: 0x7287fd,
    parameter: 0xe64553,
    operator: 0x04a5e5,
    punctuation: 0x7c7f93,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

const ROSE_PINE_DAWN_PAL: Palette = Palette {
    bg: rgb(0xfaf4ed),
    bg_panel: rgb(0xf2e9e1),
    bg_hover: rgb(0xf4ede8),
    selected: rgb(0xe8e1dc),
    fg: rgb(0x575279),
    fg_bright: rgb(0x3e3a5c),
    fg_muted: rgb(0x797593),
    dim: rgb(0x9893a5),
    accent: rgb(0x286983),
    warn: rgb(0x975366),
    border: rgb(0xdfdad9),
    hairline: rgb(0xece6e2),
    elevated: rgb(0xfffaf3),
    on_accent: rgb(0xfaf4ed),
    accent_hover: rgb(0x1f5569),
    scrollbar: rgb(0xcecacd),
    success: rgb(0x406e76),
    warning: rgb(0x8c5e1f),
    danger: rgb(0x975366),
    info: rgb(0x286983),
    find: rgb(0xea9d34),
    selection: rgb(0xdfdad9),
};
const ROSE_PINE_DAWN_TOK: TokenColors = TokenColors {
    comment: 0x9893a5,
    keyword: 0x286983,
    escape: 0xb4637a,
    string: 0xea9d34,
    number: 0xd7827e,
    function: 0xd7827e,
    type_: 0x56949f,
    property: 0x907aa9,
    parameter: 0x907aa9,
    operator: 0x797593,
    punctuation: 0x797593,
    plain: None,
    definition: None,
    module: None,
    attribute: None,
    macro_: None,
};

/// Every theme clew ships. Dark and light variants coexist; the user picks one
/// of each (see [`set_light_theme`] / [`set_dark_theme`]) and the mode flag
/// chooses between them. Order is stable — ids, not indices, are persisted.
pub static THEMES: &[ThemeDef] = &[
    ThemeDef {
        id: "clew-dark",
        name: "Clew Dark",
        is_light: false,
        palette: CLEW_DARK_PAL,
        tokens: CLEW_DARK_TOK,
    },
    ThemeDef {
        id: "clew-light",
        name: "Clew Light",
        is_light: true,
        palette: CLEW_LIGHT_PAL,
        tokens: CLEW_LIGHT_TOK,
    },
    ThemeDef {
        id: "one-dark",
        name: "One Dark",
        is_light: false,
        palette: ONE_DARK_PAL,
        tokens: ONE_DARK_TOK,
    },
    ThemeDef {
        id: "one-light",
        name: "One Light",
        is_light: true,
        palette: ONE_LIGHT_PAL,
        tokens: ONE_LIGHT_TOK,
    },
    ThemeDef {
        id: "gruvbox-dark",
        name: "Gruvbox Dark Soft",
        is_light: false,
        palette: GRUVBOX_DARK_PAL,
        tokens: GRUVBOX_DARK_TOK,
    },
    ThemeDef {
        id: "gruvbox-light",
        name: "Gruvbox Light Soft",
        is_light: true,
        palette: GRUVBOX_LIGHT_PAL,
        tokens: GRUVBOX_LIGHT_TOK,
    },
    ThemeDef {
        id: "paper-light",
        name: "Paper",
        is_light: true,
        palette: PAPER_LIGHT_PAL,
        tokens: PAPER_LIGHT_TOK,
    },
    ThemeDef {
        id: "paper-dark",
        name: "Paper Dark",
        is_light: false,
        palette: PAPER_DARK_PAL,
        tokens: PAPER_DARK_TOK,
    },
    ThemeDef {
        id: "cyberpunk",
        name: "Cyberpunk",
        is_light: false,
        palette: CYBERPUNK_PAL,
        tokens: CYBERPUNK_TOK,
    },
    ThemeDef {
        id: "nord",
        name: "Nord",
        is_light: false,
        palette: NORD_PAL,
        tokens: NORD_TOK,
    },
    ThemeDef {
        id: "dracula",
        name: "Dracula",
        is_light: false,
        palette: DRACULA_PAL,
        tokens: DRACULA_TOK,
    },
    ThemeDef {
        id: "tokyo-night",
        name: "Tokyo Night",
        is_light: false,
        palette: TOKYO_NIGHT_PAL,
        tokens: TOKYO_NIGHT_TOK,
    },
    ThemeDef {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        is_light: false,
        palette: CATPPUCCIN_MOCHA_PAL,
        tokens: CATPPUCCIN_MOCHA_TOK,
    },
    ThemeDef {
        id: "rose-pine",
        name: "Rosé Pine",
        is_light: false,
        palette: ROSE_PINE_PAL,
        tokens: ROSE_PINE_TOK,
    },
    ThemeDef {
        id: "solarized-dark",
        name: "Solarized Dark",
        is_light: false,
        palette: SOLARIZED_DARK_PAL,
        tokens: SOLARIZED_DARK_TOK,
    },
    ThemeDef {
        id: "solarized-light",
        name: "Solarized Light",
        is_light: true,
        palette: SOLARIZED_LIGHT_PAL,
        tokens: SOLARIZED_LIGHT_TOK,
    },
    ThemeDef {
        id: "catppuccin-latte",
        name: "Catppuccin Latte",
        is_light: true,
        palette: CATPPUCCIN_LATTE_PAL,
        tokens: CATPPUCCIN_LATTE_TOK,
    },
    ThemeDef {
        id: "rose-pine-dawn",
        name: "Rosé Pine Dawn",
        is_light: true,
        palette: ROSE_PINE_DAWN_PAL,
        tokens: ROSE_PINE_DAWN_TOK,
    },
];

const DEFAULT_DARK: &str = "clew-dark";
const DEFAULT_LIGHT: &str = "clew-light";
/// The defaults before Clew's own themes, and the config key that says a
/// saved selection was made knowing of them (see [`init`]).
const OLD_DEFAULT_DARK: &str = "one-dark";
const OLD_DEFAULT_LIGHT: &str = "one-light";
const THEMES_GENERATION_KEY: &str = "theme_generation";
const THEMES_GENERATION: i64 = 2;

/// 0 = dark, 1 = light. The three-way preference (Dark/Light/System) resolves to
/// this binary flag whenever it is set (see `apply_pref`).
static MODE: AtomicU8 = AtomicU8::new(0);
/// Which THEMES entry is the chosen light / dark variant: Clew's own pair
/// until the user picks otherwise.
static LIGHT_ID: AtomicUsize = AtomicUsize::new(1); // clew-light
static DARK_ID: AtomicUsize = AtomicUsize::new(0); // clew-dark

/// Bumped whenever the resolved appearance changes (mode, or either theme
/// selection). The palette above is process-global while windows are not, so
/// a change made in one window silently re-colored every other one — and left
/// their cached diagram SVGs rendered in the previous palette, because only
/// the acting window re-styled its own. The shell compares this across each
/// window update and tells the others to resync.
static REVISION: AtomicU64 = AtomicU64::new(0);

/// The current value of the appearance revision.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

fn bump_revision() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Have every window draw again — the language changed, which each draws
/// from as it does from the palette.
pub fn redraw_everywhere() {
    bump_revision();
}

/// Whether a light theme is currently active.
pub fn is_light() -> bool {
    MODE.load(Ordering::Relaxed) == 1
}

/// Switch between the light and dark selection. Request a redraw afterwards.
pub fn set_light(light: bool) {
    if MODE.swap(u8::from(light), Ordering::Relaxed) != u8::from(light) {
        bump_revision();
    }
}

/// Index into [`THEMES`] of the theme currently in effect.
pub fn active_index() -> usize {
    let id = if is_light() {
        LIGHT_ID.load(Ordering::Relaxed)
    } else {
        DARK_ID.load(Ordering::Relaxed)
    };
    id.min(THEMES.len() - 1)
}

/// The theme currently in effect.
pub fn active_theme() -> &'static ThemeDef {
    &THEMES[active_index()]
}

/// The palette currently in effect.
pub fn active() -> &'static Palette {
    &active_theme().palette
}

/// The `is_light`-matching THEMES index for an id, if one exists.
fn theme_index(id: &str, is_light: bool) -> Option<usize> {
    THEMES
        .iter()
        .position(|t| t.id == id && t.is_light == is_light)
}

/// Choose the light / dark variant by id (ignored if unknown or wrong polarity).
pub fn set_light_theme(id: &str) {
    if let Some(i) = theme_index(id, true)
        && LIGHT_ID.swap(i, Ordering::Relaxed) != i
    {
        bump_revision();
    }
}
pub fn set_dark_theme(id: &str) {
    if let Some(i) = theme_index(id, false)
        && DARK_ID.swap(i, Ordering::Relaxed) != i
    {
        bump_revision();
    }
}

/// The currently-selected light / dark theme (regardless of which is active).
pub fn current_light() -> &'static ThemeDef {
    &THEMES[LIGHT_ID.load(Ordering::Relaxed).min(THEMES.len() - 1)]
}
pub fn current_dark() -> &'static ThemeDef {
    &THEMES[DARK_ID.load(Ordering::Relaxed).min(THEMES.len() - 1)]
}

/// The user's theme preference, persisted in the shared `config.toml`. `System`
/// tracks the OS appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ThemePref {
    Dark,
    Light,
    System,
}

impl ThemePref {
    pub const ALL: [ThemePref; 3] = [ThemePref::System, ThemePref::Light, ThemePref::Dark];

    pub fn as_str(self) -> &'static str {
        match self {
            ThemePref::Dark => "dark",
            ThemePref::Light => "light",
            ThemePref::System => "system",
        }
    }

    /// Its name in Settings, in the reader's language: only ever shown
    /// ([`ThemePref::as_str`] is what the config keeps).
    pub fn label(self) -> &'static str {
        match self {
            ThemePref::Dark => crate::tr!("Dark"),
            ThemePref::Light => crate::tr!("Light"),
            ThemePref::System => crate::tr!("System"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "system" => Some(Self::System),
            _ => None,
        }
    }

    /// Resolve to whether the light palette should be active.
    pub fn resolve_light(self) -> bool {
        match self {
            ThemePref::Dark => false,
            ThemePref::Light => true,
            ThemePref::System => system_is_light(),
        }
    }
}

/// The last OS appearance read from the platform: 0 = never read, 1 = dark,
/// 2 = light. See [`system_is_light`].
static SYSTEM_APPEARANCE: AtomicU8 = AtomicU8::new(0);

/// Remember the OS appearance just read, for callers that cannot ask (only
/// macOS reads one).
#[cfg(any(target_os = "macos", test))]
fn record_system_appearance(light: bool) {
    SYSTEM_APPEARANCE.store(if light { 2 } else { 1 }, Ordering::Relaxed);
}

/// Whether the OS is currently in light appearance.
///
/// Read in-process from AppKit (`NSApp.effectiveAppearance`, see
/// `macos::appearance::effective_is_light`): no subprocess, so it is cheap
/// enough for the focus-change path that calls it. AppKit only answers on the
/// main thread; anywhere else (worker threads, the test harness) this returns
/// the last main-thread answer, or dark before there has been one.
pub fn system_is_light() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(light) = crate::macos::appearance::effective_is_light() {
            record_system_appearance(light);
            return light;
        }
    }
    SYSTEM_APPEARANCE.load(Ordering::Relaxed) == 2
}

/// The three-way preference in effect. Global for the same reason the palette
/// is: one process paints every window from it, so two windows holding
/// different preferences could only ever disagree about what is on screen.
static PREF: AtomicU8 = AtomicU8::new(ThemePref::System as u8);

/// The appearance preference currently in effect, for a window adopting a
/// change another window made.
pub fn current_pref() -> ThemePref {
    match PREF.load(Ordering::Relaxed) {
        x if x == ThemePref::Dark as u8 => ThemePref::Dark,
        x if x == ThemePref::Light as u8 => ThemePref::Light,
        _ => ThemePref::System,
    }
}

/// Point the active palette at what `pref` resolves to right now.
///
/// The preference is part of the observed state, not just its resolved
/// light/dark outcome. Bumping only when the MODE flipped meant that choosing
/// Dark while the system was already dark fanned nothing out: every other
/// window went on believing the preference was System, and the next system
/// appearance change had one of them call `apply_pref(System)` and overwrite
/// the user's explicit choice.
pub fn apply_pref(pref: ThemePref) {
    if PREF.swap(pref as u8, Ordering::Relaxed) != pref as u8 {
        bump_revision();
    }
    set_light(pref.resolve_light());
}

/// Load persisted appearance settings (mode preference + chosen light/dark
/// themes), apply the theme selections, and return the mode. Called once at
/// startup before the first frame.
pub fn init() -> ThemePref {
    let get = |key: &str| {
        clew_core::globalconfig::get(key)
            .as_ref()
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    // A selection saved before Clew had its own themes names the old
    // defaults because they were the defaults, not because they were
    // chosen: it moves to the new ones. Saved since (the generation key),
    // a selection is the user's, whatever it names.
    let knows_clew_themes = clew_core::globalconfig::get(THEMES_GENERATION_KEY)
        .as_ref()
        .and_then(|x| x.as_integer())
        .is_some_and(|g| g >= THEMES_GENERATION);
    let pick = |key: &str, old: &str, new: &'static str| match get(key) {
        Some(id) if knows_clew_themes || id != old => id,
        _ => new.to_string(),
    };
    set_light_theme(&pick("theme_light", OLD_DEFAULT_LIGHT, DEFAULT_LIGHT));
    set_dark_theme(&pick("theme_dark", OLD_DEFAULT_DARK, DEFAULT_DARK));
    let pref = get("theme")
        .and_then(|s| ThemePref::parse(&s))
        .unwrap_or(ThemePref::System);
    apply_pref(pref);
    pref
}

/// Persist the appearance settings, preserving everything else in
/// `config.toml` (see `clew_core::globalconfig` — this file also holds the
/// API keys, so it is never rewritten with a plain, default-mode write).
pub fn save(pref: ThemePref) -> Result<(), String> {
    clew_core::globalconfig::set_keys(vec![
        ("theme".into(), toml::Value::String(pref.as_str().into())),
        (
            "theme_light".into(),
            toml::Value::String(current_light().id.into()),
        ),
        (
            "theme_dark".into(),
            toml::Value::String(current_dark().id.into()),
        ),
        (
            THEMES_GENERATION_KEY.into(),
            toml::Value::Integer(THEMES_GENERATION),
        ),
    ])
}

/// A theme presented as a pick-list choice: `Copy`, compared by id, shown by
/// name.
#[derive(Clone, Copy)]
pub struct ThemeChoice(pub &'static ThemeDef);

impl PartialEq for ThemeChoice {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}
impl Eq for ThemeChoice {}
impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.name)
    }
}

/// The light / dark themes as selectable choices, for the settings pickers.
pub fn light_choices() -> Vec<ThemeChoice> {
    THEMES
        .iter()
        .filter(|t| t.is_light)
        .map(ThemeChoice)
        .collect()
}
pub fn dark_choices() -> Vec<ThemeChoice> {
    THEMES
        .iter()
        .filter(|t| !t.is_light)
        .map(ThemeChoice)
        .collect()
}

// Accessors — the chrome reads colors through these so they follow the mode.
pub fn bg() -> Color {
    active().bg
}
pub fn bg_panel() -> Color {
    active().bg_panel
}
pub fn bg_hover() -> Color {
    active().bg_hover
}
pub fn selected() -> Color {
    active().selected
}
pub fn fg() -> Color {
    active().fg
}
/// Plain code: what the syntax colours leave uncoloured.
pub fn code_fg() -> Color {
    tokens().plain.map_or_else(fg, rgb)
}
pub fn fg_bright() -> Color {
    active().fg_bright
}
pub fn fg_muted() -> Color {
    active().fg_muted
}
pub fn dim() -> Color {
    active().dim
}
pub fn accent() -> Color {
    active().accent
}
pub fn warn() -> Color {
    active().warn
}
pub fn border() -> Color {
    active().border
}
pub fn hairline() -> Color {
    active().hairline
}
pub fn success() -> Color {
    active().success
}
pub fn warning() -> Color {
    active().warning
}
pub fn danger() -> Color {
    active().danger
}
/// The dot that says the microphone is on: the design's red, in either
/// appearance.
pub fn recording() -> Color {
    Color::from_rgb8(0xF0, 0x50, 0x4F)
}
pub fn info() -> Color {
    active().info
}
pub fn find() -> Color {
    active().find
}
pub fn selection() -> Color {
    active().selection
}
/// Text drawn on an accent-filled surface (active toggle chips, primary buttons).
pub fn on_accent() -> Color {
    active().on_accent
}
/// The active theme's syntax colors, for UI marks that echo code tokens.
pub fn tokens() -> &'static TokenColors {
    &active_theme().tokens
}

/// WCAG 2 relative luminance of an sRGB colour.
pub fn luminance(c: Color) -> f32 {
    let lin = |v: f32| {
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG 2 contrast ratio between two colours (1.0 – 21.0).
pub fn contrast(a: Color, b: Color) -> f32 {
    let (x, y) = (luminance(a), luminance(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Linear blend from `a` (t = 0) to `b` (t = 1), opaque.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::from_rgb(
        a.r + (b.r - a.r) * t,
        a.g + (b.g - a.g) * t,
        a.b + (b.b - a.b) * t,
    )
}

/// The large decorative glyph of an empty-state screen: quieter than `dim` text
/// (it is ornament, not information), on either background.
pub fn empty_state_icon() -> Color {
    mix(bg(), dim(), 0.7)
}

/// A categorical color for series `i` (the Stats language bar and table). Drawn
/// from the active theme's syntax palette so a chart reads in the same inks as
/// the code around it; the order starts with the most distinct hues.
pub fn series_color(i: usize) -> Color {
    rgb(series_ink(tokens(), i))
}

/// How many colours [`series_color`] has before it comes round again.
pub const SERIES_COLORS: usize = 8;

/// The series' colours each once, in its order: a palette may give two
/// kinds of token the same ink, and two series entries read as one.
pub fn series_distinct() -> Vec<Color> {
    let mut out: Vec<Color> = Vec::new();
    for i in 0..SERIES_COLORS {
        let c = series_color(i);
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// [`series_color`] for a given syntax palette.
fn series_ink(t: &TokenColors, i: usize) -> u32 {
    let series: [u32; SERIES_COLORS] = [
        t.function,
        t.string,
        t.type_,
        t.property,
        t.keyword,
        t.escape,
        t.number,
        t.punctuation,
    ];
    series[i % series.len()]
}

/// Color of a symbol-kind badge ("fn", "struct", …) in the outline and the
/// finder: the same ink the code uses for that kind of token.
pub fn kind_color(kind: &str) -> Color {
    kind_ink(tokens(), kind).map_or_else(dim, rgb)
}

/// The syntax ink a symbol kind echoes, or `None` for kinds shown neutrally.
fn kind_ink(t: &TokenColors, kind: &str) -> Option<u32> {
    match kind {
        "function" | "method" | "macro" => Some(t.function),
        "class" | "struct" | "enum" | "union" | "trait" | "interface" | "type" | "table"
        | "view" | "inductive" => Some(t.type_),
        "module" | "implementation" => Some(t.keyword),
        "constant" | "theorem" | "axiom" => Some(t.number),
        _ => None,
    }
}

/// The 16-color ANSI palette for notebook output, tuned to read on the editor
/// background of both light and dark themes (mid-luminance values; "black" is
/// a visible gray).
pub fn ansi(idx: u8) -> Color {
    const P: [u32; 16] = [
        0x5c6370, // black → dim gray so it stays visible
        0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, // normal
        0x7f848e, // bright black
        0xef7783, 0xa9d47f, 0xf0ca85, 0x74bdf5, 0xd48ce8, 0x67c5d1, 0xcfd6e0, // bright
    ];
    rgb(P[(idx as usize) % 16])
}

/// A distinct base colour per graphed language, so a mixed-language project
/// reads by hue on the graph map. The first six are the languages clew fully
/// supports in the import/call graphs; their hues are spread warm → cool
/// around the wheel (orange · yellow · green · teal · cyan · blue) and kept
/// saturated so they survive being shaded by depth on either background.
/// Lean (the import map) and SQL (the type map, whose nodes are its tables)
/// close the wheel: violet, magenta. The later languages take the gaps
/// between: Java red-orange and Zig amber among the warm ones, C# lime, PHP
/// periwinkle, Haskell lilac and Erlang orchid among the cool, Ruby red.
/// Anything else is filtered out of the graph upstream, so the neutral
/// fallback should not normally appear.
pub fn language_hue(lang: Option<&str>) -> Color {
    match lang {
        Some("rust") => rgb(0xf2843c),
        Some("javascript") => rgb(0xedd24e),
        Some("python") => rgb(0x57c167),
        Some("dart") => rgb(0x2fc79c),
        Some("go") => rgb(0x2fc2e4),
        Some("typescript" | "tsx") => rgb(0x5090f0),
        Some("lean") => rgb(0x9b7cf2),
        Some("sql") => rgb(0xe05ab0),
        Some("java") => rgb(0xe8644a),
        Some("zig") => rgb(0xf0a030),
        Some("csharp") => rgb(0x8fc94a),
        Some("php") => rgb(0x7b84e8),
        Some("haskell") => rgb(0xc07cf0),
        Some("erlang") => rgb(0xc65bd6),
        Some("ruby") => rgb(0xe04654),
        _ => rgb(0x8a93a6),
    }
}

/// Structural grays for the graph map's edges and arrowheads, tuned per theme
/// so the faint lines read against either background.
pub fn graph_edge() -> Color {
    if is_light() {
        rgb(0xaab0ba)
    } else {
        rgb(0x5a6272)
    }
}
pub fn graph_arrow() -> Color {
    if is_light() {
        rgb(0x878d99)
    } else {
        rgb(0x9098ab)
    }
}

/// The native macOS window-control colors (close / minimize / zoom), and the
/// gray they all take while the window is inactive. Platform constants, not
/// theme colors: every macOS window draws them the same.
pub const TRAFFIC_CLOSE: Color = rgb(0xff5f57);
pub const TRAFFIC_MINIMIZE: Color = rgb(0xfebc2e);
pub const TRAFFIC_ZOOM: Color = rgb(0x28c840);
pub const TRAFFIC_INACTIVE: Color = rgb(0x8b8b8b);
/// The glyph drawn inside a hovered traffic light.
pub const TRAFFIC_GLYPH: Color = with_alpha(rgb(0x000000), 0.6);

/// Shared corner radius for buttons / small controls.
pub const RADIUS: f32 = 6.0;

// ---- Typography -----------------------------------------------------------

/// The typographic role of a piece of UI text. Every text size in the chrome
/// comes from [`ui_size`], so the scale is one table instead of hundreds of
/// literals, and nothing is set below [`MIN_TEXT_SIZE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextRole {
    /// Dense secondary annotations: counts, badges, one-line summaries.
    Caption,
    /// Secondary labels: chips, status bar, section notes, small buttons.
    Small,
    /// Panel and list body text.
    Body,
    /// Primary list rows, menu items, inputs.
    Base,
    /// What is read rather than scanned: the prose clew writes (answers,
    /// explanations), a question being typed, the palette's results.
    Reading,
    /// Emphasized lines: dialog headlines, the finder input.
    Emphasis,
    /// Sub-headings inside a page (a doc member, "Module map").
    Subtitle,
    /// Modal and card titles.
    Title,
    /// Page headings (Overview, Code Statistics).
    Heading,
    /// Big figures and the doc page's own title.
    Display,
    /// The welcome screen's brand name.
    Hero,
}

/// The smallest size any UI text is drawn at.
pub const MIN_TEXT_SIZE: f32 = 10.0;

/// The size, in logical pixels, for text of `role` — the UI type scale.
pub const fn ui_size(role: TextRole) -> f32 {
    match role {
        TextRole::Caption => 10.5,
        TextRole::Small => 11.5,
        TextRole::Body => 12.5,
        TextRole::Base => 13.0,
        TextRole::Reading => 13.5,
        TextRole::Emphasis => 14.0,
        TextRole::Subtitle => 15.0,
        TextRole::Title => 17.0,
        TextRole::Heading => 18.0,
        TextRole::Display => 22.0,
        TextRole::Hero => 34.0,
    }
}

pub fn app_theme() -> Theme {
    let p = active();
    Theme::custom(
        if is_light() { "clew-light" } else { "clew" }.to_string(),
        iced::theme::Palette {
            background: p.bg,
            text: p.fg,
            primary: p.accent,
            success: p.success,
            danger: p.danger,
            warning: p.warning,
        },
    )
}

/// Markdown rendering settings themed to the active palette.
///
/// iced's default markdown style hardcodes the inline-code chip as a near-black
/// background with white text regardless of the theme, which clashes on light
/// themes. This follows the palette instead: a faint, theme-relative chip with
/// accent-coloured code and links. The rest (body text) inherits the container's
/// colour, so it already tracks the theme.
pub fn markdown_settings() -> iced::widget::markdown::Settings {
    markdown_settings_at(PROSE_SIZE)
}

/// Size of the prose clew writes in its panels and cards (explanations,
/// answers, the overview): a notch over the interface's own text.
pub const PROSE_SIZE: f32 = ui_size(TextRole::Reading);
/// Size of a markdown file's text, read as a document in the reader.
pub const DOCUMENT_SIZE: f32 = 16.0;

/// How much larger than the prose around them its headings are, level by
/// level, in the answers, explanations and notes Clew writes in its cards
/// and panels: a notch each, as a chat's are. iced's own scale (a level-1
/// heading twice the text) is a document's, and an answer's "## How it is
/// used" stood in a card at nearly twice the size of what it headed.
pub const PROSE_HEADING_SCALE: [f32; 6] = [1.3, 1.2, 1.1, 1.0, 1.0, 1.0];

/// [`markdown_settings`] at a given base text size: prose in a card or a
/// panel, its headings a notch over it ([`PROSE_HEADING_SCALE`]).
pub fn markdown_settings_at(size: f32) -> iced::widget::markdown::Settings {
    let mut settings = document_markdown_settings_at(size);
    let [h1, h2, h3, h4, h5, h6] = PROSE_HEADING_SCALE.map(|scale| (size * scale).into());
    settings.h1_size = h1;
    settings.h2_size = h2;
    settings.h3_size = h3;
    settings.h4_size = h4;
    settings.h5_size = h5;
    settings.h6_size = h6;
    settings
}

/// A markdown file read as a document in the reader: its headings at a
/// document's scale, a level-1 one twice its text.
pub fn document_markdown_settings_at(size: f32) -> iced::widget::markdown::Settings {
    use iced::widget::markdown;
    markdown::Settings::with_text_size(
        size,
        markdown::Style {
            // The interface's face, named: the system's sans-serif (what
            // `Font::default()` asks for) is not the one Clew is set in.
            font: crate::fonts::UI,
            // The chip's padding is drawn over the spaces beside it (it takes
            // no room of its own), so it is kept narrower than a space.
            inline_code_padding: iced::padding::left(2).right(2),
            inline_code_highlight: markdown::Highlight {
                // A faint wash of the foreground: subtle on dark themes, subtle on
                // light ones, never a hard black box.
                background: with_alpha(fg(), 0.08).into(),
                border: iced::border::rounded(5),
            },
            // Code reads in the text's own colour on its chip, as names in a
            // sentence do.
            inline_code_color: fg(),
            inline_code_font: crate::fonts::CODE,
            code_block_font: crate::fonts::CODE,
            link_color: accent(),
        },
    )
}

/// Sidebar / toolbar panel background.
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// Editor pane background.
pub fn editor(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg().into()),
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// Status bar background, its corners as the window's (see [`chrome`]).
pub fn statusbar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        border: Border {
            radius: window_corners().into(),
            ..Border::default()
        },
        text_color: Some(dim()),
        ..container::Style::default()
    }
}

/// Elevated panel: modals, menus, tooltips — lifted off what is under it.
pub fn modal_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(active().elevated.into()),
        text_color: Some(fg()),
        border: Border {
            color: border(),
            width: 1.0,
            radius: MENU_RADIUS.into(),
        },
        shadow: panel_shadow(),
        ..container::Style::default()
    }
}

/// The list of Settings' pages, down the modal's left: the chrome's fill,
/// rounded only on the modal's own left corners so it never shows past them.
pub fn settings_nav(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        border: Border {
            radius: iced::border::Radius {
                top_left: MENU_RADIUS,
                bottom_left: MENU_RADIUS,
                top_right: 0.0,
                bottom_right: 0.0,
            },
            ..Border::default()
        },
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// [`modal_panel`] with the corners of `radius`: the design rounds its
/// command bar 12, its ⋯ menu 11.
pub fn modal_panel_rounded(radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let style = modal_panel(theme);
        container::Style {
            border: Border {
                radius: radius.into(),
                ..style.border
            },
            ..style
        }
    }
}

/// The design's shadow under what floats over the window — a menu, the
/// command bar, Ask, the voice panel (`panelShadow`) — its reach scaled as
/// [`with_shadow_scale`] asks.
pub fn panel_shadow() -> iced::Shadow {
    let scale = SHADOW_SCALE.with(std::cell::Cell::get);
    iced::Shadow {
        color: scrim(if is_light() { 0.22 } else { 0.6 }),
        offset: iced::Vector::new(0.0, PANEL_SHADOW_OFFSET * scale),
        blur_radius: PANEL_SHADOW_BLUR * scale,
    }
}

/// How far the design's panel shadow falls below what casts it, and how
/// far it blurs.
pub const PANEL_SHADOW_OFFSET: f32 = 24.0;
pub const PANEL_SHADOW_BLUR: f32 = 70.0;

thread_local! {
    static SHADOW_SCALE: std::cell::Cell<f32> = const { std::cell::Cell::new(1.0) };
}

/// Draw with [`panel_shadow`]'s reach scaled by `scale`.
pub fn with_shadow_scale<R>(scale: f32, draw: impl FnOnce() -> R) -> R {
    let before = SHADOW_SCALE.replace(scale);
    let drawn = draw();
    SHADOW_SCALE.set(before);
    drawn
}

/// A tooltip, in the window's own appearance: the theme's raised surface
/// with a hairline edge — not a dark bubble over a light window, nor a
/// white one over a cream theme. Its shadow lifts it off what it covers.
pub fn tip(_theme: &Theme) -> container::Style {
    let light = is_light();
    container::Style {
        background: Some(raised().into()),
        text_color: Some(tip_text()),
        border: Border {
            color: if light { hairline() } else { wash(0.08) },
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: iced::Shadow {
            color: scrim(if light { 0.14 } else { 0.5 }),
            offset: iced::Vector::new(0.0, if light { 6.0 } else { 8.0 }),
            blur_radius: if light { 20.0 } else { 24.0 },
        },
        ..container::Style::default()
    }
}

/// A tooltip's words (`tipText`): the window's own text colour.
pub fn tip_text() -> Color {
    fg()
}

/// A surface raised off the window — a tooltip, a drop-down's list, the
/// chosen segment's chip: the theme's own floating surface in light, lifted
/// a little toward its text in dark, where a floating panel reads lighter.
pub fn raised() -> Color {
    raised_in(active(), is_light())
}

/// [`raised`] in the palette `p` (`light` or dark).
fn raised_in(p: &Palette, light: bool) -> Color {
    if light {
        p.elevated
    } else {
        mix(p.elevated, p.fg, 0.06)
    }
}

/// A key of a tooltip's shortcut (`tipKey`): a wash over the bubble — of
/// the text's colour on the light one, of white on the dark.
pub fn tip_key(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(if is_light() { 0.07 } else { 0.10 }).into()),
        text_color: Some(tip_text()),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Corner radius of menus and popovers.
pub const MENU_RADIUS: f32 = 10.0;
/// Corner radius of the code's sheet.
pub const SHEET_RADIUS: f32 = 10.0;
/// The Windows close button's color under the pointer: a platform constant,
/// the one place the window buttons of Windows and Linux take a color.
pub const CLOSE_HOVER: Color = rgb(0xc42b1c);

/// A faint wash of the text color: the one way the chrome tints a surface
/// (wells, hovers, the active chip), so it reads the same over the chrome
/// and over the sheet, in light and in dark.
pub fn wash(strength: f32) -> Color {
    with_alpha(fg(), strength)
}

/// The frame Clew draws round its borderless window, its corners rounded
/// to `radius`: the design's hairline of the window's edge (`winBorder`:
/// black at 12% in light, white at 8% in dark), over the chrome — laid on
/// it here, opaque, as iced draws a border in place of what it edges, and
/// a window whose corners are clear shows the desktop through anything
/// less.
pub fn window_frame(radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(bg_panel().into()),
        border: iced::Border {
            color: if is_light() {
                mix(bg_panel(), Color::BLACK, 0.12)
            } else {
                mix(bg_panel(), Color::WHITE, 0.08)
            },
            width: 1.0,
            radius: radius.into(),
        },
        ..container::Style::default()
    }
}

thread_local! {
    /// See [`window_corners`].
    static WINDOW_CORNERS: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
}

/// The radius of the corners of the window being drawn, inside its frame:
/// what a surface reaching the window's corners (the chrome, the status
/// bar, a modal's veil) rounds its own to, for the corners to stay clear
/// (`ui::window_radius_for`). Set while the window's content is drawn
/// ([`with_window_corners`]): iced draws one window at a time, on one
/// thread, and reads a style as it draws.
pub fn window_corners() -> f32 {
    WINDOW_CORNERS.with(std::cell::Cell::get)
}

/// Draw with the window's corners `radius` (see [`window_corners`]).
pub fn with_window_corners<R>(radius: f32, draw: impl FnOnce() -> R) -> R {
    let before = WINDOW_CORNERS.replace(radius);
    let drawn = draw();
    WINDOW_CORNERS.set(before);
    drawn
}

/// The window's chrome — the surface the top bar, the sidebar, the tool
/// strip and the status bar share, and the code's sheet floats over —
/// rounded as the window's corners are: unseen over the chrome itself,
/// and the corners kept clear where it reaches them.
pub fn chrome(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        border: Border {
            radius: window_corners().into(),
            ..Border::default()
        },
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// What sits on the sheet (the panes, the reading panel, the debugger's
/// panel): no fill of its own, so the sheet's rounded surface shows through
/// to its corners.
pub fn on_sheet(_theme: &Theme) -> container::Style {
    container::Style {
        background: None,
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// The code's sheet: the reading surface, raised off the chrome by a
/// rounded edge and a soft shadow rather than by lines around it.
pub fn sheet(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg().into()),
        text_color: Some(fg()),
        border: Border {
            color: hairline(),
            width: 1.0,
            radius: SHEET_RADIUS.into(),
        },
        shadow: iced::Shadow {
            color: scrim(if is_light() { 0.06 } else { 0.32 }),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 18.0,
        },
        ..container::Style::default()
    }
}

/// A field on the chrome (the chat field, the file filter): a faint well,
/// its edge lit in the accent while it is in use.
pub fn field(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |_theme| container::Style {
        background: Some(wash(if is_light() { 0.035 } else { 0.045 }).into()),
        text_color: Some(fg()),
        border: Border {
            color: if active {
                with_alpha(accent(), 0.65)
            } else {
                hairline()
            },
            width: 1.0,
            radius: 9.0.into(),
        },
        shadow: if active {
            iced::Shadow {
                color: with_alpha(accent(), 0.22),
                offset: iced::Vector::ZERO,
                blur_radius: 8.0,
            }
        } else {
            iced::Shadow::default()
        },
        ..container::Style::default()
    }
}

/// A text input with no face of its own, set inside a [`field`] (the file
/// filter, the command palette): the field around it is the well.
/// [`bare_input`] for a field of several lines (Ask's question).
pub fn bare_editor(
    _theme: &Theme,
    _status: iced::widget::text_editor::Status,
) -> iced::widget::text_editor::Style {
    iced::widget::text_editor::Style {
        background: Color::TRANSPARENT.into(),
        border: Border::default(),
        placeholder: dim(),
        value: fg(),
        selection: selection(),
    }
}

pub fn bare_input(_theme: &Theme, _status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Color::TRANSPARENT.into(),
        border: Border::default(),
        icon: dim(),
        placeholder: dim(),
        value: fg(),
        selection: selection(),
    }
}

/// A text field on its own: the same rounded well as [`field`] — the
/// search field, Ask's input — its edge lit in the accent while it has the
/// keyboard. Every field of the app looks like this one.
/// [`input`] for a field what the form says is wrong is about: its edge in
/// the danger colour, hovered or focused alike.
pub fn input_marked(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let mut style = input(theme, status);
    style.border.color = danger();
    style
}

pub fn input(_theme: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused { .. });
    let hovered = matches!(status, text_input::Status::Hovered);
    text_input::Style {
        background: wash(if is_light() { 0.035 } else { 0.045 }).into(),
        border: Border {
            color: if focused {
                with_alpha(accent(), 0.65)
            } else if hovered {
                wash(0.16)
            } else {
                hairline()
            },
            width: 1.0,
            radius: 9.0.into(),
        },
        icon: dim(),
        placeholder: dim(),
        value: fg(),
        selection: selection(),
    }
}

/// A drop-down: the same rounded well as a field ([`input`]), its edge lit
/// while it is open.
pub fn pick(_theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        text_color: fg(),
        placeholder_color: dim(),
        handle_color: dim(),
        background: wash(if is_light() { 0.035 } else { 0.045 }).into(),
        border: Border {
            color: match status {
                pick_list::Status::Opened { .. } => with_alpha(accent(), 0.65),
                pick_list::Status::Hovered => wash(0.16),
                pick_list::Status::Active => hairline(),
            },
            width: 1.0,
            radius: 9.0.into(),
        },
    }
}

/// A drop-down's list: a rounded panel as the menus are, its row under the
/// pointer lit as theirs.
pub fn pick_menu(_theme: &Theme) -> iced::overlay::menu::Style {
    iced::overlay::menu::Style {
        background: raised().into(),
        border: Border {
            color: hairline(),
            width: 1.0,
            radius: 9.0.into(),
        },
        text_color: fg(),
        selected_text_color: fg_bright(),
        selected_background: wash(0.08).into(),
        shadow: iced::Shadow {
            color: scrim(0.18),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 16.0,
        },
    }
}

/// A checkbox: rounded as the fields are, filled in the accent when on.
pub fn check(_theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let (checked, hovered, disabled) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };
    checkbox::Style {
        background: if checked {
            with_alpha(accent(), if disabled { 0.45 } else { 1.0 }).into()
        } else if hovered {
            wash(0.06).into()
        } else {
            Color::TRANSPARENT.into()
        },
        icon_color: on_accent(),
        border: Border {
            color: if checked {
                with_alpha(accent(), if disabled { 0.45 } else { 1.0 })
            } else if hovered {
                wash(0.28)
            } else {
                wash(if disabled { 0.12 } else { 0.22 })
            },
            width: 1.0,
            radius: 5.0.into(),
        },
        text_color: Some(if disabled { dim() } else { fg() }),
    }
}

/// The wash a borderless control lays under the pointer, and a little
/// deeper while pressed: faint enough that a row of them does not read as a
/// row of boxes, and the same everywhere, so every control answers the
/// pointer alike.
pub fn hover_wash(status: button::Status) -> Option<iced::Background> {
    let light = is_light();
    match status {
        button::Status::Hovered => Some(wash(if light { 0.055 } else { 0.075 }).into()),
        button::Status::Pressed => Some(wash(if light { 0.09 } else { 0.11 }).into()),
        _ => None,
    }
}

/// An on/off option beside a field (match case, whole word, regex): as
/// round as the field, outlined while off, filled in the accent while on.
pub fn toggle(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(if on {
                accent().into()
            } else {
                hover_wash(status).unwrap_or(Color::TRANSPARENT.into())
            }),
            text_color: if on {
                on_accent()
            } else if hovered {
                fg()
            } else {
                fg_muted()
            },
            border: Border {
                radius: 7.0.into(),
                width: 1.0,
                color: if on { accent() } else { hairline() },
            },
            ..button::Style::default()
        }
    }
}

/// A field that is really a button (the search field opens the finder):
/// the field's own well and edge ([`field`]), the whole of it a shade deeper
/// under the pointer, its edge a little firmer. A wash inside the field's
/// padding read as a second box in the field.
pub fn field_button(_theme: &Theme, status: button::Status) -> button::Style {
    let light = is_light();
    let (shade, edge) = match status {
        button::Status::Hovered => (if light { 0.055 } else { 0.07 }, wash(0.16)),
        button::Status::Pressed => (if light { 0.075 } else { 0.09 }, wash(0.18)),
        _ => (if light { 0.035 } else { 0.045 }, hairline()),
    };
    button::Style {
        background: Some(wash(shade).into()),
        text_color: fg_muted(),
        border: Border {
            color: edge,
            width: 1.0,
            radius: 9.0.into(),
        },
        ..button::Style::default()
    }
}

/// The face on Ask's pill: a capsule as round as the pill, washed under the
/// pointer inside it.
pub fn pill_face(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: hover_wash(status),
        text_color: match status {
            button::Status::Disabled => dim(),
            button::Status::Hovered | button::Status::Pressed => fg_bright(),
            _ => fg(),
        },
        border: Border {
            radius: 999.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// The track a segmented control's segments sit in.
pub fn trough(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(0.045).into()),
        border: Border {
            radius: 9.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A card of Settings' rows (`ui::form`): a faint well with a hairline
/// edge, set off the page it is on.
pub fn form_card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(if is_light() { 0.018 } else { 0.03 }).into()),
        border: Border {
            color: hairline(),
            width: 1.0,
            radius: 10.0.into(),
        },
        text_color: Some(fg()),
        ..container::Style::default()
    }
}

/// One of several choices in a card (`ui::form::choice`): a row of the
/// card, washed while the pointer is over it.
pub fn form_choice(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered => Some(wash(0.04).into()),
            button::Status::Pressed => Some(wash(0.07).into()),
            _ => None,
        },
        text_color: fg(),
        border: Border {
            radius: 10.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// One segment of a segmented control: the chosen one raised as a chip,
/// the rest quiet, washed and their labels brightening under the pointer.
pub fn segment(chosen: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = if chosen {
            Some(if is_light() { raised() } else { wash(0.10) }.into())
        } else {
            hover_wash(status)
        };
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background,
            text_color: if chosen || hovered {
                fg_bright()
            } else {
                fg_muted()
            },
            border: Border {
                radius: 7.0.into(),
                width: if chosen && !is_light() { 0.5 } else { 0.0 },
                color: wash(0.10),
            },
            shadow: if chosen && is_light() {
                iced::Shadow {
                    color: scrim(0.12),
                    offset: iced::Vector::new(0.0, 1.0),
                    blur_radius: 2.0,
                }
            } else {
                iced::Shadow::default()
            },
            ..button::Style::default()
        }
    }
}

/// A borderless icon button of the chrome (the top bar, the tool strip):
/// washed under the pointer ([`hover_wash`]); `on` keeps it lit (a menu it
/// opened is showing, a panel it toggles is shown), a shade more under the
/// pointer.
pub fn icon_button(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = if on {
            Some(
                wash(match status {
                    button::Status::Hovered | button::Status::Pressed => 0.11,
                    _ => 0.08,
                })
                .into(),
            )
        } else {
            hover_wash(status)
        };
        button::Style {
            background,
            text_color: match status {
                button::Status::Disabled => dim(),
                _ => fg(),
            },
            border: Border {
                radius: 7.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// The window buttons Clew draws on Windows and Linux: square and full
/// height, the close one red under the pointer.
pub fn window_button(close: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = match status {
            button::Status::Hovered | button::Status::Pressed if close => Some(CLOSE_HOVER.into()),
            _ => None,
        };
        button::Style {
            background,
            text_color: match status {
                button::Status::Hovered | button::Status::Pressed if close => Color::WHITE,
                _ => fg(),
            },
            ..button::Style::default()
        }
    }
}

/// A note raised on the sheet: the one the reader is on, among the margin's.
pub fn note_card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(active().elevated.into()),
        text_color: Some(fg()),
        border: Border {
            color: if is_light() { border() } else { wash(0.08) },
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: iced::Shadow {
            color: scrim(if is_light() { 0.06 } else { 0.3 }),
            offset: iced::Vector::new(0.0, 4.0),
            blur_radius: 14.0,
        },
        ..container::Style::default()
    }
}

/// What the reader asked, in the conversation: a bubble on the right, its
/// tail corner squared.
pub fn bubble(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(if is_light() { 0.06 } else { 0.08 }).into()),
        text_color: Some(fg()),
        border: Border {
            radius: iced::border::Radius {
                top_left: 12.0,
                top_right: 12.0,
                bottom_right: 4.0,
                bottom_left: 12.0,
            },
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// One key of a shortcut, drawn as a small cap.
pub fn keycap(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(if is_light() { 0.02 } else { 0.05 }).into()),
        text_color: Some(fg_muted()),
        border: Border {
            color: hairline(),
            width: 1.0,
            radius: 5.0.into(),
        },
        ..container::Style::default()
    }
}

/// A small label chip (a view's tag, a step's state).
pub fn chip(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(0.06).into()),
        text_color: Some(fg_muted()),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A row of a menu: full-width, rounded, lit under the pointer.
pub fn menu_row(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(wash(0.07).into()),
            _ => None,
        },
        text_color: match status {
            button::Status::Disabled => dim(),
            _ => fg(),
        },
        border: Border {
            radius: 7.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Thin determinate progress bar — accent fill on a faint accent-tinted track.
pub fn progress(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: iced::Background::Color(with_alpha(accent(), 0.16)),
        bar: iced::Background::Color(accent()),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 2.0.into(),
        },
    }
}

/// A black veil of the given opacity, laid over the app to push it back (modal
/// backdrops, the tutorial spotlight).
pub const fn scrim(alpha: f32) -> Color {
    with_alpha(Color::BLACK, alpha)
}

/// The floating Ask window: the elevated surface, lifted off the code under
/// it by a soft shadow.
pub fn floating_card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(active().elevated.into()),
        text_color: Some(fg()),
        border: Border {
            color: border(),
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: panel_shadow(),
        ..container::Style::default()
    }
}

/// The pill Ask hides to, in the window's corner: the floating card's
/// surface, as round as it is tall, lifted by a shadow of its own size —
/// the card's, made for a panel, spread a grey haze round a small control.
pub fn floating_pill(theme: &Theme) -> container::Style {
    let card = floating_card(theme);
    container::Style {
        border: Border {
            radius: 20.0.into(),
            ..card.border
        },
        shadow: pill_shadow(),
        ..card
    }
}

/// The shadow of a small floating control: close and soft.
pub fn pill_shadow() -> iced::Shadow {
    iced::Shadow {
        color: scrim(if is_light() { 0.10 } else { 0.35 }),
        offset: iced::Vector::new(0.0, PILL_SHADOW_OFFSET),
        blur_radius: PILL_SHADOW_BLUR,
    }
}

/// How far [`pill_shadow`] falls below its control, and how far it blurs.
pub const PILL_SHADOW_OFFSET: f32 = 3.0;
pub const PILL_SHADOW_BLUR: f32 = 12.0;

/// Dimmed backdrop behind the modal: over the whole window, its corners
/// as the window's, so they stay clear.
pub fn backdrop(_theme: &Theme) -> container::Style {
    container::Style {
        // The design's `scrim`.
        background: Some(scrim(if is_light() { 0.16 } else { 0.5 }).into()),
        border: Border {
            radius: window_corners().into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Per-pane header strip; the active pane gets an accent-tinted title.
pub fn pane_header(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(wash(0.025).into()),
        border: Border {
            color: hairline(),
            width: 1.0,
            radius: 0.0.into(),
        },
        ..container::Style::default()
    }
}

/// Flat list-row button (file tree, search results, outline, finder).
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = if selected {
            Some(self::selected().into())
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => Some(bg_hover().into()),
                _ => None,
            }
        };
        button::Style {
            background,
            text_color: if selected { fg_bright() } else { fg() },
            border: Border {
                radius: 6.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// A row of the finder and the command bar: a [`list_row`], its corners
/// rounder.
pub fn finder_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let mut style = list_row(selected)(theme, status);
        style.border.radius = 8.0.into();
        style
    }
}

/// A scrollbar that stays invisible until you hover the scrollable (or drag it),
/// so panels aren't littered with always-on bars. Thin and subtle when shown.
pub fn overlay_scrollbar(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    let hidden = scrollable::Scroller {
        background: Color::TRANSPARENT.into(),
        border: iced::border::rounded(3),
    };
    let shown = scrollable::Scroller {
        background: with_alpha(active().scrollbar, 0.55).into(),
        border: iced::border::rounded(3),
    };
    let scroller = match status {
        scrollable::Status::Active { .. } => hidden,
        _ => shown, // Hovered or Dragged
    };
    style.vertical_rail.background = None;
    style.vertical_rail.scroller = scroller;
    style.horizontal_rail.background = None;
    style.horizontal_rail.scroller = scroller;
    style
}

/// [`overlay_scrollbar`] whose scroller shows at rest, faintly: for a menu
/// whose rows can run past the window, the one cue that more are below.
pub fn cue_scrollbar(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = overlay_scrollbar(theme, status);
    if let scrollable::Status::Active { .. } = status {
        let scroller = scrollable::Scroller {
            background: with_alpha(active().scrollbar, 0.35).into(),
            border: iced::border::rounded(3),
        };
        style.vertical_rail.scroller = scroller;
    }
    style
}

/// Accent-filled primary action button (modals).
pub fn primary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => active().accent_hover,
        // Not yet to be pressed (a question's Allow before it can be read).
        button::Status::Disabled => accent().scale_alpha(0.45),
        _ => accent(),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: match status {
            button::Status::Disabled => active().on_accent.scale_alpha(0.7),
            _ => active().on_accent,
        },
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// A switch's track (Keep listening): the accent when `on`, a quiet well
/// when off, round at its ends.
pub fn switch(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if on {
            if hovered {
                active().accent_hover
            } else {
                accent()
            }
        } else {
            wash(if hovered { 0.24 } else { 0.18 })
        };
        button::Style {
            background: Some(background.into()),
            border: Border {
                radius: 8.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// A switch's knob: round, in the colour drawn on the accent — white in
/// most themes, the theme's own paper in those whose accent wants it.
pub fn switch_knob(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(on_accent().into()),
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A question before something runs (an approval card): a faint wash of the
/// accent, edged in it.
pub fn approve_card(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(with_alpha(accent(), if is_light() { 0.05 } else { 0.07 }).into()),
        text_color: Some(fg()),
        border: Border {
            color: with_alpha(accent(), 0.33),
            width: 1.0,
            radius: 10.0.into(),
        },
        ..container::Style::default()
    }
}

/// A round button of the Ask composer, as wide as it is tall: the send
/// button (`primary`, in the accent) or the microphone (a quiet well).
pub fn round_button(primary: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let base = if primary {
            primary_button(theme, status)
        } else {
            button::Style {
                background: Some(
                    wash(match status {
                        button::Status::Hovered | button::Status::Pressed => 0.13,
                        _ => 0.08,
                    })
                    .into(),
                ),
                text_color: fg(),
                ..button::Style::default()
            }
        };
        button::Style {
            border: Border {
                radius: 15.0.into(),
                ..Border::default()
            },
            ..base
        }
    }
}

/// Small toolbar button: borderless and unfilled at rest, so the toolbar
/// reads as clean text rather than a wall of boxes; under the pointer,
/// washed ([`hover_wash`]) and its label brightened.
pub fn toolbar_button(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: hover_wash(status),
        text_color: match status {
            button::Status::Disabled => dim(),
            button::Status::Hovered | button::Status::Pressed => fg_bright(),
            _ => fg(),
        },
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Secondary action button — the non-primary choice next to a
/// `primary_button` (e.g. "Open Remote…"), or a field's own button ("Use",
/// "Browse…"): the rounded well of a field ([`input`]), a shade deeper so
/// it reads as something to press.
pub fn secondary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let light = is_light();
    let (shade, text_color) = match status {
        button::Status::Hovered => (if light { 0.075 } else { 0.09 }, fg()),
        button::Status::Pressed => (if light { 0.11 } else { 0.13 }, fg()),
        button::Status::Disabled => (if light { 0.03 } else { 0.04 }, dim()),
        button::Status::Active => (if light { 0.05 } else { 0.065 }, fg()),
    };
    button::Style {
        background: Some(wash(shade).into()),
        text_color,
        border: Border {
            radius: 8.0.into(),
            width: 1.0,
            color: hairline(),
        },
        ..button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    /// WCAG's contrast ratio of two colours.
    fn contrast(a: Color, b: Color) -> f32 {
        let lum = |c: Color| {
            let f = |x: f32| {
                if x <= 0.03928 {
                    x / 12.92
                } else {
                    ((x + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
        };
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// The colours words are written in where they say something — a test's
    /// name, a warning, an error, a link — read at 4.5:1 or more on either
    /// of a theme's surfaces, in every theme: the light theme's green test
    /// names were about 3:1.
    #[test]
    fn every_themes_meaningful_colours_read_on_its_surfaces() {
        for theme in THEMES {
            let p = &theme.palette;
            for (what, ink) in [
                ("success", p.success),
                ("warning", p.warning),
                ("danger", p.danger),
                ("info", p.info),
                ("accent", p.accent),
            ] {
                for (surface, bg) in [("bg", p.bg), ("bg_panel", p.bg_panel)] {
                    let ratio = contrast(ink, bg);
                    assert!(
                        ratio >= 4.5,
                        "{}: {what} on {surface} is {ratio:.2}:1",
                        theme.id
                    );
                }
            }
        }
    }

    use super::*;

    /// A raised surface — a tooltip, a drop-down's list, the chosen
    /// segment — is the theme's own: never brighter than the theme's
    /// brightest surface in light (a white list over Gruvbox's cream was),
    /// and dark in dark; and what is drawn on the accent reads on it.
    #[test]
    fn raised_surfaces_are_the_themes_own() {
        for theme in THEMES {
            let p = &theme.palette;
            let raised = raised_in(p, theme.is_light);
            if theme.is_light {
                let brightest = [p.bg, p.bg_panel, p.elevated]
                    .into_iter()
                    .map(luminance)
                    .fold(0.0, f32::max);
                assert!(
                    luminance(raised) <= brightest + 1e-4,
                    "{}: a raised surface brighter than any of its own",
                    theme.id
                );
            } else {
                assert!(
                    luminance(raised) < 0.2,
                    "{}: a light surface in dark",
                    theme.id
                );
            }
            assert!(
                contrast(raised, p.fg) >= 4.5,
                "{}: text on a raised surface is {:.2}:1",
                theme.id,
                contrast(raised, p.fg)
            );
            assert!(
                contrast(p.on_accent, p.accent) >= 4.5,
                "{}: on_accent on accent is {:.2}:1",
                theme.id,
                contrast(p.on_accent, p.accent)
            );
        }
        // Ids are unique, and there are themes of both kinds to choose.
        let mut ids: Vec<&str> = THEMES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEMES.len());
        assert!(THEMES.iter().filter(|t| t.is_light).count() >= 7);
        assert!(THEMES.iter().filter(|t| !t.is_light).count() >= 10);
    }

    /// A small floating control casts a small shadow: Ask's pill, under
    /// the panel's, spread a haze several times its own height.
    #[test]
    fn the_pill_casts_a_shadow_of_its_size() {
        let pill = floating_pill(&Theme::Dark).shadow;
        let panel = panel_shadow();
        assert!(pill.offset.y + pill.blur_radius <= 16.0, "{pill:?}");
        assert!(pill.color.a < panel.color.a);
        assert!(pill.blur_radius < panel.blur_radius / 4.0);
    }

    /// What Clew writes — explanations, answers, notes — is set in the
    /// interface's own face, never the system's sans-serif.
    #[test]
    fn prose_is_set_in_the_interfaces_face() {
        assert_eq!(markdown_settings().style.font, crate::fonts::UI);
        assert_eq!(markdown_settings_at(13.0).style.font, crate::fonts::UI);
    }

    /// The palette is process-global while windows are not, so a window that
    /// did not make the change still has to learn of it. The revision is what
    /// the shell watches to fan the change out; without it the other windows
    /// kept a stale preference and stale-colored cached SVGs.
    #[test]
    fn revision_tracks_real_appearance_changes_only() {
        apply_pref(ThemePref::Dark);
        let start = revision();

        // A no-op change must not bump it (or the shell would fan out on
        // every window update).
        apply_pref(ThemePref::Dark);
        assert_eq!(revision(), start);

        // A real one does, and the new preference is readable globally.
        apply_pref(ThemePref::Light);
        assert!(revision() > start, "flipping the mode bumps the revision");
        assert_eq!(current_pref(), ThemePref::Light);
        assert!(is_light());

        // The PREFERENCE is observed state too, even when it resolves to the
        // appearance already on screen. Watching only the mode meant a window
        // that had not made the change went on believing the preference was
        // System, and the next OS appearance change had it write that back
        // over the user's explicit choice.
        set_light(true);
        let at_light = revision();
        apply_pref(ThemePref::System);
        assert!(
            revision() > at_light,
            "changing only the preference must still bump"
        );
        assert_eq!(current_pref(), ThemePref::System);

        // So does picking a different variant of the active side.
        let at_variant = revision();
        set_light_theme("paper-light");
        assert!(revision() > at_variant, "a theme selection bumps it");
        assert_eq!(current_light().id, "paper-light");
        // Selecting the same one again does not.
        let settled = revision();
        set_light_theme("paper-light");
        assert_eq!(revision(), settled);
    }

    /// Resolving the `System` preference used to spawn `defaults read -g
    /// AppleInterfaceStyle` synchronously — on every window focus change, and
    /// from this test module whenever a test applied `System`. It is answered
    /// in-process now: AppKit on the main thread, and off it (an explicitly
    /// spawned thread here, so the answer cannot come from AppKit) the last
    /// appearance recorded there.
    #[test]
    fn system_appearance_off_the_main_thread_is_answered_in_process() {
        std::thread::spawn(|| {
            record_system_appearance(true);
            assert!(system_is_light());
            record_system_appearance(false);
            assert!(!system_is_light());
        })
        .join()
        .expect("appearance thread");
    }

    /// One type scale: strictly increasing roles, nothing under the floor.
    #[test]
    fn type_scale_is_ordered_and_never_below_the_minimum() {
        use TextRole::*;
        let roles = [
            Caption, Small, Body, Base, Emphasis, Subtitle, Title, Heading, Display, Hero,
        ];
        for pair in roles.windows(2) {
            assert!(
                ui_size(pair[0]) < ui_size(pair[1]),
                "{:?} is not smaller than {:?}",
                pair[0],
                pair[1]
            );
        }
        assert!(roles.iter().all(|r| ui_size(*r) >= MIN_TEXT_SIZE));
    }

    /// Kind badges and chart series follow the syntax palette of whichever
    /// theme is active — for One Dark exactly the inks they used to hard-code.
    #[test]
    fn kind_and_series_inks_come_from_the_theme_tokens() {
        assert_eq!(kind_ink(&ONE_DARK_TOK, "method"), Some(0x61afef));
        assert_eq!(kind_ink(&ONE_DARK_TOK, "struct"), Some(0xe5c07b));
        assert_eq!(kind_ink(&ONE_LIGHT_TOK, "module"), Some(0xa626a4));
        assert_eq!(kind_ink(&ONE_LIGHT_TOK, "constant"), Some(0x986801));
        assert_eq!(kind_ink(&PAPER_DARK_TOK, "function"), Some(0x7fa0b0));
        assert_eq!(kind_ink(&ONE_DARK_TOK, "section"), None);
        let one_dark: Vec<u32> = (0..8).map(|i| series_ink(&ONE_DARK_TOK, i)).collect();
        assert_eq!(
            one_dark,
            [
                0x61afef, 0x98c379, 0xe5c07b, 0xe06c75, 0xc678dd, 0x56b6c2, 0xd19a66, 0x848b98
            ]
        );
        assert_eq!(series_ink(&ONE_DARK_TOK, 8), series_ink(&ONE_DARK_TOK, 0));
    }
}
