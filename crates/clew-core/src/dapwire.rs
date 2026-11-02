//! The wire around a debug adapter that listens on TCP, shared by the clew
//! client (a local adapter, `src/dap/client.rs`) and clew-server (a remote
//! one, bridged to stdio: `clew-server dap-bridge`): the port it announces,
//! the connection to it, and what rdbg writes ahead of its first frame.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Child;

use crate::framing;

/// Most lines [`from_first_frame`] passes over before giving up on finding a
/// frame and handing on the rest as it is.
pub const MAX_SKIPPED_LINES: usize = 1000;

/// Longest stdout line read while waiting for the announcement, and how many
/// lines an adapter may print before it: the pipe is the adapter's, so the
/// wait is bounded in bytes as well as in time.
pub const MAX_ANNOUNCE_LINE: usize = 4096;
pub const MAX_ANNOUNCE_LINES: usize = 200;

/// Read the adapter's stdout until it announces where it listens — delve:
/// `DAP server listening at: 127.0.0.1:PORT`, js-debug: `Debug server
/// listening at 127.0.0.1:PORT`. Bounded in time, in line length and in line
/// count; an adapter that exits first ends the wait at once.
pub async fn await_listening<R>(
    out: &mut R,
    child: &mut Child,
    limit: Duration,
) -> Result<SocketAddr, String>
where
    R: AsyncBufRead + Unpin,
{
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut line = Vec::new();
    for _ in 0..MAX_ANNOUNCE_LINES {
        line.clear();
        let mut limited = (&mut *out).take(MAX_ANNOUNCE_LINE as u64);
        tokio::select! {
            read = limited.read_until(b'\n', &mut line) => {
                if matches!(read, Ok(0) | Err(_)) {
                    // Output closed: the adapter is going. Say how it went —
                    // and sweep what it started before it is reaped
                    // (`framing::wait_sweeping`): an adapter that died on
                    // its own left its group running.
                    let status = tokio::time::timeout(
                        Duration::from_secs(2),
                        framing::wait_sweeping(child),
                    )
                    .await;
                    return Err(match status {
                        Ok(Ok(status)) => {
                            format!("the debug adapter exited ({status}) before it was ready")
                        }
                        _ => "the debug adapter closed its output before it was ready".into(),
                    });
                }
                if let Some(addr) = parse_listening(&String::from_utf8_lossy(&line))? {
                    return Ok(addr);
                }
            }
            status = framing::wait_sweeping(child) => {
                return Err(match status {
                    Ok(status) => format!("the debug adapter exited ({status}) before it was ready"),
                    Err(e) => format!("the debug adapter failed: {e}"),
                });
            }
            () = &mut deadline => {
                return Err(format!(
                    "the debug adapter did not report its listening port within {} s",
                    limit.as_secs()
                ));
            }
        }
    }
    Err("the debug adapter printed too much before reporting its port".into())
}

/// The address in an adapter's "listening at" line, if this is that line.
/// Only a loopback address is usable: it is the only one clew asked for, and
/// anything else would put the session on the network.
pub fn parse_listening(line: &str) -> Result<Option<SocketAddr>, String> {
    const MARKER: &str = "listening at";
    // rdbg: `DEBUGGER: Debugger can attach via TCP/IP (127.0.0.1:PORT)`.
    const RDBG: &str = "can attach via tcp/ip (";
    let lower = line.to_ascii_lowercase();
    if let Some(at) = lower.find(RDBG) {
        let rest = &line[at + RDBG.len()..];
        let token = rest.split(')').next().unwrap_or("").trim();
        return usable(token).map(Some);
    }
    // ASCII lowercasing keeps byte offsets, so `at` indexes `line` too.
    let Some(at) = lower.find(MARKER) else {
        return Ok(None);
    };
    // delve writes `listening at: <addr>`, js-debug `listening at <addr>`.
    // Only a colon FOLLOWED BY WHITESPACE is that separator: the address
    // itself may start with one (`::1:8123`, node's spelling of IPv6
    // loopback), and stripping every leading colon turned it into `1:8123`.
    let rest = line[at + MARKER.len()..].trim_start();
    let rest = match rest.strip_prefix(':') {
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => rest,
    };
    let token = rest.split_whitespace().next().unwrap_or("");
    usable(token).map(Some)
}

/// The address `token` names, if it is one clew connects to: a loopback
/// address with a port.
pub fn usable(token: &str) -> Result<SocketAddr, String> {
    let addr = parse_addr(token)
        .filter(|a| a.port() != 0)
        .ok_or_else(|| format!("the debug adapter announced an unusable address {token:?}"))?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "the debug adapter listens on {addr}, not on loopback — refusing to connect"
        ));
    }
    Ok(addr)
}

/// `127.0.0.1:5`, `[::1]:5`, and the unbracketed `::1:5` and `localhost:5`
/// node prints.
pub fn parse_addr(token: &str) -> Option<SocketAddr> {
    if let Ok(addr) = token.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = token.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let ip = match host.trim_start_matches('[').trim_end_matches(']') {
        "localhost" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        host => host.parse().ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

/// Connect to a TCP adapter's port, retrying briefly. When the adapter is our
/// child, a child that has exited ends the retries at once: there is nothing
/// left to connect to, and before, the loop spun out its full budget.
pub async fn connect_retry(
    addr: SocketAddr,
    mut child: Option<&mut Child>,
    limit: Duration,
) -> Result<TcpStream, String> {
    let deadline = Instant::now() + limit;
    loop {
        let error = match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(e) => e,
        };
        if let Some(child) = child.as_mut()
            && let Ok(Some(status)) = framing::try_wait_sweeping(child)
        {
            return Err(format!(
                "the debug adapter exited ({status}) before accepting a connection on {addr}"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "could not connect to the debug adapter on {addr}: {error}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `read`, from its first DAP header line on. rdbg holds the connection it
/// accepts before it reads whether the client speaks DAP, and a program it
/// holds stopped writes its listing to it meanwhile, as text: lines that are
/// no frame. Once it reads the client's first frame it speaks DAP alone.
pub fn from_first_frame<R>(read: R) -> tokio::io::DuplexStream
where
    R: AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncWriteExt;
    let (mut tx, rx) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut read = BufReader::new(read);
        let mut line = Vec::new();
        for _ in 0..MAX_SKIPPED_LINES {
            line.clear();
            match (&mut read)
                .take(MAX_ANNOUNCE_LINE as u64)
                .read_until(b'\n', &mut line)
                .await
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.starts_with(b"Content-Length:") {
                break;
            }
            line.clear();
        }
        if tx.write_all(&line).await.is_err() {
            return;
        }
        let _ = tokio::io::copy(&mut read, &mut tx).await;
    });
    rx
}

/// Output read in chunks, handed on as text without cutting a character in
/// two: a character a read split waits for its other bytes.
#[derive(Default)]
pub struct Utf8Carry {
    carry: Vec<u8>,
}

impl Utf8Carry {
    /// `bytes` appended; the text that is now whole, if any.
    pub fn push(&mut self, bytes: &[u8]) -> Option<String> {
        self.carry.extend_from_slice(bytes);
        let cut = match std::str::from_utf8(&self.carry) {
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            _ => self.carry.len(),
        };
        if cut == 0 {
            return None;
        }
        let text = String::from_utf8_lossy(&self.carry[..cut]).into_owned();
        self.carry.drain(..cut);
        Some(text)
    }

    /// What is left at the end, whole or not.
    pub fn finish(self) -> Option<String> {
        (!self.carry.is_empty()).then(|| String::from_utf8_lossy(&self.carry).into_owned())
    }
}
