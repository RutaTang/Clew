//! The Message enum — every event the update loop handles — nested by
//! feature: each variant of [`Message`] wraps one feature's sub-enum
//! ([`AskMsg`], [`DebugMsg`], …), which `App::update` hands to that feature's
//! `update_<feature>` in its own module. Every async result carries a
//! [`Stamp`], checked once in `dispatch` (see [`Message::origin`]).

use crate::app::prelude::*;
use crate::*;

#[derive(Debug, Clone)]
pub enum Message {
    /// The clew-server transport: its lifecycle and what it delivers. Handled in `app::server`
    /// (`App::update_server`).
    Server(ServerMsg),
    /// The Connect modal: the host form, saved hosts, the remote folder browser. Handled in
    /// `app::connect` (`App::update_connect`).
    Connect(ConnectMsg),
    /// Opening a project: the folder picker, consent, scanning, its persisted state. Handled in
    /// `app::project` (`App::update_project`).
    Project(ProjectMsg),
    /// Keeping the index current: watcher batches, the symbol and structure indexes. Handled in
    /// `app::watch` (`App::update_watch`).
    Watch(WatchMsg),
    /// The code panes: opening files, highlighting, selection, scrolling, folding, find, the
    /// diff and the keyboard. Handled in `app::editor` (`App::update_editor`).
    Editor(EditorMsg),
    /// The Cmd-hover peek and the right-click context menu. Handled in `app::hover`
    /// (`App::update_hover`).
    Hover(HoverMsg),
    /// Getting around: search, the finder, go-to-definition and references. Handled in
    /// `app::navigation` (`App::update_nav`).
    Nav(NavMsg),
    /// The reading session: bookmarks, notes, the trail, the reading target. Handled in
    /// `app::reading` (`App::update_reading`).
    Reading(ReadingMsg),
    /// The Calls tab's call hierarchy. Handled in `app::calls` (`App::update_calls`).
    Calls(CallsMsg),
    /// The project graphs and their overlays: imports and the call graph. Handled in
    /// `app::graph` (`App::update_graph`).
    Graph(GraphMsg),
    /// Explanations: the explain pass and the explanation overlay. Handled in `app::explain`
    /// (`App::update_explain`).
    Explain(ExplainMsg),
    /// Rich content: rendered math and diagrams, and links clicked in it. Handled in
    /// `app::content` (`App::update_content`).
    Content(ContentMsg),
    /// The overview and stats homes. Handled in `app::overview` (`App::update_overview`).
    Overview(OverviewMsg),
    /// Walkthroughs: generating, saving and reading tours. Handled in `app::walkthrough`
    /// (`App::update_walk`).
    Walk(WalkMsg),
    /// Semantic search and its embedding index. Handled in `app::semantic`
    /// (`App::update_semantic`).
    Semantic(SemanticMsg),
    /// The Ask panel: questions, answers streaming in, pinned selections. Handled in `app::ask`
    /// (`App::update_ask`).
    Ask(AskMsg),
    /// Git history in the reader: time travel and "Why is this here?". Handled in
    /// `app::timetravel` (`App::update_time_travel`).
    TimeTravel(TimeTravelMsg),
    /// The debugger: sessions, adapter events, breakpoints and watches. Handled in `app::debug`
    /// (`App::update_debug`).
    Debug(DebugMsg),
    /// Language servers: starting and provisioning them, inlay hints, the Language Servers
    /// panel. Handled in `app::lsp` (`App::update_lsp`).
    Lsp(LspMsg),
    /// The DOCS tab. Handled in `app::docs` (`App::update_docs`).
    Docs(DocsMsg),
    /// The settings modal and the appearance. Handled in `app::settings`
    /// (`App::update_settings`).
    Settings(SettingsMsg),
    /// Auto-update: checking, downloading and installing a release. Handled in `app::updater`
    /// (`App::update_updater`).
    Updater(UpdaterMsg),
    /// The window and its chrome: controls, layout, panels, menus, the shortcuts modal. Handled
    /// in `app::runtime` (`App::update_window`).
    Window(WindowMsg),
    /// The guided tour. Handled in `app::tutorial` (`App::update_tutorial`).
    Tutorial(TutorialMsg),
    /// The refresh tick, while something is changing (see
    /// `App::window_subscription`): language servers starting or indexing,
    /// the server panel, an auto-refresh waiting out its cooldown.
    Tick,
    /// A no-op sink for fire-and-forget async debug commands.
    Noop,
}

/// The clew-server transport: its lifecycle and what it delivers.
#[derive(Debug, Clone)]
pub enum ServerMsg {
    /// The clew-server started and handed us its request channel. `conn` is
    /// the transport instance (`conn_gen`) this came from: a late message
    /// from a dead or replaced transport (its subscription dropped, but its
    /// channel already held messages) must be recognized and ignored, or an
    /// A→B host switch could install A's channel — or A's events — under B.
    Connected { conn: u64, tx: server::RequestTx },
    /// The clew-server could not be started or reached — or automatic
    /// reconnecting gave up — so fall back to local work. `reason` names what
    /// went wrong (an unknown or changed host key, refused credentials, a
    /// failed deploy, …) for the Connect modal and the status bar.
    Unavailable { conn: u64, reason: String },
    /// The host refused as unknown presented `key`, which the transport
    /// scanned and fingerprinted. Handled like `ServerMsg::Unavailable`, and the
    /// Connect modal then offers to trust exactly that key (see
    /// `ConnectStage::TrustHost`).
    HostKeyUnknown {
        conn: u64,
        reason: String,
        key: connect::ScannedHostKey,
    },
    /// The host presented a different key than the one on record, and the
    /// key on record is one trusted in clew (it is in clew's own
    /// known_hosts, under `forget_host`). Handled like `ServerMsg::Unavailable`,
    /// and the Connect modal then offers to forget that key and connect
    /// again — whose new key is then shown to be checked before it is
    /// trusted (see `ConnectStage::HostKeyChanged`).
    HostKeyChanged {
        conn: u64,
        reason: String,
        forget_host: String,
    },
    /// A proxied process (spawned from a background stream, e.g. the debug
    /// adapter) registers where its `ProcessOutput` should be routed.
    RegisterProcFeed {
        stamp: Stamp,
        proc: u64,
        feed: ProcFeed,
    },
    /// An event (reply or notification) from the clew-server.
    Event {
        conn: u64,
        msg: clew_protocol::ServerMessage,
    },
    /// The backoff after the server refused correlated request `id` (a
    /// Search, a `BuildDocs`) as not ready yet — its scan window — ran out:
    /// send `request` again as retry number `attempt`, if this window is
    /// still waiting on `id` (see `App::send_retrying`). Transport-stamped:
    /// a retry must not reach another project or another server.
    ResendNotReady {
        stamp: Stamp,
        id: u64,
        request: clew_protocol::Request,
        attempt: u32,
    },
    /// The server transport died mid-session (process exit, SSH drop). All
    /// in-flight server work is void; bumping `conn_gen` restarts the
    /// subscription, which is the reconnect.
    Disconnected {
        conn: u64,
        /// Why, when the transport knows more than "it closed": a frame that
        /// did not decode — every reply decodes WITH its frame, typed payloads
        /// included, so a malformed one ends the stream right there — or a
        /// server that never answered.
        reason: Option<String>,
    },
}

/// The Connect modal: the host form, saved hosts, the remote folder browser.
#[derive(Debug, Clone)]
pub enum ConnectMsg {
    /// Open the Connect modal (from the empty state, menu, or status bar).
    Open,
    /// Close the Connect modal without changing the connection.
    Close,
    /// Edit a field of the new-connection form.
    Field(ConnectField, String),
    /// Pick a private-key file for the form via the native file dialog.
    PickIdentity,
    IdentityPicked(Option<PathBuf>),
    /// Tick / untick the form's "send my AI API keys to this host" opt-in. It
    /// only records the intent for the host being CONFIGURED in the form; it
    /// never changes what the live connection holds (see
    /// [`ConnectMsg::RevokeAiKeys`] for that).
    ToggleAiKeys(bool),
    /// Withdraw the AI API keys from the host this window is connected to right
    /// now: the server is told to forget them, and the saved host's opt-in is
    /// turned off so the next connect does not re-grant them.
    RevokeAiKeys,
    /// Connect to the host currently in the form (saving it for next time).
    Submit,
    /// Trust the unknown host key the Connect modal shows — named by its
    /// fingerprint, so a click can only ever record the key it was drawn
    /// for — then connect again.
    TrustHost {
        fingerprint: String,
    },
    /// Decline the host-key question on offer (trust an unknown key, forget
    /// a changed one): back to the form, with the reason.
    TrustCancel,
    /// Forget the changed host key clew recorded under `host` in its own
    /// known_hosts (`connect::forget_host_key`), then connect again, so the
    /// new key is shown to be checked before it is trusted. Named by the host
    /// field the prompt was drawn for, so a click can only ever forget the
    /// key it showed the refusal of.
    ForgetHostKey {
        host: String,
    },
    /// Connect to the saved host with this identity (`user@host` + port —
    /// the key the store de-duplicates on). An identity, not an index: the
    /// list is re-merged from disk whenever any window saves, so a row's
    /// index can name another host by the time its click lands.
    ToSaved {
        user_host: String,
        port: u16,
    },
    /// Forget the saved host with this identity (see `ConnectMsg::ToSaved`).
    RemoveSaved {
        user_host: String,
        port: u16,
    },
    /// Switch back to reading local code (tears down the SSH transport).
    Disconnect,
    /// In the remote folder picker: list a directory (a child, or the parent
    /// the view resolved for "up").
    BrowseTo(String),
    /// Open the directory currently in view as the project.
    OpenHere,
}

