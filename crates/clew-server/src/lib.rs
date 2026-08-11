//! clew-server: the headless backend.
//!
//! It owns all filesystem / OS interaction and answers the client over
//! `clew-protocol`. The same `Server` logic runs whether the transport is a
//! local child process (stdio) or an SSH session to a remote host — the client
//! only ever speaks the protocol, so local and remote are indistinguishable to
//! it. Backend flows migrate onto `Server::handle` one at a time; today it
//! scans a project and answers text searches.

pub mod agent;
pub mod agent_lsp;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clew_core::fs_scan::FileEntry;
use clew_core::{docs, embed, git, highlight, inactive, llm, outline, search};
use clew_protocol::{ClientMessage, Event, PROTOCOL_VERSION, Request, ServerMessage};
use notify_debouncer_full::new_debouncer_opt;
use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::UnboundedSender;

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
struct Proc {
    input: tokio::sync::mpsc::Sender<Vec<u8>>,
    child: Option<tokio::process::Child>,
    /// Which registration this entry is. `proc` is chosen by the CLIENT, so
    /// the same handle can be registered twice; without this, the first
    /// process's stdout reader would deregister the second on exit and report
    /// the live one dead.
    generation: u64,
}

/// Stdin backlog per process (messages, each ≤ one client frame). A child
/// that stopped reading hits this quickly. Overflow is not survivable for the
/// stream — losing one frame desyncs `Content-Length` framing forever — so a
/// full queue kills the process and reports it instead of dropping bytes or
/// queueing without bound until the OOM killer picks the server.
const PROC_INPUT_QUEUE: usize = 256;

/// Cap on ONE `ProcessInput` message. The queue above bounds how many
/// messages can be outstanding, not how big each is — and a client frame may
/// be up to the protocol's 256 MB, so the two limits multiplied to something
/// no machine can hold. A real message is one LSP or DAP frame, whose own
/// limit is 64 MB.
const MAX_PROC_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Largest regular file `ReadFile` will serve — matching the client viewer's
/// own display limit, checked BEFORE reading so the size can't balloon the
/// reply first.
const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
/// Notebooks embed base64 images, so their JSON runs far past source-file
/// sizes; still bounded.
const MAX_NOTEBOOK_BYTES: u64 = 64 * 1024 * 1024;
/// Backpressure for proxied child stdout. The out channel is unbounded, so a
/// child spewing output faster than the transport drains it would grow the
/// queue without limit; this tracks the `ProcessOutput` bytes still queued
/// and parks the stdout pumps while over the cap — which in turn stops
/// reading the child's pipe, pushing the pressure back into the child.
pub struct OutputBudget {
    bytes: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

impl OutputBudget {
    /// Total ProcessOutput bytes allowed in flight at once.
    const CAP: usize = 32 * 1024 * 1024;

