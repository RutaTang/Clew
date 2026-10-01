//! A minimal async LSP client: spawns a server subprocess (or attaches to one
//! proxied elsewhere), speaks `Content-Length`-framed JSON-RPC over its stdio,
//! and exposes the requests clew needs (initialize, didOpen, definition, …).
//!
//! Design: the transport is [`crate::framing`] — one *reader* task parses
//! framed messages off stdout, one *actor* task owns stdin plus the
//! pending-request map. This module supplies the JSON-RPC half of that actor
//! ([`LspWire`]) and the server-state bookkeeping. `LspClient` is a cheap,
//! cloneable handle that talks to the actor over a channel, so it can be
//! moved into iced `Task`s freely.
//!
//! Lifetime: a session ends when [`LspClient::stop`]/[`LspClient::shutdown`]
//! is called or the last handle is dropped. Either way the server is asked to
//! go (`shutdown`, then `exit`), and a local child that has not left within a
//! bounded grace period is killed together with its process group, so the
//! helpers a server starts (rust-analyzer's `cargo check`) do not outlive it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::framing::{self, CallError, EndReason, Failure, FrameError, Goodbye, Handled, Ids};

/// Most recent server output lines to retain for the management panel.
const MAX_LOGS: usize = 400;
/// Longest single log entry kept, and the total the whole buffer may hold.
///
/// A count is not a bound. Both sources feeding the buffer are the server's
/// own output: stderr, which arrives with no length limit at all, and
/// `window/logMessage`, whose only limit is [`framing::MAX_FRAME_BYTES`] — so
/// 400 entries could pin gigabytes, cloned in full on every render of the
/// panel.
const MAX_LOG_ENTRY_BYTES: usize = 8 * 1024;
const MAX_LOG_TOTAL_BYTES: usize = 1024 * 1024;

/// Ceiling for one request. A server that never answers must not hang its
/// caller (a "Searching…" status forever) nor keep the pending entry.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a stopping session waits for the server to answer `shutdown`
/// before sending `exit` anyway.
const SHUTDOWN_REPLY_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(3)
};

/// How long a local server gets to exit after `exit` before it (and its
/// process group) is killed.
const EXIT_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(2)
};

/// The most a stop can take, end to end: the actor's writes (at most
/// [`framing::STOP_LIMIT`]), the answer to `shutdown` (at most
/// [`SHUTDOWN_REPLY_WAIT`]), then the server's [`EXIT_GRACE`] — plus a margin
/// for the kill and the reap.
const STOP_LIMIT: Duration = framing::STOP_LIMIT
    .saturating_add(SHUTDOWN_REPLY_WAIT)
    .saturating_add(EXIT_GRACE)
    .saturating_add(Duration::from_secs(1));

/// Most work-done progress tokens tracked at once. A token is released by its
/// `end`; a server that begins tokens and never ends them must not grow the
/// table without bound, so the stalest is dropped past this.
const MAX_PROGRESS_TOKENS: usize = 64;

/// Longest progress line handed to the UI (it goes in the status bar).
const MAX_PROGRESS_TEXT: usize = 96;

/// How long a server that has said nothing of its work yet counts as loading
/// the project after `initialized` ([`ServerState::loading`]). A server
/// loads its workspace on `initialized`, or on the first document opened,
/// and says so as work in progress moments later — gopls "Loading
/// packages", typescript-language-server "Initializing JS/TS language
/// features". Asked in that gap, it answered from nothing loaded yet: no
/// callers, or an error for a file it had not read.
const LOAD_GRACE: Duration = Duration::from_secs(2);

/// The same gap for a server that says itself when it has loaded
/// ([`SERVER_STATUS_REPORTER`], through `experimental/serverStatus`, which
/// `initialize` asks for): it is loading until its first status says it is
/// not. That status comes within milliseconds of `initialized`; a server
/// that has sent none for this long sends none at all — an older build, or
/// one that ignores the capability — and is read like any other from then
/// on.
const STATUS_GRACE: Duration = Duration::from_secs(10);

/// The name the one server known to report `experimental/serverStatus`
/// gives itself in `initialize`'s `serverInfo`.
const SERVER_STATUS_REPORTER: &str = "rust-analyzer";

/// One diagnostic (error/warning/…) at a position, in the server's encoding.
#[derive(Debug, Clone)]
pub struct Diag {
    pub line: usize,
    pub char_start: usize,
    pub char_end: usize,
    pub severity: u8, // 1 error, 2 warning, 3 info, 4 hint
    pub message: String,
}

/// Diagnostics per file, shared rather than copied: a [`Snapshot`] holds the
/// map by reference count, and a publish replaces one file's entry
/// copy-on-write (`Arc::make_mut`), so taking a snapshot never clones a
/// diagnostic.
pub type DiagMap = Arc<HashMap<PathBuf, Arc<[Diag]>>>;

/// Observable server state shared with the UI: logs, progress, diagnostics.
#[derive(Default)]
pub struct ServerState {
    pub logs: VecDeque<String>,
    /// Work-done progress in flight, one entry per token (see [`Progress`]).
    progress: Progress,
    /// Latest diagnostics per file.
    diagnostics: DiagMap,
    /// Bumped whenever diagnostics change, so the UI knows to refresh.
    pub diag_version: u64,
    /// Bumped when the server asks us to refresh inlay hints
    /// (`workspace/inlayHint/refresh`), so the UI re-requests them.
    pub inlay_epoch: u64,
    /// The workspace settings we hand back when the server pulls
    /// `workspace/configuration` (e.g. pyright asking for `python.pythonPath`).
    /// Seeded from the resolved initializationOptions at start.
    pub settings: Option<Value>,
    /// The workspace folders we opened the server on — the project root —
    /// handed back when the server asks (`workspace/workspaceFolders`).
    folders: Value,
    /// The server's current complaint, if any (e.g. rust-analyzer's "failed
    /// to load workspace"). Surfaced in the status bar so a server that is
    /// "ready" but couldn't load the project doesn't look healthy while every
    /// go-to-def silently returns nothing — and cleared when the server
    /// recovers, so a fixed problem does not stay on screen for the session.
    pub error: Option<String>,
    /// Where `error` came from, which decides what clears it.
    error_source: Option<ErrorSource>,
    /// rust-analyzer's `experimental/serverStatus` `quiescent`: `Some(false)`
    /// while it is still loading the workspace. `None` for servers that do
    /// not report it.
    quiescent: Option<bool>,
    /// The server has been idle since it started — no work in flight, and
    /// its workspace loaded — so work it reports from then on is work on the
    /// project as it changes (a check after a save), not the loading of it
    /// ([`ServerState::loading`]).
    settled: bool,
    /// When `initialized` went out: until the server has said anything of
    /// its work, it counts as loading for a grace after this ([`LOAD_GRACE`],
    /// or [`STATUS_GRACE`] for one that reports its status). `None` before
    /// then, and for a state no session started.
    started: Option<Instant>,
    /// The server says itself when it has loaded, through
    /// `experimental/serverStatus` (it named itself
    /// [`SERVER_STATUS_REPORTER`]): before its first status, it has not.
    reports_status: bool,
    /// Every report the server has made of its work — a `$/progress`, an
    /// `experimental/serverStatus` — counted. A holder waiting for it to load
    /// tells a server still at it from a silent one by whether this moved
    /// ([`Snapshot::reports`]).
    reports: u64,
    /// Bytes currently held in `logs`, so the buffer can be bounded by size
    /// as well as by entry count.
    log_bytes: usize,
    /// Bumped with every line pushed to `logs`, so a view can tell the log
    /// changed without copying it (see [`Snapshot::log_version`]).
    log_version: u64,
}

/// The server's shared state could not be read: a thread panicked while it
/// held the lock, so whatever the state says now may be half-written.
///
/// Readers used to take a poisoned lock for an empty state — no diagnostics,
/// no log, no progress, no error — which is exactly what a healthy, quiet
/// server looks like. This names the failure instead, so a UI can say the
/// server needs a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatePoisoned;

impl std::fmt::Display for StatePoisoned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("clew lost track of this language server after an internal error; restart it")
    }
}

impl std::error::Error for StatePoisoned {}

/// What a view shows about one server, read under ONE lock acquisition and
/// cheap to take: progress and error are short strings, and the diagnostics
/// are shared with the live state (see [`DiagMap`]), never copied.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// The work-done progress line ([`LspClient::progress`]).
    pub progress: Option<String>,
    /// Whether the server says it is still working ([`LspClient::busy`]).
    pub busy: bool,
    /// Whether it is still loading the project ([`LspClient::loading`]).
    pub loading: bool,
    /// How many reports of its work the server has made so far — a
    /// `$/progress`, an `experimental/serverStatus`. Moving, it is at work;
    /// standing still while it loads, it has gone silent.
    pub reports: u64,
    /// The server's current complaint ([`LspClient::error`]).
    pub error: Option<String>,
    /// Bumped whenever any diagnostics change.
    pub diag_version: u64,
    /// The inlay-hint refresh epoch.
    pub inlay_epoch: u64,
    /// Bumped with every log line: a view showing the log copies it again
    /// ([`LspClient::log_tail`]) only when this moved, instead of on every
    /// repaint.
    pub log_version: u64,
    diagnostics: DiagMap,
}

impl Snapshot {
    /// This file's diagnostics as of the snapshot (empty if none).
    pub fn diagnostics(&self, path: &Path) -> &[Diag] {
        self.diagnostics.get(path).map_or(&[], |d| d)
    }
}

impl ServerState {
    /// Whether the server is still loading the project, as far as it says:
    /// rust-analyzer's workspace not loaded yet (`quiescent`, which says so
    /// exactly), or — from a server that does not report that — work it
    /// began before it was first idle. A server just started is loading
    /// until it has said so or otherwise: through the grace after
    /// `initialized` ([`LOAD_GRACE`]) while it has reported nothing, and —
    /// rust-analyzer — until its first status ([`STATUS_GRACE`] at most).
    /// Asked in that gap, a server answered from nothing loaded yet.
    fn loading(&self, now: Instant) -> bool {
        match self.quiescent {
            Some(quiescent) => !quiescent,
            None => !(self.settled || (!self.progress.busy() && self.grace_over(now))),
        }
    }

    /// Whether the grace a just-started server gets to say how its loading
    /// goes is over ([`ServerState::loading`]).
    fn grace_over(&self, now: Instant) -> bool {
        let grace = match self.reports_status {
            true => STATUS_GRACE,
            false => LOAD_GRACE,
        };
        self.started
            .is_none_or(|started| now.saturating_duration_since(started) >= grace)
    }

    /// A report just folded in left the server idle, if it did: from then
    /// on it has loaded ([`ServerState::settled`]) — unless it says itself
    /// when it has, and has not said yet: work it reports before its first
    /// status is part of the loading that status covers.
    fn note_idle(&mut self) {
        let says = self.quiescent.is_some() || !self.reports_status;
        if says && !self.progress.busy() && self.quiescent != Some(false) {
            self.settled = true;
        }
    }

    /// The server has been idle through the whole grace after its start, if
    /// it has: it has loaded, and what it reports from now on — a check
    /// after a save — is not the loading of the project. Noted before a
    /// report is folded in, since that report may begin such work.
    fn note_quiet(&mut self, now: Instant) {
        if !self.progress.busy() && self.quiescent != Some(false) && self.grace_over(now) {
            self.settled = true;
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            progress: self.progress.summary(),
            loading: self.loading(Instant::now()),
            reports: self.reports,
            busy: self.progress.busy() || self.quiescent == Some(false),
            error: self.error.clone(),
            diag_version: self.diag_version,
            inlay_epoch: self.inlay_epoch,
            log_version: self.log_version,
            diagnostics: Arc::clone(&self.diagnostics),
        }
    }
}

/// A consistent [`Snapshot`] of `state`, or [`StatePoisoned`].
fn snapshot_of(state: &Mutex<ServerState>) -> Result<Snapshot, StatePoisoned> {
    state
        .lock()
        .map(|s| s.snapshot())
        .map_err(|_| StatePoisoned)
}

