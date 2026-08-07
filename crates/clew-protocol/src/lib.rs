//! The wire contract between the clew client (GUI) and a clew-server.
//!
//! The client renders; the server, running where the code lives (in-process
//! locally, or on a remote machine over SSH), does everything system-facing —
//! filesystem, git, LSP, DAP, indexing, and AI orchestration — and streams back
//! only what the UI needs. This crate holds the messages and the data types that
//! cross that seam, so both sides depend on one contract.
//!
//! The message set grows as backend modules are extracted into the server; this
//! is the initial, representative slice covering the core reading flows.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

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
pub const PROTOCOL_VERSION: u32 = 6;

/// Hard cap on one serialized frame (a JSON line) in either direction. A real
/// frame is at most a request or reply around one file's content — nowhere
/// near this; without a cap a broken or hostile peer could grow one "line"
/// without bound before the parser ever sees it. A peer that exceeds it is
/// dropped: past an oversized frame there is no way back into sync.
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// A path relative to the project root (the wire never carries absolute,
/// machine-specific paths for project files).
pub type Rel = String;

/// Correlates a request with its reply.
pub type RequestId = u64;

/// Identifies a long-lived subscription (a watch, an LSP session, a debug
/// session, an in-flight explanation) so its stream of events can be routed and
/// cancelled.
pub type SubId = u64;

// -- shared data types -------------------------------------------------------

/// A file or folder in the project tree.
/// A directory node: sub-directories first, then files (both sorted). The server
/// builds it; the client renders it verbatim, so a server-provided tree is
/// identical to a local scan (no client-side rebuild to drift).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DirNode {
    pub dirs: Vec<(String, DirNode)>,
    pub files: Vec<String>,
}

/// One entry in a `DirListing` — a child of the directory being browsed in the
/// remote folder picker. `is_dir` is what makes a row navigable vs. a leaf.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// One file's documented API — a group in the Docs tree (a reply piece of
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

/// A search hit (text search or find).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rel: Rel,
    pub line: usize,
    pub preview: String,
}

/// One file's entry in a `ProjectSymbols` snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSymbols {
    pub rel: Rel,
    pub symbols: Vec<IndexSymbol>,
    /// The file's raw (unresolved) import specifiers — the extraction half
    /// of the import graph; the client resolves them over the file set.
    #[serde(default)]
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
    #[serde(default)]
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
    #[serde(default)]
    pub is_test: bool,
}

/// Which side makes the outbound AI calls (chat + embeddings), chosen at connect
/// time — the server itself (remote endpoint / network) or delegated back to the
/// client (local key / network).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiEndpoint {
    /// The server calls the AI provider directly.
    Server,
    /// The server forwards prompts to the client, which calls the provider.
    Client,
}

