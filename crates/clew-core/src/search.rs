//! Project-wide text search built on ripgrep's grep crates, with literal or
//! regex patterns, case and whole-word options, and include/exclude globs.

use std::path::PathBuf;
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

use crate::fs_scan::FileEntry;

/// Stop collecting after this many matches to keep the UI snappy.
pub const MAX_HITS: usize = 2000;
const MAX_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub abs: PathBuf,
    pub rel: String,
    pub line: usize,
    pub preview: String,
}

/// A query plus its toggles, as entered in the search sidebar.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub query: String,
    /// Treat the query as a regular expression rather than literal text.
    pub regex: bool,
    /// Force case-sensitive matching (otherwise smart-case).
    pub case_sensitive: bool,
    /// Match only whole words (`\b` boundaries).
    pub whole_word: bool,
    /// Comma/space-separated globs; when non-empty, only matching files search.
    pub include: String,
    /// Comma/space-separated globs of files to skip.
    pub exclude: String,
}

/// Outcome of a search: the hits, plus an error message when the pattern or a
/// glob failed to compile (so the UI can explain an empty result).
#[derive(Debug, Clone, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
}

/// Search the project's file list with `opts`. Blocking; run off the UI thread.
pub fn search(files: Arc<Vec<FileEntry>>, opts: SearchOptions) -> SearchResult {
    let query = opts.query.trim();
    if query.is_empty() {
        return SearchResult::default();
    }

    // Literal queries are escaped; regex queries pass through. Whole-word wraps
    // the pattern in word boundaries.
    let base = if opts.regex {
        query.to_string()
    } else {
        escape_regex(query)
    };
    let pattern = if opts.whole_word {
        format!(r"\b(?:{base})\b")
    } else {
        base
    };

    let mut builder = RegexMatcherBuilder::new();
    if opts.case_sensitive {
        builder.case_insensitive(false).case_smart(false);
    } else {
        builder.case_smart(true);
    }
    let matcher = match builder.build(&pattern) {
        Ok(m) => m,
        Err(e) => {
            return SearchResult {
                hits: Vec::new(),
                error: Some(format!("Invalid pattern: {e}")),
            };
        }
    };

    let include = match build_globset(&opts.include) {
        Ok(g) => g,
        Err(e) => {
            return SearchResult {
                hits: Vec::new(),
                error: Some(format!("include: {e}")),
            };
        }
    };
    let exclude = match build_globset(&opts.exclude) {
        Ok(g) => g,
        Err(e) => {
            return SearchResult {
                hits: Vec::new(),
                error: Some(format!("exclude: {e}")),
            };
        }
    };

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut hits: Vec<SearchHit> = Vec::new();
    for file in files.iter() {
        if let Some(set) = &include
            && !set.is_match(&file.rel)
        {
            continue;
        }
        if let Some(set) = &exclude
            && set.is_match(&file.rel)
        {
            continue;
        }
        let sink = UTF8(|line, text: &str| {
            hits.push(SearchHit {
                abs: file.abs.clone(),
                rel: file.rel.clone(),
                line: line as usize,
                preview: preview_of(text),
            });
            Ok(hits.len() < MAX_HITS)
        });
        // Notebooks are searched through their script projection — raw .ipynb
        // JSON is base64/noise, and projection lines are the notebook's
        // canonical line space (hits jump to the owning cell).
        if crate::notebook::is_notebook(&file.abs) {
            let projection = std::fs::read_to_string(&file.abs)
                .ok()
                .and_then(|json| crate::notebook::parse(&json))
                .map(|nb| nb.projection);
            if let Some(projection) = projection {
                let _ = searcher.search_slice(&matcher, projection.as_bytes(), sink);
            }
        } else {
            let _ = searcher.search_path(&matcher, &file.abs, sink);
        }
        if hits.len() >= MAX_HITS {
            break;
        }
    }
    SearchResult { hits, error: None }
}

/// Compile a comma/whitespace-separated glob spec into a set, or `None` when
/// the spec is empty. `*` matches across path separators, so `*.rs` matches
/// `src/main.rs` the way an editor's file filter would.
fn build_globset(spec: &str) -> Result<Option<GlobSet>, String> {
    let patterns: Vec<&str> = spec
        .split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let glob = Glob::new(pat).map_err(|e| e.to_string())?;
        builder.add(glob);
    }
    builder.build().map(Some).map_err(|e| e.to_string())
}

