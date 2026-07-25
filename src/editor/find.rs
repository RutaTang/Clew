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
        let hay: Vec<char> = if case_sensitive {
            text.chars().collect()
        } else {
            text.chars().flat_map(char::to_lowercase).collect()
        };
        // Naive scan; queries and lines are short.
        if needle.len() > hay.len() {
            continue;
        }
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push((li, i, i + needle.len()));
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

    #[test]
    fn empty_query_no_matches() {
        assert!(find_matches("", &plain_lines("abc")).is_empty());
    }
}
