//! A tooltip that waits: shown once the pointer has rested on what it names
//! for [`DELAY`], not the moment it passes over — a pointer crossing the
//! tool strip on its way somewhere lit a bubble at every control it
//! touched. Once one is shown, the next is shown at once (until the pointer
//! has been off them all for [`WARM`]), so reading along a row of controls
//! is as quick as before. A click puts it away until the pointer leaves:
//! what was named has been chosen — unless it is [`Tip::sticky`], an ⓘ
//! whose words are what it is for: a click keeps those up until the next
//! click, on it or anywhere, or Esc.

use std::cell::Cell;
use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::widget::tooltip::Position;
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector, window};

/// How long the pointer rests on a control before its tooltip shows.
pub(crate) const DELAY: Duration = Duration::from_millis(500);
/// How long after a tooltip was put away the next one shows at once.
pub(crate) const WARM: Duration = Duration::from_millis(800);

thread_local! {
    /// Until when a tooltip shows at once: a moment past the last one
    /// that was showing.
    static WARM_UNTIL: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Whether a tooltip met while a control's content was updated is under
    /// the pointer: each says so as it is updated, for the one round it.
    static INNER_OVER: Cell<bool> = const { Cell::new(false) };
}

fn warm(now: Instant) -> bool {
    WARM_UNTIL.with(|w| w.get().is_some_and(|until| now < until))
}

fn keep_warm(now: Instant) {
    WARM_UNTIL.with(|w| w.set(Some(now + WARM)));
}

/// As though a tooltip had just been put away: the next shows at once —
/// for a test that reads one without waiting out [`DELAY`].
#[cfg(test)]
pub(crate) fn warm_now() {
    keep_warm(Instant::now());
}

/// `content`, naming itself with `tip` on its `position` side once the
/// pointer has rested on it.
pub(crate) fn tip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    tip: impl Into<Element<'a, Message>>,
    position: Position,
) -> Tip<'a, Message> {
    Tip {
        content: content.into(),
        tip: tip.into(),
        position,
        gap: 0.0,
        sticky: false,
    }
}

pub(crate) struct Tip<'a, Message> {
    content: Element<'a, Message>,
    tip: Element<'a, Message>,
    position: Position,
    gap: f32,
    sticky: bool,
}

