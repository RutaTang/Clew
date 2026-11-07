//! `clew-server node-debug --node <node>`: a debug adapter for Node.js of
//! Clew's own. It speaks DAP on stdin/stdout and, to the program, the V8
//! inspector protocol (the Chrome DevTools Protocol, CDP) that Node serves
//! over a WebSocket when it runs with `--inspect-brk`.
//!
//! On this machine Clew debugs Node under vscode-js-debug, which opens a
//! connection of its own for each process it debugs — something one stdio
//! channel cannot carry. This adapter needs no second connection, so a
//! remote host serves Node programs through it.
//!
//! One program and its main thread (worker threads and child processes are
//! not followed): breakpoints, with conditions; stepping in, over and out; a
//! real pause; exceptions caught or uncaught; the stack, each scope's
//! variables and `this`; and expressions evaluated by V8 itself — for a
//! hover, with nothing allowed that has a side effect.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use clew_core::framing::read_message;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::dapout::{Out, forward};

/// How long Node has to say where its inspector listens, and to shake hands.
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a CDP command may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the program's last output has, once it exits, to be sent.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// The largest CDP message read (a script's source, a big object).
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The most properties an object shows.
const MAX_CHILDREN: usize = 100;

/// How many steps a pause takes out of Node's own code toward the program's.
const MAX_SKIPPED: u32 = 1000;

/// The program's one thread, as DAP knows it.
const THREAD: u64 = 1;

/// Run the adapter on this process's stdin/stdout, with `node` the runtime
/// that runs the program.
pub async fn run(node: PathBuf) -> Result<(), String> {
    serve(node, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(node: PathBuf, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let out = Out::new(output);
    let mut adapter = Adapter {
        node,
        out: out.clone(),
        cdp: None,
        program: None,
        stop_on_entry: false,
        state: Arc::default(),
    };
    let mut input = BufReader::new(input);
    while let Ok(Some(message)) = read_message(&mut input).await {
        if message["type"] != "request" {
            continue;
        }
        if !adapter.handle(&message).await {
            break;
        }
    }
    adapter.end().await;
    Ok(())
}

// -- the WebSocket -----------------------------------------------------------

/// The writing half of a client's WebSocket: every frame masked, as a
/// client's must be.
struct WsWrite {
    stream: Box<dyn AsyncWrite + Unpin + Send>,
    mask: u32,
}

impl WsWrite {
    async fn frame(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n <= 0xFFFF => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        // On a loopback connection the mask need not be secret, only vary.
        self.mask = self
            .mask
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let key = self.mask.to_be_bytes();
        frame.extend_from_slice(&key);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        self.stream.write_all(&frame).await?;
        self.stream.flush().await
    }
}

/// The next message, a text or binary one whole (its fragments joined),
/// pings answered on the way; none once the connection closes.
async fn ws_message<R>(read: &mut R, write: &Mutex<WsWrite>) -> Option<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut message = Vec::new();
    loop {
        let mut head = [0u8; 2];
        read.read_exact(&mut head).await.ok()?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let length = match head[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                read.read_exact(&mut b).await.ok()?;
                u64::from(u16::from_be_bytes(b))
            }
            127 => {
                let mut b = [0u8; 8];
                read.read_exact(&mut b).await.ok()?;
                u64::from_be_bytes(b)
            }
            n => u64::from(n),
        };
        let length = usize::try_from(length)
            .ok()
            .filter(|n| message.len().saturating_add(*n) <= MAX_MESSAGE)?;
        let mut key = [0u8; 4];
        let masked = head[1] & 0x80 != 0;
        if masked {
            read.read_exact(&mut key).await.ok()?;
        }
        let mut payload = vec![0u8; length];
        read.read_exact(&mut payload).await.ok()?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i % 4];
            }
        }
        match opcode {
            0x8 => return None,
            0x9 => {
                let _ = write.lock().await.frame(0xA, &payload).await;
            }
            0xA => {}
            _ => {
                message.extend_from_slice(&payload);
                if fin {
                    return Some(message);
                }
            }
        }
    }
}

/// Open the WebSocket at `url` (`ws://host:port/path`): the HTTP upgrade
/// asked for and granted.
async fn ws_connect(
    url: &str,
) -> Result<(BufReader<tokio::net::tcp::OwnedReadHalf>, WsWrite), String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| format!("not a ws:// address: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let stream = tokio::net::TcpStream::connect(authority)
        .await
        .map_err(|e| format!("cannot reach Node's inspector: {e}"))?;
    let (read, mut write) = stream.into_split();
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (u64::from(std::process::id()) << 32);
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&seed.to_be_bytes());
    key[8..].copy_from_slice(&seed.rotate_left(29).to_le_bytes());
    let key = base64::engine::general_purpose::STANDARD.encode(key);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    write
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("cannot reach Node's inspector: {e}"))?;
    let mut read = BufReader::new(read);
    let mut status = String::new();
    read.read_line(&mut status)
        .await
        .map_err(|e| format!("Node's inspector did not answer: {e}"))?;
    if status.split_whitespace().nth(1) != Some("101") {
        return Err(format!(
            "Node's inspector refused the connection: {}",
            status.trim()
        ));
    }
    loop {
        let mut header = String::new();
        let n = read
            .read_line(&mut header)
            .await
            .map_err(|e| format!("Node's inspector did not answer: {e}"))?;
        if n == 0 || header == "\r\n" || header == "\n" {
            break;
        }
    }
    Ok((
        read,
        WsWrite {
            stream: Box::new(write),
            mask: seed as u32,
        },
    ))
}

// -- CDP ---------------------------------------------------------------------

/// The commands waiting for their replies, by id.
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// The connection to Node's inspector: commands out, replies routed by id,
/// events sent on.
struct Cdp {
    write: Arc<Mutex<WsWrite>>,
    waiting: Waiting,
    next: AtomicU64,
    /// The program is gone: a command fails at once rather than waiting.
    closed: Arc<AtomicBool>,
}

impl Cdp {
    /// Read `read`'s messages: replies to their waiting command, events to
    /// `events`.
    fn start<R>(read: R, write: WsWrite, events: mpsc::UnboundedSender<Value>) -> Arc<Cdp>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let write = Arc::new(Mutex::new(write));
        let waiting: Waiting = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let cdp = Arc::new(Cdp {
            write: write.clone(),
            waiting: waiting.clone(),
            next: AtomicU64::new(1),
            closed: closed.clone(),
        });
        tokio::spawn(async move {
            let mut read = read;
            while let Some(bytes) = ws_message(&mut read, &write).await {
                let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                match message["id"].as_u64() {
                    Some(id) => {
                        if let Some(waiter) = waiting.lock().await.remove(&id) {
                            let _ = waiter.send(match message.get("error") {
                                Some(error) => Err(error["message"]
                                    .as_str()
                                    .unwrap_or("the inspector refused")
                                    .to_string()),
                                None => Ok(message["result"].clone()),
                            });
                        }
                    }
                    None => {
                        let _ = events.send(message);
                    }
                }
            }
            // The program is gone: whoever waits hears so, and whoever asks
            // next.
            let mut waiting = waiting.lock().await;
            closed.store(true, Ordering::SeqCst);
            waiting.clear();
        });
        cdp
    }

    /// Send `method` with `params`, and wait for its result.
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiting = self.waiting.lock().await;
            if self.closed.load(Ordering::SeqCst) {
                return Err("the program is gone".into());
            }
            waiting.insert(id, tx);
        }
        let text = json!({ "id": id, "method": method, "params": params }).to_string();
        self.write
            .lock()
            .await
            .frame(0x1, text.as_bytes())
            .await
            .map_err(|e| format!("the program is gone: {e}"))?;
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err("the program is gone".into()),
            Err(_) => {
                self.waiting.lock().await.remove(&id);
                Err("Node did not answer in time".into())
            }
        }
    }

    /// Close the connection — Node, waiting at its end for its debugger to
    /// go, then exits.
    async fn close(&self) {
        let mut write = self.write.lock().await;
        let _ = write.frame(0x8, &[]).await;
        let _ = write.stream.shutdown().await;
    }
}

