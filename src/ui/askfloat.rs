//! The Ask window: the conversation with Clew, opened from the chat field
//! in the top bar and floating over the code rather than taking a panel's
//! room from it. A card hangs under the field while it is open — moved to
//! the bottom when it would cover the line the caret is on — and, made
//! large, a window centred over the dimmed rest. It holds the conversation
//! ([`ask_panel`]) and shows what voice is doing ([`voice_strip`]); while it
//! is put away, voice listening shows a panel of its own under the field
//! ([`voice_panel`]).
//!
//! The card and the large window are one widget tree, sized and placed
//! differently: going from one to the other keeps the conversation's
//! scroll and the input's text where they were.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::VoiceMsg;
use crate::app::model::AskView;
use crate::app::voice::{HeardOutcome, VoicePhase};

/// The card's size, where the window has room for it.
pub(crate) const ASK_CARD_W: f32 = 560.0;
pub(crate) const ASK_CARD_H: f32 = 560.0;
/// Least height the card shrinks to in a short window (or over a tall
/// debugger's panel).
pub(crate) const ASK_CARD_MIN_H: f32 = 120.0;
/// Space between the card and the edges around it.
pub(crate) const ASK_MARGIN: f32 = 16.0;
/// Gap between the chat field and the card hanging under it.
const FIELD_GAP: f32 = 6.0;
/// Widest the large window grows, and its share of the window.
pub(crate) const ASK_LARGE_MAX_W: f32 = 960.0;
pub(crate) const ASK_LARGE_SHARE: f32 = 0.72;
/// Width of the voice panel that hangs under the field while Ask is away.
const VOICE_PANEL_W: f32 = 460.0;

/// Where the open Ask window is drawn, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AskPlace {
    pub width: f32,
    pub height: f32,
    /// Its left edge: under the chat field for the card, centred for the
    /// large window.
    pub left: f32,
    /// The card hangs from the top, under the chat field — rather than
    /// sitting at the bottom, where it moved out of the caret's way.
    pub at_top: bool,
}

/// The band of the window the floating Ask may use: under the toolbar (and
/// the update banner), over the status bar — and, for the card, over the
/// debugger's panel too: the large window is over everything.
fn band(app: &App) -> (f32, f32) {
    let top = TOOLBAR_H + update_banner_height(app) + FIELD_GAP;
    let bottom_panel = if app.show_bottom && app.ask_view != AskView::Expanded {
        app.bottom_height + crate::resize::THICKNESS
    } else {
        0.0
    };
    let bottom = app.window_height - STATUSBAR_H - bottom_panel - ASK_MARGIN;
    (top, bottom.max(top))
}

/// The left edge of a panel `width` wide hanging under the chat field:
/// centred on it, kept inside the window.
fn under_field(app: &App, width: f32) -> f32 {
    let field = chat_field_rect(app);
    let centre = field.x + field.width / 2.0;
    (centre - width / 2.0).clamp(
        ASK_MARGIN,
        (app.window_width - width - ASK_MARGIN).max(ASK_MARGIN),
    )
}

/// Where the Ask window is drawn now; `None` while it is put away.
pub(crate) fn ask_place(app: &App) -> Option<AskPlace> {
    let (top, bottom) = band(app);
    match app.ask_view {
        AskView::Closed => None,
        AskView::Compact => {
            let height = ASK_CARD_H.min(bottom - top).max(ASK_CARD_MIN_H);
            let width = ASK_CARD_W
                .min(app.window_width - 2.0 * ASK_MARGIN)
                .max(200.0);
            let at_top = (top, top + height);
            let at_bottom = (bottom - height, bottom);
            // Moved to the bottom when the caret's line is under the card —
            // unless it would be under it there too (a short window).
            let moved = caret_line_y(app).is_some_and(|(y0, y1)| {
                let under = |(a, b): (f32, f32)| y1 > a && y0 < b;
                under(at_top) && !under(at_bottom)
            });
            Some(AskPlace {
                width,
                height,
                left: under_field(app, width),
                at_top: !moved,
            })
        }
        AskView::Expanded => {
            let width = (app.window_width * ASK_LARGE_SHARE)
                .min(ASK_LARGE_MAX_W)
                .max(ASK_CARD_W.min(app.window_width - 2.0 * ASK_MARGIN));
            let height = ((bottom - top) * 0.92).max(ASK_CARD_MIN_H);
            Some(AskPlace {
                width,
                height,
                left: ((app.window_width - width) / 2.0).max(0.0),
                at_top: false,
            })
        }
    }
}

