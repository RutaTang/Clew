//! The stdio transport: newline-delimited JSON frames in, a single writer
//! task draining the output channel out, and the byte budget that gives the
//! bulk producers backpressure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use clew_protocol::{ClientMessage, Event, ServerMessage, StructureIndex};
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::{Server, SpawnPolicy};

/// Backpressure for the bulk producers. The out channel is unbounded, so a
/// producer that outruns the transport would grow the queue without limit:
/// a child spewing stdout, or a burst of watcher rescans each publishing the
/// whole tree and symbol index over a slow SSH link. This tracks the bulk
/// bytes still queued (see `bulk_weight`) and parks those producers while
/// over the cap — a stdout pump stops reading its child's pipe, pushing the
/// pressure back into the child; a watcher thread stops publishing, and the
/// debouncer coalesces what arrives meanwhile.
///
/// Waiters are of two kinds, tasks (`OutputBudget::charge`) and threads
/// (`OutputBudget::charge_blocking_unless`), and a release wakes both.
pub struct OutputBudget {
    bytes: AtomicUsize,
    notify: tokio::sync::Notify,
    /// Orders a blocking waiter's check against a release's wakeup, so the
    /// wakeup cannot fall between the two. The count itself is `bytes`.
    lock: std::sync::Mutex<()>,
    freed: std::sync::Condvar,
    /// The writer is gone: nothing will be released again, so waiting would be
    /// forever. Producers go straight through — into a channel whose receiver
    /// is gone, which fails and stops them.
    closed: AtomicBool,
}

impl OutputBudget {
    /// Total bulk bytes allowed in flight at once. A single message larger
    /// than this still goes out, alone, once the queue has drained below it.
    pub(crate) const CAP: usize = 32 * 1024 * 1024;

    pub fn new() -> Arc<Self> {
        Arc::new(OutputBudget {
            bytes: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
            lock: std::sync::Mutex::new(()),
            freed: std::sync::Condvar::new(),
            closed: AtomicBool::new(false),
        })
    }

    fn admits(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.bytes.load(Ordering::Acquire) <= Self::CAP
    }

    /// Wait (as a task) until the queue is under the cap, then charge `n`.
    pub(crate) async fn charge(&self, n: usize) {
        loop {
            // Register for the wakeup BEFORE checking, so a release between
            // the check and the await can't be missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.admits() {
                self.bytes.fetch_add(n, Ordering::AcqRel);
                return;
            }
            notified.await;
        }
    }

    /// Wait (as a thread) until the queue is under the cap, then charge `n`
    /// — giving up, charging nothing and returning `false`, once `stopped`
    /// holds, which is tested between the waits. The waits are bounded
    /// ([`STOP_POLL`]): a stop is a flag somebody else sets, not something
    /// that wakes this thread. Never call this on the async runtime's own
    /// threads.
    pub(crate) fn charge_blocking_unless(&self, n: usize, stopped: &dyn Fn() -> bool) -> bool {
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        while !self.admits() {
            if stopped() {
                return false;
            }
            // Bounded wait as well as the wakeup: belt and braces, so a missed
            // notification costs a moment rather than the thread.
            guard = self
                .freed
                .wait_timeout(guard, STOP_POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        self.bytes.fetch_add(n, Ordering::AcqRel);
        true
    }

    /// Credit `n` bytes back once the message left the queue (was written to
    /// the transport, or dropped with it).
    pub fn release(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.bytes.fetch_sub(n, Ordering::AcqRel);
        self.wake();
    }

    /// The writer is gone: stop every current and future wait.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake();
    }

    fn wake(&self) {
        self.notify.notify_waiters();
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.freed.notify_all();
    }
}

