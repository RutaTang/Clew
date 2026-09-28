//! Project-wide text search built on ripgrep's grep crates, with literal or
//! regex patterns, case and whole-word options, and include/exclude globs.
//!
//! Every file is read the way every other project read is: confined to the
//! root (re-checked, since the scan can be stale), opened ONCE without
//! following a symlink or blocking on a FIFO, and bounded — by a per-file
//! size cap and by a cap on how much of one line the searcher will buffer.
//! Bytes that are not UTF-8 are searched too (replaced for display, never a
//! reason to drop the file's hits).
//!
//! A file that was not (fully) searched is reported in
//! [`SearchReport::skipped`] with the reason, rather than silently missing:
//! one that is no longer a regular file inside the project (swapped for a
//! link since the scan), one past the size cap, one that GREW past the cap
//! while it was being read (searched up to the cap — its hits so far are
//! kept — and reported), a line too long to buffer, an I/O error. Only a file
//! deleted since the scan is passed over without a word: there is nothing
//! left to search.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};

use crate::fs_scan::FileEntry;

/// Stop collecting after this many matches to keep the UI snappy.
pub const MAX_HITS: usize = 2000;
const MAX_PREVIEW_CHARS: usize = 200;
/// Largest `.ipynb` searched through its projection. Notebooks embed base64
/// images, so their JSON runs far past source-file sizes; still bounded.
const MAX_NOTEBOOK_BYTES: u64 = 64 * 1024 * 1024;
/// Largest file searched. The grep streams, so this bounds time rather than
/// memory: a multi-gigabyte dump or log in the tree is not source a reader is
/// looking for, and scanning it would hold the search (and, remotely, the
/// server's blocking pool) for minutes.
pub const MAX_SEARCH_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Most of ONE line the searcher buffers. Without it, a minified bundle that
/// is one 50 MB line is read into memory whole before it can be matched.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

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

/// A file the search could not (fully) read, and why — a protocol wire type
/// (clew-server reports these in `SearchResults`), re-exported here.
pub use clew_protocol::SkippedFile;

/// [`SearchResult`] plus the files that were not searched — too large, a line
/// too long to buffer, not a plain file, an I/O error. A search that quietly
/// covers less than it claims is how a real hit goes missing unnoticed.
#[derive(Debug, Clone, Default)]
pub struct SearchReport {
    pub hits: Vec<SearchHit>,
    pub error: Option<String>,
    pub skipped: Vec<SkippedFile>,
}

impl From<SearchReport> for SearchResult {
    fn from(r: SearchReport) -> Self {
        SearchResult {
            hits: r.hits,
            error: r.error,
        }
    }
}

/// Search the files of the project at `root`, every read confined to it:
/// each file is re-checked to be a regular file inside the root before it is
/// read — the scan can be stale, and a path swapped for a symlink would
/// otherwise read outside the project. The root is a required argument, so
/// containment can never be left out by omission. Blocking; run off the UI
/// thread.
pub fn search_in(root: &Path, files: Arc<Vec<FileEntry>>, opts: SearchOptions) -> SearchResult {
    search_report(root, files, opts).into()
}

/// [`search_in`], reporting the files that could not be searched.
pub fn search_report(root: &Path, files: Arc<Vec<FileEntry>>, opts: SearchOptions) -> SearchReport {
    run(root, &files, &opts, MAX_SEARCH_FILE_BYTES)
}