/// Opening a project: the folder picker, consent, scanning, its persisted state.
#[derive(Debug, Clone)]
pub enum ProjectMsg {
    OpenFolderPressed,
    FolderPicked(Option<PathBuf>),
    ConsentAllowed,
    ConsentDenied,
    /// The client-side (fallback) scan finished. Its stamp is transport-bound
    /// (the transport and the project instance it was started under): the
    /// root alone cannot tell a local scan of `/p` from a
    /// remote project that happens to live at `/p`, so a scan that outlived a
    /// transport switch must not install itself as the new host's project.
    ScanDone {
        stamp: Stamp,
        result: ScanResult,
        /// What the walk skipped or added (see `fs_scan::ScanReport`), for
        /// the status line and the tree's marks.
        report: fs_scan::ScanReport,
    },
    /// The project's persisted state finished loading off the UI thread:
    /// reading history, bookmarks, notes, `lsp.toml`, the reading target, the
    /// walkthrough library, and the derived caches (explanations, overview,
    /// stats, semantic index). Applied only to the project instance it was
    /// read for (its stamp).
    StateLoaded {
        stamp: Stamp,
        state: Handoff<LoadedProjectState>,
    },
    /// A structural change (file created/deleted/renamed) rebuilt the tree; swap
    /// it in without the full project-open reset. Stamped with the project
    /// instance the rescan was started under.
    TreeUpdated {
        stamp: Stamp,
        result: ScanResult,
        /// What the rescan skipped or added (see `ScanDone::report`).
        report: fs_scan::ScanReport,
    },
    ToggleDir(String),
}

/// Keeping the index current: watcher batches, the symbol and structure indexes.
#[derive(Debug, Clone)]
pub enum WatchMsg {
    SymbolIndexDone {
        stamp: Stamp,
        indexed: index::Indexed,
    },
    /// The project-wide Rust structure index finished building for the
    /// project instance its stamp names (checked like `SymbolIndexDone`).
    StructureBuilt {
        stamp: Stamp,
        /// The registry revision the build's files were read at. The index is
        /// rebuilt as Rust files change, so two builds of the same project can
        /// be in flight, and the one that arrives last is not the one that read
        /// the newest bytes — nothing else in the payload can tell them apart,
        /// and applying the older one pins the hover peek to the pre-edit
        /// relations until the next edit happens to trigger another build.
        rev: u64,
        index: structure::StructureIndex,
    },
    /// Off-thread re-hash classified content-tracked paths (modified / deleted),
    /// and an existence probe of the other changed paths reported whether any is
    /// a create/delete of a (non-source) tree entry that needs a rescan.
    FilesRehashed {
        stamp: Stamp,
        events: Vec<watch::FileEvent>,
        /// The registry version each path was hashed AGAINST when this batch
        /// went out (`0` for a path we did not track yet). Two batches can be
        /// in flight over the same file and they do not complete in read
        /// order — a big batch reads a file early and lands late. Applying
        /// blindly would roll the registry and symbol index back to the older
        /// read, so a batch is only applied to a file whose recorded version
        /// still is the one it was computed from (a compare-and-swap).
        baselines: HashMap<PathBuf, crate::incremental::Version>,
        fs_structural: bool,
        /// Dropped once the chunk is applied: the rehash reads its next chunk
        /// only then (see `watch::rehash_stream`).
        ack: watch::ChunkAck,
    },
    /// The second half of applying a watcher batch: the source files it
    /// modified were parsed (symbols, raw imports) on the blocking pool.
    /// Applied per file only while the registry still records the exact bytes
    /// that were parsed (`IndexedFile::hash`), and only in the project
    /// instance the batch belonged to — so the UI thread never parses, and a
    /// parse that a newer edit overtook cannot roll the index back.
    FilesIndexed {
        stamp: Stamp,
        files: Vec<IndexedFile>,
    },
}

/// The code panes: opening files, highlighting, selection, scrolling, folding, find, the diff and the keyboard.
#[derive(Debug, Clone)]
pub enum EditorMsg {
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
        stamp: Stamp,
        /// The load token minted at request time; applied only while the pane
        /// still expects this exact load (see `ProjectLink::pane_pending`).
        req: u64,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    },
    /// An open a time-travel start held back, whose load failed meanwhile,
    /// carried out now (`App::carry_out_superseded`): `said` is what that
    /// failure says on the status line, as a load's own answer, after what
    /// came with carrying it out.
    HeldOpenFailed {
        stamp: Stamp,
        said: String,
    },
    Highlighted {
        stamp: Stamp,
        abs: PathBuf,
        /// Hash of the source these were computed from. Two highlight passes
        /// for one file can be in flight at once and finish in either order,
        /// so the result has to name the bytes it describes — matching on the
        /// path (and the line COUNT) let a stale pass repaint the view while
        /// the pane's own source was already newer.
        src_hash: crate::incremental::Version,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        /// Signature line (1-based) -> doc comment, extracted alongside symbols.
        docs: HashMap<usize, String>,
        /// 0-based lines gated off by an inactive `#[cfg]` (dimmed).
        inactive: HashSet<usize>,
        /// The reading target `inactive` was evaluated against. Unlike the
        /// rest of the bundle it does not follow from the bytes, so the hash
        /// cannot vouch for it: the user may have picked another target while
        /// this pass ran, and the same source then has a different answer.
        target: inactive::Target,
    },
    /// Per-line git blame + change status finished loading for `abs`.
    GitInfoLoaded {
        stamp: Stamp,
        abs: PathBuf,
        /// The request stamp minted when this pass started (same id space as
        /// server `GitInfo` requests). Only the newest request for the file
        /// may paint its gutter: two passes for one file finish in either
        /// order, and the older one landing last described bytes the pane no
        /// longer shows.
        req: u64,
        /// `Ok(None)`: nothing to show (untracked, not a repository). `Err`:
        /// git could not answer, with the reason.
        info: Result<Option<Arc<git::GitInfo>>, String>,
    },
    Scrolled(usize, scrollable::Viewport),
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
    /// The reader scrolled `pane` — its code, or the rendered markdown,
    /// notebook or diff shown in its place — with the wheel, the trackpad or
    /// the scrollbar (`ui::reader_scroll`), once per input event that moved
    /// it. Sent only while a walkthrough step waits to settle in the pane's
    /// file (`ui::reader_scroll_report`), the one thing it tells. `Scrolled`
    /// reports every change of the viewport, the app's own scrolls included;
    /// this is only ever the reader's (see `App::reader_moved_caret`).
    ReaderScrolled(usize),
    /// Toggle the diff-vs-HEAD view for the active file. Pressed while a diff
    /// is still loading, it cancels that load.
    ToggleDiff,
    /// The diff for `abs` finished computing. `req` is the load it answers
    /// (`ProjectSession::diff_pending`): a load the user cancelled, or one superseded by
    /// a newer toggle, is dropped. `Ok(None)` = git does not track the file;
    /// `Ok(Some(lines))` = its diff against HEAD, empty when it is unchanged;
    /// `Err` = git could not be asked, which is not the same answer and is
    /// reported as an error.
    DiffLoaded {
        stamp: Stamp,
        req: u64,
        abs: PathBuf,
        rel: String,
        result: Result<Option<Vec<git::DiffLine>>, String>,
    },
    OutlineJump(usize),
    FontSizeDelta(f32),
    FontSizeReset,
    KeyPressed(keyboard::Key, keyboard::Modifiers),
    /// Run a keymap command action the way its chord does — through
    /// `App::run_command_action`, the ONE table of what an action does, which
    /// can look at the app's state first (Explain All cancels a running
    /// pass). The menu bar sends this for a click on an action's item and for
    /// a chord AppKit matched as that item's key equivalent, so a click, a
    /// menu-owned chord and a chord the key handler sees can never disagree.
    RunAction(keymap::Action),
    ModifiersChanged(keyboard::Modifiers),
    FindOpened,
    FindQueryChanged(String),
    FindStep(i32),
    FindClosed,
    /// Toggle "skim" for the active file: fold function/method bodies to
    /// signatures + summaries, or expand them again.
    SkimFile,
    /// Toggle a notebook cell's outputs between collapsed and expanded.
    NbToggleOutputs {
        pane: usize,
        cell: usize,
    },
    /// Expand (or collapse) every cell's outputs in a notebook at once.
    NbExpandAll {
        pane: usize,
        expand: bool,
    },
    /// Toggle a markdown file between its rendered view and its raw source.
    ToggleMarkdownSource(usize),
}

