//! The clew-server seam and AI plumbing: symbol index, server events/replies, AI client, file content application, stats, project-call-graph build/refine.

use crate::app::prelude::*;
use crate::*;

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
    /// they migrate onto the protocol. Returns the follow-up work an event
    /// requires (e.g. the derived-state refresh a watcher change triggers).
    pub(crate) fn handle_server_event(&mut self, event: clew_protocol::Event) -> Task<Message> {
        use clew_protocol::Event;
        let mut task = Task::none();
        match event {
            Event::Error { message } => {
                // A failed folder listing stops the picker's spinner in place.
                if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage)
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
                if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage)
                {
                    b.cwd = path;
                    b.parent = parent;
                    b.entries = entries;
                    b.loading = false;
                }
            }
            Event::GitInfo { rel, info } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
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
            Event::ProjectSymbols {
                root: snap_root,
                full,
                files,
                go_module,
                dart_package,
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
                if full {
                    self.symbol_index_by_file.clear();
                    // Resolution metadata rides on full snapshots only.
                    self.remote_import_meta = Some((go_module, dart_package));
                }
                let mut raw_imports: std::collections::HashMap<PathBuf, Vec<imports::RawImport>> =
                    std::collections::HashMap::new();
                for fs in files {
                    let abs = root.join(&fs.rel);
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
                // Partial update: refresh just the changed files' out-edges.
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
                        } else {
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
                if self.sidebar == SidebarTab::Docs && !self.docs.loading {
                    self.request_docs();
                }
                if self.connection.is_remote() {
                    // Remote: re-request any open changed file so its view
                    // reloads — the server reads it where it lives. Nothing
                    // here may read a remote-pathed file from the local
                    // disk; the rest of the derived state (index, graphs,
                    // explanations) migrates server-side with the project
                    // snapshot.
                    let open: HashSet<PathBuf> =
                        self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
                    let spec = self.target_spec();
                    for rel in &rels {
                        let abs = root.join(rel);
                        if open.contains(&abs)
                            && let Some(tx) = self.server_tx.clone()
                        {
                            let id = self
                                .next_req_id
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                    }
                } else {
                    // Local server: the watcher's paths are this machine's
                    // files, so run the FULL derived-state pipeline —
                    // registry, symbol index, import graph, call graphs,
                    // trail re-anchoring, and the throttled explanation /
                    // overview auto-refresh. It also reloads open panes in
                    // place. Without this, an edited import or a new file
                    // left every graph and explanation stale until the
                    // project was reopened.
                    task = self.on_files_changed(rels.iter().map(|rel| root.join(rel)).collect());
                }
            }
            Event::Tree {
                root: tree_root,
                tree,
                files,
                ..
            } => {
                // A structural change (create/delete) from the watcher: swap the
                // tree in place, keeping panes / scroll / everything else. The
                // notification names the project the server watched — one from
                // a project we've already left must not splice its file list
                // under the current root.
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
        task
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
        if !self.ai_on_server() {
            return;
        }
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
            clew_protocol::Event::Ready { protocol } => {
                if protocol != clew_protocol::PROTOCOL_VERSION {
                    self.server_tx = None;
                    self.status = format!(
                        "clew-server speaks protocol v{protocol}, this clew speaks v{} — \
                         update the server (local: rebuild; remote: it redeploys on reconnect)",
                        clew_protocol::PROTOCOL_VERSION
                    );
                    return Task::none();
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
                Some(ReadKind::Refresh) => {
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
                        pane, target, rel, language, cells, symbols, projection, false,
                    )
                }
                Some(ReadKind::Refresh) => {
                    // Reload in place: find the pane showing this notebook and
                    // rebuild it; `refresh` keeps scroll and expanded outputs
                    // and skips the open-time side effects.
                    let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                        return Task::none();
                    };
                    let abs = root.join(&rel);
                    let Some(pane) = self
                        .panes
                        .iter()
                        .position(|v| v.as_ref().is_some_and(|v| v.abs == abs))
                    else {
                        return Task::none();
                    };
                    self.apply_notebook_content(
                        pane, None, rel, language, cells, symbols, projection, true,
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
                // arrives otherwise is a catch-up OpenProject reply (after a
                // local-fallback open) and must not re-open the project.
                if !self.scanning {
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
                    LspResolution::Ready { init_options } => {
                        self.stash_remote_init(&language, init_options);
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
                            self.stash_remote_init(&language, spec.init_options);
                            self.pending_lsp_command = Some(PendingLspCommand {
                                root,
                                host,
                                language,
                                command: PathBuf::from(&spec.command),
                                args: spec.args,
                                server_name: spec.server,
                                version: spec.version,
                                fingerprint: spec.fingerprint,
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
            other => self.handle_server_event(other),
        }
    }

    /// Build the viewer from a clew-server `FileContent` reply — the server-side
    /// equivalent of `on_file_loaded` + `Highlighted` in one step (content
    /// arrives already highlighted, so there is no plain phase or flash).
    #[allow(clippy::too_many_arguments)]
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_notebook_content(
        &mut self,
        pane: usize,
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

        let old = self
            .panes
            .get(pane)
            .and_then(|s| s.as_ref())
            .map(|v| (v.viewport_h, v.scroll_y, v.nb_expanded.clone()));
        let source = Arc::new(projection);
        let lines = highlight::plain_lines(&source);
        let mut v = Viewer::new(abs.clone(), rel, None, source.clone(), lines);
        v.symbols = symbols;
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
        // estimate of the target cell's offset; the highlight ring on the cell
        // (drawn by the view for `target_line`) does the precise pointing. A
        // refresh keeps the reader where they were instead.
        let y = if refresh {
            v.scroll_y
        } else {
            target
                .map(|line| {
                    Self::estimate_notebook_offset(&doc, &v.nb_expanded, line, self.line_height())
                })
                .unwrap_or(0.0)
        };
        v.scroll_y = y;
        if !refresh {
            self.status = v.rel.clone();
        }
        self.panes[pane] = Some(v);
        self.registry
            .set(abs, incremental::content_hash(source.as_bytes()));
        if pane == self.active {
            self.refresh_import_tree();
        }
        tasks.push(operation::scroll_to(
            ui::code_scroll_id(pane),
            AbsoluteOffset { x: 0.0, y },
        ));
        Task::batch(tasks)
    }

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
            let id = self
                .next_req_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // stats::compute walks the LOCAL filesystem; a remote project's
        // paths belong to the remote host.
        if !self.local_project_state() {
            self.status = "Statistics aren't available on remote projects yet".into();
            self.stats.building = false;
            return Task::none();
        }
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
            move |report| Message::StatsDone {
                root: root.clone(),
                rev,
                report,
            },
        )
    }

    pub(crate) fn build_project_calls(&mut self) -> Task<Message> {
        // Reads every file from the LOCAL disk; a remote project's paths
        // belong to the remote host.
        if !self.local_project_state() {
            self.status = "Project calls aren't available on remote projects yet".into();
            return Task::none();
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
        let stream = iced::stream::channel(256, move |output| {
            refine_stream(
                output, all_defs, query_defs, base, changed, clients, root, generation,
            )
        });
        Task::run(stream, |m| m)
    }
}
