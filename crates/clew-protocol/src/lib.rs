//! The wire contract between the clew client (GUI) and a clew-server.
//!
//! The client renders; the server, running where the code lives (a local
//! child process, or a remote machine over SSH), does everything
//! system-facing — filesystem, git, language servers, debug adapters,
//! indexing, and AI orchestration — and streams back only what the UI needs.
//! This crate holds every message and every data type that crosses that seam,
//! so both sides depend on one contract, and the build fingerprint
//! ([`SCHEMA_FINGERPRINT`]) covers all of it.
//!
//! Framing: newline-delimited JSON, one [`ClientMessage`] or [`ServerMessage`]
//! per line, each at most [`MAX_FRAME_BYTES`]. A peer that sends a line it
//! cannot parse, or an over-cap line, is disconnected: past either there is no
//! way back into sync.
//!
//! The data types of the typed replies (git answers, statistics, the call
//! graph, the structure index) live in [`payload`], and byte payloads travel
//! as base64 (see `bytes`); both are re-exported here.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

mod bytes;
pub mod payload;

pub use payload::*;

/// Bumped on any incompatible change. The client refuses a server whose version
/// differs (and, for a remote, fetches the matching clew-server binary).
/// v4: `Tree` and `Docs` events carry the project `root` they describe.
/// v5: `LspResolve`/`LspResolved`/`LspApprovals` — repo-specified language
/// servers need a client-side approval that the server enforces; `Hello`/
/// `Ready` versions are now checked on both sides; `FilesChanged` carries
/// its project `root`.
/// v6: `LspResolved` carries an explicit `LspResolution` (ready / needs
/// approval / needs install / unsupported) instead of an ambiguous
/// `Option`; `LspInstall` is the consent-carrying install request — the
/// server no longer installs anything on a mere `SpawnLsp`.
/// v7: `Hello`/`Ready` carry the build's [`SCHEMA_FINGERPRINT`] so two
/// builds that both claim one version but serialize different shapes
/// refuse each other at the handshake instead of silently dropping
/// frames; `ProjectSymbols` carries a monotonic `seq` so a late full
/// snapshot can never clobber newer partial updates; `ReadState`/`WriteState`
/// name the project `root` they address, so a state write racing a project
/// switch is refused rather than applied to the wrong project.
/// v8: `ProjectSymbols` carries its resolution metadata and structure index
/// as a [`Patch`] instead of a bare `Option`, so "recomputed, and the answer
/// is nothing" is distinguishable from "not recomputed".
/// v9: `WriteState` is acknowledged with [`Event::StateWritten`], so the client
/// can tell a durable write from one queued into a dead transport.
/// v10: [`Request::EditState`] carries ONE entry-level change ([`StateMerge`])
/// that the server applies to what is on disk, replying [`Event::StateEdited`]
/// with the merged file — a wholesale snapshot reverted what another client
/// had written since.
/// v11: [`LspResolution::Ready`] carries the [`LspOptionsSpec`] of the
/// `init_options` it withheld, so the client can raise the approval modal.
/// v12: the typed payloads — [`GitResult`] (one variant per [`GitOp`]),
/// [`StatsReport`], [`CallGraph`], [`StructureIndex`] — live in this crate
/// and cross as themselves instead of JSON strings the client decoded with a
/// silent default; byte payloads are base64; [`Event::Error`] carries an
/// [`ErrorCode`] (retries match the code, never the text); [`Event::Tree`]
/// carries a scan `seq`; `BuildDocs` is answered by a correlated reply;
/// [`Event::Status`] is the channel for unsolicited notices (watcher trouble,
/// a process that died); `SearchResults` reports the files it skipped and
/// `DirListing` the entries it left out; dead surface is gone (the
/// `Find`/`Outline`/`Watch`/`Explain` requests and `AgentStop` — folded into
/// [`Request::Cancel`] — the `Explanation`/`Outline`/`SymbolIndexDone` events,
/// `Hello.ai`, and the never-set subscription id on every server frame); the
/// fingerprint ignores comments and formatting.
/// v13: a stream's terminal notification ([`Event::ChatStreamDone`],
/// [`Event::AgentDone`]) says how it ended as a [`StreamOutcome`] — a Stop is
/// not a failure, and is no longer a magic string the client had to compare;
/// [`ErrorCode::Cancelled`] answers work the client stopped or replaced (a
/// cancelled `Chat` or `Embed`, an `OpenProject` superseded by a newer one),
/// so every request is answered; [`LspResolution::NeedsInstall`] carries a
/// digest of exactly the install it describes and [`Request::LspInstall`]
/// carries it back, so the server runs only the install the user was shown;
/// `init_options` and the withheld options cross as JSON values rather than
/// as strings the client parsed with a silent fallback; [`Event::Tree`]
/// carries the tracked-but-ignored files; [`Request::EditState`] carries an
/// `edit_id` the server deduplicates, so an edit whose reply was lost with its
/// transport can be sent again without being applied twice; [`Event::Sources`]
/// names the rels that do not exist, so a file the server could not read is
/// no longer taken for a deleted one, those too large to explain, with their
/// sizes, those that are not plain text files, with why ([`Refusal`]), and
/// those it could not read, with the error; it is paged by bytes,
/// naming the rels a reply had no room for, which the client asks for again;
/// a `Chat` whose model call failed says what failed, typed
/// ([`ErrorCode::Provider`]), instead of in words the client parsed.
/// v14: an [`IndexSymbol`] says whether its function is an entry point, and
/// of which kind (`entry`), so a remote project's overview and its "reached
/// from" chains know the routes, commands and handlers a `main` alone did
/// not name; [`GitOp::Churn`] answers how often each file changed over the
/// recent history, for the graphs' change-frequency overlay.
pub const PROTOCOL_VERSION: u32 = 14;

/// A hash of this crate's source as a token stream — comments and whitespace
/// removed — computed at build time (see `build.rs`). Carried in `Hello` /
/// `Ready` next to [`PROTOCOL_VERSION`]: the version is the human-facing
/// contract, the fingerprint the mechanical one. Two builds whose protocol
/// CODE differs fail the handshake even if a version bump was forgotten, while
/// a comment or a reformat changes nothing (and forces no redeploy). For a
/// remote, a failed handshake surfaces as a redeploy of the matching server.
pub const SCHEMA_FINGERPRINT: &str = env!("CLEW_PROTOCOL_FINGERPRINT");

/// The line `clew-server --version` prints, and the one the client's SSH
/// bootstrap compares a deployed binary's output against, byte for byte. One
/// definition, so the probe can never drift from the binary it probes.
pub fn version_line() -> String {
    format!("clew-server protocol {PROTOCOL_VERSION} fingerprint {SCHEMA_FINGERPRINT}")
}

/// Hard cap on one serialized frame (a JSON line) in either direction. A real
/// frame is at most a request or reply around one file's content — nowhere
/// near this; without a cap a broken or hostile peer could grow one "line"
/// without bound before the parser ever sees it. A peer that exceeds it is
/// dropped: past an oversized frame there is no way back into sync.
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Most bytes one [`Request::ProcessInput`] or [`Event::ProcessOutput`]
/// carries. A proxied byte stream is cut into chunks no bigger than this, so a
/// chatty process holds the one ordered stream for one small frame at a time
/// instead of a frame the size of its whole message — everything else (file
/// opens, hovers) queues behind a frame while it is written and parsed.
pub const MAX_PROCESS_CHUNK: usize = 64 * 1024;

