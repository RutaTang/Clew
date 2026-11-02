//! `clew-server haskell-debug --ghci <ghci>`: a debug adapter for Haskell,
//! over GHCi's own debugger.
//!
//! GHCi debugs interpreted code: `:break` sets a breakpoint on a module's
//! line, `:trace main` runs the program until one is reached (keeping an
//! evaluation history), and a stop reports where the program is and the
//! bindings in scope — `_` for a value not yet evaluated, which `:print`
//! shows without forcing it. No adapter speaks DAP for it without a patched
//! GHCi, so this one drives GHCi's own prompt: one command at a time, each
//! reply read up to a prompt of the adapter's own.
//!
//! The program is the launched file's `main`, with the launch's arguments
//! (`:set args`). Its source directories come from the project's `.cabal`
//! and `package.yaml` (`-i`); a project whose dependencies GHCi alone cannot
//! see runs under the command `ghci` names instead — `cabal repl`, `stack
//! repl`. Haskell has no call stack to show: the frames are where the program
//! stopped, then the evaluation history behind it. `next` is `:steplocal`,
//! `stepIn` is `:step`; GHCi cannot step out, nor pause a running program,
//! and says so.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clew_core::framing::read_message;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, oneshot, watch};

use crate::dapout::{Out, forward};

/// GHCi's prompt while the adapter drives it: what ends each reply.
const PROMPT: &str = "<<clew-ghci-ready>>";

/// How long GHCi has to start and load the program.
const LOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// How long a command that runs no program code may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// How long GHCi has to exit once its session is over.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Most bytes of one reply kept: a program printing without end between two
/// prompts must not hold the adapter's memory.
const MAX_REPLY: usize = 8 * 1024 * 1024;

