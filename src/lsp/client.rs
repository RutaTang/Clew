//! A minimal async LSP client: spawns a server subprocess, speaks
//! `Content-Length`-framed JSON-RPC over stdio, and exposes the few requests
//! clew needs (initialize, didOpen, definition).
//!
//! Design: one *reader* task parses framed messages off stdout into a channel;
//! one *actor* task owns stdin plus the pending-request map and `select!`s
//! between outgoing requests from handles and incoming messages. `LspClient`
//! is a cheap, cloneable handle that talks to the actor over a channel, so it
//! can be moved into iced `Task`s freely.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// Most recent server output lines to retain for the management panel.
const MAX_LOGS: usize = 400;

/// One diagnostic (error/warning/…) at a position, in the server's encoding.
#[derive(Debug, Clone)]
pub struct Diag {
    pub line: usize,
    pub char_start: usize,
    pub char_end: usize,
    pub severity: u8, // 1 error, 2 warning, 3 info, 4 hint
    pub message: String,
}

/// Observable server state shared with the UI: logs, progress, diagnostics.
#[derive(Default)]
pub struct ServerState {
    pub logs: VecDeque<String>,
    /// Current work-done progress (e.g. "indexing 45%"), else `None`.
    pub progress: Option<String>,
    /// Latest diagnostics per file.
    pub diagnostics: HashMap<PathBuf, Vec<Diag>>,
    /// Bumped whenever diagnostics change, so the UI knows to refresh.
    pub diag_version: u64,
}

impl ServerState {
    fn push_log(&mut self, line: String) {
        if self.logs.len() >= MAX_LOGS {
            self.logs.pop_front();
        }
        self.logs.push_back(line);
    }
}

/// A resolved definition target.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,      // 0-based
    pub character: usize, // 0-based, in the negotiated encoding
}

/// A node in a call hierarchy: a callable plus the raw LSP `CallHierarchyItem`,
/// kept so it can be passed back to incoming/outgoing-calls requests.
#[derive(Debug, Clone)]
pub struct CallItem {
    pub name: String,
    pub detail: String,
    pub kind: u8, // LSP SymbolKind
    pub path: PathBuf,
    pub line: usize,      // selectionRange start (0-based) — the jump target
    pub character: usize, // 0-based, negotiated encoding
    pub raw: Value,       // the CallHierarchyItem, for incoming/outgoing params
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
    state: Arc<Mutex<ServerState>>,
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
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to launch {}: {e}", exe.display()))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        let state = Arc::new(Mutex::new(ServerState::default()));
        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();

        // Reader task: framed stdout → messages.
        tokio::spawn(reader_loop(BufReader::new(stdout), incoming_tx));
        // Stderr task: capture the server's log output.
        tokio::spawn(stderr_loop(BufReader::new(stderr), state.clone()));
        // Actor task: owns stdin + pending map, keeps the child alive.
        tokio::spawn(actor_loop(child, stdin, rx, incoming_rx, state.clone()));

        let client = Self {
            tx,
            next_id: Arc::new(AtomicI64::new(1)),
            state,
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

    /// Notify the server that an open document's full text changed on disk
    /// (whole-document sync). `version` must strictly increase per document.
    pub fn did_change(&self, path: &Path, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "version": version,
                },
                "contentChanges": [{ "text": text }],
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
        self.navigate("textDocument/definition", path, line, character)
            .await
    }

