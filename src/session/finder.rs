//! Fuzzy finder state and matching: files (Cmd+P), project symbols (Cmd+T),
//! `:N` go-to-line, commands (a query starting with `>`), and the command
//! bar (Cmd+Shift+P), which runs a command or opens a file.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::fs_scan::FileEntry;
use crate::index::SymbolEntry;

pub const MAX_RESULTS: usize = 60;

/// Most commands the command bar lists above the files a query matches.
pub const BAR_COMMANDS: usize = 6;

/// Most symbols the file finder lists below the files a query matches.
pub const FILE_FINDER_SYMBOLS: usize = 20;

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
    /// Opened as the command bar (Cmd+Shift+P): with nothing typed it
    /// suggests what to do; a query matches commands, then files.
    pub bar: bool,
    /// Go to File found no file by the query: its results are the command
    /// bar's — the commands it names, then files. "settings" said only "No
    /// matches" there, though the field over the window that opens it
    /// says it searches commands too.
    pub bar_fallback: bool,
}

impl Finder {
    /// Parse a `:N` query as a 1-based go-to-line request.
    pub fn goto_line(&self) -> Option<usize> {
        let n: usize = self.query.trim().strip_prefix(':')?.trim().parse().ok()?;
        (n > 0).then_some(n)
    }

    /// A `file:N` query — "pricing.py:37", as a compiler or a stack trace
    /// writes a place: the file part, and the 1-based line to open it at.
    /// It found "No matches", the `:37` taken as part of the name. A column
    /// after the line ("main.rs:12:5", as rustc, gcc and tsc write it) is
    /// let go: it read as file "main.rs:12", line 5.
    pub fn file_line(&self) -> Option<(&str, usize)> {
        let number = |s: &str| s.trim().parse::<usize>().ok().filter(|n| *n > 0);
        let (head, last) = self.query.trim().rsplit_once(':')?;
        let last = number(last)?;
        let (file, line) = match head.rsplit_once(':') {
            Some((file, line)) if number(line).is_some() => (file, number(line)?),
            _ => (head, last),
        };
        let file = file.trim();
        (!file.is_empty()).then_some((file, line))
    }

    /// What files are matched by: the query, less a `:N` line after it.
    fn file_query(&self) -> String {
        self.file_line().map_or_else(
            || self.query.trim().to_string(),
            |(file, _)| file.to_string(),
        )
    }

    /// Whether the query asks for a line (`:`, as Go to Line opens it) —
    /// with or without its number typed yet.
    pub fn is_line_query(&self) -> bool {
        self.query.trim_start().starts_with(':')
    }

    /// Whether the query asks for commands: it starts with `>`, as in the
    /// palette Cmd+Shift+P opens (deleting the `>` gives the files back).
    pub fn is_commands(&self) -> bool {
        self.query.trim_start().starts_with('>')
    }

    /// The command bar with nothing typed: its suggestions show.
    pub fn is_home(&self) -> bool {
        self.bar && self.query.trim().is_empty()
    }

    /// The command bar with a query that is neither `>` commands nor a
    /// `:N` line: the commands it matches, then the files.
    pub fn is_bar_search(&self) -> bool {
        (self.bar || self.bar_fallback)
            && !self.is_home()
            && !self.is_commands()
            && !self.query.trim().starts_with(':')
    }

    /// The query less its `>`.
    pub fn command_query(&self) -> &str {
        self.query.trim_start().trim_start_matches('>').trim()
    }

