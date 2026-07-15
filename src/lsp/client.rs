//! A minimal async LSP client: spawns a server subprocess, speaks
//! `Content-Length`-framed JSON-RPC over stdio, and exposes the few requests
//! clew needs (initialize, didOpen, definition).
//!
//! Design: one *reader* task parses framed messages off stdout into a channel;
//! one *actor* task owns stdin plus the pending-request map and `select!`s
//! between outgoing requests from handles and incoming messages. `LspClient`
//! is a cheap, cloneable handle that talks to the actor over a channel, so it
//! can be moved into iced `Task`s freely.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// A resolved definition target.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,      // 0-based
    pub character: usize, // 0-based, in the negotiated encoding
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
}

/// Cheap, cloneable handle to a running server.
#[derive(Clone)]
pub struct LspClient {
    tx: mpsc::UnboundedSender<Outgoing>,
    next_id: Arc<AtomicI64>,
    pub encoding: PositionEncoding,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("encoding", &self.encoding)
            .field("alive", &!self.tx.is_closed())
            .finish()
    }
}

enum Outgoing {
    Call {
        id: i64,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Notify {
        method: String,
        params: Value,
    },
}

impl LspClient {
    /// Start a server and run the LSP `initialize` handshake.
    pub async fn start(
        exe: &Path,
        args: &[String],
        root: &Path,
        init_options: Option<Value>,
    ) -> Result<Self, String> {
        let mut child = Command::new(exe)
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to launch {}: {e}", exe.display()))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();

        // Reader task: framed stdout → messages.
        tokio::spawn(reader_loop(BufReader::new(stdout), incoming_tx));
        // Actor task: owns stdin + pending map, keeps the child alive.
        tokio::spawn(actor_loop(child, stdin, rx, incoming_rx));

        let client = Self {
            tx,
            next_id: Arc::new(AtomicI64::new(1)),
            encoding: PositionEncoding::Utf16,
        };

        let result = client.initialize(root, init_options).await?;
        let encoding = match result
            .get("capabilities")
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
        {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        client.notify("initialized", json!({}));
        Ok(Self { encoding, ..client })
    }

    async fn initialize(&self, root: &Path, init_options: Option<Value>) -> Result<Value, String> {
        let mut params = json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(root),
            "capabilities": {
                // Prefer utf-8 so our byte offsets map 1:1 to LSP positions.
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "textDocument": {
                    "definition": { "linkSupport": true }
                }
            },
            "clientInfo": { "name": "clew" }
        });
        if let Some(opts) = init_options {
            params["initializationOptions"] = opts;
        }
        self.call("initialize", params).await
    }

    /// Notify the server a document is open (send full text).
    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            }),
        );
    }

    /// Request the definition(s) at a 0-based (line, character) position.
    pub async fn definition(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Vec<Target>, String> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        let result = self.call("textDocument/definition", params).await?;
        Ok(parse_definition(&result))
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Outgoing::Call {
                id,
                method: method.to_string(),
                params,
                reply,
            })
            .map_err(|_| "server is not running".to_string())?;
        rx.await.map_err(|_| "server closed".to_string())?
    }

    fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(Outgoing::Notify {
            method: method.to_string(),
            params,
        });
    }
}

/// Reader task: parse `Content-Length` frames off stdout, forward each JSON.
async fn reader_loop<R>(mut reader: BufReader<R>, tx: mpsc::UnboundedSender<Value>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        // Read headers up to the blank line.
        let mut content_length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return, // EOF
                Ok(_) => {}
                Err(_) => return,
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break; // end of headers
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        if content_length == 0 {
            continue;
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&body)
            && tx.send(value).is_err()
        {
            return;
        }
    }
}

/// Actor task: owns stdin and the pending-request map.
async fn actor_loop<W>(
    mut child: tokio::process::Child,
    mut stdin: W,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    mut incoming: mpsc::UnboundedReceiver<Value>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut pending: HashMap<i64, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(Outgoing::Call { id, method, params, reply }) => {
                    let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                    if write_frame(&mut stdin, &msg).await.is_err() {
                        let _ = reply.send(Err("write failed".into()));
                    } else {
                        pending.insert(id, reply);
                    }
                }
                Some(Outgoing::Notify { method, params }) => {
                    let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
                    let _ = write_frame(&mut stdin, &msg).await;
                }
                None => break, // all handles dropped
            },
            msg = incoming.recv() => match msg {
                Some(value) => {
                    // Route responses (those carrying our id) to their waiter.
                    if let Some(id) = value.get("id").and_then(Value::as_i64)
                        && let Some(reply) = pending.remove(&id)
                    {
                        if let Some(err) = value.get("error") {
                            let _ = reply.send(Err(err.get("message")
                                .and_then(Value::as_str).unwrap_or("error").to_string()));
                        } else {
                            let _ = reply.send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
                        }
                    }
                    // Server requests/notifications are ignored (read-only client).
                }
                None => break, // server stdout closed
            },
        }
    }

    // Fail any still-pending requests and stop the child.
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err("server stopped".into()));
    }
    let _ = child.start_kill();
}

