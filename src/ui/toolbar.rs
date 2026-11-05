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

use crate::VoiceMsg;
use crate::app::voice::VoicePhase;
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
/// The chat field: widest it grows, its height, and the room kept clear
/// either side of it.
const FIELD_MAX_W: f32 = 460.0;
const FIELD_H: f32 = 34.0;
/// The field while listening: its red dot, and the design's gap between
/// what it holds.
const REC_DOT: f32 = 7.0;
const LISTENING_GAP: f32 = 9.0;
const FIELD_MARGIN: f32 = 14.0;
/// Width of one of the window buttons of Windows and Linux.
const WIN_BTN_W: f32 = 46.0;
/// The slot the panel toggle is centred in at the top bar's right (macOS).
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

/// Width of the top bar's right cluster: the panel toggle (macOS), or the
/// window buttons (Windows and Linux).
fn right_cluster_w() -> f32 {
    if MAC_CHROME {
        TOGGLE_SLOT_W
    } else {
        WIN_BTNS_GAP + 3.0 * WIN_BTN_W
    }
}

/// Where the chat field is drawn, in window coordinates: centred in the
/// room between the left and right clusters, never wider than
/// [`FIELD_MAX_W`].
pub(crate) fn chat_field_rect(app: &App) -> iced::Rectangle {
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

/// [`chrome_tip`], on a given side of the control.
pub(crate) fn tip_at<'a>(
    control: impl Into<Element<'a, Message>>,
    name: &'a str,
    shortcut: Option<String>,
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
    tooltip(control, bubble, side)
        .gap(TIP_GAP)
        .padding(0)
        .into()
}

