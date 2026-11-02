//! `clew-server dap-bridge`: a debug adapter that listens on TCP, served as
//! one that speaks over stdio — so a remote host can run it.
//!
//! A remote debug session is proxied over the protocol's process channel
//! (`ProcessInput` / `ProcessOutput`), which carries a process's stdio. An
//! adapter that serves DAP on a loopback TCP port (delve, Ruby's rdbg) listens
//! on the REMOTE's loopback, which the client cannot reach. The bridge runs
//! there in its place: it starts the adapter, reads the port it announces
//! ([`dapwire::await_listening`]), connects, and from then on passes DAP
//! frames between its own stdin/stdout and that socket.
//!
//! rdbg is its own debuggee: it announces on stderr, and what it prints after
//! — on stdout and stderr — is the program's output. The bridge sends that on
//! as DAP `output` events, between whole frames (one writer, a frame at a
//! time), and passes over what rdbg writes ahead of its first frame
//! ([`dapwire::from_first_frame`]). delve announces on stdout and sends its
//! debuggee's output as events of its own: what else it prints is drained.
//!
//! The adapter runs in the bridge's process group, which clew-server kills
//! when the session ends. When the adapter closes the session itself (the
//! program ended), it is given [`EXIT_GRACE`] to exit, so the program's last
//! output — buffered when its stdout is a pipe — is written and sent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clew_core::dapwire::{self, Utf8Carry};
use clew_core::framing::{read_message, write_frame};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

/// How long the adapter may take to announce its port.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long connecting to the announced port may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);

/// How long an adapter that closed its session has to exit by itself, and
/// its output to be sent, before the bridge ends.
const EXIT_GRACE: Duration = Duration::from_secs(3);

/// Where the adapter announces its port, which says what the rest of its
/// output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Announce {
    /// On stdout (delve): the rest of its output is its own, drained.
    Stdout,
    /// On stderr (rdbg, its own debuggee): the rest of its stdout and stderr
    /// is the program's, sent on as `output` events.
    Stderr,
}

/// The bridge's arguments: `--announce stdout|stderr -- <adapter> <args…>`.
#[derive(Debug, PartialEq, Eq)]
pub struct Bridge {
    pub announce: Announce,
    pub command: PathBuf,
    pub args: Vec<String>,
}

impl Bridge {
    /// The arguments `clew-server dap-bridge` is run with, after the
    /// subcommand.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--announce".to_string(),
            match self.announce {
                Announce::Stdout => "stdout",
                Announce::Stderr => "stderr",
            }
            .to_string(),
            "--".to_string(),
            self.command.to_string_lossy().into_owned(),
        ];
        args.extend(self.args.iter().cloned());
        args
    }

    /// [`Self::to_args`] read back.
    pub fn from_args(args: &[String]) -> Result<Bridge, String> {
        let usage = "usage: clew-server dap-bridge --announce stdout|stderr -- <adapter> [args…]";
        let [flag, announce, dashes, command, rest @ ..] = args else {
            return Err(usage.into());
        };
        if flag != "--announce" || dashes != "--" {
            return Err(usage.into());
        }
        let announce = match announce.as_str() {
            "stdout" => Announce::Stdout,
            "stderr" => Announce::Stderr,
            _ => return Err(usage.into()),
        };
        Ok(Bridge {
            announce,
            command: PathBuf::from(command),
            args: rest.to_vec(),
        })
    }
}

/// Run the bridge on this process's stdin/stdout until the session ends.
pub async fn run(bridge: Bridge) -> Result<(), String> {
    serve(bridge, tokio::io::stdin(), tokio::io::stdout()).await
}

/// [`run`], over the given streams.
pub async fn serve<I, O>(bridge: Bridge, input: I, output: O) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut child = Command::new(&bridge.command)
        .args(&bridge.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to launch {}: {e}", bridge.command.display()))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let (announcer, other): (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncRead + Unpin + Send>,
    ) = match bridge.announce {
        Announce::Stdout => (Box::new(stdout), Box::new(stderr)),
        Announce::Stderr => (Box::new(stderr), Box::new(stdout)),
    };
    let mut announcer = BufReader::new(announcer);
    let addr = dapwire::await_listening(&mut announcer, &mut child, ANNOUNCE_TIMEOUT).await?;
    let stream = dapwire::connect_retry(addr, Some(&mut child), CONNECT_TIMEOUT).await?;
    let (read, mut write) = stream.into_split();
    let out = Arc::new(Mutex::new(output));

    // What the client sends, to the adapter, as it comes.
    let mut input = input;
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut input, &mut write).await;
        let _ = write.shutdown().await;
    });
    // The program's output, as events; or the adapter's own, drained.
    let mut forwarders = Vec::new();
    for (stream, category) in [
        (
            Box::new(announcer) as Box<dyn AsyncRead + Unpin + Send>,
            "stderr",
        ),
        (other, "stdout"),
    ] {
        let category = match (bridge.announce, category) {
            (Announce::Stderr, "stderr") => Some("stderr"),
            (Announce::Stderr, _) => Some("stdout"),
            (Announce::Stdout, _) => None,
        };
        forwarders.push(tokio::spawn(forward(stream, category, out.clone())));
    }
    // The adapter's frames, to the client, until it closes the session.
    let mut frames = BufReader::new(dapwire::from_first_frame(read));
    while let Ok(Some(frame)) = read_message(&mut frames).await {
        let mut out = out.lock().await;
        if write_frame(&mut *out, &frame).await.is_err() || out.flush().await.is_err() {
            break;
        }
    }
    // Let the program finish writing out; then whatever is left goes with
    // the bridge (`kill_on_drop`, and clew-server kills the group).
    let _ = tokio::time::timeout(EXIT_GRACE, child.wait()).await;
    for forwarder in forwarders {
        let _ = tokio::time::timeout(EXIT_GRACE, forwarder).await;
    }
    Ok(())
}

