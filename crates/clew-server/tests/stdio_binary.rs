//! The real `clew-server` binary over its real stdio — the transport every
//! client actually uses, which the in-process tests in `protocol.rs` skip:
//! newline framing, the handshake, typed and byte payloads decoded by serde on
//! the far side of a pipe, the fail-closed rules for a line that does not parse
//! or does not fit, and the `--version` line the SSH bootstrap probes for.
//!
//! Hermetic: each test gets its own data directory and project under the
//! system temp dir (removed at the end), the server is told it is local
//! whatever shell runs the tests, every wait is bounded, and no child outlives
//! its test.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use clew_core::testutil::TempDir;

use clew_protocol::{
    ClientMessage, ErrorCode, Event, MAX_FRAME_BYTES, PROTOCOL_VERSION, Request,
    SCHEMA_FINGERPRINT, ServerMessage,
};

/// Longest any single wait may take before the test fails instead of hanging.
const WAIT: Duration = Duration::from_secs(30);

/// One running `clew-server`: its stdin, every stdout frame decoded (a frame
/// that does not decode fails the test — the client would hang up on it), and
/// its stderr collected.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Every stdout line, decoded — or the decode error, which the test
    /// thread turns into a failure.
    frames: Receiver<Result<ServerMessage, String>>,
    stderr: std::thread::JoinHandle<String>,
    _data: TempDir,
}

impl Server {
    fn spawn() -> Server {
        let data = TempDir::new("data");
        let mut child = Command::new(env!("CARGO_BIN_EXE_clew-server"))
            // Launched as the app launches its local server: told it is
            // local, so it is one whatever shell runs the tests (an SSH
            // session sets SSH_CONNECTION; see SpawnPolicy).
            .arg("--local")
            // Never the developer's real trust store or language servers.
            .env("CLEW_DATA_DIR", data.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the clew-server binary runs");
        let stdout = child.stdout.take().unwrap();
        let (tx, frames) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                let msg = serde_json::from_str::<ServerMessage>(&line)
                    .map_err(|e| format!("{e}: {line}"));
                if tx.send(msg).is_err() {
                    return;
                }
            }
        });
        let mut stderr = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        let stdin = child.stdin.take();
        Server {
            child,
            stdin,
            frames,
            stderr,
            _data: data,
        }
    }

    /// Write raw bytes to the server's stdin. A server that already hung up
    /// makes this fail, which the callers that provoke exactly that ignore.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin.write_all(bytes)?;
        stdin.flush()
    }

    fn send(&mut self, id: u64, request: Request) {
        let mut line = serde_json::to_string(&ClientMessage { id, request }).unwrap();
        line.push('\n');
        self.write(line.as_bytes())
            .expect("the server reads its stdin");
    }

    /// The first frame `pick` accepts, skipping the rest.
    fn wait_for<T>(&self, what: &str, mut pick: impl FnMut(ServerMessage) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frames.recv_timeout(left) {
                Ok(Ok(msg)) => {
                    if let Some(found) = pick(msg) {
                        return found;
                    }
                }
                Ok(Err(e)) => panic!("the server sent a frame that does not decode: {e}"),
                Err(RecvTimeoutError::Timeout) => panic!("no {what} within {WAIT:?}"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("the server closed its stdout before sending {what}")
                }
            }
        }
    }

    /// The reply to request `id`.
    fn reply(&self, id: u64) -> Event {
        self.wait_for(&format!("the reply to request {id}"), |msg| match msg {
            ServerMessage::Reply { id: got, event } if got == id => Some(event),
            _ => None,
        })
    }

    fn handshake(&mut self) {
        self.send(
            1,
            Request::Hello {
                protocol: PROTOCOL_VERSION,
                fingerprint: SCHEMA_FINGERPRINT.into(),
            },
        );
        match self.reply(1) {
            Event::Ready {
                protocol,
                fingerprint,
            } => {
                assert_eq!(protocol, PROTOCOL_VERSION);
                assert_eq!(fingerprint, SCHEMA_FINGERPRINT);
            }
            other => panic!("the handshake was refused: {other:?}"),
        }
    }

    /// Wait (bounded) for the process to exit, then for its stdout to end.
    /// Returns the exit status and everything it wrote to stderr.
    fn wait_for_exit(mut self) -> (ExitStatus, String) {
        let began = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                began.elapsed() < WAIT,
                "the server is still running {WAIT:?} after it should have hung up"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        // Nothing but the frames already sent may follow, then EOF.
        let deadline = Instant::now() + WAIT;
        loop {
            match self
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("the server sent a frame that does not decode: {e}"),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("stdout stayed open after the exit"),
            }
        }
        drop(self.stdin.take());
        let stderr = std::mem::replace(&mut self.stderr, std::thread::spawn(String::new));
        (status, stderr.join().unwrap_or_default())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A throwaway project with one Rust file.
fn project() -> TempDir {
    let dir = TempDir::new("project");
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "/// Adds.\npub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    dir
}

