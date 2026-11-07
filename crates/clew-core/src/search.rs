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
//! kept — and reported), a line too long to buffer, an I/O error. Two kinds
//! of file are passed over without a word: one deleted since the scan (there
//! is nothing left to search), and a binary one, as ripgrep passes it over —
//! a NUL byte ends the search of a file at the block of it being read
//! (tens of KiB), so the hits before it in that block are passed over too:
//! all of a file of ordinary size — and it is not reported, since every
//! image and archive in the tree would be.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::{Glob, GlobSet, GlobSetBuilder};
use grep_matcher::Matcher as _;
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

    let mut searcher = line_searcher()
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
            let sink = hit_sink(file, &matcher, &mut report.hits);
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
/// through; whole-word asks the matcher for word matches, ripgrep's `-w`: a
/// non-word character or the line's edge on each side. Wrapping the pattern
/// in `\b` instead found nothing for a query that starts or ends with a
/// non-word character (`-2`, `@Override`, `foo()`), since `\b` needs a word
/// character on one side of it.
/// The searcher a [`build_matcher`] matcher searches with: lines ending in
/// `\n` or `\r\n` alike, as the matcher's anchors take them.
fn line_searcher() -> SearcherBuilder {
    let mut builder = SearcherBuilder::new();
    builder.line_terminator(grep_matcher::LineTerminator::crlf());
    builder
}

