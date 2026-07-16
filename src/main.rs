//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).

mod analyze;
mod bookmarks;
mod cache;
mod callgraph;
mod codeview;
mod explain;
mod find;
mod finder;
mod fs_scan;
mod git;
mod graphlayout;
mod highlight;
mod imports;
mod incremental;
mod history;
mod index;
mod llm;
mod lsp;
mod outline;
mod projectcalls;
mod search;
mod watch;
mod theme;
mod ui;
mod viewer;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::operation;
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::{Element, Size, Subscription, Task, keyboard};

use crate::bookmarks::Bookmark;
use crate::finder::{Finder, FinderMode};
use crate::fs_scan::{DirNode, FileEntry, ScanResult};
use crate::highlight::HlLine;
use crate::history::{History, Loc};
use crate::index::SymbolEntry;
use crate::outline::Symbol;
use crate::search::SearchHit;
use crate::viewer::{MAX_FILE_BYTES, Viewer};

pub fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size(Size::new(1280.0, 860.0))
        .centered()
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Search,
    Marks,
    /// Call hierarchy for the symbol `gc` was invoked on.
    Calls,
    /// Import graph rooted at the active file.
    Imports,
}

/// A full-screen modal showing a project-wide graph overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// The whole-project call graph (tree-sitter, name-resolved).
    ProjectCalls,
    /// The whole-project import graph.
    ProjectImports,
}

/// A file's name for a compact graph-node label (`client.rs`).
fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Column of `byte` within `line` in the server's position encoding.
fn encode_col(line: &str, byte: usize, utf16: bool) -> usize {
    let byte = byte.min(line.len());
    let prefix = &line[..byte];
    if utf16 {
        prefix.encode_utf16().count()
    } else {
        prefix.len()
    }
}

