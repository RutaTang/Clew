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
//!
//! Three index spaces meet in this widget, and mixing them was the source of
//! every "click lands on the wrong character" bug on non-ASCII lines:
//!   * **display columns** — chars of the tab-expanded line (`Hit`, `Hl`, the
//!     selection, the caret): what the rest of the app speaks;
//!   * **bytes** — what the shaper's hit test returns (cosmic-text's
//!     `Cursor::index`, surfaced as `Hit::CharOffset` despite the name);
//!   * **graphemes** — what `Paragraph::grapheme_position` takes.
//!
//! [`ColMap`] converts between them once per shaped line (cached with the
//! paragraph); nothing else in the widget indexes a paragraph directly.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet};

use iced::advanced::text::paragraph::Plain;
use iced::advanced::text::{self, Paragraph as _, Span, Text};
use iced::advanced::widget::{Widget, tree};
use iced::advanced::{Clipboard, Layout, Shell, layout, mouse, renderer};
use iced::{Color, Element, Event, Font, Length, Point, Rectangle, Size};
use unicode_segmentation::UnicodeSegmentation;

use crate::analyze::{MAX_WORD_CHARS, ident_range, line_window, word_range_at};
use crate::highlight::{HlLine, style_color};
use crate::theme;
use crate::viewer::{InlayHints, display_cols};

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
/// Longest prefix of a line (in display columns) that is shaped, drawn and
/// hit-tested. Shaping is linear in the line's length and runs on the UI
/// thread — for every newly revealed row, and for hit tests on rows outside
/// the paragraph cache — so one minified bundle line of a few megabytes used to
/// stall scrolling and every mouse move over it. Nobody reads past 4096
/// columns; the rest of such a line is elided (a dim `…` marks the cut) and
/// stays reachable through search, copy and the language server.
const MAX_SHAPED_COLS: usize = 4096;
/// Width of the elision marker's box past a cut line's shaped prefix, in
/// monospace advances: a half-advance gap, then the two-advance `…`.
const ELISION_MARKER_COLS: f32 = 2.5;
/// Columns the collapsed-fold cue takes past a header's end: one gap, then
/// the two-column `⋯` box.
const COLLAPSED_CUE_COLS: usize = 3;

/// A click resolved to a 0-based line and 0-based display column.
type Hit = (usize, usize);

/// An in-file find match: (line, start col, end col) in display columns —
/// the shape [`crate::find::Match`] has.
type FindMatch = (usize, usize, usize);

/// How a pointer x-position becomes a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rounding {
    /// The grapheme UNDER the pointer: its left and right edges bracket the
    /// pointer. What a click (the block caret sits on a character), a hover,
    /// the context menu and the go-to-definition underline need — rounding to
    /// the nearest boundary instead made the right half of a word's last
    /// character resolve to the column after the word, so hovering or
    /// Cmd-clicking there found no identifier at all.
    Under,
    /// The nearest grapheme BOUNDARY. What extending a selection by dragging
    /// needs: selection ends are gaps between characters, and a drag must be
    /// able to take in a line's last character without leaving the text.
    Nearest,
}

/// Column geometry of one shaped line. The paragraph's text is the display
/// text with inlay chips spliced in (capped at [`MAX_SHAPED_COLS`]); this maps
/// its chars to the byte offsets the shaper reports and to the grapheme
/// indexes `grapheme_position` expects.
///
/// Graphemes are the LEGACY clusters, as `grapheme_position` counts them;
/// cosmic-text's hit test reports extended-cluster boundaries, which are
/// always legacy boundaries too, so its offsets land on entries of this map.
#[derive(Debug, Default)]
struct ColMap {
    /// Chars in the shaped text.
    chars: usize,
    /// `None` for pure-ASCII text, where bytes, chars and graphemes coincide —
    /// the overwhelmingly common line pays for no table at all.
    table: Option<ColTable>,
}

#[derive(Debug)]
struct ColTable {
    /// Byte offset of each char, plus the text's byte length (`chars + 1`).
    char_byte: Vec<u32>,
    /// Index of the grapheme each char belongs to, plus the grapheme count
    /// (`chars + 1` entries).
    char_grapheme: Vec<u32>,
    /// First char of each grapheme, plus `chars` (`graphemes + 1` entries).
    grapheme_char: Vec<u32>,
}

/// The inline blame's text size: a point under the code's, never below the
/// smallest text the UI draws anywhere ([`crate::theme::MIN_TEXT_SIZE`]).
pub(crate) fn annotation_size(code_size: f32) -> f32 {
    (code_size - 1.0).max(crate::theme::MIN_TEXT_SIZE)
}

/// How the inline blame (`author, when · summary`) is shaped: `Advanced`,
/// with font fallback. It is free text from git — a CJK or accented author
/// name, an emoji in a summary — and `Basic` shaping draws whatever the
/// monospace face lacks as missing-glyph boxes. One line per frame (the
/// caret's), so the slower path costs nothing measurable.
pub(crate) const ANNOTATION_SHAPING: text::Shaping = text::Shaping::Advanced;

impl ColMap {
    fn new(text: &str) -> Self {
        if text.is_ascii() {
            return ColMap {
                chars: text.len(),
                table: None,
            };
        }
        let mut char_byte: Vec<u32> = Vec::with_capacity(text.len() + 1);
        let mut char_grapheme: Vec<u32> = Vec::with_capacity(text.len() + 1);
        let mut grapheme_char: Vec<u32> = Vec::new();
        for (g, grapheme) in text.graphemes(false).enumerate() {
            // Offsets are bounded by the shaping cap, far below u32::MAX.
            grapheme_char.push(char_byte.len() as u32);
            let base = grapheme.as_ptr() as usize - text.as_ptr() as usize;
            for (b, _) in grapheme.char_indices() {
                char_byte.push((base + b) as u32);
                char_grapheme.push(g as u32);
            }
        }
        let chars = char_byte.len();
        char_byte.push(text.len() as u32);
        char_grapheme.push(grapheme_char.len() as u32);
        grapheme_char.push(chars as u32);
        ColMap {
            chars,
            table: Some(ColTable {
                char_byte,
                char_grapheme,
                grapheme_char,
            }),
        }
    }

    /// Number of graphemes in the text.
    fn graphemes(&self) -> usize {
        match &self.table {
            None => self.chars,
            Some(t) => t.grapheme_char.len() - 1,
        }
    }

    /// Char index of byte offset `byte` (rounded down to a char boundary;
    /// past the end → the char count).
    fn char_of_byte(&self, byte: usize) -> usize {
        match &self.table {
            None => byte.min(self.chars),
            Some(t) => t
                .char_byte
                .partition_point(|&b| b as usize <= byte)
                .saturating_sub(1)
                .min(self.chars),
        }
    }

    /// Grapheme containing char `c` (`c >= chars` → the grapheme count).
    fn grapheme_of_char(&self, c: usize) -> usize {
        match &self.table {
            None => c.min(self.chars),
            Some(t) => t.char_grapheme[c.min(self.chars)] as usize,
        }
    }

    /// First char of grapheme `g` (`g >= graphemes` → the char count).
    fn grapheme_start(&self, g: usize) -> usize {
        match &self.table {
            None => g.min(self.chars),
            Some(t) => t.grapheme_char[g.min(t.grapheme_char.len() - 1)] as usize,
        }
    }

    /// Whether char `c` starts a grapheme (or is the end of the text).
    fn is_boundary(&self, c: usize) -> bool {
        c >= self.chars || self.grapheme_start(self.grapheme_of_char(c)) == c
    }
}

/// A shaped line and its column map — what the paragraph cache holds.
struct Shaped<P> {
    paragraph: P,
    map: ColMap,
    /// The line runs past [`MAX_SHAPED_COLS`]; only its prefix was shaped.
    elided: bool,
}

impl<P: text::Paragraph> Shaped<P> {
    /// x of the grapheme boundary at-or-around spliced char `c`, relative to the
    /// text origin. A column inside a multi-char grapheme (a combining mark, a
    /// ZWJ sequence) cannot be addressed on its own: `round_up` picks the
    /// cluster's right edge (for the exclusive end of a span), else its left.
    /// Columns past the text continue at one monospace advance each, so a
    /// caret or selection end beyond the last glyph stays visible.
    ///
    /// On a line cut at [`MAX_SHAPED_COLS`], every column past the cut maps
    /// onto the elision marker — its left edge, or its right edge for
    /// `round_up` — so a caret, a selection end or a match beyond the cut is
    /// drawn ON the `…`, never past it where no text is shown.
    fn boundary_x(&self, c: usize, round_up: bool, char_width: f32) -> f32 {
        // A face with broken metrics can put glyphs at NaN or infinity (see
        // `ensure_usable_fonts`); fall back to monospace columns rather than
        // paint or hit-test there.
        if c >= self.map.chars {
            let end = Some(self.paragraph.min_bounds().width)
                .filter(|w| w.is_finite())
                .unwrap_or(self.map.chars as f32 * char_width);
            if self.elided {
                // A span reaching past the cut ends at the marker's far edge.
                let past_cut = c > self.map.chars && round_up;
                let marker = if past_cut { ELISION_MARKER_COLS } else { 0.0 };
                return end + marker * char_width;
            }
            return end + (c - self.map.chars) as f32 * char_width;
        }
        let mut g = self.map.grapheme_of_char(c);
        if round_up && !self.map.is_boundary(c) {
            g += 1;
        }
        self.paragraph
            .grapheme_position(0, g)
            .map(|p| p.x)
            .filter(|x| x.is_finite())
            .unwrap_or(g as f32 * char_width)
    }

    /// Spliced char column for a pointer `x` (relative to the text origin).
    fn column_at(&self, x: f32, line_height: f32, rounding: Rounding) -> usize {
        let Some(hit) = self.paragraph.hit_test(Point::new(x, line_height * 0.5)) else {
            return 0;
        };
        // `Hit::cursor()` is cosmic-text's `Cursor::index`: a BYTE offset into
        // the paragraph text, already snapped to the nearest boundary.
        let c = self.map.char_of_byte(hit.cursor());
        if rounding == Rounding::Nearest {
            return c;
        }
        // The grapheme under the pointer is the one whose left and right edges
        // bracket it: the boundary the shaper chose is either its left edge
        // (pointer in its left half) or its right edge (right half).
        let g = self.map.grapheme_of_char(c);
        let edge = match self.paragraph.grapheme_position(0, g) {
            Some(p) if p.x.is_finite() => p.x,
            _ => return c,
        };
        let under = if x < edge && g > 0 { g - 1 } else { g };
        if under >= self.map.graphemes() {
            self.map.chars // past the last glyph: the end of the line
        } else {
            self.map.grapheme_start(under)
        }
    }
}

/// What a document's annotations add to its lines — inlay chips spliced in,
/// `cfg`-inactive lines dimmed, collapsed-fold cues drawn past headers —
/// summarized for the paragraph caches and the scroll extent. Derived where
/// those inputs change ([`crate::viewer::Viewer::annotations`]) and handed to
/// the view with [`CodeView::annotations`]: computed per view build instead,
/// it hashed every chip label and walked every annotated line on each
/// rebuild, and a mouse move is one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Annotations {
    /// Order-independent content signature of the inlay chips and inactive
    /// lines: part of the paragraph caches' key, since both can land after
    /// the first render with the lines buffer unchanged.
    pub signature: u64,
    /// Widest inlay-hinted line as drawn — its text (counted no further than
    /// the shaping cap) plus its chips — in display columns.
    pub hinted_cols: usize,
    /// Widest collapsed fold header as drawn, its "⋯" cue included.
    pub collapsed_cols: usize,
}

