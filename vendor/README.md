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

A third patch sets a line's text in its line box as a browser does
(`LayoutRunIter::next` in `src/buffer.rs`): the face's ascent and descent
each rounded to a whole pixel, the leading above them floored to one, and
the baseline of a line box that falls at a part of a pixel (in a paragraph
of 21.59375 px lines) rounded to a whole one, where cosmic-text centred
the exact ascent and descent — a part of a pixel
off from the design's text in every line, and in each line of a paragraph
by a different part. A line laid out the face's `normal` height
(`ts::Face::normal`) then has its baseline exactly its rounded ascent down.
With it, the place a glyph is drawn at is rounded to a device pixel rather
than truncated (`LayoutGlyph::physical` in `src/layout.rs`): iced adds the
line's baseline to it rounded, and text whose box falls at a part of a
pixel was drawn a device pixel above where a browser draws it.
cosmic-text centres a line the same way up to 0.19.0; drop these patches
with the others when iced moves to a release that sets a line as a browser
does.

A fourth patch breaks Korean lines between words (`ShapeLine::build` in
`src/shape.rs`), as Korean is set and as CSS's `word-break: keep-all` sets
it: UAX #14, which cosmic-text follows, allows a break before every Hangul
syllable, so a Korean label wrapped in the middle of a word. Break points
inside a run of Hangul and the letters and digits written against it are
dropped; Chinese and Japanese, written without spaces, still break between
characters. The i18n test `korean_lines_break_between_words` keeps it.
Upstream cosmic-text has no such option up to 0.19.0; keep this patch (or
an equivalent setting, if one arrives) when the others are dropped.

## iced_wgpu 0.14.0

iced 0.14's GPU renderer. On Windows it presents to a window's own swap
chain, which Windows composites opaque whatever its alpha, so a window
whose corners Clew leaves clear for the design's rounded ones showed them
black, and Clew left the rounding to Windows 11, at its own radius. wgpu
can present through a DirectComposition visual instead, which Windows
composites by its alpha, but only for an instance made to (DX12's
`Dx12SwapchainKind::DxgiFromVisual`), and iced makes its instance with
wgpu's defaults. The patch (`src/window/compositor.rs`) lets Clew ask,
before the renderer is made: `present_through_visual` has every compositor
requested after it make its instance for DX12 alone, presenting through a
visual, where its settings include DX12, and `visual_presentation_available`
says whether such a compositor gets the adapter and the device it asks for
(and `WGPU_BACKEND`, if set, names DX12): it asks for them as the
compositor will, through the same descriptor, for iced not to fall back to
drawing in software, which a window presenting through a visual does not
show. Such a compositor takes premultiplied alpha, which is
what iced draws and the only alpha a visual's swap chain takes, where iced
prefers the straight kind. `app::corners` (in Clew) decides it once at
start; the window is made for it by the patched `iced_winit` below. The
release ships no license file; its manifest declares MIT, and `LICENSE` is
iced's. When iced lets an application choose how wgpu presents, ask for it
there and delete this copy and the patch entry.

## iced_winit 0.14.1

iced 0.14's windowing. winit makes a transparent window on Windows with
the surface GDI draws to (a redirection surface) and asks Windows to show
it by its alpha; nothing draws to it, and Windows showed it white over the
pixels a window presenting through a DirectComposition visual (the patched
`iced_wgpu` above) leaves clear — read off the screen on CI's Windows,
where the same window made without one showed them clear. The patch
(`window_attributes` in `src/conversion.rs`) makes every transparent
window on Windows without one (winit's `with_no_redirection_bitmap`): it
shows only what is presented through a visual, and Clew makes a window
transparent there only when it presents so (`app::corners`). A second patch
(`typed_text`, same file) gives a key pressed with Ctrl (but not AltGr, which
Windows reports as Ctrl+Alt) or the logo key no text: winit gives the text
with every modifier applied, and on X11 Ctrl+= and Ctrl+0 came as "=" and
"0", which a focused text input typed while the chord zoomed. Its manifest
allows the `deprecated` lint, whose warnings (a method of `futures` the
release calls) were capped while it came from crates.io. The release
ships no license file; its manifest declares MIT, and `LICENSE` is iced's.
When iced lets an application ask for this, ask for it there and delete
this copy and the patch entry.

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
