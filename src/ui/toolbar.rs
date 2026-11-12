//! The window's chrome around the code: the top bar (the window buttons,
//! the sidebar toggle, back / forward, and the chat field), the tool strip
//! down the right edge, and their menus — ⋯ (More), the app menu Windows and
//! Linux open from the top bar's ☰, the reader's View menu, and the reading
//! target's drop-up.
//!
//! The geometry is fixed and published here (`TOOLBAR_H`, `STRIP_W`, the
//! strip's slots, the menus' rows, the chat field's place) so the tutorial's
//! spotlight and the floating Ask are placed from the same numbers the
//! chrome is drawn with.
//!
//! One layout on every platform: only the window buttons follow it — macOS's
//! coloured ones on the left, Clew's own on the right on Windows and Linux,
//! where a ☰ takes the top-left corner for the app menu macOS keeps in its
//! menu bar.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::row;

use crate::keymap::{Action, MenuSection};

/// Height of the top bar.
pub(crate) const TOOLBAR_H: f32 = 48.0;
/// Whether the window buttons are macOS's (coloured, top left) rather than
/// Clew's own (top right, with the app menu's ☰ top left).
pub(crate) const MAC_CHROME: bool = cfg!(target_os = "macos");

/// Size of an icon in the chrome.
const ICON: f32 = 16.0;
/// Side of an icon button in the chrome.
pub(crate) const ICON_BTN: f32 = 30.0;
/// The top bar's inset from the window's left edge, before the window
/// buttons (macOS) or the app menu's ☰.
const BAR_PAD_L: f32 = if MAC_CHROME { 16.0 } else { 10.0 };
/// Room after the sidebar toggle, at the end of the top bar's left cluster.
const BAR_CLUSTER_PAD_R: f32 = 12.0;
/// Width of macOS's three window buttons and the gaps between them.
const TRAFFIC_W: f32 = 3.0 * 12.0 + 2.0 * 8.0;
/// Width of back / forward: the room before them, and their gap.
pub(crate) const NAV_W: f32 = NAV_PAD_L + 2.0 * ICON_BTN + 2.0;
const NAV_PAD_L: f32 = 4.0;
/// The search field: widest it grows, its height, and the room kept clear
/// either side of it.
const FIELD_MAX_W: f32 = 460.0;
const FIELD_H: f32 = 34.0;
const FIELD_MARGIN: f32 = 14.0;
/// Width of one of the window buttons of Windows and Linux.
const WIN_BTN_W: f32 = 46.0;
/// The room kept at the top bar's right (macOS), past the search field.
const TOGGLE_SLOT_W: f32 = 44.0;
/// Room before the window buttons (Windows and Linux).
const WIN_BTNS_GAP: f32 = 14.0;

/// Width of the top bar's left cluster: over the sidebar, when it shows, so
/// the sidebar toggle sits at the sidebar's edge; else just its controls.
pub(crate) fn left_cluster_w(app: &App) -> f32 {
    let least = if MAC_CHROME {
        BAR_PAD_L + TRAFFIC_W + 12.0 + ICON_BTN + BAR_CLUSTER_PAD_R
    } else {
        BAR_PAD_L + ICON_BTN + 6.0 + ICON_BTN + BAR_CLUSTER_PAD_R
    };
    if app.show_left_sidebar {
        (app.sidebar_width - SIDEBAR_PAST_BAR).max(least)
    } else {
        least
    }
}

/// How far the sidebar runs past the top bar's cluster over it: the design
/// sets the cluster at 248 and lays the sidebar under it out at 254, the
/// room its six views take. The cluster keeps that short of the sidebar's
/// edge as the sidebar is resized.
const SIDEBAR_PAST_BAR: f32 = 6.0;

/// Width of the top bar's right cluster: room at the corner (macOS), or
/// the window buttons (Windows and Linux).
fn right_cluster_w() -> f32 {
    if MAC_CHROME {
        TOGGLE_SLOT_W
    } else {
        WIN_BTNS_GAP + 3.0 * WIN_BTN_W
    }
}

/// Where the search field is drawn, in window coordinates: centred in the
/// room between the left and right clusters, never wider than
/// [`FIELD_MAX_W`].
pub(crate) fn search_field_rect(app: &App) -> iced::Rectangle {
    let left = left_cluster_w(app) + NAV_W;
    let area = (app.window_width - left - right_cluster_w()).max(0.0);
    let width = (area - 2.0 * FIELD_MARGIN).clamp(0.0, FIELD_MAX_W);
    iced::Rectangle {
        x: left + (area - width) / 2.0,
        y: (TOOLBAR_H - FIELD_H) / 2.0,
        width,
        height: FIELD_H,
    }
}

/// The words of a control that spends an AI call, marked so: a sparkle
/// before them, as every such control has. Links that spent a call looked
/// like the free links beside them.
pub(crate) fn ai_words<'a>(label: &str, size: f32, color: iced::Color) -> Element<'a, Message> {
    // No smaller than 12: at the caption's size it drew as a dot. In a box
    // of its own size, so the words beside it never squeeze it.
    let mark = (size + 1.0).max(12.0);
    // In the accent, whatever colour the words are: in the words' white on
    // Test connection, it was not the mark every other AI control carries.
    // Faded with words that can do nothing now.
    // On the accent itself (a primary button), in the words' colour.
    let tint = if color == theme::dim() || color == theme::on_accent() {
        color
    } else {
        theme::accent()
    };
    iced::widget::row![
        iced::widget::container(glyph::icon(Glyph::Sparkle, tint, mark))
            .width(iced::Length::Fixed(mark))
            .height(iced::Length::Fixed(mark)),
        text(label.to_string()).size(size).color(color),
    ]
    .spacing(4)
    .align_y(iced::Center)
    .into()
}

/// Wrap a bare-icon control with a tooltip naming it and, when it has one,
/// its keyboard shortcut. Icon buttons carry no visible label, so the
/// tooltip is where a reader learns what each one does.
pub(crate) fn chrome_tip<'a>(
    control: impl Into<Element<'a, Message>>,
    name: &'a str,
    shortcut: Option<String>,
) -> Element<'a, Message> {
    tip_at(control, name, shortcut, tooltip::Position::Bottom)
}

/// An action done again and again — refresh, rebuild, expand every branch,
/// close — drawn as its icon and named by its tooltip, where a word would
/// crowd the row. With no message it is drawn, and does nothing.
pub(crate) fn icon_action<'a>(
    glyph: crate::glyph::Glyph,
    name: &'a str,
    message: Option<Message>,
) -> Element<'a, Message> {
    busy_action(glyph, name, message, false)
}

/// [`icon_action`] for work that runs a while: while it is `busy`, its icon
/// turns in its place — words that came in for it moved what stood beside
/// them — and it cannot be pressed again.
pub(crate) fn busy_action<'a>(
    glyph: crate::glyph::Glyph,
    name: &'a str,
    message: Option<Message>,
    busy: bool,
) -> Element<'a, Message> {
    busy_action_tip(glyph, name, name, message, busy)
}

/// [`busy_action`], its tooltip `tip` where it says more than its name
/// (what it costs).
pub(crate) fn busy_action_tip<'a>(
    glyph: crate::glyph::Glyph,
    name: &'a str,
    tip: &'a str,
    message: Option<Message>,
    busy: bool,
) -> Element<'a, Message> {
    let icon = if busy {
        super::meter::spinning(glyph, theme::accent(), 15.0)
    } else {
        crate::glyph::icon(glyph, theme::fg_muted(), 15.0)
    };
    let message = message
        .filter(|_| !busy)
        .map(|m| super::heard::said(name, m));
    chrome_tip(
        button(container(icon).center(Fill))
            .style(theme::icon_button(false))
            .padding(0)
            .width(24)
            .height(24)
            .on_press_maybe(message),
        tip,
        None,
    )
}

/// [`chrome_tip`], on a given side of the control.
pub(crate) fn tip_at<'a>(
    control: impl Into<Element<'a, Message>>,
    name: impl iced::widget::text::IntoFragment<'a>,
    shortcut: Option<String>,
    side: tooltip::Position,
) -> Element<'a, Message> {
    tip_at_why(control, name, shortcut, None, side)
}

/// [`tip_at`] for a control that can do nothing now, with `why` under its
/// name where it says why: a dimmed tool that named only itself was
/// inert with nothing said.
pub(crate) fn tip_at_why<'a>(
    control: impl Into<Element<'a, Message>>,
    name: impl iced::widget::text::IntoFragment<'a>,
    shortcut: Option<String>,
    why: Option<&'a str>,
    side: tooltip::Position,
) -> Element<'a, Message> {
    // Its words centred on its keys' row, as the design's flex row sets
    // them; with no keys, on a line of their own.
    let line = if shortcut.is_some() {
        TIP_KEY_H
    } else {
        ts::INTER.normal(TIP_SIZE)
    };
    let mut body = row![ts::set_in(
        text(name)
            .font(crate::fonts::UI_MEDIUM)
            .color(theme::tip_text()),
        line,
        ts::INTER,
        TIP_SIZE,
    )]
    .spacing(8)
    .align_y(iced::Center);
    if let Some(sc) = shortcut {
        body = body.push(tip_keys(&sc));
    }
    let body: Element<'a, Message> = match why {
        Some(why) => iced::widget::column![
            body,
            text(why)
                .size(TIP_SIZE)
                .color(theme::dim())
                .wrapping(iced::widget::text::Wrapping::Word),
        ]
        .spacing(4)
        .max_width(TIP_WRAP_W)
        .into(),
        None => body.into(),
    };
    // The design's padding, inside the border drawn over the bubble's edge.
    let bubble = container(body)
        .padding(Padding {
            top: 7.0,
            right: 9.0,
            bottom: 7.0,
            left: 11.0,
        })
        .style(theme::tip);
    // No padding of the tooltip's own (iced's is 5) round the bubble: it
    // set the bubble 5 further from what it names than the design's 10.
    super::tip::tip(control, bubble, side).gap(TIP_GAP).into()
}

/// [`tip_at`] for words that may run long — a value, a path: wrapped, in a
/// bubble no wider than [`TIP_WRAP_W`]. On one line, a long value ran off
/// the window's edge, and the whole of it the tip was for was not seen.
pub(crate) fn tip_wrapped<'a>(
    control: impl Into<Element<'a, Message>>,
    words: String,
    side: tooltip::Position,
) -> Element<'a, Message> {
    let bubble = container(
        text(words)
            .size(TIP_SIZE)
            .font(crate::fonts::UI_MEDIUM)
            .color(theme::tip_text())
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph),
    )
    .max_width(TIP_WRAP_W)
    .padding(Padding {
        top: 7.0,
        right: 9.0,
        bottom: 7.0,
        left: 11.0,
    })
    .style(theme::tip);
    super::tip::tip(control, bubble, side).gap(TIP_GAP).into()
}

