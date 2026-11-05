//! A height held while what is under it is asked about (see [`held`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// `content`, as tall as it lays out — until `hold` is set. From then on it
/// keeps the height it had the first time it was held (less, only if the
/// room left it shrinks below that), and what it holds scrolls inside it
/// rather than growing it. Let go, it lays out as before.
///
/// What it is for: the Ask card fitted to its conversation, with a tool
/// call's approval asked under the conversation. Steps, plan entries or
/// words arriving above the approval grew the conversation and moved the
/// approval's answers down, under a pointer about to click something else.
///
/// The wrapper is there held or not, so `content` keeps its place in the
/// widget tree — and with it its scroll — as the hold comes and goes.
pub(crate) fn held<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    hold: bool,
) -> Element<'a, Message> {
    Element::new(Held {
        content: content.into(),
        hold,
    })
}

struct Held<'a, Message> {
    content: Element<'a, Message>,
    hold: bool,
}

/// The height held, while held.
#[derive(Debug, Default)]
struct State {
    height: Option<f32>,
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Held<'_, Message> {
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
        let held = {
            let state = tree.state.downcast_mut::<State>();
            if !self.hold {
                state.height = None;
            }
            state.height
        };
        let Some(height) = held else {
            let node = self
                .content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, limits);
            if self.hold {
                tree.state.downcast_mut::<State>().height = Some(node.size().height);
            }
            return node;
        };
        let height = height.min(limits.max().height);
        let limits = layout::Limits::new(
            Size::new(limits.min().width, height),
            Size::new(limits.max().width, height),
        );
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, &limits)
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
    use super::held;
    use iced::Size;
    use iced::advanced::layout::Limits;
    use iced::advanced::widget::Tree;
    use iced::widget::space;

    /// How tall `held` lays out a space of each height in turn, holding or
    /// not, its tree kept from one layout to the next as the window keeps
    /// it.
    fn heights(steps: &[(f32, bool)]) -> Vec<f32> {
        let renderer = iced_test::futures::futures::executor::block_on(
            <iced::Renderer as iced::advanced::renderer::Headless>::new(
                iced::Font::DEFAULT,
                iced::Pixels(16.0),
                None,
            ),
        )
        .expect("a headless renderer");
        let limits = Limits::new(Size::ZERO, Size::new(400.0, 1000.0));
        let mut tree: Option<Tree> = None;
        let mut out = Vec::new();
        for &(h, hold) in steps {
            let mut element: iced::Element<'_, ()> = held(space().height(h), hold);
            let tree = tree.get_or_insert_with(|| Tree::new(&element));
            tree.diff(&element);
            let node = element.as_widget_mut().layout(tree, &renderer, &limits);
            out.push(node.size().height);
        }
        out
    }

    /// Unheld it follows its content; held, it keeps the height it had when
    /// the hold began, whatever its content does; let go, it follows again.
    #[test]
    fn a_held_height_stays_put() {
        assert_eq!(
            heights(&[
                (100.0, false),
                (120.0, false),
                (120.0, true),
                (300.0, true),
                (50.0, true),
                (300.0, false),
            ]),
            [100.0, 120.0, 120.0, 120.0, 120.0, 300.0]
        );
    }
}
