//! The App model: the window-level [`App`], the [`ProjectSession`] that holds
//! everything scoped to the open project (replaced whole when the project is
//! left or the transport switches) with its per-transport [`ProjectLink`],
//! and the per-domain state sub-structs they are made of.

use crate::app::prelude::*;
use crate::*;

/// Work the app layer has in flight for the CURRENT project, beyond the busy
/// flags the views read. Part of the [`ProjectSession`], so it goes with the
/// project, and the task handles it holds are abort-on-drop: leaving a
/// project cancels every task named here by construction — there is no
/// hand-maintained list of what to stop, and a field added here is dropped
/// (and its task aborted) without anyone having to remember a teardown site.
#[derive(Default, Debug)]
pub struct InFlight {
    /// The Ask answer streaming from the provider on THIS machine (the client
    /// AI endpoint): its stream id and an abort-on-drop handle to its pump.
    /// Aborting drops the pump's receiver, which is what the blocking
    /// provider call polls to stop (`llm::complete_chat_stream`'s cancel
    /// check), so the answer stops being generated — and billed.
    pub ask_local: Option<(u64, iced::task::Handle)>,
    /// A retrieval-mode question whose embedding is being computed: the stream
    /// id minted for it at submit (`AskMsg::Retrieved::stream`), and the
    /// guard its embeddings request polls. Retiring the question — Stop, Ask
    /// Clear, the project left — drops the guard, which gives the request up
    /// then and there, its connection with it.
    pub ask_retrieval: Option<(u64, RaiseOnDrop)>,
    /// The semantic search whose query is being embedded: the guard its
    /// request polls. A newer search takes its place, and leaving the
    /// project drops it — either way the request is given up, rather than
    /// waited out for an answer nobody will read.
    pub semantic_search: Option<RaiseOnDrop>,
    /// The semantic-index build running: an abort-on-drop handle to its task.
    /// Leaving the project aborts it, and the embeddings requests with it
    /// (`AiClient::embed`), instead of embedding — and billing — the rest of
    /// a project the window has left.
    pub embed_build: Option<iced::task::Handle>,
    /// A retrieval-mode question whose context is being assembled off the UI
    /// thread (`AskMsg::ContextReady` starts its stream).
    pub ask_context: Option<PendingAsk>,
    /// Per language, the repository command a background staging approved
    /// and staged, with the configuration it was staged for — so the start
    /// that follows need not hash the command file a second time on the UI
    /// thread (see `App::approved_init_options`).
    pub lsp_staged: HashMap<String, (lsp::config::EffectiveServer, PathBuf)>,
    /// The project's persisted state is being read (`ProjectMsg::StateLoaded` not
    /// applied yet). While set, the stores it will fill must not be written
    /// wholesale from their empty placeholders.
    pub state_loading: bool,
    /// The project's persisted state could not be read at all (the read
    /// failed as a whole): the wholesale trail write stays off for the
    /// session, since the trail in memory is not the stored one.
    pub state_unread: bool,
    /// Per-project stores (by `.clew/` rel) the user changed while that load
    /// was in flight: their in-memory copy is newer than what the load read,
    /// so the load leaves them alone.
    pub state_touched: HashSet<&'static str>,
    /// Languages whose language server was asked for before `lsp.toml` had
    /// loaded; started once it has, against the project's real config.
    pub deferred_lsp: Vec<String>,
}

/// The explanation cache, shared copy-on-write. A project's cache runs to
/// thousands of entries, and background work (an explain pass's reuse
/// baseline, a merge into the derived store) needs a snapshot of it: cloning
/// this is O(1), where cloning the map on the UI thread stalled every window.
/// Reading goes through `Deref`; a change goes through `DerefMut`, which
/// copies the map only while a snapshot is still held elsewhere.
#[derive(Default, Clone, Debug)]
pub struct SharedCache(Arc<explain::Cache>);

impl std::ops::Deref for SharedCache {
    type Target = explain::Cache;
    fn deref(&self) -> &explain::Cache {
        &self.0
    }
}

impl std::ops::DerefMut for SharedCache {
    fn deref_mut(&mut self) -> &mut explain::Cache {
        Arc::make_mut(&mut self.0)
    }
}

impl From<explain::Cache> for SharedCache {
    fn from(cache: explain::Cache) -> Self {
        SharedCache(Arc::new(cache))
    }
}

impl<'a> IntoIterator for &'a SharedCache {
    type Item = (&'a explain::Node, &'a explain::Cached);
    type IntoIter = std::collections::hash_map::Iter<'a, explain::Node, explain::Cached>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A window's saves of its explanation cache, shared between the window and
/// the task that runs them one at a time (`App::persist_explain`). The saves
/// are the task's: a save asked for while it runs joins its queue, and runs
/// after the one out — whatever becomes of the window meanwhile.
#[derive(Debug, Clone, Default)]
pub struct SaveChain(Arc<std::sync::Mutex<SaveQueue>>);

impl SaveChain {
    /// A chain whose task has not finished: it takes more saves.
    pub(crate) fn open() -> SaveChain {
        SaveChain(Arc::new(std::sync::Mutex::new(SaveQueue {
            open: true,
            ..SaveQueue::default()
        })))
    }

