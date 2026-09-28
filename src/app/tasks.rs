//! Async task bodies and pure builders behind the feature flows: the LSP
//! call-hierarchy refine pass, LLM-input gatherers, the explain pass, SVG and
//! embedding jobs, and the small file/config helpers. Re-exported from the
//! crate root.

use crate::app::prelude::*;
use crate::*;

/// A file's name for a compact graph-node label (`client.rs`).
pub(crate) fn file_label(p: &std::path::Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// What the import-graph job builds its [`imports::Resolver`] from: a
/// snapshot the window hands over when the job is spawned — the file list is
/// shared, not copied (see `App::resolver_inputs`).
#[derive(Debug, Clone)]
pub(crate) struct ResolverInputs {
    pub root: PathBuf,
    pub files: Arc<Vec<fs_scan::FileEntry>>,
    /// What a REMOTE project's host sent for resolution; `None` for a local
    /// project, whose resolver reads go.mod, pubspec.yaml and its tsconfig
    /// files off disk — on the job's thread.
    pub remote: Option<RemoteResolution>,
}

/// A remote project's resolution metadata, from its host.
#[derive(Debug, Clone)]
pub(crate) struct RemoteResolution {
    /// go.mod's module and pubspec.yaml's package name.
    pub meta: (Option<String>, Option<String>),
    /// tsconfig/jsconfig path maps, once fetched: the window's, shared.
    pub ts_configs: Option<Arc<imports::TsConfigs>>,
}

impl ResolverInputs {
    /// Build the resolver. Blocking for a local project (it reads the
    /// metadata files), so it runs inside the import-graph job.
    pub(crate) fn build(&self) -> imports::Resolver {
        let files: Vec<PathBuf> = self.files.iter().map(|f| f.abs.clone()).collect();
        let Some(remote) = &self.remote else {
            return imports::Resolver::new(&self.root, &files);
        };
        let (go_module, dart_package) = remote.meta.clone();
        let resolver = imports::Resolver::with_meta(&self.root, &files, go_module, dart_package);
        match &remote.ts_configs {
            Some(configs) => resolver.with_ts_configs(configs.clone()),
            None => resolver,
        }
    }
}

/// What an import-graph job hands back (see `App::schedule_imports`).
#[derive(Debug, Clone)]
pub struct ImportJobDone {
    /// The graph with the job's batch applied.
    pub graph: Arc<imports::ImportGraph>,
    /// The resolver it resolved against — reused by the next job unless that
    /// one's batch asks for a new one.
    pub resolver: Arc<imports::Resolver>,
    /// The result's import cycles, and the Imports overlay's counts and
    /// rankings, both found when its edge structure changed.
    pub cycles: Option<Vec<Vec<PathBuf>>>,
    pub(crate) ranks: Option<crate::ui::ImportRanks>,
    pub applied: imports::Applied,
}

/// The import-graph job, on the blocking pool: apply `batch` to `base` — a
/// working copy of it, a key per file, unless the batch restates the project
/// (`ImportGraph::applied_to`) — building the resolver first when there is
/// none to reuse, and find the cycles of the result. The window holds on to
/// the batch until the job lands (`ImportWork::in_flight`), and the job reads
/// it there: what it installs is shared with it, not copied.
pub(crate) fn run_import_job(
    base: Arc<imports::ImportGraph>,
    batch: Arc<imports::ImportBatch>,
    resolver: Option<Arc<imports::Resolver>>,
    inputs: ResolverInputs,
) -> ImportJobDone {
    let resolver = resolver.unwrap_or_else(|| Arc::new(inputs.build()));
    let (graph, applied) =
        imports::ImportGraph::applied_to(base, &batch, &resolver, highlight::detect);
    // Whole-graph passes, here rather than on the UI thread — and only when
    // the structure moved: lines alone change neither.
    let cycles = applied.structure_changed.then(|| graph.cycles());
    let ranks = applied
        .structure_changed
        .then(|| crate::ui::import_ranks(&graph));
    ImportJobDone {
        graph: Arc::new(graph),
        resolver,
        cycles,
        ranks,
        applied,
    }
}

/// Maps a `(file, name, line)` reported by a language server onto the
/// [`projectcalls::SymKey`] of the definition it names: the same-name
/// definition of that file nearest at or above the line (a Java annotation or
/// a Rust attribute can put a definition's first line above its name's), else
/// the file's first one. Ordinals come from [`projectcalls::ProjectCallGraph::keys_of`],
/// the numbering the call graph and the explain engine share.
pub(crate) struct SymKeys {
    /// file → name → `(line, ordinal)` sorted by line.
    by_file: HashMap<PathBuf, HashMap<String, Vec<(usize, u32)>>>,
}

impl SymKeys {
    pub(crate) fn new(defs: &[projectcalls::Def]) -> SymKeys {
        let mut by_file: HashMap<PathBuf, HashMap<String, Vec<(usize, u32)>>> = HashMap::new();
        for (d, key) in defs
            .iter()
            .zip(projectcalls::ProjectCallGraph::keys_of(defs))
        {
            by_file
                .entry(d.file.clone())
                .or_default()
                .entry(d.name.clone())
                .or_default()
                .push((d.line, key.2));
        }
        for names in by_file.values_mut() {
            for lines in names.values_mut() {
                lines.sort_unstable();
            }
        }
        SymKeys { by_file }
    }

    /// The key for `name` in `file` near 1-based `line`. A name the outline
    /// does not know keys as ordinal 0 (and matches no node, so its edges
    /// are dropped when the graph is assembled).
    pub(crate) fn key(&self, file: &Path, name: &str, line: usize) -> projectcalls::SymKey {
        let ordinal = self
            .by_file
            .get(file)
            .and_then(|names| names.get(name))
            .and_then(|defs| {
                defs.iter()
                    .rev()
                    .find(|(l, _)| *l <= line)
                    .or_else(|| defs.first())
                    .map(|(_, ordinal)| *ordinal)
            })
            .unwrap_or(0);
        (file.to_path_buf(), name.to_string(), ordinal)
    }
}

/// A count of the whole-project work that goes into an LSP refine pass — each
/// symbol-index entry listed ([`refine_defs`]), each edge of the set it
/// patches ([`RefinePass::plan`]) — for the tests: recorded per thread, so a
/// test can show that the window's update does none of it (the pass does, on
/// the blocking pool) and tests running in parallel do not count each other's.
/// Outside test builds `add` is an empty inline function.
pub(crate) mod refine_work {
    #[cfg(test)]
    thread_local! {
        static DONE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// Record `n` units of work on this thread.
    #[inline(always)]
    pub(crate) fn add(n: usize) {
        #[cfg(test)]
        DONE.with(|d| d.set(d.get().saturating_add(n)));
        #[cfg(not(test))]
        let _ = n;
    }

    /// Run `f`, returning its result and the work it recorded on this thread.
    #[cfg(test)]
    pub(crate) fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let before = DONE.with(std::cell::Cell::get);
        let out = f();
        (out, DONE.with(std::cell::Cell::get) - before)
    }

    #[cfg(test)]
    thread_local! {
        static HOLDERS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }

    /// Note, on this thread, how many hold the symbol index once an
    /// incremental pass has patched its edge set (`RefinePass::plan`).
    #[cfg(test)]
    pub(crate) fn note_holders(n: usize) {
        HOLDERS.with(|h| h.set(Some(n)));
    }

    /// The count [`note_holders`] noted last on this thread, taken.
    #[cfg(test)]
    pub(crate) fn noted_holders() -> Option<usize> {
        HOLDERS.with(std::cell::Cell::take)
    }
}

/// Every callable definition among `index` — each file's symbol-index entries,
/// in index order — in a language the pass covers (`covers`): the node set a
/// refine pass maps its answers onto. Walks the whole index, so the window
/// runs it only for the "Refine with LSP" button, and an incremental pass
/// runs it on the blocking pool ([`RefinePass::plan`]).
pub(crate) fn refine_defs<'a>(
    index: impl IntoIterator<Item = &'a [SymbolEntry]>,
    covers: impl Fn(&str) -> bool,
) -> Vec<projectcalls::Def> {
    let mut visited = 0;
    let defs = index
        .into_iter()
        .flatten()
        .inspect(|_| visited += 1)
        .filter(|s| outline::is_callable(&s.kind) && highlight::detect(&s.abs).is_some_and(&covers))
        .map(|s| projectcalls::Def {
            name: s.name.clone(),
            kind: s.kind.clone(),
            file: s.abs.clone(),
            line: s.line,
        })
        .collect();
    refine_work::add(visited);
    defs
}

/// The name of a file whose incremental refine panics while it is planned, on
/// the blocking pool — in tests only, where it is how a pass that fails is
/// proven to hand the call graph back rather than hold it for good.
#[cfg(test)]
pub(crate) const PANICKING_PLAN_FILE: &str = "planning_this_panics.rs";

/// The name of a file whose functions panic the refine pass as it queries
/// them, in the stream itself — in tests only, where it is how a pass that
/// unwinds is proven to end all the same.
#[cfg(test)]
pub(crate) const PANICKING_QUERY_FILE: &str = "querying_this_panics.rs";

/// What an LSP refine pass is handed when it is spawned (`App::spawn_refine`):
/// what it queries, and what their answers patch.
#[derive(Debug)]
pub(crate) enum RefinePass {
    /// The "Refine with LSP" button: every function listed here — the window
    /// lists them to say how many it refines — is queried for its callers,
    /// and the answers make up the whole edge set.
    Full(Vec<projectcalls::Def>),
    /// The files changed since the refined graph was computed, refined into
    /// it — once per watcher batch while the refine owns the graph, so the
    /// window hands over snapshots and the pass does the whole-project work
    /// on the blocking pool ([`RefinePass::plan`]).
    Incremental {
        /// Every file's symbol-index entries: a share of the window's map,
        /// not a copy nor a handle per file — the node set is listed from
        /// them, and the share let go at once (the window writes the map in
        /// place only while nothing else holds it).
        index: crate::app::state::PerFile<Arc<Vec<SymbolEntry>>>,
        /// The languages the refinement covers
        /// (`ProjectCallsState::refined_langs`): the node set is their
        /// functions, whether or not their server is ready now.
        langs: HashSet<String>,
        /// The refined edge set, lent by the window (its own is empty until
        /// the pass lands): the edges touching a changed file are dropped
        /// from it, the answers added, and it is handed back with the result.
        edges: projectcalls::SymEdges,
        /// The changed files — edited, created or deleted: their functions
        /// are queried in both directions.
        changed: HashSet<PathBuf>,
    },
}

/// A refine pass's plan ([`RefinePass::plan`]): its node set, the functions
/// it queries, their keys, and the edges the answers are added to.
pub(crate) struct RefinePlan {
    /// Every callable definition of the languages the pass covers.
    pub(crate) defs: Vec<projectcalls::Def>,
    /// The indices in `defs` of the functions to query.
    pub(crate) query: Vec<usize>,
    /// `defs`' keys, which the answers are mapped onto.
    pub(crate) keys: SymKeys,
    /// The edges the pass starts from.
    pub(crate) edges: projectcalls::SymEdges,
}

impl RefinePass {
    /// The pass's plan — whole-project work (listing, keying, patching), for
    /// the blocking pool.
    pub(crate) fn plan(self) -> RefinePlan {
        match self {
            RefinePass::Full(defs) => RefinePlan {
                query: (0..defs.len()).collect(),
                keys: SymKeys::new(&defs),
                defs,
                edges: projectcalls::SymEdges::default(),
            },
            RefinePass::Incremental {
                index,
                langs,
                mut edges,
                changed,
            } => {
                #[cfg(test)]
                if changed
                    .iter()
                    .any(|f| f.file_name().is_some_and(|n| n == PANICKING_PLAN_FILE))
                {
                    panic!("a planted refine panic");
                }
                let defs = refine_defs(index.values().map(|syms| syms.as_slice()), |lang| {
                    langs.contains(lang)
                });
                // Listed: the share goes now, not when the pass ends — a
                // watcher batch landing while it is held copies the window's
                // map before writing it.
                #[cfg(test)]
                let share = index.downgrade();
                drop(index);
                let query = (0..defs.len())
                    .filter(|&i| changed.contains(&defs[i].file))
                    .collect();
                // The edges touching a changed file are re-derived from this
                // pass's answers.
                refine_work::add(edges.len());
                edges.retain(|((caller, _, _), (callee, _, _))| {
                    !changed.contains(caller) && !changed.contains(callee)
                });
                #[cfg(test)]
                refine_work::note_holders(share.strong_count());
                RefinePlan {
                    keys: SymKeys::new(&defs),
                    defs,
                    query,
                    edges,
                }
            }
        }
    }
}

/// What an LSP refine pass lands with (see `GraphMsg::ProjectCallsRefined`).
///
/// A query that failed is told from one answered with no calls, which it
/// used to read as: the function's edges went missing without a word.
#[derive(Debug, Clone, Default)]
pub struct RefineDone {
    /// The symbol-keyed edge set, kept to patch the next incremental pass.
    pub edges: projectcalls::SymEdges,
    /// The display graph, assembled from it over the pass's node set.
    pub graph: projectcalls::ProjectCallGraph,
    /// Whether the pass queried every function of its languages, rather than
    /// those of the files that changed.
    pub full: bool,
    /// The files a query of went unanswered as its server stopped — its
    /// session over, its connection closed — each with why: nothing more
    /// will come from that server, and the files' edges are not known.
    pub stopped: HashMap<PathBuf, String>,
    /// The functions a query of failed otherwise, even asked again — an
    /// error for an answer, or none in time — or that were not asked, each
    /// with why: calls into them (and, from a pass over changed files, out
    /// of them) are not known. One such symbol is no reason to drop every
    /// other function's edges.
    pub unrefined: HashMap<projectcalls::SymKey, String>,
    /// The files of those of `unrefined` whose failure may pass — the file
    /// changed under the query, the server cancelled it or did not answer
    /// in time, or stopped being asked: refined again with the next pass
    /// that runs (`ProjectCallsState::refine_retry`). Asked again at once,
    /// a server that is wedged would be, in a loop.
    pub retry: HashSet<PathBuf>,
    /// The files a query of failed while its server was loading the
    /// project — asked about what it had not read yet ("file not found"):
    /// refined again once it has loaded, held for it meanwhile
    /// (`App::fold_refine_pending`). Their functions are not said to be
    /// unrefined: a pass is about to ask for them.
    pub loading: HashSet<PathBuf>,
    /// The languages whose server the pass stopped asking, after it let
    /// [`REFINE_BREAKER_TIMEOUTS`] queries in a row go unanswered in time:
    /// each would have cost the whole time box, twice. Their functions it
    /// did not ask are in `unrefined`, and `retry`.
    pub unanswering: std::collections::BTreeSet<String>,
}

/// How long the refine gives one function — its call hierarchy prepared at
/// the name, its calls read — on one try: a wedged server must not hang the
/// whole pass.
#[cfg(not(test))]
const REFINE_QUERY_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const REFINE_QUERY_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Background LSP call-hierarchy pass, driving a stream of progress messages and
/// a final precise call graph.
///
/// It first plans the pass on the blocking pool ([`RefinePass::plan`]): the
/// node set its answers map onto, the functions it queries, the edges it
/// starts from — for an incremental run, those touching a changed file are
/// dropped, then re-derived, so only the changed functions are re-queried.
/// For each queried function it prepares a call hierarchy at the name's
/// position and reads its incoming calls (and, for an incremental run,
/// outgoing too), adding symbol-keyed `(caller, callee)` edges kept to the
/// project; the display graph is assembled on the blocking pool too.
/// Bounded-concurrent, each function's queries time-boxed so a wedged server
/// can't hang the pass; one that failed in a way that may pass is asked once
/// more, after a jittered pause ([`retry_backoff`]), and what still fails is
/// told apart in the result ([`RefineDone`]): failed while its server loads
/// the project, left for it to load; failed in a way that may pass, asked
/// again with the next pass. A server that lets [`REFINE_BREAKER_TIMEOUTS`]
/// queries in a row go unanswered in time is asked nothing more ([`Breaker`]).
///
/// It always ends with its result — its failure included: the window takes a
/// pass in flight for the owner of the call graph, so one that ended in
/// silence owned it for the session. A step on the blocking pool that panics
/// fails the pass, and so does a panic in the pass itself, which it catches:
/// iced runs this stream as one future, the channel's receiving end with it,
/// so a pass that unwound took every message it had left along — a drop
/// guard's too.
pub(crate) async fn refine_stream(
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    pass: RefinePass,
    clients: HashMap<String, lsp::client::LspClient>,
    root: PathBuf,
    // The project instance the pass runs for; every message carries it.
    stamp: Stamp,
    generation: u64,
    // Set for a REMOTE project: the files live on the other host, so their
    // text is fetched over the protocol instead of read from this disk.
    remote: Option<crate::AiClient>,
) {
    use iced::futures::{FutureExt, SinkExt};

    let run = refine_pass(&mut output, pass, clients, root, &stamp, generation, remote);
    // Nothing the pass holds is used after it unwinds: only `output`, whose
    // channel a panic cannot leave half-written.
    let result = std::panic::AssertUnwindSafe(run)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err("the pass panicked".to_string()));
    let _ = output
        .send(Message::Graph(GraphMsg::ProjectCallsRefined {
            stamp,
            generation,
            result,
        }))
        .await;
}

