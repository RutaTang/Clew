//! The keyboard's ring: with no field holding the keys, Tab and Shift-Tab
//! go from control to control of the window — its buttons and rows (the
//! marks jump mode puts its codes on, [`super::jump`]), its fields, and the
//! Files view's tree as one stop — in the order they are laid out, the
//! window's own title bar last. A field reached takes the keys, as a click
//! on it gives them, and Tab goes on from it; a control reached is ringed,
//! and Enter or Space presses it. A control scrolled away is scrolled to.
//!
//! The stops are read off the window as it is laid out ([`Census`]), all of
//! them — those scrolled out of sight too, which drawing never visits — of
//! the layer on top: a dialog's own, while one is up.

use iced::advanced::widget::Operation;
use iced::advanced::widget::operation::Focusable;
use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, Scrollable};
use iced::widget::Id;
use iced::{Rectangle, Vector};

use super::heard::Layer;

/// What a mark says of itself to an operation: which mark it is, by the
/// number it was built with, its layer, and what pressing it sends.
pub(crate) struct MarkSeen {
    pub(crate) id: usize,
    pub(crate) layer: Layer,
    pub(crate) press: crate::Message,
}

/// A part of the window a stop's place is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeTag {
    /// One of the window's layers ([`Layer`]).
    Layer(Layer),
    /// The title bar: its stops come last.
    Chrome,
    /// The Files view's tree: one stop, its rows walked by its own keys.
    Tree,
}

/// Where a part begins and ends, said to an operation.
pub(crate) enum Edge {
    Begin(ScopeTag),
    End(ScopeTag),
}

/// Where the window asks the ring to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RingAsk {
    /// On the first stop (`forward`) or the last: the first Tab.
    Start { forward: bool },
    /// On the stop after (or before) the field holding the keys: Tab from a
    /// field the pointer gave the keys to.
    AfterFocused { forward: bool },
    /// Where it was put last.
    Kept,
}

/// What a stop is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopKind {
    /// A mark, by the number it was built with.
    Mark(usize),
    /// A field — or the tree — by its place among the window's focusable
    /// widgets, and whether it holds the keys now.
    Field { index: usize, focused: bool },
}

/// A scrollable a stop is in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Scroll {
    /// Its place among the window's scrollables.
    index: usize,
    bounds: Rectangle,
    content: Rectangle,
    translation: Vector,
}

/// One place the ring can be.
#[derive(Debug, Clone)]
pub(crate) struct Stop {
    pub(crate) kind: StopKind,
    pub(crate) layer: Layer,
    chrome: bool,
    bounds: Rectangle,
    scroll: Option<Scroll>,
    /// What pressing it sends: a mark's own press — none for a field,
    /// which holds the keys.
    pub(crate) press: Option<crate::Message>,
}

impl Stop {
    fn focused(&self) -> bool {
        matches!(self.kind, StopKind::Field { focused: true, .. })
    }
}

/// The operation that reads the window's stops, in the order it is laid
/// out.
#[derive(Default)]
pub(crate) struct Census {
    pub(crate) stops: Vec<Stop>,
    layers: Vec<Layer>,
    chrome: usize,
    tree: usize,
    focusables: usize,
    scrollables: usize,
    pending: Option<Scroll>,
    stack: Vec<Scroll>,
}

impl Census {
    fn push(
        &mut self,
        kind: StopKind,
        layer: Layer,
        bounds: Rectangle,
        press: Option<crate::Message>,
    ) {
        self.stops.push(Stop {
            kind,
            layer,
            chrome: self.chrome > 0,
            bounds,
            scroll: self.stack.last().copied(),
            press,
        });
    }
}

impl Operation for Census {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        // A scrollable says itself, then goes through its content: what is
        // in it is in it.
        let entered = self.pending.take();
        if let Some(scroll) = entered {
            self.stack.push(scroll);
        }
        operate(self);
        if entered.is_some() {
            self.stack.pop();
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        self.pending = Some(Scroll {
            index: self.scrollables,
            bounds,
            content: content_bounds,
            translation,
        });
        self.scrollables += 1;
    }

    fn focusable(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn Focusable) {
        let index = self.focusables;
        self.focusables += 1;
        if self.tree == 0 {
            let layer = self.layers.last().copied().unwrap_or(Layer::Window);
            let focused = state.is_focused();
            self.push(StopKind::Field { index, focused }, layer, bounds, None);
        }
    }

