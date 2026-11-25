//! A web page's own server, for debugging the page in a browser: the
//! project's files over HTTP on 127.0.0.1, for as long as the debug session
//! holds it. A page opened as a `file://` URL cannot load its module scripts
//! (`<script type="module">`: its origin is `null`) nor anything by a root
//! path (`/app.js` is the disk's root), and most pages use one or the other.
//!
//! Only this computer reaches it, and only by the address it was given (a
//! request naming any other host is refused, so a web page elsewhere cannot
//! rebind a name of its own to it); it serves files under its folder alone,
//! none hidden (`.env`, `.git`), and lists no folder.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// A running page server; dropped, it stops.
pub struct PageServer {
    addr: SocketAddr,
    root: PathBuf,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PageServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageServer")
            .field("addr", &self.addr)
            .field("root", &self.root)
            .finish()
    }
}

impl PageServer {
    /// Serve `root`'s files on a free port of 127.0.0.1.
    pub fn start(root: &Path) -> std::io::Result<PageServer> {
        let root = root.canonicalize()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let serving = root.clone();
        let stopped = stop.clone();
        let accept = std::thread::Builder::new()
            .name("clew-page-server".into())
            .spawn(move || {
                while !stopped.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let root = serving.clone();
                            let _ = std::thread::Builder::new()
                                .name("clew-page-request".into())
                                .spawn(move || answer(stream, &root, addr));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(25));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(25)),
                    }
                }
            })?;
        Ok(PageServer {
            addr,
            root,
            stop,
            accept: Some(accept),
        })
    }

    /// The folder it serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The URL of `page`, a file under the folder it serves.
    pub fn url_of(&self, page: &Path) -> Option<String> {
        let page = page.canonicalize().ok()?;
        let rel = page.strip_prefix(&self.root).ok()?;
        let path: Vec<String> = rel
            .components()
            .map(|c| percent_encode(&c.as_os_str().to_string_lossy()))
            .collect();
        Some(format!("http://{}/{}", self.addr, path.join("/")))
    }
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// The page that loads `script` among the HTML files of its folder and the
/// folders above it (to `root`), and the folder to serve it from: the one a
/// root path in the page (`/js/app.js`) names the script from, else
/// `root`. `None` when no page loads it.
pub fn page_of_script(root: &Path, script: &Path) -> Option<(PathBuf, PathBuf)> {
    let script = script.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    let mut dir = script.parent()?.to_path_buf();
    let mut looked = 0;
    loop {
        let mut pages: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| {
                    e.eq_ignore_ascii_case("html") || e.eq_ignore_ascii_case("htm")
                })
            })
            .collect();
        // `index.html` first: the page a site opens on.
        pages.sort_by_key(|p| (p.file_stem().is_none_or(|s| s != "index"), p.clone()));
        for page in pages {
            looked += 1;
            if looked > 64 {
                return None;
            }
            let Some(text) = crate::statefile::read_capped(&page, 1024 * 1024) else {
                continue;
            };
            for import in crate::imports::imports_of(&text, "html") {
                let src = import.module.split(['?', '#']).next().unwrap_or("");
                if src.is_empty() || src.starts_with("//") || src.contains(':') {
                    continue;
                }
                let served = match src.strip_prefix('/') {
                    // `/js/app.js`: served from where that path ends at
                    // the script, if the page is under it.
                    Some(path) => {
                        let mut base = script.clone();
                        let parts = Path::new(path).components().count();
                        for _ in 0..parts {
                            base.pop();
                        }
                        (normal(&base.join(path)) == script && page.starts_with(&base))
                            .then_some(base)
                    }
                    None => (normal(&dir.join(src)) == script).then(|| root.clone()),
                };
                if let Some(served) = served {
                    return Some((page, served));
                }
            }
        }
        if dir == root || !dir.pop() || !dir.starts_with(&root) {
            return None;
        }
    }
}

