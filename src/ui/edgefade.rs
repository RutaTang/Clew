//! A horizontal strip that scrolls (the sidebar's tabs) faded at whichever
//! end has more of it out of sight, so a tab cut off at the edge reads as
//! "there is more this way" rather than as a clipped label (see
//! [`edge_fade`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::gradient::Linear;
use iced::{Color, Element, Event, Length, Radians, Rectangle, Size, Vector};

/// Width of a fade.
pub(crate) const FADE_W: f32 = 28.0;

/// `strip` — a horizontal scrollable — faded into `color` (the background
/// it sits on) at an end with content beyond it. `offset` is how far it is
/// scrolled, as its `on_scroll` last said; whether there is more past either
/// end is read from its layout when it is drawn, so a strip widened until
/// everything fits shows no fade, though it says nothing of that.
pub(crate) fn edge_fade<'a, Message: 'a>(
    strip: impl Into<Element<'a, Message>>,
    offset: f32,
    color: Color,
) -> Element<'a, Message> {
    Element::new(EdgeFade {
        content: strip.into(),
        offset,
        color,
    })
}

/// Which ends of a strip `viewport` wide over `content` wide, scrolled
/// `offset` in, have content past them: `(left, right)`.
pub(crate) fn fades(viewport: f32, content: f32, offset: f32) -> (bool, bool) {
    let hidden = content - viewport;
    if hidden <= 0.5 {
        return (false, false);
    }
    let offset = offset.clamp(0.0, hidden);
    (offset > 0.5, offset < hidden - 0.5)
}

struct EdgeFade<'a, Message> {
    content: Element<'a, Message>,
    offset: f32,
    color: Color,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for EdgeFade<'_, Message> {
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let bounds = layout.bounds();
        // The scrollable's one child is its content, laid out at full width.
        let content = layout
            .children()
            .next()
            .map_or(bounds.width, |c| c.bounds().width);
        let (left, right) = fades(bounds.width, content, self.offset);
        let clear = Color {
            a: 0.0,
            ..self.color
        };
        let width = FADE_W.min(bounds.width / 2.0);
        let fade = |renderer: &mut iced::Renderer, x: f32, from: Color, to: Color| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x,
                        y: bounds.y,
                        width,
                        height: bounds.height,
                    },
                    ..renderer::Quad::default()
                },
                // Left to right: `from` at the left edge, `to` at the right.
                Linear::new(Radians(std::f32::consts::FRAC_PI_2))
                    .add_stop(0.0, from)
                    .add_stop(1.0, to),
            );
        };
        if !(left || right) {
            return;
        }
        // A layer of its own: within one, iced draws every quad before any
        // text, so a fade drawn with the tabs would sit under their labels.
        renderer.with_layer(bounds, |renderer| {
            if left {
                fade(renderer, bounds.x, self.color, clear);
            }
            if right {
                fade(renderer, bounds.x + bounds.width - width, clear, self.color);
            }
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

    #[test]
    fn a_strip_that_fits_shows_no_fade() {
        assert_eq!(fades(300.0, 300.0, 0.0), (false, false));
        assert_eq!(fades(300.0, 200.0, 0.0), (false, false));
        // A stale offset from before it was widened changes nothing.
        assert_eq!(fades(300.0, 280.0, 40.0), (false, false));
    }

    #[test]
    fn a_strip_fades_at_each_end_with_more_beyond_it() {
        // At the start: more to the right only.
        assert_eq!(fades(300.0, 500.0, 0.0), (false, true));
        // In the middle: both.
        assert_eq!(fades(300.0, 500.0, 100.0), (true, true));
        // At the end: more to the left only — an offset past it too.
        assert_eq!(fades(300.0, 500.0, 200.0), (true, false));
        assert_eq!(fades(300.0, 500.0, 260.0), (true, false));
    }
}
