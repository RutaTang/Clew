//! Keeping the project index current: watcher batches (rehash, structural
//! rescans), off-thread parsing into the symbol index and import graph, the
//! warm-started full symbol index, and the Rust structure index.
//!
//! Its messages, [`WatchMsg`], arrive through `App::update_watch`.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// A watched source file changed, so the understanding may be stale. Start a
    /// refresh now if the cooldown has lifted and nothing is running; otherwise
    /// mark it pending for the next `Tick` past the window, so no change is
    /// dropped. Only refreshes what already exists — the first build of each
    /// artifact stays an explicit user action.
    pub(crate) fn on_files_changed(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        // A file the resolver reads for the whole project (go.mod, a
        // tsconfig, a base a tsconfig `extends` under any name) changed:
        // every import resolves again, against a resolver that reads it anew.
        // Decided on the raw paths, before any filtering: go.mod is not a
        // source file, so no content event would carry it.
        let resolver = self.proj.import_work.resolver.as_deref();
        let reresolve = if paths.iter().any(|p| {
            imports::is_resolution_metadata(p) || resolver.is_some_and(|r| r.reads_config(p))
        }) {
            self.reresolve_import_graph()
        } else {
            Task::none()
        };
        Task::batch([reresolve, self.rehash_changed(paths)])
    }

    /// The content/existence split of [`App::on_files_changed`], read off the
    /// UI thread.
    fn rehash_changed(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let open: HashSet<PathBuf> = self
            .proj
            .panes
            .iter()
            .flatten()
            .map(|v| v.abs.clone())
            .collect();
        let mut seen = HashSet::new();
        // Split the changed paths in two. Content-tracked files (open,
        // already tracked, or a source file the index takes — the index's
        // own filter, `fs_scan::index_language`) are read + hashed for a
        // real content refresh. Everything else that changed (a .txt, a
        // .json) can't change content we display, but it can be the
        // *creation* or *deletion* of a tree entry — so it gets a cheap
        // existence probe (stat, no read) instead. The probe pairs each path
        // with whether the tree currently lists it; a mismatch with on-disk
        // existence is a create/delete that needs a rescan. Whether the tree
        // lists a path is decided by the reader, off this thread: the
        // registry only tracks source files, so only the tree's own file
        // list can tell a new/removed non-source file from an edit to one,
        // and its membership test is a set of every path.
        let mut candidates: Vec<(PathBuf, incremental::Version)> = Vec::new();
        let mut probes = watch::Probes {
            paths: Vec::new(),
            listed: self
                .proj
                .project
                .as_ref()
                .map(|p| p.files.clone())
                .unwrap_or_default(),
        };
        for p in paths {
            if !seen.insert(p.clone()) {
                continue;
            }
            if open.contains(&p)
                || self.proj.registry.is_tracked(&p)
                || clew_core::fs_scan::index_language(&self.rel_of(&p), &p).is_some()
            {
                let v = self.proj.registry.version(&p).unwrap_or(0);
                candidates.push((p, v));
            } else {
                probes.paths.push(p);
            }
        }
        if candidates.is_empty() && probes.paths.is_empty() {
            return Task::none();
        }
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // What each path is being hashed against travels with the result: by
        // the time it lands another batch may already have applied a newer
        // read of the same file, and only these baselines can tell the two
        // apart (see `FilesRehashed::baselines`).
        let baselines: HashMap<PathBuf, incremental::Version> =
            candidates.iter().cloned().collect();
        // And the registry is told these paths are being read, so the initial
        // index landing mid-flight leaves their slots alone instead of filling
        // them with its own older read and stranding this one (see
        // `Registry::begin_read`). Released in `on_files_rehashed`.
        self.proj.registry.begin_read(baselines.keys().cloned());
        let stamp = self.stamp();
        // Read in bounded chunks, one `FilesRehashed` per chunk, each carrying
        // (and releasing) exactly the reads it covered, and the ack that lets
        // the next chunk be read once this one is applied: a branch switch
        // that changes every file holds one chunk's contents at a time.
        Task::run(
            watch::rehash_stream(root, candidates, probes, viewer::MAX_FILE_BYTES as u64),
            move |chunk| {
                Message::Watch(WatchMsg::FilesRehashed {
                    stamp: stamp.clone(),
                    events: chunk.events,
                    baselines: chunk.baselines,
                    fs_structural: chunk.fs_structural,
                    ack: chunk.ack,
                })
            },
        )
    }

    pub(crate) fn on_files_rehashed(
        &mut self,
        events: Vec<watch::FileEvent>,
        baselines: HashMap<PathBuf, incremental::Version>,
        fs_structural: bool,
    ) -> Task<Message> {
        // Release the reads this batch was dispatched with before anything can
        // return early: every path it carried is here whatever its event's
        // fate, and one left marked as being read is a slot no later index
        // pass could ever seed.
        self.proj
            .registry
            .end_read(baselines.keys().map(PathBuf::as_path));
        let mut tasks = Vec::new();
        let mut index_dirty = false;
        // Paths whose read was refused below. They are re-read rather than
        // dropped: refusing is "this read cannot be trusted", not "there is
        // nothing to do here".
        let mut recheck: Vec<PathBuf> = Vec::new();
        // Non-source creations/deletions are already decided by the
        // existence probe in `FilesChanged`; source ones are folded in
        // per event below.
        let mut structural = fs_structural;
        // A source file left the project: its functions leave the call graph.
        let mut deleted = false;
        let mut refreshed = 0usize;
        let mut touched: Vec<PathBuf> = Vec::new();
        // Open notebooks whose bytes moved: they cannot be reloaded from the
        // raw .ipynb, only re-read and re-parsed (see the pane loop below).
        let mut nb_refresh: Vec<PathBuf> = Vec::new();
        // Modified source files to parse (symbols, raw imports) on the blocking
        // pool: tree-sitter over a branch switch's worth of files used to run
        // right here, on the thread that serves every window. The result is
        // applied by `on_files_indexed`, per file, only while the registry
        // still records the bytes that were parsed.
        let mut to_index: Vec<(
            PathBuf,
            String,
            Arc<String>,
            &'static str,
            incremental::Version,
        )> = Vec::new();
        for event in events {
            // Only apply an event to a file whose recorded version is still
            // the one it was hashed against. A batch that read this file
            // before a batch that already landed carries an OLDER view of it,
            // and there is no timestamp in the event to notice that — the
            // baseline is the ordering evidence. Applying it anyway rolls the
            // registry, the symbol index, the import graph and the pane back
            // together.
            //
            // But a moved registry is only evidence that SOMETHING wrote it,
            // and the change dispatch is not its only writer: opening a file
            // records the bytes that load just read (`apply_file_content` /
            // `on_file_loaded`) and derives nothing else from them. So a
            // modification whose own hash is already the recorded one is that
            // same read arriving, not an older one — it cannot roll anything
            // back, and it carries the only re-index, import refresh and trail
            // re-anchor those bytes will ever get.
            let path = match &event {
                watch::FileEvent::Modified(c) => &c.path,
                watch::FileEvent::Deleted(p) => p,
            };
            let current = self.proj.registry.version(path).unwrap_or(0);
            // Every event's path came from `candidates`, so it is in the map;
            // an absent one is treated as "was untracked", the same baseline
            // `on_files_changed` uses for a path the registry does not hold.
            let baseline = baselines.get(path).copied().unwrap_or(0);
            let applies = match &event {
                watch::FileEvent::Modified(c) => current == baseline || current == c.hash,
                // A deletion carries no bytes to be recognized by, so a moved
                // registry leaves it genuinely undecidable: it may be the
                // newer truth, or a stale delete of a path a later batch has
                // already re-created. The re-read below settles it on disk.
                watch::FileEvent::Deleted(_) => current == baseline,
            };
            if !applies {
                recheck.push(path.clone());
                continue;
            }
            match event {
                watch::FileEvent::Modified(c) => {
                    touched.push(c.path.clone());
                    let lang_key = highlight::detect(&c.path);
                    // An untracked *source* file appearing is its creation,
                    // so the tree must gain it. Judged on the baseline rather
                    // than on what the registry holds now: a pane load may
                    // have registered the new file between this batch's
                    // dispatch and its arrival (a goto-definition into a
                    // generated file does exactly that), and the tree would
                    // still be missing it. Non-source create/delete is
                    // handled by the existence probe, which keeps an open
                    // non-source file merely being edited from looking
                    // structural here.
                    structural |= lang_key.is_some() && baseline == 0;
                    self.proj.registry.set(c.path.clone(), c.hash);

                    // Re-index this one file (open or not) — off the UI thread.
                    if let Some(lang) = lang_key {
                        let rel = self.rel_of(&c.path);
                        to_index.push((c.path.clone(), rel, c.content.clone(), lang, c.hash));
                    }

                    // Refresh every pane showing this file, keeping the
                    // reader's scroll/caret/folds so nothing jumps.
                    let mut on_screen = false;
                    for slot in &mut self.proj.panes {
                        if let Some(v) = slot.as_mut().filter(|v| v.abs == c.path) {
                            // A notebook pane's text is the parsed script
                            // projection, not the file's bytes. Reloading it
                            // with the raw .ipynb JSON leaves the pre-edit
                            // cells on screen (the cell view is what gets
                            // painted) over a JSON line space, so the outline
                            // empties and every search hit or goto into that
                            // pane lands on an arbitrary cell. Only a fresh
                            // server parse can rebuild it; ask for one below.
                            if v.notebook.is_some() {
                                if !nb_refresh.contains(&c.path) {
                                    nb_refresh.push(c.path.clone());
                                }
                                continue;
                            }
                            let lines = highlight::plain_lines(&c.content);
                            v.reload(c.content.clone(), lines);
                            on_screen = true;
                        }
                    }
                    if on_screen {
                        refreshed += 1;
                        // `content_tasks` re-runs `git::try_info` over the NEW
                        // bytes. Retire any server blame still in flight for
                        // this file: it was requested against the pre-change
                        // bytes, the two passes are not ordered against each
                        // other, and the loser is simply the one applied last —
                        // so an unretired reply repaints the gutter bars and
                        // the caret-line blame from the previous revision, at
                        // line indices that no longer describe the text.
                        self.retire_git_requests_for(&c.path);
                        tasks.push(self.content_tasks(c.path.clone(), c.content.clone(), lang_key));
                    }
                    // Resync the language server's copy whether or not a pane
                    // still shows the file (see `resync_open_doc` for why).
                    self.resync_open_doc(&c.path, &c.content);
                    // A value trace through this file follows its lines.
                    self.reanchor_flow(&c.path, Some(c.content.as_str()));
                }
                watch::FileEvent::Deleted(path) => {
                    touched.push(path.clone());
                    self.reanchor_flow(&path, None);
                    structural = true;
                    deleted = true;
                    self.proj.registry.remove(&path);
                    index_dirty |= self.proj.symbol_index_by_file.remove(&path).is_some();
                    self.proj.calls_by_file.remove(&path);
                    self.note_refine_change(&path);
                    self.proj.import_work.pending.remove(path.clone());
                    if self.proj.panes.iter().flatten().any(|v| v.abs == path) {
                        self.status = format!("{} was deleted on disk", self.rel_of(&path));
                    }
                }
            }
        }
        // Re-read each changed notebook through the server, which is the only
        // thing that can hand back parsed cells. The registry entry set above
        // holds the raw bytes' hash until the reply re-seeds it with the new
        // projection's, so a reply that never comes leaves the pane stale
        // rather than lying about a rebuild that did not happen.
        for path in nb_refresh {
            let rel = self.rel_of(&path);
            self.request_file_refresh(&rel);
        }
        if index_dirty {
            self.rebuild_symbol_index();
        }
        // The modified files' symbols and imports: parsed on the blocking pool
        // and applied by `on_files_indexed` (which also re-anchors the trail
        // to the symbols' new lines). A deleted file has no symbols left, so
        // its trail entries keep their line (clicking one reports it is gone).
        if !to_index.is_empty() {
            tasks.push(self.index_files_off_thread(to_index));
        }
        // A deletion drops the file's own out-edges, queued above and applied
        // off-thread with whatever else is pending — and, for a Rust file,
        // re-resolves the files whose `use` was followed through it
        // (`ImportGraph::apply`). Everything else that pointed AT the file
        // re-resolves once the rescan lands the new file set (see
        // `TreeUpdated`).
        tasks.push(self.schedule_imports());
        // If a file the open call hierarchy references changed, the tree
        // may now be out of date — flag it (re-run `gc` to refresh).
        if let Some(t) = &mut self.proj.call_graph
            && !t.stale
            && touched.iter().any(|p| t.depends_on(p))
        {
            t.stale = true;
        }
        // A deleted file's functions leave the call graph while something
        // draws from it: refined without them at once, or rebuilt once the
        // job just queued has dropped the file's imports — the scope the
        // name-based graph links through (`refresh_call_graph_after_imports`;
        // built at once, it was built again when that job moved the scope).
        // An edited or created file's reach it once their parse lands
        // (`on_files_indexed`), which is what the graph is linked from.
        if deleted {
            tasks.push(self.refresh_call_graph_after_imports());
        }
        // A created/deleted/renamed file changes the tree and Cmd+P list;
        // rebuild them off-thread (the watcher already debounced the burst).
        if structural && let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) {
            tasks.push(self.rescan_tree(root));
        }
        // Read the refused paths again, against what the registry holds now.
        // Nothing else will: the watcher drops the next read of bytes it
        // already believes are recorded (`hash == old`), so a file that has
        // settled produces no further event, and a file that is GONE cannot
        // produce one at all — its symbols and its graph node would stay for
        // the session. This terminates because the re-read is baselined on the
        // version the registry holds, so bytes that agree with it yield no
        // event, and a re-created path comes back as a plain modification.
        if !recheck.is_empty() {
            tasks.push(self.on_files_changed(recheck));
        }
        // An edited Rust file may have gained or lost `impl` blocks, so the
        // hover peek's "impl … / Implementors …" line is out of date. One
        // rebuild for the whole batch, never one per event. A STRUCTURAL change
        // is left to the rescan instead: `p.files` does not list a file created
        // moments ago, so a build spawned from here would parse the tree as it
        // was before the creation and hide the new file's impls (see
        // `on_tree_updated`).
        if !structural && touched.iter().any(|p| highlight::detect(p) == Some("rust")) {
            tasks.push(self.request_structure_build());
        }
        if refreshed == 1 {
            self.status = "Refreshed a file changed on disk".to_string();
        } else if refreshed > 1 {
            self.status = format!("Refreshed {refreshed} files changed on disk");
        }
        // A source file changed → the understanding (explanations →
        // index → overview) may be stale. Auto-refresh it, throttled to
        // AUTO_REFRESH_MIN_INTERVAL so an edit burst coalesces into one
        // pass (see `request_auto_refresh`). That pass finds changed code
        // by content — these edits, and any the watcher missed — and pays
        // for it and for what quotes a summary it writes, never for a new
        // prompt wording; these paths only tell it where code that has no
        // summary yet was written (`explain::Reuse::ChangedSources`).
        let sources: Vec<PathBuf> = touched
            .into_iter()
            .filter(|p| highlight::detect(p).is_some())
            .collect();
        if !sources.is_empty() {
            self.proj.explain.changed_sources.extend(sources);
            tasks.push(self.request_auto_refresh());
        }
        Task::batch(tasks)
    }

    /// Parse a watcher batch's modified source files (symbols, raw imports) on
    /// the blocking pool; the result lands as `FilesIndexed`.
    fn index_files_off_thread(
        &self,
        files: Vec<(
            PathBuf,
            String,
            Arc<String>,
            &'static str,
            incremental::Version,
        )>,
    ) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        let stamp = self.stamp();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    files
                        .into_iter()
                        .map(|(path, rel, content, lang, hash)| {
                            // One parse per file for its symbols AND imports.
                            let facts = index::analyze_file(&path, &rel, &content, lang);
                            IndexedFile {
                                symbols: facts.symbols,
                                imports: facts.imports,
                                calls: facts.calls,
                                path,
                                hash,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default()
            },
            move |files| {
                Message::Watch(WatchMsg::FilesIndexed {
                    stamp: stamp.clone(),
                    files,
                })
            },
        )
    }

    /// Apply the off-thread parse of a watcher batch (see `FilesIndexed`):
    /// each file's symbols and import out-edges, only while the registry still
    /// records the exact bytes that were parsed — a newer edit that landed in
    /// between has its own parse coming, and this one must not roll the index
    /// back to the older text. Then re-anchor the reading trail to the new
    /// symbol lines and refresh the import cycles.
    pub(crate) fn on_files_indexed(&mut self, files: Vec<IndexedFile>) -> Task<Message> {
        let mut index_dirty = false;
        let mut applied: Vec<PathBuf> = Vec::new();
        for f in files {
            if self.proj.registry.version(&f.path) != Some(f.hash) {
                continue;
            }
            // The file's imports and the items it defines, from this one
            // parse, queued together: a glob re-export elsewhere resolves
            // through the items this very edit added or removed.
            let items = imports::rust_item_keys(&f.path, &f.symbols);
            self.proj.import_work.pending.set(
                f.path.clone(),
                imports::FileImports {
                    raw: f.imports,
                    items,
                },
            );
            if f.symbols.is_empty() {
                index_dirty |= self.proj.symbol_index_by_file.remove(&f.path).is_some();
            } else {
                self.proj
                    .symbol_index_by_file
                    .insert(f.path.clone(), Arc::new(f.symbols));
                index_dirty = true;
            }
            // The call sites of the same parse, for the next call-graph build.
            match f.calls.filter(|c| !c.is_empty()) {
                Some(calls) => {
                    self.proj.calls_by_file.insert(f.path.clone(), calls);
                }
                None => {
                    self.proj.calls_by_file.remove(&f.path);
                }
            }
            self.note_refine_change(&f.path);
            applied.push(f.path);
        }
        if index_dirty {
            self.rebuild_symbol_index();
        }
        // The batch's imports resolve off the UI thread, in one job, in
        // proportion to what the batch can have changed (`ImportGraph::apply`).
        let graph = self.schedule_imports();
        // What the call graph is linked from moved — definitions and call
        // sites, a file created with them: it is brought up to date while
        // something draws from it, once the job just queued has resolved
        // their imports — the scope it links through, which an edit can move
        // (built at once, it was built again when that job landed). The
        // import scope is not the only way in: a file that imports only
        // packages never moves it, and appeared on an open map only once it
        // was reopened.
        let calls = if applied.is_empty() {
            Task::none()
        } else {
            self.refresh_call_graph_after_imports()
        };
        // Keep the reading trail anchored across edits: re-point each changed
        // file's history entries to their symbol's new line.
        let mut trail_moved = false;
        for path in &applied {
            let symbols: Vec<(String, usize)> = self
                .proj
                .symbol_index_by_file
                .get(path)
                .map(|syms| {
                    syms.iter()
                        .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                        .map(|s| (s.name.clone(), s.line))
                        .collect()
                })
                .unwrap_or_default();
            trail_moved |= self.proj.history.reanchor(path, &symbols);
        }
        let saved = if trail_moved {
            self.save_history()
        } else {
            Task::none()
        };
        Task::batch([saved, graph, calls])
    }

    /// The per-file symbol map changed: a new revision, and the flattened
    /// view is stale. It is re-flattened (every symbol of the project copied)
    /// only when something reads it — at once while the finder shows symbols,
    /// which re-ranks against it anyway — never once per watcher batch.
    pub(crate) fn rebuild_symbol_index(&mut self) {
        self.proj.symbol_index_stale = true;
        self.proj.symbol_index_rev += 1;
        if self.proj.finder.open && self.proj.finder.mode == FinderMode::Symbols {
            let symbols = self.fresh_symbol_index();
            self.proj.finder.refresh_symbols(&symbols);
        }
    }

    /// The flattened symbol index, re-flattened first if it is stale.
    pub(crate) fn fresh_symbol_index(&mut self) -> Arc<Vec<SymbolEntry>> {
        if self.proj.symbol_index_stale {
            self.proj.symbol_index = Arc::new(index::flatten(&self.proj.symbol_index_by_file));
            self.proj.symbol_index_stale = false;
        }
        self.proj.symbol_index.clone()
    }

    /// How many symbols the index holds — counted per file, without
    /// flattening.
    pub(crate) fn symbol_count(&self) -> usize {
        self.proj
            .symbol_index_by_file
            .values()
            .map(|syms| syms.len())
            .sum()
    }

    /// (Project ownership is checked in `dispatch`, from the stamp: a late
    /// result would otherwise seed this project's registry with another
    /// project's files.)
    pub(crate) fn on_symbol_index_done(&mut self, indexed: index::Indexed) -> Task<Message> {
        self.proj.indexing = false;
        // What the index had to leave out (its file / size caps), said once
        // here rather than left for the reader to discover as a missing symbol
        // — and kept, so the trees and the finder built on it can say so too.
        let cap_note = indexed.cap_note();
        self.proj.index_cap_note = cap_note.clone();
        // Seed the change-detection registry from the same tree read — but the
        // read describes the tree as it was when this build was SPAWNED, and
        // the watcher (plus every file the reader opened) kept writing the
        // registry for however long it ran. The registry is the ordering
        // evidence (the same argument `on_files_rehashed` makes per event):
        // every other writer took its read after this build took its, so a
        // path already tracked at a different version is the newer read and
        // keeps its slot, and a path a change dispatch is still hashing keeps
        // its EMPTY slot, because that empty slot is the baseline proving the
        // arriving read is not stale (see `Registry::seed`). Applying the
        // build wholesale rolled exactly those files back, and the rollback
        // STUCK: restoring the file to the indexed bytes is then swallowed by
        // the watcher's `hash == old` filter, leaving an open pane showing
        // content that is not on disk.
        //
        // The registry is that evidence only for a file that still EXISTS.
        // Deletion leaves no trace in it — there are no tombstones, so a path
        // deleted during the window and a path never seen are the same empty
        // slot, and `seed` inserts the build's pre-deletion hash for both.
        // Nothing later prunes what that puts back, so the live file set is
        // consulted as well: the watcher's rescan has already spliced a tree
        // without the file, and everything this build emitted came from the
        // file list it was spawned with, so "in the result, not in the tree"
        // is exactly "deleted or renamed since the build started" and is
        // treated as stale. Otherwise the dead file kept its symbols in Cmd+P
        // (selecting one failed to open a path that is not there) and its
        // out-edges in the import graph and its cycles, for the session.
        let live: HashSet<PathBuf> = self
            .proj
            .project
            .as_ref()
            .map(|p| p.files.iter().map(|f| f.abs.clone()).collect())
            .unwrap_or_default();
        let hashes: Vec<(PathBuf, incremental::Version)> = indexed
            .hashes
            .into_iter()
            .filter(|(path, _)| live.contains(path))
            .collect();
        let stale: HashSet<PathBuf> = self.proj.registry.seed(hashes).into_iter().collect();
        // Merged per file rather than replacing the map wholesale: every file
        // untouched during the window still gets its index, and the ones the
        // watcher re-indexed (or created) meanwhile keep their newer symbols.
        for (path, syms) in indexed.by_file {
            if !stale.contains(&path) && live.contains(&path) {
                self.proj.symbol_index_by_file.insert(path, Arc::new(syms));
            }
        }
        // The call sites the same parses read, merged under the same rule.
        for (path, calls) in indexed.calls_by_file {
            if !stale.contains(&path) && live.contains(&path) {
                self.proj.calls_by_file.insert(path, calls);
            }
        }
        let changed_while_closed = indexed.changed.len();
        self.rebuild_symbol_index();
        // Build the import graph from the same single tree read, merged the
        // same way: every file of the build is queued, but never over a
        // change the watcher queued or applied during the window (the graph
        // was reset at project open, so that newer work is all it holds).
        // Every file goes into ONE batch, installed whole and resolved once,
        // off the UI thread — not one whole-graph resolution per file. A
        // file that defines items without importing anything is queued too:
        // a glob re-export elsewhere resolves through its items.
        let mut rust_items = indexed.rust_items;
        let mut raw = indexed.imports_by_file;
        let paths: HashSet<PathBuf> = raw.keys().chain(rust_items.keys()).cloned().collect();
        let pending = &mut self.proj.import_work.pending;
        for path in paths {
            if stale.contains(&path) || !live.contains(&path) {
                continue;
            }
            let imports = imports::FileImports {
                raw: raw.remove(&path).unwrap_or_default(),
                items: rust_items.remove(&path).unwrap_or_default(),
            };
            pending.set_unless_queued(path, imports);
        }
        // The overview's module map is drawn from the resolved graph.
        self.proj.import_work.refresh_overview = true;
        let graph_task = self.schedule_imports();
        if changed_while_closed > 0 {
            self.status = format!(
                "{changed_while_closed} file{} changed since last session",
                if changed_while_closed == 1 { "" } else { "s" }
            );
        } else if let Some(p) = &self.proj.project {
            self.status = format!("{} files · {} symbols", p.files.len(), self.symbol_count());
        }
        let structure_task = self.request_structure_build();
        // A refine started before the index landed read only the few files
        // indexed by then, and nothing notes what the index adds: its graph
        // cannot be completed incrementally.
        if self.refine_owns_call_graph() {
            self.drop_refinement(
                crate::app::graph::REFINED_BEFORE_INDEXED,
                crate::app::graph::REFINE_AGAIN,
            );
        }
        // What the index left out is said either way: said once, here.
        if let Some(note) = cap_note {
            self.status = format!("{} — {note}", self.status);
        }
        // The local call graph is linked from the index: one built before it
        // landed (the Calls map opened while indexing) read none of it, and
        // a build only checks what moved while it ran — so this is what
        // rebuilds it.
        let calls_task = self.refresh_call_graph_in_use();
        Task::batch([graph_task, structure_task, calls_task])
    }

    /// (Re)build the Rust type-structure index off-thread (for the hover
    /// "implements / implementors" peek), against the file set and the bytes
    /// currently on disk.
    ///
    /// Single-flight and coalescing. The build re-reads and re-parses every
    /// Rust file in the project, so a change arriving while one runs only marks
    /// the result-to-be stale and the next build is spawned when that one lands
    /// (see `StructureBuilt`) — an edit burst must not stack one whole-project
    /// parse per save. The alternative, building once per project open, is what
    /// left the peek answering with the relations the project had at open for
    /// the rest of the session.
    pub(crate) fn request_structure_build(&mut self) -> Task<Message> {
        // A REMOTE project's index is extracted where the files live and
        // arrives with `ProjectSymbols`; this machine's disk at the same paths
        // is another project's code, so parsing it here would answer the peek
        // with impls the project being read does not contain.
        if !self.local_project_state() {
            return Task::none();
        }
        let Some((root, files)) = self
            .proj
            .project
            .as_ref()
            .map(|p| (p.root.clone(), p.files.clone()))
        else {
            return Task::none();
        };
        if self.proj.structure_building {
            self.proj.structure_dirty = true;
            return Task::none();
        }
        self.proj.structure_building = true;
        self.proj.structure_dirty = false;
        // What the build read travels with the result: by the time it lands a
        // newer build may already have been applied, and only the revision can
        // tell the two apart (see `StructureBuilt::rev`).
        let rev = self.proj.registry.revision();
        let stamp = self.stamp();
        Task::perform(
            {
                let build_root = root;
                async move {
                    tokio::task::spawn_blocking(move || structure::build(&build_root, &files))
                        .await
                        .unwrap_or_default()
                }
            },
            move |index| {
                Message::Watch(WatchMsg::StructureBuilt {
                    stamp: stamp.clone(),
                    rev,
                    index,
                })
            },
        )
    }
}

