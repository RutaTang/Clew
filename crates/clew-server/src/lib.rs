//! clew-server: the headless backend.
//!
//! It owns all filesystem / OS interaction and answers the client over
//! `clew-protocol`. The same `Server` logic runs whether the transport is a
//! local child process (stdio) or an SSH session to a remote host; the client
//! only ever speaks the protocol. [`Server::handle`] answers every request —
//! slow work (scans, reads, greps, git, model calls) runs off the request loop
//! and replies from its own task, so a queued `ProcessKill` or `Cancel` is
//! never stuck behind it — and a request whose work fails unexpectedly is
//! answered with an error rather than left waiting.
//!
//! Modules: `transport` (framing, the writer, backpressure), `files`
//! (ReadFile, ListDir), `state` (`.clew/` stores), `process` (proxied
//! children), `watch` (the watcher), `index` (symbols, call graph, docs),
//! `gitops` (the git bridge), `lsp_gate` (language-server approvals), and the
//! Ask agent (`agent`, `agent_lsp`).
//!
//! ## Trust
//!
//! The client is the user's own program, reached over the user's own login, so
//! this server does not defend against a hostile CLIENT — whoever drives it can
//! already run commands on the host. It defends against hostile DATA arriving
//! through a faithful client: a repository's committed files (a `.clew/`
//! symlink, a hostile `lsp.toml`), paths a model wrote into an answer or a tool
//! call, requests a stale client sends for the wrong project. Hence every
//! client path is confined to the project ([`clew_core::confine`]), a
//! repository-specified command runs only with the user's approval
//! ([`lsp_command_allowed`]), and nothing is installed without consent.
//!
//! `SpawnProcess` — an arbitrary command line — fits that model only for a
//! LOCAL client, which resolved and gated the command itself. A remote client
//! never needs it (remote language servers and debug adapters are resolved and
//! gated here, through `SpawnLsp` / `SpawnAdapter`), so over SSH it is refused
//! instead of being kept as an unguarded way to run anything ([`SpawnPolicy`]).

// These docs are for clew's own developers and are built with
// `--document-private-items` (the doc gate in .github/workflows/ci.yml), so a
// public item's doc may link to the private helper that does the work: such a
// link resolves there. rustdoc still flags it in that mode, hence the allow.
#![allow(rustdoc::private_intra_doc_links)]

pub mod agent;
pub mod agent_lsp;
mod files;
mod gitops;
mod index;
mod lsp_gate;
mod process;
mod state;
mod transport;
mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use clew_core::fs_scan::FileEntry;
use clew_core::{embed, explain, git, inactive, llm, search};
use clew_protocol::{
    ErrorCode, Event, PROTOCOL_VERSION, Request, RequestId, ServerMessage, StreamOutcome,
};
use tokio::sync::mpsc::UnboundedSender;

use files::list_dir;
use gitops::validate_git_op;
use index::{
    MAX_INDEX_FILE_BYTES, build_docs, build_project_calls_graph, full_symbol_payload,
    publish_project_symbols,
};
pub use lsp_gate::{SharedApprovals, approved_init_options, lsp_command_allowed};
use process::{
    MAX_PROC_INPUT_BYTES, SharedProcs, Spawned, kill_removed, register_proc, spawn_registered,
    superseded,
};
use state::{StateJob, StateWork, spawn_state_worker, wrong_project};
use transport::send_bulk;
pub use transport::{OutputBudget, serve, serve_stdio};
use watch::{OpenCommit, Watcher, commit_open_project, watch_or_report};

/// The open project's scanned file list, tagged with the root it belongs to.
/// Shared between the request loop and the watcher (which refreshes it after a
/// structural change), so search / docs / agent turns always grep the current
/// file set instead of the one from the last `OpenProject`.
struct ProjectFiles {
    root: PathBuf,
    files: Arc<Vec<FileEntry>>,
}

type SharedFiles = Arc<Mutex<Option<ProjectFiles>>>;

/// Whether this server runs `SpawnProcess` — an arbitrary command line from
/// the client. See the crate docs ("Trust").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnPolicy {
    /// A local child of the app: it spawns the language servers and debug
    /// adapters it resolved (and gated) itself.
    Local,
    /// Over SSH: `SpawnProcess` is refused. Remote language servers and
    /// adapters are resolved and gated on this host (`SpawnLsp`,
    /// `SpawnAdapter`), so a remote client never needs it.
    Remote,
}

impl SpawnPolicy {
    /// `--remote` / `--local` on the command line decide, for a launcher that
    /// knows; otherwise the environment does: sshd sets `SSH_CONNECTION` (and
    /// `SSH_CLIENT`) for every session it runs a command in, and the app's own
    /// local launch sets neither.
    pub fn detect(args: &[String], env: impl Fn(&str) -> Option<String>) -> SpawnPolicy {
        if args.iter().any(|a| a == "--remote") {
            return SpawnPolicy::Remote;
        }
        if args.iter().any(|a| a == "--local") {
            return SpawnPolicy::Local;
        }
        let set = |name: &str| env(name).is_some_and(|v| !v.trim().is_empty());
        if set("SSH_CONNECTION") || set("SSH_CLIENT") {
            SpawnPolicy::Remote
        } else {
            SpawnPolicy::Local
        }
    }
}

/// Make a panic on the CALLING thread end the process at once (`abort`, after
/// the usual panic message); a panic on any other thread unwinds as before.
///
/// For the binary's request loop, which `main` runs on its own thread inside
/// `block_on`. A panic there used to unwind out of `block_on` and drop the
/// runtime on the way out — and a runtime drop waits, with no limit, for every
/// blocking thread still running: a model call, an agent turn exploring (and
/// billing) for a client that is gone. [`Server::shutdown`] never ran, so
/// nothing told them to stop, and the bounded `shutdown_timeout` in `main` was
/// never reached. Aborting ends the process and every thread in it: the OS
/// closes every pipe and socket it held, so in-flight provider requests die
/// with their connections, and proxied children see EOF on their stdin (and
/// a broken pipe on their next write) — which is how a language server or a
/// debug adapter is told to exit. They are not KILLED: each leads a process
/// group of its own (`process::spawn_registered`), which a dying parent does
/// not signal, so one that ignores EOF — or a helper it started — outlives
/// the server. Only a shutdown that runs ([`Server::shutdown`]) kills them
/// with their groups. (macOS has no parent-death signal to hand them.)
///
/// Nor may another thread's panic reach this one through a lock: the request
/// loop takes a poisoned mutex's value (`PoisonError::into_inner`) rather than
/// unwrapping it — every critical section here leaves its map or slot whole —
/// where an `unwrap` turned a worker's panic under a lock into the end of the
/// whole server.
///
/// This thread only, rather than `panic = "abort"` for the whole binary: work
/// on the runtime's other threads (a scan, a search, a git op, an agent turn)
/// is caught by its task and answered with an error — the protocol's promise
/// that a request is answered even when its work panicked
/// ([`ErrorCode::Failed`]), which an abort-everything build would turn into a
/// dropped connection. (It could not be a Cargo profile setting either:
/// `panic` cannot be set per package, and the GUI in this workspace relies on
/// unwinding the same way.)
pub fn abort_on_panic_in_this_thread() {
    end_process_on_panic_in_this_thread(std::process::abort);
}

/// [`abort_on_panic_in_this_thread`] with how the process ends given — a test
/// ends its child with an exit code instead, which proves the same thing
/// without leaving the host a crash report per run.
fn end_process_on_panic_in_this_thread(end: fn() -> !) {
    let this = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if std::thread::current().id() == this {
            end();
        }
    }));
}

/// How long a `ListDir` may take before it is answered with an error. The
/// picker browses arbitrary paths, and a hung network mount would otherwise
/// leave it spinning; the stuck thread is abandoned, the request answered.
const LIST_DIR_TIMEOUT: Duration = Duration::from_secs(15);

/// Most skipped files one `SearchResults` lists by name (`skipped_total`
/// still counts them all). A tree of huge dumps must not turn the reply to a
/// search into a frame the size of the tree's file list.
const MAX_SKIPPED_LISTED: usize = 100;

/// What a request that needs the project's file list is told while the
/// `OpenProject` scan is still running (after a bounded wait) — the text of
/// an [`ErrorCode::NotReady`], and of an agent turn that could not start.
pub(crate) const NOT_READY: &str = "not ready: the project scan has not finished";

/// An [`ErrorCode::Refused`] reply: not attempted, and would be refused again.
pub(crate) fn refused(message: impl Into<String>) -> Event {
    Event::error(ErrorCode::Refused, message)
}

/// An [`ErrorCode::Failed`] reply: attempted, and it did not work.
pub(crate) fn failed(message: impl Into<String>) -> Event {
    Event::error(ErrorCode::Failed, message)
}

/// Tell the client, in a `Status` notice, what a project scan left out or
/// added beyond the plain `.gitignore` walk
/// ([`clew_core::fs_scan::ScanReport::summary`]): entries it could not name
/// or read, tracked files it lists although the ignore rules hide them, or
/// that git could not say which files are tracked. Sent right behind the
/// `Tree` it describes — at `OpenProject` and after every watcher rescan — and
/// never for a plain walk. It used to reach only this process's log, where no
/// reader saw it.
pub(crate) fn send_scan_report(
    out: &UnboundedSender<ServerMessage>,
    report: &clew_core::fs_scan::ScanReport,
) {
    if let Some(message) = report.summary() {
        let _ = out.send(ServerMessage::Notification {
            event: Event::Status { message },
        });
    }
}

