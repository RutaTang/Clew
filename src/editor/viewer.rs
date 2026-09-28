//! State for the read-only code viewer: the document (raw source plus its
//! highlighted display lines), the caret, the selection, folds, and the
//! per-document memos. Drawing — including virtualization, which paints only
//! the rows intersecting the viewport — is the [`crate::codeview`] widget's job.
//!
//! Columns: everything here speaks DISPLAY columns ([`Col`]): chars of a line
//! with tabs expanded to four spaces and CR stripped, exactly what the display
//! lines hold. Language servers speak UTF-8 or UTF-16 offsets into the RAW
//! line, in the encoding the server negotiated ([`PositionEncoding`], the one
//! encoding type); [`Col::to_offset`] and [`Col::from_offset`] are the one
//! crossing, applied to the raw line a caller has in hand.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

pub use crate::lsp::client::PositionEncoding;

use crate::analyze;
use crate::codeview::Annotations;
use crate::highlight::HlLine;
use crate::outline::Symbol;

/// Maximum file size we attempt to display.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Inlay hints of a document: 0-based display line → chips `(display column,
/// label)`, sorted by column.
pub type InlayHints = HashMap<usize, Vec<(usize, String)>>;

/// A display column: an index into a line's display chars (tabs expanded to
/// four columns, CR stripped). Distinct from a byte offset and from an LSP
/// character offset, which count the RAW line in the server's encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Col(pub usize);

impl From<Col> for usize {
    fn from(col: Col) -> usize {
        col.0
    }
}

impl Col {
    /// Display width of one raw character.
    fn width(ch: char) -> usize {
        match ch {
            '\r' => 0,
            '\t' => 4,
            _ => 1,
        }
    }

    /// The offset of this display column on `raw_line`, in `enc` units. A
    /// column inside a tab's expansion resolves past the tab; a column past
    /// the end clamps to the line's length.
    pub fn to_offset(self, raw_line: &str, enc: PositionEncoding) -> usize {
        let mut col = 0usize;
        let mut off = 0usize;
        for ch in raw_line.chars() {
            if col >= self.0 {
                break;
            }
            col += Col::width(ch);
            off += enc.units(ch);
        }
        off
    }

    /// The display column at `offset` (in `enc` units) on `raw_line`. An
    /// offset that splits a character (inside a UTF-8 sequence or a UTF-16
    /// surrogate pair — a server should never send one) resolves to the
    /// column after that character; past the end clamps to the line's width.
    pub fn from_offset(raw_line: &str, offset: usize, enc: PositionEncoding) -> Col {
        let mut col = 0usize;
        let mut off = 0usize;
        for ch in raw_line.chars() {
            if off >= offset {
                break;
            }
            col += Col::width(ch);
            off += enc.units(ch);
        }
        Col(col)
    }
}

/// A caret position: (0-based line, 0-based display column).
pub type Pos = (usize, usize);

/// A character-level selection: (anchor, head), each a caret position.
/// Positions are caret-between-characters; the selection covers everything
/// between them in document order. Columns are display columns (tabs expanded).
pub type Selection = (Pos, Pos);

/// A highlighted span on one line: (line, col0, col1) in display columns — the
/// shape [`analyze::occurrences`] returns.
pub type Span = (usize, usize, usize);

/// One memoized answer for the CURRENT buffer: the key it was computed for and
/// the answer itself. `None` means "nothing computed since the buffer changed";
/// a key that does not match means "computed, but for something else".
type Memo<K, V> = std::cell::RefCell<Option<(K, V)>>;

/// A read-only cursor motion, Vim normal-mode style.
#[derive(Debug, Clone, Copy)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    WordForward,
    WordBack,
    FileStart,
    FileEnd,
}

#[derive(Debug, Clone)]
pub struct Viewer {
    pub abs: PathBuf,
    pub rel: String,
    pub lang_key: Option<&'static str>,
    /// Raw file content, kept for copy-to-clipboard fidelity.
    pub source: Arc<String>,
    /// Byte offset where each line of `source` starts (`str::lines`
    /// semantics), so a raw line is an O(1) lookup — diagnostics and every
    /// LSP request used to walk the file from the top with `lines().nth`.
    /// Rebuilt with `source`; shared so a split clones cheaply.
    line_starts: Arc<Vec<usize>>,
    /// Shared so a split showing the same file clones cheaply.
    pub lines: Arc<Vec<HlLine>>,
    /// Widest line in display columns; drives horizontal scroll extent.
    pub max_cols: usize,
    pub symbols: Vec<Symbol>,
    /// Signature line (1-based) -> the author's doc comment, for the hover peek.
    /// Populated off-thread alongside `symbols`; empty until highlighting lands.
    pub docs: HashMap<usize, String>,
    /// LSP inlay hints: 0-based display line -> [(display column, label)],
    /// sorted by column. Empty until the language server answers. Private:
    /// read through [`Viewer::inlay_hints`], replaced ONLY through
    /// [`Viewer::set_inlay_hints`], which keeps [`Viewer::annotations`]
    /// current — a direct write left the view's summary describing the old
    /// chips.
    inlay_hints: InlayHints,
    /// 0-based lines gated off by an inactive `#[cfg(...)]` for the host target,
    /// dimmed as a reading aid. Computed off-thread with the highlighting.
    /// Private like `inlay_hints`: read through [`Viewer::inactive_lines`],
    /// replaced through [`Viewer::set_inactive_lines`].
    inactive_lines: HashSet<usize>,
    pub highlighted: bool,
    pub scroll_y: f32,
    /// Horizontal scroll offset, so pointer positions the code view reports
    /// in content space can be mapped back to the window.
    pub scroll_x: f32,
    pub viewport_h: f32,
    pub target_line: Option<usize>, // 1-based jump target, drawn highlighted
    pub selection: Option<Selection>,
    /// Last clicked position as (0-based line, 0-based display column).
    pub caret: Option<(usize, usize)>,
    /// Foldable regions `(header, end)`; the fold hides `header+1 ..= end`.
    pub folds: Vec<(usize, usize)>,
    /// Header lines of `folds`, as a set for cheap membership tests / borrows.
    pub fold_header_set: HashSet<usize>,
    /// Header lines that are currently collapsed.
    pub collapsed: HashSet<usize>,
    /// Per-line git blame + change status, once loaded for this file.
    pub git: Option<Arc<crate::git::GitInfo>>,
    /// Rendered-markdown items for a `.md`/`.markdown` file (`None` otherwise),
    /// parsed once so the view doesn't re-parse per frame. Shown instead of the
    /// code view unless `show_source` is set.
    pub md: Option<Arc<Vec<iced::widget::markdown::Item>>>,
    /// For a markdown file, show the raw source instead of the rendered view.
    pub show_source: bool,
    /// A Jupyter notebook's prepared cells (`None` for ordinary files). Shown
    /// as the native cell view; `lines`/`source` hold the script projection so
    /// search, goto, and the outline speak in projection lines. Arc'd because
    /// the viewer is `Clone` while prepared segments are not.
    pub notebook: Option<Arc<crate::app::model::NotebookDoc>>,
    /// Notebook cells whose outputs are expanded (collapsed by default).
    pub nb_expanded: HashSet<usize>,
    /// Row → source-line projection when any fold is collapsed; empty means the
    /// identity mapping (every line visible), so the common path allocates none.
    visible: Vec<usize>,
    /// Last bracket match, keyed by the caret it was computed for. The scan
    /// itself is cheap now, but it runs on EVERY view rebuild — a keystroke, a
    /// scroll, a repaint — for a caret that has usually not moved, and each of
    /// those walks the whole enclosing region again for the same answer.
    ///
    /// Keyed on the caret rather than on the buffer, because `set_lines` (the
    /// only place `lines` is replaced after construction, `reload` included)
    /// clears it. A cache keyed on the buffer's address instead would be
    /// unsound: the allocator reuses addresses, so a new buffer of the same
    /// length could inherit the old answer.
    bracket_cache: Memo<Pos, Option<Pos>>,
    /// Last occurrence scan, keyed by the word it was computed for. Same
    /// motivation and same invalidation as `bracket_cache`; the key is the word
    /// alone because the result does not depend on where in the file the caret
    /// sits, only on which identifier is under it.
    occurrence_cache: Memo<String, Vec<Span>>,
    /// The code view's summary of `inlay_hints`, `inactive_lines` and the
    /// collapsed folds, recomputed when one of them changes here — not on
    /// every view build, where hashing every chip label and walking every
    /// annotated line cost O(annotations) per frame (see
    /// [`Viewer::annotations`]).
    annotations: Annotations,
}