/// The Cmd-hover peek and the right-click context menu.
#[derive(Debug, Clone)]
pub enum HoverMsg {
    ContextMenuOpened {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    ContextMenuClosed,
    ContextGoto(GotoKind),
    Requested {
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    /// The hover dwell elapsed for a token — show the peek if the cursor is still
    /// on it (`hover_gen` still equals `ProjectSession::hover_gen`). Debounces flicker
    /// while moving.
    Dwell {
        stamp: Stamp,
        hover_gen: u64,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    },
    /// The cursor left the code — clear any open peek.
    Cleared,
    /// The cursor entered (true) or left (false) the hover tooltip itself.
    Pin(bool),
    Answered {
        stamp: Stamp,
        /// `ProjectSession::hover_gen` at request time — the peek this text was fetched
        /// for. Coordinates alone repeat across files, so after the pane's
        /// document is replaced under a motionless cursor a stale result
        /// would otherwise paint a tooltip for the file that is gone.
        hover_gen: u64,
        line: usize,
        col: usize,
        text: Option<String>,
    },
}

/// Getting around: search, the finder, go-to-definition and references.
#[derive(Debug, Clone)]
pub enum NavMsg {
    SearchQueryChanged(String),
    SearchSubmitted,
    SearchDone {
        stamp: Stamp,
        /// Submission counter value at request time; only the latest submission
        /// may paint the Search sidebar (see `ProjectSession::search_seq`).
        seq: u64,
        /// The hits, and the files that could not be searched.
        result: search::SearchReport,
    },
    /// Toggle a match option (regex / case-sensitive / whole-word).
    SearchToggle(SearchOpt),
    SearchIncludeChanged(String),
    SearchExcludeChanged(String),
    FinderOpened(FinderMode),
    FinderClosed,
    FinderQueryChanged(String),
    /// Open what a finder row names: a file (`line: None`) or a symbol's
    /// definition. The row's target itself, not its index into the file list
    /// or the symbol index — both are replaced by a rescan or a re-index.
    FinderPick {
        abs: PathBuf,
        line: Option<usize>,
    },
    FinderConfirm,
    GotoLineRequested,
    DefinitionResult {
        stamp: Stamp,
        /// `ProjectSession::goto_seq` value at request time; a superseded request's result
        /// must not jump the editor.
        seq: u64,
        result: Result<Vec<lsp::client::Target>, String>,
    },
    ReferencesResult {
        stamp: Stamp,
        /// `ProjectSession::search_seq` value at request time (references paint the Search
        /// sidebar, so they share its staleness counter).
        seq: u64,
        result: Result<Vec<lsp::client::Target>, String>,
    },
    /// The preview lines for a location list (references, or the several
    /// targets of one definition) were read off the UI thread: `(hit index,
    /// preview)` pairs. `seq` is the `ProjectSession::search_seq` the list was painted
    /// under; a list the sidebar has since moved on from is left alone.
    LocationPreviews {
        stamp: Stamp,
        seq: u64,
        previews: Vec<(usize, String)>,
    },
    /// Jump to the exact line where `caller` calls `callee` (from CALLED BY),
    /// resolved live from the caller file; falls back to the caller's definition.
    JumpToCall {
        caller_file: PathBuf,
        caller: String,
        callee: String,
    },
}

/// The reading session: bookmarks, notes, the trail, the reading target.
#[derive(Debug, Clone)]
pub enum ReadingMsg {
    BookmarkToggled,
    /// Remove the bookmark at (rel, 1-based line) — its identity; the list
    /// is kept sorted, so another window's insertion shifts every index.
    BookmarkRemoved {
        rel: String,
        line: usize,
    },
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
    /// Jump to node `id` of the history tree view — ignored unless `id`
    /// still names a visit to `loc`: node ids are renumbered when the oldest
    /// visits are evicted, and a trail can be replaced wholesale (loaded,
    /// adopted from the remote host), so a bare id could name another visit
    /// by the time the click lands.
    HistoryJump {
        id: usize,
        loc: Loc,
    },
    /// Collapse / expand node `id`'s subtree in the TRAIL view (checked like
    /// [`ReadingMsg::HistoryJump`]).
    TrailToggleCollapse {
        id: usize,
        loc: Loc,
    },
    /// Clear the whole navigation history tree.
    HistoryClear,
    /// A write of the reading trail of the project at `root` finished (see
    /// `App::save_history`). Not stamped: the writer is the window's, and
    /// its next write must start whichever project is open now.
    TrailSaved {
        root: PathBuf,
        result: Result<(), String>,
    },
    /// Pick the target the `#[cfg]` dimming is evaluated against.
    TargetSelected(inactive::Target),
}

/// The Calls tab's call hierarchy.
#[derive(Debug, Clone)]
pub enum CallsMsg {
    /// Open the call hierarchy for the symbol under the cursor (`gc`).
    Requested,
    /// Open the call hierarchy from the right-click context menu.
    FromMenu,
    /// `prepareCallHierarchy` resolved the anchor item(s). `token` was minted
    /// at request time; only the awaited request may install a tree.
    Prepared {
        stamp: Stamp,
        token: u64,
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    },
    /// Expand (fetch children of, or toggle) node `id` of the call tree
    /// identified by `token` — ignored when the tree was replaced since the
    /// view built this message (a direction flip, a new hierarchy), so a click
    /// can never act on a different tree's node that happens to share the
    /// index.
    ExpandNode { token: u64, id: usize },
    /// A node's callers/callees arrived. `token` names the tree the fetch was
    /// issued from; node ids are bare indices, so a result for a replaced tree
    /// must not graft onto whatever occupies that index now.
    Children {
        stamp: Stamp,
        token: u64,
        id: usize,
        items: Vec<lsp::client::CallItem>,
    },
    /// Flip between callers and callees.
    Direction,
    /// Recursively expand the whole tree (to the project boundary).
    ExpandAll,
}

/// The project graphs and their overlays: imports and the call graph.
#[derive(Debug, Clone)]
pub enum GraphMsg {
    /// A remote project's tsconfig/jsconfig path mappings were fetched from
    /// the host (see `App::fetch_remote_ts_configs`); applied only to the
    /// project instance they were fetched for, and only while no later fetch
    /// or drop of them has happened (`generation`).
    RemoteTsConfigsLoaded {
        stamp: Stamp,
        /// `ProjectSession::remote_ts_configs_gen` when the fetch started.
        generation: u64,
        result: Result<imports::TsConfigs, String>,
    },
    /// An import-graph job finished (see `App::schedule_imports`): the graph
    /// with its batch applied, and that graph's cycles — or why the job
    /// failed. Applied only to the project instance it was spawned for.
    ImportGraphUpdated {
        stamp: Stamp,
        result: Result<crate::app::tasks::ImportJobDone, String>,
        /// Lay the overview's module map out again from the result.
        refresh_overview: bool,
    },
    /// Expand/collapse node `id` of the import tree identified by `token`
    /// (`ProjectSession::import_tree_token`) — ignored when the tree was
    /// rebuilt since the view drew it (focus moved, direction flipped, the
    /// graph changed), so a click can never toggle another tree's node that
    /// happens to share the index.
    /// How often each file changed over the recent history arrived (or did
    /// not): the change-frequency overlay's data.
    ChurnLoaded {
        stamp: Stamp,
        result: Result<Vec<clew_protocol::FileChurn>, String>,
    },
    /// Colour the map by change frequency instead of by language, or back.
    ToggleHeat,
    ImportExpand {
        token: u64,
        id: usize,
    },
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
    /// The project call graph finished (re)building off-thread — or failed
    /// to (the server refused, the transport died, the build panicked, the
    /// graph did not validate), which is reported, never drawn as a project
    /// without calls.
    ProjectCallsBuilt {
        stamp: Stamp,
        graph: Result<projectcalls::ProjectCallGraph, String>,
    },
    /// A map's layout, computed off the UI thread (see
    /// `App::refresh_graph_layout`) for `overlay` — or why it could not be.
    /// Applied only while it is the one requested last (`seq`,
    /// `ProjectSession::graph_layout_seq`).
    GraphLaidOut {
        stamp: Stamp,
        seq: u64,
        overlay: Overlay,
        layout: Result<crate::graphlayout::Layout, String>,
    },
    /// Flip the current overlay between the list and the node-link map.
    OverlayViewToggle,
    /// Flip the graph map between 3D (orbit + depth) and flat 2D.
    Toggle3D,
    /// Start / stop the 3D map's idle auto-spin.
    ToggleSpin,
    /// Kick a background LSP pass that rebuilds the call graph with exact edges.
    RefineProjectCalls,
    /// The bound on a wait for a starting language server ran out (see
    /// `App::fold_refine_pending`): the refinement is handed back if that
    /// wait is still on.
    RefineWaitOver {
        stamp: Stamp,
        /// The wait it bounds (`ProjectCallsState::refine_wait`, or
        /// `refine_full_wait`), by its number.
        wait: u64,
    },
    /// Progress of the running LSP refine.
    RefineProgress {
        stamp: Stamp,
        generation: u64,
        done: usize,
        total: usize,
    },
    /// The LSP-precise call graph finished (re)building. Carries the symbol-keyed
    /// edge set (kept for incremental patching) and the ready display graph —
    /// or why the pass failed (a step it runs on the blocking pool panicked),
    /// which hands the call graph back to the name-based build.
    ProjectCallsRefined {
        stamp: Stamp,
        generation: u64,
        result: Result<crate::app::tasks::RefineDone, String>,
    },
}

/// Explanations: the explain pass and the explanation overlay.
#[derive(Debug, Clone)]
pub enum ExplainMsg {
    /// Explain the function at the right-click context menu.
    FromMenu,
    /// Explain the whole project (bottom-up LLM pass), at the user's word:
    /// every node whose prompt changed is paid for.
    Project,
    /// The automatic refresh pass, started because source files changed: it
    /// pays for code that changed — found by content, wherever it changed —
    /// and for what quotes a summary it writes, never for a new prompt
    /// wording (`explain::Reuse::ChangedSources`).
    Refresh,
    /// Cancel the running explain pass (abort remaining LLM calls).
    Cancel,
    /// Force an immediate refresh of the whole understanding (explanations →
    /// index → overview), bypassing the auto-refresh cooldown. User-initiated.
    RefreshAll,
    /// Explain-pass progress. `done` counts attempts (successes + failures);
    /// `failed` is how many of those attempts errored, so the UI can report
    /// honestly instead of implying every counted item succeeded.
    Progress {
        stamp: Stamp,
        generation: u64,
        done: usize,
        total: usize,
        failed: usize,
    },
    /// The explain pass finished with the fresh cache. `failures` says what
    /// it could not explain and why, and why it stopped short if it did — a
    /// key the provider rejected, calls cancelled, a failure of its own.
    Done {
        stamp: Stamp,
        generation: u64,
        cache: explain::Cache,
        failures: PassFailures,
        /// What the pass paid for, and what it kept or left and why
        /// (`explain::Tally`). Boxed: the report is the larger part of the
        /// message, and every other one would be carried at its size.
        tally: Box<explain::Tally>,
        /// The entries the pass changed, and what each replaced
        /// (`explain::Pass::written`): what the window saves.
        written: explain::Unsaved,
    },
    /// Show a file's / folder's explanation (Cmd+click in the tree).
    Show(explain::Node),
    /// Re-explain the node in the open explanation modal: it, and what quotes
    /// its summary (`explain::Reuse::Node`).
    ReexplainNode,
    /// Show (or generate on demand) a function's block-by-block walkthrough.
    Blocks(explain::Node),
    /// The block walkthrough for `node` finished (or failed).
    BlocksExplained {
        stamp: Stamp,
        node: explain::Node,
        detail: Result<String, String>,
    },
    /// This window's changes were merged into the shared derived store off
    /// the UI thread. `merged` is the store's content afterwards (other
    /// windows' summaries too); it is adopted only when it was saved, and
    /// while the in-memory cache is still the one the merge started from
    /// (`seq` = `ExplainState::cache_seq` at dispatch) — a newer in-memory
    /// change must not be rolled back by an older merge.
    Persisted {
        stamp: Stamp,
        seq: u64,
        merged: Handoff<explain::Cache>,
        /// How many summaries the saved file left out to fit its size cap
        /// (`explain::edit`), or why nothing was saved.
        saved: Result<usize, String>,
        /// The saves this was the last of (`ExplainState::save`): what they
        /// could not save waits in it, for the window to take back.
        chain: crate::app::state::SaveChain,
    },
}

/// Rich content: rendered math and diagrams, and links clicked in it.
#[derive(Debug, Clone)]
pub enum ContentMsg {
    /// A background pass finished rendering math/mermaid blocks to SVG: what
    /// rendered, and what failed (each with a placeholder to show instead).
    SvgsGenerated {
        stamp: Stamp,
        generation: u64,
        map: SvgBatch,
    },
    /// A markdown link in an explanation was clicked.
    OpenLink(String),
}

/// The overview and stats homes.
#[derive(Debug, Clone)]
pub enum OverviewMsg {
    /// Show the architecture overview "home" in the main area.
    Show,
    /// Generate (or regenerate) the architecture overview.
    Generate,
    /// Show the code-statistics "home" in the main area (computes if stale).
    ShowStats,
    /// Recompute the code statistics regardless of freshness (the Refresh button).
    RefreshStats,
    /// A stats computation finished for the project its stamp names — or
    /// failed, which is not an empty report (see `on_stats_done`).
    StatsDone {
        stamp: Stamp,
        /// The registry revision the run was started at.
        rev: u64,
        report: Result<stats::StatsReport, String>,
    },
    /// The overview finished generating. `seq` names the generation request
    /// (`OverviewState::seq`); an older request that lands after a newer one
    /// was started is dropped without touching the busy flag the newer one
    /// owns.
    Done {
        stamp: Stamp,
        seq: u64,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    },
}

/// Walkthroughs: generating, saving and reading tours.
#[derive(Debug, Clone)]
pub enum WalkMsg {
    /// Generate a new walkthrough for `scope` (empty = the whole codebase). The
    /// result is upserted into the library by scope, then opened.
    Generate(String),
    /// Generate a narrated walkthrough of the current branch/PR diff (or the last
    /// commit when there's no base branch). Upserted into the library like a tour.
    GenerateDiff,
    /// Make a walkthrough of the last debug run, from its trace.
    GenerateTrace,
    /// Regenerate the library tour with this scope (tours are keyed by the
    /// scope they were generated for; an index into this window's copy of
    /// the library can shift when another window saves a tour).
    Regenerate(String),
    /// Delete the library tour with this scope and persist the smaller library.
    Delete(String),
    /// A walkthrough finished generating; `scope` keys the upsert into the
    /// library, the stamp names the project it was generated for (a slow
    /// generation must not be saved into another project's library).
    Done {
        stamp: Stamp,
        /// The generation request this answers (`WalkState::pending`). Only
        /// the awaited one may clear the busy row, retry, or open the tour; a
        /// superseded one is still saved into the library (it was paid for).
        seq: u64,
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    },
    /// Open the library tour with this scope for reading.
    Open(String),
    /// Return from a tour to the library list.
    Back,
    /// Flip the top input between searching the library and generating a tour.
    ToggleMode,
    /// Jump to step `step` of the tour with this scope — ignored unless that
    /// tour is the one open (a step index means nothing in another tour).
    Goto { scope: String, step: usize },
    /// Move by a relative offset (Next / Prev).
    Step(i32),
    /// The top input (search query / scope prompt) changed.
    InputChanged(String),
    /// Drag the divider between the WALK steps list and the narration.
    ResizeNarration(f32),
}

/// Semantic search and its embedding index.
#[derive(Debug, Clone)]
pub enum SemanticMsg {
    /// Build / refresh the semantic embedding index.
    BuildIndex,
    /// The embedding index finished building — or did not, and hands back
    /// the index it started from.
    IndexBuilt {
        stamp: Stamp,
        result: Result<embed::Index, crate::app::tasks::EmbedBuildFailed>,
    },
    /// A finished build was merged into the stored index off the UI thread.
    /// `saved` reports whether the merged file reached the disk; the merged
    /// index is usable for the session either way.
    IndexMerged {
        stamp: Stamp,
        index: Handoff<embed::Index>,
        saved: Result<(), String>,
    },
    /// The Semantic-tab query text changed.
    QueryChanged(String),
    /// Run the semantic search for the current query.
    Search,
    /// The query's embedding vector (or an error); the handler ranks the index.
    /// `seq` is the search submission it answers (`ProjectSession::semantic_seq`): only
    /// the latest may paint results or clear the spinner.
    Results {
        stamp: Stamp,
        seq: u64,
        query: String,
        result: Result<Vec<f32>, String>,
    },
    /// Open a semantic result: jump to the function/file in the code.
    OpenNode(explain::Node),
}

/// The Ask panel: questions, answers streaming in, pinned selections.
#[derive(Debug, Clone)]
pub enum AskMsg {
    /// Toolbar "Ask": open the bottom panel on the Ask tab, or collapse it.
    Toggle,
    /// The Ask input box text changed.
    InputChanged(String),
    /// Submit the current question.
    Submit,
    /// Ask a suggested (context-aware) question: fill the input and submit it.
    Suggested(String),
    /// The question's embedding vector came back (retrieval step). The stamp
    /// names the project the question was asked in.
    Retrieved {
        stamp: Stamp,
        /// The stream id minted for this question at submit — its identity
        /// until the answer streams (`InFlight::ask_retrieval`): a question
        /// stopped, cleared or superseded while being embedded is dropped.
        stream: u64,
        question: String,
        qvec: Result<Vec<f32>, String>,
    },
    /// The context for the retrieval-mode answer on `stream` was assembled off
    /// the UI thread (source bodies read and parsed). The stream is started
    /// only while its turn is still open — Stop, Ask Clear, or a project
    /// switch in between closes the turn and this is dropped unsent.
    ContextReady {
        stamp: Stamp,
        stream: u64,
        messages: Vec<llm::ChatMsg>,
    },
    /// A streamed token for the answer on `stream` — appended to THAT turn.
    /// The id is the identity check: a delta from a superseded stream (a
    /// project switch, a newer question, Ask Clear) matches no open turn and
    /// is dropped, instead of being appended to whatever turn happens to be
    /// streaming now.
    Delta {
        stamp: Stamp,
        stream: u64,
        text: String,
    },
    /// The streamed answer on `stream` ended — finished, stopped on request,
    /// or failed. Identified like [`AskMsg::Delta`].
    StreamEnded {
        stamp: Stamp,
        stream: u64,
        outcome: clew_protocol::StreamOutcome,
    },
    /// The agent on `stream` made a tool call — append a step chip to THAT turn.
    AgentStepped {
        stamp: Stamp,
        stream: u64,
        step: AgentStep,
    },
    /// The agent turn on `stream` ended (see [`AskMsg::StreamEnded`]).
    AgentTurnEnded {
        stamp: Stamp,
        stream: u64,
        outcome: clew_protocol::StreamOutcome,
    },
    /// Stop the in-flight Ask answer, whatever kind it is: an agent turn (on
    /// the server), a streamed answer on the server, or one streaming from
    /// the provider locally (the client AI endpoint). See
    /// [`crate::App::ask_stream_active`] for when a Stop button applies.
    Stop,
    /// Clear the whole Ask conversation.
    Clear,
    /// Remove the pinned code-selection context with this identity
    /// ([`crate::AskPin::key`]).
    Unpin(u64),
    /// Jump to the pinned selection with this identity (open its file at its
    /// line).
    PinGoto(u64),
    /// Add the current code selection as a context chip and open the Ask panel.
    AboutSelection,
}

/// Git history in the reader: time travel and "Why is this here?".
#[derive(Debug, Clone)]
pub enum TimeTravelMsg {
    /// Explain why the right-clicked line (or selection) exists, from git blame.
    WhyIsThisHere,
    /// The "why is this here?" answer finished generating.
    BlameWhyDone {
        stamp: Stamp,
        /// Which request this answers (see
        /// [`crate::app::model::BlameWhy::token`]).
        token: u64,
        title: String,
        commits: Vec<(String, String)>,
        result: Result<String, String>,
    },
    /// Close the "why is this here?" popup.
    BlameWhyClose,
    /// Enter git time travel for the active file; `symbol` scopes it to the
    /// function under the cursor.
    Start { symbol: bool },
    /// Commits for a time-travel session finished loading.
    Ready {
        stamp: Stamp,
        generation: u64,
        abs: PathBuf,
        rel: String,
        lang: Option<&'static str>,
        scope: TimeScope,
        /// `Err` = the history could not be read (git failed, the transport
        /// refused) — not the same answer as an empty history.
        commits: Result<Vec<git::HistCommit>, String>,
    },
    /// Scrub to commit index `idx` (0 = newest).
    Goto(usize),
    /// The historical content for a step finished loading (`Err` = git could
    /// not produce that revision).
    Step {
        stamp: Stamp,
        generation: u64,
        idx: usize,
        step: Result<Box<TimeStep>, String>,
    },
    /// The historical view scrolled (tracked for its sticky headers).
    Scrolled(scrollable::Viewport),
    /// Place the caret / begin a selection in the historical (read-only) view.
    SelectStart { line: usize, col: usize },
    /// Extend the historical view's selection while dragging.
    SelectDrag { line: usize, col: usize },
    /// Switch a session between whole-file and the current function's scope.
    ToggleScope,
    /// Generate the LLM "what & why" summary of the current commit's diff.
    Why,
    /// A "what & why" summary finished for commit `sha`. `session` is the
    /// time-travel SESSION it was asked in ([`crate::TimeTravel::session`]),
    /// not the per-step load generation: scrubbing to another commit while
    /// the summary is generated must not drop it (and strand the spinner).
    WhyDone {
        stamp: Stamp,
        session: u64,
        sha: String,
        result: Result<String, String>,
    },
    /// Generate the "story of this function" narrative (symbol scope).
    Story,
    /// The story finished. `session` names the session in the scope it was
    /// asked in ([`crate::TimeTravel::scoped`]), which a re-scope moves on:
    /// a story tells that scope's commits.
    StoryDone {
        stamp: Stamp,
        session: u64,
        result: Result<String, String>,
    },
    /// Leave time travel, back to the live file.
    Exit,
}

/// The debugger: sessions, adapter events, breakpoints and watches.
#[derive(Debug, Clone)]
pub enum DebugMsg {
    /// Start (or restart) a debug session from the project's launch config.
    Start,
    /// The adapter is ready; carry its handle + the TCP address it announced
    /// (for child sessions).
    /// Every DAP-side message names the `run` it belongs to (`App::debug_run`):
    /// a late event from a stopped session must not land on the next one.
    DapStarted {
        stamp: Stamp,
        run: u64,
        client: dap::DapClient,
        addr: Option<std::net::SocketAddr>,
        /// The adapter's `supportsEvaluateForHovers` capability.
        hover_safe: bool,
    },
    /// A child session (js-debug) is ready; it becomes the active client.
    DapChildStarted {
        stamp: Stamp,
        run: u64,
        client: dap::DapClient,
        /// The child's own `supportsEvaluateForHovers` promise.
        hover_safe: bool,
    },
    /// An event pushed from the debug adapter.
    DapEvent {
        stamp: Stamp,
        run: u64,
        event: dap::DapEvent,
    },
    /// The stopped frame's stack + scopes/variables finished loading.
    /// `stop` is the `App::debug_stop` the fetch was issued at (checked with
    /// `owns_debug_stop`): within one run the program may already have
    /// continued or stopped again, and this describes the older pause.
    DapStopInspected {
        stamp: Stamp,
        run: u64,
        stop: u64,
        frames: Vec<dap::StackFrame>,
        scopes: Vec<DebugScope>,
    },
    /// The adapter answered a `setBreakpoints`. Each entry pairs one file with
    /// the adapter's verdict on the lines we sent for it, already joined back
    /// to the requested line (`dap::Breakpoint::requested_line`).
    ///
    /// Carried back rather than discarded because the answer is the only thing
    /// that distinguishes a breakpoint that will fire from one the adapter
    /// refused, and the gutter draws from `App::debug.breakpoints`.
    DapBreakpointsAnswered {
        stamp: Stamp,
        run: u64,
        answers: Vec<(PathBuf, Result<Vec<dap::Breakpoint>, String>)>,
    },
    /// Stepping / continue control.
    Control(DebugCmd),
    /// End the debug session.
    Stop,
    /// Toggle a breakpoint at (file, 1-based line) — from the code context menu.
    BreakpointToggle { path: PathBuf, line: usize },
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
    WatchInput(String),
    /// Add the current input as a watch expression.
    WatchAdd,
    /// Remove a watch expression by identity (its text), from both the watch
    /// list and the last stop's evaluated values.
    WatchRemoveExpr(String),
    /// Toggle evaluating the hovered identifier in the paused debuggee. Off by
    /// default: an evaluation can run code (property getters, `Debug`
    /// impls) in the program being debugged, so it is opt-in.
    ToggleHoverEval,
    /// Watch expressions finished evaluating: (expression, value) pairs.
    /// `stop` names the pause they were read in — see `DapStopInspected`.
    WatchesEvaluated {
        stamp: Stamp,
        run: u64,
        stop: u64,
        vals: Vec<(String, String)>,
    },
    /// Starting the debugger failed: the run's own start, which ends the run
    /// (a js-debug child that cannot start is `ChildFailed`).
    Failed {
        stamp: Stamp,
        run: u64,
        error: String,
    },
    /// A js-debug child session could not start: connecting to the adapter
    /// or its `initialize` failed. Only that child is over, unless no child
    /// of the run has started yet — then it was the run's real target, and
    /// the run fails with it (see `App::on_debug_child_failed`).
    ChildFailed {
        stamp: Stamp,
        run: u64,
        error: String,
    },
    /// The debug adapter this run needs must be installed first: raises the
    /// consent modal (nothing is installed without the user's approval).
    NeedsInstall {
        stamp: Stamp,
        run: u64,
        install: dap::AdapterInstall,
    },
    /// A consented debug-adapter install finished; the stamp names the
    /// project instance the debug session was for (it starts only if that
    /// instance is still the open one — decided by the handler, since the
    /// install itself is reported whatever the project).
    AdapterInstalled {
        stamp: Stamp,
        result: Result<(), String>,
    },
}

/// Language servers: starting and provisioning them, inlay hints, the Language Servers panel.
#[derive(Debug, Clone)]
pub enum LspMsg {
    /// Inlay hints came back from the language server for `abs`.
    InlayHintsLoaded {
        stamp: Stamp,
        abs: PathBuf,
        /// The value of `inlay_gen` when the request went out. Toggling hints
        /// off bumps it, so a reply already in flight is recognized as
        /// belonging to the previous "on" period and dropped — clearing the
        /// hints on toggle was not enough, the late reply put them straight
        /// back while the toggle read as off.
        hint_gen: u64,
        /// Hash of the bytes the request was computed against. Hints carry
        /// line/character positions, so applying a reply to any other content
        /// puts every chip on the wrong token — and `hint_gen` cannot catch
        /// that, because editing the file does not touch it.
        src_hash: crate::incremental::Version,
        hints: Vec<lsp::client::InlayHint>,
    },
    StartResult {
        stamp: Stamp,
        language: String,
        /// Spawn generation minted when this start began; a result from a
        /// superseded spawn (restart, project switch) is dropped.
        generation: u64,
        result: Result<lsp::client::LspClient, String>,
    },
    ConsentAllowed,
    /// Approve running the language-server command the project's `lsp.toml`
    /// names (recorded against its fingerprint, so an edit re-asks).
    CommandAllowed,
    /// Decline the project's language-server command.
    CommandDismissed,
    ConsentDismissed,
    DownloadResult {
        stamp: Stamp,
        language: String,
        /// Spawn generation minted when the install began (same space as
        /// `LspMsg::StartResult::generation`).
        generation: u64,
        result: Result<PathBuf, String>,
    },
    /// The off-thread staging of a repository-named language-server command
    /// (`lsp.toml` `command`) finished: its bytes were hashed and, when that
    /// fingerprint is already approved, copied to clew's private exec store.
    /// Applied only while the slot is still waiting for THIS staging
    /// (`generation`, same space as `LspMsg::StartResult::generation`) in the same
    /// project instance, over the same transport (its stamp).
    Staged {
        stamp: Stamp,
        language: String,
        generation: u64,
        result: Result<LspStagedCommand, String>,
    },
    /// The Language Servers panel's background listing finished: the store's
    /// installed servers (with their on-disk sizes) and, per language, where
    /// its server would come from. `seq` retires a listing a newer one
    /// superseded; the stamp, one made for another project instance — whose
    /// lsp.toml and host (this machine or a remote) the rows were resolved
    /// against.
    PanelListed {
        stamp: Stamp,
        seq: u64,
        installed: Vec<lsp::store::InstalledServer>,
        located: HashMap<String, LocatedKind>,
    },
    /// A server was removed from the store off the UI thread (`Err` = why
    /// not).
    Removed {
        name: String,
        version: String,
        result: Result<(), String>,
    },
    /// Toggle LSP inlay hints (inferred types, parameter names).
    ToggleInlayHints,
    TogglePanel,
    Restart(String),
    Remove {
        name: String,
        version: String,
    },
    DownloadFor(String),
}

/// The DOCS tab.
#[derive(Debug, Clone)]
pub enum DocsMsg {
    /// (Re)build the project's API docs from the server.
    Refresh,
    /// Expand / collapse a file group in the DOCS tree.
    ToggleFile(String),
    /// Filter the DOCS tree by item name.
    FilterChanged(String),
    /// Toggle showing all symbols vs. only the public API.
    ToggleShowAll,
    /// Toggle grouping the Docs tree by module/package vs. by file.
    ToggleGrouping,
    /// Open the doc page for the item at (file rel, definition line).
    Select { rel: String, line: usize },
    /// Open the doc page for the symbol under the cursor (from the code view's
    /// right-click menu).
    ViewFromMenu,
}

/// The settings modal and the appearance.
#[derive(Debug, Clone)]
pub enum SettingsMsg {
    /// Set the Light/Dark/System appearance preference.
    SetThemePref(theme::ThemePref),
    /// The OS light/dark appearance changed (only acted on when following System).
    SystemAppearanceChanged,
    /// Another window changed the appearance. The palette is process-global,
    /// so this window is already being painted in the new colors — what it
    /// still has to do is adopt the new preference and re-color its own
    /// cached diagram SVGs, which are per-window and would otherwise stay
    /// rendered in the previous palette forever.
    ThemeResynced,
    /// Pick the theme used for the light or dark slot (`is_light` selects which).
    SetThemeVariant {
        id: &'static str,
        is_light: bool,
    },
    /// Embedding settings draft edits.
    EmbedKeyChanged(String),
    EmbedModelChanged(String),
    EmbedBaseUrlChanged(String),
    /// Open / close the LLM settings modal.
    Open,
    Close,
    /// LLM settings draft edits.
    ProviderPicked(llm::Provider),
    KeyChanged(String),
    ModelChanged(String),
    BaseUrlChanged(String),
    /// Save the LLM settings to the global config.
    Saved,
}

/// Auto-update: checking, downloading and installing a release.
#[derive(Debug, Clone)]
pub enum UpdaterMsg {
    /// Manually check for updates (from the menu).
    Check,
    /// A version check finished. `manual` means announce the result even when
    /// already up to date.
    Checked {
        manual: bool,
        result: Result<clew_core::update::Release, String>,
    },
    /// Dismiss the "update available" banner for this session.
    BannerDismissed,
    /// Open the release-notes modal.
    ShowNotes,
    /// Close the release-notes modal.
    CloseNotes,
    /// Start downloading and installing the available update.
    InstallStart,
    /// Stop the download in progress (the banner's Cancel).
    CancelDownload,
    /// Streamed DMG download progress.
    DownloadProgress {
        generation: u64,
        done: u64,
        total: Option<u64>,
    },
    /// The DMG finished downloading (or failed).
    Downloaded {
        generation: u64,
        result: Result<PathBuf, String>,
    },
    /// The bundle swap and relauncher finished (or failed). `Ok` means quit so
    /// the detached helper can complete.
    Installed(Result<(), String>),
    /// Toggle the "check for updates automatically" preference.
    SetAuto(bool),
}

/// The window and its chrome: controls, layout, panels, menus, the shortcuts modal.
#[derive(Debug, Clone)]
pub enum WindowMsg {
    SidebarTabPicked(SidebarTab),
    /// Show / hide the left sidebar.
    ToggleLeftSidebar,
    /// Show / hide the right sidebar (Outline / Explain tabs).
    ToggleRightPanel,
    Resized(Size),
    /// Start dragging the whole window (from the custom title-bar region).
    TitleBarDragged,
    /// The window gained or lost focus (greys out the custom controls).
    FocusChanged(bool),
    /// The pointer entered or left the traffic-light cluster (shows the icons).
    ControlsHover(bool),
    /// Custom window controls (frameless window has no OS buttons).
    Close,
    Minimize,
    ToggleFullscreen,
    /// Drag a panel divider: the payload is the cursor's absolute x (sidebar /
    /// right panel) or y (bottom panel). See [`resize::Divider`].
    ResizeSidebar(f32),
    ResizeRight(f32),
    ResizeBottom(f32),
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
}

/// The guided tour.
#[derive(Debug, Clone)]
pub enum TutorialMsg {
    /// Start the interactive tutorial (from the More menu).
    Start,
    /// Move the tutorial by `delta` steps (+1 next, -1 back); past the end ends it.
    Step(i32),
    /// End the tutorial.
    Exit,
}

impl Message {
    /// What `dispatch` checks this message against before any handler sees
    /// it: the stamp of an async result, the transport instance of a
    /// transport event, or `None` for a message that is not the answer to
    /// anything in flight (a user action, a window event, a window-level
    /// result such as the updater's or the server-store listing). Each
    /// feature classifies its own messages, exhaustively.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            Message::Server(m) => m.origin(),
            Message::Connect(m) => m.origin(),
            Message::Project(m) => m.origin(),
            Message::Watch(m) => m.origin(),
            Message::Editor(m) => m.origin(),
            Message::Hover(m) => m.origin(),
            Message::Nav(m) => m.origin(),
            Message::Reading(m) => m.origin(),
            Message::Calls(m) => m.origin(),
            Message::Graph(m) => m.origin(),
            Message::Explain(m) => m.origin(),
            Message::Content(m) => m.origin(),
            Message::Overview(m) => m.origin(),
            Message::Walk(m) => m.origin(),
            Message::Semantic(m) => m.origin(),
            Message::Ask(m) => m.origin(),
            Message::TimeTravel(m) => m.origin(),
            Message::Debug(m) => m.origin(),
            Message::Lsp(m) => m.origin(),
            Message::Docs(m) => m.origin(),
            Message::Settings(m) => m.origin(),
            Message::Updater(m) => m.origin(),
            Message::Window(m) => m.origin(),
            Message::Tutorial(m) => m.origin(),
            Message::Tick | Message::Noop => None,
        }
    }
}