/// The pass [`refine_stream`] runs: its progress sent to `output`, its
/// result returned.
async fn refine_pass(
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
    pass: RefinePass,
    clients: HashMap<String, lsp::client::LspClient>,
    root: PathBuf,
    stamp: &Stamp,
    generation: u64,
    remote: Option<crate::AiClient>,
) -> Result<RefineDone, String> {
    use iced::futures::{SinkExt, StreamExt};
    use lsp::client::QueryError;

    let incremental = matches!(pass, RefinePass::Incremental { .. });
    let RefinePlan {
        defs,
        query,
        keys,
        mut edges,
    } = tokio::task::spawn_blocking(move || pass.plan())
        .await
        .map_err(|_| "listing the project's functions panicked".to_string())?;
    let query_defs: Vec<&projectcalls::Def> = query.iter().map(|&i| &defs[i]).collect();

    // File lines, read once, for locating each function name's column. A
    // wrong column resolves a different symbol on the line, so for a remote
    // project this must come from the host that owns the files — reading this
    // machine's disk at the remote's paths would produce edges for whatever
    // happens to be there.
    let mut file_lines: HashMap<PathBuf, Vec<String>> = HashMap::new();
    // The files whose functions the index lists and the host no longer
    // holds: those functions are not queried (see below).
    let mut unplaced: HashSet<PathBuf> = HashSet::new();
    // The files the host holds and could not read, each with why: their
    // functions are not queried, and said to be unrefined.
    let mut unreadable: HashMap<PathBuf, String> = HashMap::new();
    match &remote {
        None => {
            for d in &query_defs {
                file_lines.entry(d.file.clone()).or_insert_with(|| {
                    // Same guard as the other project-source readers (the
                    // symbol index, `tasks::gather_*`): `query_defs` was built
                    // from a scan that can be minutes old, so a listed leaf may
                    // since have become a symlink pointing outside the project
                    // or a FIFO — and this task runs inside a blocking read
                    // that `project_calls.refine_abort` cannot cancel, so a
                    // wedged `open(2)` parks the refine pass for good. A refusal
                    // degrades to no lines for that file, which the existing
                    // `unwrap_or_default` already handles (column 0).
                    clew_core::fs_scan::read_confined_capped(
                        &root,
                        &d.file,
                        index::MAX_INDEX_FILE_BYTES,
                    )
                    .map(|s| s.lines().map(str::to_string).collect())
                    .unwrap_or_default()
                });
            }
        }
        Some(ai) => {
            let mut rels: Vec<String> = query_defs
                .iter()
                .filter_map(|d| d.file.strip_prefix(&root).ok())
                .map(|r| r.to_string_lossy().into_owned())
                .collect();
            rels.sort();
            rels.dedup();
            // Asked until the host has settled each: a reply names what it
            // had no room for.
            let ask = move |rels| ai.request(clew_protocol::Request::ReadSources { rels });
            let host = crate::sources::read(rels, ask).await;
            // A file the host did not answer for is not known: the pass
            // fails, and says which, with why its own batch went unanswered.
            // Its functions, queried at column 0, were asked about whatever
            // sits there — nothing, mostly — and read as called by nothing.
            // The next pass asks for it again.
            if let Some((rel, why)) = host.unread.first() {
                let why = why.as_ref().map(|why| format!(": {why}"));
                return Err(format!(
                    "could not read {rel} from the remote host{}",
                    why.unwrap_or_default()
                ));
            }
            for (rel, text) in host.files {
                file_lines.insert(root.join(&rel), text.lines().map(str::to_string).collect());
            }
            // One the host holds and could not read — this user may not —
            // is not queried either, and its functions are said to be
            // unrefined, with the host's reason: failing the pass for it
            // failed every pass, for as long as the file stayed so.
            unreadable.extend(
                host.unreadable
                    .into_iter()
                    .map(|(rel, why)| (root.join(rel), why)),
            );
            // One not there, too large to read or no plain text file has
            // changed since the index was read, into a file the index drops
            // — and a remote refinement gives way to that change once the
            // host publishes it. Asked again, the host would say the same:
            // its functions are not queried.
            let too_large = host.too_large.into_iter().map(|(rel, _)| rel);
            let gone = host
                .missing
                .into_iter()
                .chain(too_large)
                .chain(host.refused.into_iter().map(|(rel, _)| rel));
            unplaced.extend(gone.map(|rel| root.join(rel)));
        }
    }

    // One query per function; `key` is its symbol identity for edge endpoints:
    // `(file, name, ordinal)` — the identity the call graph and the explain
    // engine share — so a file's two `new`s stay two nodes instead of
    // collapsing onto one.
    struct Query {
        key: projectcalls::SymKey,
        client: lsp::client::LspClient,
        lang: &'static str,
        breaker: Arc<Breaker>,
        file: PathBuf,
        line0: usize,
        character: usize,
    }
    // One breaker per server: queries run a dozen at a time, and each that
    // goes unanswered costs the whole time box.
    let breakers: HashMap<&str, Arc<Breaker>> = clients
        .keys()
        .map(|lang| (lang.as_str(), Arc::default()))
        .collect();
    let mut unrefined: HashMap<projectcalls::SymKey, String> = HashMap::new();
    let mut queries = Vec::new();
    for d in &query_defs {
        #[cfg(test)]
        if d.file
            .file_name()
            .is_some_and(|n| n == PANICKING_QUERY_FILE)
        {
            panic!("a planted query panic");
        }
        if unplaced.contains(&d.file) {
            continue;
        }
        if let Some(why) = unreadable.get(&d.file) {
            let key = keys.key(&d.file, &d.name, d.line);
            unrefined.insert(key, format!("the remote host could not read it: {why}"));
            continue;
        }
        let Some(lang) = highlight::detect(&d.file) else {
            continue;
        };
        let (Some(client), Some(breaker)) = (clients.get(lang), breakers.get(lang)) else {
            continue;
        };
        let line0 = d.line.saturating_sub(1);
        // The name's start, in the server's position encoding (`find` answers
        // a char boundary, so the prefix slice is whole characters).
        let character = file_lines
            .get(&d.file)
            .and_then(|lines| lines.get(line0))
            .and_then(|text| {
                text.find(&d.name)
                    .map(|b| client.encoding.units_in(&text[..b]))
            })
            .unwrap_or(0);
        queries.push(Query {
            key: keys.key(&d.file, &d.name, d.line),
            client: client.clone(),
            lang,
            breaker: breaker.clone(),
            file: d.file.clone(),
            line0,
            character,
        });
    }

    let total = queries.len();
    // An incremental pass was spawned before it knew how many functions it
    // queries (the window lists none of them): the count, now that it does.
    if incremental {
        let _ = output
            .send(Message::Graph(GraphMsg::RefineProgress {
                stamp: stamp.clone(),
                generation,
                done: 0,
                total,
            }))
            .await;
    }
    let mut stream = iced::futures::stream::iter(queries.into_iter().map(|q| async move {
        let attempt = || async {
            // Not asked at all once its server stopped answering in time.
            if q.breaker.open() {
                return Err(Asked::NotAsked);
            }
            let work = async {
                let items = q
                    .client
                    .try_prepare_call_hierarchy(&q.file, q.line0, q.character)
                    .await?;
                let mut incoming = Vec::new();
                let mut outgoing = Vec::new();
                for it in items {
                    incoming.extend(q.client.try_incoming_calls(it.raw.clone()).await?);
                    if incremental {
                        outgoing.extend(q.client.try_outgoing_calls(it.raw).await?);
                    }
                }
                Ok::<_, QueryError>((incoming, outgoing))
            };
            // Dropped at the time box, the requests in flight are cancelled
            // at the server (`framing::Handle::call`), not left running
            // beside the ones a retry sends.
            let answer = match tokio::time::timeout(REFINE_QUERY_LIMIT, work).await {
                Ok(answer) => answer,
                Err(_) => Err(QueryError::TimedOut),
            };
            q.breaker.note(&answer);
            answer.map_err(Asked::Failed)
        };
        let answer = match attempt().await {
            // A server still loading the project failed what it has not read
            // yet: asked again once it has loaded, not now.
            Err(Asked::Failed(e)) if e.transient() && !q.client.loading() => {
                // Asked once more when the failure may pass — the file changed
                // under the query (rust-analyzer drops what is in flight on
                // every edit), the server cancelled it, or did not answer in
                // time — after a pause: asked again at once, the query met the
                // same burst of edits, or the same busy server. Jittered, so
                // the dozen in flight do not all come back at once.
                tokio::time::sleep(retry_backoff()).await;
                match attempt().await {
                    // Its server stopped being asked meanwhile: the failure
                    // it met is its own.
                    Err(Asked::NotAsked) => Err(Asked::Failed(e)),
                    again => again,
                }
            }
            answer => answer,
        };
        let outcome = match answer {
            Ok(calls) => Outcome::Answered(calls),
            Err(Asked::NotAsked) => Outcome::Transient(format!(
                "not asked: the {} server left {REFINE_BREAKER_TIMEOUTS} queries in a row \
                 unanswered",
                q.lang
            )),
            // A server that stopped answers nothing more; one that failed
            // this function may still answer the rest.
            Err(Asked::Failed(e)) if matches!(e, QueryError::Gone(_)) || !q.client.alive() => {
                Outcome::Stopped(e.to_string())
            }
            Err(Asked::Failed(_)) if q.client.loading() => Outcome::Loading,
            Err(Asked::Failed(e)) if e.transient() => Outcome::Transient(e.to_string()),
            Err(Asked::Failed(e)) => Outcome::Failed(e.to_string()),
        };
        (q.key, q.file, outcome)
    }))
    .buffer_unordered(12);

    let in_project = |p: &Path| p.starts_with(&root);

    let mut done = 0usize;
    let mut stopped: HashMap<PathBuf, String> = HashMap::new();
    let mut retry: HashSet<PathBuf> = HashSet::new();
    let mut loading: HashSet<PathBuf> = HashSet::new();
    while let Some((key, file, outcome)) = stream.next().await {
        match outcome {
            Outcome::Answered((incoming, outgoing)) => {
                for caller in incoming {
                    if in_project(&caller.path) {
                        let from = keys.key(&caller.path, &caller.name, caller.line + 1);
                        edges.insert((from, key.clone()));
                    }
                }
                for callee in outgoing {
                    if in_project(&callee.path) {
                        let to = keys.key(&callee.path, &callee.name, callee.line + 1);
                        edges.insert((key.clone(), to));
                    }
                }
            }
            Outcome::Stopped(why) => {
                stopped.entry(file).or_insert(why);
            }
            Outcome::Loading => {
                loading.insert(file);
            }
            Outcome::Transient(why) => {
                unrefined.insert(key, why);
                retry.insert(file);
            }
            Outcome::Failed(why) => {
                unrefined.insert(key, why);
            }
        }
        done += 1;
        if done.is_multiple_of(16) || done == total {
            let _ = output
                .send(Message::Graph(GraphMsg::RefineProgress {
                    stamp: stamp.clone(),
                    generation,
                    done,
                    total,
                }))
                .await;
        }
    }

    let unanswering = breakers
        .iter()
        .filter(|(_, breaker)| breaker.open())
        .map(|(lang, _)| lang.to_string())
        .collect();

    // The display graph, over the whole node set: on the blocking pool too.
    tokio::task::spawn_blocking(move || {
        let graph = projectcalls::ProjectCallGraph::graph_from_sym_edges(defs, &edges);
        RefineDone {
            edges,
            graph,
            full: !incremental,
            stopped,
            unrefined,
            retry,
            loading,
            unanswering,
        }
    })
    .await
    .map_err(|_| "assembling the refined graph panicked".to_string())
}

/// How one function's query went, in the refine pass ([`refine_pass`]).
enum Outcome {
    /// Answered: its incoming calls, and outgoing for a pass over changed
    /// files.
    Answered((Vec<lsp::client::CallItem>, Vec<lsp::client::CallItem>)),
    /// Its server stopped — the session over — with why.
    Stopped(String),
    /// It failed while its server was loading the project.
    Loading,
    /// It failed in a way that may pass, even asked again, or was not
    /// asked: its server stopped answering in time. With why.
    Transient(String),
    /// It failed as it would again, with why.
    Failed(String),
}

/// Why one try at a function's query got no answer ([`refine_pass`]).
enum Asked {
    /// Its server had stopped answering in time ([`Breaker`]): not asked.
    NotAsked,
    /// Asked, and it failed.
    Failed(lsp::client::QueryError),
}

/// How long the refine waits before it asks again a function whose query
/// failed in a way that may pass ([`retry_backoff`]): at least this, at most
/// twice it. rust-analyzer drops every request in flight on each edit, and
/// a save — a format-on-save, a rename across files — is a burst of them.
#[cfg(not(test))]
pub(crate) const REFINE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(750);
#[cfg(test)]
pub(crate) const REFINE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(60);

