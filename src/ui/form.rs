//! The settings' form, which every page of Settings is built from: a page
//! is groups, each a title over a card of rows; a row is what it sets (and
//! a line under that) at the left and the control at the right, or one of
//! several choices, marked as a radio's, the whole row picking it.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Room inside a group's card, at either side of its rows.
const ROW_PAD_X: f32 = 14.0;
/// Room above and below a row's words.
const ROW_PAD_Y: f32 = 10.0;
/// The column a [`field`] row's label takes.
pub(crate) const LABEL_W: f32 = 128.0;
/// Room between a page's groups.
const GROUP_GAP: f32 = 24.0;
/// Width of a picker at a row's right.
pub(crate) const PICK_W: f32 = 220.0;
/// A radio's mark, and the dot in it once chosen.
const RADIO: f32 = 16.0;
const RADIO_DOT: f32 = 8.0;
/// The switch: its track, and the knob in it.
pub(crate) const SWITCH_W: f32 = 26.0;
pub(crate) const SWITCH_H: f32 = 16.0;
const SWITCH_KNOB: f32 = 12.0;

/// A page: its parts — groups, and notes between them — [`GROUP_GAP`]
/// apart.
pub(crate) fn page<'a>(parts: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    Column::with_children(parts)
        .spacing(GROUP_GAP)
        .width(Fill)
        .into()
}

/// A group: `title` (with `aside` at its right — something that acts on
/// the whole group) and under it `about`, then its rows in a card with a
/// hairline between each. An empty title shows the card alone.
pub(crate) fn group<'a>(
    title: &str,
    about: Option<String>,
    aside: Option<Element<'a, Message>>,
    rows: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut col = Column::new().spacing(8).width(Fill);
    if !title.is_empty() || aside.is_some() {
        let mut head = row![
            text(title.to_string())
                .size(ts::BASE)
                .font(crate::fonts::UI_MEDIUM)
                .color(theme::fg()),
            space().width(Fill),
        ]
        .align_y(iced::Center);
        if let Some(aside) = aside {
            head = head.push(aside);
        }
        let mut top = column![head].spacing(2);
        if let Some(about) = about {
            top = top.push(note(about));
        }
        col = col.push(container(top).padding([0.0, 2.0]));
    } else if let Some(about) = about {
        col = col.push(note(about));
    }
    if !rows.is_empty() {
        let mut card = Column::new().width(Fill);
        let count = rows.len();
        for (i, r) in rows.into_iter().enumerate() {
            card = card.push(r);
            if i + 1 < count {
                card = card.push(container(rule()).padding(Padding {
                    left: ROW_PAD_X,
                    ..Padding::ZERO
                }));
            }
        }
        col = col.push(container(card).width(Fill).style(theme::form_card));
    }
    col.into()
}

/// A hairline across a card, between two of its rows.
fn rule<'a>() -> Element<'a, Message> {
    container(space().width(Fill).height(1))
        .width(Fill)
        .height(1)
        .style(|_: &iced::Theme| container::Style {
            background: Some(theme::hairline().into()),
            ..Default::default()
        })
        .into()
}

/// What a row sets, and under it `about`.
fn label<'a>(label: String, about: Option<String>) -> Element<'a, Message> {
    let mut col = column![text(label).size(ts::BASE).color(theme::fg())].spacing(2);
    if let Some(about) = about {
        col = col.push(
            text(about)
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::WordOrGlyph),
        );
    }
    col.into()
}

/// A row: `label` and under it `about` at the left, `control` at the right.
pub(crate) fn item<'a>(
    name: impl Into<String>,
    about: Option<String>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        container(label(name.into(), about)).width(Fill),
        control.into(),
    ]
    .spacing(16)
    .align_y(iced::Center)
    .padding([ROW_PAD_Y, ROW_PAD_X])
    .into()
}

/// A row whose control wants the width (a typed field, a list of chips):
/// `label` in a column of its own ([`LABEL_W`]), the control taking the
/// rest, their first lines level.
pub(crate) fn field<'a>(
    name: impl Into<String>,
    about: Option<String>,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        container(label(name.into(), about))
            .width(LABEL_W)
            .padding(Padding {
                top: 7.0,
                ..Padding::ZERO
            }),
        container(control.into()).width(Fill),
    ]
    .spacing(12)
    .padding([ROW_PAD_Y - 2.0, ROW_PAD_X])
    .into()
}

/// A row of its own content, padded as the others are.
pub(crate) fn plain<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content.into())
        .width(Fill)
        .padding([ROW_PAD_Y, ROW_PAD_X])
        .into()
}

/// One of several choices: a radio's mark, `name`, and under it `detail`;
/// pressing the row picks it.
pub(crate) fn choice<'a>(
    name: impl Into<String>,
    detail: Option<Element<'a, Message>>,
    chosen: bool,
    msg: Message,
) -> Element<'a, Message> {
    choice_with(name, detail, chosen, msg, None)
}