/// A path relative to the project root (the wire never carries absolute,
/// machine-specific paths for project files).
pub type Rel = String;

/// A field a partial update may leave alone, or replace — including replacing
/// it with nothing.
///
/// A bare `Option` cannot say that. It collapses "I did not recompute this"
/// and "I recomputed it and the answer is nothing" into the same `None`, and
/// a receiver that reads `None` as "unchanged" then keeps a value the sender
/// knows is gone: a deleted `go.mod` module line, a removed Dart package
/// name, or the last Rust trait in a project all left stale data resolving
/// imports and answering hover peeks until the project was reopened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Patch<T> {
    /// Not recomputed by this publication: keep what you have.
    #[default]
    Unchanged,
    /// Recomputed. `None` means it no longer exists — drop what you have.
    Set(Option<T>),
}

/// Correlates a request with its reply. Minted by the client from ONE counter
/// per connection, which also mints the `stream` ids of streamed requests, so
/// a number names exactly one piece of work (see [`Request::Cancel`]).
pub type RequestId = u64;

// -- shared data types -------------------------------------------------------

/// A directory node: sub-directories first, then files (both sorted). The server
/// builds it; the client renders it verbatim, so a server-provided tree is
/// identical to a local scan (no client-side rebuild to drift).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirNode {
    pub dirs: Vec<(String, DirNode)>,
    pub files: Vec<String>,
}

/// One entry in a `DirListing` — a child of the directory being browsed in the
/// remote folder picker. `is_dir` is what makes a row navigable vs. a leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// One documented API entry for the Docs view. Nested by source-range
/// containment, so members (methods, inner items) live under their enclosing
/// type/module. Undocumented public items are still included (an API surface,
/// like rustdoc), with an empty `doc`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocItem {
    pub name: String,
    /// Symbol kind ("function", "struct", "class", "method", …).
    pub kind: String,
    /// The declaration (signature line(s)), for display.
    pub signature: String,
    /// The doc comment as markdown; empty when undocumented. Enriched on demand
    /// by the client via LSP hover.
    pub doc: String,
    /// 1-based definition line, for jump-to-source.
    pub line: usize,
    /// Whether the item is part of the public API (pub / export / capitalized /
    /// non-underscore, per language). The client filters on this.
    pub public: bool,
    pub children: Vec<DocItem>,
    /// For a type (struct, class, enum, interface, trait, union, type alias):
    /// the identifiers its declaration, its own members (fields, variants,
    /// constants — the body less its methods' bodies) and its members'
    /// signatures name, in order of first sighting, capped. What the type map
    /// resolves against the project's types to draw "uses" and "inherits"
    /// edges; empty for everything else.
    #[serde(default)]
    pub refs: Vec<String>,
}

/// One file's documented API — a group in the Docs tree (a piece of
/// `Event::Docs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocFile {
    pub rel: Rel,
    pub items: Vec<DocItem>,
}

/// One highlighted source line: a list of `(text, style index)` spans.
///
/// The style index points into clew-core's `HIGHLIGHT_NAMES` (the shared,
/// version-locked capture list the tokenizer is configured with); `None` is
/// default foreground. The server does the tree-sitter tokenization and sends
/// these indices; the client maps an index to a theme color, so color stays a
/// client concern and the wire stays theme-agnostic and full-fidelity (no lossy
/// role bucketing — highlighting is identical to a local render).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HlLine {
    pub spans: Vec<(String, Option<u8>)>,
}

/// One outline / symbol entry. Shared with clew-core (the tokenizer produces it,
/// the outline panel renders it) so there is no conversion at the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: String,
    pub line: usize,
    pub end_line: usize,
}

/// The target facts `cfg` predicates are evaluated against, chosen client-side
/// and sent with `ReadFile` so the server dims the same inactive branches. The
/// rich `Target` (host detection, presets) lives in clew-core; this is its wire
/// form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub label: String,
    pub os: String,
    pub arch: String,
    pub family: String,
}

/// One line's git blame. Shared with clew-core (git produces it, the gutter
/// renders it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub time: i64,
    pub summary: String,
    /// True for lines not yet committed (blame sha is all zeros).
    pub uncommitted: bool,
}

/// A line's change status versus `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
}

/// Git view of one file, all indexed by 0-based final line number.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitInfo {
    pub blame: Vec<BlameLine>,
    pub status: Vec<Option<ChangeKind>>,
    /// Lines immediately below which content was deleted (a gutter marker).
    pub deleted_at: HashSet<usize>,
}

impl GitInfo {
    pub fn blame_for(&self, line: usize) -> Option<&BlameLine> {
        self.blame.get(line)
    }

    pub fn status_for(&self, line: usize) -> Option<ChangeKind> {
        self.status.get(line).copied().flatten()
    }
}

/// A text-search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rel: Rel,
    pub line: usize,
    pub preview: String,
}

/// A git operation for the `Git` request — each maps onto one
/// `clew_core::git` function, run against the server's project root (see
/// `clew_core::git::run_op`). The answer is the [`GitResult`] variant of the
/// same name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GitOp {
    FileHistory {
        rel: Rel,
        limit: usize,
    },
    SymbolHistory {
        rel: Rel,
        start: usize,
        end: usize,
        limit: usize,
    },
    FileAt {
        sha: String,
        rel: Rel,
    },
    AddedLines {
        sha: String,
        rel: Rel,
    },
    CommitMessage {
        sha: String,
    },
    CommitFileDiff {
        sha: String,
        rel: Rel,
        max_bytes: usize,
    },
    DiffLines {
        rel: Rel,
    },
    ReviewBase,
    CommitSubjects {
        base: String,
    },
    ChangedFiles {
        base: String,
    },
    RangePatch {
        base: String,
        max_bytes: usize,
    },
    /// How often each file changed over the last `commits` commits (merges
    /// left out): the files touched, with their commit counts and latest
    /// commit time, most changed first.
    Churn {
        commits: usize,
    },
}

/// One file's entry in a `ProjectSymbols` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSymbols {
    pub rel: Rel,
    pub symbols: Vec<IndexSymbol>,
    /// The file's raw (unresolved) import specifiers — the extraction half
    /// of the import graph; the client resolves them over the file set.
    pub imports: Vec<WireImport>,
}

/// One raw import in a `ProjectSymbols` snapshot (see `clew-core`'s
/// `imports::RawImport`, whose wire form this is).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireImport {
    pub module: String,
    /// 1-based line of the statement.
    pub line: usize,
    /// Rust `mod x;` — a submodule name, not a scoped path.
    pub is_mod: bool,
}