    fn new() -> Arc<Self> {
        Arc::new(OutputBudget {
            bytes: std::sync::atomic::AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    /// Wait until the queue is under the cap, then charge `n` bytes.
    async fn charge(&self, n: usize) {
        use std::sync::atomic::Ordering;
        loop {
            // Register for the wakeup BEFORE checking, so a release between
            // the check and the await can't be missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.bytes.load(Ordering::Relaxed) <= Self::CAP {
                self.bytes.fetch_add(n, Ordering::Relaxed);
                return;
            }
            notified.await;
        }
    }

    /// Credit `n` bytes back once the message left the queue (was written to
    /// the transport, or dropped with it).
    pub fn release(&self, n: usize) {
        use std::sync::atomic::Ordering;
        self.bytes.fetch_sub(n, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

/// One queued `.clew/` state operation. All state ops run on ONE ordered
/// worker so a read after a write — and two rapid writes of the same file —
/// apply in request order, while the (blocking) filesystem work stays off the
/// request loop.
struct StateJob {
    root: PathBuf,
    rel: String,
    id: clew_protocol::RequestId,
    work: StateWork,
}

enum StateWork {
    /// Read, replied as `StateContent`.
    Read,
    /// Replace the file wholesale, or delete it (`None`).
    Write(Option<String>),
    /// Read-modify-write ONE entry, replied as `StateEdited` with the merged
    /// file. The worker's ordering is what makes this atomic against the other
    /// requests of this connection; against a SECOND clew-server on the same
    /// host the file lock inside the merge is (see `run_merge`).
    Merge(clew_protocol::StateMerge),
}

/// Debounce window: coalesces the burst a single save or `git pull` produces.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// The concrete debouncer type, held to keep the watch thread alive.
///
/// `NoCache`, not `RecommendedCache`. The cache's job is to stitch a rename's
/// two halves together by file id, and to do that it walks the ENTIRE root on
/// the calling thread at `watch()` time — unfiltered, `follow_links(true)`, one
/// `stat` per entry — then retains a map entry per path for the watcher's life
/// (measured at 673k entries on this repository). That cost buys nothing here:
/// the callback below never reads a stitched rename. It tests the event KIND
/// only, to decide `structural`, and then recovers what actually changed by
/// diffing the file set before and after a rescan — which catches the vacated
/// path and the new one whether the platform reported them as one event or two.
/// Linux already built `NoCache`; this makes macOS agree.
///
/// The claim that stitching is redundant is what
/// `renaming_a_directory_updates_its_descendants_symbols` and
/// `search_sees_files_created_after_open` in `tests/protocol.rs` check; both
/// drive the watcher end to end and both pass on macOS without the cache.
type Watcher = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::NoCache,
>;

/// The open project's scanned file list, tagged with the root it belongs to.
/// Shared between the request loop and the watcher (which refreshes it after a
/// structural change), so search / docs / agent turns always grep the current
/// file set instead of the one from the last `OpenProject`.
struct ProjectFiles {
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
}

type SharedFiles = Arc<Mutex<Option<ProjectFiles>>>;
type SharedProcs = Arc<tokio::sync::Mutex<HashMap<u64, Proc>>>;

/// Client-granted approvals for repo-specified language-server commands:
/// `language` → the approved fingerprint (see `trust::lsp_fingerprint`).
/// Shared with the agent's LSP pool so **every** spawn path checks the same
/// gate — the GUI's SpawnLsp and the Ask agent's semantic tools alike.
pub type SharedApprovals = Arc<Mutex<HashMap<String, String>>>;

/// The one decision point for repo-specified language-server commands: may
/// this exact command run for `language` in `root`? Approved when the client
/// pushed a matching fingerprint (`LspApprovals`), or when this host's own
/// trust store records one (the local-server case, where client and server
/// share a machine). Errors name the reason — including a fingerprint that
/// can't be computed (unreadable command).
///
/// Returns the path to SPAWN: clew's own copy of the approved bytes, taken
/// from the same handle they were hashed from. The repository's path is never
/// executed — hashing a name and then spawning that name is a check-to-exec
/// race the repository wins by replacing the leaf, the symlink, or a parent
/// directory in between.
///
/// Takes the whole resolved `server` rather than its parts, because the
/// fingerprint now covers `init_options` too: those options come from the same
/// repo-shipped `lsp.toml` as the command, several servers run programs named
/// in them, and the caller hands the very same `server.init_options` to
/// `initialize`. Passing the struct is what keeps approved-and-run in step —
/// with loose fields a caller could fingerprint one set of options and send
/// another. `command` is passed alongside because the caller has already
/// established that `server.command` is `Some` (the `None` case never gets
/// here, and never asks for approval at all).
pub fn lsp_command_allowed(
    approvals: &SharedApprovals,
    root: &Path,
    server: &clew_core::lsp::config::EffectiveServer,
    command: &Path,
) -> Result<PathBuf, String> {
    let language = server.language.as_str();
    let staged = clew_core::trust::stage_lsp_command(
        root,
        command,
        &server.args,
        &server.server_name,
        &server.version,
        server.init_options.as_ref(),
        |fingerprint| {
            approvals
                .lock()
                .unwrap()
                .get(language)
                .is_some_and(|f| f == fingerprint)
                || clew_core::trust::Trust::load().is_lsp_approved(
                    None,
                    root,
                    language,
                    fingerprint,
                )
        },
    )
    .map_err(|e| format!("cannot fingerprint the {language} server command: {e}"))?;
    staged.exec_path.ok_or_else(|| {
        format!(
            "refused: this project's lsp.toml command for {language} is not approved — \
             open a {language} file in clew and approve it there"
        )
    })
}

/// The other half of what a repo's `lsp.toml` decides: the `init_options` it
/// asks clew to put in `initialize`. Returns the options that may be sent,
/// and — when they were withheld — the reason, for the caller to report.
///
/// These need approval in their own right. A config that sets options and no
/// `command` names no bytes to hash, so it used to reach `initialize` with no
/// consent step of any kind: the binary came from the store (consented at
/// install), and the options went out verbatim. They are the same
/// attacker-chosen input as a `command` — rust-analyzer runs
/// `cargo.buildScripts.overrideCommand` on workspace load, pyright executes
/// `python.pythonPath` to enumerate `sys.path` — so cloning a repository and
/// opening one file was code execution on this host.
///
/// Withheld rather than fatal, which is where this deliberately differs from
/// [`lsp_command_allowed`]: nothing reachable from here can ask the user
/// anything (a headless backend, and the agent's pool has no UI at all), so
/// refusing to start would take the language server away for a config that may
/// be perfectly legitimate. A server running with clew's own defaults is the
/// smaller loss. The approval itself is granted in the client, and arrives
/// here as `LspApprovals` — [`Server::resolve_lsp`] hands the client what that
/// approval needs, so a withheld config can be allowed rather than being stuck.
/// The fingerprint for the options-only shape (`init_options`, no `command`).
///
/// One derivation, two callers on purpose: the gate ([`approved_init_options`])
/// decides with it, and [`Server::resolve_lsp`] puts it in front of the user as
/// the value to approve. If those two computed it separately and ever drifted,
/// approving would record a fingerprint the gate does not recognise and the
/// modal would come straight back with no way out of the loop.
fn options_only_fingerprint(
    server: &clew_core::lsp::config::EffectiveServer,
    options: &serde_json::Value,
) -> Result<String, String> {
    clew_core::trust::lsp_options_fingerprint(
        &server.args,
        &server.server_name,
        &server.version,
        options,
    )
}

pub fn approved_init_options(
    approvals: &SharedApprovals,
    root: &Path,
    server: &clew_core::lsp::config::EffectiveServer,
) -> (Option<serde_json::Value>, Option<String>) {
    let Some(options) = server.init_options.clone() else {
        return (None, None); // nothing repo-controlled to approve
    };
    let language = server.language.as_str();
    // One invariant, two shapes of approval: the options in hand must be
    // covered by a fingerprint on record. With a `command` they are inside
    // that command's fingerprint; without one they are hashed alone. The
    // command case is re-derived here rather than assumed from the caller's
    // earlier `lsp_command_allowed`, so a config that changed underneath in
    // between withholds the options instead of inheriting an answer given
    // about different ones.
    let fingerprint = match &server.command {
        Some(command) => clew_core::trust::lsp_fingerprint(
            root,
            command,
            &server.args,
            &server.server_name,
            &server.version,
            Some(&options),
        ),
        None => options_only_fingerprint(server, &options),
    };
    let fingerprint = match fingerprint {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            return (
                None,
                Some(format!(
                    "this project's lsp.toml init_options for {language} cannot be \
                     fingerprinted ({e}) — they were not sent to the server"
                )),
            );
        }
    };
    // Same two sources as the command gate: the client's pushed set, or this
    // host's own trust store when client and server share a machine.
    let approved = approvals
        .lock()
        .unwrap()
        .get(language)
        .is_some_and(|f| *f == fingerprint)
        || clew_core::trust::Trust::load().is_lsp_approved(None, root, language, &fingerprint);
    if approved {
        return (Some(options), None);
    }
    (
        None,
        Some(format!(
            "this project's lsp.toml init_options for {language} are not approved — \
             they were NOT sent to the language server"
        )),
    )
}

/// Backend state. Grows as each flow migrates onto the protocol; today it owns
/// the scanned project (for search/read) and watches it for changes.
pub struct Server {
    /// Root of the currently open project; `rel` paths resolve against it.
    root: Option<PathBuf>,
    /// Flat file list of the open project — what search greps over. Shared
    /// with the watcher, which swaps in a fresh scan on structural changes.
    files: SharedFiles,
    /// Channel to push replies and unsolicited notifications (e.g. file changes).
    out: UnboundedSender<ServerMessage>,
    /// Live filesystem watcher for the open project; held (in a shared slot,
    /// since the async OpenProject task installs it) to keep it running.
    _watcher: Arc<Mutex<Option<Watcher>>>,
    /// Bumped per OpenProject. The async open task re-checks it before
    /// committing files/watcher/reply, so a superseded open changes nothing.
    open_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Subprocesses spawned for the client (language servers, debug adapters),
    /// keyed by the client-assigned handle. Shared (an async mutex) so a
    /// provisioning task can register the process it spawned after its
    /// download finished off the request loop.
    procs: SharedProcs,
    /// Backpressure for the proxied processes' stdout (see [`OutputBudget`]).
    proc_out_budget: Arc<OutputBudget>,
    /// The ordered `.clew/` state worker's queue (see [`StateJob`]).
    state_jobs: UnboundedSender<StateJob>,
    /// AI provider config to use when the server makes calls (endpoint = Server).
    ai_chat: Option<llm::Config>,
    ai_embed: Option<embed::Config>,
    /// Stop flags for in-flight cancellable work — agent turns and streamed
    /// chats — keyed by the client's stream id. Both kinds share one map
    /// because both ids come from the client's single request counter, so
    /// `Cancel` and `AgentStop` can address either without knowing which it
    /// is. The blocking tasks remove themselves when done.
    agents: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// Client-granted approvals for repo-specified LSP commands (see
    /// [`lsp_command_allowed`]). Replaced by `LspApprovals`, cleared on
    /// `OpenProject` (approvals are per-project).
    lsp_approvals: SharedApprovals,
    /// Language servers backing the agent's semantic tools. Lazily created for
    /// the open project on the first agent turn; replaced when the root changes.
    agent_lsp: Option<Arc<agent_lsp::LspPool>>,
    /// Publication counter for `ProjectSymbols`. Full snapshots (built off
    /// the request loop) and watcher partials are sent from different
    /// threads; stamping under this lock at send time gives the client a
    /// total order to drop stale events against (see `send_project_symbols`).
    index_seq: Arc<Mutex<u64>>,
    /// Set by a `Hello` whose protocol version matched; cleared by one that
    /// didn't. While false — before any Hello, or after a failed one — every
    /// non-Hello request is refused: the peer cannot parse half our frames
    /// anyway, and serving the half it can parse turns one clear error into
    /// a session of confusing ones.
    hello_ok: bool,
}

impl Server {
    /// Create a server that emits messages on `out`. Must run inside a tokio
    /// runtime (it spawns the ordered state worker).
    pub fn new(out: UnboundedSender<ServerMessage>) -> Self {
        let state_jobs = spawn_state_worker(out.clone());
        Server {
            root: None,
            files: Arc::new(Mutex::new(None)),
            out,
            _watcher: Arc::new(Mutex::new(None)),
            open_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            procs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            proc_out_budget: OutputBudget::new(),
            state_jobs,
            ai_chat: None,
            ai_embed: None,
            agents: Arc::new(Mutex::new(HashMap::new())),
            lsp_approvals: Arc::new(Mutex::new(HashMap::new())),
            agent_lsp: None,
            index_seq: Arc::new(Mutex::new(0)),
            hello_ok: false,
        }
    }

    /// Overwrite the stored API keys before dropping them.
    ///
    /// Best effort, and honest about it: the same secret has already been
    /// cloned into request bodies and TLS buffers this process cannot reach.
    /// What it does buy is that a key the user revoked is not left sitting in
    /// the server's own long-lived config for the rest of the session.
    fn wipe_ai_keys(&mut self) {
        fn scrub(s: &mut String) {
            // `into_bytes` hands back the SAME allocation, so filling it
            // overwrites the bytes the key actually occupied before the
            // buffer is freed. `black_box` stops the optimizer from removing
            // a write to memory it can see is about to be dropped.
            let mut bytes = std::mem::take(s).into_bytes();
            bytes.fill(0);
            std::hint::black_box(&bytes);
        }
        if let Some(c) = &mut self.ai_chat {
            scrub(&mut c.api_key);
        }
        if let Some(c) = &mut self.ai_embed {
            scrub(&mut c.api_key);
        }
        self.ai_chat = None;
        self.ai_embed = None;
    }

    /// The open project's root, or the refusal to reply with when none is
    /// open. A request that needs a project before any `OpenProject` used to
    /// fall through `?` into silence — no reply at all — which left the
    /// client waiting forever (and leaking its pending-request entry).
    /// (Boxed refusal: `Event` is large, and clippy rightly objects to fat
    /// `Err` variants on a hot call.)
    fn root_or_refuse(&self) -> Result<PathBuf, Box<Event>> {
        self.root.clone().ok_or_else(|| {
            Box::new(Event::Error {
                message: "refused: no project open".into(),
            })
        })
    }

    /// Wait (bounded) for `root`'s file list to commit — an `OpenProject`
    /// scan may still be running when a pipelined request arrives, and that
    /// window used to swallow such requests entirely (no reply, a client
    /// spinner forever). Blocking: call only on a blocking task, never on
    /// the request loop. `None` on timeout or when a NEWER open superseded
    /// `root` mid-scan; the caller replies [`clew_protocol::ERR_NOT_READY`],
    /// the one refusal clients may retry on.
    fn wait_for_files_blocking(files: &SharedFiles, root: &Path) -> Option<Arc<Vec<FileEntry>>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(p) = files.lock().unwrap().as_ref() {
                return (p.root == root).then(|| p.files.clone());
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The stdout budget, for the transport writer to credit back what it
    /// has written (see [`OutputBudget::release`]).
    pub fn output_budget(&self) -> Arc<OutputBudget> {
        self.proc_out_budget.clone()
    }

    /// Send a correlated reply. Used by arms that finish their work on a
    /// spawned task: the request loop must never wait on slow work (LLM
    /// calls, big greps, blame), or a queued `ProcessKill` / `AgentStop`
    /// would sit behind it.
    fn reply(out: &UnboundedSender<ServerMessage>, id: clew_protocol::RequestId, event: Event) {
        let _ = out.send(ServerMessage::Reply {
            id,
            sub: None,
            event,
        });
    }

    /// Handle one request, returning the event to reply with (or `None` when a
    /// request has no direct reply).
    pub async fn handle(
        &mut self,
        id: clew_protocol::RequestId,
        request: Request,
    ) -> Option<Event> {
        match request {
            // Handshake: confirm the protocol version — and refuse a client
            // speaking another one. Silently proceeding used to fail much
            // later and much more confusingly: the peer's frames simply
            // didn't deserialize and were dropped, so (for example) a remote
            // open waited forever on a Tree that could never arrive.
            Request::Hello {
                protocol,
                fingerprint,
                ..
            } => {
                if protocol != PROTOCOL_VERSION {
                    self.hello_ok = false;
                    return Some(Event::Error {
                        message: format!(
                            "protocol mismatch: client speaks v{protocol}, this clew-server \
                             speaks v{PROTOCOL_VERSION} — update so both sides match"
                        ),
                    });
                }
                // Same numeric version but a different protocol BUILD (a wire
                // change whose version bump was missed, or a stale dev
                // binary): refuse here, where the mismatch is one clear
                // error, instead of later as frames that silently fail to
                // deserialize.
                if fingerprint != clew_protocol::SCHEMA_FINGERPRINT {
                    self.hello_ok = false;
                    return Some(Event::Error {
                        message: format!(
                            "protocol build mismatch: both sides speak v{PROTOCOL_VERSION} but \
                             were built from different protocol sources (client {fingerprint}, \
                             server {}) — rebuild/redeploy so they match",
                            clew_protocol::SCHEMA_FINGERPRINT
                        ),
                    });
                }
                self.hello_ok = true;
                Some(Event::Ready {
                    protocol: PROTOCOL_VERSION,
                    fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
                })
            }
            // Fail closed OUTSIDE a completed handshake — both before any
            // Hello and after a failed one. A client that pipelined requests
            // gets a clear refusal for each, not best-effort answers on a
            // connection whose protocol neither side has confirmed.
            _ if !self.hello_ok => Some(Event::Error {
                message: "refused: the protocol handshake has not completed — send Hello \
                          first, with matching versions on both sides"
                    .into(),
            }),
            // Scan the project: store the file list for search/read, and reply
            // with the tree so the client renders it instead of scanning itself.
            // The scan runs off the loop — a large repo takes seconds, and a
            // queued ProcessKill/AgentStop must not wait behind it. The task
            // is epoch-guarded so a superseded open commits nothing.
            Request::OpenProject { root } => {
                let root = PathBuf::from(root);
                // Drop the previous project's agent language servers now, not
                // lazily on the next Ask — they can hold gigabytes.
                if self
                    .agent_lsp
                    .as_ref()
                    .is_some_and(|pool| pool.root() != root)
                {
                    self.agent_lsp = None;
                }
                // The moment the root switches, everything derived from the
                // old root goes with it — in this same handler turn, before
                // any other request can run:
                //   - the old file list (or a request in the scan window
                //     would combine the NEW root with the OLD files),
                //   - the old watcher (its refresh would re-commit the old
                //     project's files over the new one's),
                //   - every proxied process (the old project's language
                //     servers and debug adapters must not keep running, or
                //     answering, under the new root).
                // Bumped FIRST, before anything is cleared. An older open's
                // `commit_open_project` may be mid-walk on a blocking thread
                // right now; the epoch is the only thing that tells it it has
                // been superseded, so while it still reads as the older value
                // that commit can pass its second check and install a watcher
                // for the OLD root into the slot we are about to clear. Every
                // consequence of that is already caught downstream (the
                // watcher's callback re-commits only while the committed root
                // still matches its own, and the client drops a `Tree` whose
                // root is not the one it asked for), so this ordering was not
                // producing corruption — but a guard that can be observed stale
                // is not a guard, and the fix is to move one line.
                use std::sync::atomic::Ordering;
                let epoch = self.open_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.root = Some(root.clone());
                *self.files.lock().unwrap() = None;
                *self._watcher.lock().unwrap() = None;
                {
                    let mut procs = self.procs.lock().await;
                    for (proc, mut p) in procs.drain() {
                        match p.child.as_mut() {
                            // The stdout reader observes the kill and sends
                            // the ProcessExited.
                            Some(child) => {
                                let _ = child.start_kill();
                            }
                            // Still spawning: no reader exists, report here;
                            // the spawn task reaps the newborn.
                            None => {
                                let _ = self.out.send(ServerMessage::Notification {
                                    sub: None,
                                    event: Event::ProcessExited { proc, code: None },
                                });
                            }
                        }
                    }
                }
                // Agent turns captured the OLD root: left running they keep
                // calling tools against a project the client has left, and
                // keep spending on the model. The client also sends
                // AgentStop, but that frame can be lost with the transport it
                // was queued on — this is the authoritative stop, because it
                // happens where the turns actually run.
                {
                    let mut agents = self.agents.lock().unwrap();
                    for flag in agents.values() {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    agents.clear();
                }
                // Approvals are per-project; the client re-pushes them for
                // the new one after the open completes.
                self.lsp_approvals.lock().unwrap().clear();
                let open_epoch = self.open_epoch.clone();
                let files_slot = self.files.clone();
                let watcher_slot = self._watcher.clone();
                let index_seq = self.index_seq.clone();
                let out = self.out.clone();
                tokio::spawn(async move {
                    let scan_root = root.clone();
                    let Ok(scan) =
                        tokio::task::spawn_blocking(move || clew_core::fs_scan::scan(scan_root))
                            .await
                    else {
                        return;
                    };
                    let rels: Vec<String> = scan.files.iter().map(|f| f.rel.clone()).collect();
                    let files_arc = Arc::new(scan.files);
                    // Commit, watch, then reply — that order, each step with
                    // the previous lock released. `commit_open_project` owns
                    // the ordering and both epoch guards; its doc says why the
                    // watch must not ride inside the commit's critical section
                    // (registration walks the whole root, unfiltered), why it
                    // must still precede the reply, and what replaces the "one
                    // section" argument this used to make. On a blocking
                    // thread because of that same walk.
                    let commit_files = files_slot.clone();
                    let commit_watcher = watcher_slot.clone();
                    let commit_epoch = open_epoch.clone();
                    let commit_root = root.clone();
                    let commit_arc = files_arc.clone();
                    let reply_out = out.clone();
                    let reply_root = root.to_string_lossy().into_owned();
                    let tree = scan.tree;
                    let truncated = scan.truncated;
                    let watch_root = root.clone();
                    let watch_out = out.clone();
                    let watch_files = files_slot.clone();
                    let watch_seq = index_seq.clone();
                    let committed = tokio::task::spawn_blocking(move || {
                        commit_open_project(
                            &commit_files,
                            &commit_watcher,
                            &commit_epoch,
                            epoch,
                            &commit_root,
                            commit_arc,
                            // The reply follows the committed state and the
                            // live watch; a newer open that lands after us
                            // sends its own Tree (with its own root) right
                            // behind this one.
                            || {
                                Self::reply(
                                    &reply_out,
                                    id,
                                    Event::Tree {
                                        root: reply_root,
                                        tree,
                                        files: rels,
                                        truncated,
                                    },
                                )
                            },
                            // Watch the project; changes stream back as
                            // notifications, and the watcher refreshes the
                            // shared file list so search/docs/agent turns see
                            // the current set.
                            || spawn_watcher(watch_root, watch_out, watch_files, watch_seq),
                        )
                    })
                    .await;
                    // Both committed outcomes fall through: when a newer open
                    // landed during the walk the snapshot below is a no-op
                    // anyway, because it re-checks the epoch itself. Only "we
                    // never wrote anything" stops here.
                    if !matches!(committed, Ok(c) if c.committed()) {
                        return; // superseded by a newer OpenProject
                    }
                    // The project-symbol snapshot follows the tree (it reads
                    // every file, so the tree must not wait on it). This is
                    // what a remote client's symbol index and import graph
                    // are built from — it must never read remote-pathed
                    // files off its own disk, so the resolution metadata
                    // (go.mod module, pubspec name) rides along too.
                    let snap_root = root.clone();
                    let publish_files = files_slot.clone();
                    let publish_out = out.clone();
                    let publish_seq = index_seq.clone();
                    let publish_epoch = open_epoch.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        // Reading and publishing happen together under the
                        // publication lock: a watcher partial that lands
                        // mid-build is FRESHER than this snapshot, and only
                        // the lock keeps its `seq` above ours (see
                        // `publish_project_symbols`).
                        publish_project_symbols(
                            &publish_out,
                            &publish_seq,
                            &snap_root,
                            true,
                            || {
                                // The project this scan belongs to may have
                                // been replaced while we waited for the lock;
                                // publishing then would hand the client
                                // another project's index. Checked FIRST so a
                                // superseded full releases the lock at once
                                // instead of pinning it for the whole build —
                                // that wait is the window the race below needs.
                                if publish_epoch.load(Ordering::SeqCst) != epoch {
                                    return None;
                                }
                                // The CURRENT file set, read INSIDE the lock.
                                // Reading it outside took the set as of the
                                // moment this task was spawned: a watcher
                                // partial adding files could publish first and
                                // still be overwritten, because this full then
                                // took a higher `seq` while describing a tree
                                // that never had those files — clearing their
                                // symbols for good.
                                let files_arc = publish_files
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .filter(|p| p.root == snap_root)
                                    .map_or(files_arc.clone(), |p| p.files.clone());
                                let files = build_project_symbols(&snap_root, &files_arc);
                                let structure = clew_core::structure::build(&snap_root, &files_arc);
                                if publish_epoch.load(Ordering::SeqCst) != epoch {
                                    return None;
                                }
                                Some(SymbolPayload {
                                    files,
                                    go_module: clew_protocol::Patch::Set(
                                        clew_core::imports::read_go_module(&snap_root),
                                    ),
                                    dart_package: clew_protocol::Patch::Set(
                                        clew_core::imports::read_dart_package(&snap_root),
                                    ),
                                    structure: clew_protocol::Patch::Set(
                                        (!structure.is_empty())
                                            .then(|| serde_json::to_string(&structure).ok())
                                            .flatten(),
                                    ),
                                })
                            },
                        );
                    })
                    .await;
                });
                None
            }
            // Read + tokenize a file for display. `rel` resolves against the
            // project root; the reply carries per-line (text, style-index) spans
            // that the client maps to theme colors.
            Request::ReadFile { rel, target } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                // Confine the read to the project. `rel` comes from the client
                // (untrusted, especially over SSH), so reject anything that
                // escapes root — absolute paths, `..`, or symlinks pointing out.
                let Some(abs) = confine(&root, &rel) else {
                    return Some(Event::Error {
                        message: format!("refused: path escapes project: {rel}"),
                    });
                };
                // Off the request loop: a large file's highlight pass must not
                // stall queued requests. The task sends the reply itself.
                let out = self.out.clone();
                let target: inactive::Target = target.into();
                tokio::task::spawn_blocking(move || {
                    // ONE open, then everything from that handle: the type
                    // check, the size, and the bytes. Resolving the path three
                    // times (metadata, then read) let a concurrent swap turn
                    // the target into a symlink, a FIFO that parks this task
                    // forever, or a device — after it had passed the checks.
                    // The caps match the client's own viewer limits.
                    let limit = if clew_core::notebook::is_notebook(&abs) {
                        MAX_NOTEBOOK_BYTES
                    } else {
                        MAX_READ_BYTES
                    };
                    let Some(file) = clew_core::statefile::open_plain(&abs) else {
                        return Self::reply(
                            &out,
                            id,
                            Event::Error {
                                message: format!("{rel}: not a readable regular file"),
                            },
                        );
                    };
                    // fstat on the handle we will read, not on the name.
                    match file.metadata() {
                        Ok(meta) if meta.len() > limit => {
                            return Self::reply(
                                &out,
                                id,
                                Event::Error {
                                    message: format!(
                                        "{rel}: too large ({:.1} MB, limit {} MB)",
                                        meta.len() as f64 / (1024.0 * 1024.0),
                                        limit / (1024 * 1024)
                                    ),
                                },
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            return Self::reply(
                                &out,
                                id,
                                Event::Error {
                                    message: format!("read {rel}: {e}"),
                                },
                            );
                        }
                    }
                    // Read through the cap as well: the size above is a cheap
                    // early rejection, but a file can grow while being read.
                    let text = {
                        use std::io::Read;
                        let mut s = String::new();
                        match file.take(limit + 1).read_to_string(&mut s) {
                            Ok(_) if s.len() as u64 <= limit => Ok(s),
                            Ok(_) => Err(format!("{rel}: grew past the {limit}-byte limit")),
                            Err(e) => Err(format!("read {rel}: {e}")),
                        }
                    };
                    // A notebook parses into cells (highlighted server-side)
                    // and replies as `NotebookContent`; raw JSON is never shown.
                    if clew_core::notebook::is_notebook(&abs) {
                        let event = match &text {
                            Ok(json) => match clew_core::notebook::parse(json) {
                                Some(nb) => notebook_event(rel, nb),
                                None => Event::Error {
                                    message: format!("{rel}: not a readable notebook"),
                                },
                            },
                            Err(e) => Event::Error { message: e.clone() },
                        };
                        return Self::reply(&out, id, event);
                    }
                    let event = match text {
                        Ok(source) => {
                            let lang = highlight::detect(&abs);
                            let lines = highlight::highlight_lines(&source, lang);
                            // Symbols, doc comments, and inactive #[cfg] lines —
                            // the rest of what a file view shows, from one read.
                            let (symbols, docs, inactive) = match lang {
                                Some(key) => {
                                    let symbols = outline::extract(&source, key);
                                    let docs = docs::extract(&source, key, &symbols);
                                    let inactive = inactive::inactive_lines(&source, key, &target);
                                    (symbols, docs, inactive)
                                }
                                None => Default::default(),
                            };
                            Event::FileContent {
                                rel,
                                source,
                                lines,
                                symbols,
                                docs: docs.into_iter().collect(),
                                inactive: inactive.into_iter().collect(),
                            }
                        }
                        Err(e) => Event::Error {
                            message: format!("read {rel}: {e}"),
                        },
                    };
                    Self::reply(&out, id, event);
                });
                None
            }
            // Per-file git blame + change status for the gutter. Confined to the
            // project like ReadFile; `None` when the file is untracked. Blame
            // shells out to git and can be slow on a big history — off the loop.
            Request::GitInfo { rel } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let Some(abs) = confine(&root, &rel) else {
                    return Some(Event::Error {
                        message: format!("refused: path escapes project: {rel}"),
                    });
                };
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let info = git::info(&root, &abs);
                    Self::reply(&out, id, Event::GitInfo { rel, info });
                });
                None
            }
            // Grep the scanned project. Reuses the same search engine the client
            // used to run in-process; only where it runs has changed. Big repos
            // grep for seconds — off the loop, the task replies itself.
            Request::Search {
                query,
                regex,
                case_sensitive,
                whole_word,
                include,
                exclude,
            } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let files_slot = self.files.clone();
                let opts = search::SearchOptions {
                    query,
                    regex,
                    case_sensitive,
                    whole_word,
                    include,
                    exclude,
                    root: Some(root.clone()),
                };
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return Self::reply(
                            &out,
                            id,
                            Event::Error {
                                message: clew_protocol::ERR_NOT_READY.into(),
                            },
                        );
                    };
                    let result = search::search(files, opts);
                    let hits = result
                        .hits
                        .into_iter()
                        .map(|h| clew_protocol::SearchHit {
                            rel: h.rel,
                            line: h.line,
                            preview: h.preview,
                        })
                        .collect();
                    Self::reply(
                        &out,
                        id,
                        Event::SearchResults {
                            hits,
                            error: result.error,
                        },
                    );
                });
                None
            }
            // Code statistics, computed where the files live (a remote
            // client must not walk its own disk at the project's path).
            // CPU-bound: off the loop, replies itself.
            Request::Stats => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let report = clew_core::stats::compute(&root);
                    let report = serde_json::to_string(&report).unwrap_or_default();
                    Self::reply(
                        &out,
                        id,
                        Event::Stats {
                            root: root.to_string_lossy().into_owned(),
                            report,
                        },
                    );
                });
                None
            }
            // The name-based project call graph, built where the files live.
            // The client supplies its resolved import scope (rel-based); the
            // reply's node paths are project-relative too. CPU-bound: off
            // the loop, replies itself.
            Request::ProjectCalls { scope } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let files_slot = self.files.clone();
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return Self::reply(
                            &out,
                            id,
                            Event::Error {
                                message: clew_protocol::ERR_NOT_READY.into(),
                            },
                        );
                    };
                    let graph = build_project_calls_graph(&root, &files, &scope);
                    let graph = serde_json::to_string(&graph).unwrap_or_default();
                    Self::reply(
                        &out,
                        id,
                        Event::ProjectCalls {
                            root: root.to_string_lossy().into_owned(),
                            graph,
                        },
                    );
                });
                None
            }
            // A batch of plain sources for the client's Explain pass —
            // confined, per-file capped, batch capped. Unreadable entries
            // are just absent from the reply. Off the loop.
            Request::ReadSources { rels } => {
                const MAX_BATCH: usize = 1000;
                const MAX_SOURCE_BYTES: u64 = 512 * 1024;
                // Aggregate budget for the reply. The per-file and per-batch
                // caps multiply to ~500 MB of raw text — past the protocol's
                // own 256 MB frame limit, so the server would build a frame
                // the client is required to hang up on, after allocating all
                // of it. The client fetches in chunks well under this, so a
                // real batch never comes close.
                const MAX_REPLY_BYTES: usize = 48 * 1024 * 1024;
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if rels.len() > MAX_BATCH {
                    return Some(Event::Error {
                        message: format!("refused: ReadSources batch over {MAX_BATCH} files"),
                    });
                }
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let mut files: Vec<(String, String)> = Vec::new();
                    let mut budget = MAX_REPLY_BYTES;
                    for rel in rels {
                        let Some(abs) = confine(&root, &rel) else {
                            continue;
                        };
                        // One open, then the type check, the size and the
                        // bytes all from that handle — a path resolved twice
                        // can be a regular file the first time and a FIFO the
                        // second. Unreadable entries are simply absent, as
                        // this request has always specified.
                        let Some(text) = clew_core::statefile::read_capped(&abs, MAX_SOURCE_BYTES)
                        else {
                            continue;
                        };
                        let Some(left) = budget.checked_sub(text.len()) else {
                            eprintln!(
                                "[clew-server] ReadSources hit its {MAX_REPLY_BYTES}-byte reply \
                                 budget; {rel} and the rest of the batch are not included"
                            );
                            break;
                        };
                        budget = left;
                        files.push((rel, text));
                    }
                    Self::reply(
                        &out,
                        id,
                        Event::Sources {
                            root: root.to_string_lossy().into_owned(),
                            files,
                        },
                    );
                });
                None
            }
            // One git operation against the project's repository, run where
            // it lives (Time Travel, blame-why, review, the diff gutter).
            // Arguments are validated BEFORE any subprocess: rels confined,
            // shas hex-only, refs shaped like refs (never leading '-', which
            // git would read as an option). Off the loop; replies itself.
            Request::Git { op } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Err(message) = validate_git_op(&op) {
                    return Some(Event::Error { message });
                }
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let result = run_git_op(&root, op);
                    Self::reply(
                        &out,
                        id,
                        Event::GitResult {
                            root: root.to_string_lossy().into_owned(),
                            result,
                        },
                    );
                });
                None
            }
            // Project state (`<root>/.clew/<rel>`), read where the project
            // lives — how a remote client loads its session state. Same
            // rules as every state read: the rel is confined to `.clew/`,
            // and the statefile layer refuses symlinks and oversize files.
            // Runs on the ORDERED state worker, so a read after a write of
            // the same file always sees it.
            Request::ReadState { root: want, rel } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Some(refusal) = wrong_project(&root, &want, &rel) {
                    return Some(refusal);
                }
                if !clew_core::statefile::safe_rel(&rel) {
                    return Some(Event::Error {
                        message: format!("refused: bad state path: {rel}"),
                    });
                }
                let _ = self.state_jobs.send(StateJob {
                    root,
                    rel,
                    id,
                    work: StateWork::Read,
                });
                None
            }
            // Write (or delete, with `text: None`) one project state file —
            // atomic, size-capped, never through a symlinked `.clew`
            // (statefile enforces all three). Success is silent; failures
            // reply as errors so the client can surface them. Ordered: two
            // rapid writes of the same file apply in request order.
            Request::WriteState {
                root: want,
                rel,
                text,
            } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Some(refusal) = wrong_project(&root, &want, &rel) {
                    return Some(refusal);
                }
                if !clew_core::statefile::safe_rel(&rel) {
                    return Some(Event::Error {
                        message: format!("refused: bad state path: {rel}"),
                    });
                }
                if text
                    .as_ref()
                    .is_some_and(|t| t.len() as u64 > clew_core::statefile::MAX_STATE_BYTES)
                {
                    return Some(Event::Error {
                        message: format!("refused: state file too large: {rel}"),
                    });
                }
                // A dropped job would leave the client waiting for an
                // acknowledgement that can never come, and it would keep the
                // change marked unsaved forever. Say so instead.
                if self
                    .state_jobs
                    .send(StateJob {
                        root,
                        rel: rel.clone(),
                        id,
                        work: StateWork::Write(text),
                    })
                    .is_err()
                {
                    return Some(Event::Error {
                        message: format!("the state writer is gone: {rel}"),
                    });
                }
                None
            }
            // Apply ONE entry-level change where the file is. Same guards as
            // the wholesale write above; the difference is that the merge
            // reads the file first, so a client whose copy is a session old
            // adds its bookmark instead of replacing everything another
            // client wrote (see `Request::EditState`).
            Request::EditState {
                root: want,
                rel,
                merge,
            } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Some(refusal) = wrong_project(&root, &want, &rel) {
                    return Some(refusal);
                }
                if !clew_core::statefile::safe_rel(&rel) {
                    return Some(Event::Error {
                        message: format!("refused: bad state path: {rel}"),
                    });
                }
                // The merged file is bounded by the file it merges into, which
                // the read caps; only the incoming entry is unbounded here, so
                // that is what is checked. A merge that would push the file
                // past the cap makes the NEXT read refuse it, which is the
                // same outcome an oversized wholesale write has.
                let edit_bytes = serde_json::to_string(&merge).map(|s| s.len() as u64);
                if !matches!(edit_bytes, Ok(n) if n <= clew_core::statefile::MAX_STATE_BYTES) {
                    return Some(Event::Error {
                        message: format!("refused: state edit too large: {rel}"),
                    });
                }
                if self
                    .state_jobs
                    .send(StateJob {
                        root,
                        rel: rel.clone(),
                        id,
                        work: StateWork::Merge(merge),
                    })
                    .is_err()
                {
                    return Some(Event::Error {
                        message: format!("the state writer is gone: {rel}"),
                    });
                }
                None
            }
            // Spawn a subprocess and stream its stdout back, so a debug adapter
            // runs where the code lives.
            Request::SpawnProcess {
                proc,
                cmd,
                args,
                cwd,
            } => {
                let cwd =
                    cwd.or_else(|| self.root.as_ref().map(|r| r.to_string_lossy().into_owned()));
                let (input_rx, generation) = register_proc(&self.procs, proc).await;
                match spawn_registered(
                    &self.out,
                    &self.procs,
                    self.proc_out_budget.clone(),
                    proc,
                    cmd,
                    args,
                    cwd,
                    input_rx,
                    generation,
                )
                .await
                {
                    Spawned::Failed(event) => Some(event),
                    Spawned::Started | Spawned::Cancelled => None,
                }
            }
            // Resolve and spawn a debug adapter on THIS host (the debuggee
            // lives here). Like SpawnLsp: the stdin queue is registered
            // before the detached resolve, so pipelined DAP traffic buffers
            // for the child; resolution runs off the loop (it probes the
            // environment with subprocesses).
            Request::SpawnAdapter {
                proc,
                lang,
                program,
                args,
            } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                let procs = self.procs.clone();
                let budget = self.proc_out_budget.clone();
                let (input_rx, generation) = register_proc(&self.procs, proc).await;
                tokio::spawn(async move {
                    let resolve_root = root.clone();
                    let resolve_lang = lang.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                        clew_core::debugadapter::resolve_stdio(
                            &resolve_lang,
                            &program,
                            &args,
                            &resolve_root,
                        )
                    })
                    .await
                    .unwrap_or_else(|_| Err(format!("resolving the {lang} adapter failed")));
                    match resolved {
                        Ok(adapter) => {
                            let cwd = Some(root.to_string_lossy().into_owned());
                            match spawn_registered(
                                &out,
                                &procs,
                                budget,
                                proc,
                                adapter.command.to_string_lossy().into_owned(),
                                adapter.args,
                                cwd,
                                input_rx,
                                generation,
                            )
                            .await
                            {
                                Spawned::Started => Self::reply(
                                    &out,
                                    id,
                                    Event::AdapterSpawned {
                                        proc,
                                        launch: adapter.launch,
                                    },
                                ),
                                Spawned::Failed(event) => Self::reply(&out, id, event),
                                // Killed mid-spawn: reporting AdapterSpawned
                                // here had the client drive a DAP handshake
                                // against a process that never ran. The
                                // remover already sent ProcessExited.
                                Spawned::Cancelled => Self::reply(
                                    &out,
                                    id,
                                    Event::Error {
                                        message: "the debug adapter was stopped while starting"
                                            .into(),
                                    },
                                ),
                            }
                        }
                        Err(message) => {
                            // Nothing will run: retract the queue and end the
                            // proxy so the client's DAP driver sees EOF.
                            //
                            // Through `superseded`, not a blind `remove`: this
                            // resolve runs detached and can still be in flight
                            // when the client registers the same id again, and
                            // removing that entry dropped its `Child` —
                            // killing a live process with `kill_on_drop` and
                            // reporting an exit for one that had just started.
                            if !superseded(&procs, proc, generation).await {
                                let _ = out.send(ServerMessage::Notification {
                                    sub: None,
                                    event: Event::ProcessExited { proc, code: None },
                                });
                            }
                            // Unconditional: it answers THIS request's id.
                            Self::reply(&out, id, Event::Error { message });
                        }
                    }
                });
                None
            }
            // Start a language server the server resolves itself — the client
            // never ships a binary path, so a remote uses its own LSP. The
            // resolve + approval gate run on a blocking thread: the gate
            // hashes the executable's bytes, and even the bounded worst case
            // must not stall the serial request loop (a queued ProcessKill
            // has to stay reachable).
            Request::SpawnLsp { proc, language } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                let procs = self.procs.clone();
                let budget = self.proc_out_budget.clone();
                let approvals = self.lsp_approvals.clone();
                // Register the stdin queue before detaching: the client
                // pipelines the LSP `initialize` right behind this request,
                // and those frames must buffer for the child the resolve is
                // still working toward — not race its registration.
                let (input_rx, generation) = register_proc(&self.procs, proc).await;
                tokio::spawn(async move {
                    let gate_root = root.clone();
                    let gate_lang = language.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                        Self::resolve_spawn_exe(&approvals, &gate_root, &gate_lang)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err(Some(format!("resolving the {language} server failed")))
                    });
                    match resolved {
                        Ok((exe, args)) => {
                            let cwd = Some(root.to_string_lossy().into_owned());
                            if let Spawned::Failed(event) = spawn_registered(
                                &out,
                                &procs,
                                budget,
                                proc,
                                exe.to_string_lossy().into_owned(),
                                args,
                                cwd,
                                input_rx,
                                generation,
                            )
                            .await
                            {
                                Self::reply(&out, id, event);
                            }
                        }
                        Err(message) => {
                            // Nothing will ever run: retract the queue and
                            // end the proxy so the client's LSP driver sees EOF.
                            // Guarded by the generation for the same reason as
                            // the adapter path above — this resolve is
                            // detached, and it hashes the executable's bytes,
                            // so the window is not a short one.
                            if !superseded(&procs, proc, generation).await {
                                let _ = out.send(ServerMessage::Notification {
                                    sub: None,
                                    event: Event::ProcessExited { proc, code: None },
                                });
                            }
                            if let Some(message) = message {
                                Self::reply(&out, id, Event::Error { message });
                            }
                        }
                    }
                });
                None
            }
            // What would SpawnLsp run? Resolved here, on the host that would
            // execute it, so the client can show the user the real command
            // line (and fingerprint) before granting an approval — or raise
            // the install-consent prompt, or give up, each explicitly. Off
            // the loop: resolving fingerprints the command's bytes.
            Request::LspResolve { language } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                let approvals = self.lsp_approvals.clone();
                tokio::spawn(async move {
                    let (lang, r, o) = (language.clone(), root.clone(), out.clone());
                    let resolution = tokio::task::spawn_blocking(move || {
                        Self::resolve_lsp(&o, &approvals, &r, &lang)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        clew_protocol::LspResolution::Unsupported {
                            message: format!("resolving the {language} server failed"),
                        }
                    });
                    Self::reply(
                        &out,
                        id,
                        Event::LspResolved {
                            language,
                            root: root.to_string_lossy().into_owned(),
                            resolution,
                        },
                    );
                });
                None
            }
            // Install the store-managed server for `language`. This request IS
            // the consent: the client sends it only after the user approved
            // the install prompt, so the server may download/run the pinned
            // installer here (and only here — never on SpawnLsp/LspResolve).
            Request::LspInstall { language } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                let approvals = self.lsp_approvals.clone();
                tokio::spawn(async move {
                    let (lang, r, o) = (language.clone(), root.clone(), out.clone());
                    let outcome = tokio::task::spawn_blocking(move || {
                        Self::install_lsp(&o, &approvals, &r, &lang)
                    })
                    .await;
                    let resolution = match outcome {
                        Ok(resolution) => resolution,
                        Err(_) => clew_protocol::LspResolution::Unsupported {
                            message: format!("installing the {language} server failed"),
                        },
                    };
                    Self::reply(
                        &out,
                        id,
                        Event::LspResolved {
                            language,
                            root: root.to_string_lossy().into_owned(),
                            resolution,
                        },
                    );
                });
                None
            }
            // The client's per-project approvals; replace the current set.
            Request::LspApprovals { approvals } => {
                *self.lsp_approvals.lock().unwrap() = approvals.into_iter().collect();
                None
            }
            Request::ProcessInput { proc, data } => {
                // Hand the bytes to the process's stdin-writer task. Never
                // write here: a child that stopped reading would fill the
                // pipe and wedge this serial loop (and the ProcessKill that
                // could fix it) forever. `try_send` so a full backlog (same
                // non-reading child) can never block the loop either — but a
                // full queue is fatal for the stream (one lost frame desyncs
                // Content-Length framing forever), so overflow kills the
                // process and reports it instead of silently dropping bytes.
                use tokio::sync::mpsc::error::TrySendError;
                // One message is one LSP/DAP frame, whose own limit is 64 MB.
                // The queue bounds the COUNT (256), so without a per-message
                // cap a peer could park 256 near-frame-sized messages in it.
                if data.len() as u64 > MAX_PROC_INPUT_BYTES {
                    return Some(Event::Error {
                        message: format!(
                            "refused: {} bytes of input for process {proc} (limit {MAX_PROC_INPUT_BYTES})",
                            data.len()
                        ),
                    });
                }
                let mut procs = self.procs.lock().await;
                match procs.get(&proc) {
                    None => None,
                    Some(p) => match p.input.try_send(data) {
                        Ok(()) => None,
                        // The stdin writer ended: the child is dead or dying
                        // and its ProcessExited is already on the way.
                        Err(TrySendError::Closed(_)) => None,
                        Err(TrySendError::Full(_)) => {
                            let mut p = procs.remove(&proc).expect("entry just found");
                            match p.child.as_mut() {
                                // Kill; the stdout reader observes EOF and
                                // sends the ProcessExited.
                                Some(child) => {
                                    let _ = child.start_kill();
                                }
                                // Still spawning: no reader exists yet, so
                                // report the exit here. The spawn task finds
                                // the entry gone and reaps the newborn.
                                None => {
                                    let _ = self.out.send(ServerMessage::Notification {
                                        sub: None,
                                        event: Event::ProcessExited { proc, code: None },
                                    });
                                }
                            }
                            Some(Event::Error {
                                message: format!(
                                    "process {proc} stopped reading stdin (queue overflow); killed"
                                ),
                            })
                        }
                    },
                }
            }
            Request::ProcessKill { proc } => {
                if let Some(mut p) = self.procs.lock().await.remove(&proc) {
                    match p.child.as_mut() {
                        Some(child) => {
                            let _ = child.start_kill();
                        }
                        // Killed while the spawn is still in flight: nothing
                        // to kill yet — the spawn task sees the entry gone
                        // and reaps the child. No reader exists, so the exit
                        // must be reported here.
                        None => {
                            let _ = self.out.send(ServerMessage::Notification {
                                sub: None,
                                event: Event::ProcessExited { proc, code: None },
                            });
                        }
                    }
                }
                None
            }
            // Store the AI config for server-side calls.
            // Replaces the stored credentials wholesale, `None` included:
            // that is how the client says "you may no longer hold these".
            Request::SetAiConfig { chat, embed } => {
                self.wipe_ai_keys();
                self.ai_chat = chat.map(|c| llm::Config {
                    provider: llm::Provider::from_slug(&c.provider),
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                });
                self.ai_embed = embed.map(|c| embed::Config {
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                });
                None
            }
            // Run a chat completion with the server's config (blocking HTTP off
            // the reactor). The whole response comes back in one reply.
            Request::Chat {
                system,
                messages,
                max_tokens,
            } => {
                let Some(cfg) = self.ai_chat.clone() else {
                    return Some(Event::Error {
                        message: "no AI chat config on the server".into(),
                    });
                };
                let msgs: Vec<llm::ChatMsg> = messages
                    .into_iter()
                    .map(|m| {
                        if m.role == "assistant" {
                            llm::ChatMsg::assistant(m.content)
                        } else {
                            llm::ChatMsg::user(m.content)
                        }
                    })
                    .collect();
                // An LLM round-trip takes seconds; it must not stall queued
                // requests (a ProcessKill, an AgentStop). The task replies.
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let event = match llm::complete_chat(&cfg, &system, &msgs, max_tokens) {
                        Ok(text) => Event::ChatResult { text },
                        Err(e) => Event::Error { message: e },
                    };
                    Self::reply(&out, id, event);
                });
                None
            }
            // Like Chat, but streamed: each token goes back as a `ChatDelta`
            // notification, then a `ChatStreamDone`. There is no direct reply.
            Request::ChatStream {
                stream,
                system,
                messages,
                max_tokens,
            } => {
                let done = move |out: &UnboundedSender<ServerMessage>, error: Option<String>| {
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
                        event: Event::ChatStreamDone { stream, error },
                    });
                };
                let Some(cfg) = self.ai_chat.clone() else {
                    done(&self.out, Some("no AI chat config on the server".into()));
                    return None;
                };
                let msgs: Vec<llm::ChatMsg> = messages
                    .into_iter()
                    .map(|m| {
                        if m.role == "assistant" {
                            llm::ChatMsg::assistant(m.content)
                        } else {
                            llm::ChatMsg::user(m.content)
                        }
                    })
                    .collect();
                let out = self.out.clone();
                // Registered so the client can stop it: an abandoned answer
                // (project switch, Ask Clear) otherwise ran to completion on
                // the provider's meter with nobody listening.
                let flag = Arc::new(AtomicBool::new(false));
                self.agents.lock().unwrap().insert(stream, flag.clone());
                let agents = self.agents.clone();
                tokio::task::spawn_blocking(move || {
                    let sink = out.clone();
                    let stop = flag.clone();
                    let result = llm::complete_chat_stream(
                        &cfg,
                        &system,
                        &msgs,
                        max_tokens,
                        |delta| {
                            let _ = sink.send(ServerMessage::Notification {
                                sub: None,
                                event: Event::ChatDelta {
                                    stream,
                                    text: delta.to_string(),
                                },
                            });
                        },
                        &move || stop.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    agents.lock().unwrap().remove(&stream);
                    done(&out, result.err());
                });
                None
            }
            // Embed texts with the server's embedding config.
            Request::Embed { texts } => {
                let Some(cfg) = self.ai_embed.clone() else {
                    return Some(Event::Error {
                        message: "no embedding config on the server".into(),
                    });
                };
                // Same as Chat: HTTP round-trips off the loop, task replies.
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let event = match embed::embed_all(&cfg, &texts) {
                        Ok(vecs) => Event::Embeddings { vecs },
                        Err(e) => Event::Error { message: e },
                    };
                    Self::reply(&out, id, event);
                });
                None
            }
            // Run an agent turn: a blocking tool loop that streams AgentStep /
            // AgentDelta notifications and closes with AgentDone. Failures to
            // even start also arrive as AgentDone so the client's turn resolves.
            Request::AgentAsk {
                stream,
                question,
                history,
                context,
            } => {
                let fail = |out: &UnboundedSender<ServerMessage>, msg: &str| {
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
                        event: Event::AgentDone {
                            stream,
                            error: Some(msg.into()),
                        },
                    });
                };
                let Some(root) = self.root.clone() else {
                    fail(&self.out, "no project open on the server");
                    return None;
                };
                let Some(chat) = self.ai_chat.clone() else {
                    fail(&self.out, "no AI chat config on the server");
                    return None;
                };
                let flag = Arc::new(AtomicBool::new(false));
                self.agents.lock().unwrap().insert(stream, flag.clone());
                let agents = self.agents.clone();
                let out = self.out.clone();
                let embed_cfg = self.ai_embed.clone();
                // Language-server pool for the semantic tools: reuse across
                // turns, rebuild when a different project is opened.
                let lsp = match &self.agent_lsp {
                    Some(pool) if pool.root() == root => pool.clone(),
                    _ => {
                        let pool = Arc::new(agent_lsp::LspPool::new(
                            root.clone(),
                            self.lsp_approvals.clone(),
                        ));
                        self.agent_lsp = Some(pool.clone());
                        pool
                    }
                };
                let rt = tokio::runtime::Handle::current();
                let files_slot = self.files.clone();
                tokio::task::spawn_blocking(move || {
                    // The OpenProject scan may still be committing; wait for
                    // it (bounded) rather than failing a user-visible turn.
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        let _ = out.send(ServerMessage::Notification {
                            sub: None,
                            event: Event::AgentDone {
                                stream,
                                error: Some(clew_protocol::ERR_NOT_READY.into()),
                            },
                        });
                        agents.lock().unwrap().remove(&stream);
                        return;
                    };
                    agent::run(
                        root, files, chat, embed_cfg, lsp, rt, stream, question, history, context,
                        &out, &flag,
                    );
                    agents.lock().unwrap().remove(&stream);
                });
                None
            }
            // Stop an agent turn. The flag is the turn's only stop signal, and
            // it is honored at two granularities. The turn's own bookkeeping
            // (between steps, before each tool runs) tests it directly. Its
            // MODEL calls are cancelled mid-flight only because every one of
            // them goes out as a stream and polls the flag between SSE events
            // — so the request already on the wire when Stop was pressed is
            // dropped rather than generating (and billing) to its end. The one
            // gap left is an endpoint that refuses `stream: true`: those steps
            // fall back to a blocking POST, which has no seam and runs to
            // completion before the turn can close. The embeddings call inside
            // `semantic_find` is NOT part of that gap despite also being a
            // blocking POST: `agent::embed_query` waits on it from a thread that
            // re-reads this flag, so Stop ends the turn there too. The turn
            // always closes with its own `AgentDone`.
            Request::AgentStop { stream } => {
                if let Some(flag) = self.agents.lock().unwrap().get(&stream) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                None
            }
            // Stop a subscription the client has abandoned. `sub` is the same
            // client-minted id the work was started under, so this reaches an
            // agent turn or a streamed chat without the client having to say
            // which. Unknown ids are a no-op: the work has already finished.
            Request::Cancel { sub } => {
                if let Some(flag) = self.agents.lock().unwrap().get(&sub) {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                None
            }
            Request::ListDir { path } => Some(list_dir(path).await),
            Request::BuildDocs => {
                // Build off the request loop. `handle` takes `&mut self`, so
                // awaiting the build here holds that borrow and stalls every
                // other request (file opens, hover) behind it — on a big repo
                // the build takes many seconds and wedged the whole app. Run it
                // on a blocking thread and deliver the result as a `Docs`
                // notification, which the client already handles; return `None`
                // now so the loop is free immediately.
                let docs_root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let files_slot = self.files.clone();
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &docs_root) else {
                        return Self::reply(
                            &out,
                            id,
                            Event::Error {
                                message: clew_protocol::ERR_NOT_READY.into(),
                            },
                        );
                    };
                    let built = build_docs(&docs_root, &files);
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
                        event: Event::Docs {
                            root: docs_root.to_string_lossy().into_owned(),
                            files: built,
                        },
                    });
                });
                None
            }
            // Anything not yet migrated (Outline, Explain, Watch, Cancel, …)
            // is answered, not swallowed: a silent drop leaves the client
            // waiting on a reply that can never come.
            other => Some(Event::Error {
                message: format!(
                    "unsupported request: {} (not implemented by this clew-server)",
                    request_name(&other)
                ),
            }),
        }
    }

    /// Resolve what `SpawnLsp` must execute for `language`, running the
    /// approval gate for repo-specified commands (blocking — it hashes the
    /// executable). `Err(None)` means "no server configured": the proxy ends
    /// silently (EOF) with no error reply; `Err(Some(msg))` is a refusal the
    /// client is told about.
    fn resolve_spawn_exe(
        approvals: &SharedApprovals,
        root: &Path,
        language: &str,
    ) -> Result<(PathBuf, Vec<String>), Option<String>> {
        // A config that fails to load is an ERROR, not "use defaults": the
        // default could resolve (and run) a different server than the one
        // the project configured, silently.
        let config = clew_core::lsp::config::ProjectLspConfig::load(root).map_err(Some)?;
        let Some(server) = config.resolve(language) else {
            return Err(None);
        };
        use clew_core::lsp::store::Located;
        let exe = match server.command.clone() {
            // A `command` comes from the project's own lsp.toml, which ships
            // with the repository. Run it only through the one shared gate
            // every spawn path uses.
            // The approved bytes, copied where the repository cannot reach
            // them. Never the repository's own path.
            Some(cmd) => lsp_command_allowed(approvals, root, &server, &cmd).map_err(Some)?,
            // No `command`: the store-installed binary, whose consent was the
            // install. This path ships no `init_options` — the client runs the
            // handshake over the proxied stdio, so the options it sends are
            // the ones `resolve_lsp` handed it, and that is where they are
            // gated ([`approved_init_options`]).
            None => match clew_core::lsp::store::locate(&server) {
                Located::Ready(exe) => exe,
                // Not installed on this host. Spawning must never install:
                // consent lives in the client, and it arrives as an explicit
                // `LspInstall` — a client that skipped that step gets an
                // error, not a download.
                Located::NeedsDownload { .. } | Located::NeedsInstall { .. } => {
                    return Err(Some(format!(
                        "the {language} server is not installed on this host — \
                         it must be installed (with the user's consent) first"
                    )));
                }
                Located::Unsupported(message) => return Err(Some(message)),
            },
        };
        Ok((exe, server.args))
    }

    /// What stands between the client and a running `language` server on this
    /// host — the read-only resolution behind `LspResolve` (and the state
    /// reported back after an `LspInstall`). Touches nothing: no downloads,
    /// no spawns.
    ///
    /// This is also the gate for the repo's `init_options` on the remote path.
    /// The client runs the LSP handshake itself over the proxied stdio, so
    /// whatever leaves here in `init_options` is exactly what reaches
    /// `initialize`, and the client cannot re-derive the verdict itself: the
    /// fingerprint covers THIS host's server/version/args, which the client
    /// never sees. Unapproved options therefore never leave in `init_options`,
    /// and the user is told so through `out`.
    ///
    /// They do leave in `Ready::withheld`, which is the grant path rather than
    /// a hole in the gate: it carries the fingerprint and the options only so
    /// the client can SHOW them and record an approval against them. Nothing
    /// runs on that copy — an allow re-enters here, and the options that reach
    /// `initialize` are the ones re-read and re-fingerprinted on this host.
    fn resolve_lsp(
        out: &UnboundedSender<ServerMessage>,
        approvals: &SharedApprovals,
        root: &Path,
        language: &str,
    ) -> clew_protocol::LspResolution {
        use clew_core::lsp::store::Located;
        use clew_protocol::LspResolution;
        // Surface a broken config instead of silently resolving defaults.
        let config = match clew_core::lsp::config::ProjectLspConfig::load(root) {
            Ok(config) => config,
            Err(message) => return LspResolution::Unsupported { message },
        };
        let Some(server) = config.resolve(language) else {
            return LspResolution::Unsupported {
                message: format!("no language server is configured for {language}"),
            };
        };
        // Sent to the client, which runs the LSP handshake itself over the
        // proxied stdio — so these are the options that end up in `initialize`,
        // and the fingerprint below has to be taken over the same value.
        let init_options = server
            .init_options
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        // A `command` config carries its options inside the command's
        // fingerprint, and the client uses them only after approving it — so
        // this branch is unchanged, and the options-only gate below would only
        // duplicate the approval the modal is already asking for.
        if let Some(cmd) = server.command.clone() {
            return match clew_core::trust::lsp_fingerprint(
                root,
                &cmd,
                &server.args,
                &server.server_name,
                &server.version,
                server.init_options.as_ref(),
            ) {
                Ok(fingerprint) => LspResolution::Command(clew_protocol::LspCommandSpec {
                    command: cmd.to_string_lossy().into_owned(),
                    args: server.args.clone(),
                    server: server.server_name.clone(),
                    version: server.version.clone(),
                    fingerprint,
                    init_options,
                }),
                // Unfingerprintable (missing, not a regular file, oversized):
                // it can be neither approved nor run.
                Err(e) => LspResolution::Unsupported {
                    message: format!("lsp.toml command: {e}"),
                },
            };
        }
        match clew_core::lsp::store::locate(&server) {
            // The store binary was consented to at install; its `init_options`
            // were not, and there is no command to fold them into — so they go
            // only if approved on their own fingerprint. Withheld ones are
            // reported rather than dropped in silence: the difference between
            // "my lsp.toml is ignored" and "my lsp.toml is broken" is the
            // whole of the user's next hour. (An uncorrelated `Error` lands in
            // the client's status bar.)
            Located::Ready(_) => {
                let (allowed, refused) = approved_init_options(approvals, root, &server);
                let was_refused = refused.is_some();
                if let Some(message) = refused {
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
                        event: Event::Error { message },
                    });
                }
                // A refusal is only half an answer without the means to grant
                // it: the client cannot compute this fingerprint (it covers
                // THIS host's server/version/args) and cannot read this host's
                // lsp.toml, so a withheld config that named no `command` had no
                // modal, no button and no way through — on every open, across
                // restarts and reconnects. `command: None` is the shape the
                // local path already raises for exactly this config.
                //
                // Nothing offered when the fingerprint itself failed: an
                // unfingerprintable config also refuses, and there is nothing
                // to approve there — the user would be asked to allow a value
                // that can never match.
                let withheld = server
                    .init_options
                    .as_ref()
                    .filter(|_| was_refused)
                    .zip(init_options)
                    .and_then(|(options, options_json)| {
                        let fingerprint = options_only_fingerprint(&server, options).ok()?;
                        Some(clew_protocol::LspOptionsSpec {
                            server: server.server_name.clone(),
                            version: server.version.clone(),
                            args: server.args.clone(),
                            fingerprint,
                            options: options_json,
                        })
                    });
                LspResolution::Ready {
                    init_options: allowed.as_ref().and_then(|v| serde_json::to_string(v).ok()),
                    withheld,
                }
            }
            Located::NeedsDownload { download, .. } => LspResolution::NeedsInstall {
                server: server.server_name.clone(),
                version: server.version.clone(),
                describe: format!("download {}", download.url),
            },
            Located::NeedsInstall { install, .. } => LspResolution::NeedsInstall {
                server: server.server_name.clone(),
                version: server.version.clone(),
                describe: format!("{} (requires {} on PATH)", install.describe, install.tool),
            },
            Located::Unsupported(message) => LspResolution::Unsupported { message },
        }
    }

    /// Install the store-managed server for `language` (blocking). Only ever
    /// called from the `LspInstall` request — the one path that carries the
    /// user's consent. Returns the post-install resolution.
    fn install_lsp(
        out: &UnboundedSender<ServerMessage>,
        approvals: &SharedApprovals,
        root: &Path,
        language: &str,
    ) -> clew_protocol::LspResolution {
        use clew_core::lsp::store::Located;
        use clew_protocol::LspResolution;
        // Surface a broken config instead of installing the default server
        // the project may have overridden or disabled.
        let config = match clew_core::lsp::config::ProjectLspConfig::load(root) {
            Ok(config) => config,
            Err(message) => return LspResolution::Unsupported { message },
        };
        let Some(server) = config.resolve(language) else {
            return LspResolution::Unsupported {
                message: format!("no language server is configured for {language}"),
            };
        };
        if server.command.is_some() {
            // A repo-specified command is approved, not installed; a client
            // sending LspInstall for it is confused — refuse.
            return LspResolution::Unsupported {
                message: format!("the {language} server is repo-specified, nothing to install"),
            };
        }
        let installed = match clew_core::lsp::store::locate(&server) {
            Located::Ready(_) => Ok(()),
            Located::NeedsDownload { download, dest_dir } => {
                clew_core::lsp::store::download_and_install(&download, &dest_dir).map(|_| ())
            }
            Located::NeedsInstall { install, dest_dir } => {
                clew_core::lsp::store::toolchain_install(&install, &server.version, &dest_dir)
                    .map(|_| ())
            }
            Located::Unsupported(message) => Err(message),
        };
        match installed {
            Ok(()) => Self::resolve_lsp(out, approvals, root, language),
            Err(e) => LspResolution::Unsupported {
                message: format!("install {language} server: {e}"),
            },
        }
    }
}

