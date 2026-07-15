//! clew — a code reader.
//!
//! v2: gitignore-aware file tree, virtualized tree-sitter code viewer,
//! fuzzy file finder (Cmd+P), project symbol search (Cmd+T), full-text
//! search (Cmd+Shift+F), navigation history, outline, split view (Cmd+\),
//! line selection + copy (Cmd+C), bookmarks (Cmd+D), go-to-line (:N).

mod bookmarks;
mod codeview;
mod finder;
mod fs_scan;
mod highlight;
mod history;
mod index;
mod outline;
mod search;
mod theme;
mod ui;
mod viewer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::widget::operation;
use iced::widget::scrollable::{self, AbsoluteOffset};
use iced::{Element, Size, Subscription, Task, keyboard};

use crate::bookmarks::Bookmark;
use crate::finder::{Finder, FinderMode};
use crate::fs_scan::{DirNode, FileEntry, ScanResult};
use crate::highlight::HlLine;
use crate::history::{History, Loc};
use crate::index::SymbolEntry;
use crate::outline::Symbol;
use crate::search::SearchHit;
use crate::viewer::{MAX_FILE_BYTES, Viewer};

pub fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size(Size::new(1280.0, 860.0))
        .centered()
        .run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarTab {
    Files,
    Search,
    Marks,
}

pub struct Project {
    pub root: PathBuf,
    pub tree: DirNode,
    pub files: Arc<Vec<FileEntry>>,
    pub truncated: bool,
}

#[derive(Default)]
pub struct SearchState {
    pub query: String,
    pub running: bool,
    pub ran: bool,
    pub hits: Vec<SearchHit>,
}

pub struct App {
    pub project: Option<Project>,
    /// File to open automatically once the initial scan completes
    /// (set when the CLI argument is a file path).
    pub pending_open: Option<PathBuf>,
    /// Project root awaiting the user's consent to create `.clew`.
    /// While `Some`, the consent modal is shown.
    pub pending_consent: Option<PathBuf>,
    pub scanning: bool,
    pub sidebar: SidebarTab,
    pub expanded: HashSet<String>,
    /// Code panes; pane 1 exists only in split view.
    pub panes: [Option<Viewer>; 2],
    pub split: bool,
    pub active: usize,
    pub show_outline: bool,
    pub finder: Finder,
    pub search: SearchState,
    pub history: History,
    pub bookmarks: Vec<Bookmark>,
    pub symbol_index: Arc<Vec<SymbolEntry>>,
    pub indexing: bool,
    /// True while a mouse drag-selection is in progress.
    pub selecting: bool,
    pub modifiers: keyboard::Modifiers,
    pub status: String,
    /// Logical window width (from resize events), drives responsive layout.
    pub window_width: f32,
    pub font_size: f32,
}

pub const DEFAULT_FONT_SIZE: f32 = 13.0;

