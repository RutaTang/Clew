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
//! The same marks are the stops of the keyboard's ring ([`super::ring`]):
//! the window, at the root of its view ([`root`]), takes Tab and moves the
//! ring among them and its fields.
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
use crate::ui::ring::{self, RingAsk, StopKind};

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
    /// The number the next mark is built with: counted on past the view's
    /// own by marks built as the window is laid out (a `responsive`'s), so
    /// that no two marks of a window share one.
    next: usize,
}

/// The first number of a mark built as the window is laid out, past those
/// built with the view: a window laid out again after another's view was
/// built counts on from that one's, past its own view's still.
const LAID_OUT_MARKS: usize = usize::MAX / 2;

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

/// Each window's codes on screen, with what each does.
type Dealt = HashMap<Option<window::Id>, Vec<(String, Message)>>;

/// What the marks are up for: codes to type, or the keyboard's ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Up<'a> {
    /// Jump mode's codes, with what has been typed of one.
    Codes(&'a str),
    /// The ring, where the window asks it to be.
    Ring(RingAsk),
}

thread_local! {
    static BUILD: RefCell<Build> = RefCell::new(Build::default());
    static DEALING: RefCell<Option<Dealing>> = const { RefCell::new(None) };
    /// The codes on screen in each window, dealt when it was last drawn.
    /// Kept apart: another window drawn after this one, with codes or
    /// without, leaves this one's as they are.
    static DEALT: RefCell<Dealt> = RefCell::new(HashMap::new());
    /// Where each window's ring is (`ring::Placed`).
    static PLACED: RefCell<HashMap<Option<window::Id>, ring::Placed>> =
        RefCell::new(HashMap::new());
    /// The mark the ring is on in the window being drawn.
    static RINGED: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
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

/// Whether what a control is called is kept as it is built
/// ([`super::heard::name_of`]): while the codes or the ring are up, for the
/// ring to say the name of the control it is on.
pub(crate) fn naming() -> bool {
    BUILD.with(|b| b.borrow().on)
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
    // The name its control offered as it was built, just now.
    let name = naming().then(|| heard::name_of(&message)).flatten();
    let (layer, id) = BUILD.with(|b| {
        let mut b = b.borrow_mut();
        let layer = b.layer.unwrap_or(Layer::Window);
        b.marks[layer as usize] += 1;
        let id = b.next;
        b.next += 1;
        (layer, id)
    });
    Element::new(Mark {
        content,
        message,
        layer,
        id,
        name,
    })
}

/// `content`, said to be the part `tag` of the window — a layer, the
/// title bar, the Files view's tree — for the ring's order ([`ring`]).
pub(crate) fn scope<'a>(
    content: impl Into<Element<'a, Message>>,
    tag: ring::ScopeTag,
) -> Element<'a, Message> {
    Element::new(Scope {
        content: content.into(),
        tag,
    })
}

/// How the window being built stands: the layer on top — the tutorial, a
/// dialog holding the keys, else Ask over the window — and, for a dialog,
/// which one it is.
pub(crate) struct OnTop {
    pub(crate) layer: Layer,
    pub(crate) scene: Option<String>,
}

/// `view`, the whole of `window`, drawn with a code on each mark on screen
/// while the codes are `up` — or the ring round the one it is on;
/// `ask_large`, whether Ask is large over the window, leaving nothing under
/// it to press; `tab`, whether the window's Tab is the ring's (not while
/// the tutorial holds the keys, a shortcut is being typed for a command,
/// or jump mode's codes are up); `top`, what is on top of it — what a
/// press reaches, a dialog with no control of its own as well as one with.
pub(crate) fn root<'a>(
    view: Element<'a, Message>,
    up: Option<Up<'_>>,
    ask_large: bool,
    window: Option<window::Id>,
    tab: bool,
    top: OnTop,
) -> Element<'a, Message> {
    if up.is_none() {
        forget(window);
    }
    // Marks built from here on — as the window is laid out — are numbered
    // past the view's.
    BUILD.with(|b| {
        let mut b = b.borrow_mut();
        b.next = b.next.max(LAID_OUT_MARKS);
    });
    let counts = BUILD.with(|b| b.borrow().marks);
    let OnTop { layer: top, scene } = top;
    let reach = Reach::of(top, ask_large);
    let marks = match reach.only {
        Some(only) => counts[only as usize],
        None => counts.iter().sum(),
    };
    let (typed, ring) = match up {
        Some(Up::Codes(typed)) => (Some(typed.to_string()), None),
        Some(Up::Ring(ask)) => (None, Some(ask)),
        None => (None, None),
    };
    Element::new(Root {
        content: view,
        window,
        typed,
        ring,
        tab,
        top,
        scene,
        reach,
        len: code_len(marks),
    })
}