/// The pause before a retry: [`REFINE_RETRY_BACKOFF`] plus as much again at
/// most, at random — the dozen queries in flight that failed together do
/// not all come back at the same instant.
fn retry_backoff() -> std::time::Duration {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static DRAWS: AtomicU64 = AtomicU64::new(0);
    // A randomly keyed hasher over a counter: random enough to spread
    // retries, with no dependency for it.
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(DRAWS.fetch_add(1, Ordering::Relaxed));
    let unit = (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64;
    REFINE_RETRY_BACKOFF + REFINE_RETRY_BACKOFF.mul_f64(unit)
}

/// After this many of its queries in a row went unanswered in time, a
/// server is asked nothing more in the pass ([`Breaker`]).
pub(crate) const REFINE_BREAKER_TIMEOUTS: u32 = 3;

/// One server's circuit breaker in a refine pass: a server that is alive
/// and does not answer — wedged — cost the whole time box for each of its
/// functions, twice, a dozen at a time, for a pass that then refined none
/// of them. After [`REFINE_BREAKER_TIMEOUTS`] timeouts in a row it is open:
/// the server is not asked again this pass, and the pass says so
/// ([`RefineDone::unanswering`]).
#[derive(Default)]
struct Breaker {
    /// Its queries in a row that went unanswered in time; any answer, an
    /// error included, starts the count again.
    timeouts: std::sync::atomic::AtomicU32,
    /// Open: not asked again.
    open: std::sync::atomic::AtomicBool,
}

impl Breaker {
    /// Whether the server is asked nothing more.
    fn open(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Count one try's answer.
    fn note<T>(&self, answer: &Result<T, lsp::client::QueryError>) {
        use std::sync::atomic::Ordering;
        if matches!(answer, Err(lsp::client::QueryError::TimedOut)) {
            if self.timeouts.fetch_add(1, Ordering::Relaxed) + 1 >= REFINE_BREAKER_TIMEOUTS {
                self.open.store(true, Ordering::Relaxed);
            }
        } else {
            self.timeouts.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod refine_tests {
    use super::*;
    use lsp::client::QueryError;

    /// The pause before a retry is the backoff at least, twice it at most,
    /// and not the same each time: the dozen queries in flight that failed
    /// together came back at the same instant, to meet the same burst.
    #[test]
    fn the_pause_before_a_retry_is_jittered_within_its_bounds() {
        let pauses: Vec<std::time::Duration> = (0..64).map(|_| retry_backoff()).collect();
        assert!(
            pauses
                .iter()
                .all(|p| *p >= REFINE_RETRY_BACKOFF && *p <= REFINE_RETRY_BACKOFF * 2),
            "{pauses:?}"
        );
        assert!(
            pauses.iter().any(|p| *p != pauses[0]),
            "every pause the same: {pauses:?}"
        );
    }

    /// A server's breaker opens after its queries went unanswered in time so
    /// many times in a row — and any answer, an error included, starts the
    /// count again: a server that answers most is still asked.
    #[test]
    fn a_breaker_opens_only_after_timeouts_in_a_row() {
        let breaker = Breaker::default();
        let timed_out: Result<(), QueryError> = Err(QueryError::TimedOut);
        let failed: Result<(), QueryError> = Err(QueryError::Failed {
            code: Some(-32603),
            message: "no".into(),
        });
        for answer in [
            &timed_out,
            &timed_out,
            &Ok(()),
            &timed_out,
            &timed_out,
            &failed,
        ] {
            breaker.note(answer);
        }
        assert!(!breaker.open(), "opened on timeouts that were not in a row");
        for _ in 0..REFINE_BREAKER_TIMEOUTS {
            breaker.note(&timed_out);
        }
        assert!(breaker.open());
    }
}

// Every system prompt below is handed repository text — code, names, commit
// messages, diffs, and summaries a model wrote about them — and ends with the
// one rule for it (`clew_core::untrusted_text_rule!`): material to describe,
// never instructions to follow. The user prompts fence that text and say so
// again (`explain::fenced`, `explain::UNTRUSTED_NOTE`).

/// System prompt for the explain pass.
pub(crate) const EXPLAIN_SYSTEM: &str = concat!(
    "You are an expert code explainer. You are given a \
function, file, or folder, plus concise summaries of what it depends on. Reply \
with a plain-prose explanation of what it does and why it exists — 2 to 4 \
sentences, no preamble, no bullet points, no restating the code.",
    clew_core::untrusted_text_rule!()
);

/// System prompt for the on-demand per-block walkthrough (the `Explain blocks`
/// drill-down). Unlike [`EXPLAIN_SYSTEM`] this asks for structured Markdown.
pub(crate) const EXPLAIN_BLOCKS_SYSTEM: &str = concat!(
    "You are an expert code explainer. Walk \
through the given function block by block, in the order the code executes. For \
each logical block write a short bold Markdown heading naming what it does, then \
one or two sentences on how and why, quoting key lines with inline code. Be \
precise and concise; do not restate every line. Output GitHub-flavored Markdown.",
    clew_core::untrusted_text_rule!()
);

/// System prompt for "Ask clew": answers grounded in the retrieved code context.
pub(crate) const ASK_SYSTEM: &str = concat!(
    "You are answering a developer's questions about THIS \
codebase in an ongoing conversation. Earlier turns are included; a follow-up may \
refer to them (\"it\", \"that function\", \"why?\"). Use ONLY the provided code \
context — the most semantically relevant functions and files, each with a summary \
and (for functions) its source — together with the conversation so far. Whenever \
you name a file or function from the context, cite it as a Markdown link so the \
reader can click straight to it, using the path and line from that item's header: \
[name](path#Lline) — e.g. a header `### main — src/main.rs (L68)` becomes \
[main](src/main.rs#L68). Link on first mention rather than using bare backticks. \
If a \"Runtime state\" block is present, the program \
is PAUSED in the debugger — use the live call stack and variable values to \
answer questions about what is happening at that point (e.g. why a variable \
holds its value, or which branch was taken). If the context doesn't contain the \
answer, say so briefly instead of guessing. CRITICAL: do not infer or invent \
control flow, triggers, timing, or mechanisms that are not explicitly shown in \
the provided context — never write things like \"polls periodically\", \"runs on \
a background timer\", or \"the watcher marks it dirty\" unless that exact code is \
in the context. If the context shows WHAT happens but not HOW or WHEN it is \
triggered (or the relevant subsystem clearly isn't among the retrieved files), \
say that the triggering/handling code isn't in the retrieved context rather than \
describing a plausible-sounding mechanism. Be concise and concrete. Output \
GitHub-flavored Markdown.",
    clew_core::untrusted_text_rule!()
);

/// System prompt for "Why is this here?": explain a line/selection's reason for
/// existing from the commit(s) that introduced it.
pub(crate) const WHY_SYSTEM: &str = concat!(
    "You explain WHY a specific piece of code exists, using \
the commit(s) that introduced or last changed it. You are given the code and, \
for each relevant commit, its message and the change it made to this file. \
Answer the developer's implicit question — why is this here? what problem does \
it solve, or what does it guard against? — concretely and grounded in the commit \
intent. 2 to 4 sentences of GitHub-flavored Markdown. Do not just restate what \
the code obviously does; focus on the WHY. If the commit messages are \
uninformative, say what can be inferred from the change and note the history is \
terse.",
    clew_core::untrusted_text_rule!()
);

/// System prompt for a time-travel step's "what & why": summarize one commit.
pub(crate) const TIME_WHY_SYSTEM: &str = concat!(
    "You explain a single git commit's change to a \
developer scrubbing through a file's history. Given the commit message and its \
diff for one file, write 1-2 plain-English sentences: WHAT changed and WHY (the \
intent) — grounded ONLY in the diff and message, never invented. Do not restate \
the diff line by line. If the message already gives the reason, use it. If the \
history is terse, say what can be inferred. Plain text, no Markdown headers.",
    clew_core::untrusted_text_rule!()
);

/// System prompt for "the story of this code block": a narrative of its
/// evolution (a function, struct, enum, class, trait, …).
pub(crate) const TIME_STORY_SYSTEM: &str = concat!(
    "You are telling the story of how ONE code block \
— a function, struct, enum, class, trait, or similar — evolved, for a developer \
trying to understand why it is the way it is. You are given the block's kind and \
name and a reverse-chronological list of the commits that changed it (each with \
its message and diff). Write a short GitHub-flavored Markdown narrative of 3 to 6 \
steps IN CHRONOLOGICAL ORDER (oldest first): what each meaningful change did and \
why, and how it reached its current shape. Be concrete and specific to these \
diffs; skip trivial/formatting commits. Ground every claim in the provided diffs \
— do not invent motivations. Finish with a one-line **Today:** summary of what \
it now is.",
    clew_core::untrusted_text_rule!()
);

/// Auto-refresh runs at most this often. When watched source files change, the
/// understanding (explanations → semantic index → overview) is refreshed, but a
/// burst of edits coalesces into one pass no sooner than this after the last.
/// A manual (user-initiated) refresh ignores the cooldown.
pub(crate) const AUTO_REFRESH_MIN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// An explain pass's sources as its reader found them: the text and language
/// of each explainable file it read, the files it could not read, and those
/// it read and cannot explain, with why. A listed file in none is gone (see
/// `explain::Inputs::unread`).
#[derive(Debug, Default)]
pub(crate) struct ExplainSources {
    pub(crate) read: HashMap<PathBuf, (String, &'static str)>,
    pub(crate) unread: HashSet<PathBuf>,
    pub(crate) unexplainable: HashMap<PathBuf, explain::Unexplainable>,
    /// Every file the project's tree lists, source or not: what each
    /// folder's listing is made of ([`folder_listings`]).
    pub(crate) listed: Vec<PathBuf>,
}

/// Read a LOCAL project's explainable `files` — every file its tree lists —
/// (a remote pass fetches them with [`fetch_explain_sources`]). Blocking; run
/// off the UI thread.
///
/// The project's folder is looked at first, and again after: while it is not
/// there — an unmounted volume, a folder renamed away — every file under it
/// reads as not there, which would read as every file deleted. The pass then
/// stops, saying why (`Err`), and changes nothing.
pub(crate) fn read_explain_sources(
    files: Vec<PathBuf>,
    root: &Path,
) -> Result<ExplainSources, ExplainFailure> {
    project_folder_is_there(root)?;
    let mut sources = ExplainSources::default();
    for f in &files {
        let Some(lang) = highlight::detect(f) else {
            continue;
        };
        // One confined, capped read instead of stat-the-name then read-the-name.
        // Those two resolved the path independently and neither checked what it
        // landed on, so a symlink left in the project passed the size gate on
        // its target's size and that target's text — a file from anywhere on
        // this machine — went into the LLM prompt and off to the provider. A
        // FIFO in the same position blocked `read_to_string` with no writer,
        // wedging this blocking task for the life of the process.
        //
        // Only a file that is not there is gone. One that is there and this
        // user may not read, or whose read failed, is held — not known to
        // have changed. One that outgrew the cap, is not UTF-8, or is not a
        // plain file of the project cannot be explained, and will not be
        // until it changes: what was recorded under it goes. A read inside
        // a save says none of that, nor what the file holds: the file is
        // read again until it stands still (`explain::read_steadily`). One
        // that never did — a file rewritten all the time — is read as it was
        // last found, and is never taken for gone, or for one that cannot be
        // explained: it is read again next time.
        let (read, steady) = explain::read_steadily(f, || {
            let read = clew_core::fs_scan::read_confined_capped_checked(
                root,
                f,
                index::MAX_INDEX_FILE_BYTES,
            );
            #[cfg(test)]
            save_faults::hit(f);
            read
        });
        match read {
            Ok(Some(content)) => {
                sources.read.insert(f.clone(), (content, lang));
            }
            Ok(None) if steady => {}
            Err(e)
                if steady
                    && let Some(why) = explain::Unexplainable::of_confined_read(&e, root, f) =>
            {
                sources.unexplainable.insert(f.clone(), why);
            }
            Ok(None) | Err(_) => {
                sources.unread.insert(f.clone());
            }
        }
    }
    project_folder_is_there(root)?;
    sources.listed = files;
    Ok(sources)
}

/// A save a unit test lands in the middle of a pass's read of a file: the
/// file is written right after its bytes are read, before the read is looked
/// back on (`explain::read_steadily`). Keyed by path, so tests running in
/// parallel never trip each other's.
#[cfg(test)]
pub(crate) mod save_faults {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static ARMED: Mutex<Vec<(PathBuf, Vec<u8>)>> = Mutex::new(Vec::new());

    /// Write `bytes` to `path` once a pass has next read it.
    pub(crate) fn arm(path: &Path, bytes: &[u8]) {
        ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((path.to_path_buf(), bytes.to_vec()));
    }

    /// Land the save armed for `path`, once (the lock is released first).
    pub(crate) fn hit(path: &Path) {
        let armed = {
            let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
            let at = armed.iter().position(|(p, _)| p == path);
            at.map(|i| armed.remove(i))
        };
        if let Some((path, bytes)) = armed {
            std::fs::write(&path, bytes).expect("the save lands");
        }
    }
}

/// Whether the project's folder is there to be read, and why not.
fn project_folder_is_there(root: &Path) -> Result<(), ExplainFailure> {
    match std::fs::metadata(root) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(ExplainFailure::ProjectUnreadable(
            "it is not a folder".into(),
        )),
        Err(e) => Err(ExplainFailure::ProjectUnreadable(first_line(
            &e.to_string(),
        ))),
    }
}

/// Each folder's listing (`explain::Inputs::listings`): a hash of everything
/// under it — the name of every file `listed`, source or not, and of every
/// folder that holds one, with that folder's own listing — so what is added
/// anywhere below a folder changes it. The whole subtree, not only its own
/// entries: a folder that holds no code has no summary, and the nearest one
/// above it that has one is what an automatic pass asks whether code was
/// added there (`explain::Reuse::ChangedSources`) — its own entries named
/// that folder whatever it held. Any file, not only the ones this build
/// explains, so a build that explains more files than the last does not
/// read as files added.
///
/// Persisted in each folder's summary (`explain::Basis::source`): a change
/// to what goes in, or how it is encoded, must bump
/// `explain::BASIS_RECIPE` and move `explain::LISTING_RECIPE` to it —
/// `folder_listings_are_frozen` fails until it is.
fn folder_listings(root: &Path, listed: &[PathBuf]) -> HashMap<PathBuf, incremental::Version> {
    use std::collections::BTreeMap;
    // Each folder's own entries by name, with the folder each is, if it is
    // one.
    let mut children: HashMap<&Path, BTreeMap<&[u8], Option<&Path>>> = HashMap::new();
    for file in listed {
        let mut child: &Path = file;
        let mut is_dir = false;
        while let Some(parent) = child.parent().filter(|p| p.starts_with(root)) {
            let Some(name) = child.file_name() else {
                break;
            };
            let entry = is_dir.then_some(child);
            let known = children
                .entry(parent)
                .or_default()
                .insert(name.as_encoded_bytes(), entry)
                .is_some();
            // A folder already listed has had the folders above it listed.
            if known && is_dir {
                break;
            }
            child = parent;
            is_dir = true;
        }
    }
    // The deepest first: a folder's listing takes in those of its folders.
    let mut dirs: Vec<&Path> = children.keys().copied().collect();
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir.components().count()));
    let mut listings: HashMap<PathBuf, incremental::Version> = HashMap::with_capacity(dirs.len());
    for dir in dirs {
        let mut bytes = Vec::new();
        for (name, folder) in &children[dir] {
            bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
            bytes.extend_from_slice(name);
            match folder {
                None => bytes.push(0),
                Some(folder) => {
                    bytes.push(1);
                    let listing = listings.get(*folder).copied().unwrap_or_default();
                    bytes.extend_from_slice(&listing.to_le_bytes());
                }
            }
        }
        listings.insert(dir.to_path_buf(), incremental::content_hash(&bytes));
    }
    listings
}

/// Most rels one `ReadSources` asks for. It bounds the request, and the work
/// one reply does; the reply's size is bounded by bytes on the host, which
/// names what it had no room for (see [`fetch_explain_sources`]).
const SOURCES_BATCH: usize = 128;

/// A REMOTE project's explainable sources, fetched from the host in bounded
/// batches. A rel the host reports missing is gone; one it reports too large
/// to explain, or refuses as no plain text file, cannot be explained. Any
/// other it does not send is unread, never gone — one it could not read, and
/// every rel of a batch that failed (the connection, a refusal, a reply of
/// another kind): a network error used to drop every summary of the files it
/// hid. What the tree lists (`listed`) is the caller's to fill in.
///
/// A reply carries at most so many bytes, and names the rels it had no room
/// for (`deferred`): those are asked for again, first. Sizes are known only
/// where the files are — the tree names them, it does not size them, and a
/// file can grow between the scan and the read — so it is the host that
/// pages. The files a reply had no room for were read as ones it could not
/// read, on every pass, and never explained. A reply that settled none of
/// its batch is not asked again: that host would be asked forever.
pub(crate) async fn fetch_explain_sources(
    ai: &AiClient,
    root: &Path,
    rels: &[String],
) -> ExplainSources {
    let mut sources = ExplainSources::default();
    let mut queue: std::collections::VecDeque<&str> = rels.iter().map(String::as_str).collect();
    while !queue.is_empty() {
        let batch: Vec<&str> = queue.drain(..queue.len().min(SOURCES_BATCH)).collect();
        let reply = ai
            .request(clew_protocol::Request::ReadSources {
                rels: batch.iter().map(|rel| rel.to_string()).collect(),
            })
            .await;
        // What the reply does not account for is unread.
        let mut unaccounted: HashSet<&str> = batch.iter().copied().collect();
        let mut again = Vec::new();
        if let Ok(clew_protocol::Event::Sources {
            files,
            missing,
            too_large,
            refused,
            deferred,
            ..
        }) = reply
        {
            for (rel, text) in files {
                // Only what was asked for.
                if !unaccounted.remove(rel.as_str()) {
                    continue;
                }
                let abs = root.join(&rel);
                if let Some(lang) = highlight::detect(&abs) {
                    sources.read.insert(abs, (text, lang));
                }
            }
            for rel in &missing {
                unaccounted.remove(rel.as_str());
            }
            let unexplainable = too_large
                .into_iter()
                .map(|(rel, size)| (rel, explain::Unexplainable::TooLarge(size)))
                .chain(
                    refused
                        .into_iter()
                        .map(|(rel, why)| (rel, explain::Unexplainable::Refused(why))),
                );
            for (rel, why) in unexplainable {
                if unaccounted.remove(rel.as_str()) {
                    sources.unexplainable.insert(root.join(rel), why);
                }
            }
            let later: Vec<&str> = deferred
                .iter()
                .filter_map(|rel| unaccounted.take(rel.as_str()))
                .collect();
            if later.len() < batch.len() {
                again = later;
            } else {
                unaccounted.extend(later);
            }
        }
        sources
            .unread
            .extend(unaccounted.into_iter().map(|rel| root.join(rel)));
        for rel in again.into_iter().rev() {
            queue.push_front(rel);
        }
    }
    sources
}

/// Read the project and assemble the explain engine's inputs: every function's
/// body + signature + call-graph callees, each file's functions + structure, and
/// the folder tree. `recorded` is the cache the pass starts from (see
/// [`gather_explain_inputs_from`]). `Err` when the project's folder could
/// not be read ([`read_explain_sources`]). LOCAL projects only; blocking, run
/// off the UI thread.
pub(crate) fn gather_explain_inputs(
    files: Vec<PathBuf>,
    root: PathBuf,
    recorded: &explain::Cache,
) -> Result<explain::Inputs, ExplainFailure> {
    let sources = read_explain_sources(files, &root)?;
    Ok(gather_explain_inputs_from(sources, root, recorded))
}

/// The lines of a located definition, when it has a body: `(first, end)` as
/// a 0-based start and an exclusive end, clamped to the file. A bodyless
/// declaration — a C prototype, an overload signature, an interface method —
/// has nothing to explain and yields `None`; the span is read off the syntax
/// tree, so it is never the NEXT function's body (brace counting from a
/// prototype ran on into whatever followed it).
fn body_span(item: &outline::Located, line_count: usize) -> Option<(usize, usize)> {
    let (first, last) = item.body?;
    let start = first.saturating_sub(1).min(line_count);
    let end = last.clamp(start, line_count);
    (end > start).then_some((start, end))
}

/// [`gather_explain_inputs`] over already-read contents — the pure half,
/// shared with the remote pass (whose sources arrive over the protocol; the
/// map's paths are identities, never read from this machine's disk).
///
/// Everything it builds lands in a prompt, whose hash is the cache key, so it
/// works in one canonical order: the map's iteration order changes from one
/// map to the next, and following it re-billed every folder and every
/// cross-file caller on every pass. Each file is parsed ONCE; its outline,
/// imports and call sites all come from that tree.
///
/// A file that could not be read stays in its folder and is named unread
/// (`explain::Inputs::unread`), and what `recorded` — the cache the pass
/// starts from — holds for it stands in for its functions in the call graph
/// ([`unread_stand_ins`]). A file that cannot be explained is no code of the
/// pass: it is only named (`explain::Inputs::unexplainable`).
pub(crate) fn gather_explain_inputs_from(
    sources: ExplainSources,
    root: PathBuf,
    recorded: &explain::Cache,
) -> explain::Inputs {
    use std::collections::BTreeSet;

    let ExplainSources {
        read,
        unread,
        unexplainable,
        listed,
    } = sources;
    let listings = folder_listings(&root, &listed);
    let mut sources: Vec<(PathBuf, String, &'static str)> = read
        .into_iter()
        .map(|(path, (text, lang))| (path, text, lang))
        .collect();
    sources.sort_by(|a, b| a.0.cmp(&b.0));
    let analyzed: Vec<(PathBuf, String, outline::Analysis)> = sources
        .into_iter()
        .filter_map(|(path, text, lang)| {
            let analysis = outline::analyze(&text, lang)?;
            Some((path, text, analysis))
        })
        .collect();

    // Call graph for callee edges (tree-sitter; same-file + unique-name scope),
    // built from the call sites already read off each file's tree.
    let mut all_defs: Vec<projectcalls::Def> = Vec::new();
    let mut file_calls: Vec<projectcalls::FileCalls> = Vec::new();
    for (path, _, analysis) in &analyzed {
        for item in &analysis.symbols {
            all_defs.push(projectcalls::Def {
                name: item.symbol.name.clone(),
                kind: item.symbol.kind.clone(),
                file: path.clone(),
                line: item.symbol.line,
            });
        }
        file_calls.push(projectcalls::FileCalls::new(
            path.clone(),
            analysis.lang,
            analysis.calls.clone(),
            &analysis.symbols,
        ));
    }
    let (stand_in_defs, stand_in_calls) = unread_stand_ins(&unread, recorded);
    all_defs.extend(stand_in_defs);
    file_calls.extend(stand_in_calls);
    let callable = projectcalls::ProjectCallGraph::callable(&all_defs);
    let calls =
        projectcalls::ProjectCallGraph::build_from_calls(callable, &file_calls, &HashMap::new());
    let callee_map = calls.callee_keys();

    let mut functions = Vec::new();
    let mut file_inputs = Vec::new();
    let mut folder_files: HashMap<PathBuf, BTreeSet<PathBuf>> = HashMap::new();
    let mut folder_subs: HashMap<PathBuf, BTreeSet<PathBuf>> = HashMap::new();
    let mut folders_seen: BTreeSet<PathBuf> = BTreeSet::new();
    // Folder tree: register a file's ancestor dirs up to the project root.
    let mut register = |f: &Path| {
        if let Some(parent) = f.parent().filter(|p| p.starts_with(&root)) {
            folder_files
                .entry(parent.to_path_buf())
                .or_default()
                .insert(f.to_path_buf());
        }
        let mut dir = f.parent();
        while let Some(d) = dir {
            if !d.starts_with(&root) {
                break;
            }
            folders_seen.insert(d.to_path_buf());
            if d == root {
                break;
            }
            if let Some(up) = d.parent() {
                folder_subs
                    .entry(up.to_path_buf())
                    .or_default()
                    .insert(d.to_path_buf());
            }
            dir = d.parent();
        }
    };

    for (f, content, analysis) in &analyzed {
        let lines: Vec<&str> = content.lines().collect();
        let mut fn_keys = Vec::new();
        let mut types = Vec::new();
        // Ordinals exactly as the outline view and the call graph number them
        // (`outline::fn_ordinals`), so a stored explanation is found again.
        let symbols: Vec<outline::Symbol> =
            analysis.symbols.iter().map(|l| l.symbol.clone()).collect();
        let ordinals = outline::fn_ordinals(&symbols);
        for (item, ordinal) in analysis.symbols.iter().zip(ordinals) {
            let s = &item.symbol;
            if !outline::is_callable(&s.kind) {
                types.push(format!("{} {}", s.kind, s.name));
                continue;
            }
            let Some((start, end)) = body_span(item, lines.len()) else {
                continue; // a declaration without a body: nothing to explain
            };
            // The body and signature are what the function is explained
            // from, hashed into the basis its summary keeps on disk
            // (`explain::Basis`): a change to how they are cut out reads as
            // every function's code changing, and must bump
            // `explain::BASIS_RECIPE` and move `explain::FUNCTION_RECIPE` to
            // it, so that it reads as unchecked instead —
            // `function_extraction_is_frozen` fails until it is.
            let body = lines[start..end].join("\n");
            let signature = lines
                .get(s.line.saturating_sub(1))
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            let key = (f.clone(), s.name.clone(), ordinal);
            let mut callees = callee_map.get(&key).cloned().unwrap_or_default();
            callees.sort();
            functions.push(explain::FnInput {
                file: f.clone(),
                name: s.name.clone(),
                ordinal,
                signature,
                body,
                callees,
            });
            fn_keys.push(key);
        }
        // As the code wrote them: extraction's encodings are for the
        // resolver, not the model.
        let imports: Vec<String> = analysis
            .imports
            .iter()
            .map(|r| clew_core::imports::statement_of(r, analysis.lang))
            .collect();
        let mut structure = String::new();
        if !types.is_empty() {
            structure.push_str(&format!("Types: {}\n", types.join(", ")));
        }
        if !imports.is_empty() {
            structure.push_str(&format!("Imports: {}", imports.join(", ")));
        }
        // A file with neither functions nor structure (a config file, a
        // stylesheet) gives the model only its path to describe: no node,
        // and it does not keep an otherwise empty folder alive.
        if fn_keys.is_empty() && structure.trim().is_empty() {
            continue;
        }
        file_inputs.push(explain::FileInput {
            path: f.clone(),
            functions: fn_keys,
            structure,
            // Persisted in the file's summary (`explain::Basis`), like the
            // bodies above: the text as read, and nothing a build derives.
            source_hash: incremental::content_hash(content.as_bytes()),
        });
        register(f);
    }
    // A file that could not be read stays where it is in the tree: its
    // folder quotes the summary the pass keeps for it, as before.
    for f in &unread {
        register(f);
    }

    let folders = folders_seen
        .into_iter()
        .map(|d| explain::FolderInput {
            files: folder_files
                .get(&d)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            subfolders: folder_subs
                .get(&d)
                .map(|s| s.iter().cloned().collect())
                .unwrap_or_default(),
            path: d,
        })
        .collect();

    explain::Inputs {
        root,
        functions,
        files: file_inputs,
        folders,
        unread,
        unexplainable,
        listings,
    }
}

/// Stand-ins, for the call graph, for the functions of the files a pass could
/// not read: each function `recorded` holds a summary of, numbered as it was
/// then. Resolved against them, the rest of the project's calls into those
/// files land where they did when the files were last read. Without them such
/// a call resolved to nothing — or to a same-name function elsewhere — and the
/// caller's prompt changed with it: paid for while the file could not be
/// read, and again once it could.
///
/// A same-name function numbered before a recorded one that has no summary
/// of its own is taken for what such a gap almost always is, a declaration
/// without a body (a C prototype): a call lands on one only when nothing
/// defines the name.
fn unread_stand_ins(
    unread: &HashSet<PathBuf>,
    recorded: &explain::Cache,
) -> (Vec<projectcalls::Def>, Vec<projectcalls::FileCalls>) {
    use std::collections::{BTreeMap, BTreeSet};

    // Per file, per name, the ordinals a summary was recorded for.
    let mut summarized: BTreeMap<&Path, BTreeMap<&str, BTreeSet<u32>>> = BTreeMap::new();
    for node in recorded.keys() {
        if let explain::Node::Function {
            file,
            name,
            ordinal,
        } = node
            && unread.contains(file)
        {
            summarized
                .entry(file)
                .or_default()
                .entry(name)
                .or_default()
                .insert(*ordinal);
        }
    }
    let mut defs = Vec::new();
    let mut calls = Vec::new();
    for (file, names) in summarized {
        // A line per function, in order: all a line does here is number a
        // file's same-name functions and tell its declarations apart.
        let mut line = 0;
        let mut declarations = HashSet::new();
        for (name, ordinals) in names {
            let Some(&last) = ordinals.last() else {
                continue;
            };
            for ordinal in 0..=last {
                line += 1;
                if !ordinals.contains(&ordinal) {
                    declarations.insert(line);
                }
                defs.push(projectcalls::Def {
                    name: name.to_string(),
                    kind: "function".to_string(),
                    file: file.to_path_buf(),
                    line,
                });
            }
        }
        // Declarations count only in a language with a call model, as for a
        // file that was read.
        if let Some(lang) = highlight::detect(file).and_then(highlight::Lang::from_key)
            && let Some(mut stand_in) =
                projectcalls::FileCalls::of(file.to_path_buf(), lang, Vec::new(), &[])
        {
            stand_in.declarations = declarations;
            calls.push(stand_in);
        }
    }
    (defs, calls)
}

/// Re-read one file and assemble the block-detail inputs for a single function
/// (see [`FnDetailInput`]). Callees are resolved against `summaries`, a
/// unique-name → summary map (ambiguous names are skipped). Runs fresh from disk
/// so it works even before a full Explain pass this session. Blocking; run off
/// the UI thread.
pub(crate) fn gather_fn_detail_input(
    root: &Path,
    file: PathBuf,
    name: &str,
    ordinal: u32,
    summaries: &HashMap<String, Option<String>>,
) -> Option<FnDetailInput> {
    // `file` comes out of the persisted explain cache, so no project rescan ever
    // sanitizes it: the bare `read_to_string` this replaces had no containment
    // check, no regular-file check and no cap at all, and shipped whatever it
    // read into the LLM prompt — and a FIFO sitting at one of those cached
    // paths would block whichever thread reads it.
    let content =
        clew_core::fs_scan::read_confined_capped(root, &file, index::MAX_INDEX_FILE_BYTES)?;
    gather_fn_detail_from(&file, &content, name, ordinal, summaries)
}

/// [`gather_fn_detail_input`] over already-read content — the pure half,
/// shared with the remote flow (whose source arrives over the protocol; the
/// path is a language hint and identity only). One parse: the outline (with
/// body spans) and the call sites come from the same tree.
pub(crate) fn gather_fn_detail_from(
    file: &Path,
    content: &str,
    name: &str,
    ordinal: u32,
    summaries: &HashMap<String, Option<String>>,
) -> Option<FnDetailInput> {
    let lang = highlight::detect(file)?;
    let analysis = outline::analyze(content, lang)?;
    let lines: Vec<&str> = content.lines().collect();
    let (item, body) = function_body(&analysis, &lines, name, ordinal)?;
    let signature = lines
        .get(item.symbol.line.saturating_sub(1))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    // Callees THIS function's body names (not those of another same-name
    // function in the file — a second `new` used to list the first's), with
    // their summaries for context.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut callees = Vec::new();
    for cs in analysis.calls_made_by(item) {
        if !seen.insert(cs.callee.as_str()) {
            continue;
        }
        if let Some(Some(sum)) = summaries.get(&cs.callee) {
            callees.push((cs.callee.clone(), sum.clone()));
        }
    }
    callees.sort();
    Some((signature, body, callees))
}

/// The `ordinal`th callable named `name` in `analysis` (of the file split into
/// `lines`), with its body's text. Located by its identity — the nth
/// same-name callable, numbered exactly as the explain inputs number it — and
/// the body span comes off the tree: for Dart, whose outline tags only the
/// signature, it runs through the `function_body` that follows it, so the
/// model is never sent a lone header ("the body is missing").
pub(crate) fn function_body<'a>(
    analysis: &'a outline::Analysis,
    lines: &[&str],
    name: &str,
    ordinal: u32,
) -> Option<(&'a outline::Located, String)> {
    let item = analysis.function(name, ordinal)?;
    let (start, end) = body_span(item, lines.len())?;
    Some((item, lines[start..end].join("\n")))
}

/// A short, readable name for a debug stack frame — the last path segment, with
/// a trailing mangling hash (`::h1a2b3…`) dropped. "main::factorial::hd89…" →
/// "factorial".
pub fn short_frame_name(name: &str) -> String {
    let parts: Vec<&str> = name.split("::").collect();
    let drop_hash = parts.last().is_some_and(|s| {
        s.len() > 3 && s.starts_with('h') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
    });
    let end = if drop_hash {
        parts.len() - 1
    } else {
        parts.len()
    };
    parts[..end].last().copied().unwrap_or(name).to_string()
}

/// Resolve a possibly-relative path from the launch config against the root.
///
/// This is a resolve, NOT a containment check, and the distinction is
/// load-bearing: `.clew/launch.json` ships with the repository, so an absolute
/// `program`/`cwd` is honoured verbatim and a hostile repo can name any binary
/// on this machine as the thing the debugger launches. That is code execution,
/// not file disclosure — the highest-consequence repository-text-to-action path
/// in the tree. The only things standing in front of it are the out-of-project
/// trust consent taken at open (`App::request_open`) and the user explicitly
/// starting a debug session, which is the same bargain VS Code's own
/// `launch.json` makes. Accepted rather than closed, because an absolute
/// `program` is how you point at a binary in a target directory outside the
/// project and rejecting it would break real configs. Stated here so no later
/// reader mistakes this function for a guard.
pub(crate) fn resolve_rel(root: &Path, p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() { pb } else { root.join(pb) }
}

/// Read `.clew/launch.json`, resolving relative paths against the project root.
/// A missing/invalid file yields a helpful message.
pub(crate) fn read_launch_config(root: &Path) -> Result<LaunchConfig, String> {
    let path = root.join(".clew").join("launch.json");
    let text = clew_core::statefile::read_capped(&path, 1024 * 1024).ok_or_else(|| {
        format!(
            "Create {} with {{\"program\": \"path\", \"type\": \"python\"}}",
            path.display()
        )
    })?;
    parse_launch_config(root, &text)
}

/// Parse a `launch.json`'s text against `root` — shared by the local read
/// above and the remote path (the file is fetched over the protocol; a
/// remote root must never be read from the local disk).
pub(crate) fn parse_launch_config(root: &Path, text: &str) -> Result<LaunchConfig, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("launch.json: {e}"))?;
    let program = v
        .get("program")
        .and_then(|p| p.as_str())
        .ok_or("launch.json needs a \"program\" field")?;
    let args = v
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let cwd = v
        .get("cwd")
        .and_then(|c| c.as_str())
        .map(|c| resolve_rel(root, c))
        .unwrap_or_else(|| root.to_path_buf());
    let type_hint = v.get("type").and_then(|t| t.as_str()).map(str::to_string);
    Ok(LaunchConfig {
        program: resolve_rel(root, program),
        args,
        cwd,
        type_hint,
    })
}

/// What one [`generate_svgs`] batch produced.
#[derive(Debug, Default, Clone)]
pub struct SvgBatch {
    /// Rendered and prepared, keyed by content hash (also cached on disk).
    pub rendered: HashMap<u64, richmd::PreparedSvg>,
    /// Items that could not be rendered. Each shows as its source text, and is
    /// remembered for the session (`ExplainState::svg_failed`) so the renderer
    /// is not run on it again; nothing is written to the disk cache, so a
    /// later session tries again.
    pub failed: Vec<SvgFailure>,
}

/// One math/mermaid block that could not be rendered.
#[derive(Debug, Clone)]
pub struct SvgFailure {
    pub key: u64,
    /// Why, for the status line: a parse failure or a renderer panic.
    pub reason: String,
}

/// Generate the missing math/mermaid SVGs off-thread, rendering each in-process
/// — RaTeX for math, `mermaid-rs-renderer` for diagrams — with no webview and no
/// helper binary. Each result is recolored/sized and its raw SVG cached on disk.
/// Blocking. (The module map is drawn on a native canvas, not mermaid, so the
/// diagrams reaching here are the smaller ones LLM explanations emit.)
///
/// Each item is rendered in isolation, under `render::render_guarded`: on a
/// thread of its own with a large stack and a deadline. The sources are model
/// output, and the renderers are libraries full of `unwrap`s: one panicking
/// diagram used to unwind the whole batch — every other diagram with it — and
/// the status still said "Rendered"; a runaway render held the batch, and
/// every diagram queued behind it, forever; and recursion deeper than the
/// caller's stack aborted the process. Now a panic or a timeout costs its own
/// item only, and every failure, a parse error included, comes back named in
/// [`SvgBatch::failed`].
pub(crate) fn generate_svgs(
    missing: Vec<richmd::Renderable>,
    // This project's derived-artifact store (clew's own data dir, keyed by
    // host+root). `None` when there is no data directory — the SVGs then live
    // in memory for the session.
    store: Option<PathBuf>,
) -> SvgBatch {
    generate_svgs_with(missing, store, |r| render::render_guarded(r.kind, &r.src))
}

/// [`generate_svgs`] with the (guarded) renderer supplied, so every way a
/// unit can end is testable.
fn generate_svgs_with(
    missing: Vec<richmd::Renderable>,
    store: Option<PathBuf>,
    render: impl Fn(&richmd::Renderable) -> render::Guarded<Option<String>>,
) -> SvgBatch {
    let mut batch = SvgBatch::default();
    for r in missing {
        let is_math = r.kind == "math";
        let reason = match render(&r) {
            render::Guarded::Done(Some(svg)) => {
                if let Some(store) = &store {
                    richmd::store_raw(store, r.key, &svg);
                }
                batch
                    .rendered
                    .insert(r.key, richmd::prepare_svg(&svg, is_math));
                continue;
            }
            render::Guarded::Done(None) => format!("could not parse this {}", noun(r.kind)),
            render::Guarded::Crashed(what) => {
                format!("the {} renderer crashed: {what}", noun(r.kind))
            }
            render::Guarded::TimedOut => format!(
                "the {} renderer did not finish within {}s and was given up on",
                noun(r.kind),
                render::RENDER_DEADLINE.as_secs()
            ),
        };
        batch.failed.push(SvgFailure { key: r.key, reason });
    }
    batch
}

fn noun(kind: &str) -> &'static str {
    if kind == "math" {
        "equation"
    } else {
        "diagram"
    }
}