/// What a message costs against the [`OutputBudget`]: roughly its bulk
/// payload in bytes. Messages that are not bulk cost nothing and are never
/// held back. Computed from the message itself, so the producer that charges
/// and the writer that releases always agree on the amount.
pub(crate) fn bulk_weight(msg: &ServerMessage) -> usize {
    let event = match msg {
        ServerMessage::Reply { event, .. } | ServerMessage::Notification { event } => event,
    };
    match event {
        Event::ProcessOutput { data, .. } => data.len(),
        // Streamed model text: a stream to a client that stopped reading
        // parks its producer (the model call pushes back on its socket)
        // instead of growing the queue without bound.
        Event::ChatDelta { text, .. } | Event::AgentDelta { text, .. } => text.len(),
        // The flat list and the nested tree carry the same names.
        Event::Tree {
            files,
            tracked_ignored,
            ..
        } => files
            .iter()
            .chain(tracked_ignored)
            .map(|rel| 2 * (rel.len() + 16))
            .sum(),
        Event::ProjectSymbols {
            files, structure, ..
        } => {
            let symbols: usize = files
                .iter()
                .map(|f| {
                    f.rel.len()
                        + 16
                        + f.symbols
                            .iter()
                            .map(|s| s.name.len() + s.kind.len() + 32)
                            .sum::<usize>()
                        + f.imports.iter().map(|i| i.module.len() + 24).sum::<usize>()
                })
                .sum();
            let structure = match structure {
                clew_protocol::Patch::Set(Some(index)) => structure_weight(index),
                _ => 0,
            };
            symbols + structure
        }
        _ => 0,
    }
}

/// Roughly how many bytes a structure index serializes to: its names plus
/// the JSON around each.
fn structure_weight(index: &StructureIndex) -> usize {
    let names = |v: &[String]| v.iter().map(|n| n.len() + 3).sum::<usize>();
    let types: usize = index
        .by_type
        .iter()
        .map(|(name, ts)| name.len() + 32 + names(&ts.traits) + names(&ts.methods))
        .sum();
    let traits: usize = index
        .implementors
        .iter()
        .map(|(name, impls)| name.len() + 8 + names(impls))
        .sum();
    types + traits
}

/// The reply that stands in for one the writer could not send: its request
/// still gets an answer, and the connection survives. `None` for a
/// notification, which nobody is waiting on.
fn substitute(msg: &ServerMessage, why: &str) -> Option<String> {
    let ServerMessage::Reply { id, .. } = msg else {
        return None;
    };
    serde_json::to_string(&ServerMessage::Reply {
        id: *id,
        event: Event::error(clew_protocol::ErrorCode::Failed, why),
    })
    .ok()
}

/// Send a bulk message from a THREAD (the watcher, a blocking pool),
/// waiting for the transport to catch up first. Returns whether it was sent.
///
/// A message that weighs nothing (see [`bulk_weight`]) is sent at once: it
/// is not what the budget bounds, and a terminal notification of a stream
/// must not wait behind the stream's own output any longer than its order in
/// the queue makes it.
pub(crate) fn send_bulk(
    out: &UnboundedSender<ServerMessage>,
    budget: &OutputBudget,
    msg: ServerMessage,
) -> bool {
    send_bulk_unless(out, budget, msg, &|| false)
}

/// How often a producer parked on the budget tests whether its stream was
/// stopped (see [`send_bulk_unless`]).
const STOP_POLL: Duration = Duration::from_millis(100);

/// [`send_bulk`] for a stream that can be stopped — a chat answer, an agent
/// turn: while it waits for the budget, `stopped` is tested, and once it
/// holds the message is dropped — nothing charged, nothing sent — and
/// `false` comes back. A producer parked here is a model call parked inside
/// its delta callback, which only tests its own stop between deltas: with a
/// full queue ahead of it (a large snapshot over a slow link), a Stop was
/// not seen until the queue drained, and the provider kept generating — and
/// billing — all that time.
pub(crate) fn send_bulk_unless(
    out: &UnboundedSender<ServerMessage>,
    budget: &OutputBudget,
    msg: ServerMessage,
    stopped: &dyn Fn() -> bool,
) -> bool {
    let weight = bulk_weight(&msg);
    if weight > 0 && !budget.charge_blocking_unless(weight, stopped) {
        return false;
    }
    if out.send(msg).is_err() {
        budget.release(weight);
        return false;
    }
    true
}

/// How long the writer may keep flushing after the client's stream ended.
const FLUSH_GRACE: Duration = Duration::from_secs(5);

/// Run the server over stdio until the client's stream ends (or stdin closes).
///
/// The transport policy ([`SpawnPolicy`]) is taken from how this process was
/// started (see [`SpawnPolicy::detect`]).
pub async fn serve_stdio() {
    let args: Vec<String> = std::env::args().collect();
    let policy = SpawnPolicy::detect(&args, |name| std::env::var(name).ok());
    serve(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        policy,
    )
    .await;
}

