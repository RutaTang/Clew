//! The interactive tutorial: a guided overlay tour of clew's features, anchored
//! on the currently-open project. Each step points at a region of the (fixed)
//! layout and can `demo` a feature by dispatching a real Message, so the user
//! sees it live on their own code. The overlay view lives in `crate::ui`.

use crate::app::prelude::*;
use crate::*;

/// Which region of clew's fixed layout a step highlights, so the callout can be
/// placed next to the thing it describes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    /// No specific region — a centered card (welcome / summary steps).
    Center,
    /// The left sidebar (files / search / tabs).
    Sidebar,
    /// The top bar's left cluster: window controls, back/forward, breadcrumb.
    ToolbarLeft,
    /// A single tool icon in the top-right cluster, `0..=6` left → right
    /// (Overview, Stats, Ask, Debug, Call graph, Import graph, Settings).
    ToolbarIcon(usize),
    /// The "More" (⋯) menu button in the top-right cluster.
    ToolbarMore,
    /// A row (or a run of `count` rows) in the opened ⋯ menu, by index from the
    /// top. Steps using this open the menu so its items are visible.
    ToolbarMenu { first: usize, count: usize },
    /// The main reading area.
    Main,
    /// The right panel's top half — the Explain summary.
    RightTop,
    /// The right panel's bottom half — the Outline.
    RightBottom,
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
            body: body.into(),
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
    let files = &app.project.as_ref()?.files;
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
    let open_demo = demo_rel.map(|rel| Message::OpenRel { rel, line: None });
    let tab = |t: SidebarTab| Message::SidebarTabPicked(t);

    let mut v = vec![
        // -- Intro ----------------------------------------------------------
        TutStep::new(
            "Welcome to clew",
            &format!(
                "clew is a reader for code. It is built for understanding a \
                 codebase, not editing one. This tour walks through every feature, \
                 live on {project}. Use Next and Back to move through it, or Skip to \
                 leave at any time."
            ),
            Anchor::Center,
        ),
        // -- Reading --------------------------------------------------------
        TutStep::new(
            "The file tree",
            &format!(
                "The left sidebar lists every file in {project}. It respects your \
                 .gitignore, so build output and dependencies stay out of the way. \
                 Click a file to open it, or a folder to expand it."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Files)),
    ];

    v.push(
        TutStep::new(
            "The reading view",
            "This is the reader. Your code appears with syntax colors, and it is read \
             only, so nothing changes while you explore. Click any line to place your \
             reading cursor there.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
    );

    v.extend([
        TutStep::new(
            "Move with the keyboard",
            "With the code focused, h, j, k and l move the cursor and w and b jump \
             by word. Press g then d to go to a definition, or g then r to see \
             references. Holding ⌘ and clicking a name does the same.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- The top bar's left cluster -------------------------------------
        TutStep::new(
            "Navigate and retrace",
            "The arrows at the top left move back and forward through everywhere you \
             have been, just like a web browser. The path beside them shows where \
             you are right now. Hold ⌥ with the arrow keys to do the same from the \
             keyboard.",
            Anchor::ToolbarLeft,
        ),
        // -- The right panel: Explain, then Outline (top to bottom) ---------
        TutStep::new(
            "Explain the code",
            "The top of the right panel explains the code for you. clew writes a \
             short summary of each function, file, and folder, and keeps it current \
             as you read. Add an AI key in Settings, then run Explain All from the \
             ⋯ menu to fill it in.",
            Anchor::RightTop,
        ),
        TutStep::new(
            "The outline",
            &format!(
                "Below the explanation is the outline of {file}. It lists every \
                 function and type in the file, and the one you are reading stays \
                 highlighted. Click any entry to jump straight to it."
            ),
            Anchor::RightBottom,
        ),
        TutStep::new(
            "Reading notes",
            &format!(
                "In the outline you can mark a symbol as understood or write a note \
                 on it. The NOTES tab gathers all of them here and tracks how much \
                 of {project} you have worked through."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        // -- Finding --------------------------------------------------------
        TutStep::new(
            "Search the project",
            "The SEARCH tab looks through the full text of every file. Switch on \
             regular expressions, case matching, or whole word when you need them. \
             Click a result to jump to that line, and press ⌘⇧F to open it fast.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Search)),
        TutStep::new(
            "Find by meaning",
            "The FIND tab searches by meaning rather than exact words. Describe what \
             you are after, such as retry logic with backoff, and clew ranks the \
             closest code for you. Reach for it when you do not know the name to \
             search.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Semantic)),
        // -- Navigating -----------------------------------------------------
        TutStep::new(
            "Your reading trail",
            "Every jump you make is remembered. The TRAIL tab lays your whole path \
             out as a tree, so you can see where you have been and retrace any \
             branch. Backtrack and explore elsewhere, and the earlier path is still \
             kept.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Trail)),
        TutStep::new(
            "Bookmarks",
            "Press ⌘D to bookmark the line you are on. The MARKS tab collects your \
             bookmarks, and you can attach a note to each. They stay with the \
             project, so your important spots are waiting next time.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Marks)),
        TutStep::new(
            "Guided walkthroughs",
            "The WALK tab builds a guided tour of the code. Ask about a topic, such \
             as how startup works, and clew lays out an ordered walk through the \
             real files, one step at a time. It can also narrate the changes on \
             your current branch.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Walk)),
        // -- Structure (per open file) --------------------------------------
        TutStep::new(
            "Imports for this file",
            &format!(
                "The IMPORTS tab focuses on the open file. It shows what {file} \
                 imports and what imports it, and it can open the whole project map \
                 with any cycles marked."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Imports)),
        TutStep::new(
            "Calls for this file",
            "The CALLS tab is a call hierarchy for the symbol under your cursor. It \
             shows who calls it and what it calls, resolved precisely for you. \
             Expand it to trace the flow across the project.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Calls)),
        TutStep::new(
            "API docs",
            &format!(
                "The DOCS tab gathers the public API of {project}. Every documented \
                 type and function is listed, grouped by file, straight from the \
                 source with no build step. Click an item to read its full page."
            ),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Docs)),
        // -- Working in the reader ------------------------------------------
        TutStep::new(
            "Split and compare",
            "Press ⌘\\ to split the reader into two panes. Read a caller on one side \
             and what it calls on the other, each with its own scroll. Press it \
             again to return to a single pane.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        TutStep::new(
            "Why is this here?",
            "Right click any line and choose Why is this here. clew reads the git \
             history around it and explains why the code exists, grounded in the \
             commits that actually shaped it.",
            Anchor::Main,
        )
        .demo_opt(open_demo.clone()),
        // -- The tool bar, left to right: one step per icon (the ⋯ and the
        //    panel toggle are skipped, they explain themselves). Each spotlights
        //    its own icon and, where it helps, opens the tool live.
        TutStep::new(
            "The overview",
            &format!(
                "The first icon opens the overview, a written tour of the whole \
                 project. It covers what {project} does, its main modules and entry \
                 points, and a map you can explore. It is the best place to begin on \
                 code you have never seen."
            ),
            Anchor::ToolbarIcon(0),
        )
        .demo(Message::ShowOverview),
        TutStep::new(
            "Project stats",
            &format!(
                "The chart icon opens the stats page. It breaks {project} down by \
                 language, with lines of code, file counts, and the language mix, so \
                 you know at a glance what you are looking at."
            ),
            Anchor::ToolbarIcon(1),
        )
        .demo(Message::ShowStats),
        TutStep::new(
            "Ask clew",
            "The speech icon opens Ask in the bottom panel. Put a question about the \
             codebase to it, such as where is auth handled, and clew answers with \
             links that jump straight into the real files.",
            Anchor::ToolbarIcon(2),
        )
        .demo(Message::BottomTabPicked(crate::BottomTab::Ask)),
        TutStep::new(
            "Debug",
            "The bug icon opens the debugger in the bottom panel. Set a breakpoint \
             in the gutter, start a run, and step through it. The call stack, \
             variables, and watches all show here.",
            Anchor::ToolbarIcon(3),
        )
        .demo(Message::BottomTabPicked(crate::BottomTab::Debug)),
        TutStep::new(
            "The call graph",
            &format!(
                "The next icon opens the whole-project call graph. It draws how \
                 functions across {project} call one another, so you can see the \
                 flow of control at a glance."
            ),
            Anchor::ToolbarIcon(4),
        ),
        TutStep::new(
            "The import graph",
            "The icon beside it opens the whole-project import graph. It maps how \
             every file depends on the others, with any cycles marked, so the shape \
             of the codebase is there in one picture.",
            Anchor::ToolbarIcon(5),
        ),
        TutStep::new(
            "Settings",
            "The last icon opens Settings. Your AI and embedding keys live here, \
             along with the reading preferences. clew keeps them per project, so \
             each codebase can read the way it should.",
            Anchor::ToolbarIcon(6),
        ),
        // -- The ⋯ menu, opened: introduce each item top to bottom. These steps
        //    open the menu (see `apply_tutorial_demo`) so the spotlight lands on
        //    real rows. Indices match the menu order in `ui::toolbar::tools_menu`.
        TutStep::new(
            "The more menu",
            "Everything that does not need a place on the bar lives in the ⋯ menu. \
             It is open now. The next steps go through each item in it, from the \
             top.",
            Anchor::ToolbarMore,
        ),
        TutStep::new(
            "Reading toggles",
            "The top four items switch reading aids on and off. Summaries and File \
             summary show clew's one-line explanations in the code, Inlay hints add \
             type and parameter labels, and Minimap shows the scroll overview down \
             the right edge.",
            Anchor::ToolbarMenu { first: 0, count: 4 },
        ),
        TutStep::new(
            "This tour",
            "Tutorial opens this very walkthrough. You can replay it from here \
             whenever you want a refresher on a feature.",
            Anchor::ToolbarMenu { first: 4, count: 1 },
        ),
        TutStep::new(
            "Open a folder",
            "Open Folder points clew at another project on this machine. It opens in \
             place, and ⌘N opens a project in a new window instead.",
            Anchor::ToolbarMenu { first: 5, count: 1 },
        ),
        TutStep::new(
            "Read remote code",
            "Open Remote connects to another machine over SSH and reads its code as \
             if it were your own. clew runs quietly on the far side and streams only \
             what you look at.",
            Anchor::ToolbarMenu { first: 6, count: 1 },
        ),
        TutStep::new(
            "Explain All",
            "Explain All fills the whole project with AI summaries, one per \
             function, file, and folder. It runs in the background and feeds the \
             Explain panel and the Find tab. It needs an AI key in Settings.",
            Anchor::ToolbarMenu { first: 7, count: 1 },
        ),
        TutStep::new(
            "Walkthrough",
            "Walkthrough is the guided tours from the WALK tab, one click away. Ask \
             for a topic and clew builds an ordered walk through the real files.",
            Anchor::ToolbarMenu { first: 8, count: 1 },
        ),
        TutStep::new(
            "Skim",
            &format!(
                "Skim folds every function body in {file}, leaving just the \
                 signatures. It is the fastest way to take in the shape of a long \
                 file before you read it."
            ),
            Anchor::ToolbarMenu { first: 9, count: 1 },
        ),
        TutStep::new(
            "Diff vs HEAD",
            "Diff shows what you have changed in the open file since the last \
             commit, right inside the reader. It is a quick way to see edits made in \
             another tool while you read.",
            Anchor::ToolbarMenu {
                first: 10,
                count: 1,
            },
        ),
        TutStep::new(
            "Time Travel",
            "Time Travel scrubs a file back through its history. Drag along the \
             timeline and watch how the whole file, or a single function, changed \
             commit by commit.",
            Anchor::ToolbarMenu {
                first: 11,
                count: 1,
            },
        ),
        TutStep::new(
            "Language servers",
            "LSP Servers shows the language servers clew runs for you. Definitions, \
             references, and the call graph all come from these, and you can restart \
             one here if it ever gets stuck.",
            Anchor::ToolbarMenu {
                first: 12,
                count: 1,
            },
        ),
        TutStep::new(
            "Keyboard shortcuts",
            "Keyboard Shortcuts lists every command in clew and lets you rebind any \
             of them. The whole app is reachable from the keyboard.",
            Anchor::ToolbarMenu {
                first: 13,
                count: 1,
            },
        ),
        TutStep::new(
            "That's clew",
            "That is the whole tour. You have seen reading, finding, navigating, AI \
             explanations, the graphs, git history, the debugger, and everything in \
             the ⋯ menu. It is all a click or a shortcut away. Now go read some \
             code. 🧵",
            Anchor::Center,
        ),
    ]);

    v
}

impl App {
    /// Begin the tour at step 0 (from the ⋯ menu).
    pub(crate) fn on_tutorial_start(&mut self) -> Task<Message> {
        self.show_tools_menu = false;
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

    /// Jump to a specific step.
    pub(crate) fn on_tutorial_goto(&mut self, step: usize) -> Task<Message> {
        if self.tutorial.is_none() || step >= steps(self).len() {
            return Task::none();
        }
        self.tutorial = Some(step);
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
        self.show_right_panel = matches!(anchor, Anchor::RightTop | Anchor::RightBottom);
        self.show_bottom = matches!(demo, Some(Message::BottomTabPicked(_)));
        // The ⋯-menu steps open the menu (expanded) so their spotlight has the
        // real items to point at; every other step keeps it closed.
        self.show_tools_menu = matches!(anchor, Anchor::ToolbarMore | Anchor::ToolbarMenu { .. });

        let mut tasks = Vec::new();
        // Steps that feature a sidebar tab select it through their demo; every
        // other step resets the sidebar to the file tree, so a tab a later step
        // opened does not linger when you step back to an earlier one.
        if !matches!(demo, Some(Message::SidebarTabPicked(_))) {
            self.sidebar = SidebarTab::Files;
            tasks.push(crate::ui::reveal_sidebar_tab(SidebarTab::Files));
        }
        if let Some(msg) = demo {
            tasks.push(self.update(msg));
        }
        Task::batch(tasks)
    }
}
