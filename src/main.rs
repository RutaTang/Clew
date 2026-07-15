//! clew — a code reader.
//!
//! v1 features: project file tree (gitignore-aware), read-only virtualized
//! code viewer with tree-sitter highlighting, fuzzy file finder (Cmd/Ctrl+P),
//! project-wide text search, navigation history and a symbol outline.

mod finder;
mod fs_scan;
mod highlight;
mod history;
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

use crate::finder::Finder;
use crate::fs_scan::{DirNode, FileEntry, ScanResult};
use crate::highlight::HlLine;
use crate::history::{History, Loc};
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
    pub scanning: bool,
    pub sidebar: SidebarTab,
    pub expanded: HashSet<String>,
    pub viewer: Option<Viewer>,
    pub outline: Vec<Symbol>,
    pub show_outline: bool,
    pub finder: Finder,
    pub search: SearchState,
    pub history: History,
    pub status: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    OpenFolderPressed,
    FolderPicked(Option<PathBuf>),
    ScanDone(ScanResult),
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
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    },
    Highlighted {
        abs: PathBuf,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
    },
    CodeScrolled(scrollable::Viewport),
    SidebarTabPicked(SidebarTab),
    SearchQueryChanged(String),
    SearchSubmitted,
    SearchDone {
        hits: Vec<SearchHit>,
    },
    FinderOpened,
    FinderClosed,
    FinderQueryChanged(String),
    FinderPick(usize),
    FinderConfirm,
    GoBack,
    GoForward,
    ToggleOutline,
    OutlineJump(usize),
    KeyPressed(keyboard::Key, keyboard::Modifiers),
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let mut app = App {
            project: None,
            pending_open: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            expanded: HashSet::new(),
            viewer: None,
            outline: Vec::new(),
            show_outline: true,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            status: "Open a folder to start reading".to_string(),
        };
        let task = match std::env::args().nth(1) {
            Some(arg) => {
                let path = PathBuf::from(&arg);
                let path = path.canonicalize().unwrap_or(path);
                if path.is_dir() {
                    app.start_scan(path)
                } else if path.is_file() {
                    // Open the parent directory as the project, then the file.
                    let root = path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone());
                    app.pending_open = Some(path);
                    app.start_scan(root)
                } else {
                    app.status = format!("No such path: {arg}");
                    Task::none()
                }
            }
            None => Task::none(),
        };
        (app, task)
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
            _ => None,
        })
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::OpenFolderPressed => Task::perform(pick_folder(), Message::FolderPicked),
            Message::FolderPicked(None) => Task::none(),
            Message::FolderPicked(Some(root)) => self.start_scan(root),
            Message::ScanDone(result) => {
                self.scanning = false;
                self.status = format!(
                    "{} files{}",
                    result.files.len(),
                    if result.truncated { " (truncated)" } else { "" }
                );
                self.expanded.clear();
                self.viewer = None;
                self.outline.clear();
                self.history.clear();
                self.finder = Finder::default();
                self.search = SearchState::default();
                self.project = Some(Project {
                    root: result.root,
                    tree: result.tree,
                    files: Arc::new(result.files),
                    truncated: result.truncated,
                });
                match self.pending_open.take() {
                    Some(file) => self.open_file(file, None, true),
                    None => Task::none(),
                }
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
                abs,
                target,
                result,
            } => self.on_file_loaded(abs, target, result),
            Message::Highlighted {
                abs,
                lines,
                symbols,
            } => {
                if let Some(v) = &mut self.viewer
                    && v.abs == abs
                    && v.lines.len() == lines.len()
                {
                    v.lines = lines;
                    v.highlighted = true;
                    self.outline = symbols;
                }
                Task::none()
            }
            Message::CodeScrolled(viewport) => {
                if let Some(v) = &mut self.viewer {
                    v.scroll_y = viewport.absolute_offset().y;
                    v.viewport_h = viewport.bounds().height;
                }
                Task::none()
            }
            Message::SidebarTabPicked(tab) => {
                self.sidebar = tab;
                match tab {
                    SidebarTab::Search => operation::focus(ui::search_input_id()),
                    SidebarTab::Files => Task::none(),
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
            Message::FinderOpened => {
                let Some(project) = &self.project else {
                    return Task::none();
                };
                self.finder.open = true;
                self.finder.query.clear();
                self.finder.refresh(&project.files);
                operation::focus(ui::finder_input_id())
            }
            Message::FinderClosed => {
                self.finder.open = false;
                Task::none()
            }
            Message::FinderQueryChanged(query) => {
                self.finder.query = query;
                if let Some(project) = &self.project {
                    self.finder.refresh(&project.files);
                }
                Task::none()
            }
            Message::FinderPick(idx) => self.finder_open_index(idx),
            Message::FinderConfirm => {
                match self.finder.results.get(self.finder.selected).copied() {
                    Some(idx) => self.finder_open_index(idx),
                    None => Task::none(),
                }
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
            Message::OutlineJump(line) => match &self.viewer {
                Some(v) => {
                    let abs = v.abs.clone();
                    self.open_file(abs, Some(line), true)
                }
                None => Task::none(),
            },
            Message::KeyPressed(key, modifiers) => self.handle_key(key, modifiers),
        }
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
        let Some(project) = &self.project else {
            return Task::none();
        };
        let Some(entry) = project.files.get(idx) else {
            return Task::none();
        };
        let abs = entry.abs.clone();
        self.open_file(abs, None, true)
    }

    /// Open a file, optionally jumping to a 1-based line.
    fn open_file(&mut self, abs: PathBuf, line: Option<usize>, push: bool) -> Task<Message> {
        if push {
            self.history.push(Loc {
                path: abs.clone(),
                line,
            });
        }
        // Same file: just scroll.
        if let Some(v) = &mut self.viewer
            && v.abs == abs
        {
            v.target_line = line;
            let y = v.scroll_offset_for(line);
            v.scroll_y = y;
            return operation::scroll_to(ui::code_scroll_id(), AbsoluteOffset { x: 0.0, y });
        }
        self.status = format!("Loading {}…", self.rel_of(&abs));
        Task::perform(load_file(abs, line), |(abs, target, result)| {
            Message::FileLoaded {
                abs,
                target,
                result,
            }
        })
    }

    fn on_file_loaded(
        &mut self,
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
        let lines = highlight::plain_lines(&content);
        let viewport_h = self.viewer.as_ref().map(|v| v.viewport_h);
        let mut v = Viewer::new(abs.clone(), rel, lang_key, lines);
        if let Some(h) = viewport_h {
            v.viewport_h = h;
        }
        v.target_line = target;
        let y = v.scroll_offset_for(target);
        v.scroll_y = y;
        self.status = format!("{} — {} lines", v.rel, v.lines.len());
        self.outline.clear();
        self.viewer = Some(v);

        let scroll = operation::scroll_to(ui::code_scroll_id(), AbsoluteOffset { x: 0.0, y });
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&content, lang_key);
                    let symbols = lang_key
                        .map(|key| outline::extract(&content, key))
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

    fn handle_key(&mut self, key: keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        match key.as_ref() {
            Key::Character("p") | Key::Character("P")
                if modifiers.command() && !modifiers.shift() =>
            {
                self.update(Message::FinderOpened)
            }
            Key::Character("f") | Key::Character("F")
                if modifiers.command() && modifiers.shift() =>
            {
                self.update(Message::SidebarTabPicked(SidebarTab::Search))
            }
            Key::Named(Named::Escape) if self.finder.open => self.update(Message::FinderClosed),
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
    abs: PathBuf,
    target: Option<usize>,
) -> (PathBuf, Option<usize>, Result<String, String>) {
    let read_path = abs.clone();
    let result = tokio::task::spawn_blocking(move || read_text_file(&read_path))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    (abs, target, result)
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

    fn fixture_project() -> PathBuf {
        let dir = std::env::temp_dir().join("clew-app-test");
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

    fn blank_app() -> App {
        App {
            project: None,
            pending_open: None,
            scanning: false,
            sidebar: SidebarTab::Files,
            expanded: HashSet::new(),
            viewer: None,
            outline: Vec::new(),
            show_outline: true,
            finder: Finder::default(),
            search: SearchState::default(),
            history: History::default(),
            status: String::new(),
        }
    }

    /// Drive the update loop the way the runtime would, executing the
    /// blocking parts inline instead of through iced Tasks.
    fn open_synchronously(app: &mut App, rel: &str, line: Option<usize>) {
        let abs = app.project.as_ref().unwrap().root.join(rel);
        let _ = app.update(Message::OpenRel {
            rel: rel.to_string(),
            line,
        });
        let content = read_text_file(&abs).unwrap();
        let _ = app.update(Message::FileLoaded {
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

    #[test]
    fn full_reading_flow() {
        let root = fixture_project();
        let mut app = blank_app();

        // Scan.
        let _ = app.update(Message::ScanDone(fs_scan::scan(root.clone())));
        assert!(app.project.is_some());
        assert_eq!(app.project.as_ref().unwrap().files.len(), 2);

        // Open a file at a line.
        open_synchronously(&mut app, "src/lib.rs", Some(3));
        let v = app.viewer.as_ref().unwrap();
        assert_eq!(v.rel, "src/lib.rs");
        assert!(v.highlighted);
        assert_eq!(v.target_line, Some(3));
        assert_eq!(v.lines.len(), 5);

        // Outline extracted for the current file.
        let names: Vec<&str> = app.outline.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"origin"), "outline: {names:?}");

        // Open a second file, then navigate back and forward.
        open_synchronously(&mut app, "notes.txt", None);
        assert_eq!(app.viewer.as_ref().unwrap().rel, "notes.txt");
        assert!(app.history.can_back());

        let back = app.history.back().unwrap();
        assert!(back.path.ends_with("src/lib.rs"));
        assert_eq!(back.line, Some(3));
        let fwd = app.history.forward().unwrap();
        assert!(fwd.path.ends_with("notes.txt"));
    }

    #[test]
    fn finder_flow() {
        let root = fixture_project();
        let mut app = blank_app();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));

        let _ = app.update(Message::FinderOpened);
        assert!(app.finder.open);
        assert!(!app.finder.results.is_empty());

        let _ = app.update(Message::FinderQueryChanged("librs".to_string()));
        let files = app.project.as_ref().unwrap().files.clone();
        let top = files[app.finder.results[0]].rel.clone();
        assert_eq!(top, "src/lib.rs");

        // Confirm closes the finder and records history.
        let _ = app.update(Message::FinderConfirm);
        assert!(!app.finder.open);
        assert!(app
            .history
            .back()
            .is_none()); // single entry: nothing to go back to, but it exists
    }

    #[test]
    fn search_flow_message_wiring() {
        let root = fixture_project();
        let mut app = blank_app();
        let _ = app.update(Message::ScanDone(fs_scan::scan(root)));

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
            abs: hit.abs,
            target: Some(hit.line),
            result: Ok(content),
        });
        assert_eq!(app.viewer.as_ref().unwrap().target_line, Some(1));
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
