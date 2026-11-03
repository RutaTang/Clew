//! `clew-server php-debug --php <php>`: a debug adapter for PHP, over Xdebug.
//!
//! Xdebug does not speak DAP: it is a PHP extension that connects OUT to a
//! debugging client and speaks DBGp — commands as text lines, replies as XML
//! packets. This adapter is that client. It speaks DAP on its stdin/stdout,
//! so clew runs it as it runs any stdio adapter, locally or on a remote host.
//!
//! On `launch` it listens on a loopback port and runs the program under
//! `php` with Xdebug told to connect there (`xdebug.mode=debug`,
//! `start_with_request=yes`); the program is held before its first line until
//! the session is configured. Breakpoints (conditional ones too), stepping,
//! the call stack, each frame's variables (locals, superglobals, constants)
//! to any depth, and expressions map onto DBGp's commands. What the program
//! prints is sent on as DAP output.
//!
//! One program, one thread: PHP runs a request on one thread, which the DAP
//! side calls thread 1. Xdebug cannot be paused from outside while the
//! program runs, so `pause` is refused, saying so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use clew_core::framing::read_message;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

use crate::dapout::{Out, forward};

/// How long Xdebug has to connect once the program started.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long `php -r` may take to say whether Xdebug is loaded.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the program has to exit once its session is over.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Largest DBGp packet read: a reply is a few KiB of XML; a corrupt length
/// must not become an allocation.
const MAX_PACKET: usize = 16 * 1024 * 1024;

/// The children of an array or object listed at once (DBGp's
/// `max_children`), and the bytes of a value shown (`max_data`).
const MAX_CHILDREN: usize = 100;
const MAX_DATA: usize = 4096;