/// The whole happy path through a real pipe: the handshake, the tree an
/// `OpenProject` answers with, a typed payload (`Stats`), and proxied bytes
/// both ways (base64 in `ProcessInput` and `ProcessOutput`) — then a clean exit
/// when the client closes its end.
#[test]
fn a_session_over_real_stdio() {
    let root = project();
    let mut server = Server::spawn();
    server.handshake();

    server.send(
        2,
        Request::OpenProject {
            root: root.path().to_string_lossy().into_owned(),
        },
    );
    match server.reply(2) {
        Event::Tree {
            root: tree_root,
            seq,
            files,
            truncated,
            ..
        } => {
            assert_eq!(tree_root, root.path().to_string_lossy());
            assert!(seq >= 1, "a scan is stamped: {seq}");
            assert_eq!(files, vec!["src/lib.rs".to_string()]);
            assert!(!truncated);
        }
        other => panic!("expected the Tree, got {other:?}"),
    }

    // A typed payload decodes on this side of the pipe as itself.
    server.send(3, Request::Stats);
    match server.reply(3) {
        Event::Stats { report, .. } => {
            assert!(report.langs.iter().any(|l| l.name == "Rust"), "{report:?}");
        }
        other => panic!("expected Stats, got {other:?}"),
    }

    // Bytes both ways, including ones JSON strings cannot hold raw.
    server.send(
        4,
        Request::SpawnProcess {
            proc: 7,
            cmd: "cat".into(),
            args: vec![],
            cwd: None,
        },
    );
    let payload: Vec<u8> = vec![0, 1, 2, b'\n', 0xff, b'"', b'\\', 0x7f];
    server.send(
        5,
        Request::ProcessInput {
            proc: 7,
            data: payload.clone(),
        },
    );
    let mut echoed = Vec::new();
    server.wait_for("the echoed bytes", |msg| match msg {
        ServerMessage::Notification {
            event: Event::ProcessOutput { proc: 7, data },
        } => {
            echoed.extend(data);
            (echoed.len() >= payload.len()).then_some(())
        }
        _ => None,
    });
    assert_eq!(echoed, payload);
    server.send(6, Request::ProcessKill { proc: 7 });
    server.wait_for("the proxied process's exit", |msg| match msg {
        ServerMessage::Notification {
            event: Event::ProcessExited { proc: 7, .. },
        } => Some(()),
        _ => None,
    });

    // The client goes away: the server stops, cleanly.
    drop(server.stdin.take());
    let (status, _) = server.wait_for_exit();
    assert!(status.success(), "a clean exit: {status:?}");
}

/// Before the handshake completes the real binary refuses — with the code a
/// client can act on — rather than answering half a protocol.
#[test]
fn requests_before_the_handshake_are_refused_with_a_code() {
    let mut server = Server::spawn();
    server.send(9, Request::Stats);
    match server.reply(9) {
        Event::Error { code, message } => {
            assert_eq!(code, ErrorCode::Handshake, "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    drop(server.stdin.take());
    let (status, _) = server.wait_for_exit();
    assert!(status.success());
}

/// A line that does not parse ends the connection: past it nothing on the
/// stream can be trusted to mean what it says, and skipping it would drop an
/// unknowable subset of requests.
#[test]
fn a_garbage_line_ends_the_connection() {
    let mut server = Server::spawn();
    server.handshake();
    server
        .write(b"this is not a protocol frame\n")
        .expect("the server reads the line");
    let (status, stderr) = server.wait_for_exit();
    assert!(status.success(), "an orderly exit: {status:?}");
    assert!(stderr.contains("unparseable frame"), "stderr: {stderr}");
}

/// A line longer than the frame cap ends the connection before the server
/// buffers any more of it: an over-cap line cannot be resynced past.
#[test]
fn an_over_cap_line_ends_the_connection() {
    let mut server = Server::spawn();
    server.handshake();
    // Streamed in chunks, with no newline: one "line" one byte over the cap.
    // The server stops reading at the cap and hangs up, so the tail of this
    // write may hit a closed pipe — that is the outcome under test.
    let chunk = vec![b'x'; 1024 * 1024];
    let mut left = MAX_FRAME_BYTES + 1;
    while left > 0 {
        let n = left.min(chunk.len());
        if server.write(&chunk[..n]).is_err() {
            break;
        }
        left -= n;
    }
    let (status, _) = server.wait_for_exit();
    assert!(status.success(), "an orderly exit: {status:?}");
}

/// `--version` prints exactly the line the SSH bootstrap compares a deployed
/// binary's output against (`clew_protocol::version_line`, which the client's
/// probe uses too) — so a matching build is found, and a stale one is not.
#[test]
fn version_prints_the_bootstrap_probe_line() {
    let out = Command::new(env!("CARGO_BIN_EXE_clew-server"))
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .expect("the clew-server binary runs");
    assert!(out.status.success(), "{:?}", out.status);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout, format!("{}\n", clew_protocol::version_line()));
    assert_eq!(
        stdout.trim_end(),
        format!("clew-server protocol {PROTOCOL_VERSION} fingerprint {SCHEMA_FINGERPRINT}")
    );
}
