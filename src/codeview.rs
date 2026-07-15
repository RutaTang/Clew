//! `CodeView` — a custom iced widget that renders the read-only code buffer.
//!
//! Why a custom widget rather than a column of `rich_text` rows: only a real
//! widget gets the `Layout` and cursor together, which is what character-level
//! hit testing (click → (line, column)) requires. It also virtualizes properly
//! (drawing only the visible lines inside its own `draw`) and gives precise
//! control over the gutter, per-line backgrounds and bookmark markers.
//!
//! The widget sizes itself to the full content (`lines * line_height` tall) and
//! lives inside a `scrollable`, so scrolling, scrollbars and `scroll_to` are
//! handled by iced. `draw` only paints the lines intersecting the viewport.
//!
//! Rendered line paragraphs are cached in the widget's tree `State`. This is
//! required, not an optimization: the renderer keeps only a weak reference to a
//! paragraph, so it must outlive the whole frame — a paragraph built as a local
//! in `draw` would be dropped before wgpu's render phase and never appear.

use std::cell::RefCell;

use iced::advanced::text::paragraph::Plain;
use iced::advanced::text::{self, Paragraph as _, Span, Text};
use iced::advanced::widget::{Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use iced::{Color, Element, Event, Font, Length, Point, Rectangle, Size};

use crate::highlight::{HlLine, style_color};
use crate::theme;

/// Gutter width in characters: `{:>5}` line number + two spaces.
const GUTTER_CHARS: usize = 7;
const OVERSCAN: usize = 8;
/// Finite layout width for a single unwrapped line. Large enough for any line,
/// but not infinite — the text shaper does not lay out with an infinite width.
const LINE_LAYOUT_WIDTH: f32 = 1.0e6;

/// A click resolved to a 0-based line and 0-based display column.
type Hit = (usize, usize);

pub struct CodeView<'a, Message> {
    lines: &'a [HlLine],
    max_cols: usize,
    font_size: f32,
    line_height: f32,
    default_color: Color,
    target_line: Option<usize>, // 1-based
    /// Ordered char selection: ((start line, start col), (end line, end col)).
    selection: Option<((usize, usize), (usize, usize))>,
    bookmarks: std::collections::HashSet<usize>, // 1-based bookmarked lines
    on_press: Box<dyn Fn(Hit) -> Message + 'a>,
    on_drag: Box<dyn Fn(Hit) -> Message + 'a>,
}

impl<'a, Message> CodeView<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lines: &'a [HlLine],
        max_cols: usize,
        font_size: f32,
        line_height: f32,
        default_color: Color,
        target_line: Option<usize>,
        selection: Option<((usize, usize), (usize, usize))>,
        bookmarks: std::collections::HashSet<usize>,
        on_press: impl Fn(Hit) -> Message + 'a,
        on_drag: impl Fn(Hit) -> Message + 'a,
    ) -> Self {
        Self {
            lines,
            max_cols,
            font_size,
            line_height,
            default_color,
            target_line,
            selection,
            bookmarks,
            on_press: Box::new(on_press),
            on_drag: Box::new(on_drag),
        }
    }

    fn total_height(&self) -> f32 {
        self.lines.len() as f32 * self.line_height
    }

    /// Colored spans for one line, used both to build paragraphs and hit-test.
    fn line_spans(&self, i: usize) -> Vec<Span<'_, (), Font>> {
        self.lines[i]
            .spans
            .iter()
            .map(|(fragment, style)| {
                let color = style.and_then(style_color).unwrap_or(self.default_color);
                Span::new(fragment.as_str()).color(color)
            })
            .collect()
    }

    fn line_text<'s>(&self, spans: &'s [Span<'s, (), Font>]) -> Text<&'s [Span<'s, (), Font>], Font> {
        Text {
            content: spans,
            bounds: Size::new(LINE_LAYOUT_WIDTH, self.line_height),
            size: self.font_size.into(),
            line_height: text::LineHeight::Absolute(self.line_height.into()),
            font: Font::MONOSPACE,
            align_x: text::Alignment::Left,
            align_y: iced::alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
        }
    }
}