/// The widest a [`tip_wrapped`] bubble grows before its words wrap.
pub(crate) const TIP_WRAP_W: f32 = 520.0;

/// A tooltip's shortcut, as its keys: each a cap of its own on the bubble,
/// the design's 17 wide at least and 18 high, 4 either side of its words.
fn tip_keys<'a>(caps: &str) -> Element<'a, Message> {
    let cap = |key: String| -> Element<'a, Message> {
        let width = (text_width(&key, ts::CAPTION, crate::fonts::UI_MEDIUM) + 2.0 * TIP_KEY_PAD)
            .max(TIP_KEY_MIN_W);
        container(ts::set_in(
            text(key)
                .font(crate::fonts::UI_MEDIUM)
                .color(theme::tip_text()),
            TIP_KEY_H,
            ts::INTER,
            ts::CAPTION,
        ))
        .width(Length::Fixed(width))
        .center_x(Length::Fixed(width))
        .style(theme::tip_key)
        .into()
    };
    // Two shortcuts for one command, as a key cap shows them.
    let mut row = Row::new().spacing(3).align_y(iced::Center);
    for (n, alternative) in caps.split(crate::keymap::CAPS_OR).enumerate() {
        if n > 0 {
            row = row.push(
                text("/")
                    .size(ts::CAPTION)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::tip_text()),
            );
        }
        for key in split_caps(alternative) {
            row = row.push(cap(key));
        }
    }
    row.into()
}

/// A tooltip's distance from what it names: the design's 10.
const TIP_GAP: f32 = 10.0;
/// A tooltip key's height, least width, and room either side of its words.
const TIP_KEY_H: f32 = 18.0;
const TIP_KEY_MIN_W: f32 = 17.0;
const TIP_KEY_PAD: f32 = 4.0;

/// Height of a key cap, and its least width.
const KEYCAP_H: f32 = 19.0;
const KEYCAP_MIN_W: f32 = 18.0;
/// Size of a tooltip's words.
const TIP_SIZE: f32 = 12.0;

/// A shortcut on a control (the chat field), drawn as its keys — one cap
/// for each (⌘ ⇧ A, or Ctrl Shift A) — whose words keep the face's own
/// spacing (`fonts::in_control`).
fn control_keycap<'a>(caps: String) -> Element<'a, Message> {
    caps_in(
        caps,
        crate::fonts::in_control(crate::fonts::UI_MEDIUM),
        CapSize::FIELD,
    )
}

/// A shortcut drawn as its keys, at the size the design gives the keys of a
/// row of the command bar.
pub(crate) fn row_keycap<'a>(caps: String) -> Element<'a, Message> {
    caps_in(caps, crate::fonts::UI_MEDIUM, CapSize::ROW)
}

/// A size of the design's caps: what they say, how tall they are, and
/// how narrow they may be.
#[derive(Clone, Copy)]
struct CapSize {
    text: f32,
    height: f32,
    least: f32,
}

impl CapSize {
    /// In the chat field and the field while listening.
    const FIELD: Self = Self {
        text: ts::CAPTION,
        height: KEYCAP_H,
        least: KEYCAP_MIN_W,
    };
    /// In a row of the command bar.
    const ROW: Self = Self {
        text: 11.0,
        height: 20.0,
        least: 19.0,
    };
}

fn caps_in<'a>(caps: String, font: iced::Font, size: CapSize) -> Element<'a, Message> {
    // The design's cap: 5 either side of its words inside its 1 px edge
    // (drawn inside the box by iced), as wide as `size` lets it be at
    // least, its words centred.
    let cap = |key: String| -> Element<'a, Message> {
        let width = (text_width(&key, size.text, font) + 2.0 * (5.0 + 1.0)).max(size.least);
        container(ts::set_in(
            text(key).font(font).color(theme::fg_muted()),
            size.height,
            ts::INTER,
            size.text,
        ))
        .center_x(Length::Fixed(width))
        .style(theme::keycap)
        .into()
    };
    // Two shortcuts for one command ("Ctrl+L / Ctrl+G"): each as its keys,
    // a "/" between them. It was one cap of words, beside rows and a
    // search field whose keys were caps of their own.
    let mut row = Row::new().spacing(3).align_y(iced::Center);
    for (n, alternative) in caps.split(crate::keymap::CAPS_OR).enumerate() {
        if n > 0 {
            row = row.push(text("/").size(size.text).font(font).color(theme::dim()));
        }
        for key in split_caps(alternative) {
            row = row.push(cap(key));
        }
    }
    row.into()
}

/// The keys of a shortcut as [`Chord::caps`](crate::keymap::Chord::caps)
/// spells it, each a cap of its own: "Ctrl+Shift+A" and "⌘⇧A" are three
/// keys, "Ctrl++" two (the second the plus key). A shortcut of several
/// strokes ("g d") is left whole, as one cap.
pub(crate) fn split_caps(caps: &str) -> Vec<String> {
    if caps.contains(' ') || caps.chars().count() <= 1 {
        return vec![caps.to_string()];
    }
    let mac = |c: char| matches!(c, '⌘' | '⇧' | '⌥' | '⌃');
    if !caps.starts_with(mac) && caps.contains('+') {
        let mut keys: Vec<String> = caps
            .split('+')
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        // "Ctrl++": the key is the plus itself.
        if caps.ends_with('+') {
            keys.push("+".into());
        }
        return keys;
    }
    let mut keys: Vec<String> = Vec::new();
    let mut rest = String::new();
    for c in caps.chars() {
        if rest.is_empty() && mac(c) {
            keys.push(c.to_string());
        } else {
            rest.push(c);
        }
    }
    if !rest.is_empty() {
        keys.push(rest);
    }
    keys
}

/// A borderless icon button of the chrome.
/// How strongly a strip control that can do nothing now is drawn.
pub(crate) const DISABLED_OPACITY: f32 = 0.35;

fn icon_button<'a>(glyph: Glyph, color: iced::Color, on: bool) -> Press<'a> {
    strip_button(glyph, color, on, true)
}

/// A strip control: `glyph` in `color`, lit when `on`, faded when it can
/// do nothing (`able` false).
fn strip_button<'a>(glyph: Glyph, color: iced::Color, on: bool, able: bool) -> Press<'a> {
    let mark = if able {
        glyph::icon(glyph, color, ICON)
    } else {
        glyph::icon_faded(glyph, color, ICON, DISABLED_OPACITY)
    };
    button(
        container(mark)
            .width(Fill)
            .height(Fill)
            .align_x(iced::Center)
            .align_y(iced::Center),
    )
    .style(theme::icon_button(on))
    .padding(0)
    .width(Length::Fixed(ICON_BTN))
    .height(Length::Fixed(ICON_BTN))
}

pub(crate) fn toolbar(app: &App) -> Element<'_, Message> {
    let caps = |action: Action| Some(app.keymap.chord(action).caps());

    // Left: the window buttons (macOS) or the app menu (Windows, Linux),
    // and the sidebar toggle at the sidebar's edge.
    let corner: Element<'_, Message> = if MAC_CHROME {
        traffic_lights(app)
    } else {
        chrome_tip(
            icon_button(Glyph::Menu, theme::fg_muted(), app.show_app_menu).on_press(
                super::heard::said(tr!("Menu"), Message::Window(WindowMsg::ToggleAppMenu)),
            ),
            tr!("Menu"),
            None,
        )
    };
    let sidebar_toggle = chrome_tip(
        icon_button(
            Glyph::PanelLeft,
            if app.show_left_sidebar {
                theme::fg_muted()
            } else {
                theme::dim()
            },
            false,
        )
        .on_press(super::heard::said(
            tr!("Toggle sidebar"),
            Message::Window(WindowMsg::ToggleLeftSidebar),
        )),
        tr!("Toggle sidebar"),
        None,
    );
    let left = container(
        row![corner, space().width(Fill), sidebar_toggle]
            .align_y(iced::Center)
            .padding(Padding {
                left: BAR_PAD_L,
                right: BAR_CLUSTER_PAD_R,
                ..Padding::ZERO
            }),
    )
    .width(Length::Fixed(left_cluster_w(app)));

    // Back / forward along the reading trail.
    let nav = |glyph: Glyph, enabled: bool, name: &str, msg: Message| {
        let mut b = icon_button(
            glyph,
            if enabled {
                theme::fg_muted()
            } else {
                theme::dim()
            },
            false,
        );
        if enabled {
            b = b.on_press(super::heard::said(name, msg));
        }
        b
    };
    let nav = container(
        row![
            chrome_tip(
                nav(
                    Glyph::ChevronLeft,
                    app.proj.history.can_back(),
                    tr!("Back"),
                    Message::Reading(ReadingMsg::GoBack)
                ),
                tr!("Back"),
                caps(Action::GoBack),
            ),
            chrome_tip(
                nav(
                    Glyph::ChevronRight,
                    app.proj.history.can_forward(),
                    tr!("Forward"),
                    Message::Reading(ReadingMsg::GoForward)
                ),
                tr!("Forward"),
                caps(Action::GoForward),
            ),
        ]
        .spacing(2)
        .align_y(iced::Center),
    )
    .padding(iced::padding::left(NAV_PAD_L))
    .width(Length::Fixed(NAV_W));

    let field = container(search_field(app))
        .width(Fill)
        .align_x(iced::Center)
        .padding([0.0, FIELD_MARGIN]);

    // Right: room at the corner (macOS), or the window buttons. The margin
    // is shown and hidden from the View menu, where it is.
    let right: Element<'_, Message> = if MAC_CHROME {
        space().width(Length::Fixed(TOGGLE_SLOT_W)).into()
    } else {
        container(window_buttons(app))
            .padding(iced::padding::left(WIN_BTNS_GAP))
            .into()
    };

    // A fixed bar height, its content centred. The whole bar is the window's
    // drag region; its buttons capture their own clicks, so only its empty
    // parts start a drag.
    mouse_area(
        container(
            row![left, nav, field, right]
                .height(Fill)
                .align_y(iced::Center),
        )
        .width(Fill)
        .height(Length::Fixed(TOOLBAR_H))
        .style(theme::chrome),
    )
    .on_press(Message::Window(WindowMsg::TitleBarDragged))
    .into()
}