/// Parse markdown items for `.md`-family files, so a readme renders as a
/// document instead of raw source. `None` for every other extension.
fn parse_markdown(
    abs: &std::path::Path,
    source: &str,
) -> Option<Arc<Vec<iced::widget::markdown::Item>>> {
    let ext = abs
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    matches!(ext.as_str(), "md" | "markdown" | "mdx").then(|| {
        // Single-threaded UI state; Arc only for cheap clones into
        // iced widgets (markdown items are not Sync).
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(iced::widget::markdown::parse(source).collect())
    })
}

impl Viewer {
    pub fn new(
        abs: PathBuf,
        rel: String,
        lang_key: Option<&'static str>,
        source: Arc<String>,
        lines: Vec<HlLine>,
    ) -> Self {
        let max_cols = max_cols_of(&lines);
        let folds = analyze::fold_ranges(&lines);
        let fold_header_set = folds.iter().map(|&(h, _)| h).collect();
        let md = parse_markdown(&abs, &source);
        Self {
            md,
            show_source: false,
            notebook: None,
            nb_expanded: HashSet::new(),
            abs,
            rel,
            lang_key,
            line_starts: Arc::new(line_starts_of(&source)),
            source,
            lines: Arc::new(lines),
            max_cols,
            symbols: Vec::new(),
            docs: HashMap::new(),
            inlay_hints: HashMap::new(),
            inactive_lines: HashSet::new(),
            highlighted: false,
            scroll_y: 0.0,
            scroll_x: 0.0,
            // Generous default until the first scroll event reports the real
            // viewport; only affects how many rows are materialized.
            viewport_h: 2400.0,
            target_line: None,
            selection: None,
            caret: None,
            folds,
            fold_header_set,
            collapsed: HashSet::new(),
            visible: Vec::new(),
            bracket_cache: std::cell::RefCell::new(None),
            occurrence_cache: std::cell::RefCell::new(None),
            annotations: Annotations::default(),
            git: None,
        }
    }

    /// The code view's summary of this document's annotations — the inlay
    /// hints, the inactive lines and the collapsed folds — for
    /// `CodeView::annotations`. Current as long as `inlay_hints` and
    /// `inactive_lines` are replaced through [`Viewer::set_inlay_hints`] and
    /// [`Viewer::set_inactive_lines`]; folds and content changes go through
    /// this type's own methods and keep it current by themselves.
    pub fn annotations(&self) -> Annotations {
        self.annotations
    }

    /// The inlay hints (0-based display line → chips sorted by column).
    pub fn inlay_hints(&self) -> &InlayHints {
        &self.inlay_hints
    }

    /// The inactive (`cfg`-dimmed) 0-based lines.
    pub fn inactive_lines(&self) -> &HashSet<usize> {
        &self.inactive_lines
    }

    /// Replace the inlay hints (0-based display line → chips sorted by
    /// column) and refresh [`Viewer::annotations`].
    pub fn set_inlay_hints(&mut self, hints: InlayHints) {
        self.inlay_hints = hints;
        self.refresh_annotations();
    }

    /// Replace the inactive (`cfg`-dimmed) lines and refresh
    /// [`Viewer::annotations`].
    pub fn set_inactive_lines(&mut self, lines: HashSet<usize>) {
        self.inactive_lines = lines;
        self.refresh_annotations();
    }

    fn refresh_annotations(&mut self) {
        self.annotations = crate::codeview::annotations_of(
            &self.lines,
            Some(&self.inlay_hints),
            Some(&self.inactive_lines),
            Some(&self.collapsed),
        );
    }