/// Content identity for the paragraph cache: reallocation of the lines buffer,
/// a different line count, or a font-size change all invalidate it.
type CacheKey = (usize, usize, u32);

/// Cached shaped paragraphs for the currently visible line range.
struct LineCache<P> {
    key: CacheKey,
    first: usize,
    paragraphs: Vec<P>,
}

impl<P> Default for LineCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0),
            first: 0,
            paragraphs: Vec::new(),
        }
    }
}

/// Per-widget state: measured monospace advance, drag flag, paragraph cache.
struct State<P> {
    char_width: f32,
    pressed: bool,
    cache: RefCell<LineCache<P>>,
}

impl<P> Default for State<P> {
    fn default() -> Self {
        Self {
            char_width: 0.0,
            pressed: false,
            cache: RefCell::new(LineCache::default()),
        }
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for CodeView<'_, Message>
where
    Renderer: text::Renderer<Font = Font>,
    Renderer::Paragraph: 'static,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State<Renderer::Paragraph>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::<Renderer::Paragraph>::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut tree::Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        // Measure one monospace glyph once and cache it in state.
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        state.char_width = measure_char_width::<Renderer>(self.font_size);

        let width = (GUTTER_CHARS + self.max_cols + 1) as f32 * state.char_width;
        layout::Node::new(Size::new(width, self.total_height().max(self.line_height)))
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
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        let bounds = layout.bounds();

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position_in(bounds) else {
                    return;
                };
                state.pressed = true;
                let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                shell.publish((self.on_press)(hit));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.pressed => {
                // Clamp to the widget so a drag past the edges keeps selecting.
                if let Some(point) = cursor.position().map(|p| {
                    Point::new(
                        (p.x - bounds.x).clamp(0.0, bounds.width),
                        (p.y - bounds.y).clamp(0.0, bounds.height),
                    )
                }) {
                    let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                    shell.publish((self.on_drag)(hit));
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.pressed = false;
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &tree::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::None
        }
    }

    fn draw(
        &self,
        tree: &tree::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        let bounds = layout.bounds();
        let lh = self.line_height;
        let gutter_px = GUTTER_CHARS as f32 * state.char_width;

        // Visible line range relative to the content top.
        let top = (viewport.y - bounds.y).max(0.0);
        let first = ((top / lh) as usize).saturating_sub(OVERSCAN);
        let visible = (viewport.height / lh).ceil() as usize + OVERSCAN * 2;
        let last = (first + visible).min(self.lines.len());

        // Refresh the paragraph cache for the visible range if needed. Held in
        // tree state so the renderer's weak references stay valid this frame.
        let key: CacheKey = (
            self.lines.as_ptr() as usize,
            self.lines.len(),
            self.font_size.to_bits(),
        );
        {
            let mut cache = state.cache.borrow_mut();
            if cache.key != key || cache.first != first || cache.paragraphs.len() != last - first {
                cache.key = key;
                cache.first = first;
                cache.paragraphs = (first..last)
                    .map(|i| {
                        let spans = self.line_spans(i);
                        Renderer::Paragraph::with_spans(self.line_text(&spans))
                    })
                    .collect();
            }
        }

        let cache = state.cache.borrow();
        let text_x0 = bounds.x + gutter_px;
        for i in first..last {
            let y = bounds.y + i as f32 * lh;
            let paragraph = cache.paragraphs.get(i - cache.first);

            // Jump-target line: full-width background.
            if self.target_line == Some(i + 1) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y,
                            width: bounds.width,
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::BG_TARGET,
                );
            }

            // Character-level selection background for this line.
            if let Some((x0, x1)) = self.selection_span(i, paragraph, state.char_width) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: text_x0 + x0,
                            y,
                            width: (x1 - x0).max(1.0),
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::rgb(0x2d3a55),
                );
            }

            // Gutter line number (owned text; rendered directly).
            let gutter_color = if self.bookmarks.contains(&(i + 1)) {
                theme::ACCENT
            } else {
                theme::DIM
            };
            renderer.fill_text(
                text::Text {
                    content: format!("{:>5}", i + 1),
                    bounds: Size::new(gutter_px, lh),
                    size: self.font_size.into(),
                    line_height: text::LineHeight::Absolute(lh.into()),
                    font: Font::MONOSPACE,
                    align_x: text::Alignment::Left,
                    align_y: iced::alignment::Vertical::Top,
                    shaping: text::Shaping::Basic,
                    wrapping: text::Wrapping::None,
                },
                Point::new(bounds.x, y),
                gutter_color,
                *viewport,
            );

            // Code text: a cached, shaped paragraph of colored spans.
            if let Some(paragraph) = paragraph {
                renderer.fill_paragraph(
                    paragraph,
                    Point::new(text_x0, y),
                    style.text_color,
                    *viewport,
                );
            }
        }
    }
}