#[derive(Debug, Clone)]
pub enum Message {
    OpenFolderPressed,
    FolderPicked(Option<PathBuf>),
    ConsentAllowed,
    ConsentDenied,
    ScanDone(ScanResult),
    SymbolIndexDone(Vec<SymbolEntry>),
    ToggleDir(String),
    OpenRel {
        rel: String,
        line: Option<usize>,
    },
    OpenAbs {
        abs: PathBuf,
        line: Option<usize>,
        push: bool,
    },
    FileLoaded {
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    },
    Highlighted {
        abs: PathBuf,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
    },
    CodeScrolled(usize, scrollable::Viewport),
    PaneFocused(usize),
    ToggleSplit,
    SelectStart {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectDrag {
        pane: usize,
        line: usize,
        col: usize,
    },
    SelectEnd,
    CopySelection,
    SidebarTabPicked(SidebarTab),
    SearchQueryChanged(String),
    SearchSubmitted,
    SearchDone {
        hits: Vec<SearchHit>,
    },
    FinderOpened(FinderMode),
    FinderClosed,
    FinderQueryChanged(String),
    FinderPick(usize),
    FinderConfirm,
    GotoLineRequested,
    BookmarkToggled,
    BookmarkRemoved(usize),
    GoBack,
    GoForward,
    ToggleOutline,
    OutlineJump(usize),
    FontSizeDelta(f32),
    FontSizeReset,
    KeyPressed(keyboard::Key, keyboard::Modifiers),
    ModifiersChanged(keyboard::Modifiers),
    WindowResized(Size),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let mut app = App::blank();
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

    fn blank() -> Self {
        App {
            project: None,
            pending_open: None,
            pending_consent: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            expanded: HashSet::new(),
            panes: [None, None],
            split: false,
            active: 0,
            show_outline: true,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            bookmarks: Vec::new(),
            symbol_index: Arc::new(Vec::new()),
            indexing: false,
            selecting: false,
            modifiers: keyboard::Modifiers::default(),
            status: "Open a folder to start reading".to_string(),
            window_width: 1280.0,
            font_size: DEFAULT_FONT_SIZE,
        }
    }

    pub fn line_height(&self) -> f32 {
        self.font_size + 7.0
    }

    pub fn active_viewer(&self) -> Option<&Viewer> {
        self.panes[self.active].as_ref()
    }

    fn active_viewer_mut(&mut self) -> Option<&mut Viewer> {
        self.panes[self.active].as_mut()
    }

    fn title(&self) -> String {
        match &self.project {
            Some(p) => format!(
                "clew — {}",
                p.root.file_name().unwrap_or_default().to_string_lossy()
            ),
            None => "clew".to_string(),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        ui::view(self)
    }

    fn theme(&self) -> iced::Theme {
        theme::app_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        // listen_with (rather than keyboard::listen) also sees events already
        // captured by focused widgets, so shortcuts like Esc work while a
        // text input has focus.
        iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                Some(Message::KeyPressed(key, modifiers))
            }
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => {
                Some(Message::ModifiersChanged(m))
            }
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
                Some(Message::SelectEnd)
            }
            iced::Event::Window(iced::window::Event::Resized(size)) => {
                Some(Message::WindowResized(size))
            }
            _ => None,
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
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
            Message::ConsentAllowed => {
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
            Message::ScanDone(result) => self.on_scan_done(result),
            Message::SymbolIndexDone(entries) => {
                self.indexing = false;
                self.symbol_index = Arc::new(entries);
                if let Some(p) = &self.project {
                    self.status = format!(
                        "{} files · {} symbols",
                        p.files.len(),
                        self.symbol_index.len()
                    );
                }
                if self.finder.open && self.finder.mode == FinderMode::Symbols {
                    self.finder.refresh_symbols(&self.symbol_index);
                }
                Task::none()
            }
            Message::ToggleDir(rel) => {
                if !self.expanded.remove(&rel) {
                    self.expanded.insert(rel);
                }
                Task::none()
            }
            Message::OpenRel { rel, line } => {
                let Some(project) = &self.project else {
                    return Task::none();
                };
                let abs = project.root.join(&rel);
                self.open_file(abs, line, true)
            }
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
            } => {
                let lines = Arc::new(lines);
                for slot in &mut self.panes {
                    if let Some(v) = slot
                        && v.abs == abs
                        && v.lines.len() == lines.len()
                    {
                        v.set_lines(lines.clone());
                        v.symbols = symbols.clone();
                        v.highlighted = true;
                    }
                }
                Task::none()
            }
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
                }
                Task::none()
            }
            Message::ToggleSplit => {
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
            Message::SelectStart { pane, line, col } => {
                if pane == 0 || self.split {
                    self.active = pane;
                }
                let extend = self.modifiers.shift();
                if let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut) {
                    match (extend, v.selection) {
                        (true, Some((anchor, _))) => v.selection = Some((anchor, line)),
                        _ => v.selection = Some((line, line)),
                    }
                    v.caret = Some((line, col));
                    self.selecting = true;
                }
                Task::none()
            }
            Message::SelectDrag { pane, line, col } => {
                if self.selecting
                    && pane == self.active
                    && let Some(v) = self.panes.get_mut(pane).and_then(Option::as_mut)
                    && let Some((anchor, _)) = v.selection
                {
                    v.selection = Some((anchor, line));
                    v.caret = Some((line, col));
                }
                Task::none()
            }
            Message::SelectEnd => {
                self.selecting = false;
                Task::none()
            }
            Message::CopySelection => {
                let Some(text) = self.active_viewer().and_then(Viewer::selected_text) else {
                    return Task::none();
                };
                let n = text.lines().count();
                self.status = format!("Copied {n} line{}", if n == 1 { "" } else { "s" });
                iced::clipboard::write(text)
            }
            Message::SidebarTabPicked(tab) => {
                self.sidebar = tab;
                match tab {
                    SidebarTab::Search => operation::focus(ui::search_input_id()),
                    _ => Task::none(),
                }
            }
            Message::SearchQueryChanged(query) => {
                self.search.query = query;
                Task::none()
            }
            Message::SearchSubmitted => {
                let Some(project) = &self.project else {
                    return Task::none();
                };
                let query = self.search.query.trim().to_string();
                if query.is_empty() {
                    return Task::none();
                }
                self.search.running = true;
                self.search.ran = true;
                self.search.hits.clear();
                let files = project.files.clone();
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || search::search(files, query))
                            .await
                            .unwrap_or_default()
                    },
                    |hits| Message::SearchDone { hits },
                )
            }
            Message::SearchDone { hits } => {
                self.search.running = false;
                self.status = if hits.len() >= search::MAX_HITS {
                    format!("{}+ matches (capped)", hits.len())
                } else {
                    format!("{} matches", hits.len())
                };
                self.search.hits = hits;
                Task::none()
            }
            Message::FinderOpened(mode) => {
                if self.project.is_none() {
                    return Task::none();
                }
                self.finder.open = true;
                self.finder.mode = mode;
                self.finder.query.clear();
                self.refresh_finder();
                operation::focus(ui::finder_input_id())
            }
            Message::FinderClosed => {
                self.finder.open = false;
                Task::none()
            }
            Message::FinderQueryChanged(query) => {
                self.finder.query = query;
                self.refresh_finder();
                Task::none()
            }
            Message::FinderPick(idx) => self.finder_open_index(idx),
            Message::FinderConfirm => {
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
            Message::GotoLineRequested => {
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
            Message::BookmarkToggled => {
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
            Message::BookmarkRemoved(idx) => {
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
            Message::GoBack => match self.history.back() {
                Some(loc) => self.open_file(loc.path, loc.line, false),
                None => Task::none(),
            },
            Message::GoForward => match self.history.forward() {
                Some(loc) => self.open_file(loc.path, loc.line, false),
                None => Task::none(),
            },
            Message::ToggleOutline => {
                self.show_outline = !self.show_outline;
                Task::none()
            }
            Message::OutlineJump(line) => match self.active_viewer() {
                Some(v) => {
                    let abs = v.abs.clone();
                    self.open_file(abs, Some(line), true)
                }
                None => Task::none(),
            },
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
                Task::none()
            }
            Message::WindowResized(size) => {
                self.window_width = size.width;
                // Keep the materialized window generous enough for the new
                // height until the next scroll event refines it.
                for v in self.panes.iter_mut().flatten() {
                    v.viewport_h = v.viewport_h.max(size.height);
                }
                Task::none()
            }
        }
    }

    fn on_scan_done(&mut self, result: ScanResult) -> Task<Message> {
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
        self.history.clear();
        self.finder = Finder::default();
        self.search = SearchState::default();
        self.bookmarks = bookmarks::load(&result.root);
        self.symbol_index = Arc::new(Vec::new());
        let files = Arc::new(result.files);
        self.project = Some(Project {
            root: result.root,
            tree: result.tree,
            files: files.clone(),
            truncated: result.truncated,
        });

        // Build the project-wide symbol index in the background.
        self.indexing = true;
        let index_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || index::build(files))
                    .await
                    .unwrap_or_default()
            },
            Message::SymbolIndexDone,
        );

        let open_task = match self.pending_open.take() {
            Some(file) => self.open_file(file, None, true),
            None => Task::none(),
        };
        Task::batch([index_task, open_task])
    }

    fn refresh_finder(&mut self) {
        match self.finder.mode {
            FinderMode::Files => {
                if let Some(p) = &self.project {
                    let files = p.files.clone();
                    self.finder.refresh_files(&files);
                }
            }
            FinderMode::Symbols => {
                let symbols = self.symbol_index.clone();
                self.finder.refresh_symbols(&symbols);
            }
        }
    }

    /// Gate every project open behind `.clew/` consent: an existing `.clew/`
    /// directory counts as consent already given; otherwise ask, and refuse
    /// to open the project when denied or not writable.
    fn request_open(&mut self, root: PathBuf) -> Task<Message> {
        // An existing .clew records consent already given: open straight away.
        if root.join(".clew").is_dir() {
            return self.start_scan(root);
        }
        // Otherwise ask via an in-app modal (see ui::consent_modal).
        self.pending_consent = Some(root);
        Task::none()
    }

    fn start_scan(&mut self, root: PathBuf) -> Task<Message> {
        self.scanning = true;
        self.status = format!("Scanning {}…", root.display());
        Task::perform(
            async move {
                let fallback_root = root.clone();
                tokio::task::spawn_blocking(move || fs_scan::scan(root))
                    .await
                    .unwrap_or_else(|_| ScanResult {
                        root: fallback_root,
                        tree: DirNode::default(),
                        files: Vec::new(),
                        truncated: false,
                    })
            },
            Message::ScanDone,
        )
    }

    fn finder_open_index(&mut self, idx: usize) -> Task<Message> {
        self.finder.open = false;
        match self.finder.mode {
            FinderMode::Files => {
                let Some(entry) = self
                    .project
                    .as_ref()
                    .and_then(|p| p.files.get(idx))
                    .cloned()
                else {
                    return Task::none();
                };
                self.open_file(entry.abs, None, true)
            }
            FinderMode::Symbols => {
                let Some(entry) = self.symbol_index.get(idx).cloned() else {
                    return Task::none();
                };
                self.open_file(entry.abs, Some(entry.line), true)
            }
        }
    }

    /// Open a file into the active pane, optionally jumping to a 1-based line.
    fn open_file(&mut self, abs: PathBuf, line: Option<usize>, push: bool) -> Task<Message> {
        if push {
            self.history.push(Loc {
                path: abs.clone(),
                line,
            });
        }
        let pane = self.active;
        let line_height = self.line_height();
        // Same file already in the active pane: just scroll.
        if let Some(v) = self.active_viewer_mut()
            && v.abs == abs
        {
            v.target_line = line;
            let y = v.scroll_offset_for(line, line_height);
            v.scroll_y = y;
            return operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        }
        self.status = format!("Loading {}…", self.rel_of(&abs));
        Task::perform(load_file(pane, abs, line), |(pane, abs, target, result)| {
            Message::FileLoaded {
                pane,
                abs,
                target,
                result,
            }
        })
    }

    fn on_file_loaded(
        &mut self,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        let rel = self.rel_of(&abs);
        let content = match result {
            Err(e) => {
                self.status = format!("{rel}: {e}");
                return Task::none();
            }
            Ok(content) => content,
        };

        let lang_key = highlight::detect(&abs);
        let source = Arc::new(content);
        let lines = highlight::plain_lines(&source);
        let line_height = self.line_height();
        let old_viewport = self.panes[pane].as_ref().map(|v| v.viewport_h);
        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        if let Some(h) = old_viewport {
            v.viewport_h = h;
        }
        v.target_line = target;
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = format!("{} — {} lines", v.rel, v.lines.len());
        self.panes[pane] = Some(v);

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&source, lang_key);
                    let symbols = lang_key
                        .map(|key| outline::extract(&source, key))
                        .unwrap_or_default();
                    (lines, symbols)
                })
                .await
                .unwrap_or_default()
            },
            move |(lines, symbols)| Message::Highlighted {
                abs: abs.clone(),
                lines,
                symbols,
            },
        );
        Task::batch([scroll, highlight_task])
    }

    /// Keep the top visible line stable across a line-height change.
    fn rescale_scroll(&mut self, old_line_height: f32) -> Task<Message> {
        let new_line_height = self.line_height();
        if (new_line_height - old_line_height).abs() < f32::EPSILON {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for (pane, slot) in self.panes.iter_mut().enumerate() {
            if let Some(v) = slot {
                let first = v.scroll_y / old_line_height;
                v.scroll_y = first * new_line_height;
                tasks.push(operation::scroll_to(
                    ui::code_scroll_id(pane),
                    AbsoluteOffset {
                        x: 0.0,
                        y: v.scroll_y,
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    fn handle_key(&mut self, key: keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();
        match key.as_ref() {
            Key::Character(c) if cmd && !modifiers.shift() && c.eq_ignore_ascii_case("p") => {
                self.update(Message::FinderOpened(FinderMode::Files))
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("t") => {
                self.update(Message::FinderOpened(FinderMode::Symbols))
            }
            Key::Character(c) if cmd && modifiers.shift() && c.eq_ignore_ascii_case("f") => {
                self.update(Message::SidebarTabPicked(SidebarTab::Search))
            }
            Key::Character(c) if cmd && !self.finder.open && c.eq_ignore_ascii_case("c") => {
                self.update(Message::CopySelection)
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("d") => {
                self.update(Message::BookmarkToggled)
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("l") => {
                self.update(Message::GotoLineRequested)
            }
            Key::Character("\\") if cmd => self.update(Message::ToggleSplit),
            Key::Character("=") | Key::Character("+") if cmd => {
                self.update(Message::FontSizeDelta(1.0))
            }
            Key::Character("-") if cmd => self.update(Message::FontSizeDelta(-1.0)),
            Key::Character("0") if cmd => self.update(Message::FontSizeReset),
            Key::Named(Named::Escape) => {
                if self.finder.open {
                    return self.update(Message::FinderClosed);
                }
                if let Some(v) = self.active_viewer_mut() {
                    v.selection = None;
                    v.target_line = None;
                }
                Task::none()
            }
            Key::Named(Named::ArrowDown) if self.finder.open => {
                self.finder.move_selection(1);
                Task::none()
            }
            Key::Named(Named::ArrowUp) if self.finder.open => {
                self.finder.move_selection(-1);
                Task::none()
            }
            Key::Named(Named::ArrowLeft) if modifiers.alt() => self.update(Message::GoBack),
            Key::Named(Named::ArrowRight) if modifiers.alt() => self.update(Message::GoForward),
            _ => Task::none(),
        }
    }

    fn rel_of(&self, abs: &Path) -> String {
        self.project
            .as_ref()
            .and_then(|p| abs.strip_prefix(&p.root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| abs.display().to_string())
    }
}

async fn pick_folder() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Open a project folder")
        .pick_folder()
        .await
        .map(|handle| handle.path().to_path_buf())
}


async fn load_file(
    pane: usize,
    abs: PathBuf,
    target: Option<usize>,
) -> (usize, PathBuf, Option<usize>, Result<String, String>) {
    let read_path = abs.clone();
    let result = tokio::task::spawn_blocking(move || read_text_file(&read_path))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    (pane, abs, target, result)
}

fn read_text_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file too large ({:.1} MB, limit {} MB)",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Err("binary file".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod app_tests {
    use super::*;

    /// Each test gets its own directory: tests run in parallel and would
    /// otherwise race on remove_dir_all/create of a shared fixture.
    fn fixture_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clew-app-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub struct Point { x: f64 }\n\npub fn origin() -> Point {\n    Point { x: 0.0 }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "needle in notes\n").unwrap();
        dir.canonicalize().unwrap()
    }

    /// Drive the update loop the way the runtime would, executing the
    /// blocking parts inline instead of through iced Tasks.
    fn open_synchronously(app: &mut App, rel: &str, line: Option<usize>) {
        let abs = app.project.as_ref().unwrap().root.join(rel);
        let pane = app.active;
        let _ = app.update(Message::OpenRel {
            rel: rel.to_string(),
            line,
        });
        let content = read_text_file(&abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane,
            abs: abs.clone(),
            target: line,
            result: Ok(content.clone()),
        });
        let lang = highlight::detect(&abs);
        let lines = highlight::highlight_lines(&content, lang);
        let symbols = lang
            .map(|k| outline::extract(&content, k))
            .unwrap_or_default();
        let _ = app.update(Message::Highlighted {
            abs,
            lines,
            symbols,
        });
    }

    fn scanned_app(tag: &str) -> App {
        let root = fixture_project(tag);
        let mut app = App::blank();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));
        app
    }

    #[test]
    fn full_reading_flow() {
        let mut app = scanned_app("reading");
        assert!(app.project.is_some());
        assert_eq!(app.project.as_ref().unwrap().files.len(), 2);

        // Open a file at a line.
        open_synchronously(&mut app, "src/lib.rs", Some(3));
        let v = app.active_viewer().unwrap();
        assert_eq!(v.rel, "src/lib.rs");
        assert!(v.highlighted);
        assert_eq!(v.target_line, Some(3));
        assert_eq!(v.lines.len(), 5);

        // Outline extracted for the current file.
        let names: Vec<&str> = v.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"origin"), "outline: {names:?}");

        // Open a second file, then navigate back and forward.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.active_viewer().unwrap().rel, "notes.txt");
        assert!(app.history.can_back());

        let back = app.history.back().unwrap();
        assert!(back.path.ends_with("src/lib.rs"));
        assert_eq!(back.line, Some(3));
        let fwd = app.history.forward().unwrap();
        assert!(fwd.path.ends_with("notes.txt"));
    }

    #[test]
    fn finder_flow() {
        let mut app = scanned_app("finder");

        let _ = app.update(Message::FinderOpened(FinderMode::Files));
        assert!(app.finder.open);
        assert!(!app.finder.results.is_empty());

        let _ = app.update(Message::FinderQueryChanged("librs".to_string()));
        let files = app.project.as_ref().unwrap().files.clone();
        let top = files[app.finder.results[0]].rel.clone();
        assert_eq!(top, "src/lib.rs");

        // Confirm closes the finder.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
    }

    #[test]
    fn symbol_finder_flow() {
        let mut app = scanned_app("symbols");
        // Build the index synchronously (the runtime does this in a task).
        let files = app.project.as_ref().unwrap().files.clone();
        let _ = app.update(Message::SymbolIndexDone(index::build(files)));
        assert!(!app.indexing);
        assert!(app.symbol_index.len() >= 2, "{:?}", app.symbol_index);

        let _ = app.update(Message::FinderOpened(FinderMode::Symbols));
        let _ = app.update(Message::FinderQueryChanged("origin".to_string()));
        assert!(!app.finder.results.is_empty());
        let entry = &app.symbol_index[app.finder.results[0]];
        assert_eq!(entry.name, "origin");
        assert_eq!(entry.line, 3);

        // Confirm records the jump in history.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        let _ = app.update(Message::GoBack); // no-op or previous loc; must not panic
    }

    #[test]
    fn goto_line_via_finder() {
        let mut app = scanned_app("goto");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::GotoLineRequested);
        assert!(app.finder.open);
        let _ = app.update(Message::FinderQueryChanged(":4".to_string()));
        assert_eq!(app.finder.goto_line(), Some(4));
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        assert_eq!(app.active_viewer().unwrap().target_line, Some(4));
    }

    #[test]
    fn split_view_routes_to_active_pane() {
        let mut app = scanned_app("split");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::ToggleSplit);
        assert!(app.split);
        assert_eq!(app.active, 1);
        // Split duplicates the current file.
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "src/lib.rs");

        // Opening now targets pane 1; pane 0 keeps its file.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.panes[1].as_ref().unwrap().rel, "notes.txt");
        assert_eq!(app.panes[0].as_ref().unwrap().rel, "src/lib.rs");

        // Refocus pane 0 and close the split.
        let _ = app.update(Message::PaneFocused(0));
        assert_eq!(app.active, 0);
        let _ = app.update(Message::ToggleSplit);
        assert!(!app.split);
        assert!(app.panes[1].is_none());
    }

    #[test]
    fn selection_and_copy_state() {
        let mut app = scanned_app("select");
        open_synchronously(&mut app, "src/lib.rs", None);

        let _ = app.update(Message::SelectStart { pane: 0, line: 1, col: 4 });
        assert!(app.selecting);
        assert_eq!(app.active_viewer().unwrap().caret, Some((1, 4)));
        let _ = app.update(Message::SelectDrag { pane: 0, line: 3, col: 2 });
        let _ = app.update(Message::SelectEnd);
        assert!(!app.selecting);

        let v = app.active_viewer().unwrap();
        assert_eq!(v.selection_bounds(), Some((1, 3)));
        assert_eq!(v.caret, Some((3, 2)));
        let text = v.selected_text().unwrap();
        assert!(text.starts_with('\n') || text.contains("origin"), "{text}");

        // Esc clears the selection.
        let _ = app.update(Message::KeyPressed(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::Modifiers::default(),
        ));
        assert!(app.active_viewer().unwrap().selection.is_none());
    }

    #[test]
    fn bookmark_toggle_persists_in_project() {
        let mut app = scanned_app("bookmark");
        let root = app.project.as_ref().unwrap().root.clone();
        open_synchronously(&mut app, "src/lib.rs", Some(3));

        let _ = app.update(Message::BookmarkToggled);
        assert_eq!(app.bookmarks.len(), 1);
        assert_eq!(app.bookmarks[0].rel, "src/lib.rs");
        assert_eq!(app.bookmarks[0].line, 3);
        assert!(root.join(".clew/bookmarks.json").exists());
        assert_eq!(bookmarks::load(&root), app.bookmarks);

        // Toggling again removes it and cleans up the store file; the .clew
        // directory itself stays (consent record).
        let _ = app.update(Message::BookmarkToggled);
        assert!(app.bookmarks.is_empty());
        assert!(!root.join(".clew/bookmarks.json").exists());
    }

    #[test]
    fn consent_gates_project_open() {
        let root = fixture_project("consent");

        // Picking a folder without .clew opens the consent modal, not the project.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        assert_eq!(app.pending_consent.as_deref(), Some(root.as_path()));
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());

        // Denied: nothing is created, no project opens, modal dismissed.
        let _ = app.update(Message::ConsentDenied);
        assert!(app.pending_consent.is_none());
        assert!(app.project.is_none() && !app.scanning);
        assert!(!root.join(".clew").exists());
        assert!(app.status.contains("not allowed"), "{}", app.status);

        // Allowed: .clew is created and the scan starts.
        let mut app = App::blank();
        let _ = app.update(Message::FolderPicked(Some(root.clone())));
        let _ = app.update(Message::ConsentAllowed);
        assert!(root.join(".clew").is_dir());
        assert!(app.scanning);
        assert!(app.pending_consent.is_none());

        // A project with .clew already present skips the consent modal:
        // FolderPicked goes straight to scanning.
        let mut app2 = App::blank();
        let _ = app2.update(Message::FolderPicked(Some(root.clone())));
        assert!(app2.scanning, "existing .clew must skip the prompt");
        assert!(app2.pending_consent.is_none());
    }

    #[test]
    fn search_flow_message_wiring() {
        let mut app = scanned_app("search");

        let files = app.project.as_ref().unwrap().files.clone();
        let hits = search::search(files, "needle".to_string());
        assert_eq!(hits.len(), 1);
        let _ = app.update(Message::SearchDone { hits });
        assert_eq!(app.search.hits.len(), 1);
        assert_eq!(app.search.hits[0].rel, "notes.txt");

        // Clicking a hit opens the file at that line.
        let hit = app.search.hits[0].clone();
        let _ = app.update(Message::OpenAbs {
            abs: hit.abs.clone(),
            line: Some(hit.line),
            push: true,
        });
        let content = read_text_file(&hit.abs).unwrap();
        let _ = app.update(Message::FileLoaded {
            pane: 0,
            abs: hit.abs,
            target: Some(hit.line),
            result: Ok(content),
        });
        assert_eq!(app.active_viewer().unwrap().target_line, Some(1));
    }

    #[test]
    fn font_size_rescales_scroll() {
        let mut app = scanned_app("font");
        open_synchronously(&mut app, "src/lib.rs", None);
        app.panes[0].as_mut().unwrap().scroll_y = 40.0; // line 2 at 20px
        let _ = app.update(Message::FontSizeDelta(2.0));
        assert_eq!(app.font_size, 15.0);
        let v = app.active_viewer().unwrap();
        assert!((v.scroll_y - 44.0).abs() < 0.01, "{}", v.scroll_y); // 2 * 22px
        let _ = app.update(Message::FontSizeReset);
        assert_eq!(app.font_size, DEFAULT_FONT_SIZE);
    }

    #[test]
    fn binary_and_oversized_files_are_rejected() {
        let dir = std::env::temp_dir().join("clew-guard-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0u8, 159, 146, 150]).unwrap();
        assert!(read_text_file(&bin).unwrap_err().contains("binary"));

        let big = dir.join("huge.txt");
        std::fs::write(&big, vec![b'a'; MAX_FILE_BYTES + 1]).unwrap();
        assert!(read_text_file(&big).unwrap_err().contains("too large"));
    }
}
