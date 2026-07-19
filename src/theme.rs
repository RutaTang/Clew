//! Color palette (One Dark inspired) and reusable widget styles.

use iced::widget::{button, container, scrollable};
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

// Base UI colors.
pub const BG: Color = rgb(0x282c34); // editor background
pub const BG_PANEL: Color = rgb(0x21252b); // sidebar / toolbar background
pub const BG_HOVER: Color = rgb(0x2d323c); // subtle hover wash
pub const BG_ACTIVE: Color = rgb(0x353c48); // pressed / active control
pub const SELECTED: Color = rgb(0x2f3a4c); // selected list row (accent-tinted)
pub const FG: Color = rgb(0xabb2bf); // primary text
pub const FG_BRIGHT: Color = rgb(0xdfe4ec); // emphasised / selected text
pub const FG_MUTED: Color = rgb(0x828b9c); // secondary text, section labels
pub const DIM: Color = rgb(0x5c6370); // tertiary / disabled
pub const ACCENT: Color = rgb(0x61afef);
pub const BORDER: Color = rgb(0x181a1f);
pub const HAIRLINE: Color = rgb(0x30353f); // 1px dividers

/// Shared corner radius for buttons / small controls.
pub const RADIUS: f32 = 6.0;

pub fn app_theme() -> Theme {
    Theme::custom(
        "clew".to_string(),
        iced::theme::Palette {
            background: BG,
            text: FG,
            primary: ACCENT,
            success: rgb(0x98c379),
            danger: rgb(0xe06c75),
            warning: rgb(0xe5c07b),
        },
    )
}

/// Sidebar / toolbar panel background.
pub fn panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_PANEL.into()),
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// Editor pane background.
pub fn editor(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG.into()),
        text_color: Some(FG),
        ..container::Style::default()
    }
}

/// Status bar background.
pub fn statusbar(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(BG_PANEL.into()),
        text_color: Some(DIM),
        ..container::Style::default()
    }
}

/// A pick_list that blends into the status bar: no boxy border, transparent
/// until hovered, muted text — so it reads like the surrounding status text
/// rather than a heavy boxed control.
pub fn statusbar_picker(
    _theme: &Theme,
    status: iced::widget::pick_list::Status,
) -> iced::widget::pick_list::Style {
    use iced::widget::pick_list::Status;
    let background = match status {
        Status::Hovered | Status::Opened { .. } => BG_HOVER.into(),
        Status::Active => Color::TRANSPARENT.into(),
    };
    iced::widget::pick_list::Style {
        text_color: FG_MUTED,
        placeholder_color: DIM,
        handle_color: FG_MUTED,
        background,
        border: Border { radius: 4.0.into(), width: 0.0, color: Color::TRANSPARENT },
    }
}

/// Elevated panel used by the fuzzy-finder modal.
pub fn modal_panel(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(rgb(0x2c313a).into()),
        text_color: Some(FG),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
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
        background: Some(BG_PANEL.into()),
        border: Border {
            color: BORDER,
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
            Some(SELECTED.into())
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => Some(BG_HOVER.into()),
                _ => None,
            }
        };
        button::Style {
            background,
            text_color: if selected { FG_BRIGHT } else { FG },
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
        background: with_alpha(rgb(0x8a94a6), 0.55).into(),
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
        button::Status::Hovered | button::Status::Pressed => rgb(0x539bd4),
        _ => ACCENT,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: rgb(0x1b1d23),
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
        button::Status::Hovered => Some(BG_HOVER.into()),
        button::Status::Pressed => Some(BG_ACTIVE.into()),
        _ => None,
    };
    button::Style {
        background,
        text_color: match status {
            button::Status::Disabled => DIM,
            _ => FG,
        },
        border: Border {
            radius: RADIUS.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
}

/// Sidebar tab button (Files / Search).
pub fn tab_button(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_theme, status| button::Style {
        background: if active {
            Some(BG.into())
        } else if matches!(status, button::Status::Hovered) {
            Some(BG_HOVER.into())
        } else {
            None
        },
        text_color: if active { FG_BRIGHT } else { FG_MUTED },
        border: Border { radius: 5.0.into(), ..Border::default() },
        ..button::Style::default()
    }
}
