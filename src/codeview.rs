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
use std::collections::HashSet;

use iced::advanced::text::paragraph::Plain;
use iced::advanced::text::{self, Paragraph as _, Span, Text};
use iced::advanced::widget::{Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use iced::{Color, Element, Event, Font, Length, Point, Rectangle, Size};

use crate::highlight::{HlLine, style_color};
use crate::theme;

/// Gutter width in characters: `{:>5}` line number + two spaces.
const GUTTER_CHARS: usize = 7;
/// Column (within the gutter) where the fold arrow is drawn; the two trailing
/// gutter spaces double as its click target.
const FOLD_ARROW_COL: usize = 5;
const OVERSCAN: usize = 8;
/// Finite layout width for a single unwrapped line. Large enough for any line,
/// but not infinite — the text shaper does not lay out with an infinite width.
const LINE_LAYOUT_WIDTH: f32 = 1.0e6;

/// A click resolved to a 0-based line and 0-based display column.
type Hit = (usize, usize);

/// A highlighted span within one line, for find matches / occurrences /
/// brackets. Columns are 0-based display columns, `[col0, col1)`.
#[derive(Debug, Clone, Copy)]
pub struct Hl {
    pub line: usize,
    pub col0: usize,
    pub col1: usize,
    pub kind: HlKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HlKind {
    /// A search match (in-file find).
    FindMatch,
    /// The current/active search match.
    FindCurrent,
    /// Another occurrence of the identifier under the cursor.
    Occurrence,
    /// A matched bracket pair.
    Bracket,
    /// Diagnostic underline (drawn as an underline, not a fill).
    DiagError,
    DiagWarn,
    DiagHint,
}

impl HlKind {
    fn is_underline(self) -> bool {
        matches!(self, HlKind::DiagError | HlKind::DiagWarn | HlKind::DiagHint)
    }
}

pub struct CodeView<'a, Message> {
    lines: &'a [HlLine],
    max_cols: usize,
    font_size: f32,
    line_height: f32,
    default_color: Color,
    /// Ordered char selection: ((start line, start col), (end line, end col)).
    selection: Option<((usize, usize), (usize, usize))>,
    /// Block cursor position (0-based line, col) — drawn only when `Some`.
    cursor: Option<(usize, usize)>,
    /// Extra span highlights (find / occurrences / brackets).
    highlights: Vec<Hl>,
    /// Enclosing header lines pinned at the top (sticky scroll).
    sticky: Vec<usize>,
    bookmarks: HashSet<usize>, // 1-based bookmarked lines
    /// Row → source-line projection when folds are collapsed; `None` is the
    /// identity mapping (row == line).
    visible: Option<&'a [usize]>,
    /// Lines that head a foldable region (for drawing the gutter arrow).
    fold_headers: Option<&'a HashSet<usize>>,
    /// Collapsed fold headers (arrow points right, body hidden).
    collapsed: Option<&'a HashSet<usize>>,
    on_press: Box<dyn Fn(Hit) -> Message + 'a>,
    on_drag: Box<dyn Fn(Hit) -> Message + 'a>,
    /// Right-click: (line, col) hit + window point to place a context menu.
    on_context: Box<dyn Fn(Hit, Point) -> Message + 'a>,
    /// Cmd-hover over a new token: (line, col) hit + window point (for hover).
    on_hover: Option<Box<dyn Fn(Hit, Point) -> Message + 'a>>,
    /// Gutter fold-arrow click on a header line.
    on_fold: Option<Box<dyn Fn(usize) -> Message + 'a>>,
}

impl<'a, Message> CodeView<'a, Message> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lines: &'a [HlLine],
        max_cols: usize,
        font_size: f32,
        line_height: f32,
        default_color: Color,
        on_press: impl Fn(Hit) -> Message + 'a,
        on_drag: impl Fn(Hit) -> Message + 'a,
        on_context: impl Fn(Hit, Point) -> Message + 'a,
    ) -> Self {
        Self {
            lines,
            max_cols,
            font_size,
            line_height,
            default_color,
            selection: None,
            cursor: None,
            highlights: Vec::new(),
            sticky: Vec::new(),
            bookmarks: HashSet::new(),
            visible: None,
            fold_headers: None,
            collapsed: None,
            on_press: Box::new(on_press),
            on_drag: Box::new(on_drag),
            on_context: Box::new(on_context),
            on_hover: None,
            on_fold: None,
        }
    }

    pub fn on_hover(mut self, f: impl Fn(Hit, Point) -> Message + 'a) -> Self {
        self.on_hover = Some(Box::new(f));
        self
    }

    pub fn on_fold(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_fold = Some(Box::new(f));
        self
    }

    /// Folding inputs: the row→line projection (`None` when nothing is folded),
    /// the set of foldable header lines, and which of them are collapsed.
    pub fn folds(
        mut self,
        visible: Option<&'a [usize]>,
        headers: &'a HashSet<usize>,
        collapsed: &'a HashSet<usize>,
    ) -> Self {
        self.visible = visible;
        self.fold_headers = Some(headers);
        self.collapsed = Some(collapsed);
        self
    }

    pub fn selection(mut self, sel: Option<((usize, usize), (usize, usize))>) -> Self {
        self.selection = sel;
        self
    }

    pub fn cursor(mut self, cursor: Option<(usize, usize)>) -> Self {
        self.cursor = cursor;
        self
    }

    pub fn highlights(mut self, highlights: Vec<Hl>) -> Self {
        self.highlights = highlights;
        self
    }

    pub fn sticky(mut self, sticky: Vec<usize>) -> Self {
        self.sticky = sticky;
        self
    }

    pub fn bookmarks(mut self, bookmarks: HashSet<usize>) -> Self {
        self.bookmarks = bookmarks;
        self
    }

    /// Number of displayed rows (folded-away lines excluded).
    fn row_count(&self) -> usize {
        match self.visible {
            Some(v) => v.len(),
            None => self.lines.len(),
        }
    }

    /// Source line shown at display `row`, if any.
    fn line_at_row(&self, row: usize) -> Option<usize> {
        match self.visible {
            Some(v) => v.get(row).copied(),
            None => (row < self.lines.len()).then_some(row),
        }
    }

    fn is_fold_header(&self, line: usize) -> bool {
        self.fold_headers.is_some_and(|h| h.contains(&line))
    }

    fn is_collapsed(&self, line: usize) -> bool {
        self.collapsed.is_some_and(|c| c.contains(&line))
    }

    fn total_height(&self) -> f32 {
        self.row_count() as f32 * self.line_height
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
/// a different line count, a font-size change, or a change to the fold
/// projection (different `visible` allocation) all invalidate it.
type CacheKey = (usize, usize, u32, usize);

/// Cached shaped paragraphs for the currently visible line range.
struct LineCache<P> {
    key: CacheKey,
    first: usize,
    paragraphs: Vec<P>,
}

impl<P> Default for LineCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0, 0),
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
    /// Last (line, col) a Cmd-hover was reported for, to debounce hover.
    last_hover: Option<Hit>,
    /// Row the mouse currently hovers, to reveal the fold arrow on that line.
    hover_row: Option<usize>,
    cache: RefCell<LineCache<P>>,
}