    /// Rank the files, then — once something is typed — the project's
    /// symbols it names: the top bar's field searches "files, symbols and
    /// commands", and a class or function typed there found nothing. A
    /// result at or past `files.len()` is the symbol at that index less
    /// `files.len()`.
    ///
    /// With nothing typed, the files read lately (`recent`, most recent
    /// first) lead, then the rest in their order: the project's first files
    /// alphabetically were rarely the ones wanted.
    pub fn refresh_files(
        &mut self,
        files: &[FileEntry],
        symbols: &[SymbolEntry],
        recent: &[&std::path::Path],
    ) {
        self.selected = 0;
        let query = self.file_query();
        let query = query.as_str();
        let mut results = if query.is_empty() {
            let mut order: Vec<usize> = recent
                .iter()
                .filter_map(|r| files.iter().position(|f| f.abs == *r))
                .take(MAX_RESULTS)
                .collect();
            let rest: Vec<usize> = (0..files.len())
                .filter(|i| !order.contains(i))
                .take(MAX_RESULTS - order.len())
                .collect();
            order.extend(rest);
            order
        } else {
            rank(
                query,
                files.iter().map(|f| f.rel.as_str()),
                MAX_RESULTS,
                true,
            )
        };
        if !query.is_empty() {
            let n = files.len();
            results.extend(
                tests_last(
                    rank(
                        query,
                        symbols.iter().map(|s| s.name.as_str()),
                        usize::MAX,
                        true,
                    ),
                    symbols,
                )
                .into_iter()
                .take(FILE_FINDER_SYMBOLS)
                .map(|i| i + n),
            );
            // What the query names leads: "tot" listed every file under
            // `teashop/` (t…o…t through the folder) above the function
            // `total`. Files and symbols each keep their rank within a tier.
            let tier = |i: usize| match i.checked_sub(n) {
                Some(s) => symbols
                    .get(s)
                    .map_or(4, |s| if contains_words(&s.name, query) { 1 } else { 3 }),
                None => files.get(i).map_or(4, |f| file_tier(&f.rel, query)),
            };
            // Each key worked out once: a file's builds a matcher.
            results.sort_by_cached_key(|&i| tier(i));
        }
        self.results = results;
    }