/// The folder to serve `page` (an HTML file under `root`) from: the one
/// its root paths (`/js/app.js`) name files from — its own folder or one
/// above it — else `root`.
pub fn served_root_of_page(root: &Path, page: &Path) -> PathBuf {
    let (Ok(root), Ok(page)) = (root.canonicalize(), page.canonicalize()) else {
        return root.to_path_buf();
    };
    let Some(text) = crate::statefile::read_capped(&page, 1024 * 1024) else {
        return root;
    };
    for import in crate::imports::imports_of(&text, "html") {
        let src = import.module.split(['?', '#']).next().unwrap_or("");
        let Some(path) = src.strip_prefix('/').filter(|p| !p.starts_with('/')) else {
            continue;
        };
        let mut base = page.parent().map(Path::to_path_buf).unwrap_or_default();
        while base.starts_with(&root) {
            if base.join(path).is_file() {
                return base;
            }
            if !base.pop() {
                break;
            }
        }
    }
    root
}

/// `path` with its `.` and `..` parts worked out, by the path alone.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Answer one request: the file its path names, or why not.
fn answer(stream: TcpStream, root: &Path, addr: SocketAddr) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(&stream);
    let mut head = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        head.push(line.trim_end().to_string());
        if head.len() > 100 {
            return;
        }
    }
    let mut out = &stream;
    let Some(request) = head.first() else {
        return;
    };
    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    // Asked for by this address alone: a name of another site's that
    // resolves here (DNS rebinding) is not this page's server.
    let host = head
        .iter()
        .skip(1)
        .find_map(|h| {
            let (name, value) = h.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("host")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    let ours = [addr.to_string(), format!("localhost:{}", addr.port())];
    if !ours.iter().any(|o| o.eq_ignore_ascii_case(&host)) {
        let _ = respond(
            &mut out,
            403,
            "text/plain; charset=utf-8",
            b"Forbidden",
            true,
        );
        return;
    }
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        let _ = respond(
            &mut out,
            405,
            "text/plain; charset=utf-8",
            b"Method Not Allowed",
            true,
        );
        return;
    }
    let Some(file) = file_for(root, target) else {
        let _ = respond(
            &mut out,
            404,
            "text/plain; charset=utf-8",
            b"Not Found",
            !head_only,
        );
        return;
    };
    let Ok(mut opened) = std::fs::File::open(&file) else {
        let _ = respond(
            &mut out,
            404,
            "text/plain; charset=utf-8",
            b"Not Found",
            !head_only,
        );
        return;
    };
    let len = opened.metadata().map(|m| m.len()).unwrap_or(0);
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        content_type(&file)
    );
    if out.write_all(header.as_bytes()).is_err() || head_only {
        return;
    }
    let _ = std::io::copy(&mut (&mut opened).take(len), &mut out);
}

