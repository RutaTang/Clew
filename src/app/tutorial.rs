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
    /// The top toolbar.
    Toolbar,
    /// The main reading area.
    Main,
    /// The right context panel (outline / explain).
    RightPanel,
    /// The bottom panel (ask / debug).
    Bottom,
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
        Self { title: title.into(), body: body.into(), anchor, demo: None }
    }
    /// Attaches a live demo Message, dispatched when the step is shown.
    fn demo(mut self, msg: Message) -> Self {
        self.demo = Some(msg);
        self
    }
}

/// A representative source file to open live during the tour, if the project has
/// one (preferring an entry-point-ish name).
fn demo_file(app: &App) -> Option<String> {
    let files = &app.project.as_ref()?.files;
    let source: Vec<&FileEntry> =
        files.iter().filter(|f| crate::highlight::detect(&f.abs).is_some()).collect();
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
        .map(|p| p.root.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .unwrap_or_else(|| "this project".into());
    let open_demo = demo_file(app).map(|rel| Message::OpenRel { rel, line: None });
    let tab = |t: SidebarTab| Message::SidebarTabPicked(t);

    let mut v = vec![
        TutStep::new(
            "Welcome to clew",
            &format!(
                "clew is a reader for code — built for understanding a codebase, not \
                 editing it. This tour walks through every feature, live on {project}. \
                 Use Next / Back to move, or Skip to leave anytime."
            ),
            Anchor::Center,
        ),
        // -- Reading --------------------------------------------------------
        TutStep::new(
            "The file tree",
            "The left sidebar lists the project's files, gitignore-aware — build \
             output and node_modules stay hidden. Click a file to open it, a folder \
             to expand it.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Files)),
    ];

    let mut reading = TutStep::new(
        "The reading view",
        "This is the reader: read-only, tree-sitter syntax highlight, virtualized so \
         even a 100k-line file scrolls smoothly. Click a line to set the reading \
         cursor. It's built for one thing — reading code, with no editing to \
         distract you.",
        Anchor::Main,
    );
    reading.demo = open_demo.clone();
    v.push(reading);

    v.extend([
        TutStep::new(
            "Move like Vim",
            "With the code focused, h / j / k / l move the cursor, w / b jump by \
             word, and g d / g r / g i / g y jump to a symbol's definition / \
             references / implementation / type — no mouse needed. ⌘-click an \
             identifier does the same.",
            Anchor::Main,
        ),
        TutStep::new(
            "The outline",
            "The right panel's Outline lists this file's functions and types; click \
             one to jump straight to it. The panel also hosts Explain.",
            Anchor::RightPanel,
        ),
        // -- Finding --------------------------------------------------------
        TutStep::new(
            "Jump anywhere",
            "⌘P fuzzy-finds any file. ⌘T jumps to any symbol across the whole \
             project. ⌘L goes to a line (or type :123). ⌘F finds within the open \
             file. These four are the fastest way around a codebase.",
            Anchor::Toolbar,
        ),
        TutStep::new(
            "Search the project",
            "The SEARCH tab is a ripgrep-powered full-text search across the project \
             (⌘⇧F), with regex, case, and whole-word options. Click a hit to jump \
             right to that line.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Search)),
        // -- Navigating -----------------------------------------------------
        TutStep::new(
            "Never lose your place",
            "Every jump is remembered. ⌥← / ⌥→ step back and forward through your \
             reading history — like a browser. The TRAIL tab shows the whole \
             navigation tree so you can retrace any path.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Trail)),
        TutStep::new(
            "Split & compare",
            "⌘\\ splits the view into two independent panes — read a caller on the \
             left and its callee on the right, each with its own scroll.",
            Anchor::Main,
        ),
        TutStep::new(
            "Bookmarks",
            "⌘D bookmarks the current line; the MARKS tab collects them (with notes) \
             so you can pin the important spots and jump back anytime. Bookmarks \
             persist with the project.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Marks)),
        TutStep::new(
            "Reading notes",
            "Jot a note on any symbol and mark it “understood” — the NOTES tab tracks \
             your progress through the codebase, so a long read stays organized.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        // -- Understanding with AI -----------------------------------------
        TutStep::new(
            "Explain it",
            "clew can explain the code with an LLM — a summary per function, file and \
             folder, kept fresh incrementally. Run “Explain All” from the ⋯ menu (or \
             the toolbar's Explain), then hover or open the Explain panel to read it. \
             Needs an LLM key in Settings.",
            Anchor::RightPanel,
        ),
        TutStep::new(
            "The architecture overview",
            "The overview home is a generated tour of the whole project — what it \
             does, its core modules, entry points, and a native module map. It's the \
             best place to start on an unfamiliar codebase.",
            Anchor::Main,
        )
        .demo(Message::ShowOverview),
        TutStep::new(
            "Guided walkthroughs",
            "The WALK tab holds guided, code-anchored tours: ask for one on a topic \
             (“how does startup work?”) and clew composes an ordered walk through the \
             real files, step by step.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Walk)),
        TutStep::new(
            "Ask clew",
            "The bottom panel is a Q&A over this codebase: ask “where is auth \
             handled?” and clew answers with citations into the real files. It shares \
             the panel with the debugger.",
            Anchor::Bottom,
        ),
        TutStep::new(
            "Semantic search",
            "The FIND tab searches by meaning, not text: describe what you're looking \
             for (“retry logic with backoff”) and it ranks the closest code by its \
             explanation. Great when you don't know the exact name.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Semantic)),
        // -- Structure ------------------------------------------------------
        TutStep::new(
            "The import graph",
            "The IMPORTS tab shows how files depend on each other — imports and \
             importers of the open file, and a whole-project map with cycles \
             flagged. See the shape of the codebase at a glance.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Imports)),
        TutStep::new(
            "The call graph",
            "The CALLS tab is a call hierarchy: who calls a function, and what it \
             calls, resolved precisely by the language server. Expand it to trace \
             control flow across the project.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Calls)),
        TutStep::new(
            "API docs",
            "The DOCS tab is the project's public API surface — every documented \
             type and function, grouped by file or module, straight from the source \
             (no build step). Click an item to read its doc page.",
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Docs)),
        TutStep::new(
            "Project stats",
            "The Stats home breaks the project down by language — lines of code, file \
             counts, and the language mix — so you know what you're looking at.",
            Anchor::Main,
        )
        .demo(Message::ShowStats),
        // -- Git & time -----------------------------------------------------
        TutStep::new(
            "Why is this here?",
            "Right-click a line and pick “Why is this here?” — clew reads the git \
             history around it and explains, grounded in the actual commits, why that \
             code exists.",
            Anchor::Main,
        ),
        TutStep::new(
            "Time travel",
            "“Time travel” (in the ⋯ menu) scrubs a file — or a single symbol — back \
             through its git history, so you can watch how it evolved commit by \
             commit.",
            Anchor::Main,
        ),
        TutStep::new(
            "Diff vs HEAD",
            "“Diff” shows the open file's uncommitted changes against HEAD, inline in \
             the reader — a quick way to see what's changed while you read.",
            Anchor::Toolbar,
        ),
        // -- Run ------------------------------------------------------------
        TutStep::new(
            "Debug (DAP)",
            "clew is also a debugger: set breakpoints in the gutter and step through \
             a real run — call stack, variables and watches live in the bottom panel, \
             powered by the Debug Adapter Protocol.",
            Anchor::Bottom,
        ),
        // -- Beyond ---------------------------------------------------------
        TutStep::new(
            "Read remote code",
            "“Open Remote…” connects over SSH and reads a codebase on another machine \
             as if it were local — clew runs its server on the remote and streams \
             just what you look at.",
            Anchor::Toolbar,
        ),
        TutStep::new(
            "Make it yours",
            "Settings holds your LLM / embedding keys; Keyboard Shortcuts (in the ⋯ \
             menu) lists every command and lets you rebind it. The native menu bar \
             mirrors it all, and ⌘N opens another project in a new window.",
            Anchor::Toolbar,
        ),
        TutStep::new(
            "That's clew",
            "You've seen every feature — reading, finding, navigating, AI \
             understanding, structure graphs, git, and debugging. Everything is a \
             shortcut away. Now go read some code. 🧵",
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

    /// Make the region a step highlights visible, then run its `demo` (if any)
    /// so the feature shows live.
    fn apply_tutorial_demo(&mut self) -> Task<Message> {
        let Some(step) = self.tutorial else {
            return Task::none();
        };
        let (anchor, demo) = match steps(self).get(step) {
            Some(s) => (s.anchor, s.demo.clone()),
            None => return Task::none(),
        };
        // Open the panel the step points at, so its spotlight has something to
        // frame (idempotent — safe to re-run on Back).
        match anchor {
            Anchor::Sidebar => self.show_left_sidebar = true,
            Anchor::RightPanel => self.show_right_panel = true,
            Anchor::Bottom => self.show_bottom = true,
            _ => {}
        }
        if let Some(msg) = demo {
            return self.update(msg);
        }
        Task::none()
    }
}