    /// The queue. A panic elsewhere while it was held leaves it whole: each
    /// change to it is a single assignment.
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, SaveQueue> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `other` is this very chain.
    pub(crate) fn is(&self, other: &SaveChain) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What a [`SaveChain`] holds.
#[derive(Debug, Default)]
pub struct SaveQueue {
    /// Whether the task still takes saves. Once it has finished, a save asked
    /// for starts a task of its own.
    pub(crate) open: bool,
    /// The save asked for while one was out, which runs next.
    pub(crate) next: Option<QueuedSave>,
    /// What a save that failed carried and no later save took: handed back to
    /// the window when the chain's end lands (`ExplainMsg::Persisted`), or
    /// carried by the next save the window starts before that.
    pub(crate) left: explain::Unsaved,
}

/// A save that waits for the one out: every change asked to be saved since,
/// and the window's cache and its sequence number as they were when last
/// asked.
#[derive(Debug)]
pub struct QueuedSave {
    pub(crate) changes: explain::Unsaved,
    pub(crate) mine: SharedCache,
    pub(crate) seq: u64,
}

/// A retrieval-mode Ask question waiting for its context (see
/// `InFlight::ask_context`).
#[derive(Debug)]
pub struct PendingAsk {
    pub stream: u64,
    pub question: String,
    pub sources: Vec<(explain::Node, f32)>,
}

/// The debugger (DAP client): the active session, plus the breakpoints and
/// watch expressions that persist independently of any running session.
#[derive(Default)]
pub struct DebugState {
    /// The active debug session (DAP), if any.
    pub session: Option<DebugSession>,
    /// The last run's stops, in order (see [`TraceStop`]): what the program
    /// actually did, kept after the session ends so a walkthrough can be
    /// made of it; cleared when the next run starts.
    pub trace: Vec<TraceStop>,
    /// Bumped with every change to `trace`: what views derived from it are
    /// keyed by.
    pub trace_rev: u64,
    /// The trace hit [`MAX_TRACE_STOPS`] and stopped recording.
    pub trace_cut: bool,
    /// The program the trace is of, by its file name, for labels.
    pub trace_program: Option<String>,
    /// Why the program last stopped (the adapter's reason), for the stop
    /// the trace records once its stack arrives.
    pub pending_reason: String,
    /// Watch expressions (persist across stops/sessions).
    pub watches: Vec<String>,
    /// The add-watch input box.
    pub watch_input: String,
    /// The last function the debugger stopped in — so entering a NEW function
    /// records one reading-trail entry (not one per line step).
    pub last_fn: Option<String>,
    /// Breakpoints per file (absolute path → 1-based line → breakpoint),
    /// independent of a running session so they can be set before and persist
    /// across runs.
    pub breakpoints: HashMap<PathBuf, std::collections::BTreeMap<usize, Bp>>,
    /// The running adapter advertised `supportsEvaluateForHovers` in its
    /// `initialize` answer — the DAP spec's promise that an evaluate for a
    /// data hover has no side effects. Reset whenever the session ends.
    pub hover_safe: bool,
    /// The run a js-debug child session has started in
    /// (`DebugMsg::DapChildStarted`). Keyed by the run, so a later run starts
    /// with none. Until one has, a child that fails to start was the run's
    /// real target, and the run fails with it (`App::on_debug_child_failed`).
    pub child_started: Option<u64>,
}

/// The whole-project symbol call graph (tree-sitter name-resolved, optionally
/// LSP-refined to exact edges) plus its build / incremental-refine state.
#[derive(Default, Debug)]
pub struct ProjectCallsState {
    /// Whole-project symbol call graph (tree-sitter, name-resolved), built lazily
    /// when its overlay opens; drives the project call-graph overlay. Installed
    /// only by `App::set_project_calls_graph`, which hands back the graph it
    /// replaces — a node per function of the project — to be freed off the UI
    /// thread. Shared (`Arc`): the map's layout reads it on the blocking pool
    /// without a copy (`App::refresh_graph_layout`), and a graph is freed
    /// wherever its last holder lets it go.
    pub graph: Arc<projectcalls::ProjectCallGraph>,
    /// A build — or an LSP refine — landed: `graph` is its result for the
    /// revisions below. Empty is a result too (a project without callable
    /// definitions), never "not built yet", which read as stale forever and
    /// rebuilt it in a loop.
    pub built: bool,
    /// Registry revision the graph was last built (or refined) at, to rebuild
    /// it only when files actually changed since.
    pub rev: u64,
    /// The symbol-index revision it read (`ProjectSession::symbol_index_rev`):
    /// the index supplies the definitions and call sites a local build links,
    /// and lands after the files are registered, so a graph built before it
    /// read none of it.
    pub index_rev: u64,
    /// The import scope it was built against (`ProjectSession::import_scope_rev`):
    /// a graph linked before the import job landed resolved cross-file calls
    /// by name alone, and must not pass for current once the scope is in.
    pub scope_rev: u64,
    /// The registry and symbol-index revisions the LSP refine pass running,
    /// or the last one that landed, read when it started: what a refined
    /// graph is current for (it is stamped with them when it lands).
    pub refine_read: (u64, u64),
    /// True while the graph is being (re)built off-thread.
    pub building: bool,
    /// True when `graph` is the exact LSP-resolved graph rather than the
    /// tree-sitter name-based approximation.
    pub precise: bool,
    /// Generation counter for LSP-refine runs, so a late result from a superseded
    /// run (new project, re-refine, or a dropped refinement) is dropped.
    pub generation: u64,
    /// LSP-refine progress `(done, total)` while a refine is running.
    pub refine_progress: Option<(usize, usize)>,
    /// Abort handle for the running LSP refine. The pass holds CLONES of the
    /// project's language-server clients, so dropping `ProjectLink::lsp` does not
    /// stop it — it keeps querying servers for a project we have left.
    pub refine_abort: Option<iced::task::Handle>,
    /// The precise edge set, symbol-keyed, kept while `precise` so a file change
    /// can patch only the affected functions. Lent to the incremental pass in
    /// flight (`RefinePass::Incremental`) rather than copied — it patches the
    /// set on the blocking pool and hands it back when it lands — so it is
    /// empty meanwhile; no second pass starts before then.
    pub precise_edges: projectcalls::SymEdges,
    /// Source files changed since the refined graph was computed — noted
    /// whenever the refine owns the graph (`App::note_refine_change`) —
    /// awaiting an incremental refine: one runs at a time, and folds in what
    /// was noted meanwhile when it lands.
    pub precise_pending: HashSet<PathBuf>,
    /// The languages the refinement covers: those a ready call-hierarchy
    /// server answered for when its full pass started. Its graph holds their
    /// functions, and only theirs — a server ready later adds none, unrefined
    /// — and follows a change only to their files, each through its
    /// language's server: while that one starts, they wait for it; with it
    /// down, the refinement is handed back (`App::fold_refine_pending`).
    pub refined_langs: HashSet<String>,
    /// The spawn generation (`App::lsp_gen`) of each server the pass running
    /// queries, by language — `None` for one started without a generation
    /// minted: one restarted while the pass ran answered through the client
    /// it replaced (`App::on_project_calls_refined`).
    pub refine_gens: HashMap<String, Option<u64>>,
    /// The files the incremental pass running refines: those of a language
    /// whose server restarted meanwhile are refined again, through the new
    /// one.
    pub refine_changed: HashSet<PathBuf>,
    /// The wait for a starting server that changed files are held for
    /// (`App::fold_refine_pending`), while one is armed.
    pub refine_wait: Option<RefineWait>,
    /// The wait of a full pass ("Refine with LSP") held for its servers to
    /// load the project (`App::refine_project_calls`): started once none of
    /// them is loading. Apart from `refine_wait`, which the refinement on
    /// screen holds its changed files under, and which a name-based build
    /// landing drops.
    pub refine_full_wait: Option<RefineWait>,
    /// Waits armed so far: the number of the last one.
    pub refine_waits: u64,
    /// The files of the functions a pass failed in a way that may pass —
    /// the file changed under the query, the server cancelled it or did not
    /// answer in time (`RefineDone::retry`): refined again with the next pass
    /// that runs (`App::fold_refine_pending`), never in a pass of their own,
    /// which a wedged server would answer the same, in a loop.
    pub refine_retry: HashSet<PathBuf>,
    /// Bumped on every change to `graph` (see `App::set_project_calls_graph`):
    /// the generation the overlay's derived rankings are memoized by.
    pub graph_rev: u64,
    /// The functions of the refined graph its passes could not refine — the
    /// server answered them with an error, or not in time, even asked again
    /// — each with why (`RefineDone::unrefined`): calls into them may be
    /// missing from it, and it says so. Kept while it is refined: a full
    /// pass replaces them, and a pass over changed files re-queries theirs.
    pub unrefined: HashMap<projectcalls::SymKey, String>,
    /// Those of `unrefined` a pass over changed files failed: it dropped
    /// every edge of their files, and the calls out of them — which a full
    /// pass reads off the callees' own answers — are missing too.
    pub unrefined_out: HashSet<projectcalls::SymKey>,
}

/// A wait of the call-graph refine for language servers that are starting,
/// or loading the project (`App::fold_refine_pending`).
#[derive(Debug, Clone)]
pub struct RefineWait {
    /// Its number, which the timer bounding it carries
    /// (`GraphMsg::RefineWaitOver`); a renewed wait takes a new one.
    pub id: u64,
    /// When it was first armed: a server that keeps reporting its loading
    /// is waited for up to `graph::REFINE_LOAD_LIMIT` from then.
    pub since: std::time::Instant,
    /// The reports each server it waits for had made
    /// (`lsp::client::Snapshot::reports`) when it was armed or last renewed:
    /// one that has made none since has gone silent.
    pub heard: HashMap<String, u64>,
}

/// The architecture-overview "home": the generated prose, its native module
/// map, and the generation/freshness bookkeeping.
#[derive(Default, Debug)]
pub struct OverviewState {
    /// The generated architecture overview — RAW LLM markdown, no module map.
    /// The module diagram is injected fresh at prepare time from the current
    /// import graph (never baked into the cache), so it can't go stale.
    pub markdown: Option<String>,
    /// The module map, drawn natively on a canvas in the overview home (like the
    /// Import Graph overlay) — laid out from the current import graph, not baked
    /// into the prose or a mermaid diagram.
    pub map: Option<graphlayout::Layout>,
    /// The revision of `map`'s content (`ui::next_layout_rev`), bumped where
    /// it is laid out — what tells the map's cached drawing it is stale.
    pub map_rev: u64,
    /// The overview prepared for display (markdown + math/mermaid SVG segments).
    pub prepared: Vec<PreparedSeg>,
    /// True while the overview is being generated.
    pub generating: bool,
    /// True when the main area shows the overview "home" (vs. code / empty).
    pub showing: bool,
    /// Prompt hash of the cached overview, so a re-explain regenerates it only
    /// when its inputs actually changed (avoids a needless overview LLM call).
    pub prompt_hash: Option<incremental::Version>,
    /// Mints generation requests; `OverviewMsg::Done` carries the value its request
    /// was minted with and only the newest may land or clear `generating`.
    pub seq: u64,
}

/// The Stats "home": the per-language code statistics and its freshness.
#[derive(Debug)]
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
/// state that drives reading one and generating a new one in the WALK tab
/// (the tab's own controls are the window's: see [`WalkUi`]).
#[derive(Default, Debug)]
pub struct WalkState {
    /// The per-project library of saved walkthroughs (persisted with the project).
    pub library: Vec<walkthrough::Walkthrough>,
    /// The SCOPE of the tour being read (the key tours are upserted on), or
    /// `None` while browsing the library list. Not an index into `library`:
    /// the list is re-read from disk (another window's save) and re-adopted
    /// from the server (another client's), and an index computed against the
    /// old order silently switched the reader into somebody else's tour.
    pub open: Option<String>,
    pub step: usize,
    /// Where the open step actually landed, when that is not where it meant to
    /// (its symbol is missing: shown at its own line, or at the top of the
    /// file). Kept WITH the step and shown under it in the WALK panel: the
    /// status line that used to carry it is overwritten by the very file load
    /// the step starts.
    pub anchor_note: Option<String>,
    /// A step whose file was opened before that file's own symbols were known
    /// (the index had not reached it, or skipped it), to resolve again — and
    /// jump if the answer differs, unless the reader has moved since — once
    /// the opened viewer's symbols land (`App::settle_walk_anchor`).
    pub pending_anchor: Option<PendingAnchor>,
    /// The scope currently being (re)generated, or `None` when idle. Lets the UI
    /// mark just that one row as busy while the rest of the library stays usable.
    pub generating: Option<String>,
    /// The request `generating` belongs to (see `WalkMsg::Done::seq`).
    pub pending: Option<u64>,
    /// Mints generation requests.
    pub seq: u64,
    /// True while a walkthrough generation is on its one automatic retry (the LLM
    /// occasionally emits malformed JSON); prevents an endless retry loop.
    pub retried: bool,
    /// The current step's narration, prepared for rich display (markdown, plus
    /// mermaid diagrams and math rendered as inline SVGs — same pipeline as the
    /// overview and explanations).
    pub prepared: Vec<PreparedSeg>,
}

/// A walkthrough step waiting for its file's own symbols (see
/// `WalkState::pending_anchor`). Bound to the one load of its file it waits
/// for: once that load is no longer the pane's pending one and the pane
/// shows another file, the reader has moved on, and the step must not reach
/// back later and move a cursor the reader put somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAnchor {
    /// The step's file.
    pub abs: PathBuf,
    /// The step's index in the open tour.
    pub step: usize,
    /// The pane the step opened its file in.
    pub pane: usize,
    /// The load it waits for in `pane` (`ProjectLink::pane_pending`): the one
    /// the step started, or the one of a jump into the same file that
    /// superseded it there (`App::open_file`) — or `None` when the file was
    /// already on screen, waiting only for symbols.
    pub load: Option<u64>,
    /// The reader has moved since the step opened its file — a click, a
    /// motion or a scroll in it, going into its history, or any jump
    /// (`App::reader_moved_caret`, `App::open_file`). Their move stands: the
    /// step moves nothing once the symbols land. It still waits for them to
    /// learn where it would have landed, when that is not where it meant to
    /// (`WalkState::anchor_note`) — which is the step's to say whatever the
    /// reader did meanwhile.
    pub superseded: bool,
}

impl WalkState {
    /// The tour being read.
    pub fn open_tour(&self) -> Option<&walkthrough::Walkthrough> {
        let scope = self.open.as_deref()?;
        self.library.iter().find(|w| w.scope == scope)
    }

