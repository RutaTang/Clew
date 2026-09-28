//! The reader's own scrolling of a pane, told apart from the app's (see
//! [`reader_scroll`]).

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::operation::scrollable::Scrollable as ScrollState;
use iced::advanced::widget::{Operation, Tree, Widget};
use iced::advanced::{Clipboard, Shell, mouse, overlay, renderer};
use iced::widget::{Id, Scrollable};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// `scrollable`, publishing `report` whenever the reader scrolls it: the
/// wheel or the trackpad anywhere over it, its scrollbar included, a drag of
/// the scrollbar, a click on its track. Its own `on_scroll` cannot say who
/// scrolled: it reports every change of the viewport, and the app's
/// `scroll_to` makes those too.
///
/// What counts is the scrollable moving while it handles an input event:
/// then only the reader's input moves it, since the app's scrolls are
/// operations, which run between events. A wheel that moves nothing is not
/// reported. Nothing is captured, and the scrollable scrolls as it would
/// without this. The watch sits around the scrollable, not inside it: a
/// scrollable hands its content no event its scrollbar takes, and no wheel
/// for a second and a half after it last scrolled.
///
/// `None` watches nothing and costs nothing. The wrapper stays in the tree
/// either way, so the scrollable keeps its place there — and with it its
/// offset — as the report comes and goes.
pub(crate) fn reader_scroll<'a, Message: Clone + 'a>(
    scrollable: Scrollable<'a, Message>,
    report: Option<Message>,
) -> Element<'a, Message> {
    Element::new(ReaderScroll {
        scrollable: scrollable.into(),
        report,
    })
}

/// See [`reader_scroll`]: the scrollable unchanged — its layout, drawing,
/// state and events — plus the report.
struct ReaderScroll<'a, Message> {
    scrollable: Element<'a, Message>,
    report: Option<Message>,
}

impl<Message> ReaderScroll<'_, Message> {
    /// Where the scrollable is scrolled to.
    fn translation(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
    ) -> Option<Vector> {
        let mut probe = Translation(None);
        self.scrollable.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut probe,
        );
        probe.0
    }
}

/// Reads the translation of the first scrollable it meets, and goes no
/// deeper.
struct Translation(Option<Vector>);

impl Operation for Translation {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        if self.0.is_none() {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn ScrollState,
    ) {
        self.0.get_or_insert(translation);
    }
}