/// Where the current [`ServerState::error`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorSource {
    /// An error-level `window/showMessage`. Servers send no matching "fixed"
    /// message, so recovery is inferred: an Info-level `showMessage`, or a
    /// request answered with real content.
    ShowMessage,
    /// `experimental/serverStatus` health. The same channel reports recovery
    /// precisely (`health: "ok"`), so nothing else may clear it: answering a
    /// hover about `std` says nothing about a workspace that failed to load.
    ServerStatus,
    /// The connection to the server broke. Final for this client.
    Transport,
}

/// Work-done progress (`$/progress`), tracked per token.
///
/// Servers run several progress reports at once (rust-analyzer: "Fetching",
/// "Indexing", "Roots Scanned"), and one ending says nothing about the
/// others. Folding them into one line let the first `end` declare the server
/// idle while it was still indexing — which is exactly what the agent's
/// readiness check asks.
#[derive(Default)]
struct Progress {
    /// Token (its JSON text, so `1` and `"1"` stay distinct) → state.
    tokens: HashMap<String, ProgressEntry>,
    /// Bumped on every update: the most recently touched token is the one
    /// worth showing, and the least recently touched the one to drop.
    clock: u64,
}

struct ProgressEntry {
    title: String,
    message: Option<String>,
    percentage: Option<u64>,
    touched: u64,
}

impl Progress {
    /// Fold one `$/progress` notification's params in.
    fn update(&mut self, params: Option<&Value>) {
        let Some(token) = params.and_then(|p| p.get("token")) else {
            return;
        };
        let key = token.to_string();
        let value = params.and_then(|p| p.get("value"));
        let field = |name: &str| value.and_then(|v| v.get(name));
        let text = |name: &str| field(name).and_then(Value::as_str).map(str::to_string);
        match field("kind").and_then(Value::as_str) {
            Some("end") => {
                self.tokens.remove(&key);
                return;
            }
            Some("begin") | Some("report") => {}
            _ => return, // not work-done progress
        }
        self.clock += 1;
        let clock = self.clock;
        let entry = self.tokens.entry(key).or_insert_with(|| ProgressEntry {
            title: String::new(),
            message: None,
            percentage: None,
            touched: clock,
        });
        // `begin` carries the title; `report` only updates message and
        // percentage and must not blank the title out.
        if let Some(title) = text("title") {
            entry.title = title;
        }
        if let Some(message) = text("message") {
            entry.message = Some(message);
        }
        if let Some(pct) = field("percentage").and_then(Value::as_u64) {
            entry.percentage = Some(pct.min(100));
        }
        entry.touched = clock;
        while self.tokens.len() > MAX_PROGRESS_TOKENS {
            let Some(stalest) = self
                .tokens
                .iter()
                .min_by_key(|(_, e)| e.touched)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.tokens.remove(&stalest);
        }
    }

    fn busy(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// One status line: the most recently updated report, plus how many
    /// others are running.
    fn summary(&self) -> Option<String> {
        let latest = self.tokens.values().max_by_key(|e| e.touched)?;
        let mut line = if latest.title.is_empty() {
            "working".to_string()
        } else {
            latest.title.clone()
        };
        if let Some(message) = latest.message.as_deref().filter(|m| !m.is_empty()) {
            line.push_str(": ");
            line.push_str(message);
        }
        // The message is server text of any length; cut it, not the number.
        let mut line = truncate_to(line, MAX_PROGRESS_TEXT);
        if let Some(pct) = latest.percentage {
            line.push_str(&format!(" {pct}%"));
        }
        let others = self.tokens.len() - 1;
        if others > 0 {
            line.push_str(&format!(" (+{others} more)"));
        }
        Some(line)
    }
}

impl ServerState {
    /// Record a complaint from `source`.
    fn set_error(&mut self, source: ErrorSource, message: String) {
        // A broken transport is final; nothing the server said before it
        // can replace the explanation of why it is gone.
        if self.error_source == Some(ErrorSource::Transport) {
            return;
        }
        self.error = Some(truncate_to(message, MAX_LOG_ENTRY_BYTES));
        self.error_source = Some(source);
    }

    /// Forget the complaint, but only one `source` may withdraw.
    fn clear_error(&mut self, source: ErrorSource) {
        if self.error_source == Some(source) {
            self.error = None;
            self.error_source = None;
        }
    }

    fn push_log(&mut self, line: String) {
        let line = truncate_to(line, MAX_LOG_ENTRY_BYTES);
        self.log_bytes += line.len();
        self.log_version += 1;
        self.logs.push_back(line);
        while self.logs.len() > MAX_LOGS || self.log_bytes > MAX_LOG_TOTAL_BYTES {
            match self.logs.pop_front() {
                Some(dropped) => self.log_bytes -= dropped.len(),
                None => break,
            }
        }
    }
}

/// `s` cut to at most `max` bytes on a character boundary, marked when
/// anything was dropped so a truncated diagnostic does not read as a complete
/// one.
fn truncate_to(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str(" … (truncated)");
    s
}

/// A resolved definition target.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,      // 0-based
    pub character: usize, // 0-based, in the negotiated encoding
}

/// A node in a call hierarchy: a callable plus the raw LSP `CallHierarchyItem`,
/// kept so it can be passed back to incoming/outgoing-calls requests.
#[derive(Debug, Clone)]
pub struct CallItem {
    pub name: String,
    pub detail: String,
    pub kind: u8, // LSP SymbolKind
    pub path: PathBuf,
    pub line: usize,      // selectionRange start (0-based) — the jump target
    pub character: usize, // 0-based, negotiated encoding
    pub raw: Value,       // the CallHierarchyItem, for incoming/outgoing params
}

/// How long a call-hierarchy request may take. Some servers never answer
/// `prepareCallHierarchy` for certain symbols (observed on decorator
/// functions and multi-candidate names), which would otherwise hang the panel
/// on "Building…" for the whole of [`REQUEST_TIMEOUT`].
const CALL_HIERARCHY_TIMEOUT: Duration = Duration::from_secs(12);

/// LSP's code for a request the server dropped because the document it asks
/// about changed under it — rust-analyzer answers what is in flight with it
/// whenever a file is edited.
const CONTENT_MODIFIED: i64 = -32801;

/// LSP's code for a request cancelled — by its client, or on the server's
/// own account.
const REQUEST_CANCELLED: i64 = -32800;

/// LSP's code (3.17) for a request the server cancelled on its own account,
/// telling the client it may ask again — what a server answers what is in
/// flight with while it restarts its analysis.
const SERVER_CANCELLED: i64 = -32802;

/// Why a call-hierarchy request got no answer, told apart for a caller that
/// must not take a failure for an empty answer, and that asks again only
/// what may be answered then: the project call graph's refine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// The session is over — the server is not running, or its connection
    /// closed — and nothing it was asked will be answered.
    Gone(String),
    /// No answer in time.
    TimedOut,
    /// The server answered with an error: its code (a server that follows
    /// JSON-RPC always gives one; a session that ended with the request in
    /// flight gives none), and what it said.
    Failed { code: Option<i64>, message: String },
}

impl QueryError {
    /// Whether the same request asked again may be answered: the server
    /// dropped it because the document changed under it, or cancelled it —
    /// saying it may be asked again (`ServerCancelled`), or not — or did not
    /// answer in time. Any other error it would give again.
    pub fn transient(&self) -> bool {
        match self {
            QueryError::TimedOut => true,
            QueryError::Failed { code, .. } => {
                matches!(
                    code,
                    Some(CONTENT_MODIFIED | REQUEST_CANCELLED | SERVER_CANCELLED)
                )
            }
            QueryError::Gone(_) => false,
        }
    }
}

impl From<CallError> for QueryError {
    fn from(e: CallError) -> Self {
        match e {
            CallError::NotRunning => QueryError::Gone("server is not running".into()),
            CallError::Closed => QueryError::Gone("server closed".into()),
            CallError::TimedOut => QueryError::TimedOut,
            CallError::Failed(Failure { code, message }) => QueryError::Failed { code, message },
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Gone(why) => f.write_str(why),
            QueryError::TimedOut => f.write_str("request timed out"),
            QueryError::Failed { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for QueryError {}

/// An inlay hint: a short label the server wants shown inline at a position
/// (an inferred type after `let x`, a parameter name at a call site).
#[derive(Debug, Clone)]
pub struct InlayHint {
    pub line: usize,      // 0-based
    pub character: usize, // 0-based, in the negotiated encoding
    pub label: String,
    pub padding_left: bool,
    pub padding_right: bool,
}

/// How the server counts characters within a line — LSP's negotiated
/// `positionEncoding`. The one encoding type: the viewer's display-column
/// conversions take it too, rather than a parallel enum or a `utf16: bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    /// UTF-8 code units (bytes).
    Utf8,
    /// UTF-16 code units: astral characters (emoji, rare CJK) count two.
    Utf16,
}

impl PositionEncoding {
    /// Code units `ch` occupies in this encoding.
    pub fn units(self, ch: char) -> usize {
        match self {
            PositionEncoding::Utf8 => ch.len_utf8(),
            PositionEncoding::Utf16 => ch.len_utf16(),
        }
    }

    /// Code units `text` occupies in this encoding — the offset of the
    /// position just past it.
    pub fn units_in(self, text: &str) -> usize {
        match self {
            PositionEncoding::Utf8 => text.len(),
            PositionEncoding::Utf16 => text.chars().map(char::len_utf16).sum(),
        }
    }
}

/// Cheap, cloneable handle to a running server.
#[derive(Clone)]
pub struct LspClient {
    rpc: framing::Handle,
    state: Arc<Mutex<ServerState>>,
    pub encoding: PositionEncoding,
    /// Whether the server advertised `callHierarchyProvider` at initialize.
    pub call_hierarchy: bool,
    /// Whether the server advertised `inlayHintProvider` at initialize.
    pub inlay_hint: bool,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("encoding", &self.encoding)
            .field("alive", &self.rpc.alive())
            .finish()
    }
}

impl LspClient {
    /// Start a language server as a local child process and run the LSP
    /// `initialize` handshake.
    ///
    /// The child is the leader of its own process group and is killed on
    /// drop, so neither it nor what it spawns outlives the session; see the
    /// module docs for the stop sequence.
    pub async fn start(
        exe: &Path,
        args: &[String],
        root: &Path,
        init_options: Option<Value>,
    ) -> Result<Self, String> {
        let mut command = Command::new(exe);
        command
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|e| format!("failed to launch {}: {e}", exe.display()))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        let state = Arc::new(Mutex::new(ServerState::default()));
        // Capture the server's log output (only available for a local child).
        tokio::spawn(stderr_loop(BufReader::new(stderr), state.clone()));
        // The server is OUR child, so it may watch our pid and exit if clew
        // dies without saying goodbye.
        let process_id = Some(std::process::id());
        Self::wire_and_init(
            stdin,
            stdout,
            Some(child),
            root,
            init_options,
            state,
            process_id,
        )
        .await
    }

    /// Connect to a language server over a provided transport — its process is
    /// owned elsewhere (proxied by clew-server over its stdio). Same handshake,
    /// no local child, so the server can run where the code lives.
    ///
    /// `initialize` says `processId: null` here. The field names the process
    /// that started the server, which the server may poll and exit when it is
    /// gone: node-based servers (pyright, typescript-language-server, the
    /// vscode json/html/css servers) all do. Over a proxy the server's parent
    /// is clew-server — on a remote host, where this machine's pid is either
    /// no process at all (the server exited within seconds) or somebody
    /// else's. `null` is the spec's value for "not started by the client".
    pub async fn connect<R, W>(
        stdin: W,
        stdout: R,
        root: &Path,
        init_options: Option<Value>,
    ) -> Result<Self, String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let state = Arc::new(Mutex::new(ServerState::default()));
        Self::wire_and_init(stdin, stdout, None, root, init_options, state, None).await
    }

    /// Spawn the reader/actor tasks over `stdin`/`stdout`, run the LSP
    /// `initialize` handshake, and return the ready client.
    async fn wire_and_init<R, W>(
        stdin: W,
        stdout: R,
        child: Option<tokio::process::Child>,
        root: &Path,
        init_options: Option<Value>,
        state: Arc<Mutex<ServerState>>,
        process_id: Option<u32>,
    ) -> Result<Self, String>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        // Seed the settings we answer `workspace/configuration` pulls with, so a
        // server that pulls (pyright) still sees our initializationOptions.
        if let Ok(mut s) = state.lock() {
            s.settings = init_options.clone();
            s.folders = workspace_folders(root);
        }
        let (rpc, mailbox) = framing::mailbox();
        // Reader task: framed stdout → messages.
        let incoming = framing::spawn_reader(stdout);
        // Actor task: owns stdin + the pending map, and the child (if any)
        // until the session ends.
        tokio::spawn(run_session(stdin, mailbox, incoming, child, state.clone()));

