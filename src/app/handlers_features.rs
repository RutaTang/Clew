//! Message handlers for the major features: explain, walkthrough, files-watch, semantic/ask, time-travel, hover, overview, LSP/index/DAP, server/connect, project-calls.

use crate::app::prelude::*;
use crate::*;

impl App {
    /// Explain the whole project (bottom-up LLM pass), abortable from the UI.
    pub(crate) fn on_explain_project(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Set your Anthropic key in {}", llm::config_hint());
            return Task::none();
        };
        let Some(project) = &self.project else {
            return Task::none();
        };
        let root = project.root.clone();
        let files: Vec<PathBuf> = project.files.iter().map(|f| f.abs.clone()).collect();
        let prev = self.explain.cache.clone();
        let ai = self.ai_client();
        self.explain.generation += 1;
        let generation = self.explain.generation;
        self.explain.running = true;
        self.explain.progress = Some((0, 0));
        self.explain.failed = 0;
        self.status = "Explaining project…".into();
        let stream = iced::stream::channel(256, move |output| {
            let gather_root = root.clone();
            async move {
                let inputs =
                    tokio::task::spawn_blocking(move || gather_explain_inputs(files, gather_root))
                        .await
                        .unwrap_or_default();
                explain_stream(output, inputs, prev, cfg, ai, root, generation).await;
            }
        });
        // Abortable so a long project pass (thousands of LLM calls on a big repo)
        // can be cancelled from the UI; the handle is dropped when the pass
        // finishes (ExplainDone) or is cancelled.
        let (task, handle) = Task::run(stream, |m| m).abortable();
        self.explain.abort = Some(handle);
        task
    }

    /// Cancel the running explain pass (abort remaining calls, keep + save work).
    pub(crate) fn on_cancel_explain(&mut self) -> Task<Message> {
        // Stop the in-flight pass: abort the task (halts further LLM calls) and
        // bump the generation so any already-queued progress messages are
        // ignored. Cached explanations so far are kept.
        if let Some(handle) = self.explain.abort.take() {
            handle.abort();
        }
        self.explain.generation += 1;
        self.explain.running = false;
        self.explain.progress = None;
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            let _ = explain::save(&root, &self.explain.cache);
        }
        self.status = "Explain cancelled".into();
        Task::none()
    }

    /// Fold a finished project explain pass into state and fan out the downstream
    /// refresh (index / overview / open panel), reporting the outcome honestly.
    pub(crate) fn on_explain_done(
        &mut self,
        root: PathBuf,
        generation: u64,
        cache: explain::Cache,
        failed: usize,
        auth_error: Option<String>,
    ) -> Task<Message> {
        if generation != self.explain.generation
            || self.project.as_ref().map(|p| &p.root) != Some(&root)
        {
            return Task::none();
        }
        self.explain.cache = cache;
        self.explain.running = false;
        self.explain.progress = None;
        self.explain.abort = None;
        self.explain.failed = failed;
        let _ = explain::save(&root, &self.explain.cache);
        // Report honestly: a rejected key stops the pass and says why; a partial
        // run names how many failed; only a clean pass claims unqualified success.
        let n = self.explain.cache.len();
        self.status = if let Some(err) = auth_error {
            let reason: String = err
                .lines()
                .next()
                .unwrap_or(&err)
                .chars()
                .take(160)
                .collect();
            format!(
                "Explain stopped — the LLM rejected the request ({reason}). Check your API key in Settings."
            )
        } else if failed > 0 {
            format!("Explained {n} · {failed} failed — check your LLM connection and retry")
        } else {
            format!("Explained {n} functions/files/folders")
        };

        // Propagate the refreshed summaries to the downstream artifacts already in
        // use, each guarded so an unchanged input stays cheap. These run in the
        // background — they never switch the user's view.
        let mut tasks = Vec::new();
        if self.embed_available && !self.explain.cache.is_empty() {
            tasks.push(Task::done(Message::BuildEmbeddings));
        }
        if self.overview.markdown.is_some() && self.overview_inputs_changed() {
            tasks.push(Task::done(Message::GenerateOverview));
        }
        if self.refresh_pending {
            tasks.push(self.request_auto_refresh());
        }
        if let Some(node) = self.explain.view.clone() {
            let fresh_detail = self
                .explain
                .showing_detail
                .then(|| self.explain.cache.get(&node).and_then(|c| c.detail.clone()))
                .flatten();
            tasks.push(match fresh_detail {
                Some(detail) => self.show_detail(node, detail),
                None => self.show_explanation(node),
            });
        }
        Task::batch(tasks)
    }

    /// Re-explain the node in the open panel. Runs the cache-aware project pass
    /// (which regenerates this node and anything that embedded its summary), but
    /// only when the node is already cached — otherwise a single click would
    /// explain the whole project, so point the user at the explicit Explain-All.
    pub(crate) fn on_reexplain_node(&mut self) -> Task<Message> {
        let Some(node) = self.explain.view.clone() else {
            return Task::none();
        };
        if !self.llm_available {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::none();
        }
        if !self.explain.cache.contains_key(&node) {
            self.status =
                "Nothing to re-explain yet — run Explain in the toolbar to explain the project first.".into();
            return Task::none();
        }
        self.explain.cache.remove(&node);
        self.status = "Re-explaining…".into();
        Task::done(Message::ExplainProject)
    }

    /// Generate (or show the cached) block-by-block walkthrough for a function.
    pub(crate) fn on_explain_blocks(&mut self, node: explain::Node) -> Task<Message> {
        let explain::Node::Function { file, name } = node.clone() else {
            return Task::none(); // block detail only applies to functions
        };
        // Already generated? Show the cached walkthrough immediately.
        if let Some(detail) = self.explain.cache.get(&node).and_then(|c| c.detail.clone()) {
            return self.show_detail(node, detail);
        }
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Set your Anthropic key in {}", llm::config_hint());
            return Task::none();
        };
        // Unique-name → summary map so the off-thread gather can attach callee
        // context (ambiguous names resolve to None and are skipped).
        let mut summaries: HashMap<String, Option<String>> = HashMap::new();
        for (n, c) in &self.explain.cache {
            if let explain::Node::Function { name: fname, .. } = n {
                summaries
                    .entry(fname.clone())
                    .and_modify(|e| *e = None)
                    .or_insert_with(|| Some(c.summary.clone()));
            }
        }
        self.status = "Explaining blocks…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let Some((sig, body, callees)) =
                        gather_fn_detail_input(file, &name, &summaries)
                    else {
                        return Err::<String, String>("function body not found".to_string());
                    };
                    Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                })
                .await
                .unwrap_or_else(|_| Err("task join failed".into()));
                match prompt {
                    Ok(p) => ai.complete(cfg, EXPLAIN_BLOCKS_SYSTEM, p, 1024).await,
                    Err(e) => Err(e),
                }
            },
            move |detail| Message::BlocksExplained {
                node: node.clone(),
                detail,
            },
        )
    }

    /// Persist a generated block walkthrough and show it if still on that node.
    pub(crate) fn on_blocks_explained(
        &mut self,
        node: explain::Node,
        detail: Result<String, String>,
    ) -> Task<Message> {
        match detail {
            Ok(md) => {
                // Persist the walkthrough alongside the summary (dropped
                // automatically when the entry is regenerated).
                if let Some(c) = self.explain.cache.get_mut(&node) {
                    c.detail = Some(md.clone());
                    if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
                        let _ = explain::save(&root, &self.explain.cache);
                    }
                }
                self.status = "Explained blocks".into();
                // Only swap the view if the user is still on this node.
                if self.explain.view.as_ref() == Some(&node) {
                    return self.show_detail(node, md);
                }
            }
            Err(e) => self.status = format!("Block explanation failed: {e}"),
        }
        Task::none()
    }

    // ---- Walkthrough-domain handlers (extracted from `update`) ----------------

    /// Generate a scoped AI walkthrough (guided reading tour) of the project.
    pub(crate) fn on_generate_walkthrough(&mut self, scope: String) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if self.project.is_none() {
            return Task::none();
        }
        let project_name = self
            .project
            .as_ref()
            .and_then(|p| p.root.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        let context = self.gather_walkthrough_context();
        let overview = self.overview.markdown.clone();
        let scope = scope.trim().to_string();
        let scope_opt = (!scope.is_empty()).then(|| scope.clone());
        let prompt = walkthrough::prompt(
            &project_name,
            overview.as_deref(),
            &context,
            scope_opt.as_deref(),
        );
        self.walk.generating = Some(scope.clone());
        self.status = "Generating walkthrough…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let resp = ai.complete(cfg, walkthrough::SYSTEM, prompt, 4096).await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone {
                scope: scope.clone(),
                result,
            },
        )
    }

    /// Generate a "review my changes" walkthrough from the diff vs the review base.
    pub(crate) fn on_generate_diff_walkthrough(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some((base, label)) = git::review_base(&root) else {
            self.status =
                "Nothing to review (need a branch vs main/master, or a prior commit)".into();
            return Task::none();
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        // Collect the change: intent, changed files + their symbols, patch.
        let commits = git::commit_subjects(&root, &base);
        let mut changed_text = String::new();
        for (rel, ch) in git::changed_files(&root, &base) {
            changed_text.push_str(&format!("{ch} {rel}\n"));
            if let Some(syms) = self.symbol_index_by_file.get(&root.join(&rel)) {
                for s in syms.iter().filter(|s| {
                    matches!(
                        s.kind.as_str(),
                        "function" | "method" | "struct" | "class" | "enum" | "trait"
                    )
                }) {
                    changed_text.push_str(&format!("    {} {} @ L{}\n", s.kind, s.name, s.line));
                }
            }
        }
        let patch = git::range_patch(&root, &base, 12000);
        let prompt =
            walkthrough::diff_prompt(&project_name, &label, &commits, &changed_text, &patch);
        // A sentinel scope so the library shows it as a change review and
        // Regenerate re-runs the diff (not a normal scoped tour).
        let scope = format!("@diff {label}");
        self.walk.generating = Some(scope.clone());
        self.status = "Reviewing changes…".into();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let resp = ai
                    .complete(cfg, walkthrough::DIFF_SYSTEM, prompt, 4096)
                    .await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone {
                scope: scope.clone(),
                result,
            },
        )
    }

    /// Fold a finished walkthrough into the library and open it (retry once on a
    /// malformed-JSON parse error).
    pub(crate) fn on_walkthrough_done(
        &mut self,
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    ) -> Task<Message> {
        self.walk.generating = None;
        match result {
            Ok(mut wt) => {
                // Drop steps that don't resolve to a real project file.
                wt.steps
                    .retain(|s| self.resolve_walk_file(&s.file).is_some());
                if wt.steps.is_empty() {
                    self.status = "Walkthrough had no valid steps".into();
                    return Task::none();
                }
                wt.scope = scope.clone();
                // Upsert by scope: regenerating a tour replaces it in place, a
                // fresh scope is appended.
                let idx = match self.walk.library.iter().position(|w| w.scope == scope) {
                    Some(i) => {
                        self.walk.library[i] = wt;
                        i
                    }
                    None => {
                        self.walk.library.push(wt);
                        self.walk.library.len() - 1
                    }
                };
                self.walk.open = Some(idx);
                self.walk.step = 0;
                self.sidebar = SidebarTab::Walk;
                self.show_left_sidebar = true;
                self.walk.retried = false;
                if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
                    && let Err(e) = walkthrough::save_library(&root, &self.walk.library)
                {
                    self.status = format!("Could not save walkthrough: {e}");
                }
                self.walkthrough_goto(0)
            }
            Err(e) => {
                // The model occasionally returns malformed JSON — retry the
                // generation once before surfacing the failure.
                if e.starts_with("parse") && !self.walk.retried {
                    self.walk.retried = true;
                    self.status = "Retrying walkthrough…".into();
                    return if scope.starts_with("@diff") {
                        Task::done(Message::GenerateDiffWalkthrough)
                    } else {
                        Task::done(Message::GenerateWalkthrough(scope))
                    };
                }
                self.walk.retried = false;
                self.status = format!("Walkthrough failed: {e}");
                Task::none()
            }
        }
    }

    /// A watched source file changed, so the understanding may be stale. Start a
    /// refresh now if the cooldown has lifted and nothing is running; otherwise
    /// mark it pending for the next `Tick` past the window, so no change is
    /// dropped. Only refreshes what already exists — the first build of each
    /// artifact stays an explicit user action.
    pub(crate) fn on_files_changed(&mut self, paths: Vec<PathBuf>) -> Task<Message> {
        let open: HashSet<PathBuf> = self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
        // Every file the tree currently lists. The registry only tracks
        // source files, so it can't tell a new/removed non-source file
        // from an edit to one — the tree's own file list can.
        let known: HashSet<&PathBuf> = self
            .project
            .as_ref()
            .map(|p| p.files.iter().map(|f| &f.abs).collect())
            .unwrap_or_default();
        let mut seen = HashSet::new();
        // Split the changed paths in two. Content-tracked files (open,
        // already tracked, or a source file we index) are read + hashed
        // for a real content refresh. Everything else that changed (a
        // .txt, a .json) can't change content we display, but it can be
        // the *creation* or *deletion* of a tree entry — so it gets a
        // cheap existence probe (stat, no read) instead. The probe pairs
        // each path with whether the tree currently lists it; a mismatch
        // with on-disk existence is a create/delete that needs a rescan.
        let mut candidates: Vec<(PathBuf, incremental::Version)> = Vec::new();
        let mut probes: Vec<(PathBuf, bool)> = Vec::new();
        for p in paths {
            if !seen.insert(p.clone()) {
                continue;
            }
            if open.contains(&p) || self.registry.is_tracked(&p) || highlight::detect(&p).is_some()
            {
                let v = self.registry.version(&p).unwrap_or(0);
                candidates.push((p, v));
            } else {
                let in_tree = known.contains(&p);
                probes.push((p, in_tree));
            }
        }
        if candidates.is_empty() && probes.is_empty() {
            return Task::none();
        }
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let events = watch::rehash(candidates);
                    let fs_structural = watch::structural_changes(&probes);
                    (events, fs_structural)
                })
                .await
                .unwrap_or_default()
            },
            |(events, fs_structural)| Message::FilesRehashed {
                events,
                fs_structural,
            },
        )
    }

    pub(crate) fn on_files_rehashed(
        &mut self,
        events: Vec<watch::FileEvent>,
        fs_structural: bool,
    ) -> Task<Message> {
        let mut tasks = Vec::new();
        let mut index_dirty = false;
        // Non-source creations/deletions are already decided by the
        // existence probe in `FilesChanged`; source ones are folded in
        // per event below.
        let mut structural = fs_structural;
        let mut graph_dirty = false;
        let mut refreshed = 0usize;
        let mut touched: Vec<PathBuf> = Vec::new();
        // One resolver for the whole batch, over the current file set. A
        // structural change re-resolves the whole graph later (once the
        // rescan lands the new file set); here we only refresh out-edges.
        let resolver = self.import_resolver();
        for event in events {
            match event {
                watch::FileEvent::Modified(c) => {
                    touched.push(c.path.clone());
                    let lang_key = highlight::detect(&c.path);
                    // An untracked *source* file appearing is its creation,
                    // so the tree must gain it. Non-source create/delete is
                    // handled by the existence probe, which keeps an open
                    // non-source file merely being edited from looking
                    // structural here.
                    structural |= lang_key.is_some() && !self.registry.is_tracked(&c.path);
                    self.registry.set(c.path.clone(), c.hash);

                    // Re-index this one file in place (open or not).
                    if let Some(lang) = lang_key {
                        let rel = self.rel_of(&c.path);
                        let syms = index::file_symbols(&c.path, &rel, &c.content, lang);
                        if syms.is_empty() {
                            index_dirty |= self.symbol_index_by_file.remove(&c.path).is_some();
                        } else {
                            self.symbol_index_by_file.insert(c.path.clone(), syms);
                            index_dirty = true;
                        }
                        // Re-extract this file's imports and refresh its
                        // out-edges in the graph.
                        if let Some(res) = &resolver {
                            let raw = index::file_imports(&c.content, lang);
                            graph_dirty |= self.import_graph.set_file(
                                c.path.clone(),
                                raw,
                                res,
                                highlight::detect,
                            );
                        }
                    }

                    // Refresh every pane showing this file, keeping the
                    // reader's scroll/caret/folds so nothing jumps.
                    let mut on_screen = false;
                    for slot in &mut self.panes {
                        if let Some(v) = slot.as_mut().filter(|v| v.abs == c.path) {
                            let lines = highlight::plain_lines(&c.content);
                            v.reload(c.content.clone(), lines);
                            on_screen = true;
                        }
                    }
                    if on_screen {
                        refreshed += 1;
                        tasks.push(self.content_tasks(c.path.clone(), c.content.clone(), lang_key));
                        if let Some(lang) = lang_key
                            && let Some(LspSlot::Ready(client)) = self.lsp.get(lang)
                        {
                            self.lsp_doc_rev += 1;
                            client.did_change(&c.path, self.lsp_doc_rev, &c.content);
                        }
                    }
                }
                watch::FileEvent::Deleted(path) => {
                    touched.push(path.clone());
                    structural = true;
                    self.registry.remove(&path);
                    index_dirty |= self.symbol_index_by_file.remove(&path).is_some();
                    self.import_graph.remove_file(&path);
                    graph_dirty = true;
                    if self.panes.iter().flatten().any(|v| v.abs == path) {
                        self.status = format!("{} was deleted on disk", self.rel_of(&path));
                    }
                }
            }
        }
        if index_dirty {
            self.rebuild_symbol_index();
        }
        // Keep the reading trail anchored across edits: re-point each
        // changed file's history entries to their symbol's new line. A
        // deleted file has no symbols left, so its entries keep their line
        // (clicking one just reports the file is gone).
        let mut trail_moved = false;
        for path in &touched {
            let symbols: Vec<(String, usize)> = self
                .symbol_index_by_file
                .get(path)
                .map(|syms| {
                    syms.iter()
                        .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                        .map(|s| (s.name.clone(), s.line))
                        .collect()
                })
                .unwrap_or_default();
            trail_moved |= self.history.reanchor(path, &symbols);
        }
        if trail_moved {
            self.save_history();
        }
        // A pure content change only refreshes out-edges, so update the
        // tree now. A structural change re-resolves the whole graph once
        // the rescan lands the new file set (see `TreeUpdated`).
        if graph_dirty && !structural {
            self.import_cycles = self.import_graph.cycles();
            self.refresh_import_tree();
        }
        // If a file the open call hierarchy references changed, the tree
        // may now be out of date — flag it (re-run `gc` to refresh).
        if let Some(t) = &mut self.call_graph
            && !t.stale
            && touched.iter().any(|p| t.depends_on(p))
        {
            t.stale = true;
        }
        // Keep the open LSP-precise call graph fresh: re-query just the
        // changed files' functions, coalescing while a refine is running.
        if self.project_calls.precise && self.overlay == Some(Overlay::ProjectCalls) {
            let changed: HashSet<PathBuf> = touched
                .iter()
                .filter(|p| highlight::detect(p).is_some())
                .cloned()
                .collect();
            if !changed.is_empty() {
                self.project_calls.precise_pending.extend(changed);
                if self.project_calls.refine_progress.is_none() {
                    let pending = std::mem::take(&mut self.project_calls.precise_pending);
                    tasks.push(self.refine_incremental(pending));
                }
            }
        }
        // A created/deleted/renamed file changes the tree and Cmd+P list;
        // rebuild them off-thread (the watcher already debounced the burst).
        if structural && let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            tasks.push(self.rescan_tree(root));
        }
        if refreshed == 1 {
            self.status = "Refreshed a file changed on disk".to_string();
        } else if refreshed > 1 {
            self.status = format!("Refreshed {refreshed} files changed on disk");
        }
        // A source file changed → the understanding (explanations →
        // index → overview) may be stale. Auto-refresh it, throttled to
        // AUTO_REFRESH_MIN_INTERVAL so an edit burst coalesces into one
        // pass (see `request_auto_refresh`).
        if touched.iter().any(|p| highlight::detect(p).is_some()) {
            tasks.push(self.request_auto_refresh());
        }
        Task::batch(tasks)
    }

    pub(crate) fn on_build_embeddings(&mut self) -> Task<Message> {
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        if self.explain.cache.is_empty() {
            self.status = "Run Explain All first — the index embeds the summaries".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let nodes = self.gather_embed_nodes();
        let existing = std::mem::take(&mut self.embed_index);
        self.building_embeddings = true;
        self.status = "Building semantic index…".into();
        let ai = self.ai_client();
        Task::perform(
            async move { build_embeddings(&ai, &cfg, nodes, existing).await },
            move |result| Message::EmbeddingsBuilt {
                root: root.clone(),
                result,
            },
        )
    }

    pub(crate) fn on_semantic_search(&mut self) -> Task<Message> {
        let query = self.semantic_query.trim().to_string();
        if query.is_empty() {
            return Task::none();
        }
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        if self.embed_index.entries.is_empty() {
            self.status = "Build the semantic index first (Semantic tab → Build index)".into();
            return Task::none();
        }
        self.searching_semantic = true;
        let label = query.clone();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    embed::embed_batch(&cfg, std::slice::from_ref(&query))
                        .map(|mut v| v.pop().unwrap_or_default())
                })
                .await
                .unwrap_or_else(|_| Err("task join failed".into()))
            },
            move |result| Message::SemanticResults {
                query: label.clone(),
                result,
            },
        )
    }

    pub(crate) fn on_ask_submit(&mut self) -> Task<Message> {
        let question = self.ask_input.trim().to_string();
        if question.is_empty() {
            return Task::none();
        }
        let Some(_lcfg) = llm::Config::load() else {
            self.status = "Configure an LLM provider in Settings to ask".into();
            return Task::none();
        };
        // Agent mode: the server explores the project with tools, so no
        // semantic index is required. Retrieval mode remains the fallback
        // (no server channel, or the server can't run an agent turn).
        if self.server_tx.is_some() && self.agent_stream.is_none() {
            self.ask_input.clear();
            return self.start_agent_ask(question);
        }
        self.on_ask_submit_rag(question)
    }

    /// The pre-agent Ask path: retrieval (embed → top-K) + one streamed
    /// completion. Kept as the fallback when an agent turn can't run.
    pub(crate) fn on_ask_submit_rag(&mut self, question: String) -> Task<Message> {
        // Semantic retrieval needs an embedding index. But when the
        // debugger is paused or a selection is pinned, that live context
        // is the grounding — allow asking without an index.
        let ecfg = embed::Config::load();
        let has_index = !self.embed_index.entries.is_empty() && ecfg.is_some();
        let grounded = self.debug_context().is_some() || !self.ask_pins.is_empty();
        if !has_index && !grounded {
            // Be specific when a pass is already building the index, so a
            // question asked mid-"Explain All" doesn't read as a silent no-op.
            self.status = if self.explain.running || self.building_embeddings {
                "Ask needs the semantic index — it's building now (finish Explain All), then re-ask"
                    .into()
            } else {
                "Build the semantic index first (FIND tab → Build index)".into()
            };
            return Task::none();
        }
        self.ask_input.clear();
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;
        self.asking = true;
        match ecfg.filter(|_| has_index) {
            Some(ecfg) => {
                let q = question.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            embed::embed_batch(&ecfg, std::slice::from_ref(&q))
                                .map(|mut v| v.pop().unwrap_or_default())
                        })
                        .await
                        .unwrap_or_else(|_| Err("task join failed".into()))
                    },
                    move |qvec| Message::AskRetrieved {
                        question: question.clone(),
                        qvec,
                    },
                )
            }
            // No index: skip retrieval, answer from the live grounding.
            None => Task::done(Message::AskRetrieved {
                question,
                qvec: Ok(Vec::new()),
            }),
        }
    }

    pub(crate) fn on_ask_retrieved(
        &mut self,
        question: String,
        qvec: Result<Vec<f32>, String>,
    ) -> Task<Message> {
        let qvec = match qvec {
            Ok(v) => v,
            Err(e) => {
                self.asking = false;
                self.status = format!("Ask failed: {e}");
                return Task::none();
            }
        };
        let Some(lcfg) = llm::Config::load() else {
            self.asking = false;
            return Task::none();
        };

        // Build the context node set: the freshly retrieved top-K, plus
        // the function under the cursor and the previous turn's sources —
        // so a follow-up ("why does it…") still has that code in view.
        // Dedup, keep the highest-scoring, cap the total.
        const MAX_CTX: usize = 18;
        let mut sources: Vec<(explain::Node, f32)> = embed::search(&self.embed_index, &qvec, 16)
            .into_iter()
            .map(|(n, s)| (n.clone(), s))
            .collect();
        let mut carried: Vec<explain::Node> = Vec::new();
        if let Some(t) = self.cursor_target() {
            carried.push(t);
        }
        if let Some(prev) = self.ask_turns.last() {
            carried.extend(prev.sources.iter().map(|(n, _)| n.clone()));
        }
        for n in carried {
            if !sources.iter().any(|(c, _)| *c == n) {
                let s = self.node_score(&n, &qvec);
                sources.push((n, s));
            }
        }
        // Broaden recall for cross-cutting questions: pull in the
        // import-graph neighbours of the top few non-hub files, so a
        // subsystem that feeds or uses the retrieved code (e.g. the file
        // watcher behind the indexer) can enter the context. Neighbours
        // still compete on relevance via `node_score`, with a small
        // connectivity nudge, and are capped so they can't crowd out
        // direct hits. Hub files (huge fan) are skipped — expanding them
        // would flood the context with loosely-related neighbours.
        {
            let node_file = |n: &explain::Node| match n {
                explain::Node::Function { file, .. } => file.clone(),
                explain::Node::File(p) | explain::Node::Folder(p) => p.clone(),
            };
            let mut have: HashSet<PathBuf> = sources.iter().map(|(n, _)| node_file(n)).collect();
            let seeds: Vec<PathBuf> = sources
                .iter()
                .take(4)
                .map(|(n, _)| node_file(n))
                .filter(|f| self.import_graph.fan_in(f) + self.import_graph.fan_out(f) <= 20)
                .collect();
            let mut added = 0usize;
            for f in seeds {
                if added >= 4 {
                    break;
                }
                let mut neigh: Vec<PathBuf> = self
                    .import_graph
                    .imports(&f)
                    .iter()
                    .filter_map(|e| match &e.target {
                        imports::Target::Internal(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect();
                neigh.extend(self.import_graph.importers(&f));
                neigh.sort();
                neigh.dedup();
                for nf in neigh {
                    if added >= 4 {
                        break;
                    }
                    if have.contains(&nf) {
                        continue;
                    }
                    let node = explain::Node::File(nf.clone());
                    if !self.explain.cache.contains_key(&node) {
                        continue;
                    }
                    let s = self.node_score(&node, &qvec) + 0.05;
                    sources.push((node, s));
                    have.insert(nf);
                    added += 1;
                }
            }
        }
        sources.sort_by(|a, b| b.1.total_cmp(&a.1));
        sources.truncate(MAX_CTX);

        // Assemble the context: the pinned selection first (if any), then
        // the ranked node context.
        let nodes: Vec<explain::Node> = sources.iter().map(|(n, _)| n.clone()).collect();
        let mut context = String::new();
        // If the debugger is paused, ground the answer in the live state.
        if let Some(state) = self.debug_context() {
            context.push_str(&state);
        }
        for pin in &self.ask_pins {
            context.push_str(&format!(
                "### Selected code — {} (L{})\n```\n{}\n```\n\n",
                pin.rel, pin.line, pin.code
            ));
        }
        context.push_str(&self.gather_ask_context(&nodes));

        // Replay recent turns as chat history so follow-ups resolve.
        const HIST_TURNS: usize = 6;
        let mut messages: Vec<llm::ChatMsg> = Vec::new();
        let start = self.ask_turns.len().saturating_sub(HIST_TURNS);
        for turn in &self.ask_turns[start..] {
            messages.push(llm::ChatMsg::user(turn.question.clone()));
            messages.push(llm::ChatMsg::assistant(turn.answer_md.clone()));
        }
        messages.push(llm::ChatMsg::user(format!(
            "Question: {question}\n\nCode context:\n{context}"
        )));

        self.start_ask_stream(question, sources, lcfg, ASK_SYSTEM.to_string(), messages)
    }

    pub(crate) fn on_ask_stream_ended(&mut self, error: Option<String>) -> Task<Message> {
        self.asking = false;
        if let Some(e) = &error {
            self.status = format!("Ask failed: {e}");
        }
        // Finalize the open turn: on error with no text, show why; then
        // render the accumulated markdown as rich segments.
        let md = match self.ask_turns.last_mut() {
            Some(turn) => {
                turn.streaming = false;
                if let Some(e) = &error
                    && turn.answer_md.trim().is_empty()
                {
                    turn.answer_md = format!("*Couldn't answer: {e}*");
                }
                turn.answer_md.clone()
            }
            None => return Task::none(),
        };
        let (prepared, task) = self.prepare_segments(&md);
        if let Some(turn) = self.ask_turns.last_mut() {
            turn.answer = prepared;
        }
        let to_bottom = operation::scroll_to(
            ui::ask_scroll_id(),
            AbsoluteOffset {
                x: 0.0,
                y: f32::MAX,
            },
        );
        Task::batch([task, to_bottom])
    }

    /// The agent made a tool call: append its chip to the open turn and keep
    /// the conversation pinned to the bottom.
    pub(crate) fn on_agent_stepped(&mut self, step: AgentStep) -> Task<Message> {
        if let Some(turn) = self.ask_turns.last_mut()
            && turn.streaming
        {
            turn.steps.push(step);
        }
        operation::scroll_to(
            ui::ask_scroll_id(),
            AbsoluteOffset {
                x: 0.0,
                y: f32::MAX,
            },
        )
    }

    /// An agent turn finished. On a start-up failure (nothing explored, nothing
    /// answered), fall back to the retrieval path so the question still gets an
    /// answer — e.g. an older server or a provider without tool support.
    pub(crate) fn on_agent_turn_ended(&mut self, error: Option<String>) -> Task<Message> {
        self.agent_stream = None;
        let bare_failure = error.as_deref().is_some_and(|e| e != "stopped")
            && self.ask_turns.last().is_some_and(|t| {
                t.streaming && t.steps.is_empty() && t.answer_md.trim().is_empty()
            });
        if bare_failure {
            let turn = self.ask_turns.pop().expect("checked above");
            self.status = format!(
                "Agent mode unavailable ({}) — answering from the semantic index",
                error.unwrap_or_default()
            );
            return self.on_ask_submit_rag(turn.question);
        }
        self.on_ask_stream_ended(error)
    }

    /// Stop the in-flight agent turn; the server closes it with `AgentDone`.
    pub(crate) fn on_agent_stop(&mut self) -> Task<Message> {
        if let (Some(stream), Some(tx)) = (self.agent_stream, &self.server_tx) {
            let _ = tx.send(clew_protocol::ClientMessage {
                id: 0,
                request: clew_protocol::Request::AgentStop { stream },
            });
        }
        Task::none()
    }

    pub(crate) fn on_ask_about_selection(&mut self) -> Task<Message> {
        // Add the right-clicked pane's selection (or the active pane's) as a
        // context chip, open the panel, and focus the input.
        let pane = self
            .context_menu
            .take()
            .map(|m| m.pane)
            .unwrap_or(self.active);
        match self.selection_pin(pane) {
            Some(pin) => {
                // Skip an exact duplicate (same file, line and code).
                let dup = self
                    .ask_pins
                    .iter()
                    .any(|p| p.file == pin.file && p.line == pin.line && p.code == pin.code);
                if !dup {
                    self.ask_pins.push(pin);
                }
                self.show_bottom = true;
                self.bottom_tab = BottomTab::Ask;
                self.code_focused = false; // the Ask input takes focus
                self.status = "Added selection to Ask — ask your question".into();
                operation::focus(ui::ask_input_id())
            }
            None => {
                self.status = "Select some code first, then Add to Ask".into();
                Task::none()
            }
        }
    }

    pub(crate) fn on_why_is_this_here(&mut self) -> Task<Message> {
        let menu = self.context_menu.take();
        let pane = menu.map(|m| m.pane).unwrap_or(self.active);
        let menu_line = menu.map(|m| m.line);
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(v) = self.panes.get(pane).and_then(Option::as_ref) else {
            return Task::none();
        };
        let Some(git) = v.git.clone() else {
            self.status = "No git history for this file".into();
            return Task::none();
        };
        // Target line range (0-based inclusive): the selection, else the
        // clicked/caret line.
        let (l0, l1) = match v.selection_ordered() {
            Some(((a, _), (b, _))) => (a, b),
            None => match menu_line.or(v.caret.map(|(l, _)| l)) {
                Some(l) => (l, l),
                None => return Task::none(),
            },
        };
        // Distinct committed commits touching the range (a few at most).
        let mut seen = HashSet::new();
        let mut commits: Vec<(String, String)> = Vec::new();
        for line in l0..=l1 {
            if let Some(b) = git.blame_for(line)
                && !b.uncommitted
                && !b.commit.is_empty()
                && seen.insert(b.commit.clone())
            {
                commits.push((b.commit.clone(), b.summary.clone()));
                if commits.len() >= 4 {
                    break;
                }
            }
        }
        if commits.is_empty() {
            self.status = "This code isn't committed yet — no history to explain".into();
            return Task::none();
        }
        let last = l1.min(l0 + 40); // cap the snippet
        let code: String = (l0..=last)
            .filter_map(|l| v.source_line(l))
            .collect::<Vec<_>>()
            .join("\n");
        let rel = v.rel.clone();
        let title = if l0 == l1 {
            format!("Why line {} exists", l0 + 1)
        } else {
            format!("Why lines {}–{} exist", l0 + 1, l1 + 1)
        };
        self.blame_why = Some(BlameWhy {
            title: title.clone(),
            commits: commits.clone(),
            loading: true,
            prepared: Vec::new(),
        });
        self.status = "Explaining why…".into();
        let commits_ctx = commits.clone();
        let ai = self.ai_client();
        Task::perform(
            async move {
                // Build the prompt off-thread (git diffs), then complete.
                let prompt = tokio::task::spawn_blocking(move || {
                    let mut ctx = format!(
                        "Code ({rel}, lines {}-{}):\n```\n{code}\n```\n\n",
                        l0 + 1,
                        last + 1
                    );
                    for (sha, _) in &commits_ctx {
                        let msg = git::commit_message(&root, sha).unwrap_or_default();
                        let diff = git::commit_file_diff(&root, sha, &rel, 3000);
                        ctx.push_str(&format!(
                            "### Commit {sha}\nMessage:\n{msg}\n\nWhat it changed here:\n```\n{diff}\n```\n\n"
                        ));
                    }
                    format!("Why does this code exist?\n\n{ctx}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, WHY_SYSTEM, prompt, 512).await
            },
            move |result| Message::BlameWhyDone {
                title,
                commits,
                result,
            },
        )
    }

    pub(crate) fn on_time_travel_start(&mut self, symbol: bool) -> Task<Message> {
        self.show_tools_menu = false;
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            self.status = "Time Travel needs a git repository".into();
            return Task::none();
        };
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let (abs, rel, lang) = (v.abs.clone(), v.rel.clone(), v.lang_key);
        // Scope: the innermost code block (any kind — function, struct,
        // enum, class, trait, …) whose span contains the caret, else the
        // whole file. When re-scoping mid-session the caret comes from the
        // historical view; either way the block's NAME is resolved to its
        // HEAD line range, since `git log -L` interprets ranges vs HEAD.
        let scope = if symbol {
            let name = {
                let (line1, syms): (usize, &[outline::Symbol]) = match self
                    .time_travel
                    .as_ref()
                    .and_then(|t| t.viewer.as_ref().map(|hv| (t.caret, hv)))
                {
                    Some((c, hv)) => (c.map(|(l, _)| l + 1).unwrap_or(1), &hv.symbols),
                    None => (v.caret.map(|(l, _)| l + 1).unwrap_or(1), &v.symbols),
                };
                syms.iter()
                    .filter(|s| s.line <= line1 && line1 <= s.end_line && s.end_line >= s.line)
                    .min_by_key(|s| s.end_line.saturating_sub(s.line))
                    .map(|s| s.name.clone())
            };
            name.and_then(|n| {
                v.symbols
                    .iter()
                    .find(|s| s.name == n)
                    .map(|s| TimeScope::Symbol {
                        name: s.name.clone(),
                        kind: s.kind.clone(),
                        start: s.line,
                        end: s.end_line,
                    })
            })
            .unwrap_or(TimeScope::File)
        } else {
            TimeScope::File
        };
        self.time_gen += 1;
        let generation = self.time_gen;
        self.status = "Loading history…".into();
        let (scope_task, rel_task) = (scope.clone(), rel.clone());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || match &scope_task {
                    TimeScope::File => git::file_history(&root, &rel_task, 200),
                    TimeScope::Symbol { start, end, .. } => {
                        git::symbol_history(&root, &rel_task, *start, *end, 200)
                    }
                })
                .await
                .unwrap_or_default()
            },
            move |commits| Message::TimeTravelReady {
                generation,
                abs: abs.clone(),
                rel: rel.clone(),
                lang,
                scope: scope.clone(),
                commits,
            },
        )
    }

    pub(crate) fn on_time_travel_ready(
        &mut self,
        generation: u64,
        abs: PathBuf,
        rel: String,
        lang: Option<&'static str>,
        scope: TimeScope,
        commits: Vec<git::HistCommit>,
    ) -> Task<Message> {
        if generation != self.time_gen {
            return Task::none();
        }
        if commits.is_empty() {
            self.status = match &scope {
                TimeScope::Symbol { name, .. } => format!("No git history for `{name}`"),
                TimeScope::File => "No git history for this file".into(),
            };
            return Task::none();
        }
        self.status.clear();
        // Start where the reader was: keep the existing session's scroll
        // and caret when re-scoping (so a scope toggle doesn't snap back),
        // else take them from the live file on first entry.
        let (scroll_y, caret) = self
            .time_travel
            .as_ref()
            .map(|t| (t.scroll_y, t.caret))
            .or_else(|| self.active_viewer().map(|v| (v.scroll_y, v.caret)))
            .unwrap_or((0.0, None));
        self.time_travel = Some(TimeTravel {
            abs,
            rel,
            lang,
            scope,
            commits,
            idx: 0,
            viewer: None,
            scroll_y,
            caret,
            focus_line: None,
            loading: true,
            generation,
            why: HashMap::new(),
            why_loading: false,
            story: None,
            story_loading: false,
        });
        Task::done(Message::TimeTravelGoto(0))
    }

    pub(crate) fn on_time_travel_goto(&mut self, idx: usize) -> Task<Message> {
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let (commit, lang, focus_name) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let Some(commit) = tt.commits.get(idx) else {
                return Task::none();
            };
            (
                commit.clone(),
                tt.lang,
                tt.scope.symbol_name().map(str::to_string),
            )
        };
        self.time_gen += 1;
        let generation = self.time_gen;
        if let Some(tt) = self.time_travel.as_mut() {
            tt.idx = idx;
            tt.loading = true;
            tt.generation = generation;
        }
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let content =
                        git::file_at(&root, &commit.sha, &commit.path).unwrap_or_default();
                    let lines = highlight::highlight_lines(&content, lang);
                    let symbols = lang
                        .map(|l| outline::extract(&content, l))
                        .unwrap_or_default();
                    let added = git::commit_added_lines(&root, &commit.sha, &commit.path);
                    let focus_line = focus_name
                        .and_then(|n| symbols.iter().find(|s| s.name == n).map(|s| s.line));
                    Box::new(TimeStep {
                        lines,
                        content,
                        symbols,
                        added,
                        focus_line,
                    })
                })
                .await
                .ok()
            },
            move |step| match step {
                Some(step) => Message::TimeTravelStep {
                    generation,
                    idx,
                    step,
                },
                None => Message::TimeTravelExit,
            },
        )
    }

    pub(crate) fn on_time_travel_step(
        &mut self,
        generation: u64,
        idx: usize,
        step: Box<TimeStep>,
    ) -> Task<Message> {
        if generation != self.time_gen {
            return Task::none();
        }
        let line_height = self.line_height();
        let Some(tt) = self.time_travel.as_mut() else {
            return Task::none();
        };
        tt.loading = false;
        tt.idx = idx;
        tt.focus_line = step.focus_line;
        let n = step.lines.len();
        let status: Vec<Option<git::ChangeKind>> = (0..n)
            .map(|i| {
                step.added
                    .contains(&(i + 1))
                    .then_some(git::ChangeKind::Added)
            })
            .collect();
        let source = std::sync::Arc::new(step.content);
        let mut v =
            viewer::Viewer::new(tt.abs.clone(), tt.rel.clone(), tt.lang, source, step.lines);
        v.symbols = step.symbols;
        v.highlighted = true;
        v.git = Some(std::sync::Arc::new(git::GitInfo {
            blame: Vec::new(),
            status,
            deleted_at: HashSet::new(),
        }));
        let last_line = v.lines.len().saturating_sub(1);
        // Block scope: bring the block into view. File scope: keep the
        // reader's caret and scroll position (carried from entry).
        if let Some(fl) = step.focus_line {
            let head = (fl.saturating_sub(1), 0);
            v.caret = Some(head);
            tt.caret = Some(head);
            let y = v.scroll_offset_for(Some(fl), line_height);
            v.scroll_y = y;
            tt.scroll_y = y;
        } else {
            // Clamp the carried caret to this revision's bounds (older
            // revisions are shorter, and lines may be shorter too).
            v.caret = tt.caret.map(|(l, c)| {
                let l = l.min(last_line);
                let cols = v
                    .lines
                    .get(l)
                    .map(|ln| {
                        ln.spans
                            .iter()
                            .map(|(t, _)| t.chars().count())
                            .sum::<usize>()
                    })
                    .unwrap_or(0);
                (l, c.min(cols))
            });
            v.scroll_y = tt.scroll_y;
        }
        tt.viewer = Some(v);
        // Explicitly scroll the (freshly mounted) historical scrollable to
        // the carried offset — iced doesn't preserve scroll across the swap.
        let y = self.time_travel.as_ref().map(|t| t.scroll_y).unwrap_or(0.0);
        operation::scroll_to(
            ui::code_scroll_id(self.active),
            AbsoluteOffset { x: 0.0, y },
        )
    }

    pub(crate) fn on_time_travel_select_start(&mut self, line: usize, col: usize) -> Task<Message> {
        let extend = self.modifiers.shift();
        let mut started = false;
        if let Some(tt) = self.time_travel.as_mut() {
            let head = (line, col);
            tt.caret = Some(head); // persist across scrubs
            if let Some(v) = tt.viewer.as_mut() {
                match (extend, v.selection) {
                    (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                    _ => v.selection = Some((head, head)),
                }
                v.caret = Some(head);
                started = true;
            }
        }
        if started {
            self.selecting = true;
        }
        Task::none()
    }

    pub(crate) fn on_time_travel_why(&mut self) -> Task<Message> {
        let (root, sha, path, subject) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let Some(c) = tt.commits.get(tt.idx) else {
                return Task::none();
            };
            if tt.why.contains_key(&c.sha) {
                return Task::none(); // already have it
            }
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            (root, c.sha.clone(), c.path.clone(), c.subject.clone())
        };
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if let Some(tt) = self.time_travel.as_mut() {
            tt.why_loading = true;
        }
        let generation = self.time_gen;
        let sha2 = sha.clone();
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let msg = git::commit_message(&root, &sha2).unwrap_or(subject);
                    let diff = git::commit_file_diff(&root, &sha2, &path, 8000);
                    format!("Commit message:\n{msg}\n\nDiff of {path}:\n{diff}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, TIME_WHY_SYSTEM, prompt, 220).await
            },
            move |result| Message::TimeTravelWhyDone {
                generation,
                sha,
                result,
            },
        )
    }

    pub(crate) fn on_time_travel_story(&mut self) -> Task<Message> {
        // Toggle: if a story is already showing, hide it.
        if self.time_travel.as_ref().is_some_and(|t| t.story.is_some()) {
            if let Some(tt) = self.time_travel.as_mut() {
                tt.story = None;
            }
            return Task::none();
        }
        let (root, name, commits) = {
            let Some(tt) = self.time_travel.as_ref() else {
                return Task::none();
            };
            let TimeScope::Symbol { name, kind, .. } = &tt.scope else {
                return Task::none();
            };
            let name = format!("{kind} {name}");
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            let commits: Vec<(String, String, String)> = tt
                .commits
                .iter()
                .take(12)
                .map(|c| (c.sha.clone(), c.subject.clone(), c.path.clone()))
                .collect();
            (root, name, commits)
        };
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        };
        if let Some(tt) = self.time_travel.as_mut() {
            tt.story_loading = true;
        }
        let generation = self.time_gen;
        let ai = self.ai_client();
        Task::perform(
            async move {
                let prompt = tokio::task::spawn_blocking(move || {
                    let mut ctx = String::new();
                    for (sha, subject, path) in &commits {
                        let short = &sha[..sha.len().min(8)];
                        let diff = git::commit_file_diff(&root, sha, path, 2500);
                        ctx.push_str(&format!(
                            "### {short} — {subject}\n```diff\n{diff}\n```\n\n"
                        ));
                    }
                    format!("Code block: {name}\n\nCommits (newest first):\n{ctx}")
                })
                .await
                .unwrap_or_default();
                ai.complete(cfg, TIME_STORY_SYSTEM, prompt, 900).await
            },
            move |result| Message::TimeTravelStoryDone { generation, result },
        )
    }

    pub(crate) fn on_hover_dwell(
        &mut self,
        epoch: u64,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        if epoch != self.hover_gen || self.hover_pinned {
            return Task::none(); // cursor moved on, or is inside the tooltip
        }
        self.hover = Some(HoverState {
            line,
            col,
            x,
            y,
            text: None,
            // The Explain one-liner is cached, so attach it synchronously;
            // any LSP text arrives later and renders below it.
            summary: self.hover_summary(pane, line, col),
            // The diagnostic under the cursor (if the symbol is
            // underlined), so the hover explains the error.
            diagnostic: self.diagnostic_at(pane, line, col),
        });
        // Debug: while paused, hovering an identifier shows its live
        // value (evaluated in the current frame) instead of LSP info.
        if let Some(session) = self
            .debug
            .session
            .as_ref()
            .filter(|s| s.status == DebugStatus::Stopped)
            && let (Some(client), Some(frame)) = (session.client.clone(), session.frames.first())
        {
            let frame_id = frame.id;
            if let Some(word) = self
                .panes
                .get(pane)
                .and_then(Option::as_ref)
                .and_then(|v| analyze::word_at(&v.lines, line, col))
            {
                let w = word.clone();
                return Task::perform(
                    async move { client.evaluate(&word, frame_id).await },
                    move |res| Message::HoverResult {
                        line,
                        col,
                        text: res
                            .ok()
                            .filter(|v| !v.is_empty())
                            .map(|v| format!("{w} = {v}")),
                    },
                );
            }
        }
        // Local peek (tree-sitter only): the same-file symbol's doc
        // comment and/or the Rust type's structure. Instant, no LSP
        // round-trip, and works with no server configured at all.
        if let Some(text) = self.local_peek(pane, line, col) {
            if let Some(h) = &mut self.hover {
                h.text = Some(text);
            }
            return Task::none();
        }
        // Pull the request context before mutating self further.
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
            _ => return Task::none(),
        };
        let utf16 = client.encoding == lsp::client::PositionEncoding::Utf16;
        let character = viewer::character_offset(&source_line, col, utf16);
        Task::perform(
            async move { client.hover(&path, line, character).await },
            move |result| Message::HoverResult {
                line,
                col,
                text: result.ok().flatten(),
            },
        )
    }

    pub(crate) fn on_hover_requested(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        // The cursor is inside the tooltip — leave it be so it can be read
        // and scrolled.
        if self.hover_pinned {
            return Task::none();
        }
        // The code view reports the cursor in the scrollable's *content*
        // space (offset by the scroll); the tooltip overlay lives in
        // window space, so remove the pane's scroll to anchor it at the
        // cursor rather than that far below it.
        let y = y - self
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or(0.0, |v| v.scroll_y);
        // Same token already shown: just reposition.
        if let Some(h) = &mut self.hover
            && h.line == line
            && h.col == col
        {
            h.x = x;
            h.y = y;
            return Task::none();
        }
        // New token: start a dwell so moving across code doesn't flash
        // tooltips — it shows only if the cursor rests here for a moment.
        // The current peek stays visible until the new one is ready, so the
        // cursor can travel down into it without it vanishing first.
        self.hover_gen = self.hover_gen.wrapping_add(1);
        let epoch = self.hover_gen;
        Task::perform(
            async move { tokio::time::sleep(std::time::Duration::from_millis(300)).await },
            move |_| Message::HoverDwell {
                epoch,
                pane,
                line,
                col,
                x,
                y,
            },
        )
    }

    pub(crate) fn on_generate_overview(&mut self) -> Task<Message> {
        let Some(cfg) = llm::Config::load() else {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::none();
        };
        if self.explain.cache.is_empty() {
            self.status =
                "Run Explain All first — the overview is built from the explanations".into();
            return Task::none();
        }
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let inputs = self.gather_overview_inputs();
        let prompt = overview::prompt(&inputs);
        let prompt_hash = incremental::content_hash(prompt.as_bytes());
        self.overview.generating = true;
        // Don't force the overview into view: a chained/background
        // regeneration must not interrupt someone reading code. The
        // manual entry points are already on the overview page.
        self.status = "Generating architecture overview…".into();
        let ai = self.ai_client();
        Task::perform(
            // Raw LLM prose only; the module map is folded in fresh at
            // prepare time so it always reflects the live imports.
            async move { ai.complete(cfg, overview::SYSTEM, prompt, 2048).await },
            move |result| Message::OverviewDone {
                root: root.clone(),
                prompt_hash,
                result,
            },
        )
    }

    pub(crate) fn on_overview_done(
        &mut self,
        root: PathBuf,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    ) -> Task<Message> {
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.overview.generating = false;
        match result {
            Ok(markdown) => {
                // Persist the raw prose; fold the live module map in only
                // for display so the cache never carries a stale diagram.
                let _ = overview::save(
                    &root,
                    &overview::Cached {
                        markdown: markdown.clone(),
                        prompt_hash,
                    },
                );
                let display = self.overview_display(&markdown);
                let (prepared, task) = self.prepare_segments(&display);
                self.overview.prepared = prepared;
                self.overview.markdown = Some(markdown);
                self.overview.map = self.compute_overview_map();
                self.overview.prompt_hash = Some(prompt_hash);
                self.status = "Architecture overview ready".into();
                task
            }
            Err(e) => {
                self.status = format!("Overview failed: {e}");
                Task::none()
            }
        }
    }

    pub(crate) fn on_symbol_index_done(
        &mut self,
        root: PathBuf,
        indexed: index::Indexed,
    ) -> Task<Message> {
        // Ignore a late result from a project the user already switched
        // away from (it would seed the new project's registry with the
        // old project's files).
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.indexing = false;
        // Seed the change-detection registry from the same tree read.
        self.registry.seed(indexed.hashes);
        self.symbol_index_by_file = indexed.by_file;
        let changed_while_closed = indexed.changed.len();
        self.rebuild_symbol_index();
        // Build the import graph from the same single tree read.
        self.rebuild_import_graph(indexed.imports_by_file);
        if changed_while_closed > 0 {
            self.status = format!(
                "{changed_while_closed} file{} changed since last session",
                if changed_while_closed == 1 { "" } else { "s" }
            );
        } else if let Some(p) = &self.project {
            self.status = format!(
                "{} files · {} symbols",
                p.files.len(),
                self.symbol_index.len()
            );
        }
        // The import graph is now resolved, so refresh the overview's
        // module map if it was prepared before the imports were ready.
        let map_task = self.refresh_overview_map();
        // Build the Rust type-structure index off-thread (for the hover
        // "implements / implementors" peek).
        let structure_task = match self.project.as_ref().map(|p| p.files.clone()) {
            Some(files) => Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || structure::build(&files))
                        .await
                        .unwrap_or_default()
                },
                Message::StructureBuilt,
            ),
            None => Task::none(),
        };
        Task::batch([map_task, structure_task])
    }

    pub(crate) fn on_inlay_hints_loaded(
        &mut self,
        abs: PathBuf,
        hints: Vec<lsp::client::InlayHint>,
    ) -> Task<Message> {
        // Encoding for mapping the server's character offsets to display
        // columns (tabs already expanded to 4).
        let utf16 = self
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == abs)
            .and_then(|v| v.lang_key)
            .and_then(|l| match self.lsp.get(l) {
                Some(LspSlot::Ready(c)) => Some(c.encoding == lsp::client::PositionEncoding::Utf16),
                _ => None,
            })
            .unwrap_or(true);
        for slot in &mut self.panes {
            let Some(v) = slot.as_mut().filter(|v| v.abs == abs) else {
                continue;
            };
            let source = v.source.clone();
            let src_lines: Vec<&str> = source.lines().collect();
            let mut map: HashMap<usize, Vec<(usize, String)>> = HashMap::new();
            for h in &hints {
                let Some(line) = src_lines.get(h.line) else {
                    continue;
                };
                let col = viewer::display_col_from_char(line, h.character, utf16);
                let mut text = h.label.clone();
                if h.padding_left {
                    text.insert(0, ' ');
                }
                if h.padding_right {
                    text.push(' ');
                }
                map.entry(h.line).or_default().push((col, text));
            }
            for chips in map.values_mut() {
                chips.sort_by_key(|(c, _)| *c);
            }
            v.inlay_hints = map;
        }
        Task::none()
    }

    /// The user approved the language-server command this project's `lsp.toml`
    /// names. Record the approval against its fingerprint (so an edited command
    /// is asked about again) and start it.
    pub(crate) fn on_lsp_command_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.pending_lsp_command.take() else {
            return Task::none();
        };
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            self.trust.approve_lsp(&root, &c.language, &c.fingerprint);
            if let Err(e) = self.trust.save() {
                self.status = format!("Could not record the approval: {e}");
            }
        }
        self.start_lsp_with(&c.language, c.command)
    }

    pub(crate) fn on_lsp_consent_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.pending_lsp_consent.take() else {
            return Task::none();
        };
        self.lsp.insert(c.language.clone(), LspSlot::Starting);
        let (dest, language, version) = (c.dest_dir, c.language, c.version);
        match c.provision {
            LspProvision::Download(download) => {
                self.status = format!("Downloading {}…", c.server_name);
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            lsp::store::download_and_install(&download, &dest)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::LspDownloadResult {
                        language: language.clone(),
                        result,
                    },
                )
            }
            LspProvision::Install(install) => {
                self.status = format!("Installing {}…", c.server_name);
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            lsp::store::toolchain_install(&install, &version, &dest)
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    },
                    move |result| Message::LspDownloadResult {
                        language: language.clone(),
                        result,
                    },
                )
            }
        }
    }

    pub(crate) fn on_call_hierarchy_children(
        &mut self,
        id: usize,
        items: Vec<lsp::client::CallItem>,
    ) -> Task<Message> {
        // Keep only project-internal callers/callees — don't descend into
        // external libraries / std.
        let root = self.project.as_ref().map(|p| p.root.clone());
        let items: Vec<_> = match &root {
            Some(r) => items
                .into_iter()
                .filter(|i| i.path.starts_with(r))
                .collect(),
            None => items,
        };
        let new_ids = match &mut self.call_graph {
            Some(t) => t.set_children(id, items),
            None => return Task::none(),
        };
        // In "expand all" mode, recurse into the new project-internal
        // children until the frontier is empty or the node cap is hit.
        let recurse = self
            .call_graph
            .as_ref()
            .is_some_and(|t| t.full && t.node_count() < callgraph::MAX_NODES);
        if recurse {
            let to_fetch: Vec<usize> = new_ids
                .into_iter()
                .filter(|&cid| self.call_graph.as_ref().is_some_and(|t| t.needs_fetch(cid)))
                .collect();
            Task::batch(
                to_fetch
                    .into_iter()
                    .map(|cid| self.fetch_children(cid))
                    .collect::<Vec<_>>(),
            )
        } else {
            Task::none()
        }
    }

    pub(crate) fn on_dap_stop_inspected(
        &mut self,
        frames: Vec<dap::StackFrame>,
        scopes: Vec<DebugScope>,
    ) -> Task<Message> {
        // Jump to the innermost frame that has source, and highlight it.
        let (target, fname) = {
            let Some(session) = self.debug.session.as_mut() else {
                return Task::none();
            };
            session.frames = frames;
            session.scopes = scopes;
            let t = session
                .frames
                .iter()
                .find_map(|f| f.path.clone().map(|p| (p, f.line)));
            if let Some((path, line)) = &t {
                session.current = Some((path.clone(), *line));
            }
            let fname = session.frames.first().map(|f| short_frame_name(&f.name));
            (t, fname)
        };
        // Fuse into the reading trail: when execution enters a NEW
        // function, record one entry (labelled with the function name) so
        // the debug run becomes a navigable path in the TRAIL tab.
        if let (Some(fname), Some((path, line))) = (&fname, &target)
            && self.debug.last_fn.as_ref() != Some(fname)
        {
            self.debug.last_fn = Some(fname.clone());
            self.history.push(
                Loc {
                    path: path.clone(),
                    line: Some(*line),
                },
                Some(fname.clone()),
            );
            self.save_history();
        }
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Debug;
        match target {
            Some((path, line)) => {
                Task::batch([self.open_file(path, Some(line), false), self.eval_watches()])
            }
            None => self.eval_watches(),
        }
    }

    pub(crate) fn on_conditional_breakpoint_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let Some(abs) = self
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .map(|v| v.abs.clone())
        else {
            return Task::none();
        };
        let line = menu.line + 1;
        // Pre-fill with any existing condition on this line.
        let existing = self
            .debug
            .breakpoints
            .get(&abs)
            .and_then(|m| m.get(&line))
            .and_then(|bp| bp.condition.clone())
            .unwrap_or_default();
        self.debug.bp_cond_edit = Some((abs, line, existing));
        operation::focus(ui::bp_condition_input_id())
    }

    pub(crate) fn on_server_connected(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
    ) -> Task<Message> {
        // The in-process clew-server is up; keep its request channel and
        // greet it. Backend flows migrate onto this seam one at a time.
        let hello = clew_protocol::ClientMessage {
            id: 0,
            request: clew_protocol::Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                ai: clew_protocol::AiEndpoint::Server,
            },
        };
        let _ = tx.send(hello);
        self.server_tx = Some(tx);
        // Resume a scan that was waiting for the server (its Tree reply
        // opens the project); otherwise, if a project is already open
        // (local-fallback path), tell the server about it for search.
        if let Some(root) = self.pending_scan_root.clone() {
            self.request_open_project(root);
        } else {
            self.sync_project_to_server();
        }
        // Give the server the AI config so server-endpoint calls work.
        self.send_ai_config();
        // If the Connect modal was waiting on this transport, move it into
        // the remote folder picker and list the home directory.
        if let Some(ui) = &self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            self.enter_remote_browser(None);
        }
        Task::none()
    }

    pub(crate) fn on_server_unavailable(&mut self) -> Task<Message> {
        // A remote bootstrap failure surfaces in the Connect modal rather
        // than falling back to a (meaningless) local scan of a remote path.
        if let Some(ui) = &mut self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            ui.stage = ConnectStage::Error(
                "Could not reach the host. Check the address, port, and key.".into(),
            );
            self.pending_scan_root = None;
            return Task::none();
        }
        // The server binary didn't spawn. Fall back to a local scan for
        // any project that was deferred waiting on it.
        if let Some(root) = self.pending_scan_root.take() {
            return self.local_scan(root);
        }
        Task::none()
    }

    pub(crate) fn on_connect_submit(&mut self) -> Task<Message> {
        let Some(ui) = &self.connect else {
            return Task::none();
        };
        let host = ui.host.trim().to_string();
        let user = ui.user.trim().to_string();
        if host.is_empty() || user.is_empty() {
            if let Some(ui) = &mut self.connect {
                ui.stage = ConnectStage::Error("Host and user are required.".into());
            }
            return Task::none();
        }
        let conn = connect::SavedConnection {
            name: ui.name.trim().to_string(),
            host,
            user,
            port: ui.port.parse().unwrap_or(22),
            identity: ui.identity.trim().to_string(),
        };
        self.remember_connection(conn.clone());
        self.connect_to(conn.target());
        Task::none()
    }

    pub(crate) fn on_target_selected(&mut self, target: inactive::Target) -> Task<Message> {
        self.reading_target = target;
        self.show_tools_menu = false;
        self.show_target_menu = false;
        // Re-evaluate the cfg dimming for every open file.
        let t = self.reading_target.clone();
        for v in self.panes.iter_mut().flatten() {
            if let Some(lang) = v.lang_key {
                let src = v.source.clone();
                v.inactive_lines = inactive::inactive_lines(&src, lang, &t);
            }
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = reading::save_target(&root, &self.reading_target)
        {
            self.status = format!("Could not save target: {e}");
        }
        Task::none()
    }

    pub(crate) fn on_project_calls_built(
        &mut self,
        root: PathBuf,
        graph: projectcalls::ProjectCallGraph,
    ) -> Task<Message> {
        // Drop a late result from a previous project.
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.project_calls.building = false;
        self.project_calls.graph = graph;
        // This is the name-based approximation; a superseding refine is
        // no longer valid, and no precise result is in effect.
        self.project_calls.precise = false;
        self.project_calls.refine_progress = None;
        self.project_calls.generation += 1;
        self.project_calls.precise_edges = projectcalls::SymEdges::default();
        self.project_calls.precise_pending.clear();
        // The map depends on the freshly built graph.
        if self.overlay == Some(Overlay::ProjectCalls) {
            self.refresh_graph_layout();
        }
        // If files changed while this build was running, its data is
        // already stale — rebuild once so the open overlay self-heals.
        if self.overlay == Some(Overlay::ProjectCalls)
            && self.project_calls.rev != self.registry.revision()
        {
            return self.build_project_calls();
        }
        Task::none()
    }

    pub(crate) fn on_project_calls_refined(
        &mut self,
        root: PathBuf,
        generation: u64,
        edges: projectcalls::SymEdges,
        graph: projectcalls::ProjectCallGraph,
    ) -> Task<Message> {
        // Accept only the latest refine for the current project.
        if generation != self.project_calls.generation
            || self.project.as_ref().map(|p| &p.root) != Some(&root)
        {
            return Task::none();
        }
        self.project_calls.graph = graph;
        self.project_calls.precise_edges = edges;
        self.project_calls.precise = true;
        self.project_calls.refine_progress = None;
        self.status = "Call graph refined with LSP".into();
        if self.overlay == Some(Overlay::ProjectCalls) {
            self.refresh_graph_layout();
        }
        // Files changed while this refine ran → fold them in now.
        if self.overlay == Some(Overlay::ProjectCalls)
            && !self.project_calls.precise_pending.is_empty()
        {
            let changed = std::mem::take(&mut self.project_calls.precise_pending);
            return self.refine_incremental(changed);
        }
        Task::none()
    }
}
