//! In-file find (Cmd+F): match computation over the display lines.

use std::path::PathBuf;

use crate::highlight::HlLine;

/// A match as (line, start col, end col) in 0-based display columns.
pub type Match = (usize, usize, usize);

/// Identity of the document a match list describes: the file it came from plus
/// the identity of the lines matched (`Viewer::lines_generation`), which every
/// re-read and re-highlight moves on — so a re-read of the SAME path counts as
/// a different document, its matches computed over lines that are gone. The
/// source buffer's address served before, and the allocator reuses addresses:
/// a file closed and opened again, edited meanwhile, could land where it had
/// been, and keep matches for its old text.
pub type DocId = (PathBuf, u64);

#[derive(Debug, Default)]
pub struct FindState {
    pub open: bool,
    pub query: String,
    pub matches: Vec<Match>,
    pub current: usize,
    /// The document `matches` are expressed in. Every consumer paints or jumps
    /// to those raw (line, col) triples, so once the pane shows anything else
    /// they describe a file that is no longer on screen.
    pub doc: Option<DocId>,
    /// Match case: on, always; off, only once the query has a capital in it
    /// (smart case), as project search means it.
    pub case_sensitive: bool,
    /// Match whole words only.
    pub whole_word: bool,
    /// The query is a regular expression.
    pub regex: bool,
    /// The query, a regular expression, is not one: nothing matches, and
    /// the field says so.
    pub invalid: bool,
}

impl FindState {
    /// Recompute matches of the current query over `lines`, which are `doc`'s.
    /// Smart-case: any uppercase in the query makes it case-sensitive. Keeps
    /// `current` near the previous position when possible.
    pub fn recompute(&mut self, doc: DocId, lines: &[HlLine]) {
        let prev = self.matches.get(self.current).copied();
        (self.matches, self.invalid) = if self.whole_word || self.regex {
            match_with_options(self, lines)
        } else {
            (
                find_matches_cased(&self.query, lines, self.case_sensitive),
                false,
            )
        };
        self.doc = Some(doc);
        self.current = match prev {
            Some((line, _, _)) => self.matches.iter().position(|m| m.0 >= line).unwrap_or(0),
            None => 0,
        };
    }

    /// The current match, if any.
    pub fn current_match(&self) -> Option<Match> {
        self.matches.get(self.current).copied()
    }

    pub fn step(&mut self, delta: i32) -> Option<Match> {
        if self.matches.is_empty() {
            return None;
        }
        let n = self.matches.len() as i32;
        self.current = (self.current as i32 + delta).rem_euclid(n) as usize;
        self.current_match()
    }
}

/// The display text of one line (spans concatenated, tabs already expanded).
fn line_text(line: &HlLine) -> String {
    line.spans.iter().map(|(t, _)| t.as_str()).collect()
}

/// All matches of `query` across `lines`, in document order, its case
/// mattering once it has a capital in it.
#[cfg(test)]
pub fn find_matches(query: &str, lines: &[HlLine]) -> Vec<Match> {
    find_matches_cased(query, lines, false)
}

/// All matches of `query` across `lines`, in document order: its case
/// mattering when `case`, or once it has a capital in it.
fn find_matches_cased(query: &str, lines: &[HlLine], case: bool) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = case || query.chars().any(|c| c.is_uppercase());
    let needle: Vec<char> = if case_sensitive {
        query.chars().collect()
    } else {
        query.chars().flat_map(char::to_lowercase).collect()
    };

    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let text = line_text(line);
        // `col[k]` is the DISPLAY column that produced `hay[k]`. Lowercasing
        // is not one-to-one — `İ` (U+0130) folds to two chars — so an index
        // into the folded text is not a column, and reporting it directly put
        // the highlight and the jump on the wrong character for every match
        // after such a letter. With the query case-sensitive no folding
        // happens and the mapping is the identity, but it costs nothing to
        // build it the same way.
        let mut hay: Vec<char> = Vec::with_capacity(text.len());
        let mut col: Vec<usize> = Vec::with_capacity(text.len());
        for (c, ch) in text.chars().enumerate() {
            if case_sensitive {
                hay.push(ch);
                col.push(c);
            } else {
                for folded in ch.to_lowercase() {
                    hay.push(folded);
                    col.push(c);
                }
            }
        }
        // One past the column of the LAST folded char the match consumed.
        // Reading the column of the NEXT folded slot instead looks equivalent
        // and is not: when a match ends part-way through one source char's
        // fold, that next slot still carries the SAME column, so the range
        // came out empty — searching `i` over `İ` reported (0, 0, 0). Ending
        // on the consumed char's own column covers the whole source char,
        // which is the only span a display column can address.
        let end_col = |k: usize| col[k - 1] + 1;
        // Naive scan; queries and lines are short.
        if needle.len() > hay.len() {
            continue;
        }
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push((li, col[i], end_col(i + needle.len())));
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