/// Where the active pane's caret line is on screen, top and bottom, in
/// window coordinates; `None` with no caret, or with it scrolled out of
/// view. The code fills its pane to the pane's bottom, which sits on the
/// band's lower edge.
fn caret_line_y(app: &App) -> Option<(f32, f32)> {
    let v = app.active_viewer()?;
    let (line, _) = v.caret?;
    let lh = app.line_height();
    let y = v.row_of(line) as f32 * lh - v.scroll_y;
    if y + lh < 0.0 || y > v.viewport_h {
        return None;
    }
    let (_, bottom) = band(app);
    let pane_bottom = bottom + ASK_MARGIN;
    let pane_top = pane_bottom - v.viewport_h;
    Some((pane_top + y, pane_top + y + lh))
}

/// The floating Ask layer of the window: its card under the chat field, or
/// its large window over a dimmed backdrop — or, while it is put away and
/// voice is listening, the voice panel under the field. Nothing otherwise:
/// the chat field in the top bar is where Ask opens from.
pub(crate) fn ask_layer(app: &App) -> Element<'_, Message> {
    let Some(place) = ask_place(app) else {
        if !app.voice.is_busy() {
            return slot();
        }
        let width = VOICE_PANEL_W.min(app.window_width - 2.0 * ASK_MARGIN);
        return container(container(opaque(voice_panel(app))).width(width))
            .width(Fill)
            .height(Fill)
            .padding(Padding {
                top: band(app).0,
                left: under_field(app, width),
                ..Padding::ZERO
            })
            .into();
    };
    let (top, bottom) = band(app);
    let card = container(opaque(ask_card(app, place)))
        .width(place.width)
        .style(theme::floating_card);
    let card = if card_fits(app) {
        card.max_height(place.height)
    } else {
        card.height(place.height)
    };
    let large = app.ask_view == AskView::Expanded;
    // A click on the dimmed window makes the large window a card again.
    // Under the card, the layer is still the whole window — a stack is as
    // large as its first layer — but takes no clicks: the code under it
    // gets them.
    let backdrop: Element<'_, Message> = if large {
        opaque(
            mouse_area(
                container(space())
                    .width(Fill)
                    .height(Fill)
                    .style(theme::backdrop),
            )
            .on_press(Message::Ask(AskMsg::Shrink)),
        )
    } else {
        container(space()).width(Fill).height(Fill).into()
    };
    let frame = container(card).width(Fill).height(Fill);
    let frame = if large {
        frame
            .align_x(iced::Center)
            .align_y(iced::Center)
            .padding(Padding {
                top,
                bottom: app.window_height - bottom,
                ..Padding::ZERO
            })
    } else if place.at_top {
        frame
            .align_y(iced::alignment::Vertical::Top)
            .padding(Padding {
                top,
                left: place.left,
                ..Padding::ZERO
            })
    } else {
        frame
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(Padding {
                bottom: app.window_height - bottom,
                left: place.left,
                ..Padding::ZERO
            })
    };
    stack![backdrop, frame].into()
}

/// What can be said, and what comes of it: shown while voice listens.
const TRY_SAYING: [(&str, &str); 5] = [
    ("“Open the call graph”", "Clew runs it"),
    ("“Hide the minimap”", "Clew runs it"),
    (
        "“Take me to where the config is parsed”",
        "Clew finds it and goes there",
    ),
    ("“Why is this function here?”", "Clew answers"),
    ("“Start debugging”", "Clew asks you first"),
];