    pub fn refresh_symbols(&mut self, symbols: &[SymbolEntry]) {
        self.selected = 0;
        let mut ranked = tests_last(
            rank(
                &self.query,
                symbols.iter().map(|s| s.name.as_str()),
                usize::MAX,
                true,
            ),
            symbols,
        );
        ranked.truncate(MAX_RESULTS);
        self.results = ranked;
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

    /// Rank for the command bar: the best few of the commands titled
    /// `titles`, then the files. A result at or past `titles.len()` is the
    /// file at that index less `titles.len()`.
    pub fn refresh_bar(&mut self, titles: &[&str], files: &[FileEntry]) {
        self.selected = 0;
        let query = self.query.trim().to_string();
        let query = query.as_str();
        let mut results = rank_commands(query, titles);
        let n = titles.len();
        // A file asked for at a line is matched by its name.
        let file_query = self.file_query();
        let mut ranked = rank(
            &file_query,
            files.iter().map(|f| f.rel.as_str()),
            MAX_RESULTS,
            true,
        );
        // Files the query names ahead of those it reaches through a folder.
        ranked.sort_by_key(|&i| files.get(i).map_or(4, |f| file_tier(&f.rel, &file_query)));
        results.extend(ranked.into_iter().map(|i| i + n));
        self.results = results;
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

/// The best few of the commands titled `titles` for the command bar's
/// `query`: those whose titles hold every word typed, when some do, else
/// those that hold its letters in order ("gtl" still finds Go to Line…).
/// Each word was matched by its letters anywhere in a title: "go to line"
/// listed "Light theme: One Light", "Rosé Pine Dawn" and "Start
/// listening…" under Go to Line….
fn rank_commands(query: &str, titles: &[&str]) -> Vec<usize> {
    let ranked = rank(query, titles.iter().copied(), titles.len(), false);
    let worded: Vec<usize> = ranked
        .iter()
        .copied()
        .filter(|&i| contains_words(titles[i], query))
        .collect();
    let mut ranked = if worded.is_empty() { ranked } else { worded };
    ranked.truncate(BAR_COMMANDS);
    ranked
}

/// Whether `name` holds every word of `query` as it is spelled (case
/// aside): a name that says what was typed, not one that merely has its
/// letters in order.
fn contains_words(name: &str, query: &str) -> bool {
    let name = name.to_lowercase();
    let mut words = query.split_whitespace().peekable();
    words.peek().is_some() && words.all(|w| name.contains(&w.to_lowercase()))
}

/// How well a file's path answers `query`, best first: 0 when its name
/// holds the query's words, 2 when its name has the query's letters in
/// order, 4 when only its path does (the query reached it through a
/// folder). Between them sit a symbol named by the query (1) and one
/// matched by its letters (3). A query with a `/` asks for a path, and
/// every file it matches is of one tier, ranked by the match alone.
fn file_tier(rel: &str, query: &str) -> u8 {
    if query.contains('/') || query.contains('\\') {
        return 0;
    }
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    if contains_words(name, query) {
        return 0;
    }
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    if pattern
        .score(Utf32Str::new(name, &mut buf), &mut matcher)
        .is_some()
    {
        2
    } else {
        4
    }
}

/// `ranked` (indices into `symbols`) with the tests after the rest, each
/// part in its order: a project's tests are named for what they test, and
/// crowded out what was asked for.
fn tests_last(ranked: Vec<usize>, symbols: &[SymbolEntry]) -> Vec<usize> {
    let (tests, rest): (Vec<usize>, Vec<usize>) = ranked
        .into_iter()
        .partition(|&i| symbols.get(i).is_some_and(|s| s.is_test));
    rest.into_iter().chain(tests).collect()
}

/// Rank `items` against `query`, at most `limit` of them; empty query lists
/// the first entries as-is. Higher score first, shorter item wins ties.
/// `paths` scores the way a path is matched (separators weigh more).
pub(crate) fn rank<'a>(
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

    /// The command bar lists the commands that say what was typed, when
    /// some do — not every title holding its letters somewhere — and
    /// falls back to the letters when none says it.
    #[test]
    fn the_command_bar_lists_commands_that_say_what_was_typed() {
        let titles = [
            "Light theme: One Light",
            "Rosé Pine Dawn",
            "Start listening… On/Off",
            "Go to Line…",
            "Go to File…",
        ];
        let bar = |query: &str| {
            let mut f = Finder {
                query: query.into(),
                ..Default::default()
            };
            f.refresh_bar(&titles, &[]);
            f.results.iter().map(|&i| titles[i]).collect::<Vec<_>>()
        };
        assert_eq!(bar("go to line"), ["Go to Line…"]);
        assert_eq!(bar("go to"), ["Go to Line…", "Go to File…"]);
        // An abbreviation says no title's words: its letters find it.
        assert!(bar("gtl").contains(&"Go to Line…"), "{:?}", bar("gtl"));
    }

    #[test]
    fn a_file_at_a_line_is_found_by_its_name() {
        let mut f = Finder {
            query: "pricing.py:37".into(),
            ..Finder::default()
        };
        assert_eq!(f.file_line(), Some(("pricing.py", 37)));
        let files: Vec<FileEntry> = ["teashop/pricing.py", "teashop/models.py"]
            .iter()
            .map(|rel| FileEntry {
                rel: rel.to_string(),
                abs: std::path::PathBuf::from("/p").join(rel),
            })
            .collect();
        f.refresh_files(&files, &[], &[]);
        assert_eq!(f.results.first(), Some(&0), "{:?}", f.results);
        f.refresh_bar(&["Open Folder…"], &files);
        assert!(f.results.contains(&1), "{:?}", f.results);
        // A column after the line, as compilers write a place, is let go.
        f.query = "pricing.py:37:5".into();
        assert_eq!(f.file_line(), Some(("pricing.py", 37)));
        f.query = r"C:\shop\pricing.py:37".into();
        assert_eq!(f.file_line(), Some((r"C:\shop\pricing.py", 37)));
        // A line alone, or no line, is not a file at a line.
        for query in [":37", "pricing.py", "pricing.py:", "pricing.py:0", ":37:5"] {
            f.query = query.into();
            assert_eq!(f.file_line(), None, "{query}");
        }
    }

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

    fn files(rels: &[&str]) -> Vec<FileEntry> {
        rels.iter()
            .map(|rel| FileEntry {
                abs: std::path::PathBuf::from("/p").join(rel),
                rel: rel.to_string(),
            })
            .collect()
    }

    /// With nothing typed, the files read lately lead, most recent first,
    /// then the rest in their order; once something is typed, the match
    /// decides.
    #[test]
    fn an_empty_file_query_leads_with_the_files_read_lately() {
        let files = files(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let (c, a) = (files[2].abs.clone(), files[0].abs.clone());
        let gone = std::path::PathBuf::from("/p/gone.rs");
        let mut f = Finder::default();
        f.refresh_files(&files, &[], &[&c, &gone, &a]);
        assert_eq!(f.results, [2, 0, 1, 3]);
        f.query = "b".into();
        f.refresh_files(&files, &[], &[&c, &a]);
        assert_eq!(f.results.first(), Some(&1));
    }

    /// The symbols a query names list the tests last: a project's tests are
    /// named for what they test, and crowded out what was asked for.
    #[test]
    fn tests_come_after_the_symbols_they_are_named_for() {
        let symbol = |name: &str, is_test: bool| SymbolEntry {
            name: name.into(),
            kind: "function".into(),
            rel: "src/lib.rs".into(),
            abs: "/p/src/lib.rs".into(),
            line: 1,
            is_test,
            entry: None,
        };
        let symbols = [
            symbol("parse", true),
            symbol("parse_line", true),
            symbol("parse_file", false),
            symbol("parser", false),
        ];
        let mut f = Finder {
            query: "parse".into(),
            ..Default::default()
        };
        f.refresh_symbols(&symbols);
        assert_eq!(f.results.len(), 4);
        assert!(
            f.results[..2].iter().all(|&i| !symbols[i].is_test),
            "{:?}",
            f.results
        );
        // And past the files in the file finder.
        let files = files(&["src/lib.rs"]);
        f.refresh_files(&files, &symbols, &[]);
        let named: Vec<usize> = f
            .results
            .iter()
            .filter(|&&i| i >= 1)
            .map(|i| i - 1)
            .collect();
        assert!(named[..2].iter().all(|&i| !symbols[i].is_test), "{named:?}");
    }

    /// "tot" reached every file under `teashop/` through its folder, and
    /// those were listed above the function `total`, which it names.
    #[test]
    fn what_the_query_names_leads_what_it_reaches_through_a_folder() {
        let files = files(&[
            "teashop/catalog.py",
            "teashop/checkout.py",
            "teashop/totals.py",
            "teashop/top_list.py",
        ]);
        let symbol = |name: &str| SymbolEntry {
            name: name.into(),
            kind: "function".into(),
            rel: "teashop/checkout.py".into(),
            abs: "/p/teashop/checkout.py".into(),
            line: 1,
            is_test: false,
            entry: None,
        };
        let symbols = [symbol("total"), symbol("to_text")];
        let mut f = Finder {
            query: "tot".into(),
            ..Default::default()
        };
        f.refresh_files(&files, &symbols, &[]);
        let n = files.len();
        // totals.py names it; then `total`; then the file whose name has its
        // letters (top_list.py); then `to_text`; the folder matches last.
        assert_eq!(f.results[0], 2, "{:?}", f.results);
        assert_eq!(f.results[1], n, "{:?}", f.results);
        assert_eq!(f.results[2], 3, "{:?}", f.results);
        assert_eq!(f.results[3], n + 1, "{:?}", f.results);
        assert!(f.results[4..].iter().all(|&i| i < 2), "{:?}", f.results);
        // A path asks for the path: its files keep the match's order.
        f.query = "teashop/ch".into();
        f.refresh_files(&files, &symbols, &[]);
        assert_eq!(f.results.first(), Some(&1), "{:?}", f.results);
        // The command bar's files are ordered the same way.
        f.query = "tot".into();
        f.refresh_bar(&[], &files);
        assert_eq!(f.results[..2], [2, 3], "{:?}", f.results);
    }

    #[test]
    fn the_command_bar_matches_commands_then_files() {
        let titles = ["Call Graph", "Settings", "Stats"];
        let files: Vec<FileEntry> = ["src/lib.rs", "src/settings.rs", "README.md"]
            .iter()
            .map(|rel| FileEntry {
                abs: rel.into(),
                rel: rel.to_string(),
            })
            .collect();
        let mut f = Finder {
            bar: true,
            ..Default::default()
        };
        assert!(f.is_home() && !f.is_bar_search() && !f.is_commands());
        f.query = "settings".into();
        assert!(f.is_bar_search() && !f.is_home());
        f.refresh_bar(&titles, &files);
        // The command first, then the file, its index past the titles.
        assert_eq!(f.results, [1, 3 + 1]);
        // `>` still asks for commands alone, `:N` for a line.
        f.query = ">set".into();
        assert!(f.is_commands() && !f.is_bar_search());
        f.query = ":12".into();
        assert!(!f.is_bar_search() && f.goto_line() == Some(12));
        // At most a few commands lead, however many match.
        let many: Vec<String> = (0..20).map(|i| format!("Settings {i}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        f.query = "settings".into();
        f.refresh_bar(&many, &files);
        assert_eq!(
            f.results.iter().filter(|&&i| i < many.len()).count(),
            BAR_COMMANDS
        );
        assert_eq!(f.results.last(), Some(&(20 + 1)));
    }
}
