//! The clew-server seam: the window's link to its server ([`ServerLink`]:
//! the request channel, handed out only while it is alive), the transport's
//! lifecycle (connected, handshake, ready, disconnected, unavailable — and
//! released when nothing needs it any more), what a dead transport takes with
//! it, the RPC clients the AI and git flows use, and the routing of the
//! server's replies and notifications to the features they answer.
//!
//! Its messages, [`ServerMsg`], arrive through `App::update_server`.

use crate::app::prelude::*;
use crate::*;

/// The request id the handshake's `Hello` carries. Reserved outside the
/// counted range because a REFUSED handshake is answered with a bare `Error`,
/// and the client has to tell that refusal apart from an error about any other
/// request in flight — mistaking one for the other either strands the deferred
/// scan or tears down a healthy transport. Every other request, fire-and-forget
/// ones included, is minted from `next_req_id`, which counts up from 1, so no
/// reply can ever collide with it.
pub(crate) const HELLO_REQ_ID: u64 = u64::MAX;

/// The `reason` of the `ServerMsg::Disconnected` the app emits itself when it lets
/// its transport go (see `App::update`): not a failure, and never shown.
pub(crate) const SERVER_RELEASED: &str = "no longer needed by this window";

/// How many times a correlated request the server refused as not ready yet is
/// sent again before the refusal stands (see `App::send_retrying`). The server
/// already waits a bounded time for its scan before refusing, so this bounds
/// the whole wait too.
const NOT_READY_RETRIES: u32 = 4;

/// The backoff before re-sending a not-ready refusal: doubled per attempt.
#[cfg(not(test))]
const NOT_READY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(test)]
const NOT_READY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(1);

/// Why a request could not be handed to the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SendError {
    /// There is no live transport: none was ever connected, or its writer
    /// has gone (the server exited, the link dropped, the app released it).
    NoServer,
    /// The transport is alive but its request queue is full: the server has
    /// stopped reading. The request is refused, not queued (see
    /// `server::REQUEST_QUEUE`).
    Full,
}

/// The window's link to its clew-server: the request channel, and what is
/// awaiting an answer over it that belongs to no one project — the AI round
/// trips, the proxied processes, the Connect modal's folder listing. (What a
/// project awaits over it is its [`ProjectLink`].) Replaced whole when the
/// transport dies or is switched (`App::drop_connection_state`); dropping it
/// wakes every RPC still waiting for a reply.
///
/// The channel itself is private: [`ServerLink::tx`] hands it out only while
/// its writer is alive, so no code path holds on to — and keeps "sending"
/// into — a transport that is gone, whether it died, was switched away from,
/// or was released because nothing needed it (`App::update`).
#[derive(Default)]
pub struct ServerLink {
    tx: Option<server::RequestTx>,
    /// In-flight AI RPCs: request id -> the caller awaiting its reply. Shared
    /// so background AI tasks can register/await while `update` resolves them.
    pub ai_pending: std::sync::Arc<std::sync::Mutex<HashMap<u64, PendingReply>>>,
    /// proc handle -> the channel that feeds `ProcessOutput` bytes into the
    /// matching client's stdout bridge (bounded: see `PROC_FEED_QUEUE`).
    pub proc_feeds: HashMap<u64, ProcFeed>,
    /// language -> its live server-spawned proc handle, so a restart can kill the
    /// old process before starting a new one.
    pub lsp_procs: HashMap<String, u64>,
    /// Request id of the in-flight `ListDir` for the Connect modal's folder
    /// picker. Only the newest listing may paint the browser: two quick
    /// clicks used to let a slower earlier reply overwrite the newer one.
    pub pending_list_dir: Option<u64>,
    /// Request id of the `OpenProject` a project OPEN waits on (not the
    /// resync a reconnect sends for the project on screen). Its failure is
    /// that open's answer (`App::on_open_project_failed`): uncorrelated, it
    /// only reached the status line, and "Scanning…" hid the panes until a
    /// reconnect. Per transport, like everything here.
    pub pending_open: Option<u64>,
    /// This transport's handshake completed (`App::on_server_ready`), which
    /// is also where the project on screen is opened on it. The channel is
    /// installed before that, and a request sent in between reaches a server
    /// that holds no project — a journaled state edit refused there would be
    /// dropped, so those wait for this (`App::send_remote_edits`).
    pub ready: bool,
}

impl ServerLink {
    /// The link over a freshly connected transport's request channel.
    pub(crate) fn connected(tx: server::RequestTx) -> ServerLink {
        let mut link = ServerLink::default();
        link.tx = Some(tx);
        link
    }

    /// The request channel, while its writer is alive — never a channel into
    /// a transport that has gone.
    pub fn tx(&self) -> Option<&server::RequestTx> {
        self.tx.as_ref().filter(|tx| !tx.is_closed())
    }

    /// Whether a live transport is connected.
    pub fn is_up(&self) -> bool {
        self.tx().is_some()
    }

    /// Whether a channel is installed at all, alive or not — a transport this
    /// link has not been dropped for yet.
    pub(crate) fn installed(&self) -> bool {
        self.tx.is_some()
    }

    /// Let the transport go: dropping the last sender ends the writer task,
    /// so the server sees EOF and exits. The rest of the link stays until the
    /// disconnect that follows drops it whole.
    pub(crate) fn close(&mut self) {
        self.tx = None;
    }

    /// Hand `message` to the server without blocking (this runs on the UI
    /// thread). The queue is bounded: when it is full the request is refused
    /// with [`SendError::Full`] — never buffered without bound, never waited
    /// on. Callers treat both errors as the request not having been made.
    pub(crate) fn send(&self, message: clew_protocol::ClientMessage) -> Result<(), SendError> {
        let tx = self.tx().ok_or(SendError::NoServer)?;
        tx.try_send(message).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(_) => SendError::Full,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => SendError::NoServer,
        })
    }
}

impl Drop for ServerLink {
    /// Dropping the oneshot senders wakes every task awaiting an AI reply
    /// with an error; their (stamped) result messages reset the busy flags.
    /// The map is shared with those tasks, so it is emptied, not just let go.
    fn drop(&mut self) {
        self.ai_pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl App {
    pub(crate) fn on_server_connected(&mut self, tx: server::RequestTx) -> Task<Message> {
        // The in-process clew-server is up; keep its request channel and
        // greet it. Backend flows migrate onto this seam one at a time.
        let hello = clew_protocol::ClientMessage {
            id: HELLO_REQ_ID,
            request: clew_protocol::Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
            },
        };
        // A fresh channel has room: the greeting is its first frame.
        let _ = tx.try_send(hello);
        // A fresh transport must not inherit the previous one's in-flight
        // bookkeeping (normally already dropped with its link by the
        // disconnect handler; this also covers a target switch, where no
        // disconnect fires).
        if self.server.installed() {
            self.drop_connection_state();
        }
        self.server = ServerLink::connected(tx);
        // A transport is up again, so the previous refusal no longer stands:
        // this one gets a verdict of its own. Cleared here rather than in
        // `on_server_ready` so a REFUSAL from this server is what re-latches
        // it, not a leftover from the last one.
        self.handshake_failure = None;
        // Nothing else yet: business requests wait for the `Ready` reply
        // (`on_server_ready`). Pipelining them behind Hello meant a server
        // speaking another protocol version received — and half-answered —
        // requests on a connection neither side fully understood.
        Task::none()
    }

    /// The handshake did NOT succeed: the server refused our `Hello`, or
    /// answered with a protocol version / build we don't share. Every failure
    /// path lands here, because each one used to strand the deferred scan:
    /// `start_scan` parks its root waiting for a server, and the ONLY code
    /// that ever sends it is `on_server_ready`. Leaving `scanning` and
    /// `pending_scan_root` set painted "Scanning…" over an empty window for
    /// the rest of the session — no project, no fallback, and re-picking the
    /// folder just re-parked it.
    pub(crate) fn on_handshake_failed(&mut self, why: String) -> Task<Message> {
        // Drop the transport. Nothing can be asked of a server whose protocol
        // we don't share — it refuses every later request anyway — and
        // dropping the last sender ends the writer task, so the server sees
        // EOF and exits rather than lingering as a process nobody talks to.
        self.server.close();
        // Latch the verdict BEFORE that EOF comes back as a disconnect. Every
        // path into this handler is a version or build mismatch between two
        // binaries on disk: spawning the same server again re-runs the same
        // refusal, so the disconnect handler must not treat it as a crash to
        // recover from (see `on_server_disconnected`). A rebuilt server is
        // still picked up without restarting clew — on the next project open,
        // which re-arms the attempt, rather than 1.5 s later.
        self.handshake_failure = Some(why.clone());
        self.status = why;
        let Some(root) = self.pending_scan_root.take() else {
            return Task::none();
        };
        // Same escape hatch `on_server_unavailable` takes, and the same
        // asymmetry: a remote root names a path on the OTHER host, so scanning
        // it here would open whatever this machine happens to have there.
        if self.connection.is_remote() {
            self.scanning = false;
            return Task::none();
        }
        self.local_scan(root)
    }

    /// The handshake succeeded (`Ready` matched our protocol): only now do
    /// business requests flow to the server.
    pub(crate) fn on_server_ready(&mut self) -> Task<Message> {
        self.server.ready = true;
        // Resume a scan that was waiting for the server (its Tree reply
        // opens the project); otherwise, if a project is already open
        // (local-fallback path), tell the server about it for search.
        if let Some(root) = self.pending_scan_root.clone() {
            self.request_open_project(root);
        } else {
            self.sync_project_to_server();
            // Re-arm the remote `.clew/` session state. The previous
            // transport's `ReadState` replies can never arrive, so the rels
            // they would have cleared stay outstanding — and every later save
            // of history / bookmarks / notes / reading target is deferred
            // forever instead of being written. Re-reading clears them, and
            // anything changed while the link was down is still in
            // `remote_state_dirty` and flushes as each read lands.
            //
            // Not needed on the branch above: reopening the project runs
            // `on_scan_done`, which requests the state itself.
            if self.proj.project.is_some() && !self.local_project_state() {
                self.request_remote_state();
            }
        }
        // Give the server the AI config so server-endpoint calls work.
        self.send_ai_config();
        // If the Connect modal was waiting on this transport, move it into
        // the remote folder picker and list the home directory.
        if let Some(ui) = &self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            self.enter_remote_browser(None);
        }
        Task::none()
    }

