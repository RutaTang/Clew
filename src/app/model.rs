//! The shared app model: the domain types the GUI and its handlers pass
//! around, and the payloads carried by off-thread results. The RPC client the
//! AI and git flows go through (`AiClient`, `GitSource`) lives in `app::rpc`,
//! the async task bodies and LLM-input builders in `app::tasks`. Re-exported
//! from the crate root so `crate::<Type>` holds.

use crate::app::prelude::*;
use crate::*;

/// One modified source file of a watcher batch, parsed on the blocking pool
/// (see `WatchMsg::FilesIndexed`).
#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub path: PathBuf,
    /// The content hash of the bytes that were parsed — the compare-and-swap
    /// key against the registry when the result is applied.
    pub hash: incremental::Version,
    /// The file's symbol-index entries.
    pub symbols: Vec<SymbolEntry>,
    /// The file's raw (unresolved) imports.
    pub imports: Vec<imports::RawImport>,
    /// The file's call sites, read off the same parse (`None`: a language
    /// without a call model) — see `ProjectSession::calls_by_file`.
    pub calls: Option<Arc<projectcalls::FileCalls>>,
}

/// Where the language server for a language would come from, as the Language
/// Servers panel shows it. Computed off the UI thread (it probes the store on
/// disk) and cached until the panel's next refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocatedKind {
    /// Installed (or a project command), ready to start.
    Ready,
    NeedsDownload,
    NeedsInstall,
    Unsupported(String),
    /// No server is configured or known for the language.
    NoServer,
}

/// The result of staging a repository-named language-server command off the
/// UI thread (a `Clone` + `Debug` mirror of `clew_core::trust::StagedCommand`,
/// plus the configuration the staging was computed for).
#[derive(Debug, Clone)]
pub struct LspStagedCommand {
    /// The fingerprint the user approves (command bytes + args + options).
    pub fingerprint: String,
    /// The repository path — for the consent modal ONLY; never spawned.
    pub source: PathBuf,
    /// clew's private copy of the approved bytes, the only thing that may run.
    /// `None` = not approved yet.
    pub exec_path: Option<PathBuf>,
    /// The resolved server the staging hashed. The handler re-resolves
    /// `lsp.toml` and refuses a result computed for a configuration that has
    /// changed since.
    pub server: lsp::config::EffectiveServer,
}

/// A project's persisted state, read off the UI thread when the project opens
/// (see `ProjectMsg::StateLoaded`). The per-project user stores are
/// `None` for a REMOTE project, whose `.clew/` lives on the other host and
/// arrives over the protocol instead. A store that exists but cannot be read
/// or understood is an `Err` naming why — shown to the user, never mistaken
/// for an empty store (which the next save would have written over it).
pub struct LoadedProjectState {
    pub history: Option<Result<History, String>>,
    pub bookmarks: Option<Result<Vec<Bookmark>, String>>,
    pub notes: Option<Result<Vec<notes::Note>, String>>,
    /// `Err` = a malformed `lsp.toml`, surfaced rather than replaced by
    /// defaults (which could resolve a different server than configured).
    pub lsp_config: Option<Result<lsp::config::ProjectLspConfig, String>>,
    pub reading_target: Option<Result<Option<inactive::Target>, String>>,
    pub walk_library: Option<Result<Vec<walkthrough::Walkthrough>, String>>,
    /// The derived caches, from clew's own data dir (local AND remote).
    pub explain: explain::Cache,
    /// Why a stored explanation cache that EXISTS was not used (not a plain
    /// file, over the read cap, written by a newer clew) — shown with the
    /// other stores' problems. The file is left as it is: every later save
    /// refuses to write over it (`explain::edit`).
    pub explain_problem: Option<String>,
    pub overview: Option<overview::Cached>,
    pub stats: Option<stats::StatsReport>,
    pub embed: embed::Index,
    /// Why a stored semantic index that EXISTS was not used (not a plain
    /// file, over the read cap, unreadable) — shown with the other stores'
    /// problems. Without it, a refused index was rebuilt (and re-billed) at
    /// every open with nothing but a stderr line to say why.
    pub embed_problem: Option<String>,
    /// The read itself failed (it panicked), so nothing above was read — as
    /// opposed to stores that are simply absent. Reported, and the wholesale
    /// trail write stays off for the session (`InFlight::state_unread`): what
    /// is in memory is not the stored trail, and the file is still there.
    pub unread: Option<String>,
}