/// What pressing the control `window`'s ring is on sends, as its click
/// sends; nothing while it is on a field (which holds the keys) or not up.
pub(crate) fn ring_press(window: Option<window::Id>) -> Option<Message> {
    PLACED.with(|p| {
        p.borrow()
            .get(&window)
            .and_then(|placed| placed.press.clone())
    })
}

/// The field that held the keys in `window` when its ring came up from it.
pub(crate) fn ring_from(window: Option<window::Id>) -> Option<iced::widget::Id> {
    PLACED.with(|p| {
        p.borrow()
            .get(&window)
            .and_then(|placed| placed.from.clone())
    })
}

/// Which stop `window`'s ring is on.
#[cfg(test)]
pub(crate) fn ring_target(window: Option<window::Id>) -> Option<ring::StopKind> {
    PLACED.with(|p| p.borrow().get(&window).and_then(|placed| placed.target))
}

/// Where `window`'s ring is: its place among the stops, how many there
/// are, and what pressing it sends.
#[cfg(test)]
pub(crate) fn ring_placed(
    window: Option<window::Id>,
) -> Option<(Option<usize>, usize, Option<Message>)> {
    PLACED.with(|p| {
        p.borrow()
            .get(&window)
            .map(|placed| (placed.at, placed.count, placed.press.clone()))
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
    PLACED.with(|p| p.borrow_mut().remove(&window));
}

/// The codes dealt when `window` was last drawn, in the order drawn.
#[cfg(test)]
pub(crate) fn dealt(window: Option<window::Id>) -> Vec<(String, Message)> {
    DEALT.with(|d| d.borrow().get(&window).cloned().unwrap_or_default())
}

/// The window, dealing codes to its marks as it is drawn; and the
/// keyboard's ring, placed as it is laid out and moved by its Tab.
struct Root<'a> {
    content: Element<'a, Message>,
    window: Option<window::Id>,
    /// Jump mode's: what has been typed of a code.
    typed: Option<String>,
    /// The ring's: where the window asks it to be ([`Up::Ring`]).
    ring: Option<RingAsk>,
    /// Whether the window's Tab is the ring's.
    tab: bool,
    /// The layer on top, whose marks a press reaches, and the dialog there.
    top: Layer,
    scene: Option<String>,
    reach: Reach,
    len: usize,
}

/// A control marked with its press.
struct Mark<'a> {
    content: Element<'a, Message>,
    message: Message,
    layer: Layer,
    /// The number it was built with.
    id: usize,
    /// What its control is called, as it said when built (while the ring
    /// or the codes are up).
    name: Option<String>,
}

/// A part of the window, said to the ring's operations ([`scope`]).
struct Scope<'a> {
    content: Element<'a, Message>,
    tag: ring::ScopeTag,
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

/// [`transparent!`]'s layout: the content's own.
macro_rules! transparent_layout {
    () => {
        fn layout(
            &mut self,
            tree: &mut Tree,
            renderer: &iced::Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            self.content.as_widget_mut().layout(tree, renderer, limits)
        }
    };
}