/// Run the adapter on this process's stdin/stdout, with `php` the
/// interpreter that runs the program.
pub async fn run(php: PathBuf) -> Result<(), String> {
    serve(php, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(php: PathBuf, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let out = Out::new(output);
    let mut adapter = Adapter {
        php,
        out: out.clone(),
        session: None,
        program: None,
        breakpoints: HashMap::new(),
        refs: Arc::new(Mutex::new(Vec::new())),
        pause: None,
    };
    let mut input = BufReader::new(input);
    while let Ok(Some(message)) = read_message(&mut input).await {
        if message["type"] != "request" {
            continue;
        }
        if adapter.handle(&message).await == Flow::End {
            break;
        }
    }
    adapter.end().await;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Go,
    End,
}

/// What a `variablesReference` stands for: a context of a frame (`fullname`
/// none) or a value with children in it.
#[derive(Debug, Clone)]
struct VarRef {
    depth: u32,
    context: u32,
    fullname: Option<String>,
}

/// The program `launch` started: its exit code once it has one, and how to
/// stop it.
struct Program {
    exited: watch::Receiver<Option<i32>>,
    kill: Option<tokio::sync::oneshot::Sender<()>>,
}

struct Adapter {
    php: PathBuf,
    out: Out,
    /// The DBGp session, once Xdebug has connected. Held across a
    /// continuation (the program running) by the task awaiting its stop.
    session: Option<Arc<Mutex<Dbgp>>>,
    program: Option<Program>,
    /// Each file's breakpoints (by its `file://` URI): the engine's ids.
    breakpoints: HashMap<String, Vec<String>>,
    /// The `variablesReference`s handed out since the program last stopped.
    refs: Arc<Mutex<Vec<VarRef>>>,
    /// How to pause the running program, when this Xdebug can be asked to
    /// ([`PauseBy`]); none when it cannot.
    pause: Option<Pause>,
}

/// How a pause reaches the engine while the program runs.
#[derive(Clone)]
enum PauseBy {
    /// DBGp's `break`, on the debugging connection — where the engine takes
    /// a command while the program runs (`supports_async`).
    Break(Breaker),
    /// Xdebug's control socket (3.4 and later, on Linux): `pause`, on the
    /// abstract socket it names after the PHP process, which the engine
    /// reads between statements.
    ControlSocket(String),
}

/// A pause, asked of an engine that can be asked while the program runs.
#[derive(Clone)]
struct Pause {
    by: PauseBy,
    /// The program runs: a `break` is due only then.
    running: Arc<AtomicBool>,
    /// A pause was asked for: the next stop is it.
    asked: Arc<AtomicBool>,
}

impl Adapter {
    async fn handle(&mut self, request: &Value) -> Flow {
        let command = request["command"].as_str().unwrap_or("");
        let args = &request["arguments"];
        let answer = match command {
            "initialize" => Ok(json!({
                "supportsConfigurationDoneRequest": true,
                "supportsConditionalBreakpoints": true,
                "supportsEvaluateForHovers": true,
                "supportsTerminateRequest": true,
            })),
            "launch" => match self.launch(args).await {
                Ok(()) => {
                    self.out.respond(request, Ok(json!({}))).await;
                    self.out.event("initialized", json!({})).await;
                    return Flow::Go;
                }
                Err(e) => Err(e),
            },
            "setBreakpoints" => self.set_breakpoints(args).await,
            "configurationDone" => {
                self.out.respond(request, Ok(json!({}))).await;
                self.resume("run", "breakpoint").await;
                return Flow::Go;
            }
            "threads" => Ok(json!({ "threads": [{ "id": 1, "name": "PHP" }] })),
            "continue" | "next" | "stepIn" | "stepOut" => {
                let (dbgp, reason) = match command {
                    "continue" => ("run", "breakpoint"),
                    "next" => ("step_over", "step"),
                    "stepIn" => ("step_into", "step"),
                    _ => ("step_out", "step"),
                };
                self.out
                    .respond(request, Ok(json!({ "allThreadsContinued": true })))
                    .await;
                self.resume(dbgp, reason).await;
                return Flow::Go;
            }
            "pause" => match &self.pause {
                Some(pause) => {
                    if pause.running.load(Ordering::SeqCst) {
                        pause.asked.store(true, Ordering::SeqCst);
                        match &pause.by {
                            PauseBy::Break(breaker) => {
                                breaker.send("break").await.map(|()| json!({}))
                            }
                            PauseBy::ControlSocket(name) => {
                                signal_pause(name.clone(), self.out.clone());
                                Ok(json!({}))
                            }
                        }
                    } else {
                        Ok(json!({}))
                    }
                }
                None => Err(
                    "this Xdebug cannot pause a running PHP program: it takes no command \
                     while the program runs (supports_async is off), and has no control \
                     socket (Xdebug 3.4 and later, on Linux)"
                        .into(),
                ),
            },
            "stackTrace" => self.stack_trace().await,
            "scopes" => self.scopes(args).await,
            "variables" => self.variables(args).await,
            "evaluate" => self.evaluate(args).await,
            "disconnect" | "terminate" => {
                self.out.respond(request, Ok(json!({}))).await;
                return Flow::End;
            }
            other => Err(format!("{other} is not supported")),
        };
        self.out.respond(request, answer).await;
        Flow::Go
    }

    /// Start the program under `php` with Xdebug, and take its connection.
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
        let cwd = args["cwd"].as_str().map(PathBuf::from);
        xdebug_loaded(&self.php).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("cannot listen for Xdebug: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("cannot listen for Xdebug: {e}"))?
            .port();
        let mut command = tokio::process::Command::new(&self.php);
        command
            .args(xdebug_args(port))
            .arg(&program)
            .args(&program_args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run {}: {e}", self.php.display()))?;
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
        let (kill, killed) = tokio::sync::oneshot::channel::<()>();
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
        self.program = Some(Program {
            exited: exited.clone(),
            kill: Some(kill),
        });
        let stream = tokio::select! {
            accepted = tokio::time::timeout(CONNECT_TIMEOUT, listener.accept()) => match accepted {
                Ok(Ok((stream, _))) => stream,
                Ok(Err(e)) => return Err(format!("Xdebug did not connect: {e}")),
                Err(_) => return Err(format!(
                    "Xdebug did not connect within {} s — is it Xdebug 3, and enabled for {}?",
                    CONNECT_TIMEOUT.as_secs(),
                    self.php.display()
                )),
            },
            _ = exited.wait_for(Option::is_some) => {
                return Err("the program ended before Xdebug connected — see its output".into());
            }
        };
        let (read, write) = stream.into_split();
        let mut dbgp = Dbgp::new(read, write);
        let init = dbgp.packet().await?;
        if init.name != "init" {
            return Err(format!("Xdebug sent <{}> where <init> was due", init.name));
        }
        for (feature, value) in [
            ("max_depth", "1".to_string()),
            ("max_children", MAX_CHILDREN.to_string()),
            ("max_data", MAX_DATA.to_string()),
        ] {
            dbgp.command("feature_set", &[("-n", feature), ("-v", &value)], None)
                .await?;
        }
        let supports_async = dbgp
            .command("feature_get", &[("-n", "supports_async")], None)
            .await
            .is_ok_and(|reply| takes_async(&reply));
        let by = match supports_async {
            true => Some(PauseBy::Break(dbgp.breaker())),
            false => control_socket(&init).map(PauseBy::ControlSocket),
        };
        self.pause = by.map(|by| Pause {
            by,
            running: Arc::default(),
            asked: Arc::default(),
        });
        self.session = Some(Arc::new(Mutex::new(dbgp)));
        Ok(())
    }

    /// Replace one file's breakpoints.
    async fn set_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let path = args["source"]["path"]
            .as_str()
            .ok_or("a breakpoint needs its file's path")?;
        let wanted: Vec<(u64, Option<String>)> = args["breakpoints"]
            .as_array()
            .map(|bps| {
                bps.iter()
                    .filter_map(|bp| {
                        let line = bp["line"].as_u64()?;
                        let condition = bp["condition"]
                            .as_str()
                            .filter(|c| !c.trim().is_empty())
                            .map(str::to_string);
                        Some((line, condition))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(session) = &self.session else {
            let unverified: Vec<Value> = wanted
                .iter()
                .map(|(line, _)| json!({ "verified": false, "line": line, "message": "no program is running" }))
                .collect();
            return Ok(json!({ "breakpoints": unverified }));
        };
        let uri = path_to_uri(path);
        let mut dbgp = session.lock().await;
        for id in self.breakpoints.remove(&uri).unwrap_or_default() {
            let _ = dbgp
                .command("breakpoint_remove", &[("-d", &id)], None)
                .await;
        }
        let mut ids = Vec::new();
        let mut answers = Vec::new();
        for (line, condition) in wanted {
            let line_s = line.to_string();
            let kind = if condition.is_some() {
                "conditional"
            } else {
                "line"
            };
            let set = dbgp
                .command(
                    "breakpoint_set",
                    &[("-t", kind), ("-f", &uri), ("-n", &line_s)],
                    condition.as_deref(),
                )
                .await;
            match set {
                Ok(reply) => match reply.attr("id") {
                    Some(id) => {
                        ids.push(id.to_string());
                        answers.push(json!({ "id": ids.len(), "verified": true, "line": line }));
                    }
                    None => answers.push(json!({
                        "verified": false, "line": line,
                        "message": reply.error().unwrap_or_else(|| "not set".into()),
                    })),
                },
                Err(e) => answers.push(json!({ "verified": false, "line": line, "message": e })),
            }
        }
        self.breakpoints.insert(uri, ids);
        Ok(json!({ "breakpoints": answers }))
    }

    /// Run the program on (`run`, a step) in the background: what it stops
    /// at — or that it ended — arrives as an event once it does.
    async fn resume(&mut self, command: &'static str, reason: &'static str) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let out = self.out.clone();
        let refs = self.refs.clone();
        let exited = self.program.as_ref().map(|p| p.exited.clone());
        let pause = self.pause.clone();
        tokio::spawn(async move {
            let mut dbgp = session.lock().await;
            if let Some(pause) = &pause {
                pause.running.store(true, Ordering::SeqCst);
            }
            let reply = dbgp.command(command, &[], None).await;
            let paused = match &pause {
                Some(pause) => {
                    pause.running.store(false, Ordering::SeqCst);
                    pause.asked.swap(false, Ordering::SeqCst)
                }
                None => false,
            };
            match reply.as_ref().map(|r| r.attr("status").unwrap_or("")) {
                Ok("break") => {
                    refs.lock().await.clear();
                    let reason = if paused { "pause" } else { reason };
                    out.event(
                        "stopped",
                        json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true }),
                    )
                    .await;
                }
                _ => {
                    // The script ran to its end ("stopping"), or the
                    // connection went: let PHP finish and go.
                    let _ = tokio::time::timeout(EXIT_GRACE, dbgp.command("stop", &[], None)).await;
                    dbgp.closed = true;
                    drop(dbgp);
                    let code = match exited {
                        Some(mut exited) => {
                            tokio::time::timeout(EXIT_GRACE, exited.wait_for(Option::is_some))
                                .await
                                .ok()
                                .and_then(|r| r.ok().and_then(|code| *code))
                        }
                        None => None,
                    };
                    out.terminated(code).await;
                }
            }
        });
    }

    async fn session(&self) -> Result<Arc<Mutex<Dbgp>>, String> {
        self.session
            .clone()
            .ok_or_else(|| "no program is running".to_string())
    }

    async fn stack_trace(&self) -> Result<Value, String> {
        let session = self.session().await?;
        let reply = session.lock().await.command("stack_get", &[], None).await?;
        let frames: Vec<Value> = reply
            .children
            .iter()
            .filter(|c| c.name == "stack")
            .map(|frame| {
                let level: u64 = frame
                    .attr("level")
                    .and_then(|l| l.parse().ok())
                    .unwrap_or(0);
                let path = frame.attr("filename").map(uri_to_path).unwrap_or_default();
                let name = Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                json!({
                    "id": level + 1,
                    "name": frame.attr("where").unwrap_or("?"),
                    "source": { "path": path, "name": name },
                    "line": frame.attr("lineno").and_then(|l| l.parse::<u64>().ok()).unwrap_or(0),
                    "column": 1,
                })
            })
            .collect();
        Ok(json!({ "totalFrames": frames.len(), "stackFrames": frames }))
    }

    async fn scopes(&self, args: &Value) -> Result<Value, String> {
        let depth = frame_depth(args)?;
        let session = self.session().await?;
        let reply = session
            .lock()
            .await
            .command("context_names", &[("-d", &depth.to_string())], None)
            .await?;
        let mut refs = self.refs.lock().await;
        let scopes: Vec<Value> = reply
            .children
            .iter()
            .filter(|c| c.name == "context")
            .map(|context| {
                let id: u32 = context.attr("id").and_then(|i| i.parse().ok()).unwrap_or(0);
                refs.push(VarRef {
                    depth,
                    context: id,
                    fullname: None,
                });
                json!({
                    "name": context.attr("name").unwrap_or("Locals"),
                    "variablesReference": refs.len(),
                    "expensive": id != 0,
                })
            })
            .collect();
        Ok(json!({ "scopes": scopes }))
    }

    async fn variables(&self, args: &Value) -> Result<Value, String> {
        let reference = args["variablesReference"].as_u64().unwrap_or(0) as usize;
        let found = {
            let refs = self.refs.lock().await;
            reference.checked_sub(1).and_then(|i| refs.get(i)).cloned()
        };
        let Some(var) = found else {
            return Ok(json!({ "variables": [] }));
        };
        let session = self.session().await?;
        let depth = var.depth.to_string();
        let context = var.context.to_string();
        let properties = {
            let mut dbgp = session.lock().await;
            match &var.fullname {
                None => {
                    dbgp.command("context_get", &[("-d", &depth), ("-c", &context)], None)
                        .await?
                        .children
                }
                Some(fullname) => dbgp
                    .command(
                        "property_get",
                        &[("-d", &depth), ("-c", &context), ("-n", fullname)],
                        None,
                    )
                    .await?
                    .children
                    .into_iter()
                    .find(|p| p.name == "property")
                    .map(|p| p.children)
                    .unwrap_or_default(),
            }
        };
        let mut refs = self.refs.lock().await;
        let variables: Vec<Value> = properties
            .iter()
            .filter(|p| p.name == "property")
            .map(|p| variable(p, var.depth, var.context, &mut refs))
            .collect();
        Ok(json!({ "variables": variables }))
    }

    /// An expression's value: a variable (or an element or property of one)
    /// in the frame asked about; anything else, evaluated by PHP in the
    /// frame the program stopped in.
    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").trim().to_string();
        if expression.is_empty() {
            return Err("nothing to evaluate".into());
        }
        let depth = frame_depth(args).unwrap_or(0);
        let session = self.session().await?;
        let mut dbgp = session.lock().await;
        let property = if is_variable_path(&expression) {
            dbgp.command(
                "property_get",
                &[("-d", &depth.to_string()), ("-c", "0"), ("-n", &expression)],
                None,
            )
            .await
        } else {
            dbgp.command("eval", &[], Some(&expression)).await
        }?;
        if let Some(error) = property.error() {
            return Err(error);
        }
        let Some(value) = property.children.iter().find(|p| p.name == "property") else {
            return Err("no value".into());
        };
        drop(dbgp);
        let mut refs = self.refs.lock().await;
        let shown = variable(value, depth, 0, &mut refs);
        Ok(json!({
            "result": shown["value"],
            "type": shown["type"],
            "variablesReference": shown["variablesReference"],
        }))
    }

    /// The session is over: stop the program, and say so.
    async fn end(&mut self) {
        if let Some(program) = &mut self.program {
            if let Some(kill) = program.kill.take() {
                let _ = kill.send(());
            }
            let mut exited = program.exited.clone();
            let code = tokio::time::timeout(EXIT_GRACE, exited.wait_for(Option::is_some))
                .await
                .ok()
                .and_then(|r| r.ok().and_then(|code| *code));
            self.out.terminated(code).await;
        }
    }
}

/// The frame a request names (`frameId`, from [`Adapter::stack_trace`]: its
/// level plus one), as DBGp's depth.
fn frame_depth(args: &Value) -> Result<u32, String> {
    args["frameId"]
        .as_u64()
        .and_then(|id| id.checked_sub(1))
        .and_then(|d| u32::try_from(d).ok())
        .ok_or_else(|| "no such frame".to_string())
}

/// A DBGp `<property>` as a DAP variable — one with children given a
/// reference to list them by.
fn variable(property: &Xml, depth: u32, context: u32, refs: &mut Vec<VarRef>) -> Value {
    let kind = property.attr("type").unwrap_or("");
    let name = property
        .attr("name")
        .or_else(|| property.attr("fullname"))
        .unwrap_or("");
    let text = property.value();
    let value = match kind {
        "array" => format!("array({})", property.attr("numchildren").unwrap_or("0")),
        "object" => property.attr("classname").unwrap_or("object").to_string(),
        "null" | "uninitialized" => "null".to_string(),
        "bool" => match text.as_str() {
            "1" => "true".to_string(),
            _ => "false".to_string(),
        },
        "string" => format!("\"{text}\""),
        _ => text,
    };
    let reference = match (property.attr("children"), property.attr("fullname")) {
        (Some("1"), Some(fullname)) => {
            refs.push(VarRef {
                depth,
                context,
                fullname: Some(fullname.to_string()),
            });
            refs.len()
        }
        _ => 0,
    };
    json!({
        "name": name,
        "value": value,
        "type": kind,
        "variablesReference": reference,
    })
}

/// Whether `expression` names a variable, an element or a property of one
/// (`$cart`, `$cart["apple"]`, `$user->name`): what DBGp's `property_get`
/// reads in any frame, where `eval` reads only the top one.
fn is_variable_path(expression: &str) -> bool {
    expression.starts_with('$')
        && expression.len() > 1
        && expression.chars().all(|c| {
            c.is_alphanumeric() || matches!(c, '_' | '$' | '[' | ']' | '\'' | '"' | '-' | '>' | ':')
        })
        && !expression.contains("--")
}

/// The `php` settings that have Xdebug connect to `port` at once.
fn xdebug_args(port: u16) -> Vec<String> {
    vec![
        "-dxdebug.mode=debug".into(),
        "-dxdebug.start_with_request=yes".into(),
        "-dxdebug.client_host=127.0.0.1".into(),
        format!("-dxdebug.client_port={port}"),
        // The control socket (pause, Xdebug 3.4+), polled by time: its
        // default turns it off where the CPU's clock is not one it trusts.
        "-dxdebug.control_socket=time".into(),
    ]
}

/// The name of Xdebug's control socket for the PHP process an `init`
/// packet is from — where this Xdebug has one (3.4 and later, on Linux).
fn control_socket(init: &Xml) -> Option<String> {
    let version = init
        .children
        .iter()
        .find(|c| c.name == "engine")?
        .attr("version")?;
    let mut parts = version.split(['.', '-']).map(|p| p.parse::<u32>().ok());
    let (major, minor) = (parts.next()??, parts.next()??);
    let pid = init.attr("appid")?.parse::<u32>().ok()?;
    (cfg!(target_os = "linux") && (major, minor) >= (3, 4)).then(|| format!("xdebug-ctrl.{pid}"))
}

/// Ask Xdebug, on its control socket `name`, to break into the debugger at
/// the program's next statement — the stop that answers the waiting `run`
/// is the pause. The engine reads the socket only between statements, so
/// its reply is waited for in the background — and always read: Xdebug
/// writes it whether or not anyone reads, and to a closed connection that
/// would kill PHP (SIGPIPE).
fn signal_pause(name: String, out: Out) {
    tokio::task::spawn_blocking(move || {
        if let Err(e) = ask_control_socket(&name, "pause") {
            let handle = tokio::runtime::Handle::current();
            handle.block_on(out.output("console", format!("pause: {e}\n")));
        }
    });
}

/// Send `command` on Xdebug's control socket `name`; Xdebug's reply, read
/// to its end, or why there was none or it refused.
#[cfg(target_os = "linux")]
fn ask_control_socket(name: &str, command: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .map_err(|e| format!("no control socket {name}: {e}"))?;
    let mut stream = std::os::unix::net::UnixStream::connect_addr(&addr)
        .map_err(|e| format!("Xdebug's control socket did not answer: {e}"))?;
    stream
        .write_all(format!("{command}\0").as_bytes())
        .map_err(|e| format!("Xdebug's control socket broke: {e}"))?;
    let mut reply = Vec::new();
    stream
        .read_to_end(&mut reply)
        .map_err(|e| format!("Xdebug's control socket broke: {e}"))?;
    let text = String::from_utf8_lossy(&reply);
    let text = text.trim_end_matches('\0').trim();
    if text.is_empty() {
        return Err("the program ended before Xdebug read it".into());
    }
    let xml = Xml::parse(text).map_err(|e| format!("Xdebug's control socket said: {e}"))?;
    let refused = xml
        .children
        .iter()
        .find(|c| c.name == "error")
        .map(
            |error| match error.children.iter().find(|m| m.name == "message") {
                Some(message) => message.text.trim().to_string(),
                None => format!("error {}", error.attr("code").unwrap_or("?")),
            },
        );
    match refused {
        Some(why) => Err(format!("Xdebug refused: {why}")),
        None => Ok(text.to_string()),
    }
}

#[cfg(not(target_os = "linux"))]
fn ask_control_socket(name: &str, _command: &str) -> Result<String, String> {
    Err(format!("no control socket {name} here"))
}

/// Whether `php` has Xdebug loaded; an error saying how to get it if not.
async fn xdebug_loaded(php: &Path) -> Result<(), String> {
    let probe = tokio::process::Command::new(php)
        .args(["-r", "exit(extension_loaded('xdebug') ? 0 : 3);"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Ok(Ok(status)) if status.success() => Ok(()),
        Ok(Ok(_)) => Err(format!(
            "Xdebug is not loaded in {} — install it (pecl install xdebug, or your system's \
             php-xdebug package) and enable it",
            php.display()
        )),
        Ok(Err(e)) => Err(format!("cannot run {}: {e}", php.display())),
        Err(_) => Err(format!("{} did not answer in time", php.display())),
    }
}

/// A local path as the `file://` URI Xdebug names files by.
fn path_to_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }
    for byte in path.replace('\\', "/").bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                uri.push(byte as char)
            }
            other => uri.push_str(&format!("%{other:02X}")),
        }
    }
    uri
}

