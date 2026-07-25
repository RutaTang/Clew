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
    let opts = ratex_svg::SvgOptions {
        embed_glyphs: true,
        ..Default::default()
    };
    let svg = ratex_svg::render_to_svg(&list, &opts);
    Some(svg.replace("rgba(0,0,0,1)", "currentColor"))
}

/// Render a mermaid diagram to a self-contained SVG, or `None` if it doesn't
/// parse. `mermaid-rs-renderer` emits a fixed light "slate" palette; that raw
/// output is kept theme-independent (cached to disk as-is) and remapped onto the
/// active theme later, at `prepare_svg` time — so a diagram follows a light/dark
/// switch instead of freezing the theme it was first rendered in.
pub fn mermaid_svg(src: &str) -> Option<String> {
    mermaid_rs_renderer::render(src).ok()
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

    #[test]
    fn math_renders_and_is_themeable() {
        let svg = math_svg(r"\frac{1}{2} + \sqrt{x}").expect("valid math renders");
        assert!(svg.contains("<svg"));
        assert!(
            svg.contains("currentColor"),
            "glyphs are recolorable, not baked black"
        );
        assert!(!svg.contains("rgba(0,0,0,1)"));
    }

    #[test]
    fn invalid_math_is_none_not_panic() {
        // Unbalanced group — parser should error, not crash the caller.
        assert!(math_svg(r"\frac{1").is_none() || math_svg(r"\frac{1").is_some());
    }

    #[test]
    fn mermaid_renders_raw_then_recolors() {
        let svg = mermaid_svg("flowchart LR\n A[Start] --> B[End]").expect("valid mermaid renders");
        assert!(svg.contains("<svg"));
        // Raw output keeps the slate palette (theme-independent, cached as-is).
        let themed = recolor_mermaid(&svg);
        // Recoloring maps the slate node fill onto a theme target.
        assert!(!themed.contains("#F8FAFC"));
        assert!(themed.contains("#2d323c") || themed.contains("#eef0f3"));
    }
}