/// A semantic-index build that produced no index, and the index it started
/// from — handed back UNTOUCHED, for the window to keep. The window takes its
/// index out for the build ([`build_embeddings`] moves the vectors it reuses),
/// and a build that failed used to leave it with none: the next build then
/// re-embedded — and re-billed — every node.
#[derive(Debug, Clone)]
pub struct EmbedBuildFailed {
    pub error: String,
    pub previous: Handoff<embed::Index>,
}

impl EmbedBuildFailed {
    pub fn new(error: impl Into<String>, previous: embed::Index) -> Self {
        EmbedBuildFailed {
            error: error.into(),
            previous: Handoff::new(previous),
        }
    }
}

/// (Re)build the embedding index: reuse a node's vector when its summary hash is
/// unchanged, embed the rest. Blocking — run off the UI thread.
///
/// The result has ONE vector length. A reused vector of a different length
/// than the freshly embedded ones — the endpoint changed its output size, or
/// the request stopped carrying `dimensions` for a model that never documented
/// it — is embedded again rather than kept: cosine across two lengths is 0,
/// which read as "no results".
///
/// `existing` is only READ until every embedding call has succeeded; a build
/// that fails hands it back as it came ([`EmbedBuildFailed`]).
pub(crate) async fn build_embeddings(
    ai: &AiClient,
    cfg: &embed::Config,
    nodes: Vec<(explain::Node, String, incremental::Version)>,
    existing: embed::Index,
) -> Result<embed::Index, EmbedBuildFailed> {
    // The plan, by index into `existing.entries`: what is reused (with the
    // text it embeds, in case it must be embedded again) and what is new.
    let mut reused: Vec<(usize, String)> = Vec::new();
    let mut pending: Vec<(explain::Node, incremental::Version)> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    {
        let mut have: HashMap<&explain::Node, usize> = existing
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (&e.node, i))
            .collect();
        for (node, text, hash) in nodes {
            match have.remove(&node) {
                Some(i) if existing.entries[i].hash == hash => reused.push((i, text)),
                _ => {
                    pending.push((node, hash));
                    texts.push(text);
                }
            }
        }
    }
    let vecs = match ai.embed(cfg.clone(), texts).await {
        Ok(vecs) => vecs,
        Err(e) => return Err(EmbedBuildFailed::new(e, existing)),
    };
    if vecs.len() != pending.len() {
        let error = format!("asked for {} embeddings, got {}", pending.len(), vecs.len());
        return Err(EmbedBuildFailed::new(error, existing));
    }
    // Reused vectors of another length than the fresh ones, by their place
    // among the reused, embedded again before anything is taken.
    let fresh_len = vecs.first().map(Vec::len);
    let stale: Vec<usize> = match fresh_len {
        Some(len) => (0..reused.len())
            .filter(|&k| existing.entries[reused[k].0].vec.len() != len)
            .collect(),
        None => Vec::new(),
    };
    let again = if stale.is_empty() {
        Vec::new()
    } else {
        let texts: Vec<String> = stale.iter().map(|&k| reused[k].1.clone()).collect();
        match ai.embed(cfg.clone(), texts).await {
            Ok(again)
                if again.len() == stale.len()
                    && again.iter().all(|v| Some(v.len()) == fresh_len) =>
            {
                again
            }
            Ok(_) => {
                let error = "the embedding endpoint changed its vector length mid-build";
                return Err(EmbedBuildFailed::new(error, existing));
            }
            Err(e) => return Err(EmbedBuildFailed::new(e, existing)),
        }
    };
    // Everything is paid for: only now are the reused entries moved out.
    let mut old: Vec<Option<embed::Entry>> = existing.entries.into_iter().map(Some).collect();
    let mut entries: Vec<embed::Entry> =
        reused.iter().filter_map(|&(i, _)| old[i].take()).collect();
    for ((node, hash), vec) in pending.into_iter().zip(vecs) {
        entries.push(embed::Entry { node, hash, vec });
    }
    for (k, vec) in stale.into_iter().zip(again) {
        entries[k].vec = vec;
    }
    Ok(embed::Index {
        model: cfg.model.clone(),
        // The space these vectors were actually produced in, so a later
        // `is_foreign` can see a repoint that left the model name alone.
        base_url: cfg.base_url.clone(),
        entries,
    })
}

/// Why an explain call failed, or a pass stopped short — told apart because
/// each asks something different of the user, and only some may pass
/// another time. It is said in the status line, never recorded as a
/// summary: a group whose call failed keeps what it had (`explain::Pass`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainFailure {
    /// The request was refused, and would be again as it stands: by the
    /// provider, with a 4xx status and its reason; by the settings before it
    /// went out (an endpoint that redirects, one that is not a URL); or by
    /// the clew-server that was to make it.
    Rejected { what: Rejection, detail: String },
    /// The provider could not serve it now: a 5xx, an overload or a rate
    /// limit (408, 429), or a failure it reported mid-answer.
    Unavailable(String),
    /// No answer came: the connection to the provider — or, for a remote
    /// project, to the clew-server that calls it — could not be made, broke,
    /// or went quiet.
    Network(String),
    /// The provider answered, and the answer could not be used: not what
    /// its API sends, or no text in it.
    Unusable(String),
    /// Stopped on request (a remote host stops its calls when its project
    /// is switched).
    Cancelled,
    /// clew failed — a step that panicked, a reply of the wrong kind: its
    /// own bug, not the provider's or the network's.
    Internal(String),
    /// The project's folder could not be read — not there (an unmounted
    /// volume, a folder renamed away) or not a folder — so nothing is known
    /// about any file in it: why a pass stops before it starts.
    ProjectUnreadable(String),
}

/// What an [`ExplainFailure::Rejected`] request was refused for, which says
/// what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The API key: a 401 or 403.
    Key,
    /// The prompt's length: over the model's context window, or a 413.
    Length,
    /// The model: the endpoint has none of that name (a 404 that says it is
    /// about the model) — the model named in Settings.
    Model(String),
    /// The endpoint: there is nothing at the address the request went to (a
    /// 404 that does not say it is about the model) — the base URL in
    /// Settings.
    Endpoint,
    /// Anything else about the request: another 4xx, the settings.
    Request,
}

impl ExplainFailure {
    /// Classify why a call to `model` failed, by what failed ([`CallError`]):
    /// a provider's status, the connection, a cancellation, clew itself.
    /// The status decides wherever there is one — a 429 whose message says
    /// "try again in 401ms" is a rate limit, not a rejected key — and only a
    /// refusal of the request (another 4xx) is told apart by what the
    /// provider says it refused. A call made here is read by the code a
    /// clew-server answers such a call with (`explain::chat_error_code`), so
    /// that the two read as one failure. There is no reading a server's
    /// words back: the handshake refuses a server of another protocol
    /// (`clew_protocol::SCHEMA_FINGERPRINT`), so every server this client
    /// talks to says what failed with its code.
    pub(crate) fn of_call(err: &CallError, model: &str) -> ExplainFailure {
        match err {
            CallError::Llm(e) => {
                ExplainFailure::of_code(&explain::chat_error_code(e), &e.to_string(), model)
            }
            // No answer came from the clew-server: its connection failed,
            // or it went quiet.
            CallError::Rpc(RpcError {
                code: None,
                message,
            }) => ExplainFailure::Network(first_line(message)),
            CallError::Rpc(RpcError {
                code: Some(code),
                message,
            }) => ExplainFailure::of_code(code, message, model),
            CallError::Internal(message) => ExplainFailure::Internal(first_line(message)),
        }
    }

    /// A call to `model` answered with `code`, and `message` for people.
    fn of_code(code: &clew_protocol::ErrorCode, message: &str, model: &str) -> ExplainFailure {
        use clew_protocol::{ErrorCode, ProviderFailure};
        let detail = first_line(message);
        match code {
            ErrorCode::Provider(failure) => match failure {
                ProviderFailure::Status {
                    code,
                    kind,
                    message,
                } => ExplainFailure::of_status(*code, kind.as_deref(), message, detail, model),
                // Never reached the provider (llm sent it again while
                // another attempt could get further), or reached it and
                // broke.
                ProviderFailure::Unreached | ProviderFailure::Broken => {
                    ExplainFailure::Network(detail)
                }
                ProviderFailure::Stream => ExplainFailure::Unavailable(detail),
                ProviderFailure::Unusable => ExplainFailure::Unusable(detail),
                ProviderFailure::Settings => ExplainFailure::Rejected {
                    what: Rejection::Request,
                    detail,
                },
            },
            ErrorCode::Cancelled => ExplainFailure::Cancelled,
            // Not attempted, and refused again if sent again: no provider
            // set up on the server, a connection that did not complete its
            // handshake.
            ErrorCode::Refused | ErrorCode::Handshake => ExplainFailure::Rejected {
                what: Rejection::Request,
                detail,
            },
            ErrorCode::NotReady => ExplainFailure::Unavailable(detail),
            // The server failed making the call — its own failure: the
            // provider's comes as `Provider`.
            ErrorCode::Failed => ExplainFailure::Internal(detail),
        }
    }

    /// A failure the provider answered with HTTP status `code`, its error
    /// `kind` and `message`: classified by the status, and a refusal of the
    /// request (another 4xx) by what the provider says it refused.
    fn of_status(
        code: u16,
        kind: Option<&str>,
        message: &str,
        detail: String,
        model: &str,
    ) -> ExplainFailure {
        let what = match code {
            408 | 429 | 500..=599 => return ExplainFailure::Unavailable(detail),
            401 | 403 => Rejection::Key,
            404 if is_about_the_model(kind, message, model) => Rejection::Model(model.to_string()),
            404 => Rejection::Endpoint,
            413 => Rejection::Length,
            _ if is_length_refusal(kind, message) => Rejection::Length,
            _ => Rejection::Request,
        };
        ExplainFailure::Rejected { what, detail }
    }

    /// What went wrong, and what to do about it: a clause of the status line.
    pub(crate) fn describe(&self) -> String {
        match self {
            ExplainFailure::Rejected {
                what: Rejection::Key,
                detail,
            } => format!("the provider rejected the API key ({detail}) — check it in Settings"),
            ExplainFailure::Rejected {
                what: Rejection::Length,
                detail,
            } => format!(
                "the prompt is too long for the model ({detail}) — a model with a larger \
                 context window can explain it"
            ),
            ExplainFailure::Rejected {
                what: Rejection::Model(model),
                detail,
            } => format!(
                "the provider has no model `{model}` at this endpoint ({detail}) — check the \
                 model and endpoint in Settings"
            ),
            ExplainFailure::Rejected {
                what: Rejection::Endpoint,
                detail,
            } => format!("the endpoint was not found ({detail}) — check the base URL in Settings"),
            ExplainFailure::Rejected {
                what: Rejection::Request,
                detail,
            } => format!(
                "the request was refused ({detail}) — check the model and endpoint in Settings"
            ),
            ExplainFailure::Unavailable(detail) => {
                format!("the provider was unavailable ({detail}) — try again later")
            }
            ExplainFailure::Network(detail) => {
                format!("the connection failed ({detail}) — check the network and try again")
            }
            ExplainFailure::Unusable(detail) => format!(
                "the provider's answer could not be used ({detail}) — check the model and \
                 endpoint in Settings"
            ),
            ExplainFailure::Cancelled => {
                "its calls were cancelled — run it again to finish".to_string()
            }
            ExplainFailure::Internal(detail) => format!(
                "clew failed internally ({detail}) — try again, and report it if it happens again"
            ),
            ExplainFailure::ProjectUnreadable(detail) => format!(
                "the project folder could not be read ({detail}) — nothing was changed; check \
                 that it is still there"
            ),
        }
    }

