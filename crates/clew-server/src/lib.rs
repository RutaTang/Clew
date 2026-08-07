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
use notify_debouncer_full::new_debouncer;
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
}

/// Stdin backlog per process (messages, each ≤ one client frame). A child
/// that stopped reading hits this quickly. Overflow is not survivable for the
/// stream — losing one frame desyncs `Content-Length` framing forever — so a
/// full queue kills the process and reports it instead of dropping bytes or
/// queueing without bound until the OOM killer picks the server.
const PROC_INPUT_QUEUE: usize = 256;

/// Debounce window: coalesces the burst a single save or `git pull` produces.
const DEBOUNCE: Duration = Duration::from_millis(250);

/// The concrete debouncer type, held to keep the watch thread alive.
type Watcher = notify_debouncer_full::Debouncer<
    notify_debouncer_full::notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
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
/// Residual risk, accepted: the file is hashed here and spawned a moment
/// later — a same-instant swap between the two would win. Closing that needs
/// exec-by-fd, which std can't express portably; the threat the fingerprint
/// defends against is a *committed* change to an approved script (hours
/// apart), and a sub-second race requires code already running on this host.
pub fn lsp_command_allowed(
    approvals: &SharedApprovals,
    root: &Path,
    language: &str,
    command: &Path,
    args: &[String],
    server_name: &str,
    version: &str,
) -> Result<(), String> {
    let fingerprint = clew_core::trust::lsp_fingerprint(root, command, args, server_name, version)
        .map_err(|e| format!("cannot fingerprint the {language} server command: {e}"))?;
    let granted = approvals
        .lock()
        .unwrap()
        .get(language)
        .is_some_and(|f| *f == fingerprint);
    if granted
        || clew_core::trust::Trust::load().is_lsp_approved(None, root, language, &fingerprint)
    {
        return Ok(());
    }
    Err(format!(
        "refused: this project's lsp.toml command for {language} is not approved — \
         open a {language} file in clew and approve it there"
    ))
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
    /// AI provider config to use when the server makes calls (endpoint = Server).
    ai_chat: Option<llm::Config>,
    ai_embed: Option<embed::Config>,
    /// Stop flags for in-flight agent turns, keyed by the client's stream id.
    /// Shared with the blocking agent tasks, which remove themselves when done.
    agents: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    /// Client-granted approvals for repo-specified LSP commands (see
    /// [`lsp_command_allowed`]). Replaced by `LspApprovals`, cleared on
    /// `OpenProject` (approvals are per-project).
    lsp_approvals: SharedApprovals,
    /// Language servers backing the agent's semantic tools. Lazily created for
    /// the open project on the first agent turn; replaced when the root changes.
    agent_lsp: Option<Arc<agent_lsp::LspPool>>,
    /// Set when a `Hello` carried the wrong protocol version. From then on
    /// every non-Hello request is refused: the peer cannot parse half our
    /// frames anyway, and serving the half it can parse turns one clear
    /// error into a session of confusing ones.
    hello_failed: bool,
}

impl Server {
    /// Create a server that emits messages on `out`.
    pub fn new(out: UnboundedSender<ServerMessage>) -> Self {
        Server {
            root: None,
            files: Arc::new(Mutex::new(None)),
            out,
            _watcher: Arc::new(Mutex::new(None)),
            open_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            procs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            ai_chat: None,
            ai_embed: None,
            agents: Arc::new(Mutex::new(HashMap::new())),
            lsp_approvals: Arc::new(Mutex::new(HashMap::new())),
            agent_lsp: None,
            hello_failed: false,
        }
    }