/// A `file://` URI as the path it names.
fn uri_to_path(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && i + 2 < raw.len()
            && let Ok(byte) = u8::from_str_radix(&rest[i + 1..i + 3], 16)
        {
            bytes.push(byte);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let path = String::from_utf8_lossy(&bytes).into_owned();
    // `file:///C:/x` names `C:/x`.
    match path.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => path[1..].to_string(),
        _ => path,
    }
}

/// An XML element as DBGp sends it, owned: its local name, attributes, text
/// and child elements.
#[derive(Debug, Clone, Default, PartialEq)]
struct Xml {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Xml>,
}

impl Xml {
    fn parse(text: &str) -> Result<Xml, String> {
        let document = roxmltree::Document::parse(text)
            .map_err(|e| format!("Xdebug sent a reply that is not XML: {e}"))?;
        Ok(Xml::from_node(document.root_element()))
    }

    fn from_node(node: roxmltree::Node) -> Xml {
        Xml {
            name: node.tag_name().name().to_string(),
            attrs: node
                .attributes()
                .map(|a| (a.name().to_string(), a.value().to_string()))
                .collect(),
            text: node
                .children()
                .filter(|c| c.is_text())
                .filter_map(|c| c.text())
                .collect(),
            children: node
                .children()
                .filter(|c| c.is_element())
                .map(Xml::from_node)
                .collect(),
        }
    }

    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// A property's value: its text, decoded when DBGp sent it in base64.
    fn value(&self) -> String {
        match self.attr("encoding") {
            Some("base64") => base64::engine::general_purpose::STANDARD
                .decode(self.text.trim())
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default(),
            _ => self.text.clone(),
        }
    }

