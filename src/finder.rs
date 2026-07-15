//! Fuzzy finder state and matching: files (Cmd+P), project symbols (Cmd+T)
//! and `:N` go-to-line.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::fs_scan::FileEntry;
use crate::index::SymbolEntry;

pub const MAX_RESULTS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FinderMode {
    #[default]
    Files,
    Symbols,
}

#[derive(Debug, Default)]
pub struct Finder {
    pub open: bool,
    pub mode: FinderMode,
    pub query: String,
    pub results: Vec<usize>, // indices into the mode's backing list
    pub selected: usize,
}

impl Finder {
    /// Parse a `:N` query as a 1-based go-to-line request.
    pub fn goto_line(&self) -> Option<usize> {
        let n: usize = self.query.trim().strip_prefix(':')?.trim().parse().ok()?;
        (n > 0).then_some(n)
    }

    pub fn refresh_files(&mut self, files: &[FileEntry]) {
        self.selected = 0;
        self.results = rank(&self.query, files.iter().map(|f| f.rel.as_str()));
    }

    pub fn refresh_symbols(&mut self, symbols: &[SymbolEntry]) {
        self.selected = 0;
        self.results = rank(&self.query, symbols.iter().map(|s| s.name.as_str()));
    }

    pub fn move_selection(&mut self, delta: i32) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.results.len() as i32;
        self.selected = (self.selected as i32 + delta).rem_euclid(len) as usize;
    }
}

/// Rank `items` against `query`; empty query lists the first entries as-is.
/// Higher score first, shorter item wins ties.
fn rank<'a>(query: &str, items: impl Iterator<Item = &'a str>) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() || query.starts_with(':') {
        return items.take(MAX_RESULTS).enumerate().map(|(i, _)| i).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize, usize)> = items
        .enumerate()
        .filter_map(|(i, item)| {
            let haystack = Utf32Str::new(item, &mut buf);
            pattern
                .score(haystack, &mut matcher)
                .map(|s| (s, item.len(), i))
        })
        .collect();
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored.truncate(MAX_RESULTS);
    scored.into_iter().map(|(_, _, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goto_line_parses_colon_queries() {
        let mut f = Finder::default();
        f.query = ":128".into();
        assert_eq!(f.goto_line(), Some(128));
        f.query = ": 7".into();
        assert_eq!(f.goto_line(), Some(7));
        f.query = ":0".into();
        assert_eq!(f.goto_line(), None);
        f.query = "main".into();
        assert_eq!(f.goto_line(), None);
    }

    #[test]
    fn ranking_prefers_better_and_shorter_matches() {
        let items = ["src/main.rs", "src/domain/remains.rs", "main.rs"];
        let ranked = rank("mainrs", items.iter().copied());
        assert_eq!(ranked[0], 2, "shortest strong match first: {ranked:?}");
        assert!(ranked.contains(&0));
    }

    #[test]
    fn empty_query_lists_head() {
        let items = ["a", "b", "c"];
        assert_eq!(rank("", items.iter().copied()), vec![0, 1, 2]);
    }
}
