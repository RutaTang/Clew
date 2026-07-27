//! The App model: the central state struct and its per-domain state sub-structs.

use crate::app::prelude::*;
use crate::*;

/// The debugger (DAP client): the active session, plus the breakpoints and
/// watch expressions that persist independently of any running session.
#[derive(Default)]
pub struct DebugState {
    /// The active debug session (DAP), if any.
    pub session: Option<DebugSession>,
    /// Watch expressions (persist across stops/sessions).
    pub watches: Vec<String>,
    /// The add-watch input box.
    pub watch_input: String,
    /// Editing a breakpoint condition: (file, 1-based line, draft expression).
    pub bp_cond_edit: Option<(PathBuf, usize, String)>,
    /// The last function the debugger stopped in — so entering a NEW function
    /// records one reading-trail entry (not one per line step).
    pub last_fn: Option<String>,
    /// Breakpoints per file (absolute path → 1-based line → breakpoint),
    /// independent of a running session so they can be set before and persist
    /// across runs.
    pub breakpoints: HashMap<PathBuf, std::collections::BTreeMap<usize, Bp>>,
}

/// The whole-project symbol call graph (tree-sitter name-resolved, optionally
/// LSP-refined to exact edges) plus its build / incremental-refine state.
#[derive(Default)]
pub struct ProjectCallsState {
    /// Whole-project symbol call graph (tree-sitter, name-resolved), built lazily
    /// when its overlay opens; drives the project call-graph overlay.
    pub graph: projectcalls::ProjectCallGraph,
    /// Registry revision the graph was last built at (to rebuild it only when
    /// files actually changed since).
    pub rev: u64,
    /// True while the graph is being (re)built off-thread.
    pub building: bool,
    /// True when `graph` is the exact LSP-resolved graph rather than the
    /// tree-sitter name-based approximation.
    pub precise: bool,
    /// Generation counter for LSP-refine runs, so a late result from a superseded
    /// run (new project, re-refine, or a rebuild) is dropped.
    pub generation: u64,
    /// LSP-refine progress `(done, total)` while a refine is running.
    pub refine_progress: Option<(usize, usize)>,
    /// The precise edge set, symbol-keyed, kept while `precise` so a file change
    /// can patch only the affected functions.
    pub precise_edges: projectcalls::SymEdges,
    /// Source files changed since the last precise update, awaiting an
    /// incremental refine (coalesced when one is already running).
    pub precise_pending: HashSet<PathBuf>,
}

/// The architecture-overview "home": the generated prose, its native module
/// map, and the generation/freshness bookkeeping.
#[derive(Default)]
pub struct OverviewState {
    /// The generated architecture overview — RAW LLM markdown, no module map.
    /// The module diagram is injected fresh at prepare time from the current
    /// import graph (never baked into the cache), so it can't go stale.
    pub markdown: Option<String>,
    /// The module map, drawn natively on a canvas in the overview home (like the
    /// Import Graph overlay) — laid out from the current import graph, not baked
    /// into the prose or a mermaid diagram.
    pub map: Option<graphlayout::Layout>,
    /// The overview prepared for display (markdown + math/mermaid SVG segments).
    pub prepared: Vec<PreparedSeg>,
    /// True while the overview is being generated.
    pub generating: bool,
    /// True when the main area shows the overview "home" (vs. code / empty).
    pub showing: bool,
    /// Prompt hash of the cached overview, so a re-explain regenerates it only
    /// when its inputs actually changed (avoids a needless overview LLM call).
    pub prompt_hash: Option<incremental::Version>,
}

/// The Stats "home": the per-language code statistics and its freshness.
pub struct StatsState {
    /// Code statistics (lines by language) shown in the Stats full-pane view.
    pub report: Option<stats::StatsReport>,
    /// True when the main area shows the Stats "home" (vs. code / overview).
    pub showing: bool,
    /// True while a stats computation is running (single-flight guard).
    pub building: bool,
    /// Registry revision the stats were last computed at; a newer revision
    /// (a created / deleted / edited file) marks them stale. `u64::MAX` on
    /// project load forces one background refresh over the warm disk cache.
    pub rev: u64,
}

impl Default for StatsState {
    fn default() -> Self {
        Self {
            report: None,
            showing: false,
            building: false,
            rev: u64::MAX,
        }
    }
}

