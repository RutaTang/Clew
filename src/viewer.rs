//! State for the read-only, virtualized code viewer.
//!
//! Virtualization: the scrollable content keeps a constant total height of
//! `lines.len() * line_height` using two spacers, and only the visible window
//! of lines (plus overscan) is materialized as widgets.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::analyze;
use crate::highlight::HlLine;
use crate::outline::Symbol;

pub const OVERSCAN: usize = 12;

/// Maximum file size we attempt to display.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// A caret position: (0-based line, 0-based display column).
pub type Pos = (usize, usize);

/// A character-level selection: (anchor, head), each a caret position.
/// Positions are caret-between-characters; the selection covers everything
/// between them in document order. Columns are display columns (tabs expanded).
pub type Selection = (Pos, Pos);

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
    /// Shared so a split showing the same file clones cheaply.
    pub lines: Arc<Vec<HlLine>>,
    /// Widest line in display columns; drives horizontal scroll extent.
    pub max_cols: usize,
    pub symbols: Vec<Symbol>,
    /// Signature line (1-based) -> the author's doc comment, for the hover peek.
    /// Populated off-thread alongside `symbols`; empty until highlighting lands.
    pub docs: HashMap<usize, String>,
    /// LSP inlay hints: 0-based display line -> [(display column, label)],
    /// sorted by column. Empty until the language server answers.
    pub inlay_hints: HashMap<usize, Vec<(usize, String)>>,
    pub highlighted: bool,
    pub scroll_y: f32,
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
    /// Row → source-line projection when any fold is collapsed; empty means the
    /// identity mapping (every line visible), so the common path allocates none.
    visible: Vec<usize>,
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
        Self {
            abs,
            rel,
            lang_key,
            source,
            lines: Arc::new(lines),
            max_cols,
            symbols: Vec::new(),
            docs: HashMap::new(),
            inlay_hints: HashMap::new(),
            highlighted: false,
            scroll_y: 0.0,
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
            git: None,
        }
    }

    /// Replace the file's content in place after an on-disk change, keeping the
    /// reader's position (scroll, caret, folds) so the view doesn't jump. The
    /// caret is clamped to the new bounds and stale collapsed headers dropped;
    /// symbols/highlighting are refreshed asynchronously afterwards.
    pub fn reload(&mut self, source: Arc<String>, lines: Vec<HlLine>) {
        self.source = source;
        self.set_lines(Arc::new(lines)); // recomputes folds / header set / visible / max_cols
        self.highlighted = false;
        self.symbols.clear();
        self.docs.clear();
        self.inlay_hints.clear();
        if let Some((line, col)) = self.caret {
            let line = line.min(self.lines.len().saturating_sub(1));
            self.caret = Some((line, col.min(self.line_len(line))));
        }
        // Drop collapsed headers that no longer head a fold, then reproject.
        self.collapsed.retain(|h| self.fold_header_set.contains(h));
        self.recompute_visible();
    }

    /// Replace the highlighted lines (same line count) and refresh `max_cols`.
    /// Folds are indentation-derived, so the collapsed set stays valid.
    pub fn set_lines(&mut self, lines: Arc<Vec<HlLine>>) {
        self.max_cols = max_cols_of(&lines);
        self.folds = analyze::fold_ranges(&lines);
        self.fold_header_set = self.folds.iter().map(|&(h, _)| h).collect();
        self.lines = lines;
        self.recompute_visible();
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
        let folded = headers.iter().filter(|h| self.collapsed.contains(h)).count();
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

    /// Half-open range of line indices to materialize.
    pub fn visible_range(&self, line_height: f32) -> (usize, usize) {
        let total = self.lines.len();
        let first = ((self.scroll_y / line_height) as usize).saturating_sub(OVERSCAN);
        let count = (self.viewport_h / line_height).ceil() as usize + OVERSCAN * 2;
        let last = (first + count).min(total);
        (first.min(total), last)
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
        let lines: Vec<&str> = self.source.lines().collect();
        if sl == el {
            let line = lines.get(sl).copied().unwrap_or("");
            let (a, b) = (col_to_byte(line, sc), col_to_byte(line, ec));
            return Some(line.get(a..b).unwrap_or("").to_string());
        }
        let mut out = String::new();
        for i in sl..=el {
            let line = lines.get(i).copied().unwrap_or("");
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
        (self.scroll_y / line_height) as usize + 1
    }

    /// Raw source line (0-based), if present.
    pub fn source_line(&self, line0: usize) -> Option<&str> {
        self.source.lines().nth(line0)
    }

    /// Display length (in columns) of line `i`, tabs already expanded.
    pub fn line_len(&self, i: usize) -> usize {
        self.lines
            .get(i)
            .map(|l| l.spans.iter().map(|(t, _)| t.chars().count()).sum())
            .unwrap_or(0)
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
            Ok(idx) | Err(idx) => idx.checked_sub(1).and_then(|i| self.visible.get(i).copied()).unwrap_or(line),
        }
    }

    fn word_forward(&self, line: usize, col: usize, last_line: usize) -> (usize, usize) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let chars = self.line_chars(line);
        let mut c = col;
        // Skip the rest of the current word, then any gap.
        while c < chars.len() && is_word(chars[c]) {
            c += 1;
        }
        while c < chars.len() && !is_word(chars[c]) {
            c += 1;
        }
        if c >= chars.len() && line < last_line {
            return (line + 1, 0);
        }
        (line, c)
    }

    fn word_back(&self, line: usize, col: usize) -> (usize, usize) {
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        if col == 0 && line > 0 {
            let prev = self.line_len(line - 1);
            return (line - 1, prev);
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
    character_offset(raw_line, display_col, false)
}

/// LSP character offset for a display column on a raw source line, in the
/// server's negotiated encoding: utf-16 code units when `utf16`, else utf-8
/// bytes. Walks the line applying the same tab expansion used for display.
pub fn character_offset(raw_line: &str, display_col: usize, utf16: bool) -> usize {
    let mut col = 0usize; // display column
    let mut off = 0usize; // byte or utf-16 offset
    for ch in raw_line.chars() {
        if col >= display_col {
            break;
        }
        match ch {
            '\r' => {}
            '\t' => col += 4,
            _ => col += 1,
        }
        off += if utf16 { ch.len_utf16() } else { ch.len_utf8() };
    }
    off
}

/// Inverse of [`character_offset`]: the display column at a server-encoding
/// offset (utf-16 code units when `utf16`, else utf-8 bytes) on a raw line.
pub fn display_col_from_char(raw_line: &str, char_off: usize, utf16: bool) -> usize {
    let mut col = 0usize;
    let mut off = 0usize;
    for ch in raw_line.chars() {
        if off >= char_off {
            break;
        }
        match ch {
            '\r' => {}
            '\t' => col += 4,
            _ => col += 1,
        }
        off += if utf16 { ch.len_utf16() } else { ch.len_utf8() };
    }
    col
}

/// Widest line, measured in display characters (chars, tabs already expanded).
fn max_cols_of(lines: &[HlLine]) -> usize {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
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

    #[test]
    fn visible_range_covers_viewport_plus_overscan() {
        let mut v = viewer_with_lines(10_000);
        v.viewport_h = 800.0;
        v.scroll_y = 100.0 * LH; // scrolled to line 100
        let (first, last) = v.visible_range(LH);
        assert!(first <= 100 - OVERSCAN);
        assert!(last >= 100 + (800.0 / LH) as usize);
        assert!(last <= 10_000);
        // Materialized window stays small regardless of file size.
        assert!(last - first < 100);
    }

    #[test]
    fn visible_range_clamps_at_edges() {
        let mut v = viewer_with_lines(10);
        v.viewport_h = 800.0;
        v.scroll_y = 0.0;
        assert_eq!(v.visible_range(LH), (0, 10));
        v.scroll_y = 1e9;
        let (first, last) = v.visible_range(LH);
        assert!(first <= last && last == 10);
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
        // "\tlet" displays as "    let"; byte 1 ('l') is display col 4.
        assert_eq!(display_col_from_char("\tlet", 1, false), 4);
        assert_eq!(character_offset("\tlet", 4, false), 1);
        // Plain ascii is identity.
        assert_eq!(display_col_from_char("hello", 3, false), 3);
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
