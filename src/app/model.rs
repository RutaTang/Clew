//! The shared app model: the domain types the GUI and its handlers pass
//! around, plus the async task bodies and LLM-input builders that back the
//! feature flows. Re-exported from the crate root so `crate::<Type>` holds.

use crate::app::prelude::*;
use crate::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Search,
    /// Semantic search over the embedding index.
    Semantic,
    Marks,
    /// The navigation history tree (reading trail).
    Trail,
    /// Call hierarchy for the symbol `gc` was invoked on.
    Calls,
    /// Import graph rooted at the active file.
    Imports,
    /// The guided walkthrough: an ordered, code-anchored tour.
    Walk,
    /// Reading notes and per-file "understood" progress.
    Notes,
    /// The project's API documentation surface.
    Docs,
}

/// What the WALK tab's top input does: search the saved library, or generate a
/// new tour from a scope prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkMode {
    Search,
    Walk,
}

/// The two views that share the collapsible bottom panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomTab {
    /// "Ask clew" Q&A.
    Ask,
    /// The debugger (call stack, variables, output).
    Debug,
}

/// A full-screen modal showing a project-wide graph overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The whole-project call graph (tree-sitter, name-resolved).
    ProjectCalls,
    /// The whole-project import graph.
    ProjectImports,
}

/// One function's block-detail inputs: its signature, full body, and
/// `(callee_name, summary)` context for the functions it calls.
pub(crate) type FnDetailInput = (String, String, Vec<(String, String)>);

/// A math/mermaid diagram rendered to an SVG, ready to place in the modal at a
/// fixed logical size.
#[derive(Debug, Clone)]
pub struct ExplainSvg {
    pub handle: iced::widget::svg::Handle,
    pub width: f32,
    pub height: f32,
}

/// One explanation segment prepared for display: markdown is pre-parsed once (not
/// per frame), math/mermaid carry the cache key of their rendered [`ExplainSvg`].
pub enum PreparedSeg {
    Markdown(Vec<iced::widget::markdown::Item>),
    DisplayMath(u64),
    InlineLine(Vec<PreparedInline>),
    /// A mermaid diagram: its render key, and the raw source kept as a fallback
    /// to show when the SVG isn't available (still rendering, or it failed).
    Mermaid(u64, String),
    /// A fenced code block, highlighted with clew's own tree-sitter pipeline
    /// (per-line styled spans, same palette as the editor).
    Code(Vec<crate::highlight::HlLine>),
}

/// An inline piece of a text line that mixes prose and inline math.
pub enum PreparedInline {
    Text(String),
    Math(u64),
}

/// A Jupyter notebook prepared for the native cell view.
pub struct NotebookDoc {
    /// Language key from the notebook's kernel metadata (e.g. "python").
    pub language: String,
    pub cells: Vec<NbCell>,
}

// The viewer derives Debug; prepared segments (markdown items, svg keys) have
// no useful debug form, so summarize.
impl std::fmt::Debug for NotebookDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NotebookDoc({} cells)", self.cells.len())
    }
}

/// One notebook cell, render-ready: markdown as prepared segments (math and
/// mermaid included), code as highlighted lines, outputs as widgets-to-be.
pub struct NbCell {
    /// "markdown" | "code" | "raw".
    pub kind: String,
    /// Raw cell source (markdown text / code), kept for copy and heuristics.
    pub source: String,
    /// Prepared segments for markdown/raw cells (empty for code cells).
    pub segs: Vec<PreparedSeg>,
    /// Highlighted lines for code cells (empty otherwise).
    pub lines: Vec<crate::highlight::HlLine>,
    /// 1-based first line of the cell in the script projection.
    pub proj_line: usize,
    pub outputs: Vec<NbOutput>,
    pub execution_count: Option<u64>,
}

/// A code-cell output with its display resources already built.
pub enum NbOutput {
    /// `(run, ansi_color)` spans; color indexes the 16-color ANSI palette.
    Text {
        spans: Vec<(String, Option<u8>)>,
        stderr: bool,
    },
    Image(iced::widget::image::Handle),
    Svg(iced::widget::svg::Handle),
    /// Not natively renderable (interactive widgets / HTML-only).
    Placeholder(String),
}

