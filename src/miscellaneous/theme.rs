//! Color palette (One Dark / One Light) and reusable widget styles.
//!
//! Every color the chrome draws with resolves at runtime from the [`active`]
//! palette, chosen by a global mode flag ([`set_light`]). The flag is app-wide
//! and read on the UI thread, so a plain atomic (no lock, no unsafe) is enough:
//! flip it and request a redraw and every window repaints in the new theme.
//! Colors are exposed as accessor functions (`theme::bg()`, `theme::fg()`, …)
//! rather than consts precisely so they can follow that flag.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use iced::widget::{button, container, progress_bar, scrollable};
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
};

/// Every theme clew ships. Dark and light variants coexist; the user picks one
/// of each (see [`set_light_theme`] / [`set_dark_theme`]) and the mode flag
/// chooses between them. Order is stable — ids, not indices, are persisted.
pub static THEMES: &[ThemeDef] = &[
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
];

const DEFAULT_DARK: &str = "one-dark";
const DEFAULT_LIGHT: &str = "one-light";

/// 0 = dark, 1 = light. The three-way preference (Dark/Light/System) resolves to
/// this binary flag whenever it is set (see `apply_pref`).
static MODE: AtomicU8 = AtomicU8::new(0);
/// Which THEMES entry is the chosen light / dark variant. Default to the
/// originals so existing installs are unchanged until the user picks otherwise.
static LIGHT_ID: AtomicUsize = AtomicUsize::new(1); // one-light
static DARK_ID: AtomicUsize = AtomicUsize::new(0); // one-dark

/// Whether a light theme is currently active.
pub fn is_light() -> bool {
    MODE.load(Ordering::Relaxed) == 1
}

/// Switch between the light and dark selection. Request a redraw afterwards.
pub fn set_light(light: bool) {
    MODE.store(u8::from(light), Ordering::Relaxed);
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
    if let Some(i) = theme_index(id, true) {
        LIGHT_ID.store(i, Ordering::Relaxed);
    }
}
pub fn set_dark_theme(id: &str) {
    if let Some(i) = theme_index(id, false) {
        DARK_ID.store(i, Ordering::Relaxed);
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

/// Whether the OS is currently in light appearance. On macOS
/// `AppleInterfaceStyle` reads "Dark" in dark mode and is absent (the command
/// fails) in light mode.
pub fn system_is_light() -> bool {
    #[cfg(target_os = "macos")]
    {
        match std::process::Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
        {
            Ok(o) => !String::from_utf8_lossy(&o.stdout)
                .trim()
                .eq_ignore_ascii_case("dark"),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Point the active palette at what `pref` resolves to right now.
pub fn apply_pref(pref: ThemePref) {
    set_light(pref.resolve_light());
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(crate::lsp::store::data_root()?.join("config.toml"))
}

/// Load persisted appearance settings (mode preference + chosen light/dark
/// themes), apply the theme selections, and return the mode. Called once at
/// startup before the first frame.
pub fn init() -> ThemePref {
    let table: Option<toml::Value> = config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str(&t).ok());
    let get = |key: &str| {
        table
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(|x| x.as_str())
            .map(str::to_string)
    };
    set_light_theme(get("theme_light").as_deref().unwrap_or(DEFAULT_LIGHT));
    set_dark_theme(get("theme_dark").as_deref().unwrap_or(DEFAULT_DARK));
    let pref = get("theme")
        .and_then(|s| ThemePref::parse(&s))
        .unwrap_or(ThemePref::System);
    apply_pref(pref);
    pref
}

/// Persist the appearance settings, preserving other `config.toml` sections.
pub fn save(pref: ThemePref) -> Result<(), String> {
    let path = config_path().ok_or("no data directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    root.insert("theme".into(), toml::Value::String(pref.as_str().into()));
    root.insert(
        "theme_light".into(),
        toml::Value::String(current_light().id.into()),
    );
    root.insert(
        "theme_dark".into(),
        toml::Value::String(current_dark().id.into()),
    );
    let s = toml::to_string(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| e.to_string())
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
pub fn info() -> Color {
    active().info
}
pub fn find() -> Color {
    active().find
}
pub fn selection() -> Color {
    active().selection
}

/// Shared corner radius for buttons / small controls.
pub const RADIUS: f32 = 6.0;

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

/// Status bar background.
pub fn statusbar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        text_color: Some(dim()),
        ..container::Style::default()
    }
}

/// Elevated panel used by the fuzzy-finder modal.
pub fn modal_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(active().elevated.into()),
        text_color: Some(fg()),
        border: Border {
            color: border(),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
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

/// Dimmed backdrop behind the modal.
pub fn backdrop(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(with_alpha(Color::BLACK, 0.45).into()),
        ..container::Style::default()
    }
}

/// Per-pane header strip; the active pane gets an accent-tinted title.
pub fn pane_header(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(bg_panel().into()),
        border: Border {
            color: border(),
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
            ..button::Style::default()
        }
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
        _ => accent(),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: active().on_accent,
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
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
