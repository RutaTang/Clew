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
                // Build the API docs the first time the tab is opened.
                if self.docs.files.is_empty() && !self.docs.loading {
                    self.request_docs();
                }
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
        Task::perform(
            async move {
                let file = abs.clone();
                let lines = tokio::task::spawn_blocking(move || {
                    git::diff_lines(&root, &file).unwrap_or_default()
                })
                .await
                .unwrap_or_default();
                (abs, rel, lines)
            },
            |(abs, rel, lines)| Message::DiffLoaded { abs, rel, lines },
        )
    }

    pub(crate) fn on_open_link(&mut self, url: String) -> Task<Message> {
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
        let saved = cfg.save().and_then(|()| emb.save());
        match saved {
            Ok(()) => {
                self.llm_available = llm::Config::available();
                self.embed_available = embed::Config::available();
                self.settings.open = false;
                self.status = if self.llm_available {
                    format!("Settings saved ({})", cfg.provider.label())
                } else {
                    "Saved — add an API key to enable Explain".into()
                };
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
        // Docs are built but hold no entry for this symbol (e.g. an
        // undocumented private item): rather than silently doing nothing,
        // fall back to its definition so "View docs" always lands the
        // reader somewhere useful.
        if !self.docs.files.is_empty() && find_doc_by_name(&self.docs.files, &word).is_none() {
            self.status = format!("No doc entry for “{word}” — showing its definition");
            return self.goto_definition(menu.pane, menu.line, menu.col);
        }
        self.view_docs_for(&word);
        Task::none()
    }

    pub(crate) fn on_highlighted(
        &mut self,
        abs: PathBuf,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: HashMap<usize, String>,
        inactive: HashSet<usize>,
    ) -> Task<Message> {
        let lines = Arc::new(lines);
        for slot in &mut self.panes {
            if let Some(v) = slot
                && v.abs == abs
                && v.lines.len() == lines.len()
            {
                v.set_lines(lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                v.inactive_lines = inactive.clone();
                v.highlighted = true;
            }
        }
        // Symbols just landed — resolve the function under the caret.
        self.follow_caret(Task::none())
    }

    pub(crate) fn on_walkthrough_delete(&mut self, i: usize) -> Task<Message> {
        if i >= self.walk.library.len() {
            return Task::none();
        }
        self.walk.library.remove(i);
        // Keep the open index pointing at the same tour (or clear it when
        // the open one was removed).
        match self.walk.open {
            Some(o) if o == i => {
                self.walk.open = None;
                self.walk.prepared = Vec::new();
            }
            Some(o) if o > i => self.walk.open = Some(o - 1),
            _ => {}
        }
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = walkthrough::save_library(&root, &self.walk.library)
        {
            self.status = format!("Could not save walkthrough: {e}");
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
        let added = bookmarks::toggle(&mut self.bookmarks, &rel, line, preview);
        self.status = match bookmarks::save(&root, &self.bookmarks) {
            Ok(()) if added => format!("Bookmarked {rel}:{line}"),
            Ok(()) => format!("Removed bookmark {rel}:{line}"),
            Err(e) => format!("Cannot write .clew/bookmarks.json: {e}"),
        };
        Task::none()
    }

    pub(crate) fn on_tree_updated(&mut self, result: ScanResult) -> Task<Message> {
        // Only apply to the current project (a stale rescan from a
        // previous root is ignored).
        let current = self.project.as_ref().map(|p| p.root.clone());
        if current.as_deref() == Some(result.root.as_path()) {
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
        if let Some(v) = self.active_viewer() {
            let lines = v.lines.clone();
            self.find.recompute(&lines);
        }
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
            // Clear so the hints disappear immediately.
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
        let root_items = tree
            .roots()
            .iter()
            .map(|&r| tree.node(r).item.clone())
            .collect();
        let mut rebuilt = callgraph::CallTree::new(toggled, lang, root_items);
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
        title: String,
        commits: Vec<(String, String)>,
        result: Result<String, String>,
    ) -> Task<Message> {
        // Ignore a late answer if the user already closed the popup.
        if self.blame_why.is_none() {
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

    pub(crate) fn on_embeddings_built(
        &mut self,
        root: PathBuf,
        result: Result<embed::Index, String>,
    ) -> Task<Message> {
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.building_embeddings = false;
        match result {
            Ok(index) => {
                let _ = embed::save(&root, &index);
                self.status = format!("Semantic index ready ({} items)", index.entries.len());
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
        // Consent is recorded by the .clew directory itself.
        match std::fs::create_dir_all(root.join(".clew")) {
            Ok(()) => self.start_scan(root),
            Err(e) => {
                self.pending_open = None;
                self.status = format!("Cannot open project: .clew is not writable ({e})");
                Task::none()
            }
        }
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
        if self.split {
            self.split = false;
            self.panes[1] = None;
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
        self.settings.key = c.api_key;
        self.settings.model = c.model;
        self.settings.base_url = c.base_url;
        let e = embed::Config::current_or_default();
        self.settings.embed_key = e.api_key;
        self.settings.embed_model = e.model;
        self.settings.embed_base_url = e.base_url;
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
        direction: callgraph::Direction,
        lang: &'static str,
        items: Vec<lsp::client::CallItem>,
    ) -> Task<Message> {
        if items.is_empty() {
            self.status = "No call hierarchy for the symbol under the cursor".into();
            return Task::none();
        }
        self.call_graph = Some(callgraph::CallTree::new(direction, lang, items));
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

    pub(crate) fn on_stats_done(
        &mut self,
        root: PathBuf,
        rev: u64,
        report: stats::StatsReport,
    ) -> Task<Message> {
        // Drop a result from a project the user already switched away from.
        if self.project.as_ref().map(|p| &p.root) != Some(&root) {
            return Task::none();
        }
        self.stats.building = false;
        self.stats.rev = rev;
        let _ = stats::save(
            &root,
            &stats::Cached {
                report: report.clone(),
                rev,
            },
        );
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
        match self.debug.session.take().and_then(|s| s.client) {
            Some(client) => Task::perform(
                async move {
                    let _ = client.disconnect().await;
                },
                |()| Message::Noop,
            ),
            None => Task::none(),
        }
    }

    pub(crate) fn on_bp_condition_set(&mut self) -> Task<Message> {
        let Some((path, line, draft)) = self.debug.bp_cond_edit.take() else {
            return Task::none();
        };
        let cond = draft.trim();
        let bp = Bp {
            condition: (!cond.is_empty()).then(|| cond.to_string()),
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
        if idx < self.bookmarks.len() {
            self.bookmarks.remove(idx);
            if let Some(p) = &self.project
                && let Err(e) = bookmarks::save(&p.root, &self.bookmarks)
            {
                self.status = format!("Cannot write .clew/bookmarks.json: {e}");
            }
        }
        Task::none()
    }

    pub(crate) fn on_bookmark_note_save(&mut self) -> Task<Message> {
        if let Some((rel, line, draft)) = self.note_edit.take() {
            bookmarks::set_note(&mut self.bookmarks, &rel, line, Some(draft));
            if let Some(p) = &self.project
                && let Err(e) = bookmarks::save(&p.root, &self.bookmarks)
            {
                self.status = format!("Cannot write .clew/bookmarks.json: {e}");
            }
        }
        Task::none()
    }

    pub(crate) fn on_ask_delta(&mut self, text: String) -> Task<Message> {
        // First token(s): the answer is streaming, not "thinking".
        self.asking = false;
        if let Some(turn) = self.ask_turns.last_mut()
            && turn.streaming
        {
            turn.answer_md.push_str(&text);
        }
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