    /// The error a reply carries (`<error><message>…</message></error>`).
    fn error(&self) -> Option<String> {
        let error = self.children.iter().find(|c| c.name == "error")?;
        let message = error
            .children
            .iter()
            .find(|c| c.name == "message")
            .map(|m| m.text.trim().to_string())
            .filter(|m| !m.is_empty());
        Some(message.unwrap_or_else(|| format!("error {}", error.attr("code").unwrap_or("?"))))
    }
}

/// The DBGp connection: commands out, replies (and the `init` packet) in.
struct Dbgp {
    read: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    /// Shared with a [`Breaker`], which writes while a command waits.
    write: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    transaction: Arc<AtomicU64>,
    /// The session ended: a command is refused rather than written to a
    /// connection that is going.
    closed: bool,
}

impl Dbgp {
    fn new<R, W>(read: R, write: W) -> Dbgp
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Dbgp {
            read: BufReader::new(Box::new(read)),
            write: Arc::new(Mutex::new(Box::new(write))),
            transaction: Arc::default(),
            closed: false,
        }
    }

    /// What sends a command while another waits for its reply (`break`,
    /// while the program runs).
    fn breaker(&self) -> Breaker {
        Breaker {
            write: self.write.clone(),
            transaction: self.transaction.clone(),
        }
    }

    /// One packet: `<length>\0<xml>\0`.
    async fn packet(&mut self) -> Result<Xml, String> {
        use tokio::io::AsyncBufReadExt;
        let mut length = Vec::new();
        let n = (&mut self.read)
            .take(32)
            .read_until(0, &mut length)
            .await
            .map_err(|e| format!("the connection to Xdebug broke: {e}"))?;
        if n == 0 || length.last() != Some(&0) {
            return Err("Xdebug closed the connection".into());
        }
        length.pop();
        let length: usize = std::str::from_utf8(&length)
            .ok()
            .and_then(|l| l.trim().parse().ok())
            .filter(|&l| l <= MAX_PACKET)
            .ok_or("Xdebug sent a packet with no length")?;
        let mut body = vec![0u8; length + 1];
        self.read
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("the connection to Xdebug broke: {e}"))?;
        body.pop();
        Xml::parse(&String::from_utf8_lossy(&body))
    }

    /// Send `name` with `args` (and `data`, base64-encoded, after `--`), and
    /// wait for its reply — notifications and stream packets passed over.
    async fn command(
        &mut self,
        name: &str,
        args: &[(&str, &str)],
        data: Option<&str>,
    ) -> Result<Xml, String> {
        if self.closed {
            return Err("the program has ended".into());
        }
        let id = (self.transaction.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let mut line = format!("{name} -i {id}");
        for (flag, value) in args {
            line.push(' ');
            line.push_str(flag);
            line.push(' ');
            line.push_str(&quote(value));
        }
        if let Some(data) = data {
            line.push_str(" -- ");
            line.push_str(&base64::engine::general_purpose::STANDARD.encode(data));
        }
        line.push('\0');
        write_line(&self.write, &line).await?;
        loop {
            let reply = self.packet().await?;
            if reply.name == "response" && reply.attr("transaction_id") == Some(&id) {
                return Ok(reply);
            }
        }
    }
}