    /// Run any location-returning navigation request (definition, references,
    /// implementation, typeDefinition) and normalize the result to targets.
    pub async fn navigate(
        &self,
        method: &str,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Vec<Target>, String> {
        let mut params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        if method.ends_with("/references") {
            params["context"] = json!({ "includeDeclaration": false });
        }
        let result = self.call(method, params).await?;
        Ok(parse_definition(&result))
    }

    /// Request hover info at a 0-based (line, character); returns plain text.
    pub async fn hover(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Option<String>, String> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        let result = self.call("textDocument/hover", params).await?;
        Ok(parse_hover(&result))
    }

    /// Resolve the call-hierarchy item(s) at a position (the anchor for
    /// incoming/outgoing queries). Empty when the server lacks the capability.
    pub async fn prepare_call_hierarchy(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Vec<CallItem> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        match self.call("textDocument/prepareCallHierarchy", params).await {
            Ok(result) => parse_call_items(&result),
            Err(_) => Vec::new(),
        }
    }

    /// Callers of `item` (the raw `CallHierarchyItem`).
    pub async fn incoming_calls(&self, item: Value) -> Vec<CallItem> {
        match self.call("callHierarchy/incomingCalls", json!({ "item": item })).await {
            Ok(result) => parse_calls(&result, "from"),
            Err(_) => Vec::new(),
        }
    }

    /// Callees of `item` (the raw `CallHierarchyItem`).
    pub async fn outgoing_calls(&self, item: Value) -> Vec<CallItem> {
        match self.call("callHierarchy/outgoingCalls", json!({ "item": item })).await {
            Ok(result) => parse_calls(&result, "to"),
            Err(_) => Vec::new(),
        }
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

    /// A snapshot of the most recent server log lines.
    pub fn logs(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|s| s.logs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Current work-done progress line (e.g. indexing), if any.
    pub fn progress(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.progress.clone())
    }

    /// Diagnostics for a file (empty if none).
    pub fn diagnostics(&self, path: &Path) -> Vec<Diag> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.diagnostics.get(path).cloned())
            .unwrap_or_default()
    }

    /// Monotonic version bumped whenever any diagnostics change.
    pub fn diag_version(&self) -> u64 {
        self.state.lock().map(|s| s.diag_version).unwrap_or(0)
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

/// Stderr task: capture the server's log output into shared state.
async fn stderr_loop<R>(mut reader: BufReader<R>, state: Arc<Mutex<ServerState>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let trimmed = line.trim_end().to_string();
                if !trimmed.is_empty()
                    && let Ok(mut s) = state.lock()
                {
                    s.push_log(trimmed);
                }
            }
        }
    }
}

