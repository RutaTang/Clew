//! Jump: every control on screen a few keys away. ⌘J puts a short code of
//! letters on each button, switch and row the window offers — of the layer
//! on top, as a click reaches — and typing a code presses it, sending the
//! very message its click sends. Esc, or a letter no code goes on with,
//! puts the codes away.
//!
//! A control is marked where it is built: [`crate::ui::button()`]'s press is,
//! and [`mark`] wraps any other (a row a pointer presses). The codes are
//! dealt as the window is drawn, in the order it is drawn, to the marks
//! that are on screen ([`root`]), and kept for the keys typed next
//! ([`pressed`]). Off, a mark costs a look at a thread-local: the press it
//! holds is kept only while the codes are up, or while [`heard::gather`]
//! reads the window.
//!
//! The same marks carry the keyboard's ring: with no field holding the
//! keys, Tab and Shift-Tab move a ring from control to control in the
//! order they are drawn ([`Up::Ring`]), and Enter or Space presses the one
//! it is on ([`ring_press`]) — a button reached by the keyboard is seen to
//! be, as a field holding the keys is.
//!
//! [`heard::gather`]: crate::ui::heard::gather

use std::cell::RefCell;
use std::collections::HashMap;

use iced::advanced::layout::{self, Layout};
use iced::advanced::text::Renderer as _;
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Renderer as _, Shell, mouse, overlay, renderer};
use iced::{Border, Color, Element, Event, Length, Pixels, Point, Rectangle, Size, Vector, window};

use crate::Message;
use crate::ui::heard::{self, Layer};

/// The letters codes are made of: the home row's first, as the hand rests.
const LETTERS: &[u8] = b"asdfjklghqwertyuiopzxcvbnm";

/// A badge's text size.
const BADGE_SIZE: f32 = 10.0;

/// How far a badge stands out left of its control's left edge.
const BADGE_OUT: f32 = 4.0;

/// How much of a badge's height comes down over its control: the rest
/// stands above the control's top edge, over its padding and what is above
/// it. Set on the corner 4 in, a badge hid the first letters of a crumb, a
/// status bar's "Python", and the icons of the sidebar's views.
const BADGE_OVER: f32 = 5.0;

/// What the view being built marks: whether the codes are up, the layer
/// it is in, and how many marks each layer has.
#[derive(Default)]
struct Build {
    on: bool,
    layer: Option<Layer>,
    marks: [usize; 4],
}

/// The codes being dealt as the window is drawn.
struct Dealing {
    /// Whether codes are dealt; else the marks are counted for the ring,
    /// which goes on [`Self::ring`].
    codes: bool,
    /// The mark the ring is on, by its place among those dealt.
    ring: Option<usize>,
    /// What has been typed of a code.
    typed: String,
    /// Which layers' marks a press reaches now.
    reach: Reach,
    /// How many letters a code has.
    len: usize,
    /// The marks dealt a code so far, with their presses.
    dealt: Vec<(String, Message)>,
}

/// The layers a press reaches: the one on top only, when it holds the
/// keys — a dialog, the tutorial, Ask large over the window — else all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reach {
    only: Option<Layer>,
}

impl Reach {
    fn of(top: Layer, ask_large: bool) -> Reach {
        Reach {
            only: (top >= Layer::Overlay || (ask_large && top == Layer::Ask)).then_some(top),
        }
    }

    fn reaches(self, layer: Layer) -> bool {
        self.only.is_none_or(|only| only == layer)
    }
}

/// Each window's codes on screen, with what each does.
type Dealt = HashMap<Option<window::Id>, Vec<(String, Message)>>;

/// Where each window's ring was drawn last: the mark it was on, among how
/// many, and the layer on top then.
#[derive(Debug, Clone, Copy)]
struct Ringed {
    at: Option<usize>,
    count: usize,
    top: Option<Layer>,
    /// Where the window asked it to be then ([`Up::Ring`]): while it asks
    /// the same, it stays where it was drawn — on the first control of a
    /// layer come up since, though the window asked for another.
    asked: isize,
}