/// macOS's window buttons, drawn by Clew (the window is frameless): red,
/// amber and green, showing their glyphs while the pointer is over the
/// cluster, grey while the window has no focus. Being real buttons, they
/// take their own clicks, so a drag from them never moves the window.
pub(crate) fn traffic_lights(app: &App) -> Element<'_, Message> {
    let show_icon = app.controls_hovered;
    let colored = app.window_focused || app.controls_hovered;
    let light = move |color: iced::Color, icon: TrafficIcon, label: &str, msg: Message| {
        let content: Element<'_, Message> = if show_icon {
            iced::widget::canvas::Canvas::new(TrafficGlyph {
                icon,
                color: theme::TRAFFIC_GLYPH,
            })
            .width(12)
            .height(12)
            .into()
        } else {
            space().width(12).height(12).into()
        };
        button(content)
            .style(move |_theme, status: button::Status| {
                let bg = if !colored {
                    theme::TRAFFIC_INACTIVE
                } else {
                    match status {
                        // Native traffic lights keep full colour on hover (only
                        // the glyph appears); they darken only on a press.
                        button::Status::Pressed => theme::with_alpha(color, 0.82),
                        _ => color,
                    }
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..button::Style::default()
                }
            })
            .padding(0)
            .on_press(super::heard::said(label, msg))
    };
    let fullscreen = if app.fullscreen {
        tr!("Exit Full Screen")
    } else {
        tr!("Full Screen")
    };
    // No tooltips: native traffic lights show only their glyph on hover.
    mouse_area(
        row![
            light(
                theme::TRAFFIC_CLOSE,
                TrafficIcon::Close,
                tr!("Close"),
                Message::Window(WindowMsg::Close)
            ),
            light(
                theme::TRAFFIC_MINIMIZE,
                TrafficIcon::Minimize,
                tr!("Minimize"),
                Message::Window(WindowMsg::Minimize)
            ),
            light(
                theme::TRAFFIC_ZOOM,
                TrafficIcon::Fullscreen(app.fullscreen),
                fullscreen,
                Message::Window(WindowMsg::ToggleFullscreen)
            ),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .on_enter(Message::Window(WindowMsg::ControlsHover(true)))
    .on_exit(Message::Window(WindowMsg::ControlsHover(false)))
    .into()
}

/// The window buttons of Windows and Linux: minimize, maximize (restore
/// once maximized), close — full height at the top-right corner, where
/// those systems put them.
fn window_buttons(app: &App) -> Element<'_, Message> {
    let key = |glyph: WindowGlyph, close: bool, label: &'static str, msg: Message| {
        let face = move |color: iced::Color| -> Element<'static, Message> {
            container(glyph::window_icon(glyph, color, 10.0))
                .width(Fill)
                .height(Fill)
                .align_x(iced::Center)
                .align_y(iced::Center)
                .into()
        };
        chrome_tip(
            button(face(theme::fg()))
                .style(theme::window_button(close))
                .padding(0)
                .width(Length::Fixed(WIN_BTN_W))
                .height(Fill)
                .on_press(super::heard::said(label, msg)),
            label,
            None,
        )
    };
    let (max_glyph, max_label) = if app.maximized {
        (WindowGlyph::Restore, tr!("Restore"))
    } else {
        (WindowGlyph::Maximize, tr!("Maximize"))
    };
    row![
        key(
            WindowGlyph::Minimize,
            false,
            tr!("Minimize"),
            Message::Window(WindowMsg::Minimize)
        ),
        key(
            max_glyph,
            false,
            max_label,
            Message::Window(WindowMsg::ToggleMaximize)
        ),
        key(
            WindowGlyph::Close,
            true,
            tr!("Close"),
            Message::Window(WindowMsg::Close)
        ),
    ]
    .height(Fill)
    .into()
}

/// The search field in the top bar: files, symbols and — typed after `>`
/// — commands, in the finder it opens. Ask and voice are not here: they
/// live in the window's corner ([`super::askfloat`]).
fn search_field(app: &App) -> Element<'_, Message> {
    let rect = search_field_rect(app);
    let chord = app.keymap.chord(Action::OpenFile).caps();
    let face = button(
        row![
            glyph::icon(Glyph::Search, theme::fg_muted(), 15.0),
            // Cut to the room the field leaves it, with an ellipsis: clipped,
            // it stopped mid-word at its edge.
            container(iced::widget::responsive(|room| {
                let label = tr!("Search files, symbols and commands");
                ts::set_in(
                    text(super::steady::cut(
                        label,
                        room.width,
                        ts::BASE,
                        crate::fonts::UI,
                    ))
                    .font(crate::fonts::in_control(crate::fonts::UI))
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
                    FIELD_H - 2.0,
                    ts::INTER,
                    ts::BASE,
                )
            }))
            .width(Fill)
            .height(Length::Fixed(FIELD_H - 2.0))
            .clip(true),
            control_keycap(chord),
        ]
        .spacing(9)
        .height(Fill)
        .align_y(iced::Center),
    )
    // The whole field is the button, so the pointer shades all of it: the
    // design's 12 before and 6 after, inside its 1 px edge (which iced draws
    // inside the box).
    .style(theme::field_button)
    .padding(Padding {
        top: 1.0,
        right: 6.0 + 1.0,
        bottom: 1.0,
        left: 12.0 + 1.0,
    })
    .width(Length::Fixed(rect.width))
    .height(Length::Fixed(FIELD_H))
    .on_press(super::heard::said(
        tr!("Search files, symbols and commands"),
        Message::Nav(NavMsg::FinderOpened(FinderMode::Files)),
    ));
    face.into()
}

// ---------------------------------------------------------------- tool strip

/// Width of the tool strip down the window's right edge.
pub(crate) const STRIP_W: f32 = 44.0;
/// Space above the strip's first tool, and below its last.
const STRIP_PAD_TOP: f32 = 2.0;
const STRIP_PAD_BOTTOM: f32 = 10.0;
/// Gap between the strip's slots.
const STRIP_SPACING: f32 = 2.0;
/// Height of the slot a group rule stands in (the rule itself is 1px).
const STRIP_RULE_H: f32 = 11.0;

/// A tool on the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripTool {
    Overview,
    Stats,
    /// The weak spots page.
    WeakSpots,
    /// The program run to its end, for real.
    Run,
    /// A function worked out by the model to its end, on inputs given.
    FakeRun,
    /// The program run under the debugger.
    Debug,
    /// A function worked out by the model, stepped through from its first line.
    FakeDebug,
    /// The four above in one menu, from the bug: where the strip is too
    /// short for them ([`strip_slots`]).
    RunMenu,
    CallGraph,
    ImportGraph,
    TypeMap,
    ProjectMaps,
    /// The ⋯ menu.
    More,
    /// At the strip's foot.
    Settings,
}

/// The strip's tools from the top, in their groups, in the order a project
/// is come to know: the whole of it (Overview, Stats, its weak spots), how its parts
/// connect (its graphs and maps), running it ([`RUN_TOOLS`]) — and More. A
/// rule stands before each group but the first ([`STRIP_RULE_BEFORE`]). The
/// strip holds what opens over the code or below it; what opens in the
/// sidebar is the sidebar's to show ([`super::SIDEBAR_TABS`]). Settings
/// stands apart, at the strip's foot.
pub(crate) const STRIP_TOP: [StripTool; 12] = [
    StripTool::Overview,
    StripTool::Stats,
    StripTool::WeakSpots,
    StripTool::CallGraph,
    StripTool::ImportGraph,
    StripTool::TypeMap,
    StripTool::ProjectMaps,
    StripTool::Run,
    StripTool::FakeRun,
    StripTool::Debug,
    StripTool::FakeDebug,
    StripTool::More,
];
/// The [`STRIP_TOP`] positions a group rule stands before.
const STRIP_RULE_BEFORE: [usize; 3] = [3, 7, 11];
/// The four ways to run, each a button of its own: for real or worked out
/// by the model, each beside its twin. Behind one bug, Fake Run and Fake
/// Debug were not found. Where the strip is too short for all four, they
/// fold back into the bug's menu ([`StripTool::RunMenu`]).
pub(crate) const RUN_TOOLS: [StripTool; 4] = [
    StripTool::Run,
    StripTool::FakeRun,
    StripTool::Debug,
    StripTool::FakeDebug,
];

/// Where the strip's body starts: under the top bar and the update banner.
pub(crate) fn body_top(app: &App) -> f32 {
    TOOLBAR_H + update_banner_height(app)
}

/// One slot down the strip: a tool, a row of the ⋯ menu laid on the strip,
/// or a group's rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripSlot {
    Tool(StripTool),
    Row(ToolsRow),
    Rule,
}

/// The least room left between the strip's last tool and Settings at its
/// foot.
const STRIP_FOOT_GAP: f32 = 12.0;

/// The strip's slots from the top (Settings, at its foot, apart). Where the
/// strip is tall enough, the ⋯ menu's rows are laid on it too, in their
/// groups, and the ⋯ is not there: a menu of what the strip had room to
/// show was a click more for each of them. Shorter, they stay in the menu;
/// shorter still, the four ways to run fold into the bug's menu too.
pub(crate) fn strip_slots(app: &App) -> Vec<StripSlot> {
    let top = |with_more: bool, runs_folded: bool| {
        let mut slots = Vec::new();
        for (i, t) in STRIP_TOP.iter().enumerate() {
            if *t == StripTool::More && !with_more {
                continue;
            }
            let t = match *t {
                StripTool::Run if runs_folded => StripTool::RunMenu,
                t if runs_folded && RUN_TOOLS.contains(&t) => continue,
                t => t,
            };
            if STRIP_RULE_BEFORE.contains(&i) {
                slots.push(StripSlot::Rule);
            }
            slots.push(StripSlot::Tool(t));
        }
        slots
    };
    let mut inline = top(false, false);
    for (i, row) in TOOLS_MENU_ROWS.iter().enumerate() {
        if i == 0 || TOOLS_MENU_SEPARATORS.contains(&i) {
            inline.push(StripSlot::Rule);
        }
        inline.push(StripSlot::Row(*row));
    }
    let height = |slots: &[StripSlot]| {
        slots
            .iter()
            .map(|s| match s {
                StripSlot::Rule => STRIP_RULE_H,
                _ => ICON_BTN,
            })
            .sum::<f32>()
            + slots.len().saturating_sub(1) as f32 * STRIP_SPACING
    };
    let room = app.window_height
        - body_top(app)
        - STATUSBAR_H
        - STRIP_PAD_TOP
        - STRIP_PAD_BOTTOM
        - ICON_BTN
        - STRIP_FOOT_GAP;
    if height(&inline) <= room {
        return inline;
    }
    let with_runs = top(true, false);
    if height(&with_runs) <= room {
        with_runs
    } else {
        top(true, true)
    }
}

