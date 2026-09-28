//! Splits an LLM markdown explanation into renderable segments and defines the
//! cache keys for its math/mermaid → SVG renders.
//!
//! iced's markdown widget renders prose, headings, lists and code, but not math
//! or mermaid diagrams. So we split the source into [`Segment`]s: ordinary
//! markdown runs (handed to the markdown widget, which wraps and styles them)
//! interleaved with math and mermaid, which are rendered to SVG in-process
//! ([`crate::render`]: RaTeX for math, `mermaid-rs-renderer` for diagrams) and
//! shown inline via iced's `svg` widget. Each renderable is keyed by a content
//! hash so a given equation or diagram is generated at most once and cached on
//! disk.
//!
//! Math follows Pandoc's `tex_math_dollars` rules, so prose about money or
//! shell variables stays prose: `$…$` needs a non-space character just inside
//! both delimiters and a closing `$` not followed by a digit (`$5 and $10`,
//! `$PATH and $HOME` are text), `\$` is a literal dollar, and code — fenced
//! blocks (backticks or tildes) and inline code spans — is never scanned.

use std::path::{Path, PathBuf};

use crate::incremental::{Version, content_hash};

fn svg_dir(store: &Path) -> PathBuf {
    store.join("svg")
}

fn svg_path(store: &Path, key: Version) -> PathBuf {
    svg_dir(store).join(format!("{key}.svg"))
}

/// Load a previously generated raw SVG for `key`, if cached on disk.
pub fn load_raw(store: &Path, key: Version) -> Option<String> {
    clew_core::statefile::read(&svg_path(store, key))
}

/// Persist a raw SVG for `key` (best-effort; ignored on error).
pub fn store_raw(store: &Path, key: Version, raw: &str) {
    let _ = clew_core::statefile::write_atomic(&svg_path(store, key), raw.as_bytes());
}

/// Which renderer produced a unit's SVG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderKind {
    Math,
    Mermaid,
}

impl RenderKind {
    pub fn is_math(self) -> bool {
        self == RenderKind::Math
    }
}

/// The root-element attribute [`crate::render`] stamps on every SVG it
/// produces, naming the renderer: `data-clew-kind="math"` / `"mermaid"`.
pub const KIND_ATTR: &str = "data-clew-kind";

/// Which renderer produced raw SVG `svg`, read from the [`KIND_ATTR`] stamp
/// on its root element — so re-preparing a cached SVG (a theme switch) never
/// guesses from its contents. SVGs cached before the stamp existed fall back
/// to the old inference (RaTeX math paints `currentColor`).
pub fn svg_kind(svg: &str) -> RenderKind {
    let head = svg.find('>').map_or(svg, |gt| &svg[..gt]);
    if head.contains(&format!("{KIND_ATTR}=\"math\"")) {
        RenderKind::Math
    } else if head.contains(&format!("{KIND_ATTR}=\"mermaid\"")) {
        RenderKind::Mermaid
    } else if svg.contains("currentColor") {
        RenderKind::Math
    } else {
        RenderKind::Mermaid
    }
}

/// One inline piece of a text line that mixes prose with inline `$…$` math.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    /// Inline math (`$…$`); rendered as a small SVG sitting on the text baseline.
    Math(String),
}

/// A top-level piece of an explanation, rendered in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// A run of ordinary markdown with no math — rendered by the markdown widget.
    Markdown(String),
    /// A display equation (`$$…$$`) — its own centered SVG block.
    DisplayMath(String),
    /// A line mixing prose and inline `$…$` math — rendered as a horizontal row.
    InlineLine(Vec<Inline>),
    /// A mermaid diagram — rendered as an SVG block.
    Mermaid(String),
    /// A fenced code block — syntax-highlighted with clew's own tree-sitter
    /// pipeline (iced's markdown widget would render it as plain text).
    Code { lang: String, code: String },
}

/// A math/mermaid unit that needs an SVG render, with its stable cache key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderable {
    pub key: Version,
    /// `"math"` or `"mermaid"`.
    pub kind: &'static str,
    pub src: String,
    /// Display vs. inline, for math (ignored for mermaid).
    pub display: bool,
}

/// Cache key for a math render.
pub fn math_key(tex: &str, display: bool) -> Version {
    content_hash(format!("math:{display}:{tex}").as_bytes())
}

/// Cache key for a mermaid render.
pub fn mermaid_key(src: &str) -> Version {
    content_hash(format!("mermaid:{src}").as_bytes())
}

/// Every math/mermaid unit across `segments`, de-duplicated by key.
pub fn renderables(segments: &[Segment]) -> Vec<Renderable> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |r: Renderable| {
        if seen.insert(r.key) {
            out.push(r);
        }
    };
    for s in segments {
        match s {
            Segment::DisplayMath(tex) => push(Renderable {
                key: math_key(tex, true),
                kind: "math",
                src: tex.clone(),
                display: true,
            }),
            Segment::Mermaid(src) => push(Renderable {
                key: mermaid_key(src),
                kind: "mermaid",
                src: src.clone(),
                display: false,
            }),
            Segment::InlineLine(parts) => {
                for p in parts {
                    if let Inline::Math(tex) = p {
                        push(Renderable {
                            key: math_key(tex, false),
                            kind: "math",
                            src: tex.clone(),
                            display: false,
                        });
                    }
                }
            }
            Segment::Markdown(_) | Segment::Code { .. } => {}
        }
    }
    out
}

/// Turn `` `path:line` `` / `` `path` `` inline-code citations into markdown
/// links (``[`path:line`](<clew:rel:line>)``) so they become clickable jumps.
/// `resolve` maps a candidate path to a real project rel (exact rel, or a
/// unique basename like `theme.rs`) — only those are linkified, so type names
/// like `Vec<String>` stay code chips. Fenced blocks (backticks or tildes) are
/// left untouched.
///
/// The code span stays INSIDE the link text: as plain link text,
/// `pkg/__init__.py` rendered as a bold "init.py" (`__…__` is emphasis) and
/// lost its code style. The destination is written in angle brackets, which
/// CommonMark allows to contain spaces, so a path with a space stays one link
/// and reaches the click handler exactly as the project names it.
pub fn linkify_citations(md: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence: Option<(u8, usize)> = None;
    for (i, line) in md.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match fence {
            Some((ch, n)) => {
                if fence_closes(line, ch, n) {
                    fence = None;
                }
                out.push_str(line);
                continue;
            }
            None => {
                if let Some((ch, n, _)) = fence_open(line) {
                    fence = Some((ch, n));
                    out.push_str(line);
                    continue;
                }
            }
        }
        let mut last = 0;
        for (start, end) in code_spans(line) {
            let run = line[start..].bytes().take_while(|&b| b == b'`').count();
            let code = &line[start + run..end - run];
            // Already a link's text (`[`x`](…)`): leave it alone.
            let in_link = line[..start].ends_with('[') && line[end..].starts_with("](");
            if in_link {
                continue;
            }
            if let Some(target) = citation_target(code, &resolve) {
                out.push_str(&line[last..start]);
                out.push('[');
                out.push_str(&line[start..end]);
                out.push_str("](<clew:");
                out.push_str(&target);
                out.push_str(">)");
                last = end;
            }
        }
        out.push_str(&line[last..]);
    }
    if md.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// If an inline-code span reads as a citation of a real project file, return