    /// Whether a step waits to settle in `abs` that the reader's own move
    /// there would still supersede (`reader_moved_in`).
    pub fn waits_in(&self, abs: &std::path::Path) -> bool {
        self.pending_anchor
            .as_ref()
            .is_some_and(|anchor| !anchor.superseded && anchor.abs == abs)
    }

    /// The reader moved in `abs` themselves: a step still waiting to settle
    /// there moves nothing once its symbols land (`PendingAnchor::superseded`).
    pub fn reader_moved_in(&mut self, abs: &std::path::Path) {
        if let Some(anchor) = self.pending_anchor.as_mut()
            && anchor.abs == abs
        {
            anchor.superseded = true;
        }
    }

    /// After the library was replaced or shrunk: a tour no longer in it is no
    /// longer open, and its narration (and step note) goes with it.
    pub fn forget_vanished_tour(&mut self) {
        if self.open.is_some() && self.open_tour().is_none() {
            self.open = None;
        }
        if self.open.is_none() {
            self.prepared = Vec::new();
            self.anchor_note = None;
            self.pending_anchor = None;
        }
    }
}

/// The WALK tab's own controls. They belong to the window, not to a project:
/// what the reader typed and the layout they chose stay put when another
/// project opens.
pub struct WalkUi {
    /// The shared top input: a search query in `Search` mode, a scope prompt in
    /// `Walk` mode.
    pub input: String,
    /// Whether the top input searches the library or generates a new tour.
    pub mode: WalkMode,
    /// Height of the narration block in the WALK tab; the steps list above it
    /// takes the rest. The divider between them is draggable.
    pub narration_height: f32,
}

impl Default for WalkUi {
    fn default() -> Self {
        Self {
            input: String::new(),
            mode: WalkMode::Search,
            narration_height: 240.0,
        }
    }
}

/// The Explain feature's state: the incremental explanation cache plus the
/// currently-open explanation overlay and its render artifacts.
#[derive(Default, Debug)]
pub struct ExplainState {
    /// LLM explanations keyed by function/file/folder, kept fresh incrementally.
    /// Copy-on-write (see [`SharedCache`]); app code changes it through
    /// `App::explain_cache_mut` / `App::set_explain_cache`, which also bump
    /// `cache_seq`.
    pub cache: SharedCache,
    /// Bumped on every change to `cache`. A background merge carries the value
    /// it started from and is adopted only if nothing changed since (see
    /// `ExplainMsg::Persisted`).
    pub cache_seq: u64,
    /// True while the explain pass is running.
    pub running: bool,
    /// Explain progress `(done, total)` while a pass runs.
    pub progress: Option<(usize, usize)>,
    /// How many attempts in the current pass have errored (surfaced in the UI so
    /// a failing pass doesn't masquerade as success).
    pub failed: usize,
    /// Generation for explain passes, so a superseded result is dropped.
    pub generation: u64,
    /// Source files the watcher reported since a pass last took them (every
    /// pass does but a re-explain), plus the sources of what a finished pass
    /// could not explain (`explain::Tally::retry`) and of the files it could
    /// not read where it would have explained code without a summary. Only a
    /// hint: a pass finds changed code by the code itself (`explain::Basis`);
    /// this says where code that has no summary yet is to be explained — which
    /// the automatic refresh does, and elsewhere only for code added since the
    /// summaries around it were written (`explain::Reuse::ChangedSources`).
    pub changed_sources: HashSet<PathBuf>,
    /// The `changed_sources` the running pass took. Handed back when it does
    /// not complete (cancelled, stopped by the provider): code written there
    /// may still have no summary.
    pub pass_sources: HashSet<PathBuf>,
    /// The groups, by key, the passes of this session left waiting for a
    /// summary they quote (`explain::Tally::waiting`), until a pass writes
    /// them: they do not wait again, so a call that keeps failing freezes
    /// nothing above it (`explain::Pass::waited_before`).
    pub waited: HashSet<explain::Node>,
    /// Whether the running pass is the automatic refresh (see
    /// `ExplainMsg::Refresh`).
    pub pass_automatic: bool,
    /// The node the running pass re-explains (`ExplainMsg::ReexplainNode`).
    pub pass_target: Option<explain::Node>,
    /// The entries this window changed since it last saved, and what each
    /// replaced: all a save writes (`explain::merge_unsaved`). Taken by the
    /// save, handed back when it fails.
    pub unsaved: explain::Unsaved,
    /// The saves that are out: one runs at a time, in its own task, and a
    /// save asked for meanwhile joins it (`App::persist_explain`).
    pub save: Option<SaveChain>,
    /// Whether this session already said the stored cache is over its size
    /// cap (said once, not on every save).
    pub cap_reported: bool,
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
    /// across every explanation, backed on disk by `svg/` in the project's
    /// derived-artifact directory (`clew_core::derived::dir`, in clew's own data
    /// directory — never inside the project).
    pub svgs: HashMap<u64, ExplainSvg>,
    /// Math/mermaid sources the renderer could not draw this session, keyed
    /// like `svgs`, with the reason. They show as their source text, and are
    /// not handed to the renderer again until the next session.
    pub svg_failed: HashMap<u64, String>,
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
#[derive(Default, Debug)]
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
    /// [`crate::app::docs::DOCS_REV_STALE`] when a build is abandoned
    /// unanswered.
    pub rev: u64,
    /// Which files are expanded in the DOCS tree (keys are file rels).
    pub expanded: HashSet<String>,
    /// Filter text for the DOCS tree (matches item names).
    pub filter: String,
    /// Bumped whenever `files` is replaced (`App::apply_docs`, its one
    /// assignment site): the generation the DOCS tree's grouping is memoized
    /// by (`ui::ViewMemo::docs_groups`).
    pub generation: u64,
    /// The doc page rendered in the main pane, with the selected item's doc
    /// markdown pre-parsed (the markdown widget borrows it). `None` = no page.
    pub page: Option<DocPage>,
}

/// The DOCS tab's view options. The window's, not the project's: they shape
/// how any project's documentation is shown.
#[derive(Default)]
pub struct DocsView {
    /// Show all symbols vs. only the public API surface (default: public only).
    pub show_all: bool,
    /// Group the Docs tree by module/package instead of by file (default: file).
    pub by_module: bool,
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
    /// Stops the download in flight (see `updater::download_task`): on
    /// Cancel, and when this window closes. `None` while none is running.
    pub download: Option<iced::task::Handle>,
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
            download: None,
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
    /// Everything scoped to the open project — and, in its `link`, to the
    /// transport it is open over. Leaving the project (opening another,
    /// switching transport) replaces it WHOLE, which is the reset: nothing
    /// project-scoped lives outside it, so there is no list of fields to
    /// clear by hand (see [`ProjectSession`]).
    pub proj: ProjectSession,
    /// The link to this window's clew-server: its request channel (handed out
    /// only while alive) and what awaits answers over it that no project owns.
    /// Replaced whole when the transport dies or is switched (see
    /// [`crate::app::server::ServerLink`]).
    pub server: crate::app::server::ServerLink,
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
    pub scanning: bool,
    pub sidebar: SidebarTab,
    /// Monotone counter minting call-tree identities. Each tree (and each
    /// `CallsMsg::Prepared` request) gets the next value; results carry it
    /// back, so children fetched for a tree that has since been replaced (a
    /// direction flip, a new hierarchy) can't graft onto the wrong nodes.
    pub call_token: u64,
    /// Identity of the latest value trace, so a stale answer is dropped.
    pub flow_token: u64,
    /// Monotone debug-run counter: bumped when a session starts or stops. Every
    /// DAP-side message carries the run it belongs to; a late event from a
    /// previous run (a final Terminated, a stop inspection) is dropped instead
    /// of landing on the next session.
    pub debug_run: u64,
    /// Live mirror of [`Self::debug_run`] shared with the adapter-startup stream:
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
    /// Persisted Imports/Importers direction preference across focus changes.
    pub import_dir: imports::Dir,
    /// Writes the reading trail off the UI thread, one write at a time (see
    /// `App::save_history`). Window-level: the trail of a project just left
    /// still reaches its disk.
    pub trail_writer: TrailWriter,
    /// Where code is read from — local, or a remote host over SSH. This keys the
    /// server subscription: changing it restarts the transport against the new
    /// target, which is how an in-app Connect switches between local and remote.
    pub connection: connect::ConnTarget,
    /// Whether the user granted THIS remote connection the right to hold the
    /// AI API keys and run AI calls server-side (the per-host opt-in from the
    /// Connect form). Always reset to false on a transport switch; without
    /// it, AI calls stay on the client and no key ever crosses the SSH link.
    pub remote_ai_opt_in: bool,
    /// Remembered SSH hosts, shown in the Connect modal (from `connections.toml`).
    pub saved_connections: Vec<connect::SavedConnection>,
    /// The Connect modal's state (closed, editing a host, browsing a remote's
    /// folders). `None` when the modal is closed.
    pub connect: Option<ConnectUi>,
    /// Next request id for server calls that need a correlated reply.
    pub next_req_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The first request id minted for the project open now: `next_req_id`
    /// as `App::forget_project_state` dropped the one before. An earlier id
    /// that nothing still waiting claims — the open's own requests, the
    /// handshake and a folder listing are claimed before this is looked at
    /// — was asked for a project since left, whose session, and every
    /// record of what it waited for, went whole: a refusal of one is
    /// nobody's, where it reached the status line as an error of this
    /// project's. Kept here, as the session a project starts with is every
    /// window's.
    pub session_first_req: u64,
    /// Root of an in-flight server `OpenProject`, so its `Tree` reply can build
    /// the project (abs paths resolve against it). Doubles as the identity
    /// check for the local-fallback `ScanDone`: a scan result for any other
    /// root is stale and dropped.
    pub pending_scan_root: Option<PathBuf>,
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
    /// This window is closing, or the app quitting: it wants no clew-server
    /// any more (`wants_server`), so its transport is released — the server
    /// sees EOF and gets its teardown grace, which the quit waits for
    /// (`server::all_servers_reaped`) — and a transport that ends meanwhile
    /// is not restarted.
    pub quitting: bool,
    /// The current project instance. Bumped on every project install
    /// (`on_scan_done`) and every transport switch; async task results
    /// carry the value they were spawned under and are dropped when it no
    /// longer matches. A `root` comparison alone cannot do this: a local
    /// and a remote project can share the same absolute path while being
    /// different machines' code.
    pub project_epoch: u64,
    /// The `project_epoch` the connected server was last told about: set when
    /// its `Tree` reply installs the project, or when `sync_project_to_server`
    /// hands it a project opened by the local fallback. Server notifications
    /// name only a root, so they are applied only while this equals
    /// `project_epoch` (see `App::owns_server_event`): a same-rooted project
    /// the client re-opened on its own is not the one the server's watcher is
    /// still reporting on.
    pub server_epoch: u64,
    /// Next handle for a server-spawned process (language server / debug adapter).
    pub next_proc_id: u64,
    /// language -> spawn generation. Bumped every time a server (re)start or
    /// install begins for that language; a `LspMsg::StartResult` / `LspMsg::DownloadResult`
    /// carrying an older generation is from a superseded spawn (restart, project
    /// switch) and is dropped instead of installing a dead client as Ready.
    pub lsp_gen: std::collections::HashMap<String, u64>,
    /// Whether an embedding endpoint is configured.
    pub embed_available: bool,
    /// The collapsible bottom panel: whether it's shown, and which of its two
    /// tabs (Ask / Debug) is active.
    pub show_bottom: bool,
    pub bottom_tab: BottomTab,
    /// The debugger (DAP): the active session plus breakpoints and watches that
    /// persist across sessions (see [`DebugState`]).
    pub debug: DebugState,
    /// Opt-in: while paused, hovering an identifier evaluates it in the
    /// debuggee. Off by default — an evaluation can run the program's own code
    /// (getters, `Debug`/`toString` impls) with side effects.
    pub debug_hover_eval: bool,
    /// Whether an LLM key is configured (gates the explain UI). Checked at
    /// startup / project open, not per frame.
    pub llm_available: bool,
    /// The chat-model config as last read (`None` inside = no key configured),
    /// so the dozen flows that need it do not each re-read `config.toml`.
    /// Dropped — and re-read on next use — when this window saves Settings,
    /// when a project opens, and when the window regains focus (another
    /// window may have saved Settings meanwhile). See `App::llm_config`.
    pub llm_config_cache: Option<Option<llm::Config>>,
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
    /// Heat: the map colours nodes by how often their file changed over the
    /// recent history ([`ProjectSession::churn`]) instead of by language.
    /// Toggled from the map header; applies to every graph map.
    pub graph_heat: bool,
    /// Whether the left sidebar (files / search / marks / calls / imports) is shown.
    pub show_left_sidebar: bool,
    /// Whether the right sidebar (Outline / Explain tabs) is shown.
    pub show_right_panel: bool,
    /// Monotonic LSP document version, bumped on every `didChange`.
    pub lsp_doc_rev: i64,
    /// Mints [`BlameWhy::token`]. Monotonic for the window's lifetime, so a
    /// token can never be reused by a later request.
    pub blame_why_seq: u64,
    /// Bumped whenever inlay hints are turned off, invalidating every request
    /// still in flight (see [`crate::app::message::LspMsg::InlayHintsLoaded`]).
    pub inlay_gen: u64,
    /// Whether the "Language Servers" management panel is open.
    pub server_panel: bool,
    /// Installed servers listed in the management panel (name, version, bytes).
    pub installed_servers: Vec<lsp::store::InstalledServer>,
    /// Per language, where its server would come from (the panel's row
    /// status), as the last background listing found it. Probing the store
    /// is filesystem work, so the view reads this instead of probing per
    /// frame.
    pub lsp_located: HashMap<String, LocatedKind>,
    /// Mints server-panel listings (see `LspMsg::PanelListed::seq`).
    pub server_panel_seq: u64,
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
    /// The status line has said, in the wait a close or a quit is in, that
    /// the window waits for its host (`App::show_waiting`): said once, it is
    /// not said again over what the line says since, unless that is taken
    /// back (`App::update_waiting`). Cleared as the wait ends.
    pub waiting_said: bool,
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
    /// How many asynchronous messages `dispatch` dropped at its one ownership
    /// check (see `Message::origin`). Test instrumentation: it is how a test
    /// shows that a stale message was stopped THERE, before any handler.
    #[cfg(test)]
    pub stale_dropped: usize,
    /// The WALK tab's own controls (they outlive a project: see [`WalkUi`]).
    pub walk_ui: WalkUi,
    /// The DOCS tab's view options (they outlive a project: see [`DocsView`]).
    pub docs_view: DocsView,
}

/// One entry or TOML-key change to a remote mergeable store, kept until the server
/// acknowledges it (see `ProjectSession::remote_edits`).
#[derive(Debug, Clone)]
pub struct RemoteEdit {
    /// The store, under `.clew/`.
    pub rel: String,
    /// The idempotency key (`Request::EditState::edit_id`): the same for
    /// every sending of this change, so the server applies it once.
    pub id: String,
    pub merge: clew_protocol::StateMerge,
    /// The request carrying it over the CURRENT transport; `None` until it
    /// has been sent on it (and again once that transport died).
    pub request: Option<u64>,
    /// A host may have applied it, or may yet: it is on the wire, or it was
    /// when a transport died ([`Self::adrift`]). Such an edit is sent again,
    /// never left out. `false` until it is first sent, and again once the
    /// host answers that it could not apply it (`Failed`) — unless it is
    /// adrift: a later change to its entry may then take its place
    /// (`App::drop_superseded`).
    pub may_have_landed: bool,
    /// It was on the wire when a transport died, unanswered: the host that
    /// took it may apply it at any time — its worker slow, the last frames
    /// read after the next transport replayed the edit — whatever a later
    /// host answers of its replay. Left out once the next host had failed
    /// the replay, it landed after the edit that took its place, over it: a
    /// note ended as its first save. Set for good.
    ///
    /// What this does NOT close: an adrift edit the next host fails
    /// [`crate::app::remote_state::EDIT_ATTEMPTS`] times is given up, said
    /// to be lost, and the edits after it go; the host it went down with
    /// may still apply it after them. That takes that host's worker holding
    /// the frame through every pause of the retries — seconds — and the
    /// next host failing the store each time; the ledger that de-duplicates
    /// replays cannot order a writer it never hears from again.
    pub adrift: bool,
    /// Transient failures so far: the server could not write it (an I/O
    /// error, a lock, a panic). This client's request queue being full is
    /// none: the edit goes with the next tick, or answer, that finds room
    /// (`App::send_remote_edits_due`).
    pub failures: u32,
    /// Held back until then after such a failure — or due at once, after a
    /// full queue; the tick sends it again.
    pub retry_at: Option<std::time::Instant>,
}

/// The journaled edits given up while a window closes, kept to be named in
/// its questions (see `ProjectSession::remote_edits_lost`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LostEdits {
    /// The first [`crate::app::remote_state::LOST_KEPT`] of them: each by
    /// its id (`RemoteEdit::id`), with the entry it changed as the user
    /// knows it (`remote_state::entry_name`) — not the edit itself, which
    /// for a walkthrough carries the whole tour.
    pub kept: Vec<(String, String)>,
    /// The ones given up past those: by their ids alone, which the user's
    /// agreement to lose them names (`crate::shell`), while a question says
    /// only how many they are. Counted without their ids, one given up after
    /// a question was asked went unasked about.
    pub unnamed: Vec<String>,
}

