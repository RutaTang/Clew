//! The project-wide graphs: the import graph (resolution, remote tsconfig
//! paths, cycles, the Imports tree), the symbol call graph (build, LSP refine,
//! incremental refine), and the overlay maps drawn from them.
//!
//! Its messages, [`GraphMsg`], arrive through `App::update_graph`.

use std::collections::BTreeSet;

use crate::app::prelude::*;
use crate::*;

/// How many commits back the change-frequency overlay looks.
pub(crate) const CHURN_COMMITS: usize = 300;
/// How long a loaded change history is trusted before an overlay's opening
/// asks git again.
const CHURN_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether a file's language is one clew fully supports in the graphs (the six
/// with import/call extraction). Files in any other language are kept out of the
/// Import and Call graphs entirely, so every node has a real language colour.
pub(crate) fn graph_language(path: &std::path::Path) -> bool {
    crate::highlight::detect(path).is_some_and(graph_language_name)
}

/// [`graph_language`], by the language's name.
fn graph_language_name(lang: &str) -> bool {
    matches!(
        lang,
        "rust" | "javascript" | "typescript" | "tsx" | "python" | "go" | "dart"
    )
}

/// The same stamp for `project_calls.rev`, written when a build FAILED: the
/// graph on screen (if any) stays, but no longer reads as current, so the next
/// `ensure_call_graph` retries instead of trusting it.
pub(crate) const CALLS_REV_STALE: u64 = u64::MAX;

/// What the status line says to do once a refinement was handed back to the
/// name-based build (`App::drop_refinement`) with its servers still ready.
pub(crate) const REFINE_AGAIN: &str = "refine it with LSP for exact edges";

/// Why a refinement is handed back when the project's index lands after it
/// began: it read only what was indexed by then, and nothing notes what the
/// index adds.
pub(crate) const REFINED_BEFORE_INDEXED: &str =
    "The project finished indexing after the refine began";

/// How long the refine holds the changed files of a language whose server is
/// starting — restarted, or replacing one that died — before it hands the
/// refinement back (`App::fold_refine_pending`). A server that has not
/// started by then is not coming soon, and the files' old edges would pass
/// for exact meanwhile. The same bound, renewed, is how long a server that
/// is loading the project may go without reporting how it gets on: one
/// that has said nothing for that long is not loading, it is stuck.
#[cfg(not(test))]
pub(crate) const REFINE_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(test)]
pub(crate) const REFINE_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_millis(50);

/// The most the refine waits for a server that has started and is loading
/// the project, while it keeps reporting how it gets on
/// ([`REFINE_WAIT_LIMIT`] at a time). Loading takes longer than starting:
/// rust-analyzer took 75 s to load this repository — its build scripts run
/// first — and a large workspace's take minutes. Bounded all the same: a
/// server that reports work forever is not loading either.
#[cfg(not(test))]
pub(crate) const REFINE_LOAD_LIMIT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
#[cfg(test)]
pub(crate) const REFINE_LOAD_LIMIT: std::time::Duration = std::time::Duration::from_millis(400);

/// A bound on the refine's waits, as the status line says it: "60 s", or
/// "10 min".
fn in_words(bound: std::time::Duration) -> String {
    match bound.as_secs() {
        secs if secs >= 120 && secs.is_multiple_of(60) => format!("{} min", secs / 60),
        secs => format!("{secs} s"),
    }
}

/// What the LSP refine can do with a language's server now
/// (`App::fold_refine_pending`).
enum RefineServer {
    /// Ready, alive, and offering the call hierarchy: it is queried.
    Ready,
    /// Starting, or ready and still loading the project — where it answers
    /// from what it has loaded so far, with callers missing: the files wait
    /// for it, for a bounded time.
    Starting,
    /// Failed, absent, not offering the call hierarchy, or dead while still
    /// marked ready: the refinement cannot follow its files.
    Down,
}

/// How long a remote tsconfig/jsconfig fetch that failed waits before it is
/// tried once more (`App::on_remote_ts_configs_loaded`): a host that was busy
/// or not ready yet, asked again at once, answered the same.
#[cfg(not(test))]
pub(crate) const TS_CONFIGS_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(test)]
pub(crate) const TS_CONFIGS_RETRY_BACKOFF: std::time::Duration =
    std::time::Duration::from_millis(100);

/// A REMOTE project's import scope as the server takes it: each file's
/// project-relative path with the ones it imports.
fn remote_scope(
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
    root: &Path,
) -> Vec<(String, Vec<String>)> {
    scope
        .iter()
        .filter_map(|(file, imports)| {
            let rel = file.strip_prefix(root).ok()?;
            Some((
                rel.to_string_lossy().into_owned(),
                imports
                    .iter()
                    .filter_map(|i| i.strip_prefix(root).ok())
                    .map(|i| i.to_string_lossy().into_owned())
                    .collect(),
            ))
        })
        .collect()
}

/// Link the local project call graph from call sites already extracted (the
/// symbol index's, see `ProjectSession::calls_by_file`) — no file is read or
/// parsed here. The blocking half of `App::build_project_calls`.
pub(crate) fn local_call_graph(
    defs: Vec<projectcalls::Def>,
    calls: &[Arc<projectcalls::FileCalls>],
    scope: &HashMap<PathBuf, HashSet<PathBuf>>,
) -> projectcalls::ProjectCallGraph {
    projectcalls::ProjectCallGraph::build_from_calls(defs, calls.iter().map(Arc::as_ref), scope)
}

/// The call-graph languages `index` holds a callable function of, sorted
/// (`App::languages_left_out`): a walk of every file's entries, memoized by
/// the index's revision.
fn function_languages(
    index: &crate::app::state::PerFile<Arc<Vec<SymbolEntry>>>,
) -> Vec<&'static str> {
    let mut languages = BTreeSet::new();
    for (path, symbols) in index.iter() {
        let Some(lang) = highlight::detect(path).filter(|lang| graph_language_name(lang)) else {
            continue;
        };
        if !languages.contains(lang) && symbols.iter().any(|s| outline::is_callable(&s.kind)) {
            languages.insert(lang);
        }
    }
    languages.into_iter().collect()
}

/// Free `value` on the blocking pool, not on the thread that serves every
/// window: what an import job leaves behind (a graph, a resolver, a batch) is
/// a map per file, a key per entry, a set of every path.
fn free_off_thread(value: impl Send + 'static) -> Task<Message> {
    #[cfg(test)]
    HANDED_OFF.with(|n| n.set(n.get() + 1));
    Task::future(async move {
        let _ = tokio::task::spawn_blocking(move || drop(value)).await;
    })
    .discard()
}

#[cfg(test)]
thread_local! {
    /// How many values this thread has handed off ([`handed_off`]).
    static HANDED_OFF: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many values this thread has handed off to be freed on the blocking
/// pool ([`free_off_thread`]) — in tests only, where it tells a value a
/// message carried that `update()` let go from one the task it returned
/// frees.
#[cfg(test)]
pub(crate) fn handed_off() -> usize {
    HANDED_OFF.with(std::cell::Cell::get)
}

/// What a map is laid out from: the graph of the overlay it is for, shared
/// with the window (`App::refresh_graph_layout`).
enum LayoutInput {
    Imports(Arc<imports::ImportGraph>),
    Calls(Arc<projectcalls::ProjectCallGraph>),
    Types(Arc<typegraph::TypeGraph>),
}

impl LayoutInput {
    /// The layout — whole-graph work, for the blocking pool.
    fn lay_out(&self) -> graphlayout::Layout {
        match self {
            LayoutInput::Imports(graph) => import_graph_layout(graph),
            LayoutInput::Calls(graph) => calls_graph_layout(graph),
            LayoutInput::Types(graph) => types_graph_layout(graph),
        }
    }
}

/// Force-directed layout of the import graph `g`: nodes are files, sized by
/// fan-in+fan-out; edges are every internal import edge — `mod` declarations
/// and re-exports included, which the cycle detection (dependencies only)
/// leaves out. The cycles' rings are marked where it lands, as the cycles
/// are then (`mark_cycles`).
fn import_graph_layout(g: &imports::ImportGraph) -> graphlayout::Layout {
    // Only graph the fully-supported languages; edges to any excluded file
    // fall away since `idx` is built from this filtered set.
    let files: Vec<PathBuf> = g
        .files()
        .into_iter()
        .filter(|f| graph_language(f))
        .collect();
    let idx: HashMap<PathBuf, usize> = files
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, f)| (f, i))
        .collect();
    let nodes = files
        .iter()
        .map(|f| graphlayout::NodeInput {
            label: file_label(f),
            file: f.clone(),
            line: 1,
            weight: (g.fan_in(f) + g.fan_out(f) + 1) as f32,
            cyclic: false,
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

/// Force-directed layout of the file-aggregated call graph `g`: nodes are
/// files sized by call degree; edges are cross-file call flow.
fn calls_graph_layout(g: &projectcalls::ProjectCallGraph) -> graphlayout::Layout {
    let (all_files, all_edges) = g.file_graph();
    // Keep only the fully-supported languages, remapping edge indices onto
    // the filtered node set.
    let mut remap = vec![usize::MAX; all_files.len()];
    let mut files: Vec<PathBuf> = Vec::new();
    for (i, f) in all_files.iter().enumerate() {
        if graph_language(f) {
            remap[i] = files.len();
            files.push(f.clone());
        }
    }
    let edges: Vec<(usize, usize)> = all_edges
        .into_iter()
        .filter(|&(a, b)| remap[a] != usize::MAX && remap[b] != usize::MAX)
        .map(|(a, b)| (remap[a], remap[b]))
        .collect();
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
            line: 1,
            weight: (degree[i] + 1) as f32,
            cyclic: false,
        })
        .collect();
    graphlayout::layout(nodes, edges)
}

/// The type map's layout: a node per type, labelled by its name and opening
/// at its definition, weighted by how many relations it has; an edge per
/// related pair.
fn types_graph_layout(g: &typegraph::TypeGraph) -> graphlayout::Layout {
    let nodes = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, t)| graphlayout::NodeInput {
            label: t.name.clone(),
            file: t.file.clone(),
            line: t.line,
            weight: (g.fan_in(i) + g.fan_out(i) + 1) as f32,
            cyclic: false,
        })
        .collect();
    graphlayout::layout(nodes, g.layout_edges())
}

/// Mark the nodes of `layout` that are members of `cycles` — its cycle
/// rings, redrawn in place.
fn mark_cycles(layout: &mut graphlayout::Layout, cycles: &[Vec<PathBuf>]) {
    let cyclic: HashSet<&PathBuf> = cycles.iter().flatten().collect();
    for node in &mut layout.nodes {
        node.cyclic = cyclic.contains(&node.file);
    }
}