impl App {
    /// Handle a [`WatchMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_watch(&mut self, message: WatchMsg) -> Task<Message> {
        match message {
            // (A build from a superseded project instance would seed this
            // project's registry with the other one's files: dropped in
            // `dispatch`.)
            WatchMsg::SymbolIndexDone { indexed, .. } => self.on_symbol_index_done(indexed),
            WatchMsg::StructureBuilt { rev, index, .. } => {
                // The build belongs to one project instance, checked in
                // `dispatch` before any flag is touched: a build from a project
                // we have left must not clear the single-flight flag the
                // CURRENT project's build set.
                self.proj.structure_building = false;
                // A build describes the files as they were when it was
                // spawned, and arriving last does not make it the newest read.
                if rev >= self.proj.structure_rev {
                    self.proj.structure_rev = rev;
                    self.proj.structure = index;
                }
                // Rust files changed while this one ran, so it is already
                // behind: rebuild once now instead of once per event then.
                if self.proj.structure_dirty {
                    return self.request_structure_build();
                }
                Task::none()
            }
            // (A rehash computed for a project we have left would write its
            // files into this one's registry, symbol index and import graph,
            // and buy a rescan plus an LLM auto-refresh pass with them:
            // dropped in `dispatch`.)
            WatchMsg::FilesRehashed {
                events,
                baselines,
                fs_structural,
                ack,
                ..
            } => {
                let applied = self.on_files_rehashed(events, baselines, fs_structural);
                // Applied: the rehash may read its next chunk now. (A batch
                // dropped in `dispatch` as stale releases it the same way.)
                drop(ack);
                applied
            }
            WatchMsg::FilesIndexed { files, .. } => self.on_files_indexed(files),
        }
    }
}
