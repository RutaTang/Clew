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
    }
}

pub(crate) struct Dropdown<'a, Message> {
    anchor: Element<'a, Message>,
    menu: Option<Element<'a, Message>>,
    offset: Vector,
}

impl<Message> Widget<Message, Theme, Renderer> for Dropdown<'_, Message> {
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
        let bounds = layout.bounds();
        let at = Point::new(bounds.x, bounds.y + bounds.height) + translation + self.offset;
        let menu = match (self.menu.as_mut(), menu_tree) {
            (Some(menu), Some(tree)) => {
                Some(overlay::Element::new(Box::new(Dropped { at, menu, tree })))
            }
            _ => None,
        };
        if anchors.is_none() && menu.is_none() {
            return None;
        }
        Some(overlay::Group::with_children(anchors.into_iter().chain(menu).collect()).overlay())
    }
}

impl<'a, Message: 'a> From<Dropdown<'a, Message>> for Element<'a, Message> {
    fn from(dropdown: Dropdown<'a, Message>) -> Self {
        Element::new(dropdown)
    }
}

/// The menu, dropped: at its point, kept inside the window.
struct Dropped<'a, 'b, Message> {
    at: Point,
    menu: &'b mut Element<'a, Message>,
    tree: &'b mut Tree,
}

/// Where a menu of `size` goes, asked for at `at` in a window of `window`
/// size: there, or moved left and up just enough to stay inside it.
fn dropped_at(at: Point, size: Size, window: Size) -> Point {
    Point::new(
        at.x.clamp(0.0, (window.width - size.width).max(0.0)),
        at.y.clamp(0.0, (window.height - size.height).max(0.0)),
    )
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for Dropped<'_, '_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let laid = self.menu.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let at = dropped_at(self.at, laid.size(), bounds);
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
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.menu.as_widget().mouse_interaction(
            self.tree,
            layout,
            cursor,
            &layout.bounds(),
            renderer,
        )
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

    /// A menu drops where it is asked, or as near as keeps it inside the
    /// window.
    #[test]
    fn a_menu_stays_inside_the_window() {
        let window = Size::new(800.0, 600.0);
        let menu = Size::new(320.0, 340.0);
        assert_eq!(
            dropped_at(Point::new(100.0, 50.0), menu, window),
            Point::new(100.0, 50.0)
        );
        assert_eq!(
            dropped_at(Point::new(700.0, 500.0), menu, window),
            Point::new(480.0, 260.0)
        );
    }
}