// -- the adapter -------------------------------------------------------------

/// A breakpoint set: the DAP id, the inspector's, and its line.
#[derive(Debug, Clone)]
struct Breakpoint {
    id: u64,
    cdp: String,
    line: u64,
}

/// The pause Node makes at the program's first line (`--inspect-brk`): passed
/// unless the launch asked to stop on entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Entry {
    #[default]
    Done,
    Pass,
    Stop,
}

#[derive(Default)]
struct State {
    /// The call frames of the pause the program is in; none while it runs.
    frames: Vec<Value>,
    /// What each `variablesReference` opens: an object (a scope's, or a
    /// value's) and, for a frame's local scope, its `this`.
    refs: Vec<(String, Option<Value>)>,
    /// What the next pause is: a step's end, or the pause asked for.
    stepping: bool,
    pausing: bool,
    entry: Entry,
    /// Each file's breakpoints.
    breakpoints: HashMap<String, Vec<Breakpoint>>,
    /// The program's own context: its end is the program's.
    main_context: Option<u64>,
    /// Each script's URL, by its id: a call frame names its script by id.
    scripts: HashMap<String, String>,
    /// Steps taken out of Node's own code toward the program's, for a pause.
    skipped: u32,
}

/// The program `launch` started: its exit code once it has one, and how to
/// stop it.
struct Program {
    exited: watch::Receiver<Option<i32>>,
    kill: Option<oneshot::Sender<()>>,
}

struct Adapter {
    node: PathBuf,
    out: Out,
    cdp: Option<Arc<Cdp>>,
    program: Option<Program>,
    stop_on_entry: bool,
    state: Arc<Mutex<State>>,
}

static NEXT_BREAKPOINT: AtomicU64 = AtomicU64::new(1);

