//! A draggable divider between panels: a thin strip you grab to resize the
//! neighbouring panel. It reports the cursor's position (absolute window coords)
//! as you drag, and the app turns that into a width or height. Vertical dividers
//! resize width (the left sidebar, the right context panel); horizontal ones
//! resize height (the bottom debug / ask panel).

use iced::advanced::widget::{Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use iced::{Color, Element, Event, Length, Rectangle, Size};

use crate::theme;

/// The clickable thickness of the strip; the visible line is 1px, centered.
const THICKNESS: f32 = 8.0;

pub struct Divider<'a, Message> {
    vertical: bool,
    on_drag: Box<dyn Fn(f32) -> Message + 'a>,
}

#[derive(Default)]
struct State {
    dragging: bool,
    hovered: bool,
}

impl<'a, Message> Divider<'a, Message> {
    /// A vertical bar between side-by-side panels; reports the cursor's x.
    pub fn vertical(on_drag: impl Fn(f32) -> Message + 'a) -> Self {
        Self { vertical: true, on_drag: Box::new(on_drag) }
    }

    /// A horizontal bar between stacked panels; reports the cursor's y.
    pub fn horizontal(on_drag: impl Fn(f32) -> Message + 'a) -> Self {
        Self { vertical: false, on_drag: Box::new(on_drag) }
    }

    fn lengths(&self) -> (Length, Length) {
        if self.vertical {
            (Length::Fixed(THICKNESS), Length::Fill)
        } else {
            (Length::Fill, Length::Fixed(THICKNESS))
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Divider<'_, Message>
where
    Renderer: renderer::Renderer,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn size(&self) -> Size<Length> {
        let (w, h) = self.lengths();
        Size::new(w, h)
    }

    fn layout(
        &mut self,
        _tree: &mut tree::Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let (w, h) = self.lengths();
        layout::atomic(limits, w, h)
    }

    fn update(
        &mut self,
        tree: &mut tree::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State>();
        let bounds = layout.bounds();
        let over = cursor.position().is_some_and(|p| bounds.contains(p));
        if !state.dragging && state.hovered != over {
            state.hovered = over;
            shell.request_redraw();
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if over => {
                state.dragging = true;
                shell.capture_event();
            }
            // A drag keeps firing even as the cursor leaves the thin strip, so
            // resizing stays smooth; `dragging` gates it.
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                if let Some(p) = cursor.position() {
                    let coord = if self.vertical { p.x } else { p.y };
                    shell.publish((self.on_drag)(coord));
                }
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &tree::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();
        let over = cursor.position().is_some_and(|p| layout.bounds().contains(p));
        if state.dragging || over {
            if self.vertical {
                mouse::Interaction::ResizingHorizontally
            } else {
                mouse::Interaction::ResizingVertically
            }
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &tree::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let over = cursor.position().is_some_and(|p| bounds.contains(p));
        let active = state.dragging || state.hovered || over;
        let color: Color = if active { theme::ACCENT } else { theme::HAIRLINE };
        // A 1px line centered in the strip; the rest is invisible grab area.
        let line = if self.vertical {
            Rectangle { x: bounds.x + bounds.width / 2.0 - 0.5, y: bounds.y, width: 1.0, height: bounds.height }
        } else {
            Rectangle { x: bounds.x, y: bounds.y + bounds.height / 2.0 - 0.5, width: bounds.width, height: 1.0 }
        };
        renderer.fill_quad(renderer::Quad { bounds: line, ..Default::default() }, color);
    }
}

impl<'a, Message, Theme, Renderer> From<Divider<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(divider: Divider<'a, Message>) -> Self {
        Self::new(divider)
    }
}
