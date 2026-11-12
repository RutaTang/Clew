//! A drop-down the keyboard can work as well as the pointer ([`picker`]):
//! a well showing the value chosen, which opens a list under it. Pressed
//! from the keyboard's ring, by its jump code or with Enter or Space, it
//! opens, the arrows walk its rows, Enter or Space picks one and Esc puts
//! it away (`App::picker_key`). iced's own drop-down opened for the pointer
//! alone: pressed from the keyboard it moved on to the next value — Enter
//! on Display language switched the whole window to French, then Chinese —
//! and the ring named it by the value it would switch to.
//!
//! What is open is the App's ([`crate::app::pickers::Picker`]), read here
//! as the window is built ([`set_open`]).

use std::cell::Cell;
use std::hash::{Hash, Hasher};

use super::*;
use crate::app::pickers::{Picker, PickerMsg};

thread_local! {
    /// The drop-down open as the window is built, by its key, and the row
    /// the keys rest on.
    static OPEN: Cell<Option<(u64, usize)>> = const { Cell::new(None) };
}

/// Say which drop-down is open as the window is built (`ui::view`).
pub(crate) fn set_open(open: Option<&Picker>) {
    OPEN.with(|o| o.set(open.map(|p| (p.key, p.at))));
}

/// Whether a drop-down is open as the window is built: the arrows are its
/// rows', not the ring's.
pub(crate) fn is_open() -> bool {
    OPEN.with(Cell::get).is_some()
}

/// The open list's scrollable, which the keys keep their row in view in.
pub(crate) fn list_id() -> iced::widget::Id {
    iced::widget::Id::new("picker-list")
}

/// The most a list shows before it scrolls.
const LIST_MAX_H: f32 = 320.0;

/// A drop-down of `options`, `selected` shown in its well, sending
/// `on_select` of the one picked.
pub(crate) fn picker<'a, T, L, V>(
    options: L,
    selected: Option<V>,
    on_select: impl Fn(T) -> Message + 'a,
) -> Pick<'a>
where
    T: ToString + PartialEq + Clone + 'a,
    L: std::borrow::Borrow<[T]> + 'a,
    V: std::borrow::Borrow<T> + 'a,
{
    let choices = std::borrow::Borrow::<[T]>::borrow(&options);
    let at = selected
        .as_ref()
        .and_then(|v| choices.iter().position(|c| c == v.borrow()));
    let picks = choices
        .iter()
        .map(|c| (c.to_string(), on_select(c.clone())))
        .collect();
    Pick {
        picks,
        at,
        placeholder: None,
        label: None,
        size: ts::BASE,
        padding: Padding::from([6, 10]),
        width: Length::Shrink,
        _life: std::marker::PhantomData,
    }
}

/// A drop-down as [`picker`] builds it.
pub(crate) struct Pick<'a> {
    /// Each row's text, and what picking it sends.
    picks: Vec<(String, Message)>,
    /// The row chosen now.
    at: Option<usize>,
    placeholder: Option<String>,
    /// What the ring calls it, ahead of its value, when no field round it
    /// names it ([`super::jump::field_named`]).
    label: Option<String>,
    size: f32,
    padding: Padding,
    width: Length,
    _life: std::marker::PhantomData<&'a ()>,
}

impl Pick<'_> {
    /// What its well says while nothing is chosen.
    pub(crate) fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        self.placeholder = Some(placeholder.into());
        self
    }

    /// What the ring calls it, ahead of its value ("Map: Imports").
    pub(crate) fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub(crate) fn text_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub(crate) fn padding(mut self, padding: impl Into<Padding>) -> Self {
        self.padding = padding.into();
        self
    }

    pub(crate) fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }
}

/// Which drop-down `picks` is: the same list with the same sends is the
/// same control, built again.
fn key_of(picks: &[(String, Message)]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (said, send) in picks {
        said.hash(&mut hasher);
        format!("{send:?}").hash(&mut hasher);
    }
    hasher.finish()
}