fn run(
    root: &Path,
    files: &[FileEntry],
    opts: &SearchOptions,
    max_file_bytes: u64,
) -> SearchReport {
    let query = opts.query.trim();
    if query.is_empty() {
        return SearchReport::default();
    }
    let failed = |error: String| SearchReport {
        error: Some(error),
        ..SearchReport::default()
    };
    let matcher = match build_matcher(query, opts) {
        Ok(m) => m,
        Err(e) => return failed(format!("Invalid pattern: {e}")),
    };
    let include = match build_globset(&opts.include) {
        Ok(g) => g,
        Err(e) => return failed(format!("include: {e}")),
    };
    let exclude = match build_globset(&opts.exclude) {
        Ok(g) => g,
        Err(e) => return failed(format!("exclude: {e}")),
    };

    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .line_number(true)
        .heap_limit(Some(MAX_LINE_BYTES))
        .build();

    let mut report = SearchReport::default();
    for file in files {
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
        let skip = |reason: String| SkippedFile {
            rel: file.rel.clone(),
            reason,
        };
        // Re-verify before reading: the scan may be stale, and a path swapped
        // for a symlink would otherwise let a grep read outside the project.
        // Refused is reported; merely deleted since the scan is not.
        if !crate::fs_scan::is_inside(root, &file.abs) {
            if std::fs::symlink_metadata(&file.abs).is_ok() {
                report.skipped.push(skip(
                    "not a regular file inside the project (a link, or moved since the scan)"
                        .into(),
                ));
            }
            continue;
        }
        // ONE open, and everything below reads this handle: re-opening by
        // path (as the grep used to) re-resolves the name, so a file swapped
        // for a symlink or a FIFO after the check above was followed or
        // blocked on.
        let f = match crate::statefile::open_plain_checked(&file.abs) {
            Ok(Some(f)) => f,
            Ok(None) => continue, // deleted since the scan
            Err(e) => {
                report.skipped.push(skip(e.to_string()));
                continue;
            }
        };
        let is_notebook = crate::notebook::is_notebook(&file.abs);
        let cap = if is_notebook {
            MAX_NOTEBOOK_BYTES
        } else {
            max_file_bytes
        };
        match f.metadata() {
            Ok(m) if m.len() > cap => {
                report
                    .skipped
                    .push(skip(format!("larger than {} MB", cap / (1024 * 1024))));
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                report.skipped.push(skip(e.to_string()));
                continue;
            }
        }
        let searched = if is_notebook {
            // Notebooks are searched through their script projection — raw
            // .ipynb JSON is base64/noise, and projection lines are the
            // notebook's canonical line space (hits jump to the owning cell).
            // Bounded at the read: projecting holds the whole JSON *and* the
            // script in memory at once.
            search_notebook(&mut searcher, &matcher, f, cap, file, &mut report.hits)
        } else {
            let sink = hit_sink(file, &mut report.hits);
            searcher
                .search_reader(&matcher, Capped::new(f, cap), sink)
                .map_err(|e| e.to_string())
        };
        if let Err(reason) = searched {
            report.skipped.push(skip(reason));
        }
        if report.hits.len() >= MAX_HITS {
            break;
        }
    }
    report
}

/// A reader that yields at most `cap` bytes and then FAILS if there is more,
/// instead of ending quietly the way `take` does: a file that grew past the
/// cap between the size check and the read would otherwise look searched in
/// full. What was read before the error has been searched (its hits stand);
/// the error puts the file in [`SearchReport::skipped`].
struct Capped<R> {
    inner: R,
    left: u64,
    cap: u64,
}

impl<R: Read> Capped<R> {
    fn new(inner: R, cap: u64) -> Self {
        Capped {
            inner,
            left: cap,
            cap,
        }
    }
}

impl<R: Read> Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::other(format!(
                    "grew past {} MB while it was being searched (searched up to there)",
                    self.cap / (1024 * 1024)
                ))),
            };
        }
        let want = buf
            .len()
            .min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// The pattern as a matcher: literal queries are escaped, regex queries pass
/// through; whole-word wraps the pattern in word boundaries.
fn build_matcher(query: &str, opts: &SearchOptions) -> Result<RegexMatcher, String> {
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
    builder.build(&pattern).map_err(|e| e.to_string())
}

/// A sink that records every matching line of `file` into `hits`. Lossy: a
/// line that is not valid UTF-8 (a Latin-1 comment, a stray byte) is shown
/// with replacement characters instead of aborting the file — the strict sink
/// errored on the first such line, and every hit in the file vanished with
/// the error nobody read.
fn hit_sink<'a>(
    file: &'a FileEntry,
    hits: &'a mut Vec<SearchHit>,
) -> Lossy<impl FnMut(u64, &str) -> Result<bool, std::io::Error> + 'a> {
    Lossy(move |line, text: &str| {
        hits.push(SearchHit {
            abs: file.abs.clone(),
            rel: file.rel.clone(),
            line: line as usize,
            preview: preview_of(text),
        });
        Ok(hits.len() < MAX_HITS)
    })
}

