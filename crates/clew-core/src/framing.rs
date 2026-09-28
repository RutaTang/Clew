//! `Content-Length` framing, and the client actor built on it, shared by
//! clew's LSP client ([`crate::lsp::client`]) and its DAP client (the GUI's
//! `dap::client`).
//!
//! Both protocols carry one JSON message per frame:
//!
//! ```text
//! Content-Length: 52\r\n
//! \r\n
//! {"jsonrpc":"2.0","id":1,"method":"initialize",...}
//! ```
//!
//! The peer is another program — a language server or a debug adapter, which
//! the repository's own `.clew/lsp.toml` or `launch.json` may have chosen — so
//! every byte it writes is untrusted input. The reader fails closed on
//! FRAMING: a header it cannot parse (a missing, malformed, conflicting or
//! oversized `Content-Length`, an overlong or non-ASCII header line, a stream
//! that ends inside a frame) ends the transport with an error, because once a
//! frame boundary has been misread every later frame would be read from the
//! wrong offset. A frame whose body is well delimited but is not JSON is only
//! that one message's problem: it is reported and skipped, and the stream
//! stays in sync.
//!
//! On top of the framing sits the actor skeleton both clients run: one task
//! owns the write half and the table of requests awaiting answers, and
//! `select!`s between commands from cheap, cloneable [`Handle`]s and messages
//! from the reader task. What differs between LSP and DAP — the envelope, how
//! a response names its request, what the peer's own requests and
//! notifications mean, how a session is ended politely — comes in through the
//! [`Protocol`] trait.
//!
//! Every write the actor makes is bounded, because the peer decides whether
//! its input is read at all: a message the peer accepts none of for
//! [`Protocol::write_stall_limit`] ends the session, and once a stop has been
//! requested a write still in progress gets only [`STOP_WRITE_GRACE`] more.
//! A peer that stops reading can therefore neither wedge the actor nor hold
//! up the stop that is meant to get rid of it.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

/// Longest header line accepted, terminator included. Real headers are a few
/// dozen bytes; a line this long is garbage or an attempt to balloon memory.
pub const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Most header lines one frame may carry (blank separator lines ahead of the
/// headers included). Real peers send one or two; the cap keeps a peer from
/// streaming headers forever without ever reaching a body.
pub const MAX_HEADER_LINES: usize = 32;

/// Largest message body accepted. `Content-Length` is peer-chosen, so it is
/// checked against this before a single body byte is read, and the body
/// buffer then grows only with the bytes that actually arrive.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Initial body allocation, whatever `Content-Length` claims: a peer that
/// announces 64 MiB and then sends nothing must not pin 64 MiB.
const INITIAL_BODY_CAPACITY: usize = 64 * 1024;

/// How long the peer may accept NONE of a message before the session is
/// declared wedged — the default of [`Protocol::write_stall_limit`].
///
/// A clock on progress, not on the whole message: every chunk the peer takes
/// restarts it, so a large `didOpen` to a slow reader goes through, while a
/// peer that stopped reading its input altogether is found out. Language
/// servers and debug adapters read their input on a thread (or event loop) of
/// its own, so this long without taking a single byte is not a busy peer.
pub const WRITE_STALL_LIMIT: Duration = Duration::from_secs(30);

/// How long a write the peer is not taking may continue once a stop has been
/// requested — shared by every write after the request, so it bounds how much
/// a stop can be delayed by writes, however many are queued ahead of it.
pub const STOP_WRITE_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(1)
};

/// The most the writes of a stop can take once it is requested: a write
/// already under way, the goodbye's closing message and the final shutdown of
/// the peer's input, one [`STOP_WRITE_GRACE`] each. The goodbye's own request
/// and answer come on top (its `reply_wait`), and so does whatever the owner
/// then does with the peer process.
pub const STOP_LIMIT: Duration = STOP_WRITE_GRACE
    .saturating_add(STOP_WRITE_GRACE)
    .saturating_add(STOP_WRITE_GRACE);

/// Why a frame could not be used.
#[derive(Debug)]
pub enum FrameError {
    /// Reading from the peer failed.
    Io(std::io::Error),
    /// The stream ended inside a frame: mid-header or mid-body.
    Truncated,
    /// A header line ran past [`MAX_HEADER_LINE_BYTES`].
    HeaderLineTooLong,
    /// More than [`MAX_HEADER_LINES`] header lines before a body.
    TooManyHeaderLines,
    /// A header line that is not `Name: value` in ASCII.
    MalformedHeader(String),
    /// The headers ended without a `Content-Length`.
    MissingContentLength,
    /// A `Content-Length` that is not a plain decimal byte count.
    InvalidContentLength(String),
    /// Two `Content-Length` headers that disagree.
    ConflictingContentLength,
    /// A `Content-Length` over [`MAX_FRAME_BYTES`].
    TooLarge(u64),
    /// A well-framed body that is not JSON. The only error that does not end
    /// the transport: the next frame starts exactly where this one ended.
    InvalidJson(String),
}

impl FrameError {
    /// Whether the stream is unusable after this error. Everything but a bad
    /// body is: with a frame boundary misread there is no way back into sync.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, FrameError::InvalidJson(_))
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "read failed: {e}"),
            FrameError::Truncated => write!(f, "the stream ended inside a message"),
            FrameError::HeaderLineTooLong => {
                write!(f, "a header line exceeds {MAX_HEADER_LINE_BYTES} bytes")
            }
            FrameError::TooManyHeaderLines => {
                write!(
                    f,
                    "more than {MAX_HEADER_LINES} header lines in one message"
                )
            }
            FrameError::MalformedHeader(line) => write!(f, "malformed header line {line}"),
            FrameError::MissingContentLength => write!(f, "a message without Content-Length"),
            FrameError::InvalidContentLength(v) => write!(f, "invalid Content-Length {v}"),
            FrameError::ConflictingContentLength => {
                write!(f, "conflicting Content-Length headers")
            }
            FrameError::TooLarge(n) => {
                write!(
                    f,
                    "a {n}-byte message exceeds the {MAX_FRAME_BYTES}-byte limit"
                )
            }
            FrameError::InvalidJson(e) => write!(f, "a message that is not JSON ({e})"),
        }
    }
}

impl std::error::Error for FrameError {}

/// A short, escaped rendering of peer bytes for an error message: enough to
/// recognize what arrived, never the whole of whatever a peer chose to send.
fn preview(bytes: &[u8]) -> String {
    const MAX: usize = 64;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]);
    let more = if bytes.len() > MAX { "…" } else { "" };
    format!("{text:?}{more}")
}

/// An RFC 7230 `token` byte — what a header field name is made of. No
/// whitespace, so `Content-Length :` is malformed rather than guessed at.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Parse a `Content-Length` value: ASCII digits only. `str::parse` would also
/// take a leading `+`, which no conforming peer sends and which therefore
/// says the stream is not what it claims to be.
fn parse_length(value: &[u8]) -> Result<u64, FrameError> {
    let invalid = || FrameError::InvalidContentLength(preview(value));
    if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid());
    }
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(invalid)
}

/// Read one frame's body. `Ok(None)` is a clean end of stream between frames;
/// an end anywhere inside a frame is [`FrameError::Truncated`].
///
/// Header names are matched case-insensitively (`content-length` is the same
/// header), and both `\r\n` and a bare `\n` end a line. Headers other than
/// `Content-Length` (`Content-Type`) are accepted and ignored. Blank lines
/// ahead of the first header are skipped as separators; they still count
/// against [`MAX_HEADER_LINES`].
pub async fn read_frame<R>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<u64> = None;
    let mut headers = 0usize;
    let mut lines = 0usize;
    let mut line = Vec::with_capacity(64);
    loop {
        line.clear();
        let n = (&mut *reader)
            .take(MAX_HEADER_LINE_BYTES as u64)
            .read_until(b'\n', &mut line)
            .await
            .map_err(FrameError::Io)?;
        if n == 0 {
            // Between frames, EOF is the peer closing cleanly; after a
            // header it cut a message short.
            return if headers == 0 {
                Ok(None)
            } else {
                Err(FrameError::Truncated)
            };
        }
        if line.last() != Some(&b'\n') {
            return Err(if line.len() >= MAX_HEADER_LINE_BYTES {
                FrameError::HeaderLineTooLong
            } else {
                FrameError::Truncated
            });
        }
        lines += 1;
        if lines > MAX_HEADER_LINES {
            return Err(FrameError::TooManyHeaderLines);
        }
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            if headers == 0 {
                continue; // a separator ahead of the headers
            }
            break; // the blank line that ends the headers
        }
        headers += 1;
        let malformed = || FrameError::MalformedHeader(preview(&line));
        if !line.is_ascii() {
            return Err(malformed());
        }
        let colon = line.iter().position(|&b| b == b':').ok_or_else(malformed)?;
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        if name.is_empty() || !name.iter().all(|&b| is_token_byte(b)) {
            return Err(malformed());
        }
        if name.eq_ignore_ascii_case(b"content-length") {
            let value = value.trim_ascii();
            let len = parse_length(value)?;
            if content_length.is_some_and(|prev| prev != len) {
                return Err(FrameError::ConflictingContentLength);
            }
            if len > MAX_FRAME_BYTES as u64 {
                return Err(FrameError::TooLarge(len));
            }
            content_length = Some(len);
        }
    }
    let len = content_length.ok_or(FrameError::MissingContentLength)? as usize;
    let mut body = Vec::with_capacity(len.min(INITIAL_BODY_CAPACITY));
    (&mut *reader)
        .take(len as u64)
        .read_to_end(&mut body)
        .await
        .map_err(FrameError::Io)?;
    if body.len() < len {
        return Err(FrameError::Truncated);
    }
    Ok(Some(body))
}

