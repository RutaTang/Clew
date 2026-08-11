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
    /// Abort handle for the running LSP refine. The pass holds CLONES of the
    /// project's language-server clients, so dropping `App::lsp` does not
    /// stop it — it keeps querying servers for a project we have left.
    pub refine_abort: Option<iced::task::Handle>,
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
    /// The key in effect came from the environment, so the field above is
    /// deliberately BLANK rather than pre-filled with it. Set in
    /// `on_open_settings`; the form uses it only to say which variable it is
    /// deferring to, so an empty field does not read as "your key is gone".
    ///
    /// Pre-filling the resolved key was a leak: an environment key is only
    /// allowed to reach its own provider's endpoint, but saving the form turns
    /// whatever is in the field into a STORED key, and a stored key follows
    /// `base_url` anywhere.
    pub key_from_env: bool,
    /// As `key_from_env`, for the embeddings endpoint (`OPENAI_API_KEY`).
    pub embed_key_from_env: bool,
    /// The appearance captured when the modal opened: (mode, light-theme id,
    /// dark-theme id). Theme changes in the modal preview live but only commit on
    /// Save; Close restores this snapshot. Set in `on_open_settings`.
    pub theme_snapshot: (theme::ThemePref, &'static str, &'static str),
    /// The two AI configs as the modal pre-filled them, so Save can write only
    /// the fields the user actually changed.
    ///
    /// The form is read from storage when the modal OPENS and written back on
    /// Save, and Save is also the only way to commit a theme change — so a
    /// window whose modal had been open since before another window stored an
    /// API key wrote its own stale blank over that key, and pushed
    /// `chat: None` to its server on top. Comparing against this snapshot
    /// inside the config lock (`llm::Config::save_from`) is what tells "the
    /// user cleared this field" apart from "the user never touched it".
    pub ai_snapshot: (llm::Config, embed::Config),
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
            key_from_env: false,
            embed_key_from_env: false,
            theme_snapshot: (theme::ThemePref::System, "", ""),
            // Replaced with what was read from storage the moment the modal
            // opens; nothing saves from a modal that was never opened.
            ai_snapshot: (
                llm::Config::from_parts(
                    llm::Provider::Anthropic,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                embed::Config::from_parts(String::new(), String::new(), String::new()),
            ),
        }
    }
}

