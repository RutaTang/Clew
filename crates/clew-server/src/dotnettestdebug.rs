//! `clew-server dotnet-test-debug --netcoredbg <netcoredbg>`: C#'s tests
//! debugged as VSTest has them debugged.
//!
//! `dotnet test` runs a project's tests in a test host of its own, a
//! process the debugger does not start. With `VSTEST_HOST_DEBUG` set, the
//! host says its process id (`Process Id: 6934, Name: dotnet`) and waits
//! for a debugger to attach — `VSTEST_DEBUG_NOBP` keeps it from breaking
//! into it once one has. This adapter speaks DAP to the client and has
//! netcoredbg do the debugging: the client's `launch` names the test
//! command, which runs, its output the session's; once the host waits,
//! the launch is netcoredbg's `attach` to it, and every other request and
//! answer passes through as it is. The session is the test run's: it ends
//! when `dotnet test` does, with its exit code — netcoredbg's own word
//! that the host ended is not passed on, the run's summary still to come.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use clew_core::framing::{read_message, write_frame};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::dapout::Out;

/// Run the adapter on this process's stdin/stdout.
pub async fn run(netcoredbg: PathBuf) -> Result<(), String> {
    serve(netcoredbg, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(netcoredbg: PathBuf, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let out = Out::new(output);
    let mut debugger = clew_core::spawn::tokio_command(&netcoredbg)
        .arg("--interpreter=vscode")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", netcoredbg.display()))?;
    let to_debugger = Arc::new(Mutex::new(
        debugger.stdin.take().ok_or("no stdin for netcoredbg")?,
    ));
    let from_debugger = debugger.stdout.take().ok_or("no stdout for netcoredbg")?;
    // The client's `launch`, answered by netcoredbg's `attach`: its seq.
    let launch_seq = Arc::new(AtomicI64::new(-1));
    // The test project's own assembly, once the launch names it.
    let own = Arc::new(std::sync::Mutex::new(None::<String>));
    tokio::spawn(relay(
        from_debugger,
        out.clone(),
        launch_seq.clone(),
        own.clone(),
    ));

    let mut tests: Option<Tests> = None;
    let mut input = BufReader::new(input);
    while let Ok(Some(message)) = read_message(&mut input).await {
        if message["type"] != "request" {
            continue;
        }
        match message["command"].as_str().unwrap_or("") {
            "launch" => {
                if let Ok(mut own) = own.lock() {
                    *own = own_assembly(&message["arguments"]);
                }
                match start_tests(&message["arguments"], &out).await {
                    Ok((running, pid)) => {
                        tests = Some(running);
                        launch_seq.store(message["seq"].as_i64().unwrap_or(0), Ordering::SeqCst);
                        let mut attach = message.clone();
                        attach["command"] = json!("attach");
                        attach["arguments"] = json!({ "processId": pid });
                        send(&to_debugger, &attach).await;
                    }
                    Err(why) => out.respond(&message, Err(why)).await,
                }
            }
            "disconnect" | "terminate" => {
                send(&to_debugger, &message).await;
                if let Some(mut running) = tests.take() {
                    running.stop().await;
                }
                break;
            }
            _ => send(&to_debugger, &message).await,
        }
    }
    if let Some(mut running) = tests.take() {
        running.stop().await;
    }
    let _ = debugger.kill().await;
    Ok(())
}

/// The test run: `dotnet test`, and how to stop it.
struct Tests {
    stop: Option<oneshot::Sender<()>>,
    ended: oneshot::Receiver<()>,
}

impl Tests {
    async fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), &mut self.ended).await;
    }
}

/// Start the launch's `command` in its `cwd`, the test host told to wait
/// for a debugger; its output is the session's as it comes. The process
/// id the host says it waits under, once it does.
async fn start_tests(args: &Value, out: &Out) -> Result<(Tests, i64), String> {
    let words: Vec<String> = args["command"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| w.as_str().map(str::to_string))
        .collect();
    let (program, rest) = words.split_first().ok_or("no test command to run")?;
    let cwd = args["cwd"].as_str().map(PathBuf::from);
    let mut command = clew_core::spawn::tokio_command(Path::new(program));
    command
        .args(rest)
        .env("VSTEST_HOST_DEBUG", "1")
        .env("VSTEST_DEBUG_NOBP", "1")
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program))?;
    let (pid_tx, mut pids) = mpsc::unbounded_channel::<i64>();
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(watch(
            Box::new(stdout),
            "stdout",
            out.clone(),
            pid_tx.clone(),
        ));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(watch(Box::new(stderr), "stderr", out.clone(), pid_tx));
    }
    let (stop, stopped) = oneshot::channel::<()>();
    let (ended_tx, ended) = oneshot::channel::<()>();
    let (exit_tx, mut exited) = tokio::sync::watch::channel(false);
    let end = out.clone();
    tokio::spawn(async move {
        let status = wait_or_kill(&mut child, stopped).await;
        let _ = exit_tx.send(true);
        end.terminated(Some(status)).await;
        let _ = ended_tx.send(());
    });
    // As long as building and starting the tests takes.
    let pid = tokio::select! {
        Some(pid) = pids.recv() => pid,
        _ = exited.wait_for(|e| *e) => {
            return Err("The tests ended before their host waited for the debugger. See their output.".to_string());
        }
    };
    Ok((
        Tests {
            stop: Some(stop),
            ended,
        },
        pid,
    ))
}

