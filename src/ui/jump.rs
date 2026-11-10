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

/// How far a badge stands out over its control's top-left corner, so
/// that a small icon under it still shows.
const BADGE_OUT: f32 = 4.0;

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

thread_local! {
    static BUILD: RefCell<Build> = RefCell::new(Build::default());
    static DEALING: RefCell<Option<Dealing>> = const { RefCell::new(None) };
    /// The codes on screen in each window, dealt when it was last drawn.
    /// Kept apart: another window drawn after this one, with codes or
    /// without, leaves this one's as they are.
    static DEALT: RefCell<HashMap<Option<window::Id>, Vec<(String, Message)>>> =
        RefCell::new(HashMap::new());
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
    typed: Option<&str>,
    ask_large: bool,
    window: Option<window::Id>,
) -> Element<'a, Message> {
    let Some(typed) = typed else {
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
    Element::new(Root {
        content: view,
        window,
        typed: typed.to_string(),
        reach,
        len: code_len(marks),
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
}

/// The codes dealt when `window` was last drawn, in the order drawn.
#[cfg(test)]
pub(crate) fn dealt(window: Option<window::Id>) -> Vec<(String, Message)> {
    DEALT.with(|d| d.borrow().get(&window).cloned().unwrap_or_default())
}

/// The window, dealing codes to its marks as it is drawn.
struct Root<'a> {
    content: Element<'a, Message>,
    window: Option<window::Id>,
    typed: String,
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

impl Widget<Message, iced::Theme, iced::Renderer> for Root<'_> {
    transparent!();

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
        DEALING.with(|d| {
            *d.borrow_mut() = Some(Dealing {
                typed: self.typed.clone(),
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
        DEALT.with(|d| d.borrow_mut().insert(self.window, dealt));
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Mark<'_> {
    transparent!();

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
            // Past the last code, a mark goes without (see `code_len`).
            if !d.reach.reaches(self.layer) || d.dealt.len() >= codes_of(d.len) {
                return None;
            }
            let code = code(d.dealt.len(), d.len);
            d.dealt.push((code.clone(), self.message.clone()));
            let shown = code.starts_with(&d.typed);
            shown.then_some((code, d.typed.len()))
        });
        if let Some((code, typed)) = shown {
            badge(renderer, seen, *viewport, &code, typed);
        }
    }
}

/// A code's badge on the top-left corner of `at`, standing out over it,
/// within `room` (what of the window is seen), the letters already typed
/// dimmed.
fn badge(renderer: &mut iced::Renderer, at: Rectangle, room: Rectangle, code: &str, typed: usize) {
    let palette = crate::theme::active();
    let letters = code.chars().count() as f32;
    let width = (letters * BADGE_SIZE * 0.62 + 6.0).round();
    let height = (BADGE_SIZE + 4.0).round();
    let bounds = Rectangle {
        x: (at.x - BADGE_OUT).max(room.x).round(),
        y: (at.y - BADGE_OUT).max(room.y).round(),
        width,
        height,
    };
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