/// What the marks are up for: codes to type, or the keyboard's ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Up<'a> {
    /// Jump mode's codes, with what has been typed of one.
    Codes(&'a str),
    /// The ring, on the mark at this place among those drawn — counted
    /// from the end when below 0 (Shift-Tab's first press: the last).
    Ring(isize),
}

thread_local! {
    static BUILD: RefCell<Build> = RefCell::new(Build::default());
    static DEALING: RefCell<Option<Dealing>> = const { RefCell::new(None) };
    /// The codes on screen in each window, dealt when it was last drawn.
    /// Kept apart: another window drawn after this one, with codes or
    /// without, leaves this one's as they are.
    static DEALT: RefCell<Dealt> = RefCell::new(HashMap::new());
    /// Where each window's ring was drawn last.
    static RINGED: RefCell<HashMap<Option<window::Id>, Ringed>> = RefCell::new(HashMap::new());
}

/// The view is built afresh: with the codes up (`on`) or not.
pub(crate) fn begin(on: bool) {
    BUILD.with(|b| {
        *b.borrow_mut() = Build {
            on,
            ..Build::default()
        }
    });
}

/// What is built from here on is in `layer`.
pub(crate) fn layer(layer: Layer) {
    BUILD.with(|b| b.borrow_mut().layer = Some(layer));
}

/// Whether a control built now keeps its press for a mark: while the codes
/// are up, or while the window is read for what can be pressed.
pub(crate) fn wanted() -> bool {
    BUILD.with(|b| b.borrow().on) || heard::gathering()
}

/// `content`, marked as pressed with `message`: a code is put on it while
/// the codes are up, and typing that code sends `message`.
pub(crate) fn mark<'a>(
    content: impl Into<Element<'a, Message>>,
    message: Message,
) -> Element<'a, Message> {
    let content = content.into();
    if !wanted() {
        return content;
    }
    heard::marked(&message);
    let layer = BUILD.with(|b| {
        let mut b = b.borrow_mut();
        let layer = b.layer.unwrap_or(Layer::Window);
        b.marks[layer as usize] += 1;
        layer
    });
    Element::new(Mark {
        content,
        message,
        layer,
    })
}

/// `view`, the whole of `window`, drawn with a code on each mark on screen
/// while `typed` is (what has been typed of a code so far); `ask_large`,
/// whether Ask is large over the window, leaving nothing under it to press.
pub(crate) fn root<'a>(
    view: Element<'a, Message>,
    up: Option<Up<'_>>,
    ask_large: bool,
    window: Option<window::Id>,
) -> Element<'a, Message> {
    let Some(up) = up else {
        forget(window);
        return view;
    };
    let (top, counts) = BUILD.with(|b| {
        let b = b.borrow();
        let top = [Layer::Tutorial, Layer::Overlay, Layer::Ask, Layer::Window]
            .into_iter()
            .find(|&l| b.marks[l as usize] > 0)
            .unwrap_or(Layer::Window);
        (top, b.marks)
    });
    let reach = Reach::of(top, ask_large);
    let marks = match reach.only {
        Some(only) => counts[only as usize],
        None => counts.iter().sum(),
    };
    let (typed, ring) = match up {
        Up::Codes(typed) => (Some(typed.to_string()), None),
        Up::Ring(at) => (None, Some(at)),
    };
    Element::new(Root {
        content: view,
        window,
        typed,
        ring,
        top,
        reach,
        len: code_len(marks),
    })
}

/// The mark the ring would be on in a window of `count` marks, asked for
/// `at` (from the end below 0); none when there are none — or, with the
/// count not known yet, for one from the end.
fn ring_place(at: isize, count: Option<usize>) -> Option<usize> {
    match count {
        Some(0) => None,
        Some(n) => Some(at.rem_euclid(n as isize) as usize),
        None => usize::try_from(at).ok(),
    }
}

