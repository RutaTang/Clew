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

/// An inclusive, unordered line-index selection: (anchor, head), 0-based.
pub type Selection = (usize, usize);

#[derive(Debug, Clone)]
pub struct Viewer {
    pub abs: PathBuf,
    pub rel: String,
    pub lang_key: Option<&'static str>,
    /// Raw file content, kept for copy-to-clipboard fidelity.
    pub source: Arc<String>,
    /// Shared so a split showing the same file clones cheaply.
    pub lines: Arc<Vec<HlLine>>,
    pub symbols: Vec<Symbol>,
    pub highlighted: bool,
    pub scroll_y: f32,
    pub viewport_h: f32,
    pub target_line: Option<usize>, // 1-based jump target, drawn highlighted
    pub selection: Option<Selection>,
}

impl Viewer {
    pub fn new(
        abs: PathBuf,
        rel: String,
        lang_key: Option<&'static str>,
        source: Arc<String>,
        lines: Vec<HlLine>,
    ) -> Self {
        Self {
            abs,
            rel,
            lang_key,
            source,
            lines: Arc::new(lines),
            symbols: Vec::new(),
            highlighted: false,
            scroll_y: 0.0,
            // Generous default until the first scroll event reports the real
            // viewport; only affects how many rows are materialized.
            viewport_h: 2400.0,
            target_line: None,
            selection: None,
        }
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

    /// Ordered inclusive selection bounds, if any.
    pub fn selection_bounds(&self) -> Option<(usize, usize)> {
        self.selection
            .map(|(a, b)| (a.min(b), a.max(b).min(self.lines.len().saturating_sub(1))))
    }

    /// Text of the selected lines (raw source, newline-joined).
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_bounds()?;
        let lines: Vec<&str> = self
            .source
            .lines()
            .skip(start)
            .take(end - start + 1)
            .collect();
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }

    /// The line (1-based) that best represents "where the reader is":
    /// selection start, else jump target, else first visible line.
    pub fn current_line(&self, line_height: f32) -> usize {
        if let Some((start, _)) = self.selection_bounds() {
            return start + 1;
        }
        if let Some(t) = self.target_line {
            return t;
        }
        (self.scroll_y / line_height) as usize + 1
    }

    /// Plain text of a line (cleaned spans, concatenated), for previews.
    pub fn line_text(&self, line: usize) -> String {
        self.lines
            .get(line.saturating_sub(1))
            .map(|l| l.spans.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .unwrap_or_default()
    }
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
    fn selection_bounds_and_text() {
        let mut v = viewer_with_lines(10);
        assert_eq!(v.selected_text(), None);
        v.selection = Some((4, 2)); // dragged upwards
        assert_eq!(v.selection_bounds(), Some((2, 4)));
        assert_eq!(v.selected_text().unwrap(), "line 2\nline 3\nline 4");
        assert_eq!(v.current_line(LH), 3);
    }

    #[test]
    fn current_line_prefers_target_then_scroll() {
        let mut v = viewer_with_lines(100);
        v.scroll_y = 50.0 * LH;
        assert_eq!(v.current_line(LH), 51);
        v.target_line = Some(7);
        assert_eq!(v.current_line(LH), 7);
    }
}