/// The matches of a whole-word or regular-expression find: by project
/// search's own matcher, its byte ranges in each line's text turned into
/// display columns. And whether the query, a pattern, is not one.
fn match_with_options(find: &FindState, lines: &[HlLine]) -> (Vec<Match>, bool) {
    if find.query.is_empty() {
        return (Vec::new(), false);
    }
    let opts = clew_core::search::SearchOptions {
        query: find.query.clone(),
        case_sensitive: find.case_sensitive,
        whole_word: find.whole_word,
        regex: find.regex,
        ..Default::default()
    };
    let Some(matcher) = clew_core::search::PreviewMatcher::of_search(&opts) else {
        // Not empty, so not a pattern.
        return (Vec::new(), true);
    };
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let text = line_text(line);
        // The display column at each byte where a char starts.
        let column = |byte: usize| text[..byte].chars().count();
        for (start, end) in matcher.ranges(&text) {
            out.push((li, column(start), column(end)));
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::plain_lines;

    #[test]
    fn finds_all_and_is_smart_case() {
        let lines = plain_lines("foo Foo foo\nbar FOO\n");
        // lowercase query → case-insensitive: matches all foo/Foo/FOO.
        assert_eq!(find_matches("foo", &lines).len(), 4);
        // uppercase present → case-sensitive.
        let m = find_matches("Foo", &lines);
        assert_eq!(m, vec![(0, 4, 7)]);
    }

    #[test]
    fn step_wraps() {
        let mut f = FindState {
            query: "foo".into(),
            ..Default::default()
        };
        f.recompute(("a.rs".into(), 1), &plain_lines("foo\nfoo\n"));
        assert_eq!(f.matches.len(), 2);
        assert_eq!(f.step(1), Some((1, 0, 3)));
        assert_eq!(f.step(1), Some((0, 0, 3))); // wraps
        assert_eq!(f.step(-1), Some((1, 0, 3)));
    }

    /// Lowercasing is not one-to-one: `\u{130}` folds to two chars, so an
    /// index into the folded text is not a display column. Reporting it
    /// directly shifted the highlight (and the jump) one column right for
    /// everything after such a letter.
    #[test]
    fn columns_are_display_columns_after_a_multi_char_fold() {
        // 4 display columns: \u{130}, x, y, z.
        let lines = plain_lines("\u{130}xyz\n");
        assert_eq!(find_matches("x", &lines), vec![(0, 1, 2)]);
        assert_eq!(find_matches("z", &lines), vec![(0, 3, 4)]);
        // The folded letter itself still matches, at its own column.
        assert_eq!(find_matches("\u{130}", &lines), vec![(0, 0, 1)]);
    }

    /// A match ending part-way through one source char's fold must still span
    /// that char. `\u{130}` folds to `i` + U+0307, so the query `i` consumes
    /// only the first half; reporting the next slot's column made the range
    /// empty, which inflated the match count and put the highlight nowhere.
    #[test]
    fn a_match_ending_inside_a_fold_is_never_empty() {
        let lines = plain_lines("\u{130}xyz\nhi\u{130}\n");
        let m = find_matches("i", &lines);
        assert!(m.iter().all(|(_, c0, c1)| c1 > c0), "empty range in {m:?}");
        // The fold's first half is at the letter's own column, spanning it.
        assert_eq!(m[0], (0, 0, 1));
        // Line 2: the real `i`, then the folded letter, each one column wide.
        assert_eq!(m[1..], [(1, 1, 2), (1, 2, 3)]);
    }

    /// The same root cause under-covered a match that merely ENDS on a fold:
    /// `ai` over `aİ` used to highlight only the `a`.
    #[test]
    fn a_match_ending_on_a_fold_covers_the_whole_char() {
        assert_eq!(
            find_matches("ai", &plain_lines("a\u{130}z\n")),
            vec![(0, 0, 2)]
        );
    }

    /// L14: Match case, Whole word and Regular expression, as project
    /// search has them; a pattern that is not one matches nothing, and says
    /// so.
    #[test]
    fn the_options_narrow_what_matches() {
        let lines = plain_lines("foo Foo food\nlet x = f(a1 + b22);\n");
        let mut find = FindState {
            query: "foo".into(),
            ..Default::default()
        };
        let found = |find: &mut FindState| {
            find.recompute(("a.rs".into(), 1), &lines);
            find.matches.clone()
        };
        assert_eq!(found(&mut find), [(0, 0, 3), (0, 4, 7), (0, 8, 11)]);
        find.case_sensitive = true;
        assert_eq!(found(&mut find), [(0, 0, 3), (0, 8, 11)]);
        find.whole_word = true;
        assert_eq!(found(&mut find), [(0, 0, 3)]);
        find.case_sensitive = false;
        assert_eq!(found(&mut find), [(0, 0, 3), (0, 4, 7)]);
        find.whole_word = false;
        find.regex = true;
        find.query = r"[a-z]\d+".into();
        assert_eq!(found(&mut find), [(1, 10, 12), (1, 15, 18)]);
        assert!(!find.invalid);
        find.query = "(".into();
        assert!(found(&mut find).is_empty());
        assert!(find.invalid, "a pattern that is not one says so");
        find.regex = false;
        assert!(!found(&mut find).is_empty(), "as text, ( is found");
        assert!(!find.invalid);
    }

    /// Columns after a character of more than one byte are display
    /// columns, as the plain find's are.
    #[test]
    fn option_matches_are_in_display_columns() {
        let lines = plain_lines("é foo\n");
        let find = FindState {
            query: "foo".into(),
            whole_word: true,
            ..Default::default()
        };
        assert_eq!(match_with_options(&find, &lines).0, [(0, 2, 5)]);
    }

    #[test]
    fn empty_query_no_matches() {
        assert!(find_matches("", &plain_lines("abc")).is_empty());
    }
}