/// the `rel[:line]` jump target (a `path:12-34` range collapses to its start).
fn citation_target(code: &str, resolve: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let code = code.trim();
    // Spaces are fine (the resolver only answers for real project paths);
    // line breaks and angle brackets cannot appear in the link destination.
    if code.is_empty() || code.contains(['\n', '\r', '<', '>']) {
        return None;
    }
    // Split a trailing :line or :line-line; the remainder must be a project file.
    let (path, line) = match code.rsplit_once(':') {
        Some((p, nums)) => {
            let start = nums.split('-').next().unwrap_or("");
            match start.parse::<usize>() {
                Ok(n) if !nums.split('-').any(|s| s.parse::<usize>().is_err()) => (p, Some(n)),
                _ => (code, None),
            }
        }
        None => (code, None),
    };
    let rel = resolve(path)?;
    if rel.contains(['\n', '\r', '<', '>']) {
        return None;
    }
    Some(match line {
        Some(n) => format!("{rel}:{n}"),
        None => rel,
    })
}

/// A fenced-code opening line: the fence byte (`` ` `` or `~`), its run
/// length, and the info string. CommonMark: three or more backticks or tildes
/// (indentation is tolerated — model output nests fences in list items); a
/// backtick fence's info string may not itself contain a backtick.
fn fence_open(line: &str) -> Option<(u8, usize, &str)> {
    let t = line.trim_start();
    let ch = *t.as_bytes().first()?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let n = t.bytes().take_while(|&b| b == ch).count();
    if n < 3 {
        return None;
    }
    let info = t[n..].trim();
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, n, info))
}

/// Whether `line` closes a fence opened with `n` × `ch`: the same character,
/// at least as many of them, and nothing else on the line.
fn fence_closes(line: &str, ch: u8, n: usize) -> bool {
    let t = line.trim();
    let m = t.bytes().take_while(|&b| b == ch).count();
    m >= n && m == t.len()
}

#[cfg(test)]
thread_local! {
    /// Work done by the math scan's searches on this thread: one step per
    /// backtick run, closing `$` or byte of a candidate span a search
    /// examines. The tests bound it by the line's length — every one of those
    /// searches once rescanned the line from each candidate, quadratic on the
    /// UI thread over text a model wrote — and a count, unlike a timing,
    /// cannot be blurred by a slow machine. Per thread, so tests running in
    /// parallel keep apart.
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Bytes of text the math scan built a per-byte link table for
    /// ([`Literals::link_at`], sixteen bytes per byte) on this thread.
    static LINK_TABLE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one step of the math scan's work (`STEPS`, in tests only).
#[inline]
fn step() {
    #[cfg(test)]
    STEPS.with(|steps| steps.set(steps.get() + 1));
}

/// Byte ranges `[start, end)` of the inline code spans in `line`, delimiters
/// included. CommonMark: a run of N backticks opens a span that the next run
/// of exactly N backticks closes; a run without such a partner is literal
/// text, and so is a backslash-escaped backtick.
///
/// Linear in the line: each run's partner is looked up in a table filled in
/// one pass from the right. Searching the rest of the line from every run
/// was quadratic — a line of backslash-escaped backtick pairs, none of which
/// has a partner, searched to its end from each one.
fn code_spans(line: &str) -> Vec<(usize, usize)> {
    let b = line.as_bytes();
    // The line's maximal backtick runs, as (start, length).
    let mut runs = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let n = b[i..].iter().take_while(|&&c| c == b'`').count();
        if n > 0 {
            runs.push((i, n));
        }
        i += n.max(1);
    }
    // What a run opens: all of it — or, when a backslash escapes its first
    // backtick, the rest (nothing, for a lone one).
    let opens = |(start, n): (usize, usize)| {
        if start > 0 && b[start - 1] == b'\\' {
            (start + 1, n - 1)
        } else {
            (start, n)
        }
    };
    // `partner[r]`: the first later run exactly as long as what run `r`
    // opens, which closes it. `nearest[n]` is the nearest run of length `n`
    // right of the one being filled in; no run is empty, so an escaped lone
    // backtick gets none.
    let longest = runs.iter().map(|&(_, n)| n).max().unwrap_or(0);
    let mut nearest = vec![None; longest + 1];
    let mut partner = vec![None; runs.len()];
    for (r, &run) in runs.iter().enumerate().rev() {
        step();
        partner[r] = nearest[opens(run).1];
        nearest[run.1] = Some(r);
    }
    let mut out = Vec::new();
    let mut r = 0;
    while r < runs.len() {
        step();
        match partner[r] {
            Some(p) => {
                let (at, n) = runs[p];
                out.push((opens(runs[r]).0, at + n));
                // The runs in between are inside the span.
                r = p + 1;
            }
            None => r += 1,
        }
    }
    out
}

/// Byte mask of `text` marking what is literal to the math scan — a `$` there
/// is not math: inline code spans, and link destinations — a cited
/// `src/routes/$lang.$slug.tsx` (`linkify_citations` writes it as a
/// destination too) is a path, and reading `$lang.$` as math broke the link it
/// was in. See [`literal_regions`].
///
/// The mask alone: the display-math scan runs this over a whole prose run
/// and has no use for which link a byte belongs to. It built that table too
/// (sixteen bytes per byte of prose) and threw it away.
fn code_mask(text: &str) -> Vec<bool> {
    mark_literals(text, |_| {})
}