/// Where `window`'s ring was drawn last — the mark it was on — and how
/// many marks there were.
#[cfg(test)]
pub(crate) fn ring_drawn(window: Option<window::Id>) -> Option<(usize, usize)> {
    RINGED.with(|r| {
        let r = r.borrow();
        let ringed = r.get(&window)?;
        Some((ringed.at?, ringed.count))
    })
}

/// The mark `window`'s ring is on, asked to be at `asked`: where it was
/// drawn, while that is what was asked (a layer come up since put it on
/// its first); else where `asked` falls among the marks drawn — a key
/// pressed again before the window is drawn again goes on from the last.
pub(crate) fn ring_on(window: Option<window::Id>, asked: isize) -> Option<usize> {
    RINGED.with(|r| {
        let r = r.borrow();
        let ringed = r.get(&window)?;
        match ringed.at {
            Some(at) if ringed.asked == asked => Some(at),
            _ => ring_place(asked, Some(ringed.count)),
        }
    })
}

/// What pressing the control `window`'s ring is on sends, as its click
/// sends; the ring asked to be at `asked`.
pub(crate) fn ring_press(window: Option<window::Id>, asked: isize) -> Option<Message> {
    let at = ring_on(window, asked)?;
    DEALT.with(|d| {
        d.borrow()
            .get(&window)
            .and_then(|dealt| dealt.get(at))
            .map(|(_, message)| message.clone())
    })
}

/// How many letters each code has for `marks` marks built: one while one
/// each will do, else two. Two tell 676 apart, more than a window shows at
/// once; a long tree builds its every row, most of them scrolled away, and
/// counted by those would make every code longer than any needs to be.
fn code_len(marks: usize) -> usize {
    if marks <= LETTERS.len() { 1 } else { 2 }
}

/// How many codes of `len` letters there are.
fn codes_of(len: usize) -> usize {
    LETTERS.len().pow(len as u32)
}

/// The `i`th code of `len` letters.
fn code(mut i: usize, len: usize) -> String {
    let n = LETTERS.len();
    let mut out = vec![0u8; len];
    for slot in out.iter_mut().rev() {
        *slot = LETTERS[i % n];
        i /= n;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// What typing `typed` does with the codes on screen: the press of the mark
/// whose code it is, or whether some code still goes on from it.
#[derive(Debug, Clone)]
pub(crate) enum Typed {
    /// The code of a mark: its press.
    Press(Box<Message>),
    /// A code goes on from it: wait for the next letter.
    More,
    /// No code goes on from it.
    Nothing,
}

/// `typed` against the codes dealt when `window` was last drawn. Before
/// any is dealt, every letter waits (the codes are on their way).
pub(crate) fn pressed(window: Option<window::Id>, typed: &str) -> Typed {
    DEALT.with(|d| {
        let all = d.borrow();
        let dealt = all.get(&window).map(Vec::as_slice).unwrap_or_default();
        if dealt.is_empty() {
            return Typed::More;
        }
        if let Some((_, message)) = dealt.iter().find(|(code, _)| code == typed) {
            return Typed::Press(Box::new(message.clone()));
        }
        if dealt.iter().any(|(code, _)| code.starts_with(typed)) {
            Typed::More
        } else {
            Typed::Nothing
        }
    })
}

/// `window`'s codes are put away: its codes are down, or it is closed.
pub(crate) fn forget(window: Option<window::Id>) {
    DEALT.with(|d| d.borrow_mut().remove(&window));
    RINGED.with(|r| r.borrow_mut().remove(&window));
}

/// The codes dealt when `window` was last drawn, in the order drawn.
#[cfg(test)]
pub(crate) fn dealt(window: Option<window::Id>) -> Vec<(String, Message)> {
    DEALT.with(|d| d.borrow().get(&window).cloned().unwrap_or_default())
}

/// The window, dealing codes to its marks as it is drawn — or counting
/// them for the ring.
struct Root<'a> {
    content: Element<'a, Message>,
    window: Option<window::Id>,
    /// Jump mode's: what has been typed of a code.
    typed: Option<String>,
    /// The ring's: the mark it is asked to be on ([`Up::Ring`]).
    ring: Option<isize>,
    /// The layer on top, whose marks a press reaches.
    top: Layer,
    reach: Reach,
    len: usize,
}

/// A control marked with its press.
struct Mark<'a> {
    content: Element<'a, Message>,
    message: Message,
    layer: Layer,
}