/// Backend state for one client connection: the open project (root, file
/// list, watcher), the processes and model calls it started, and the
/// credentials and approvals the client pushed.
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
    /// Backpressure for the bulk producers — proxied stdout, trees, symbol
    /// snapshots (see [`OutputBudget`]).
    out_budget: Arc<OutputBudget>,
    /// Whether `SpawnProcess` is honoured (see [`SpawnPolicy`]).
    spawn_policy: SpawnPolicy,
    /// The ordered `.clew/` state worker's queue (see [`StateJob`]).
    state_jobs: UnboundedSender<StateJob>,
    /// AI provider config to use when the server makes calls (endpoint = Server).
    ai_chat: Option<llm::Config>,
    ai_embed: Option<embed::Config>,
    /// Stop flags for in-flight cancellable work — agent turns and streamed
    /// chats by their stream id, one-shot `Chat` completions and `Embed`
    /// requests by their request id. One map, because every one of those ids
    /// comes from the client's single request counter, so `Cancel` can
    /// address any of them without knowing which kind it is. The tasks
    /// remove themselves when done.
    agents: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// Client-granted approvals for repo-specified LSP commands (see
    /// [`lsp_command_allowed`]). Replaced by `LspApprovals`, cleared on
    /// `OpenProject` (approvals are per-project).
    lsp_approvals: SharedApprovals,
    /// Stops the language-server installs (`LspInstall`) started for the open
    /// project: set when the connection leaves the project or closes, and
    /// replaced with a fresh flag for the next project. An install is minutes
    /// of downloading or compiling, and ran on for a project nobody had open.
    install_stop: Arc<AtomicBool>,
    /// Language servers backing the agent's semantic tools. Lazily created for
    /// the open project on the first agent turn; replaced when the root changes.
    agent_lsp: Option<Arc<agent_lsp::LspPool>>,
    /// Publication counter for `ProjectSymbols`. Full snapshots (built off
    /// the request loop) and watcher partials are sent from different
    /// threads; stamping under this lock at send time gives the client a
    /// total order to drop stale events against (see `send_project_symbols`).
    index_seq: Arc<Mutex<u64>>,
    /// Scan counter for `Tree`: each scan takes the next value when it
    /// STARTS (the open's and every watcher rescan alike), so the client can
    /// keep the tree that saw the most. Server-lifetime, never reset.
    tree_seq: Arc<AtomicU64>,
    /// Set by a `Hello` whose protocol version matched; cleared by one that
    /// didn't. While false — before any Hello, or after a failed one — every
    /// non-Hello request is refused: the peer cannot parse half our frames
    /// anyway, and serving the half it can parse turns one clear error into
    /// a session of confusing ones.
    hello_ok: bool,
}

impl Server {
    /// Create a server that emits messages on `out`, as a local client's
    /// server ([`SpawnPolicy::Local`]). Must run inside a tokio runtime (it
    /// spawns the ordered state worker).
    pub fn new(out: UnboundedSender<ServerMessage>) -> Self {
        Self::with_policy(out, SpawnPolicy::Local)
    }

