//! Language-server access for the Ask agent's semantic tools.
//!
//! The agent runs on the server, so it owns its language-server instances,
//! resolved exactly like `SpawnLsp` resolves them (project `.clew/lsp.toml`
//! over the built-in registry). A server starts lazily on the first semantic
//! tool call for its language and is reused across turns. Nothing is
//! auto-installed here: provisioning stays a user-consented action in the
//! client, and an uninstalled server surfaces as a tool result the model can
//! react to (fall back to `search`, tell the user).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clew_core::highlight;
use clew_core::incremental::content_hash;
use clew_core::lsp::client::{LspClient, PositionEncoding, Target};
use clew_core::lsp::{config, store};
use tokio::sync::Mutex;

/// How long to give a freshly-started server's initial indexing before
/// answering queries anyway. Results while indexing carry a retry note.
const INDEX_WAIT: Duration = Duration::from_secs(20);
/// Time for indexing progress to first appear after the handshake.
const INDEX_GRACE: Duration = Duration::from_millis(1500);
/// Per-request timeout — a hung server must not hang the whole agent turn.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// Most targets a result lists, mirroring the `search` tool's cap.
const MAX_TARGETS: usize = 40;
/// Cooldown before a failed server start is attempted again. Failures like
/// "not installed" heal (the user consents to the install in the client), so
/// they must not be cached for the pool's lifetime.
const FAIL_RETRY: Duration = Duration::from_secs(60);
/// Largest file the semantic tools will pull into memory, matching the agent's
/// own read cap. Both the `didOpen` source and the preview lines come from
/// model-named (or language-server-named) paths, so neither can be unbounded.
const MAX_SEMANTIC_READ_BYTES: u64 = 4 * 1024 * 1024;
/// Most documents one language server keeps open for the agent. Every query
/// re-checks the whole open set against disk, and the server holds each open
/// document's text; unbounded, both grew with every file any turn ever asked
/// about. Past this the least recently queried document is closed.
const MAX_OPEN_DOCS: usize = 64;

/// Lazily-started language servers for one project, keyed by language.
pub struct LspPool {
    /// Root as the rest of the server knows it — identity for pool reuse.
    root: PathBuf,
    /// Canonical root: external processes (cargo, the language servers)
    /// report paths in canonical form, so URIs we send and prefixes we strip
    /// must use it or a symlinked root (e.g. `/tmp` on macOS) breaks both.
    /// Resolved on first use ([`LspPool::canon`]), never at construction: the
    /// pool is built on the request loop, and resolving a path is filesystem
    /// I/O that a hung mount could stall — along with every request queued
    /// behind it.
    canon: std::sync::OnceLock<PathBuf>,
    /// One slot per language, each with its own lock, so a slow first start
    /// of one language never blocks queries on another.
    slots: Mutex<HashMap<String, Arc<LangSlot>>>,
    /// The client's approvals for repo-specified commands — the agent's
    /// spawns go through the same [`crate::lsp_command_allowed`] gate as
    /// `SpawnLsp`. Without this, one semantic tool call could execute a
    /// hostile repo's `command` that the user never approved (or declined).
    approvals: crate::SharedApprovals,
    /// Every server this pool started, reachable without the slots' locks:
    /// [`LspPool::close`] stops each one even while a query holds its slot —
    /// a first query holds it through the whole start and index wait.
    started: std::sync::Mutex<Started>,
}

/// The servers a pool has started, and whether it has been closed.
#[derive(Default)]
struct Started {
    clients: Vec<LspClient>,
    closed: bool,
}

/// What a query on a closed pool is told.
const CLOSED: &str = "the project was closed — its language servers are stopped";

struct LangSlot {
    state: Mutex<LangState>,
}

enum LangState {
    Unstarted,
    Ready(Entry),
    /// Startup failed; cached with a cooldown (see [`FAIL_RETRY`]).
    Failed {
        error: String,
        at: Instant,
    },
}

struct Entry {
    client: LspClient,
    /// When the server was started. Readiness cannot rely on progress
    /// reporting alone (there are busy-but-silent phases, e.g. while `cargo
    /// metadata` blocks on a lock), so empty results from a freshly-started
    /// server are retried for a while after this instant.
    started: Instant,
    /// `didOpen`'d documents (see [`OpenDocs`]). Every query brings *all* of
    /// them back in line with disk, not just the file it is about — nothing
    /// else pushes edits at this server (no file watcher is wired to it), so
    /// an overlay left behind by an earlier query would otherwise stay
    /// pre-edit forever and silently answer later queries about other files.
    docs: OpenDocs,
}

/// What the pool told one server about one open document.
struct DocState {
    /// Hash of the text the server holds.
    hash: u64,
    version: i64,
    /// What disk looked like when that text was read. While the file still
    /// looks the same it is not re-read — let alone re-hashed — on resync.
    stamp: Option<FileStamp>,
    /// [`OpenDocs::clock`] at the document's last query, for eviction.
    last_used: u64,
}