/// Background LSP call-hierarchy pass, driving a stream of progress messages and
/// a final precise call graph.
///
/// For each function in `query_defs` it prepares a call hierarchy at the name's
/// position and reads its incoming calls (and, for an incremental run, outgoing
/// too), producing symbol-keyed `(caller, callee)` edges kept to the project.
/// The result starts from `base` — for an incremental run, edges touching a
/// `changed` file are dropped first, then re-derived — so only the changed
/// functions are re-queried. Bounded-concurrent, per-request timeout so a wedged
/// server can't hang the pass.
#[allow(clippy::too_many_arguments)]
async fn refine_stream(
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    all_defs: Vec<projectcalls::Def>,
    query_defs: Vec<projectcalls::Def>,
    base: projectcalls::SymEdges,
    changed: Option<HashSet<PathBuf>>,
    clients: HashMap<String, lsp::client::LspClient>,
    root: PathBuf,
    generation: u64,
) {
    use iced::futures::{SinkExt, StreamExt};
    use std::time::Duration;

    // File lines, read once, for locating each function name's column.
    let mut file_lines: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for d in &query_defs {
        file_lines.entry(d.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(&d.file)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
    }

    // One query per function; `key` is its symbol identity for edge endpoints.
    let both_dirs = changed.is_some();
    struct Query {
        key: projectcalls::SymKey,
        client: lsp::client::LspClient,
        file: PathBuf,
        line0: usize,
        character: usize,
    }
    let mut queries = Vec::new();
    for d in &query_defs {
        let Some(lang) = highlight::detect(&d.file) else {
            continue;
        };
        let Some(client) = clients.get(lang) else {
            continue;
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let line0 = d.line.saturating_sub(1);
        let character = file_lines
            .get(&d.file)
            .and_then(|lines| lines.get(line0))
            .and_then(|text| text.find(&d.name).map(|b| encode_col(text, b, utf16)))
            .unwrap_or(0);
        queries.push(Query {
            key: (d.file.clone(), d.name.clone()),
            client: client.clone(),
            file: d.file.clone(),
            line0,
            character,
        });
    }

    let total = queries.len();
    let mut stream = iced::futures::stream::iter(queries.into_iter().map(|q| async move {
        // A wedged server must not hang the whole pass.
        let work = async {
            let items = q
                .client
                .prepare_call_hierarchy(&q.file, q.line0, q.character)
                .await;
            let mut incoming = Vec::new();
            let mut outgoing = Vec::new();
            for it in items {
                incoming.extend(q.client.incoming_calls(it.raw.clone()).await);
                if both_dirs {
                    outgoing.extend(q.client.outgoing_calls(it.raw).await);
                }
            }
            (incoming, outgoing)
        };
        let (incoming, outgoing) = tokio::time::timeout(Duration::from_secs(15), work)
            .await
            .unwrap_or_default();
        (q.key, incoming, outgoing)
    }))
    .buffer_unordered(12);

    // Start from the base edge set, dropping edges that touch a changed file
    // (they'll be re-derived from this pass's queries).
    let mut edges: projectcalls::SymEdges = base;
    if let Some(changed) = &changed {
        edges.retain(|((cf, _), (ef, _))| !changed.contains(cf) && !changed.contains(ef));
    }
    let in_project = |p: &Path| p.starts_with(&root);

    let mut done = 0usize;
    while let Some((key, incoming, outgoing)) = stream.next().await {
        for caller in incoming {
            if in_project(&caller.path) {
                edges.insert(((caller.path, caller.name), key.clone()));
            }
        }
        for callee in outgoing {
            if in_project(&callee.path) {
                edges.insert((key.clone(), (callee.path, callee.name)));
            }
        }
        done += 1;
        if done.is_multiple_of(16) || done == total {
            let _ = output
                .send(Message::RefineProgress { generation, done, total })
                .await;
        }
    }

    let graph = projectcalls::ProjectCallGraph::graph_from_sym_edges(all_defs, &edges);
    let _ = output
        .send(Message::ProjectCallsRefined { root, generation, edges, graph })
        .await;
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

    fn method(self) -> &'static str {
        match self {
            GotoKind::Definition => "textDocument/definition",
            GotoKind::References => "textDocument/references",
            GotoKind::Implementation => "textDocument/implementation",
            GotoKind::TypeDefinition => "textDocument/typeDefinition",
        }
    }
    fn verb(self) -> &'static str {
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
            // A ready server shows live progress (e.g. indexing) when active.
            LspSlot::Ready(client) => client.progress().unwrap_or_else(|| "ready".into()),
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

pub struct App {
    pub project: Option<Project>,
    /// File to open automatically once the initial scan completes
    /// (set when the CLI argument is a file path).
    pub pending_open: Option<PathBuf>,
    /// Project root awaiting the user's consent to create `.clew`.
    /// While `Some`, the consent modal is shown.
    pub pending_consent: Option<PathBuf>,
    pub scanning: bool,
    pub sidebar: SidebarTab,
    /// The call hierarchy shown in the Calls sidebar tab, if any.
    pub call_graph: Option<callgraph::CallTree>,
    /// Whole-project file→file import graph, derived from tree-sitter and kept
    /// incrementally fresh; the Imports sidebar tab is a view onto it.
    pub import_graph: imports::ImportGraph,
    /// The import tree currently shown, rooted at the active file.
    pub import_tree: Option<imports::ImportTree>,
    /// Persisted Imports/Importers direction preference across focus changes.
    pub import_dir: imports::Dir,
    /// Import cycles in the project, recomputed when the graph changes (cached so
    /// the sidebar banner doesn't re-run cycle detection every frame).
    pub import_cycles: Vec<Vec<PathBuf>>,
    /// Whole-project symbol call graph (tree-sitter, name-resolved), built lazily
    /// when its overlay opens; drives the project call-graph overlay.
    pub project_calls: projectcalls::ProjectCallGraph,
    /// Registry revision the project call graph was last built at (to rebuild it
    /// only when files actually changed since).
    pub project_calls_rev: u64,
    /// True while the project call graph is being (re)built off-thread.
    pub building_calls: bool,
    /// True when `project_calls` is the exact LSP-resolved graph rather than the
    /// tree-sitter name-based approximation.
    pub project_calls_precise: bool,
    /// Generation counter for LSP-refine runs, so a late result from a superseded
    /// run (new project, re-refine, or a rebuild) is dropped.
    pub calls_gen: u64,
    /// LSP-refine progress `(done, total)` while a refine is running.
    pub refine_progress: Option<(usize, usize)>,
    /// The precise edge set, symbol-keyed, kept while `project_calls_precise` so
    /// a file change can patch only the affected functions.
    pub precise_edges: projectcalls::SymEdges,
    /// Source files changed since the last precise update, awaiting an
    /// incremental refine (coalesced when one is already running).
    pub precise_pending: HashSet<PathBuf>,
    /// The active project-graph modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// Overlay view: `true` shows the node-link map, `false` the list.
    pub graph_mode: bool,
    /// Precomputed force-directed layout for the current overlay's map.
    pub graph_layout: Option<graphlayout::Layout>,
    pub expanded: HashSet<String>,
    /// Code panes; pane 1 exists only in split view.
    pub panes: [Option<Viewer>; 2],
    pub split: bool,
    pub active: usize,
    pub show_outline: bool,
    /// When set, the active pane shows this file's diff against `HEAD`.
    pub diff: Option<DiffState>,
    pub finder: Finder,
    pub search: SearchState,
    pub history: History,
    pub bookmarks: Vec<Bookmark>,
    pub symbol_index: Arc<Vec<SymbolEntry>>,
    pub indexing: bool,
    /// Per-project LSP config from `.clew/lsp.toml`.
    pub lsp_config: lsp::config::ProjectLspConfig,
    /// One server slot per language.
    pub lsp: std::collections::HashMap<String, LspSlot>,
    /// Documents already sent to a server via didOpen (cleared per project).
    pub lsp_opened: HashSet<PathBuf>,
    /// Whole-project content-hash oracle: the authority on what changed, so the
    /// watcher's noisy events collapse to real byte changes.
    pub registry: incremental::Registry,
    /// Symbol index kept per file so a single file can be re-indexed in place;
    /// `symbol_index` is the flattened view the finder consumes.
    pub symbol_index_by_file: HashMap<PathBuf, Vec<SymbolEntry>>,
    /// Monotonic LSP document version, bumped on every `didChange`.
    pub lsp_doc_rev: i64,
    /// Last diagnostics version seen per language, to gate refresh ticks.
    pub seen_diag_version: std::collections::HashMap<String, u64>,
    /// A language server download awaiting the user's consent.
    pub pending_lsp_consent: Option<LspConsent>,
    /// In-file find (Cmd+F), applied to the active pane.
    pub find: find::FindState,
    /// Active hover tooltip (Cmd-hover): position + content.
    pub hover: Option<HoverState>,
    /// An open right-click navigation menu: (pane, line, col, window x, y).
    pub context_menu: Option<ContextMenu>,
    /// Whether the "Language Servers" management panel is open.
    pub server_panel: bool,
    /// Installed servers listed in the management panel (name, version, bytes).
    pub installed_servers: Vec<lsp::store::InstalledServer>,
    /// Languages actually present in the project that clew can serve.
    pub project_languages: Vec<String>,
    /// True while a mouse drag-selection is in progress.
    pub selecting: bool,
    /// True when the code view (not a text input/finder) has keyboard focus,
    /// so Vim-style motion keys move the cursor instead of typing.
    pub code_focused: bool,
    /// Pending `g` prefix for two-key motions (gg / gd / gr / gi / gy).
    pub pending_g: bool,
    /// Pending `z` prefix for fold commands (za / zR / zM).
    pub pending_z: bool,
    pub modifiers: keyboard::Modifiers,
    pub status: String,
    /// Logical window width (from resize events), drives responsive layout.
    pub window_width: f32,
    pub font_size: f32,
}

pub const DEFAULT_FONT_SIZE: f32 = 13.0;

#[derive(Debug, Clone)]
pub enum Message {
    OpenFolderPressed,
    FolderPicked(Option<PathBuf>),
    ConsentAllowed,
    ConsentDenied,
    ScanDone(ScanResult),
    /// A structural change (file created/deleted/renamed) rebuilt the tree; swap
    /// it in without the full project-open reset.
    TreeUpdated(ScanResult),
    SymbolIndexDone {
        root: PathBuf,
        indexed: index::Indexed,
    },
    ToggleDir(String),
    OpenRel {
        rel: String,
        line: Option<usize>,
    },
    OpenAbs {
        abs: PathBuf,
        line: Option<usize>,
        push: bool,
    },
    FileLoaded {
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    },
    Highlighted {
        abs: PathBuf,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
    },
    /// The watcher reports paths that may have changed on disk (unfiltered).
    FilesChanged(Vec<PathBuf>),
    /// Off-thread re-hash classified these watched paths (modified / deleted).
    FilesRehashed(Vec<watch::FileEvent>),
    /// Per-line git blame + change status finished loading for `abs`.
    GitInfoLoaded {
        abs: PathBuf,
        info: Option<Arc<git::GitInfo>>,
    },
    CodeScrolled(usize, scrollable::Viewport),
    PaneFocused(usize),
    ToggleSplit,
    SelectStart {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectDrag {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectEnd,
    CopySelection,
    /// Toggle the fold headed by `line` in `pane` (gutter arrow click).
    FoldToggle {
        pane: usize,
        line: usize,
    },
    /// Scroll `pane` to a fraction `[0,1]` of the file (minimap click/drag).
    MinimapScrolled {
        pane: usize,
        fraction: f32,
    },
    SidebarTabPicked(SidebarTab),
    SearchQueryChanged(String),
    SearchSubmitted,
    SearchDone {
        result: search::SearchResult,
    },
    /// Toggle a match option (regex / case-sensitive / whole-word).
    SearchToggle(SearchOpt),
    SearchIncludeChanged(String),
    SearchExcludeChanged(String),
    FinderOpened(FinderMode),
    FinderClosed,
    FinderQueryChanged(String),
    FinderPick(usize),
    FinderConfirm,
    GotoLineRequested,
    BookmarkToggled,
    BookmarkRemoved(usize),
    GoBack,
    GoForward,
    ToggleOutline,
    /// Toggle the diff-vs-HEAD view for the active file.
    ToggleDiff,
    /// The diff for `abs` finished computing.
    DiffLoaded {
        abs: PathBuf,
        rel: String,
        lines: Vec<git::DiffLine>,
    },
    OutlineJump(usize),
    FontSizeDelta(f32),
    FontSizeReset,
    KeyPressed(keyboard::Key, keyboard::Modifiers),
    ModifiersChanged(keyboard::Modifiers),
    WindowResized(Size),
    // --- LSP ---
    LspStartResult {
        language: String,
        result: Result<lsp::client::LspClient, String>,
    },
    LspConsentAllowed,
    LspConsentDismissed,
    LspDownloadResult {
        language: String,
        result: Result<PathBuf, String>,
    },
    GotoDefinition {
        pane: usize,
        line: usize,
        col: usize,
    },
    ContextMenuOpened {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    ContextMenuClosed,
    ContextGoto(GotoKind),
    FindOpened,
    FindQueryChanged(String),
    FindStep(i32),
    FindClosed,
    HoverRequested {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    HoverResult {
        line: usize,
        col: usize,
        text: Option<String>,
    },
    DefinitionResult {
        result: Result<Vec<lsp::client::Target>, String>,
    },
    ReferencesResult {
        result: Result<Vec<lsp::client::Target>, String>,
    },
    /// Open the call hierarchy for the symbol under the cursor (`gc`).
    CallHierarchyRequested,
    /// Open the call hierarchy from the right-click context menu.
    CallHierarchyFromMenu,
    /// `prepareCallHierarchy` resolved the anchor item(s).
    CallHierarchyPrepared {
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    },
    /// Expand (fetch children of, or toggle) a node in the call tree.
    CallHierarchyExpand(usize),
    /// A node's callers/callees arrived.
    CallHierarchyChildren {
        id: usize,
        items: Vec<lsp::client::CallItem>,
    },
    /// Flip between callers and callees.
    CallHierarchyDirection,
    /// Recursively expand the whole tree (to the project boundary).
    CallHierarchyExpandAll,
    /// Expand/collapse an import-tree node.
    ImportExpand(usize),
    /// Flip the import tree between Imports and Importers.
    ImportDirection,
    /// Recursively expand the whole import tree (to the project boundary).
    ImportExpandAll,
    /// Open a project-wide graph overlay (or switch which one).
    OpenOverlay(Overlay),
    /// Close the project-graph overlay.
    CloseOverlay,
    /// From an overlay: open a file, focus the Imports tab, and close the overlay.
    OverlayOpenImports(PathBuf),
    /// From an overlay: open a file at a line and close the overlay.
    OverlayOpenAt { abs: PathBuf, line: usize },
    /// The project call graph finished (re)building off-thread.
    ProjectCallsBuilt {
        root: PathBuf,
        graph: projectcalls::ProjectCallGraph,
    },
    /// Flip the current overlay between the list and the node-link map.
    OverlayViewToggle,
    /// Kick a background LSP pass that rebuilds the call graph with exact edges.
    RefineProjectCalls,
    /// Progress of the running LSP refine.
    RefineProgress {
        generation: u64,
        done: usize,
        total: usize,
    },
    /// The LSP-precise call graph finished (re)building. Carries the symbol-keyed
    /// edge set (kept for incremental patching) and the ready display graph.
    ProjectCallsRefined {
        root: PathBuf,
        generation: u64,
        edges: projectcalls::SymEdges,
        graph: projectcalls::ProjectCallGraph,
    },
    Tick,
    ToggleServerPanel,
    LspRestart(String),
    LspRemove {
        name: String,
        version: String,
    },
    LspDownloadFor(String),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let mut app = App::blank();
        let task = match std::env::args().nth(1) {
            Some(arg) => {
                let path = PathBuf::from(&arg);
                let path = path.canonicalize().unwrap_or(path);
                if path.is_dir() {
                    app.request_open(path)
                } else if path.is_file() {
                    // Open the parent directory as the project, then the file.
                    let root = path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| path.clone());
                    app.pending_open = Some(path);
                    app.request_open(root)
                } else {
                    app.status = format!("No such path: {arg}");
                    Task::none()
                }
            }
            None => Task::none(),
        };
        (app, task)
    }

    fn blank() -> Self {
        App {
            project: None,
            pending_open: None,
            pending_consent: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            call_graph: None,
            import_graph: imports::ImportGraph::default(),
            import_tree: None,
            import_dir: imports::Dir::Imports,
            import_cycles: Vec::new(),
            project_calls: projectcalls::ProjectCallGraph::default(),
            project_calls_rev: 0,
            building_calls: false,
            project_calls_precise: false,
            calls_gen: 0,
            refine_progress: None,
            precise_edges: projectcalls::SymEdges::default(),
            precise_pending: HashSet::new(),
            overlay: None,
            graph_mode: true,
            graph_layout: None,
            expanded: HashSet::new(),
            panes: [None, None],
            split: false,
            active: 0,
            show_outline: true,
            diff: None,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            bookmarks: Vec::new(),
            symbol_index: Arc::new(Vec::new()),
            indexing: false,
            lsp_config: lsp::config::ProjectLspConfig::default(),
            lsp: std::collections::HashMap::new(),
            lsp_opened: HashSet::new(),
            registry: incremental::Registry::default(),
            symbol_index_by_file: HashMap::new(),
            lsp_doc_rev: 1,
            seen_diag_version: std::collections::HashMap::new(),
            pending_lsp_consent: None,
            find: find::FindState::default(),
            hover: None,
            context_menu: None,
            server_panel: false,
            installed_servers: Vec::new(),
            project_languages: Vec::new(),
            selecting: false,
            code_focused: true,
            pending_g: false,
            pending_z: false,
            modifiers: keyboard::Modifiers::default(),
            status: "Open a folder to start reading".to_string(),
            window_width: 1280.0,
            font_size: DEFAULT_FONT_SIZE,
        }
    }

    pub fn line_height(&self) -> f32 {
        self.font_size + 7.0
    }

    pub fn active_viewer(&self) -> Option<&Viewer> {
        self.panes[self.active].as_ref()
    }

    fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.panes[self.active].as_mut()
    }

    fn title(&self) -> String {
        match &self.project {
            Some(p) => format!(
                "clew — {}",
                p.root.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "clew".to_string(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        ui::view(self)
    }

    fn theme(&self) -> iced::Theme {
        theme::app_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        // listen_with (rather than keyboard::listen) also sees events already
        // captured by focused widgets, so shortcuts like Esc work while a
        // text input has focus.
        let events = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some(Message::KeyPressed(key, modifiers))
            }
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::SelectEnd)
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size))
            }
            _ => None,
        });

        // Watch the project tree for on-disk changes (live refresh) — keyed on
        // the root so opening a different project restarts the watcher.
        let mut subs = vec![events];
        if let Some(project) = &self.project {
            subs.push(watch::watch(project.root.clone()));
        }
        // Poll for live refresh only while something is changing (a server is
        // starting, indexing, or the management panel is open) — idle stays quiet.
        if self.lsp_needs_refresh() {
            subs.push(iced::time::every(std::time::Duration::from_millis(400)).map(|_| Message::Tick));
        }
        Subscription::batch(subs)
    }

    fn lsp_needs_refresh(&self) -> bool {
        self.server_panel
            || self.lsp.iter().any(|(lang, s)| match s {
                LspSlot::Starting => true,
                LspSlot::Ready(c) => {
                    c.progress().is_some()
                        || self.seen_diag_version.get(lang).copied() != Some(c.diag_version())
                }
                _ => false,
            })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFolderPressed => Task::perform(pick_folder(), Message::FolderPicked),
            Message::FolderPicked(None) => Task::none(),
            Message::FolderPicked(Some(root)) => self.request_open(root),
            Message::ConsentDenied => {
                self.pending_consent = None;
                self.pending_open = None;
                self.status = "Project not opened: creating .clew was not allowed".to_string();
                Task::none()
            }
            Message::ConsentAllowed => {
                let Some(root) = self.pending_consent.take() else {
                    return Task::none();
                };
                // Consent is recorded by the .clew directory itself.
                match std::fs::create_dir_all(root.join(".clew")) {
                    Ok(()) => self.start_scan(root),
                    Err(e) => {
                        self.pending_open = None;
                        self.status = format!("Cannot open project: .clew is not writable ({e})");
                        Task::none()
                    }
                }
            }
            Message::ScanDone(result) => self.on_scan_done(result),
            Message::TreeUpdated(result) => {
                // Only apply to the current project (a stale rescan from a
                // previous root is ignored).
                let current = self.project.as_ref().map(|p| p.root.clone());
                if current.as_deref() == Some(result.root.as_path()) {
                    if let Some(p) = &mut self.project {
                        p.tree = result.tree;
                        p.files = Arc::new(result.files);
                        p.truncated = result.truncated;
                    }
                    self.refresh_finder();
                    // The file set changed, so imports that were unresolved (or
                    // resolved to a since-moved file) may now resolve differently.
                    self.reresolve_import_graph();
                    if let Some(p) = &self.project {
                        self.status =
                            format!("{} files · {} symbols", p.files.len(), self.symbol_index.len());
                    }
                }
                Task::none()
            }
            Message::SymbolIndexDone { root, indexed } => {
                // Ignore a late result from a project the user already switched
                // away from (it would seed the new project's registry with the
                // old project's files).
                if self.project.as_ref().map(|p| &p.root) != Some(&root) {
                    return Task::none();
                }
                self.indexing = false;
                // Seed the change-detection registry from the same tree read.
                self.registry.seed(indexed.hashes);
                self.symbol_index_by_file = indexed.by_file;
                let changed_while_closed = indexed.changed.len();
                self.rebuild_symbol_index();
                // Build the import graph from the same single tree read.
                self.rebuild_import_graph(indexed.imports_by_file);
                if changed_while_closed > 0 {
                    self.status = format!(
                        "{changed_while_closed} file{} changed since last session",
                        if changed_while_closed == 1 { "" } else { "s" }
                    );
                } else if let Some(p) = &self.project {
                    self.status = format!(
                        "{} files · {} symbols",
                        p.files.len(),
                        self.symbol_index.len()
                    );
                }
                Task::none()
            }
            Message::ToggleDir(rel) => {
                if !self.expanded.remove(&rel) {
                    self.expanded.insert(rel);
                }
                Task::none()
            }
            Message::OpenRel { rel, line } => {
                let Some(project) = &self.project else {
                    return Task::none();
                };
                let abs = project.root.join(&rel);
                self.open_file(abs, line, true)
            }
            Message::OpenAbs { abs, line, push } => self.open_file(abs, line, push),
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            } => self.on_file_loaded(pane, abs, target, result),
            Message::Highlighted {
                abs,
                lines,
                symbols,
            } => {
                let lines = Arc::new(lines);
                for slot in &mut self.panes {
                    if let Some(v) = slot
                        && v.abs == abs
                        && v.lines.len() == lines.len()
                    {
                        v.set_lines(lines.clone());
                        v.symbols = symbols.clone();
                        v.highlighted = true;
                    }
                }
                Task::none()
            }
            Message::GitInfoLoaded { abs, info } => {
                for slot in &mut self.panes {
                    if let Some(v) = slot
                        && v.abs == abs
                    {
                        v.git = info.clone();
                    }
                }
                Task::none()
            }
            Message::FilesChanged(paths) => {
                // Consider a path worth re-hashing if it's on screen, already
                // tracked, or a source file we index (this catches edits to
                // files that aren't open, plus newly created source files).
                let open: HashSet<PathBuf> =
                    self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
                let mut seen = HashSet::new();
                let candidates: Vec<(PathBuf, incremental::Version)> = paths
                    .into_iter()
                    .filter(|p| seen.insert(p.clone()))
                    .filter(|p| {
                        open.contains(p)
                            || self.registry.is_tracked(p)
                            || highlight::detect(p).is_some()
                    })
                    .map(|p| {
                        let v = self.registry.version(&p).unwrap_or(0);
                        (p, v)
                    })
                    .collect();
                if candidates.is_empty() {
                    return Task::none();
                }
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || watch::rehash(candidates))
                            .await
                            .unwrap_or_default()
                    },
                    Message::FilesRehashed,
                )
            }
            Message::FilesRehashed(events) => {
                let mut tasks = Vec::new();
                let mut index_dirty = false;
                let mut structural = false;
                let mut graph_dirty = false;
                let mut refreshed = 0usize;
                let mut touched: Vec<PathBuf> = Vec::new();
                // One resolver for the whole batch, over the current file set. A
                // structural change re-resolves the whole graph later (once the
                // rescan lands the new file set); here we only refresh out-edges.
                let resolver = self.import_resolver();
                for event in events {
                    match event {
                        watch::FileEvent::Modified(c) => {
                            touched.push(c.path.clone());
                            // A modification of an untracked path is a creation —
                            // the tree/file list must gain it.
                            structural |= !self.registry.is_tracked(&c.path);
                            self.registry.set(c.path.clone(), c.hash);
                            let lang_key = highlight::detect(&c.path);

                            // Re-index this one file in place (open or not).
                            if let Some(lang) = lang_key {
                                let rel = self.rel_of(&c.path);
                                let syms = index::file_symbols(&c.path, &rel, &c.content, lang);
                                if syms.is_empty() {
                                    index_dirty |= self.symbol_index_by_file.remove(&c.path).is_some();
                                } else {
                                    self.symbol_index_by_file.insert(c.path.clone(), syms);
                                    index_dirty = true;
                                }
                                // Re-extract this file's imports and refresh its
                                // out-edges in the graph.
                                if let Some(res) = &resolver {
                                    let raw = index::file_imports(&c.content, lang);
                                    graph_dirty |= self.import_graph.set_file(
                                        c.path.clone(),
                                        raw,
                                        res,
                                        highlight::detect,
                                    );
                                }
                            }

                            // Refresh every pane showing this file, keeping the
                            // reader's scroll/caret/folds so nothing jumps.
                            let mut on_screen = false;
                            for slot in &mut self.panes {
                                if let Some(v) = slot.as_mut().filter(|v| v.abs == c.path) {
                                    let lines = highlight::plain_lines(&c.content);
                                    v.reload(c.content.clone(), lines);
                                    on_screen = true;
                                }
                            }
                            if on_screen {
                                refreshed += 1;
                                tasks.push(self.content_tasks(
                                    c.path.clone(),
                                    c.content.clone(),
                                    lang_key,
                                ));
                                if let Some(lang) = lang_key
                                    && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
                                {
                                    self.lsp_doc_rev += 1;
                                    client.did_change(&c.path, self.lsp_doc_rev, &c.content);
                                }
                            }
                        }
                        watch::FileEvent::Deleted(path) => {
                            touched.push(path.clone());
                            structural = true;
                            self.registry.remove(&path);
                            index_dirty |= self.symbol_index_by_file.remove(&path).is_some();
                            self.import_graph.remove_file(&path);
                            graph_dirty = true;
                            if self.panes.iter().flatten().any(|v| v.abs == path) {
                                self.status = format!("{} was deleted on disk", self.rel_of(&path));
                            }
                        }
                    }
                }
                if index_dirty {
                    self.rebuild_symbol_index();
                }
                // A pure content change only refreshes out-edges, so update the
                // tree now. A structural change re-resolves the whole graph once
                // the rescan lands the new file set (see `TreeUpdated`).
                if graph_dirty && !structural {
                    self.import_cycles = self.import_graph.cycles();
                    self.refresh_import_tree();
                }
                // If a file the open call hierarchy references changed, the tree
                // may now be out of date — flag it (re-run `gc` to refresh).
                if let Some(t) = &mut self.call_graph
                    && !t.stale
                    && touched.iter().any(|p| t.depends_on(p))
                {
                    t.stale = true;
                }
                // Keep the open LSP-precise call graph fresh: re-query just the
                // changed files' functions, coalescing while a refine is running.
                if self.project_calls_precise && self.overlay == Some(Overlay::ProjectCalls) {
                    let changed: HashSet<PathBuf> = touched
                        .iter()
                        .filter(|p| highlight::detect(p).is_some())
                        .cloned()
                        .collect();
                    if !changed.is_empty() {
                        self.precise_pending.extend(changed);
                        if self.refine_progress.is_none() {
                            let pending = std::mem::take(&mut self.precise_pending);
                            tasks.push(self.refine_incremental(pending));
                        }
                    }
                }
                // A created/deleted/renamed file changes the tree and Cmd+P list;
                // rebuild them off-thread (the watcher already debounced the burst).
                if structural
                    && let Some(root) = self.project.as_ref().map(|p| p.root.clone())
                {
                    tasks.push(self.rescan_tree(root));
                }
                if refreshed == 1 {
                    self.status = "Refreshed a file changed on disk".to_string();
                } else if refreshed > 1 {
                    self.status = format!("Refreshed {refreshed} files changed on disk");
                }
                Task::batch(tasks)
            }
            Message::CodeScrolled(pane, viewport) => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.scroll_y = viewport.absolute_offset().y;
                    v.viewport_h = viewport.bounds().height;
                }
                Task::none()
            }
            Message::PaneFocused(pane) => {
                if pane == 0 || self.split {
                    self.active = pane;
                    // The Imports tab follows the focused pane's file.
                    self.refresh_import_tree();
                }
                Task::none()
            }
            Message::ToggleSplit => {
                if self.split {
                    self.split = false;
                    self.panes[1] = None;
                    self.active = 0;
                } else {
                    self.split = true;
                    // Duplicate the current file for side-by-side reading.
                    self.panes[1] = self.panes[0].clone();
                    self.active = 1;
                }
                Task::none()
            }
            Message::SelectStart { pane, line, col } => {
                if pane == 0 || self.split {
                    self.active = pane;
                }
                // Clicking the code gives it keyboard focus for cursor motion.
                self.code_focused = true;
                // Cmd/Ctrl-click is go-to-definition, not selection.
                if self.modifiers.command() && !self.modifiers.shift() {
                    return self.goto_definition(pane, line, col);
                }
                let extend = self.modifiers.shift();
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    let head = (line, col);
                    match (extend, v.selection) {
                        // Shift-click keeps the existing anchor and moves the head.
                        (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                        _ => v.selection = Some((head, head)),
                    }
                    v.caret = Some(head);
                    self.selecting = true;
                }
                Task::none()
            }
            Message::SelectDrag { pane, line, col } => {
                if self.selecting
                    && pane == self.active
                    && let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut)
                    && let Some((anchor, _)) = v.selection
                {
                    let head = (line, col);
                    v.selection = Some((anchor, head));
                    v.caret = Some(head);
                }
                Task::none()
            }
            Message::SelectEnd => {
                self.selecting = false;
                Task::none()
            }
            Message::FoldToggle { pane, line } => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.toggle_fold(line);
                }
                Task::none()
            }
            Message::MinimapScrolled { pane, fraction } => {
                let lh = self.line_height();
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    let total = v.content_rows() as f32 * lh;
                    let max_y = (total - v.viewport_h).max(0.0);
                    // Center the clicked fraction in the viewport.
                    let y = (fraction * total - v.viewport_h / 2.0).clamp(0.0, max_y);
                    v.scroll_y = y;
                    return operation::scroll_to(
                        ui::code_scroll_id(pane),
                        AbsoluteOffset { x: 0.0, y },
                    );
                }
                Task::none()
            }
            Message::CopySelection => {
                let Some(text) = self.active_viewer().and_then(Viewer::selected_text) else {
                    return Task::none();
                };
                let n = text.lines().count();
                self.status = format!("Copied {n} line{}", if n == 1 { "" } else { "s" });
                iced::clipboard::write(text)
            }
            Message::SidebarTabPicked(tab) => {
                self.sidebar = tab;
                match tab {
                    SidebarTab::Search => {
                        // The search input takes keyboard focus.
                        self.code_focused = false;
                        operation::focus(ui::search_input_id())
                    }
                    SidebarTab::Imports => {
                        // Sync the tree with the current file when the tab opens.
                        self.refresh_import_tree();
                        Task::none()
                    }
                    _ => Task::none(),
                }
            }
            Message::SearchQueryChanged(query) => {
                self.search.query = query;
                Task::none()
            }
            Message::SearchToggle(opt) => {
                match opt {
                    SearchOpt::Regex => self.search.regex = !self.search.regex,
                    SearchOpt::Case => self.search.case_sensitive = !self.search.case_sensitive,
                    SearchOpt::WholeWord => self.search.whole_word = !self.search.whole_word,
                }
                // Re-run live so the effect of the toggle is immediate.
                self.run_search()
            }
            Message::SearchIncludeChanged(s) => {
                self.search.include = s;
                Task::none()
            }
            Message::SearchExcludeChanged(s) => {
                self.search.exclude = s;
                Task::none()
            }
            Message::SearchSubmitted => self.run_search(),
            Message::SearchDone { result } => {
                self.search.running = false;
                self.search.error = result.error.clone();
                self.status = match &result.error {
                    Some(e) => e.clone(),
                    None if result.hits.len() >= search::MAX_HITS => {
                        format!("{}+ matches (capped)", result.hits.len())
                    }
                    None => format!("{} matches", result.hits.len()),
                };
                self.search.hits = result.hits;
                Task::none()
            }
            Message::FinderOpened(mode) => {
                if self.project.is_none() {
                    return Task::none();
                }
                self.finder.open = true;
                self.finder.mode = mode;
                self.finder.query.clear();
                self.code_focused = false; // the finder input takes focus
                self.refresh_finder();
                operation::focus(ui::finder_input_id())
            }
            Message::FinderClosed => {
                self.finder.open = false;
                self.code_focused = true; // back to reading
                Task::none()
            }
            Message::FinderQueryChanged(query) => {
                self.finder.query = query;
                self.refresh_finder();
                Task::none()
            }
            Message::FinderPick(idx) => self.finder_open_index(idx),
            Message::FinderConfirm => {
                if let Some(line) = self.finder.goto_line() {
                    self.finder.open = false;
                    if let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) {
                        return self.open_file(abs, Some(line), true);
                    }
                    return Task::none();
                }
                match self.finder.results.get(self.finder.selected).copied() {
                    Some(idx) => self.finder_open_index(idx),
                    None => Task::none(),
                }
            }
            Message::GotoLineRequested => {
                if self.project.is_none() {
                    return Task::none();
                }
                self.finder.open = true;
                self.finder.mode = FinderMode::Files;
                self.finder.query = ":".to_string();
                self.refresh_finder();
                Task::batch([
                    operation::focus(ui::finder_input_id()),
                    operation::move_cursor_to_end(ui::finder_input_id()),
                ])
            }
            Message::BookmarkToggled => {
                let line_height = self.line_height();
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let Some(v) = self.active_viewer() else {
                    return Task::none();
                };
                let line = v.current_line(line_height);
                let mut preview = v.line_text(line).trim().to_string();
                if preview.chars().count() > 80 {
                    preview = preview.chars().take(80).collect();
                }
                let rel = v.rel.clone();
                let added = bookmarks::toggle(&mut self.bookmarks, &rel, line, preview);
                self.status = match bookmarks::save(&root, &self.bookmarks) {
                    Ok(()) if added => format!("Bookmarked {rel}:{line}"),
                    Ok(()) => format!("Removed bookmark {rel}:{line}"),
                    Err(e) => format!("Cannot write .clew/bookmarks.json: {e}"),
                };
                Task::none()
            }
            Message::BookmarkRemoved(idx) => {
                if idx < self.bookmarks.len() {
                    self.bookmarks.remove(idx);
                    if let Some(p) = &self.project
                        && let Err(e) = bookmarks::save(&p.root, &self.bookmarks)
                    {
                        self.status = format!("Cannot write .clew/bookmarks.json: {e}");
                    }
                }
                Task::none()
            }
            Message::GoBack => match self.history.back() {
                Some(loc) => self.open_file(loc.path, loc.line, false),
                None => Task::none(),
            },
            Message::GoForward => match self.history.forward() {
                Some(loc) => self.open_file(loc.path, loc.line, false),
                None => Task::none(),
            },
            Message::ToggleOutline => {
                self.show_outline = !self.show_outline;
                Task::none()
            }
            Message::ToggleDiff => {
                // Toggle off if already showing this file's diff.
                let active_abs = self.active_viewer().map(|v| v.abs.clone());
                if let (Some(d), Some(abs)) = (&self.diff, &active_abs)
                    && d.abs == *abs
                {
                    self.diff = None;
                    return Task::none();
                }
                let Some(abs) = active_abs else {
                    return Task::none();
                };
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let rel = self.rel_of(&abs);
                Task::perform(
                    async move {
                        let file = abs.clone();
                        let lines = tokio::task::spawn_blocking(move || {
                            git::diff_lines(&root, &file).unwrap_or_default()
                        })
                        .await
                        .unwrap_or_default();
                        (abs, rel, lines)
                    },
                    |(abs, rel, lines)| Message::DiffLoaded { abs, rel, lines },
                )
            }
            Message::DiffLoaded { abs, rel, lines } => {
                self.diff = Some(DiffState { abs, rel, lines });
                Task::none()
            }
            Message::OutlineJump(line) => match self.active_viewer() {
                Some(v) => {
                    let abs = v.abs.clone();
                    self.open_file(abs, Some(line), true)
                }
                None => Task::none(),
            },
            Message::FontSizeDelta(delta) => {
                let old = self.line_height();
                self.font_size = (self.font_size + delta).clamp(9.0, 22.0);
                self.rescale_scroll(old)
            }
            Message::FontSizeReset => {
                let old = self.line_height();
                self.font_size = DEFAULT_FONT_SIZE;
                self.rescale_scroll(old)
            }
            Message::KeyPressed(key, modifiers) => self.handle_key(key, modifiers),
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                if !modifiers.command() {
                    self.hover = None; // hover is a Cmd-hover affordance
                }
                Task::none()
            }
            Message::WindowResized(size) => {
                self.window_width = size.width;
                // Keep the materialized window generous enough for the new
                // height until the next scroll event refines it.
                for v in self.panes.iter_mut().flatten() {
                    v.viewport_h = v.viewport_h.max(size.height);
                }
                Task::none()
            }
            Message::LspStartResult { language, result } => match result {
                Ok(client) => {
                    // Open every already-loaded document of this language.
                    let open_task = self.open_docs_for_language(&language, &client);
                    self.lsp.insert(language, LspSlot::Ready(client));
                    open_task
                }
                Err(e) => {
                    self.status = format!("{language} server failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::LspConsentDismissed => {
                if let Some(c) = self.pending_lsp_consent.take() {
                    self.lsp.insert(
                        c.language,
                        LspSlot::Unsupported("server download declined".into()),
                    );
                }
                Task::none()
            }
            Message::LspConsentAllowed => {
                let Some(c) = self.pending_lsp_consent.take() else {
                    return Task::none();
                };
                self.lsp.insert(c.language.clone(), LspSlot::Starting);
                let (dest, language, version) = (c.dest_dir, c.language, c.version);
                match c.provision {
                    LspProvision::Download(download) => {
                        self.status = format!("Downloading {}…", c.server_name);
                        Task::perform(
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    lsp::store::download_and_install(&download, &dest)
                                })
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()))
                            },
                            move |result| Message::LspDownloadResult {
                                language: language.clone(),
                                result,
                            },
                        )
                    }
                    LspProvision::Install(install) => {
                        self.status = format!("Installing {}…", c.server_name);
                        Task::perform(
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    lsp::store::toolchain_install(&install, &version, &dest)
                                })
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()))
                            },
                            move |result| Message::LspDownloadResult {
                                language: language.clone(),
                                result,
                            },
                        )
                    }
                }
            }
            Message::LspDownloadResult { language, result } => match result {
                Ok(exe) => {
                    self.status = format!("{language} server installed");
                    self.start_lsp_with(&language, exe)
                }
                Err(e) => {
                    self.status = format!("{language} server download failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::GotoDefinition { pane, line, col } => self.goto_definition(pane, line, col),
            Message::ContextMenuOpened {
                pane,
                line,
                col,
                x,
                y,
            } => {
                if pane == 0 || self.split {
                    self.active = pane;
                }
                self.context_menu = Some(ContextMenu {
                    pane,
                    line,
                    col,
                    x,
                    y,
                });
                Task::none()
            }
            Message::ContextMenuClosed => {
                self.context_menu = None;
                Task::none()
            }
            Message::ContextGoto(kind) => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.goto_request(menu.pane, menu.line, menu.col, kind)
            }
            Message::FindOpened => {
                if self.active_viewer().is_none() {
                    return Task::none();
                }
                self.find.open = true;
                self.code_focused = false; // the find input takes focus
                // Reveal everything so matches inside collapsed folds are shown.
                if let Some(v) = self.active_viewer_mut() {
                    v.expand_all();
                }
                if let Some(v) = self.active_viewer() {
                    let lines = v.lines.clone();
                    self.find.recompute(&lines);
                }
                Task::batch([
                    operation::focus(ui::find_input_id()),
                    operation::select_all(ui::find_input_id()),
                ])
            }
            Message::FindQueryChanged(q) => {
                self.find.query = q;
                if let Some(v) = self.active_viewer() {
                    let lines = v.lines.clone();
                    self.find.recompute(&lines);
                }
                self.jump_to_find_match()
            }
            Message::FindStep(delta) => {
                self.find.step(delta);
                self.jump_to_find_match()
            }
            Message::FindClosed => {
                self.find.open = false;
                self.code_focused = true;
                Task::none()
            }
            Message::HoverRequested {
                pane,
                line,
                col,
                x,
                y,
            } => {
                // Same token already shown: just reposition.
                if let Some(h) = &mut self.hover
                    && h.line == line
                    && h.col == col
                {
                    h.x = x;
                    h.y = y;
                    return Task::none();
                }
                self.hover = Some(HoverState {
                    line,
                    col,
                    x,
                    y,
                    text: None,
                });
                // Pull the request context before mutating self further.
                let Some((lang, path, source_line)) =
                    self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                        v.lang_key.map(|l| {
                            (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string())
                        })
                    })
                else {
                    return Task::none();
                };
                let client = match self.lsp.get(lang) {
                    Some(LspSlot::Ready(c)) => c.clone(),
                    _ => return Task::none(),
                };
                let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
                let character = viewer::character_offset(&source_line, col, utf16);
                Task::perform(
                    async move { client.hover(&path, line, character).await },
                    move |result| Message::HoverResult {
                        line,
                        col,
                        text: result.ok().flatten(),
                    },
                )
            }
            Message::HoverResult { line, col, text } => {
                if let Some(h) = &mut self.hover
                    && h.line == line
                    && h.col == col
                {
                    h.text = text;
                }
                Task::none()
            }
            Message::DefinitionResult { result } => match result {
                Ok(targets) if !targets.is_empty() => {
                    let t = &targets[0];
                    let abs = t.path.clone();
                    let target_line = t.line + 1;
                    self.open_file(abs, Some(target_line), true)
                }
                Ok(_) => {
                    self.status = "No definition found".into();
                    Task::none()
                }
                Err(e) => {
                    self.status = format!("Definition failed: {e}");
                    Task::none()
                }
            },
            Message::ReferencesResult { result } => match result {
                Ok(refs) if !refs.is_empty() => {
                    self.status = format!("{} reference(s) — showing them in Search", refs.len());
                    self.show_references(refs)
                }
                Ok(_) => {
                    self.status = "No references".into();
                    Task::none()
                }
                Err(e) => {
                    self.status = format!("References failed: {e}");
                    Task::none()
                }
            },
            Message::CallHierarchyRequested => {
                let pane = self.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    return Task::none();
                };
                self.call_hierarchy_at(pane, line, col)
            }
            Message::CallHierarchyFromMenu => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.call_hierarchy_at(menu.pane, menu.line, menu.col)
            }
            Message::CallHierarchyPrepared { direction, lang, items } => {
                if items.is_empty() {
                    self.status = "No call hierarchy for the symbol under the cursor".into();
                    return Task::none();
                }
                self.call_graph = Some(callgraph::CallTree::new(direction, lang, items));
                self.sidebar = SidebarTab::Calls;
                let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
                Task::batch(roots.into_iter().map(|r| self.fetch_children(r)).collect::<Vec<_>>())
            }
            Message::CallHierarchyExpand(id) => {
                let needs = self
                    .call_graph
                    .as_ref()
                    .is_some_and(|t| t.needs_fetch(id));
                if needs {
                    self.fetch_children(id)
                } else {
                    if let Some(t) = &mut self.call_graph {
                        t.toggle(id);
                    }
                    Task::none()
                }
            }
            Message::CallHierarchyChildren { id, items } => {
                // Keep only project-internal callers/callees — don't descend into
                // external libraries / std.
                let root = self.project.as_ref().map(|p| p.root.clone());
                let items: Vec<_> = match &root {
                    Some(r) => items.into_iter().filter(|i| i.path.starts_with(r)).collect(),
                    None => items,
                };
                let new_ids = match &mut self.call_graph {
                    Some(t) => t.set_children(id, items),
                    None => return Task::none(),
                };
                // In "expand all" mode, recurse into the new project-internal
                // children until the frontier is empty or the node cap is hit.
                let recurse = self
                    .call_graph
                    .as_ref()
                    .is_some_and(|t| t.full && t.node_count() < callgraph::MAX_NODES);
                if recurse {
                    let to_fetch: Vec<usize> = new_ids
                        .into_iter()
                        .filter(|&cid| {
                            self.call_graph.as_ref().is_some_and(|t| t.needs_fetch(cid))
                        })
                        .collect();
                    Task::batch(
                        to_fetch
                            .into_iter()
                            .map(|cid| self.fetch_children(cid))
                            .collect::<Vec<_>>(),
                    )
                } else {
                    Task::none()
                }
            }
            Message::CallHierarchyDirection => {
                let Some(tree) = &self.call_graph else {
                    return Task::none();
                };
                let toggled = tree.direction.toggled();
                let lang = tree.lang;
                let was_full = tree.full;
                let root_items = tree
                    .roots()
                    .iter()
                    .map(|&r| tree.node(r).item.clone())
                    .collect();
                let mut rebuilt = callgraph::CallTree::new(toggled, lang, root_items);
                rebuilt.full = was_full; // keep "expand all" across a direction flip
                self.call_graph = Some(rebuilt);
                let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
                Task::batch(roots.into_iter().map(|r| self.fetch_children(r)).collect::<Vec<_>>())
            }
            Message::CallHierarchyExpandAll => {
                let frontier = match &mut self.call_graph {
                    Some(t) => {
                        t.full = true;
                        t.unfetched_frontier()
                    }
                    None => return Task::none(),
                };
                Task::batch(frontier.into_iter().map(|id| self.fetch_children(id)).collect::<Vec<_>>())
            }
            Message::ImportExpand(id) => {
                if let (Some(mut tree), Some(root)) =
                    (self.import_tree.take(), self.project.as_ref().map(|p| p.root.clone()))
                {
                    // Guard against a stale id from a since-rebuilt (smaller) tree.
                    if id < tree.node_count() {
                        tree.toggle(id, &self.import_graph, &root);
                    }
                    self.import_tree = Some(tree);
                }
                Task::none()
            }
            Message::ImportDirection => {
                self.import_dir = self.import_dir.toggled();
                // A direction flip resets the (now meaningless) expand-all state.
                if let Some(t) = &mut self.import_tree {
                    t.full = false;
                }
                self.refresh_import_tree();
                Task::none()
            }
            Message::ImportExpandAll => {
                if let (Some(mut tree), Some(root)) =
                    (self.import_tree.take(), self.project.as_ref().map(|p| p.root.clone()))
                {
                    tree.expand_all(&self.import_graph, &root);
                    self.import_tree = Some(tree);
                }
                Task::none()
            }
            Message::OpenOverlay(which) => {
                // The server panel and an overlay are mutually exclusive modals.
                self.server_panel = false;
                self.overlay = Some(which);
                // The call graph is built on demand; (re)build it if the project
                // changed since the last build — but never launch a second build
                // while one is already in flight (single-flight).
                if which == Overlay::ProjectCalls
                    && !self.building_calls
                    && (self.project_calls.is_empty()
                        || self.project_calls_rev != self.registry.revision())
                {
                    return self.build_project_calls();
                }
                self.refresh_graph_layout();
                Task::none()
            }
            Message::OverlayViewToggle => {
                self.graph_mode = !self.graph_mode;
                if self.graph_mode {
                    self.refresh_graph_layout();
                }
                Task::none()
            }
            Message::CloseOverlay => {
                self.overlay = None;
                Task::none()
            }
            Message::OverlayOpenImports(path) => {
                self.overlay = None;
                self.sidebar = SidebarTab::Imports;
                self.open_file(path, None, true)
            }
            Message::OverlayOpenAt { abs, line } => {
                self.overlay = None;
                self.open_file(abs, Some(line), true)
            }
            Message::ProjectCallsBuilt { root, graph } => {
                // Drop a late result from a previous project.
                if self.project.as_ref().map(|p| &p.root) != Some(&root) {
                    return Task::none();
                }
                self.building_calls = false;
                self.project_calls = graph;
                // This is the name-based approximation; a superseding refine is
                // no longer valid, and no precise result is in effect.
                self.project_calls_precise = false;
                self.refine_progress = None;
                self.calls_gen += 1;
                self.precise_edges = projectcalls::SymEdges::default();
                self.precise_pending.clear();
                // The map depends on the freshly built graph.
                if self.overlay == Some(Overlay::ProjectCalls) {
                    self.refresh_graph_layout();
                }
                // If files changed while this build was running, its data is
                // already stale — rebuild once so the open overlay self-heals.
                if self.overlay == Some(Overlay::ProjectCalls)
                    && self.project_calls_rev != self.registry.revision()
                {
                    return self.build_project_calls();
                }
                Task::none()
            }
            Message::RefineProjectCalls => self.refine_project_calls(),
            Message::RefineProgress { generation, done, total } => {
                if generation == self.calls_gen {
                    self.refine_progress = Some((done, total));
                }
                Task::none()
            }
            Message::ProjectCallsRefined { root, generation, edges, graph } => {
                // Accept only the latest refine for the current project.
                if generation != self.calls_gen
                    || self.project.as_ref().map(|p| &p.root) != Some(&root)
                {
                    return Task::none();
                }
                self.project_calls = graph;
                self.precise_edges = edges;
                self.project_calls_precise = true;
                self.refine_progress = None;
                self.status = "Call graph refined with LSP".into();
                if self.overlay == Some(Overlay::ProjectCalls) {
                    self.refresh_graph_layout();
                }
                // Files changed while this refine ran → fold them in now.
                if self.overlay == Some(Overlay::ProjectCalls) && !self.precise_pending.is_empty() {
                    let changed = std::mem::take(&mut self.precise_pending);
                    return self.refine_incremental(changed);
                }
                Task::none()
            }
            Message::Tick => {
                // Mark current diagnostics as seen so ticks quiesce once caught up.
                let versions: Vec<(String, u64)> = self
                    .lsp
                    .iter()
                    .filter_map(|(lang, slot)| match slot {
                        LspSlot::Ready(c) => Some((lang.clone(), c.diag_version())),
                        _ => None,
                    })
                    .collect();
                for (lang, ver) in versions {
                    self.seen_diag_version.insert(lang, ver);
                }
                Task::none() // the render itself reflects the latest diagnostics
            }
            Message::ToggleServerPanel => {
                self.server_panel = !self.server_panel;
                if self.server_panel {
                    self.installed_servers = lsp::store::installed_servers();
                }
                Task::none()
            }
            Message::LspRestart(language) => {
                // Drop the running server (kills its child), then re-provision.
                self.lsp.remove(&language);
                self.lsp_opened.retain(|p| {
                    // Re-open docs of this language on restart.
                    highlight::detect(p) != Some(language.as_str())
                });
                self.ensure_lsp(&language)
            }
            Message::LspRemove { name, version } => {
                // Stop any running instance of this server first.
                let langs: Vec<String> = lsp::registry::by_name(&name)
                    .map(|s| s.languages.iter().map(|l| l.to_string()).collect())
                    .unwrap_or_default();
                for lang in langs {
                    self.lsp.remove(&lang);
                }
                match lsp::store::remove(&name, &version) {
                    Ok(_) => self.status = format!("Removed {name} {version}"),
                    Err(e) => self.status = format!("Remove failed: {e}"),
                }
                self.installed_servers = lsp::store::installed_servers();
                Task::none()
            }
            Message::LspDownloadFor(language) => {
                // Force a fresh provisioning attempt for this language.
                self.lsp.remove(&language);
                self.ensure_lsp(&language)
            }
        }
    }

    fn on_scan_done(&mut self, result: ScanResult) -> Task<Message> {
        self.scanning = false;
        self.status = format!(
            "{} files{}",
            result.files.len(),
            if result.truncated { " (truncated)" } else { "" }
        );
        self.expanded.clear();
        self.panes = [None, None];
        self.split = false;
        self.active = 0;
        self.history.clear();
        self.finder = Finder::default();
        self.search = SearchState::default();
        self.bookmarks = bookmarks::load(&result.root);
        self.symbol_index = Arc::new(Vec::new());
        self.symbol_index_by_file.clear();
        self.registry.clear();
        self.call_graph = None;
        self.import_graph = imports::ImportGraph::default();
        self.import_tree = None;
        self.import_cycles = Vec::new();
        self.project_calls = projectcalls::ProjectCallGraph::default();
        self.project_calls_rev = 0;
        self.building_calls = false;
        self.project_calls_precise = false;
        self.calls_gen += 1;
        self.refine_progress = None;
        self.precise_edges = projectcalls::SymEdges::default();
        self.precise_pending = HashSet::new();
        self.overlay = None;
        self.graph_layout = None;
        // Drop any servers from the previous project (kills their children).
        self.lsp.clear();
        self.lsp_opened.clear();
        self.pending_lsp_consent = None;
        self.lsp_config = lsp::config::ProjectLspConfig::load(&result.root).unwrap_or_default();
        // Languages actually present in the project that clew ships a server
        // for — drives which rows the server panel shows.
        let mut langs: Vec<String> = result
            .files
            .iter()
            .filter_map(|f| highlight::detect(&f.abs))
            .filter(|l| lsp::registry::default_for_language(l).is_some())
            .map(|l| l.to_string())
            .collect();
        langs.sort();
        langs.dedup();
        self.project_languages = langs;
        let files = Arc::new(result.files);
        self.project = Some(Project {
            root: result.root,
            tree: result.tree,
            files: files.clone(),
            truncated: result.truncated,
        });

        // Build the project-wide symbol index in the background, warm-starting
        // from the persistent cache (only files changed while clew was closed
        // are re-read/re-parsed), and persist the refreshed cache.
        self.indexing = true;
        let index_root = self.project.as_ref().unwrap().root.clone();
        let tag_root = index_root.clone();
        let index_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || index::build_indexed_warm(&index_root, files))
                    .await
                    .unwrap_or_default()
            },
            move |indexed| Message::SymbolIndexDone {
                root: tag_root.clone(),
                indexed,
            },
        );

        let open_task = match self.pending_open.take() {
            Some(file) => self.open_file(file, None, true),
            None => Task::none(),
        };
        Task::batch([index_task, open_task])
    }

    /// Status text and the action button for a language row in the server
    /// panel. Distinguishes running / installed-but-idle / not-downloaded so an
    /// installed server never shows a misleading "Download".
    pub fn lsp_row(&self, language: &str) -> (String, Option<(&'static str, Message)>) {
        let restart = || {
            Some((
                "Restart",
                Message::LspRestart(language.to_string()),
            ))
        };
        let provision = |label: &'static str| {
            Some((label, Message::LspDownloadFor(language.to_string())))
        };
        match self.lsp.get(language) {
            Some(LspSlot::Ready(c)) => (c.progress().unwrap_or_else(|| "ready".into()), restart()),
            Some(LspSlot::Starting) => ("starting…".into(), None),
            Some(LspSlot::Failed(e)) => (format!("error: {e}"), provision("Retry")),
            Some(LspSlot::Unsupported(e)) => (e.clone(), None),
            Some(LspSlot::AwaitingConsent) => ("download pending".into(), provision("Download")),
            None => match self.lsp_config.resolve(language) {
                Some(server) => match lsp::store::locate(&server) {
                    lsp::store::Located::Ready(_) => {
                        ("installed · starts on open".into(), provision("Start"))
                    }
                    lsp::store::Located::NeedsDownload { .. } => {
                        ("not downloaded".into(), provision("Download"))
                    }
                    lsp::store::Located::NeedsInstall { .. } => {
                        ("not installed".into(), provision("Install"))
                    }
                    lsp::store::Located::Unsupported(m) => (m, None),
                },
                None => ("no server".into(), None),
            },
        }
    }

    /// Languages to show in the server panel: those present in the project,
    /// plus any installed or running server (so they can be managed anywhere).
    pub fn managed_languages(&self) -> Vec<String> {
        let mut langs = self.project_languages.clone();
        for srv in &self.installed_servers {
            if let Some(spec) = lsp::registry::by_name(&srv.name) {
                langs.extend(spec.languages.iter().map(|l| l.to_string()));
            }
        }
        langs.extend(self.lsp.keys().cloned());
        langs.sort();
        langs.dedup();
        langs
    }

    /// Ensure a language server is provisioned/started for `language`, and open
    /// any already-loaded documents once it is ready. Idempotent.
    fn ensure_lsp(&mut self, language: &str) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        match self.lsp.get(language) {
            Some(LspSlot::Ready(client)) => {
                let client = client.clone();
                return self.open_docs_for_language(language, &client);
            }
            // Starting / failed / unsupported / awaiting consent: nothing to do.
            Some(_) => return Task::none(),
            None => {}
        }

        let Some(server) = self.lsp_config.resolve(language) else {
            self.lsp.insert(
                language.to_string(),
                LspSlot::Unsupported("no server for this language".into()),
            );
            return Task::none();
        };
        let (provision, dest_dir) = match lsp::store::locate(&server) {
            lsp::store::Located::Ready(exe) => return self.start_lsp_with(language, exe),
            lsp::store::Located::NeedsDownload { download, dest_dir } => {
                (LspProvision::Download(download), dest_dir)
            }
            lsp::store::Located::NeedsInstall {
                install, dest_dir, ..
            } => (LspProvision::Install(install), dest_dir),
            lsp::store::Located::Unsupported(msg) => {
                self.lsp.insert(language.to_string(), LspSlot::Unsupported(msg));
                return Task::none();
            }
        };
        self.lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        self.pending_lsp_consent = Some(LspConsent {
            language: language.to_string(),
            server_name: server.server_name,
            version: server.version,
            provision,
            dest_dir,
        });
        Task::none()
    }

    /// Launch the server executable and run the handshake in the background.
    fn start_lsp_with(&mut self, language: &str, exe: PathBuf) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(server) = self.lsp_config.resolve(language) else {
            return Task::none();
        };
        self.lsp.insert(language.to_string(), LspSlot::Starting);
        let lang = language.to_string();
        let args = server.args.clone();
        let init = server.init_options.clone();
        Task::perform(
            async move { lsp::client::LspClient::start(&exe, &args, &root, init).await },
            move |result| Message::LspStartResult {
                language: lang.clone(),
                result,
            },
        )
    }

    /// Send `didOpen` for every loaded document of `language` not yet opened.
    fn open_docs_for_language(
        &mut self,
        language: &str,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        let docs: Vec<(PathBuf, Arc<String>)> = self
            .panes
            .iter()
            .flatten()
            .filter(|v| v.lang_key == Some(language))
            .map(|v| (v.abs.clone(), v.source.clone()))
            .collect();
        for (path, source) in docs {
            if self.lsp_opened.insert(path.clone()) {
                client.did_open(&path, language, 1, &source);
            }
        }
        Task::none()
    }

    /// Resolve the definition at a clicked (line, display col) in `pane`.
    fn goto_definition(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        self.goto_request(pane, line, col, GotoKind::Definition)
    }

    /// Kick off a project search from the current query and options.
    fn run_search(&mut self) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        if self.search.query.trim().is_empty() {
            self.search.hits.clear();
            self.search.error = None;
            self.search.ran = false;
            return Task::none();
        }
        self.search.running = true;
        self.search.ran = true;
        self.search.hits.clear();
        let files = project.files.clone();
        let opts = search::SearchOptions {
            query: self.search.query.trim().to_string(),
            regex: self.search.regex,
            case_sensitive: self.search.case_sensitive,
            whole_word: self.search.whole_word,
            include: self.search.include.clone(),
            exclude: self.search.exclude.clone(),
        };
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || search::search(files, opts))
                    .await
                    .unwrap_or_default()
            },
            |result| Message::SearchDone { result },
        )
    }

    /// Show LSP references in the Search sidebar (reusing its result list).
    fn show_references(&mut self, refs: Vec<lsp::client::Target>) -> Task<Message> {
        let hits: Vec<SearchHit> = refs
            .into_iter()
            .take(search::MAX_HITS)
            .map(|t| {
                let rel = self.rel_of(&t.path);
                let preview = std::fs::read_to_string(&t.path)
                    .ok()
                    .and_then(|s| s.lines().nth(t.line).map(|l| l.trim().to_string()))
                    .unwrap_or_default();
                SearchHit {
                    abs: t.path,
                    rel,
                    line: t.line + 1,
                    preview,
                }
            })
            .collect();
        self.search.query = "(references)".to_string();
        self.search.ran = true;
        self.search.running = false;
        self.search.hits = hits;
        self.sidebar = SidebarTab::Search;
        self.code_focused = false;
        Task::none()
    }

    /// Run a navigation request from the active pane's cursor.
    fn goto_at_cursor(&mut self, kind: GotoKind) -> Task<Message> {
        let pane = self.active;
        let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
            return Task::none();
        };
        self.goto_request(pane, line, col, kind)
    }

    /// Dispatch an LSP navigation request (definition / references / …) at a
    /// clicked or cursor position.
    fn goto_request(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        kind: GotoKind,
    ) -> Task<Message> {
        // Pull everything we need from the viewer before mutating self.
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key
                    .map(|l| (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string()))
            })
        else {
            return Task::none();
        };

        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready (⌘T to search symbols)");
                return Task::none();
            }
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = format!("{}…", kind.verb());
        let is_references = matches!(kind, GotoKind::References);
        Task::perform(
            async move { client.navigate(kind.method(), &path, line, character).await },
            move |result| {
                if is_references {
                    Message::ReferencesResult { result }
                } else {
                    Message::DefinitionResult { result }
                }
            },
        )
    }

    /// Prepare a call hierarchy at a (display line, col) in `pane`, gated on the
    /// server actually supporting it. Shared by `gc` and the context menu.
    fn call_hierarchy_at(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key
                    .map(|l| (l, v.abs.clone(), v.source_line(line).unwrap_or("").to_string()))
            })
        else {
            return Task::none();
        };
        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        if !client.call_hierarchy {
            self.status = format!("Call hierarchy isn't supported for {lang}");
            return Task::none();
        }
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = "Building call hierarchy…".into();
        let direction = callgraph::Direction::Incoming;
        Task::perform(
            async move { client.prepare_call_hierarchy(&path, line, character).await },
            move |items| Message::CallHierarchyPrepared { direction, lang, items },
        )
    }

    /// Mark a node loading, then kick its fetch — the panel shows a spinner in
    /// the gap before the children arrive.
    fn fetch_children(&mut self, id: usize) -> Task<Message> {
        if let Some(t) = &mut self.call_graph {
            t.set_loading(id);
        }
        self.call_fetch_task(id)
    }

    /// Off-thread fetch of a call-tree node's callers/callees (direction from
    /// the tree), delivered as `CallHierarchyChildren`.
    fn call_fetch_task(&self, id: usize) -> Task<Message> {
        let Some(tree) = &self.call_graph else {
            return Task::none();
        };
        let client = match self.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return Task::none(),
        };
        let raw = tree.raw_of(id);
        let direction = tree.direction;
        Task::perform(
            async move {
                match direction {
                    callgraph::Direction::Incoming => client.incoming_calls(raw).await,
                    callgraph::Direction::Outgoing => client.outgoing_calls(raw).await,
                }
            },
            move |items| Message::CallHierarchyChildren { id, items },
        )
    }

    /// Re-flatten the per-file symbol map into `symbol_index` and refresh the
    /// finder when it is showing symbols.
    fn rebuild_symbol_index(&mut self) {
        self.symbol_index = Arc::new(index::flatten(&self.symbol_index_by_file));
        if self.finder.open && self.finder.mode == FinderMode::Symbols {
            self.finder.refresh_symbols(&self.symbol_index);
        }
    }

    /// Kick an off-thread (re)build of the project call graph from the current
    /// symbol index + file contents. Delivered as `ProjectCallsBuilt`.
    fn build_project_calls(&mut self) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        // Callable definitions to link against, from the symbol index.
        let defs: Vec<projectcalls::Def> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .map(|s| projectcalls::Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: s.abs.clone(),
                line: s.line,
            })
            .collect();
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        let tag_root = project.root.clone();
        // Import scope: each file → the internal files it imports, so a called
        // name resolves to the definition actually in scope.
        let scope = self.import_graph.scope_map();
        self.project_calls_rev = self.registry.revision();
        self.building_calls = true;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    // Read the current source of each supported, reasonably sized
                    // file (a big file's calls aren't worth the parse cost).
                    let sources: Vec<(PathBuf, String)> = files
                        .into_iter()
                        .filter(|f| highlight::detect(f).is_some())
                        .filter(|f| {
                            std::fs::metadata(f)
                                .map(|m| m.len() <= index::MAX_INDEX_FILE_BYTES)
                                .unwrap_or(false)
                        })
                        .filter_map(|f| std::fs::read_to_string(&f).ok().map(|c| (f, c)))
                        .collect();
                    projectcalls::ProjectCallGraph::build(defs, &sources, &scope)
                })
                .await
                .unwrap_or_default()
            },
            move |graph| Message::ProjectCallsBuilt {
                root: tag_root.clone(),
                graph,
            },
        )
    }

    /// Ready, call-hierarchy-capable servers keyed by language.
    fn call_hierarchy_clients(&self) -> HashMap<String, lsp::client::LspClient> {
        let mut clients = HashMap::new();
        for (lang, slot) in &self.lsp {
            if let LspSlot::Ready(c) = slot
                && c.call_hierarchy
            {
                clients.insert(lang.clone(), c.clone());
            }
        }
        clients
    }

    /// Every callable function whose language has a ready call-hierarchy server.
    fn refinable_defs(&self, clients: &HashMap<String, lsp::client::LspClient>) -> Vec<projectcalls::Def> {
        let all: Vec<projectcalls::Def> = self
            .symbol_index_by_file
            .values()
            .flatten()
            .map(|s| projectcalls::Def {
                name: s.name.clone(),
                kind: s.kind.clone(),
                file: s.abs.clone(),
                line: s.line,
            })
            .collect();
        projectcalls::ProjectCallGraph::callable(&all)
            .into_iter()
            .filter(|d| highlight::detect(&d.file).is_some_and(|l| clients.contains_key(l)))
            .collect()
    }

    /// Full LSP refine (the "Refine with LSP" button): query every project
    /// function and rebuild the precise graph from scratch.
    fn refine_project_calls(&mut self) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            self.status =
                "No language server ready — open a file to start one, then retry".into();
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        if all.is_empty() {
            self.status = "No functions to refine for the ready server(s)".into();
            return Task::none();
        }
        self.spawn_refine(clients, all.clone(), all, projectcalls::SymEdges::default(), None)
    }

    /// Incrementally refresh the precise graph after files changed: re-query only
    /// the changed files' functions and patch the edge set.
    fn refine_incremental(&mut self, changed: HashSet<PathBuf>) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        let query: Vec<projectcalls::Def> =
            all.iter().filter(|d| changed.contains(&d.file)).cloned().collect();
        let base = self.precise_edges.clone();
        // Even with nothing to re-query (e.g. all changed functions removed), we
        // still rebuild so deleted files' edges drop out.
        self.spawn_refine(clients, all, query, base, Some(changed))
    }

    /// Shared refine launcher. `query_defs` are LSP-queried; `all_defs` is the
    /// full node set the result maps onto; `base` is the starting edge set;
    /// `changed` (when incremental) is the files whose old edges to drop before
    /// re-querying, and also selects incoming+outgoing (vs incoming-only) queries.
    fn spawn_refine(
        &mut self,
        clients: HashMap<String, lsp::client::LspClient>,
        all_defs: Vec<projectcalls::Def>,
        query_defs: Vec<projectcalls::Def>,
        base: projectcalls::SymEdges,
        changed: Option<HashSet<PathBuf>>,
    ) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        let root = project.root.clone();
        self.calls_gen += 1;
        let generation = self.calls_gen;
        self.refine_progress = Some((0, query_defs.len()));
        if changed.is_none() {
            self.status = format!("Refining {} functions with LSP…", query_defs.len());
        }
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(output, all_defs, query_defs, base, changed, clients, root, generation)
        });
        Task::run(stream, |m| m)
    }

    /// Recompute the node-link layout for whichever overlay is open.
    fn refresh_graph_layout(&mut self) {
        self.graph_layout = match self.overlay {
            Some(Overlay::ProjectImports) => Some(self.import_graph_layout()),
            Some(Overlay::ProjectCalls) => Some(self.calls_graph_layout()),
            None => None,
        };
    }

    /// Force-directed layout of the import graph: nodes are files, sized by
    /// fan-in+fan-out, cycle members highlighted; edges are `use` dependencies.
    fn import_graph_layout(&self) -> graphlayout::Layout {
        let g = &self.import_graph;
        let files = g.files();
        let idx: HashMap<PathBuf, usize> =
            files.iter().cloned().enumerate().map(|(i, f)| (f, i)).collect();
        let cyclic: HashSet<PathBuf> = self.import_cycles.iter().flatten().cloned().collect();
        let nodes = files
            .iter()
            .map(|f| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (g.fan_in(f) + g.fan_out(f) + 1) as f32,
                cyclic: cyclic.contains(f),
            })
            .collect();
        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for f in &files {
            for e in g.imports(f) {
                if let imports::Target::Internal(t) = &e.target
                    && let (Some(&a), Some(&b)) = (idx.get(f), idx.get(t))
                {
                    edge_set.insert((a, b));
                }
            }
        }
        graphlayout::layout(nodes, edge_set.into_iter().collect())
    }

    /// Force-directed layout of the file-aggregated call graph: nodes are files
    /// sized by call degree; edges are cross-file call flow.
    fn calls_graph_layout(&self) -> graphlayout::Layout {
        let (files, edges) = self.project_calls.file_graph();
        let mut degree = vec![0usize; files.len()];
        for &(a, b) in &edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let nodes = files
            .iter()
            .enumerate()
            .map(|(i, f)| graphlayout::NodeInput {
                label: file_label(f),
                file: f.clone(),
                weight: (degree[i] + 1) as f32,
                cyclic: false,
            })
            .collect();
        graphlayout::layout(nodes, edges)
    }

    /// A resolver over the project's current file set (for building/refreshing
    /// the import graph). Cheap: in-memory path work plus one `go.mod` read.
    fn import_resolver(&self) -> Option<imports::Resolver> {
        let project = self.project.as_ref()?;
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        Some(imports::Resolver::new(&project.root, &files))
    }

    /// Rebuild the whole import graph from the per-file raw imports (after the
    /// index build) and refresh the tree.
    fn rebuild_import_graph(&mut self, raw: HashMap<PathBuf, Vec<imports::RawImport>>) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph = imports::ImportGraph::build(raw, &resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// Re-resolve every edge against the current file set (after a file was
    /// created/deleted/renamed, which can change how other files resolve).
    fn reresolve_import_graph(&mut self) {
        if let Some(resolver) = self.import_resolver() {
            self.import_graph.reresolve(&resolver, highlight::detect);
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
    }

    /// The file the Imports tab is focused on — the active pane's file.
    fn import_focus(&self) -> Option<PathBuf> {
        self.active_viewer().map(|v| v.abs.clone())
    }

    /// Rebuild the import tree for the focus file, preserving the current
    /// direction and "expand all" state. Cheap — pure in-memory graph lookups.
    fn refresh_import_tree(&mut self) {
        let (Some(root), Some(focus)) = (
            self.project.as_ref().map(|p| p.root.clone()),
            self.import_focus(),
        ) else {
            self.import_tree = None;
            return;
        };
        let was_full = self.import_tree.as_ref().is_some_and(|t| t.full);
        let mut tree = imports::ImportTree::new(&self.import_graph, &root, focus, self.import_dir);
        if was_full {
            tree.expand_all(&self.import_graph, &root);
        }
        self.import_tree = Some(tree);
    }

    fn refresh_finder(&mut self) {
        match self.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.project {
                    let files = p.files.clone();
                    self.finder.refresh_files(&files);
                }
            }
            FinderMode::Symbols => {
                let symbols = self.symbol_index.clone();
                self.finder.refresh_symbols(&symbols);
            }
        }
    }

    /// Gate every project open behind `.clew/` consent: an existing `.clew/`
    /// directory counts as consent already given; otherwise ask, and refuse
    /// to open the project when denied or not writable.
    fn request_open(&mut self, root: PathBuf) -> Task<Message> {
        // An existing .clew records consent already given: open straight away.
        if root.join(".clew").is_dir() {
            return self.start_scan(root);
        }
        // Otherwise ask via an in-app modal (see ui::consent_modal).
        self.pending_consent = Some(root);
        Task::none()
    }

    fn start_scan(&mut self, root: PathBuf) -> Task<Message> {
        self.scanning = true;
        self.status = format!("Scanning {}…", root.display());
        Task::perform(
            async move {
                let fallback_root = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback_root,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::ScanDone,
        )
    }

    /// Re-scan the tree off-thread after a structural change, delivering the
    /// result as `TreeUpdated` (a light swap, not a full project reopen).
    fn rescan_tree(&self, root: PathBuf) -> Task<Message> {
        Task::perform(
            async move {
                let fallback = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::TreeUpdated,
        )
    }

    fn finder_open_index(&mut self, idx: usize) -> Task<Message> {
        self.finder.open = false;
        match self.finder.mode {
            FinderMode::Files => {
                let Some(entry) = self
                    .project
                    .as_ref()
                    .and_then(|p| p.files.get(idx))
                    .cloned()
                else {
                    return Task::none();
                };
                self.open_file(entry.abs, None, true)
            }
            FinderMode::Symbols => {
                let Some(entry) = self.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// Open a file into the active pane, optionally jumping to a 1-based line.
    fn open_file(&mut self, abs: PathBuf, line: Option<usize>, push: bool) -> Task<Message> {
        if push {
            self.history.push(Loc {
                path: abs.clone(),
                line,
            });
        }
        // A jump lands the reader in the code view.
        self.code_focused = true;
        let pane = self.active;
        let line_height = self.line_height();
        // Same file already in the active pane: move the cursor and scroll.
        if let Some(v) = self.active_viewer_mut()
            && v.abs == abs
        {
            v.target_line = line;
            if let Some(l) = line {
                let l0 = l.saturating_sub(1);
                v.reveal(l0); // expand any fold hiding the jump target
                v.caret = Some((l0, 0));
            }
            let y = v.scroll_offset_for(line, line_height);
            v.scroll_y = y;
            return operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        }
        self.status = format!("Loading {}…", self.rel_of(&abs));
        Task::perform(load_file(pane, abs, line), |(pane, abs, target, result)| {
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            }
        })
    }

    fn on_file_loaded(
        &mut self,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        let rel = self.rel_of(&abs);
        let content = match result {
            Err(e) => {
                self.status = format!("{rel}: {e}");
                return Task::none();
            }
            Ok(content) => content,
        };

        let lang_key = highlight::detect(&abs);
        let source = Arc::new(content);
        let lines = highlight::plain_lines(&source);
        let line_height = self.line_height();
        let old_viewport = self.panes[pane].as_ref().map(|v| v.viewport_h);
        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        // Put the block cursor on the jump target (or the top of the file).
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = format!("{} — {} lines", v.rel, v.lines.len());
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        self.registry
            .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        // Point the Imports tab at the newly focused file.
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        // Start (or reuse) a language server for this file and open the doc.
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        let content = self.content_tasks(abs, source, lang_key);
        Task::batch([scroll, lsp_task, content])
    }

    /// Off-thread re-highlight + git-info tasks for a file's current source,
    /// shared by initial load and live refresh. Both deliver `Highlighted` /
    /// `GitInfoLoaded` keyed by `abs`, so they route to whatever pane shows it.
    fn content_tasks(
        &self,
        abs: PathBuf,
        source: Arc<String>,
        lang_key: Option<&'static str>,
    ) -> Task<Message> {
        let hl_abs = abs.clone();
        let hl_source = source.clone();
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&hl_source, lang_key);
                    let symbols = lang_key
                        .map(|key| outline::extract(&hl_source, key))
                        .unwrap_or_default();
                    (lines, symbols)
                })
                .await
                .unwrap_or_default()
            },
            move |(lines, symbols)| Message::Highlighted {
                abs: hl_abs.clone(),
                lines,
                symbols,
            },
        );

        let git_task = match self.project.as_ref().map(|p| p.root.clone()) {
            Some(root) => {
                let file = abs.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || git::info(&root, &file).map(Arc::new))
                            .await
                            .ok()
                            .flatten()
                    },
                    move |info| Message::GitInfoLoaded {
                        abs: abs.clone(),
                        info,
                    },
                )
            }
            None => Task::none(),
        };
        Task::batch([highlight_task, git_task])
    }

    /// Keep the top visible line stable across a line-height change.
    fn rescale_scroll(&mut self, old_line_height: f32) -> Task<Message> {
        let new_line_height = self.line_height();
        if (new_line_height - old_line_height).abs() < f32::EPSILON {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (pane, slot) in self.panes.iter_mut().enumerate() {
            if let Some(v) = slot {
                let first = v.scroll_y / old_line_height;
                v.scroll_y = first * new_line_height;
                tasks.push(operation::scroll_to(
                    ui::code_scroll_id(pane),
                    AbsoluteOffset {
                        x: 0.0,
                        y: v.scroll_y,
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    fn handle_key(&mut self, key: keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();
        match key.as_ref() {
            Key::Character(c) if cmd && !modifiers.shift() && c.eq_ignore_ascii_case("p") => {
                self.update(Message::FinderOpened(FinderMode::Files))
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("t") => {
                self.update(Message::FinderOpened(FinderMode::Symbols))
            }
            Key::Character(c) if cmd && modifiers.shift() && c.eq_ignore_ascii_case("f") => {
                self.update(Message::SidebarTabPicked(SidebarTab::Search))
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("f") && !modifiers.shift() => {
                self.update(Message::FindOpened)
            }
            Key::Character(c) if cmd && !self.finder.open && c.eq_ignore_ascii_case("c") => {
                self.update(Message::CopySelection)
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("d") => {
                self.update(Message::BookmarkToggled)
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("l") => {
                self.update(Message::GotoLineRequested)
            }
            Key::Character("\\") if cmd => self.update(Message::ToggleSplit),
            Key::Character("=") | Key::Character("+") if cmd => {
                self.update(Message::FontSizeDelta(1.0))
            }
            Key::Character("-") if cmd => self.update(Message::FontSizeDelta(-1.0)),
            Key::Character("0") if cmd => self.update(Message::FontSizeReset),
            // In-file find bar: Enter next, Shift+Enter prev.
            Key::Named(Named::Enter) if self.find.open => {
                self.update(Message::FindStep(if modifiers.shift() { -1 } else { 1 }))
            }
            Key::Named(Named::Escape) => {
                self.pending_g = false;
                self.pending_z = false;
                if self.context_menu.is_some() {
                    self.context_menu = None;
                    return Task::none();
                }
                if self.find.open {
                    return self.update(Message::FindClosed);
                }
                if self.finder.open {
                    return self.update(Message::FinderClosed);
                }
                if let Some(v) = self.active_viewer_mut() {
                    v.selection = None;
                    v.target_line = None;
                }
                Task::none()
            }
            Key::Named(Named::ArrowDown) if self.finder.open => {
                self.finder.move_selection(1);
                Task::none()
            }
            Key::Named(Named::ArrowUp) if self.finder.open => {
                self.finder.move_selection(-1);
                Task::none()
            }
            Key::Named(Named::ArrowLeft) if modifiers.alt() => self.update(Message::GoBack),
            Key::Named(Named::ArrowRight) if modifiers.alt() => self.update(Message::GoForward),
            // -------- Vim-style read-only cursor (only when the code view has
            // focus, so it never steals keys from a text input) --------
            _ if cmd
                || self.finder.open
                || self.context_menu.is_some()
                || !self.code_focused
                || self.active_viewer().is_none() =>
            {
                Task::none()
            }
            // Two-key `g` prefix: gg / gd / gr / gi / gy / gc.
            _ if self.pending_g => {
                self.pending_g = false;
                match key.as_ref() {
                    Key::Character("g") => self.move_cursor(viewer::Motion::FileStart),
                    Key::Character("d") => self.goto_at_cursor(GotoKind::Definition),
                    Key::Character("r") => self.goto_at_cursor(GotoKind::References),
                    Key::Character("i") => self.goto_at_cursor(GotoKind::Implementation),
                    Key::Character("y") => self.goto_at_cursor(GotoKind::TypeDefinition),
                    Key::Character("c") => self.update(Message::CallHierarchyRequested),
                    _ => Task::none(),
                }
            }
            Key::Character("g") => {
                self.pending_g = true;
                Task::none()
            }
            // Two-key `z` prefix for folding: za toggle, zR open all, zM close all.
            _ if self.pending_z => {
                self.pending_z = false;
                match key.as_ref() {
                    Key::Character("a") => self.fold_toggle_at_cursor(),
                    Key::Character("R") => self.fold_all(false),
                    Key::Character("M") => self.fold_all(true),
                    _ => {}
                }
                Task::none()
            }
            Key::Character("z") => {
                self.pending_z = true;
                Task::none()
            }
            Key::Character("h") | Key::Named(Named::ArrowLeft) => {
                self.move_cursor(viewer::Motion::Left)
            }
            Key::Character("l") | Key::Named(Named::ArrowRight) => {
                self.move_cursor(viewer::Motion::Right)
            }
            Key::Character("k") | Key::Named(Named::ArrowUp) => {
                self.move_cursor(viewer::Motion::Up)
            }
            Key::Character("j") | Key::Named(Named::ArrowDown) => {
                self.move_cursor(viewer::Motion::Down)
            }
            Key::Character("w") => self.move_cursor(viewer::Motion::WordForward),
            Key::Character("b") => self.move_cursor(viewer::Motion::WordBack),
            Key::Character("0") => self.move_cursor(viewer::Motion::LineStart),
            Key::Character("$") => self.move_cursor(viewer::Motion::LineEnd),
            Key::Character("G") => self.move_cursor(viewer::Motion::FileEnd),
            _ => Task::none(),
        }
    }

    /// Move the active pane's block cursor and scroll it into view.
    fn move_cursor(&mut self, motion: viewer::Motion) -> Task<Message> {
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.move_caret(motion);
        let (line, _) = v.caret.unwrap_or((0, 0));
        // Keep the cursor line within the viewport (in display rows, so folds
        // above it are accounted for).
        let top = v.row_of(line) as f32 * line_height;
        let bottom = top + line_height;
        if top < v.scroll_y {
            v.scroll_y = top;
        } else if bottom > v.scroll_y + v.viewport_h {
            v.scroll_y = bottom - v.viewport_h;
        }
        let y = v.scroll_y;
        operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y })
    }

    /// Toggle the fold enclosing the caret (`za`).
    fn fold_toggle_at_cursor(&mut self) {
        if let Some(v) = self.active_viewer_mut() {
            let line = v.caret.map(|(l, _)| l).unwrap_or(0);
            if let Some(header) = v.fold_header_for(line) {
                v.toggle_fold(header);
            }
        }
    }

    /// Collapse (`zM`) or expand (`zR`) every fold in the active pane.
    fn fold_all(&mut self, collapse: bool) {
        if let Some(v) = self.active_viewer_mut() {
            if collapse {
                v.collapse_all();
            } else {
                v.expand_all();
            }
        }
    }

    /// Move the cursor to the current find match and scroll it into view.
    fn jump_to_find_match(&mut self) -> Task<Message> {
        let Some((line, col, _)) = self.find.current_match() else {
            return Task::none();
        };
        let pane = self.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.caret = Some((line, col));
        // Center-ish the match line (display rows account for folds).
        let top = v.row_of(line) as f32 * line_height;
        if top < v.scroll_y || top + line_height > v.scroll_y + v.viewport_h {
            v.scroll_y = (top - v.viewport_h / 3.0).max(0.0);
        }
        let y = v.scroll_y;
        operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y })
    }

    /// Extra span highlights for the code view of `pane`: find matches, or
    /// (when not finding) the occurrences of the identifier under the cursor
    /// and the matching bracket.
    pub fn code_highlights(&self, pane: usize, v: &Viewer) -> Vec<codeview::Hl> {
        use codeview::{Hl, HlKind};
        let mut out = Vec::new();
        if pane != self.active {
            return out;
        }

        // Diagnostic underlines (always shown, from the LSP server).
        if let Some(lang) = v.lang_key
            && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
        {
            let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
            for d in client.diagnostics(&v.abs) {
                let raw = v.source_line(d.line).unwrap_or("");
                let c0 = viewer::display_col_from_char(raw, d.char_start, utf16);
                let c1 = viewer::display_col_from_char(raw, d.char_end, utf16).max(c0 + 1);
                out.push(Hl {
                    line: d.line,
                    col0: c0,
                    col1: c1,
                    kind: match d.severity {
                        1 => HlKind::DiagError,
                        2 => HlKind::DiagWarn,
                        _ => HlKind::DiagHint,
                    },
                });
            }
        }

        if self.find.open {
            for (i, &(line, col0, col1)) in self.find.matches.iter().enumerate() {
                out.push(Hl {
                    line,
                    col0,
                    col1,
                    kind: if i == self.find.current {
                        HlKind::FindCurrent
                    } else {
                        HlKind::FindMatch
                    },
                });
            }
            return out;
        }

        // Cursor-derived aids, only while reading (code has focus).
        if !self.code_focused {
            return out;
        }
        let Some((line, col)) = v.caret else {
            return out;
        };

        // Occurrences of the identifier under the cursor (2+ to be useful).
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            let occ = analyze::occurrences(&word, &v.lines, 500);
            if occ.len() > 1 {
                for (l, c0, c1) in occ {
                    out.push(Hl {
                        line: l,
                        col0: c0,
                        col1: c1,
                        kind: HlKind::Occurrence,
                    });
                }
            }
        }

        // Matching bracket pair.
        if let Some((ml, mc)) = analyze::matching_bracket(&v.lines, line, col) {
            out.push(Hl {
                line,
                col0: col,
                col1: col + 1,
                kind: HlKind::Bracket,
            });
            out.push(Hl {
                line: ml,
                col0: mc,
                col1: mc + 1,
                kind: HlKind::Bracket,
            });
        }
        out
    }

    /// Inline blame annotation for the caret line: `author, when · summary`.
    pub fn blame_annotation(&self, v: &Viewer) -> Option<(usize, String)> {
        let git = v.git.as_ref()?;
        let (line, _) = v.caret?;
        let b = git.blame_for(line)?;
        if b.commit.is_empty() {
            return None;
        }
        let text = if b.uncommitted {
            "· Uncommitted change".to_string()
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(b.time);
            let mut summary = b.summary.clone();
            if summary.chars().count() > 60 {
                summary = summary.chars().take(59).collect::<String>() + "…";
            }
            format!(
                "{}, {} · {}",
                b.author,
                git::relative_time(b.time, now),
                summary
            )
        };
        Some((line, text))
    }

    /// Sticky-scroll header lines for a viewer at its current scroll position.
    pub fn sticky_headers(&self, v: &Viewer) -> Vec<usize> {
        let row = (v.scroll_y / self.line_height()) as usize;
        let first_visible = v.line_at_row(row);
        analyze::sticky_headers(&v.lines, first_visible, 5)
    }

    fn rel_of(&self, abs: &Path) -> String {
        self.project
            .as_ref()
            .and_then(|p| abs.strip_prefix(&p.root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| abs.display().to_string())
    }
}

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Open a project folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}


async fn load_file(
    pane: usize,
    abs: PathBuf,
    target: Option<usize>,
) -> (usize, PathBuf, Option<usize>, Result<String, String>) {
    let read_path = abs.clone();
    let result = tokio::task::spawn_blocking(move || read_text_file(&read_path))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    (pane, abs, target, result)
}

fn read_text_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file too large ({:.1} MB, limit {} MB)",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod app_tests {
    use super::*;

    /// Each test gets its own directory: tests run in parallel and would
    /// otherwise race on remove_dir_all/create of a shared fixture.
    fn fixture_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clew-app-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "needle in notes\n").unwrap();
        dir.canonicalize().unwrap()
    }

    /// Drive the update loop the way the runtime would, executing the
    /// blocking parts inline instead of through iced Tasks.
    fn open_synchronously(app: &mut App, rel: &str, line: Option<usize>) {
        let abs = app.project.as_ref().unwrap().root.join(rel);
        let pane = app.active;
        let _ = app.update(Message::OpenRel {
            rel: rel.to_string(),
            line,
        });
        let content = read_text_file(&abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane,
            abs: abs.clone(),
            target: line,
            result: Ok(content.clone()),
        });
        let lang = highlight::detect(&abs);
        let lines = highlight::highlight_lines(&content, lang);
        let symbols = lang
            .map(|k| outline::extract(&content, k))
            .unwrap_or_default();
        let _ = app.update(Message::Highlighted {
            abs,
            lines,
            symbols,
        });
    }

    fn scanned_app(tag: &str) -> App {
        let root = fixture_project(tag);
        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        app
    }

    #[test]
    fn full_reading_flow() {
        let mut app = scanned_app("reading");
        assert!(app.project.is_some());
        assert_eq!(app.project.as_ref().unwrap().files.len(), 2);

        // Open a file at a line.
        open_synchronously(&mut app, "src/lib.rs", Some(3));
        let v = app.active_viewer().unwrap();
        assert_eq!(v.rel, "src/lib.rs");
        assert!(v.highlighted);
        assert_eq!(v.target_line, Some(3));
        assert_eq!(v.lines.len(), 5);

        // Outline extracted for the current file.
        let names: Vec<&str> = v.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"origin"), "outline: {names:?}");

        // Open a second file, then navigate back and forward.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");
        assert!(app.history.can_back());

        let back = app.history.back().unwrap();
        assert!(back.path.ends_with("src/lib.rs"));
        assert_eq!(back.line, Some(3));
        let fwd = app.history.forward().unwrap();
        assert!(fwd.path.ends_with("notes.txt"));
    }

    #[test]
    fn finder_flow() {
        let mut app = scanned_app("finder");

        let _ = app.update(Message::FinderOpened(FinderMode::Files));
        assert!(app.finder.open);
        assert!(!app.finder.results.is_empty());

        let _ = app.update(Message::FinderQueryChanged("librs".to_string()));
        let files = app.project.as_ref().unwrap().files.clone();
        let top = files[app.finder.results[0]].rel.clone();
        assert_eq!(top, "src/lib.rs");

        // Confirm closes the finder.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
    }

    #[test]
    fn incremental_reindex_on_change_and_delete() {
        let mut app = scanned_app("reindex");
        let files = app.project.as_ref().unwrap().files.clone();
        let root = app.project.as_ref().unwrap().root.clone();
        let _ = app.update(Message::SymbolIndexDone {
            root,
            indexed: index::build_indexed(files),
        });
        let abs = app.project.as_ref().unwrap().root.join("src/lib.rs");
        assert!(app.symbol_index.iter().any(|e| e.name == "origin"));
        assert!(app.registry.version(&abs).is_some());

        // An external edit that renames the function re-indexes just that file.
        let new = std::sync::Arc::new("pub fn renamed() -> u8 {\n    1\n}\n".to_string());
        let ev = watch::FileEvent::Modified(watch::Changed {
            path: abs.clone(),
            hash: 424242,
            content: new,
        });
        let _ = app.update(Message::FilesRehashed(vec![ev]));
        assert!(app.symbol_index.iter().any(|e| e.name == "renamed"));
        assert!(!app.symbol_index.iter().any(|e| e.name == "origin"));
        assert_eq!(app.registry.version(&abs), Some(424242));

        // Deleting the file drops its symbols and forgets its version.
        let _ = app.update(Message::FilesRehashed(vec![watch::FileEvent::Deleted(abs.clone())]));
        assert!(!app.symbol_index.iter().any(|e| e.name == "renamed"));
        assert_eq!(app.registry.version(&abs), None);
        assert!(!app.symbol_index_by_file.contains_key(&abs));
    }

    #[test]
    fn tree_update_swaps_files_and_ignores_stale_root() {
        let mut app = scanned_app("tree");
        let root = app.project.as_ref().unwrap().root.clone();
        let before = app.project.as_ref().unwrap().files.len();

        // A new file on disk, applied via a rescan result, grows the file list
        // without a full project reopen.
        std::fs::write(root.join("src/newmod.rs"), "pub fn brand_new() {}\n").unwrap();
        let _ = app.update(Message::TreeUpdated(fs_scan::scan(root.clone())));
        let after = app.project.as_ref().unwrap().files.len();
        assert_eq!(after, before + 1);
        assert!(
            app.project
                .as_ref()
                .unwrap()
                .files
                .iter()
                .any(|f| f.rel.ends_with("newmod.rs"))
        );

        // A rescan for a different root (a stale one) is ignored.
        let stale = fs_scan::ScanResult {
            root: PathBuf::from("/definitely/not/this/project"),
            tree: fs_scan::DirNode::default(),
            files: Vec::new(),
            truncated: false,
        };
        let _ = app.update(Message::TreeUpdated(stale));
        assert_eq!(app.project.as_ref().unwrap().files.len(), after);
    }

    #[test]
    fn symbol_finder_flow() {
        let mut app = scanned_app("symbols");
        // Build the index synchronously (the runtime does this in a task).
        let files = app.project.as_ref().unwrap().files.clone();
        let root = app.project.as_ref().unwrap().root.clone();
        let _ = app.update(Message::SymbolIndexDone {
            root,
            indexed: index::build_indexed(files),
        });
        assert!(!app.indexing);
        assert!(app.symbol_index.len() >= 2, "{:?}", app.symbol_index);

        let _ = app.update(Message::FinderOpened(FinderMode::Symbols));
        let _ = app.update(Message::FinderQueryChanged("origin".to_string()));
        assert!(!app.finder.results.is_empty());
        let entry = &app.symbol_index[app.finder.results[0]];
        assert_eq!(entry.name, "origin");
        assert_eq!(entry.line, 3);

        // Confirm records the jump in history.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        let _ = app.update(Message::GoBack); // no-op or previous loc; must not panic
    }

    #[test]
    fn goto_line_via_finder() {
        let mut app = scanned_app("goto");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::GotoLineRequested);
        assert!(app.finder.open);
        let _ = app.update(Message::FinderQueryChanged(":4".to_string()));
        assert_eq!(app.finder.goto_line(), Some(4));
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        assert_eq!(app.active_viewer().unwrap().target_line, Some(4));
    }

    #[test]
    fn split_view_routes_to_active_pane() {
        let mut app = scanned_app("split");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::ToggleSplit);
        assert!(app.split);
        assert_eq!(app.active, 1);
        // Split duplicates the current file.
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "src/lib.rs");

        // Opening now targets pane 1; pane 0 keeps its file.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "notes.txt");
        assert_eq!(app.panes[0].as_ref().unwrap().rel, "src/lib.rs");

        // Refocus pane 0 and close the split.
        let _ = app.update(Message::PaneFocused(0));
        assert_eq!(app.active, 0);
        let _ = app.update(Message::ToggleSplit);
        assert!(!app.split);
        assert!(app.panes[1].is_none());
    }

    #[test]
    fn selection_and_copy_state() {
        let mut app = scanned_app("select");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::SelectStart { pane: 0, line: 1, col: 4 });
        assert!(app.selecting);
        assert_eq!(app.active_viewer().unwrap().caret, Some((1, 4)));
        let _ = app.update(Message::SelectDrag { pane: 0, line: 3, col: 2 });
        let _ = app.update(Message::SelectEnd);
        assert!(!app.selecting);

        let v = app.active_viewer().unwrap();
        assert_eq!(v.selection_ordered(), Some(((1, 4), (3, 2))));
        assert_eq!(v.caret, Some((3, 2)));
        let text = v.selected_text().unwrap();
        assert!(text.contains("origin"), "{text}");

        // Esc clears the selection.
        let _ = app.update(Message::KeyPressed(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::Modifiers::default(),
        ));
        assert!(app.active_viewer().unwrap().selection.is_none());
    }

    #[test]
    fn bookmark_toggle_persists_in_project() {
        let mut app = scanned_app("bookmark");
        let root = app.project.as_ref().unwrap().root.clone();
        open_synchronously(&mut app, "src/lib.rs", Some(3));

        let _ = app.update(Message::BookmarkToggled);
        assert_eq!(app.bookmarks.len(), 1);
        assert_eq!(app.bookmarks[0].rel, "src/lib.rs");
        assert_eq!(app.bookmarks[0].line, 3);
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(bookmarks::load(&root), app.bookmarks);

        // Toggling again removes it and cleans up the store file; the .clew
        // directory itself stays (consent record).
        let _ = app.update(Message::BookmarkToggled);
        assert!(app.bookmarks.is_empty());
        assert!(!root.join(".clew/bookmarks.json").exists());
    }

    #[test]
    fn consent_gates_project_open() {
        let root = fixture_project("consent");

        // Picking a folder without .clew opens the consent modal, not the project.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        assert_eq!(app.pending_consent.as_deref(), Some(root.as_path()));
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());

        // Denied: nothing is created, no project opens, modal dismissed.
        let _ = app.update(Message::ConsentDenied);
        assert!(app.pending_consent.is_none());
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());
        assert!(app.status.contains("not allowed"), "{}", app.status);

        // Allowed: .clew is created and the scan starts.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        let _ = app.update(Message::ConsentAllowed);
        assert!(root.join(".clew").is_dir());
        assert!(app.scanning);
        assert!(app.pending_consent.is_none());

        // A project with .clew already present skips the consent modal:
        // FolderPicked goes straight to scanning.
        let mut app2 = App::blank();
        let _ = app2.update(Message::FolderPicked(Some(root.clone())));
        assert!(app2.scanning, "existing .clew must skip the prompt");
        assert!(app2.pending_consent.is_none());
    }

    #[test]
    fn search_flow_message_wiring() {
        let mut app = scanned_app("search");

        let files = app.project.as_ref().unwrap().files.clone();
        let result = search::search(
            files,
            search::SearchOptions {
                query: "needle".to_string(),
                ..Default::default()
            },
        );
        assert_eq!(result.hits.len(), 1);
        let _ = app.update(Message::SearchDone { result });
        assert_eq!(app.search.hits.len(), 1);
        assert_eq!(app.search.hits[0].rel, "notes.txt");

        // Clicking a hit opens the file at that line.
        let hit = app.search.hits[0].clone();
        let _ = app.update(Message::OpenAbs {
            abs: hit.abs.clone(),
            line: Some(hit.line),
            push: true,
        });
        let content = read_text_file(&hit.abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: hit.abs,
            target: Some(hit.line),
            result: Ok(content),
        });
        assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
    }

    #[test]
    fn font_size_rescales_scroll() {
        let mut app = scanned_app("font");
        open_synchronously(&mut app, "src/lib.rs", None);
        app.panes[0].as_mut().unwrap().scroll_y = 40.0; // line 2 at 20px
        let _ = app.update(Message::FontSizeDelta(2.0));
        assert_eq!(app.font_size, 15.0);
        let v = app.active_viewer().unwrap();
        assert!((v.scroll_y - 44.0).abs() < 0.01, "{}", v.scroll_y); // 2 * 22px
        let _ = app.update(Message::FontSizeReset);
        assert_eq!(app.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn binary_and_oversized_files_are_rejected() {
        let dir = std::env::temp_dir().join("clew-guard-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        assert!(read_text_file(&bin).unwrap_err().contains("binary"));

        let big = dir.join("huge.txt");
        std::fs::write(&big, vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(read_text_file(&big).unwrap_err().contains("too large"));
    }

    // ---------------------------------------------------------------- LSP

    /// Opening a Rust file with no installed server and no override prompts
    /// for a download; dismissing marks the language unsupported (falls back
    /// to ⌘T).
    #[test]
    fn opening_rust_prompts_for_server_download() {
        // Point the store at a guaranteed-empty dir so nothing is "installed".
        let store = std::env::temp_dir().join("clew-lsp-empty-store");
        let _ = std::fs::remove_dir_all(&store);
        // SAFETY: test-only env mutation.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &store) };

        let mut app = scanned_app("lsp-prompt");
        open_synchronously(&mut app, "src/lib.rs", None);

        let consent = app.pending_lsp_consent.as_ref().expect("download prompt");
        assert_eq!(consent.server_name, "rust-analyzer");
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::AwaitingConsent)));

        let _ = app.update(Message::LspConsentDismissed);
        assert!(app.pending_lsp_consent.is_none());
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Unsupported(_))));

        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
    }

    /// The server panel lists only project-relevant languages — a Rust
    /// project does not show c/cpp.
    #[test]
    fn managed_languages_are_project_relevant() {
        let app = scanned_app("lsp-langs"); // fixture has src/lib.rs (Rust) + notes.txt
        assert_eq!(app.managed_languages(), vec!["rust".to_string()]);
        // notes.txt has no server; c/cpp are not in the project.
        assert!(!app.managed_languages().iter().any(|l| l == "cpp"));
    }

    /// Right-click opens a navigation menu carrying the clicked position;
    /// choosing an action closes it.
    #[test]
    fn context_menu_flow() {
        let mut app = scanned_app("ctxmenu");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::ContextMenuOpened {
            pane: 0,
            line: 2,
            col: 7,
            x: 120.0,
            y: 40.0,
        });
        let menu = app.context_menu.expect("menu open");
        assert_eq!((menu.line, menu.col), (2, 7));

        // Choosing an action closes the menu (and dispatches a goto).
        let _ = app.update(Message::ContextGoto(GotoKind::Definition));
        assert!(app.context_menu.is_none());

        // Outside click / Esc closes without acting.
        let _ = app.update(Message::ContextMenuOpened {
            pane: 0,
            line: 0,
            col: 0,
            x: 0.0,
            y: 0.0,
        });
        let _ = app.update(Message::ContextMenuClosed);
        assert!(app.context_menu.is_none());
    }

    /// A Go project surfaces the gopls row (toolchain-installed server).
    #[test]
    fn go_project_is_served_by_gopls() {
        let dir = std::env::temp_dir().join("clew-go-proj");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.go"), "package main\nfunc main() {}\n").unwrap();
        let root = dir.canonicalize().unwrap();

        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        assert!(app.managed_languages().contains(&"go".to_string()));
        assert_eq!(
            lsp::registry::default_for_language("go").unwrap().name,
            "gopls"
        );
    }

    /// A custom `command` in `.clew/lsp.toml` bypasses the store and starts
    /// directly — no download prompt.
    #[test]
    fn custom_command_starts_without_prompt() {
        let root = fixture_project("lsp-escape");
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join(".clew/lsp.toml"),
            "[rust]\ncommand = \"/nonexistent/rust-analyzer\"\n",
        )
        .unwrap();
        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        open_synchronously(&mut app, "src/lib.rs", None);

        assert!(app.pending_lsp_consent.is_none());
        assert!(matches!(app.lsp.get("rust"), Some(LspSlot::Starting)));
    }

    /// A definition result jumps to the target line and records history.
    #[test]
    fn definition_result_jumps_and_records_history() {
        let mut app = scanned_app("lsp-jump");
        open_synchronously(&mut app, "notes.txt", None);
        let target = app.project.as_ref().unwrap().root.join("src/lib.rs");

        let _ = app.update(Message::DefinitionResult {
            result: Ok(vec![lsp::client::Target {
                path: target.clone(),
                line: 2, // 0-based → jump to line 3
                character: 7,
            }]),
        });
        // open_file kicked off an async load; feed the FileLoaded it awaits.
        let content = read_text_file(&target).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: target,
            target: Some(3),
            result: Ok(content),
        });
        assert_eq!(app.active_viewer().unwrap().rel, "src/lib.rs");
        // The cursor moves to the jump target (line 3 → 0-based line 2).
        assert_eq!(app.active_viewer().unwrap().caret, Some((2, 0)));
        assert!(app.history.can_back(), "definition jump is undoable");
    }

    /// Full chain against a real rust-analyzer via the escape hatch: scan →
    /// open → start server → didOpen → definition → jump. Ignored by default
    /// (spawns rust-analyzer); run explicitly.
    #[tokio::test]
    #[ignore]
    async fn live_goto_definition_through_app() {
        let ra = PathBuf::from(std::env::var("HOME").unwrap()).join(".cargo/bin/rust-analyzer");
        assert!(ra.exists(), "needs rust-analyzer at {ra:?}");

        // Cargo project with origin() defined and called.
        let root = std::env::temp_dir().join("clew-app-live-lsp");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".clew")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".clew/lsp.toml"),
            format!("[rust]\ncommand = {:?}\n", ra.to_string_lossy()),
        )
        .unwrap();
        let src = "fn origin() -> i32 {\n    0\n}\n\nfn main() {\n    let _ = origin();\n}\n";
        std::fs::write(root.join("src/main.rs"), src).unwrap();
        let root = root.canonicalize().unwrap();

        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root.clone())));
        open_synchronously(&mut app, "src/main.rs", None);

        // Start the real server (the escape hatch resolved it) and register it.
        let server = app.lsp_config.resolve("rust").unwrap();
        let client = lsp::client::LspClient::start(&server.command.unwrap(), &[], &root, None)
            .await
            .unwrap();
        let _ = app.update(Message::LspStartResult {
            language: "rust".into(),
            result: Ok(client.clone()),
        });

        // Simulate ⌘-click on the `origin()` call (line 5, inside the name).
        let v = app.active_viewer().unwrap();
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let ch = viewer::character_offset(v.source_line(5).unwrap(), 12, utf16);
        let path = v.abs.clone();

        // Poll until rust-analyzer has indexed.
        let mut targets = Vec::new();
        for _ in 0..40 {
            targets = client.definition(&path, 5, ch).await.unwrap_or_default();
            if !targets.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        assert!(!targets.is_empty(), "expected a definition");

        // Feed the result through the app and complete the jump.
        let _ = app.update(Message::DefinitionResult {
            result: Ok(targets.clone()),
        });
        let content = read_text_file(&targets[0].path).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: targets[0].path.clone(),
            target: Some(targets[0].line + 1),
            result: Ok(content),
        });
        // Jumped to the `origin` definition on line 1 (1-based).
        assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
        assert!(app.history.can_back());
    }
}
