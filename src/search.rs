//! Project-wide literal text search built on ripgrep's grep crates.

use std::path::PathBuf;
use std::sync::Arc;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

use crate::fs_scan::FileEntry;

/// Stop collecting after this many matches to keep the UI snappy.
pub const MAX_HITS: usize = 500;
const MAX_PREVIEW_CHARS: usize = 200;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub abs: PathBuf,
    pub rel: String,
    pub line: usize,
    pub preview: String,
}

/// Literal, smart-case search over the project's file list.
/// Blocking; run off the UI thread.
pub fn search(files: Arc<Vec<FileEntry>>, query: String) -> Vec<SearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let Ok(matcher) = RegexMatcherBuilder::new()
        .case_smart(true)
        .build(&escape_regex(query))
    else {
        return Vec::new();
    };
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .build();

    let mut hits: Vec<SearchHit> = Vec::new();
    for file in files.iter() {
        let _ = searcher.search_path(
            &matcher,
            &file.abs,
            UTF8(|line, text| {
                hits.push(SearchHit {
                    abs: file.abs.clone(),
                    rel: file.rel.clone(),
                    line: line as usize,
                    preview: preview_of(text),
                });
                Ok(hits.len() < MAX_HITS)
            }),
        );
        if hits.len() >= MAX_HITS {
            break;
        }
    }
    hits
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

    #[test]
    fn finds_literal_matches_with_line_numbers() {
        let dir = std::env::temp_dir().join("clew-search-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one\nneedle here\nthree\n").unwrap();
        std::fs::write(dir.join("b.txt"), "no match\n").unwrap();

        let files = Arc::new(vec![
            FileEntry {
                abs: dir.join("a.txt"),
                rel: "a.txt".into(),
            },
            FileEntry {
                abs: dir.join("b.txt"),
                rel: "b.txt".into(),
            },
        ]);
        let hits = search(files, "needle".to_string());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
        assert_eq!(hits[0].rel, "a.txt");
        assert!(hits[0].preview.contains("needle"));
    }
}