    /// Replace the file's content in place after an on-disk change, keeping the
    /// reader's position (scroll, caret, folds) so the view doesn't jump. The
    /// caret is clamped to the new bounds and stale collapsed headers dropped;
    /// symbols/highlighting are refreshed asynchronously afterwards.
    pub fn reload(&mut self, source: Arc<String>, lines: Vec<HlLine>) {
        self.md = parse_markdown(&self.abs, &source);
        self.line_starts = Arc::new(line_starts_of(&source));
        self.source = source;
        self.set_lines(Arc::new(lines)); // recomputes folds / header set / visible / max_cols
        self.highlighted = false;
        self.symbols.clear();
        self.docs.clear();
        self.inlay_hints.clear();
        self.inactive_lines.clear();
        if let Some((line, col)) = self.caret {
            let line = line.min(self.lines.len().saturating_sub(1));
            self.caret = Some((line, col.min(self.line_len(line))));
        }
        // Drop collapsed headers that no longer head a fold, then reproject.
        self.collapsed.retain(|h| self.fold_header_set.contains(h));
        self.recompute_visible();
        self.refresh_annotations();
    }

    /// Replace the highlighted lines (same line count) and refresh `max_cols`.
    /// Folds are indentation-derived, so the collapsed set stays valid.
    pub fn set_lines(&mut self, lines: Arc<Vec<HlLine>>) {
        self.max_cols = max_cols_of(&lines);
        self.folds = analyze::fold_ranges(&lines);
        self.fold_header_set = self.folds.iter().map(|&(h, _)| h).collect();
        self.lines = lines;
        // Both caches describe the buffer being replaced. This is the ONLY
        // place `lines` is assigned after construction (`reload` goes through
        // here), so clearing here is what makes them correct.
        self.bracket_cache.get_mut().take();
        self.occurrence_cache.get_mut().take();
        self.recompute_visible();
        self.refresh_annotations();
    }

    /// The bracket matching the one at the caret, memoized for this buffer and
    /// caret (see `bracket_cache`). Prefer this to calling
    /// [`analyze::matching_bracket`] directly: the highlight set is rebuilt on
    /// every repaint, and the answer only changes when the caret or the buffer
    /// does.
    pub fn matching_bracket(&self, line: usize, col: usize) -> Option<Pos> {
        if let Some((key, hit)) = *self.bracket_cache.borrow()
            && key == (line, col)
        {
            return hit;
        }
        let hit = analyze::matching_bracket(&self.lines, line, col);
        *self.bracket_cache.borrow_mut() = Some(((line, col), hit));
        hit
    }

    /// Occurrences of `word` in this buffer, memoized (see `occurrence_cache`).
    /// `cap` bounds how many are returned, not how far the scan reads, so it is
    /// not part of the key — every caller passes the same one.
    pub fn occurrences(&self, word: &str, cap: usize) -> Vec<Span> {
        if let Some((key, hits)) = self.occurrence_cache.borrow().as_ref()
            && key == word
        {
            return hits.clone();
        }
        let hits = analyze::occurrences(word, &self.lines, cap);
        *self.occurrence_cache.borrow_mut() = Some((word.to_string(), hits.clone()));
        hits
    }

    // ------------------------------------------------------------ folding

    /// The fold headed exactly by `line`, if any.
    pub fn fold_at(&self, line: usize) -> Option<(usize, usize)> {
        self.folds.iter().copied().find(|&(h, _)| h == line)
    }

    /// Whether `line` heads a foldable region.
    pub fn is_fold_header(&self, line: usize) -> bool {
        self.fold_header_set.contains(&line)
    }

    /// The fold to act on for a caret on `line`: one headed exactly there, else
    /// the innermost region enclosing it (largest header ≤ line ≤ end).
    pub fn fold_header_for(&self, line: usize) -> Option<usize> {
        if self.is_fold_header(line) {
            return Some(line);
        }
        self.folds
            .iter()
            .filter(|&&(h, e)| h <= line && line <= e)
            .map(|&(h, _)| h)
            .max()
    }

    /// Row → source-line projection while folds are collapsed, or `None` for the
    /// identity mapping (every line visible).
    pub fn visible_rows(&self) -> Option<&[usize]> {
        (!self.visible.is_empty()).then_some(self.visible.as_slice())
    }

    /// Number of displayed rows (folded lines excluded).
    pub fn content_rows(&self) -> usize {
        if self.visible.is_empty() {
            self.lines.len()
        } else {
            self.visible.len()
        }
    }

    /// Source line shown at display `row` (clamped to the last line).
    pub fn line_at_row(&self, row: usize) -> usize {
        if self.visible.is_empty() {
            return row.min(self.lines.len().saturating_sub(1));
        }
        self.visible
            .get(row)
            .copied()
            .unwrap_or_else(|| self.lines.len().saturating_sub(1))
    }

    /// Display row of a source line, accounting for collapsed folds above it.
    pub fn row_of(&self, line: usize) -> usize {
        if self.visible.is_empty() {
            return line;
        }
        match self.visible.binary_search(&line) {
            Ok(idx) | Err(idx) => idx,
        }
    }

    /// Toggle the fold at `line` if it heads one; used by gutter clicks / `za`.
    pub fn toggle_fold(&mut self, line: usize) {
        if !self.is_fold_header(line) {
            return;
        }
        if !self.collapsed.remove(&line) {
            self.collapsed.insert(line);
        }
        self.after_fold_change();
    }

    /// Collapse every foldable region.
    pub fn collapse_all(&mut self) {
        self.collapsed = self.folds.iter().map(|&(h, _)| h).collect();
        self.after_fold_change();
    }

    /// Skim: fold every function/method body down to its signature, so the file
    /// reads as a list of signatures (each still showing its inline summary).
    /// `sig_lines` are 0-based signature lines from the symbol index; each maps
    /// to the fold it heads (the signature line, or a few lines down for a
    /// multi-line signature). Toggles — if most targets are already folded this
    /// way, it expands them instead, so the same action enters and leaves skim.
    /// Unlike `collapse_all`, it leaves container folds (impl/mod) open so every
    /// signature stays visible.
    pub fn skim_bodies(&mut self, sig_lines: &[usize]) {
        let headers: Vec<usize> = sig_lines
            .iter()
            .filter_map(|&l0| (l0..=l0.saturating_add(4)).find(|&l| self.is_fold_header(l)))
            .collect();
        if headers.is_empty() {
            return;
        }
        let folded = headers
            .iter()
            .filter(|h| self.collapsed.contains(h))
            .count();
        if folded * 2 >= headers.len() {
            for h in &headers {
                self.collapsed.remove(h);
            }
        } else {
            for &h in &headers {
                self.collapsed.insert(h);
            }
        }
        self.after_fold_change();
    }