impl ServerMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ServerMsg::RegisterProcFeed { stamp, .. } | ServerMsg::ResendNotReady { stamp, .. } => {
                Some(Origin::Stamped(stamp))
            }
            ServerMsg::Connected { conn, .. }
            | ServerMsg::Unavailable { conn, .. }
            | ServerMsg::HostKeyUnknown { conn, .. }
            | ServerMsg::HostKeyChanged { conn, .. }
            | ServerMsg::Event { conn, .. }
            | ServerMsg::Disconnected { conn, .. } => Some(Origin::Transport(*conn)),
        }
    }
}

impl ConnectMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ConnectMsg::Open
            | ConnectMsg::Close
            | ConnectMsg::Field(..)
            | ConnectMsg::PickIdentity
            | ConnectMsg::IdentityPicked(..)
            | ConnectMsg::ToggleAiKeys(..)
            | ConnectMsg::RevokeAiKeys
            | ConnectMsg::Submit
            | ConnectMsg::TrustHost { .. }
            | ConnectMsg::TrustCancel
            | ConnectMsg::ForgetHostKey { .. }
            | ConnectMsg::ToSaved { .. }
            | ConnectMsg::RemoveSaved { .. }
            | ConnectMsg::Disconnect
            | ConnectMsg::BrowseTo(..)
            | ConnectMsg::OpenHere => None,
        }
    }
}

