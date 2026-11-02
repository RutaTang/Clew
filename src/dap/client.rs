//! A minimal async DAP client: spawns a debug-adapter subprocess (or attaches
//! to one proxied elsewhere), speaks `Content-Length`-framed JSON to it, and
//! exposes the requests clew's debugger drives — initialize, launch,
//! breakpoints, stepping, stack/scopes/variables, evaluate.
//!
//! The transport is [`clew_core::framing`], shared with the LSP client: a
//! *reader* task parses framed messages off the adapter's output, an *actor*
//! task owns its input plus the pending-request map (keyed by DAP `seq`). This
//! module supplies the DAP half of that actor ([`DapWire`]). Adapter
//! **events** (stopped, output, terminated…) are forwarded to an unbounded
//! channel the app runs as an iced `Task`, so they land as `Message`s. That
//! channel carries the debuggee's whole stdout, so it is metered by
//! [`EventSink`] — see there for why it is metered rather than bounded.
//!
//! TCP adapters (delve, vscode-js-debug) are started on port 0: the OS picks a
//! free port, the adapter announces it on its stdout, and the client — and
//! every child session after it — connects to exactly that address. Only our
//! child writes that pipe, so the address named there is one OUR process
//! bound — the check that the connection reaches the adapter we spawned. The
//! old scheme picked a "free" port first (falling back to a fixed 8123) and
//! hoped nobody took it before the adapter did.
//!
//! What remains, stated plainly: a loopback port is reachable by every user
//! on the machine. While the adapter listens, another local account can
//! connect to it too — js-debug serves any number of sessions, and a debug
//! session launches programs as us. clew connects the moment the port is
//! announced, which leaves that window as short as it can be, but on a
//! shared machine the stdio adapters (lldb-dap, debugpy, dart) are the ones
//! without this exposure.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use clew_core::dapwire::{await_listening, connect_retry, from_first_frame};
use clew_core::framing::{self, CallError, EndReason, Failure, FrameError, Handled, Ids};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::proto::{Breakpoint, DapEvent, EvalContext, Output, Scope, StackFrame, Variable};

/// How the adapter speaks DAP: over its own stdio, or over a loopback TCP
/// port it announces on its stdout once it listens (js-debug, dlv).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stdio,
    Tcp,
    /// A TCP adapter that is the debuggee too (`rdbg`): its port is
    /// announced on stderr, and what it prints is the program's output.
    TcpOnStderr,
}

/// Ceiling for one DAP request. Generous — attaching or launching a big
/// debuggee takes seconds — but bounded: an adapter that never answers must
/// not leave the session (and whatever awaits it) hung forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long an adapter has to answer `disconnect`, asked as a run stops —
/// the adapter goes with its client after it. Waited for as long as any
/// request, a silent adapter kept its debuggee running, and its port held,
/// for a minute after Stop.
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// How long a `launch` refusal is listened for. The answer comes only after
/// `configurationDone`, and a launch can take as long as the program does to
/// start, so this is no failure when it passes: just the end of listening.
const LAUNCH_ANSWER_WAIT: Duration = Duration::from_secs(600);

/// How long a TCP adapter has to announce its port.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long connecting to an announced (or a child session's) port may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// A cheap, cloneable handle to the debug adapter; talks to the actor over a
/// channel so it can be moved into iced `Task`s freely.
#[derive(Clone, Debug)]
pub struct DapClient {
    rpc: framing::Handle,
    /// The loopback address a TCP adapter announced — which child sessions
    /// connect to as well. The whole address, not just the port: the port is
    /// only known to be OUR adapter's on the address it was announced for,
    /// and an adapter listening on `[::1]` is not reachable at `127.0.0.1`
    /// (where the same port number may be someone else's). `None` over stdio.
    addr: Option<SocketAddr>,
    timeout: Duration,
}

impl DapClient {
    /// Spawn the adapter and return the handle plus the stream of adapter events.
    /// Does *not* run the handshake — the caller drives initialize → launch →
    /// (on the `Initialized` event) setBreakpoints + configurationDone, because
    /// that ordering is event-driven.
    ///
    /// The adapter is killed with its process group when the session ends
    /// (every handle dropped), and on drop of the runtime.
    pub async fn start(
        adapter: &Path,
        args: &[String],
        cwd: &Path,
        transport: Transport,
    ) -> Result<(DapClient, DapEvents), String> {
        Self::start_with(adapter, args, cwd, transport, ANNOUNCE_TIMEOUT).await
    }