/// Chat/LLM provider config (the provider is a slug the server maps back). The
/// client sends this so the server can make AI calls on the chosen endpoint.
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
    /// A raster image (PNG/JPEG bytes, base64-decoded server-side).
    Image {
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

/// Client → server. User-initiated operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Handshake: agree on protocol version + AI endpoint choice.
    Hello { protocol: u32, ai: AiEndpoint },
    /// Open a project rooted at this server-side path; server replies with the
    /// tree and begins indexing.
    OpenProject { root: String },
    /// Read a file for display: the reply (`FileContent`) carries its highlighted
    /// lines, outline symbols, doc comments, and inactive `#[cfg]` lines. `target`
    /// selects which cfg branches count as active.
    ReadFile { rel: Rel, target: TargetSpec },
    /// Text search across the project, with the sidebar's toggles and globs.
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
    /// Semantic search over the explanation-summary embeddings.
    Find { query: String },
    /// Outline / symbols for one file.
    Outline { rel: Rel },
    /// Per-line blame + change status for the gutter.
    GitInfo { rel: Rel },
    /// Read one project state file (`<root>/.clew/<rel>`) where the project
    /// lives. This is how a REMOTE client loads its per-project session
    /// state (history, bookmarks, notes, reading target) — the same-pathed
    /// files on its own disk belong to a different machine. The reply is
    /// `StateContent` (text `None` when missing or refused).
    ReadState { rel: Rel },
    /// Write (or, with `text: None`, delete) one project state file under
    /// `<root>/.clew/`. Applied with the same rules as every local state
    /// write: atomic, size-capped, never through a symlinked `.clew`. No
    /// reply on success; failures come back as an `Error`.
    WriteState { rel: Rel, text: Option<String> },
    /// Watch the project for changes (server streams `FilesChanged`).
    Watch,
    /// Spawn a subprocess (e.g. a language server) on the server and proxy its
    /// stdio. `proc` is a client-assigned handle correlating input/output/exit,
    /// so a language server always runs where the code lives. Bytes are framed as
    /// the process emits them — the caller reassembles the protocol.
    SpawnProcess {
        proc: u64,
        cmd: String,
        args: Vec<String>,
        /// Working directory; defaults to the project root when `None`.
        cwd: Option<String>,
    },
    /// Start the language server for `language`, resolved and provisioned on the
    /// server (where the code lives) — the client never ships a binary path, so
    /// the remote uses its own LSP. Proxied like `SpawnProcess` via `proc`.
    /// A repo-specified `command` runs only when its fingerprint is approved
    /// (see `LspApprovals`).
    SpawnLsp { proc: u64, language: String },
    /// Resolve AND spawn the debug adapter for `lang` (a client `Lang` slug:
    /// "native", "python", "dart", …) on this host — where the debuggee
    /// lives — proxying its stdio under `proc` like `SpawnProcess`. Only
    /// stdio-transport adapters work remotely (TCP ones listen on the
    /// server's loopback, unreachable from the client). The reply is
    /// `AdapterSpawned`, carrying the adapter-specific `launch` request body
    /// built with THIS host's paths.
    SpawnAdapter {
        proc: u64,
        lang: String,
        program: String,
        args: Vec<String>,
    },
    /// Resolve what `SpawnLsp` for `language` would execute, without running
    /// anything. The reply is `LspResolved`; when the project's own `lsp.toml`
    /// names a `command`, it carries the full command line and fingerprint so
    /// the client can show the user exactly what would run and record an
    /// approval against it.
    LspResolve { language: String },
    /// Install the store-managed server for `language` on this host. Sent
    /// only after the user consented in the client — the server itself never
    /// initiates a download or toolchain install. The reply is `LspResolved`
    /// with the post-install state (`Ready`, or `Unsupported` on failure).
    LspInstall { language: String },
    /// The user's language-server command approvals for the open project
    /// (`language` → fingerprint), recorded client-side and pushed here so the
    /// server's spawn paths (SpawnLsp, the Ask agent's semantic tools) honor
    /// them. Replaces the previous set.
    LspApprovals { approvals: Vec<(String, String)> },
    /// Write bytes to a spawned process's stdin.
    ProcessInput { proc: u64, data: Vec<u8> },
    /// Terminate a spawned process.
    ProcessKill { proc: u64 },
    /// Generate an explanation for a node (file / symbol).
    Explain { rel: Rel, symbol: Option<String> },
    /// Cancel a subscription / in-flight stream.
    Cancel { sub: SubId },
    /// Give the server the AI provider config to use when it makes calls
    /// (endpoint = Server). Sent at connect and whenever the config changes.
    SetAiConfig {
        chat: Option<AiChatConfig>,
        embed: Option<AiEmbedConfig>,
    },
    /// A chat/completion the server runs with its stored chat config. Batch:
    /// the reply carries the whole response.
    Chat {
        system: String,
        messages: Vec<AiChatMsg>,
        max_tokens: u32,
    },
    /// Embed texts with the server's stored embedding config.
    Embed { texts: Vec<String> },
    /// Like `Chat`, but streamed: the server sends `ChatDelta` notifications as
    /// tokens arrive and a final `ChatStreamDone`, both tagged with `stream`
    /// (a client-assigned id correlating the deltas to this request).
    ChatStream {
        stream: u64,
        system: String,
        messages: Vec<AiChatMsg>,
        max_tokens: u32,
    },
    /// Run an agent turn for the Ask panel: the server explores the project with
    /// tools (search / read / outline / …) and streams its progress back —
    /// `AgentStep` per tool call, `AgentDelta` tokens for the final answer, and
    /// a closing `AgentDone`, all tagged with the client-assigned `stream` id.
    /// `history` replays recent turns so follow-ups resolve; `context` carries
    /// client-side grounding (pinned selections, debugger state) verbatim.
    AgentAsk {
        stream: u64,
        question: String,
        history: Vec<AiChatMsg>,
        context: String,
    },
    /// Stop an in-flight agent turn; the server finishes with `AgentDone`.
    AgentStop { stream: u64 },
    /// List a directory on the server host — for the remote folder picker, which
    /// browses before a project (hence a root) is chosen. `path` is an absolute
    /// path or `~`-relative; `None` means the login home. Not confined: the server
    /// runs as the user on their own host, so browsing their filesystem is theirs
    /// to do. The reply is a `DirListing`.
    ListDir { path: Option<String> },
    /// Build the project's API documentation index (per-file documented
    /// symbols). The reply is `Docs`. Rebuilt by the server on file changes.
    BuildDocs,
}