impl<Message> Tip<'_, Message> {
    /// The room between what is named and its tooltip.
    pub(crate) fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }

    /// A click shows it and keeps it up — until the next click, on it or
    /// anywhere else, or Esc — rather than putting it away: for an ⓘ, whose
    /// words are what it is clicked for.
    pub(crate) fn sticky(mut self) -> Self {
        self.sticky = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum State {
    #[default]
    Idle,
    /// The pointer came to rest on it `at`: shown once it has stayed.
    Resting {
        at: Instant,
    },
    Shown,
    /// Clicked: not shown again until the pointer has left.
    PutAway,
    /// A sticky one clicked: shown until the next click or Esc.
    Pinned,
}

impl<Message> Widget<Message, Theme, Renderer> for Tip<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content), Tree::new(&self.tip)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(&[self.content.as_widget(), self.tip.as_widget()]);
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
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
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // What it names first, and whether a tooltip in it is under the
        // pointer: the innermost names what the pointer is on. A tab's ×
        // showed its "Close" and, over it, the tab's own name at once.
        let before = INNER_OVER.replace(false);
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let inner = INNER_OVER.get();
        let on = cursor.position_over(layout.bounds()).is_some();
        INNER_OVER.set(before || inner || on);
        let escape = matches!(
            event,
            Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape),
                ..
            })
        );
        if matches!(
            event,
            Event::Mouse(_) | Event::Window(window::Event::RedrawRequested(_))
        ) || (escape && self.sticky)
        {
            let state = tree.state.downcast_mut::<State>();
            let now = Instant::now();
            let moved = matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. }));
            let over = rests(*state, on && !inner, moved);
            // A press on it puts its tip away whether or not the pointer
            // rested there first: a control come up under a still pointer,
            // clicked, showed its tip at the first nudge after the click.
            let press = matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)));
            let pressed = on && press;
            let next = if self.sticky {
                pin(*state, over, pressed, (press && !on) || escape, now)
            } else {
                step(*state, over, pressed, now)
            };
            if next != *state {
                // Shown or put away: the overlay comes or goes.
                let up = |s: State| matches!(s, State::Shown | State::Pinned);
                if up(next) || up(*state) {
                    shell.invalidate_layout();
                    shell.request_redraw();
                }
                *state = next;
            }
            if let State::Resting { at } = *state {
                shell.request_redraw_at(at + DELAY);
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let shown = matches!(
            tree.state.downcast_ref::<State>(),
            State::Shown | State::Pinned
        );
        let mut children = tree.children.iter_mut();
        let content = self.content.as_widget_mut().overlay(
            children.next()?,
            layout,
            renderer,
            viewport,
            translation,
        );
        let bubble = shown.then(|| {
            overlay::Element::new(Box::new(Bubble {
                at: layout.position() + translation,
                named: layout.bounds(),
                tip: &mut self.tip,
                tree: children.next().expect("the tooltip's tree"),
                position: self.position,
                gap: self.gap,
            }))
        });
        if content.is_none() && bubble.is_none() {
            return None;
        }
        Some(overlay::Group::with_children(content.into_iter().chain(bubble).collect()).overlay())
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

/// Whether the pointer, `over` what a tooltip names, is resting there for
/// it: one that moved onto it, or is still where it was. One that never
/// moved, a control come up under it — the Ask card's mic under the pill
/// just pressed — is not: its tip came up over the suggestions under it.
fn rests(state: State, over: bool, moved: bool) -> bool {
    over && (moved || !matches!(state, State::Idle))
}

/// Where a tooltip goes from `state`, the pointer `over` what it names or
/// not (resting there, `rests`), a button `pressed` on it, at `now`.
fn step(state: State, over: bool, pressed: bool, now: Instant) -> State {
    if pressed {
        return State::PutAway;
    }
    match (state, over) {
        (State::Idle, true) if warm(now) => State::Shown,
        (State::Idle, true) => State::Resting { at: now },
        (State::Resting { at }, true) if now.duration_since(at) >= DELAY => State::Shown,
        (State::Shown, true) => {
            keep_warm(now);
            State::Shown
        }
        (State::Shown, false) => {
            keep_warm(now);
            State::Idle
        }
        (_, false) => State::Idle,
        (state, true) => state,
    }
}

/// Where a sticky tooltip goes from `state`: a click on it shows it, kept
/// up, or puts away the one it kept up; a click elsewhere or Esc
/// (`dismissed`) puts that away; resting on it shows it as any other does.
fn pin(state: State, over: bool, pressed: bool, dismissed: bool, now: Instant) -> State {
    match (state, pressed) {
        (State::Pinned, true) => State::PutAway,
        (_, true) => State::Pinned,
        (State::Pinned, false) if dismissed => State::Idle,
        (State::Pinned, false) => State::Pinned,
        (state, false) => step(state, over, false, now),
    }
}

impl<'a, Message: 'a> From<Tip<'a, Message>> for Element<'a, Message> {
    fn from(tip: Tip<'a, Message>) -> Self {
        Element::new(tip)
    }
}

/// A shown tooltip, beside what it names and kept inside the window.
struct Bubble<'a, 'b, Message> {
    at: Point,
    named: Rectangle,
    tip: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    position: Position,
    gap: f32,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for Bubble<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let laid = self.tip.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = laid.bounds().size();
        let (named, at, gap) = (self.named, self.at, self.gap);
        let across = at.x + (named.width - size.width) / 2.0;
        let down = at.y + (named.height - size.height) / 2.0;
        let (x, y) = match self.position {
            Position::Top => (across, at.y - size.height - gap),
            Position::Bottom => (across, at.y + named.height + gap),
            Position::Left => (at.x - size.width - gap, down),
            Position::Right | Position::FollowCursor => (at.x + named.width + gap, down),
        };
        // Inside the window, whichever side it was asked for.
        let x = x.clamp(0.0, (bounds.width - size.width).max(0.0));
        let y = y.clamp(0.0, (bounds.height - size.height).max(0.0));
        laid.move_to(Point::new(x, y))
    }

    /// Its words are found as any others on screen are (by an operation —
    /// a test reading what a tip says, a screen reader).
    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.tip
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.tip.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &Rectangle::with_size(Size::INFINITE),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cool() {
        WARM_UNTIL.with(|w| w.set(None));
    }

    /// A tooltip waits for the pointer to rest: passing over, nothing; held
    /// there for the delay, shown.
    #[test]
    fn a_tooltip_waits_for_the_pointer_to_rest() {
        cool();
        let t0 = Instant::now();
        let s = step(State::Idle, true, false, t0);
        assert_eq!(s, State::Resting { at: t0 });
        let s = step(s, true, false, t0 + DELAY / 2);
        assert_eq!(s, State::Resting { at: t0 });
        assert_eq!(step(s, false, false, t0 + DELAY / 2), State::Idle);
        assert_eq!(step(s, true, false, t0 + DELAY), State::Shown);
    }

    /// An ⓘ's words: a click shows them at once and keeps them up as the
    /// pointer goes, until the next click — on it, which puts them away
    /// until the pointer leaves, or anywhere — or Esc; resting on it shows
    /// them as any tooltip.
    #[test]
    fn a_sticky_tip_is_kept_up_by_a_click() {
        cool();
        let t0 = Instant::now();
        let pinned = pin(State::Idle, true, true, false, t0);
        assert_eq!(pinned, State::Pinned);
        assert_eq!(
            pin(pinned, false, false, false, t0),
            State::Pinned,
            "the pointer gone"
        );
        assert_eq!(
            pin(pinned, false, false, true, t0),
            State::Idle,
            "a click elsewhere, or Esc"
        );
        let put_away = pin(pinned, true, true, false, t0);
        assert_eq!(put_away, State::PutAway, "clicked again");
        assert_eq!(
            pin(put_away, true, false, false, t0 + DELAY),
            State::PutAway
        );
        assert_eq!(
            pin(State::Idle, true, false, false, t0),
            State::Resting { at: t0 }
        );
    }

    /// Once one is shown, the next is shown at once — until the pointer has
    /// been off them for a while.
    #[test]
    fn the_next_tooltip_shows_at_once() {
        cool();
        let t0 = Instant::now();
        let shown = step(State::Resting { at: t0 }, true, false, t0 + DELAY);
        assert_eq!(shown, State::Shown);
        let left = t0 + DELAY + Duration::from_millis(10);
        assert_eq!(step(shown, false, false, left), State::Idle);
        // Its neighbour, a moment later: at once.
        assert_eq!(
            step(State::Idle, true, false, left + WARM / 2),
            State::Shown
        );
        // Long after: it waits again.
        cool();
        assert!(matches!(
            step(State::Idle, true, false, left + WARM * 2),
            State::Resting { .. }
        ));
    }

    /// L13: a control come up under a pointer that has not moved is not
    /// rested on — its tip waits for the pointer to move onto it.
    #[test]
    fn a_control_come_up_under_a_still_pointer_shows_no_tip() {
        assert!(!rests(State::Idle, true, false), "came up under it");
        assert!(rests(State::Idle, true, true), "moved onto it");
        let t0 = Instant::now();
        assert!(
            rests(State::Resting { at: t0 }, true, false),
            "still resting"
        );
        assert!(rests(State::Shown, true, false), "still shown");
        assert!(!rests(State::Shown, false, true), "left it");
    }

    /// A click puts it away, and it stays away while the pointer stays.
    #[test]
    fn a_click_puts_it_away_until_the_pointer_leaves() {
        cool();
        let t0 = Instant::now();
        let s = step(State::Shown, true, true, t0);
        assert_eq!(s, State::PutAway);
        assert_eq!(step(s, true, false, t0 + DELAY * 4), State::PutAway);
        assert_eq!(step(s, false, false, t0 + DELAY * 4), State::Idle);
        // Pressed on a control come up under a still pointer, not rested on
        // (`rests` is false): put away all the same, and the next nudge
        // shows nothing.
        assert!(!rests(State::Idle, true, false));
        let s = step(State::Idle, false, true, t0);
        assert_eq!(s, State::PutAway);
        assert!(rests(s, true, true));
        assert_eq!(step(s, true, false, t0 + DELAY * 4), State::PutAway);
    }
}