/// One indexed symbol in a `ProjectSymbols` snapshot. Like [`Symbol`] but
/// carrying the test classification, which needs the file's text — available
/// where the snapshot is built, not where it is consumed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexSymbol {
    pub name: String,
    pub kind: String,
    /// 1-based first line.
    pub line: usize,
    pub is_test: bool,
    /// The entry-point kind, by its key (`main`, `route`, `command`,
    /// `handler`; see `clew_core::outline::EntryKind`), for a function
    /// execution enters the project through; `None` for the rest. Classified
    /// where the file's text is, like `is_test`.
    pub entry: Option<String>,
}

/// Chat/LLM provider config (the provider is a slug the server maps back). The
/// client sends this so the server can make AI calls on its behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiChatConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

/// Embedding provider config (OpenAI-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiEmbedConfig {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
}

/// One chat turn; `role` is "user" or "assistant".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiChatMsg {
    pub role: String,
    pub content: String,
}

/// A code location an agent step touched, for click-through in the step chip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRef {
    pub rel: Rel,
    /// 1-based line, when the step points at a specific place.
    pub line: Option<usize>,
}

/// One output of a notebook code cell, ready to render natively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NotebookOutput {
    /// Stream/plain text as `(run, ansi_color)` spans (color: 0–15 palette).
    Text {
        spans: Vec<(String, Option<u8>)>,
        stderr: bool,
    },
    /// A raster image: the PNG/JPEG bytes (decoded from the notebook's own
    /// base64 server-side; base64 again on the wire).
    Image {
        #[serde(with = "crate::bytes")]
        data: Vec<u8>,
    },
    Svg(String),
    /// Output clew doesn't render natively; the label names what was skipped.
    Placeholder(String),
}

/// One notebook cell, prepared for the client's notebook view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotebookCell {
    /// "markdown" | "code" | "raw".
    pub kind: String,
    /// Raw cell source (markdown for md cells; code text for code cells).
    pub source: String,
    /// Highlighted code lines (code cells; empty otherwise).
    pub lines: Vec<HlLine>,
    /// 1-based first line of this cell in the script projection — the
    /// notebook's canonical line space (search hits / outline / citations).
    pub proj_line: usize,
    pub outputs: Vec<NotebookOutput>,
    pub execution_count: Option<u64>,
}

// -- messages ----------------------------------------------------------------