impl LoadedProjectState {
    /// The state of a read that failed as a whole (`why`): no store was read.
    pub(crate) fn unread(why: String) -> Self {
        LoadedProjectState {
            history: None,
            bookmarks: None,
            notes: None,
            lsp_config: None,
            reading_target: None,
            walk_library: None,
            explain: explain::Cache::new(),
            explain_problem: None,
            overview: None,
            stats: None,
            embed: embed::Index::default(),
            embed_problem: None,
            unread: Some(why),
        }
    }

    /// Read everything a project open needs from disk. Blocking: runs on the
    /// blocking pool (and directly in tests, to mirror that job exactly).
    pub(crate) fn read(root: &Path, store: Option<&Path>, local: bool) -> Self {
        fn checked<T>(r: Result<T, clew_core::statefile::StoreError>) -> Result<T, String> {
            r.map_err(|e| e.to_string())
        }
        let (embed, embed_problem) = store
            .map(|s| embed::load_checked(s, root))
            .unwrap_or_default();
        let (explain, explain_problem) = match store.map(|s| (s, explain::load_checked(s, root))) {
            Some((_, Ok(cache))) => (cache, None),
            Some((s, Err(e))) => (
                explain::Cache::new(),
                Some(format!("{} {e}", explain::cache_path(s).display())),
            ),
            None => (explain::Cache::new(), None),
        };
        LoadedProjectState {
            history: local.then(|| checked(history::load_checked(root))),
            bookmarks: local.then(|| checked(bookmarks::load_checked(root))),
            notes: local.then(|| checked(notes::load_checked(root))),
            lsp_config: local.then(|| lsp::config::ProjectLspConfig::load(root)),
            reading_target: local.then(|| checked(reading::load_target_checked(root))),
            walk_library: local.then(|| walkthrough::load_library_checked(root)),
            explain,
            explain_problem,
            overview: store.and_then(overview::load),
            stats: store.and_then(stats::load).map(|c| c.report),
            embed,
            embed_problem,
            unread: None,
        }
    }
}

/// Lookup tables for turning `path:line` citations in LLM prose into links: the
/// project's file rels, and each basename mapped to its rel (`None` when
/// several files share it, so an ambiguous name stays a code chip).
#[derive(Debug)]
pub struct CitationIndex {
    /// The file list these were built from. Identity (`Arc::ptr_eq`) is the
    /// freshness test: a rescan or project switch installs a new list.
    pub files: Arc<Vec<FileEntry>>,
    pub rels: HashSet<String>,
    pub by_name: HashMap<String, Option<String>>,
}

impl CitationIndex {
    pub(crate) fn build(files: Arc<Vec<FileEntry>>) -> Self {
        let rels = files.iter().map(|f| f.rel.clone()).collect();
        let mut by_name: HashMap<String, Option<String>> = HashMap::new();
        for f in files.iter() {
            let name = f.rel.rsplit('/').next().unwrap_or(&f.rel).to_string();
            by_name
                .entry(name)
                .and_modify(|e| *e = None)
                .or_insert_with(|| Some(f.rel.clone()));
        }
        Self {
            files,
            rels,
            by_name,
        }
    }

    /// The project rel a cited `path` names: an exact rel, or a basename that
    /// is unique in the project.
    pub(crate) fn resolve(&self, path: &str) -> Option<String> {
        if self.rels.contains(path) {
            return Some(path.to_string());
        }
        self.by_name.get(path).cloned().flatten()
    }
}

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

/// How often the project's files changed over the recent history — the
/// change-frequency overlay's data, built from a `GitOp::Churn` answer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Churn {
    /// Per file (absolute path): its commits among those scanned, and the
    /// time of its latest.
    pub by_file: HashMap<PathBuf, (u32, i64)>,
    /// The files, most changed first (git's answer, already capped).
    pub top: Vec<clew_protocol::FileChurn>,
    /// The largest commit count, the heat scale's top; 0 when nothing changed.
    pub max: u32,
    /// How many commits back the scan looked.
    pub commits: usize,
}