/// Parse one frame body. A failure is [`FrameError::InvalidJson`], the one
/// non-fatal error: the body was delimited correctly, so the stream is fine.
pub fn decode(body: &[u8]) -> Result<Value, FrameError> {
    serde_json::from_slice(body).map_err(|e| FrameError::InvalidJson(e.to_string()))
}

/// Read one newline-terminated clew protocol frame (a JSON line), capped at
/// [`clew_protocol::MAX_FRAME_BYTES`] — the reader both ends of a clew
/// connection use. `None` on EOF, on a read error, or on an oversized frame:
/// an over-cap line cannot be resynced past, so the connection ends (the
/// client reconnects with fresh state).
pub async fn read_protocol_line<R>(reader: &mut R) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    read_line_capped(reader, clew_protocol::MAX_FRAME_BYTES).await
}

/// [`read_protocol_line`] with the cap on the line's CONTENT, the newline
/// not counted: exactly what a writer refusing `json.len() > cap` sends is
/// read. (Each end used to count the newline, so the largest frame the other
/// end would send was the one frame it dropped the connection on.)
pub async fn read_line_capped<R>(reader: &mut R, cap: usize) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let n = reader
        .take(cap as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
        .ok()?;
    if n == 0 {
        return None; // EOF
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > cap {
        return None; // over the cap, and no end in sight: fail closed
    }
    String::from_utf8(buf).ok()
}

/// Read and parse one message; `Ok(None)` at a clean end of stream.
pub async fn read_message<R>(reader: &mut R) -> Result<Option<Value>, FrameError>
where
    R: AsyncBufRead + Unpin,
{
    match read_frame(reader).await? {
        Some(body) => decode(&body).map(Some),
        None => Ok(None),
    }
}

/// One message as bytes on the wire. `Content-Length` counts BYTES: a body
/// with non-ASCII text is longer than its character count.
pub fn encode(msg: &Value) -> Vec<u8> {
    let body = msg.to_string();
    let mut out = Vec::with_capacity(body.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body.as_bytes());
    out
}

/// Write one message and flush it.
pub async fn write_frame<W>(writer: &mut W, msg: &Value) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(&encode(msg)).await?;
    writer.flush().await
}

/// What the reader task hands the actor: a message, or a frame that could
/// not be used. A fatal error is always the last item before the channel
/// closes; a clean end of stream just closes it.
pub type Inbound = Result<Value, FrameError>;

/// Start the reader task for `reader` and return its output.
///
/// The channel is unbounded on purpose. The reader must keep draining the
/// peer's output even while the actor is busy writing to the peer's input: a
/// peer blocked writing its stdout stops reading its stdin, so a bounded queue
/// here could wedge both sides. Each message is itself bounded by
/// [`MAX_FRAME_BYTES`], and the actor drains the queue as fast as it can
/// dispatch.
pub fn spawn_reader<R>(reader: R) -> mpsc::UnboundedReceiver<Inbound>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(reader_loop(BufReader::new(reader), tx));
    rx
}

/// The reader task: frames off `reader` into `tx` until a clean end of
/// stream, a fatal framing error (sent, then the loop ends), or the actor
/// going away.
pub async fn reader_loop<R>(mut reader: R, tx: mpsc::UnboundedSender<Inbound>)
where
    R: AsyncBufRead + Unpin,
{
    loop {
        // Racing `closed` means an actor that has ended stops this task too,
        // even while the peer is silent; the half-read frame dropped with the
        // race was going nowhere.
        let frame = tokio::select! {
            frame = read_frame(&mut reader) => frame,
            () = tx.closed() => return,
        };
        let item = match frame {
            Ok(None) => return,
            Ok(Some(body)) => decode(&body),
            Err(e) => Err(e),
        };
        let fatal = matches!(&item, Err(e) if e.is_fatal());
        if tx.send(item).is_err() || fatal {
            return;
        }
    }
}

// ------------------------------------------------------------------- actor

/// Where the answer to one request goes: the body, or why there is none.
pub type Reply = oneshot::Sender<Result<Value, Failure>>;

/// Why a request got no body: what the peer answered it with — its text, and
/// the error code it gave, if it gave one (a JSON-RPC peer does; a DAP one
/// does not) — or the reason the session ended with it in flight, which has
/// no code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The peer's error code: what tells one kind of refusal from another
    /// (a request the peer dropped because what it asked about changed, say)
    /// where the text is the peer's own prose.
    pub code: Option<i64>,
    pub message: String,
}

impl From<&str> for Failure {
    /// A failure with no code: a session's end, or a peer that gives none.
    fn from(message: &str) -> Self {
        Failure {
            code: None,
            message: message.to_string(),
        }
    }
}

/// Requests awaiting their answers, keyed by the id they went out with.
///
/// Two ways an entry leaves besides being answered, because peers do not
/// answer everything: [`Pending::forget`] when its caller timed out (it says
/// so with [`Command::Forget`]), and [`Pending::sweep`] for callers whose
/// future was dropped before they could. Without both, a session with a peer
/// that ignores some request type grows this map for its whole lifetime.
#[derive(Default)]
pub struct Pending {
    map: HashMap<i64, Reply>,
}

impl Pending {
    pub fn insert(&mut self, id: i64, reply: Reply) {
        self.map.insert(id, reply);
    }

    /// Deliver the answer for `id`. `false` when nobody is waiting for it
    /// (never asked, forgotten, or answered twice).
    pub fn resolve(&mut self, id: i64, result: Result<Value, Failure>) -> bool {
        match self.map.remove(&id) {
            Some(reply) => {
                let _ = reply.send(result);
                true
            }
            None => false,
        }
    }

    /// Drop the entry for `id` without answering it. `false` when there was
    /// none: answered already, or swept.
    pub fn forget(&mut self, id: i64) -> bool {
        self.map.remove(&id).is_some()
    }

    /// Drop every entry whose caller has gone away, and say which.
    pub fn sweep(&mut self) -> Vec<i64> {
        let mut gone = Vec::new();
        self.map.retain(|&id, reply| {
            let closed = reply.is_closed();
            if closed {
                gone.push(id);
            }
            !closed
        });
        gone
    }

    /// Answer everything still waiting with `reason`.
    pub fn fail_all(&mut self, reason: &str) {
        for (_, reply) in self.map.drain() {
            let _ = reply.send(Err(Failure::from(reason)));
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// What a [`Handle`] asks of the actor.
pub enum Command {
    /// Send a request and route its answer to `reply`.
    Request {
        id: i64,
        method: String,
        params: Value,
        reply: Reply,
    },
    /// Send a message nobody waits on (an LSP notification, a DAP request
    /// whose response is ignored). `id` is for protocols that number every
    /// message; JSON-RPC notifications carry none.
    Notify {
        id: i64,
        method: String,
        params: Value,
    },
    /// The caller of request `id` gave up — it timed out, or dropped the
    /// call: stop holding a place for it, and tell the peer, where the
    /// protocol can ([`Protocol::cancel`]).
    Forget { id: i64 },
    /// End the session, politely if the protocol has a way ([`Goodbye`]).
    /// `done` fires once the session and its peer process are gone.
    Stop { done: Option<oneshot::Sender<()>> },
    /// How many requests are waiting (tests only).
    #[cfg(test)]
    PendingCount(oneshot::Sender<usize>),
}

/// Hands out message ids, shared by every [`Handle`] of one session and by
/// the actor (for its own [`Goodbye`] request), so no two messages share one.
#[derive(Clone, Default)]
pub struct Ids(Arc<AtomicI64>);

impl Ids {
    pub fn next(&self) -> i64 {
        // Ids start at 1: the first request of a session carries id 1.
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Why a call through a [`Handle`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The session had already ended before the request could be queued.
    NotRunning,
    /// The session ended while the request was in flight, without saying why.
    Closed,
    /// No answer within the caller's time box. The pending entry is released.
    TimedOut,
    /// The peer refused the request, or the session ended with this reason.
    Failed(Failure),
}

/// A cheap, cloneable handle to a running session.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::UnboundedSender<Command>,
    ids: Ids,
    /// Raised by [`Handle::stop`], beside the [`Command::Stop`] it sends: the
    /// command waits its turn in the queue, while this reaches an actor that
    /// is stuck writing to a peer that stopped reading. The last handle going
    /// away closes the channel, which the actor reads the same way.
    stop: watch::Sender<bool>,
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("alive", &self.alive())
            .finish()
    }
}

/// The actor's end of a session's command channel.
pub struct Mailbox {
    rx: mpsc::UnboundedReceiver<Command>,
    ids: Ids,
    stop: watch::Receiver<bool>,
}

/// A new session's handle and the mailbox its actor will read.
pub fn mailbox() -> (Handle, Mailbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let ids = Ids::default();
    (
        Handle {
            tx,
            ids: ids.clone(),
            stop: stop_tx,
        },
        Mailbox {
            rx,
            ids,
            stop: stop_rx,
        },
    )
}

/// A request whose caller may give up on it: dropped before [`Abandon::keep`]
/// — the call timed out, or its future was dropped part-way (an outer time
/// box, an aborted task) — it tells the actor to forget the request, which
/// tells the peer ([`Protocol::cancel`]). A caller that stopped waiting left
/// the peer working on its request: asked again, it worked on two.
struct Abandon<'a> {
    tx: &'a mpsc::UnboundedSender<Command>,
    id: i64,
    answered: bool,
}

impl Abandon<'_> {
    /// The request was settled — answered, or failed with its session: there
    /// is nothing to forget.
    fn keep(&mut self) {
        self.answered = true;
    }
}

impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if !self.answered {
            let _ = self.tx.send(Command::Forget { id: self.id });
        }
    }
}