/// A tooltip's shortcut, as its keys: each a cap of its own on the bubble,
/// the design's 17 wide at least and 18 high, 4 either side of its words.
fn tip_keys<'a>(caps: &str) -> Element<'a, Message> {
    Row::with_children(split_caps(caps).into_iter().map(|key| {
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
    }))
    .spacing(3)
    .align_y(iced::Center)
    .into()
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

/// A shortcut, drawn as its keys: one cap for each (⌘ ⇧ A, or Ctrl Shift A).
pub(crate) fn keycap<'a>(caps: String) -> Element<'a, Message> {
    caps_in(caps, crate::fonts::UI_MEDIUM, CapSize::FIELD)
}

/// [`keycap`] on a control (the chat field), whose words keep the face's
/// own spacing (`fonts::in_control`).
fn control_keycap<'a>(caps: String) -> Element<'a, Message> {
    caps_in(
        caps,
        crate::fonts::in_control(crate::fonts::UI_MEDIUM),
        CapSize::FIELD,
    )
}

/// [`keycap`] at the size the design gives the keys of a row of the
/// command bar.
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
    Row::with_children(split_caps(&caps).into_iter().map(|key| {
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
    }))
    .spacing(3)
    .align_y(iced::Center)
    .into()
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
fn icon_button<'a>(glyph: Glyph, color: iced::Color, on: bool) -> button::Button<'a, Message> {
    button(
        container(glyph::icon(glyph, color, ICON))
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
            icon_button(Glyph::Menu, theme::fg_muted(), app.show_app_menu)
                .on_press(Message::Window(WindowMsg::ToggleAppMenu)),
            "Menu",
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
        .on_press(Message::Window(WindowMsg::ToggleLeftSidebar)),
        "Toggle sidebar",
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
    let nav = |glyph: Glyph, enabled: bool, msg: Message| {
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
            b = b.on_press(msg);
        }
        b
    };
    let nav = container(
        row![
            chrome_tip(
                nav(
                    Glyph::ChevronLeft,
                    app.proj.history.can_back(),
                    Message::Reading(ReadingMsg::GoBack)
                ),
                "Back",
                caps(Action::GoBack),
            ),
            chrome_tip(
                nav(
                    Glyph::ChevronRight,
                    app.proj.history.can_forward(),
                    Message::Reading(ReadingMsg::GoForward)
                ),
                "Forward",
                caps(Action::GoForward),
            ),
        ]
        .spacing(2)
        .align_y(iced::Center),
    )
    .padding(iced::padding::left(NAV_PAD_L))
    .width(Length::Fixed(NAV_W));

    let field = container(chat_field(app))
        .width(Fill)
        .align_x(iced::Center)
        .padding([0.0, FIELD_MARGIN]);

    // Right: the panel toggle (macOS), or the window buttons.
    let right: Element<'_, Message> = if MAC_CHROME {
        container(chrome_tip(
            icon_button(
                Glyph::PanelRight,
                if app.show_right_panel {
                    theme::fg_muted()
                } else {
                    theme::dim()
                },
                false,
            )
            .on_press(Message::Window(WindowMsg::ToggleRightPanel)),
            "Toggle panel",
            None,
        ))
        .width(Length::Fixed(TOGGLE_SLOT_W))
        .align_x(iced::Center)
        .into()
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
fn traffic_lights(app: &App) -> Element<'_, Message> {
    let show_icon = app.controls_hovered;
    let colored = app.window_focused || app.controls_hovered;
    let light = move |color: iced::Color, icon: TrafficIcon, msg: Message| {
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
            .on_press(msg)
    };
    // No tooltips: native traffic lights show only their glyph on hover.
    mouse_area(
        row![
            light(
                theme::TRAFFIC_CLOSE,
                TrafficIcon::Close,
                Message::Window(WindowMsg::Close)
            ),
            light(
                theme::TRAFFIC_MINIMIZE,
                TrafficIcon::Minimize,
                Message::Window(WindowMsg::Minimize)
            ),
            light(
                theme::TRAFFIC_ZOOM,
                TrafficIcon::Fullscreen(app.fullscreen),
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
                .on_press(msg),
            label,
            None,
        )
    };
    let (max_glyph, max_label) = if app.maximized {
        (WindowGlyph::Restore, "Restore")
    } else {
        (WindowGlyph::Maximize, "Maximize")
    };
    row![
        key(
            WindowGlyph::Minimize,
            false,
            "Minimize",
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
            "Close",
            Message::Window(WindowMsg::Close)
        ),
    ]
    .height(Fill)
    .into()
}

/// The chat field: the one place to talk to Clew. Typed or said, a question
/// or something to do — Clew answers, and opens, jumps and runs what was
/// asked for. A click opens the conversation; the microphone listens. While
/// voice is busy the field says what it is doing, with a way to stop.
fn chat_field(app: &App) -> Element<'_, Message> {
    let rect = chat_field_rect(app);
    let listening = app.voice.is_listening();
    let busy = app.voice.is_busy() || app.voice.confirm.is_some();
    let active = app.ask_view != AskView::Closed || busy;
    let small = |label: &'static str, msg: Message, primary: bool| {
        button(text(label).size(ts::SMALL))
            .style(if primary {
                theme::primary_button
            } else {
                theme::toolbar_button
            })
            .padding([2, 8])
            .on_press(msg)
    };
    // The card opens over the field: what voice is doing is said on the
    // card then, and the field under it keeps its plain face — no second
    // set of the same controls hidden beneath the card.
    let covered = super::askfloat::ask_is_card(app);
    let face: Element<'_, Message> =
        if let Some(title) = app.voice.confirm.as_ref().filter(|_| !covered) {
            row![
                text(format!("Run “{title}”?"))
                    .size(ts::BASE)
                    .color(theme::fg())
                    .width(Fill)
                    .wrapping(Wrapping::None),
                small("Run", Message::Voice(VoiceMsg::Confirm), true),
                small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
            ]
            .spacing(6)
            .align_y(iced::Center)
            .into()
        } else {
            match app.voice.phase {
                // Listening: how loud, and how to stop — the microphone itself,
                // lit, or Esc.
                VoicePhase::Listening { level, .. } if !covered => row![
                    container(space().width(REC_DOT).height(REC_DOT)).style(|_: &iced::Theme| {
                        container::Style {
                            background: Some(theme::recording().into()),
                            border: iced::border::rounded(REC_DOT / 2.0),
                            ..container::Style::default()
                        }
                    }),
                    ts::set_in(
                        text("Listening…").color(theme::fg()),
                        FIELD_H - 2.0,
                        ts::INTER,
                        ts::BASE,
                    ),
                    super::askfloat::level_bars(&[5.0, 9.0, 13.0, 8.0, 11.0, 6.0], level, false),
                    space().width(Fill),
                    keycap("Esc".to_string()),
                ]
                .spacing(LISTENING_GAP)
                .align_y(iced::Center)
                .into(),
                VoicePhase::Transcribing { .. } if !covered => row![
                    text("Working out what you said…")
                        .size(ts::BASE)
                        .color(theme::fg_muted())
                        .width(Fill)
                        .wrapping(Wrapping::None),
                    small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
                ]
                .spacing(6)
                .align_y(iced::Center)
                .into(),
                _ => {
                    let chord = app.keymap.chord(Action::ToggleAsk).caps();
                    button(
                        row![
                            glyph::icon(Glyph::Message, theme::accent(), 15.0),
                            container(ts::set_in(
                                text("Ask a question, or say what to do")
                                    .font(crate::fonts::in_control(crate::fonts::UI))
                                    .color(theme::dim())
                                    .wrapping(Wrapping::None),
                                FIELD_H - 2.0,
                                ts::INTER,
                                ts::BASE,
                            ))
                            .width(Fill)
                            .clip(true),
                            control_keycap(chord),
                        ]
                        .spacing(9)
                        .height(Fill)
                        .align_y(iced::Center),
                    )
                    .style(theme::field_face)
                    .padding(0)
                    .width(Fill)
                    .height(Fill)
                    .on_press(Message::Ask(AskMsg::Toggle))
                    .into()
                }
            }
        };
    let chord = app.keymap.chord(Action::Voice).caps();
    let mic = tip_at(
        button(
            container(glyph::icon(
                Glyph::Mic,
                if listening {
                    crate::theme::active().on_accent
                } else {
                    theme::fg_muted()
                },
                15.0,
            ))
            .width(Fill)
            .height(Fill)
            .align_x(iced::Center)
            .align_y(iced::Center),
        )
        .style(move |t: &iced::Theme, status| {
            if listening {
                theme::round_button(true)(t, status)
            } else {
                theme::icon_button(false)(t, status)
            }
        })
        .padding(0)
        .width(Length::Fixed(26.0))
        .height(Length::Fixed(26.0))
        .on_press(Message::Voice(VoiceMsg::Toggle)),
        if listening { "Stop listening" } else { "Talk" },
        Some(chord),
        tooltip::Position::Bottom,
    );
    // The design's 12 before and 4 after, inside the field's 1 px edge
    // (which iced draws inside the box).
    container(
        row![face, mic]
            .spacing(4)
            .height(Fill)
            .align_y(iced::Center),
    )
    .padding(Padding {
        top: 1.0,
        right: 4.0 + 1.0,
        bottom: 1.0,
        left: 12.0 + 1.0,
    })
    .width(Length::Fixed(rect.width))
    .height(Length::Fixed(FIELD_H))
    .style(theme::field(active))
    .into()
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
    Debug,
    CallGraph,
    ImportGraph,
    TypeMap,
    ProjectMaps,
    /// The ⋯ menu.
    More,
    /// At the strip's foot.
    Settings,
}

