//! The interactive tutorial: a guided overlay tour of clew's features, anchored
//! on the currently-open project. Each step points at a region of the (fixed)
//! layout and can `demo` a feature by dispatching a real Message, so the user
//! sees it live on their own code. The overlay view lives in `crate::ui`.
//!
//! Its messages, [`TutorialMsg`], arrive through `App::update_tutorial`.

use crate::app::prelude::*;
use crate::ui::{StripTool, ToolsRow};
use crate::*;

/// Whether `message` is one a ⋯-menu row emits — picking any row dismisses
/// the menu. Covers every row of `ui::TOOLS_MENU_ROWS` (a test walks them
/// all), so the dismissal cannot drift from the menu the way the old partial
/// whitelist in the dispatcher did.
pub(crate) fn closes_tools_menu(message: &Message) -> bool {
    matches!(
        message,
        Message::Tutorial(TutorialMsg::Start)
            | Message::Project(ProjectMsg::OpenFolderPressed)
            | Message::Connect(ConnectMsg::Open)
            | Message::Explain(ExplainMsg::Cancel)
            | Message::Settings(SettingsMsg::Open | SettingsMsg::OpenAt(_))
            | Message::Explain(ExplainMsg::Project)
            | Message::Project(ProjectMsg::SetupShow(_))
            | Message::Window(WindowMsg::SidebarTabPicked(_))
            | Message::Lsp(LspMsg::TogglePanel)
            | Message::Window(WindowMsg::OpenShortcuts)
            | Message::Glossary(GlossaryMsg::Open)
            | Message::Export(ExportMsg::Start)
    )
}

/// The spotlight anchor for one row of the ⋯ menu.
/// The ⋯ menu's rows from `first` to `last`, as one place to point at.
fn menu_rows(first: ToolsRow, last: ToolsRow) -> Anchor {
    Anchor::ToolbarMenu {
        first: first.index(),
        count: last.index() + 1 - first.index(),
    }
}

/// Which region of clew's fixed layout a step highlights, so the callout can be
/// placed next to the thing it describes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// No specific region — a centered card (welcome / summary steps).
    Center,
    /// The left sidebar (files / search / tabs).
    Sidebar,
    /// The top bar's left cluster: the window buttons (or the app menu),
    /// the sidebar toggle, back / forward.
    ToolbarLeft,
    /// Ask's corner: its card, or the pill it hides to.
    AskCorner,
    /// A tool on the strip down the window's right edge.
    Strip(StripTool),
    /// A row (or a run of `count` rows) in the opened ⋯ menu, by index from the
    /// top. Steps using this open the menu so its items are visible.
    ToolbarMenu { first: usize, count: usize },
    /// The file tools at the top of the sheet (Explain, Skim, Diff, Time
    /// Travel, View).
    FileTools,
    /// The opened View menu of reading aids. Steps using this open it.
    ViewMenu,
    /// The main reading area.
    Main,
    /// The margin beside the code, with its notes.
    RightTop,
}

/// One step of the tour.
pub struct TutStep {
    pub title: String,
    pub body: String,
    pub anchor: Anchor,
    /// A Message dispatched when the step is shown, to demo the feature live.
    pub demo: Option<Message>,
}

impl TutStep {
    fn new(title: &str, body: &str, anchor: Anchor) -> Self {
        Self {
            title: title.into(),
            // The shortcuts it names, as this platform spells them.
            body: crate::keymap::local_keys(body),
            anchor,
            demo: None,
        }
    }
    /// Attaches a live demo Message, dispatched when the step is shown.
    fn demo(mut self, msg: Message) -> Self {
        self.demo = Some(msg);
        self
    }
    /// Attaches an optional demo Message (a no-op when `None`).
    fn demo_opt(mut self, msg: Option<Message>) -> Self {
        self.demo = msg;
        self
    }
}

/// A representative source file to open live during the tour, if the project has
/// one (preferring an entry-point-ish name).
fn demo_file(app: &App) -> Option<String> {
    let files = &app.proj.project.as_ref()?.files;
    let source: Vec<&FileEntry> = files
        .iter()
        .filter(|f| crate::highlight::detect(&f.abs).is_some())
        .collect();
    let pick = source
        .iter()
        .find(|f| {
            let stem = f.abs.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            matches!(stem, "main" | "lib" | "index" | "mod" | "app")
        })
        .or_else(|| source.first())?;
    Some(pick.rel.clone())
}