/// The child's exit code — once it exits, or once it is told to stop.
async fn wait_or_kill(child: &mut Child, stopped: oneshot::Receiver<()>) -> i32 {
    let status = tokio::select! {
        status = child.wait() => status.ok(),
        _ = stopped => {
            let _ = child.kill().await;
            child.wait().await.ok()
        }
    };
    status.and_then(|s| s.code()).unwrap_or(-1)
}

/// The test command's output, line by line, as the session's; the first
/// `Process Id: N` line's `N` sent to `pids` (the host says it twice).
async fn watch(
    stream: Box<dyn AsyncRead + Unpin + Send>,
    category: &'static str,
    out: Out,
    pids: mpsc::UnboundedSender<i64>,
) {
    let mut lines = BufReader::new(stream);
    let mut line = String::new();
    let mut said = false;
    loop {
        line.clear();
        match lines.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if let Some(pid) = waiting_host(&line) {
            if !said {
                said = true;
                let _ = pids.send(pid);
            }
            continue;
        }
        // VSTest's words to a person attaching by hand: Clew attaches.
        if is_host_debugging_notice(&line) {
            continue;
        }
        out.output(category, line.clone()).await;
    }
}

/// The process id in the host's `Process Id: 6934, Name: dotnet`.
fn waiting_host(line: &str) -> Option<i64> {
    let rest = line.trim().strip_prefix("Process Id:")?;
    rest.split(',').next()?.trim().parse().ok()
}

/// VSTest's lines about attaching a debugger by hand.
fn is_host_debugging_notice(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("Host debugging is enabled.") || line == "Waiting for debugger attach..."
}

/// The test project's assembly, by its project file's name in the test
/// command (`Shop.Tests.csproj`: `Shop.Tests`).
fn own_assembly(args: &Value) -> Option<String> {
    args["command"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .map(Path::new)
        .find(|w| {
            w.extension()
                .is_some_and(|e| e == "csproj" || e == "fsproj" || e == "vbproj")
        })?
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
}

/// netcoredbg's warning that an assembly is a Release build, about one of
/// the test framework's (xUnit's, the test host's), not the project's own:
/// nothing the reader can rebuild.
fn is_library_release_warning(text: &str, own: Option<&str>) -> bool {
    let Some(rest) = text
        .trim_start()
        .strip_prefix("You are debugging a Release build of ")
    else {
        return false;
    };
    own.is_some_and(|own| !rest.starts_with(&format!("{own}.dll")))
}

/// What netcoredbg says, passed to the client: its answer to the `attach`
/// in place of the `launch` it stands for, and the end of the host it
/// debugged not the session's (the test run's is).
async fn relay(
    from: tokio::process::ChildStdout,
    out: Out,
    launch_seq: Arc<AtomicI64>,
    own: Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut from = BufReader::new(from);
    while let Ok(Some(mut message)) = read_message(&mut from).await {
        match message["type"].as_str() {
            Some("event") if matches!(message["event"].as_str(), Some("exited" | "terminated")) => {
                continue;
            }
            Some("event")
                if message["event"] == "output"
                    && is_library_release_warning(
                        message["body"]["output"].as_str().unwrap_or(""),
                        own.lock().ok().and_then(|o| o.clone()).as_deref(),
                    ) =>
            {
                continue;
            }
            Some("response")
                if message["command"] == "attach"
                    && message["request_seq"].as_i64()
                        == Some(launch_seq.load(Ordering::SeqCst)) =>
            {
                message["command"] = json!("launch");
            }
            _ => {}
        }
        out.pass(message).await;
    }
}

async fn send(to: &Mutex<ChildStdin>, message: &Value) {
    let mut to = to.lock().await;
    let _ = write_frame(&mut *to, message).await;
    let _ = to.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The host's process id, from the line it says it under; VSTest's
    /// words to a person attaching by hand are not the run's output.
    #[test]
    fn the_waiting_host_is_found_by_its_line() {
        assert_eq!(waiting_host("Process Id: 6934, Name: dotnet\n"), Some(6934));
        assert_eq!(waiting_host("  Process Id: 12, Name: testhost"), Some(12));
        assert_eq!(waiting_host("Passed!  - Failed: 0"), None);
        assert!(is_host_debugging_notice(
            "Host debugging is enabled. Please attach debugger to testhost process to continue.\n"
        ));
        assert!(is_host_debugging_notice("Waiting for debugger attach...\n"));
        assert!(!is_host_debugging_notice(
            "Starting test execution, please wait..."
        ));
    }

    /// The project's own assembly, from its project file in the command;
    /// netcoredbg's Release warning kept for it, not for the framework's.
    #[test]
    fn a_release_warning_is_kept_for_the_projects_own_assembly() {
        let args =
            json!({ "command": ["dotnet", "test", "/p/Shop.Tests.csproj", "--filter", "x"] });
        assert_eq!(own_assembly(&args).as_deref(), Some("Shop.Tests"));
        let own = Some("Shop.Tests");
        assert!(is_library_release_warning(
            "You are debugging a Release build of xunit.execution.dotnet.dll. Using Just My Code…",
            own
        ));
        assert!(!is_library_release_warning(
            "You are debugging a Release build of Shop.Tests.dll. Using Just My Code…",
            own
        ));
        assert!(!is_library_release_warning("total: 5\n", own));
    }
}