impl App {
    /// How often each file changed, for the map's heat and the overlays'
    /// MOST CHANGED list: asked of git (local or remote) when an overlay
    /// opens, unless a load is in flight or the last one is recent enough
    /// ([`CHURN_TTL`]). A project without git simply has none.
    pub(crate) fn ensure_churn(&mut self) -> Task<Message> {
        if self.proj.churn_loading
            || self
                .proj
                .churn_at
                .is_some_and(|at| at.elapsed() < CHURN_TTL)
        {
            return Task::none();
        }
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        self.proj.churn_loading = true;
        let stamp = self.stamp();
        Task::perform(
            async move {
                git.run::<Vec<clew_protocol::FileChurn>>(clew_protocol::GitOp::Churn {
                    commits: CHURN_COMMITS,
                })
                .await
            },
            move |result| {
                Message::Graph(GraphMsg::ChurnLoaded {
                    stamp: stamp.clone(),
                    result,
                })
            },
        )
    }

    pub(crate) fn on_churn_loaded(
        &mut self,
        result: Result<Vec<clew_protocol::FileChurn>, String>,
    ) -> Task<Message> {
        self.proj.churn_loading = false;
        self.proj.churn_at = Some(std::time::Instant::now());
        match result {
            Ok(files) => {
                let root = self
                    .proj
                    .project
                    .as_ref()
                    .map(|p| p.root.clone())
                    .unwrap_or_default();
                self.proj.churn = Some(Arc::new(Churn::from_files(&root, files, CHURN_COMMITS)));
                self.proj.churn_rev += 1;
            }
            Err(e) => {
                // No git, no history: nothing to colour by, and nothing to
                // say. Any other failure is worth a line.
                self.proj.churn = None;
                self.proj.churn_rev += 1;
                if !e.contains("not a git repository") {
                    self.status = format!("Couldn't read the change history: {e}");
                }
            }
        }
        Task::none()
    }

    /// The type map's graph, rebuilt from the Docs index and the structure
    /// index when either moved since the last build. Cheap enough to run on
    /// the UI thread: a pass over the index's items.
    pub(crate) fn rebuild_type_graph(&mut self) {
        let key = (self.proj.docs.generation, self.proj.structure_rev);
        if self.proj.type_graph_key == Some(key) {
            return;
        }
        let root = self
            .proj
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .unwrap_or_default();
        self.proj.type_graph = Arc::new(typegraph::TypeGraph::build(
            &root,
            &self.proj.docs.files,
            &self.proj.structure,
        ));
        self.proj.type_graph_key = Some(key);
    }

    pub(crate) fn on_open_overlay(&mut self, which: Overlay) -> Task<Message> {
        // The server panel and an overlay are mutually exclusive modals.
        self.server_panel = false;
        self.proj.overlay = Some(which);
        // A map laid out for another overlay is not shown under this one:
        // until its own lands, the map says what it waits for. One laid out
        // for this overlay is shown until a fresh one replaces it.
        if self.proj.graph_layout_for != Some(which) {
            self.proj.graph_layout = None;
            self.proj.graph_layout_for = None;
            self.proj.graph_layout_rev = ui::next_layout_rev();
        }
        let mut task = self.ensure_churn();
        if which == Overlay::ProjectTypes {
            // The map is drawn from the API index: asked for when stale (it
            // arrives through `apply_docs`, which redraws), and the graph
            // rebuilt from whatever index is here now.
            self.ensure_docs();
            self.rebuild_type_graph();
        }
        if which == Overlay::ProjectCalls {
            // The call graph is brought up to date on demand
            // (`ensure_call_graph`): rebuilt if what it was built from moved,
            // never twice at once — or, while the LSP refine owns it, refined
            // for the files changed since, never replaced by a name-based
            // build.
            let idle = !self.proj.project_calls.building;
            task = Task::batch([task, self.ensure_call_graph()]);
            // A build started here lays the map out when it lands; until then
            // the map says it is building — what it drew was laid out for
            // another graph.
            if idle && self.proj.project_calls.building {
                self.proj.graph_layout = None;
                self.proj.graph_layout_for = None;
                self.proj.graph_layout_rev = ui::next_layout_rev();
                self.proj.graph_layout_seq += 1;
                self.proj.graph_layout_pending = false;
                return task;
            }
        }
        Task::batch([task, self.refresh_graph_layout()])
    }