/// The Walkthroughs feature: the per-project library of saved tours plus the
/// state that drives reading one and composing a new one in the WALK tab.
pub struct WalkState {
    /// The per-project library of saved walkthroughs (persisted with the project).
    pub library: Vec<walkthrough::Walkthrough>,
    /// Index into `library` of the tour being read, or `None` while browsing the
    /// library list.
    pub open: Option<usize>,
    pub step: usize,
    /// The scope currently being (re)generated, or `None` when idle. Lets the UI
    /// mark just that one row as busy while the rest of the library stays usable.
    pub generating: Option<String>,
    /// True while a walkthrough generation is on its one automatic retry (the LLM
    /// occasionally emits malformed JSON); prevents an endless retry loop.
    pub retried: bool,
    /// The shared top input: a search query in `Search` mode, a scope prompt in
    /// `Walk` mode.
    pub input: String,
    /// Whether the top input searches the library or generates a new tour.
    pub mode: WalkMode,
    /// The current step's narration, prepared for rich display (markdown, plus
    /// mermaid diagrams and math rendered as inline SVGs — same pipeline as the
    /// overview and explanations).
    pub prepared: Vec<PreparedSeg>,
    /// Height of the narration block in the WALK tab; the steps list above it
    /// takes the rest. The divider between them is draggable.
    pub narration_height: f32,
}

impl Default for WalkState {
    fn default() -> Self {
        Self {
            library: Vec::new(),
            open: None,
            step: 0,
            generating: None,
            retried: false,
            input: String::new(),
            mode: WalkMode::Search,
            prepared: Vec::new(),
            narration_height: 240.0,
        }
    }
}

/// The Explain feature's state: the incremental explanation cache plus the
/// currently-open explanation overlay and its render artifacts.
#[derive(Default)]
pub struct ExplainState {
    /// LLM explanations keyed by function/file/folder, kept fresh incrementally.
    pub cache: explain::Cache,
    /// True while the explain pass is running.
    pub running: bool,
    /// Explain progress `(done, total)` while a pass runs.
    pub progress: Option<(usize, usize)>,
    /// How many attempts in the current pass have errored (surfaced in the UI so
    /// a failing pass doesn't masquerade as success).
    pub failed: usize,
    /// Generation for explain passes, so a superseded result is dropped.
    pub generation: u64,
    /// Abort handle for the running explain pass, so a long project pass can be
    /// cancelled (the bottom-up pass over a big repo is thousands of LLM calls).
    pub abort: Option<iced::task::Handle>,
    /// The file/folder whose explanation overlay is open (Cmd+click a tree node).
    pub view: Option<explain::Node>,
    /// The open explanation's content, prepared as ordered segments (markdown
    /// pre-parsed; math/mermaid keyed to rendered SVGs) — either the node's
    /// summary or a function's block detail (see `showing_detail`).
    pub prepared: Vec<PreparedSeg>,
    /// Rendered math/mermaid SVGs, keyed by content hash — a session cache shared
    /// across every explanation, backed by `.clew/cache/svg/` on disk.
    pub svgs: HashMap<u64, ExplainSvg>,
    /// Generation for async SVG passes, so a superseded batch is dropped.
    pub svg_gen: u64,
    /// True when the overlay is showing a function's per-block detail rather than
    /// its summary (toggled by the `Explain blocks` / `Summary` button).
    pub showing_detail: bool,
}

/// The LLM settings modal: whether it's open, plus its draft chat / embedding
/// endpoint fields. Defaults to a closed modal with the Anthropic provider.
pub struct SettingsDraft {
    pub open: bool,
    pub provider: llm::Provider,
    pub key: String,
    pub model: String,
    pub base_url: String,
    pub embed_key: String,
    pub embed_model: String,
    pub embed_base_url: String,
}

impl Default for SettingsDraft {
    fn default() -> Self {
        Self {
            open: false,
            provider: llm::Provider::Anthropic,
            key: String::new(),
            model: String::new(),
            base_url: String::new(),
            embed_key: String::new(),
            embed_model: String::new(),
            embed_base_url: String::new(),
        }
    }
}

