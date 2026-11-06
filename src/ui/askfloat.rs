//! The Ask window: the conversation with Clew about the whole project. It
//! is the window's, not a file's, so it floats in the window's bottom-right
//! corner — over the margin's corner, the code above and beside it in view —
//! rather than taking a panel's room. Hidden, it is a pill in that corner;
//! made large, a window centred over the dimmed rest. Voice lives there too:
//! the pill says what it hears while the card is hidden ([`ask_pill`]), the
//! card while it is open ([`voice_strip`]).
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
pub(crate) const ASK_CARD_W: f32 = 400.0;
pub(crate) const ASK_CARD_H: f32 = 520.0;
/// Least height the card shrinks to in a short window (or over a tall
/// debugger's panel).
pub(crate) const ASK_CARD_MIN_H: f32 = 120.0;
/// Space between the card and the edges around it.
pub(crate) const ASK_MARGIN: f32 = 16.0;
/// Gap between the window's top bar and what Ask may cover under it.
const TOP_GAP: f32 = 6.0;
/// Space between the card (or the pill) and the tool strip at its right.
const STRIP_GAP: f32 = 12.0;
/// Widest the large window grows, and its share of the window.
pub(crate) const ASK_LARGE_MAX_W: f32 = 960.0;
pub(crate) const ASK_LARGE_SHARE: f32 = 0.72;
/// Height of the pill Ask hides to.
pub(crate) const PILL_H: f32 = 40.0;

/// Where the open Ask window is drawn, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AskPlace {
    pub width: f32,
    pub height: f32,
    /// Its left edge: in the corner for the card, centred for the large
    /// window.
    pub left: f32,
}

/// The band of the window the floating Ask may use: under the toolbar (and
/// the update banner), over the status bar — and, for the card, over the
/// debugger's panel too: the large window is over everything.
fn band(app: &App) -> (f32, f32) {
    let top = TOOLBAR_H + update_banner_height(app) + TOP_GAP;
    let bottom_panel = if app.show_bottom && app.ask_view != AskView::Expanded {
        app.bottom_height + crate::resize::THICKNESS
    } else {
        0.0
    };
    let bottom = app.window_height - STATUSBAR_H - bottom_panel - ASK_MARGIN;
    (top, bottom.max(top))
}

/// The corner the card and the pill sit in, as how far their right edge is
/// from the window's (left of the tool strip) and their bottom edge from its
/// bottom (over the status bar, and the debugger's panel when it shows).
fn corner(app: &App) -> (f32, f32) {
    let (_, bottom) = band(app);
    (STRIP_W + STRIP_GAP, app.window_height - bottom)
}

/// Whether the conversation is a card in the corner.
pub(crate) fn ask_is_card(app: &App) -> bool {
    app.ask_view == AskView::Compact
}

/// Where the Ask window is drawn now; `None` while it is hidden to its pill
/// or put away.
pub(crate) fn ask_place(app: &App) -> Option<AskPlace> {
    let (top, bottom) = band(app);
    match app.ask_view {
        AskView::Closed | AskView::Hidden => None,
        AskView::Compact => {
            let (right, _) = corner(app);
            let width = ASK_CARD_W
                .min(app.window_width - right - ASK_MARGIN)
                .max(200.0);
            let height = ASK_CARD_H.min(bottom - top).max(ASK_CARD_MIN_H);
            Some(AskPlace {
                width,
                height,
                left: (app.window_width - right - width).max(0.0),
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
            })
        }
    }
}

/// About how wide the pill is with Ask's own face on it.
const PILL_W: f32 = 150.0;

/// Where Ask is in its corner, in window coordinates: the card, as tall as
/// it may grow, or the pill it hides to. (The large window is centred; the
/// tutorial only points at the corner.)
pub(crate) fn ask_corner_rect(app: &App) -> iced::Rectangle {
    let (right, bottom) = corner(app);
    let (width, height) = match ask_place(app) {
        Some(place) if ask_is_card(app) => (place.width, place.height),
        _ => (PILL_W, PILL_H),
    };
    iced::Rectangle {
        x: app.window_width - right - width,
        y: app.window_height - bottom - height,
        width,
        height,
    }
}