/// Whether the four ways to run are buttons on the strip, not the bug's
/// menu (see [`strip_slots`]).
pub(crate) fn runs_on_strip(app: &App) -> bool {
    strip_slots(app).contains(&StripSlot::Tool(StripTool::Run))
}

/// Whether the ⋯ menu's rows are on the strip (see [`strip_slots`]).
pub(crate) fn tools_on_strip(app: &App) -> bool {
    strip_slots(app)
        .iter()
        .any(|s| matches!(s, StripSlot::Row(_)))
}

/// Centre of a slot down the strip, in window coordinates; `None` when it
/// is not there (the ⋯ while the menu's rows are on the strip, a row while
/// they are in the menu).
pub(crate) fn strip_slot_center(app: &App, slot: StripSlot) -> Option<iced::Point> {
    let x = app.window_width - STRIP_W / 2.0;
    if slot == StripSlot::Tool(StripTool::Settings) {
        let y = app.window_height - STATUSBAR_H - STRIP_PAD_BOTTOM - ICON_BTN / 2.0;
        return Some(iced::Point::new(x, y));
    }
    let mut y = body_top(app) + STRIP_PAD_TOP;
    for s in strip_slots(app) {
        let h = match s {
            StripSlot::Rule => STRIP_RULE_H,
            _ => ICON_BTN,
        };
        if s == slot {
            return Some(iced::Point::new(x, y + h / 2.0));
        }
        y += h + STRIP_SPACING;
    }
    None
}

/// Centre of a strip tool, in window coordinates. The ⋯, while the menu's
/// rows are on the strip in its place, is where the first of them is.
pub(crate) fn strip_tool_center(app: &App, tool: StripTool) -> iced::Point {
    strip_slot_center(app, StripSlot::Tool(tool))
        .or_else(|| strip_slot_center(app, StripSlot::Row(TOOLS_MENU_ROWS[0])))
        .unwrap_or_else(|| iced::Point::new(app.window_width - STRIP_W / 2.0, body_top(app)))
}

/// What a tool of the strip shows and does: its icon, its name, its
/// shortcut, and its message.
pub(crate) fn strip_tool(
    app: &App,
    tool: StripTool,
) -> (Glyph, &'static str, Option<String>, Message) {
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    match tool {
        StripTool::Overview => (
            Glyph::Layout,
            tr!("Architecture Overview"),
            caps(Action::Overview),
            Message::Overview(OverviewMsg::Show),
        ),
        StripTool::Stats => (
            Glyph::Stats,
            tr!("Code Statistics"),
            caps(Action::Stats),
            Message::Overview(OverviewMsg::ShowStats),
        ),
        StripTool::WeakSpots => (
            Glyph::WeakSpots,
            tr!("Weak Spots"),
            caps(Action::WeakSpots),
            Message::Weak(crate::WeakMsg::Show),
        ),
        // Running it, four ways: for real or worked out by the model, run
        // to its end or stepped through. Each is named as the menu bar
        // names it.
        StripTool::Run => (
            Glyph::Play,
            tr!("Run Without Debugging"),
            caps(Action::Run),
            Message::Debug(DebugMsg::Run),
        ),
        StripTool::FakeRun => (
            Glyph::FakeRun,
            tr!("Fake Run…"),
            caps(Action::FakeRun),
            Message::Fake(crate::FakeMsg::AtCaret { debug: false }),
        ),
        StripTool::Debug => (
            Glyph::Debug,
            tr!("Start Debugging"),
            caps(Action::StartDebug),
            Message::Debug(DebugMsg::Start),
        ),
        StripTool::FakeDebug => (
            Glyph::FakeDebug,
            tr!("Fake Debug…"),
            caps(Action::FakeDebug),
            Message::Fake(crate::FakeMsg::AtCaret { debug: true }),
        ),
        // The four in one menu, where the strip is short.
        StripTool::RunMenu => (
            Glyph::Debug,
            tr!("Run and Debug"),
            None,
            Message::Window(WindowMsg::ToggleRunMenu),
        ),
        StripTool::CallGraph => (
            Glyph::Network,
            tr!("Call Graph"),
            caps(Action::CallGraph),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectCalls)),
        ),
        StripTool::ImportGraph => (
            Glyph::ImportGraph,
            tr!("Import Graph"),
            caps(Action::ImportGraph),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectImports)),
        ),
        StripTool::TypeMap => (
            Glyph::TypeMap,
            tr!("Type Graph"),
            caps(Action::TypeGraph),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectTypes)),
        ),
        StripTool::ProjectMaps => (
            Glyph::ProjectMaps,
            tr!("Project Graphs"),
            caps(Action::ProjectMaps),
            Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectMaps)),
        ),
        StripTool::More => (
            Glyph::More,
            tr!("More"),
            None,
            Message::Window(WindowMsg::ToggleToolsMenu),
        ),
        StripTool::Settings => (
            Glyph::Settings,
            tr!("Settings"),
            Some(crate::keymap::local_keys("⌘,")),
            Message::Settings(SettingsMsg::Open),
        ),
    }
}

/// Whether a tool of the strip can do anything now. What reads a project
/// does nothing with none open: drawn so, and pressed, nothing — they
/// looked as ready as the rest. Fake Run and Fake Debug work out code in
/// view, so they need a file open, as their rows in the bug's menu did.
pub(crate) fn strip_tool_able(app: &App, tool: StripTool) -> bool {
    match tool {
        StripTool::More | StripTool::Settings => true,
        StripTool::FakeRun | StripTool::FakeDebug => app.active_viewer().is_some(),
        _ => app.proj.project.is_some(),
    }
}

/// Why a tool of the strip can do nothing now ([`strip_tool_able`]), said
/// where the pointer rests on it.
pub(crate) fn strip_tool_why(app: &App, tool: StripTool) -> Option<&'static str> {
    if strip_tool_able(app, tool) {
        return None;
    }
    Some(match tool {
        StripTool::FakeRun => {
            tr!("Open a file and put the caret in a function, class or struct to fake-run it")
        }
        StripTool::FakeDebug => {
            tr!("Open a file and put the caret in a function, class or struct to fake-debug it")
        }
        _ => tr!("Open a folder first"),
    })
}

/// The tool strip: the project's tools down the window's right edge, each
/// naming itself (and its shortcut) to the left on hover.
pub(crate) fn tool_strip(app: &App) -> Element<'_, Message> {
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    let tool = |tool: StripTool| -> Element<'_, Message> {
        let (glyph, label, shortcut, msg) = strip_tool(app, tool);
        // Its menu open, the ⋯ is lit: drawn in the text's colour. So is
        // the bug, its Run menu open.
        let on = (tool == StripTool::More && app.show_tools_menu)
            || (tool == StripTool::RunMenu && app.show_run_menu);
        let tint = if on { theme::fg() } else { theme::fg_muted() };
        let able = strip_tool_able(app, tool);
        tip_at_why(
            strip_button(glyph, tint, on, able)
                .on_press_maybe(able.then(|| super::heard::said(label, msg))),
            label,
            shortcut,
            strip_tool_why(app, tool),
            tooltip::Position::Left,
        )
    };
    let rule = || {
        container(
            container(space())
                .width(Length::Fixed(16.0))
                .height(Length::Fixed(1.0))
                .style(|_: &iced::Theme| container::Style {
                    background: Some(theme::hairline().into()),
                    ..container::Style::default()
                }),
        )
        .height(Length::Fixed(STRIP_RULE_H))
        .align_y(iced::Center)
    };
    // As wide as the strip, so its tools are centred in it (a column only
    // as wide as its tools was laid at the strip's left edge).
    let mut strip = Column::new()
        .width(Fill)
        .spacing(STRIP_SPACING)
        .align_x(iced::Center);
    for slot in strip_slots(app) {
        strip = strip.push(match slot {
            StripSlot::Rule => rule().into(),
            StripSlot::Tool(t) => tool(t),
            StripSlot::Row(row) => {
                let (glyph, label, msg) = tools_row(app, row);
                tip_at(
                    strip_button(glyph, theme::fg_muted(), false, msg.is_some())
                        .on_press_maybe(msg.map(|m| super::heard::said(&label, m))),
                    label,
                    caps(tools_row_action(row)),
                    tooltip::Position::Left,
                )
            }
        });
    }
    strip = strip
        .push(space().height(Fill))
        .push(tool(StripTool::Settings));
    container(strip.padding(Padding {
        top: STRIP_PAD_TOP,
        bottom: STRIP_PAD_BOTTOM,
        ..Padding::ZERO
    }))
    .width(Length::Fixed(STRIP_W))
    .height(Fill)
    .style(theme::strip)
    .into()
}

// ------------------------------------------------------------------ ⋯ menu

/// A row of the ⋯ menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolsRow {
    ExplainAll,
    Glossary,
    SetUp,
    Tutorial,
}

/// The ⋯ menu's rows, top to bottom, in two groups: understanding the
/// project, and help. Each tool is where what it opens is: walkthroughs and
/// the reader's notes (and their export) in the sidebar, opening another
/// project in the app menu, the language servers on the status bar and in
/// Settings, the keyboard shortcuts in the app menu and on ⌘/ — none of
/// them here. The menu is built from this list, and the
/// tutorial points at rows by their position in it — so the two cannot
/// drift.
pub(crate) const TOOLS_MENU_ROWS: [ToolsRow; 4] = [
    ToolsRow::ExplainAll,
    ToolsRow::Glossary,
    ToolsRow::SetUp,
    ToolsRow::Tutorial,
];

/// The [`TOOLS_MENU_ROWS`] positions a separator stands before.
pub(crate) const TOOLS_MENU_SEPARATORS: [usize; 1] = [3];
/// The [`TOOLS_MENU_ROWS`] positions a heading stands over, and what it
/// says: the group of rows it names.
pub(crate) const TOOLS_MENU_HEADINGS: [(usize, &str); 2] = [
    (0, tr_noop!("Understand the project")),
    (3, tr_noop!("Help")),
];

impl ToolsRow {
    /// This row's position in [`TOOLS_MENU_ROWS`] (0 = top).
    pub(crate) const fn index(self) -> usize {
        let mut i = 0;
        while i < TOOLS_MENU_ROWS.len() {
            if TOOLS_MENU_ROWS[i] as u8 == self as u8 {
                return i;
            }
            i += 1;
        }
        // Every variant is listed (a test enforces it); unreachable otherwise.
        TOOLS_MENU_ROWS.len()
    }
}