/// The strip's tools from the top, in their groups: seeing the whole project
/// (Overview, Stats), running it (Debug), its graphs and maps, and More. A
/// rule stands before each group but the first ([`STRIP_RULE_BEFORE`]).
/// Settings stands apart, at the strip's foot.
pub(crate) const STRIP_TOP: [StripTool; 8] = [
    StripTool::Overview,
    StripTool::Stats,
    StripTool::Debug,
    StripTool::CallGraph,
    StripTool::ImportGraph,
    StripTool::TypeMap,
    StripTool::ProjectMaps,
    StripTool::More,
];
/// The [`STRIP_TOP`] positions a group rule stands before.
const STRIP_RULE_BEFORE: [usize; 3] = [2, 3, 7];

/// Where the strip's body starts: under the top bar and the update banner.
pub(crate) fn body_top(app: &App) -> f32 {
    TOOLBAR_H + update_banner_height(app)
}

/// Centre of a strip tool, in window coordinates.
pub(crate) fn strip_tool_center(app: &App, tool: StripTool) -> iced::Point {
    let x = app.window_width - STRIP_W / 2.0;
    let y = match tool {
        StripTool::Settings => {
            let toggle = if MAC_CHROME {
                0.0
            } else {
                ICON_BTN + STRIP_SPACING
            };
            app.window_height - STATUSBAR_H - STRIP_PAD_BOTTOM - toggle - ICON_BTN / 2.0
        }
        tool => {
            let i = STRIP_TOP.iter().position(|t| *t == tool).unwrap_or(0);
            let rules = STRIP_RULE_BEFORE.iter().filter(|&&r| r <= i).count();
            body_top(app)
                + STRIP_PAD_TOP
                + i as f32 * (ICON_BTN + STRIP_SPACING)
                + rules as f32 * (STRIP_RULE_H + STRIP_SPACING)
                + ICON_BTN / 2.0
        }
    };
    iced::Point::new(x, y)
}

