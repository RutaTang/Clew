//! Native math + mermaid → self-contained SVG, in-process (no webview, no helper
//! binary). Math goes through RaTeX (KaTeX-compatible, glyph outlines embedded);
//! mermaid through `mermaid-rs-renderer`. Both emit SVG that
//! `richmd::prepare_svg` then recolors/sizes for the active theme, and both
//! stamp their root element with the renderer's name
//! ([`crate::richmd::KIND_ATTR`]) so a cached SVG says what it is.
//!
//! These are libraries fed model output. Three guards keep one bad unit from
//! costing more than itself: sources past a size or nesting limit are refused
//! before they reach a renderer (deep nesting is what recurses deepest, and a
//! stack overflow aborts the process — no `catch_unwind` contains it); a
//! panic is caught; and [`render_guarded`] runs each unit on its own thread
//! with a large stack and a deadline, so a runaway render neither blocks the
//! batch behind it nor exhausts the caller's stack.

use std::time::Duration;

use crate::richmd::KIND_ATTR;

/// Longest TeX source rendered: far past any real equation.
const MAX_TEX_BYTES: usize = 16 * 1024;
/// Deepest `{`/`[`/`(` nesting of TeX rendered. The parser and the layout
/// recurse on groups.
const MAX_TEX_DEPTH: usize = 64;
/// Longest mermaid source rendered: far past any diagram the explanations
/// draw (a dozen nodes).
const MAX_MERMAID_BYTES: usize = 64 * 1024;
/// Deepest bracket or `subgraph … end` nesting of mermaid rendered.
const MAX_MERMAID_DEPTH: usize = 32;

/// Stack for one guarded render: the default 2 MiB of a spawned thread is
/// what a deeply nested source overflowed. Reserved, not committed, so
/// generous is cheap.
pub const RENDER_STACK_BYTES: usize = 64 * 1024 * 1024;
/// How long one render may take before its unit is given up on.
pub const RENDER_DEADLINE: Duration = Duration::from_secs(10);

/// Deepest nesting of `(`/`[`/`{` in `src`, not counting a bracket escaped
/// with a backslash (`\{` is a literal brace in TeX).
fn bracket_depth(src: &str) -> usize {
    let (mut depth, mut deepest, mut escaped) = (0usize, 0usize, false);
    for c in src.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '(' | '[' | '{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// Deepest `subgraph … end` nesting of a mermaid source.
fn subgraph_depth(src: &str) -> usize {
    let (mut depth, mut deepest) = (0usize, 0usize);
    for line in src.lines() {
        let word = line.split_whitespace().next().unwrap_or("");
        if word == "subgraph" {
            depth += 1;
            deepest = deepest.max(depth);
        } else if word == "end" {
            depth = depth.saturating_sub(1);
        }
    }
    deepest
}

/// Render a LaTeX math string to a self-contained SVG, or `None` if it doesn't
/// parse (or is past the size or nesting limit). RaTeX paints glyphs solid
/// black; we rewrite that to `currentColor` so `prepare_svg` can theme it for
/// the active theme.
pub fn math_svg(tex: &str) -> Option<String> {
    if tex.len() > MAX_TEX_BYTES || bracket_depth(tex) > MAX_TEX_DEPTH {
        return None;
    }
    let nodes = ratex_parser::parse(tex).ok()?;
    let boxed = ratex_layout::layout(&nodes, &ratex_layout::LayoutOptions::default());
    let list = ratex_layout::to_display_list(&boxed);
    let opts = ratex_svg::SvgOptions {
        embed_glyphs: true,
        ..Default::default()
    };
    let svg = ratex_svg::render_to_svg(&list, &opts);
    Some(stamp(&svg.replace("rgba(0,0,0,1)", "currentColor"), "math"))
}

/// Render a mermaid diagram to a self-contained SVG, or `None` if it doesn't
/// parse (or is past the size or nesting limit). `mermaid-rs-renderer` emits a
/// fixed light "slate" palette; that raw output is kept theme-independent
/// (cached to disk as-is) and remapped onto the active theme later, at
/// `prepare_svg` time — so a diagram follows a light/dark switch instead of
/// freezing the theme it was first rendered in.
pub fn mermaid_svg(src: &str) -> Option<String> {
    if src.len() > MAX_MERMAID_BYTES
        || bracket_depth(src) > MAX_MERMAID_DEPTH
        || subgraph_depth(src) > MAX_MERMAID_DEPTH
    {
        return None;
    }
    mermaid_rs_renderer::render(src)
        .ok()
        .map(|svg| stamp(&svg, "mermaid"))
}

/// How one render run through [`run_guarded`] ended.
#[derive(Debug, PartialEq)]
pub enum Guarded<T> {
    /// It returned.
    Done(T),
    /// It panicked (with the panic's message, cut to 120 chars), or no thread
    /// could be started for it.
    Crashed(String),
    /// It was still running at the deadline. A thread cannot be stopped from
    /// outside: it is left to finish (or spin) on its own, detached, and the
    /// caller moves on.
    TimedOut,
}

/// Run `render` on a thread of its own with `stack_bytes` of stack, waiting
/// at most `deadline` for it. `catch_unwind` alone (on the caller's thread)
/// contained a renderer's panics but neither a runaway loop — the batch, and
/// every diagram queued behind the unit, waited forever — nor recursion
/// deeper than the stack, which aborts the whole process.
pub fn run_guarded<T: Send + 'static>(
    stack_bytes: usize,
    deadline: Duration,
    render: impl FnOnce() -> T + Send + 'static,
) -> Guarded<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("clew-render".into())
        .stack_size(stack_bytes)
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(render));
            let _ = tx.send(outcome);
        });
    if let Err(e) = spawned {
        return Guarded::Crashed(format!("no render thread: {e}"));
    }
    match rx.recv_timeout(deadline) {
        Ok(Ok(value)) => Guarded::Done(value),
        Ok(Err(panic)) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".into());
            Guarded::Crashed(what.chars().take(120).collect())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Guarded::TimedOut,
        // The thread ended without reporting: only an abort does that, and
        // an abort takes this process with it — kept for completeness.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Guarded::Crashed("the renderer stopped".into())
        }
    }
}

