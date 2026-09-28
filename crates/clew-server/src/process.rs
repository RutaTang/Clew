//! Subprocesses spawned for the client (language servers, debug adapters):
//! registration ahead of the spawn, the stdin writer, the stdout pump, the
//! stderr drain, and exit reporting.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clew_protocol::{Event, ServerMessage};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;

use crate::transport::OutputBudget;

/// A subprocess spawned for the client (a language server / debug adapter):
/// the channel feeding its stdin-writer task, and the child handle to keep
/// alive and later kill. Stdin is written by a dedicated task so a child that
/// stops reading (full pipe) can never block the request loop — `ProcessKill`
/// must always be reachable, most of all for exactly such a process.
///
/// The entry is registered *at the spawn request*, before the OS process
/// exists (`child: None` until then): the client pipelines protocol traffic
/// (an LSP `initialize`) right behind its spawn request, and those frames
/// must queue for the child rather than race its registration.
pub(crate) struct Proc {
    pub(crate) input: tokio::sync::mpsc::Sender<Vec<u8>>,
    pub(crate) child: Option<tokio::process::Child>,
    /// Which registration this entry is. `proc` is chosen by the CLIENT, so
    /// the same handle can be registered twice; without this, the first
    /// process's stdout reader would deregister the second on exit and report
    /// the live one dead.
    pub(crate) generation: u64,
}

/// Stdin backlog per process (messages, each ≤ [`MAX_PROC_INPUT_BYTES`]). A
/// child that stopped reading hits this quickly. Overflow is not survivable
/// for the stream — losing one chunk desyncs `Content-Length` framing forever
/// — so a full queue kills the process and reports it instead of dropping
/// bytes or queueing without bound until the OOM killer picks the server.
/// With the chunk cap below, at most 16 MiB wait per process.
pub(crate) const PROC_INPUT_QUEUE: usize = 256;

/// Cap on ONE `ProcessInput` message: the protocol's chunk size. The queue
/// above bounds how many messages can be outstanding, not how big each is —
/// and a client frame may be up to the protocol's 256 MB, so the two limits
/// multiplied to something no machine can hold. The client cuts its stream
/// into chunks of at most this size, so a bigger one is a protocol violation:
/// refused, and — like an overflow — fatal for the process, whose stream now
/// lacks those bytes.
pub(crate) const MAX_PROC_INPUT_BYTES: usize = clew_protocol::MAX_PROCESS_CHUNK;

pub(crate) type SharedProcs = Arc<tokio::sync::Mutex<HashMap<u64, Proc>>>;

/// How long a process being stopped gets to leave by itself before it is
/// killed. Its stdin closes as its entry goes (the writer task's queue
/// ends), which is how most language servers and debug adapters are told
/// to exit.
const KILL_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(2)
};

/// End `child` in the background: `grace` to exit by itself, then SIGKILL to
/// its whole process group — it leads one of its own (see
/// [`spawn_registered`]), so the helpers it started go with it — then reap.
/// The sequence is the LSP/DAP clients' ([`clew_core::framing::reap`]),
/// cancel-safety included: a runtime that shuts down mid-grace (a
/// disconnect) kills the group on the spot.
fn reap_in_background(child: tokio::process::Child, grace: Duration) {
    tokio::spawn(clew_core::framing::reap(child, grace, cfg!(unix)));
}

/// Stop a process whose table entry the caller has just REMOVED (a
/// `ProcessKill`, a project switch, a stdin overflow, a disconnect) — the one
/// sequence all of those share.
///
/// Its stdout reader, when there is one, observes the kill and reports the
/// exit. A process still being spawned has no reader yet, so the exit is
/// reported here; the spawn task then finds the entry gone and reaps the
/// newborn itself.
pub(crate) fn kill_removed(out: &UnboundedSender<ServerMessage>, proc: u64, mut p: Proc) {
    match p.child.take() {
        Some(child) => reap_in_background(child, KILL_GRACE),
        None => {
            let _ = out.send(ServerMessage::Notification {
                event: Event::ProcessExited { proc, code: None },
            });
        }
    }
}

/// Lines of a process's stderr kept for its exit report.
const STDERR_TAIL_LINES: usize = 20;
/// Longest stderr line kept (or logged); the rest of a longer one becomes
/// the next line, so memory stays bounded whatever the process writes.
const STDERR_LINE_BYTES: usize = 2048;
/// How much of the tail an exit report quotes.
const EXIT_REPORT_CHARS: usize = 400;