/// [`choice`], with `trailing` at its right — something of the choice's
/// own to press (its download, say), outside the part that picks it.
pub(crate) fn choice_with<'a>(
    name: impl Into<String>,
    detail: Option<Element<'a, Message>>,
    chosen: bool,
    msg: Message,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut words = column![text(name.into()).size(ts::BASE).color(theme::fg())].spacing(2);
    if let Some(detail) = detail {
        words = words.push(detail);
    }
    let pick = button(
        row![radio(chosen), words.width(Fill)]
            .spacing(10)
            .align_y(iced::Center),
    )
    .style(theme::form_choice)
    .width(Fill)
    .padding([ROW_PAD_Y - 2.0, ROW_PAD_X])
    .on_press(msg);
    match trailing {
        None => pick.into(),
        Some(trailing) => row![
            pick,
            container(trailing).padding(Padding {
                right: ROW_PAD_X,
                ..Padding::ZERO
            }),
        ]
        .align_y(iced::Center)
        .into(),
    }
}

/// A radio's mark: a ring, lit with a dot in it once chosen.
pub(crate) fn radio<'a>(on: bool) -> Element<'a, Message> {
    let dot =
        container(space().width(RADIO_DOT).height(RADIO_DOT)).style(move |_: &iced::Theme| {
            container::Style {
                background: on.then(|| theme::accent().into()),
                border: iced::Border {
                    radius: (RADIO_DOT / 2.0).into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        });
    container(dot)
        .width(RADIO)
        .height(RADIO)
        .center_x(RADIO)
        .center_y(RADIO)
        .style(move |_: &iced::Theme| container::Style {
            background: Some(theme::bg().into()),
            border: iced::Border {
                radius: (RADIO / 2.0).into(),
                width: 1.5,
                color: if on {
                    theme::accent()
                } else {
                    theme::wash(0.28)
                },
            },
            ..Default::default()
        })
        .into()
}

/// A switch: its track, the knob in from the end it is at; pressing it
/// sends `msg`.
pub(crate) fn switch<'a>(on: bool, msg: Message) -> iced::widget::Button<'a, Message> {
    let knob = container(space().width(SWITCH_KNOB).height(SWITCH_KNOB)).style(theme::switch_knob);
    button(
        container(knob)
            .width(Fill)
            .height(Fill)
            .padding((SWITCH_H - SWITCH_KNOB) / 2.0)
            .align_x(if on {
                iced::alignment::Horizontal::Right
            } else {
                iced::alignment::Horizontal::Left
            }),
    )
    .width(Length::Fixed(SWITCH_W))
    .height(Length::Fixed(SWITCH_H))
    .padding(0)
    .style(theme::switch(on))
    .on_press(msg)
}

/// A row that turns something on or off: `name`, `about`, and a switch.
pub(crate) fn toggle<'a>(
    name: impl Into<String>,
    about: Option<String>,
    on: bool,
    msg: fn(bool) -> Message,
) -> Element<'a, Message> {
    item(name, about, switch(on, msg(!on)))
}

/// A button of a row: the fields' rounded well, a shade deeper.
pub(crate) fn action<'a>(label: &str, msg: Option<Message>) -> iced::widget::Button<'a, Message> {
    button(text(label.to_string()).size(ts::BODY))
        .style(theme::secondary_button)
        .padding([5, 12])
        .on_press_maybe(msg)
}

/// A row's main button (Add, Download): filled with the accent.
pub(crate) fn primary<'a>(label: &str, msg: Option<Message>) -> iced::widget::Button<'a, Message> {
    button(text(label.to_string()).size(ts::BODY))
        .style(theme::primary_button)
        .padding([5, 12])
        .on_press_maybe(msg)
}

/// A typed field of a row: the rounded well every field has.
pub(crate) fn input<'a>(placeholder: &str, value: &str) -> iced::widget::TextInput<'a, Message> {
    text_input(placeholder, value)
        .style(theme::input)
        .size(ts::BASE)
        .padding([6, 9])
        .width(Fill)
}

/// A picker of a row, [`PICK_W`] wide.
pub(crate) fn pick<'a, T, L, V>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> Message + 'a,
) -> iced::widget::PickList<'a, T, L, V, Message>
where
    T: ToString + PartialEq + Clone + 'a,
    L: std::borrow::Borrow<[T]> + 'a,
    V: std::borrow::Borrow<T> + 'a,
{
    pick_list(options, selected, on_select)
        .style(theme::pick)
        .menu_style(theme::pick_menu)
        .text_size(ts::BASE)
        .padding([6, 10])
        .width(PICK_W)
}

/// A line said under a group, or between them: small and dim.
pub(crate) fn note<'a>(s: impl Into<String>) -> iced::widget::Text<'a> {
    text(s.into())
        .size(ts::SMALL)
        .color(theme::dim())
        .wrapping(Wrapping::WordOrGlyph)
}

/// A path or command as a row shows it: in the code face, muted, broken
/// anywhere.
pub(crate) fn path<'a>(s: impl Into<String>) -> iced::widget::Text<'a> {
    text(s.into())
        .size(ts::SMALL)
        .font(crate::fonts::CODE)
        .color(theme::fg_muted())
        .wrapping(Wrapping::Glyph)
}

/// A segmented control: one segment per `(label, chosen, msg)`, the
/// chosen one raised in the trough.
pub(crate) fn segmented<'a>(
    segments: impl IntoIterator<Item = (String, bool, Message)>,
) -> Element<'a, Message> {
    container(
        Row::with_children(segments.into_iter().map(|(label, chosen, msg)| {
            button(text(label).size(ts::BODY))
                .style(theme::segment(chosen))
                .padding([4, 14])
                .on_press(msg)
                .into()
        }))
        .spacing(2),
    )
    .padding(2)
    .style(theme::trough)
    .into()
}
