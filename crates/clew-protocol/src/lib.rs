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

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. The client refuses a server whose version
/// differs (and, for a remote, fetches the matching clew-server binary).
pub const PROTOCOL_VERSION: u32 = 1;

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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeEntry {
    pub rel: Rel,
    pub is_dir: bool,
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

/// A search hit (text search or find).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub rel: Rel,
    pub line: usize,
    pub preview: String,
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
    /// Watch the project for changes (server streams `FilesChanged`).
    Watch,
    /// Generate an explanation for a node (file / symbol).
    Explain { rel: Rel, symbol: Option<String> },
    /// Cancel a subscription / in-flight stream.
    Cancel { sub: SubId },
}

/// Server → client. Replies and streamed events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Handshake accepted.
    Ready { protocol: u32 },
    /// The project tree (a reply to `OpenProject`).
    Tree { entries: Vec<TreeEntry> },
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
    /// The symbol index for the whole project finished (re)building.
    SymbolIndexDone,
    /// One file's outline (a reply to `Outline`).
    Outline { rel: Rel, symbols: Vec<Symbol> },
    /// Search results (a reply to `Search` / `Find`). `error` carries a pattern
    /// or glob compile failure so the client can explain an empty result.
    SearchResults {
        hits: Vec<SearchHit>,
        error: Option<String>,
    },
    /// Files created / changed / deleted on the server (a `Watch` stream event).
    FilesChanged { rels: Vec<Rel> },
    /// A ready explanation (markdown), for a node.
    Explanation { rel: Rel, symbol: Option<String>, markdown: String },
    /// A one-line status update for the status bar.
    Status { message: String },
    /// An operation failed.
    Error { message: String },
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
    Reply { id: RequestId, sub: Option<SubId>, event: Event },
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
            request: Request::ReadFile { rel: "src/main.rs".into(), target },
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
                lines: vec![HlLine { spans: vec![("fn".into(), Some(10)), (" main".into(), None)] }],
                symbols: vec![Symbol { name: "main".into(), kind: "function".into(), line: 1, end_line: 3 }],
                docs: vec![(1, "entry point".into())],
                inactive: vec![7, 8],
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let _back: ServerMessage = serde_json::from_str(&json).unwrap();
    }
}