/// Client → server. Every request is answered by exactly one
/// [`ServerMessage::Reply`] carrying its id, EXCEPT the fire-and-forget ones,
/// which say so: `LspApprovals`, `ProcessKill`, `Cancel` and `SetAiConfig`
/// are never answered; `SpawnProcess`, `SpawnLsp` and `ProcessInput` are
/// answered only by an `Error`, when they fail for a reason the user should
/// see (a spawn's outcome itself is always its `ProcessStarted` or
/// `ProcessExited` notification); `ChatStream` and `AgentAsk` answer with a
/// stream of notifications that always ends in a terminal one. Work the
/// client stopped (`Cancel`) or replaced (a newer `OpenProject`) is still
/// answered, with [`ErrorCode::Cancelled`]. Before a successful `Hello`,
/// every other request is answered with [`ErrorCode::Handshake`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Handshake: agree on the protocol. The `fingerprint` is the client
    /// build's [`SCHEMA_FINGERPRINT`]; the server refuses a version or a
    /// fingerprint that differs from its own. Reply: `Ready`, or `Error`.
    ///
    /// FROZEN: the handshake frames (`Hello`, `Ready`, and the `Error` that
    /// refuses a handshake) are how two builds that disagree find out — they
    /// must stay parseable by every older and newer peer, so a field may
    /// never be added to them unless it is defaulted. (`fingerprint` is
    /// defaulted for pre-v7 peers, which then fail the version check with a
    /// clear message instead of a parse error.) v12 dropped the `ai` field a
    /// v5–v11 server requires — the last break of that rule: such a server
    /// cannot parse this `Hello` and hangs up, which the client reports; it
    /// never gets that far in practice, since the SSH bootstrap probes the
    /// remote binary's `--version` first and the local server ships with the
    /// client.
    Hello {
        protocol: u32,
        #[serde(default)]
        fingerprint: String,
    },
    /// Open a project rooted at this server-side path. Reply: `Tree` (once the
    /// scan committed and the watcher is live); then a full `ProjectSymbols`
    /// notification, and watcher notifications for as long as it stays open.
    /// An open that a newer `OpenProject` superseded before it finished is
    /// answered with [`ErrorCode::Cancelled`] (the newer one answers for
    /// itself); a scan that fails, with [`ErrorCode::Failed`].
    OpenProject { root: String },
    /// Read a file for display. Reply: `FileContent` (highlighted lines,
    /// outline symbols, doc comments, inactive `#[cfg]` lines — `target`
    /// selects which cfg branches count as active) or, for a notebook,
    /// `NotebookContent`.
    ReadFile { rel: Rel, target: TargetSpec },
    /// Text search across the project, with the sidebar's toggles and globs.
    /// Reply: `SearchResults`.
    Search {
        query: String,
        regex: bool,
        case_sensitive: bool,
        whole_word: bool,
        /// Comma/space-separated globs; when non-empty, only matching files search.
        include: String,
        /// Comma/space-separated globs of files to skip.
        exclude: String,
    },
    /// Per-line blame + change status for the gutter. Reply: `GitInfo`.
    GitInfo { rel: Rel },
    /// Read one project state file (`<root>/.clew/<rel>`) where the project
    /// lives. This is how a REMOTE client loads its per-project session
    /// state (history, bookmarks, notes, reading target) — the same-pathed
    /// files on its own disk belong to a different machine. Reply:
    /// `StateContent` (text `None` when the file does not exist), or `Error`
    /// when it exists but cannot be read safely.
    /// `root` names the project the client believes is open; the server
    /// refuses when it is not the one it holds (see [`Request::WriteState`]).
    ReadState { root: String, rel: Rel },
    /// Write (or, with `text: None`, delete) one project state file under
    /// `<root>/.clew/`. Applied with the same rules as every local state
    /// write: atomic, size-capped, never through a symlinked `.clew`.
    /// Reply: [`Event::StateWritten`] once it is on disk, or `Error`.
    ///
    /// `root` is the project the state belongs to, and the server refuses a
    /// write whose root is not the one it currently holds: a save racing a
    /// project switch must not land in the other project's `.clew/`.
    ///
    /// Correct only for a store whose whole content ONE client owns —
    /// `history.json` and `reading.toml`, whose last-writer-wins semantics are
    /// deliberate. Anything two clients may both add entries to must use
    /// [`Request::EditState`] instead, or the later snapshot deletes the
    /// other's entries.
    WriteState {
        root: String,
        rel: Rel,
        text: Option<String>,
    },
    /// Apply ONE entry-level change to a project state file that holds a JSON
    /// array of objects (`bookmarks.json`, `notes.json`,
    /// `cache/walkthroughs.json`), where the project lives. Reply:
    /// [`Event::StateEdited`] carrying the merged file, or `Error`.
    ///
    /// A client re-reads remote state only at project open and on reconnect,
    /// so its copy is stale for the whole session by construction; only the
    /// SERVER sees every writer, so the read-modify-write happens there. The
    /// change travels as data ([`StateMerge`]) and is applied to what is on
    /// disk right now. Same guards as `WriteState`.
    ///
    /// `edit_id` makes the request idempotent. A client whose transport died
    /// before the reply arrived cannot know whether the edit was applied, so
    /// it sends the SAME edit again, with the same id, over the next
    /// transport — which may reach another server process. The server keeps
    /// the ids it applied on disk, per store, in the project's
    /// `.clew/cache/edits/`, and answers a repeated one with the file as it
    /// is, applying nothing: a [`StateEdit::Toggle`]
    /// replayed after it landed would otherwise undo it. Unique per edit, and
    /// [`valid_edit_id`] (the server refuses anything else).
    EditState {
        root: String,
        rel: Rel,
        merge: StateMerge,
        edit_id: String,
    },
    /// Compute the project's code statistics where the files live. Reply:
    /// `Stats`.
    Stats,
    /// Build the name-based project call graph where the files live. `scope`
    /// is the client's resolved import scope (file → the internal files it
    /// imports, project-relative), which the server cannot derive alone.
    /// Reply: `ProjectCalls`.
    ProjectCalls { scope: Vec<(Rel, Vec<Rel>)> },
    /// Read a batch of source files (plain text, no highlighting) — what a
    /// remote client's Explain pass consumes instead of reading
    /// remote-pathed files off its own disk. Bounded on the server: per file,
    /// per batch, and per reply, in bytes — a reply names the rels it had no
    /// room for, to be asked for again. Reply: `Sources`, which names the
    /// rels that do not exist, those too large to explain, those that are
    /// not plain text files and those that could not be read, each apart.
    ReadSources { rels: Vec<Rel> },
    /// Run one git operation where the repository lives (Time Travel,
    /// blame-why, the change-review walkthrough, the diff gutter). Arguments
    /// are validated server-side. Reply: `GitResult`, whose variant is the
    /// op's.
    Git { op: GitOp },
    /// Spawn a subprocess and proxy its stdio. `proc` is a client-assigned
    /// handle correlating input/output/exit. Bytes are framed as the process
    /// emits them — the caller reassembles its protocol. Refused by a remote
    /// server: remote language servers and debug adapters start through
    /// `SpawnLsp` / `SpawnAdapter`, resolved and gated on that host.
    /// Answered only on failure (`Error`); success is the `ProcessStarted`
    /// notification, and every spawn ends in exactly one of `ProcessStarted`
    /// or `ProcessExited`.
    SpawnProcess {
        proc: u64,
        cmd: String,
        args: Vec<String>,
        /// Working directory; defaults to the project root when `None`.
        cwd: Option<String>,
    },
    /// Start the language server for `language`, resolved and provisioned on the
    /// server (where the code lives) — the client never ships a binary path, so
    /// the remote uses its own LSP. Proxied like `SpawnProcess` via `proc`, and
    /// answered like it: an `Error` when nothing will run — no server
    /// configured for the language, one not installed, a refused command, a
    /// failed spawn — besides the `ProcessExited` that ends the proxy. A
    /// repo-specified `command` runs only when its fingerprint is approved
    /// (see `LspApprovals`).
    SpawnLsp { proc: u64, language: String },
    /// Resolve AND spawn the debug adapter for `lang` (a client `Lang` slug:
    /// "native", "python", "dart", …) on this host — where the debuggee
    /// lives — proxying its stdio under `proc` like `SpawnProcess`. Only
    /// stdio-transport adapters work remotely (TCP ones listen on the
    /// server's loopback, unreachable from the client). Reply:
    /// `AdapterSpawned`, carrying the adapter-specific `launch` request body
    /// built with THIS host's paths.
    SpawnAdapter {
        proc: u64,
        lang: String,
        program: String,
        args: Vec<String>,
    },
    /// Resolve what `SpawnLsp` for `language` would execute, without running
    /// anything. Reply: `LspResolved`; when the project's own `lsp.toml`
    /// names a `command`, it carries the full command line and fingerprint so
    /// the client can show the user exactly what would run and record an
    /// approval against it.
    LspResolve { language: String },
    /// Install the store-managed server for `language` on this host. Sent
    /// only after the user consented in the client — the server itself never
    /// initiates a download or toolchain install. Reply: `LspResolved` with
    /// the post-install state (`Ready`, or `Unsupported` on failure).
    ///
    /// `consent` is the `consent` digest of the [`LspResolution::NeedsInstall`]
    /// the user was shown and allowed, returned verbatim: the approval is for
    /// THAT install. The server resolves again at install time and runs
    /// nothing whose digest differs — the host's `lsp.toml` edited while the
    /// prompt sat open, say — replying with the current `NeedsInstall`
    /// instead, so the user is asked about what would actually run.
    LspInstall { language: String, consent: String },
    /// The user's language-server command approvals for the open project
    /// (`language` → fingerprint), recorded client-side and pushed here so the
    /// server's spawn paths (SpawnLsp, the Ask agent's semantic tools) honor
    /// them. Replaces the previous set. No reply.
    LspApprovals { approvals: Vec<(String, String)> },
    /// Bytes for a spawned process's stdin, at most [`MAX_PROCESS_CHUNK`] of
    /// them. Answered only on failure: an over-cap chunk (which also stops
    /// the process — its stream would be missing those bytes for good), or a
    /// process that stopped reading and was killed for it.
    ProcessInput {
        proc: u64,
        #[serde(with = "crate::bytes")]
        data: Vec<u8>,
    },
    /// Terminate a spawned process. No reply; its `ProcessExited` follows.
    ProcessKill { proc: u64 },
    /// Stop in-flight cancellable work. No reply.
    ///
    /// `id` names the work by the number it was started under: the request id
    /// of a `Chat` or an `Embed`, or the `stream` of a `ChatStream` or
    /// `AgentAsk`. The client mints both from its one request counter, so a
    /// number names one piece of work and the server needs no hint which kind
    /// it is. The work still ends the way it always ends — `Chat` and `Embed`
    /// with their reply (an `Error` with [`ErrorCode::Cancelled`]), a stream
    /// with `ChatStreamDone`, an agent turn with `AgentDone` (both
    /// [`StreamOutcome::Stopped`]) — so a caller waiting on that end is never
    /// stranded. An id that names nothing running (finished, or never
    /// cancellable) is a no-op.
    Cancel { id: RequestId },
    /// Give the server the AI provider config to use when it makes calls on
    /// the client's behalf. Replaces the stored credentials wholesale — `None`
    /// included, which is how the client says "you may no longer hold these"
    /// (a deleted key, a revoked per-host grant). Sent after every handshake
    /// and whenever the config changes. No reply.
    SetAiConfig {
        chat: Option<AiChatConfig>,
        embed: Option<AiEmbedConfig>,
    },
    /// A chat completion the server runs with its stored chat config. Reply:
    /// `ChatResult` with the whole response; a model call that failed is an
    /// `Error` with [`ErrorCode::Provider`]. Cancellable by its request id.
    Chat {
        system: String,
        messages: Vec<AiChatMsg>,
        max_tokens: u32,
    },
    /// Embed texts with the server's stored embedding config. Reply:
    /// `Embeddings`. Cancellable by its request id.
    Embed { texts: Vec<String> },
    /// Like `Chat`, but streamed: `ChatDelta` notifications as tokens arrive
    /// and always a final `ChatStreamDone` saying how it ended, all tagged
    /// with `stream`. No reply. Cancellable by `stream`.
    ChatStream {
        stream: u64,
        system: String,
        messages: Vec<AiChatMsg>,
        max_tokens: u32,
    },
    /// Run an agent turn for the Ask panel: the server explores the project with
    /// tools (search / read / outline / …) and streams its progress back —
    /// `AgentStep` per tool call, `AgentDelta` tokens for the final answer, and
    /// always a closing `AgentDone`, all tagged with the client-assigned
    /// `stream`. No reply. Cancellable by `stream`.
    /// `history` replays recent turns so follow-ups resolve; `context` carries
    /// client-side grounding (pinned selections, debugger state) verbatim.
    AgentAsk {
        stream: u64,
        question: String,
        history: Vec<AiChatMsg>,
        context: String,
    },
    /// List a directory on the server host — for the remote folder picker, which
    /// browses before a project (hence a root) is chosen. `path` is an absolute
    /// path or `~`-relative; `None` means the login home. Not confined: the server
    /// runs as the user on their own host, so browsing their filesystem is theirs
    /// to do. Reply: `DirListing`.
    ListDir { path: Option<String> },
    /// Build the project's API documentation index (per-file documented
    /// symbols). Reply: `Docs`.
    BuildDocs,
}