/// Width of the ⋯ menu's panel as designed.
const TOOLS_MENU_DESIGN_W: f32 = 248.0;

/// Width of a menu panel holding `rows` (each a label and the width of what
/// trails it) and `headings`: as designed, or wider when a label in the
/// reader's language needs it, so no row wraps.
fn menu_w(design: f32, rows: &[(&str, f32)], headings: &[&str]) -> f32 {
    // A row: its padding, its icon's gutter and the gaps around the label.
    const ROW_CHROME: f32 = 2.0 * 9.0 + 15.0 + 3.0 * 10.0 + 2.0 * MENU_PAD;
    let label = |l: &str| super::sheet::text_width(l, ts::BASE, crate::fonts::UI);
    let widest_row = rows
        .iter()
        .map(|(l, trailing)| label(l) + trailing + ROW_CHROME)
        .fold(0.0, f32::max);
    let widest_heading = headings
        .iter()
        .map(|h| {
            super::sheet::text_width(h, ts::SMALL, crate::fonts::UI_MEDIUM)
                + 2.0 * 9.0
                + 2.0 * MENU_PAD
        })
        .fold(0.0, f32::max);
    design.max(widest_row).max(widest_heading).ceil()
}

/// The action a ⋯ menu row runs, whose shortcut is shown on the row and in
/// its tip on the strip: every row has one.
pub(crate) fn tools_row_action(row: ToolsRow) -> Action {
    match row {
        ToolsRow::ExplainAll => Action::ExplainAll,
        ToolsRow::Glossary => Action::Glossary,
        ToolsRow::SetUp => Action::SetUpProject,
        ToolsRow::Tutorial => Action::Tutorial,
    }
}

/// What a ⋯ menu row says.
pub(crate) fn tools_row_label(row: ToolsRow) -> &'static str {
    match row {
        ToolsRow::ExplainAll => tr!("Explain All…"),
        ToolsRow::Glossary => tr!("Glossary"),
        ToolsRow::SetUp => tr!("Set Up Project…"),
        ToolsRow::Tutorial => tr!("Tutorial"),
    }
}

/// What the ⋯ menu's Explain All row says while a pass runs, `progress`
/// being how far it has got (done, of total).
fn cancel_explaining_label(progress: Option<(usize, usize)>) -> String {
    match progress {
        Some((done, total)) if total > 0 => tr!(
            "Cancel explaining ({done}/{total})",
            done = done,
            total = total
        ),
        _ => tr!("Cancel explaining").to_string(),
    }
}

/// Width of the ⋯ menu's panel, in the reader's language.
pub(crate) fn tools_menu_w(app: &App) -> f32 {
    // The Explain All row as it reads while a pass runs: with its count
    // once there is one, at the most it says — as many done as the total —
    // so the menu does not widen as the count climbs.
    let cancel = cancel_explaining_label(
        app.proj
            .explain
            .progress
            .filter(|_| app.proj.explain.running)
            .map(|(_, total)| (total, total)),
    );
    let chord = |r: ToolsRow| {
        super::sheet::text_width(
            &app.keymap.chord(tools_row_action(r)).caps(),
            ts::SMALL,
            crate::fonts::UI,
        )
    };
    let rows: Vec<(&str, f32)> = TOOLS_MENU_ROWS
        .iter()
        .map(|&r| (tools_row_label(r), chord(r)))
        .chain([(cancel.as_str(), chord(ToolsRow::ExplainAll))])
        .collect();
    let headings: Vec<&str> = TOOLS_MENU_HEADINGS
        .iter()
        .map(|&(_, h)| crate::i18n::lookup(h))
        .collect();
    menu_w(TOOLS_MENU_DESIGN_W, &rows, &headings)
}
/// Inner padding of a menu panel: the design's 5, inside the panel's 1 px
/// edge — which CSS lays outside the padding and iced draws inside it.
const MENU_PAD: f32 = 5.0 + 1.0;
/// Height of one menu row.
const MENU_ROW_H: f32 = 32.0;
/// Height of a heading over a group of menu rows.
const MENU_HEADING_H: f32 = 24.0;
/// Gap between menu rows.
const MENU_ROW_SPACING: f32 = 0.0;
/// Height of a separator between groups of rows.
const MENU_SEP_H: f32 = 11.0;
/// Gap between the strip and a menu it opened.
const STRIP_MENU_GAP: f32 = 3.0;

/// Room the ⋯ menu keeps from the window's foot, and from the top bar.
const MENU_EDGE: f32 = 8.0;

/// The ⋯ menu panel's height, every row of it shown.
pub(crate) fn tools_menu_h() -> f32 {
    2.0 * MENU_PAD
        + TOOLS_MENU_ROWS.len() as f32 * (MENU_ROW_H + MENU_ROW_SPACING)
        + TOOLS_MENU_SEPARATORS.len() as f32 * (MENU_SEP_H + MENU_ROW_SPACING)
        + TOOLS_MENU_HEADINGS.len() as f32 * (MENU_HEADING_H + MENU_ROW_SPACING)
}

/// Distance of the ⋯ menu panel from the window's top edge: level with the
/// ⋯ button it opens from — or raised so it ends above the window's foot,
/// but not over the top bar. The strip falls back to ⋯ in a short window,
/// where the menu hung from the button ran past the foot: its last rows
/// could not be reached.
pub(crate) fn tools_menu_top(app: &App) -> f32 {
    let at_button = strip_tool_center(app, StripTool::More).y - ICON_BTN / 2.0;
    let ends_at_foot = app.window_height - MENU_EDGE - tools_menu_h();
    at_button.min(ends_at_foot).max(TOOLBAR_H + MENU_EDGE)
}

/// The most the ⋯ menu panel may take: what is left under its top; past
/// it, its rows scroll.
pub(crate) fn tools_menu_room(app: &App) -> f32 {
    (app.window_height - MENU_EDGE - tools_menu_top(app)).max(MENU_ROW_H)
}

/// Distance of the ⋯ menu panel from the window's right edge: just left of
/// the strip.
pub(crate) const TOOLS_MENU_RIGHT: f32 = STRIP_W + STRIP_MENU_GAP;

/// Window-space rectangle covering `count` consecutive ⋯ menu rows starting
/// at row `first`, spanning the item column (inside the panel's padding).
pub(crate) fn tools_menu_rows_rect(app: &App, first: usize, count: usize) -> iced::Rectangle {
    let last = (first + count.max(1) - 1).min(TOOLS_MENU_ROWS.len() - 1);
    // On the strip, where it had room for them: their buttons there.
    if let (Some(a), Some(b)) = (
        strip_slot_center(app, StripSlot::Row(TOOLS_MENU_ROWS[first])),
        strip_slot_center(app, StripSlot::Row(TOOLS_MENU_ROWS[last])),
    ) {
        let half = ICON_BTN / 2.0;
        return iced::Rectangle {
            x: a.x - half,
            y: a.y - half,
            width: ICON_BTN,
            height: b.y - a.y + ICON_BTN,
        };
    }
    let top = tools_menu_top(app);
    let top_of = |i: usize| {
        let seps = TOOLS_MENU_SEPARATORS.iter().filter(|&&s| s <= i).count() as f32;
        let heads = TOOLS_MENU_HEADINGS.iter().filter(|&&(h, _)| h <= i).count() as f32;
        top + MENU_PAD
            + i as f32 * (MENU_ROW_H + MENU_ROW_SPACING)
            + seps * (MENU_SEP_H + MENU_ROW_SPACING)
            + heads * (MENU_HEADING_H + MENU_ROW_SPACING)
    };
    let last = first + count.max(1) - 1;
    let y = top_of(first);
    iced::Rectangle {
        x: app.window_width - TOOLS_MENU_RIGHT - tools_menu_w(app) + MENU_PAD,
        y,
        width: tools_menu_w(app) - 2.0 * MENU_PAD,
        height: top_of(last) + MENU_ROW_H - y,
    }
}

/// One row of a menu: an icon in a fixed gutter, the label, and what
/// trails it (a shortcut, a check).
fn menu_item<'a>(
    glyph: Option<Glyph>,
    label: String,
    trailing: Element<'a, Message>,
    msg: Option<Message>,
) -> Element<'a, Message> {
    let gutter: Element<'a, Message> = match glyph {
        // What spends an AI call, in the accent as everywhere.
        Some(Glyph::Sparkle) => glyph::icon(Glyph::Sparkle, theme::accent(), 15.0),
        Some(g) => glyph::icon(g, theme::fg_muted(), 15.0),
        None => space().width(15).into(),
    };
    let mut b = button(
        row![
            container(gutter).width(15),
            ts::set_in(
                text(label.clone()).wrapping(Wrapping::None),
                MENU_ROW_H,
                ts::INTER,
                ts::BASE
            ),
            space().width(Fill),
            trailing,
        ]
        .spacing(10)
        .height(Fill)
        .align_y(iced::Center),
    )
    .style(theme::menu_row)
    .width(Fill)
    .height(Length::Fixed(MENU_ROW_H))
    .padding([0, 9]);
    if let Some(msg) = msg {
        b = b.on_press(super::heard::said(&label, msg));
    }
    b.into()
}

/// The width of the sidebar's More views menu.
const VIEWS_MENU_W: f32 = 220.0;

/// The sidebar's More views menu, from the ⋯ after its views: the views
/// not on the strip — a function's calls, a value's flow, the imports,
/// the reader's notes, the API docs — the one showing checked.
pub(crate) fn views_menu(app: &App) -> Element<'_, Message> {
    use super::sidebar::{
        SIDEBAR_TABS, STRIP_VIEWS, VIEWS_ROW_H, VIEWS_ROW_RIGHT_PAD, VIEWS_ROW_TOP, tab_glyph,
        tab_name,
    };
    let items: Vec<Element<'_, Message>> = SIDEBAR_TABS
        .iter()
        .skip(STRIP_VIEWS)
        .map(|(_, tab)| {
            let trailing: Element<'_, Message> = if app.sidebar == *tab && app.show_left_sidebar {
                glyph::icon(Glyph::Check, theme::accent(), 13.0)
            } else {
                slot()
            };
            menu_item(
                Some(tab_glyph(*tab)),
                tab_name(*tab).to_string(),
                trailing,
                Some(Message::Window(WindowMsg::SidebarTabPicked(*tab))),
            )
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(MENU_ROW_SPACING))
        .width(VIEWS_MENU_W)
        .padding(MENU_PAD)
        .style(theme::modal_panel);
    // Hung from the ⋯, its right edge under the row's.
    let left = (app.sidebar_width - VIEWS_ROW_RIGHT_PAD - VIEWS_MENU_W).max(4.0);
    modal(
        panel,
        Placement::TopLeft {
            top: body_top(app) + VIEWS_ROW_TOP + VIEWS_ROW_H + 3.0,
            left,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleViewsMenu)),
    )
}