/// Both widgets are the content they wrap to everything but drawing: the
/// same tree, the same layout, the same events.
macro_rules! transparent {
    () => {
        fn tag(&self) -> tree::Tag {
            self.content.as_widget().tag()
        }

        fn state(&self) -> tree::State {
            self.content.as_widget().state()
        }

        fn children(&self) -> Vec<Tree> {
            self.content.as_widget().children()
        }

        fn diff(&self, tree: &mut Tree) {
            self.content.as_widget().diff(tree);
        }

        fn size(&self) -> Size<Length> {
            self.content.as_widget().size()
        }

        fn size_hint(&self) -> Size<Length> {
            self.content.as_widget().size_hint()
        }

        fn layout(
            &mut self,
            tree: &mut Tree,
            renderer: &iced::Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            self.content.as_widget_mut().layout(tree, renderer, limits)
        }

        fn operate(
            &mut self,
            tree: &mut Tree,
            layout: Layout<'_>,
            renderer: &iced::Renderer,
            operation: &mut dyn Operation,
        ) {
            self.content
                .as_widget_mut()
                .operate(tree, layout, renderer, operation);
        }

        fn mouse_interaction(
            &self,
            tree: &Tree,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            viewport: &Rectangle,
            renderer: &iced::Renderer,
        ) -> mouse::Interaction {
            self.content
                .as_widget()
                .mouse_interaction(tree, layout, cursor, viewport, renderer)
        }

        fn overlay<'b>(
            &'b mut self,
            tree: &'b mut Tree,
            layout: Layout<'b>,
            renderer: &iced::Renderer,
            viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
            self.content
                .as_widget_mut()
                .overlay(tree, layout, renderer, viewport, translation)
        }
    };
}