        let client = Self {
            rpc,
            state,
            encoding: PositionEncoding::Utf16,
            call_hierarchy: false,
            inlay_hint: false,
        };

        // On failure `client` — the only handle — drops here, which ends the
        // session and takes the child down with it.
        let params = initialize_params(root, init_options, process_id);
        let result = client.call("initialize", params).await?;
        let caps = result.get("capabilities");
        let encoding = match caps
            .and_then(|c| c.get("positionEncoding"))
            .and_then(Value::as_str)
        {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        // The provider may be `true` or a (possibly empty) object; only an
        // explicit `false`/absence means unsupported.
        let call_hierarchy = caps
            .and_then(|c| c.get("callHierarchyProvider"))
            .is_some_and(|v| v != &Value::Bool(false));
        let inlay_hint = caps
            .and_then(|c| c.get("inlayHintProvider"))
            .is_some_and(|v| v != &Value::Bool(false));
        // rust-analyzer names itself, and says when it has loaded; any other
        // server gets the grace. Both run from here, before `initialized` goes
        // out: the first report it answers with is folded in after.
        let reports_status = result
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|name| name == SERVER_STATUS_REPORTER);
        if let Ok(mut s) = client.state.lock() {
            s.reports_status = reports_status;
            s.started = Some(Instant::now());
        }
        client.notify("initialized", json!({}));
        Ok(Self {
            encoding,
            call_hierarchy,
            inlay_hint,
            ..client
        })
    }

    /// Notify the server a document is open (send full text).
    pub fn did_open(&self, path: &Path, language_id: &str, version: i64, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": language_id,
                    "version": version,
                    "text": text,
                }
            }),
        );
    }

    /// Notify the server that an open document's full text changed on disk
    /// (whole-document sync). `version` must strictly increase per document.
    pub fn did_change(&self, path: &Path, version: i64, text: &str) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "version": version,
                },
                "contentChanges": [{ "text": text }],
            }),
        );
    }

    /// Notify the server that a document is no longer open: it reads the file
    /// from disk again and may drop what it held for it. A holder that keeps
    /// a bounded set of documents open closes the ones it evicts.
    pub fn did_close(&self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": path_to_uri(path) } }),
        );
    }

    /// Request the definition(s) at a 0-based (line, character) position.
    pub async fn definition(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Vec<Target>, String> {
        self.navigate("textDocument/definition", path, line, character)
            .await
    }

    /// Run any location-returning navigation request (definition, references,
    /// implementation, typeDefinition) and normalize the result to targets.
    pub async fn navigate(
        &self,
        method: &str,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Vec<Target>, String> {
        let mut params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        if method.ends_with("/references") {
            params["context"] = json!({ "includeDeclaration": false });
        }
        let result = self.call(method, params).await?;
        Ok(parse_definition(&result))
    }

    /// Request hover info at a 0-based (line, character); returns plain text.
    pub async fn hover(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Option<String>, String> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        let result = self.call("textDocument/hover", params).await?;
        Ok(parse_hover(&result))
    }

    /// Request inlay hints for the 0-based line range `[start_line, end_line)`.
    /// Empty when the server lacks the capability or the request fails.
    pub async fn inlay_hints(
        &self,
        path: &Path,
        start_line: usize,
        end_line: usize,
    ) -> Vec<InlayHint> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "range": {
                "start": { "line": start_line, "character": 0 },
                "end": { "line": end_line, "character": 0 }
            }
        });
        match self.call("textDocument/inlayHint", params).await {
            Ok(result) => parse_inlay_hints(&result),
            Err(_) => Vec::new(),
        }
    }

    /// Resolve the call-hierarchy item(s) at a position (the anchor for
    /// incoming/outgoing queries). Empty when the server lacks the capability.
    pub async fn prepare_call_hierarchy(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Vec<CallItem> {
        // A failure falls back to "no items", which the panel reports cleanly.
        self.try_prepare_call_hierarchy(path, line, character)
            .await
            .unwrap_or_default()
    }

    /// [`prepare_call_hierarchy`](Self::prepare_call_hierarchy), telling a
    /// request that failed — the server gone, an error for an answer, none in
    /// time — from an empty answer, for a caller that must not take one for
    /// the other: the project call graph's refine, whose edges would go
    /// missing without a word.
    pub async fn try_prepare_call_hierarchy(
        &self,
        path: &Path,
        line: usize,
        character: usize,
    ) -> Result<Vec<CallItem>, QueryError> {
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": character }
        });
        self.query("textDocument/prepareCallHierarchy", params)
            .await
            .map(|result| parse_call_items(&result))
    }

    /// Callers of `item` (the raw `CallHierarchyItem`).
    pub async fn incoming_calls(&self, item: Value) -> Vec<CallItem> {
        self.try_incoming_calls(item).await.unwrap_or_default()
    }

    /// [`incoming_calls`](Self::incoming_calls), telling a failed request
    /// from an empty answer (see
    /// [`try_prepare_call_hierarchy`](Self::try_prepare_call_hierarchy)).
    pub async fn try_incoming_calls(&self, item: Value) -> Result<Vec<CallItem>, QueryError> {
        self.query("callHierarchy/incomingCalls", json!({ "item": item }))
            .await
            .map(|result| parse_calls(&result, "from"))
    }

    /// Callees of `item` (the raw `CallHierarchyItem`).
    pub async fn outgoing_calls(&self, item: Value) -> Vec<CallItem> {
        self.try_outgoing_calls(item).await.unwrap_or_default()
    }

    /// [`outgoing_calls`](Self::outgoing_calls), telling a failed request
    /// from an empty answer (see
    /// [`try_prepare_call_hierarchy`](Self::try_prepare_call_hierarchy)).
    pub async fn try_outgoing_calls(&self, item: Value) -> Result<Vec<CallItem>, QueryError> {
        self.query("callHierarchy/outgoingCalls", json!({ "item": item }))
            .await
            .map(|result| parse_calls(&result, "to"))
    }

    /// A call-hierarchy request, in the tighter time box those take
    /// ([`CALL_HIERARCHY_TIMEOUT`]), failing with why ([`QueryError`]).
    async fn query(&self, method: &str, params: Value) -> Result<Value, QueryError> {
        self.rpc
            .call(method, params, CALL_HIERARCHY_TIMEOUT)
            .await
            .map_err(QueryError::from)
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        // Every request is time-boxed: a server that never answers must not
        // hang its caller (a "Searching…" status forever) nor leave the
        // pending entry behind — the actor is told to release it.
        // Call-hierarchy requests take a tighter box (see `query`).
        self.rpc
            .call(method, params, REQUEST_TIMEOUT)
            .await
            .map_err(|e| match e {
                CallError::NotRunning => "server is not running".to_string(),
                CallError::Closed => "server closed".to_string(),
                CallError::TimedOut => format!("{method}: timed out"),
                CallError::Failed(failure) => failure.message,
            })
    }

    fn notify(&self, method: &str, params: Value) {
        self.rpc.notify(method, params);
    }

    /// Whether the server is still reachable (its actor loop is running).
    /// False once the process died, its transport closed or broke, or the
    /// session was stopped — every request would fail, so holders should
    /// discard this client and restart.
    pub fn alive(&self) -> bool {
        self.rpc.alive()
    }

    /// End the session without waiting: the server is sent `shutdown` and
    /// `exit`, and a local child that does not leave in time is killed with
    /// its process group. Dropping the last handle does the same.
    pub fn stop(&self) {
        self.rpc.stop();
    }

    /// End the session (as [`Self::stop`]) and wait until the server is gone.
    /// Bounded: every step has a deadline of its own — a write the server is
    /// not taking included (see [`framing::run`]) — and this waits no longer
    /// than their sum.
    pub async fn shutdown(&self) {
        self.rpc.stop_and_wait(STOP_LIMIT).await;
    }

    /// Everything a view shows about this server — progress, busy, error,
    /// the diagnostics of every file — read under one lock acquisition, and
    /// cheap (the diagnostics are shared, not copied). A view takes one per
    /// server per rebuild and reads every widget's data from it, rather than
    /// each widget locking the state and cloning what it needs.
    ///
    /// `Err` when the state lock is poisoned (see [`StatePoisoned`]): that is
    /// a server clew can no longer describe, not an empty one.
    pub fn snapshot(&self) -> Result<Snapshot, StatePoisoned> {
        snapshot_of(&self.state)
    }

    /// A copy of the most recent server log lines (`Err` when the state is
    /// poisoned — an unreadable log is not an empty one).
    pub fn logs(&self) -> Result<Vec<String>, StatePoisoned> {
        self.state
            .lock()
            .map(|s| s.logs.iter().cloned().collect())
            .map_err(|_| StatePoisoned)
    }

    /// The last `n` log lines, oldest first — what a view shows, copied
    /// without the rest of the buffer.
    pub fn log_tail(&self, n: usize) -> Result<Vec<String>, StatePoisoned> {
        self.state
            .lock()
            .map(|s| {
                let skip = s.logs.len().saturating_sub(n);
                s.logs.iter().skip(skip).cloned().collect()
            })
            .map_err(|_| StatePoisoned)
    }

    /// Current work-done progress line (e.g. "Indexing 45%"), if any report
    /// is in flight. With several running, the most recently updated one is
    /// shown with a count of the rest.
    pub fn progress(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.progress.summary())
    }

    /// Whether the server says it is still working: any progress report in
    /// flight, or (rust-analyzer) a workspace that is not loaded yet. An
    /// empty answer from a busy server may be a false negative.
    pub fn busy(&self) -> bool {
        self.state
            .lock()
            .is_ok_and(|s| s.progress.busy() || s.quiescent == Some(false))
    }

    /// Whether the server is still loading the project: rust-analyzer's
    /// workspace not loaded yet, or — from a server that does not say that —
    /// work reported before it was first idle. A server just started is
    /// loading until it has said anything of its work, for a short grace
    /// ([`LOAD_GRACE`]; rust-analyzer: until its first status). Unlike
    /// [`Self::busy`], work it reports once it has loaded (a check after a
    /// save) is not loading. A server that is loading answers from what it
    /// has loaded so far: a call hierarchy with callers missing, or an
    /// error for a file it has not read yet.
    pub fn loading(&self) -> bool {
        self.state.lock().is_ok_and(|s| s.loading(Instant::now()))
    }

    /// The server's current complaint (e.g. "failed to load workspace"), if
    /// any — the server is running but something is wrong. Cleared again when
    /// it recovers. A poisoned state is a complaint too ([`StatePoisoned`]):
    /// it must not read as a healthy server with nothing to say.
    pub fn error(&self) -> Option<String> {
        match self.state.lock() {
            Ok(s) => s.error.clone(),
            Err(_) => Some(StatePoisoned.to_string()),
        }
    }

    /// Diagnostics for a file (empty if none, and when the state is
    /// poisoned — a view reads them from [`Self::snapshot`], which says so).
    pub fn diagnostics(&self, path: &Path) -> Vec<Diag> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.diagnostics.get(path).map(|d| d.to_vec()))
            .unwrap_or_default()
    }

    /// Monotonic version bumped whenever any diagnostics change.
    pub fn diag_version(&self) -> u64 {
        self.state.lock().map(|s| s.diag_version).unwrap_or(0)
    }

    /// The inlay-hint refresh epoch (bumped when the server requests a refresh).
    pub fn inlay_epoch(&self) -> u64 {
        self.state.lock().map(|s| s.inlay_epoch).unwrap_or(0)
    }
}