impl ProjectMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ProjectMsg::ScanDone { stamp, .. }
            | ProjectMsg::StateLoaded { stamp, .. }
            | ProjectMsg::TreeUpdated { stamp, .. } => Some(Origin::Stamped(stamp)),
            ProjectMsg::OpenFolderPressed
            | ProjectMsg::FolderPicked(..)
            | ProjectMsg::ConsentAllowed
            | ProjectMsg::ConsentDenied
            | ProjectMsg::ToggleDir(..) => None,
        }
    }
}

impl WatchMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            WatchMsg::SymbolIndexDone { stamp, .. }
            | WatchMsg::StructureBuilt { stamp, .. }
            | WatchMsg::FilesRehashed { stamp, .. }
            | WatchMsg::FilesIndexed { stamp, .. } => Some(Origin::Stamped(stamp)),
        }
    }
}

impl EditorMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            EditorMsg::FileLoaded { stamp, .. }
            | EditorMsg::HeldOpenFailed { stamp, .. }
            | EditorMsg::Highlighted { stamp, .. }
            | EditorMsg::GitInfoLoaded { stamp, .. }
            | EditorMsg::DiffLoaded { stamp, .. } => Some(Origin::Stamped(stamp)),
            EditorMsg::OpenRel { .. }
            | EditorMsg::OpenAbs { .. }
            | EditorMsg::Scrolled(..)
            | EditorMsg::PaneFocused(..)
            | EditorMsg::ToggleSplit
            | EditorMsg::SelectStart { .. }
            | EditorMsg::SelectDrag { .. }
            | EditorMsg::SelectEnd
            | EditorMsg::CopySelection
            | EditorMsg::FoldToggle { .. }
            | EditorMsg::MinimapScrolled { .. }
            | EditorMsg::ReaderScrolled(..)
            | EditorMsg::ToggleDiff
            | EditorMsg::OutlineJump(..)
            | EditorMsg::FontSizeDelta(..)
            | EditorMsg::FontSizeReset
            | EditorMsg::KeyPressed(..)
            | EditorMsg::RunAction(..)
            | EditorMsg::ModifiersChanged(..)
            | EditorMsg::FindOpened
            | EditorMsg::FindQueryChanged(..)
            | EditorMsg::FindStep(..)
            | EditorMsg::FindClosed
            | EditorMsg::SkimFile
            | EditorMsg::NbToggleOutputs { .. }
            | EditorMsg::NbExpandAll { .. }
            | EditorMsg::ToggleMarkdownSource(..) => None,
        }
    }
}