/// Mints edit ids for one project session: a random prefix drawn at the
/// first edit, then a counter — unique across windows, sessions and
/// machines editing the same remote store, since the server keeps the ids it
/// applied per store (`clew_core::statefile::merge_file`).
#[derive(Debug, Default)]
pub struct EditIds {
    prefix: Option<String>,
    next: u64,
}

impl EditIds {
    /// The next id: `<32 random hex digits>-<n>`.
    pub fn mint(&mut self) -> String {
        let prefix = self.prefix.get_or_insert_with(random_hex_128);
        self.next += 1;
        format!("{prefix}-{}", self.next)
    }
}

/// 128 random bits as 32 hex digits: from the OS (`/dev/urandom`), or — only
/// if that cannot be read — from std's randomly keyed hasher over the time,
/// the process id and a counter.
fn random_hex_128() -> String {
    use std::io::Read;
    let mut bytes = [0u8; 16];
    let from_os = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if from_os.is_err() {
        use std::hash::{BuildHasher, Hasher};
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for half in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
            h.write_u32(std::process::id());
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default(),
            );
            half.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The import graph's queue of changes and its single off-thread job (see
/// `App::schedule_imports`).
#[derive(Debug, Default)]
pub struct ImportWork {
    /// Changes not yet handed to a job, merged per file.
    pub pending: imports::ImportBatch,
    /// A job is running. One at a time: it applies its batch to a copy of the
    /// graph the window shows, and a second job alongside it would compute
    /// from a graph without the first one's changes.
    pub running: bool,
    /// The batch the running job applies, held until it lands (the job
    /// reads it here): a job that fails hands nothing back, and its changes
    /// run once more (`retry`) instead of being lost.
    pub in_flight: Option<Arc<imports::ImportBatch>>,
    /// The batch of a job that failed, to run once more before anything
    /// queued since — which changes the same files again, so it applies
    /// after it (`ImportBatch::retry_before`). A second failure drops it:
    /// requeued for good, a batch that failed every time failed every job
    /// after it, and the graph froze.
    pub retry: Option<imports::ImportBatch>,
    /// The running job is that second run.
    pub retrying: bool,
    /// The resolver the last job built, reused by the next unless its batch
    /// asks for a new one — which everything that can change what a resolver
    /// is built from does (`ImportBatch::reresolve`).
    pub resolver: Option<Arc<imports::Resolver>>,
    /// Lay the overview's module map out again when the next job lands.
    pub refresh_overview: bool,
    /// How many jobs have started: the number of the one running, or of the
    /// last one that ran — single flight, so a job that lands is always the
    /// last one started.
    pub started: u64,
    /// The call graph is due a refresh once the import graph has caught up:
    /// files it is linked from were parsed (`App::on_files_indexed`),
    /// deleted, or published by a remote host (`Event::ProjectSymbols`), and
    /// their imports — the scope it links through — resolve in a job still
    /// to land. Built at once, against the scope from before, it was stale
    /// the moment that job moved the scope, and built a second time — on a
    /// remote project, a second whole-project `ProjectCalls` on the host.
    ///
    /// The number of the job whose landing refreshes it: the first to carry
    /// a change it waits on (`App::refresh_deferred_calls`). Not "when no job
    /// is left running": a steady stream of edits keeps one running, and the
    /// refresh never came.
    pub refresh_calls: Option<u64>,
}

/// A map from each of the project's files to what the index holds for it —
/// its symbol-index entries, its call sites — kept as ONE shared map, so the
/// work the window hands off (an incremental LSP refine pass, a call-graph
/// build) takes all of it as a single `Arc` clone, however many files the
/// project has. Handed one `Arc` per file instead, each such pass cost the
/// window a step that grew with the project, once per watcher batch.
///
/// It is written in place (`Arc::make_mut`) while nothing else holds it, so
/// a batch costs what it changes. A write while something still does copies
/// the whole map first — which is why what it is handed to lets its share go
/// as soon as it has read what it needs.
#[derive(Debug)]
pub struct PerFile<V>(Arc<HashMap<PathBuf, V>>);

impl<V> PerFile<V> {
    /// Remove every entry. A map something else still holds is let go, not
    /// copied to be emptied.
    pub fn clear(&mut self) {
        match Arc::get_mut(&mut self.0) {
            Some(map) => map.clear(),
            None => self.0 = Arc::default(),
        }
    }

    /// How many hold the map: the window, and each pass handed it that has
    /// not let its share go yet.
    #[cfg(test)]
    pub(crate) fn holders(&self) -> usize {
        Arc::strong_count(&self.0)
    }

    /// A handle to the map that is no share of it, which still tells how
    /// many hold it (`Weak::strong_count`).
    #[cfg(test)]
    pub(crate) fn downgrade(&self) -> std::sync::Weak<HashMap<PathBuf, V>> {
        Arc::downgrade(&self.0)
    }
}

impl<V: Clone> PerFile<V> {
    /// Set `path`'s entry, returning the one it replaces.
    pub fn insert(&mut self, path: PathBuf, value: V) -> Option<V> {
        Arc::make_mut(&mut self.0).insert(path, value)
    }

    /// Remove `path`'s entry, returning it. A path without one leaves the map
    /// as it is: shared, it is not copied for nothing.
    pub fn remove(&mut self, path: &Path) -> Option<V> {
        if !self.0.contains_key(path) {
            return None;
        }
        Arc::make_mut(&mut self.0).remove(path)
    }
}

impl<V> Clone for PerFile<V> {
    /// Another share of the same map: one `Arc` clone, whatever its size.
    fn clone(&self) -> Self {
        PerFile(Arc::clone(&self.0))
    }
}

impl<V> Default for PerFile<V> {
    fn default() -> Self {
        PerFile(Arc::default())
    }
}

impl<V> From<HashMap<PathBuf, V>> for PerFile<V> {
    fn from(map: HashMap<PathBuf, V>) -> Self {
        PerFile(Arc::new(map))
    }
}

impl<V> std::ops::Deref for PerFile<V> {
    type Target = HashMap<PathBuf, V>;

    fn deref(&self) -> &HashMap<PathBuf, V> {
        &self.0
    }
}

/// Everything the window holds for the OPEN project: the project itself, what
/// was derived from it, what the reader built up in it, the surfaces that
/// show it, and the work in flight for it.
///
/// `App::proj` is replaced whole — never reset field by field — when the
/// project is left (another one is installed, or the transport switches:
/// see `App::forget_project_state`), and [`ProjectLink`] likewise when the
/// transport under it dies. A field added here is therefore reset by
/// construction; the reset lists that used to live in `on_scan_done`,
/// `drop_project_work`, `drop_connection_state` and `connect_to` (and that
/// kept missing fields: time travel, the diff and blame popups, note
/// editors, caches) are gone. What leaving still does explicitly is STOP
/// work that runs elsewhere (a pass on the blocking pool, an agent turn on
/// the server, a debuggee) — see `App::drop_project_work` — and say what the
/// user loses (an unsaved draft, unsent remote state).
///
/// Its request sequences (`search_seq`, `explain.generation`, …) start over
/// with it: every async result carries a [`crate::Stamp`] of the project
/// instance it was spawned for and is dropped in `dispatch` once that
/// instance is gone, so a sequence only has to order work WITHIN one
/// session.
#[derive(Debug)]
pub struct ProjectSession {
    // -- The project and its index -----------------------------------------------
    /// The open project: its root, tree and file list (`None` while none is).
    pub project: Option<Project>,
    /// This project's derived-artifact directory (`clew_core::derived::dir`),
    /// inside clew's own data dir — never inside the project, whose contents
    /// the repository controls. `None` when there is no data directory, in
    /// which case every derived cache runs in memory for the session.
    pub derived_dir: Option<PathBuf>,
    /// Languages actually present in the project that clew can serve.
    pub project_languages: Vec<String>,
    /// Whole-project content-hash oracle: the authority on what changed, so the
    /// watcher's noisy events collapse to real byte changes.
    pub registry: incremental::Registry,
    /// The flattened view of `symbol_index_by_file` the symbol finder
    /// consumes (its results are indices into it). Flattening copies every
    /// symbol of the project, so it is NOT redone per change: a change marks
    /// it stale (`symbol_index_stale`) and it is rebuilt when the finder needs
    /// it — at once while the finder shows symbols, else when it next does
    /// (`App::fresh_symbol_index`).
    pub symbol_index: Arc<Vec<SymbolEntry>>,
    /// `symbol_index` no longer reflects `symbol_index_by_file`.
    pub symbol_index_stale: bool,
    /// Symbol index kept per file so a single file can be re-indexed in place;
    /// `symbol_index` is the flattened view the finder consumes. One shared
    /// map ([`PerFile`]): an incremental refine pass and a call-graph build
    /// take it whole, as one `Arc`.
    pub symbol_index_by_file: PerFile<Arc<Vec<SymbolEntry>>>,
    /// Per file, the call sites the symbol index read off the same parse
    /// (`index::FileFacts::calls`) — what the local project call graph is
    /// built from (`ProjectCallGraph::build_from_calls`), so that build
    /// parses nothing a second time. Kept in step with
    /// `symbol_index_by_file`: the index build, each watcher batch and each
    /// deletion update both. Only files that contribute (non-empty calls).
    /// Shared the same way ([`PerFile`]).
    pub calls_by_file: PerFile<Arc<projectcalls::FileCalls>>,
    /// Bumped by every `App::rebuild_symbol_index` — every change to the
    /// index goes through it — so view work derived from the index (the
    /// project-calls overlay's test filter) can be memoized by it.
    pub symbol_index_rev: u64,
    /// The whole-project symbol index is being built (warm start).
    pub indexing: bool,
    /// What the last symbol-index build left out (its file / size caps), so
    /// the trees and the finder can say the index is incomplete instead of
    /// silently missing files.
    pub index_cap_note: Option<String>,
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
    /// Whole-project file→file import graph, derived from tree-sitter and kept
    /// incrementally fresh; the Imports sidebar tab is a view onto it. Written
    /// only by the import-graph job (`App::queue_imports`), off the UI thread:
    /// the window queues changes and swaps each result in.
    pub import_graph: Arc<imports::ImportGraph>,
    /// Bumped whenever a job changes `import_graph`'s edges, so view work
    /// derived from them is memoized by it.
    pub import_graph_rev: u64,
    /// Bumped whenever a job changes which project files some file imports:
    /// what `ImportGraph::scope_map` gives the call graph
    /// (`imports::Applied::scope_changed`). External and unresolved imports,
    /// edge kinds and lines moving do not count.
    pub import_scope_rev: u64,
    /// The import graph's queued changes and its single job (see [`ImportWork`]).
    pub import_work: ImportWork,
    /// The import tree currently shown, rooted at the active file.
    pub import_tree: Option<imports::ImportTree>,
    /// The identity of `import_tree`, minted (`App::mint_request_id`) every
    /// time a tree is installed: its rows' clicks carry it
    /// (`GraphMsg::ImportExpand`), so a click drawn from a replaced tree is
    /// recognized and dropped.
    pub import_tree_token: u64,
    /// Import cycles in the project, recomputed when the graph changes (cached so
    /// the sidebar banner doesn't re-run cycle detection every frame).
    pub import_cycles: Vec<Vec<PathBuf>>,
    /// How often each file changed over the recent history, for the graphs'
    /// change-frequency overlay: loaded when a graph overlay opens
    /// (`App::ensure_churn`), kept for a while (`churn_at`), `None` for a
    /// project without git or before the first load.
    pub churn: Option<Arc<Churn>>,
    /// When `churn` was last loaded, or last failed to.
    pub churn_at: Option<std::time::Instant>,
    /// Bumped with every new `churn`: what the map's paint is keyed by.
    pub churn_rev: u64,
    /// A churn load is in flight.
    pub churn_loading: bool,
    /// The project's types and their relations (the type map), built from
    /// the Docs index and the structure index (`App::rebuild_type_graph`).
    pub type_graph: Arc<typegraph::TypeGraph>,
    /// The `(docs generation, structure revision)` `type_graph` was built
    /// from; another pair means it is stale.
    pub type_graph_key: Option<(u64, u64)>,
    /// The Imports overlay's counts and rankings, computed by the import job
    /// (off the UI thread) whenever the graph's structure changed.
    pub(crate) import_ranks: crate::ui::ImportRanks,
    /// Import-resolution metadata (go.mod module, pubspec package name) from
    /// the server's project snapshot — a remote project's resolver must not
    /// read those files off the local disk. `None` until a full snapshot
    /// arrives.
    pub remote_import_meta: Option<(Option<String>, Option<String>)>,
    /// A remote project's tsconfig/jsconfig path mappings, fetched from the
    /// host (`imports::load_remote_ts_configs`) — a remote resolver cannot
    /// read them off the local disk. Shared with every import job's
    /// resolver, not copied into it.
    pub remote_ts_configs: Option<Arc<imports::TsConfigs>>,
    /// Bumped by every fetch of `remote_ts_configs` and every drop of them:
    /// only the result of the latest fetch applies (`RemoteTsConfigsLoaded`),
    /// so an older one still in flight cannot bring back what a later change
    /// removed.
    pub remote_ts_configs_gen: u64,
    /// The generation of the fetch that retries one that failed: failing
    /// too, it is not tried again (`App::on_remote_ts_configs_loaded`).
    pub remote_ts_configs_retry: Option<u64>,
    /// What the tsconfig/jsconfig caps left out of alias resolution, as last
    /// said in the status line (`imports::TsConfigs::cap_note`): said again
    /// only when it changes, not for every resolver built.
    pub ts_cap_note: Option<String>,
    /// The whole-project symbol call graph and its LSP-refine state (see
    /// [`ProjectCallsState`]).
    pub project_calls: ProjectCallsState,
    // -- The reader's surfaces ---------------------------------------------------
    /// Code panes; pane 1 exists only in split view.
    pub panes: [Option<Viewer>; 2],
    pub split: bool,
    pub active: usize,
    pub expanded: HashSet<String>,
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
    /// An open right-click navigation menu: (pane, line, col, window x, y).
    pub context_menu: Option<ContextMenu>,
    /// When set, the active pane shows this file's diff against `HEAD`.
    pub diff: Option<DiffState>,
    /// The diff load in flight: its request stamp and the file. A second
    /// toggle while it loads cancels it (clears this), so its reply is
    /// dropped instead of opening the view the user just turned off.
    pub diff_pending: Option<(u64, PathBuf)>,
    /// Per file, the newest blame request (local `git::try_info` pass or
    /// server `GitInfo`, one id space). A `GitInfoLoaded` paints only when it is
    /// still the newest request for its file.
    pub git_latest: HashMap<PathBuf, u64>,
    /// The "Why is this here?" popup, when open.
    pub blame_why: Option<BlameWhy>,
    /// The active git time-travel session, if any.
    pub time_travel: Option<TimeTravel>,
    /// Numbers the time-travel requests — each start (`time_start`), each
    /// revision a session asks for (`TimeTravel::generation`) — so no two
    /// share one. A reply applies only while its own request is still the
    /// one awaited: a number minted since, for another request, leaves it
    /// be. (Checked against this counter's latest value instead, a start's
    /// history was dropped by any step or exit made while it loaded.)
    pub time_gen: u64,
    /// The start whose history is still loading, if any: the one
    /// `TimeTravelMsg::Ready` that applies. Given up — its history dropped
    /// as it lands, what it held back settled at once — by a newer start,
    /// or a newer request its landing would undo
    /// (`App::give_up_time_travel_start`): a jump it would undo
    /// (`App::jump_gives_up_time_start`); a step, a selection or a story
    /// made of the session it would replace, or a summary unless it only
    /// re-scopes that session, which keeps its summaries
    /// (`App::time_travel_start_gives_way`); leaving that session while its
    /// own file's history loads; the split closing under it
    /// (`App::on_toggle_split`).
    pub time_start: Option<TimeStart>,
    /// The active project-graph modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// Precomputed force-directed layout for the current overlay's map.
    pub graph_layout: Option<graphlayout::Layout>,
    /// The overlay `graph_layout` was laid out for: shown again when that
    /// overlay reopens, until a fresh one lands — never under another.
    pub graph_layout_for: Option<Overlay>,
    /// The revision of `graph_layout`'s content (`ui::next_layout_rev`),
    /// bumped wherever it is installed or edited in place — what tells the
    /// map's cached drawing it is stale.
    pub graph_layout_rev: u64,
    /// The number of the layout requested last (`App::refresh_graph_layout`):
    /// only the one that lands with it applies (`GraphMsg::GraphLaidOut`) —
    /// one laid out for a graph since replaced, or an overlay since left, is
    /// dropped.
    pub graph_layout_seq: u64,
    /// A layout is being computed, off the UI thread: the map says so while
    /// it has none to show.
    pub graph_layout_pending: bool,
    /// Lookup tables for turning `path:line` citations in LLM prose into
    /// links, built once per project file list rather than on every
    /// `prepare_segments` call (see `ProjectSession::citation_index`).
    pub citation_index: Option<CitationIndex>,
    /// The view's memos: whole-collection view work kept per generation of
    /// its data instead of redone on every repaint (see `ui::ViewMemo`).
    pub view_memo: crate::ui::ViewMemo,
    // -- The reading session -----------------------------------------------------
    pub history: History,
    /// Trail nodes whose subtree is collapsed in the TRAIL view.
    pub trail_collapsed: std::collections::HashSet<usize>,
    /// The `History::renumbering` stamp `trail_collapsed` was recorded under:
    /// when the history's node ids change meaning (an eviction renumbers the
    /// survivors, a clear empties the tree) the marks are dropped (see
    /// `App::sync_trail_collapsed`).
    pub trail_renumbering: u64,
    pub bookmarks: Vec<Bookmark>,
    /// Editing a bookmark note: (rel path, 1-based line, draft note text).
    pub note_edit: Option<(String, usize, String)>,
    /// Per-project reading notes / progress, anchored by (rel, symbol name).
    pub notes: Vec<notes::Note>,
    /// Editing a reading note: (rel path, symbol name, draft note text).
    pub reading_note_edit: Option<(String, String, String)>,
    /// Target the `#[cfg]` dimming is evaluated against (host, or one the reader
    /// picks to study another platform's branches). Persisted per project.
    pub reading_target: inactive::Target,
    /// Editing a breakpoint condition: (file, 1-based line, draft expression).
    /// Anchored to a file of this project, so it goes with it (the
    /// breakpoints themselves persist, in `DebugState`).
    pub bp_cond_edit: Option<(PathBuf, usize, String)>,
    // -- Navigation --------------------------------------------------------------
    pub finder: Finder,
    pub search: SearchState,
    /// Monotone search-submission counter. Local `SearchDone` / LSP
    /// `ReferencesResult` carry the value minted at their request; only the
    /// latest submission may paint the Search sidebar.
    pub search_seq: u64,
    /// Monotone go-to-definition counter, same pattern as `search_seq`: a
    /// `DefinitionResult` from a superseded request must not jump the editor.
    pub goto_seq: u64,
    /// The call hierarchy shown in the Calls sidebar tab, if any.
    pub call_graph: Option<callgraph::CallTree>,
    /// The token of the `CallsMsg::Prepared` currently awaited, if any.
    pub call_pending: Option<u64>,
    /// The value trace shown in the FLOW sidebar tab, if any.
    pub flow: Option<crate::app::flow::FlowTree>,
    /// The token of the `FlowMsg::Found` currently awaited, if any.
    pub flow_pending: Option<u64>,
    // -- Generated understanding -------------------------------------------------
    /// The Explain feature's state — the explanation cache and the open
    /// explanation overlay (see [`ExplainState`]).
    pub explain: ExplainState,
    /// The architecture-overview "home" view's state (see [`OverviewState`]).
    pub overview: OverviewState,
    /// The Stats "home" view's state (see [`StatsState`]).
    pub stats: StatsState,
    /// The Walkthroughs feature — the saved-tour library and the reader/composer
    /// state for the WALK tab (see [`WalkState`]).
    pub walk: WalkState,
    /// The API documentation view's state (see [`DocsState`]).
    pub docs: DocsState,
    /// The Glossary page (`app::glossary`).
    pub glossary: crate::app::glossary::GlossaryState,
    /// Auto-refresh throttle: when the last refresh pass began (`None` until the
    /// first). A watched-file change starts a pass only once the cooldown has
    /// lifted; a manual refresh ignores it. Runtime-only (not persisted).
    pub last_auto_refresh: Option<std::time::Instant>,
    /// A source file changed during the cooldown — refresh when the window lifts
    /// (picked up by `Tick`), so no change is dropped.
    pub refresh_pending: bool,
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
    /// True while the embedding index is being (re)built.
    pub building_embeddings: bool,
    /// A build asked for while one was running: it goes out once that one
    /// lands (see `App::on_build_embeddings`).
    pub embed_build_pending: bool,
    /// The Semantic-tab query box and its ranked results.
    pub semantic_query: String,
    pub semantic_results: Vec<(explain::Node, f32)>,
    /// True while a semantic query is being embedded/searched.
    pub searching_semantic: bool,
    /// Mints semantic-search submissions (see `SemanticMsg::Results::seq`).
    pub semantic_seq: u64,
    // -- Ask ---------------------------------------------------------------------
    /// "Ask clew" Q&A: input, conversation, state.
    pub ask_input: String,
    pub ask_turns: Vec<AskTurn>,
    pub asking: bool,
    /// Code selections pinned as context, shown as chips above the input. They
    /// persist across turns until removed.
    pub ask_pins: Vec<AskPin>,
    // -- Language servers: configuration and consent (the running servers are in `link`) ----
    /// Per-project LSP config from `.clew/lsp.toml`.
    pub lsp_config: lsp::config::ProjectLspConfig,
    /// Per-language `init_options` a remote clew-server sent with
    /// `LspResolved` — the LSP handshake runs client-side even for a remote
    /// server, but the options belong to the host that owns the lsp.toml.
    pub remote_lsp_init: std::collections::HashMap<String, serde_json::Value>,
    /// The language-server (and debug-adapter) provisionings awaiting the
    /// user's consent, asked one at a time (see [`ConsentQueue`]).
    pub pending_lsp_consent: ConsentQueue,
    /// Files git TRACKS that the ignore rules (or a build-directory name)
    /// would have hidden, listed anyway by the scan — a local one's report,
    /// or the server's `Event::Tree` — marked in the file tree (see
    /// `fs_scan::ScanReport::tracked_ignored`).
    pub tracked_ignored: HashSet<String>,
    /// Raised when this project is left (the session is dropped whole): the
    /// debug-adapter install it consented to stops between chunks instead of
    /// downloading on for a project that is gone.
    pub adapter_install_cancel: RaiseOnDrop,
    /// A repo-specified language server awaiting the user's approval, with the
    /// exact command line to show them.
    pub pending_lsp_command: Option<PendingLspCommand>,
    // -- The remote `.clew/` state -----------------------------------------------
    /// Remote `.clew/` state files whose content has not arrived yet. Until it
    /// does, what this client holds for them is the empty baseline every
    /// remote project starts from — writing that back would replace the
    /// remote file, or delete it (an empty list serializes to `None`).
    pub remote_state_pending: HashSet<String>,
    /// Remote state files whose latest change is not known to be on the
    /// remote's disk: either it could not be sent yet (its load is still
    /// outstanding, or there is no transport), or it was sent and has not been
    /// acknowledged. A re-read keeps the user's version over the remote's for
    /// anything listed here, and the change is sent again once it can be — a
    /// mergeable store's edits one by one from [`Self::remote_edits`], a
    /// wholesale store's file whole.
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
    /// moment, and cleared only with the mark it qualifies. Only the stores
    /// written WHOLESALE (`history.json`) gets here: a change
    /// to a mergeable one waits in [`Self::remote_edits`] instead.
    pub remote_state_unsent: HashSet<String>,
    /// Entry or TOML-key changes to the REMOTE project's mergeable stores
    /// (bookmarks, notes, reading preferences, the walkthrough library) the server has not
    /// acknowledged, oldest first — see [`RemoteEdit`]. A change leaves only
    /// when its `StateEdited` (or a refusal) arrives; one whose transport
    /// died is sent again, under the same id, over the next, and the server
    /// applies each id once. So what a dead link ate costs neither this
    /// window's change nor another client's (the whole-list write this
    /// replaced overwrote whatever the other client had saved since).
    pub remote_edits: Vec<RemoteEdit>,
    /// Mints the ids of [`Self::remote_edits`] (see [`EditIds`]).
    pub remote_edit_ids: EditIds,
    /// The edits of [`Self::remote_edits`] given up — the server refused
    /// them, or they ran out of tries — since the window began to close:
    /// every question its close, or a quit, asks names them, until the
    /// window is in use again. `None` while it is — until it begins to
    /// close, and again once the user keeps it open, or cancels the quit,
    /// and no close or quit still under way may close it — when the status
    /// line says what was lost, in a window the user is in.
    pub remote_edits_lost: Option<LostEdits>,
    /// The journaled edit the status line says is tried again, by id, with
    /// the words it says (`remote_state::retrying_status`): taken back when
    /// that edit leaves the journal — saved, or left out after all — and by
    /// that edit alone, as two walkthroughs' names, cut to fit a status line,
    /// can read the same; handed on, while another edit of its entry is still
    /// tried again, to that one, which the same words speak for
    /// (`App::take_back_retrying`).
    pub remote_edit_retrying: Option<(String, String)>,
    // -- Work in flight ----------------------------------------------------------
    /// Work in flight for this project, beyond the busy flags the views read
    /// (see [`InFlight`]).
    pub inflight: InFlight,
    // -- Over the current transport ---------------------------------------------
    /// What the project holds over the current transport, dropped whole
    /// when that transport dies (see [`ProjectLink`]).
    pub link: ProjectLink,
}

impl Default for ProjectSession {
    /// The session of a window with no project open — and of every project at
    /// the moment it is installed, before anything was derived from it.
    fn default() -> Self {
        ProjectSession {
            project: None,
            derived_dir: Default::default(),
            project_languages: Default::default(),
            registry: Default::default(),
            symbol_index: Default::default(),
            symbol_index_stale: Default::default(),
            symbol_index_by_file: Default::default(),
            calls_by_file: Default::default(),
            symbol_index_rev: Default::default(),
            indexing: Default::default(),
            index_cap_note: Default::default(),
            structure: Default::default(),
            structure_rev: Default::default(),
            structure_building: Default::default(),
            structure_dirty: Default::default(),
            import_graph: Default::default(),
            import_graph_rev: Default::default(),
            import_scope_rev: Default::default(),
            import_work: Default::default(),
            import_tree: Default::default(),
            import_tree_token: Default::default(),
            import_cycles: Default::default(),
            churn: None,
            churn_at: None,
            churn_rev: 0,
            churn_loading: false,
            type_graph: Default::default(),
            type_graph_key: None,
            import_ranks: Default::default(),
            remote_import_meta: Default::default(),
            remote_ts_configs: Default::default(),
            remote_ts_configs_gen: Default::default(),
            remote_ts_configs_retry: None,
            ts_cap_note: None,
            project_calls: Default::default(),
            panes: Default::default(),
            split: Default::default(),
            active: Default::default(),
            expanded: Default::default(),
            find: Default::default(),
            hover: Default::default(),
            hover_gen: Default::default(),
            hover_pinned: Default::default(),
            context_menu: Default::default(),
            diff: Default::default(),
            diff_pending: Default::default(),
            git_latest: Default::default(),
            blame_why: Default::default(),
            time_travel: Default::default(),
            time_gen: Default::default(),
            time_start: Default::default(),
            overlay: Default::default(),
            graph_layout: Default::default(),
            graph_layout_for: Default::default(),
            graph_layout_rev: Default::default(),
            graph_layout_seq: Default::default(),
            graph_layout_pending: Default::default(),
            citation_index: Default::default(),
            view_memo: Default::default(),
            history: Default::default(),
            trail_collapsed: Default::default(),
            trail_renumbering: Default::default(),
            bookmarks: Default::default(),
            note_edit: Default::default(),
            notes: Default::default(),
            reading_note_edit: Default::default(),
            reading_target: inactive::Target::host(),
            bp_cond_edit: Default::default(),
            finder: Default::default(),
            search: Default::default(),
            search_seq: Default::default(),
            goto_seq: Default::default(),
            call_graph: Default::default(),
            call_pending: Default::default(),
            flow: None,
            flow_pending: None,
            explain: Default::default(),
            overview: Default::default(),
            stats: Default::default(),
            walk: Default::default(),
            docs: Default::default(),
            glossary: Default::default(),
            last_auto_refresh: Default::default(),
            refresh_pending: Default::default(),
            embed_index: Default::default(),
            building_embeddings: Default::default(),
            embed_build_pending: Default::default(),
            semantic_query: Default::default(),
            semantic_results: Default::default(),
            searching_semantic: Default::default(),
            semantic_seq: Default::default(),
            ask_input: Default::default(),
            ask_turns: Default::default(),
            asking: Default::default(),
            ask_pins: Default::default(),
            lsp_config: Default::default(),
            remote_lsp_init: Default::default(),
            pending_lsp_consent: Default::default(),
            tracked_ignored: Default::default(),
            adapter_install_cancel: Default::default(),
            pending_lsp_command: Default::default(),
            remote_state_pending: Default::default(),
            remote_state_dirty: Default::default(),
            remote_state_unsent: Default::default(),
            remote_edits: Default::default(),
            remote_edit_ids: Default::default(),
            remote_edits_lost: Default::default(),
            remote_edit_retrying: Default::default(),
            inflight: Default::default(),
            link: Default::default(),
        }
    }
}

/// What the open project holds over the CURRENT transport: requests awaiting
/// their replies, the server's publication counters, the Ask streams, and the
/// language servers (proxied through it, or — sharing their fate — spawned
/// locally). A transport that dies takes all of it along: `App::proj.link`
/// is replaced whole on a disconnect (`App::drop_connection_state`), and goes
/// with its [`ProjectSession`] when the project is left.
#[derive(Default, Debug)]
pub struct ProjectLink {
    /// In-flight `ReadFile` requests: id -> why it was asked, so the reply is
    /// applied correctly (open-and-jump vs reload-in-place).
    pub pending_reads: std::collections::HashMap<u64, ReadKind>,
    /// Per-pane: the request id of the file-open this pane is waiting for.
    /// A `FileContent` / `FileLoaded` result is applied only while its id is
    /// still the one the pane expects — a slower earlier open must not
    /// overwrite a faster later one, a load issued before a project switch or
    /// a split-close must not resurrect, and one issued before the reader went
    /// into the history of the file on screen must not land over it
    /// (`App::on_time_travel_start`; see `superseded_open`).
    pub pane_pending: [Option<u64>; 2],
    /// What each pane's pending open is opening, kept so that going into the
    /// history of the file on screen, which supersedes that open, can carry
    /// it out after all (`superseded_open`), and so that going back to the
    /// file on screen, which cancels it, takes back its "Loading…"
    /// (`App::open_file_at`). Stale once its request is no longer the pane's
    /// pending one.
    pub pane_opening: [Option<PaneOpen>; 2],
    /// The open the time-travel start still loading superseded
    /// (`ProjectSession::time_start`), until its history lands or the start
    /// is given up — its load's failure with it, should the file turn out
    /// not to be readable meanwhile (`SupersededOpen::failed`).
    pub superseded_open: Option<SupersededOpen>,
    /// In-flight `GitInfo` requests: id -> the file the blame was asked for.
    /// The reply paints THAT path, rather than re-deriving one from the
    /// current root and the reply's rel — which, after a project switch,
    /// resolves to a different project's file of the same name.
    pub pending_git: std::collections::HashMap<u64, PathBuf>,
    /// `GitInfo` requests retired while in flight (a newer one for the same
    /// file, or the file changed under it): their late replies — a refusal
    /// included — are nobody's answer any more and are dropped quietly,
    /// rather than reaching the status bar as an error about nothing the
    /// reader asked for. Each id leaves when its reply arrives.
    pub retired_git: HashSet<u64>,
    /// Request id of the in-flight server-side search, if any; its
    /// `SearchResults` reply is applied only while it is still the latest.
    pub pending_search: Option<u64>,
    /// The correlated requests the server may refuse as not ready yet
    /// (`ErrorCode::NotReady`, its scan window) — the Search and the
    /// `BuildDocs` this window waits on: request id -> the request to send
    /// again, and how many times it already was (see `App::send_retrying`).
    /// Each reply retires its entry.
    pub not_ready_retry: HashMap<u64, NotReadyRetry>,
    /// Request id of the in-flight `BuildDocs`, if any. Its reply — the
    /// `Docs` index, or an `Error` (e.g. the server's not-ready refusal) that
    /// stops the Docs spinner — is applied only while it is this one.
    pub pending_docs: Option<u64>,
    /// A symbol name whose doc page to open once that build's index arrives
    /// (set by "View docs" when the docs aren't built yet). It rides on the
    /// build request, so it dies with it: a refused build clears it, and a
    /// dead transport drops it with the link — left parked, it was aimed at
    /// the next SUCCESSFUL build, long after the reader moved on.
    pub pending_docs_view: Option<String>,
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
    /// The last `ProjectSymbols.seq` applied. The server stamps every
    /// publication (full or partial) in send order; anything at or below
    /// this is stale — most importantly a full snapshot that was still
    /// building while the watcher already sent fresher partial updates.
    /// Starts at 0 with every link: the counter is server-lifetime, and a new
    /// link is a new server (or a new project on it, whose publications
    /// always exceed 0).
    pub remote_index_seq: u64,
    /// The last server `Tree.seq` applied (the reply to `OpenProject`, or a
    /// watcher rescan). Trees are stamped when their scan STARTS, so one at or
    /// below this saw less than the tree on screen and is dropped. Starts at
    /// 0 with every link, like `remote_index_seq` (the counter is
    /// server-lifetime).
    pub server_tree_seq: u64,
    /// Request id -> rel for `WriteState`s (the stores written wholesale)
    /// awaiting their `StateWritten`. The mergeable stores' edits are tracked
    /// by `ProjectSession::remote_edits`, which outlives this link.
    ///
    /// A queued frame is NOT a durable write: a transport that has died
    /// without being detected accepts frames into a pipe that goes nowhere.
    /// Clearing the dirty mark on send therefore lost every trail entry saved
    /// in that window, and the reconnect's re-read then replaced them with the
    /// stale remote copy. The mark is cleared only when the server says the
    /// bytes reached the disk.
    pub remote_state_inflight: HashMap<u64, String>,
    /// Request id -> rel for the WHOLESALE writes that carry a change the
    /// server never received ([`ProjectSession::remote_state_unsent`]) — the
    /// rescue flushes, tracked apart from [`Self::remote_state_inflight`]
    /// because they answer a different question.
    ///
    /// `remote_state_inflight` answers "which change owns the dirty mark", so
    /// a newer write supersedes an older one there. The unsent mark is not
    /// about the newest change at all: it says one specific earlier change is
    /// missing from the remote file, and ONLY the flush that carries it can
    /// retire it. Superseding that id along with the dirty ownership left both
    /// marks set forever.
    ///
    /// An entry is safe to honour whenever it is acknowledged, because every
    /// `write_remote_state` caller serializes THIS WINDOW'S current copy,
    /// which by definition holds the unsent change.
    pub remote_state_rescue: HashMap<u64, String>,
    /// In-flight streaming chat answers: stream id -> the channel feeding the Ask
    /// flow. `ChatDelta` / `ChatStreamDone` notifications are routed here. Only
    /// ever touched on the UI thread, so a plain map (no lock).
    pub chat_streams: HashMap<u64, tokio::sync::mpsc::UnboundedSender<ChatStreamPiece>>,
    /// In-flight agent turns: stream id -> the channel feeding the Ask flow.
    /// `AgentStep` / `AgentDelta` / `AgentDone` notifications are routed here.
    /// UI-thread only, like `chat_streams`.
    pub agent_streams: HashMap<u64, tokio::sync::mpsc::UnboundedSender<AgentPiece>>,
    /// The stream id of the agent turn currently answering, for Stop.
    pub agent_stream: Option<u64>,
    /// Stream id of the in-flight server-side `ChatStream`, if any. Abandoning
    /// the answer (project switch, Ask Clear) must tell the SERVER to stop:
    /// dropping the client-side pump leaves the provider call running on the
    /// meter with nobody listening.
    pub chat_stream: Option<u64>,
    /// One server slot per language; a Ready client leaving it — this link
    /// dropped included — is stopped (see [`LspSlots`]).
    pub lsp: LspSlots,
    /// Per language, the cancel guard of its server download or toolchain
    /// install in flight: a newer install for the language, a restart
    /// (`App::reset_lsp`), or this link going (a disconnect, leaving the
    /// project) drops it, which stops the install instead of letting it run
    /// to its half-hour deadline for a result that would be dropped anyway.
    pub lsp_installs: HashMap<String, RaiseOnDrop>,
    /// Each READY server's state as read once per update (see
    /// `App::sync_lsp_snapshots`); the view takes diagnostics, progress and
    /// errors from here instead of locking the server's state per widget.
    pub lsp_snapshots: std::collections::HashMap<String, LspSnapshot>,
    /// The Language Servers panel's log of the active file's server, copied
    /// only when that log moved (see `App::sync_lsp_log`): the panel used to
    /// copy the whole log — up to a megabyte — on every repaint while open.
    pub lsp_log: Option<LspLogTail>,
    /// Documents already sent to a server via didOpen.
    pub lsp_opened: HashSet<PathBuf>,
    /// Last diagnostics version seen per language, to gate refresh ticks.
    pub seen_diag_version: std::collections::HashMap<String, u64>,
    /// Last inlay-hint refresh epoch seen per language, so a server-requested
    /// refresh re-fetches hints exactly once.
    pub seen_inlay_epoch: std::collections::HashMap<String, u64>,
}

/// What a pane's pending open is opening (`ProjectLink::pane_opening`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOpen {
    /// Its load: the pane's pending request while it is this one
    /// (`ProjectLink::pane_pending`).
    pub req: u64,
    /// The file, and the 1-based line to show.
    pub abs: PathBuf,
    pub target: Option<usize>,
}

