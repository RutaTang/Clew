//! The clew-server seam and AI plumbing: symbol index, server events/replies, AI client, file content application, stats, project-call-graph build/refine.

use crate::app::prelude::*;
use crate::*;

/// The "this is not a real registry revision" stamp for `stats.rev`. The same
/// value `on_scan_done` writes on project load to force one recompute: the
/// freshness test is `stats.rev == registry.revision()`, and the revision
/// counter starts at 0 and only climbs, so this can never look fresh.
pub(crate) const STATS_REV_STALE: u64 = u64::MAX;

/// The same "not a real registry revision" stamp for `docs.rev`, written when a
/// `BuildDocs` is abandoned without a reply (the transport died, the server
/// refused). `request_docs` stamps the revision it asked at BEFORE the answer
/// exists, so leaving that stamp behind on an abandoned build would leave the
/// PREVIOUS index labelled with the CURRENT revision — the one over-claim this
/// freshness key must never make. Unlike stats, a docs build has no result
/// message to carry a failure stamp: the reply is a server event that simply
/// never arrives.
pub(crate) const DOCS_REV_STALE: u64 = u64::MAX;

/// Build the `StatsDone` message for a finished run — `None` when the run
/// FAILED.
///
/// `Message::StatsDone` has no error channel, so a failure is reported by
/// stamping [`STATS_REV_STALE`] instead of the revision the run was started
/// at, and `on_stats_done` reads that as "no report". Handing the failure over
/// as an ordinary empty report instead made the Stats view claim "No code
/// files to count in this project." for a repo full of code, announce "Code
/// statistics ready", write the empty report into the derived cache, and — the
/// part with no way out — stamp it as fresh, so re-entering the view never
/// retried (that screen's Refresh button is only rendered once a non-empty
/// report exists).
pub(crate) fn stats_done(
    root: PathBuf,
    epoch: u64,
    rev: u64,
    report: Option<stats::StatsReport>,
) -> Message {
    Message::StatsDone {
        root,
        epoch,
        rev: if report.is_some() {
            rev
        } else {
            STATS_REV_STALE
        },
        report: report.unwrap_or_default(),
    }
}

/// Read the current source of each supported, reasonably sized file the local
/// project call graph is built from (a big file's calls aren't worth the parse
/// cost).
///
/// Every read goes through the family guard, exactly as the AI-side gather
/// (`tasks.rs`) and the server's own readers do: the file list was produced by
/// a scan that can be minutes old, so a listed leaf may since have become a
/// symlink — whose target's text would be parsed into the graph and drawn as
/// this project's code — or a FIFO, which would block this blocking thread
/// forever and leave `project_calls.building` stuck true, making every later
/// `ensure_call_graph` a no-op. `read_confined_capped` re-checks containment,
/// opens once with `O_NOFOLLOW | O_NONBLOCK`, and enforces the cap on the read
/// rather than on a stat the read never sees.
fn read_call_sources(root: &Path, files: Vec<PathBuf>) -> Vec<(PathBuf, String)> {
    files
        .into_iter()
        .filter(|f| highlight::detect(f).is_some())
        .filter_map(|f| {
            clew_core::fs_scan::read_confined_capped(root, &f, index::MAX_INDEX_FILE_BYTES)
                .map(|c| (f, c))
        })
        .collect()
}

impl App {
    /// Re-flatten the per-file symbol map into `symbol_index` and refresh the
    /// finder when it is showing symbols.
    pub(crate) fn rebuild_symbol_index(&mut self) {
        self.symbol_index = Arc::new(index::flatten(&self.symbol_index_by_file));
        if self.finder.open && self.finder.mode == FinderMode::Symbols {
            self.finder.refresh_symbols(&self.symbol_index);
        }
    }

    /// Take one remote `.clew/<rel>` file's text as this window's copy of that
    /// store — from a read (`StateContent`) or from the merged file a
    /// `StateEdited` carries back.
    ///
    /// The two mergeable list stores are re-sorted here. A remote merge is
    /// applied by the server, which is told the fields that IDENTIFY an entry
    /// and not the ones the store displays by, so it appends where the local
    /// path would have inserted in order.
    ///
    /// Any view state that ADDRESSES one of these lists by position has to be
    /// rebased for that reordering. Only the walkthrough library has such
    /// state (`walk.open`), and it is re-resolved by scope below. Bookmarks
    /// and notes need nothing: the indices in their messages are read in the
    /// same update as the click that produced them, against the list on
    /// screen, and the only selection either keeps across a round trip
    /// (`note_edit`) is keyed by `(rel, line)`.
    fn adopt_remote_state(&mut self, root: &Path, rel: &str, text: &str) {
        match rel {
            "history.json" => self.history = history::from_text(root, text),
            _ if rel == bookmarks::REL => {
                let mut list = bookmarks::from_text(text);
                bookmarks::sort(&mut list);
                self.bookmarks = list;
            }
            _ if rel == notes::REL => {
                let mut list = notes::from_text(text);
                notes::sort(&mut list);
                self.notes = list;
            }
            "reading.toml" => {
                if let Some(target) = reading::target_from_text(text) {
                    self.reading_target = target;
                    // Re-evaluate the cfg dimming for anything open.
                    let t = self.reading_target.clone();
                    for v in self.panes.iter_mut().flatten() {
                        if let Some(lang) = v.lang_key {
                            let src = v.source.clone();
                            v.inactive_lines = inactive::inactive_lines(&src, lang, &t);
                        }
                    }
                }
            }
            _ if rel == walkthrough::LIBRARY_REL => {
                if let Some(library) = walkthrough::from_text(text) {
                    // The open tour is remembered by SCOPE, not by the index
                    // `walk.open` holds: the list arriving here is the FILE's,
                    // in the file's order, so a tour another client appended
                    // before this window's — or removed — moves every index in
                    // the snapshot `open` was computed against. Left alone,
                    // the WALK pane silently switched to somebody else's tour
                    // (or, past the end, rendered nothing at all) while
                    // next/prev navigated the editor into that tour's files.
                    // Both LOCAL mutation paths already re-resolve this way
                    // (`on_walkthrough_delete`, `on_walkthrough_done`); this
                    // is the same rule for the two remote adoptions —
                    // `StateEdited` after a merge, and the `StateContent`
                    // re-read on reconnect.
                    let open_scope = self
                        .walk
                        .open
                        .and_then(|o| self.walk.library.get(o))
                        .map(|w| w.scope.clone());
                    self.walk.library = library;
                    self.walk.open = open_scope
                        .and_then(|s| self.walk.library.iter().position(|w| w.scope == s));
                    // The tour is gone from the file: drop its narration too,
                    // the same way the local delete path does, instead of
                    // leaving prose on screen for a tour nothing selects.
                    //
                    // NOT closed: when the scope survives, `prepared` is left
                    // as it is. It still holds the narration of the version
                    // this window rendered, so a tour another client
                    // REGENERATED under the same scope shows its old prose
                    // until the user steps or reopens it. Re-preparing here
                    // means `walkthrough_goto`, which opens files and moves
                    // the editor — surprise navigation from a background state
                    // adoption is the worse failure.
                    if self.walk.open.is_none() {
                        self.walk.prepared = Vec::new();
                    }
                }
            }
            // An unknown rel: nothing here holds it.
            _ => {}
        }
    }

