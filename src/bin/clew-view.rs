//! clew's out-of-process web helper.
//!
//! `clew-view --export <req.json> <resp.json> <assets_dir>` runs **headless**:
//! it drives MathJax + mermaid.js in an off-screen WebKit webview to turn a list
//! of math/mermaid blocks into SVG strings, which clew then shows inline in its
//! explanation modal via iced's `svg` widget.
//!
//! It lives in its own binary because AppKit/GTK demand ownership of the main
//! thread's event loop, which the main `clew` process already gives to iced.

use tao::dpi::LogicalSize;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("--export") => run_export(&args),
        _ => Err("usage: clew-view --export <req.json> <resp.json> <assets_dir>".to_string()),
    };
    if let Err(e) = result {
        eprintln!("clew-view: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Headless SVG export
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize, serde::Serialize)]
struct ReqBlock {
    // A string, not a number: the cache key is a full u64 that would lose
    // precision as a JavaScript `Number` (f64) when round-tripped through the page.
    id: String,
    kind: String, // "math" | "mermaid"
    src: String,
    #[serde(default)]
    display: bool,
}

/// A message from the page: the JSON results, or a fatal error string.
enum ExportEvent {
    Done(String),
    Timeout,
}

fn run_export(args: &[String]) -> Result<(), String> {
    let req_path = args.get(1).ok_or("export: missing request path")?;
    let resp_path = args.get(2).ok_or("export: missing response path")?.clone();
    let assets_dir = args.get(3).ok_or("export: missing assets dir")?;

    let req = std::fs::read_to_string(req_path).map_err(|e| format!("read request: {e}"))?;
    let blocks: Vec<ReqBlock> =
        serde_json::from_str(&req).map_err(|e| format!("parse request: {e}"))?;
    let blocks_json = serde_json::to_string(&blocks).map_err(|e| e.to_string())?;

    let dir = std::path::Path::new(assets_dir);
    let tex = read_guarded(&dir.join("tex-svg.js"))?;
    let mermaid = read_guarded(&dir.join("mermaid.min.js"))?;
    let html = export_html(&tex, &mermaid, &blocks_json);

    let event_loop: EventLoop<ExportEvent> = EventLoopBuilder::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Off-screen so no window ever flashes, but still realized so mermaid's
    // text measurement (getBBox) works.
    let window = WindowBuilder::new()
        .with_visible(false)
        .with_inner_size(LogicalSize::new(1400.0, 900.0))
        .build(&event_loop)
        .map_err(|e| format!("build window: {e}"))?;

    let ipc_proxy = proxy.clone();
    let builder = WebViewBuilder::new().with_html(html).with_ipc_handler(move |req| {
        let _ = ipc_proxy.send_event(ExportEvent::Done(req.into_body()));
    });

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "ios"))]
    let _webview = builder.build(&window).map_err(|e| format!("build webview: {e}"))?;
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder
            .build_gtk(window.default_vbox().unwrap())
            .map_err(|e| format!("build webview: {e}"))?
    };

    // Fail open after a hard deadline so clew never blocks forever.
    let timeout_proxy = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(25));
        let _ = timeout_proxy.send_event(ExportEvent::Timeout);
    });

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(ExportEvent::Done(json)) => {
                let _ = std::fs::write(&resp_path, &json);
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(ExportEvent::Timeout) => {
                if !std::path::Path::new(&resp_path).exists() {
                    let _ = std::fs::write(&resp_path, "[]");
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

/// The self-contained page that renders every requested block to SVG and posts
/// the results back over the IPC bridge.
fn export_html(tex_svg: &str, mermaid: &str, blocks_json: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<script>window.MathJax={{tex:{{inlineMath:[['$','$']],displayMath:[['$$','$$']]}},svg:{{fontCache:'local'}}}};</script>
<script>{tex_svg}</script>
<script>{mermaid}</script>
<script>window.__blocks={blocks_json};</script>
</head><body><div id="sink" style="position:absolute;left:-9999px"></div>
<script>
async function run() {{
  await MathJax.startup.promise;
  mermaid.initialize({{startOnLoad:false, securityLevel:'strict', theme:'dark', flowchart:{{htmlLabels:false}}}});
  var out = [];
  for (var i = 0; i < window.__blocks.length; i++) {{
    var b = window.__blocks[i];
    try {{
      if (b.kind === 'math') {{
        var node = MathJax.tex2svg(b.src, {{display: b.display}});
        var svg = node.querySelector('svg');
        out.push({{id: b.id, svg: svg.outerHTML}});
      }} else {{
        var res = await mermaid.render('mm' + b.id, b.src);
        out.push({{id: b.id, svg: res.svg}});
      }}
    }} catch (e) {{
      out.push({{id: b.id, error: String((e && e.message) || e)}});
    }}
  }}
  window.ipc.postMessage(JSON.stringify(out));
}}
window.addEventListener('load', function() {{
  run().catch(function(e) {{ window.ipc.postMessage(JSON.stringify([{{error: String(e)}}])); }});
}});
</script></body></html>"#
    )
}

/// Read a JS library file and neutralize any embedded `</script>` so it can be
/// inlined into a `<script>` block.
fn read_guarded(path: &std::path::Path) -> Result<String, String> {
    let js = std::fs::read_to_string(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let bytes = js.as_bytes();
    let mut out = String::with_capacity(js.len());
    let mut last = 0;
    let mut i = 0;
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
    Ok(out)
}