    pub(crate) fn on_server_unavailable(&mut self, reason: String) -> Task<Message> {
        // `scanning` and `pending_scan_root` are a pair — "an open is in
        // progress" and "which one" — so every path that abandons the parked
        // root has to drop the flag with it, exactly as `on_handshake_failed`
        // does below. Nothing else can: `on_scan_done` needs a Tree/ScanDone
        // that can no longer come, and the only other clear site is
        // `connect_to`. Left set, `ui::pane_area` returns the "Scanning
        // project…" placeholder ahead of every other branch, hiding the panes
        // of an already-open project (and the welcome screen when none is
        // open) until the user happens to reconnect.
        //
        // A remote bootstrap failure surfaces in the Connect modal rather
        // than falling back to a (meaningless) local scan of a remote path.
        if let Some(ui) = &mut self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            ui.stage = ConnectStage::Error(reason);
            self.pending_scan_root = None;
            self.scanning = false;
            return Task::none();
        }
        // A remote transport that died outside the Connect modal (e.g. a
        // reconnect that failed): fail closed. The deferred root is a remote
        // path — a local scan of it would read this machine's files instead.
        if self.connection.is_remote() {
            self.pending_scan_root = None;
            self.scanning = false;
            self.status = format!("{reason} — use Connect to reconnect.");
            return Task::none();
        }
        // The server binary didn't spawn. Fall back to a local scan for
        // any project that was deferred waiting on it.
        self.status = reason;
        if let Some(root) = self.pending_scan_root.take() {
            return self.local_scan(root);
        }
        Task::none()
    }

    /// The server transport died mid-session (process exit, SSH drop). Every
    /// piece of in-flight bookkeeping tied to that transport is now garbage:
    /// replies can no longer arrive (the stream is gone), so anything still
    /// "pending" would wait forever, and proc handles name processes on a
    /// server that no longer exists. Clear it all, then bump `conn_gen` — the
    /// subscription is keyed on it, so iced starts a fresh transport, which is
    /// the reconnect. `reason` (a frame that did not decode, …) is shown with
    /// the reconnect notice, so a disconnect the client caused by refusing a
    /// frame does not read as the server's crash.
    pub(crate) fn on_server_disconnected(&mut self, reason: Option<String>) -> Task<Message> {
        // Released by this window, not lost: nothing needs a server any more
        // (see `App::update`, which emits this when the subscription's
        // predicate flips off). The subscription is being dropped, so there is
        // nothing to reconnect — tear down what the transport carried, and
        // move the transport identity on so anything the dropped stream had
        // already queued is recognized as late. The status line is left alone:
        // this is not an event the reader needs to hear about.
        if !self.wants_server() {
            self.drop_connection_state();
            self.conn_gen += 1;
            self.conn_respawn = false;
            return Task::none();
        }
        // A handshake refusal is not a crash. `on_handshake_failed` drops the
        // transport itself, and the server exits on that EOF — so this runs
        // for it too, and re-keying here started the SAME incompatible binary
        // again, got the same refusal, and dropped the transport again, about
        // every 1.5 s for as long as the window stayed open. Worse, each cycle
        // ran `drop_connection_state`, which kills the language servers this
        // client had started locally (its own children, in exactly the state
        // where there is no server to proxy them) before they finish indexing.
        // So: keep the local fallback the handshake failure already fell back
        // to, and keep the one message that says how to fix it. An explicit
        // user action re-arms the retry — see
        // `retry_server_after_handshake_failure`.
        //
        // Not closed here: the teardown below still drops locally-spawned
        // language servers along with the proxied ones — it cannot tell them
        // apart, and at its other call site (a target switch) clearing both is
        // required. This bounds that to the ONE disconnect a refusal produces
        // instead of one every 1.5 s.
        if let Some(why) = self.handshake_failure.clone() {
            self.drop_connection_state();
            self.status = why;
            return Task::none();
        }
        self.conn_gen += 1;
        self.conn_respawn = true;
        self.drop_connection_state();
        self.status = match reason {
            Some(why) => format!("clew-server disconnected ({why}) — reconnecting…"),
            None => "clew-server disconnected — reconnecting…".into(),
        };
        Task::none()
    }

    /// Re-arm the transport after a handshake refusal latched the automatic
    /// reconnect off, if one did. Called from the paths where the USER asks
    /// for a project (folder picked, consent granted).
    ///
    /// Two reasons this cannot wait for the next crash. The binary may have
    /// been rebuilt since the refusal, and that is the fix the message asked
    /// for. And with no transport at all `start_scan` parks its root waiting
    /// for a server (`pending_scan_root`) — the ONLY thing that ever releases
    /// it is a handshake outcome, so without a fresh attempt the window would
    /// sit on "Scanning…" forever, which is the stall `on_handshake_failed`
    /// exists to prevent. Bumping the generation re-keys the subscription,
    /// which IS the reconnect; if the server is still incompatible the refusal
    /// lands again and releases the parked root into the local scan.
    pub(crate) fn retry_server_after_handshake_failure(&mut self) {
        if self.handshake_failure.take().is_some() {
            self.conn_gen += 1;
            // Not a respawn-after-crash: connect immediately, the user is
            // waiting on this project opening.
            self.conn_respawn = false;
        }
    }

    /// Forget every request, stream, and process handle tied to the current
    /// (now dead, replaced or released) server transport: the window's
    /// [`ServerLink`] and the open project's [`ProjectLink`], each dropped
    /// whole, plus what their loss means to the features that were waiting on
    /// them. Shared by disconnect, release and (re)connect — a new transport
    /// must not inherit the old one's in-flight bookkeeping.
    pub(crate) fn drop_connection_state(&mut self) {
        // Every write still awaiting its acknowledgement is now unanswerable.
        // It stays marked dirty (`write_remote_state` marks before sending),
        // so the reconnect's re-read keeps this client's version and flushes
        // it. Record WHICH files those are before the link that tracked them
        // goes: from here that dirt means something stronger than
        // "unacknowledged" — the server never got it. Without the distinction,
        // a second edit of the same file made in the reconnect window put the
        // rel back in flight and the `StateContent` guard read the mark as
        // "the merge is on its way", skipping the flush that rescues this
        // change — and the merge, which was computed without it, was then
        // adopted over this window's copy, erasing it from both sides.
        //
        // Only the stores written wholesale: a change to a mergeable one is
        // in the edit journal, which the next transport replays as itself.
        let wholesale = self
            .proj
            .remote_state_dirty
            .iter()
            .filter(|rel| !self.remote_edits_pending(rel))
            .cloned()
            .collect::<Vec<_>>();
        self.proj.remote_state_unsent.extend(wholesale);
        self.unsend_remote_edits();
        // Everything the project held over the dead transport goes as ONE
        // unit (see `ProjectLink`): the requests awaiting replies (reads, blame,
        // search, docs, state writes and their rescue records — none can be
        // answered now, and a later id collision must not find them), the
        // server's publication counters (server-lifetime: the next transport's
        // start again at 0, and a kept high-water mark made its fresh snapshot
        // look stale), the tree resync (the reconnect re-sends its own), the
        // Ask streams (dropping their senders ends the pumps), and the language
        // servers — the proxied ones died with the server, and the local ones
        // cannot be told apart from them.
        drop(std::mem::take(&mut self.proj.link));
        // What that loss means to the features that were waiting on it:
        // decisions, not link state.
        //
        // The search spinner: its reply cannot come.
        self.proj.search.running = false;
        // An unanswerable docs build must not leave its request-time revision
        // stamped on the index it was going to replace, and a reconnect cannot
        // vouch for what it already holds: the next visit to DOCS rebuilds.
        self.proj.docs.rev = crate::app::docs::DOCS_REV_STALE;
        // The window's side of the dead transport goes as one unit too (see
        // `ServerLink`): its request channel, the AI round trips awaiting
        // replies (dropping the link wakes each with an error; their stamped
        // result messages reset the busy flags), the proxied processes' feeds
        // and handles (those processes died with the server), and the Connect
        // modal's listing.
        drop(std::mem::take(&mut self.server));
        // Nothing will answer the in-flight listing, and only the correlated
        // reply clears this spinner now.
        if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage) {
            b.loading = false;
        }
        // The Ask pumps ended with their senders; close any turn that was still
        // streaming so the UI doesn't show a spinner forever. (The streams'
        // ids went with the link: the server dies with the transport, so there
        // is nothing left to cancel.)
        for turn in &mut self.proj.ask_turns {
            if turn.streaming {
                turn.streaming = false;
                if turn.answer_md.trim().is_empty() {
                    turn.answer_md = "*Couldn't answer: server disconnected*".into();
                }
            }
        }
        self.proj.asking = false;
        // Server-proxied processes (language servers, debug adapters) died with
        // the server; their language-server clients went with the project's
        // link, so the next need respawns them. (Spawn results still in flight
        // for them are transport-stamped, and dropped in `dispatch`.)
        //
        // A proxied debug session can't outlive its transport.
        if let Some(session) = self.debug.session.as_mut() {
            session.status = DebugStatus::Terminated;
            session.current = None;
        }
        self.bump_debug_run();
    }

    /// A fresh id from the window's request-id space — the one server request
    /// ids, stream ids and local request stamps (`FileLoaded`, `DiffLoaded`,
    /// `GitInfoLoaded`) all share, so no two stamps are ever equal.
    pub(crate) fn mint_request_id(&self) -> u64 {
        self.next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Send `request` to the server under a fresh id from the window's
    /// request-id space: the id, or `None` when it could not be handed over
    /// (see [`Self::send_message`]).
    pub(crate) fn send_to_server(&mut self, request: clew_protocol::Request) -> Option<u64> {
        let id = self.mint_request_id();
        self.send_message(clew_protocol::ClientMessage { id, request })
            .then_some(id)
    }

    /// [`Self::send_to_server`] for a sender that holds its request back
    /// itself while the transport's queue is full, and sends it again — the
    /// edit journal: the status line is left alone, as "one was dropped" is
    /// not so of such a request, and said again on every try.
    pub(crate) fn offer_to_server(&mut self, request: clew_protocol::Request) -> Option<u64> {
        let id = self.mint_request_id();
        self.server
            .send(clew_protocol::ClientMessage { id, request })
            .is_ok()
            .then_some(id)
    }

    /// [`Self::send_to_server`] for a correlated request the server answers
    /// with `ErrorCode::NotReady` while its project scan runs (a Search, a
    /// `BuildDocs`): the refusal is not shown — the request is sent again
    /// after a backoff, up to [`NOT_READY_RETRIES`] times, for as long as
    /// this window still waits on it (`ServerMsg::ResendNotReady`). Only
    /// `rpc()` retried the scan window before, and these never go through it,
    /// so a search typed while a big project was opening failed outright.
    pub(crate) fn send_retrying(&mut self, request: clew_protocol::Request) -> Option<u64> {
        let id = self.send_to_server(request.clone())?;
        self.proj.link.not_ready_retry.insert(
            id,
            NotReadyRetry {
                request,
                attempt: 0,
            },
        );
        Some(id)
    }

    /// Whether a correlated request this window re-sends on a not-ready
    /// refusal (see [`Self::send_retrying`]) is still the one it waits on.
    fn waits_for(&self, id: u64) -> bool {
        self.proj.link.pending_search == Some(id) || self.proj.link.pending_docs == Some(id)
    }

    /// The not-ready backoff ran out: send the request again, under a new id
    /// that takes over the old one's place — unless the window stopped
    /// waiting on it meanwhile (a newer search, the tab's own refresh).
    pub(crate) fn on_resend_not_ready(
        &mut self,
        id: u64,
        request: clew_protocol::Request,
        attempt: u32,
    ) -> Task<Message> {
        if !self.waits_for(id) {
            return Task::none();
        }
        let Some(new_id) = self.send_to_server(request.clone()) else {
            // Not handed over — no transport, or its queue full, which the
            // status line says: nothing will answer the id waited on, so
            // what waits on it ends here, as a refusal would end it. Left
            // waiting, a docs build's spinner stayed up, and the tab's own
            // refresh sent nothing while it did.
            let _ = self.end_awaited(
                id,
                Some("not sent again: clew-server took no request".into()),
            );
            return Task::none();
        };
        self.proj
            .link
            .not_ready_retry
            .insert(new_id, NotReadyRetry { request, attempt });
        let link = &mut self.proj.link;
        for slot in [&mut link.pending_search, &mut link.pending_docs] {
            if *slot == Some(id) {
                *slot = Some(new_id);
            }
        }
        Task::none()
    }

    /// The search or the docs build this window waits on under `id` ends
    /// without an answer — refused, or not sent again: its slot is freed and
    /// its spinner stopped, and what rode on it goes with it. `error` is
    /// what the search tab says (`None`: cancelled, which is no error).
    /// Whether `id` was one of them.
    fn end_awaited(&mut self, id: u64, error: Option<String>) -> bool {
        let mut ended = false;
        if self.proj.link.pending_search == Some(id) {
            self.proj.link.pending_search = None;
            self.proj.search.running = false;
            self.proj.search.error = error;
            ended = true;
        }
        if self.proj.link.pending_docs == Some(id) {
            self.proj.link.pending_docs = None;
            // No index arrives, so the revision this build was requested at
            // must not stay stamped on the older index still on screen.
            self.proj.docs.rev = crate::app::docs::DOCS_REV_STALE;
            // The "View docs" this build was carrying dies with it. The
            // `Docs` reply is the ONLY consumer of the parked name, so
            // leaving it set aimed it at the next SUCCESSFUL build of this
            // project — a sidebar visit or an edit-triggered rebuild minutes
            // later opened the doc page over whatever the reader had in the
            // pane, for a request this client had already reported as
            // refused.
            self.proj.link.pending_docs_view = None;
            ended = true;
        }
        ended
    }

    /// Hand `message` to the server — whether it was. It is not when there is
    /// no live transport (callers fall back or wait for the reconnect, as
    /// before), nor when the server has stopped reading and its bounded queue
    /// is full: the request is then dropped with an error in the status line,
    /// never buffered without bound and never waited on here, on the UI thread
    /// (see `ServerLink::send`).
    pub(crate) fn send_message(&mut self, message: clew_protocol::ClientMessage) -> bool {
        match self.server.send(message) {
            Ok(()) => true,
            Err(SendError::NoServer) => false,
            Err(SendError::Full) => {
                self.status =
                    "clew-server is not taking requests (its queue is full) — one was dropped"
                        .into();
                false
            }
        }
    }

    /// Whether this window needs its clew-server: a project is open or being
    /// opened (or awaits consent to), or the source is remote. The server
    /// subscription runs exactly while this holds (`window_subscription`), and
    /// `update` releases the transport when it stops holding.
    pub(crate) fn wants_server(&self) -> bool {
        !self.quitting
            && (self.proj.project.is_some()
                || self.pending_scan_root.is_some()
                || self.pending_consent.is_some()
                || self.connection.is_remote())
    }

    /// A journaled edit left the journal unapplied (refused, or out of
    /// tries): the status line says it was not saved, and which entry it
    /// changed. This window's copy still shows it, applied optimistically, so
    /// the store is read again — once no other edit of it is waiting
    /// (`last`) — and the screen matches the disk.
    fn edit_not_saved(&mut self, edit: crate::app::remote_state::Unapplied, message: &str) {
        let crate::app::remote_state::Unapplied {
            rel, what, last, ..
        } = edit;
        self.status = format!(
            "{}{rel}: the change to {what} is lost ({message})",
            crate::app::remote_state::NOT_SAVED
        );
        if last
            && let Some(root) = self
                .proj
                .project
                .as_ref()
                .map(|p| p.root.to_string_lossy().into_owned())
        {
            let _ = self.send_to_server(clew_protocol::Request::ReadState { root, rel });
        }
    }

    /// An AI router for background tasks. Endpoint is Server (matching the Hello
    /// handshake); with no server channel it transparently runs calls locally.
    pub(crate) fn ai_client(&self) -> AiClient {
        AiClient {
            endpoint: self.ai_endpoint(),
            server_tx: self.server.tx().cloned(),
            next_id: self.next_req_id.clone(),
            pending: self.server.ai_pending.clone(),
        }
    }

    /// Whether the connected server may hold the AI keys and run AI calls.
    /// Local: yes — the server is this machine, the keys never travel.
    /// Remote: only with the per-host opt-in granted in the Connect form;
    /// otherwise every AI call runs on the client and no key crosses SSH.
    pub(crate) fn ai_on_server(&self) -> bool {
        !self.connection.is_remote() || self.remote_ai_opt_in
    }

    /// The endpoint AI calls should use, per [`Self::ai_on_server`].
    pub(crate) fn ai_endpoint(&self) -> AiEndpoint {
        if self.ai_on_server() {
            AiEndpoint::Server
        } else {
            AiEndpoint::Client
        }
    }

    /// Hand the server the current AI provider config so it can make calls.
    /// For a remote host this is gated on the per-host opt-in: API keys are
    /// credentials, and a host the user hasn't explicitly trusted with them
    /// must never see them.
    pub(crate) fn send_ai_config(&mut self) {
        if !self.server.is_up() {
            return;
        }
        // Sent UNCONDITIONALLY, including as a pair of `None`s. The two cases
        // that most need to reach the server are exactly the two that used to
        // send nothing: the user deleted their API keys, and the user revoked
        // this host's permission to hold them. Both left the server holding —
        // and free to keep using — the old credentials.
        let (chat, embed) = if self.ai_on_server() {
            (
                llm::Config::load().map(|c| clew_protocol::AiChatConfig {
                    provider: c.provider.slug().to_string(),
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                }),
                embed::Config::load().map(|c| clew_protocol::AiEmbedConfig {
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                }),
            )
        } else {
            (None, None)
        };
        // A refused config (a full queue) is said in the status line by
        // `send_message`: a revoke that did not reach the server must not
        // read as done.
        let _ = self.send_to_server(clew_protocol::Request::SetAiConfig { chat, embed });
    }

    /// Where this project's git history is read (see [`GitSource`]): the local
    /// repository, or the server's for a remote project. `None` without a
    /// project.
    pub(crate) fn git_source(&self) -> Option<GitSource> {
        let root = self.proj.project.as_ref()?.root.clone();
        Some(if self.local_project_state() {
            GitSource::Local(root)
        } else {
            GitSource::Remote(self.ai_client())
        })
    }

    /// Whether a server notification about project `root` belongs to the open
    /// project INSTANCE. Notifications carry only a root, so the root must
    /// match AND the server must have been told about this very instance
    /// (`server_epoch`, set when its `Tree` installed the project or when
    /// `sync_project_to_server` handed it a locally scanned one) — the same
    /// root re-opened, or opened by the local fallback before the server knew
    /// it, is another instance the server's watcher is not reporting on.
    pub(crate) fn owns_server_event(&self, root: &str) -> bool {
        self.server_epoch == self.project_epoch
            && self
                .proj
                .project
                .as_ref()
                .is_some_and(|p| p.root.to_string_lossy() == root)
    }

    /// Apply an event from the clew-server. Backend flows are handled here as
    /// they migrate onto the protocol. Returns the follow-up work an event
    /// requires (e.g. the derived-state refresh a watcher change triggers).
    pub(crate) fn handle_server_event(&mut self, event: clew_protocol::Event) -> Task<Message> {
        use clew_protocol::Event;
        let mut task = Task::none();
        match event {
            // Work this client stopped or replaced — a `Chat` it cancelled
            // (see `rpc::Awaiting`), an `OpenProject` a newer open superseded:
            // what it asked for, so nothing to report, and no request of its
            // own is waiting on it.
            Event::Error {
                code: clew_protocol::ErrorCode::Cancelled,
                ..
            } => {}
            Event::Error { message, .. } => {
                // A failure no tracked request owns (a spawn, a stdin write, a
                // request whose caller gave up): stop the picker's spinner
                // only when no listing is in flight, so a stray error cannot
                // un-spin a request that is still coming (the correlated arm
                // owns that).
                if self.server.pending_list_dir.is_none()
                    && let Some(ConnectStage::Browsing(b)) =
                        self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.loading = false;
                }
                self.status = message;
            }
            Event::ChatDelta { stream, text } => {
                if let Some(tx) = self.proj.link.chat_streams.get(&stream) {
                    let _ = tx.send(ChatStreamPiece::Delta(text));
                }
            }
            Event::ChatStreamDone { stream, outcome } => {
                // Finished (or cancelled) on the server: nothing left to stop.
                if self.proj.link.chat_stream == Some(stream) {
                    self.proj.link.chat_stream = None;
                }
                if let Some(tx) = self.proj.link.chat_streams.remove(&stream) {
                    let _ = tx.send(ChatStreamPiece::Done(outcome));
                }
            }
            Event::AgentStep {
                stream,
                tool,
                title,
                refs,
            } => {
                if let Some(tx) = self.proj.link.agent_streams.get(&stream) {
                    let _ = tx.send(AgentPiece::Step(AgentStep {
                        tool,
                        title,
                        refs: refs.into_iter().map(|r| (r.rel, r.line)).collect(),
                    }));
                }
            }
            Event::AgentDelta { stream, text } => {
                if let Some(tx) = self.proj.link.agent_streams.get(&stream) {
                    let _ = tx.send(AgentPiece::Delta(text));
                }
            }
            Event::AgentDone { stream, outcome } => {
                if let Some(tx) = self.proj.link.agent_streams.remove(&stream) {
                    let _ = tx.send(AgentPiece::Done(outcome));
                }
            }
            // An unsolicited notice about the connection's background work
            // (the watcher, a process that died, withheld lsp.toml options).
            Event::Status { message } => {
                self.status = message;
            }
            Event::StateContent {
                root: state_root,
                rel,
                text,
            } => {
                // A remote project's `.clew/` session state, read where the
                // project lives. Applied only for the project it names, and
                // only on a remote connection (local projects load their own
                // files directly).
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if !self.owns_server_event(&state_root) || !self.connection.is_remote() {
                    return Task::none();
                }
                // No longer outstanding, whatever happens below.
                self.proj.remote_state_pending.remove(&rel);
                // A mergeable store with journaled edits still unanswered:
                // this read may predate them, and their replies carry the
                // merged truth. This window's copy — which has them — stays.
                if self.remote_edits_pending(&rel) {
                    return Task::none();
                }
                // A store written wholesale (the trail, the reading target)
                // that this client changed and the remote's disk is not known
                // to have. Their version wins — assigning the loaded one here
                // would silently revert the action they just took, which is
                // exactly how a reconnect used to erase a session's trail.
                if self.proj.remote_state_dirty.contains(&rel) {
                    // With a write of it already on its way, this read
                    // describes the file BEFORE it, and the acknowledgement is
                    // about to arrive: nothing to send.
                    //
                    // Unless the mark ALSO covers a change the server never
                    // received (`remote_state_unsent`, stamped when the
                    // transport carrying it died): then flush this window's
                    // copy, which holds it (see `flush_remote_state`).
                    if !self.remote_state_edit_inflight(&rel)
                        || self.proj.remote_state_unsent.contains(&rel)
                    {
                        self.flush_remote_state(&rel);
                    }
                    return Task::none();
                }
                if let Some(text) = text {
                    self.adopt_remote_state(&root, &rel, &text);
                }
                // A missing file (or an unknown rel) keeps the defaults.
            }
            Event::ProjectSymbols {
                root: snap_root,
                seq,
                full,
                files,
                go_module,
                dart_package,
                structure,
            } => {
                // The server-extracted index data. Applied only for REMOTE
                // projects: a local project builds its own richer, cached
                // index, and the joined paths here are pure identities —
                // nothing ever reads them from this machine's disk.
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if !self.owns_server_event(&snap_root) || !self.connection.is_remote() {
                    return Task::none();
                }
                // Publication order: a full snapshot built during the scan
                // can land AFTER partials the watcher sent while it was
                // building — applying it would clear those fresher files.
                if seq <= self.proj.link.remote_index_seq {
                    return Task::none();
                }
                self.proj.link.remote_index_seq = seq;
                if full {
                    self.proj.symbol_index_by_file.clear();
                    // A full snapshot restates the whole file set, so the
                    // change registry is rebuilt from it below rather than
                    // patched: an entry for a file this publication no longer
                    // lists describes a file that is gone, and keeping it
                    // would leave the derived caches keyed on it.
                    self.proj.registry.clear();
                }
                // Resolution metadata and the type/trait structure index are
                // both extracted where the files live, and arrive as a
                // `Patch`: `Unchanged` means keep what we hold, `Set(None)`
                // means the value is GONE. The two metadata halves are
                // patched INDIVIDUALLY — they share one slot, and replacing
                // the pair wholesale wiped whichever half this publication
                // did not recompute.
                let (mut go, mut dart) =
                    self.proj.remote_import_meta.clone().unwrap_or((None, None));
                let mut meta_changed = false;
                if let clew_protocol::Patch::Set(v) = go_module {
                    go = v;
                    meta_changed = true;
                }
                if let clew_protocol::Patch::Set(v) = dart_package {
                    dart = v;
                    meta_changed = true;
                }
                if meta_changed {
                    self.proj.remote_import_meta = Some((go, dart));
                }
                if let clew_protocol::Patch::Set(index) = structure {
                    // `None`: recomputed, and the project has none.
                    self.proj.structure = index.unwrap_or_default();
                }
                // Whether the tree lists a file, by rel: one lookup per entry
                // against tables built once per file list — a scan of the
                // whole list per entry made a publication cost its size times
                // the project's.
                let listed: Option<HashSet<String>> = (!full).then(|| {
                    self.fresh_citation_index()
                        .map(|idx| {
                            files
                                .iter()
                                .filter(|fs| idx.rels.contains(&fs.rel))
                                .map(|fs| fs.rel.clone())
                                .collect()
                        })
                        .unwrap_or_default()
                });
                let mut graph_files: std::collections::HashMap<PathBuf, imports::FileImports> =
                    std::collections::HashMap::new();
                // Whether the index, and the registry, move: a full snapshot
                // restates them; a partial one only with a file in it — one
                // that carries only metadata or the structure index moves
                // neither, and must not make what is built from them stale
                // (the host-built call graph was asked for again).
                let index_moved = full || !files.is_empty();
                for fs in files {
                    let abs = root.join(&fs.rel);
                    // Change detection for a REMOTE project. This client never
                    // sees the file's bytes, so the registry cannot hold their
                    // hash; what it records instead is the publication that
                    // last reported the file. `seq` only grows and the server
                    // republishes a file exactly when its watcher saw that file
                    // change, so a real change gives the file a new version and
                    // bumps the revision once — and the revision is the
                    // freshness key `stats.rev` and `project_calls.rev` are
                    // compared against. Without it, a remote edit refreshed the
                    // sidebar's symbols while Stats and Project Calls went on
                    // serving pre-edit results, and only the edits that
                    // happened to land in an OPEN file ever invalidated them.
                    //
                    // An empty entry for a rel the tree no longer lists is a
                    // deletion (the same test the import graph uses below). The
                    // membership scan is skipped for a full snapshot, which has
                    // just cleared the registry and lists everything that
                    // exists — and is far too big to scan per file.
                    if !full
                        && fs.symbols.is_empty()
                        && fs.imports.is_empty()
                        && !listed.as_ref().is_some_and(|l| l.contains(&fs.rel))
                    {
                        self.proj.registry.remove(&abs);
                    } else {
                        self.proj.registry.set(abs.clone(), seq);
                    }
                    let raw: Vec<imports::RawImport> = fs
                        .imports
                        .iter()
                        .map(|i| imports::RawImport {
                            module: i.module.clone(),
                            line: i.line,
                            is_mod_decl: i.is_mod,
                        })
                        .collect();
                    if fs.symbols.is_empty() {
                        self.proj.symbol_index_by_file.remove(&abs);
                        graph_files.insert(
                            abs,
                            imports::FileImports {
                                raw,
                                items: Default::default(),
                            },
                        );
                    } else {
                        let entries: Vec<index::SymbolEntry> = fs
                            .symbols
                            .iter()
                            .map(|s| index::SymbolEntry {
                                name: s.name.clone(),
                                kind: s.kind.clone(),
                                rel: fs.rel.clone(),
                                abs: abs.clone(),
                                line: s.line,
                                is_test: s.is_test,
                                entry: s.entry.as_deref().and_then(index::EntryKind::from_key),
                            })
                            .collect::<Vec<_>>();
                        let items = imports::rust_item_keys(&abs, &entries);
                        graph_files.insert(abs.clone(), imports::FileImports { raw, items });
                        self.proj
                            .symbol_index_by_file
                            .insert(abs, Arc::new(entries));
                    }
                }
                if index_moved {
                    self.rebuild_symbol_index();
                }
                // The index arrived, was restated or changed: a refined call
                // graph is handed back to the name-based build at once (a
                // remote project has no incremental refine:
                // `release_stale_remote_refine`), and one in use that was
                // built before it is built again (a build only checks what
                // moved while it ran) — once the import job queued below has
                // resolved the scope it links through
                // (`refresh_call_graph_after_imports`). Asked for at once, it
                // was stale when that job moved the scope: a second
                // whole-project `ProjectCalls` on the host.
                if full {
                    // Build the import graph from the snapshot's extraction,
                    // resolved over the (identity-only) file set; the overview
                    // map is laid out when the resolved graph lands.
                    let graph = self.rebuild_import_graph(graph_files);
                    // While the index still reads as being built, so that a
                    // refinement handed back says the host finished indexing
                    // rather than that files changed.
                    let calls = self.refresh_call_graph_after_imports();
                    self.proj.indexing = false;
                    return Task::batch([graph, calls]);
                }
                // Partial update: the changed files' imports, applied off the
                // UI thread in one batch. An entry with nothing in it is the
                // file's deletion when the tree no longer lists it — decided
                // in the job, against the file set it resolves with. Resolving
                // the changed files is enough unless what OTHER files resolve
                // through changed: a Rust file's declarations or items (the
                // job sees that itself), or the metadata (below). A created or
                // deleted file reaches the file set through `Event::Tree`,
                // which re-resolves everything (`splice_tree`).
                let applied = self.queue_imports(|batch| {
                    for (abs, imports) in graph_files {
                        if imports.raw.is_empty() {
                            batch.set_if_listed(abs, imports);
                        } else {
                            batch.set(abs, imports);
                        }
                    }
                    if meta_changed {
                        batch.reresolve();
                    }
                });
                let calls = if index_moved {
                    self.refresh_call_graph_after_imports()
                } else {
                    Task::none()
                };
                task = Task::batch([applied, calls]);
            }
            Event::FilesChanged {
                root: changed_root,
                rels,
            } => {
                // The server's watcher reports on-disk changes. A late
                // notification from a watcher for a project we have already
                // left must not be applied under the new root.
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if !self.owns_server_event(&changed_root) {
                    return Task::none();
                }
                // Keep the API docs fresh while their tab is open (the docs
                // build runs on the server, so this works for both targets).
                // Deliberately NOT `ensure_docs`: the registry has not learned
                // of this change yet — locally the hash lands with
                // `FilesRehashed`, remotely with the next `ProjectSymbols` — so
                // a freshness test asked here would call the pre-edit index
                // current and skip the rebuild the visible tab needs. The cost
                // is that this build stamps the pre-bump revision and so reads
                // as stale afterwards, buying one extra rebuild the next time
                // the tab is opened. Edits made while another tab is visible
                // are caught by that same tab-entry test.
                if self.sidebar == SidebarTab::Docs && !self.docs_loading() {
                    self.request_docs();
                }
                if self.connection.is_remote() {
                    // Remote: re-request any changed file we still hold a copy
                    // of — in a pane, or in a language server's document
                    // overlay — so the reply can reload the view and resync
                    // the server (`apply_file_refresh` does both). The server
                    // reads it where it lives. Nothing here may read a
                    // remote-pathed file from the local disk.
                    // The index and the graphs are re-derived where the files
                    // live and arrive as a `ProjectSymbols` publication, which
                    // is also what advances the change registry (so Stats and
                    // Project Calls invalidate); the explanations are aged
                    // below, since no server event does that for us.
                    let open: HashSet<PathBuf> = self
                        .proj
                        .panes
                        .iter()
                        .flatten()
                        .map(|v| v.abs.clone())
                        .collect();
                    for rel in &rels {
                        // `lsp_opened` too, not just `open`: the language
                        // server keeps a document from didOpen until a
                        // didClose clew never sends, so a file that has left
                        // the pane still needs its bytes, or every position it
                        // answers about stays pinned to the text as it was
                        // when the file was first opened.
                        let abs = root.join(rel);
                        if open.contains(&abs) || self.proj.link.lsp_opened.contains(&abs) {
                            self.request_file_refresh(rel);
                        }
                    }
                    // A changed source file ages the understanding
                    // (explanations → semantic index → overview) here exactly
                    // as it does at the end of `on_files_rehashed`. The pass
                    // fetches its sources over the protocol, so it reads no
                    // local file; being unreachable from this branch is why a
                    // remote project's explanations only ever refreshed by
                    // hand. Throttled, so an edit burst coalesces into one
                    // pass (see `request_auto_refresh`).
                    let sources: Vec<PathBuf> = rels
                        .iter()
                        .map(|rel| root.join(rel))
                        .filter(|abs| highlight::detect(abs).is_some())
                        .collect();
                    if !sources.is_empty() {
                        self.proj.explain.changed_sources.extend(sources);
                        task = self.request_auto_refresh();
                    }
                    // A tsconfig/jsconfig edited on the host — or a base one
                    // `extends`, whatever its name: its `paths` aliases are
                    // fetched again (they used to be fetched only with a full
                    // snapshot), and resolve once they land. One lookup per
                    // path, however many configs share the chain.
                    let configs = self.proj.remote_ts_configs.as_deref();
                    if rels.iter().any(|rel| {
                        imports::is_resolution_metadata(Path::new(rel))
                            || configs.is_some_and(|c| c.reads(&root.join(rel)))
                    }) {
                        task = Task::batch([task, self.fetch_remote_ts_configs()]);
                    }
                } else {
                    // Local server: the watcher's paths are this machine's
                    // files, so run the FULL derived-state pipeline —
                    // registry, symbol index, import graph, call graphs,
                    // trail re-anchoring, and the throttled explanation /
                    // overview auto-refresh. It also reloads open panes in
                    // place. Without this, an edited import or a new file
                    // left every graph and explanation stale until the
                    // project was reopened. The remote branch above upholds
                    // the same contract by other means: the server re-derives
                    // the index and graphs and publishes them, and the two
                    // pieces it cannot publish (the registry bump, the
                    // explanation refresh) are driven from there.
                    task = self.on_files_changed(rels.iter().map(|rel| root.join(rel)).collect());
                }
            }
            Event::Tree {
                root: tree_root,
                seq,
                tree,
                files,
                tracked_ignored,
                ..
            } => {
                // A structural change (create/delete) from the watcher.
                task = self.splice_tree(&tree_root, seq, tree, files, tracked_ignored);
            }
            Event::ProcessOutput { proc, data } => {
                // Feed a proxied process's stdout into its client's bridge.
                // Never blocking (this is the UI thread), so the feed is
                // bounded (`PROC_FEED_QUEUE`) and a full one gets the explicit
                // policy below instead of a wait.
                let fed = match self.server.proc_feeds.get(&proc) {
                    Some(feed) => feed.try_send(data),
                    None => Ok(()),
                };
                match fed {
                    Ok(()) => {}
                    // The bridge is gone (its client was dropped): stop
                    // routing to it, as its exit would.
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        self.server.proc_feeds.remove(&proc);
                    }
                    // The client stopped reading its output. Dropping a
                    // chunk would corrupt the framed LSP/DAP stream and
                    // buffering more would grow without bound, so the
                    // process is stopped, and said so: dropping the feed
                    // ends the bridge (its client sees EOF and reads as
                    // stopped, restarting on next use), which kills the
                    // process on the server — its one kill
                    // (`tasks::proxy_streams`): a kill sent from here as
                    // well was a second once the client went.
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        self.server.proc_feeds.remove(&proc);
                        let languages: Vec<String> = self
                            .server
                            .lsp_procs
                            .iter()
                            .filter(|(_, p)| **p == proc)
                            .map(|(lang, _)| lang.clone())
                            .collect();
                        self.server.lsp_procs.retain(|_, p| *p != proc);
                        self.status = match languages.first() {
                            Some(lang) => format!(
                                "The {lang} language server's output was not being read — stopped it (it restarts on next use)"
                            ),
                            None => {
                                "A proxied process's output was not being read — stopped it".into()
                            }
                        };
                    }
                }
            }
            Event::ProcessExited { proc, code } => {
                // Dropping the feed closes the bridge, so the LspClient sees EOF.
                self.server.proc_feeds.remove(&proc);
                // Only a proc STILL mapped to a language is that language's live
                // server: a deliberate restart drops the mapping before killing
                // the old child (`start_lsp_with`), so the killed predecessor's
                // late exit finds nothing here and cannot tear down the
                // successor that already replaced it.
                let dead: Vec<String> = self
                    .server
                    .lsp_procs
                    .iter()
                    .filter(|(_, p)| **p == proc)
                    .map(|(lang, _)| lang.clone())
                    .collect();
                self.server.lsp_procs.retain(|_, p| *p != proc);
                for language in dead {
                    // Back to "not started" rather than an immediate respawn: a
                    // server that just died (own crash, or the server's
                    // stdin-overflow kill) would very likely die again, and a
                    // restart loop is worse than none. The next `ensure_lsp` —
                    // the next file open or LSP action — brings it back, and
                    // until then the slot must not keep handing out a client
                    // whose every request fails.
                    self.reset_lsp(&language);
                    self.status = match code {
                        Some(c) => format!(
                            "{language} language server exited (code {c}). It restarts on the next request."
                        ),
                        None => format!(
                            "{language} language server exited. It restarts on the next request."
                        ),
                    };
                }
            }
            // Everything else is a reply, and a reply reaches here only when
            // no tracked request owns it any more — its caller timed out, or
            // a newer request superseded it. Nothing is waiting: drop it.
            _ => {}
        }
        task
    }

    /// Route a correlated server reply to whatever asked for it — by its id:
    /// an awaiting RPC task, or the pane / panel / request bookkeeping the id
    /// was recorded in. A reply nothing owns any more is dropped.
    pub(crate) fn handle_server_reply(
        &mut self,
        id: u64,
        event: clew_protocol::Event,
    ) -> Task<Message> {
        // An RPC reply: hand the event to the task awaiting it, which also
        // reads a server `Error` (and its code) itself.
        if let Some(otx) = crate::app::rpc::lock_replies(&self.server.ai_pending).remove(&id) {
            let _ = otx.send(event);
            return Task::none();
        }
        // Whatever the reply, it retires the request's re-send record; only a
        // not-ready refusal (below) uses it.
        let retry = self.proj.link.not_ready_retry.remove(&id);
        match event {
            // The Hello reply. A server speaking another protocol version
            // can't be used: its frames would fail to deserialize and
            // silently vanish (a remote open then waits forever). Newer
            // servers refuse in their Hello reply; this covers OLDER ones,
            // which happily answer Ready with their own version.
            clew_protocol::Event::Ready {
                protocol,
                fingerprint,
            } => {
                if protocol != clew_protocol::PROTOCOL_VERSION {
                    return self.on_handshake_failed(format!(
                        "clew-server speaks protocol v{protocol}, this clew speaks v{} — \
                         update the server (local: rebuild; remote: it redeploys on reconnect)",
                        clew_protocol::PROTOCOL_VERSION
                    ));
                }
                // Same version number, different protocol BUILD (a wire change
                // whose bump was missed, or a stale sibling/dev binary): its
                // frames would deserialize wrongly or not at all. Refuse now,
                // as one clear error, instead of a session of silent drops.
                if fingerprint != clew_protocol::SCHEMA_FINGERPRINT {
                    return self.on_handshake_failed(format!(
                        "clew-server was built from different protocol sources (server {}, \
                         this clew {}) — rebuild the server (remote: reconnect to redeploy)",
                        fingerprint,
                        clew_protocol::SCHEMA_FINGERPRINT
                    ));
                }
                // The handshake is internal — don't surface version jargon in
                // the status bar; stay quiet until there's something to say.
                self.status.clear();
                self.on_server_ready()
            }
            clew_protocol::Event::FileContent {
                rel,
                source,
                lines,
                symbols,
                docs,
                inactive,
            } => match self.proj.link.pending_reads.remove(&id) {
                // Apply an open only while the pane still waits for this exact
                // load; a later open (or a project switch, which clears the
                // tokens) supersedes it.
                Some(ReadKind::Open { pane, target })
                    if self.proj.link.pane_pending.get(pane).copied().flatten() == Some(id) =>
                {
                    self.proj.link.pane_pending[pane] = None;
                    self.apply_file_content(
                        pane, target, rel, source, lines, symbols, docs, inactive,
                    )
                }
                Some(ReadKind::Refresh { .. }) => {
                    self.apply_file_refresh(rel, source, lines, symbols, docs, inactive)
                }
                _ => Task::none(),
            },
            clew_protocol::Event::NotebookContent {
                rel,
                language,
                cells,
                symbols,
                projection,
            } => match self.proj.link.pending_reads.remove(&id) {
                Some(ReadKind::Open { pane, target })
                    if self.proj.link.pane_pending.get(pane).copied().flatten() == Some(id) =>
                {
                    self.proj.link.pane_pending[pane] = None;
                    self.apply_notebook_content(
                        &[pane],
                        target,
                        rel,
                        language,
                        cells,
                        symbols,
                        projection,
                        false,
                    )
                }
                Some(ReadKind::Refresh { .. }) => {
                    // Reload in place: rebuild EVERY pane showing this notebook,
                    // exactly as `apply_file_refresh` does for a plain file.
                    // Taking only the first match left the other half of a split
                    // painting the pre-edit cells for the rest of the session:
                    // the watcher sends ONE re-read per changed file, and
                    // nothing else ever rebuilds `v.notebook` — only a fresh
                    // open into that pane replaces it. `refresh` keeps each
                    // pane's own scroll and expanded outputs and skips the
                    // open-time side effects.
                    let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                        return Task::none();
                    };
                    let abs = root.join(&rel);
                    let targets: Vec<usize> = self
                        .proj
                        .panes
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.as_ref().is_some_and(|v| v.abs == abs))
                        .map(|(i, _)| i)
                        .collect();
                    if targets.is_empty() {
                        return Task::none();
                    }
                    self.apply_notebook_content(
                        &targets, None, rel, language, cells, symbols, projection, true,
                    )
                }
                _ => Task::none(),
            },
            clew_protocol::Event::Tree {
                root: tree_root,
                seq,
                tree,
                files,
                truncated,
                tracked_ignored,
            } => {
                // Only build the project while we're opening one; a Tree that
                // arrives otherwise answers the OpenProject a (re)connect (or a
                // local-fallback open) re-sent for the project already on
                // screen, and must not re-open it. Discarding it outright was
                // wrong too: on a reconnect this reply is the only report of
                // what changed while the link was down, since the watcher
                // starts from the current state and reports only later events.
                // Splice it in, keeping panes, scroll and Ask history.
                if !self.scanning {
                    if std::mem::take(&mut self.proj.link.pending_tree_resync) {
                        return self.splice_tree(&tree_root, seq, tree, files, tracked_ignored);
                    }
                    return Task::none();
                }
                let Some(root) = self.pending_scan_root.take() else {
                    return Task::none();
                };
                // The reply must describe the project we are waiting for.
                if root.to_string_lossy() != tree_root {
                    self.pending_scan_root = Some(root);
                    return Task::none();
                }
                if self.server.pending_open == Some(id) {
                    self.server.pending_open = None;
                }
                let files = files
                    .into_iter()
                    .map(|rel| fs_scan::FileEntry {
                        abs: root.join(&rel),
                        rel,
                    })
                    .collect();
                // The server says what its walk skipped or added in an
                // `Event::Status` of its own; the tracked files its walk
                // would have hidden come with the tree, to be marked.
                let opened = self.on_scan_done(
                    ScanResult {
                        root,
                        tree,
                        files,
                        truncated,
                    },
                    Default::default(),
                );
                self.proj.tracked_ignored = tracked_ignored.into_iter().collect();
                // The server scanned this project for this open: it serves
                // this instance, so its notifications apply to it — from this
                // scan on.
                self.server_epoch = self.project_epoch;
                self.proj.link.server_tree_seq = self.proj.link.server_tree_seq.max(seq);
                opened
            }
            clew_protocol::Event::LspResolved {
                language,
                root: resolved_root,
                resolution,
            } => {
                // Reply to the remote ensure_lsp (or a finished remote
                // install): each resolution state demands its own action —
                // start, raise the approval modal, raise the install-consent
                // modal, or give up with the server's reason.
                if !matches!(
                    self.proj.link.lsp.get(&language),
                    Some(LspSlot::AwaitingConsent)
                ) {
                    return Task::none(); // superseded (project switch, restart)
                }
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                // A resolution computed for another project (a late reply
                // that straddled an A→B switch) must not drive THIS
                // project's approval or install consent.
                if resolved_root != root.to_string_lossy() {
                    return Task::none();
                }
                let host = self.connection.approval_host().map(str::to_string);
                use clew_protocol::LspResolution;
                match resolution {
                    LspResolution::Ready {
                        init_options,
                        withheld,
                    } => {
                        // Stashed first, and with the APPROVED options only:
                        // when something was withheld this is `None`, which
                        // also drops any entry an earlier resolve left, so a
                        // start can never pick up options the server refused.
                        self.stash_remote_init(&language, init_options);
                        // The host's lsp.toml asks for options it has not been
                        // approved for. Ask, with the same modal the `Command`
                        // arm and the local options-only path use — the slot
                        // stays `AwaitingConsent`, so the reply to the resolve
                        // that Allow re-issues is not dropped as superseded.
                        //
                        // Asked every time rather than short-circuiting on an
                        // approval this client already holds: the server is the
                        // one that decides, and re-pushing plus re-resolving on
                        // its refusal is a loop with no bound. One extra click
                        // repairs a desync (the allow re-sends the whole set).
                        if let Some(spec) = withheld {
                            // Typed on the wire (a JSON value, not text the
                            // client re-parses), so the options always render:
                            // the modal never asks about nothing.
                            let shown = crate::app::lsp::pretty_init_options(Some(&spec.options));
                            self.proj.pending_lsp_command = Some(PendingLspCommand {
                                root,
                                host,
                                language,
                                // No repo-named command: what runs is the
                                // host's store-installed server, covered by
                                // the install consent. The question here is
                                // about the options alone.
                                command: None,
                                args: spec.args,
                                server_name: spec.server,
                                version: spec.version,
                                fingerprint: spec.fingerprint,
                                init_options: shown,
                            });
                            return Task::none();
                        }
                        // Nothing was withheld: start with what was approved.
                        self.proj.link.lsp.remove(&language);
                        // The exe path is unused on the remote spawn path.
                        self.start_lsp_with(&language, PathBuf::new())
                    }
                    LspResolution::Command(spec) => {
                        if self.trust.is_lsp_approved(
                            host.as_deref(),
                            &root,
                            &language,
                            &spec.fingerprint,
                        ) {
                            // Already approved: refresh the server's set, start.
                            self.stash_remote_init(&language, spec.init_options);
                            self.send_lsp_approvals();
                            self.proj.link.lsp.remove(&language);
                            self.start_lsp_with(&language, PathBuf::new())
                        } else {
                            // Shown before stashing: the remote's options ride
                            // this same fingerprint, so they are part of what
                            // is being approved and must be visible.
                            let shown = spec.init_options.clone();
                            self.stash_remote_init(&language, spec.init_options);
                            self.proj.pending_lsp_command = Some(PendingLspCommand {
                                root,
                                host,
                                language,
                                command: Some(PathBuf::from(&spec.command)),
                                args: spec.args,
                                server_name: spec.server,
                                version: spec.version,
                                fingerprint: spec.fingerprint,
                                init_options: crate::app::lsp::pretty_init_options(shown.as_ref()),
                            });
                            Task::none()
                        }
                    }
                    LspResolution::NeedsInstall {
                        server,
                        version,
                        describe,
                        consent,
                    } => {
                        // Slot stays AwaitingConsent; on Allow the client
                        // sends `LspInstall` and the reply lands right here.
                        self.proj.pending_lsp_consent.offer(LspConsent {
                            language,
                            server_name: server,
                            version,
                            provision: LspProvision::Remote { describe, consent },
                            dest_dir: PathBuf::new(),
                        });
                        Task::none()
                    }
                    LspResolution::Unsupported { message } => {
                        self.proj
                            .link
                            .lsp
                            .insert(language, LspSlot::Unsupported(message));
                        Task::none()
                    }
                }
            }
            clew_protocol::Event::SearchResults {
                hits,
                error,
                skipped,
                skipped_total,
            } => {
                // A search reply: apply only while it is still the latest
                // submission (a newer one replaced `pending_search`).
                if self.proj.link.pending_search != Some(id) {
                    return Task::none();
                }
                self.proj.link.pending_search = None;
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let hits = hits
                    .into_iter()
                    .map(|h| search::SearchHit {
                        abs: root.join(&h.rel),
                        rel: h.rel,
                        line: h.line,
                        preview: h.preview,
                    })
                    .collect();
                self.apply_search_result(
                    search::SearchReport {
                        hits,
                        error,
                        skipped,
                    },
                    skipped_total,
                );
                Task::none()
            }
            // Blame for the file this request named. Re-deriving the path
            // from the current root and the reply's `rel` would, after a
            // project switch, paint a DIFFERENT project's same-named file.
            clew_protocol::Event::GitInfo { info, .. } => {
                let Some(abs) = self.proj.link.pending_git.remove(&id) else {
                    self.proj.link.retired_git.remove(&id);
                    return Task::none();
                };
                self.on_git_info_loaded(abs, id, Ok(info.map(Arc::new)))
            }
            // The folder picker's listing, applied only while it is the one
            // being waited for: two quick clicks used to let the slower,
            // earlier reply overwrite the newer directory.
            clew_protocol::Event::DirListing {
                path,
                parent,
                entries,
                omitted,
            } => {
                if self.server.pending_list_dir != Some(id) {
                    return Task::none();
                }
                self.server.pending_list_dir = None;
                // A partial listing says so: the entries past the server's
                // cap (files — directories are listed first) are not shown.
                if omitted > 0 {
                    self.status = format!(
                        "{path}: showing {} of {} entries",
                        entries.len(),
                        entries.len() + omitted
                    );
                }
                if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.cwd = path;
                    b.parent = parent;
                    b.entries = entries;
                    b.omitted = omitted;
                    b.loading = false;
                }
                Task::none()
            }
            // The API docs index, for the build this window is waiting on —
            // correlated by id, so a build abandoned by a project switch or a
            // reconnect can never land on the next one. The root check is the
            // belt to that: the build is slow.
            clew_protocol::Event::Docs { root, files } => {
                if self.proj.link.pending_docs != Some(id) || !self.owns_server_event(&root) {
                    return Task::none();
                }
                self.apply_docs(files);
                Task::none()
            }
            // A refusal correlated to a tracked request (e.g. the server's
            // not-ready answer during its scan window): stop the matching
            // spinner — the generic Error arm only sets the status line, and
            // the panels would otherwise load forever.
            // The bytes reached the remote's disk. Only now is the change
            // durable, so only now may its unsaved mark come off.
            clew_protocol::Event::StateWritten { rel, .. } => {
                // Keyed on the in-flight id, not the rel alone: a NEWER write
                // of the same file supersedes this one and owns the mark, so a
                // late acknowledgement must not clear it.
                if self.proj.link.remote_state_inflight.remove(&id).as_deref() == Some(rel.as_str())
                {
                    self.proj.remote_state_dirty.remove(&rel);
                }
                // The unsent mark is retired on its OWN record, not on the one
                // above: these are the bytes this window holds, so a change an
                // earlier transport ate is in them and the remote file is whole
                // again — and that stays true even when a newer write made
                // inside the round trip has since taken the dirty mark's
                // ownership away from this id (see `remote_state_rescue`). The
                // dirty mark is left to that newer write, whose own reply the
                // ordered state worker sends after this one.
                if self.proj.link.remote_state_rescue.remove(&id).as_deref() == Some(rel.as_str()) {
                    self.proj.remote_state_unsent.remove(&rel);
                }
                Task::none()
            }
            // The merged file, after the server applied this client's change
            // on what was actually on the remote's disk. It — not this
            // window's copy — is the truth, the same way the local
            // `bookmarks::edit` returns the merged list its caller adopts.
            clew_protocol::Event::StateEdited {
                root: state_root,
                rel,
                text,
            } => {
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if root.to_string_lossy() != state_root || !self.connection.is_remote() {
                    return Task::none();
                }
                // The answered edit leaves the journal. Only the answer to the
                // LAST edit of the store still waiting is adopted: an earlier
                // one describes the file before the edits after it, and
                // adopting it rolled this window's copy back below its own
                // optimistic changes — the entry the user had just acted on
                // came back until the newer reply landed, and a re-press in
                // that window sent a TOGGLE the server applied to its newer
                // truth, deleting what the user meant to keep. Nothing is
                // lost by waiting: the state worker applies edits in order,
                // so the last answer carries the cumulative merge, another
                // client's writes included. (A replayed edit the server had
                // already applied is answered the same way, with the file.)
                if let Some((edited, true)) = self.settle_remote_edit(id)
                    && edited == rel
                {
                    // `None` = the merge emptied the store and its file was
                    // deleted, which for every mergeable store is an empty list.
                    self.adopt_remote_state(&root, &rel, text.as_deref().unwrap_or("[]"));
                }
                Task::none()
            }
            clew_protocol::Event::Error { code, message } => {
                // The refusal a mismatched handshake actually produces: the
                // server checks OUR version and fingerprint first, so it
                // answers `Error` instead of the `Ready` the arm above
                // inspects — and then refuses every later request too. Take
                // the same fallback, or the parked scan is stranded and the
                // window never opens a project again.
                if id == crate::app::server::HELLO_REQ_ID {
                    return self.on_handshake_failed(message);
                }
                // The open this window waits on could not be done.
                if self.server.pending_open == Some(id) {
                    self.server.pending_open = None;
                    return self.on_open_project_failed(code, message);
                }
                // One sent to be sent again on a not-ready refusal that this
                // window no longer waits on — a newer search took its place,
                // or the docs tab's own refresh (`waits_for`): its refusal,
                // whatever it says, answers nothing asked for now. Said, a
                // search's "not ready" showed while the one that replaced it
                // ran.
                if retry.is_some() && !self.waits_for(id) {
                    return Task::none();
                }
                // The scan window: a request this window still waits on is
                // sent again after a backoff, and the refusal is not shown
                // (see `send_retrying`) — until the retries run out.
                if code == clew_protocol::ErrorCode::NotReady
                    && let Some(retry) = retry
                    && retry.attempt < NOT_READY_RETRIES
                    && self.waits_for(id)
                {
                    let stamp = self.transport_stamp();
                    let attempt = retry.attempt + 1;
                    let request = retry.request;
                    let delay = NOT_READY_BACKOFF * 2u32.pow(retry.attempt);
                    // Built inside the future: a timer needs the runtime,
                    // which the update loop is not running on.
                    return Task::perform(
                        async move { tokio::time::sleep(delay).await },
                        move |()| {
                            Message::Server(ServerMsg::ResendNotReady {
                                stamp: stamp.clone(),
                                id,
                                request: request.clone(),
                                attempt,
                            })
                        },
                    );
                }
                // A refused blame is the pass's answer: reported, and the
                // gutter it would have repainted cleared, exactly like a local
                // pass git could not answer (`on_git_info_loaded`) — the old
                // gutter describes bytes the pane no longer shows.
                if let Some(abs) = self.proj.link.pending_git.remove(&id) {
                    return self.on_git_info_loaded(abs, id, Err(message));
                }
                // A blame retired while in flight: its refusal answers a
                // question nobody is asking any more.
                if self.proj.link.retired_git.remove(&id) {
                    return Task::none();
                }
                // A journaled edit the server could not apply THIS time (an
                // I/O error, a lock, a panic — `Failed`): it stays in the
                // journal and goes again, a few times, before it is given up.
                if code == clew_protocol::ErrorCode::Failed
                    && let Some(failed) = self.retry_remote_edit(id)
                {
                    match failed.fate {
                        crate::app::remote_state::Fate::Retried => {
                            self.status = crate::app::remote_state::retrying_status(
                                &failed.rel,
                                &failed.what,
                                &message,
                            );
                            self.proj.remote_edit_retrying = Some((failed.id, self.status.clone()));
                        }
                        // A later change to the entry takes its place, and
                        // goes out in its turn: nothing to say.
                        crate::app::remote_state::Fate::Superseded => {}
                        crate::app::remote_state::Fate::GivenUp => {
                            self.edit_not_saved(failed, &message);
                        }
                    }
                    return Task::none();
                }
                // A journaled edit the server refused (a store it cannot
                // understand — `Refused`): it will not apply on any transport,
                // so it leaves the journal, and is reported as not saved.
                if let Some(refused) = self.give_up_remote_edit(id) {
                    self.edit_not_saved(refused, &message);
                    return Task::none();
                }
                // A write that failed stays dirty: the change is still only in
                // this client, so the next re-read must not overwrite it. Same
                // for the unsent mark, whose rescue record dies with the
                // request that would have retired it — the refused bytes never
                // reached the disk, so the change is still missing from it.
                self.proj.link.remote_state_inflight.remove(&id);
                self.proj.link.remote_state_rescue.remove(&id);
                // Stopped because this client no longer wants it: the
                // spinners below still stop, but nothing reads as an error.
                let cancelled = code == clew_protocol::ErrorCode::Cancelled;
                let mut correlated = false;
                if self.end_awaited(id, (!cancelled).then(|| message.clone())) {
                    correlated = true;
                }
                if self.server.pending_list_dir == Some(id) {
                    self.server.pending_list_dir = None;
                    if let Some(ConnectStage::Browsing(b)) =
                        self.connect.as_mut().map(|u| &mut u.stage)
                    {
                        b.loading = false;
                    }
                    correlated = true;
                }
                // A file the server could not read — a symlink at its name, a
                // folder, a file gone since it was asked for: the pane that
                // waited for it waits for nothing any more, as when a read of
                // its own fails (`on_file_loaded`). Left waiting, a later
                // time-travel start in the pane took the dead load for an
                // open to carry out, and asked for the file again.
                if let Some(read) = self.proj.link.pending_reads.remove(&id) {
                    match read {
                        ReadKind::Open { pane, .. }
                            if self.proj.link.pane_pending.get(pane).copied().flatten()
                                == Some(id) =>
                        {
                            self.proj.link.pane_pending[pane] = None;
                            self.proj.link.pane_opening[pane] = None;
                        }
                        // An open a later request took the place of: the pane
                        // shows, or waits for, what was asked for since, and
                        // nothing is said of this one — as its `FileContent`
                        // would have been dropped. Said, it took over the
                        // status line while another file was on screen. One
                        // a time-travel start took over keeps what it says,
                        // to be said should the open be carried out after
                        // all (`fail_superseded_open`).
                        ReadKind::Open { .. } => {
                            if !cancelled {
                                self.fail_superseded_open(id, message);
                            }
                            return Task::none();
                        }
                        // A refresh a newer one retired, or one of a file no
                        // pane shows any more — kept up, perhaps, for a
                        // language server's copy of it, which its
                        // `FileContent` would have brought up to date:
                        // nothing on screen asked, and the reader, in another
                        // file, is not told of one they are not reading.
                        // That copy stays as it was, as it does for a file
                        // gone from the project.
                        ReadKind::Retired => return Task::none(),
                        ReadKind::Refresh { rel } if !self.shows_rel(&rel) => {
                            return Task::none();
                        }
                        ReadKind::Refresh { .. } => {}
                    }
                    correlated = true;
                }
                if correlated {
                    if !cancelled {
                        self.status = message;
                    }
                    return Task::none();
                }
                // Asked for the project since left, whose records of it went
                // with it: nothing here waits for it.
                if id < self.session_first_req {
                    return Task::none();
                }
                self.handle_server_event(clew_protocol::Event::Error { code, message })
            }
            other => self.handle_server_event(other),
        }
    }
}