/// One turn in the "Ask clew" conversation.
pub struct AskTurn {
    pub question: String,
    /// Raw markdown answer, replayed to the LLM as history so follow-ups have
    /// the prior exchange in context. Accumulates token-by-token while streaming.
    pub answer_md: String,
    /// The answer rendered as ordered display segments (filled when the stream
    /// finishes; empty while streaming, when `answer_md` is shown as plain text).
    pub answer: Vec<PreparedSeg>,
    /// The retrieved nodes (with similarity scores) that grounded this answer,
    /// shown beneath it as clickable source chips.
    pub sources: Vec<(explain::Node, f32)>,
    /// The agent's exploration steps (tool calls), shown as chips above the
    /// answer. Empty for retrieval-mode turns.
    pub steps: Vec<AgentStep>,
    /// True while the answer is still streaming in.
    pub streaming: bool,
}

/// One piece of a streaming chat answer, routed from the server's `ChatDelta` /
/// `ChatStreamDone` notifications (or a local stream) to the Ask flow.
pub enum ChatStreamPiece {
    Delta(String),
    /// Stream finished; `Some` carries the error when it failed.
    Done(Option<String>),
}

/// One tool call an agent turn made, rendered as a step chip in the Ask panel.
#[derive(Debug, Clone)]
pub struct AgentStep {
    /// Tool name, driving the chip icon ("search", "read", "outline", …).
    pub tool: String,
    /// Human-readable one-liner, e.g. `search "scroll_offset" → 6`.
    pub title: String,
    /// Code locations the step touched; the first is the chip's click target.
    pub refs: Vec<(String, Option<usize>)>,
}

/// One piece of an agent turn, routed from the server's `AgentStep` /
/// `AgentDelta` / `AgentDone` notifications into the Ask flow.
pub enum AgentPiece {
    Step(AgentStep),
    Delta(String),
    /// Turn finished; `Some` carries the error when it failed or was stopped.
    Done(Option<String>),
}

/// A code selection pinned as extra context for the conversation (added by the
/// code view's "Add to Ask"; shown as a removable, clickable chip above the
/// input). Pins persist across turns until removed, and several can be attached
/// at once so distinct snippets can be asked about together.
#[derive(Debug, Clone)]
pub struct AskPin {
    pub rel: String,
    pub file: PathBuf,
    pub line: usize,
    pub code: String,
}

/// Where a debug session is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugStatus {
    /// Adapter starting / launching the program.
    Launching,
    /// The debuggee is running (not paused).
    Running,
    /// Paused at a breakpoint / step / exception.
    Stopped,
    /// The debuggee exited or the session ended.
    Terminated,
}

/// One scope of the stopped frame, with its variables loaded.
#[derive(Debug, Clone)]
pub struct DebugScope {
    pub name: String,
    pub vars: Vec<dap::Variable>,
}

/// A breakpoint on a line: unconditional, or stopping only when `condition`
/// (an expression the adapter evaluates in scope) is true.
#[derive(Debug, Clone, Default)]
pub struct Bp {
    pub condition: Option<String>,
}

/// A file's breakpoints as `(line, optional condition)` pairs — the shape the
/// DAP adapter's `setBreakpoints` takes.
pub(crate) type BpList = Vec<(usize, Option<String>)>;

/// Which stepping action to send the adapter.
#[derive(Debug, Clone, Copy)]
pub enum DebugCmd {
    Continue,
    StepOver,
    StepIn,
    StepOut,
}

/// A live debug session: the adapter handle plus the state clew shows (stack,
/// scopes, output, the current stopped line).
pub struct DebugSession {
    /// The adapter handle (None between StartDebug and the adapter being ready).
    pub client: Option<dap::DapClient>,
    pub status: DebugStatus,
    pub thread_id: Option<i64>,
    /// The call stack at the current stop (top frame first).
    pub frames: Vec<dap::StackFrame>,
    pub scopes: Vec<DebugScope>,
    /// Watch expressions re-evaluated on each stop: (expression, value).
    pub watches: Vec<(String, String)>,
    /// Program/adapter output, as (category, text) chunks.
    pub output: Vec<(String, String)>,
    /// The current stopped location (absolute file, 1-based line).
    pub current: Option<(PathBuf, usize)>,
    /// Resolved launch config for this session.
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// TCP port of the adapter (js-debug/dlv) — used to open child sessions.
    pub port: Option<u16>,
}

/// A parsed `.clew/launch.json`: what to run and (optionally) which adapter.
pub(crate) struct LaunchConfig {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    /// Optional `"type"` hint (rust/python/go/dart/node) — else inferred.
    pub(crate) type_hint: Option<String>,
}

/// The kind of LSP navigation request.
#[derive(Debug, Clone, Copy)]
pub enum GotoKind {
    Definition,
    References,
    Implementation,
    TypeDefinition,
}