    /// [`Server::new`] with an explicit [`SpawnPolicy`].
    pub fn with_policy(out: UnboundedSender<ServerMessage>, spawn_policy: SpawnPolicy) -> Self {
        let state_jobs = spawn_state_worker(out.clone());
        Server {
            root: None,
            files: Arc::new(Mutex::new(None)),
            out,
            _watcher: Arc::new(Mutex::new(None)),
            open_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            procs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            out_budget: OutputBudget::new(),
            spawn_policy,
            state_jobs,
            ai_chat: None,
            ai_embed: None,
            agents: Arc::new(Mutex::new(HashMap::new())),
            lsp_approvals: Arc::new(Mutex::new(HashMap::new())),
            install_stop: Arc::new(AtomicBool::new(false)),
            agent_lsp: None,
            index_seq: Arc::new(Mutex::new(0)),
            tree_seq: Arc::new(AtomicU64::new(0)),
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
        self.root
            .clone()
            .ok_or_else(|| Box::new(refused("refused: no project open")))
    }

    /// Wait (bounded) for `root`'s file list to commit — an `OpenProject`
    /// scan may still be running when a pipelined request arrives, and that
    /// window used to swallow such requests entirely (no reply, a client
    /// spinner forever). Blocking: call only on a blocking task, never on
    /// the request loop. `None` on timeout or when a NEWER open superseded
    /// `root` mid-scan; the caller replies [`ErrorCode::NotReady`], the one
    /// refusal clients may retry on.
    fn wait_for_files_blocking(files: &SharedFiles, root: &Path) -> Option<Arc<Vec<FileEntry>>> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(p) = files
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                return (p.root == root).then(|| p.files.clone());
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The bulk budget, for the transport writer to credit back what it has
    /// written (see [`OutputBudget::release`]).
    pub fn output_budget(&self) -> Arc<OutputBudget> {
        self.out_budget.clone()
    }

    /// Stop everything this connection started: agent turns, chats and
    /// embeddings (their flags — each closes with its own `AgentDone` /
    /// `ChatStreamDone` / reply), proxied processes (killed, exits
    /// reported), the agent's language servers, the watcher, and any open
    /// still scanning.
    ///
    /// Called when the client's stream ends. A disconnect used to stop
    /// nothing: a turn kept exploring and calling (and billing) the model for
    /// a client that was gone, and its blocking thread then held the process
    /// open on exit.
    pub async fn shutdown(&mut self) {
        for (_, flag) in self
            .agents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
        {
            flag.store(true, Ordering::Relaxed);
        }
        self.install_stop.store(true, Ordering::Relaxed);
        for (proc, p) in self.procs.lock().await.drain() {
            kill_removed(&self.out, proc, p);
        }
        // Closed, not just let go: an agent turn still finishing holds the
        // pool too, and dropping this handle alone would leave its servers
        // running until that turn ends (see `LspPool::close`).
        if let Some(pool) = self.agent_lsp.take() {
            pool.close();
        }
        *self._watcher.lock().unwrap_or_else(PoisonError::into_inner) = None;
        // An open still scanning must not install a watcher or publish after
        // this: bumping the epoch supersedes it.
        self.open_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Send a correlated reply. Used by arms that finish their work on a
    /// spawned task: the request loop must never wait on slow work (LLM
    /// calls, big greps, blame), or a queued `ProcessKill` / `Cancel` would
    /// sit behind it.
    fn reply(out: &UnboundedSender<ServerMessage>, id: RequestId, event: Event) {
        let _ = out.send(ServerMessage::Reply { id, event });
    }

    /// Handle one request, returning the event to reply with (or `None` when a
    /// request has no direct reply).
    pub async fn handle(&mut self, id: RequestId, request: Request) -> Option<Event> {
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
                    return Some(Event::error(
                        ErrorCode::Handshake,
                        format!(
                            "protocol mismatch: client speaks v{protocol}, this clew-server \
                             speaks v{PROTOCOL_VERSION} — update so both sides match"
                        ),
                    ));
                }
                // Same numeric version but a different protocol BUILD (a wire
                // change whose version bump was missed, or a stale dev
                // binary): refuse here, where the mismatch is one clear
                // error, instead of later as frames that silently fail to
                // deserialize.
                if fingerprint != clew_protocol::SCHEMA_FINGERPRINT {
                    self.hello_ok = false;
                    return Some(Event::error(
                        ErrorCode::Handshake,
                        format!(
                            "protocol build mismatch: both sides speak v{PROTOCOL_VERSION} but \
                             were built from different protocol sources (client {fingerprint}, \
                             server {}) — rebuild/redeploy so they match",
                            clew_protocol::SCHEMA_FINGERPRINT
                        ),
                    ));
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
            _ if !self.hello_ok => Some(Event::error(
                ErrorCode::Handshake,
                "refused: the protocol handshake has not completed — send Hello first, with \
                 matching versions on both sides",
            )),
            // Scan the project: store the file list for search/read, and reply
            // with the tree so the client renders it instead of scanning itself.
            // The scan runs off the loop — a large repo takes seconds, and a
            // queued ProcessKill/Cancel must not wait behind it. The task
            // is epoch-guarded so a superseded open commits nothing.
            Request::OpenProject { root } => {
                let root = PathBuf::from(root);
                // Drop the previous project's agent language servers now, not
                // lazily on the next Ask — they can hold gigabytes. Closed,
                // not just let go: an agent turn still running holds the pool
                // too, and would keep them alive until it ends.
                if let Some(pool) = self.agent_lsp.take_if(|pool| pool.root() != root) {
                    pool.close();
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
                let epoch = self.open_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                self.root = Some(root.clone());
                *self.files.lock().unwrap_or_else(PoisonError::into_inner) = None;
                *self._watcher.lock().unwrap_or_else(PoisonError::into_inner) = None;
                for (proc, p) in self.procs.lock().await.drain() {
                    kill_removed(&self.out, proc, p);
                }
                // Agent turns captured the OLD root: left running they keep
                // calling tools against a project the client has left, and
                // keep spending on the model. The client also sends
                // Cancel, but that frame can be lost with the transport it
                // was queued on — this is the authoritative stop, because it
                // happens where the turns actually run.
                {
                    let mut agents = self.agents.lock().unwrap_or_else(PoisonError::into_inner);
                    for flag in agents.values() {
                        flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    agents.clear();
                }
                // Approvals are per-project; the client re-pushes them for
                // the new one after the open completes.
                self.lsp_approvals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
                // Installs started for the old project stop with it (a
                // download between chunks, a toolchain command with all it
                // spawned); the new project starts with a fresh flag.
                self.install_stop.store(true, Ordering::Relaxed);
                self.install_stop = Arc::new(AtomicBool::new(false));
                let open_epoch = self.open_epoch.clone();
                let files_slot = self.files.clone();
                let watcher_slot = self._watcher.clone();
                let index_seq = self.index_seq.clone();
                let tree_seq = self.tree_seq.clone();
                let out = self.out.clone();
                let budget = self.out_budget.clone();
                // Stamped as the scan starts (see `Event::Tree::seq`): a
                // watcher rescan that starts after this one outranks it.
                let seq = tree_seq.fetch_add(1, Ordering::SeqCst) + 1;
                // Every way out of the task below answers `id` exactly once:
                // the `Tree` (sent from inside the commit, behind the live
                // watch), a failure, or `Cancelled` when a newer open
                // superseded this one — which answers for itself. An open used
                // to end in silence on two of those paths, and a client
                // waiting on its reply waited forever.
                let superseded = || {
                    Event::error(
                        ErrorCode::Cancelled,
                        "superseded: another OpenProject arrived before this one finished",
                    )
                };
                tokio::spawn(async move {
                    let current = || open_epoch.load(Ordering::SeqCst) == epoch;
                    let scan_root = root.clone();
                    let (scan, report) = match tokio::task::spawn_blocking(move || {
                        #[cfg(test)]
                        open_faults::hit(&scan_root, open_faults::Stage::Scan);
                        clew_core::fs_scan::scan_with_report(scan_root)
                    })
                    .await
                    {
                        Ok(scanned) => scanned,
                        // The scan panicked.
                        Err(_) => {
                            let event = if current() {
                                failed(format!("scanning {} failed", root.display()))
                            } else {
                                superseded()
                            };
                            Self::reply(&out, id, event);
                            return;
                        }
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
                    let tracked_ignored = report.tracked_ignored.clone();
                    let watch_root = root.clone();
                    let watch_out = out.clone();
                    let watch_files = files_slot.clone();
                    let watch_seq = index_seq.clone();
                    let watch_tree_seq = tree_seq.clone();
                    let watch_budget = budget.clone();
                    let reply_budget = budget.clone();
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
                                send_bulk(
                                    &reply_out,
                                    &reply_budget,
                                    ServerMessage::Reply {
                                        id,
                                        event: Event::Tree {
                                            root: reply_root,
                                            seq,
                                            tree,
                                            files: rels,
                                            truncated,
                                            tracked_ignored,
                                        },
                                    },
                                );
                            },
                            // Watch the project; changes stream back as
                            // notifications, and the watcher refreshes the
                            // shared file list so search/docs/agent turns see
                            // the current set. A project that cannot be
                            // watched still opens, and the client is told.
                            || {
                                #[cfg(test)]
                                open_faults::hit(&watch_root, open_faults::Stage::Commit);
                                watch_or_report(
                                    watch_root,
                                    watch_out,
                                    watch_files,
                                    watch_seq,
                                    watch_tree_seq,
                                    watch_budget,
                                )
                            },
                        )
                    })
                    .await;
                    match committed {
                        // The Tree went out, behind the live watch. What the
                        // scan left out or added follows it (see
                        // `send_scan_report`), unless a newer open has
                        // already taken over.
                        Ok(OpenCommit::Replied) => {
                            if current() {
                                send_scan_report(&out, &report);
                            }
                        }
                        // A newer open landed first — before the commit, or
                        // during the watcher's walk (its files then went with
                        // the newer open's reset). Nothing of this one stands.
                        Ok(OpenCommit::Superseded | OpenCommit::CommittedThenSuperseded) => {
                            Self::reply(&out, id, superseded());
                            return;
                        }
                        // The commit, or the watcher it built, panicked: no
                        // Tree went out (the reply is its last step).
                        Err(_) => {
                            let event = if current() {
                                failed(format!("opening {} failed", root.display()))
                            } else {
                                superseded()
                            };
                            Self::reply(&out, id, event);
                            return;
                        }
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
                    let publish_budget = budget.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        // Reading and publishing happen together under the
                        // publication lock: a watcher partial that lands
                        // mid-build is FRESHER than this snapshot, and only
                        // the lock keeps its `seq` above ours (see
                        // `publish_project_symbols`).
                        publish_project_symbols(
                            &publish_out,
                            &publish_budget,
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
                                    .unwrap_or_else(PoisonError::into_inner)
                                    .as_ref()
                                    .filter(|p| p.root == snap_root)
                                    .map_or(files_arc.clone(), |p| p.files.clone());
                                let payload = full_symbol_payload(&snap_root, &files_arc);
                                if publish_epoch.load(Ordering::SeqCst) != epoch {
                                    return None;
                                }
                                Some(payload)
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
                // Confined to the project. The shape check is free and
                // refuses at once; resolving symlinks is filesystem I/O and
                // runs with the read, off the request loop (a slow mount must
                // not stall it) — see `files::read_file_event`.
                if let Err(e) = clew_core::confine::check_lexical(&rel) {
                    return Some(files::confine_refusal(&rel, &e));
                }
                let target: inactive::Target = target.into();
                spawn_reply(&self.out, id, "reading the file", move || {
                    files::read_file_event(&root, rel, &target)
                });
                None
            }
            // Per-file git blame + change status for the gutter. Confined to the
            // project like ReadFile; `None` when there is nothing to show (the
            // file is untracked, the project is not a repository); a git that
            // could not answer is an error with its reason, never a gutter that
            // silently stays empty. Blame shells out to git and can be slow on
            // a big history — off the loop.
            Request::GitInfo { rel } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Err(e) = clew_core::confine::check_lexical(&rel) {
                    return Some(files::confine_refusal(&rel, &e));
                }
                spawn_reply(&self.out, id, "reading git blame", move || {
                    match clew_core::confine::confine(&root, &rel) {
                        Ok(abs) => match git::try_info(&root, &abs) {
                            Ok(info) => Event::GitInfo { rel, info },
                            Err(e) => failed(format!("git blame of {rel}: {e}")),
                        },
                        Err(e) => files::confine_refusal(&rel, &e),
                    }
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
                };
                spawn_reply(&self.out, id, "the search", move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return not_ready();
                    };
                    // Every read confined to the root, which is a required
                    // argument here rather than an optional field.
                    let report = search::search_report(&root, files, opts);
                    let hits = report
                        .hits
                        .into_iter()
                        .map(|h| clew_protocol::SearchHit {
                            rel: h.rel,
                            line: h.line,
                            preview: h.preview,
                        })
                        .collect();
                    // The files it could not read go back with the hits: a
                    // search that covered less than the project must say so.
                    let skipped_total = report.skipped.len();
                    let mut skipped = report.skipped;
                    skipped.truncate(MAX_SKIPPED_LISTED);
                    Event::SearchResults {
                        hits,
                        error: report.error,
                        skipped,
                        skipped_total,
                    }
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
                let files_slot = self.files.clone();
                spawn_reply(&self.out, id, "computing statistics", move || {
                    // Counted over the scan the server already holds — the
                    // same files, ignore rules and confinement as the tree —
                    // instead of walking the project a second time.
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return not_ready();
                    };
                    Event::Stats {
                        root: root.to_string_lossy().into_owned(),
                        report: clew_core::stats::compute_files(&root, &files),
                    }
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
                spawn_reply(&self.out, id, "building the call graph", move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return not_ready();
                    };
                    Event::ProjectCalls {
                        root: root.to_string_lossy().into_owned(),
                        graph: build_project_calls_graph(&root, &files, &scope),
                    }
                });
                None
            }
            // A batch of plain sources for the client's Explain pass —
            // confined, per-file capped, batch capped, and paged by bytes
            // (see `read_sources`). Off the loop.
            Request::ReadSources { rels } => {
                const MAX_BATCH: usize = 1000;
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if rels.len() > MAX_BATCH {
                    return Some(refused(format!(
                        "refused: ReadSources batch over {MAX_BATCH} files"
                    )));
                }
                spawn_reply(&self.out, id, "reading sources", move || {
                    read_sources(&root, rels, MAX_SOURCES_REPLY_BYTES)
                });
                None
            }
            // One git operation against the project's repository, run where
            // it lives (Time Travel, blame-why, review, the diff gutter).
            // Arguments are validated BEFORE any subprocess: rels confined,
            // shas hex-only, refs shaped like refs (never leading '-', which
            // git would read as an option). Off the loop; replies itself —
            // with the `GitResult`, or, when git could not answer (timed out,
            // not a repository, an object it could not read), with an error
            // naming why: an empty answer there would read as "no history".
            Request::Git { op } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Err(message) = validate_git_op(&op) {
                    return Some(refused(message));
                }
                spawn_reply(
                    &self.out,
                    id,
                    "the git operation",
                    move || match git::run_op(&root, op) {
                        Ok(result) => Event::GitResult {
                            root: root.to_string_lossy().into_owned(),
                            result,
                        },
                        Err(e) => failed(e.to_string()),
                    },
                );
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
                    return Some(refused(format!("refused: bad state path: {rel}")));
                }
                if self
                    .state_jobs
                    .send(StateJob {
                        root,
                        rel: rel.clone(),
                        id,
                        work: StateWork::Read,
                    })
                    .is_err()
                {
                    return Some(failed(format!("the state worker is gone: {rel}")));
                }
                None
            }
            // Write (or delete, with `text: None`) one project state file —
            // atomic, size-capped, never through a symlinked `.clew`
            // (statefile enforces all three), and never over a file this
            // build cannot read or parse (`state::check_replaceable`). Both
            // outcomes are answered: `StateWritten`, or an error the client
            // surfaces. Ordered: two rapid writes of the same file apply in
            // request order.
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
                    return Some(refused(format!("refused: bad state path: {rel}")));
                }
                if text
                    .as_ref()
                    .is_some_and(|t| t.len() as u64 > clew_core::statefile::MAX_STATE_BYTES)
                {
                    return Some(refused(format!("refused: state file too large: {rel}")));
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
                    return Some(failed(format!("the state worker is gone: {rel}")));
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
                edit_id,
            } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                if let Some(refusal) = wrong_project(&root, &want, &rel) {
                    return Some(refusal);
                }
                if !clew_core::statefile::safe_rel(&rel) {
                    return Some(refused(format!("refused: bad state path: {rel}")));
                }
                // Recorded on disk once applied (see `StateWork::Merge`).
                if !clew_protocol::valid_edit_id(&edit_id) {
                    return Some(refused(format!("refused: bad edit id for {rel}")));
                }
                // The merged file is bounded by the file it merges into, which
                // the read caps; only the incoming entry is unbounded here, so
                // that is what is checked. A merge that would push the file
                // past the cap makes the NEXT read refuse it, which is the
                // same outcome an oversized wholesale write has.
                let edit_bytes = serde_json::to_string(&merge).map(|s| s.len() as u64);
                if !matches!(edit_bytes, Ok(n) if n <= clew_core::statefile::MAX_STATE_BYTES) {
                    return Some(refused(format!("refused: state edit too large: {rel}")));
                }
                if self
                    .state_jobs
                    .send(StateJob {
                        root,
                        rel: rel.clone(),
                        id,
                        work: StateWork::Merge { merge, edit_id },
                    })
                    .is_err()
                {
                    return Some(failed(format!("the state worker is gone: {rel}")));
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
                if self.spawn_policy == SpawnPolicy::Remote {
                    // End the stream the client may already be feeding, as
                    // every failed spawn does.
                    let _ = self.out.send(ServerMessage::Notification {
                        event: Event::ProcessExited { proc, code: None },
                    });
                    return Some(refused(
                        "refused: a remote clew-server does not run arbitrary commands \
                         (SpawnProcess) — language servers and debug adapters start through \
                         SpawnLsp / SpawnAdapter",
                    ));
                }
                let cwd =
                    cwd.or_else(|| self.root.as_ref().map(|r| r.to_string_lossy().into_owned()));
                let (input_rx, generation) = register_proc(&self.procs, proc).await;
                match spawn_registered(
                    &self.out,
                    &self.procs,
                    self.out_budget.clone(),
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
                let budget = self.out_budget.clone();
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
                                    failed("the debug adapter was stopped while starting"),
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
                                    event: Event::ProcessExited { proc, code: None },
                                });
                            }
                            // Unconditional: it answers THIS request's id.
                            Self::reply(&out, id, failed(message));
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
                let budget = self.out_budget.clone();
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
                        lsp_gate::resolve_spawn_exe(&approvals, &gate_root, &gate_lang)
                    })
                    .await
                    .unwrap_or_else(|_| {
                        Err((
                            ErrorCode::Failed,
                            format!("resolving the {language} server failed"),
                        ))
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
                        Err((code, message)) => {
                            // Nothing will ever run: retract the queue and
                            // end the proxy so the client's LSP driver sees EOF.
                            // Guarded by the generation for the same reason as
                            // the adapter path above — this resolve is
                            // detached, and it hashes the executable's bytes,
                            // so the window is not a short one.
                            if !superseded(&procs, proc, generation).await {
                                let _ = out.send(ServerMessage::Notification {
                                    event: Event::ProcessExited { proc, code: None },
                                });
                            }
                            Self::reply(&out, id, Event::error(code, message));
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
                        lsp_gate::resolve_lsp(&o, &approvals, &r, &lang)
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
            //
            // The consent is for the install the user was SHOWN, whose digest
            // the request carries back; `install_lsp` runs nothing else. It
            // stops when the connection leaves the project.
            Request::LspInstall { language, consent } => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let out = self.out.clone();
                let approvals = self.lsp_approvals.clone();
                let stop = self.install_stop.clone();
                tokio::spawn(async move {
                    let (lang, r, o) = (language.clone(), root.clone(), out.clone());
                    let outcome = tokio::task::spawn_blocking(move || {
                        lsp_gate::install_lsp(&o, &approvals, &r, &lang, &consent, &stop)
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
                *self
                    .lsp_approvals
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = approvals.into_iter().collect();
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
                // One message is at most one protocol chunk. The queue bounds
                // the COUNT (256), so without a per-message cap a peer could
                // park 256 frame-sized messages in it. An over-cap chunk ends
                // the process as an overflow does, and for the same reason:
                // the child never sees those bytes, so its stream is desynced
                // for good, and one left running would answer garbage.
                if data.len() > MAX_PROC_INPUT_BYTES {
                    if let Some(p) = self.procs.lock().await.remove(&proc) {
                        kill_removed(&self.out, proc, p);
                    }
                    return Some(refused(format!(
                        "refused: {} bytes of input for process {proc} (limit \
                         {MAX_PROC_INPUT_BYTES}); the process was stopped",
                        data.len()
                    )));
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
                            if let Some(p) = procs.remove(&proc) {
                                kill_removed(&self.out, proc, p);
                            }
                            Some(failed(format!(
                                "process {proc} stopped reading stdin (queue overflow); killed"
                            )))
                        }
                    },
                }
            }
            Request::ProcessKill { proc } => {
                let removed = self.procs.lock().await.remove(&proc);
                if let Some(p) = removed {
                    kill_removed(&self.out, proc, p);
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
            // the reactor). The whole response comes back in one reply; a
            // truncated answer says so at its end.
            //
            // Stoppable: registered under the REQUEST id, so `Cancel { id }`
            // (or a project switch) reaches it — the caller is answered at once
            // (`ErrorCode::Cancelled`) instead of when the whole generation
            // finished. For an https provider the request's connection is shut
            // down then too, which ends the generation and its billing; a
            // plain-http endpoint's socket cannot be reached (`clew_core::llm`,
            // `Line`), and that abandoned request ends when its response has
            // been read, or at the call's overall deadline, which bounds every
            // model call anyway.
            Request::Chat {
                system,
                messages,
                max_tokens,
            } => {
                let Some(cfg) = self.ai_chat.clone() else {
                    return Some(refused("no AI chat config on the server"));
                };
                let msgs = chat_msgs(messages);
                let flag = Arc::new(AtomicBool::new(false));
                self.agents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(id, flag.clone());
                let agents = self.agents.clone();
                let out = self.out.clone();
                tokio::spawn(async move {
                    let stop = flag.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        llm::complete_chat_full(&cfg, &system, &msgs, max_tokens, &|| {
                            stop.load(Ordering::Relaxed)
                        })
                    })
                    .await;
                    agents
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    let event = match result {
                        Ok(Ok(done)) => Event::ChatResult {
                            text: done.into_text_with_note(),
                        },
                        // What failed, typed — or, stopped by a `Cancel` for
                        // this id, what the client asked for, which is not a
                        // failure to report. The words are for people.
                        Ok(Err(e)) => Event::error(explain::chat_error_code(&e), e.to_string()),
                        // The server's own failure, not the provider's.
                        Err(_) => failed("the chat request failed unexpectedly"),
                    };
                    Self::reply(&out, id, event);
                });
                None
            }
            // Like Chat, but streamed: each token goes back as a `ChatDelta`
            // notification, then a `ChatStreamDone` saying how it ended. There
            // is no direct reply.
            Request::ChatStream {
                stream,
                system,
                messages,
                max_tokens,
            } => {
                let done = move |out: &UnboundedSender<ServerMessage>, outcome: StreamOutcome| {
                    let _ = out.send(ServerMessage::Notification {
                        event: Event::ChatStreamDone { stream, outcome },
                    });
                };
                let Some(cfg) = self.ai_chat.clone() else {
                    done(
                        &self.out,
                        StreamOutcome::Failed("no AI chat config on the server".into()),
                    );
                    return None;
                };
                let msgs = chat_msgs(messages);
                let out = self.out.clone();
                let budget = self.out_budget.clone();
                // Registered so the client can stop it: an abandoned answer
                // (project switch, Ask Clear) otherwise ran to completion on
                // the provider's meter with nobody listening.
                let flag = Arc::new(AtomicBool::new(false));
                self.agents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(stream, flag.clone());
                let agents = self.agents.clone();
                tokio::spawn(async move {
                    let sink = out.clone();
                    let stop = flag.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let mut on_delta = |delta: &str| {
                            let msg = ServerMessage::Notification {
                                event: Event::ChatDelta {
                                    stream,
                                    text: delta.to_string(),
                                },
                            };
                            // Charged against the transport's budget: a client
                            // that stopped reading holds this stream up rather
                            // than the queue growing — but a Stop still gets
                            // through while it waits. Nobody left to deliver
                            // to, or stopped: stop generating.
                            let stopped = || stop.load(Ordering::Relaxed);
                            if !transport::send_bulk_unless(&sink, &budget, msg, &stopped) {
                                stop.store(true, Ordering::Relaxed);
                            }
                        };
                        let result = llm::complete_chat_stream_full(
                            &cfg,
                            &system,
                            &msgs,
                            max_tokens,
                            &mut on_delta,
                            &|| stop.load(Ordering::Relaxed),
                        );
                        // An answer cut off at the output limit says so in its
                        // own text, as it does on every other path.
                        if result.as_ref().is_ok_and(|done| done.truncated) {
                            on_delta(llm::TRUNCATED_NOTE);
                        }
                        result
                    })
                    .await;
                    agents
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&stream);
                    // Always closed, a panicking stream included — and a Stop
                    // closes as `Stopped`, which is not a failure.
                    done(
                        &out,
                        match result {
                            Ok(Ok(_)) => StreamOutcome::Done,
                            Ok(Err(llm::LlmError::Cancelled)) => StreamOutcome::Stopped,
                            Ok(Err(e)) => StreamOutcome::Failed(e.to_string()),
                            Err(_) => {
                                StreamOutcome::Failed("the chat stream failed unexpectedly".into())
                            }
                        },
                    );
                });
                None
            }
            // Embed texts with the server's embedding config. Cancellable by
            // its request id, as a `Chat` is: an index build the client gave
            // up — its window left the project, or closed — stops embedding,
            // and billing, with the batch in flight, instead of running
            // through every batch it has left for nobody. A project switch or
            // a disconnect stops it the same way (`OpenProject`, `shutdown`).
            Request::Embed { texts } => {
                let Some(cfg) = self.ai_embed.clone() else {
                    return Some(refused("no embedding config on the server"));
                };
                let flag = Arc::new(AtomicBool::new(false));
                self.agents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(id, flag.clone());
                let agents = self.agents.clone();
                let out = self.out.clone();
                // Same as Chat: HTTP round-trips off the loop, task replies.
                tokio::spawn(async move {
                    let stop = flag.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        embed::embed_all(&cfg, &texts, &|| stop.load(Ordering::Relaxed))
                    })
                    .await;
                    agents
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    let event = match result {
                        Ok(Ok(vecs)) => Event::Embeddings { vecs },
                        // Stopped by a `Cancel` for this id (or a project
                        // switch): what the client asked for, not a failure
                        // to report.
                        Ok(Err(e)) if e == llm::CANCELLED => {
                            Event::error(ErrorCode::Cancelled, llm::CANCELLED)
                        }
                        Ok(Err(e)) => failed(e),
                        Err(_) => failed("the embedding request failed unexpectedly"),
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
                        event: Event::AgentDone {
                            stream,
                            outcome: StreamOutcome::Failed(msg.into()),
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
                self.agents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(stream, flag.clone());
                let agents = self.agents.clone();
                let out = self.out.clone();
                let budget = self.out_budget.clone();
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
                tokio::spawn(async move {
                    let turn_out = out.clone();
                    let turn = tokio::task::spawn_blocking(move || {
                        // The OpenProject scan may still be committing; wait
                        // for it (bounded) rather than failing a user-visible
                        // turn.
                        let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                            let _ = turn_out.send(ServerMessage::Notification {
                                event: Event::AgentDone {
                                    stream,
                                    outcome: StreamOutcome::Failed(NOT_READY.into()),
                                },
                            });
                            return;
                        };
                        agent::run(
                            root, files, chat, embed_cfg, lsp, rt, stream, question, history,
                            context, &turn_out, &budget, &flag,
                        );
                    })
                    .await;
                    agents
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&stream);
                    // A turn that panicked never sent its `AgentDone`; without
                    // one the Ask panel spins forever.
                    if turn.is_err() {
                        let _ = out.send(ServerMessage::Notification {
                            event: Event::AgentDone {
                                stream,
                                outcome: StreamOutcome::Failed(
                                    "the agent turn failed unexpectedly".into(),
                                ),
                            },
                        });
                    }
                });
                None
            }
            // Stop cancellable work: a `Chat` or an `Embed` by its request
            // id, a streamed chat or an agent turn by its stream id — one
            // number space, the client's request counter, so no hint of the
            // kind is needed. Unknown ids are a no-op: the work has already
            // finished.
            //
            // The flag is the work's only stop signal. An agent turn's own
            // bookkeeping (between steps, before each tool runs) tests it
            // directly. Its MODEL calls — streamed, or the blocking POST an
            // endpoint that refuses `stream: true` falls back to — each run on
            // a worker whose caller polls the flag, so the turn lets go within
            // moments of Stop, and the worker's connection is shut down then
            // too: for an https provider that is what ends the generation
            // (and its billing) instead of the provider's next byte, or never
            // (`clew_core::llm`, "cancellable connections"). Embeddings
            // requests — `Embed`'s batches, and the query `semantic_find`
            // embeds — run on such a worker too (`embed_batch_cancellable`).
            // The work always closes with its own terminal message
            // (`AgentDone` or `ChatStreamDone`, both `Stopped`, or the `Chat`
            // or `Embed` reply, `ErrorCode::Cancelled`).
            Request::Cancel { id: work } => {
                if let Some(flag) = self
                    .agents
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&work)
                {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                None
            }
            // Off the request loop, and answered within LIST_DIR_TIMEOUT even
            // when the directory sits on a mount that hangs.
            Request::ListDir { path } => {
                let out = self.out.clone();
                tokio::spawn(async move {
                    let listing = tokio::time::timeout(
                        LIST_DIR_TIMEOUT,
                        tokio::task::spawn_blocking(move || list_dir(path)),
                    )
                    .await;
                    let event = match listing {
                        Ok(Ok(event)) => event,
                        Ok(Err(_)) => failed("listing the directory failed unexpectedly"),
                        Err(_) => failed(format!(
                            "listing the directory took longer than {}s",
                            LIST_DIR_TIMEOUT.as_secs()
                        )),
                    };
                    Self::reply(&out, id, event);
                });
                None
            }
            // Build off the request loop — `handle` takes `&mut self`, so
            // awaiting the build here would stall every other request (file
            // opens, hover) behind it, and on a big repo that is many seconds.
            // Answered like every other request: the index, or why not.
            Request::BuildDocs => {
                let root = match self.root_or_refuse() {
                    Ok(root) => root,
                    Err(refusal) => return Some(*refusal),
                };
                let files_slot = self.files.clone();
                spawn_reply(&self.out, id, "building the API docs", move || {
                    let Some(files) = Self::wait_for_files_blocking(&files_slot, &root) else {
                        return not_ready();
                    };
                    Event::Docs {
                        files: build_docs(&root, &files),
                        root: root.to_string_lossy().into_owned(),
                    }
                });
                None
            }
        }
    }
}