    /// Expand every fold.
    pub fn expand_all(&mut self) {
        if self.collapsed.is_empty() {
            return;
        }
        self.collapsed.clear();
        self.after_fold_change();
    }

    /// Ensure `line` (0-based) is not hidden by expanding any collapsed fold
    /// that covers it.
    pub fn reveal(&mut self, line: usize) {
        if self.visible.is_empty() {
            return;
        }
        let covering: Vec<usize> = self
            .folds
            .iter()
            .filter(|&&(h, e)| self.collapsed.contains(&h) && h < line && line <= e)
            .map(|&(h, _)| h)
            .collect();
        if covering.is_empty() {
            return;
        }
        for h in covering {
            self.collapsed.remove(&h);
        }
        self.after_fold_change();
    }

    fn after_fold_change(&mut self) {
        self.recompute_visible();
        self.refresh_annotations();
        // If the caret fell into a now-hidden region, pull it to the header.
        if let Some((line, col)) = self.caret
            && !self.visible.is_empty()
            && self.visible.binary_search(&line).is_err()
        {
            let header = self
                .folds
                .iter()
                .filter(|&&(h, e)| self.collapsed.contains(&h) && h < line && line <= e)
                .map(|&(h, _)| h)
                .min()
                .unwrap_or(line);
            self.caret = Some((header, col.min(self.line_len(header))));
        }
    }

    fn recompute_visible(&mut self) {
        if self.collapsed.is_empty() {
            self.visible.clear();
            return;
        }
        let n = self.lines.len();
        let mut hidden = vec![false; n];
        for &(h, e) in &self.folds {
            if self.collapsed.contains(&h) {
                for slot in hidden.iter_mut().take((e + 1).min(n)).skip(h + 1) {
                    *slot = true;
                }
            }
        }
        self.visible = (0..n).filter(|&i| !hidden[i]).collect();
    }

    /// Absolute scroll offset that brings `line` (1-based) near the top,
    /// keeping a few lines of context above it.
    pub fn scroll_offset_for(&self, line: Option<usize>, line_height: f32) -> f32 {
        match line {
            Some(l) => {
                // Work in display rows so collapsed folds above the target are
                // accounted for. `l` is 1-based; keep ~3 rows of context above.
                let row = self.row_of(l.saturating_sub(1));
                let max_y = (self.content_rows().saturating_sub(1)) as f32 * line_height;
                ((row.saturating_sub(3)) as f32 * line_height).clamp(0.0, max_y.max(0.0))
            }
            None => 0.0,
        }
    }