impl Handle {
    /// Send a request and await its answer, for at most `timeout`. A call
    /// that times out, or is dropped before its answer, tells the actor to
    /// release the request's entry, and the peer that it need not answer.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let id = self.ids.next();
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Request {
                id,
                method: method.to_string(),
                params,
                reply,
            })
            .map_err(|_| CallError::NotRunning)?;
        let mut abandon = Abandon {
            tx: &self.tx,
            id,
            answered: false,
        };
        let answer = tokio::time::timeout(timeout, rx).await;
        if answer.is_ok() {
            abandon.keep();
        }
        match answer {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(failure))) => Err(CallError::Failed(failure)),
            Ok(Err(_)) => Err(CallError::Closed),
            Err(_) => Err(CallError::TimedOut),
        }
    }

    /// Send a message nobody waits on.
    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(Command::Notify {
            id: self.ids.next(),
            method: method.to_string(),
            params,
        });
    }

    /// Ask the session to end; returns at once.
    pub fn stop(&self) {
        self.stop.send_replace(true);
        let _ = self.tx.send(Command::Stop { done: None });
    }

    /// Ask the session to end and wait (at most `limit`) until it has.
    pub async fn stop_and_wait(&self, limit: Duration) {
        let (done, rx) = oneshot::channel();
        self.stop.send_replace(true);
        if self.tx.send(Command::Stop { done: Some(done) }).is_ok() {
            let _ = tokio::time::timeout(limit, rx).await;
        }
    }

    /// Whether the session is still running: false once it ended for any
    /// reason (stopped, peer gone, transport broken).
    pub fn alive(&self) -> bool {
        !self.tx.is_closed()
    }

    #[cfg(test)]
    pub(crate) async fn pending_count(&self) -> Option<usize> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Command::PendingCount(tx)).ok()?;
        rx.await.ok()
    }
}

/// What the protocol made of one inbound message.
pub enum Handled {
    /// The answer to our request `id`.
    Response {
        id: i64,
        result: Result<Value, Failure>,
    },
    /// Write this back to the peer (the answer to one of ITS requests).
    Reply(Value),
    /// Folded in; nothing to send.
    Nothing,
    /// Whoever consumes this session is gone: end it.
    Close,
}

/// A polite end to a session: a request whose answer is awaited for at most
/// `reply_wait`, then an optional final message (LSP: `shutdown`, `exit`).
pub struct Goodbye {
    pub method: &'static str,
    pub params: Value,
    pub reply_wait: Duration,
    pub then: Option<(&'static str, Value)>,
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    /// We ended it: `stop`, or every handle was dropped.
    Stopped,
    /// The peer closed its output cleanly.
    Closed,
    /// The transport failed: the peer's output broke framing or could not be
    /// read, or its input could not be written — it failed, the peer stopped
    /// reading it, or a stop found a write the peer was not taking. A peer
    /// ended this way is owed no grace period before it is killed.
    Broken(String),
    /// The protocol's consumer went away ([`Handled::Close`]).
    ConsumerGone,
}

/// The protocol-specific half of a session.
pub trait Protocol: Send + 'static {
    /// The envelope for request `id`.
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value;
    /// The envelope for a message nobody waits on.
    fn notification(&mut self, id: i64, method: &str, params: Value) -> Value;
    /// Make sense of one inbound message. `ids` numbers any reply for
    /// protocols that sequence every message (DAP).
    fn inbound(&mut self, msg: Value, ids: &Ids) -> Handled;
    /// A frame could not be used. Fatal ones end the session right after.
    fn frame_error(&mut self, error: &FrameError);
    /// How to end the session politely, if there is a way.
    fn goodbye(&mut self) -> Option<Goodbye> {
        None
    }
    /// How long the peer may accept none of a message before the session is
    /// ended as [`EndReason::Broken`] (see [`WRITE_STALL_LIMIT`]).
    fn write_stall_limit(&self) -> Duration {
        WRITE_STALL_LIMIT
    }
    /// The message that tells the peer request `id` is no longer wanted, if
    /// the protocol has one (LSP: `$/cancelRequest`): sent when its caller
    /// gave up on it while it was still in flight, so a peer still at work
    /// on it can stop — and one asked again does not work on two copies.
    fn cancel(&mut self, _id: i64) -> Option<Value> {
        None
    }
    /// The session ended: record it however the protocol surfaces such
    /// things, and return the error text for requests still waiting.
    fn ended(&mut self, reason: &EndReason) -> String;
}

/// A finished session.
pub struct Ended {
    pub reason: EndReason,
    /// [`Command::Stop`] callers waiting for the session to be fully gone.
    /// The owner fires them once it has also dealt with the peer process.
    pub waiters: Vec<oneshot::Sender<()>>,
}

impl Ended {
    /// Tell every `stop_and_wait` caller the session is gone.
    pub fn release(self) {
        for waiter in self.waiters {
            let _ = waiter.send(());
        }
    }
}

/// Why a write to the peer did not go through. Every one of them ends the
/// session: a message cut off part-way leaves the peer's input mid-frame, and
/// nothing written after it could be read from the right offset.
#[derive(Debug)]
enum WriteError {
    /// Writing failed outright (the peer closed its input, the socket reset).
    Io(std::io::Error),
    /// The peer accepted none of the message for this long: it stopped
    /// reading its input.
    Stalled(Duration),
    /// A stop was requested while the peer was not taking the message, and it
    /// still had not by the end of [`STOP_WRITE_GRACE`] (or of the goodbye's
    /// own deadline).
    Abandoned,
}

impl WriteError {
    /// What the session ends as. The text completes "the connection … broke:
    /// ", which is how both protocols word a broken transport.
    fn end_reason(&self) -> EndReason {
        EndReason::Broken(match self {
            WriteError::Io(e) => format!("writing to it failed: {e}"),
            WriteError::Stalled(limit) => {
                format!("it stopped reading its input (nothing accepted for {limit:?})")
            }
            WriteError::Abandoned => {
                "it was not reading its input when the session was stopped".to_string()
            }
        })
    }
}

/// Write all of `bytes` and flush them, requiring progress: the peer must take
/// some of what is left within `stall`, and every chunk it takes restarts that
/// clock. Cancel-safe between chunks — a `write` that has not completed has
/// written nothing.
async fn write_progressing<W>(
    writer: &mut W,
    bytes: &[u8],
    stall: Duration,
) -> Result<(), WriteError>
where
    W: AsyncWrite + Unpin,
{
    let mut rest = bytes;
    while !rest.is_empty() {
        match tokio::time::timeout(stall, writer.write(rest)).await {
            Err(_) => return Err(WriteError::Stalled(stall)),
            Ok(Ok(0)) => return Err(WriteError::Io(std::io::ErrorKind::WriteZero.into())),
            Ok(Ok(n)) => rest = &rest[n..],
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return Err(WriteError::Io(e)),
        }
    }
    match tokio::time::timeout(stall, writer.flush()).await {
        Err(_) => Err(WriteError::Stalled(stall)),
        Ok(flushed) => flushed.map_err(WriteError::Io),
    }
}

/// Raised by [`Handle::stop`], and by the last handle going away.
struct StopSignal(watch::Receiver<bool>);

impl StopSignal {
    /// Resolves once a stop has been requested, or no handle is left to ask.
    /// Level-triggered: it resolves at once for as long as either holds.
    async fn raised(&mut self) {
        let _ = self.0.wait_for(|stop| *stop).await;
    }
}

/// The actor's side of the peer's input. Every write must make progress
/// ([`write_progressing`]); and once a stop has been requested, a write the
/// peer is not taking gets [`STOP_WRITE_GRACE`] more — counted from the first
/// such write, so a queue of them cannot stretch it.
struct Outbox<W> {
    writer: W,
    stall: Duration,
    stop: StopSignal,
    /// When the grace given to writes after a stop request runs out. Set the
    /// first time a write is found still in progress with a stop requested.
    cutoff: Option<Instant>,
}

impl<W: AsyncWrite + Unpin> Outbox<W> {
    /// Send one message on the session's behalf, bounded as described on the
    /// type.
    async fn send(&mut self, msg: &Value) -> Result<(), WriteError> {
        let bytes = encode(msg);
        let write = write_progressing(&mut self.writer, &bytes, self.stall);
        tokio::pin!(write);
        if self.cutoff.is_none() {
            tokio::select! {
                // A write that can finish at once always does, whatever else
                // is pending.
                biased;
                done = &mut write => return done,
                () = self.stop.raised() => {
                    self.cutoff = Some(Instant::now() + STOP_WRITE_GRACE);
                }
            }
        }
        let cutoff = self.cutoff.unwrap_or_else(Instant::now);
        tokio::time::timeout_at(cutoff, &mut write)
            .await
            .unwrap_or(Err(WriteError::Abandoned))
    }

    /// Send one message by `deadline` at the latest — the goodbye's own clock,
    /// which runs while a stop is (necessarily) already requested.
    async fn send_by(&mut self, msg: &Value, deadline: Instant) -> Result<(), WriteError> {
        let bytes = encode(msg);
        tokio::time::timeout_at(
            deadline,
            write_progressing(&mut self.writer, &bytes, self.stall),
        )
        .await
        .unwrap_or(Err(WriteError::Abandoned))
    }

