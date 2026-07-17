//! A minimal async DAP client: spawns a debug-adapter subprocess, speaks
//! `Content-Length`-framed JSON over stdio (identical framing to
//! [`crate::lsp::client`]), and exposes the requests clew's debugger drives —
//! initialize, launch, breakpoints, stepping, stack/scopes/variables, evaluate.
//!
//! Design mirrors the LSP client: a *reader* task parses framed messages off
//! stdout; an *actor* task owns stdin plus the pending-request map (keyed by DAP
//! `seq`) and `select!`s between outgoing requests and incoming messages.
//! Adapter **events** (stopped, output, terminated…) are forwarded to an
//! unbounded channel the app runs as an iced `Task`, so they land as `Message`s.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::proto::{Breakpoint, DapEvent, Scope, StackFrame, Variable};

/// An outgoing DAP request awaiting its response `body`.
struct Outgoing {
    seq: i64,
    command: String,
    arguments: Value,
    reply: oneshot::Sender<Result<Value, String>>,
}

/// A cheap, cloneable handle to the debug adapter; talks to the actor over a
/// channel so it can be moved into iced `Task`s freely.
#[derive(Clone, Debug)]
pub struct DapClient {
    tx: mpsc::UnboundedSender<Outgoing>,
    next_seq: Arc<AtomicI64>,
}

impl DapClient {
    /// Spawn the adapter and return the handle plus the stream of adapter events.
    /// Does *not* run the handshake — the caller drives initialize → launch →
    /// (on the `Initialized` event) setBreakpoints + configurationDone, because
    /// that ordering is event-driven.
    pub async fn start(
        adapter: &Path,
        args: &[String],
        cwd: &Path,
    ) -> Result<(DapClient, mpsc::UnboundedReceiver<DapEvent>), String> {
        let mut child = Command::new(adapter)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to launch adapter {}: {e}", adapter.display()))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<DapEvent>();

        tokio::spawn(reader_loop(BufReader::new(stdout), incoming_tx));
        tokio::spawn(actor_loop(child, stdin, rx, incoming_rx, event_tx));

        let client = DapClient { tx, next_seq: Arc::new(AtomicI64::new(1)) };
        Ok((client, event_rx))
    }

    /// Send a DAP request and await its response `body` (or the failure message).
    async fn request(&self, command: &str, arguments: Value) -> Result<Value, String> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Outgoing { seq, command: command.to_string(), arguments, reply })
            .map_err(|_| "debug adapter not running".to_string())?;
        rx.await.map_err(|_| "debug adapter closed".to_string())?
    }

    /// Fire a request without awaiting its response — used for `launch`, whose
    /// response the adapter defers until after `configurationDone`.
    fn send_nowait(&self, command: &str, arguments: Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (reply, _rx) = oneshot::channel();
        let _ = self.tx.send(Outgoing { seq, command: command.to_string(), arguments, reply });
    }

    pub async fn initialize(&self) -> Result<Value, String> {
        self.request(
            "initialize",
            json!({
                "adapterID": "clew", "clientID": "clew", "clientName": "clew",
                "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
                "supportsRunInTerminalRequest": false,
            }),
        )
        .await
    }

    /// Launch the program with adapter-specific arguments. Fire-and-forget: the
    /// launch response arrives after we send `configurationDone` (driven by the
    /// `Initialized` event), so awaiting it here would stall the handshake.
    pub fn launch(&self, launch_args: Value) {
        self.send_nowait("launch", launch_args);
    }

    /// Set all breakpoints for one source file (replaces the file's set). Each
    /// breakpoint is `(line, optional condition)`; a condition-only-stops when
    /// the adapter evaluates it to true.
    pub async fn set_breakpoints(
        &self,
        source: &Path,
        lines: &[(usize, Option<String>)],
    ) -> Result<Vec<Breakpoint>, String> {
        let bps: Vec<Value> = lines
            .iter()
            .map(|(l, cond)| match cond {
                Some(c) => json!({ "line": l, "condition": c }),
                None => json!({ "line": l }),
            })
            .collect();
        let body = self
            .request(
                "setBreakpoints",
                json!({ "source": { "path": source.to_string_lossy() }, "breakpoints": bps }),
            )
            .await?;
        Ok(body
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(Breakpoint::from_value).collect())
            .unwrap_or_default())
    }

    pub async fn configuration_done(&self) -> Result<(), String> {
        self.request("configurationDone", json!({})).await.map(|_| ())
    }

    pub async fn continue_(&self, thread_id: i64) -> Result<(), String> {
        self.request("continue", json!({ "threadId": thread_id })).await.map(|_| ())
    }
    pub async fn next(&self, thread_id: i64) -> Result<(), String> {
        self.request("next", json!({ "threadId": thread_id })).await.map(|_| ())
    }
    pub async fn step_in(&self, thread_id: i64) -> Result<(), String> {
        self.request("stepIn", json!({ "threadId": thread_id })).await.map(|_| ())
    }
    pub async fn step_out(&self, thread_id: i64) -> Result<(), String> {
        self.request("stepOut", json!({ "threadId": thread_id })).await.map(|_| ())
    }

    pub async fn stack_trace(&self, thread_id: i64) -> Result<Vec<StackFrame>, String> {
        let body = self.request("stackTrace", json!({ "threadId": thread_id })).await?;
        Ok(body
            .get("stackFrames")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(StackFrame::from_value).collect())
            .unwrap_or_default())
    }

    pub async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, String> {
        let body = self.request("scopes", json!({ "frameId": frame_id })).await?;
        Ok(body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Scope::from_value).collect())
            .unwrap_or_default())
    }

    pub async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, String> {
        let body = self
            .request("variables", json!({ "variablesReference": variables_reference }))
            .await?;
        Ok(body
            .get("variables")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Variable::from_value).collect())
            .unwrap_or_default())
    }

    /// Evaluate an expression in a frame's context; returns the result string.
    pub async fn evaluate(&self, expression: &str, frame_id: i64) -> Result<String, String> {
        let body = self
            .request(
                "evaluate",
                json!({ "expression": expression, "frameId": frame_id, "context": "watch" }),
            )
            .await?;
        Ok(body.get("result").and_then(Value::as_str).unwrap_or("").to_string())
    }

    /// End the session and kill the debuggee.
    pub async fn disconnect(&self) -> Result<(), String> {
        self.request("disconnect", json!({ "terminateDebuggee": true })).await.map(|_| ())
    }
}

