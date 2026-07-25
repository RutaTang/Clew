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
use crate::connect::ConnTarget;

/// Name of the backend binary, looked up next to the running clew executable
/// (both live in the same `target/<profile>/` dir) and then on `PATH`.
const SERVER_BIN: &str = "clew-server";

/// The subscription that spawns the clew-server and streams its events to the
/// client. On start it hands the client a request sender via
/// `Message::ServerConnected`, then pumps the server's stdout until it exits.
///
/// Keyed on `target`: connecting to a different host (or back to local) changes
/// the subscription identity, so iced drops the old transport — killing its
/// server — and runs a fresh one for the new target. That single seam is how an
/// in-app "Connect" switches between local and remote.
pub fn subscription(target: ConnTarget) -> iced::Subscription<Message> {
    iced::Subscription::run_with(target, stream)
}

/// Locate the clew-server binary: prefer a sibling of the running executable
/// (the workspace builds both into the same directory), else fall back to
/// `PATH`.
fn server_bin_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidate = dir.join(SERVER_BIN);
        if candidate.exists() {
            return candidate;
        }
    }
    std::path::PathBuf::from(SERVER_BIN)
}

/// The remote path (relative to the login home) where clew installs and runs the
/// server. `~` is expanded by the remote login shell.
const REMOTE_SERVER: &str = "~/.clew/server/clew-server";

/// Run one command on the remote over SSH (plain shell, not clew-server),
/// returning its stdout.
async fn ssh_run(ssh_args: &[String], remote_cmd: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg(remote_cmd)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Bootstrap the remote: if clew-server isn't present + version-compatible,
/// detect the platform, find a matching local binary, and stream it over SSH to
/// the remote. Returns the remote path to run. This is the "no server yet" step:
/// the first SSH calls run plain shell to check and install, before any protocol.
async fn bootstrap_remote(ssh_args: &[String]) -> Result<String, String> {
    let want = format!("protocol {}", clew_protocol::PROTOCOL_VERSION);
    // Already installed and compatible? (--version is a plain-shell probe.)
    if let Ok(out) = ssh_run(ssh_args, &format!("{REMOTE_SERVER} --version 2>/dev/null")).await
        && out.contains(&want)
    {
        return Ok(REMOTE_SERVER.to_string());
    }
    // Not there (or wrong version): detect the remote platform and get a binary
    // for it — downloaded from the release host and cached, fully automatically.
    let platform = ssh_run(ssh_args, "uname -sm").await?;
    let local = tokio::task::spawn_blocking(move || {
        clew_core::server_dist::ensure_server_binary(&platform)
    })
    .await
    .map_err(|e| e.to_string())??;
    // Deploy: stream the binary over SSH to a temp path, chmod, atomic rename so
    // a half-copied binary is never run.
    let data = tokio::fs::read(&local).await.map_err(|e| e.to_string())?;
    let install = format!(
        "mkdir -p ~/.clew/server && cat > {REMOTE_SERVER}.tmp \
         && chmod +x {REMOTE_SERVER}.tmp && mv {REMOTE_SERVER}.tmp {REMOTE_SERVER}"
    );
    let mut child = tokio::process::Command::new("ssh")
        .args(ssh_args)
        .arg(&install)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&data).await.map_err(|e| e.to_string())?;
        let _ = stdin.flush().await;
        drop(stdin); // EOF so the remote `cat` completes
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("failed to install clew-server on the remote".into());
    }
    Ok(REMOTE_SERVER.to_string())
}

/// Plain `fn(&ConnTarget)` (no captures) as `Subscription::run_with` requires;
/// the target arrives by reference and is cloned into the async body. `use<>`
/// opts the returned stream out of capturing the input lifetime (it doesn't
/// borrow — the clone is owned), so the type matches `fn(&D) -> S`.
fn stream(target: &ConnTarget) -> impl Stream<Item = Message> + use<> {
    let target = target.clone();
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            // Build the command that runs clew-server: a local child, or — for an
            // SSH target — bootstrap the remote (install if needed) then run it over
            // SSH, whose stdio is the remote server's stdio.
            let mut cmd = match &target {
                ConnTarget::Local => tokio::process::Command::new(server_bin_path()),
                ConnTarget::Ssh { args, .. } => match bootstrap_remote(args).await {
                    Ok(remote) => {
                        let mut c = tokio::process::Command::new("ssh");
                        c.args(args).arg(&remote);
                        c
                    }
                    Err(e) => {
                        eprintln!("[clew] remote bootstrap failed: {e}");
                        let _ = output.send(Message::ServerUnavailable).await;
                        return;
                    }
                },
            };
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
                    eprintln!("[clew] could not spawn clew-server: {e}");
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
                        if let Ok(msg) = serde_json::from_str::<ServerMessage>(&line)
                            && output.send(Message::ServerEvent(msg)).await.is_err()
                        {
                            break; // client gone
                        }
                    }
                    Ok(Some(_)) => {} // blank keep-alive line
                    _ => break,       // EOF or read error: the server exited
                }
            }
            // Hold `child` to here so kill_on_drop reaps it when we stop.
            drop(child);
        },
    )
}