/// State for the DOCS tab — the project's API documentation view.
#[derive(Default)]
pub struct DocsState {
    /// The project's API documentation, per file (from the server's `BuildDocs`).
    pub files: Vec<clew_protocol::DocFile>,
    /// A `BuildDocs` is in flight.
    pub loading: bool,
    /// Which files are expanded in the DOCS tree (keys are file rels).
    pub expanded: HashSet<String>,
    /// Filter text for the DOCS tree (matches item names).
    pub filter: String,
    /// Show all symbols vs. only the public API surface (default: public only).
    pub show_all: bool,
    /// Group the Docs tree by module/package instead of by file (default: file).
    pub by_module: bool,
    /// The doc page rendered in the main pane, with the selected item's doc
    /// markdown pre-parsed (the markdown widget borrows it). `None` = no page.
    pub page: Option<DocPage>,
    /// A symbol name whose doc page to open once the index finishes building
    /// (set by "View docs" when the docs aren't built yet).
    pub pending_view: Option<String>,
}

/// Auto-update: what the background check found and how far an in-progress
/// download / install has got. Runtime-only, except `auto_check`, which mirrors
/// the persisted `config.toml` preference.
pub struct UpdateState {
    /// A newer release the user has been told about (drives the banner and the
    /// release-notes modal). `None` until a check finds one.
    pub available: Option<AvailableUpdate>,
    /// The release-notes modal is open.
    pub show_notes: bool,
    /// The stage an in-progress update is at.
    pub phase: UpdatePhase,
    /// Download progress: (bytes so far, total if the server sent a length).
    pub progress: Option<(u64, Option<u64>)>,
    /// Bumped per download so a superseded run's late messages are dropped.
    pub generation: u64,
    /// A manual "Check for Updates" is running, so its result is announced even
    /// when already up to date.
    pub checking: bool,
    /// Whether clew checks for updates automatically at startup (persisted).
    pub auto_check: bool,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            available: None,
            show_notes: false,
            phase: UpdatePhase::Idle,
            progress: None,
            generation: 0,
            checking: false,
            auto_check: true,
        }
    }
}

/// A newer release, ready to present and install.
pub struct AvailableUpdate {
    pub version: clew_core::update::Version,
    /// The DMG download URL, if the release attached one (absent → manual only).
    pub dmg_url: Option<String>,
    /// Release notes, parsed once for the notes modal.
    pub notes: Vec<iced::widget::markdown::Item>,
}

