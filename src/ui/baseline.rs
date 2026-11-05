//! A line of text set on the design's baseline wherever its box lands
//! (see [`super::ts::set_in`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{self, Renderer as _};
use iced::advanced::widget::{Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell, mouse, overlay};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

use super::ts::{Face, line_top};

/// `content` — a box holding a line of `face` at `size`, laid out to set
/// its baseline where the design's renderer paints it in a box whose top
/// is a whole pixel — drawn moved by under a pixel, so the baseline lands
/// there from wherever the box really is: a box at a fractional place (a
/// menu placed by arithmetic, a row centred in an odd height) would
/// otherwise set it off by as much.
pub(crate) struct Snapped<'a, Message> {
    content: Element<'a, Message>,
    height: f32,
    own: f32,
    face: Face,
    size: f32,
}

impl<'a, Message> Snapped<'a, Message> {
    /// `content`, a box `height` tall holding a line whose own line box is
    /// `own` tall ([`line_top`]).
    pub(crate) fn new(
        content: Element<'a, Message>,
        height: f32,
        own: f32,
        face: Face,
        size: f32,
    ) -> Self {
        Self {
            content,
            height,
            own,
            face,
            size,
        }
    }
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Snapped<'_, Message> {
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
        // Where the line goes from where the box is, less where the
        // content puts it, laid out as if the box's top were whole.
        let top = layout.bounds().y;
        let at = |top| line_top(top, self.height, self.own, self.face, self.size);
        let shift = at(top) - (top + at(0.0));
        renderer.with_translation(Vector::new(0.0, shift), |renderer| {
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

impl<'a, Message: 'a> From<Snapped<'a, Message>> for Element<'a, Message> {
    fn from(snapped: Snapped<'a, Message>) -> Self {
        Element::new(snapped)
    }
}