/// An active hover tooltip.
#[derive(Clone)]
pub struct HoverState {
    pub line: usize,
    pub col: usize,
    pub x: f32,
    pub y: f32,
    pub text: Option<String>,
    /// The cached one-line LLM summary of the hovered symbol, if it's an
    /// explained function/method. Set synchronously; shown above the LSP text.
    pub summary: Option<String>,
    /// The LSP diagnostic (error/warning) under the cursor, if any — so hovering
    /// a red-underlined symbol shows *what* the problem is, not just the squiggle.
    pub diagnostic: Option<String>,
}

/// The "Why is this here?" popup: an LLM explanation of why a line/selection
/// exists, grounded in the commit(s) that last touched it.
pub struct BlameWhy {
    /// e.g. "Why line 42 exists" / "Why lines 40–48 exist".
    pub title: String,
    /// The cited commits `(short sha, subject)`.
    pub commits: Vec<(String, String)>,
    /// True while the LLM answer is being generated.
    pub loading: bool,
    /// The rendered answer (empty while loading).
    pub prepared: Vec<PreparedSeg>,
}

/// A time-travel session: scrub the active file (or one function's line range)
/// through its git history, viewing each past revision read-only, with the lines
/// that revision changed highlighted in the gutter.
pub struct TimeTravel {
    pub abs: PathBuf,
    pub rel: String,
    pub lang: Option<&'static str>,
    /// Whole file, or scoped to one function's line range (`git log -L`).
    pub scope: TimeScope,
    /// Commits that touched the scope, newest first.
    pub commits: Vec<git::HistCommit>,
    /// Current position in `commits` (0 = newest / most recent).
    pub idx: usize,
    /// The historical content for `commits[idx]`, built read-only.
    pub viewer: Option<viewer::Viewer>,
    /// Scroll offset of the historical view (drives its sticky headers).
    pub scroll_y: f32,
    /// Caret position, carried in from the live file and kept across revisions
    /// (and clicks) so the reader's place doesn't vanish on entry.
    pub caret: Option<(usize, usize)>,
    /// The line to bring into view (symbol scope: the function's line).
    pub focus_line: Option<usize>,
    pub loading: bool,
    /// Bumped on every enter/step so stale async results are dropped.
    pub generation: u64,
    /// LLM "what & why" summary per commit sha (cached across steps).
    pub why: HashMap<String, String>,
    pub why_loading: bool,
    /// The "story of this function" narrative (symbol scope), prepared markdown.
    pub story: Option<Vec<PreparedSeg>>,
    pub story_loading: bool,
}

/// Whether a time-travel session follows the whole file or one code block —
/// any outline symbol with a line range (function, struct, enum, class, trait,
/// interface, impl, …).
#[derive(Debug, Clone)]
pub enum TimeScope {
    File,
    Symbol {
        name: String,
        kind: String,
        start: usize,
        end: usize,
    },
}

impl TimeScope {
    pub fn symbol_name(&self) -> Option<&str> {
        match self {
            TimeScope::Symbol { name, .. } => Some(name),
            TimeScope::File => None,
        }
    }
}

/// Async-built content for one revision of a time-travel session.
#[derive(Debug, Clone)]
pub struct TimeStep {
    pub lines: Vec<highlight::HlLine>,
    pub content: String,
    pub symbols: Vec<outline::Symbol>,
    pub added: HashSet<usize>,
    pub focus_line: Option<usize>,
}

/// An open right-click navigation menu.
#[derive(Clone, Copy)]
pub struct ContextMenu {
    pub pane: usize,
    pub line: usize,
    pub col: usize,
    pub x: f32,
    pub y: f32,
}

impl GotoKind {
    /// Menu label for this navigation action.
    pub fn label(self) -> &'static str {
        match self {
            GotoKind::Definition => "Go to Definition",
            GotoKind::References => "Find References",
            GotoKind::Implementation => "Go to Implementation",
            GotoKind::TypeDefinition => "Go to Type Definition",
        }
    }

    pub(crate) fn method(self) -> &'static str {
        match self {
            GotoKind::Definition => "textDocument/definition",
            GotoKind::References => "textDocument/references",
            GotoKind::Implementation => "textDocument/implementation",
            GotoKind::TypeDefinition => "textDocument/typeDefinition",
        }
    }
    pub(crate) fn verb(self) -> &'static str {
        match self {
            GotoKind::Definition => "Looking up definition",
            GotoKind::References => "Finding references",
            GotoKind::Implementation => "Finding implementations",
            GotoKind::TypeDefinition => "Looking up type definition",
        }
    }
}

