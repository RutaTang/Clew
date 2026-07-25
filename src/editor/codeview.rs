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
use std::collections::{HashMap, HashSet};

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
/// Minimap band width, and the smallest file (in rows) worth showing one for.
const MINIMAP_WIDTH: f32 = 68.0;
const MINIMAP_MIN_ROWS: usize = 40;
/// Display columns a full-width minimap bar represents.
const MINIMAP_FULL_COLS: f32 = 100.0;
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
        matches!(
            self,
            HlKind::DiagError | HlKind::DiagWarn | HlKind::DiagHint
        )
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
    bookmarks: HashSet<usize>,        // 1-based bookmarked lines
    breakpoints: HashSet<usize>,      // 1-based lines with a debug breakpoint
    cond_breakpoints: HashSet<usize>, // subset that are conditional (drawn amber)
    debug_current: Option<usize>,     // 1-based current stopped line (debug)
    /// 1-based line → a one-line LLM summary, shown dim past the line's end so
    /// you read a function together with what it does (assisted reading).
    summaries: HashMap<usize, String>,
    /// 0-based display line → inlay chips `(display column, label)` spliced into
    /// the line at render time (inferred types, parameter names).
    inlay_hints: HashMap<usize, Vec<(usize, String)>>,
    /// Colour for inlay-hint text (dim, so it reads as annotation not code).
    inlay_color: Color,
    /// 0-based lines gated off by an inactive `#[cfg]`, drawn dimmed.
    inactive: HashSet<usize>,
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
    /// Hover over a new token: (line, col) hit + window point (for the peek).
    on_hover: Option<Box<dyn Fn(Hit, Point) -> Message + 'a>>,
    /// The cursor left the code area — clear any open peek.
    on_hover_end: Option<Box<dyn Fn() -> Message + 'a>>,
    /// Gutter fold-arrow click on a header line.
    on_fold: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    /// Click in the line-number gutter margin: toggle a breakpoint on that
    /// 1-based line (the conventional editor gesture).
    on_breakpoint: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    /// Draw vertical indentation guides.
    indent_guides: bool,
    /// Minimap click/drag: the fraction `[0,1]` of the content to scroll to.
    on_minimap: Option<Box<dyn Fn(f32) -> Message + 'a>>,
    /// Per-line git change status for the gutter bar.
    git_status: Option<&'a [Option<crate::git::ChangeKind>]>,
    /// Lines below which git shows deleted content (gutter marker).
    git_deleted: Option<&'a HashSet<usize>>,
    /// Inline blame for the caret line: (line, formatted annotation).
    blame: Option<(usize, String)>,
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
            breakpoints: HashSet::new(),
            cond_breakpoints: HashSet::new(),
            debug_current: None,
            summaries: HashMap::new(),
            inlay_hints: HashMap::new(),
            inlay_color: default_color,
            inactive: HashSet::new(),
            visible: None,
            fold_headers: None,
            collapsed: None,
            on_press: Box::new(on_press),
            on_drag: Box::new(on_drag),
            on_context: Box::new(on_context),
            on_hover: None,
            on_hover_end: None,
            on_fold: None,
            on_breakpoint: None,
            indent_guides: false,
            on_minimap: None,
            git_status: None,
            git_deleted: None,
            blame: None,
        }
    }

    /// Git gutter inputs: per-line change status and deleted-below markers.
    pub fn git_gutter(mut self, git: Option<&'a crate::git::GitInfo>) -> Self {
        if let Some(g) = git {
            self.git_status = Some(&g.status);
            self.git_deleted = Some(&g.deleted_at);
        }
        self
    }

    /// Inline blame annotation for the caret line.
    pub fn blame(mut self, blame: Option<(usize, String)>) -> Self {
        self.blame = blame;
        self
    }

    pub fn on_hover_end(mut self, f: impl Fn() -> Message + 'a) -> Self {
        self.on_hover_end = Some(Box::new(f));
        self
    }

    pub fn on_hover(mut self, f: impl Fn(Hit, Point) -> Message + 'a) -> Self {
        self.on_hover = Some(Box::new(f));
        self
    }

    pub fn indent_guides(mut self, on: bool) -> Self {
        self.indent_guides = on;
        self
    }

    pub fn on_minimap(mut self, f: impl Fn(f32) -> Message + 'a) -> Self {
        self.on_minimap = Some(Box::new(f));
        self
    }

    pub fn on_fold(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_fold = Some(Box::new(f));
        self
    }

    pub fn on_breakpoint(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_breakpoint = Some(Box::new(f));
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

    /// Debug breakpoints on this file (1-based lines) — drawn as gutter dots.
    pub fn breakpoints(mut self, breakpoints: HashSet<usize>) -> Self {
        self.breakpoints = breakpoints;
        self
    }

    /// The subset of breakpoints that are conditional (drawn amber, not red).
    pub fn cond_breakpoints(mut self, cond: HashSet<usize>) -> Self {
        self.cond_breakpoints = cond;
        self
    }

    /// The current stopped line (1-based) while debugging — full-row highlight.
    pub fn debug_current(mut self, line: Option<usize>) -> Self {
        self.debug_current = line;
        self
    }

    /// Per-line one-line summaries (1-based) shown dim past each line's end.
    pub fn summaries(mut self, summaries: HashMap<usize, String>) -> Self {
        self.summaries = summaries;
        self
    }

    /// Inlay hints (0-based display line → `(display column, label)`), spliced
    /// into each line at render time in `color`.
    pub fn inlay_hints(
        mut self,
        hints: HashMap<usize, Vec<(usize, String)>>,
        color: Color,
    ) -> Self {
        self.inlay_hints = hints;
        self.inlay_color = color;
        self
    }

    /// 0-based lines gated off by an inactive `#[cfg]`, drawn dimmed.
    pub fn inactive(mut self, inactive: HashSet<usize>) -> Self {
        self.inactive = inactive;
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

    /// Display length (columns) of a source line.
    fn line_display_len(&self, line: usize) -> usize {
        self.lines
            .get(line)
            .map(|l| l.spans.iter().map(|(t, _)| t.chars().count()).sum())
            .unwrap_or(0)
    }

    /// The dominant syntax color of a line for the minimap: the color of the
    /// longest colored span, or the default foreground when the line is plain.
    fn line_minimap_color(&self, line: usize) -> Color {
        let Some(l) = self.lines.get(line) else {
            return self.default_color;
        };
        let mut best: Option<(usize, Color)> = None;
        for (frag, style) in &l.spans {
            if let Some(color) = style.and_then(style_color) {
                let len = frag.chars().count();
                if best.is_none_or(|(n, _)| len > n) {
                    best = Some((len, color));
                }
            }
        }
        best.map(|(_, c)| c).unwrap_or(self.default_color)
    }

    /// Leading-space indentation (columns) of a source line.
    fn line_indent(&self, line: usize) -> usize {
        let Some(l) = self.lines.get(line) else {
            return 0;
        };
        let mut n = 0;
        for (frag, _) in &l.spans {
            for c in frag.chars() {
                if c == ' ' {
                    n += 1;
                } else {
                    return n;
                }
            }
        }
        n
    }

    /// The minimap band rectangle pinned to the right of the viewport, or `None`
    /// when there is no minimap callback or the file is too short to warrant one.
    fn minimap_band(&self, viewport: &Rectangle) -> Option<Rectangle> {
        if self.on_minimap.is_none() || self.row_count() < MINIMAP_MIN_ROWS {
            return None;
        }
        let w = MINIMAP_WIDTH.min(viewport.width * 0.4);
        Some(Rectangle {
            x: viewport.x + viewport.width - w,
            y: viewport.y,
            width: w,
            height: viewport.height,
        })
    }

    fn is_collapsed(&self, line: usize) -> bool {
        self.collapsed.is_some_and(|c| c.contains(&line))
    }

    fn total_height(&self) -> f32 {
        self.row_count() as f32 * self.line_height
    }

    /// Colored spans for one line, used both to build paragraphs and hit-test.
    fn line_spans(&self, i: usize) -> Vec<Span<'_, (), Font>> {
        let src = &self.lines[i].spans;
        let dim_line = self.inactive.contains(&i);
        let color_of = |style: &Option<u8>| {
            let c = style.and_then(style_color).unwrap_or(self.default_color);
            // Fade inactive-`cfg` code toward the background so live code stands out.
            if dim_line {
                Color { a: c.a * 0.38, ..c }
            } else {
                c
            }
        };
        let hints = self.inlay_hints.get(&i).filter(|h| !h.is_empty());
        let Some(hints) = hints else {
            // Fast path: no hints on this line, borrow the fragments directly.
            return src
                .iter()
                .map(|(fragment, style)| Span::new(fragment.as_str()).color(color_of(style)))
                .collect();
        };
        // Splice each chip into the styled spans at its display column. Tabs are
        // already expanded to spaces, so one char == one display column.
        let mut out: Vec<Span<'_, (), Font>> = Vec::new();
        let mut col = 0usize; // display column at the start of the current fragment
        let mut hi = 0usize; // next chip to place
        for (fragment, style) in src {
            let color = color_of(style);
            let flen = fragment.chars().count();
            let mut cut = 0usize; // chars of this fragment already emitted
            while hi < hints.len() {
                let (hcol, label) = &hints[hi];
                if *hcol < col + cut {
                    hi += 1; // stale / overlapping; skip
                    continue;
                }
                if *hcol >= col + flen {
                    break; // chip falls beyond this fragment
                }
                let within = *hcol - col;
                if within > cut {
                    let sub: String = fragment.chars().skip(cut).take(within - cut).collect();
                    out.push(Span::new(sub).color(color));
                }
                out.push(Span::new(label.clone()).color(self.inlay_color));
                cut = within;
                hi += 1;
            }
            if cut == 0 {
                out.push(Span::new(fragment.as_str()).color(color));
            } else if cut < flen {
                let sub: String = fragment.chars().skip(cut).collect();
                out.push(Span::new(sub).color(color));
            }
            col += flen;
        }
        // Chips past the end of the line's text render at the end.
        for (_, label) in &hints[hi..] {
            out.push(Span::new(label.clone()).color(self.inlay_color));
        }
        out
    }

    /// Grapheme index in the inlay-spliced paragraph for source display column
    /// `col` on `line`. Inlay-hint labels are spliced into the line, so a source
    /// column sits `label-length` graphemes further right for each hint before
    /// it — without this, span highlights (find/occurrence/bracket) drawn from
    /// source columns land left of the text on inlay lines. `inclusive` counts a
    /// hint sitting exactly at `col` (use it for a span's left edge, exclude it
    /// for the right edge so the highlight doesn't swallow a trailing chip).
    fn spliced_col(&self, line: usize, col: usize, inclusive: bool) -> usize {
        let shift: usize = self.inlay_hints.get(&line).map_or(0, |hints| {
            hints
                .iter()
                .filter(|(hcol, _)| if inclusive { *hcol <= col } else { *hcol < col })
                .map(|(_, label)| label.chars().count())
                .sum()
        });
        col + shift
    }

    /// An order-independent signature of the inlay hints and inactive lines, so
    /// the paragraph cache invalidates when either changes (both can land after
    /// the first render with the lines buffer unchanged, so the pointer-based key
    /// alone misses them).
    fn annotation_signature(&self) -> u64 {
        let inlay = self
            .inlay_hints
            .iter()
            .map(|(line, chips)| {
                let mut h = *line as u64;
                for (col, text) in chips {
                    h = h
                        .wrapping_mul(31)
                        .wrapping_add(*col as u64)
                        .wrapping_add(text.len() as u64);
                }
                h
            })
            .fold(0u64, |acc, h| acc.wrapping_add(h)); // sum: independent of order
        let inactive = self.inactive.iter().fold(0u64, |acc, &l| {
            acc.wrapping_add((l as u64 + 1).wrapping_mul(2654435761))
        });
        inlay.wrapping_add(inactive)
    }

    fn line_text<'s>(
        &self,
        spans: &'s [Span<'s, (), Font>],
    ) -> Text<&'s [Span<'s, (), Font>], Font> {
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
// (lines ptr, line count, font-size bits, fold-projection ptr, inlay signature).
type CacheKey = (usize, usize, u32, usize, u64);

/// Cached shaped paragraphs for the currently visible line range.
struct LineCache<P> {
    key: CacheKey,
    first: usize,
    paragraphs: Vec<P>,
}

impl<P> Default for LineCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0, 0, 0),
            first: 0,
            paragraphs: Vec::new(),
        }
    }
}