/// The tool strip: the project's tools down the window's right edge, each
/// naming itself (and its shortcut) to the left on hover.
pub(crate) fn tool_strip(app: &App) -> Element<'_, Message> {
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    let tool = |tool: StripTool| -> Element<'_, Message> {
        let (glyph, label, shortcut, msg) = match tool {
            StripTool::Overview => (
                Glyph::Overview,
                "Overview",
                None,
                Message::Overview(OverviewMsg::Show),
            ),
            StripTool::Stats => (
                Glyph::Stats,
                "Stats",
                None,
                Message::Overview(OverviewMsg::ShowStats),
            ),
            StripTool::Debug => (
                Glyph::Debug,
                "Debug",
                caps(Action::StartDebug),
                Message::Debug(DebugMsg::Start),
            ),
            StripTool::CallGraph => (
                Glyph::Network,
                "Call Graph",
                caps(Action::CallGraph),
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectCalls)),
            ),
            StripTool::ImportGraph => (
                Glyph::ImportGraph,
                "Import Graph",
                caps(Action::ImportGraph),
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectImports)),
            ),
            StripTool::TypeMap => (
                Glyph::TypeMap,
                "Type Map",
                caps(Action::TypeGraph),
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectTypes)),
            ),
            StripTool::ProjectMaps => (
                Glyph::ProjectMaps,
                "Project Maps",
                caps(Action::ProjectMaps),
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectMaps)),
            ),
            StripTool::More => (
                Glyph::More,
                "More",
                None,
                Message::Window(WindowMsg::ToggleToolsMenu),
            ),
            StripTool::Settings => (
                Glyph::Settings,
                "Settings",
                Some(crate::keymap::local_keys("⌘,")),
                Message::Settings(SettingsMsg::Open),
            ),
        };
        // Its menu open, the ⋯ is lit: drawn in the text's colour.
        let on = tool == StripTool::More && app.show_tools_menu;
        let tint = if on { theme::fg() } else { theme::fg_muted() };
        tip_at(
            icon_button(glyph, tint, on).on_press(msg),
            label,
            shortcut,
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
    for (i, t) in STRIP_TOP.iter().enumerate() {
        if STRIP_RULE_BEFORE.contains(&i) {
            strip = strip.push(rule());
        }
        strip = strip.push(tool(*t));
    }
    strip = strip
        .push(space().height(Fill))
        .push(tool(StripTool::Settings));
    // Windows and Linux keep the right panel's toggle here, at the foot:
    // their top-right corner belongs to the window buttons.
    if !MAC_CHROME {
        strip = strip.push(tip_at(
            icon_button(
                Glyph::PanelRight,
                if app.show_right_panel {
                    theme::fg_muted()
                } else {
                    theme::dim()
                },
                false,
            )
            .on_press(Message::Window(WindowMsg::ToggleRightPanel)),
            "Toggle panel",
            None,
            tooltip::Position::Left,
        ));
    }
    container(strip.padding(Padding {
        top: STRIP_PAD_TOP,
        bottom: STRIP_PAD_BOTTOM,
        ..Padding::ZERO
    }))
    .width(Length::Fixed(STRIP_W))
    .height(Fill)
    .style(theme::chrome)
    .into()
}

