//! `clew-server erlang-debug --erl <erl> --erlc <erlc>`: a debug adapter for
//! Erlang, over OTP's own interpreter.
//!
//! OTP debugs code by interpreting it: `int` runs a module compiled with
//! `debug_info` statement by statement, holds a process at a breakpoint, and
//! reads its stack and bindings through the process's "meta" process. No
//! adapter speaks DAP for it, so this one does: it compiles the program's
//! application (`debug_info` on, into a directory of its own), starts a node
//! that runs [`BRIDGE`] — an Erlang module that drives `int` and talks to
//! this adapter over a loopback socket — and maps DAP onto it.
//!
//! The program is `Module:Function(Args)`: the `.erl` file launched names the
//! module, `function` its entry (`main` unless said; `start/0` names one that
//! takes no arguments), and the launch's arguments are its `Args`, as an
//! escript's `main/1` takes them. Each Erlang process that reaches a
//! breakpoint becomes a thread; a frame's bindings are its scope; lists,
//! tuples and maps open into their elements. What the node prints is the
//! program's output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use clew_core::framing::read_message;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot, watch};

use crate::dapout::{Out, forward};

/// The Erlang half: run in the debugged node, it drives `int` for the
/// adapter (see its own comments).
const BRIDGE: &str = include_str!("debug_erlang/clew_dap.erl");

/// How long compiling the program's application may take.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the node has to start and connect back.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the bridge has to answer a command.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the node has to exit once its session is over.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Most source files compiled for one session.
const MAX_SOURCES: usize = 2000;

/// Frame ids carry their thread: `thread * FRAMES + level`.
const FRAMES: u64 = 100_000;

