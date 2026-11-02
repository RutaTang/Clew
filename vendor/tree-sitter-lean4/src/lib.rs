//! Tree-sitter grammar for Lean 4.
//!
//! clew patch: the release exposed `language() -> tree_sitter::Language` of
//! `tree-sitter` 0.25. This copy exposes the grammar as a
//! [`LanguageFn`](tree_sitter_language::LanguageFn), as every other grammar
//! crate does, so it loads into any `tree-sitter` ≥ 0.23; and the repository's
//! highlight query, which the release does not ship.

use tree_sitter_language::LanguageFn;

extern "C" {
    fn tree_sitter_lean() -> *const ();
}

/// The tree-sitter [`LanguageFn`] for this grammar.
pub const LANGUAGE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_lean) };

/// The syntax highlighting query for this language.
pub const HIGHLIGHTS_QUERY: &str = include_str!("../queries/highlights.scm");