/// Extract the project-symbol snapshot: per supported file (bounded exactly
/// like the client's own indexer — file count, per-file size, regular files
/// confined to the root), its outline symbols with the test classification
/// and its raw import specifiers. Blocking; run off the request loop.
fn build_project_symbols(root: &Path, files: &[FileEntry]) -> Vec<clew_protocol::FileSymbols> {
    const MAX_FILES: usize = 20_000;
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    let mut snapshot = Vec::new();
    for f in files.iter().take(MAX_FILES) {
        let Some(entry) = file_symbols_for(root, &f.abs, &f.rel, MAX_FILE_BYTES) else {
            continue;
        };
        if !entry.symbols.is_empty() || !entry.imports.is_empty() {
            snapshot.push(entry);
        }
    }
    snapshot
}

/// One file's `FileSymbols` entry, or `None` when the file isn't indexable
/// (unsupported language, too large, not a plain in-root file). A readable
/// file with no symbols yields an entry with an empty list — for the partial
/// (watcher) updates that means "clear what you had for this rel".
fn file_symbols_for(
    root: &Path,
    abs: &Path,
    rel: &str,
    max_bytes: u64,
) -> Option<clew_protocol::FileSymbols> {
    let lang = highlight::detect(abs)?;
    clew_core::highlight::tags_for(lang)?;
    // One open, checked and capped on the handle. Checking the path and then
    // reading it again by name resolved the name twice and enforced the size
    // on a stat the read never saw.
    let content = clew_core::fs_scan::read_confined_capped(root, abs, max_bytes)?;
    let lines: Vec<&str> = content.lines().collect();
    let symbols = outline::extract(&content, lang)
        .into_iter()
        .map(|s| clew_protocol::IndexSymbol {
            is_test: matches!(s.kind.as_str(), "function" | "method")
                && outline::is_test_fn(&lines, s.line, &s.name, lang),
            name: s.name,
            kind: s.kind,
            line: s.line,
        })
        .collect();
    let imports = clew_core::imports::imports_of(&content, lang)
        .into_iter()
        .map(|i| clew_protocol::WireImport {
            module: i.module,
            line: i.line,
            is_mod: i.is_mod_decl,
        })
        .collect();
    Some(clew_protocol::FileSymbols {
        rel: rel.to_string(),
        symbols,
        imports,
    })
}