/// `what`, in the window's corner where Ask lives.
fn in_corner<'a>(app: &App, what: Element<'a, Message>) -> Element<'a, Message> {
    let (right, bottom) = corner(app);
    container(super::corner_safe::CornerSafe::new(what))
        .width(Fill)
        .height(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Bottom)
        .padding(Padding {
            right,
            bottom,
            ..Padding::ZERO
        })
        .into()
}

/// The floating Ask layer of the window: its card in the corner, or its
/// large window over a dimmed backdrop — or, while it is hidden, its pill
/// there, which says what voice is doing too.
pub(crate) fn ask_layer(app: &App) -> Element<'_, Message> {
    let Some(place) = ask_place(app) else {
        return match ask_pill(app) {
            Some(pill) => in_corner(app, pill),
            None => slot(),
        };
    };
    let (top, bottom) = band(app);
    // The border is drawn over the card's edge; what is in it starts inside.
    let card = container(opaque(ask_card(app, place)))
        .width(place.width)
        .padding(1)
        .style(theme::floating_card);
    let card = if card_fits(app) {
        card.max_height(place.height)
    } else {
        card.height(place.height)
    };
    let large = app.ask_view == AskView::Expanded;
    if !large {
        // Under the card the layer takes no clicks: the code gets them.
        return in_corner(app, card.into());
    }
    // A click on the dimmed window makes the large window a card again.
    let backdrop: Element<'_, Message> = opaque(
        mouse_area(
            container(space())
                .width(Fill)
                .height(Fill)
                .style(theme::backdrop),
        )
        .on_press(Message::Ask(AskMsg::Shrink)),
    );
    // Its shadow kept off the window's clear corners.
    let frame = container(super::corner_safe::CornerSafe::new(card))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(Padding {
            top,
            bottom: app.window_height - bottom,
            ..Padding::ZERO
        });
    stack![backdrop, frame].into()
}

/// The pill Ask hides to: what voice is doing while it hears, works out the
/// words, waits for a yes or has just done what was said — else Ask's own
/// face, a click from opening. `None` when Ask is put away and voice has
/// nothing to say.
fn ask_pill(app: &App) -> Option<Element<'_, Message>> {
    let voice = pill_voice(app);
    if app.ask_view == AskView::Closed && voice.is_none() {
        return None;
    }
    let listening = app.voice.is_listening();
    let mic = tip_at(
        button(
            container(glyph::icon(
                Glyph::Mic,
                if listening {
                    crate::theme::active().on_accent
                } else {
                    theme::fg_muted()
                },
                14.0,
            ))
            .center(Fill),
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
        Some(app.keymap.chord(crate::keymap::Action::Voice).caps()),
        tooltip::Position::Top,
    );
    let face: Element<'_, Message> = match voice {
        Some(said) => said,
        None => {
            let turns = app.proj.ask_turns.len();
            let mut face = row![
                glyph::icon(Glyph::Message, theme::accent(), 15.0),
                text("Ask")
                    .size(ts::BASE)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::fg()),
            ]
            .spacing(9)
            .align_y(iced::Center);
            if turns > 0 {
                face = face.push(
                    text(if turns == 1 {
                        "1 answer".to_string()
                    } else {
                        format!("{turns} answers")
                    })
                    .size(ts::SMALL)
                    .color(theme::dim()),
                );
            }
            tip_at(
                button(face)
                    .style(theme::toolbar_button)
                    .padding([4, 6])
                    .on_press(Message::Ask(AskMsg::Open)),
                "Ask",
                Some(app.keymap.chord(crate::keymap::Action::ToggleAsk).caps()),
                tooltip::Position::Top,
            )
        }
    };
    Some(
        container(opaque(row![face, mic].spacing(8).align_y(iced::Center)))
            .height(Length::Fixed(PILL_H))
            .align_y(iced::Center)
            .padding(Padding {
                top: 0.0,
                right: 7.0,
                bottom: 0.0,
                left: 8.0,
            })
            .style(theme::floating_pill)
            .into(),
    )
}

