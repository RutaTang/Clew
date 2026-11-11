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
/// Least height the card shrinks to in a short window.
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

/// Room under the last row of a page over the code (the glossary, the
/// statistics, the overview, a docs page, the weak spots), clear of the
/// pill floating at the window's foot: scrolled to its end, nothing of the
/// page is under the pill.
pub(crate) fn pill_foot<'a, M: 'a>() -> Element<'a, M> {
    space().height(PILL_H + ASK_MARGIN).into()
}

/// How far up from the foot of `pane`'s code view the pill floats over its
/// lines: while Ask is hidden to it and nothing else is under it there — no
/// margin (which is always wider than the pill), no debugger's panel, no
/// page over the panes — in the pane at the window's right. Else nothing.
/// The caret is kept above it ([`App::code_seen_h`](crate::App)): moved
/// down a line at a time in a narrow window, it went on under the pill.
pub(crate) fn pill_over_code(app: &App, pane: usize) -> f32 {
    let rightmost = usize::from(app.proj.split);
    let over = app.ask_view == AskView::Hidden
        && pane == rightmost
        && !right_panel_shown(app)
        && !app.show_bottom
        && pane_cover(app).is_none();
    if over { PILL_H + ASK_MARGIN } else { 0.0 }
}

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
/// the update banner), over the status bar. The debugger's panel takes none
/// of it: Ask lives in the window's corner, over the panel's corner when it
/// shows, rather than moving up with it to float over the code.
fn band(app: &App) -> (f32, f32) {
    let top = TOOLBAR_H + update_banner_height(app) + TOP_GAP;
    let bottom = app.window_height - STATUSBAR_H - ASK_MARGIN;
    (top, bottom.max(top))
}

/// The corner the card and the pill sit in, as how far their right edge is
/// from the window's (left of the tool strip) and their bottom edge from its
/// bottom (over the status bar), whatever panels are open.
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
        AskView::Hidden => None,
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

