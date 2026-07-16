//! clew's embedded explanation renderer.
//!
//! A tiny standalone window that clew spawns to display a rendered explanation
//! page (markdown + KaTeX-quality math + mermaid diagrams) in a real webview —
//! WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux — instead of
//! handing the file off to the user's browser. The page is fully self-contained
//! (every script inlined and checksum-verified by clew before it gets here), so
//! this process only needs to read the file and load its contents.
//!
//! Usage: `clew-view <html-file> [window-title]`
//!
//! It lives in its own binary because AppKit/GTK demand ownership of the main
//! thread's event loop, which the main `clew` process already gives to iced;
//! running the webview in a separate process sidesteps that conflict entirely.

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("clew-view: missing HTML file argument");
        std::process::exit(2);
    };
    let title = args.next().unwrap_or_else(|| "clew".to_string());

    let html = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("clew-view: cannot read {path}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run(&title, html) {
        eprintln!("clew-view: {e}");
        std::process::exit(1);
    }
}

fn run(title: &str, html: String) -> wry::Result<()> {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(title)
        .with_inner_size(LogicalSize::new(1000.0, 760.0))
        .build(&event_loop)
        .expect("build window");

    let builder = WebViewBuilder::new().with_html(html);

    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "ios"))]
    let _webview = builder.build(&window)?;
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios")))]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().unwrap();
        builder.build_gtk(vbox)?
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}