impl HoverMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            HoverMsg::Dwell { stamp, .. } | HoverMsg::Answered { stamp, .. } => {
                Some(Origin::Stamped(stamp))
            }
            HoverMsg::ContextMenuOpened { .. }
            | HoverMsg::ContextMenuClosed
            | HoverMsg::ContextGoto(..)
            | HoverMsg::Requested { .. }
            | HoverMsg::Cleared
            | HoverMsg::Pin(..) => None,
        }
    }
}

impl NavMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            NavMsg::SearchDone { stamp, .. }
            | NavMsg::DefinitionResult { stamp, .. }
            | NavMsg::ReferencesResult { stamp, .. }
            | NavMsg::LocationPreviews { stamp, .. } => Some(Origin::Stamped(stamp)),
            NavMsg::SearchQueryChanged(..)
            | NavMsg::SearchSubmitted
            | NavMsg::SearchToggle(..)
            | NavMsg::SearchIncludeChanged(..)
            | NavMsg::SearchExcludeChanged(..)
            | NavMsg::FinderOpened(..)
            | NavMsg::FinderClosed
            | NavMsg::FinderQueryChanged(..)
            | NavMsg::FinderPick { .. }
            | NavMsg::FinderConfirm
            | NavMsg::GotoLineRequested
            | NavMsg::JumpToCall { .. } => None,
        }
    }
}

