//! Fuzzy finder state and matching: files (Cmd+P), project symbols (Cmd+T),
//! `:N` go-to-line, and commands (Cmd+Shift+P, or a query starting with
//! `>`: the command palette).

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

    /// Whether the query asks for commands: it starts with `>`, as in the
    /// palette Cmd+Shift+P opens (deleting the `>` gives the files back).
    pub fn is_commands(&self) -> bool {
        self.query.trim_start().starts_with('>')
    }

    /// The query less its `>`.
    pub fn command_query(&self) -> &str {
        self.query.trim_start().trim_start_matches('>').trim()
    }

    pub fn refresh_files(&mut self, files: &[FileEntry]) {
        self.selected = 0;
        self.results = rank(
            &self.query,
            files.iter().map(|f| f.rel.as_str()),
            MAX_RESULTS,
            true,
        );
    }

    pub fn refresh_symbols(&mut self, symbols: &[SymbolEntry]) {
        self.selected = 0;
        self.results = rank(
            &self.query,
            symbols.iter().map(|s| s.name.as_str()),
            MAX_RESULTS,
            true,
        );
    }

    /// Rank the commands titled `titles` against the query: every match,
    /// best first. With nothing typed, the ones in `recent` (titles, most
    /// recent first) lead, then the rest in their own order.
    pub fn refresh_commands(&mut self, titles: &[&str], recent: &[String]) {
        self.selected = 0;
        let query = self.command_query();
        if !query.is_empty() {
            self.results = rank(query, titles.iter().copied(), usize::MAX, false);
            return;
        }
        let mut order: Vec<usize> = recent
            .iter()
            .filter_map(|r| titles.iter().position(|t| t == r))
            .collect();
        let rest: Vec<usize> = (0..titles.len()).filter(|i| !order.contains(i)).collect();
        order.extend(rest);
        self.results = order;
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

/// Rank `items` against `query`, at most `limit` of them; empty query lists
/// the first entries as-is. Higher score first, shorter item wins ties.
/// `paths` scores the way a path is matched (separators weigh more).
fn rank<'a>(
    query: &str,
    items: impl Iterator<Item = &'a str>,
    limit: usize,
    paths: bool,
) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() || query.starts_with(':') {
        return items.take(limit).enumerate().map(|(i, _)| i).collect();
    }

    let config = if paths {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
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
    scored.truncate(limit);
    scored.into_iter().map(|(_, _, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goto_line_parses_colon_queries() {
        let mut f = Finder {
            query: ":128".into(),
            ..Default::default()
        };
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
        let ranked = rank("mainrs", items.iter().copied(), MAX_RESULTS, true);
        assert_eq!(ranked[0], 2, "shortest strong match first: {ranked:?}");
        assert!(ranked.contains(&0));
    }

    #[test]
    fn empty_query_lists_head() {
        let items = ["a", "b", "c"];
        assert_eq!(
            rank("", items.iter().copied(), MAX_RESULTS, true),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn a_query_starting_with_a_chevron_asks_for_commands() {
        let mut f = Finder {
            query: ">".into(),
            ..Default::default()
        };
        assert!(f.is_commands());
        assert_eq!(f.command_query(), "");
        f.query = "  > call gr".into();
        assert!(f.is_commands());
        assert_eq!(f.command_query(), "call gr");
        f.query = "src/a>b".into();
        assert!(!f.is_commands());
    }

    #[test]
    fn commands_rank_every_match_and_lead_with_the_recent_ones() {
        let titles = [
            "Call Graph",
            "Import Graph",
            "Go to File…",
            "Settings",
            "Debug: Stop",
        ];
        let mut f = Finder {
            query: ">".into(),
            ..Default::default()
        };
        // Nothing typed: the recent ones first, most recent first, then the
        // rest in order; a recent title no longer listed is skipped.
        f.refresh_commands(
            &titles,
            &["Settings".into(), "Gone".into(), "Import Graph".into()],
        );
        assert_eq!(f.results, [3, 1, 0, 2, 4]);
        // Typed: only the matches, best first, and none cut off.
        f.query = "> graph".into();
        f.refresh_commands(&titles, &[]);
        assert_eq!(f.results.len(), 2);
        assert!(f.results.contains(&0) && f.results.contains(&1));
        f.query = ">stop".into();
        f.refresh_commands(&titles, &[]);
        assert_eq!(f.results, [4]);
        let many: Vec<String> = (0..100).map(|i| format!("Command {i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        f.query = ">command".into();
        f.refresh_commands(&many, &[]);
        assert_eq!(f.results.len(), 100, "commands are not capped");
    }
}