/// Server → client. Replies and streamed events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Handshake accepted.
    Ready { protocol: u32 },
    /// The project tree (a reply to `OpenProject`, and a watcher notification
    /// after a structural change): the directory structure, the flat list of
    /// file rels, and whether the scan hit the entry cap. `root` names the
    /// project it describes, so a notification for a project the client has
    /// already left can be recognized and dropped.
    Tree {
        root: String,
        tree: DirNode,
        files: Vec<Rel>,
        truncated: bool,
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
    /// The symbol index for the whole project finished (re)building.
    SymbolIndexDone,
    /// One file's outline (a reply to `Outline`).
    Outline { rel: Rel, symbols: Vec<Symbol> },
    /// One file's git blame + change status (a reply to `GitInfo`). `None` when
    /// the file is untracked or not in a repo.
    GitInfo { rel: Rel, info: Option<GitInfo> },
    /// One project state file's text (reply to `ReadState`). `root` names
    /// the project it belongs to, so a late reply from a project already
    /// left cannot seed the next one's state.
    StateContent {
        root: String,
        rel: Rel,
        text: Option<String>,
    },
    /// Search results (a reply to `Search` / `Find`). `error` carries a pattern
    /// or glob compile failure so the client can explain an empty result.
    SearchResults {
        hits: Vec<SearchHit>,
        error: Option<String>,
    },
    /// Files created / changed / deleted on the server (a `Watch` stream event).
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
        full: bool,
        files: Vec<FileSymbols>,
        /// The `module` line of the project's `go.mod` (full snapshots only)
        /// — resolution metadata the client must not read off its own disk.
        #[serde(default)]
        go_module: Option<String>,
        /// The package `name:` of the project's `pubspec.yaml` (full
        /// snapshots only).
        #[serde(default)]
        dart_package: Option<String>,
    },
    /// Bytes from a spawned process's stdout (a stream, keyed by `proc`).
    ProcessOutput { proc: u64, data: Vec<u8> },
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
    /// A ready explanation (markdown), for a node.
    Explanation {
        rel: Rel,
        symbol: Option<String>,
        markdown: String,
    },
    /// The full text of a `Chat` completion (a reply to `Chat`).
    ChatResult { text: String },
    /// One token of a `ChatStream`, tagged with the request's `stream` id
    /// (a notification; many arrive per request).
    ChatDelta { stream: u64, text: String },
    /// A `ChatStream` finished (a notification): `error` is set if it failed.
    ChatStreamDone { stream: u64, error: Option<String> },
    /// Embedding vectors (a reply to `Embed`), one per input text.
    Embeddings { vecs: Vec<Vec<f32>> },
    /// A directory's contents on the server host (a reply to `ListDir`), for the
    /// remote folder picker. `path` is the resolved absolute directory; `parent`
    /// is its parent (`None` at the filesystem root, for the "up" control).
    DirListing {
        path: String,
        parent: Option<String>,
        entries: Vec<DirEntry>,
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
    /// One token of an agent turn's final answer.
    AgentDelta { stream: u64, text: String },
    /// An agent turn finished; `error` is set if it failed or was stopped.
    AgentDone { stream: u64, error: Option<String> },
    /// A one-line status update for the status bar.
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
    /// An operation failed.
    Error { message: String },
}

/// The server-side state of a language server, as an explicit enum — the
/// states demand *different* client actions (start / ask approval / ask
/// install consent / give up), so collapsing any two of them (the old
/// `Option<LspCommandSpec>`) mis-routed the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspResolution {
    /// Installed (or toolchain-provided): `SpawnLsp` will run it.
    Ready {
        /// The `init_options` from the host's `lsp.toml`, as a JSON string.
        /// The LSP handshake happens client-side even for a remote server,
        /// so the client needs the options of the host that owns the config.
        init_options: Option<String>,
    },
    /// The repository's own `lsp.toml` names a `command`: it runs only after
    /// the user approves this exact command line and fingerprint.
    Command(LspCommandSpec),
    /// Store-managed but not installed on this host. The client asks the
    /// user; consent arrives as an `LspInstall` request.
    NeedsInstall {
        server: String,
        version: String,
        /// One line describing what installing will do, for the consent
        /// prompt (mirrors the local consent dialog's description).
        describe: String,
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
    /// The `init_options` from the host's `lsp.toml`, as a JSON string (the
    /// LSP handshake happens client-side; see [`LspResolution::Ready`]).
    pub init_options: Option<String>,
}

/// The framed message a client sends: a correlated request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMessage {
    pub id: RequestId,
    pub request: Request,
}

/// The framed message a server sends: either a reply to a request, or an
/// unsolicited notification (a stream event / status).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// A reply correlated to a `RequestId`; `sub` is set when the request opened
    /// a subscription whose further events arrive as `Notification`s.
    Reply {
        id: RequestId,
        sub: Option<SubId>,
        event: Event,
    },
    /// A stream event or spontaneous status, not tied to a single request.
    Notification { sub: Option<SubId>, event: Event },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_through_serde() {
        // The contract must serialize both ways (bincode/json framing later).
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
            sub: None,
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
        let _back: ServerMessage = serde_json::from_str(&json).unwrap();
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
            sub: None,
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
            ..
        } = back
        else {
            panic!("wrong variant");
        };
        assert_eq!(refs[0].line, Some(355));
    }
}