    /// Whether the call may succeed another time: the provider, the
    /// connection or clew failed this time. A refusal is refused again, an
    /// answer that could not be used is answered again, and a cancellation
    /// was asked for. Nothing is sent again within the pass for it but a
    /// gateway's 502 or 503 in an explicit pass (see [`explain_pass`]); a
    /// later pass retries the group.
    fn may_pass_later(&self) -> bool {
        matches!(
            self,
            ExplainFailure::Unavailable(_)
                | ExplainFailure::Network(_)
                | ExplainFailure::Internal(_)
        )
    }

    /// What a call that failed this way is to the pass: one that may
    /// succeed next time, or one that will fail the same way (see
    /// `explain::Pass`).
    fn to_pass(&self) -> explain::Failure {
        if self.may_pass_later() {
            explain::Failure::Transient
        } else {
            explain::Failure::Definitive
        }
    }

    /// Whether every later call of the pass would end the same way, so the
    /// pass stops at once instead of making them: a key the provider does
    /// not accept, a model it does not have, an endpoint that is not there,
    /// or calls it was asked to stop.
    fn stops_the_pass(&self) -> bool {
        matches!(
            self,
            ExplainFailure::Rejected {
                what: Rejection::Key | Rejection::Model(_) | Rejection::Endpoint,
                ..
            } | ExplainFailure::Cancelled
        )
    }

    /// Whether `other` is worded as this one is, whatever its detail.
    fn same_kind(&self, other: &ExplainFailure) -> bool {
        match (self, other) {
            (
                ExplainFailure::Rejected { what: a, .. },
                ExplainFailure::Rejected { what: b, .. },
            ) => a == b,
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// Whether a provider's 404 is about the model: its error kind says so, or
/// its words name `model`. Any other — a base URL whose path is wrong, a
/// gateway's page — is about the endpoint, and was said to be the model's.
fn is_about_the_model(kind: Option<&str>, message: &str, model: &str) -> bool {
    kind == Some("model_not_found") || (!model.is_empty() && message.contains(model))
}

/// Whether a provider's refusal of a request (a 4xx) is of its length: its
/// error kind says so, or its words — as the providers word it — do.
fn is_length_refusal(kind: Option<&str>, message: &str) -> bool {
    const KINDS: &[&str] = &[
        "context_length_exceeded",
        "exceed_context_size_error",
        "request_too_large",
    ];
    const WORDS: &[&str] = &[
        "context length",
        "context window",
        "maximum context",
        "exceeds the available context size",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "reduce the length",
        "request too large",
        "payload too large",
        "entity too large",
    ];
    let message = message.to_ascii_lowercase();
    kind.is_some_and(|kind| KINDS.contains(&kind)) || WORDS.iter().any(|w| message.contains(w))
}

/// An error's first line, trimmed and capped: what the status line quotes.
fn first_line(err: &str) -> String {
    let line = err.trim().lines().next().unwrap_or_default().trim();
    line.chars().take(160).collect()
}

/// How an explain pass fell short, if it did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassFailures {
    /// Groups the pass could not explain (`explain::Pass::failed`).
    pub failed: usize,
    /// The first of its failed calls of each kind, in the order they
    /// failed: what the status line says went wrong.
    pub calls: Vec<ExplainFailure>,
    /// Why it stopped before its end, if it did: a key the provider rejected,
    /// a model it does not have, its calls cancelled, or a failure of its
    /// own. It then holds only part of the project, so its caller merges it
    /// insert-only, never prunes by it.
    pub stopped: Option<ExplainFailure>,
}

impl PassFailures {
    /// Note a failed call, unless one of its kind is noted already.
    fn note(&mut self, failure: ExplainFailure) {
        if !self.calls.iter().any(|f| f.same_kind(&failure)) {
            self.calls.push(failure);
        }
    }
}

/// Prompts a model has refused for their size this session, by (model
/// identity, prompt hash). Without this, a prompt over a small local model's
/// window was sent again on every pass, and an auto-refresh runs a pass
/// within half a minute of each edit.
static REJECTED_FOR_SIZE: std::sync::LazyLock<
    std::sync::Mutex<HashSet<(String, incremental::Version)>>,
> = std::sync::LazyLock::new(Default::default);

fn remember_rejected(model: &str, hash: incremental::Version) {
    REJECTED_FOR_SIZE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((model.to_string(), hash));
}

fn was_rejected(model: &str, hash: incremental::Version) -> bool {
    REJECTED_FOR_SIZE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(model.to_string(), hash))
}

/// How one explain call ended: its summary, or why there is none.
type CallOutcome = Result<String, CallFailed>;

/// Why one explain call failed: what the status line says of it, and
/// whether it was a gateway's 502 or 503, which an explicit pass sends
/// again (see [`explain_pass`]).
struct CallFailed {
    why: ExplainFailure,
    gateway: bool,
}

/// How long after a gateway's 502 or 503 an explicit pass sends the call
/// again (see [`explain_pass`]): time for a gateway that lost one answer
/// from upstream, or was overloaded for a moment, to be past it.
const GATEWAY_RESEND_DELAY: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(100)
} else {
    std::time::Duration::from_secs(2)
};

/// LLM calls in flight at once.
const EXPLAIN_CONCURRENCY: usize = 12;

/// The model a pass's calls go to.
#[derive(Debug, Clone)]
pub(crate) struct CallModel {
    /// Its name, as Settings give it: how a failure names it.
    pub(crate) name: String,
    /// The endpoint that serves it, with its name: what the prompts it
    /// refused for their length are remembered by.
    pub(crate) key: String,
}

impl CallModel {
    /// The model `cfg` names, at the endpoint it names.
    pub(crate) fn of(cfg: &llm::Config) -> CallModel {
        CallModel {
            name: cfg.model.clone(),
            key: format!("{}|{}", cfg.base_url, cfg.model),
        }
    }
}

/// Make one explain call — once. llm's send is the one retry policy
/// (`send_with_retry`): it resends a request that never reached the
/// provider and one the provider asked to have sent again, after the wait it
/// asked for, and gives up on the rest. Resent here, a request the provider
/// had received — the connection broke while it answered, a gateway's 504,
/// a clew-server that went quiet for ten minutes — was generated and billed
/// again, up to three times over; what llm had given up on, it retried on
/// top of its own retries. A group whose call failed is retried by a later
/// pass (`explain::Tally::retry`) — and one a gateway failed with a 502 or
/// 503, by an explicit pass, once, at the end of its level ([`CallFailed`]).
async fn explain_one<C, F>(complete: C, model: &CallModel, job: &explain::Job) -> CallOutcome
where
    C: Fn(String) -> F,
    F: std::future::Future<Output = Result<String, CallError>>,
{
    // Refused for its size earlier this session: it would be refused again.
    if was_rejected(&model.key, job.hash) {
        return Err(CallFailed {
            why: ExplainFailure::Rejected {
                what: Rejection::Length,
                detail: "refused for its length earlier in this session".into(),
            },
            gateway: false,
        });
    }
    let error = match complete(job.prompt.clone()).await {
        Ok(summary) => return Ok(summary),
        Err(error) => error,
    };
    let why = ExplainFailure::of_call(&error, &model.name);
    if matches!(
        why,
        ExplainFailure::Rejected {
            what: Rejection::Length,
            ..
        }
    ) {
        remember_rejected(&model.key, job.hash);
    }
    Err(CallFailed {
        why,
        gateway: matches!(error.status(), Some(502 | 503)),
    })
}

/// What an explain pass produced.
pub(crate) struct ExplainOutcome {
    pub cache: explain::Cache,
    /// What it could not explain, and why ([`PassFailures`]).
    pub failures: PassFailures,
    /// What the pass paid for, kept and left (`explain::Pass::tally`).
    pub tally: explain::Tally,
    /// The entries it changed (`explain::Pass::written`).
    pub written: explain::Unsaved,
}

impl ExplainOutcome {
    /// A pass that broke before it could finish (`what` it was doing): it
    /// holds nothing, so its caller changes nothing by it (`stopped`).
    fn broke(what: &str) -> ExplainOutcome {
        ExplainOutcome::stopped(ExplainFailure::Internal(format!("while {what}")))
    }

    /// A pass that stopped before it started, for `why`: it holds nothing,
    /// so its caller changes nothing by it.
    fn stopped(why: ExplainFailure) -> ExplainOutcome {
        ExplainOutcome {
            cache: explain::Cache::new(),
            failures: PassFailures {
                stopped: Some(why),
                ..PassFailures::default()
            },
            tally: explain::Tally::default(),
            written: explain::Unsaved::new(),
        }
    }
}

/// Run one explain pass: [`explain::Pass`] schedules and renders (on the
/// blocking pool — both are CPU-bound over the whole project), `complete`
/// makes the LLM calls, each dependency level concurrently, and progress
/// streams to `output`.
///
/// Reuse, hashing and recording all happen in `explain::Pass`; this is only
/// the driver, generic over `complete` so the tests run the production pass
/// against a mock.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn explain_pass<C, F>(
    output: &mut iced::futures::channel::mpsc::Sender<Message>,
    inputs: explain::Inputs,
    prev: explain::Cache,
    // Which recorded summaries the pass may keep (an automatic pass keeps
    // what nothing changed under).
    reuse: explain::Reuse,
    // The groups the last pass left waiting, which do not wait again
    // (`explain::Pass::waited_before`).
    waited: HashSet<explain::Node>,
    // The project instance the pass runs for; its progress carries it.
    stamp: Stamp,
    generation: u64,
    // The model the calls go to: what a failure names, and what keys the
    // too-large memory.
    model: CallModel,
    complete: C,
) -> ExplainOutcome
where
    C: Fn(String) -> F + Clone,
    F: std::future::Future<Output = Result<String, CallError>>,
{
    use iced::futures::{SinkExt, StreamExt};
    use std::sync::{Arc, Mutex};

    // Poisoning only means a render panicked inside the blocking pool; the
    // summaries the pass holds are still exactly the ones recorded.
    fn lock(pass: &Mutex<explain::Pass>) -> std::sync::MutexGuard<'_, explain::Pass> {
        pass.lock().unwrap_or_else(|e| e.into_inner())
    }

    // A failed call: its group keeps what it had, and the failure is said,
    // or stops the pass.
    fn fail(
        pass: &mut explain::Pass,
        failures: &mut PassFailures,
        job: &explain::Job,
        why: ExplainFailure,
    ) {
        pass.fail(job, why.to_pass());
        if why.stops_the_pass() {
            failures.stopped.get_or_insert(why);
        } else {
            failures.note(why);
        }
    }

    let explicit = matches!(reuse, explain::Reuse::SamePrompt);
    let Ok(pass) = tokio::task::spawn_blocking(move || {
        let mut pass = explain::Pass::with_reuse(inputs, prev, reuse);
        pass.waited_before(waited);
        pass
    })
    .await
    else {
        return ExplainOutcome::broke("scheduling the pass");
    };
    let total = pass.total();
    let pass = Arc::new(Mutex::new(pass));
    let mut failures = PassFailures::default();

    // Show a determinate 0/total at once so the bar appears immediately, then
    // update as items land (see the throttle below).
    let _ = output
        .send(Message::Explain(ExplainMsg::Progress {
            stamp: stamp.clone(),
            generation,
            done: 0,
            total,
            failed: 0,
        }))
        .await;
    // Coalesce progress emits to ~10/s. Each emit drives a full UI re-render;
    // firing one after every item on a big repo (thousands of functions) floods
    // the iced event loop and starves it, so interactive LSP requests (hover,
    // go-to-def) queue behind the re-renders and lag to several seconds. When
    // items land slower than the interval every one still emits, so small/medium
    // passes keep their per-item smoothness.
    let mut last_emit = std::time::Instant::now();
    const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    loop {
        // Render the next level's prompts off the async runtime.
        let level = {
            let pass = pass.clone();
            tokio::task::spawn_blocking(move || lock(&pass).next_level()).await
        };
        let jobs = match level {
            Ok(Some(jobs)) => jobs,
            Ok(None) => break,
            Err(_) => {
                let failure = ExplainFailure::Internal("while rendering prompts".into());
                failures.stopped = Some(failure);
                break;
            }
        };

        // Run the level's LLM calls concurrently, folding each result in as it
        // lands so progress advances smoothly. A failure records nothing —
        // its group keeps what it had — and is said in the status line, never
        // written to the cache.
        //
        // In an explicit pass, a call a gateway failed with a 502 or 503 is
        // sent again, once: at the end of its level, a moment after it
        // failed, and so before anything that quotes it renders its prompt.
        // Left failed, it had its caller, its file and every folder up to the
        // root paid for without it, then paid for again when a later pass
        // explained it — for a blip the pass's own retries used to ride out.
        // Having those wait for it instead, as an automatic pass does
        // (`explain::Pass`), would leave them unexplained by the pass that
        // was asked to explain everything, until some later one. Never sent
        // again: a 504, a connection that broke, a clew-server that went
        // quiet — the provider may be answering those.
        let mut round: Vec<Arc<explain::Job>> = jobs.into_iter().map(Arc::new).collect();
        let mut resend = explicit;
        while !round.is_empty() {
            let mut again: Vec<(Arc<explain::Job>, ExplainFailure)> = Vec::new();
            let mut gateway_failed = None;
            // The groups whose call ended, and was folded in.
            let mut ran: HashSet<usize> = HashSet::new();
            let mut calls = iced::futures::stream::iter(round.clone().into_iter().map(|job| {
                let complete = complete.clone();
                let model = model.clone();
                async move {
                    let outcome = explain_one(complete, &model, &job).await;
                    (job, outcome)
                }
            }))
            .buffer_unordered(EXPLAIN_CONCURRENCY);
            while let Some((job, outcome)) = calls.next().await {
                // Settled counts reused and skipped groups too, so the bar
                // reaches the total.
                let (done, failed) = {
                    let mut pass = lock(&pass);
                    match outcome {
                        Ok(summary) => {
                            ran.insert(job.group);
                            pass.complete(&job, summary);
                        }
                        Err(call) if resend && call.gateway => {
                            gateway_failed = Some(std::time::Instant::now());
                            again.push((job, call.why));
                        }
                        Err(call) => {
                            ran.insert(job.group);
                            fail(&mut pass, &mut failures, &job, call.why);
                        }
                    }
                    (pass.settled(), pass.failed())
                };
                // Throttled: emit only once the interval has passed since the
                // last update (or on the very last item), so the UI thread
                // stays free to service interactive LSP requests during a
                // long pass.
                if last_emit.elapsed() >= PROGRESS_INTERVAL || done == total {
                    last_emit = std::time::Instant::now();
                    let _ = output
                        .send(Message::Explain(ExplainMsg::Progress {
                            stamp: stamp.clone(),
                            generation,
                            done,
                            total,
                            failed,
                        }))
                        .await;
                }
                // A rejected key, a model the provider does not have or an
                // endpoint that is not there fails every call the same way,
                // and cancelled calls were asked to stop: the pass stops at
                // once — the calls in flight are cancelled with `calls` — and
                // says why.
                if failures.stopped.is_some() {
                    break;
                }
            }
            drop(calls);
            if failures.stopped.is_some() {
                let mut pass = lock(&pass);
                for (job, why) in again {
                    ran.insert(job.group);
                    fail(&mut pass, &mut failures, &job, why);
                }
                // Not made, or cancelled in flight: handed back, keeping
                // what they had, for a later pass.
                for job in round.iter().filter(|job| !ran.contains(&job.group)) {
                    pass.hand_back(job);
                }
                break;
            }
            if let Some(at) = gateway_failed {
                tokio::time::sleep(GATEWAY_RESEND_DELAY.saturating_sub(at.elapsed())).await;
            }
            round = again.into_iter().map(|(job, _)| job).collect();
            resend = false;
        }
        if failures.stopped.is_some() {
            break;
        }
    }

    // What it changed is found by walking the project: off the runtime too.
    let written = {
        let pass = pass.clone();
        tokio::task::spawn_blocking(move || lock(&pass).written()).await
    };
    let pass = std::mem::replace(
        &mut *lock(&pass),
        explain::Pass::new(explain::Inputs::default(), explain::Cache::new()),
    );
    explain_outcome(pass, written.ok(), failures)
}

/// What a pass that ran produced: what it holds, its tally, what it changed,
/// and how it fell short (`failures`, its count of failed groups filled in
/// here). When what it changed could not be listed (`written` is `None`) the
/// pass counts as stopped, so its caller never prunes by it, and it saves
/// only what the pass added where the window knew of nothing
/// (`explain::Pass::inserted`) — never over a summary the window had, which
/// an entry marked new (`None`) would be written over.
fn explain_outcome(
    pass: explain::Pass,
    written: Option<explain::Unsaved>,
    mut failures: PassFailures,
) -> ExplainOutcome {
    let tally = pass.tally();
    let written = written.unwrap_or_else(|| {
        failures.stopped.get_or_insert_with(|| {
            ExplainFailure::Internal("while listing what the pass changed".into())
        });
        pass.inserted()
    });
    let (cache, failed) = pass.finish();
    failures.failed = failed;
    ExplainOutcome {
        cache,
        failures,
        tally,
        written,
    }
}

/// Background explain pass: schedule bottom-up, run each dependency level
/// concurrently (reusing `prev` where the prompt is unchanged, else calling the
/// LLM), streaming progress and the finished cache. `gathered` is the pass's
/// inputs and its `prev`, or `None` when the project could not be read at all.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn explain_stream(
    mut output: iced::futures::channel::mpsc::Sender<Message>,
    gathered: Result<(explain::Inputs, explain::Cache), ExplainFailure>,
    reuse: explain::Reuse,
    waited: HashSet<explain::Node>,
    cfg: llm::Config,
    ai: AiClient,
    stamp: Stamp,
    generation: u64,
) {
    use iced::futures::SinkExt;

    let model = CallModel::of(&cfg);
    let complete = move |prompt: String| {
        let ai = ai.clone();
        let cfg = cfg.clone();
        async move { ai.complete_typed(cfg, EXPLAIN_SYSTEM, prompt, 400).await }
    };
    let outcome = match gathered {
        Ok((inputs, prev)) => {
            explain_pass(
                &mut output,
                inputs,
                prev,
                reuse,
                waited,
                stamp.clone(),
                generation,
                model,
                complete,
            )
            .await
        }
        // The project could not be read — its folder is not there, or the
        // task reading it died — so nothing is known about any file: the pass
        // stops and changes nothing. Run over no inputs, it dropped every
        // summary, as if every file had been deleted.
        Err(why) => ExplainOutcome::stopped(why),
    };
    let _ = output
        .send(Message::Explain(ExplainMsg::Done {
            stamp,
            generation,
            cache: outcome.cache,
            failures: outcome.failures,
            tally: Box::new(outcome.tally),
            written: outcome.written,
        }))
        .await;
}

/// A compact, single-line form of a server error for the status bar: the first
/// line, trimmed of any trailing detail after a colon, capped in length.
pub(crate) fn lsp_error_summary(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e).trim();
    // rust-analyzer's message reads "…failed to load workspace: <long detail>";
    // keep the human part before the first colon so the chip stays short.
    let head = first
        .split_once(':')
        .map(|(h, _)| h)
        .unwrap_or(first)
        .trim();
    let head = if head.is_empty() { first } else { head };
    if head.chars().count() > 64 {
        format!("{}…", head.chars().take(64).collect::<String>())
    } else {
        head.to_string()
    }
}

/// Find the first documented item named `name` anywhere in the index, returning
/// its (file rel, definition line). Used by "View docs".
pub(crate) fn find_doc_by_name(
    files: &[clew_protocol::DocFile],
    name: &str,
) -> Option<(String, usize)> {
    fn search(items: &[clew_protocol::DocItem], name: &str) -> Option<usize> {
        for it in items {
            if it.name == name {
                return Some(it.line);
            }
            if let Some(line) = search(&it.children, name) {
                return Some(line);
            }
        }
        None
    }
    for f in files {
        if let Some(line) = search(&f.items, name) {
            return Some((f.rel.clone(), line));
        }
    }
    None
}

