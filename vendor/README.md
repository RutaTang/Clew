# Vendored crates

Third-party crates clew builds from this directory instead of crates.io,
through `[patch.crates-io]` in the root `Cargo.toml`. Each is the published
release, cut down to what a build needs (its `Cargo.toml` without the test and
bench targets, its `src/` and its license files), plus a fix of clew's. Each
fix is marked `clew patch` in the source.

## cosmic-text 0.15.0

The text shaper under iced 0.14. `ShapeLine::build` (`src/shape.rs`) asserted
that every bidi paragraph of a line has the first one's direction. A line
holds several paragraphs when it contains a paragraph separator other than CR
and LF (U+001C, U+001D, U+001E, U+0085, U+2029), and their directions differ
when, for example, a Hebrew or Arabic word follows one — so a file name, a line
of code, a commit message or a model's answer holding one panicked the UI
thread and closed every window. The patch analyses such a line again with the
first paragraph's direction for all of them. The same assert is still present
upstream (0.19.0); when iced moves to a release that no longer has it, delete
this copy and the patch entry.

A second patch sets the tracking text is shaped with where none is asked
for (`letter_spacing` in `src/shape.rs`): -0.004 em, the design's
`letter-spacing` (-0.048 px at the sizes Clew's text is read at). As a
browser does, the text of a control (a button, an input) is left in the
face's own spacing, and the design's titles are tracked -0.01 em. iced 0.14
has no way to ask for tracking, so Clew asks for these by a stretch its
faces do not come in — semi-expanded for a control's, semi-condensed for a
title's (`fonts::in_control`, `fonts::titled`) — and the patch matches such
text to the face of the normal width (`Attrs::matches` in `src/attrs.rs`).
When iced has a way to ask for tracking, ask for it there and drop this
patch.

## tree-sitter-lean4 0.3.0

The Lean 4 grammar. The release's binding depends on `tree-sitter = "0.25"`,
and `tree-sitter` declares `links = "tree-sitter"`, so it cannot share a build
with clew's 0.26: the crate could not be used at all. This copy exposes the
grammar as a `tree_sitter_language::LanguageFn` (`src/lib.rs`), as every other
grammar crate does, and its build script compiles the generated parser the
release ships without the release's fallback of running the `tree-sitter` CLI
to generate one, and — with MSVC — as UTF-8 (`/utf-8`), so that a node named
`λ` is spelled as the queries spell it on Windows too. It adds the highlight query of the grammar's repository
(`queries/highlights.scm`), which the release does not ship, cut to the node
types of this release and with clew's capture names. The release ships no
license file; its manifest declares MIT, and `LICENSE` is that of the grammar
it continues, Julian Berman's tree-sitter-lean. When a release depends on
`tree-sitter-language` and ships its queries, delete this copy and the patch
entry.