/// The tour script. Takes `&App` so steps can weave in the real project (its
/// name, its files) and demo features live. The tour is only ever started with a
/// project open.
pub(crate) fn steps(app: &App) -> Vec<TutStep> {
    let project = app
        .proj
        .project
        .as_ref()
        .map(|p| {
            p.root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| tr!("this project").into());
    let demo_rel = demo_file(app);
    let file = demo_rel
        .as_deref()
        .and_then(|rel| rel.rsplit('/').next())
        .unwrap_or(tr!("this file"))
        .to_string();
    let open_demo = demo_rel.map(|rel| Message::Editor(EditorMsg::OpenRel { rel, line: None }));
    let tab = |t: SidebarTab| Message::Window(WindowMsg::SidebarTabPicked(t));

    let mut v = vec![
        // -- Intro ----------------------------------------------------------
        TutStep::new(
            tr!("Welcome to Clew"),
            &tr!(
                "A reader for code, not an editor. A quick tour of {project}.",
                project = project
            ),
            Anchor::Center,
        ),
        // -- Reading --------------------------------------------------------
        TutStep::new(
            tr!("The file tree"),
            &tr!(
                "Every file in {project}, minus what .gitignore hides.",
                project = project
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Files)),
    ];

    v.push(
        TutStep::new(
            tr!("The reading view"),
            tr!("Your code, read-only. Click a line to place the cursor."),
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
    );

    v.extend([
        TutStep::new(
            tr!("Move with the keyboard"),
            tr!("h j k l move, w and b jump by word. g d goes to a definition, g r to references."),
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- The top bar's left cluster -------------------------------------
        TutStep::new(
            tr!("Navigate and retrace"),
            tr!("Go back and forward through the places you've been. The line above the code shows how you got here."),
            Anchor::ToolbarLeft,
        ),
        // -- The margin beside the code, then the outline --------------------
        TutStep::new(
            tr!("Notes beside the code"),
            tr!("Clew's note on each function sits beside it. Explain All fills them in."),
            Anchor::RightTop,
        ),
        TutStep::new(
            tr!("The outline"),
            &tr!("Every function and type in {file}.", file = file),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        TutStep::new(
            tr!("Reading notes"),
            tr!("Mark symbols as understood, or add notes to them. They're all collected here."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        // -- Finding --------------------------------------------------------
        TutStep::new(
            tr!("Search the project"),
            tr!("Full-text search across every file."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Search)),
        TutStep::new(
            tr!("Find by meaning"),
            tr!("Search by what code does, not by its name."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Semantic)),
        // -- Navigating -----------------------------------------------------
        TutStep::new(
            tr!("Your reading trail"),
            tr!("Every jump you made, as a tree."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Trail)),
        TutStep::new(
            tr!("Bookmarks"),
            tr!("The lines you bookmark, to come back to."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Marks)),
        TutStep::new(
            tr!("Guided walkthroughs"),
            tr!("Pick a topic, and Clew walks you through the files."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Walk)),
        // -- Structure (per open file) --------------------------------------
        TutStep::new(
            tr!("Imports for this file"),
            &tr!("What {file} imports, and what imports it.", file = file),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Imports)),
        TutStep::new(
            tr!("Calls for this file"),
            tr!("Who calls the symbol at the cursor, and what it calls."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Calls)),
        TutStep::new(
            tr!("API docs"),
            &tr!("The public API of {project}, read from the source.", project = project),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Docs)),
        // -- Working in the reader ------------------------------------------
        TutStep::new(
            tr!("Split and compare"),
            tr!("Read two files side by side."),
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        TutStep::new(
            tr!("Why is this here?"),
            tr!("Right-click a line, and Clew explains it from git history."),
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- Talking to clew: Ask, in the corner ----------------------------
        TutStep::new(
            tr!("Ask Clew, or tell it what to do"),
            tr!("Ask about the project, or tell Clew what to do. It asks before running anything."),
            Anchor::AskCorner,
        )
        .demo(Message::Ask(AskMsg::Open)),
        // -- The file tools, over the code -----------------------------------
        TutStep::new(
            tr!("Tools for this file"),
            &tr!("Explain, Skim, Diff and Time Travel for {file}.", file = file),
            Anchor::FileTools,
        )
        .demo_opt(open_demo.clone()),
        TutStep::new(
            tr!("Reading aids"),
            tr!("Turn summaries, inlay hints and the minimap on or off."),
            Anchor::ViewMenu,
        )
        .demo_opt(open_demo.clone()),
        // -- The tool strip, top to bottom: one step per tool ----------------
        TutStep::new(
            tr!("The overview"),
            &tr!(
                "Tools for the whole project. First, an overview of {project}.",
                project = project
            ),
            Anchor::Strip(StripTool::Overview),
        )
        .demo(Message::Overview(OverviewMsg::Show)),
        TutStep::new(
            tr!("Project stats"),
            &tr!("{project} by language and size.", project = project),
            Anchor::Strip(StripTool::Stats),
        )
        .demo(Message::Overview(OverviewMsg::ShowStats)),
        TutStep::new(
            tr!("Debug"),
            tr!("Breakpoints, stepping, the stack and variables."),
            Anchor::Strip(StripTool::Debug),
        )
        .demo(Message::Window(WindowMsg::ShowDebugPanel)),
        TutStep::new(
            tr!("The call graph"),
            &tr!("How functions across {project} call each other.", project = project),
            Anchor::Strip(StripTool::CallGraph),
        ),
        TutStep::new(
            tr!("The import graph"),
            tr!("How files depend on each other, with cycles marked."),
            Anchor::Strip(StripTool::ImportGraph),
        ),
        TutStep::new(
            tr!("The type map"),
            tr!("How the project's types relate."),
            Anchor::Strip(StripTool::TypeMap),
        ),
        TutStep::new(
            tr!("Project maps"),
            tr!("Schemas, routes, supervision trees and more, read from the code."),
            Anchor::Strip(StripTool::ProjectMaps),
        ),
        TutStep::new(
            tr!("Settings"),
            tr!("API keys and reading preferences."),
            Anchor::Strip(StripTool::Settings),
        ),
        // -- The ⋯ menu, opened: introduce each item top to bottom. These steps
        //    open the menu (see `apply_tutorial_demo`) so the spotlight lands on
        //    real rows, named by `ui::ToolsRow` — the list the menu is built from.
        TutStep::new(
            tr!("The More menu"),
            tr!("Everything else lives here."),
            Anchor::Strip(StripTool::More),
        ),
        // The menu's rows a group at a time, as the menu groups them: one
        // step a row made the tour forty-one steps long.
        TutStep::new(
            tr!("Understand the project"),
            tr!("Explain All writes a note for every function, file and folder (it needs an API key); walkthroughs guide you through the code; the glossary defines the project's own terms."),
            menu_rows(ToolsRow::ExplainAll, ToolsRow::Glossary),
        ),
        TutStep::new(
            tr!("Open another project"),
            tr!("A folder on this machine, or code on another over SSH."),
            menu_rows(ToolsRow::OpenFolder, ToolsRow::OpenRemote),
        ),
        TutStep::new(
            tr!("And the rest"),
            tr!("Export your notes as one Markdown file, set the project up again, restart a language server, look up a shortcut, or take this tour again."),
            menu_rows(ToolsRow::ExportNotes, ToolsRow::Tutorial),
        ),
        TutStep::new(
            tr!("That's Clew"),
            tr!("That's the tour. Go read some code. 🧵"),
            Anchor::Center,
        ),
    ]);

    v
}

/// The layout a tour began from ([`App::on_tutorial_exit`] gives it back).
#[derive(Debug, Clone)]
pub(crate) struct TourLayout {
    left: bool,
    right: bool,
    bottom: bool,
    ask: AskView,
    sidebar: SidebarTab,
    /// The pages over the code (the overview, the statistics, the glossary,
    /// a doc page): the tour left the last it showed — the statistics.
    pages: (bool, bool, bool),
    docs: Option<crate::DocPage>,
    /// The file being read and the line, and the trail to it: the tour's
    /// files were left open, and in the trail.
    reading: Option<(PathBuf, Option<usize>)>,
    trail: crate::history::History,
}

impl App {
    /// Begin the tour at step 0 (from the ⋯ menu, which the dispatcher has
    /// already closed).
    pub(crate) fn on_tutorial_start(&mut self) -> Task<Message> {
        // The reader's own layout, to give back at the end (a tour started
        // again while one runs keeps the first one's).
        if self.tutorial.is_none() {
            let line_h = self.line_height();
            let reading = self.active_viewer().map(|v| {
                let line = match v.caret {
                    Some((line, _)) => line + 1,
                    None => (v.scroll_y / line_h.max(1.0)) as usize + 1,
                };
                (v.abs.clone(), Some(line))
            });
            self.tutorial_layout = Some(TourLayout {
                left: self.show_left_sidebar,
                right: self.show_right_panel,
                bottom: self.show_bottom,
                ask: self.ask_view,
                sidebar: self.sidebar,
                pages: (
                    self.proj.overview.showing,
                    self.proj.stats.showing,
                    self.proj.glossary.showing,
                ),
                docs: self.proj.docs.page.clone(),
                reading,
                trail: self.proj.history.clone(),
            });
        }
        self.tutorial = Some(0);
        self.apply_tutorial_demo()
    }

    /// Advance / rewind the tour. Stepping past the last step ends it.
    pub(crate) fn on_tutorial_step(&mut self, delta: i32) -> Task<Message> {
        let Some(cur) = self.tutorial else {
            return Task::none();
        };
        let total = steps(self).len() as i32;
        let next = cur as i32 + delta;
        if next < 0 {
            return Task::none();
        }
        if next >= total {
            return self.on_tutorial_exit();
        }
        self.tutorial = Some(next as usize);
        self.apply_tutorial_demo()
    }

    /// End the tour, the reader's layout as it was before it: each step set
    /// the whole layout its spotlight needed, and the last one's stayed —
    /// the margin hidden, the sidebar on another view, a menu open.
    pub(crate) fn on_tutorial_exit(&mut self) -> Task<Message> {
        self.tutorial = None;
        self.close_menus();
        self.show_tools_menu = false;
        self.show_view_menu = false;
        let Some(layout) = self.tutorial_layout.take() else {
            return Task::none();
        };
        self.show_left_sidebar = layout.left;
        self.show_right_panel = layout.right;
        self.show_bottom = layout.bottom;
        self.ask_view = layout.ask;
        self.sidebar = layout.sidebar;
        // The reader's place, as it was: the file and line, and the trail
        // to them — none of the tour's files in it — then the page over
        // them, if one was.
        self.proj.history.restore(layout.trail);
        let back = match layout.reading {
            Some((abs, line)) if self.active_viewer().is_none_or(|v| v.abs != abs) => {
                self.open_file(abs, line, false)
            }
            _ => Task::none(),
        };
        (
            self.proj.overview.showing,
            self.proj.stats.showing,
            self.proj.glossary.showing,
        ) = layout.pages;
        self.proj.docs.page = layout.docs;
        back
    }

    /// Set the whole layout this step needs, then run its `demo` (if any) so the
    /// feature shows live.
    ///
    /// The layout is set in full every step — not just "open what this step
    /// points at" — so the structure is deterministic whether you arrive by Next
    /// or Back. A panel a step does not use is closed, not left open by an
    /// earlier step (e.g. the outline step must not still show the bottom panel a
    /// later Ask/Debug step opened, which would also throw off its spotlight).
    fn apply_tutorial_demo(&mut self) -> Task<Message> {
        let Some(step) = self.tutorial else {
            return Task::none();
        };
        let (anchor, demo) = match steps(self).get(step) {
            Some(s) => (s.anchor, s.demo.clone()),
            None => return Task::none(),
        };
        // The file tree is always available. The right panel is shown only while
        // its own two steps are on screen (so the reader is uncluttered before
        // and after). The bottom panel is shown only for the steps that demo it.
        self.show_left_sidebar = true;
        self.show_right_panel = matches!(anchor, Anchor::RightTop);
        self.show_bottom = matches!(demo, Some(Message::Window(WindowMsg::ShowDebugPanel)));
        self.ask_view = if matches!(demo, Some(Message::Ask(AskMsg::Open))) {
            AskView::Compact
        } else {
            AskView::Hidden
        };
        // The ⋯-menu steps open the menu so their spotlight has the real
        // items to point at, and the View menu's step that menu; every other
        // step keeps them closed.
        self.close_menus();
        // (Not where its rows are on the strip: there they are pointed at.)
        self.show_tools_menu = !crate::ui::tools_on_strip(self)
            && matches!(
                anchor,
                Anchor::Strip(StripTool::More) | Anchor::ToolbarMenu { .. }
            );
        self.show_view_menu = matches!(anchor, Anchor::ViewMenu);

        let mut tasks = Vec::new();
        // Steps that feature a sidebar tab select it through their demo; every
        // other step resets the sidebar to the file tree, so a tab a later step
        // opened does not linger when you step back to an earlier one.
        if !matches!(demo, Some(Message::Window(WindowMsg::SidebarTabPicked(_)))) {
            self.sidebar = SidebarTab::Files;
        }
        if let Some(msg) = demo {
            tasks.push(self.update(msg));
        }
        // No text field may hold the keyboard while the tour runs: one that
        // did (the SEARCH box, or the FIND box in its place) took the → and
        // Enter that move the tour, so it could not be stepped by keyboard.
        // Focusing an id no widget has unfocuses every one.
        tasks.push(operation::focus(iced::widget::Id::new(
            "tutorial-holds-no-focus",
        )));
        Task::batch(tasks)
    }
}

impl App {
    /// Handle a [`TutorialMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_tutorial(&mut self, message: TutorialMsg) -> Task<Message> {
        match message {
            TutorialMsg::Start => self.on_tutorial_start(),
            TutorialMsg::Step(delta) => self.on_tutorial_step(delta),
            TutorialMsg::Exit => self.on_tutorial_exit(),
        }
    }
}