/// One inline link of a line, as the math scan needs it: where its text opens
/// (the `[`, or the `]` when none opens it), and where its destination opens
/// (the `(`) and ends (just past the `)`). Offsets into the scanned text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinkSpan {
    text: usize,
    open: usize,
    end: usize,
}

/// What the math scan reads as literal in a text ([`literal_regions`]).
struct Literals {
    /// [`code_mask`]: inline code spans and link destinations.
    mask: Vec<bool>,
    /// The inline links, in order; their destinations are disjoint.
    links: Vec<LinkSpan>,
    /// For each byte, the link in `links` whose destination holds it.
    link_at: Vec<Option<usize>>,
}

/// [`code_mask`], the links whose destinations it marks, and which link each
/// of those bytes belongs to: math may hold a link only whole
/// ([`math_blocker`]), and with each byte's link recorded that is read off
/// per byte instead of searched for.
fn literal_regions(text: &str) -> Literals {
    #[cfg(test)]
    LINK_TABLE_BYTES.with(|bytes| bytes.set(bytes.get() + text.len()));
    let mut link_at = vec![None; text.len()];
    let mut links = Vec::new();
    let mask = mark_literals(text, |link| {
        link_at[link.open..link.end].fill(Some(links.len()));
        links.push(link);
    });
    Literals {
        mask,
        links,
        link_at,
    }
}

/// [`code_mask`] of `text`, handing each inline link found on the way to
/// `on_link` (its offsets into `text`), in order. Computed line by line —
/// model output does not break a span or a link across lines.
fn mark_literals(text: &str, mut on_link: impl FnMut(LinkSpan)) -> Vec<bool> {
    let mut mask = vec![false; text.len()];
    let mut base = 0;
    for line in text.split_inclusive('\n') {
        let mut code = vec![false; line.len()];
        for (s, e) in code_spans(line) {
            code[s..e].fill(true);
        }
        for link in link_destinations(line, &code) {
            mask[base + link.open..base + link.end].fill(true);
            on_link(LinkSpan {
                text: base + link.text,
                open: base + link.open,
                end: base + link.end,
            });
        }
        for (k, &c) in code.iter().enumerate() {
            mask[base + k] |= c;
        }
        base += line.len();
    }
    mask
}

/// `line`'s inline links: each `](` outside the code spans `code` (a byte
/// mask) that opens a valid destination — `(<…>)` up to its `>` (CommonMark
/// lets that form hold spaces), or a bare destination up to the `)` that
/// balances its `(`, which it may not reach across whitespace. A `](` that
/// opens no valid destination is no link.
///
/// Linear in the line: the `)` balancing every `(` (and the `[` every `]`
/// closes) is found in one pass with a stack beforehand. Scanning forward
/// from each `](` for its balancing `)` was quadratic — a run of `](` with no
/// `)` scanned to the end of the line from every one — and this runs on the
/// UI thread over model output, which repository text can steer.
fn link_destinations(line: &str, code: &[bool]) -> Vec<LinkSpan> {
    let b = line.as_bytes();
    let mut closes: Vec<Option<usize>> = vec![None; b.len()];
    let mut opens_text: Vec<Option<usize>> = vec![None; b.len()];
    let (mut parens, mut brackets) = (Vec::new(), Vec::new());
    for (k, &c) in b.iter().enumerate() {
        match c {
            b'(' => parens.push(k),
            b')' => {
                if let Some(p) = parens.pop() {
                    closes[p] = Some(k);
                }
            }
            b'[' => brackets.push(k),
            b']' => opens_text[k] = brackets.pop(),
            // A bare destination cannot hold whitespace.
            c if c.is_ascii_whitespace() => parens.clear(),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if b[i] != b']' || b[i + 1] != b'(' || code[i] {
            i += 1;
            continue;
        }
        let open = i + 1;
        let end = if b.get(open + 1) == Some(&b'<') {
            // `(<…>)`: no `<` inside, then `>` and `)`. Each such scan stops
            // at the next `<` or `>`, so together they read the line once.
            b[open + 2..]
                .iter()
                .position(|&c| c == b'>' || c == b'<')
                .map(|k| open + 2 + k)
                .filter(|&gt| b[gt] == b'>' && b.get(gt + 1) == Some(&b')'))
                .map(|gt| gt + 2)
        } else {
            closes[open].map(|close| close + 1)
        };
        match end {
            Some(end) => {
                out.push(LinkSpan {
                    text: opens_text[i].unwrap_or(i),
                    open,
                    end,
                });
                i = end;
            }
            None => i += 2,
        }
    }
    out
}

/// What keeps `$…$` from byte `i` to byte `j` (the two dollars) from being
/// math: its first literal byte — code span, link destination — outside the
/// destinations of links that lie in it WHOLE, text and all; `None` when
/// there is none, and it may be math. `$L[y](t)$` and
/// `$\mathcal{F}[f](\omega)$` are math notation; a `$` pair inside a link's
/// destination, or across its edge, is part of a path.
///
/// O(j − i): each literal byte's link is recorded ([`Literals::link_at`]).
/// Filtering the line's links for every candidate, then searching them for
/// every literal byte, was super-linear on the UI thread.
fn math_blocker(i: usize, j: usize, literal: &Literals) -> Option<usize> {
    (i..j).find(|&k| {
        step();
        literal.mask[k]
            && !literal.link_at[k]
                .is_some_and(|l| literal.links[l].text > i && literal.links[l].end <= j)
    })
}

/// A math/mermaid SVG readied for iced's `svg` widget: recolored for the dark
/// theme and given a logical pixel size (its intrinsic units are stripped so the
/// widget's `Fixed` bounds + `Contain` fit drive the on-screen size).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSvg {
    pub svg: String,
    pub width: f32,
    pub height: f32,
}

/// RaTeX renders math at 40 user-units per em; this maps those `viewBox` units to
/// logical pixels so inline math sits near the surrounding text size and display
/// math scales with it.
const MATH_SCALE: f32 = 0.36;
/// Widest an SVG may render — fits the explanation side panel; wider math or
/// diagrams scale down (preserving aspect) to fit.
const MAX_W: f32 = 356.0;
/// The panel foreground; RaTeX math emits `currentColor`, which resvg can't
/// resolve — resolve it to the active theme's foreground so math reads on either
/// background.
fn fg_hex() -> String {
    crate::theme::hex(crate::theme::fg_bright())
}

