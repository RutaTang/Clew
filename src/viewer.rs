//! State for the read-only, virtualized code viewer.
//!
//! Virtualization: the scrollable content keeps a constant total height of
//! `lines.len() * LINE_HEIGHT` using two spacers, and only the visible window
//! of lines (plus overscan) is materialized as widgets.

use std::path::PathBuf;

use crate::highlight::HlLine;

pub const FONT_SIZE: f32 = 13.0;
pub const LINE_HEIGHT: f32 = 20.0;
pub const OVERSCAN: usize = 12;

/// Maximum file size we attempt to display.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct Viewer {
    pub abs: PathBuf,
    pub rel: String,
    pub lang_key: Option<&'static str>,
    pub lines: Vec<HlLine>,
    pub highlighted: bool,
    pub scroll_y: f32,
    pub viewport_h: f32,
    pub target_line: Option<usize>, // 1-based jump target, drawn highlighted
}

impl Viewer {
    pub fn new(abs: PathBuf, rel: String, lang_key: Option<&'static str>, lines: Vec<HlLine>) -> Self {
        Self {
            abs,
            rel,
            lang_key,
            lines,
            highlighted: false,
            scroll_y: 0.0,
            // Generous default until the first scroll event reports the real
            // viewport; only affects how many rows are materialized.
            viewport_h: 2400.0,
            target_line: None,
        }
    }

    /// Half-open range of line indices to materialize.
    pub fn visible_range(&self) -> (usize, usize) {
        let total = self.lines.len();
        let first = ((self.scroll_y / LINE_HEIGHT) as usize).saturating_sub(OVERSCAN);
        let count = (self.viewport_h / LINE_HEIGHT).ceil() as usize + OVERSCAN * 2;
        let last = (first + count).min(total);
        (first.min(total), last)
    }

    /// Absolute scroll offset that brings `line` (1-based) near the top,
    /// keeping a few lines of context above it.
    pub fn scroll_offset_for(&self, line: Option<usize>) -> f32 {
        match line {
            Some(l) => {
                let max_y = (self.lines.len().saturating_sub(1)) as f32 * LINE_HEIGHT;
                ((l.saturating_sub(4)) as f32 * LINE_HEIGHT).clamp(0.0, max_y.max(0.0))
            }
            None => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain_lines;

    fn viewer_with_lines(n: usize) -> Viewer {
        let source: String = (0..n).map(|i| format!("line {i}\n")).collect();
        Viewer::new(
            PathBuf::from("/tmp/x.txt"),
            "x.txt".into(),
            None,
            plain_lines(&source),
        )
    }

    #[test]
    fn visible_range_covers_viewport_plus_overscan() {
        let mut v = viewer_with_lines(10_000);
        v.viewport_h = 800.0;
        v.scroll_y = 100.0 * LINE_HEIGHT; // scrolled to line 100
        let (first, last) = v.visible_range();
        assert!(first <= 100 - OVERSCAN);
        assert!(last >= 100 + (800.0 / LINE_HEIGHT) as usize);
        assert!(last <= 10_000);
        // Materialized window stays small regardless of file size.
        assert!(last - first < 100);
    }

    #[test]
    fn visible_range_clamps_at_edges() {
        let mut v = viewer_with_lines(10);
        v.viewport_h = 800.0;
        v.scroll_y = 0.0;
        assert_eq!(v.visible_range(), (0, 10));
        v.scroll_y = 1e9;
        let (first, last) = v.visible_range();
        assert!(first <= last && last == 10);
    }

    #[test]
    fn scroll_offset_keeps_context_above_target() {
        let v = viewer_with_lines(1000);
        assert_eq!(v.scroll_offset_for(Some(100)), 96.0 * LINE_HEIGHT);
        assert_eq!(v.scroll_offset_for(Some(1)), 0.0);
        assert_eq!(v.scroll_offset_for(None), 0.0);
        // Clamped to content height.
        let small = viewer_with_lines(5);
        assert!(small.scroll_offset_for(Some(1_000_000)) <= 5.0 * LINE_HEIGHT);
    }
}