/// The [`Annotations`] of `lines` with these inlay hints, inactive lines and
/// collapsed fold headers. O(annotations); cheap enough per change, too much
/// per frame.
pub fn annotations_of(
    lines: &[HlLine],
    inlay: Option<&InlayHints>,
    inactive: Option<&HashSet<usize>>,
    collapsed: Option<&HashSet<usize>>,
) -> Annotations {
    let widest = |headers: &mut dyn Iterator<Item = usize>, extra: usize| {
        headers
            .map(|line| visual_line_cols(lines, inlay, line) + extra)
            .max()
            .unwrap_or(0)
    };
    Annotations {
        signature: annotation_signature(inlay, inactive),
        hinted_cols: widest(&mut inlay.into_iter().flat_map(HashMap::keys).copied(), 0),
        collapsed_cols: widest(
            &mut collapsed.into_iter().flatten().copied(),
            COLLAPSED_CUE_COLS,
        ),
    }
}

/// Display columns of 0-based `line` *as drawn*: its text plus the inlay chips
/// spliced into it. Everything that draws past a line's end is positioned from
/// the spliced paragraph's width (`paragraph.min_bounds()`), so every extent
/// term has to start from this, not from the text alone — otherwise the tail
/// of an annotation on a hinted line is drawn outside the content width and
/// can never be scrolled into view. A line past the shaping cap draws only its
/// first [`MAX_SHAPED_COLS`] columns plus the elision marker, so the text is
/// counted no further.
fn visual_line_cols(lines: &[HlLine], inlay: Option<&InlayHints>, line: usize) -> usize {
    let chips: usize = inlay
        .and_then(|h| h.get(&line))
        .map_or(0, |h| h.iter().map(|(_, l)| l.chars().count()).sum());
    lines
        .get(line)
        .map_or(0, |l| display_cols(l, MAX_SHAPED_COLS + 2))
        + chips
}

/// An order-independent signature of the inlay hints and inactive lines, so
/// the paragraph cache invalidates when either changes.
fn annotation_signature(inlay: Option<&InlayHints>, inactive: Option<&HashSet<usize>>) -> u64 {
    // splitmix64's finalizer. Each element must pass through a NONLINEAR
    // mix before the order-independent sum: summing anything linear in
    // the line number collides on trivially rearranged sets ({0,3} vs
    // {1,2} — same sum, different lines highlighted).
    fn mix(mut h: u64) -> u64 {
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d049bb133111eb);
        h ^ (h >> 31)
    }
    // Each annotation kind mixes under its own domain tag: without one,
    // a chipless inlay entry on line N and an inactive line near N feed
    // the shared sum the same value, so one appearing while the other
    // disappears could leave the signature unchanged — and the stale
    // paragraph cached.
    const INLAY_DOMAIN: u64 = 0x1;
    const INACTIVE_DOMAIN: u64 = 0x2;
    let inlay = inlay
        .into_iter()
        .flatten()
        .map(|(line, chips)| {
            let mut h = (*line as u64).wrapping_mul(1000003) ^ INLAY_DOMAIN;
            for (col, text) in chips {
                h = h.wrapping_mul(31).wrapping_add(*col as u64);
                // Hash the label's CONTENT, not just its length: a
                // re-resolved hint often keeps its width while changing
                // (`: i32` → `: u32`), and the paragraph bakes the text.
                for b in text.as_bytes() {
                    h = h.wrapping_mul(131).wrapping_add(*b as u64);
                }
            }
            h
        })
        .fold(0u64, |acc, h| acc.wrapping_add(mix(h))); // summed: order-free
    let inactive = inactive.into_iter().flatten().fold(0u64, |acc, &l| {
        acc.wrapping_add(mix((l as u64).wrapping_mul(1000003) ^ INACTIVE_DOMAIN))
    });
    inlay.wrapping_add(inactive)
}

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
    /// Extra span highlights (occurrences / brackets / diagnostics), sorted by
    /// line so each drawn row finds its own with a binary search instead of a
    /// scan of the whole list.
    highlights: Vec<Hl>,
    /// In-file find matches `(line, col0, col1)` in document order, and the
    /// index of the current one. Borrowed from the find state, where they are
    /// computed once per query/document change: rebuilding a highlight per
    /// match on every view cost O(matches) per frame for a one-letter query.
    find: Option<(&'a [FindMatch], usize)>,
    /// Enclosing header lines pinned at the top (sticky scroll).
    sticky: Vec<usize>,
    bookmarks: HashSet<usize>,        // 1-based bookmarked lines
    breakpoints: HashSet<usize>,      // 1-based lines with a debug breakpoint
    cond_breakpoints: HashSet<usize>, // subset that are conditional (drawn amber)
    /// The subset the adapter explicitly REFUSED to bind (drawn as an outline,
    /// not a filled dot). Only lines the adapter answered "no" for belong here:
    /// a breakpoint it has not answered about yet is still drawn solid, because
    /// hollowing it would claim knowledge we do not have.
    unverified_breakpoints: HashSet<usize>,
    debug_current: Option<usize>, // 1-based current stopped line (debug)
    /// 0-based display line → inlay chips `(display column, label)` spliced into
    /// the line at render time (inferred types, parameter names). Borrowed:
    /// the viewer owns them, and cloning both maps on every view was a
    /// whole-document copy per frame.
    inlay_hints: Option<&'a InlayHints>,
    /// Colour for inlay-hint text (dim, so it reads as annotation not code).
    inlay_color: Color,
    /// 0-based lines gated off by an inactive `#[cfg]`, drawn dimmed.
    inactive: Option<&'a HashSet<usize>>,
    /// The [`Annotations`] summary: handed in precomputed
    /// ([`CodeView::annotations`]), else computed at most once per view build
    /// — `draw` needs it for both paragraph caches on every frame.
    annotations: OnceCell<Annotations>,
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
            find: None,
            sticky: Vec::new(),
            bookmarks: HashSet::new(),
            breakpoints: HashSet::new(),
            cond_breakpoints: HashSet::new(),
            unverified_breakpoints: HashSet::new(),
            debug_current: None,
            inlay_hints: None,
            inlay_color: default_color,
            inactive: None,
            annotations: OnceCell::new(),
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

    pub fn highlights(mut self, mut highlights: Vec<Hl>) -> Self {
        // Stable: spans on one line keep the caller's paint order.
        highlights.sort_by_key(|h| h.line);
        self.highlights = highlights;
        self
    }

    /// In-file find matches (document order, as [`crate::find::find_matches`]
    /// returns them) and the index of the current one, painted like
    /// highlights without being copied into the highlight list.
    pub fn find_matches(mut self, matches: &'a [FindMatch], current: usize) -> Self {
        self.find = Some((matches, current));
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

    /// The subset the debug adapter refused to bind — drawn as an outline, so
    /// a breakpoint that will never fire does not look like one that will.
    pub fn unverified_breakpoints(mut self, unverified: HashSet<usize>) -> Self {
        self.unverified_breakpoints = unverified;
        self
    }

    /// The current stopped line (1-based) while debugging — full-row highlight.
    pub fn debug_current(mut self, line: Option<usize>) -> Self {
        self.debug_current = line;
        self
    }

    /// Inlay hints (0-based display line → `(display column, label)`, sorted by
    /// column), spliced into each line at render time in `color`.
    pub fn inlay_hints(mut self, hints: &'a InlayHints, color: Color) -> Self {
        self.inlay_hints = Some(hints);
        self.inlay_color = color;
        self
    }

    /// The document's [`Annotations`] summary, kept current where its inputs
    /// change ([`crate::viewer::Viewer::annotations`]) — it must describe the
    /// very inlay hints, inactive lines and collapsed folds given to this
    /// view. Without it the view derives the summary itself on every build,
    /// hashing every chip label.
    pub fn annotations(self, annotations: Annotations) -> Self {
        Self {
            annotations: OnceCell::from(annotations),
            ..self
        }
    }

    /// 0-based lines gated off by an inactive `#[cfg]`, drawn dimmed.
    pub fn inactive(mut self, inactive: &'a HashSet<usize>) -> Self {
        self.inactive = Some(inactive);
        self
    }

    /// Every span highlight on 0-based `line`: the caller's highlights, then
    /// the find matches. Both lists are ordered by line, so a binary search
    /// finds the row's run — a frame costs O(rows · log n), not O(rows · n).
    fn highlights_on(&self, line: usize) -> impl Iterator<Item = Hl> + '_ {
        let from = self.highlights.partition_point(|h| h.line < line);
        let own = self.highlights[from..]
            .iter()
            .take_while(move |h| h.line == line)
            .copied();
        let found = self.find.into_iter().flat_map(move |(matches, current)| {
            let from = matches.partition_point(|m| m.0 < line);
            matches[from..]
                .iter()
                .enumerate()
                .take_while(move |(_, m)| m.0 == line)
                .map(move |(k, &(line, col0, col1))| Hl {
                    line,
                    col0,
                    col1,
                    kind: if from + k == current {
                        HlKind::FindCurrent
                    } else {
                        HlKind::FindMatch
                    },
                })
        });
        own.chain(found)
    }

    /// The (non-empty) inlay chips on 0-based `line`, if any.
    fn hints(&self, line: usize) -> Option<&'a [(usize, String)]> {
        self.inlay_hints
            .and_then(|h| h.get(&line))
            .map(Vec::as_slice)
            .filter(|h| !h.is_empty())
    }

    fn is_inactive(&self, line: usize) -> bool {
        self.inactive.is_some_and(|s| s.contains(&line))
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

    /// Display columns of a 0-based line, counted no further than `limit`.
    /// The minimap only ever needs a line's first ~100 columns, and counting a
    /// minified multi-megabyte line in full on every frame is what made the
    /// band stutter.
    fn line_cols_upto(&self, line: usize, limit: usize) -> usize {
        self.lines.get(line).map_or(0, |l| display_cols(l, limit))
    }

    /// The dominant syntax color of a line for the minimap: the color of the
    /// longest colored span, or the default foreground when the line is plain.
    fn line_minimap_color(&self, line: usize) -> Color {
        const SAMPLE_COLS: usize = 512;
        let Some(l) = self.lines.get(line) else {
            return self.default_color;
        };
        let mut best: Option<(usize, Color)> = None;
        let mut seen = 0usize;
        for (frag, style) in &l.spans {
            if seen >= SAMPLE_COLS {
                break;
            }
            let len = frag.chars().take(SAMPLE_COLS - seen).count();
            seen += len;
            if let Some(color) = style.and_then(style_color)
                && best.is_none_or(|(n, _)| len > n)
            {
                best = Some((len, color));
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

    /// The [`Annotations`] summary this view draws with: the one handed in,
    /// else derived once per build.
    fn annotation_summary(&self) -> Annotations {
        *self.annotations.get_or_init(|| {
            annotations_of(self.lines, self.inlay_hints, self.inactive, self.collapsed)
        })
    }

    /// Widest rendered line in display columns, as an ESTIMATE: one monospace
    /// advance per column. Inlay chips are spliced into the line and the
    /// collapsed cue / blame annotation draw past its end, so the scroll
    /// extent must be based on what is drawn, not just the source text —
    /// otherwise those annotations can never be scrolled into view. Every arm
    /// measures from [`visual_line_cols`]: a line can carry chips *and* an
    /// annotation, and the two widths add up rather than competing. The
    /// whole-document arms come precomputed in the [`Annotations`] summary;
    /// only the one blame line is measured here.
    ///
    /// Wide glyphs (CJK, emoji) are wider than one advance, so this undercounts
    /// such lines; `layout` takes the max with the MEASURED right edge of every
    /// line drawn so far (see `State::measured`), which is exact for what the
    /// reader can actually scroll to.
    fn visual_max_cols(&self, char_width: f32) -> usize {
        let summary = self.annotation_summary();
        let mut cols = self
            .max_cols
            .min(MAX_SHAPED_COLS + 2)
            .max(summary.hinted_cols)
            .max(summary.collapsed_cols);
        // The blame annotation stops before the minimap band, so give it room
        // to clear the band when a minimap is shown.
        let band = if self.on_minimap.is_some() && self.row_count() >= MINIMAP_MIN_ROWS {
            ((MINIMAP_WIDTH + 6.0) / char_width.max(1.0)).ceil() as usize
        } else {
            0
        };
        if let Some((line, annotation)) = &self.blame {
            let anno = 2 + annotation.chars().count();
            cols = cols.max(visual_line_cols(self.lines, self.inlay_hints, *line) + anno + band);
        }
        cols
    }

    /// Colored spans for one line — the text a paragraph is shaped from — and
    /// whether the line was cut at [`MAX_SHAPED_COLS`]. Inlay chips are spliced
    /// in at their display columns; a cut line keeps only the chips inside the
    /// shaped prefix.
    fn line_spans(&self, i: usize) -> (Vec<Span<'_, (), Font>>, bool) {
        let src = &self.lines[i].spans;
        let dim_line = self.is_inactive(i);
        let color_of = |style: &Option<u8>| {
            let c = style.and_then(style_color).unwrap_or(self.default_color);
            // Fade inactive-`cfg` code toward the background so live code stands out.
            if dim_line {
                Color { a: c.a * 0.38, ..c }
            } else {
                c
            }
        };
        let hints = self.hints(i).unwrap_or(&[]);
        // Splice each chip into the styled spans at its display column. Tabs are
        // already expanded to spaces, so one char == one display column.
        let mut out: Vec<Span<'_, (), Font>> = Vec::new();
        let mut col = 0usize; // display column at the start of the current fragment
        let mut hi = 0usize; // next chip to place
        let mut elided = false;
        for (fragment, style) in src {
            let color = color_of(style);
            let mut flen = fragment.chars().count();
            let mut fragment = fragment.as_str();
            if col + flen > MAX_SHAPED_COLS {
                // Past the shaping cap: keep the part that fits, drop the rest
                // of the line (and every chip anchored in it).
                flen = MAX_SHAPED_COLS - col;
                let end = fragment
                    .char_indices()
                    .nth(flen)
                    .map_or(fragment.len(), |(b, _)| b);
                fragment = &fragment[..end];
                elided = true;
            }
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
                out.push(Span::new(label.as_str()).color(self.inlay_color));
                cut = within;
                hi += 1;
            }
            if cut == 0 {
                out.push(Span::new(fragment).color(color));
            } else if cut < flen {
                let sub: String = fragment.chars().skip(cut).collect();
                out.push(Span::new(sub).color(color));
            }
            col += flen;
            if elided {
                return (out, true);
            }
        }
        // Chips past the end of the line's text render at the end.
        for (_, label) in &hints[hi..] {
            out.push(Span::new(label.as_str()).color(self.inlay_color));
        }
        (out, false)
    }

    /// Shape display line `line` into a paragraph plus its column map. The
    /// expensive step: callers go through the paragraph cache when they can.
    fn shape<P: text::Paragraph<Font = Font>>(&self, line: usize) -> Shaped<P> {
        ensure_usable_fonts();
        let (spans, elided) = self.line_spans(line);
        let text: String = spans.iter().map(|s| s.text.as_ref()).collect();
        Shaped {
            paragraph: P::with_spans(self.line_text(&spans)),
            map: ColMap::new(&text),
            elided,
        }
    }

    /// Column (char index) in the inlay-spliced paragraph for source display
    /// column `col` on `line`. Inlay-hint labels are spliced into the line, so a
    /// source column sits `label-length` columns further right for each hint
    /// before it — without this, span highlights (find/occurrence/bracket)
    /// drawn from source columns land left of the text on inlay lines.
    /// `inclusive` counts a hint sitting exactly at `col` (use it for a span's
    /// left edge, exclude it for the right edge so the highlight doesn't
    /// swallow a trailing chip).
    fn spliced_col(&self, line: usize, col: usize, inclusive: bool) -> usize {
        let shift: usize = self.hints(line).map_or(0, |hints| {
            hints
                .iter()
                .filter(|(hcol, _)| if inclusive { *hcol <= col } else { *hcol < col })
                .map(|(_, label)| label.chars().count())
                .sum()
        });
        col + shift
    }

    /// Inverse of [`Self::spliced_col`]: source display column for char index
    /// `spliced` in the inlay-spliced paragraph. Hit-testing measures against
    /// the rendered paragraph — which has the chips spliced in — but every
    /// consumer of a `Hit` (caret, selection, ⌘-click, context menu, hover)
    /// works in source columns; without this inverse, any click to the right
    /// of a hint landed `label-length` columns too far. A hit *inside* a chip
    /// snaps to the source column the chip is attached to.
    fn unspliced_col(&self, line: usize, spliced: usize) -> usize {
        let Some(hints) = self.hints(line) else {
            return spliced;
        };
        let mut shift = 0usize;
        for (hcol, label) in hints {
            let start = hcol + shift; // chip start in spliced space
            if spliced <= start {
                break; // hit is before this chip
            }
            let len = label.chars().count();
            if spliced < start + len {
                return *hcol; // hit is inside the chip: snap to its anchor
            }
            shift += len;
        }
        spliced - shift
    }

    /// The annotations' content signature (see [`Annotations::signature`]).
    fn annotation_signature(&self) -> u64 {
        self.annotation_summary().signature
    }

    /// The paragraph-cache key for the current content (see [`CacheKey`]).
    fn cache_key(&self) -> CacheKey {
        (
            self.lines.as_ptr() as usize,
            self.lines.len(),
            self.font_size.to_bits(),
            self.visible.map(|v| v.as_ptr() as usize).unwrap_or(0),
            self.annotation_signature(),
            theme::active_theme() as *const _ as usize,
        )
    }

    /// Identity of what the measured extent describes: the document, the font
    /// size and the annotations spliced into / drawn past its lines.
    fn extent_key(&self) -> ExtentKey {
        (
            self.lines.as_ptr() as usize,
            self.lines.len(),
            self.font_size.to_bits(),
            self.annotation_signature(),
        )
    }

    /// The token under `(line, col)` for hover debouncing: an identifier run,
    /// a run of whitespace, or a single other character (so `?` and `::` still
    /// get their own hover). Returns the token's first column. Reads a window
    /// of [`MAX_WORD_CHARS`] columns before `col`, not the whole line: a run
    /// longer than that starts, for debouncing, at the window's edge.
    fn hover_token(&self, line: usize, col: usize) -> usize {
        let from = col.saturating_sub(MAX_WORD_CHARS);
        let chars = self
            .lines
            .get(line)
            .map(|l| line_window(l, from, col + 1))
            .unwrap_or_default();
        let at = col - from;
        let Some(&c) = chars.get(at) else {
            // Past the end of the line: one token.
            return from + chars.len();
        };
        if let Some((start, _)) = ident_range(&chars, at) {
            return from + start;
        }
        if c.is_whitespace() {
            let mut start = at;
            while start > 0 && chars[start - 1].is_whitespace() {
                start -= 1;
            }
            return from + start;
        }
        col
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
/// a different line count, a font-size change, a change to the fold projection
/// (different `visible` allocation), or a theme switch all invalidate it —
/// shaped paragraphs bake span colors, so without the theme component a switch
/// left stale-colored lines (and scrolling mixed old and new palettes).
// (lines ptr, line count, font-size bits, fold-projection ptr, inlay
// signature, active-theme identity).
type CacheKey = (usize, usize, u32, usize, u64, usize);

/// (lines ptr, line count, font-size bits, annotation signature).
type ExtentKey = (usize, usize, u32, u64);

/// Cached shaped paragraphs for the currently visible line range.
struct LineCache<P> {
    key: CacheKey,
    first: usize,
    paragraphs: Vec<Shaped<P>>,
}

impl<P> Default for LineCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0, 0, 0, 0),
            first: 0,
            paragraphs: Vec::new(),
        }
    }
}