impl<Message: Clone> Widget<Message, iced::Theme, iced::Renderer> for ReaderScroll<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.scrollable.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.scrollable.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.scrollable)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.scrollable));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.scrollable
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
        self.scrollable.as_widget().draw(
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
        self.scrollable
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
        let watching = self.report.is_some();
        let before = if watching {
            self.translation(tree, layout, renderer)
        } else {
            None
        };
        self.scrollable.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if watching
            && self.translation(tree, layout, renderer) != before
            && let Some(report) = &self.report
        {
            shell.publish(report.clone());
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
        self.scrollable.as_widget().mouse_interaction(
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
        self.scrollable.as_widget_mut().overlay(
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
    use super::reader_scroll;
    use iced::advanced::clipboard;
    use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, scroll_to};
    use iced::widget::{Id, container, scrollable, space};
    use iced::{Event, Fill, Font, Pixels, Point, Size, mouse, window};
    use iced_test::runtime::user_interface::{Cache, UserInterface};

    #[derive(Debug, Clone)]
    enum Msg {
        Reader,
        Scrolled(scrollable::Viewport),
    }

    /// A document `height` tall and narrower than its 1024 × 768 pane, in a
    /// scrollable that reports the reader's scrolling when `watched`.
    fn pane<'a>(height: f32, watched: bool) -> iced::Element<'a, Msg> {
        let scroller = scrollable(container(space().width(300).height(height)))
            .id(Id::new("pane"))
            .on_scroll(Msg::Scrolled)
            .width(Fill)
            .height(Fill);
        reader_scroll(scroller, watched.then_some(Msg::Reader))
    }

    fn reports(messages: &[Msg]) -> usize {
        messages.iter().filter(|m| matches!(m, Msg::Reader)).count()
    }

    fn scrolled(messages: &[Msg]) -> bool {
        messages
            .iter()
            .any(|m| matches!(m, Msg::Scrolled(v) if v.absolute_offset().y > 0.0))
    }

    fn wheel() -> Event {
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -120.0 },
        })
    }

    /// Over the pane's vertical scrollbar, at `y` (the default one is 10
    /// wide, along the right edge).
    fn on_scrollbar(y: f32) -> Point {
        Point::new(1019.0, y)
    }

    /// A renderer for a [`UserInterface`] built by hand.
    fn headless() -> iced::Renderer {
        iced_test::futures::futures::executor::block_on(
            <iced::Renderer as iced::advanced::renderer::Headless>::new(
                Font::with_name("Fira Sans"),
                Pixels(16.0),
                None,
            ),
        )
        .expect("a headless renderer")
    }

    /// The reader's wheel over a pane is reported — over the margin beside a
    /// document narrower than the pane as well, where it scrolls the
    /// document all the same — and the pane still scrolls: nothing captures
    /// the event.
    #[test]
    fn the_readers_wheel_is_reported_and_still_scrolls() {
        let mut sim = iced_test::simulator(pane(4000.0, true));
        sim.point_at(Point::new(700.0, 300.0));
        let _ = sim.simulate([wheel()]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(reports(&messages), 1, "{messages:?}");
        assert!(
            scrolled(&messages),
            "the wheel no longer scrolled: {messages:?}"
        );
    }

    /// The scrollbar is the reader's too: a wheel over it, a drag of its
    /// thumb, a click on its track. The scrollable takes each of these for
    /// itself, and hands its content none of them.
    #[test]
    fn the_readers_scrollbar_is_reported() {
        let mut sim = iced_test::simulator(pane(4000.0, true));
        sim.point_at(on_scrollbar(300.0));
        let _ = sim.simulate([wheel()]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(
            reports(&messages),
            1,
            "a wheel on the scrollbar: {messages:?}"
        );

        // The thumb sits at the top: grab it and drag it down.
        let mut sim = iced_test::simulator(pane(4000.0, true));
        sim.point_at(on_scrollbar(40.0));
        let _ = sim.simulate([Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))]);
        sim.point_at(on_scrollbar(400.0));
        let _ = sim.simulate([
            Event::Mouse(mouse::Event::CursorMoved {
                position: on_scrollbar(400.0),
            }),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert!(scrolled(&messages), "the drag did not scroll: {messages:?}");
        assert!(reports(&messages) >= 1, "a thumb drag: {messages:?}");

        // Below the thumb, on the track.
        let mut sim = iced_test::simulator(pane(4000.0, true));
        sim.point_at(on_scrollbar(600.0));
        let _ = sim.simulate(iced_test::simulator::click());
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert!(
            scrolled(&messages),
            "the click did not scroll: {messages:?}"
        );
        assert_eq!(reports(&messages), 1, "a click on the track: {messages:?}");
    }

    /// A scrollable hands its content no wheel for a while after it last
    /// scrolled — one gesture, to it — so a watch inside it missed a new
    /// gesture right after another. Around it, every wheel that moves the
    /// pane is seen.
    #[test]
    fn a_wheel_right_after_another_is_reported() {
        let mut sim = iced_test::simulator(pane(4000.0, true));
        sim.point_at(Point::new(150.0, 300.0));
        let _ = sim.simulate([wheel()]);
        let _ = sim.simulate([wheel()]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(reports(&messages), 2, "{messages:?}");
    }

    /// Nothing is reported that the reader did not move: a wheel over a
    /// document that fits its pane, and — unwatched — any scroll at all,
    /// which then still happens.
    #[test]
    fn only_a_watched_pane_the_reader_moved_is_reported() {
        let mut sim = iced_test::simulator(pane(200.0, true));
        sim.point_at(Point::new(150.0, 100.0));
        let _ = sim.simulate([wheel(), wheel()]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(reports(&messages), 0, "nothing moved: {messages:?}");

        let mut sim = iced_test::simulator(pane(4000.0, false));
        sim.point_at(Point::new(150.0, 300.0));
        let _ = sim.simulate([wheel()]);
        let messages: Vec<Msg> = sim.into_messages().collect();
        assert!(scrolled(&messages), "{messages:?}");
        assert_eq!(reports(&messages), 0, "unwatched: {messages:?}");
    }

    /// The pane keeps its offset as the watch comes and goes: the wrapper is
    /// in the tree either way, so the scrollable keeps its place there, and
    /// with it its state. A bare scrollable while nothing was watched put
    /// another widget in that place each time the watch toggled, and the pane
    /// jumped to the top — at once, as the reader's first scroll superseded
    /// a step and so disarmed the watch.
    #[test]
    fn the_offset_is_kept_as_the_watch_comes_and_goes() {
        /// Where the pane is scrolled to.
        fn offset(
            ui: &mut UserInterface<'_, Msg, iced::Theme, iced::Renderer>,
            renderer: &iced::Renderer,
        ) -> iced::Vector {
            let mut probe = super::Translation(None);
            ui.operate(renderer, &mut probe);
            probe.0.expect("the pane's scrollable")
        }
        let mut renderer = headless();
        let size = Size::new(1024.0, 768.0);
        for watched in [true, false] {
            let mut ui =
                UserInterface::build(pane(4000.0, watched), size, Cache::default(), &mut renderer);
            let _ = ui.update(
                &[wheel()],
                mouse::Cursor::Available(Point::new(150.0, 300.0)),
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            let scrolled_to = offset(&mut ui, &renderer);
            assert!(scrolled_to.y > 0.0, "the wheel did not scroll");
            let mut ui =
                UserInterface::build(pane(4000.0, !watched), size, ui.into_cache(), &mut renderer);
            assert_eq!(
                offset(&mut ui, &renderer),
                scrolled_to,
                "the pane jumped as the watch {}",
                if watched { "went" } else { "came" }
            );
        }
    }

    /// The app's own scroll is not the reader's: a `scroll_to` moves the
    /// pane — its `on_scroll` reports that at the next event — and nothing
    /// is reported as the reader's.
    #[test]
    fn the_apps_own_scroll_is_not_reported() {
        let mut renderer = headless();
        let mut ui = UserInterface::build(
            pane(4000.0, true),
            Size::new(1024.0, 768.0),
            Cache::default(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut scroll_to(
                Id::new("pane"),
                AbsoluteOffset {
                    x: None,
                    y: Some(500.0),
                },
            ),
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[Event::Window(window::Event::RedrawRequested(
                std::time::Instant::now(),
            ))],
            mouse::Cursor::Available(Point::new(150.0, 300.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(scrolled(&messages), "the app's scroll: {messages:?}");
        assert_eq!(reports(&messages), 0, "{messages:?}");
    }
}