    /// The current file list, if a project is open.
    fn current_files(&self) -> Option<Arc<Vec<FileEntry>>> {
        self.files.lock().unwrap().as_ref().map(|p| p.files.clone())
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
            Request::Hello { protocol, .. } => {
                if protocol != PROTOCOL_VERSION {
                    self.hello_failed = true;
                    return Some(Event::Error {
                        message: format!(
                            "protocol mismatch: client speaks v{protocol}, this clew-server \
                             speaks v{PROTOCOL_VERSION} — update so both sides match"
                        ),
                    });
                }
                self.hello_failed = false;
                Some(Event::Ready {
                    protocol: PROTOCOL_VERSION,
                })
            }
            // Fail closed after a version mismatch: a client that pipelined
            // requests behind its Hello gets a clear refusal for each, not
            // best-effort answers on a connection it half-understands.
            _ if self.hello_failed => Some(Event::Error {
                message: "refused: the protocol handshake failed — update client and \
                          server so both speak the same version"
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
                // Approvals are per-project; the client re-pushes them for
                // the new one after the open completes.
                self.lsp_approvals.lock().unwrap().clear();
                use std::sync::atomic::Ordering;
                let epoch = self.open_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                let open_epoch = self.open_epoch.clone();
                let files_slot = self.files.clone();
                let watcher_slot = self._watcher.clone();
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
                    // Check-and-commit in ONE critical section. With the
                    // epoch check outside it, a superseded open could pass
                    // the check, lose the race to the newer open's commit,
                    // and then overwrite it — files of project A filed under
                    // project B's root. The watcher swap rides in the same
                    // section so files and watcher can never disagree.
                    {
                        let mut slot = files_slot.lock().unwrap();
                        if open_epoch.load(Ordering::SeqCst) != epoch {
                            return; // superseded by a newer OpenProject
                        }
                        *slot = Some(ProjectFiles {
                            root: root.clone(),
                            files: Arc::new(scan.files),
                        });
                        // Watch the project; changes stream back as
                        // notifications, and the watcher refreshes the shared
                        // file list so search/docs/agent turns see the
                        // current set. (Setup only registers the watch — the
                        // callback runs on the watcher's own thread — so
                        // holding the files lock here cannot deadlock.)
                        let watcher = spawn_watcher(root.clone(), out.clone(), files_slot.clone());
                        *watcher_slot.lock().unwrap() = watcher;
                    }
                    // The reply follows the committed state; a newer open
                    // that lands after us sends its own Tree (with its own
                    // root) right behind this one.
                    Self::reply(
                        &out,
                        id,
                        Event::Tree {
                            root: root.to_string_lossy().into_owned(),
                            tree: scan.tree,
                            files: rels,
                            truncated: scan.truncated,
                        },
                    );
                });
                None
            }
            // Read + tokenize a file for display. `rel` resolves against the
            // project root; the reply carries per-line (text, style-index) spans
            // that the client maps to theme colors.
            Request::ReadFile { rel, target } => {
                let root = self.root.clone()?;
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
                    // A notebook parses into cells (highlighted server-side)
                    // and replies as `NotebookContent`; raw JSON is never shown.
                    if clew_core::notebook::is_notebook(&abs) {
                        let event = match std::fs::read_to_string(&abs) {
                            Ok(json) => match clew_core::notebook::parse(&json) {
                                Some(nb) => notebook_event(rel, nb),
                                None => Event::Error {
                                    message: format!("{rel}: not a readable notebook"),
                                },
                            },
                            Err(e) => Event::Error {
                                message: format!("read {rel}: {e}"),
                            },
                        };
                        return Self::reply(&out, id, event);
                    }
                    let event = match std::fs::read_to_string(&abs) {
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
                let root = self.root.clone()?;
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
                let files = self.current_files()?;
                let opts = search::SearchOptions {
                    query,
                    regex,
                    case_sensitive,
                    whole_word,
                    include,
                    exclude,
                    root: self.root.clone(),
                };
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
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
                let input_rx = register_proc(&self.procs, proc).await;
                spawn_registered(&self.out, &self.procs, proc, cmd, args, cwd, input_rx).await
            }
            // Start a language server the server resolves itself — the client
            // never ships a binary path, so a remote uses its own LSP. The
            // resolve + approval gate run on a blocking thread: the gate
            // hashes the executable's bytes, and even the bounded worst case
            // must not stall the serial request loop (a queued ProcessKill
            // has to stay reachable).
            Request::SpawnLsp { proc, language } => {
                let root = self.root.clone()?;
                let out = self.out.clone();
                let procs = self.procs.clone();
                let approvals = self.lsp_approvals.clone();
                // Register the stdin queue before detaching: the client
                // pipelines the LSP `initialize` right behind this request,
                // and those frames must buffer for the child the resolve is
                // still working toward — not race its registration.
                let input_rx = register_proc(&self.procs, proc).await;
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
                            if let Some(event) = spawn_registered(
                                &out,
                                &procs,
                                proc,
                                exe.to_string_lossy().into_owned(),
                                args,
                                cwd,
                                input_rx,
                            )
                            .await
                            {
                                Self::reply(&out, id, event);
                            }
                        }
                        Err(message) => {
                            // Nothing will ever run: retract the queue and
                            // end the proxy so the client's LSP driver sees EOF.
                            procs.lock().await.remove(&proc);
                            let _ = out.send(ServerMessage::Notification {
                                sub: None,
                                event: Event::ProcessExited { proc, code: None },
                            });
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
                let root = self.root.clone()?;
                let out = self.out.clone();
                tokio::spawn(async move {
                    let (lang, r) = (language.clone(), root.clone());
                    let resolution =
                        tokio::task::spawn_blocking(move || Self::resolve_lsp(&r, &lang))
                            .await
                            .unwrap_or_else(|_| clew_protocol::LspResolution::Unsupported {
                                message: format!("resolving the {language} server failed"),
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
                let root = self.root.clone()?;
                let out = self.out.clone();
                tokio::spawn(async move {
                    let (lang, r) = (language.clone(), root.clone());
                    let outcome =
                        tokio::task::spawn_blocking(move || Self::install_lsp(&r, &lang)).await;
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
            Request::SetAiConfig { chat, embed } => {
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
                tokio::task::spawn_blocking(move || {
                    let sink = out.clone();
                    let result =
                        llm::complete_chat_stream(&cfg, &system, &msgs, max_tokens, |delta| {
                            let _ = sink.send(ServerMessage::Notification {
                                sub: None,
                                event: Event::ChatDelta {
                                    stream,
                                    text: delta.to_string(),
                                },
                            });
                        });
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
                let (Some(root), Some(files)) = (self.root.clone(), self.current_files()) else {
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
                tokio::task::spawn_blocking(move || {
                    agent::run(
                        root, files, chat, embed_cfg, lsp, rt, stream, question, history, context,
                        &out, &flag,
                    );
                    agents.lock().unwrap().remove(&stream);
                });
                None
            }
            Request::AgentStop { stream } => {
                if let Some(flag) = self.agents.lock().unwrap().get(&stream) {
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
                let files = self.current_files()?;
                let docs_root = self.root.clone()?;
                let out = self.out.clone();
                tokio::task::spawn_blocking(move || {
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
            // Remaining flows migrate here (Outline, Explain, …).
            _ => None,
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
        let config = clew_core::lsp::config::ProjectLspConfig::load(root).unwrap_or_default();
        let Some(server) = config.resolve(language) else {
            return Err(None);
        };
        use clew_core::lsp::store::Located;
        let exe = match server.command.clone() {
            // A `command` comes from the project's own lsp.toml, which ships
            // with the repository. Run it only through the one shared gate
            // every spawn path uses.
            Some(cmd) => {
                lsp_command_allowed(
                    approvals,
                    root,
                    language,
                    &cmd,
                    &server.args,
                    &server.server_name,
                    &server.version,
                )
                .map_err(Some)?;
                // Spawn exactly the file the fingerprint approved: a bare
                // name would be looked up on PATH instead.
                clew_core::trust::resolve_command(root, &cmd)
            }
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
    fn resolve_lsp(root: &Path, language: &str) -> clew_protocol::LspResolution {
        use clew_core::lsp::store::Located;
        use clew_protocol::LspResolution;
        let config = clew_core::lsp::config::ProjectLspConfig::load(root).unwrap_or_default();
        let Some(server) = config.resolve(language) else {
            return LspResolution::Unsupported {
                message: format!("no language server is configured for {language}"),
            };
        };
        let init_options = server
            .init_options
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        if let Some(cmd) = server.command.clone() {
            return match clew_core::trust::lsp_fingerprint(
                root,
                &cmd,
                &server.args,
                &server.server_name,
                &server.version,
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
            Located::Ready(_) => LspResolution::Ready { init_options },
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
    fn install_lsp(root: &Path, language: &str) -> clew_protocol::LspResolution {
        use clew_core::lsp::store::Located;
        use clew_protocol::LspResolution;
        let config = clew_core::lsp::config::ProjectLspConfig::load(root).unwrap_or_default();
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
            Ok(()) => Self::resolve_lsp(root, language),
            Err(e) => LspResolution::Unsupported {
                message: format!("install {language} server: {e}"),
            },
        }
    }
}

/// Register the stdin queue for `proc` in the table, ahead of the actual
/// spawn. From this moment `ProcessInput` frames buffer in the queue; once
/// the OS process exists, [`spawn_registered`] wires the queue to its stdin
/// and every buffered byte drains in order. This is what makes a client's
/// pipelined `spawn; write` correct even though the spawn itself runs on a
/// detached task.
async fn register_proc(procs: &SharedProcs, proc: u64) -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (input, input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PROC_INPUT_QUEUE);
    procs.lock().await.insert(proc, Proc { input, child: None });
    input_rx
}

/// Spawn `cmd` (in `cwd` when given) and proxy its stdio to the client under
/// handle `proc`, whose stdin queue was set up by [`register_proc`]: stdout
/// streams back as `ProcessOutput`, stdin drains `input_rx` (frames fed by
/// `ProcessInput`, possibly queued since before the spawn). Emits exactly one
/// of `ProcessStarted` or `ProcessExited`; on failure the table entry is
/// removed and the error returned for the caller to report.
async fn spawn_registered(
    out: &UnboundedSender<ServerMessage>,
    procs: &SharedProcs,
    proc: u64,
    cmd: String,
    args: Vec<String>,
    cwd: Option<String>,
    mut input_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Option<Event> {
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
            // Attach the child to its pre-registered entry. A missing entry
            // means the client killed the process (or its queue overflowed)
            // while the spawn was in flight — the remover already reported
            // the exit, so just reap the newborn quietly.
            match procs.lock().await.get_mut(&proc) {
                Some(p) => p.child = Some(child),
                None => {
                    let mut child = child;
                    let _ = child.start_kill();
                    return None;
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
                loop {
                    match stdout.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let msg = ServerMessage::Notification {
                                sub: None,
                                event: Event::ProcessOutput {
                                    proc,
                                    data: buf[..n].to_vec(),
                                },
                            };
                            if out.send(msg).is_err() {
                                break;
                            }
                        }
                    }
                }
                // The child is gone (or the client is): drop its table entry
                // so naturally-exited processes don't accumulate for the
                // session's lifetime, and tell the client.
                procs_cleanup.lock().await.remove(&proc);
                let _ = out.send(ServerMessage::Notification {
                    sub: None,
                    event: Event::ProcessExited { proc, code: None },
                });
            });
            None
        }
        Err(e) => {
            // The process never existed: retract the pre-registered entry and
            // close the proxy (EOF for the client's driver) before reporting,
            // so no half-open stream or stale mapping outlives the failure.
            procs.lock().await.remove(&proc);
            let _ = out.send(ServerMessage::Notification {
                sub: None,
                event: Event::ProcessExited { proc, code: None },
            });
            Some(Event::Error {
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
    if std::fs::metadata(&f.abs)
        .map(|m| m.len() > MAX_DOC_FILE_BYTES)
        .unwrap_or(true)
    {
        return None;
    }
    // Re-verify the path is still a regular file inside the project: the
    // scan can be stale, and a symlink would read outside it.
    let source = clew_core::fs_scan::read_confined(root, &f.abs)?;
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

/// Watch `root` recursively; stream changes back on `out` as notifications. A
/// content change emits `FilesChanged`; a create/delete also re-scans and emits
/// an updated `Tree`. Returns the debouncer, which must be kept alive to run.
fn spawn_watcher(
    root: PathBuf,
    out: UnboundedSender<ServerMessage>,
    files: SharedFiles,
) -> Option<Watcher> {
    let cb_root = root.clone();
    let mut debouncer = new_debouncer(
        DEBOUNCE,
        None,
        move |res: notify_debouncer_full::DebounceEventResult| {
            let Ok(events) = res else { return };
            let mut rels: Vec<String> = Vec::new();
            let mut structural = false;
            for ev in &events {
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
                        | EventKind::Modify(
                            notify_debouncer_full::notify::event::ModifyKind::Name(_)
                        )
                ) {
                    structural = true;
                }
                for p in &ev.paths {
                    if is_noise(p) {
                        continue;
                    }
                    if let Ok(rel) = p.strip_prefix(&cb_root) {
                        rels.push(rel.to_string_lossy().into_owned());
                    }
                }
            }
            // A create/delete changes the file set: re-scan, refresh the
            // server's shared file list (so search/docs/agent turns grep the
            // current set, not the one from OpenProject), and push a fresh tree.
            if structural {
                let scan = clew_core::fs_scan::scan(cb_root.clone());
                let rels = scan.files.iter().map(|f| f.rel.clone()).collect();
                {
                    let mut slot = files.lock().unwrap();
                    // Only while this watcher's project is still the open one:
                    // a late callback from a replaced watcher must not clobber
                    // the next project's file list.
                    if slot.as_ref().is_some_and(|p| p.root == cb_root) {
                        *slot = Some(ProjectFiles {
                            root: cb_root.clone(),
                            files: Arc::new(scan.files),
                        });
                    }
                }
                let _ = out.send(ServerMessage::Notification {
                    sub: None,
                    event: Event::Tree {
                        root: cb_root.to_string_lossy().into_owned(),
                        tree: scan.tree,
                        files: rels,
                        truncated: scan.truncated,
                    },
                });
            }
            rels.sort();
            rels.dedup();
            if !rels.is_empty() {
                let _ = out.send(ServerMessage::Notification {
                    sub: None,
                    event: Event::FilesChanged {
                        root: cb_root.to_string_lossy().into_owned(),
                        rels,
                    },
                });
            }
        },
    )
    .ok()?;
    debouncer.watch(&root, RecursiveMode::Recursive).ok()?;
    Some(debouncer)
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

/// Run the server over stdio until the client's stream ends (or stdin closes).
///
/// Framing is newline-delimited JSON: each `ClientMessage` arrives as one line,
/// each `ServerMessage` is written back as one line. serde_json's compact output
/// never contains a literal newline (string values escape theirs), so a line is
/// always exactly one message. A dedicated writer task drains the output channel
/// so replies and unsolicited notifications (file changes) share one stdout.
pub async fn serve_stdio() {
    let (out, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = out_rx.recv().await {
            let Ok(mut json) = serde_json::to_string(&msg) else {
                continue;
            };
            json.push('\n');
            if stdout.write_all(json.as_bytes()).await.is_err() {
                break;
            }
            if stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut server = Server::new(out.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.is_empty() {
            continue;
        }
        let Ok(ClientMessage { id, request }) = serde_json::from_str::<ClientMessage>(&line) else {
            continue; // ignore malformed frames rather than dying
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
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::{confine, is_generated_source};
    use std::path::Path;

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
}
