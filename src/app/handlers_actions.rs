//! Message handlers for discrete UI actions (toggles, selection, bookmarks, finder, menus, small async completions).

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn on_tick(&mut self) -> Task<Message> {
        // Snapshot each ready server's diagnostics + inlay-refresh epoch.
        let versions: Vec<(String, u64, u64)> = self
            .lsp
            .iter()
            .filter_map(|(lang, slot)| match slot {
                LspSlot::Ready(c) => Some((lang.clone(), c.diag_version(), c.inlay_epoch())),
                _ => None,
            })
            .collect();
        // Languages where the server just did work (re-analyzed, or asked
        // us to refresh inlay hints): (re)fetch hints for their shown
        // files. This is what makes hints appear after a cold-start
        // server finishes indexing and pushes inlayHint/refresh.
        let changed: Vec<String> = versions
            .iter()
            .filter(|(lang, diag, epoch)| {
                self.seen_diag_version.get(lang).copied() != Some(*diag)
                    || self.seen_inlay_epoch.get(lang).copied() != Some(*epoch)
            })
            .map(|(lang, _, _)| lang.clone())
            .collect();
        for (lang, diag, epoch) in &versions {
            self.seen_diag_version.insert(lang.clone(), *diag);
            self.seen_inlay_epoch.insert(lang.clone(), *epoch);
        }
        let mut inlay_tasks = Vec::new();
        for lang in &changed {
            let files: Vec<PathBuf> = self
                .panes
                .iter()
                .flatten()
                .filter(|v| v.lang_key == Some(lang.as_str()))
                .map(|v| v.abs.clone())
                .collect();
            for abs in files {
                inlay_tasks.push(self.inlay_request_lookup(&abs));
            }
        }
        // A change queued during the auto-refresh cooldown: fire it once
        // the window has lifted and nothing is running.
        let refresh = if self.refresh_pending
            && !self.explain.running
            && !self.overview.generating
            && !self.building_embeddings
            && self
                .last_auto_refresh
                .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
                .unwrap_or(true)
        {
            self.begin_refresh()
        } else {
            Task::none()
        };
        Task::batch([Task::batch(inlay_tasks), refresh])
    }

    pub(crate) fn on_sidebar_tab_picked(&mut self, tab: SidebarTab) -> Task<Message> {
        self.sidebar = tab;
        self.show_left_sidebar = true; // reveal it for external triggers
        self.show_tools_menu = false; // close the More menu if it opened this
        // Always scroll the picked tab into view — the strip scrolls horizontally
        // and a tab off the right edge would otherwise look unselected.
        let reveal = ui::reveal_sidebar_tab(tab);
        let action = match tab {
            SidebarTab::Search => {
                // The search input takes keyboard focus.
                self.code_focused = false;
                operation::focus(ui::search_input_id())
            }
            SidebarTab::Imports => {
                // Sync the tree with the current file when the tab opens.
                self.refresh_import_tree();
                Task::none()
            }
            SidebarTab::Walk => {
                // Prepare the open tour's current step (markdown/mermaid)
                // if we haven't yet (e.g. a cached tour was just loaded).
                match self
                    .walk
                    .open
                    .and_then(|o| self.walk.library.get(o))
                    .and_then(|w| w.steps.get(self.walk.step))
                {
                    Some(step) if self.walk.prepared.is_empty() => {
                        let (prepared, task) = self.prepare_segments(&step.narration.clone());
                        self.walk.prepared = prepared;
                        task
                    }
                    _ => Task::none(),
                }
            }
            SidebarTab::Docs => {
                // Build the API docs the first time the tab is opened — and
                // REBUILD them when the index predates the current revision.
                // Gating on an empty list alone meant every edit made while
                // another tab was visible (the only automatic rebuild fires on
                // `FilesChanged` while DOCS is the visible tab) left the
                // pre-edit API surface standing here for the rest of the
                // session: old signatures, old doc text, and an "Open source"
                // button jumping to a line the edit had moved.
                self.ensure_docs();
                Task::none()
            }
            _ => Task::none(),
        };
        Task::batch([reveal, action])
    }

    pub(crate) fn on_toggle_diff(&mut self) -> Task<Message> {
        // Toggle off if already showing this file's diff.
        let active_abs = self.active_viewer().map(|v| v.abs.clone());
        if let (Some(d), Some(abs)) = (&self.diff, &active_abs)
            && d.abs == *abs
        {
            self.diff = None;
            return Task::none();
        }
        let Some(abs) = active_abs else {
            return Task::none();
        };
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let rel = self.rel_of(&abs);
        // Remote: the repository lives on the remote host.
        let remote_ai = (!self.local_project_state()).then(|| self.ai_client());
        Task::perform(
            async move {
                let lines = match remote_ai {
                    Some(ai) => ai
                        .git::<Option<Vec<git::DiffLine>>>(clew_protocol::GitOp::DiffLines {
                            rel: rel.clone(),
                        })
                        .await
                        .unwrap_or_default(),
                    None => {
                        let file = abs.clone();
                        tokio::task::spawn_blocking(move || {
                            git::diff_lines(&root, &file).unwrap_or_default()
                        })
                        .await
                        .unwrap_or_default()
                    }
                };
                (abs, rel, lines)
            },
            |(abs, rel, lines)| Message::DiffLoaded { abs, rel, lines },
        )
    }

    pub(crate) fn on_open_link(&mut self, url: String) -> Task<Message> {
        // clew:<rel>[:line] — the citation scheme `linkify_citations` mints;
        // jump to that file (and line) in the editor. Provenance is NOT
        // guaranteed, see the check below.
        if let Some(target) = url.strip_prefix("clew:") {
            let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                return Task::none();
            };
            let (rel, line) = match target.rsplit_once(':') {
                Some((p, n)) => match n.parse::<usize>() {
                    Ok(n) => (p, Some(n)),
                    Err(_) => (target, None),
                },
                None => (target, None),
            };
            // The scheme is only ever MINTED by `linkify_citations`, but nothing
            // forces a `clew:` URL to have come from there: iced's markdown
            // widget passes any link destination through verbatim, and the
            // markdown we render is untrusted — a repository README, a `///`
            // doc comment shown in the Docs tab, a prompt-injected LLM answer.
            // So `rel` is attacker data. `Path::join` DISCARDS the root when the
            // argument is absolute, so `[Setup](clew:/Users/me/.ssh/id_rsa)`
            // used to open that file in the editor under a project-looking
            // breadcrumb, from one click on innocuous link text.
            //
            // This test is purely lexical, so it holds identically for a REMOTE
            // project, where probing this machine's disk at the remote's paths
            // is itself forbidden. It rejects exactly what `linkify_citations`
            // never emits: empty, absolute, and any `.`/`..` component.
            if !clew_core::statefile::safe_rel(rel) {
                self.status = format!("Refused a link outside the project: {url}");
                return Task::none();
            }
            let abs = root.join(rel);
            // Local only, and only defence in depth: a repo-shipped symlinked
            // DIRECTORY component (`link/x` with `link -> /etc`) passes every
            // lexical test, and the fallback read used when clew-server isn't
            // up (`tasks::read_text_file`) has no containment check of its own.
            // A remote target must never be resolved against this filesystem,
            // so remote keeps the lexical result alone and relies on the
            // server's own `confine` in `ReadFile`. Decided from the path, and
            // the read that follows resolves it again, so a component swapped
            // in between stays open — the same residual `.clew` documents.
            if self.local_project_state() && !clew_core::fs_scan::is_inside(&root, &abs) {
                self.status = format!("Couldn't open {rel}: not a file inside the project");
                return Task::none();
            }
            return self.open_file(abs, line, true);
        }
        // http(s): hand a validated plain URL to the OS opener — never
        // file://, javascript:, a leading '-' (flag injection), etc.
        if url.starts_with("http://") || url.starts_with("https://") {
            let safe = !url.contains(['\n', '\r', '\0']) && url.len() < 2048;
            if safe {
                let opener = if cfg!(target_os = "macos") {
                    "open"
                } else if cfg!(target_os = "windows") {
                    "explorer"
                } else {
                    "xdg-open"
                };
                let _ = std::process::Command::new(opener).arg(&url).spawn();
            } else {
                self.status = format!("Refused to open link: {url}");
            }
            return Task::none();
        }
        // Otherwise treat it as a project-file reference (the overview's
        // links), e.g. `src/find.rs` or `find.rs#L20` — jump to it.
        if let Some((abs, line)) = self.resolve_project_link(&url) {
            self.overview.showing = false;
            self.stats.showing = false;
            return self.open_file(abs, line, true);
        }
        self.status = format!("Couldn't resolve link: {url}");
        Task::none()
    }

    pub(crate) fn on_settings_saved(&mut self) -> Task<Message> {
        // The embedding space the vectors in `self.embed_index` belong to, read
        // BEFORE the write. Not `self.settings.ai_snapshot.1`: that is the form
        // as the modal opened, which another window may have overtaken, while
        // this is what the config says one instant before we change it.
        let space_before = embed::stored_space();
        // Commit the previewed appearance (applied live but not persisted while
        // the modal was open) and refresh the snapshot so Close won't revert it.
        let _ = theme::save(self.theme_pref);
        self.settings.theme_snapshot = (
            self.theme_pref,
            theme::current_light().id,
            theme::current_dark().id,
        );
        let cfg = llm::Config::from_parts(
            self.settings.provider,
            self.settings.key.clone(),
            self.settings.model.clone(),
            self.settings.base_url.clone(),
        );
        let emb = embed::Config::from_parts(
            self.settings.embed_key.clone(),
            self.settings.embed_model.clone(),
            self.settings.embed_base_url.clone(),
        );
        // Written as an EDIT of what the modal was opened with, not as a
        // wholesale replacement of both sections: this form's values are as
        // old as the modal, and Save is also the only way to commit a theme
        // change, so a Save the user believed only changed the theme wrote a
        // stale blank over an API key another window had stored in between —
        // and `send_ai_config` below then revoked it on the server too. A
        // field another writer has changed since is kept, and named.
        let saved = cfg
            .save_from(&self.settings.ai_snapshot.0)
            .and_then(|mut kept| {
                emb.save_from(&self.settings.ai_snapshot.1)
                    .map(|embed_kept| {
                        kept.extend(embed_kept);
                        kept
                    })
            });
        match saved {
            Ok(kept) => {
                self.llm_available = llm::Config::available();
                self.embed_available = embed::Config::available();
                self.settings.open = false;
                // A changed embedding space makes every vector held in memory
                // unusable, and keeping them is worse than losing them: FIND
                // would embed the query at the NEW endpoint and rank it against
                // OLD-space vectors (cosine still answers confidently), and a
                // "Build index" would REUSE them — the builder's gate is the
                // summary hash, which a config change does not move — and then
                // save the mix stamped with the new space, at which point
                // `load_for` trusts it forever. `load_for` applies this rule to
                // the file, but only when the project opens; nothing was
                // re-applying it to the copy already in memory.
                let dropped =
                    embed::stored_space() != space_before && !self.embed_index.entries.is_empty();
                if dropped {
                    self.embed_index = embed::Index::default();
                    self.semantic_results.clear();
                }
                self.status = if !kept.is_empty() {
                    format!(
                        "Settings saved — {} changed in another window and was kept",
                        kept.join(", ")
                    )
                } else if self.llm_available {
                    format!("Settings saved ({})", cfg.provider.label())
                } else {
                    "Saved — add an API key to enable Explain".into()
                };
                if dropped {
                    self.status
                        .push_str(" — semantic index dropped (new embedding space); rebuild it");
                }
                // The server holds a copy for server-endpoint AI calls (Ask's
                // agent turns) — keep it in step with the new settings.
                self.send_ai_config();
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
        Task::none()
    }

    pub(crate) fn on_select_start(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        if pane == 0 || self.split {
            self.active = pane;
        }
        // Clicking the code gives it keyboard focus for cursor motion.
        self.code_focused = true;
        // Cmd/Ctrl-click is go-to-definition, not selection.
        if self.modifiers.command() && !self.modifiers.shift() {
            return self.goto_definition(pane, line, col);
        }
        let extend = self.modifiers.shift();
        if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
            let head = (line, col);
            match (extend, v.selection) {
                // Shift-click keeps the existing anchor and moves the head.
                (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                _ => v.selection = Some((head, head)),
            }
            v.caret = Some(head);
            self.selecting = true;
        }
        let follow = self.follow_caret(Task::none());
        Task::batch([follow, self.sync_reading_context()])
    }

    pub(crate) fn on_view_docs_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let Some(word) = self
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .and_then(|v| analyze::word_at(&v.lines, menu.line, menu.col))
        else {
            return Task::none();
        };
        // Docs are built AND current but hold no entry for this symbol (e.g. an
        // undocumented private item): rather than silently doing nothing,
        // fall back to its definition so "View docs" always lands the
        // reader somewhere useful. A stale index cannot support that verdict —
        // it reported "No doc entry for X" for every symbol added since it was
        // built — so it is sent to `view_docs_for`, which rebuilds and resolves
        // the name against the answer.
        if self.docs_fresh() && find_doc_by_name(&self.docs.files, &word).is_none() {
            self.status = format!("No doc entry for “{word}” — showing its definition");
            return self.goto_definition(menu.pane, menu.line, menu.col);
        }
        self.view_docs_for(&word);
        Task::none()
    }

    #[allow(clippy::too_many_arguments)] // one highlight pass's whole result
    pub(crate) fn on_highlighted(
        &mut self,
        abs: PathBuf,
        src_hash: incremental::Version,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: HashMap<usize, String>,
        inactive: HashSet<usize>,
        target: inactive::Target,
    ) -> Task<Message> {
        let lines = Arc::new(lines);
        // Lines, symbols and docs follow from the bytes alone, so the hash
        // below vouches for them. `inactive` does not: it also depends on the
        // reading target, and accepting a pass that ran against the PREVIOUS
        // target put the old dimming back over unchanged source. When the
        // target has moved on, keep what the pane already has —
        // `on_target_selected` recomputed it for every open pane at the moment
        // of the change, so it is current by construction.
        let dimming_is_current = target == self.reading_target;
        for slot in &mut self.panes {
            // Applied only to a pane still showing the exact bytes this pass
            // ran over. Two passes for one file can be in flight and finish
            // in either order, and the old test — same path, same line COUNT
            // — happily accepted a stale pass whenever an edit left the line
            // count alone. The view then showed the previous highlighting,
            // symbols and doc comments over source the rest of the app (LSP,
            // index, search) already treated as current.
            if let Some(v) = slot
                && v.abs == abs
                && incremental::content_hash(v.source.as_bytes()) == src_hash
            {
                v.set_lines(lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                if dimming_is_current {
                    v.inactive_lines = inactive.clone();
                }
                v.highlighted = true;
            }
        }
        // Symbols just landed — resolve the function under the caret.
        self.follow_caret(Task::none())
    }

    pub(crate) fn on_walkthrough_delete(&mut self, i: usize) -> Task<Message> {
        let Some(gone) = self.walk.library.get(i).map(|w| w.scope.clone()) else {
            return Task::none();
        };
        // The open tour is remembered by scope, not by index: the merge below
        // re-reads a library another window may have appended to, so every
        // index in this window's snapshot can move.
        let open_scope = match self.walk.open {
            Some(o) if o == i => None,
            Some(o) => self.walk.library.get(o).map(|w| w.scope.clone()),
            None => None,
        };
        self.walk.library.remove(i);
        if self.local_project_state()
            && let Some(root) = self.project.as_ref().map(|p| p.root.clone())
        {
            // Delete by scope (the key tours are upserted on) against what is
            // on disk now: writing this window's whole library back erased
            // every tour a second window had generated since it loaded.
            let (merged, saved) = walkthrough::edit_library(&root, |lib| {
                lib.retain(|w| w.scope != gone);
            });
            // Adopted on failure too, so the deletion the user asked for holds
            // for the session instead of the tour reappearing in the sidebar.
            self.walk.library = merged;
            if let Err(e) = saved {
                self.status = format!("Could not save walkthrough: {e} — removed for this session");
            }
        } else {
            self.save_walkthrough_scope(&gone, None);
        }
        self.walk.open =
            open_scope.and_then(|s| self.walk.library.iter().position(|w| w.scope == s));
        if self.walk.open.is_none() {
            self.walk.prepared = Vec::new();
        }
        Task::none()
    }

    pub(crate) fn on_context_menu_opened(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
        x: f32,
        y: f32,
    ) -> Task<Message> {
        if pane == 0 || self.split {
            self.active = pane;
        }
        // Content space → window space (see HoverRequested): drop the
        // pane's scroll so the menu opens at the click, not below it.
        let y = y - self
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or(0.0, |v| v.scroll_y);
        self.context_menu = Some(ContextMenu {
            pane,
            line,
            col,
            x,
            y,
        });
        Task::none()
    }

    pub(crate) fn on_bookmark_toggled(&mut self) -> Task<Message> {
        let line_height = self.line_height();
        let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let line = v.current_line(line_height);
        let mut preview = v.line_text(line).trim().to_string();
        if preview.chars().count() > 80 {
            preview = preview.chars().take(80).collect();
        }
        let rel = v.rel.clone();
        // A remote project's bookmarks persist over the protocol, where the
        // project lives — never in a same-pathed local .clew.
        let mut added = false;
        let saved = if self.local_project_state() {
            // Toggle against what is on disk now, not against this window's
            // open-time snapshot: a second window on the same project has been
            // writing the same file, and a wholesale write of our copy erased
            // everything it added. Adopt the merged list so this window stops
            // rendering a copy that disagrees with disk.
            let (merged, saved) = bookmarks::edit(&root, |list| {
                added = bookmarks::toggle(list, &rel, line, preview)
            });
            // Adopted even when the write failed: the toggle is what the user
            // just did, and reverting to the pre-toggle list would make the
            // gutter disagree with the click as well as with disk.
            self.bookmarks = merged;
            saved
        } else {
            // The same toggle, replayed by the SERVER on the remote file: this
            // window's list is its copy from project open, and shipping it
            // wholesale deleted every bookmark another client had added since.
            // Applied here as well so the gutter answers the click without
            // waiting for the round trip; the merged file replaces it when it
            // lands (`StateEdited`).
            let merge = bookmarks::merge_toggle(&rel, line, preview.clone());
            added = bookmarks::toggle(&mut self.bookmarks, &rel, line, preview);
            self.edit_remote_state(bookmarks::REL, merge);
            Ok(())
        };
        self.status = match saved {
            Ok(()) if added => format!("Bookmarked {rel}:{line}"),
            Ok(()) => format!("Removed bookmark {rel}:{line}"),
            Err(e) => {
                format!("Cannot write .clew/bookmarks.json: {e} — kept for this session, not saved")
            }
        };
        Task::none()
    }

    pub(crate) fn on_tree_updated(&mut self, epoch: u64, result: ScanResult) -> Task<Message> {
        // Only apply to the project instance this rescan was started for. The
        // root alone cannot say that: a local project and a remote one can
        // share an absolute path while being different machines' code, and
        // the file list drives the index, the graphs and the AI context.
        if self.owns_result(&result.root, epoch) {
            if let Some(p) = &mut self.project {
                p.tree = result.tree;
                p.files = Arc::new(result.files);
                p.truncated = result.truncated;
            }
            self.refresh_finder();
            // The file set changed, so imports that were unresolved (or
            // resolved to a since-moved file) may now resolve differently.
            self.reresolve_import_graph();
            if let Some(p) = &self.project {
                self.status = format!(
                    "{} files · {} symbols",
                    p.files.len(),
                    self.symbol_index.len()
                );
            }
            // This is the first point at which a created or deleted Rust file
            // is part of the file set, so it is where the structure index can
            // learn its `impl` blocks — the watcher batch that saw the creation
            // ran against the tree as it was before it (see `on_files_rehashed`).
            return self.request_structure_build();
        }
        Task::none()
    }

    pub(crate) fn on_find_opened(&mut self) -> Task<Message> {
        if self.active_viewer().is_none() {
            return Task::none();
        }
        self.find.open = true;
        self.code_focused = false; // the find input takes focus
        // Reveal everything so matches inside collapsed folds are shown.
        if let Some(v) = self.active_viewer_mut() {
            v.expand_all();
        }
        self.recompute_find();
        Task::batch([
            operation::focus(ui::find_input_id()),
            operation::select_all(ui::find_input_id()),
        ])
    }

    pub(crate) fn on_toggle_inlay_hints(&mut self) -> Task<Message> {
        self.show_inlay_hints = !self.show_inlay_hints;
        self.show_tools_menu = false;
        if self.show_inlay_hints {
            // Re-fetch for every shown file.
            let files: Vec<PathBuf> = self.panes.iter().flatten().map(|v| v.abs.clone()).collect();
            let tasks: Vec<Task<Message>> = files
                .iter()
                .map(|abs| self.inlay_request_lookup(abs))
                .collect();
            Task::batch(tasks)
        } else {
            // Clear so the hints disappear immediately, and invalidate every
            // request still in flight. Clearing alone was not enough: a reply
            // already on its way repopulated the hints a moment later, so the
            // toggle read "off" while the hints stayed on screen.
            self.inlay_gen += 1;
            for v in self.panes.iter_mut().flatten() {
                v.inlay_hints.clear();
            }
            Task::none()
        }
    }

    pub(crate) fn on_call_hierarchy_direction(&mut self) -> Task<Message> {
        let Some(tree) = &self.call_graph else {
            return Task::none();
        };
        let toggled = tree.direction.toggled();
        let lang = tree.lang;
        let was_full = tree.full;
        let root_items: Vec<_> = tree
            .roots()
            .iter()
            .map(|&r| tree.node(r).item.clone())
            .collect();
        // The flipped tree is a new identity: in-flight children fetched from
        // the old one carry its token and can no longer graft onto this one.
        self.call_token += 1;
        let mut rebuilt = callgraph::CallTree::new(self.call_token, toggled, lang, root_items);
        rebuilt.full = was_full; // keep "expand all" across a direction flip
        self.call_graph = Some(rebuilt);
        let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(
            roots
                .into_iter()
                .map(|r| self.fetch_children(r))
                .collect::<Vec<_>>(),
        )
    }

    pub(crate) fn on_semantic_results(
        &mut self,
        query: String,
        result: Result<Vec<f32>, String>,
    ) -> Task<Message> {
        self.searching_semantic = false;
        if query != self.semantic_query.trim() {
            return Task::none(); // superseded by a newer query
        }
        match result {
            Ok(qvec) => {
                self.semantic_results = embed::search(&self.embed_index, &qvec, 20)
                    .into_iter()
                    .map(|(n, s)| (n.clone(), s))
                    .collect();
                self.status = format!("{} semantic matches", self.semantic_results.len());
            }
            Err(e) => self.status = format!("Search failed: {e}"),
        }
        Task::none()
    }

    pub(crate) fn on_open_overlay(&mut self, which: Overlay) -> Task<Message> {
        // The server panel and an overlay are mutually exclusive modals.
        self.server_panel = false;
        self.overlay = Some(which);
        // The call graph is built on demand; (re)build it if the project
        // changed since the last build — but never launch a second build
        // while one is already in flight (single-flight).
        if which == Overlay::ProjectCalls
            && !self.project_calls.building
            && (self.project_calls.graph.is_empty()
                || self.project_calls.rev != self.registry.revision())
        {
            return self.build_project_calls();
        }
        self.refresh_graph_layout();
        Task::none()
    }

    pub(crate) fn on_window_resized(&mut self, size: Size) -> Task<Message> {
        self.window_width = size.width;
        self.window_height = size.height;
        // Keep panel sizes sane against the new window bounds.
        self.clamp_panel_sizes();
        // Keep the materialized window generous enough for the new
        // height until the next scroll event refines it.
        for v in self.panes.iter_mut().flatten() {
            v.viewport_h = v.viewport_h.max(size.height);
        }
        // The content layer is re-laid-out on resize; re-assert the frameless
        // chrome so the corner clip and hidden title bar survive (idempotent,
        // cheap). Leaving native fullscreen also lands here, where AppKit has
        // rebuilt and re-shown the title bar.
        #[cfg(target_os = "macos")]
        macos::configure_frameless(10.0);
        Task::none()
    }

    pub(crate) fn on_blame_why_done(
        &mut self,
        token: u64,
        root: &Path,
        epoch: u64,
        title: String,
        commits: Vec<(String, String)>,
        result: Result<String, String>,
    ) -> Task<Message> {
        // Apply only while the popup is still waiting for THIS request, and
        // only in the project it was asked in. "The popup is open" was not
        // enough: asking about A, closing it, then asking about B let A's late
        // answer replace B's — under B's title, and across a project switch,
        // in a popup about entirely different code.
        if !self.owns_result(root, epoch) || self.blame_why.as_ref().map(|b| b.token) != Some(token)
        {
            return Task::none();
        }
        let md = match result {
            Ok(m) => m,
            Err(e) => {
                self.status = format!("Couldn't explain: {e}");
                format!("*Couldn't explain why: {e}*")
            }
        };
        let (prepared, task) = self.prepare_segments(&md);
        self.blame_why = Some(BlameWhy {
            token,
            title,
            commits,
            loading: false,
            prepared,
        });
        task
    }

    pub(crate) fn on_toggle_dir(&mut self, rel: String) -> Task<Message> {
        // Cmd+click a folder shows its architectural explanation instead
        // of expanding it.
        if self.modifiers.command()
            && let Some(project) = &self.project
        {
            let node = explain::Node::Folder(project.root.join(&rel));
            self.show_right_panel = true;
            return self.show_explanation(node);
        }
        if !self.expanded.remove(&rel) {
            self.expanded.insert(rel);
        }
        Task::none()
    }

    pub(crate) fn on_time_travel_story_done(
        &mut self,
        result: Result<String, String>,
    ) -> Task<Message> {
        let md = match result {
            Ok(md) => md,
            Err(e) => {
                self.status = format!("Story failed: {e}");
                return Task::none();
            }
        };
        let (prepared, task) = self.prepare_segments(&md);
        if let Some(tt) = self.time_travel.as_mut() {
            tt.story_loading = false;
            tt.story = Some(prepared);
        }
        task
    }

    pub(crate) fn on_minimap_scrolled(&mut self, pane: usize, fraction: f32) -> Task<Message> {
        let lh = self.line_height();
        if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
            let total = v.content_rows() as f32 * lh;
            let max_y = (total - v.viewport_h).max(0.0);
            // Center the clicked fraction in the viewport.
            let y = (fraction * total - v.viewport_h / 2.0).clamp(0.0, max_y);
            v.scroll_y = y;
            return operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        }
        Task::none()
    }

    pub(crate) fn on_lsp_remove(&mut self, name: String, version: String) -> Task<Message> {
        // Stop any running instance of this server first.
        let langs: Vec<String> = lsp::registry::by_name(&name)
            .map(|s| s.languages.iter().map(|l| l.to_string()).collect())
            .unwrap_or_default();
        for lang in langs {
            self.lsp.remove(&lang);
        }
        match lsp::store::remove(&name, &version) {
            Ok(_) => self.status = format!("Removed {name} {version}"),
            Err(e) => self.status = format!("Remove failed: {e}"),
        }
        self.installed_servers = lsp::store::installed_servers();
        Task::none()
    }

    /// (Project ownership is checked by the caller via `owns_result`.)
    pub(crate) fn on_embeddings_built(
        &mut self,
        result: Result<embed::Index, String>,
    ) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        self.building_embeddings = false;
        match result {
            Ok(index) => {
                // Folded into the stored index rather than written over it:
                // this build covers the nodes THIS window's explanation cache
                // holds, and the derived store is shared by every window and
                // every clew process on the project, so a wholesale write
                // shrank a whole-project index down to one window's subset.
                let root = self.project.as_ref().map(|p| p.root.clone());
                let (index, saved) = match (&self.derived_dir, root) {
                    (Some(store), Some(root)) => {
                        let (merged, saved) = embed::merge_built(store, &root, &index);
                        // The merged index is adopted whether or not the write
                        // landed: the vectors are usable for this session, and
                        // only the persistence failed.
                        (merged, saved.is_ok())
                    }
                    _ => (index, true),
                };
                self.status = match saved {
                    true => format!("Semantic index ready ({} items)", index.entries.len()),
                    false => format!(
                        "Semantic index ready ({} items) — not saved",
                        index.entries.len()
                    ),
                };
                self.embed_index = index;
            }
            Err(e) => self.status = format!("Index build failed: {e}"),
        }
        Task::none()
    }

    pub(crate) fn on_connect_field(&mut self, field: ConnectField, value: String) -> Task<Message> {
        if let Some(ui) = &mut self.connect {
            match field {
                ConnectField::Name => ui.name = value,
                ConnectField::Host => ui.host = value,
                ConnectField::User => ui.user = value,
                // Keep only digits so the port stays parseable.
                ConnectField::Port => {
                    ui.port = value.chars().filter(char::is_ascii_digit).collect()
                }
                ConnectField::Identity => ui.identity = value,
            }
        }
        Task::none()
    }

    pub(crate) fn on_copy_selection(&mut self) -> Task<Message> {
        // In time travel, copy the historical selection, not the live one.
        let viewer = self
            .time_travel
            .as_ref()
            .and_then(|t| t.viewer.as_ref())
            .or_else(|| self.active_viewer());
        let Some(text) = viewer.and_then(Viewer::selected_text) else {
            return Task::none();
        };
        let n = text.lines().count();
        self.status = format!("Copied {n} line{}", if n == 1 { "" } else { "s" });
        iced::clipboard::write(text)
    }

    pub(crate) fn on_consent_allowed(&mut self) -> Task<Message> {
        let Some(root) = self.pending_consent.take() else {
            return Task::none();
        };
        // Consent is recorded outside the project (see `request_open`): a
        // repository must never be able to grant itself permission. It is
        // bound to the host it was granted for.
        let host = self.connection.approval_host().map(str::to_string);
        if let Err(e) = self.trust.update(|t| t.trust_root(host.as_deref(), &root)) {
            self.pending_open = None;
            self.status = format!("Cannot record consent: {e}");
            return Task::none();
        }
        // `.clew/` still holds this project's own state (bookmarks, caches);
        // create it now so the first save doesn't fail, but a failure here is
        // not fatal — a read-only project still opens, it just can't persist.
        // Never for a remote project: the root is a remote path, and creating
        // it HERE would plant directories on the local machine.
        if self.local_project_state() {
            let _ = std::fs::create_dir_all(root.join(".clew"));
        }
        // The user asking for a project is the retry a latched handshake
        // refusal does not take on its own; `start_scan` below would otherwise
        // park this root waiting for a server that can never arrive.
        self.retry_server_after_handshake_failure();
        self.start_scan(root)
    }

    pub(crate) fn on_call_hierarchy_expand(&mut self, id: usize) -> Task<Message> {
        let needs = self.call_graph.as_ref().is_some_and(|t| t.needs_fetch(id));
        if needs {
            self.fetch_children(id)
        } else {
            if let Some(t) = &mut self.call_graph {
                t.toggle(id);
            }
            Task::none()
        }
    }

    pub(crate) fn on_toggle_split(&mut self) -> Task<Message> {
        // Panes are about to move or close under the cursor.
        self.invalidate_hover();
        if self.split {
            self.split = false;
            self.panes[1] = None;
            // A load still in flight for the closed pane must not resurrect
            // it as an invisible viewer.
            self.pane_pending[1] = None;
            self.active = 0;
        } else {
            self.split = true;
            // Duplicate the current file for side-by-side reading.
            self.panes[1] = self.panes[0].clone();
            self.active = 1;
        }
        Task::none()
    }

    pub(crate) fn on_svgs_generated(
        &mut self,
        generation: u64,
        map: HashMap<u64, richmd::PreparedSvg>,
    ) -> Task<Message> {
        // SVGs are keyed by content hash (and disk-cached), so inserting
        // is idempotent — accept them even from a superseded generation,
        // otherwise a concurrent `prepare_segments` bumping the counter
        // can strand a diagram as a perpetual placeholder.
        for (key, prepared) in map {
            self.insert_svg(key, prepared);
        }
        if generation == self.explain.svg_gen {
            self.status = "Rendered math & diagrams".into();
        }
        Task::none()
    }

    pub(crate) fn on_refresh_all(&mut self) -> Task<Message> {
        if !self.llm_available {
            self.status = format!("Add an API key in Settings ({})", llm::config_hint());
            return Task::done(Message::OpenSettings);
        }
        // Already refreshing — let it finish (the chip is disabled too).
        if self.explain.running || self.overview.generating || self.building_embeddings {
            return Task::none();
        }
        // Manual: bypass the 30s cooldown entirely.
        self.status = "Refreshing…".into();
        self.begin_refresh()
    }

    pub(crate) fn on_definition_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            Ok(targets) if !targets.is_empty() => {
                let t = &targets[0];
                let abs = t.path.clone();
                let target_line = t.line + 1;
                // Clear the "Looking up definition…" progress; the jump itself
                // is the feedback (otherwise the status stays stuck on it).
                self.status.clear();
                self.open_file(abs, Some(target_line), true)
            }
            Ok(_) => {
                self.status = "No definition found".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("Definition failed: {e}");
                Task::none()
            }
        }
    }

    pub(crate) fn on_references_result(
        &mut self,
        result: Result<Vec<lsp::client::Target>, String>,
    ) -> Task<Message> {
        match result {
            Ok(refs) if !refs.is_empty() => {
                self.status = format!("{} reference(s) — showing them in Search", refs.len());
                self.show_references(refs)
            }
            Ok(_) => {
                self.status = "No references".into();
                Task::none()
            }
            Err(e) => {
                self.status = format!("References failed: {e}");
                Task::none()
            }
        }
    }

    pub(crate) fn on_open_settings(&mut self) -> Task<Message> {
        let c = llm::Config::current_or_default();
        self.settings.provider = c.provider;
        self.settings.model = c.model;
        self.settings.base_url = c.base_url;
        // Pre-fill the key from the FILE, never from `c.api_key`, which may
        // have been resolved from the environment. Saving the form stores
        // whatever is in the field, and a stored key follows `base_url`
        // anywhere — so a pre-filled form let a user with `ANTHROPIC_API_KEY`
        // exported type a gateway URL and write their real provider secret
        // into config.toml next to it, reaching the exact outcome the endpoint
        // check in `env_key_for_endpoint` refuses.
        let stored = llm::Config::stored_key();
        self.settings.key_from_env = stored.is_empty() && !c.api_key.is_empty();
        self.settings.key = stored;
        let e = embed::Config::current_or_default();
        self.settings.embed_model = e.model;
        self.settings.embed_base_url = e.base_url;
        let embed_stored = embed::Config::stored_key();
        self.settings.embed_key_from_env = embed_stored.is_empty() && !e.api_key.is_empty();
        self.settings.embed_key = embed_stored;
        // What the form was pre-filled WITH, so Save can tell a field the user
        // edited from one they never touched. Built the same way Save builds
        // the config it writes, so an untouched form compares equal field by
        // field.
        self.settings.ai_snapshot = (
            llm::Config::from_parts(
                self.settings.provider,
                self.settings.key.clone(),
                self.settings.model.clone(),
                self.settings.base_url.clone(),
            ),
            embed::Config::from_parts(
                self.settings.embed_key.clone(),
                self.settings.embed_model.clone(),
                self.settings.embed_base_url.clone(),
            ),
        );
        // Capture the stored appearance so theme changes can preview live and
        // revert on Close-without-Save (see `restore_theme_snapshot`).
        self.settings.theme_snapshot = (
            self.theme_pref,
            theme::current_light().id,
            theme::current_dark().id,
        );
        self.settings.open = true;
        Task::none()
    }

    pub(crate) fn on_goto_line_requested(&mut self) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        self.finder.open = true;
        self.finder.mode = FinderMode::Files;
        self.finder.query = ":".to_string();
        self.refresh_finder();
        Task::batch([
            operation::focus(ui::finder_input_id()),
            operation::move_cursor_to_end(ui::finder_input_id()),
        ])
    }

    pub(crate) fn on_finder_confirm(&mut self) -> Task<Message> {
        if let Some(line) = self.finder.goto_line() {
            self.finder.open = false;
            if let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) {
                return self.open_file(abs, Some(line), true);
            }
            return Task::none();
        }
        match self.finder.results.get(self.finder.selected).copied() {
            Some(idx) => self.finder_open_index(idx),
            None => Task::none(),
        }
    }

    pub(crate) fn on_call_hierarchy_prepared(
        &mut self,
        token: u64,
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    ) -> Task<Message> {
        if items.is_empty() {
            self.status = "No call hierarchy for the symbol under the cursor".into();
            return Task::none();
        }
        // The new tree inherits the prepare request's token (already unique).
        self.call_graph = Some(callgraph::CallTree::new(token, direction, lang, items));
        self.sidebar = SidebarTab::Calls;
        // The tree is now the feedback; clear the transient "Building…"
        // status so it doesn't linger after results appear.
        self.status.clear();
        let roots = self.call_graph.as_ref().unwrap().roots().to_vec();
        Task::batch(
            roots
                .into_iter()
                .map(|r| self.fetch_children(r))
                .collect::<Vec<_>>(),
        )
    }

    pub(crate) fn on_walkthrough_step(&mut self, delta: i32) -> Task<Message> {
        let n = self
            .walk
            .open
            .and_then(|o| self.walk.library.get(o))
            .map(|w| w.steps.len())
            .unwrap_or(0);
        if n == 0 {
            return Task::none();
        }
        let i = (self.walk.step as i32 + delta).clamp(0, n as i32 - 1) as usize;
        self.walkthrough_goto(i)
    }

    pub(crate) fn on_toggle_breakpoint_from_menu(&mut self) -> Task<Message> {
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
        // menu.line is 0-based; breakpoints are 1-based.
        self.update(Message::BreakpointToggle {
            path: abs,
            line: menu.line + 1,
        })
    }

    pub(crate) fn on_time_travel_why_done(
        &mut self,
        sha: String,
        result: Result<String, String>,
    ) -> Task<Message> {
        if let Some(tt) = self.time_travel.as_mut() {
            tt.why_loading = false;
            match result {
                Ok(text) => {
                    tt.why.insert(sha, text.trim().to_string());
                }
                Err(e) => self.status = format!("Couldn't summarize: {e}"),
            }
        }
        Task::none()
    }

    /// (Project ownership is checked by the caller via `owns_result`.)
    pub(crate) fn on_stats_done(&mut self, rev: u64, report: stats::StatsReport) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        // The run FAILED (see `stats_done`: the message carries no error
        // channel, so a failure arrives stamped with the stale sentinel).
        // Commit nothing: any report already shown stays, the derived cache
        // keeps whatever it had, and leaving `rev` stale is what makes the
        // next entry into the view retry instead of trusting an empty answer.
        if rev == crate::app::server_ai::STATS_REV_STALE {
            self.stats.building = false;
            self.stats.rev = rev;
            self.status = "Couldn't compute code statistics".into();
            return Task::none();
        }
        self.stats.building = false;
        self.stats.rev = rev;
        if let Some(store) = &self.derived_dir {
            let _ = stats::save(
                store,
                &stats::Cached {
                    report: report.clone(),
                    rev,
                },
            );
        }
        self.stats.report = Some(report);
        self.status = "Code statistics ready".into();
        Task::none()
    }

    pub(crate) fn on_select_drag(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        if self.selecting
            && pane == self.active
            && let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut)
            && let Some((anchor, _)) = v.selection
        {
            let head = (line, col);
            v.selection = Some((anchor, head));
            v.caret = Some(head);
        }
        Task::none()
    }

    pub(crate) fn on_open_rel(&mut self, rel: String, line: Option<usize>) -> Task<Message> {
        let Some(project) = &self.project else {
            return Task::none();
        };
        // Defence in depth, not a live escape from repository content: every
        // caller today passes a rel that is already safe — the sidebar builds
        // its rels by walking the scanned tree, and bookmarks, notes and trail
        // history each `safe_rel`-filter their entries at load. What this
        // guards is the one source that is neither scan-built nor load-filtered
        // on the way in: on a REMOTE project the tree and the Docs index arrive
        // over the wire, so an absolute rel from a hostile or wrong server
        // would reach `join`, which DISCARDS the root for an absolute argument.
        // Purely lexical, so it is identical for local and remote and never
        // probes this machine's disk at a remote path.
        if !clew_core::statefile::safe_rel(&rel) {
            self.status = format!("Refused a path outside the project: {rel}");
            return Task::none();
        }
        let abs = project.root.join(&rel);
        // Cmd+click a file shows its explanation instead of opening it.
        if self.modifiers.command() {
            self.show_right_panel = true;
            return self.show_explanation(explain::Node::File(abs));
        }
        self.open_file(abs, line, true)
    }

    pub(crate) fn on_import_expand(&mut self, id: usize) -> Task<Message> {
        if let (Some(mut tree), Some(root)) = (
            self.import_tree.take(),
            self.project.as_ref().map(|p| p.root.clone()),
        ) {
            // Guard against a stale id from a since-rebuilt (smaller) tree.
            if id < tree.node_count() {
                tree.toggle(id, &self.import_graph, &root);
            }
            self.import_tree = Some(tree);
        }
        Task::none()
    }

    pub(crate) fn on_debug_stop(&mut self) -> Task<Message> {
        self.status = "Debugger stopped".into();
        // The teardown itself is shared with the project switch, which used to
        // carry its own copy of it (see `stop_debug_session`).
        self.stop_debug_session()
    }

    pub(crate) fn on_bp_condition_set(&mut self) -> Task<Message> {
        let Some((path, line, draft)) = self.debug.bp_cond_edit.take() else {
            return Task::none();
        };
        let cond = draft.trim();
        // The adapter's previous answer (if any) does not carry over: the
        // condition changes what we are asking for, and `push_breakpoints`
        // below asks again. Until that reply lands the state is "unknown".
        let bp = Bp {
            condition: (!cond.is_empty()).then(|| cond.to_string()),
            ..Bp::default()
        };
        self.debug
            .breakpoints
            .entry(path.clone())
            .or_default()
            .insert(line, bp);
        self.status = "Conditional breakpoint set".into();
        self.push_breakpoints(&path)
    }

    pub(crate) fn on_toggle_ask(&mut self) -> Task<Message> {
        // Toolbar "Ask": open the bottom panel on the Ask tab, or collapse
        // it if Ask is already the shown tab.
        if self.show_bottom && self.bottom_tab == BottomTab::Ask {
            self.show_bottom = false;
        } else {
            self.show_bottom = true;
            self.bottom_tab = BottomTab::Ask;
        }
        Task::none()
    }

    pub(crate) fn on_time_travel_select_drag(&mut self, line: usize, col: usize) -> Task<Message> {
        if self.selecting
            && let Some(v) = self.time_travel.as_mut().and_then(|t| t.viewer.as_mut())
            && let Some((anchor, _)) = v.selection
        {
            let head = (line, col);
            v.selection = Some((anchor, head));
            v.caret = Some(head);
        }
        Task::none()
    }

    pub(crate) fn on_time_travel_scrolled(
        &mut self,
        viewport: scrollable::Viewport,
    ) -> Task<Message> {
        // Only track real scrolls once the revision is loaded; the loading
        // fallback view mounts at offset 0 and would otherwise clobber the
        // carried entry scroll before the step applies it.
        if let Some(tt) = self.time_travel.as_mut()
            && tt.viewer.is_some()
        {
            tt.scroll_y = viewport.absolute_offset().y;
        }
        Task::none()
    }

    pub(crate) fn on_remote_open_here(&mut self) -> Task<Message> {
        let cwd = match self.connect.as_ref().map(|u| &u.stage) {
            Some(ConnectStage::Browsing(b)) => Some(b.cwd.clone()),
            _ => None,
        };
        if let Some(cwd) = cwd {
            self.connect = None;
            return self.start_scan(PathBuf::from(cwd));
        }
        Task::none()
    }

    pub(crate) fn on_outline_jump(&mut self, line: usize) -> Task<Message> {
        let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) else {
            return Task::none();
        };
        // Cmd+click an outline symbol explains it; a plain click jumps.
        if self.modifiers.command() {
            self.explain_symbol_at(abs, line)
        } else {
            self.open_file(abs, Some(line), true)
        }
    }

    pub(crate) fn on_explain_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.context_menu.take() else {
            return Task::none();
        };
        let file = self
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .map(|v| v.abs.clone());
        match file {
            // menu.line is 0-based; explain_symbol_at wants 1-based.
            Some(file) => self.explain_symbol_at(file, menu.line + 1),
            None => Task::none(),
        }
    }

    pub(crate) fn on_debug_watch_remove(&mut self, i: usize) -> Task<Message> {
        if i < self.debug.watches.len() {
            self.debug.watches.remove(i);
        }
        if let Some(s) = self.debug.session.as_mut()
            && i < s.watches.len()
        {
            s.watches.remove(i);
        }
        Task::none()
    }

    pub(crate) fn on_bookmark_removed(&mut self, idx: usize) -> Task<Message> {
        // Checked before the branch: with no project open the remote arm
        // would write this window's leftover list into whichever project the
        // server has, replacing its bookmarks with ours.
        if self.project.is_none() {
            return Task::none();
        }
        let Some(gone) = self.bookmarks.get(idx).cloned() else {
            return Task::none();
        };
        if !self.local_project_state() {
            // Identity, not index, remotely too: `idx` points into this
            // window's snapshot, and the server removes from a file another
            // client may have inserted into (bookmarks are kept sorted), which
            // shifts every entry after the insertion point.
            self.bookmarks.remove(idx);
            self.edit_remote_state(
                bookmarks::REL,
                bookmarks::merge_remove(&gone.rel, gone.line),
            );
        } else if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            // Identity, not index: `idx` points into this window's snapshot,
            // and the merge below re-reads a list another window may have
            // inserted into (bookmarks are kept sorted), which shifts every
            // entry after the insertion point.
            let (merged, saved) = bookmarks::edit(&root, |list| {
                list.retain(|b| !(b.rel == gone.rel && b.line == gone.line))
            });
            // Adopted on failure too: the removal is the user's own action, so
            // the list stays as they left it for the session. Only the write
            // is lost, and the status line says so rather than the entry
            // silently reappearing on the next render.
            self.bookmarks = merged;
            if let Err(e) = saved {
                self.status =
                    format!("Cannot write .clew/bookmarks.json: {e} — removed for this session");
            }
        }
        Task::none()
    }

    pub(crate) fn on_bookmark_note_save(&mut self) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        if let Some((rel, line, draft)) = self.note_edit.take() {
            if !self.local_project_state() {
                // Only the note field travels, so a bookmark another client
                // added — and everything else in the file — survives the save.
                let merge = bookmarks::merge_note(&rel, line, Some(draft.clone()));
                bookmarks::set_note(&mut self.bookmarks, &rel, line, Some(draft));
                self.edit_remote_state(bookmarks::REL, merge);
            } else if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
                // The note is attached on the list read here, so it survives
                // another window's edits instead of being written back as part
                // of this window's whole (stale) snapshot.
                let (merged, saved) = bookmarks::edit(&root, |list| {
                    bookmarks::set_note(list, &rel, line, Some(draft))
                });
                // Adopted whether or not the write landed: `note_edit` was
                // taken above, so the typed note lives ONLY in the merged
                // list. Dropping it on an unwritable `.clew/` deleted what the
                // user had just written, with the editor already closed.
                //
                // Not fully closed: the note is attached to the list just READ
                // from disk, so on a store that cannot be read either (`.clew`
                // shipped as a symlink, or replaced by a file) a bookmark that
                // only ever existed in this window's memory is not there to
                // attach it to, and the text is still lost. Fixing that means
                // merging in memory, which cannot tell a bookmark this window
                // deleted from one another window just added.
                self.bookmarks = merged;
                if let Err(e) = saved {
                    self.status = format!(
                        "Cannot write .clew/bookmarks.json: {e} — kept for this session, not saved"
                    );
                }
            }
        }
        Task::none()
    }

    pub(crate) fn on_ask_delta(&mut self, stream: u64, text: String) -> Task<Message> {
        // Route by stream id. "The last streaming turn" also matches a turn
        // belonging to another question — or, after a project switch, to a
        // conversation in another project entirely.
        let Some(turn) = self
            .ask_turns
            .iter_mut()
            .find(|t| t.stream == stream && t.streaming)
        else {
            return Task::none();
        };
        turn.answer_md.push_str(&text);
        // First token(s): the answer is streaming, not "thinking".
        self.asking = false;
        // Follow the growing answer.
        operation::scroll_to(
            ui::ask_scroll_id(),
            AbsoluteOffset {
                x: 0.0,
                y: f32::MAX,
            },
        )
    }

    pub(crate) fn on_finder_opened(&mut self, mode: FinderMode) -> Task<Message> {
        if self.project.is_none() {
            return Task::none();
        }
        self.finder.open = true;
        self.finder.mode = mode;
        self.finder.query.clear();
        self.code_focused = false; // the finder input takes focus
        self.refresh_finder();
        operation::focus(ui::finder_input_id())
    }

    pub(crate) fn on_walkthrough_regenerate(&mut self, i: usize) -> Task<Message> {
        let Some(scope) = self.walk.library.get(i).map(|w| w.scope.clone()) else {
            return Task::none();
        };
        // A change-review tour re-runs the diff; a normal tour re-generates.
        if scope.starts_with("@diff") {
            return Task::done(Message::GenerateDiffWalkthrough);
        }
        Task::done(Message::GenerateWalkthrough(scope))
    }

    /// The document under the cursor changed, so every hover in flight is
    /// about a file that is no longer there. Drops the peek and bumps the
    /// generation so a late `HoverResult` is recognized as stale.
    ///
    /// Unlike [`Self::on_hover_cleared`] this ignores `hover_pinned` and
    /// clears it: the tooltip widget only exists while `hover` is `Some`, so
    /// a pin left set could never be released by the mouse leaving it, and
    /// would suppress every later hover.
    pub(crate) fn invalidate_hover(&mut self) {
        self.hover = None;
        self.hover_pinned = false;
        self.hover_gen = self.hover_gen.wrapping_add(1);
    }

    pub(crate) fn on_hover_cleared(&mut self) -> Task<Message> {
        // Cursor left the code area — drop the peek and cancel any pending
        // dwell (a stale HoverDwell will see the bumped gen and no-op).
        // But not if it's inside the tooltip (which overlaps the code).
        if !self.hover_pinned {
            self.hover = None;
            self.hover_gen = self.hover_gen.wrapping_add(1);
        }
        Task::none()
    }

    pub(crate) fn on_git_info_loaded(
        &mut self,
        abs: PathBuf,
        info: Option<Arc<git::GitInfo>>,
    ) -> Task<Message> {
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                v.git = info.clone();
            }
        }
        Task::none()
    }

    pub(crate) fn on_call_hierarchy_expand_all(&mut self) -> Task<Message> {
        let frontier = match &mut self.call_graph {
            Some(t) => {
                t.full = true;
                t.unfetched_frontier()
            }
            None => return Task::none(),
        };
        Task::batch(
            frontier
                .into_iter()
                .map(|id| self.fetch_children(id))
                .collect::<Vec<_>>(),
        )
    }

    pub(crate) fn on_breakpoint_toggle(&mut self, path: PathBuf, line: usize) -> Task<Message> {
        let map = self.debug.breakpoints.entry(path.clone()).or_default();
        if map.remove(&line).is_none() {
            map.insert(line, Bp::default());
        }
        if map.is_empty() {
            self.debug.breakpoints.remove(&path);
        }
        self.push_breakpoints(&path)
    }

    pub(crate) fn on_bookmark_note_edit(&mut self, rel: String, line: usize) -> Task<Message> {
        let existing = self
            .bookmarks
            .iter()
            .find(|b| b.rel == rel && b.line == line)
            .and_then(|b| b.note.clone())
            .unwrap_or_default();
        self.note_edit = Some((rel, line, existing));
        operation::focus(ui::note_input_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One indexed unit, with a vector that makes it the obvious hit for
    /// anything (so a stale index answering a query would be conspicuous).
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

    /// Saving Settings is the one moment the embedding space can move under a
    /// session, and the vectors held in memory do not move with it. Keeping
    /// them is the silent failure: FIND embeds the query at the NEW endpoint
    /// and ranks it against the OLD space's vectors, and "Build index" reuses
    /// them (its gate is the summary hash, which a config change does not
    /// move) and then writes the mix stamped with the new space, at which
    /// point `embed::load_for` accepts the file for good.
    ///
    /// A repoint of the SAME model name at another provider is the case this
    /// has to catch: it is a different space, it is exactly why the on-disk
    /// index records the endpoint, and it is invisible to every check that
    /// compares model names.
    #[test]
    fn a_settings_save_that_moves_the_embedding_space_drops_the_in_memory_index() {
        const OPENAI: &str = "https://api.openai.com/v1";
        let dir = std::env::temp_dir().join("clew-embed-space-settings-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // `App::blank()` reads `trust.toml` / `connections.toml` through the
        // data dir, and this test writes the config file the handler saves to,
        // so isolate both (holding the env lock for the whole test).
        let _env = clew_core::env_lock();
        // SAFETY: env mutation is serialized by the lock held above.
        unsafe { std::env::set_var("CLEW_DATA_DIR", &dir) };
        std::fs::write(
            dir.join("config.toml"),
            format!("[embedding]\napi_key = \"sk\"\nmodel = \"m\"\nbase_url = \"{OPENAI}\"\n"),
        )
        .unwrap();

        let mut app = App::blank();
        app.embed_index = embed::Index {
            model: "m".into(),
            base_url: OPENAI.into(),
            entries: vec![entry("f")],
        };
        app.semantic_results = vec![(entry("f").node, 0.9)];
        // The modal as it opened on that stored config.
        let opened_on = embed::Config::from_parts("sk".into(), "m".into(), OPENAI.into());
        app.settings.ai_snapshot.1 = opened_on.clone();
        app.settings.embed_key = "sk".into();
        app.settings.embed_model = "m".into();
        app.settings.embed_base_url = OPENAI.into();

        // Save is also the only way to commit a theme change, so a save that
        // leaves the space alone must not cost the user a full re-embed.
        let _ = app.on_settings_saved();
        assert_eq!(
            app.embed_index.entries.len(),
            1,
            "an unchanged embedding config forced a rebuild"
        );

        // Same model, another provider serving it: a different space.
        app.settings.ai_snapshot.1 = opened_on;
        app.settings.embed_base_url = "http://localhost:1234/v1".into();
        let _ = app.on_settings_saved();
        assert!(
            app.embed_index.entries.is_empty(),
            "old-space vectors survived the repoint: {} left",
            app.embed_index.entries.len()
        );
        assert!(
            app.semantic_results.is_empty(),
            "results ranked in the old space stayed on screen"
        );
        assert!(
            app.status.contains("rebuild it"),
            "the drop was not explained: {}",
            app.status
        );

        // SAFETY: same lock, still held.
        unsafe { std::env::remove_var("CLEW_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