impl Churn {
    /// From git's answer, with the files keyed by their absolute paths under
    /// `root`, as the graphs name them.
    pub fn from_files(root: &Path, files: Vec<clew_protocol::FileChurn>, commits: usize) -> Churn {
        let by_file = files
            .iter()
            .map(|f| (root.join(&f.rel), (f.commits, f.last)))
            .collect();
        let max = files.iter().map(|f| f.commits).max().unwrap_or(0);
        Churn {
            by_file,
            top: files,
            max,
            commits,
        }
    }

    /// The file's commits among those scanned; 0 for a file none touched.
    pub fn commits_of(&self, file: &Path) -> u32 {
        self.by_file.get(file).map_or(0, |c| c.0)
    }

    /// Where the file sits on the heat scale, 0 (unchanged) to 1 (the most
    /// changed file), on a log scale so one hot file does not flatten the
    /// rest.
    pub fn heat_of(&self, file: &Path) -> f32 {
        let commits = self.commits_of(file);
        if commits == 0 || self.max == 0 {
            return 0.0;
        }
        let t = ((1.0 + commits as f32).ln() / (1.0 + self.max as f32).ln()).clamp(0.0, 1.0);
        if t.is_finite() { t } else { 0.0 }
    }
}

/// A full-screen modal showing a project-wide graph overview.
// The shared prefix is the point: each names the project-wide graph it shows.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The project's types and how they relate (the type map).
    ProjectTypes,
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
/// per frame), math/mermaid carry the cache key of their rendered [`ExplainSvg`]
/// AND their source, which is what shows whenever the SVG is not available
/// (still rendering, or the renderer could not draw it) — never a placeholder
/// that can outlive the render it stands for.
#[derive(Debug)]
pub enum PreparedSeg {
    Markdown(Vec<iced::widget::markdown::Item>),
    /// A display equation: its render key and its TeX.
    DisplayMath(u64, String),
    InlineLine(Vec<PreparedInline>),
    /// A mermaid diagram: its render key and its source.
    Mermaid(u64, String),
    /// A fenced code block, highlighted with clew's own tree-sitter pipeline
    /// (per-line styled spans, same palette as the editor).
    Code(Vec<crate::highlight::HlLine>),
}

/// An inline piece of a text line that mixes prose and inline math.
#[derive(Debug)]
pub enum PreparedInline {
    /// Prose around the math, its markdown parsed ONCE here: parsing it in
    /// the view re-ran the parse for every piece of every inline-math line
    /// on every rebuild (a mouse move is one).
    Text(richmd::InlinePiece),
    /// Inline math: its render key and its TeX (shown until, or instead of,
    /// the rendered SVG).
    Math(u64, String),
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
#[derive(Debug)]
pub struct AskTurn {
    /// The stream this turn's tokens arrive on. Deltas and steps route to the
    /// turn with THIS id, never to "the last streaming turn": two turns can be
    /// open at once (an agent turn and its retrieval fallback), and a stream
    /// started in a project we have left must never feed a turn in the new
    /// one. Minted from `next_req_id`, which is never reset, so the id is
    /// unique for the App's whole lifetime.
    pub stream: u64,
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

/// One write of a reading trail: the project's root and the trail as
/// `history::to_text` encoded it (`None` removes the file).
pub type TrailWrite = (PathBuf, Option<String>);

/// The reading trail's writer (see `App::save_history`): one write at a time,
/// on the blocking pool, and — while one runs — only the NEWEST trail of each
/// project waits behind it, since writing an older one would only be
/// overwritten. Window-level, not per project: the trail of a project just
/// left still reaches its disk.
#[derive(Debug, Default)]
pub struct TrailWriter {
    busy: bool,
    queued: std::collections::VecDeque<TrailWrite>,
}

impl TrailWriter {
    /// Submit `write`: the write to start now, or `None` when one is running
    /// (it then waits, replacing any older one of the same project).
    pub fn submit(&mut self, write: TrailWrite) -> Option<TrailWrite> {
        if !self.busy {
            self.busy = true;
            return Some(write);
        }
        match self.queued.iter_mut().find(|(root, _)| *root == write.0) {
            Some(waiting) => *waiting = write,
            None => self.queued.push_back(write),
        }
        None
    }