/// One entry-level change to a state file holding a JSON array of objects,
/// addressed by the fields that IDENTIFY an entry rather than by an index.
///
/// Identity is the point. A client's index points into the snapshot it loaded
/// when it opened the project; by the time it saves, another client may have
/// inserted ahead of it (bookmarks and notes are stored sorted), so an
/// index-addressed change edits a different entry than the one the user
/// clicked. Every field here is supplied by the store — the server applies the
/// merge without knowing what a bookmark or a note is.
///
/// Applied by `clew_core::statefile::merge_file` (through
/// `merge_entries_checked`), which is also where the exact semantics of each
/// variant are pinned down.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateMerge {
    /// The object fields that identify an entry: `["rel", "line"]` for a
    /// bookmark, `["rel", "symbol"]` for a reading note, `["scope"]` for a
    /// walkthrough.
    pub key_fields: Vec<String>,
    /// The identifying values, in `key_fields` order.
    pub key: Vec<serde_json::Value>,
    /// What to do to that entry.
    pub edit: StateEdit,
    /// Whether an empty result means "delete the file" (the bookmark and note
    /// stores: an empty one has no file) rather than an empty array (the
    /// walkthrough library, whose loader also migrates a legacy file when its
    /// own is absent).
    pub delete_when_empty: bool,
}

impl StateMerge {
    /// Whether `entry` is the one this merge addresses.
    pub fn matches(&self, entry: &serde_json::Value) -> bool {
        self.key_fields
            .iter()
            .zip(&self.key)
            .all(|(field, want)| entry.get(field) == Some(want))
    }
}

/// Longest [`Request::EditState`] `edit_id` a server accepts.
pub const MAX_EDIT_ID_LEN: usize = 64;

/// Whether `id` may be an [`Request::EditState`] `edit_id`: 1 to
/// [`MAX_EDIT_ID_LEN`] ASCII letters, digits, `-`, `_` or `.`. The server
/// records these ids on disk, so they are held to a shape that needs no
/// escaping anywhere.
pub fn valid_edit_id(id: &str) -> bool {
    (1..=MAX_EDIT_ID_LEN).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The change [`StateMerge`] applies to the addressed entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StateEdit {
    /// Replace the entry, or append it when it is absent.
    Upsert(serde_json::Value),
    /// Drop the entry; a no-op when it is already gone.
    Remove,
    /// Drop the entry when present, append it when absent — a bookmark toggle,
    /// resolved against the file rather than against the caller's copy of it.
    Toggle(serde_json::Value),
    /// Merge `fields` into the entry.
    Patch {
        fields: serde_json::Map<String, serde_json::Value>,
        /// The entry to seed when it is absent, or `None` to make the patch a
        /// no-op then (attaching a note to a bookmark another client deleted
        /// must not recreate the bookmark).
        insert: Option<serde_json::Value>,
        /// Fields that make the entry worth keeping: when ALL of them end up
        /// blank (absent, null, false, or whitespace) the entry is dropped.
        /// Empty = never dropped.
        empty_when: Vec<String>,
    },
}

/// Why a request failed — the machine-readable half of [`Event::Error`] (the
/// message is for people). A client decides on the code, never on the text:
/// the retry this replaced compared the message string for equality, which a
/// reworded message would have silently disabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Attempted, and it did not succeed: an I/O or git failure, an
    /// embedding provider's error, work that panicked. The message says why.
    Failed,
    /// The model call a [`Request::Chat`] made failed, and how
    /// ([`ProviderFailure`]): what a client acts on — whether the call may
    /// pass another time, whether every later one fails the same way, what
    /// the user is to fix — as it does on a call it made itself. It crossed
    /// as the words the call failed with, which the client parsed back.
    Provider(ProviderFailure),
    /// Refused without being attempted: a path that would leave the project,
    /// malformed or oversized arguments, no project (or another project)
    /// open, a policy (`SpawnProcess` on a remote server). Sending the same
    /// request again is refused again.
    Refused,
    /// The handshake has not completed: before `Hello`, or after one whose
    /// version or fingerprint did not match. Every request but `Hello` is
    /// refused with this until a matching `Hello` succeeds.
    Handshake,
    /// The project's scan has not finished (after the server waited a bounded
    /// time for it). The one code a client retries on — and only for
    /// idempotent requests.
    NotReady,
    /// Stopped before it finished because the client no longer wants it: a
    /// `Chat` or an `Embed` it sent `Cancel` for, or one a project switch
    /// stopped (a new `OpenProject` stops all of the old project's AI work),
    /// and an `OpenProject` a newer one superseded. Streamed work
    /// (`ChatStream`, `AgentAsk`) says the same in its terminal notification
    /// instead, as [`StreamOutcome::Stopped`]. Not a failure — the caller
    /// asked for this — so nothing to report; a client that already stopped
    /// waiting for the reply drops it.
    Cancelled,
}

/// What failed when a model call a server made failed ([`ErrorCode::Provider`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderFailure {
    /// The provider answered with an HTTP error status: the status, its own
    /// kind of error (`invalid_api_key`, `model_not_found`, …) when it named
    /// one, and what it said.
    Status {
        code: u16,
        kind: Option<String>,
        message: String,
    },
    /// The request never reached the provider: the name lookup, the
    /// connection, a proxy, TLS — after the resends such failures get.
    Unreached,
    /// The request was on the wire, and the connection failed afterwards:
    /// the provider may be answering it.
    Broken,
    /// The provider reported a failure in the middle of its answer.
    Stream,
    /// The provider's answer could not be used: not what its API sends, or
    /// no text in it.
    Unusable,
    /// The request could not be made as the settings say: an endpoint that
    /// is not one, or that redirects elsewhere.
    Settings,
}

