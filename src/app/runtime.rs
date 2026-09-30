//! Window lifecycle and chrome: construction, the iced hooks (view, theme,
//! title, subscription), window geometry and panel layout, the sidebar tabs,
//! and the refresh tick.
//!
//! Its messages, [`WindowMsg`], arrive through `App::update_window`.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn new() -> (Self, Task<Message>) {
        let mut app = App::blank();
        // On a remote connection the path is on that host, so it can't be
        // validated locally — it goes to the server as given. It still passes
        // the workspace-trust gate first: `request_open` is host-aware (the
        // approval is recorded for THIS host), and a remote repository is no
        // more trustworthy than a local one — its `.clew/lsp.toml` and
        // `launch.json` are read and acted on the same way.
        let open_task = if app.connection.is_remote() {
            match std::env::args().nth(1) {
                Some(arg) => app.request_open(PathBuf::from(arg)),
                None => Task::none(),
            }
        } else {
            match std::env::args().nth(1) {
                Some(arg) => {
                    let path = PathBuf::from(&arg);
                    let path = path.canonicalize().unwrap_or(path);
                    if path.is_dir() {
                        app.request_open(path)
                    } else if path.is_file() {
                        // Open the parent directory as the project, then the file.
                        let root = path
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| path.clone());
                        app.pending_open = Some(path);
                        app.request_open(root)
                    } else {
                        app.status = format!("No such path: {arg}");
                        Task::none()
                    }
                }
                None => Task::none(),
            }
        };
        // Silently check for a newer release in the background (once per process).
        let check = app.startup_update_check();
        (app, Task::batch([open_task, check]))
    }

    /// The transport this window starts on. `CLEW_SSH` selects a remote host
    /// for manual testing — but never inside the test suite, where an
    /// exported value would silently turn every fixture remote and make
    /// assertions depend on the developer's shell.
    fn startup_connection() -> connect::ConnTarget {
        if cfg!(test) {
            connect::ConnTarget::Local
        } else {
            connect::ConnTarget::from_env()
        }
    }

    pub(crate) fn blank() -> Self {
        App {
            proj: ProjectSession::default(),
            server: crate::app::server::ServerLink::default(),
            pending_open: None,
            pending_consent: None,
            trust: clew_core::trust::Trust::load(),
            scanning: false,
            sidebar: SidebarTab::Files,
            call_token: 0,
            debug_run: 0,
            debug_run_live: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            debug_stop: 0,
            import_dir: imports::Dir::Imports,
            trail_writer: TrailWriter::default(),
            connection: Self::startup_connection(),
            // A CLEW_SSH startup target never carries the per-host AI-key
            // opt-in; only the Connect flow can grant it.
            remote_ai_opt_in: false,
            saved_connections: connect::load(),
            connect: None,
            next_req_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            session_first_req: 0,
            pending_scan_root: None,
            conn_gen: 0,
            conn_respawn: false,
            handshake_failure: None,
            quitting: false,
            project_epoch: 0,
            server_epoch: 0,
            next_proc_id: 1,
            lsp_gen: std::collections::HashMap::new(),
            embed_available: embed::Config::available(),
            show_bottom: false,
            bottom_tab: BottomTab::Ask,
            debug: DebugState::default(),
            debug_hover_eval: false,
            llm_available: llm::Config::available(),
            llm_config_cache: None,
            show_tools_menu: false,
            show_target_menu: false,
            keymap: keymap::Keymap::load(),
            show_shortcuts: false,
            rebinding: None,
            keymap_notice: None,
            show_inline_summaries: true,
            show_file_banner: true,
            show_inlay_hints: true,
            show_minimap: true,
            settings: SettingsDraft::default(),
            // Load the persisted appearance settings and point the palette at
            // them before the first frame, so the window opens in the right theme.
            theme_pref: theme::init(),
            update: UpdateState {
                auto_check: updater::auto_check_enabled(),
                ..UpdateState::default()
            },
            graph_mode: true,
            graph_3d: true,
            graph_spin: true,
            graph_heat: false,
            show_left_sidebar: true,
            show_right_panel: true,
            lsp_doc_rev: 1,
            blame_why_seq: 0,
            inlay_gen: 0,
            server_panel: false,
            installed_servers: Vec::new(),
            lsp_located: HashMap::new(),
            server_panel_seq: 0,
            selecting: false,
            code_focused: true,
            pending_g: false,
            pending_z: false,
            modifiers: keyboard::Modifiers::default(),
            status: "Open a folder to start reading".to_string(),
            waiting_said: false,
            main_window: None,
            tutorial: None,
            window_width: 1280.0,
            window_height: 800.0,
            fullscreen: false,
            window_focused: true,
            controls_hovered: false,
            sidebar_width: 280.0,
            right_width: 400.0,
            bottom_height: 340.0,
            font_size: DEFAULT_FONT_SIZE,
            #[cfg(test)]
            stale_dropped: 0,
            walk_ui: WalkUi::default(),
            docs_view: DocsView::default(),
        }
    }

    /// The window title — `project — file` when a file is open, so each window
    /// is distinguishable in the Dock's window list and the Window menu.
    pub(crate) fn title(&self) -> String {
        let project = self.proj.project.as_ref().map(|p| {
            p.root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        let file = self.active_viewer().map(|v| {
            v.abs
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        match (project, file) {
            (Some(p), Some(f)) => format!("{p} — {f}"),
            (Some(p), None) => p,
            _ => "Clew".to_string(),
        }
    }

    pub(crate) fn view(&self) -> Element<'_, Message> {
        ui::view(self)
    }

    pub(crate) fn theme(&self) -> iced::Theme {
        theme::app_theme()
    }

    /// This window's own async subscriptions: its clew-server stream and, while
    /// something is changing, a refresh tick. Global input events and the menu
    /// bridge live in the multi-window shell (see `crate::shell`), which routes
    /// them to the right window.
    pub(crate) fn window_subscription(&self) -> Subscription<Message> {
        let mut subs = Vec::new();
        // Start this window's clew-server only when it actually needs one — a
        // project is open or being opened, or the source is remote. An empty
        // window stays server-free until the user opens a folder: `start_scan`
        // then defers the OpenProject to `on_server_connected`, so the server
        // spins up on demand. On-disk changes are watched by that server, which
        // streams FilesChanged / Tree notifications (see `handle_server_event`).
        // When this stops holding, `update` releases the transport explicitly.
        if self.wants_server() {
            subs.push(server::subscription(server::ConnKey {
                target: self.connection.clone(),
                seq: self.conn_gen,
                respawn: self.conn_respawn,
            }));
        }
        // Poll for live refresh only while something is changing (a server is
        // starting, indexing, the management panel is open, an auto-refresh is
        // queued waiting out its cooldown, a bookmark/note edit waits to be
        // sent again, or the call-graph refine holds files for a server) —
        // idle stays quiet.
        if self.wants_tick() {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(400)).map(|_| Message::Tick),
            );
        }
        // No per-frame clock for the graph maps: the canvas requests its own
        // redraws (`canvas::Action::request_redraw`) while its simulation
        // moves and stops when it settles. The `window::frames()`
        // subscription this replaced ran update + view for EVERY window on
        // every frame for as long as a map was merely visible — settled or
        // not, and the Overview home shows one by default.
        Subscription::batch(subs)
    }

    /// Whether the window polls (`Message::Tick`) — see `subscription`. A
    /// refine holding files, or a full pass, for a server that is loading
    /// the project (`App::settle_refine_wait`) polls too: rust-analyzer can
    /// say it has loaded with no progress report in flight, a server just
    /// started is loaded once its grace runs out, and no message would
    /// follow either — the files waited out the whole bound, then were
    /// refined.
    pub(crate) fn wants_tick(&self) -> bool {
        let calls = &self.proj.project_calls;
        self.lsp_needs_refresh()
            || self.proj.refresh_pending
            || self.remote_edits_waiting()
            || calls.refine_wait.is_some()
            || calls.refine_full_wait.is_some()
    }

    /// Keep the draggable panel sizes within sensible bounds for the current
    /// window, so a panel can never be dragged to nothing or over the code.
    pub(crate) fn clamp_panel_sizes(&mut self) {
        let w = self.window_width.max(400.0);
        let h = self.window_height.max(300.0);
        self.sidebar_width = self.sidebar_width.clamp(160.0, (w * 0.5).max(200.0));
        self.right_width = self.right_width.clamp(240.0, (w * 0.6).max(280.0));
        self.bottom_height = self.bottom_height.clamp(100.0, (h * 0.75).max(160.0));
    }

    pub fn line_height(&self) -> f32 {
        self.font_size + 7.0
    }

    pub fn active_viewer(&self) -> Option<&Viewer> {
        self.proj.panes[self.proj.active].as_ref()
    }

    pub(crate) fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.proj.panes[self.proj.active].as_mut()
    }

    pub(crate) fn on_window_resized(&mut self, size: Size) -> Task<Message> {
        self.window_width = size.width;
        self.window_height = size.height;
        // Keep panel sizes sane against the new window bounds.
        self.clamp_panel_sizes();
        // Keep the materialized window generous enough for the new
        // height until the next scroll event refines it.
        for v in self.proj.panes.iter_mut().flatten() {
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
                    .proj
                    .walk
                    .open_tour()
                    .and_then(|w| w.steps.get(self.proj.walk.step))
                {
                    Some(step) if self.proj.walk.prepared.is_empty() => {
                        let (prepared, task) = self.prepare_segments(&step.narration.clone());
                        self.proj.walk.prepared = prepared;
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

    pub(crate) fn on_tick(&mut self) -> Task<Message> {
        // Journaled edits held back after a transient failure, once due.
        self.send_remote_edits();
        // Snapshot each ready server's diagnostics + inlay-refresh epoch.
        let versions: Vec<(String, u64, u64)> = self
            .proj
            .link
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
                self.proj.link.seen_diag_version.get(lang).copied() != Some(*diag)
                    || self.proj.link.seen_inlay_epoch.get(lang).copied() != Some(*epoch)
            })
            .map(|(lang, _, _)| lang.clone())
            .collect();
        for (lang, diag, epoch) in &versions {
            self.proj.link.seen_diag_version.insert(lang.clone(), *diag);
            self.proj.link.seen_inlay_epoch.insert(lang.clone(), *epoch);
        }
        let mut inlay_tasks = Vec::new();
        for lang in &changed {
            let files: Vec<PathBuf> = self
                .proj
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
        let refresh = if self.proj.refresh_pending
            && !self.proj.explain.running
            && !self.proj.overview.generating
            && !self.proj.building_embeddings
            && self
                .proj
                .last_auto_refresh
                .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
                .unwrap_or(true)
        {
            self.begin_refresh(true)
        } else {
            Task::none()
        };
        Task::batch([Task::batch(inlay_tasks), refresh])
    }
}

impl App {
    /// Handle a [`WindowMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_window(&mut self, message: WindowMsg) -> Task<Message> {
        match message {
            WindowMsg::SidebarTabPicked(tab) => self.on_sidebar_tab_picked(tab),
            WindowMsg::ToggleLeftSidebar => {
                self.show_left_sidebar = !self.show_left_sidebar;
                Task::none()
            }
            WindowMsg::ToggleRightPanel => {
                self.show_right_panel = !self.show_right_panel;
                Task::none()
            }
            // Drag *this* window — not `window::latest()`, which would drag the
            // most-recently-opened window no matter which one you grabbed.
            WindowMsg::TitleBarDragged => match self.main_window {
                Some(id) => iced::window::drag(id),
                None => Task::none(),
            },
            WindowMsg::FocusChanged(focused) => {
                self.window_focused = focused;
                // When following the system appearance, re-check on focus — the
                // user may have flipped the OS theme while clew was in the
                // background. Only re-color if it actually changed.
                if focused {
                    self.follow_system_appearance();
                    // Another window may have saved Settings while this one was
                    // in the background: re-read the AI config on next use.
                    self.invalidate_llm_config();
                }
                Task::none()
            }
            WindowMsg::ControlsHover(over) => {
                self.controls_hovered = over;
                Task::none()
            }
            // Closing is the shell's (`Shell::CloseRequested`): it asks about
            // edits the window cannot send before it lets the window go, and
            // it tears the window down. It takes this message before it
            // reaches the App, so a close can never skip the question.
            WindowMsg::Close => Task::none(),
            // These act on *this* App's own window (not `window::latest()`, which
            // is wrong once there are several windows).
            WindowMsg::Minimize => {
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
            WindowMsg::ToggleFullscreen => {
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
            WindowMsg::Resized(size) => self.on_window_resized(size),
            WindowMsg::ResizeSidebar(x) => {
                self.sidebar_width = x;
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::ResizeRight(x) => {
                self.right_width = self.window_width - x;
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::ResizeBottom(y) => {
                self.bottom_height = self.window_height - y;
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::BottomTabPicked(tab) => {
                self.show_bottom = true;
                self.bottom_tab = tab;
                Task::none()
            }
            WindowMsg::CollapseBottom => {
                self.show_bottom = false;
                Task::none()
            }
            WindowMsg::ToggleToolsMenu => {
                self.show_tools_menu = !self.show_tools_menu;
                self.show_target_menu = false;
                Task::none()
            }
            WindowMsg::ToggleTargetMenu => {
                self.show_target_menu = !self.show_target_menu;
                self.show_tools_menu = false;
                Task::none()
            }
            WindowMsg::OpenShortcuts => {
                self.show_shortcuts = true;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            WindowMsg::CloseShortcuts => {
                self.show_shortcuts = false;
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            WindowMsg::RebindStart(action) => {
                self.rebinding = Some(action);
                self.keymap_notice = None;
                Task::none()
            }
            WindowMsg::RebindReset(action) => {
                self.keymap.reset(action);
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            WindowMsg::RebindResetAll => {
                self.keymap.reset_all();
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = format!("Could not save shortcuts: {e}");
                }
                Task::none()
            }
            WindowMsg::ToggleInlineSummaries => {
                self.show_inline_summaries = !self.show_inline_summaries;
                Task::none()
            }
            WindowMsg::ToggleFileBanner => {
                self.show_file_banner = !self.show_file_banner;
                Task::none()
            }
            WindowMsg::ToggleMinimap => {
                self.show_minimap = !self.show_minimap;
                Task::none()
            }
        }
    }
}