impl<'a> From<Pick<'a>> for Element<'a, Message> {
    fn from(pick: Pick<'a>) -> Self {
        let Pick {
            picks,
            at,
            placeholder,
            label,
            size,
            padding,
            width,
            ..
        } = pick;
        // Each choice said by what the list shows for it.
        if super::heard::gathering() {
            for (said, send) in &picks {
                super::heard::said(said, send.clone());
            }
        }
        let key = key_of(&picks);
        let open = OPEN.with(Cell::get).filter(|(k, _)| *k == key);
        let shown = at.and_then(|i| picks.get(i)).map(|(said, _)| said.clone());
        let well = row![
            text(shown.clone().or(placeholder).unwrap_or_default())
                .size(size)
                .color(if shown.is_some() {
                    theme::fg()
                } else {
                    theme::dim()
                })
                .wrapping(Wrapping::None)
                .width(if matches!(width, Length::Shrink) {
                    Length::Shrink
                } else {
                    Length::Fill
                }),
            glyph::icon(Glyph::ChevronDown, theme::dim(), size - 1.0),
        ]
        .spacing(8)
        .align_y(iced::Center);
        let press = if open.is_some() {
            Message::Picker(PickerMsg::Close)
        } else {
            Message::Picker(PickerMsg::Open(Picker {
                key,
                picks: picks.clone(),
                at: at.unwrap_or(0),
                chosen: at,
                owner: None,
            }))
        };
        // Opened by voice by its name — its row's — or, with none, by the
        // value it shows.
        let spoken = label.clone().or_else(|| shown.clone()).unwrap_or_default();
        // iced's own button: the mark round it is this one's
        // (`mark_field`), named by its row and value — the window's buttons
        // mark themselves, and the ring stopped on it twice.
        let well = iced::widget::button(well)
            .padding(padding)
            .width(width)
            .style(theme::pick_well(open.is_some()))
            .on_press(super::heard::said(&spoken, press.clone()));
        // Named by its field and the value it shows ("Display language:
        // English") — not by the value pressing it gave next.
        let well = super::jump::mark_field(well, press, shown.unwrap_or_default(), label);
        let list = open.map(|(_, row_at)| list(&picks, row_at, at, size, width));
        super::dropdown::dropdown(well, list, iced::Vector::new(0.0, 4.0))
            .dismiss(Message::Picker(PickerMsg::Close))
            .into()
    }
}

/// The open list: a row for each choice, the one the keys rest on lit, the
/// one chosen ticked.
fn list<'a>(
    picks: &[(String, Message)],
    row_at: usize,
    chosen: Option<usize>,
    size: f32,
    width: Length,
) -> Element<'a, Message> {
    let mut rows = Column::new().padding(4);
    // As wide as the well at least: a list narrower than what opened it
    // read as another control's.
    if let Length::Fixed(w) = width {
        rows = rows.push(space().width(Length::Fixed(w - 8.0)).height(0));
    }
    for (i, (said, _)) in picks.iter().enumerate() {
        let tick: Element<'a, Message> = if chosen == Some(i) {
            glyph::icon(Glyph::Check, theme::accent(), size)
        } else {
            space().width(Length::Fixed(size)).into()
        };
        // Its rows are the keys' (`App::picker_key`) and the pointer's: no
        // ring stops or codes of their own.
        rows = rows.push(
            iced::widget::button(
                row![
                    container(tick).width(Length::Fixed(size + 6.0)),
                    text(said.clone()).size(size).wrapping(Wrapping::None),
                ]
                .align_y(iced::Center),
            )
            .width(Length::Fill)
            .padding([5, 8])
            .style(theme::pick_row(i == row_at))
            .on_press(Message::Picker(PickerMsg::Pick(i))),
        );
    }
    container(
        scrollable(rows)
            .id(list_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar),
    )
    .max_height(LIST_MAX_H)
    .style(theme::pick_list_panel)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same list is the same drop-down, built again; another list, or
    /// the same rows sending something else, is another.
    #[test]
    fn a_drop_down_is_known_by_its_rows_and_what_they_send() {
        let picks = |sends: &[&str]| -> Vec<(String, Message)> {
            sends
                .iter()
                .map(|s| {
                    (
                        s.to_string(),
                        Message::Ask(AskMsg::PickAgent(Some(s.to_string()))),
                    )
                })
                .collect()
        };
        assert_eq!(key_of(&picks(&["a", "b"])), key_of(&picks(&["a", "b"])));
        assert_ne!(key_of(&picks(&["a", "b"])), key_of(&picks(&["a", "c"])));
    }
}