/// Render one unit of `kind` (`"math"`, else mermaid) under [`run_guarded`]
/// with [`RENDER_STACK_BYTES`] and [`RENDER_DEADLINE`]: what an SVG batch
/// should call per unit.
pub fn render_guarded(kind: &str, src: &str) -> Guarded<Option<String>> {
    let (math, src) = (kind == "math", src.to_string());
    run_guarded(RENDER_STACK_BYTES, RENDER_DEADLINE, move || {
        if math {
            math_svg(&src)
        } else {
            mermaid_svg(&src)
        }
    })
}

/// Add `data-clew-kind="{kind}"` to the root `<svg` element.
fn stamp(svg: &str, kind: &str) -> String {
    match svg.find("<svg") {
        Some(at) => {
            let insert = at + "<svg".len();
            format!(
                "{} {KIND_ATTR}=\"{kind}\"{}",
                &svg[..insert],
                &svg[insert..]
            )
        }
        None => svg.to_string(),
    }
}

/// Map mermaid-rs's default slate palette onto clew's active theme.
pub fn recolor_mermaid(svg: &str) -> String {
    // (slate default, dark target, light target): page bg, node fill, label
    // text, edges/arrowheads, node borders.
    const MAP: &[(&str, &str, &str)] = &[
        ("#FFFFFF", "#282c34", "#fafafa"),
        ("#F8FAFC", "#2d323c", "#eef0f3"),
        ("#0F172A", "#dfe4ec", "#383a42"),
        ("#64748B", "#7d8799", "#6b7280"),
        ("#94A3B8", "#565d6b", "#cfd3d9"),
    ];
    let light = crate::theme::is_light();
    let mut out = svg.to_string();
    for (from, dark, lt) in MAP {
        out = out.replace(from, if light { lt } else { dark });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mermaid renderer writes a font cache under `$XDG_CACHE_HOME` (else
    /// `$HOME/.cache`): every render test primes the suite's isolation first,
    /// which points that into the suite's temp directory.
    fn isolated() {
        crate::app::tests::isolated_data_dir();
    }

    #[test]
    fn math_renders_and_is_themeable() {
        isolated();
        let svg = math_svg(r"\frac{1}{2} + \sqrt{x}").expect("valid math renders");
        assert!(svg.contains("<svg"));
        assert!(
            svg.contains("currentColor"),
            "glyphs are recolorable, not baked black"
        );
        assert!(!svg.contains("rgba(0,0,0,1)"));
    }

    /// Source a renderer rejects comes back as `None` — the caller shows a
    /// failure placeholder — never as a panic or an empty SVG.
    #[test]
    fn unparseable_sources_are_none() {
        isolated();
        assert_eq!(math_svg(r"\frac{1}{"), None, "an unclosed group");
        assert_eq!(math_svg(r"\notacommand{x}"), None, "an unknown command");
        assert_eq!(mermaid_svg("this is not a diagram"), None);
    }

    /// Sources past the size or nesting limits never reach a renderer: deep
    /// nesting is what recurses deepest, and overflowing the stack aborts the
    /// process. Ordinary sources are untouched by the limits.
    #[test]
    fn oversized_or_deeply_nested_sources_are_refused() {
        isolated();
        let deep_tex = format!("{}x{}", "{".repeat(100_000), "}".repeat(100_000));
        assert_eq!(math_svg(&deep_tex), None);
        assert_eq!(math_svg(&"x+".repeat(MAX_TEX_BYTES)), None);
        let nested = format!("{}x{}", "\\left(".repeat(8), "\\right)".repeat(8));
        assert!(math_svg(&nested).is_some(), "ordinary nesting renders");
        // Escaped braces are text, not nesting.
        assert_eq!(bracket_depth(r"\{\{\{ a \}\}\}"), 0);
        assert_eq!(bracket_depth("f(g[h{x}])"), 3);

        let mut deep_graph = String::from("flowchart LR\n");
        for i in 0..MAX_MERMAID_DEPTH + 1 {
            deep_graph.push_str(&format!("subgraph s{i}\n"));
        }
        deep_graph.push_str("A --> B\n");
        for _ in 0..MAX_MERMAID_DEPTH + 1 {
            deep_graph.push_str("end\n");
        }
        assert_eq!(subgraph_depth(&deep_graph), MAX_MERMAID_DEPTH + 1);
        assert_eq!(mermaid_svg(&deep_graph), None);
        assert_eq!(
            mermaid_svg(&format!("flowchart LR\n{}", "A --> B\n".repeat(20_000))),
            None
        );
    }

    /// The guarded runner returns what the render returns, contains a panic,
    /// gives up on a render past its deadline (without waiting for it), and
    /// runs on a stack far deeper than a spawned thread's default.
    #[test]
    fn guarded_renders_contain_panics_hangs_and_deep_recursion() {
        isolated();
        assert_eq!(
            run_guarded(1 << 20, Duration::from_secs(5), || 7),
            Guarded::Done(7)
        );
        assert_eq!(
            run_guarded(1 << 20, Duration::from_secs(5), || -> u8 {
                panic!("bad diagram")
            }),
            Guarded::Crashed("bad diagram".into())
        );
        let started = std::time::Instant::now();
        let hung = run_guarded(1 << 20, Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_secs(30));
        });
        assert_eq!(hung, Guarded::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "did not wait it out"
        );

        // ~5k frames of >1 KiB: past a default 2 MiB thread stack, well
        // inside the guarded one.
        fn deep(n: usize) -> usize {
            let frame = std::hint::black_box([n as u8; 1024]);
            if n == 0 {
                frame[0] as usize
            } else {
                deep(n - 1) + frame[n % 1024] as usize
            }
        }
        assert!(matches!(
            run_guarded(RENDER_STACK_BYTES, Duration::from_secs(30), || deep(5_000)),
            Guarded::Done(_)
        ));
        // The per-unit entry point renders both kinds.
        assert!(matches!(
            render_guarded("math", "x^2"),
            Guarded::Done(Some(_))
        ));
        assert!(matches!(
            render_guarded("mermaid", "flowchart LR\n A --> B"),
            Guarded::Done(Some(_))
        ));
    }

    #[test]
    fn mermaid_renders_raw_then_recolors() {
        isolated();
        let svg = mermaid_svg("flowchart LR\n A[Start] --> B[End]").expect("valid mermaid renders");
        assert!(svg.contains("<svg"));
        // Raw output keeps the slate palette (theme-independent, cached as-is).
        let themed = recolor_mermaid(&svg);
        // Recoloring maps the slate node fill onto a theme target.
        assert!(!themed.contains("#F8FAFC"));
        assert!(themed.contains("#2d323c") || themed.contains("#eef0f3"));
    }

    /// Each SVG names its renderer, so a cached one is re-prepared as what it
    /// is — even a diagram that happens to use `currentColor`.
    #[test]
    fn renders_are_stamped_with_their_kind() {
        isolated();
        use crate::richmd::{RenderKind, svg_kind};
        let math = math_svg("x^2").expect("renders");
        let diagram = mermaid_svg("flowchart LR\n A --> B").expect("renders");
        assert_eq!(svg_kind(&math), RenderKind::Math);
        assert_eq!(svg_kind(&diagram), RenderKind::Mermaid);
        let tricky = diagram.replacen("</svg>", "<g fill=\"currentColor\"/></svg>", 1);
        assert!(tricky.contains("currentColor"));
        assert_eq!(svg_kind(&tricky), RenderKind::Mermaid);
        // The stamp does not disturb sizing: the root still parses.
        let prepared = crate::richmd::prepare_svg(&math, true);
        assert!(prepared.width > 1.0 && prepared.height > 1.0);
        // Legacy (unstamped) cache entries still classify.
        assert_eq!(
            svg_kind(r#"<svg viewBox="0 0 1 1"><path fill="currentColor"/></svg>"#),
            RenderKind::Math
        );
        assert_eq!(
            svg_kind(r#"<svg viewBox="0 0 1 1"><rect/></svg>"#),
            RenderKind::Mermaid
        );
    }
}
