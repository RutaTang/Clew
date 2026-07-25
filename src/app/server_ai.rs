//! The clew-server seam and AI plumbing: symbol index, server events/replies, AI client, file content application, stats, project-call-graph build/refine.

use crate::*;
use crate::app::prelude::*;

impl App {
    /// Re-flatten the per-file symbol map into `symbol_index` and refresh the
    /// finder when it is showing symbols.
    pub(crate) fn rebuild_symbol_index(&mut self) {
        self.symbol_index = Arc::new(index::flatten(&self.symbol_index_by_file));
        if self.finder.open && self.finder.mode == FinderMode::Symbols {
            self.finder.refresh_symbols(&self.symbol_index);
        }
    }

    /// Kick an off-thread (re)build of the project call graph from the current
    /// symbol index + file contents. Delivered as `ProjectCallsBuilt`.
    /// Apply an event from the clew-server. Backend flows are handled here as
    /// they migrate onto the protocol; for now it's just the handshake.
    pub(crate) fn handle_server_event(&mut self, event: clew_protocol::Event) {
        use clew_protocol::Event;
        match event {
            Event::Ready { .. } => {
                // The protocol handshake is internal — don't surface version
                // jargon in the status bar. Stay quiet until there's something
                // to say (a scan, a file count).
                self.status.clear();
            }
            Event::Error { message } => {
                // A failed folder listing stops the picker's spinner in place.
                if let Some(ConnectStage::Browsing(b)) =
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
                if let Some(tx) = self.chat_streams.lock().unwrap().remove(&stream) {
                    let _ = tx.send(ChatStreamPiece::Done(error));
                }
            }
            Event::Docs { files } => {
                self.docs.files = files;
                self.docs.loading = false;
                // Resolve a "View docs" that was waiting on the index.
                if let Some(name) = self.docs.pending_view.take() {
                    match find_doc_by_name(&self.docs.files, &name) {
                        Some((rel, line)) => self.open_doc_page(&rel, line),
                        None => self.status = format!("No docs for “{name}”"),
                    }
                }
            }
            Event::DirListing {
                path,
                parent,
                entries,
            } => {
                // Fill the remote folder picker with this directory's contents.
                if let Some(ConnectStage::Browsing(b)) =
                    self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.cwd = path;
                    b.parent = parent;
                    b.entries = entries;
                    b.loading = false;
                }
            }
            Event::SearchResults { hits, error } => {
                // Rebuild absolute paths from the project root; the wire carries
                // only root-relative paths (meaningful across a remote server).
                let root = self.project.as_ref().map(|p| p.root.clone());
                let Some(root) = root else { return };
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
            }
            Event::GitInfo { rel, info } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return;
                };
                let abs = root.join(&rel);
                let info = info.map(Arc::new);
                for slot in &mut self.panes {
                    if let Some(v) = slot
                        && v.abs == abs
                    {
                        v.git = info.clone();
                    }
                }
            }
            Event::FilesChanged { rels } => {
                // The server's watcher reports on-disk changes.
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return;
                };
                let open: HashSet<PathBuf> =
                    self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
                let spec = self.target_spec();
                let mut index_dirty = false;
                for rel in &rels {
                    let abs = root.join(rel);
                    // Re-request an open file so its view reloads in place.
                    if open.contains(&abs)
                        && let Some(tx) = self.server_tx.clone()
                    {
                        let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let request = clew_protocol::Request::ReadFile {
                            rel: rel.clone(),
                            target: spec.clone(),
                        };
                        if tx
                            .send(clew_protocol::ClientMessage { id, request })
                            .is_ok()
                        {
                            self.pending_reads.insert(id, ReadKind::Refresh);
                        }
                    }
                    // Keep the project symbol index (Cmd+T) fresh. The index is
                    // still client-side, so it re-reads locally — as it already
                    // does when built on open; this moves to the server with the
                    // index flow.
                    if let Some(lang) = highlight::detect(&abs) {
                        match std::fs::read_to_string(&abs) {
                            Ok(content) => {
                                let syms = index::file_symbols(&abs, rel, &content, lang);
                                self.symbol_index_by_file.insert(abs, syms);
                                index_dirty = true;
                            }
                            Err(_) => {
                                index_dirty |= self.symbol_index_by_file.remove(&abs).is_some();
                            }
                        }
                    }
                }
                if index_dirty {
                    self.rebuild_symbol_index();
                }
                // Keep the API docs fresh while their tab is open.
                if self.sidebar == SidebarTab::Docs && !self.docs.loading {
                    self.request_docs();
                }
            }
            Event::Tree { tree, files, .. } => {
                // A structural change (create/delete) from the watcher: swap the
                // tree in place, keeping panes / scroll / everything else.
                if let Some(project) = &mut self.project {
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
            Event::ProcessOutput { proc, data } => {
                // Feed a proxied process's stdout into its LspClient bridge.
                if let Some(feed) = self.proc_feeds.get(&proc) {
                    let _ = feed.send(data);
                }
            }
            Event::ProcessExited { proc, .. } => {
                // Dropping the feed closes the bridge, so the LspClient sees EOF.
                self.proc_feeds.remove(&proc);
                self.lsp_procs.retain(|_, p| *p != proc);
            }
            // Other flows (Outline, …) handled here as they migrate.
            _ => {}
        }
    }

    /// Route a correlated server reply. `FileContent` needs the request id to
    /// find which pane asked for it; everything else is id-agnostic.
    /// An AI router for background tasks. Endpoint is Server (matching the Hello
    /// handshake); with no server channel it transparently runs calls locally.
    pub(crate) fn ai_client(&self) -> AiClient {
        AiClient {
            endpoint: clew_protocol::AiEndpoint::Server,
            server_tx: self.server_tx.clone(),
            next_id: self.next_req_id.clone(),
            pending: self.ai_pending.clone(),
        }
    }

    /// Hand the server the current AI provider config so it can make calls.
    pub(crate) fn send_ai_config(&self) {
        let Some(tx) = &self.server_tx else { return };
        let chat = llm::Config::load().map(|c| clew_protocol::AiChatConfig {
            provider: c.provider.slug().to_string(),
            api_key: c.api_key,
            model: c.model,
            base_url: c.base_url,
        });
        let embed = embed::Config::load().map(|c| clew_protocol::AiEmbedConfig {
            api_key: c.api_key,
            model: c.model,
            base_url: c.base_url,
        });
        if chat.is_some() || embed.is_some() {
            let _ = tx.send(clew_protocol::ClientMessage {
                id: 0,
                request: clew_protocol::Request::SetAiConfig { chat, embed },
            });
        }
    }

    pub(crate) fn handle_server_reply(&mut self, id: u64, event: clew_protocol::Event) -> Task<Message> {
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
            clew_protocol::Event::FileContent {
                rel,
                source,
                lines,
                symbols,
                docs,
                inactive,
            } => match self.pending_reads.remove(&id) {
                Some(ReadKind::Open { pane, target }) => {
                    self.apply_file_content(pane, target, rel, source, lines, symbols, docs, inactive)
                }
                Some(ReadKind::Refresh) => {
                    self.apply_file_refresh(rel, source, lines, symbols, docs, inactive)
                }
                None => Task::none(),
            },
            clew_protocol::Event::Tree {
                tree,
                files,
                truncated,
            } => {
                // Only build the project while we're opening one; a Tree that
                // arrives otherwise is a catch-up OpenProject reply (after a
                // local-fallback open) and must not re-open the project.
                if !self.scanning {
                    return Task::none();
                }
                let Some(root) = self.pending_scan_root.take() else {
                    return Task::none();
                };
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
            other => {
                self.handle_server_event(other);
                Task::none()
            }
        }
    }

    /// Build the viewer from a clew-server `FileContent` reply — the server-side
    /// equivalent of `on_file_loaded` + `Highlighted` in one step (content
    /// arrives already highlighted, so there is no plain phase or flash).
    #[allow(clippy::too_many_arguments)]
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
        v.inactive_lines = inactive.into_iter().collect();
        v.highlighted = true;
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        v.caret = Some((target.map(|t| t.saturating_sub(1)).unwrap_or(0), 0));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = v.rel.clone();
        self.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        self.registry
            .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        if pane == self.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        // Ask the server for git blame; it fills in asynchronously via
        // Event::GitInfo, routed back to this file by rel.
        if let Some(tx) = self.server_tx.clone() {
            let id = self.next_req_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let request = clew_protocol::Request::GitInfo { rel: git_rel };
            let _ = tx.send(clew_protocol::ClientMessage { id, request });
        }
        self.follow_caret(Task::batch([scroll, lsp_task]))
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
        let inactive: HashSet<usize> = inactive.into_iter().collect();
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
            }
        }
        // Track the new bytes so the next change is detected against them.
        self.registry
            .set(abs, incremental::content_hash(source.as_bytes()));
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
        if self.stats.report.is_none() {
            self.status = "Computing code statistics…".into();
        }
        let compute_root = root.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || stats::compute(&compute_root))
                    .await
                    .unwrap_or_default()
            },
            move |report| Message::StatsDone { root: root.clone(), rev, report },
        )
    }

    pub(crate) fn build_project_calls(&mut self) -> Task<Message> {
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
        self.project_calls.rev = self.registry.revision();
        self.project_calls.building = true;
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
    pub(crate) fn refinable_defs(&self, clients: &HashMap<String, lsp::client::LspClient>) -> Vec<projectcalls::Def> {
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
    pub(crate) fn refine_incremental(&mut self, changed: HashSet<PathBuf>) -> Task<Message> {
        let clients = self.call_hierarchy_clients();
        if clients.is_empty() {
            return Task::none();
        }
        let all = self.refinable_defs(&clients);
        let query: Vec<projectcalls::Def> =
            all.iter().filter(|d| changed.contains(&d.file)).cloned().collect();
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
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(output, all_defs, query_defs, base, changed, clients, root, generation)
        });
        Task::run(stream, |m| m)
    }

}