/// What voice says on the pill: listening (how loud, and Esc), working out
/// the words, a command waiting for a yes — or, for a moment, what was
/// just heard and what came of it. `None` when it has nothing to say, or the
/// card says it.
fn pill_voice(app: &App) -> Option<Element<'_, Message>> {
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
                    .size(ts::BASE)
                    .color(theme::fg()),
                small("Run", Message::Voice(VoiceMsg::Confirm), true),
                small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
            ]
            .spacing(6)
            .padding(Padding {
                left: 6.0,
                ..Padding::ZERO
            })
            .align_y(iced::Center)
            .into(),
        );
    }
    match app.voice.phase {
        VoicePhase::Listening { level, .. } => {
            return Some(
                row![
                    container(space().width(REC_DOT).height(REC_DOT)).style(|_: &iced::Theme| {
                        container::Style {
                            background: Some(theme::recording().into()),
                            border: iced::border::rounded(REC_DOT / 2.0),
                            ..container::Style::default()
                        }
                    }),
                    text("Listening…").size(ts::BASE).color(theme::fg()),
                    level_bars(&[5.0, 9.0, 13.0, 8.0, 11.0, 6.0, 9.0], level, false),
                    keycap("Esc".to_string()),
                ]
                .spacing(8)
                .padding(Padding {
                    left: 8.0,
                    ..Padding::ZERO
                })
                .align_y(iced::Center)
                .into(),
            );
        }
        VoicePhase::Transcribing { .. } => {
            return Some(
                row![
                    text("Working out what you said…")
                        .size(ts::BASE)
                        .color(theme::fg_muted()),
                    small("Cancel", Message::Voice(VoiceMsg::Cancel), false),
                ]
                .spacing(6)
                .padding(Padding {
                    left: 8.0,
                    ..Padding::ZERO
                })
                .align_y(iced::Center)
                .into(),
            );
        }
        VoicePhase::Idle => {}
    }
    if !app.voice.heard_fresh() {
        return None;
    }
    let heard = app.voice.heard.as_ref()?;
    let (mark, mark_color, said, said_color) = match &heard.outcome {
        HeardOutcome::Ran(title) => (Glyph::Check, theme::success(), title.clone(), theme::dim()),
        HeardOutcome::Confirm(title) => (
            Glyph::Check,
            theme::dim(),
            format!("{title}?"),
            theme::dim(),
        ),
        // A question opened the card, which says it.
        HeardOutcome::Asked => return None,
        HeardOutcome::Nothing => (
            Glyph::Mic,
            theme::dim(),
            "Didn't catch that".to_string(),
            theme::dim(),
        ),
        HeardOutcome::Failed(e) => (Glyph::Info, theme::warn(), e.clone(), theme::warn()),
    };
    let mut line = row![glyph::icon(mark, mark_color, 14.0)]
        .spacing(8)
        .padding(Padding {
            left: 8.0,
            ..Padding::ZERO
        })
        .align_y(iced::Center);
    if !heard.text.is_empty() {
        line = line.push(
            text(format!("“{}”", heard.text))
                .size(ts::BASE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
        );
    }
    Some(
        line.push(
            text(said)
                .size(ts::SMALL)
                .color(said_color)
                .wrapping(Wrapping::None),
        )
        .into(),
    )
}

/// Side of the dot that says the microphone is open.
const REC_DOT: f32 = 7.0;

/// What can be said, and what comes of it: shown while voice listens.
const TRY_SAYING: [(&str, &str); 5] = [
    ("“Open the call graph”", "Clew runs it"),
    ("“Hide the minimap”", "Clew runs it"),
    (
        "“Take me to where the config is parsed”",
        "Clew finds it and takes you there",
    ),
    ("“Why is this function here?”", "Clew answers"),
    ("“Start debugging”", "Clew asks you first"),
];

/// What can be said while voice listens, as the empty card shows it: the
/// question among them made about the function the caret is in.
pub(crate) fn try_saying(app: &App) -> Vec<(String, &'static str)> {
    let about = question_about(app);
    TRY_SAYING
        .iter()
        .map(|&(say, does)| match (&about, does) {
            (Some(about), "Clew answers") => (about.clone(), does),
            _ => (say.to_string(), does),
        })
        .collect()
}