impl<P> Default for State<P> {
    fn default() -> Self {
        Self {
            char_width: 0.0,
            pressed: false,
            cmd_held: false,
            last_hover: None,
            hover_row: None,
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
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<State<Renderer::Paragraph>>();
        let bounds = layout.bounds();

        // A click on the pinned sticky region is swallowed (it covers, but does
        // not belong to, the scrolled line underneath).
        if let Event::Mouse(mouse::Event::ButtonPressed(_)) = event
            && !self.sticky.is_empty()
            && let Some(abs) = cursor.position()
            && abs.y >= viewport.y
            && abs.y < viewport.y + self.sticky.len() as f32 * self.line_height
            && abs.x >= bounds.x
        {
            shell.capture_event();
            return;
        }

        match event {
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m)) => {
                // Track Cmd/Ctrl so draw can underline the hovered symbol.
                if state.cmd_held != m.command() {
                    state.cmd_held = m.command();
                    if !state.cmd_held {
                        state.last_hover = None;
                    }
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = cursor.position_in(bounds) else {
                    return;
                };
                // A click on a fold arrow toggles the fold instead of moving
                // the cursor. The arrow lives in the trailing gutter columns.
                let arrow_x0 = FOLD_ARROW_COL as f32 * state.char_width;
                let gutter_px = GUTTER_CHARS as f32 * state.char_width;
                if let Some(on_fold) = &self.on_fold
                    && point.x >= arrow_x0
                    && point.x < gutter_px
                {
                    let row = (point.y / self.line_height) as usize;
                    if let Some(line) = self.line_at_row(row)
                        && self.is_fold_header(line)
                    {
                        shell.publish(on_fold(line));
                        shell.capture_event();
                        return;
                    }
                }
                state.pressed = true;
                let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                shell.publish((self.on_press)(hit));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // Track the hovered row so the fold arrow can appear on it.
                if self.fold_headers.is_some()
                    && let Some(p) = cursor.position_in(bounds)
                {
                    let row = Some((p.y / self.line_height) as usize);
                    if state.hover_row != row {
                        state.hover_row = row;
                        shell.request_redraw();
                    }
                }
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
                    // Keep the symbol underline following the cursor, and ask
                    // for hover when the token under it changes.
                    shell.request_redraw();
                    if let (Some(on_hover), Some(point)) = (&self.on_hover, cursor.position_in(bounds))
                    {
                        let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                        if state.last_hover != Some(hit) {
                            state.last_hover = Some(hit);
                            let at = cursor.position().unwrap_or(Point::new(bounds.x, bounds.y));
                            shell.publish(on_hover(hit, at));
                        }
                    }
                } else {
                    state.last_hover = None;
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.pressed = false;
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
                let Some(local) = cursor.position_in(bounds) else {
                    return;
                };
                let hit = self.hit::<Renderer::Paragraph>(local, state.char_width);
                // Window point for placing the menu.
                let at = cursor.position().unwrap_or(Point::new(bounds.x, bounds.y));
                shell.publish((self.on_context)(hit, at));
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

        // Visible row range relative to the content top (rows, not source
        // lines — collapsed folds compress the vertical space).
        let top = (viewport.y - bounds.y).max(0.0);
        let first = ((top / lh) as usize).saturating_sub(OVERSCAN);
        let visible = (viewport.height / lh).ceil() as usize + OVERSCAN * 2;
        let last = (first + visible).min(self.row_count());

        // Refresh the paragraph cache for the visible range if needed. Held in
        // tree state so the renderer's weak references stay valid this frame.
        // `first` is a row index; the projection token invalidates on fold.
        let key: CacheKey = (
            self.lines.as_ptr() as usize,
            self.lines.len(),
            self.font_size.to_bits(),
            self.visible.map(|v| v.as_ptr() as usize).unwrap_or(0),
        );
        {
            let mut cache = state.cache.borrow_mut();
            if cache.key != key || cache.first != first || cache.paragraphs.len() != last - first {
                cache.key = key;
                cache.first = first;
                cache.paragraphs = (first..last)
                    .map(|row| {
                        let line = self.line_at_row(row).unwrap_or(0);
                        let spans = self.line_spans(line);
                        Renderer::Paragraph::with_spans(self.line_text(&spans))
                    })
                    .collect();
            }
        }

        let cache = state.cache.borrow();
        let text_x0 = bounds.x + gutter_px;
        for row in first..last {
            let Some(i) = self.line_at_row(row) else {
                continue;
            };
            let y = bounds.y + row as f32 * lh;
            let paragraph = cache.paragraphs.get(row - cache.first);

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

            // Extra span highlights on this line (find / occurrences / bracket).
            for hl in self.highlights.iter().filter(|h| h.line == i) {
                let col_x = |c: usize| match paragraph.and_then(|p| p.grapheme_position(0, c)) {
                    Some(pt) => pt.x,
                    None => c as f32 * state.char_width,
                };
                let x0 = col_x(hl.col0);
                let x1 = col_x(hl.col1);
                let color = match hl.kind {
                    HlKind::FindCurrent => theme::with_alpha(theme::rgb(0xe5c07b), 0.55),
                    HlKind::FindMatch => theme::with_alpha(theme::rgb(0xe5c07b), 0.28),
                    HlKind::Occurrence => theme::with_alpha(theme::FG, 0.16),
                    HlKind::Bracket => theme::with_alpha(theme::ACCENT, 0.35),
                    HlKind::DiagError => theme::rgb(0xe06c75),
                    HlKind::DiagWarn => theme::rgb(0xe5c07b),
                    HlKind::DiagHint => theme::rgb(0x56b6c2),
                };
                // Diagnostics underline; everything else fills the cell.
                let bounds = if hl.kind.is_underline() {
                    Rectangle {
                        x: text_x0 + x0,
                        y: y + lh - 2.0,
                        width: (x1 - x0).max(2.0),
                        height: 2.0,
                    }
                } else {
                    Rectangle {
                        x: text_x0 + x0,
                        y,
                        width: (x1 - x0).max(2.0),
                        height: lh,
                    }
                };
                renderer.fill_quad(
                    renderer::Quad {
                        bounds,
                        ..renderer::Quad::default()
                    },
                    color,
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

            // Fold arrow: collapsed headers always show ▸; expanded headers
            // show ▾ only under the mouse, to keep the gutter quiet.
            if self.is_fold_header(i) {
                let collapsed = self.is_collapsed(i);
                if collapsed || state.hover_row == Some(row) {
                    renderer.fill_text(
                        text::Text {
                            content: if collapsed { "▸" } else { "▾" }.to_string(),
                            bounds: Size::new(2.0 * state.char_width, lh),
                            size: self.font_size.into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: text::Shaping::Advanced,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(bounds.x + FOLD_ARROW_COL as f32 * state.char_width, y),
                        if collapsed { theme::ACCENT } else { theme::DIM },
                        *viewport,
                    );
                }
            }

            // Code text: a cached, shaped paragraph of colored spans.
            if let Some(paragraph) = paragraph {
                renderer.fill_paragraph(
                    paragraph,
                    Point::new(text_x0, y),
                    style.text_color,
                    *viewport,
                );
                // Collapsed cue: a dim ⋯ after the header line's end.
                if self.is_collapsed(i) {
                    let end_x = text_x0 + paragraph.min_bounds().width + state.char_width;
                    renderer.fill_text(
                        text::Text {
                            content: "⋯".to_string(),
                            bounds: Size::new(2.0 * state.char_width, lh),
                            size: self.font_size.into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: text::Shaping::Advanced,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(end_x, y),
                        theme::DIM,
                        *viewport,
                    );
                }
            }
        }

        // Go-to-definition affordance: underline the symbol under the cursor
        // while Cmd/Ctrl is held.
        if state.cmd_held
            && let Some(p) = cursor.position_in(bounds)
            && p.x > gutter_px
        {
            let row = (p.y / lh) as usize;
            if let Some(line) = self.line_at_row(row)
                && let Some(paragraph) = row
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
                    let y = bounds.y + row as f32 * lh;
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

        // Sticky scroll: pin the enclosing headers at the very top. Drawn last
        // so they cover the scrolled content, via owned fill_text (no cache).
        if !self.sticky.is_empty() {
            let n = self.sticky.len();
            let sticky_h = n as f32 * lh;
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x,
                        y: viewport.y,
                        width: viewport.width,
                        height: sticky_h,
                    },
                    ..renderer::Quad::default()
                },
                theme::BG_PANEL,
            );
            for (k, &line) in self.sticky.iter().enumerate() {
                let y = viewport.y + k as f32 * lh;
                renderer.fill_text(
                    text::Text {
                        content: format!("{:>5}", line + 1),
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
                    theme::DIM,
                    *viewport,
                );
                // Draw the line's colored spans with monospace offsets.
                let mut x = text_x0;
                if let Some(hl) = self.lines.get(line) {
                    for (frag, sty) in &hl.spans {
                        let color = sty.and_then(style_color).unwrap_or(self.default_color);
                        renderer.fill_text(
                            text::Text {
                                content: frag.clone(),
                                bounds: Size::new(f32::MAX, lh),
                                size: self.font_size.into(),
                                line_height: text::LineHeight::Absolute(lh.into()),
                                font: Font::MONOSPACE,
                                align_x: text::Alignment::Left,
                                align_y: iced::alignment::Vertical::Top,
                                shaping: text::Shaping::Basic,
                                wrapping: text::Wrapping::None,
                            },
                            Point::new(x, y),
                            color,
                            *viewport,
                        );
                        x += frag.chars().count() as f32 * state.char_width;
                    }
                }
            }
            // Separator line under the sticky region.
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x,
                        y: viewport.y + sticky_h - 1.0,
                        width: viewport.width,
                        height: 1.0,
                    },
                    ..renderer::Quad::default()
                },
                theme::BORDER,
            );
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
        let row = (point.y / self.line_height) as usize;
        let line = self
            .line_at_row(row)
            .unwrap_or(self.lines.len().saturating_sub(1));
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
