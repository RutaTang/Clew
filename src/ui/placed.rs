//! A question's answers, reporting where they are laid out (see [`placed`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

use crate::Message;
use crate::app::model::Question;

/// What a question's view needs of the gate its Allow goes through
/// (`App::may_allow`): whether the Allow counts, and — to report where the
/// answers are now ([`placed`]) — which question it is and where its
/// answers were last laid out. A bare `bool` (a view drawn on its own, in a
/// test) reports nothing.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub ready: bool,
    pub report: Option<(Question, Option<Option<Rectangle>>)>,
}

impl From<bool> for Asked {
    fn from(ready: bool) -> Self {
        Asked {
            ready,
            report: None,
        }
    }
}

/// `content` — a question, its answers in it — reporting where it is laid
/// out, as `Message::Placed`, whenever that is not where `asked` last had
/// it: its bounds, or none when not all of it is in view (past the window
/// or a scrollable's edge) or it was given less room than it takes (a panel
/// too low for it). The gate counts a question's delay from when its
/// answers last moved: one that moved — the window resized, the panel
/// dragged, what is around it wrapped anew — or came into view was not read
/// where it is. Each question is drawn once: two views of one, at two
/// places, would each report against the other's.
pub(crate) fn placed<'a>(
    asked: &Asked,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let content = content.into();
    match &asked.report {
        None => content,
        Some((question, known)) => Element::new(Placed {
            question: question.clone(),
            known: *known,
            content,
        }),
    }
}

/// See [`placed`].
struct Placed<'a> {
    question: Question,
    /// Where the app has it (`None`: nowhere yet).
    known: Option<Option<Rectangle>>,
    content: Element<'a, Message>,
}

/// What [`Placed`] keeps between frames.
#[derive(Default)]
struct State {
    /// Laid out in less room than it takes.
    squeezed: bool,
    /// What it reported in this view, for which question: said once for
    /// all the events of a frame, and again in the next view if the app
    /// does not have it (it dropped it: the window moved).
    reported: Option<(Question, Option<Rectangle>)>,
}

/// Whether `inner` lies within `outer`, give or take rounding.
fn within(outer: &Rectangle, inner: &Rectangle) -> bool {
    const SLACK: f32 = 0.5;
    inner.x >= outer.x - SLACK
        && inner.y >= outer.y - SLACK
        && inner.x + inner.width <= outer.x + outer.width + SLACK
        && inner.y + inner.height <= outer.y + outer.height + SLACK
}

impl Widget<Message, iced::Theme, iced::Renderer> for Placed<'_> {
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
        // A view built anew, from what the app has now.
        tree.state.downcast_mut::<State>().reported = None;
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // The room it takes, as wide as it may be and as tall as it likes;
        // then the room it has — last, so its widgets keep what that left.
        let free = layout::Limits::new(Size::ZERO, Size::new(limits.max().width, f32::INFINITY));
        let wanted = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, &free)
            .size();
        let node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let got = node.size();
        tree.state.downcast_mut::<State>().squeezed =
            got.height + 0.5 < wanted.height || got.width + 0.5 < wanted.width;
        node
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
        // Where it is, said ahead of what the event does to it: a release
        // that is the first event after a move is an Allow only after the
        // move.
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        let place = (!state.squeezed
            && bounds.width > 0.0
            && bounds.height > 0.0
            && within(viewport, &bounds))
        .then_some(bounds);
        if self.known != Some(place)
            && state.reported.as_ref() != Some(&(self.question.clone(), place))
        {
            // Once for this view: one frame's events come in a batch. For
            // this question: another drawn in its place, alike, is not the
            // one reported.
            state.reported = Some((self.question.clone(), place));
            shell.publish(Message::Placed {
                question: self.question.clone(),
                place,
            });
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

/// `content` — the whole window — reporting, as `Message::OverlayOpen`,
/// whether a widget in it draws an overlay of its own now (a picker's menu,
/// a tooltip, …) whenever that is not what the app has (`known`). iced
/// draws such an overlay over everything, so it may hide any question:
/// read from what is drawn, frame by frame, it cannot be left standing by
/// a picker taken away with its menu open, nor dropped by one picker's
/// close while another opens.
pub(crate) fn watch_overlays<'a>(
    known: bool,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Overlays {
        known,
        content: content.into(),
    })
}

