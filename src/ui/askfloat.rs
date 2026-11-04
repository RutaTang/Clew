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
pub(crate) const ASK_CARD_W: f32 = 620.0;
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
const VOICE_PANEL_W: f32 = 560.0;

/// Where the open Ask window is drawn, in window coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AskPlace {
    pub width: f32,
    pub height: f32,
    /// Its left edge: under the chat field for the card, centred for the
    /// large window.
    pub left: f32,
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

/// The top of the card: over the chat field, a little above it, so the
/// conversation opens where it was asked — the card takes the field's
/// place while it is open.
fn card_top(app: &App) -> f32 {
    // From inside the field's 1 px border, as the design hangs it.
    (chat_field_rect(app).y + 1.0 - FIELD_GAP).max(0.0)
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

/// Whether the conversation is kept in the margin and the margin shows it
/// — not floating over the code.
pub(crate) fn ask_in_margin(app: &App) -> bool {
    app.ask_view == AskView::Margin && super::right_panel_shown(app)
}

/// Whether the conversation is a card under the chat field: as a card, or
/// kept in a margin that cannot show.
pub(crate) fn ask_is_card(app: &App) -> bool {
    app.ask_view == AskView::Compact || (app.ask_view == AskView::Margin && !ask_in_margin(app))
}

/// Where the Ask window is drawn now; `None` while it is put away, or kept
/// in the margin.
pub(crate) fn ask_place(app: &App) -> Option<AskPlace> {
    let (top, bottom) = band(app);
    match app.ask_view {
        AskView::Closed => None,
        AskView::Margin if ask_in_margin(app) => None,
        AskView::Compact | AskView::Margin => {
            let top = card_top(app);
            let height = ASK_CARD_H.min(bottom - top).max(ASK_CARD_MIN_H);
            let width = ASK_CARD_W
                .min(app.window_width - 2.0 * ASK_MARGIN)
                .max(200.0);
            // Always hanging from the chat field it opened from, as the
            // design has it — over the code read, the caret's line too.
            Some(AskPlace {
                width,
                height,
                left: under_field(app, width),
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
    } else {
        frame
            .align_y(iced::alignment::Vertical::Top)
            .padding(Padding {
                top: card_top(app),
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
        "Clew finds it and takes you there",
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
    // A question about what is being read, when the caret is in a function.
    let about = question_about(app);
    let mut tries = Column::new();
    for (say, does) in TRY_SAYING {
        let say = match (&about, does) {
            (Some(about), "Clew answers") => about.clone(),
            _ => say.to_string(),
        };
        tries = tries.push(
            row![
                text(say).size(ts::BASE).color(theme::fg()).width(Fill),
                container(text(does).size(ts::SMALL).color(theme::fg_muted()))
                    .padding([2, 8])
                    .style(theme::chip),
            ]
            .spacing(10)
            .height(Length::Fixed(TRY_ROW_H))
            .align_y(iced::Center)
            .padding([0, 10]),
        );
    }
    let model = app.voice.prefs.model().name;
    container(column![
        column![
            level_bars(&WAVE, level, true),
            text(title)
                .size(ts::SUBTITLE)
                .line_height(ts::NORMAL_LINE)
                .color(theme::fg_bright()),
            text(hint)
                .size(HINT_SIZE)
                .line_height(ts::NORMAL_LINE)
                .color(theme::fg_muted()),
        ]
        .spacing(10)
        .align_x(iced::Center)
        .width(Fill)
        .padding(Padding {
            top: 24.0,
            right: 20.0,
            bottom: 18.0,
            left: 20.0,
        }),
        hairline(),
        container(column![
            container(
                text("Try saying")
                    .size(ts::SMALL)
                    .line_height(ts::NORMAL_LINE)
                    .color(theme::dim()),
            )
            .padding(Padding {
                top: 8.0,
                right: 10.0,
                bottom: 4.0,
                left: 10.0,
            }),
            tries,
        ])
        .padding(Padding {
            top: 4.0,
            right: 6.0,
            bottom: 8.0,
            left: 6.0,
        }),
        hairline(),
        row![
            glyph::icon(Glyph::Lock, theme::fg_muted(), 13.0),
            text(format!(
                "Transcribed on this computer by Whisper {model}. Nothing you say \
                     leaves your computer."
            ))
            .size(ts::SMALL)
            .line_height(ts::NORMAL_LINE)
            .color(theme::fg_muted()),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .padding([10, 16]),
    ])
    .style(theme::floating_card)
    .into()
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

/// Height of a row of what can be said.
const TRY_ROW_H: f32 = 34.0;
/// Size of the line under the voice panel's title.
const HINT_SIZE: f32 = 12.0;

/// The heights of the voice panel's level meter, quiet at its ends.
const WAVE: [f32; 28] = [
    5.0, 8.0, 12.0, 9.0, 16.0, 22.0, 14.0, 26.0, 18.0, 30.0, 24.0, 16.0, 28.0, 34.0, 26.0, 20.0,
    30.0, 22.0, 14.0, 24.0, 18.0, 12.0, 20.0, 14.0, 9.0, 12.0, 7.0, 5.0,
];

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

/// Height of the card's header row.
const ASK_HEADER_H: f32 = 44.0;

/// Whether the card fits its height to its conversation, as the card under
/// the chat field does; the large window keeps its height. A tool call's
/// approval is given up to half the card, the conversation over it what is
/// left, so the input stays in view under them however long either runs.
fn card_fits(app: &App) -> bool {
    ask_is_card(app)
}

/// The card: its header (what it is, what is being read, size, close),
/// what voice is doing, and the conversation.
fn ask_card(app: &App, place: AskPlace) -> Element<'_, Message> {
    let large = app.ask_view == AskView::Expanded;
    let fit = card_fits(app);
    let mut header = row![
        glyph::icon(Glyph::Message, theme::accent(), 15.0),
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
    // The card keeps the conversation in the margin; the large window is
    // made small again. Clearing it is the margin's, and the palette's.
    header = header
        .push(if large {
            header_button(
                Glyph::Shrink,
                "Make small (Esc)",
                Message::Ask(AskMsg::Shrink),
            )
        } else {
            header_button(
                Glyph::PanelRight,
                "Keep in the margin",
                Message::Ask(AskMsg::KeepInMargin),
            )
        })
        .push(header_button(
            Glyph::Close,
            "Put away (Esc)",
            Message::Ask(AskMsg::Close),
        ));
    column![
        container(header)
            .height(Length::Fixed(ASK_HEADER_H))
            .align_y(iced::Center)
            .padding(Padding {
                top: 0.0,
                right: 8.0,
                bottom: 0.0,
                left: 16.0,
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

/// The conversation kept in the margin: the margin's Ask tab, as tall as
/// the margin, `height` — its header (what it is, clearing it, taking it
/// out of the margin, putting it away), what voice is doing, and the
/// conversation with its composer.
pub(crate) fn ask_margin(app: &App, height: f32) -> Element<'_, Message> {
    let mut header = row![
        glyph::icon(Glyph::Message, theme::accent(), 15.0),
        text("Ask")
            .size(ts::BASE)
            .font(crate::fonts::UI_SEMIBOLD)
            .color(theme::fg()),
        space().width(Fill),
    ]
    .spacing(8)
    .align_y(iced::Center);
    if !app.proj.ask_turns.is_empty() {
        header = header.push(
            button(text("Clear").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Ask(AskMsg::Clear)),
        );
    }
    let header = header
        .push(header_button(
            Glyph::Expand,
            "Take out of the margin",
            Message::Ask(AskMsg::Float),
        ))
        .push(header_button(
            Glyph::Close,
            "Put away (Esc)",
            Message::Ask(AskMsg::Close),
        ));
    column![
        container(header)
            .height(Length::Fixed(ASK_HEADER_H))
            .align_y(iced::Center)
            .padding(Padding {
                top: 0.0,
                right: 6.0,
                bottom: 0.0,
                left: 12.0,
            }),
        hairline(),
        voice_strip(app).unwrap_or_else(slot),
        ask_panel(app, (height - ASK_HEADER_H).max(ASK_CARD_MIN_H)),
    ]
    .height(Fill)
    .into()
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