/// Build the name-based project call graph for a `ProjectCalls` request:
/// callable definitions and sources come from this host's files (under the
/// indexer's caps), the import scope from the client (rel-based, converted
/// to this host's absolute paths for the build, and back to rels for the
/// wire). Blocking; run off the request loop.
fn build_project_calls_graph(
    root: &Path,
    files: &[FileEntry],
    scope: &[(String, Vec<String>)],
) -> clew_core::projectcalls::ProjectCallGraph {
    const MAX_FILES: usize = 20_000;
    const MAX_FILE_BYTES: u64 = 512 * 1024;
    let mut defs: Vec<clew_core::projectcalls::Def> = Vec::new();
    let mut sources: Vec<(PathBuf, String)> = Vec::new();
    for f in files.iter().take(MAX_FILES) {
        let Some(lang) = highlight::detect(&f.abs) else {
            continue;
        };
        if clew_core::highlight::tags_for(lang).is_none() {
            continue;
        }
        // Confined, capped, and read through the handle that was checked.
        let Some(content) = clew_core::fs_scan::read_confined_capped(root, &f.abs, MAX_FILE_BYTES)
        else {
            continue;
        };
        for s in outline::extract(&content, lang) {
            if matches!(s.kind.as_str(), "function" | "method") {
                defs.push(clew_core::projectcalls::Def {
                    name: s.name,
                    kind: s.kind,
                    file: f.abs.clone(),
                    line: s.line,
                });
            }
        }
        sources.push((f.abs.clone(), content));
    }
    let scope: std::collections::HashMap<PathBuf, std::collections::HashSet<PathBuf>> = scope
        .iter()
        .map(|(rel, imports)| {
            (
                root.join(rel),
                imports.iter().map(|r| root.join(r)).collect(),
            )
        })
        .collect();
    clew_core::projectcalls::ProjectCallGraph::build(defs, &sources, &scope)
        .rebase(|p| p.strip_prefix(root).unwrap_or(p).to_path_buf())
}

