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
//! That channel carries the debuggee's whole stdout, so it is metered by
//! [`EventSink`] — see there for why it is metered rather than bounded.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use super::proto::{Breakpoint, DapEvent, Output, Scope, StackFrame, Variable};

/// How the adapter speaks DAP: over its own stdio, or over a TCP socket it
/// listens on (js-debug, dlv) that clew connects to after the process starts.
#[derive(Debug, Clone, Copy)]
pub enum Transport {
    Stdio,
    Tcp(u16),
}

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
        transport: Transport,
    ) -> Result<(DapClient, DapEvents), String> {
        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = event_channel();

        match transport {
            Transport::Stdio => {
                let mut child = Command::new(adapter)
                    .args(args)
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("failed to launch adapter {}: {e}", adapter.display()))?;
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                // Drain stderr: an adapter that logs enough to fill the pipe
                // would otherwise block on write and wedge the session.
                if let Some(e) = child.stderr.take() {
                    tokio::spawn(drain(e));
                }
                tokio::spawn(reader_loop(BufReader::new(stdout), incoming_tx));
                tokio::spawn(actor_loop(Some(child), stdin, rx, incoming_rx, event_tx));
            }
            Transport::Tcp(port) => {
                // The adapter listens on `port`; spawn it, drain its stdio, then
                // connect a socket and speak DAP over it.
                let mut child = Command::new(adapter)
                    .args(args)
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| format!("failed to launch adapter {}: {e}", adapter.display()))?;
                if let Some(o) = child.stdout.take() {
                    tokio::spawn(drain(o));
                }
                if let Some(e) = child.stderr.take() {
                    tokio::spawn(drain(e));
                }
                // The child is already running: a failed connect must kill
                // it, not leave an orphaned adapter listening forever.
                let stream = match connect_retry(port).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = child.start_kill();
                        return Err(e);
                    }
                };
                let (read, write) = stream.into_split();
                tokio::spawn(reader_loop(BufReader::new(read), incoming_tx));
                tokio::spawn(actor_loop(Some(child), write, rx, incoming_rx, event_tx));
            }
        }

        let client = DapClient {
            tx,
            next_seq: Arc::new(AtomicI64::new(1)),
        };
        Ok((client, event_rx))
    }

    /// Connect to a debug adapter over a provided stdio transport — its process
    /// is owned elsewhere (proxied by clew-server), so the adapter runs where the
    /// code lives. No local child; same DAP wire format over the bridge.
    pub async fn connect<R, W>(stdin: W, stdout: R) -> Result<(DapClient, DapEvents), String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = event_channel();
        tokio::spawn(reader_loop(BufReader::new(stdout), incoming_tx));
        tokio::spawn(actor_loop(None, stdin, rx, incoming_rx, event_tx));
        let client = DapClient {
            tx,
            next_seq: Arc::new(AtomicI64::new(1)),
        };
        Ok((client, event_rx))
    }

    /// Open a *child* session on an already-running TCP adapter (js-debug asks
    /// the client to start one per debuggee target). No process is spawned; the
    /// parent session owns the adapter.
    pub async fn connect_tcp(port: u16) -> Result<(DapClient, DapEvents), String> {
        let (tx, rx) = mpsc::unbounded_channel::<Outgoing>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<Value>();
        let (event_tx, event_rx) = event_channel();
        let stream = connect_retry(port).await?;
        let (read, write) = stream.into_split();
        tokio::spawn(reader_loop(BufReader::new(read), incoming_tx));
        tokio::spawn(actor_loop(None, write, rx, incoming_rx, event_tx));
        let client = DapClient {
            tx,
            next_seq: Arc::new(AtomicI64::new(1)),
        };
        Ok((client, event_rx))
    }

    /// Ceiling for one DAP request. Generous — attaching or launching a big
    /// debuggee takes seconds — but bounded: an adapter that never answers
    /// must not leave the session (and whatever awaits it) hung forever.
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

    /// Send a DAP request and await its response `body` (or the failure message).
    async fn request(&self, command: &str, arguments: Value) -> Result<Value, String> {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Outgoing {
                seq,
                command: command.to_string(),
                arguments,
                reply,
            })
            .map_err(|_| "debug adapter not running".to_string())?;
        match tokio::time::timeout(Self::REQUEST_TIMEOUT, rx).await {
            Ok(reply) => reply.map_err(|_| "debug adapter closed".to_string())?,
            Err(_) => Err(format!("debug adapter did not answer '{command}' in time")),
        }
    }

    /// Fire a request without awaiting its response — used for `launch`, whose
    /// response the adapter defers until after `configurationDone`.
    fn send_nowait(&self, command: &str, arguments: Value) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (reply, _rx) = oneshot::channel();
        let _ = self.tx.send(Outgoing {
            seq,
            command: command.to_string(),
            arguments,
            reply,
        });
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
    ///
    /// The answer is returned PAIRED with the requested lines: the caller needs
    /// to know which of its own lines the adapter refused or moved, because the
    /// gutter is drawn from clew's map and not from this response.
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
        let answers = body
            .get("breakpoints")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        // DAP pairs the response array with the request array element for
        // element. `zip` stops at the shorter side on purpose: an adapter that
        // answers with fewer entries leaves the surplus lines UNREPORTED, which
        // is recoverable, rather than shifting answers onto the wrong lines,
        // which would have the caller mark a good breakpoint as rejected.
        Ok(lines
            .iter()
            .zip(answers)
            .map(|((line, _cond), v)| Breakpoint::from_value(*line, v))
            .collect())
    }

    pub async fn configuration_done(&self) -> Result<(), String> {
        self.request("configurationDone", json!({}))
            .await
            .map(|_| ())
    }

    pub async fn continue_(&self, thread_id: i64) -> Result<(), String> {
        self.request("continue", json!({ "threadId": thread_id }))
            .await
            .map(|_| ())
    }
    pub async fn next(&self, thread_id: i64) -> Result<(), String> {
        self.request("next", json!({ "threadId": thread_id }))
            .await
            .map(|_| ())
    }
    pub async fn step_in(&self, thread_id: i64) -> Result<(), String> {
        self.request("stepIn", json!({ "threadId": thread_id }))
            .await
            .map(|_| ())
    }
    pub async fn step_out(&self, thread_id: i64) -> Result<(), String> {
        self.request("stepOut", json!({ "threadId": thread_id }))
            .await
            .map(|_| ())
    }

    /// Most frames kept from one stop. DAP treats an absent `levels` as "every
    /// frame", and a debugger is reached for precisely when a program has
    /// recursed away — an exhausted 8 MiB native stack is tens of thousands of
    /// frames. The panel builds several widgets per frame with no
    /// virtualization and re-lays them out on every redraw and every step, so
    /// the whole stack is what freezes the window, not the bytes on the wire.
    ///
    /// This bounds what clew KEEPS, and (plus one probe frame) what it asks
    /// for. It does not bound the response: an adapter that ignores `levels`
    /// still sends the full stack, capped only by [`MAX_FRAME_BYTES`] at the
    /// reader.
    const MAX_STACK_FRAMES: usize = 512;

    /// Fetch the innermost [`Self::MAX_STACK_FRAMES`] frames of a thread's call
    /// stack. When frames are left out, the returned vector ends with one
    /// [`StackFrame::elided`] marker row, so the panel shows a PARTIAL stack as
    /// partial instead of passing it off as the whole thing.
    ///
    /// Frames come innermost-first, so what is cut is always the deepest part,
    /// and everything the app consumes off this vector — the top frame's
    /// scopes, the innermost frame that has source, the Ask prompt's first
    /// eight — lives at the head.
    pub async fn stack_trace(&self, thread_id: i64) -> Result<Vec<StackFrame>, String> {
        let body = self
            .request(
                "stackTrace",
                json!({
                    "threadId": thread_id,
                    "startFrame": 0,
                    // One PAST the cap on purpose: `totalFrames` is optional, so
                    // asking for exactly the cap and getting exactly the cap
                    // back would be indistinguishable from a stack that ends
                    // there, and the truncation would go unannounced.
                    "levels": Self::MAX_STACK_FRAMES + 1,
                }),
            )
            .await?;
        let returned = body.get("stackFrames").and_then(Value::as_array);
        let sent = returned.map(Vec::len).unwrap_or(0);
        // `levels` is a request, not a contract: truncate as well, so a
        // non-conforming adapter cannot get past the render cap either.
        let mut frames: Vec<StackFrame> = returned
            .map(|a| {
                a.iter()
                    .filter_map(StackFrame::from_value)
                    .take(Self::MAX_STACK_FRAMES)
                    .collect()
            })
            .unwrap_or_default();
        // How many were left out is only knowable from `totalFrames`; the probe
        // frame above proves that some WERE, which is the part that must never
        // go unsaid.
        let total = body
            .get("totalFrames")
            .and_then(Value::as_u64)
            .map(|t| t as usize);
        let cut = sent > frames.len() || total.is_some_and(|t| t > frames.len());
        let remaining = total
            .filter(|t| *t > frames.len())
            .map(|t| t - frames.len());
        // Never on an empty stack: the marker would be `frames.first()`, which
        // callers take to be the frame execution is stopped in.
        if cut && !frames.is_empty() {
            frames.push(StackFrame::elided(remaining));
        }
        Ok(frames)
    }

    pub async fn scopes(&self, frame_id: i64) -> Result<Vec<Scope>, String> {
        let body = self
            .request("scopes", json!({ "frameId": frame_id }))
            .await?;
        Ok(body
            .get("scopes")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Scope::from_value).collect())
            .unwrap_or_default())
    }

    pub async fn variables(&self, variables_reference: i64) -> Result<Vec<Variable>, String> {
        let body = self
            .request(
                "variables",
                json!({ "variablesReference": variables_reference }),
            )
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
        Ok(body
            .get("result")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// End the session and kill the debuggee.
    pub async fn disconnect(&self) -> Result<(), String> {
        self.request("disconnect", json!({ "terminateDebuggee": true }))
            .await
            .map(|_| ())
    }
}

/// Largest single `output` chunk kept from an adapter. A debuggee that writes
/// megabytes in one `write` arrives as ONE output event, and every cap around
/// it — the queue budget below, the panel's retained tail — is sized in whole
/// chunks, so the chunk itself has to be cut down at ingest or it slips past
/// all of them.
const MAX_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;

/// Total output bytes allowed to sit in the event queue at once, i.e. produced
/// by the adapter but not yet taken by the app's event pump. A debuggee can
/// write far faster than the UI folds its output, and the queue is unbounded,
/// so without this it grows until the machine gives out.
const MAX_PENDING_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

/// The app's end of the adapter's event stream, plus the accounting that pairs
/// with [`EventSink`]: whatever `recv` hands out is no longer queued, so its
/// bytes are credited back here.
///
/// `recv` is cancel-safe: its only await is the channel's own cancel-safe
/// `recv`. The session loop selects it against the Stop flag, so a poll that
/// loses that race must not consume or mis-credit an event.
pub struct DapEvents {
    rx: mpsc::UnboundedReceiver<DapEvent>,
    pending: Arc<AtomicUsize>,
}

impl DapEvents {
    pub async fn recv(&mut self) -> Option<DapEvent> {
        let ev = self.rx.recv().await?;
        if let DapEvent::Output(o) = &ev {
            self.pending.fetch_sub(o.text.len(), Ordering::Relaxed);
        }
        Some(ev)
    }
}

/// The actor's end. Output is charged against [`MAX_PENDING_OUTPUT_BYTES`] and
/// DROPPED (counted, then reported) when the app is too far behind; every other
/// event is queued unconditionally, because losing a `stopped` or `terminated`
/// would leave the session's state permanently wrong.
///
/// Metered rather than bounded on purpose: a bounded channel would park the
/// actor on a full queue, and the actor is also what drains `incoming` and
/// answers requests. An adapter blocked writing its own stdout stops reading
/// its stdin, so a parked actor's next `write_frame` never completes — the
/// session wedges instead of merely dropping output.
struct EventSink {
    tx: mpsc::UnboundedSender<DapEvent>,
    pending: Arc<AtomicUsize>,
    /// Output bytes discarded since the last time we managed to say so.
    dropped: usize,
}

impl EventSink {
    /// Budget held back so the drop notice below — itself charged, one short
    /// line — can always be queued without taking the queue over the cap.
    const NOTICE_RESERVE: usize = 256;

    /// Forward one event. `Err(())` means the app dropped the receiver.
    fn send(&mut self, event: DapEvent) -> Result<(), ()> {
        let event = match event {
            DapEvent::Output(o) => {
                let text = truncate_chunk(o.text);
                if self.pending.load(Ordering::Relaxed) + text.len() + Self::NOTICE_RESERVE
                    > MAX_PENDING_OUTPUT_BYTES
                {
                    // Silent loss would read as a debuggee that stopped
                    // printing; hold the count until there is room to say it.
                    self.dropped += text.len();
                    return Ok(());
                }
                DapEvent::Output(Output {
                    category: o.category,
                    text,
                })
            }
            other => other,
        };
        // Ahead of the event that made room, so the notice lands in the output
        // where the gap actually is. Terminated/Exited come through here too,
        // so a run that ends while over budget still reports its losses.
        if self.dropped > 0 {
            let n = std::mem::take(&mut self.dropped);
            self.queue(DapEvent::Output(Output {
                category: "console".into(),
                text: format!("[clew] {n} bytes of output dropped — the debuggee outran the UI\n"),
            }))?;
        }
        self.queue(event)
    }

    /// Queue an event, charging any output text so `DapEvents::recv` credits
    /// back exactly what was charged (an unpaired credit would underflow the
    /// counter and wedge every later chunk as over-budget).
    fn queue(&self, event: DapEvent) -> Result<(), ()> {
        if let DapEvent::Output(o) = &event {
            self.pending.fetch_add(o.text.len(), Ordering::Relaxed);
        }
        self.tx.send(event).map_err(|_| ())
    }
}

/// The event channel plus the byte accounting its two ends share.
fn event_channel() -> (EventSink, DapEvents) {
    let (tx, rx) = mpsc::unbounded_channel::<DapEvent>();
    let pending = Arc::new(AtomicUsize::new(0));
    (
        EventSink {
            tx,
            pending: pending.clone(),
            dropped: 0,
        },
        DapEvents { rx, pending },
    )
}

/// Cut one output chunk to [`MAX_OUTPUT_CHUNK_BYTES`], keeping the head and
/// saying how much went missing. The cut backs up to a char boundary because
/// the text is UTF-8 the panel renders directly.
fn truncate_chunk(text: String) -> String {
    if text.len() <= MAX_OUTPUT_CHUNK_BYTES {
        return text;
    }
    let mut cut = MAX_OUTPUT_CHUNK_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let dropped = text.len() - cut;
    // A fresh, right-sized buffer rather than `String::truncate`: truncating in
    // place keeps the original allocation, so a 64 MiB chunk would go on
    // costing 64 MiB while every byte count here saw 64 KiB.
    let mut out = String::with_capacity(cut + 64);
    out.push_str(&text[..cut]);
    out.push_str(&format!("\n[clew] …{dropped} bytes truncated\n"));
    out
}

/// Longest header line an adapter may send; longer is garbage or a memory
/// balloon, and reading on can only desync the stream.
const MAX_HEADER_BYTES: u64 = 8 * 1024;
/// Largest message body accepted from an adapter — `Content-Length` used to
/// be allocated verbatim, so one bogus header could demand gigabytes.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Reader task: parse `Content-Length` frames off stdout, forward each JSON.
/// Oversized headers or bodies end the stream (fail closed): after refusing a
/// frame there is no way back into sync.
async fn reader_loop<R>(mut reader: BufReader<R>, tx: mpsc::UnboundedSender<Value>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    loop {
        let mut content_length = 0usize;
        let mut line = String::new();
        loop {
            line.clear();
            match (&mut reader)
                .take(MAX_HEADER_BYTES)
                .read_line(&mut line)
                .await
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if !line.ends_with('\n') && line.len() as u64 >= MAX_HEADER_BYTES {
                return; // header line over the cap: the stream is garbage
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
        if content_length > MAX_FRAME_BYTES {
            return; // refuse to allocate; skipping would desync anyway
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

/// Actor task: owns the write half and the pending-request map (keyed by DAP
/// `seq`). `child` is the adapter process to kill on exit; `None` for a child
/// session that shares its parent's already-running adapter (js-debug).
async fn actor_loop<W>(
    child: Option<tokio::process::Child>,
    mut stdin: W,
    mut outgoing: mpsc::UnboundedReceiver<Outgoing>,
    mut incoming: mpsc::UnboundedReceiver<Value>,
    mut events: EventSink,
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
                        // A reverse request from the adapter. `startDebugging`
                        // (js-debug's multi-session model) is acknowledged and
                        // forwarded so the app opens the child session; everything
                        // else is declined so the adapter never hangs waiting.
                        Some("request") => {
                            if let Some(seq) = value.get("seq").and_then(Value::as_i64) {
                                let cmd = value.get("command").and_then(Value::as_str).unwrap_or("");
                                let is_start = cmd == "startDebugging";
                                let resp = json!({
                                    "type": "response", "request_seq": seq, "success": is_start,
                                    "command": cmd,
                                    "message": if is_start { Value::Null } else { "unsupported by clew".into() },
                                });
                                let _ = write_frame(&mut stdin, &resp).await;
                                if is_start {
                                    let config = value
                                        .get("arguments")
                                        .and_then(|a| a.get("configuration"))
                                        .cloned()
                                        .unwrap_or(Value::Null);
                                    if events.send(DapEvent::StartDebugging(config)).is_err() {
                                        break;
                                    }
                                }
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
    if let Some(mut c) = child {
        let _ = c.start_kill();
    }
}

/// Connect to a TCP adapter, retrying while it starts up (up to ~6s).
async fn connect_retry(port: u16) -> Result<TcpStream, String> {
    let addr = format!("127.0.0.1:{port}");
    for _ in 0..60 {
        if let Ok(s) = TcpStream::connect(&addr).await {
            return Ok(s);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("could not connect to debug adapter on {addr}"))
}

/// Consume a child's stdout/stderr so its pipe buffer never fills and blocks it.
async fn drain<R>(mut r: R)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = [0u8; 2048];
    while let Ok(n) = r.read(&mut buf).await {
        if n == 0 {
            break;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A client wired to a scripted adapter over in-memory pipes: `respond`
    /// answers each request `(command, arguments)` with a response `body`. Uses
    /// the real [`reader_loop`]/[`write_frame`], so requests go out and answers
    /// come back over genuine `Content-Length` framing.
    async fn scripted_adapter<F>(respond: F) -> (DapClient, DapEvents)
    where
        F: Fn(&str, &Value) -> Value + Send + 'static,
    {
        let (to_adapter, from_client) = tokio::io::duplex(1 << 16);
        let (mut to_client, from_adapter) = tokio::io::duplex(1 << 16);
        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(reader_loop(BufReader::new(from_client), req_tx));
        tokio::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let cmd = req
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = req.get("arguments").cloned().unwrap_or(Value::Null);
                let resp = json!({
                    "type": "response",
                    "request_seq": req.get("seq").and_then(Value::as_i64).unwrap_or(0),
                    "success": true,
                    "command": cmd,
                    "body": respond(&cmd, &args),
                });
                if write_frame(&mut to_client, &resp).await.is_err() {
                    break;
                }
            }
        });
        DapClient::connect(to_adapter, from_adapter).await.unwrap()
    }

    /// Record every request the client sends, so a test can assert on the wire
    /// arguments and not just on the parsed answer.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    /// `stackTrace` used to send no `levels`, which DAP defines as "every
    /// frame", and every frame returned was kept and rendered — so one stop in
    /// deep recursion handed the panel tens of thousands of rows and froze the
    /// window. The request is now windowed, the parse is capped regardless of
    /// what the adapter honours, and the frames left out are STATED rather than
    /// silently dropped (a stack that just ends looks like a shallow stack).
    #[tokio::test]
    async fn deep_stack_is_capped_and_the_remainder_is_reported() {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let rec = seen.clone();
        let cap = DapClient::MAX_STACK_FRAMES;
        let (client, _events) = scripted_adapter(move |cmd, args| {
            rec.lock().unwrap().push((cmd.to_string(), args.clone()));
            // An adapter that ignores `levels` (they are advisory), on a thread
            // that recursed ten times deeper still.
            let frames: Vec<Value> = (0..cap * 3)
                .map(|i| json!({ "id": i, "name": format!("f{i}"), "line": 1, "column": 2 }))
                .collect();
            json!({ "stackFrames": frames, "totalFrames": cap * 10 })
        })
        .await;

        let frames = client.stack_trace(7).await.unwrap();

        let (cmd, args) = seen.lock().unwrap()[0].clone();
        assert_eq!(cmd, "stackTrace");
        assert_eq!(
            args.get("levels").and_then(Value::as_u64),
            Some(cap as u64 + 1),
            "the request must ask for a bounded window (plus the probe frame), \
             not the whole stack"
        );

        assert_eq!(
            frames.len(),
            cap + 1,
            "cap plus one marker row, whatever the adapter sends"
        );
        assert_eq!(frames[0].name, "f0", "frames are kept innermost-first");
        assert_eq!(frames[cap - 1].name, format!("f{}", cap - 1));
        let marker = frames.last().unwrap();
        assert!(
            marker.name.contains(&(cap * 10 - cap).to_string()),
            "the marker must count against totalFrames, got {:?}",
            marker.name
        );
        assert!(
            marker.path.is_none(),
            "the marker must carry no source: that is what keeps the panel from \
             making it clickable and from treating it as a stop location"
        );
    }

    /// `totalFrames` is optional in DAP. An adapter that honours `levels` and
    /// omits it would, if we asked for exactly the cap, return exactly the cap
    /// and be indistinguishable from a stack that genuinely ends there — the
    /// truncation would go unannounced. The request asks for one frame PAST the
    /// cap to detect that, and the marker names no count, because the only
    /// number available here (one) is not how many frames are really below.
    #[tokio::test]
    async fn truncation_is_announced_even_when_the_adapter_omits_total_frames() {
        let cap = DapClient::MAX_STACK_FRAMES;
        let (client, _events) = scripted_adapter(move |_cmd, args| {
            let want = args.get("levels").and_then(Value::as_u64).unwrap_or(0) as usize;
            let frames: Vec<Value> = (0..want)
                .map(|i| json!({ "id": i, "name": format!("f{i}"), "line": 1, "column": 2 }))
                .collect();
            json!({ "stackFrames": frames })
        })
        .await;

        let frames = client.stack_trace(1).await.unwrap();
        assert_eq!(frames.len(), cap + 1);
        let marker = frames.last().unwrap();
        assert_eq!(marker.name, "… more frames not shown");
        assert!(marker.path.is_none());
    }

    /// A stack that fits is passed through untouched — no marker row, or the
    /// panel would claim frames were hidden when none were.
    #[tokio::test]
    async fn complete_stack_gains_no_marker() {
        let (client, _events) = scripted_adapter(|_cmd, _args| {
            json!({
                "stackFrames": [
                    { "id": 1, "name": "main", "line": 3, "column": 1 },
                    { "id": 2, "name": "start", "line": 9, "column": 1 },
                ],
                "totalFrames": 2,
            })
        })
        .await;
        let frames = client.stack_trace(1).await.unwrap();
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| !f.name.contains("not shown")));
    }

    /// `setBreakpoints` verification was parsed and thrown away, so the gutter
    /// kept a solid dot on lines the adapter had refused or bound elsewhere.
    /// The answer now comes back paired with the line clew asked for, which is
    /// the only way a caller can tell WHICH of its own breakpoints went wrong.
    #[tokio::test]
    async fn breakpoint_refusals_and_relocations_come_back_paired() {
        let (client, _events) = scripted_adapter(|_cmd, _args| {
            json!({ "breakpoints": [
                // Refused: a blank line / code that was optimized away.
                { "verified": false, "message": "no locations found" },
                // Bound, but the adapter slid it to the next statement.
                { "verified": true, "line": 42 },
                // Bound exactly where it was asked for.
                { "verified": true, "line": 90 },
            ]})
        })
        .await;

        let out = client
            .set_breakpoints(
                Path::new("/p/a.rs"),
                &[(10, None), (40, None), (90, Some("i > 2".into()))],
            )
            .await
            .unwrap();

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].requested_line, 10);
        assert!(!out[0].verified, "the refusal must survive to the caller");
        assert_eq!(out[0].message.as_deref(), Some("no locations found"));
        assert_eq!(
            out[0].relocated_to(),
            None,
            "a refusal is not a move: nothing to redraw elsewhere"
        );

        assert_eq!(out[1].requested_line, 40);
        assert!(out[1].verified);
        assert_eq!(
            out[1].relocated_to(),
            Some(42),
            "the adapter's own line must reach the caller, not the requested one"
        );

        assert_eq!(out[2].requested_line, 90);
        assert_eq!(out[2].relocated_to(), None, "bound where it was asked");
    }

    /// A non-conforming adapter that answers with fewer entries than requested
    /// must leave the surplus lines unreported, never shift answers onto the
    /// wrong ones — a misaligned pairing would mark a live breakpoint rejected.
    #[tokio::test]
    async fn short_breakpoint_response_is_not_shifted_onto_other_lines() {
        let (client, _events) =
            scripted_adapter(|_cmd, _args| json!({ "breakpoints": [{ "verified": false }] })).await;
        let out = client
            .set_breakpoints(Path::new("/p/a.rs"), &[(10, None), (20, None), (30, None)])
            .await
            .unwrap();
        assert_eq!(out.len(), 1, "unanswered lines stay unanswered");
        assert_eq!(out[0].requested_line, 10);
    }

    fn output(text: String) -> DapEvent {
        DapEvent::Output(Output {
            category: "stdout".into(),
            text,
        })
    }

    /// The event channel is unbounded, so a debuggee that outruns the app's
    /// event pump used to grow it without limit — one adapter frame may carry
    /// 64 MiB, and nothing between the two ends counted bytes at all. Now the
    /// queue is metered: an oversized chunk is cut at ingest, output over the
    /// budget is dropped rather than queued, and the drop is REPORTED (silent
    /// loss reads as a debuggee that simply stopped printing).
    #[tokio::test]
    async fn queued_output_stays_under_budget_and_says_what_it_dropped() {
        let (mut sink, mut events) = event_channel();
        // 4 MiB of text in one event: far past a chunk, and past the whole
        // queue budget on its own. 200 of them, with nobody draining.
        for _ in 0..200 {
            sink.send(output("y".repeat(4 * 1024 * 1024))).unwrap();
            assert!(
                sink.pending.load(Ordering::Relaxed) <= MAX_PENDING_OUTPUT_BYTES,
                "queue grew past its budget with nobody draining it"
            );
        }
        assert!(sink.dropped > 0, "expected the flood to be dropped");

        // Nothing queued exceeds the per-chunk cap, and draining credits the
        // budget back exactly (an unpaired credit would underflow the counter).
        let mut drained = 0usize;
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(10), events.recv()).await {
            let Some(DapEvent::Output(o)) = ev else { break };
            assert!(o.text.len() <= MAX_OUTPUT_CHUNK_BYTES + 64);
            drained += 1;
        }
        assert!(drained > 0);
        assert_eq!(sink.pending.load(Ordering::Relaxed), 0);

        // With room again, the next chunk is preceded by the drop notice.
        sink.send(output("tail\n".into())).unwrap();
        let Some(DapEvent::Output(notice)) = events.recv().await else {
            panic!("expected the dropped-bytes notice");
        };
        assert!(
            notice.text.contains("bytes of output dropped"),
            "got {:?}",
            notice.text
        );
        assert_eq!(sink.dropped, 0);
        let Some(DapEvent::Output(tail)) = events.recv().await else {
            panic!("expected the chunk that followed the notice");
        };
        assert_eq!(tail.text, "tail\n");
    }

    /// A chunk is cut on a char boundary: the panel renders the text as UTF-8,
    /// and slicing mid-codepoint would panic or garble it.
    #[test]
    fn oversized_chunk_is_truncated_on_a_char_boundary() {
        let text = "é".repeat(MAX_OUTPUT_CHUNK_BYTES); // 2 bytes each
        let cut = truncate_chunk(text);
        assert!(cut.starts_with('é'));
        assert!(cut.contains("bytes truncated"));
        assert!(cut.len() < MAX_OUTPUT_CHUNK_BYTES + 64);
        // Under the cap the text is passed through untouched.
        assert_eq!(truncate_chunk("short\n".into()), "short\n");
    }
}