/// A cheap identity for a file's current content: size, inode and times. Any
/// write moves the change time (which, unlike the modification time, cannot
/// be set back), and an atomic replace brings a new inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    inode: u64,
}

/// `path`'s stamp, or `None` when it is not a plain file (or cannot be
/// stat'ed) — such a document is always re-read, and the read decides.
fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(FileStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
        #[cfg(unix)]
        changed: (meta.ctime(), meta.ctime_nsec()),
        #[cfg(unix)]
        inode: meta.ino(),
    })
}

/// What the pool sends a server about its documents. A trait so the
/// bookkeeping below can be tested without a live server.
trait DocSink {
    fn open(&mut self, path: &Path, version: i64, text: &str);
    fn change(&mut self, path: &Path, version: i64, text: &str);
    fn close(&mut self, path: &Path);
}

/// The live sink: the language server itself.
struct ClientSink<'a> {
    client: &'a LspClient,
    language: &'a str,
}

impl DocSink for ClientSink<'_> {
    fn open(&mut self, path: &Path, version: i64, text: &str) {
        self.client.did_open(path, self.language, version, text);
    }
    fn change(&mut self, path: &Path, version: i64, text: &str) {
        self.client.did_change(path, version, text);
    }
    fn close(&mut self, path: &Path) {
        self.client.did_close(path);
    }
}

/// The documents the pool has open on one server — at most
/// [`MAX_OPEN_DOCS`], least recently queried closed first.
#[derive(Default)]
struct OpenDocs {
    docs: HashMap<PathBuf, DocState>,
    /// Counts queries; a document's `last_used` is the count at its last one.
    clock: u64,
}

impl OpenDocs {
    /// Bring the QUERIED document up to date on the server — `didOpen` the
    /// first time, `didChange` when its text moved — mark it most recently
    /// used, and close the least recently used ones past `cap`. `text` is
    /// what the query's positions were computed against; `stamp` was taken
    /// BEFORE that text was read, so a write racing the read shows as a
    /// changed stamp next time rather than hiding behind the new one.
    fn sync_queried(
        &mut self,
        abs: &Path,
        text: &str,
        stamp: Option<FileStamp>,
        cap: usize,
        sink: &mut impl DocSink,
    ) {
        self.clock += 1;
        let hash = content_hash(text.as_bytes());
        match self.docs.get_mut(abs) {
            None => {
                sink.open(abs, 1, text);
                self.docs.insert(
                    abs.to_path_buf(),
                    DocState {
                        hash,
                        version: 1,
                        stamp,
                        last_used: self.clock,
                    },
                );
            }
            Some(doc) => {
                if doc.hash != hash {
                    doc.version += 1;
                    sink.change(abs, doc.version, text);
                    doc.hash = hash;
                }
                doc.stamp = stamp;
                doc.last_used = self.clock;
            }
        }
        while self.docs.len() > cap {
            let oldest = self
                .docs
                .iter()
                .filter(|(path, _)| path.as_path() != abs)
                .min_by_key(|(_, doc)| doc.last_used)
                .map(|(path, _)| path.clone());
            let Some(path) = oldest else { break };
            self.docs.remove(&path);
            sink.close(&path);
        }
    }

    /// Bring every open document except `skip` back in line with disk, so no
    /// query is answered against an overlay whose text disk no longer has.
    ///
    /// A document whose stamp has not moved is skipped without being read;
    /// one that moved is re-read with `read` and pushed only when its text
    /// actually changed. A document that has become unreadable (deleted,
    /// replaced by a directory or a symlink, grown past the cap) must not keep
    /// its old text on the server either: its honest overlay is the empty
    /// document, and if the file comes back the hash moves again and it
    /// re-syncs.
    ///
    /// Work is bounded by the open set ([`MAX_OPEN_DOCS`]) times the per-file
    /// read cap, and one document is handed off at a time, so peak memory stays
    /// one capped file.
    fn resync_others(
        &mut self,
        skip: &Path,
        read: impl Fn(&Path) -> Option<String>,
        sink: &mut impl DocSink,
    ) {
        for (path, doc) in self.docs.iter_mut() {
            if path.as_path() == skip {
                continue;
            }
            let now = file_stamp(path);
            if now.is_some() && now == doc.stamp {
                continue;
            }
            let text = read(path).unwrap_or_default();
            let hash = content_hash(text.as_bytes());
            doc.stamp = now;
            if doc.hash == hash {
                continue;
            }
            doc.version += 1;
            doc.hash = hash;
            sink.change(path, doc.version, &text);
        }
    }
}

/// The read every resync uses: bounded and plain-file-only.
fn read_open_doc(path: &Path) -> Option<String> {
    clew_core::statefile::read_capped(path, MAX_SEMANTIC_READ_BYTES)
}