type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// Drain a process's stderr until it closes: each line goes to this server's
/// own log (the client's stderr for a local server, ssh's for a remote one),
/// and the last [`STDERR_TAIL_LINES`] are kept for the exit report.
///
/// Draining is not optional once the pipe exists: a process that fills an
/// unread stderr pipe blocks on its next write. It used to be discarded
/// (`Stdio::null`), so a language server that died on startup — a missing
/// toolchain, a bad config — died in silence.
async fn drain_stderr(stderr: tokio::process::ChildStderr, label: String, tail: StderrTail) {
    let mut reader = tokio::io::BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match (&mut reader)
            .take(STDERR_LINE_BYTES as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf).trim_end().to_string();
        if line.is_empty() {
            continue;
        }
        eprintln!("[clew-server] {label}: {line}");
        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }
}

/// The one-line report of a process that ended on its own and not cleanly —
/// nonzero, or on a signal — quoting the end of what it wrote to stderr.
fn exit_report(label: &str, code: Option<i32>, tail: &StderrTail) -> String {
    let how = match code {
        Some(code) => format!("exited with code {code}"),
        None => "was terminated by a signal".to_string(),
    };
    let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
    if tail.is_empty() {
        return format!("{label} {how}");
    }
    // The last lines are the likeliest to say why.
    let joined = tail.iter().cloned().collect::<Vec<_>>().join(" | ");
    let skip = joined.chars().count().saturating_sub(EXIT_REPORT_CHARS);
    let quoted: String = joined.chars().skip(skip).collect();
    let ellipsis = if skip > 0 { "…" } else { "" };
    format!("{label} {how}: {ellipsis}{quoted}")
}

/// Register the stdin queue for `proc` in the table, ahead of the actual
/// spawn. From this moment `ProcessInput` frames buffer in the queue; once
/// the OS process exists, [`spawn_registered`] wires the queue to its stdin
/// and every buffered byte drains in order. This is what makes a client's
/// pipelined `spawn; write` correct even though the spawn itself runs on a
/// detached task.
pub(crate) async fn register_proc(
    procs: &SharedProcs,
    proc: u64,
) -> (tokio::sync::mpsc::Receiver<Vec<u8>>, u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    let (input, input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PROC_INPUT_QUEUE);
    // A re-registered handle replaces the old entry, whose process is then
    // stopped like any other (its group with it). The generation is what
    // stops that process's reader from deregistering this new entry.
    let replaced = procs.lock().await.insert(
        proc,
        Proc {
            input,
            child: None,
            generation,
        },
    );
    if let Some(child) = replaced.and_then(|old| old.child) {
        reap_in_background(child, KILL_GRACE);
    }
    (input_rx, generation)
}

/// Retire generation `generation` of handle `proc`, reporting whether a NEWER
/// registration has taken the handle over.
///
/// Our own entry is removed (so naturally-exited processes do not accumulate).
/// An entry already gone — removed by a `ProcessKill` or a project switch — is
/// not superseded: those paths rely on the reader to send the exit. Only a
/// live entry from a later registration is, and reporting an exit for it would
/// deregister a running process.
pub(crate) async fn superseded(procs: &SharedProcs, proc: u64, generation: u64) -> bool {
    retire(procs, proc, generation).await == Retired::Superseded
}

/// What [`retire`] found under a handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Retired {
    /// Our own entry, now removed: nobody stopped the process — it ended on
    /// its own.
    Own,
    /// Already removed by whoever stopped it (a kill of some kind).
    Gone,
    /// A newer registration owns the handle; it must not be touched.
    Superseded,
}

async fn retire(procs: &SharedProcs, proc: u64, generation: u64) -> Retired {
    let mut table = procs.lock().await;
    match table.get(&proc) {
        Some(p) if p.generation == generation => {
            table.remove(&proc);
            Retired::Own
        }
        Some(_) => Retired::Superseded,
        None => Retired::Gone,
    }
}

/// How long the stdout pump keeps reading after the child has exited, so its
/// last frames still reach the client. Bounded, because a descendant that
/// inherited the pipe can hold it open for as long as it likes.
pub(crate) const FINAL_DRAIN: Duration = Duration::from_millis(250);