    /// Lay out the node-link map of whichever overlay is open, on the
    /// blocking pool: the call graph's file graph walks every function and
    /// every edge, the import graph every file and import, and the
    /// force-directed layout is quadratic in the nodes it keeps — once per
    /// build or refine that lands while the Calls map is open, on the thread
    /// that serves every window. The map shows what it has until the layout
    /// lands (`on_graph_laid_out`), and only the one requested last applies.
    pub(crate) fn refresh_graph_layout(&mut self) -> Task<Message> {
        // Whatever is in flight is superseded, the overlay closed included.
        self.proj.graph_layout_seq += 1;
        let seq = self.proj.graph_layout_seq;
        let (overlay, input) = match self.proj.overlay {
            Some(Overlay::ProjectImports) => (
                Overlay::ProjectImports,
                LayoutInput::Imports(self.proj.import_graph.clone()),
            ),
            Some(Overlay::ProjectCalls) => (
                Overlay::ProjectCalls,
                LayoutInput::Calls(self.proj.project_calls.graph.clone()),
            ),
            Some(Overlay::ProjectTypes) => (
                Overlay::ProjectTypes,
                LayoutInput::Types(self.proj.type_graph.clone()),
            ),
            None => {
                self.proj.graph_layout = None;
                self.proj.graph_layout_for = None;
                self.proj.graph_layout_rev = ui::next_layout_rev();
                self.proj.graph_layout_pending = false;
                return Task::none();
            }
        };
        self.proj.graph_layout_pending = true;
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || input.lay_out())
                    .await
                    .map_err(|_| "laying out the map failed unexpectedly".to_string())
            },
            move |layout| {
                Message::Graph(GraphMsg::GraphLaidOut {
                    stamp: stamp.clone(),
                    seq,
                    overlay,
                    layout,
                })
            },
        )
    }

    /// A map's layout landed: installed when it is the one requested last
    /// (`graph_layout_seq`) — the import map's with its cycle rings, as the
    /// cycles are now: a job can have moved them while it was computed.
    fn on_graph_laid_out(
        &mut self,
        seq: u64,
        overlay: Overlay,
        layout: Result<graphlayout::Layout, String>,
    ) -> Task<Message> {
        if seq != self.proj.graph_layout_seq {
            return Task::none();
        }
        self.proj.graph_layout_pending = false;
        match layout {
            Ok(mut layout) => {
                if overlay == Overlay::ProjectImports {
                    mark_cycles(&mut layout, &self.proj.import_cycles);
                }
                self.proj.graph_layout = Some(layout);
                self.proj.graph_layout_for = Some(overlay);
                self.proj.graph_layout_rev = ui::next_layout_rev();
            }
            // What the map showed stays; the next change lays it out again.
            Err(e) => self.status = format!("Couldn't lay out the map: {e}"),
        }
        Task::none()
    }

    /// What a resolver over the project's current file set is built from, as
    /// it stands now: the job that builds it runs off the UI thread (see
    /// [`App::schedule_imports`]), so the window hands over a snapshot — the
    /// shared file list, not a copy of it.
    pub(crate) fn resolver_inputs(&self) -> Option<ResolverInputs> {
        let project = self.proj.project.as_ref()?;
        // A remote project's resolver must not read go.mod/pubspec off the
        // local disk (the root is a remote path): the metadata comes with the
        // server's snapshot, or resolution runs without it. Its
        // tsconfig/jsconfig path mappings are fetched from the host
        // (`fetch_remote_ts_configs`) and apply once they have arrived.
        let remote = self.connection.is_remote().then(|| RemoteResolution {
            meta: self.proj.remote_import_meta.clone().unwrap_or_default(),
            ts_configs: self.proj.remote_ts_configs.clone(),
        });
        Some(ResolverInputs {
            root: project.root.clone(),
            files: project.files.clone(),
            remote,
        })
    }

    /// A resolver over the project's current file set, built here and now —
    /// for tests that resolve one specifier the way the graph would.
    #[cfg(test)]
    pub(crate) fn import_resolver(&self) -> Option<imports::Resolver> {
        self.resolver_inputs().map(|inputs| inputs.build())
    }

    /// Restate the whole import graph from a full snapshot's raw imports (a
    /// remote project's full `ProjectSymbols`): every change queued before it
    /// is superseded. The overview's module map is laid out again when the
    /// result lands, since it is drawn from the resolved graph.
    pub(crate) fn rebuild_import_graph(
        &mut self,
        files: HashMap<PathBuf, imports::FileImports>,
    ) -> Task<Message> {
        self.proj.import_work.refresh_overview = true;
        self.queue_imports_after_ts_configs(|batch| {
            batch.reset();
            for (path, imports) in files {
                batch.set(path, imports);
            }
        })
    }

    /// [`App::queue_imports`] for a change that can also have changed a
    /// REMOTE project's path-mapping configs (the file set was restated):
    /// they are settled before the job starts. A config deleted on the host
    /// drops its aliases at once (`fetch_remote_ts_configs`), so the one job
    /// resolves without them. Started first, the job still applied the
    /// deleted aliases, and dropping them queued a second whole-project job.
    /// A config that is there is fetched meanwhile, and re-resolves once it
    /// lands.
    pub(crate) fn queue_imports_after_ts_configs(
        &mut self,
        change: impl FnOnce(&mut imports::ImportBatch),
    ) -> Task<Message> {
        change(&mut self.proj.import_work.pending);
        let configs = self.fetch_remote_ts_configs();
        Task::batch([configs, self.schedule_imports()])
    }

    /// Fetch a REMOTE project's tsconfig/jsconfig files (and the bases they
    /// extend) from the host, for the `paths` / `baseUrl` aliases its imports
    /// resolve through — `@ui/Button` is a project file only once they are
    /// known. The configs arrive as `RemoteTsConfigsLoaded`; a local
    /// project's are read off disk by the import job, as it builds its
    /// resolver (`ResolverInputs::build`).
    pub(crate) fn fetch_remote_ts_configs(&mut self) -> Task<Message> {
        self.fetch_remote_ts_configs_after(std::time::Duration::ZERO)
    }

    /// [`App::fetch_remote_ts_configs`], asking the host only once `delay`
    /// has passed. It supersedes any fetch in flight at once, and is
    /// superseded by any started meanwhile.
    fn fetch_remote_ts_configs_after(&mut self, delay: std::time::Duration) -> Task<Message> {
        if !self.connection.is_remote() || !self.server.is_up() {
            return Task::none();
        }
        let Some(project) = self.proj.project.as_ref() else {
            return Task::none();
        };
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        let root = project.root.clone();
        // What this finds out supersedes any fetch still in flight, which
        // read the host before the change that asked for this one: landing
        // after it, an older fetch brought back the aliases of a config
        // deleted meanwhile.
        self.proj.remote_ts_configs_gen += 1;
        let generation = self.proj.remote_ts_configs_gen;
        if !imports::has_ts_configs(&files) {
            // The last config went on the host: its aliases go with it. They
            // stayed in force until the project was reopened. So does the
            // note on what the caps left out of them.
            self.say_ts_cap_note(None);
            let had = self
                .proj
                .remote_ts_configs
                .take()
                .is_some_and(|configs| !configs.is_empty());
            return if had {
                self.reresolve_import_graph()
            } else {
                Task::none()
            };
        }
        let (stamp, ai) = (self.stamp(), self.ai_client());
        Task::perform(
            {
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    // Each batch asked until the host has settled every path
                    // of it: a reply names what it had no room for.
                    let ai = &ai;
                    let fetch = move |rels| {
                        let ask =
                            move |rels| ai.request(clew_protocol::Request::ReadSources { rels });
                        sources::read(rels, ask)
                    };
                    imports::load_remote_ts_configs(&root, &files, fetch).await
                }
            },
            move |result| {
                Message::Graph(GraphMsg::RemoteTsConfigsLoaded {
                    stamp: stamp.clone(),
                    generation,
                    result,
                })
            },
        )
    }

    /// A remote project's path-mapping configs arrived: resolve through them
    /// — re-resolving the graph only when they changed. Only the latest
    /// fetch's result applies (`generation`): a later fetch, or the configs
    /// dropped since, describe the host as it is now. A failed fetch says
    /// what it means: alias imports stay unresolved on this host.
    pub(crate) fn on_remote_ts_configs_loaded(
        &mut self,
        generation: u64,
        result: Result<imports::TsConfigs, String>,
    ) -> Task<Message> {
        if generation != self.proj.remote_ts_configs_gen {
            return Task::none();
        }
        let retried = self.proj.remote_ts_configs_retry == Some(generation);
        match result {
            Ok(configs) => {
                // The second try worked: the status said it was under way.
                if retried {
                    self.status =
                        "Read tsconfig/jsconfig from the remote host on a second try".into();
                }
                if self.proj.remote_ts_configs.as_deref() == Some(&configs) {
                    return Task::none();
                }
                self.say_ts_cap_note(configs.cap_note());
                self.proj.remote_ts_configs = Some(Arc::new(configs));
                self.reresolve_import_graph()
            }
            // This fetch superseded any still in flight, whose result was
            // dropped however good: failing, it left the aliases as they
            // were before either, until a config changed again. It is
            // fetched once more instead — after a pause, as a host that was
            // busy answered the same when asked again at once — and said
            // so; failing again (or with no host to ask), what that leaves
            // is said.
            Err(e) => {
                if !retried && self.remote_ts_configs_fetchable() {
                    let retry = self.fetch_remote_ts_configs_after(TS_CONFIGS_RETRY_BACKOFF);
                    self.proj.remote_ts_configs_retry = Some(self.proj.remote_ts_configs_gen);
                    self.status = format!(
                        "Could not read tsconfig/jsconfig from the remote host ({e}) — trying \
                         once more"
                    );
                    return retry;
                }
                let again = if retried {
                    ", even on a second try"
                } else {
                    ""
                };
                let then = if self.proj.remote_ts_configs.is_some() {
                    "their `paths` aliases resolve as last read until a config changes"
                } else {
                    "their `paths` aliases stay unresolved until a config changes"
                };
                self.status = format!(
                    "Could not read tsconfig/jsconfig from the remote host{again} ({e}) — {then}"
                );
                Task::none()
            }
        }
    }

    /// Whether `fetch_remote_ts_configs` can fetch now: a remote project
    /// whose host is connected, and which lists a tsconfig/jsconfig.
    fn remote_ts_configs_fetchable(&self) -> bool {
        self.connection.is_remote()
            && self.server.is_up()
            && self.proj.project.as_ref().is_some_and(|project| {
                let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
                imports::has_ts_configs(&files)
            })
    }

    /// Say what the tsconfig/jsconfig caps left out of alias resolution
    /// (`imports::TsConfigs::cap_note`) — after the status line, like the
    /// index's cap note — when it changed since last said: a resolver is
    /// built for every re-resolve, and each would say it again.
    fn say_ts_cap_note(&mut self, note: Option<String>) {
        if note == self.proj.ts_cap_note {
            return;
        }
        if let Some(note) = &note {
            self.status = format!("{} — {note}", self.status);
        }
        self.proj.ts_cap_note = note;
    }

    /// Re-resolve every edge against a resolver built anew — after the file
    /// set (a file created, deleted or renamed), the go.mod / pubspec
    /// metadata or the tsconfig path maps changed, any of which can change
    /// how every file resolves.
    pub(crate) fn reresolve_import_graph(&mut self) -> Task<Message> {
        self.queue_imports(imports::ImportBatch::reresolve)
    }

    /// Queue a change for the import graph and start the job that applies it
    /// — unless one is already running: then the change waits in
    /// `ImportWork::pending`, merged per file with everything else that
    /// arrives meanwhile, and goes out as ONE batch when that job lands.
    pub(crate) fn queue_imports(
        &mut self,
        change: impl FnOnce(&mut imports::ImportBatch),
    ) -> Task<Message> {
        change(&mut self.proj.import_work.pending);
        self.schedule_imports()
    }

    /// Start the import-graph job for everything pending, if none is running.
    ///
    /// Nothing that scales with the project runs here, on the thread that
    /// serves every window: the job takes the pending batch, the graph the
    /// window shows (shared) and the resolver the previous job built (or
    /// what to build a new one from — reading go.mod, pubspec.yaml and the
    /// tsconfig files happens on the job's thread), applies the batch to a
    /// working copy (`ImportGraph::apply`: every file installed first, then
    /// resolved once, in proportion to what the batch can have changed), and
    /// finds the cycles of the result. The window only swaps the result in
    /// (`on_import_graph_updated`). Single flight: the graph is written by
    /// one job at a time, so a result never overwrites a change it did not
    /// see — that change is still pending, for the next job. The window keeps
    /// the batch until the job lands (`ImportWork::in_flight`), so a job that
    /// fails loses none of it: its batch runs once more, first
    /// (`ImportWork::retry`).
    pub(crate) fn schedule_imports(&mut self) -> Task<Message> {
        let work = &self.proj.import_work;
        if work.running || (work.retry.is_none() && work.pending.is_empty()) {
            return Task::none();
        }
        let Some(inputs) = self.resolver_inputs() else {
            return Task::none();
        };
        let work = &mut self.proj.import_work;
        // A failed job's batch goes ahead of what was queued since, which
        // changes the same files again and must land after it.
        let (batch, retrying) = match work.retry.take() {
            Some(retry) => (retry, true),
            None => (std::mem::take(&mut work.pending), false),
        };
        work.retrying = retrying;
        let batch = Arc::new(batch);
        let resolver = if batch.needs_new_resolver() {
            None
        } else {
            work.resolver.clone()
        };
        work.in_flight = Some(batch.clone());
        let refresh_overview = std::mem::take(&mut work.refresh_overview);
        work.running = true;
        work.started += 1;
        let base = self.proj.import_graph.clone();
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || run_import_job(base, batch, resolver, inputs))
                    .await
                    .map_err(|e| e.to_string())
            },
            move |result| {
                Message::Graph(GraphMsg::ImportGraphUpdated {
                    stamp: stamp.clone(),
                    result,
                    refresh_overview,
                })
            },
        )
    }

    /// An import-graph job landed: swap its graph in (it is the only writer,
    /// so nothing newer can be overwritten), keep its resolver for the next
    /// job, refresh what is drawn from the graph when its edges changed, and
    /// start the next job if changes arrived meanwhile. A job that failed is
    /// run once more, at once, and dropped if it fails again — and either way
    /// what arrived meanwhile is scheduled without waiting for another
    /// change.
    pub(crate) fn on_import_graph_updated(
        &mut self,
        result: Result<ImportJobDone, String>,
        refresh_overview: bool,
    ) -> Task<Message> {
        let work = &mut self.proj.import_work;
        work.running = false;
        // Single flight: the job that landed is the last one started.
        let landed = work.started;
        let applied = work.in_flight.take();
        let retried = std::mem::take(&mut work.retrying);
        let done = match result {
            Ok(done) => done,
            // The job panicked outside any one file's resolution (which
            // contains its own: `Applied::unresolvable`) and the graph stays
            // as it was. Said, not hidden. Its batch runs once more, ahead of
            // what arrived meanwhile, with any re-resolve or reset it carried
            // (`ImportBatch::retry_before`): dropped at once, the files it
            // changed kept their old edges, and every other file resolved
            // against the file set or configs it replaced. Failing again, it
            // is dropped: requeued for good, a batch that fails every time
            // failed every job after it, and the graph froze. The resolver
            // is released, on the blocking pool like everything a job
            // replaces, and the next job builds its own.
            Err(e) => {
                work.refresh_overview |= refresh_overview;
                let mut failed = applied.map(Arc::unwrap_or_clone);
                self.status = if retried {
                    format!(
                        "Could not update the import graph, even on a second try ({e}) — what \
                         that update carried was dropped, and applies when its files next change"
                    )
                } else if let Some(batch) =
                    failed.take_if(|batch| batch.retry_before(&work.pending))
                {
                    work.retry = Some(batch);
                    format!("Could not update the import graph ({e}) — trying once more")
                } else {
                    format!(
                        "Could not update the import graph ({e}) — the changes queued since \
                         take its place"
                    )
                };
                let released = free_off_thread((work.resolver.take(), failed));
                let next = self.schedule_imports();
                // A refresh this job was to bring waits for the one started
                // now — its batch run again, or what took its place — and is
                // made at once when none was: no job will bring it.
                let calls = if self.proj.import_work.running {
                    Task::none()
                } else {
                    self.refresh_deferred_calls(landed)
                };
                return Task::batch([released, next, calls]);
            }
        };
        // What the tsconfig/jsconfig caps left out, as a local job read the
        // configs off disk. A remote project's are the window's, and said as
        // they arrive (`on_remote_ts_configs_loaded`): a job spawned before
        // they did holds none, and its landing took the note back — to be
        // said a second time by the job after it.
        let ts_note = (!self.connection.is_remote()).then(|| done.resolver.ts_cap_note());
        // Installed even when no edge changed: the Rust facts and raw imports
        // it carries are what the next job builds on.
        let old = std::mem::replace(&mut self.proj.import_graph, done.graph);
        let old_resolver = self.proj.import_work.resolver.replace(done.resolver);
        // The graph and the resolver they replace, and the batch the job
        // applied, are freed on the blocking pool: a map per file, a key per
        // entry, a set of every path — whole-project work, however small the
        // batch was (a resolver is replaced by every re-resolve).
        let retired = free_off_thread((old, old_resolver, applied));
        if let Some((first, panic)) = done.applied.unresolvable.first() {
            // Each is left with its imports unresolved, and resolved again
            // when it changes; everything else in the batch landed. The first
            // one's panic is named in short, the whole of it on stderr.
            let others = done.applied.unresolvable.len() - 1;
            self.status = format!(
                "Could not resolve the imports of {} ({panic}){} — left unresolved",
                self.rel_of(first),
                match others {
                    0 => String::new(),
                    1 => " and 1 other file".to_string(),
                    n => format!(" and {n} other files"),
                }
            );
        } else if retried {
            self.status = "Updated the import graph on a second try".into();
        }
        if let Some(note) = ts_note {
            self.say_ts_cap_note(note);
        }
        let mut calls = Task::none();
        if done.applied.scope_changed {
            self.proj.import_scope_rev += 1;
            // A call graph linked against the scope before this one (the
            // first job lands after the index — a graph built in between had
            // no scope at all) is rebuilt while in use, and otherwise the next
            // time it is needed; one the LSP refine owns is left to it.
            calls = self.refresh_call_graph_in_use();
        }
        if done.applied.changed {
            self.proj.import_graph_rev += 1;
            if let Some(cycles) = done.cycles {
                self.set_import_cycles(cycles);
            }
            if let Some(ranks) = done.ranks {
                self.proj.import_ranks = ranks;
            }
            self.refresh_import_tree();
        }
        // The overview's module map is laid out from the resolved graph; one
        // drawn before the imports were resolved fills in now.
        let overview = if refresh_overview {
            self.refresh_overview_map()
        } else {
            Task::none()
        };
        let next = self.schedule_imports();
        Task::batch([
            retired,
            overview,
            calls,
            next,
            self.refresh_deferred_calls(landed),
        ])
    }

    /// Install the import cycles of the graph just swapped in, re-marking the
    /// open import map's cycle members in place rather than re-laying it out
    /// under the reader.
    fn set_import_cycles(&mut self, cycles: Vec<Vec<PathBuf>>) {
        self.proj.import_cycles = cycles;
        if self.proj.overlay == Some(Overlay::ProjectImports)
            && let Some(layout) = &mut self.proj.graph_layout
        {
            mark_cycles(layout, &self.proj.import_cycles);
            self.proj.graph_layout_rev = ui::next_layout_rev();
        }
    }

    /// The file the Imports tab is focused on — the active pane's file.
    pub(crate) fn import_focus(&self) -> Option<PathBuf> {
        self.active_viewer().map(|v| v.abs.clone())
    }

    /// Rebuild the import tree for the focus file, preserving the current
    /// direction and "expand all" state. Cheap — pure in-memory graph lookups.
    pub(crate) fn refresh_import_tree(&mut self) {
        let (Some(root), Some(focus)) = (
            self.proj.project.as_ref().map(|p| p.root.clone()),
            self.import_focus(),
        ) else {
            self.proj.import_tree = None;
            return;
        };
        let was_full = self.proj.import_tree.as_ref().is_some_and(|t| t.full);
        let mut tree =
            imports::ImportTree::new(&self.proj.import_graph, &root, focus, self.import_dir);
        if was_full {
            tree.expand_all(&self.proj.import_graph, &root);
        }
        self.proj.import_tree = Some(tree);
        // A new tree: clicks drawn from the one it replaces name its nodes.
        self.proj.import_tree_token = self.mint_request_id();
    }

    pub(crate) fn on_import_expand(&mut self, token: u64, id: usize) -> Task<Message> {
        // Only the tree the click was drawn from: node ids are bare indices,
        // and a rebuilt tree reuses them for other files.
        if token != self.proj.import_tree_token {
            return Task::none();
        }
        if let (Some(mut tree), Some(root)) = (
            self.proj.import_tree.take(),
            self.proj.project.as_ref().map(|p| p.root.clone()),
        ) {
            if id < tree.node_count() {
                tree.toggle(id, &self.proj.import_graph, &root);
            }
            self.proj.import_tree = Some(tree);
        }
        Task::none()
    }

    pub(crate) fn build_project_calls(&mut self) -> Task<Message> {
        let stamp = self.stamp();
        // Remote project: the build reads every file, so it runs where the
        // files live. The client contributes the one input the server can't
        // derive — the resolved import scope — as project-relative paths.
        if !self.local_project_state() {
            let Some(project) = &self.proj.project else {
                return Task::none();
            };
            let root = project.root.clone();
            let imports = self.proj.import_graph.clone();
            self.stamp_call_graph_build();
            let ai = self.ai_client();
            return Task::perform(
                async move {
                    // The scope walks the whole import graph: here, off the
                    // update loop, on the shared graph the window holds.
                    let scope_root = root.clone();
                    let scope = tokio::task::spawn_blocking(move || {
                        remote_scope(&imports.scope_map(), &scope_root)
                    })
                    .await
                    .map_err(|_| "collecting the import scope failed unexpectedly".to_string())?;
                    // Every failure is reported — the transport, a refusal,
                    // and a graph whose indices do not hold together — rather
                    // than drawn as a project with no calls.
                    match ai
                        .request(clew_protocol::Request::ProjectCalls { scope })
                        .await?
                    {
                        // The wire carries project-relative paths; rebuild
                        // this client's identities, validating as it goes.
                        clew_protocol::Event::ProjectCalls { graph, .. } => {
                            projectcalls::ProjectCallGraph::from_wire(graph, |rel| root.join(rel))
                        }
                        other => Err(format!(
                            "unexpected reply to ProjectCalls: {}",
                            crate::app::rpc::event_name(&other)
                        )),
                    }
                },
                move |graph| {
                    Message::Graph(GraphMsg::ProjectCallsBuilt {
                        stamp: stamp.clone(),
                        graph,
                    })
                },
            );
        }
        if self.proj.project.is_none() {
            return Task::none();
        }
        // What the build links, each taken whole as one shared handle
        // (`PerFile`, the import graph's `Arc`) — nothing here grows with the
        // project; the listing, the collecting and the scope walk all happen
        // in the task, off the update loop, which `present` runs this from
        // after every revision bump while a function's explanation is shown:
        //   * callable definitions, from the symbol index;
        //   * the call sites the symbol index read off its own parse of each
        //     file (`ProjectSession::calls_by_file`): the graph is linked from
        //     them, and reads and parses no file a second time;
        //   * the import scope — each file → the internal files it imports —
        //     so a called name resolves to the definition actually in scope.
        let symbols = self.proj.symbol_index_by_file.clone();
        let calls = self.proj.calls_by_file.clone();
        let imports = self.proj.import_graph.clone();
        self.stamp_call_graph_build();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let defs: Vec<projectcalls::Def> = symbols
                        .values()
                        .flat_map(|syms| syms.iter())
                        .map(|s| projectcalls::Def {
                            name: s.name.clone(),
                            kind: s.kind.clone(),
                            file: s.abs.clone(),
                            line: s.line,
                        })
                        .collect();
                    let sites: Vec<Arc<projectcalls::FileCalls>> =
                        calls.values().cloned().collect();
                    // The window writes both maps in place only while
                    // nothing else holds them (`PerFile`): their shares go
                    // before the linking, however long that runs.
                    drop((symbols, calls));
                    local_call_graph(defs, &sites, &imports.scope_map())
                })
                .await
                // A panicked build is a failure, not a project without calls.
                .map_err(|_| "building the call graph failed unexpectedly".to_string())
            },
            move |graph| {
                Message::Graph(GraphMsg::ProjectCallsBuilt {
                    stamp: stamp.clone(),
                    graph,
                })
            },
        )
    }

    /// Bring the project call graph up to date for something about to draw
    /// from it — the Calls map opening, the Explain panel's call-flow strip
    /// showing a function — cheap enough to call as the cursor moves between
    /// functions:
    ///   * while the LSP refine owns it, the files changed since it was
    ///     computed are refined in (`fold_refine_pending`): it is never
    ///     replaced by a name-based build, which would drop its exact edges;
    ///   * otherwise it is built in the background when no build landed yet
    ///     or it is stale — unless one is already in flight (single-flight).
    pub(crate) fn ensure_call_graph(&mut self) -> Task<Message> {
        self.release_stale_remote_refine();
        if self.refine_owns_call_graph() {
            return self.fold_refine_pending();
        }
        if self.proj.project_calls.building || self.call_graph_current() {
            return Task::none();
        }
        self.build_project_calls()
    }

    /// A build starts: stamp what it reads — the files (registry revision),
    /// the symbol index and the import scope — so the graph it lands can be
    /// told current or stale from them alone.
    fn stamp_call_graph_build(&mut self) {
        let calls = &mut self.proj.project_calls;
        calls.rev = self.proj.registry.revision();
        calls.index_rev = self.proj.symbol_index_rev;
        calls.scope_rev = self.proj.import_scope_rev;
        calls.building = true;
    }

    /// Whether the call graph on screen is the one the project calls for now:
    /// a build (or a refine) landed, and nothing it read — the files, the
    /// symbol index, the import scope — has moved since it started. What the
    /// build found does not count: an empty graph is current for a project
    /// without callable definitions.
    pub(crate) fn call_graph_current(&self) -> bool {
        let calls = &self.proj.project_calls;
        calls.built
            && calls.rev == self.proj.registry.revision()
            && calls.index_rev == self.proj.symbol_index_rev
            && calls.scope_rev == self.proj.import_scope_rev
    }

    /// Whether the LSP refine owns the call graph: a refined graph is in
    /// effect (`precise`), or a refine pass is running — the first one
    /// included. While it does, no name-based build replaces the graph (one
    /// would stop the pass and drop its exact edges); the refine keeps it
    /// current instead, from every file noted as changed meanwhile
    /// (`note_refine_change`, `fold_refine_pending`).
    pub(crate) fn refine_owns_call_graph(&self) -> bool {
        let calls = &self.proj.project_calls;
        calls.precise || calls.refine_progress.is_some()
    }

    /// Whether nothing the refine read — the files, the symbol index — has
    /// moved since its pass started (`ProjectCallsState::refine_read`).
    fn refine_current(&self) -> bool {
        self.proj.project_calls.refine_read
            == (self.proj.registry.revision(), self.proj.symbol_index_rev)
    }

    /// A REMOTE project has no incremental refine, so its refined graph (or
    /// the pass computing one) is kept only while nothing it read has moved:
    /// the first change hands the graph back to the name-based build, and
    /// says so. Its changes arrive as the host's publications, while the
    /// language server's copy of a document this window opened is resynced
    /// by a round trip of its own (`request_file_refresh`), ordered against
    /// none of them — re-querying the changed files could read positions
    /// from before the change, and edges wrong that way would stay wrong.
    /// The name-based build is exact about what it reads, and costs one
    /// request per settled change.
    fn release_stale_remote_refine(&mut self) {
        if !self.local_project_state() && self.refine_owns_call_graph() && !self.refine_current() {
            // The host's first snapshot of the index is no change to a file.
            let why = if self.proj.indexing {
                REFINED_BEFORE_INDEXED
            } else {
                "Files changed on the remote host"
            };
            self.drop_refinement(why, REFINE_AGAIN);
        }
    }

    /// Hand the call graph back to the name-based build: the refine cannot
    /// keep it current, for the reason `why` gives — said in the status line
    /// with `then`, what to do about it. Its pass is stopped and its edges
    /// dropped; the graph on screen stays until a build replaces it, but is
    /// no longer taken for one that landed (`built`), so the next use
    /// rebuilds it.
    pub(crate) fn drop_refinement(&mut self, why: &str, then: &str) {
        self.abort_refine();
        let calls = &mut self.proj.project_calls;
        calls.precise = false;
        calls.refine_progress = None;
        calls.generation += 1;
        calls.precise_edges = projectcalls::SymEdges::default();
        calls.precise_pending.clear();
        calls.refine_changed.clear();
        calls.refine_gens.clear();
        calls.refine_wait = None;
        calls.refine_retry.clear();
        calls.unrefined.clear();
        calls.unrefined_out.clear();
        calls.built = false;
        self.status = format!("{why}: the call graph is name-based again — {then}");
    }

    /// Note that `path` changed — edited, created or deleted — for the refine
    /// that owns the call graph to fold in (`fold_refine_pending`): whether
    /// or not anything draws from the graph right now, and during its first
    /// pass too (a file deleted while that pass ran used to land as a node).
    /// Only a file of a language the refinement covers is noted: no other
    /// holds any of its functions, and a JSON or TOML edit started a pass
    /// that queried nothing.
    pub(crate) fn note_refine_change(&mut self, path: &Path) {
        let langs = &self.proj.project_calls.refined_langs;
        let covered = highlight::detect(path).is_some_and(|lang| langs.contains(lang));
        if covered && self.refine_owns_call_graph() {
            self.proj
                .project_calls
                .precise_pending
                .insert(path.to_path_buf());
        }
    }

    /// Refine the files changed since the refined graph was computed
    /// (`precise_pending`) into it: an incremental pass over just their
    /// functions. One pass at a time: while one runs they wait, and it folds
    /// them in when it lands.
    ///
    /// Each is re-queried through its language's server (`refine_server`):
    ///   * ready — the pass takes it;
    ///   * starting — restarted, or replacing one that died — its files wait
    ///     for it, the status says so, and the rest are refined now: they are
    ///     folded in once it is ready (`settle_refine_wait`), or the
    ///     refinement is handed back if it has not started within
    ///     [`REFINE_WAIT_LIMIT`] (`on_refine_wait_over`). Handed back at once,
    ///     a restart cost a full pass to get the refinement back;
    ///   * down — failed, gone, or dead while still marked ready — the
    ///     refinement cannot follow the change, and it is handed back: the
    ///     name-based graph is rebuilt while in use, the status says which
    ///     server it waited on, and "Refine with LSP" is offered again.
    ///     Waiting instead would show the file's old edges as exact for as
    ///     long as that server stays down, which nothing bounds.
    fn fold_refine_pending(&mut self) -> Task<Message> {
        let calls = &mut self.proj.project_calls;
        if calls.refine_progress.is_some() {
            return Task::none();
        }
        if calls.precise_pending.is_empty() {
            // Nothing held, nothing to wait for.
            calls.refine_wait = None;
            return Task::none();
        }
        // The files of functions a pass failed in a way that may pass ride
        // along with a change, and are asked again now: in a pass of their
        // own they were asked again in a loop, of a server that may well
        // answer the same.
        let retry = std::mem::take(&mut calls.refine_retry);
        calls.precise_pending.extend(retry);
        let mut starting: BTreeSet<&'static str> = BTreeSet::new();
        let mut down: BTreeSet<&'static str> = BTreeSet::new();
        for lang in self.held_languages() {
            match self.refine_server(lang) {
                RefineServer::Ready => {}
                RefineServer::Starting => {
                    starting.insert(lang);
                }
                RefineServer::Down => {
                    down.insert(lang);
                }
            }
        }
        if let Some(lang) = down.first() {
            self.drop_refinement(
                &format!("No {lang} server is ready to refine the files that changed"),
                "refine it with LSP once one is",
            );
            return self.refresh_call_graph_in_use();
        }
        let pending = std::mem::take(&mut self.proj.project_calls.precise_pending);
        let (held, changed): (HashSet<PathBuf>, HashSet<PathBuf>) = pending
            .into_iter()
            .partition(|p| highlight::detect(p).is_some_and(|lang| starting.contains(lang)));
        self.proj.project_calls.precise_pending = held;
        let wait = self.wait_for_refine_servers(&starting);
        if changed.is_empty() {
            return wait;
        }
        let clients = self.call_hierarchy_clients();
        Task::batch([wait, self.refine_incremental(clients, changed)])
    }

    /// What the refine can do with `lang`'s server now (`fold_refine_pending`).
    /// One marked ready the instant it has started — while rust-analyzer,
    /// tsserver or gopls still loads the project — is waited for until it
    /// says it has loaded it. Read from the server as it is now, not from
    /// the snapshot the last update took (`sync_lsp_snapshots`): a refine
    /// landing, or a wait's bound running out, is handled before this update
    /// takes its own, and a server that began loading since — failing a
    /// query for it — was asked again at once.
    fn refine_server(&self, lang: &str) -> RefineServer {
        match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) if c.alive() && c.call_hierarchy => {
                if c.loading() {
                    RefineServer::Starting
                } else {
                    RefineServer::Ready
                }
            }
            Some(LspSlot::Starting) => RefineServer::Starting,
            _ => RefineServer::Down,
        }
    }

    /// Whether `lang`'s server is ready and still loading the project.
    fn lsp_slot_loading(&self, lang: &str) -> bool {
        match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.loading(),
            _ => false,
        }
    }

    /// How many reports of its work `lang`'s ready server has made so far
    /// (`Snapshot::reports`), if one is ready — as it is now, like its
    /// loading ([`App::refine_server`]).
    fn lsp_reports(&self, lang: &str) -> Option<u64> {
        match self.proj.link.lsp.get(lang) {
            Some(LspSlot::Ready(client)) => client.snapshot().ok().map(|snapshot| snapshot.reports),
            _ => None,
        }
    }

    /// The languages of the files the refinement holds (`precise_pending`),
    /// each once: what their servers can do is read per language, not per
    /// file.
    fn held_languages(&self) -> BTreeSet<&'static str> {
        self.proj
            .project_calls
            .precise_pending
            .iter()
            .filter_map(|p| highlight::detect(p))
            .collect()
    }

    /// The languages whose ready call-hierarchy server is still loading the
    /// project, sorted.
    fn loading_refine_servers(&self) -> BTreeSet<String> {
        self.proj
            .link
            .lsp
            .iter()
            .filter_map(|(lang, slot)| match slot {
                LspSlot::Ready(c) if c.call_hierarchy && c.alive() && c.loading() => {
                    Some(lang.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// A new wait for the servers of `langs` — numbered, with how many
    /// reports each has made so far — and the timer bounding it
    /// ([`REFINE_WAIT_LIMIT`]). `since` is when the wait it renews was first
    /// armed; a new one is armed now.
    fn arm_refine_wait<'a>(
        &mut self,
        langs: impl IntoIterator<Item = &'a str>,
        since: Option<std::time::Instant>,
    ) -> (crate::app::state::RefineWait, Task<Message>) {
        let heard = langs
            .into_iter()
            .filter_map(|lang| Some((lang.to_string(), self.lsp_reports(lang)?)))
            .collect();
        let calls = &mut self.proj.project_calls;
        calls.refine_waits += 1;
        let id = calls.refine_waits;
        let stamp = self.stamp();
        // The timer is made where the task runs: made here, it would belong
        // to whatever runtime this thread is in, if any.
        let timer = Task::perform(
            async { tokio::time::sleep(REFINE_WAIT_LIMIT).await },
            move |()| {
                Message::Graph(GraphMsg::RefineWaitOver {
                    stamp: stamp.clone(),
                    wait: id,
                })
            },
        );
        let wait = crate::app::state::RefineWait {
            id,
            since: since.unwrap_or_else(std::time::Instant::now),
            heard,
        };
        (wait, timer)
    }

    /// Hold the changed files of the `starting` servers' languages for them
    /// (`fold_refine_pending`): the wait is armed once — said in the status,
    /// and bounded ([`REFINE_WAIT_LIMIT`], renewed while a server loading the
    /// project reports how it gets on) — and disarmed when none is left.
    fn wait_for_refine_servers(&mut self, starting: &BTreeSet<&str>) -> Task<Message> {
        let calls = &mut self.proj.project_calls;
        if starting.is_empty() {
            calls.refine_wait = None;
            return Task::none();
        }
        if calls.refine_wait.is_some() {
            return Task::none();
        }
        let (wait, timer) = self.arm_refine_wait(starting.iter().copied(), None);
        self.proj.project_calls.refine_wait = Some(wait);
        if let Some(what) = self.refine_wait_note() {
            self.status = format!("Waiting {what}");
        }
        timer
    }

    /// Whether the refine waits on for `lang`'s server as a bound on `wait`
    /// runs out with it still starting or loading the project: `Ok` while
    /// it loads and has reported how it gets on since the bound was set, up
    /// to [`REFINE_LOAD_LIMIT`] — building a workspace takes longer than
    /// starting a server; else `Err` with what it did not do. A server that
    /// is starting, or has gone silent, is not coming soon.
    fn refine_wait_verdict(
        &self,
        wait: &crate::app::state::RefineWait,
        lang: &str,
    ) -> Result<(), String> {
        if !self.lsp_slot_loading(lang) {
            return Err(format!(
                "did not start within {}",
                in_words(REFINE_WAIT_LIMIT)
            ));
        }
        if wait.since.elapsed() >= REFINE_LOAD_LIMIT {
            return Err(format!(
                "did not load the project within {}",
                in_words(REFINE_LOAD_LIMIT)
            ));
        }
        match (wait.heard.get(lang), self.lsp_reports(lang)) {
            (Some(heard), Some(now)) if now <= *heard => Err(format!(
                "reported nothing for {} while loading the project",
                in_words(REFINE_WAIT_LIMIT)
            )),
            _ => Ok(()),
        }
    }

    /// The bound on wait `wait` ran out. Of the servers the refinement's
    /// held files wait for, one still starting did not start in time, and
    /// one loading the project that went silent — or took past
    /// [`REFINE_LOAD_LIMIT`] — is not coming soon either: the refinement is
    /// handed back. One that loads and keeps saying so is waited on; the
    /// files of one that is ready are refined in (or, down after all, hand
    /// the refinement back too). It was handed back after the first bound
    /// however the loading went, and rust-analyzer took longer than that to
    /// load this repository.
    fn on_refine_wait_over(&mut self, wait: u64) -> Task<Message> {
        let calls = &mut self.proj.project_calls;
        if calls
            .refine_full_wait
            .as_ref()
            .is_some_and(|w| w.id == wait)
        {
            return self.on_full_refine_wait_over();
        }
        let Some(held) = calls.refine_wait.take_if(|w| w.id == wait) else {
            return Task::none();
        };
        let late: BTreeSet<&'static str> = self
            .held_languages()
            .into_iter()
            .filter(|lang| matches!(self.refine_server(lang), RefineServer::Starting))
            .collect();
        if late.is_empty() {
            return self.fold_refine_pending();
        }
        let gave_up = late.iter().find_map(|lang| {
            let why = self.refine_wait_verdict(&held, lang).err()?;
            Some((*lang, why))
        });
        if let Some((lang, why)) = gave_up {
            self.drop_refinement(
                &format!("The {lang} server {why} to refine the files that changed"),
                "refine it with LSP once it has",
            );
            return self.refresh_call_graph_in_use();
        }
        let (renewed, timer) = self.arm_refine_wait(late.iter().copied(), Some(held.since));
        self.proj.project_calls.refine_wait = Some(renewed);
        timer
    }

    /// The bound on the wait of a full pass held for its servers to load the
    /// project ran out (`refine_project_calls`): one that loads and keeps
    /// saying so is waited on; otherwise the pass starts, without the
    /// servers still loading — said to be left out — rather than not at all.
    fn on_full_refine_wait_over(&mut self) -> Task<Message> {
        let Some(held) = self.proj.project_calls.refine_full_wait.take() else {
            return Task::none();
        };
        let loading = self.loading_refine_servers();
        let gave_up = loading.iter().find_map(|lang| {
            let why = self.refine_wait_verdict(&held, lang).err()?;
            Some((lang.clone(), why))
        });
        match gave_up {
            None if loading.is_empty() => self.start_full_refine(&loading),
            None => {
                let (renewed, timer) =
                    self.arm_refine_wait(loading.iter().map(String::as_str), Some(held.since));
                self.proj.project_calls.refine_full_wait = Some(renewed);
                timer
            }
            Some((lang, why)) => {
                let pass = self.start_full_refine(&loading);
                self.status = format!("The {lang} server {why} — {}", self.status);
                pass
            }
        }
    }

    /// A refinement holding files for a starting server
    /// (`fold_refine_pending`) folds them in as soon as none of their servers
    /// is starting any more — ready, or down after all — whatever message
    /// made it so: a server's start lands as an LSP message, which the refine
    /// does not see. So does a full pass held for its servers to load the
    /// project (`refine_project_calls`). Checked after every message
    /// (`App::update`), at the cost of a look at each held file's server
    /// while a wait is on, and of nothing otherwise.
    pub(crate) fn settle_refine_wait(&mut self) -> Option<Task<Message>> {
        if self.proj.project_calls.refine_full_wait.is_some()
            && self.loading_refine_servers().is_empty()
        {
            return Some(self.refine_project_calls());
        }
        let calls = &self.proj.project_calls;
        if calls.refine_wait.is_none() || calls.refine_progress.is_some() {
            return None;
        }
        let starting = self
            .held_languages()
            .into_iter()
            .any(|lang| matches!(self.refine_server(lang), RefineServer::Starting));
        (!starting).then(|| self.fold_refine_pending())
    }

    /// What an input of the call graph moving does to it — the symbol index
    /// arriving or changing, the file set, the import scope: it is brought up
    /// to date at once while something draws from it (`ensure_call_graph`; a
    /// build in flight catches the change itself when it lands), else the
    /// next time it is needed. A remote refinement that fell behind is
    /// dropped either way, stopping its pass.
    pub(crate) fn refresh_call_graph_in_use(&mut self) -> Task<Message> {
        if self.call_graph_in_use() {
            return self.ensure_call_graph();
        }
        self.release_stale_remote_refine();
        Task::none()
    }

    /// `refresh_call_graph_in_use` for a change whose imports were just
    /// queued — a parse landing, a deletion, a remote host's publication: the
    /// name-based graph links through the import scope they can move, so
    /// while a job is running it is refreshed once the job carrying them has
    /// landed (`ImportWork::refresh_calls`), not built now to be stale when
    /// that job lands — built twice, and on a remote project asked of the
    /// host twice. The LSP refine reads no import scope, and folds the change
    /// in at once; a remote refinement the change left behind is handed back
    /// first (`release_stale_remote_refine`), and the build taking over waits
    /// too.
    pub(crate) fn refresh_call_graph_after_imports(&mut self) -> Task<Message> {
        self.release_stale_remote_refine();
        if !self.proj.import_work.running || self.refine_owns_call_graph() {
            return self.refresh_call_graph_in_use();
        }
        let work = &mut self.proj.import_work;
        // The job carrying the change: the one running when it took the
        // queued batch — it was started for it — else the next to start.
        let carrier = work.started + u64::from(!work.pending.is_empty());
        // A refresh already due comes first: the build it starts reads the
        // index as it is by then, this change included, and the change's
        // scope, if it moves, rebuilds the graph when its job lands.
        work.refresh_calls = Some(work.refresh_calls.map_or(carrier, |due| due.min(carrier)));
        Task::none()
    }

    /// The refresh deferred to the import graph catching up
    /// (`ImportWork::refresh_calls`), now that job `landed` has: made when
    /// that is the job it waited on, or a later one.
    fn refresh_deferred_calls(&mut self, landed: u64) -> Task<Message> {
        let work = &mut self.proj.import_work;
        if work.refresh_calls.is_none_or(|due| due > landed) {
            return Task::none();
        }
        work.refresh_calls = None;
        self.refresh_call_graph_in_use()
    }

    /// Whether something on screen draws from the call graph: its overlay, or
    /// the Explain panel's call-flow strip for a function.
    pub(crate) fn call_graph_in_use(&self) -> bool {
        self.proj.overlay == Some(Overlay::ProjectCalls)
            || (self.show_right_panel
                && matches!(self.proj.explain.view, Some(explain::Node::Function { .. })))
    }

    /// Install a new project call graph — the one assignment site, so the
    /// overlay's memoized rankings (keyed by `graph_rev`) see every change —
    /// and hand back the one it replaces, for the caller to free off the UI
    /// thread (`free_off_thread`): a node per function of the project,
    /// however little changed.
    #[must_use = "the graph it replaces is freed off the UI thread (`free_off_thread`)"]
    pub(crate) fn set_project_calls_graph(
        &mut self,
        graph: projectcalls::ProjectCallGraph,
    ) -> Arc<projectcalls::ProjectCallGraph> {
        self.proj.project_calls.graph_rev += 1;
        std::mem::replace(&mut self.proj.project_calls.graph, Arc::new(graph))
    }

    /// (Project ownership is checked in `dispatch`, from the stamp.)
    pub(crate) fn on_project_calls_built(
        &mut self,
        graph: Result<projectcalls::ProjectCallGraph, String>,
    ) -> Task<Message> {
        self.proj.project_calls.building = false;
        // Started before the LSP refine took the graph over — nothing starts
        // a build while it owns it, but "Refine with LSP" is offered while
        // one runs. The refine is the newer, and the pass runs on to its own
        // result: this one never passes for the graph it is current for (not
        // `built`), and leaves the refine's state alone. Until the first pass
        // lands, though, the name-based graph is all there is to show, and it
        // is shown — dropped, the map stood empty ("No functions found"), or
        // on another overlay's layout, for as long as that pass ran.
        if self.refine_owns_call_graph() {
            self.proj.project_calls.built = false;
            return match graph {
                Ok(graph) if !self.proj.project_calls.precise => {
                    let retired = free_off_thread(self.set_project_calls_graph(graph));
                    Task::batch([retired, self.relayout_calls_map()])
                }
                // Not shown: a node per function of the project, freed off
                // the UI thread like any graph a landing replaces.
                unused => free_off_thread(unused),
            };
        }
        let graph = match graph {
            Ok(graph) => graph,
            // Reported, never drawn as a project without calls. The graph on
            // screen (if any) stays but no longer reads as current, so the
            // next `ensure_call_graph` retries rather than trusting it.
            Err(e) => {
                self.proj.project_calls.rev = crate::app::graph::CALLS_REV_STALE;
                self.status = format!("Couldn't build the call graph: {e}");
                return Task::none();
            }
        };
        let retired = free_off_thread(self.set_project_calls_graph(graph));
        self.proj.project_calls.built = true;
        // This is the name-based approximation; a superseding refine is
        // no longer valid, and no precise result is in effect. A refine still
        // running is STOPPED, not merely outdated by the generation below: it
        // would go on querying the language servers for a graph nothing will
        // accept, while the "Refine with LSP" button offers a second pass.
        self.abort_refine();
        self.proj.project_calls.precise = false;
        self.proj.project_calls.refine_progress = None;
        self.proj.project_calls.generation += 1;
        self.proj.project_calls.precise_edges = projectcalls::SymEdges::default();
        self.proj.project_calls.precise_pending.clear();
        self.proj.project_calls.refine_wait = None;
        self.proj.project_calls.refine_retry.clear();
        self.proj.project_calls.unrefined.clear();
        self.proj.project_calls.unrefined_out.clear();
        // The map depends on the freshly built graph.
        let map = self.relayout_calls_map();
        // If what the build read — the files, the symbol index, the import
        // scope, all stamped when it started — moved while it ran, its result
        // is already stale: it is built once more while something draws from
        // it. Decided from those stamps alone, never from what the build
        // found: taking an empty graph for "not built yet" rebuilt it forever.
        if self.call_graph_in_use() && !self.call_graph_current() {
            return Task::batch([retired, map, self.build_project_calls()]);
        }
        Task::batch([retired, map])
    }

    /// The Calls map laid out again for the call graph just installed, when
    /// it is the overlay open (`refresh_graph_layout`).
    fn relayout_calls_map(&mut self) -> Task<Message> {
        if self.proj.overlay == Some(Overlay::ProjectCalls) {
            self.refresh_graph_layout()
        } else {
            Task::none()
        }
    }

    /// Ready, call-hierarchy-capable servers keyed by language — alive: a
    /// server that died after startup stays `Ready` until something restarts
    /// it, and every query to it fails.
    pub(crate) fn call_hierarchy_clients(&self) -> HashMap<String, lsp::client::LspClient> {
        let mut clients = HashMap::new();
        for (lang, slot) in self.proj.link.lsp.iter() {
            if let LspSlot::Ready(c) = slot
                && c.call_hierarchy
                && c.alive()
            {
                clients.insert(lang.clone(), c.clone());
            }
        }
        clients
    }

    /// What the Calls map's header says of the refine pass running: how far
    /// it has got — or, while an incremental one has yet to list what it
    /// queries, what it refines. It read "Refining 0/0…" until then. A full
    /// pass held for its servers to load the project says what it waits
    /// for, where the button offered it again.
    pub(crate) fn refine_progress_label(&self) -> Option<String> {
        let calls = &self.proj.project_calls;
        let Some((done, total)) = calls.refine_progress else {
            let held = calls.refine_full_wait.as_ref()?;
            let mut waiting: Vec<&str> = held.heard.keys().map(String::as_str).collect();
            waiting.sort_unstable();
            return Some(match waiting.as_slice() {
                [] => "Waiting for its servers to load the project…".to_string(),
                langs => format!("Waiting for {} to load the project…", langs.join(", ")),
            });
        };
        Some(match total {
            0 => "Refining the files that changed…".to_string(),
            _ => format!("Refining {done}/{total}…"),
        })
    }

    /// What the Calls map's header says of a refined graph: which languages
    /// its exact edges are for — it holds their functions, and only theirs —
    /// which of the project's it leaves out, and which server it waits for.
    /// A pass started while a server was down left that language out, and
    /// the map said "● LSP-precise" all the same.
    pub(crate) fn precise_label(&self) -> Option<String> {
        let calls = &self.proj.project_calls;
        if !calls.precise {
            return None;
        }
        let mut label = match self.refined_languages().as_slice() {
            [] => "● LSP-precise".to_string(),
            covered => format!("● LSP-precise: {}", covered.join(", ")),
        };
        let left = self.languages_left_out();
        if !left.is_empty() {
            label.push_str(&format!(" · {} left out", left.join(", ")));
        }
        match calls.unrefined.len() {
            0 => {}
            1 => label.push_str(" · 1 function unrefined"),
            n => label.push_str(&format!(" · {n} functions unrefined")),
        }
        let waiting = self.refine_waiting_for();
        if !waiting.is_empty() {
            let names = waiting.into_iter().collect::<Vec<_>>().join(", ");
            label.push_str(&format!(" · waiting for {names}"));
        }
        Some(label)
    }

    /// What the Calls list says of a refined graph: the languages its exact
    /// edges are for, and those it leaves out.
    pub(crate) fn precise_summary(&self) -> Option<String> {
        if !self.proj.project_calls.precise {
            return None;
        }
        let mut summary = match self.refined_languages().as_slice() {
            [] => "LSP-precise — exact caller/callee edges.".to_string(),
            covered => format!(
                "LSP-precise for {} — exact caller/callee edges.",
                covered.join(", ")
            ),
        };
        let left = self.languages_left_out();
        if !left.is_empty() {
            summary.push_str(&format!(
                " Functions in {} are left out: no server was ready to refine them.",
                left.join(", ")
            ));
        }
        // After a pass over changed files, the calls out of a function it
        // failed are missing too: it dropped every edge of the file.
        let calls = &self.proj.project_calls;
        let missing = match calls.unrefined_out.is_empty() {
            true => "calls into",
            false => "calls into and out of",
        };
        match calls.unrefined.len() {
            0 => {}
            1 => summary.push_str(&format!(
                " 1 function could not be refined: {missing} it may be missing."
            )),
            n => summary.push_str(&format!(
                " {n} functions could not be refined: {missing} them may be missing."
            )),
        }
        Some(summary)
    }

    /// The languages the refinement covers (`refined_langs`), sorted.
    fn refined_languages(&self) -> Vec<&str> {
        let mut langs: Vec<&str> = self
            .proj
            .project_calls
            .refined_langs
            .iter()
            .map(String::as_str)
            .collect();
        langs.sort_unstable();
        langs
    }

    /// The project's call-graph languages the refinement does not cover:
    /// their functions are not in its graph. Those of the index as it is —
    /// a language whose first function came after the refine began is left
    /// out too — and only those with a function: a language of the project
    /// with none, a config file or a script, loses nothing.
    fn languages_left_out(&self) -> Vec<&'static str> {
        let covered = &self.proj.project_calls.refined_langs;
        let languages = self
            .proj
            .view_memo
            .function_languages
            .get_or(self.proj.symbol_index_rev, || {
                function_languages(&self.proj.symbol_index_by_file)
            });
        languages
            .iter()
            .copied()
            .filter(|lang| !covered.contains(*lang))
            .collect()
    }

    /// Every callable function whose language has a ready call-hierarchy
    /// server: a walk of the whole index, for the "Refine with LSP" button
    /// only — an incremental pass lists its own, off the UI thread.
    pub(crate) fn refinable_defs(
        &self,
        clients: &HashMap<String, lsp::client::LspClient>,
    ) -> Vec<projectcalls::Def> {
        refine_defs(
            self.proj
                .symbol_index_by_file
                .values()
                .map(|syms| syms.as_slice()),
            |lang| clients.contains_key(lang),
        )
    }

    /// Full LSP refine (the "Refine with LSP" button): query every project
    /// function and rebuild the precise graph from scratch.
    ///
    /// A server still loading the project answers from what it has read so
    /// far — callers missing, an error for a file it has not read — which
    /// the pass took for exact: while one of its servers loads, the pass is
    /// held (`refine_full_wait`, said in the status and on the map), and
    /// starts once none does (`settle_refine_wait`) — or, one gone silent or
    /// past [`REFINE_LOAD_LIMIT`], without it (`on_full_refine_wait_over`).
    pub(crate) fn refine_project_calls(&mut self) -> Task<Message> {
        let loading = self.loading_refine_servers();
        if !loading.is_empty() {
            return self.hold_full_refine(&loading);
        }
        self.start_full_refine(&loading)
    }

    /// Hold the full pass for the servers of `loading` to load the project
    /// ([`App::refine_project_calls`]), under one wait.
    fn hold_full_refine(&mut self, loading: &BTreeSet<String>) -> Task<Message> {
        let names = loading.iter().cloned().collect::<Vec<_>>().join(", ");
        let servers = match loading.len() {
            1 => "server",
            _ => "servers",
        };
        self.status = format!(
            "Waiting for the {names} {servers} to load the project before refining the call graph"
        );
        if self.proj.project_calls.refine_full_wait.is_some() {
            return Task::none();
        }
        let (wait, timer) = self.arm_refine_wait(loading.iter().map(String::as_str), None);
        self.proj.project_calls.refine_full_wait = Some(wait);
        timer
    }

    /// Start the full pass through every ready call-hierarchy server but
    /// those of `leave_out`.
    fn start_full_refine(&mut self, leave_out: &BTreeSet<String>) -> Task<Message> {
        self.proj.project_calls.refine_full_wait = None;
        let mut clients = self.call_hierarchy_clients();
        clients.retain(|lang, _| !leave_out.contains(lang));
        if clients.is_empty() {
            self.status = "No language server ready — open a file to start one, then retry".into();
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        if all.is_empty() {
            self.status = "No functions to refine for the ready server(s)".into();
            return Task::none();
        }
        // A full pass reads every function as it stands now: nothing noted
        // as changed before it is left to fold in, nor held for a server,
        // nor asked again. What it covers is the languages whose servers
        // answer it.
        let calls = &mut self.proj.project_calls;
        calls.precise_pending.clear();
        calls.refine_wait = None;
        calls.refine_retry.clear();
        calls.refine_changed.clear();
        calls.refined_langs = clients.keys().cloned().collect();
        self.spawn_refine(clients, RefinePass::Full(all))
    }

    /// Incrementally refresh the precise graph after files changed: re-query
    /// only the changed files' functions, through `clients`, and patch the
    /// edge set. This runs once per watcher batch while the refine owns the
    /// graph, so it does no work that grows with the project: the pass takes
    /// the symbol index whole, as one share of the window's map (`PerFile`),
    /// and borrows the edge set — handed back patched when it lands — and
    /// lists the functions and patches the edges itself, on the blocking pool
    /// (`RefinePass::plan`).
    fn refine_incremental(
        &mut self,
        clients: HashMap<String, lsp::client::LspClient>,
        changed: HashSet<PathBuf>,
    ) -> Task<Message> {
        let index = self.proj.symbol_index_by_file.clone();
        let calls = &mut self.proj.project_calls;
        let langs = calls.refined_langs.clone();
        let edges = std::mem::take(&mut calls.precise_edges);
        // Kept to refine again those whose server restarts meanwhile
        // (`on_project_calls_refined`): a copy of the batch, not of the
        // project.
        calls.refine_changed = changed.clone();
        // Even with nothing to re-query (e.g. all changed functions removed),
        // the pass still runs, so deleted files' edges drop out.
        self.spawn_refine(
            clients,
            RefinePass::Incremental {
                index,
                langs,
                edges,
                changed,
            },
        )
    }

    /// Shared refine launcher: `pass` is what is queried and what the answers
    /// patch (see `RefinePass`).
    pub(crate) fn spawn_refine(
        &mut self,
        clients: HashMap<String, lsp::client::LspClient>,
        pass: RefinePass,
    ) -> Task<Message> {
        let Some(project) = &self.proj.project else {
            return Task::none();
        };
        let root = project.root.clone();
        self.proj.project_calls.generation += 1;
        let generation = self.proj.project_calls.generation;
        // A full pass was listed here, so it says at once how many functions
        // it refines; an incremental one lists its own, and reports how many
        // when it has (`refine_stream`).
        let queried = match &pass {
            RefinePass::Full(defs) => {
                // What it covers is said with it: the languages without a
                // ready server are left out of its graph altogether.
                let left = self.languages_left_out();
                let note = match left.is_empty() {
                    true => String::new(),
                    false => format!(" ({} left out: no server ready)", left.join(", ")),
                };
                self.status = format!("Refining {} functions with LSP…{note}", defs.len());
                defs.len()
            }
            RefinePass::Incremental { .. } => 0,
        };
        self.proj.project_calls.refine_progress = Some((0, queried));
        // The servers it queries, by their spawn generation: one that
        // restarts while it runs answers it through the client it replaced.
        self.proj.project_calls.refine_gens = clients
            .keys()
            .map(|lang| (lang.clone(), self.lsp_gen.get(lang).copied()))
            .collect();
        // What the pass reads — the files as the registry has them, the
        // definitions of the symbol index: what its graph is current for.
        self.proj.project_calls.refine_read =
            (self.proj.registry.revision(), self.proj.symbol_index_rev);
        // A remote project's files live on the other host; the pass fetches
        // their text over the protocol rather than reading this machine's disk.
        let remote = (!self.local_project_state()).then(|| self.ai_client());
        let stamp = self.stamp();
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(output, pass, clients, root, stamp, generation, remote)
        });
        // Abortable so leaving the project actually stops the pass: it holds
        // clones of this project's language-server clients, so dropping
        // `ProjectLink::lsp` would not.
        let (task, handle) = Task::run(stream, |m| m).abortable();
        // One pass at a time: the one this replaces is stopped (an iced
        // handle does not abort on drop), or it keeps querying the servers.
        self.abort_refine();
        self.proj.project_calls.refine_abort = Some(handle);
        task
    }

    /// Stop the LSP refine pass in flight, if any.
    pub(crate) fn abort_refine(&mut self) {
        if let Some(handle) = self.proj.project_calls.refine_abort.take() {
            handle.abort();
        }
    }

    pub(crate) fn on_project_calls_refined(
        &mut self,
        generation: u64,
        result: Result<RefineDone, String>,
    ) -> Task<Message> {
        // Accept only the latest refine (the project was checked in
        // `dispatch`) — its failure included: a superseded pass that failed
        // says nothing about the one that replaced it. What it landed with —
        // a graph and an edge set, each the size of the project — is freed
        // off the UI thread.
        if generation != self.proj.project_calls.generation {
            return free_off_thread(result);
        }
        let done = match result {
            Ok(done) => done,
            // A step the pass runs on the blocking pool panicked, or a remote
            // pass could not read a file it queries: it ended without its
            // graph (an incremental one, with the edge set it was lent).
            // Ignored, it would read as in flight for good — the refine
            // owning the graph, with nothing left to keep it current. The
            // graph is handed back to the name-based build instead, and the
            // status says why.
            Err(e) => {
                self.drop_refinement(&format!("The LSP refine failed ({e})"), REFINE_AGAIN);
                return self.refresh_call_graph_in_use();
            }
        };
        let calls = &mut self.proj.project_calls;
        let changed = std::mem::take(&mut calls.refine_changed);
        // The servers restarted while it ran: what they answered, they
        // answered through the clients they replaced.
        let restarted: HashSet<String> = std::mem::take(&mut calls.refine_gens)
            .into_iter()
            .filter(|(lang, generation)| self.lsp_gen.get(lang).copied() != *generation)
            .map(|(lang, _)| lang)
            .collect();
        let restarted_lang =
            |file: &Path| highlight::detect(file).is_some_and(|lang| restarted.contains(lang));
        // A server that stopped while it ran — its session over, and not
        // restarted since — answers nothing more: every edge of its language
        // the pass did not get is unknown, and the refinement is handed back
        // rather than kept with them missing, as an empty answer used to
        // keep it.
        if let Some((file, why)) = done
            .stopped
            .iter()
            .filter(|(file, _)| !restarted_lang(file))
            .min_by(|a, b| a.0.cmp(b.0))
        {
            let lang = highlight::detect(file).unwrap_or("language");
            let why = format!(
                "The {lang} server stopped while refining {} ({why})",
                self.rel_of(file)
            );
            let retired = free_off_thread(done);
            self.drop_refinement(&why, REFINE_AGAIN);
            return Task::batch([retired, self.refresh_call_graph_in_use()]);
        }
        // The files of a restarted server's language the pass refined, and
        // those it could not, are refined again — through the server that
        // replaced it, once it has started (`fold_refine_pending`).
        let unrefined_files = done.unrefined.keys().map(|(file, _, _)| file.clone());
        let again: Vec<PathBuf> = changed
            .iter()
            .cloned()
            .chain(done.stopped.into_keys())
            .chain(unrefined_files)
            .filter(|file| restarted_lang(file))
            .collect();
        // Those of functions a server failed while it was loading the
        // project — asked about what it had not read yet — are refined again
        // once it has: held for it (`fold_refine_pending`). Taken for
        // failures of their own, they stayed unrefined until they changed.
        let reloading = done.loading;
        // A function any server still serving failed, even asked again, is
        // noted, and the rest of the graph kept: one symbol a server never
        // answers for handed the whole pass back, on every try. Calls into
        // it are not made up from the name-based graph — guesses among exact
        // edges, which later passes patch as exact — but said to be missing.
        // One whose failure may pass is asked again with the next pass that
        // runs (`refine_retry`).
        let retry = done.retry.into_iter().filter(|file| !restarted_lang(file));
        let unanswering = done.unanswering;
        let calls = &mut self.proj.project_calls;
        if done.full {
            calls.unrefined = done.unrefined;
            calls.unrefined_out.clear();
            calls.refine_retry = retry.collect();
        } else {
            let unchanged = |file: &Path| !changed.contains(file);
            calls.unrefined.retain(|(file, _, _), _| unchanged(file));
            calls.unrefined_out.retain(|(file, _, _)| unchanged(file));
            calls.refine_retry.retain(|file| unchanged(file));
            // A pass over changed files dropped every edge of theirs: the
            // calls out of one it failed are missing too.
            calls.unrefined_out.extend(done.unrefined.keys().cloned());
            calls.unrefined.extend(done.unrefined);
            calls.refine_retry.extend(retry);
        }
        let old_graph = self.set_project_calls_graph(done.graph);
        let calls = &mut self.proj.project_calls;
        calls.precise_pending.extend(again);
        calls.precise_pending.extend(reloading);
        // An incremental pass hands back the edge set it was lent, patched
        // (the window's is empty meanwhile); a full one replaces the set.
        let old_edges = std::mem::replace(&mut calls.precise_edges, done.edges);
        // What it replaces is freed on the blocking pool, like everything
        // else that grows with the project: this lands once per watcher
        // batch while the refine owns the graph.
        let retired = free_off_thread((old_graph, old_edges));
        calls.precise = true;
        calls.refine_progress = None;
        calls.refine_abort = None;
        // Current for what the pass read when it started — and for any
        // import scope, which the language servers do not resolve calls
        // through — so nothing takes it for stale until one of those moves.
        (calls.rev, calls.index_rev) = calls.refine_read;
        calls.scope_rev = self.proj.import_scope_rev;
        calls.built = true;
        let mut refined = String::from("Call graph refined with LSP");
        if !unanswering.is_empty() {
            let (servers, were) = match unanswering.len() {
                1 => ("server", "was"),
                _ => ("servers", "were"),
            };
            let names = unanswering.into_iter().collect::<Vec<_>>().join(", ");
            refined.push_str(&format!(
                " — the {names} {servers} left {REFINE_BREAKER_TIMEOUTS} queries in a row \
                 unanswered, and {were} asked no more"
            ));
        }
        if let Some(note) = self.unrefined_note() {
            refined.push_str(&format!(" — {note}"));
        }
        self.status.clone_from(&refined);
        let map = self.relayout_calls_map();
        // A remote pass that fell behind while it ran is not kept.
        self.release_stale_remote_refine();
        if !self.refine_owns_call_graph() {
            return Task::batch([retired, map, self.refresh_call_graph_in_use()]);
        }
        // The files changed while it ran are folded in now, whether or not
        // anything draws from the graph: its servers just answered, and a
        // file deleted meanwhile must not stay a node.
        let fold = self.fold_refine_pending();
        // Those held for a starting server are said to be, still.
        if let Some(what) = self.refine_wait_note() {
            self.status = format!("{refined} — waiting {what}");
        }
        Task::batch([retired, map, fold])
    }

    /// What the status says of the functions the refinement could not refine
    /// (`ProjectCallsState::unrefined`), if any: how many, and the first by
    /// file, name and place, with why — "1 function unrefined (c in tool.py:
    /// the server is busy)".
    fn unrefined_note(&self) -> Option<String> {
        let unrefined = &self.proj.project_calls.unrefined;
        let ((file, name, _), why) = unrefined.iter().min_by(|a, b| a.0.cmp(b.0))?;
        let (count, more) = match unrefined.len() {
            1 => ("1 function".to_string(), String::new()),
            n => (format!("{n} functions"), format!(", and {} more", n - 1)),
        };
        Some(format!(
            "{count} unrefined ({name} in {}: {why}{more})",
            self.rel_of(file)
        ))
    }

    /// The languages whose starting servers the refine holds changed files
    /// for (`fold_refine_pending`), while a wait is on.
    fn refine_waiting_for(&self) -> BTreeSet<&'static str> {
        if self.proj.project_calls.refine_wait.is_none() {
            return BTreeSet::new();
        }
        self.held_languages()
            .into_iter()
            .filter(|lang| matches!(self.refine_server(lang), RefineServer::Starting))
            .collect()
    }

    /// What the status says a wait for starting servers is for, if one is
    /// on: "for the python server to start to refine the files that
    /// changed", or for one that has started to load the project.
    fn refine_wait_note(&self) -> Option<String> {
        let (loading, starting): (Vec<&str>, Vec<&str>) = self
            .refine_waiting_for()
            .into_iter()
            .partition(|lang| self.lsp_slot_loading(lang));
        let wait = |langs: &[&str], what: &str| {
            let servers = if langs.len() == 1 {
                "server"
            } else {
                "servers"
            };
            (!langs.is_empty()).then(|| format!("the {} {servers} to {what}", langs.join(", ")))
        };
        let waits: Vec<String> = [wait(&starting, "start"), wait(&loading, "load the project")]
            .into_iter()
            .flatten()
            .collect();
        (!waits.is_empty()).then(|| {
            format!(
                "for {} to refine the files that changed",
                waits.join(" and ")
            )
        })
    }
}