/// Validate a `GitOp`'s arguments before anything reaches a git subprocess.
/// Everything here arrives from the client (untrusted over SSH): rels must
/// stay confined, shas must be plain hex, and refs must never look like
/// options (`-...`).
fn validate_git_op(op: &clew_protocol::GitOp) -> Result<(), String> {
    use clew_protocol::GitOp;
    const MAX_LIMIT: usize = 1000;
    const MAX_DIFF_BYTES: usize = 1024 * 1024;
    let rel_ok = |rel: &str| {
        clew_core::statefile::safe_rel(rel)
            .then_some(())
            .ok_or_else(|| format!("refused: bad path: {rel}"))
    };
    // One definition of "a sha" for both paths: the local GUI reaches these
    // same git helpers directly, and a second copy of the predicate here is
    // exactly how the remote gate and the local one drifted apart before.
    let sha_ok = |sha: &str| {
        clew_core::git::is_hex_sha(sha)
            .then_some(())
            .ok_or_else(|| format!("refused: bad commit id: {sha}"))
    };
    let ref_ok = |base: &str| {
        (!base.is_empty()
            && !base.starts_with('-')
            && base.len() <= 256
            && base
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._/@~^-".contains(c)))
        .then_some(())
        .ok_or_else(|| format!("refused: bad ref: {base}"))
    };
    let limit_ok = |n: usize| {
        (n <= MAX_LIMIT)
            .then_some(())
            .ok_or_else(|| "refused: history limit too large".to_string())
    };
    let bytes_ok = |n: usize| {
        (n <= MAX_DIFF_BYTES)
            .then_some(())
            .ok_or_else(|| "refused: diff cap too large".to_string())
    };
    match op {
        GitOp::FileHistory { rel, limit } => rel_ok(rel).and(limit_ok(*limit)),
        GitOp::SymbolHistory { rel, limit, .. } => rel_ok(rel).and(limit_ok(*limit)),
        GitOp::FileAt { sha, rel } | GitOp::AddedLines { sha, rel } => sha_ok(sha).and(rel_ok(rel)),
        GitOp::CommitMessage { sha } => sha_ok(sha),
        GitOp::CommitFileDiff {
            sha,
            rel,
            max_bytes,
        } => sha_ok(sha).and(rel_ok(rel)).and(bytes_ok(*max_bytes)),
        GitOp::DiffLines { rel } => rel_ok(rel),
        GitOp::ReviewBase => Ok(()),
        GitOp::CommitSubjects { base } | GitOp::ChangedFiles { base } => ref_ok(base),
        GitOp::RangePatch { base, max_bytes } => ref_ok(base).and(bytes_ok(*max_bytes)),
    }
}

/// Run a (validated) `GitOp` against `root` and serialize its result — the
/// shapes documented on the protocol enum. Blocking (git subprocesses).
fn run_git_op(root: &Path, op: clew_protocol::GitOp) -> String {
    use clew_protocol::GitOp;
    fn ser<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_string(v).unwrap_or_default()
    }
    match op {
        GitOp::FileHistory { rel, limit } => ser(&git::file_history(root, &rel, limit)),
        GitOp::SymbolHistory {
            rel,
            start,
            end,
            limit,
        } => ser(&git::symbol_history(root, &rel, start, end, limit)),
        GitOp::FileAt { sha, rel } => ser(&git::file_at(root, &sha, &rel)),
        GitOp::AddedLines { sha, rel } => ser(&git::commit_added_lines(root, &sha, &rel)),
        GitOp::CommitMessage { sha } => ser(&git::commit_message(root, &sha)),
        GitOp::CommitFileDiff {
            sha,
            rel,
            max_bytes,
        } => ser(&git::commit_file_diff(root, &sha, &rel, max_bytes)),
        GitOp::DiffLines { rel } => ser(&git::diff_lines(root, &root.join(&rel))),
        GitOp::ReviewBase => ser(&git::review_base(root)),
        GitOp::CommitSubjects { base } => ser(&git::commit_subjects(root, &base)),
        GitOp::ChangedFiles { base } => ser(&git::changed_files(root, &base)),
        GitOp::RangePatch { base, max_bytes } => ser(&git::range_patch(root, &base, max_bytes)),
    }
}

/// Apply one [`clew_protocol::StateMerge`] to `path`, returning the merged
/// file's text (`None` = the store emptied and the file was deleted).
///
/// The read and the write are one operation here, which is the whole point of
/// moving the merge server-side: the state worker is ordered, so nothing else
/// on THIS connection interleaves, and the file lock covers the case ordering
/// cannot — a second clew-server process on the same host, which is what two
/// windows of one clew produce (each window opens its own SSH session).
///
/// The lock is best effort (`None` on a read-only `.clew/`, or on a
/// filesystem without `flock`); when it cannot be taken the merge still runs,
/// with the same microsecond-wide window the local stores accept.
fn run_merge(
    path: &Path,
    rel: &str,
    merge: &clew_protocol::StateMerge,
) -> Result<Option<String>, String> {
    let _exclusive = clew_core::statefile::lock_exclusive(path);
    let current = clew_core::statefile::read(path);
    match clew_core::statefile::merge_entries(current.as_deref(), merge) {
        Some(text) => clew_core::statefile::write_atomic(path, text.as_bytes())
            .map(|()| Some(text))
            .map_err(|e| format!("write .clew/{rel}: {e}")),
        None => clew_core::statefile::remove(path)
            .map(|()| None)
            .map_err(|e| format!("delete .clew/{rel}: {e}")),
    }
}

/// Spawn the ordered `.clew/` state worker: one task drains the queue and
/// runs each job's (blocking) filesystem work to completion before the next,
/// so state operations apply exactly in request order without ever stalling
/// the request loop.
fn spawn_state_worker(out: UnboundedSender<ServerMessage>) -> UnboundedSender<StateJob> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StateJob>();
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let out = out.clone();
            // Awaited: the next job starts only after this one finished —
            // that ordering is the worker's whole point.
            let _ = tokio::task::spawn_blocking(move || {
                let path = job.root.join(".clew").join(&job.rel);
                match job.work {
                    StateWork::Read => {
                        let text = clew_core::statefile::read(&path);
                        Server::reply(
                            &out,
                            job.id,
                            Event::StateContent {
                                root: job.root.to_string_lossy().into_owned(),
                                rel: job.rel,
                                text,
                            },
                        );
                    }
                    // Both write paths answer either way. The client cannot
                    // treat a queued frame as a durable write — a dead but
                    // undetected transport swallows frames silently — so
                    // success has to be as observable as failure.
                    StateWork::Write(Some(text)) => {
                        let event = match clew_core::statefile::write_atomic(&path, text.as_bytes())
                        {
                            Ok(()) => Event::StateWritten {
                                root: job.root.to_string_lossy().into_owned(),
                                rel: job.rel,
                            },
                            Err(e) => Event::Error {
                                message: format!("write .clew/{}: {e}", job.rel),
                            },
                        };
                        Server::reply(&out, job.id, event);
                    }
                    StateWork::Write(None) => {
                        let event = match clew_core::statefile::remove(&path) {
                            Ok(()) => Event::StateWritten {
                                root: job.root.to_string_lossy().into_owned(),
                                rel: job.rel,
                            },
                            Err(e) => Event::Error {
                                message: format!("delete .clew/{}: {e}", job.rel),
                            },
                        };
                        Server::reply(&out, job.id, event);
                    }
                    StateWork::Merge(merge) => {
                        let event = run_merge(&path, &job.rel, &merge)
                            .map(|text| Event::StateEdited {
                                root: job.root.to_string_lossy().into_owned(),
                                rel: job.rel.clone(),
                                text,
                            })
                            .unwrap_or_else(|message| Event::Error { message });
                        Server::reply(&out, job.id, event);
                    }
                }
            })
            .await;
        }
    });
    tx
}

/// The variant name of a request, for "unsupported request" error messages.
fn request_name(request: &Request) -> &'static str {
    match request {
        Request::Hello { .. } => "Hello",
        Request::OpenProject { .. } => "OpenProject",
        Request::ReadFile { .. } => "ReadFile",
        Request::GitInfo { .. } => "GitInfo",
        Request::Search { .. } => "Search",
        Request::Stats => "Stats",
        Request::ProjectCalls { .. } => "ProjectCalls",
        Request::ReadSources { .. } => "ReadSources",
        Request::Git { .. } => "Git",
        Request::ReadState { .. } => "ReadState",
        Request::WriteState { .. } => "WriteState",
        Request::EditState { .. } => "EditState",
        Request::Find { .. } => "Find",
        Request::Outline { .. } => "Outline",
        Request::Watch => "Watch",
        Request::Explain { .. } => "Explain",
        Request::Cancel { .. } => "Cancel",
        Request::SpawnProcess { .. } => "SpawnProcess",
        Request::SpawnLsp { .. } => "SpawnLsp",
        Request::SpawnAdapter { .. } => "SpawnAdapter",
        Request::LspResolve { .. } => "LspResolve",
        Request::LspInstall { .. } => "LspInstall",
        Request::LspApprovals { .. } => "LspApprovals",
        Request::ProcessInput { .. } => "ProcessInput",
        Request::ProcessKill { .. } => "ProcessKill",
        Request::SetAiConfig { .. } => "SetAiConfig",
        Request::Chat { .. } => "Chat",
        Request::ChatStream { .. } => "ChatStream",
        Request::Embed { .. } => "Embed",
        Request::AgentAsk { .. } => "AgentAsk",
        Request::AgentStop { .. } => "AgentStop",
        Request::ListDir { .. } => "ListDir",
        Request::BuildDocs => "BuildDocs",
    }
}

/// Refuse a state operation whose project is not the one this server holds.
///
/// These writes replace a file wholesale (and delete it when the text is
/// `None`), and the client can only ever have one project open — so a request
/// naming a different root is a save that raced a project switch. Applying it
/// would put one project's bookmarks, trail or tours into another's `.clew/`.
fn wrong_project(root: &Path, want: &str, rel: &str) -> Option<Event> {
    (root.to_string_lossy() != want).then(|| Event::Error {
        message: format!(
            "refused: state {rel} is for project {want}, this server has {}",
            root.display()
        ),
    })
}

