//! A draggable divider between panels: a thin strip you grab to resize the
//! neighbouring panel. It reports the cursor's position (absolute window coords)
//! as you drag, and the app turns that into a width or height. Vertical dividers
//! resize width (the left sidebar, the right context panel); horizontal ones
//! resize height (the bottom debug / ask panel).
//!
//! Beside a side panel the strip is drawn as part of it, in its color, so the
//! panel's own edge is the one border between it and the code — no line of
//! its own a few pixels off that edge, which read as a double border. The edge
//! lights up in the accent color while it is hovered or dragged.

use iced::advanced::widget::{Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use iced::{Color, Element, Event, Length, Rectangle, Size};

use crate::theme;

/// The clickable thickness of the strip; the visible line is 1px, centered.
/// Public for layout math that must place things where the view draws them
/// (the tutorial's spotlight).
pub const THICKNESS: f32 = 8.0;

pub struct Divider<'a, Message> {
    vertical: bool,
    panel: Panel,
    on_drag: Box<dyn Fn(f32) -> Message + 'a>,
}

/// Which side of the strip the panel it resizes is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// Between two parts of one panel: a hairline down the middle.
    Within,
    /// Between two parts that read as one surface (the code and the margin
    /// beside it): nothing drawn until the pointer finds it, then the
    /// accent line a drag would move.
    Seamless,
    /// The panel is before it (left, or above): the strip is its margin.
    Before,
    /// The panel is after it (right, or below).
    After,
}

#[derive(Default)]
struct State {
    dragging: bool,
    hovered: bool,
}

impl<'a, Message> Divider<'a, Message> {
    /// A vertical bar between side-by-side panels; reports the cursor's x.
    pub fn vertical(on_drag: impl Fn(f32) -> Message + 'a) -> Self {
        Self {
            vertical: true,
            panel: Panel::Within,
            on_drag: Box::new(on_drag),
        }
    }

    /// A horizontal bar between stacked panels; reports the cursor's y.
    pub fn horizontal(on_drag: impl Fn(f32) -> Message + 'a) -> Self {
        Self {
            vertical: false,
            panel: Panel::Within,
            on_drag: Box::new(on_drag),
        }
    }

    /// The panel it resizes is on its `side`: the strip is drawn in the
    /// panel's color, its far edge the panel's border.
    pub fn panel(mut self, side: Panel) -> Self {
        self.panel = side;
        self
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
        let over = cursor
            .position()
            .is_some_and(|p| layout.bounds().contains(p));
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
        let colors = Colors {
            panel: theme::bg_panel(),
            line: theme::hairline(),
            accent: theme::accent(),
        };
        for (rect, color) in paint(bounds, self.vertical, self.panel, active, colors) {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: rect,
                    ..Default::default()
                },
                color,
            );
        }
    }
}

/// The colors a divider is painted with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    pub panel: Color,
    pub line: Color,
    pub accent: Color,
}

/// Width of the line an active (hovered, dragged) panel edge shows.
const ACTIVE_LINE: f32 = 2.0;

/// What a divider over `bounds` paints, in order: within a panel a 1px line
/// down the middle (the accent while active); beside a panel the whole strip
/// in the panel's color and, while active, an accent line along the edge it
/// shares with the code.
pub fn paint(
    bounds: Rectangle,
    vertical: bool,
    panel: Panel,
    active: bool,
    colors: Colors,
) -> Vec<(Rectangle, Color)> {
    // A line across the strip at `at` (an offset along its thickness).
    let line = |at: f32, width: f32| {
        if vertical {
            Rectangle {
                x: bounds.x + at,
                y: bounds.y,
                width,
                height: bounds.height,
            }
        } else {
            Rectangle {
                x: bounds.x,
                y: bounds.y + at,
                width: bounds.width,
                height: width,
            }
        }
    };
    let thickness = if vertical {
        bounds.width
    } else {
        bounds.height
    };
    match panel {
        Panel::Within => {
            let color = if active { colors.accent } else { colors.line };
            vec![(line(thickness / 2.0 - 0.5, 1.0), color)]
        }
        Panel::Seamless if active => vec![(line(thickness / 2.0 - 0.5, 1.0), colors.accent)],
        Panel::Seamless => Vec::new(),
        Panel::Before | Panel::After => {
            let mut out = vec![(bounds, colors.panel)];
            if active {
                let at = match panel {
                    Panel::Before => thickness - ACTIVE_LINE,
                    _ => 0.0,
                };
                out.push((line(at, ACTIVE_LINE), colors.accent));
            }
            out
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Divider<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(divider: Divider<'a, Message>) -> Self {
        Self::new(divider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: Colors = Colors {
        panel: Color::from_rgb(0.9, 0.9, 0.9),
        line: Color::from_rgb(0.8, 0.8, 0.8),
        accent: Color::from_rgb(0.2, 0.4, 0.9),
    };

    fn strip(vertical: bool) -> Rectangle {
        if vertical {
            Rectangle {
                x: 100.0,
                y: 0.0,
                width: THICKNESS,
                height: 500.0,
            }
        } else {
            Rectangle {
                x: 0.0,
                y: 300.0,
                width: 800.0,
                height: THICKNESS,
            }
        }
    }

    /// Beside a panel, at rest, the strip is the panel's own color and draws
    /// no line: the panel's edge is the only border.
    #[test]
    fn beside_a_panel_the_strip_is_the_panel_and_draws_no_line() {
        for vertical in [true, false] {
            for side in [Panel::Before, Panel::After] {
                let painted = paint(strip(vertical), vertical, side, false, COLORS);
                assert_eq!(painted, [(strip(vertical), COLORS.panel)]);
            }
        }
    }

    /// Hovered or dragged, the edge the panel shares with the code lights up:
    /// the strip's far side from the panel.
    #[test]
    fn an_active_edge_lights_up_where_the_panel_meets_the_code() {
        let left = paint(strip(true), true, Panel::Before, true, COLORS);
        assert_eq!(
            left[1],
            (
                Rectangle {
                    x: 100.0 + THICKNESS - ACTIVE_LINE,
                    y: 0.0,
                    width: ACTIVE_LINE,
                    height: 500.0
                },
                COLORS.accent
            )
        );
        let right = paint(strip(true), true, Panel::After, true, COLORS);
        assert_eq!(right[1].0.x, 100.0);
        let bottom = paint(strip(false), false, Panel::After, true, COLORS);
        assert_eq!(
            bottom[1],
            (
                Rectangle {
                    x: 0.0,
                    y: 300.0,
                    width: 800.0,
                    height: ACTIVE_LINE
                },
                COLORS.accent
            )
        );
    }

    /// Between two parts of one panel it keeps its hairline down the middle.
    #[test]
    fn within_a_panel_a_hairline_runs_down_the_middle() {
        let rest = paint(strip(false), false, Panel::Within, false, COLORS);
        assert_eq!(
            rest,
            [(
                Rectangle {
                    x: 0.0,
                    y: 300.0 + THICKNESS / 2.0 - 0.5,
                    width: 800.0,
                    height: 1.0
                },
                COLORS.line
            )]
        );
        let active = paint(strip(false), false, Panel::Within, true, COLORS);
        assert_eq!(active[0].1, COLORS.accent);
    }
}