/// The `initialize` request's params. `process_id` is our pid for a server
/// we spawned, `None` (sent as `null`) for one whose parent is someone else —
/// see [`LspClient::connect`].
fn initialize_params(root: &Path, init_options: Option<Value>, process_id: Option<u32>) -> Value {
    let mut params = json!({
        "processId": process_id,
        "rootUri": path_to_uri(root),
        // The root again, as the one workspace folder. pyright only looks
        // for a function's callers in other files within its workspace
        // folders: opened on `rootUri` alone it answered every
        // `callHierarchy/incomingCalls` with null.
        "workspaceFolders": workspace_folders(root),
        "capabilities": {
            // Prefer utf-8 so our byte offsets map 1:1 to LSP positions.
            "general": { "positionEncodings": ["utf-8", "utf-16"] },
            // We accept server-initiated progress (`window/workDoneProgress/
            // create` is acknowledged, `$/progress` tracked per token). Without
            // this servers such as rust-analyzer report no progress at all,
            // and "still indexing" is indistinguishable from "no answer".
            "window": { "workDoneProgress": true },
            "workspace": {
                // We answer settings pulls (pyright uses this for the venv).
                "configuration": true,
                // We name the project root as the one workspace folder, and
                // answer `workspace/workspaceFolders` with it.
                "workspaceFolders": true,
                // We re-request inlay hints when the server asks.
                "inlayHint": { "refreshSupport": true }
            },
            "textDocument": {
                "definition": { "linkSupport": true },
                // Declare inlay-hint support so the server provides them.
                "inlayHint": { "dynamicRegistration": false },
                // Declare call-hierarchy support. Some servers (e.g.
                // typescript-language-server) only advertise
                // `callHierarchyProvider` when the client asks for it;
                // gopls advertises it unconditionally. Without this, call
                // hierarchy was reported "unsupported" for JS/TS.
                "callHierarchy": { "dynamicRegistration": false }
            },
            // rust-analyzer: report health and readiness through
            // `experimental/serverStatus`, which — unlike an error-level
            // `showMessage` — also says when the problem is gone.
            "experimental": { "serverStatusNotification": true }
        },
        "clientInfo": { "name": "clew" }
    });
    if let Some(opts) = init_options {
        params["initializationOptions"] = opts;
    }
    params
}

/// The project root as the one LSP workspace folder: its URI, named after
/// its last component.
fn workspace_folders(root: &Path) -> Value {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_else(|| root.to_string_lossy());
    json!([{ "uri": path_to_uri(root), "name": name }])
}

/// The actor for one session, then the child's end: a server that was asked
/// to leave gets [`EXIT_GRACE`]; one whose transport broke — it broke the
/// protocol, or stopped reading its input — gets none.
async fn run_session<W>(
    stdin: W,
    mailbox: framing::Mailbox,
    incoming: tokio::sync::mpsc::UnboundedReceiver<framing::Inbound>,
    child: Option<tokio::process::Child>,
    state: Arc<Mutex<ServerState>>,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let ended = framing::run(stdin, LspWire { state }, mailbox, incoming).await;
    if let Some(child) = child {
        let grace = match ended.reason {
            EndReason::Broken(_) => Duration::ZERO,
            _ => EXIT_GRACE,
        };
        framing::reap(child, grace, cfg!(unix)).await;
    }
    ended.release();
}

/// Stderr task: capture the server's log output into shared state.
async fn stderr_loop<R>(mut reader: BufReader<R>, state: Arc<Mutex<ServerState>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // Cap the read the way the framing reader caps a header line.
        // `read_line` appends until a newline arrives, so a server writing megabytes
        // without one grew this buffer for as long as it kept writing, and
        // the retention cap could not help: it only ever saw whole lines.
        //
        // Unlike a header, an over-long stderr run is no reason to stop
        // logging — there is no framing to lose sync with — so the rest of it
        // is consumed as further capped chunks. Read as BYTES, since a chunk
        // boundary can fall inside a character and stderr is not required to
        // be UTF-8 at all.
        match (&mut reader)
            .take(MAX_LOG_ENTRY_BYTES as u64)
            .read_until(b'\n', &mut buf)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf);
        let trimmed = text.trim_end();
        if !trimmed.is_empty()
            && let Ok(mut s) = state.lock()
        {
            s.push_log(trimmed.to_string());
        }
    }
}

/// What one inbound JSON-RPC message is, once its `id` and `method` are read
/// together. Split out from the actor loop so the dispatch rule is testable
/// without a live server.
enum Inbound<'a> {
    /// A response to a request WE sent. Our ids are always integers.
    Response(i64),
    /// A server→client request, which must be answered with this exact id.
    Request { id: Value, method: &'a str },
    /// A server→client notification (no id, so no reply).
    Notification(&'a str),
    /// Neither — nothing to do.
    Ignored,
}

/// Classify an inbound message.
///
/// The spec allows an id to be "a String, Number, or NULL". Accepting only
/// integers made a request that used a STRING id — `workspace/configuration`
/// with `"id": "cfg-1"` is a real shape — look like a notification, so it was
/// never answered and the server blocked waiting for a reply that could not
/// come.
fn classify(value: &Value) -> Inbound<'_> {
    let id = value.get("id").filter(|v| !v.is_null());
    let method = value.get("method").and_then(Value::as_str);
    match (id, method) {
        (Some(id), None) => match id.as_i64() {
            Some(id) => Inbound::Response(id),
            // A response to an id we could not have sent.
            None => Inbound::Ignored,
        },
        (Some(id), Some(method)) => Inbound::Request {
            id: id.clone(),
            method,
        },
        (None, Some(method)) => Inbound::Notification(method),
        (None, None) => Inbound::Ignored,
    }
}

/// The JSON-RPC half of a session's actor (see [`framing::run`]): envelopes,
/// response routing, the server's own requests, and its notifications.
struct LspWire {
    state: Arc<Mutex<ServerState>>,
}

/// A JSON-RPC message. `params` is left out when there are none: the spec
/// lets it be omitted but, if present, requires an object or array — `null`
/// is not a legal value for it (`shutdown` and `exit` take no params).
fn envelope(id: Option<i64>, method: &str, params: Value) -> Value {
    let mut msg = json!({ "jsonrpc": "2.0", "method": method });
    if let Some(id) = id {
        msg["id"] = json!(id);
    }
    if !params.is_null() {
        msg["params"] = params;
    }
    msg
}

