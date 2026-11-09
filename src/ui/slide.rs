//! Content that slides to where it is laid out (see [`slide`]).

use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};

/// How long a slide takes: the tour's card going to the next step's place,
/// and its spotlight with it.
pub(crate) const GLIDE: Duration = Duration::from_millis(320);

/// `t` of the way through a glide (0 to 1), eased: quick to leave, slow to
/// settle — a cubic ease-out.
pub(crate) fn eased(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// `content`, drawn sliding from where it was to where it is laid out
/// whenever its place changes, over [`GLIDE`]: the tour's card, which each
/// step places beside what it is about, went there in one jump.
///
/// Only the drawing moves: it is where it is laid out for the pointer at
/// once. Its first place it simply appears at.
pub(crate) fn slide<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Slide {
        content: content.into(),
    })
}

struct Slide<'a, Message> {
    content: Element<'a, Message>,
}

/// Where the content set out from and is going, when it set out, and the
/// frame being drawn.
#[derive(Debug, Default)]
struct State {
    from: Option<Point>,
    to: Option<Point>,
    since: Option<Instant>,
    now: Option<Instant>,
}

impl State {
    /// How far along at `now` (eased); 1 once there.
    fn progress(&self, now: Instant) -> f32 {
        self.since.map_or(1.0, |since| {
            eased(now.duration_since(since).as_secs_f32() / GLIDE.as_secs_f32())
        })
    }

    /// Where it is drawn at `now`.
    fn at(&self, now: Instant) -> Option<Point> {
        let to = self.to?;
        let Some(from) = self.from else {
            return Some(to);
        };
        let t = self.progress(now);
        Some(Point::new(
            from.x + (to.x - from.x) * t,
            from.y + (to.y - from.y) * t,
        ))
    }

    /// A frame at `now`, the content laid out at `place`: a new place sets
    /// out from where it was drawn last. Whether another frame is wanted.
    fn frame(&mut self, place: Point, now: Instant) -> bool {
        if self.to != Some(place) {
            // Its first place it is simply at.
            let first = self.to.is_none();
            self.from = self.now.and_then(|then| self.at(then));
            self.to = Some(place);
            self.since = (!first).then_some(now);
        }
        self.now = Some(now);
        self.progress(now) < 1.0
    }

    /// How far from its place it is drawn: nothing until a frame has seen
    /// that place.
    fn offset(&self, place: Point) -> Vector {
        match (self.now, self.to == Some(place)) {
            (Some(now), true) => self.at(now).map_or(Vector::ZERO, |at| at - place),
            _ => Vector::ZERO,
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Slide<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
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
        use iced::advanced::Renderer as _;
        let offset = tree.state.downcast_ref::<State>().offset(layout.position());
        renderer.with_translation(offset, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
        });
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
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        if let Event::Window(iced::window::Event::RedrawRequested(now)) = event
            && tree
                .state
                .downcast_mut::<State>()
                .frame(layout.position(), *now)
        {
            shell.request_redraw();
        }
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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slide eases from where the content was to its new place, and
    /// stops asking for frames once there; its first place it is simply
    /// at; a place changed mid-slide sets out from where it is drawn.
    #[test]
    fn the_content_slides_to_its_new_place_and_settles() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let a = Point::new(0.0, 0.0);
        let b = Point::new(100.0, 40.0);
        let mut state = State::default();
        assert!(!state.frame(a, ms(0)), "its first place: no slide");
        assert_eq!(state.offset(a), Vector::ZERO);

        assert!(state.frame(b, ms(16)), "moved: frames asked for");
        assert_eq!(state.offset(b), a - b, "drawn where it was");
        assert!(state.frame(b, ms(16 + 160)));
        let half = state.offset(b);
        assert!(
            half.x > -50.0 && half.x < 0.0,
            "past halfway, eased: {half:?}"
        );
        assert!(!state.frame(b, ms(16 + 400)), "settled: no more frames");
        assert_eq!(state.offset(b), Vector::ZERO);

        // Turned round halfway: from where it is drawn, not from b.
        assert!(state.frame(a, ms(1000)));
        assert!(state.frame(a, ms(1100)));
        let drawn = a + state.offset(a);
        assert!(state.frame(b, ms(1100)));
        assert_eq!(b + state.offset(b), drawn, "no jump");
    }

    /// Eased: 0 at the start, 1 at the end, faster early than late.
    #[test]
    fn the_ease_leaves_quickly_and_settles_slowly() {
        assert_eq!(eased(0.0), 0.0);
        assert_eq!(eased(1.0), 1.0);
        assert_eq!(eased(2.0), 1.0);
        assert!(eased(0.25) > 0.25 && eased(0.5) > 0.5);
        assert!(eased(0.5) - eased(0.25) > eased(0.75) - eased(0.5));
    }
}