fn build_matcher(query: &str, opts: &SearchOptions) -> Result<RegexMatcher, String> {
    let pattern = if opts.regex {
        query.to_string()
    } else {
        escape_regex(query)
    };
    let mut builder = RegexMatcherBuilder::new();
    // `$` (and `^`) at a line's end whatever ends it: in a file with CRLF
    // line endings `foo$` matched nothing, silently. The searcher splits
    // lines the same way ([`line_searcher`]), as ripgrep's `--crlf` does.
    builder.crlf(true);
    builder.word(opts.whole_word);
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
    matcher: &'a RegexMatcher,
    hits: &'a mut Vec<SearchHit>,
) -> Lossy<impl FnMut(u64, &str) -> Result<bool, std::io::Error> + 'a> {
    Lossy(move |line, text: &str| {
        let at = matcher
            .find(text.as_bytes())
            .ok()
            .flatten()
            .map(|m| m.start());
        hits.push(SearchHit {
            abs: file.abs.clone(),
            rel: file.rel.clone(),
            line: line as usize,
            preview: preview_around(text, at),
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
    let sink = hit_sink(file, matcher, hits);
    searcher
        .search_slice(matcher, notebook.projection.as_bytes(), sink)
        .map_err(|e| e.to_string())
}

/// Compile a comma/whitespace-separated glob spec into a set, or `None` when
/// the spec is empty. `*` matches across path separators, so `*.rs` matches
/// `src/main.rs` the way an editor's file filter would.
///
/// As an editor's filter (and ripgrep's `-g`) reads them, a pattern names
/// what is under a folder it matches too, and a bare name — `tests`,
/// `main.rs` — names a file or folder anywhere. Matched against each whole
/// relative path alone, `tests` excluded nothing but a file of that name at
/// the top, and `main.rs` found no `src/main.rs`.
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
        let pat = pat.strip_prefix("./").unwrap_or(pat);
        let pat = pat.trim_end_matches('/');
        let mut forms = vec![pat.to_string(), format!("{pat}/**")];
        if !pat.contains('/') {
            forms.push(format!("**/{pat}"));
            forms.push(format!("**/{pat}/**"));
        }
        for form in forms {
            builder.add(Glob::new(&form).map_err(|e| e.to_string())?);
        }
    }
    builder.build().map(Some).map_err(|e| e.to_string())
}

/// Characters of a line a preview keeps before its match: enough to read
/// what leads to it, few enough that the match shows in a narrow sidebar.
const LEAD_CHARS: usize = 16;

/// The preview of a line `text` whose match starts at byte `at`: the line
/// without its indentation and, when the match starts far into it, from a
/// little before the match on, an ellipsis for what is left out — shown
/// from its start, a match past the sidebar's edge was not to be seen.
/// Cut at [`MAX_PREVIEW_CHARS`].
pub fn preview_around(text: &str, at: Option<usize>) -> String {
    let (line, indent, start) = preview_start(text, at);
    // Drawn as the code view draws the line (`highlight::line_in_order`): a
    // preview holding a direction control read in another order than the
    // line does, and one holding a `\r` split where the line does not.
    let shown = crate::highlight::line_in_order(&line[start..]).replace('\t', "    ");
    let mut out = String::new();
    if start > indent {
        out.push('…');
    }
    if shown.chars().count() <= MAX_PREVIEW_CHARS {
        out.push_str(&shown);
        return out;
    }
    out.extend(shown.chars().take(MAX_PREVIEW_CHARS));
    out.push('…');
    out
}

/// The line [`preview_around`] previews, the width of its indentation, and
/// where in it the preview starts.
fn preview_start(text: &str, at: Option<usize>) -> (&str, usize, usize) {
    let line = text.trim_end_matches(['\n', '\r']);
    let indent = line.len() - line.trim_start().len();
    let start = match at {
        Some(at) if at > indent && at <= line.len() && line.is_char_boundary(at) => {
            let before: Vec<usize> = line[indent..at].char_indices().map(|(i, _)| i).collect();
            match before.len().checked_sub(LEAD_CHARS) {
                Some(cut) if cut > 0 => indent + before[cut],
                _ => indent,
            }
        }
        _ => indent,
    };
    (line, indent, start)
}

/// Where `ranges` of `text` (byte ranges, as a matcher found them in the
/// whole line) stand in [`preview_around`]`(text, at)`: each cut to what the
/// preview shows of it, those it shows none of left out. Matched in the line
/// itself, a mark is where the search matched — a pattern anchored at the
/// line's start or matching its tabs, a match running past the preview's
/// end, a whole word the preview's `…` would cut out of a longer one —
/// where matched in the preview it was not. `None` when the preview does
/// not keep the line's characters in their places (a direction control or
/// a `\r` in what it shows).
pub fn preview_marks(
    text: &str,
    at: Option<usize>,
    ranges: &[(usize, usize)],
) -> Option<Vec<(usize, usize)>> {
    let (line, indent, start) = preview_start(text, at);
    let rest = &line[start..];
    if rest
        .chars()
        .any(|c| c == '\r' || crate::highlight::rearranges_text(c))
    {
        return None;
    }
    // Each shown character's place in the line and in the preview, then
    // where the preview's text ends in both.
    let mut places: Vec<(usize, usize)> = Vec::new();
    let mut shown = if start > indent { '…'.len_utf8() } else { 0 };
    let mut count = 0;
    let mut end = line.len();
    for (i, c) in rest.char_indices() {
        let width = if c == '\t' { 4 } else { 1 };
        if count + width > MAX_PREVIEW_CHARS {
            end = start + i;
            break;
        }
        places.push((start + i, shown));
        shown += if c == '\t' { 4 } else { c.len_utf8() };
        count += width;
    }
    places.push((end, shown));
    let place = |at: usize| match places.binary_search_by_key(&at, |&(line, _)| line) {
        Ok(i) => places[i].1,
        // Inside a character: where it starts.
        Err(i) => places[i.saturating_sub(1)].1,
    };
    Some(
        ranges
            .iter()
            .filter(|&&(from, to)| to > start && from < end)
            .map(|&(from, to)| (place(from.max(start)), place(to.min(end))))
            .filter(|(from, to)| from < to)
            .collect(),
    )
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

/// Where a search's query, or a symbol's name, stands in the text of a
/// result's preview: what the results list marks in each preview, matched as
/// the search matched its line.
#[derive(Debug, Clone)]
pub struct PreviewMatcher {
    matcher: RegexMatcher,
    /// Matching whole words: what a preview's `…` stands beside may be the
    /// rest of one.
    whole_word: bool,
}

impl PreviewMatcher {
    /// The matcher of a search as `opts` asks for it; `None` for an empty
    /// query or one that is not a valid pattern.
    pub fn of_search(opts: &SearchOptions) -> Option<Self> {
        let query = opts.query.trim();
        if query.is_empty() {
            return None;
        }
        build_matcher(query, opts).ok().map(|matcher| Self {
            matcher,
            whole_word: opts.whole_word,
        })
    }

    /// The matcher of a symbol's own name: the name as a whole word, in its
    /// case — as a list of the symbol's references reads it.
    pub fn of_name(name: &str) -> Option<Self> {
        let opts = SearchOptions {
            query: name.to_string(),
            case_sensitive: true,
            whole_word: true,
            ..SearchOptions::default()
        };
        Self::of_search(&opts)
    }

    /// The byte ranges of each match in `text`, in order. An empty match (a
    /// pattern that can match nothing) marks nothing.
    pub fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let _ = self.matcher.find_iter(text.as_bytes(), |m| {
            if m.end() > m.start() {
                out.push((m.start(), m.end()));
            }
            out.len() < MAX_PREVIEW_MATCHES
        });
        out
    }

    /// [`Self::ranges`] in a preview whose line cannot be had to match in
    /// ([`preview_marks`]): a whole word standing beside the `…` the cut
    /// put in may be the end or the start of a longer one, and is not marked.
    pub fn ranges_in_preview(&self, preview: &str) -> Vec<(usize, usize)> {
        let cut = '…'.len_utf8();
        let led = preview.starts_with('…');
        let trails = preview.ends_with('…');
        self.ranges(preview)
            .into_iter()
            .filter(|&(from, to)| {
                !self.whole_word || !((led && from == cut) || (trails && to == preview.len() - cut))
            })
            .collect()
    }
}

/// Most matches a preview marks: enough for any line that fits the sidebar.
const MAX_PREVIEW_MATCHES: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// A preview marks its matches as the search found its line: in the
    /// search's case and word rules, a pattern read literally unless it is a
    /// regex; a reference list marks its symbol's name as a whole word.
    #[test]
    fn a_preview_marks_its_matches() {
        let search = |query: &str, f: fn(&mut SearchOptions)| {
            let mut opts = SearchOptions {
                query: query.into(),
                ..SearchOptions::default()
            };
            f(&mut opts);
            PreviewMatcher::of_search(&opts)
        };
        let line = "return self.price * self.quantity  # Quantity";
        // Smart case: a lowercase query matches either case.
        let m = search("quantity", |_| {}).unwrap();
        assert_eq!(m.ranges(line), [(25, 33), (37, 45)]);
        let m = search("quantity", |o| o.case_sensitive = true).unwrap();
        assert_eq!(m.ranges(line), [(25, 33)]);
        // Read literally: `.` is a dot.
        let m = search("self.p", |_| {}).unwrap();
        assert_eq!(m.ranges(line), [(7, 13)]);
        let m = search(r"self\.\w+", |o| o.regex = true).unwrap();
        assert_eq!(m.ranges(line), [(7, 17), (20, 33)]);
        let m = search("price", |o| o.whole_word = true).unwrap();
        assert_eq!(m.ranges("prices = price"), [(9, 14)]);
        assert!(search("  ", |_| {}).is_none());
        assert!(search("(", |o| o.regex = true).is_none());
        // A symbol's name: whole word, its own case.
        let m = PreviewMatcher::of_name("total").unwrap();
        assert_eq!(m.ranges("subtotal = total() + Total"), [(11, 16)]);
        // An empty match marks nothing.
        let m = search("x*", |o| o.regex = true).unwrap();
        assert!(m.ranges("abc").is_empty());
    }

    /// What a search matched in a line is marked where it stands in the
    /// line's preview: matched in the preview instead, a pattern anchored at
    /// the line's start or matching its tabs marked nothing, a match running
    /// past the preview's end nothing, and a whole word the preview's `…`
    /// cut out of a longer one was marked as though it stood alone.
    #[test]
    fn marks_found_in_the_line_land_in_its_preview() {
        let marks = |line: &str, matcher: &PreviewMatcher| {
            let ranges = matcher.ranges(line);
            let at = ranges.first().map(|r| r.0);
            let preview = preview_around(line, at);
            let marked: Vec<String> = preview_marks(line, at, &ranges)
                .unwrap()
                .into_iter()
                .map(|(from, to)| preview[from..to].to_string())
                .collect();
            (preview, marked)
        };
        let regex = |q: &str| {
            PreviewMatcher::of_search(&SearchOptions {
                query: q.into(),
                regex: true,
                ..SearchOptions::default()
            })
            .unwrap()
        };
        // Anchored at the line's start, past its indentation.
        let (preview, marked) = marks("\t\treturn total", &regex(r"^\s+return"));
        assert_eq!(preview, "return total");
        assert_eq!(marked, ["return"]);
        // A tab, spaced out in the preview.
        let (_, marked) = marks("a\tfoo", &regex(r"\tfoo"));
        assert_eq!(marked, ["    foo"]);
        // Running past the preview's end: what it shows of it.
        let long = "x".repeat(250);
        let (_, marked) = marks(&long, &regex("x+"));
        assert_eq!(marked, ["x".repeat(MAX_PREVIEW_CHARS)]);
        // A whole word: `xfoo-…` is not one, though cut to `…foo-…` it reads so.
        let words = PreviewMatcher::of_search(&SearchOptions {
            query: "foo".into(),
            whole_word: true,
            ..SearchOptions::default()
        })
        .unwrap();
        let line = format!("{}xfoo-234567890ab foo", "z".repeat(20));
        let (preview, marked) = marks(&line, &words);
        assert_eq!(preview, "…foo-234567890ab foo");
        assert_eq!(
            words.ranges(&preview).len(),
            2,
            "the preview alone reads two"
        );
        assert_eq!(marked, ["foo"]);
        assert_eq!(
            preview_marks(&line, Some(line.len() - 3), &words.ranges(&line)),
            Some(vec![(19, 22)])
        );
        // A direction control: the preview is not the line in its places.
        assert_eq!(preview_marks("a\u{202E}b", None, &[(0, 1)]), None);
        // With no line to match in, a whole word beside the cut's `…` may be
        // part of a longer one: not marked.
        assert_eq!(words.ranges_in_preview(&preview), [(19, 22)]);
        let any = regex("foo");
        assert_eq!(any.ranges_in_preview(&preview).len(), 2);
    }

    #[test]
    fn escapes_regex_metacharacters() {
        let escaped = escape_regex("a.b(c)*+?");
        assert_eq!(escaped, r"a\.b\(c\)\*\+\?");
    }

    /// A hit's preview drops the line's indentation and, when the match
    /// starts far into it, starts a little before the match: shown from its
    /// start, a match past a narrow sidebar's edge was not to be seen.
    #[test]
    fn a_preview_shows_its_match() {
        let line = "        return apply_discount(self.subtotal(), discount)";
        let at = line.find("apply_discount");
        assert_eq!(
            preview_around(line, at),
            "return apply_discount(self.subtotal(), discount)"
        );
        let far = "    \"\"\"A shopping cart: items, totals and discounts.\"\"\"";
        let at = far.find("discounts");
        let said = preview_around(far, at);
        assert!(said.starts_with('…'), "{said}");
        let lead = said.chars().position(|c| c == 'd').unwrap();
        assert!(lead <= LEAD_CHARS + 1, "{said}");
        assert!(said.contains("totals and discounts"), "{said}");
        // Found by a search, the same.
        let dir = TempDir::new("search-preview");
        std::fs::write(dir.join("cart.py"), format!("{far}\n")).unwrap();
        let files = Arc::new(vec![FileEntry {
            abs: dir.join("cart.py"),
            rel: "cart.py".into(),
        }]);
        let out = search_in(&dir, files, literal("discounts"));
        assert_eq!(out.hits[0].preview, said);
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

    /// A regex anchored at a line's end matches in a file whose lines end
    /// in CRLF as in one whose end in LF: `$` matched nothing there,
    /// silently.
    #[test]
    fn a_line_end_anchor_matches_crlf_lines() {
        let dir = TempDir::new("search-crlf");
        std::fs::write(dir.join("w.txt"), "foo\r\nbar foo\r\nfoo bar\r\n").unwrap();
        let files = Arc::new(vec![FileEntry {
            abs: dir.join("w.txt"),
            rel: "w.txt".into(),
        }]);
        for (query, lines) in [
            ("foo$", vec![1, 2]),
            ("^foo$", vec![1]),
            ("^foo", vec![1, 3]),
        ] {
            let out = search_in(
                &dir,
                files.clone(),
                SearchOptions {
                    query: query.into(),
                    regex: true,
                    ..Default::default()
                },
            );
            assert!(out.error.is_none(), "{query}: {:?}", out.error);
            let found: Vec<usize> = out.hits.iter().map(|h| h.line).collect();
            assert_eq!(found, lines, "{query}");
        }
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
    fn whole_word_finds_a_query_edged_by_non_word_characters() {
        // `\b-2\b` matches nothing in `x = -2`: `\b` needs a word character on
        // one side. Whole-word asks for a non-word character or the line edge.
        let dir = TempDir::new("search-whole-word-edges");
        std::fs::write(dir.join("a.java"), "    @Override\nx = -2;\ny = 3-2;\n").unwrap();
        let files = Arc::new(vec![FileEntry {
            abs: dir.join("a.java"),
            rel: "a.java".into(),
        }]);
        let lines = |query: &str| {
            let mut opts = literal(query);
            opts.whole_word = true;
            let out = search_in(&dir, files.clone(), opts);
            out.hits.iter().map(|h| h.line).collect::<Vec<_>>()
        };
        assert_eq!(lines("@Override"), [1]);
        // `3-2` has a word character before the `-`, so it is not the word.
        assert_eq!(lines("-2"), [2]);
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

    /// A binary file — one holding a NUL byte — is passed over from the
    /// block the NUL is read in, the hits before it there included, and
    /// not reported; hits in a block read before it are kept.
    #[test]
    fn a_nul_byte_passes_over_its_block_silently() {
        let dir = crate::testutil::TempDir::new("search-nul");
        let root = dir.path().canonicalize().unwrap();
        let mut big = b"needle first\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', 1 << 20));
        big.extend(b"\nneedle late\n\0needle after\n");
        let files: Vec<FileEntry> = [
            ("small.bin", b"needle\n\0needle\n".to_vec()),
            ("big.bin", big),
        ]
        .into_iter()
        .map(|(rel, body)| {
            let abs = root.join(rel);
            std::fs::write(&abs, body).unwrap();
            FileEntry {
                rel: rel.into(),
                abs,
            }
        })
        .collect();
        let report = search_report(&root, Arc::new(files), literal("needle"));
        let hits: Vec<_> = report
            .hits
            .iter()
            .map(|h| (h.rel.as_str(), h.line))
            .collect();
        assert_eq!(hits, [("big.bin", 1)]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }

    /// A pattern names what is under a folder it matches, and a bare name
    /// names a file or folder anywhere, as an editor's filter reads them.
    #[test]
    fn a_glob_names_folders_and_bare_names_anywhere() {
        let dir = crate::testutil::TempDir::new("search-glob-forms");
        let root = dir.path().canonicalize().unwrap();
        let rels = [
            "src/main.rs",
            "src/app/view.rs",
            "tests/it.rs",
            "crates/x/tests/unit.rs",
            "main.rs.bak",
        ];
        let files: Vec<FileEntry> = rels
            .iter()
            .map(|rel| {
                let abs = root.join(rel);
                std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                std::fs::write(&abs, "needle\n").unwrap();
                FileEntry {
                    rel: (*rel).into(),
                    abs,
                }
            })
            .collect();
        let files = Arc::new(files);
        let found = |include: &str, exclude: &str| {
            let mut opts = literal("needle");
            opts.include = include.into();
            opts.exclude = exclude.into();
            let mut rels: Vec<String> = search_in(&root, files.clone(), opts)
                .hits
                .into_iter()
                .map(|h| h.rel)
                .collect();
            rels.sort();
            rels
        };
        assert_eq!(found("main.rs", ""), ["src/main.rs"]);
        assert_eq!(found("src/app", ""), ["src/app/view.rs"]);
        assert_eq!(found("./src/app/", ""), ["src/app/view.rs"]);
        assert_eq!(
            found("", "tests"),
            ["main.rs.bak", "src/app/view.rs", "src/main.rs"]
        );
        assert_eq!(
            found("*.rs", "src"),
            ["crates/x/tests/unit.rs", "tests/it.rs"]
        );
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
        let mut searcher = line_searcher().line_number(true).build();
        let file = FileEntry {
            abs: PathBuf::from("/p/grown.txt"),
            rel: "grown.txt".into(),
        };
        let mut hits = Vec::new();
        let body = b"needle one\nfiller\nneedle two\n";
        let result = searcher.search_reader(
            &matcher,
            Capped::new(&body[..], 12),
            hit_sink(&file, &matcher, &mut hits),
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
