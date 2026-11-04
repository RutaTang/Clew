//! The Ask window, floating over the code rather than taking a panel's
//! room from it: a launcher in the bottom-right corner while it is put away;
//! a card there while it is open — moved to the top when it would cover the
//! line the caret is on — and, made large, a window centred over the
//! dimmed rest. It holds the conversation ([`ask_panel`]), and shows what
//! voice is doing ([`voice_strip`]).
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
pub(crate) const ASK_CARD_W: f32 = 420.0;
pub(crate) const ASK_CARD_H: f32 = 560.0;
/// Least height the card shrinks to in a short window (or over a tall
/// debugger's panel).
pub(crate) const ASK_CARD_MIN_H: f32 = 120.0;
/// Space between the card (or the launcher) and the edges around it.
pub(crate) const ASK_MARGIN: f32 = 16.0;
/// Widest the large window grows, and its share of the window.
pub(crate) const ASK_LARGE_MAX_W: f32 = 960.0;
pub(crate) const ASK_LARGE_SHARE: f32 = 0.72;

/// Where the open Ask window is drawn, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AskPlace {
    pub width: f32,
    pub height: f32,
    /// The card sits at the top of its corner (it moved out of the caret's
    /// way) rather than at the bottom.
    pub at_top: bool,
}

/// The band of the window the floating Ask may use: under the toolbar (and
/// the update banner), over the status bar — and, for the card, over the
/// debugger's panel too: the large window is over everything.
fn band(app: &App) -> (f32, f32) {
    let top = TOOLBAR_H + update_banner_height(app) + ASK_MARGIN;
    let bottom_panel = if app.show_bottom && app.ask_view != AskView::Expanded {
        app.bottom_height + crate::resize::THICKNESS
    } else {
        0.0
    };
    let bottom = app.window_height - STATUSBAR_H - bottom_panel - ASK_MARGIN;
    (top, bottom.max(top))
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
            let at_bottom = (bottom - height, bottom);
            let at_top = (top, top + height);
            // Moved to the top when the caret's line is under the card —
            // unless it would be under it there too (a short window).
            let moved = caret_line_y(app).is_some_and(|(y0, y1)| {
                let under = |(a, b): (f32, f32)| y1 > a && y0 < b;
                under(at_bottom) && !under(at_top)
            });
            Some(AskPlace {
                width,
                height,
                at_top: moved,
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

/// The floating Ask layer of the window: its launcher, its card, or its
/// large window over a dimmed backdrop. Nothing without a project, unless
/// voice has something to show.
pub(crate) fn ask_layer(app: &App) -> Element<'_, Message> {
    let Some(place) = ask_place(app) else {
        if app.proj.project.is_none() && !app.voice.is_busy() {
            return slot();
        }
        return container(launcher(app))
            .width(Fill)
            .height(Fill)
            .align_x(iced::alignment::Horizontal::Right)
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(Padding {
                bottom: app.window_height - band(app).1,
                right: ASK_MARGIN,
                ..Padding::ZERO
            })
            .into();
    };
    let (top, bottom) = band(app);
    let card = container(opaque(ask_card(app, place)))
        .width(place.width)
        .height(place.height)
        .style(theme::floating_card);
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
            .align_x(iced::alignment::Horizontal::Right)
            .align_y(iced::alignment::Vertical::Top)
            .padding(Padding {
                top,
                right: ASK_MARGIN,
                ..Padding::ZERO
            })
    } else {
        frame
            .align_x(iced::alignment::Horizontal::Right)
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(Padding {
                bottom: app.window_height - bottom,
                right: ASK_MARGIN,
                ..Padding::ZERO
            })
    };
    stack![backdrop, frame].into()
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
fn ask_card(app: &App, place: AskPlace) -> Element<'_, Message> {
    let large = app.ask_view == AskView::Expanded;
    let mut header = row![
        glyph::icon(Glyph::Ask, theme::accent(), 16.0),
        text("Ask").size(ts::BASE).color(theme::fg()),
        space().width(Fill),
    ]
    .spacing(6)
    .align_y(iced::Center);
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
        ask_panel(app, place.height),
    ]
    .height(Fill)
    .into()
}

/// The launcher, while the Ask window is put away: a pill to open it, and
/// the microphone — and, while voice is busy, what it is doing.
fn launcher(app: &App) -> Element<'_, Message> {
    let chord = app.keymap.chord(crate::keymap::Action::ToggleAsk).caps();
    let open = button(
        row![
            glyph::icon(Glyph::Ask, theme::accent(), 15.0),
            text("Ask").size(ts::BODY).color(theme::fg()),
            text(chord).size(ts::CAPTION).color(theme::dim()),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding([5, 10])
    .on_press(Message::Ask(AskMsg::Open));
    let mut pill = row![].spacing(2).align_y(iced::Center);
    if let Some(strip) = voice_status(app) {
        pill = pill.push(container(strip).padding([0, 8]));
    } else if app.proj.project.is_some() {
        pill = pill.push(open);
    }
    pill = pill.push(mic_button(app));
    opaque(container(pill).padding([3, 4]).style(theme::launcher_pill))
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