/// Register the stdin queue for `proc` in the table, ahead of the actual
/// spawn. From this moment `ProcessInput` frames buffer in the queue; once
/// the OS process exists, [`spawn_registered`] wires the queue to its stdin
/// and every buffered byte drains in order. This is what makes a client's
/// pipelined `spawn; write` correct even though the spawn itself runs on a
/// detached task.
async fn register_proc(
    procs: &SharedProcs,
    proc: u64,
) -> (tokio::sync::mpsc::Receiver<Vec<u8>>, u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    let (input, input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PROC_INPUT_QUEUE);
    // A re-registered handle replaces the old entry, whose process is then
    // reaped by dropping it (`kill_on_drop`). The generation is what stops
    // that process's reader from deregistering this new entry.
    procs.lock().await.insert(
        proc,
        Proc {
            input,
            child: None,
            generation,
        },
    );
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
async fn superseded(procs: &SharedProcs, proc: u64, generation: u64) -> bool {
    let mut table = procs.lock().await;
    match table.get(&proc) {
        Some(p) if p.generation == generation => {
            table.remove(&proc);
            false
        }
        Some(_) => true,
        None => false,
    }
}

/// How long the stdout pump keeps reading after the child has exited, so its
/// last frames still reach the client. Bounded, because a descendant that
/// inherited the pipe can hold it open for as long as it likes.
const FINAL_DRAIN: Duration = Duration::from_millis(250);

/// Wait for the process behind `proc` to actually exit, returning its exit
/// code (`None` when it was signalled, the handle is gone, or a newer
/// registration took the id over).
///
/// Polled rather than awaited on the `Child` directly: the handle has to stay
/// in the table so a concurrent `ProcessKill` can still reach it, and holding
/// the table lock across an await would stall every other process operation.
/// The interval backs off, so a child that closed stdout and then ran for an
/// hour costs a handful of wakeups rather than one per tick.
async fn wait_for_exit(procs: &SharedProcs, proc: u64, generation: u64) -> Option<i32> {
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
            match p.child.as_mut().map(tokio::process::Child::try_wait) {
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
enum Spawned {
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
async fn spawn_registered(
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
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let spawned =
        command.spawn().and_then(
            |mut child| match (child.stdin.take(), child.stdout.take()) {
                (Some(stdin), Some(stdout)) => Ok((child, stdin, stdout)),
                _ => Err(std::io::Error::other("stdio pipes missing")),
            },
        );
    match spawned {
        Ok((child, mut stdin, mut stdout)) => {
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
                    let mut child = child;
                    let _ = child.start_kill();
                    return Spawned::Cancelled;
                }
            }
            let _ = out.send(ServerMessage::Notification {
                sub: None,
                event: Event::ProcessStarted { proc },
            });
            // Stdout reader, started only after the child is attached above:
            // its exit path removes the table entry, and a child that exits
            // instantly could otherwise run that removal first — leaving a
            // dead entry in the table forever.
            let out = out.clone();
            let procs_cleanup = procs.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
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
                        sub: None,
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
                let superseded = superseded(&procs_cleanup, proc, generation).await;
                if !superseded {
                    let _ = out.send(ServerMessage::Notification {
                        sub: None,
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
                    sub: None,
                    event: Event::ProcessExited { proc, code: None },
                });
            }
            Spawned::Failed(Event::Error {
                message: format!("spawn {cmd}: {e}"),
            })
        }
    }
}

/// Build the project's API documentation index: for every file with a
/// recognized language and a non-empty documented API, its nested doc items.
/// Blocking; run off the async runtime.
fn build_docs(root: &Path, files: &[FileEntry]) -> Vec<clew_protocol::DocFile> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    // Per-file API extraction is independent, so fan it out across cores — a
    // single-threaded pass takes minutes on a large repo (flutter_rust_bridge is
    // ~5k files). Work-steal from a shared atomic cursor rather than pre-slicing
    // into contiguous chunks: the heavy files (generated, symbol-dense) cluster
    // in one directory, so a contiguous split dumps them all on one thread while
    // the rest idle. Pulling one file at a time keeps every core busy.
    let threads = std::thread::available_parallelism().map_or(4, |p| p.get());
    let next = AtomicUsize::new(0);
    let root = &root;
    let mut out: Vec<clew_protocol::DocFile> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let next = &next;
                scope.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        if let Some(doc) = build_doc_one(root, f) {
                            local.push(doc);
                        }
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    // Threads finish in nondeterministic order; sort so the DOCS list is stable.
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// The public-API doc items for one file, or `None` if it has no recognized
/// language, is too large, unreadable, or has no documented API. See
/// [`build_docs`].
fn build_doc_one(root: &Path, f: &FileEntry) -> Option<clew_protocol::DocFile> {
    // Skip very large files. A generated / bundled / macro-heavy source (napi's
    // `async_runtime.rs` is ~1 MB) makes the tree-sitter parse + API-surface
    // extraction crawl. 512 KB matches the semantic index's per-file cap.
    const MAX_DOC_FILE_BYTES: u64 = 512 * 1024;
    let lang = highlight::detect(&f.abs)?;
    // Re-verify the path is still a regular file inside the project — the scan
    // can be stale — and enforce the cap on the READ. Sizing it from a
    // separate `metadata` call left a file free to grow past the limit in
    // between, and left the read itself able to block on a FIFO swapped in.
    let source = clew_core::fs_scan::read_confined_capped(root, &f.abs, MAX_DOC_FILE_BYTES)?;
    // Skip generated code. It isn't the hand-written public API the DOCS view is
    // for, and codegen output (Dart freezed/`.g.dart`, protobuf, flutter_rust_
    // bridge's `frb_generated.*` — thousands of lines of boilerplate each) is the
    // main thing that made the extraction crawl on a big repo.
    if is_generated_source(&source) {
        return None;
    }
    let items = clew_core::apidoc::build_file(&source, lang);
    (!items.is_empty()).then(|| clew_protocol::DocFile {
        rel: f.rel.clone(),
        items,
    })
}

/// Whether a source file is machine-generated, by the "do not edit" banner that
/// generators (freezed, protobuf, flutter_rust_bridge, prost, …) put at the top.
/// Checked against the first lines only, lower-cased, so it's cheap and robust
/// to a leading license block.
fn is_generated_source(source: &str) -> bool {
    // Normalize `’`/`'` apostrophes so "don't" matches, and lower-case.
    let head = source
        .lines()
        .take(40)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase()
        .replace('\u{2019}', "'");
    const MARKERS: &[&str] = &[
        "do not edit",
        "don't edit",
        "do not modify",
        "don't modify",
        "@generated",
        "generated by",
        "generated file",
        "generated code",
        "code generated",
        "automatically generated",
        "auto-generated",
        "autogenerated",
    ];
    MARKERS.iter().any(|m| head.contains(m))
}

/// List a directory on this host for the remote folder picker. `path` is an
/// absolute or `~`-relative directory, or `None` for the login home. Directories
/// sort before files, each alphabetically (case-insensitive). Unreadable entries
/// are skipped rather than failing the whole listing.
async fn list_dir(path: Option<String>) -> Event {
    let home = std::env::var("HOME").ok();
    // Resolve the target directory: home when unset, `~`-expanded, else as given.
    let dir: PathBuf = match path.as_deref() {
        None | Some("") | Some("~") => match &home {
            Some(h) => PathBuf::from(h),
            None => PathBuf::from("/"),
        },
        Some(p) if p == "~" || p.starts_with("~/") => match &home {
            Some(h) => Path::new(h).join(p.trim_start_matches("~/")),
            None => PathBuf::from(p),
        },
        Some(p) => PathBuf::from(p),
    };
    // Canonicalize so the reported path and its parent are stable and absolute.
    let dir = tokio::fs::canonicalize(&dir).await.unwrap_or(dir);

    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(e) => {
            return Event::Error {
                message: format!("cannot list {}: {e}", dir.display()),
            };
        }
    };
    let mut entries: Vec<clew_protocol::DirEntry> = Vec::new();
    while let Ok(Some(ent)) = read.next_entry().await {
        let name = ent.file_name().to_string_lossy().into_owned();
        // A symlink to a directory should still browse as one.
        let is_dir = match ent.file_type().await {
            Ok(ft) if ft.is_symlink() => tokio::fs::metadata(ent.path())
                .await
                .map(|m| m.is_dir())
                .unwrap_or(false),
            Ok(ft) => ft.is_dir(),
            Err(_) => continue,
        };
        entries.push(clew_protocol::DirEntry { name, is_dir });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Event::DirListing {
        path: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        entries,
    }
}

/// Build the `NotebookContent` reply for a parsed notebook: highlight each
/// code cell with the notebook's language and map cells/outputs/outline onto
/// the protocol types.
fn notebook_event(rel: String, nb: clew_core::notebook::Notebook) -> Event {
    use clew_core::notebook as nbk;
    let key = highlight::static_key(&nb.language);
    let cells = nb
        .cells
        .iter()
        .map(|c| clew_protocol::NotebookCell {
            kind: match c.kind {
                nbk::CellKind::Markdown => "markdown",
                nbk::CellKind::Code => "code",
                nbk::CellKind::Raw => "raw",
            }
            .to_string(),
            lines: if c.kind == nbk::CellKind::Code {
                highlight::highlight_lines(&c.source, key)
            } else {
                Vec::new()
            },
            source: c.source.clone(),
            proj_line: c.proj_line,
            outputs: c
                .outputs
                .iter()
                .map(|o| match o {
                    nbk::Output::Text { spans, stderr } => clew_protocol::NotebookOutput::Text {
                        spans: spans.clone(),
                        stderr: *stderr,
                    },
                    nbk::Output::Image { data } => {
                        clew_protocol::NotebookOutput::Image { data: data.clone() }
                    }
                    nbk::Output::Svg(s) => clew_protocol::NotebookOutput::Svg(s.clone()),
                    nbk::Output::Placeholder(l) => {
                        clew_protocol::NotebookOutput::Placeholder(l.clone())
                    }
                })
                .collect(),
            execution_count: c.execution_count,
        })
        .collect();
    let symbols = nb
        .outline()
        .into_iter()
        .map(|(name, kind, line, end_line)| clew_protocol::Symbol {
            name,
            kind,
            line,
            end_line,
        })
        .collect();
    Event::NotebookContent {
        rel,
        language: nb.language,
        cells,
        symbols,
        projection: nb.projection,
    }
}

/// Resolve `rel` against `root`, refusing anything that escapes the project:
/// absolute paths, `..` traversal, or symlinks that point outside `root`.
/// Returns the canonical absolute path only when it genuinely lives inside root.
fn confine(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    // Reject absolute paths and any parent/root/prefix components up front.
    let escapes = rel_path.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if rel_path.is_absolute() || escapes {
        return None;
    }
    // Canonicalize and confirm containment. Canonicalizing resolves symlinks, so
    // a link inside the project that points outside it is rejected too.
    let canonical_root = std::fs::canonicalize(root).ok()?;
    let canonical = std::fs::canonicalize(canonical_root.join(rel_path)).ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

/// One `ProjectSymbols` payload, as read from disk.
struct SymbolPayload {
    files: Vec<clew_protocol::FileSymbols>,
    go_module: clew_protocol::Patch<String>,
    dart_package: clew_protocol::Patch<String>,
    structure: clew_protocol::Patch<String>,
}

/// Build and send one `ProjectSymbols` publication, with `build` running
/// INSIDE the publication lock. `build` returns `None` to publish nothing.
///
/// The read has to be inside the lock, not just the stamp-and-send. Stamping
/// at send time makes `seq` the SEND order, and send order is not read order:
/// a full snapshot reads every file and can take seconds, so a watcher
/// partial published during that build carries fresher content yet a lower
/// seq — and the full, sent afterwards and stamped higher, overwrites it with
/// what the file looked like before the change. Holding the lock across the
/// read makes seq order equal read order, which is the ordering the client's
/// `seq > last applied` test actually needs.
fn publish_project_symbols<F>(
    out: &UnboundedSender<ServerMessage>,
    seq: &Mutex<u64>,
    root: &Path,
    full: bool,
    build: F,
) where
    F: FnOnce() -> Option<SymbolPayload>,
{
    let mut n = seq.lock().unwrap_or_else(|e| e.into_inner());
    let Some(payload) = build() else {
        return;
    };
    *n += 1;
    let _ = out.send(ServerMessage::Notification {
        sub: None,
        event: Event::ProjectSymbols {
            root: root.to_string_lossy().into_owned(),
            seq: *n,
            full,
            files: payload.files,
            go_module: payload.go_module,
            dart_package: payload.dart_package,
            structure: payload.structure,
        },
    });
}

/// Above this many changed files in one watcher batch, republish the whole
/// project rather than patching it. A directory rename expands to every
/// descendant, and past a point the patch is both a huge frame and slower to
/// apply than a fresh snapshot.
const MAX_PARTIAL_FILES: usize = 400;

/// Commit a finished `OpenProject` scan, install the watcher, then answer the
/// client — in that order, and with every lock dropped before the next step.
/// Returns whether the file list was committed (false = superseded).
///
/// `make_watcher` is not the cheap FSEvents registration this code used to
/// assume: `notify-debouncer-full`'s file-id cache walks the entire root on
/// the calling thread — no ignore filtering, `follow_links(true)`, one `stat`
/// per entry — so on a repo carrying `target/` or `node_modules/` it is
/// seconds, and through a symlink it can leave the project altogether. It used
/// to run with the files mutex held, which put that walk in front of every
/// request parked in [`Server::wait_for_files_blocking`] and in front of the
/// previous watcher's callback. Now it runs holding nothing.
///
/// What the walk is still in front of is the reply, deliberately. The watch
/// has to be live before the client is told the project is open, or a change
/// made in that window is missed until some later structural event re-scans —
/// and the window is not theoretical: replying first was tried and measured,
/// and it loses the race widely enough that five tests in `tests/protocol.rs`
/// fail, `search_sees_files_created_after_open` on its own as well as in a
/// full run. So this does NOT shorten a project open. Only dropping the
/// file-id cache does that (`NoCache`, which is what Linux already builds),
/// and that is a change to what the watcher reports, not to this ordering.
///
/// What used to justify the single critical section — "files and watcher can
/// never disagree" — is preserved by the two epoch checks instead. The first
/// is the one that matters: with it outside the files lock a superseded open
/// could pass the check, lose the race to the newer open's commit, and then
/// overwrite it, filing project A's files under project B's root. The second
/// covers the window this split opens: while we walk, a newer open can clear
/// the watcher slot and install its own, so a stale watcher must be dropped
/// here rather than written over the live one.
///
/// `reply` and `make_watcher` are parameters so the ordering can be tested
/// without a repository big enough to make the walk observable.
/// How far [`commit_open_project`] got. Three outcomes, not two: the function
/// used to answer `bool` and returned `true` both when it finished and when it
/// was superseded mid-walk, which are different states — only the first sent
/// the `Tree` reply. Nothing was wrong at the one call site (it re-checks the
/// epoch downstream anyway), but "committed" and "replied" are not the same
/// fact and a caller should not have to read this body to learn that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenCommit {
    /// Superseded before anything was written. Nothing changed.
    Superseded,
    /// Files committed, then a newer open landed while we built the watcher.
    /// The watcher was dropped and NO reply was sent.
    CommittedThenSuperseded,
    /// Files committed, watcher installed, `Tree` replied.
    Replied,
}

impl OpenCommit {
    /// Whether this open's files reached the shared slot. Both committed
    /// outcomes count: the caller's downstream work re-checks the epoch itself.
    fn committed(self) -> bool {
        matches!(
            self,
            OpenCommit::CommittedThenSuperseded | OpenCommit::Replied
        )
    }
}

#[allow(clippy::too_many_arguments)] // the whole commit sequence, not state
fn commit_open_project(
    files_slot: &SharedFiles,
    watcher_slot: &Mutex<Option<Watcher>>,
    open_epoch: &std::sync::atomic::AtomicU64,
    epoch: u64,
    root: &Path,
    files: Arc<Vec<FileEntry>>,
    reply: impl FnOnce(),
    make_watcher: impl FnOnce() -> Option<Watcher>,
) -> OpenCommit {
    use std::sync::atomic::Ordering;
    {
        let mut slot = files_slot.lock().unwrap();
        if open_epoch.load(Ordering::SeqCst) != epoch {
            return OpenCommit::Superseded;
        }
        *slot = Some(ProjectFiles {
            root: root.to_path_buf(),
            files,
        });
    }
    // The walk, with no lock held.
    let watcher = make_watcher();
    {
        let mut slot = watcher_slot.lock().unwrap();
        if open_epoch.load(Ordering::SeqCst) != epoch {
            // Superseded while we walked. Dropping `watcher` here stops its
            // thread; installing it would leave the OLD root watched and throw
            // away the watcher the newer open already put in this slot.
            return OpenCommit::CommittedThenSuperseded;
        }
        *slot = watcher;
    }
    // Last, so that by the time the client acts on the tree the watch behind
    // it is already running.
    reply();
    OpenCommit::Replied
}

/// Watch `root` recursively; stream changes back on `out` as notifications. A
/// content change emits `FilesChanged`; a create/delete, an edit to a file that
/// defines the ignore rules, or the backend reporting that it dropped events
/// also re-scans and emits an updated `Tree`. Returns the debouncer, which must
/// be kept alive to run.
///
/// Registration is NOT cheap: see [`commit_open_project`] for what
/// `Debouncer::watch` does to the calling thread and why nothing may wait on
/// this behind a lock.
fn spawn_watcher(
    root: PathBuf,
    out: UnboundedSender<ServerMessage>,
    files: SharedFiles,
    index_seq: Arc<Mutex<u64>>,
) -> Option<Watcher> {
    let cb_root = root.clone();
    let mut debouncer = new_debouncer_opt(
        DEBOUNCE,
        None,
        move |res: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = res else { return };
            on_watch_batch(&events, &cb_root, &out, &files, &index_seq);
        },
        notify_debouncer_full::NoCache,
        notify_debouncer_full::notify::Config::default(),
    )
    .ok()?;
    debouncer.watch(&root, RecursiveMode::Recursive).ok()?;
    Some(debouncer)
}

/// One debounced batch from the watcher: work out what changed, refresh the
/// server's own view of the project, and notify the client.
///
/// Split out of the callback closure so a batch can be driven directly in a
/// test — in particular the lost-events batch below, which no test can provoke
/// from the kernel.
fn on_watch_batch(
    events: &[notify_debouncer_full::DebouncedEvent],
    cb_root: &Path,
    out: &UnboundedSender<ServerMessage>,
    files: &SharedFiles,
    index_seq: &Mutex<u64>,
) {
    let mut rels: Vec<String> = Vec::new();
    let mut structural = false;
    // The backend told us it dropped events: inotify `Q_OVERFLOW`, FSEvents
    // `MUST_SCAN_SUBDIRS`. Everything below is then incomplete, so the batch is
    // answered from disk instead of from the events.
    let mut rescan = false;
    for ev in events {
        // notify reports the loss as a synthetic `EventKind::Other` carrying
        // `Flag::Rescan` and NO paths. It matches none of the kinds below, so
        // it used to be skipped — discarding the one signal that says "what I
        // told you is incomplete", and leaving the changes lost with it
        // unlearned until some unrelated create/delete happened to force a
        // scan. Nothing here can name what was missed, so both halves of the
        // work are redone from disk: the file-set scan, and a FULL symbol
        // republish rather than a patch (a lost in-place edit changes no rel,
        // so the set diff below would not see it either).
        if ev.need_rescan() {
            structural = true;
            rescan = true;
            continue;
        }
        let relevant = matches!(
            ev.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        );
        if !relevant {
            continue;
        }
        // A rename changes the file SET as much as a create/delete
        // does — `Modify(Name)` is how the watcher reports it, and
        // treating it as a content change left the tree stale.
        if matches!(
            ev.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Name(_))
        ) {
            structural = true;
        }
        for p in &ev.paths {
            // Relativize FIRST. `is_noise` rejects a path if ANY of its
            // components is a build/VCS name, so testing the absolute
            // path made every event under a root that itself sits in one
            // — /work/node_modules/app, a checkout named `target` — look
            // like noise, and in-place edits there published nothing.
            let Ok(rel) = p.strip_prefix(cb_root) else {
                continue;
            };
            // Decided BEFORE the noise filter on purpose: git's per-repo
            // exclude file lives under `.git`, which the filter drops.
            if is_ignore_rules(rel) {
                structural = true;
            }
            if is_noise(rel) {
                continue;
            }
            rels.push(rel.to_string_lossy().into_owned());
        }
    }
    // A create/delete — or an edit to the ignore rules — changes the
    // file set: re-scan, refresh the server's shared file list (so
    // search/docs/agent turns grep the current set, not the one from
    // OpenProject), and push a fresh tree.
    if structural {
        let scan = clew_core::fs_scan::scan(cb_root.to_path_buf());
        let tree_rels: Vec<String> = scan.files.iter().map(|f| f.rel.clone()).collect();
        let fresh = Arc::new(scan.files);
        let mut previous: Option<Arc<Vec<FileEntry>>> = None;
        {
            let mut slot = files.lock().unwrap();
            // Only while this watcher's project is still the open one:
            // a late callback from a replaced watcher must not clobber
            // the next project's file list.
            if slot.as_ref().is_some_and(|p| p.root == cb_root) {
                previous = slot.as_ref().map(|p| p.files.clone());
                *slot = Some(ProjectFiles {
                    root: cb_root.to_path_buf(),
                    files: fresh.clone(),
                });
            }
        }
        // What the watcher NAMES is not what changed. A directory
        // event names the directory, never the files under it, so
        // publishing that rel updated nothing — the old path's
        // descendants kept their stale symbols and the new path's were
        // never read. Worse, a rename may be reported from one side
        // only (macOS gives the destination), so even expanding the
        // named directory would leave the vacated one behind.
        //
        // Diff the file sets instead: every rel that appeared has to
        // be read, every rel that vanished has to be cleared, whatever
        // the platform chose to tell us.
        if let Some(before) = &previous {
            let before_set: std::collections::HashSet<&str> =
                before.iter().map(|f| f.rel.as_str()).collect();
            let after_set: std::collections::HashSet<&str> =
                fresh.iter().map(|f| f.rel.as_str()).collect();
            rels.extend(
                before_set
                    .symmetric_difference(&after_set)
                    .map(|rel| (*rel).to_string()),
            );
        }
        let _ = out.send(ServerMessage::Notification {
            sub: None,
            event: Event::Tree {
                root: cb_root.to_string_lossy().into_owned(),
                tree: scan.tree,
                files: tree_rels,
                truncated: scan.truncated,
            },
        });
    }
    rels.sort();
    rels.dedup();
    if rescan || rels.len() > MAX_PARTIAL_FILES {
        // A subtree rename expands to every descendant. Past a point a
        // patch is both a huge frame and slower to apply than a fresh
        // snapshot, so republish the project instead.
        //
        // A lost-events batch takes the same path for the opposite
        // reason: it names nothing at all, so a patch would carry the
        // set diff only and leave every file whose CONTENT changed
        // while the queue overflowed indexed as it was before.
        publish_project_symbols(out, index_seq, cb_root, true, || {
            let all = files
                .lock()
                .unwrap()
                .as_ref()
                .filter(|p| p.root == cb_root)
                .map(|p| p.files.clone())?;
            let structure = clew_core::structure::build(cb_root, &all);
            Some(SymbolPayload {
                files: build_project_symbols(cb_root, &all),
                go_module: clew_protocol::Patch::Set(clew_core::imports::read_go_module(cb_root)),
                dart_package: clew_protocol::Patch::Set(clew_core::imports::read_dart_package(
                    cb_root,
                )),
                structure: clew_protocol::Patch::Set(
                    (!structure.is_empty())
                        .then(|| serde_json::to_string(&structure).ok())
                        .flatten(),
                ),
            })
        });
    } else if !rels.is_empty() {
        // Read and publish under the publication lock, so this
        // update's `seq` reflects when its files were READ. Without
        // that, a full snapshot still building elsewhere is stamped
        // later and overwrites these fresher entries with what those
        // files looked like before the change.
        publish_project_symbols(out, index_seq, cb_root, false, || {
            // Per-file symbol updates for the changed set, so a remote
            // client's index stays fresh without local reads. A rel
            // that no longer resolves to an indexable file gets an
            // empty entry — "clear what you had". (This thread is the
            // watcher's own; the reads don't block the request loop.)
            let files_out: Vec<clew_protocol::FileSymbols> =
                rels.iter()
                    .map(|rel| {
                        file_symbols_for(cb_root, &cb_root.join(rel), rel, 512 * 1024)
                            .unwrap_or_else(|| clew_protocol::FileSymbols {
                                rel: rel.clone(),
                                symbols: Vec::new(),
                                imports: Vec::new(),
                            })
                    })
                    .collect();
            // Resolution metadata and the structure index are
            // re-extracted only when their INPUTS changed, and the
            // result is sent as a `Patch` — `Set(None)` says the value
            // is GONE. Collapsing that into a bare `None` made it
            // indistinguishable from "not recomputed", so a deleted
            // `go.mod` module line kept mis-resolving every Go import
            // in the project until it was reopened.
            let go_module = match rels.iter().any(|r| r == "go.mod") {
                true => clew_protocol::Patch::Set(clew_core::imports::read_go_module(cb_root)),
                false => clew_protocol::Patch::Unchanged,
            };
            let dart_package = match rels.iter().any(|r| r == "pubspec.yaml") {
                true => clew_protocol::Patch::Set(clew_core::imports::read_dart_package(cb_root)),
                false => clew_protocol::Patch::Unchanged,
            };
            // The structure index is whole-project (a trait's
            // implementors live anywhere), so it is rebuilt rather
            // than patched. Only for batches that can affect it, on
            // the watcher's own debounced thread — never on the
            // request loop.
            let structure = if rels.iter().any(|r| r.ends_with(".rs")) {
                // Cloned out on its own line: the guard must not be
                // held across the rebuild below.
                let all = files.lock().unwrap().as_ref().map(|p| p.files.clone());
                match all {
                    Some(all) => {
                        let index = clew_core::structure::build(cb_root, &all);
                        clew_protocol::Patch::Set(
                            (!index.is_empty())
                                .then(|| serde_json::to_string(&index).ok())
                                .flatten(),
                        )
                    }
                    // Could not recompute (no project). Say nothing,
                    // rather than claim the index is gone.
                    None => clew_protocol::Patch::Unchanged,
                }
            } else {
                clew_protocol::Patch::Unchanged
            };
            Some(SymbolPayload {
                files: files_out,
                go_module,
                dart_package,
                structure,
            })
        });
    }
    // Both publication paths tell the client which files moved, so a
    // local client's own pipelines reindex the same set.
    //
    // A lost-events batch names nothing, so it sends this only for whatever the
    // set diff turned up. Residual, stated rather than papered over: an OPEN
    // buffer whose bytes changed inside the dropped burst is not re-read by the
    // client until it is touched again — the server's own index recovers above,
    // the client's editor view does not.
    if !rels.is_empty() {
        let _ = out.send(ServerMessage::Notification {
            sub: None,
            event: Event::FilesChanged {
                root: cb_root.to_string_lossy().into_owned(),
                rels,
            },
        });
    }
}