/// Recolor and size a raw SVG (RaTeX math / mermaid) for inline display. Both
/// carry a `viewBox`; math is in em-units (scaled to text size), mermaid in px.
pub fn prepare_svg(svg: &str, is_math: bool) -> PreparedSvg {
    // Both raws are theme-independent: math paints `currentColor`, mermaid keeps
    // its slate palette. Resolve to the active theme here so a cached diagram
    // follows a light/dark switch.
    let colored = if is_math {
        svg.replace("currentColor", &fg_hex())
    } else {
        crate::ai::render::recolor_mermaid(svg)
    };
    let (svg, mut width, mut height) = if is_math {
        let (vw, vh) = viewbox_wh(svg).unwrap_or((40.0, 40.0));
        (
            strip_root_attrs(&colored, &["width", "height"]),
            vw * MATH_SCALE,
            vh * MATH_SCALE,
        )
    } else {
        let (vw, vh) = viewbox_wh(svg).unwrap_or((MAX_W, MAX_W * 0.6));
        (strip_root_attrs(&colored, &["width"]), vw, vh)
    };
    // Scale down to fit the panel (preserving aspect); never scale up.
    if width > MAX_W {
        height *= MAX_W / width;
        width = MAX_W;
    }
    PreparedSvg {
        svg,
        width: width.max(1.0),
        height: height.max(1.0),
    }
}

/// Parse the 3rd/4th numbers of the root `viewBox` (its width and height).
fn viewbox_wh(svg: &str) -> Option<(f32, f32)> {
    let head = svg.get(..svg.find('>')?)?;
    let at = head.find("viewBox=\"")? + 9;
    let val = &head[at..at + head[at..].find('"')?];
    let nums: Vec<f32> = val
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    match nums.as_slice() {
        [_, _, w, h, ..] => Some((*w, *h)),
        _ => None,
    }
}

/// Remove the named attributes from the root element (its first `<…>` tag).
fn strip_root_attrs(svg: &str, names: &[&str]) -> String {
    let Some(gt) = svg.find('>') else {
        return svg.to_string();
    };
    let mut head = svg[..gt].to_string();
    for name in names {
        let needle = format!(" {name}=\"");
        while let Some(p) = head.find(&needle) {
            if let Some(q) = head[p + needle.len()..].find('"') {
                head.replace_range(p..p + needle.len() + q + 1, "");
            } else {
                break;
            }
        }
    }
    format!("{head}{}", &svg[gt..])
}

/// Split an LLM markdown string into ordered [`Segment`]s.
pub fn segment(md: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut prose = String::new();
    let mut lines = md.lines();

    while let Some(line) = lines.next() {
        let Some((ch, n, info)) = fence_open(line) else {
            prose.push_str(line);
            prose.push('\n');
            continue;
        };
        // A fenced block (``` or ~~~): mermaid becomes a diagram; anything
        // else a Code segment, highlighted natively at prepare time. Either
        // way its contents — which may hold `$` — are never scanned for math.
        // An unclosed fence runs to the end, as in CommonMark.
        let lang = fence_lang(info).to_string();
        let mut body = Vec::new();
        for l in lines.by_ref() {
            if fence_closes(l, ch, n) {
                break;
            }
            body.push(l);
        }
        flush_prose(&mut prose, &mut out);
        if lang.eq_ignore_ascii_case("mermaid") {
            out.push(Segment::Mermaid(body.join("\n")));
        } else {
            out.push(Segment::Code {
                lang,
                code: body.join("\n"),
            });
        }
    }
    flush_prose(&mut prose, &mut out);
    out
}

/// The language of a fence's info string: its first word (`rust` of
/// `rust,ignore` or `python title="x.py"`).
fn fence_lang(info: &str) -> &str {
    info.split(|c: char| c.is_whitespace() || c == ',' || c == '{')
        .next()
        .unwrap_or("")
}

/// Scan an accumulated prose run for display math (`$$…$$`, possibly
/// multi-line) and inline math (`$…$`, single-line), appending the resulting
/// segments. Code spans are opaque to both.
fn flush_prose(prose: &mut String, out: &mut Vec<Segment>) {
    if prose.trim().is_empty() {
        prose.clear();
        return;
    }
    // Split out display math first, so the inline scan never meets `$$`.
    let mask = code_mask(prose);
    let open = |i: usize| is_dollar(prose, &mask, i) && is_dollar(prose, &mask, i + 1);
    let mut i = 0;
    let mut last = 0;
    while i + 1 < prose.len() {
        if open(i)
            && let Some(end) = (i + 2..prose.len().saturating_sub(1)).find(|&j| open(j))
            && !prose[i + 2..end].trim().is_empty()
        {
            emit_text(&prose[last..i], out);
            out.push(Segment::DisplayMath(prose[i + 2..end].trim().to_string()));
            i = end + 2;
            last = i;
            continue;
        }
        i += 1;
    }
    emit_text(&prose[last..], out);
    prose.clear();
}

/// Whether byte `i` of `text` is a live `$`: not inside a code span and not
/// backslash-escaped.
fn is_dollar(text: &str, mask: &[bool], i: usize) -> bool {
    text.as_bytes().get(i) == Some(&b'$') && !mask[i] && !escaped(text.as_bytes(), i)
}

/// Emit a non-display-math text chunk: group its lines into markdown runs, with
/// any line that carries inline `$…$` math split out as an [`Segment::InlineLine`].
fn emit_text(text: &str, out: &mut Vec<Segment>) {
    if text.trim().is_empty() {
        return;
    }
    let mut md = String::new();
    for line in text.lines() {
        if let Some(parts) = inline_math(line) {
            if !md.trim().is_empty() {
                out.push(Segment::Markdown(md.trim_end().to_string()));
            }
            md.clear();
            out.push(Segment::InlineLine(parts));
        } else {
            md.push_str(line);
            md.push('\n');
        }
    }
    if !md.trim().is_empty() {
        out.push(Segment::Markdown(md.trim_end().to_string()));
    }
}