/// A question about the function the caret is in, as a reader would put
/// it: about its first edit of a string on one character —
/// `rel.replace('\\', "/")` asks "Why does rel_string replace backslashes?"
/// — or else why it is written as it is. `None` outside a function.
fn question_about(app: &App) -> Option<String> {
    let crate::explain::Node::Function { name, .. } = app.cursor_target()? else {
        return None;
    };
    let v = app.active_viewer()?;
    let line1 = v.caret?.0 + 1;
    let edit = v
        .symbols
        .iter()
        .filter(|s| s.name == name && s.line <= line1 && line1 <= s.end_line)
        .min_by_key(|s| s.end_line.saturating_sub(s.line))
        .and_then(|sym| {
            v.source
                .lines()
                .skip(sym.line.saturating_sub(1))
                .take(sym.end_line + 1 - sym.line)
                .find_map(string_edit)
        });
    Some(match edit {
        Some((verb, what)) => format!("“Why does {name} {verb} {what}?”"),
        None => format!("“Why is {name} written this way?”"),
    })
}

/// The first edit of a string on one character `line` makes, as what is
/// done and what to: `.replace('\\', "/")` is ("replace", "backslashes").
fn string_edit(line: &str) -> Option<(&'static str, &'static str)> {
    const EDITS: [(&str, &str); 6] = [
        (".replace(", "replace"),
        (".replacen(", "replace"),
        (".split(", "split on"),
        (".trim_matches(", "trim"),
        (".trim_start_matches(", "trim"),
        (".trim_end_matches(", "trim"),
    ];
    EDITS.iter().find_map(|(call, verb)| {
        let at = line.find(call)?;
        let arg = line[at + call.len()..].split([',', ')']).next()?.trim();
        character_name(arg).map(|what| (*verb, what))
    })
}

/// A literal of one character — `'\\'`, `"/"` — by the name of what it is,
/// in the plural.
fn character_name(literal: &str) -> Option<&'static str> {
    let unquote = |q: char| literal.strip_prefix(q).and_then(|s| s.strip_suffix(q));
    let inner = unquote('\'').or_else(|| unquote('"'))?;
    Some(match inner {
        "\\\\" => "backslashes",
        "/" => "slashes",
        " " => "spaces",
        "\\t" => "tabs",
        "\\n" => "newlines",
        "." => "dots",
        ":" => "colons",
        ";" => "semicolons",
        "-" => "dashes",
        "_" => "underscores",
        _ => return None,
    })
}

/// How loud the microphone hears, as bars in the accent: each of `heights`
/// grown with the `level` (0–1), and, with `fade`, the ends quieter than
/// the middle.
pub(crate) fn level_bars<'a>(heights: &[f32], level: f32, fade: bool) -> Element<'a, Message> {
    // The panel's meter has wide bars; the field's, thin ones.
    let bar = if fade { 3.0 } else { 2.0 };
    let n = heights.len().max(2) as f32;
    let tallest = heights.iter().copied().fold(0.0_f32, f32::max);
    let gain = 0.35 + 0.65 * level.clamp(0.0, 1.0);
    let bars = heights.iter().enumerate().map(|(i, &h)| {
        let alpha = if fade {
            0.5 + 0.5 * (i as f32 / (n - 1.0) * std::f32::consts::PI).sin()
        } else {
            1.0
        };
        container(space())
            .width(Length::Fixed(bar))
            .height(Length::Fixed((h * gain).max(2.0)))
            .style(move |_: &iced::Theme| container::Style {
                background: Some(theme::with_alpha(theme::accent(), alpha).into()),
                border: iced::border::rounded(bar / 2.0),
                ..container::Style::default()
            })
            .into()
    });
    container(Row::with_children(bars).spacing(bar).align_y(iced::Center))
        .height(Length::Fixed(tallest))
        .align_y(iced::Center)
        .into()
}

/// A small borderless control of the card's header.
fn header_button<'a>(g: Glyph, tip: &'a str, msg: Message) -> Element<'a, Message> {
    super::tip_at(
        button(container(glyph::icon(g, theme::fg_muted(), 15.0)).center(Fill))
            .style(theme::toolbar_button)
            .width(Length::Fixed(HEADER_BUTTON))
            .height(Length::Fixed(HEADER_BUTTON))
            .padding(0)
            .on_press(msg),
        tip,
        None,
        tooltip::Position::Bottom,
    )
}