impl App {
    /// Handle a [`GraphMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_graph(&mut self, message: GraphMsg) -> Task<Message> {
        match message {
            GraphMsg::ImportGraphUpdated {
                result,
                refresh_overview,
                ..
            } => self.on_import_graph_updated(result, refresh_overview),
            GraphMsg::ImportExpand { token, id } => self.on_import_expand(token, id),
            GraphMsg::ImportDirection => {
                self.import_dir = self.import_dir.toggled();
                // A direction flip resets the (now meaningless) expand-all state.
                if let Some(t) = &mut self.proj.import_tree {
                    t.full = false;
                }
                self.refresh_import_tree();
                Task::none()
            }
            GraphMsg::ImportExpandAll => {
                if let (Some(mut tree), Some(root)) = (
                    self.proj.import_tree.take(),
                    self.proj.project.as_ref().map(|p| p.root.clone()),
                ) {
                    tree.expand_all(&self.proj.import_graph, &root);
                    self.proj.import_tree = Some(tree);
                }
                Task::none()
            }
            GraphMsg::OpenOverlay(which) => self.on_open_overlay(which),
            GraphMsg::OverlayViewToggle => {
                self.graph_mode = !self.graph_mode;
                if self.graph_mode {
                    return self.refresh_graph_layout();
                }
                Task::none()
            }
            GraphMsg::GraphLaidOut {
                seq,
                overlay,
                layout,
                ..
            } => self.on_graph_laid_out(seq, overlay, layout),
            GraphMsg::Toggle3D => {
                self.graph_3d = !self.graph_3d;
                Task::none()
            }
            GraphMsg::ToggleSpin => {
                self.graph_spin = !self.graph_spin;
                Task::none()
            }
            GraphMsg::CloseOverlay => {
                self.proj.overlay = None;
                Task::none()
            }
            GraphMsg::OverlayOpenImports(path) => {
                self.proj.overlay = None;
                self.sidebar = SidebarTab::Imports;
                self.open_file(path, None, true)
            }
            GraphMsg::OverlayOpenAt { abs, line } => {
                self.proj.overlay = None;
                self.open_file(abs, Some(line), true)
            }
            GraphMsg::ProjectCallsBuilt { graph, .. } => self.on_project_calls_built(graph),
            GraphMsg::ChurnLoaded { result, .. } => self.on_churn_loaded(result),
            GraphMsg::ToggleHeat => {
                self.graph_heat = !self.graph_heat;
                Task::none()
            }
            GraphMsg::RefineProjectCalls => self.refine_project_calls(),
            GraphMsg::RefineWaitOver { wait, .. } => self.on_refine_wait_over(wait),
            GraphMsg::RefineProgress {
                generation,
                done,
                total,
                ..
            } => {
                if generation == self.proj.project_calls.generation {
                    self.proj.project_calls.refine_progress = Some((done, total));
                }
                Task::none()
            }
            GraphMsg::ProjectCallsRefined {
                generation, result, ..
            } => self.on_project_calls_refined(generation, result),
            GraphMsg::RemoteTsConfigsLoaded {
                generation, result, ..
            } => self.on_remote_ts_configs_loaded(generation, result),
        }
    }
}