/// Skip VCS internals, build output, dependencies, and clew's own data dir so a
/// `cargo build` or `npm install` doesn't drown the channel.
fn is_noise(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some(".git")
                | Some("target")
                | Some("node_modules")
                | Some(".clew")
                | Some(".hg")
                | Some(".svn")
                | Some(".idea")
                | Some(".DS_Store")
        )
    })
}

/// Does this root-relative path define which files belong to the project? The
/// scanner re-reads these on every scan, so an edit to one changes the file SET
/// without creating or removing anything: a plain in-place write (`echo >>`,
/// `sed -i`) is a bare `Modify(Data)`, nothing else in the batch marks it
/// structural, and the stale rules survive — newly ignored files stay in the
/// tree and the search set, newly un-ignored ones stay invisible — until some
/// unrelated create/delete or a reopen forces a rescan. (Atomic-write editors
/// and `git checkout` emit Create/`Modify(Name)` and were always covered.)
fn is_ignore_rules(rel: &Path) -> bool {
    if matches!(
        rel.file_name().and_then(|n| n.to_str()),
        Some(".gitignore") | Some(".ignore")
    ) {
        return true;
    }
    // The repo-local exclude list, equal in force to a `.gitignore`.
    rel.ends_with(".git/info/exclude")
}

/// Run the server over stdio until the client's stream ends (or stdin closes).
///
/// Framing is newline-delimited JSON: each `ClientMessage` arrives as one line,
/// each `ServerMessage` is written back as one line. serde_json's compact output
/// never contains a literal newline (string values escape theirs), so a line is
/// always exactly one message. A dedicated writer task drains the output channel
/// so replies and unsolicited notifications (file changes) share one stdout.
pub async fn serve_stdio() {
    let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let mut server = Server::new(out.clone());
    let budget = server.output_budget();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            // Credit ProcessOutput bytes back to the stdout budget once
            // written (or unserializable): the pumps wait on this while the
            // transport is behind.
            let charged = match &msg {
                ServerMessage::Notification {
                    event: Event::ProcessOutput { data, .. },
                    ..
                } => data.len(),
                _ => 0,
            };
            let json = serde_json::to_string(&msg);
            if charged > 0 {
                budget.release(charged);
            }
            let Ok(mut json) = json else {
                continue;
            };
            // Last line of defence on frame size. Every construction site has
            // its own budget, so reaching this means one of them is wrong —
            // but writing the frame anyway would make the CLIENT hang up (it
            // cannot resync past an over-cap line), turning a bug in one
            // reply into a dropped connection. A correlated reply degrades to
            // an error the caller can surface; a notification is dropped.
            if json.len() > clew_protocol::MAX_FRAME_BYTES {
                eprintln!(
                    "[clew-server] refusing to send a {}-byte frame (cap {}); this is a missing \
                     construction-site budget",
                    json.len(),
                    clew_protocol::MAX_FRAME_BYTES
                );
                let ServerMessage::Reply { id, sub, .. } = msg else {
                    continue;
                };
                let Ok(replacement) = serde_json::to_string(&ServerMessage::Reply {
                    id,
                    sub,
                    event: Event::Error {
                        message: "the reply was too large to send".into(),
                    },
                }) else {
                    continue;
                };
                json = replacement;
            }
            json.push('\n');
            if stdout.write_all(json.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(tokio::io::stdin());
    while let Some(line) = read_frame_line(&mut reader).await {
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
            && out
                .send(ServerMessage::Reply {
                    id,
                    sub: None,
                    event,
                })
                .is_err()
        {
            break; // writer gone
        }
    }
    drop(server); // stop the watcher
    drop(out); // close the channel so the writer task ends
    // Bounded, because dropping OUR sender is not enough to close the channel:
    // background work (an agent turn blocked on a provider that stopped
    // sending) holds clones of it, and waiting outright made the exit hostage
    // to a stream that might never end. Long enough to flush anything real
    // that is still queued, short enough that a wedged stream cannot pin the
    // process — it dies with us either way.
    const FLUSH_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
    if tokio::time::timeout(FLUSH_GRACE, writer).await.is_err() {
        eprintln!("[clew-server] exiting with output still in flight");
    }
}

/// Read one newline-terminated protocol frame, capped at
/// [`clew_protocol::MAX_FRAME_BYTES`]. `None` on EOF, on a read error, or on
/// an oversized frame — an over-cap line cannot be resynced past, so the
/// connection ends (the client reconnects with fresh state).
async fn read_frame_line<R>(reader: &mut R) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let n = reader
        .take(clew_protocol::MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
        .ok()?;
    if n == 0 {
        return None; // EOF
    }
    if buf.len() > clew_protocol::MAX_FRAME_BYTES {
        return None; // over the cap: fail closed
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    String::from_utf8(buf).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Event, OpenCommit, ProjectFiles, Server, ServerMessage, SharedApprovals, SharedFiles,
        Watcher, approved_init_options, commit_open_project, confine, is_generated_source,
        on_watch_batch, read_frame_line, run_merge, spawn_watcher,
    };
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// Registering the watch must hold no lock. `Debouncer::watch` is not the
    /// cheap syscall this code once assumed: it walks the whole root —
    /// unfiltered, following symlinks, one `stat` per entry — on the calling
    /// thread, seconds on a repo carrying `target/` or `node_modules/`. It ran
    /// inside the commit's critical section, so every request parked in
    /// `wait_for_files_blocking` (Search, ReadSources, AgentAsk, BuildDocs) and
    /// the previous watcher's callback waited it out.
    ///
    /// The reply stays behind it on purpose, and that half is asserted here
    /// too: the watch must be live before the client is told the project is
    /// open, or a change made in that window is missed until some later
    /// structural event re-scans. Moving the reply first does shorten the open,
    /// and it also loses that race consistently enough to fail four watcher
    /// tests in `tests/protocol.rs`.
    ///
    /// Rendezvous, not timing: the factory parks INSIDE the walk, so the
    /// assertions observe exactly the state at that instant.
    #[test]
    fn an_open_registers_its_watch_holding_no_lock_and_replies_only_after() {
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let watcher: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(7));
        let (entered_walk, in_walk) = std::sync::mpsc::channel::<()>();
        let (may_finish, finish_now) = std::sync::mpsc::channel::<()>();
        let (replied, saw_reply) = std::sync::mpsc::channel::<()>();
        let root = std::env::temp_dir().join("clew-server-ut-open-order");
        let (f, w, e, r) = (files.clone(), watcher.clone(), epoch.clone(), root.clone());
        let task = std::thread::spawn(move || {
            commit_open_project(
                &f,
                &w,
                &e,
                7,
                &r,
                Arc::new(Vec::new()),
                || replied.send(()).unwrap(),
                || {
                    entered_walk.send(()).unwrap();
                    finish_now.recv().unwrap();
                    None
                },
            )
        });
        in_walk.recv().unwrap();
        // The committed file list is reachable while the walk runs, not locked
        // away for its duration...
        let guard = files
            .try_lock()
            .expect("the files lock must not be held across the watch registration");
        assert_eq!(
            guard.as_ref().map(|p| p.root.clone()),
            Some(root),
            "the file list must be committed before the walk, not after it"
        );
        drop(guard);
        assert!(
            watcher.try_lock().is_ok(),
            "the watcher lock must not be held across the registration either"
        );
        // ...and the client has not been told yet, because the watch it will
        // act against is not running.
        assert!(
            saw_reply.try_recv().is_err(),
            "the Tree reply must follow the watch registration"
        );
        may_finish.send(()).unwrap();
        assert_eq!(
            task.join().unwrap(),
            OpenCommit::Replied,
            "the commit ran to completion"
        );
        assert!(saw_reply.try_recv().is_ok(), "and the reply did go out");
    }

    /// Building the watcher outside the commit's critical section opens a
    /// window: a newer `OpenProject` can clear the slot and install its own
    /// while we walk. The stale watcher must then be DROPPED — writing it over
    /// the live one would leave the old root watched and the new project not
    /// watched at all, the disagreement the single critical section used to
    /// rule out.
    #[test]
    fn a_watcher_built_for_a_superseded_open_is_dropped_not_installed() {
        let root = std::env::temp_dir().join("clew-server-ut-open-superseded");
        std::fs::create_dir_all(&root).unwrap();
        // `spawn_watcher` needs a channel and a sequence counter; nothing here
        // reads them, and a dropped receiver only makes the callback's sends
        // no-ops.
        let make = |root: std::path::PathBuf| {
            let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
            spawn_watcher(
                root,
                out,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(0)),
            )
        };

        // Control: nothing supersedes it, so the watcher is installed.
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(1));
        let committed = commit_open_project(
            &files,
            &slot,
            &epoch,
            1,
            &root,
            Arc::new(Vec::new()),
            || {},
            || make(root.clone()),
        );
        assert_eq!(committed, OpenCommit::Replied);
        assert!(
            slot.lock().unwrap().is_some(),
            "an open that was not superseded installs its watcher"
        );

        // Superseded DURING the walk: the file list was still committed under
        // the matching epoch, but the watcher must not land.
        let files: SharedFiles = Arc::new(Mutex::new(None));
        let slot: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let epoch = Arc::new(AtomicU64::new(1));
        let bumping = epoch.clone();
        let committed = commit_open_project(
            &files,
            &slot,
            &epoch,
            1,
            &root,
            Arc::new(Vec::new()),
            || {},
            || {
                bumping.fetch_add(1, Ordering::SeqCst); // a newer OpenProject
                make(root.clone())
            },
        );
        assert_eq!(
            committed,
            OpenCommit::CommittedThenSuperseded,
            "the commit itself won its race, but the reply never went out"
        );
        assert!(
            slot.lock().unwrap().is_none(),
            "a watcher built for a superseded open must be dropped, not installed"
        );
    }

    /// The backend can report that it LOST events — inotify `Q_OVERFLOW`,
    /// FSEvents `MUST_SCAN_SUBDIRS` — and notify passes that on as a synthetic
    /// `EventKind::Other` carrying `Flag::Rescan` and no paths. It matched none
    /// of the kinds the callback looks for, so the one signal meaning "what I
    /// told you is incomplete" was dropped and the changes lost with it were
    /// never learned.
    ///
    /// It has to drive the whole recovery instead: re-scan the set (refreshed
    /// shared file list, fresh `Tree`) and republish EVERY file's symbols,
    /// because the batch names no file that a patch could carry — a content
    /// edit lost in the burst appears in no set diff.
    ///
    /// The kernel cannot be made to overflow from a test, so the batch is
    /// handed to `on_watch_batch` directly.
    #[test]
    fn a_lost_events_signal_drives_a_full_rescan() {
        use notify_debouncer_full::DebouncedEvent;
        use notify_debouncer_full::notify::{EventKind, event::Flag};

        let root = std::env::temp_dir().join("clew-server-ut-watch-rescan");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "pub fn appeared() {}\n").unwrap();

        // Deliberately stale: the project is open with an EMPTY file list, so
        // nothing below can pass unless the batch itself went back to disk.
        let stale = || -> SharedFiles {
            Arc::new(Mutex::new(Some(ProjectFiles {
                root: root.clone(),
                files: Arc::new(Vec::new()),
            })))
        };
        let batch = |ev: DebouncedEvent, files: &SharedFiles| {
            let (out, rx) = tokio::sync::mpsc::unbounded_channel();
            on_watch_batch(&[ev], &root, &out, files, &Mutex::new(0));
            drop(out);
            rx
        };

        // Control first: `Other` WITHOUT the flag is an uninteresting event
        // (notify uses it for anything it can't classify) and must stay
        // ignored. If this ever fires, the gate below was widened to the kind
        // rather than to the flag.
        let files = stale();
        let mut rx = batch(
            DebouncedEvent::new(
                notify_debouncer_full::notify::Event::new(EventKind::Other),
                std::time::Instant::now(),
            ),
            &files,
        );
        assert!(
            rx.try_recv().is_err(),
            "an unflagged `Other` event must publish nothing"
        );
        assert!(
            files.lock().unwrap().as_ref().unwrap().files.is_empty(),
            "and must not re-scan the project"
        );

        // The real thing.
        let files = stale();
        let mut rx = batch(
            DebouncedEvent::new(
                notify_debouncer_full::notify::Event::new(EventKind::Other).set_flag(Flag::Rescan),
                std::time::Instant::now(),
            ),
            &files,
        );

        assert!(
            files
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.rel == "a.rs"),
            "the server's own file list must re-converge: search, docs and agent \
             turns grep this list"
        );
        let (mut tree, mut symbols) = (false, None);
        while let Ok(msg) = rx.try_recv() {
            match msg {
                ServerMessage::Notification {
                    event: Event::Tree { files, .. },
                    ..
                } => tree = files.iter().any(|r| r == "a.rs"),
                ServerMessage::Notification {
                    event: Event::ProjectSymbols { full, files, .. },
                    ..
                } => symbols = Some((full, files)),
                _ => {}
            }
        }
        assert!(tree, "the client must be sent a rebuilt tree");
        let (full, files) = symbols.expect("the symbol index must be republished");
        assert!(
            full,
            "the republish must be FULL: a patch carries only the files the batch \
             named, and this batch names none"
        );
        assert!(
            files.iter().any(|f| f.rel == "a.rs"),
            "and it must carry the project's symbols"
        );
    }

    /// Frames read within the cap; a single over-cap "line" ends the stream
    /// instead of growing memory without bound.
    #[tokio::test]
    async fn frame_reader_enforces_the_cap() {
        let mut ok =
            tokio::io::BufReader::new(std::io::Cursor::new(b"{\"id\":1}\nnext\n".to_vec()));
        assert_eq!(
            read_frame_line(&mut ok).await.as_deref(),
            Some("{\"id\":1}")
        );
        assert_eq!(read_frame_line(&mut ok).await.as_deref(), Some("next"));
        assert_eq!(read_frame_line(&mut ok).await, None); // EOF

        // An over-cap line: None, fail closed. (Simulated with a reader whose
        // one line exceeds the cap — built sparsely to keep the test cheap.)
        let big = vec![b'x'; clew_protocol::MAX_FRAME_BYTES + 2];
        let mut over = tokio::io::BufReader::new(std::io::Cursor::new(big));
        assert_eq!(read_frame_line(&mut over).await, None);
    }

    #[test]
    fn detects_generated_sources() {
        // Dart freezed / .g.dart, flutter_rust_bridge, protobuf, prost headers.
        assert!(is_generated_source(
            "// coverage:ignore-file\n// GENERATED CODE - DO NOT MODIFY BY HAND\n"
        ));
        assert!(is_generated_source(
            "// This file is automatically generated, so please do not edit it.\n// @generated by `flutter_rust_bridge`\n"
        ));
        assert!(is_generated_source(
            "// Code generated by protoc-gen-go. DO NOT EDIT.\n"
        ));
        assert!(is_generated_source("# @generated by prost-build\n"));
        // ruff style: "generated file" + the "don't" contraction (both misses before).
        assert!(is_generated_source(
            "// This is a generated file. Don't modify it by hand!\n"
        ));
        assert!(is_generated_source(
            "// This is a generated file. Don\u{2019}t modify it by hand!\n"
        ));
        // Hand-written source is not skipped.
        assert!(!is_generated_source(
            "/// Starts the main function in Rust.\npub fn initialize() {}\n"
        ));
        assert!(!is_generated_source("import 'dart:async';\nclass Foo {}\n"));
    }

    #[test]
    fn confine_allows_files_inside_root() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confine(root, "Cargo.toml").is_some());
        assert!(confine(root, "src/lib.rs").is_some());
    }

    #[test]
    fn confine_rejects_escapes() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(confine(root, "/etc/passwd").is_none()); // absolute path
        assert!(confine(root, "../clew-core/Cargo.toml").is_none()); // parent escape
        assert!(confine(root, "src/../../Cargo.toml").is_none()); // .. in the middle
        assert!(confine(root, "does/not/exist.rs").is_none()); // nonexistent
    }

    fn bookmark_toggle(rel: &str, line: i64) -> clew_protocol::StateMerge {
        clew_protocol::StateMerge {
            key_fields: vec!["rel".into(), "line".into()],
            key: vec![rel.into(), line.into()],
            edit: clew_protocol::StateEdit::Toggle(
                serde_json::json!({"rel": rel, "line": line, "preview": rel}),
            ),
            delete_when_empty: true,
        }
    }

    fn rels_in(path: &Path) -> Vec<String> {
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| "[]".into());
        serde_json::from_str::<Vec<serde_json::Value>>(&text)
            .unwrap()
            .iter()
            .map(|e| e["rel"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Two clients on ONE remote project, each holding the snapshot it loaded
    /// at project open. Both must keep their bookmark: the server is the only
    /// place both writers are visible, so the read-modify-write happens here.
    #[test]
    fn two_divergent_clients_both_keep_their_edit() {
        let dir = std::env::temp_dir().join("clew-server-state-merge");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".clew")).unwrap();
        let path = dir.join(".clew").join("bookmarks.json");
        let start = r#"[{"rel":"a.rs","line":1,"preview":"a.rs"}]"#;
        std::fs::write(&path, start).unwrap();

        // What a whole-snapshot write did: client A adds b.rs, then client B —
        // still holding [a.rs] — adds c.rs and ships its whole list.
        clew_core::statefile::write_atomic(
            &path,
            br#"[{"rel":"a.rs","line":1},{"rel":"c.rs","line":3}]"#,
        )
        .unwrap();
        assert!(
            !rels_in(&path).contains(&"b.rs".to_string()),
            "the wholesale write is what destroyed the other client's bookmark"
        );

        // The same two saves as merges, from the same divergent snapshots.
        std::fs::write(&path, start).unwrap();
        run_merge(&path, "bookmarks.json", &bookmark_toggle("b.rs", 2)).unwrap();
        run_merge(&path, "bookmarks.json", &bookmark_toggle("c.rs", 3)).unwrap();
        assert_eq!(rels_in(&path), ["a.rs", "b.rs", "c.rs"]);

        // The reply carries the merged file, so the client stops disagreeing
        // with disk instead of re-sending its own copy.
        let merged = run_merge(&path, "bookmarks.json", &bookmark_toggle("d.rs", 4))
            .unwrap()
            .expect("not empty");
        assert_eq!(
            serde_json::from_str::<Vec<serde_json::Value>>(&merged)
                .unwrap()
                .len(),
            4
        );

        // Emptying the store deletes its file, as an empty list does locally.
        for (rel, line) in [("a.rs", 1), ("b.rs", 2), ("c.rs", 3), ("d.rs", 4)] {
            run_merge(&path, "bookmarks.json", &bookmark_toggle(rel, line)).unwrap();
        }
        assert!(!path.exists());
    }

    /// Isolate the store/trust directory for a test, so nothing here reads the
    /// developer's real approvals. Serialized: `CLEW_DATA_DIR` is process-wide.
    fn with_data_dir<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        static ENV: Mutex<()> = Mutex::new(());
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: env mutation serialized by ENV, held for the whole call.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        let out = f(&dir);
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
        out
    }

    /// The remote twin of the client's gate. `resolve_lsp` is what feeds the
    /// client's `initialize` for a remote project — the client cannot re-derive
    /// the verdict (the fingerprint covers THIS host's server/version/args), so
    /// unapproved options must never leave in `init_options`. Left open, this
    /// was the unguarded sibling of the local path: clone a repo on the SSH
    /// host, open one file, and its `init_options` reached the language server
    /// with nothing asked.
    ///
    /// The other half is that a refusal must be grantable. `Ready::withheld`
    /// carries what the approval needs, and this pins the two halves against
    /// each other: the fingerprint the client is offered is the same one the
    /// gate then accepts. If they drifted, approving would record a value the
    /// gate does not recognise and the modal would come straight back, with no
    /// way out but closing the project.
    #[test]
    fn a_remote_resolve_withholds_init_options_until_they_are_approved() {
        with_data_dir("clew-server-ut-lsp-options", |data| {
            // A store-installed rust-analyzer, as any earlier project leaves.
            let version = clew_core::lsp::registry::by_name("rust-analyzer")
                .unwrap()
                .version;
            let store = data.join("servers").join("rust-analyzer").join(version);
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join("rust-analyzer"), b"#!/bin/sh\nexit 0\n").unwrap();

            let root = data.join("proj");
            std::fs::create_dir_all(root.join(".clew")).unwrap();
            std::fs::write(
                root.join(".clew/lsp.toml"),
                "[rust.init_options]\n\
                 \"rust-analyzer.cargo.buildScripts.overrideCommand\" = [\"/bin/sh\", \"-c\", \"id\"]\n",
            )
            .unwrap();

            let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let approvals: SharedApprovals = Arc::new(Mutex::new(HashMap::new()));
            let resolution = Server::resolve_lsp(&out, &approvals, &root, "rust");
            let offered = match resolution {
                clew_protocol::LspResolution::Ready {
                    init_options,
                    withheld,
                } => {
                    assert!(
                        init_options.is_none(),
                        "unapproved options must not cross the wire, got {init_options:?}"
                    );
                    withheld.expect("a withheld config must come with the means to allow it")
                }
                other => panic!("expected Ready, got {other:?}"),
            };
            // What the modal will show, so the user approves what they read.
            assert!(
                offered.options.contains("overrideCommand"),
                "the withheld options must be shown: {:?}",
                offered.options
            );
            assert_eq!(offered.server, "rust-analyzer");
            // …and the user is told, rather than left wondering why the config
            // they committed has no effect.
            let told = std::iter::from_fn(|| rx.try_recv().ok()).any(|m| {
                matches!(m, ServerMessage::Notification { event: Event::Error { message }, .. }
                    if message.contains("init_options") && message.contains("not approved"))
            });
            assert!(told, "withholding must be reported, not silent");

            // The client approves it there and pushes the fingerprint here.
            // It pushes back exactly what it was OFFERED — the round trip the
            // grant path is made of — so the gate must accept that value.
            let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
            let server = config.resolve("rust").unwrap();
            let fingerprint = clew_core::trust::lsp_options_fingerprint(
                &server.args,
                &server.server_name,
                &server.version,
                server.init_options.as_ref().unwrap(),
            )
            .unwrap();
            assert_eq!(
                offered.fingerprint, fingerprint,
                "the value offered for approval must be the one the gate checks"
            );
            approvals
                .lock()
                .unwrap()
                .insert("rust".into(), offered.fingerprint.clone());
            match Server::resolve_lsp(&out, &approvals, &root, "rust") {
                clew_protocol::LspResolution::Ready {
                    init_options,
                    withheld,
                } => {
                    assert!(
                        init_options
                            .as_deref()
                            .is_some_and(|o| o.contains("overrideCommand")),
                        "an approved config must get its options"
                    );
                    assert!(
                        withheld.is_none(),
                        "nothing is withheld once it is approved"
                    );
                }
                other => panic!("expected Ready, got {other:?}"),
            }

            // A commit that edits only the options loses that approval — the
            // stale fingerprint must not keep covering the new ones.
            std::fs::write(
                root.join(".clew/lsp.toml"),
                "[rust.init_options]\n\"rust-analyzer.procMacro.server\" = \"./payload\"\n",
            )
            .unwrap();
            match Server::resolve_lsp(&out, &approvals, &root, "rust") {
                clew_protocol::LspResolution::Ready {
                    init_options,
                    withheld,
                } => {
                    assert!(init_options.is_none(), "edited options need a fresh answer");
                    // And the fresh answer is askable: a stale approval must
                    // not leave the new options unallowable either.
                    let offered = withheld.expect("edited options must be offered for approval");
                    assert_ne!(offered.fingerprint, fingerprint);
                    assert!(offered.options.contains("procMacro"));
                }
                other => panic!("expected Ready, got {other:?}"),
            }

            // A config with no options at all is untouched by the gate: it is
            // not withheld, so it must not raise a prompt either.
            std::fs::write(root.join(".clew/lsp.toml"), "[rust]\nenabled = true\n").unwrap();
            let quiet = Server::resolve_lsp(&out, &approvals, &root, "rust");
            assert!(matches!(
                quiet,
                clew_protocol::LspResolution::Ready {
                    init_options: None,
                    withheld: None
                }
            ));

            // And the helper agrees with itself: same inputs, same verdict,
            // whichever spawn path asks. The agent's LSP pool calls it
            // directly — an Ask turn must not be the way around the gate.
            let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
            let server = config.resolve("rust").unwrap();
            assert_eq!(
                approved_init_options(&approvals, &root, &server).0,
                None,
                "no options in the config means nothing to send"
            );
            std::fs::write(
                root.join(".clew/lsp.toml"),
                "[rust.init_options]\n\"rust-analyzer.procMacro.server\" = \"./payload\"\n",
            )
            .unwrap();
            let config = clew_core::lsp::config::ProjectLspConfig::load(&root).unwrap();
            let server = config.resolve("rust").unwrap();
            let (sent, withheld) = approved_init_options(&approvals, &root, &server);
            assert_eq!(sent, None, "the agent pool must withhold them too");
            assert!(withheld.is_some(), "and say why");
        });
    }
}