/// If `line` contains inline `$…$` math, split it into text/math pieces; else
/// `None`. Pandoc's rule decides what is math: the opening `$` has a
/// non-space character right after it, the closing `$` a non-space character
/// right before it and no digit right after it — so `$5 and $10`, `$PATH`,
/// `$(…)` and `costs $5/$10` stay text. A span may not cross a code span, and
/// `\$` never delimits.
///
/// Linear in the line, which is model output on the UI thread. An opener
/// takes the first closing `$` past it, and whether a `$` closes depends on
/// its neighbours alone, so the closers are listed once and read from a
/// cursor that only moves right, as the openers do — each opener searching
/// the rest of the line for one was quadratic. A span that may not be math
/// is skipped to its blocker ([`math_blocker`]), not retried from the next
/// opener.
fn inline_math(line: &str) -> Option<Vec<Inline>> {
    let literal = literal_regions(line);
    let live = |i: usize| is_dollar(line, &literal.mask, i);
    let is_space = |c: Option<char>| c.is_none_or(char::is_whitespace);
    let closers: Vec<usize> = (0..line.len())
        .filter(|&j| {
            live(j)
                && !is_space(line[..j].chars().next_back())
                && !line[j + 1..].starts_with(|c: char| c.is_ascii_digit())
        })
        .collect();
    let mut next_closer = 0;
    let mut parts = Vec::new();
    let mut i = 0;
    let mut last = 0;
    while i < line.len() {
        if !live(i) {
            i += 1;
            continue;
        }
        let after_open = line[i + 1..].chars().next();
        if is_space(after_open) || after_open == Some('$') {
            i += 1;
            continue;
        }
        while closers.get(next_closer).is_some_and(|&j| j < i + 2) {
            step();
            next_closer += 1;
        }
        // No `$` past this opener closes it, so none closes a later one.
        let Some(&j) = closers.get(next_closer) else {
            break;
        };
        match math_blocker(i, j, &literal) {
            None => {
                if last < i {
                    parts.push(Inline::Text(line[last..i].to_string()));
                }
                parts.push(Inline::Math(line[i + 1..j].to_string()));
                i = j + 1;
                last = i;
            }
            // Every opener before the blocker would close at `j` too and
            // meet the same blocker: the byte is still between them, and a
            // link it lies in opens before them as well. So none of them is
            // math, and the scan resumes past it.
            Some(k) => i = k + 1,
        }
    }
    if parts.is_empty() {
        return None;
    }
    if last < line.len() {
        parts.push(Inline::Text(line[last..].to_string()));
    }
    Some(parts)
}

