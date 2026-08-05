//! The Message enum — every event the update loop handles.

use crate::app::prelude::*;
use crate::*;

#[derive(Debug, Clone)]
pub enum Message {
    /// The clew-server started and handed us its request channel.
    ServerConnected(tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>),
    /// The clew-server binary could not be spawned; fall back to local work.
    ServerUnavailable,
    // -- Connect (remote over SSH) ------------------------------------------
    /// Open the Connect modal (from the empty state, menu, or status bar).
    OpenConnect,
    /// Close the Connect modal without changing the connection.
    CloseConnect,
    /// Edit a field of the new-connection form.
    ConnectField(ConnectField, String),
    /// Pick a private-key file for the form via the native file dialog.
    ConnectPickIdentity,
    ConnectIdentityPicked(Option<PathBuf>),
    /// Connect to the host currently in the form (saving it for next time).
    ConnectSubmit,
    /// Connect to a saved host by index.
    ConnectToSaved(usize),
    /// Forget a saved host by index.
    ConnectRemoveSaved(usize),
    /// Switch back to reading local code (tears down the SSH transport).
    ConnectDisconnect,
    /// In the remote folder picker: enter a child directory / go up / list a path.
    RemoteBrowseTo(String),
    RemoteBrowseUp,
    /// Open the directory currently in view as the project.
    RemoteOpenHere,
    // -- Docs ---------------------------------------------------------------
    /// (Re)build the project's API docs from the server.
    DocsRefresh,
    /// Expand / collapse a file group in the DOCS tree.
    DocsToggleFile(String),
    /// Filter the DOCS tree by item name.
    DocsFilterChanged(String),
    /// Toggle showing all symbols vs. only the public API.
    DocsToggleShowAll,
    /// Toggle grouping the Docs tree by module/package vs. by file.
    DocsToggleGrouping,
    /// Open the doc page for the item at (file rel, definition line).
    DocsSelect {
        rel: String,
        line: usize,
    },
    /// Open the doc page for the symbol under the cursor (from the code view's
    /// right-click menu).
    ViewDocsFromMenu,
    /// A proxied process (spawned from a background stream, e.g. the debug
    /// adapter) registers where its `ProcessOutput` should be routed.
    RegisterProcFeed {
        proc: u64,
        feed: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    },
    /// An event (reply or notification) from the clew-server.
    ServerEvent(clew_protocol::ServerMessage),
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
        /// Signature line (1-based) -> doc comment, extracted alongside symbols.
        docs: HashMap<usize, String>,
        /// 0-based lines gated off by an inactive `#[cfg]` (dimmed).
        inactive: HashSet<usize>,
    },
    /// The project-wide Rust structure index finished building.
    StructureBuilt(structure::StructureIndex),
    /// Inlay hints came back from the language server for `abs`.
    InlayHintsLoaded {
        abs: PathBuf,
        hints: Vec<lsp::client::InlayHint>,
    },
    /// The watcher reports paths that may have changed on disk (unfiltered).
    FilesChanged(Vec<PathBuf>),
    /// Off-thread re-hash classified content-tracked paths (modified / deleted),
    /// and an existence probe of the other changed paths reported whether any is
    /// a create/delete of a (non-source) tree entry that needs a rescan.
    FilesRehashed {
        events: Vec<watch::FileEvent>,
        fs_structural: bool,
    },
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
    /// Open the note editor for the bookmark at (rel, 1-based line).
    BookmarkNoteEdit(String, usize),
    /// The bookmark-note draft text changed.
    BookmarkNoteInput(String),
    /// Save the bookmark note draft.
    BookmarkNoteSave,
    /// Cancel editing the bookmark note.
    BookmarkNoteCancel,
    /// Toggle the "understood" flag on a symbol (from the outline / notes list).
    NoteToggleUnderstood {
        rel: String,
        symbol: String,
    },
    /// Open the reading-note editor for a symbol (from the outline / notes list).
    NoteEditStart {
        rel: String,
        symbol: String,
    },
    /// The reading-note draft text changed.
    NoteEditInput(String),
    /// Save the reading-note draft.
    NoteEditSave,
    /// Cancel editing the reading note.
    NoteEditCancel,
    /// Remove a reading note entirely (from the notes list).
    NoteRemove {
        rel: String,
        symbol: String,
    },
    /// Jump to a noted symbol (resolving its live line; opens the file top if the
    /// symbol is orphaned).
    NoteJump {
        rel: String,
        symbol: String,
    },
    GoBack,
    GoForward,
    /// Jump to a node in the history tree view.
    HistoryJump(usize),
    /// Collapse / expand a node's subtree in the TRAIL view.
    TrailToggleCollapse(usize),
    /// Clear the whole navigation history tree.
    HistoryClear,
    /// Show / hide the left sidebar.
    ToggleLeftSidebar,
    /// Show / hide the right sidebar (Outline / Explain tabs).
    ToggleRightPanel,
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
    /// The window has been realized; apply one-time native tweaks (rounded
    /// corners on macOS, since the frameless window has square corners).
    WindowOpened,
    /// Start dragging the whole window (from the custom title-bar region).
    TitleBarDragged,
    /// The window gained or lost focus (greys out the custom controls).
    WindowFocusChanged(bool),
    /// The pointer entered or left the traffic-light cluster (shows the icons).
    ControlsHover(bool),
    /// Custom window controls (frameless window has no OS buttons).
    CloseWindow,
    MinimizeWindow,
    ToggleFullscreen,
    /// Drag a panel divider: the payload is the cursor's absolute x (sidebar /
    /// right panel) or y (bottom panel). See [`resize::Divider`].
    ResizeSidebar(f32),
    ResizeRight(f32),
    ResizeBottom(f32),
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
    /// The hover dwell elapsed for a token — show the peek if the cursor is still
    /// on it (`gen` matches the latest hover). Debounces flicker while moving.
    HoverDwell {
        epoch: u64,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    /// The cursor left the code — clear any open peek.
    HoverCleared,
    /// The cursor entered (true) or left (false) the hover tooltip itself.
    HoverPin(bool),
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
    /// Explain the function at the right-click context menu.
    ExplainFromMenu,
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
    OverlayOpenAt {
        abs: PathBuf,
        line: usize,
    },
    /// The project call graph finished (re)building off-thread.
    ProjectCallsBuilt {
        root: PathBuf,
        graph: projectcalls::ProjectCallGraph,
    },
    /// Flip the current overlay between the list and the node-link map.
    OverlayViewToggle,
    /// Flip the graph map between 3D (orbit + depth) and flat 2D.
    GraphToggle3D,
    /// Start / stop the 3D map's idle auto-spin.
    GraphToggleSpin,
    /// Set the Light/Dark/System appearance preference.
    SetThemePref(theme::ThemePref),
    /// Cycle the appearance (Dark → Light → System), for a menu/keyboard toggle.
    CycleTheme,
    /// The OS light/dark appearance changed (only acted on when following System).
    SystemAppearanceChanged,
    /// Pick the theme used for the light or dark slot (`is_light` selects which).
    SetThemeVariant {
        id: &'static str,
        is_light: bool,
    },
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
    /// Explain the whole project (bottom-up LLM pass).
    ExplainProject,
    /// Cancel the running explain pass (abort remaining LLM calls).
    CancelExplain,
    /// Force an immediate refresh of the whole understanding (explanations →
    /// index → overview), bypassing the auto-refresh cooldown. User-initiated.
    RefreshAll,
    /// Explain-pass progress. `done` counts attempts (successes + failures);
    /// `failed` is how many of those attempts errored, so the UI can report
    /// honestly instead of implying every counted item succeeded.
    ExplainProgress {
        generation: u64,
        done: usize,
        total: usize,
        failed: usize,
    },
    /// The explain pass finished with the fresh cache. `failed` is the number of
    /// nodes whose explanation errored; `auth_error` is set when the pass was cut
    /// short because the LLM rejected the request (e.g. a bad API key).
    ExplainDone {
        root: PathBuf,
        generation: u64,
        cache: explain::Cache,
        failed: usize,
        auth_error: Option<String>,
    },
    /// Show a file's / folder's explanation (Cmd+click in the tree).
    ShowExplanation(explain::Node),
    /// Re-explain the node in the open explanation modal (invalidate + rerun).
    ReexplainNode,
    /// Show (or generate on demand) a function's block-by-block walkthrough.
    ExplainBlocks(explain::Node),
    /// The block walkthrough for `node` finished (or failed).
    BlocksExplained {
        node: explain::Node,
        detail: Result<String, String>,
    },
    /// A background pass finished rendering math/mermaid blocks to SVG.
    SvgsGenerated {
        generation: u64,
        map: HashMap<u64, richmd::PreparedSvg>,
    },
    /// Show the architecture overview "home" in the main area.
    ShowOverview,
    /// Generate (or regenerate) the architecture overview.
    GenerateOverview,
    /// Show the code-statistics "home" in the main area (computes if stale).
    ShowStats,
    /// Recompute the code statistics regardless of freshness (the Refresh button).
    RefreshStats,
    /// A stats computation finished for `root`.
    StatsDone {
        root: PathBuf,
        rev: u64,
        report: stats::StatsReport,
    },
    /// Generate a new walkthrough for `scope` (empty = the whole codebase). The
    /// result is upserted into the library by scope, then opened.
    GenerateWalkthrough(String),
    /// Generate a narrated walkthrough of the current branch/PR diff (or the last
    /// commit when there's no base branch). Upserted into the library like a tour.
    GenerateDiffWalkthrough,
    /// Regenerate the library tour at this index (reusing its saved scope).
    WalkthroughRegenerate(usize),
    /// Delete the library tour at this index and persist the smaller library.
    WalkthroughDelete(usize),
    /// A walkthrough finished generating; `scope` keys the upsert into the library.
    WalkthroughDone {
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    },
    /// Open the library tour at this index for reading.
    WalkthroughOpen(usize),
    /// Return from a tour to the library list.
    WalkthroughBack,
    /// Flip the top input between searching the library and generating a tour.
    WalkthroughToggleMode,
    /// Jump to an absolute step index in the open walkthrough.
    WalkthroughGoto(usize),
    /// Move by a relative offset (Next / Prev).
    WalkthroughStep(i32),
    /// The top input (search query / scope prompt) changed.
    WalkthroughInputChanged(String),
    /// Drag the divider between the WALK steps list and the narration.
    ResizeWalkNarration(f32),
    /// The overview finished generating.
    OverviewDone {
        root: PathBuf,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    },
    /// Build / refresh the semantic embedding index.
    BuildEmbeddings,
    /// The embedding index finished building.
    EmbeddingsBuilt {
        root: PathBuf,
        result: Result<embed::Index, String>,
    },
    /// The Semantic-tab query text changed.
    SemanticQueryChanged(String),
    /// Run the semantic search for the current query.
    SemanticSearch,
    /// The query's embedding vector (or an error); the handler ranks the index.
    SemanticResults {
        query: String,
        result: Result<Vec<f32>, String>,
    },
    /// Open a semantic result: jump to the function/file in the code.
    OpenNode(explain::Node),
    /// Jump to the exact line where `caller` calls `callee` (from CALLED BY),
    /// resolved live from the caller file; falls back to the caller's definition.
    JumpToCall {
        caller_file: PathBuf,
        caller: String,
        callee: String,
    },
    /// Toolbar "Ask": open the bottom panel on the Ask tab, or collapse it.
    ToggleAsk,
    /// Switch the bottom panel's tab (Ask / Debug), opening it if collapsed.
    BottomTabPicked(BottomTab),
    /// Collapse the bottom panel.
    CollapseBottom,
    /// Show / hide the toolbar's "More" overflow menu.
    ToggleToolsMenu,
    ToggleTargetMenu,
    /// Open the "Keyboard Shortcuts" modal (from the More menu).
    OpenShortcuts,
    /// Close the "Keyboard Shortcuts" modal.
    CloseShortcuts,
    /// Start the interactive tutorial (from the More menu).
    TutorialStart,
    /// Move the tutorial by `delta` steps (+1 next, -1 back); past the end ends it.
    TutorialStep(i32),
    /// Jump the tutorial to a specific step.
    TutorialGoto(usize),
    /// End the tutorial.
    TutorialExit,
    /// Begin capturing a new chord for an action (click a binding).
    RebindStart(keymap::Action),
    /// Reset one action's binding to its default.
    RebindReset(keymap::Action),
    /// Reset every binding to its default.
    RebindResetAll,
    /// Toggle inline function summaries in the code view.
    ToggleInlineSummaries,
    /// Toggle the file-top summary banner in the code view.
    ToggleFileBanner,
    /// Toggle the code minimap.
    ToggleMinimap,
    /// Toggle LSP inlay hints (inferred types, parameter names).
    ToggleInlayHints,
    /// Pick the target the `#[cfg]` dimming is evaluated against.
    TargetSelected(inactive::Target),
    /// Toggle "skim" for the active file: fold function/method bodies to
    /// signatures + summaries, or expand them again.
    SkimFile,
    /// The Ask input box text changed.
    AskInputChanged(String),
    /// Submit the current question.
    AskSubmit,
    /// Ask a suggested (context-aware) question: fill the input and submit it.
    AskSuggested(String),
    /// The question's embedding vector came back (retrieval step).
    AskRetrieved {
        question: String,
        qvec: Result<Vec<f32>, String>,
    },
    /// A streamed token for the current answer — append to the open turn.
    AskDelta(String),
    /// The streamed answer finished (`Some` = the error it failed with).
    AskStreamEnded(Option<String>),
    /// The agent made a tool call — append a step chip to the open turn.
    AgentStepped(AgentStep),
    /// The agent turn finished (`Some` = the error it failed/stopped with).
    AgentTurnEnded(Option<String>),
    /// Stop the in-flight agent turn.
    AgentStop,
    /// Toggle a notebook cell's outputs between collapsed and expanded.
    NbToggleOutputs {
        pane: usize,
        cell: usize,
    },
    /// Clear the whole Ask conversation.
    AskClear,
    /// Remove the pinned code-selection context at this index.
    AskUnpin(usize),
    /// Jump to the pinned selection at this index (open its file at its line).
    AskPinGoto(usize),
    /// Add the current code selection as a context chip and open the Ask panel.
    AskAboutSelection,
    /// Explain why the right-clicked line (or selection) exists, from git blame.
    WhyIsThisHere,
    /// The "why is this here?" answer finished generating.
    BlameWhyDone {
        title: String,
        commits: Vec<(String, String)>,
        result: Result<String, String>,
    },
    /// Close the "why is this here?" popup.
    BlameWhyClose,
    /// Enter git time travel for the active file; `symbol` scopes it to the
    /// function under the cursor.
    TimeTravelStart {
        symbol: bool,
    },
    /// Commits for a time-travel session finished loading.
    TimeTravelReady {
        generation: u64,
        abs: PathBuf,
        rel: String,
        lang: Option<&'static str>,
        scope: TimeScope,
        commits: Vec<git::HistCommit>,
    },
    /// Scrub to commit index `idx` (0 = newest).
    TimeTravelGoto(usize),
    /// The historical content for a step finished loading.
    TimeTravelStep {
        generation: u64,
        idx: usize,
        step: Box<TimeStep>,
    },
    /// The historical view scrolled (tracked for its sticky headers).
    TimeTravelScrolled(scrollable::Viewport),
    /// Place the caret / begin a selection in the historical (read-only) view.
    TimeTravelSelectStart {
        line: usize,
        col: usize,
    },
    /// Extend the historical view's selection while dragging.
    TimeTravelSelectDrag {
        line: usize,
        col: usize,
    },
    /// Switch a session between whole-file and the current function's scope.
    TimeTravelToggleScope,
    /// Generate the LLM "what & why" summary of the current commit's diff.
    TimeTravelWhy,
    TimeTravelWhyDone {
        generation: u64,
        sha: String,
        result: Result<String, String>,
    },
    /// Generate the "story of this function" narrative (symbol scope).
    TimeTravelStory,
    TimeTravelStoryDone {
        generation: u64,
        result: Result<String, String>,
    },
    /// Leave time travel, back to the live file.
    TimeTravelExit,
    /// A no-op sink for fire-and-forget async debug commands.
    Noop,
    /// Start (or restart) a debug session from the project's launch config.
    StartDebug,
    /// The adapter is ready; carry its handle + TCP port (for child sessions).
    DapStarted {
        client: dap::DapClient,
        port: Option<u16>,
    },
    /// A child session (js-debug) is ready; it becomes the active client.
    DapChildStarted(dap::DapClient),
    /// An event pushed from the debug adapter.
    DapEvent(dap::DapEvent),
    /// The stopped frame's stack + scopes/variables finished loading.
    DapStopInspected {
        frames: Vec<dap::StackFrame>,
        scopes: Vec<DebugScope>,
    },
    /// Stepping / continue control.
    DebugControl(DebugCmd),
    /// End the debug session.
    DebugStop,
    /// Toggle a breakpoint at (file, 1-based line) — from the code context menu.
    BreakpointToggle {
        path: PathBuf,
        line: usize,
    },
    /// Toggle a breakpoint at the right-clicked line (code context menu).
    ToggleBreakpointFromMenu,
    /// Open the condition editor for the right-clicked line (context menu).
    ConditionalBreakpointFromMenu,
    /// The breakpoint-condition draft changed.
    BpConditionInput(String),
    /// Apply the drafted condition (set a conditional breakpoint).
    BpConditionSet,
    /// Close the condition editor.
    BpConditionCancel,
    /// The add-watch input changed.
    DebugWatchInput(String),
    /// Add the current input as a watch expression.
    DebugWatchAdd,
    /// Remove watch expression at index.
    DebugWatchRemove(usize),
    /// Watch expressions finished evaluating: (expression, value) pairs.
    DebugWatchesEvaluated(Vec<(String, String)>),
    /// Starting the debugger failed.
    DebugFailed(String),
    /// Embedding settings draft edits.
    SettingsEmbedKeyChanged(String),
    SettingsEmbedModelChanged(String),
    SettingsEmbedBaseUrlChanged(String),
    /// Close the explanation overlay.
    CloseExplanation,
    /// A markdown link in an explanation was clicked.
    OpenLink(String),
    /// Toggle a markdown file between its rendered view and its raw source.
    ToggleMarkdownSource(usize),
    /// Open / close the LLM settings modal.
    OpenSettings,
    CloseSettings,
    /// LLM settings draft edits.
    SettingsProviderPicked(llm::Provider),
    SettingsKeyChanged(String),
    SettingsModelChanged(String),
    SettingsBaseUrlChanged(String),
    /// Save the LLM settings to the global config.
    SettingsSaved,
    Tick,
    /// A window animation frame. Carries no work of its own — it exists so the
    /// `window::frames()` subscription keeps the window redrawing while a graph
    /// map is on screen, so its canvas can step its live force simulation.
    GraphFrame,
    ToggleServerPanel,
    LspRestart(String),
    LspRemove {
        name: String,
        version: String,
    },
    LspDownloadFor(String),

    // -- Auto-update ---------------------------------------------------------
    /// Manually check for updates (from the menu).
    CheckForUpdates,
    /// A version check finished. `manual` means announce the result even when
    /// already up to date.
    UpdateChecked {
        manual: bool,
        result: Result<clew_core::update::Release, String>,
    },
    /// Dismiss the "update available" banner for this session.
    UpdateBannerDismissed,
    /// Open the release-notes modal.
    UpdateShowNotes,
    /// Close the release-notes modal.
    UpdateCloseNotes,
    /// Start downloading and installing the available update.
    UpdateInstallStart,
    /// Streamed DMG download progress.
    UpdateDownloadProgress {
        generation: u64,
        done: u64,
        total: Option<u64>,
    },
    /// The DMG finished downloading (or failed).
    UpdateDownloaded {
        generation: u64,
        result: Result<PathBuf, String>,
    },
    /// The bundle swap and relauncher finished (or failed). `Ok` means quit so
    /// the detached helper can complete.
    UpdateInstalled(Result<(), String>),
    /// Toggle the "check for updates automatically" preference.
    SetAutoUpdate(bool),
}