/// Run the adapter on this process's stdin/stdout.
pub async fn run(erl: PathBuf, erlc: PathBuf) -> Result<(), String> {
    serve(erl, erlc, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(erl: PathBuf, erlc: PathBuf, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let mut adapter = Adapter {
        erl,
        erlc,
        out: Out::new(output),
        bridge: None,
        node: None,
        scratch: None,
        modules: HashMap::new(),
        entry: None,
        refs: Arc::new(Mutex::new(Vec::new())),
        stepping: Arc::new(AtomicBool::new(false)),
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

/// What a `variablesReference` stands for: a frame's bindings, or the
/// elements of a value the bridge holds (`ref`, until the process moves).
#[derive(Debug, Clone, Copy)]
enum VarRef {
    Bindings { thread: u64, level: u64 },
    Parts { reference: u64 },
}

/// The node: its exit code once it has one, and how to stop it.
struct Node {
    exited: watch::Receiver<Option<i32>>,
    kill: Option<oneshot::Sender<()>>,
}

/// The program `configurationDone` starts: `Module:Function(Args)`.
struct Entry {
    module: String,
    function: String,
    /// `None` for an entry that takes no arguments.
    args: Option<Vec<String>>,
}

struct Adapter {
    erl: PathBuf,
    erlc: PathBuf,
    out: Out,
    bridge: Option<Arc<Bridge>>,
    node: Option<Node>,
    /// Where the program's modules and the bridge were compiled to.
    scratch: Option<PathBuf>,
    /// Each module compiled for the session, with its source file.
    modules: HashMap<String, PathBuf>,
    entry: Option<Entry>,
    /// The references handed out since a process last stopped.
    refs: Arc<Mutex<Vec<VarRef>>>,
    /// The last resume was a step: what stops it is a step's end.
    stepping: Arc<AtomicBool>,
}

impl Adapter {
    /// Handle one request; `false` once the session is over.
    async fn handle(&mut self, request: &Value) -> bool {
        let command = request["command"].as_str().unwrap_or("");
        let args = &request["arguments"];
        let answer = match command {
            "initialize" => Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsEvaluateForHovers": true,
                "supportsTerminateRequest": true,
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
            "configurationDone" => self.start_program().await,
            "threads" => self.threads().await,
            "continue" | "next" | "stepIn" | "stepOut" => {
                let how = match command {
                    "continue" => "continue",
                    "next" => "next",
                    "stepIn" => "step",
                    _ => "finish",
                };
                self.resume(args, how).await
            }
            "pause" => self.pause().await,
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

    /// Compile the program's application with `debug_info`, start a node
    /// running the bridge, and have it interpret those modules.
    async fn launch(&mut self, args: &Value) -> Result<(), String> {
        let program = args["program"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .ok_or("no program to run")?;
        if program.extension().is_none_or(|e| e != "erl") {
            return Err(format!(
                "{} is not an Erlang source file (.erl)",
                program.display()
            ));
        }
        let cwd = args["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| program.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let module = args["module"]
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                program
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            })
            .ok_or("no module to run")?;
        let function = args["function"].as_str().unwrap_or("main");
        let (function, takes_args) = match function.split_once('/') {
            Some((name, "0")) => (name.to_string(), false),
            Some((name, _)) => (name.to_string(), true),
            None => (function.to_string(), true),
        };
        let program_args: Vec<String> = args["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.entry = Some(Entry {
            module: module.clone(),
            function,
            args: takes_args.then_some(program_args),
        });

        let scratch = scratch_dir()?;
        self.scratch = Some(scratch.clone());
        let (sources, includes) = sources_of(&program, &cwd);
        let compiled = self.compile(&sources, &includes, &scratch).await?;
        if !compiled.contains_key(&module) {
            return Err(format!("{module} did not compile — see the output"));
        }
        self.modules = compiled;
        let bridge_dir = scratch.join("bridge");
        std::fs::create_dir_all(&bridge_dir).map_err(|e| e.to_string())?;
        std::fs::write(bridge_dir.join("clew_dap.erl"), BRIDGE).map_err(|e| e.to_string())?;
        self.compile(&[bridge_dir.join("clew_dap.erl")], &[], &bridge_dir)
            .await?;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("cannot listen for the node: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("cannot listen for the node: {e}"))?
            .port();
        let mut command = clew_core::spawn::tokio_command(&self.erl);
        command
            .arg("-noshell")
            .arg("-pa")
            .arg(&scratch)
            .arg("-pa")
            .arg(&bridge_dir);
        for ebin in dependency_ebins(&cwd) {
            command.arg("-pa").arg(ebin);
        }
        command
            .args(["-s", "clew_dap", "start", &port.to_string()])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", self.erl.display()))?;
        for (stream, category) in [
            (
                child
                    .stdout
                    .take()
                    .map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
                "stdout",
            ),
            (
                child
                    .stderr
                    .take()
                    .map(|s| Box::new(s) as Box<dyn AsyncRead + Unpin + Send>),
                "stderr",
            ),
        ] {
            if let Some(stream) = stream {
                tokio::spawn(forward(stream, category, self.out.clone()));
            }
        }
        let (exit_tx, mut exited) = watch::channel(None);
        let (kill, killed) = oneshot::channel::<()>();
        let out = self.out.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status.ok(),
                _ = killed => {
                    let _ = child.kill().await;
                    child.wait().await.ok()
                }
            };
            let code = status.and_then(|s| s.code()).unwrap_or(-1);
            let _ = exit_tx.send(Some(code));
            out.terminated(Some(code)).await;
        });
        self.node = Some(Node {
            exited: exited.clone(),
            kill: Some(kill),
        });
        let stream = tokio::select! {
            accepted = tokio::time::timeout(CONNECT_TIMEOUT, listener.accept()) => match accepted {
                Ok(Ok((stream, _))) => stream,
                Ok(Err(e)) => return Err(format!("the node did not connect: {e}")),
                Err(_) => return Err(format!("the node did not connect within {} s", CONNECT_TIMEOUT.as_secs())),
            },
            _ = exited.wait_for(Option::is_some) => {
                return Err("the node ended before it connected — see its output".into());
            }
        };
        let bridge = Bridge::start(
            stream,
            self.out.clone(),
            self.refs.clone(),
            self.stepping.clone(),
        );
        let mut modules: Vec<&String> = self.modules.keys().collect();
        modules.sort();
        let list = modules
            .iter()
            .map(|m| atom(m))
            .collect::<Vec<_>>()
            .join(", ");
        let interpreted = bridge
            .ask_named("interpret", format!("{{interpret, [{list}]}}."))
            .await?;
        let failed: Vec<String> = interpreted["modules"]
            .as_array()
            .map(|ms| {
                ms.iter()
                    .filter(|m| m["ok"] != true)
                    .filter_map(|m| m["module"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if !failed.is_empty() {
            self.out
                .output(
                    "console",
                    format!(
                        "Not interpreted (no breakpoints there): {}\n",
                        failed.join(", ")
                    ),
                )
                .await;
        }
        self.bridge = Some(bridge);
        Ok(())
    }

    /// `erlc +debug_info` the `sources` into `into`: the modules compiled,
    /// with their sources. What the compiler says goes to the output.
    async fn compile(
        &self,
        sources: &[PathBuf],
        includes: &[PathBuf],
        into: &Path,
    ) -> Result<HashMap<String, PathBuf>, String> {
        if sources.is_empty() {
            return Ok(HashMap::new());
        }
        let mut command = clew_core::spawn::tokio_command(&self.erlc);
        command.arg("+debug_info").arg("-o").arg(into);
        for include in includes {
            command.arg("-I").arg(include);
        }
        command
            .args(sources)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(COMPILE_TIMEOUT, command.output())
            .await
            .map_err(|_| "compiling took too long".to_string())?
            .map_err(|e| format!("cannot run {}: {e}", self.erlc.display()))?;
        let said = String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr);
        if !said.trim().is_empty() {
            self.out.output("console", said).await;
        }
        Ok(sources
            .iter()
            .filter_map(|source| {
                let module = source.file_stem()?.to_string_lossy().into_owned();
                into.join(format!("{module}.beam"))
                    .is_file()
                    .then(|| (module, source.clone()))
            })
            .collect())
    }

    fn bridge(&self) -> Result<Arc<Bridge>, String> {
        self.bridge
            .clone()
            .ok_or_else(|| "no program is running".to_string())
    }

    /// Replace one file's breakpoints.
    async fn set_breakpoints(&self, args: &Value) -> Result<Value, String> {
        let path = PathBuf::from(
            args["source"]["path"]
                .as_str()
                .ok_or("a breakpoint needs its file's path")?,
        );
        let lines: Vec<u64> = args["breakpoints"]
            .as_array()
            .map(|bps| bps.iter().filter_map(|bp| bp["line"].as_u64()).collect())
            .unwrap_or_default();
        let module = self
            .modules
            .iter()
            .find(|(_, source)| same_file(source, &path))
            .map(|(module, _)| module.clone());
        let (Some(module), Ok(bridge)) = (module, self.bridge()) else {
            let unverified: Vec<Value> = lines
                .iter()
                .map(|line| json!({ "verified": false, "line": line, "message": "not one of the program's modules" }))
                .collect();
            return Ok(json!({ "breakpoints": unverified }));
        };
        let list = lines
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let set = bridge
            .ask(|seq| format!("{{break, {seq}, {}, [{list}]}}.", atom(&module)))
            .await?;
        let answers: Vec<Value> = lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let ok = set["body"][i]["ok"] == true;
                json!({
                    "verified": ok,
                    "line": line,
                    "message": if ok { Value::Null } else { json!("no executable code on this line") },
                })
            })
            .collect();
        Ok(json!({ "breakpoints": answers }))
    }

    async fn start_program(&self) -> Result<Value, String> {
        let bridge = self.bridge()?;
        let entry = self.entry.as_ref().ok_or("no program to run")?;
        let args = match &entry.args {
            Some(args) => format!(
                "[{}]",
                args.iter()
                    .map(|a| string(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "none".into(),
        };
        bridge
            .send(&format!(
                "{{run, {}, {}, {args}}}.",
                atom(&entry.module),
                atom(&entry.function)
            ))
            .await?;
        Ok(json!({}))
    }

    async fn threads(&self) -> Result<Value, String> {
        let Ok(bridge) = self.bridge() else {
            return Ok(json!({ "threads": [] }));
        };
        let threads = bridge.ask(|seq| format!("{{threads, {seq}}}.")).await?;
        let threads: Vec<Value> = threads["body"]
            .as_array()
            .map(|ts| {
                ts.iter()
                    .map(|t| json!({ "id": t["id"], "name": format!("process {}", t["name"].as_str().unwrap_or("?")) }))
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "threads": threads }))
    }

    async fn resume(&self, args: &Value, how: &str) -> Result<Value, String> {
        let bridge = self.bridge()?;
        let thread = args["threadId"].as_u64().ok_or("which process?")?;
        self.stepping.store(how != "continue", Ordering::SeqCst);
        bridge
            .send(&format!("{{resume, {thread}, {how}}}."))
            .await?;
        Ok(json!({ "allThreadsContinued": false }))
    }

    /// Stop the program wherever it is next in interpreted code (see the
    /// bridge's `pause`).
    async fn pause(&self) -> Result<Value, String> {
        let bridge = self.bridge()?;
        self.stepping.store(false, Ordering::SeqCst);
        bridge.ask(|seq| format!("{{pause, {seq}}}.")).await?;
        Ok(json!({}))
    }

    async fn stack_trace(&self, args: &Value) -> Result<Value, String> {
        let bridge = self.bridge()?;
        let thread = args["threadId"].as_u64().ok_or("which process?")?;
        let stack = bridge
            .ask(|seq| format!("{{stack, {seq}, {thread}}}."))
            .await?;
        let frames: Vec<Value> = stack["body"]
            .as_array()
            .map(|frames| {
                frames
                    .iter()
                    .map(|f| {
                        let module = f["module"].as_str().unwrap_or("?");
                        let level = f["level"].as_u64().unwrap_or(0);
                        let mut frame = json!({
                            "id": thread * FRAMES + level,
                            "name": format!("{module}:{}/{}", f["function"].as_str().unwrap_or("?"), f["arity"]),
                            "line": f["line"],
                            "column": 1,
                        });
                        if let Some(source) = self.modules.get(module) {
                            frame["source"] = json!({
                                "path": source.to_string_lossy(),
                                "name": source.file_name().map(|n| n.to_string_lossy().into_owned()),
                            });
                        }
                        frame
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "totalFrames": frames.len(), "stackFrames": frames }))
    }

    async fn scopes(&self, args: &Value) -> Result<Value, String> {
        let (thread, level) = frame_of(args)?;
        let mut refs = self.refs.lock().await;
        refs.push(VarRef::Bindings { thread, level });
        Ok(
            json!({ "scopes": [{ "name": "Bindings", "variablesReference": refs.len(), "expensive": false }] }),
        )
    }

    async fn variables(&self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
        let found = {
            let refs = self.refs.lock().await;
            reference.checked_sub(1).and_then(|i| refs.get(i)).copied()
        };
        let Some(var) = found else {
            return Ok(json!({ "variables": [] }));
        };
        let bridge = self.bridge()?;
        let listed = match var {
            VarRef::Bindings { thread, level } => {
                bridge
                    .ask(|seq| format!("{{bindings, {seq}, {thread}, {level}}}."))
                    .await?
            }
            VarRef::Parts { reference } => {
                bridge
                    .ask(|seq| format!("{{children, {seq}, {reference}}}."))
                    .await?
            }
        };
        let mut refs = self.refs.lock().await;
        let variables: Vec<Value> = listed["body"]
            .as_array()
            .map(|vs| vs.iter().map(|v| shown(v, &mut refs)).collect())
            .unwrap_or_default();
        Ok(json!({ "variables": variables }))
    }

    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").trim().to_string();
        if expression.is_empty() {
            return Err("nothing to evaluate".into());
        }
        let (thread, level) =
            frame_of(args).map_err(|_| "evaluate in a stopped process's frame".to_string())?;
        let bridge = self.bridge()?;
        let value = bridge
            .ask(|seq| {
                format!(
                    "{{eval, {seq}, {thread}, {level}, {}}}.",
                    string(&expression)
                )
            })
            .await?;
        let mut refs = self.refs.lock().await;
        let shown = shown(&value["body"], &mut refs);
        Ok(json!({ "result": shown["value"], "variablesReference": shown["variablesReference"] }))
    }

    /// The session is over: stop the node, and clear what was compiled.
    async fn end(&mut self) {
        if let Some(node) = &mut self.node {
            if let Some(kill) = node.kill.take() {
                let _ = kill.send(());
            }
            let mut exited = node.exited.clone();
            let _ = tokio::time::timeout(EXIT_GRACE, exited.wait_for(Option::is_some)).await;
        }
        if let Some(scratch) = self.scratch.take() {
            let _ = std::fs::remove_dir_all(scratch);
        }
    }
}

/// A frame's thread and level, from its id ([`Adapter::stack_trace`]).
fn frame_of(args: &Value) -> Result<(u64, u64), String> {
    let id = args["frameId"].as_u64().ok_or("no such frame")?;
    Ok((id / FRAMES, id % FRAMES))
}

/// A value from the bridge as a DAP variable: one with parts given a
/// reference to list them by.
fn shown(value: &Value, refs: &mut Vec<VarRef>) -> Value {
    let reference = match value["ref"].as_u64() {
        Some(r) if r > 0 => {
            refs.push(VarRef::Parts { reference: r });
            refs.len()
        }
        _ => 0,
    };
    json!({
        "name": value["name"],
        "value": value["value"],
        "variablesReference": reference,
    })
}

/// An Erlang atom literal for `name`.
fn atom(name: &str) -> String {
    let mut quoted = String::from("'");
    for c in name.chars() {
        if matches!(c, '\'' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('\'');
    quoted
}

/// An Erlang string literal for `text`, on one line.
fn string(text: &str) -> String {
    let mut quoted = String::from("\"");
    for c in text.chars() {
        match c {
            '"' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Whether two paths name one file.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// A directory of the session's own for what it compiles: made new — never
/// one that was there, which is someone else's — and readable by its owner
/// alone.
fn scratch_dir() -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut last = String::new();
    for _ in 0..16 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "clew-erlang-scratch-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        // (Only Unix has a mode to set; on Windows the directory takes its
        // parent's per-user access list.)
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last = e.to_string(),
            Err(e) => return Err(format!("cannot make {}: {e}", dir.display())),
        }
    }
    Err(format!("cannot make a scratch directory: {last}"))
}

/// The program's application's sources and include directories: the
/// `.erl` files beside the program, in the project's `src/` and its
/// `apps/*/src/` (the rebar3 layouts); the `include/` directories beside
/// those.
fn sources_of(program: &Path, root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = program.parent() {
        dirs.push(dir.to_path_buf());
    }
    dirs.push(root.join("src"));
    if let Ok(apps) = std::fs::read_dir(root.join("apps")) {
        let mut apps: Vec<PathBuf> = apps.flatten().map(|e| e.path().join("src")).collect();
        apps.sort();
        dirs.extend(apps);
    }
    let mut sources = Vec::new();
    let mut includes = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for dir in dirs {
        let Ok(canonical) = std::fs::canonicalize(&dir) else {
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "erl") && p.is_file())
                .collect();
            found.sort();
            sources.extend(found);
        }
        includes.push(dir.clone());
        if let Some(app) = dir.parent() {
            let include = app.join("include");
            if include.is_dir() {
                includes.push(include);
            }
        }
    }
    sources.truncate(MAX_SOURCES);
    (sources, includes)
}

/// The compiled dependencies the program runs with: rebar3's
/// `_build/default/lib/*/ebin`, and `deps/*/ebin` and `ebin/` of older
/// layouts.
fn dependency_ebins(root: &Path) -> Vec<PathBuf> {
    let mut ebins = Vec::new();
    for parent in [root.join("_build/default/lib"), root.join("deps")] {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut found: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path().join("ebin"))
                .filter(|p| p.is_dir())
                .collect();
            found.sort();
            ebins.extend(found);
        }
    }
    let own = root.join("ebin");
    if own.is_dir() {
        ebins.push(own);
    }
    ebins
}

/// The connection to the bridge in the node: commands out as Erlang terms,
/// answers and events back as JSON lines.
struct Bridge {
    write: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    seq: AtomicU64,
}

impl Bridge {
    fn start(
        stream: tokio::net::TcpStream,
        out: Out,
        refs: Arc<Mutex<Vec<VarRef>>>,
        stepping: Arc<AtomicBool>,
    ) -> Arc<Bridge> {
        let (read, write) = stream.into_split();
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Arc::default();
        let bridge = Arc::new(Bridge {
            write: Mutex::new(write),
            pending: pending.clone(),
            seq: AtomicU64::new(0),
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(answer) = message.get("answer") {
                    let key = match answer {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    if let Some(waiting) = pending.lock().await.remove(&key) {
                        let _ = waiting.send(message);
                    }
                    continue;
                }
                match message["event"].as_str() {
                    Some("stopped") => {
                        refs.lock().await.clear();
                        let reason = match (
                            stepping.swap(false, Ordering::SeqCst),
                            message["reason"].as_str(),
                        ) {
                            (_, Some("pause")) => "pause",
                            (true, _) => "step",
                            (false, _) => "breakpoint",
                        };
                        out.event(
                            "stopped",
                            json!({ "reason": reason, "threadId": message["thread"], "allThreadsStopped": false }),
                        )
                        .await;
                    }
                    Some("exited") => {
                        let code = message["code"].as_i64().unwrap_or(0) as i32;
                        out.terminated(Some(code)).await;
                    }
                    Some("error") => {
                        out.output(
                            "console",
                            format!("[clew] {}\n", message["message"].as_str().unwrap_or("")),
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        });
        bridge
    }

    async fn send(&self, line: &str) -> Result<(), String> {
        let mut write = self.write.lock().await;
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(|e| format!("the node is gone: {e}"))
    }

    /// Send the command `build` makes of a fresh sequence number, and wait
    /// for its answer.
    async fn ask(&self, build: impl FnOnce(u64) -> String) -> Result<Value, String> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.ask_named(&seq.to_string(), build(seq)).await
    }

    /// Send `line`, and wait for the answer named `key`.
    async fn ask_named(&self, key: &str, line: String) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(key.to_string(), tx);
        self.send(&line).await?;
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err("the node is gone".into()),
            Err(_) => {
                self.pending.lock().await.remove(key);
                Err("the node did not answer in time".into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapout::testing::Client;

    /// Commands are Erlang terms: names quoted as atoms, text as strings on
    /// one line, whatever they hold; frames carry their process.
    #[test]
    fn commands_quote_what_they_carry() {
        assert_eq!(atom("shop"), "'shop'");
        assert_eq!(atom("it's"), r"'it\'s'");
        assert_eq!(string("say \"hi\"\nnow"), r#""say \"hi\"\nnow""#);
        assert_eq!(string(r"a\b"), r#""a\\b""#);
        assert_eq!(frame_of(&json!({ "frameId": 3 * FRAMES + 2 })), Ok((3, 2)));
        assert!(frame_of(&json!({})).is_err());
        let mut refs = Vec::new();
        let leaf = shown(&json!({ "name": "N", "value": "1", "ref": 0 }), &mut refs);
        assert_eq!(leaf["variablesReference"], 0);
        let list = shown(&json!({ "name": "L", "value": "[1]", "ref": 7 }), &mut refs);
        assert_eq!(list["variablesReference"], 1);
        assert!(matches!(refs[0], VarRef::Parts { reference: 7 }));
    }

    /// The program's application is compiled from rebar3's layouts — the
    /// file's own directory, `src/`, `apps/*/src/` — each once, with their
    /// `include/`; dependencies run from their `ebin/`. The scratch
    /// directory is new each time, never one that was there.
    #[test]
    fn the_application_is_found_where_rebar3_keeps_it() {
        let dir = clew_core::testutil::TempDir::new("erl-layout");
        for file in [
            "src/a.erl",
            "src/b.erl",
            "apps/web/src/w.erl",
            "src/notes.txt",
        ] {
            std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            std::fs::write(dir.join(file), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("include")).unwrap();
        std::fs::create_dir_all(dir.join("_build/default/lib/cowboy/ebin")).unwrap();
        std::fs::create_dir_all(dir.join("_build/default/lib/empty")).unwrap();
        let (sources, includes) = sources_of(&dir.join("src/a.erl"), &dir);
        let names: Vec<String> = sources
            .iter()
            .map(|s| {
                s.strip_prefix(&*dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, ["src/a.erl", "src/b.erl", "apps/web/src/w.erl"]);
        assert!(includes.contains(&dir.join("include")));
        assert_eq!(
            dependency_ebins(&dir),
            [dir.join("_build/default/lib/cowboy/ebin")]
        );
        let (one, two) = (scratch_dir().unwrap(), scratch_dir().unwrap());
        assert_ne!(one, two);
        assert!(one.is_dir() && two.is_dir());
        let _ = (std::fs::remove_dir_all(one), std::fs::remove_dir_all(two));
    }

    /// A running Erlang program is paused — by breakpoints on every line,
    /// taken off at the first stop — and runs on again when continued. Needs
    /// Erlang/OTP.
    #[tokio::test]
    #[ignore = "needs Erlang/OTP with its debugger application; run explicitly"]
    async fn a_running_erlang_program_is_paused() {
        let dir = clew_core::testutil::TempDir::new("erlang-pause");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let program = dir.join("src/spin.erl");
        std::fs::write(
            &program,
            "-module(spin).\n-export([main/1]).\n\nloop(N) ->\n    timer:sleep(10),\n    loop(N + 1).\n\nmain(_) ->\n    loop(0).\n",
        )
        .unwrap();
        let tool = |name| clew_core::lsp::store::find_on_path_unresolved(name).expect(name);
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(tool("erl"), tool("erlc"), input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "cwd": dir.to_string_lossy() }),
            )
            .await;
        assert_eq!(client.answer(launch).await["success"], true);
        client.event("initialized").await;
        client.request("configurationDone", json!({})).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        client.request("pause", json!({ "threadId": 1 })).await;
        let stopped = client.event("stopped").await;
        assert_eq!(stopped["body"]["reason"], "pause");
        let thread = stopped["body"]["threadId"].clone();
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        assert_eq!(stack["stackFrames"][0]["name"], "spin:loop/1", "{stack}");
        // On again — the pause's breakpoints gone, it does not stop.
        client
            .request("continue", json!({ "threadId": thread }))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        client.gather(Duration::from_millis(200)).await;
        assert!(
            !client.seen.iter().any(|m| m["event"] == "stopped"),
            "{:?}",
            client.seen
        );
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }

    /// An Erlang program debugged for real, under OTP's interpreter: held at
    /// a breakpoint, its stack (each frame at its line) and bindings read, a
    /// list opened, an expression evaluated, stepped, stopped again further
    /// on, and run to its end with its output. Needs `erl` and `erlc`, and
    /// OTP's `debugger` application.
    #[tokio::test]
    #[ignore = "needs Erlang/OTP with its debugger application; run explicitly"]
    async fn an_erlang_program_is_debugged_under_the_interpreter() {
        let dir = clew_core::testutil::TempDir::new("erlang-debug");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let program = dir.join("src/shop.erl");
        std::fs::write(
            &program,
            "-module(shop).\n-export([main/1, total/1]).\n\ntotal(Items) ->\n    Sum = lists:sum(Items),\n    Sum * 2.\n\nmain(Args) ->\n    T = total([2, 3]),\n    io:format(\"total ~p ~p~n\", [T, Args]),\n    ok.\n",
        )
        .unwrap();
        let tool = |name| clew_core::lsp::store::find_on_path_unresolved(name).expect(name);
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(tool("erl"), tool("erlc"), input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "args": ["x"], "cwd": dir.to_string_lossy() }),
            )
            .await;
        let launched = client.answer(launch).await;
        assert_eq!(launched["success"], true, "{launched}: {:?}", client.seen);
        client.event("initialized").await;
        let source = json!({ "path": program.to_string_lossy() });
        let set = client
            .request(
                "setBreakpoints",
                json!({ "source": source, "breakpoints": [{ "line": 5 }, { "line": 10 }] }),
            )
            .await;
        assert_eq!(set["breakpoints"][0]["verified"], true, "{set}");
        assert_eq!(set["breakpoints"][1]["verified"], true, "{set}");
        client.request("configurationDone", json!({})).await;

        let stopped = client.event("stopped").await;
        assert_eq!(stopped["body"]["reason"], "breakpoint");
        let thread = stopped["body"]["threadId"].clone();
        let threads = client.request("threads", json!({})).await;
        assert_eq!(threads["threads"][0]["id"], thread);
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        let frames = stack["stackFrames"].as_array().unwrap();
        assert_eq!(frames[0]["name"], "shop:total/1");
        assert_eq!(frames[0]["line"], 5);
        assert_eq!(
            frames[0]["source"]["path"],
            program.to_string_lossy().as_ref()
        );
        assert_eq!(frames[1]["name"], "shop:main/1");
        assert_eq!(frames[1]["line"], 9, "the caller's frame, where it called");
        let scopes = client
            .request("scopes", json!({ "frameId": frames[0]["id"] }))
            .await;
        let bindings = client
            .request(
                "variables",
                json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
            )
            .await;
        assert_eq!(bindings["variables"][0]["name"], "Items");
        assert_eq!(bindings["variables"][0]["value"], "[2,3]");
        let items = client
            .request(
                "variables",
                json!({ "variablesReference": bindings["variables"][0]["variablesReference"] }),
            )
            .await;
        assert_eq!(items["variables"][1]["value"], "3");
        let caller = client
            .request("scopes", json!({ "frameId": frames[1]["id"] }))
            .await;
        let args = client
            .request(
                "variables",
                json!({ "variablesReference": caller["scopes"][0]["variablesReference"] }),
            )
            .await;
        assert_eq!(args["variables"][0]["name"], "Args");
        let evaluated = client
            .request(
                "evaluate",
                json!({ "expression": "length(Items) + 100", "frameId": frames[0]["id"] }),
            )
            .await;
        assert_eq!(evaluated["result"], "102");

        client.request("next", json!({ "threadId": thread })).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "step");
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        assert_eq!(stack["stackFrames"][0]["line"], 6);
        client
            .request("continue", json!({ "threadId": thread }))
            .await;
        assert_eq!(
            client.event("stopped").await["body"]["reason"],
            "breakpoint"
        );
        let stack = client
            .request("stackTrace", json!({ "threadId": thread }))
            .await;
        assert_eq!(stack["stackFrames"][0]["line"], 10);
        client
            .request("continue", json!({ "threadId": thread }))
            .await;
        let exited = client.event("exited").await;
        assert_eq!(exited["body"]["exitCode"], 0);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !client.output().contains("total 10") && std::time::Instant::now() < deadline {
            client.gather(Duration::from_millis(500)).await;
        }
        assert!(
            client.output().contains("total 10 [\"x\"]"),
            "{}",
            client.output()
        );
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }
}