// ------------------------------------------------------------------ ⋯ menu

/// A row of the ⋯ menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolsRow {
    ExplainAll,
    Walkthrough,
    Glossary,
    OpenFolder,
    OpenRemote,
    ExportNotes,
    LspServers,
    Shortcuts,
    Tutorial,
}

/// The ⋯ menu's rows, top to bottom, in three groups: understanding the
/// project, opening another, and the rest. The menu is built from this list,
/// and the tutorial points at rows by their position in it — so the two
/// cannot drift.
pub(crate) const TOOLS_MENU_ROWS: [ToolsRow; 9] = [
    ToolsRow::ExplainAll,
    ToolsRow::Walkthrough,
    ToolsRow::Glossary,
    ToolsRow::OpenFolder,
    ToolsRow::OpenRemote,
    ToolsRow::ExportNotes,
    ToolsRow::LspServers,
    ToolsRow::Shortcuts,
    ToolsRow::Tutorial,
];

/// The [`TOOLS_MENU_ROWS`] positions a separator stands before.
pub(crate) const TOOLS_MENU_SEPARATORS: [usize; 2] = [3, 5];
/// The [`TOOLS_MENU_ROWS`] positions a heading stands over, and what it
/// says: the group of rows it names.
pub(crate) const TOOLS_MENU_HEADINGS: [(usize, &str); 2] =
    [(0, "Understand the project"), (3, "Open")];

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

/// Width of a menu panel.
pub(crate) const TOOLS_MENU_W: f32 = 248.0;
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

/// Distance of the ⋯ menu panel from the window's top edge: level with the
/// ⋯ button it opens from.
pub(crate) fn tools_menu_top(app: &App) -> f32 {
    strip_tool_center(app, StripTool::More).y - ICON_BTN / 2.0
}

/// Distance of the ⋯ menu panel from the window's right edge: just left of
/// the strip.
pub(crate) const TOOLS_MENU_RIGHT: f32 = STRIP_W + STRIP_MENU_GAP;

/// Window-space rectangle covering `count` consecutive ⋯ menu rows starting
/// at row `first`, spanning the item column (inside the panel's padding).
pub(crate) fn tools_menu_rows_rect(app: &App, first: usize, count: usize) -> iced::Rectangle {
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
        x: app.window_width - TOOLS_MENU_RIGHT - TOOLS_MENU_W + MENU_PAD,
        y,
        width: TOOLS_MENU_W - 2.0 * MENU_PAD,
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
        Some(g) => glyph::icon(g, theme::fg_muted(), 15.0),
        None => space().width(15).into(),
    };
    let mut b = button(
        row![
            container(gutter).width(15),
            ts::set_in(text(label), MENU_ROW_H, ts::INTER, ts::BASE),
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
        b = b.on_press(msg);
    }
    b.into()
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
    text(app.keymap.chord(action).caps())
        .size(ts::SMALL)
        .color(theme::dim())
        .into()
}

