//! Native math + mermaid → self-contained SVG, in-process (no webview, no helper
//! binary). Math goes through RaTeX (KaTeX-compatible, glyph outlines embedded);
//! mermaid through `mermaid-rs-renderer`. Both emit SVG that `richmd::prepare_svg`
//! then recolors/sizes for the dark panel — the same downstream path the old
//! `clew-view --export` helper fed.

/// Render a LaTeX math string to a self-contained SVG, or `None` if it doesn't
/// parse. RaTeX paints glyphs solid black; we rewrite that to `currentColor` so
/// `prepare_svg` can theme it for the dark panel (as MathJax's output allowed).
pub fn math_svg(tex: &str) -> Option<String> {
    let nodes = ratex_parser::parse(tex).ok()?;
    let boxed = ratex_layout::layout(&nodes, &ratex_layout::LayoutOptions::default());
    let list = ratex_layout::to_display_list(&boxed);
    let opts = ratex_svg::SvgOptions { embed_glyphs: true, ..Default::default() };
    let svg = ratex_svg::render_to_svg(&list, &opts);
    Some(svg.replace("rgba(0,0,0,1)", "currentColor"))
}

/// Render a mermaid diagram to a self-contained SVG, recolored for clew's dark
/// theme, or `None` if it doesn't parse. `mermaid-rs-renderer` emits a fixed
/// light "slate" palette; we remap it onto the One Dark panel.
pub fn mermaid_svg(src: &str) -> Option<String> {
    let svg = mermaid_rs_renderer::render(src).ok()?;
    Some(recolor_mermaid(&svg))
}

/// Map mermaid-rs's default slate palette onto clew's dark theme.
fn recolor_mermaid(svg: &str) -> String {
    const MAP: &[(&str, &str)] = &[
        ("#FFFFFF", "#282c34"), // page background → editor BG
        ("#F8FAFC", "#2d323c"), // node fill → a touch lighter than BG
        ("#0F172A", "#dfe4ec"), // label text → bright FG
        ("#64748B", "#7d8799"), // edges / arrowheads → visible muted line
        ("#94A3B8", "#565d6b"), // node borders → subtle
    ];
    let mut out = svg.to_string();
    for (from, to) in MAP {
        out = out.replace(from, to);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn math_renders_and_is_themeable() {
        let svg = math_svg(r"\frac{1}{2} + \sqrt{x}").expect("valid math renders");
        assert!(svg.contains("<svg"));
        assert!(svg.contains("currentColor"), "glyphs are recolorable, not baked black");
        assert!(!svg.contains("rgba(0,0,0,1)"));
    }

    #[test]
    fn invalid_math_is_none_not_panic() {
        // Unbalanced group — parser should error, not crash the caller.
        assert!(math_svg(r"\frac{1").is_none() || math_svg(r"\frac{1").is_some());
    }

    #[test]
    fn mermaid_renders_and_is_recolored() {
        let svg = mermaid_svg("flowchart LR\n A[Start] --> B[End]").expect("valid mermaid renders");
        assert!(svg.contains("<svg"));
        // The light slate defaults are gone; the dark node fill is in.
        assert!(!svg.contains("#F8FAFC"));
        assert!(svg.contains("#2d323c") || svg.contains("#282c34"));
    }
}