    fn custom(&mut self, _id: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(seen) = state.downcast_ref::<MarkSeen>() {
            // The tree's rows are walked by its own keys: the tree is one
            // stop, its field's.
            if self.tree == 0 {
                self.push(
                    StopKind::Mark(seen.id),
                    seen.layer,
                    bounds,
                    Some(seen.press.clone()),
                );
            }
            return;
        }
        if let Some(edge) = state.downcast_ref::<Edge>() {
            match edge {
                Edge::Begin(ScopeTag::Layer(layer)) => self.layers.push(*layer),
                Edge::End(ScopeTag::Layer(_)) => {
                    self.layers.pop();
                }
                Edge::Begin(ScopeTag::Chrome) => self.chrome += 1,
                Edge::End(ScopeTag::Chrome) => self.chrome = self.chrome.saturating_sub(1),
                Edge::Begin(ScopeTag::Tree) => self.tree += 1,
                Edge::End(ScopeTag::Tree) => self.tree = self.tree.saturating_sub(1),
            }
        }
    }
}

/// The ring's stops of `stops` it `reaches`, in its order: the window's
/// own, then its title bar's — Tab goes first to what is read, and to the
/// window's buttons last.
pub(crate) fn order(stops: Vec<Stop>, reaches: impl Fn(Layer) -> bool) -> Vec<Stop> {
    let (mut body, chrome): (Vec<Stop>, Vec<Stop>) = stops
        .into_iter()
        .filter(|s| reaches(s.layer))
        .partition(|s| !s.chrome);
    body.extend(chrome);
    body
}

/// The place of the stop holding the keys among `stops`.
pub(crate) fn focused(stops: &[Stop]) -> Option<usize> {
    stops.iter().position(Stop::focused)
}

/// The place after (`forward`) or before `from` among `n`, round the ends.
pub(crate) fn step(from: usize, forward: bool, n: usize) -> usize {
    if forward {
        (from + 1) % n
    } else {
        (from + n - 1) % n
    }
}

/// The first place among `n` (`forward`), or the last.
fn start(forward: bool, n: usize) -> usize {
    if forward { 0 } else { n - 1 }
}

/// Where the ring was last placed in a window, and what is kept for a
/// layer under the one on top.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    pub(crate) ask: RingAsk,
    pub(crate) at: Option<usize>,
    /// How many stops there were.
    #[cfg(test)]
    pub(crate) count: usize,
    pub(crate) top: Layer,
    /// Where it was on each layer a layer came up over: where it goes back
    /// to when that layer goes.
    pub(crate) saved: Vec<(Layer, usize)>,
    pub(crate) target: Option<StopKind>,
    /// What pressing the stop it is on sends, for a mark.
    pub(crate) press: Option<crate::Message>,
    /// Whether the window is to be told where it is now.
    pub(crate) announce: bool,
}

/// Where the ring goes among `stops` (in its [`order`]), asked to be at
/// `ask`, `top` the layer on top now, `last` where it was placed before:
/// on the first stop of a layer come up since — back where it was on a
/// layer that layer covered, once it goes — else where it was asked to be.
/// With what is kept for the layers under.
pub(crate) fn resolve(
    ask: RingAsk,
    stops: &[Stop],
    top: Layer,
    last: Option<&Placed>,
) -> (Option<usize>, Vec<(Layer, usize)>) {
    let n = stops.len();
    let mut saved = last.map(|l| l.saved.clone()).unwrap_or_default();
    if n == 0 {
        return (None, saved);
    }
    if let Some(last) = last
        && last.top != top
    {
        if top > last.top {
            if let Some(at) = last.at {
                saved.push((last.top, at));
            }
            return (Some(0), saved);
        }
        let back = saved
            .iter()
            .rposition(|(layer, _)| *layer == top)
            .map(|i| saved.remove(i).1);
        saved.retain(|(layer, _)| *layer < top);
        return (Some(back.unwrap_or(0).min(n - 1)), saved);
    }
    // What was asked is placed once; kept there since.
    if let Some(last) = last
        && last.ask == ask
        && let Some(at) = last.at
    {
        return (Some(at.min(n - 1)), saved);
    }
    let at = match ask {
        RingAsk::Start { forward } => start(forward, n),
        RingAsk::AfterFocused { forward } => {
            focused(stops).map_or_else(|| start(forward, n), |f| step(f, forward, n))
        }
        RingAsk::Kept => last.and_then(|l| l.at).unwrap_or(0).min(n - 1),
    };
    (Some(at), saved)
}

/// The operation that gives the keys to the `target`th focusable widget —
/// none of them, for `None` — taking them from every other.
pub(crate) struct FocusNth {
    pub(crate) target: Option<usize>,
    pub(crate) seen: usize,
}

impl Operation for FocusNth {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(&mut self, _id: Option<&Id>, _bounds: Rectangle, state: &mut dyn Focusable) {
        if self.target == Some(self.seen) {
            state.focus();
        } else {
            state.unfocus();
        }
        self.seen += 1;
    }
}

