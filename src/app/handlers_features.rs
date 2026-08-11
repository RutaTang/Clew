//! Message handlers for the major features: explain, walkthrough, files-watch, semantic/ask, time-travel, hover, overview, LSP/index/DAP, server/connect, project-calls.

use crate::app::prelude::*;
use crate::*;

/// The request id the handshake's `Hello` carries. Reserved rather than the
/// plain `0` every fire-and-forget request uses, because a REFUSED handshake
/// is answered with a bare `Error` and the client has to tell that refusal
/// apart from an error about some proc kill or agent stop that happens to be
/// in flight — mistaking one for the other either strands the deferred scan
/// or tears down a healthy transport. Correlated requests are minted from
/// `next_req_id`, which counts up from 1, so no reply can ever collide.
pub(crate) const HELLO_REQ_ID: u64 = u64::MAX;

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
        // For a remote project the sources come over the protocol — the abs
        // paths above are identities only, never read from this disk.
        let remote_rels: Option<Vec<String>> = (!self.local_project_state())
            .then(|| project.files.iter().map(|f| f.rel.clone()).collect());
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
            let fetch_ai = ai.clone();
            async move {
                let inputs = match remote_rels {
                    // Remote: fetch the sources in bounded batches, then run
                    // the same (pure) gather over them.
                    Some(rels) => {
                        let mut contents: HashMap<PathBuf, (String, &'static str)> = HashMap::new();
                        // Chunked well under the server's per-reply budget:
                        // 400 files at the per-file cap would be ~200 MB in
                        // one frame. Progress is per chunk, so a project that
                        // does hit the budget still explains the rest.
                        for chunk in rels.chunks(128) {
                            // A failed batch leaves its files absent from
                            // the pass, like unreadable local ones.
                            if let Ok(clew_protocol::Event::Sources { files, .. }) = fetch_ai
                                .request(clew_protocol::Request::ReadSources {
                                    rels: chunk.to_vec(),
                                })
                                .await
                            {
                                for (rel, text) in files {
                                    let abs = gather_root.join(&rel);
                                    if let Some(lang) = highlight::detect(&abs) {
                                        contents.insert(abs, (text, lang));
                                    }
                                }
                            }
                        }
                        let g = gather_root.clone();
                        tokio::task::spawn_blocking(move || gather_explain_inputs_from(contents, g))
                            .await
                            .unwrap_or_default()
                    }
                    None => {
                        let g = gather_root.clone();
                        tokio::task::spawn_blocking(move || gather_explain_inputs(files, g))
                            .await
                            .unwrap_or_default()
                    }
                };
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
        // Merged into what is on disk, never written over it. This cache is
        // this window's copy from project open, and the derived store is
        // shared by every window and every clew process on the project — so
        // cancelling a pass here used to replace fifty summaries another
        // window had just paid for with this window's (possibly empty) copy.
        // Insert-only: a node this window explained is added, a node only the
        // other window has is left alone.
        if let (Some(store), Some(root)) = (
            self.derived_dir.clone(),
            self.project.as_ref().map(|p| p.root.clone()),
        ) {
            let mine = self.explain.cache.clone();
            let (merged, _) = explain::edit(&store, &root, |disk| disk.extend(mine));
            self.explain.cache = merged;
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
        if let Some(store) = self.derived_dir.clone() {
            // A pass that RAN TO COMPLETION may legitimately PRUNE — nodes
            // whose file was deleted must not linger — so it is not
            // insert-only: it walked every node of the project, so what it
            // holds is a superset of anything another window explained, and
            // what it does not hold is gone from the project. Merging rather
            // than replacing is what stops a second window's stale cache from
            // deleting summaries this pass just paid for, and vice versa.
            //
            // An ABORTED pass holds no such superset. `auth_error` breaks out
            // of the level loop, and the cache the pass builds starts EMPTY —
            // a rejected key fails every call, so it can arrive with nothing
            // in it at all. Pruning to that would delete a whole project's
            // stored explanations because a key expired, which is thousands of
            // billed calls destroyed by a recoverable error. Insert-only
            // there, which also restores this window's view of the summaries
            // it already had.
            //
            // NOT covered either way: a node whose individual call failed is
            // absent from `mine`, so a completed pass drops its stored
            // summary. That node is only re-explained when its prompt changed
            // (an unchanged one is reused into `mine`), so what is dropped is
            // a summary of text that no longer exists — stale, not lost.
            let completed = auth_error.is_none();
            let mine = self.explain.cache.clone();
            let (merged, _) = explain::edit(&store, &root, |disk| {
                if completed {
                    disk.retain(|node, _| mine.contains_key(node));
                }
                disk.extend(mine);
            });
            self.explain.cache = merged;
        }
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
        let epoch = self.project_epoch;
        let explain::Node::Function {
            file,
            name,
            ordinal,
        } = node.clone()
        else {
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
        // Remote: the function's source comes over the protocol, never from
        // a same-pathed local file.
        let remote_rel = (!self.local_project_state())
            .then(|| self.project.as_ref().map(|p| p.root.clone()))
            .flatten()
            .and_then(|root| {
                file.strip_prefix(&root)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            });
        // The local gather reads this disk, so it needs the project root to
        // confine that read to.
        let root = self.project.as_ref().map(|p| p.root.clone());
        Task::perform(
            async move {
                let prompt = match remote_rel {
                    Some(rel) => match ai
                        .request(clew_protocol::Request::ReadSources { rels: vec![rel] })
                        .await
                    {
                        Ok(clew_protocol::Event::Sources { files, .. }) if !files.is_empty() => {
                            let (_, content) = &files[0];
                            match gather_fn_detail_from(&file, content, &name, ordinal, &summaries)
                            {
                                Some((sig, body, callees)) => {
                                    Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                                }
                                None => Err("function body not found".to_string()),
                            }
                        }
                        _ => Err("could not read the remote source".to_string()),
                    },
                    None => tokio::task::spawn_blocking(move || {
                        let Some(root) = root else {
                            return Err::<String, String>("no project open".to_string());
                        };
                        let Some((sig, body, callees)) =
                            gather_fn_detail_input(&root, file, &name, ordinal, &summaries)
                        else {
                            return Err::<String, String>("function body not found".to_string());
                        };
                        Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                    })
                    .await
                    .unwrap_or_else(|_| Err("task join failed".into())),
                };
                match prompt {
                    Ok(p) => ai.complete(cfg, EXPLAIN_BLOCKS_SYSTEM, p, 1024).await,
                    Err(e) => Err(e),
                }
            },
            move |detail| Message::BlocksExplained {
                epoch,
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
                    // Insert-only merge into what is on disk: writing this
                    // window's whole cache back to store ONE block walkthrough
                    // dropped every summary another window had added since
                    // this one loaded the file.
                    if let (Some(store), Some(root)) = (
                        self.derived_dir.clone(),
                        self.project.as_ref().map(|p| p.root.clone()),
                    ) {
                        let mine = self.explain.cache.clone();
                        let (merged, _) = explain::edit(&store, &root, |disk| disk.extend(mine));
                        self.explain.cache = merged;
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
        let root = self.project.as_ref().unwrap().root.clone();
        let epoch = self.project_epoch;
        Task::perform(
            async move {
                let resp = ai.complete(cfg, walkthrough::SYSTEM, prompt, 4096).await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone {
                root: root.clone(),
                epoch,
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
        let remote = !self.local_project_state();
        // Local: the base resolves synchronously (and "nothing to review" is
        // reported right away). Remote: everything resolves inside the task.
        let local_base = if remote {
            None
        } else {
            match git::review_base(&root) {
                Some(base) => Some(base),
                None => {
                    self.status =
                        "Nothing to review (need a branch vs main/master, or a prior commit)"
                            .into();
                    return Task::none();
                }
            }
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        // Symbols per file for the changed-file annotations (in memory —
        // fed by the server's snapshot on a remote project).
        let symbols_by_file = self.symbol_index_by_file.clone();
        // A sentinel scope so the library shows it as a change review and
        // Regenerate re-runs the diff (not a normal scoped tour). The remote
        // label is refined once the base is known; the scope key is stable.
        let scope = match &local_base {
            Some((_, label)) => format!("@diff {label}"),
            None => "@diff changes".to_string(),
        };
        self.walk.generating = Some(scope.clone());
        self.status = "Reviewing changes…".into();
        let ai = self.ai_client();
        let task_root = root.clone();
        let epoch = self.project_epoch;
        Task::perform(
            async move {
                let root = task_root;
                // Resolve the base + collect the change: intent, changed
                // files + their symbols, patch — locally, or over the
                // protocol for a remote repository.
                let (base, label) = match local_base {
                    Some(pair) => pair,
                    None => match ai
                        .git::<Option<(String, String)>>(clew_protocol::GitOp::ReviewBase)
                        .await
                    {
                        Some(pair) => pair,
                        None => {
                            return Err(
                                "Nothing to review (need a branch vs main/master, or a prior \
                                 commit)"
                                    .to_string(),
                            );
                        }
                    },
                };
                let (commits, changed, patch) = if remote {
                    (
                        ai.git::<Vec<String>>(clew_protocol::GitOp::CommitSubjects {
                            base: base.clone(),
                        })
                        .await,
                        ai.git::<Vec<(String, char)>>(clew_protocol::GitOp::ChangedFiles {
                            base: base.clone(),
                        })
                        .await,
                        ai.git::<String>(clew_protocol::GitOp::RangePatch {
                            base: base.clone(),
                            max_bytes: 12000,
                        })
                        .await,
                    )
                } else {
                    let (r, b) = (root.clone(), base.clone());
                    tokio::task::spawn_blocking(move || {
                        (
                            git::commit_subjects(&r, &b),
                            git::changed_files(&r, &b),
                            git::range_patch(&r, &b, 12000),
                        )
                    })
                    .await
                    .unwrap_or_default()
                };
                let mut changed_text = String::new();
                for (rel, ch) in changed {
                    changed_text.push_str(&format!("{ch} {rel}\n"));
                    if let Some(syms) = symbols_by_file.get(&root.join(&rel)) {
                        for s in syms.iter().filter(|s| {
                            matches!(
                                s.kind.as_str(),
                                "function" | "method" | "struct" | "class" | "enum" | "trait"
                            )
                        }) {
                            changed_text
                                .push_str(&format!("    {} {} @ L{}\n", s.kind, s.name, s.line));
                        }
                    }
                }
                let prompt = walkthrough::diff_prompt(
                    &project_name,
                    &label,
                    &commits,
                    &changed_text,
                    &patch,
                );
                let resp = ai
                    .complete(cfg, walkthrough::DIFF_SYSTEM, prompt, 4096)
                    .await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| Message::WalkthroughDone {
                root: root.clone(),
                epoch,
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
                let stored = wt.clone();
                match self.walk.library.iter().position(|w| w.scope == scope) {
                    Some(i) => self.walk.library[i] = wt,
                    None => self.walk.library.push(wt),
                }
                if self.local_project_state()
                    && let Some(root) = self.project.as_ref().map(|p| p.root.clone())
                {
                    // Upserted into what is on disk RIGHT NOW, by scope: writing
                    // this window's whole library back erased every tour a
                    // second window on the same project had generated since it
                    // loaded (the same merge `on_walkthrough_delete` makes). The
                    // tour is already in this window's library, so a failed write
                    // still leaves it on screen — only unsaved.
                    let (merged, saved) = walkthrough::edit_library(&root, |lib| {
                        match lib.iter().position(|w| w.scope == scope) {
                            Some(i) => lib[i] = stored,
                            None => lib.push(stored),
                        }
                    });
                    // The merged library carries the new tour whether or not
                    // the write landed, so adopting it here is what keeps the
                    // generated tour on screen — the promise the comment above
                    // makes. Dropping it would throw away a tour that cost a
                    // full LLM pass.
                    self.walk.library = merged;
                    if let Err(e) = saved {
                        self.status =
                            format!("Could not save walkthrough: {e} — shown but not saved");
                    }
                } else {
                    // Remotely the SAME upsert-by-scope, applied to the file
                    // by the server: shipping this window's whole library
                    // erased every tour another client had generated since.
                    self.save_walkthrough_scope(&scope, Some(&stored));
                }
                // Resolved by scope rather than by the index the upsert used:
                // the merge re-reads a library another window may have appended
                // to, which moves every index after the insertion point.
                self.walk.open = self.walk.library.iter().position(|w| w.scope == scope);
                self.walk.step = 0;
                self.sidebar = SidebarTab::Walk;
                self.show_left_sidebar = true;
                self.walk.retried = false;
                // No second save here. The branch above already persisted, and
                // locally it did so by merging under the save lock; following
                // that with a second `save_walkthrough_scope()` writes the same
                // bytes again but outside the lock, reopening the unlocked
                // whole-file window `edit_library` exists to close. Nothing
                // between there and here touches the library — `open`, `step`
                // and `retried` are per-window view state.
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
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
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
        self.registry.begin_read(baselines.keys().cloned());
        let scan_root = root.clone();
        let epoch = self.project_epoch;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let events =
                        watch::rehash(&scan_root, candidates, viewer::MAX_FILE_BYTES as u64);
                    let fs_structural = watch::structural_changes(&probes);
                    (events, fs_structural)
                })
                .await
                .unwrap_or_default()
            },
            move |(events, fs_structural)| Message::FilesRehashed {
                root: root.clone(),
                epoch,
                events,
                baselines: baselines.clone(),
                fs_structural,
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
        self.registry
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
        let mut graph_dirty = false;
        let mut refreshed = 0usize;
        let mut touched: Vec<PathBuf> = Vec::new();
        // Open notebooks whose bytes moved: they cannot be reloaded from the
        // raw .ipynb, only re-read and re-parsed (see the pane loop below).
        let mut nb_refresh: Vec<PathBuf> = Vec::new();
        // One resolver for the whole batch, over the current file set. A
        // structural change re-resolves the whole graph later (once the
        // rescan lands the new file set); here we only refresh out-edges.
        let resolver = self.import_resolver();
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
            let current = self.registry.version(path).unwrap_or(0);
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
                        // `content_tasks` re-runs `git::info` over the NEW
                        // bytes. Retire any server blame still in flight for
                        // this file: it was requested against the pre-change
                        // bytes, the two passes are not ordered against each
                        // other, and the loser is simply the one applied last —
                        // so an unretired reply repaints the gutter bars and
                        // the caret-line blame from the previous revision, at
                        // line indices that no longer describe the text.
                        self.pending_git.retain(|_, p| p != &c.path);
                        tasks.push(self.content_tasks(c.path.clone(), c.content.clone(), lang_key));
                    }
                    // Resync the language server's copy whether or not a pane
                    // still shows the file (see `resync_open_doc` for why).
                    self.resync_open_doc(&c.path, &c.content);
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
        // pass (see `request_auto_refresh`).
        if touched.iter().any(|p| highlight::detect(p).is_some()) {
            tasks.push(self.request_auto_refresh());
        }
        Task::batch(tasks)
    }

    /// Discard the in-memory index when `cfg` names an embedding space its
    /// vectors cannot belong to, so a query is never ranked in one space
    /// against vectors from another and a rebuild never reuses them.
    ///
    /// Called on every path that USES the index, because the config it was
    /// loaded under can move under a running session: `embed::load_for` applies
    /// the same rule to the file but only at project open, and
    /// `on_settings_saved` covers this window's own Settings. What is left for
    /// here is a change made by ANOTHER window or by hand-editing `config.toml`
    /// — and only the part of it an in-memory index can show, which is the
    /// model (see [`embed::Space::is_foreign`] for the endpoint-only case this
    /// cannot see).
    fn drop_foreign_embed_index(&mut self, cfg: &embed::Config) {
        if cfg.space().is_foreign(&self.embed_index) {
            self.embed_index = embed::Index::default();
            self.semantic_results.clear();
        }
    }

    pub(crate) fn on_build_embeddings(&mut self) -> Task<Message> {
        let Some(cfg) = embed::Config::load() else {
            self.status = "Configure an embedding endpoint in Settings".into();
            return Task::none();
        };
        self.drop_foreign_embed_index(&cfg);
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
        let epoch = self.project_epoch;
        // The space these vectors are being built in. `embed::save` stamps the
        // file with the endpoint from the config that is live WHEN IT WRITES,
        // so a config changed while the build ran would publish old-space
        // vectors under the new space's name — and `load_for` would then trust
        // that file for good. Discarding the build is the cheap half of that
        // trade: only this run's embedding calls are lost.
        let space = cfg.space();
        Task::perform(
            async move {
                let index = build_embeddings(&ai, &cfg, nodes, existing).await?;
                if embed::stored_space() != space {
                    return Err(
                        "embedding config changed while the index was building — discarded, build it again"
                            .to_string(),
                    );
                }
                Ok(index)
            },
            move |result| Message::EmbeddingsBuilt {
                root: root.clone(),
                epoch,
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
        self.drop_foreign_embed_index(&cfg);
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
        // (no server channel, no AI-on-server grant — the agent's LLM calls
        // run on the server, which then must hold the keys — or the server
        // can't run an agent turn).
        if self.server_tx.is_some() && self.ai_on_server() && self.agent_stream.is_none() {
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
        // Retrieval embeds the question at the live endpoint, so the same
        // space check FIND makes applies here — an index from another space
        // would rank nonsense into the context and the answer would cite it.
        if let Some(ecfg) = &ecfg {
            self.drop_foreign_embed_index(ecfg);
        }
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
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        self.ask_input.clear();
        self.show_bottom = true;
        self.bottom_tab = BottomTab::Ask;
        self.asking = true;
        let epoch = self.project_epoch;
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
                        root: root.clone(),
                        epoch,
                        question: question.clone(),
                        qvec,
                    },
                )
            }
            // No index: skip retrieval, answer from the live grounding.
            None => Task::done(Message::AskRetrieved {
                root,
                epoch,
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

    pub(crate) fn on_ask_stream_ended(
        &mut self,
        stream: u64,
        error: Option<String>,
    ) -> Task<Message> {
        // A superseded stream must not touch this project's UI at all — not
        // the spinner, not the status line, not another turn's text.
        let Some(idx) = self
            .ask_turns
            .iter()
            .position(|t| t.stream == stream && t.streaming)
        else {
            return Task::none();
        };
        self.asking = false;
        if let Some(e) = &error {
            self.status = format!("Ask failed: {e}");
        }
        // Finalize the turn: on error with no text, show why; then render the
        // accumulated markdown as rich segments.
        let md = {
            let turn = &mut self.ask_turns[idx];
            turn.streaming = false;
            if let Some(e) = &error
                && turn.answer_md.trim().is_empty()
            {
                turn.answer_md = format!("*Couldn't answer: {e}*");
            }
            turn.answer_md.clone()
        };
        let (prepared, task) = self.prepare_segments(&md);
        self.ask_turns[idx].answer = prepared;
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
    pub(crate) fn on_agent_stepped(&mut self, stream: u64, step: AgentStep) -> Task<Message> {
        let Some(turn) = self
            .ask_turns
            .iter_mut()
            .find(|t| t.stream == stream && t.streaming)
        else {
            return Task::none();
        };
        turn.steps.push(step);
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
    pub(crate) fn on_agent_turn_ended(
        &mut self,
        stream: u64,
        error: Option<String>,
    ) -> Task<Message> {
        // Only the turn that is actually open may release the Stop button's
        // id: a late Done from a superseded turn used to unblock the
        // one-agent-at-a-time gate (`on_ask_submit`) for a turn still running.
        if self.agent_stream == Some(stream) {
            self.agent_stream = None;
        }
        let Some(idx) = self
            .ask_turns
            .iter()
            .position(|t| t.stream == stream && t.streaming)
        else {
            return Task::none();
        };
        let bare_failure = error.as_deref().is_some_and(|e| e != "stopped")
            && self.ask_turns[idx].steps.is_empty()
            && self.ask_turns[idx].answer_md.trim().is_empty();
        if bare_failure {
            let turn = self.ask_turns.remove(idx);
            self.status = format!(
                "Agent mode unavailable ({}) — answering from the semantic index",
                error.unwrap_or_default()
            );
            return self.on_ask_submit_rag(turn.question);
        }
        self.on_ask_stream_ended(stream, error)
    }

    /// Stop the in-flight answer where it actually runs. An agent turn closes
    /// with `AgentDone`; a plain streamed chat is cancelled by its stream id.
    pub(crate) fn on_agent_stop(&mut self) -> Task<Message> {
        let Some(tx) = &self.server_tx else {
            return Task::none();
        };
        if let Some(stream) = self.agent_stream {
            let _ = tx.send(clew_protocol::ClientMessage {
                id: 0,
                request: clew_protocol::Request::AgentStop { stream },
            });
        }
        if let Some(sub) = self.chat_stream {
            let _ = tx.send(clew_protocol::ClientMessage {
                id: 0,
                request: clew_protocol::Request::Cancel { sub },
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
        self.blame_why_seq += 1;
        let token = self.blame_why_seq;
        self.blame_why = Some(BlameWhy {
            token,
            title: title.clone(),
            commits: commits.clone(),
            loading: true,
            prepared: Vec::new(),
        });
        // The answer is only meaningful for the project it was asked in: this
        // pairs with `owns_result` on arrival, so a reply that outlives a
        // project switch cannot land in the new project's popup.
        let (Some(ask_root), ask_epoch) = (
            self.project.as_ref().map(|p| p.root.clone()),
            self.project_epoch,
        ) else {
            return Task::none();
        };
        self.status = "Explaining why…".into();
        let commits_ctx = commits.clone();
        let ai = self.ai_client();
        let remote = !self.local_project_state();
        Task::perform(
            async move {
                // Build the prompt (git diffs — over the protocol for a
                // remote repository), then complete.
                let mut ctx = format!(
                    "Code ({rel}, lines {}-{}):\n```\n{code}\n```\n\n",
                    l0 + 1,
                    last + 1
                );
                for (sha, _) in &commits_ctx {
                    let (msg, diff) = if remote {
                        (
                            ai.git::<Option<String>>(clew_protocol::GitOp::CommitMessage {
                                sha: sha.clone(),
                            })
                            .await
                            .unwrap_or_default(),
                            ai.git::<String>(clew_protocol::GitOp::CommitFileDiff {
                                sha: sha.clone(),
                                rel: rel.clone(),
                                max_bytes: 3000,
                            })
                            .await,
                        )
                    } else {
                        let (root, sha, rel) = (root.clone(), sha.clone(), rel.clone());
                        tokio::task::spawn_blocking(move || {
                            (
                                git::commit_message(&root, &sha).unwrap_or_default(),
                                git::commit_file_diff(&root, &sha, &rel, 3000),
                            )
                        })
                        .await
                        .unwrap_or_default()
                    };
                    ctx.push_str(&format!(
                        "### Commit {sha}\nMessage:\n{msg}\n\nWhat it changed here:\n```\n{diff}\n```\n\n"
                    ));
                }
                let prompt = format!("Why does this code exist?\n\n{ctx}");
                ai.complete(cfg, WHY_SYSTEM, prompt, 512).await
            },
            move |result| Message::BlameWhyDone {
                token,
                root: ask_root.clone(),
                epoch: ask_epoch,
                title: title.clone(),
                commits: commits.clone(),
                result,
            },
        )
    }

    /// Resolve the scope a Time Travel run should use. `symbol` = the reader
    /// asked for "this symbol" (the toolbar entry asks for the whole file, the
    /// bar's toggle flips between the two).
    ///
    /// Returns the scope and whether a symbol scope was REFUSED — as opposed to
    /// merely not found, which also falls back to `TimeScope::File` but is not
    /// worth a status line.
    pub(crate) fn time_travel_scope(&self, symbol: bool) -> (TimeScope, bool) {
        let Some(v) = self.active_viewer() else {
            return (TimeScope::File, false);
        };
        // A notebook pane's symbols are outline entries in the `# %%` SCRIPT
        // PROJECTION, but `git log -L` resolves its range against the raw
        // .ipynb blob at HEAD — where those numbers name `outputs` entries,
        // base64 fragments or metadata, and git happily returns an unrelated
        // commit list labelled with the cell's name. Refused rather than
        // mapped: the client holds only the parsed projection of the WORKING
        // copy, so recovering a cell's JSON line span as of HEAD would take the
        // raw bytes of a different revision (and a protocol addition for remote
        // projects), and even a perfect span would scope the history to output
        // and execution-count churn. Whole-file history is the honest answer
        // here, and a wrong range that looks like an answer is worse than not
        // offering one.
        let refused = symbol && v.notebook.is_some();
        if !symbol || refused {
            return (TimeScope::File, refused);
        }
        // Scope: the innermost code block (any kind — function, struct,
        // enum, class, trait, …) whose span contains the caret, else the
        // whole file. When re-scoping mid-session the caret comes from the
        // historical view; either way the block's NAME is resolved to its
        // HEAD line range, since `git log -L` interprets ranges vs HEAD.
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
        let scope = name
            .and_then(|n| {
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
            .unwrap_or(TimeScope::File);
        (scope, false)
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
        let (scope, symbol_refused) = self.time_travel_scope(symbol);
        self.time_gen += 1;
        let generation = self.time_gen;
        // Say why the scope did not change, so the toggle is not silently
        // inert on a notebook. Transient: `TimeTravelReady` clears the status
        // when the history lands, exactly as it does over "Loading history…".
        self.status = if symbol_refused {
            "Cell history isn't available for notebooks — showing the whole file".into()
        } else {
            "Loading history…".to_string()
        };
        let (scope_task, rel_task) = (scope.clone(), rel.clone());
        // Remote: the repository lives on the remote host — the history
        // comes over the protocol instead of a local `git` run.
        let remote_ai = (!self.local_project_state()).then(|| self.ai_client());
        Task::perform(
            async move {
                if let Some(ai) = remote_ai {
                    let op = match &scope_task {
                        TimeScope::File => clew_protocol::GitOp::FileHistory {
                            rel: rel_task.clone(),
                            limit: 200,
                        },
                        TimeScope::Symbol { start, end, .. } => {
                            clew_protocol::GitOp::SymbolHistory {
                                rel: rel_task.clone(),
                                start: *start,
                                end: *end,
                                limit: 200,
                            }
                        }
                    };
                    return ai.git::<Vec<git::HistCommit>>(op).await;
                }
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
        let remote_ai = (!self.local_project_state()).then(|| self.ai_client());
        Task::perform(
            async move {
                // Remote: content + added-lines over the protocol; the
                // highlight/outline work stays local (pure).
                let fetched = match remote_ai {
                    Some(ai) => {
                        let content = ai
                            .git::<Option<String>>(clew_protocol::GitOp::FileAt {
                                sha: commit.sha.clone(),
                                rel: commit.path.clone(),
                            })
                            .await
                            .unwrap_or_default();
                        let added = ai
                            .git::<HashSet<usize>>(clew_protocol::GitOp::AddedLines {
                                sha: commit.sha.clone(),
                                rel: commit.path.clone(),
                            })
                            .await;
                        Some((content, added))
                    }
                    None => None,
                };
                tokio::task::spawn_blocking(move || {
                    let (content, added) = match fetched {
                        Some(pair) => pair,
                        None => (
                            git::file_at(&root, &commit.sha, &commit.path).unwrap_or_default(),
                            git::commit_added_lines(&root, &commit.sha, &commit.path),
                        ),
                    };
                    let lines = highlight::highlight_lines(&content, lang);
                    let symbols = lang
                        .map(|l| outline::extract(&content, l))
                        .unwrap_or_default();
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

    // The step arrives boxed in its Message (keeping the enum small); the
    // handler takes it as-is.
    #[allow(clippy::boxed_local)]
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
        let remote = !self.local_project_state();
        Task::perform(
            async move {
                let prompt = if remote {
                    let msg = ai
                        .git::<Option<String>>(clew_protocol::GitOp::CommitMessage {
                            sha: sha2.clone(),
                        })
                        .await
                        .unwrap_or(subject);
                    let diff = ai
                        .git::<String>(clew_protocol::GitOp::CommitFileDiff {
                            sha: sha2.clone(),
                            rel: path.clone(),
                            max_bytes: 8000,
                        })
                        .await;
                    format!("Commit message:\n{msg}\n\nDiff of {path}:\n{diff}")
                } else {
                    tokio::task::spawn_blocking(move || {
                        let msg = git::commit_message(&root, &sha2).unwrap_or(subject);
                        let diff = git::commit_file_diff(&root, &sha2, &path, 8000);
                        format!("Commit message:\n{msg}\n\nDiff of {path}:\n{diff}")
                    })
                    .await
                    .unwrap_or_default()
                };
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
        let remote = !self.local_project_state();
        Task::perform(
            async move {
                let prompt = if remote {
                    let mut ctx = String::new();
                    for (sha, subject, path) in &commits {
                        let short = &sha[..sha.len().min(8)];
                        let diff = ai
                            .git::<String>(clew_protocol::GitOp::CommitFileDiff {
                                sha: sha.clone(),
                                rel: path.clone(),
                                max_bytes: 2500,
                            })
                            .await;
                        ctx.push_str(&format!(
                            "### {short} — {subject}\n```diff\n{diff}\n```\n\n"
                        ));
                    }
                    format!("Code block: {name}\n\nCommits (newest first):\n{ctx}")
                } else {
                    tokio::task::spawn_blocking(move || {
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
                    .unwrap_or_default()
                };
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
                        epoch,
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
        // It also SUPPRESSES the language server for this token, which is why
        // the structure index has to track disk (see `request_structure_build`)
        // rather than being built once per project open: a name only the stale
        // index still knew — a type deleted during the session — answered here
        // with its old relations instead of falling through to rust-analyzer,
        // which would have reported the identifier as unresolved.
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
                epoch,
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
        let epoch = self.project_epoch;
        Task::perform(
            // Raw LLM prose only; the module map is folded in fresh at
            // prepare time so it always reflects the live imports.
            async move { ai.complete(cfg, overview::SYSTEM, prompt, 2048).await },
            move |result| Message::OverviewDone {
                root: root.clone(),
                epoch,
                prompt_hash,
                result,
            },
        )
    }

    /// (Project ownership is checked by the caller via `owns_result`.)
    pub(crate) fn on_overview_done(
        &mut self,
        prompt_hash: incremental::Version,
        result: Result<String, String>,
    ) -> Task<Message> {
        self.overview.generating = false;
        match result {
            Ok(markdown) => {
                // Persist the raw prose; fold the live module map in only
                // for display so the cache never carries a stale diagram.
                if let Some(store) = &self.derived_dir {
                    let _ = overview::save(
                        store,
                        &overview::Cached {
                            markdown: markdown.clone(),
                            prompt_hash,
                        },
                    );
                }
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

    /// (Project ownership is checked by the caller via `owns_result`: a late
    /// result would otherwise seed this project's registry with another
    /// project's files.)
    pub(crate) fn on_symbol_index_done(&mut self, indexed: index::Indexed) -> Task<Message> {
        self.indexing = false;
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
            .project
            .as_ref()
            .map(|p| p.files.iter().map(|f| f.abs.clone()).collect())
            .unwrap_or_default();
        let hashes: Vec<(PathBuf, incremental::Version)> = indexed
            .hashes
            .into_iter()
            .filter(|(path, _)| live.contains(path))
            .collect();
        let stale: HashSet<PathBuf> = self.registry.seed(hashes).into_iter().collect();
        // Merged per file rather than replacing the map wholesale: every file
        // untouched during the window still gets its index, and the ones the
        // watcher re-indexed (or created) meanwhile keep their newer symbols.
        for (path, syms) in indexed.by_file {
            if !stale.contains(&path) && live.contains(&path) {
                self.symbol_index_by_file.insert(path, syms);
            }
        }
        let changed_while_closed = indexed.changed.len();
        self.rebuild_symbol_index();
        // Build the import graph from the same single tree read, merged the
        // same way: `rebuild_import_graph` replaces every file's raw imports,
        // which would drop the out-edges the watcher installed during the
        // window. The graph was reset at project open, so installing this
        // build's files one by one leaves exactly that newer work standing.
        if let Some(resolver) = self.import_resolver() {
            for (path, raw) in indexed.imports_by_file {
                if stale.contains(&path) || !live.contains(&path) {
                    continue;
                }
                self.import_graph
                    .set_file(path, raw, &resolver, highlight::detect);
            }
        }
        self.import_cycles = self.import_graph.cycles();
        self.refresh_import_tree();
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
        let structure_task = self.request_structure_build();
        Task::batch([map_task, structure_task])
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
            .project
            .as_ref()
            .map(|p| (p.root.clone(), p.files.clone()))
        else {
            return Task::none();
        };
        if self.structure_building {
            self.structure_dirty = true;
            return Task::none();
        }
        self.structure_building = true;
        self.structure_dirty = false;
        // What the build read travels with the result: by the time it lands a
        // newer build may already have been applied, and only the revision can
        // tell the two apart (see `StructureBuilt::rev`).
        let rev = self.registry.revision();
        let epoch = self.project_epoch;
        Task::perform(
            {
                let build_root = root.clone();
                async move {
                    tokio::task::spawn_blocking(move || structure::build(&build_root, &files))
                        .await
                        .unwrap_or_default()
                }
            },
            move |index| Message::StructureBuilt {
                root: root.clone(),
                epoch,
                rev,
                index,
            },
        )
    }

    pub(crate) fn on_inlay_hints_loaded(
        &mut self,
        abs: PathBuf,
        hint_gen: u64,
        src_hash: incremental::Version,
        hints: Vec<lsp::client::InlayHint>,
    ) -> Task<Message> {
        // Both halves matter. The toggle may be off right now, or it may have
        // been turned off and on again since this request went out — in which
        // case these hints describe the earlier period and a fresher batch is
        // already coming.
        if !self.show_inlay_hints || hint_gen != self.inlay_gen {
            return Task::none();
        }
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
            // Applied only to a pane still showing the exact bytes the server
            // computed these hints for. `hint_gen` alone could not tell: it
            // moves on a TOGGLE, never on an edit, so an in-flight reply for
            // the pre-edit file was accepted and every chip landed on the
            // wrong token. Same test the highlighting pass makes.
            let Some(v) = slot
                .as_mut()
                .filter(|v| v.abs == abs)
                .filter(|v| incremental::content_hash(v.source.as_bytes()) == src_hash)
            else {
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
    /// names. Record the approval against the fingerprint the modal SHOWED,
    /// then restart the resolve flow from scratch — never spawn what the
    /// modal remembered. The command file may have changed while the dialog
    /// sat open; re-entering `ensure_lsp` re-fingerprints the file as it is
    /// NOW, so a swapped script fails the approval check and raises a fresh
    /// modal instead of running.
    pub(crate) fn on_lsp_command_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.pending_lsp_command.take() else {
            return Task::none();
        };
        // Record against the root/host the modal was raised for — never the
        // current project, which may have changed while the modal sat open.
        if self.project.as_ref().map(|p| &p.root) != Some(&c.root)
            || self.connection.approval_host().map(str::to_string) != c.host
        {
            self.status = "The project changed — nothing was approved".into();
            return Task::none();
        }
        if let Err(e) = self
            .trust
            .update(|t| t.approve_lsp(c.host.as_deref(), &c.root, &c.language, &c.fingerprint))
        {
            // `Trust::update` adopts the change only once the file is written,
            // so a failure means NOTHING was approved — not on disk, not in
            // memory. Falling through to `ensure_lsp` then re-raised the very
            // same modal, and the loop had no exit but closing the project.
            // Report it in the slot instead, which offers a deliberate Retry.
            self.status = format!("Could not record the approval: {e}");
            self.lsp.insert(
                c.language.clone(),
                LspSlot::Failed(format!("could not record the approval: {e}")),
            );
            return Task::none();
        }
        // The server enforces the same gate (SpawnLsp, the Ask agent's
        // semantic tools) — push the fresh approval before starting.
        self.send_lsp_approvals();
        self.lsp.remove(&c.language);
        self.ensure_lsp(&c.language)
    }

    /// Push this project's language-server command approvals to the server,
    /// which enforces them on every spawn path. Replaces the server's set.
    pub(crate) fn send_lsp_approvals(&mut self) {
        let (Some(root), Some(tx)) = (self.project.as_ref().map(|p| &p.root), &self.server_tx)
        else {
            return;
        };
        let approvals = self
            .trust
            .lsp_approvals_for(self.connection.approval_host(), root);
        let id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = tx.send(clew_protocol::ClientMessage {
            id,
            request: clew_protocol::Request::LspApprovals { approvals },
        });
    }

    pub(crate) fn on_lsp_consent_allowed(&mut self) -> Task<Message> {
        let Some(c) = self.pending_lsp_consent.take() else {
            return Task::none();
        };
        // A remote install: the consent turns into an `LspInstall` request —
        // the ONLY message the server installs anything on. The slot stays
        // AwaitingConsent so the `LspResolved` reply (Ready on success) is
        // picked up by the same handler that started this flow.
        if let LspProvision::Remote { .. } = &c.provision {
            self.lsp
                .insert(c.language.clone(), LspSlot::AwaitingConsent);
            if let Some(tx) = &self.server_tx {
                self.status = format!("Installing {} on the remote host…", c.server_name);
                let id = self
                    .next_req_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = tx.send(clew_protocol::ClientMessage {
                    id,
                    request: clew_protocol::Request::LspInstall {
                        language: c.language,
                    },
                });
            }
            return Task::none();
        }
        self.lsp.insert(c.language.clone(), LspSlot::Starting);
        let (dest, language, version) = (c.dest_dir, c.language, c.version);
        // Mint this install's generation so a result landing after a restart
        // or project switch is recognized as superseded.
        let generation = self.next_lsp_gen(&language);
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
                        generation,
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
                        generation,
                        result,
                    },
                )
            }
            LspProvision::Remote { .. } => unreachable!("handled by the early return above"),
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

    /// The server transport died mid-session (process exit, SSH drop). Every
    /// piece of in-flight bookkeeping tied to that transport is now garbage:
    /// replies can no longer arrive (the stream is gone), so anything still
    /// "pending" would wait forever, and proc handles name processes on a
    /// server that no longer exists. Clear it all, then bump `conn_gen` — the
    /// subscription is keyed on it, so iced starts a fresh transport, which is
    /// the reconnect.
    pub(crate) fn on_server_disconnected(&mut self) -> Task<Message> {
        self.server_tx = None;
        // A handshake refusal is not a crash. `on_handshake_failed` drops the
        // transport itself, and the server exits on that EOF — so this runs
        // for it too, and re-keying here started the SAME incompatible binary
        // again, got the same refusal, and dropped the transport again, about
        // every 1.5 s for as long as the window stayed open. Worse, each cycle
        // ran `drop_connection_state`, which kills the language servers this
        // client had started locally (its own children, in exactly the state
        // where there is no server to proxy them) before they finish indexing.
        // So: keep the local fallback the handshake failure already fell back
        // to, and keep the one message that says how to fix it. An explicit
        // user action re-arms the retry — see
        // `retry_server_after_handshake_failure`.
        //
        // Not closed here: the teardown below still drops locally-spawned
        // language servers along with the proxied ones — it cannot tell them
        // apart, and at its other call site (a target switch) clearing both is
        // required. This bounds that to the ONE disconnect a refusal produces
        // instead of one every 1.5 s.
        if let Some(why) = self.handshake_failure.clone() {
            self.drop_connection_state();
            self.status = why;
            return Task::none();
        }
        self.conn_gen += 1;
        self.conn_respawn = true;
        self.drop_connection_state();
        self.status = "clew-server disconnected — reconnecting…".into();
        Task::none()
    }

    /// Re-arm the transport after a handshake refusal latched the automatic
    /// reconnect off, if one did. Called from the paths where the USER asks
    /// for a project (folder picked, consent granted).
    ///
    /// Two reasons this cannot wait for the next crash. The binary may have
    /// been rebuilt since the refusal, and that is the fix the message asked
    /// for. And with no transport at all `start_scan` parks its root waiting
    /// for a server (`pending_scan_root`) — the ONLY thing that ever releases
    /// it is a handshake outcome, so without a fresh attempt the window would
    /// sit on "Scanning…" forever, which is the stall `on_handshake_failed`
    /// exists to prevent. Bumping the generation re-keys the subscription,
    /// which IS the reconnect; if the server is still incompatible the refusal
    /// lands again and releases the parked root into the local scan.
    pub(crate) fn retry_server_after_handshake_failure(&mut self) {
        if self.handshake_failure.take().is_some() {
            self.conn_gen += 1;
            // Not a respawn-after-crash: connect immediately, the user is
            // waiting on this project opening.
            self.conn_respawn = false;
        }
    }

    /// Stop everything running FOR the project being left — the work itself,
    /// not just its results.
    ///
    /// Resetting state only makes late results ignorable. The work carries on:
    /// an Explain pass keeps issuing LLM calls, a server-side agent keeps
    /// calling tools against the OLD root, an LSP refine keeps querying
    /// servers through clones it already holds, and a debuggee keeps running.
    /// All of it is billed, and some of it feeds the next project's context.
    ///
    /// Runs at both entries to a different project: opening one
    /// (`on_scan_done`) and switching transport (`connect_to`). Distinct from
    /// [`Self::drop_connection_state`], which handles the TRANSPORT dying, and
    /// must run before it — `AgentStop` has to reach the server over the
    /// channel the turn started on.
    pub(crate) fn drop_project_work(&mut self) -> Task<Message> {
        // The bottom-up Explain pass is thousands of LLM calls. Bumping the
        // generation only makes its results ignorable; an iced `Handle` does
        // NOT abort when dropped, and `explain_stream` has no cancel check of
        // its own, so without this the calls keep going (and keep being
        // billed). Aborting stops scheduling; for the client AI endpoint the
        // calls already handed to `spawn_blocking` still finish.
        if let Some(handle) = self.explain.abort.take() {
            handle.abort();
        }
        self.explain.running = false;
        self.explain.progress = None;
        // The old pass's error count is shown in the status bar next to the
        // progress it belongs to; carried over, it reads as this project's.
        self.explain.failed = 0;
        self.explain.generation += 1;

        // Navigation results are guarded ONLY by these counters, and the LSP
        // tasks that produce them hold CLONES of the old project's clients
        // (same reason the refine above needs an explicit abort), so clearing
        // `self.lsp` does not stop them. Without the bumps a late definition
        // or reference result jumps the editor to a path in the project — or
        // on the host — we have just left.
        self.goto_seq += 1;
        self.search_seq += 1;

        // The LSP refine holds CLONES of the old project's language-server
        // clients, so dropping `self.lsp` does not stop it.
        if let Some(handle) = self.project_calls.refine_abort.take() {
            handle.abort();
        }
        self.project_calls.generation += 1;
        self.project_calls.refine_progress = None;

        // The agent runs ON THE SERVER against the old root. Its cancel flag
        // lives in the server's map keyed by the stream id, so forgetting the
        // id locally would leave it looping with no way to stop it. (The
        // server also drains its own agent map on `OpenProject`, which covers
        // a lost frame or a client that never sends this.)
        if let Some(tx) = &self.server_tx {
            if let Some(stream) = self.agent_stream.take() {
                let _ = tx.send(clew_protocol::ClientMessage {
                    id: 0,
                    request: clew_protocol::Request::AgentStop { stream },
                });
            }
            // Same for a plain streamed answer: it runs on the server too.
            if let Some(sub) = self.chat_stream.take() {
                let _ = tx.send(clew_protocol::ClientMessage {
                    id: 0,
                    request: clew_protocol::Request::Cancel { sub },
                });
            }
        } else {
            self.agent_stream = None;
            self.chat_stream = None;
        }

        // In-flight per-project work that has a "busy" flag: clearing these
        // here (not only in `on_scan_done`) is what makes a transport switch
        // leave no stale spinner behind.
        self.indexing = false;
        self.building_embeddings = false;
        self.overview.generating = false;
        self.stats.building = false;
        self.project_calls.building = false;
        self.docs.loading = false;
        // The walkthrough generation's ONLY clear site is its own result
        // handler, and that result is dropped by the epoch guard the moment the
        // project or the transport changes — so the flag survived forever, and
        // the WALK tab marked whichever saved tour of the NEW project shared
        // the stranded scope string as "Generating…", building neither its
        // Regenerate nor its Delete control (or, with no such tour, painted a
        // phantom busy row that suppressed the empty state). `retried` is
        // stranded with it, which would silently spend the new project's one
        // automatic retry. The LLM call has no abort handle, so it still runs
        // to completion — only its result is discarded.
        self.walk.generating = None;
        self.walk.retried = false;
        // The embedding request is client-side and does answer, clearing this
        // itself — but not before the new project's Semantic tab has shown a
        // spinner for the OLD project's query.
        self.searching_semantic = false;
        // A docs build cleared here will never answer, so the revision it was
        // requested at describes nothing we hold: keeping the stamp would label
        // the PREVIOUS index as current for that revision (see
        // `DOCS_REV_STALE`). Written unconditionally, which also stales an idle
        // index — deliberately: across a transport switch this client cannot
        // know what changed where the files live, so the next visit to DOCS
        // rebuilds rather than trusting what it holds.
        self.docs.rev = crate::app::server_ai::DOCS_REV_STALE;
        self.refresh_pending = false;

        // The debuggee belongs to the old project: left running, its variables
        // would keep feeding the new project's Ask context. Literally the same
        // teardown the Stop button performs now, rather than a copy of it that
        // reads the same: the copy was where the breakpoint-verdict reset would
        // have gone missing.
        self.stop_debug_session()
    }

    /// Forget every request, stream, and process handle tied to the current
    /// (now dead or replaced) server transport. Shared by disconnect and
    /// (re)connect — a new transport must not inherit the old one's in-flight
    /// bookkeeping.
    pub(crate) fn drop_connection_state(&mut self) {
        // Every write still awaiting its acknowledgement is now unanswerable.
        // It stays marked dirty (`write_remote_state` marks before sending),
        // so the reconnect's re-read keeps this client's version and flushes
        // it — but the id will never be answered, so stop tracking it or a
        // later id collision could clear a mark it does not own.
        //
        // Record WHICH files those are before the ids go. From here that dirt
        // means something stronger than "unacknowledged": the server never got
        // it. Without the distinction, a second edit of the same file made in
        // the reconnect window put the rel back in flight and the `StateContent`
        // guard read the mark as "the merge is on its way", skipping the flush
        // that rescues this change — and the merge, which was computed without
        // it, was then adopted over this window's copy, erasing it from both
        // sides.
        self.remote_state_unsent
            .extend(self.remote_state_dirty.iter().cloned());
        self.remote_state_inflight.clear();
        // The rescue records go with the ids they are keyed on: a flush this
        // transport swallowed did not put its change on the remote's disk, so
        // nothing may retire the mark that says so — and the extend above has
        // just restated that the change is unsent.
        self.remote_state_rescue.clear();
        // The index publication counter is a SERVER-lifetime counter: the next
        // transport is a new clew-server whose `index_seq` restarts at 0.
        // Keeping the old high-water mark made the reconnect's own full
        // snapshot — and every partial until the fresh counter climbed past it
        // — look stale, and those partials are never re-sent, so a file edited
        // in that window stayed wrong for the rest of the session.
        self.remote_index_seq = 0;
        // Nothing will answer the OpenProject whose Tree this flag was waiting
        // for; the reconnect re-sends its own and sets it again.
        self.pending_tree_resync = false;
        self.pending_reads.clear();
        self.pending_git.clear();
        self.pane_pending = [None, None];
        self.pending_search = None;
        self.search.running = false;
        self.pending_docs = None;
        self.docs.loading = false;
        // Same as the teardown above: an unanswerable docs build must not leave
        // its request-time revision stamped on the index it was going to
        // replace, and a reconnect cannot vouch for what it already holds.
        self.docs.rev = crate::app::server_ai::DOCS_REV_STALE;
        // A "View docs" waiting on that build dies with it. Only `Event::Docs`
        // consumes the parked name, so a build abandoned here left it aimed at
        // the next successful build of the SAME project: long after the status
        // line said the server had gone, an unrelated rebuild opened that
        // symbol's page over the file the reader was on.
        self.docs.pending_view = None;
        // Nothing will answer the in-flight listing, and only the correlated
        // reply clears this spinner now.
        self.pending_list_dir = None;
        if let Some(ConnectStage::Browsing(b)) = self.connect.as_mut().map(|u| &mut u.stage) {
            b.loading = false;
        }
        // Dropping the oneshot senders wakes every task awaiting an AI reply
        // with an error; their (guarded) result messages reset the busy flags.
        self.ai_pending.lock().unwrap().clear();
        // Dropping the stream senders ends the Ask pumps; close any turn that
        // was still streaming so the UI doesn't show a spinner forever.
        self.chat_streams.lock().unwrap().clear();
        self.agent_streams.lock().unwrap().clear();
        self.agent_stream = None;
        // The transport is gone; the server dies with it, so there is nothing
        // left to cancel — just stop naming it.
        self.chat_stream = None;
        for turn in &mut self.ask_turns {
            if turn.streaming {
                turn.streaming = false;
                if turn.answer_md.trim().is_empty() {
                    turn.answer_md = "*Couldn't answer: server disconnected*".into();
                }
            }
        }
        self.asking = false;
        // Server-proxied processes (language servers, debug adapters) died with
        // the server. Drop the clients so the next need respawns them.
        self.proc_feeds.clear();
        self.lsp_procs.clear();
        self.lsp.clear();
        self.lsp_opened.clear();
        // Invalidate any in-flight spawn results for those dead processes.
        for g in self.lsp_gen.values_mut() {
            *g += 1;
        }
        self.seen_diag_version.clear();
        self.seen_inlay_epoch.clear();
        // A proxied debug session can't outlive its transport.
        if let Some(session) = self.debug.session.as_mut() {
            session.status = DebugStatus::Terminated;
            session.current = None;
        }
        self.bump_debug_run();
    }

    pub(crate) fn on_server_connected(
        &mut self,
        tx: tokio::sync::mpsc::UnboundedSender<clew_protocol::ClientMessage>,
    ) -> Task<Message> {
        // The in-process clew-server is up; keep its request channel and
        // greet it. Backend flows migrate onto this seam one at a time.
        let hello = clew_protocol::ClientMessage {
            id: HELLO_REQ_ID,
            request: clew_protocol::Request::Hello {
                protocol: clew_protocol::PROTOCOL_VERSION,
                fingerprint: clew_protocol::SCHEMA_FINGERPRINT.into(),
                ai: self.ai_endpoint(),
            },
        };
        let _ = tx.send(hello);
        // A fresh transport must not inherit the previous one's in-flight
        // bookkeeping (normally already cleared by the disconnect handler;
        // this also covers a target switch, where no disconnect fires).
        if self.server_tx.is_some() {
            self.drop_connection_state();
        }
        self.server_tx = Some(tx);
        // A transport is up again, so the previous refusal no longer stands:
        // this one gets a verdict of its own. Cleared here rather than in
        // `on_server_ready` so a REFUSAL from this server is what re-latches
        // it, not a leftover from the last one.
        self.handshake_failure = None;
        // Nothing else yet: business requests wait for the `Ready` reply
        // (`on_server_ready`). Pipelining them behind Hello meant a server
        // speaking another protocol version received — and half-answered —
        // requests on a connection neither side fully understood.
        Task::none()
    }

    /// The handshake did NOT succeed: the server refused our `Hello`, or
    /// answered with a protocol version / build we don't share. Every failure
    /// path lands here, because each one used to strand the deferred scan:
    /// `start_scan` parks its root waiting for a server, and the ONLY code
    /// that ever sends it is `on_server_ready`. Leaving `scanning` and
    /// `pending_scan_root` set painted "Scanning…" over an empty window for
    /// the rest of the session — no project, no fallback, and re-picking the
    /// folder just re-parked it.
    pub(crate) fn on_handshake_failed(&mut self, why: String) -> Task<Message> {
        // Drop the transport. Nothing can be asked of a server whose protocol
        // we don't share — it refuses every later request anyway — and
        // dropping the last sender ends the writer task, so the server sees
        // EOF and exits rather than lingering as a process nobody talks to.
        self.server_tx = None;
        // Latch the verdict BEFORE that EOF comes back as a disconnect. Every
        // path into this handler is a version or build mismatch between two
        // binaries on disk: spawning the same server again re-runs the same
        // refusal, so the disconnect handler must not treat it as a crash to
        // recover from (see `on_server_disconnected`). A rebuilt server is
        // still picked up without restarting clew — on the next project open,
        // which re-arms the attempt, rather than 1.5 s later.
        self.handshake_failure = Some(why.clone());
        self.status = why;
        let Some(root) = self.pending_scan_root.take() else {
            return Task::none();
        };
        // Same escape hatch `on_server_unavailable` takes, and the same
        // asymmetry: a remote root names a path on the OTHER host, so scanning
        // it here would open whatever this machine happens to have there.
        if self.connection.is_remote() {
            self.scanning = false;
            return Task::none();
        }
        self.local_scan(root)
    }

    /// The handshake succeeded (`Ready` matched our protocol): only now do
    /// business requests flow to the server.
    pub(crate) fn on_server_ready(&mut self) -> Task<Message> {
        // Resume a scan that was waiting for the server (its Tree reply
        // opens the project); otherwise, if a project is already open
        // (local-fallback path), tell the server about it for search.
        if let Some(root) = self.pending_scan_root.clone() {
            self.request_open_project(root);
        } else {
            self.sync_project_to_server();
            // Re-arm the remote `.clew/` session state. The previous
            // transport's `ReadState` replies can never arrive, so the rels
            // they would have cleared stay outstanding — and every later save
            // of history / bookmarks / notes / reading target is deferred
            // forever instead of being written. Re-reading clears them, and
            // anything changed while the link was down is still in
            // `remote_state_dirty` and flushes as each read lands.
            //
            // Not needed on the branch above: reopening the project runs
            // `on_scan_done`, which requests the state itself.
            if self.project.is_some() && !self.local_project_state() {
                self.request_remote_state();
            }
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
        // `scanning` and `pending_scan_root` are a pair — "an open is in
        // progress" and "which one" — so every path that abandons the parked
        // root has to drop the flag with it, exactly as `on_handshake_failed`
        // does below. Nothing else can: `on_scan_done` needs a Tree/ScanDone
        // that can no longer come, and the only other clear site is
        // `connect_to`. Left set, `ui::pane_area` returns the "Scanning
        // project…" placeholder ahead of every other branch, hiding the panes
        // of an already-open project (and the welcome screen when none is
        // open) until the user happens to reconnect.
        //
        // A remote bootstrap failure surfaces in the Connect modal rather
        // than falling back to a (meaningless) local scan of a remote path.
        if let Some(ui) = &mut self.connect
            && matches!(ui.stage, ConnectStage::Connecting { .. })
        {
            ui.stage = ConnectStage::Error(
                "Could not reach the host. Check the address, port, and key.".into(),
            );
            self.pending_scan_root = None;
            self.scanning = false;
            return Task::none();
        }
        // A remote transport that died outside the Connect modal (e.g. a
        // reconnect that failed): fail closed. The deferred root is a remote
        // path — a local scan of it would read this machine's files instead.
        if self.connection.is_remote() {
            self.pending_scan_root = None;
            self.scanning = false;
            self.status = "Lost the remote host — use Connect to reconnect.".into();
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
            send_ai_keys: ui.send_ai_keys,
        };
        self.remember_connection(conn.clone());
        let opt_in = conn.send_ai_keys;
        let stop_old = self.connect_to(conn.target());
        // After connect_to: it resets the opt-in for every transport switch.
        self.remote_ai_opt_in = opt_in;
        stop_old
    }

    /// Grant or revoke this host's permission to hold the AI keys, and make it
    /// take effect on the live connection.
    ///
    /// Revoking has to be an ACTIVE step: the server already holds the keys,
    /// so merely deciding to stop sending new ones leaves it able to keep
    /// using the old ones for the rest of the session.
    pub(crate) fn set_remote_ai_opt_in(&mut self, on: bool) {
        if self.remote_ai_opt_in == on {
            return;
        }
        self.remote_ai_opt_in = on;
        self.send_ai_config();
    }

    pub(crate) fn on_target_selected(&mut self, target: inactive::Target) -> Task<Message> {
        self.reading_target = target;
        self.show_tools_menu = false;
        self.show_target_menu = false;
        // The reading target is per-project state; with none open there is
        // nothing to persist it to, and the remote arm below would otherwise
        // write it into whichever project the server currently has.
        if self.project.is_none() {
            return Task::none();
        }
        // Re-evaluate the cfg dimming for every open file.
        let t = self.reading_target.clone();
        for v in self.panes.iter_mut().flatten() {
            if let Some(lang) = v.lang_key {
                let src = v.source.clone();
                v.inactive_lines = inactive::inactive_lines(&src, lang, &t);
            }
        }
        if !self.local_project_state() {
            self.write_remote_state(
                "reading.toml",
                reading::target_to_text(&self.reading_target),
            );
            return Task::none();
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = reading::save_target(&root, &self.reading_target)
        {
            self.status = format!("Could not save target: {e}");
        }
        Task::none()
    }

    /// (Project ownership is checked by the caller via `owns_result`.)
    pub(crate) fn on_project_calls_built(
        &mut self,
        graph: projectcalls::ProjectCallGraph,
    ) -> Task<Message> {
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
        self.project_calls.refine_abort = None;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> embed::Entry {
        embed::Entry {
            node: explain::Node::Function {
                file: PathBuf::from("/p/a.rs"),
                name: name.into(),
                ordinal: 0,
            },
            hash: 0,
            vec: vec![1.0, 0.0],
        }
    }

    /// The index is loaded once, at project open, and then kept for the whole
    /// session — so a config change made by ANOTHER window or by hand-editing
    /// `config.toml` leaves this window ranking a query embedded at the new
    /// endpoint against vectors from the old space. Cosine answers confidently
    /// either way, so nothing about the results says they are meaningless.
    /// FIND must refuse and ask for a rebuild instead.
    #[test]
    fn find_refuses_an_index_the_live_embedding_config_disowns() {
        let dir = std::env::temp_dir().join("clew-embed-space-find-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // `App::blank()` reads `trust.toml` / `connections.toml` through the
        // data dir, and the handler reads the config file from it, so isolate
        // both (holding the env lock for the whole test).
        let _env = clew_core::env_lock();
        // SAFETY: env mutation is serialized by the lock held above.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        std::fs::write(
            dir.join("config.toml"),
            "[embedding]\napi_key = \"sk\"\nmodel = \"m-b\"\n",
        )
        .unwrap();

        let mut app = App::blank();
        app.semantic_query = "where is the parser".into();
        // Built under m-a, which is not what the config names now. The
        // endpoint matches, so the MODEL half is what has to disown it.
        app.embed_index = embed::Index {
            model: "m-a".into(),
            base_url: "https://api.openai.com/v1".into(),
            entries: vec![entry("f")],
        };
        app.semantic_results = vec![(entry("f").node, 0.9)];
        let _ = app.on_semantic_search();
        assert!(
            app.embed_index.entries.is_empty(),
            "vectors from the old space stayed queryable"
        );
        assert!(
            app.semantic_results.is_empty(),
            "results ranked in the old space stayed on screen"
        );
        assert!(
            !app.searching_semantic,
            "the query was embedded to be ranked against a foreign index"
        );
        assert!(
            app.status.contains("Build the semantic index first"),
            "the refusal was not explained: {}",
            app.status
        );

        // An index that DOES belong to the live space is still queried — the
        // check must not cost a rebuild on every search.
        app.embed_index = embed::Index {
            model: "m-b".into(),
            base_url: "https://api.openai.com/v1".into(),
            entries: vec![entry("f")],
        };
        let _ = app.on_semantic_search();
        assert_eq!(
            app.embed_index.entries.len(),
            1,
            "an index in the live space was thrown away"
        );
        assert!(app.searching_semantic, "the query never went out");

        // SAFETY: same lock, still held.
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