    /// The running write finished: the next one to start, if any waits.
    pub fn finished(&mut self) -> Option<TrailWrite> {
        let next = self.queued.pop_front();
        self.busy = next.is_some();
        next
    }

    /// Whether a write is running (tests).
    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.busy
    }
}

/// A correlated request to send again should the server refuse it as not
/// ready yet (see `ProjectLink::not_ready_retry`).
#[derive(Debug, Clone)]
pub struct NotReadyRetry {
    pub request: clew_protocol::Request,
    /// How many times it was already re-sent.
    pub attempt: u32,
}

/// One piece of a streaming chat answer, routed from the server's `ChatDelta` /
/// `ChatStreamDone` notifications (or a local stream) to the Ask flow.
pub enum ChatStreamPiece {
    Delta(String),
    /// The stream ended, and how: finished, stopped on request, or failed —
    /// typed end to end, so a Stop can never be read as a failure because of
    /// how some layer spelled it.
    Done(clew_protocol::StreamOutcome),
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
    /// The turn ended, and how (see [`ChatStreamPiece::Done`]).
    Done(clew_protocol::StreamOutcome),
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

impl AskPin {
    /// This pin's identity for the messages its chip sends: a hash of what it
    /// pins (file, line, code). Pins are de-duplicated on exactly those three
    /// at insertion (`on_ask_about_selection`), so equal keys are the same
    /// pin — unlike an index into the list, which the removal of an earlier
    /// chip shifts.
    pub fn key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        self.file.hash(&mut h);
        self.line.hash(&mut h);
        self.code.hash(&mut h);
        h.finish()
    }
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
    /// What the adapter said about this line, or `None` when it has not
    /// answered: no session, the request still in flight, or an adapter that
    /// returned fewer entries than we sent lines. `None` and `Some(false)` must
    /// not be drawn alike — one means "not known yet", the other means "this
    /// breakpoint will never fire", and showing a solid dot for the second is
    /// the gutter telling the user their breakpoint is live when it is not.
    pub verified: Option<bool>,
    /// Where the adapter actually bound it, when that is not the map key.
    /// Adapters slide a breakpoint forward to the next line that has code.
    pub bound_line: Option<usize>,
    /// The adapter's handle for this breakpoint. Kept so a later `breakpoint`
    /// event — the channel adapters use to bind lazily, long after
    /// `setBreakpoints` answered — can be matched back to this line.
    pub adapter_id: Option<i64>,
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
/// One stop of a debug run as the trace keeps it: why the program stopped
/// and the stack it stopped with, innermost frame first (see
/// `DebugState::trace`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceStop {
    pub reason: String,
    pub frames: Vec<TraceFrame>,
}

/// A frame of a [`TraceStop`]: the function, the file it is in (as the
/// adapter names it; `None` for a frame without source) and the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFrame {
    pub name: String,
    pub path: Option<PathBuf>,
    pub line: usize,
}