impl ReadingMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ReadingMsg::BookmarkToggled
            | ReadingMsg::BookmarkRemoved { .. }
            | ReadingMsg::BookmarkNoteEdit(..)
            | ReadingMsg::BookmarkNoteInput(..)
            | ReadingMsg::BookmarkNoteSave
            | ReadingMsg::BookmarkNoteCancel
            | ReadingMsg::NoteToggleUnderstood { .. }
            | ReadingMsg::NoteEditStart { .. }
            | ReadingMsg::NoteEditInput(..)
            | ReadingMsg::NoteEditSave
            | ReadingMsg::NoteEditCancel
            | ReadingMsg::NoteRemove { .. }
            | ReadingMsg::NoteJump { .. }
            | ReadingMsg::GoBack
            | ReadingMsg::GoForward
            | ReadingMsg::HistoryJump { .. }
            | ReadingMsg::TrailToggleCollapse { .. }
            | ReadingMsg::HistoryClear
            | ReadingMsg::TrailSaved { .. }
            | ReadingMsg::TargetSelected(..) => None,
        }
    }
}

impl CallsMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            CallsMsg::Prepared { stamp, .. } | CallsMsg::Children { stamp, .. } => {
                Some(Origin::Stamped(stamp))
            }
            CallsMsg::Requested
            | CallsMsg::FromMenu
            | CallsMsg::ExpandNode { .. }
            | CallsMsg::Direction
            | CallsMsg::ExpandAll => None,
        }
    }
}

impl GraphMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            GraphMsg::RemoteTsConfigsLoaded { stamp, .. }
            | GraphMsg::ImportGraphUpdated { stamp, .. }
            | GraphMsg::ProjectCallsBuilt { stamp, .. }
            | GraphMsg::ChurnLoaded { stamp, .. }
            | GraphMsg::GraphLaidOut { stamp, .. }
            | GraphMsg::RefineWaitOver { stamp, .. }
            | GraphMsg::RefineProgress { stamp, .. }
            | GraphMsg::ProjectCallsRefined { stamp, .. } => Some(Origin::Stamped(stamp)),
            GraphMsg::ImportExpand { .. }
            | GraphMsg::ImportDirection
            | GraphMsg::ImportExpandAll
            | GraphMsg::OpenOverlay(..)
            | GraphMsg::CloseOverlay
            | GraphMsg::OverlayOpenImports(..)
            | GraphMsg::OverlayOpenAt { .. }
            | GraphMsg::OverlayViewToggle
            | GraphMsg::Toggle3D
            | GraphMsg::ToggleSpin
            | GraphMsg::ToggleHeat
            | GraphMsg::RefineProjectCalls => None,
        }
    }
}

impl ExplainMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ExplainMsg::Progress { stamp, .. }
            | ExplainMsg::Done { stamp, .. }
            | ExplainMsg::BlocksExplained { stamp, .. }
            | ExplainMsg::Persisted { stamp, .. } => Some(Origin::Stamped(stamp)),
            ExplainMsg::FromMenu
            | ExplainMsg::Project
            | ExplainMsg::Refresh
            | ExplainMsg::Cancel
            | ExplainMsg::RefreshAll
            | ExplainMsg::Show(..)
            | ExplainMsg::ReexplainNode
            | ExplainMsg::Blocks(..) => None,
        }
    }
}

impl ContentMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            ContentMsg::SvgsGenerated { stamp, .. } => Some(Origin::Stamped(stamp)),
            ContentMsg::OpenLink(..) => None,
        }
    }
}

impl OverviewMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            OverviewMsg::StatsDone { stamp, .. } | OverviewMsg::Done { stamp, .. } => {
                Some(Origin::Stamped(stamp))
            }
            OverviewMsg::Show
            | OverviewMsg::Generate
            | OverviewMsg::ShowStats
            | OverviewMsg::RefreshStats => None,
        }
    }
}

impl WalkMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            WalkMsg::Done { stamp, .. } => Some(Origin::Stamped(stamp)),
            WalkMsg::Generate(..)
            | WalkMsg::GenerateDiff
            | WalkMsg::GenerateTrace
            | WalkMsg::Regenerate(..)
            | WalkMsg::Delete(..)
            | WalkMsg::Open(..)
            | WalkMsg::Back
            | WalkMsg::ToggleMode
            | WalkMsg::Goto { .. }
            | WalkMsg::Step(..)
            | WalkMsg::InputChanged(..)
            | WalkMsg::ResizeNarration(..) => None,
        }
    }
}

impl SemanticMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            SemanticMsg::IndexBuilt { stamp, .. }
            | SemanticMsg::IndexMerged { stamp, .. }
            | SemanticMsg::Results { stamp, .. } => Some(Origin::Stamped(stamp)),
            SemanticMsg::BuildIndex
            | SemanticMsg::QueryChanged(..)
            | SemanticMsg::Search
            | SemanticMsg::OpenNode(..) => None,
        }
    }
}

impl AskMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            AskMsg::Retrieved { stamp, .. }
            | AskMsg::ContextReady { stamp, .. }
            | AskMsg::Delta { stamp, .. }
            | AskMsg::StreamEnded { stamp, .. }
            | AskMsg::AgentStepped { stamp, .. }
            | AskMsg::AgentTurnEnded { stamp, .. } => Some(Origin::Stamped(stamp)),
            AskMsg::Toggle
            | AskMsg::InputChanged(..)
            | AskMsg::Submit
            | AskMsg::Suggested(..)
            | AskMsg::Stop
            | AskMsg::Clear
            | AskMsg::Unpin(..)
            | AskMsg::PinGoto(..)
            | AskMsg::AboutSelection => None,
        }
    }
}

