//! The lexical half of path confinement: whether a project-relative path's
//! spelling alone could lead out of the project. `check_lexical` in
//! `crates/clew-core/src/confine.rs` calls this on the UTF-8 bytes of every
//! path that arrives from outside, before anything is resolved, and
//! `confine` there asks `names_a_folder` before it resolves one.
//!
//! That a path `check_lexical` accepts, resolved one piece at a time against
//! any root, never stands outside that root is proved in
//! `verification/Clew/Confine.lean`, and the Lean Aeneas makes of this source
//! is proved to be that model, for the UTF-8 bytes of any string.

use crate::text::trim_bounds;

/// What the lexical check says of a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lexical {
    /// Nothing in its spelling can lead out of the root.
    Inside,
    /// It is empty, or only white space: it names no file.
    Empty,
    /// It starts with `/`.
    Absolute,
    /// One of its pieces between `/`s is `..`.
    Traversal,
}

/// Whether the piece `rel[from..to]` is `..`.
fn is_parent(rel: &[u8], from: usize, to: usize) -> bool {
    if to - from != 2 {
        return false;
    }
    rel[from] == b'.' && rel[from + 1] == b'.'
}

/// Whether any piece of `rel` between its `/`s is `..`.
fn has_parent(rel: &[u8]) -> bool {
    let mut start = 0;
    let mut i = 0;
    while i < rel.len() {
        if rel[i] == b'/' {
            if is_parent(rel, start, i) {
                return true;
            }
            start = i + 1;
        }
        i += 1;
    }
    is_parent(rel, start, rel.len())
}

/// Whether a path's spelling alone could lead out of the root: empty (after
/// `str::trim`) first, then a leading `/`, then any `..` piece.
pub fn check_lexical(rel: &[u8]) -> Lexical {
    let (start, end) = trim_bounds(rel);
    if start == end {
        return Lexical::Empty;
    }
    // Not empty: it holds a character that is not white space.
    if rel[0] == b'/' {
        return Lexical::Absolute;
    }
    if has_parent(rel) {
        return Lexical::Traversal;
    }
    Lexical::Inside
}

/// Where the last piece of `rel` between its `/`s starts.
fn last_piece(rel: &[u8]) -> usize {
    let mut start = 0;
    let mut i = 0;
    while i < rel.len() {
        if rel[i] == b'/' {
            start = i + 1;
        }
        i += 1;
    }
    start
}

/// Whether `rel` names a folder by its spelling: its last piece is empty or
/// `.` (`src/`, `src/.`, `.`).
pub fn names_a_folder(rel: &[u8]) -> bool {
    let start = last_piece(rel);
    let len = rel.len() - start;
    if len == 0 {
        return true;
    }
    len == 1 && rel[start] == b'.'
}

#[cfg(test)]
mod tests {
    use super::{Lexical, check_lexical, names_a_folder};

    #[test]
    fn check_lexical_reads_pieces_between_slashes() {
        assert_eq!(check_lexical(b""), Lexical::Empty);
        assert_eq!(check_lexical(" \u{3000}\t".as_bytes()), Lexical::Empty);
        assert_eq!(check_lexical(b"/etc/passwd"), Lexical::Absolute);
        assert_eq!(check_lexical(b"/.."), Lexical::Absolute);
        assert_eq!(check_lexical(b".."), Lexical::Traversal);
        assert_eq!(check_lexical(b"a/../b"), Lexical::Traversal);
        assert_eq!(check_lexical(b"a/.."), Lexical::Traversal);
        assert_eq!(check_lexical(b"../a"), Lexical::Traversal);
        assert_eq!(check_lexical(b"a/...b/..c/c.."), Lexical::Inside);
        assert_eq!(check_lexical(b"a//./b/"), Lexical::Inside);
        assert_eq!(check_lexical(b" /x"), Lexical::Inside);
        assert_eq!(check_lexical("é/..é".as_bytes()), Lexical::Inside);
    }

    #[test]
    fn names_a_folder_reads_the_last_piece() {
        for yes in ["", ".", "src/", "src/.", "a/b/./", "/"] {
            assert!(names_a_folder(yes.as_bytes()), "{yes:?}");
        }
        for no in ["a", "..", "src/..", "src/.x", "src/x.", "./a"] {
            assert!(!names_a_folder(no.as_bytes()), "{no:?}");
        }
    }
}