    /// Kick an off-thread (re)build of the project call graph from the current
    /// symbol index + file contents. Delivered as `ProjectCallsBuilt`.
    /// Apply an event from the clew-server. Backend flows are handled here as
    /// they migrate onto the protocol. Returns the follow-up work an event
    /// requires (e.g. the derived-state refresh a watcher change triggers).
    pub(crate) fn handle_server_event(&mut self, event: clew_protocol::Event) -> Task<Message> {
        use clew_protocol::Event;
        let mut task = Task::none();
        match event {
            Event::Error { message } => {
                // An uncorrelated failure: stop the picker's spinner only when
                // no listing is in flight, so a stray error cannot un-spin a
                // request that is still coming (the correlated arm owns that).
                if self.pending_list_dir.is_none()
                    && let Some(ConnectStage::Browsing(b)) =
                        self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.loading = false;
                }
                self.status = message;
            }
            Event::ChatDelta { stream, text } => {
                if let Some(tx) = self.chat_streams.lock().unwrap().get(&stream) {
                    let _ = tx.send(ChatStreamPiece::Delta(text));
                }
            }
            Event::ChatStreamDone { stream, error } => {
                // Finished (or cancelled) on the server: nothing left to stop.
                if self.chat_stream == Some(stream) {
                    self.chat_stream = None;
                }
                if let Some(tx) = self.chat_streams.lock().unwrap().remove(&stream) {
                    let _ = tx.send(ChatStreamPiece::Done(error));
                }
            }
            Event::AgentStep {
                stream,
                tool,
                title,
                refs,
            } => {
                if let Some(tx) = self.agent_streams.lock().unwrap().get(&stream) {
                    let _ = tx.send(AgentPiece::Step(AgentStep {
                        tool,
                        title,
                        refs: refs.into_iter().map(|r| (r.rel, r.line)).collect(),
                    }));
                }
            }
            Event::AgentDelta { stream, text } => {
                if let Some(tx) = self.agent_streams.lock().unwrap().get(&stream) {
                    let _ = tx.send(AgentPiece::Delta(text));
                }
            }
            Event::AgentDone { stream, error } => {
                if let Some(tx) = self.agent_streams.lock().unwrap().remove(&stream) {
                    let _ = tx.send(AgentPiece::Done(error));
                }
            }
            Event::Docs { root, files } => {
                // The build is slow; its result may describe a project we have
                // already left.
                if self
                    .project
                    .as_ref()
                    .map(|p| p.root.to_string_lossy().into_owned())
                    != Some(root)
                {
                    return Task::none();
                }
                self.docs.files = files;
                self.docs.loading = false;
                self.pending_docs = None;
                // The open doc page was flattened from the PREVIOUS index, so a
                // rebuild would leave it presenting pre-edit signatures and
                // doc text, with an "Open source" button carrying the pre-edit
                // line. Re-resolve it against the index that just arrived,
                // matching the item by NAME within the same file — its line is
                // exactly what an edit above it moves, so the line cannot be
                // the key here.
                if let Some((rel, name)) = self
                    .docs
                    .page
                    .as_ref()
                    .and_then(|p| Some((p.rel.clone(), p.entries.first()?.name.clone())))
                {
                    match self
                        .docs
                        .files
                        .iter()
                        .find(|f| f.rel == rel)
                        .and_then(|f| find_doc_by_name(std::slice::from_ref(f), &name))
                    {
                        Some((_, line)) => self.open_doc_page(&rel, line),
                        // The item is gone from the file (deleted, renamed, or
                        // no longer parsed). There is nothing to re-resolve to,
                        // and leaving the page up would present a symbol this
                        // project no longer documents as current.
                        None => {
                            self.docs.page = None;
                            self.status = format!("“{name}” is no longer in {rel}");
                        }
                    }
                }
                // Resolve a "View docs" that was waiting on the index.
                if let Some(name) = self.docs.pending_view.take() {
                    match find_doc_by_name(&self.docs.files, &name) {
                        Some((rel, line)) => self.open_doc_page(&rel, line),
                        None => self.status = format!("No docs for “{name}”"),
                    }
                }
            }
            Event::StateContent {
                root: state_root,
                rel,
                text,
            } => {
                // A remote project's `.clew/` session state, read where the
                // project lives. Applied only for the project it names, and
                // only on a remote connection (local projects load their own
                // files directly).
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if root.to_string_lossy() != state_root || !self.connection.is_remote() {
                    return Task::none();
                }
                // No longer outstanding, whatever happens below.
                self.remote_state_pending.remove(&rel);
                // This client holds a change that is not known to be on the
                // remote's disk. Their version wins — assigning the loaded one
                // here would silently revert the action they just took, which
                // is exactly how a reconnect used to erase a session's
                // bookmarks.
                if self.remote_state_dirty.contains(&rel) {
                    // With an edit already on its way to the server, this read
                    // describes the file BEFORE it, and the merged file is
                    // about to arrive as `StateEdited`. Re-flushing this
                    // window's snapshot over it would undo exactly the merge
                    // that edit exists to get.
                    //
                    // Unless the mark ALSO covers a change the server never
                    // received (`remote_state_unsent`, stamped when the
                    // transport carrying it died). That one is in no merge —
                    // the in-flight edit's reply carries the file WITHOUT it —
                    // so skipping here is what made it disappear for good. Flush
                    // this window's copy, which holds both changes, at the
                    // wholesale write's known cost (see `flush_remote_state`);
                    // it also supersedes the in-flight edit, so the reply that
                    // lacks the change can no longer be adopted over it.
                    if !self.remote_state_edit_inflight(&rel)
                        || self.remote_state_unsent.contains(&rel)
                    {
                        self.flush_remote_state(&rel);
                    }
                    return Task::none();
                }
                if let Some(text) = text {
                    self.adopt_remote_state(&root, &rel, &text);
                }
                // A missing file (or an unknown rel) keeps the defaults.
            }
            Event::ProjectSymbols {
                root: snap_root,
                seq,
                full,
                files,
                go_module,
                dart_package,
                structure,
            } => {
                // The server-extracted index data. Applied only for REMOTE
                // projects: a local project builds its own richer, cached
                // index, and the joined paths here are pure identities —
                // nothing ever reads them from this machine's disk.
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if root.to_string_lossy() != snap_root || !self.connection.is_remote() {
                    return Task::none();
                }
                // Publication order: a full snapshot built during the scan
                // can land AFTER partials the watcher sent while it was
                // building — applying it would clear those fresher files.
                if seq <= self.remote_index_seq {
                    return Task::none();
                }
                self.remote_index_seq = seq;
                if full {
                    self.symbol_index_by_file.clear();
                    // A full snapshot restates the whole file set, so the
                    // change registry is rebuilt from it below rather than
                    // patched: an entry for a file this publication no longer
                    // lists describes a file that is gone, and keeping it
                    // would leave the derived caches keyed on it.
                    self.registry.clear();
                }
                // Resolution metadata and the type/trait structure index are
                // both extracted where the files live, and arrive as a
                // `Patch`: `Unchanged` means keep what we hold, `Set(None)`
                // means the value is GONE. The two metadata halves are
                // patched INDIVIDUALLY — they share one slot, and replacing
                // the pair wholesale wiped whichever half this publication
                // did not recompute.
                let (mut go, mut dart) = self.remote_import_meta.clone().unwrap_or((None, None));
                let mut meta_changed = false;
                if let clew_protocol::Patch::Set(v) = go_module {
                    go = v;
                    meta_changed = true;
                }
                if let clew_protocol::Patch::Set(v) = dart_package {
                    dart = v;
                    meta_changed = true;
                }
                if meta_changed {
                    self.remote_import_meta = Some((go, dart));
                }
                if let clew_protocol::Patch::Set(s) = &structure {
                    self.structure = s
                        .as_deref()
                        .and_then(|s| serde_json::from_str(s).ok())
                        .unwrap_or_default();
                }
                let mut raw_imports: std::collections::HashMap<PathBuf, Vec<imports::RawImport>> =
                    std::collections::HashMap::new();
                for fs in files {
                    let abs = root.join(&fs.rel);
                    // Change detection for a REMOTE project. This client never
                    // sees the file's bytes, so the registry cannot hold their
                    // hash; what it records instead is the publication that
                    // last reported the file. `seq` only grows and the server
                    // republishes a file exactly when its watcher saw that file
                    // change, so a real change gives the file a new version and
                    // bumps the revision once — and the revision is the
                    // freshness key `stats.rev` and `project_calls.rev` are
                    // compared against. Without it, a remote edit refreshed the
                    // sidebar's symbols while Stats and Project Calls went on
                    // serving pre-edit results, and only the edits that
                    // happened to land in an OPEN file ever invalidated them.
                    //
                    // An empty entry for a rel the tree no longer lists is a
                    // deletion (the same test the import graph uses below). The
                    // membership scan is skipped for a full snapshot, which has
                    // just cleared the registry and lists everything that
                    // exists — and is far too big to scan per file.
                    if !full
                        && fs.symbols.is_empty()
                        && fs.imports.is_empty()
                        && !self
                            .project
                            .as_ref()
                            .is_some_and(|p| p.files.iter().any(|f| f.abs == abs))
                    {
                        self.registry.remove(&abs);
                    } else {
                        self.registry.set(abs.clone(), seq);
                    }
                    raw_imports.insert(
                        abs.clone(),
                        fs.imports
                            .iter()
                            .map(|i| imports::RawImport {
                                module: i.module.clone(),
                                line: i.line,
                                is_mod_decl: i.is_mod,
                            })
                            .collect(),
                    );
                    if fs.symbols.is_empty() {
                        self.symbol_index_by_file.remove(&abs);
                    } else {
                        let entries: Vec<index::SymbolEntry> = fs
                            .symbols
                            .iter()
                            .map(|s| index::SymbolEntry {
                                name: s.name.clone(),
                                kind: s.kind.clone(),
                                rel: fs.rel.clone(),
                                abs: abs.clone(),
                                line: s.line,
                                is_test: s.is_test,
                            })
                            .collect();
                        self.symbol_index_by_file.insert(abs, entries);
                    }
                }
                self.rebuild_symbol_index();
                if full {
                    // Build the import graph from the snapshot's extraction,
                    // resolved over the (identity-only) file set, and lay
                    // out the overview map now that edges exist.
                    self.rebuild_import_graph(raw_imports);
                    self.indexing = false;
                    return self.refresh_overview_map();
                }
                // Partial update: refresh the changed files' out-edges.
                let mut file_set_changed = false;
                if let Some(resolver) = self.import_resolver() {
                    let mut graph_dirty = false;
                    for (abs, raw) in raw_imports {
                        if raw.is_empty()
                            && !self
                                .project
                                .as_ref()
                                .is_some_and(|p| p.files.iter().any(|f| f.abs == abs))
                        {
                            self.import_graph.remove_file(&abs);
                            graph_dirty = true;
                            file_set_changed = true;
                        } else {
                            let known = self.import_graph.files().iter().any(|f| f == &abs);
                            file_set_changed |= !known;
                            graph_dirty |=
                                self.import_graph
                                    .set_file(abs, raw, &resolver, highlight::detect);
                        }
                    }
                    if graph_dirty {
                        self.import_cycles = self.import_graph.cycles();
                        self.refresh_import_tree();
                    }
                }
                // Creating or deleting a file changes how OTHER files'
                // specifiers resolve (a new `mod`/module target, a deleted
                // one), so the whole edge set is re-resolved — patching only
                // the changed files left every other file pointing at the
                // old resolution until the project was reopened. Pure
                // in-memory work over the identity-only file list.
                if file_set_changed || meta_changed {
                    self.reresolve_import_graph();
                }
            }
            Event::FilesChanged {
                root: changed_root,
                rels,
            } => {
                // The server's watcher reports on-disk changes. A late
                // notification from a watcher for a project we have already
                // left must not be applied under the new root.
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if root.to_string_lossy() != changed_root {
                    return Task::none();
                }
                // Keep the API docs fresh while their tab is open (the docs
                // build runs on the server, so this works for both targets).
                // Deliberately NOT `ensure_docs`: the registry has not learned
                // of this change yet — locally the hash lands with
                // `FilesRehashed`, remotely with the next `ProjectSymbols` — so
                // a freshness test asked here would call the pre-edit index
                // current and skip the rebuild the visible tab needs. The cost
                // is that this build stamps the pre-bump revision and so reads
                // as stale afterwards, buying one extra rebuild the next time
                // the tab is opened. Edits made while another tab is visible
                // are caught by that same tab-entry test.
                if self.sidebar == SidebarTab::Docs && !self.docs.loading {
                    self.request_docs();
                }
                if self.connection.is_remote() {
                    // Remote: re-request any changed file we still hold a copy
                    // of — in a pane, or in a language server's document
                    // overlay — so the reply can reload the view and resync
                    // the server (`apply_file_refresh` does both). The server
                    // reads it where it lives. Nothing here may read a
                    // remote-pathed file from the local disk.
                    // The index and the graphs are re-derived where the files
                    // live and arrive as a `ProjectSymbols` publication, which
                    // is also what advances the change registry (so Stats and
                    // Project Calls invalidate); the explanations are aged
                    // below, since no server event does that for us.
                    let open: HashSet<PathBuf> =
                        self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
                    for rel in &rels {
                        // `lsp_opened` too, not just `open`: the language
                        // server keeps a document from didOpen until a
                        // didClose clew never sends, so a file that has left
                        // the pane still needs its bytes, or every position it
                        // answers about stays pinned to the text as it was
                        // when the file was first opened.
                        let abs = root.join(rel);
                        if open.contains(&abs) || self.lsp_opened.contains(&abs) {
                            self.request_file_refresh(rel);
                        }
                    }
                    // A changed source file ages the understanding
                    // (explanations → semantic index → overview) here exactly
                    // as it does at the end of `on_files_rehashed`. The pass
                    // fetches its sources over the protocol, so it reads no
                    // local file; being unreachable from this branch is why a
                    // remote project's explanations only ever refreshed by
                    // hand. Throttled, so an edit burst coalesces into one
                    // pass (see `request_auto_refresh`).
                    if rels
                        .iter()
                        .any(|rel| highlight::detect(&root.join(rel)).is_some())
                    {
                        task = self.request_auto_refresh();
                    }
                } else {
                    // Local server: the watcher's paths are this machine's
                    // files, so run the FULL derived-state pipeline —
                    // registry, symbol index, import graph, call graphs,
                    // trail re-anchoring, and the throttled explanation /
                    // overview auto-refresh. It also reloads open panes in
                    // place. Without this, an edited import or a new file
                    // left every graph and explanation stale until the
                    // project was reopened. The remote branch above upholds
                    // the same contract by other means: the server re-derives
                    // the index and graphs and publishes them, and the two
                    // pieces it cannot publish (the registry bump, the
                    // explanation refresh) are driven from there.
                    task = self.on_files_changed(rels.iter().map(|rel| root.join(rel)).collect());
                }
            }
            Event::Tree {
                root: tree_root,
                tree,
                files,
                ..
            } => {
                // A structural change (create/delete) from the watcher.
                self.splice_tree(&tree_root, tree, files);
            }
            Event::ProcessOutput { proc, data } => {
                // Feed a proxied process's stdout into its LspClient bridge.
                if let Some(feed) = self.proc_feeds.get(&proc) {
                    let _ = feed.send(data);
                }
            }
            Event::ProcessExited { proc, code } => {
                // Dropping the feed closes the bridge, so the LspClient sees EOF.
                self.proc_feeds.remove(&proc);
                // Only a proc STILL mapped to a language is that language's live
                // server: a deliberate restart drops the mapping before killing
                // the old child (`start_lsp_with`), so the killed predecessor's
                // late exit finds nothing here and cannot tear down the
                // successor that already replaced it.
                let dead: Vec<String> = self
                    .lsp_procs
                    .iter()
                    .filter(|(_, p)| **p == proc)
                    .map(|(lang, _)| lang.clone())
                    .collect();
                self.lsp_procs.retain(|_, p| *p != proc);
                for language in dead {
                    // Back to "not started" rather than an immediate respawn: a
                    // server that just died (own crash, or the server's
                    // stdin-overflow kill) would very likely die again, and a
                    // restart loop is worse than none. The next `ensure_lsp` —
                    // the next file open or LSP action — brings it back, and
                    // until then the slot must not keep handing out a client
                    // whose every request fails.
                    self.reset_lsp(&language);
                    self.status = match code {
                        Some(c) => format!(
                            "{language} language server exited (code {c}). It restarts on the next request."
                        ),
                        None => format!(
                            "{language} language server exited. It restarts on the next request."
                        ),
                    };
                }
            }
            // Other flows (Outline, …) handled here as they migrate.
            _ => {}
        }
        task
    }

    /// Swap a server-sent file list into the project that is already open,
    /// keeping panes, scroll and every derived artifact. Shared by the
    /// watcher's structural notification and the resync `OpenProject` reply, so
    /// the same snapshot lands the same way whichever way it arrives. The
    /// snapshot names the project the server holds — one from a project we've
    /// already left must not splice its file list under the current root.
    fn splice_tree(
        &mut self,
        tree_root: &str,
        tree: clew_protocol::DirNode,
        files: Vec<clew_protocol::Rel>,
    ) {
        if let Some(project) = &mut self.project
            && project.root.to_string_lossy() == tree_root
        {
            let root = project.root.clone();
            project.tree = tree;
            project.files = Arc::new(
                files
                    .into_iter()
                    .map(|rel| fs_scan::FileEntry {
                        abs: root.join(&rel),
                        rel,
                    })
                    .collect(),
            );
        }
    }

    /// Route a correlated server reply. `FileContent` needs the request id to
    /// find which pane asked for it; everything else is id-agnostic.
    /// An AI router for background tasks. Endpoint is Server (matching the Hello
    /// handshake); with no server channel it transparently runs calls locally.
    pub(crate) fn ai_client(&self) -> AiClient {
        AiClient {
            endpoint: self.ai_endpoint(),
            server_tx: self.server_tx.clone(),
            next_id: self.next_req_id.clone(),
            pending: self.ai_pending.clone(),
        }
    }

    /// Whether the connected server may hold the AI keys and run AI calls.
    /// Local: yes — the server is this machine, the keys never travel.
    /// Remote: only with the per-host opt-in granted in the Connect form;
    /// otherwise every AI call runs on the client and no key crosses SSH.
    pub(crate) fn ai_on_server(&self) -> bool {
        !self.connection.is_remote() || self.remote_ai_opt_in
    }

    /// The endpoint AI calls should use, per [`Self::ai_on_server`].
    pub(crate) fn ai_endpoint(&self) -> clew_protocol::AiEndpoint {
        if self.ai_on_server() {
            clew_protocol::AiEndpoint::Server
        } else {
            clew_protocol::AiEndpoint::Client
        }
    }

    /// Hand the server the current AI provider config so it can make calls.
    /// For a remote host this is gated on the per-host opt-in: API keys are
    /// credentials, and a host the user hasn't explicitly trusted with them
    /// must never see them.
    pub(crate) fn send_ai_config(&self) {
        let Some(tx) = &self.server_tx else { return };
        // Sent UNCONDITIONALLY, including as a pair of `None`s. The two cases
        // that most need to reach the server are exactly the two that used to
        // send nothing: the user deleted their API keys, and the user revoked
        // this host's permission to hold them. Both left the server holding —
        // and free to keep using — the old credentials.
        let (chat, embed) = if self.ai_on_server() {
            (
                llm::Config::load().map(|c| clew_protocol::AiChatConfig {
                    provider: c.provider.slug().to_string(),
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                }),
                embed::Config::load().map(|c| clew_protocol::AiEmbedConfig {
                    api_key: c.api_key,
                    model: c.model,
                    base_url: c.base_url,
                }),
            )
        } else {
            (None, None)
        };
        let _ = tx.send(clew_protocol::ClientMessage {
            id: 0,
            request: clew_protocol::Request::SetAiConfig { chat, embed },
        });
    }

    pub(crate) fn handle_server_reply(
        &mut self,
        id: u64,
        event: clew_protocol::Event,
    ) -> Task<Message> {
        // An AI RPC reply: hand the event to the task awaiting it.
        if let Some(otx) = self.ai_pending.lock().unwrap().remove(&id) {
            let result = match event {
                clew_protocol::Event::Error { message } => Err(message),
                other => Ok(other),
            };
            let _ = otx.send(result);
            return Task::none();
        }
        match event {
            // The Hello reply. A server speaking another protocol version
            // can't be used: its frames would fail to deserialize and
            // silently vanish (a remote open then waits forever). Newer
            // servers refuse in their Hello reply; this covers OLDER ones,
            // which happily answer Ready with their own version.
            clew_protocol::Event::Ready {
                protocol,
                fingerprint,
            } => {
                if protocol != clew_protocol::PROTOCOL_VERSION {
                    return self.on_handshake_failed(format!(
                        "clew-server speaks protocol v{protocol}, this clew speaks v{} — \
                         update the server (local: rebuild; remote: it redeploys on reconnect)",
                        clew_protocol::PROTOCOL_VERSION
                    ));
                }
                // Same version number, different protocol BUILD (a wire change
                // whose bump was missed, or a stale sibling/dev binary): its
                // frames would deserialize wrongly or not at all. Refuse now,
                // as one clear error, instead of a session of silent drops.
                if fingerprint != clew_protocol::SCHEMA_FINGERPRINT {
                    return self.on_handshake_failed(format!(
                        "clew-server was built from different protocol sources (server {}, \
                         this clew {}) — rebuild the server (remote: reconnect to redeploy)",
                        fingerprint,
                        clew_protocol::SCHEMA_FINGERPRINT
                    ));
                }
                // The handshake is internal — don't surface version jargon in
                // the status bar; stay quiet until there's something to say.
                self.status.clear();
                self.on_server_ready()
            }
            clew_protocol::Event::FileContent {
                rel,
                source,
                lines,
                symbols,
                docs,
                inactive,
            } => match self.pending_reads.remove(&id) {
                // Apply an open only while the pane still waits for this exact
                // load; a later open (or a project switch, which clears the
                // tokens) supersedes it.
                Some(ReadKind::Open { pane, target })
                    if self.pane_pending.get(pane).copied().flatten() == Some(id) =>
                {
                    self.pane_pending[pane] = None;
                    self.apply_file_content(
                        pane, target, rel, source, lines, symbols, docs, inactive,
                    )
                }
                Some(ReadKind::Refresh { .. }) => {
                    self.apply_file_refresh(rel, source, lines, symbols, docs, inactive)
                }
                _ => Task::none(),
            },
            clew_protocol::Event::NotebookContent {
                rel,
                language,
                cells,
                symbols,
                projection,
            } => match self.pending_reads.remove(&id) {
                Some(ReadKind::Open { pane, target })
                    if self.pane_pending.get(pane).copied().flatten() == Some(id) =>
                {
                    self.pane_pending[pane] = None;
                    self.apply_notebook_content(
                        &[pane],
                        target,
                        rel,
                        language,
                        cells,
                        symbols,
                        projection,
                        false,
                    )
                }
                Some(ReadKind::Refresh { .. }) => {
                    // Reload in place: rebuild EVERY pane showing this notebook,
                    // exactly as `apply_file_refresh` does for a plain file.
                    // Taking only the first match left the other half of a split
                    // painting the pre-edit cells for the rest of the session:
                    // the watcher sends ONE re-read per changed file, and
                    // nothing else ever rebuilds `v.notebook` — only a fresh
                    // open into that pane replaces it. `refresh` keeps each
                    // pane's own scroll and expanded outputs and skips the
                    // open-time side effects.
                    let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                        return Task::none();
                    };
                    let abs = root.join(&rel);
                    let targets: Vec<usize> = self
                        .panes
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.as_ref().is_some_and(|v| v.abs == abs))
                        .map(|(i, _)| i)
                        .collect();
                    if targets.is_empty() {
                        return Task::none();
                    }
                    self.apply_notebook_content(
                        &targets, None, rel, language, cells, symbols, projection, true,
                    )
                }
                _ => Task::none(),
            },
            clew_protocol::Event::Tree {
                root: tree_root,
                tree,
                files,
                truncated,
            } => {
                // Only build the project while we're opening one; a Tree that
                // arrives otherwise answers the OpenProject a (re)connect (or a
                // local-fallback open) re-sent for the project already on
                // screen, and must not re-open it. Discarding it outright was
                // wrong too: on a reconnect this reply is the only report of
                // what changed while the link was down, since the watcher
                // starts from the current state and reports only later events.
                // Splice it in, keeping panes, scroll and Ask history.
                if !self.scanning {
                    if std::mem::take(&mut self.pending_tree_resync) {
                        self.splice_tree(&tree_root, tree, files);
                    }
                    return Task::none();
                }
                let Some(root) = self.pending_scan_root.take() else {
                    return Task::none();
                };
                // The reply must describe the project we are waiting for.
                if root.to_string_lossy() != tree_root {
                    self.pending_scan_root = Some(root);
                    return Task::none();
                }
                let files = files
                    .into_iter()
                    .map(|rel| fs_scan::FileEntry {
                        abs: root.join(&rel),
                        rel,
                    })
                    .collect();
                self.on_scan_done(ScanResult {
                    root,
                    tree,
                    files,
                    truncated,
                })
            }
            clew_protocol::Event::LspResolved {
                language,
                root: resolved_root,
                resolution,
            } => {
                // Reply to the remote ensure_lsp (or a finished remote
                // install): each resolution state demands its own action —
                // start, raise the approval modal, raise the install-consent
                // modal, or give up with the server's reason.
                if !matches!(self.lsp.get(&language), Some(LspSlot::AwaitingConsent)) {
                    return Task::none(); // superseded (project switch, restart)
                }
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                // A resolution computed for another project (a late reply
                // that straddled an A→B switch) must not drive THIS
                // project's approval or install consent.
                if resolved_root != root.to_string_lossy() {
                    return Task::none();
                }
                let host = self.connection.approval_host().map(str::to_string);
                use clew_protocol::LspResolution;
                match resolution {
                    LspResolution::Ready {
                        init_options,
                        withheld,
                    } => {
                        // Stashed first, and with the APPROVED options only:
                        // when something was withheld this is `None`, which
                        // also drops any entry an earlier resolve left, so a
                        // start can never pick up options the server refused.
                        self.stash_remote_init(&language, init_options);
                        // The host's lsp.toml asks for options it has not been
                        // approved for. Ask, with the same modal the `Command`
                        // arm and the local options-only path use — the slot
                        // stays `AwaitingConsent`, so the reply to the resolve
                        // that Allow re-issues is not dropped as superseded.
                        //
                        // Asked every time rather than short-circuiting on an
                        // approval this client already holds: the server is the
                        // one that decides, and re-pushing plus re-resolving on
                        // its refusal is a loop with no bound. One extra click
                        // repairs a desync (the allow re-sends the whole set).
                        if let Some(spec) = withheld
                            && let Some(shown) =
                                serde_json::from_str::<serde_json::Value>(&spec.options)
                                    .ok()
                                    .and_then(|v| {
                                        crate::app::services::pretty_init_options(Some(&v))
                                    })
                        {
                            self.pending_lsp_command = Some(PendingLspCommand {
                                root,
                                host,
                                language,
                                // No repo-named command: what runs is the
                                // host's store-installed server, covered by
                                // the install consent. The question here is
                                // about the options alone.
                                command: None,
                                args: spec.args,
                                server_name: spec.server,
                                version: spec.version,
                                fingerprint: spec.fingerprint,
                                init_options: Some(shown),
                            });
                            return Task::none();
                        }
                        // Either nothing was withheld, or the options came back
                        // unrenderable — and a modal with nothing in it is not
                        // consent. Fall back to starting without them, which is
                        // what the server already told the status bar it did.
                        self.lsp.remove(&language);
                        // The exe path is unused on the remote spawn path.
                        self.start_lsp_with(&language, PathBuf::new())
                    }
                    LspResolution::Command(spec) => {
                        if self.trust.is_lsp_approved(
                            host.as_deref(),
                            &root,
                            &language,
                            &spec.fingerprint,
                        ) {
                            // Already approved: refresh the server's set, start.
                            self.stash_remote_init(&language, spec.init_options);
                            self.send_lsp_approvals();
                            self.lsp.remove(&language);
                            self.start_lsp_with(&language, PathBuf::new())
                        } else {
                            // Shown before stashing: the remote's options ride
                            // this same fingerprint, so they are part of what
                            // is being approved and must be visible.
                            let shown = spec
                                .init_options
                                .as_deref()
                                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
                            self.stash_remote_init(&language, spec.init_options);
                            self.pending_lsp_command = Some(PendingLspCommand {
                                root,
                                host,
                                language,
                                command: Some(PathBuf::from(&spec.command)),
                                args: spec.args,
                                server_name: spec.server,
                                version: spec.version,
                                fingerprint: spec.fingerprint,
                                init_options: crate::app::services::pretty_init_options(
                                    shown.as_ref(),
                                ),
                            });
                            Task::none()
                        }
                    }
                    LspResolution::NeedsInstall {
                        server,
                        version,
                        describe,
                    } => {
                        // Slot stays AwaitingConsent; on Allow the client
                        // sends `LspInstall` and the reply lands right here.
                        self.pending_lsp_consent = Some(LspConsent {
                            language,
                            server_name: server,
                            version,
                            provision: LspProvision::Remote { describe },
                            dest_dir: PathBuf::new(),
                        });
                        Task::none()
                    }
                    LspResolution::Unsupported { message } => {
                        self.lsp.insert(language, LspSlot::Unsupported(message));
                        Task::none()
                    }
                }
            }
            clew_protocol::Event::SearchResults { hits, error } => {
                // A search reply: apply only while it is still the latest
                // submission (a newer one replaced `pending_search`).
                if self.pending_search != Some(id) {
                    return Task::none();
                }
                self.pending_search = None;
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let hits = hits
                    .into_iter()
                    .map(|h| search::SearchHit {
                        abs: root.join(&h.rel),
                        rel: h.rel,
                        line: h.line,
                        preview: h.preview,
                    })
                    .collect();
                self.apply_search_result(search::SearchResult { hits, error });
                Task::none()
            }
            // Blame for the file this request named. Re-deriving the path
            // from the current root and the reply's `rel` would, after a
            // project switch, paint a DIFFERENT project's same-named file.
            clew_protocol::Event::GitInfo { info, .. } => {
                let Some(abs) = self.pending_git.remove(&id) else {
                    return Task::none();
                };
                self.on_git_info_loaded(abs, info.map(Arc::new))
            }
            // The folder picker's listing, applied only while it is the one
            // being waited for: two quick clicks used to let the slower,
            // earlier reply overwrite the newer directory.
            clew_protocol::Event::DirListing {
                path,
                parent,
                entries,
            } => {
                if self.pending_list_dir != Some(id) {
                    return Task::none();
                }
                self.pending_list_dir = None;
                if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.cwd = path;
                    b.parent = parent;
                    b.entries = entries;
                    b.loading = false;
                }
                Task::none()
            }
            // A refusal correlated to a tracked request (e.g. the server's
            // not-ready answer during its scan window): stop the matching
            // spinner — the generic Error arm only sets the status line, and
            // the panels would otherwise load forever.
            // The bytes reached the remote's disk. Only now is the change
            // durable, so only now may its unsaved mark come off.
            clew_protocol::Event::StateWritten { rel, .. } => {
                // Keyed on the in-flight id, not the rel alone: a NEWER write
                // of the same file supersedes this one and owns the mark, so a
                // late acknowledgement must not clear it.
                if self.remote_state_inflight.remove(&id).as_deref() == Some(rel.as_str()) {
                    self.remote_state_dirty.remove(&rel);
                }
                // The unsent mark is retired on its OWN record, not on the one
                // above: these are the bytes this window holds, so a change an
                // earlier transport ate is in them and the remote file is whole
                // again — and that stays true even when an edit made inside the
                // round trip has since taken the dirty mark's ownership away
                // from this id. Reading it off the id gate above left the mark
                // set forever in exactly that order, which froze every later
                // merge for this store (see `remote_state_rescue`). The dirty
                // mark is correctly left to that newer edit, whose own reply
                // the ordered state worker sends after this one — and which
                // clears it, unless it is refused, in which case the mark
                // stands and the next re-read flushes it, as for any other
                // failed write.
                if self.remote_state_rescue.remove(&id).as_deref() == Some(rel.as_str()) {
                    self.remote_state_unsent.remove(&rel);
                }
                Task::none()
            }
            // The merged file, after the server replayed this client's change
            // on what was actually on the remote's disk. It — not this
            // window's copy — is the truth, the same way the local
            // `bookmarks::edit` returns the merged list its caller adopts.
            clew_protocol::Event::StateEdited {
                root: state_root,
                rel,
                text,
            } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                if root.to_string_lossy() != state_root || !self.connection.is_remote() {
                    return Task::none();
                }
                // Keyed on the in-flight id for the same reason as above, and
                // the ADOPTION is inside the same gate: a reply that no longer
                // owns the rel was superseded by a newer edit of the same file,
                // so it describes that file BEFORE the newer edit. Adopting it
                // rolled this window's copy back below its own optimistic
                // change — the entry the user had just made vanished from the
                // list until the newer reply landed, and a re-press inside that
                // window sent a TOGGLE that the server applied to its (newer)
                // truth and deleted the entry outright. Worse, if the link died
                // in the gap, the rolled-back copy is what the reconnect's
                // `flush_remote_state` wrote wholesale over the server's file.
                //
                // Dropping the superseded reply loses nothing: the state worker
                // applies edits in order, so the newest reply carries the
                // cumulative merge, including whatever another client wrote.
                // Same shape as the `owns_result` / registry-CAS staleness
                // guards elsewhere — the newest writer owns the state.
                if self.remote_state_inflight.remove(&id).as_deref() == Some(rel.as_str()) {
                    // A merge computed WITHOUT a change the server never
                    // received is not this file's truth. Adopting it drops that
                    // change from this window's list — the last copy of it —
                    // and clearing the mark throws away the record that it is
                    // still missing from the remote's disk. Keep both and let
                    // the re-read's flush carry it (`StateContent` above).
                    //
                    // The mark is still set here whenever the rescue flush has
                    // not been acknowledged yet: this reply overtook the
                    // re-read that would have produced the flush, or that
                    // flush failed. It is NOT set for an edit made after the
                    // flush went out — the ordered state worker answers the
                    // flush first, and that acknowledgement retires the mark
                    // (`StateWritten` above), so this merge, computed on the
                    // rescued file, is adopted like any other.
                    if self.remote_state_unsent.contains(&rel) {
                        return Task::none();
                    }
                    self.remote_state_dirty.remove(&rel);
                    // `None` = the merge emptied the store and its file was
                    // deleted, which for every mergeable store is an empty list.
                    self.adopt_remote_state(&root, &rel, text.as_deref().unwrap_or("[]"));
                }
                Task::none()
            }
            clew_protocol::Event::Error { message } => {
                // The refusal a mismatched handshake actually produces: the
                // server checks OUR version and fingerprint first, so it
                // answers `Error` instead of the `Ready` the arm above
                // inspects — and then refuses every later request too. Take
                // the same fallback, or the parked scan is stranded and the
                // window never opens a project again.
                if id == crate::app::handlers_features::HELLO_REQ_ID {
                    return self.on_handshake_failed(message);
                }
                // A refused blame has no reply to reap its entry.
                self.pending_git.remove(&id);
                // A write that failed stays dirty: the change is still only in
                // this client, so the next re-read must not overwrite it. Same
                // for the unsent mark, whose rescue record dies with the
                // request that would have retired it — the refused bytes never
                // reached the disk, so the change is still missing from it.
                self.remote_state_inflight.remove(&id);
                self.remote_state_rescue.remove(&id);
                let mut correlated = false;
                if self.pending_search == Some(id) {
                    self.pending_search = None;
                    self.search.running = false;
                    self.search.error = Some(message.clone());
                    correlated = true;
                }
                if self.pending_docs == Some(id) {
                    self.pending_docs = None;
                    self.docs.loading = false;
                    // The refusal is the whole reply: no index arrives, so the
                    // revision this build was requested at must not stay
                    // stamped on the older index still on screen.
                    self.docs.rev = DOCS_REV_STALE;
                    // The "View docs" this build was carrying dies with it.
                    // `Event::Docs` is the ONLY consumer of the parked name, so
                    // leaving it set aimed it at the next SUCCESSFUL build of
                    // this project — a sidebar visit or an edit-triggered
                    // rebuild minutes later opened the doc page over whatever
                    // the reader had in the pane, for a request this client had
                    // already reported as refused.
                    self.docs.pending_view = None;
                    correlated = true;
                }
                if self.pending_list_dir == Some(id) {
                    self.pending_list_dir = None;
                    if let Some(ConnectStage::Browsing(b)) =
                        self.connect.as_mut().map(|u| &mut u.stage)
                    {
                        b.loading = false;
                    }
                    correlated = true;
                }
                if correlated {
                    self.status = message;
                    return Task::none();
                }
                self.handle_server_event(clew_protocol::Event::Error { message })
            }
            other => self.handle_server_event(other),
        }
    }

    /// Rough pixel height of one rendered notebook cell, for scroll estimation.
    /// (Cells have variable height; goto scrolls near the cell and the target
    /// ring points precisely.)
    fn estimate_nb_cell_height(cell: &NbCell, expanded: bool, line_height: f32) -> f32 {
        let body = match cell.kind.as_str() {
            "code" => cell.lines.len().max(1) as f32 * (line_height + 1.0) + 34.0,
            _ => cell.source.lines().count().max(1) as f32 * 22.0 + 16.0,
        };
        let outputs = if cell.outputs.is_empty() {
            0.0
        } else if expanded {
            cell.outputs
                .iter()
                .map(|o| match o {
                    NbOutput::Text { spans, .. } => {
                        let lines: usize = spans
                            .iter()
                            .map(|(t, _)| t.matches('\n').count())
                            .sum::<usize>()
                            + 1;
                        lines.min(1_000) as f32 * 17.0 + 12.0
                    }
                    NbOutput::Image(_) | NbOutput::Svg(_) => 332.0,
                    NbOutput::Placeholder(_) => 24.0,
                })
                .sum::<f32>()
                + 26.0
        } else {
            26.0
        };
        body + outputs + 14.0
    }

    /// Estimated scroll offset that brings the cell containing projection
    /// `line` near the top of the notebook view.
    pub(crate) fn estimate_notebook_offset(
        doc: &NotebookDoc,
        expanded: &std::collections::HashSet<usize>,
        line: usize,
        line_height: f32,
    ) -> f32 {
        let target = doc
            .cells
            .iter()
            .rposition(|c| c.proj_line <= line)
            .unwrap_or(0);
        let y: f32 = doc
            .cells
            .iter()
            .enumerate()
            .take(target)
            .map(|(i, c)| Self::estimate_nb_cell_height(c, expanded.contains(&i), line_height))
            .sum();
        (y - 40.0).max(0.0)
    }

    /// Apply a `NotebookContent` reply: build the render-ready cell doc (markdown
    /// through the richmd pipeline, outputs into image/svg handles) and mount a
    /// viewer whose text is the script projection — so search hits, the outline,
    /// and goto all speak projection lines.
    ///
    /// `panes` is every pane the doc must be mounted into: an open names one, a
    /// refresh names all the panes showing the file (a split shows the same
    /// notebook twice). The cells are parsed once and shared; only the
    /// scroll/expanded state kept across a refresh is per pane.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_notebook_content(
        &mut self,
        panes: &[usize],
        target: Option<usize>,
        rel: String,
        language: String,
        cells: Vec<clew_protocol::NotebookCell>,
        symbols: Vec<Symbol>,
        projection: String,
        // refresh = a watcher-triggered reload of the open notebook: keep
        // scroll and expanded outputs, and skip the open-time side effects
        // (status, leaving the Docs page).
        refresh: bool,
    ) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        if !refresh {
            self.docs.page = None;
        }
        let abs = root.join(&rel);
        // Prepare markdown cells first (this may spawn math/mermaid renders).
        let mut tasks: Vec<Task<Message>> = Vec::new();
        let mut prepared: Vec<NbCell> = Vec::new();
        for c in cells {
            let (segs, lines) = match c.kind.as_str() {
                "code" => (Vec::new(), c.lines),
                _ => {
                    let (segs, task) = self.prepare_segments(&c.source);
                    tasks.push(task);
                    (segs, Vec::new())
                }
            };
            let outputs = c
                .outputs
                .into_iter()
                .map(|o| match o {
                    clew_protocol::NotebookOutput::Text { spans, stderr } => {
                        NbOutput::Text { spans, stderr }
                    }
                    clew_protocol::NotebookOutput::Image { data } => {
                        NbOutput::Image(iced::widget::image::Handle::from_bytes(data))
                    }
                    clew_protocol::NotebookOutput::Svg(svg) => {
                        NbOutput::Svg(iced::widget::svg::Handle::from_memory(svg.into_bytes()))
                    }
                    clew_protocol::NotebookOutput::Placeholder(label) => {
                        NbOutput::Placeholder(label)
                    }
                })
                .collect();
            prepared.push(NbCell {
                kind: c.kind,
                source: c.source,
                segs,
                lines,
                proj_line: c.proj_line,
                outputs,
                execution_count: c.execution_count,
            });
        }
        // Single-threaded UI state; Arc only for cheap clones into iced
        // widgets (the prepared markdown items are not Sync).
        #[allow(clippy::arc_with_non_send_sync)]
        let doc = std::sync::Arc::new(NotebookDoc {
            language,
            cells: prepared,
        });

        let source = Arc::new(projection);
        let lines = highlight::plain_lines(&source);
        if !refresh {
            self.status = rel.clone();
        }
        // The pane's document is being replaced: any hover in flight is
        // about the file that was there.
        self.invalidate_hover();
        let mut active_mounted = false;
        for &pane in panes {
            // Each pane keeps ITS own scroll and expanded outputs across a
            // refresh; the two halves of a split are read at different places.
            let Some(slot) = self.panes.get(pane) else {
                continue;
            };
            let old = slot
                .as_ref()
                .map(|v| (v.viewport_h, v.scroll_y, v.nb_expanded.clone()));
            let mut v = Viewer::new(
                abs.clone(),
                rel.clone(),
                None,
                source.clone(),
                lines.clone(),
            );
            v.symbols = symbols.clone();
            v.highlighted = true;
            v.notebook = Some(doc.clone());
            if let Some((h, old_scroll, old_expanded)) = old {
                v.viewport_h = h;
                if refresh {
                    v.nb_expanded = old_expanded;
                    v.scroll_y = old_scroll;
                }
            }
            v.target_line = target;
            // The cell view has variable-height cells, so a goto scrolls to an
            // estimate of the target cell's offset; the highlight ring on the
            // cell (drawn by the view for `target_line`) does the precise
            // pointing. A refresh keeps the reader where they were instead.
            let y = if refresh {
                v.scroll_y
            } else {
                target
                    .map(|line| {
                        Self::estimate_notebook_offset(
                            &doc,
                            &v.nb_expanded,
                            line,
                            self.line_height(),
                        )
                    })
                    .unwrap_or(0.0)
            };
            v.scroll_y = y;
            self.panes[pane] = Some(v);
            active_mounted |= pane == self.active;
            tasks.push(operation::scroll_to(
                ui::code_scroll_id(pane),
                AbsoluteOffset { x: 0.0, y },
            ));
        }
        // The projection's hash stands in for the notebook's bytes — local
        // only, for the reason given in `apply_file_content`.
        if self.local_project_state() {
            self.registry
                .set(abs, incremental::content_hash(source.as_bytes()));
        }
        if active_mounted {
            self.refresh_import_tree();
        }
        Task::batch(tasks)
    }

    /// Build the viewer from a clew-server `FileContent` reply — the server-side
    /// equivalent of `on_file_loaded` + `Highlighted` in one step (content
    /// arrives already highlighted, so there is no plain phase or flash).
    #[allow(clippy::too_many_arguments)] // mirrors the FileContent event's fields
    pub(crate) fn apply_file_content(
        &mut self,
        pane: usize,
        target: Option<usize>,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // Opening a file leaves the doc page (and the overview/stats homes).
        self.docs.page = None;
        let abs = root.join(&rel);
        let git_rel = rel.clone();
        let lang_key = highlight::detect(&abs);
        let source = Arc::new(source);
        let line_height = self.line_height();
        let old_viewport = self
            .panes
            .get(pane)
            .and_then(|s| s.as_ref())
            .map(|v| v.viewport_h);

        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        v.symbols = symbols;
        v.docs = docs.into_iter().collect();
        // As in `apply_file_refresh`: the reading target is the client's, and
        // this pane did not exist when `on_target_selected` last refreshed the
        // open ones, so it has no later chance to be corrected.
        v.inactive_lines = match lang_key {
            Some(lang) => inactive::inactive_lines(&source, lang, &self.reading_target),
            None => inactive.into_iter().collect(),
        };
        v.highlighted = true;
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = v.rel.clone();
        // The pane's document is being replaced: any hover in flight is
        // about the file that was there.
        self.invalidate_hover();
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        // Local projects only: a remote project's versions come from the index
        // publications, which are the one writer that sees EVERY file (see
        // `ProjectSymbols`). Hashing bytes in here as well would count a single
        // remote edit twice — once when the publication lands, once when this
        // re-read does — and make merely opening an untouched file look like a
        // change, rebuilding Stats and the project call graph for nothing.
        if self.local_project_state() {
            self.registry
                .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        }
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        self.request_git_info(git_rel, abs);
        self.follow_caret(Task::batch([scroll, lsp_task]))
    }

    /// Ask the server for `rel`'s per-line blame + change status. It fills in
    /// asynchronously via `Event::GitInfo`, routed back to this file by the
    /// recorded `abs` rather than by re-deriving it from the current root (a
    /// project switch would otherwise paint another project's same-named file).
    ///
    /// Ordering is enforced on the REQUEST side, through `pending_git`: this
    /// retires earlier server blames for the file, and `on_files_rehashed`
    /// retires them when it starts a local `git::info` pass over newer bytes.
    /// NOT closed: a local pass already running when a newer request goes out
    /// (the same file opened in a second pane mid-pass) still paints last and
    /// wins, because `Message::GitInfoLoaded` carries no request stamp for
    /// `on_git_info_loaded` to check. Closing that needs the id threaded onto
    /// the message, as `Highlighted` does with `src_hash`.
    pub(crate) fn request_git_info(&mut self, rel: String, abs: PathBuf) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = clew_protocol::Request::GitInfo { rel };
        if tx
            .send(clew_protocol::ClientMessage { id, request })
            .is_ok()
        {
            // Retire any earlier blame still in flight for this file: only the
            // newest may paint, and replies for one file are not ordered (the
            // server reads off its request loop), so an older one landing last
            // would describe bytes the pane no longer shows. Same reasoning as
            // `request_file_refresh`.
            self.pending_git.retain(|_, p| p != &abs);
            self.pending_git.insert(id, abs);
        }
    }

    /// The current reading target in its protocol wire form.
    pub(crate) fn target_spec(&self) -> clew_protocol::TargetSpec {
        clew_protocol::TargetSpec {
            label: self.reading_target.label.clone(),
            os: self.reading_target.os.clone(),
            arch: self.reading_target.arch.clone(),
            family: self.reading_target.family.clone(),
        }
    }

    /// Ask the server to read `rel` again so the panes showing it can reload in
    /// place. The reply lands as `ReadKind::Refresh`, which rebuilds a plain
    /// file through `apply_file_refresh` and a notebook through
    /// `apply_notebook_content` — the only way to rebuild a notebook pane, whose
    /// text is the parsed script projection rather than the file's bytes.
    pub(crate) fn request_file_refresh(&mut self, rel: &str) {
        let Some(tx) = self.server_tx.clone() else {
            return;
        };
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = clew_protocol::Request::ReadFile {
            rel: rel.to_string(),
            target: self.target_spec(),
        };
        if tx
            .send(clew_protocol::ClientMessage { id, request })
            .is_ok()
        {
            // Retire any earlier refresh still in flight for this file: only
            // the newest may apply, and replies for one rel are not ordered
            // (the server reads off its request loop).
            self.pending_reads
                .retain(|_, k| !matches!(k, ReadKind::Refresh { rel: r } if r == rel));
            self.pending_reads.insert(
                id,
                ReadKind::Refresh {
                    rel: rel.to_string(),
                },
            );
        }
    }

    /// Reload every pane showing `rel` in place after an on-disk change, keeping
    /// scroll / caret / folds (unlike opening, which jumps to a target line).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_file_refresh(
        &mut self,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let abs = root.join(&rel);
        let source = Arc::new(source);
        let docs: HashMap<usize, String> = docs.into_iter().collect();
        // Evaluate the cfg dimming against the client's CURRENT reading
        // target rather than trusting the server's answer. The two agree
        // normally, but the server evaluated whatever `reading.toml` said when
        // this read started, and a target the user picked meanwhile would
        // otherwise be undone by the reply.
        let inactive: HashSet<usize> = match highlight::detect(&abs) {
            Some(lang) => inactive::inactive_lines(&source, lang, &self.reading_target),
            None => inactive.into_iter().collect(),
        };
        let mut on_screen = false;
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                // Keeps scroll / caret / collapsed folds; then restore the
                // highlighting bundle the reload cleared.
                v.reload(source.clone(), lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                v.inactive_lines = inactive.clone();
                v.highlighted = true;
                on_screen = true;
                // `reload` clears the derived per-line state it cannot trust
                // across a content change, but not `git`, whose `blame` and
                // `status` vectors are indexed by 0-based line and describe
                // bytes that are now gone. Drop it here for the same reason,
                // and re-request below: a single insertion above shifts every
                // gutter bar, and the caret-line blame then names a plausible
                // but wrong commit, which "Explain why this line exists" would
                // hand to the LLM as fact. Cleared BEFORE the request so the
                // round trip shows nothing rather than something wrong, and so
                // a refresh with no transport leaves it empty, not lying.
                v.git = None;
            }
        }
        // Only the panes' gutters consume blame, so a file refreshed purely to
        // resync the language server (off screen) needs no git pass.
        if on_screen {
            self.request_git_info(rel, abs.clone());
        }
        // The server's copy of an open document is not refreshed by this
        // reload either — on a remote project this reply is the only carrier
        // of the new bytes the client ever sees (see `resync_open_doc`).
        self.resync_open_doc(&abs, &source);
        // Track the new bytes so the next change is detected against them —
        // local only, for the reason given in `apply_file_content`: remotely,
        // the publication that reported this change already bumped the version,
        // and hashing here would bump it a second time for the same edit.
        if self.local_project_state() {
            self.registry
                .set(abs, incremental::content_hash(source.as_bytes()));
        }
        self.follow_caret(Task::none())
    }

    /// Kick off a stats computation off the UI thread when it's stale (or
    /// `force`d). Single-flight: never launches a second run while one is in
    /// flight. Stamps `stats_rev` with the registry revision so a later file
    /// change (which bumps the revision) marks the result stale.
    pub(crate) fn start_stats(&mut self, force: bool) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let rev = self.registry.revision();
        let fresh = self.stats.report.is_some() && self.stats.rev == rev;
        if self.stats.building || (!force && fresh) {
            return Task::none();
        }
        self.stats.building = true;
        self.stats.rev = rev;
        let epoch = self.project_epoch;
        if self.stats.report.is_none() {
            self.status = "Computing code statistics…".into();
        }
        // Remote project: the walk happens where the files live — this
        // machine's disk at the same path is another project's data.
        if !self.local_project_state() {
            let ai = self.ai_client();
            return Task::perform(
                async move {
                    match ai.request(clew_protocol::Request::Stats).await {
                        Ok(clew_protocol::Event::Stats { report, .. }) => {
                            serde_json::from_str::<stats::StatsReport>(&report).ok()
                        }
                        // A dead transport, the RPC timeout, a refusal
                        // (`Event::Error`), a malformed payload: none of them
                        // is an answer, and `unwrap_or_default()` turned every
                        // one into a report of zero files.
                        _ => None,
                    }
                },
                move |report| stats_done(root.clone(), epoch, rev, report),
            );
        }
        let compute_root = root.clone();
        Task::perform(
            // A panicked or cancelled compute is not an empty project either.
            async move {
                tokio::task::spawn_blocking(move || stats::compute(&compute_root))
                    .await
                    .ok()
            },
            move |report| stats_done(root.clone(), epoch, rev, report),
        )
    }

    pub(crate) fn build_project_calls(&mut self) -> Task<Message> {
        let epoch = self.project_epoch;
        // Remote project: the build reads every file, so it runs where the
        // files live. The client contributes the one input the server can't
        // derive — the resolved import scope — as project-relative paths.
        if !self.local_project_state() {
            let Some(project) = &self.project else {
                return Task::none();
            };
            let root = project.root.clone();
            let scope: Vec<(String, Vec<String>)> = self
                .import_graph
                .scope_map()
                .into_iter()
                .filter_map(|(file, imports)| {
                    let rel = file.strip_prefix(&root).ok()?;
                    Some((
                        rel.to_string_lossy().into_owned(),
                        imports
                            .iter()
                            .filter_map(|i| i.strip_prefix(&root).ok())
                            .map(|i| i.to_string_lossy().into_owned())
                            .collect(),
                    ))
                })
                .collect();
            self.project_calls.rev = self.registry.revision();
            self.project_calls.building = true;
            let ai = self.ai_client();
            let tag_root = root.clone();
            return Task::perform(
                async move {
                    match ai
                        .request(clew_protocol::Request::ProjectCalls { scope })
                        .await
                    {
                        Ok(clew_protocol::Event::ProjectCalls { graph, .. }) => {
                            serde_json::from_str::<projectcalls::ProjectCallGraph>(&graph)
                                .unwrap_or_default()
                                // The wire carries project-relative paths;
                                // rebuild this client's identities.
                                .rebase(|p| root.join(p))
                        }
                        _ => projectcalls::ProjectCallGraph::default(),
                    }
                },
                move |graph| Message::ProjectCallsBuilt {
                    root: tag_root.clone(),
                    epoch,
                    graph,
                },
            );
        }
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
        let read_root = project.root.clone();
        let tag_root = project.root.clone();
        // Import scope: each file → the internal files it imports, so a called
        // name resolves to the definition actually in scope.
        let scope = self.import_graph.scope_map();
        self.project_calls.rev = self.registry.revision();
        self.project_calls.building = true;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let sources = read_call_sources(&read_root, files);
                    projectcalls::ProjectCallGraph::build(defs, &sources, &scope)
                })
                .await
                .unwrap_or_default()
            },
            move |graph| Message::ProjectCallsBuilt {
                root: tag_root.clone(),
                epoch,
                graph,
            },
        )
    }

    /// Ensure the project call graph is available for the Explain panel's
    /// call-flow strip, building it in the background if it's empty or stale
    /// (single-flight). No-op when one is already in flight or the graph is
    /// current — so it's cheap to call as the cursor moves between functions.
    pub(crate) fn ensure_call_graph(&mut self) -> Task<Message> {
        if self.project_calls.building
            || (!self.project_calls.graph.is_empty()
                && self.project_calls.rev == self.registry.revision())
        {
            return Task::none();
        }
        self.build_project_calls()
    }

    /// Whether the loaded API-docs index describes the files as they are NOW.
    /// Keyed on the change registry, the freshness key its peers already use
    /// (`stats.rev`, `project_calls.rev`); on a REMOTE project that revision is
    /// advanced by the server's `ProjectSymbols` publications, so this reads a
    /// remote edit exactly as it reads a local one.
    ///
    /// Deliberately conservative in one direction: `request_docs` stamps the
    /// revision it asked at, so a change landing WHILE a build is in flight
    /// leaves the (genuinely current) result reading as stale and costs one
    /// extra rebuild the next time the tab is opened. Under-claiming freshness
    /// only wastes a build; over-claiming is what put a pre-edit API surface on
    /// screen and is the reason this key exists.
    pub(crate) fn docs_fresh(&self) -> bool {
        !self.docs.files.is_empty() && self.docs.rev == self.registry.revision()
    }

    /// Rebuild the API docs if what we hold is missing or stale. Cheap to call
    /// from any place that is about to READ the index. Single-flight, though
    /// the guard that matters lives in `request_docs` — every caller needs it,
    /// not just this one.
    pub(crate) fn ensure_docs(&mut self) {
        if !self.docs.loading && !self.docs_fresh() {
            self.request_docs();
        }
    }

    /// Ready, call-hierarchy-capable servers keyed by language.
    pub(crate) fn call_hierarchy_clients(&self) -> HashMap<String, lsp::client::LspClient> {
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
    pub(crate) fn refinable_defs(
        &self,
        clients: &HashMap<String, lsp::client::LspClient>,
    ) -> Vec<projectcalls::Def> {
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
    pub(crate) fn refine_project_calls(&mut self) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            self.status = "No language server ready — open a file to start one, then retry".into();
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        if all.is_empty() {
            self.status = "No functions to refine for the ready server(s)".into();
            return Task::none();
        }
        self.spawn_refine(
            clients,
            all.clone(),
            all,
            projectcalls::SymEdges::default(),
            None,
        )
    }

    /// Incrementally refresh the precise graph after files changed: re-query only
    /// the changed files' functions and patch the edge set.
    pub(crate) fn refine_incremental(&mut self, changed: HashSet<PathBuf>) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        let query: Vec<projectcalls::Def> = all
            .iter()
            .filter(|d| changed.contains(&d.file))
            .cloned()
            .collect();
        let base = self.project_calls.precise_edges.clone();
        // Even with nothing to re-query (e.g. all changed functions removed), we
        // still rebuild so deleted files' edges drop out.
        self.spawn_refine(clients, all, query, base, Some(changed))
    }

    /// Shared refine launcher. `query_defs` are LSP-queried; `all_defs` is the
    /// full node set the result maps onto; `base` is the starting edge set;
    /// `changed` (when incremental) is the files whose old edges to drop before
    /// re-querying, and also selects incoming+outgoing (vs incoming-only) queries.
    pub(crate) fn spawn_refine(
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
        self.project_calls.generation += 1;
        let generation = self.project_calls.generation;
        self.project_calls.refine_progress = Some((0, query_defs.len()));
        if changed.is_none() {
            self.status = format!("Refining {} functions with LSP…", query_defs.len());
        }
        // A remote project's files live on the other host; the pass fetches
        // their text over the protocol rather than reading this machine's disk.
        let remote = (!self.local_project_state()).then(|| self.ai_client());
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(
                output, all_defs, query_defs, base, changed, clients, root, generation, remote,
            )
        });
        // Abortable so leaving the project actually stops the pass: it holds
        // clones of this project's language-server clients, so dropping
        // `App::lsp` would not.
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.project_calls.refine_abort = Some(handle);
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file list is a snapshot of a scan that may be minutes old, so the
    /// call-graph build has to re-decide containment at read time: a listed
    /// leaf that has since become a symlink out of the project would otherwise
    /// have its target's text parsed and drawn as this project's code.
    #[test]
    #[cfg(unix)]
    fn call_sources_refuse_a_leaf_symlinked_out_of_the_project() {
        let base = std::env::temp_dir().join("clew-call-sources");
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("proj");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let inside = root.join("a.rs");
        std::fs::write(&inside, "fn a() { b(); }\n").unwrap();
        let secret = outside.join("secret.rs");
        std::fs::write(&secret, "fn secret() { leak(); }\n").unwrap();
        // The swap: a file the scan listed as a plain source is now a link.
        let link = root.join("linked.rs");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let sources = read_call_sources(&root, vec![inside.clone(), link.clone(), secret.clone()]);
        assert_eq!(
            sources.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            vec![inside],
            "only the real in-project file may be read"
        );
        assert!(
            !sources.iter().any(|(_, c)| c.contains("secret")),
            "out-of-project text must not reach the call graph"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