impl TimeTravelMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            TimeTravelMsg::BlameWhyDone { stamp, .. }
            | TimeTravelMsg::Ready { stamp, .. }
            | TimeTravelMsg::Step { stamp, .. }
            | TimeTravelMsg::WhyDone { stamp, .. }
            | TimeTravelMsg::StoryDone { stamp, .. } => Some(Origin::Stamped(stamp)),
            TimeTravelMsg::WhyIsThisHere
            | TimeTravelMsg::BlameWhyClose
            | TimeTravelMsg::Start { .. }
            | TimeTravelMsg::Goto(..)
            | TimeTravelMsg::Scrolled(..)
            | TimeTravelMsg::SelectStart { .. }
            | TimeTravelMsg::SelectDrag { .. }
            | TimeTravelMsg::ToggleScope
            | TimeTravelMsg::Why
            | TimeTravelMsg::Story
            | TimeTravelMsg::Exit => None,
        }
    }
}

impl DebugMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    /// `AdapterInstalled` carries a stamp but is not checked here: the install
    /// it reports is global and is announced whatever the project, and only
    /// its follow-up (starting the session) is project-scoped, which its
    /// handler decides from the stamp.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            DebugMsg::DapStarted { stamp, .. }
            | DebugMsg::DapChildStarted { stamp, .. }
            | DebugMsg::DapEvent { stamp, .. }
            | DebugMsg::DapStopInspected { stamp, .. }
            | DebugMsg::DapBreakpointsAnswered { stamp, .. }
            | DebugMsg::WatchesEvaluated { stamp, .. }
            | DebugMsg::Failed { stamp, .. }
            | DebugMsg::ChildFailed { stamp, .. }
            | DebugMsg::NeedsInstall { stamp, .. } => Some(Origin::Stamped(stamp)),
            DebugMsg::Start
            | DebugMsg::Control(..)
            | DebugMsg::Stop
            | DebugMsg::BreakpointToggle { .. }
            | DebugMsg::ToggleBreakpointFromMenu
            | DebugMsg::ConditionalBreakpointFromMenu
            | DebugMsg::BpConditionInput(..)
            | DebugMsg::BpConditionSet
            | DebugMsg::BpConditionCancel
            | DebugMsg::WatchInput(..)
            | DebugMsg::WatchAdd
            | DebugMsg::WatchRemoveExpr(..)
            | DebugMsg::ToggleHoverEval
            | DebugMsg::AdapterInstalled { .. } => None,
        }
    }
}

impl LspMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            LspMsg::InlayHintsLoaded { stamp, .. }
            | LspMsg::StartResult { stamp, .. }
            | LspMsg::DownloadResult { stamp, .. }
            | LspMsg::Staged { stamp, .. }
            | LspMsg::PanelListed { stamp, .. } => Some(Origin::Stamped(stamp)),
            LspMsg::ConsentAllowed
            | LspMsg::CommandAllowed
            | LspMsg::CommandDismissed
            | LspMsg::ConsentDismissed
            | LspMsg::Removed { .. }
            | LspMsg::ToggleInlayHints
            | LspMsg::TogglePanel
            | LspMsg::Restart(..)
            | LspMsg::Remove { .. }
            | LspMsg::DownloadFor(..) => None,
        }
    }
}

impl DocsMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            DocsMsg::Refresh
            | DocsMsg::ToggleFile(..)
            | DocsMsg::FilterChanged(..)
            | DocsMsg::ToggleShowAll
            | DocsMsg::ToggleGrouping
            | DocsMsg::Select { .. }
            | DocsMsg::ViewFromMenu => None,
        }
    }
}

impl SettingsMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            SettingsMsg::SetThemePref(..)
            | SettingsMsg::SystemAppearanceChanged
            | SettingsMsg::ThemeResynced
            | SettingsMsg::SetThemeVariant { .. }
            | SettingsMsg::EmbedKeyChanged(..)
            | SettingsMsg::EmbedModelChanged(..)
            | SettingsMsg::EmbedBaseUrlChanged(..)
            | SettingsMsg::Open
            | SettingsMsg::Close
            | SettingsMsg::ProviderPicked(..)
            | SettingsMsg::KeyChanged(..)
            | SettingsMsg::ModelChanged(..)
            | SettingsMsg::BaseUrlChanged(..)
            | SettingsMsg::Saved => None,
        }
    }
}

impl UpdaterMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            UpdaterMsg::Check
            | UpdaterMsg::Checked { .. }
            | UpdaterMsg::BannerDismissed
            | UpdaterMsg::ShowNotes
            | UpdaterMsg::CloseNotes
            | UpdaterMsg::InstallStart
            | UpdaterMsg::CancelDownload
            | UpdaterMsg::DownloadProgress { .. }
            | UpdaterMsg::Downloaded { .. }
            | UpdaterMsg::Installed(..)
            | UpdaterMsg::SetAuto(..) => None,
        }
    }
}

impl WindowMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            WindowMsg::SidebarTabPicked(..)
            | WindowMsg::ToggleLeftSidebar
            | WindowMsg::ToggleRightPanel
            | WindowMsg::Resized(..)
            | WindowMsg::TitleBarDragged
            | WindowMsg::FocusChanged(..)
            | WindowMsg::ControlsHover(..)
            | WindowMsg::Close
            | WindowMsg::Minimize
            | WindowMsg::ToggleFullscreen
            | WindowMsg::ResizeSidebar(..)
            | WindowMsg::ResizeRight(..)
            | WindowMsg::ResizeBottom(..)
            | WindowMsg::BottomTabPicked(..)
            | WindowMsg::CollapseBottom
            | WindowMsg::ToggleToolsMenu
            | WindowMsg::ToggleTargetMenu
            | WindowMsg::OpenShortcuts
            | WindowMsg::CloseShortcuts
            | WindowMsg::RebindStart(..)
            | WindowMsg::RebindReset(..)
            | WindowMsg::RebindResetAll
            | WindowMsg::ToggleInlineSummaries
            | WindowMsg::ToggleFileBanner
            | WindowMsg::ToggleMinimap => None,
        }
    }
}

impl TutorialMsg {
    /// See [`Message::origin`]. Exhaustive on purpose — no wildcard arm — so a
    /// new variant cannot compile until it is classified here.
    pub fn origin(&self) -> Option<Origin<'_>> {
        match self {
            TutorialMsg::Start | TutorialMsg::Step(..) | TutorialMsg::Exit => None,
        }
    }
}

/// The identity of what an asynchronous result was produced FOR: the project
/// instance open when its work was spawned and, for work that dies with its
/// transport, the transport instance it ran over.
///
/// Every async-result `Message` carries one (the `stamp` field), minted by
/// [`crate::App::stamp`] or [`crate::App::transport_stamp`] at the moment the
/// work is spawned, and `dispatch` checks it ONCE, before the message reaches
/// its handler (see [`Message::origin`] and `App::owns`). A result from a
/// project the window has since left — or opened again: a reopen is a new
/// instance — or, when transport-bound, from a transport that has since died
/// or been replaced, is dropped right there. Handlers keep only their
/// feature-local sequence checks, which order work WITHIN one project
/// instance (a newer search superseding an older one, a restarted server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The open project's root at spawn (`None`: none was open). Implied by
    /// `epoch` (every install and every transport switch mints a new one);
    /// carried so a stamp names what it belongs to on its own.
    pub root: Option<PathBuf>,
    /// `App::project_epoch` at spawn: the project instance.
    pub epoch: u64,
    /// `App::conn_gen` at spawn, for work that runs over — and dies with —
    /// the transport (a proxied process, a language server, a scan the
    /// transport parked); `None` for work that stays valid across a
    /// reconnect of the same project.
    pub conn: Option<u64>,
}

/// What `dispatch` checks an asynchronous message against (see
/// [`Message::origin`]).
#[derive(Debug, Clone, Copy)]
pub enum Origin<'a> {
    /// Project-scoped work, stamped when it was spawned.
    Stamped(&'a Stamp),
    /// A transport event, named by the transport instance (`conn_gen`) that
    /// emitted it: a late event from a dead or replaced transport must not
    /// act on the current one.
    Transport(u64),
}

/// A one-shot, cloneable carrier for a payload that is neither `Clone` nor
/// cheap to clone. `Message` must be `Clone` + `Debug`, but a large result
/// (a whole explanation cache, a project's loaded state) is only ever
/// consumed once, by one handler: that handler takes it out with
/// [`Handoff::take`]. A clone of the message shares the same slot, so the
/// payload is still moved exactly once and never copied.
pub struct Handoff<T>(std::sync::Arc<std::sync::Mutex<Option<T>>>);

impl<T> Handoff<T> {
    pub fn new(value: T) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(Some(value))))
    }

    /// The payload, the first time only (`None` once taken).
    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl<T> Clone for Handoff<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> std::fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handoff(..)")
    }
}