pub struct Project {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Arc<Vec<FileEntry>>,
    pub truncated: bool,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub running: bool,
    pub ran: bool,
    pub hits: Vec<SearchHit>,
    /// Last search's error (bad regex/glob), shown under the input.
    pub error: Option<String>,
    /// Match options (regex/case/whole-word) and include/exclude globs.
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub include: String,
    pub exclude: String,
}

/// A toggleable match option in the search sidebar.
#[derive(Debug, Clone, Copy)]
pub enum SearchOpt {
    Regex,
    Case,
    WholeWord,
}

/// The active pane's diff-vs-HEAD, shown in place of the code when set.
pub struct DiffState {
    pub abs: PathBuf,
    pub rel: String,
    pub lines: Vec<git::DiffLine>,
}

/// State of the language server for one language.
pub enum LspSlot {
    Starting,
    Ready(lsp::client::LspClient),
    Failed(String),
    Unsupported(String),
    /// Awaiting the user's consent to download the server (see LspConsent).
    AwaitingConsent,
}

impl LspSlot {
    pub fn label(&self) -> String {
        match self {
            LspSlot::Starting => "starting…".into(),
            // A ready server that surfaced an error (e.g. rust-analyzer couldn't
            // load the workspace) shows it — otherwise it reads "ready" while
            // every go-to-def silently returns nothing. Else: live progress
            // (indexing) when active, else "ready".
            LspSlot::Ready(client) => match client.error() {
                Some(e) => format!("⚠ {}", lsp_error_summary(&e)),
                None => client.progress().unwrap_or_else(|| "ready".into()),
            },
            LspSlot::Failed(e) => format!("error: {e}"),
            LspSlot::Unsupported(e) => e.clone(),
            LspSlot::AwaitingConsent => "download needed".into(),
        }
    }
}

/// How a pending, consent-gated provisioning will obtain the server.
#[derive(Clone)]
pub enum LspProvision {
    Download(lsp::registry::Download),
    Install(lsp::registry::Install),
}

/// A pending language-server provisioning the user must approve.
#[derive(Clone)]
pub struct LspConsent {
    pub language: String,
    pub server_name: String,
    pub version: String,
    pub provision: LspProvision,
    pub dest_dir: PathBuf,
}

impl LspConsent {
    /// One line describing what running the provisioning will do.
    pub fn describe(&self) -> String {
        match &self.provision {
            LspProvision::Download(d) => d
                .url
                .rsplit('/')
                .next()
                .map(|f| format!("download {f}"))
                .unwrap_or_else(|| "download a binary".into()),
            LspProvision::Install(i) => format!("{} (requires {} on PATH)", i.describe, i.tool),
        }
    }
}

/// A language-server command the project's own `lsp.toml` asks clew to run,
/// awaiting the user's approval. The project file is attacker-controlled when
/// the repository is untrusted, so a command it names must be shown in full and
/// confirmed before it is executed — approval is recorded against its
/// fingerprint, so an edited `lsp.toml` has to be confirmed again.
#[derive(Clone)]
pub struct PendingLspCommand {
    pub language: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub server_name: String,
    pub version: String,
    pub fingerprint: String,
}

impl PendingLspCommand {
    /// The exact command line that would run, for the confirmation dialog.
    pub fn command_line(&self) -> String {
        let mut out = self.command.to_string_lossy().into_owned();
        for a in &self.args {
            out.push(' ');
            out.push_str(a);
        }
        out
    }
}

/// Routes AI calls to clew-server (endpoint = Server) or runs them locally
/// (endpoint = Client). Cheap to clone (handles only), so each background AI
/// task takes one.
#[derive(Clone)]
pub struct AiClient {
    pub(crate) endpoint: clew_protocol::AiEndpoint,
    pub(crate) server_tx: Option<tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>>,
    pub(crate) next_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    #[allow(clippy::type_complexity)]
    pub(crate) pending: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                u64,
                tokio::sync::oneshot::Sender<Result<clew_protocol::Event, String>>,
            >,
        >,
    >,
}

impl AiClient {
    /// The request channel to use when the AI endpoint is the server.
    fn server(&self) -> Option<&tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>> {
        if self.endpoint == clew_protocol::AiEndpoint::Server {
            self.server_tx.as_ref()
        } else {
            None
        }
    }

