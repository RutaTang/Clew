//! Client-side transport to the clew-server.
//!
//! The client is a pure renderer: it never touches the backend logic directly,
//! it speaks `clew-protocol` to a clew-server process. This module is the
//! transport — it spawns the server and frames messages over its stdio. Today
//! the server is a local child process; the same code will drive an SSH session
//! to a remote host, because from here it is just another process whose stdin we
//! write requests to and whose stdout we read events from.

use std::process::Stdio;

use clew_protocol::{ClientMessage, ServerMessage};
use iced::futures::{SinkExt, Stream};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::Message;

/// Name of the backend binary, looked up next to the running clew executable
/// (both live in the same `target/<profile>/` dir) and then on `PATH`.
const SERVER_BIN: &str = "clew-server";

/// The subscription that spawns the clew-server and streams its events to the
/// client. On start it hands the client a request sender via
/// `Message::ServerConnected`, then pumps the server's stdout until it exits.
pub fn subscription() -> iced::Subscription<Message> {
    iced::Subscription::run(stream)
}

/// Locate the clew-server binary: prefer a sibling of the running executable
/// (the workspace builds both into the same directory), else fall back to
/// `PATH`.
fn server_bin_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(SERVER_BIN);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    std::path::PathBuf::from(SERVER_BIN)
}

/// Build the command that runs clew-server. Local by default; when `CLEW_SSH`
/// is set it runs the server on a remote host over SSH — the ssh process's stdio
/// *is* the remote server's stdio, so the protocol framing is identical and the
/// whole backend runs where the code lives. `CLEW_SSH` holds the ssh arguments
/// ending in the remote clew-server path, e.g.
/// `-p 2222 -i ~/.ssh/id root@host /path/clew-server`.
fn server_command() -> (tokio::process::Command, String) {
    if let Ok(ssh) = std::env::var("CLEW_SSH") {
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args(ssh.split_whitespace());
        (cmd, format!("ssh {ssh}"))
    } else {
        let bin = server_bin_path();
        let label = bin.display().to_string();
        (tokio::process::Command::new(bin), label)
    }
}

/// Plain `fn` (no captures) as `Subscription::run` requires.
fn stream() -> impl Stream<Item = Message> {
    iced::stream::channel(256, |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
        let (mut cmd, label) = server_command();
        let mut child = match cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Reap the server when this subscription future is dropped (app exit).
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                // No server: tell the client so it falls back to local work
                // (scanning, search, reads) instead of waiting forever.
                eprintln!("[clew] could not spawn clew-server ({label}): {e}");
                let _ = output.send(Message::ServerUnavailable).await;
                return;
            }
        };
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        // Hand the client the request end; if the app is already gone, stop.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
        if output.send(Message::ServerConnected(tx)).await.is_err() {
            return;
        }

        // Writer: client requests -> server stdin, one NDJSON line each.
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let Ok(mut json) = serde_json::to_string(&msg) else {
                    continue;
                };
                json.push('\n');
                if stdin.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Reader: server stdout -> `ServerEvent` messages, one per NDJSON line.
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.is_empty() => {
                    if let Ok(msg) = serde_json::from_str::<ServerMessage>(&line) {
                        if output.send(Message::ServerEvent(msg)).await.is_err() {
                            break; // client gone
                        }
                    }
                }
                Ok(Some(_)) => {} // blank keep-alive line
                _ => break,       // EOF or read error: the server exited
            }
        }
        // Hold `child` to here so kill_on_drop reaps it when we stop.
        drop(child);
    })
}