impl Adapter {
    /// Answer `request`; false once the session is over.
    async fn handle(&mut self, request: &Value) -> bool {
        let command = request["command"].as_str().unwrap_or("");
        let args = &request["arguments"];
        let answer = match command {
            "initialize" => Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsConditionalBreakpoints": true,
                "supportsEvaluateForHovers": true,
                "supportsTerminateRequest": true,
                "exceptionBreakpointFilters": [
                    { "filter": "uncaught", "label": "Uncaught exceptions" },
                    { "filter": "all", "label": "All exceptions" },
                ],
            })),
            "launch" => match self.launch(args).await {
                Ok(()) => {
                    self.out.respond(request, Ok(json!({}))).await;
                    self.out.event("initialized", json!({})).await;
                    return true;
                }
                Err(e) => Err(e),
            },
            "setBreakpoints" => self.set_breakpoints(args).await,
            "setExceptionBreakpoints" => self.set_exception_breakpoints(args).await,
            "configurationDone" => match self.cdp() {
                Ok(cdp) => {
                    self.state.lock().await.entry = match self.stop_on_entry {
                        true => Entry::Stop,
                        false => Entry::Pass,
                    };
                    cdp.call("Runtime.runIfWaitingForDebugger", json!({}))
                        .await
                        .map(|_| json!({}))
                }
                Err(e) => Err(e),
            },
            "threads" => Ok(json!({ "threads": [{ "id": THREAD, "name": "main" }] })),
            "continue" | "next" | "stepIn" | "stepOut" | "pause" => {
                let cdp = match self.cdp() {
                    Ok(cdp) => cdp,
                    Err(e) => {
                        self.out.respond(request, Err(e)).await;
                        return true;
                    }
                };
                let method = match command {
                    "continue" => "Debugger.resume",
                    "next" => "Debugger.stepOver",
                    "stepIn" => "Debugger.stepInto",
                    "stepOut" => "Debugger.stepOut",
                    _ => "Debugger.pause",
                };
                {
                    let mut state = self.state.lock().await;
                    state.stepping = matches!(command, "next" | "stepIn" | "stepOut");
                    state.pausing = command == "pause";
                }
                // Answered before the program runs: its next stop is never
                // reported ahead of the answer.
                let body = match command {
                    "pause" => json!({}),
                    _ => json!({ "allThreadsContinued": true }),
                };
                self.out.respond(request, Ok(body)).await;
                if let Err(e) = cdp.call(method, json!({})).await {
                    self.out
                        .output("console", format!("{command}: {e}\n"))
                        .await;
                }
                return true;
            }
            "stackTrace" => self.stack_trace(args).await,
            "scopes" => self.scopes(args).await,
            "variables" => self.variables(args).await,
            "evaluate" => self.evaluate(args).await,
            "disconnect" | "terminate" => {
                self.out.respond(request, Ok(json!({}))).await;
                return false;
            }
            other => Err(format!("{other} is not supported")),
        };
        self.out.respond(request, answer).await;
        true
    }

    fn cdp(&self) -> Result<Arc<Cdp>, String> {
        self.cdp
            .clone()
            .ok_or_else(|| "no program is running".to_string())
    }

    /// Run the program under `node --inspect-brk`, held at its first line,
    /// and connect to its inspector.
    async fn launch(&mut self, args: &Value) -> Result<(), String> {
        let program = args["program"]
            .as_str()
            .filter(|p| !p.is_empty())
            .ok_or("no program to run")?
            .to_string();
        let program_args: Vec<String> = args["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.stop_on_entry = args["stopOnEntry"].as_bool().unwrap_or(false);
        let mut command = clew_core::spawn::tokio_command(&self.node);
        command.arg("--inspect-brk=127.0.0.1:0");
        // TypeScript runs as Node runs it, its types stripped — blanked in
        // place, so each line is where the source has it.
        if is_typescript(&program) {
            let version = node_version(&self.node).await?;
            command.args(strip_types_flags(version)?);
        }
        command
            .arg(&program)
            .args(&program_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = args["cwd"].as_str().filter(|c| !c.is_empty()) {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", self.node.display()))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut stderr = BufReader::new(child.stderr.take().ok_or("no stderr")?);
        let url = match tokio::time::timeout(START_TIMEOUT, inspector_url(&mut stderr)).await {
            Ok(found) => found,
            Err(_) => Err(format!(
                "Node did not open its inspector within {} s",
                START_TIMEOUT.as_secs()
            )),
        };
        let url = match url {
            Ok(url) => url,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e);
            }
        };
        let forwarders = vec![
            tokio::spawn(forward(Box::new(stdout), "stdout", self.out.clone())),
            tokio::spawn(forward_stderr(stderr, self.out.clone())),
        ];
        let (exit_tx, exited) = watch::channel(None);
        let (kill, killed) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = killed => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            let _ = exit_tx.send(Some(status.and_then(|s| s.code()).unwrap_or(-1)));
        });
        // The end, said once its output is out.
        let mut ended = exited.clone();
        let out = self.out.clone();
        tokio::spawn(async move {
            let code = match ended.wait_for(Option::is_some).await {
                Ok(code) => code.unwrap_or(-1),
                Err(_) => -1,
            };
            for forwarder in forwarders {
                let _ = tokio::time::timeout(EXIT_GRACE, forwarder).await;
            }
            out.terminated(Some(code)).await;
        });
        self.program = Some(Program {
            exited,
            kill: Some(kill),
        });

        let (read, write) = match tokio::time::timeout(START_TIMEOUT, ws_connect(&url)).await {
            Ok(connected) => connected?,
            Err(_) => return Err("Node's inspector did not answer".into()),
        };
        let (events_tx, events) = mpsc::unbounded_channel();
        let cdp = Cdp::start(read, write, events_tx);
        tokio::spawn(event_loop(
            cdp.clone(),
            events,
            self.state.clone(),
            self.out.clone(),
        ));
        cdp.call("Runtime.enable", json!({})).await?;
        cdp.call("Debugger.enable", json!({})).await?;
        // Node's own modules are not stepped into.
        let _ = cdp
            .call(
                "Debugger.setBlackboxPatterns",
                json!({ "patterns": ["^node:"] }),
            )
            .await;
        self.cdp = Some(cdp);
        Ok(())
    }

    /// Replace one file's breakpoints.
    async fn set_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let cdp = self.cdp()?;
        let path = args["source"]["path"]
            .as_str()
            .ok_or("a breakpoint needs its file's path")?
            .to_string();
        let wanted: Vec<(u64, Option<String>)> = args["breakpoints"]
            .as_array()
            .map(|bps| {
                bps.iter()
                    .filter_map(|bp| {
                        let line = bp["line"].as_u64().filter(|l| *l > 0)?;
                        let condition = bp["condition"]
                            .as_str()
                            .filter(|c| !c.trim().is_empty())
                            .map(str::to_string);
                        Some((line, condition))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let old = self
            .state
            .lock()
            .await
            .breakpoints
            .remove(&path)
            .unwrap_or_default();
        for breakpoint in old {
            let _ = cdp
                .call(
                    "Debugger.removeBreakpoint",
                    json!({ "breakpointId": breakpoint.cdp }),
                )
                .await;
        }
        let regex = url_regex(&path);
        let mut set: Vec<Breakpoint> = Vec::new();
        let mut answers = Vec::new();
        for (line, condition) in wanted {
            let id = NEXT_BREAKPOINT.fetch_add(1, Ordering::Relaxed);
            // The same line twice is the one breakpoint.
            if let Some(same) = set.iter().find(|b| b.line == line) {
                answers.push(json!({ "id": same.id, "verified": true, "line": same.line }));
                continue;
            }
            let mut params = json!({ "lineNumber": line - 1, "urlRegex": regex });
            if let Some(condition) = condition {
                params["condition"] = json!(condition);
            }
            match cdp.call("Debugger.setBreakpointByUrl", params).await {
                Ok(reply) => {
                    let resolved = reply["locations"][0]["lineNumber"].as_u64().map(|l| l + 1);
                    set.push(Breakpoint {
                        id,
                        cdp: reply["breakpointId"].as_str().unwrap_or("").to_string(),
                        line: resolved.unwrap_or(line),
                    });
                    answers.push(json!({
                        "id": id,
                        "verified": resolved.is_some(),
                        "line": resolved.unwrap_or(line),
                    }));
                }
                Err(e) => answers.push(json!({
                    "id": id, "verified": false, "line": line, "message": e,
                })),
            }
        }
        self.state.lock().await.breakpoints.insert(path, set);
        Ok(json!({ "breakpoints": answers }))
    }

    async fn set_exception_breakpoints(&self, args: &Value) -> Result<Value, String> {
        let filters: Vec<&str> = args["filters"]
            .as_array()
            .map(|f| f.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let state = match () {
            _ if filters.contains(&"all") => "all",
            _ if filters.contains(&"uncaught") => "uncaught",
            _ => "none",
        };
        self.cdp()?
            .call("Debugger.setPauseOnExceptions", json!({ "state": state }))
            .await?;
        Ok(json!({}))
    }

    async fn stack_trace(&self, args: &Value) -> Result<Value, String> {
        let state = self.state.lock().await;
        let start = args["startFrame"].as_u64().unwrap_or(0) as usize;
        let levels = args["levels"]
            .as_u64()
            .filter(|n| *n > 0)
            .map_or(usize::MAX, |n| n as usize);
        let frames: Vec<Value> = state
            .frames
            .iter()
            .enumerate()
            .skip(start)
            .take(levels)
            .map(|(i, frame)| dap_frame(i + 1, frame, &state.scripts))
            .collect();
        Ok(json!({ "stackFrames": frames, "totalFrames": state.frames.len() }))
    }

    async fn scopes(&self, args: &Value) -> Result<Value, String> {
        let mut state = self.state.lock().await;
        let frame = frame_at(&state, args)?.clone();
        let mut scopes = Vec::new();
        for scope in frame["scopeChain"].as_array().into_iter().flatten() {
            let Some(object) = scope["object"]["objectId"].as_str() else {
                continue;
            };
            let kind = scope["type"].as_str().unwrap_or("scope");
            let this = (kind == "local").then(|| frame["this"].clone());
            state.refs.push((object.to_string(), this));
            scopes.push(json!({
                "name": scope_name(kind),
                "variablesReference": state.refs.len(),
                // The global scope is all of Node's: opened only when asked.
                "expensive": kind == "global",
            }));
        }
        Ok(json!({ "scopes": scopes }))
    }

    async fn variables(&self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
        let (object, this) = {
            let state = self.state.lock().await;
            reference
                .checked_sub(1)
                .and_then(|i| state.refs.get(i))
                .cloned()
                .ok_or("those variables are gone: the program ran on")?
        };
        let reply = self
            .cdp()?
            .call(
                "Runtime.getProperties",
                json!({ "objectId": object, "ownProperties": true, "generatePreview": true }),
            )
            .await?;
        let mut state = self.state.lock().await;
        let mut variables = Vec::new();
        if let Some(this) = this.filter(|t| t["type"].is_string() && t["type"] != "undefined") {
            variables.push(shown(&mut state, "this", &this));
        }
        let properties = reply["result"].as_array().into_iter().flatten();
        let entries = reply["internalProperties"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["name"] == "[[Entries]]");
        for property in properties.chain(entries) {
            if variables.len() >= MAX_CHILDREN {
                break;
            }
            let name = property["name"].as_str().unwrap_or("");
            if name == "__proto__" {
                continue;
            }
            match property.get("value") {
                Some(value) => variables.push(shown(&mut state, name, value)),
                // An accessor: read only when asked, as reading runs code.
                None => variables.push(json!({
                    "name": name, "value": "(getter)", "variablesReference": 0,
                })),
            }
        }
        Ok(json!({ "variables": variables }))
    }

    /// An expression, evaluated by V8: in a frame of the pause, or — none
    /// given — in the program's global scope. A hover's may have no side
    /// effect: V8 refuses one that would.
    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").to_string();
        let hover = args["context"] == "hover";
        let call_frame = match args["frameId"].as_u64() {
            Some(_) => {
                let state = self.state.lock().await;
                Some(frame_at(&state, args)?["callFrameId"].clone())
            }
            None => None,
        };
        let cdp = self.cdp()?;
        let reply = match call_frame {
            Some(call_frame) => {
                cdp.call(
                    "Debugger.evaluateOnCallFrame",
                    json!({
                        "callFrameId": call_frame,
                        "expression": expression,
                        "generatePreview": true,
                        "silent": true,
                        "throwOnSideEffect": hover,
                    }),
                )
                .await?
            }
            None => {
                cdp.call(
                    "Runtime.evaluate",
                    json!({
                        "expression": expression,
                        "generatePreview": true,
                        "silent": true,
                        "throwOnSideEffect": hover,
                        "includeCommandLineAPI": !hover,
                    }),
                )
                .await?
            }
        };
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(exception_text(details));
        }
        let mut state = self.state.lock().await;
        let shown = shown(&mut state, "", &reply["result"]);
        Ok(json!({
            "result": shown["value"],
            "variablesReference": shown["variablesReference"],
        }))
    }

    /// The session is over: the connection closed, the program stopped.
    async fn end(&mut self) {
        if let Some(cdp) = self.cdp.take() {
            cdp.close().await;
        }
        if let Some(mut program) = self.program.take() {
            if let Some(kill) = program.kill.take() {
                let _ = kill.send(());
            }
            let _ =
                tokio::time::timeout(EXIT_GRACE, program.exited.wait_for(Option::is_some)).await;
        }
    }
}

/// The inspector's events: stops reported, breakpoints confirmed as their
/// scripts load, and the connection closed when the program's context ends
/// (Node waits at its end for its debugger to go).
async fn event_loop(
    cdp: Arc<Cdp>,
    mut events: mpsc::UnboundedReceiver<Value>,
    state: Arc<Mutex<State>>,
    out: Out,
) {
    while let Some(event) = events.recv().await {
        let params = &event["params"];
        match event["method"].as_str().unwrap_or("") {
            "Debugger.scriptParsed" => {
                if let (Some(id), Some(url)) = (params["scriptId"].as_str(), params["url"].as_str())
                {
                    state
                        .lock()
                        .await
                        .scripts
                        .insert(id.to_string(), url.to_string());
                }
            }
            "Runtime.executionContextCreated" => {
                let mut state = state.lock().await;
                if state.main_context.is_none() {
                    state.main_context = params["context"]["id"].as_u64();
                }
            }
            "Runtime.executionContextDestroyed" => {
                let id = params["executionContextId"].as_u64();
                if id.is_some() && state.lock().await.main_context == id {
                    cdp.close().await;
                }
            }
            "Debugger.breakpointResolved" => {
                let Some(line) = params["location"]["lineNumber"].as_u64() else {
                    continue;
                };
                let mut state = state.lock().await;
                let found = state
                    .breakpoints
                    .values_mut()
                    .flatten()
                    .find(|b| params["breakpointId"] == b.cdp.as_str());
                if let Some(breakpoint) = found {
                    breakpoint.line = line + 1;
                    let body = json!({
                        "reason": "changed",
                        "breakpoint": { "id": breakpoint.id, "verified": true, "line": line + 1 },
                    });
                    drop(state);
                    out.event("breakpoint", body).await;
                }
            }
            "Debugger.paused" => {
                let hit = params["hitBreakpoints"]
                    .as_array()
                    .is_some_and(|h| !h.is_empty());
                let exception = matches!(
                    params["reason"].as_str(),
                    Some("exception" | "promiseRejection")
                );
                let mut st = state.lock().await;
                let entry = std::mem::take(&mut st.entry);
                if entry == Entry::Pass && !hit {
                    drop(st);
                    let _ = cdp.call("Debugger.resume", json!({})).await;
                    continue;
                }
                // A pause lands in whatever JavaScript runs next — often
                // Node's own, about to call the program's (a timer's, an
                // I/O callback's): stepped on, into the program's code, up
                // to a bound. V8 passes over ignored scripts only when
                // stepping in from code that is not.
                let top = params["callFrames"][0].clone();
                if st.pausing
                    && !hit
                    && !exception
                    && st.skipped < MAX_SKIPPED
                    && frame_url(&top, &st.scripts).starts_with("node:")
                {
                    st.skipped += 1;
                    drop(st);
                    let _ = cdp.call("Debugger.stepInto", json!({})).await;
                    continue;
                }
                st.skipped = 0;
                let reason = match () {
                    _ if hit => "breakpoint",
                    _ if entry == Entry::Stop => "entry",
                    _ if exception => "exception",
                    _ if st.stepping => "step",
                    _ if st.pausing => "pause",
                    // A `debugger;` statement.
                    _ => "breakpoint",
                };
                st.stepping = false;
                st.pausing = false;
                st.frames = params["callFrames"].as_array().cloned().unwrap_or_default();
                st.refs.clear();
                drop(st);
                let mut body = json!({
                    "reason": reason,
                    "threadId": THREAD,
                    "allThreadsStopped": true,
                });
                if exception {
                    let text = params["data"]["description"]
                        .as_str()
                        .or(params["data"]["value"].as_str())
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_string();
                    body["text"] = json!(text);
                }
                out.event("stopped", body).await;
            }
            "Debugger.resumed" => {
                let mut state = state.lock().await;
                state.frames.clear();
                state.refs.clear();
            }
            _ => {}
        }
    }
}

/// The frame a request names by its `frameId`.
fn frame_at<'a>(state: &'a State, args: &Value) -> Result<&'a Value, String> {
    args["frameId"]
        .as_u64()
        .and_then(|id| (id as usize).checked_sub(1))
        .and_then(|i| state.frames.get(i))
        .ok_or_else(|| "that frame is gone: the program ran on".to_string())
}