/// Run `work` on a blocking thread and reply to `id` with what it returns — or,
/// if it panics, with an error naming `what` failed.
///
/// Every request gets an answer. A panic inside a detached `spawn_blocking`
/// used to vanish with its dropped `JoinHandle`, leaving the client waiting
/// on a reply that could never come (and its request entry leaked).
fn spawn_reply(
    out: &UnboundedSender<ServerMessage>,
    id: RequestId,
    what: &'static str,
    work: impl FnOnce() -> Event + Send + 'static,
) {
    let out = out.clone();
    tokio::spawn(async move {
        let event = tokio::task::spawn_blocking(work)
            .await
            .unwrap_or_else(|_| failed(format!("{what} failed unexpectedly")));
        Server::reply(&out, id, event);
    });
}

/// The retryable refusal for a request that arrived before the project's
/// scan committed (see [`Server::wait_for_files_blocking`]).
fn not_ready() -> Event {
    Event::error(ErrorCode::NotReady, NOT_READY)
}

/// The client's chat turns as the LLM client's messages.
fn chat_msgs(messages: Vec<clew_protocol::AiChatMsg>) -> Vec<llm::ChatMsg> {
    messages
        .into_iter()
        .map(|m| {
            if m.role == "assistant" {
                llm::ChatMsg::assistant(m.content)
            } else {
                llm::ChatMsg::user(m.content)
            }
        })
        .collect()
}