/// Stops a trace keeps at most: a run that stops more is recorded up to
/// here and marked as cut.
pub const MAX_TRACE_STOPS: usize = 500;

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
    /// The address a TCP adapter (js-debug, dlv) announced — where child
    /// sessions are opened. The whole address, not a port: the adapter may
    /// listen on `[::1]` as well as on `127.0.0.1`.
    pub addr: Option<std::net::SocketAddr>,
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
#[derive(Clone, Debug)]
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
#[derive(Debug)]
pub struct BlameWhy {
    /// Identifies this request among all of them. A reply is applied only
    /// while the popup is still waiting for THIS one: asking about A, closing
    /// the popup, then asking about B used to let A's late answer overwrite
    /// B's — "the popup is open" is not the same question as "the popup is
    /// waiting for you".
    pub token: u64,
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
#[derive(Debug)]
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
    /// The historical view's horizontal offset, carried across scrubs like
    /// `scroll_y` (a wide file read to the right stays read to the right).
    pub scroll_x: f32,
    /// Caret position, carried in from the live file and kept across revisions
    /// (and clicks) so the reader's place doesn't vanish on entry.
    pub caret: Option<(usize, usize)>,
    /// The line to bring into view (symbol scope: the function's line).
    pub focus_line: Option<usize>,
    pub loading: bool,
    /// Bumped on every enter/step so stale async results are dropped.
    pub generation: u64,
    /// This session's identity: the generation it was entered at, fixed for
    /// its whole life — a re-scope included, which re-reads its history in
    /// place and keeps its "what & why" summaries: each is of one commit's
    /// change to the file, whatever the scope. Their replies are keyed on it
    /// rather than on `generation`, which moves on every scrub — keying them
    /// on that dropped every reply the user scrubbed past and left its
    /// spinner up for good.
    pub session: u64,
    /// The generation its scope was set at: entered, or re-scoped. The story
    /// replies are keyed on it, not on `session`: a story tells the commits
    /// of the scope it was asked in, so one asked before a re-scope is
    /// dropped as it lands, not shown under the new scope's name.
    pub scoped: u64,
    /// LLM "what & why" summary per commit sha (cached across steps).
    pub why: HashMap<String, String>,
    /// Whether a "what & why" for the commit ON SCREEN is being generated
    /// (derived from `why_pending`; the view reads this).
    pub why_loading: bool,
    /// Commits whose "what & why" is being generated. Several can be in flight
    /// when the reader scrubs while one is running.
    pub why_pending: HashSet<String>,
    /// The "story of this function" narrative (symbol scope), prepared markdown.
    pub story: Option<Vec<PreparedSeg>>,
    pub story_loading: bool,
}

impl TimeTravel {
    /// Re-derive `why_loading` for the commit now on screen.
    pub(crate) fn sync_why_loading(&mut self) {
        self.why_loading = self
            .commits
            .get(self.idx)
            .is_some_and(|c| self.why_pending.contains(&c.sha));
    }
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
#[derive(Clone, Copy, Debug)]
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

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Arc<Vec<FileEntry>>,
    pub truncated: bool,
}