impl<P> LineCache<P> {
    /// The cached shaped line at display `row`, when the cache describes the
    /// content identified by `key` and covers that row.
    fn get(&self, key: CacheKey, row: usize) -> Option<&Shaped<P>> {
        if self.key != key {
            return None;
        }
        row.checked_sub(self.first)
            .and_then(|idx| self.paragraphs.get(idx))
    }
}

/// Cached shaped paragraphs for the pinned sticky-header lines. Rebuilt only
/// when the pinned set (or the underlying content / font / theme) changes, so
/// scrolling with a header pinned never re-shapes it per frame — the source of
/// the jank.
struct StickyCache<P> {
    /// (lines ptr, line count, font-size bits, theme identity, annotation
    /// signature). The annotations matter here too: a pinned header line can
    /// carry inlay chips, and its paragraph bakes them.
    key: (usize, usize, u32, usize, u64),
    lines: Vec<usize>,
    paragraphs: Vec<Shaped<P>>,
}

impl<P> Default for StickyCache<P> {
    fn default() -> Self {
        Self {
            key: (0, 0, 0, 0, 0),
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
    /// The token (line, first column) a hover was last reported for. Hover is
    /// published when the TOKEN under the pointer changes, not the column: each
    /// report rebuilds the whole view, and moving across one identifier used to
    /// pay that once per character.
    last_hover: Option<(usize, usize)>,
    /// Row the mouse currently hovers, to reveal the fold arrow on that line.
    hover_row: Option<usize>,
    /// True while dragging the minimap to scroll.
    minimap_drag: bool,
    cache: RefCell<LineCache<P>>,
    sticky_cache: RefCell<StickyCache<P>>,
    /// Right edge (px, from the text origin) of the widest line DRAWN so far —
    /// the shaped paragraph's real width plus anything painted past its end —
    /// for the content described by `measured_key`. The column estimate in
    /// `layout` counts one monospace advance per character, which is short for
    /// CJK and emoji; `draw` records the truth for every line it paints and the
    /// next event relayouts when it outgrew the estimate. Lazily exact: the
    /// lines a reader can scroll along are exactly the lines on screen.
    measured: Cell<f32>,
    measured_key: Cell<ExtentKey>,
    /// The text width (px) the last `layout` reserved.
    laid_out: Cell<f32>,
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
            measured: Cell::new(0.0),
            measured_key: Cell::new((0, 0, 0, 0)),
            laid_out: Cell::new(0.0),
        }
    }
}

impl<P> State<P> {
    /// Record a drawn line's right edge for the content `key` describes.
    fn record_extent(&self, key: ExtentKey, right: f32) {
        if self.measured_key.get() != key {
            self.measured_key.set(key);
            self.measured.set(0.0);
        }
        // A non-finite width (a face with broken metrics) must never become
        // the layout width: the scrollable would lose its extent altogether.
        if right.is_finite() && right > self.measured.get() {
            self.measured.set(right);
        }
    }