/// Most bytes of rels and text one `Sources` reply carries, as the wire
/// encodes them. The per-file and per-batch caps multiply to ~500 MB of raw
/// text — past the protocol's 256 MB frame limit, so a batch read whole
/// would be a frame the client is required to hang up on, after both sides
/// allocated all of it. What does not fit is paged ([`read_sources`]).
const MAX_SOURCES_REPLY_BYTES: usize = 48 * 1024 * 1024;

/// Read a `ReadSources` batch into its `Sources` reply, in order, with at
/// most `budget` bytes of rels and text in it as the wire encodes them.
///
/// A rel that does not exist is `missing`; a file over the per-file cap, or
/// whose text alone is more than `budget`, is `too_large`, with its size; a
/// file that is not a plain text file of the project is `refused`, with why;
/// a file that is there and could not be read — this user may not, the read
/// failed, the project root is not there — is `unreadable`, with the error:
/// in no list, a client took it for one not answered for, and asked for it
/// again forever. One that changed while it was read is in no list (see
/// `Event::Sources`): it is read next time. The first file the reply has no room
/// left for is `deferred`, with every rel after it, unread: the client asks
/// for them again. Only a file that fits an empty reply is ever deferred, so
/// a reply always settles the first rel of its batch — asking again always
/// gets further, and a file no reply could carry is said to be too large
/// once, never deferred forever.
fn read_sources(root: &Path, rels: Vec<clew_protocol::Rel>, budget: usize) -> Event {
    use clew_core::confine::ConfineError;
    use clew_core::explain::{Unexplainable, read_steadily};
    use clew_protocol::Refusal;
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let mut files = Vec::new();
    let mut missing = Vec::new();
    let mut too_large = Vec::new();
    let mut refused = Vec::new();
    let mut unreadable = Vec::new();
    let mut deferred = Vec::new();
    let mut room = budget;
    let mut rels = rels.into_iter();
    while let Some(rel) = rels.next() {
        // Confined like every client path — this batch feeds a model prompt,
        // so a symlink out of the project would send another file's contents
        // to the provider.
        let abs = match clew_core::confine::confine(root, &rel) {
            Ok(abs) => abs,
            Err(ConfineError::Unresolvable(e)) if not_found(&e) => {
                #[cfg(test)]
                sources_faults::hit(&root.join(&rel), sources_faults::Stage::Unresolved);
                // Missing only when nothing is at the path: a dangling link
                // does not resolve either, and it is there — a link, not a
                // file of the project. Anything else found there now came
                // since it did not resolve — a save that unlinked the file
                // and wrote it again, a checkout — and is read next time.
                match std::fs::symlink_metadata(root.join(&rel)) {
                    Err(e) if not_found(&e) => missing.push(rel),
                    Ok(meta) if meta.file_type().is_symlink() => {
                        refused.push((rel, Refusal::NotPlainFile));
                    }
                    Ok(_) | Err(_) => {}
                }
                continue;
            }
            // No file of the project by its very name, or one reached through
            // a link out of it.
            Err(
                ConfineError::Empty
                | ConfineError::Absolute
                | ConfineError::Traversal
                | ConfineError::Escapes,
            ) => {
                refused.push((rel, Refusal::OutsideProject));
                continue;
            }
            // The root or the path could not be looked at.
            Err(e @ (ConfineError::Root(_) | ConfineError::Unresolvable(_))) => {
                unreadable.push((rel, e.to_string()));
                continue;
            }
        };
        // One open, then the type check, the size and the bytes all from
        // that handle — a path resolved twice can be a regular file the first
        // time and a FIFO the second. A read inside a save says nothing of
        // the file: it is read again until the file stands still. One that
        // never does — rewritten all the time — is sent as it was last read,
        // and is never said to be gone or not to be explained: the client
        // reads it again next time.
        let (read, steady) = read_steadily(&abs, || {
            let read = clew_core::statefile::read_capped_checked(&abs, MAX_INDEX_FILE_BYTES);
            #[cfg(test)]
            sources_faults::hit(&abs, sources_faults::Stage::Read);
            read
        });
        let text = match read {
            Ok(Some(text)) => text,
            // Gone since it resolved.
            Ok(None) if steady => {
                missing.push(rel);
                continue;
            }
            Err(e) if steady => {
                match Unexplainable::of_read(&e) {
                    Some(Unexplainable::TooLarge(size)) => too_large.push((rel, size)),
                    Some(Unexplainable::Refused(why)) => refused.push((rel, why)),
                    // There, standing still, and not readable: this user may
                    // not, or the read failed.
                    None => unreadable.push((rel, e.to_string())),
                }
                continue;
            }
            // It changed while it was read: read next time.
            Ok(None) | Err(_) => continue,
        };
        let cost = wire_len(&rel) + wire_len(&text);
        if cost > budget {
            too_large.push((rel, text.len() as u64));
        } else if cost > room {
            deferred.push(rel);
            deferred.extend(rels.by_ref());
            break;
        } else {
            room -= cost;
            files.push((rel, text));
        }
    }
    Event::Sources {
        root: root.to_string_lossy().into_owned(),
        files,
        missing,
        too_large,
        refused,
        unreadable,
        deferred,
    }
}