/// The ⋯ menu: what the strip has no room for, in [`TOOLS_MENU_ROWS`] order,
/// opening to the left of its button. Its rows are a glyph and a name: the
/// keys of those that have them are in the palette and the shortcuts.
pub(crate) fn tools_menu(app: &App) -> Element<'_, Message> {
    let has_project = app.proj.project.is_some();
    let mut items: Vec<Element<'_, Message>> = Vec::new();
    for (i, &r) in TOOLS_MENU_ROWS.iter().enumerate() {
        if TOOLS_MENU_SEPARATORS.contains(&i) {
            items.push(menu_separator());
        }
        if let Some(&(_, heading)) = TOOLS_MENU_HEADINGS.iter().find(|&&(h, _)| h == i) {
            items.push(menu_heading(heading));
        }
        let item = match r {
            // While a pass runs the row cancels it — the bottom-up pass is
            // thousands of model calls on a big repo, so it must be
            // stoppable; with no key configured it routes to Settings.
            ToolsRow::ExplainAll if app.proj.explain.running => {
                let label = match app.proj.explain.progress {
                    Some((done, total)) if total > 0 => {
                        format!("Cancel explaining ({done}/{total})")
                    }
                    _ => "Cancel explaining".to_string(),
                };
                menu_item(
                    Some(Glyph::Close),
                    label,
                    slot(),
                    Some(Message::Explain(ExplainMsg::Cancel)),
                )
            }
            ToolsRow::ExplainAll => menu_item(
                Some(Glyph::TextLines),
                "Explain All".into(),
                slot(),
                Some(if app.llm_available {
                    Message::Explain(ExplainMsg::Project)
                } else {
                    Message::Settings(SettingsMsg::Open)
                }),
            ),
            ToolsRow::Walkthrough => menu_item(
                Some(Glyph::ListOrdered),
                "Walkthrough".into(),
                slot(),
                Some(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Walk,
                ))),
            ),
            ToolsRow::Glossary => menu_item(
                Some(Glyph::Book),
                "Glossary".into(),
                slot(),
                has_project.then_some(Message::Glossary(GlossaryMsg::Open)),
            ),
            ToolsRow::OpenFolder => menu_item(
                Some(Glyph::FolderOpen),
                "Open Folder…".into(),
                slot(),
                Some(Message::Project(ProjectMsg::OpenFolderPressed)),
            ),
            ToolsRow::OpenRemote => menu_item(
                Some(Glyph::Servers),
                "Open Remote…".into(),
                slot(),
                Some(Message::Connect(ConnectMsg::Open)),
            ),
            ToolsRow::ExportNotes => menu_item(
                Some(Glyph::Export),
                "Export Notes…".into(),
                slot(),
                has_project.then_some(Message::Export(ExportMsg::Start)),
            ),
            ToolsRow::LspServers => menu_item(
                Some(Glyph::Plug),
                "Language Servers".into(),
                slot(),
                Some(Message::Lsp(LspMsg::TogglePanel)),
            ),
            ToolsRow::Shortcuts => menu_item(
                Some(Glyph::Shortcuts),
                "Keyboard Shortcuts".into(),
                slot(),
                Some(Message::Window(WindowMsg::OpenShortcuts)),
            ),
            // The tour needs a project to show: without one the row stays
            // (the menu keeps its shape) but is inert.
            ToolsRow::Tutorial => menu_item(
                Some(Glyph::Help),
                "Tutorial".into(),
                slot(),
                has_project.then_some(Message::Tutorial(TutorialMsg::Start)),
            ),
        };
        items.push(item);
    }
    let panel = container(Column::with_children(items).spacing(MENU_ROW_SPACING))
        .width(TOOLS_MENU_W)
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
}

/// The View menu's rows, top to bottom.
pub(crate) const VIEW_MENU_ROWS: [ViewRow; 7] = [
    ViewRow::OutlineSummaries,
    ViewRow::FileSummary,
    ViewRow::Blocks,
    ViewRow::InlayHints,
    ViewRow::InlineBlame,
    ViewRow::IndentGuides,
    ViewRow::Minimap,
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
        }
    }
}

/// Whether `message` is a pick from the View menu, which closes it.
pub(crate) fn closes_view_menu(message: &Message) -> bool {
    matches!(
        message,
        Message::Window(WindowMsg::ToggleInlineSummaries)
            | Message::Window(WindowMsg::ToggleFileBanner)
            | Message::Explain(ExplainMsg::ToggleBlocks)
            | Message::Lsp(LspMsg::ToggleInlayHints)
            | Message::Window(WindowMsg::ToggleInlineBlame)
            | Message::Window(WindowMsg::ToggleIndentGuides)
            | Message::Window(WindowMsg::ToggleMinimap)
    )
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
    x.min(app.window_width - STRIP_W - VIEW_MENU_W - 8.0)
        .max(8.0)
}