/// A CDP call frame as a DAP stack frame; `scripts` has each script's URL,
/// which V8 no longer puts in the frame itself.
fn dap_frame(id: usize, frame: &Value, scripts: &HashMap<String, String>) -> Value {
    let url = frame_url(frame, scripts);
    let name = frame["functionName"]
        .as_str()
        .filter(|n| !n.is_empty())
        .unwrap_or("(anonymous)");
    let mut out = json!({
        "id": id,
        "name": name,
        "line": frame["location"]["lineNumber"].as_u64().unwrap_or(0) + 1,
        "column": frame["location"]["columnNumber"].as_u64().unwrap_or(0) + 1,
    });
    let path = match url {
        _ if url.starts_with("file://") => Some(uri_to_path(url)),
        _ if url.starts_with('/') => Some(url.to_string()),
        _ => None,
    };
    match path {
        Some(path) => {
            let file = path.rsplit('/').next().unwrap_or(&path).to_string();
            out["source"] = json!({ "name": file, "path": path });
        }
        // Node's own modules, and code from `eval`: no file to open.
        None => {
            let shown = match url {
                "" => "<eval>",
                url => url,
            };
            out["source"] = json!({ "name": shown });
            out["presentationHint"] = json!("subtle");
        }
    }
    out
}

/// The URL of a frame's script.
fn frame_url<'a>(frame: &'a Value, scripts: &'a HashMap<String, String>) -> &'a str {
    frame["url"]
        .as_str()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let script = frame["location"]["scriptId"].as_str()?;
            scripts.get(script).map(String::as_str)
        })
        .unwrap_or("")
}

/// A scope's name as a debugger shows it.
fn scope_name(kind: &str) -> String {
    match kind {
        "local" => "Local".into(),
        "closure" => "Closure".into(),
        "block" => "Block".into(),
        "script" => "Script".into(),
        "module" => "Module".into(),
        "global" => "Global".into(),
        "catch" => "Catch".into(),
        "with" => "With".into(),
        "eval" => "Eval".into(),
        other => other.to_string(),
    }
}

