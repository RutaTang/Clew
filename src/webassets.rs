//! Provisions the JS libraries that render an explanation as a rich HTML page
//! (markdown + math + mermaid diagrams) into clew's global data dir.
//!
//! Downloads are checksum-verified over HTTPS — the same trust model as the LSP
//! server store — so the rendered page references only local, verified files
//! (no CDN, no subresource-integrity gap) and works offline after the first
//! fetch. MathJax is used in SVG mode, which embeds glyph paths and so needs no
//! separate font files.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

struct Asset {
    file: &'static str,
    url: &'static str,
    sha256: &'static str,
}

const ASSETS: &[Asset] = &[
    Asset {
        file: "marked.min.js",
        url: "https://cdn.jsdelivr.net/npm/marked@12.0.2/marked.min.js",
        sha256: "15fabce5b65898b32b03f5ed25e9f891a729ad4c0d6d877110a7744aa847a894",
    },
    Asset {
        file: "purify.min.js",
        url: "https://cdn.jsdelivr.net/npm/dompurify@3.1.6/dist/purify.min.js",
        sha256: "c0845096a7c4a6741f362ac506c94c1c7d27dc603bcc1bf64a587f76f2dbe3a1",
    },
    Asset {
        file: "tex-svg.js",
        url: "https://cdn.jsdelivr.net/npm/mathjax@3.2.2/es5/tex-svg.js",
        sha256: "d4295dc33744836935c1399feece5159577b34c5c8ffb9f1c6324cd82e03a882",
    },
    Asset {
        file: "mermaid.min.js",
        url: "https://cdn.jsdelivr.net/npm/mermaid@10.9.1/dist/mermaid.min.js",
        sha256: "61b335a46df05a7ce1c98378f60e5f3e77a7fb608a1056997e8a649304a936d6",
    },
];

