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
    /// How many times the window has been built.
    static BUILT: Cell<u64> = const { Cell::new(0) };
    /// The open drop-down's key, once its list was drawn in the window
    /// built last.
    static DRAWN: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Say which drop-down is open as the window is built (`ui::view`).
pub(crate) fn set_open(open: Option<&Picker>) {
    OPEN.with(|o| o.set(open.map(|p| (p.key, p.at))));
    BUILT.with(|b| b.set(b.get() + 1));
    DRAWN.with(|d| d.set(None));
}

/// How many times the window has been built: what a drop-down opened now
/// is drawn after ([`drawn`]).
pub(crate) fn built() -> u64 {
    BUILT.with(Cell::get)
}

/// Whether the drop-down of `key`, opened when the window had been built
/// `built` times, is the one drawn open: not built since, or its list in
/// the window built last. One whose rows changed under it — a voice said
/// another of them, the language they are said in changed, its options
/// came anew — was not drawn, and stood open unseen, taking every key and
/// picking with Enter the row it had when it opened.
pub(crate) fn drawn(key: u64, built: u64) -> bool {
    BUILT.with(Cell::get) == built || DRAWN.with(Cell::get) == Some(key)
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
        // Nothing to choose from: shown, dimmed, and not pressed — it was
        // a stop of the ring, a code and a word that opened nothing and
        // said nothing.
        if picks.is_empty() {
            return container(
                row![
                    text(placeholder.unwrap_or_default())
                        .size(size)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                    glyph::icon(Glyph::ChevronDown, theme::dim(), size),
                ]
                .spacing(8)
                .align_y(iced::Center),
            )
            .padding(padding)
            .width(width)
            .style(theme::pick_well_idle)
            .into();
        }
        // Each choice said by what the list shows for it.
        if super::heard::gathering() {
            for (said, send) in &picks {
                super::heard::said(said, send.clone());
            }
        }
        let key = key_of(&picks);
        let open = OPEN.with(Cell::get).filter(|(k, _)| *k == key);
        if open.is_some() {
            DRAWN.with(|d| d.set(Some(key)));
        }
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
            // As plain as its words, nearly: dim, it was a few faint pixels
            // on a dark theme, and the well read as a field.
            glyph::icon(Glyph::ChevronDown, theme::fg_muted(), size),
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
                built: 0,
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
    let mut rows = Column::new().padding(LIST_PAD);
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
            .padding(Padding::from([5.0, ROW_PAD_X]))
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
    .width(Length::Fixed(list_width(picks, size, width)))
    .max_height(LIST_MAX_H)
    .style(theme::pick_list_panel)
    .into()
}

/// The room round a list's rows, inside its panel.
const LIST_PAD: f32 = 4.0;
/// A row's room beside its words, each side.
const ROW_PAD_X: f32 = 8.0;

/// How wide an open list is: as its widest row — its tick, its words and
/// the scrollbar's lane — and no narrower than the well that opened it.
/// Its rows fill it: with no width of its own it was as wide as the
/// window, and opened at the window's left edge, far from its well.
fn list_width(picks: &[(String, Message)], size: f32, well: Length) -> f32 {
    let words = picks
        .iter()
        .map(|(said, _)| super::steady::width_of(said, size, crate::fonts::UI))
        .fold(0.0, f32::max);
    let rows = 2.0 * LIST_PAD + 2.0 * ROW_PAD_X + (size + 6.0) + words.ceil() + SCROLLBAR_W + 4.0;
    match well {
        Length::Fixed(w) => rows.max(w),
        _ => rows,
    }
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
        // The same rows sending something else: another.
        let mut other = picks(&["a", "b"]);
        other[1].1 = Message::Ask(AskMsg::PickAgent(None));
        assert_ne!(key_of(&picks(&["a", "b"])), key_of(&other));
    }

    /// A drop-down with nothing to choose from is no stop of the ring, has
    /// no code and no word: each opened nothing and said nothing.
    #[test]
    fn a_drop_down_with_nothing_to_choose_from_is_pressed_by_nothing() {
        let build = |rows: Vec<String>| {
            super::super::heard::gather_all_of(move || -> Element<'static, Message> {
                picker(rows, None::<String>, |_: String| Message::Tick)
                    .label("Language")
                    .into()
            })
        };
        let (words, marks) = build(Vec::new());
        assert!(
            words.is_empty(),
            "{:?}",
            words.iter().map(|w| &w.title).collect::<Vec<_>>()
        );
        assert!(marks.is_empty());
        let (words, marks) = build(vec!["python".into()]);
        assert!(!words.is_empty() && !marks.is_empty(), "one with a row is");
    }
}