async fn write_frame<W>(stdin: &mut W, msg: &Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(msg).unwrap_or_default();
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await
}

/// Normalize a definition response (Location | Location[] | LocationLink[]).
fn parse_definition(result: &Value) -> Vec<Target> {
    fn one(v: &Value) -> Option<Target> {
        // LocationLink uses targetUri/targetSelectionRange; Location uses uri/range.
        let uri = v
            .get("uri")
            .or_else(|| v.get("targetUri"))
            .and_then(Value::as_str)?;
        let range = v
            .get("range")
            .or_else(|| v.get("targetSelectionRange"))
            .or_else(|| v.get("targetRange"))?;
        let start = range.get("start")?;
        Some(Target {
            path: uri_to_path(uri)?,
            line: start.get("line").and_then(Value::as_u64)? as usize,
            character: start.get("character").and_then(Value::as_u64)? as usize,
        })
    }
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Object(_) => one(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = if s.starts_with('/') { s } else { format!("/{s}") };
    // Percent-encode spaces and a few characters commonly found in paths.
    let encoded: String = s
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            _ => c.to_string(),
        })
        .collect();
    format!("file://{encoded}")
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Strip an optional authority (empty for local files).
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(path);
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = PathBuf::from("/Users/x/my code/main.rs");
        let uri = path_to_uri(&p);
        assert_eq!(uri, "file:///Users/x/my%20code/main.rs");
        assert_eq!(uri_to_path(&uri), Some(p));
    }

    #[test]
    fn parse_location_object() {
        let v = json!({
            "uri": "file:///a/b.rs",
            "range": { "start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 12} }
        });
        assert_eq!(
            parse_definition(&v),
            vec![Target { path: PathBuf::from("/a/b.rs"), line: 4, character: 8 }]
        );
    }

    #[test]
    fn parse_location_link_array() {
        let v = json!([{
            "targetUri": "file:///a/b.rs",
            "targetSelectionRange": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} },
            "targetRange": { "start": {"line": 0, "character": 0}, "end": {"line": 3, "character": 0} }
        }]);
        assert_eq!(parse_definition(&v)[0].line, 1);
        assert_eq!(parse_definition(&v)[0].character, 2);
    }

    #[test]
    fn parse_null_is_empty() {
        assert!(parse_definition(&Value::Null).is_empty());
    }

    /// Full protocol round-trip against a real rust-analyzer. Ignored by
    /// default (spawns an external dev tool); run explicitly to verify.
    #[tokio::test]
    #[ignore]
    async fn live_definition_against_rust_analyzer() {
        let exe = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".cargo/bin/rust-analyzer");
        assert!(exe.exists(), "needs rust-analyzer at {exe:?}");

        // Minimal cargo project: `origin()` defined and called.
        let root = std::env::temp_dir().join("clew-ra-live");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let src = "fn origin() -> i32 {\n    0\n}\n\nfn main() {\n    let _ = origin();\n}\n";
        let main_rs = root.join("src/main.rs");
        std::fs::write(&main_rs, src).unwrap();

        let client = LspClient::start(&exe, &[], &root, None).await.unwrap();
        eprintln!("negotiated encoding: {:?}", client.encoding);
        client.did_open(&main_rs, "rust", 1, src);

        // Line 5 (0-based), the call `origin()` — character 12 is inside it.
        // rust-analyzer indexes async; poll until it resolves.
        let mut targets = Vec::new();
        for _ in 0..40 {
            targets = client.definition(&main_rs, 5, 12).await.unwrap_or_default();
            if !targets.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        eprintln!("definition targets: {targets:?}");
        assert!(!targets.is_empty(), "expected a definition target");
        // Should point back to the `origin` definition on line 0.
        assert_eq!(targets[0].line, 0);
        assert!(targets[0].path.ends_with("src/main.rs"));
    }
}
