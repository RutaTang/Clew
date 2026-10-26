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
/// where it is.
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
    /// What it reported last, until the app has it.
    reported: Option<Option<Rectangle>>,
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
        let bounds = layout.bounds();
        let state = tree.state.downcast_mut::<State>();
        let place = (!state.squeezed
            && bounds.width > 0.0
            && bounds.height > 0.0
            && within(viewport, &bounds))
        .then_some(bounds);
        if self.known == Some(place) {
            state.reported = None;
        } else if state.reported != Some(place) {
            // Once until the app has it: one frame's events come in a batch.
            state.reported = Some(place);
            shell.publish(Message::Placed {
                question: self.question.clone(),
                place,
            });
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