/// Actor task: owns stdin and the pending-request map.
async fn actor_loop<W>(
    mut child: tokio::process::Child,
    mut stdin: W,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    mut incoming: mpsc::UnboundedReceiver<Value>,
    state: Arc<Mutex<ServerState>>,
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
                    let id = value.get("id").and_then(Value::as_i64);
                    let method = value.get("method").and_then(Value::as_str);
                    match (id, method) {
                        // A response to one of our requests.
                        (Some(id), None) => {
                            if let Some(reply) = pending.remove(&id) {
                                if let Some(err) = value.get("error") {
                                    let _ = reply.send(Err(err.get("message")
                                        .and_then(Value::as_str).unwrap_or("error").to_string()));
                                } else {
                                    let _ = reply.send(Ok(value.get("result").cloned().unwrap_or(Value::Null)));
                                }
                            }
                        }
                        // A server→client request (e.g. workDoneProgress/create):
                        // we implement none, so acknowledge with a null result.
                        (Some(id), Some(_)) => {
                            let ack = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                            let _ = write_frame(&mut stdin, &ack).await;
                        }
                        // A server notification (logs, progress).
                        (None, Some(method)) => {
                            handle_notification(method, &value, &state);
                        }
                        (None, None) => {}
                    }
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

/// Fold a server notification into the shared state.
fn handle_notification(method: &str, value: &Value, state: &Arc<Mutex<ServerState>>) {
    let params = value.get("params");
    let Ok(mut s) = state.lock() else {
        return;
    };
    match method {
        "window/logMessage" | "window/showMessage" => {
            if let Some(msg) = params.and_then(|p| p.get("message")).and_then(Value::as_str) {
                s.push_log(msg.to_string());
            }
        }
        "textDocument/publishDiagnostics" => {
            let Some(uri) = params.and_then(|p| p.get("uri")).and_then(Value::as_str) else {
                return;
            };
            let Some(path) = uri_to_path(uri) else {
                return;
            };
            let diags = params
                .and_then(|p| p.get("diagnostics"))
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(parse_diag).collect())
                .unwrap_or_default();
            s.diagnostics.insert(path, diags);
            s.diag_version = s.diag_version.wrapping_add(1);
        }
        "$/progress" => {
            let value = params.and_then(|p| p.get("value"));
            let kind = value
                .and_then(|v| v.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match kind {
                "end" => s.progress = None,
                _ => {
                    let title = value
                        .and_then(|v| v.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("working");
                    let pct = value
                        .and_then(|v| v.get("percentage"))
                        .and_then(Value::as_u64);
                    s.progress = Some(match pct {
                        Some(p) => format!("{title} {p}%"),
                        None => title.to_string(),
                    });
                }
            }
        }
        _ => {}
    }
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

/// Parse one `CallHierarchyItem` into a `CallItem` (keeping the raw JSON).
fn parse_call_item(v: &Value) -> Option<CallItem> {
    let uri = v.get("uri").and_then(Value::as_str)?;
    // Jump to the name (selectionRange), not the whole definition range.
    let start = v
        .get("selectionRange")
        .or_else(|| v.get("range"))?
        .get("start")?;
    Some(CallItem {
        name: v.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
        detail: v.get("detail").and_then(Value::as_str).unwrap_or_default().to_string(),
        kind: v.get("kind").and_then(Value::as_u64).unwrap_or(0) as u8,
        path: uri_to_path(uri)?,
        line: start.get("line").and_then(Value::as_u64)? as usize,
        character: start.get("character").and_then(Value::as_u64)? as usize,
        raw: v.clone(),
    })
}

/// `prepareCallHierarchy` returns `CallHierarchyItem[] | null`.
fn parse_call_items(result: &Value) -> Vec<CallItem> {
    match result {
        Value::Array(items) => items.iter().filter_map(parse_call_item).collect(),
        _ => Vec::new(),
    }
}

/// `incomingCalls` returns `[{ from: item, .. }]`, `outgoingCalls` `[{ to: item }]`.
/// De-duplicates so a caller/callee that appears via several call sites is one row.
fn parse_calls(result: &Value, field: &str) -> Vec<CallItem> {
    let Value::Array(items) = result else {
        return Vec::new();
    };
    let mut out: Vec<CallItem> = Vec::new();
    for call in items {
        if let Some(item) = call.get(field).and_then(parse_call_item)
            && !out
                .iter()
                .any(|e| e.path == item.path && e.line == item.line && e.name == item.name)
        {
            out.push(item);
        }
    }
    out
}

/// Parse one LSP diagnostic. Multi-line diagnostics are clamped to the start
/// line for underlining.
fn parse_diag(v: &Value) -> Option<Diag> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let line = start.get("line").and_then(Value::as_u64)? as usize;
    let char_start = start.get("character").and_then(Value::as_u64)? as usize;
    let end_line = end.get("line").and_then(Value::as_u64).unwrap_or(line as u64) as usize;
    let char_end = if end_line == line {
        end.get("character").and_then(Value::as_u64).unwrap_or(char_start as u64 + 1) as usize
    } else {
        char_start + 1 // spans to next line; underline just the start
    };
    Some(Diag {
        line,
        char_start,
        char_end: char_end.max(char_start + 1),
        severity: v.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
        message: v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Extract plain text from a hover response (MarkupContent | MarkedString[]).
fn parse_hover(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let text = match contents {
        // MarkupContent { kind, value } or MarkedString { language, value }.
        Value::Object(o) => o.get("value").and_then(Value::as_str)?.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|e| {
                    e.get("value")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                        .map(str::to_string)
                })
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n")
        }
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
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

    #[test]
    fn parse_diagnostic() {
        let v = json!({
            "range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 13}},
            "severity": 1,
            "message": "cannot find value"
        });
        let d = parse_diag(&v).unwrap();
        assert_eq!((d.line, d.char_start, d.char_end, d.severity), (4, 8, 13, 1));
        assert!(d.message.contains("cannot find"));
    }

    #[test]
    fn parse_hover_variants() {
        // MarkupContent
        let v = json!({"contents": {"kind": "markdown", "value": "```rust\nfn origin() -> i32\n```"}});
        assert!(parse_hover(&v).unwrap().contains("origin"));
        // MarkedString array
        let v = json!({"contents": [{"language":"rust","value":"i32"}, "an integer"]});
        assert_eq!(parse_hover(&v).unwrap(), "i32\nan integer");
        // Plain string
        assert_eq!(parse_hover(&json!({"contents": "hi"})).unwrap(), "hi");
        // Empty / null
        assert!(parse_hover(&Value::Null).is_none());
        assert!(parse_hover(&json!({"contents": {"value": "  "}})).is_none());
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