    /// EOF on the peer's input — bounded too, since shutting a buffered
    /// writer down flushes it.
    async fn close(&mut self) {
        let _ = tokio::time::timeout(STOP_WRITE_GRACE, self.writer.shutdown()).await;
    }
}

/// Run a session: `writer` is the peer's input, `incoming` what
/// [`spawn_reader`] makes of its output. Returns once the session is over;
/// every handle's `alive` is false by then, and every request still waiting
/// has been answered with the protocol's end-of-session text.
///
/// Bounded however the peer behaves: a write it takes none of for
/// [`Protocol::write_stall_limit`] ends the session as
/// [`EndReason::Broken`], and a stop reaches the actor even while a write is
/// blocked. From a stop request to this returning takes at most
/// [`STOP_LIMIT`] plus the goodbye's `reply_wait`.
pub async fn run<W, P>(
    writer: W,
    mut proto: P,
    mailbox: Mailbox,
    mut incoming: mpsc::UnboundedReceiver<Inbound>,
) -> Ended
where
    W: AsyncWrite + Unpin,
    P: Protocol,
{
    let Mailbox {
        rx: mut commands,
        ids,
        stop,
    } = mailbox;
    let mut outbox = Outbox {
        writer,
        stall: proto.write_stall_limit(),
        stop: StopSignal(stop),
        cutoff: None,
    };
    let mut pending = Pending::default();
    let mut waiters = Vec::new();

    let mut reason = loop {
        if let Err(e) = sweep(&mut pending, &mut proto, &mut outbox).await {
            break e.end_reason();
        }
        tokio::select! {
            command = commands.recv() => match command {
                Some(Command::Request { id, method, params, reply }) => {
                    let msg = proto.request(id, &method, params);
                    // Registered BEFORE the write, so an answer can never
                    // arrive for an id this table does not know yet. A failed
                    // write ends the session, which answers it below.
                    pending.insert(id, reply);
                    if let Err(e) = outbox.send(&msg).await {
                        break e.end_reason();
                    }
                }
                Some(Command::Notify { id, method, params }) => {
                    let msg = proto.notification(id, &method, params);
                    if let Err(e) = outbox.send(&msg).await {
                        break e.end_reason();
                    }
                }
                Some(Command::Forget { id }) => {
                    // Only a request still in flight: an answered one needs
                    // no cancel, and a swept one had its own.
                    if pending.forget(id)
                        && let Some(msg) = proto.cancel(id)
                        && let Err(e) = outbox.send(&msg).await
                    {
                        break e.end_reason();
                    }
                }
                Some(Command::Stop { done }) => {
                    waiters.extend(done);
                    break EndReason::Stopped;
                }
                #[cfg(test)]
                Some(Command::PendingCount(tx)) => {
                    if let Err(e) = sweep(&mut pending, &mut proto, &mut outbox).await {
                        break e.end_reason();
                    }
                    let _ = tx.send(pending.len());
                }
                None => break EndReason::Stopped, // every handle dropped
            },
            item = incoming.recv() => match item {
                Some(Ok(msg)) => match proto.inbound(msg, &ids) {
                    Handled::Response { id, result } => {
                        pending.resolve(id, result);
                    }
                    Handled::Reply(reply) => {
                        if let Err(e) = outbox.send(&reply).await {
                            break e.end_reason();
                        }
                    }
                    Handled::Nothing => {}
                    Handled::Close => break EndReason::ConsumerGone,
                },
                Some(Err(e)) => {
                    proto.frame_error(&e);
                    if e.is_fatal() {
                        break EndReason::Broken(e.to_string());
                    }
                }
                None => break EndReason::Closed,
            },
        }
    };

    // Refuse new work at once, so `alive` turns false while the goodbye is
    // still in progress, and settle whatever was already queued.
    commands.close();
    while let Ok(command) = commands.try_recv() {
        match command {
            Command::Stop { done } => waiters.extend(done),
            Command::Request { reply, .. } => {
                let _ = reply.send(Err("the session is ending".into()));
            }
            _ => {}
        }
    }

    if reason == EndReason::Stopped
        && let Some(bye) = proto.goodbye()
        && let Err(e) = goodbye(
            &mut outbox,
            &mut proto,
            &mut pending,
            &mut incoming,
            &ids,
            bye,
        )
        .await
    {
        // A peer that will not take its goodbye is owed no grace period
        // either: the owner kills it at once.
        reason = e.end_reason();
    }
    // EOF on the peer's input: the last word, whatever else happened.
    outbox.close().await;
    let text = proto.ended(&reason);
    pending.fail_all(&text);
    Ended { reason, waiters }
}

/// Release the requests whose callers went away without a word
/// ([`Pending::sweep`]) — a dropped call whose `Forget` is still queued —
/// telling the peer they need not be answered, where the protocol can
/// ([`Protocol::cancel`]).
async fn sweep<W, P>(
    pending: &mut Pending,
    proto: &mut P,
    outbox: &mut Outbox<W>,
) -> Result<(), WriteError>
where
    W: AsyncWrite + Unpin,
    P: Protocol,
{
    for id in pending.sweep() {
        if let Some(msg) = proto.cancel(id) {
            outbox.send(&msg).await?;
        }
    }
    Ok(())
}

/// Send the goodbye request, keep serving the peer until it is answered (or
/// the wait runs out, or the peer goes), then send the closing message. Every
/// write is bounded by the goodbye's clock; `Err` means the peer would not
/// take one — it is not reading at all.
async fn goodbye<W, P>(
    outbox: &mut Outbox<W>,
    proto: &mut P,
    pending: &mut Pending,
    incoming: &mut mpsc::UnboundedReceiver<Inbound>,
    ids: &Ids,
    bye: Goodbye,
) -> Result<(), WriteError>
where
    W: AsyncWrite + Unpin,
    P: Protocol,
{
    // Its input is closed: nothing more can be said, and a peer that closed
    // it is on its way out already — that is no reason to deny it the grace.
    let unwritable = |e: WriteError| match e {
        WriteError::Io(_) => Ok(()),
        e => Err(e),
    };
    let deadline = Instant::now() + bye.reply_wait;
    let id = ids.next();
    let msg = proto.request(id, bye.method, bye.params);
    if let Err(e) = outbox.send_by(&msg, deadline).await {
        return unwritable(e);
    }
    let answer_by = tokio::time::sleep_until(deadline);
    tokio::pin!(answer_by);
    loop {
        tokio::select! {
            () = &mut answer_by => break,
            item = incoming.recv() => match item {
                Some(Ok(msg)) => match proto.inbound(msg, ids) {
                    Handled::Response { id: answered, .. } if answered == id => break,
                    // Requests sent before the goodbye may still be answered.
                    Handled::Response { id, result } => {
                        pending.resolve(id, result);
                    }
                    Handled::Reply(reply) => {
                        if let Err(e) = outbox.send_by(&reply, deadline).await {
                            return unwritable(e);
                        }
                    }
                    Handled::Nothing | Handled::Close => {}
                },
                Some(Err(e)) => {
                    proto.frame_error(&e);
                    if e.is_fatal() {
                        return Ok(());
                    }
                }
                None => return Ok(()), // the peer is already gone
            },
        }
    }
    if let Some((method, params)) = bye.then {
        let msg = proto.notification(ids.next(), method, params);
        if let Err(e) = outbox
            .send_by(&msg, Instant::now() + STOP_WRITE_GRACE)
            .await
        {
            return unwritable(e);
        }
    }
    Ok(())
}

// ------------------------------------------------------------ peer process

/// End a peer process we spawned: give it `grace` to exit on its own (it was
/// just told to), then kill it and reap it.
///
/// `own_group` says the child was spawned as the leader of its own process
/// group (`process_group(0)`). Then everything left in that group is killed
/// too ([`crate::procgroup::kill`]) — whether the leader had to be killed or
/// left by itself, since a server that exits cleanly can still leave helpers
/// behind (a language server's workers, a debug adapter's debuggee). The
/// leader is kept UNREAPED until the group has been killed: an exited child
/// stays a zombie, which keeps its pid — and so the group's id — from being
/// recycled, so the signal can only ever reach the group we created.
///
/// Cancel-safe: dropped part-way (a runtime shutting down), it kills the
/// group on the spot rather than leaving it running.
pub async fn reap(child: tokio::process::Child, grace: Duration, own_group: bool) {
    #[cfg(unix)]
    if own_group && let Some(pid) = child.id() {
        let mut leader = GroupLeader {
            child,
            pid,
            signalable: true,
        };
        leader.exited_within(grace).await;
        leader.kill_group_async().await;
        let _ = leader.child.start_kill();
        let _ = leader.child.wait().await;
        return;
    }
    #[cfg(not(unix))]
    let _ = own_group;
    let mut child = child;
    if !grace.is_zero() && tokio::time::timeout(grace, child.wait()).await.is_ok() {
        return; // exited by itself
    }
    let _ = child.kill().await;
}

/// Whether `child` — spawned as the leader of a process group of its own
/// (`process_group(0)`) — has exited; if it has, SIGKILL whatever is left in
/// its group ([`crate::procgroup::kill`]), THEN reap it and return its
/// status. `Ok(None)` while it runs.
///
/// For a peer that ends BY ITSELF. [`reap`] sweeps the group of a peer clew
/// stops; one that crashed or quit on its own was reaped and its group left
/// running — a language server's `cargo check`, an adapter's debuggee. The
/// group is killed while the leader is still an unreaped zombie holding its
/// pid, and so the group's id, exactly as [`reap`] does: once reaped, that
/// id is free for someone else's group.
///
/// Not async, so it can be asked under a lock. The kill blocks while the
/// members die: not at all when none is left alive, about a millisecond
/// when some are (at most [`crate::procgroup::KILL_BOUND`]).
pub fn try_wait_sweeping(
    child: &mut tokio::process::Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        match leader_state(pid) {
            LeaderState::Running => return Ok(None),
            // The leader is our unreaped child, so `pid` still names the
            // group it created.
            LeaderState::Exited => crate::procgroup::kill(pid),
            // Not waitable by us: nothing may be signalled; tokio says what
            // it knows.
            LeaderState::Unknown => {}
        }
    }
    child.try_wait()
}