/// Run the server over one transport until the client's stream ends.
///
/// Framing is newline-delimited JSON: each `ClientMessage` arrives as one line,
/// each `ServerMessage` is written back as one line. serde_json's compact output
/// never contains a literal newline (string values escape theirs), so a line is
/// always exactly one message. A dedicated writer task drains the output channel
/// so replies and unsolicited notifications (file changes) share one stream.
///
/// When the stream ends, everything this connection started is stopped first
/// ([`Server::shutdown`]) — a turn exploring for a client that has gone would
/// otherwise keep calling (and billing) the model — and the writer is given a
/// bounded grace to flush. The caller must still bound the RUNTIME's
/// shutdown: a blocking thread parked in a syscall cannot be interrupted, and
/// a plain runtime drop waits for it without limit (see `main.rs`).
pub async fn serve<R, W>(reader: R, writer: W, policy: SpawnPolicy)
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::with_policy(out.clone(), policy);
    let budget = server.output_budget();
    let writer = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            // Credit the bulk bytes back once written (or unserializable):
            // the bulk producers wait on this while the transport is behind.
            let weight = bulk_weight(&msg);
            let json = serde_json::to_string(&msg);
            budget.release(weight);
            // A payload that cannot be serialized (a map key JSON cannot
            // hold, say) must not cost its caller an answer: a correlated
            // reply degrades to an error; a notification is dropped.
            let json = match json {
                Ok(json) => json,
                Err(e) => {
                    eprintln!("[clew-server] a frame did not serialize: {e}");
                    match substitute(&msg, &format!("the reply could not be encoded: {e}")) {
                        Some(json) => json,
                        None => continue,
                    }
                }
            };
            // Last line of defence on frame size. Every construction site has
            // its own budget, so reaching this means one of them is wrong —
            // but writing the frame anyway would make the CLIENT hang up (it
            // cannot resync past an over-cap line), turning a bug in one
            // reply into a dropped connection. A correlated reply degrades to
            // an error the caller can surface; a notification is dropped.
            let mut json = if json.len() > clew_protocol::MAX_FRAME_BYTES {
                eprintln!(
                    "[clew-server] refusing to send a {}-byte frame (cap {}); this is a missing \
                     construction-site budget",
                    json.len(),
                    clew_protocol::MAX_FRAME_BYTES
                );
                match substitute(&msg, "the reply was too large to send") {
                    Some(json) => json,
                    None => continue,
                }
            } else {
                json
            };
            json.push('\n');
            if writer.write_all(json.as_bytes()).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        // Nothing will be released from here on; producers must stop waiting.
        budget.close();
    });

    let mut reader = reader;
    while let Some(line) = clew_core::framing::read_protocol_line(&mut reader).await {
        if line.is_empty() {
            continue;
        }
        let Ok(ClientMessage { id, request }) = serde_json::from_str::<ClientMessage>(&line) else {
            // Fail closed: a frame that doesn't parse means the peer's
            // protocol build differs (or the stream is corrupt) — past it,
            // nothing on this connection can be trusted to mean what it
            // says. Ending the transport surfaces the problem immediately
            // (the client reconnects and the handshake explains it) instead
            // of silently dropping an unknowable subset of requests.
            eprintln!("[clew-server] unparseable frame — closing the connection");
            break;
        };
        if let Some(event) = server.handle(id, request).await
            && out.send(ServerMessage::Reply { id, event }).is_err()
        {
            break; // writer gone
        }
    }
    // The client is gone (or unintelligible): stop what it started.
    server.shutdown().await;
    drop(server);
    drop(out); // close the channel so the writer task ends
    // Bounded, because dropping OUR sender is not enough to close the channel:
    // background work still unwinding holds clones of it, and waiting outright
    // made the exit hostage to a stream that might never end. Long enough to
    // flush anything real that is still queued, short enough that a wedged
    // producer cannot pin the process — it dies with us either way.
    if tokio::time::timeout(FLUSH_GRACE, writer).await.is_err() {
        eprintln!("[clew-server] exiting with output still in flight");
    }
}

#[cfg(test)]
mod tests {
    use super::{OutputBudget, bulk_weight, send_bulk};
    use clew_protocol::{Event, ServerMessage};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    fn tree(files: usize) -> ServerMessage {
        ServerMessage::Notification {
            event: Event::Tree {
                root: "/p".into(),
                seq: 1,
                tree: Default::default(),
                files: (0..files).map(|i| format!("src/file{i}.rs")).collect(),
                truncated: false,
                tracked_ignored: Vec::new(),
            },
        }
    }