/// Reader task: parse `Content-Length` frames off stdout, forward each JSON.
async fn reader_loop<R>(mut reader: BufReader<R>, tx: mpsc::UnboundedSender<Value>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let mut content_length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
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

/// Actor task: owns stdin and the pending-request map (keyed by DAP `seq`).
async fn actor_loop<W>(
    mut child: tokio::process::Child,
    mut stdin: W,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    mut incoming: mpsc::UnboundedReceiver<Value>,
    events: mpsc::UnboundedSender<DapEvent>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut pending: HashMap<i64, oneshot::Sender<Result<Value, String>>> = HashMap::new();

    loop {
        tokio::select! {
            out = outgoing.recv() => match out {
                Some(Outgoing { seq, command, arguments, reply }) => {
                    let msg = json!({
                        "seq": seq, "type": "request", "command": command, "arguments": arguments
                    });
                    if write_frame(&mut stdin, &msg).await.is_err() {
                        let _ = reply.send(Err("write failed".into()));
                    } else {
                        pending.insert(seq, reply);
                    }
                }
                None => break, // all handles dropped
            },
            msg = incoming.recv() => match msg {
                Some(value) => {
                    match value.get("type").and_then(Value::as_str) {
                        Some("response") => {
                            let req_seq = value.get("request_seq").and_then(Value::as_i64);
                            if let Some(reply) = req_seq.and_then(|s| pending.remove(&s)) {
                                let ok = value.get("success").and_then(Value::as_bool).unwrap_or(false);
                                if ok {
                                    let _ = reply.send(Ok(value.get("body").cloned().unwrap_or(Value::Null)));
                                } else {
                                    let m = value.get("message").and_then(Value::as_str).unwrap_or("request failed");
                                    let _ = reply.send(Err(m.to_string()));
                                }
                            }
                        }
                        Some("event") => {
                            if let Some(name) = value.get("event").and_then(Value::as_str) {
                                let body = value.get("body").cloned().unwrap_or(Value::Null);
                                if events.send(DapEvent::parse(name, &body)).is_err() {
                                    break; // app dropped the receiver
                                }
                            }
                        }
                        // A reverse request (runInTerminal / startDebugging): clew
                        // doesn't implement these, so decline so the adapter never
                        // hangs waiting (console-mode launches never send one).
                        Some("request") => {
                            if let Some(seq) = value.get("seq").and_then(Value::as_i64) {
                                let cmd = value.get("command").and_then(Value::as_str).unwrap_or("");
                                let resp = json!({
                                    "type": "response", "request_seq": seq, "success": false,
                                    "command": cmd, "message": "unsupported by clew"
                                });
                                let _ = write_frame(&mut stdin, &resp).await;
                            }
                        }
                        _ => {}
                    }
                }
                None => break, // adapter stdout closed
            },
        }
    }

    for (_, reply) in pending.drain() {
        let _ = reply.send(Err("debug adapter stopped".into()));
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
