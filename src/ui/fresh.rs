//! A part of the view that starts afresh whenever what it shows is another
//! one (see [`fresh`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// `content`, its widgets' state (a button held pressed, a scroll offset)
/// thrown away whenever `key` changes: one question drawn where another
/// was, in the same shape, is not handed that one's state. iced matches a
/// widget's state to it by its place in the view, and its keyed column by
/// key only when its children are added or removed — one child replaced by
/// another kept its pressed buttons, so the release of a press made on one
/// question's Allow answered the next.
pub(crate) fn fresh<'a, Message: 'a>(
    key: u64,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Fresh {
        key,
        content: content.into(),
    })
}

/// A question's `button` that allows, drawn anew whenever it comes to count
/// (`ready`) or stops: a button keeps a press it holds while it is
/// disabled, so a press made on an Allow that counted, held through a
/// stretch where it did not (its question moved, or covered), was let go
/// of as an answer once it counted again. Its declines are left as they
/// are: one pressed just before the Allow comes to count still declines.
pub(crate) fn allow<'a, Message: 'a>(
    ready: bool,
    button: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    fresh(u64::from(ready), button)
}

/// [`fresh`], keyed by what `what` hashes to: the question a prompt asks,
/// the card a tool call is waiting on.
pub(crate) fn fresh_for<'a, Message: 'a>(
    what: &impl std::hash::Hash,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    what.hash(&mut hasher);
    fresh(hasher.finish(), content)
}

/// See [`fresh`].
struct Fresh<'a, Message> {
    key: u64,
    content: Element<'a, Message>,
}

/// The key the state below was made for.
struct Key(u64);

/// The keys of the [`fresh`] parts of `element`, outermost first.
#[cfg(test)]
pub(crate) fn keys_in<Message>(element: &Element<'_, Message>) -> Vec<u64> {
    fn walk(tree: &Tree, keys: &mut Vec<u64>) {
        if tree.tag == tree::Tag::of::<Key>() {
            keys.push(tree.state.downcast_ref::<Key>().0);
        }
        for child in &tree.children {
            walk(child, keys);
        }
    }
    let mut keys = Vec::new();
    walk(&Tree::new(element), &mut keys);
    keys
}

impl<Message> Widget<Message, iced::Theme, iced::Renderer> for Fresh<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Key>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Key(self.key))
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
        if tree.state.downcast_ref::<Key>().0 == self.key {
            tree.diff_children(std::slice::from_ref(&self.content));
        } else {
            tree.state = tree::State::new(Key(self.key));
            tree.children = vec![Tree::new(&self.content)];
        }
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
    use super::fresh;
    use iced::advanced::clipboard;
    use iced::widget::button;
    use iced::{Event, Font, Pixels, Point, Size, mouse};
    use iced_test::runtime::user_interface::{Cache, UserInterface};

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Allow(u64),
    }

    /// The Allow of question `key`, enabled or not, filling the window.
    fn question<'a>(key: u64, enabled: bool, keyed: bool) -> iced::Element<'a, Msg> {
        let allow = button("Allow")
            .width(iced::Fill)
            .height(iced::Fill)
            .on_press_maybe(enabled.then_some(Msg::Allow(key)));
        match keyed {
            true => fresh(key, allow),
            false => allow.into(),
        }
    }

    /// Press on question 1's Allow, then — the button held — question 2
    /// drawn in its place, disabled for a while, then enabled, and the
    /// release: what it sent.
    fn held_across(keyed: bool) -> Vec<Msg> {
        let mut renderer = iced_test::futures::futures::executor::block_on(
            <iced::Renderer as iced::advanced::renderer::Headless>::new(
                Font::with_name("Fira Sans"),
                Pixels(16.0),
                None,
            ),
        )
        .expect("a headless renderer");
        let size = Size::new(200.0, 100.0);
        let cursor = mouse::Cursor::Available(Point::new(100.0, 50.0));
        let mut cache = Cache::default();
        let mut sent = Vec::new();
        let steps = [
            (
                question(1, true, keyed),
                Some(mouse::Event::ButtonPressed(mouse::Button::Left)),
            ),
            (question(2, false, keyed), None),
            (
                question(2, true, keyed),
                Some(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ),
        ];
        for (view, event) in steps {
            let mut ui = UserInterface::build(view, size, cache, &mut renderer);
            let events: Vec<Event> = event.into_iter().map(Event::Mouse).collect();
            let _ = ui.update(
                &events,
                cursor,
                &mut renderer,
                &mut clipboard::Null,
                &mut sent,
            );
            cache = ui.into_cache();
        }
        sent
    }

    #[test]
    fn a_press_held_on_one_question_does_not_answer_the_next() {
        assert_eq!(held_across(false), [Msg::Allow(2)], "the case it is for");
        assert!(held_across(true).is_empty());
    }
}