/// Which semantic query to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantic {
    Definition,
    References,
    Hover,
}

/// A formatted query result: the text for the model, plus project-relative
/// `(rel, 1-based line)` targets for the client's step chips.
pub struct SemanticResult {
    pub content: String,
    pub targets: Vec<(String, usize)>,
    /// The result may legitimately change on a later retry (the server was
    /// still indexing). Callers must not dedup-block an identical retry.
    pub transient: bool,
}

impl LspPool {
    pub fn new(root: PathBuf, approvals: crate::SharedApprovals) -> Self {
        LspPool {
            root,
            canon: std::sync::OnceLock::new(),
            approvals,
            slots: Mutex::new(HashMap::new()),
            started: std::sync::Mutex::default(),
        }
    }

    /// The canonical root (see the field): resolved once, by the first query
    /// — on the agent turn's thread, off the request loop.
    fn canon(&self) -> &Path {
        self.canon.get_or_init(|| {
            self.root
                .canonicalize()
                .unwrap_or_else(|_| self.root.clone())
        })
    }

    /// Stop every server this pool started, now, and start no more: the
    /// project is being left. Call it before letting go of the pool. Agent
    /// turns still running hold the pool (by `Arc`) for as long as they run,
    /// and each server would live as long as the last of them; their queries
    /// fail from here on instead.
    pub fn close(&self) {
        let clients = {
            let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
            started.closed = true;
            std::mem::take(&mut started.clients)
        };
        for client in clients {
            client.stop();
        }
    }

    /// Record a server this pool just started. `false` — nothing recorded —
    /// when the pool was closed meanwhile: the caller stops it.
    fn register(&self, client: &LspClient) -> bool {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if started.closed {
            return false;
        }
        started.clients.retain(LspClient::alive);
        started.clients.push(client.clone());
        true
    }