/// Find the doc item defined at `line`, searching nested members.
pub(crate) fn find_doc_item(
    items: &[clew_protocol::DocItem],
    line: usize,
) -> Option<&clew_protocol::DocItem> {
    for it in items {
        if it.line == line {
            return Some(it);
        }
        if let Some(found) = find_doc_item(&it.children, line) {
            return Some(found);
        }
    }
    None
}

/// Flatten an item and its members into page entries (depth-tagged for
/// indentation), parsing each doc comment to markdown. Members are included
/// only when public, unless `show_all`.
pub(crate) fn flatten_doc(
    item: &clew_protocol::DocItem,
    depth: usize,
    show_all: bool,
    out: &mut Vec<DocEntryView>,
) {
    out.push(DocEntryView {
        name: item.name.clone(),
        kind: item.kind.clone(),
        signature: item.signature.clone(),
        line: item.line,
        depth,
        doc_items: iced::widget::markdown::parse(&item.doc).collect(),
    });
    for c in &item.children {
        if show_all || c.public {
            flatten_doc(c, depth + 1, show_all, out);
        }
    }
}

/// How many `ProcessOutput` chunks (each at most
/// [`clew_protocol::MAX_PROCESS_CHUNK`]) may wait for a proxied process's
/// reader. The UI thread feeds them and must not block, so a full feed means
/// the reader stopped draining: the process is stopped rather than its output
/// buffered without bound — or, worse, dropped mid-frame, which would corrupt
/// the LSP/DAP stream (see the `ProcessOutput` arm of `handle_server_event`).
pub(crate) const PROC_FEED_QUEUE: usize = 256;

/// Where a proxied process's `ProcessOutput` bytes are fed (bounded: see
/// [`PROC_FEED_QUEUE`]) — and the app's hold on the process: once every copy
/// of its feed is dropped, its bridge stops it ([`proxy_streams`]).
#[derive(Debug, Clone)]
pub struct ProcFeed {
    output: tokio::sync::mpsc::Sender<Vec<u8>>,
    /// Never read: the bridge waits for the last copy of it to go.
    _held: tokio::sync::watch::Receiver<()>,
    /// Closed as the bridge ends: once it has sent the process its kill, or
    /// its transport is gone ([`Self::killed`]).
    bridge: tokio::sync::watch::Receiver<()>,
}

impl ProcFeed {
    /// Queue `data` for the process's reader, without waiting.
    pub fn try_send(
        &self,
        data: Vec<u8>,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Vec<u8>>> {
        self.output.try_send(data)
    }

    /// Resolves once the bridge has sent the process its kill — or ended
    /// without, its transport gone. For what replaces the process, which is
    /// to start only after the process was told to stop; dropping every copy
    /// of the feed is what has the bridge send that kill.
    pub(crate) fn killed(&self) -> impl std::future::Future<Output = ()> + Send + use<> {
        let mut bridge = self.bridge.clone();
        async move { while bridge.changed().await.is_ok() {} }
    }

    /// A feed into `output` with no bridge behind it.
    #[cfg(test)]
    pub(crate) fn unbridged(output: tokio::sync::mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            output,
            _held: tokio::sync::watch::channel(()).1,
            bridge: tokio::sync::watch::channel(()).1,
        }
    }
}

/// Set up a proxied-process transport: ask clew-server (via `tx`) to spawn `cmd`
/// and bridge its stdio to two in-memory streams. Returns the caller's (stdin,
/// stdout) ends plus the feed the caller registers so `ProcessOutput` events for
/// `proc` reach the stdout bridge — dropping it stops the process (see
/// [`proxy_streams`]). Shared by the LSP and DAP proxies. Every
/// request it sends takes a fresh id from `next_id`, the window's request
/// counter.
pub(crate) fn proxy_transport(
    tx: &server::RequestTx,
    next_id: &Arc<std::sync::atomic::AtomicU64>,
    proc: u64,
    spawn: clew_protocol::Request,
) -> (tokio::io::DuplexStream, tokio::io::DuplexStream, ProcFeed) {
    // `spawn` is SpawnProcess (client-resolved, e.g. a debug adapter) or SpawnLsp
    // (server-resolved, so a remote runs its own language server).
    send_request(tx, next_id, spawn);
    proxy_streams(tx, next_id, proc)
}

/// [`proxy_transport`] without sending a spawn request — for callers that
/// send their own, correlated one (e.g. `SpawnAdapter`, whose reply carries
/// the launch config) and only need the stdio bridge here.
pub(crate) fn proxy_streams(
    tx: &server::RequestTx,
    next_id: &Arc<std::sync::atomic::AtomicU64>,
    proc: u64,
) -> (tokio::io::DuplexStream, tokio::io::DuplexStream, ProcFeed) {
    bridge_streams(tx, next_id, proc, None)
}

/// A spawn a bridge sends before anything else ([`proxy_streams_spawning`]),
/// and what it waits for first, if anything.
type Prelude = (
    clew_protocol::Request,
    Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
);

/// [`proxy_streams`], its bridge sending `spawn` before it forwards anything
/// — once `after` is done, when given: the kill of the process this one
/// replaces going out (`ProcFeed::killed`, a restarted language server's).
///
/// So everything the bridge sends for the process goes out behind that kill,
/// and the process's own kill, which the bridge sends last, behind its own
/// spawn — however soon the process is let go. The spawn used to be sent by
/// whoever asked for the process, apart from its bridge: a second restart
/// before the first one's spawn was out had the first replacement's kill go
/// out ahead of its spawn, the server did nothing with a kill for a process
/// it did not know, then spawned that process, and it ran on, untracked. A
/// process let go while its bridge still waited to spawn it is not spawned
/// at all, and has no kill to send.
pub(crate) fn proxy_streams_spawning(
    tx: &server::RequestTx,
    next_id: &Arc<std::sync::atomic::AtomicU64>,
    proc: u64,
    spawn: clew_protocol::Request,
    after: Option<impl std::future::Future<Output = ()> + Send + 'static>,
) -> (tokio::io::DuplexStream, tokio::io::DuplexStream, ProcFeed) {
    let after = after.map(|after| {
        Box::pin(after) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
    });
    bridge_streams(tx, next_id, proc, Some((spawn, after)))
}

/// [`proxy_streams`] and [`proxy_streams_spawning`]: the bridge, sending
/// `prelude`'s spawn first when there is one.
fn bridge_streams(
    tx: &server::RequestTx,
    next_id: &Arc<std::sync::atomic::AtomicU64>,
    proc: u64,
    prelude: Option<Prelude>,
) -> (tokio::io::DuplexStream, tokio::io::DuplexStream, ProcFeed) {
    let (client_stdin, mut stdin_reader) = tokio::io::duplex(64 * 1024);
    let (mut stdout_writer, client_stdout) = tokio::io::duplex(64 * 1024);
    let (feed_tx, mut feed_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(PROC_FEED_QUEUE);
    let (held, held_rx) = tokio::sync::watch::channel(());
    // Forward what the client writes → the process's stdin, one
    // `ProcessInput` per read. A read is at most the protocol's chunk cap, so
    // a large LSP/DAP message crosses as several small frames instead of one
    // that holds the whole stream (and that the server would refuse). Off the
    // UI thread, so a full request queue is waited out (backpressure) rather
    // than refused: a chunk dropped here would corrupt the stream.
    //
    // When the client's end closes — its LSP/DAP client stopped or was
    // dropped (a `stop()`, a slot reset, a finished debug session) — nothing
    // will talk to the process again, so it is stopped where it runs: a
    // `ProcessKill`, behind whatever input is still queued (a `shutdown` and
    // `exit` included; the server's writer drains its queue before the
    // process's stdin closes). Only a restart or a disconnect stopped it
    // before, so a stopped proxied server ran on idle until then. A kill for
    // a process the server already retired is a no-op.
    //
    // So too once the app lets go of the process's feed, its output routed
    // nowhere any more: its reader stopped draining it, or its language
    // server was replaced. A client that stopped reading may not close its
    // end for a long while, so the bridge does not wait for it. Whichever
    // comes first ends the bridge: this is the one kill the process gets —
    // the app used to send its own as well, which made two. What replaces the
    // process waits for the bridge to end (`ProcFeed::killed`), so it is
    // spawned behind that kill.
    let tx_in = tx.clone();
    let ids = next_id.clone();
    let (bridge, bridge_rx) = tokio::sync::watch::channel(());
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // Dropped as the bridge ends, which is what `ProcFeed::killed` waits
        // for.
        let _bridge = bridge;
        if let Some((spawn, after)) = prelude {
            if let Some(after) = after {
                after.await;
            }
            // Let go while it waited: nothing would read it, so it is not
            // started — and there is nothing to kill.
            if held.is_closed() || !send_request_async(&tx_in, &ids, spawn).await {
                return;
            }
        }
        let mut buf = vec![0u8; clew_protocol::MAX_PROCESS_CHUNK];
        loop {
            // Input first, so what the client already wrote still goes.
            let read = tokio::select! {
                biased;
                read = stdin_reader.read(&mut buf) => read,
                () = held.closed() => Ok(0),
            };
            match read {
                Ok(0) | Err(_) => {
                    let kill = clew_protocol::Request::ProcessKill { proc };
                    let _ = send_request_async(&tx_in, &ids, kill).await;
                    break;
                }
                Ok(n) => {
                    let input = clew_protocol::Request::ProcessInput {
                        proc,
                        data: buf[..n].to_vec(),
                    };
                    if !send_request_async(&tx_in, &ids, input).await {
                        break;
                    }
                }
            }
        }
    });
    // Pump the process's stdout (fed by ProcessOutput events) → the client.
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(data) = feed_rx.recv().await {
            if stdout_writer.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    let feed = ProcFeed {
        output: feed_tx,
        _held: held_rx,
        bridge: bridge_rx,
    };
    (client_stdin, client_stdout, feed)
}

/// Native picker for a project folder (Open Folder), in a sheet on `window`.
pub(crate) fn pick_folder(window: Option<iced::window::Id>) -> Task<Option<PathBuf>> {
    picker(window, |dialog| {
        dialog.set_title("Open a project folder").pick_folder()
    })
}

/// Native picker for an SSH private-key file (the Connect form's "Browse…"),
/// in a sheet on `window`.
pub(crate) fn pick_file(window: Option<iced::window::Id>) -> Task<Option<PathBuf>> {
    picker(window, |dialog| {
        dialog.set_title("Choose an SSH private key").pick_file()
    })
}

/// A file picker `pick` shows, in a sheet on `window` — the window it was
/// opened from — and the path picked.
///
/// Begun from the window's own callback, on the main thread, with the
/// window as its parent. Without one, rfd made it a sheet on whichever
/// window AppKit calls main: maybe another of clew's, whose close then took
/// the picker down with it, and whose questions queued behind it. A window
/// gone before the callback ran — or none, which no window of clew's is —
/// gets no picker, which picks nothing.
fn picker<F>(
    window: Option<iced::window::Id>,
    pick: impl FnOnce(rfd::AsyncFileDialog) -> F + Send + 'static,
) -> Task<Option<PathBuf>>
where
    F: std::future::Future<Output = Option<rfd::FileHandle>> + Send + 'static,
{
    let Some(window) = window else {
        return Task::done(None);
    };
    iced::window::run(window, move |parent| {
        pick(rfd::AsyncFileDialog::new().set_parent(parent))
    })
    .collect()
    .then(|shown| match shown.into_iter().next() {
        Some(picked) => {
            Task::future(picked).map(|picked| picked.map(|handle| handle.path().to_path_buf()))
        }
        None => Task::done(None),
    })
}

pub(crate) async fn load_file(
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

pub(crate) fn read_text_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    // ONE open, then the size and the bytes from that handle. Reading the
    // whole file and checking its length afterwards — what this used to do —
    // pulled every byte into memory before rejecting it, so the cap bounded
    // the error message and nothing else.
    //
    // `open_plain` rather than `File::open`, matching the server's `ReadFile`
    // (clew-server/src/lib.rs) that normally serves this pane: a plain
    // `File::open` BLOCKS on a FIFO before any check on the handle can run, so
    // the `is_file` test that used to follow it could never have caught one —
    // it wedged a blocking worker for the life of the process. `O_NOFOLLOW`
    // also makes this fallback refuse the same symlinked leaf the server
    // refuses, instead of the two paths disagreeing about what the project
    // contains. `open_plain` fstats the open handle for a regular file itself,
    // so no separate type check is needed here — and it reports no reason, so
    // the message below has to name every case it folds together (this
    // replaces the OS error text a missing file used to produce).
    let file = clew_core::statefile::open_plain(path)
        .ok_or_else(|| "cannot read: missing, a symlink, or not a regular file".to_string())?;
    let too_large = |n: u64| {
        format!(
            "file too large ({:.1} MB, limit {} MB)",
            n as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        )
    };
    // fstat on the handle we will read, not on the name.
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(too_large(meta.len()));
    }
    // Read through the cap as well: the size above is a cheap early
    // rejection, but a file can grow between the fstat and the read.
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(too_large(bytes.len() as u64));
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::test_dir;

    /// Everything the local Explain gatherers read is quoted verbatim into an
    /// LLM prompt and sent off this machine, so both of them go through the
    /// project-confined, capped read. A symlink planted in the project used to
    /// be size-checked on its target and then read by name, exfiltrating a file
    /// from outside the root; the detail gatherer, whose paths come from the
    /// persisted cache, had no containment check and no cap whatsoever.
    #[test]
    fn explain_gatherers_refuse_a_symlink_out_of_the_project_and_an_over_cap_file() {
        let dir = test_dir("explain-confine");
        let secret_dir = test_dir("explain-confine-secret");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();

        // Control: an ordinary project file is still gathered, so a refusal
        // below means the guard fired and not that the whole pass broke.
        let ok = dir.join("ok.rs");
        std::fs::write(&ok, "fn ok_fn() {\n    let _ = 1;\n}\n").unwrap();

        // A symlink inside the project pointing at a file outside it.
        let secret = secret_dir.join("secret.rs");
        std::fs::write(&secret, "fn leaked_secret() {\n    let _ = 2;\n}\n").unwrap();
        let leak = dir.join("leak.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, &leak).unwrap();

        // A regular project file past the index cap.
        let big = dir.join("big.rs");
        std::fs::write(
            &big,
            format!(
                "fn too_big() {{\n    let _ = 3;\n}}\n// {}\n",
                "x".repeat(index::MAX_INDEX_FILE_BYTES as usize)
            ),
        )
        .unwrap();

        let inputs = gather_explain_inputs(
            vec![ok.clone(), leak.clone(), big.clone()],
            dir.clone(),
            &explain::Cache::new(),
        )
        .expect("the project is there");
        let names: Vec<&str> = inputs.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"ok_fn"), "control file dropped: {names:?}");
        assert!(
            !names.contains(&"leaked_secret"),
            "out-of-project symlink target reached the prompt: {names:?}"
        );
        assert!(
            !names.contains(&"too_big"),
            "over-cap file reached the prompt: {names:?}"
        );
        // Refused, and not gone: they cannot be explained — the file over the
        // cap, named with its size, and the link, which leads out of the
        // project, as a host says of it — which is not a file that could not
        // be read.
        let size = std::fs::metadata(&big).unwrap().len();
        let mut refused = HashMap::from([(big.clone(), explain::Unexplainable::TooLarge(size))]);
        #[cfg(unix)]
        refused.insert(
            leak.clone(),
            explain::Unexplainable::Refused(explain::Refusal::OutsideProject),
        );
        assert_eq!(inputs.unexplainable, refused);
        assert!(inputs.unread.is_empty(), "{:?}", inputs.unread);

        // The block-detail gatherer applies the same gate, named path and all.
        let empty: HashMap<String, Option<String>> = HashMap::new();
        assert!(
            gather_fn_detail_input(&dir, ok, "ok_fn", 0, &empty).is_some(),
            "control detail dropped"
        );
        #[cfg(unix)]
        assert!(
            gather_fn_detail_input(&dir, leak, "leaked_secret", 0, &empty).is_none(),
            "detail read through a symlink out of the project"
        );
        assert!(
            gather_fn_detail_input(&dir, big, "too_big", 0, &empty).is_none(),
            "detail read an over-cap file"
        );
    }

    /// A folder's listing is stored in its summary (`explain::Basis::source`),
    /// so how it is taken is frozen here. Changed unbumped — what goes in, or
    /// how it is encoded — every stored folder summary would read as code
    /// added under it, and the next automatic refresh would pay for
    /// everything it had left for Explain All. Each listing takes in the
    /// whole of its folder, so a file added deep inside moves every folder
    /// above it.
    #[test]
    fn folder_listings_are_frozen() {
        let root = PathBuf::from("/p");
        let listed =
            |rels: &[&str]| -> Vec<PathBuf> { rels.iter().map(|r| root.join(r)).collect() };
        let tree = [
            "Cargo.toml",
            "docs/README.md",
            "src/lib.rs",
            "src/util/b.rs",
        ];
        let listings = folder_listings(&root, &listed(&tree));
        let pinned: [(&str, incremental::Version); 4] = [
            ("/p", 17_827_452_912_526_991_789),
            ("/p/docs", 3_175_690_100_756_742_925),
            ("/p/src", 12_325_642_721_469_760_362),
            ("/p/src/util", 16_710_645_793_682_982_028),
        ];
        assert_eq!(
            explain::LISTING_RECIPE,
            2,
            "the recipe these listings were pinned under: pin them again"
        );
        assert_eq!(listings.len(), pinned.len(), "{listings:?}");
        for (dir, want) in pinned {
            assert_eq!(
                listings[Path::new(dir)],
                want,
                "{dir}: a folder's listing is taken another way — bump explain::BASIS_RECIPE, \
                 move explain::LISTING_RECIPE to it, then pin the new listings here"
            );
        }

        let deeper = folder_listings(&root, &listed(&[&tree[..], &["src/util/new.rs"]].concat()));
        for dir in ["/p", "/p/src", "/p/src/util"] {
            assert_ne!(deeper[Path::new(dir)], listings[Path::new(dir)], "{dir}");
        }
        assert_eq!(deeper[Path::new("/p/docs")], listings[Path::new("/p/docs")]);
    }

    /// A function's signature and body, as the gatherer cuts them out of
    /// its file, are what its summary's basis is taken over
    /// (`explain::Basis`), and the basis is stored with the summary: how they
    /// are cut out is frozen here, in each language's shape. Cut out another
    /// way unbumped, every stored function summary would read as its code
    /// having changed, and the next automatic refresh would pay for every
    /// function of the project.
    #[test]
    fn function_extraction_is_frozen() {
        let root = PathBuf::from("/p");
        let files = [
            (
                "src/lib.rs",
                "/// Adds one.\n#[inline]\npub fn add_one(x: u32) -> u32 {\n    x + 1\n}\n\n\
                 impl Point {\n    pub fn new() -> Point {\n        Point { x: 0 }\n    }\n}\n",
            ),
            (
                "src/shapes.py",
                "@cached\ndef area(r):\n    return 3.14 * r * r\n\nclass Shape:\n    \
                 def name(self):\n        return \"shape\"\n",
            ),
            (
                "src/greet.ts",
                "export function greet(name: string): string {\n  return `hi ${name}`;\n}\n",
            ),
            (
                "src/util.c",
                "int helper(int x);\n\nstatic int\nhelper_impl(int x)\n{\n  return x;\n}\n",
            ),
            (
                "src/norm.go",
                "package p\n\nfunc (p *Point) Norm() float64 {\n\treturn p.x\n}\n",
            ),
        ];
        let sources = ExplainSources {
            read: files
                .iter()
                .map(|(rel, text)| {
                    let path = root.join(rel);
                    let lang = highlight::detect(&path).unwrap();
                    (path, (text.to_string(), lang))
                })
                .collect(),
            ..ExplainSources::default()
        };
        let inputs = gather_explain_inputs_from(sources, root.clone(), &explain::Cache::new());
        let mut cut: Vec<(String, String, String, String)> = inputs
            .functions
            .iter()
            .map(|f| {
                let rel = f.file.strip_prefix(&root).unwrap().display().to_string();
                (rel, f.name.clone(), f.signature.clone(), f.body.clone())
            })
            .collect();
        cut.sort();
        let pinned = [
            (
                "src/greet.ts",
                "greet",
                "export function greet(name: string): string {",
                "export function greet(name: string): string {\n  return `hi ${name}`;\n}",
            ),
            (
                "src/lib.rs",
                "add_one",
                "pub fn add_one(x: u32) -> u32 {",
                "pub fn add_one(x: u32) -> u32 {\n    x + 1\n}",
            ),
            (
                "src/lib.rs",
                "new",
                "pub fn new() -> Point {",
                "    pub fn new() -> Point {\n        Point { x: 0 }\n    }",
            ),
            (
                "src/norm.go",
                "Norm",
                "func (p *Point) Norm() float64 {",
                "func (p *Point) Norm() float64 {\n\treturn p.x\n}",
            ),
            (
                "src/shapes.py",
                "area",
                "def area(r):",
                "def area(r):\n    return 3.14 * r * r",
            ),
            (
                "src/shapes.py",
                "name",
                "def name(self):",
                "    def name(self):\n        return \"shape\"",
            ),
            (
                "src/util.c",
                "helper_impl",
                "helper_impl(int x)",
                "static int\nhelper_impl(int x)\n{\n  return x;\n}",
            ),
        ];
        assert_eq!(
            explain::FUNCTION_RECIPE,
            1,
            "the recipe these cuts were pinned under: pin them again"
        );
        let pinned: Vec<(String, String, String, String)> = pinned
            .iter()
            .map(|(rel, name, sig, body)| {
                let text = |s: &str| s.to_string();
                (text(rel), text(name), text(sig), text(body))
            })
            .collect();
        assert_eq!(
            cut, pinned,
            "a function is cut out another way — bump explain::BASIS_RECIPE, move \
             explain::FUNCTION_RECIPE to it, then pin the new cuts here"
        );
    }

    /// While a file cannot be read, a call into it resolves as it did when it
    /// last could be: to the same function, numbered the same — here the
    /// second `helper` of `a.c`, after a prototype, which a call lands on only
    /// when nothing defines the name. Taken for a definition too, the
    /// prototype made `helper` ambiguous and the call resolve to nothing. A
    /// call into a file that cannot be explained resolves to nothing: that
    /// file is no code of the pass.
    #[test]
    fn a_call_into_a_file_that_could_not_be_read_resolves_as_before() {
        let root = PathBuf::from("/proj");
        let lib = "int helper(void);\n\nint helper(void) {\n    return 1;\n}\n";
        let caller = "int top(void) {\n    return helper() + 1;\n}\n";
        let sources = |read: &[(&str, &str)], unread: &[&str]| ExplainSources {
            read: read
                .iter()
                .map(|(rel, text)| {
                    let path = root.join(rel);
                    let lang = highlight::detect(&path).unwrap();
                    (path, (text.to_string(), lang))
                })
                .collect(),
            unread: unread.iter().map(|rel| root.join(rel)).collect(),
            unexplainable: HashMap::new(),
            listed: Vec::new(),
        };
        let callees = |inputs: &explain::Inputs| {
            let top = inputs.functions.iter().find(|f| f.name == "top").unwrap();
            top.callees.clone()
        };
        let read = gather_explain_inputs_from(
            sources(&[("a.c", lib), ("b.c", caller)], &[]),
            root.clone(),
            &explain::Cache::new(),
        );
        let helper = (root.join("a.c"), "helper".to_string(), 1);
        assert_eq!(callees(&read), std::slice::from_ref(&helper));

        let recorded = explain::Cache::from([(
            explain::Node::Function {
                file: helper.0.clone(),
                name: helper.1.clone(),
                ordinal: helper.2,
            },
            explain::Cached {
                summary: "returns one".into(),
                prompt_hash: 1,
                detail: None,
                basis: None,
            },
        )]);
        let unread = gather_explain_inputs_from(
            sources(&[("b.c", caller)], &["a.c"]),
            root.clone(),
            &recorded,
        );
        assert_eq!(callees(&unread), std::slice::from_ref(&helper));

        let mut refused = sources(&[("b.c", caller)], &[]);
        let too_large = explain::Unexplainable::TooLarge(600_000);
        refused.unexplainable.insert(root.join("a.c"), too_large);
        let refused = gather_explain_inputs_from(refused, root.clone(), &recorded);
        assert!(callees(&refused).is_empty(), "{:?}", callees(&refused));
    }

    /// `read_text_file` is the pane load used when clew-server is not up, so
    /// it must refuse the same leaf the server's `ReadFile` refuses — otherwise
    /// the two paths disagree about what the project contains. `File::open`
    /// followed by an `is_file` check could not deliver that: it followed a
    /// symlink, and on a FIFO it blocked inside `open(2)` before the check
    /// ever ran, wedging the blocking worker for the life of the process.
    #[test]
    #[cfg(unix)]
    fn pane_fallback_read_refuses_a_symlink_and_does_not_block_on_a_fifo() {
        let dir = test_dir("panefallback-guard");
        std::fs::create_dir_all(&dir).unwrap();

        // Control: an ordinary file still loads, so a refusal below is the
        // guard firing and not the whole read breaking.
        let plain = dir.join("plain.rs");
        std::fs::write(&plain, "fn plain() {}\n").unwrap();
        assert_eq!(read_text_file(&plain).as_deref(), Ok("fn plain() {}\n"));

        let outside_dir = test_dir("panefallback-guard-outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("outside.rs");
        std::fs::write(&outside, "fn outside() {}\n").unwrap();
        let link = dir.join("link.rs");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            read_text_file(&link).is_err(),
            "the fallback followed a symlink the server's ReadFile refuses"
        );

        // A writer-less FIFO: this must RETURN, not park. Run it on a worker
        // with a deadline so a regression fails the suite instead of hanging
        // it (the blocking `open(2)` never returns, so the thread is leaked
        // deliberately rather than joined).
        let fifo = dir.join("pipe.rs");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .is_ok_and(|s| s.success()),
            "mkfifo is needed for this test"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let p = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_text_file(&p).is_err());
        });
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5)),
            Ok(true),
            "the fallback blocked on a FIFO instead of refusing it"
        );
    }
}

