//! The central update() dispatcher: routes each Message to its on_* handler.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        // Any action picked from the toolbar "More" menu dismisses it.
        if self.show_tools_menu
            && matches!(
                message,
                Message::OpenFolderPressed
                    | Message::ToggleServerPanel
                    | Message::ToggleDiff
                    | Message::ExplainProject
                    | Message::OpenSettings
            )
        {
            self.show_tools_menu = false;
        }
        match message {
            Message::OpenFolderPressed => Task::perform(pick_folder(), Message::FolderPicked),
            Message::FolderPicked(None) => Task::none(),
            Message::FolderPicked(Some(root)) => self.request_open(root),
            Message::ConsentDenied => {
                self.pending_consent = None;
                self.pending_open = None;
                self.status = "Project not opened: creating .clew was not allowed".to_string();
                Task::none()
            }
            Message::ConsentAllowed => self.on_consent_allowed(),
            Message::ScanDone(result) => self.on_scan_done(result),
            Message::TreeUpdated(result) => self.on_tree_updated(result),
            Message::SymbolIndexDone { root, indexed } => self.on_symbol_index_done(root, indexed),
            Message::StructureBuilt(index) => {
                self.structure = index;
                Task::none()
            }
            Message::InlayHintsLoaded { abs, hints } => self.on_inlay_hints_loaded(abs, hints),
            Message::ToggleDir(rel) => self.on_toggle_dir(rel),
            Message::OpenRel { rel, line } => self.on_open_rel(rel, line),
            Message::OpenAbs { abs, line, push } => self.open_file(abs, line, push),
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            } => self.on_file_loaded(pane, abs, target, result),
            Message::Highlighted {
                abs,
                lines,
                symbols,
                docs,
                inactive,
            } => self.on_highlighted(abs, lines, symbols, docs, inactive),
            Message::GitInfoLoaded { abs, info } => self.on_git_info_loaded(abs, info),
            Message::FilesChanged(paths) => self.on_files_changed(paths),
            Message::FilesRehashed {
                events,
                fs_structural,
            } => self.on_files_rehashed(events, fs_structural),
            Message::CodeScrolled(pane, viewport) => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.scroll_y = viewport.absolute_offset().y;
                    v.viewport_h = viewport.bounds().height;
                }
                Task::none()
            }
            Message::PaneFocused(pane) => {
                if pane == 0 || self.split {
                    self.active = pane;
                    // The Imports tab follows the focused pane's file.
                    self.refresh_import_tree();
                }
                Task::none()
            }
            Message::ToggleSplit => self.on_toggle_split(),
            Message::SelectStart { pane, line, col } => self.on_select_start(pane, line, col),
            Message::SelectDrag { pane, line, col } => self.on_select_drag(pane, line, col),
            Message::SelectEnd => {
                self.selecting = false;
                Task::none()
            }
            Message::FoldToggle { pane, line } => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.toggle_fold(line);
                }
                Task::none()
            }
            Message::MinimapScrolled { pane, fraction } => self.on_minimap_scrolled(pane, fraction),
            Message::CopySelection => self.on_copy_selection(),
            Message::SidebarTabPicked(tab) => self.on_sidebar_tab_picked(tab),
            Message::SearchQueryChanged(query) => {
                self.search.query = query;
                Task::none()
            }
            Message::SearchToggle(opt) => {
                match opt {
                    SearchOpt::Regex => self.search.regex = !self.search.regex,
                    SearchOpt::Case => self.search.case_sensitive = !self.search.case_sensitive,
                    SearchOpt::WholeWord => self.search.whole_word = !self.search.whole_word,
                }
                // Re-run live so the effect of the toggle is immediate.
                self.run_search()
            }
            Message::SearchIncludeChanged(s) => {
                self.search.include = s;
                Task::none()
            }
            Message::SearchExcludeChanged(s) => {
                self.search.exclude = s;
                Task::none()
            }
            Message::SearchSubmitted => self.run_search(),
            Message::SearchDone { result } => {
                self.apply_search_result(result);
                Task::none()
            }
            Message::FinderOpened(mode) => self.on_finder_opened(mode),
            Message::FinderClosed => {
                self.finder.open = false;
                self.code_focused = true; // back to reading
                Task::none()
            }
            Message::FinderQueryChanged(query) => {
                self.finder.query = query;
                self.refresh_finder();
                Task::none()
            }
            Message::FinderPick(idx) => self.finder_open_index(idx),
            Message::FinderConfirm => self.on_finder_confirm(),
            Message::GotoLineRequested => self.on_goto_line_requested(),
            Message::BookmarkToggled => self.on_bookmark_toggled(),
            Message::BookmarkRemoved(idx) => self.on_bookmark_removed(idx),
            Message::BookmarkNoteEdit(rel, line) => self.on_bookmark_note_edit(rel, line),
            Message::BookmarkNoteInput(s) => {
                if let Some((_, _, draft)) = &mut self.note_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::BookmarkNoteSave => self.on_bookmark_note_save(),
            Message::BookmarkNoteCancel => {
                self.note_edit = None;
                Task::none()
            }
            Message::NoteToggleUnderstood { rel, symbol } => {
                notes::toggle_understood(&mut self.notes, &rel, &symbol);
                self.save_notes();
                Task::none()
            }
            Message::NoteEditStart { rel, symbol } => {
                let existing = notes::find(&self.notes, &rel, &symbol)
                    .map(|n| n.text.clone())
                    .unwrap_or_default();
                self.reading_note_edit = Some((rel, symbol, existing));
                operation::focus(ui::note_input_id())
            }
            Message::NoteEditInput(s) => {
                if let Some((_, _, draft)) = &mut self.reading_note_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::NoteEditSave => {
                if let Some((rel, symbol, draft)) = self.reading_note_edit.take() {
                    notes::set_text(&mut self.notes, &rel, &symbol, &draft);
                    self.save_notes();
                }
                Task::none()
            }
            Message::NoteEditCancel => {
                self.reading_note_edit = None;
                Task::none()
            }
            Message::NoteRemove { rel, symbol } => {
                notes::remove(&mut self.notes, &rel, &symbol);
                self.save_notes();
                Task::none()
            }
            Message::NoteJump { rel, symbol } => {
                let Some(root) = self.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let line = self.note_symbol_line(&rel, &symbol);
                self.open_file(root.join(&rel), line, true)
            }
            Message::GoBack => match self.history.back() {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::GoForward => match self.history.forward() {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::HistoryJump(id) => match self.history.goto(id) {
                Some(loc) => {
                    self.save_history();
                    self.open_file(loc.path, loc.line, false)
                }
                None => Task::none(),
            },
            Message::TrailToggleCollapse(id) => {
                if !self.trail_collapsed.remove(&id) {
                    self.trail_collapsed.insert(id);
                }
                Task::none()
            }
            Message::HistoryClear => {
                self.history.clear();
                self.save_history();
                Task::none()
            }
            Message::ToggleLeftSidebar => {
                self.show_left_sidebar = !self.show_left_sidebar;
                Task::none()
            }
            Message::ToggleRightPanel => {
                self.show_right_panel = !self.show_right_panel;
                Task::none()
            }
            Message::ToggleDiff => self.on_toggle_diff(),
            Message::DiffLoaded { abs, rel, lines } => {
                self.diff = Some(DiffState { abs, rel, lines });
                Task::none()
            }
            Message::OutlineJump(line) => self.on_outline_jump(line),
            Message::FontSizeDelta(delta) => {
                let old = self.line_height();
                self.font_size = (self.font_size + delta).clamp(9.0, 22.0);
                self.rescale_scroll(old)
            }
            Message::FontSizeReset => {
                let old = self.line_height();
                self.font_size = DEFAULT_FONT_SIZE;
                self.rescale_scroll(old)
            }
            Message::KeyPressed(key, modifiers) => self.handle_key(key, modifiers),
            Message::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                if !modifiers.command() {
                    self.hover = None; // hover is a Cmd-hover affordance
                }
                Task::none()
            }
            // Drag *this* window — not `window::latest()`, which would drag the
            // most-recently-opened window no matter which one you grabbed.
            Message::TitleBarDragged => match self.main_window {
                Some(id) => iced::window::drag(id),
                None => Task::none(),
            },
            Message::WindowFocusChanged(focused) => {
                self.window_focused = focused;
                // When following the system appearance, re-check on focus — the
                // user may have flipped the OS theme while clew was in the
                // background. Only re-color if it actually changed.
                if focused && self.theme_pref == theme::ThemePref::System {
                    let was_light = theme::is_light();
                    theme::apply_pref(theme::ThemePref::System);
                    if theme::is_light() != was_light {
                        self.restyle_svgs();
                    }
                }
                Task::none()
            }
            Message::ControlsHover(over) => {
                self.controls_hovered = over;
                Task::none()
            }
            // These act on *this* App's own window (not `window::latest()`, which
            // is wrong once there are several windows).
            Message::CloseWindow => match self.main_window {
                Some(id) => iced::window::close(id),
                None => Task::none(),
            },
            Message::MinimizeWindow => {
                // A frameless window can't be minimized via winit (no
                // miniaturizable style mask); minimize its NSWindow directly.
                #[cfg(target_os = "macos")]
                macos::minimize_key_window();
                #[cfg(not(target_os = "macos"))]
                if let Some(id) = self.main_window {
                    return iced::window::minimize(id, true);
                }
                Task::none()
            }
            Message::ToggleFullscreen => {
                self.fullscreen = !self.fullscreen;
                let mode = if self.fullscreen {
                    iced::window::Mode::Fullscreen
                } else {
                    iced::window::Mode::Windowed
                };
                match self.main_window {
                    Some(id) => iced::window::set_mode(id, mode),
                    None => Task::none(),
                }
            }
            Message::WindowOpened => {
                // Frameless windows lose the OS rounded corners; restore them,
                // and install the native menu bar (both main-thread, once).
                #[cfg(target_os = "macos")]
                {
                    macos::round_corners(10.0);
                    macos::menu::install_once();
                    macos::appearance::install_once();
                }
                Task::none()
            }
            Message::WindowResized(size) => self.on_window_resized(size),
            Message::ResizeSidebar(x) => {
                self.sidebar_width = x;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::ResizeRight(x) => {
                self.right_width = self.window_width - x;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::ResizeBottom(y) => {
                self.bottom_height = self.window_height - y;
                self.clamp_panel_sizes();
                Task::none()
            }
            Message::LspStartResult { language, result } => match result {
                Ok(client) => {
                    // Open every already-loaded document of this language.
                    let open_task = self.open_docs_for_language(&language, &client);
                    self.lsp.insert(language, LspSlot::Ready(client));
                    open_task
                }
                Err(e) => {
                    self.status = format!("{language} server failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::LspConsentDismissed => {
                if let Some(c) = self.pending_lsp_consent.take() {
                    self.lsp.insert(
                        c.language,
                        LspSlot::Unsupported("server download declined".into()),
                    );
                }
                Task::none()
            }
            Message::LspConsentAllowed => self.on_lsp_consent_allowed(),
            Message::LspDownloadResult { language, result } => match result {
                Ok(exe) => {
                    self.status = format!("{language} server installed");
                    self.start_lsp_with(&language, exe)
                }
                Err(e) => {
                    self.status = format!("{language} server download failed: {e}");
                    self.lsp.insert(language, LspSlot::Failed(e));
                    Task::none()
                }
            },
            Message::GotoDefinition { pane, line, col } => self.goto_definition(pane, line, col),
            Message::ContextMenuOpened {
                pane,
                line,
                col,
                x,
                y,
            } => self.on_context_menu_opened(pane, line, col, x, y),
            Message::ContextMenuClosed => {
                self.context_menu = None;
                Task::none()
            }
            Message::ContextGoto(kind) => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.goto_request(menu.pane, menu.line, menu.col, kind)
            }
            Message::FindOpened => self.on_find_opened(),
            Message::FindQueryChanged(q) => {
                self.find.query = q;
                if let Some(v) = self.active_viewer() {
                    let lines = v.lines.clone();
                    self.find.recompute(&lines);
                }
                self.jump_to_find_match()
            }
            Message::FindStep(delta) => {
                self.find.step(delta);
                self.jump_to_find_match()
            }
            Message::FindClosed => {
                self.find.open = false;
                self.code_focused = true;
                Task::none()
            }
            Message::HoverRequested {
                pane,
                line,
                col,
                x,
                y,
            } => self.on_hover_requested(pane, line, col, x, y),
            Message::HoverDwell {
                epoch,
                pane,
                line,
                col,
                x,
                y,
            } => self.on_hover_dwell(epoch, pane, line, col, x, y),
            Message::HoverResult { line, col, text } => {
                if let Some(h) = &mut self.hover
                    && h.line == line
                    && h.col == col
                {
                    h.text = text;
                }
                Task::none()
            }
            Message::HoverCleared => self.on_hover_cleared(),
            Message::HoverPin(inside) => {
                self.hover_pinned = inside;
                if !inside {
                    // Left the tooltip: dismiss it and cancel any pending dwell.
                    self.hover = None;
                    self.hover_gen = self.hover_gen.wrapping_add(1);
                }
                Task::none()
            }
            Message::DefinitionResult { result } => self.on_definition_result(result),
            Message::ReferencesResult { result } => self.on_references_result(result),
            Message::CallHierarchyRequested => {
                let pane = self.active;
                let Some((line, col)) = self.active_viewer().and_then(|v| v.caret) else {
                    return Task::none();
                };
                self.call_hierarchy_at(pane, line, col)
            }
            Message::CallHierarchyFromMenu => {
                let Some(menu) = self.context_menu.take() else {
                    return Task::none();
                };
                self.call_hierarchy_at(menu.pane, menu.line, menu.col)
            }
            Message::ExplainFromMenu => self.on_explain_from_menu(),
            Message::CallHierarchyPrepared {
                direction,
                lang,
                items,
            } => self.on_call_hierarchy_prepared(direction, lang, items),
            Message::CallHierarchyExpand(id) => self.on_call_hierarchy_expand(id),
            Message::CallHierarchyChildren { id, items } => {
                self.on_call_hierarchy_children(id, items)
            }
            Message::CallHierarchyDirection => self.on_call_hierarchy_direction(),
            Message::CallHierarchyExpandAll => self.on_call_hierarchy_expand_all(),
            Message::ImportExpand(id) => self.on_import_expand(id),
            Message::ImportDirection => {
                self.import_dir = self.import_dir.toggled();
                // A direction flip resets the (now meaningless) expand-all state.
                if let Some(t) = &mut self.import_tree {
                    t.full = false;
                }
                self.refresh_import_tree();
                Task::none()
            }
            Message::ImportExpandAll => {
                if let (Some(mut tree), Some(root)) = (
                    self.import_tree.take(),
                    self.project.as_ref().map(|p| p.root.clone()),
                ) {
                    tree.expand_all(&self.import_graph, &root);
                    self.import_tree = Some(tree);
                }
                Task::none()
            }
            Message::OpenOverlay(which) => self.on_open_overlay(which),
            Message::OverlayViewToggle => {
                self.graph_mode = !self.graph_mode;
                if self.graph_mode {
                    self.refresh_graph_layout();
                }
                Task::none()
            }
            Message::GraphToggle3D => {
                self.graph_3d = !self.graph_3d;
                Task::none()
            }
            Message::GraphToggleSpin => {
                self.graph_spin = !self.graph_spin;
                Task::none()
            }
            Message::SetThemePref(pref) => {
                self.set_theme(pref);
                Task::none()
            }
            Message::CycleTheme => {
                let next = match self.theme_pref {
                    theme::ThemePref::Dark => theme::ThemePref::Light,
                    theme::ThemePref::Light => theme::ThemePref::System,
                    theme::ThemePref::System => theme::ThemePref::Dark,
                };
                self.set_theme(next);
                Task::none()
            }
            Message::SystemAppearanceChanged => {
                // Only follow the OS when the preference is System; re-color
                // cached diagrams if the resolved appearance actually flipped.
                if self.theme_pref == theme::ThemePref::System {
                    let was_light = theme::is_light();
                    theme::apply_pref(theme::ThemePref::System);
                    if theme::is_light() != was_light {
                        self.restyle_svgs();
                    }
                }
                Task::none()
            }
            Message::CloseOverlay => {
                self.overlay = None;
                Task::none()
            }
            Message::OverlayOpenImports(path) => {
                self.overlay = None;
                self.sidebar = SidebarTab::Imports;
                self.open_file(path, None, true)
            }
            Message::OverlayOpenAt { abs, line } => {
                self.overlay = None;
                self.open_file(abs, Some(line), true)
            }
            Message::ProjectCallsBuilt { root, graph } => self.on_project_calls_built(root, graph),
            Message::RefineProjectCalls => self.refine_project_calls(),
            Message::RefineProgress {
                generation,
                done,
                total,
            } => {
                if generation == self.project_calls.generation {
                    self.project_calls.refine_progress = Some((done, total));
                }
                Task::none()
            }
            Message::ProjectCallsRefined {
                root,
                generation,
                edges,
                graph,
            } => self.on_project_calls_refined(root, generation, edges, graph),
            Message::ExplainProject => self.on_explain_project(),
            Message::CancelExplain => self.on_cancel_explain(),
            Message::ExplainProgress {
                generation,
                done,
                total,
                failed,
            } => {
                if generation == self.explain.generation {
                    self.explain.progress = Some((done, total));
                    self.explain.failed = failed;
                }
                Task::none()
            }
            Message::ExplainDone {
                root,
                generation,
                cache,
                failed,
                auth_error,
            } => self.on_explain_done(root, generation, cache, failed, auth_error),
            Message::RefreshAll => self.on_refresh_all(),
            Message::ShowExplanation(node) => {
                self.show_right_panel = true;
                self.show_explanation(node)
            }
            Message::ReexplainNode => self.on_reexplain_node(),
            Message::ExplainBlocks(node) => self.on_explain_blocks(node),
            Message::BlocksExplained { node, detail } => self.on_blocks_explained(node, detail),
            Message::SvgsGenerated { generation, map } => self.on_svgs_generated(generation, map),
            Message::ShowOverview => {
                self.overview.showing = true;
                self.stats.showing = false;
                self.docs.page = None;
                Task::none()
            }
            Message::ServerConnected(tx) => self.on_server_connected(tx),
            Message::ServerUnavailable => self.on_server_unavailable(),
            Message::OpenConnect => {
                self.connect = Some(ConnectUi::default());
                // Already on a live remote? Skip the form and browse its folders.
                if self.connection.is_remote() && self.server_tx.is_some() {
                    self.enter_remote_browser(None);
                }
                Task::none()
            }
            Message::CloseConnect => {
                self.connect = None;
                Task::none()
            }
            Message::ConnectField(field, value) => self.on_connect_field(field, value),
            Message::ConnectPickIdentity => {
                Task::perform(pick_file(), Message::ConnectIdentityPicked)
            }
            Message::ConnectIdentityPicked(path) => {
                if let (Some(ui), Some(path)) = (&mut self.connect, path) {
                    ui.identity = path.to_string_lossy().into_owned();
                }
                Task::none()
            }
            Message::ConnectSubmit => self.on_connect_submit(),
            Message::ConnectToSaved(idx) => {
                if let Some(conn) = self.saved_connections.get(idx).cloned() {
                    self.connect_to(conn.target());
                }
                Task::none()
            }
            Message::ConnectRemoveSaved(idx) => {
                if idx < self.saved_connections.len() {
                    self.saved_connections.remove(idx);
                    if let Err(e) = connect::save(&self.saved_connections) {
                        self.status = format!("Cannot save connections: {e}");
                    }
                }
                Task::none()
            }
            Message::ConnectDisconnect => {
                self.connect = None;
                if self.connection.is_remote() {
                    self.connect_to(connect::ConnTarget::Local);
                }
                Task::none()
            }
            Message::RemoteBrowseTo(path) => {
                self.enter_remote_browser(Some(path));
                Task::none()
            }
            Message::RemoteBrowseUp => {
                if let Some(ConnectStage::Browsing(b)) = self.connect.as_ref().map(|u| &u.stage)
                    && let Some(parent) = b.parent.clone()
                {
                    self.enter_remote_browser(Some(parent));
                }
                Task::none()
            }
            Message::RemoteOpenHere => self.on_remote_open_here(),
            Message::DocsRefresh => {
                self.request_docs();
                Task::none()
            }
            Message::DocsToggleFile(rel) => {
                if !self.docs.expanded.remove(&rel) {
                    self.docs.expanded.insert(rel);
                }
                Task::none()
            }
            Message::DocsFilterChanged(s) => {
                self.docs.filter = s;
                Task::none()
            }
            Message::DocsToggleShowAll => {
                self.docs.show_all = !self.docs.show_all;
                Task::none()
            }
            Message::DocsToggleGrouping => {
                self.docs.by_module = !self.docs.by_module;
                Task::none()
            }
            Message::DocsSelect { rel, line } => {
                self.open_doc_page(&rel, line);
                Task::none()
            }
            Message::ViewDocsFromMenu => self.on_view_docs_from_menu(),
            Message::RegisterProcFeed { proc, feed } => {
                self.proc_feeds.insert(proc, feed);
                Task::none()
            }
            Message::ServerEvent(msg) => match msg {
                clew_protocol::ServerMessage::Reply { id, event, .. } => {
                    self.handle_server_reply(id, event)
                }
                clew_protocol::ServerMessage::Notification { event, .. } => {
                    self.handle_server_event(event);
                    Task::none()
                }
            },
            Message::ShowStats => {
                self.stats.showing = true;
                self.overview.showing = false;
                self.docs.page = None;
                // Compute on entry when there's nothing to show or the file set
                // changed since the last run; otherwise the cached report stays.
                self.start_stats(false)
            }
            Message::RefreshStats => self.start_stats(true),
            Message::StatsDone { root, rev, report } => self.on_stats_done(root, rev, report),
            Message::GenerateOverview => self.on_generate_overview(),
            Message::GenerateWalkthrough(scope) => self.on_generate_walkthrough(scope),
            Message::GenerateDiffWalkthrough => self.on_generate_diff_walkthrough(),
            Message::WalkthroughRegenerate(i) => self.on_walkthrough_regenerate(i),
            Message::WalkthroughDelete(i) => self.on_walkthrough_delete(i),
            Message::WalkthroughDone { scope, result } => self.on_walkthrough_done(scope, result),
            Message::WalkthroughOpen(i) => {
                if i >= self.walk.library.len() {
                    return Task::none();
                }
                self.walk.open = Some(i);
                self.walk.step = 0;
                self.walkthrough_goto(0)
            }
            Message::WalkthroughBack => {
                self.walk.open = None;
                self.walk.prepared = Vec::new();
                Task::none()
            }
            Message::WalkthroughToggleMode => {
                self.walk.mode = match self.walk.mode {
                    WalkMode::Search => WalkMode::Walk,
                    WalkMode::Walk => WalkMode::Search,
                };
                Task::none()
            }
            Message::WalkthroughGoto(i) => self.walkthrough_goto(i),
            Message::WalkthroughStep(delta) => self.on_walkthrough_step(delta),
            Message::WalkthroughInputChanged(s) => {
                self.walk.input = s;
                Task::none()
            }
            Message::ResizeWalkNarration(y) => {
                // Narration height = distance from the drag point to the window
                // bottom, clamped so neither block collapses.
                let max = (self.window_height - 160.0).max(120.0);
                self.walk.narration_height = (self.window_height - y).clamp(90.0, max);
                Task::none()
            }
            Message::OverviewDone {
                root,
                prompt_hash,
                result,
            } => self.on_overview_done(root, prompt_hash, result),
            Message::BuildEmbeddings => self.on_build_embeddings(),
            Message::EmbeddingsBuilt { root, result } => self.on_embeddings_built(root, result),
            Message::SemanticQueryChanged(q) => {
                self.semantic_query = q;
                Task::none()
            }
            Message::SemanticSearch => self.on_semantic_search(),
            Message::SemanticResults { query, result } => self.on_semantic_results(query, result),
            Message::OpenNode(node) => match node {
                explain::Node::Function { file, name } => {
                    let line = self
                        .symbol_index_by_file
                        .get(&file)
                        .and_then(|syms| syms.iter().find(|s| s.name == name).map(|s| s.line));
                    self.open_file(file, line, true)
                }
                explain::Node::File(p) => self.open_file(p, None, true),
                explain::Node::Folder(p) => self.show_explanation(explain::Node::Folder(p)),
            },
            Message::JumpToCall {
                caller_file,
                caller,
                callee,
            } => {
                let line = self
                    .call_site_line(&caller_file, &caller, &callee)
                    .or_else(|| {
                        self.symbol_index_by_file
                            .get(&caller_file)
                            .and_then(|syms| syms.iter().find(|s| s.name == caller).map(|s| s.line))
                    });
                self.open_file(caller_file, line, true)
            }
            Message::ToggleAsk => self.on_toggle_ask(),
            Message::BottomTabPicked(tab) => {
                self.show_bottom = true;
                self.bottom_tab = tab;
                Task::none()
            }
            Message::CollapseBottom => {
                self.show_bottom = false;
                Task::none()
            }
            Message::ToggleToolsMenu => {
                self.show_tools_menu = !self.show_tools_menu;
                self.show_target_menu = false;
                Task::none()
            }
            Message::ToggleTargetMenu => {
                self.show_target_menu = !self.show_target_menu;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::OpenShortcuts => {
                self.show_tools_menu = false;
                self.show_shortcuts = true;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            Message::TutorialStart => self.on_tutorial_start(),
            Message::TutorialStep(delta) => self.on_tutorial_step(delta),
            Message::TutorialGoto(step) => self.on_tutorial_goto(step),
            Message::TutorialExit => self.on_tutorial_exit(),
            Message::CloseShortcuts => {
                self.show_shortcuts = false;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            Message::RebindStart(action) => {
                self.rebinding = Some(action);
                self.keymap_notice = None;
                Task::none()
            }
            Message::RebindReset(action) => {
                self.keymap.reset(action);
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            Message::RebindResetAll => {
                self.keymap.reset_all();
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            Message::ToggleInlineSummaries => {
                self.show_inline_summaries = !self.show_inline_summaries;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::ToggleFileBanner => {
                self.show_file_banner = !self.show_file_banner;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::ToggleMinimap => {
                self.show_minimap = !self.show_minimap;
                self.show_tools_menu = false;
                Task::none()
            }
            Message::TargetSelected(target) => self.on_target_selected(target),
            Message::ToggleInlayHints => self.on_toggle_inlay_hints(),
            Message::SkimFile => {
                self.skim_active_file();
                self.show_tools_menu = false;
                Task::none()
            }
            Message::AskInputChanged(s) => {
                self.ask_input = s;
                Task::none()
            }
            Message::AskSuggested(q) => {
                self.ask_input = q;
                Task::done(Message::AskSubmit)
            }
            Message::AskSubmit => self.on_ask_submit(),
            Message::AskRetrieved { question, qvec } => self.on_ask_retrieved(question, qvec),
            Message::AskDelta(text) => self.on_ask_delta(text),
            Message::AskStreamEnded(error) => self.on_ask_stream_ended(error),
            Message::AskClear => {
                self.ask_turns.clear();
                self.ask_pins.clear();
                Task::none()
            }
            Message::AskUnpin(i) => {
                if i < self.ask_pins.len() {
                    self.ask_pins.remove(i);
                }
                Task::none()
            }
            Message::AskPinGoto(i) => match self.ask_pins.get(i) {
                Some(pin) => self.open_file(pin.file.clone(), Some(pin.line), true),
                None => Task::none(),
            },
            Message::AskAboutSelection => self.on_ask_about_selection(),
            Message::WhyIsThisHere => self.on_why_is_this_here(),
            Message::BlameWhyDone {
                title,
                commits,
                result,
            } => self.on_blame_why_done(title, commits, result),
            Message::BlameWhyClose => {
                self.blame_why = None;
                Task::none()
            }
            Message::TimeTravelStart { symbol } => self.on_time_travel_start(symbol),
            Message::TimeTravelReady {
                generation,
                abs,
                rel,
                lang,
                scope,
                commits,
            } => self.on_time_travel_ready(generation, abs, rel, lang, scope, commits),
            Message::TimeTravelGoto(idx) => self.on_time_travel_goto(idx),
            Message::TimeTravelStep {
                generation,
                idx,
                step,
            } => self.on_time_travel_step(generation, idx, step),
            Message::TimeTravelScrolled(viewport) => self.on_time_travel_scrolled(viewport),
            Message::TimeTravelSelectStart { line, col } => {
                self.on_time_travel_select_start(line, col)
            }
            Message::TimeTravelSelectDrag { line, col } => {
                self.on_time_travel_select_drag(line, col)
            }
            Message::TimeTravelToggleScope => {
                let Some(tt) = self.time_travel.as_ref() else {
                    return Task::none();
                };
                // File -> the function at the current focus/caret; Symbol -> File.
                let symbol = matches!(tt.scope, TimeScope::File);
                Task::done(Message::TimeTravelStart { symbol })
            }
            Message::TimeTravelExit => {
                self.time_travel = None;
                self.time_gen += 1; // invalidate any in-flight loads
                // Restore the live pane to where the reader was before entering
                // (its scrollable remounts at the top otherwise).
                let y = self.active_viewer().map(|v| v.scroll_y).unwrap_or(0.0);
                operation::scroll_to(
                    ui::code_scroll_id(self.active),
                    AbsoluteOffset { x: 0.0, y },
                )
            }
            Message::TimeTravelWhy => self.on_time_travel_why(),
            Message::TimeTravelWhyDone {
                generation: _,
                sha,
                result,
            } => self.on_time_travel_why_done(sha, result),
            Message::TimeTravelStory => self.on_time_travel_story(),
            Message::TimeTravelStoryDone {
                generation: _,
                result,
            } => self.on_time_travel_story_done(result),
            Message::Noop => Task::none(),
            Message::StartDebug => self.start_debug(),
            Message::DapStarted { client, port } => {
                if let Some(session) = self.debug.session.as_mut() {
                    session.client = Some(client);
                    session.port = port;
                    session.status = DebugStatus::Running;
                    self.status = "Debugger running…".into();
                }
                Task::none()
            }
            Message::DapChildStarted(client) => {
                // js-debug's child session owns the real target: make it active.
                if let Some(session) = self.debug.session.as_mut() {
                    session.client = Some(client);
                }
                Task::none()
            }
            Message::DapEvent(ev) => self.on_dap_event(ev),
            Message::DapStopInspected { frames, scopes } => {
                self.on_dap_stop_inspected(frames, scopes)
            }
            Message::DebugControl(cmd) => self.debug_control(cmd),
            Message::DebugStop => self.on_debug_stop(),
            Message::BreakpointToggle { path, line } => self.on_breakpoint_toggle(path, line),
            Message::DebugFailed(e) => {
                self.debug.session = None;
                self.status = format!("Debug failed: {e}");
                Task::none()
            }
            Message::ToggleBreakpointFromMenu => self.on_toggle_breakpoint_from_menu(),
            Message::ConditionalBreakpointFromMenu => self.on_conditional_breakpoint_from_menu(),
            Message::BpConditionInput(s) => {
                if let Some((_, _, draft)) = &mut self.debug.bp_cond_edit {
                    *draft = s;
                }
                Task::none()
            }
            Message::BpConditionSet => self.on_bp_condition_set(),
            Message::BpConditionCancel => {
                self.debug.bp_cond_edit = None;
                Task::none()
            }
            Message::DebugWatchInput(s) => {
                self.debug.watch_input = s;
                Task::none()
            }
            Message::DebugWatchAdd => {
                let expr = self.debug.watch_input.trim().to_string();
                if expr.is_empty() {
                    return Task::none();
                }
                self.debug.watches.push(expr);
                self.debug.watch_input.clear();
                self.eval_watches()
            }
            Message::DebugWatchRemove(i) => self.on_debug_watch_remove(i),
            Message::DebugWatchesEvaluated(vals) => {
                if let Some(s) = self.debug.session.as_mut() {
                    s.watches = vals;
                }
                Task::none()
            }
            Message::SettingsEmbedKeyChanged(s) => {
                self.settings.embed_key = s;
                Task::none()
            }
            Message::SettingsEmbedModelChanged(s) => {
                self.settings.embed_model = s;
                Task::none()
            }
            Message::SettingsEmbedBaseUrlChanged(s) => {
                self.settings.embed_base_url = s;
                Task::none()
            }
            Message::CloseExplanation => {
                self.explain.view = None;
                self.explain.prepared = Vec::new();
                self.explain.showing_detail = false;
                Task::none()
            }
            Message::ToggleMarkdownSource(pane) => {
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.show_source = !v.show_source;
                }
                Task::none()
            }
            Message::OpenLink(url) => self.on_open_link(url),
            Message::OpenSettings => self.on_open_settings(),
            Message::CloseSettings => {
                self.settings.open = false;
                Task::none()
            }
            Message::SettingsProviderPicked(p) => {
                // Switching provider resets model/base_url to that provider's
                // defaults (the user can still edit them).
                self.settings.provider = p;
                self.settings.model = p.default_model().to_string();
                self.settings.base_url = p.default_base_url().to_string();
                Task::none()
            }
            Message::SettingsKeyChanged(s) => {
                self.settings.key = s;
                Task::none()
            }
            Message::SettingsModelChanged(s) => {
                self.settings.model = s;
                Task::none()
            }
            Message::SettingsBaseUrlChanged(s) => {
                self.settings.base_url = s;
                Task::none()
            }
            Message::SettingsSaved => self.on_settings_saved(),
            Message::Tick => self.on_tick(),
            // No-op: the redraw it triggers is the point (drives the graph
            // canvas's per-frame simulation via the frames() subscription).
            Message::GraphFrame => Task::none(),
            Message::ToggleServerPanel => {
                self.server_panel = !self.server_panel;
                if self.server_panel {
                    self.installed_servers = lsp::store::installed_servers();
                }
                Task::none()
            }
            Message::LspRestart(language) => {
                // Drop the running server (kills its child), then re-provision.
                self.lsp.remove(&language);
                self.lsp_opened.retain(|p| {
                    // Re-open docs of this language on restart.
                    highlight::detect(p) != Some(language.as_str())
                });
                self.ensure_lsp(&language)
            }
            Message::LspRemove { name, version } => self.on_lsp_remove(name, version),
            Message::LspDownloadFor(language) => {
                // Force a fresh provisioning attempt for this language.
                self.lsp.remove(&language);
                self.ensure_lsp(&language)
            }
        }
    }

    // ---- Explain-domain handlers (extracted from `update`) --------------------
}