    /// The measured extent for `key`, or 0 when it describes other content.
    fn measured_for(&self, key: ExtentKey) -> f32 {
        if self.measured_key.get() == key {
            self.measured.get()
        } else {
            0.0
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
        let cw = state.char_width;

        // The column estimate, widened to the measured right edge of every
        // line drawn so far (wide glyphs make a line wider than its columns).
        let estimate = self.visual_max_cols(cw) as f32 * cw;
        let text_width = estimate.max(state.measured_for(self.extent_key()));
        state.laid_out.set(text_width);
        let width = GUTTER_CHARS as f32 * cw + text_width + cw;
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

        // `draw` found a line wider than the last layout reserved (wide glyphs
        // outgrow the column estimate): relayout so the scrollable's extent
        // reaches its end. Checked on every event — the next one after the
        // frame that measured it is a redraw request at the latest.
        if state.measured_for(self.extent_key()) > state.laid_out.get() + 0.5 {
            shell.invalidate_layout();
        }

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
            // Track Cmd/Ctrl so draw can underline the hovered symbol.
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(m))
                if state.cmd_held != m.command() =>
            {
                state.cmd_held = m.command();
                if !state.cmd_held {
                    state.last_hover = None;
                }
                shell.request_redraw();
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
                let hit = self.hit(state, point, Rounding::Under);
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
                        let hit = self.hit(state, point, Rounding::Nearest);
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
                                let hit = self.hit(state, point, Rounding::Under);
                                let token = (hit.0, self.hover_token(hit.0, hit.1));
                                if state.last_hover != Some(token) {
                                    state.last_hover = Some(token);
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
                let hit = self.hit(state, local, Rounding::Under);
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
        let key = self.cache_key();
        let extent_key = self.extent_key();
        {
            // Shape one row's line into a paragraph (the expensive step).
            let shape =
                |row: usize| self.shape::<Renderer::Paragraph>(self.line_at_row(row).unwrap_or(0));
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
            let shaped = cache.paragraphs.get(row - cache.first);
            let cw = state.char_width;
            // x (from the text origin) of source column `col` on this line:
            // mapped through the spliced chips, then through the column map to
            // the grapheme index the paragraph measures by.
            let col_x = |col: usize, round_up: bool| -> f32 {
                let spliced = self.spliced_col(i, col, !round_up);
                match shaped {
                    Some(s) => s.boundary_x(spliced, round_up, cw),
                    None => spliced as f32 * cw,
                }
            };

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
                    theme::with_alpha(theme::warning(), 0.16),
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
                    theme::with_alpha(theme::fg(), 0.04),
                );
            }
            // Debug: a breakpoint dot at the left of the gutter — red normally,
            // amber when conditional.
            if self.breakpoints.contains(&(i + 1)) {
                let d = (lh * 0.55).min(9.0);
                let color = if self.cond_breakpoints.contains(&(i + 1)) {
                    theme::warning()
                } else {
                    theme::danger()
                };
                // A breakpoint the adapter refused is drawn as a ring: it is
                // still the user's breakpoint and still where they put it, but
                // it will not fire, and a filled dot said the opposite.
                let refused = self.unverified_breakpoints.contains(&(i + 1));
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
                            width: if refused { 1.5 } else { 0.0 },
                            color,
                        },
                        ..renderer::Quad::default()
                    },
                    if refused { Color::TRANSPARENT } else { color },
                );
            }

            // Character-level selection background for this line.
            if let Some((x0, x1)) = self.selection_span(i, shaped, cw, &col_x) {
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
                    theme::selection(),
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
                        theme::with_alpha(theme::dim(), 0.45),
                    );
                    level += 4;
                }
            }

            // Extra span highlights on this line (find / occurrences / bracket).
            for hl in self.highlights_on(i) {
                // The columns are source display columns; `col_x` maps them
                // through the spliced chips and the grapheme geometry.
                let x0 = col_x(hl.col0, false);
                let x1 = col_x(hl.col1, true);
                let color = match hl.kind {
                    HlKind::FindCurrent => theme::with_alpha(theme::find(), 0.55),
                    HlKind::FindMatch => theme::with_alpha(theme::find(), 0.28),
                    HlKind::Occurrence => theme::with_alpha(theme::fg(), 0.16),
                    HlKind::Bracket => theme::with_alpha(theme::accent(), 0.35),
                    HlKind::DiagError => theme::danger(),
                    HlKind::DiagWarn => theme::warning(),
                    HlKind::DiagHint => theme::info(),
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
            // character under it still shows. The paragraph is inlay-spliced,
            // so source columns map through spliced_col (left edge counts a
            // chip anchored at the cell, the right edge excludes one anchored
            // just past it — same convention as the span highlights).
            if let Some((_, cc)) = self.cursor.filter(|(cl, _)| *cl == i) {
                // The cell spans the whole grapheme at the caret (a CJK glyph,
                // an emoji sequence), not one monospace advance.
                let x0 = col_x(cc, false);
                let width = (col_x(cc + 1, true) - x0).max(cw.max(2.0));
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
                    theme::with_alpha(theme::accent(), 0.4),
                );
            }

            // Git change bar at the very left of the gutter.
            if let Some(status) = self.git_status
                && let Some(Some(kind)) = status.get(i)
            {
                let color = match kind {
                    crate::git::ChangeKind::Added => theme::success(),
                    crate::git::ChangeKind::Modified => theme::accent(),
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
                    theme::danger(),
                );
            }

            // Gutter line number (owned text; rendered directly). The caret's
            // line number is brightened so you can find your place at a glance.
            let gutter_color = if self.breakpoints.contains(&(i + 1)) {
                theme::danger()
            } else if self.bookmarks.contains(&(i + 1)) {
                theme::accent()
            } else if self.cursor.is_some_and(|(cl, _)| cl == i) {
                theme::fg()
            } else {
                theme::dim()
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
                        if collapsed {
                            theme::accent()
                        } else {
                            theme::dim()
                        },
                        *viewport,
                    );
                }
            }

            // Code text: a cached, shaped paragraph of colored spans.
            if let Some(shaped) = shaped {
                let paragraph = &shaped.paragraph;
                let text_w = paragraph.min_bounds().width;
                renderer.fill_paragraph(
                    paragraph,
                    Point::new(text_x0, y),
                    style.text_color,
                    *viewport,
                );
                // Right edge of everything painted for this line, for the
                // measured scroll extent (see `State::measured`).
                let mut right = text_w;
                // A line cut at the shaping cap ends in a dim ellipsis.
                if shaped.elided {
                    renderer.fill_text(
                        text::Text {
                            content: "…".to_string(),
                            bounds: Size::new(2.0 * cw, lh),
                            size: self.font_size.into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: text::Shaping::Basic,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(text_x0 + text_w + 0.5 * cw, y),
                        theme::dim(),
                        *viewport,
                    );
                    right = right.max(text_w + ELISION_MARKER_COLS * cw);
                }
                // Collapsed cue: a dim ⋯ after the header line's end.
                if self.is_collapsed(i) {
                    let end_x = text_x0 + text_w + cw;
                    right = right.max(text_w + COLLAPSED_CUE_COLS as f32 * cw);
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
                        theme::dim(),
                        *viewport,
                    );
                }
                // The end-of-line blame annotation must stop before the
                // minimap band rather than sliding under it, so clip it to the
                // code area left of the band with a small gap.
                let band = self.minimap_band(viewport);
                let anno_clip = match band {
                    Some(band) => Rectangle {
                        width: (band.x - 6.0 - viewport.x).max(0.0),
                        ..*viewport
                    },
                    None => *viewport,
                };
                // An annotation must be scrollable out from under the band.
                let band_room = if band.is_some() {
                    MINIMAP_WIDTH + 6.0
                } else {
                    0.0
                };
                // Inline git blame for the caret line, past the line's end.
                if let Some((bl, annotation)) = &self.blame
                    && *bl == i
                {
                    let end_x = text_x0 + text_w + 2.0 * cw;
                    right = right
                        .max(text_w + (2 + estimated_cols(annotation)) as f32 * cw + band_room);
                    renderer.fill_text(
                        text::Text {
                            content: annotation.clone(),
                            bounds: Size::new(f32::MAX, lh),
                            size: annotation_size(self.font_size).into(),
                            line_height: text::LineHeight::Absolute(lh.into()),
                            font: Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Top,
                            shaping: ANNOTATION_SHAPING,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(end_x, y),
                        theme::with_alpha(theme::dim(), 0.9),
                        anno_clip,
                    );
                }
                state.record_extent(extent_key, right);
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
                && let Some(shaped) = cache.get(key, row)
            {
                // The paragraph is inlay-spliced: map the visual hit back to a
                // source column for word lookup, and source word bounds back to
                // visual columns to measure the underline against the paragraph.
                let spliced = shaped.column_at(p.x - gutter_px, lh, Rounding::Under);
                let col = self.unspliced_col(line, spliced);
                if let Some((s, e)) = self.word_at(line, col) {
                    let cx = |c: usize, round_up: bool| {
                        shaped.boundary_x(
                            self.spliced_col(line, c, !round_up),
                            round_up,
                            state.char_width,
                        )
                    };
                    let y = bounds.y + row as f32 * lh;
                    let (x0, x1) = (cx(s, false), cx(e, true));
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: text_x0 + x0,
                                y: y + lh - 2.0,
                                width: (x1 - x0).max(1.0),
                                height: 1.0,
                            },
                            ..renderer::Quad::default()
                        },
                        theme::accent(),
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
                theme::bg_panel(),
            );
            // Shape the pinned lines once and cache them, so a per-frame scroll
            // with a header stuck to the top doesn't re-shape every colored span.
            {
                let sticky_key = (
                    self.lines.as_ptr() as usize,
                    self.lines.len(),
                    self.font_size.to_bits(),
                    theme::active_theme() as *const _ as usize,
                    self.annotation_signature(),
                );
                let mut sc = state.sticky_cache.borrow_mut();
                if sc.key != sticky_key || sc.lines != self.sticky {
                    sc.key = sticky_key;
                    sc.lines = self.sticky.clone();
                    sc.paragraphs = self
                        .sticky
                        .iter()
                        .map(|&line| self.shape::<Renderer::Paragraph>(line))
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
                    theme::dim(),
                    *viewport,
                );
                if let Some(shaped) = sc.paragraphs.get(k) {
                    renderer.fill_paragraph(
                        &shaped.paragraph,
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
                theme::border(),
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
                theme::with_alpha(theme::bg_panel(), 0.85),
            );
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle { width: 1.0, ..band },
                    ..renderer::Quad::default()
                },
                theme::border(),
            );

            let max_bar = band.width - 6.0;
            let row_px = band.height / total as f32;
            let step = (1.0 / row_px).ceil().max(1.0) as usize;
            let bar_h = row_px.max(1.0);
            let mut row = 0;
            while row < total {
                let line = self.line_at_row(row).unwrap_or(0);
                // Bars saturate at the band's width, so a line is only counted
                // as far as the widest bar could show.
                let len = self.line_cols_upto(line, 4 * MINIMAP_FULL_COLS as usize);
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
                theme::with_alpha(theme::accent(), 0.16),
            );
        }
    }
}