/// Wait for the process behind `proc` to actually exit, returning its exit
/// code (`None` when it was signalled, the handle is gone, or a newer
/// registration took the id over).
///
/// A process that exits by ITSELF takes its group with it: what it started
/// (a language server's `cargo check`, an adapter's debuggee) is killed
/// before the leader is reaped ([`clew_core::framing::try_wait_sweeping`]),
/// as it is when clew does the stopping (`kill_removed`). Only a stop swept
/// the group before, so a crashed server left its helpers running.
///
/// Polled rather than awaited on the `Child` directly: the handle has to stay
/// in the table so a concurrent `ProcessKill` can still reach it, and holding
/// the table lock across an await would stall every other process operation.
/// The interval backs off, so a child that closed stdout and then ran for an
/// hour costs a handful of wakeups rather than one per tick.
pub(crate) async fn wait_for_exit(procs: &SharedProcs, proc: u64, generation: u64) -> Option<i32> {
    const FIRST_POLL: Duration = Duration::from_millis(20);
    const MAX_POLL: Duration = Duration::from_secs(2);
    let mut delay = FIRST_POLL;
    loop {
        {
            let mut table = procs.lock().await;
            // Entry gone (killed) or superseded: the caller handles both, and
            // there is no longer a handle here to wait on.
            let p = table
                .get_mut(&proc)
                .filter(|p| p.generation == generation)?;
            match p.child.as_mut().map(clew_core::framing::try_wait_sweeping) {
                Some(Ok(Some(status))) => return status.code(),
                // Still running — fall through to the sleep.
                Some(Ok(None)) => {}
                // No handle yet, or waiting failed: nothing to learn by
                // looping.
                Some(Err(_)) | None => return None,
            }
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_POLL);
    }
}

/// What became of a [`spawn_registered`] attempt.
///
/// Cancellation and success used to share one value (`None`), so a spawn the
/// client had already killed was reported to it as a running adapter — the
/// client then drove a DAP handshake against a process that did not exist.
///
/// (An `Event` is a couple of hundred bytes; boxing it here would add an
/// allocation to a value built once per spawn and matched at once, to shrink
/// something that is never stored.)
#[allow(clippy::large_enum_variant)]
pub(crate) enum Spawned {
    /// The process is running; its stdio is being proxied.
    Started,
    /// The client killed the handle (or its input queue overflowed) while the
    /// spawn was in flight. The remover already sent `ProcessExited`, so the
    /// caller must report nothing.
    Cancelled,
    /// The spawn failed; the table entry is gone and this is the error to
    /// report to the caller.
    Failed(Event),
}