/// A CDP value (a `RemoteObject`) as a DAP variable named `name`: an object
/// given a reference to open it by.
fn shown(state: &mut State, name: &str, value: &Value) -> Value {
    let reference = match (value["type"].as_str(), value["objectId"].as_str()) {
        (Some("object"), Some(object)) if value["subtype"] != "null" => {
            state.refs.push((object.to_string(), None));
            state.refs.len()
        }
        _ => 0,
    };
    json!({ "name": name, "value": describe(value), "variablesReference": reference })
}

/// A value as a debugger writes it: a string quoted, an object by its class
/// with a preview of what is in it.
fn describe(value: &Value) -> String {
    let description = value["description"].as_str().unwrap_or("");
    let first_line = description.lines().next().unwrap_or("").to_string();
    match value["type"].as_str().unwrap_or("") {
        "undefined" => "undefined".into(),
        "string" => {
            serde_json::to_string(value["value"].as_str().unwrap_or("")).unwrap_or_default()
        }
        "boolean" => value["value"].to_string(),
        "number" | "bigint" | "symbol" => match description {
            "" => value["value"].to_string(),
            d => d.to_string(),
        },
        "function" => {
            // A function's description is its source: its head is enough.
            let head = first_line.split('{').next().unwrap_or("").trim();
            let head: String = head.chars().take(80).collect();
            format!("ƒ {head}")
        }
        "object" if value["subtype"] == "null" => "null".into(),
        "object" => match value.get("preview") {
            Some(preview) => with_preview(&first_line, preview),
            None => first_line,
        },
        other => other.to_string(),
    }
}

/// `Cart {owner: 'ann', items: Array(2)}`, `Array(2) [2, 3]`,
/// `Map(1) {'a' => 1}`.
fn with_preview(description: &str, preview: &Value) -> String {
    let more = match preview["overflow"].as_bool() {
        Some(true) => ", …",
        _ => "",
    };
    if let Some(entries) = preview["entries"].as_array() {
        let shown: Vec<String> = entries
            .iter()
            .map(|e| match e.get("key") {
                Some(key) => format!("{} => {}", preview_text(key), preview_text(&e["value"])),
                None => preview_text(&e["value"]),
            })
            .collect();
        return format!("{description} {{{}{more}}}", shown.join(", "));
    }
    let properties = preview["properties"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if preview["subtype"] == "array" || preview["subtype"] == "typedarray" {
        let items: Vec<String> = properties.iter().map(property_text).collect();
        return format!("{description} [{}{more}]", items.join(", "));
    }
    let fields: Vec<String> = properties
        .iter()
        .map(|p| format!("{}: {}", p["name"].as_str().unwrap_or(""), property_text(p)))
        .collect();
    match description {
        "Object" => format!("{{{}{more}}}", fields.join(", ")),
        _ if fields.is_empty() && more.is_empty() => description.to_string(),
        _ => format!("{description} {{{}{more}}}", fields.join(", ")),
    }
}

/// A property of a preview, as a preview shows it.
fn property_text(property: &Value) -> String {
    let text = property["value"].as_str().unwrap_or("");
    match property["type"].as_str() {
        Some("string") => format!("'{text}'"),
        Some("function") => "ƒ".into(),
        Some("undefined") => "undefined".into(),
        _ => text.to_string(),
    }
}

/// An entry's key or value in a preview: an object preview of its own.
fn preview_text(preview: &Value) -> String {
    match preview["type"].as_str() {
        Some("string") => format!("'{}'", preview["description"].as_str().unwrap_or("")),
        _ => preview["description"].as_str().unwrap_or("").to_string(),
    }
}

/// What an exception says: its first line (an `Error`'s description carries
/// its stack after it).
fn exception_text(details: &Value) -> String {
    let exception = &details["exception"];
    let text = exception["description"]
        .as_str()
        .or(exception["value"].as_str())
        .or(details["text"].as_str())
        .unwrap_or("it threw");
    text.lines().next().unwrap_or(text).to_string()
}

/// Whether `program` is TypeScript Node can strip (not TSX).
fn is_typescript(program: &str) -> bool {
    [".ts", ".mts", ".cts"]
        .iter()
        .any(|ext| program.to_ascii_lowercase().ends_with(ext))
}

/// `node --version`, as (major, minor).
async fn node_version(node: &std::path::Path) -> Result<(u32, u32), String> {
    let output = tokio::time::timeout(
        START_TIMEOUT,
        clew_core::spawn::tokio_command(node)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "node --version did not answer".to_string())?
    .map_err(|e| format!("cannot run {}: {e}", node.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.trim().trim_start_matches('v').split('.');
    match (
        parts.next().and_then(|p| p.parse().ok()),
        parts.next().and_then(|p| p.parse().ok()),
    ) {
        (Some(major), Some(minor)) => Ok((major, minor)),
        _ => Err(format!("node gave no version: {}", text.trim())),
    }
}

/// What Node `version` needs to run TypeScript: nothing from 22.18 and
/// 23.6, where it strips types by default; the flag from 22.6; and before
/// that, it cannot.
fn strip_types_flags(version: (u32, u32)) -> Result<Vec<&'static str>, String> {
    match version {
        (major, _) if major >= 24 => Ok(vec![]),
        (23, minor) if minor >= 6 => Ok(vec![]),
        (23, _) => Ok(vec!["--experimental-strip-types"]),
        (22, minor) if minor >= 18 => Ok(vec![]),
        (22, minor) if minor >= 6 => Ok(vec!["--experimental-strip-types"]),
        (major, minor) => Err(format!(
            "a TypeScript program needs Node 22.6 or later, which strips its types; \
             this host has {major}.{minor} — run its compiled JavaScript instead"
        )),
    }
}

/// The `ws://` address Node prints once its inspector listens; what else it
/// printed first is the error when it never does.
async fn inspector_url<R>(stderr: &mut BufReader<R>) -> Result<String, String>
where
    R: AsyncRead + Unpin,
{
    let mut said = String::new();
    loop {
        let mut line = String::new();
        let n = stderr
            .read_line(&mut line)
            .await
            .map_err(|e| format!("cannot read node's output: {e}"))?;
        if n == 0 {
            let said = said.trim();
            return Err(match said {
                "" => "node ended before its inspector listened".to_string(),
                said => format!("Node.js exited before its debugger started listening: {said}"),
            });
        }
        if let Some(url) = line
            .trim()
            .strip_prefix("Debugger listening on ")
            .filter(|u| u.starts_with("ws://"))
        {
            return Ok(url.to_string());
        }
        if said.len() < 4096 {
            said.push_str(&line);
        }
    }
}

/// Whether a line of Node's stderr is its inspector speaking, not the
/// program.
fn is_inspector_chatter(line: &str) -> bool {
    line.starts_with("Debugger listening on ws://")
        || line.starts_with("Debugger ending on ws://")
        || line.starts_with("For help, see: https://nodejs.org/")
        || line == "Debugger attached."
        || line == "Waiting for the debugger to disconnect..."
}

/// Send the program's stderr as output, line by line, without what Node's
/// inspector adds to it.
async fn forward_stderr<R>(mut stderr: BufReader<R>, out: Out)
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        line.clear();
        match stderr.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line).into_owned();
        if !is_inspector_chatter(text.trim_end()) {
            out.output("stderr", text).await;
        }
    }
}