/// The pill Ask hides and closes to: what voice is doing while it hears,
/// waits for a yes, speaks, or has just done what was said — else Ask's own
/// face, a click from opening.
fn ask_pill(app: &App) -> Option<Element<'_, Message>> {
    let voice = pill_voice(app);
    let listening = app.voice.mic_on;
    let mic = tip_at(
        button(
            container(glyph::icon(
                Glyph::Mic,
                if listening {
                    crate::theme::active().on_accent
                } else {
                    talk_tint(app, theme::fg_muted())
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
        .on_press(super::heard::said(
            if listening {
                tr!("Stop listening")
            } else {
                tr!("Talk")
            },
            Message::Voice(VoiceMsg::Toggle),
        )),
        if listening {
            tr!("Stop listening")
        } else {
            talk_tip(app)
        },
        Some(app.keymap.chord(crate::keymap::Action::Voice).caps()),
        tooltip::Position::Top,
    );
    // Clew speaking: the button lit, as while it listens, to stop it.
    let mic = match voice_lit(app, 26.0, 14.0) {
        Some(lit) if app.voice.speech.is_speaking() => lit,
        _ => mic,
    };
    let face: Element<'_, Message> = match voice {
        Some(said) => said,
        None => {
            // Ask's face, and nothing more: a count of its answers beside it
            // said nothing the card does not, and moved the pill's edge as
            // it grew.
            // An answer that came while the card was put away: a dot on
            // the mark, until the card is opened — nothing beside it, so
            // the pill keeps its size.
            let mark: Element<'_, Message> = if app.proj.ask_unread {
                stack![
                    glyph::icon(Glyph::Message, theme::accent(), 15.0),
                    container(
                        container(space().width(UNREAD_DOT).height(UNREAD_DOT)).style(
                            |_: &iced::Theme| container::Style {
                                background: Some(theme::accent().into()),
                                // Ringed in the pill's surface, apart
                                // from the mark it sits on.
                                border: iced::Border {
                                    color: crate::theme::active().elevated,
                                    width: 1.5,
                                    radius: (UNREAD_DOT / 2.0).into(),
                                },
                                ..container::Style::default()
                            }
                        )
                    )
                    .width(Length::Fixed(15.0))
                    .height(Length::Fixed(15.0))
                    .align_x(iced::Right)
                    .align_y(iced::Top),
                ]
                .into()
            } else {
                glyph::icon(Glyph::Message, theme::accent(), 15.0)
            };
            let face = row![
                mark,
                text(tr!("Ask"))
                    .size(ts::BASE)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::fg()),
            ]
            .spacing(9)
            .align_y(iced::Center);
            tip_at(
                button(face)
                    .style(theme::pill_face)
                    .padding([4, 8])
                    .on_press(super::heard::said(
                        tr!("Open Ask"),
                        Message::Ask(AskMsg::Open),
                    )),
                tr!("Ask"),
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

/// What voice says on the pill: a command waiting for a yes, Clew
/// speaking, a question being answered — or, for a moment, what was just
/// heard and what came of it, as the microphone listens on — else that it
/// listens (how loud). `None` when it has nothing to say.
fn pill_voice(app: &App) -> Option<Element<'_, Message>> {
    let inset = |bar: Element<'static, Message>| -> Element<'static, Message> {
        container(bar)
            .padding(Padding {
                left: 8.0,
                ..Padding::ZERO
            })
            .into()
    };
    // A command waiting for a yes has buttons of its own; anything else the
    // pill says opens the conversation, as its face does — Clew speaking an
    // answer most of all, which is in it.
    if let Some(bar) = voice_confirm(app) {
        return Some(inset(bar));
    }
    if let Some(line) = speaking_line(app) {
        return Some(opens_ask(line));
    }
    // A question answered with the card put away (one said aloud is):
    // the pill says so, a click opening the card on it.
    if !app.ask_view.is_open() && app.ask_stream_active() {
        let line = phase_line(
            glyph::icon(Glyph::Message, theme::accent(), 14.0),
            phase_words()[2],
            super::meter::ripple(&PILL_BARS),
        );
        return Some(
            button(line)
                .style(theme::pill_face)
                .padding(Padding {
                    top: 4.0,
                    right: 8.0,
                    bottom: 4.0,
                    left: 8.0,
                })
                .on_press(Message::Ask(AskMsg::Open))
                .into(),
        );
    }
    pill_heard(app)
        .or_else(|| listening_line(app))
        .map(opens_ask)
}

/// `face` on the pill, pressed to open the conversation.
fn opens_ask(face: Element<'_, Message>) -> Element<'_, Message> {
    button(face)
        .style(theme::pill_face)
        .padding(Padding {
            top: 4.0,
            right: 8.0,
            bottom: 4.0,
            left: 8.0,
        })
        .on_press(super::heard::said(
            tr!("Open Ask"),
            Message::Ask(AskMsg::Open),
        ))
        .into()
}

/// For a moment, what was just heard and what came of it.
fn pill_heard(app: &App) -> Option<Element<'_, Message>> {
    if !app.voice.heard_fresh() {
        return None;
    }
    let heard = app.voice.heard.as_ref()?;
    let (mark, mark_color, said, said_color) = match &heard.outcome {
        HeardOutcome::Ran(title) => (
            Glyph::Check,
            theme::success(),
            format!("→ {}", crate::i18n::lookup(title)),
            theme::dim(),
        ),
        HeardOutcome::Confirm(title) => (
            Glyph::Check,
            theme::dim(),
            tr!("{command}?", command = crate::i18n::lookup(title)),
            theme::dim(),
        ),
        // A question sent is being answered, which the pill says above;
        // one kept for the reader to send waits in the card, open on it.
        HeardOutcome::Asked => return None,
        HeardOutcome::Failed(e) => (
            Glyph::Info,
            theme::warn(),
            crate::i18n::message(e),
            theme::warn(),
        ),
    };
    let mut line = row![glyph::icon(mark, mark_color, 14.0)]
        .spacing(8)
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

/// What can be said, and what comes of it: shown while voice listens, in
/// the reader's language. A command said is matched by its English title
/// or by the title the palette shows (`app::voice::said_as_shown`), so each
/// language's examples name a command as its palette does; a question goes
/// to Ask, which takes it in any language.
pub(crate) const TRY_SAYING: [(&str, &str); 5] = [
    (tr_noop!("“Open the call graph”"), tr_noop!("Clew runs it")),
    (tr_noop!("“Hide the minimap”"), tr_noop!("Clew runs it")),
    (
        tr_noop!("“Take me to where the config is parsed”"),
        tr_noop!("Clew finds it and takes you there"),
    ),
    (tr_noop!("“Why is this function here?”"), CLEW_ANSWERS),
    (
        tr_noop!("“Start debugging”"),
        tr_noop!("Clew asks you first"),
    ),
];

/// What a question said aloud comes to: the row of [`TRY_SAYING`] the
/// question about the caret's function takes the place of.
const CLEW_ANSWERS: &str = tr_noop!("Clew answers");

/// What can be said while voice listens, as the empty card shows it: the
/// question among them made about the function the caret is in.
pub(crate) fn try_saying(app: &App) -> Vec<(String, &'static str)> {
    let about = question_about(app);
    TRY_SAYING
        .iter()
        .map(|&(say, does)| match (&about, does) {
            (Some(about), CLEW_ANSWERS) => (about.clone(), crate::i18n::lookup(does)),
            _ => (
                crate::i18n::lookup(say).to_string(),
                crate::i18n::lookup(does),
            ),
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
    // A sentence of its own for each edit, the characters named in the
    // reader's language.
    Some(match edit {
        Some((verb, what)) => {
            let what = crate::i18n::lookup(what);
            match verb {
                "split on" => tr!(
                    "“Why does {name} split on {what}?”",
                    name = name,
                    what = what
                ),
                "trim" => tr!("“Why does {name} trim {what}?”", name = name, what = what),
                _ => tr!(
                    "“Why does {name} replace {what}?”",
                    name = name,
                    what = what
                ),
            }
        }
        None => tr!("“Why is {name} written this way?”", name = name),
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
    // Kept in English: `question_about` says it in the reader's language.
    Some(match inner {
        "\\\\" => tr_noop!("backslashes"),
        "/" => tr_noop!("slashes"),
        " " => tr_noop!("spaces"),
        "\\t" => tr_noop!("tabs"),
        "\\n" => tr_noop!("newlines"),
        "." => tr_noop!("dots"),
        ":" => tr_noop!("colons"),
        ";" => tr_noop!("semicolons"),
        "-" => tr_noop!("dashes"),
        "_" => tr_noop!("underscores"),
        _ => return None,
    })
}

/// The pill's level meter: its bars' heights at the loudest.
const PILL_BARS: [f32; 7] = [5.0, 9.0, 13.0, 8.0, 11.0, 6.0, 9.0];

/// A small borderless control of the card's header.
fn header_button<'a>(g: Glyph, tip: &'a str, msg: Message) -> Element<'a, Message> {
    header_button_keyed(g, tip, None, msg)
}

/// A [`header_button`] whose tooltip also shows its key (`keys`), in the
/// keycap every control's tooltip shows its own in.
fn header_button_keyed<'a>(
    g: Glyph,
    tip: &'a str,
    keys: Option<&str>,
    msg: Message,
) -> Element<'a, Message> {
    super::tip_at(
        button(container(glyph::icon(g, theme::fg_muted(), 15.0)).center(Fill))
            .style(theme::toolbar_button)
            .width(Length::Fixed(HEADER_BUTTON))
            .height(Length::Fixed(HEADER_BUTTON))
            .padding(0)
            .on_press(super::heard::said(tip, msg)),
        tip,
        keys.map(str::to_string),
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
    // While an answer comes in, the card is as tall as it may be: fitted,
    // it grew up the window a line at a time, its header moving with every
    // line said. (Its menu of sessions drops over the window, and leaves
    // the card as it was.)
    ask_is_card(app) && !app.ask_stream_active()
}

/// The card: its header (what it is, what is being read, size, close),
/// what voice is doing, and the conversation.
fn ask_card(app: &App, place: AskPlace) -> Element<'_, Message> {
    let large = app.ask_view == AskView::Expanded;
    let fit = card_fits(app);
    // What the reader is on, which is what a question is asked against —
    // said in the large window, where there is room for it.
    let reading = app.active_viewer().filter(|_| large).map(|v| {
        let name = v.rel.rsplit('/').next().unwrap_or(&v.rel).to_string();
        match &app.proj.explain.view {
            Some(crate::explain::Node::Function { file, name: f, .. }) if *file == v.abs => {
                tr!("Viewing {function} in {file}", file = name, function = f)
            }
            _ => tr!("Viewing {file}", file = name),
        }
    });
    // The session's name, which opens the menu of sessions: cut to the room
    // the header leaves it — less what is read, its padding and the gap
    // before it, when that is said — as wide as it is drawn.
    let mut room = title_room(place.width);
    if let Some(on) = &reading {
        room -= super::sheet::text_width(on, ts::SMALL, crate::fonts::UI) + 2.0 * 7.0 + 6.0;
    }
    if large {
        room = room.min(TITLE_MAX_W);
    }
    let menu_open = app.ask_sessions_menu;
    let title = button(
        row![
            text(cut_to_width(
                &app.ask_session_title(),
                room,
                ts::BASE,
                crate::fonts::UI_SEMIBOLD
            ))
            .size(ts::BASE)
            .font(crate::fonts::UI_SEMIBOLD)
            .color(theme::fg())
            .wrapping(Wrapping::None),
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
    .on_press(super::heard::said(
        tr!("Sessions"),
        Message::Ask(AskMsg::SessionsMenu(!menu_open)),
    ));
    // The sessions drop from the name, over the window: held inside the
    // card, the card grew to its full height to hold them, and its header —
    // and the menu — jumped 290 px up from where the pointer was.
    let title = super::dropdown::dropdown(
        title,
        menu_open.then(|| sessions_menu(app)),
        iced::Vector::new(-6.0, 6.0),
    );
    let mut header = row![glyph::icon(Glyph::Message, theme::accent(), 15.0), title]
        .spacing(6)
        .align_y(iced::Center);
    if let Some(on) = reading {
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
    // A new session; made large or small again; put away to the pill. No
    // Hide beside Close: both put it away to the pill (Close stops Clew
    // speaking too), and Hide's chevron was the session menu's again. Esc
    // is named on the one it does: Shrink when large, Close when a card.
    header = header
        .push(header_button(
            Glyph::Plus,
            tr!("New session"),
            Message::Ask(AskMsg::NewSession),
        ))
        .push(if large {
            header_button_keyed(
                Glyph::Shrink,
                tr!("Shrink"),
                Some("Esc"),
                Message::Ask(AskMsg::Shrink),
            )
        } else {
            header_button(Glyph::Expand, tr!("Expand"), Message::Ask(AskMsg::Expand))
        })
        .push(header_button_keyed(
            Glyph::Close,
            tr!("Close Ask"),
            (!large).then_some("Esc"),
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
    card
}

/// Widest the session's name is drawn in the large window, where the
/// header has more room than a name needs.
const TITLE_MAX_W: f32 = 420.0;

/// Room the card's header leaves the session's name in a card `width`
/// wide: less the card's border and the header's padding, its icon and four
/// buttons, the six gaps between the seven, and the name's own padding,
/// gap and chevron. A row is laid out in order, so a name wider than this
/// left the buttons after it no room: they were drawn narrower, then not
/// at all.
fn title_room(width: f32) -> f32 {
    let border_and_padding = 2.0 + 12.0 + ASK_HEADER_PAD.right;
    let icon_and_buttons = 15.0 + 4.0 * HEADER_BUTTON;
    let gaps = 6.0 * 6.0;
    let name_around = 2.0 * 6.0 + 5.0 + 12.0;
    (width - border_and_padding - icon_and_buttons - gaps - name_around).max(0.0)
}

/// `s`, cut short with an ellipsis to fit `width` when set at `size` in
/// `font`. Measured, not counted: a name in Chinese, Japanese or Korean,
/// about an em a character, is cut as soon as one in Latin letters is.
fn cut_to_width(s: &str, width: f32, size: f32, font: iced::Font) -> String {
    // Asked again with every view, for each name in the menu: kept.
    type Key = (String, u32, u32, iced::Font);
    thread_local! {
        static CUT: std::cell::RefCell<std::collections::HashMap<Key, String>> =
            Default::default();
    }
    let key = (s.to_string(), width.to_bits(), size.to_bits(), font);
    if let Some(cut) = CUT.with(|held| held.borrow().get(&key).cloned()) {
        return cut;
    }
    let fits = |t: &str| super::sheet::text_width(t, size, font) <= width;
    let cut = if fits(s) {
        s.to_string()
    } else {
        // The most characters that fit with the ellipsis, found by halving.
        let chars: Vec<char> = s.chars().collect();
        let with_ellipsis = |n: usize| {
            let kept: String = chars[..n].iter().collect();
            format!("{}…", kept.trim_end())
        };
        let (mut least, mut most) = (0, chars.len());
        while least < most {
            let n = (least + most).div_ceil(2);
            if fits(&with_ellipsis(n)) {
                least = n;
            } else {
                most = n - 1;
            }
        }
        with_ellipsis(least)
    };
    CUT.with(|held| {
        let mut held = held.borrow_mut();
        if held.len() > 256 {
            held.clear();
        }
        held.insert(key, cut.clone());
    });
    cut
}

/// Widest the menu of sessions grows, and tallest before it scrolls.
const SESSIONS_MENU_W: f32 = 320.0;
const SESSIONS_MENU_MAX_H: f32 = 340.0;
/// Room a session's name has in the menu: its width less its padding, a
/// row's padding, dot and the gap after it, and the × that forgets it.
pub(crate) const SESSIONS_MENU_NAME_W: f32 =
    SESSIONS_MENU_W - 2.0 * 5.0 - 2.0 * 9.0 - (6.0 + 9.0) - (2.0 + 24.0);

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
        .map(|n| n.to_string_lossy().into_owned());
    // The search over the sessions' names, questions and answers.
    let search = container(
        row![
            glyph::icon(Glyph::Search, theme::fg_muted(), 14.0),
            text_input(tr!("Search sessions"), &app.ask_sessions_query)
                .id(crate::ui::ask_sessions_search_id())
                .on_input(|q| Message::Ask(AskMsg::SessionsQuery(q)))
                .on_submit(Message::Ask(AskMsg::OpenFirstFound))
                .size(ts::BASE)
                .padding([5, 0])
                .style(theme::bare_input),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .padding([0, 9])
    .style(theme::field(!app.ask_sessions_query.is_empty()));
    let found = app.found_ask_sessions();
    let mut list = Column::new().spacing(1);
    super::heard::list();
    for (session, how) in &found {
        let current = app.proj.ask_session == Some(session.id);
        let questions = session.turns.len();
        let meta = format!(
            "{}, {}",
            when(session.updated, now),
            trn!("1 question", "{n} questions", questions)
        );
        let dot =
            container(space().width(6).height(6)).style(move |_: &iced::Theme| container::Style {
                background: current.then(|| theme::accent().into()),
                border: iced::border::rounded(3.0),
                ..container::Style::default()
            });
        let pick = button(
            row![dot, {
                let mut lines = column![
                    text(cut_to_width(
                        &crate::app::ask_sessions::shown_title(&session.title),
                        SESSIONS_MENU_NAME_W,
                        ts::BASE,
                        crate::fonts::UI
                    ))
                    .size(ts::BASE)
                    .color(theme::fg())
                    .wrapping(Wrapping::None),
                ]
                .spacing(1)
                .width(Fill);
                // Found in what was asked or answered: where.
                if let Some(passage) = &how.passage {
                    lines = lines.push(
                        text(passage.clone())
                            .size(ts::SMALL)
                            .color(theme::fg_muted())
                            .wrapping(Wrapping::WordOrGlyph),
                    );
                }
                lines.push(text(meta).size(ts::SMALL).color(theme::dim()))
            },]
            .spacing(9)
            .align_y(iced::Center),
        )
        // The one open is marked by its dot alone: filled as well, it was a
        // second lit row beside the one under the pointer.
        .style(theme::menu_row)
        .padding([6, 9])
        .width(Fill)
        .on_press(super::heard::row(
            || crate::app::ask_sessions::shown_title(&session.title).to_string(),
            Message::Ask(AskMsg::SwitchSession(session.id)),
        ));
        let mut line = row![pick].spacing(2).align_y(iced::Center);
        if !current {
            line = line.push(tip_at(
                button(container(glyph::icon(Glyph::Close, theme::dim(), 12.0)).center(Fill))
                    .style(theme::toolbar_button)
                    .width(Length::Fixed(24.0))
                    .height(Length::Fixed(24.0))
                    .padding(0)
                    .on_press(super::heard::said_to(
                        tr!("Forget"),
                        &crate::app::ask_sessions::shown_title(&session.title),
                        Message::Ask(AskMsg::DeleteSession(session.id)),
                    )),
                tr!("Forget this session"),
                None,
                tooltip::Position::Right,
            ));
        }
        list = list.push(line);
    }
    let note = |s: String| -> Element<'_, Message> {
        container(text(s).size(ts::SMALL).color(theme::dim()))
            .padding([4, 9])
            .into()
    };
    let body: Element<'_, Message> = if app.proj.ask_sessions.is_empty() {
        note(tr!("No sessions yet. A session is saved once Clew has answered in it.").into())
    } else if found.is_empty() {
        note(tr!(
            "No sessions match “{query}”.",
            query = app.ask_sessions_query.trim()
        ))
    } else {
        scrollable(list)
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .into()
    };
    let heading = if app.ask_sessions_query.trim().is_empty() {
        match &project {
            Some(project) => tr!("Sessions in {project}", project = project),
            None => tr!("Sessions in this project").to_string(),
        }
    } else {
        trn!("1 session found", "{n} sessions found", found.len())
    };
    // Searching is worth it once there is more than one to look through —
    // and the field stays while what it holds still filters the menu, so
    // it can be cleared. A new session is the header's +, beside the menu:
    // not offered a second time in it.
    let searching = app.proj.ask_sessions.len() > 1 || !app.ask_sessions_query.is_empty();
    let mut menu = Column::new().spacing(1);
    if searching {
        menu = menu
            .push(search)
            .push(container(hairline()).padding([3, 2]));
    }
    container(opaque(
        menu.push(
            container(
                text(heading)
                    .size(ts::SMALL)
                    .font(crate::fonts::UI_MEDIUM)
                    .color(theme::dim()),
            )
            .padding([4, 9]),
        )
        .push(body),
    ))
    .width(SESSIONS_MENU_W)
    .max_height(SESSIONS_MENU_MAX_H)
    .padding(5)
    .style(theme::modal_panel_rounded(11.0))
    .into()
}

/// A command heard that waits for a yes — said, or pressed (Run, Cancel):
/// on the pill, and over the card's composer. The microphone listens on
/// for the yes meanwhile.
pub(crate) fn voice_confirm(app: &App) -> Option<Element<'static, Message>> {
    let small = |label: &'static str, msg: Message, primary: bool| {
        button(text(label).size(ts::SMALL))
            .style(if primary {
                theme::primary_button
            } else {
                theme::toolbar_button
            })
            .padding([2, 8])
            .on_press(super::heard::said(label, msg))
    };
    let waiting = app.voice.confirm.as_ref()?;
    use crate::app::voice::Waiting;
    // A command is run; a button pressed by a word is pressed.
    let (asked, go) = match waiting {
        Waiting::Command(_) => (
            tr!("Run “{command}”?", command = waiting.said()),
            tr!("Run"),
        ),
        Waiting::Press { .. } => (tr!("{command}?", command = waiting.said()), tr!("Yes")),
    };
    Some(
        row![
            text(asked)
                .size(ts::BASE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
            small(go, Message::Voice(VoiceMsg::Confirm), true),
            small(tr!("Cancel"), Message::Voice(VoiceMsg::Cancel), false),
        ]
        .spacing(6)
        .align_y(iced::Center)
        .into(),
    )
}

// The mark before the word (the red dot, the speaker) and the word itself
// each in a place as wide as the widest of them, so the meter after them
// stays where it is as one turns to the next — on the pill, its left edge
// and the mark jumped at every change.

/// The microphone open, as the pill says it: the red dot, how loud. The
/// round button beside it is the way to turn it off ([`voice_lit`]).
fn listening_line(app: &App) -> Option<Element<'static, Message>> {
    let VoicePhase::Listening { level, .. } = app.voice.phase else {
        return None;
    };
    Some(
        phase_line(
            container(space().width(REC_DOT).height(REC_DOT))
                .style(|_: &iced::Theme| container::Style {
                    background: Some(theme::recording().into()),
                    border: iced::border::rounded(REC_DOT / 2.0),
                    ..container::Style::default()
                })
                .into(),
            phase_words()[0],
            super::meter::level_meter(&PILL_BARS, level),
        )
        .into(),
    )
}

/// Clew speaking, as the pill says it: the speaker in the dot's place —
/// the bars rippling while what it says is still being made.
fn speaking_line(app: &App) -> Option<Element<'static, Message>> {
    if !app.voice.speech.is_speaking() {
        return None;
    }
    Some(
        phase_line(
            glyph::icon(Glyph::Speaker, theme::accent(), 14.0),
            phase_words()[1],
            speaking_meter(app),
        )
        .into(),
    )
}

fn speaking_meter(app: &App) -> Element<'static, Message> {
    if app.voice.speech.preparing() {
        super::meter::ripple(&PILL_BARS)
    } else {
        super::meter::level_meter(&PILL_BARS, app.voice.speech.level)
    }
}

/// How loud what the microphone hears is, or what Clew says: the card's
/// composer shows it beside the round button, the field left to type in.
pub(crate) fn voice_meter(app: &App) -> Option<Element<'static, Message>> {
    match app.voice.phase {
        _ if app.voice.speech.is_speaking() => Some(speaking_meter(app)),
        VoicePhase::Listening { level, .. } => Some(super::meter::level_meter(&PILL_BARS, level)),
        VoicePhase::Idle => None,
    }
}

/// The dot on the pill's mark for an answer not read yet.
const UNREAD_DOT: f32 = 7.0;

/// The place of the mark before what voice is doing: the speaker's width,
/// the widest of them.
const MARK_W: f32 = 14.0;

/// What the pill says is happening: listening, speaking, answering.
fn phase_words() -> [&'static str; 3] {
    [
        tr!("listening now\u{4}Listening"),
        tr!("speaking now\u{4}Speaking"),
        tr!("answering now\u{4}Answering"),
    ]
}

/// One of [`phase_words`] as a line: its mark, the word — in a place as
/// wide as the widest of them, so what follows stays where it is as one
/// turns to the next — and its meter.
fn phase_line(
    mark: Element<'static, Message>,
    word: &'static str,
    meter: Element<'static, Message>,
) -> iced::widget::Row<'static, Message> {
    let word = super::steady::slot(
        text(word)
            .size(ts::BASE)
            .color(theme::fg())
            .wrapping(Wrapping::None),
        word,
        &phase_words(),
        ts::BASE,
    );
    row![
        container(mark)
            .width(Length::Fixed(MARK_W))
            .align_x(iced::Center),
        word,
        meter,
    ]
    .spacing(8)
    .align_y(iced::Center)
}

/// The microphone at rest drawn in `tint` — in the accent while it waits
/// for "Hey, Clew": it is on, though nothing is being listened to.
pub(crate) fn talk_tint(app: &App, tint: iced::Color) -> iced::Color {
    if app.voice.wake.is_waiting() {
        theme::accent()
    } else {
        tint
    }
}

/// What the microphone at rest says it does: talk — or, while it waits,
/// that "Hey, Clew" does too.
pub(crate) fn talk_tip(app: &App) -> &'static str {
    if app.voice.wake.is_waiting() {
        tr!("Talk — or say “Hey, Clew”")
    } else {
        tr!("Talk")
    }
}

/// The voice key's round button, `side` across, lit in the accent while
/// the microphone is on — the microphone, which turns it off — or Clew
/// speaks — a stop. `None` otherwise: the microphone as it is at rest.
pub(crate) fn voice_lit(app: &App, side: f32, icon: f32) -> Option<Element<'_, Message>> {
    let (glyph, message, tip, lit) = if app.voice.speech.is_speaking() {
        (
            Glyph::Stop,
            VoiceMsg::StopSpeaking,
            tr!("Stop speaking"),
            true,
        )
    } else if app.voice.mic_on {
        (Glyph::Mic, VoiceMsg::Toggle, tr!("Stop listening"), true)
    } else {
        return None;
    };
    let chord = Some(app.keymap.chord(crate::keymap::Action::Voice).caps());
    let tint = if lit {
        crate::theme::active().on_accent
    } else {
        theme::fg()
    };
    let lit = button(container(glyph::icon(glyph, tint, icon)).center(Fill))
        .style(theme::round_button(lit))
        .padding(0)
        .width(Length::Fixed(side))
        .height(Length::Fixed(side))
        .on_press(super::heard::said(tip, Message::Voice(message)));

    Some(tip_at(lit, tip, chord, tooltip::Position::Top))
}

/// Voice on the card, under its header: what was last heard and what came
/// of it, as the microphone listens on. A command waiting for a yes is over
/// the composer ([`voice_confirm`]), and Clew speaking beside it
/// ([`voice_meter`]). `None` when there is nothing to say.
pub(crate) fn voice_strip(app: &App) -> Option<Element<'_, Message>> {
    if app.voice.confirm.is_some() || app.voice.speech.is_speaking() {
        return None;
    }
    let line: Element<'_, Message> = {
        {
            let heard = app.voice.heard.as_ref()?;
            let (said, color) = match &heard.outcome {
                HeardOutcome::Ran(title) => (
                    format!("→ {}", crate::i18n::lookup(title)),
                    theme::fg_muted(),
                ),
                HeardOutcome::Confirm(title) => (
                    format!(
                        "→ {}",
                        tr!("{command}?", command = crate::i18n::lookup(title))
                    ),
                    theme::fg_muted(),
                ),
                HeardOutcome::Asked => (format!("→ {}", tr!("asked")), theme::fg_muted()),
                HeardOutcome::Failed(e) => (crate::i18n::message(e), theme::warn()),
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