    /// A thread publishing trees faster than the transport drains them is
    /// held at the cap until the writer releases what it wrote — the queue no
    /// longer grows with every watcher rescan.
    #[test]
    fn a_bulk_producer_waits_for_the_writer_to_catch_up() {
        let budget = OutputBudget::new();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Fill the budget past the cap in one message (a single oversized
        // message always goes through when the queue is under the cap).
        let big = ServerMessage::Notification {
            event: Event::ProcessOutput {
                proc: 1,
                data: vec![0; OutputBudget::CAP + 1],
            },
        };
        assert!(send_bulk(&out, &budget, big));

        let sent = Arc::new(AtomicBool::new(false));
        let (flag, producer_budget, producer_out) = (sent.clone(), budget.clone(), out.clone());
        let producer = std::thread::spawn(move || {
            send_bulk(&producer_out, &producer_budget, tree(10));
            flag.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !sent.load(Ordering::SeqCst),
            "the second message must wait while the queue is over the cap"
        );
        // The writer drains the first message and credits it back.
        let first = rx.try_recv().unwrap();
        budget.release(bulk_weight(&first));
        producer.join().unwrap();
        assert!(
            sent.load(Ordering::SeqCst),
            "released, the producer proceeds"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerMessage::Notification {
                event: Event::Tree { .. },
                ..
            })
        ));
    }

    /// When the writer is gone nothing will ever be released: a waiting
    /// producer must be let go, not parked forever.
    #[test]
    fn closing_the_budget_releases_every_waiter() {
        let budget = OutputBudget::new();
        assert!(budget.charge_blocking_unless(OutputBudget::CAP + 1, &|| false));
        let waiter_budget = budget.clone();
        let waiter =
            std::thread::spawn(move || waiter_budget.charge_blocking_unless(10, &|| false));
        std::thread::sleep(Duration::from_millis(100));
        budget.close();
        waiter.join().unwrap();
    }

    /// A reply the writer cannot send is still answered — with an error under
    /// the same id — while a notification (nobody waits on it) is dropped.
    #[test]
    fn an_unsendable_reply_is_answered_and_a_notification_dropped() {
        let reply = ServerMessage::Reply {
            id: 42,
            event: Event::ChatResult { text: "x".into() },
        };
        let json = super::substitute(&reply, "too large").expect("a stand-in reply");
        match serde_json::from_str::<ServerMessage>(&json).unwrap() {
            ServerMessage::Reply {
                id: 42,
                event:
                    Event::Error {
                        code: clew_protocol::ErrorCode::Failed,
                        message,
                    },
            } => assert_eq!(message, "too large"),
            other => panic!("expected the stand-in error, got {other:?}"),
        }
        assert!(super::substitute(&tree(1), "too large").is_none());
    }

