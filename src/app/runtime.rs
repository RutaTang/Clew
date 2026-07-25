//! App lifecycle: construction, viewport accessors, and the iced application hooks (view / theme / subscription).

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn new() -> (Self, Task<Message>) {
        let mut app = App::blank();
        // On a remote connection the path is on that host, so it can't be
        // validated locally — hand it straight to the server.
        if app.connection.is_remote() {
            let task = match std::env::args().nth(1) {
                Some(arg) => app.start_scan(PathBuf::from(arg)),
                None => Task::none(),
            };
            return (app, task);
        }
        let task = match std::env::args().nth(1) {
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
        };
        (app, task)
    }

    pub(crate) fn blank() -> Self {
        App {
            project: None,
            pending_open: None,
            pending_consent: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            call_graph: None,
            import_graph: imports::ImportGraph::default(),
            import_tree: None,
            import_dir: imports::Dir::Imports,
            import_cycles: Vec::new(),
            project_calls: ProjectCallsState::default(),
            overlay: None,
            explain: ExplainState::default(),
            overview: OverviewState::default(),
            walk: WalkState::default(),
            stats: StatsState::default(),
            server_tx: None,
            connection: connect::ConnTarget::from_env(),
            saved_connections: connect::load(),
            connect: None,
            docs: DocsState::default(),
            chat_streams: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            next_req_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ai_pending: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            pending_reads: std::collections::HashMap::new(),
            pending_scan_root: None,
            next_proc_id: 1,
            proc_feeds: std::collections::HashMap::new(),
            lsp_procs: std::collections::HashMap::new(),
            embed_index: embed::Index::default(),
            embed_available: embed::Config::available(),
            building_embeddings: false,
            semantic_query: String::new(),
            semantic_results: Vec::new(),
            searching_semantic: false,
            show_bottom: false,
            bottom_tab: BottomTab::Ask,
            ask_input: String::new(),
            ask_turns: Vec::new(),
            asking: false,
            ask_pins: Vec::new(),
            debug: DebugState::default(),
            note_edit: None,
            notes: Vec::new(),
            reading_note_edit: None,
            last_auto_refresh: None,
            refresh_pending: false,
            llm_available: llm::Config::available(),
            show_tools_menu: false,
            show_target_menu: false,
            keymap: keymap::Keymap::load(),
            show_shortcuts: false,
            rebinding: None,
            keymap_notice: None,
            show_inline_summaries: true,
            show_file_banner: true,
            show_inlay_hints: true,
            reading_target: inactive::Target::host(),
            show_minimap: true,
            settings: SettingsDraft::default(),
            graph_mode: true,
            graph_layout: None,
            expanded: HashSet::new(),
            panes: [None, None],
            split: false,
            active: 0,
            show_left_sidebar: true,
            show_right_panel: true,
            diff: None,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            trail_collapsed: std::collections::HashSet::new(),
            bookmarks: Vec::new(),
            symbol_index: Arc::new(Vec::new()),
            indexing: false,
            lsp_config: lsp::config::ProjectLspConfig::default(),
            lsp: std::collections::HashMap::new(),
            lsp_opened: HashSet::new(),
            registry: incremental::Registry::default(),
            symbol_index_by_file: HashMap::new(),
            structure: structure::StructureIndex::default(),
            lsp_doc_rev: 1,
            seen_diag_version: std::collections::HashMap::new(),
            seen_inlay_epoch: std::collections::HashMap::new(),
            pending_lsp_consent: None,
            find: find::FindState::default(),
            hover: None,
            hover_gen: 0,
            hover_pinned: false,
            blame_why: None,
            time_travel: None,
            time_gen: 0,
            context_menu: None,
            server_panel: false,
            installed_servers: Vec::new(),
            project_languages: Vec::new(),
            selecting: false,
            code_focused: true,
            pending_g: false,
            pending_z: false,
            modifiers: keyboard::Modifiers::default(),
            status: "Open a folder to start reading".to_string(),
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
        }
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
        self.panes[self.active].as_ref()
    }

    pub(crate) fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.panes[self.active].as_mut()
    }

    /// The window title — `project — file` when a file is open, so each window
    /// is distinguishable in the Dock's window list and the Window menu.
    pub(crate) fn title(&self) -> String {
        let project = self.project.as_ref().map(|p| {
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
        if self.project.is_some()
            || self.pending_scan_root.is_some()
            || self.pending_consent.is_some()
            || self.connection.is_remote()
        {
            subs.push(server::subscription(self.connection.clone()));
        }
        // Poll for live refresh only while something is changing (a server is
        // starting, indexing, the management panel is open, or an auto-refresh is
        // queued waiting out its cooldown) — idle stays quiet.
        if self.lsp_needs_refresh() || self.refresh_pending {
            subs.push(
                iced::time::every(std::time::Duration::from_millis(400)).map(|_| Message::Tick),
            );
        }
        Subscription::batch(subs)
    }

    pub(crate) fn lsp_needs_refresh(&self) -> bool {
        self.server_panel
            || self.lsp.iter().any(|(lang, s)| match s {
                LspSlot::Starting => true,
                LspSlot::Ready(c) => {
                    c.progress().is_some()
                        || self.seen_diag_version.get(lang).copied() != Some(c.diag_version())
                        || self.seen_inlay_epoch.get(lang).copied() != Some(c.inlay_epoch())
                }
                _ => false,
            })
    }
}