/// Cached shaped paragraphs for the pinned sticky-header lines. Rebuilt only
/// when the pinned set (or the underlying content / font) changes, so scrolling
/// with a header pinned never re-shapes it per frame — the source of the jank.
struct StickyCache<P> {
    key: (usize, usize, u32),
    lines: Vec<usize>,
    paragraphs: Vec<P>,
}

impl<P> Default for StickyCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0),
            lines: Vec::new(),
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
    /// True while dragging the minimap to scroll.
    minimap_drag: bool,
    cache: RefCell<LineCache<P>>,
    sticky_cache: RefCell<StickyCache<P>>,
}

impl<P> Default for State<P> {
    fn default() -> Self {
        Self {
            char_width: 0.0,
            pressed: false,
            cmd_held: false,
            last_hover: None,
            hover_row: None,
            minimap_drag: false,
            cache: RefCell::new(LineCache::default()),
            sticky_cache: RefCell::new(StickyCache::default()),
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

        // Minimap drag-to-scroll: a press or drag inside the band scrolls the
        // content to the corresponding fraction of the file.
        if let Some(on_minimap) = &self.on_minimap
            && let Some(band) = self.minimap_band(viewport)
        {
            let abs = cursor.position();
            let in_band = abs.is_some_and(|p| band.contains(p));
            let fraction = |p: Point| ((p.y - band.y) / band.height).clamp(0.0, 1.0);
            match event {
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) if in_band => {
                    state.minimap_drag = true;
                    shell.publish(on_minimap(fraction(abs.unwrap())));
                    shell.capture_event();
                    return;
                }
                Event::Mouse(mouse::Event::CursorMoved { .. }) if state.minimap_drag => {
                    if let Some(p) = abs {
                        shell.publish(on_minimap(fraction(p)));
                    }
                    shell.capture_event();
                    return;
                }
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                    if state.minimap_drag =>
                {
                    state.minimap_drag = false;
                    shell.capture_event();
                    return;
                }
                _ => {}
            }
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
                let arrow_x0 = FOLD_ARROW_COL as f32 * state.char_width;
                let gutter_px = GUTTER_CHARS as f32 * state.char_width;
                // A click in the line-number gutter margin (left of the fold
                // arrow) toggles a breakpoint on that line — the conventional
                // editor gesture, so users don't have to reach the context menu.
                if let Some(on_breakpoint) = &self.on_breakpoint
                    && point.x < arrow_x0
                {
                    let row = (point.y / self.line_height) as usize;
                    if let Some(line) = self.line_at_row(row) {
                        shell.publish(on_breakpoint(line + 1)); // 1-based
                        shell.capture_event();
                        return;
                    }
                }
                // A click on a fold arrow toggles the fold instead of moving
                // the cursor. The arrow lives in the trailing gutter columns.
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
                } else {
                    // Cmd keeps the go-to-definition underline following the cursor.
                    if state.cmd_held {
                        shell.request_redraw();
                    }
                    // Ask for a peek when the token under the cursor changes — on
                    // plain hover, not only Cmd-hover (the app applies a dwell
                    // before it actually shows). Clear it when the cursor leaves.
                    match cursor.position_in(bounds) {
                        Some(point) => {
                            if let Some(on_hover) = &self.on_hover {
                                let hit = self.hit::<Renderer::Paragraph>(point, state.char_width);
                                if state.last_hover != Some(hit) {
                                    state.last_hover = Some(hit);
                                    let at =
                                        cursor.position().unwrap_or(Point::new(bounds.x, bounds.y));
                                    shell.publish(on_hover(hit, at));
                                }
                            }
                        }
                        None => {
                            if state.last_hover.take().is_some()
                                && let Some(end) = &self.on_hover_end
                            {
                                shell.publish(end());
                            }
                        }
                    }
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
            self.annotation_signature(),
        );
        {
            // Shape one row's line into a paragraph (the expensive step).
            let shape = |row: usize| {
                let line = self.line_at_row(row).unwrap_or(0);
                let spans = self.line_spans(line);
                Renderer::Paragraph::with_spans(self.line_text(&spans))
            };
            let want_len = last - first;
            let mut cache = state.cache.borrow_mut();
            let old_first = cache.first;
            let old_end = old_first + cache.paragraphs.len();
            if cache.key != key
                || cache.paragraphs.is_empty()
                || first >= old_end
                || last <= old_first
            {
                // Content changed, or the new window doesn't overlap the cached
                // one (a jump) — shape the whole visible range.
                cache.key = key;
                cache.first = first;
                cache.paragraphs = (first..last).map(&shape).collect();
            } else if cache.first != first || cache.paragraphs.len() != want_len {
                // A scroll shift: slide the window, MOVING the paragraphs that are
                // still on screen and shaping only the newly-revealed rows. A
                // one-line scroll then shapes ~1 line instead of the whole screen
                // — the re-shape-everything-per-frame cost was the scroll jank.
                let ov_start = first.max(old_first);
                let ov_end = last.min(old_end);
                let old = std::mem::take(&mut cache.paragraphs);
                let mut next = Vec::with_capacity(want_len);
                for row in first..ov_start {
                    next.push(shape(row));
                }
                for p in old
                    .into_iter()
                    .skip(ov_start - old_first)
                    .take(ov_end - ov_start)
                {
                    next.push(p);
                }
                for row in ov_end..last {
                    next.push(shape(row));
                }
                cache.key = key;
                cache.first = first;
                cache.paragraphs = next;
            }
        }

        let cache = state.cache.borrow();
        let text_x0 = bounds.x + gutter_px;
        // Rows hidden behind the sticky band are not drawn at all, so their
        // text never bleeds through the pinned headers (the header text renders
        // above the panel fill regardless of draw order).
        let sticky_h = self.sticky.len() as f32 * lh;
        let first_shown_row = if self.sticky.is_empty() {
            first
        } else {
            (((viewport.y + sticky_h - bounds.y) / lh).ceil() as usize).max(first)
        };
        for row in first..last {
            if row < first_shown_row {
                continue;
            }
            let Some(i) = self.line_at_row(row) else {
                continue;
            };
            let y = bounds.y + row as f32 * lh;
            let paragraph = cache.paragraphs.get(row - cache.first);

            // Debug: highlight the current stopped line across the FULL editor
            // width. The widget lays out to content width (longest line), so we
            // extend to the visible viewport's right edge (clipped there).
            if self.debug_current == Some(i + 1) {
                let width = (viewport.x + viewport.width - bounds.x).max(bounds.width);
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y,
                            width,
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::with_alpha(theme::rgb(0xe5c07b), 0.16),
                );
            } else if self.cursor.is_some_and(|(cl, _)| cl == i) {
                // Current line: a whisper-faint full-width wash for orientation.
                let width = (viewport.x + viewport.width - bounds.x).max(bounds.width);
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y,
                            width,
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::with_alpha(theme::FG, 0.04),
                );
            }
            // Debug: a breakpoint dot at the left of the gutter — red normally,
            // amber when conditional.
            if self.breakpoints.contains(&(i + 1)) {
                let d = (lh * 0.55).min(9.0);
                let color = if self.cond_breakpoints.contains(&(i + 1)) {
                    theme::rgb(0xe5c07b)
                } else {
                    theme::rgb(0xe06c75)
                };
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x + 1.0,
                            y: y + (lh - d) / 2.0,
                            width: d,
                            height: d,
                        },
                        border: iced::Border {
                            radius: (d / 2.0).into(),
                            ..Default::default()
                        },
                        ..renderer::Quad::default()
                    },
                    color,
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

            // Indentation guides: a faint vertical line at each enclosing
            // indent level (columns 4, 8, … strictly inside this line's indent).
            if self.indent_guides {
                let ind = self.line_indent(i);
                let mut level = 4;
                while level < ind {
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: text_x0 + level as f32 * state.char_width,
                                y,
                                width: 1.0,
                                height: lh,
                            },
                            ..renderer::Quad::default()
                        },
                        theme::with_alpha(theme::DIM, 0.45),
                    );
                    level += 4;
                }
            }

            // Extra span highlights on this line (find / occurrences / bracket).
            for hl in self.highlights.iter().filter(|h| h.line == i) {
                let grapheme_x = |g: usize| match paragraph.and_then(|p| p.grapheme_position(0, g))
                {
                    Some(pt) => pt.x,
                    None => g as f32 * state.char_width,
                };
                // The columns are source display columns, but the paragraph has
                // inlay chips spliced in — map through them so the box aligns.
                let x0 = grapheme_x(self.spliced_col(i, hl.col0, true));
                let x1 = grapheme_x(self.spliced_col(i, hl.col1, false));
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

            // Git change bar at the very left of the gutter.
            if let Some(status) = self.git_status
                && let Some(Some(kind)) = status.get(i)
            {
                let color = match kind {
                    crate::git::ChangeKind::Added => theme::rgb(0x98c379),
                    crate::git::ChangeKind::Modified => theme::rgb(0x61afef),
                };
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y,
                            width: 3.0,
                            height: lh,
                        },
                        ..renderer::Quad::default()
                    },
                    color,
                );
            }
            // Deleted-below marker: a short red bar at the line's bottom edge.
            if self.git_deleted.is_some_and(|d| d.contains(&i)) {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: bounds.x,
                            y: y + lh - 2.0,
                            width: 6.0,
                            height: 3.0,
                        },
                        ..renderer::Quad::default()
                    },
                    theme::rgb(0xe06c75),
                );
            }

            // Gutter line number (owned text; rendered directly). The caret's
            // line number is brightened so you can find your place at a glance.
            let gutter_color = if self.breakpoints.contains(&(i + 1)) {
                theme::rgb(0xe06c75)
            } else if self.bookmarks.contains(&(i + 1)) {
                theme::ACCENT
            } else if self.cursor.is_some_and(|(cl, _)| cl == i) {
                theme::FG
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
                // Inline LLM summary past a function's signature end (assisted
                // reading). Skipped on the blame line so git blame wins there.
                // End-of-line annotations (LLM summary / git blame) must stop
                // before the minimap band rather than sliding under it, so clip
                // them to the code area left of the band with a small gap.
                let anno_clip = match self.minimap_band(viewport) {
                    Some(band) => Rectangle {
                        width: (band.x - 6.0 - viewport.x).max(0.0),
                        ..*viewport
                    },
                    None => *viewport,
                };
                let on_blame_line = self.blame.as_ref().is_some_and(|(bl, _)| *bl == i);
                if !on_blame_line && let Some(summary) = self.summaries.get(&(i + 1)) {
                    let end_x = text_x0 + paragraph.min_bounds().width + 2.0 * state.char_width;
                    renderer.fill_text(
                        text::Text {
                            content: format!("— {summary}"),
                            bounds: Size::new(f32::MAX, lh),
                            size: (self.font_size - 1.0).max(8.0).into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: text::Shaping::Basic,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(end_x, y),
                        theme::with_alpha(theme::rgb(0x7e8aa0), 0.85),
                        anno_clip,
                    );
                }
                // Inline git blame for the caret line, past the line's end.
                if let Some((bl, annotation)) = &self.blame
                    && *bl == i
                {
                    let end_x = text_x0 + paragraph.min_bounds().width + 2.0 * state.char_width;
                    renderer.fill_text(
                        text::Text {
                            content: annotation.clone(),
                            bounds: Size::new(f32::MAX, lh),
                            size: (self.font_size - 1.0).max(8.0).into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: text::Shaping::Basic,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(end_x, y),
                        theme::with_alpha(theme::DIM, 0.9),
                        anno_clip,
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

        // Sticky scroll: pin the enclosing headers at the very top. The rows
        // beneath were skipped above, so the panel fill covers clean editor
        // background; it extends to the first shown row so no sliver peeks at
        // fractional scroll offsets.
        if !self.sticky.is_empty() {
            let band_bottom = bounds.y + first_shown_row as f32 * lh;
            let band_h = (band_bottom - viewport.y).max(sticky_h);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x,
                        y: viewport.y,
                        width: viewport.width,
                        height: band_h,
                    },
                    ..renderer::Quad::default()
                },
                theme::BG_PANEL,
            );
            // Shape the pinned lines once and cache them, so a per-frame scroll
            // with a header stuck to the top doesn't re-shape every colored span.
            {
                let sticky_key = (
                    self.lines.as_ptr() as usize,
                    self.lines.len(),
                    self.font_size.to_bits(),
                );
                let mut sc = state.sticky_cache.borrow_mut();
                if sc.key != sticky_key || sc.lines != self.sticky {
                    sc.key = sticky_key;
                    sc.lines = self.sticky.clone();
                    sc.paragraphs = self
                        .sticky
                        .iter()
                        .map(|&line| {
                            let spans = self.line_spans(line);
                            Renderer::Paragraph::with_spans(self.line_text(&spans))
                        })
                        .collect();
                }
            }
            let sc = state.sticky_cache.borrow();
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
                if let Some(para) = sc.paragraphs.get(k) {
                    renderer.fill_paragraph(
                        para,
                        Point::new(text_x0, y),
                        self.default_color,
                        *viewport,
                    );
                }
            }
            // Separator line under the sticky region.
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x,
                        y: band_bottom - 1.0,
                        width: viewport.width,
                        height: 1.0,
                    },
                    ..renderer::Quad::default()
                },
                theme::BORDER,
            );
        }

        // Minimap: a compressed overview pinned to the right of the viewport,
        // one sampled bar per pixel row, shaped by indentation and line length,
        // with a translucent box marking the visible range.
        if let Some(band) = self.minimap_band(viewport) {
            let total = self.row_count().max(1);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: band,
                    ..renderer::Quad::default()
                },
                theme::with_alpha(theme::BG_PANEL, 0.85),
            );
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle { width: 1.0, ..band },
                    ..renderer::Quad::default()
                },
                theme::BORDER,
            );

            let max_bar = band.width - 6.0;
            let row_px = band.height / total as f32;
            let step = (1.0 / row_px).ceil().max(1.0) as usize;
            let bar_h = row_px.max(1.0);
            let mut row = 0;
            while row < total {
                let line = self.line_at_row(row).unwrap_or(0);
                let len = self.line_display_len(line);
                if len > 0 {
                    let indent = self.line_indent(line).min(len);
                    let y = band.y + row as f32 / total as f32 * band.height;
                    let x = band.x + 3.0 + (indent as f32 / MINIMAP_FULL_COLS) * max_bar;
                    let content = (len - indent) as f32 / MINIMAP_FULL_COLS * max_bar;
                    let right = band.x + 3.0 + max_bar;
                    let w = content.clamp(1.0, (right - x).max(1.0));
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x,
                                y,
                                width: w,
                                height: bar_h,
                            },
                            ..renderer::Quad::default()
                        },
                        theme::with_alpha(self.line_minimap_color(line), 0.55),
                    );
                }
                row += step;
            }

            // Visible-range indicator.
            let first_row = ((viewport.y - bounds.y) / lh).max(0.0);
            let vis_rows = viewport.height / lh;
            let iy = band.y + (first_row / total as f32) * band.height;
            let ih = ((vis_rows / total as f32) * band.height).max(6.0);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: band.x,
                        y: iy,
                        width: band.width,
                        height: ih,
                    },
                    ..renderer::Quad::default()
                },
                theme::with_alpha(theme::ACCENT, 0.16),
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