    /// Streamed model text is bulk, like process output: a chat or agent
    /// delta is charged against the budget, so a model streaming to a client
    /// that stopped reading parks its producer instead of queueing without
    /// bound. (Every other bulk producer was held; the deltas bypassed it.)
    #[test]
    fn streamed_deltas_are_held_by_the_budget() {
        let chat = |text: &str| ServerMessage::Notification {
            event: Event::ChatDelta {
                stream: 1,
                text: text.into(),
            },
        };
        let agent = ServerMessage::Notification {
            event: Event::AgentDelta {
                stream: 2,
                text: "abc".into(),
            },
        };
        assert_eq!(bulk_weight(&chat("hello")), 5);
        assert_eq!(bulk_weight(&agent), 3);

        let budget = OutputBudget::new();
        assert!(budget.charge_blocking_unless(OutputBudget::CAP + 1, &|| false));
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let producer_budget = budget.clone();
        let producer = std::thread::spawn(move || send_bulk(&out, &producer_budget, chat("x")));
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            rx.try_recv().is_err(),
            "the delta waits while the queue is over the cap"
        );
        budget.release(OutputBudget::CAP + 1);
        assert!(producer.join().unwrap());
        assert!(rx.try_recv().is_ok(), "released, it goes out");
    }

    /// A producer parked on a full budget is let go by its stream's stop —
    /// nothing charged, nothing sent — instead of waiting for the queue to
    /// drain while its model keeps generating.
    #[test]
    fn a_stop_releases_a_producer_parked_on_the_budget() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let budget = OutputBudget::new();
        assert!(budget.charge_blocking_unless(OutputBudget::CAP + 1, &|| false));
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (done_tx, done) = std::sync::mpsc::channel();
        let (producer_budget, producer_stop) = (budget.clone(), stop.clone());
        std::thread::spawn(move || {
            let delta = ServerMessage::Notification {
                event: Event::ChatDelta {
                    stream: 1,
                    text: "x".into(),
                },
            };
            let stopped = || producer_stop.load(Ordering::Relaxed);
            let _ = done_tx.send(super::send_bulk_unless(
                &out,
                &producer_budget,
                delta,
                &stopped,
            ));
        });
        std::thread::sleep(Duration::from_millis(200));
        stop.store(true, Ordering::Relaxed);
        let sent = done.recv_timeout(Duration::from_secs(3));
        // Let a producer that ignored the stop go, so the test ends.
        budget.release(OutputBudget::CAP + 1);
        assert_eq!(
            sent,
            Ok(false),
            "the stop did not reach the parked producer"
        );
        assert!(rx.try_recv().is_err(), "a stopped delta was sent");
        assert!(budget.admits() && budget.bytes.load(Ordering::Acquire) == 0);
    }

    /// A request stream generated as it is read: each piece bytes, or a run
    /// of spaces — JSON whitespace, which pads a frame to any length without
    /// the test holding it.
    enum Piece {
        Bytes(Vec<u8>),
        Spaces(usize),
    }

    struct Generated(std::collections::VecDeque<Piece>);

    impl tokio::io::AsyncRead for Generated {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let pieces = &mut self.get_mut().0;
            while buf.remaining() > 0 {
                match pieces.front_mut() {
                    None => break,
                    Some(Piece::Bytes(bytes)) => {
                        let n = bytes.len().min(buf.remaining());
                        buf.put_slice(&bytes[..n]);
                        bytes.drain(..n);
                        if bytes.is_empty() {
                            pieces.pop_front();
                        }
                    }
                    Some(Piece::Spaces(left)) => {
                        let n = (*left).min(buf.remaining()).min(64 * 1024);
                        buf.put_slice(&[b' '; 64 * 1024][..n]);
                        *left -= n;
                        if *left == 0 {
                            pieces.pop_front();
                        }
                    }
                }
            }
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The ids `serve` answered, given a first frame of `content` bytes (a
    /// `Hello` padded with whitespace) and a second, ordinary one.
    async fn answered_after_a_frame_of(content: usize) -> Vec<u64> {
        use tokio::io::AsyncReadExt;
        let hello = |id: u64| {
            serde_json::to_string(&clew_protocol::ClientMessage {
                id,
                request: clew_protocol::Request::Hello {
                    protocol: clew_protocol::PROTOCOL_VERSION,
                    fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
                },
            })
            .unwrap()
        };
        let first = hello(1);
        let padding = content - first.len();
        let stream = Generated(std::collections::VecDeque::from([
            Piece::Bytes(first.into_bytes()),
            Piece::Spaces(padding),
            Piece::Bytes(format!("\n{}\n", hello(2)).into_bytes()),
        ]));
        let (mut client, server_end) = tokio::io::duplex(1 << 20);
        super::serve(
            tokio::io::BufReader::new(stream),
            server_end,
            crate::SpawnPolicy::Remote,
        )
        .await;
        let mut replies = String::new();
        tokio::time::timeout(Duration::from_secs(10), client.read_to_string(&mut replies))
            .await
            .expect("the writer ends with the connection")
            .unwrap();
        replies
            .lines()
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(ServerMessage::Reply { id, .. }) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// The request loop reads its frames with the one protocol reader
    /// (`framing::read_protocol_line`): a frame of exactly the protocol's cap
    /// — the largest a client sends — is read and answered, and so is the
    /// frame after it; one byte more ends the connection, with nothing after
    /// it read. A reader of the loop's own that counted the newline against
    /// the cap (what both ends did before they shared one) drops the first;
    /// one without the cap answers the second.
    #[tokio::test]
    async fn the_request_loop_reads_frames_up_to_the_cap_and_no_further() {
        let cap = clew_protocol::MAX_FRAME_BYTES;
        assert_eq!(
            answered_after_a_frame_of(cap).await,
            [1, 2],
            "a frame of exactly the cap, and the one after it"
        );
        assert_eq!(
            answered_after_a_frame_of(cap + 1).await,
            Vec::<u64>::new(),
            "a frame over the cap ends the connection"
        );
    }

    #[test]
    fn only_bulk_messages_weigh_anything() {
        let small = ServerMessage::Reply {
            id: 1,
            event: Event::error(clew_protocol::ErrorCode::Failed, "x".repeat(1000)),
        };
        assert_eq!(bulk_weight(&small), 0);
        assert!(bulk_weight(&tree(100)) > 100 * 10);
    }
}