/// How a streamed piece of work (`ChatStream`, `AgentAsk`) ended: the payload
/// of its terminal notification. Typed because the three call for different
/// things from a client — a Stop the user pressed is not a failure to report,
/// and it used to arrive as an error STRING the client compared against one
/// spelling (a server that said `cancelled` where the client expected
/// `stopped` turned every Stop into "Ask failed").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamOutcome {
    /// Finished: everything it produced has been delivered.
    Done,
    /// Stopped on request — a `Cancel` for its stream, a project switch, a
    /// client that went away — before it finished.
    Stopped,
    /// It failed; the message is for the user.
    Failed(String),
}

/// Why a host will not send a file as a source's text ([`Event::Sources`]).
/// Typed because each is said to the user as what it is: a Latin-1 file and
/// a link out of the project were both "not a text file".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Not UTF-8 text: another encoding, or binary.
    NotUtf8,
    /// Not a plain file: a link that does not lead out of the project —
    /// dangling or not — a FIFO, a device.
    NotPlainFile,
    /// Not a file of the project: a link out of it, or reached through one,
    /// or named by a path that leaves it.
    OutsideProject,
}

/// Server → client: replies, stream events and notices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Handshake accepted. `fingerprint` is the server build's
    /// [`SCHEMA_FINGERPRINT`]; the client verifies it in turn. FROZEN like
    /// [`Request::Hello`]: an older server answers Ready with its own version
    /// (and, before v7, no fingerprint) and must still parse, so the client
    /// can name the mismatch.
    Ready {
        protocol: u32,
        #[serde(default)]
        fingerprint: String,
    },
    /// The project tree: the reply to `OpenProject`, and a watcher
    /// notification after a structural change. The directory structure, the
    /// flat list of file rels, and whether the scan hit the entry cap. `root`
    /// names the project it describes, so a notification for a project the
    /// client has already left can be recognized and dropped.
    Tree {
        root: String,
        /// Scan order, stamped when the scan STARTED from one counter for the
        /// server's lifetime (never reset; the first scan is 1). A scan that
        /// started later saw every change an earlier one did, so the client
        /// applies a tree only when its `seq` is above the last one it
        /// applied — an `OpenProject` reply can land after a watcher rescan
        /// that began after it.
        seq: u64,
        tree: DirNode,
        files: Vec<Rel>,
        truncated: bool,
        /// The files among `files` that git tracks but the walk would have
        /// left out — ignored by the repository's rules, or under a
        /// build-output directory name — so the file tree can mark them, as
        /// it does a locally scanned project's. Only their count crossed
        /// before (in the scan's status notice), and a remote project's tree
        /// marked none.
        tracked_ignored: Vec<Rel>,
    },
    /// A file's highlighted content (a reply to `ReadFile`).
    FileContent {
        rel: Rel,
        /// Raw file text. The client keeps it for copy fidelity (tabs) and for
        /// correct LSP line/column positions, which index the raw source — the
        /// highlighted `lines` are cleaned (tabs expanded) and can't serve those.
        source: String,
        lines: Vec<HlLine>,
        /// Outline symbols in the file.
        symbols: Vec<Symbol>,
        /// (signature line, doc comment) pairs.
        docs: Vec<(usize, String)>,
        /// 0-based lines gated off by an inactive `#[cfg]` (dimmed).
        inactive: Vec<usize>,
    },
    /// A parsed Jupyter notebook (a reply to `ReadFile` on a `.ipynb`): its
    /// cells ready to render, outline entries (cells/headings) in projection
    /// lines, and the script projection the client uses as the file's text.
    NotebookContent {
        rel: Rel,
        /// Notebook language key (highlighting already applied server-side).
        language: String,
        cells: Vec<NotebookCell>,
        /// Cell/heading outline in projection-line space.
        symbols: Vec<Symbol>,
        /// The jupytext-style `# %%` projection of the whole notebook.
        projection: String,
    },
    /// One file's git blame + change status (a reply to `GitInfo`). `None` when
    /// the file is untracked or not in a repo.
    GitInfo { rel: Rel, info: Option<GitInfo> },
    /// Reply to `Stats`: the code-statistics report for `root`.
    Stats { root: String, report: StatsReport },
    /// Reply to `ProjectCalls`: the call graph for `root`, with
    /// project-relative node paths.
    ProjectCalls { root: String, graph: CallGraph },
    /// Reply to `ReadSources`, for the batch's rels in order: the readable
    /// sources the reply had room for; `missing`, the rels that do not exist
    /// on the host; `too_large`, those too large to explain — over the
    /// per-file cap, or too big for any reply — each with its size in bytes;
    /// `refused`, those that are no plain text file of the project — a rel
    /// shaped to leave it, whatever is there, or a file that is there and is
    /// not one — each with why ([`Refusal`]); `unreadable`, those that could
    /// not be read — this user may not, the path could not be looked up, the
    /// read failed, or the project's folder is not there, under which nothing
    /// is — each with the error the read met; and `deferred`, the rels the reply
    /// had no room left for, not looked at, which the client asks for again.
    /// Every reply settles the first rel of its batch, so asking again always
    /// gets further. An unreadable rel is not the same as a gone one: the
    /// Explain pass keeps what it recorded for such a file, and drops it for
    /// a missing, too large or refused one. A rel in none of these lists
    /// changed while the host read it — a save, a checkout — which says
    /// nothing of what it is: it is read again next time.
    Sources {
        root: String,
        files: Vec<(Rel, String)>,
        missing: Vec<Rel>,
        too_large: Vec<(Rel, u64)>,
        refused: Vec<(Rel, Refusal)>,
        unreadable: Vec<(Rel, String)>,
        deferred: Vec<Rel>,
    },
    /// Reply to `Git`: the operation's answer, in the [`GitResult`] variant of
    /// the requested [`GitOp`].
    GitResult { root: String, result: GitResult },
    /// One project state file's text (reply to `ReadState`). `root` names
    /// the project it belongs to, so a late reply from a project already
    /// left cannot seed the next one's state.
    StateContent {
        root: String,
        rel: Rel,
        text: Option<String>,
    },
    /// One project state file reached the disk (the reply to a successful
    /// [`Request::WriteState`]). Without it a client that queued a write into
    /// a dead-but-undetected transport cleared its "unsaved" mark on a write
    /// that never happened. Correlated by the request id.
    StateWritten { root: String, rel: Rel },
    /// A [`Request::EditState`] was applied — or had been, for a replay of
    /// its `edit_id` — and `text` is the file now, AFTER the merge (`None`
    /// when the store ended up empty and its file was deleted).
    /// The merged content comes back because it, not the client's copy, is
    /// the truth.
    StateEdited {
        root: String,
        rel: Rel,
        text: Option<String>,
    },
    /// Reply to `Search`. `error` carries a pattern or glob compile failure
    /// so the client can explain an empty result. `skipped` lists files the
    /// search could not read (at most a bounded few; `skipped_total` counts
    /// them all), so a search that covered less than the project says so.
    SearchResults {
        hits: Vec<SearchHit>,
        error: Option<String>,
        skipped: Vec<SkippedFile>,
        skipped_total: usize,
    },
    /// Files created / changed / deleted in the open project (a watcher
    /// notification).
    FilesChanged { root: String, rels: Vec<Rel> },
    /// The project-wide symbol snapshot, extracted where the files live. Sent
    /// (as a notification) after the `OpenProject` scan commits (`full`), and
    /// as per-file updates from the watcher (`full: false`, where an entry
    /// with no symbols means the file is gone or no longer parses). This is
    /// what lets a REMOTE client build its symbol index without ever reading
    /// remote-pathed files from its own disk — a same-pathed local file is a
    /// different machine's data.
    ProjectSymbols {
        root: String,
        /// Monotonic publication order, stamped under one lock (server
        /// lifetime, never reset). A full snapshot is built off the request
        /// loop and can land AFTER partial updates the watcher sent while it
        /// was building — without an order, the stale full clears the newer
        /// partials' files from the client index. The client drops any event
        /// whose `seq` is not greater than the last one applied.
        seq: u64,
        full: bool,
        files: Vec<FileSymbols>,
        /// The `module` line of the project's `go.mod` — resolution metadata
        /// the client must not read off its own disk.
        go_module: Patch<String>,
        /// The package `name:` of the project's `pubspec.yaml`.
        dart_package: Patch<String>,
        /// The Rust type/trait structure index — the hover peek's
        /// "implements / implementors" data, extracted where the files live.
        structure: Patch<StructureIndex>,
    },
    /// Bytes from a spawned process's stdout (a notification keyed by `proc`),
    /// at most [`MAX_PROCESS_CHUNK`] of them.
    ProcessOutput {
        proc: u64,
        #[serde(with = "crate::bytes")]
        data: Vec<u8>,
    },
    /// The spawn for `proc` succeeded: the OS process exists and its stdin
    /// queue (buffering `ProcessInput` since the spawn request) now drains
    /// into a live pipe. Every spawn ends in exactly one of `ProcessStarted`
    /// or `ProcessExited`.
    ProcessStarted { proc: u64 },
    /// A spawned process exited — or never started (resolve/spawn failure).
    ProcessExited { proc: u64, code: Option<i32> },
    /// Reply to `SpawnAdapter`: the adapter under `proc` is running, and
    /// `launch` is the serialized launch-request body for it (host-native
    /// paths). Failures reply as `Error` (plus `ProcessExited` for `proc`).
    AdapterSpawned { proc: u64, launch: String },
    /// The full text of a `Chat` completion (a reply to `Chat`).
    ChatResult { text: String },
    /// One token of a `ChatStream`, tagged with the request's `stream` id
    /// (a notification; many arrive per request).
    ChatDelta { stream: u64, text: String },
    /// A `ChatStream` ended (a notification), and how.
    ChatStreamDone { stream: u64, outcome: StreamOutcome },
    /// Embedding vectors (a reply to `Embed`), one per input text.
    Embeddings { vecs: Vec<Vec<f32>> },
    /// A directory's contents on the server host (a reply to `ListDir`), for the
    /// remote folder picker. `path` is the resolved absolute directory; `parent`
    /// is its parent (`None` at the filesystem root, for the "up" control).
    /// `omitted` counts the entries left out past the listing cap
    /// (directories are listed first, so it is files that go) — `0` when the
    /// listing is complete.
    DirListing {
        path: String,
        parent: Option<String>,
        entries: Vec<DirEntry>,
        omitted: usize,
    },
    /// The project's API documentation index (a reply to `BuildDocs`), grouped
    /// by file. Files with no documentable symbols are omitted. `root` names
    /// the project the index was built for — the build is slow, so its result
    /// can arrive after the client switched projects.
    Docs { root: String, files: Vec<DocFile> },
    /// One tool call an agent turn made (a notification): what it did, for the
    /// step chips in the Ask panel. `refs` are click-through code locations.
    AgentStep {
        stream: u64,
        /// Tool name (drives the chip icon), e.g. "search" / "read" / "outline".
        tool: String,
        /// Human-readable one-liner, e.g. `search "scroll_offset" → 6 hits`.
        title: String,
        refs: Vec<AgentRef>,
    },
    /// One token of an agent turn's final answer (a notification).
    AgentDelta { stream: u64, text: String },
    /// An agent turn ended (a notification), and how.
    AgentDone { stream: u64, outcome: StreamOutcome },
    /// A one-line notice for the status bar about the connection's background
    /// work — never a reply, never about one request: the project cannot be
    /// watched (or the watcher reported trouble, so external changes may be
    /// missed), a project scan skipped entries or listed tracked files the
    /// ignore rules hide (sent right behind the `Tree` it describes, at open
    /// and after a watcher rescan), a proxied process ended on its own with
    /// an error, the host's `lsp.toml` options were withheld pending approval.
    Status { message: String },
    /// Reply to `LspResolve` / `LspInstall`: what stands between the client
    /// and a running `language` server on this host. `root` is the project
    /// the resolution was computed against — the client must drop replies
    /// whose root is no longer the open project, or a resolution from
    /// project A could drive an approval/install consent shown for B.
    LspResolved {
        language: String,
        root: String,
        resolution: LspResolution,
    },
    /// A request failed; `code` says how (see [`ErrorCode`]). Always a
    /// [`ServerMessage::Reply`] — unsolicited trouble is [`Event::Status`].
    Error { code: ErrorCode, message: String },
}