/// Whether the byte at `i` is backslash-escaped (an odd run of backslashes
/// right before it).
fn escaped(bytes: &[u8], i: usize) -> bool {
    bytes[..i].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

/// One prose piece of a [`Segment::InlineLine`] (the text around inline
/// math), parsed ONCE when the segments are prepared, for the view to draw
/// its **bold**, *emphasis*, `code` and links with `rich_text` beside the
/// equations instead of printing the markdown source. Parsed by iced's own
/// markdown parser, so it looks like the markdown around it; a list marker at
/// the start of the line becomes a bullet, and whitespace at the piece's
/// edges (the gap before or after an equation) is kept.
///
/// The parse is the expensive part, and running it for every piece of every
/// inline-math line on every view rebuild — a mouse move is one — re-parsed
/// the same text per frame. The walk to styled runs ([`InlinePiece::spans`])
/// stays per view: it depends on the theme's style.
///
/// Known limit: formatting that spans an equation (`**a $x$ b**`) is split
/// with the line, so its markers show literally.
#[derive(Debug, Clone)]
pub struct InlinePiece {
    items: Vec<iced::widget::markdown::Item>,
    /// The piece is only whitespace (a gap between two equations).
    blank: bool,
    lead_space: bool,
    trail_space: bool,
}

impl InlinePiece {
    /// Parse `piece` once: its markdown items, and whether it is blank or
    /// has whitespace at either edge (kept, so the gap beside an equation
    /// survives — see [`InlinePiece::spans`]).
    pub fn parse(piece: &str) -> Self {
        let blank = piece.trim().is_empty();
        InlinePiece {
            items: if blank {
                Vec::new()
            } else {
                iced::widget::markdown::parse(piece).collect()
            },
            blank,
            lead_space: piece.starts_with(char::is_whitespace),
            trail_space: piece.ends_with(char::is_whitespace),
        }
    }

    /// The piece's styled runs in `style` — a walk, no parsing.
    pub fn spans(
        &self,
        style: iced::widget::markdown::Style,
    ) -> Vec<iced::widget::text::Span<'static, String>> {
        use iced::widget::markdown::{Bullet, Item, Style};
        use iced::widget::text::Span;
        fn walk(items: &[Item], style: Style, out: &mut Vec<Span<'static, String>>) {
            for item in items {
                match item {
                    Item::Paragraph(text) | Item::Heading(_, text) => {
                        out.extend(text.spans(style).iter().cloned());
                    }
                    Item::List { bullets, .. } => {
                        for bullet in bullets {
                            out.push(Span::new("• "));
                            let (Bullet::Point { items } | Bullet::Task { items, .. }) = bullet;
                            walk(items, style, out);
                        }
                    }
                    Item::Quote(items) => walk(items, style, out),
                    Item::CodeBlock { code, .. } => {
                        out.push(Span::new(code.clone()).font(style.inline_code_font));
                    }
                    Item::Image { alt, .. } => out.extend(alt.spans(style).iter().cloned()),
                    Item::Rule | Item::Table { .. } => {}
                }
            }
        }
        let mut out = Vec::new();
        if self.blank {
            if self.lead_space {
                out.push(Span::new(" "));
            }
            return out;
        }
        if self.lead_space {
            out.push(Span::new(" "));
        }
        walk(&self.items, style, &mut out);
        if self.trail_space {
            out.push(Span::new(" "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Math that holds a whole link-shaped construct is math: `F[f](ω)` and
    /// `L[y](t)` are notation, and masking every `](…)` as a destination
    /// sent those lines to the markdown parser, which drew a link. A `$`
    /// pair inside a destination, or across its edge, is still a path.
    #[test]
    fn math_may_hold_a_whole_link_shaped_construct() {
        let maths = |line: &str| -> Vec<String> {
            inline_math(line)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|p| match p {
                    Inline::Math(m) => Some(m),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            maths("The transform $\\mathcal{F}[f](\\omega)$ here."),
            ["\\mathcal{F}[f](\\omega)"]
        );
        assert_eq!(maths("Solve $L[y](t) = 0$."), ["L[y](t) = 0"]);
        assert!(maths("Open [it](src/$lang.$slug.tsx) now.").is_empty());
        assert!(
            maths("A $x [t](a$b) edge.").is_empty(),
            "across a destination's edge"
        );
        assert_eq!(maths("See [t](a$b) and $c$."), ["c"]);
    }

    /// The math scan is linear in the line. Finding link destinations, code
    /// spans, each opener's closing `$`, and whether a span may be math each
    /// once searched the rest of the line from every candidate — quadratic,
    /// on the UI thread, over text a model wrote. Bounded in counted steps
    /// (`STEPS`), which no machine's speed can blur, with a timing kept as
    /// the backstop for work the count does not see.
    #[test]
    fn the_math_scan_is_linear_in_the_line() {
        let started = std::time::Instant::now();
        for line in [
            // A run of `](` with no `)`: each looked for one to the end.
            "](".repeat(40_000),
            // Every opener closes at the last `$`, and each span meets the
            // code span before it, past a thousand links it may hold.
            "$x ".repeat(1_000) + &"[a](b)".repeat(1_000) + "`c` y$",
            // Nothing closes: each opener looked for a closer to the end.
            "$a ".repeat(20_000),
            // Escaped runs, none with a partner: each looked to the end.
            "\\``".repeat(20_000),
        ] {
            STEPS.set(0);
            let segs = segment(&line);
            let steps = STEPS.get();
            assert!(
                segs.iter().all(|s| matches!(s, Segment::Markdown(_))),
                "math in {:?}…",
                &line[..12]
            );
            assert!(
                steps <= 4 * line.len(),
                "{steps} steps for {:?}… ({} bytes)",
                &line[..12],
                line.len()
            );
        }
        let code = vec![false; 80_000];
        assert!(link_destinations(&"](".repeat(40_000), &code).is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// The display-math scan reads a whole prose run's mask and nothing else:
    /// it also built the per-byte link table the inline scan uses — sixteen
    /// bytes per byte of prose — and threw it away. Only the inline scan
    /// builds one, a line at a time; the mask is the same either way.
    #[test]
    fn the_display_math_scan_builds_no_link_table() {
        let line = "See [a](x/$y$.md), `$z$` and [b](<c $d$>) for $x$.";
        let prose = format!("{line}\n").repeat(200);
        LINK_TABLE_BYTES.set(0);
        let mask = code_mask(&prose);
        assert_eq!(
            LINK_TABLE_BYTES.get(),
            0,
            "a link table for the whole prose"
        );
        assert_eq!(mask, literal_regions(&prose).mask);
        assert!(mask.iter().any(|&m| m) && !mask.iter().all(|&m| m));

        LINK_TABLE_BYTES.set(0);
        let _ = segment(&prose);
        assert_eq!(
            LINK_TABLE_BYTES.get(),
            200 * line.len(),
            "the inline scan's tables are the lines' alone"
        );
    }

    /// A15: a link destination is a path, not prose: a cited route file named
    /// `$lang.$slug.tsx` — linkified or written as a plain link — stays one
    /// markdown link instead of having `$lang.$` read as math, while real
    /// math on the same line is still math.
    #[test]
    fn dollars_in_a_link_destination_are_not_math() {
        let cited = linkify_citations("See `src/routes/$lang.$slug.tsx:3` and $x^2$.", |p| {
            (p == "src/routes/$lang.$slug.tsx").then(|| p.to_string())
        });
        assert!(
            cited.contains("](<clew:src/routes/$lang.$slug.tsx:3>)"),
            "{cited}"
        );
        let inline = |md: &str| -> Vec<Inline> {
            segment(md)
                .into_iter()
                .flat_map(|s| match s {
                    Segment::InlineLine(parts) => parts,
                    _ => Vec::new(),
                })
                .collect()
        };
        let parts = inline(&cited);
        let maths: Vec<&str> = parts
            .iter()
            .filter_map(|p| match p {
                Inline::Math(m) => Some(m.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(maths, ["x^2"], "{parts:?}");
        assert!(
            parts.iter().any(|p| matches!(
                p,
                Inline::Text(t) if t.contains("(<clew:src/routes/$lang.$slug.tsx:3>)")
            )),
            "the link was split: {parts:?}"
        );
        // A plain link, and one whose destination has balanced parentheses.
        for md in [
            "Open [the route](src/routes/$lang.$slug.tsx) now.",
            "Open [it](docs/a_(b)_$x.$y.md) now.",
        ] {
            assert!(
                segment(md)
                    .iter()
                    .all(|s| matches!(s, Segment::Markdown(_))),
                "{md}: {:?}",
                segment(md)
            );
        }
        // Brackets that open no destination leave the math alone.
        assert_eq!(
            inline("f[x] ($a$) and $b$")
                .iter()
                .filter(|p| matches!(p, Inline::Math(_)))
                .count(),
            2
        );
    }

    #[test]
    fn plain_markdown_is_one_segment() {
        let segs = segment("# Title\n\nA paragraph with `code`.");
        assert_eq!(segs.len(), 1);
        assert!(matches!(&segs[0], Segment::Markdown(s) if s.contains("# Title")));
    }

    #[test]
    fn display_math_becomes_its_own_block() {
        let segs = segment("Before.\n\n$$\\int_0^1 x\\,dx$$\n\nAfter.");
        let maths: Vec<_> = segs
            .iter()
            .filter(|s| matches!(s, Segment::DisplayMath(_)))
            .collect();
        assert_eq!(maths.len(), 1);
        assert!(matches!(&maths[0], Segment::DisplayMath(t) if t.contains("\\int_0^1")));
        // Surrounding prose is preserved on both sides.
        assert!(
            segs.iter()
                .any(|s| matches!(s, Segment::Markdown(m) if m.contains("Before")))
        );
        assert!(
            segs.iter()
                .any(|s| matches!(s, Segment::Markdown(m) if m.contains("After")))
        );
    }

    #[test]
    fn mermaid_fence_becomes_a_diagram_and_other_fences_become_code() {
        let segs = segment("```mermaid\ngraph TD\n A-->B\n```\n\n```rust\nlet x = 1;\n```");
        assert!(
            segs.iter()
                .any(|s| matches!(s, Segment::Mermaid(b) if b.contains("A-->B")))
        );
        // The rust fence becomes a Code segment for native highlighting.
        assert!(segs.iter().any(
            |s| matches!(s, Segment::Code { lang, code } if lang == "rust" && code == "let x = 1;")
        ));
    }

    #[test]
    fn citations_linkify_only_real_project_files() {
        let resolve = |p: &str| matches!(p, "src/main.rs" | "Cargo.toml").then(|| p.to_string());
        let md = "Entry is `src/main.rs:62` ([[bin]] in `Cargo.toml`), not `Vec<String>`.\n\
                  ```rust\nlet a = `src/main.rs:1`;\n```";
        let out = linkify_citations(md, resolve);
        assert!(out.contains("[`src/main.rs:62`](<clew:src/main.rs:62>)"));
        assert!(out.contains("[`Cargo.toml`](<clew:Cargo.toml>)"));
        // Non-files keep their code-chip form; fenced code is untouched.
        assert!(out.contains("`Vec<String>`"));
        assert!(out.contains("let a = `src/main.rs:1`;"));
    }

    #[test]
    fn citation_line_ranges_collapse_to_their_start() {
        let resolve = |p: &str| (p == "src/app/update.rs").then(|| p.to_string());
        let out = linkify_citations("see `src/app/update.rs:62-70`", resolve);
        assert!(out.contains("[`src/app/update.rs:62-70`](<clew:src/app/update.rs:62>)"));
    }

    #[test]
    fn bare_file_names_resolve_through_the_lookup() {
        // The caller resolves unique basenames to their rel.
        let resolve = |p: &str| (p == "theme.rs").then(|| "src/miscellaneous/theme.rs".to_string());
        let out = linkify_citations("palette in `theme.rs:37`", resolve);
        assert!(out.contains("[`theme.rs:37`](<clew:src/miscellaneous/theme.rs:37>)"));
    }

    /// The link text and destination of every link iced's markdown parser
    /// finds in `md`.
    fn parsed_links(md: &str) -> Vec<(String, String)> {
        use iced::widget::markdown::{Item, parse};
        let style = crate::theme::markdown_settings().style;
        let mut out = Vec::new();
        for item in parse(md).collect::<Vec<Item>>() {
            if let Item::Paragraph(text) = item {
                for span in text.spans(style).iter() {
                    if let Some(link) = &span.link {
                        out.push((span.text.to_string(), link.clone()));
                    }
                }
            }
        }
        out
    }

    /// `__init__.py` inside plain link text parsed as emphasis ("init.py" in
    /// bold); inside a code span it stays literal and monospaced. A path with
    /// a space stays one link whose destination is the path itself.
    #[test]
    fn citations_keep_their_code_span_and_survive_spaces() {
        let resolve =
            |p: &str| matches!(p, "pkg/__init__.py" | "docs/my notes.md").then(|| p.to_string());
        let out = linkify_citations("see `pkg/__init__.py` and `docs/my notes.md:3`\n", resolve);
        assert_eq!(
            out,
            "see [`pkg/__init__.py`](<clew:pkg/__init__.py>) and \
             [`docs/my notes.md:3`](<clew:docs/my notes.md:3>)\n"
        );
        assert_eq!(
            parsed_links(&out),
            [
                (
                    "pkg/__init__.py".to_string(),
                    "clew:pkg/__init__.py".to_string()
                ),
                (
                    "docs/my notes.md:3".to_string(),
                    "clew:docs/my notes.md:3".to_string()
                ),
            ]
        );
    }

    /// Fences of either kind and of any length are left alone, as are code
    /// spans that already are link text.
    #[test]
    fn citations_skip_tilde_fences_and_existing_links() {
        let resolve = |p: &str| (p == "a.rs").then(|| p.to_string());
        let md = "~~~\n`a.rs`\n~~~\n````\n```\n`a.rs`\n````\n[`a.rs`](https://x)\n`a.rs`";
        let out = linkify_citations(md, resolve);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[1], "`a.rs`", "inside ~~~");
        assert_eq!(
            lines[5], "`a.rs`",
            "inside a 4-backtick fence (``` does not close it)"
        );
        assert_eq!(lines[7], "[`a.rs`](https://x)", "already a link");
        assert_eq!(lines[8], "[`a.rs`](<clew:a.rs>)");
    }

    #[test]
    fn inline_math_keeps_the_line_together() {
        let segs = segment("The ratio $E = mc^2$ must hold here.");
        let line = segs.iter().find_map(|s| match s {
            Segment::InlineLine(parts) => Some(parts),
            _ => None,
        });
        let parts = line.expect("an inline line");
        assert_eq!(parts[0], Inline::Text("The ratio ".into()));
        assert_eq!(parts[1], Inline::Math("E = mc^2".into()));
        assert_eq!(parts[2], Inline::Text(" must hold here.".into()));
    }

    #[test]
    fn dollars_in_code_fence_are_not_math() {
        let segs = segment("```sh\necho $HOME and $PATH\n```");
        // Stays one markdown segment; no math extracted.
        assert!(renderables(&segs).is_empty());
        assert!(
            segs.iter()
                .all(|s| !matches!(s, Segment::DisplayMath(_) | Segment::InlineLine(_)))
        );
    }

    #[test]
    fn prepare_svg_recolors_and_sizes_math() {
        // RaTeX-style output: pt width/height plus an em-unit viewBox.
        let raw = r#"<svg width="100pt" height="40pt" viewBox="0 0 100 40"><path fill="currentColor" d="M0 0"/></svg>"#;
        let p = prepare_svg(raw, true);
        assert!(
            !p.svg.contains("currentColor"),
            "currentColor resolved for resvg"
        );
        assert!(!p.svg.contains("width=\"100pt\""), "pt width stripped");
        assert!(!p.svg.contains("height=\"40pt\""), "pt height stripped");
        // Sized from the viewBox in em-units, not the pt attrs.
        assert!((p.width - 100.0 * MATH_SCALE).abs() < 0.5);
        assert!((p.height - 40.0 * MATH_SCALE).abs() < 0.5);
    }

    #[test]
    fn prepare_svg_sizes_mermaid_from_viewbox_and_caps_width() {
        let raw = r#"<svg viewBox="-8 -8 285.6 216.8" width="100%" style="max-width:285.6px"><text>x</text></svg>"#;
        let p = prepare_svg(raw, false);
        assert!(
            !p.svg.contains("width=\"100%\""),
            "percentage width stripped"
        );
        assert!(
            (p.width - 285.6).abs() < 1.0,
            "under the cap → intrinsic width"
        );
        // Aspect preserved.
        assert!((p.height / p.width - 216.8 / 285.6).abs() < 0.01);
    }

    #[test]
    fn renderables_are_deduplicated_by_key() {
        let segs = segment("$$a+b$$\n\nand again $$a+b$$");
        let r = renderables(&segs);
        assert_eq!(r.len(), 1, "identical equations share one render: {r:?}");
        assert_eq!(r[0].kind, "math");
        assert!(r[0].display);
    }
}

/// Math detection (E1-6): Pandoc's `$` rules, code opacity, `~~~` fences, and
/// markdown kept around inline math.
#[cfg(test)]
mod math_tests {
    use super::*;

    fn has_math(segs: &[Segment]) -> bool {
        segs.iter()
            .any(|s| matches!(s, Segment::DisplayMath(_) | Segment::InlineLine(_)))
    }

    /// Money, shell variables and jQuery are prose, and the paragraph stays
    /// ONE markdown segment — its bold and links intact.
    #[test]
    fn dollar_signs_in_prose_are_not_math() {
        for md in [
            "It costs $5 and **$10** with tax.",
            "Set $PATH and $HOME, then run `$(pwd)`.",
            "Upgrades are $5/$10 per [seat](clew:src/a.rs).",
            "jQuery's $('#id') and $.ajax both work.",
            "Escaped \\$x\\$ stays literal.",
            "A lone $ sign.",
            "Price $ 5 $ total.",
        ] {
            let segs = segment(md);
            assert!(!has_math(&segs), "false math in {md:?}: {segs:?}");
            assert!(renderables(&segs).is_empty());
            assert_eq!(segs, vec![Segment::Markdown(md.to_string())], "{md:?}");
        }
    }

    /// Code is opaque: `$$` or `$…$` inside a code span (of any backtick
    /// length) or a fence of either kind is never math.
    #[test]
    fn code_is_opaque_to_math() {
        let segs = segment("Use `echo $$` for the PID and `$x$` literally.");
        assert!(!has_math(&segs), "{segs:?}");
        let segs = segment("A span ``a $b$ ` c`` then $y$.");
        let parts = segs.iter().find_map(|s| match s {
            Segment::InlineLine(p) => Some(p.clone()),
            _ => None,
        });
        assert_eq!(
            parts,
            Some(vec![
                Inline::Text("A span ``a $b$ ` c`` then ".into()),
                Inline::Math("y".into()),
                Inline::Text(".".into()),
            ])
        );
        let segs = segment("~~~sh\necho $HOME $$ $x$\n~~~\n\n~~~mermaid\ngraph TD\n A-->B\n~~~");
        assert!(!has_math(&segs), "{segs:?}");
        assert!(segs.contains(&Segment::Code {
            lang: "sh".into(),
            code: "echo $HOME $$ $x$".into()
        }));
        assert!(
            segs.iter()
                .any(|s| matches!(s, Segment::Mermaid(b) if b.contains("A-->B")))
        );
        // A longer fence is only closed by one at least as long.
        let segs = segment("````md\n```rust\nlet x = \"$y$\";\n```\n````");
        assert_eq!(
            segs,
            vec![Segment::Code {
                lang: "md".into(),
                code: "```rust\nlet x = \"$y$\";\n```".into()
            }]
        );
    }

    /// Real math is still found: inline next to punctuation, and display
    /// math spanning lines.
    #[test]
    fn real_math_is_still_found() {
        let segs = segment("Energy $E = mc^2$, and $x$.\n\n$$\n\\sum_i x_i\n$$\n");
        let parts = segs.iter().find_map(|s| match s {
            Segment::InlineLine(p) => Some(p.clone()),
            _ => None,
        });
        assert_eq!(
            parts,
            Some(vec![
                Inline::Text("Energy ".into()),
                Inline::Math("E = mc^2".into()),
                Inline::Text(", and ".into()),
                Inline::Math("x".into()),
                Inline::Text(".".into()),
            ])
        );
        assert!(segs.contains(&Segment::DisplayMath("\\sum_i x_i".into())));
        // A closing `$` followed by a digit does not close.
        assert!(!has_math(&segment("between $5$10 and more")));
    }

    /// The prose around inline math keeps its markdown: bold, code and links
    /// come back as styled spans (not asterisks and brackets), with the gaps
    /// next to the equation preserved.
    #[test]
    fn inline_pieces_render_markdown() {
        let style = crate::theme::markdown_settings().style;
        let inline_spans = |piece: &str, style| InlinePiece::parse(piece).spans(style);
        let spans = inline_spans("The **ratio** of `a` to [b](clew:src/b.rs:2) ", style);
        let text: String = spans.iter().map(|s| s.text.as_ref()).collect();
        assert_eq!(text, "The ratio of a to b ");
        let bold = spans
            .iter()
            .find(|s| s.text.as_ref() == "ratio")
            .expect("a bold span");
        assert!(
            bold.font
                .is_some_and(|f| f.weight == iced::font::Weight::Bold)
        );
        let link = spans
            .iter()
            .find(|s| s.link.is_some())
            .expect("a link span");
        assert_eq!(link.link.as_deref(), Some("clew:src/b.rs:2"));
        // A list marker becomes a bullet; a leading gap is kept.
        let text: String = inline_spans("- item ", style)
            .iter()
            .map(|s| s.text.to_string())
            .collect();
        assert_eq!(text, "• item ");
        let text: String = inline_spans(" tail", style)
            .iter()
            .map(|s| s.text.to_string())
            .collect();
        assert_eq!(text, " tail");
    }

    /// A piece parsed once yields the same runs on every view — the view
    /// need not re-run the markdown parser per frame — and a blank piece (the
    /// gap between two equations) keeps its width.
    #[test]
    fn a_parsed_piece_renders_the_same_on_every_view() {
        let style = crate::theme::markdown_settings().style;
        let flat = |spans: Vec<iced::widget::text::Span<'static, String>>| -> Vec<(String, bool)> {
            spans
                .iter()
                .map(|s| (s.text.to_string(), s.link.is_some()))
                .collect()
        };
        for piece in [
            "The **ratio** of `a` to [b](clew:src/b.rs:2) ",
            "- item ",
            " tail",
            "   ",
            "",
            "> quoted *text*",
        ] {
            let parsed = InlinePiece::parse(piece);
            let first = flat(parsed.spans(style));
            assert_eq!(flat(parsed.spans(style)), first, "{piece:?}");
            let text: String = first.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(text.trim().is_empty(), piece.trim().is_empty(), "{piece:?}");
        }
    }
}