/// Run the adapter on this process's stdin/stdout. `ghci` is the command
/// that starts GHCi, its arguments included (`ghci`, `cabal repl`).
pub async fn run(ghci: Vec<String>) -> Result<(), String> {
    serve(ghci, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(ghci: Vec<String>, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let mut adapter = Adapter {
        ghci,
        out: Out::new(output),
        session: None,
        process: None,
        breakpoints: HashMap::new(),
        stop: Arc::new(Mutex::new(None)),
        args: Vec::new(),
        cwd: PathBuf::new(),
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

/// Where the program stopped: the binding being evaluated, its source
/// span, and the bindings GHCi listed there.
#[derive(Debug, Clone, PartialEq)]
struct Stop {
    name: String,
    path: String,
    line: u64,
    column: u64,
    bindings: Vec<Binding>,
}

/// One `name :: Type = value` GHCi listed.
#[derive(Debug, Clone, PartialEq)]
struct Binding {
    name: String,
    kind: String,
    value: String,
}

/// GHCi itself: its exit code once it has one, and how to stop it.
struct Process {
    exited: watch::Receiver<Option<i32>>,
    kill: Option<oneshot::Sender<()>>,
}

struct Adapter {
    ghci: Vec<String>,
    out: Out,
    session: Option<Arc<Mutex<Ghci>>>,
    process: Option<Process>,
    /// Each file's breakpoints: GHCi's numbers for them.
    breakpoints: HashMap<String, Vec<String>>,
    /// Where the program is stopped, if it is.
    stop: Arc<Mutex<Option<Stop>>>,
    args: Vec<String>,
    /// GHCi's working directory: what the paths it names are relative to.
    cwd: PathBuf,
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
            "configurationDone" => {
                self.out.respond(request, Ok(json!({}))).await;
                self.run_on(":trace main", "breakpoint").await;
                return true;
            }
            "threads" => Ok(json!({ "threads": [{ "id": 1, "name": "main" }] })),
            "continue" | "next" | "stepIn" => {
                let (ghci, reason) = match command {
                    "continue" => (":trace", "breakpoint"),
                    "next" => (":steplocal", "step"),
                    _ => (":step", "step"),
                };
                self.out
                    .respond(request, Ok(json!({ "allThreadsContinued": true })))
                    .await;
                self.run_on(ghci, reason).await;
                return true;
            }
            "stepOut" => Err("GHCi cannot step out of a binding — continue instead".into()),
            "pause" => Err("GHCi cannot pause a running program".into()),
            "stackTrace" => self.stack_trace().await,
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

    /// Start GHCi, have it load the program with the project's source
    /// directories, and set its arguments.
    async fn launch(&mut self, args: &Value) -> Result<(), String> {
        let program = args["program"]
            .as_str()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .ok_or("no program to run")?;
        let cwd = args["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| program.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        self.cwd = cwd.clone();
        self.args = args["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // A launch configuration may name another GHCi (`cabal repl`).
        let ghci: Vec<String> = match args["ghci"].as_str().filter(|g| !g.trim().is_empty()) {
            Some(command) => command.split_whitespace().map(str::to_string).collect(),
            None => self.ghci.clone(),
        };
        let (program_name, ghci_args) = ghci.split_first().ok_or("no GHCi to run")?;
        let mut command = tokio::process::Command::new(program_name);
        command
            .args(ghci_args)
            .arg("-ignore-dot-ghci")
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run {program_name}: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(forward(Box::new(stderr), "stderr", self.out.clone()));
        }
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
        self.process = Some(Process {
            exited,
            kill: Some(kill),
        });
        let mut ghci = Ghci {
            stdin,
            stdout: BufReader::new(stdout),
        };
        // Our own prompt, and stops on uncaught exceptions.
        ghci.write(&format!(
            ":set prompt \"{PROMPT}\"\n:set prompt-cont \"\"\n:set -fbreak-on-error\n"
        ))
        .await?;
        for _ in 0..3 {
            ghci.reply(LOAD_TIMEOUT).await?;
        }
        let dirs = source_dirs(&cwd);
        if !dirs.is_empty() {
            let list = dirs
                .iter()
                .map(|d| d.to_string_lossy())
                .collect::<Vec<_>>()
                .join(":");
            ghci.command(&format!(":set -i{list}"), COMMAND_TIMEOUT)
                .await?;
        }
        let loaded = ghci
            .command(
                &format!(":load {}", program.to_string_lossy()),
                LOAD_TIMEOUT,
            )
            .await?;
        if !loaded.trim().is_empty() {
            self.out
                .output("console", format!("{}\n", loaded.trim_end()))
                .await;
        }
        if loaded.contains("Failed,") {
            return Err("the program did not compile — see the output".into());
        }
        let quoted: Vec<String> = self.args.iter().map(|a| haskell_string(a)).collect();
        // `:set args` takes them as Haskell would read a command line.
        ghci.command(&format!(":set args {}", quoted.join(" ")), COMMAND_TIMEOUT)
            .await?;
        self.session = Some(Arc::new(Mutex::new(ghci)));
        Ok(())
    }

    fn session(&self) -> Result<Arc<Mutex<Ghci>>, String> {
        self.session
            .clone()
            .ok_or_else(|| "no program is running".to_string())
    }

    /// Replace one file's breakpoints.
    async fn set_breakpoints(&mut self, args: &Value) -> Result<Value, String> {
        let path = args["source"]["path"]
            .as_str()
            .ok_or("a breakpoint needs its file's path")?
            .to_string();
        let lines: Vec<u64> = args["breakpoints"]
            .as_array()
            .map(|bps| bps.iter().filter_map(|bp| bp["line"].as_u64()).collect())
            .unwrap_or_default();
        let session = match self.session() {
            Ok(session) => session,
            Err(_) => {
                let unverified: Vec<Value> = lines
                    .iter()
                    .map(|line| json!({ "verified": false, "line": line, "message": "no program is running" }))
                    .collect();
                return Ok(json!({ "breakpoints": unverified }));
            }
        };
        let module = std::fs::read_to_string(&path)
            .map(|source| module_name(&source))
            .unwrap_or_else(|_| "Main".into());
        let mut ghci = session.lock().await;
        for number in self.breakpoints.remove(&path).unwrap_or_default() {
            let _ = ghci
                .command(&format!(":delete {number}"), COMMAND_TIMEOUT)
                .await;
        }
        let mut numbers = Vec::new();
        let mut answers = Vec::new();
        for line in lines {
            let said = ghci
                .command(&format!(":break {module} {line}"), COMMAND_TIMEOUT)
                .await?;
            match parse_activated(&said) {
                Some((number, at)) => {
                    numbers.push(number);
                    answers.push(json!({ "verified": true, "line": at }));
                }
                None => answers.push(json!({
                    "verified": false, "line": line,
                    "message": said.trim().lines().last().unwrap_or("not set"),
                })),
            }
        }
        self.breakpoints.insert(path, numbers);
        Ok(json!({ "breakpoints": answers }))
    }

    /// Run the program on with `command` in the background: where it stops
    /// — or that it ended — arrives as an event once it does.
    async fn run_on(&self, command: &'static str, reason: &'static str) {
        let Ok(session) = self.session() else {
            return;
        };
        let out = self.out.clone();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            let mut ghci = session.lock().await;
            // No timeout: the program runs as long as it runs.
            let reply = ghci.command(command, Duration::MAX).await;
            let Ok(reply) = reply else {
                *stop.lock().await = None;
                out.terminated(None).await;
                return;
            };
            let (printed, stopped) = parse_stop(&reply);
            if !printed.is_empty() {
                out.output("stdout", printed).await;
            }
            match stopped {
                Some(at) => {
                    let exception = at.name == "<exception thrown>";
                    *stop.lock().await = Some(at);
                    let reason = if exception { "exception" } else { reason };
                    out.event(
                        "stopped",
                        json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true }),
                    )
                    .await;
                }
                None => {
                    *stop.lock().await = None;
                    out.terminated(Some(0)).await;
                }
            }
        });
    }

    /// Where the program stopped, then its evaluation history.
    async fn stack_trace(&self) -> Result<Value, String> {
        let Some(at) = self.stop.lock().await.clone() else {
            return Ok(json!({ "totalFrames": 0, "stackFrames": [] }));
        };
        let mut frames = vec![frame(
            1,
            &at.name,
            &self.absolute(&at.path),
            at.line,
            at.column,
        )];
        let session = self.session()?;
        let history = session
            .lock()
            .await
            .command(":history", COMMAND_TIMEOUT)
            .await?;
        for (i, (name, path, line, column)) in parse_history(&history).into_iter().enumerate() {
            frames.push(frame(
                i as u64 + 2,
                &format!("{name} (earlier)"),
                &self.absolute(&path),
                line,
                column,
            ));
        }
        Ok(json!({ "totalFrames": frames.len(), "stackFrames": frames }))
    }

    async fn scopes(&self, args: &Value) -> Result<Value, String> {
        // Only where the program is stopped has bindings to show.
        let reference = match args["frameId"].as_u64() {
            Some(1) => 1,
            _ => 0,
        };
        Ok(
            json!({ "scopes": [{ "name": "Bindings", "variablesReference": reference, "expensive": false }] }),
        )
    }

    async fn variables(&self, args: &Value) -> Result<Value, String> {
        if args["variablesReference"].as_u64() != Some(1) {
            return Ok(json!({ "variables": [] }));
        }
        let Some(at) = self.stop.lock().await.clone() else {
            return Ok(json!({ "variables": [] }));
        };
        let variables: Vec<Value> = at
            .bindings
            .iter()
            .map(|b| {
                json!({
                    "name": b.name,
                    "value": shown_value(&b.value),
                    "type": b.kind,
                    "variablesReference": 0,
                })
            })
            .collect();
        Ok(json!({ "variables": variables }))
    }

    /// A name is shown as GHCi prints it, unforced (`:print`); anything else
    /// is evaluated — forcing what it needs.
    async fn evaluate(&self, args: &Value) -> Result<Value, String> {
        let expression = args["expression"].as_str().unwrap_or("").trim().to_string();
        if expression.is_empty() || expression.contains('\n') {
            return Err("one line to evaluate".into());
        }
        let session = self.session()?;
        let command = match is_identifier(&expression) {
            true => format!(":print {expression}"),
            false => expression.clone(),
        };
        let reply = session
            .lock()
            .await
            .command(&command, COMMAND_TIMEOUT)
            .await?;
        let text = strip_location(&reply);
        if text.contains("error:") {
            return Err(text);
        }
        // `:print x` says `x = value`.
        let value = match text.split_once(" = ") {
            Some((name, value)) if name.trim() == expression => value.to_string(),
            _ => text,
        };
        Ok(json!({ "result": shown_value(&value), "variablesReference": 0 }))
    }

    /// A path GHCi named, made absolute against its working directory.
    fn absolute(&self, path: &str) -> String {
        match Path::new(path).is_absolute() {
            true => path.to_string(),
            false => self.cwd.join(path).to_string_lossy().into_owned(),
        }
    }

    /// The session is over: stop GHCi.
    async fn end(&mut self) {
        if let Some(session) = self.session.take()
            && let Ok(mut ghci) = session.try_lock()
        {
            let _ = ghci.write(":quit\n").await;
        }
        if let Some(process) = &mut self.process {
            let mut exited = process.exited.clone();
            let lingered = tokio::time::timeout(EXIT_GRACE, exited.wait_for(Option::is_some))
                .await
                .is_err();
            if lingered && let Some(kill) = process.kill.take() {
                let _ = kill.send(());
            }
            if !self.out.has_terminated() {
                self.out.terminated(None).await;
            }
        }
    }
}

/// GHCi, driven at its prompt.
struct Ghci {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Ghci {
    async fn write(&mut self, text: &str) -> Result<(), String> {
        self.stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|e| format!("GHCi is gone: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("GHCi is gone: {e}"))
    }

    /// Run `command`; what GHCi said, up to its next prompt.
    async fn command(&mut self, command: &str, limit: Duration) -> Result<String, String> {
        self.write(&format!("{command}\n")).await?;
        self.reply(limit).await
    }

    /// What GHCi says up to its next prompt.
    async fn reply(&mut self, limit: Duration) -> Result<String, String> {
        let read = async {
            let mut said: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let n = self
                    .stdout
                    .read(&mut byte)
                    .await
                    .map_err(|e| format!("GHCi is gone: {e}"))?;
                if n == 0 {
                    return Err("GHCi ended".to_string());
                }
                if said.len() < MAX_REPLY {
                    said.push(byte[0]);
                }
                if said.ends_with(PROMPT.as_bytes()) {
                    said.truncate(said.len() - PROMPT.len());
                    return Ok(String::from_utf8_lossy(&said).into_owned());
                }
            }
        };
        match limit {
            Duration::MAX => read.await,
            limit => tokio::time::timeout(limit, read)
                .await
                .map_err(|_| "GHCi did not answer in time".to_string())?,
        }
    }
}

fn frame(id: u64, name: &str, path: &str, line: u64, column: u64) -> Value {
    let file = Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    json!({
        "id": id,
        "name": name,
        "source": { "path": path, "name": file },
        "line": line,
        "column": column,
    })
}

/// A value as GHCi shows it, an unevaluated one (`_`) said so.
fn shown_value(value: &str) -> String {
    match value.trim() {
        "_" => "_ (not evaluated yet)".to_string(),
        other => other.to_string(),
    }
}

/// What a run printed, and where it stopped — if it did: GHCi says
/// `Stopped in <name>, <file>:<span>`, lists the bindings in scope as
/// `name :: Type = value`, and ends with the span in brackets. The program's
/// own output comes before, a line it had not finished included.
fn parse_stop(reply: &str) -> (String, Option<Stop>) {
    let Some(at) = reply.find("Stopped in ") else {
        return (
            strip_location(reply) + if reply.ends_with('\n') { "\n" } else { "" },
            None,
        );
    };
    let printed = reply[..at].to_string();
    let rest = &reply[at + "Stopped in ".len()..];
    let mut lines = rest.lines();
    let head = lines.next().unwrap_or("");
    let (name, location) = head.split_once(", ").unwrap_or((head, ""));
    let (path, line, column) = parse_span(location);
    let mut bindings: Vec<Binding> = Vec::new();
    for line in lines {
        if is_location(line) {
            break;
        }
        match parse_binding(line) {
            Some(binding) => bindings.push(binding),
            // A value printed over several lines.
            None => {
                if let Some(last) = bindings.last_mut() {
                    last.value.push('\n');
                    last.value.push_str(line);
                }
            }
        }
    }
    bindings.retain(|b| b.name != "_result");
    let stop = Stop {
        name: name.to_string(),
        path,
        line,
        column,
        bindings,
    };
    (printed, Some(stop))
}

/// `name :: Type = value`.
fn parse_binding(line: &str) -> Option<Binding> {
    let (name, rest) = line.split_once(" :: ")?;
    if name.is_empty() || name.contains(' ') {
        return None;
    }
    let (kind, value) = rest.split_once(" = ").unwrap_or((rest, "_"));
    Some(Binding {
        name: name.to_string(),
        kind: kind.to_string(),
        value: value.to_string(),
    })
}

/// `app/Main.hs:13:11-22` (or `:(7,3)-(8,9)`, a span over lines): the file,
/// line and column it starts at.
fn parse_span(span: &str) -> (String, u64, u64) {
    // The file may hold a colon; the span is what follows its last `.hs:`.
    let Some(at) = span
        .rfind(".hs:")
        .map(|i| i + 3)
        .or_else(|| span.rfind(".lhs:").map(|i| i + 4))
    else {
        return (span.to_string(), 0, 0);
    };
    let path = span[..at].to_string();
    let rest = &span[at + 1..];
    let numbers: Vec<u64> = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    let line = numbers.first().copied().unwrap_or(0);
    let column = numbers.get(1).copied().unwrap_or(1);
    (path, line, column)
}

/// `:history`'s entries (`-1  : Main.total (app/Main.hs:7:11-16)`), latest
/// first.
fn parse_history(history: &str) -> Vec<(String, String, u64, u64)> {
    history
        .lines()
        .filter_map(|line| {
            let (_, entry) = line.split_once(" : ")?;
            let (name, span) = entry.split_once(" (")?;
            let (path, line, column) = parse_span(span.trim_end_matches(')'));
            Some((name.trim().to_string(), path, line, column))
        })
        .collect()
}

/// `Breakpoint 0 activated at app/Main.hs:7:11-16`: its number and line.
fn parse_activated(said: &str) -> Option<(String, u64)> {
    let line = said.lines().find(|l| l.starts_with("Breakpoint "))?;
    let rest = line.strip_prefix("Breakpoint ")?;
    let (number, rest) = rest.split_once(' ')?;
    let (_, span) = rest.split_once(" at ")?;
    let (_, line, _) = parse_span(span);
    Some((number.to_string(), line))
}

/// A reply without the `[span]` line GHCi ends it with while stopped.
fn strip_location(reply: &str) -> String {
    reply
        .lines()
        .filter(|l| !is_location(l))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Whether `line` is the `[file:span]` GHCi prints before its prompt while
/// stopped.
fn is_location(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('[') && line.ends_with(']') && line.contains(".hs:")
}

/// The module a source file declares (`module A.B where`), `Main` for one
/// that declares none.
fn module_name(source: &str) -> String {
    for line in source.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '\''))
                .collect();
            if !name.is_empty() {
                return name;
            }
        }
    }
    "Main".to_string()
}