impl<Message> CodeView<'_, Message> {
    /// Display-column range `[start, end)` of the identifier under `col` on
    /// `line`, or `None` when `col` is not on an identifier character.
    fn word_at(&self, line: usize, col: usize) -> Option<(usize, usize)> {
        word_range_at(self.lines, line, col)
    }

    /// Horizontal span `(x0, x1)` (relative to the text origin) of the selection
    /// on line `i`, or `None` when the line is outside the selection. `col_x`
    /// maps a source column to its glyph-accurate x (see `draw`).
    fn selection_span<P: text::Paragraph>(
        &self,
        i: usize,
        shaped: Option<&Shaped<P>>,
        char_width: f32,
        col_x: &impl Fn(usize, bool) -> f32,
    ) -> Option<(f32, f32)> {
        let ((sl, sc), (el, ec)) = self.selection?;
        if i < sl || i > el {
            return None;
        }
        let line_end = shaped.map_or(0.0, |s| s.paragraph.min_bounds().width);
        let x0 = if i == sl { col_x(sc, false) } else { 0.0 };
        let x1 = if i == el {
            col_x(ec, true)
        } else {
            // Continuation lines extend to the text end, with a small sliver so
            // selected empty lines are still visible.
            line_end.max(char_width * 0.5)
        };
        Some((x0, x1.max(x0)))
    }

    /// Resolve a widget-local point to a (line, display column), glyph-accurate:
    /// measured against the line's shaped paragraph — the cached one when the
    /// row is on screen (it always is for a click), else shaped on the spot —
    /// with the shaper's byte offset converted to a column and `rounding`
    /// deciding between the grapheme under the pointer and the nearest gap.
    fn hit<P: text::Paragraph<Font = Font>>(
        &self,
        state: &State<P>,
        point: Point,
        rounding: Rounding,
    ) -> Hit {
        let row = (point.y / self.line_height) as usize;
        let line = self
            .line_at_row(row)
            .unwrap_or(self.lines.len().saturating_sub(1));
        let gutter_px = GUTTER_CHARS as f32 * state.char_width;
        let text_x = point.x - gutter_px;
        if text_x <= 0.0 || self.lines.get(line).is_none() {
            return (line, 0);
        }
        let cache = state.cache.borrow();
        let spliced = match self
            .line_at_row(row)
            .and_then(|_| cache.get(self.cache_key(), row))
        {
            Some(shaped) => shaped.column_at(text_x, self.line_height, rounding),
            None => self
                .shape::<P>(line)
                .column_at(text_x, self.line_height, rounding),
        };
        // The paragraph has inlay chips spliced in; map the visual column back
        // to the source column every Hit consumer expects.
        (line, self.unspliced_col(line, spliced))
    }
}

/// Columns a string of annotation text occupies at one monospace advance per
/// column, counting non-ASCII characters double (CJK and emoji are about two
/// advances wide) — an estimate that errs on the reachable side.
fn estimated_cols(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

/// Drop font faces that shape to non-finite advances from the global font
/// database, once, before the code view first shapes anything.
///
/// cosmic-text's monospace fallback prefers any face flagged monospaced that
/// covers a script, and on macOS the system's "GB18030 Bitmap" is such a face
/// for Han: it has no usable metrics, so every CJK glyph in the code view got
/// an INFINITE advance — the rest of the line was drawn at infinity, the
/// paragraph's width was `inf`, and every hit test on it answered column 0.
/// With the face gone the fallback reaches the platform CJK font (PingFang,
/// Hiragino). The probe is generic — any face that yields a non-finite advance
/// for a sample of scripts is removed ([`broken_faces`]) — and cheap: one
/// shaping pass, once.
///
/// Candidates are enumerated from the database on every fallback query
/// (`db_mut` clears the match cache), so removal holds even for a face that
/// was already loaded. The code view calls this before its first measurement
/// and shaping ([`measure_char_width`], `shape`).
///
/// Coupled to the text stack's internals: iced's global `font_system()` wraps
/// a cosmic-text (0.15) `FontSystem`, whose `raw()` database is edited in
/// place. Re-check on an iced or cosmic-text upgrade;
/// `the_probe_keeps_the_primary_monospace_face` exercises the whole path on
/// the machine's real fonts.
pub fn ensure_usable_fonts() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use iced::advanced::graphics::text::font_system;
        let mut guard = match font_system().write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        remove_broken_faces(guard.raw());
    });
}

/// The probe behind [`ensure_usable_fonts`], on any font system.
fn remove_broken_faces(raw: &mut iced::advanced::graphics::text::cosmic_text::FontSystem) {
    use iced::advanced::graphics::text::cosmic_text as ct;
    const SAMPLE: &str = "日本語 ひらがな カタカナ 한국어 Русский Ελληνικά עברית \
                          العربية हिन्दी ไทย → ─ ✓ …";
    // Each round removes the faces that failed; the next round shapes
    // again to see what the fallback picks instead. Bounded, because a
    // system could in principle ship several broken faces per script.
    for _ in 0..8 {
        let mut buffer = ct::Buffer::new(raw, ct::Metrics::new(13.0, 20.0));
        // One unwrapped line, like a code line: every glyph of the sample is
        // shaped in each round. Wrapped, an infinite advance pushed the rest
        // of the sample onto lines past the buffer's height, unexamined.
        buffer.set_wrap(raw, ct::Wrap::None);
        buffer.set_size(raw, Some(LINE_LAYOUT_WIDTH), Some(40.0));
        let attrs = ct::Attrs::new().family(ct::Family::Monospace);
        buffer.set_text(raw, SAMPLE, &attrs, ct::Shaping::Advanced, None);
        buffer.shape_until_scroll(raw, false);
        let broken = broken_faces(
            buffer
                .layout_runs()
                .flat_map(|run| run.glyphs.iter())
                .map(|g| (g.font_id, g.w)),
        );
        if broken.is_empty() {
            break;
        }
        let db = raw.db_mut();
        for id in broken {
            db.remove_face(id);
        }
    }
}

/// The faces, among `(face, advance)` per shaped glyph, that gave a glyph a
/// non-finite ADVANCE. Only the advance indicts a face: a glyph's x is where
/// the glyphs before it ended, so after one broken glyph every later glyph's
/// x is infinite too — the primary monospace face's own spaces between the
/// sample's words included, and judging by x removed that face along with
/// the broken one, leaving the whole code view in a fallback font.
fn broken_faces<Id: Ord + Copy>(glyphs: impl IntoIterator<Item = (Id, f32)>) -> Vec<Id> {
    let mut broken: Vec<Id> = glyphs
        .into_iter()
        .filter(|(_, advance)| !advance.is_finite())
        .map(|(face, _)| face)
        .collect();
    broken.sort();
    broken.dedup();
    broken
}

/// Measure the advance width of one monospace glyph at `font_size`.
/// Paragraph shaping is done by the associated type, so no renderer instance
/// is needed — the type parameter only selects the paragraph implementation.
fn measure_char_width<Renderer>(font_size: f32) -> f32
where
    Renderer: text::Renderer<Font = Font>,
{
    ensure_usable_fonts();
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
    use super::{ColMap, ident_range};

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn word_bounds_finds_identifier() {
        // "    let origin = 1;" — 'o' of origin is at column 8.
        let c = chars("    let origin = 1;");
        assert_eq!(ident_range(&c, 8), Some((8, 14))); // "origin"
        assert_eq!(ident_range(&c, 11), Some((8, 14))); // mid-word
        assert_eq!(ident_range(&c, 4), Some((4, 7))); // "let"
    }

    #[test]
    fn word_bounds_none_on_whitespace_or_punct() {
        let c = chars("a + b");
        assert_eq!(ident_range(&c, 1), None); // space
        assert_eq!(ident_range(&c, 2), None); // '+'
        assert_eq!(ident_range(&c, 99), None); // past end
    }

    #[test]
    fn word_bounds_includes_underscore_and_digits() {
        let c = chars("foo_bar2");
        assert_eq!(ident_range(&c, 0), Some((0, 8)));
    }

    /// Each row finds exactly its own highlights (given in any order) and its
    /// find matches, with the current match marked, without scanning the
    /// whole list — and the find matches are borrowed, not copied per view.
    #[test]
    fn highlights_are_looked_up_per_line() {
        use super::{CodeView, Hl, HlKind};
        let lines = crate::highlight::plain_lines("a\nb\nc\nd\n");
        let hl = |line, kind| Hl {
            line,
            col0: 0,
            col1: 1,
            kind,
        };
        let matches = [(0, 0, 1), (1, 0, 2), (1, 4, 6), (3, 0, 1)];
        let cv: CodeView<'_, ()> = CodeView::new(
            &lines,
            1,
            13.0,
            20.0,
            iced::Color::WHITE,
            |_| (),
            |_| (),
            |_, _| (),
        )
        .highlights(vec![
            hl(3, HlKind::Occurrence),
            hl(1, HlKind::Bracket),
            hl(3, HlKind::DiagError),
        ])
        .find_matches(&matches, 2);
        let kinds = |line| {
            cv.highlights_on(line)
                .map(|h| (h.line, h.col0, h.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(1),
            [
                (1, 0, HlKind::Bracket),
                (1, 0, HlKind::FindMatch),
                (1, 4, HlKind::FindCurrent)
            ]
        );
        // Caller order within a line is kept (stable sort).
        assert_eq!(
            kinds(3),
            [
                (3, 0, HlKind::Occurrence),
                (3, 0, HlKind::DiagError),
                (3, 0, HlKind::FindMatch)
            ]
        );
        assert_eq!(kinds(2), []);
    }

    /// The column map is the one place bytes, chars and graphemes meet:
    /// every conversion must agree with a direct count over the text.
    #[test]
    fn col_map_converts_bytes_chars_and_graphemes() {
        // a · 日 · — · e+U+0301 · 👨‍👩‍👧 (5 chars, one grapheme) · b
        let text = "a日—e\u{301}\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b";
        let map = ColMap::new(text);
        let chars: Vec<(usize, char)> = text.char_indices().collect();
        assert_eq!(map.chars, chars.len());
        // Byte → char: every char's own byte offset, and the end.
        for (c, &(b, _)) in chars.iter().enumerate() {
            assert_eq!(map.char_of_byte(b), c, "byte {b}");
        }
        assert_eq!(map.char_of_byte(text.len()), chars.len());
        // Graphemes: a, 日, —, é (2 chars), the family (5 chars), b.
        assert_eq!(map.graphemes(), 6);
        let starts: Vec<usize> = (0..=6).map(|g| map.grapheme_start(g)).collect();
        assert_eq!(starts, [0, 1, 2, 3, 5, 10, 11]);
        // A char inside a cluster belongs to the cluster; it is no boundary.
        assert_eq!(map.grapheme_of_char(4), 3); // the combining acute
        assert_eq!(map.grapheme_of_char(7), 4); // inside the family
        assert!(!map.is_boundary(4) && !map.is_boundary(7));
        assert!(map.is_boundary(5) && map.is_boundary(10) && map.is_boundary(11));
        assert_eq!(map.grapheme_of_char(10), 5); // 'b'
        assert_eq!(map.grapheme_of_char(11), 6); // end
        // ASCII needs no table and is the identity.
        let ascii = ColMap::new("let x = 1;");
        assert!(ascii.table.is_none());
        assert_eq!(ascii.char_of_byte(4), 4);
        assert_eq!(ascii.grapheme_of_char(4), 4);
        assert_eq!(ascii.grapheme_start(4), 4);
    }
}

#[cfg(test)]
mod splice_tests {
    use super::CodeView;
    use crate::highlight::plain_lines;
    use std::collections::HashMap;

    #[derive(Debug, Clone)]
    enum Msg {}

    type Hints = HashMap<usize, Vec<(usize, String)>>;

    fn with_hints<'a>(
        lines: &'a [crate::highlight::HlLine],
        hints: &'a Hints,
    ) -> CodeView<'a, Msg> {
        CodeView::new(
            lines,
            80,
            13.0,
            20.0,
            iced::Color::WHITE,
            |_| unreachable!(),
            |_| unreachable!(),
            |_, _| unreachable!(),
        )
        .inlay_hints(hints, iced::Color::WHITE)
    }

    /// The visual↔source column mappings are inverses of each other: a click
    /// after an inlay chip must resolve to the source column the character
    /// actually has, and a hit inside a chip snaps to the chip's anchor.
    #[test]
    fn spliced_and_unspliced_are_inverses() {
        // "let point = origin();" with ": Point" chip after `point` (col 9)
        // and a "x:" chip before the call's argument position (col 19).
        let lines = plain_lines("let point = origin();");
        let hints: Hints = HashMap::from([(0, vec![(9, ": Point".into()), (19, "x:".into())])]);
        let cv = with_hints(&lines, &hints);

        for src_col in 0..25 {
            let visual = cv.spliced_col(0, src_col, true);
            assert_eq!(
                cv.unspliced_col(0, visual),
                src_col,
                "round-trip at source col {src_col}"
            );
        }
        // Columns before the first chip are identity.
        assert_eq!(cv.unspliced_col(0, 5), 5);
        // A hit inside the first chip (visual 9..16) snaps to its anchor.
        for inside in 10..16 {
            assert_eq!(cv.unspliced_col(0, inside), 9);
        }
        // Just past the first chip: visual 16 is source 9 (the char pushed
        // right by the 7-char label).
        assert_eq!(cv.unspliced_col(0, 16), 9);
        assert_eq!(cv.unspliced_col(0, 17), 10);

        // No hints on the line: identity both ways.
        let none: Hints = HashMap::from([(0, vec![])]);
        let plain = with_hints(&lines, &none);
        assert_eq!(plain.unspliced_col(0, 12), 12);
        assert_eq!(plain.spliced_col(0, 12, true), 12);
    }
}