/// The voice panel, under the chat field while Ask is put away: how loud
/// the microphone hears, what can be said, and that what is said stays on
/// this computer.
fn voice_panel(app: &App) -> Element<'_, Message> {
    let (title, hint, level): (&str, String, f32) = match app.voice.phase {
        VoicePhase::Listening { level, .. } => (
            "Talk to Clew",
            "Ask anything, or say what to do. Stop talking to finish, Esc to cancel.".into(),
            level,
        ),
        _ => (
            "Working out what you said…",
            "On this computer, with the model you chose.".into(),
            0.0,
        ),
    };
    let mut tries = Column::new().spacing(2);
    for (say, does) in TRY_SAYING {
        tries = tries.push(
            row![
                text(say).size(ts::BODY).color(theme::fg()).width(Fill),
                container(text(does).size(ts::CAPTION).color(theme::fg_muted()))
                    .padding([2, 7])
                    .style(theme::chip),
            ]
            .spacing(10)
            .align_y(iced::Center)
            .padding([5, 10]),
        );
    }
    let model = app.voice.prefs.model().name;
    container(column![
        column![
            container(
                progress_bar(0.0..=1.0, level)
                    .length(160)
                    .girth(5)
                    .style(theme::level_meter)
            )
            .width(160),
            text(title).size(ts::SUBTITLE).color(theme::fg_bright()),
            text(hint).size(ts::SMALL).color(theme::fg_muted()),
        ]
        .spacing(9)
        .align_x(iced::Center)
        .width(Fill)
        .padding([20, 16]),
        hairline(),
        container(
            column![
                text("Try saying").size(ts::SMALL).color(theme::dim()),
                tries,
            ]
            .spacing(4),
        )
        .padding([8, 6]),
        hairline(),
        row![
            glyph::icon(Glyph::Lock, theme::fg_muted(), 13.0),
            text(format!(
                "Transcribed on this computer by Whisper {model}. Nothing you say \
                     leaves it."
            ))
            .size(ts::SMALL)
            .color(theme::fg_muted()),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .padding([9, 14]),
    ])
    .style(theme::floating_card)
    .into()
}

/// A small borderless control of the card's header.
fn header_button<'a>(g: Glyph, tip: &'a str, msg: Message) -> Element<'a, Message> {
    tooltip(
        button(glyph::icon(g, theme::fg_muted(), 15.0))
            .style(theme::toolbar_button)
            .padding([4, 6])
            .on_press(msg),
        text(tip).size(ts::CAPTION),
        tooltip::Position::Bottom,
    )
    .style(theme::modal_panel)
    .padding(6)
    .into()
}

/// The microphone button: starts listening, or stops it.
fn mic_button<'a>(app: &App) -> Element<'a, Message> {
    let listening = app.voice.is_listening();
    let chord = app.keymap.chord(crate::keymap::Action::Voice).caps();
    let tint = if listening {
        theme::danger()
    } else {
        theme::fg_muted()
    };
    tooltip(
        button(glyph::icon(Glyph::Mic, tint, 15.0))
            .style(theme::toolbar_button)
            .padding([4, 6])
            .on_press(Message::Voice(VoiceMsg::Toggle)),
        text(if listening {
            format!("Stop listening ({chord})")
        } else {
            format!("Talk ({chord})")
        })
        .size(ts::CAPTION),
        tooltip::Position::Bottom,
    )
    .style(theme::modal_panel)
    .padding(6)
    .into()
}

/// The card: its header (what it is, voice, size, close), what voice is
/// doing, and the conversation.
/// Whether the card fits its height to its conversation, as the card under
/// the chat field does. The large window keeps its height, and so does a
/// card asking for a tool call's approval: that card's room is half the
/// card's height, the input kept in view under it.
fn card_fits(app: &App) -> bool {
    app.ask_view == AskView::Compact && app.first_approval().is_none()
}

