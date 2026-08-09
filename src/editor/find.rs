//! In-file find (Cmd+F): match computation over the display lines.

use crate::highlight::HlLine;

/// A match as (line, start col, end col) in 0-based display columns.
pub type Match = (usize, usize, usize);

#[derive(Debug, Default)]
pub struct FindState {
    pub open: bool,
    pub query: String,
    pub matches: Vec<Match>,
    pub current: usize,
}

impl FindState {
    /// Recompute matches of the current query over `lines`. Smart-case: any
    /// uppercase in the query makes it case-sensitive. Keeps `current` near the
    /// previous position when possible.
    pub fn recompute(&mut self, lines: &[HlLine]) {
        let prev = self.matches.get(self.current).copied();
        self.matches = find_matches(&self.query, lines);
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

/// All matches of `query` across `lines`, in document order.
pub fn find_matches(query: &str, lines: &[HlLine]) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let case_sensitive = query.chars().any(|c| c.is_uppercase());
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
        f.recompute(&plain_lines("foo\nfoo\n"));
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

    #[test]
    fn empty_query_no_matches() {
        assert!(find_matches("", &plain_lines("abc")).is_empty());
    }
}