/// See [`watch_overlays`].
struct Overlays<'a> {
    known: bool,
    content: Element<'a, Message>,
}

/// What [`Overlays`] keeps between frames.
#[derive(Default)]
struct Seen {
    /// Whether the content gave an overlay when last asked (iced asks at
    /// the start of every frame's events, and to draw).
    open: bool,
    /// What it reported in this view.
    reported: Option<bool>,
}

impl Widget<Message, iced::Theme, iced::Renderer> for Overlays<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Seen>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Seen::default())
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
        tree.state.downcast_mut::<Seen>().reported = None;
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
        // Said ahead of what the event does, as `placed` says where it is.
        let seen = tree.state.downcast_mut::<Seen>();
        if seen.open != self.known && seen.reported != Some(seen.open) {
            seen.reported = Some(seen.open);
            shell.publish(Message::OverlayOpen(seen.open));
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
        let (seen, children) = (&mut tree.state, &mut tree.children);
        let overlay = self.content.as_widget_mut().overlay(
            &mut children[0],
            layout,
            renderer,
            viewport,
            translation,
        );
        seen.downcast_mut::<Seen>().open = overlay.is_some();
        overlay
    }
}

#[cfg(test)]
mod tests {
    use super::{Asked, placed};
    use crate::Message;
    use crate::app::model::Question;
    use iced::advanced::clipboard;
    use iced::widget::{button, column, space};
    use iced::{Event, Font, Pixels, Point, Rectangle, Size, mouse};
    use iced_test::runtime::user_interface::{Cache, UserInterface};

    /// A release that is the first event after a move — what is above the
    /// question grew while its Allow was held — is told after where the
    /// Allow is now: the gate has the move before it has the answer.
    #[test]
    fn where_it_is_is_said_before_what_the_event_does() {
        let mut renderer = iced_test::futures::futures::executor::block_on(
            <iced::Renderer as iced::advanced::renderer::Headless>::new(
                Font::with_name("Fira Sans"),
                Pixels(16.0),
                None,
            ),
        )
        .expect("a headless renderer");
        let question = Question::LspConsent(1);
        let view =
            |above: f32, known: Option<Option<Rectangle>>| -> iced::Element<'static, Message> {
                let asked = Asked {
                    ready: true,
                    report: Some((question.clone(), known)),
                };
                column![
                    space().height(above),
                    placed(&asked, button("Allow").height(60).on_press(Message::Noop)),
                ]
                .into()
            };
        let cursor = mouse::Cursor::Available(Point::new(20.0, 30.0));
        let mut sent: Vec<Message> = Vec::new();
        let size = Size::new(200.0, 200.0);
        // (Not of no height: a column leaves that out, and the button's
        // state would move with it.)
        let mut ui = UserInterface::build(view(1.0, None), size, Cache::default(), &mut renderer);
        let press = [Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))];
        let _ = ui.update(
            &press,
            cursor,
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        let first = match sent.as_slice() {
            [
                Message::Placed {
                    place: Some(at), ..
                },
            ] => *at,
            other => panic!("{other:?}"),
        };
        let cache = ui.into_cache();
        sent.clear();
        let mut ui = UserInterface::build(view(5.0, Some(Some(first))), size, cache, &mut renderer);
        let release = [Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))];
        let _ = ui.update(
            &release,
            cursor,
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        assert!(
            matches!(
                sent.as_slice(),
                [Message::Placed { place: Some(at), .. }, Message::Noop] if at.y == first.y + 4.0
            ),
            "{sent:?}"
        );
    }
}
