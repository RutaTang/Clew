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
/// `Message::ServerConnected`, then pumps the server's stdout until it exits —
/// and reports the exit as `Message::ServerDisconnected`.
///
/// Keyed on `(target, generation)`: connecting to a different host (or back to
/// local) changes the subscription identity, so iced drops the old transport —
/// killing its server — and runs a fresh one for the new target. That single
/// seam is how an in-app "Connect" switches between local and remote. The
/// generation is the client's `conn_gen`, bumped when a transport dies: a
/// finished stream never restarts on its own (iced only reacts to identity
/// changes), so the bump *is* the reconnect.
pub fn subscription(target: ConnTarget, generation: u64) -> iced::Subscription<Message> {
    iced::Subscription::run_with((target, generation), stream)
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

/// The client's own release version. The remote server is deployed and cached
/// keyed by it (`~/.clew/server/<version>/clew-server`), so a client update
/// carries its matching — possibly patched — server to the remote, each version
/// at its own path (like VS Code's per-commit remote server). `~` is expanded by
/// the remote login shell.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

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
    let remote_dir = format!("~/.clew/server/{CLIENT_VERSION}");
    let remote_server = format!("{remote_dir}/clew-server");
    // This exact version already deployed and runnable? The path pins the
    // version, so any live clew-server there is the right build; `--version` is a
    // plain-shell liveness probe.
    if let Ok(out) = ssh_run(ssh_args, &format!("{remote_server} --version 2>/dev/null")).await
        && out.contains("clew-server")
    {
        return Ok(remote_server);
    }
    // Not there: detect the remote platform and get this version's binary for it
    // — downloaded from the release host and cached, fully automatically.
    let platform = ssh_run(ssh_args, "uname -sm").await?;
    let local = tokio::task::spawn_blocking(move || {
        clew_core::server_dist::ensure_server_binary(&platform, CLIENT_VERSION)
    })
    .await
    .map_err(|e| e.to_string())??;
    // Deploy: stream the binary over SSH to a temp path, chmod, atomic rename so
    // a half-copied binary is never run.
    let data = tokio::fs::read(&local).await.map_err(|e| e.to_string())?;
    let install = format!(
        "mkdir -p {remote_dir} && cat > {remote_server}.tmp \
         && chmod +x {remote_server}.tmp && mv {remote_server}.tmp {remote_server}"
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
    Ok(remote_server)
}

/// Plain `fn(&(ConnTarget, u64))` (no captures) as `Subscription::run_with`
/// requires; the key arrives by reference and is cloned into the async body.
/// `use<>` opts the returned stream out of capturing the input lifetime (it
/// doesn't borrow — the clone is owned), so the type matches `fn(&D) -> S`.
fn stream(key: &(ConnTarget, u64)) -> impl Stream<Item = Message> + use<> {
    let (target, generation) = key.clone();
    iced::stream::channel(
        256,
        move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            // A reconnect (generation > 0) after a death waits a moment first,
            // so a server that crashes on startup can't hot-loop respawns.
            if generation > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }
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
            let mut client_gone = false;
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.is_empty() => {
                        if let Ok(msg) = serde_json::from_str::<ServerMessage>(&line)
                            && output.send(Message::ServerEvent(msg)).await.is_err()
                        {
                            client_gone = true;
                            break;
                        }
                    }
                    Ok(Some(_)) => {} // blank keep-alive line
                    _ => break,       // EOF or read error: the server exited
                }
            }
            // The server died mid-session: tell the client, so it can clear
            // its in-flight bookkeeping and bump the generation to reconnect.
            // (Skipped when the *client* went away — the app is closing.)
            if !client_gone {
                let _ = output.send(Message::ServerDisconnected).await;
            }
            // Hold `child` to here so kill_on_drop reaps it when we stop.
            drop(child);
        },
    )
}