/// Height of the card's header row.
const ASK_HEADER_H: f32 = 44.0;
/// Side of the header's buttons, as the design's.
const HEADER_BUTTON: f32 = 28.0;
/// Padding along the header's row: the design's 16 before what it says,
/// 8 after its buttons.
const ASK_HEADER_PAD: Padding = Padding {
    top: 0.0,
    right: 8.0,
    bottom: 0.0,
    left: 16.0,
};

/// Whether the card fits its height to its conversation, as the card under
/// the chat field does; the large window keeps its height. A tool call's
/// approval is given up to half the card, the conversation over it what is
/// left, so the input stays in view under them however long either runs.
fn card_fits(app: &App) -> bool {
    // With its menu of sessions open, the card is as tall as it may be, so
    // the menu is inside it.
    ask_is_card(app) && !app.ask_sessions_menu
}

/// The card: its header (what it is, what is being read, size, close),
/// what voice is doing, and the conversation.
fn ask_card(app: &App, place: AskPlace) -> Element<'_, Message> {
    let large = app.ask_view == AskView::Expanded;
    let fit = card_fits(app);
    // The session's name, which opens the menu of sessions.
    let menu_open = app.ask_sessions_menu;
    let title = button(
        row![
            container(
                text(app.ask_session_title())
                    .size(ts::BASE)
                    .font(crate::fonts::UI_SEMIBOLD)
                    .color(theme::fg())
                    .wrapping(Wrapping::None),
            )
            .max_width(if large { 420.0 } else { 200.0 })
            .clip(true),
            glyph::icon(Glyph::ChevronDown, theme::dim(), 12.0),
        ]
        .spacing(5)
        .align_y(iced::Center),
    )
    .style(move |t: &iced::Theme, status| {
        if menu_open {
            theme::list_row(true)(t, status)
        } else {
            theme::toolbar_button(t, status)
        }
    })
    .padding([4, 6])
    .on_press(Message::Ask(AskMsg::SessionsMenu(!menu_open)));
    let mut header = row![glyph::icon(Glyph::Message, theme::accent(), 15.0), title]
        .spacing(6)
        .align_y(iced::Center);
    // What the reader is on, which is what a question is asked against —
    // said in the large window, where there is room for it.
    if let Some(v) = app.active_viewer().filter(|_| large) {
        let name = v.rel.rsplit('/').next().unwrap_or(&v.rel).to_string();
        let on = match &app.proj.explain.view {
            Some(crate::explain::Node::Function { file, name: f, .. }) if *file == v.abs => {
                format!("Reading {name} · {f}")
            }
            _ => format!("Reading {name}"),
        };
        header = header.push(
            container(ts::set_in(
                text(on).color(theme::fg_muted()).wrapping(Wrapping::None),
                ts::INTER.normal(ts::SMALL),
                ts::INTER,
                ts::SMALL,
            ))
            .padding([2, 7])
            .style(theme::chip),
        );
    }
    header = header.push(space().width(Fill));
    // A new session; hidden to the pill, made large or small again, put away.
    header = header
        .push(header_button(
            Glyph::Plus,
            "New session",
            Message::Ask(AskMsg::NewSession),
        ))
        .push(header_button(
            Glyph::ChevronDown,
            "Hide (Esc)",
            Message::Ask(AskMsg::Hide),
        ))
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
            "Put away",
            Message::Ask(AskMsg::Close),
        ));
    let card: Element<'_, Message> = column![
        container(header)
            .height(Length::Fixed(ASK_HEADER_H))
            .align_y(iced::Center)
            .padding(Padding {
                left: 12.0,
                ..ASK_HEADER_PAD
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
    .into();
    if !menu_open {
        return card;
    }
    stack![
        card,
        container(sessions_menu(app)).padding(Padding {
            top: ASK_HEADER_H - 4.0,
            left: 10.0,
            ..Padding::ZERO
        }),
    ]
    .into()
}

/// Widest the menu of sessions grows, and tallest before it scrolls.
const SESSIONS_MENU_W: f32 = 320.0;
const SESSIONS_MENU_MAX_H: f32 = 340.0;

/// The menu of Ask's sessions, under the card's title: a new session, then
/// this project's sessions, newest first — when each was last asked in and
/// how many questions it holds; the one on screen marked, the others each a
/// click from going back to, or from being forgotten.
fn sessions_menu(app: &App) -> Element<'_, Message> {
    use crate::app::ask_sessions::{now, when};
    let now = now();
    let project = app
        .proj
        .project
        .as_ref()
        .and_then(|p| p.root.file_name())
        .map_or_else(
            || "this project".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    let new = button(
        row![
            glyph::icon(Glyph::Plus, theme::accent(), 14.0),
            text("New session").size(ts::BASE).color(theme::fg()),
        ]
        .spacing(9)
        .align_y(iced::Center),
    )
    .style(theme::menu_row)
    .padding([7, 9])
    .width(Fill)
    .on_press(Message::Ask(AskMsg::NewSession));
    let mut list = Column::new().spacing(1);
    for session in &app.proj.ask_sessions {
        let current = app.proj.ask_session == Some(session.id);
        let questions = session.turns.len();
        let meta = format!(
            "{} · {questions} {}",
            when(session.updated, now),
            if questions == 1 {
                "question"
            } else {
                "questions"
            }
        );
        let dot =
            container(space().width(6).height(6)).style(move |_: &iced::Theme| container::Style {
                background: current.then(|| theme::accent().into()),
                border: iced::border::rounded(3.0),
                ..container::Style::default()
            });
        let pick = button(
            row![
                dot,
                column![
                    text(session.title.clone())
                        .size(ts::BASE)
                        .color(theme::fg())
                        .wrapping(Wrapping::None),
                    text(meta).size(ts::SMALL).color(theme::dim()),
                ]
                .spacing(1)
                .width(Fill),
            ]
            .spacing(9)
            .align_y(iced::Center),
        )
        .style(move |t: &iced::Theme, status| {
            if current {
                theme::list_row(true)(t, status)
            } else {
                theme::menu_row(t, status)
            }
        })
        .padding([6, 9])
        .width(Fill)
        .on_press(Message::Ask(AskMsg::SwitchSession(session.id)));
        let mut line = row![pick].spacing(2).align_y(iced::Center);
        if !current {
            line = line.push(tip_at(
                button(container(glyph::icon(Glyph::Close, theme::dim(), 12.0)).center(Fill))
                    .style(theme::toolbar_button)
                    .width(Length::Fixed(24.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .on_press(Message::Ask(AskMsg::DeleteSession(session.id))),
                "Forget this session",
                None,
                tooltip::Position::Right,
            ));
        }
        list = list.push(line);
    }
    let body: Element<'_, Message> = if app.proj.ask_sessions.is_empty() {
        container(
            text("Nothing asked yet: a session is kept once Clew has answered in it.")
                .size(ts::SMALL)
                .color(theme::dim()),
        )
        .padding([4, 9])
        .into()
    } else {
        scrollable(list)
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .into()
    };
    container(opaque(
        column![
            new,
            container(hairline()).padding([3, 2]),
            container(
                text(format!("Sessions in {project}"))
                    .size(ts::SMALL)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::dim()),
            )
            .padding([4, 9]),
            body,
        ]
        .spacing(1),
    ))
    .width(SESSIONS_MENU_W)
    .max_height(SESSIONS_MENU_MAX_H)
    .padding(5)
    .style(theme::modal_panel_rounded(11.0))
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

#[cfg(test)]
mod question_tests {
    use super::{character_name, string_edit};

    /// A string edit on one character reads as what it does and to what;
    /// anything else is no such edit.
    #[test]
    fn a_string_edit_reads_as_what_it_does() {
        assert_eq!(
            string_edit(r#"    return rel.replace('\\', "/");"#),
            Some(("replace", "backslashes"))
        );
        assert_eq!(
            string_edit(r#"let parts = path.split("/").collect();"#),
            Some(("split on", "slashes"))
        );
        assert_eq!(
            string_edit(r#"s.trim_matches(' ')"#),
            Some(("trim", "spaces"))
        );
        assert_eq!(string_edit(r#"s.replace("abc", "d")"#), None);
        assert_eq!(string_edit("let rel = rel.to_string_lossy();"), None);
        assert_eq!(character_name("'\\n'"), Some("newlines"));
        assert_eq!(character_name("x"), None);
    }
}