/// State for the DOCS tab — the project's API documentation view.
#[derive(Default)]
pub struct DocsState {
    /// The project's API documentation, per file (from the server's `BuildDocs`).
    pub files: Vec<clew_protocol::DocFile>,
    /// The change-registry revision the in-flight-or-loaded index was requested
    /// at — the freshness key `stats.rev` and `project_calls.rev` already use.
    /// A non-empty `files` is no evidence of freshness on its own: the only
    /// automatic rebuild fires while the DOCS tab is the VISIBLE one, so every
    /// edit made from another tab used to leave a pre-edit index that the
    /// tab-entry gate then accepted (stale signatures, and an "Open source"
    /// button pointing at a line the edit has since moved). Stamped by
    /// `request_docs`, compared by `App::docs_fresh`, and reset to
    /// [`crate::app::server_ai::DOCS_REV_STALE`] when a build is abandoned
    /// unanswered.
    pub rev: u64,
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
#[derive(Debug, Default, Clone, PartialEq)]
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
    /// Workspace trust, loaded from clew's global data directory: which project
    /// roots may be opened, and which repo-specified language-server commands
    /// may run. Deliberately not stored in the project — see `trust`.
    pub trust: clew_core::trust::Trust,
    /// A repo-specified language server awaiting the user's approval, with the
    /// exact command line to show them.
    pub pending_lsp_command: Option<PendingLspCommand>,
    pub scanning: bool,
    pub sidebar: SidebarTab,
    /// The call hierarchy shown in the Calls sidebar tab, if any.
    pub call_graph: Option<callgraph::CallTree>,
    /// Monotone counter minting call-tree identities. Each tree (and each
    /// `CallHierarchyPrepared` request) gets the next value; results carry it
    /// back, so children fetched for a tree that has since been replaced (a
    /// direction flip, a new hierarchy) can't graft onto the wrong nodes.
    pub call_token: u64,
    /// The token of the `CallHierarchyPrepared` currently awaited, if any.
    pub call_pending: Option<u64>,
    /// Monotone debug-run counter: bumped when a session starts or stops. Every
    /// DAP-side message carries the run it belongs to; a late event from a
    /// previous run (a final Terminated, a stop inspection) is dropped instead
    /// of landing on the next session.
    pub debug_run: u64,
    /// Live mirror of [`debug_run`] shared with the adapter-startup stream:
    /// the stream re-checks it between its slow steps (spawn, initialize) so
    /// a Stop during `Launching` — when no client exists yet to disconnect —
    /// actually cancels the startup and kills what it already spawned,
    /// instead of a second adapter running invisibly. Kept in sync via
    /// `bump_debug_run`.
    pub debug_run_live: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Monotone stop counter WITHIN a run: bumped on every stop and on every
    /// step/continue that leaves one. The run id alone cannot tell two stops
    /// of the same session apart, so a slow stack/scopes/watch reply for the
    /// first stop would land after the user continued (or after a second
    /// stop) and repaint the old frame, variables and highlight — feeding a
    /// stale location into `debug_context` and the reading trail. Results
    /// carry the value minted at their stop and are checked with
    /// `owns_debug_stop`.
    pub debug_stop: u64,
    /// Monotone search-submission counter. Local `SearchDone` / LSP
    /// `ReferencesResult` carry the value minted at their request; only the
    /// latest submission may paint the Search sidebar.
    pub search_seq: u64,
    /// Request id of the in-flight server-side search, if any; its
    /// `SearchResults` reply is applied only while it is still the latest.
    pub pending_search: Option<u64>,
    /// Request id of the in-flight `BuildDocs`, if any — so an `Error` reply
    /// (e.g. the server's not-ready refusal) stops the Docs spinner instead
    /// of leaving it loading forever.
    ///
    /// It cannot correlate the SUCCESS reply: that arrives as an unsolicited
    /// `Event::Docs` notification with no id. `request_docs` keeps a single
    /// build in flight so there is only ever one build this id can belong to.
    pub pending_docs: Option<u64>,
    /// Request id of the in-flight `ListDir` for the Connect modal's folder
    /// picker. Only the newest listing may paint the browser: two quick
    /// clicks used to let a slower earlier reply overwrite the newer one.
    pub pending_list_dir: Option<u64>,
    /// Monotone go-to-definition counter, same pattern as `search_seq`: a
    /// `DefinitionResult` from a superseded request must not jump the editor.
    pub goto_seq: u64,
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
    /// Whether the user granted THIS remote connection the right to hold the
    /// AI API keys and run AI calls server-side (the per-host opt-in from the
    /// Connect form). Always reset to false on a transport switch; without
    /// it, AI calls stay on the client and no key ever crosses the SSH link.
    pub remote_ai_opt_in: bool,
    /// Import-resolution metadata (go.mod module, pubspec package name) from
    /// the server's project snapshot — a remote project's resolver must not
    /// read those files off the local disk. `None` until a full snapshot
    /// arrives; reset on every scan.
    pub remote_import_meta: Option<(Option<String>, Option<String>)>,
    /// The last `ProjectSymbols.seq` applied. The server stamps every
    /// publication (full or partial) in send order; anything at or below
    /// this is stale — most importantly a full snapshot that was still
    /// building while the watcher already sent fresher partial updates.
    /// Reset on every scan/connect (the counter is server-lifetime).
    pub remote_index_seq: u64,
    /// This project's derived-artifact directory (`clew_core::derived::dir`),
    /// inside clew's own data dir — never inside the project, whose contents
    /// the repository controls. `None` when there is no data directory, in
    /// which case every derived cache runs in memory for the session.
    pub derived_dir: Option<PathBuf>,
    /// Remote `.clew/` state files whose content has not arrived yet. Until it
    /// does, what this client holds for them is the empty baseline every
    /// remote project starts from — writing that back would replace the
    /// remote file, or delete it (an empty list serializes to `None`).
    pub remote_state_pending: HashSet<String>,
    /// Remote state files whose latest change is not known to be on the
    /// remote's disk: either it could not be sent yet (its load is still
    /// outstanding, or there is no transport), or it was sent and has not been
    /// acknowledged. A re-read keeps the user's version over the remote's for
    /// anything listed here, and the change is rewritten once it can be.
    pub remote_state_dirty: HashSet<String>,
    /// The subset of [`Self::remote_state_dirty`] whose change the server
    /// demonstrably never received: the transport carrying it died before
    /// acknowledging it (or there was none to carry it).
    ///
    /// Kept apart because the dirty mark alone cannot say WHICH change it
    /// stands for. A second edit of the same file, made in the reconnect
    /// window, puts that rel back in flight — and the guards that read the
    /// mark then took the in-flight edit for the whole story: the re-read's
    /// flush was skipped as "the merge is on its way", and the merged file
    /// that arrived — computed without the change the dead transport ate —
    /// was adopted over this window's copy. The earlier change disappeared
    /// from the remote file and from this window's list at once, with no
    /// message, after the user had been told it was saved.
    ///
    /// Filled by `drop_connection_state` from whatever is dirty at that
    /// moment, and cleared only with the mark it qualifies.
    pub remote_state_unsent: HashSet<String>,
    /// Request id -> rel for `WriteState`s awaiting their `StateWritten`.
    ///
    /// A queued frame is NOT a durable write: a transport that has died
    /// without being detected accepts frames into a pipe that goes nowhere.
    /// Clearing the dirty mark on send therefore lost every bookmark, note,
    /// trail entry and tour saved in that window, and the reconnect's re-read
    /// then replaced them with the stale remote copy. The mark is cleared only
    /// when the server says the bytes reached the disk.
    pub remote_state_inflight: HashMap<u64, String>,
    /// Request id -> rel for the WHOLESALE writes that carry a change the
    /// server never received ([`Self::remote_state_unsent`]) — the rescue
    /// flushes, tracked apart from [`Self::remote_state_inflight`] because
    /// they answer a different question.
    ///
    /// `remote_state_inflight` answers "which change owns the dirty mark", so
    /// a newer request of any kind supersedes an older one there. The unsent
    /// mark is not about the newest change at all: it says one specific
    /// earlier change is missing from the remote file, and ONLY the flush that
    /// carries it can retire it. Superseding that id along with the dirty
    /// ownership left both marks set forever — the flush's `StateWritten` no
    /// longer owned the rel, and every later `StateEdited` returned early on
    /// the unsent mark, so the store stopped adopting other clients' entries
    /// and every reconnect rewrote its increasingly stale copy wholesale.
    ///
    /// An entry is safe to honour whenever it is acknowledged, because every
    /// `write_remote_state` caller serializes THIS WINDOW'S current copy,
    /// which by definition holds the unsent change.
    pub remote_state_rescue: HashMap<u64, String>,
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
    /// In-flight agent turns: stream id -> the channel feeding the Ask flow.
    /// `AgentStep` / `AgentDelta` / `AgentDone` notifications are routed here.
    #[allow(clippy::type_complexity)]
    pub agent_streams: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<AgentPiece>>,
        >,
    >,
    /// The stream id of the agent turn currently answering, for Stop.
    pub agent_stream: Option<u64>,
    /// Stream id of the in-flight server-side `ChatStream`, if any. Abandoning
    /// the answer (project switch, Ask Clear) must tell the SERVER to stop:
    /// dropping the client-side pump leaves the provider call running on the
    /// meter with nobody listening.
    pub chat_stream: Option<u64>,
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
    /// Per-pane: the request id of the file-open this pane is waiting for.
    /// A `FileContent` / `FileLoaded` result is applied only while its id is
    /// still the one the pane expects — a slower earlier open must not
    /// overwrite a faster later one, and a load issued before a project
    /// switch or a split-close must not resurrect.
    pub pane_pending: [Option<u64>; 2],
    /// In-flight `GitInfo` requests: id -> the file the blame was asked for.
    /// The reply paints THAT path, rather than re-deriving one from the
    /// current root and the reply's rel — which, after a project switch,
    /// resolves to a different project's file of the same name.
    pub pending_git: std::collections::HashMap<u64, PathBuf>,
    /// Root of an in-flight server `OpenProject`, so its `Tree` reply can build
    /// the project (abs paths resolve against it). Doubles as the identity
    /// check for the local-fallback `ScanDone`: a scan result for any other
    /// root is stale and dropped.
    pub pending_scan_root: Option<PathBuf>,
    /// Set while the `OpenProject` a (re)connect re-sent for an ALREADY-open
    /// project is in flight, so its `Tree` reply is recognized as a resync and
    /// spliced into the open project. It is deliberately not `scanning`: that
    /// flag means "opening", and reusing it would blank the panes and the file
    /// tree behind a "Scanning…" placeholder and then run `on_scan_done`,
    /// which closes every pane and drops the Ask history — over a transport
    /// hiccup. Without the splice the reply was dropped outright, and files
    /// created while the link was down stayed invisible until the next
    /// structural change (the watcher only reports what happens AFTER it).
    pub pending_tree_resync: bool,
    /// The current transport instance number. Part of the subscription key
    /// (bumping it after a disconnect makes iced tear down the dead stream
    /// and start a fresh one — that is the reconnect mechanism), and bumped
    /// on every target switch too, so it alone identifies a transport:
    /// every transport message carries the value it was minted under, and a
    /// late message from a dead or replaced transport is recognized and
    /// dropped by comparing it.
    pub conn_gen: u64,
    /// Whether the next transport instance replaces one that died (delays
    /// the respawn briefly to stop crash hot-loops). Cleared on a
    /// user-initiated switch, which should connect immediately.
    pub conn_respawn: bool,
    /// Why the last clew-server refused our handshake (protocol version or
    /// build fingerprint), while that verdict still stands.
    ///
    /// Two jobs, both about a failure that RETRYING CANNOT FIX: it keeps the
    /// only actionable message ("rebuild the server") on screen, which the
    /// reconnect line used to overwrite milliseconds later, and it stops
    /// `on_server_disconnected` from re-keying the subscription — the same
    /// binary answers the same way every time, so the automatic reconnect
    /// spawned and reaped a server every 1.5 s for the rest of the session.
    /// A transport that merely DIED leaves this `None` and still reconnects.
    /// Cleared whenever a fresh transport comes up, so a user-initiated
    /// connect or project open gets a full verdict again.
    pub handshake_failure: Option<String>,
    /// The current project instance. Bumped on every project install
    /// (`on_scan_done`) and every transport switch; async task results
    /// carry the value they were spawned under and are dropped when it no
    /// longer matches. A `root` comparison alone cannot do this: a local
    /// and a remote project can share the same absolute path while being
    /// different machines' code.
    pub project_epoch: u64,
    /// Next handle for a server-spawned process (language server / debug adapter).
    pub next_proc_id: u64,
    /// proc handle -> the channel that feeds `ProcessOutput` bytes into the
    /// matching LspClient's stdout bridge.
    pub proc_feeds: std::collections::HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
    /// language -> its live server-spawned proc handle, so a restart can kill the
    /// old process before starting a new one.
    pub lsp_procs: std::collections::HashMap<String, u64>,
    /// language -> spawn generation. Bumped every time a server (re)start or
    /// install begins for that language; a `LspStartResult` / `LspDownloadResult`
    /// carrying an older generation is from a superseded spawn (restart, project
    /// switch) and is dropped instead of installing a dead client as Ready.
    pub lsp_gen: std::collections::HashMap<String, u64>,
    /// Semantic search: the embedding index over explanation summaries.
    ///
    /// Its vectors belong to the embedding space (`embed::Space`) that was live
    /// when the project opened or when the last build ran — and the config can
    /// move under a session, from this window's Settings, another window's, or
    /// a hand edit of `config.toml`. Nothing about the vectors themselves says
    /// so, and cosine keeps answering confidently across two spaces, so every
    /// path that USES this drops it first when the live config disowns it
    /// (`App::drop_foreign_embed_index`) and `on_settings_saved` drops it when
    /// it writes a space change of its own. Keeping it is the silent failure,
    /// not losing it.
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
    /// built off-thread after indexing and rebuilt as Rust files change; feeds
    /// the hover structure peek.
    pub structure: structure::StructureIndex,
    /// The registry revision the live `structure` was read at, so a build that
    /// started earlier and finished later cannot put the pre-edit relations
    /// back (the same freshness key `stats.rev` and `project_calls.rev` use).
    pub structure_rev: u64,
    /// A structure build is in flight. It re-reads and re-parses every Rust
    /// file in the project, so it is single-flight: an edit burst must not
    /// stack one whole-project parse per save.
    pub structure_building: bool,
    /// Rust files changed while that build was running, so its result is
    /// already behind. One rebuild is spawned when it lands, rather than one
    /// per event while it ran.
    pub structure_dirty: bool,
    /// Monotonic LSP document version, bumped on every `didChange`.
    pub lsp_doc_rev: i64,
    /// Last diagnostics version seen per language, to gate refresh ticks.
    pub seen_diag_version: std::collections::HashMap<String, u64>,
    /// Last inlay-hint refresh epoch seen per language, so a server-requested
    /// refresh re-fetches hints exactly once.
    pub seen_inlay_epoch: std::collections::HashMap<String, u64>,
    /// A language server download awaiting the user's consent.
    pub pending_lsp_consent: Option<LspConsent>,
    /// Per-language `init_options` a remote clew-server sent with
    /// `LspResolved` — the LSP handshake runs client-side even for a remote
    /// server, but the options belong to the host that owns the lsp.toml.
    pub remote_lsp_init: std::collections::HashMap<String, serde_json::Value>,
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
    /// Mints [`BlameWhy::token`]. Monotonic for the window's lifetime, so a
    /// token can never be reused by a later request.
    pub blame_why_seq: u64,
    /// Bumped whenever inlay hints are turned off, invalidating every request
    /// still in flight (see [`crate::app::message::Message::InlayHintsLoaded`]).
    pub inlay_gen: u64,
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