/// Send what `stream` prints as `category` output events — or, with no
/// category, read it and let it go.
async fn forward<O>(
    mut stream: Box<dyn AsyncRead + Unpin + Send>,
    category: Option<&'static str>,
    out: Arc<Mutex<O>>,
) where
    O: tokio::io::AsyncWrite + Unpin + Send,
{
    let mut buf = [0u8; 4096];
    let mut text = Utf8Carry::default();
    loop {
        let n = match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Some(category) = category else { continue };
        if let Some(whole) = text.push(&buf[..n]) {
            emit(&out, category, whole).await;
        }
    }
    if let (Some(category), Some(rest)) = (category, text.finish()) {
        emit(&out, category, rest).await;
    }
}

/// One `output` event, written whole.
async fn emit<O>(out: &Mutex<O>, category: &str, text: String)
where
    O: tokio::io::AsyncWrite + Unpin + Send,
{
    let event = json!({
        "seq": 0,
        "type": "event",
        "event": "output",
        "body": { "category": category, "output": text },
    });
    let mut out = out.lock().await;
    let _ = write_frame(&mut *out, &event).await;
    let _ = out.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use clew_core::debugadapter::{AdapterTransport, bridge_args};
    use std::path::Path;

    /// The arguments the resolver gives the bridge are the ones it reads.
    #[test]
    fn the_resolvers_arguments_are_the_bridges() {
        for (transport, announce) in [
            (AdapterTransport::TcpOnStderr, Announce::Stderr),
            (AdapterTransport::Tcp, Announce::Stdout),
        ] {
            let args = bridge_args(
                transport,
                Path::new("/usr/bin/rdbg"),
                &["--open".into(), "--".into(), "app.rb".into()],
            );
            assert_eq!(args[0], "dap-bridge");
            let bridge = Bridge::from_args(&args[1..]).unwrap();
            assert_eq!(
                bridge,
                Bridge {
                    announce,
                    command: "/usr/bin/rdbg".into(),
                    args: vec!["--open".into(), "--".into(), "app.rb".into()],
                }
            );
            assert_eq!(bridge.to_args(), args[1..]);
        }
        assert!(Bridge::from_args(&["--announce".into(), "both".into()]).is_err());
    }

    /// The bridge reaches the adapter at the port it names on stderr, passes
    /// frames both ways — over the text ahead of the adapter's first — and
    /// sends what the program prints on as output events, whole.
    #[tokio::test]
    #[cfg(unix)]
    async fn frames_pass_both_ways_and_the_programs_output_comes_as_events() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // Stands in for rdbg: announces on stderr, then prints as its
        // program would, and lives until its session is over.
        let script = format!(
            "echo 'DEBUGGER: Debugger can attach via TCP/IP (127.0.0.1:{port})' >&2; \
             sleep 0.3; printf 'caf\\303'; sleep 0.2; printf '\\251\\n'; echo oops >&2; \
             sleep 1"
        );
        let bridge = Bridge {
            announce: Announce::Stderr,
            command: "/bin/sh".into(),
            args: vec!["-c".into(), script],
        };
        let (mut client_in, bridge_in) = tokio::io::duplex(64 * 1024);
        let (bridge_out, client_out) = tokio::io::duplex(64 * 1024);
        let running = tokio::spawn(serve(bridge, bridge_in, bridge_out));
        let (socket, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .expect("the bridge connects to the port named on stderr")
            .unwrap();
        let (adapter_read, mut adapter_write) = socket.into_split();
        let mut adapter_read = BufReader::new(adapter_read);
        // The client's request reaches the adapter.
        let request = json!({"seq": 1, "type": "request", "command": "initialize"});
        write_frame(&mut client_in, &request).await.unwrap();
        let got = read_message(&mut adapter_read).await.unwrap().unwrap();
        assert_eq!(got["command"], "initialize");
        // The adapter's answer — after a listing that is no frame — reaches
        // the client.
        adapter_write
            .write_all(b"out [1, 3] in app.rb\nout =>   1| x = 41\n")
            .await
            .unwrap();
        let answer = json!({"seq": 1, "type": "response", "request_seq": 1,
                            "success": true, "command": "initialize"});
        write_frame(&mut adapter_write, &answer).await.unwrap();
        let mut client_out = BufReader::new(client_out);
        let (mut answered, mut stdout, mut stderr) = (false, String::new(), String::new());
        while !(answered && stdout.ends_with('\n') && stderr.ends_with('\n')) {
            let frame =
                tokio::time::timeout(Duration::from_secs(10), read_message(&mut client_out))
                    .await
                    .expect("a frame in time")
                    .unwrap()
                    .expect("the bridge is still up");
            match (frame["type"].as_str(), frame["body"]["category"].as_str()) {
                (Some("response"), _) => {
                    assert_eq!(frame, answer);
                    answered = true;
                }
                (Some("event"), Some("stdout")) => {
                    stdout.push_str(frame["body"]["output"].as_str().unwrap())
                }
                (Some("event"), Some("stderr")) => {
                    stderr.push_str(frame["body"]["output"].as_str().unwrap())
                }
                other => panic!("{other:?}: {frame}"),
            }
        }
        assert_eq!(stdout, "café\n");
        assert_eq!(stderr, "oops\n");
        // The adapter ends the session; the bridge ends with it.
        drop(adapter_write);
        drop(adapter_read);
        let ended = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the bridge ends with its session")
            .unwrap();
        assert_eq!(ended, Ok(()));
    }
}