/// The operation that scrolls the scrollable a stop is in so the stop shows
/// whole, when it does not.
pub(crate) struct Reveal {
    scroll: usize,
    y: f32,
    seen: usize,
}

impl Reveal {
    /// The scroll that brings `stop` into sight, if it is out of it.
    pub(crate) fn of(stop: &Stop) -> Option<Reveal> {
        let scroll = stop.scroll?;
        let top = stop.bounds.y - scroll.content.y;
        let y = super::reveal_offset(
            scroll.translation.y,
            scroll.bounds.height,
            scroll.content.height,
            top,
            top + stop.bounds.height,
        )?;
        Some(Reveal {
            scroll: scroll.index,
            y,
            seen: 0,
        })
    }
}

impl Operation for Reveal {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: Vector,
        state: &mut dyn Scrollable,
    ) {
        if self.seen == self.scroll {
            state.scroll_to(AbsoluteOffset {
                x: None,
                y: Some(self.y),
            });
        }
        self.seen += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(kind: StopKind, layer: Layer, chrome: bool) -> Stop {
        Stop {
            kind,
            layer,
            chrome,
            bounds: Rectangle::default(),
            scroll: None,
            press: None,
        }
    }

    /// The window's own stops come first, its title bar's last; a layer
    /// the ring does not reach has none.
    #[test]
    fn the_title_bar_comes_last_and_unreached_layers_not_at_all() {
        let stops = vec![
            stop(StopKind::Mark(0), Layer::Window, true),
            stop(StopKind::Mark(1), Layer::Window, false),
            stop(StopKind::Mark(2), Layer::Overlay, false),
            stop(StopKind::Mark(3), Layer::Window, false),
        ];
        let all = order(stops.clone(), |_| true);
        let kinds: Vec<_> = all.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [
                StopKind::Mark(1),
                StopKind::Mark(2),
                StopKind::Mark(3),
                StopKind::Mark(0)
            ]
        );
        let dialog = order(stops, |l| l == Layer::Overlay);
        assert_eq!(dialog.len(), 1);
    }

    fn placed(ask: RingAsk, at: usize, top: Layer, saved: Vec<(Layer, usize)>) -> Placed {
        Placed {
            ask,
            at: Some(at),
            count: 9,
            top,
            saved,
            target: None,
            press: None,
            announce: false,
        }
    }

    /// The first Tab rings the first stop, Shift-Tab's the last; from a
    /// field, the one after it; asked the same again, it stays where it
    /// went.
    #[test]
    fn the_ring_goes_where_it_is_asked_once() {
        let stops: Vec<Stop> = (0..5)
            .map(|i| {
                let kind = if i == 2 {
                    StopKind::Field {
                        index: 0,
                        focused: true,
                    }
                } else {
                    StopKind::Mark(i)
                };
                stop(kind, Layer::Window, false)
            })
            .collect();
        let at = |ask, last| resolve(ask, &stops, Layer::Window, last).0;
        assert_eq!(at(RingAsk::Start { forward: true }, None), Some(0));
        assert_eq!(at(RingAsk::Start { forward: false }, None), Some(4));
        assert_eq!(at(RingAsk::AfterFocused { forward: true }, None), Some(3));
        assert_eq!(at(RingAsk::AfterFocused { forward: false }, None), Some(1));
        let moved = placed(RingAsk::Kept, 4, Layer::Window, vec![]);
        assert_eq!(at(RingAsk::Kept, Some(&moved)), Some(4));
        // Asked to start again, it starts again; asked the same, it stays.
        let started = placed(RingAsk::Start { forward: true }, 3, Layer::Window, vec![]);
        assert_eq!(
            at(RingAsk::Start { forward: true }, Some(&started)),
            Some(3)
        );
        assert_eq!(resolve(RingAsk::Kept, &[], Layer::Window, None).0, None);
    }

    /// A dialog come up takes the ring to its first stop; gone, the ring is
    /// back where it was under it.
    #[test]
    fn a_layer_come_and_gone_takes_the_ring_and_gives_it_back() {
        let stops: Vec<Stop> = (0..9)
            .map(|i| stop(StopKind::Mark(i), Layer::Window, false))
            .collect();
        let under = placed(RingAsk::Kept, 6, Layer::Window, vec![]);
        let (at, saved) = resolve(RingAsk::Kept, &stops, Layer::Overlay, Some(&under));
        assert_eq!(at, Some(0));
        assert_eq!(saved, [(Layer::Window, 6)]);
        let over = placed(RingAsk::Kept, 2, Layer::Overlay, saved);
        let (at, saved) = resolve(RingAsk::Kept, &stops, Layer::Window, Some(&over));
        assert_eq!(at, Some(6), "back on what opened it");
        assert!(saved.is_empty());
    }
}