fn dir() -> Option<PathBuf> {
    Some(crate::lsp::store::data_root()?.join("webassets"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn digest_matches(path: &Path, sha256: &str) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    hex(&Sha256::digest(&bytes)) == sha256
}

fn download(asset: &Asset, dest: &Path) -> Result<(), String> {
    if !asset.url.starts_with("https://") {
        return Err("refusing non-HTTPS asset URL".into());
    }
    let resp = ureq::get(asset.url)
        .call()
        .map_err(|e| format!("download {}: {e}", asset.file))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", asset.file))?;
    let got = hex(&Sha256::digest(&buf));
    if got != asset.sha256 {
        return Err(format!("{}: checksum mismatch (got {got})", asset.file));
    }
    let tmp = dest.with_extension("tmp");
    std::fs::write(&tmp, &buf).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    Ok(())
}

/// Ensure every render asset is present and verified, downloading what's
/// missing. Returns the assets directory. Blocking (network) — run off-thread.
pub fn ensure() -> Result<PathBuf, String> {
    let dir = dir().ok_or("no data directory")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for asset in ASSETS {
        let path = dir.join(asset.file);
        if path.exists() && digest_matches(&path, asset.sha256) {
            continue;
        }
        download(asset, &path)?;
    }
    Ok(dir)
}

/// Provision assets, build a self-contained page, write it beside the assets,
/// and open it in clew's own embedded webview window (the `clew-view` helper
/// binary) — never the user's browser. Blocking — run off-thread.
pub fn render_and_show(title: &str, markdown: &str) -> Result<(), String> {
    let dir = ensure()?;
    let scripts = inline_scripts(&dir)?;
    let html = render_page(title, markdown, &scripts);
    let page = dir.join("explanation.html");
    std::fs::write(&page, &html).map_err(|e| format!("write page: {e}"))?;

    let viewer = viewer_bin()
        .ok_or("clew-view renderer not found next to clew (rebuild to install it)")?;
    // Both args are values clew controls (a path under the data dir, a label),
    // passed as argv (no shell) — not a URL handed to a general-purpose opener.
    std::process::Command::new(&viewer)
        .arg(&page)
        .arg(title)
        .spawn()
        .map_err(|e| format!("launch clew-view: {e}"))?;
    Ok(())
}

/// Locate the sibling `clew-view` binary next to the running executable.
fn viewer_bin() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(target_os = "windows") {
        "clew-view.exe"
    } else {
        "clew-view"
    };
    let path = exe.parent()?.join(name);
    path.exists().then_some(path)
}

/// Read the verified asset files and concatenate them into inline `<script>`
/// blocks. Inlining (rather than `src=`) keeps the page a single self-contained
/// document, which the webview can load via `with_html` with no local-file
/// origin or subresource concerns. Any `</script` inside the library source is
/// neutralized so it cannot close the block early.
fn inline_scripts(dir: &Path) -> Result<String, String> {
    let mut out = String::new();
    // MathJax must be configured before its script runs.
    out.push_str(
        "<script>window.MathJax={tex:{inlineMath:[['$','$']],\
         displayMath:[['$$','$$']]},svg:{fontCache:'global'}};</script>\n",
    );
    for asset in ASSETS {
        let path = dir.join(asset.file);
        let js = std::fs::read_to_string(&path)
            .map_err(|e| format!("read {}: {e}", asset.file))?;
        out.push_str("<script>");
        out.push_str(&guard_script(&js));
        out.push_str("</script>\n");
    }
    Ok(out)
}

/// Prevent an embedded `</script>` (any case) from prematurely closing the
/// inline block that wraps the library source.
fn guard_script(js: &str) -> String {
    let bytes = js.as_bytes();
    let mut out = String::with_capacity(js.len());
    let mut last = 0;
    let mut i = 0;
    // "<" and "/script" are all ASCII, so every index below lands on a UTF-8
    // char boundary and the copied slices stay valid.
    while i + 8 <= bytes.len() {
        if bytes[i] == b'<' && bytes[i + 1..i + 8].eq_ignore_ascii_case(b"/script") {
            out.push_str(&js[last..i]);
            out.push_str("<\\/script");
            i += 8;
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&js[last..]);
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A self-contained page (all scripts inlined, no remote or local references)
/// that renders `markdown` with full markdown, KaTeX-quality math (MathJax SVG),
/// and mermaid diagrams. The markdown is injected as a JS string and
/// DOMPurify-sanitized before insertion, so LLM output cannot inject script.
/// `scripts` is the inline `<script>` block from [`inline_scripts`].
fn render_page(title: &str, markdown: &str, scripts: &str) -> String {
    let md = serde_json::to_string(markdown).unwrap_or_else(|_| "\"\"".into());
    let title = html_escape(title);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>{title}</title>
{scripts}<style>
body{{background:#282c34;color:#abb2bf;font:15px/1.65 -apple-system,system-ui,sans-serif;max-width:820px;margin:40px auto;padding:0 24px}}
h1,h2,h3,h4{{color:#e5c07b;line-height:1.3}} h1{{border-bottom:1px solid #3a3f4b;padding-bottom:.3em}}
code{{background:#21252b;padding:1px 5px;border-radius:4px;color:#98c379;font-size:.9em}}
pre{{background:#21252b;padding:14px;border-radius:8px;overflow:auto}} pre code{{background:none;padding:0}}
a{{color:#61afef}} blockquote{{border-left:3px solid #3a3f4b;margin:0;padding-left:1em;color:#828b98}}
.mermaid{{background:#21252b;border-radius:8px;padding:14px;text-align:center}}
table{{border-collapse:collapse}} td,th{{border:1px solid #3a3f4b;padding:4px 10px}}
</style></head>
<body><h1>{title}</h1><div id="content"></div>
<script>
window.addEventListener('load', function() {{
  var md = {md};
  var content = document.getElementById('content');
  content.innerHTML = DOMPurify.sanitize(marked.parse(md));
  content.querySelectorAll('code.language-mermaid').forEach(function(el) {{
    var div = document.createElement('div');
    div.className = 'mermaid';
    div.textContent = el.textContent;
    (el.closest('pre') || el).replaceWith(div);
  }});
  if (window.mermaid) {{
    mermaid.initialize({{startOnLoad:false, securityLevel:'strict', theme:'dark'}});
    mermaid.run({{querySelector:'.mermaid'}});
  }}
  if (window.MathJax && MathJax.typesetPromise) {{ MathJax.typesetPromise([content]); }}
}});
</script></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_self_contained_and_sanitizes() {
        let scripts = "<script>window.__marked=1;</script>\n";
        let html = render_page(
            "theme.rs",
            "## Hi\n$E=mc^2$\n```mermaid\ngraph TD; A-->B\n```",
            scripts,
        );
        // Scripts are inlined, not referenced — no src= and no remote URL.
        assert!(html.contains("window.__marked=1"));
        assert!(!html.contains("src="), "scripts must be inlined, not linked");
        assert!(!html.contains("https://"), "no remote refs in the page");
        // Markdown is JSON-escaped into the page, and sanitized before insertion.
        assert!(html.contains("E=mc^2"));
        assert!(html.contains("DOMPurify.sanitize"));
    }

    #[test]
    fn guard_script_neutralizes_closing_tag() {
        // A </script> inside library source must not close the inline block.
        let guarded = guard_script("a='</script>'; b='</SCRIPT >';");
        assert!(!guarded.contains("</script"));
        assert!(!guarded.contains("</SCRIPT"));
        assert!(guarded.contains("<\\/script"));
        // Non-ASCII content survives intact.
        assert_eq!(guard_script("café → ☕"), "café → ☕");
    }
}
