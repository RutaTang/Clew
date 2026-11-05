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

use iced::widget::{button, container, progress_bar, scrollable, text_input};
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
    pub bg_active: Color, // pressed / active control
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
    pub elevated: Color,       // floating modal panel background
    pub on_accent: Color,      // text on an accent-filled button
    pub accent_hover: Color,   // accent button, hovered/pressed
    pub control_border: Color, // border of a filled neutral (secondary) button
    pub scrollbar: Color,      // overlay scrollbar thumb
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
    bg_active: rgb(0x2a2a2f),
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
    control_border: rgb(0x3a3a40),
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
    bg_active: rgb(0xe2e2e6),
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
    control_border: rgb(0xd4d4d8),
    scrollbar: rgb(0xa1a1aa),
    success: rgb(0x16a34a),
    warning: rgb(0xb45309),
    danger: rgb(0xdc2626),
    info: rgb(0x0284c7),
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
    bg_active: rgb(0x353c48),
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
    control_border: rgb(0x3d4450),
    scrollbar: rgb(0x8a94a6),
    success: rgb(0x98c379),
    warning: rgb(0xe5c07b),
    danger: rgb(0xe06c75),
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
    bg_active: rgb(0xd7dbe0),
    selected: rgb(0xd3e3fb),
    fg: rgb(0x383a42),
    fg_bright: rgb(0x1b1d23),
    fg_muted: rgb(0x6b7280),
    dim: rgb(0x9ca3af),
    accent: rgb(0x4078f2),
    warn: rgb(0xd6372c),
    border: rgb(0xcfd3d9),
    hairline: rgb(0xe6e8ec),
    elevated: rgb(0xffffff),
    on_accent: rgb(0xffffff),
    accent_hover: rgb(0x3467d6),
    control_border: rgb(0xc3c8d0),
    scrollbar: rgb(0x9096a1),
    success: rgb(0x50a14f),
    warning: rgb(0xb26f00),
    danger: rgb(0xe45649),
    info: rgb(0x0184bc),
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
    bg_active: rgb(0x504945),
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
    control_border: rgb(0x504945),
    scrollbar: rgb(0x7c6f64),
    success: rgb(0xb8bb26),
    warning: rgb(0xfabd2f),
    danger: rgb(0xfb4934),
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
    bg_active: rgb(0xd5c4a1),
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
    control_border: rgb(0xbdae93),
    scrollbar: rgb(0xa89984),
    success: rgb(0x79740e),
    warning: rgb(0xb57614),
    danger: rgb(0x9d0006),
    info: rgb(0x427b58),
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
    bg_active: rgb(0xddd0b2),
    selected: rgb(0xe4d7ad),
    fg: rgb(0x4a4436),
    fg_bright: rgb(0x2e2a20),
    fg_muted: rgb(0x857a60),
    dim: rgb(0xa99f86),
    accent: rgb(0x3b6ea5),
    warn: rgb(0xa03e3e),
    border: rgb(0xd8ccae),
    hairline: rgb(0xe4dabf),
    elevated: rgb(0xfaf4e2),
    on_accent: rgb(0xf4ecd8),
    accent_hover: rgb(0x305d8c),
    control_border: rgb(0xcabf9f),
    scrollbar: rgb(0xb3a988),
    success: rgb(0x4c6b3c),
    warning: rgb(0xa5762f),
    danger: rgb(0xa03e3e),
    info: rgb(0x4a7a70),
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
    bg_active: rgb(0x3b3228),
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
    control_border: rgb(0x443a2c),
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
    bg_active: rgb(0x251d40),
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
    control_border: rgb(0x352b5a),
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

    pub fn label(self) -> &'static str {
        match self {
            ThemePref::Dark => "Dark",
            ThemePref::Light => "Light",
            ThemePref::System => "System",
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
pub fn bg_active() -> Color {
    active().bg_active
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

/// [`series_color`] for a given syntax palette.
fn series_ink(t: &TokenColors, i: usize) -> u32 {
    let series = [
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

/// [`markdown_settings`] at a given base text size.
pub fn markdown_settings_at(size: f32) -> iced::widget::markdown::Settings {
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

/// A tooltip: the design's dark bubble in either appearance (`tip`), its
/// hairline edge in dark (`tipBorder`) and its shadow (`tipShadow`).
pub fn tip(_theme: &Theme) -> container::Style {
    let light = is_light();
    container::Style {
        background: Some(if light { rgb(0x18181B) } else { rgb(0x2A2A2F) }.into()),
        text_color: Some(tip_text()),
        border: Border {
            color: if light {
                Color::TRANSPARENT
            } else {
                Color::from_rgba(1.0, 1.0, 1.0, 0.08)
            },
            width: 1.0,
            radius: 8.0.into(),
        },
        shadow: iced::Shadow {
            color: scrim(if light { 0.18 } else { 0.5 }),
            offset: iced::Vector::new(0.0, if light { 6.0 } else { 8.0 }),
            blur_radius: if light { 20.0 } else { 24.0 },
        },
        ..container::Style::default()
    }
}

/// A tooltip's words (`tipText`): light on its dark bubble.
pub fn tip_text() -> Color {
    if is_light() {
        rgb(0xFAFAFA)
    } else {
        rgb(0xEDEDEF)
    }
}

/// A key of a tooltip's shortcut (`tipKey`): a wash of white on the bubble.
pub fn tip_key(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(
            Color::from_rgba(1.0, 1.0, 1.0, if is_light() { 0.14 } else { 0.10 }).into(),
        ),
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

/// The face of a field that is really a button (the chat field opens the
/// conversation): no fill of its own, the field around it is the well.
pub fn field_face(_theme: &Theme, status: button::Status) -> button::Style {
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(wash(0.03).into()),
            _ => None,
        },
        text_color: fg_muted(),
        border: Border {
            radius: 8.0.into(),
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

/// One segment of a segmented control: the chosen one raised as a chip,
/// the rest quiet until the pointer is over them.
pub fn segment(chosen: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = if chosen {
            Some(if is_light() { Color::WHITE } else { wash(0.10) }.into())
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => Some(wash(0.05).into()),
                _ => None,
            }
        };
        button::Style {
            background,
            text_color: if chosen { fg_bright() } else { fg_muted() },
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
/// nothing until the pointer is over it; `on` keeps it lit (a menu it
/// opened is showing, a panel it toggles is shown).
pub fn icon_button(on: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| {
        let background = match status {
            button::Status::Pressed => Some(wash(0.11).into()),
            button::Status::Hovered => Some(wash(0.07).into()),
            _ if on => Some(wash(0.08).into()),
            _ => None,
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
            button::Status::Pressed => Some(wash(0.12).into()),
            button::Status::Hovered => Some(wash(0.07).into()),
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

/// The microphone's level while it listens: a bar in the accent color on a
/// faint track.
pub fn level_meter(_theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: iced::Background::Color(with_alpha(danger(), 0.14)),
        bar: iced::Background::Color(danger()),
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: 2.0.into(),
        },
    }
}

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

/// A switch's knob: white, round.
pub fn switch_knob(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color::WHITE.into()),
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

/// Small toolbar button — borderless until hovered, so the toolbar reads as
/// clean text that lights up on interaction rather than a wall of boxes.
pub fn toolbar_button(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => Some(bg_hover().into()),
        button::Status::Pressed => Some(bg_active().into()),
        _ => None,
    };
    button::Style {
        background,
        text_color: match status {
            button::Status::Disabled => dim(),
            _ => fg(),
        },
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Secondary action button — a filled neutral button with a subtle border, for
/// the non-primary choice next to a `primary_button` (e.g. "Open Remote…").
pub fn secondary_button(_theme: &Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => bg_active(),
        _ => bg_hover(),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg_bright(),
        border: Border {
            radius: RADIUS.into(),
            width: 1.0,
            color: active().control_border,
        },
        ..button::Style::default()
    }
}

/// Sidebar tab button (Files / Search).
pub fn tab_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| button::Style {
        background: if active {
            Some(bg().into())
        } else if matches!(status, button::Status::Hovered) {
            Some(bg_hover().into())
        } else {
            None
        },
        text_color: if active { fg_bright() } else { fg_muted() },
        border: Border {
            radius: 5.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
