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
            | Message::Settings(SettingsMsg::Open)
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
fn menu_row(row: ToolsRow) -> Anchor {
    Anchor::ToolbarMenu {
        first: row.index(),
        count: 1,
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
        .unwrap_or_else(|| "this project".into());
    let demo_rel = demo_file(app);
    let file = demo_rel
        .as_deref()
        .and_then(|rel| rel.rsplit('/').next())
        .unwrap_or("this file")
        .to_string();
    let open_demo = demo_rel.map(|rel| Message::Editor(EditorMsg::OpenRel { rel, line: None }));
    let tab = |t: SidebarTab| Message::Window(WindowMsg::SidebarTabPicked(t));

    let mut v = vec![
        // -- Intro ----------------------------------------------------------
        TutStep::new(
            "Welcome to clew",
            &format!("A reader for code, not an editor. A quick tour of {project}."),
            Anchor::Center,
        ),
        // -- Reading --------------------------------------------------------
        TutStep::new(
            "The file tree",
            &format!("Every file in {project}, minus what .gitignore hides."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Files)),
    ];

    v.push(
        TutStep::new(
            "The reading view",
            "Your code, read-only. Click a line to place the cursor.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
    );

    v.extend([
        TutStep::new(
            "Move with the keyboard",
            "h j k l move, w and b jump by word. g d goes to a definition, g r to references.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- The top bar's left cluster -------------------------------------
        TutStep::new(
            "Navigate and retrace",
            "Back and forward through where you have been. The line above the code shows how you got here.",
            Anchor::ToolbarLeft,
        ),
        // -- The margin beside the code, then the outline --------------------
        TutStep::new(
            "Notes beside the code",
            "clew's note on each function, beside it. Explain All fills them in.",
            Anchor::RightTop,
        ),
        TutStep::new(
            "The outline",
            &format!("Every function and type in {file}."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        TutStep::new(
            "Reading notes",
            "Mark symbols understood, or note them. They gather here.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        // -- Finding --------------------------------------------------------
        TutStep::new(
            "Search the project",
            "Full-text search across every file.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Search)),
        TutStep::new(
            "Find by meaning",
            "Search by what code does, not by its name.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Semantic)),
        // -- Navigating -----------------------------------------------------
        TutStep::new(
            "Your reading trail",
            "Every jump you made, as a tree.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Trail)),
        TutStep::new(
            "Bookmarks",
            "⌘D bookmarks the line you are on.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Marks)),
        TutStep::new(
            "Guided walkthroughs",
            "Pick a topic, and clew walks you through the files.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Walk)),
        // -- Structure (per open file) --------------------------------------
        TutStep::new(
            "Imports for this file",
            &format!("What {file} imports, and what imports it."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Imports)),
        TutStep::new(
            "Calls for this file",
            "Who calls the symbol at the cursor, and what it calls.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Calls)),
        TutStep::new(
            "API docs",
            &format!("The public API of {project}, read from the source."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Docs)),
        // -- Working in the reader ------------------------------------------
        TutStep::new(
            "Split and compare",
            "⌘\\ splits the reader in two.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        TutStep::new(
            "Why is this here?",
            "Right-click a line, and clew explains it from git history.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- Talking to clew: Ask, in the corner ----------------------------
        TutStep::new(
            "Ask clew, or tell it what to do",
            "Ask about the project, or tell clew what to do. It asks before running anything.",
            Anchor::AskCorner,
        )
        .demo(Message::Ask(AskMsg::Open)),
        // -- The file tools, over the code -----------------------------------
        TutStep::new(
            "Tools for this file",
            &format!("Explain, Skim, Diff and Time Travel for {file}."
            ),
            Anchor::FileTools,
        )
        .demo_opt(open_demo.clone()),
        TutStep::new(
            "Reading aids",
            "Summaries, inlay hints and the minimap, on or off.",
            Anchor::ViewMenu,
        )
        .demo_opt(open_demo.clone()),
        // -- The tool strip, top to bottom: one step per tool ----------------
        TutStep::new(
            "The overview",
            &format!("Tools for the whole project. First, an overview of {project}."
            ),
            Anchor::Strip(StripTool::Overview),
        )
        .demo(Message::Overview(OverviewMsg::Show)),
        TutStep::new(
            "Project stats",
            &format!("{project} by language and size."
            ),
            Anchor::Strip(StripTool::Stats),
        )
        .demo(Message::Overview(OverviewMsg::ShowStats)),
        TutStep::new(
            "Debug",
            "Breakpoints, stepping, the stack and variables.",
            Anchor::Strip(StripTool::Debug),
        )
        .demo(Message::Window(WindowMsg::ShowDebugPanel)),
        TutStep::new(
            "The call graph",
            &format!("How functions across {project} call each other."
            ),
            Anchor::Strip(StripTool::CallGraph),
        ),
        TutStep::new(
            "The import graph",
            "How files depend on each other, cycles marked.",
            Anchor::Strip(StripTool::ImportGraph),
        ),
        TutStep::new(
            "The type map",
            "How the project's types relate.",
            Anchor::Strip(StripTool::TypeMap),
        ),
        TutStep::new(
            "Project maps",
            "Schemas, routes, supervision trees and more, read from the code.",
            Anchor::Strip(StripTool::ProjectMaps),
        ),
        TutStep::new(
            "Settings",
            "AI keys and reading preferences.",
            Anchor::Strip(StripTool::Settings),
        ),
        // -- The ⋯ menu, opened: introduce each item top to bottom. These steps
        //    open the menu (see `apply_tutorial_demo`) so the spotlight lands on
        //    real rows, named by `ui::ToolsRow` — the list the menu is built from.
        TutStep::new(
            "The more menu",
            "Everything else lives here.",
            Anchor::Strip(StripTool::More),
        ),
        TutStep::new(
            "Explain All",
            "AI summaries for every function, file and folder. Needs an AI key.",
            menu_row(ToolsRow::ExplainAll),
        ),
        TutStep::new(
            "Walkthrough",
            "Guided tours through the code.",
            menu_row(ToolsRow::Walkthrough),
        ),
        TutStep::new(
            "Glossary",
            "The project's own terms, defined.",
            menu_row(ToolsRow::Glossary),
        ),
        TutStep::new(
            "Open a folder",
            "Open another project. ⌘N opens it in a new window.",
            menu_row(ToolsRow::OpenFolder),
        ),
        TutStep::new(
            "Read remote code",
            "Read code on another machine over SSH.",
            menu_row(ToolsRow::OpenRemote),
        ),
        TutStep::new(
            "Export notes",
            "Your notes, bookmarks and trail as one Markdown file.",
            menu_row(ToolsRow::ExportNotes),
        ),
        TutStep::new(
            "Set up the project",
            "Language servers and explaining, again any time.",
            menu_row(ToolsRow::SetUp),
        ),
        TutStep::new(
            "Language servers",
            "The servers behind definitions and references. Restart one here.",
            menu_row(ToolsRow::LspServers),
        ),
        TutStep::new(
            "Keyboard shortcuts",
            "Every command, and its keys.",
            menu_row(ToolsRow::Shortcuts),
        ),
        TutStep::new(
            "This tour",
            "Replay this tour any time.",
            menu_row(ToolsRow::Tutorial),
        ),
        TutStep::new(
            "That's clew",
            "That's the tour. Go read some code. 🧵",
            Anchor::Center,
        ),
    ]);

    v
}

impl App {
    /// Begin the tour at step 0 (from the ⋯ menu, which the dispatcher has
    /// already closed).
    pub(crate) fn on_tutorial_start(&mut self) -> Task<Message> {
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
            self.tutorial = None;
            return Task::none();
        }
        self.tutorial = Some(next as usize);
        self.apply_tutorial_demo()
    }

    /// End the tour.
    pub(crate) fn on_tutorial_exit(&mut self) -> Task<Message> {
        self.tutorial = None;
        Task::none()
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
        self.show_tools_menu = matches!(
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
