//! State for the read-only, virtualized code viewer.
//!
//! Virtualization: the scrollable content keeps a constant total height of
//! `lines.len() * line_height` using two spacers, and only the visible window
//! of lines (plus overscan) is materialized as widgets.

use std::path::PathBuf;
use std::sync::Arc;

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
    pub highlighted: bool,
    pub scroll_y: f32,
    pub viewport_h: f32,
    pub target_line: Option<usize>, // 1-based jump target, drawn highlighted
    pub selection: Option<Selection>,
    /// Last clicked position as (0-based line, 0-based display column).
    pub caret: Option<(usize, usize)>,
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
        Self {
            abs,
            rel,
            lang_key,
            source,
            lines: Arc::new(lines),
            max_cols,
            symbols: Vec::new(),
            highlighted: false,
            scroll_y: 0.0,
            // Generous default until the first scroll event reports the real
            // viewport; only affects how many rows are materialized.
            viewport_h: 2400.0,
            target_line: None,
            selection: None,
            caret: None,
        }
    }

    /// Replace the highlighted lines (same line count) and refresh `max_cols`.
    pub fn set_lines(&mut self, lines: Arc<Vec<HlLine>>) {
        self.max_cols = max_cols_of(&lines);
        self.lines = lines;
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
                let max_y = (self.lines.len().saturating_sub(1)) as f32 * line_height;
                ((l.saturating_sub(4)) as f32 * line_height).clamp(0.0, max_y.max(0.0))
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