#[cfg(test)]
mod scroll_tests {
    use super::CodeView;
    use crate::highlight::plain_lines;
    use iced::widget::scrollable::{self, Direction, Scrollbar};
    use iced::{Event, Fill, Point, mouse};

    #[derive(Debug, Clone)]
    enum Msg {
        Hit,
        Scrolled(scrollable::Viewport),
    }

    /// Regression test for horizontal scrolling in the code view: a wheel
    /// event with an x delta over the scrollable must move the horizontal
    /// offset (the content is wider than the viewport).
    #[test]
    fn wheel_scrolls_horizontally() {
        let src = (0..200)
            .map(|i| format!("line{i} {}", "x".repeat(400)))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = plain_lines(&src);
        let code = CodeView::new(
            &lines,
            410,
            13.0,
            20.0,
            iced::Color::WHITE,
            |_| Msg::Hit,
            |_| Msg::Hit,
            |_, _| Msg::Hit,
        );
        let elem: iced::Element<'_, Msg> = scrollable::Scrollable::new(code)
            .on_scroll(Msg::Scrolled)
            .direction(Direction::Both {
                vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
            })
            .width(Fill)
            .height(Fill)
            .into();
        let mut sim = iced_test::simulator(elem);
        sim.point_at(Point::new(400.0, 300.0));
        let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
        })]);
        let offsets: Vec<f32> = sim
            .into_messages()
            .filter_map(|m| match m {
                Msg::Scrolled(v) => Some(v.absolute_offset().x),
                _ => None,
            })
            .collect();
        assert!(
            offsets.last().copied().unwrap_or(0.0) > 0.0,
            "horizontal wheel did not scroll: {offsets:?}"
        );
    }

    /// R4-19, the same defect one layer in: the blame annotation is drawn from
    /// the *inlay-spliced* paragraph's width, so a caret line carrying both a
    /// chip and the annotation needs room for both. The extent used to take the
    /// max of (line + chips) and (line + annotation), never their sum, so the
    /// last `chip` columns of the annotation were drawn outside the content
    /// width and could not be scrolled to.
    #[test]
    fn content_width_covers_blame_past_an_inlay_chip() {
        use std::collections::HashMap;
        let src = (0..50)
            .map(|i| format!("let v{i} = f()"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = plain_lines(&src);
        let max_cols = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|(t, _)| t.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        // Long enough that the content overflows the simulator viewport with and
        // without the chip, so both measurements are real content widths.
        let annotation = "Ada Lovelace, 3 days ago · rework the retry budget so a cold \
                          cache cannot stall the first request"
            .to_string();
        let chip = ": HashMap<String, Vec<u8>>".to_string();
        let width_of = |hints: &HashMap<usize, Vec<(usize, String)>>| -> f32 {
            let code = CodeView::new(
                &lines,
                max_cols,
                13.0,
                20.0,
                iced::Color::WHITE,
                |_| Msg::Hit,
                |_| Msg::Hit,
                |_, _| Msg::Hit,
            )
            .inlay_hints(hints, iced::Color::WHITE)
            .blame(Some((1, annotation.clone())));
            let elem: iced::Element<'_, Msg> = scrollable::Scrollable::new(code)
                .on_scroll(Msg::Scrolled)
                .direction(Direction::Both {
                    vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                    horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
                })
                .width(Fill)
                .height(Fill)
                .into();
            let mut sim = iced_test::simulator(elem);
            sim.point_at(Point::new(400.0, 300.0));
            let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels {
                    x: -10_000.0,
                    y: 0.0,
                },
            })]);
            sim.into_messages()
                .filter_map(|m| match m {
                    Msg::Scrolled(v) => Some(v.content_bounds().width),
                    _ => None,
                })
                .last()
                .unwrap_or(0.0)
        };

        let blame_only = width_of(&HashMap::new());
        let with_chip = width_of(&HashMap::from([(1, vec![(6usize, chip.clone())])]));
        // ~7.8px/char at this size; the chip is 26 chars, so the extent has to
        // grow by roughly 200px on top of the annotation's own width.
        assert!(
            with_chip > blame_only + chip.chars().count() as f32 * 5.0,
            "inlay chip did not widen the blame line's extent: \
             blame_only={blame_only}, with_chip={with_chip}"
        );
    }

    /// The other thing drawn past a line's end: a collapsed fold header shows a
    /// dim "⋯" one column out, in a two-column box. On the file's widest line
    /// that cue used to fall outside the content width entirely — it was never
    /// part of the extent at all, unlike the summary and blame arms.
    #[test]
    fn content_width_covers_the_collapsed_cue() {
        use std::collections::HashSet;
        let src = (0..50)
            .map(|i| format!("fn f{i}() {}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = plain_lines(&src);
        let max_cols = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|(t, _)| t.chars().count())
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        let headers: HashSet<usize> = HashSet::from([1]);
        let none: HashSet<usize> = HashSet::new();
        let one: HashSet<usize> = HashSet::from([1]);
        let width_of = |collapsed: &HashSet<usize>| -> f32 {
            let code = CodeView::new(
                &lines,
                max_cols,
                13.0,
                20.0,
                iced::Color::WHITE,
                |_| Msg::Hit,
                |_| Msg::Hit,
                |_, _| Msg::Hit,
            )
            .folds(None, &headers, collapsed);
            let elem: iced::Element<'_, Msg> = scrollable::Scrollable::new(code)
                .on_scroll(Msg::Scrolled)
                .direction(Direction::Both {
                    vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                    horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
                })
                .width(Fill)
                .height(Fill)
                .into();
            let mut sim = iced_test::simulator(elem);
            sim.point_at(Point::new(400.0, 300.0));
            let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels {
                    x: -10_000.0,
                    y: 0.0,
                },
            })]);
            sim.into_messages()
                .filter_map(|m| match m {
                    Msg::Scrolled(v) => Some(v.content_bounds().width),
                    _ => None,
                })
                .last()
                .unwrap_or(0.0)
        };

        let expanded = width_of(&none);
        let collapsed = width_of(&one);
        // The cue needs three columns (one gap plus its two-column box); at
        // ~7.8px/char that is ~23px, so two columns of growth is proof enough.
        assert!(
            collapsed > expanded + 2.0 * 7.0,
            "collapsed cue did not widen the scroll extent: \
             expanded={expanded}, collapsed={collapsed}"
        );
    }
}

/// Hit-testing on lines where display columns, bytes and graphemes diverge.
/// Every click is aimed at a glyph's real on-screen geometry — read off the
/// very paragraph the widget draws — and the reported column must be that
/// glyph's display column (not its byte offset, not the next column).
#[cfg(test)]
mod hit_tests {
    use super::{CodeView, GUTTER_CHARS, Rounding, measure_char_width};
    use crate::highlight::plain_lines;
    use iced::advanced::text::Paragraph as _;
    use iced::{Event, Point, mouse};
    use unicode_segmentation::UnicodeSegmentation;

    type P = <iced::Renderer as iced::advanced::text::Renderer>::Paragraph;

    const FONT: f32 = 13.0;
    const LH: f32 = 20.0;

    #[derive(Debug, Clone, PartialEq)]
    enum Msg {
        Press(usize, usize),
        Drag(usize, usize),
        Context(usize, usize),
        Hover(usize, usize),
    }