/// How many bytes `s` takes on the wire as a JSON string: its quotes, and
/// each byte the encoder escapes at its escaped length. A control character
/// takes six (`\u0001`), so a text's own length can be a sixth of what it
/// sends — and a budget kept on that length let a reply past the frame
/// limit.
fn wire_len(s: &str) -> usize {
    let escaped: usize = s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum();
    escaped + 2
}

#[cfg(test)]
mod test_support;

/// Changes a unit test makes to a file while `ReadSources` reads it, at a
/// `Stage` of the read. Keyed by path, so tests running in parallel never
/// trip each other's.
#[cfg(test)]
pub(crate) mod sources_faults {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Where in the read of one rel.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Stage {
        /// Its path did not resolve: nothing was found there.
        Unresolved,
        /// Its bytes were read, and the read is not yet looked back on.
        Read,
    }

    static ARMED: Mutex<Vec<(PathBuf, Stage, Vec<u8>)>> = Mutex::new(Vec::new());

    /// Write `bytes` to `path` once a read of it next reaches `stage`.
    pub(crate) fn arm(path: &Path, stage: Stage, bytes: &[u8]) {
        ARMED.lock().unwrap_or_else(|e| e.into_inner()).push((
            path.to_path_buf(),
            stage,
            bytes.to_vec(),
        ));
    }

    /// Make the change armed for `path` at `stage`, once (the lock is
    /// released first).
    pub(crate) fn hit(path: &Path, stage: Stage) {
        let armed = {
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            let at = armed.iter().position(|(p, s, _)| p == path && *s == stage);
            at.map(|i| armed.remove(i))
        };
        if let Some((path, _, bytes)) = armed {
            std::fs::write(&path, bytes).expect("the change lands");
        }
    }
}

/// Faults a unit test injects into an `OpenProject`'s scan or commit, to check
/// the open is still answered when either panics. Keyed by root, so tests
/// running in parallel never trip each other's.
#[cfg(test)]
pub(crate) mod open_faults {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Stage {
        Scan,
        Commit,
    }

    static ARMED: Mutex<Vec<(PathBuf, Stage)>> = Mutex::new(Vec::new());

    /// Make `stage` panic for opens of `root`.
    pub(crate) fn arm(root: &Path, stage: Stage) {
        ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((root.to_path_buf(), stage));
    }