fn preview_of(text: &str) -> String {
    let trimmed = text.trim_end_matches(['\n', '\r']).replace('\t', "    ");
    if trimmed.chars().count() <= MAX_PREVIEW_CHARS {
        return trimmed;
    }
    let mut out: String = trimmed.chars().take(MAX_PREVIEW_CHARS).collect();
    out.push('…');
    out
}

/// Escape regex metacharacters so the query is matched literally.
fn escape_regex(s: &str) -> String {
    const META: &[char] = &[
        '\\', '.', '+', '*', '?', '(', ')', '|', '[', ']', '{', '}', '^', '$', '#', '&', '-', '~',
    ];
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if META.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_regex_metacharacters() {
        let escaped = escape_regex("a.b(c)*+?");
        assert_eq!(escaped, r"a\.b\(c\)\*\+\?");
    }

    fn literal(query: &str) -> SearchOptions {
        SearchOptions {
            query: query.to_string(),
            ..Default::default()
        }
    }

    fn fixture(name: &str) -> (std::path::PathBuf, Arc<Vec<FileEntry>>) {
        // A per-test directory so tests don't race on a shared path.
        let dir = std::env::temp_dir().join(format!("clew-search-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\nneedle here\nthree\n").unwrap();
        std::fs::write(dir.join("b.txt"), "no match\n").unwrap();
        std::fs::write(dir.join("c.rs"), "let needle = 1;\nlet needles = 2;\n").unwrap();
        let files = Arc::new(vec![
            FileEntry {
                abs: dir.join("a.txt"),
                rel: "a.txt".into(),
            },
            FileEntry {
                abs: dir.join("b.txt"),
                rel: "b.txt".into(),
            },
            FileEntry {
                abs: dir.join("c.rs"),
                rel: "c.rs".into(),
            },
        ]);
        (dir, files)
    }

    #[test]
    fn finds_literal_matches_with_line_numbers() {
        let (_dir, files) = fixture("0");
        let out = search(files, literal("needle here"));
        assert!(out.error.is_none());
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].line, 2);
        assert_eq!(out.hits[0].rel, "a.txt");
        assert!(out.hits[0].preview.contains("needle"));
    }

    #[test]
    fn whole_word_excludes_substrings() {
        let (_dir, files) = fixture("1");
        // "needle" appears in a.txt, and in c.rs as both `needle` and `needles`.
        let mut opts = literal("needle");
        opts.whole_word = true;
        let out = search(files, opts);
        // Whole-word drops the `needles` line but keeps the two exact ones.
        assert_eq!(out.hits.len(), 2);
        assert!(out.hits.iter().all(|h| !h.preview.contains("needles")));
    }

    #[test]
    fn regex_and_bad_regex() {
        let (_dir, files) = fixture("2");
        let mut opts = literal(r"need\w+e");
        opts.regex = true;
        let out = search(files.clone(), opts);
        assert!(out.error.is_none());
        assert!(!out.hits.is_empty());

        let mut bad = literal("(unclosed");
        bad.regex = true;
        let out = search(files, bad);
        assert!(out.error.is_some());
        assert!(out.hits.is_empty());
    }

    #[test]
    fn include_and_exclude_globs() {
        let (_dir, files) = fixture("3");
        let mut only_rs = literal("needle");
        only_rs.include = "*.rs".into();
        let out = search(files.clone(), only_rs);
        assert!(out.hits.iter().all(|h| h.rel == "c.rs"));

        let mut no_rs = literal("needle");
        no_rs.exclude = "*.rs".into();
        let out = search(files, no_rs);
        assert!(out.hits.iter().all(|h| h.rel != "c.rs"));
    }

    #[test]
    fn escapes_regex_metacharacters_when_literal() {
        let (_dir, files) = fixture("4");
        // A literal query with regex metacharacters matches nothing here but
        // must not error out (proving it was escaped, not compiled as regex).
        let out = search(files, literal("a.b(c)*+?"));
        assert!(out.error.is_none());
    }
}
