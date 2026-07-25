//! Background services: auto-refresh, project scan, LSP provisioning, search, go-to-definition and call-hierarchy.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn request_auto_refresh(&mut self) -> Task<Message> {
        if !self.llm_available || self.explain.cache.is_empty() {
            return Task::none();
        }
        // Let any running pass finish, then re-check on completion / next tick.
        if self.explain.running || self.overview.generating || self.building_embeddings {
            self.refresh_pending = true;
            return Task::none();
        }
        let cooled = self
            .last_auto_refresh
            .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
            .unwrap_or(true);
        if cooled {
            self.begin_refresh()
        } else {
            self.refresh_pending = true;
            Task::none()
        }
    }

    /// Begin a refresh pass now, resetting the cooldown. Shared by the auto path
    /// and the manual force-refresh. The explain pass is cache-aware (only changed
    /// nodes hit the LLM); on completion it chains the semantic index and overview
    /// when those already exist (see `ExplainDone`).
    pub(crate) fn begin_refresh(&mut self) -> Task<Message> {
        self.last_auto_refresh = Some(std::time::Instant::now());
        self.refresh_pending = false;
        Task::done(Message::ExplainProject)
    }

    /// Whether the overview's inputs changed since it was generated, so a chained
    /// refresh regenerates it only when the result would actually differ (an
    /// overview pass is a full LLM call, unlike the incremental explain/index).
    pub(crate) fn overview_inputs_changed(&self) -> bool {
        let hash =
            incremental::content_hash(overview::prompt(&self.gather_overview_inputs()).as_bytes());
        self.overview.prompt_hash != Some(hash)
    }

    /// Fold a freshly-computed module diagram into the raw overview markdown for
    /// display. Returns the assembled markdown and the diagram used (so callers
    /// can tell whether a re-prepare is worthwhile).
    /// The overview prose for display: strip any legacy mermaid "Module map"
    /// section a cached overview may still carry (the map is drawn natively now).
    pub(crate) fn overview_display(&self, raw: &str) -> String {
        overview::strip_module_map(raw)
    }

    /// Lay out the module map from the current import graph, or None when there's
    /// too little structure to show.
    pub(crate) fn compute_overview_map(&self) -> Option<graphlayout::Layout> {
        let (nodes, edges) = overview::module_layout_inputs(&self.import_graph.scope_map())?;
        Some(graphlayout::layout(nodes, edges))
    }

    /// Recompute the native module-map layout, e.g. once the import graph finishes
    /// resolving. Cheap and synchronous — the map is a canvas, not a prose segment.
    pub(crate) fn refresh_overview_map(&mut self) -> Task<Message> {
        if self.overview.markdown.is_some() {
            self.overview.map = self.compute_overview_map();
        }
        Task::none()
    }

    pub(crate) fn on_scan_done(&mut self, result: ScanResult) -> Task<Message> {
        self.scanning = false;
        self.status = format!(
            "{} files{}",
            result.files.len(),
            if result.truncated { " (truncated)" } else { "" }
        );
        self.expanded.clear();
        self.panes = [None, None];
        self.split = false;
        self.active = 0;
        // Warm-start the navigation tree from this project's persisted history.
        self.history = history::load(&result.root);
        self.finder = Finder::default();
        self.search = SearchState::default();
        self.bookmarks = bookmarks::load(&result.root);
        self.notes = notes::load(&result.root);
        self.symbol_index = Arc::new(Vec::new());
        self.symbol_index_by_file.clear();
        // A new project: drop the old API docs (they belong to the old root).
        self.docs.files = Vec::new();
        self.docs.loading = false;
        self.docs.expanded.clear();
        self.docs.page = None;
        self.docs.filter.clear();
        self.docs.pending_view = None;
        self.registry.clear();
        self.call_graph = None;
        self.import_graph = imports::ImportGraph::default();
        self.import_tree = None;
        self.import_cycles = Vec::new();
        self.project_calls.graph = projectcalls::ProjectCallGraph::default();
        self.project_calls.rev = 0;
        self.project_calls.building = false;
        self.project_calls.precise = false;
        self.project_calls.generation += 1;
        self.project_calls.refine_progress = None;
        self.project_calls.precise_edges = projectcalls::SymEdges::default();
        self.project_calls.precise_pending = HashSet::new();
        self.overlay = None;
        self.graph_layout = None;
        // Warm-start explanations from this project's persisted cache.
        self.explain.cache = explain::load(&result.root);
        self.explain.running = false;
        self.explain.progress = None;
        self.explain.generation += 1;
        self.explain.view = None;
        self.explain.prepared = Vec::new();
        self.explain.svgs.clear();
        self.explain.showing_detail = false;
        // Land on the architecture-overview home (warm-started from cache below).
        let cached_overview = overview::load(&result.root);
        self.overview.prompt_hash = cached_overview.as_ref().map(|c| c.prompt_hash);
        self.overview.markdown = cached_overview.map(|c| c.markdown);
        self.overview.prepared = Vec::new();
        self.overview.generating = false;
        self.overview.showing = true;
        // Warm-start stats from disk so the Stats view paints instantly; the
        // `u64::MAX` sentinel forces one background refresh on first entry (the
        // registry revision — the freshness key — isn't stable across restarts).
        self.stats.report = stats::load(&result.root).map(|c| c.report);
        self.stats.rev = u64::MAX;
        self.stats.building = false;
        self.stats.showing = false;
        // A fresh project starts with a clean auto-refresh cooldown.
        self.last_auto_refresh = None;
        self.refresh_pending = false;
        // Warm-start the semantic index and reset the search state.
        self.embed_index = embed::load(&result.root);
        self.embed_available = embed::Config::available();
        self.building_embeddings = false;
        self.semantic_query = String::new();
        self.semantic_results = Vec::new();
        self.searching_semantic = false;
        // Drop any servers from the previous project (kills their children).
        self.lsp.clear();
        self.lsp_opened.clear();
        self.pending_lsp_consent = None;
        self.lsp_config = lsp::config::ProjectLspConfig::load(&result.root).unwrap_or_default();
        self.reading_target =
            reading::load_target(&result.root).unwrap_or_else(inactive::Target::host);
        self.walk.library = walkthrough::load_library(&result.root);
        self.walk.open = None;
        self.walk.step = 0;
        self.walk.prepared = Vec::new(); // prepared lazily when a tour opens
        // Languages actually present in the project that clew ships a server
        // for — drives which rows the server panel shows.
        let mut langs: Vec<String> = result
            .files
            .iter()
            .filter_map(|f| highlight::detect(&f.abs))
            .filter(|l| lsp::registry::default_for_language(l).is_some())
            .map(|l| l.to_string())
            .collect();
        langs.sort();
        langs.dedup();
        self.project_languages = langs;
        let files = Arc::new(result.files);
        self.project = Some(Project {
            root: result.root,
            tree: result.tree,
            files: files.clone(),
            truncated: result.truncated,
        });
        // The server already knows this project: either it produced this tree
        // (server-scan path in `start_scan`), or — if this came from the local
        // fallback — the `ServerConnected` handler syncs it when the server is up.

        // Build the project-wide symbol index in the background, warm-starting
        // from the persistent cache (only files changed while clew was closed
        // are re-read/re-parsed), and persist the refreshed cache.
        self.indexing = true;
        let index_root = self.project.as_ref().unwrap().root.clone();
        let tag_root = index_root.clone();
        let index_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || index::build_indexed_warm(&index_root, files))
                    .await
                    .unwrap_or_default()
            },
            move |indexed| Message::SymbolIndexDone {
                root: tag_root.clone(),
                indexed,
            },
        );

        let open_task = match self.pending_open.take() {
            Some(file) => self.open_file(file, None, true), // clears show_overview
            None => Task::none(),
        };
        // Prepare the cached overview so the home screen renders it immediately.
        // The module map lays out from the import graph; if imports aren't
        // resolved yet, it fills in when indexing completes (refresh_overview_map).
        let overview_task = match self.overview.markdown.clone() {
            Some(md) => {
                let display = self.overview_display(&md);
                let (prepared, task) = self.prepare_segments(&display);
                self.overview.prepared = prepared;
                self.overview.map = self.compute_overview_map();
                task
            }
            None => Task::none(),
        };
        // No auto-explain on startup: warm-start from the persisted cache and
        // show what's there. Explanations (re)generate only on an explicit
        // request (whole project / one function) or when a file's hash changes.
        Task::batch([index_task, open_task, overview_task])
    }

    /// Status text and the action button for a language row in the server
    /// panel. Distinguishes running / installed-but-idle / not-downloaded so an
    /// installed server never shows a misleading "Download".
    pub fn lsp_row(&self, language: &str) -> (String, Option<(&'static str, Message)>) {
        let restart = || Some(("Restart", Message::LspRestart(language.to_string())));
        let provision =
            |label: &'static str| Some((label, Message::LspDownloadFor(language.to_string())));
        match self.lsp.get(language) {
            Some(LspSlot::Ready(c)) => (c.progress().unwrap_or_else(|| "ready".into()), restart()),
            Some(LspSlot::Starting) => ("starting…".into(), None),
            Some(LspSlot::Failed(e)) => (format!("error: {e}"), provision("Retry")),
            Some(LspSlot::Unsupported(e)) => (e.clone(), None),
            Some(LspSlot::AwaitingConsent) => ("download pending".into(), provision("Download")),
            None => match self.lsp_config.resolve(language) {
                Some(server) => match lsp::store::locate(&server) {
                    lsp::store::Located::Ready(_) => {
                        ("installed · starts on open".into(), provision("Start"))
                    }
                    lsp::store::Located::NeedsDownload { .. } => {
                        ("not downloaded".into(), provision("Download"))
                    }
                    lsp::store::Located::NeedsInstall { .. } => {
                        ("not installed".into(), provision("Install"))
                    }
                    lsp::store::Located::Unsupported(m) => (m, None),
                },
                None => ("no server".into(), None),
            },
        }
    }

    /// Languages to show in the server panel: those present in the project,
    /// plus any installed or running server (so they can be managed anywhere).
    pub fn managed_languages(&self) -> Vec<String> {
        let mut langs = self.project_languages.clone();
        for srv in &self.installed_servers {
            if let Some(spec) = lsp::registry::by_name(&srv.name) {
                langs.extend(spec.languages.iter().map(|l| l.to_string()));
            }
        }
        langs.extend(self.lsp.keys().cloned());
        langs.sort();
        langs.dedup();
        langs
    }

    /// Ensure a language server is provisioned/started for `language`, and open
    /// any already-loaded documents once it is ready. Idempotent.
    pub(crate) fn ensure_lsp(&mut self, language: &str) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        match self.lsp.get(language) {
            Some(LspSlot::Ready(client)) => {
                let client = client.clone();
                return self.open_docs_for_language(language, &client);
            }
            // Starting / failed / unsupported / awaiting consent: nothing to do.
            Some(_) => return Task::none(),
            None => {}
        }

        let Some(server) = self.lsp_config.resolve(language) else {
            self.lsp.insert(
                language.to_string(),
                LspSlot::Unsupported("no server for this language".into()),
            );
            return Task::none();
        };
        let (provision, dest_dir) = match lsp::store::locate(&server) {
            lsp::store::Located::Ready(exe) => return self.start_lsp_with(language, exe),
            lsp::store::Located::NeedsDownload { download, dest_dir } => {
                (LspProvision::Download(download), dest_dir)
            }
            lsp::store::Located::NeedsInstall {
                install, dest_dir, ..
            } => (LspProvision::Install(install), dest_dir),
            lsp::store::Located::Unsupported(msg) => {
                self.lsp
                    .insert(language.to_string(), LspSlot::Unsupported(msg));
                return Task::none();
            }
        };
        self.lsp
            .insert(language.to_string(), LspSlot::AwaitingConsent);
        self.pending_lsp_consent = Some(LspConsent {
            language: language.to_string(),
            server_name: server.server_name,
            version: server.version,
            provision,
            dest_dir,
        });
        Task::none()
    }

    /// Launch the server executable and run the handshake in the background.
    pub(crate) fn start_lsp_with(&mut self, language: &str, exe: PathBuf) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(server) = self.lsp_config.resolve(language) else {
            return Task::none();
        };
        self.lsp.insert(language.to_string(), LspSlot::Starting);
        let lang = language.to_string();
        let args = server.args.clone();
        // Merge the auto-detected language environment (e.g. a project venv for
        // Python) under any explicit lsp.toml init_options (explicit wins).
        let init = langenv::merge(
            language,
            &server.server_name,
            &root,
            server.init_options.clone(),
        );

        // Preferred: spawn the language server on clew-server and proxy its
        // stdio, so it runs where the code lives (local today, remote later).
        if let Some(tx) = self.server_tx.clone() {
            // Kill a previous instance for this language (a restart).
            if let Some(old) = self.lsp_procs.remove(&lang) {
                self.proc_feeds.remove(&old);
                let _ = tx.send(clew_protocol::ClientMessage {
                    id: 0,
                    request: clew_protocol::Request::ProcessKill { proc: old },
                });
            }
            let proc = self.next_proc_id;
            self.next_proc_id += 1;
            self.lsp_procs.insert(lang.clone(), proc);

            // Remote: the server resolves and runs its OWN language server, so we
            // never ship a binary path. Local: send the client-resolved binary.
            let spawn = if self.connection.is_remote() {
                clew_protocol::Request::SpawnLsp {
                    proc,
                    language: lang.clone(),
                }
            } else {
                clew_protocol::Request::SpawnProcess {
                    proc,
                    cmd: exe.to_string_lossy().into_owned(),
                    args: args.clone(),
                    cwd: Some(root.to_string_lossy().into_owned()),
                }
            };
            let (client_stdin, client_stdout, feed) = proxy_transport(&tx, proc, spawn);
            self.proc_feeds.insert(proc, feed);

            let lang_done = lang.clone();
            return Task::perform(
                async move {
                    lsp::client::LspClient::connect(client_stdin, client_stdout, &root, init).await
                },
                move |result| Message::LspStartResult {
                    language: lang_done.clone(),
                    result,
                },
            );
        }

        // Fallback: spawn the language server locally.
        Task::perform(
            async move { lsp::client::LspClient::start(&exe, &args, &root, init).await },
            move |result| Message::LspStartResult {
                language: lang.clone(),
                result,
            },
        )
    }

    /// Send `didOpen` for every loaded document of `language` not yet opened.
    pub(crate) fn open_docs_for_language(
        &mut self,
        language: &str,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        let docs: Vec<(PathBuf, Arc<String>)> = self
            .panes
            .iter()
            .flatten()
            .filter(|v| v.lang_key == Some(language))
            .map(|v| (v.abs.clone(), v.source.clone()))
            .collect();
        let mut tasks = Vec::new();
        for (path, source) in docs {
            if self.lsp_opened.insert(path.clone()) {
                client.did_open(&path, language, 1, &source);
            }
            tasks.push(self.inlay_request(&path, client));
        }
        Task::batch(tasks)
    }

    /// Request whole-file inlay hints for `abs` from `client` (no-op unless the
    /// server advertised the capability). Whole-file, not per-viewport: simpler,
    /// and the server caches.
    pub(crate) fn inlay_request(
        &self,
        abs: &Path,
        client: &lsp::client::LspClient,
    ) -> Task<Message> {
        if !client.inlay_hint || !self.show_inlay_hints {
            return Task::none();
        }
        let Some(lines) = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == *abs)
            .map(|v| v.lines.len())
        else {
            return Task::none();
        };
        let client = client.clone();
        let path = abs.to_path_buf();
        let tag = path.clone();
        Task::perform(
            async move { client.inlay_hints(&path, 0, lines).await },
            move |hints| Message::InlayHintsLoaded {
                abs: tag.clone(),
                hints,
            },
        )
    }

    /// Request inlay hints for `abs`, looking its language's server up in the
    /// registry (for callers that don't already hold the client).
    pub(crate) fn inlay_request_lookup(&self, abs: &Path) -> Task<Message> {
        let Some(lang) = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == *abs)
            .and_then(|v| v.lang_key)
        else {
            return Task::none();
        };
        match self.lsp.get(lang) {
            Some(LspSlot::Ready(client)) => self.inlay_request(abs, client),
            _ => Task::none(),
        }
    }

    /// Resolve the definition at a clicked (line, display col) in `pane`.
    pub(crate) fn goto_definition(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        self.goto_request(pane, line, col, GotoKind::Definition)
    }

    /// Kick off a project search from the current query and options.
    pub(crate) fn run_search(&mut self) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        if self.search.query.trim().is_empty() {
            self.search.hits.clear();
            self.search.error = None;
            self.search.ran = false;
            return Task::none();
        }
        self.search.running = true;
        self.search.ran = true;
        self.search.hits.clear();
        let files = project.files.clone();
        let opts = search::SearchOptions {
            query: self.search.query.trim().to_string(),
            regex: self.search.regex,
            case_sensitive: self.search.case_sensitive,
            whole_word: self.search.whole_word,
            include: self.search.include.clone(),
            exclude: self.search.exclude.clone(),
        };

        // Preferred path: run the search on the clew-server over the protocol.
        // Results come back as `Event::SearchResults` (see `handle_server_event`).
        if let Some(tx) = &self.server_tx {
            let request = clew_protocol::Request::Search {
                query: opts.query.clone(),
                regex: opts.regex,
                case_sensitive: opts.case_sensitive,
                whole_word: opts.whole_word,
                include: opts.include.clone(),
                exclude: opts.exclude.clone(),
            };
            if tx
                .send(clew_protocol::ClientMessage { id: 0, request })
                .is_ok()
            {
                return Task::none();
            }
        }

        // Fallback: server not connected yet (or its channel closed) — run the
        // same search in-process so search never depends on handshake timing.
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || search::search(files, opts))
                    .await
                    .unwrap_or_default()
            },
            |result| Message::SearchDone { result },
        )
    }

    /// Apply a completed search to the UI — shared by the server path and the
    /// in-process fallback so both render results identically.
    pub(crate) fn apply_search_result(&mut self, result: search::SearchResult) {
        self.search.running = false;
        self.search.error = result.error.clone();
        self.status = match &result.error {
            Some(e) => e.clone(),
            None if result.hits.len() >= search::MAX_HITS => {
                format!("{}+ matches (capped)", result.hits.len())
            }
            None => format!("{} matches", result.hits.len()),
        };
        self.search.hits = result.hits;
    }

    /// Tell the clew-server which project to scan, so its file-backed flows
    /// (search today) have a file list. Sent on project open and on (re)connect,
    /// whichever happens second; a no-op until both a project and the server are
    /// present.
    pub(crate) fn sync_project_to_server(&self) {
        if let (Some(tx), Some(project)) = (&self.server_tx, &self.project) {
            let request = clew_protocol::Request::OpenProject {
                root: project.root.to_string_lossy().into_owned(),
            };
            let _ = tx.send(clew_protocol::ClientMessage { id: 0, request });
        }
    }

    /// Show LSP references in the Search sidebar (reusing its result list).
    pub(crate) fn show_references(&mut self, refs: Vec<lsp::client::Target>) -> Task<Message> {
        let hits: Vec<SearchHit> = refs
            .into_iter()
            .take(search::MAX_HITS)
            .map(|t| {
                let rel = self.rel_of(&t.path);
                let preview = std::fs::read_to_string(&t.path)
                    .ok()
                    .and_then(|s| s.lines().nth(t.line).map(|l| l.trim().to_string()))
                    .unwrap_or_default();
                SearchHit {
                    abs: t.path,
                    rel,
                    line: t.line + 1,
                    preview,
                }
            })
            .collect();
        self.search.query = "(references)".to_string();
        self.search.ran = true;
        self.search.running = false;
        self.search.hits = hits;
        self.sidebar = SidebarTab::Search;
        self.code_focused = false;
        Task::none()
    }

    /// Run a navigation request from the active pane's cursor.
    pub(crate) fn goto_at_cursor(&mut self, kind: GotoKind) -> Task<Message> {
        let pane = self.active;
        let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
            return Task::none();
        };
        self.goto_request(pane, line, col, kind)
    }

    /// Dispatch an LSP navigation request (definition / references / …) at a
    /// clicked or cursor position.
    pub(crate) fn goto_request(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        kind: GotoKind,
    ) -> Task<Message> {
        // Pull everything we need from the viewer before mutating self.
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key.map(|l| {
                    (
                        l,
                        v.abs.clone(),
                        v.source_line(line).unwrap_or("").to_string(),
                    )
                })
            })
        else {
            return Task::none();
        };

        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready (⌘T to search symbols)");
                return Task::none();
            }
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = format!("{}…", kind.verb());
        let is_references = matches!(kind, GotoKind::References);
        Task::perform(
            async move { client.navigate(kind.method(), &path, line, character).await },
            move |result| {
                if is_references {
                    Message::ReferencesResult { result }
                } else {
                    Message::DefinitionResult { result }
                }
            },
        )
    }

    /// Prepare a call hierarchy at a (display line, col) in `pane`, gated on the
    /// server actually supporting it. Shared by `gc` and the context menu.
    pub(crate) fn call_hierarchy_at(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        let Some((lang, path, source_line)) =
            self.panes.get(pane).and_then(Option::as_ref).and_then(|v| {
                v.lang_key.map(|l| {
                    (
                        l,
                        v.abs.clone(),
                        v.source_line(line).unwrap_or("").to_string(),
                    )
                })
            })
        else {
            return Task::none();
        };
        let client = match self.lsp.get(lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => {
                self.status = format!("No {lang} server ready");
                return Task::none();
            }
        };
        if !client.call_hierarchy {
            self.status = format!("Call hierarchy isn't supported for {lang}");
            return Task::none();
        }
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        self.status = "Building call hierarchy…".into();
        let direction = callgraph::Direction::Incoming;
        Task::perform(
            async move { client.prepare_call_hierarchy(&path, line, character).await },
            move |items| Message::CallHierarchyPrepared {
                direction,
                lang,
                items,
            },
        )
    }

    /// Mark a node loading, then kick its fetch — the panel shows a spinner in
    /// the gap before the children arrive.
    pub(crate) fn fetch_children(&mut self, id: usize) -> Task<Message> {
        if let Some(t) = &mut self.call_graph {
            t.set_loading(id);
        }
        self.call_fetch_task(id)
    }

    /// Off-thread fetch of a call-tree node's callers/callees (direction from
    /// the tree), delivered as `CallHierarchyChildren`.
    pub(crate) fn call_fetch_task(&self, id: usize) -> Task<Message> {
        let Some(tree) = &self.call_graph else {
            return Task::none();
        };
        let client = match self.lsp.get(tree.lang) {
            Some(LspSlot::Ready(c)) => c.clone(),
            _ => return Task::none(),
        };
        let raw = tree.raw_of(id);
        let direction = tree.direction;
        Task::perform(
            async move {
                match direction {
                    callgraph::Direction::Incoming => client.incoming_calls(raw).await,
                    callgraph::Direction::Outgoing => client.outgoing_calls(raw).await,
                }
            },
            move |items| Message::CallHierarchyChildren { id, items },
        )
    }
}