/// Width of the View menu.
pub(crate) const VIEW_MENU_W: f32 = 220.0;

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
                ViewRow::OutlineSummaries => {
                    (Glyph::Note, "Outline summaries", app.show_inline_summaries)
                }
                ViewRow::FileSummary => (Glyph::Info, "File summary", app.show_file_banner),
                ViewRow::Blocks => (
                    Glyph::List,
                    "Explanation in blocks",
                    app.proj.explain.showing_detail,
                ),
                ViewRow::InlayHints => (Glyph::Lightbulb, "Inlay hints", app.show_inlay_hints),
                ViewRow::InlineBlame => (Glyph::TimeTravel, "Inline blame", app.show_inline_blame),
                ViewRow::IndentGuides => {
                    (Glyph::TextLines, "Indent guides", app.show_indent_guides)
                }
                ViewRow::Minimap => (Glyph::Minimap, "Minimap", app.show_minimap),
            };
            // Blocks are a function's: with no function explained, the row
            // is there, inert.
            let enabled = r != ViewRow::Blocks
                || matches!(
                    app.proj.explain.view,
                    Some(crate::explain::Node::Function { .. })
                );
            menu_item(
                Some(glyph),
                label.into(),
                check(on),
                enabled.then(|| r.message()),
            )
        })
        .collect();
    let panel = container(Column::with_children(items).spacing(MENU_ROW_SPACING))
        .width(VIEW_MENU_W)
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

/// Width of the app menu.
const APP_MENU_W: f32 = 280.0;

/// The rows of the app menu that are not keymap actions, top first.
fn app_menu_head(app: &App) -> Vec<(Glyph, &'static str, Option<String>, Message)> {
    let _ = app;
    vec![
        (
            Glyph::Folder,
            "Open Folder…",
            None,
            Message::Project(ProjectMsg::OpenFolderPressed),
        ),
        (
            Glyph::Remote,
            "Open Remote…",
            None,
            Message::Connect(ConnectMsg::Open),
        ),
        (
            Glyph::Settings,
            "Settings…",
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
/// action under the title the menu bar gives it (Edit, View, Go), each with
/// its shortcut.
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
            items.push(menu_item(
                None,
                placed.title.to_string(),
                menu_chord(app, action),
                Some(Message::Editor(EditorMsg::RunAction(action))),
            ));
        }
    }
    items.push(menu_separator());
    items.push(menu_item(
        Some(Glyph::Close),
        "Close Window".into(),
        slot(),
        Some(Message::Window(WindowMsg::Close)),
    ));
    let max_h = (app.window_height - TOOLBAR_H - 24.0).max(160.0);
    let panel = container(
        scrollable(Column::with_children(items).spacing(MENU_ROW_SPACING))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar),
    )
    .width(APP_MENU_W)
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
                    text(t.to_string()).size(ts::BODY)
                ]
                .align_y(iced::Center),
            )
            .style(theme::menu_row)
            .width(Fill)
            .padding([5, 8])
            .on_press(Message::Reading(ReadingMsg::TargetSelected(t)))
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

    /// The strip's slots go down one pitch apart, a rule's slot more after
    /// each group, and follow the window's right edge.
    #[test]
    fn the_strip_slots_follow_the_groups_and_the_right_edge() {
        let mut app = crate::app::tests::blank_app();
        app.window_width = 1280.0;
        app.window_height = 900.0;
        let y = |app: &App, t| strip_tool_center(app, t).y;
        let pitch = ICON_BTN + STRIP_SPACING;
        let rule = STRIP_RULE_H + STRIP_SPACING;
        assert_eq!(
            y(&app, StripTool::Stats) - y(&app, StripTool::Overview),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::Debug) - y(&app, StripTool::Stats),
            pitch + rule
        );
        assert_eq!(
            y(&app, StripTool::CallGraph) - y(&app, StripTool::Debug),
            pitch + rule
        );
        assert_eq!(
            y(&app, StripTool::TypeMap) - y(&app, StripTool::ImportGraph),
            pitch
        );
        assert_eq!(
            y(&app, StripTool::More) - y(&app, StripTool::ProjectMaps),
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
        let r = chat_field_rect(&app);
        assert_eq!(r.width, FIELD_MAX_W);
        let room_left = left_cluster_w(&app) + NAV_W;
        let room_right = app.window_width - right_cluster_w();
        assert!(
            ((r.x - room_left) - (room_right - (r.x + r.width))).abs() < 0.5,
            "{r:?}"
        );
        app.window_width = 600.0;
        assert!(chat_field_rect(&app).width < FIELD_MAX_W);
    }
}