    /// Send a request and await its correlated reply (resolved in `update`).
    async fn rpc(
        &self,
        tx: &tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
        request: clew_protocol::Request,
    ) -> Result<clew_protocol::Event, String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (otx, orx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, otx);
        tx.send(clew_protocol::ClientMessage { id, request })
            .map_err(|_| "server gone".to_string())?;
        orx.await
            .map_err(|_| "server dropped the request".to_string())?
    }

    /// A single-prompt completion.
    pub async fn complete(
        &self,
        cfg: llm::Config,
        system: &str,
        prompt: String,
        max_tokens: u32,
    ) -> Result<String, String> {
        self.complete_chat(cfg, system, vec![llm::ChatMsg::user(prompt)], max_tokens)
            .await
    }

    /// A multi-turn completion.
    pub async fn complete_chat(
        &self,
        cfg: llm::Config,
        system: &str,
        messages: Vec<llm::ChatMsg>,
        max_tokens: u32,
    ) -> Result<String, String> {
        if let Some(tx) = self.server() {
            let req = clew_protocol::Request::Chat {
                system: system.to_string(),
                messages: messages
                    .iter()
                    .map(|m| clew_protocol::AiChatMsg {
                        role: m.role_str().to_string(),
                        content: m.content.clone(),
                    })
                    .collect(),
                max_tokens,
            };
            return match self.rpc(tx, req).await? {
                clew_protocol::Event::ChatResult { text } => Ok(text),
                clew_protocol::Event::Error { message } => Err(message),
                _ => Err("unexpected reply to Chat".into()),
            };
        }
        // Client endpoint: call the provider directly (blocking HTTP off-thread).
        let system = system.to_string();
        tokio::task::spawn_blocking(move || {
            llm::complete_chat(&cfg, &system, &messages, max_tokens)
        })
        .await
        .unwrap_or_else(|_| Err("task join failed".into()))
    }

    /// Embed texts.
    pub async fn embed(
        &self,
        cfg: embed::Config,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, String> {
        if let Some(tx) = self.server() {
            return match self
                .rpc(tx, clew_protocol::Request::Embed { texts })
                .await?
            {
                clew_protocol::Event::Embeddings { vecs } => Ok(vecs),
                clew_protocol::Event::Error { message } => Err(message),
                _ => Err("unexpected reply to Embed".into()),
            };
        }
        tokio::task::spawn_blocking(move || embed::embed_all(&cfg, &texts))
            .await
            .unwrap_or_else(|_| Err("task join failed".into()))
    }
}

/// Why a `ReadFile` was requested, so its `FileContent` reply is applied right.
pub enum ReadKind {
    /// Opening the file: jump to `target` (1-based) in `pane`.
    Open { pane: usize, target: Option<usize> },
    /// Live refresh after an on-disk change: reload every pane showing the file
    /// in place, preserving scroll / caret / folds.
    Refresh,
}

/// One rendered entry on a doc page: a symbol with its signature and its doc
/// comment parsed to markdown items (which the markdown widget borrows).
pub struct DocEntryView {
    pub name: String,
    pub kind: String,
    pub signature: String,
    pub line: usize,
    /// Nesting depth for indenting members under their type (0 = the top item).
    pub depth: usize,
    pub doc_items: Vec<iced::widget::markdown::Item>,
}

/// The doc page shown in the main pane: the selected item followed by its
/// public members (like a rustdoc type page), all in one file.
pub struct DocPage {
    pub rel: String,
    pub entries: Vec<DocEntryView>,
}

/// The Connect modal's editable form + where it is in the flow. One modal walks
/// from picking a host, to waiting on the transport, to browsing the remote's
/// folders for the one to open.
pub struct ConnectUi {
    // New-connection form fields (strings so the text inputs bind directly;
    // `port` is parsed on submit).
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: String,
    pub identity: String,
    pub stage: ConnectStage,
}

impl Default for ConnectUi {
    fn default() -> Self {
        ConnectUi {
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: "22".to_string(),
            identity: String::new(),
            stage: ConnectStage::Picking,
        }
    }
}

/// Which field of the new-connection form an edit targets.
#[derive(Debug, Clone, Copy)]
pub enum ConnectField {
    Name,
    Host,
    User,
    Port,
    Identity,
}

/// Where the Connect modal is in its flow.
pub enum ConnectStage {
    /// Choosing a saved host or filling in a new one.
    Picking,
    /// The SSH transport is coming up (bootstrapping the remote server).
    Connecting { label: String },
    /// Connected: browse the remote filesystem to pick a folder to open.
    Browsing(RemoteBrowser),
    /// The connection failed; show why, with the form still available.
    Error(String),
}

/// The remote folder picker's state: the directory in view and its children.
pub struct RemoteBrowser {
    /// Absolute path of the directory being shown (as the server resolved it).
    pub cwd: String,
    /// Parent directory for the "up" control, `None` at the filesystem root.
    pub parent: Option<String>,
    pub entries: Vec<clew_protocol::DirEntry>,
    /// True while a `ListDir` is in flight, so the view can show it is loading.
    pub loading: bool,
}
