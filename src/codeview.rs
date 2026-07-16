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
    /// Block cursor position (0-based line, col) — drawn only when `Some`.
    cursor: Option<(usize, usize)>,
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
        cursor: Option<(usize, usize)>,
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
            cursor,
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
    /// True while Cmd/Ctrl is held — enables the go-to-definition affordance.
    cmd_held: bool,
    cache: RefCell<LineCache<P>>,
}

impl<P> Default for State<P> {
    fn default() -> Self {
        Self {
            char_width: 0.0,
            pressed: false,
            cmd_held: false,
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
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                // Track Cmd/Ctrl so draw can underline the hovered symbol.
                if state.cmd_held != m.command() {
                    state.cmd_held = m.command();
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position_in(bounds) else {
                    return;
                };
                state.pressed = true;
                let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                shell.publish((self.on_press)(hit));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.pressed {
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
                } else if state.cmd_held {
                    // Keep the symbol underline following the cursor.
                    shell.request_redraw();
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
        tree: &tree::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State<Renderer::Paragraph>>();
        match cursor.position_in(layout.bounds()) {
            // Cmd/Ctrl over the code text is "click to go to definition".
            Some(p) if state.cmd_held && p.x > GUTTER_CHARS as f32 * state.char_width => {
                mouse::Interaction::Pointer
            }
            Some(_) => mouse::Interaction::Text,
            None => mouse::Interaction::None,
        }
    }

    fn draw(
        &self,
        tree: &tree::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
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

            // Block cursor (Vim normal-mode style): a translucent cell so the
            // character under it still shows.
            if let Some((_, cc)) = self.cursor.filter(|(cl, _)| *cl == i) {
                let col_x = |c: usize| match paragraph.and_then(|p| p.grapheme_position(0, c)) {
                    Some(pt) => pt.x,
                    None => c as f32 * state.char_width,
                };
                let x0 = col_x(cc);
                let width = (col_x(cc + 1) - x0).max(state.char_width.max(2.0));
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: text_x0 + x0,
                            y,
                            width,
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::with_alpha(theme::ACCENT, 0.4),
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

        // Go-to-definition affordance: underline the symbol under the cursor
        // while Cmd/Ctrl is held.
        if state.cmd_held
            && let Some(p) = cursor.position_in(bounds)
            && p.x > gutter_px
        {
            let line = (p.y / lh) as usize;
            if let Some(paragraph) = line
                .checked_sub(cache.first)
                .and_then(|idx| cache.paragraphs.get(idx))
            {
                let col = paragraph
                    .hit_test(Point::new(p.x - gutter_px, lh * 0.5))
                    .map(|h| h.cursor())
                    .unwrap_or(0);
                if let Some((s, e)) = self.word_at(line, col) {
                    let cx = |c: usize| {
                        paragraph
                            .grapheme_position(0, c)
                            .map(|pt| pt.x)
                            .unwrap_or(c as f32 * state.char_width)
                    };
                    let y = bounds.y + line as f32 * lh;
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: text_x0 + cx(s),
                                y: y + lh - 2.0,
                                width: (cx(e) - cx(s)).max(1.0),
                                height: 1.0,
                            },
                            ..renderer::Quad::default()
                        },
                        theme::ACCENT,
                    );
                }
            }
        }
    }
}

impl<Message> CodeView<'_, Message> {
    /// Display-column range `[start, end)` of the identifier under `col` on
    /// `line`, or `None` when `col` is not on an identifier character.
    fn word_at(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        let text: String = self
            .lines
            .get(line)?
            .spans
            .iter()
            .map(|(t, _)| t.as_str())
            .collect();
        word_bounds(&text.chars().collect::<Vec<_>>(), col)
    }

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

/// Identifier range `[start, end)` around `col` in `chars`, or `None` when
/// `col` is not on an identifier character (`[A-Za-z0-9_]`).
fn word_bounds(chars: &[char], col: usize) -> Option<(usize, usize)> {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if col >= chars.len() || !is_word(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    Some((start, end))
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

#[cfg(test)]
mod tests {
    use super::word_bounds;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn word_bounds_finds_identifier() {
        // "    let origin = 1;" — 'o' of origin is at column 8.
        let c = chars("    let origin = 1;");
        assert_eq!(word_bounds(&c, 8), Some((8, 14))); // "origin"
        assert_eq!(word_bounds(&c, 11), Some((8, 14))); // mid-word
        assert_eq!(word_bounds(&c, 4), Some((4, 7))); // "let"
    }

    #[test]
    fn word_bounds_none_on_whitespace_or_punct() {
        let c = chars("a + b");
        assert_eq!(word_bounds(&c, 1), None); // space
        assert_eq!(word_bounds(&c, 2), None); // '+'
        assert_eq!(word_bounds(&c, 99), None); // past end
    }

    #[test]
    fn word_bounds_includes_underscore_and_digits() {
        let c = chars("foo_bar2");
        assert_eq!(word_bounds(&c, 0), Some((0, 8)));
    }
}