/// [`transparent!`]'s drawing: the content's own.
macro_rules! transparent_draw {
    () => {
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

impl Root<'_> {
    /// The ring's stops in the window as laid out (`layout`), in its order.
    fn stops(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
    ) -> Vec<ring::Stop> {
        let mut census = ring::Census::default();
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, &mut census);
        let reach = self.reach;
        ring::order(census.stops, |layer| reach.reaches(layer))
    }

    /// The ring put on `stops[at]`: a field reached takes the keys (and
    /// every other gives them up); a stop scrolled away is scrolled to.
    fn put_on(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        stop: &ring::Stop,
    ) {
        let target = match stop.kind {
            StopKind::Field { index, .. } => Some(index),
            StopKind::Mark(_) => None,
        };
        let mut focus = ring::FocusNth { target, seen: 0 };
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, &mut focus);
        if let Some(mut reveal) = ring::Reveal::of(stop) {
            self.content
                .as_widget_mut()
                .operate(tree, layout, renderer, &mut reveal);
        }
    }

    /// The ring kept at `at` among `stops`, asked `ask`, come up from the
    /// field `from`: the window told where it is when `announce`.
    fn keep(
        &self,
        ask: RingAsk,
        at: Option<usize>,
        stops: &[ring::Stop],
        saved: Vec<(Layer, usize)>,
        from: Option<iced::widget::Id>,
        announce: bool,
    ) {
        let stop = at.and_then(|at| stops.get(at));
        let placed = ring::Placed {
            ask,
            at,
            #[cfg(test)]
            count: stops.len(),
            top: self.top,
            saved,
            target: stop.map(|s| s.kind),
            key: at.and_then(|at| ring::key_at(stops, at)),
            scene: self.scene.clone(),
            from,
            press: stop.and_then(|s| s.press.clone()),
            name: stop.and_then(|s| s.name.clone()),
            announce,
        };
        PLACED.with(|p| p.borrow_mut().insert(self.window, placed));
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Root<'_> {
    transparent!();

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

    /// Laid out, the ring is placed: where the window asks it to be, among
    /// the stops as they are now.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // What is built as the window is laid out — a `responsive`'s
        // controls — is built as the view was: marked while the codes or the
        // ring are up, as this window's are, and numbered past its view's.
        BUILD.with(|b| {
            let mut b = b.borrow_mut();
            b.on = self.typed.is_some() || self.ring.is_some();
            b.next = b.next.max(LAID_OUT_MARKS);
        });
        let node = self.content.as_widget_mut().layout(tree, renderer, limits);
        let at = Layout::new(&node);
        // The codes dealt are as long as tell apart every mark there is,
        // those built as the window was laid out too.
        if self.typed.is_some() {
            let mut census = ring::Census::default();
            self.content
                .as_widget_mut()
                .operate(tree, at, renderer, &mut census);
            let reach = self.reach;
            let marks = census.marks.iter().filter(|l| reach.reaches(**l)).count();
            self.len = code_len(marks);
        }
        let Some(ask) = self.ring else {
            return node;
        };
        let stops = self.stops(tree, at, renderer);
        let last = PLACED.with(|p| p.borrow().get(&self.window).cloned());
        let (place, saved) =
            ring::resolve(ask, &stops, self.top, self.scene.as_deref(), last.as_ref());
        let key = place.and_then(|i| ring::key_at(&stops, i));
        // Moved: on another stop, or asked anew (a first Tab). Asked to keep
        // it where it is, it has not: a field the pointer focused since keeps
        // the keys.
        let moved = last
            .as_ref()
            .is_none_or(|l| l.key != key || (ask != RingAsk::Kept && l.ask != ask));
        if moved && let Some(stop) = place.and_then(|i| stops.get(i)).cloned() {
            self.put_on(tree, at, renderer, &stop);
        }
        // Come up from a field: the keys go back to it when the ring is put
        // away by a key.
        let from = match ask {
            RingAsk::AfterFocused { .. } if last.as_ref().is_none_or(|l| l.ask != ask) => {
                ring::focused(&stops).and_then(|f| stops[f].id.clone())
            }
            _ => last.as_ref().and_then(|l| l.from.clone()),
        };
        let announce = moved || last.is_some_and(|l| l.announce);
        self.keep(ask, place, &stops, saved, from, announce);
        node
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
        use iced::keyboard::{self, key::Named};
        // Put away by this event: not said to be anywhere after.
        let mut ended = false;
        match event {
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if self.tab && !(modifiers.command() || modifiers.control() || modifiers.alt()) =>
            {
                let step = match key.as_ref() {
                    keyboard::Key::Named(Named::Tab) => Some(!modifiers.shift()),
                    // The arrows move the ring too, while no field holds
                    // the keys (they are the field's then).
                    keyboard::Key::Named(Named::ArrowDown | Named::ArrowRight) => Some(true),
                    keyboard::Key::Named(Named::ArrowUp | Named::ArrowLeft) => Some(false),
                    _ => None,
                };
                let is_tab = matches!(key.as_ref(), keyboard::Key::Named(Named::Tab));
                match (self.ring, step) {
                    // The first Tab: the window puts the ring up — after the
                    // field holding the keys, if one does.
                    (None, Some(forward)) if is_tab => {
                        let stops = self.stops(tree, layout, renderer);
                        let field = ring::focused(&stops).is_some();
                        shell.publish(Message::Window(crate::WindowMsg::TabPressed {
                            forward,
                            field,
                        }));
                        shell.capture_event();
                        return;
                    }
                    (Some(ask), step) => {
                        let stops = self.stops(tree, layout, renderer);
                        let held = ring::focused(&stops);
                        match step {
                            Some(forward) if is_tab || held.is_none() => {
                                let last = PLACED.with(|p| p.borrow().get(&self.window).cloned());
                                if !stops.is_empty() {
                                    let from = held.or(last.as_ref().and_then(|l| l.at));
                                    let to = from
                                        .map_or(if forward { 0 } else { stops.len() - 1 }, |f| {
                                            ring::step(f, forward, stops.len())
                                        });
                                    let stop = stops[to].clone();
                                    self.put_on(tree, layout, renderer, &stop);
                                    let (saved, from) =
                                        last.map(|l| (l.saved, l.from)).unwrap_or_default();
                                    self.keep(RingAsk::Kept, Some(to), &stops, saved, from, true);
                                    let _ = ask;
                                }
                                shell.capture_event();
                                shell.request_redraw();
                            }
                            // A field holds the keys: what is typed is its,
                            // and the ring is put away. Not Esc: that puts the
                            // ring away and does nothing else, wherever the
                            // ring is (the window's key handler, which the
                            // field passes it on to).
                            _ if held.is_some()
                                && !matches!(key.as_ref(), keyboard::Key::Named(Named::Escape)) =>
                            {
                                shell.publish(Message::Window(crate::WindowMsg::RingEnd));
                                ended = true;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // A press of the pointer puts the ring away: the pointer has
            // the window now.
            Event::Mouse(mouse::Event::ButtonPressed(_)) if self.ring.is_some() => {
                shell.publish(Message::Window(crate::WindowMsg::RingEnd));
                ended = true;
            }
            _ => {}
        }
        // The window told where the ring is, once it is put somewhere new.
        let told = PLACED.with(|p| {
            let mut p = p.borrow_mut();
            let placed = p.get_mut(&self.window)?;
            if !std::mem::take(&mut placed.announce) {
                return None;
            }
            Some((placed.press.clone(), placed.name.clone()))
        });
        if self.ring.is_some()
            && !ended
            && let Some((press, name)) = told
        {
            shell.publish(Message::Window(crate::WindowMsg::RingMoved(
                press.map(Box::new),
                name,
            )));
        }
        if shell.is_event_captured() {
            return;
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
        // The mark the ring is on, for it to draw the ring round itself.
        let ringed = self.ring.and_then(|_| {
            PLACED.with(|p| {
                p.borrow()
                    .get(&self.window)
                    .and_then(|placed| match placed.target {
                        Some(StopKind::Mark(id)) => Some(id),
                        _ => None,
                    })
            })
        });
        RINGED.with(|r| r.set(ringed));
        if let Some(typed) = &self.typed {
            DEALING.with(|d| {
                *d.borrow_mut() = Some(Dealing {
                    typed: typed.clone(),
                    reach: self.reach,
                    len: self.len,
                    dealt: Vec::new(),
                });
            });
        }
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
        RINGED.with(|r| r.set(None));
        if self.typed.is_some() {
            let dealt = DEALING
                .with(|d| d.borrow_mut().take())
                .map(|d| d.dealt)
                .unwrap_or_default();
            DEALT.with(|d| d.borrow_mut().insert(self.window, dealt));
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Scope<'_> {
    transparent!();
    transparent_update!();
    transparent_draw!();

    /// A control built as its layer is laid out (a `responsive`'s) is in
    /// that layer, as one built with the view is.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let ring::ScopeTag::Layer(layer) = self.tag else {
            return self.content.as_widget_mut().layout(tree, renderer, limits);
        };
        let outer = BUILD.with(|b| b.borrow_mut().layer.replace(layer));
        let node = self.content.as_widget_mut().layout(tree, renderer, limits);
        BUILD.with(|b| b.borrow_mut().layer = outer);
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        let bounds = layout.bounds();
        operation.custom(None, bounds, &mut ring::Edge::Begin(self.tag));
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
        operation.custom(None, bounds, &mut ring::Edge::End(self.tag));
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Mark<'_> {
    transparent!();
    transparent_layout!();
    transparent_update!();

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        // Said to the ring's census: a stop.
        operation.custom(
            None,
            layout.bounds(),
            &mut ring::MarkSeen {
                id: self.id,
                layer: self.layer,
                press: self.message.clone(),
                name: self.name.clone(),
            },
        );
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
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
        if RINGED.with(|r| r.get()) == Some(self.id) {
            ring_round(renderer, bounds, *viewport);
        }
        let shown = DEALING.with(|d| {
            let mut d = d.borrow_mut();
            let d = d.as_mut()?;
            if !d.reach.reaches(self.layer) {
                return None;
            }
            // Past the last code, a mark goes without (see `code_len`).
            if d.dealt.len() >= codes_of(d.len) {
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

/// How far the ring stands out round its control.
const RING_OUT: f32 = 2.0;

/// The ring round `at`, the control the keyboard is on: the accent edge a
/// field holding the keys has, a little outside the control, within
/// `room` (what of the window is seen).
fn ring_round(renderer: &mut iced::Renderer, at: Rectangle, room: Rectangle) {
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