/// [`try_wait_sweeping`], waiting for the exit (polled with a backoff; the
/// caller bounds the wait, and dropping the future mid-way leaves nothing
/// behind).
pub async fn wait_sweeping(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    let mut delay = Duration::from_millis(5);
    loop {
        if let Some(status) = try_wait_sweeping(child)? {
            return Ok(status);
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(100));
    }
}

/// The unreaped leader of a process group we spawned (see [`reap`]).
#[cfg(unix)]
struct GroupLeader {
    child: tokio::process::Child,
    pid: u32,
    /// Cleared if the leader turns out to have been reaped behind our back:
    /// its pid may then already belong to someone else.
    signalable: bool,
}

#[cfg(unix)]
impl GroupLeader {
    /// Wait (at most `limit`) for the leader to exit, WITHOUT reaping it.
    async fn exited_within(&mut self, limit: Duration) {
        let deadline = Instant::now() + limit;
        let mut delay = Duration::from_millis(5);
        loop {
            match leader_state(self.pid) {
                LeaderState::Running => {}
                LeaderState::Exited => return,
                LeaderState::Unknown => {
                    self.signalable = false;
                    return;
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            tokio::time::sleep(delay.min(deadline - now)).await;
            delay = (delay * 2).min(Duration::from_millis(100));
        }
    }

    /// The leader's pid, which names its group, while that group is still
    /// ours to signal: the leader is unreaped (`id()` turns `None` once tokio
    /// has reaped it) and was not reaped behind our back.
    fn group(&self) -> Option<u32> {
        (self.signalable && self.child.id().is_some()).then_some(self.pid)
    }

    /// SIGKILL everything in the group, the leader included, until none of
    /// it can run on ([`crate::procgroup::kill`]). Blocking, as a drop is.
    fn kill_group(&self) {
        if let Some(leader) = self.group() {
            crate::procgroup::kill(leader);
        }
    }

    /// [`Self::kill_group`], leaving the thread to other tasks between
    /// rounds. Dropped part-way, the leader's drop finishes the job.
    async fn kill_group_async(&self) {
        if let Some(leader) = self.group() {
            crate::procgroup::kill_async(leader).await;
        }
    }
}

#[cfg(unix)]
impl Drop for GroupLeader {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// What `waitid` says about a child, without reaping it — git's kill asks
/// too (`crate::git`), before it signals a group.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeaderState {
    Running,
    /// Exited, and still waitable: a zombie holding its pid.
    Exited,
    /// Not waitable at all — reaped by someone else.
    Unknown,
}

#[cfg(unix)]
pub(crate) fn leader_state(pid: u32) -> LeaderState {
    loop {
        // SAFETY: `siginfo_t` is plain data, for which all-zero is a valid
        // value; `waitid` only writes into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: plain syscall on our own child. `WNOWAIT` leaves an exited
        // child waitable, so nothing is reaped here; `WNOHANG` never blocks.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: reads the field `waitid` fills in (still zero when no
            // child has changed state).
            return if unsafe { info.si_pid() } == 0 {
                LeaderState::Running
            } else {
                LeaderState::Exited
            };
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return LeaderState::Unknown;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A protocol frame's cap is on its content: a line of exactly the cap
    /// (plus its newline) is read — it is the largest frame a writer
    /// refusing `len > cap` sends — one byte more ends the stream (no resync
    /// past it, whatever follows), and frames under the cap read in order.
    #[tokio::test]
    async fn a_protocol_line_is_capped_on_its_content() {
        let read_all = |bytes: Vec<u8>| async move {
            let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(bytes));
            let mut lines = Vec::new();
            while let Some(line) = read_line_capped(&mut reader, 8).await {
                lines.push(line);
            }
            lines
        };
        assert_eq!(
            read_all(b"ab\n12345678\ncd".to_vec()).await,
            ["ab", "12345678", "cd"]
        );
        assert_eq!(
            read_all(b"ab\n123456789\nlater\n".to_vec()).await,
            ["ab"],
            "an over-cap line must end the stream"
        );
        assert!(read_all(Vec::new()).await.is_empty());
        let mut real = tokio::io::BufReader::new(std::io::Cursor::new(b"{\"id\":1}\n".to_vec()));
        assert_eq!(
            read_protocol_line(&mut real).await.as_deref(),
            Some("{\"id\":1}")
        );
    }

    /// One line of `left` bytes with no newline, generated as it is read — a
    /// source of any length that costs nothing to hold — and then the end of
    /// the stream: a reader that lost its cap reads to that end and fails the
    /// test's assertions, instead of reading forever.
    struct LongLine {
        left: usize,
    }

    impl tokio::io::AsyncRead for LongLine {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let n = buf.remaining().min(64 * 1024).min(self.left);
            buf.put_slice(&[b'x'; 64 * 1024][..n]);
            self.left -= n;
            Poll::Ready(Ok(()))
        }
    }

    /// The reader BOTH ends of a connection call — the client's transport
    /// and the server's request loop use it directly, no private copy of
    /// their own — reads frames in order to EOF, and refuses a line past the
    /// protocol's cap ([`clew_protocol::MAX_FRAME_BYTES`]) instead of growing
    /// memory without bound: it stops reading one byte past the cap. The line
    /// is generated as it is read, so the test holds the cap's worth of it
    /// (256 MiB) once — not a source that size as well — and it ends a MiB
    /// past the cap, so a reader that lost its cap fails the assertions
    /// below rather than exhausting memory.
    #[tokio::test]
    async fn the_protocol_reader_refuses_a_line_past_the_frame_cap() {
        let mut ok =
            tokio::io::BufReader::new(std::io::Cursor::new(b"{\"id\":1}\nnext\n".to_vec()));
        assert_eq!(
            read_protocol_line(&mut ok).await.as_deref(),
            Some("{\"id\":1}")
        );
        assert_eq!(read_protocol_line(&mut ok).await.as_deref(), Some("next"));
        assert_eq!(read_protocol_line(&mut ok).await, None, "EOF");

        let cap = clew_protocol::MAX_FRAME_BYTES;
        let length = cap + (1 << 20);
        const AHEAD: usize = 8 * 1024;
        let mut over = tokio::io::BufReader::with_capacity(AHEAD, LongLine { left: length });
        assert_eq!(read_protocol_line(&mut over).await, None);
        // What the reader took: at most one byte past the cap, plus what its
        // buffer fetched ahead.
        let taken = length - over.get_ref().left;
        assert!(
            taken <= cap + 1 + AHEAD,
            "the reader took {taken} bytes of a line capped at {cap}"
        );
    }
    use serde_json::json;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// A reader that hands out at most one byte per read and says "not yet"
    /// on every other poll: the most fragmented delivery a pipe can produce.
    struct Trickle {
        data: Vec<u8>,
        at: usize,
        stall: bool,
    }

    impl Trickle {
        fn new(data: &[u8]) -> Self {
            Trickle {
                data: data.to_vec(),
                at: 0,
                stall: true,
            }
        }
    }

    impl AsyncRead for Trickle {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.stall {
                self.stall = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.stall = true;
            if self.at < self.data.len() && buf.remaining() > 0 {
                let byte = self.data[self.at];
                buf.put_slice(&[byte]);
                self.at += 1;
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn frames_of(bytes: &[u8]) -> Vec<Inbound> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        reader_loop(BufReader::new(Trickle::new(bytes)), tx).await;
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            out.push(item);
        }
        out
    }

    async fn one(bytes: &[u8]) -> Result<Option<Vec<u8>>, FrameError> {
        read_frame(&mut BufReader::new(Trickle::new(bytes))).await
    }

    #[tokio::test]
    async fn frames_survive_byte_by_byte_delivery() {
        let mut wire = encode(&json!({"id": 1, "text": "héllo — ✓"}));
        wire.extend(encode(&json!({"id": 2})));
        let got = frames_of(&wire).await;
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].as_ref().unwrap()["text"], "héllo — ✓");
        assert_eq!(got[1].as_ref().unwrap()["id"], 2);
    }

    /// `Content-Length` counts bytes, not characters: a body with multi-byte
    /// text framed by its char count would cut the message short.
    #[test]
    fn encode_counts_bytes() {
        let wire = encode(&json!("é"));
        let text = String::from_utf8(wire).unwrap();
        assert!(text.starts_with("Content-Length: 4\r\n\r\n"), "{text:?}");
    }