fn respond(
    out: &mut impl Write,
    status: u16,
    kind: &str,
    body: &[u8],
    with_body: bool,
) -> std::io::Result<()> {
    let reason = match status {
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    if with_body {
        out.write_all(body)?;
    }
    Ok(())
}

/// The file a request's path names under `root`: a folder's `index.html`;
/// nothing outside it (by `..` or a link), nothing hidden.
fn file_for(root: &Path, target: &str) -> Option<PathBuf> {
    let path = target.split(['?', '#']).next()?.strip_prefix('/')?;
    let decoded = percent_decode(path)?;
    let mut file = root.to_path_buf();
    for part in decoded.split('/').filter(|p| !p.is_empty()) {
        if part.starts_with('.') || part.contains('\\') || part.contains('\0') {
            return None;
        }
        file.push(part);
    }
    if file.is_dir() {
        file.push("index.html");
    }
    let file = file.canonicalize().ok()?;
    (file.starts_with(root) && file.is_file()).then_some(file)
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A file's `Content-Type`, by its extension, as browsers need it: a module
/// script must be served as JavaScript.
fn content_type(file: &Path) -> &'static str {
    let ext = file
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(addr: &str, path: &str, host: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").unwrap();
        let mut text = String::new();
        stream.read_to_string(&mut text).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let kind = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Type: "))
            .unwrap_or("")
            .to_string();
        (status, kind, body.to_string())
    }

    fn site() -> PathBuf {
        let root = std::env::temp_dir().join(format!("clew-page-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("js")).unwrap();
        std::fs::create_dir_all(root.join("about")).unwrap();
        std::fs::write(
            root.join("index.html"),
            "<script type=\"module\" src=\"/js/app.js\"></script>",
        )
        .unwrap();
        std::fs::write(root.join("js/app.js"), "export const a = 1;\n").unwrap();
        std::fs::write(root.join("about/index.html"), "<h1>about</h1>").unwrap();
        std::fs::write(root.join("my page.html"), "<p>spaced</p>").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1\n").unwrap();
        root
    }

    /// The page and its module script, by the types a browser needs; a
    /// folder by its index; nothing hidden, above it, or asked for by
    /// another host.
    #[test]
    fn serves_a_sites_files_and_nothing_else() {
        let root = site();
        let server = PageServer::start(&root).unwrap();
        let addr = server.addr.to_string();
        let page = server.url_of(&root.join("index.html")).unwrap();
        assert_eq!(page, format!("http://{addr}/index.html"));
        let (status, kind, body) = get(&addr, "/index.html", &addr);
        assert_eq!((status, kind.as_str()), (200, "text/html; charset=utf-8"));
        assert!(body.contains("/js/app.js"));
        let (status, kind, _) = get(&addr, "/js/app.js?v=2", &addr);
        assert_eq!(
            (status, kind.as_str()),
            (200, "text/javascript; charset=utf-8")
        );
        assert_eq!(get(&addr, "/about/", &addr).2, "<h1>about</h1>");
        assert_eq!(get(&addr, "/my%20page.html", &addr).2, "<p>spaced</p>");
        assert_eq!(
            server.url_of(&root.join("my page.html")).unwrap(),
            format!("http://{addr}/my%20page.html")
        );
        assert_eq!(get(&addr, "/.env", &addr).0, 404);
        assert_eq!(get(&addr, "/../etc/passwd", &addr).0, 404);
        assert_eq!(get(&addr, "/js/%2e%2e/%2e%2e/x", &addr).0, 404);
        assert_eq!(get(&addr, "/missing.js", &addr).0, 404);
        let localhost = format!("localhost:{}", server.addr.port());
        assert_eq!(get(&addr, "/index.html", &localhost).0, 200);
        assert_eq!(get(&addr, "/index.html", "evil.example:80").0, 403);
        drop(server);
        assert!(TcpStream::connect(&addr).is_err(), "stopped when dropped");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A script is known by the page that loads it — by a root path, from
    /// the folder that path starts at; by a relative one, from the
    /// project's — and a script no page loads is none's.
    #[test]
    fn the_page_a_script_is_loaded_by() {
        let root = std::env::temp_dir().join(format!("clew-page-of-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("site/js")).unwrap();
        std::fs::create_dir_all(root.join("tools")).unwrap();
        std::fs::write(
            root.join("site/index.html"),
            "<!doctype html><script type=\"module\" src=\"/js/app.js\"></script>",
        )
        .unwrap();
        std::fs::write(root.join("site/js/app.js"), "").unwrap();
        std::fs::write(
            root.join("page.html"),
            "<link rel=\"stylesheet\" href=\"main.css\"><script src=\"./tools/util.js\"></script>",
        )
        .unwrap();
        std::fs::write(root.join("tools/util.js"), "").unwrap();
        std::fs::write(root.join("tools/cli.js"), "").unwrap();
        let canon = |p: &str| root.join(p).canonicalize().unwrap();
        assert_eq!(
            page_of_script(&root, &root.join("site/js/app.js")),
            Some((canon("site/index.html"), canon("site")))
        );
        assert_eq!(
            page_of_script(&root, &root.join("tools/util.js")),
            Some((canon("page.html"), canon("")))
        );
        assert_eq!(page_of_script(&root, &root.join("tools/cli.js")), None);
        // A page debugged itself: served from where its root paths start.
        assert_eq!(
            served_root_of_page(&root, &root.join("site/index.html")),
            canon("site")
        );
        assert_eq!(
            served_root_of_page(&root, &root.join("page.html")),
            canon("")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