/// A heading over a group of menu rows, naming it.
fn menu_heading<'a>(label: &'a str) -> Element<'a, Message> {
    container(ts::set_in(
        text(label)
            .font(crate::fonts::UI_MEDIUM)
            .color(theme::dim()),
        ts::INTER.normal(ts::SMALL),
        ts::INTER,
        ts::SMALL,
    ))
    .height(Length::Fixed(MENU_HEADING_H))
    .align_y(iced::alignment::Vertical::Bottom)
    .padding(Padding {
        top: 6.0,
        right: 9.0,
        bottom: 4.0,
        left: 9.0,
    })
    .into()
}

/// A separator between groups of menu rows.
fn menu_separator<'a>() -> Element<'a, Message> {
    container(hairline())
        .height(Length::Fixed(MENU_SEP_H))
        .align_y(iced::Center)
        .padding([0, 4])
        .into()
}

/// A shortcut trailing a menu row.
fn menu_chord<'a>(app: &App, action: Action) -> Element<'a, Message> {
    text(app.keymap.shown(action))
        .size(ts::SMALL)
        .color(theme::dim())
        .into()
}

/// The ⋯ menu: what the strip has no room for, in [`TOOLS_MENU_ROWS`] order,
/// opening to the left of its button. Its rows are a glyph, a name, and the
/// keys of those that have them.
pub(crate) fn tools_menu(app: &App) -> Element<'_, Message> {
    let mut items: Vec<Element<'_, Message>> = Vec::new();
    for (i, &r) in TOOLS_MENU_ROWS.iter().enumerate() {
        if TOOLS_MENU_SEPARATORS.contains(&i) {
            items.push(menu_separator());
        }
        if let Some(&(_, heading)) = TOOLS_MENU_HEADINGS.iter().find(|&&(h, _)| h == i) {
            items.push(menu_heading(crate::i18n::lookup(heading)));
        }
        let (glyph, label, msg) = tools_row(app, r);
        let keys = menu_chord(app, tools_row_action(r));
        let item = menu_item(Some(glyph), label, keys, msg);
        items.push(item);
    }
    let rows = Column::with_children(items).spacing(MENU_ROW_SPACING);
    let room = tools_menu_room(app);
    // A window shorter than the menu: its rows scroll in what there is.
    let rows: Element<'_, Message> = if tools_menu_h() > room {
        scrollable(rows)
            .height(Length::Fixed(room - 2.0 * MENU_PAD))
            .into()
    } else {
        rows.into()
    };
    let panel = container(rows)
        .width(tools_menu_w(app))
        .padding(MENU_PAD)
        // The design's ⋯ menu is rounded 11.
        .style(theme::modal_panel_rounded(11.0));
    modal(
        panel,
        Placement::TopRight {
            top: tools_menu_top(app),
            right: TOOLS_MENU_RIGHT,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleToolsMenu)),
    )
}

/// A row of the bug's Run menu, where the strip is too short for the four
/// buttons ([`RUN_TOOLS`]): the program run for real, or worked out by the
/// model ("fake"), to its end or stepped through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunRow {
    Run,
    Debug,
    FakeRun,
    FakeDebug,
}

/// The Run menu's rows, top down: the real ones, then the model's.
pub(crate) const RUN_MENU_ROWS: [RunRow; 4] = [
    RunRow::Run,
    RunRow::Debug,
    RunRow::FakeRun,
    RunRow::FakeDebug,
];

/// The [`RUN_MENU_ROWS`] positions a heading stands over, and what it says.
const RUN_MENU_HEADINGS: [(usize, &str); 2] = [
    (0, tr_noop!("Run the program")),
    (2, tr_noop!("Work it out with AI")),
];

/// Width of the Run menu's panel as designed.
const RUN_MENU_DESIGN_W: f32 = 232.0;

/// The action a Run menu row runs, whose shortcut the row shows.
pub(crate) fn run_row_action(row: RunRow) -> Action {
    match row {
        RunRow::Run => Action::Run,
        RunRow::Debug => Action::StartDebug,
        RunRow::FakeRun => Action::FakeRun,
        RunRow::FakeDebug => Action::FakeDebug,
    }
}

/// What a Run menu row shows and does: its icon, its name, and its message
/// (`None` while it can do nothing: no project to run, no code open to
/// work out).
pub(crate) fn run_row(app: &App, row: RunRow) -> (Glyph, &'static str, Option<Message>) {
    let project = app.proj.project.is_some();
    let code = app.active_viewer().is_some();
    match row {
        RunRow::Run => (
            Glyph::Play,
            tr!("Run Without Debugging"),
            project.then_some(Message::Debug(DebugMsg::Run)),
        ),
        RunRow::Debug => (
            Glyph::Debug,
            tr!("Start Debugging"),
            project.then_some(Message::Debug(DebugMsg::Start)),
        ),
        RunRow::FakeRun => (
            Glyph::FakeRun,
            tr!("Fake Run…"),
            code.then_some(Message::Fake(crate::FakeMsg::AtCaret { debug: false })),
        ),
        RunRow::FakeDebug => (
            Glyph::FakeDebug,
            tr!("Fake Debug…"),
            code.then_some(Message::Fake(crate::FakeMsg::AtCaret { debug: true })),
        ),
    }
}

/// Whether `message` is one a Run menu row sends: picking it puts the menu
/// away.
pub(crate) fn closes_run_menu(message: &Message) -> bool {
    matches!(
        message,
        Message::Debug(DebugMsg::Run | DebugMsg::Start)
            | Message::Fake(crate::FakeMsg::AtCaret { .. })
    )
}

/// Width of the Run menu's panel, in the reader's language.
pub(crate) fn run_menu_w(app: &App) -> f32 {
    let chord = |r: RunRow| {
        super::sheet::text_width(
            &app.keymap.chord(run_row_action(r)).caps(),
            ts::SMALL,
            crate::fonts::UI,
        )
    };
    let rows: Vec<(&str, f32)> = RUN_MENU_ROWS
        .iter()
        .map(|&r| (run_row(app, r).1, chord(r)))
        .collect();
    let headings: Vec<&str> = RUN_MENU_HEADINGS
        .iter()
        .map(|&(_, h)| crate::i18n::lookup(h))
        .collect();
    menu_w(RUN_MENU_DESIGN_W, &rows, &headings)
}

/// The Run menu panel's height.
fn run_menu_h() -> f32 {
    2.0 * MENU_PAD
        + RUN_MENU_ROWS.len() as f32 * (MENU_ROW_H + MENU_ROW_SPACING)
        + RUN_MENU_HEADINGS.len() as f32 * (MENU_HEADING_H + MENU_ROW_SPACING)
}

/// Distance of the Run menu panel from the window's top edge: level with
/// the bug it opens from, or raised to end above the window's foot.
pub(crate) fn run_menu_top(app: &App) -> f32 {
    let at_button = strip_tool_center(app, StripTool::RunMenu).y - ICON_BTN / 2.0;
    let ends_at_foot = app.window_height - MENU_EDGE - run_menu_h();
    at_button.min(ends_at_foot).max(TOOLBAR_H + MENU_EDGE)
}

/// The strip's Run menu: Run and Debug, then Fake Run and Fake Debug, each
/// with its keys, opening to the left of the bug.
pub(crate) fn run_menu(app: &App) -> Element<'_, Message> {
    let mut items: Vec<Element<'_, Message>> = Vec::new();
    for (i, &r) in RUN_MENU_ROWS.iter().enumerate() {
        if let Some(&(_, heading)) = RUN_MENU_HEADINGS.iter().find(|&&(h, _)| h == i) {
            items.push(menu_heading(crate::i18n::lookup(heading)));
        }
        let (glyph, label, msg) = run_row(app, r);
        let keys = menu_chord(app, run_row_action(r));
        items.push(menu_item(Some(glyph), label.to_string(), keys, msg));
    }
    let panel = container(Column::with_children(items).spacing(MENU_ROW_SPACING))
        .width(run_menu_w(app))
        .padding(MENU_PAD)
        .style(theme::modal_panel_rounded(11.0));
    modal(
        panel,
        Placement::TopRight {
            top: run_menu_top(app),
            right: TOOLS_MENU_RIGHT,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleRunMenu)),
    )
}

/// What Explain All does, wherever it is offered: asks first what it costs
/// (the setup sheet), or for a key, on the page of Settings that takes it.
pub(crate) fn explain_all_message(app: &App) -> Message {
    if app.llm_available {
        Message::Project(ProjectMsg::SetupShow(crate::app::setup::SetupMode::Explain))
    } else {
        Message::Settings(SettingsMsg::NeedKey)
    }
}

/// What a row of the ⋯ menu shows and does — in the menu, or on the strip
/// where there is room for it ([`strip_slots`]): its icon, its name, and
/// its message (`None` while it can do nothing, as without a project).
pub(crate) fn tools_row(app: &App, row: ToolsRow) -> (Glyph, String, Option<Message>) {
    let has_project = app.proj.project.is_some();
    match row {
        // While a pass runs the row cancels it — the bottom-up pass is
        // thousands of model calls on a big repo, so it must be
        // stoppable; with no key configured it routes to Settings.
        ToolsRow::ExplainAll if app.proj.explain.running => (
            Glyph::Close,
            cancel_explaining_label(app.proj.explain.progress),
            Some(Message::Explain(ExplainMsg::Cancel)),
        ),
        ToolsRow::ExplainAll => (
            Glyph::TextLines,
            tr!("Explain All…").into(),
            Some(explain_all_message(app)),
        ),
        ToolsRow::Glossary => (
            Glyph::Dictionary,
            tr!("Glossary").into(),
            has_project.then_some(Message::Glossary(GlossaryMsg::Open)),
        ),
        // Not Settings' gear: on the strip it stood over Settings itself.
        ToolsRow::SetUp => (
            Glyph::CheckCircle,
            tr!("Set Up Project…").into(),
            has_project.then_some(Message::Project(ProjectMsg::SetupShow(
                crate::app::setup::SetupMode::Manage,
            ))),
        ),
        // The tour brings its own project: there with one open or none.
        ToolsRow::Tutorial => (
            Glyph::Help,
            tr!("Tutorial").into(),
            Some(Message::Tutorial(TutorialMsg::Start)),
        ),
    }
}

