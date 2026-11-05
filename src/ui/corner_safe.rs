//! What floats over the window — a panel, a card — drawn so its shadow
//! leaves the window's rounded corners clear (see [`CornerSafe`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell, mouse, overlay};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// `content`, a panel floating over the window, drawn so its shadow (the
/// design's 70 px blur) stays off the window's corners, which are left
/// clear over the desktop. The design's window clips what floats over it
/// to its rounded shape; iced clips to rectangles only. The GPU renderer
/// draws it clipped to the window less a band as tall as the corners'
/// radius at its top and its foot: no panel comes so near either, only the
/// faint ends of a shadow are cut, and what sets layers of its own (a
/// field's text) stays inside the panel, so one clip draws everything once.
/// The software renderer draws a quad's shadow past any clip, so there the
/// shadow is scaled down, as little as keeps it off the corners
/// ([`shadow_scale`]); Clew rounds no window that renderer draws, but a
/// window drawn so is still left clear.
pub(crate) struct CornerSafe<'a, Message> {
    content: Element<'a, Message>,
}

impl<'a, Message> CornerSafe<'a, Message> {
    pub(crate) fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for CornerSafe<'_, Message> {
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
        let draw = |renderer: &mut iced::Renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
        };
        let radius = crate::theme::window_corners();
        if radius <= 0.0 {
            draw(renderer);
            return;
        }
        if matches!(renderer, iced::Renderer::Secondary(_)) {
            let scale = shadow_scale(layout.bounds(), *viewport, radius);
            crate::theme::with_shadow_scale(scale, || draw(renderer));
            return;
        }
        let clear = Rectangle {
            x: viewport.x,
            y: viewport.y + radius,
            width: viewport.width,
            height: (viewport.height - 2.0 * radius).max(0.0),
        };
        renderer.with_layer(clear, draw);
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

/// The most of its reach (0 to 1) the shadow of a panel at `panel` may
/// have in a window `window` whose corners are `radius`, for it to touch
/// none of the corners' squares: for each, it may reach as far as keeps it
/// either beside the square or above or below it.
pub(crate) fn shadow_scale(panel: Rectangle, window: Rectangle, radius: f32) -> f32 {
    use crate::theme::{PANEL_SHADOW_BLUR as BLUR, PANEL_SHADOW_OFFSET as OFFSET};
    let left = (panel.x - (window.x + radius)) / BLUR;
    let right = ((window.x + window.width - radius) - (panel.x + panel.width)) / BLUR;
    let top = (panel.y - (window.y + radius)) / (BLUR - OFFSET);
    let bottom = ((window.y + window.height - radius) - (panel.y + panel.height)) / (BLUR + OFFSET);
    [
        left.max(top),
        right.max(top),
        left.max(bottom),
        right.max(bottom),
    ]
    .into_iter()
    .fold(1.0_f32, f32::min)
    .clamp(0.0, 1.0)
}

impl<'a, Message: 'a> From<CornerSafe<'a, Message>> for Element<'a, Message> {
    fn from(safe: CornerSafe<'a, Message>) -> Self {
        Element::new(safe)
    }
}

#[cfg(test)]
mod tests {
    use super::shadow_scale;
    use iced::Rectangle;

    /// A panel well inside the window casts the design's whole shadow; one
    /// near a corner, only as much as stays off it.
    #[test]
    fn a_shadow_keeps_off_the_corners() {
        use crate::theme::{PANEL_SHADOW_BLUR as BLUR, PANEL_SHADOW_OFFSET as OFFSET};
        let window = Rectangle::new(iced::Point::ORIGIN, iced::Size::new(1216.0, 912.0));
        let palette = Rectangle::new(iced::Point::new(317.0, 89.0), iced::Size::new(582.0, 352.0));
        assert_eq!(shadow_scale(palette, window, 12.0), 1.0);
        // A dialog low in a small window: its shadow keeps beside, above or
        // below every corner.
        let window = Rectangle::new(iced::Point::ORIGIN, iced::Size::new(640.0, 420.0));
        let dialog = Rectangle::new(iced::Point::new(50.0, 41.0), iced::Size::new(540.0, 338.0));
        let radius = 11.0;
        let scale = shadow_scale(dialog, window, radius);
        assert!(scale < 1.0);
        // The shadow's reach, and the corners' squares it keeps beside or
        // above (or below).
        let (blur, offset) = (BLUR * scale, OFFSET * scale);
        let shadow = Rectangle {
            x: dialog.x - blur,
            y: dialog.y + offset - blur,
            width: dialog.width + 2.0 * blur,
            height: dialog.height + 2.0 * blur,
        };
        for (x, y) in [
            (0.0, 0.0),
            (window.width - radius, 0.0),
            (0.0, window.height - radius),
            (window.width - radius, window.height - radius),
        ] {
            let corner = Rectangle::new(iced::Point::new(x, y), iced::Size::new(radius, radius));
            let apart = shadow.x >= corner.x + corner.width - 1e-3
                || shadow.x + shadow.width <= corner.x + 1e-3
                || shadow.y >= corner.y + corner.height - 1e-3
                || shadow.y + shadow.height <= corner.y + 1e-3;
            assert!(apart, "{shadow:?} reaches {corner:?}");
        }
    }
}
