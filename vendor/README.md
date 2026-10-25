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