/// An open that going into the history of the file on screen superseded in
/// its pane (`App::on_time_travel_start`), kept until that history lands.
/// Superseded for good if a session starts; carried out after all if none
/// does — git failed, or the file has no history — since the reader's later
/// request then came to nothing (`App::on_time_travel_ready`). So too if the
/// start is given up (`App::give_up_time_travel_start`), unless what gave it
/// up is newer in this pane. Carried out, an open whose load failed while
/// it was held says why, rather than asking for the file again
/// (`failed`, `App::carry_out_superseded`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersededOpen {
    /// The time-travel start that superseded it (`TimeStart::generation`).
    pub generation: u64,
    pub pane: usize,
    pub open: PaneOpen,
    /// What its load's failure says, when the file could not be read while
    /// the open was held (`App::fail_superseded_open`): kept quiet until the
    /// open is carried out — "Loading history…" stays up meanwhile — and
    /// said then in place of a second read, which would only be refused
    /// again. Dropped with the open if a session starts.
    pub failed: Option<String>,
}

/// A time-travel start whose history is still loading
/// (`ProjectSession::time_start`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeStart {
    /// Its request (`ProjectSession::time_gen`), which the `Ready` bringing
    /// its history names.
    pub generation: u64,
    /// The file whose history it asked for.
    pub abs: PathBuf,
    /// The pane it was asked in, which shows that file until the history
    /// lands: the start holds back any other open there
    /// (`App::on_time_travel_start`), a jump there gives the start up, and
    /// closing the split moves it to the pane left, or gives it up
    /// (`App::jump_gives_up_time_start`, `App::on_toggle_split`).
    pub pane: usize,
    /// It re-scopes the session on screen: its history takes that
    /// session's place, and keeps where it was, as it lands
    /// (`App::on_time_travel_ready`). Any other start's history starts a
    /// session of its own, from its live file.
    pub rescope: bool,
}

pub const DEFAULT_FONT_SIZE: f32 = 13.0;
