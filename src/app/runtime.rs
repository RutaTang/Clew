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
            match path_argument(std::env::args_os()) {
                Some(arg) => app.request_open(arg),
                None => Task::none(),
            }
        } else {
            match path_argument(std::env::args_os()) {
                Some(arg) => {
                    let path = arg.canonicalize().unwrap_or_else(|_| arg.clone());
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
                        app.status = tr!("No such path: {path}", path = arg.display());
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
            setup: None,
            pending_setup: None,
            trust: clew_core::trust::Trust::load(),
            scanning: false,
            sidebar: SidebarTab::Files,
            call_token: 0,
            flow_token: 0,
            debug_run: 0,
            debug_run_live: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            debug_stop: 0,
            import_dir: imports::Dir::Imports,
            trail_writer: TrailWriter::default(),
            connection: Self::startup_connection(),
            left_local: None,
            escape_taken: false,
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
            embed_missing: crate::app::embedding::why_no_embeddings(),
            show_bottom: false,
            ask_view: AskView::Hidden,
            ask_sessions_menu: false,
            ask_sessions_query: String::new(),
            voice: crate::app::voice::VoiceState::new(),
            local_embedding: crate::app::embedding::LocalEmbedding {
                program: clew_core::embed::local::program().is_some(),
                ..Default::default()
            },
            debug: DebugState::default(),
            debug_hover_eval: false,
            llm_available: llm::Config::available(),
            llm_config_cache: None,
            show_tools_menu: false,
            show_target_menu: false,
            show_app_menu: false,
            margin_tab: MarginTab::default(),
            show_view_menu: false,
            file_filter: String::new(),
            keymap: keymap::Keymap::load(),
            rebinding: None,
            keymap_notice: None,
            show_inline_summaries: view_pref(VIEW_SUMMARIES, true),
            show_file_banner: view_pref(VIEW_BANNER, true),
            show_inlay_hints: view_pref(VIEW_INLAY_HINTS, true),
            show_minimap: view_pref(VIEW_MINIMAP, false),
            show_inline_blame: view_pref(VIEW_BLAME, false),
            show_indent_guides: view_pref(VIEW_INDENT_GUIDES, false),
            settings: SettingsDraft::default(),
            ask_agent: crate::app::integrations::stored_ask_agent(),
            overlay_open: false,
            arm_on_enter: window_unplaced(),
            acp_agents: clew_core::acp::config::load().0,
            // Load the persisted appearance settings and point the palette at
            // them before the first frame, so the window opens in the right theme.
            theme_pref: {
                // And the language, so the first frame speaks it: the saved
                // one for the first window, the one spoken for a later one.
                crate::i18n::init();
                theme::init()
            },
            update: UpdateState {
                auto_check: updater::auto_check_enabled(),
                ..UpdateState::default()
            },
            graph_mode: true,
            graph_3d: true,
            graph_spin: true,
            graph_heat: false,
            graph_fit_rev: 0,
            show_left_sidebar: true,
            show_right_panel: true,
            lsp_doc_rev: 1,
            blame_why_seq: 0,
            insight_seq: 0,
            palette: Vec::new(),
            palette_recent: Vec::new(),
            palette_home: Vec::new(),
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
            status: tr!("Open a folder to start reading").to_string(),
            status_seen: tr!("Open a folder to start reading").to_string(),
            status_since: None,
            waiting_said: None,
            main_window: None,
            tutorial: None,
            tutorial_layout: None,
            tour_prep: None,
            tour_sample: None,
            tour_then: None,
            window_width: 1280.0,
            window_height: 800.0,
            fullscreen: false,
            maximized: false,
            window_outer: iced::Size::ZERO,
            window_frame: 0.0,
            window_radius: 0.0,
            composited: composited_from_start(),
            software_renderer: false,
            window_focused: true,
            asking: Default::default(),
            controls_hovered: false,
            // The design's sidebar, as it is laid out: its six views take
            // 234 between its 10s.
            sidebar_width: 254.0,
            right_width: 272.0,
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
        // In the order they are spelled (`highlight::in_order`): the Dock
        // and the Window menu draw a direction override as the file would
        // like to be read.
        let title = match (project, file) {
            (Some(p), Some(f)) => format!("{p} — {f}"),
            (Some(p), None) => p,
            _ => return "Clew".to_string(),
        };
        crate::highlight::in_order(&title).into_owned()
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
        } else if self.status_may_expire() {
            // A notice on the status line, to be taken away once read.
            subs.push(iced::time::every(STATUS_CHECK).map(|_| Message::Tick));
        }
        // The level meter and the pause that ends listening, read only while
        // the microphone is open — and the moment the pill says what was
        // heard, so it goes when the moment is over.
        if self.voice.is_listening() || self.voice.heard_fresh() {
            subs.push(
                iced::time::every(crate::app::voice::LISTEN_TICK)
                    .map(|_| Message::Voice(crate::VoiceMsg::Tick)),
            );
        }
        // While Clew speaks: the meter, and the moment all is said — when
        // the microphone, on, opens again.
        if self.voice.speech.is_speaking() {
            subs.push(
                iced::time::every(crate::app::speech::SPEAK_TICK)
                    .map(|_| Message::Voice(crate::VoiceMsg::SpeakTick)),
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

    /// The code's line height: airy, as the design sets code — 24 for the
    /// default 13.
    pub fn line_height(&self) -> f32 {
        self.font_size + 11.0
    }

    pub fn active_viewer(&self) -> Option<&Viewer> {
        self.proj.panes[self.proj.active].as_ref()
    }

    pub(crate) fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.proj.panes[self.proj.active].as_mut()
    }

    pub(crate) fn on_window_resized(&mut self, size: Size) -> Task<Message> {
        self.window_outer = size;
        self.reframe();
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
        macos::configure_frameless(macos::WINDOW_RADIUS);
        // And so does entering or leaving it by the window's own means: the
        // controls show what the window is in.
        // A window maximized by the system's own means (a double click on
        // the bar, a snap) is a resize too: its button shows "restore".
        match self.main_window {
            Some(id) => Task::batch([
                iced::window::mode(id).map(|mode| Message::Window(WindowMsg::ModeIs(mode))),
                iced::window::is_maximized(id)
                    .map(|now| Message::Window(WindowMsg::MaximizedIs(now))),
            ]),
            None => Task::none(),
        }
    }

    /// The frame round the window, its corners, and the room inside it,
    /// from the window's size and whether it fills the screen. Before the
    /// system has said the window's size there is nothing to frame.
    pub(crate) fn reframe(&mut self) {
        if self.window_outer == Size::ZERO {
            return;
        }
        let full = self.maximized || self.fullscreen;
        self.window_frame = crate::ui::window_frame_for(full);
        self.window_radius =
            crate::ui::window_radius_for(full, self.composited && !self.software_renderer);
        self.window_width = self.window_outer.width - 2.0 * self.window_frame;
        self.window_height = self.window_outer.height - 2.0 * self.window_frame;
    }

    /// Close every menu of the chrome: they open one at a time.
    pub(crate) fn close_menus(&mut self) {
        self.show_tools_menu = false;
        self.show_target_menu = false;
        self.show_app_menu = false;
        self.show_view_menu = false;
    }

    pub(crate) fn on_sidebar_tab_picked(&mut self, tab: SidebarTab) -> Task<Message> {
        self.sidebar = tab;
        self.show_left_sidebar = true; // reveal it for external triggers
        // Close the menu that opened this, if one did.
        self.show_tools_menu = false;
        match tab {
            // The search input takes keyboard focus — except while the
            // tutorial shows the tab, whose keys step the tour.
            SidebarTab::Search if self.tutorial.is_none() => {
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
        }
    }

    /// Note what the status line says now: when it says something new, the
    /// time it began to (see `status_since`).
    pub(crate) fn note_status(&mut self) {
        if self.status != self.status_seen {
            self.status_seen.clone_from(&self.status);
            self.status_since = (!self.status.is_empty()).then(std::time::Instant::now);
        }
    }

    /// Whether what the status line says tells of something still going on —
    /// work under way ("Loading…"), a wait, an edit not saved, a link that
    /// failed — and so stands for as long as it does.
    fn status_stands(&self) -> bool {
        let calls = &self.proj.project_calls;
        self.status.ends_with('…')
            || self.status.ends_with("...")
            || self.waiting_said.is_some()
            || self.says_not_saved()
            || self.handshake_failure.as_deref() == Some(self.status.as_str())
            || calls.refine_wait.is_some()
            || calls.refine_full_wait.is_some()
    }

    /// Whether the status line says a notice that will be taken away.
    pub(crate) fn status_may_expire(&self) -> bool {
        self.status_since.is_some() && !self.status_stands()
    }

    /// Take a notice off the status line once it has been up for
    /// `STATUS_LIFETIME`: the line goes back to the project's counts.
    pub(crate) fn expire_status(&mut self) {
        let due = self
            .status_since
            .is_some_and(|since| since.elapsed() >= STATUS_LIFETIME);
        if due && self.status_may_expire() {
            self.status.clear();
            self.note_status();
        }
    }

    pub(crate) fn on_tick(&mut self) -> Task<Message> {
        self.expire_status();
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
            // The system resizes it from here, as from a frame of its own.
            WindowMsg::ResizeDragged(direction) => match self.main_window {
                Some(id) => iced::window::drag_resize(id, direction),
                None => Task::none(),
            },
            WindowMsg::FocusChanged(focused) => {
                self.window_focused = focused;
                // The window coming to the front: the question in it is
                // within reach from now, though it may never have been
                // told it had lost the focus (some platforms report no
                // focus at start), and the click that raised it is no
                // answer (`App::track_question`).
                if focused {
                    self.asking.question = None;
                }
                // When following the system appearance, re-check on focus — the
                // user may have flipped the OS theme while clew was in the
                // background. Only re-color if it actually changed.
                if focused {
                    // A commit, a checkout made elsewhere: the Files view's
                    // marks are read anew.
                    self.proj.git_status_stale = true;
                    self.follow_system_appearance();
                    // Another window may have saved Settings while this one was
                    // in the background: re-read the AI config on next use.
                    self.invalidate_llm_config();
                    // Or downloaded or removed the embedding model, or chosen
                    // another source: what can be made follows, and the
                    // server is told what is in effect now.
                    self.refresh_embeddings();
                    self.send_ai_config();
                    // A compositing manager may have come or gone meanwhile
                    // (a desktop's switch for games, say).
                    return self.ask_composited();
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
            // The window is asked what it is in, not a flag remembered: it
            // leaves fullscreen by itself too (the green control, ⌃⌘F,
            // Esc), and the remembered flag then made the next toggle do
            // nothing.
            WindowMsg::ToggleFullscreen => match self.main_window {
                Some(id) => iced::window::mode(id).then(move |mode| {
                    let next = if mode == iced::window::Mode::Fullscreen {
                        iced::window::Mode::Windowed
                    } else {
                        iced::window::Mode::Fullscreen
                    };
                    Task::batch([
                        iced::window::set_mode(id, next),
                        Task::done(Message::Window(WindowMsg::ModeIs(next))),
                    ])
                }),
                None => Task::none(),
            },
            WindowMsg::ModeIs(mode) => {
                self.fullscreen = mode == iced::window::Mode::Fullscreen;
                // Filling the screen, the window has no frame or rounded
                // corners; a window again, it has them again.
                self.reframe();
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::Composited(now) => {
                self.composited = now;
                self.reframe();
                Task::none()
            }
            WindowMsg::SoftwareRenderer(now) => {
                self.software_renderer = now;
                self.reframe();
                Task::none()
            }
            WindowMsg::Resized(size) => self.on_window_resized(size),
            // Moved, or drawn at another scale, at the same size: what is in
            // it is where it was in the window, not on the screen — every
            // question's answers report anew, and wait anew (`ui::placed`).
            // (Wayland tells a client nothing of where its window is: a move
            // there reaches it as the pointer's own motion.)
            WindowMsg::Moved => {
                self.asking.places.clear();
                Task::none()
            }
            WindowMsg::ResizeSidebar(x) => {
                self.sidebar_width = x;
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::ResizeRight(x) => {
                // The margin reaches from the pointer to the sheet's right
                // edge (its border inside the gap before the tool strip).
                self.right_width = crate::ui::margin_width_at(self.window_width, x);
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::ResizeBottom(y) => {
                self.bottom_height = self.window_height - y;
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::ShowDebugPanel => {
                self.show_bottom = true;
                Task::none()
            }
            WindowMsg::CollapseBottom => {
                self.show_bottom = false;
                Task::none()
            }
            WindowMsg::ToggleToolsMenu => {
                // Its tools on the strip, there is no ⋯ to open a menu
                // from: one opened then hung under a button not there.
                let open = !self.show_tools_menu && !crate::ui::tools_on_strip(self);
                self.close_menus();
                self.show_tools_menu = open;
                Task::none()
            }
            WindowMsg::ToggleTargetMenu => {
                let open = !self.show_target_menu;
                self.close_menus();
                self.show_target_menu = open;
                Task::none()
            }
            WindowMsg::ToggleAppMenu => {
                let open = !self.show_app_menu;
                self.close_menus();
                self.show_app_menu = open;
                Task::none()
            }
            WindowMsg::ToggleViewMenu => {
                let open = !self.show_view_menu;
                self.close_menus();
                self.show_view_menu = open;
                Task::none()
            }
            WindowMsg::FileFilterChanged(filter) => {
                self.file_filter = filter;
                Task::none()
            }
            // The Escape that clears a filled filter does only that: the
            // same press, forwarded to the key handler (`shell::forward_key`),
            // also closed the find bar or the Ask card, or dropped the code
            // selection. An empty filter leaves Escape to do what it does.
            WindowMsg::FileFilterEscaped => {
                if !self.file_filter.is_empty() {
                    self.file_filter.clear();
                    self.escape_taken = true;
                }
                Task::none()
            }
            // Enter opens what the filter found first, as a click on its row
            // would: typing a file's name and pressing Enter did nothing.
            WindowMsg::FileFilterSubmitted => {
                let first = self.proj.project.as_ref().and_then(|p| {
                    crate::ui::filtered_files(p, &self.file_filter)
                        .next()
                        .map(|f| f.rel.clone())
                });
                match first {
                    Some(rel) => {
                        Task::done(Message::Editor(EditorMsg::OpenRel { rel, line: None }))
                    }
                    None => Task::none(),
                }
            }
            // Asked first, then set: what the button shows next is what was
            // asked for, not a read that may race the change.
            WindowMsg::ToggleMaximize => match self.main_window {
                Some(id) => iced::window::is_maximized(id).then(move |was| {
                    Task::batch([
                        iced::window::maximize(id, !was),
                        Task::done(Message::Window(WindowMsg::MaximizedIs(!was))),
                    ])
                }),
                None => Task::none(),
            },
            WindowMsg::FilesTreeScrolled { offset, height } => {
                self.proj.tree_scroll = Some((offset, height));
                Task::none()
            }
            WindowMsg::MarginTabPicked(tab) => {
                self.margin_tab = tab;
                self.show_right_panel = true;
                Task::none()
            }
            WindowMsg::MaximizedIs(now) => {
                self.maximized = now;
                // Maximized, the window has no frame; restored, it has again.
                self.reframe();
                self.clamp_panel_sizes();
                Task::none()
            }
            WindowMsg::OpenShortcuts => {
                // The shortcuts are a page of Settings, not a panel apart.
                self.rebinding = None;
                self.keymap_notice = None;
                self.update(Message::Settings(SettingsMsg::OpenAt(
                    crate::SettingsPage::Shortcuts,
                )))
            }
            WindowMsg::RebindStart(action) => {
                self.rebinding = Some(action);
                self.keymap_notice = None;
                Task::none()
            }
            WindowMsg::RebindCancel => {
                self.rebinding = None;
                self.keymap_notice = None;
                Task::none()
            }
            WindowMsg::RebindReset(action) => {
                self.rebinding = None;
                if let Err(other) = self.keymap.reset(action) {
                    self.keymap_notice = Some(tr!(
                        "Its default shortcut is now used by “{action}” — change that one first",
                        action = crate::i18n::lookup(other.label())
                    ));
                    return Task::none();
                }
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = tr!("Could not save shortcuts: {error}", error = e);
                }
                Task::none()
            }
            WindowMsg::RebindResetAll => {
                self.keymap.reset_all();
                self.rebinding = None;
                self.keymap_notice = None;
                if let Err(e) = self.keymap.save() {
                    self.status = tr!("Could not save shortcuts: {error}", error = e);
                }
                Task::none()
            }
            WindowMsg::ToggleInlineSummaries => {
                self.show_inline_summaries = !self.show_inline_summaries;
                self.keep_view_pref(VIEW_SUMMARIES, self.show_inline_summaries);
                Task::none()
            }
            WindowMsg::ToggleFileBanner => {
                self.show_file_banner = !self.show_file_banner;
                self.keep_view_pref(VIEW_BANNER, self.show_file_banner);
                Task::none()
            }
            WindowMsg::ToggleMinimap => {
                self.show_minimap = !self.show_minimap;
                self.keep_view_pref(VIEW_MINIMAP, self.show_minimap);
                Task::none()
            }
            WindowMsg::ToggleInlineBlame => {
                self.show_inline_blame = !self.show_inline_blame;
                self.keep_view_pref(VIEW_BLAME, self.show_inline_blame);
                Task::none()
            }
            WindowMsg::ToggleIndentGuides => {
                self.show_indent_guides = !self.show_indent_guides;
                self.keep_view_pref(VIEW_INDENT_GUIDES, self.show_indent_guides);
                Task::none()
            }
            WindowMsg::NoFileDialog => {
                self.status =
                    tr!("No file dialog could open here — install xdg-desktop-portal or zenity")
                        .into();
                Task::none()
            }
        }
    }
}

/// The View menu's choices, as the global config's root keeps them: a
/// minimap or blame turned on is there next launch (they were this
/// session's only, unlike the theme).
/// How long a notice stays on the status line: long enough to be read.
pub(crate) const STATUS_LIFETIME: std::time::Duration = std::time::Duration::from_secs(12);
/// How often a notice on the status line is looked at to see whether its
/// time is up.
const STATUS_CHECK: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) const VIEW_SUMMARIES: &str = "view_inline_summaries";
pub(crate) const VIEW_BANNER: &str = "view_file_banner";
pub(crate) const VIEW_INLAY_HINTS: &str = "view_inlay_hints";
pub(crate) const VIEW_MINIMAP: &str = "view_minimap";
pub(crate) const VIEW_BLAME: &str = "view_inline_blame";
pub(crate) const VIEW_INDENT_GUIDES: &str = "view_indent_guides";

/// A View-menu choice as saved, or `default` when it never was (or the
/// config cannot be read).
fn view_pref(key: &str, default: bool) -> bool {
    // A test's app is the app as shipped: the suite shares one data
    // directory, so a test that toggled a choice would change every later
    // test's view.
    if cfg!(test) {
        return default;
    }
    clew_core::globalconfig::get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

impl App {
    /// Keep a View-menu choice for the next launch. Not kept, it still holds
    /// for this session, and the reader is told.
    pub(crate) fn keep_view_pref(&mut self, key: &str, on: bool) {
        if cfg!(test) {
            return;
        }
        let saved =
            clew_core::globalconfig::set_keys(vec![(key.to_string(), toml::Value::Boolean(on))]);
        if let Err(e) = saved {
            self.status = tr!("Could not save settings: {error}", error = e);
        }
    }
}

/// Whether the window system tells a window nothing of where it is on the
/// screen: Wayland, which winit takes where `WAYLAND_DISPLAY` or
/// `WAYLAND_SOCKET` says it is. (macOS, Windows and X11 report a move.) As
/// noted at start ([`note_window_system`]), or read now.
fn window_unplaced() -> bool {
    *UNPLACED.get_or_init(wayland_named)
}

/// Whether the window system composites Clew's windows over the desktop
/// before anything is asked: Wayland always does, and Windows does a window
/// presented through a visual (`app::corners`). X is asked once the window
/// is up ([`App::ask_composited`]).
fn composited_from_start() -> bool {
    window_unplaced() || crate::app::corners::through_visual()
}

impl App {
    /// Have Windows draw no border of its own round this window, which
    /// presents through a visual and leaves its corners clear
    /// (`app::corners`). Nothing to do elsewhere.
    pub(crate) fn leave_edges_clear(&self) -> Task<Message> {
        #[cfg(windows)]
        if let Some(id) = self.main_window
            && crate::app::corners::through_visual()
        {
            return iced::window::raw_id::<Message>(id).then(|window| {
                crate::app::corners::leave_edges_clear(window);
                Task::none()
            });
        }
        Task::none()
    }

    /// Ask whether X composites this window (`app::corners`), for its
    /// corners: where they can be left clear, they are rounded. Nothing to
    /// ask on Wayland, which always composites, nor off Linux.
    pub(crate) fn ask_composited(&self) -> Task<Message> {
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(id) = self.main_window
            && !window_unplaced()
        {
            return iced::window::raw_id::<Message>(id).then(|window| {
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            crate::app::corners::x11_composited(window)
                        })
                        .await
                        .unwrap_or(false)
                    },
                    |now| Message::Window(WindowMsg::Composited(now)),
                )
            });
        }
        Task::none()
    }
}

/// What [`window_unplaced`] says, once noted.
static UNPLACED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Note which window system the windows will be on, before iced connects to
/// it: Wayland's client takes `WAYLAND_SOCKET` out of the environment as it
/// connects.
pub fn note_window_system() {
    let _ = UNPLACED.set(wayland_named());
}

/// Whether the environment names a Wayland display, as winit reads it.
fn wayland_named() -> bool {
    let named = |var| std::env::var_os(var).is_some_and(|v| !v.is_empty());
    cfg!(all(unix, not(target_os = "macos")))
        && (named("WAYLAND_DISPLAY") || named("WAYLAND_SOCKET"))
}

/// The path clew was started with (its first argument), as the system gave
/// it: a name that is not UTF-8 (a Latin-1 folder on Linux) is a path all
/// the same — read as a `String`, the `std::env::args` iterator panicked on
/// it, before any window was up.
pub(crate) fn path_argument(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<PathBuf> {
    args.into_iter().nth(1).map(PathBuf::from)
}