    /// Panic when `stage` is armed for `root` (the lock is released first).
    pub(crate) fn hit(root: &Path, stage: Stage) {
        let armed = ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(r, s)| r == root && *s == stage);
        if armed {
            panic!("injected {stage:?} fault for {}", root.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SpawnPolicy, spawn_reply};
    use clew_protocol::{ErrorCode, Event, ServerMessage};

    /// The request loop's panic policy, checked in a child process (the point
    /// is to end one): a worker's panic still unwinds into whoever joins it,
    /// which is how every request stays answered, while a panic on the
    /// installing thread ends the process on the spot — before any unwinding,
    /// a `catch_unwind` included — instead of unwinding into a runtime drop
    /// that waits forever.
    #[test]
    fn a_request_loop_panic_ends_the_process_while_a_worker_panic_unwinds() {
        let out =
            crate::test_support::child_output("tests::child_panics_under_the_loop_policy", &[]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("worker panic unwound"),
            "the worker's panic must unwind first: {stdout}"
        );
        assert!(
            !stdout.contains("still running"),
            "the loop's panic must not return: {stdout}"
        );
        assert_eq!(
            out.status.code(),
            Some(LOOP_PANIC_EXIT),
            "the loop's panic must end the process: {:?}",
            out.status
        );
    }

    /// How the child below ends in place of `abort`.
    const LOOP_PANIC_EXIT: i32 = 86;

    #[test]
    #[ignore = "runs in a child process, from a_request_loop_panic_ends_the_process_while_a_worker_panic_unwinds"]
    fn child_panics_under_the_loop_policy() {
        if !crate::test_support::in_child() {
            return;
        }
        super::end_process_on_panic_in_this_thread(|| std::process::exit(LOOP_PANIC_EXIT));
        let worker = std::thread::spawn(|| panic!("a worker's job panicked"));
        assert!(worker.join().is_err());
        println!("worker panic unwound");
        let _ = std::panic::catch_unwind(|| panic!("the request loop panicked"));
        println!("still running");
    }

    /// A language-server install stops when its connection leaves the project
    /// and when the connection closes: `LspInstall` hands the install the
    /// connection's stop flag, which both set. (In a child process: it needs
    /// a data dir, and a `go` of its own on PATH that only waits.)
    #[test]
    fn an_install_stops_when_the_connection_leaves_the_project_or_closes() {
        use crate::test_support::{Scratch, fake_slow_go, run_in_child};
        let data = Scratch::new("server-install-stop");
        let bin = Scratch::new("server-slow-go");
        fake_slow_go(&bin);
        run_in_child(
            "tests::child_install_stops_on_leave_and_close",
            &[("CLEW_DATA_DIR", &data), ("PATH", &bin)],
        );
    }

    #[test]
    #[ignore = "runs in a child process, from an_install_stops_when_the_connection_leaves_the_project_or_closes"]
    fn child_install_stops_on_leave_and_close() {
        use crate::test_support::{alive, in_child, started_pid};
        use clew_protocol::{LspResolution, Request};
        if !in_child() {
            return;
        }
        let data = std::path::PathBuf::from(std::env::var_os("CLEW_DATA_DIR").unwrap());
        let bin = std::path::PathBuf::from(std::env::var_os("PATH").unwrap());
        let reply = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMessage>, want: u64| {
            let began = std::time::Instant::now();
            loop {
                match rx.try_recv() {
                    Ok(ServerMessage::Reply { id, event }) if id == want => return event,
                    Ok(_) => {}
                    Err(_) => {
                        assert!(
                            began.elapsed() < std::time::Duration::from_secs(10),
                            "no reply to request {want}"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        for leave_by_closing in [false, true] {
            let _ = std::fs::remove_file(bin.join("started"));
            let root = data.join(format!("proj-{leave_by_closing}"));
            std::fs::create_dir_all(&root).unwrap();
            let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut server = rt.block_on(async { super::Server::new(out) });
            let hello = Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
            };
            assert!(matches!(
                rt.block_on(server.handle(0, hello)),
                Some(Event::Ready { .. })
            ));
            let open = |root: &std::path::Path| Request::OpenProject {
                root: root.to_string_lossy().into_owned(),
            };
            rt.block_on(server.handle(1, open(&root)));
            assert!(matches!(reply(&mut rx, 1), Event::Tree { .. }));
            let resolve = Request::LspResolve {
                language: "go".into(),
            };
            rt.block_on(server.handle(2, resolve));
            let consent = match reply(&mut rx, 2) {
                Event::LspResolved {
                    resolution: LspResolution::NeedsInstall { consent, .. },
                    ..
                } => consent,
                other => panic!("expected NeedsInstall, got {other:?}"),
            };
            let install = Request::LspInstall {
                language: "go".into(),
                consent,
            };
            rt.block_on(server.handle(3, install));
            let pid = started_pid(&bin);
            if leave_by_closing {
                rt.block_on(server.shutdown());
            } else {
                let elsewhere = data.join("elsewhere");
                std::fs::create_dir_all(&elsewhere).unwrap();
                rt.block_on(server.handle(4, open(&elsewhere)));
            }
            match reply(&mut rx, 3) {
                Event::LspResolved {
                    resolution: LspResolution::Unsupported { message },
                    ..
                } => assert!(message.contains("cancelled"), "{message}"),
                other => panic!("the install must stop, got {other:?}"),
            }
            assert!(!alive(pid), "its toolchain command must stop with it");
        }
    }

    /// Every `OpenProject` is answered, one whose scan or commit panics
    /// included — with `Failed`, under its id — so a client waiting on it is
    /// never stranded. Neither path had a test.
    #[tokio::test]
    async fn an_open_whose_scan_or_commit_panics_is_answered() {
        use super::open_faults::{Stage, arm};
        use clew_protocol::Request;
        for (stage, says) in [(Stage::Scan, "scanning "), (Stage::Commit, "opening ")] {
            let dir = crate::test_support::Scratch::new(&format!("server-open-{stage:?}"));
            let root = dir.to_path_buf();
            arm(&root, stage);
            let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let mut server = super::Server::new(out);
            let hello = Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
            };
            assert!(matches!(
                server.handle(0, hello).await,
                Some(Event::Ready { .. })
            ));
            let open = Request::OpenProject {
                root: root.to_string_lossy().into_owned(),
            };
            assert!(server.handle(1, open).await.is_none());
            let answer = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    match rx.recv().await.expect("the stream is open") {
                        ServerMessage::Reply { id: 1, event } => return event,
                        _ => continue,
                    }
                }
            })
            .await
            .expect("the open is answered");
            assert!(
                matches!(
                    answer,
                    Event::Error { code: ErrorCode::Failed, ref message } if message.starts_with(says)
                ),
                "{stage:?}: {answer:?}"
            );
        }
    }

    /// A state request the ordered worker can no longer take (its task is
    /// gone) is answered at once, never dropped: the client holds the change
    /// as unsaved until it hears back.
    #[tokio::test]
    async fn a_state_request_the_worker_cannot_take_is_answered() {
        use clew_protocol::{Request, StateEdit, StateMerge};
        let dir = crate::test_support::Scratch::new("server-state-gone");
        let root = dir.to_path_buf();
        let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut server = super::Server::new(out);
        let hello = Request::Hello {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        };
        assert!(server.handle(0, hello).await.is_some());
        server.root = Some(root.clone());
        let (gone, taker) = tokio::sync::mpsc::unbounded_channel();
        drop(taker);
        server.state_jobs = gone;
        let want = root.to_string_lossy().into_owned();
        let requests = [
            Request::ReadState {
                root: want.clone(),
                rel: "notes.json".into(),
            },
            Request::WriteState {
                root: want.clone(),
                rel: "history.json".into(),
                text: Some("{}".into()),
            },
            Request::EditState {
                root: want,
                rel: "bookmarks.json".into(),
                merge: StateMerge {
                    key_fields: vec!["line".into()],
                    key: vec![serde_json::json!(1)],
                    edit: StateEdit::Remove,
                    delete_when_empty: true,
                },
                edit_id: "e1".into(),
            },
        ];
        for (id, request) in (1..).zip(requests) {
            let answer = server.handle(id, request).await;
            assert!(
                matches!(
                    answer,
                    Some(Event::Error { code: ErrorCode::Failed, ref message })
                        if message.contains("the state worker is gone")
                ),
                "{answer:?}"
            );
        }
    }

    /// A streamed answer's deltas are charged against the output budget —
    /// with a full queue ahead (a large snapshot a slow link has not taken)
    /// none of them goes out — and a Stop still ends the stream while it
    /// waits there. The delta callback used to park on the budget without
    /// looking at the stop, so the provider kept generating, and billing,
    /// until the link drained.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_reaches_a_chat_stream_parked_on_the_output_budget() {
        use clew_protocol::{AiChatConfig, AiChatMsg, Request, StreamOutcome};
        use std::io::Write;
        use std::time::Duration;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (streaming_tx, streaming) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut conn, _)) = listener.accept() else {
                return;
            };
            clew_core::testutil::read_http_request(&mut conn);
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                        connection: close\r\n\r\n";
            let _ = conn.write_all(head.as_bytes());
            for _ in 0..200 {
                let event = "data: {\"choices\":[{\"delta\":{\"content\":\"token \"}}]}\n\n";
                if conn
                    .write_all(event.as_bytes())
                    .and_then(|()| conn.flush())
                    .is_err()
                {
                    return;
                }
                let _ = streaming_tx.send(());
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut server = super::Server::new(out);
        let hello = Request::Hello {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        };
        assert!(server.handle(0, hello).await.is_some());
        let config = Request::SetAiConfig {
            chat: Some(AiChatConfig {
                provider: "custom".into(),
                api_key: "k".into(),
                model: "m".into(),
                base_url: base,
            }),
            embed: None,
        };
        assert!(server.handle(1, config).await.is_none());
        let budget = server.output_budget();
        assert!(budget.charge_blocking_unless(crate::OutputBudget::CAP + 1, &|| false));
        let chat = Request::ChatStream {
            stream: 41,
            system: "s".into(),
            messages: vec![AiChatMsg {
                role: "user".into(),
                content: "hi".into(),
            }],
            max_tokens: 64,
        };
        assert!(server.handle(2, chat).await.is_none());
        // The provider is streaming: its tokens have somewhere to wait.
        let first = tokio::task::spawn_blocking(move || {
            streaming.recv_timeout(Duration::from_secs(20)).is_ok()
        });
        assert!(first.await.unwrap(), "the provider was never asked");
        let is_delta = |msg: &ServerMessage| {
            matches!(
                msg,
                ServerMessage::Notification {
                    event: Event::ChatDelta { stream: 41, .. },
                }
            )
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            assert!(!is_delta(&msg), "a delta went out past a full queue");
        }

        assert!(server.handle(3, Request::Cancel { id: 41 }).await.is_none());
        let ended = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match rx.recv().await {
                    Some(ServerMessage::Notification {
                        event:
                            Event::ChatStreamDone {
                                stream: 41,
                                outcome,
                            },
                    }) => return outcome,
                    Some(_) => {}
                    None => panic!("the server's stream ended"),
                }
            }
        })
        .await;
        budget.release(crate::OutputBudget::CAP + 1);
        assert_eq!(
            ended.expect("the stopped stream went on waiting for the queue"),
            StreamOutcome::Stopped
        );
    }

    /// An `Embed` the client cancels — the index build of a project its
    /// window left — stops at the endpoint: the batch in flight gives its
    /// connection up, and the request is answered `Cancelled`. `Embed` could
    /// not be cancelled: the server went on through every batch it had left,
    /// billing for them, for a client that wanted none of them any more.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_embed_lets_its_endpoint_go() {
        use clew_protocol::{AiEmbedConfig, Request};
        use std::time::{Duration, Instant};
        let (base, arrived, closed) = clew_core::testutil::silent_http_endpoint();
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut server = super::Server::new(out);
        let hello = Request::Hello {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        };
        assert!(server.handle(0, hello).await.is_some());
        let config = Request::SetAiConfig {
            chat: None,
            embed: Some(AiEmbedConfig {
                api_key: "k".into(),
                model: "m".into(),
                base_url: base,
            }),
        };
        assert!(server.handle(1, config).await.is_none());
        let embed = Request::Embed {
            texts: vec!["x".into()],
        };
        assert!(server.handle(2, embed).await.is_none());
        let out_there = tokio::task::spawn_blocking(move || {
            arrived.recv_timeout(Duration::from_secs(10)).is_ok()
        });
        assert!(
            out_there.await.unwrap(),
            "the batch never reached the endpoint"
        );

        let cancelled_at = Instant::now();
        assert!(server.handle(3, Request::Cancel { id: 2 }).await.is_none());
        let answer = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rx.recv().await.expect("the server's stream is open") {
                    ServerMessage::Reply { id: 2, event } => return event,
                    _ => continue,
                }
            }
        })
        .await
        .expect("the cancelled Embed went on waiting for the endpoint");
        assert!(
            matches!(
                answer,
                Event::Error {
                    code: ErrorCode::Cancelled,
                    ..
                }
            ),
            "{answer:?}"
        );
        let (gone, at) =
            tokio::task::spawn_blocking(move || closed.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap()
                .expect("the Cancel never reached the batch's connection");
        assert!(gone);
        assert!(
            at.saturating_duration_since(cancelled_at) < Duration::from_secs(2),
            "the connection closed {:?} after the Cancel",
            at.saturating_duration_since(cancelled_at)
        );
    }

    /// The request loop takes a mutex another thread's panic poisoned rather
    /// than panicking on it — a panic on the loop ends the whole server (see
    /// `abort_on_panic_in_this_thread`). Every critical section leaves its map
    /// whole, so the value is still good.
    #[tokio::test]
    async fn a_poisoned_lock_does_not_take_the_request_loop_down() {
        use clew_protocol::Request;
        let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut server = super::Server::new(out);
        let hello = Request::Hello {
            protocol: clew_protocol::PROTOCOL_VERSION,
            fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
        };
        assert!(server.handle(0, hello).await.is_some());
        let agents = server.agents.clone();
        let _ = std::thread::spawn(move || {
            let _held = agents.lock().unwrap();
            panic!("a worker panicked holding the agents");
        })
        .join();
        let approvals = server.lsp_approvals.clone();
        let _ = std::thread::spawn(move || {
            let _held = approvals.lock().unwrap();
            panic!("a worker panicked holding the approvals");
        })
        .join();
        assert!(server.agents.is_poisoned() && server.lsp_approvals.is_poisoned());

        let push = Request::LspApprovals {
            approvals: vec![("rust".into(), "fp".into())],
        };
        assert!(server.handle(1, push).await.is_none());
        assert!(server.handle(2, Request::Cancel { id: 99 }).await.is_none());
        let pushed = server
            .lsp_approvals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get("rust")
            .cloned();
        assert_eq!(pushed.as_deref(), Some("fp"));
    }

    /// A job that panics is still answered — with an error, under its id.
    #[tokio::test]
    async fn a_panicking_job_is_answered_with_an_error() {
        let (out, mut rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_reply(&out, 7, "the test job", || panic!("boom"));
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("an answer")
            .expect("a message");
        match reply {
            ServerMessage::Reply {
                id: 7,
                event:
                    Event::Error {
                        code: ErrorCode::Failed,
                        message,
                    },
            } => assert_eq!(message, "the test job failed unexpectedly"),
            other => panic!("expected the error reply, got {other:?}"),
        }
    }

    #[test]
    fn the_spawn_policy_follows_the_launch() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        const SSH: &[(&str, &str)] = &[("SSH_CONNECTION", "10.0.0.2 51234 10.0.0.1 22")];
        assert_eq!(
            SpawnPolicy::detect(&args(&["clew-server"]), env(&[])),
            SpawnPolicy::Local
        );
        assert_eq!(
            SpawnPolicy::detect(&args(&["clew-server"]), env(SSH)),
            SpawnPolicy::Remote
        );
        assert_eq!(
            SpawnPolicy::detect(
                &args(&["clew-server"]),
                env(&[("SSH_CLIENT", "10.0.0.2 1 22")])
            ),
            SpawnPolicy::Remote
        );
        // An empty variable is not a session.
        assert_eq!(
            SpawnPolicy::detect(&args(&["clew-server"]), env(&[("SSH_CONNECTION", " ")])),
            SpawnPolicy::Local
        );
        // An explicit flag wins over the environment.
        assert_eq!(
            SpawnPolicy::detect(&args(&["clew-server", "--local"]), env(SSH)),
            SpawnPolicy::Local
        );
        assert_eq!(
            SpawnPolicy::detect(&args(&["clew-server", "--remote"]), env(&[])),
            SpawnPolicy::Remote
        );
    }

    /// One `Sources` reply to `rels` under `budget`: the rels it carries
    /// text for, and its `missing`, `too_large` and `deferred`.
    type Page = (Vec<String>, Vec<String>, Vec<(String, u64)>, Vec<String>);

    fn page(root: &std::path::Path, rels: &[String], budget: usize) -> Page {
        match super::read_sources(root, rels.to_vec(), budget) {
            Event::Sources {
                files,
                missing,
                too_large,
                deferred,
                ..
            } => (
                files.into_iter().map(|(rel, _)| rel).collect(),
                missing,
                too_large,
                deferred,
            ),
            other => panic!("expected Sources, got {other:?}"),
        }
    }

    /// A batch past one reply's budget is paged, not cut off. Each reply
    /// carries what fits, in order, and names the rest as deferred — the
    /// files past the budget were left out of every list, and a client took
    /// them for files it could not read, on every pass. Asked for again,
    /// the rest arrives. A file no reply could carry is too large, said
    /// once with its size, and the files after it are still read; so is a
    /// file over the per-file cap.
    #[test]
    fn a_batch_past_the_reply_budget_is_paged_not_cut_off() {
        let scratch = crate::test_support::Scratch::new("sources-paged");
        let root = scratch.canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let text = format!("fn f() {{}}\n{}", "x".repeat(1000));
        for name in ["a", "b", "c", "d"] {
            std::fs::write(root.join(format!("src/{name}.rs")), &text).unwrap();
        }
        let cost = super::wire_len("src/a.rs") + super::wire_len(&text);
        // Room for two files a reply.
        let budget = 2 * cost + cost / 2;
        let huge = "y".repeat(3 * cost);
        std::fs::write(root.join("src/huge.rs"), &huge).unwrap();
        let cap = super::MAX_INDEX_FILE_BYTES as usize + 1;
        std::fs::write(root.join("src/over.rs"), "z".repeat(cap)).unwrap();
        let rels: Vec<String> = [
            "src/huge.rs",
            "src/over.rs",
            "src/a.rs",
            "src/gone.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
        ]
        .map(String::from)
        .to_vec();

        let (files, missing, too_large, deferred) = page(&root, &rels, budget);
        assert_eq!(files, ["src/a.rs", "src/b.rs"]);
        assert_eq!(missing, ["src/gone.rs"]);
        assert_eq!(
            too_large,
            [
                ("src/huge.rs".to_string(), huge.len() as u64),
                ("src/over.rs".to_string(), cap as u64),
            ]
        );
        assert_eq!(deferred, ["src/c.rs", "src/d.rs"]);

        // Asked again until nothing is deferred, every file arrives, and
        // each reply gets further.
        let mut read = files;
        let mut asked = deferred;
        let mut replies = 1;
        while !asked.is_empty() {
            assert!(replies < rels.len(), "the pages never ended: {asked:?}");
            let (files, missing, too_large, deferred) = page(&root, &asked, budget);
            assert!(missing.is_empty() && too_large.is_empty());
            assert!(deferred.len() < asked.len(), "a reply settled nothing");
            read.extend(files);
            asked = deferred;
            replies += 1;
        }
        assert_eq!(read, ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]);
        assert_eq!(replies, 2);
    }

    /// The budget is kept on what the wire carries: a text's length as a
    /// JSON string, every escape included. Kept on the text's own length, a
    /// reply of control characters — six bytes each on the wire — could be
    /// six times its budget, past the frame limit the client hangs up on.
    #[test]
    fn a_reply_is_budgeted_at_its_length_on_the_wire() {
        let every_ascii: String = (0u8..128).map(char::from).collect();
        for s in [
            "",
            "plain",
            "a \"quote\" and a \\ slash",
            "tab\tnew\nline\r\u{8}\u{c}",
            "\u{1}\u{1f}\u{7f}",
            "héllo — 世界",
            every_ascii.as_str(),
        ] {
            let json = serde_json::to_string(s).unwrap();
            assert_eq!(super::wire_len(s), json.len(), "{s:?}");
        }

        let scratch = crate::test_support::Scratch::new("sources-wire");
        let root = scratch.canonicalize().unwrap();
        let controls = "\u{1}".repeat(100);
        std::fs::write(root.join("a.rs"), &controls).unwrap();
        std::fs::write(root.join("b.rs"), &controls).unwrap();
        let rels = ["a.rs", "b.rs"].map(String::from).to_vec();
        // Room for both by their own length, for one on the wire.
        let budget = super::wire_len("a.rs") + super::wire_len(&controls) + 100;
        assert!(budget >= 2 * (super::wire_len("a.rs") + controls.len()));
        let (files, _, _, deferred) = page(&root, &rels, budget);
        assert_eq!(files, ["a.rs"]);
        assert_eq!(deferred, ["b.rs"]);
    }

    /// Which list of one `Sources` reply to `rel` names it: `"files"`,
    /// `"missing"`, `"too_large"`, `"refused"` with why, `"unreadable"` with
    /// the error, or none — a file that changed while it was read, which the
    /// client reads again.
    fn settled_as(root: &std::path::Path, rel: &str) -> String {
        match super::read_sources(root, vec![rel.to_string()], usize::MAX) {
            Event::Sources {
                files,
                missing,
                too_large,
                refused,
                unreadable,
                deferred,
                ..
            } => {
                assert!(deferred.is_empty(), "{deferred:?}");
                if !files.is_empty() {
                    "files".into()
                } else if !missing.is_empty() {
                    "missing".into()
                } else if !too_large.is_empty() {
                    "too_large".into()
                } else if let [(_, why)] = refused[..] {
                    format!("refused as {why:?}")
                } else if let [(_, why)] = &unreadable[..] {
                    format!("unreadable: {why}")
                } else {
                    "unread".into()
                }
            }
            other => panic!("expected Sources, got {other:?}"),
        }
    }

    /// A file that is there and cannot be read — this user may not, or its
    /// path cannot be looked up — is named unreadable, with the error. It was
    /// in no list, which a client takes for a file not answered for: asked
    /// again, and again, one unreadable tsconfig kept every alias of the
    /// project from applying.
    #[test]
    fn a_file_that_cannot_be_read_is_named_with_the_error() {
        let scratch = crate::test_support::Scratch::new("sources-unreadable");
        let root = scratch.canonicalize().unwrap();
        // A name no file system takes, in a directory that is there: its
        // lookup fails, whatever the permissions.
        std::fs::create_dir_all(root.join("src")).unwrap();
        let long = format!("src/{}.rs", "n".repeat(300));
        let why = settled_as(&root, &long);
        assert!(why.starts_with("unreadable: cannot resolve path:"), "{why}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let secret = root.join("secret.rs");
            std::fs::write(&secret, "fn secret() {}\n").unwrap();
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Unless permissions are not enforced here — root, or a volume
            // that ignores ownership — the read is refused.
            if std::fs::File::open(&secret).is_err() {
                let why = settled_as(&root, "secret.rs");
                assert!(
                    why.starts_with("unreadable: ") && why.contains("ermission denied"),
                    "{why}"
                );
            }
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(settled_as(&root, "secret.rs"), "files");
        }
    }

    /// A file saved again while its path did not resolve — a save that
    /// unlinks the file and writes it anew, a checkout — is read next time.
    /// Anything found at a path that did not resolve was refused, as a
    /// dangling link is, and the client dropped every summary of the file
    /// as "not a text file".
    #[test]
    fn a_file_written_again_while_it_did_not_resolve_is_read_next_time() {
        use super::sources_faults::{Stage, arm};
        let scratch = crate::test_support::Scratch::new("sources-rewritten");
        let root = scratch.canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        arm(
            &root.join("src/saved.rs"),
            Stage::Unresolved,
            b"fn saved() {}\n",
        );
        assert_eq!(settled_as(&root, "src/saved.rs"), "unread");
        assert_eq!(settled_as(&root, "src/saved.rs"), "files");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("src/nowhere.rs"), root.join("src/dangling.rs"))
                .unwrap();
            assert_eq!(
                settled_as(&root, "src/dangling.rs"),
                "refused as NotPlainFile"
            );
        }
        assert_eq!(settled_as(&root, "src/gone.rs"), "missing");
    }

    /// A file saved while it was read is not taken for what the read found:
    /// it is read again, and sent as it was saved. One that read
    /// mid-character, or past the cap, was refused as not UTF-8, or said to
    /// be too large, and the client dropped every summary of a file that was
    /// only being saved; one that read half the old text and half the new
    /// was sent as it read, explained, billed, and billed again for the
    /// save. A file saved on every read is sent as it was last read; where
    /// that read says it cannot be explained, it is left for the next read
    /// to say, the file standing still.
    #[test]
    fn a_file_saved_while_it_was_read_is_read_again() {
        use super::sources_faults::{Stage, arm};
        let scratch = crate::test_support::Scratch::new("sources-mid-save");
        let root = scratch.canonicalize().unwrap();
        let path = root.join("saved.rs");
        let saved = "fn saved() -> &'static str {\n    \"café\"\n}\n";
        let mid_character = &saved.as_bytes()[..saved.find('é').unwrap() + 1];
        let past_the_cap = "x".repeat(super::MAX_INDEX_FILE_BYTES as usize + 1);
        let half_saved = "fn saved() -> &'static str {\n";
        for (during, what) in [
            (mid_character, "refused as NotUtf8"),
            (past_the_cap.as_bytes(), "too_large"),
            (half_saved.as_bytes(), "files"),
        ] {
            std::fs::write(&path, during).unwrap();
            // Standing still, the file is what the read found.
            assert_eq!(settled_as(&root, "saved.rs"), what);
            arm(&path, Stage::Read, saved.as_bytes());
            assert_eq!(sent(&root, "saved.rs"), Some(saved.to_string()), "{what}");
        }

        // Saved on every read — each save changing its size, so that it
        // shows however coarse the file system's clock — and read as the
        // last read found it: `one` or `other`, in turn. Its text is sent; a
        // verdict it cannot be explained is not.
        let rewritten = |one: &[u8], other: &[u8]| {
            std::fs::write(&path, one).unwrap();
            for read in 1..=clew_core::explain::STEADY_READS {
                arm(&path, Stage::Read, if read % 2 == 1 { other } else { one });
            }
            if clew_core::explain::STEADY_READS % 2 == 1 {
                one.to_vec()
            } else {
                other.to_vec()
            }
        };
        let cut_again = [mid_character, b"\xc3"].concat();
        rewritten(mid_character, &cut_again);
        assert_eq!(settled_as(&root, "saved.rs"), "unread");
        assert_eq!(settled_as(&root, "saved.rs"), "refused as NotUtf8");
        let last = rewritten(half_saved.as_bytes(), saved.as_bytes());
        assert_eq!(sent(&root, "saved.rs"), String::from_utf8(last).ok());
    }

    /// The text one `Sources` reply sends for `rel`, if it sends any.
    fn sent(root: &std::path::Path, rel: &str) -> Option<String> {
        match super::read_sources(root, vec![rel.to_string()], usize::MAX) {
            Event::Sources { files, .. } => files.into_iter().next().map(|(_, text)| text),
            other => panic!("expected Sources, got {other:?}"),
        }
    }
}