#[cfg(test)]
mod pass_tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(fut)
    }

    /// A small project straight from source text, through the production
    /// gatherer.
    fn project(files: &[(&str, &str)]) -> explain::Inputs {
        let root = PathBuf::from("/proj");
        let read: HashMap<PathBuf, (String, &'static str)> = files
            .iter()
            .map(|(rel, text)| {
                let path = root.join(rel);
                let lang = highlight::detect(&path).unwrap();
                (path, (text.to_string(), lang))
            })
            .collect();
        let sources = ExplainSources {
            read,
            ..ExplainSources::default()
        };
        gather_explain_inputs_from(sources, root, &explain::Cache::new())
    }

    const LIB: &str =
        "pub fn leaf() -> i32 { 1 }\npub fn caller() -> i32 { leaf() + helper::other() }\n";
    const HELPER: &str = "pub fn other() -> i32 { 2 }\n";

    /// Run the production pass, an explicit one, against `model` with
    /// `complete`, returning its outcome and how many calls it made.
    fn pass_with(
        inputs: explain::Inputs,
        prev: explain::Cache,
        model: &str,
        reply: impl Fn(&str) -> Result<String, CallError> + Clone + Send + 'static,
    ) -> (ExplainOutcome, usize) {
        pass_as(explain::Reuse::SamePrompt, inputs, prev, model, reply)
    }

    /// [`pass_with`], the pass keeping what `reuse` says.
    fn pass_as(
        reuse: explain::Reuse,
        inputs: explain::Inputs,
        prev: explain::Cache,
        model: &str,
        reply: impl Fn(&str) -> Result<String, CallError> + Clone + Send + 'static,
    ) -> (ExplainOutcome, usize) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let complete = move |prompt: String| {
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = reply.clone();
            async move { reply(&prompt) }
        };
        let (mut tx, _rx) = iced::futures::channel::mpsc::channel(4096);
        let outcome = block_on(explain_pass(
            &mut tx,
            inputs,
            prev,
            reuse,
            HashSet::new(),
            Stamp {
                root: None,
                epoch: 0,
                conn: None,
            },
            1,
            CallModel {
                name: model.to_string(),
                key: model.to_string(),
            },
            complete,
        ));
        (outcome, calls.load(Ordering::SeqCst))
    }

    fn echo(prompt: &str) -> Result<String, CallError> {
        Ok(format!(
            "sum:{}",
            incremental::content_hash(prompt.as_bytes())
        ))
    }

    /// The production driver reuses every unchanged prompt: a second pass over
    /// the same sources makes no calls at all.
    #[test]
    fn a_repeat_pass_over_unchanged_sources_makes_no_calls() {
        let files = [("src/lib.rs", LIB), ("src/helper.rs", HELPER)];
        let (first, calls) = pass_with(project(&files), explain::Cache::new(), "m-repeat", echo);
        assert_eq!(first.failures, PassFailures::default());
        assert!(calls >= 6, "3 functions, 2 files, 1+ folders: {calls}");
        let (second, calls) = pass_with(project(&files), first.cache.clone(), "m-repeat", echo);
        assert_eq!(calls, 0, "an identical project re-billed {calls} calls");
        assert_eq!(second.cache.len(), first.cache.len());
    }

    /// When what a pass changed cannot be listed, it stops — so nothing is
    /// pruned by it — and saves only what it added where the window knew of
    /// nothing, each as new. It used to hand over every entry it held as
    /// new: a summary written over whatever the store holds, however much
    /// newer, for every node of the project.
    #[test]
    fn a_pass_whose_changes_cannot_be_listed_saves_only_what_it_added() {
        let files = [("src/lib.rs", LIB), ("src/helper.rs", HELPER)];
        let (first, _) = pass_with(project(&files), explain::Cache::new(), "m-unlisted", echo);
        let added = first
            .cache
            .keys()
            .find(|n| matches!(n, explain::Node::Function { name, .. } if name == "other"))
            .unwrap()
            .clone();
        let mut known = first.cache.clone();
        known.remove(&added);
        let mut pass = explain::Pass::new(project(&files), known);
        while let Some(jobs) = pass.next_level() {
            for job in jobs {
                pass.complete(&job, "again".into());
            }
        }
        let outcome = explain_outcome(pass, None, PassFailures::default());
        assert!(
            matches!(outcome.failures.stopped, Some(ExplainFailure::Internal(_))),
            "{:?}",
            outcome.failures
        );
        assert_eq!(outcome.written, explain::Unsaved::from([(added, None)]));
    }

    /// Gathering the same files from maps built in different orders — as the
    /// server and the scan hand them over — gives the same prompts.
    #[test]
    fn gathering_is_independent_of_map_order() {
        let files = [
            ("src/a.rs", "pub fn a() { b(); c(); }\n"),
            ("src/b.rs", "pub fn b() {}\n"),
            ("src/c.rs", "pub fn c() {}\n"),
            ("src/d/e.rs", "pub fn e() { a(); }\n"),
        ];
        let (first, _) = pass_with(project(&files), explain::Cache::new(), "m-order", echo);
        let mut reversed = files;
        reversed.reverse();
        for _ in 0..5 {
            // A fresh map each time: a new random hasher, a new iteration order.
            let (again, calls) =
                pass_with(project(&reversed), first.cache.clone(), "m-order", echo);
            assert_eq!(calls, 0, "a reordered gather changed {calls} prompts");
            assert_eq!(again.cache.len(), first.cache.len());
        }
    }

    /// A provider failure with HTTP status `code`, as llm types it.
    fn status(code: u16, kind: Option<&str>, message: &str) -> llm::LlmError {
        llm::LlmError::Status {
            who: "OpenAI".into(),
            code,
            kind: kind.map(str::to_string),
            message: message.into(),
        }
    }

    /// `e` as a clew-server answers it: what failed, typed, and llm's words
    /// for people.
    fn from_server(e: &llm::LlmError) -> CallError {
        CallError::Rpc(RpcError {
            code: Some(explain::chat_error_code(e)),
            message: e.to_string(),
        })
    }

    /// A rejected key stops the pass at once (no retries) and says why.
    #[test]
    fn a_rejected_key_stops_the_pass_without_retrying() {
        let files = [("src/lib.rs", LIB), ("src/helper.rs", HELPER)];
        let (outcome, calls) = pass_with(project(&files), explain::Cache::new(), "m-auth", |_| {
            Err(CallError::Llm(status(401, None, "invalid x-api-key")))
        });
        let stopped = outcome.failures.stopped.expect("the pass stops");
        assert!(
            matches!(
                stopped,
                ExplainFailure::Rejected {
                    what: Rejection::Key,
                    ..
                }
            ),
            "{stopped:?}"
        );
        assert!(stopped.describe().contains("401"), "{stopped:?}");
        // Only the first level ran, each call exactly once.
        assert!(calls <= 3, "{calls} calls after a rejected key");
        assert!(outcome.cache.is_empty());
    }

    /// A model the provider does not have — a 404 — stops the pass at once,
    /// and the status names it: every call of the pass would be refused the
    /// same way. The pass went on, and each group made a call of its own.
    #[test]
    fn a_model_the_provider_does_not_have_stops_the_pass_and_is_named() {
        let files = [("src/lib.rs", LIB), ("src/helper.rs", HELPER)];
        let missing = status(404, Some("model_not_found"), "The model does not exist");
        let (outcome, calls) = pass_with(
            project(&files),
            explain::Cache::new(),
            "gpt-nano-9",
            move |_| Err(CallError::Llm(missing.clone())),
        );
        let stopped = outcome.failures.stopped.expect("the pass stops");
        assert!(
            matches!(
                &stopped,
                ExplainFailure::Rejected {
                    what: Rejection::Model(model),
                    ..
                } if model == "gpt-nano-9"
            ),
            "{stopped:?}"
        );
        assert!(
            stopped.describe().starts_with(
                "the provider has no model `gpt-nano-9` at this endpoint (OpenAI API error 404"
            ),
            "{}",
            stopped.describe()
        );
        assert_eq!(calls, 1, "the pass stopped at the first refusal");
    }

    /// A 404 names the model only where the provider says it is about the
    /// model: by its kind of error, or by naming it. Any other — a base URL
    /// whose path is wrong, a gateway's page — is said to be about the
    /// endpoint, and points at the base URL; every 404 read "no model `X`".
    /// Either stops the pass.
    #[test]
    fn a_404_names_the_model_only_when_it_is_about_the_model() {
        let model = "gpt-nano-9";
        let no_model = "the provider has no model `gpt-nano-9` at this endpoint (";
        let no_endpoint = "the endpoint was not found (";
        for (e, said) in [
            (
                status(404, Some("model_not_found"), "The model does not exist"),
                no_model,
            ),
            (
                status(404, Some("not_found_error"), "model: gpt-nano-9"),
                no_model,
            ),
            (
                status(
                    404,
                    None,
                    "<html><body><h1>404 Not Found</h1></body></html>",
                ),
                no_endpoint,
            ),
            (
                status(404, Some("not_found_error"), "Not Found"),
                no_endpoint,
            ),
        ] {
            for call in [CallError::Llm(e.clone()), from_server(&e)] {
                let failure = ExplainFailure::of_call(&call, model);
                let words = failure.describe();
                assert!(words.starts_with(said), "{call:?}: {words}");
                assert!(failure.stops_the_pass(), "{call:?}");
                if said == no_endpoint {
                    assert!(
                        words.ends_with("— check the base URL in Settings"),
                        "{words}"
                    );
                }
            }
        }
    }

    /// A prompt refused for its size is not retried in the pass, nor sent
    /// again on a later pass to the same model — but another model gets it.
    #[test]
    fn a_prompt_refused_for_size_is_not_sent_again() {
        let files = [("src/lib.rs", "pub fn only() {}\n")];
        let too_big = |_: &str| -> Result<String, CallError> {
            let words = "prompt is too long: 250000 tokens > 200000 maximum";
            Err(CallError::Llm(status(
                400,
                Some("invalid_request_error"),
                words,
            )))
        };
        let (first, calls) = pass_with(project(&files), explain::Cache::new(), "m-size", too_big);
        // One function, one call: its file and folders, with nothing left to
        // summarize, fail without a call of their own.
        assert_eq!(calls, 1, "a size refusal was retried");
        assert!(first.failures.failed >= 1);
        assert!(
            first.failures.stopped.is_none(),
            "a size refusal is not an auth error"
        );
        let (again, calls) = pass_with(project(&files), explain::Cache::new(), "m-size", too_big);
        assert_eq!(calls, 0, "the refused prompt was sent again");
        assert_eq!(again.failures.failed, first.failures.failed);
        for outcome in [&first, &again] {
            assert!(
                matches!(
                    outcome.failures.calls[..],
                    [ExplainFailure::Rejected {
                        what: Rejection::Length,
                        ..
                    }]
                ),
                "{:?}",
                outcome.failures
            );
        }
        let (_, calls) = pass_with(
            project(&files),
            explain::Cache::new(),
            "m-size-bigger",
            echo,
        );
        assert!(calls >= 1, "a different model is asked");
    }

    /// A call that failed is sent once, whatever failed — but a gateway's
    /// 502 or 503 in an explicit pass, sent again once
    /// (`a_gateway_failure_is_sent_again_before_what_quotes_it`): llm's send
    /// is the one retry policy — it sends again what never reached the
    /// provider, and what the provider asked to have sent again, after the
    /// wait it asked for. The pass sent every failure of the provider, the
    /// connection or clew again, twice: a request the provider had received —
    /// the connection broke while it answered, a gateway's 504, a clew-server
    /// that went quiet — was generated and billed three times, and what llm
    /// had given up on after its own retries was retried on top of them.
    /// Failed, it is left to a later pass. An automatic pass sends a
    /// gateway's failure once too: what quotes it waits for it.
    #[test]
    fn a_failed_call_is_sent_once() {
        use llm::LlmError;
        let files = [("src/lib.rs", "pub fn only() {}\n")];
        let rpc = |code, message: &str| {
            CallError::Rpc(RpcError {
                code,
                message: message.into(),
            })
        };
        let explicit = || explain::Reuse::SamePrompt;
        let automatic =
            || explain::Reuse::ChangedSources(HashSet::from(["/proj/src/lib.rs".into()]));
        let sent = |reuse: explain::Reuse, failure: &CallError| {
            let attempts = Arc::new(AtomicUsize::new(0));
            let (seen, fails) = (attempts.clone(), failure.clone());
            let reply = move |prompt: &str| {
                if prompt.contains("fn only") {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Err(fails.clone())
                } else {
                    echo(prompt)
                }
            };
            let (outcome, _) = pass_as(
                reuse,
                project(&files),
                explain::Cache::new(),
                "m-once",
                reply,
            );
            assert_eq!(outcome.failures.failed, 1, "{failure:?}");
            assert!(outcome.failures.stopped.is_none(), "{failure:?}");
            attempts.load(Ordering::SeqCst)
        };
        let once = [
            CallError::Llm(LlmError::Connect("connection refused".into())),
            CallError::Llm(LlmError::Transport("connection reset by peer".into())),
            CallError::Llm(status(429, None, "Rate limit reached")),
            CallError::Llm(status(504, None, "Gateway Timeout")),
            CallError::Llm(status(529, Some("overloaded_error"), "Overloaded")),
            CallError::Llm(LlmError::Stream("overloaded".into())),
            from_server(&status(504, None, "Gateway Timeout")),
            from_server(&LlmError::Transport(
                "timed out reading the response".into(),
            )),
            rpc(None, "no reply from the server within 600s"),
            rpc(None, "server dropped the request"),
            CallError::Internal("task join failed".into()),
        ];
        for failure in &once {
            assert_eq!(sent(explicit(), failure), 1, "sent again: {failure:?}");
            assert_eq!(sent(automatic(), failure), 1, "sent again: {failure:?}");
        }
        let gateway = [502, 503].map(|code| status(code, None, "Bad Gateway"));
        for failure in gateway
            .iter()
            .flat_map(|e| [CallError::Llm(e.clone()), from_server(e)])
        {
            assert_eq!(sent(explicit(), &failure), 2, "{failure:?}");
            assert_eq!(sent(automatic(), &failure), 1, "{failure:?}");
        }
    }

    /// In an explicit pass, a call a gateway failed with a 502 or 503 is
    /// sent again, once: at the end of its level, a moment after it failed,
    /// and before what quotes it renders its prompt — which quotes it. It
    /// used to stay failed, and its caller, its file and every folder up to
    /// the root were paid for without it, then paid for again when a later
    /// pass explained it. Failing again, it is not sent a third time, and
    /// what quotes it is paid for without it.
    #[test]
    fn a_gateway_failure_is_sent_again_before_what_quotes_it() {
        use std::sync::Mutex;
        use std::time::Instant;
        /// A call made: for which function, when, and its prompt.
        struct Asked {
            name: &'static str,
            at: Instant,
            prompt: String,
        }
        let files = [("src/lib.rs", LIB), ("src/helper.rs", HELPER)];
        for (fails, code) in [(1, 502), (1, 503), (usize::MAX, 502)] {
            for remote in [false, true] {
                let gateway = status(code, None, "Bad Gateway");
                let failure = if remote {
                    from_server(&gateway)
                } else {
                    CallError::Llm(gateway)
                };
                let log: Arc<Mutex<Vec<Asked>>> = Arc::default();
                let failed = Arc::new(AtomicUsize::new(0));
                let reply = {
                    let (log, failed) = (log.clone(), failed.clone());
                    move |prompt: &str| {
                        let name = ["leaf", "other", "caller"]
                            .into_iter()
                            .find(|name| prompt.contains(&format!("Function `{name}`")))
                            .unwrap_or("-");
                        log.lock().unwrap().push(Asked {
                            name,
                            at: Instant::now(),
                            prompt: prompt.to_string(),
                        });
                        if name == "leaf" && failed.fetch_add(1, Ordering::SeqCst) < fails {
                            return Err(failure.clone());
                        }
                        echo(prompt)
                    }
                };
                let (outcome, _) = pass_with(project(&files), explain::Cache::new(), "m-gw", reply);
                let log = log.lock().unwrap();
                let calls = |name| log.iter().filter(|a| a.name == name).collect::<Vec<_>>();
                let (leaf, other, caller) = (calls("leaf"), calls("other"), calls("caller"));
                let what = format!("{code}, remote: {remote}, failing {fails}");
                assert_eq!(leaf.len(), 2, "{what}");
                assert!(
                    leaf[1].at >= other[0].at,
                    "sent again before its level ended: {what}"
                );
                assert!(leaf[1].at - leaf[0].at >= GATEWAY_RESEND_DELAY, "{what}");
                assert!(caller[0].at >= leaf[1].at, "{what}");
                if fails == 1 {
                    let quoted = echo(&leaf[1].prompt).unwrap();
                    let prompt = &caller[0].prompt;
                    assert!(prompt.contains(&quoted), "{what}: {prompt}");
                    assert_eq!(outcome.failures.failed, 0, "{what}");
                    assert_eq!(outcome.tally.unquoted, 0, "{what}");
                } else {
                    assert_eq!(outcome.failures.failed, 1, "{what}");
                    assert!(outcome.tally.unquoted >= 1, "{what}");
                }
            }
        }
    }

    /// A call refused in a way every later one would be — a key the provider
    /// rejects, a model it does not have, an endpoint that is not there —
    /// stops the pass at once: the rest of its level is not asked for, but
    /// handed back, keeping what it had. Every call of the level was made,
    /// and each refused alike.
    #[test]
    fn a_refusal_that_stops_the_pass_stops_it_at_once_and_hands_the_rest_back() {
        let bodies: Vec<String> = (0..30).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
        let files: Vec<(String, &str)> = bodies
            .iter()
            .enumerate()
            .map(|(i, body)| (format!("src/f{i}.rs"), body.as_str()))
            .collect();
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(rel, body)| (rel.as_str(), *body))
            .collect();
        let (first, _) = pass_with(project(&files), explain::Cache::new(), "m-stop", echo);
        // Every prompt as another wording renders it: all are paid for again.
        let prev: explain::Cache = first
            .cache
            .iter()
            .map(|(node, cached)| {
                let mut cached = cached.clone();
                cached.prompt_hash = cached.prompt_hash.wrapping_add(1);
                (node.clone(), cached)
            })
            .collect();
        for refusal in [
            status(401, Some("invalid_api_key"), "Incorrect API key"),
            status(404, Some("model_not_found"), "The model does not exist"),
            status(404, None, "Not Found"),
        ] {
            let refused = refusal.clone();
            let (outcome, calls) = pass_with(project(&files), prev.clone(), "m-stop", move |_| {
                Err(CallError::Llm(refused.clone()))
            });
            assert!(calls <= EXPLAIN_CONCURRENCY, "{calls} calls: {refusal:?}");
            assert!(outcome.failures.stopped.is_some(), "{refusal:?}");
            assert_eq!(outcome.failures.failed, 1, "{refusal:?}");
            // Kept, as recorded: nothing is written, and nothing is dropped.
            assert!(outcome.written.is_empty(), "{:?}", outcome.written);
            let functions = prev
                .iter()
                .filter(|(node, _)| matches!(node, explain::Node::Function { .. }));
            for (node, had) in functions {
                assert_eq!(outcome.cache.get(node), Some(had), "{refusal:?}");
            }
            assert_eq!(outcome.tally.retry.len(), files.len(), "{refusal:?}");
        }
    }

    /// Bodies come off the syntax tree: a C prototype has none and is not an
    /// explain node, and a definition's body is its own — not the next
    /// function's. A config file is not a node; a `.h` C++ header is read as
    /// C++.
    #[test]
    fn the_gatherer_reads_bodies_off_the_tree() {
        let inputs = project(&[
            (
                "src/util.h",
                "namespace u {\nclass Buf {\npublic:\n  int size() const { return n; }\n  int n;\n};\n}\nint helper(int x);\n",
            ),
            (
                "src/util.c",
                "int helper(int x);\nstatic int\nhelper_impl(int x)\n{\n  return x;\n}\nint helper(int x) { return helper_impl(x); }\n",
            ),
            ("config/app.json", "{\"k\": 1}\n"),
        ]);
        let names: Vec<(&str, &str)> = inputs
            .functions
            .iter()
            .map(|f| {
                (
                    f.file.file_name().unwrap().to_str().unwrap(),
                    f.name.as_str(),
                )
            })
            .collect();
        assert!(
            names.contains(&("util.h", "size")),
            "C++ header read as C++: {names:?}"
        );
        let helpers: Vec<&explain::FnInput> = inputs
            .functions
            .iter()
            .filter(|f| f.name == "helper")
            .collect();
        assert_eq!(
            helpers.len(),
            1,
            "prototypes are not explain nodes: {names:?}"
        );
        assert!(
            helpers[0].body.contains("helper_impl(x)"),
            "{:?}",
            helpers[0].body
        );
        let imp = inputs
            .functions
            .iter()
            .find(|f| f.name == "helper_impl")
            .expect("GNU-style definition");
        assert!(imp.body.starts_with("static int"), "{:?}", imp.body);
        assert!(!imp.body.contains("helper(int x) {"), "{:?}", imp.body);
        assert!(
            !inputs.files.iter().any(|f| f.path.ends_with("app.json")),
            "a config file became a node"
        );
        assert!(
            !inputs.folders.iter().any(|d| d.path.ends_with("config")),
            "a folder of config files became a node"
        );
        assert_eq!(inputs.root, PathBuf::from("/proj"));
    }

    /// One panicking or runaway diagram costs its own item only — run the
    /// way the batch runs every unit (`render::run_guarded`: its own thread,
    /// a deadline) — and every failure is named.
    #[test]
    fn a_panicking_or_runaway_renderer_fails_its_item_only() {
        let item = |key: u64, kind: &'static str, src: &str| richmd::Renderable {
            key,
            kind,
            src: src.into(),
            display: true,
        };
        let batch = generate_svgs_with(
            vec![
                item(1, "mermaid", "good"),
                item(2, "mermaid", "boom"),
                item(3, "math", "bad"),
                item(4, "math", "fine"),
                item(5, "mermaid", "forever"),
            ],
            None,
            |r| {
                let src = r.src.clone();
                render::run_guarded(
                    1024 * 1024,
                    std::time::Duration::from_millis(300),
                    move || match src.as_str() {
                        "boom" => panic!("index out of bounds"),
                        "bad" => None,
                        "forever" => {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            None
                        }
                        _ => Some(r#"<svg viewBox="0 0 10 10"></svg>"#.to_string()),
                    },
                )
            },
        );
        let mut rendered: Vec<u64> = batch.rendered.keys().copied().collect();
        rendered.sort();
        assert_eq!(rendered, vec![1, 4]);
        let failed: Vec<(u64, &str)> = batch
            .failed
            .iter()
            .map(|f| (f.key, f.reason.as_str()))
            .collect();
        assert_eq!(failed.len(), 3);
        assert!(
            failed[0].1.contains("crashed") && failed[0].1.contains("index out of bounds"),
            "{failed:?}"
        );
        assert!(failed[1].1.contains("could not parse"), "{failed:?}");
        assert!(
            failed[2].0 == 5 && failed[2].1.contains("did not finish"),
            "{failed:?}"
        );
    }

    /// The block walkthrough's callees are THIS function's: a file's second
    /// `new` (another impl's) is attributed under the same name as the first,
    /// and filtering the call sites by name listed the first's callees as its.
    #[test]
    fn a_same_name_function_lists_only_its_own_callees() {
        let src = "struct A;\nstruct B;\nimpl A {\n    fn new() -> A {\n        alpha();\n        A\n    }\n}\n\
                   impl B {\n    fn new() -> B {\n        beta();\n        B\n    }\n}\n\
                   fn alpha() {}\nfn beta() {}\n";
        let summaries: HashMap<String, Option<String>> = HashMap::from([
            ("alpha".to_string(), Some("does a".to_string())),
            ("beta".to_string(), Some("does b".to_string())),
        ]);
        let file = Path::new("/p/src/lib.rs");
        let (_, body, callees) =
            gather_fn_detail_from(file, src, "new", 1, &summaries).expect("B::new");
        assert!(
            body.contains("beta()") && !body.contains("alpha()"),
            "{body}"
        );
        assert_eq!(callees, vec![("beta".to_string(), "does b".to_string())]);
        let (_, _, callees) =
            gather_fn_detail_from(file, src, "new", 0, &summaries).expect("A::new");
        assert_eq!(callees, vec![("alpha".to_string(), "does a".to_string())]);
    }

    /// The body comes off the syntax tree, so a Dart signature whose named
    /// parameters sit in `{ }` inside the parens — which a brace-counting
    /// heuristic stopped at — still yields the whole body.
    #[test]
    fn a_dart_body_after_named_parameters_is_read_whole() {
        let src = "Future<void> initializeRust(\n  AssignRustSignal<String, dynamic> sig, {\n  \
                   String? compiledLibPath,\n}) async {\n  if (compiledLibPath != null) {\n    \
                   setPath(compiledLibPath);\n  }\n}\nvoid next() {}\n";
        let (_, body, _) = gather_fn_detail_from(
            Path::new("/p/lib/main.dart"),
            src,
            "initializeRust",
            0,
            &HashMap::new(),
        )
        .expect("the function");
        assert!(body.contains("setPath(compiledLibPath);"), "{body}");
        assert!(!body.contains("void next()"), "{body}");
    }

    #[test]
    fn lsp_items_map_onto_the_definition_they_name() {
        let def = |name: &str, line: usize| projectcalls::Def {
            name: name.into(),
            kind: "method".into(),
            file: PathBuf::from("/p/a.rs"),
            line,
        };
        let keys = SymKeys::new(&[def("new", 4), def("new", 9), def("run", 12)]);
        let file = Path::new("/p/a.rs");
        assert_eq!(keys.key(file, "new", 4).2, 0);
        assert_eq!(keys.key(file, "new", 9).2, 1);
        assert_eq!(
            keys.key(file, "new", 10).2,
            1,
            "a name line below the def line"
        );
        assert_eq!(keys.key(file, "new", 2).2, 0, "above every def: the first");
        assert_eq!(keys.key(file, "gone", 5).2, 0);
    }

    /// The status decides what failed wherever there is one — never digits
    /// in the provider's words. An OpenAI 429 ending "Please try again in
    /// 401ms." read as a rejected key, and stopped Explain; "413ms", as a
    /// prompt too long for the model, never sent again that session. So
    /// here, and from a clew-server, which sends the status typed. A
    /// refusal of the request is told apart by what the provider says it
    /// refused, never by a number in it.
    #[test]
    fn the_status_decides_what_failed_not_digits_in_its_words() {
        for ms in ["401ms", "403ms", "413ms"] {
            let words = format!("Rate limit reached for requests. Please try again in {ms}.");
            let limited = status(429, Some("rate_limit_exceeded"), &words);
            for call in [CallError::Llm(limited.clone()), from_server(&limited)] {
                let failure = ExplainFailure::of_call(&call, "m");
                assert!(
                    matches!(failure, ExplainFailure::Unavailable(_)),
                    "{call:?}: {failure:?}"
                );
                assert!(!failure.stops_the_pass(), "{call:?}");
            }
        }
        let refused = status(400, None, "Invalid value 401 for 'n': 413 is the most");
        for call in [CallError::Llm(refused.clone()), from_server(&refused)] {
            assert!(
                matches!(
                    ExplainFailure::of_call(&call, "m"),
                    ExplainFailure::Rejected {
                        what: Rejection::Request,
                        ..
                    }
                ),
                "{call:?}"
            );
        }
        // Words of a status in a failure of another kind are not its status.
        let quoted = llm::LlmError::Transport("proxy said: OpenAI API error 401: no".into());
        assert!(matches!(
            ExplainFailure::of_call(&from_server(&quoted), "m"),
            ExplainFailure::Network(_)
        ));
    }

    /// A failed call is classified by what failed, as the side that made it
    /// types it — and a clew-server's failure, which crosses the wire typed,
    /// as the same failure made here: the provider refusing the request (a
    /// 4xx and its reason: the key, the prompt's length, the model, the
    /// endpoint, the rest; the settings, or the server, refusing it), the
    /// provider unavailable (408, 429, a 5xx, a failure mid-stream),
    /// the connection failing (to the provider, or to the clew-server that
    /// calls it), an answer that could not be used, a cancellation, and
    /// clew's own failure. An answer that could not be used read as the
    /// request refused.
    #[test]
    fn errors_are_classified_by_what_failed() {
        use clew_protocol::ErrorCode;
        use llm::LlmError;
        let of = |call: &CallError| ExplainFailure::of_call(call, "gpt-x");
        let rpc = |code, message: &str| {
            CallError::Rpc(RpcError {
                code,
                message: message.into(),
            })
        };
        let both = |e: LlmError| [CallError::Llm(e.clone()), from_server(&e)];
        let rejected = |call: &CallError| match of(call) {
            ExplainFailure::Rejected { what, detail } => {
                let words = String::from(call.clone());
                assert_eq!(detail, words, "the detail is the error's own words");
                Some(what)
            }
            _ => None,
        };

        let refusals = [
            (
                status(401, Some("invalid_api_key"), "Incorrect API key"),
                Rejection::Key,
            ),
            (status(403, None, "Forbidden"), Rejection::Key),
            (
                status(404, Some("model_not_found"), "The model does not exist"),
                Rejection::Model("gpt-x".into()),
            ),
            (
                status(404, None, "model \"gpt-x\" not found, try pulling it first"),
                Rejection::Model("gpt-x".into()),
            ),
            (
                status(404, None, "<html><body>404 Not Found</body></html>"),
                Rejection::Endpoint,
            ),
            (status(413, None, "Payload Too Large"), Rejection::Length),
            (
                status(
                    400,
                    Some("context_length_exceeded"),
                    "Too long for 8192 tokens",
                ),
                Rejection::Length,
            ),
            (
                status(
                    400,
                    None,
                    "prompt is too long: 250000 tokens > 200000 maximum",
                ),
                Rejection::Length,
            ),
            (
                status(422, None, "Unprocessable Entity"),
                Rejection::Request,
            ),
            (
                LlmError::Redirected {
                    who: "OpenAI".into(),
                    location: "https://elsewhere/".into(),
                },
                Rejection::Request,
            ),
            (
                LlmError::Config("no base URL set for this provider".into()),
                Rejection::Request,
            ),
        ];
        for (e, what) in refusals {
            for call in both(e) {
                assert_eq!(rejected(&call), Some(what.clone()), "{call:?}");
            }
        }
        for call in [
            rpc(Some(ErrorCode::Refused), "no AI chat config on the server"),
            rpc(Some(ErrorCode::Handshake), "no handshake yet"),
        ] {
            assert_eq!(rejected(&call), Some(Rejection::Request), "{call:?}");
        }

        let unavailable = [408, 429, 500, 502, 503, 504, 529]
            .map(|code| status(code, None, "try later"))
            .into_iter()
            .chain([LlmError::Stream("overloaded".into())]);
        for call in unavailable.flat_map(both) {
            assert!(
                matches!(of(&call), ExplainFailure::Unavailable(_)),
                "{call:?}"
            );
        }

        let network = [
            LlmError::Connect("connection refused".into()),
            LlmError::Transport("timed out after 4013 ms".into()),
        ]
        .into_iter()
        .flat_map(both)
        .chain(
            [
                "server gone",
                "server dropped the request",
                "no reply from the server within 600s",
                "the server took no requests for 600s",
                "not connected to a server",
            ]
            .map(|words| rpc(None, words)),
        );
        for call in network {
            assert!(matches!(of(&call), ExplainFailure::Network(_)), "{call:?}");
        }

        let unusable = [
            LlmError::Protocol("bad JSON response: expected value at line 1".into()),
            LlmError::Protocol("no text in response (stop reason: content_filter)".into()),
            LlmError::Protocol("the response is not UTF-8".into()),
        ];
        for call in unusable.into_iter().flat_map(both) {
            assert!(matches!(of(&call), ExplainFailure::Unusable(_)), "{call:?}");
        }

        for call in [
            CallError::Llm(LlmError::Cancelled),
            rpc(Some(ErrorCode::Cancelled), llm::CANCELLED),
        ] {
            assert_eq!(of(&call), ExplainFailure::Cancelled, "{call:?}");
        }

        for call in [
            CallError::Internal("task join failed".into()),
            CallError::Internal("unexpected reply to Chat: Tree".into()),
            rpc(
                Some(ErrorCode::Failed),
                "the chat request failed unexpectedly",
            ),
        ] {
            assert!(matches!(of(&call), ExplainFailure::Internal(_)), "{call:?}");
        }
    }
}