    /// Selection endpoints in document order (start ≤ end), if non-empty.
    pub fn selection_ordered(&self) -> Option<(Pos, Pos)> {
        let (a, b) = self.selection?;
        if a == b {
            return None; // a bare caret is not a selection
        }
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// Selected text as raw source, mapping display columns back to source
    /// bytes (tabs were expanded for display, so columns ≠ byte offsets).
    pub fn selected_text(&self) -> Option<String> {
        let ((sl, sc), (el, ec)) = self.selection_ordered()?;
        if sl == el {
            let line = self.source_line(sl).unwrap_or("");
            let (a, b) = (col_to_byte(line, sc), col_to_byte(line, ec));
            return Some(line.get(a..b).unwrap_or("").to_string());
        }
        let mut out = String::new();
        for i in sl..=el {
            let line = self.source_line(i).unwrap_or("");
            if i == sl {
                out.push_str(&line[col_to_byte(line, sc)..]);
            } else if i == el {
                out.push('\n');
                out.push_str(&line[..col_to_byte(line, ec)]);
            } else {
                out.push('\n');
                out.push_str(line);
            }
        }
        Some(out)
    }

    /// The line (1-based) that best represents "where the reader is":
    /// caret, else selection start, else jump target, else first visible line.
    pub fn current_line(&self, line_height: f32) -> usize {
        if let Some((line, _)) = self.caret {
            return line + 1;
        }
        if let Some(((sl, _), _)) = self.selection_ordered() {
            return sl + 1;
        }
        if let Some(t) = self.target_line {
            return t;
        }
        // The top of the viewport is a display ROW; with folds collapsed
        // above it that is not the source line (a bookmark placed from here
        // landed that many lines too high).
        self.line_at_row((self.scroll_y / line_height) as usize) + 1
    }

    /// Raw source line (0-based), if present, as `str::lines` would yield it
    /// (without its `\n` / `\r\n` terminator). O(line), not O(file).
    pub fn source_line(&self, line0: usize) -> Option<&str> {
        let start = *self.line_starts.get(line0)?;
        let rest = &self.source[start..];
        Some(match rest.find('\n') {
            Some(nl) => rest[..nl].strip_suffix('\r').unwrap_or(&rest[..nl]),
            None => rest,
        })
    }

    /// Display length (in columns) of line `i`, tabs already expanded.
    pub fn line_len(&self, i: usize) -> usize {
        self.lines.get(i).map_or(0, |l| display_cols(l, usize::MAX))
    }

    /// The LSP character offset of display column `col` on 0-based line
    /// `line0` of this document, in the server's encoding `enc` (0 when the
    /// line does not exist). Test-only: the app converts through
    /// [`Col::to_offset`] where it has the line in hand.
    #[cfg(test)]
    pub fn lsp_character(&self, line0: usize, col: Col, enc: PositionEncoding) -> usize {
        self.source_line(line0)
            .map_or(0, |raw| col.to_offset(raw, enc))
    }

    /// The display column of the LSP character offset `character` (in the
    /// server's encoding `enc`) on 0-based line `line0` of this document.
    /// Test-only, like [`Viewer::lsp_character`].
    #[cfg(test)]
    pub fn col_from_lsp(&self, line0: usize, character: usize, enc: PositionEncoding) -> Col {
        Col::from_offset(self.source_line(line0).unwrap_or(""), character, enc)
    }

    /// The display-column span `[start, end)` of an LSP range `start..end`
    /// (character offsets in `enc`) on 0-based line `line0` — never empty, so
    /// a zero-width diagnostic still marks one column.
    pub fn lsp_span(
        &self,
        line0: usize,
        start: usize,
        end: usize,
        enc: PositionEncoding,
    ) -> (Col, Col) {
        let raw = self.source_line(line0).unwrap_or("");
        let c0 = Col::from_offset(raw, start, enc);
        let c1 = Col::from_offset(raw, end, enc).max(Col(c0.0 + 1));
        (c0, c1)
    }

    fn line_chars(&self, i: usize) -> Vec<char> {
        self.lines
            .get(i)
            .map(|l| l.spans.iter().flat_map(|(t, _)| t.chars()).collect())
            .unwrap_or_default()
    }

    /// Move the block cursor by one motion (Vim-style, read-only). Movement
    /// leaves any visual selection.
    pub fn move_caret(&mut self, motion: Motion) {
        let last_line = self.lines.len().saturating_sub(1);
        let (mut line, mut col) = self.caret.unwrap_or((0, 0));
        match motion {
            Motion::Left => col = col.saturating_sub(1),
            Motion::Right => col = (col + 1).min(self.line_len(line)),
            Motion::Up => {
                line = self.prev_visible(line);
                col = col.min(self.line_len(line));
            }
            Motion::Down => {
                line = self.next_visible(line, last_line);
                col = col.min(self.line_len(line));
            }
            Motion::LineStart => col = 0,
            Motion::LineEnd => col = self.line_len(line),
            Motion::FileStart => {
                line = 0;
                col = col.min(self.line_len(line));
            }
            Motion::FileEnd => {
                line = self.visible.last().copied().unwrap_or(last_line);
                col = col.min(self.line_len(line));
            }
            Motion::WordForward => (line, col) = self.word_forward(line, col, last_line),
            Motion::WordBack => (line, col) = self.word_back(line, col),
        }
        self.caret = Some((line, col));
        self.selection = None;
    }

    /// Next visible line below `line` (skips folded-away lines).
    fn next_visible(&self, line: usize, last_line: usize) -> usize {
        if self.visible.is_empty() {
            return (line + 1).min(last_line);
        }
        match self.visible.binary_search(&line) {
            Ok(idx) => self.visible.get(idx + 1).copied().unwrap_or(line),
            Err(idx) => self.visible.get(idx).copied().unwrap_or(line),
        }
    }

    /// Previous visible line above `line` (skips folded-away lines).
    fn prev_visible(&self, line: usize) -> usize {
        if self.visible.is_empty() {
            return line.saturating_sub(1);
        }
        match self.visible.binary_search(&line) {
            Ok(idx) | Err(idx) => idx
                .checked_sub(1)
                .and_then(|i| self.visible.get(i).copied())
                .unwrap_or(line),
        }
    }

    fn word_forward(&self, line: usize, col: usize, last_line: usize) -> (usize, usize) {
        let is_word = analyze::is_ident_char;
        let chars = self.line_chars(line);
        let mut c = col;
        // Skip the rest of the current word, then any gap.
        while c < chars.len() && is_word(chars[c]) {
            c += 1;
        }
        while c < chars.len() && !is_word(chars[c]) {
            c += 1;
        }
        // Over a fold, not into it. The physically next line can be folded
        // away, and landing on it put the caret on a line that is not drawn —
        // it simply vanished from the screen. `Up`/`Down` already move this
        // way; `w`/`b` were the two motions that did not.
        if c >= chars.len() && line < last_line {
            let next = self.next_visible(line, last_line);
            return if next == line { (line, c) } else { (next, 0) };
        }
        (line, c)
    }

    fn word_back(&self, line: usize, col: usize) -> (usize, usize) {
        let is_word = analyze::is_ident_char;
        if col == 0 && line > 0 {
            // Over a fold, as in `word_forward`.
            let prev = self.prev_visible(line);
            if prev != line {
                return (prev, self.line_len(prev));
            }
            return (line, col);
        }
        let chars = self.line_chars(line);
        let mut c = col.min(chars.len());
        while c > 0 && !is_word(chars[c - 1]) {
            c -= 1;
        }
        while c > 0 && is_word(chars[c - 1]) {
            c -= 1;
        }
        (line, c)
    }

    /// Plain text of a line (cleaned spans, concatenated), for previews.
    pub fn line_text(&self, line: usize) -> String {
        self.lines
            .get(line.saturating_sub(1))
            .map(|l| l.spans.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .unwrap_or_default()
    }
}

/// Map a display column to a UTF-8 byte offset in the raw source line.
/// Display text expands tabs to four columns and strips CR, so a display
/// column does not equal a byte offset; this walks the raw line applying the
/// same expansion. Clamps to the line length when the column runs past the end.
fn col_to_byte(raw_line: &str, display_col: usize) -> usize {
    Col(display_col).to_offset(raw_line, PositionEncoding::Utf8)
}

/// Byte offset where each line of `source` starts, with `str::lines`
/// semantics: a final `\n` does not open another (empty) line.
fn line_starts_of(source: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut pos = 0usize;
    while pos < source.len() {
        starts.push(pos);
        match source[pos..].find('\n') {
            Some(nl) => pos += nl + 1,
            None => break,
        }
    }
    starts
}

/// Display columns of one display line (tabs already expanded, so one char
/// is one column), counted no further than `limit` — the one line-length
/// measure the viewer and the code view share. ASCII spans, nearly all of
/// code, are measured by their byte length.
pub fn display_cols(line: &HlLine, limit: usize) -> usize {
    let mut n = 0usize;
    for (text, _) in &line.spans {
        if n >= limit {
            break;
        }
        n += if text.is_ascii() {
            text.len().min(limit - n)
        } else {
            text.chars().take(limit - n).count()
        };
    }
    n
}

/// Widest line, measured in display characters (chars, tabs already expanded).
fn max_cols_of(lines: &[HlLine]) -> usize {
    lines
        .iter()
        .map(|l| display_cols(l, usize::MAX))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain_lines;

    const LH: f32 = 20.0;

    fn viewer_with_lines(n: usize) -> Viewer {
        let source: String = (0..n).map(|i| format!("line {i}\n")).collect();
        let lines = plain_lines(&source);
        Viewer::new(
            PathBuf::from("/tmp/x.txt"),
            "x.txt".into(),
            None,
            Arc::new(source),
            lines,
        )
    }

    /// A wide buffer whose first and last lines carry the enclosing braces, so
    /// a match from the caret has to cross the whole file.
    fn viewer_with_braces(rows: usize, width: usize) -> Viewer {
        let filler = "x".repeat(width);
        let mut source = String::from("{\n");
        for _ in 0..rows {
            source.push_str(&filler);
            source.push('\n');
        }
        source.push_str("}\n");
        let lines = plain_lines(&source);
        Viewer::new(
            PathBuf::from("/tmp/wide.txt"),
            "wide.txt".into(),
            None,
            Arc::new(source),
            lines,
        )
    }

    /// The highlight set is rebuilt on every repaint, so an un-memoized match
    /// re-walked the whole enclosing region for a caret that had not moved.
    #[test]
    fn a_repeated_bracket_lookup_is_not_a_repeated_scan() {
        let v = viewer_with_braces(1000, 280);
        let last = v.lines.len() - 1;
        assert_eq!(
            v.matching_bracket(0, 0),
            Some((last, 0)),
            "the caret is on the opening brace"
        );

        let started = std::time::Instant::now();
        for _ in 0..1000 {
            assert_eq!(v.matching_bracket(0, 0), Some((last, 0)));
        }
        let waited = started.elapsed();
        // One scan of this buffer is milliseconds, so a thousand of them is
        // seconds. Anything near that means the memo is not being consulted.
        assert!(
            waited < std::time::Duration::from_millis(500),
            "1000 repeats took {waited:?}, which is a rescan every time"
        );
    }

    /// The memo describes ONE buffer. Replacing the content must drop it, or
    /// the highlight would keep pointing at a brace that is no longer there.
    #[test]
    fn replacing_the_buffer_drops_both_memos() {
        let mut v = viewer_with_braces(3, 4);
        let last = v.lines.len() - 1;
        assert_eq!(v.matching_bracket(0, 0), Some((last, 0)));
        assert_eq!(v.occurrences("xxxx", 500).len(), 3);

        // Same caret, same word, different content: the closing brace moves up
        // and the word disappears.
        let source = String::from("{\nyyyy\n}\n");
        v.set_lines(Arc::new(plain_lines(&source)));
        assert_eq!(
            v.matching_bracket(0, 0),
            Some((2, 0)),
            "a stale memo would still name the old closing line"
        );
        assert!(
            v.occurrences("xxxx", 500).is_empty(),
            "a stale memo would still report the old word's hits"
        );
    }

    #[test]
    fn source_line_agrees_with_str_lines() {
        for src in [
            "",
            "\n",
            "one",
            "one\n",
            "one\ntwo",
            "a\r\nb\r\n",
            "lone\rcr\nnext\n",
            "\n\nthird\n",
            "日本\n😀x\r\n\ttab\n",
            "trailing cr\r",
        ] {
            let v = Viewer::new(
                PathBuf::from("/tmp/s.txt"),
                "s.txt".into(),
                None,
                Arc::new(src.to_string()),
                plain_lines(src),
            );
            let expected: Vec<&str> = src.lines().collect();
            for (i, want) in expected.iter().enumerate() {
                assert_eq!(v.source_line(i), Some(*want), "line {i} of {src:?}");
            }
            assert_eq!(
                v.source_line(expected.len()),
                None,
                "past the end of {src:?}"
            );
        }
    }

    /// Display columns ↔ LSP offsets in both encodings, on a line where they
    /// all differ: a tab (1 byte, 1 unit, 4 columns), CJK (3 bytes, 1 unit),
    /// an astral emoji (4 bytes, a 2-unit surrogate pair), a CR (0 columns).
    #[test]
    fn column_offsets_round_trip_in_both_encodings() {
        use super::{Col, PositionEncoding::*};
        let line = "\t日😀x\ry";
        // (display col, utf-8 offset, utf-16 offset) of each char start up to
        // the CR (the CR and `y` share column 7; the column maps to the CR).
        let table = [(0, 0, 0), (4, 1, 1), (5, 4, 2), (6, 8, 4), (7, 9, 5)];
        for &(col, b, u) in &table {
            assert_eq!(Col(col).to_offset(line, Utf8), b, "col {col} → utf-8");
            assert_eq!(Col(col).to_offset(line, Utf16), u, "col {col} → utf-16");
            assert_eq!(Col::from_offset(line, b, Utf8), Col(col), "utf-8 {b} → col");
            assert_eq!(
                Col::from_offset(line, u, Utf16),
                Col(col),
                "utf-16 {u} → col"
            );
        }
        // `y` sits at display column 7 (the CR is zero-width) and UTF-16 6.
        assert_eq!(Col::from_offset(line, 6, Utf16), Col(7));
        assert_eq!(Col(8).to_offset(line, Utf16), 7, "end of line");
        // An offset splitting the surrogate pair (or a UTF-8 sequence) never
        // lands mid-character: it resolves past the emoji.
        assert_eq!(Col::from_offset(line, 3, Utf16), Col(6));
        assert_eq!(Col::from_offset(line, 6, Utf8), Col(6));
        // A column inside the tab's expansion resolves past the tab.
        assert_eq!(Col(2).to_offset(line, Utf16), 1);
        // Past the end clamps to the line.
        assert_eq!(Col(99).to_offset(line, Utf8), line.len());
        assert_eq!(Col::from_offset(line, 99, Utf16), Col(8));
    }

    /// The typed crossings apply the encoding to a line of the document: the
    /// same answers as the raw conversions, with `PositionEncoding` (the
    /// language client's own type) rather than a `utf16: bool`.
    #[test]
    fn lsp_positions_convert_through_the_documents_lines() {
        use super::{Col, PositionEncoding::*};
        let src = "fn f() {}\n\t日😀x = 1;\n";
        let v = Viewer::new(
            PathBuf::from("/tmp/l.rs"),
            "l.rs".into(),
            None,
            Arc::new(src.to_string()),
            plain_lines(src),
        );
        // `x` on line 1: display column 6; UTF-8 byte 8; UTF-16 unit 4.
        assert_eq!(v.lsp_character(1, Col(6), Utf8), 8);
        assert_eq!(v.lsp_character(1, Col(6), Utf16), 4);
        assert_eq!(v.col_from_lsp(1, 4, Utf16), Col(6));
        assert_eq!(v.col_from_lsp(1, 8, Utf8), Col(6));
        // A diagnostic range is never empty, and a missing line is column 0.
        assert_eq!(v.lsp_span(1, 4, 4, Utf16), (Col(6), Col(7)));
        assert_eq!(v.lsp_span(1, 1, 4, Utf16), (Col(4), Col(6)));
        assert_eq!(v.lsp_character(9, Col(3), Utf8), 0);
        assert_eq!(v.col_from_lsp(9, 3, Utf8), Col(0));
        // The encoding's own measure agrees with the column walk.
        let prefix = "\t日😀";
        assert_eq!(Utf16.units_in(prefix), 4);
        assert_eq!(Utf8.units_in(prefix), 8);
        assert_eq!(usize::from(Col(6)), 6);
    }

    /// The code view's annotation summary is kept on the viewer and refreshed
    /// where its inputs change — hints, inactive lines, folds, content — so
    /// a view build never recomputes it; each refresh equals a derivation
    /// from scratch.
    #[test]
    fn the_annotation_summary_follows_hints_inactive_lines_and_folds() {
        use crate::codeview::annotations_of;
        let src = "fn a() {\n    x();\n}\nfn b() {\n    y();\n}\n";
        let mut v = Viewer::new(
            PathBuf::from("/tmp/a.rs"),
            "a.rs".into(),
            None,
            Arc::new(src.to_string()),
            plain_lines(src),
        );
        let derived = |v: &Viewer| {
            annotations_of(
                &v.lines,
                Some(&v.inlay_hints),
                Some(&v.inactive_lines),
                Some(&v.collapsed),
            )
        };
        assert_eq!(v.annotations(), derived(&v));
        let blank = v.annotations();

        v.set_inlay_hints(HashMap::from([(1, vec![(5, ": Unit".to_string())])]));
        let hinted = v.annotations();
        assert_eq!(hinted, derived(&v));
        assert_ne!(hinted.signature, blank.signature);
        assert_eq!(hinted.hinted_cols, "    x();".len() + ": Unit".len());

        // Same width, different label: still a different signature.
        v.set_inlay_hints(HashMap::from([(1, vec![(5, ": Uint".to_string())])]));
        assert_ne!(v.annotations().signature, hinted.signature);

        v.set_inactive_lines(HashSet::from([4]));
        assert_eq!(v.annotations(), derived(&v));

        v.toggle_fold(3); // collapse `fn b() {`
        let folded = v.annotations();
        assert_eq!(folded, derived(&v));
        assert_eq!(folded.collapsed_cols, "fn b() {".len() + 3);
        v.expand_all();
        assert_eq!(v.annotations().collapsed_cols, 0);

        // A reload drops hints and dimming, and the summary with them.
        v.reload(Arc::new(src.to_string()), plain_lines(src));
        assert_eq!(v.annotations(), blank);
    }

    #[test]
    fn display_cols_counts_to_a_limit() {
        let lines = plain_lines("ab日本cd\n");
        assert_eq!(display_cols(&lines[0], usize::MAX), 6);
        assert_eq!(display_cols(&lines[0], 3), 3);
        assert_eq!(display_cols(&lines[0], 0), 0);
        let long = plain_lines(&"x".repeat(10_000));
        assert_eq!(display_cols(&long[0], 4098), 4098);
    }

    /// With folds collapsed above the viewport, the line "where the reader
    /// is" is the source line shown at the top row, not the row number.
    #[test]
    fn current_line_maps_the_top_row_through_folds() {
        let src = "fn a() {\n    x();\n    y();\n}\nfn b() {\n    z();\n}\n";
        let mut v = Viewer::new(
            PathBuf::from("/tmp/f.rs"),
            "f.rs".into(),
            None,
            Arc::new(src.to_string()),
            plain_lines(src),
        );
        v.toggle_fold(0); // hides lines 1..=2
        assert_eq!(v.visible_rows(), Some(&[0, 3, 4, 5, 6][..]));
        v.scroll_y = 2.0 * LH; // row 2 at the top: `fn b() {` (line 4)
        assert_eq!(v.current_line(LH), 5, "1-based line 5, not row 3");
    }

    #[test]
    fn scroll_offset_keeps_context_above_target() {
        let v = viewer_with_lines(1000);
        assert_eq!(v.scroll_offset_for(Some(100), LH), 96.0 * LH);
        assert_eq!(v.scroll_offset_for(Some(1), LH), 0.0);
        assert_eq!(v.scroll_offset_for(None, LH), 0.0);
        // Clamped to content height.
        let small = viewer_with_lines(5);
        assert!(small.scroll_offset_for(Some(1_000_000), LH) <= 5.0 * LH);
    }

    #[test]
    fn char_selection_orders_endpoints_and_extracts_text() {
        let mut v = viewer_with_lines(10);
        assert_eq!(v.selected_text(), None);

        // Dragged upwards/backwards: head before anchor.
        v.selection = Some(((3, 2), (1, 4)));
        assert_eq!(v.selection_ordered(), Some(((1, 4), (3, 2))));
        // "line 1"[4..] = " 1", full "line 2", "line 3"[..2] = "li".
        assert_eq!(v.selected_text().unwrap(), " 1\nline 2\nli");
    }

    #[test]
    fn single_line_selection_and_bare_caret() {
        let mut v = viewer_with_lines(10);
        // Same anchor and head is a caret, not a selection.
        v.selection = Some(((2, 3), (2, 3)));
        assert_eq!(v.selection_ordered(), None);
        assert_eq!(v.selected_text(), None);
        // A real single-line span.
        v.selection = Some(((2, 1), (2, 4)));
        assert_eq!(v.selected_text().unwrap(), "ine"); // "line 2"[1..4]
    }

    #[test]
    fn char_offset_inverse_roundtrips() {
        use super::PositionEncoding::Utf8;
        // "\tlet" displays as "    let"; byte 1 ('l') is display col 4.
        assert_eq!(Col::from_offset("\tlet", 1, Utf8), Col(4));
        assert_eq!(Col(4).to_offset("\tlet", Utf8), 1);
        // Plain ascii is identity.
        assert_eq!(Col::from_offset("hello", 3, Utf8), Col(3));
    }

    #[test]
    fn selected_text_maps_tabs_to_source_bytes() {
        let source = "\tlet x = 1;\n".to_string();
        let lines = plain_lines(&source);
        let mut v = Viewer::new(
            PathBuf::from("/tmp/t.rs"),
            "t.rs".into(),
            None,
            Arc::new(source),
            lines,
        );
        // Display "    let ...": tab shows as 4 columns. Columns 4..7 = "let".
        v.selection = Some(((0, 4), (0, 7)));
        assert_eq!(v.selected_text().unwrap(), "let");
    }

    #[test]
    fn cursor_motions() {
        // Lines: "line 0".."line 9", each 6 display columns.
        let mut v = viewer_with_lines(10);
        v.caret = Some((0, 0));

        v.move_caret(Motion::Right);
        assert_eq!(v.caret, Some((0, 1)));
        v.move_caret(Motion::Down);
        assert_eq!(v.caret, Some((1, 1)));
        v.move_caret(Motion::LineEnd);
        assert_eq!(v.caret, Some((1, 6))); // "line 1" is 6 cols
        v.move_caret(Motion::Right); // clamped at line end
        assert_eq!(v.caret, Some((1, 6)));
        v.move_caret(Motion::LineStart);
        assert_eq!(v.caret, Some((1, 0)));
        v.move_caret(Motion::Left); // clamped at col 0
        assert_eq!(v.caret, Some((1, 0)));
        v.move_caret(Motion::FileEnd);
        assert_eq!(v.caret, Some((9, 0)));
        v.move_caret(Motion::Down); // clamped at last line
        assert_eq!(v.caret, Some((9, 0)));
        v.move_caret(Motion::FileStart);
        assert_eq!(v.caret, Some((0, 0)));
        // Word motion within "line 0": "line" then "0".
        v.move_caret(Motion::WordForward);
        assert_eq!(v.caret, Some((0, 5))); // start of "0"
    }

    #[test]
    fn folding_projects_rows_and_moves_caret() {
        let src = "impl Foo {\n    fn bar() {\n        a();\n        b();\n    }\n}\n";
        let lines = plain_lines(src);
        let mut v = Viewer::new(
            PathBuf::from("/tmp/f.rs"),
            "f.rs".into(),
            None,
            Arc::new(src.to_string()),
            lines,
        );
        // No folds collapsed: identity projection.
        assert_eq!(v.visible_rows(), None);
        assert_eq!(v.content_rows(), 6);
        assert_eq!(v.row_of(4), 4);

        // Collapse the inner fn (header line 1 hides lines 2..=3).
        assert!(v.is_fold_header(1));
        v.toggle_fold(1);
        assert_eq!(v.visible_rows(), Some(&[0, 1, 4, 5][..]));
        assert_eq!(v.content_rows(), 4);
        // Line 4 now sits on display row 2.
        assert_eq!(v.row_of(4), 2);
        assert_eq!(v.line_at_row(2), 4);

        // A caret inside the collapsed body is pulled up to the header.
        v.caret = Some((3, 2));
        v.toggle_fold(1); // expand
        v.toggle_fold(1); // collapse again with caret inside
        assert_eq!(v.caret, Some((1, 2.min(v.line_len(1)))));

        // reveal() expands the fold hiding a target line.
        v.reveal(3);
        assert_eq!(v.visible_rows(), None);
    }

    #[test]
    fn reload_keeps_caret_and_clamps_on_shrink() {
        let src = "aaa\nbbbbbbbbbb\nccc\n";
        let mut v = Viewer::new(
            PathBuf::from("/tmp/r.rs"),
            "r.rs".into(),
            None,
            Arc::new(src.to_string()),
            plain_lines(src),
        );
        v.caret = Some((1, 6));
        // Same line count, middle line still long enough → caret unchanged.
        let src2 = "aaa\nBBBBBBBBBBBBBB\nccc\n";
        v.reload(Arc::new(src2.to_string()), plain_lines(src2));
        assert_eq!(v.caret, Some((1, 6)));
        // Middle line shrinks below the caret column → column clamps to its end.
        let src3 = "aaa\nbb\nccc\n";
        v.reload(Arc::new(src3.to_string()), plain_lines(src3));
        assert_eq!(v.caret, Some((1, 2)));
    }

    #[test]
    fn current_line_prefers_caret_then_target() {
        let mut v = viewer_with_lines(100);
        v.scroll_y = 50.0 * LH;
        assert_eq!(v.current_line(LH), 51);
        v.target_line = Some(7);
        assert_eq!(v.current_line(LH), 7);
        v.caret = Some((11, 0));
        assert_eq!(v.current_line(LH), 12);
    }
}

#[cfg(test)]
mod fold_motion_tests {
    use super::*;
    use crate::highlight::plain_lines;

    /// A viewer over `n` lines with `hidden` folded away.
    fn viewer_with_fold(n: usize, hidden: &[usize]) -> Viewer {
        let source: String = (0..n).map(|i| format!("word{i}\n")).collect();
        let lines = plain_lines(&source);
        let mut v = Viewer::new(
            PathBuf::from("/tmp/x.txt"),
            "x.txt".into(),
            None,
            Arc::new(source),
            lines,
        );
        v.visible = (0..n).filter(|i| !hidden.contains(i)).collect();
        v
    }

    /// `w` and `b` cross folds instead of landing inside them. Stepping onto a
    /// folded-away line put the caret on a line that is not drawn, so it
    /// disappeared from the screen.
    #[test]
    fn word_motions_skip_folded_lines() {
        // Lines 1..=3 are folded away; 0 and 4 are visible.
        let mut v = viewer_with_fold(6, &[1, 2, 3]);
        let last = 5;

        // Forward from the end of line 0 lands on 4, not 1.
        assert_eq!(v.word_forward(0, v.line_len(0), last), (4, 0));
        // Back from the start of line 4 lands on 0, at its end.
        assert_eq!(v.word_back(4, 0), (0, v.line_len(0)));

        // With nothing folded the behaviour is unchanged.
        v.visible = Vec::new();
        assert_eq!(v.word_forward(0, v.line_len(0), last), (1, 0));
        assert_eq!(v.word_back(4, 0), (3, v.line_len(3)));
    }

    /// At the first/last visible line there is nowhere to go, and the caret
    /// must stay put rather than step into a hidden line.
    #[test]
    fn word_motions_stop_at_the_visible_edges() {
        let v = viewer_with_fold(4, &[1, 2, 3]);
        // Only line 0 is visible: forward has nowhere to land.
        assert_eq!(v.word_forward(0, v.line_len(0), 3), (0, v.line_len(0)));
        // And back from column 0 of the first line stays.
        assert_eq!(v.word_back(0, 0), (0, 0));
    }
}