impl Event {
    /// An [`Event::Error`].
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Event {
        Event::Error {
            code,
            message: message.into(),
        }
    }
}

/// The server-side state of a language server, as an explicit enum — the
/// states demand *different* client actions (start / ask approval / ask
/// install consent / give up), so collapsing any two of them (the old
/// `Option<LspCommandSpec>`) mis-routed the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspResolution {
    /// Installed (or toolchain-provided): `SpawnLsp` will run it.
    Ready {
        /// The `init_options` from the host's `lsp.toml` — present only once
        /// they are APPROVED. The LSP handshake happens client-side even for a
        /// remote server, so the client needs the options of the host that
        /// owns the config. A JSON value, not a JSON string: a string the
        /// client parsed with `.ok()` dropped unparseable options silently.
        init_options: Option<serde_json::Value>,
        /// Set instead when the host's config asks for `init_options` the
        /// user has not approved: what the approval needs, so the client can
        /// raise the same modal the [`LspResolution::Command`] case does.
        /// Without it the gate is one-way — the fingerprint is derived from
        /// the HOST's server/version/args, which the client cannot see, so a
        /// config withheld once would be withheld forever.
        withheld: Option<LspOptionsSpec>,
    },
    /// The repository's own `lsp.toml` names a `command`: it runs only after
    /// the user approves this exact command line and fingerprint.
    Command(LspCommandSpec),
    /// Store-managed but not installed on this host. The client asks the
    /// user; consent arrives as an `LspInstall` request carrying `consent`
    /// back, and covers exactly the install it names.
    NeedsInstall {
        server: String,
        version: String,
        /// One line describing what installing will do, for the consent
        /// prompt (mirrors the local consent dialog's description).
        describe: String,
        /// A digest (hex) of everything the install would do — the verified
        /// download, or the toolchain command with every argument — and where
        /// it lands. Opaque to the client, which only hands it back.
        consent: String,
    },
    /// Nothing can run for this language on this host.
    Unsupported { message: String },
}