/// Spawn `cmd` (in `cwd` when given) and proxy its stdio to the client under
/// handle `proc`, whose stdin queue was set up by [`register_proc`]: stdout
/// streams back as `ProcessOutput`, stdin drains `input_rx` (frames fed by
/// `ProcessInput`, possibly queued since before the spawn).
///
/// Emits exactly one of `ProcessStarted` or `ProcessExited` — except on
/// [`Spawned::Cancelled`], where whoever removed the table entry has already
/// sent the exit.
#[allow(clippy::too_many_arguments)] // the spawn's full contract, not state
pub(crate) async fn spawn_registered(
    out: &UnboundedSender<ServerMessage>,
    procs: &SharedProcs,
    budget: Arc<OutputBudget>,
    proc: u64,
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    mut input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    generation: u64,
) -> Spawned {
    let mut command = tokio::process::Command::new(&cmd);
    command
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // The leader of a process group of its own, so a stop reaches what it
    // starts: rust-analyzer's `cargo check`, an adapter's debuggee. Killing
    // the leader alone left those running for as long as they cared to.
    #[cfg(unix)]
    command.process_group(0);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let spawned = command.spawn().and_then(|mut child| {
        match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => Ok((child, stdin, stdout, stderr)),
            _ => Err(std::io::Error::other("stdio pipes missing")),
        }
    });
    // How the process is named in the log and in its exit report.
    let label = format!(
        "{} (process {proc})",
        std::path::Path::new(&cmd)
            .file_name()
            .map_or_else(|| cmd.clone(), |n| n.to_string_lossy().into_owned())
    );
    match spawned {
        Ok((child, mut stdin, mut stdout, stderr)) => {
            // Stdin writer: owns the pipe so a non-reading child blocks only
            // this task, never the request loop. Ends when the Proc is
            // dropped (kill/exit) or the child's pipe breaks. Bounded — see
            // [`PROC_INPUT_QUEUE`].
            tokio::spawn(async move {
                while let Some(data) = input_rx.recv().await {
                    if stdin.write_all(&data).await.is_err() || stdin.flush().await.is_err() {
                        break;
                    }
                }
            });
            // Attach the child to OUR pre-registered entry. A missing entry
            // means the client killed the process (or its queue overflowed)
            // while the spawn was in flight — the remover already reported
            // the exit, so just reap the newborn quietly.
            //
            // The generation is what makes "our" load-bearing. `proc` is
            // chosen by the client, so a slow resolve can still be in flight
            // when the same id is registered again; attaching to whatever sat
            // under the id OVERWROTE the newer registration's `Child`, and
            // dropping that handle with `kill_on_drop` killed a running
            // process the client believed was healthy — after which
            // `wait_for_exit` polled the surviving child forever and no
            // `ProcessExited` was ever sent for the one that died.
            match procs
                .lock()
                .await
                .get_mut(&proc)
                .filter(|p| p.generation == generation)
            {
                Some(p) => p.child = Some(child),
                None => {
                    reap_in_background(child, Duration::ZERO);
                    return Spawned::Cancelled;
                }
            }
            let _ = out.send(ServerMessage::Notification {
                event: Event::ProcessStarted { proc },
            });
            let tail: StderrTail = Arc::default();
            let stderr_task = tokio::spawn(drain_stderr(stderr, label.clone(), tail.clone()));
            // Stdout reader, started only after the child is attached above:
            // its exit path removes the table entry, and a child that exits
            // instantly could otherwise run that removal first — leaving a
            // dead entry in the table forever.
            let out = out.clone();
            let procs_cleanup = procs.clone();
            tokio::spawn(async move {
                // One read is one `ProcessOutput`, so the buffer IS the
                // protocol's chunk cap.
                let mut buf = vec![0u8; clew_protocol::MAX_PROCESS_CHUNK];
                // Watch for the real exit CONCURRENTLY with the pump. Neither
                // event implies the other: stdout can close on a child that
                // keeps working, and a child can exit while a descendant it
                // spawned still holds the write end of the pipe open. Pumping
                // first and waiting afterwards handled the first case and hung
                // forever on the second — the client was never told the
                // process had ended, and the table entry never went away.
                //
                // Pinned and polled by reference, so the waiter keeps its
                // backoff instead of restarting (and re-locking the table) on
                // every chunk of output.
                let waiter = wait_for_exit(&procs_cleanup, proc, generation);
                tokio::pin!(waiter);
                let mut exit: Option<Option<i32>> = None;
                loop {
                    let n = if exit.is_none() {
                        tokio::select! {
                            // `read` is cancel-safe: losing the race means no
                            // bytes were taken from the pipe.
                            read = stdout.read(&mut buf) => match read {
                                Ok(0) | Err(_) => break,
                                Ok(n) => n,
                            },
                            code = &mut waiter => {
                                exit = Some(code);
                                continue;
                            }
                        }
                    } else {
                        // The child is gone. Let what it already wrote drain,
                        // but only briefly: a surviving descendant holding the
                        // pipe would otherwise keep this task, and the handle
                        // the client thinks is dead, alive indefinitely.
                        match tokio::time::timeout(FINAL_DRAIN, stdout.read(&mut buf)).await {
                            Ok(Ok(n)) if n > 0 => n,
                            _ => break,
                        }
                    };
                    // Charge the queued bytes against the shared budget
                    // first: while the transport is behind, this pump pauses
                    // (and the child's pipe fills) instead of the out queue
                    // growing without bound.
                    budget.charge(n).await;
                    let msg = ServerMessage::Notification {
                        event: Event::ProcessOutput {
                            proc,
                            data: buf[..n].to_vec(),
                        },
                    };
                    if out.send(msg).is_err() {
                        budget.release(n);
                        break;
                    }
                }
                let code = match exit {
                    Some(code) => code,
                    // Stdout closed first. That is NOT the same as the process
                    // exiting: a child may legitimately close its stdout and
                    // keep working. Treating EOF as the exit dropped the table
                    // entry, and the entry owns the `Child` with
                    // `kill_on_drop` — so a healthy long-running process was
                    // KILLED, and the client was told it had died on its own.
                    //
                    // The handle stays in the table throughout, so a
                    // `ProcessKill` arriving meanwhile still reaches the child.
                    None => waiter.await,
                };

                // Now drop the table entry, so naturally-exited processes
                // don't accumulate for the session's lifetime, and tell the
                // client.
                //
                // …unless a NEWER registration owns this handle. `proc` is
                // chosen by the client, so the same id can be registered
                // twice; removing it blindly would deregister the live
                // process and tell the client it had died.
                let retired = retire(&procs_cleanup, proc, generation).await;
                // A process nobody stopped that did not end cleanly is worth
                // a word: that is how a language server that cannot start
                // looks from here. A killed one (its entry already removed)
                // was stopped on purpose, and says nothing.
                if retired == Retired::Own && code != Some(0) {
                    // Let the drain take the last lines, briefly — a
                    // descendant may hold stderr open indefinitely.
                    let _ = tokio::time::timeout(FINAL_DRAIN, stderr_task).await;
                    let _ = out.send(ServerMessage::Notification {
                        event: Event::Status {
                            message: exit_report(&label, code, &tail),
                        },
                    });
                }
                if retired != Retired::Superseded {
                    let _ = out.send(ServerMessage::Notification {
                        event: Event::ProcessExited { proc, code },
                    });
                }
            });
            Spawned::Started
        }
        Err(e) => {
            // The process never existed: retract the pre-registered entry and
            // close the proxy (EOF for the client's driver) before reporting,
            // so no half-open stream or stale mapping outlives the failure.
            if !superseded(procs, proc, generation).await {
                let _ = out.send(ServerMessage::Notification {
                    event: Event::ProcessExited { proc, code: None },
                });
            }
            Spawned::Failed(crate::failed(format!("spawn {cmd}: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A fresh directory, unique per process and per call, removed on drop.
    fn scratch(tag: &str) -> crate::test_support::Scratch {
        crate::test_support::Scratch::new(&format!("server-proc-{tag}"))
    }

    /// Whether `pid` is gone within a few seconds. Probed through the shell's
    /// `kill -0`, by absolute path.
    async fn gone(pid: u32) -> bool {
        for _ in 0..300 {
            let alive = tokio::process::Command::new("/bin/sh")
                .args(["-c", "kill -0 \"$1\" 2>/dev/null", "sh", &pid.to_string()])
                .status()
                .await
                .is_ok_and(|status| status.success());
            if !alive {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    /// F4: a proxied process is the leader of its own group, and stopping it
    /// stops what it started — whether it leaves by itself once its stdin
    /// closes (the helper it leaves behind is swept), or ignores that and is
    /// killed at the end of the grace. Killing the leader alone left
    /// rust-analyzer's `cargo check` and friends running.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_killed_process_takes_its_group_with_it() {
        for script in [
            "sleep 30 & echo $! > \"$1\"; read -r line; exit 0",
            "sleep 30 & echo $! > \"$1\"; wait",
        ] {
            let dir = scratch("group");
            let pidfile = dir.join("helper.pid");
            let procs: SharedProcs = Arc::default();
            let (out, _events) = tokio::sync::mpsc::unbounded_channel();
            let (input_rx, generation) = register_proc(&procs, 7).await;
            let args = vec![
                "-c".to_string(),
                script.to_string(),
                "sh".to_string(),
                pidfile.to_string_lossy().into_owned(),
            ];
            let spawned = spawn_registered(
                &out,
                &procs,
                crate::transport::OutputBudget::new(),
                7,
                "/bin/sh".to_string(),
                args,
                None,
                input_rx,
                generation,
            )
            .await;
            assert!(matches!(spawned, Spawned::Started), "{script}");
            let helper = wait_for_pid(&pidfile).await;
            let entry = procs.lock().await.remove(&7).expect("registered");
            kill_removed(&out, 7, entry);
            let helper_gone = gone(helper).await;
            assert!(helper_gone, "the helper outlived its process: {script}");
        }
    }

    /// A proxied process that exits BY ITSELF — nobody killed it — takes
    /// its group with it too, before it is reaped: a crashed language server
    /// left its `cargo check` running. Its exit is still reported, with its
    /// own code, and its entry leaves the table.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_process_that_exits_on_its_own_takes_its_group_with_it() {
        let dir = scratch("self-exit");
        let pidfile = dir.join("helper.pid");
        let procs: SharedProcs = Arc::default();
        let (out, mut events) = tokio::sync::mpsc::unbounded_channel();
        let (input_rx, generation) = register_proc(&procs, 9).await;
        let script = "sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; exit 4";
        let args = vec![
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
            pidfile.to_string_lossy().into_owned(),
        ];
        let spawned = spawn_registered(
            &out,
            &procs,
            crate::transport::OutputBudget::new(),
            9,
            "/bin/sh".to_string(),
            args,
            None,
            input_rx,
            generation,
        )
        .await;
        assert!(matches!(spawned, Spawned::Started));
        let helper = wait_for_pid(&pidfile).await;
        let code = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match events.recv().await {
                    Some(ServerMessage::Notification {
                        event: Event::ProcessExited { proc: 9, code },
                    }) => return code,
                    Some(_) => {}
                    None => panic!("the stream ended before the exit"),
                }
            }
        })
        .await
        .expect("the exit is reported");
        assert_eq!(code, Some(4));
        assert!(!procs.lock().await.contains_key(&9), "the entry is retired");
        assert!(
            gone(helper).await,
            "the helper outlived a process that exited"
        );
    }

    async fn wait_for_pid(path: &Path) -> u32 {
        for _ in 0..500 {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the helper never started");
    }
}