/// A regular expression for the URLs Node may give the script at `path`:
/// the path itself or its `file://` URL, any character percent-encoded or
/// not, and the same for the path with its links resolved (Node runs a
/// program by its real path).
fn url_regex(path: &str) -> String {
    let mut paths = vec![path.to_string()];
    if let Ok(real) = std::fs::canonicalize(path) {
        let real = real.to_string_lossy().into_owned();
        if real != path {
            paths.push(real);
        }
    }
    let patterns: Vec<String> = paths.iter().map(|p| path_pattern(p)).collect();
    format!("^(?:file://)?(?:{})$", patterns.join("|"))
}

fn path_pattern(path: &str) -> String {
    let mut out = String::new();
    for c in path.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_') {
            out.push(c);
            continue;
        }
        if c == '.' {
            out.push_str("\\.");
            continue;
        }
        let literal = match c.is_ascii() {
            true => format!("\\{c}"),
            false => c.to_string(),
        };
        let mut buf = [0u8; 4];
        let encoded: String = c
            .encode_utf8(&mut buf)
            .bytes()
            .map(|b| {
                let hex: String = format!("{b:02X}")
                    .chars()
                    .map(|h| match h.is_ascii_alphabetic() {
                        true => format!("[{h}{}]", h.to_ascii_lowercase()),
                        false => h.to_string(),
                    })
                    .collect();
                format!("%{hex}")
            })
            .collect();
        out.push_str(&format!("(?:{literal}|{encoded})"));
    }
    out
}