/// Whether a result carries actual content. `null` and `[]` are what a
/// server that cannot see the workspace answers everything with, so they
/// prove nothing about recovery.
fn has_content(result: &Value) -> bool {
    match result {
        Value::Null => false,
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

impl framing::Protocol for LspWire {
    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        envelope(Some(id), method, params)
    }

    fn notification(&mut self, _id: i64, method: &str, params: Value) -> Value {
        envelope(None, method, params)
    }

    fn inbound(&mut self, value: Value, _ids: &Ids) -> Handled {
        match classify(&value) {
            // A response to one of our requests.
            Inbound::Response(id) => {
                let result = match value.get("error") {
                    Some(err) => Err(Failure {
                        code: err.get("code").and_then(Value::as_i64),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("error")
                            .to_string(),
                    }),
                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                };
                // Real content is the recovery signal an error-level
                // `showMessage` otherwise never gets.
                if matches!(&result, Ok(v) if has_content(v))
                    && let Ok(mut s) = self.state.lock()
                {
                    s.clear_error(ErrorSource::ShowMessage);
                }
                Handled::Response { id, result }
            }
            // A server→client request. We answer the config pull (so pyright
            // et al. get our settings) and the workspace folders, and
            // acknowledge everything else with a null result.
            Inbound::Request { id, method } => {
                let result = if method == "workspace/configuration" {
                    let settings = self.state.lock().ok().and_then(|s| s.settings.clone());
                    configuration_response(settings.as_ref(), value.get("params"))
                } else if method == "workspace/workspaceFolders" {
                    self.state
                        .lock()
                        .map(|s| s.folders.clone())
                        .unwrap_or(Value::Null)
                } else {
                    // The server asks us to re-pull inlay hints once they're
                    // ready; bump the epoch the UI watches.
                    if method == "workspace/inlayHint/refresh"
                        && let Ok(mut s) = self.state.lock()
                    {
                        s.inlay_epoch = s.inlay_epoch.wrapping_add(1);
                    }
                    Value::Null
                };
                // `id` echoed VERBATIM: it is whatever JSON value the server
                // chose, and a reply carrying a different one answers nothing.
                Handled::Reply(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
            // A server notification (logs, progress, diagnostics).
            Inbound::Notification(method) => {
                handle_notification(method, &value, &self.state);
                Handled::Nothing
            }
            Inbound::Ignored => Handled::Nothing,
        }
    }

    fn frame_error(&mut self, error: &FrameError) {
        if let Ok(mut s) = self.state.lock() {
            if error.is_fatal() {
                s.push_log(format!(
                    "[clew] the connection to the server broke: {error}"
                ));
            } else {
                s.push_log(format!("[clew] skipped a message from the server: {error}"));
            }
        }
    }

    fn goodbye(&mut self) -> Option<Goodbye> {
        Some(Goodbye {
            method: "shutdown",
            params: Value::Null,
            reply_wait: SHUTDOWN_REPLY_WAIT,
            then: Some(("exit", Value::Null)),
        })
    }

    /// `$/cancelRequest`: a request asked again after a timeout would
    /// otherwise stack a second copy on a server still at work on the first.
    fn cancel(&mut self, id: i64) -> Option<Value> {
        Some(envelope(None, "$/cancelRequest", json!({ "id": id })))
    }

    fn ended(&mut self, reason: &EndReason) -> String {
        match reason {
            EndReason::Broken(error) => {
                let message = format!("the connection to the server broke: {error}");
                if let Ok(mut s) = self.state.lock() {
                    s.set_error(ErrorSource::Transport, message.clone());
                }
                message
            }
            _ => "server stopped".to_string(),
        }
    }
}

/// Build the `workspace/configuration` reply: one entry per requested item,
/// each the settings sub-tree named by its dotted `section` (or the whole
/// settings object when no section is given), and `null` for anything absent.
fn configuration_response(settings: Option<&Value>, params: Option<&Value>) -> Value {
    let Some(items) = params
        .and_then(|p| p.get("items"))
        .and_then(Value::as_array)
    else {
        return Value::Array(Vec::new());
    };
    let out: Vec<Value> = items
        .iter()
        .map(
            |item| match (settings, item.get("section").and_then(Value::as_str)) {
                (Some(s), Some(section)) => section_value(s, section),
                (Some(s), None) => s.clone(),
                (None, _) => Value::Null,
            },
        )
        .collect();
    Value::Array(out)
}

/// Navigate `settings` by a dotted section path (`"python.analysis"`), or `null`
/// if any segment is missing.
fn section_value(settings: &Value, dotted: &str) -> Value {
    let mut cur = settings;
    for part in dotted.split('.') {
        match cur.get(part) {
            Some(v) => cur = v,
            None => return Value::Null,
        }
    }
    cur.clone()
}

/// Fold a server notification into the shared state.
fn handle_notification(method: &str, value: &Value, state: &Arc<Mutex<ServerState>>) {
    let params = value.get("params");
    let Ok(mut s) = state.lock() else {
        return;
    };
    match method {
        "window/logMessage" | "window/showMessage" => {
            if let Some(msg) = params
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
            {
                s.push_log(msg.to_string());
                if method == "window/showMessage" {
                    // Type 1 (Error) is how a server tells the user something
                    // is wrong (gopls: "error loading workspace"). Remember it
                    // so the status bar can flag it; a later Info (3) is the
                    // closest thing to "fixed" the protocol offers.
                    match params.and_then(|p| p.get("type")).and_then(Value::as_u64) {
                        Some(1) => s.set_error(ErrorSource::ShowMessage, msg.to_string()),
                        Some(3) => s.clear_error(ErrorSource::ShowMessage),
                        _ => {}
                    }
                }
            }
        }
        // rust-analyzer's health report (we opt in at `initialize`). It
        // replaces the error-level `showMessage` for workspace problems, and
        // unlike that message it also says when they are gone.
        "experimental/serverStatus" => {
            let health = params.and_then(|p| p.get("health")).and_then(Value::as_str);
            let message = params
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty());
            if let Some(message) = message {
                s.push_log(message.to_string());
            }
            match health {
                Some("error") => s.set_error(
                    ErrorSource::ServerStatus,
                    message
                        .unwrap_or("the server reported an error")
                        .to_string(),
                ),
                // A warning still means the error is over; the warning itself
                // is in the log.
                Some("ok") | Some("warning") => s.clear_error(ErrorSource::ServerStatus),
                _ => {}
            }
            s.reports += 1;
            if let Some(quiescent) = params
                .and_then(|p| p.get("quiescent"))
                .and_then(Value::as_bool)
            {
                s.quiescent = Some(quiescent);
                s.note_idle();
            }
        }
        "textDocument/publishDiagnostics" => {
            let Some(uri) = params.and_then(|p| p.get("uri")).and_then(Value::as_str) else {
                return;
            };
            let Some(path) = uri_to_path(uri) else {
                return;
            };
            let diags = params
                .and_then(|p| p.get("diagnostics"))
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(parse_diag).collect())
                .unwrap_or_default();
            // Copy-on-write: a snapshot still holding the previous map keeps
            // it; nobody else pays for the update.
            let diags: Vec<Diag> = diags;
            Arc::make_mut(&mut s.diagnostics).insert(path, diags.into());
            s.diag_version = s.diag_version.wrapping_add(1);
        }
        "$/progress" => {
            // Idle through the whole grace: loaded before this report, which
            // may begin work on the project as it changes.
            s.note_quiet(Instant::now());
            s.progress.update(params);
            s.reports += 1;
            s.note_idle();
        }
        _ => {}
    }
}

/// The largest position a language server may send: LSP's `uinteger` is
/// `0..=2^31 - 1`. A bigger number is out of spec, and is clamped here, at the
/// parse, so every consumer can do plain arithmetic on a position — the GUI
/// shows `line + 1` and jumps to it in several places, and a `u64::MAX` (well
/// formed JSON) overflowed each of them.
const MAX_LSP_UINT: u64 = i32::MAX as u64;

/// A position field (`line`, `character`), clamped to [`MAX_LSP_UINT`].
/// `None` when it is absent or not a non-negative integer.
fn lsp_uint(v: Option<&Value>) -> Option<usize> {
    v?.as_u64().map(|n| n.min(MAX_LSP_UINT) as usize)
}

/// Normalize a definition response (Location | Location[] | LocationLink[]).
fn parse_definition(result: &Value) -> Vec<Target> {
    fn one(v: &Value) -> Option<Target> {
        // LocationLink uses targetUri/targetSelectionRange; Location uses uri/range.
        let uri = v
            .get("uri")
            .or_else(|| v.get("targetUri"))
            .and_then(Value::as_str)?;
        let range = v
            .get("range")
            .or_else(|| v.get("targetSelectionRange"))
            .or_else(|| v.get("targetRange"))?;
        let start = range.get("start")?;
        Some(Target {
            path: uri_to_path(uri)?,
            line: lsp_uint(start.get("line"))?,
            character: lsp_uint(start.get("character"))?,
        })
    }
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Object(_) => one(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Parse one `CallHierarchyItem` into a `CallItem` (keeping the raw JSON).
fn parse_call_item(v: &Value) -> Option<CallItem> {
    let uri = v.get("uri").and_then(Value::as_str)?;
    // Jump to the name (selectionRange), not the whole definition range.
    let start = v
        .get("selectionRange")
        .or_else(|| v.get("range"))?
        .get("start")?;
    Some(CallItem {
        name: v
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string(),
        detail: v
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        kind: v.get("kind").and_then(Value::as_u64).unwrap_or(0) as u8,
        path: uri_to_path(uri)?,
        line: lsp_uint(start.get("line"))?,
        character: lsp_uint(start.get("character"))?,
        raw: v.clone(),
    })
}

/// `prepareCallHierarchy` returns `CallHierarchyItem[] | null`.
fn parse_call_items(result: &Value) -> Vec<CallItem> {
    match result {
        Value::Array(items) => items.iter().filter_map(parse_call_item).collect(),
        _ => Vec::new(),
    }
}

/// `incomingCalls` returns `[{ from: item, .. }]`, `outgoingCalls` `[{ to: item }]`.
/// De-duplicates so a caller/callee that appears via several call sites is one row.
fn parse_calls(result: &Value, field: &str) -> Vec<CallItem> {
    let Value::Array(items) = result else {
        return Vec::new();
    };
    let mut out: Vec<CallItem> = Vec::new();
    for call in items {
        if let Some(item) = call.get(field).and_then(parse_call_item)
            && !out
                .iter()
                .any(|e| e.path == item.path && e.line == item.line && e.name == item.name)
        {
            out.push(item);
        }
    }
    out
}

/// Parse one LSP diagnostic. Multi-line diagnostics are clamped to the start
/// line for underlining.
///
/// Every position is the server's number, so the arithmetic saturates: this
/// runs under the state lock, and an overflow panic there (a `character` of
/// `u64::MAX` is well-formed JSON) would poison the state for good.
fn parse_diag(v: &Value) -> Option<Diag> {
    let range = v.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let line = lsp_uint(start.get("line"))?;
    let char_start = lsp_uint(start.get("character"))?;
    let one_past_start = char_start.saturating_add(1);
    let end_line = lsp_uint(end.get("line")).unwrap_or(line);
    let char_end = if end_line == line {
        lsp_uint(end.get("character")).unwrap_or(one_past_start)
    } else {
        one_past_start // spans to next line; underline just the start
    };
    Some(Diag {
        line,
        char_start,
        char_end: char_end.max(one_past_start),
        severity: v.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
        message: v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

/// Extract plain text from a hover response (MarkupContent | MarkedString[]).
fn parse_hover(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let text = match contents {
        // MarkupContent { kind, value } or MarkedString { language, value }.
        Value::Object(o) => o.get("value").and_then(Value::as_str)?.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|e| {
                    e.get("value")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                        .map(str::to_string)
                })
                .collect();
            if parts.is_empty() {
                return None;
            }
            parts.join("\n")
        }
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_inlay_hints(result: &Value) -> Vec<InlayHint> {
    let Some(arr) = result.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|h| {
            let pos = h.get("position")?;
            let label = parse_inlay_label(h.get("label")?);
            if label.is_empty() {
                return None;
            }
            Some(InlayHint {
                line: lsp_uint(pos.get("line"))?,
                character: lsp_uint(pos.get("character"))?,
                label,
                padding_left: h
                    .get("paddingLeft")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                padding_right: h
                    .get("paddingRight")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// An inlay label is either a plain string or an array of `{ value, … }` parts.
fn parse_inlay_label(label: &Value) -> String {
    match label {
        Value::String(s) => s.trim().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("value").and_then(Value::as_str))
            .collect::<String>()
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().into_owned();
    // Only Windows spells a separator `\`. On unix it is an ordinary filename
    // character, and rewriting it there turned `back\slash.rs` into a URI
    // naming a file in a `back/` directory.
    #[cfg(windows)]
    let s = s.replace('\\', "/");
    // A UNC path (`\\host\share\…`, now `//host/share/…`) names its host in
    // the URI's AUTHORITY. Spelling it as part of the path produced
    // `file:////host/share/…`, which is not the form any server resolves back.
    #[cfg(windows)]
    let (authority, s) = match s.strip_prefix("//") {
        Some(rest) => match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        },
        None => (String::new(), s),
    };
    #[cfg(not(windows))]
    let authority = "";
    let s = if s.starts_with('/') {
        s
    } else {
        format!("/{s}")
    };
    // Percent-encode by RULE, not by a whitelist of characters that happen to
    // be awkward. The whitelist left `%` itself untouched, so a file named
    // `a%20b.rs` produced the URI of `a b.rs` and every request about it —
    // didOpen, definition, references — silently addressed the wrong file.
    //
    // What stays literal is what RFC 3986 allows in a path segment (unreserved
    // + sub-delims + `:` + `@`), plus `/` as the separator. Everything else,
    // including non-ASCII, is encoded byte by byte.
    let mut encoded = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'/'
            | b':'
            | b'@'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b';'
            | b'=' => encoded.push(b as char),
            _ => encoded.push_str(&format!("%{b:02X}")),
        }
    }
    format!("file://{authority}{encoded}")
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file://<authority>/<path>`. The authority is empty for a local file,
    // and `localhost` means the same thing. Split it off properly rather than
    // stripping the literal "localhost": doing that left a REAL host glued to
    // the front of the path, and with no leading `/` the result was a relative
    // path addressing some arbitrary file under the working directory.
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let decoded = percent_decode(path);
    if authority.is_empty() || authority.eq_ignore_ascii_case("localhost") {
        return Some(local_path(&decoded));
    }
    // Another host. Windows spells that as a UNC path; unix cannot address it
    // at all, and inventing a local path for it would silently open the wrong
    // file.
    #[cfg(windows)]
    {
        let host = percent_decode(authority);
        Some(PathBuf::from(format!(
            "\\\\{host}{}",
            decoded.replace('/', "\\")
        )))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The decoded path component of a local `file://` URI, as this platform
/// spells a path.
#[cfg(windows)]
fn local_path(decoded: &str) -> PathBuf {
    // `file:///C:/work/a.rs`: the leading slash is URI syntax, not part of the
    // path, and keeping it produced `/C:/work/a.rs` — a path with no drive
    // prefix, which compares equal to nothing the editor holds. `C|` is the
    // legacy spelling of the drive separator.
    let body = decoded.strip_prefix('/').unwrap_or(decoded);
    let mut chars = body.chars();
    let is_drive = matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':' | '|')) if letter.is_ascii_alphabetic()
    );
    if !is_drive {
        return PathBuf::from(decoded.replace('/', "\\"));
    }
    let mut body = body.to_string();
    body.replace_range(1..2, ":"); // the letter and the separator are 1 byte each
    PathBuf::from(body.replace('/', "\\"))
}

#[cfg(not(windows))]
fn local_path(decoded: &str) -> PathBuf {
    PathBuf::from(decoded)
}

/// Decode `%XX` escapes. Anything that is not a complete escape is kept
/// literally, so a path a server sent unencoded still round-trips.
///
/// The two digits are read as BYTES. Slicing the `&str` (`&s[i + 1..i + 3]`)
/// is the obvious spelling and it PANICS: a literal `%` followed within one
/// byte by a 3- or 4-byte character puts the slice's end inside that
/// character. `file:///tmp/%日本.rs` is a legal thing for a server to send,
/// and it took the LSP task down with it.
fn percent_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_roundtrip() {
        let p = PathBuf::from("/Users/x/my code/main.rs");
        let uri = path_to_uri(&p);
        assert_eq!(uri, "file:///Users/x/my%20code/main.rs");
        assert_eq!(uri_to_path(&uri), Some(p));
    }

    /// A literal `%` has to become `%25`. Left raw, the server decoded the
    /// URI back to a DIFFERENT file — `percent%20name.rs` addressed
    /// `percent name.rs` — so didOpen, definition and references all worked on
    /// the wrong document.
    #[test]
    fn uri_encodes_a_literal_percent() {
        let p = PathBuf::from("/tmp/percent%20name.rs");
        let uri = path_to_uri(&p);
        assert_eq!(uri, "file:///tmp/percent%2520name.rs");
        assert_eq!(uri_to_path(&uri), Some(p));
    }

    /// Everything outside the RFC 3986 path-segment set round-trips, and the
    /// characters that set allows are left alone.
    #[test]
    fn uri_roundtrips_awkward_names() {
        for raw in [
            "/tmp/a#b.rs",
            "/tmp/a?b.rs",
            "/tmp/a b%c#d?e.rs",
            "/tmp/[brackets].rs",
            "/tmp/back\\slash.rs",
            "/tmp/日本語/файл.rs",
            "/tmp/plus+and,comma;semi=eq(paren).rs",
        ] {
            let p = PathBuf::from(raw);
            let uri = path_to_uri(&p);
            assert!(
                !uri["file://".len()..].contains(['#', '?', ' ']),
                "unencoded delimiter in {uri}"
            );
            assert_eq!(uri_to_path(&uri), Some(p), "round-trip of {raw} via {uri}");
        }
    }

    /// A `%` a server left unencoded must not take the LSP task down. The
    /// digits are read as bytes precisely so the two after it can be the
    /// start of a multi-byte character.
    #[test]
    #[cfg(not(windows))] // asserts the unix spelling of the decoded path
    fn a_malformed_escape_decodes_instead_of_panicking() {
        for uri in [
            "file:///tmp/100%日本.rs", // 3-byte char one byte after `%`
            "file:///tmp/50%🎉.rs",    // 4-byte char
            "file:///tmp/a%",          // truncated at the end
            "file:///tmp/b%A",         // one digit only
            "file:///tmp/c%zz.rs",     // not hex
            "file:///tmp/d%+1.rs",     // `from_str_radix` used to take this
        ] {
            let got = uri_to_path(uri).expect("every input starts with file://");
            assert_eq!(
                got,
                PathBuf::from(uri.trim_start_matches("file://")),
                "incomplete escape must stay literal in {uri}"
            );
        }
    }

    /// An entry count is not a memory bound. Both feeds are the server's own
    /// output, and one `window/logMessage` body may be tens of megabytes —
    /// which the management panel then clones in full on every render.
    #[test]
    fn the_log_buffer_is_bounded_by_bytes_not_just_entries() {
        let mut s = ServerState::default();
        for _ in 0..MAX_LOGS {
            s.push_log("x".repeat(MAX_LOG_ENTRY_BYTES * 4));
        }
        assert!(
            s.logs.iter().all(|l| l.len() <= MAX_LOG_ENTRY_BYTES + 32),
            "an entry was kept whole"
        );
        let total: usize = s.logs.iter().map(String::len).sum();
        assert!(total <= MAX_LOG_TOTAL_BYTES, "{total} bytes retained");
        assert!(s.logs.back().unwrap().ends_with("(truncated)"));

        // Short lines are kept verbatim, and the count cap still applies.
        let mut s = ServerState::default();
        for i in 0..MAX_LOGS + 10 {
            s.push_log(format!("line {i}"));
        }
        assert_eq!(s.logs.len(), MAX_LOGS);
        assert_eq!(s.logs.front().unwrap(), "line 10");
    }

    /// Every pushed line moves the log's version — a view re-copies the log
    /// only then — and a snapshot carries it.
    #[test]
    fn the_log_version_moves_with_every_line() {
        let mut s = ServerState::default();
        assert_eq!(s.snapshot().log_version, 0);
        s.push_log("one".into());
        s.push_log("two".into());
        assert_eq!(s.snapshot().log_version, 2);
    }

    /// The authority is a host, not a prefix of the path. An empty one and
    /// `localhost` both mean this machine.
    #[test]
    fn the_authority_is_parsed_as_a_host() {
        let want = Some(PathBuf::from("/tmp/a.rs"));
        assert_eq!(uri_to_path("file:///tmp/a.rs"), want);
        assert_eq!(uri_to_path("file://localhost/tmp/a.rs"), want);
        assert_eq!(uri_to_path("file://LOCALHOST/tmp/a.rs"), want);
        // A file named `localhostile.rs` must not lose its first nine letters.
        assert_eq!(
            uri_to_path("file:///localhostile.rs"),
            Some(PathBuf::from("/localhostile.rs"))
        );
    }

    /// A real remote host has no unix path. Folding it into the path produced
    /// the RELATIVE path `srv/share/a.rs`, addressing whatever sat under the
    /// working directory.
    #[test]
    #[cfg(not(windows))]
    fn a_remote_authority_is_refused_rather_than_guessed() {
        assert_eq!(uri_to_path("file://srv/share/a.rs"), None);
    }

    /// Windows spells a drive and a UNC share in ways the URI does not.
    #[test]
    #[cfg(windows)]
    fn windows_drive_and_unc_round_trip() {
        for raw in ["C:\\work\\a.rs", "\\\\srv\\share\\a.rs"] {
            let p = PathBuf::from(raw);
            assert_eq!(
                uri_to_path(&path_to_uri(&p)),
                Some(p),
                "round-trip of {raw}"
            );
        }
        assert_eq!(
            path_to_uri(&PathBuf::from("C:\\work\\a.rs")),
            "file:///C:/work/a.rs"
        );
        assert_eq!(
            path_to_uri(&PathBuf::from("\\\\srv\\share\\a.rs")),
            "file://srv/share/a.rs"
        );
        // The forms other editors emit: a percent-encoded colon, and the
        // legacy `|` separator.
        let want = Some(PathBuf::from("C:\\work\\a.rs"));
        assert_eq!(
            uri_to_path("file:///c%3A/work/a.rs"),
            Some(PathBuf::from("c:\\work\\a.rs"))
        );
        assert_eq!(uri_to_path("file:///C|/work/a.rs"), want);
    }

    #[test]
    fn configuration_reply_maps_each_requested_section() {
        let settings = json!({
            "python": { "pythonPath": "/venv/bin/python", "analysis": { "level": "basic" } }
        });
        let params = json!({ "items": [
            { "section": "python" },
            { "section": "python.analysis" },
            { "section": "rust-analyzer" },  // absent → null
            {},                               // no section → whole settings
        ]});
        let reply = configuration_response(Some(&settings), Some(&params));
        let arr = reply.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["pythonPath"], "/venv/bin/python");
        assert_eq!(arr[1]["level"], "basic");
        assert_eq!(arr[2], Value::Null);
        assert_eq!(arr[3], settings);
    }

    #[test]
    fn parse_inlay_hints_string_and_parts() {
        let v = json!([
            { "position": {"line": 3, "character": 9}, "label": ": i32", "paddingLeft": true },
            { "position": {"line": 5, "character": 2}, "label": [{"value": "count"}, {"value": ":"}] },
            { "position": {"line": 7, "character": 0}, "label": "" }  // empty → dropped
        ]);
        let hints = parse_inlay_hints(&v);
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].line, 3);
        assert_eq!(hints[0].character, 9);
        assert_eq!(hints[0].label, ": i32");
        assert!(hints[0].padding_left);
        assert_eq!(hints[1].label, "count:");
    }

    #[test]
    fn configuration_reply_is_null_per_item_without_settings() {
        let params = json!({ "items": [{ "section": "python" }, { "section": "go" }] });
        let reply = configuration_response(None, Some(&params));
        assert_eq!(reply, json!([Value::Null, Value::Null]));
    }

    #[test]
    fn parse_location_object() {
        let v = json!({
            "uri": "file:///a/b.rs",
            "range": { "start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 12} }
        });
        assert_eq!(
            parse_definition(&v),
            vec![Target {
                path: PathBuf::from("/a/b.rs"),
                line: 4,
                character: 8
            }]
        );
    }

    #[test]
    fn parse_location_link_array() {
        let v = json!([{
            "targetUri": "file:///a/b.rs",
            "targetSelectionRange": { "start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5} },
            "targetRange": { "start": {"line": 0, "character": 0}, "end": {"line": 3, "character": 0} }
        }]);
        assert_eq!(parse_definition(&v)[0].line, 1);
        assert_eq!(parse_definition(&v)[0].character, 2);
    }

    #[test]
    fn parse_null_is_empty() {
        assert!(parse_definition(&Value::Null).is_empty());
    }

    #[test]
    fn parse_diagnostic() {
        let v = json!({
            "range": {"start": {"line": 4, "character": 8}, "end": {"line": 4, "character": 13}},
            "severity": 1,
            "message": "cannot find value"
        });
        let d = parse_diag(&v).unwrap();
        assert_eq!(
            (d.line, d.char_start, d.char_end, d.severity),
            (4, 8, 13, 1)
        );
        assert!(d.message.contains("cannot find"));
    }

    /// F9: positions are the server's numbers. `character + 1` on
    /// `u64::MAX` panicked (in a debug build) under the state lock, poisoning
    /// it for the rest of the session; the arithmetic now saturates.
    #[test]
    fn extreme_diagnostic_positions_do_not_overflow() {
        let max = u64::MAX;
        let top = MAX_LSP_UINT as usize;
        for range in [
            json!({"start": {"line": 1, "character": max}, "end": {"line": 1, "character": max}}),
            json!({"start": {"line": 1, "character": max}, "end": {"line": 2, "character": 0}}),
            json!({"start": {"line": max, "character": max}, "end": {"line": max}}),
        ] {
            let d = parse_diag(&json!({"range": range, "message": "m"})).unwrap();
            assert_eq!(d.char_start, top);
            assert_eq!(d.char_end, top + 1, "{range}");
        }
    }

    /// Every position a server sends is clamped at the parse to LSP's own
    /// maximum (`2^31 - 1`), whichever response it arrives in. The GUI adds
    /// one to a line to show or jump to it in several places, and a
    /// `u64::MAX` there panicked a debug build; after the parse, no position
    /// can make that overflow.
    #[test]
    fn every_parsed_position_is_clamped_to_the_lsp_range() {
        let max = u64::MAX;
        let top = MAX_LSP_UINT as usize;
        let at = json!({"start": {"line": max, "character": max},
                        "end": {"line": max, "character": max}});
        let def = parse_definition(&json!({"uri": "file:///p/a.rs", "range": at}));
        assert_eq!((def[0].line, def[0].character), (top, top));
        let calls = parse_call_items(&json!([{
            "name": "f", "kind": 12, "uri": "file:///p/a.rs",
            "range": at, "selectionRange": at
        }]));
        assert_eq!((calls[0].line, calls[0].character), (top, top));
        let hints = parse_inlay_hints(&json!([{
            "position": {"line": max, "character": max}, "label": ": T"
        }]));
        assert_eq!((hints[0].line, hints[0].character), (top, top));
        // In range, a number is kept exactly; below zero, it is no position.
        assert_eq!(lsp_uint(Some(&json!(MAX_LSP_UINT))), Some(top));
        assert_eq!(lsp_uint(Some(&json!(7))), Some(7));
        assert_eq!(lsp_uint(Some(&json!(-1))), None);
        assert!(top.checked_add(1).is_some(), "`line + 1` cannot overflow");
    }

    #[test]
    fn parse_hover_variants() {
        // MarkupContent
        let v =
            json!({"contents": {"kind": "markdown", "value": "```rust\nfn origin() -> i32\n```"}});
        assert!(parse_hover(&v).unwrap().contains("origin"));
        // MarkedString array
        let v = json!({"contents": [{"language":"rust","value":"i32"}, "an integer"]});
        assert_eq!(parse_hover(&v).unwrap(), "i32\nan integer");
        // Plain string
        assert_eq!(parse_hover(&json!({"contents": "hi"})).unwrap(), "hi");
        // Empty / null
        assert!(parse_hover(&Value::Null).is_none());
        assert!(parse_hover(&json!({"contents": {"value": "  "}})).is_none());
    }

    /// Full protocol round-trip against a real rust-analyzer. Ignored by
    /// default (spawns an external dev tool); run explicitly to verify.
    #[tokio::test]
    #[ignore = "spawns the developer's rust-analyzer; run explicitly"]
    async fn live_definition_against_rust_analyzer() {
        let exe = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(".cargo/bin/rust-analyzer");
        assert!(exe.exists(), "needs rust-analyzer at {exe:?}");

        // Minimal cargo project: `origin()` defined and called.
        let root = crate::testutil::TempDir::new("ra-live");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let src = "fn origin() -> i32 {\n    0\n}\n\nfn main() {\n    let _ = origin();\n}\n";
        let main_rs = root.join("src/main.rs");
        std::fs::write(&main_rs, src).unwrap();

        let client = LspClient::start(&exe, &[], &root, None).await.unwrap();
        eprintln!("negotiated encoding: {:?}", client.encoding);
        client.did_open(&main_rs, "rust", 1, src);

        // Line 5 (0-based), the call `origin()` — character 12 is inside it.
        // rust-analyzer indexes async; poll until it resolves.
        let mut targets = Vec::new();
        for _ in 0..40 {
            targets = client.definition(&main_rs, 5, 12).await.unwrap_or_default();
            if !targets.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        eprintln!("definition targets: {targets:?}");
        assert!(!targets.is_empty(), "expected a definition target");
        // Should point back to the `origin` definition on line 0.
        assert_eq!(targets[0].line, 0);
        assert!(targets[0].path.ends_with("src/main.rs"));
    }
}

#[cfg(test)]
mod inbound_tests {
    use super::*;

    /// JSON-RPC ids may be strings. Treating a string-id REQUEST as a
    /// notification left the server waiting on a reply forever — for
    /// `workspace/configuration` that stalls the whole session.
    #[test]
    fn a_string_id_request_is_a_request() {
        let v = json!({
            "jsonrpc": "2.0",
            "id": "cfg-1",
            "method": "workspace/configuration",
            "params": {"items": []}
        });
        match classify(&v) {
            Inbound::Request { id, method } => {
                assert_eq!(method, "workspace/configuration");
                // Echoed verbatim, still a string.
                assert_eq!(id, json!("cfg-1"));
            }
            _ => panic!("string-id request must classify as a request"),
        }
    }

    #[test]
    fn numeric_ids_still_split_responses_from_requests() {
        assert!(matches!(
            classify(&json!({"id": 7, "result": null})),
            Inbound::Response(7)
        ));
        assert!(matches!(
            classify(&json!({"id": 7, "method": "workspace/configuration"})),
            Inbound::Request { .. }
        ));
        assert!(matches!(
            classify(&json!({"method": "window/logMessage"})),
            Inbound::Notification("window/logMessage")
        ));
        // A null id is "no id" per the spec, not a response to request 0.
        assert!(matches!(
            classify(&json!({"id": null, "method": "window/logMessage"})),
            Inbound::Notification(_)
        ));
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::framing::{read_message, write_frame};
    use tokio::io::AsyncWriteExt;

    /// The peer's ends of an in-memory transport.
    struct Peer {
        from_client: BufReader<tokio::io::DuplexStream>,
        to_client: tokio::io::DuplexStream,
    }

    impl Peer {
        async fn next(&mut self) -> Value {
            tokio::time::timeout(Duration::from_secs(5), read_message(&mut self.from_client))
                .await
                .expect("the client said nothing")
                .expect("a well-framed message")
                .expect("the client closed the stream")
        }

        async fn send(&mut self, msg: Value) {
            write_frame(&mut self.to_client, &msg).await.unwrap();
        }
    }

    /// Connect a client to a scripted peer, completing the handshake. Returns
    /// the client, the peer, and the `initialize` params the client sent.
    async fn connected() -> (LspClient, Peer, Value) {
        let (client_stdin, peer_rx) = tokio::io::duplex(1 << 16);
        let (peer_tx, client_stdout) = tokio::io::duplex(1 << 16);
        let mut peer = Peer {
            from_client: BufReader::new(peer_rx),
            to_client: peer_tx,
        };
        let client = tokio::spawn(LspClient::connect(
            client_stdin,
            client_stdout,
            Path::new("/proj"),
            None,
        ));
        let init = peer.next().await;
        assert_eq!(init["method"], "initialize");
        peer.send(json!({"jsonrpc": "2.0", "id": init["id"], "result": {"capabilities": {}}}))
            .await;
        let initialized = peer.next().await;
        assert_eq!(initialized["method"], "initialized");
        let client = client.await.unwrap().expect("handshake");
        (client, peer, init["params"].clone())
    }

    async fn wait_for(mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("condition never became true");
    }

    /// F1: a proxied server's parent is clew-server — possibly on another
    /// host — so our pid would be a stranger's (or nobody's), and node-based
    /// servers that watch it exit within seconds. `connect` sends null.
    #[tokio::test]
    async fn connect_sends_a_null_process_id() {
        let (_client, _peer, params) = connected().await;
        assert!(
            params.get("processId").is_some_and(Value::is_null),
            "processId must be present and null, got {params}"
        );
        // A local child, by contrast, is ours and gets our pid.
        let local = initialize_params(Path::new("/proj"), None, Some(4242));
        assert_eq!(local["processId"], 4242);
        // Server-initiated progress is advertised; without it servers report
        // none, and "indexing" looks the same as "no answer".
        assert_eq!(params["capabilities"]["window"]["workDoneProgress"], true);
    }

    /// The project root is the one workspace folder, in `initialize` and
    /// when the server asks for the folders: pyright looks for callers in
    /// other files only within its workspace folders.
    #[tokio::test]
    async fn the_project_root_is_the_workspace_folder() {
        let (_client, mut peer, params) = connected().await;
        let folders = json!([{ "uri": "file:///proj", "name": "proj" }]);
        assert_eq!(params["workspaceFolders"], folders);
        assert_eq!(
            params["capabilities"]["workspace"]["workspaceFolders"],
            true
        );
        peer.send(json!({"jsonrpc": "2.0", "id": "wf", "method": "workspace/workspaceFolders"}))
            .await;
        let reply = peer.next().await;
        assert_eq!(reply["id"], "wf");
        assert_eq!(reply["result"], folders);
    }

    /// F5: several progress reports run at once; one ending must not declare
    /// the server idle while another is still going.
    #[test]
    fn progress_is_tracked_per_token() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let progress = |token: Value, value: Value| {
            handle_notification(
                "$/progress",
                &json!({"params": {"token": token, "value": value}}),
                &state,
            );
        };
        let busy = || state.lock().unwrap().progress.busy();
        let summary = || state.lock().unwrap().progress.summary();

        progress(
            json!("index"),
            json!({"kind": "begin", "title": "Indexing"}),
        );
        progress(json!(7), json!({"kind": "begin", "title": "Fetching"}));
        assert!(busy());
        assert_eq!(summary().unwrap(), "Fetching (+1 more)");

        // A report updates its own token and keeps the title from `begin`.
        progress(
            json!("index"),
            json!({"kind": "report", "message": "3/10", "percentage": 30}),
        );
        assert_eq!(summary().unwrap(), "Indexing: 3/10 30% (+1 more)");

        // The first `end` used to clear everything.
        progress(json!(7), json!({"kind": "end"}));
        assert!(busy(), "Indexing is still running");
        assert_eq!(summary().unwrap(), "Indexing: 3/10 30%");

        // `7` and `"7"` are different tokens; ending one leaves the other.
        progress(json!("7"), json!({"kind": "begin", "title": "Other"}));
        progress(json!(7), json!({"kind": "end"}));
        progress(json!("index"), json!({"kind": "end"}));
        assert_eq!(summary().unwrap(), "Other");
        progress(json!("7"), json!({"kind": "end"}));
        assert!(!busy());
        assert_eq!(summary(), None);

        // Ending an unknown token is harmless.
        progress(json!("never-begun"), json!({"kind": "end"}));
        assert!(!busy());
    }

    /// A server that begins tokens and never ends them cannot grow the table
    /// without bound: the stalest one goes.
    #[test]
    fn abandoned_progress_tokens_are_bounded() {
        let mut progress = Progress::default();
        for i in 0..(MAX_PROGRESS_TOKENS + 20) {
            progress.update(Some(
                &json!({"token": i, "value": {"kind": "begin", "title": format!("t{i}")}}),
            ));
        }
        assert_eq!(progress.tokens.len(), MAX_PROGRESS_TOKENS);
        assert!(!progress.tokens.contains_key("0"), "the stalest is dropped");
        let newest = (MAX_PROGRESS_TOKENS + 19).to_string();
        assert!(progress.tokens.contains_key(&newest));
    }

    /// F13: the error was sticky — a server that recovered kept showing the
    /// old complaint for the rest of the session.
    #[test]
    fn a_server_error_clears_when_the_server_recovers() {
        let state = Arc::new(Mutex::new(ServerState::default()));
        let notify = |method: &str, params: Value| {
            handle_notification(method, &json!({"params": params}), &state);
        };
        let error = || state.lock().unwrap().error.clone();

        notify(
            "window/showMessage",
            json!({"type": 1, "message": "error loading workspace"}),
        );
        assert_eq!(error().as_deref(), Some("error loading workspace"));
        // A warning is not a recovery…
        notify("window/showMessage", json!({"type": 2, "message": "slow"}));
        assert!(error().is_some());
        // …an Info message is.
        notify(
            "window/showMessage",
            json!({"type": 3, "message": "loaded"}),
        );
        assert_eq!(error(), None);

        // So is a request answered with real content, but not an empty one.
        notify(
            "window/showMessage",
            json!({"type": 1, "message": "broken"}),
        );
        let mut wire = LspWire {
            state: state.clone(),
        };
        let ids = Ids::default();
        let _ = framing::Protocol::inbound(
            &mut wire,
            json!({"jsonrpc": "2.0", "id": 5, "result": null}),
            &ids,
        );
        assert!(error().is_some(), "null proves nothing");
        let _ = framing::Protocol::inbound(
            &mut wire,
            json!({"jsonrpc": "2.0", "id": 6, "result": [{"uri": "file:///a.rs"}]}),
            &ids,
        );
        assert_eq!(error(), None);

        // rust-analyzer's health report sets and clears its own error, and an
        // unrelated answer must not clear it: the workspace can be broken
        // while a hover about `std` still works.
        notify(
            "experimental/serverStatus",
            json!({"health": "error", "quiescent": true, "message": "failed to load workspace"}),
        );
        assert_eq!(error().as_deref(), Some("failed to load workspace"));
        let _ = framing::Protocol::inbound(
            &mut wire,
            json!({"jsonrpc": "2.0", "id": 7, "result": {"contents": "fn std"}}),
            &ids,
        );
        assert!(
            error().is_some(),
            "only the status channel clears its error"
        );
        notify(
            "experimental/serverStatus",
            json!({"health": "ok", "quiescent": true}),
        );
        assert_eq!(error(), None);
    }

    /// rust-analyzer says when it is still loading the workspace, which is
    /// exactly what a readiness check needs; other servers report progress.
    #[tokio::test]
    async fn busy_follows_progress_and_quiescence() {
        let (client, mut peer, _) = connected().await;
        assert!(!client.busy());
        peer.send(
            json!({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                         "params": {"health": "ok", "quiescent": false}}),
        )
        .await;
        wait_for(|| client.busy()).await;
        peer.send(
            json!({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                         "params": {"health": "ok", "quiescent": true}}),
        )
        .await;
        wait_for(|| !client.busy()).await;
        peer.send(json!({"jsonrpc": "2.0", "method": "$/progress",
                         "params": {"token": "t", "value": {"kind": "begin", "title": "Indexing"}}}))
            .await;
        wait_for(|| client.busy()).await;
        assert_eq!(client.progress().as_deref(), Some("Indexing"));
    }

    /// [`connected`], the peer answering `initialize` with `result`.
    async fn connected_as(result: Value) -> (LspClient, Peer) {
        let (client_stdin, peer_rx) = tokio::io::duplex(1 << 16);
        let (peer_tx, client_stdout) = tokio::io::duplex(1 << 16);
        let mut peer = Peer {
            from_client: BufReader::new(peer_rx),
            to_client: peer_tx,
        };
        let client = tokio::spawn(LspClient::connect(
            client_stdin,
            client_stdout,
            Path::new("/proj"),
            None,
        ));
        let init = peer.next().await;
        peer.send(json!({"jsonrpc": "2.0", "id": init["id"], "result": result}))
            .await;
        assert_eq!(peer.next().await["method"], "initialized");
        (client.await.unwrap().expect("handshake"), peer)
    }

    /// A server is loading from the moment it has started, before it has
    /// said anything: rust-analyzer — which names itself — until its first
    /// status says it has loaded, any other server until it reports its
    /// work, and done. It read as loaded then, and was asked about files it
    /// had not read: "file not found", for good.
    #[tokio::test]
    async fn a_server_just_started_is_loading_until_it_says_otherwise() {
        let status = |quiescent: bool| {
            json!({"jsonrpc": "2.0", "method": "experimental/serverStatus",
                   "params": {"health": "ok", "quiescent": quiescent}})
        };
        let progress = |kind: &str| {
            json!({"jsonrpc": "2.0", "method": "$/progress",
                   "params": {"token": "load", "value": {"kind": kind, "title": "Loading"}}})
        };
        let (ra, mut peer) = connected_as(json!({
            "capabilities": {},
            "serverInfo": {"name": "rust-analyzer", "version": "1.95.0"},
        }))
        .await;
        assert!(
            ra.loading(),
            "rust-analyzer read as loaded before its first status"
        );
        // Work it reports before its first status is part of the loading.
        peer.send(progress("begin")).await;
        peer.send(progress("end")).await;
        wait_for(|| ra.snapshot().is_ok_and(|s| s.reports == 2)).await;
        assert!(
            ra.loading(),
            "a report before its first status ended its loading"
        );
        peer.send(status(false)).await;
        wait_for(|| ra.snapshot().is_ok_and(|s| s.reports == 3)).await;
        assert!(ra.loading());
        peer.send(status(true)).await;
        wait_for(|| !ra.loading()).await;

        let (other, mut peer) = connected_as(json!({"capabilities": {}})).await;
        assert!(
            other.loading(),
            "a server read as loaded before it said anything"
        );
        peer.send(progress("begin")).await;
        peer.send(progress("end")).await;
        wait_for(|| !other.loading()).await;
    }

    /// The grace a just-started server gets is bounded: one that reports
    /// nothing through it has loaded — and work it reports after is work on
    /// the project as it changes, not its loading. rust-analyzer's grace is
    /// longer: its first status is due at once, and one that never comes
    /// means a server that sends none.
    #[test]
    fn the_grace_of_a_just_started_server_is_bounded() {
        let long_ago = Instant::now() - STATUS_GRACE - Duration::from_secs(1);
        let started = |at: Instant, reports_status: bool| {
            Arc::new(Mutex::new(ServerState {
                started: Some(at),
                reports_status,
                ..ServerState::default()
            }))
        };
        let loading =
            |state: &Arc<Mutex<ServerState>>| state.lock().unwrap().loading(Instant::now());
        let progress = |state: &Arc<Mutex<ServerState>>, kind: &str| {
            let msg = json!({"params": {"token": "t", "value": {"kind": kind, "title": "Check"}}});
            handle_notification("$/progress", &msg, state);
        };

        let just_now = started(Instant::now(), false);
        assert!(loading(&just_now));
        let quiet = started(Instant::now() - LOAD_GRACE, false);
        assert!(!loading(&quiet), "the grace did not end");
        progress(&quiet, "begin");
        assert!(
            !loading(&quiet),
            "work after a quiet start read as its loading"
        );

        // Work begun within the grace is the loading, however long it runs.
        let working = started(Instant::now(), false);
        progress(&working, "begin");
        working.lock().unwrap().started = Some(long_ago);
        assert!(loading(&working));
        progress(&working, "end");
        assert!(!loading(&working));

        let ra = started(Instant::now() - LOAD_GRACE, true);
        assert!(loading(&ra), "rust-analyzer's first status is waited for");
        let silent = started(long_ago, true);
        assert!(
            !loading(&silent),
            "a server that sends no status is waited for forever"
        );
    }

    /// A request the server cancelled on its own account (`ServerCancelled`)
    /// may be asked again, as one dropped for an edit may; an internal error
    /// would come back the same.
    #[test]
    fn a_request_the_server_cancelled_may_be_asked_again() {
        let failed = |code| QueryError::Failed {
            code: Some(code),
            message: "no".into(),
        };
        assert!(failed(SERVER_CANCELLED).transient());
        assert!(failed(CONTENT_MODIFIED).transient());
        assert!(failed(REQUEST_CANCELLED).transient());
        assert!(QueryError::TimedOut.transient());
        assert!(!failed(-32603).transient());
        assert!(!QueryError::Gone("server closed".into()).transient());
    }

    /// A request its caller gave up on — timed out, or dropped part-way — is
    /// cancelled at the server (`$/cancelRequest`), which was left at work on
    /// it: asked again, it worked on two. One answered is not.
    #[tokio::test]
    async fn a_request_given_up_on_is_cancelled_at_the_server() {
        let (client, mut peer, _) = connected().await;
        let cancel_of = |id: &Value| json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}});

        let timed_out = client
            .rpc
            .call("slow", json!({}), Duration::from_millis(20))
            .await;
        assert_eq!(timed_out, Err(CallError::TimedOut));
        let asked = peer.next().await;
        assert_eq!(asked["method"], "slow");
        assert_eq!(peer.next().await, cancel_of(&asked["id"]));

        let dropped = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .rpc
                    .call("dropped", json!({}), Duration::from_secs(60))
                    .await
            }
        });
        let asked = peer.next().await;
        assert_eq!(asked["method"], "dropped");
        dropped.abort();
        let _ = dropped.await;
        assert_eq!(peer.next().await, cancel_of(&asked["id"]));

        let answered = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .rpc
                    .call("quick", json!({}), Duration::from_secs(60))
                    .await
            }
        });
        let asked = peer.next().await;
        peer.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": 1}))
            .await;
        assert_eq!(answered.await.unwrap(), Ok(json!(1)));
        client.did_close(Path::new("/proj/a.rs"));
        assert_eq!(
            peer.next().await["method"],
            "textDocument/didClose",
            "an answered request was cancelled"
        );
    }

    /// E2-3 / I5: one snapshot carries everything a view shows, and taking it
    /// copies no diagnostic — it shares the live map, which a later publish
    /// replaces copy-on-write without disturbing the snapshot already taken.
    #[tokio::test]
    async fn a_snapshot_shares_diagnostics_and_stays_consistent() {
        let (client, mut peer, _) = connected().await;
        let publish = |message: &str| {
            json!({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                   "params": {"uri": "file:///proj/src/lib.rs", "diagnostics": [
                       {"range": {"start": {"line": 3, "character": 1},
                                  "end": {"line": 3, "character": 4}},
                        "severity": 1, "message": message}]}})
        };
        peer.send(publish("first")).await;
        wait_for(|| client.snapshot().is_ok_and(|s| s.diag_version == 1)).await;
        let before = client.snapshot().unwrap();
        let again = client.snapshot().unwrap();
        let lib = Path::new("/proj/src/lib.rs");
        assert!(
            std::ptr::eq(before.diagnostics(lib), again.diagnostics(lib)),
            "two snapshots of an unchanged state share the same diagnostics"
        );
        assert_eq!(before.diagnostics(lib)[0].message, "first");
        assert!(before.diagnostics(Path::new("/proj/other.rs")).is_empty());

        peer.send(publish("second")).await;
        wait_for(|| client.snapshot().is_ok_and(|s| s.diag_version == 2)).await;
        assert_eq!(
            client.snapshot().unwrap().diagnostics(lib)[0].message,
            "second"
        );
        assert_eq!(
            before.diagnostics(lib)[0].message,
            "first",
            "a snapshot is a consistent reading, not a live view"
        );
        assert_eq!(before.error, None);
        assert!(!before.busy);
    }

    /// E2-14 / I4: a thread that panics while holding the state lock poisons
    /// it. Every reader used to take that for an EMPTY state — no
    /// diagnostics, no log, no error: a healthy, quiet server. The snapshot
    /// and the log now say the state is unreadable, and `error` (what the
    /// status line and the agent consult) reports it as the server's problem.
    #[tokio::test]
    async fn a_poisoned_state_is_reported_not_read_as_empty() {
        let (client, _peer, _) = connected().await;
        assert!(client.snapshot().is_ok());
        assert_eq!(client.logs(), Ok(Vec::new()));
        // The tail is the newest lines, oldest first.
        {
            let mut s = client.state.lock().unwrap();
            for line in ["one", "two", "three"] {
                s.push_log(line.into());
            }
        }
        assert_eq!(client.log_tail(2), Ok(vec!["two".into(), "three".into()]));
        assert_eq!(client.log_tail(9).map(|t| t.len()), Ok(3));
        assert_eq!(client.error(), None);

        let state = Arc::clone(&client.state);
        let _ = std::thread::spawn(move || {
            let _held = state.lock().unwrap();
            panic!("a panic while updating the server state");
        })
        .join();

        assert_eq!(client.snapshot().unwrap_err(), StatePoisoned);
        assert_eq!(client.logs(), Err(StatePoisoned));
        assert_eq!(client.log_tail(2), Err(StatePoisoned));
        let error = client.error().expect("poisoning is reported as an error");
        assert!(error.contains("restart"), "{error}");
    }

    /// F7: a malformed `Content-Length` used to parse as 0 and desync the
    /// stream. It now ends the session with an error that says so: the
    /// request in flight fails at once, the client reports itself dead, and
    /// the status bar gets the reason.
    #[tokio::test]
    async fn a_malformed_frame_ends_the_session_with_an_error() {
        let (client, mut peer, _) = connected().await;
        let hover = tokio::spawn({
            let client = client.clone();
            async move { client.hover(Path::new("/proj/a.rs"), 0, 0).await }
        });
        let request = peer.next().await;
        assert_eq!(request["method"], "textDocument/hover");
        peer.to_client
            .write_all(b"Content-Length: 1x\r\n\r\n{}")
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(5), hover)
            .await
            .expect("the failure must be prompt, not a 30 s timeout")
            .unwrap()
            .unwrap_err();
        assert!(err.contains("connection to the server broke"), "{err}");
        wait_for(|| !client.alive()).await;
        assert!(
            client.error().is_some_and(|e| e.contains("Content-Length")),
            "{:?}",
            client.error()
        );
    }

    /// F4: stopping says `shutdown`, waits for the answer, then `exit` — and
    /// `shutdown` goes without a `params: null`, which JSON-RPC forbids.
    #[tokio::test]
    async fn shutdown_asks_the_server_to_leave() {
        let (client, mut peer, _) = connected().await;
        let stopping = tokio::spawn({
            let client = client.clone();
            async move { client.shutdown().await }
        });
        let shutdown = peer.next().await;
        assert_eq!(shutdown["method"], "shutdown");
        assert!(shutdown.get("params").is_none(), "{shutdown}");
        assert!(!client.alive(), "a stopping client takes no new work");
        peer.send(json!({"jsonrpc": "2.0", "id": shutdown["id"], "result": null}))
            .await;
        let exit = peer.next().await;
        assert_eq!(exit["method"], "exit");
        assert!(exit.get("id").is_none());
        tokio::time::timeout(Duration::from_secs(5), stopping)
            .await
            .expect("shutdown returns once the session is over")
            .unwrap();
    }

    /// F4: a local server that ignores `shutdown`/`exit` is killed after the
    /// grace period — together with what it spawned (rust-analyzer's cargo
    /// processes used to survive it).
    #[tokio::test]
    #[cfg(unix)]
    async fn a_local_server_that_ignores_exit_is_killed_with_its_children() {
        let dir = crate::testutil::TempDir::new("lsp-stop");
        let pidfile = dir.join("helper.pid");
        // Answers `initialize` (id 1, the first request) once it is on the
        // wire, starts a helper, then ignores everything.
        let script = r#"
            head -c 1 >/dev/null
            body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
            printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
            sleep 30 &
            echo $! > "$1"
            wait
        "#;
        let args = vec![
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
            pidfile.to_string_lossy().into_owned(),
        ];
        let client = LspClient::start(Path::new("/bin/sh"), &args, &dir, None)
            .await
            .expect("handshake with the scripted server");
        let mut helper = None;
        for _ in 0..500 {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                helper = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let helper = helper.expect("the server started its helper");
        let started = std::time::Instant::now();
        client.shutdown().await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the stop sequence is bounded"
        );
        let mut gone = false;
        for _ in 0..400 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(helper, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(gone, "the server's helper outlived the session");
    }

    /// F4/F2: a server that stops reading its stdin used to wedge the actor
    /// in the write to it, where the stop behind it was never seen: the
    /// shutdown never finished, the server was never reaped, and the client
    /// reported it alive. The stop now gets past the blocked write within its
    /// grace, and the server — its group with it — is killed without one.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_local_server_that_stops_reading_is_still_stopped_in_time() {
        let dir = crate::testutil::TempDir::new("lsp-deaf");
        let pidfile = dir.join("helper.pid");
        // Answers `initialize` once it arrives, starts a helper, and never
        // reads its stdin again.
        let script = r#"
            head -c 1 >/dev/null
            body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
            printf 'Content-Length: %d\r\n\r\n%s' "${#body}" "$body"
            sleep 30 &
            echo $! > "$1"
            wait
        "#;
        let args = vec![
            "-c".to_string(),
            script.to_string(),
            "sh".to_string(),
            pidfile.to_string_lossy().into_owned(),
        ];
        let client = LspClient::start(Path::new("/bin/sh"), &args, &dir, None)
            .await
            .expect("handshake with the scripted server");
        // Many times what a pipe holds: the write blocks for good.
        client.did_open(&dir.join("big.rs"), "rust", 1, &"x".repeat(4 << 20));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            client.alive(),
            "stuck, not dead: the stall limit is far off"
        );
        let mut helper = None;
        for _ in 0..500 {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                helper = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let helper = helper.expect("the server started its helper");

        let started = std::time::Instant::now();
        client.shutdown().await;
        let took = started.elapsed();
        assert!(!client.alive());
        let mut gone = false;
        for _ in 0..400 {
            // SAFETY: signal 0 only probes for existence.
            if unsafe { libc::kill(helper, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // The blocked write's grace, then an immediate kill — not the
        // polite path's reply wait and exit grace, let alone forever.
        assert!(
            took < framing::STOP_WRITE_GRACE + Duration::from_secs(1),
            "the stop took {took:?}"
        );
        assert!(gone, "the server's helper outlived the session");
    }
}