    fn is_closed(&self) -> bool {
        self.started
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run one semantic query against the file's language server. `line1` is
    /// 1-based (the numbering every other agent tool uses); `symbol` is the
    /// identifier text to anchor on within that line.
    pub async fn query(
        &self,
        kind: Semantic,
        rel: &str,
        abs: &Path,
        line1: usize,
        symbol: &str,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<SemanticResult, String> {
        // Canonical from here on: the URI we open must be the path the
        // server's own project model (e.g. cargo metadata) uses.
        let abs = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
        let Some(language) = highlight::detect(&abs) else {
            return Err(format!("no language server support for {rel}"));
        };
        // Bounded and plain-file-only at the read: `rel` is whatever path the
        // model named, and this whole file goes into the `didOpen` we send.
        // Stamped BEFORE the read (see `OpenDocs::sync_queried`).
        let stamp = file_stamp(&abs);
        let source = clew_core::statefile::read_capped(&abs, MAX_SEMANTIC_READ_BYTES)
            .ok_or_else(|| format!("cannot read {rel} (missing, too large, or not text)"))?;
        let Some(line_text) = source.lines().nth(line1.saturating_sub(1)) else {
            return Err(format!(
                "{rel} has only {} lines (asked for line {line1})",
                source.lines().count()
            ));
        };
        let Some(byte_col) = symbol_byte_col(line_text, symbol) else {
            return Err(format!(
                "`{symbol}` does not appear on line {line1} of {rel} — \
                 pass the line the symbol is on, as shown by read/search/outline"
            ));
        };

        let (client, started) = self.client_for(language, &abs, &source, stamp).await?;
        let character = lsp_character(client.encoding, line_text, byte_col);
        let line0 = line1 - 1;

        // An empty result — or an outright request error — from a busy server
        // is usually transient: the symbol isn't in its index yet, or the
        // server rejects requests while loading the workspace. "Busy" is
        // reported progress OR a freshly-started server (there are
        // busy-but-silent phases, e.g. `cargo metadata` blocked on a lock).
        // Re-query until the result lands, the server settles, or the bounded
        // window closes.
        let waited = Instant::now();
        let outcome = loop {
            let result = run_query(&client, kind, &abs, line0, character).await;
            let stopped = stop.load(std::sync::atomic::Ordering::Relaxed);
            let busy = client.busy() || started.elapsed() < INDEX_WAIT;
            let retryable = busy && waited.elapsed() < INDEX_WAIT && !stopped;
            match result {
                Ok(out) => {
                    let empty = match &out {
                        HoverOrTargets::Hover(text) => text.is_none(),
                        HoverOrTargets::Targets(t) => t.is_empty(),
                    };
                    if !empty || !retryable {
                        break out;
                    }
                }
                Err(e) if !retryable => return Err(e),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };

        let mut result = match outcome {
            HoverOrTargets::Hover(Some(text)) => SemanticResult {
                content: text,
                targets: Vec::new(),
                transient: false,
            },
            HoverOrTargets::Hover(None) => SemanticResult {
                content: format!("no hover info for `{symbol}`"),
                targets: Vec::new(),
                transient: false,
            },
            HoverOrTargets::Targets(targets) => self.format_targets(&targets, symbol, kind),
        };
        if client.busy() {
            result.content.push_str(
                "\n(note: the language server is still indexing — an empty result may \
                 be a false negative; retry this call in a later step)",
            );
            result.transient = true;
        }
        Ok(result)
    }

    /// Get or start the server for `language`, and sync `abs` onto it with
    /// `source` (the text the query positions were computed against).
    async fn client_for(
        &self,
        language: &str,
        abs: &Path,
        source: &str,
        stamp: Option<FileStamp>,
    ) -> Result<(LspClient, Instant), String> {
        if self.is_closed() {
            return Err(CLOSED.into());
        }
        // The pool lock is held only to fetch the per-language slot; slot
        // work (startup included) proceeds under the slot's own lock.
        let slot = {
            let mut slots = self.slots.lock().await;
            slots
                .entry(language.to_string())
                .or_insert_with(|| {
                    Arc::new(LangSlot {
                        state: Mutex::new(LangState::Unstarted),
                    })
                })
                .clone()
        };
        let mut state = slot.state.lock().await;
        if let LangState::Failed { error, at } = &*state {
            if at.elapsed() < FAIL_RETRY {
                return Err(error.clone());
            }
            *state = LangState::Unstarted;
        }
        // A server that died after startup (crash, OOM-kill) leaves a client
        // whose every request fails: discard it and start fresh. The docs map
        // goes with it — the new server needs its own didOpens.
        if matches!(&*state, LangState::Ready(e) if !e.client.alive()) {
            *state = LangState::Unstarted;
        }
        if matches!(*state, LangState::Unstarted) {
            match start(self.canon(), language, &self.approvals).await {
                Ok(client) => {
                    // Closed while this one was starting: it must not
                    // outlive the pool's other servers.
                    if !self.register(&client) {
                        client.stop();
                        return Err(CLOSED.into());
                    }
                    *state = LangState::Ready(Entry {
                        client,
                        started: Instant::now(),
                        docs: OpenDocs::default(),
                    });
                }
                Err(error) => {
                    *state = LangState::Failed {
                        error: error.clone(),
                        at: Instant::now(),
                    };
                    return Err(error);
                }
            }
        }
        let LangState::Ready(entry) = &mut *state else {
            unreachable!("state is Ready after the arms above")
        };
        let mut sink = ClientSink {
            client: &entry.client,
            language,
        };
        entry
            .docs
            .sync_queried(abs, source, stamp, MAX_OPEN_DOCS, &mut sink);
        // The queried file is now in sync; every *other* doc an earlier query
        // opened is still whatever it was then. Re-check those too, so the
        // answer can never come out of a pre-edit overlay of some sibling
        // file that only happens to self-heal when it is next queried.
        entry.docs.resync_others(abs, read_open_doc, &mut sink);
        Ok((entry.client.clone(), entry.started))
    }

    /// Render targets as `path:line: <line text>` rows, project paths first.
    fn format_targets(&self, targets: &[Target], symbol: &str, kind: Semantic) -> SemanticResult {
        if targets.is_empty() {
            let what = match kind {
                Semantic::Definition => "definition",
                _ => "references",
            };
            return SemanticResult {
                content: format!("no {what} found for `{symbol}`"),
                targets: Vec::new(),
                transient: false,
            };
        }
        let mut line_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut rows = Vec::new();
        let mut refs = Vec::new();
        for t in targets.iter().take(MAX_TARGETS) {
            // The server's number: saturating, since `line` may be anything
            // up to `u64::MAX` and an overflow here would panic the turn.
            let line1 = t.line.saturating_add(1);
            // Servers answer in canonical paths; fall back to the raw root
            // for callers that pass non-canonical targets (tests).
            let rel = t
                .path
                .strip_prefix(self.canon())
                .or_else(|_| t.path.strip_prefix(&self.root))
                .ok();
            let label = match rel {
                Some(rel) => {
                    let rel = rel.to_string_lossy().into_owned();
                    if refs.len() < 8 {
                        refs.push((rel.clone(), line1));
                    }
                    rel
                }
                // Outside the project (stdlib, dependencies): show where, but
                // it is not navigable in clew, so no chip.
                None => t.path.to_string_lossy().into_owned(),
            };
            let preview = line_cache
                .entry(t.path.clone())
                .or_insert_with(|| {
                    // A target can point anywhere the language server knows
                    // about, including generated files outside the project,
                    // so this read is bounded like every other.
                    clew_core::statefile::read_capped(&t.path, MAX_SEMANTIC_READ_BYTES)
                        .map(|s| s.lines().map(str::to_string).collect())
                        .unwrap_or_default()
                })
                .get(t.line)
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            rows.push(format!("{label}:{line1}: {preview}"));
        }
        let mut content = rows.join("\n");
        if targets.len() > MAX_TARGETS {
            content.push_str(&format!("\n… {} more", targets.len() - MAX_TARGETS));
        }
        SemanticResult {
            content,
            targets: refs,
            transient: false,
        }
    }
}

impl Drop for LspPool {
    /// Stop every server this pool started (`shutdown`, `exit`, then a kill
    /// of the process group if one lingers), as [`LspPool::close`] does. A
    /// client also stops its server when its last handle goes, but whoever
    /// still holds a clone would keep it running.
    fn drop(&mut self) {
        self.close();
    }
}

enum HoverOrTargets {
    Hover(Option<String>),
    Targets(Vec<Target>),
}

/// One raw LSP request for `kind`, time-boxed so a hung server cannot hang
/// the agent turn.
async fn run_query(
    client: &LspClient,
    kind: Semantic,
    abs: &Path,
    line0: usize,
    character: usize,
) -> Result<HoverOrTargets, String> {
    let fut = async {
        match kind {
            Semantic::Definition => client.definition(abs, line0, character).await,
            Semantic::References => {
                client
                    .navigate("textDocument/references", abs, line0, character)
                    .await
            }
            Semantic::Hover => {
                let text = client.hover(abs, line0, character).await?;
                return Ok(HoverOrTargets::Hover(text));
            }
        }
        .map(HoverOrTargets::Targets)
    };
    tokio::time::timeout(CALL_TIMEOUT, fut)
        .await
        .map_err(|_| "the language server timed out".to_string())?
}

/// Resolve and launch the server for `language`, then wait out its initial
/// indexing (bounded). Mirrors the `SpawnLsp` resolution, minus installs —
/// including the approval gate: a repo-specified `command` the user hasn't
/// approved must not run just because the *agent* asked instead of the GUI.
async fn start(
    root: &Path,
    language: &str,
    approvals: &crate::SharedApprovals,
) -> Result<LspClient, String> {
    // A broken config is an error, not "use defaults" — the default could
    // resolve a different server than the project configured.
    let config = config::ProjectLspConfig::load(root)?;
    let Some(server) = config.resolve(language) else {
        return Err(format!(
            "no language server is configured for {language} — use `search` instead"
        ));
    };
    let exe = match server.command.clone() {
        // The approved bytes, copied where the repository cannot reach them.
        // The gate fingerprints `server.init_options` too — the same value
        // handed to `initialize` below — so a commit that edits only the
        // options loses the approval instead of inheriting it.
        Some(cmd) => crate::lsp_command_allowed(approvals, root, &server, &cmd)?,
        // No `command`: the store binary, consented to at install time. Its
        // `init_options` are gated separately just below — nothing here can
        // ask the user anything, so unapproved ones are dropped, never sent.
        None => match store::locate(&server) {
            store::Located::Ready(exe) => exe,
            store::Located::NeedsDownload { .. } | store::Located::NeedsInstall { .. } => {
                return Err(format!(
                    "the {language} language server is not installed — the user can \
                     open a {language} file in clew to install it; use `search` for now"
                ));
            }
            // Handled by the `Some(cmd)` arm above; never spawned as-is.
            store::Located::RepoCommand(_) => {
                return Err("a repo-specified command must go through its approval".into());
            }
            store::Located::Unsupported(msg) => return Err(msg),
        },
    };
    // The repository's `init_options` reach `initialize` only when approved —
    // the agent asking instead of the GUI must not be a way around the gate,
    // exactly as it is not for a `command`. Withheld options only make the
    // server less configured; they never stop the agent's tools.
    let (init_options, _withheld) = crate::approved_init_options(approvals, root, &server);
    let client = LspClient::start(&exe, &server.args, root, init_options).await?;
    // Progress can lag the handshake; give it a moment to appear, then wait
    // (bounded) for the initial index so first queries aren't false negatives.
    tokio::time::sleep(INDEX_GRACE).await;
    let started = Instant::now();
    while client.busy() && started.elapsed() < INDEX_WAIT {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(client)
}

/// The LSP `character` of byte column `byte_col` of `line`, counted in the
/// server's negotiated position encoding — the same conversion `LspClient`
/// applies to every position it sends, so an astral character counts two
/// UTF-16 units here exactly as it does there.
fn lsp_character(encoding: PositionEncoding, line: &str, byte_col: usize) -> usize {
    encoding.units_in(&line[..byte_col])
}

/// Byte offset of `symbol` in `line`, preferring a match that stands alone as
/// an identifier (not embedded in a longer word) so `id` anchors on `id`, not
/// the middle of `identifier`.
fn symbol_byte_col(line: &str, symbol: &str) -> Option<usize> {
    if symbol.is_empty() {
        return None;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut fallback = None;
    let mut from = 0;
    while let Some(i) = line[from..].find(symbol) {
        let at = from + i;
        let before_ok = line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = line[at + symbol.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return Some(at);
        }
        fallback.get_or_insert(at);
        from = at + symbol.len();
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::Scratch;

    #[test]
    fn symbol_position_prefers_whole_identifiers() {
        // `id` embedded in `identifier` is skipped in favor of the standalone one.
        assert_eq!(symbol_byte_col("let identifier = id;", "id"), Some(17));
        // No standalone occurrence: fall back to the first embedded match.
        assert_eq!(symbol_byte_col("let identifier = 1;", "id"), Some(4));
        assert_eq!(symbol_byte_col("nothing here", "id"), None);
        assert_eq!(symbol_byte_col("x", ""), None);
    }

    #[test]
    fn symbol_position_counts_bytes_for_unicode_lines() {
        // "变量" is 6 bytes; the byte column must reflect that.
        let line = "变量 = call()";
        assert_eq!(symbol_byte_col(line, "call"), Some(9));
        // The column sent to the server counts the negotiated encoding's code
        // units: an astral character is four bytes but two UTF-16 units.
        let line = "😀变量 = call()";
        let byte_col = symbol_byte_col(line, "call").unwrap();
        assert_eq!(byte_col, 13);
        assert_eq!(lsp_character(PositionEncoding::Utf16, line, byte_col), 7);
        assert_eq!(lsp_character(PositionEncoding::Utf8, line, byte_col), 13);
    }

    #[test]
    fn format_targets_relativizes_and_previews() {
        let dir = Scratch::new("agent-lsp-fmt");
        std::fs::write(dir.join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let pool = LspPool::new(dir.to_path_buf(), Default::default());
        let targets = vec![
            // Canonical form, as a real server reports it.
            Target {
                path: dir.canonicalize().unwrap().join("a.rs"),
                line: 1,
                character: 3,
            },
            // Raw (non-canonical) form still relativizes via the fallback.
            Target {
                path: dir.join("a.rs"),
                line: 0,
                character: 0,
            },
            Target {
                path: PathBuf::from("/outside/lib.rs"),
                line: 0,
                character: 0,
            },
        ];
        let out = pool.format_targets(&targets, "two", Semantic::Definition);
        assert!(out.content.contains("a.rs:2: fn two() {}"));
        assert!(out.content.contains("a.rs:1: fn one() {}"));
        assert!(out.content.contains("/outside/lib.rs:1"));
        // Only the in-project targets become chips.
        assert_eq!(
            out.targets,
            vec![("a.rs".to_string(), 2), ("a.rs".to_string(), 1)]
        );

        let empty = pool.format_targets(&[], "two", Semantic::References);
        assert!(empty.content.contains("no references"));
    }

    /// Building a pool resolves nothing: it is built on the request loop, and
    /// canonicalizing its root there was filesystem I/O a hung mount could
    /// stall the loop on. The first query resolves it, off the loop.
    #[test]
    fn a_pool_resolves_its_root_on_first_use_not_at_construction() {
        let dir = crate::test_support::Scratch::new("agent-lsp-lazy-root");
        let pool = LspPool::new(dir.to_path_buf(), Default::default());
        assert!(
            pool.canon.get().is_none(),
            "construction touched the filesystem"
        );
        assert_eq!(pool.canon(), dir.canonicalize().unwrap());
        assert!(pool.canon.get().is_some());
    }

    /// F9: a target's line is the server's number; `line + 1` on the largest
    /// one panicked the agent's turn.
    #[test]
    fn a_target_on_the_last_possible_line_does_not_overflow() {
        let pool = LspPool::new(PathBuf::from("/proj"), Default::default());
        let targets = [Target {
            path: PathBuf::from("/elsewhere/lib.rs"),
            line: usize::MAX,
            character: 0,
        }];
        let out = pool.format_targets(&targets, "x", Semantic::Definition);
        assert!(
            out.content.contains(&format!("lib.rs:{}", usize::MAX)),
            "{}",
            out.content
        );
    }

    /// A client connected to a scripted peer that answers `initialize`; the
    /// peer's ends are returned so the session stays up.
    async fn connected_client() -> (
        LspClient,
        (
            tokio::io::BufReader<tokio::io::DuplexStream>,
            tokio::io::DuplexStream,
        ),
    ) {
        use clew_core::framing::{read_message, write_frame};
        let (client_stdin, peer_rx) = tokio::io::duplex(1 << 16);
        let (mut peer_tx, client_stdout) = tokio::io::duplex(1 << 16);
        let mut peer_rx = tokio::io::BufReader::new(peer_rx);
        let connecting = tokio::spawn(LspClient::connect(
            client_stdin,
            client_stdout,
            Path::new("/proj"),
            None,
        ));
        let init = read_message(&mut peer_rx).await.unwrap().unwrap();
        let answer = serde_json::json!({
            "jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}
        });
        write_frame(&mut peer_tx, &answer).await.unwrap();
        let client = connecting.await.unwrap().expect("handshake");
        (client, (peer_rx, peer_tx))
    }

    async fn wait_until_stopped(client: &LspClient) -> bool {
        for _ in 0..200 {
            if !client.alive() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    /// F4: leaving a project stops the pool's servers at once — while an agent
    /// turn still holds the pool, and while a query holds a server's slot —
    /// instead of when the last turn lets go of the pool. Nothing starts on a
    /// closed pool afterwards.
    #[tokio::test]
    async fn closing_the_pool_stops_its_servers_even_mid_query() {
        let (client, _peer) = connected_client().await;
        let pool = Arc::new(LspPool::new(PathBuf::from("/proj"), Default::default()));
        assert!(pool.register(&client));
        let slot = Arc::new(LangSlot {
            state: Mutex::new(LangState::Ready(Entry {
                client: client.clone(),
                started: Instant::now(),
                docs: OpenDocs::default(),
            })),
        });
        pool.slots.lock().await.insert("rust".into(), slot.clone());
        let turn = pool.clone(); // an agent turn still running
        let in_flight = slot.state.lock().await; // …with a query on the slot
        assert!(client.alive());
        pool.close();
        assert!(
            wait_until_stopped(&client).await,
            "the server outlived the close"
        );
        drop(in_flight);

        // A straggling query starts nothing: it is told the project is gone.
        let err = turn
            .client_for("rust", Path::new("/proj/a.rs"), "", None)
            .await
            .unwrap_err();
        assert_eq!(err, CLOSED);
        // And a server that finished starting after the close is refused.
        let (late, _late_peer) = connected_client().await;
        assert!(!turn.register(&late));
    }

    /// Dropping the pool stops its servers too.
    #[tokio::test]
    async fn dropping_the_pool_stops_its_servers() {
        let (client, _peer) = connected_client().await;
        let pool = LspPool::new(PathBuf::from("/proj"), Default::default());
        assert!(pool.register(&client));
        drop(pool);
        assert!(
            wait_until_stopped(&client).await,
            "the server outlived the pool"
        );
    }

    /// Records what the bookkeeping would have told a server.
    #[derive(Default)]
    struct Recorder {
        sent: Vec<(String, PathBuf, i64, String)>,
    }

    impl DocSink for Recorder {
        fn open(&mut self, path: &Path, version: i64, text: &str) {
            self.sent
                .push(("open".into(), path.to_path_buf(), version, text.to_string()));
        }
        fn change(&mut self, path: &Path, version: i64, text: &str) {
            self.sent.push((
                "change".into(),
                path.to_path_buf(),
                version,
                text.to_string(),
            ));
        }
        fn close(&mut self, path: &Path) {
            self.sent
                .push(("close".into(), path.to_path_buf(), 0, String::new()));
        }
    }

    fn scratch(tag: &str) -> Scratch {
        Scratch::new(&format!("agent-lsp-{tag}"))
    }

    /// Open `paths` as a run of queries, as `client_for` would.
    fn open_all(docs: &mut OpenDocs, paths: &[&PathBuf], sink: &mut Recorder) {
        for path in paths {
            let text = std::fs::read_to_string(path).unwrap();
            docs.sync_queried(path, &text, file_stamp(path), MAX_OPEN_DOCS, sink);
        }
    }

    /// Regression guard for the cross-file stale-overlay fix, without a live
    /// server: a doc an earlier query left open must be pushed back in line
    /// with disk before the next query is answered, and a doc that has not
    /// moved must not be pushed at all (a `didChange` per query per open file
    /// would make every turn re-upload the project).
    #[test]
    fn resync_pushes_only_the_open_docs_disk_has_moved_on_from() {
        let dir = scratch("resync");
        let queried = dir.join("queried.rs");
        let untouched = dir.join("untouched.rs");
        let edited = dir.join("edited.rs");
        let deleted = dir.join("deleted.rs");
        for (path, body) in [
            (&queried, "fn q() {}\n"),
            (&untouched, "fn u() {}\n"),
            (&edited, "fn e() {}\n"),
            (&deleted, "fn d() {}\n"),
        ] {
            std::fs::write(path, body).unwrap();
        }
        // The pool's view after four earlier queries: every file open at
        // version 1, hashed at the text the server was handed.
        let mut docs = OpenDocs::default();
        let mut sink = Recorder::default();
        open_all(
            &mut docs,
            &[&queried, &untouched, &edited, &deleted],
            &mut sink,
        );
        sink.sent.clear();

        // Disk moves on under three of them. `queried.rs` moved too, but
        // `client_for` syncs it itself before the resync runs, so re-sending
        // it here would double-bump the version it just assigned.
        std::fs::write(&edited, "//! new line\nfn e() {}\n").unwrap();
        std::fs::write(&queried, "//! new line\nfn q() {}\n").unwrap();
        std::fs::remove_file(&deleted).unwrap();

        docs.resync_others(&queried, read_open_doc, &mut sink);
        // Map order is arbitrary; the set of pushes is what matters.
        sink.sent.sort();
        assert_eq!(
            sink.sent,
            vec![
                // Unreadable now, so the honest overlay is the empty document
                // rather than the text the server still holds.
                ("change".to_string(), deleted.clone(), 2, String::new()),
                (
                    "change".to_string(),
                    edited.clone(),
                    2,
                    "//! new line\nfn e() {}\n".to_string()
                ),
            ],
            "only the moved docs, at the next version, carrying the new text"
        );
        assert_eq!(
            docs.docs[&untouched].version, 1,
            "unmoved doc is left alone"
        );
        // Skipped means untouched, hash included: the recorded hash must still
        // be the pre-edit one that `client_for` owns.
        assert_eq!(docs.docs[&queried].version, 1);
        assert_eq!(docs.docs[&queried].hash, content_hash(b"fn q() {}\n"));

        // Second pass with disk unchanged sends nothing: the hashes recorded
        // above must have advanced with the text, or every later query in the
        // turn would re-push the same documents.
        sink.sent.clear();
        docs.resync_others(&queried, read_open_doc, &mut sink);
        assert!(
            sink.sent.is_empty(),
            "resync is idempotent, got: {:?}",
            sink.sent
        );
    }

    /// A document whose file has not changed on disk is not even READ on
    /// resync, let alone re-hashed: every query used to read and hash every
    /// file any earlier query had opened.
    #[test]
    fn unchanged_docs_are_not_read_again() {
        let dir = scratch("stamp");
        let paths: Vec<PathBuf> = (0..5).map(|i| dir.join(format!("f{i}.rs"))).collect();
        for (i, path) in paths.iter().enumerate() {
            std::fs::write(path, format!("fn f{i}() {{}}\n")).unwrap();
        }
        let mut docs = OpenDocs::default();
        let mut sink = Recorder::default();
        open_all(&mut docs, &paths.iter().collect::<Vec<_>>(), &mut sink);

        let reads = std::cell::Cell::new(0usize);
        let counting = |path: &Path| {
            reads.set(reads.get() + 1);
            read_open_doc(path)
        };
        sink.sent.clear();
        docs.resync_others(&paths[0], counting, &mut sink);
        assert_eq!(reads.get(), 0, "nothing moved, so nothing is read");
        assert!(sink.sent.is_empty());

        // One file changes: exactly that one is read and pushed.
        std::fs::write(&paths[3], "fn f3() { changed(); }\n").unwrap();
        docs.resync_others(&paths[0], counting, &mut sink);
        assert_eq!(reads.get(), 1);
        assert_eq!(sink.sent.len(), 1);
        assert_eq!(sink.sent[0].1, paths[3]);
    }

    /// The open set is bounded: past the cap the least recently QUERIED
    /// document is closed on the server (`didClose`), never the one being
    /// queried, and a re-query makes a document recent again.
    #[test]
    fn the_least_recently_queried_doc_is_closed_past_the_cap() {
        let dir = scratch("evict");
        let paths: Vec<PathBuf> = (0..=MAX_OPEN_DOCS)
            .map(|i| {
                let path = dir.join(format!("d{i}.rs"));
                std::fs::write(&path, format!("fn d{i}() {{}}\n")).unwrap();
                path
            })
            .collect();
        let mut docs = OpenDocs::default();
        let mut sink = Recorder::default();
        open_all(
            &mut docs,
            &paths[..MAX_OPEN_DOCS].iter().collect::<Vec<_>>(),
            &mut sink,
        );
        assert_eq!(docs.docs.len(), MAX_OPEN_DOCS);
        assert!(sink.sent.iter().all(|(what, ..)| what == "open"));

        // Re-query the oldest: it becomes the newest.
        open_all(&mut docs, &[&paths[0]], &mut sink);
        sink.sent.clear();
        // One more document: the least recently used is now `d1`.
        open_all(&mut docs, &[&paths[MAX_OPEN_DOCS]], &mut sink);
        assert_eq!(docs.docs.len(), MAX_OPEN_DOCS, "bounded");
        let closed: Vec<_> = sink
            .sent
            .iter()
            .filter(|(what, ..)| what == "close")
            .map(|(_, path, ..)| path.clone())
            .collect();
        assert_eq!(closed, vec![paths[1].clone()]);
        assert!(
            docs.docs.contains_key(&paths[0]),
            "the re-queried doc stays"
        );
        assert!(
            docs.docs.contains_key(&paths[MAX_OPEN_DOCS]),
            "the queried doc stays"
        );

        // A closed document queried again is opened afresh.
        sink.sent.clear();
        open_all(&mut docs, &[&paths[1]], &mut sink);
        assert_eq!(sink.sent[0].0, "open");
        assert_eq!(sink.sent[0].2, 1, "a new document starts at version 1");
    }
}
