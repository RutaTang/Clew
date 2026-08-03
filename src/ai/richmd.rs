//! Splits an LLM markdown explanation into renderable segments and defines the
//! cache keys for its math/mermaid → SVG renders.
//!
//! iced's markdown widget renders prose, headings, lists and code, but not math
//! or mermaid diagrams. So we split the source into [`Segment`]s: ordinary
//! markdown runs (handed to the markdown widget, which wraps and styles them)
//! interleaved with math and mermaid, which are rendered to SVG out-of-process
//! (the `clew-view --export` helper drives MathJax + mermaid.js) and shown
//! inline via iced's `svg` widget. Each renderable is keyed by a content hash so
//! a given equation or diagram is generated at most once and cached on disk.

use std::path::{Path, PathBuf};

use crate::incremental::{Version, content_hash};

fn svg_dir(root: &Path) -> PathBuf {
    root.join(".clew").join("cache").join("svg")
}

/// Load a previously generated raw SVG for `key`, if cached on disk.
pub fn load_raw(root: &Path, key: Version) -> Option<String> {
    std::fs::read_to_string(svg_dir(root).join(format!("{key}.svg"))).ok()
}

/// Persist a raw SVG for `key` (best-effort; ignored on error).
pub fn store_raw(root: &Path, key: Version, raw: &str) {
    let dir = svg_dir(root);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join(format!("{key}.svg")), raw);
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
    /// `"math"` or `"mermaid"` — matches the `clew-view --export` request schema.
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
/// links (`[path:line](clew:rel:line)`) so they become clickable jumps.
/// `resolve` maps a candidate path to a real project rel (exact rel, or a
/// unique basename like `theme.rs`) — only those are linkified, so type names
/// like `Vec<String>` stay code chips. Fenced blocks are left untouched.
pub fn linkify_citations(md: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(md.len());
    let mut in_fence = false;
    for (i, line) in md.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        // Scan the line's `code` spans; rebuild it with citations linkified.
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else {
                out.push_str(&rest[open..]);
                rest = "";
                break;
            };
            let code = &after[..close];
            match citation_target(code, &resolve) {
                Some(target) => out.push_str(&format!("[{code}](clew:{target})")),
                None => {
                    out.push('`');
                    out.push_str(code);
                    out.push('`');
                }
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
    }
    out
}

/// If an inline-code span reads as a citation of a real project file, return
/// the `rel[:line]` jump target (a `path:12-34` range collapses to its start).
fn citation_target(code: &str, resolve: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let code = code.trim();
    if code.contains(char::is_whitespace) || code.is_empty() {
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
    Some(match line {
        Some(n) => format!("{rel}:{n}"),
        None => rel,
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
        let t = line.trim_start();
        if t.starts_with("```") {
            // A fenced block: mermaid becomes a diagram; anything else stays as
            // literal markdown (so the markdown widget syntax-highlights it) and
            // is never scanned for `$` math.
            let lang = t.trim_start_matches('`').trim().to_string();
            let mut body = Vec::new();
            let mut closed = false;
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    closed = true;
                    break;
                }
                body.push(l);
            }
            flush_prose(&mut prose, &mut out);
            let _ = closed;
            if lang.eq_ignore_ascii_case("mermaid") {
                out.push(Segment::Mermaid(body.join("\n")));
            } else {
                // Its own segment, highlighted natively at prepare time — and
                // its contents (which may hold `$`) are never scanned as math.
                out.push(Segment::Code {
                    lang,
                    code: body.join("\n"),
                });
            }
        } else {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    flush_prose(&mut prose, &mut out);
    out
}

/// Scan an accumulated prose run for display math (`$$…$$`, possibly multi-line)
/// and inline math (`$…$`, single-line), appending the resulting segments.
fn flush_prose(prose: &mut String, out: &mut Vec<Segment>) {
    if prose.trim().is_empty() {
        prose.clear();
        return;
    }
    // Split out display math first, so the inline scan never meets `$$`.
    let bytes = prose.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$'
            && bytes[i + 1] == b'$'
            && !escaped(bytes, i)
            && let Some(end) = find(prose, i + 2, "$$")
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

/// If `line` contains inline `$…$` math, split it into text/math pieces; else None.
fn inline_math(line: &str) -> Option<Vec<Inline>> {
    let bytes = line.as_bytes();
    let mut parts = Vec::new();
    let mut i = 0;
    let mut last = 0;
    let mut found = false;
    let mut in_code = false; // inside a `backtick` span → `$` is literal
    while i < bytes.len() {
        if bytes[i] == b'`' {
            in_code = !in_code;
            i += 1;
            continue;
        }
        if bytes[i] == b'$'
            && !in_code
            && !escaped(bytes, i)
            && let Some(end) = find(line, i + 1, "$")
        {
            let tex = line[i + 1..end].trim();
            // Reject empty / whitespace-only spans (e.g. a lone "$" or "$ $").
            if !tex.is_empty() {
                if last < i {
                    parts.push(Inline::Text(line[last..i].to_string()));
                }
                parts.push(Inline::Math(tex.to_string()));
                i = end + 1;
                last = i;
                found = true;
                continue;
            }
        }
        i += 1;
    }
    if !found {
        return None;
    }
    if last < line.len() {
        parts.push(Inline::Text(line[last..].to_string()));
    }
    Some(parts)
}

/// Find `needle` in `hay` at or after `from`, returning the byte index of its
/// start. `needle` is ASCII, so the index lands on a char boundary.
fn find(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..)
        .and_then(|s| s.find(needle))
        .map(|p| from + p)
}

/// Whether the byte at `i` is backslash-escaped.
fn escaped(bytes: &[u8], i: usize) -> bool {
    i > 0 && bytes[i - 1] == b'\\'
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(out.contains("[src/main.rs:62](clew:src/main.rs:62)"));
        assert!(out.contains("[Cargo.toml](clew:Cargo.toml)"));
        // Non-files keep their code-chip form; fenced code is untouched.
        assert!(out.contains("`Vec<String>`"));
        assert!(out.contains("let a = `src/main.rs:1`;"));
    }

    #[test]
    fn citation_line_ranges_collapse_to_their_start() {
        let resolve = |p: &str| (p == "src/app/update.rs").then(|| p.to_string());
        let out = linkify_citations("see `src/app/update.rs:62-70`", resolve);
        assert!(out.contains("[src/app/update.rs:62-70](clew:src/app/update.rs:62)"));
    }

    #[test]
    fn bare_file_names_resolve_through_the_lookup() {
        // The caller resolves unique basenames to their rel.
        let resolve = |p: &str| (p == "theme.rs").then(|| "src/miscellaneous/theme.rs".to_string());
        let out = linkify_citations("palette in `theme.rs:37`", resolve);
        assert!(out.contains("[theme.rs:37](clew:src/miscellaneous/theme.rs:37)"));
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