impl<Message> CodeView<'_, Message> {
    /// Horizontal span `(x0, x1)` (relative to the text origin) of the selection
    /// on line `i`, or `None` when the line is outside the selection. Column
    /// x-positions come from the shaped paragraph, so they are glyph-accurate.
    fn selection_span<P: text::Paragraph>(
        &self,
        i: usize,
        paragraph: Option<&P>,
        char_width: f32,
    ) -> Option<(f32, f32)> {
        let ((sl, sc), (el, ec)) = self.selection?;
        if i < sl || i > el {
            return None;
        }
        let line_end = paragraph.map(|p| p.min_bounds().width).unwrap_or(0.0);
        let col_x = |col: usize| -> f32 {
            match paragraph.and_then(|p| p.grapheme_position(0, col)) {
                Some(point) => point.x,
                None => col as f32 * char_width, // past line end / no paragraph
            }
        };
        let x0 = if i == sl { col_x(sc) } else { 0.0 };
        let x1 = if i == el {
            col_x(ec)
        } else {
            // Continuation lines extend to the text end, with a small sliver so
            // selected empty lines are still visible.
            line_end.max(char_width * 0.5)
        };
        Some((x0, x1.max(x0)))
    }

    /// Resolve a widget-local point to a (line, display column). Generic over
    /// the paragraph type so it matches whatever renderer the widget runs under.
    fn hit<P: text::Paragraph<Font = Font>>(&self, point: Point, char_width: f32) -> Hit {
        let line = ((point.y / self.line_height) as usize).min(self.lines.len().saturating_sub(1));
        let gutter_px = GUTTER_CHARS as f32 * char_width;
        let text_x = point.x - gutter_px;
        if text_x <= 0.0 || self.lines.get(line).is_none() {
            return (line, 0);
        }
        // Glyph-accurate hit test against the actual line paragraph so wide
        // (e.g. CJK) characters resolve to the correct column. A temporary
        // paragraph is fine here: hit_test is pure geometry, no rendering.
        let spans = self.line_spans(line);
        let para = P::with_spans(self.line_text(&spans));
        let col = para
            .hit_test(Point::new(text_x, self.line_height * 0.5))
            .map(|h| h.cursor())
            .unwrap_or(0);
        (line, col)
    }
}

/// Measure the advance width of one monospace glyph at `font_size`.
/// Paragraph shaping is done by the associated type, so no renderer instance
/// is needed — the type parameter only selects the paragraph implementation.
fn measure_char_width<Renderer>(font_size: f32) -> f32
where
    Renderer: text::Renderer<Font = Font>,
{
    let sample = Plain::<Renderer::Paragraph>::new(Text {
        content: "0".to_string(),
        bounds: Size::INFINITE,
        size: font_size.into(),
        line_height: text::LineHeight::Absolute(font_size.into()),
        font: Font::MONOSPACE,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    });
    let w = sample.min_bounds().width;
    if w > 0.0 { w } else { font_size * 0.6 }
}

impl<'a, Message: 'a> From<CodeView<'a, Message>> for Element<'a, Message> {
    fn from(view: CodeView<'a, Message>) -> Self {
        Self::new(view)
    }
}