/// Whether `text` is one Haskell name (`xs`, `s'`, `Data.Map.empty`).
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '\'' | '.'))
}

/// A Haskell string literal for `text`, as `:set args` takes them.
fn haskell_string(text: &str) -> String {
    format!("{text:?}")
}

/// The project's Haskell source directories, as `-i` takes them: what its
/// `.cabal` and `package.yaml` files name, then the conventional ones there
/// are.
fn source_dirs(root: &Path) -> Vec<PathBuf> {
    let mut rels: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if clew_core::imports::is_haskell_manifest(&name) {
                rels.push(name);
            }
        }
    }
    rels.push("Main.hs".into());
    let manifests = clew_core::imports::source_manifests(root, &rels);
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (_, named) in &manifests.haskell {
        for dir in named {
            let dir = root.join(dir);
            if dir.is_dir() && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    for conventional in ["src", "app", "lib"] {
        let dir = root.join(conventional);
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dapout::testing::Client;

    #[test]
    fn ghcis_replies_are_read() {
        let reply = "total Stopped in Main.total.s, app/Main.hs:7:11-16\n_result :: Int = _\nxs :: [Int] = [2,3]\n[app/Main.hs:7:11-16] ";
        let (printed, stop) = parse_stop(reply);
        assert_eq!(printed, "total ");
        let stop = stop.unwrap();
        assert_eq!(
            (
                stop.name.as_str(),
                stop.path.as_str(),
                stop.line,
                stop.column
            ),
            ("Main.total.s", "app/Main.hs", 7, 11)
        );
        assert_eq!(
            stop.bindings,
            [Binding {
                name: "xs".into(),
                kind: "[Int]".into(),
                value: "[2,3]".into()
            }]
        );
        let (printed, stop) = parse_stop("10 [\"a\"]\n");
        assert_eq!((printed.as_str(), stop), ("10 [\"a\"]\n", None));
        assert_eq!(
            parse_span("src/A.hs:(7,3)-(8,9)"),
            ("src/A.hs".into(), 7, 3)
        );
        assert_eq!(
            parse_activated("Breakpoint 2 activated at app/Main.hs:13:11-22\n"),
            Some(("2".into(), 13))
        );
        assert_eq!(
            parse_activated("No breakpoints found at that location.\n"),
            None
        );
        assert_eq!(
            parse_history(
                "-1  : Main.total (app/Main.hs:7:11-16)\n-2  : Main.main (app/Main.hs:13:11-22)\n<end of history>\n"
            ),
            [
                ("Main.total".into(), "app/Main.hs".into(), 7, 11),
                ("Main.main".into(), "app/Main.hs".into(), 13, 11)
            ]
        );
        assert_eq!(
            module_name("{-# LANGUAGE CPP #-}\nmodule Data.Shop (total) where\n"),
            "Data.Shop"
        );
        assert_eq!(module_name("main = pure ()\n"), "Main");
        assert!(is_identifier("xs'") && is_identifier("Data.Map.empty"));
        assert!(!is_identifier("length xs") && !is_identifier("1 + 2"));
        assert_eq!(shown_value("_"), "_ (not evaluated yet)");
    }

    /// A Haskell program debugged for real, under GHCi: held at its
    /// breakpoints, where it stopped and its bindings read, names shown
    /// without forcing them and expressions evaluated, run on to its end with
    /// its output. Needs `ghci`.
    #[tokio::test]
    #[ignore = "needs GHC's ghci; run explicitly"]
    async fn a_haskell_program_is_debugged_under_ghci() {
        let dir = clew_core::testutil::TempDir::new("haskell-dbg-project");
        std::fs::create_dir_all(dir.join("app")).unwrap();
        let program = dir.join("app/Main.hs");
        std::fs::write(
            &program,
            "module Main where\n\nimport System.Environment (getArgs)\n\ntotal :: [Int] -> Int\ntotal xs =\n  let s = sum xs\n  in s * 2\n\nmain :: IO ()\nmain = do\n  args <- getArgs\n  let t = total [2, 3]\n  putStrLn (\"total \" ++ show t ++ \" \" ++ show args)\n",
        )
        .unwrap();
        let ghci = clew_core::lsp::store::find_on_path_unresolved("ghci").expect("ghci");
        let (mut client, input, output) = Client::pipes();
        let running = tokio::spawn(serve(
            vec![ghci.to_string_lossy().into_owned()],
            input,
            output,
        ));
        client
            .request("initialize", json!({ "adapterID": "clew" }))
            .await;
        let launch = client
            .send(
                "launch",
                json!({ "program": program.to_string_lossy(), "args": ["a b", "c"], "cwd": dir.to_string_lossy() }),
            )
            .await;
        let launched = client.answer(launch).await;
        assert_eq!(launched["success"], true, "{launched}: {:?}", client.seen);
        client.event("initialized").await;
        let source = json!({ "path": program.to_string_lossy() });
        let set = client
            .request("setBreakpoints", json!({ "source": source, "breakpoints": [{ "line": 7 }, { "line": 13 }, { "line": 2 }] }))
            .await;
        assert_eq!(set["breakpoints"][0]["verified"], true, "{set}");
        assert_eq!(set["breakpoints"][1]["verified"], true, "{set}");
        assert_eq!(
            set["breakpoints"][2]["verified"], false,
            "no code on line 2: {set}"
        );
        client.request("configurationDone", json!({})).await;

        // Lazily, `t` is wanted first (line 13), then `s` in `total` (7).
        let mut visited = Vec::new();
        let mut saw_xs = false;
        loop {
            let message = client.event_any(&["stopped", "terminated"]).await;
            if message["event"] == "terminated" {
                break;
            }
            let stack = client.request("stackTrace", json!({ "threadId": 1 })).await;
            let top = &stack["stackFrames"][0];
            assert_eq!(top["source"]["path"], program.to_string_lossy().as_ref());
            let line = top["line"].as_u64().unwrap();
            visited.push(line);
            if line == 7 {
                let scopes = client.request("scopes", json!({ "frameId": 1 })).await;
                let bindings = client
                    .request(
                        "variables",
                        json!({ "variablesReference": scopes["scopes"][0]["variablesReference"] }),
                    )
                    .await;
                let xs = bindings["variables"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["name"] == "xs")
                    .cloned()
                    .unwrap_or_else(|| panic!("xs in {bindings}"));
                assert_eq!(xs["value"], "[2,3]");
                assert_eq!(xs["type"], "[Int]");
                let shown = client
                    .request("evaluate", json!({ "expression": "xs", "frameId": 1 }))
                    .await;
                assert_eq!(shown["result"], "[2,3]");
                let sum = client
                    .request(
                        "evaluate",
                        json!({ "expression": "length xs + 100", "frameId": 1 }),
                    )
                    .await;
                assert_eq!(sum["result"], "102");
                saw_xs = true;
            }
            client.request("continue", json!({ "threadId": 1 })).await;
            assert!(visited.len() < 10, "{visited:?}");
        }
        assert!(visited.contains(&13) && visited.contains(&7), "{visited:?}");
        assert!(saw_xs);
        assert!(
            client.output().contains("total 10 [\"a b\",\"c\"]"),
            "{}",
            client.output()
        );
        client.request("disconnect", json!({})).await;
        assert_eq!(running.await.unwrap(), Ok(()));
    }
}