// ------------------------------------------------------- the View menu

/// A row of the reader's View menu: a reading aid, on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewRow {
    OutlineSummaries,
    FileSummary,
    /// The note on the function the caret is in, block by block.
    Blocks,
    InlayHints,
    InlineBlame,
    IndentGuides,
    Minimap,
    /// The margin beside the code: its notes, calls and history.
    Margin,
}

/// The View menu's rows, top to bottom.
pub(crate) const VIEW_MENU_ROWS: [ViewRow; 8] = [
    ViewRow::OutlineSummaries,
    ViewRow::FileSummary,
    ViewRow::Blocks,
    ViewRow::InlayHints,
    ViewRow::InlineBlame,
    ViewRow::IndentGuides,
    ViewRow::Minimap,
    ViewRow::Margin,
];

impl ViewRow {
    /// What the row toggles.
    pub(crate) fn message(self) -> Message {
        match self {
            ViewRow::OutlineSummaries => Message::Window(WindowMsg::ToggleInlineSummaries),
            ViewRow::FileSummary => Message::Window(WindowMsg::ToggleFileBanner),
            ViewRow::Blocks => Message::Explain(ExplainMsg::ToggleBlocks),
            ViewRow::InlayHints => Message::Lsp(LspMsg::ToggleInlayHints),
            ViewRow::InlineBlame => Message::Window(WindowMsg::ToggleInlineBlame),
            ViewRow::IndentGuides => Message::Window(WindowMsg::ToggleIndentGuides),
            ViewRow::Minimap => Message::Window(WindowMsg::ToggleMinimap),
            ViewRow::Margin => Message::Window(WindowMsg::ToggleRightPanel),
        }
    }
}

/// Distance of the View menu from the window's top edge: under the file
/// tools it opens from.
pub(crate) fn view_menu_top(app: &App) -> f32 {
    body_top(app) + SHEET_HEADER_ROW_H + 2.0
}

/// Distance of the View menu from the window's left edge: under the View
/// button it opens from, kept inside the window.
pub(crate) fn view_menu_left(app: &App) -> f32 {
    let x = super::sheet::view_button_rect(app).map_or(0.0, |b| b.x);
    x.min(app.window_width - STRIP_W - view_menu_w() - 8.0)
        .max(8.0)
}

/// Width of the View menu, in the reader's language (its rows end in a
/// check).
pub(crate) fn view_menu_w() -> f32 {
    let rows: Vec<(&str, f32)> = [
        tr!("Outline summaries"),
        tr!("File summary"),
        tr!("Explanation in blocks"),
        tr!("Inlay hints"),
        tr!("Inline blame"),
        tr!("Indent guides"),
        tr!("Minimap"),
        tr!("Margin"),
    ]
    .into_iter()
    .map(|l| (l, 14.0))
    .collect();
    menu_w(220.0, &rows, &[])
}

/// Height of the View menu's panel.
pub(crate) fn view_menu_height() -> f32 {
    let rows = VIEW_MENU_ROWS.len() as f32;
    2.0 * MENU_PAD + rows * MENU_ROW_H + (rows - 1.0) * MENU_ROW_SPACING
}

/// The reader's View menu: the reading aids, each checked while it shows.
pub(crate) fn view_menu(app: &App) -> Element<'_, Message> {
    let check = |on: bool| -> Element<'_, Message> {
        if on {
            glyph::icon(Glyph::Check, theme::accent(), 14.0)
        } else {
            slot()
        }
    };
    let items: Vec<Element<'_, Message>> = VIEW_MENU_ROWS
        .iter()
        .map(|&r| {
            let (glyph, label, on) = match r {
                ViewRow::OutlineSummaries => (
                    Glyph::Note,
                    tr!("Outline summaries"),
                    app.show_inline_summaries,
                ),
                ViewRow::FileSummary => (Glyph::Info, tr!("File summary"), app.show_file_banner),
                ViewRow::Blocks => (
                    Glyph::List,
                    tr!("Explanation in blocks"),
                    app.proj.explain.showing_detail,
                ),
                ViewRow::InlayHints => (Glyph::Lightbulb, tr!("Inlay hints"), app.show_inlay_hints),
                ViewRow::InlineBlame => (
                    Glyph::TimeTravel,
                    tr!("Inline blame"),
                    app.show_inline_blame,
                ),
                ViewRow::IndentGuides => (
                    Glyph::TextLines,
                    tr!("Indent guides"),
                    app.show_indent_guides,
                ),
                ViewRow::Minimap => (Glyph::Minimap, tr!("Minimap"), app.show_minimap),
                ViewRow::Margin => (Glyph::PanelRight, tr!("Margin"), app.show_right_panel),
            };
            // Blocks are a function's: with no function explained, the row
            // is there, inert.
            let enabled = r != ViewRow::Blocks
                || matches!(
                    app.proj.explain.view,
                    Some(crate::explain::Node::Function { .. })
                );
            let item = menu_item(
                Some(glyph),
                label.into(),
                check(on),
                enabled.then(|| r.message()),
            );
            // The minimap on, on a file too short for one: said why there is
            // none, where the tick alone looked broken.
            let short_file = r == ViewRow::Minimap
                && app.show_minimap
                && app
                    .active_viewer()
                    .is_some_and(|v| v.content_rows() < crate::codeview::MINIMAP_MIN_ROWS);
            // Inert, it says why where the pointer rests.
            if short_file {
                tip_at(
                    item,
                    trn!(
                        "Shown when a file shows 1 line or more (folded lines don't count)",
                        "Shown when a file shows {n} lines or more (folded lines don't count)",
                        crate::codeview::MINIMAP_MIN_ROWS
                    ),
                    None,
                    tooltip::Position::Right,
                )
            } else if enabled {
                item
            } else {
                tip_at(
                    item,
                    tr!("Explain a function to read its explanation in blocks"),
                    None,
                    tooltip::Position::Right,
                )
            }
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(MENU_ROW_SPACING))
        .width(view_menu_w())
        .padding(MENU_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        Placement::TopLeft {
            top: view_menu_top(app),
            left: view_menu_left(app),
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleViewMenu)),
    )
}

// ------------------------------------------------ the app menu (☰)

/// Width of the app menu, in the reader's language: its rows and their
/// shortcuts.
fn app_menu_w(app: &App) -> f32 {
    let chord = |c: &str| super::sheet::text_width(c, ts::SMALL, crate::fonts::UI);
    let mut rows: Vec<(String, f32)> = app_menu_head(app)
        .into_iter()
        .map(|(_, label, keys, _)| (label.to_string(), keys.as_deref().map_or(0.0, chord)))
        .collect();
    for action in Action::ALL {
        let title = crate::i18n::lookup(action.menu().title).to_string();
        rows.push((title, chord(&app.keymap.chord(action).caps())));
    }
    rows.push((tr!("Close Window").to_string(), 0.0));
    let rows: Vec<(&str, f32)> = rows.iter().map(|(l, w)| (l.as_str(), *w)).collect();
    menu_w(280.0, &rows, &[])
}

/// The rows of the app menu that are not keymap actions, top first.
pub(crate) fn app_menu_head(app: &App) -> Vec<(Glyph, &'static str, Option<String>, Message)> {
    let _ = app;
    vec![
        (
            Glyph::FolderOpen,
            tr!("Open Folder…"),
            None,
            Message::Project(ProjectMsg::OpenFolderPressed),
        ),
        (
            Glyph::Remote,
            tr!("Open Remote…"),
            None,
            Message::Connect(ConnectMsg::Open),
        ),
        (
            Glyph::Settings,
            tr!("Settings…"),
            Some(crate::keymap::local_keys("⌘,")),
            Message::Settings(SettingsMsg::Open),
        ),
    ]
}

/// Whether `message` is a pick from the app menu, which closes it.
pub(crate) fn closes_app_menu(message: &Message) -> bool {
    matches!(
        message,
        Message::Editor(EditorMsg::RunAction(_))
            | Message::Project(ProjectMsg::OpenFolderPressed)
            | Message::Connect(ConnectMsg::Open)
            | Message::Settings(SettingsMsg::Open)
            | Message::Window(WindowMsg::Close)
    )
}

/// The app menu of Windows and Linux, from the top bar's ☰: what macOS keeps
/// in its menu bar — opening a folder or a host, Settings, and every keymap
/// action under the title the menu bar gives it (Edit, View, Tools, Go),
/// each with its shortcut.
pub(crate) fn app_menu(app: &App) -> Element<'_, Message> {
    let mut items: Vec<Element<'_, Message>> = Vec::new();
    for (glyph, label, chord, msg) in app_menu_head(app) {
        let trailing: Element<'_, Message> = match chord {
            Some(c) => text(c).size(ts::SMALL).color(theme::dim()).into(),
            None => slot(),
        };
        items.push(menu_item(Some(glyph), label.into(), trailing, Some(msg)));
    }
    for section in MenuSection::ALL {
        items.push(menu_separator());
        items.push(
            container(text(section.title()).size(ts::SMALL).color(theme::dim()))
                .padding(Padding {
                    top: 2.0,
                    right: 8.0,
                    bottom: 2.0,
                    left: 37.0,
                })
                .into(),
        );
        for action in Action::ALL {
            let placed = action.menu();
            if placed.section != section {
                continue;
            }
            let msg = Message::Editor(EditorMsg::RunAction(action));
            // What spends AI calls carries the sparkle here too: Explain
            // All and the fake runs looked like the free rows around them.
            let mark = crate::app::palette::spends_ai(&msg).then_some(Glyph::Sparkle);
            items.push(menu_item(
                mark,
                crate::i18n::lookup(placed.title).to_string(),
                menu_chord(app, action),
                Some(msg),
            ));
        }
    }
    items.push(menu_separator());
    items.push(menu_item(
        Some(Glyph::Close),
        tr!("Close Window").into(),
        slot(),
        Some(Message::Window(WindowMsg::Close)),
    ));
    let max_h = (app.window_height - TOOLBAR_H - 24.0).max(160.0);
    // Its scroller shows at rest: in a short window, the rows that run
    // past its bottom had nothing to say they were there.
    let panel = container(
        scrollable(Column::with_children(items).spacing(MENU_ROW_SPACING))
            .direction(thin_scroll())
            .style(theme::cue_scrollbar),
    )
    .width(app_menu_w(app))
    .max_height(max_h)
    .padding(MENU_PAD)
    .style(theme::modal_panel);
    modal(
        panel,
        Placement::TopLeft {
            top: TOOLBAR_H - 4.0,
            left: BAR_PAD_L - 2.0,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleAppMenu)),
    )
}

