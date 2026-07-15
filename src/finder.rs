//! Fuzzy file finder (Ctrl/Cmd+P) state and matching.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::fs_scan::FileEntry;

pub const MAX_RESULTS: usize = 60;

#[derive(Debug, Default)]
pub struct Finder {
    pub open: bool,
    pub query: String,
    pub results: Vec<usize>, // indices into the project file list
    pub selected: usize,
}

impl Finder {
    /// Re-rank results for the current query.
    pub fn refresh(&mut self, files: &[FileEntry]) {
        self.selected = 0;
        if self.query.trim().is_empty() {
            self.results = (0..files.len().min(MAX_RESULTS)).collect();
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let mut buf = Vec::new();
        let mut scored: Vec<(u32, usize)> = files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let haystack = Utf32Str::new(&f.rel, &mut buf);
                pattern.score(haystack, &mut matcher).map(|s| (s, i))
            })
            .collect();
        // Higher score first; shorter path wins ties.
        scored.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| files[a.1].rel.len().cmp(&files[b.1].rel.len()))
        });
        scored.truncate(MAX_RESULTS);
        self.results = scored.into_iter().map(|(_, i)| i).collect();
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.results.len() as i32;
        let next = (self.selected as i32 + delta).rem_euclid(len);
        self.selected = next as usize;
    }
}