fn search_notebook(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    f: std::fs::File,
    cap: u64,
    file: &FileEntry,
    hits: &mut Vec<SearchHit>,
) -> Result<(), String> {
    let mut bytes = Vec::new();
    f.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > cap {
        return Err(format!("larger than {} MB", cap / (1024 * 1024)));
    }
    let json = String::from_utf8(bytes).map_err(|_| "not valid UTF-8 JSON".to_string())?;
    let notebook = crate::notebook::parse(&json).ok_or("not a readable notebook")?;
    let sink = hit_sink(file, hits);
    searcher
        .search_slice(matcher, notebook.projection.as_bytes(), sink)
        .map_err(|e| e.to_string())
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
    use crate::testutil::TempDir;

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

    fn fixture(name: &str) -> (TempDir, Arc<Vec<FileEntry>>) {
        let dir = TempDir::new(&format!("search-{name}"));
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
        let (dir, files) = fixture("literal");
        let out = search_in(&dir, files, literal("needle here"));
        assert!(out.error.is_none());
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].line, 2);
        assert_eq!(out.hits[0].rel, "a.txt");
        assert!(out.hits[0].preview.contains("needle"));
    }

    #[test]
    fn whole_word_excludes_substrings() {
        let (dir, files) = fixture("whole-word");
        // "needle" appears in a.txt, and in c.rs as both `needle` and `needles`.
        let mut opts = literal("needle");
        opts.whole_word = true;
        let out = search_in(&dir, files, opts);
        // Whole-word drops the `needles` line but keeps the two exact ones.
        assert_eq!(out.hits.len(), 2);
        assert!(out.hits.iter().all(|h| !h.preview.contains("needles")));
    }

    #[test]
    fn regex_and_bad_regex() {
        let (dir, files) = fixture("regex");
        let mut opts = literal(r"need\w+e");
        opts.regex = true;
        let out = search_in(&dir, files.clone(), opts);
        assert!(out.error.is_none());
        assert!(!out.hits.is_empty());

        let mut bad = literal("(unclosed");
        bad.regex = true;
        let out = search_in(&dir, files, bad);
        assert!(out.error.is_some());
        assert!(out.hits.is_empty());
    }

    #[test]
    fn include_and_exclude_globs() {
        let (dir, files) = fixture("globs");
        let mut only_rs = literal("needle");
        only_rs.include = "*.rs".into();
        let out = search_in(&dir, files.clone(), only_rs);
        assert!(out.hits.iter().all(|h| h.rel == "c.rs"));

        let mut no_rs = literal("needle");
        no_rs.exclude = "*.rs".into();
        let out = search_in(&dir, files, no_rs);
        assert!(out.hits.iter().all(|h| h.rel != "c.rs"));
    }

    #[test]
    fn escapes_regex_metacharacters_when_literal() {
        let (dir, files) = fixture("meta");
        // A literal query with regex metacharacters matches nothing here but
        // must not error out (proving it was escaped, not compiled as regex).
        let out = search_in(&dir, files, literal("a.b(c)*+?"));
        assert!(out.error.is_none());
    }

    /// A Latin-1 byte in a matching line used to make the strict UTF-8 sink
    /// error out of the file — and the error was discarded, so every hit in
    /// it silently vanished. The lossy sink keeps them all.
    #[test]
    fn hits_in_a_file_that_is_not_utf8_are_found() {
        let dir = TempDir::new("search-latin1");
        std::fs::write(
            dir.join("legacy.c"),
            b"/* caf\xe9 needle */\nint x;\nint needle = 1;\n",
        )
        .unwrap();
        let files = Arc::new(vec![FileEntry {
            abs: dir.join("legacy.c"),
            rel: "legacy.c".into(),
        }]);
        let report = search_report(&dir, files, literal("needle"));
        assert_eq!(
            report.hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            [1, 3]
        );
        assert!(report.hits[0].preview.contains('\u{fffd}'));
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    /// A symlink leaf is never followed (with or without the root), and a
    /// FIFO never blocks the search — both used to be opened by path. A leaf
    /// the containment check refuses is reported, not silently dropped.
    #[test]
    #[cfg(unix)]
    fn symlinks_are_refused_and_fifos_do_not_block() {
        let scratch = TempDir::new("search-leaf");
        let dir = scratch.join("proj");
        let outside = scratch.join("outside");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "needle secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("link.txt")).unwrap();
        let fifo = dir.join("pipe.txt");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        std::fs::write(dir.join("ok.txt"), "needle ok\n").unwrap();
        let files = Arc::new(
            ["link.txt", "pipe.txt", "ok.txt", "deleted.txt"]
                .iter()
                .map(|rel| FileEntry {
                    abs: dir.join(rel),
                    rel: (*rel).into(),
                })
                .collect::<Vec<_>>(),
        );

        let (tx, rx) = std::sync::mpsc::channel();
        let (d, f) = (dir.clone(), files.clone());
        std::thread::spawn(move || {
            let _ = tx.send(search_report(&d, f, literal("needle")));
        });
        let confined = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the search blocked on the FIFO");
        assert_eq!(
            confined
                .hits
                .iter()
                .map(|h| h.rel.as_str())
                .collect::<Vec<_>>(),
            ["ok.txt"],
            "only the plain file is read"
        );
        let skipped: Vec<&str> = confined.skipped.iter().map(|s| s.rel.as_str()).collect();
        assert_eq!(
            skipped,
            ["link.txt", "pipe.txt"],
            "refused leaves are reported; a deleted one is not"
        );
    }

    /// A file past the cap is not scanned, and the report says so instead of
    /// the file quietly having no hits.
    #[test]
    fn an_over_cap_file_is_skipped_and_reported() {
        let dir = TempDir::new("search-cap");
        std::fs::write(dir.join("big.sql"), "needle\n".repeat(64)).unwrap();
        std::fs::write(dir.join("small.sql"), "needle\n").unwrap();
        let files = vec![
            FileEntry {
                abs: dir.join("big.sql"),
                rel: "big.sql".into(),
            },
            FileEntry {
                abs: dir.join("small.sql"),
                rel: "small.sql".into(),
            },
        ];
        let report = run(&dir, &files, &literal("needle"), 100);
        assert_eq!(
            report
                .hits
                .iter()
                .map(|h| h.rel.as_str())
                .collect::<Vec<_>>(),
            ["small.sql"]
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].rel, "big.sql");
        assert!(report.skipped[0].reason.contains("larger than"));
    }

    /// A file that grows past the cap after the size check is not quietly
    /// searched up to the cap and called complete: the reader fails past the
    /// cap, and the search reports it (its earlier hits stand).
    #[test]
    fn a_file_that_grows_past_the_cap_is_reported_not_truncated_silently() {
        let mut exact = Capped::new(&b"0123456789"[..], 10);
        let mut all = Vec::new();
        exact.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"0123456789", "exactly the cap is fine");

        let mut grown = Capped::new(&b"0123456789+"[..], 10);
        let mut some = Vec::new();
        let err = grown.read_to_end(&mut some).unwrap_err();
        assert!(err.to_string().contains("grew past"), "{err}");
        assert_eq!(some, b"0123456789", "what fit was read");

        let matcher = build_matcher("needle", &literal("needle")).unwrap();
        let mut searcher = SearcherBuilder::new().line_number(true).build();
        let file = FileEntry {
            abs: PathBuf::from("/p/grown.txt"),
            rel: "grown.txt".into(),
        };
        let mut hits = Vec::new();
        let body = b"needle one\nfiller\nneedle two\n";
        let result = searcher.search_reader(
            &matcher,
            Capped::new(&body[..], 12),
            hit_sink(&file, &mut hits),
        );
        assert!(result.is_err(), "the growth is an error the caller sees");
        assert_eq!(
            hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            [1],
            "hits before the cap stand"
        );
    }

    /// Both entry points take the root and confine every read to it: a file
    /// the scan listed that now resolves outside (a directory swapped for a
    /// link) is skipped — and `search_report` says so.
    #[test]
    #[cfg(unix)]
    fn search_in_confines_reads_to_the_root() {
        let scratch = TempDir::new("search-confine");
        let dir = scratch.join("proj");
        let other = scratch.join("other");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("x.txt"), "needle\n").unwrap();
        // A directory swapped for a link after the scan listed `sub/x.txt`.
        std::fs::remove_dir(dir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&other, dir.join("sub")).unwrap();
        let files = Arc::new(vec![FileEntry {
            abs: dir.join("sub/x.txt"),
            rel: "sub/x.txt".into(),
        }]);
        let report = search_report(&dir, files.clone(), literal("needle"));
        assert!(report.hits.is_empty());
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert!(search_in(&dir, files, literal("needle")).hits.is_empty());
    }
}