/// The status-bar `#[cfg]` target dropdown: which platform's cfg branches read
/// as live. A hand-rolled popup (not a `pick_list`, which pads to its widest
/// option and leaves a gap before the chevron) anchored to the bottom-right so
/// it hugs its trigger button.
pub(crate) fn target_menu(app: &App) -> Element<'_, Message> {
    let current = app.proj.reading_target.clone();
    let items: Vec<Element<'_, Message>> = crate::inactive::Target::presets()
        .into_iter()
        .map(|t| {
            let selected = t == current;
            let mark: Element<'_, Message> = if selected {
                glyph::icon(Glyph::Check, theme::accent(), 13.0)
            } else {
                slot()
            };
            button(
                row![
                    container(mark).width(18),
                    text(super::statusbar::target_label(&t)).size(ts::BODY)
                ]
                .align_y(iced::Center),
            )
            .style(theme::menu_row)
            .width(Fill)
            .padding([5, 8])
            .on_press(super::heard::said_with(
                || super::statusbar::target_label(&t),
                Message::Reading(ReadingMsg::TargetSelected(t.clone())),
            ))
            .into()
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(1))
        .width(TARGET_MENU_W)
        .padding(MENU_PAD)
        .style(theme::modal_panel);
    modal(
        panel,
        Placement::BottomRight {
            bottom: STATUSBAR_H + 3.0,
            right: STRIP_W + 4.0,
        },
        Backdrop::Clear(Message::Window(WindowMsg::ToggleTargetMenu)),
    )
}

#[cfg(test)]
mod tests {

    /// A shortcut is drawn a cap per key, however it is spelled.
    #[test]
    fn a_shortcut_is_split_into_its_keys() {
        use super::split_caps;
        assert_eq!(split_caps("Ctrl+Shift+A"), ["Ctrl", "Shift", "A"]);
        assert_eq!(split_caps("Ctrl++"), ["Ctrl", "+"]);
        assert_eq!(split_caps("Alt+←"), ["Alt", "←"]);
        assert_eq!(split_caps("⌘⇧A"), ["⌘", "⇧", "A"]);
        assert_eq!(split_caps("⌥⌘F12"), ["⌥", "⌘", "F12"]);
        assert_eq!(split_caps("⌘+"), ["⌘", "+"]);
        assert_eq!(split_caps("⌘⇧+"), ["⌘", "⇧", "+"]);
        assert_eq!(split_caps("F5"), ["F5"]);
        assert_eq!(split_caps("g d"), ["g d"]);
        assert_eq!(split_caps("?"), ["?"]);
    }
    use super::*;

    #[test]
    fn every_tools_row_is_listed_once_and_knows_its_index() {
        for (i, row) in TOOLS_MENU_ROWS.iter().enumerate() {
            assert_eq!(row.index(), i, "{row:?}");
        }
        let mut seen = TOOLS_MENU_ROWS.to_vec();
        seen.dedup();
        assert_eq!(seen.len(), TOOLS_MENU_ROWS.len());
    }

    /// Where the strip is tall enough, the ⋯ menu's rows are laid on it, in
    /// their groups, after the strip's own tools — and the ⋯ is not there;
    /// shorter, the ⋯ is, and they are in its menu. The tutorial's hole for
    /// a row is its button on the strip, then.
    #[test]
    fn the_menus_tools_are_on_the_strip_where_it_has_room() {
        let mut app = crate::app::tests::blank_app();
        app.window_width = 1280.0;
        app.window_height = 900.0;
        assert!(tools_on_strip(&app));
        let slots = strip_slots(&app);
        assert!(!slots.contains(&StripSlot::Tool(StripTool::More)));
        for row in TOOLS_MENU_ROWS {
            assert!(slots.contains(&StripSlot::Row(row)), "{row:?}");
        }
        let center = |slot| strip_slot_center(&app, slot).unwrap().y;
        let fake_debug = center(StripSlot::Tool(StripTool::FakeDebug));
        let first = center(StripSlot::Row(ToolsRow::ExplainAll));
        let last = center(StripSlot::Row(ToolsRow::Tutorial));
        assert!(first > fake_debug);
        let settings = center(StripSlot::Tool(StripTool::Settings));
        assert!(last + ICON_BTN / 2.0 + STRIP_FOOT_GAP <= settings - ICON_BTN / 2.0);
        let hole = tools_menu_rows_rect(&app, ToolsRow::Glossary.index(), 1);
        let glossary = strip_slot_center(&app, StripSlot::Row(ToolsRow::Glossary)).unwrap();
        assert!(hole.contains(glossary), "{hole:?} {glossary:?}");

        app.window_height = 620.0;
        assert!(!tools_on_strip(&app));
        assert!(
            runs_on_strip(&app),
            "the four ways to run keep their buttons"
        );
        assert!(strip_slots(&app).contains(&StripSlot::Tool(StripTool::More)));
        assert!(strip_slot_center(&app, StripSlot::Row(ToolsRow::Glossary)).is_none());
    }

    /// The four ways to run are buttons of their own, each beside its twin
    /// — Run, Fake Run, Debug, Fake Debug — each named and keyed as the
    /// menu bar has it, and sending what its key does. In the shortest
    /// window they fold into the bug's menu, and the strip still ends
    /// clear of Settings at its foot.
    #[test]
    fn the_four_ways_to_run_are_buttons_and_fold_into_the_bug_when_short() {
        let mut app = crate::app::tests::blank_app();
        app.window_width = 1280.0;
        app.window_height = 860.0;
        let slots = strip_slots(&app);
        let at = |t| {
            slots
                .iter()
                .position(|s| *s == StripSlot::Tool(t))
                .unwrap_or_else(|| panic!("{t:?} is not on the strip"))
        };
        assert_eq!(
            [
                at(StripTool::Run),
                at(StripTool::FakeRun),
                at(StripTool::Debug),
                at(StripTool::FakeDebug)
            ],
            [
                at(StripTool::Run),
                at(StripTool::Run) + 1,
                at(StripTool::Run) + 2,
                at(StripTool::Run) + 3
            ]
        );
        assert!(!slots.contains(&StripSlot::Tool(StripTool::RunMenu)));
        for (tool, action) in [
            (StripTool::Run, Action::Run),
            (StripTool::FakeRun, Action::FakeRun),
            (StripTool::Debug, Action::StartDebug),
            (StripTool::FakeDebug, Action::FakeDebug),
        ] {
            let (_, label, keys, _) = strip_tool(&app, tool);
            assert_eq!(label, crate::i18n::lookup(action.menu().title), "{tool:?}");
            assert_eq!(keys, Some(app.keymap.chord(action).caps()), "{tool:?}");
        }
        assert!(matches!(
            strip_tool(&app, StripTool::Run).3,
            Message::Debug(DebugMsg::Run)
        ));
        assert!(matches!(
            strip_tool(&app, StripTool::Debug).3,
            Message::Debug(DebugMsg::Start)
        ));
        assert!(matches!(
            strip_tool(&app, StripTool::FakeRun).3,
            Message::Fake(crate::FakeMsg::AtCaret { debug: false })
        ));
        assert!(matches!(
            strip_tool(&app, StripTool::FakeDebug).3,
            Message::Fake(crate::FakeMsg::AtCaret { debug: true })
        ));

        // The window at its least: the four in the bug's menu.
        app.window_height = 520.0;
        assert!(!runs_on_strip(&app));
        let slots = strip_slots(&app);
        assert!(slots.contains(&StripSlot::Tool(StripTool::RunMenu)));
        for t in RUN_TOOLS {
            assert!(!slots.contains(&StripSlot::Tool(t)), "{t:?}");
        }
        let more = strip_slot_center(&app, StripSlot::Tool(StripTool::More)).unwrap();
        let settings = strip_slot_center(&app, StripSlot::Tool(StripTool::Settings)).unwrap();
        assert!(more.y + ICON_BTN / 2.0 + STRIP_FOOT_GAP <= settings.y - ICON_BTN / 2.0);
    }

    /// The strip's slots go down one pitch apart, a rule's slot more after
    /// each group, and follow the window's right edge.
    #[test]
    fn the_strip_slots_follow_the_groups_and_the_right_edge() {
        let mut app = crate::app::tests::blank_app();
        app.window_width = 1280.0;
        // Short enough for the ⋯ to be on the strip.
        app.window_height = 620.0;
        let y = |app: &App, t| strip_tool_center(app, t).y;
        let pitch = ICON_BTN + STRIP_SPACING;
        let rule = STRIP_RULE_H + STRIP_SPACING;
        assert_eq!(
            y(&app, StripTool::Stats) - y(&app, StripTool::Overview),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::WeakSpots) - y(&app, StripTool::Stats),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::CallGraph) - y(&app, StripTool::WeakSpots),
            pitch + rule
        );
        assert_eq!(
            y(&app, StripTool::TypeMap) - y(&app, StripTool::ImportGraph),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::Run) - y(&app, StripTool::ProjectMaps),
            pitch + rule
        );
        assert_eq!(y(&app, StripTool::FakeRun) - y(&app, StripTool::Run), pitch);
        assert_eq!(
            y(&app, StripTool::Debug) - y(&app, StripTool::FakeRun),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::FakeDebug) - y(&app, StripTool::Debug),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::More) - y(&app, StripTool::FakeDebug),
            pitch + rule
        );
        let x = strip_tool_center(&app, StripTool::Overview).x;
        app.window_width += 100.0;
        assert_eq!(strip_tool_center(&app, StripTool::Overview).x - x, 100.0);
        // Settings stands at the foot, moving with the bottom edge.
        let settings = y(&app, StripTool::Settings);
        app.window_height += 50.0;
        assert_eq!(y(&app, StripTool::Settings) - settings, 50.0);
    }

    /// The chat field is centred in the room the clusters leave, never
    /// wider than its widest.
    #[test]
    fn the_chat_field_is_centred_in_its_room() {
        let mut app = crate::app::tests::blank_app();
        app.window_width = 1400.0;
        app.show_left_sidebar = true;
        let r = search_field_rect(&app);
        assert_eq!(r.width, FIELD_MAX_W);
        let room_left = left_cluster_w(&app) + NAV_W;
        let room_right = app.window_width - right_cluster_w();
        assert!(
            ((r.x - room_left) - (room_right - (r.x + r.width))).abs() < 0.5,
            "{r:?}"
        );
        app.window_width = 600.0;
        assert!(search_field_rect(&app).width < FIELD_MAX_W);
    }
}