/// The stage an in-progress update is at, so the UI can label it and lock the
/// action button. `Installing` covers verifying the download, swapping the
/// bundle, and launching the relauncher.
#[derive(Default, Clone, PartialEq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Downloading,
    Installing,
    Failed(String),
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
    /// The whole-project symbol call graph and its LSP-refine state (see
    /// [`ProjectCallsState`]).
    pub project_calls: ProjectCallsState,
    /// The active project-graph modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// The Explain feature's state — the explanation cache and the open
    /// explanation overlay (see [`ExplainState`]).
    pub explain: ExplainState,
    /// The architecture-overview "home" view's state (see [`OverviewState`]).
    pub overview: OverviewState,
    /// The Walkthroughs feature — the saved-tour library and the reader/composer
    /// state for the WALK tab (see [`WalkState`]).
    pub walk: WalkState,
    /// The Stats "home" view's state (see [`StatsState`]).
    pub stats: StatsState,
    /// Request channel to the in-process clew-server, once it has connected.
    /// The client sends `clew-protocol` requests here and receives events back
    /// as `Message::ServerEvent` (see `server`). The client/server split is
    /// grown one flow at a time onto this seam.
    pub server_tx: Option<tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>>,
    /// Where code is read from — local, or a remote host over SSH. This keys the
    /// server subscription: changing it restarts the transport against the new
    /// target, which is how an in-app Connect switches between local and remote.
    pub connection: connect::ConnTarget,
    /// Remembered SSH hosts, shown in the Connect modal (from `connections.toml`).
    pub saved_connections: Vec<connect::SavedConnection>,
    /// The Connect modal's state (closed, editing a host, browsing a remote's
    /// folders). `None` when the modal is closed.
    pub connect: Option<ConnectUi>,
    // -- Docs (API documentation view) --------------------------------------
    /// The API documentation view's state (see [`DocsState`]).
    pub docs: DocsState,
    /// In-flight streaming chat answers: stream id -> the channel feeding the Ask
    /// flow. `ChatDelta` / `ChatStreamDone` notifications are routed here.
    #[allow(clippy::type_complexity)]
    pub chat_streams: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<ChatStreamPiece>>,
        >,
    >,
    /// Next request id for server calls that need a correlated reply.
    pub next_req_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// In-flight AI RPCs: request id -> the caller awaiting its reply. Shared so
    /// background AI tasks can register/await while `update` resolves them.
    #[allow(clippy::type_complexity)]
    pub ai_pending: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                u64,
                tokio::sync::oneshot::Sender<Result<clew_protocol::Event, String>>,
            >,
        >,
    >,
    /// In-flight `ReadFile` requests: id -> why it was asked, so the reply is
    /// applied correctly (open-and-jump vs reload-in-place).
    pub pending_reads: std::collections::HashMap<u64, ReadKind>,
    /// Root of an in-flight server `OpenProject`, so its `Tree` reply can build
    /// the project (abs paths resolve against it).
    pub pending_scan_root: Option<PathBuf>,
    /// Next handle for a server-spawned process (language server / debug adapter).
    pub next_proc_id: u64,
    /// proc handle -> the channel that feeds `ProcessOutput` bytes into the
    /// matching LspClient's stdout bridge.
    pub proc_feeds: std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// language -> its live server-spawned proc handle, so a restart can kill the
    /// old process before starting a new one.
    pub lsp_procs: std::collections::HashMap<String, u64>,
    /// Semantic search: the embedding index over explanation summaries.
    pub embed_index: embed::Index,
    /// Whether an embedding endpoint is configured.
    pub embed_available: bool,
    /// True while the embedding index is being (re)built.
    pub building_embeddings: bool,
    /// The Semantic-tab query box and its ranked results.
    pub semantic_query: String,
    pub semantic_results: Vec<(explain::Node, f32)>,
    /// True while a semantic query is being embedded/searched.
    pub searching_semantic: bool,
    /// The collapsible bottom panel: whether it's shown, and which of its two
    /// tabs (Ask / Debug) is active.
    pub show_bottom: bool,
    pub bottom_tab: BottomTab,
    /// "Ask clew" Q&A: input, conversation, state.
    pub ask_input: String,
    pub ask_turns: Vec<AskTurn>,
    pub asking: bool,
    /// Code selections pinned as context, shown as chips above the input. They
    /// persist across turns until removed.
    pub ask_pins: Vec<AskPin>,
    /// The debugger (DAP): the active session plus breakpoints and watches that
    /// persist across sessions (see [`DebugState`]).
    pub debug: DebugState,
    /// Editing a bookmark note: (rel path, 1-based line, draft note text).
    pub note_edit: Option<(String, usize, String)>,
    /// Per-project reading notes / progress, anchored by (rel, symbol name).
    pub notes: Vec<notes::Note>,
    /// Editing a reading note: (rel path, symbol name, draft note text).
    pub reading_note_edit: Option<(String, String, String)>,
    /// Auto-refresh throttle: when the last refresh pass began (`None` until the
    /// first). A watched-file change starts a pass only once the cooldown has
    /// lifted; a manual refresh ignores it. Runtime-only (not persisted).
    pub last_auto_refresh: Option<std::time::Instant>,
    /// A source file changed during the cooldown — refresh when the window lifts
    /// (picked up by `Tick`), so no change is dropped.
    pub refresh_pending: bool,
    /// Whether an LLM key is configured (gates the explain UI). Checked at
    /// startup / project open, not per frame.
    pub llm_available: bool,
    /// Whether the toolbar's "More" overflow menu is open.
    pub show_tools_menu: bool,
    /// The interactive tutorial: the current step index while a tour is running,
    /// `None` when idle (see `crate::app::tutorial`).
    pub tutorial: Option<usize>,
    /// The status-bar `#[cfg]` target dropdown is open.
    pub show_target_menu: bool,
    /// The customizable command keymap (loaded from the global config).
    pub keymap: keymap::Keymap,
    /// Whether the "Keyboard Shortcuts" modal is open.
    pub show_shortcuts: bool,
    /// The action currently awaiting a new chord (capture mode), if any.
    pub rebinding: Option<keymap::Action>,
    /// Transient message in the shortcuts modal (conflict / invalid key).
    pub keymap_notice: Option<String>,
    /// Show each function's one-line summary inline past its signature.
    pub show_inline_summaries: bool,
    /// Show a one-line "what is this file" banner at the top of the code view.
    pub show_file_banner: bool,
    /// Show LSP inlay hints (inferred types, parameter names) inline.
    pub show_inlay_hints: bool,
    /// Target the `#[cfg]` dimming is evaluated against (host, or one the reader
    /// picks to study another platform's branches). Persisted per project.
    pub reading_target: inactive::Target,
    /// Show the code minimap on the right edge of the editor.
    pub show_minimap: bool,
    /// The LLM settings modal: whether it's open and its draft fields (see
    /// [`SettingsDraft`]).
    pub settings: SettingsDraft,
    /// Light/Dark/System appearance preference (persisted; drives the palette).
    pub theme_pref: theme::ThemePref,
    /// Auto-update state: the available release plus any in-progress download /
    /// install (see [`UpdateState`]).
    pub update: UpdateState,
    /// Overlay view: `true` shows the node-link map, `false` the list.
    pub graph_mode: bool,
    /// Map projection: `true` renders the force graph in 3D (orbit + depth),
    /// `false` flattens it to a plain 2D plane. Applies to every graph map.
    pub graph_3d: bool,
    /// Whether the 3D map auto-spins (idle rotation). Toggled from the map header.
    pub graph_spin: bool,
    /// Precomputed force-directed layout for the current overlay's map.
    pub graph_layout: Option<graphlayout::Layout>,
    pub expanded: HashSet<String>,
    /// Code panes; pane 1 exists only in split view.
    pub panes: [Option<Viewer>; 2],
    pub split: bool,
    pub active: usize,
    /// Whether the left sidebar (files / search / marks / calls / imports) is shown.
    pub show_left_sidebar: bool,
    /// Whether the right sidebar (Outline / Explain tabs) is shown.
    pub show_right_panel: bool,
    /// When set, the active pane shows this file's diff against `HEAD`.
    pub diff: Option<DiffState>,
    pub finder: Finder,
    pub search: SearchState,
    pub history: History,
    /// Trail nodes whose subtree is collapsed in the TRAIL view.
    pub trail_collapsed: std::collections::HashSet<usize>,
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
    /// Project-wide Rust type relations (traits implemented / implementors),
    /// built off-thread after indexing; feeds the hover structure peek.
    pub structure: structure::StructureIndex,
    /// Monotonic LSP document version, bumped on every `didChange`.
    pub lsp_doc_rev: i64,
    /// Last diagnostics version seen per language, to gate refresh ticks.
    pub seen_diag_version: std::collections::HashMap<String, u64>,
    /// Last inlay-hint refresh epoch seen per language, so a server-requested
    /// refresh re-fetches hints exactly once.
    pub seen_inlay_epoch: std::collections::HashMap<String, u64>,
    /// A language server download awaiting the user's consent.
    pub pending_lsp_consent: Option<LspConsent>,
    /// In-file find (Cmd+F), applied to the active pane.
    pub find: find::FindState,
    /// Active hover tooltip (Cmd-hover): position + content.
    pub hover: Option<HoverState>,
    /// Bumped on every hover-token change; a dwell task only shows the peek if its
    /// captured `gen` still matches (i.e. the cursor hasn't moved on).
    pub hover_gen: u64,
    /// The cursor is inside the hover tooltip — keep it open (so you can move into
    /// it and scroll it) and ignore code-view hover events until it leaves.
    pub hover_pinned: bool,
    /// The "Why is this here?" popup, when open.
    pub blame_why: Option<BlameWhy>,
    /// The active git time-travel session, if any.
    pub time_travel: Option<TimeTravel>,
    /// Generation counter for time-travel async results (drops stale loads).
    pub time_gen: u64,
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
    /// The main window's id, set once it is opened (daemon mode opens windows
    /// explicitly). Used to target window operations at the right window.
    pub main_window: Option<iced::window::Id>,
    /// Logical window size (from resize events), drives responsive layout and
    /// clamps the draggable panel sizes below.
    pub window_width: f32,
    pub window_height: f32,
    /// Whether the window is in fullscreen (toggled by the green control).
    pub fullscreen: bool,
    /// Whether the window has keyboard focus. The custom traffic-light controls
    /// grey out when it doesn't, like native macOS.
    pub window_focused: bool,
    /// Whether the pointer is over the traffic-light cluster, so the icons show
    /// on all three (native behaviour), not just the hovered one.
    pub controls_hovered: bool,
    /// User-draggable panel sizes (px): left sidebar width, right context-panel
    /// width, and the bottom debug/ask panel height. See [`resize::Divider`].
    pub sidebar_width: f32,
    pub right_width: f32,
    pub bottom_height: f32,
    pub font_size: f32,
}

pub const DEFAULT_FONT_SIZE: f32 = 13.0;