impl App {
    /// Handle a [`ServerMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_server(&mut self, message: ServerMsg) -> Task<Message> {
        match message {
            // Transport messages from a dead or replaced connection (their
            // subscription already dropped, but its channel still held them)
            // must not act on the current one: a late Connected would install
            // the old host's request channel, a late Disconnected would tear
            // down a healthy transport, late events would apply another
            // project's state.
            ServerMsg::Connected { tx, .. } => self.on_server_connected(tx),
            ServerMsg::Disconnected { reason, .. } => self.on_server_disconnected(reason),
            ServerMsg::Unavailable { reason, .. } => self.on_server_unavailable(reason),
            ServerMsg::HostKeyUnknown { reason, key, .. } => self.on_host_key_unknown(reason, key),
            ServerMsg::HostKeyChanged {
                reason,
                forget_host,
                ..
            } => self.on_host_key_changed(reason, forget_host),
            ServerMsg::RegisterProcFeed { proc, feed, .. } => {
                self.server.proc_feeds.insert(proc, feed);
                Task::none()
            }
            ServerMsg::ResendNotReady {
                id,
                request,
                attempt,
                ..
            } => self.on_resend_not_ready(id, request, attempt),
            // (An event queued by a transport this window has already left was
            // dropped in `dispatch`, like the lifecycle messages.)
            ServerMsg::Event { msg, .. } => match msg {
                clew_protocol::ServerMessage::Reply { id, event, .. } => {
                    self.handle_server_reply(id, event)
                }
                clew_protocol::ServerMessage::Notification { event, .. } => {
                    self.handle_server_event(event)
                }
            },
        }
    }
}