/// [`transparent!`]'s events: the content's own.
macro_rules! transparent_update {
    () => {
        fn update(
            &mut self,
            tree: &mut Tree,
            event: &Event,
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            renderer: &iced::Renderer,
            clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, Message>,
            viewport: &Rectangle,
        ) {
            self.content.as_widget_mut().update(
                tree, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
        }
    };
}

thread_local! {
    /// Windows whose ring could not be placed when drawn (its marks not
    /// counted yet, or fewer now): drawn again, now they are.
    static REDRAW: RefCell<std::collections::HashSet<Option<window::Id>>> =
        RefCell::new(std::collections::HashSet::new());
}

impl Widget<Message, iced::Theme, iced::Renderer> for Root<'_> {
    transparent!();

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.ring.is_some() {
            if REDRAW.with(|r| r.borrow_mut().remove(&self.window)) {
                shell.request_redraw();
            }
            // A press of the pointer puts the ring away: the pointer has
            // the window now.
            if let Event::Mouse(mouse::Event::ButtonPressed(_)) = event {
                shell.publish(Message::Window(crate::WindowMsg::RingEnd));
            }
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // Where the ring goes: among as many marks as were drawn last, and
        // on the first of a layer come up on top since (a menu opened, a
        // dialog) — as a dialog takes the keyboard.
        let last = RINGED.with(|r| r.borrow().get(&self.window).copied());
        let ring = self.ring.and_then(|at| match last {
            Some(last) if last.top.is_some_and(|top| top != self.top) => Some(0),
            Some(last) if last.asked == at => last.at.or(ring_place(at, Some(last.count))),
            Some(last) => ring_place(at, Some(last.count)),
            None => ring_place(at, None),
        });
        DEALING.with(|d| {
            *d.borrow_mut() = Some(Dealing {
                codes: self.typed.is_some(),
                ring,
                typed: self.typed.clone().unwrap_or_default(),
                reach: self.reach,
                len: self.len,
                dealt: Vec::new(),
            });
        });
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        let dealt = DEALING
            .with(|d| d.borrow_mut().take())
            .map(|d| d.dealt)
            .unwrap_or_default();
        if self.ring.is_some() {
            let count = dealt.len();
            let at = ring.filter(|&at| at < count);
            // Not placed — the count was not known, or there are fewer marks
            // than it was asked for — while there are marks to place it on:
            // drawn again, the count known now.
            if at.is_none() && count > 0 {
                REDRAW.with(|r| r.borrow_mut().insert(self.window));
            }
            RINGED.with(|r| {
                r.borrow_mut().insert(
                    self.window,
                    Ringed {
                        at,
                        count,
                        top: Some(self.top),
                        asked: self.ring.unwrap_or_default(),
                    },
                )
            });
        }
        DEALT.with(|d| d.borrow_mut().insert(self.window, dealt));
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Mark<'_> {
    transparent!();
    transparent_update!();

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        let bounds = layout.bounds();
        // Off screen — scrolled away, clipped out — it is no place to go.
        let Some(seen) = bounds.intersection(viewport) else {
            return;
        };
        if seen.width < 1.0 || seen.height < 1.0 {
            return;
        }
        let shown = DEALING.with(|d| {
            let mut d = d.borrow_mut();
            let d = d.as_mut()?;
            if !d.reach.reaches(self.layer) {
                return None;
            }
            // The ring's: counted, and ringed when it is the one.
            if !d.codes {
                let place = d.dealt.len();
                d.dealt.push((String::new(), self.message.clone()));
                return (d.ring == Some(place)).then_some(Shown::Ring);
            }
            // Past the last code, a mark goes without (see `code_len`).
            if d.dealt.len() >= codes_of(d.len) {
                return None;
            }
            let code = code(d.dealt.len(), d.len);
            d.dealt.push((code.clone(), self.message.clone()));
            let shown = code.starts_with(&d.typed);
            shown.then_some(Shown::Badge(code, d.typed.len()))
        });
        match shown {
            Some(Shown::Badge(code, typed)) => badge(renderer, seen, *viewport, &code, typed),
            Some(Shown::Ring) => ring(renderer, bounds, *viewport),
            None => {}
        }
    }
}

/// What a mark shows: its code's badge, or the keyboard's ring.
enum Shown {
    Badge(String, usize),
    Ring,
}

/// How far the ring stands out round its control.
const RING_OUT: f32 = 2.0;

/// The ring round `at`, the control the keyboard is on: the accent edge a
/// field holding the keys has, a little outside the control, within
/// `room` (what of the window is seen).
fn ring(renderer: &mut iced::Renderer, at: Rectangle, room: Rectangle) {
    let palette = crate::theme::active();
    let bounds = at.expand(RING_OUT);
    let Some(seen) = bounds.expand(1.0).intersection(&room) else {
        return;
    };
    renderer.with_layer(seen, |renderer| {
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border {
                    color: palette.accent,
                    width: 2.0,
                    radius: 7.0.into(),
                },
                ..renderer::Quad::default()
            },
            Color::TRANSPARENT,
        );
    });
}

/// Where the badge of a `letters`-long code goes for the control at `at`:
/// on its top-left corner, mostly above it — only [`BADGE_OVER`] of it
/// down over the control — and inside `room`, what of the window is seen.
fn badge_bounds(at: Rectangle, room: Rectangle, letters: usize) -> Rectangle {
    let width = (letters as f32 * BADGE_SIZE * 0.62 + 6.0).round();
    let height = (BADGE_SIZE + 4.0).round();
    Rectangle {
        x: (at.x - BADGE_OUT).max(room.x).round(),
        y: (at.y + BADGE_OVER - height).max(room.y).round(),
        width,
        height,
    }
}

