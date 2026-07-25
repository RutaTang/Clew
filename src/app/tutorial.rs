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
    // Attaches a live demo Message to a step (wired up per-step as content grows).
    #[allow(dead_code)]
    fn demo(mut self, msg: Message) -> Self {
        self.demo = Some(msg);
        self
    }
}

/// The tour script. Takes `&App` so steps can weave in the real project (its
/// name, its files). The tour is only ever started with a project open.
pub(crate) fn steps(app: &App) -> Vec<TutStep> {
    let project = app
        .project
        .as_ref()
        .map(|p| p.root.file_name().unwrap_or_default().to_string_lossy().into_owned())
        .unwrap_or_else(|| "this project".into());

    vec![
        TutStep::new(
            "Welcome to clew",
            &format!(
                "clew is a reader for code — built for understanding a codebase, not \
                 editing it. This quick tour walks through every feature, right here \
                 on {project}. Use Next / Back, or Skip to leave anytime."
            ),
            Anchor::Center,
        ),
        TutStep::new(
            "The file tree",
            "The left sidebar lists the project's files, gitignore-aware (build \
             output and node_modules stay hidden). Click a file to open it; click a \
             folder to expand it.",
            Anchor::Sidebar,
        ),
        TutStep::new(
            "The reading view",
            "The main area shows the file: read-only, tree-sitter syntax highlight, \
             virtualized so even a 100k-line file scrolls smoothly. Click a line to \
             place the reading cursor; ⌘-click an identifier to jump to its \
             definition. No editing, no distractions — just reading.",
            Anchor::Main,
        ),
        TutStep::new(
            "Jump anywhere — the finder",
            "Press ⌘P to fuzzy-find any file, ⌘T to jump to any symbol \
             (function / struct / class) across the project, and ⌘L to go to a \
             line. It's the fastest way to move around.",
            Anchor::Toolbar,
        ),
        TutStep::new(
            "Understand it — Explain",
            "clew can explain the code with an LLM: a summary per function, file, \
             and folder, rolled up into a project overview. The toolbar's Explain \
             button (and “Explain All” in the ⋯ menu) drive it.",
            Anchor::RightPanel,
        ),
        TutStep::new(
            "Ask clew",
            "The bottom panel is a Q&A over this codebase: ask “where is auth \
             handled?” and clew answers with citations into the real files. It \
             shares the panel with the debugger (DAP).",
            Anchor::Bottom,
        ),
        TutStep::new(
            "That's the tour",
            "You've seen the essentials. Everything here is a keyboard shortcut away \
             — open the ⋯ menu or Keyboard Shortcuts to explore more. Happy reading!",
            Anchor::Center,
        ),
    ]
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

    /// Run the current step's `demo` (if any), so the feature shows live.
    fn apply_tutorial_demo(&mut self) -> Task<Message> {
        let Some(step) = self.tutorial else {
            return Task::none();
        };
        if let Some(msg) = steps(self).get(step).and_then(|s| s.demo.clone()) {
            return self.update(msg);
        }
        Task::none()
    }
}