    fn view(lines: &[crate::highlight::HlLine]) -> CodeView<'_, Msg> {
        // A generous width: before the first frame the extent is only the
        // column estimate, and these lines are wider than their columns.
        CodeView::new(
            lines,
            200,
            FONT,
            LH,
            iced::Color::WHITE,
            |(l, c)| Msg::Press(l, c),
            |(l, c)| Msg::Drag(l, c),
            |(l, c), _| Msg::Context(l, c),
        )
        .on_hover(|(l, c), _| Msg::Hover(l, c))
    }

    fn gutter() -> f32 {
        GUTTER_CHARS as f32 * measure_char_width::<iced::Renderer>(FONT)
    }

    /// Per grapheme of a line: its first display column and its left/right x
    /// on screen (relative to the text origin).
    type Graphemes = Vec<(usize, f32, f32)>;

    /// Display text of each line and its grapheme geometry.
    fn geometry(src: &str) -> Vec<(String, Graphemes)> {
        let lines = plain_lines(src);
        let cv = view(&lines);
        (0..lines.len())
            .map(|i| {
                let text: String = lines[i].spans.iter().map(|(t, _)| t.as_str()).collect();
                let shaped = cv.shape::<P>(i);
                let mut col = 0;
                let mut out = Vec::new();
                for (g, grapheme) in text.graphemes(false).enumerate() {
                    let x0 = shaped.paragraph.grapheme_position(0, g).unwrap().x;
                    let x1 = shaped.paragraph.grapheme_position(0, g + 1).unwrap().x;
                    out.push((col, x0, x1));
                    col += grapheme.chars().count();
                }
                (text, out)
            })
            .collect()
    }

    /// Press (and release) the left button at `(x, row)` for each probe;
    /// returns the messages in order.
    fn click_all(src: &str, probes: &[(f32, usize)]) -> Vec<Msg> {
        let lines = plain_lines(src);
        let mut sim = iced_test::simulator(view(&lines));
        for &(x, row) in probes {
            sim.point_at(Point::new(gutter() + x, row as f32 * LH + LH * 0.5));
            let _ = sim.simulate([
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ]);
        }
        sim.into_messages().collect()
    }

    /// Clicking anywhere on a glyph — its middle or its right quarter —
    /// reports that glyph's display column, on lines mixing CJK, an em dash,
    /// a ZWJ emoji sequence, a combining mark and tabs. The old code reported
    /// the shaper's BYTE offset (every CJK char left of the pointer added
    /// two), rounded to the nearest gap (the right half of a glyph answered
    /// the next column), and ignored tab expansion nowhere but by luck.
    #[test]
    fn clicks_resolve_to_the_display_column_of_the_glyph_under_the_pointer() {
        let src = "日本語のx = 1;\n\
                   a — b\n\
                   a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b = 2\n\
                   \tfoo\t日本\n\
                   e\u{301}x";
        let geo = geometry(src);
        let mut probes = Vec::new();
        let mut expected = Vec::new();
        for (row, (_, graphemes)) in geo.iter().enumerate() {
            for &(col, x0, x1) in graphemes {
                for frac in [0.5, 0.8] {
                    probes.push((x0 + (x1 - x0) * frac, row));
                    expected.push(Msg::Press(row, col));
                }
            }
        }
        let got: Vec<Msg> = click_all(src, &probes)
            .into_iter()
            .filter(|m| matches!(m, Msg::Press(..)))
            .collect();
        assert_eq!(got, expected);
        // Spot checks of what that means on the lines that motivated it.
        assert_eq!(
            geo[0].1[4].0, 4,
            "`x` after four CJK chars is column 4, not byte 12"
        );
        assert_eq!(
            geo[2].1[2].0, 6,
            "`b` after a 5-char emoji cluster is column 6"
        );
        assert_eq!(geo[3].1[4].0, 4, "`f` after a tab is display column 4");
    }

    /// The LEFT half of a glyph is that glyph too: the shaper snaps a
    /// pointer there to the glyph's left boundary, which is its own column —
    /// on wide glyphs, clusters and after tabs alike.
    #[test]
    fn clicks_in_the_left_half_of_a_glyph_resolve_to_that_glyph() {
        let src = "日本語のx = 1;\n\
                   a — b\n\
                   a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b = 2\n\
                   \tfoo\t日本\n\
                   e\u{301}x";
        let geo = geometry(src);
        let mut probes = Vec::new();
        let mut expected = Vec::new();
        for (row, (_, graphemes)) in geo.iter().enumerate() {
            for &(col, x0, x1) in graphemes {
                for frac in [0.05, 0.3] {
                    probes.push((x0 + (x1 - x0) * frac, row));
                    expected.push(Msg::Press(row, col));
                }
            }
        }
        let got: Vec<Msg> = click_all(src, &probes)
            .into_iter()
            .filter(|m| matches!(m, Msg::Press(..)))
            .collect();
        assert_eq!(got, expected);
    }

    /// Past the last glyph a click lands at the end of the line (the caret
    /// can sit after the last character), and the gutter reports column 0.
    #[test]
    fn clicks_past_the_end_and_in_the_gutter() {
        let src = "日本";
        let geo = geometry(src);
        let end_x = geo[0].1.last().unwrap().2;
        let msgs = click_all(src, &[(end_x + 30.0, 0), (-gutter() * 0.5, 0)]);
        assert_eq!(msgs, vec![Msg::Press(0, 2), Msg::Press(0, 0)]);
    }

    /// A drag extends the selection to the NEAREST gap, so it can take in a
    /// glyph by crossing its middle — and the last glyph of a line without
    /// leaving the text.
    #[test]
    fn drags_round_to_the_nearest_gap() {
        let src = "日本x";
        let geo = geometry(src);
        let lines = plain_lines(src);
        let mut sim = iced_test::simulator(view(&lines));
        let at = |x: f32| Point::new(gutter() + x, LH * 0.5);
        sim.point_at(at(1.0));
        let _ = sim.simulate([Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))]);
        let mut expected = vec![Msg::Press(0, 0)];
        for (g, &(_, x0, x1)) in geo[0].1.iter().enumerate() {
            for (frac, col) in [(0.3, g), (0.7, g + 1)] {
                let p = at(x0 + (x1 - x0) * frac);
                sim.point_at(p);
                let _ = sim.simulate([Event::Mouse(mouse::Event::CursorMoved { position: p })]);
                expected.push(Msg::Drag(0, col));
            }
        }
        let got: Vec<Msg> = sim
            .into_messages()
            .filter(|m| matches!(m, Msg::Press(..) | Msg::Drag(..)))
            .collect();
        assert_eq!(got, expected);
    }

    /// Hovering reports once per TOKEN: moving across one identifier (and
    /// across the right half of its last character, which used to resolve to
    /// the column after it) does not rebuild the view per column; stepping
    /// onto the next token does.
    #[test]
    fn hover_reports_once_per_token() {
        let src = "let count = 1;";
        let geo = geometry(src);
        let lines = plain_lines(src);
        let mut sim = iced_test::simulator(view(&lines));
        let centre = |g: usize, frac: f32| {
            let (_, x0, x1) = geo[0].1[g];
            Point::new(gutter() + x0 + (x1 - x0) * frac, LH * 0.5)
        };
        // `count` spans columns 4..9; then the space (9), `=` (10), space (11).
        for p in [
            centre(4, 0.5),
            centre(5, 0.5),
            centre(8, 0.9), // right half of `t`: still `count`
            centre(9, 0.5),
            centre(10, 0.5),
            centre(11, 0.5),
        ] {
            sim.point_at(p);
            let _ = sim.simulate([Event::Mouse(mouse::Event::CursorMoved { position: p })]);
        }
        let hovers: Vec<Msg> = sim
            .into_messages()
            .filter(|m| matches!(m, Msg::Hover(..)))
            .collect();
        assert_eq!(
            hovers,
            vec![
                Msg::Hover(0, 4),
                Msg::Hover(0, 9),
                Msg::Hover(0, 10),
                Msg::Hover(0, 11)
            ]
        );
    }

    /// The right-click menu resolves the word under the pointer the same way.
    #[test]
    fn context_menu_uses_the_glyph_under_the_pointer() {
        let src = "日本 name";
        let geo = geometry(src);
        let lines = plain_lines(src);
        let mut sim = iced_test::simulator(view(&lines));
        let (col, x0, x1) = geo[0].1[6]; // the `e` of `name`
        sim.point_at(Point::new(gutter() + x0 + (x1 - x0) * 0.9, LH * 0.5));
        let _ = sim.simulate([Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Right,
        ))]);
        let msgs: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(msgs, vec![Msg::Context(0, col)]);
        assert_eq!(col, 6);
    }

    /// Inlay chips spliced into the paragraph shift the glyphs after them; a
    /// click on a real character past a chip on a CJK line still reports
    /// that character's SOURCE column, and a click inside the chip snaps to
    /// its anchor.
    #[test]
    fn clicks_past_an_inlay_chip_on_a_wide_line() {
        let src = "let 名前 = f();";
        let lines = plain_lines(src);
        // A ": 文字列" chip after `名前` (source column 6).
        let hints =
            std::collections::HashMap::from([(0usize, vec![(6usize, ": 文字列".to_string())])]);
        let cv = view(&lines).inlay_hints(&hints, iced::Color::WHITE);
        let shaped = cv.shape::<P>(0);
        // Spliced text: "let 名前: 文字列 = f();" — `=` is at spliced column 13
        // (source column 7); the chip covers spliced columns 6..11.
        let x = |c: usize| shaped.boundary_x(c, false, 7.8);
        let mid = |c: usize| (x(c) + x(c + 1)) / 2.0;
        assert_eq!(shaped.column_at(mid(13), LH, Rounding::Under), 13);
        let mut sim = iced_test::simulator(cv);
        for c in [13, 8] {
            sim.point_at(Point::new(gutter() + mid(c), LH * 0.5));
            let _ = sim.simulate([
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ]);
        }
        let msgs: Vec<Msg> = sim.into_messages().collect();
        assert_eq!(msgs, vec![Msg::Press(0, 8), Msg::Press(0, 6)]);
    }
}

/// Scroll extent and shaping cost on lines whose width is not their column
/// count: wide glyphs (E1-3) and pathologically long lines (E1-5).
#[cfg(test)]
mod extent_tests {
    use super::{CodeView, GUTTER_CHARS, MAX_SHAPED_COLS, Rounding, measure_char_width};
    use crate::highlight::plain_lines;
    use iced::advanced::text::Paragraph as _;
    use iced::widget::scrollable::{self, Direction, Scrollbar};
    use iced::{Event, Fill, Point, mouse};

    type P = <iced::Renderer as iced::advanced::text::Renderer>::Paragraph;

    #[derive(Debug, Clone)]
    enum Msg {
        Hit,
        Scrolled(scrollable::Viewport),
    }