    async fn start_with(
        adapter: &Path,
        args: &[String],
        cwd: &Path,
        transport: Transport,
        announce: Duration,
    ) -> Result<(DapClient, DapEvents), String> {
        let mut command = Command::new(adapter);
        command
            .args(args)
            .current_dir(cwd)
            // Only the absolute entries of `PATH`: a relative one resolves
            // against the project (the working directory), where a
            // repository could plant the `node` or `python` the adapter runs.
            .env("PATH", clew_core::linerpc::child_path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let spawn_failed =
            |e: std::io::Error| format!("failed to launch adapter {}: {e}", adapter.display());
        match transport {
            Transport::Stdio => {
                command.stdin(std::process::Stdio::piped());
                let mut child = command.spawn().map_err(spawn_failed)?;
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                // Drain stderr: an adapter that logs enough to fill the pipe
                // would otherwise block on write and wedge the session.
                if let Some(e) = child.stderr.take() {
                    tokio::spawn(drain(e));
                }
                Ok(Self::wire(stdin, stdout, Some(child), None))
            }
            Transport::Tcp => {
                command.stdin(std::process::Stdio::null());
                let mut child = command.spawn().map_err(spawn_failed)?;
                if let Some(e) = child.stderr.take() {
                    tokio::spawn(drain(e));
                }
                let stdout = child.stdout.take().ok_or("no stdout")?;
                let mut out = BufReader::new(stdout);
                // The child is already running: every failure from here on
                // must take it down, not leave an adapter listening forever.
                let addr = match await_listening(&mut out, &mut child, announce).await {
                    Ok(addr) => addr,
                    Err(e) => {
                        framing::reap(child, Duration::ZERO, cfg!(unix)).await;
                        return Err(e);
                    }
                };
                // Keep draining whatever else it prints.
                tokio::spawn(drain(out));
                let stream = match connect_retry(addr, Some(&mut child), CONNECT_TIMEOUT).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        framing::reap(child, Duration::ZERO, cfg!(unix)).await;
                        return Err(e);
                    }
                };
                let (read, write) = stream.into_split();
                Ok(Self::wire(write, read, Some(child), Some(addr)))
            }
            Transport::TcpOnStderr => {
                command.stdin(std::process::Stdio::null());
                let mut child = command.spawn().map_err(spawn_failed)?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                let stderr = child.stderr.take().ok_or("no stderr")?;
                let mut err = BufReader::new(stderr);
                let addr = match await_listening(&mut err, &mut child, announce).await {
                    Ok(addr) => addr,
                    Err(e) => {
                        framing::reap(child, Duration::ZERO, cfg!(unix)).await;
                        return Err(e);
                    }
                };
                let stream = match connect_retry(addr, Some(&mut child), CONNECT_TIMEOUT).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        framing::reap(child, Duration::ZERO, cfg!(unix)).await;
                        return Err(e);
                    }
                };
                let (read, write) = stream.into_split();
                let read = from_first_frame(read);
                // The program's output is the adapter's: shown in the
                // console as the program's.
                let outputs: Vec<(BoxedOutput, &'static str)> =
                    vec![(Box::new(stdout), "stdout"), (Box::new(err), "stderr")];
                Ok(Self::wire_forwarding(
                    write,
                    read,
                    Some(child),
                    Some(addr),
                    outputs,
                ))
            }
        }
    }

    /// Connect to a debug adapter over a provided stdio transport — its process
    /// is owned elsewhere (proxied by clew-server), so the adapter runs where the
    /// code lives. No local child; same DAP wire format over the bridge.
    pub async fn connect<R, W>(stdin: W, stdout: R) -> Result<(DapClient, DapEvents), String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Ok(Self::wire(stdin, stdout, None, None))
    }

    /// Open a *child* session on an already-running TCP adapter (js-debug asks
    /// the client to start one per debuggee target). No process is spawned;
    /// the parent session owns the adapter, and `addr` is the address it
    /// announced ([`Self::tcp_addr`]) — dialled exactly, never re-derived.
    pub async fn connect_tcp_addr(addr: SocketAddr) -> Result<(DapClient, DapEvents), String> {
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to open a debug session on {addr}: only a loopback adapter is used"
            ));
        }
        let stream = connect_retry(addr, None, CONNECT_TIMEOUT).await?;
        let (read, write) = stream.into_split();
        Ok(Self::wire(write, read, None, Some(addr)))
    }

    /// Start the reader and actor over a transport.
    fn wire<W, R>(
        writer: W,
        reader: R,
        child: Option<Child>,
        addr: Option<SocketAddr>,
    ) -> (DapClient, DapEvents)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        Self::wire_forwarding(writer, reader, child, addr, Vec::new())
    }

    /// [`Self::wire`], with `outputs` — a debuggee's own streams, each with
    /// its output category — forwarded into the event stream as output.
    fn wire_forwarding<W, R>(
        writer: W,
        reader: R,
        child: Option<Child>,
        addr: Option<SocketAddr>,
        outputs: Vec<(BoxedOutput, &'static str)>,
    ) -> (DapClient, DapEvents)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, events) = event_channel();
        let (rpc, mailbox) = framing::mailbox();
        let incoming = framing::spawn_reader(reader);
        // The adapter that runs the program (its output forwarded) is let
        // finish writing it out.
        let grace = match outputs.is_empty() {
            true => Duration::ZERO,
            false => DEBUGGEE_EXIT_GRACE,
        };
        for (stream, category) in outputs {
            tokio::spawn(forward_output(stream, category, sink.sharing()));
        }
        tokio::spawn(run_session(writer, mailbox, incoming, child, sink, grace));
        let client = DapClient {
            rpc,
            addr,
            timeout: REQUEST_TIMEOUT,
        };
        (client, events)
    }

    /// The loopback address of a TCP adapter, for child sessions
    /// ([`Self::connect_tcp_addr`]); `None` over stdio.
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.addr
    }

    /// Whether the session is still running.
    pub fn alive(&self) -> bool {
        self.rpc.alive()
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Send a DAP request and await its response `body` (or the failure message).
    /// A request the adapter never answers fails after the timeout, and its
    /// pending entry is released rather than kept for the session's lifetime.
    async fn request(&self, command: &str, arguments: Value) -> Result<Value, String> {
        self.request_within(command, arguments, self.timeout).await
    }

    /// [`Self::request`], answered within `timeout`.
    async fn request_within(
        &self,
        command: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.rpc
            .call(command, arguments, timeout)
            .await
            .map_err(|e| match e {
                CallError::NotRunning => "debug adapter not running".to_string(),
                CallError::Closed => "debug adapter closed".to_string(),
                CallError::TimedOut => format!("debug adapter did not answer '{command}' in time"),
                CallError::Failed(failure) => failure.message,
            })
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

    /// Launch the program with adapter-specific arguments. Sent at once; the
    /// launch response arrives after we send `configurationDone` (driven by the
    /// `Initialized` event), so awaiting it before handling events would stall
    /// the handshake — the caller awaits what this returns ALONGSIDE them.
    ///
    /// It resolves to the adapter's refusal, if it refused: the program it
    /// could not find or start. Fired and forgotten, that answer was dropped
    /// unread, and the session sat with no program and no word why until the
    /// reader pressed Stop. Any other ending (answered, a session that closed,
    /// an adapter slow to answer) is `Ok`: only a refusal says something.
    pub fn launch(
        &self,
        launch_args: Value,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let answer = self.rpc.call_now("launch", launch_args, LAUNCH_ANSWER_WAIT);
        async move {
            match answer.await {
                Err(CallError::Failed(failure)) => Err(failure.message),
                _ => Ok(()),
            }
        }
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
    /// still sends the full stack, capped only by
    /// [`framing::MAX_FRAME_BYTES`] at the reader.
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

    /// Evaluate an expression in a frame; returns the result string.
    /// `context` says why (see [`EvalContext`]): the adapter's side-effect
    /// guarantees are tied to it, so a hover must never be sent as a watch.
    pub async fn evaluate(
        &self,
        expression: &str,
        frame_id: i64,
        context: EvalContext,
    ) -> Result<String, String> {
        let body = self
            .request(
                "evaluate",
                json!({
                    "expression": expression,
                    "frameId": frame_id,
                    "context": context.as_str(),
                }),
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
        let within = self.timeout.min(DISCONNECT_TIMEOUT);
        self.request_within("disconnect", json!({ "terminateDebuggee": true }), within)
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

impl std::fmt::Debug for DapEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapEvents")
            .field("queued_output_bytes", &self.pending.load(Ordering::Relaxed))
            .finish()
    }
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

    /// Another sender into the same stream, under the same budget, counting
    /// what it drops on its own.
    fn sharing(&self) -> EventSink {
        EventSink {
            tx: self.tx.clone(),
            pending: self.pending.clone(),
            dropped: 0,
        }
    }

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

/// The DAP half of a session's actor (see [`framing::run`]): envelopes,
/// response routing by `request_seq`, events, and the adapter's own
/// ("reverse") requests.
struct DapWire {
    events: EventSink,
}

impl DapWire {
    /// A line in the debug console from clew itself.
    fn console(&mut self, text: String) {
        let _ = self.events.send(DapEvent::Output(Output {
            category: "console".into(),
            text,
        }));
    }

    /// Answer a reverse request. `startDebugging` (js-debug's multi-session
    /// model) is acknowledged and forwarded so the app opens the child
    /// session; everything else is declined, so the adapter never hangs
    /// waiting for an answer that will not come.
    fn reverse_request(&mut self, value: &Value, ids: &Ids) -> Handled {
        let Some(seq) = value.get("seq").and_then(Value::as_i64) else {
            return Handled::Nothing;
        };
        let command = value
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        // A response is a protocol message like any other: it carries a `seq`
        // of its own (the old reply had none), from the same counter as our
        // requests, and says which request it answers.
        let mut response = json!({
            "seq": ids.next(),
            "type": "response",
            "request_seq": seq,
            "command": command,
        });
        if command == "startDebugging" {
            let config = value
                .get("arguments")
                .and_then(|a| a.get("configuration"))
                .cloned()
                .unwrap_or(Value::Null);
            if self.events.send(DapEvent::StartDebugging(config)).is_err() {
                return Handled::Close;
            }
            response["success"] = json!(true);
        } else {
            response["success"] = json!(false);
            // `message` is optional and, when present, a string: the old
            // reply sent `"message": null` on success, which strict adapters
            // reject.
            response["message"] = json!(match command.as_str() {
                "runInTerminal" => "clew runs the debuggee in its own console, not a terminal",
                _ => "unsupported by clew",
            });
        }
        Handled::Reply(response)
    }
}

impl framing::Protocol for DapWire {
    fn request(&mut self, seq: i64, command: &str, arguments: Value) -> Value {
        json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })
    }

    /// DAP has no client-to-adapter notifications; a message nobody waits on
    /// is a request whose response is ignored when it comes.
    fn notification(&mut self, seq: i64, command: &str, arguments: Value) -> Value {
        self.request(seq, command, arguments)
    }

    fn inbound(&mut self, value: Value, ids: &Ids) -> Handled {
        match value.get("type").and_then(Value::as_str) {
            Some("response") => {
                let Some(id) = value.get("request_seq").and_then(Value::as_i64) else {
                    return Handled::Nothing;
                };
                let ok = value
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let result = if ok {
                    Ok(value.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    Err(Failure::from(
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("request failed"),
                    ))
                };
                Handled::Response { id, result }
            }
            Some("event") => {
                let Some(name) = value.get("event").and_then(Value::as_str) else {
                    return Handled::Nothing;
                };
                let body = value.get("body").cloned().unwrap_or(Value::Null);
                match self.events.send(DapEvent::parse(name, &body)) {
                    Ok(()) => Handled::Nothing,
                    Err(()) => Handled::Close, // the app dropped the receiver
                }
            }
            Some("request") => self.reverse_request(&value, ids),
            _ => Handled::Nothing,
        }
    }

    fn frame_error(&mut self, error: &FrameError) {
        let text = if error.is_fatal() {
            format!(
                "[clew] the debug adapter's output broke framing ({error}) — ending the session\n"
            )
        } else {
            format!("[clew] skipped a malformed message from the debug adapter ({error})\n")
        };
        self.console(text);
    }

    fn ended(&mut self, reason: &EndReason) -> String {
        match reason {
            EndReason::Broken(error) => {
                format!("the connection to the debug adapter broke: {error}")
            }
            _ => "debug adapter stopped".to_string(),
        }
    }
}

/// The actor for one session, then the adapter's end: killed (with its
/// process group, so a debuggee it spawned goes too) once the session is
/// over — after `grace` to exit by itself. `child` is `None` for a proxied
/// adapter and for a child session sharing its parent's adapter (js-debug).
async fn run_session<W>(
    writer: W,
    mailbox: framing::Mailbox,
    incoming: mpsc::UnboundedReceiver<framing::Inbound>,
    child: Option<Child>,
    events: EventSink,
    grace: Duration,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let ended = framing::run(writer, DapWire { events }, mailbox, incoming).await;
    if let Some(child) = child {
        framing::reap(child, grace, cfg!(unix)).await;
    }
    ended.release();
}

/// How long an adapter that is its own debuggee (rdbg) has to exit by
/// itself once its session is over. It closes the session as the program
/// ends, and the program has yet to write out what it buffered: killed at
/// once, the program's last output — all of it, for a short run whose
/// stdout is a pipe — was lost.
const DEBUGGEE_EXIT_GRACE: Duration = Duration::from_secs(3);

/// A debuggee's output stream, as [`DapClient::wire_forwarding`] takes it.
type BoxedOutput = Box<dyn tokio::io::AsyncRead + Unpin + Send>;

/// Forward what `r` (a debuggee's stdout or stderr) prints into `sink` as
/// `category` output, chunk by chunk as it comes — a prompt with no newline
/// shows at once — never cutting a character in two. Once the app is gone,
/// the rest is drained unread, so the program never blocks on a full pipe.
async fn forward_output(mut r: BoxedOutput, category: &'static str, mut sink: EventSink) {
    let mut buf = [0u8; 4096];
    let mut text = clew_core::dapwire::Utf8Carry::default();
    let send = |sink: &mut EventSink, text: String| {
        sink.send(DapEvent::Output(Output {
            category: category.into(),
            text,
        }))
    };
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if let Some(whole) = text.push(&buf[..n])
            && send(&mut sink, whole).is_err()
        {
            drain(r).await;
            return;
        }
    }
    if let Some(rest) = text.finish() {
        let _ = send(&mut sink, rest);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::dapwire::parse_listening;
    use clew_core::framing::{read_message, write_frame};
    use std::time::Instant;
    use tokio::net::TcpStream;

    /// A client wired to a scripted adapter over in-memory pipes: `respond`
    /// answers each request `(command, arguments)` with a response `body`. Uses
    /// the real framing on both sides, so requests go out and answers come
    /// back over genuine `Content-Length` framing.
    async fn scripted_adapter<F>(respond: F) -> (DapClient, DapEvents)
    where
        F: Fn(&str, &Value) -> Value + Send + 'static,
    {
        let (to_adapter, from_client) = tokio::io::duplex(1 << 16);
        let (mut to_client, from_adapter) = tokio::io::duplex(1 << 16);
        let mut req_rx = framing::spawn_reader(from_client);
        tokio::spawn(async move {
            while let Some(Ok(req)) = req_rx.recv().await {
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

    /// A refused `launch` comes back to the caller, with the adapter's words:
    /// fired and forgotten, the refusal was dropped unread, and the session
    /// waited on a program that never ran. A launch answered is `Ok`.
    #[tokio::test]
    async fn a_refused_launch_is_reported_with_the_adapters_message() {
        let (to_adapter, from_client) = tokio::io::duplex(1 << 16);
        let (mut to_client, from_adapter) = tokio::io::duplex(1 << 16);
        let mut req_rx = framing::spawn_reader(from_client);
        tokio::spawn(async move {
            while let Some(Ok(req)) = req_rx.recv().await {
                let program = req["arguments"]["program"].as_str().unwrap_or("");
                let ok = program != "/missing";
                let resp = json!({
                    "type": "response",
                    "request_seq": req["seq"],
                    "success": ok,
                    "command": req["command"],
                    "message": "error: unable to find executable",
                });
                if write_frame(&mut to_client, &resp).await.is_err() {
                    break;
                }
            }
        });
        let (client, _events) = DapClient::connect(to_adapter, from_adapter).await.unwrap();
        assert_eq!(
            client.launch(json!({ "program": "/missing" })).await,
            Err("error: unable to find executable".to_string())
        );
        assert_eq!(
            client.launch(json!({ "program": "/bin/true" })).await,
            Ok(())
        );
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

    /// The adapter's ends of an in-memory transport, driven by hand.
    struct Adapter {
        from_client: BufReader<tokio::io::DuplexStream>,
        to_client: tokio::io::DuplexStream,
    }

    impl Adapter {
        async fn next(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(5), read_message(&mut self.from_client))
                .await
                .expect("the client said nothing")
                .expect("a well-framed message")
                .expect("the client closed the stream")
        }

        async fn send(&mut self, msg: Value) {
            write_frame(&mut self.to_client, &msg).await.unwrap();
        }
    }

    async fn manual_adapter() -> (DapClient, DapEvents, Adapter) {
        let (to_adapter, from_client) = tokio::io::duplex(1 << 16);
        let (to_client, from_adapter) = tokio::io::duplex(1 << 16);
        let (client, events) = DapClient::connect(to_adapter, from_adapter).await.unwrap();
        let adapter = Adapter {
            from_client: BufReader::new(from_client),
            to_client,
        };
        (client, events, adapter)
    }

    /// F17: the adapter's own requests are answered — `startDebugging`
    /// accepted and handed to the app, anything else declined — each reply a
    /// proper response with a `seq` of its own and no `message: null`.
    #[tokio::test]
    async fn reverse_requests_are_answered() {
        let (_client, mut events, mut adapter) = manual_adapter().await;
        adapter
            .send(
                json!({"seq": 100, "type": "request", "command": "runInTerminal",
                         "arguments": {"args": ["sh"]}}),
            )
            .await;
        let declined = adapter.next().await;
        assert_eq!(declined["type"], "response");
        assert_eq!(declined["request_seq"], 100);
        assert_eq!(declined["command"], "runInTerminal");
        assert_eq!(declined["success"], false);
        assert!(declined["message"].is_string());
        assert!(declined["seq"].is_i64());

        adapter
            .send(
                json!({"seq": 101, "type": "request", "command": "startDebugging",
                         "arguments": {"configuration": {"__pendingTargetId": "t1"}}}),
            )
            .await;
        let accepted = adapter.next().await;
        assert_eq!(accepted["request_seq"], 101);
        assert_eq!(accepted["success"], true);
        assert!(accepted.get("message").is_none(), "{accepted}");
        assert_ne!(
            accepted["seq"], declined["seq"],
            "every message has its own seq"
        );
        match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
            Ok(Some(DapEvent::StartDebugging(config))) => {
                assert_eq!(config["__pendingTargetId"], "t1");
            }
            other => panic!("expected the child-session request, got {other:?}"),
        }
    }

    /// An adapter that does not answer `disconnect` is waited for a few
    /// seconds, not the minute any request gets: the run is over, and the
    /// adapter goes with its client after it.
    #[tokio::test]
    async fn an_unanswered_disconnect_is_not_waited_out() {
        let (client, _events, mut adapter) = manual_adapter().await;
        let started = std::time::Instant::now();
        let err = client.disconnect().await.unwrap_err();
        assert!(err.contains("did not answer 'disconnect'"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(adapter.next().await["command"], "disconnect");
    }

    /// F8/F17: a request the adapter never answers fails after the timeout —
    /// and is released: the old pending map kept every such entry for the
    /// session's lifetime. A later, answered request still works.
    #[tokio::test]
    async fn an_unanswered_request_times_out_and_the_session_goes_on() {
        let (client, _events, mut adapter) = manual_adapter().await;
        let client = client.with_timeout(Duration::from_millis(100));
        let err = client.continue_(1).await.unwrap_err();
        assert!(err.contains("did not answer 'continue'"), "{err}");
        let ignored = adapter.next().await;
        assert_eq!(ignored["command"], "continue");

        let next = tokio::spawn({
            let client = client.clone().with_timeout(Duration::from_secs(5));
            async move { client.next(1).await }
        });
        let request = adapter.next().await;
        assert_eq!(request["command"], "next");
        // A late answer to the forgotten request is dropped harmlessly.
        adapter
            .send(
                json!({"seq": 1, "type": "response", "request_seq": ignored["seq"],
                         "success": true, "command": "continue"}),
            )
            .await;
        adapter
            .send(
                json!({"seq": 2, "type": "response", "request_seq": request["seq"],
                         "success": true, "command": "next"}),
            )
            .await;
        assert_eq!(next.await.unwrap(), Ok(()));
    }

    /// F7: a malformed frame ends the session with a reason — the request in
    /// flight fails at once instead of timing out, and the console says why.
    #[tokio::test]
    async fn a_malformed_frame_ends_the_session() {
        use tokio::io::AsyncWriteExt;
        let (client, mut events, mut adapter) = manual_adapter().await;
        let pending = tokio::spawn({
            let client = client.clone();
            async move { client.threads_probe().await }
        });
        let _ = adapter.next().await;
        adapter
            .to_client
            .write_all(b"Content-Length: -3\r\n\r\n{}")
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("fails promptly")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("broke"), "{err}");
        let Some(DapEvent::Output(notice)) = events.recv().await else {
            panic!("expected the console notice");
        };
        assert!(notice.text.contains("Content-Length"), "{:?}", notice.text);
        assert!(events.recv().await.is_none(), "the event stream ends");
        assert!(!client.alive());
    }

    /// D1-10 / I2: an evaluation says WHY it runs. A hover goes out as the
    /// `"hover"` context — the only one an adapter's
    /// `supportsEvaluateForHovers` (no side effects) promise covers — and a
    /// watch as `"watch"`; neither is ever sent as the other.
    #[tokio::test]
    async fn evaluate_sends_the_context_it_was_asked_for() {
        let (client, _events, mut adapter) = manual_adapter().await;
        for (context, wire) in [(EvalContext::Hover, "hover"), (EvalContext::Watch, "watch")] {
            let pending = tokio::spawn({
                let client = client.clone();
                async move { client.evaluate("self.len", 7, context).await }
            });
            let request = adapter.next().await;
            assert_eq!(request["command"], "evaluate");
            assert_eq!(request["arguments"]["context"], wire);
            assert_eq!(request["arguments"]["expression"], "self.len");
            assert_eq!(request["arguments"]["frameId"], 7);
            adapter
                .send(
                    json!({"seq": 1, "type": "response", "request_seq": request["seq"],
                             "success": true, "command": "evaluate",
                             "body": {"result": "3", "variablesReference": 0}}),
                )
                .await;
            assert_eq!(pending.await.unwrap(), Ok("3".to_string()));
        }
    }

    impl DapClient {
        /// Any answered-or-not request, for tests that only care about the
        /// transport.
        async fn threads_probe(&self) -> Result<Value, String> {
            self.request("threads", json!({})).await
        }
    }

    #[test]
    fn listening_announcements_are_parsed_and_checked() {
        let v4 = SocketAddr::from(([127, 0, 0, 1], 38697));
        let v6: SocketAddr = "[::1]:8123".parse().unwrap();
        for (line, want) in [
            ("DAP server listening at: 127.0.0.1:38697\n", v4),
            ("Debug server listening at 127.0.0.1:38697\n", v4),
            ("Debug server listening at ::1:8123", v6),
            ("Debug server listening at [::1]:8123", v6),
            ("debug server LISTENING AT localhost:38697", v4),
        ] {
            assert_eq!(parse_listening(line), Ok(Some(want)), "{line:?}");
        }
        assert_eq!(parse_listening("API server starting\n"), Ok(None));
        assert!(
            parse_listening("DAP server listening at: 10.0.0.5:4000")
                .unwrap_err()
                .contains("not on loopback")
        );
        assert!(parse_listening("DAP server listening at: 127.0.0.1:0").is_err());
        assert!(parse_listening("Debug server listening at somewhere").is_err());
        // rdbg's own words, on its stderr.
        assert_eq!(
            parse_listening("DEBUGGER: Debugger can attach via TCP/IP (127.0.0.1:38697)\n"),
            Ok(Some(v4))
        );
        assert!(
            parse_listening("DEBUGGER: Debugger can attach via TCP/IP (10.0.0.5:4000)")
                .unwrap_err()
                .contains("not on loopback")
        );
        assert_eq!(
            parse_listening("DEBUGGER: wait for debugger connection..."),
            Ok(None)
        );
    }

    /// An adapter that is its own debuggee (rdbg) is reached at the port it
    /// names on stderr, and what it prints after — on stdout and stderr — is
    /// the program's output, a character split across two writes kept whole.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_adapter_that_runs_the_program_forwards_its_output() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            "echo 'DEBUGGER: Debugger can attach via TCP/IP (127.0.0.1:{port})' >&2; \
             sleep 0.5; printf 'caf\\303'; sleep 0.2; printf '\\251\\n'; \
             echo oops >&2; exec sleep 30"
        );
        let args = vec!["-c".to_string(), script];
        let start = tokio::spawn(async move {
            DapClient::start_with(
                Path::new("/bin/sh"),
                &args,
                Path::new("/"),
                Transport::TcpOnStderr,
                Duration::from_secs(10),
            )
            .await
        });
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the client connects to the port named on stderr")
            .unwrap();
        let (client, mut events) = start.await.unwrap().expect("started");
        assert_eq!(
            client.tcp_addr(),
            Some(SocketAddr::from(([127, 0, 0, 1], port)))
        );
        let mut stdout = String::new();
        let mut stderr = String::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(stdout.contains('\n') && stderr.contains("oops")) && Instant::now() < deadline {
            let Ok(Some(DapEvent::Output(o))) =
                tokio::time::timeout(Duration::from_secs(10), events.recv()).await
            else {
                break;
            };
            match o.category.as_str() {
                "stdout" => stdout.push_str(&o.text),
                "stderr" => stderr.push_str(&o.text),
                other => panic!("{other}: {:?}", o.text),
            }
        }
        assert_eq!(stdout, "café\n");
        assert_eq!(stderr, "oops\n");
    }

    /// What rdbg writes to a connection before it reads the client's first
    /// frame — its listing of where the program is held, as text — is passed
    /// over; the frames after it arrive whole, a line within one included.
    #[tokio::test]
    async fn text_ahead_of_the_first_frame_is_passed_over() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = r#"{"seq":1,"type":"event","event":"output","body":{"output":"a\nb"}}"#;
            let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
            socket
                .write_all(
                    format!("out [1, 3] in app.rb\nout =>   1| x = 41\n{frame}{frame}").as_bytes(),
                )
                .await
                .unwrap();
            socket
        });
        let (read, _write) = TcpStream::connect(addr).await.unwrap().into_split();
        let _socket = server.await.unwrap();
        let mut frames = BufReader::new(from_first_frame(read));
        for _ in 0..2 {
            let frame = read_message(&mut frames).await.unwrap().unwrap();
            assert_eq!(frame["body"]["output"], "a\nb");
        }
    }

    /// A native program under the real lldb-dap — the path Rust's, C's, C++'s
    /// and Zig's binaries take: held at a breakpoint, its stack and a local
    /// read, stepped, run on to its end with its output shown. Needs
    /// `lldb-dap` (or a versioned one) and a C compiler.
    #[tokio::test]
    #[ignore = "needs lldb-dap and cc on the PATH; run explicitly"]
    async fn a_native_program_is_debugged_with_lldb_dap() {
        let dir = clew_core::testutil::TempDir::new("dap-lldb");
        let source = dir.join("app.c");
        std::fs::write(
            &source,
            "#include <stdio.h>\n\nint total(int a, int b) {\n    int sum = a + b;\n    return sum * 2;\n}\n\nint main(void) {\n    int t = total(2, 3);\n    printf(\"answer %d\\n\", t);\n    return 0;\n}\n",
        )
        .unwrap();
        let binary = dir.join("app");
        let built = std::process::Command::new("cc")
            .args(["-g", "-O0", "-o"])
            .arg(&binary)
            .arg(&source)
            .status()
            .unwrap();
        assert!(built.success());
        let spec = match clew_core::debugadapter::resolve(
            "native",
            &binary.to_string_lossy(),
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        ) {
            Ok(clew_core::debugadapter::Resolution::Ready(spec)) => spec,
            other => panic!("{other:?}"),
        };
        let (client, mut events) =
            DapClient::start(&spec.command, &spec.args, &dir, Transport::Stdio)
                .await
                .expect("lldb-dap started");
        client.initialize().await.expect("initialized");
        let launched = client.launch(spec.launch.clone());
        let mut output = String::new();
        let mut stops: Vec<usize> = Vec::new();
        let mut configured = false;
        let mut stepped = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(60), events.recv())
                .await
                .expect("an event in time");
            match event {
                Some(DapEvent::Initialized) if !configured => {
                    configured = true;
                    let bps = client.set_breakpoints(&source, &[(4, None)]).await.unwrap();
                    assert!(bps.iter().all(|b| b.verified), "{bps:?}");
                    client.configuration_done().await.unwrap();
                }
                Some(DapEvent::Stopped(stop)) => {
                    let thread = stop.thread_id.unwrap_or(1);
                    let frames = client.stack_trace(thread).await.unwrap();
                    stops.push(frames[0].line);
                    if !stepped {
                        assert_eq!(frames[0].name, "total");
                        assert_eq!(frames[1].line, 9, "main, where it called");
                        let scopes = client.scopes(frames[0].id).await.unwrap();
                        let locals = client
                            .variables(scopes[0].variables_reference)
                            .await
                            .unwrap();
                        assert!(
                            locals.iter().any(|v| v.name == "a" && v.value == "2"),
                            "{locals:?}"
                        );
                        stepped = true;
                        client.next(thread).await.unwrap();
                    } else {
                        client.continue_(thread).await.unwrap();
                    }
                }
                Some(DapEvent::Output(o)) if o.category != "console" => output.push_str(&o.text),
                Some(DapEvent::Terminated) | Some(DapEvent::Exited { .. }) | None => break,
                Some(_) => {}
            }
        }
        let settle = Instant::now() + Duration::from_secs(5);
        while !output.contains("answer 10") && Instant::now() < settle {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Some(DapEvent::Output(o))) if o.category != "console" => {
                    output.push_str(&o.text)
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert_eq!(launched.await, Ok(()));
        assert_eq!(stops, [4, 5], "held at the breakpoint, then a line on");
        assert!(output.contains("answer 10"), "{output:?}");
    }

    /// Go's real delve, end to end: the program built and held at a
    /// breakpoint, its stack read, run on to its end with its output shown.
    /// Needs `go` and `dlv` on the PATH.
    #[tokio::test]
    #[ignore = "needs Go and delve on the PATH; run explicitly"]
    async fn a_go_program_is_debugged_with_delve() {
        let dir = clew_core::testutil::TempDir::new("dap-delve");
        std::fs::write(dir.join("go.mod"), "module example.com/shop\n\ngo 1.21\n").unwrap();
        let program = dir.join("main.go");
        std::fs::write(
            &program,
            "package main\n\nimport \"fmt\"\n\nfunc total(items []int) int {\n\tsum := 0\n\tfor _, i := range items {\n\t\tsum += i\n\t}\n\treturn sum * 2\n}\n\nfunc main() {\n\tt := total([]int{2, 3})\n\tfmt.Println(\"answer\", t)\n}\n",
        )
        .unwrap();
        let spec = match clew_core::debugadapter::resolve(
            "go",
            &dir.to_string_lossy(),
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        ) {
            Ok(clew_core::debugadapter::Resolution::Ready(spec)) => spec,
            other => panic!("{other:?}"),
        };
        let (client, mut events) =
            DapClient::start(&spec.command, &spec.args, &dir, Transport::Tcp)
                .await
                .expect("delve started");
        client.initialize().await.expect("initialized");
        let launched = client.launch(spec.launch.clone());
        let mut output = String::new();
        let mut stopped_at = None;
        let mut configured = false;
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            let event = tokio::time::timeout(Duration::from_secs(120), events.recv())
                .await
                .expect("an event in time");
            match event {
                Some(DapEvent::Initialized) if !configured => {
                    configured = true;
                    let bps = client
                        .set_breakpoints(&program, &[(6, None)])
                        .await
                        .unwrap();
                    assert!(bps.iter().all(|b| b.verified), "{bps:?}");
                    client.configuration_done().await.unwrap();
                }
                Some(DapEvent::Stopped(stop)) => {
                    let thread = stop.thread_id.unwrap_or(1);
                    let frames = client.stack_trace(thread).await.unwrap();
                    stopped_at = frames.first().map(|f| f.line);
                    client.continue_(thread).await.unwrap();
                }
                Some(DapEvent::Output(o)) if o.category != "console" => output.push_str(&o.text),
                Some(DapEvent::Terminated) | Some(DapEvent::Exited { .. }) | None => break,
                Some(_) => {}
            }
        }
        // Output may trail the end a moment.
        let settle = Instant::now() + Duration::from_secs(5);
        while !output.contains("answer 10") && Instant::now() < settle {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Some(DapEvent::Output(o))) if o.category != "console" => {
                    output.push_str(&o.text)
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert_eq!(launched.await, Ok(()));
        assert_eq!(stopped_at, Some(6), "stopped at the breakpoint");
        assert!(output.contains("answer 10"), "{output:?}");
    }

    /// Ruby's real rdbg, end to end: the program waits at its start until
    /// the session is configured, stops at the breakpoint, and runs on to
    /// its end with its output shown. Needs `rdbg` (the `debug` gem) on the
    /// PATH.
    #[tokio::test]
    #[ignore = "needs Ruby's rdbg on the PATH; run explicitly"]
    async fn a_ruby_program_is_debugged_with_rdbg() {
        let dir = clew_core::testutil::TempDir::new("dap-rdbg");
        let program = dir.join("app.rb");
        std::fs::write(&program, "x = 41\nx += 1\nputs \"answer #{x}\"\n").unwrap();
        let rdbg = clew_core::lsp::store::find_on_path_unresolved("rdbg").expect("rdbg on PATH");
        let spec = match clew_core::debugadapter::resolve(
            "ruby",
            &program.to_string_lossy(),
            &[],
            &dir,
            &dir,
            &Default::default(),
            None,
        ) {
            Ok(clew_core::debugadapter::Resolution::Ready(spec)) => spec,
            other => panic!("{other:?}"),
        };
        assert_eq!(spec.command, rdbg);
        let (client, mut events) =
            DapClient::start(&spec.command, &spec.args, &dir, Transport::TcpOnStderr)
                .await
                .expect("rdbg started");
        client.initialize().await.expect("initialized");
        let launched = client.launch(spec.launch.clone());
        let mut output = String::new();
        let mut stopped_at = None;
        let mut configured = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .expect("an event in time");
            match event {
                Some(DapEvent::Initialized) if !configured => {
                    configured = true;
                    let bps = client
                        .set_breakpoints(&program, &[(2, None)])
                        .await
                        .unwrap();
                    assert!(bps.iter().all(|b| b.verified), "{bps:?}");
                    client.configuration_done().await.unwrap();
                }
                Some(DapEvent::Stopped(stop)) => {
                    let thread = stop.thread_id.unwrap_or(1);
                    let frames = client.stack_trace(thread).await.unwrap();
                    stopped_at = frames.first().map(|f| f.line);
                    client.continue_(thread).await.unwrap();
                }
                Some(DapEvent::Output(o)) if o.category == "stdout" => output.push_str(&o.text),
                Some(DapEvent::Terminated) | None => break,
                Some(_) => {}
            }
            if output.contains("answer 42") && stopped_at.is_some() {
                break;
            }
        }
        assert_eq!(launched.await, Ok(()));
        assert_eq!(stopped_at, Some(2), "stopped at the breakpoint");
        // The program's output may trail its end: gathered a moment more.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !output.contains("answer 42") && Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await {
                Ok(Some(DapEvent::Output(o))) if o.category == "stdout" => output.push_str(&o.text),
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(output.contains("answer 42"), "{output:?}");
    }

    /// F9: a TCP adapter is reached at the port it announced — not at one
    /// guessed in advance — and the session is wired over that socket.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_tcp_adapter_is_reached_at_the_port_it_announces() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Stands in for dlv: announces a port, then stays up.
        let script = format!(
            "echo 'warming up'; echo 'DAP server listening at: 127.0.0.1:{port}'; exec sleep 30"
        );
        let args = vec!["-c".to_string(), script];
        let start = tokio::spawn(async move {
            DapClient::start_with(
                Path::new("/bin/sh"),
                &args,
                Path::new("/"),
                Transport::Tcp,
                Duration::from_secs(10),
            )
            .await
        });
        let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the client connects to the announced port")
            .unwrap();
        let (client, _events) = start.await.unwrap().expect("started");
        assert_eq!(
            client.tcp_addr(),
            Some(SocketAddr::from(([127, 0, 0, 1], port)))
        );
        // The session really runs over that socket.
        let (read, mut write) = socket.into_split();
        let mut read = BufReader::new(read);
        let init = tokio::spawn({
            let client = client.clone();
            async move { client.initialize().await }
        });
        let request = read_message(&mut read).await.unwrap().unwrap();
        assert_eq!(request["command"], "initialize");
        write_frame(
            &mut write,
            &json!({"seq": 1, "type": "response", "request_seq": request["seq"],
                    "success": true, "command": "initialize", "body": {}}),
        )
        .await
        .unwrap();
        assert_eq!(init.await.unwrap(), Ok(json!({})));
    }

    /// F9: the address a TCP adapter announced is kept whole, and a child
    /// session dials exactly it. Keeping only the port made every child
    /// session dial `127.0.0.1`, so an adapter announcing `[::1]` got child
    /// sessions that could not reach it — or reached whoever held that port
    /// on the other loopback.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_child_session_dials_the_address_the_adapter_announced() {
        // Every macOS host has `::1` on lo0, and is held to it. A Linux host
        // with no IPv6 (a container) checks the same thing on another IPv4
        // loopback: what matters is an announced address that is not
        // 127.0.0.1. Neither fails here rather than passing having checked
        // nothing.
        let listener = match tokio::net::TcpListener::bind("[::1]:0").await {
            Ok(listener) => listener,
            Err(e) if cfg!(target_os = "macos") => panic!("an IPv6 loopback to listen on: {e}"),
            Err(_) => tokio::net::TcpListener::bind("127.0.0.2:0")
                .await
                .expect("a loopback other than 127.0.0.1 to listen on"),
        };
        let announced = listener.local_addr().unwrap();
        let script = format!("echo 'Debug server listening at {announced}'; exec sleep 30");
        let args = vec!["-c".to_string(), script];
        let start = tokio::spawn(async move {
            DapClient::start_with(
                Path::new("/bin/sh"),
                &args,
                Path::new("/"),
                Transport::Tcp,
                Duration::from_secs(10),
            )
            .await
        });
        let (_parent, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the parent session connects")
            .unwrap();
        let (client, _events) = start.await.unwrap().expect("started");
        assert_eq!(client.tcp_addr(), Some(announced));

        let child = tokio::spawn(DapClient::connect_tcp_addr(announced));
        let (_socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the child session reaches the adapter on its own address")
            .unwrap();
        let (child, _events) = child.await.unwrap().expect("connected");
        assert_eq!(child.tcp_addr(), Some(announced));

        // Anything but loopback is refused before a connection is attempted.
        let err = DapClient::connect_tcp_addr(SocketAddr::from(([10, 0, 0, 5], 4000)))
            .await
            .unwrap_err();
        assert!(err.contains("only a loopback"), "{err}");
    }

    /// A helper an adapter script starts in its own process group — a
    /// debuggee, a worker — before the leader exits: the pid is read from
    /// the file the script writes it to.
    #[cfg(unix)]
    async fn helper_pid(pidfile: &Path) -> libc::pid_t {
        for _ in 0..500 {
            if let Some(pid) = std::fs::read_to_string(pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the helper never started");
    }

    /// Whether `pid` is gone (within a couple of seconds).
    #[cfg(unix)]
    async fn gone(pid: libc::pid_t) -> bool {
        for _ in 0..200 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// F9: an adapter that dies before announcing ends the start at once,
    /// with its exit status — not after a retry budget — and what it started
    /// in its process group goes with it (the wait sweeps the group before
    /// reaping the leader: a plain wait left it running).
    #[tokio::test]
    #[cfg(unix)]
    async fn an_adapter_that_exits_before_announcing_fails_fast() {
        let dir = clew_core::testutil::TempDir::new("dap-exit-early");
        let pidfile = dir.join("helper.pid");
        let started = std::time::Instant::now();
        let args = vec![
            "-c".to_string(),
            "sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; echo 'bad flag' >&2; exit 3".to_string(),
            "sh".to_string(),
            pidfile.display().to_string(),
        ];
        let err = DapClient::start_with(
            Path::new("/bin/sh"),
            &args,
            Path::new("/"),
            Transport::Tcp,
            Duration::from_secs(20),
        )
        .await
        .unwrap_err();
        assert!(err.contains("exited"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let helper = helper_pid(&pidfile).await;
        assert!(gone(helper).await, "the adapter's helper outlived it");
    }

    /// The same when what the adapter started keeps the adapter's output
    /// open: no end of output ever comes, so only the wait on the adapter
    /// itself (`framing::wait_sweeping`, in `await_listening`) sees it go —
    /// at once, with its status — and its sweep takes the helper along
    /// (which is also what finally closes the output). A plain wait saw the
    /// exit and left the helper running; without that wait, the start sat
    /// out its whole time limit.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_adapter_that_exits_while_its_helper_holds_its_output_fails_fast() {
        let dir = clew_core::testutil::TempDir::new("dap-exit-held-output");
        let pidfile = dir.join("helper.pid");
        let started = std::time::Instant::now();
        let args = vec![
            "-c".to_string(),
            // The helper inherits the adapter's stdout, so it never closes.
            "sleep 30 & echo $! > \"$1\"; exit 3".to_string(),
            "sh".to_string(),
            pidfile.display().to_string(),
        ];
        let err = DapClient::start_with(
            Path::new("/bin/sh"),
            &args,
            Path::new("/"),
            Transport::Tcp,
            Duration::from_secs(20),
        )
        .await
        .unwrap_err();
        assert!(err.contains("exited"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let helper = helper_pid(&pidfile).await;
        assert!(gone(helper).await, "the adapter's helper outlived it");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn an_adapter_that_never_announces_times_out() {
        let args = vec!["-c".to_string(), "exec sleep 30".to_string()];
        let err = DapClient::start_with(
            Path::new("/bin/sh"),
            &args,
            Path::new("/"),
            Transport::Tcp,
            Duration::from_millis(300),
        )
        .await
        .unwrap_err();
        assert!(err.contains("did not report its listening port"), "{err}");
    }

    /// F9/F17: `connect_retry` gives up the moment the adapter process is
    /// gone. It used to retry its whole budget against a port nobody would
    /// ever open.
    #[tokio::test]
    #[cfg(unix)]
    async fn connect_retry_stops_when_the_child_exits() {
        // A port with nothing behind it.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        // The adapter leaves a helper in its process group and exits — not
        // reaped yet, as the connect loop finds it.
        let dir = clew_core::testutil::TempDir::new("dap-connect-exit");
        let pidfile = dir.join("helper.pid");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; exit 0")
            .arg("sh")
            .arg(&pidfile)
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .unwrap();
        let helper = helper_pid(&pidfile).await;
        let started = std::time::Instant::now();
        let err = connect_retry(closed, Some(&mut child), Duration::from_secs(20))
            .await
            .unwrap_err();
        assert!(err.contains("exited"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(gone(helper).await, "the adapter's helper outlived it");

        // Without a child the retries are bounded by the limit.
        let err = connect_retry(closed, None, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.contains("could not connect"), "{err}");
    }
}