    #[tokio::test]
    async fn header_names_are_case_insensitive_and_lf_is_accepted() {
        for header in [
            "content-length: 2\r\n\r\n{}",
            "CONTENT-LENGTH:2\r\n\r\n{}",
            "Content-Length: 2\n\n{}",
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}",
            // A blank separator ahead of the headers is skipped.
            "\r\nContent-Length: 2\r\n\r\n{}",
            // An identical repeat is not a conflict.
            "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        ] {
            assert_eq!(
                one(header.as_bytes()).await.unwrap(),
                Some(b"{}".to_vec()),
                "{header:?}"
            );
        }
    }

    /// A malformed length used to parse as 0 and be skipped, leaving the
    /// reader to take the body's first line for the next header — reading
    /// every later frame from the wrong offset. Now it ends the transport.
    #[tokio::test]
    async fn a_malformed_length_is_fatal() {
        for value in [
            "abc",
            "-1",
            "+5",
            "",
            "12a",
            "1 2",
            "0x10",
            "99999999999999999999999",
        ] {
            let wire = format!("Content-Length: {value}\r\n\r\n{{}}");
            let err = one(wire.as_bytes()).await.unwrap_err();
            assert!(
                matches!(err, FrameError::InvalidContentLength(_)),
                "{value:?} gave {err:?}"
            );
            assert!(err.is_fatal());
        }
    }

    type Check = fn(&FrameError) -> bool;

    #[tokio::test]
    async fn missing_conflicting_or_malformed_headers_are_fatal() {
        let cases: [(&str, Check); 5] = [
            ("Content-Type: x\r\n\r\n{}", |e| {
                matches!(e, FrameError::MissingContentLength)
            }),
            ("Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}", |e| {
                matches!(e, FrameError::ConflictingContentLength)
            }),
            ("not a header\r\n\r\n{}", |e| {
                matches!(e, FrameError::MalformedHeader(_))
            }),
            ("Content-Length : 2\r\n\r\n{}", |e| {
                matches!(e, FrameError::MalformedHeader(_))
            }),
            ("Cöntent-Length: 2\r\n\r\n{}", |e| {
                matches!(e, FrameError::MalformedHeader(_))
            }),
        ];
        for (wire, expected) in cases {
            let err = one(wire.as_bytes()).await.unwrap_err();
            assert!(expected(&err), "{wire:?} gave {err:?}");
            assert!(err.is_fatal());
        }
    }

    #[tokio::test]
    async fn oversized_headers_and_bodies_are_refused_before_reading_them() {
        let long = format!("X-Junk: {}\r\n", "a".repeat(MAX_HEADER_LINE_BYTES));
        assert!(matches!(
            one(long.as_bytes()).await,
            Err(FrameError::HeaderLineTooLong)
        ));

        let many = "X-Junk: 1\r\n".repeat(MAX_HEADER_LINES + 1);
        assert!(matches!(
            one(many.as_bytes()).await,
            Err(FrameError::TooManyHeaderLines)
        ));

        // Refused from the header alone: none of the (absent) body is read,
        // and nothing near the announced size is allocated.
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        assert!(matches!(
            one(huge.as_bytes()).await,
            Err(FrameError::TooLarge(n)) if n == MAX_FRAME_BYTES as u64 + 1
        ));
    }

    #[tokio::test]
    async fn eof_is_clean_between_frames_and_truncation_inside_one() {
        assert!(matches!(one(b"").await, Ok(None)));
        assert!(matches!(one(b"\r\n").await, Ok(None)));
        for cut in [
            &b"Content-Length: 10\r\n"[..],
            b"Content-Length: 10\r\n\r\n{\"a\":",
            b"Content-Len",
        ] {
            assert!(
                matches!(one(cut).await, Err(FrameError::Truncated)),
                "{:?}",
                String::from_utf8_lossy(cut)
            );
        }
    }

    /// A body that is well framed but not JSON is that message's problem
    /// alone: it is reported, and the next frame still arrives.
    #[tokio::test]
    async fn a_non_json_body_is_skipped_and_the_stream_stays_in_sync() {
        let mut wire = b"Content-Length: 9\r\n\r\nnot json!".to_vec();
        wire.extend(encode(&json!({"after": true})));
        let got = frames_of(&wire).await;
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(matches!(&got[0], Err(FrameError::InvalidJson(_))));
        assert!(!got[0].as_ref().unwrap_err().is_fatal());
        assert_eq!(got[1].as_ref().unwrap()["after"], true);
    }

    /// A fatal error is delivered as the last item and ends the reader, so
    /// the actor learns WHY the transport ended rather than just that it did.
    #[tokio::test]
    async fn the_reader_ends_with_the_fatal_error() {
        let mut wire = encode(&json!({"first": 1}));
        wire.extend(b"Content-Length: nope\r\n\r\n{}");
        wire.extend(encode(&json!({"never": 1})));
        let got = frames_of(&wire).await;
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].as_ref().unwrap()["first"], 1);
        assert!(matches!(&got[1], Err(FrameError::InvalidContentLength(_))));
    }

    // ---------------------------------------------------------- actor tests

    /// A minimal JSON-RPC-shaped protocol for driving [`run`].
    struct Echo {
        log: Arc<std::sync::Mutex<Vec<String>>>,
        bye: bool,
        stall: Duration,
    }

    impl Protocol for Echo {
        fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
            json!({"id": id, "method": method, "params": params})
        }
        fn notification(&mut self, _id: i64, method: &str, params: Value) -> Value {
            json!({"method": method, "params": params})
        }
        fn inbound(&mut self, msg: Value, _ids: &Ids) -> Handled {
            match (msg.get("id").and_then(Value::as_i64), msg.get("method")) {
                (Some(id), None) => Handled::Response {
                    id,
                    result: Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                },
                (Some(id), Some(_)) => Handled::Reply(json!({"id": id, "result": "ack"})),
                _ => Handled::Nothing,
            }
        }
        fn frame_error(&mut self, error: &FrameError) {
            self.log.lock().unwrap().push(format!("frame: {error}"));
        }
        fn goodbye(&mut self) -> Option<Goodbye> {
            self.bye.then(|| Goodbye {
                method: "shutdown",
                params: Value::Null,
                reply_wait: Duration::from_millis(300),
                then: Some(("exit", Value::Null)),
            })
        }
        fn write_stall_limit(&self) -> Duration {
            self.stall
        }
        fn cancel(&mut self, id: i64) -> Option<Value> {
            Some(json!({"method": "cancel", "params": {"id": id}}))
        }
        fn ended(&mut self, reason: &EndReason) -> String {
            self.log.lock().unwrap().push(format!("ended: {reason:?}"));
            format!("session ended: {reason:?}")
        }
    }

    /// The next message the peer reads, waited for 5 s at most: one that
    /// does not come fails the test instead of stalling it.
    async fn read_within(peer_in: &mut BufReader<tokio::io::DuplexStream>) -> Value {
        let read = tokio::time::timeout(Duration::from_secs(5), read_message(peer_in)).await;
        read.expect("no message within 5 s").unwrap().unwrap()
    }

    /// What the test protocol recorded.
    type Log = Arc<std::sync::Mutex<Vec<String>>>;
    /// The actor's handle and task, the peer's ends of the pipes, the log.
    type Session = (
        Handle,
        tokio::task::JoinHandle<Ended>,
        BufReader<tokio::io::DuplexStream>,
        tokio::io::DuplexStream,
        Log,
    );

    /// An actor over in-memory pipes, plus the peer's ends of them.
    fn session(bye: bool) -> Session {
        session_with(bye, 1 << 16, WRITE_STALL_LIMIT)
    }

    /// [`session`] with the peer's input pipe holding only `input` bytes (a
    /// peer that does not read blocks the actor's writes past that), and a
    /// stall limit of `stall`.
    fn session_with(bye: bool, input: usize, stall: Duration) -> Session {
        let (client_out, peer_in) = tokio::io::duplex(input);
        let (peer_out, client_in) = tokio::io::duplex(1 << 16);
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (handle, mailbox) = mailbox();
        let incoming = spawn_reader(client_in);
        let proto = Echo {
            log: log.clone(),
            bye,
            stall,
        };
        let actor = tokio::spawn(run(client_out, proto, mailbox, incoming));
        (handle, actor, BufReader::new(peer_in), peer_out, log)
    }

    #[tokio::test]
    async fn calls_are_answered_and_peer_requests_are_replied_to() {
        let (handle, _actor, mut peer_in, mut peer_out, _log) = session(false);
        let call = tokio::spawn({
            let handle = handle.clone();
            async move { handle.call("ping", json!({}), Duration::from_secs(5)).await }
        });
        let req = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(req["method"], "ping");
        // A request of the peer's own, then the answer to ours.
        write_frame(&mut peer_out, &json!({"id": 77, "method": "hello"}))
            .await
            .unwrap();
        write_frame(&mut peer_out, &json!({"id": req["id"], "result": "pong"}))
            .await
            .unwrap();
        assert_eq!(call.await.unwrap(), Ok(json!("pong")));
        let reply = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(reply, json!({"id": 77, "result": "ack"}));
    }

    /// A request the peer never answers is released on timeout, not held for
    /// the session's lifetime (the DAP client used to keep every one): the
    /// timed-out call tells the actor to forget it. Driven against a bare
    /// channel, so the only thing that can release the entry is that message
    /// — the sweep, which removes entries whose caller went away, would have
    /// hidden a missing `Forget` in a live session.
    #[tokio::test]
    async fn a_call_that_times_out_tells_the_actor_to_forget_it() {
        let (tx, mut commands) = mpsc::unbounded_channel();
        let (stop, _stop) = watch::channel(false);
        let handle = Handle {
            tx,
            ids: Ids::default(),
            stop,
        };
        let call = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .call("silence", json!({}), Duration::from_millis(50))
                    .await
            }
        });
        // Played by hand: the "actor" holds the reply and never answers.
        let Some(Command::Request { id, reply, .. }) = commands.recv().await else {
            panic!("the call must queue its request first");
        };
        assert_eq!(call.await.unwrap(), Err(CallError::TimedOut));
        match commands.recv().await {
            Some(Command::Forget { id: forgotten }) => assert_eq!(forgotten, id),
            _ => panic!("a timed-out call must send Forget for its own id"),
        }
        drop(reply);
    }

    /// `Forget` releases an entry even while its caller is still waiting —
    /// the one case the sweep never touches.
    #[tokio::test]
    async fn forget_releases_an_entry_whose_caller_still_waits() {
        let (handle, _actor, mut peer_in, _peer_out, _log) = session(false);
        let (reply, mut answer) = oneshot::channel();
        handle
            .tx
            .send(Command::Request {
                id: 99,
                method: "silence".into(),
                params: json!({}),
                reply,
            })
            .unwrap();
        let _ = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(handle.pending_count().await, Some(1), "the caller waits");
        handle.tx.send(Command::Forget { id: 99 }).unwrap();
        assert_eq!(handle.pending_count().await, Some(0));
        assert!(
            matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Closed)),
            "a forgotten request is released unanswered"
        );
    }

    /// A caller whose future is simply dropped has its entry released, and
    /// the request cancelled at the peer, once: by the `Forget` its call
    /// sends as it drops, or — a caller gone before that is read — by the
    /// sweep.
    #[tokio::test]
    async fn a_dropped_caller_is_swept() {
        let (handle, _actor, mut peer_in, _peer_out, _log) = session(false);
        let dropped = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .call("dropped", json!({}), Duration::from_secs(60))
                    .await
            }
        });
        let asked = read_within(&mut peer_in).await;
        assert_eq!(handle.pending_count().await, Some(1));
        dropped.abort();
        let _ = dropped.await;
        assert_eq!(handle.pending_count().await, Some(0));
        let cancel = read_within(&mut peer_in).await;
        assert_eq!(
            cancel,
            json!({"method": "cancel", "params": {"id": asked["id"]}})
        );

        // A caller that says nothing as it goes: the sweep cancels it.
        let (reply, answer) = oneshot::channel();
        handle
            .tx
            .send(Command::Request {
                id: 99,
                method: "silent".into(),
                params: json!({}),
                reply,
            })
            .unwrap();
        let _ = read_within(&mut peer_in).await;
        drop(answer);
        assert_eq!(handle.pending_count().await, Some(0));
        let cancel = read_within(&mut peer_in).await;
        assert_eq!(cancel, json!({"method": "cancel", "params": {"id": 99}}));
        // Forgotten too late — swept already — it is not cancelled twice.
        handle.tx.send(Command::Forget { id: 99 }).unwrap();
        handle.notify("after", json!({}));
        let next = read_within(&mut peer_in).await;
        assert_eq!(next["method"], "after", "cancelled twice: {next}");
    }

    /// A fatal framing error ends the session WITH the reason: requests in
    /// flight fail with it at once instead of waiting out their timeouts, and
    /// every handle reports the session dead.
    #[tokio::test]
    async fn a_broken_transport_fails_pending_requests_with_its_reason() {
        let (handle, actor, mut peer_in, mut peer_out, log) = session(false);
        let call = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .call("anything", json!({}), Duration::from_secs(30))
                    .await
            }
        });
        let _ = read_message(&mut peer_in).await.unwrap().unwrap();
        peer_out
            .write_all(b"Content-Length: twelve\r\n\r\n{}")
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("must fail promptly, not time out")
            .unwrap()
            .unwrap_err();
        match err {
            CallError::Failed(failure) => {
                assert!(failure.message.contains("Broken"), "{failure:?}")
            }
            other => panic!("expected the end reason, got {other:?}"),
        }
        let ended = actor.await.unwrap();
        assert!(matches!(ended.reason, EndReason::Broken(ref e) if e.contains("Content-Length")));
        assert!(!handle.alive());
        assert!(log.lock().unwrap().iter().any(|l| l.starts_with("frame:")));
    }

    /// Stopping sends the goodbye request, waits for its answer, then sends
    /// the closing message — and the waiter hears about it only after.
    #[tokio::test]
    async fn stop_says_goodbye_in_order() {
        let (handle, actor, mut peer_in, mut peer_out, _log) = session(true);
        let stopper = tokio::spawn({
            let handle = handle.clone();
            async move { handle.stop_and_wait(Duration::from_secs(5)).await }
        });
        let shutdown = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(shutdown["method"], "shutdown");
        write_frame(
            &mut peer_out,
            &json!({"id": shutdown["id"], "result": null}),
        )
        .await
        .unwrap();
        let exit = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(exit["method"], "exit");
        assert!(exit.get("id").is_none(), "exit is a notification");
        let ended = actor.await.unwrap();
        assert_eq!(ended.reason, EndReason::Stopped);
        assert!(!handle.alive());
        ended.release();
        stopper.await.unwrap();
        // Our side of the pipe is closed after the goodbye.
        assert!(matches!(read_message(&mut peer_in).await, Ok(None)));
    }

    /// A peer that never answers the goodbye still gets the closing message,
    /// after the bounded wait.
    #[tokio::test]
    async fn an_unanswered_goodbye_is_bounded() {
        let (handle, actor, mut peer_in, _peer_out, _log) = session(true);
        drop(handle); // every handle gone: the same path as `stop`
        let started = std::time::Instant::now();
        let shutdown = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(shutdown["method"], "shutdown");
        let exit = read_message(&mut peer_in).await.unwrap().unwrap();
        assert_eq!(exit["method"], "exit");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(actor.await.unwrap().reason, EndReason::Stopped);
    }

    /// No goodbye when the peer is the one that left.
    #[tokio::test]
    async fn a_closed_peer_gets_no_goodbye() {
        let (handle, actor, _peer_in, peer_out, _log) = session(true);
        drop(peer_out);
        let ended = actor.await.unwrap();
        assert_eq!(ended.reason, EndReason::Closed);
        assert!(!handle.alive());
    }

    #[test]
    fn pending_resolves_forgets_and_fails_the_rest() {
        let mut pending = Pending::default();
        let (a, mut ra) = oneshot::channel();
        let (b, mut rb) = oneshot::channel();
        let (c, rc) = oneshot::channel();
        pending.insert(1, a);
        pending.insert(2, b);
        pending.insert(3, c);
        assert!(pending.resolve(1, Ok(json!(1))));
        assert!(!pending.resolve(1, Ok(json!(1))), "answered twice");
        assert!(pending.forget(2));
        drop(rc);
        pending.sweep();
        assert!(pending.is_empty());
        assert_eq!(ra.try_recv().unwrap(), Ok(json!(1)));
        assert!(rb.try_recv().is_err(), "a forgotten request gets no answer");

        let (d, mut rd) = oneshot::channel();
        pending.insert(4, d);
        pending.fail_all("gone");
        assert_eq!(rd.try_recv().unwrap(), Err(Failure::from("gone")));
    }

    // --------------------------------------------------- unwritable peers

    /// F4/F2: a peer that stops reading its input used to wedge the actor in
    /// its write, where the `Stop` behind it was never seen — the stop never
    /// completed, the peer was never reaped, and `alive` stayed true. The stop
    /// now reaches the blocked write and ends it within the grace.
    #[tokio::test]
    async fn a_peer_that_stops_reading_cannot_hold_up_a_stop() {
        let (handle, actor, _peer_in, _peer_out, _log) =
            session_with(true, 1024, WRITE_STALL_LIMIT);
        // Far more than the pipe holds, to a peer that never reads.
        handle.notify("didOpen", json!({"text": "x".repeat(256 * 1024)}));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            handle.alive(),
            "stuck, not dead: the stall limit is far off"
        );

        let started = std::time::Instant::now();
        let stopper = tokio::spawn({
            let handle = handle.clone();
            async move { handle.stop_and_wait(Duration::from_secs(30)).await }
        });
        let ended = tokio::time::timeout(Duration::from_secs(5), actor)
            .await
            .expect("the stop must get past the blocked write")
            .unwrap();
        // Within the grace (plus slack for a loaded machine), where the write
        // it got past would otherwise have run to the 30 s stall limit.
        assert!(
            started.elapsed() < STOP_WRITE_GRACE + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        // Broken, so the owner kills the peer without a grace period — and
        // no goodbye was attempted into a pipe nobody reads.
        assert!(
            matches!(&ended.reason, EndReason::Broken(e) if e.contains("not reading")),
            "{:?}",
            ended.reason
        );
        assert!(!handle.alive());
        ended.release();
        tokio::time::timeout(Duration::from_secs(1), stopper)
            .await
            .expect("the stop's waiter is released")
            .unwrap();
    }

    /// Nobody has to ask: a write the peer takes none of for the stall limit
    /// ends the session by itself, and the request that could not be
    /// delivered fails with the reason at once.
    #[tokio::test]
    async fn a_peer_that_stops_reading_is_found_out() {
        let (handle, actor, _peer_in, _peer_out, _log) =
            session_with(false, 1024, Duration::from_millis(200));
        let call = tokio::spawn({
            let handle = handle.clone();
            async move {
                let big = json!({"text": "x".repeat(256 * 1024)});
                handle.call("big", big, Duration::from_secs(30)).await
            }
        });
        let ended = tokio::time::timeout(Duration::from_secs(5), actor)
            .await
            .expect("a stalled write must end the session")
            .unwrap();
        assert!(
            matches!(&ended.reason, EndReason::Broken(e) if e.contains("stopped reading")),
            "{:?}",
            ended.reason
        );
        assert!(!handle.alive());
        match call.await.unwrap() {
            Err(CallError::Failed(failure)) => {
                assert!(failure.message.contains("stopped reading"), "{failure:?}")
            }
            other => panic!("expected the end reason, got {other:?}"),
        }
    }

    /// The stall clock measures progress, not the whole message: a reader
    /// that takes a little at a time is slow, not stuck, however long the
    /// message takes in total.
    #[tokio::test]
    async fn a_slow_reader_is_not_a_stalled_one() {
        use tokio::io::AsyncReadExt;
        let (handle, _actor, mut peer_in, _peer_out, _log) =
            session_with(false, 1024, Duration::from_millis(200));
        let text = "y".repeat(16 * 1024);
        let expected = encode(&json!({"method": "didOpen", "params": {"text": text}}));
        handle.notify("didOpen", json!({"text": text}));
        // 1 KiB every 25 ms: about 400 ms in all, twice the stall limit, but
        // no wait for progress comes near it.
        let mut wire = Vec::new();
        while wire.len() < expected.len() {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let mut chunk = [0u8; 1024];
            let n = tokio::time::timeout(Duration::from_secs(5), peer_in.read(&mut chunk))
                .await
                .expect("the actor keeps writing")
                .unwrap();
            assert!(n > 0, "the actor closed the stream");
            wire.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(wire, expected);
        assert!(handle.alive());
    }

    /// The goodbye is bounded too: a peer that will not take the `shutdown`
    /// request is given up on at the goodbye's deadline, and — having shown
    /// it is not reading — ends as Broken, so it is killed without a grace.
    #[tokio::test]
    async fn a_goodbye_the_peer_will_not_take_is_bounded() {
        let fill = |n: usize| json!({"text": "z".repeat(n)});
        let frame = encode(&json!({"method": "fill", "params": fill(900)}));
        // A pipe the first message fills exactly: it goes through, and the
        // goodbye behind it has nowhere to go.
        let (handle, actor, _peer_in, _peer_out, _log) =
            session_with(true, frame.len(), WRITE_STALL_LIMIT);
        handle.notify("fill", fill(900));
        let started = std::time::Instant::now();
        handle.stop();
        let ended = tokio::time::timeout(Duration::from_secs(5), actor)
            .await
            .expect("the goodbye must be bounded")
            .unwrap();
        // The goodbye's reply_wait (300 ms) bounds it, plus slack for a
        // loaded machine.
        assert!(
            started.elapsed() < Duration::from_millis(300) + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(&ended.reason, EndReason::Broken(e) if e.contains("not reading")),
            "{:?}",
            ended.reason
        );
    }

    /// A write that fails (the peer closed its input) ends the session: a
    /// frame cut off part-way leaves nothing after it readable.
    #[tokio::test]
    async fn a_failed_write_ends_the_session() {
        let (handle, actor, peer_in, _peer_out, _log) = session(false);
        drop(peer_in);
        let got = handle.call("x", json!({}), Duration::from_secs(5)).await;
        assert!(
            matches!(&got, Err(CallError::Failed(f)) if f.message.contains("writing to it failed")),
            "{got:?}"
        );
        let ended = actor.await.unwrap();
        assert!(
            matches!(&ended.reason, EndReason::Broken(e) if e.contains("writing to it failed")),
            "{:?}",
            ended.reason
        );
        assert!(!handle.alive());
    }

    // ------------------------------------------------------------- reaping

    /// A fresh directory for one test, removed when it ends.
    #[cfg(unix)]
    fn scratch(tag: &str) -> crate::testutil::TempDir {
        crate::testutil::TempDir::new(&format!("framing-{tag}"))
    }

    /// Start `/bin/sh -c script` as the leader of its own process group; the
    /// script writes its helper's pid to `$1`. Returns the leader and the
    /// helper's pid, once it is all there (`echo` creates the file before it
    /// writes the pid) — waited for up to a minute, for a busy machine: only
    /// the script getting going, which nothing here times.
    #[cfg(unix)]
    async fn group_with_helper(
        dir: &std::path::Path,
        script: &str,
    ) -> (tokio::process::Child, libc::pid_t) {
        let pidfile = dir.join("helper.pid");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(script)
            .arg("sh")
            .arg(&pidfile)
            .kill_on_drop(true)
            .process_group(0);
        let child = cmd.spawn().unwrap();
        for _ in 0..6000 {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .as_deref()
                .and_then(|s| s.strip_suffix('\n'))
                .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                return (child, pid);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the helper never started");
    }

    /// Whether `pid` is gone (within a couple of seconds).
    #[cfg(unix)]
    async fn gone(pid: libc::pid_t) -> bool {
        for _ in 0..200 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// The hard kill takes the whole process group: a helper the peer
    /// started (a server's worker, an adapter's debuggee) must not outlive
    /// it.
    #[tokio::test]
    #[cfg(unix)]
    async fn reap_kills_the_process_group() {
        let dir = scratch("reap");
        let (child, helper) = group_with_helper(&dir, "sleep 30 & echo $! > \"$1\"; wait").await;
        reap(child, Duration::from_millis(50), true).await;
        let helper_gone = gone(helper).await;
        assert!(helper_gone, "the helper outlived its group's leader");
    }

    /// Whether group `pgid`, its leader reaped, is left with nothing alive
    /// within 10 s: a helper the kill missed would live on for 30. Waited
    /// for, because the helpers' zombies are the system's to reap as
    /// orphans, and Linux still finds them until then.
    #[cfg(unix)]
    async fn group_gone(pgid: libc::pid_t) -> bool {
        for _ in 0..1000 {
            // SAFETY: signal 0 only probes; the group is this test's own.
            if unsafe { libc::killpg(pgid, 0) } != 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // SAFETY: as above; the survivors are this test's helpers.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
        false
    }

    /// The hard kill takes a group that is forking as it comes, too. One
    /// SIGKILL to the group did not: a child forked as it was sent could
    /// join the group after the kernel went through it, and ran on — a
    /// language server's worker, an adapter's debuggee, outliving the stop.
    /// At a different point of the forking each time.
    #[tokio::test]
    #[cfg(unix)]
    async fn reap_kills_a_group_that_is_forking() {
        for attempt in 0..8u64 {
            let dir = scratch(&format!("reap-forking-{attempt}"));
            // The leader records its own pid, the group's id, then starts
            // helpers back to back: a bounded number of them, whatever
            // becomes of the reap.
            let (child, pgid) = group_with_helper(
                &dir,
                "echo $$ > \"$1\"; for _ in $(seq 300); do sleep 30 & done; wait",
            )
            .await;
            tokio::time::sleep(Duration::from_millis(2 + 3 * attempt)).await;
            reap(child, Duration::ZERO, true).await;
            assert!(
                group_gone(pgid).await,
                "a helper forked as its group was reaped outlived it (attempt {attempt})"
            );
        }
    }

    /// A leader that exits by itself within the grace used to be reaped
    /// straight away — after which its group could no longer be signalled
    /// safely, so what it left behind ran on. The leader is now held as a
    /// zombie until the group has been swept, and the grace is not waited out.
    #[tokio::test]
    #[cfg(unix)]
    async fn reap_sweeps_the_group_when_the_leader_exits_by_itself() {
        let dir = scratch("reap-exit");
        let (child, helper) = group_with_helper(&dir, "sleep 30 & echo $! > \"$1\"; exit 0").await;
        let started = std::time::Instant::now();
        reap(child, Duration::from_secs(10), true).await;
        let elapsed = started.elapsed();
        let helper_gone = gone(helper).await;
        assert!(elapsed < Duration::from_secs(5), "the grace was waited out");
        assert!(helper_gone, "the helper outlived a leader that exited");
    }

    /// A peer that exits BY ITSELF — nobody stopping it — has its group swept
    /// by the wait that notices, before the leader is reaped (a crashed
    /// language server's `cargo check` used to run on); the status is still
    /// the leader's own. While the leader runs, the probe neither reaps nor
    /// signals anything.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_leader_that_exits_on_its_own_is_reaped_with_its_group() {
        let dir = scratch("sweep-exit");
        let (mut child, helper) =
            group_with_helper(&dir, "sleep 30 & echo $! > \"$1\"; exit 3").await;
        let status = tokio::time::timeout(Duration::from_secs(10), wait_sweeping(&mut child))
            .await
            .expect("the leader exits")
            .unwrap();
        assert_eq!(status.code(), Some(3));
        assert!(child.id().is_none(), "the leader is reaped");
        assert!(
            gone(helper).await,
            "the helper outlived a leader that exited"
        );

        let dir = scratch("sweep-running");
        let (mut running, sibling) =
            group_with_helper(&dir, "sleep 30 & echo $! > \"$1\"; wait").await;
        assert!(try_wait_sweeping(&mut running).unwrap().is_none());
        // SAFETY: signal 0 only probes for existence.
        assert_eq!(
            unsafe { libc::kill(sibling, 0) },
            0,
            "nothing was signalled"
        );
        reap(running, Duration::ZERO, true).await;
        assert!(gone(sibling).await);
    }

    /// Dropped part-way — the runtime shutting down while a peer is still
    /// in its grace — the reap kills the group on the spot instead of leaving
    /// the leader to `kill_on_drop` and its helpers to run on.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_cancelled_reap_still_kills_the_group() {
        let dir = scratch("reap-cancel");
        let (child, helper) = group_with_helper(&dir, "sleep 30 & echo $! > \"$1\"; wait").await;
        let reaping = tokio::spawn(reap(child, Duration::from_secs(30), true));
        tokio::time::sleep(Duration::from_millis(100)).await;
        reaping.abort();
        let _ = reaping.await;
        let helper_gone = gone(helper).await;
        assert!(helper_gone, "the helper outlived a cancelled reap");
    }
}