    fn view(lines: &[crate::highlight::HlLine], max_cols: usize) -> CodeView<'_, Msg> {
        CodeView::new(
            lines,
            max_cols,
            13.0,
            20.0,
            iced::Color::WHITE,
            |_| Msg::Hit,
            |_| Msg::Hit,
            |_, _| Msg::Hit,
        )
    }

    /// The monospace fallback must give CJK glyphs a real, finite advance —
    /// about two columns — not the infinite one the system's bitmap GB18030
    /// face produced.
    #[test]
    fn cjk_glyphs_have_finite_advances() {
        let lines = plain_lines("漢字漢字");
        let shaped = view(&lines, 4).shape::<P>(0);
        let width = shaped.paragraph.min_bounds().width;
        let cw = measure_char_width::<iced::Renderer>(13.0);
        assert!(width.is_finite(), "CJK line shaped to width {width}");
        assert!(
            width > 4.0 * cw * 1.2,
            "CJK glyphs narrower than wide: {width} for 4 glyphs at {cw}"
        );
    }

    /// A line of wide glyphs is wider than its column count says. Once it has
    /// been drawn, the scrollable's content must reach its end — the column
    /// estimate alone (150 ASCII columns here) left the last third of the
    /// 120-glyph CJK line unreachable.
    #[test]
    fn content_width_covers_a_line_of_wide_glyphs() {
        let cjk = "漢".repeat(120);
        let src = format!("{}\n{cjk}\n", "x".repeat(150));
        let lines = plain_lines(&src);
        let cjk_width = view(&lines, 150).shape::<P>(1).paragraph.min_bounds().width;
        let cw = measure_char_width::<iced::Renderer>(13.0);
        assert!(
            cjk_width > 150.0 * cw,
            "precondition: the CJK line is the widest"
        );

        let elem: iced::Element<'_, Msg> = scrollable::Scrollable::new(view(&lines, 150))
            .on_scroll(Msg::Scrolled)
            .direction(Direction::Both {
                vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
            })
            .width(Fill)
            .height(Fill)
            .into();
        let mut sim = iced_test::simulator(elem);
        // Two frames: the first draws (and measures) the lines, the second's
        // redraw request relayouts with the measured width.
        let _ = sim.snapshot(&iced::Theme::Dark);
        let _ = sim.snapshot(&iced::Theme::Dark);
        sim.point_at(Point::new(400.0, 300.0));
        let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels {
                x: -100_000.0,
                y: 0.0,
            },
        })]);
        let content = sim
            .into_messages()
            .filter_map(|m| match m {
                Msg::Scrolled(v) => Some(v.content_bounds().width),
                Msg::Hit => None,
            })
            .last()
            .unwrap_or(0.0);
        let needed = GUTTER_CHARS as f32 * cw + cjk_width;
        assert!(
            content >= needed,
            "content width {content} does not reach the end of the CJK line ({needed})"
        );
    }

    /// A multi-megabyte minified line is shaped only up to the cap: hit tests
    /// on it (a click, a hover on a row outside the paragraph cache) stay
    /// cheap, and a click past the cap lands at the end of what is shown.
    #[test]
    fn a_huge_line_is_shaped_only_up_to_the_cap() {
        let src = "ab".repeat(1_000_000); // 2M columns
        let lines = plain_lines(&src);
        let cv = view(&lines, src.len());
        let started = std::time::Instant::now();
        let shaped = cv.shape::<P>(0);
        assert!(shaped.elided, "the line is marked as cut");
        assert_eq!(shaped.map.chars, MAX_SHAPED_COLS);
        let end = shaped.paragraph.min_bounds().width;
        assert_eq!(
            shaped.column_at(end + 100.0, 20.0, Rounding::Under),
            MAX_SHAPED_COLS
        );
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(2),
            "shaping a capped line took {took:?}"
        );
        // The extent estimate is capped too, so the view is not 2M columns wide.
        assert!(cv.visual_max_cols(7.8) <= MAX_SHAPED_COLS + 2);
    }

    /// Past the cut nothing is shown, so nothing may be drawn there: a caret,
    /// a selection end or a find match beyond column 4096 lands ON the `…`
    /// marker — never out past it. An uncut line still continues at one
    /// advance per column, so a caret after its last glyph stays visible.
    #[test]
    fn columns_past_the_cut_collapse_onto_the_elision_marker() {
        let src = format!("{}\nshort\n", "ab".repeat(4000)); // 8000 columns
        let lines = plain_lines(&src);
        let cv = view(&lines, 8000);
        let cw = 7.8;
        let cut = cv.shape::<P>(0);
        assert!(cut.elided);
        let end = cut.paragraph.min_bounds().width;
        let marker = super::ELISION_MARKER_COLS * cw;
        for c in [MAX_SHAPED_COLS + 1, MAX_SHAPED_COLS + 700, 7999, 1_000_000] {
            assert_eq!(cut.boundary_x(c, false, cw), end, "left edge of col {c}");
            assert_eq!(
                cut.boundary_x(c, true, cw),
                end + marker,
                "right edge of col {c}"
            );
        }
        // The cut itself is the end of the shown text.
        assert_eq!(cut.boundary_x(MAX_SHAPED_COLS, true, cw), end);
        // Inside the shown prefix, nothing changes.
        assert!(cut.boundary_x(10, false, cw) < end);
        let uncut = cv.shape::<P>(1);
        let short_end = uncut.paragraph.min_bounds().width;
        assert_eq!(uncut.boundary_x(7, false, cw), short_end + 2.0 * cw);
    }

    /// A summary handed in (the viewer's, kept current where its inputs
    /// change) is what the view uses — it is not derived again per build —
    /// and without one the view derives the very same summary itself.
    #[test]
    fn a_precomputed_annotation_summary_is_used_as_given() {
        use super::{Annotations, annotations_of};
        use std::collections::{HashMap, HashSet};
        let lines = plain_lines("fn a() {\n    x();\n}\n");
        let hints = HashMap::from([(1usize, vec![(5usize, ": Unit".to_string())])]);
        let inactive = HashSet::from([2usize]);
        let derived = annotations_of(&lines, Some(&hints), Some(&inactive), None);
        let plain = view(&lines, 8)
            .inlay_hints(&hints, iced::Color::WHITE)
            .inactive(&inactive);
        assert_eq!(plain.annotation_summary(), derived);
        let given = Annotations {
            signature: 42,
            hinted_cols: 900,
            collapsed_cols: 0,
        };
        let fed = view(&lines, 8)
            .inlay_hints(&hints, iced::Color::WHITE)
            .inactive(&inactive)
            .annotations(given);
        assert_eq!(fed.annotation_signature(), 42);
        assert_eq!(fed.visual_max_cols(7.8), 900);
        assert_eq!(fed.cache_key().4, 42, "the paragraph cache keys on it");
    }
}

/// The font probe (`ensure_usable_fonts`): what indicts a face, and that the
/// face the code view actually reads in survives it.
#[cfg(test)]
mod font_tests {
    use super::{broken_faces, remove_broken_faces};
    use iced::advanced::graphics::text::cosmic_text as ct;

    /// A renderer that draws nothing and records every `fill_text`: its
    /// content and shaping. Paragraphs are iced's real ones (CPU-side), so a
    /// widget lays out and measures as it does on screen.
    #[derive(Default)]
    struct Recorder {
        texts: Vec<(String, iced::advanced::text::Shaping)>,
    }

    impl iced::advanced::Renderer for Recorder {
        fn start_layer(&mut self, _: iced::Rectangle) {}
        fn end_layer(&mut self) {}
        fn start_transformation(&mut self, _: iced::Transformation) {}
        fn end_transformation(&mut self) {}
        fn fill_quad(&mut self, _: iced::advanced::renderer::Quad, _: impl Into<iced::Background>) {
        }
        fn reset(&mut self, _: iced::Rectangle) {}
        fn allocate_image(
            &mut self,
            _: &iced::advanced::image::Handle,
            _: impl FnOnce(Result<iced::advanced::image::Allocation, iced::advanced::image::Error>)
            + Send
            + 'static,
        ) {
        }
    }

    impl iced::advanced::text::Renderer for Recorder {
        type Font = iced::Font;
        type Paragraph = iced::advanced::graphics::text::Paragraph;
        type Editor = iced::advanced::graphics::text::Editor;
        const ICON_FONT: iced::Font = iced::Font::DEFAULT;
        const CHECKMARK_ICON: char = 'x';
        const ARROW_DOWN_ICON: char = 'v';
        const SCROLL_UP_ICON: char = '^';
        const SCROLL_DOWN_ICON: char = 'v';
        const SCROLL_LEFT_ICON: char = '<';
        const SCROLL_RIGHT_ICON: char = '>';
        const ICED_LOGO: char = 'i';
        fn default_font(&self) -> iced::Font {
            iced::Font::DEFAULT
        }
        fn default_size(&self) -> iced::Pixels {
            iced::Pixels(16.0)
        }
        fn fill_paragraph(
            &mut self,
            _: &Self::Paragraph,
            _: iced::Point,
            _: iced::Color,
            _: iced::Rectangle,
        ) {
        }
        fn fill_editor(
            &mut self,
            _: &Self::Editor,
            _: iced::Point,
            _: iced::Color,
            _: iced::Rectangle,
        ) {
        }
        fn fill_text(
            &mut self,
            text: iced::advanced::text::Text<String, Self::Font>,
            _: iced::Point,
            _: iced::Color,
            _: iced::Rectangle,
        ) {
            self.texts.push((text.content, text.shaping));
        }
    }

    /// A15: the inline blame names people and quotes commit summaries, which
    /// are not ASCII everywhere. The code view draws it shaped with font
    /// fallback — checked on the draw itself — so a CJK author's name is
    /// drawn, with this machine's fonts a real glyph for every character,
    /// rather than the monospace face's missing-glyph boxes.
    #[test]
    fn a_non_ascii_blame_annotation_is_shaped_with_fallback() {
        use iced::advanced::Widget;
        use iced::advanced::text::Shaping;
        use iced::advanced::widget::Tree;
        let blame = "张三, 3 days ago · 修复 bug";
        let lines = crate::highlight::plain_lines("fn main() {}\n");
        let view = crate::codeview::CodeView::new(
            &lines,
            12,
            13.0,
            20.0,
            iced::Color::WHITE,
            |_| (),
            |_| (),
            |_, _| (),
        )
        .blame(Some((0, blame.to_string())));
        let mut view = view;
        let widget: &mut dyn Widget<(), iced::Theme, Recorder> = &mut view;
        let mut tree = Tree::new(&*widget);
        let mut renderer = Recorder::default();
        let viewport = iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(800.0, 400.0));
        let node = widget.layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(iced::Size::ZERO, viewport.size()),
        );
        widget.draw(
            &tree,
            &mut renderer,
            &iced::Theme::Dark,
            &iced::advanced::renderer::Style::default(),
            iced::advanced::Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &viewport,
        );
        let shaping = renderer
            .texts
            .iter()
            .find(|(content, _)| content == blame)
            .map(|(_, shaping)| *shaping)
            .expect("the blame annotation was not drawn");
        assert!(matches!(shaping, Shaping::Advanced), "drawn {shaping:?}");
        let glyph_ids = |shaping: ct::Shaping| {
            let mut fs = ct::FontSystem::new();
            let mut buffer = ct::Buffer::new(&mut fs, ct::Metrics::new(13.0, 20.0));
            buffer.set_size(&mut fs, Some(1.0e6), Some(40.0));
            let attrs = ct::Attrs::new().family(ct::Family::Monospace);
            buffer.set_text(&mut fs, blame, &attrs, shaping, None);
            buffer.shape_until_scroll(&mut fs, false);
            buffer
                .layout_runs()
                .flat_map(|run| run.glyphs.iter().map(|g| g.glyph_id))
                .collect::<Vec<u16>>()
        };
        let drawn = glyph_ids(ct::Shaping::Advanced);
        assert!(!drawn.is_empty());
        assert!(
            drawn.iter().all(|&id| id != 0),
            "characters drawn as missing-glyph boxes: {drawn:?}"
        );
        // What `Basic` — the shaping this used to get — draws instead.
        assert!(glyph_ids(ct::Shaping::Basic).contains(&0));
    }

    /// Only a non-finite ADVANCE indicts a face. After one broken glyph every
    /// later glyph's x is infinite too, so judging by x also indicted the
    /// faces shaping everything after it — the primary monospace face (the
    /// spaces between the sample's words) first among them.
    #[test]
    fn only_a_non_finite_advance_indicts_a_face() {
        let (bitmap, mono, cjk) = (7u32, 1u32, 3u32);
        let glyphs = [
            (bitmap, f32::INFINITY), // 日: broken metrics
            (mono, 7.8),             // the space after it: x = inf, advance fine
            (cjk, 13.0),
            (bitmap, f32::NAN),
            (mono, 7.8),
        ];
        assert_eq!(broken_faces(glyphs), [bitmap]);
        assert!(broken_faces([(mono, 7.8)]).is_empty());
    }

    /// The face that shapes ASCII in the monospace family is the code view's
    /// face; on this machine's real fonts (a fresh font system, so the
    /// process-wide one is untouched), the probe must leave it in place and
    /// still resolving — whatever broken faces it removes for other scripts.
    #[test]
    fn the_probe_keeps_the_primary_monospace_face() {
        fn ascii_face(fs: &mut ct::FontSystem) -> Option<ct::fontdb::ID> {
            let mut buffer = ct::Buffer::new(fs, ct::Metrics::new(13.0, 20.0));
            buffer.set_size(fs, Some(1.0e6), Some(40.0));
            let attrs = ct::Attrs::new().family(ct::Family::Monospace);
            buffer.set_text(fs, "let x = 1;", &attrs, ct::Shaping::Advanced, None);
            buffer.shape_until_scroll(fs, false);
            let run = buffer.layout_runs().next()?;
            run.glyphs.first().map(|g| g.font_id)
        }
        let mut fs = ct::FontSystem::new();
        // The app is built for macOS, which always has a monospace face:
        // finding none is a failure, not a reason to skip the check.
        let primary = ascii_face(&mut fs).expect("a monospace face shapes ASCII");
        let faces = fs.db().len();
        remove_broken_faces(&mut fs);
        assert!(
            fs.db().face(primary).is_some(),
            "the probe removed the primary monospace face"
        );
        assert_eq!(ascii_face(&mut fs), Some(primary));
        assert!(fs.db().len() <= faces);
    }
}