#[cfg(test)]
mod proxy_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// What the client writes into a proxied process crosses as
    /// `ProcessInput` frames of at most the protocol's chunk size — a large
    /// LSP message becomes several small frames, not one that holds the whole
    /// stream — in order, each under its own minted request id.
    #[tokio::test]
    async fn proxied_stdin_is_cut_into_chunks_under_real_ids() {
        use tokio::io::AsyncWriteExt;
        // A queue far smaller than the message: the pump must wait for room
        // (backpressure) rather than drop a chunk.
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let next_id = Arc::new(AtomicU64::new(1));
        let (mut stdin, _stdout, _feed) = proxy_streams(&tx, &next_id, 7);
        let message: Vec<u8> = (0..(3 * clew_protocol::MAX_PROCESS_CHUNK + 123))
            .map(|i| (i % 251) as u8)
            .collect();
        stdin.write_all(&message).await.unwrap();
        drop(stdin);

        let mut received = Vec::new();
        let mut ids = HashSet::new();
        while received.len() < message.len() {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
                .await
                .expect("the bytes are forwarded")
                .expect("the channel is open");
            let clew_protocol::Request::ProcessInput { proc, data } = msg.request else {
                panic!("expected ProcessInput, got {:?}", msg.request);
            };
            assert_eq!(proc, 7);
            assert!(
                !data.is_empty() && data.len() <= clew_protocol::MAX_PROCESS_CHUNK,
                "a chunk of {} bytes",
                data.len()
            );
            assert!(
                msg.id != 0 && ids.insert(msg.id),
                "a fresh id each: {}",
                msg.id
            );
            received.extend(data);
        }
        assert_eq!(received, message, "the stream arrives whole and in order");
        assert!(ids.len() >= 4, "more than one chunk: {}", ids.len());
    }
}