/// A repo-specified language-server command, resolved on the host that would
/// run it: the exact command line plus the fingerprint (which hashes the
/// executable's bytes) an approval must match.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspCommandSpec {
    pub command: String,
    pub args: Vec<String>,
    pub server: String,
    pub version: String,
    pub fingerprint: String,
    /// The `init_options` from the host's `lsp.toml` (the LSP handshake
    /// happens client-side; see [`LspResolution::Ready`]).
    pub init_options: Option<serde_json::Value>,
}

/// The other half of what a repo's `lsp.toml` can ask for, when it asks for it
/// ALONE: `init_options` with no `command`. There are no command bytes to
/// hash, so the fingerprint is taken over the options plus the server /
/// version / args they were written for — all of which belong to the host that
/// owns the config, which is why the client is told them rather than deriving
/// them.
///
/// Sent only to DESCRIBE what was withheld. The options travel here for the
/// approval modal to display; the ones that reach `initialize` are the ones
/// the next resolve returns in [`LspResolution::Ready::init_options`], re-read
/// and re-fingerprinted on the host, so an `lsp.toml` edited while the modal
/// sat open is asked about again instead of riding on this answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspOptionsSpec {
    pub server: String,
    pub version: String,
    pub args: Vec<String>,
    /// The value an `LspApprovals` entry must carry for these options to be
    /// sent — computed on the host, since only it knows the three fields above.
    pub fingerprint: String,
    /// The withheld `init_options` themselves, so the modal can show what is
    /// being approved. Approving them unseen would be approving the payload
    /// blind: servers read these as a place to name programs they then run.
    pub options: serde_json::Value,
}

/// The framed message a client sends: a correlated request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: RequestId,
    pub request: Request,
}

/// The framed message a server sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// The answer to the request with this `id` (see [`Request`] for which
    /// requests are answered).
    Reply { id: RequestId, event: Event },
    /// Everything not tied to one request: streamed output (`ProcessOutput`,
    /// `ChatDelta`, `AgentStep`, …), watcher publications (`Tree`,
    /// `FilesChanged`, `ProjectSymbols`), process lifecycle (`ProcessStarted`,
    /// `ProcessExited`), and `Status` notices.
    Notification { event: Event },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An edit id is recorded on disk by the server, so only a plain,
    /// bounded token is one.
    #[test]
    fn an_edit_id_is_a_plain_bounded_token() {
        for good in ["a", "0f3a9c-17", "w1.edit_2", &"x".repeat(MAX_EDIT_ID_LEN)] {
            assert!(valid_edit_id(good), "{good:?}");
        }
        for bad in [
            "",
            "has space",
            "../up",
            "a/b",
            "é",
            "new\nline",
            &"x".repeat(MAX_EDIT_ID_LEN + 1),
        ] {
            assert!(!valid_edit_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn messages_round_trip_through_serde() {
        let target = TargetSpec {
            label: "Host (macos)".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            family: "unix".into(),
        };
        let msg = ClientMessage {
            id: 7,
            request: Request::ReadFile {
                rel: "src/main.rs".into(),
                target,
            },
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 7);
        assert!(matches!(back.request, Request::ReadFile { rel, .. } if rel == "src/main.rs"));

        let ev = ServerMessage::Reply {
            id: 7,
            event: Event::FileContent {
                rel: "src/main.rs".into(),
                source: "fn main".into(),
                lines: vec![HlLine {
                    spans: vec![("fn".into(), Some(10)), (" main".into(), None)],
                }],
                symbols: vec![Symbol {
                    name: "main".into(),
                    kind: "function".into(),
                    line: 1,
                    end_line: 3,
                }],
                docs: vec![(1, "entry point".into())],
                inactive: vec![7, 8],
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        let ServerMessage::Reply {
            id: 7,
            event:
                Event::FileContent {
                    rel,
                    source,
                    lines,
                    symbols,
                    docs,
                    inactive,
                },
        } = back
        else {
            panic!("wrong envelope or variant: {json}");
        };
        assert_eq!(rel, "src/main.rs");
        assert_eq!(source, "fn main");
        // The unstyled span keeps its `None` (default foreground) apart from
        // a styled one: that distinction is the whole of a span's color.
        assert_eq!(
            lines[0].spans,
            vec![("fn".to_string(), Some(10)), (" main".to_string(), None)]
        );
        assert_eq!((symbols[0].line, symbols[0].end_line), (1, 3));
        assert_eq!(docs, vec![(1, "entry point".to_string())]);
        assert_eq!(inactive, vec![7, 8]);
    }

    #[test]
    fn agent_messages_round_trip_through_serde() {
        let ask = ClientMessage {
            id: 9,
            request: Request::AgentAsk {
                stream: 3,
                question: "where is the scroll offset clamped?".into(),
                history: vec![AiChatMsg {
                    role: "user".into(),
                    content: "hi".into(),
                }],
                context: "### Selected code\n```rust\nfn f() {}\n```".into(),
            },
        };
        let json = serde_json::to_string(&ask).unwrap();
        let back: ClientMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.request, Request::AgentAsk { stream: 3, .. }));

        let step = ServerMessage::Notification {
            event: Event::AgentStep {
                stream: 3,
                tool: "search".into(),
                title: "search \"clamp\" → 4 hits".into(),
                refs: vec![AgentRef {
                    rel: "src/editor/viewer.rs".into(),
                    line: Some(355),
                }],
            },
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        let ServerMessage::Notification {
            event: Event::AgentStep { refs, .. },
        } = back
        else {
            panic!("wrong variant");
        };
        assert_eq!(refs[0].line, Some(355));
    }

    /// What the probe line's consumers rely on, checked against the line
    /// itself rather than against a second copy of its format string: the
    /// SSH bootstrap compares it byte for byte with a deployed binary's
    /// `--version` output, so it must be ONE line of plain tokens, and it must
    /// carry this build's version and fingerprint where a reader looks.
    #[test]
    fn the_version_line_names_the_protocol_and_the_build() {
        let line = version_line();
        assert!(
            line.bytes().all(|b| b.is_ascii_graphic() || b == b' '),
            "one printable line, nothing a shell or a byte compare could mangle: {line:?}"
        );
        let tokens: Vec<&str> = line.split(' ').collect();
        let [name, protocol, version, fingerprint, value] = tokens[..] else {
            panic!("five space-separated tokens expected: {line:?}");
        };
        assert_eq!(
            (name, protocol, fingerprint),
            ("clew-server", "protocol", "fingerprint")
        );
        assert_eq!(version.parse::<u32>().ok(), Some(PROTOCOL_VERSION));
        assert_eq!(value, SCHEMA_FINGERPRINT);
        // The fingerprint is a plain 16-digit hex token: it lands in a remote
        // directory name and a shell script.
        assert_eq!(value.len(), 16);
        assert!(value.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