/// A code's badge on the top-left corner of `at`, standing out over it,
/// within `room` (what of the window is seen), the letters already typed
/// dimmed.
fn badge(renderer: &mut iced::Renderer, at: Rectangle, room: Rectangle, code: &str, typed: usize) {
    let palette = crate::theme::active();
    let bounds = badge_bounds(at, room, code.chars().count());
    let height = bounds.height;
    let room = Rectangle {
        x: bounds.x - 1.0,
        y: bounds.y - 1.0,
        width: bounds.width + 2.0,
        height: bounds.height + 2.0,
    };
    renderer.with_layer(room, |renderer| {
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border {
                    color: palette.on_accent,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..renderer::Quad::default()
            },
            palette.accent,
        );
        let upper = code.to_uppercase();
        let (done, rest) = upper.split_at(typed.min(upper.len()));
        let advance = BADGE_SIZE * 0.62;
        let mut x = bounds.x + 3.0;
        for (part, color) in [
            (
                done,
                Color {
                    a: 0.55,
                    ..palette.on_accent
                },
            ),
            (rest, palette.on_accent),
        ] {
            if part.is_empty() {
                continue;
            }
            let w = part.chars().count() as f32 * advance;
            renderer.fill_text(
                iced::advanced::Text {
                    content: part.to_string(),
                    bounds: Size::new(w + 2.0, height),
                    size: Pixels(BADGE_SIZE),
                    line_height: iced::widget::text::LineHeight::Absolute(Pixels(height)),
                    font: crate::fonts::CODE_BOLD,
                    align_x: iced::advanced::text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Center,
                    shaping: iced::advanced::text::Shaping::Basic,
                    wrapping: iced::advanced::text::Wrapping::None,
                },
                Point::new(x, bounds.center_y()),
                color,
                room,
            );
            x += w;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A badge comes down over its control by no more than a sliver —
    /// most of it stands above — and stays in the window: pinned to its
    /// top, it is as low as it must be, no lower.
    #[test]
    fn a_badge_stands_above_what_it_marks() {
        let room = Rectangle::new(Point::ORIGIN, Size::new(800.0, 600.0));
        let at = Rectangle::new(Point::new(100.0, 100.0), Size::new(32.0, 32.0));
        let b = badge_bounds(at, room, 2);
        assert!(b.y + b.height - at.y <= BADGE_OVER, "{b:?}");
        assert!(b.x < at.x && b.y < at.y, "{b:?}");
        let top = Rectangle::new(Point::new(100.0, 0.0), Size::new(32.0, 20.0));
        assert_eq!(badge_bounds(top, room, 2).y, 0.0, "kept in the window");
    }

    #[test]
    fn codes_are_as_short_as_tell_the_marks_apart() {
        assert_eq!(code_len(0), 1);
        assert_eq!(code_len(26), 1);
        assert_eq!(code_len(27), 2);
        assert_eq!(code_len(26 * 26), 2);
        assert_eq!(code_len(5000), 2, "never longer than two");
        assert_eq!(codes_of(2), 676);
        assert_eq!(code(0, 1), "a");
        assert_eq!(code(1, 1), "s");
        assert_eq!(code(0, 2), "aa");
        assert_eq!(code(27, 2), "ss");
        // Every code of a length is another.
        let all: std::collections::HashSet<String> = (0..26 * 26).map(|i| code(i, 2)).collect();
        assert_eq!(all.len(), 26 * 26);
    }

    #[test]
    fn a_dialog_or_large_ask_takes_the_codes_from_what_is_under_it() {
        assert!(Reach::of(Layer::Ask, false).reaches(Layer::Window));
        assert!(Reach::of(Layer::Ask, true).reaches(Layer::Ask));
        assert!(!Reach::of(Layer::Ask, true).reaches(Layer::Window));
        assert!(!Reach::of(Layer::Overlay, false).reaches(Layer::Ask));
        assert!(Reach::of(Layer::Tutorial, false).reaches(Layer::Tutorial));
        assert!(!Reach::of(Layer::Tutorial, false).reaches(Layer::Overlay));
    }
}