/// A `file://` URL as the path it names.
fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let raw = rest.as_bytes();
    let mut bytes = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(byte) = rest
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            bytes.push(byte);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapout::testing::Client;

    /// A server's side of a WebSocket, for the client's frames to be read
    /// and its own written.
    async fn server_frame(
        to: &mut (impl AsyncWrite + Unpin),
        fin: bool,
        opcode: u8,
        payload: &[u8],
    ) {
        let mut frame = vec![(u8::from(fin) << 7) | opcode];
        match payload.len() {
            n if n < 126 => frame.push(n as u8),
            n => {
                frame.push(126);
                frame.extend_from_slice(&(n as u16).to_be_bytes());
            }
        }
        frame.extend_from_slice(payload);
        to.write_all(&frame).await.unwrap();
    }

    #[tokio::test]
    async fn websocket_frames_are_read_and_written() {
        let (client_side, mut server_side) = tokio::io::duplex(1 << 20);
        let (client_read, client_write) = tokio::io::split(client_side);
        let write = Mutex::new(WsWrite {
            stream: Box::new(client_write),
            mask: 7,
        });
        let mut client_read = client_read;
        // Fragments joined, a ping between them answered, a long one whole.
        server_frame(&mut server_side, false, 0x1, b"hel").await;
        server_frame(&mut server_side, true, 0x9, b"p").await;
        server_frame(&mut server_side, true, 0x0, b"lo").await;
        let long = vec![b'x'; 300];
        server_frame(&mut server_side, true, 0x1, &long).await;
        assert_eq!(
            ws_message(&mut client_read, &write).await.as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(ws_message(&mut client_read, &write).await, Some(long));
        // The pong, masked as a client's frame must be.
        let mut head = [0u8; 2];
        server_side.read_exact(&mut head).await.unwrap();
        assert_eq!(head, [0x8A, 0x81]);
        let mut rest = [0u8; 5];
        server_side.read_exact(&mut rest).await.unwrap();
        assert_eq!(rest[4] ^ rest[0], b'p');
        // A message of ours, masked.
        write.lock().await.frame(0x1, &[b'a'; 200]).await.unwrap();
        let mut head = [0u8; 4];
        server_side.read_exact(&mut head).await.unwrap();
        assert_eq!(head, [0x81, 0xFE, 0, 200]);
        let mut key = [0u8; 4];
        server_side.read_exact(&mut key).await.unwrap();
        let mut body = vec![0u8; 200];
        server_side.read_exact(&mut body).await.unwrap();
        assert!(body.iter().enumerate().all(|(i, b)| b ^ key[i % 4] == b'a'));
        // A close ends it.
        server_frame(&mut server_side, true, 0x8, b"").await;
        assert_eq!(ws_message(&mut client_read, &write).await, None);
    }

    #[test]
    fn values_are_written_as_a_debugger_writes_them() {
        assert_eq!(describe(&json!({ "type": "undefined" })), "undefined");
        assert_eq!(
            describe(&json!({ "type": "string", "value": "a\"b" })),
            "\"a\\\"b\""
        );
        assert_eq!(
            describe(&json!({ "type": "number", "value": 3, "description": "3" })),
            "3"
        );
        assert_eq!(
            describe(
                &json!({ "type": "number", "unserializableValue": "NaN", "description": "NaN" })
            ),
            "NaN"
        );
        assert_eq!(
            describe(&json!({ "type": "boolean", "value": true })),
            "true"
        );
        assert_eq!(
            describe(&json!({ "type": "object", "subtype": "null", "value": null })),
            "null"
        );
        assert_eq!(
            describe(
                &json!({ "type": "function", "description": "function total(cart) {\n  return 1;\n}" })
            ),
            "ƒ function total(cart)"
        );
        let cart = json!({
            "type": "object", "className": "Cart", "description": "Cart", "objectId": "1",
            "preview": { "type": "object", "description": "Cart", "overflow": false, "properties": [
                { "name": "owner", "type": "string", "value": "ann" },
                { "name": "items", "type": "object", "value": "Array(2)", "subtype": "array" },
            ]},
        });
        assert_eq!(describe(&cart), "Cart {owner: 'ann', items: Array(2)}");
        let array = json!({
            "type": "object", "subtype": "array", "description": "Array(2)", "objectId": "2",
            "preview": { "type": "object", "subtype": "array", "description": "Array(2)", "overflow": true, "properties": [
                { "name": "0", "type": "number", "value": "2" },
            ]},
        });
        assert_eq!(describe(&array), "Array(2) [2, …]");
        let plain = json!({
            "type": "object", "description": "Object", "objectId": "3",
            "preview": { "type": "object", "description": "Object", "overflow": false, "properties": [
                { "name": "a", "type": "undefined", "value": "undefined" },
            ]},
        });
        assert_eq!(describe(&plain), "{a: undefined}");
        let map = json!({
            "type": "object", "subtype": "map", "description": "Map(1)", "objectId": "4",
            "preview": { "type": "object", "subtype": "map", "description": "Map(1)", "overflow": false, "properties": [], "entries": [
                { "key": { "type": "string", "description": "k" }, "value": { "type": "number", "description": "1" } },
            ]},
        });
        assert_eq!(describe(&map), "Map(1) {'k' => 1}");
        let error = json!({
            "type": "object", "subtype": "error", "description": "Error: no\n    at x (/a.js:1:1)",
        });
        assert_eq!(describe(&error), "Error: no");

        let mut state = State::default();
        assert_eq!(shown(&mut state, "c", &cart)["variablesReference"], 1);
        assert_eq!(state.refs[0].0, "1");
        assert_eq!(
            shown(
                &mut state,
                "n",
                &json!({ "type": "object", "subtype": "null", "objectId": "9" })
            )["variablesReference"],
            0
        );
        assert_eq!(
            exception_text(
                &json!({ "text": "Uncaught", "exception": { "description": "ReferenceError: x is not defined\n    at <anonymous>" } })
            ),
            "ReferenceError: x is not defined"
        );
        assert_eq!(exception_text(&json!({ "text": "Uncaught" })), "Uncaught");
    }

    #[test]
    fn frames_name_their_files_and_node_s_own_code() {
        let none = HashMap::new();
        let frame = json!({
            "callFrameId": "f", "functionName": "total",
            "location": { "scriptId": "1", "lineNumber": 9, "columnNumber": 2 },
            "url": "file:///home/a%20b/app.js",
        });
        assert_eq!(
            dap_frame(1, &frame, &none),
            json!({
                "id": 1, "name": "total", "line": 10, "column": 3,
                "source": { "name": "app.js", "path": "/home/a b/app.js" },
            })
        );
        let internal = json!({
            "functionName": "", "location": { "lineNumber": 0, "columnNumber": 0 },
            "url": "node:internal/main/run_main_module",
        });
        let shown = dap_frame(2, &internal, &none);
        assert_eq!(shown["name"], "(anonymous)");
        assert_eq!(
            shown["source"]["name"],
            "node:internal/main/run_main_module"
        );
        assert!(shown["source"].get("path").is_none());
        assert_eq!(shown["presentationHint"], "subtle");
        assert_eq!(
            dap_frame(3, &json!({ "url": "" }), &none)["source"]["name"],
            "<eval>"
        );
        // V8 names a frame's script by id only: its URL is the script's.
        let scripts = HashMap::from([("7".to_string(), "file:///p/a.js".to_string())]);
        let by_id = json!({ "url": "", "location": { "scriptId": "7", "lineNumber": 0 } });
        assert_eq!(dap_frame(4, &by_id, &scripts)["source"]["path"], "/p/a.js");
        assert_eq!(scope_name("local"), "Local");
        assert_eq!(uri_to_path("file:///x/%E4%B8%AD.js"), "/x/中.js");
        assert_eq!(uri_to_path("file:///x/100%"), "/x/100%");
    }

    #[test]
    fn breakpoint_urls_match_however_node_encodes_them() {
        assert_eq!(
            url_regex("/p/my app/a.js"),
            "^(?:file://)?(?:/p/my(?:\\ |%20)app/a\\.js)$"
        );
        assert_eq!(path_pattern("/中"), "/(?:中|%[Ee]4%[Bb]8%[Aa][Dd])");
        assert_eq!(path_pattern("a@b"), "a(?:\\@|%40)b");
    }

    #[test]
    fn typescript_runs_as_the_host_s_node_runs_it() {
        assert!(
            is_typescript("/p/app.ts") && is_typescript("/p/APP.MTS") && is_typescript("x.cts")
        );
        assert!(!is_typescript("/p/app.tsx") && !is_typescript("/p/app.js"));
        assert_eq!(strip_types_flags((22, 22)), Ok(vec![]));
        assert_eq!(strip_types_flags((22, 18)), Ok(vec![]));
        assert_eq!(
            strip_types_flags((22, 17)),
            Ok(vec!["--experimental-strip-types"])
        );
        assert_eq!(
            strip_types_flags((22, 6)),
            Ok(vec!["--experimental-strip-types"])
        );
        assert_eq!(
            strip_types_flags((23, 5)),
            Ok(vec!["--experimental-strip-types"])
        );
        assert_eq!(strip_types_flags((23, 6)), Ok(vec![]));
        assert_eq!(strip_types_flags((25, 0)), Ok(vec![]));
        let err = strip_types_flags((22, 5)).unwrap_err();
        assert!(err.contains("22.6") && err.contains("22.5"), "{err}");
        assert!(strip_types_flags((20, 11)).is_err());
    }

    #[tokio::test]
    async fn the_inspector_address_is_read_and_its_chatter_dropped() {
        let text = "Debugger listening on ws://127.0.0.1:9229/abc-def\nFor help, see: https://nodejs.org/en/docs/inspector\n";
        let mut reader = BufReader::new(text.as_bytes());
        assert_eq!(
            inspector_url(&mut reader).await,
            Ok("ws://127.0.0.1:9229/abc-def".to_string())
        );
        let mut failed =
            BufReader::new(&b"Starting inspector on 127.0.0.1:0 failed: address in use\n"[..]);
        let err = inspector_url(&mut failed).await.unwrap_err();
        assert!(err.contains("address in use"), "{err}");
        assert!(is_inspector_chatter("Debugger attached."));
        assert!(is_inspector_chatter(
            "Waiting for the debugger to disconnect..."
        ));
        assert!(is_inspector_chatter(
            "For help, see: https://nodejs.org/en/docs/inspector"
        ));
        assert!(!is_inspector_chatter(
            "Debugger attached. (the program's own words)"
        ));
    }

    fn node() -> PathBuf {
        clew_core::lsp::store::find_on_path_unresolved("node").expect("node")
    }

    /// The adapter driven to the program's end: `stopped` reasons and lines
    /// read as they come.
    async fn start(
        program: &std::path::Path,
        args: Value,
    ) -> (Client, tokio::task::JoinHandle<Result<(), String>>) {
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(node(), input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let mut launch = json!({
            "program": program.to_string_lossy(),
            "cwd": program.parent().unwrap().to_string_lossy(),
        });
        if let (Some(launch), Some(extra)) = (launch.as_object_mut(), args.as_object()) {
            launch.extend(extra.clone());
        }
        let seq = client.send("launch", launch).await;
        let launched = client.answer(seq).await;
        assert_eq!(launched["success"], true, "{launched}");
        client.event("initialized").await;
        (client, running)
    }

    async fn top(client: &mut Client) -> Value {
        let stack = client
            .request("stackTrace", json!({ "threadId": THREAD }))
            .await;
        stack["stackFrames"][0].clone()
    }

    async fn evaluate(
        client: &mut Client,
        frame: &Value,
        expression: &str,
        context: &str,
    ) -> Value {
        let seq = client
            .send(
                "evaluate",
                json!({ "expression": expression, "frameId": frame["id"], "context": context }),
            )
            .await;
        client.answer(seq).await
    }

    /// A Node program debugged for real, over the inspector: from a
    /// directory whose name Node percent-encodes, breakpoints confirmed as
    /// the script loads, conditions kept, the stack with each frame at its
    /// line, locals and `this`, an object opened, expressions evaluated (a
    /// hover's refused a side effect), stepped over and out, and run to its
    /// end with its output. Needs `node`.
    #[tokio::test]
    #[ignore = "needs node; run explicitly"]
    async fn a_node_program_is_debugged_over_the_inspector() {
        let dir = clew_core::testutil::TempDir::new("node-dbg-project");
        std::fs::create_dir_all(dir.join("my app")).unwrap();
        let program = dir.join("my app/main.js");
        std::fs::write(
            &program,
            "// a cart\nclass Cart {\n  constructor(owner) {\n    this.owner = owner;\n    this.items = [2, 3];\n  }\n}\n\nfunction total(cart) {\n  let sum = 0;\n  for (const i of cart.items) {\n    sum += i;\n  }\n  return sum * 2;\n}\n\nconst cart = new Cart(\"ann\");\nconst t = total(cart);\nconsole.log(\"total \" + t + \" \" + process.argv.slice(2).length);\nconsole.error(\"done\");\n",
        )
        .unwrap();
        let (mut client, running) = start(&program, json!({ "args": ["x"] })).await;
        let source = json!({ "path": program.to_string_lossy() });
        let set = client
            .request(
                "setBreakpoints",
                json!({ "source": source, "breakpoints": [
                    { "line": 10 }, { "line": 12, "condition": "i === 3" }, { "line": 19, "condition": "t === 11" },
                ]}),
            )
            .await;
        assert_eq!(set["breakpoints"].as_array().unwrap().len(), 3, "{set}");
        client.request("configurationDone", json!({})).await;

        let stopped = client.event("stopped").await;
        assert_eq!(
            stopped["body"]["reason"], "breakpoint",
            "the entry is passed: {stopped}"
        );
        let changed = client.event("breakpoint").await;
        assert_eq!(changed["body"]["breakpoint"]["verified"], true);
        let threads = client.request("threads", json!({})).await;
        assert_eq!(threads["threads"][0]["id"], THREAD);
        let stack = client
            .request("stackTrace", json!({ "threadId": THREAD }))
            .await;
        let frames = stack["stackFrames"].as_array().unwrap();
        assert_eq!(frames[0]["name"], "total");
        assert_eq!(frames[0]["line"], 10);
        assert_eq!(
            frames[0]["source"]["path"],
            program.to_string_lossy().as_ref()
        );
        assert_eq!(frames[1]["line"], 18);
        let frame = frames[0].clone();
        let scopes = client
            .request("scopes", json!({ "frameId": frame["id"] }))
            .await;
        assert_eq!(scopes["scopes"][0]["name"], "Local");
        let locals = client
            .request(
                "variables",
                json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
            )
            .await;
        let cart = locals["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "cart")
            .cloned()
            .unwrap_or_else(|| panic!("cart in {locals}"));
        assert_eq!(cart["value"], "Cart {owner: 'ann', items: Array(2)}");
        let fields = client
            .request(
                "variables",
                json!({ "variablesReference": cart["variablesReference"] }),
            )
            .await;
        assert!(
            fields["variables"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["name"] == "owner" && v["value"] == "\"ann\""),
            "{fields}"
        );
        for (expression, expected) in [
            ("cart.items[0] + cart.items[1]", "5"),
            ("cart.owner.toUpperCase()", "\"ANN\""),
            ("cart.items.map(x => x * 10)", "Array(2) [20, 30]"),
            ("typeof cart", "\"object\""),
        ] {
            let answer = evaluate(&mut client, &frame, expression, "repl").await;
            assert_eq!(answer["body"]["result"], expected, "{expression}: {answer}");
        }
        let hovered = evaluate(&mut client, &frame, "cart.owner", "hover").await;
        assert_eq!(hovered["body"]["result"], "\"ann\"");
        let refused = evaluate(&mut client, &frame, "cart.items.push(4)", "hover").await;
        assert_eq!(
            refused["success"], false,
            "a hover changes nothing: {refused}"
        );
        let unchanged = evaluate(&mut client, &frame, "cart.items.length", "repl").await;
        assert_eq!(unchanged["body"]["result"], "2");
        let thrown = evaluate(&mut client, &frame, "nothing.x", "repl").await;
        assert_eq!(thrown["success"], false);
        assert!(
            thrown["message"]
                .as_str()
                .unwrap()
                .contains("ReferenceError"),
            "{thrown}"
        );

        client.request("next", json!({ "threadId": THREAD })).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "step");
        assert_eq!(top(&mut client).await["line"], 11);
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        assert_eq!(
            client.event("stopped").await["body"]["reason"],
            "breakpoint"
        );
        let at = top(&mut client).await;
        assert_eq!(at["line"], 12);
        let i = evaluate(&mut client, &at, "i", "watch").await;
        assert_eq!(
            i["body"]["result"], "3",
            "the condition held it until i was 3"
        );
        client
            .request("stepOut", json!({ "threadId": THREAD }))
            .await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "step");
        // Back in the caller, at its next statement (where V8 stops after
        // the call's assignment).
        let caller = top(&mut client).await;
        assert_eq!(
            (caller["name"].as_str(), caller["line"].as_u64()),
            (Some("(anonymous)"), Some(19))
        );

        // The line-19 condition never holds: the program runs to its end.
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        let exited = client.event("exited").await;
        assert_eq!(exited["body"]["exitCode"], 0);
        client.event("terminated").await;
        assert!(
            client.output().contains("total 10 1"),
            "{}",
            client.output()
        );
        let stderr: String = client
            .seen
            .iter()
            .filter(|m| m["event"] == "output" && m["body"]["category"] == "stderr")
            .filter_map(|m| m["body"]["output"].as_str())
            .collect();
        assert_eq!(
            stderr, "done\n",
            "the program's own, none of the inspector's"
        );
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }

    /// A TypeScript program, its types stripped by Node: held at its line,
    /// its typed variable read. Needs node 22.6 or later.
    #[tokio::test]
    #[ignore = "needs node; run explicitly"]
    async fn a_typescript_program_is_debugged_with_its_types_stripped() {
        let dir = clew_core::testutil::TempDir::new("node-ts-project");
        let program = dir.join("app.ts");
        std::fs::write(
            &program,
            "interface Cart {\n  owner: string;\n  items: number[];\n}\n\nconst cart: Cart = { owner: \"ann\", items: [2, 3] };\nconst sum: number = cart.items.reduce((a: number, b: number) => a + b, 0);\nconsole.log(`sum ${sum}`);\n",
        )
        .unwrap();
        let (mut client, running) = start(&program, json!({})).await;
        client
            .request(
                "setBreakpoints",
                json!({ "source": { "path": program.to_string_lossy() }, "breakpoints": [{ "line": 8 }] }),
            )
            .await;
        client.request("configurationDone", json!({})).await;
        assert_eq!(
            client.event("stopped").await["body"]["reason"],
            "breakpoint"
        );
        let at = top(&mut client).await;
        assert_eq!(at["line"], 8);
        assert_eq!(at["source"]["path"], program.to_string_lossy().as_ref());
        let sum = evaluate(&mut client, &at, "sum + cart.items.length", "repl").await;
        assert_eq!(sum["body"]["result"], "7", "{sum}");
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        assert_eq!(client.event("exited").await["body"]["exitCode"], 0);
        client.event("terminated").await;
        assert!(client.output().contains("sum 5"), "{}", client.output());
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }

    /// A running Node program paused where it is, read, and let go; an
    /// uncaught exception stopped at; a launch held at its entry.
    #[tokio::test]
    #[ignore = "needs node; run explicitly"]
    async fn a_running_node_program_is_paused_and_its_exceptions_caught() {
        let dir = clew_core::testutil::TempDir::new("node-pause-project");
        let program = dir.join("spin.js");
        std::fs::write(
            &program,
            "let n = 0;\nconst timer = setInterval(() => {\n  n++;\n  if (n > 150) {\n    clearInterval(timer);\n    throw new Error(\"too many\");\n  }\n}, 10);\n",
        )
        .unwrap();
        let (mut client, running) = start(&program, json!({ "stopOnEntry": true })).await;
        client
            .request(
                "setExceptionBreakpoints",
                json!({ "filters": ["uncaught"] }),
            )
            .await;
        client.request("configurationDone", json!({})).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "entry");
        assert_eq!(top(&mut client).await["line"], 1);
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.request("pause", json!({ "threadId": THREAD })).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "pause");
        let at = top(&mut client).await;
        let n = evaluate(&mut client, &at, "n > 0 && n <= 150", "repl").await;
        assert_eq!(n["body"]["result"], "true", "{n}");
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        let thrown = client.event("stopped").await;
        assert_eq!(thrown["body"]["reason"], "exception");
        assert_eq!(thrown["body"]["text"], "Error: too many");
        assert_eq!(top(&mut client).await["line"], 6);
        client
            .request("continue", json!({ "threadId": THREAD }))
            .await;
        let exited = client.event("exited").await;
        assert_eq!(exited["body"]["exitCode"], 1, "the error ended it");
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }
}
