//! A menu that drops down from what opens it (see [`dropdown`]), drawn
//! over everything rather than laid out with it.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Tree, Widget};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Renderer, Size, Theme, Vector};

/// `anchor`, with `menu` — while there is one — drawn over the window just
/// under it, `offset` from its bottom-left corner and kept inside the
/// window. Laid out with the rest, a menu made what held it grow to hold
/// it: the Ask card, fitted to its conversation, grew 290 px up the window
/// as its sessions menu opened, and the menu opened far from the pointer.
pub(crate) fn dropdown<'a, Message: 'a>(
    anchor: impl Into<Element<'a, Message>>,
    menu: Option<Element<'a, Message>>,
    offset: Vector,
) -> Dropdown<'a, Message> {
    Dropdown {
        anchor: anchor.into(),
        menu,
        offset,
        dismiss: None,
    }
}

pub(crate) struct Dropdown<'a, Message> {
    anchor: Element<'a, Message>,
    menu: Option<Element<'a, Message>>,
    offset: Vector,
    /// Sent by a press of the pointer away from the menu and what opened
    /// it, while it is open.
    dismiss: Option<Message>,
}

impl<Message> Dropdown<'_, Message> {
    /// Send `message` when the pointer presses away from the open menu
    /// and what opened it.
    pub(crate) fn dismiss(mut self, message: Message) -> Self {
        self.dismiss = Some(message);
        self
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Dropdown<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.anchor.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.anchor.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        std::iter::once(Tree::new(&self.anchor))
            .chain(self.menu.as_ref().map(Tree::new))
            .collect()
    }

    fn diff(&self, tree: &mut Tree) {
        match &self.menu {
            Some(menu) => tree.diff_children(&[&self.anchor, menu]),
            None => tree.diff_children(&[&self.anchor]),
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.anchor
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
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
        self.anchor.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.anchor
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        self.anchor.as_widget_mut().update(
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
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.anchor.as_widget().mouse_interaction(
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
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let (anchor_tree, menu_tree) = match tree.children.as_mut_slice() {
            [anchor, menu, ..] => (anchor, Some(menu)),
            [anchor] => (anchor, None),
            [] => return None,
        };
        let anchors = self.anchor.as_widget_mut().overlay(
            anchor_tree,
            layout,
            renderer,
            viewport,
            translation,
        );
        let anchor = layout.bounds() + translation;
        let offset = self.offset;
        let dismiss = self.dismiss.clone();
        let menu = match (self.menu.as_mut(), menu_tree) {
            (Some(menu), Some(tree)) => Some(overlay::Element::new(Box::new(Dropped {
                anchor,
                offset,
                menu,
                tree,
                dismiss,
            }))),
            _ => None,
        };
        if anchors.is_none() && menu.is_none() {
            return None;
        }
        Some(overlay::Group::with_children(anchors.into_iter().chain(menu).collect()).overlay())
    }
}

impl<'a, Message: Clone + 'a> From<Dropdown<'a, Message>> for Element<'a, Message> {
    fn from(dropdown: Dropdown<'a, Message>) -> Self {
        Element::new(dropdown)
    }
}

/// The menu, dropped: under what opened it, kept inside the window.
struct Dropped<'a, 'b, Message> {
    /// What opened it, in the window.
    anchor: Rectangle,
    offset: Vector,
    menu: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
    dismiss: Option<Message>,
}

/// The room a menu opened from `anchor` has under it and over it, in a
/// window of `window` size, `offset` away from it.
fn rooms(anchor: Rectangle, offset: Vector, window: Size) -> (f32, f32) {
    let below = window.height - (anchor.y + anchor.height + offset.y);
    let above = anchor.y - offset.y;
    (below.max(0.0), above.max(0.0))
}

/// Where a menu of `size` goes, opened from `anchor` in a window of
/// `window` size: `offset` under it — or, with no room below, as far over
/// it, so it does not cover what opened it (shifted up over it, the menu
/// hid the toggle that would close it); fitting neither, on the side with
/// more room (it was laid out no taller: [`rooms`]) — then moved just
/// enough to stay inside the window.
fn dropped_at(anchor: Rectangle, offset: Vector, size: Size, window: Size) -> Point {
    let (room_below, room_above) = rooms(anchor, offset, window);
    let below = anchor.y + anchor.height + offset.y;
    let above = anchor.y - offset.y - size.height;
    let y = if size.height <= room_below {
        below
    } else if size.height <= room_above || room_above > room_below {
        above
    } else {
        below
    };
    Point::new(
        (anchor.x + offset.x).clamp(0.0, (window.width - size.width).max(0.0)),
        y.clamp(0.0, (window.height - size.height).max(0.0)),
    )
}

impl<Message: Clone> overlay::Overlay<Message, Theme, Renderer> for Dropped<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        // No taller than the larger side's room: a menu fitting neither
        // side was clamped over what opened it. Its list scrolls.
        let (below, above) = rooms(self.anchor, self.offset, bounds);
        let tall = below.max(above);
        let laid = self.menu.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(bounds.width, tall)),
        );
        let at = dropped_at(self.anchor, self.offset, laid.size(), bounds);
        laid.move_to(at)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.menu.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &layout.bounds(),
        );
    }

    fn operate(
        &mut self,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.menu
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        self.menu.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
        // A press away from it, and from what opened it (whose own press
        // closes it), puts it away.
        if let Some(dismiss) = &self.dismiss
            && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
            && !cursor.is_over(layout.bounds())
            && !cursor.is_over(self.anchor)
        {
            shell.publish(dismiss.clone());
        }
        // The menu is the pointer's over all of it, its rim too: a press or
        // a wheel there reached the code or the card under it.
        if cursor.is_over(layout.bounds())
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::WheelScrolled { .. })
            )
        {
            shell.capture_event();
        }
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let over = self.menu.as_widget().mouse_interaction(
            self.tree,
            layout,
            cursor,
            &layout.bounds(),
            renderer,
        );
        // Over its rim too, the pointer is the menu's, not what is under it.
        if over == mouse::Interaction::None && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Idle
        } else {
            over
        }
    }

    fn overlay<'c>(
        &'c mut self,
        layout: Layout<'c>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'c, Message, Theme, Renderer>> {
        self.menu.as_widget_mut().overlay(
            self.tree,
            layout,
            renderer,
            &layout.bounds(),
            Vector::ZERO,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A menu drops under what opened it; with no room below it opens
    /// above it, never over it; and it stays inside the window.
    #[test]
    fn a_menu_drops_under_its_anchor_or_above_it() {
        let window = Size::new(800.0, 600.0);
        let menu = Size::new(320.0, 340.0);
        let gap = Vector::new(-6.0, 6.0);
        let anchor = |x: f32, y: f32| Rectangle::new(Point::new(x, y), Size::new(120.0, 28.0));
        // Room below: under it.
        assert_eq!(
            dropped_at(anchor(100.0, 50.0), gap, menu, window),
            Point::new(94.0, 84.0)
        );
        // An Ask card's header near the window's foot: above it, clear of it.
        let low = anchor(700.0, 450.0);
        let at = dropped_at(low, gap, menu, window);
        assert_eq!(at, Point::new(480.0, 450.0 - 6.0 - 340.0));
        assert!(at.y + menu.height <= low.y, "the toggle stays uncovered");
        // Room on neither side: on the side with more, never over the
        // toggle — laid out no taller than that side's room.
        let mid = anchor(100.0, 300.0);
        let (below, above) = rooms(mid, gap, window);
        assert_eq!((below, above), (266.0, 294.0));
        let at = dropped_at(mid, gap, Size::new(320.0, above), window);
        assert!(at.y + above <= mid.y, "over it: {at:?}");
        let low = anchor(100.0, 250.0);
        let (below, above) = rooms(low, gap, window);
        assert!(below > above);
        let at = dropped_at(low, gap, Size::new(320.0, below), window);
        assert!(at.y >= low.y + low.height, "under it: {at:?}");
    }
}