/// Whether a `feature_get -n supports_async` reply says the engine takes a
/// command while the program runs.
fn takes_async(reply: &Xml) -> bool {
    reply.attr("supported") != Some("0") && reply.value().trim() == "1"
}

/// A command sent with no wait for its reply — its reply is passed over by
/// whichever command reads next.
#[derive(Clone)]
struct Breaker {
    write: Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    transaction: Arc<AtomicU64>,
}

impl Breaker {
    async fn send(&self, name: &str) -> Result<(), String> {
        let id = self.transaction.fetch_add(1, Ordering::SeqCst) + 1;
        write_line(&self.write, &format!("{name} -i {id}\0")).await
    }
}

async fn write_line(
    write: &Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    line: &str,
) -> Result<(), String> {
    let mut write = write.lock().await;
    write
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("the connection to Xdebug broke: {e}"))?;
    write
        .flush()
        .await
        .map_err(|e| format!("the connection to Xdebug broke: {e}"))
}

/// A DBGp argument value, quoted when it holds a space or a quote.
fn quote(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '"', '\\', '\0']) {
        return value.to_string();
    }
    let mut quoted = String::from("\"");
    for c in value.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapout::testing::Client;

    /// The next command line the fake engine got — a missing one fails
    /// rather than waiting for ever.
    async fn next_line(seen: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("the engine got a command in time")
            .expect("the engine is up")
    }

    /// The adapter's next message.
    async fn next_message(from: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        tokio::time::timeout(Duration::from_secs(5), read_message(from))
            .await
            .expect("the adapter said something in time")
            .unwrap()
            .expect("the adapter is up")
    }

    /// An engine that answers each command with the reply `script` gives
    /// it (`TX` replaced by the command's transaction id), and reports
    /// every command line it got.
    fn fake_engine(
        script: fn(&str) -> String,
    ) -> (Dbgp, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ours);
        let (seen_tx, seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let (engine_read, mut engine_write) = tokio::io::split(theirs);
            let mut engine_read = BufReader::new(engine_read);
            loop {
                let mut line = Vec::new();
                match engine_read.read_until(0, &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                line.pop();
                let line = String::from_utf8_lossy(&line).into_owned();
                let name = line.split(' ').next().unwrap_or("").to_string();
                let tx = line
                    .split(" -i ")
                    .nth(1)
                    .and_then(|r| r.split(' ').next())
                    .unwrap_or("0")
                    .to_string();
                let _ = seen_tx.send(line.clone());
                let body = script(&name).replace("TX", &tx);
                let packet = format!("{}\0{body}\0", body.len());
                if engine_write.write_all(packet.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        (Dbgp::new(read, write), seen)
    }

    const NS: &str =
        r#"xmlns="urn:debugger_protocol_v1" xmlns:xdebug="https://xdebug.org/dbgp/xdebug""#;

    fn engine_reply(command: &str) -> String {
        let body = match command {
            "breakpoint_set" => String::new(),
            "stack_get" => r#"<stack where="total" level="0" type="file" filename="file:///p/my%20app.php" lineno="5"></stack><stack where="{main}" level="1" type="file" filename="file:///p/my%20app.php" lineno="10"></stack>"#.into(),
            "context_names" => r#"<context name="Locals" id="0"></context><context name="Superglobals" id="1"></context>"#.into(),
            "context_get" => r#"<property name="$name" fullname="$name" type="string" size="3" encoding="base64"><![CDATA[YW5u]]></property><property name="$ok" fullname="$ok" type="bool"><![CDATA[1]]></property><property name="$items" fullname="$items" type="array" children="1" numchildren="2"></property><property name="$user" fullname="$user" type="object" classname="App\User" children="1" numchildren="1"></property><property name="$none" fullname="$none" type="null"></property>"#.into(),
            "property_get" => r#"<property name="$items" fullname="$items" type="array" children="1" numchildren="2"><property name="apple" fullname="$items[&quot;apple&quot;]" type="int"><![CDATA[2]]></property><property name="pear" fullname="$items[&quot;pear&quot;]" type="int"><![CDATA[3]]></property></property>"#.into(),
            "eval" => r#"<property type="int"><![CDATA[100]]></property>"#.into(),
            "run" => return format!(r#"<?xml version="1.0" encoding="iso-8859-1"?><response {NS} command="run" transaction_id="TX" status="break" reason="ok"><xdebug:message filename="file:///p/my%20app.php" lineno="5"></xdebug:message></response>"#),
            "step_over" => return format!(r#"<?xml version="1.0"?><response {NS} command="step_over" transaction_id="TX" status="stopping" reason="ok"></response>"#),
            _ => String::new(),
        };
        let id = match command {
            "breakpoint_set" => r#" id="7001""#,
            _ => "",
        };
        format!(
            r#"<?xml version="1.0" encoding="iso-8859-1"?><response {NS} command="{command}" transaction_id="TX"{id}>{body}</response>"#
        )
    }

    /// An adapter whose session is a scripted engine, and what it writes.
    fn scripted() -> (
        Adapter,
        tokio::sync::mpsc::UnboundedReceiver<String>,
        BufReader<tokio::io::DuplexStream>,
    ) {
        let (dbgp, seen) = fake_engine(engine_reply);
        let (adapter_out, from) = tokio::io::duplex(1 << 20);
        let adapter = Adapter {
            php: PathBuf::from("php"),
            out: Out::new(adapter_out),
            session: Some(Arc::new(Mutex::new(dbgp))),
            program: None,
            breakpoints: HashMap::new(),
            refs: Arc::new(Mutex::new(Vec::new())),
            pause: None,
        };
        (adapter, seen, BufReader::new(from))
    }

    async fn ask(
        adapter: &mut Adapter,
        from: &mut BufReader<tokio::io::DuplexStream>,
        command: &str,
        arguments: Value,
    ) -> Value {
        let request =
            json!({ "seq": 1, "type": "request", "command": command, "arguments": arguments });
        assert_eq!(adapter.handle(&request).await, Flow::Go);
        let response = next_message(from).await;
        assert_eq!(response["success"], true, "{command}: {response}");
        response["body"].clone()
    }

    /// Each DAP request becomes the DBGp command it maps to, and each reply
    /// the DAP answer: breakpoints (a condition sent in base64) on the
    /// file's URI, the stack with paths decoded, the frame's contexts as
    /// scopes, values shown by type (base64 decoded), children listed by
    /// reference, expressions read or evaluated.
    #[tokio::test]
    async fn requests_map_onto_dbgp_and_back() {
        let (mut adapter, mut seen, mut from) = scripted();
        let set = ask(
            &mut adapter,
            &mut from,
            "setBreakpoints",
            json!({ "source": { "path": "/p/my app.php" }, "breakpoints": [{ "line": 5 }, { "line": 9, "condition": "$t == 5" }] }),
        )
        .await;
        assert_eq!(set["breakpoints"][0]["verified"], true);
        assert_eq!(
            next_line(&mut seen).await,
            "breakpoint_set -i 1 -t line -f file:///p/my%20app.php -n 5"
        );
        assert_eq!(
            next_line(&mut seen).await,
            "breakpoint_set -i 2 -t conditional -f file:///p/my%20app.php -n 9 -- JHQgPT0gNQ=="
        );
        // Set again: the old ones are taken off first.
        ask(
            &mut adapter,
            &mut from,
            "setBreakpoints",
            json!({ "source": { "path": "/p/my app.php" }, "breakpoints": [] }),
        )
        .await;
        assert_eq!(next_line(&mut seen).await, "breakpoint_remove -i 3 -d 7001");

        let stack = ask(
            &mut adapter,
            &mut from,
            "stackTrace",
            json!({ "threadId": 1 }),
        )
        .await;
        assert_eq!(stack["stackFrames"][0]["id"], 1);
        assert_eq!(stack["stackFrames"][0]["source"]["path"], "/p/my app.php");
        assert_eq!(stack["stackFrames"][1]["line"], 10);

        let scopes = ask(&mut adapter, &mut from, "scopes", json!({ "frameId": 2 })).await;
        assert_eq!(scopes["scopes"][1]["expensive"], true);
        while let Ok(line) = seen.try_recv() {
            assert!(
                !line.starts_with("context_names") || line.ends_with("-d 1"),
                "{line}"
            );
        }
        let locals = ask(
            &mut adapter,
            &mut from,
            "variables",
            json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
        )
        .await;
        let values: Vec<(&str, &str)> = locals["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| (v["name"].as_str().unwrap(), v["value"].as_str().unwrap()))
            .collect();
        assert_eq!(
            values,
            [
                ("$name", "\"ann\""),
                ("$ok", "true"),
                ("$items", "array(2)"),
                ("$user", "App\\User"),
                ("$none", "null")
            ]
        );
        let items = locals["variables"][2]["variablesReference"].clone();
        assert_ne!(items, 0);
        assert_eq!(locals["variables"][0]["variablesReference"], 0);
        let children = ask(
            &mut adapter,
            &mut from,
            "variables",
            json!({ "variablesReference": items }),
        )
        .await;
        assert_eq!(children["variables"][1]["value"], "3");
        let mut asked = Vec::new();
        while let Ok(line) = seen.try_recv() {
            asked.push(line);
        }
        assert!(
            asked
                .iter()
                .any(|l| l.contains("context_get") && l.contains("-d 1 -c 0")),
            "{asked:?}"
        );
        assert!(
            asked
                .iter()
                .any(|l| l.contains("property_get") && l.contains("-d 1 -c 0 -n $items")),
            "{asked:?}"
        );

        let evaluated = ask(
            &mut adapter,
            &mut from,
            "evaluate",
            json!({ "expression": "$sum + 100", "frameId": 1 }),
        )
        .await;
        assert_eq!(evaluated["result"], "100");
        assert!(next_line(&mut seen).await.starts_with("eval -i "));
        ask(
            &mut adapter,
            &mut from,
            "evaluate",
            json!({ "expression": "$items[\"apple\"]", "frameId": 2 }),
        )
        .await;
        assert!(
            next_line(&mut seen)
                .await
                .ends_with(r#"-d 1 -c 0 -n "$items[\"apple\"]""#),
            "a name with quotes is quoted"
        );
    }

    /// `continue` runs the program on and reports where it stopped; a step
    /// that ends it stops the engine and says the program terminated.
    #[tokio::test]
    async fn the_program_runs_on_and_its_end_is_reported() {
        let (mut adapter, mut seen, mut from) = scripted();
        ask(
            &mut adapter,
            &mut from,
            "continue",
            json!({ "threadId": 1 }),
        )
        .await;
        let stopped = next_message(&mut from).await;
        assert_eq!(stopped["event"], "stopped");
        assert_eq!(stopped["body"]["reason"], "breakpoint");
        assert!(next_line(&mut seen).await.starts_with("run -i "));
        ask(&mut adapter, &mut from, "next", json!({ "threadId": 1 })).await;
        let terminated = next_message(&mut from).await;
        assert_eq!(terminated["event"], "terminated");
        assert!(next_line(&mut seen).await.starts_with("step_over -i "));
        assert!(next_line(&mut seen).await.starts_with("stop -i "));
        // Over: further commands are refused, not sent.
        let request =
            json!({ "seq": 9, "type": "request", "command": "stackTrace", "arguments": {} });
        adapter.handle(&request).await;
        let refused = next_message(&mut from).await;
        assert_eq!(refused["success"], false);
        assert_eq!(
            adapter
                .handle(&json!({ "seq": 10, "type": "request", "command": "pause" }))
                .await,
            Flow::Go
        );
        let pause = next_message(&mut from).await;
        assert!(pause["message"].as_str().unwrap().contains("cannot pause"));
    }

    /// An engine that takes `break` while the program runs (Xdebug with
    /// `supports_async`): `run` is answered only once `break` comes, as a
    /// stop — which is reported as the pause; a pause with nothing running
    /// sends nothing.
    #[tokio::test]
    async fn a_running_program_is_paused_where_the_engine_takes_break() {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ours);
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let (engine_read, mut engine_write) = tokio::io::split(theirs);
            let mut engine_read = BufReader::new(engine_read);
            let mut running = None;
            loop {
                let mut line = Vec::new();
                match engine_read.read_until(0, &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                line.pop();
                let line = String::from_utf8_lossy(&line).into_owned();
                let _ = seen_tx.send(line.clone());
                let mut words = line.split(' ');
                let name = words.next().unwrap_or("").to_string();
                let tx = words.nth(1).unwrap_or("0").to_string();
                let replies = match name.as_str() {
                    "run" => {
                        running = Some(tx);
                        vec![]
                    }
                    "break" => {
                        let mut replies = vec![format!(
                            r#"<?xml version="1.0"?><response {NS} command="break" transaction_id="{tx}" success="1"></response>"#
                        )];
                        if let Some(run) = running.take() {
                            replies.push(format!(
                                r#"<?xml version="1.0"?><response {NS} command="run" transaction_id="{run}" status="break" reason="ok"></response>"#
                            ));
                        }
                        replies
                    }
                    other => vec![engine_reply(other).replace("TX", &tx)],
                };
                for body in replies {
                    let packet = format!("{}\0{body}\0", body.len());
                    if engine_write.write_all(packet.as_bytes()).await.is_err() {
                        return;
                    }
                }
            }
        });
        let dbgp = Dbgp::new(read, write);
        let pause = Pause {
            by: PauseBy::Break(dbgp.breaker()),
            running: Arc::default(),
            asked: Arc::default(),
        };
        let (adapter_out, from) = tokio::io::duplex(1 << 20);
        let mut from = BufReader::new(from);
        let mut adapter = Adapter {
            php: PathBuf::from("php"),
            out: Out::new(adapter_out),
            session: Some(Arc::new(Mutex::new(dbgp))),
            program: None,
            breakpoints: HashMap::new(),
            refs: Arc::new(Mutex::new(Vec::new())),
            pause: Some(pause.clone()),
        };
        // Stopped: a pause sends nothing.
        ask(&mut adapter, &mut from, "pause", json!({ "threadId": 1 })).await;
        assert!(seen.try_recv().is_err(), "nothing sent while stopped");
        ask(
            &mut adapter,
            &mut from,
            "continue",
            json!({ "threadId": 1 }),
        )
        .await;
        assert!(next_line(&mut seen).await.starts_with("run -i "));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pause.running.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "the run began");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ask(&mut adapter, &mut from, "pause", json!({ "threadId": 1 })).await;
        assert!(next_line(&mut seen).await.starts_with("break -i "));
        let stopped = next_message(&mut from).await;
        assert_eq!(stopped["event"], "stopped", "{stopped}");
        assert_eq!(stopped["body"]["reason"], "pause");
        // The break's own reply is passed over by the next command.
        let stack = ask(
            &mut adapter,
            &mut from,
            "stackTrace",
            json!({ "threadId": 1 }),
        )
        .await;
        assert_eq!(stack["stackFrames"][0]["line"], 5);
        // A stop of its own afterwards is not taken for the pause.
        assert!(!pause.asked.load(Ordering::SeqCst));
    }

    /// Xdebug 3.4+ on Linux: the pause goes to its control socket — the
    /// abstract socket it names after the PHP process — as `pause`, and the
    /// engine, breaking at the next statement, answers the waiting `run`
    /// with the stop that is reported as the pause. The socket's reply is
    /// always read (Xdebug writes it either way).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_running_program_is_paused_through_xdebug_s_control_socket() {
        use std::io::{Read, Write};
        use std::os::linux::net::SocketAddrExt;
        let name = format!(
            "xdebug-ctrl.clew-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        // The engine: `run` answered once the control socket says pause.
        let (signalled_tx, mut signalled) = tokio::sync::mpsc::unbounded_channel::<String>();
        let ctrl = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut byte = [0u8; 1];
            while conn.read(&mut byte).unwrap() == 1 && byte[0] != 0 {
                got.push(byte[0]);
            }
            let command = String::from_utf8(got).unwrap();
            let _ = signalled_tx.send(command);
            let reply = r#"<?xml version="1.0" encoding="UTF-8"?>
<ctrl-response xmlns:xdebug-ctrl="https://xdebug.org/ctrl/xdebug"><pause success="1"><pid>1</pid><action>Breakpoint Signalled</action></pause></ctrl-response>"#;
            conn.write_all(reply.as_bytes()).unwrap();
            conn.write_all(b"\0").unwrap();
        });
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (read, write) = tokio::io::split(ours);
        let (seen_tx, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let (paused_tx, mut paused) = tokio::sync::mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let (engine_read, mut engine_write) = tokio::io::split(theirs);
            let mut engine_read = BufReader::new(engine_read);
            loop {
                let mut line = Vec::new();
                match engine_read.read_until(0, &mut line).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                line.pop();
                let line = String::from_utf8_lossy(&line).into_owned();
                let _ = seen_tx.send(line.clone());
                let mut words = line.split(' ');
                let name = words.next().unwrap_or("").to_string();
                let tx = words.nth(1).unwrap_or("0").to_string();
                let body = match name.as_str() {
                    "run" => {
                        // Held until the control socket's pause.
                        let _ = paused.recv().await;
                        format!(
                            r#"<?xml version="1.0"?><response {NS} command="run" transaction_id="{tx}" status="break" reason="ok"></response>"#
                        )
                    }
                    other => engine_reply(other).replace("TX", &tx),
                };
                let packet = format!("{}\0{body}\0", body.len());
                if engine_write.write_all(packet.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        let pause = Pause {
            by: PauseBy::ControlSocket(name),
            running: Arc::default(),
            asked: Arc::default(),
        };
        let (adapter_out, from) = tokio::io::duplex(1 << 20);
        let mut from = BufReader::new(from);
        let mut adapter = Adapter {
            php: PathBuf::from("php"),
            out: Out::new(adapter_out),
            session: Some(Arc::new(Mutex::new(Dbgp::new(read, write)))),
            program: None,
            breakpoints: HashMap::new(),
            refs: Arc::new(Mutex::new(Vec::new())),
            pause: Some(pause.clone()),
        };
        ask(
            &mut adapter,
            &mut from,
            "continue",
            json!({ "threadId": 1 }),
        )
        .await;
        assert!(next_line(&mut seen).await.starts_with("run -i "));
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pause.running.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "the run began");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ask(&mut adapter, &mut from, "pause", json!({ "threadId": 1 })).await;
        let command = tokio::time::timeout(Duration::from_secs(5), signalled.recv())
            .await
            .expect("the control socket got the pause")
            .unwrap();
        assert_eq!(command, "pause");
        paused_tx.send(()).unwrap();
        let stopped = next_message(&mut from).await;
        assert_eq!(stopped["event"], "stopped", "{stopped}");
        assert_eq!(stopped["body"]["reason"], "pause");
        ctrl.join().unwrap();
    }

    /// The control socket's name comes from the `init` packet — Xdebug 3.4
    /// and later, the PHP process's id — and a refusal on it is said.
    #[cfg(target_os = "linux")]
    #[test]
    fn xdebug_s_control_socket_is_named_and_its_refusals_read() {
        let init = |version: &str, appid: &str| {
            Xml::parse(&format!(
                r#"<init {NS} appid="{appid}" protocol_version="1.0"><engine version="{version}"><![CDATA[Xdebug]]></engine></init>"#
            ))
            .unwrap()
        };
        assert_eq!(
            control_socket(&init("3.5.0", "4321")).as_deref(),
            Some("xdebug-ctrl.4321")
        );
        assert_eq!(
            control_socket(&init("3.4.0beta1", "7")).as_deref(),
            Some("xdebug-ctrl.7")
        );
        assert_eq!(control_socket(&init("3.2.0", "4321")), None, "before 3.4");
        assert_eq!(control_socket(&init("3.5.0", "")), None);
        assert!(xdebug_args(9000).contains(&"-dxdebug.control_socket=time".to_string()));

        use std::io::{Read, Write};
        use std::os::linux::net::SocketAddrExt;
        let name = format!("xdebug-ctrl.clew-refusal-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        let engine = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = conn.read(&mut buf).unwrap();
            conn.write_all(br#"<?xml version="1.0" encoding="UTF-8"?>
<ctrl-response xmlns:xdebug-ctrl="https://xdebug.org/ctrl/xdebug"><error code="400"><message>step debugger is not enabled</message></error><pause success="1"><pid>1</pid></pause></ctrl-response>"#).unwrap();
            conn.write_all(b"\0").unwrap();
        });
        let err = ask_control_socket(&name, "pause").unwrap_err();
        assert!(err.contains("step debugger is not enabled"), "{err}");
        engine.join().unwrap();
        assert!(ask_control_socket("xdebug-ctrl.nobody-here", "pause").is_err());
    }

    #[test]
    fn the_engine_says_whether_it_takes_commands_while_running() {
        let reply = |xml: &str| Xml::parse(xml).unwrap();
        assert!(takes_async(&reply(&format!(
            r#"<response {NS} command="feature_get" transaction_id="1" feature_name="supports_async" supported="1"><![CDATA[1]]></response>"#
        ))));
        assert!(!takes_async(&reply(&format!(
            r#"<response {NS} command="feature_get" transaction_id="1" feature_name="supports_async" supported="1"><![CDATA[0]]></response>"#
        ))));
        assert!(!takes_async(&reply(&format!(
            r#"<response {NS} command="feature_get" transaction_id="1" feature_name="supports_async" supported="0"></response>"#
        ))));
    }

    #[test]
    fn values_are_quoted_and_paths_named_as_dbgp_names_them() {
        assert_eq!(quote("$items"), "$items");
        assert_eq!(quote(r#"$a["k"]"#), r#""$a[\"k\"]""#);
        assert_eq!(quote("a b"), r#""a b""#);
        assert_eq!(quote(""), r#""""#);
        for path in ["/p/my app.php", "/p/ünï/x.php", "/p/100%.php"] {
            assert_eq!(uri_to_path(&path_to_uri(path)), path);
        }
        assert_eq!(path_to_uri("/p/my app.php"), "file:///p/my%20app.php");
        assert_eq!(uri_to_path("file:///C:/www/index.php"), "C:/www/index.php");
        for (expression, variable) in [
            ("$cart", true),
            (r#"$cart["pear"]"#, true),
            ("$user->name", true),
            ("$sum + 100", false),
            ("count($cart)", false),
            ("$", false),
            ("$a--", false),
        ] {
            assert_eq!(is_variable_path(expression), variable, "{expression}");
        }
    }

    /// A PHP program debugged for real, under Xdebug: held at a breakpoint,
    /// its stack and variables read (an array's elements too), expressions
    /// evaluated, stepped, stopped again where a condition holds, and run to
    /// its end with its output and exit code. Needs `php` with Xdebug 3.
    #[tokio::test]
    #[ignore = "needs php with Xdebug 3; run explicitly"]
    async fn a_php_program_is_debugged_under_xdebug() {
        let dir = clew_core::testutil::TempDir::new("php-debug");
        let program = dir.join("app.php");
        std::fs::write(
            &program,
            "<?php\nfunction total(array $items): int {\n    $sum = 0;\n    foreach ($items as $i) {\n        $sum += $i;\n    }\n    return $sum;\n}\n$cart = ['apple' => 2, 'pear' => 3];\n$t = total($cart);\necho \"total $t\\n\";\n",
        )
        .unwrap();
        let php = clew_core::lsp::store::find_on_path_unresolved("php").expect("php on PATH");
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(php, input, output));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "args": [], "cwd": dir.to_string_lossy() }),
            )
            .await;
        let launched = client.answer(launch).await;
        assert_eq!(launched["success"], true, "{launched}");
        client.event("initialized").await;
        let source = json!({ "path": program.to_string_lossy() });
        let set = client
            .request(
                "setBreakpoints",
                json!({ "source": source, "breakpoints": [{ "line": 5 }, { "line": 11, "condition": "$t == 5" }] }),
            )
            .await;
        assert_eq!(set["breakpoints"][0]["verified"], true, "{set}");
        assert_eq!(set["breakpoints"][1]["verified"], true, "{set}");
        client.request("configurationDone", json!({})).await;

        let stopped = client.event("stopped").await;
        assert_eq!(stopped["body"]["reason"], "breakpoint");
        let stack = client.request("stackTrace", json!({ "threadId": 1 })).await;
        assert_eq!(stack["stackFrames"][0]["name"], "total");
        assert_eq!(stack["stackFrames"][0]["line"], 5);
        assert_eq!(
            stack["stackFrames"][0]["source"]["path"],
            program.to_string_lossy().as_ref()
        );
        assert_eq!(stack["stackFrames"][1]["name"], "{main}");
        let scopes = client.request("scopes", json!({ "frameId": 1 })).await;
        assert_eq!(scopes["scopes"][0]["name"], "Locals");
        let locals = client
            .request(
                "variables",
                json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
            )
            .await;
        let local = |name: &str| {
            locals["variables"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("{name} in {locals}"))
        };
        assert_eq!(local("$sum")["value"], "0");
        assert_eq!(local("$items")["value"], "array(2)");
        let items = client
            .request(
                "variables",
                json!({ "variablesReference": local("$items")["variablesReference"] }),
            )
            .await;
        assert_eq!(items["variables"][0]["name"], "apple");
        assert_eq!(items["variables"][0]["value"], "2");
        let sum = client
            .request(
                "evaluate",
                json!({ "expression": "$sum + 100", "frameId": 1 }),
            )
            .await;
        assert_eq!(sum["result"], "100");
        // A variable of the caller's frame, read in that frame.
        let cart = client
            .request(
                "evaluate",
                json!({ "expression": "$cart[\"pear\"]", "frameId": 2 }),
            )
            .await;
        assert_eq!(cart["result"], "3");

        client.request("next", json!({ "threadId": 1 })).await;
        assert_eq!(client.event("stopped").await["body"]["reason"], "step");
        // Out of the loop (its breakpoint taken off), on to the condition.
        client
            .request("setBreakpoints", json!({ "source": source, "breakpoints": [{ "line": 11, "condition": "$t == 5" }] }))
            .await;
        client.request("continue", json!({ "threadId": 1 })).await;
        client.event("stopped").await;
        let stack = client.request("stackTrace", json!({ "threadId": 1 })).await;
        assert_eq!(stack["stackFrames"][0]["line"], 11);
        client.request("continue", json!({ "threadId": 1 })).await;
        let exited = client.event("exited").await;
        assert_eq!(exited["body"]["exitCode"], 0);
        client.event("terminated").await;
        // The program's output may trail its end a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !client.output().contains("total 5") && std::time::Instant::now() < deadline {
            client.gather(Duration::from_millis(500)).await;
        }
        assert!(client.output().contains("total 5"), "{}", client.output());
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }
}
