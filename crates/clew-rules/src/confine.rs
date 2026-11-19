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

use crate::text::{starts_with, trim_bounds};

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

/// Whether a resolved `path` is the resolved `root` itself or lies under
/// it: the root's bytes, then a `/` and more (or, after a root that ends in
/// `/`, such as `/` itself, anything more). `confine` asks this of the
/// canonical path and the canonical root, whose pieces the operating system
/// has resolved: no `..`, no `.`, no link left in either.
pub fn inside(root: &[u8], path: &[u8]) -> bool {
    if !starts_with(path, root) {
        return false;
    }
    if path.len() == root.len() {
        return true;
    }
    if !root.is_empty() && root[root.len() - 1] == b'/' {
        return true;
    }
    path[root.len()] == b'/'
}

#[cfg(test)]
mod tests {
    use super::{Lexical, check_lexical, inside, names_a_folder};

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

    #[test]
    fn inside_is_the_root_or_under_it() {
        for (root, path) in [
            ("/p", "/p"),
            ("/p", "/p/a"),
            ("/p", "/p/a/b"),
            ("/", "/"),
            ("/", "/etc"),
            ("/p/", "/p/a"),
        ] {
            assert!(inside(root.as_bytes(), path.as_bytes()), "{root} {path}");
        }
        for (root, path) in [
            ("/p", "/pq"),
            ("/p", "/"),
            ("/p", "/q/p"),
            ("/p/a", "/p"),
            ("/p", ""),
            ("/p", "/P"),
        ] {
            assert!(!inside(root.as_bytes(), path.as_bytes()), "{root} {path}");
        }
    }

    /// On canonical paths, what `inside` says is what `Path::starts_with`
    /// says: `confine` asks both, and neither may let the other's refusal
    /// through.
    #[test]
    fn inside_agrees_with_path_starts_with_on_canonical_paths() {
        let pieces = ["a", "ab", "b", "a b", "é"];
        let mut paths = vec!["/".to_string()];
        for x in pieces {
            paths.push(format!("/{x}"));
            for y in pieces {
                paths.push(format!("/{x}/{y}"));
                for z in pieces {
                    paths.push(format!("/{x}/{y}/{z}"));
                }
            }
        }
        for root in &paths {
            for path in &paths {
                assert_eq!(
                    inside(root.as_bytes(), path.as_bytes()),
                    std::path::Path::new(path).starts_with(root),
                    "{root} {path}"
                );
            }
        }
    }
}