fn ask_card(app: &App, place: AskPlace) -> Element<'_, Message> {
    let large = app.ask_view == AskView::Expanded;
    let fit = card_fits(app);
    let mut header = row![
        glyph::icon(Glyph::Ask, theme::accent(), 16.0),
        text("Ask")
            .size(ts::BASE)
            .font(crate::fonts::UI_SEMIBOLD)
            .color(theme::fg()),
    ]
    .spacing(8)
    .align_y(iced::Center);
    // What the reader is on, which is what a question is asked against.
    if let Some(v) = app.active_viewer() {
        let name = v.rel.rsplit('/').next().unwrap_or(&v.rel).to_string();
        let on = match &app.proj.explain.view {
            Some(crate::explain::Node::Function { file, name: f, .. }) if *file == v.abs => {
                format!("Reading {name} · {f}")
            }
            _ => format!("Reading {name}"),
        };
        header = header.push(
            container(
                text(on)
                    .size(ts::CAPTION)
                    .color(theme::fg_muted())
                    .wrapping(Wrapping::None),
            )
            .padding([2, 7])
            .style(theme::chip),
        );
    }
    header = header.push(space().width(Fill));
    if !app.proj.ask_turns.is_empty() {
        header = header.push(
            button(text("Clear").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Ask(AskMsg::Clear)),
        );
    }
    header = header
        .push(mic_button(app))
        .push(if large {
            header_button(
                Glyph::Shrink,
                "Make small (Esc)",
                Message::Ask(AskMsg::Shrink),
            )
        } else {
            header_button(Glyph::Expand, "Make large", Message::Ask(AskMsg::Expand))
        })
        .push(header_button(
            Glyph::Close,
            "Put away (Esc)",
            Message::Ask(AskMsg::Close),
        ));
    column![
        container(header).padding(Padding {
            top: 8.0,
            right: 8.0,
            bottom: 2.0,
            left: 12.0,
        }),
        hairline(),
        voice_strip(app).unwrap_or_else(slot),
        if fit {
            ask_panel_fitted(app, place.height)
        } else {
            ask_panel(app, place.height)
        },
    ]
    .height(if fit { Length::Shrink } else { Fill })
    .into()
}

/// What voice is doing, as one line: listening (with the level), working
/// out the words, or a command waiting for a yes. `None` while idle.
fn voice_status(app: &App) -> Option<Element<'_, Message>> {
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
    if let Some(title) = &app.voice.confirm {
        return Some(
            row![
                text(format!("Run “{title}”?"))
                    .size(ts::SMALL)
                    .color(theme::fg()),
                small("Run", Message::Voice(VoiceMsg::Confirm), true),
                small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
            ]
            .spacing(6)
            .align_y(iced::Center)
            .into(),
        );
    }
    match app.voice.phase {
        VoicePhase::Idle => None,
        VoicePhase::Listening { level, .. } => Some(
            row![
                text("●").size(ts::SMALL).color(theme::danger()),
                text("Listening").size(ts::SMALL).color(theme::fg()),
                container(
                    progress_bar(0.0..=1.0, level)
                        .length(64)
                        .girth(4)
                        .style(theme::level_meter)
                )
                .width(64),
                small("Done", Message::Voice(VoiceMsg::Stop), true),
            ]
            .spacing(6)
            .align_y(iced::Center)
            .into(),
        ),
        VoicePhase::Transcribing { .. } => Some(
            row![
                text("Working out what you said…")
                    .size(ts::SMALL)
                    .color(theme::dim()),
                small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
            ]
            .spacing(6)
            .align_y(iced::Center)
            .into(),
        ),
    }
}

/// Voice on the card: what it is doing ([`voice_status`]) or, idle, what
/// was last heard and what came of it. `None` when there is nothing to say.
pub(crate) fn voice_strip(app: &App) -> Option<Element<'_, Message>> {
    let line: Element<'_, Message> = match voice_status(app) {
        Some(status) => status,
        None => {
            let heard = app.voice.heard.as_ref()?;
            let (said, color) = match &heard.outcome {
                HeardOutcome::Ran(title) => (format!("→ {title}"), theme::fg_muted()),
                HeardOutcome::Confirm(title) => (format!("→ {title}?"), theme::fg_muted()),
                HeardOutcome::Asked => ("→ asked".to_string(), theme::fg_muted()),
                HeardOutcome::Nothing => ("Didn't catch that".to_string(), theme::dim()),
                HeardOutcome::Failed(e) => (e.clone(), theme::warn()),
            };
            let mut line = row![].spacing(6).align_y(iced::Center);
            if !heard.text.is_empty() {
                line = line.push(
                    text(format!("“{}”", heard.text))
                        .size(ts::SMALL)
                        .color(theme::fg()),
                );
            }
            line.push(text(said).size(ts::SMALL).color(color)).into()
        }
    };
    Some(container(line).padding([6, 12]).width(Fill).into())
}
