//! A popup placed at a point in the window — the hover peek and the context
//! menu — positioned from its *measured* size, so it can flip to the other
//! side of the point and stay inside the window whatever its content.
//!
//! The previous placement padded a full-window container by the anchor point,
//! which cannot know the popup's size: a peek near the bottom edge ran off the
//! window, and the context menu flipped on a hand-counted height estimate.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};

use crate::Message;

/// How a popup relates to its anchor point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Anchoring {
    /// A tooltip: `gap` below the point, flipped above it when there is no
    /// room below; slid sideways (never flipped) to stay in the window.
    Below { gap: f32 },
    /// A menu: its corner at the point, opening down-right and flipping up
    /// and/or left when it would cross the bottom / right edge.
    Corner,
}

/// Where a popup of `size` goes for an anchor at `anchor` in a window of
/// `window` size, keeping `margin` clear of every edge when it fits. Returns
/// the popup's top-left corner.
pub(crate) fn place_popup(
    anchor: Point,
    size: Size,
    window: Size,
    anchoring: Anchoring,
    margin: f32,
) -> Point {
    // The farthest a popup's top-left may sit and still end `margin` inside
    // the window; a popup larger than the window pins to the near margin.
    let max_x = (window.width - margin - size.width).max(margin);
    let max_y = (window.height - margin - size.height).max(margin);
    let (x, y) = match anchoring {
        Anchoring::Below { gap } => {
            let below = anchor.y + gap;
            let above = anchor.y - gap - size.height;
            let y = if below + size.height <= window.height - margin || above < margin {
                below
            } else {
                above
            };
            (anchor.x, y)
        }
        Anchoring::Corner => {
            let x = if anchor.x + size.width > window.width - margin {
                anchor.x - size.width
            } else {
                anchor.x
            };
            let y = if anchor.y + size.height > window.height - margin {
                anchor.y - size.height
            } else {
                anchor.y
            };
            (x, y)
        }
    };
    Point::new(x.clamp(margin, max_x), y.clamp(margin, max_y))
}

/// Lays `content` out at its natural size (at most the window minus margins)
/// and places it with [`place_popup`]. Fills the window, but everywhere outside
/// the content it is transparent to input: events, hover and the cursor shape
/// all belong to whatever lies underneath.
pub(crate) struct Anchored<'a> {
    content: Element<'a, Message>,
    anchor: Point,
    anchoring: Anchoring,
    margin: f32,
}

impl<'a> Anchored<'a> {
    pub(crate) fn new(
        content: impl Into<Element<'a, Message>>,
        anchor: Point,
        anchoring: Anchoring,
    ) -> Self {
        Self {
            content: content.into(),
            anchor,
            anchoring,
            margin: 8.0,
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Anchored<'_> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
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
        let window = limits.max();
        let room = Size::new(
            (window.width - 2.0 * self.margin).max(0.0),
            (window.height - 2.0 * self.margin).max(0.0),
        );
        let child = self.content.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(Size::ZERO, room),
        );
        let at = place_popup(
            self.anchor,
            child.size(),
            window,
            self.anchoring,
            self.margin,
        );
        layout::Node::with_children(window, vec![child.move_to(at)])
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
        if let Some(child) = layout.children().next() {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                child,
                cursor,
                viewport,
            );
        }
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        if let Some(child) = layout.children().next() {
            operation.container(None, layout.bounds());
            operation.traverse(&mut |operation| {
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    child,
                    renderer,
                    operation,
                );
            });
        }
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
        if let Some(child) = layout.children().next() {
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                event,
                child,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        layout
            .children()
            .next()
            .map_or(mouse::Interaction::None, |child| {
                self.content.as_widget().mouse_interaction(
                    &tree.children[0],
                    child,
                    cursor,
                    viewport,
                    renderer,
                )
            })
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        let child = layout.children().next()?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            child,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<Anchored<'a>> for Element<'a, Message> {
    fn from(anchored: Anchored<'a>) -> Self {
        Element::new(anchored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Size = Size {
        width: 1000.0,
        height: 800.0,
    };
    const PEEK: Anchoring = Anchoring::Below { gap: 10.0 };

    #[test]
    fn a_peek_sits_below_the_pointer_when_it_fits() {
        let at = place_popup(
            Point::new(200.0, 300.0),
            Size::new(300.0, 120.0),
            WINDOW,
            PEEK,
            8.0,
        );
        assert_eq!(at, Point::new(200.0, 310.0));
    }

    #[test]
    fn a_peek_near_the_bottom_flips_above_the_pointer() {
        let at = place_popup(
            Point::new(200.0, 740.0),
            Size::new(300.0, 120.0),
            WINDOW,
            PEEK,
            8.0,
        );
        // Its bottom edge ends `gap` above the pointer instead of running off
        // the window.
        assert_eq!(at, Point::new(200.0, 740.0 - 10.0 - 120.0));
    }

    #[test]
    fn a_peek_near_the_right_edge_slides_left_to_stay_visible() {
        let at = place_popup(
            Point::new(900.0, 100.0),
            Size::new(300.0, 120.0),
            WINDOW,
            PEEK,
            8.0,
        );
        assert_eq!(at.x, 1000.0 - 8.0 - 300.0);
        assert_eq!(at.y, 110.0);
    }

    #[test]
    fn a_peek_too_tall_for_either_side_is_clamped_into_the_window() {
        // No room below (pointer low) and none above (popup taller than the
        // space): it stays on the preferred side, clamped to the margin.
        let at = place_popup(
            Point::new(10.0, 600.0),
            Size::new(200.0, 700.0),
            WINDOW,
            PEEK,
            8.0,
        );
        assert!(at.y >= 8.0 && at.y + 700.0 <= 800.0 - 8.0 + 0.001, "{at:?}");
        assert_eq!(at.x, 10.0);
    }

    #[test]
    fn a_menu_opens_down_right_and_flips_at_each_edge() {
        let menu = Size::new(210.0, 320.0);
        assert_eq!(
            place_popup(
                Point::new(100.0, 100.0),
                menu,
                WINDOW,
                Anchoring::Corner,
                8.0
            ),
            Point::new(100.0, 100.0)
        );
        // Bottom edge: opens upward from the click.
        assert_eq!(
            place_popup(
                Point::new(100.0, 700.0),
                menu,
                WINDOW,
                Anchoring::Corner,
                8.0
            ),
            Point::new(100.0, 380.0)
        );
        // Right edge: opens leftward.
        assert_eq!(
            place_popup(
                Point::new(950.0, 100.0),
                menu,
                WINDOW,
                Anchoring::Corner,
                8.0
            ),
            Point::new(740.0, 100.0)
        );
        // Both at once: the corner.
        assert_eq!(
            place_popup(
                Point::new(950.0, 700.0),
                menu,
                WINDOW,
                Anchoring::Corner,
                8.0
            ),
            Point::new(740.0, 380.0)
        );
    }

    #[test]
    fn a_popup_larger_than_the_window_pins_to_the_top_left_margin() {
        let at = place_popup(
            Point::new(500.0, 400.0),
            Size::new(2000.0, 2000.0),
            WINDOW,
            Anchoring::Corner,
            8.0,
        );
        assert_eq!(at, Point::new(8.0, 8.0));
    }
}