#[derive(Default, Debug)]
pub struct SearchState {
    pub query: String,
    pub running: bool,
    pub ran: bool,
    pub hits: Vec<SearchHit>,
    /// Last search's error (bad regex/glob), shown under the input.
    pub error: Option<String>,
    /// Files the last search could not read (too large, not a plain file, an
    /// I/O error) — reported, so a search that covered less than the project
    /// says so. A remote search lists a bounded few.
    pub skipped: Vec<search::SkippedFile>,
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

/// The active pane's diff-vs-HEAD, shown in place of the code when set —
/// prepared for the view ONCE, when the diff arrives ([`DiffState::new`]):
/// the view used to re-join every run and re-measure up to 8000 lines on
/// every repaint.
#[derive(Debug)]
pub struct DiffState {
    pub abs: PathBuf,
    pub rel: String,
    pub lines: Vec<git::DiffLine>,
    /// The drawn lines, as runs of one kind each (one text block per run),
    /// tabs expanded — at most `ui::MAX_DIFF_ROWS` lines.
    pub runs: Vec<DiffRun>,
    /// Display columns of the widest drawn line, which sizes every run so
    /// the tints span the content and long lines scroll into reach.
    pub max_cols: usize,
}

/// One run of same-kind diff lines as the diff view draws it.
#[derive(Debug, Clone)]
pub struct DiffRun {
    pub kind: git::DiffKind,
    /// The run's lines as drawn (`ui::diff_display_text`), joined.
    pub text: String,
}

impl DiffState {
    pub fn new(abs: PathBuf, rel: String, lines: Vec<git::DiffLine>) -> Self {
        let shown = &lines[..lines.len().min(crate::ui::MAX_DIFF_ROWS)];
        let max_cols = shown
            .iter()
            .map(|l| crate::ui::display_cols(&l.text))
            .max()
            .unwrap_or(0);
        let runs = shown
            .chunk_by(|a, b| a.kind == b.kind)
            .map(|run| DiffRun {
                kind: run[0].kind,
                text: run
                    .iter()
                    .map(|l| crate::ui::diff_display_text(&l.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            })
            .collect();
        DiffState {
            abs,
            rel,
            lines,
            runs,
            max_cols,
        }
    }
}

/// State of the language server for one language.
#[derive(Debug)]
pub enum LspSlot {
    Starting,
    Ready(lsp::client::LspClient),
    Failed(String),
    Unsupported(String),
    /// Awaiting the user's consent to download the server (see LspConsent).
    AwaitingConsent,
}

/// One slot per language — with the invariant that a Ready client leaving
/// the map is STOPPED: replaced, removed, cleared, or dropped with the link
/// that holds it (a disconnect, leaving the project). Stopping sends the
/// server `shutdown`/`exit` and kills a local child with its process group.
/// Dropping the slot's handle alone did not end the session while anything
/// still held a clone — a hover, a goto, a call-hierarchy request in flight —
/// so a restarted or abandoned server (and, for rust-analyzer, the cargo
/// processes it runs) could outlive the window's interest in it.
///
/// Reads go through [`LspSlots::get`] and friends; every mutation goes
/// through the methods here, so no path can drop a client unstopped.
#[derive(Debug, Default)]
pub struct LspSlots(HashMap<String, LspSlot>);

impl LspSlots {
    pub fn get(&self, language: &str) -> Option<&LspSlot> {
        self.0.get(language)
    }

    pub fn contains_key(&self, language: &str) -> bool {
        self.0.contains_key(language)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &LspSlot)> {
        self.0.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    /// Set `language`'s slot, stopping the client it replaces (if Ready).
    pub fn insert(&mut self, language: String, slot: LspSlot) {
        if let Some(old) = self.0.insert(language, slot) {
            old.stop();
        }
    }

    /// Empty `language`'s slot, stopping its client (if Ready).
    pub fn remove(&mut self, language: &str) {
        if let Some(old) = self.0.remove(language) {
            old.stop();
        }
    }

    /// Empty every slot, stopping every Ready client.
    pub fn clear(&mut self) {
        for (_, slot) in self.0.drain() {
            slot.stop();
        }
    }
}

impl Drop for LspSlots {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A flag raised when this guard is dropped — the cancellation seam between
/// whoever owns a piece of blocking work and the work itself: the work polls
/// [`RaiseOnDrop::flag`], and dropping the guard (its task aborted, the
/// project or the transport it belongs to left) raises it. Raised on a
/// normal finish too, by which time nothing polls it any more.
#[derive(Debug, Default)]
pub struct RaiseOnDrop(Arc<std::sync::atomic::AtomicBool>);

impl RaiseOnDrop {
    /// The flag the work polls.
    pub fn flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.0.clone()
    }
}

impl Drop for RaiseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The newest lines of one language server's log, as the Language Servers
/// panel shows them — copied from the server only when its log moved (see
/// `App::sync_lsp_log`).
#[derive(Debug, Clone)]
pub struct LspLogTail {
    pub language: String,
    /// The log's version the lines were copied at (`Snapshot::log_version`).
    pub version: u64,
    /// `Err` when the server's state is unreadable (see `StatePoisoned`).
    pub lines: Result<Vec<String>, lsp::client::StatePoisoned>,
}

/// One ready server's state as the current update read it (see
/// `App::sync_lsp_snapshots`): `Err` when its state lock is poisoned.
pub type LspSnapshot = Result<lsp::client::Snapshot, lsp::client::StatePoisoned>;

impl LspSlot {
    /// End a Ready client's session (see [`LspSlots`]); nothing for any other
    /// state.
    fn stop(&self) {
        if let LspSlot::Ready(client) = self {
            client.stop();
        }
    }

    /// The status text for this slot. A ready server is described from its
    /// `snapshot` — the reading `App::sync_lsp_snapshots` took for this
    /// update — rather than by locking its state again for every widget.
    pub fn label(&self, snapshot: Option<&LspSnapshot>) -> String {
        match self {
            LspSlot::Starting => "starting…".into(),
            // A ready server that surfaced an error (e.g. rust-analyzer couldn't
            // load the workspace) shows it — otherwise it reads "ready" while
            // every go-to-def silently returns nothing. Else: live progress
            // (indexing) when active, else "ready". A state that cannot be
            // read at all (a poisoned lock) says so: it is not a quiet server.
            LspSlot::Ready(_) => match snapshot {
                Some(Ok(s)) => match &s.error {
                    Some(e) => format!("⚠ {}", lsp_error_summary(e)),
                    None => s.progress.clone().unwrap_or_else(|| "ready".into()),
                },
                Some(Err(_)) => "⚠ state unreadable after an internal error — restart it".into(),
                None => "ready".into(),
            },
            LspSlot::Failed(e) => format!("error: {e}"),
            LspSlot::Unsupported(e) => e.clone(),
            LspSlot::AwaitingConsent => "download needed".into(),
        }
    }
}

/// How a pending, consent-gated provisioning will obtain the server.
#[derive(Clone, Debug)]
pub enum LspProvision {
    Download(lsp::registry::Download),
    Install(lsp::registry::Install),
    /// Installed by the connected clew-server on ITS host (a remote). The
    /// consent is carried by the `LspInstall` request this turns into; the
    /// description came with the server's `LspResolved` reply, and `consent`
    /// is that reply's digest of the install, handed back verbatim so the
    /// server runs only the install described here.
    Remote {
        describe: String,
        consent: String,
    },
    /// A debug adapter the Debug button needs (debugpy, vscode-js-debug),
    /// installed on this machine once approved; `stamp` names the project
    /// instance whose debug session then starts.
    DebugAdapter {
        install: dap::AdapterInstall,
        stamp: Stamp,
    },
}

/// The provisionings awaiting the user's consent, asked one at a time in
/// arrival order (the modal shows the front). Two needs that arrive together
/// — a language server and the debug adapter, two languages at once — used to
/// share one slot, and the later silently replaced the earlier: its language
/// sat "awaiting consent" with no question left on screen to answer.
#[derive(Debug, Default)]
pub struct ConsentQueue(std::collections::VecDeque<LspConsent>);

impl ConsentQueue {
    /// Queue `consent`. A request for what is already waiting — the same
    /// language, the same kind of provisioning, the same server — replaces
    /// that entry in place rather than asking twice: a re-resolve carries the
    /// newer description (and, for a remote install, the newer digest the
    /// server will check), a new debug run the newer session.
    pub fn offer(&mut self, consent: LspConsent) {
        match self.0.iter_mut().find(|c| c.same_request(&consent)) {
            Some(waiting) => *waiting = consent,
            None => self.0.push_back(consent),
        }
    }

    /// The question on screen.
    pub fn front(&self) -> Option<&LspConsent> {
        self.0.front()
    }

    /// Answer the question on screen: it leaves the queue.
    pub fn pop_front(&mut self) -> Option<LspConsent> {
        self.0.pop_front()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &LspConsent> {
        self.0.iter()
    }
}

/// A pending language-server provisioning the user must approve.
#[derive(Clone, Debug)]
pub struct LspConsent {
    pub language: String,
    pub server_name: String,
    pub version: String,
    pub provision: LspProvision,
    pub dest_dir: PathBuf,
}

impl LspConsent {
    /// Whether `other` asks for the same thing: the same language, the same
    /// kind of provisioning and the same server (see [`ConsentQueue::offer`]).
    fn same_request(&self, other: &LspConsent) -> bool {
        self.language == other.language
            && self.server_name == other.server_name
            && std::mem::discriminant(&self.provision) == std::mem::discriminant(&other.provision)
    }

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
            LspProvision::Remote { describe, .. } => format!("{describe} — on the remote host"),
            LspProvision::DebugAdapter { install, .. } => install.describe(),
        }
    }
}

/// What the project's own `lsp.toml` asks clew to do for a language, awaiting
/// the user's approval: run a command, send `init_options` to the server, or
/// both. The project file is attacker-controlled when the repository is
/// untrusted, so whatever it names must be shown in full and confirmed before
/// it takes effect — approval is recorded against a fingerprint, so an edited
/// `lsp.toml` has to be confirmed again.
#[derive(Clone, Debug)]
pub struct PendingLspCommand {
    /// The project the command belongs to, captured when the modal was
    /// raised. Approval must record against THIS root — the current project
    /// may have changed while the modal sat open, and approving must never
    /// grant the old command to the new project.
    pub root: PathBuf,
    /// The host the command would run on (`None` = this machine), captured
    /// with `root` for the same reason: the approval key includes it.
    pub host: Option<String>,
    pub language: String,
    /// The program `lsp.toml` names, when it names one. `None` when the config
    /// sets only `init_options`: what runs is then clew's own store-installed
    /// server, already covered by the install consent, and the question put to
    /// the user is about the options alone.
    pub command: Option<PathBuf>,
    pub args: Vec<String>,
    pub server_name: String,
    pub version: String,
    pub fingerprint: String,
    /// The `init_options` this approval covers, pretty-printed for display.
    /// Always shown when present: they are inside the fingerprint either way,
    /// and several servers treat them as a place to name programs to run, so
    /// approving them unseen would be approving the payload blind.
    pub init_options: Option<String>,
}

impl PendingLspCommand {
    /// The exact command line that would run, for the confirmation dialog, or
    /// `None` when the config names no command.
    pub fn command_line(&self) -> Option<String> {
        let mut out = self.command.as_ref()?.to_string_lossy().into_owned();
        for a in &self.args {
            out.push(' ');
            out.push_str(a);
        }
        Some(out)
    }
}

/// Why a `ReadFile` was requested, so its `FileContent` reply is applied right.
#[derive(Debug)]
pub enum ReadKind {
    /// Opening the file: jump to `target` (1-based) in `pane`.
    Open { pane: usize, target: Option<usize> },
    /// Live refresh after an on-disk change: reload every pane showing the file
    /// in place, preserving scroll / caret / folds.
    ///
    /// Carries the `rel` so a newer refresh can retire the older ones for the
    /// same file. The server answers reads off its request loop, so two
    /// refreshes for one file complete in either order, and an untagged older
    /// reply rolled the pane — and the content hash the change detector
    /// compares against — back to the previous bytes.
    Refresh { rel: String },
    /// A refresh a newer one of the same file retired while in flight
    /// (`App::request_file_refresh`): its answer, content or refusal, is
    /// nobody's, and is dropped quietly as it lands. Kept rather than
    /// forgotten, so that a refusal is known for one: forgotten, it reached
    /// the status line as an error about nothing the reader asked for.
    Retired,
}

/// One rendered entry on a doc page: a symbol with its signature and its doc
/// comment parsed to markdown items (which the markdown widget borrows).
#[derive(Debug)]
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
#[derive(Debug)]
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
    /// Per-host opt-in: let this host's clew-server hold the AI API keys and
    /// run AI calls. Off by default; see `App::remote_ai_opt_in`.
    pub send_ai_keys: bool,
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
            send_ai_keys: false,
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
    /// `target` presented a host key clew does not know: show its fingerprint
    /// with Trust / Cancel. `reason` is the refusal, shown again on Cancel.
    TrustHost {
        target: connect::ConnTarget,
        reason: String,
        key: connect::ScannedHostKey,
    },
    /// `target` presented a CHANGED key, and the key on record is one trusted
    /// in clew (under `forget_host` in clew's own known_hosts): show the
    /// refusal with Forget / Cancel. Forgetting only clears the old key — the
    /// reconnect then shows the new one as unknown, to be checked and trusted.
    HostKeyChanged {
        target: connect::ConnTarget,
        reason: String,
        forget_host: String,
    },
}

/// The remote folder picker's state: the directory in view and its children.
pub struct RemoteBrowser {
    /// Absolute path of the directory being shown (as the server resolved it).
    pub cwd: String,
    /// Parent directory for the "up" control, `None` at the filesystem root.
    pub parent: Option<String>,
    pub entries: Vec<clew_protocol::DirEntry>,
    /// Entries of `cwd` left out of `entries` past the server's listing cap
    /// (0 = the listing is complete), so the view can say it is partial.
    pub omitted: usize,
    /// True while a `ListDir` is in flight, so the view can show it is loading.
    pub loading: bool,
}
