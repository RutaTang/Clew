//! Color palette (One Dark / One Light) and reusable widget styles.
//!
//! Every color the chrome draws with resolves at runtime from the [`active`]
//! palette, chosen by a global mode flag ([`set_light`]). The flag is app-wide
//! and read on the UI thread, so a plain atomic (no lock, no unsafe) is enough:
//! flip it and request a redraw and every window repaints in the new theme.
//! Colors are exposed as accessor functions (`theme::bg()`, `theme::fg()`, …)
//! rather than consts precisely so they can follow that flag.

use std::sync::atomic::{AtomicU8, Ordering};

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

/// The full set of semantic UI colors for one theme. Two instances exist —
/// [`DARK`] and [`LIGHT`] — and [`active`] returns whichever the mode flag
/// selects.
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

/// One Dark — the original palette.
pub const DARK: Palette = Palette {
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

/// One Light — the paired light palette.
pub const LIGHT: Palette = Palette {
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

/// 0 = dark, 1 = light. The three-way user preference (Dark/Light/System) is
/// resolved to this binary flag whenever it is set (see `prefs`).
static MODE: AtomicU8 = AtomicU8::new(0);

/// Whether the light palette is currently active.
pub fn is_light() -> bool {
    MODE.load(Ordering::Relaxed) == 1
}

/// Switch the active palette. Callers should request a redraw afterwards.
pub fn set_light(light: bool) {
    MODE.store(u8::from(light), Ordering::Relaxed);
}

/// The palette currently in effect.
pub fn active() -> &'static Palette {
    if is_light() { &LIGHT } else { &DARK }
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

/// Load the persisted theme preference (defaults to `System`).
pub fn load_pref() -> ThemePref {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
        .and_then(|v| {
            v.get("theme")
                .and_then(|x| x.as_str())
                .and_then(ThemePref::parse)
        })
        .unwrap_or(ThemePref::System)
}

/// Persist the theme preference, preserving other `config.toml` sections.
pub fn save_pref(pref: ThemePref) -> Result<(), String> {
    let path = config_path().ok_or("no data directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    root.insert("theme".into(), toml::Value::String(pref.as_str().into()));
    let s = toml::to_string(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, s).map_err(|e| e.to_string())
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
