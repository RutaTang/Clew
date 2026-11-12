//! The interactive tutorial: a guided overlay tour of Clew's features, on
//! Tea Shop, a small project Clew ships for it (`crate::app::tour_sample`).
//! A sheet first gets the tour ready, one step at a time: a language model
//! (needed), Find by meaning (up to the reader); the sample is written
//! meanwhile, and opened — in a window of its own when this one has a
//! project — with its language server and its explanations set up. The
//! tour itself is chapters, in the order one comes to know a project; each
//! step points at a region of the layout and shows its feature live, with
//! real Messages, on the sample's code. The overlay view lives in
//! `crate::ui`.
//!
//! Its messages, [`TutorialMsg`], arrive through `App::update_tutorial`.

use crate::app::prelude::*;
use crate::app::setup::Census;
use crate::app::tour_sample::Placed;
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
            | Message::Explain(ExplainMsg::Cancel)
            | Message::Settings(SettingsMsg::Open | SettingsMsg::OpenAt(_) | SettingsMsg::NeedKey)
            | Message::Explain(ExplainMsg::Project)
            | Message::Project(ProjectMsg::SetupShow(_))
            | Message::Window(WindowMsg::OpenShortcuts)
            | Message::Glossary(GlossaryMsg::Open)
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
    /// The strip's four ways to run (Run, Fake Run, Debug, Fake Debug) —
    /// or the bug they fold into where the strip is short.
    RunTools,
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

/// A chapter of the tour: the steps under one heading, in the order a
/// reader comes to know a project — the whole of it first, then finding a
/// way round it, reading it, following its code, asking about it, its
/// history, keeping one's place in it, and running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chapter {
    /// The welcome, before the first chapter.
    Welcome,
    BigPicture,
    FindYourWay,
    Read,
    Follow,
    Ask,
    History,
    KeepYourPlace,
    Run,
    /// The last card, after the last chapter.
    Done,
}

impl Chapter {
    /// Its heading, in English (shown through `crate::i18n::lookup`); none
    /// for the welcome and the last card.
    pub fn title(self) -> Option<&'static str> {
        Some(match self {
            Chapter::Welcome | Chapter::Done => return None,
            Chapter::BigPicture => tr_noop!("The big picture"),
            Chapter::FindYourWay => tr_noop!("Find your way"),
            Chapter::Read => tr_noop!("Read the code"),
            Chapter::Follow => tr_noop!("Follow the code"),
            Chapter::Ask => tr_noop!("Ask"),
            Chapter::History => tr_noop!("How it came to be"),
            Chapter::KeepYourPlace => tr_noop!("Keep your place"),
            Chapter::Run => tr_noop!("Run it"),
        })
    }
}

/// Where a step puts the caret: on `name`, in the first line of the
/// sample's file `rel` that holds `line_has`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub rel: &'static str,
    pub line_has: &'static str,
    pub name: &'static str,
}

/// One step of the tour.
pub struct TutStep {
    pub chapter: Chapter,
    pub title: String,
    pub body: String,
    pub anchor: Anchor,
    /// Messages dispatched when the step is shown, to show the feature
    /// live.
    pub demos: Vec<Message>,
    /// Where the caret goes first, in the sample.
    pub place: Option<Place>,
    /// Dispatched once the caret is there: what the step shows of the code
    /// at it (who calls the function, say).
    pub then: Option<Message>,
}

impl TutStep {
    fn new(chapter: Chapter, title: &str, body: &str, anchor: Anchor) -> Self {
        Self {
            chapter,
            title: title.into(),
            // The shortcuts it names, as this platform spells them.
            body: crate::keymap::local_keys(body),
            anchor,
            demos: Vec::new(),
            place: None,
            then: None,
        }
    }
    /// Adds a Message dispatched when the step is shown.
    fn demo(mut self, msg: Message) -> Self {
        self.demos.push(msg);
        self
    }
    /// Puts the caret on `name` in `rel`, on the line holding `line_has`.
    fn at(mut self, rel: &'static str, line_has: &'static str, name: &'static str) -> Self {
        self.place = Some(Place {
            rel,
            line_has,
            name,
        });
        self
    }
    /// What to show once the caret is in place.
    fn then(mut self, msg: Message) -> Self {
        self.then = Some(msg);
        self
    }
}

/// The sample's file most steps read: what an order costs.
const PRICING: &str = crate::app::tour_sample::FIXED_FILE;

/// The tour script, on Tea Shop (`crate::app::tour_sample`). The history
/// chapter is left out when the sample was written without its history
/// (no git to write it with).
pub(crate) fn steps(app: &App) -> Vec<TutStep> {
    use Chapter as C;
    let tab = |t: SidebarTab| Message::Window(WindowMsg::SidebarTabPicked(t));
    let history = app.tour_sample.as_ref().is_none_or(|s| s.history);

    let mut v = vec![
        TutStep::new(
            C::Welcome,
            tr!("Welcome to Clew"),
            tr!("Tea Shop is a small online shop, written for this tour. Each step shows one way into code you don't know yet, from the whole project down to why one line is there."),
            Anchor::Center,
        ),
        // -- The big picture ------------------------------------------------
        TutStep::new(
            C::BigPicture,
            tr!("Start with the overview"),
            tr!("What the project does and how its parts fit together, written from its code. In a project you don't know, read this first."),
            Anchor::Strip(StripTool::Overview),
        )
        .demo(Message::Overview(OverviewMsg::Show)),
        TutStep::new(
            C::BigPicture,
            tr!("Its data and its API"),
            tr!("Project Graphs reads the tables in schema.sql and the routes in api.py from the code, and draws them."),
            Anchor::Strip(StripTool::ProjectMaps),
        ),
        TutStep::new(
            C::BigPicture,
            tr!("Its own words"),
            tr!("The glossary explains the project's own terms, such as SKU, member and tin, from how its code uses them."),
            menu_rows(ToolsRow::Glossary, ToolsRow::Glossary),
        ),
        // -- Find your way --------------------------------------------------
        TutStep::new(
            C::FindYourWay,
            tr!("Every file"),
            tr!("The project's folders and files. teashop/pricing.py works out what an order costs; it's open now."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Files))
        .at(PRICING, "def total(", "total"),
        TutStep::new(
            C::FindYourWay,
            tr!("Search the project"),
            tr!("Search every file for a word or a pattern, such as TAX_RATE."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Search)),
        TutStep::new(
            C::FindYourWay,
            tr!("Find by meaning"),
            tr!("Describe what code does, such as \"where is tax added?\", and Clew finds it, even where no name says so."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Semantic)),
        // -- Read -----------------------------------------------------------
        TutStep::new(
            C::Read,
            tr!("Explanations beside the code"),
            tr!("Each function's explanation sits level with its code. Clew wrote Tea Shop's with your model as the tour began."),
            Anchor::RightTop,
        )
        .at(PRICING, "def total(", "total"),
        TutStep::new(
            C::Read,
            tr!("Tools for this file"),
            tr!("Explain the whole file, Skim it to see only its parts, see what changed in it, or go back through its history."),
            Anchor::FileTools,
        )
        .at(PRICING, "def total(", "total"),
        TutStep::new(
            C::Read,
            tr!("Reading aids"),
            tr!("Turn on what helps you read: a summary over each function, the types a name has, a map of the file."),
            Anchor::ViewMenu,
        )
        .at(PRICING, "def total(", "total"),
        TutStep::new(
            C::Read,
            tr!("Its outline, and your notes"),
            tr!("Every function and class in the file. Mark the ones you've understood, or write a note on one."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Notes)),
        // -- Follow the code ------------------------------------------------
        TutStep::new(
            C::Follow,
            tr!("Definitions and uses"),
            tr!("With the cursor on a name, g d goes to where it is defined and g r lists everywhere it is used. ⌘-click works too."),
            Anchor::Main,
        )
        .at(PRICING, "def total(", "total"),
        TutStep::new(
            C::Follow,
            tr!("Who calls what"),
            tr!("Who calls total(), and who calls them, up to the API's place_order(). g c shows this for the name at the cursor."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Calls))
        .at(PRICING, "def total(", "total")
        .then(Message::Calls(CallsMsg::Requested)),
        TutStep::new(
            C::Follow,
            tr!("The call graph"),
            tr!("Every function's calls across the project, as one graph you can move around."),
            Anchor::Strip(StripTool::CallGraph),
        ),
        TutStep::new(
            C::Follow,
            tr!("Types and their kinds"),
            tr!("The Type Graph shows Discount and the two kinds of it the shop has: MemberDiscount and BulkDiscount."),
            Anchor::Strip(StripTool::TypeMap),
        ),
        TutStep::new(
            C::Follow,
            tr!("What imports what"),
            tr!("How the files depend on each other, with any cycle marked."),
            Anchor::Strip(StripTool::ImportGraph),
        ),
        // -- Ask ------------------------------------------------------------
        TutStep::new(
            C::Ask,
            tr!("Ask Clew"),
            tr!("Ask about the project, such as \"why is tax worked out after discounts?\", and Clew answers from its code and shows where. You can also tell it what to do; it asks before running anything."),
            Anchor::AskCorner,
        )
        .demo(Message::Ask(AskMsg::Open)),
        TutStep::new(
            C::Ask,
            tr!("Walk through a topic"),
            tr!("Name a topic, such as \"checkout\", and Clew walks you through the code that does it, in order."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Walk)),
    ];
    if history {
        v.extend([
            TutStep::new(
                C::History,
                tr!("Why is this here?"),
                tr!("Right-click a line and choose Why Is This Here? to see the commit that wrote it, and why. This line came from the fix that taxes the discounted amount, not the whole subtotal."),
                Anchor::Main,
            )
            .at(PRICING, "discounted = ", "discounted"),
            TutStep::new(
                C::History,
                tr!("Time Travel"),
                tr!("Step back through the file's history. Before the fix, total() taxed the whole subtotal."),
                Anchor::FileTools,
            )
            .at(PRICING, "discounted = ", "discounted"),
        ]);
    }
    v.extend([
        // -- Keep your place ------------------------------------------------
        TutStep::new(
            C::KeepYourPlace,
            tr!("Back and forward"),
            tr!("Go back and forward through the places you've been, as in a browser."),
            Anchor::ToolbarLeft,
        ),
        TutStep::new(
            C::KeepYourPlace,
            tr!("Your trail"),
            tr!("Every jump you made, as a tree, so you can always find your way back."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Trail)),
        TutStep::new(
            C::KeepYourPlace,
            tr!("Bookmarks"),
            tr!("Bookmark a line to come back to it, with a note to say why."),
            Anchor::Sidebar,
        )
        .demo(tab(SidebarTab::Marks)),
        // -- Run it ---------------------------------------------------------
        TutStep::new(
            C::Run,
            tr!("Run and debug"),
            tr!("Run main.py to its end, or Debug it to stop at a breakpoint in checkout() and see its variables and its stack. Fake Run and Fake Debug, each under its twin, have the AI work a function out on inputs you give, without running anything."),
            Anchor::RunTools,
        )
        .demo(Message::Window(WindowMsg::ShowDebugPanel)),
        TutStep::new(
            C::Done,
            tr!("Your turn"),
            tr!("Open your own project from the ☰ menu. Tea Shop stays in this window to try things on, and the tour is in Help whenever you want it again."),
            Anchor::Center,
        ),
    ]);
    v
}

/// The tour, ready to begin: the sample written, and what opening it sets
/// up — the language servers to fetch for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TourStart {
    pub sample: Placed,
    pub fetch: Vec<(String, crate::app::setup::Fetch)>,
}

/// The sheet before the tour: what it needs, one step at a time, and the
/// sample it runs on as it is written.
#[derive(Debug, Clone, Default)]
pub struct TourPrep {
    /// The sample, once written, and what a look at it found (its servers,
    /// what explaining it takes) — or why it could not be written.
    pub sample: Option<Result<(Placed, Census), String>>,
    /// Find by meaning, passed over for this tour.
    pub skip_embeddings: bool,
}

/// One thing the tour needs, as the sheet before it lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// A language model: the tour cannot start without one.
    Model,
    /// Embeddings, for Find by meaning: the tour can do without.
    Embeddings,
    /// The sample's language server: set up as the tour starts.
    Server,
}

impl TourPrep {
    /// Whether `need` is met (or, for one the tour can do without, passed
    /// over).
    pub fn met(&self, app: &App, need: Need) -> bool {
        match need {
            // A key, and the model it is for answered: a key alone set up
            // the tour's calls, and every one failed where nothing answered.
            Need::Model => {
                app.llm_available
                    && matches!(
                        app.model_check.result,
                        crate::app::state::ConnectionTest::Answered(_)
                    )
            }
            Need::Embeddings => app.embed_available || self.skip_embeddings,
            // Nothing to ask: what the server takes is fetched as the tour
            // starts. Met once the look says what that is.
            Need::Server => matches!(self.sample, Some(Ok(_))),
        }
    }

    /// The step the sheet is on: the first need not met, in order.
    pub fn current(&self, app: &App) -> Option<Need> {
        [Need::Model, Need::Embeddings, Need::Server]
            .into_iter()
            .find(|need| !self.met(app, *need))
    }

    /// Whether the tour can start: a model, and the sample written.
    pub fn ready(&self, app: &App) -> bool {
        self.met(app, Need::Model) && matches!(self.sample, Some(Ok(_)))
    }
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
    /// The tour, asked for (from Help, the ⋯ menu, the welcome page): the
    /// sheet that gets it ready first, while the sample is written.
    pub(crate) fn on_tour_prepare(&mut self) -> Task<Message> {
        if self.tutorial.is_some() {
            return Task::none();
        }
        self.tour_prep = Some(TourPrep::default());
        self.write_sample()
    }

    /// Write the sample (see `crate::app::tour_sample`) off the UI thread,
    /// and look at it as a first open would: the servers it needs, what
    /// explaining it takes.
    fn write_sample(&mut self) -> Task<Message> {
        let Some(data) = clew_core::lsp::store::data_root() else {
            if let Some(prep) = self.tour_prep.as_mut() {
                prep.sample = Some(Err(tr!("Clew has no folder to keep its data in.").into()));
            }
            return Task::none();
        };
        let look = self.server_look(&data);
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let placed = crate::app::tour_sample::place(&data.join("tour"))?;
                    let census = crate::app::setup::census(&placed.root, None, &look);
                    Ok((placed, census))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()))
            },
            |written| Message::Tutorial(TutorialMsg::SampleWritten(written)),
        )
    }

    /// The sample is written (or could not be).
    pub(crate) fn on_sample_written(
        &mut self,
        written: Result<(Placed, Census), String>,
    ) -> Task<Message> {
        if let Some(prep) = self.tour_prep.as_mut() {
            prep.sample = Some(written);
        }
        Task::none()
    }

    /// The sheet's Start the Tour: on the sample, here when this window has
    /// nothing open (and is on this computer), else in a window of its own
    /// — the project open here stays as it is.
    pub(crate) fn on_tour_go(&mut self) -> Task<Message> {
        let Some(prep) = self.tour_prep.as_ref().filter(|p| p.ready(self)) else {
            return Task::none();
        };
        let Some(Ok((sample, census))) = prep.sample.clone() else {
            return Task::none();
        };
        self.tour_prep = None;
        let start = TourStart {
            sample,
            fetch: census
                .servers
                .iter()
                .filter(|s| s.fetch != crate::app::setup::Fetch::Nothing)
                .map(|s| (s.language.clone(), s.fetch.clone()))
                .collect(),
        };
        let here = self.proj.project.is_none()
            && !self.scanning
            && self.pending_consent.is_none()
            && !self.connection.is_remote();
        if here {
            self.begin_tour_in(start)
        } else {
            // The shell opens the window (`shell::update`).
            Task::done(Message::Tutorial(TutorialMsg::InNewWindow(start)))
        }
    }

    /// Open the sample in this window, and begin the tour once it is open
    /// (see [`App::begin_tour_when_open`]). Clew wrote it, so it is opened
    /// without the question a project is asked on its first open; what that
    /// question would set up — its language servers, its explanations — is
    /// set up as the tour begins.
    pub(crate) fn begin_tour_in(&mut self, start: TourStart) -> Task<Message> {
        let root = start.sample.root.clone();
        if let Err(e) = self.trust.update(|t| t.trust_root(None, &root)) {
            self.status = tr!("Cannot record consent: {error}", error = e);
            return Task::none();
        }
        self.pending_setup = Some(crate::app::setup::SetupIntent {
            root: root.clone(),
            fetch: start.fetch,
            explain: self.llm_available,
            servers_not_now: false,
        });
        self.tour_sample = Some(start.sample);
        self.request_open(root)
    }

    /// The tour begins once the sample it waits for is open and set up:
    /// called as that setup is carried out, which is once — the sample
    /// opened again later is not toured again unasked.
    pub(crate) fn begin_tour_when_open(&mut self) -> Task<Message> {
        let open = self.proj.project.as_ref().map(|p| &p.root);
        let waiting = self
            .tour_sample
            .as_ref()
            .is_some_and(|s| Some(&s.root) == open);
        if !waiting || self.tutorial.is_some() {
            return Task::none();
        }
        self.on_tutorial_start()
    }

    /// Begin the tour at step 0, on what is open.
    pub(crate) fn on_tutorial_start(&mut self) -> Task<Message> {
        // The tour holds the keys: no ring over it.
        let _ = self.end_ring(false);
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
        self.tour_then = None;
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
        let (anchor, demos, place, then) = match steps(self).get(step) {
            Some(s) => (s.anchor, s.demos.clone(), s.place, s.then.clone()),
            None => return Task::none(),
        };
        let demos_has = |pick: fn(&Message) -> bool| demos.iter().any(pick);
        // The file tree is always available. The right panel is shown only while
        // its own two steps are on screen (so the reader is uncluttered before
        // and after). The bottom panel is shown only for the steps that demo it.
        self.show_left_sidebar = true;
        self.show_right_panel = matches!(anchor, Anchor::RightTop);
        self.show_bottom = demos_has(|m| matches!(m, Message::Window(WindowMsg::ShowDebugPanel)));
        self.ask_view = if demos_has(|m| matches!(m, Message::Ask(AskMsg::Open))) {
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
        if !demos_has(|m| matches!(m, Message::Window(WindowMsg::SidebarTabPicked(_)))) {
            self.sidebar = SidebarTab::Files;
        }
        // A page over the code an earlier step showed (the overview) is
        // put away for a step about something else.
        if !demos_has(|m| matches!(m, Message::Overview(_))) {
            self.proj.overview.showing = false;
        }
        for msg in demos {
            tasks.push(self.update(msg));
        }
        self.tour_then = None;
        if let Some(place) = place {
            tasks.push(self.tour_place(place, then));
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
    /// Put the caret where `place` says, in the sample open in this window,
    /// and once it is there, `then`: at once when the file is on screen
    /// already, else when its load lands (`App::on_file_loaded`).
    fn tour_place(&mut self, place: Place, then: Option<Message>) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let abs = root.join(place.rel);
        let Some(line) = crate::app::tour_sample::line_of(place.rel, place.line_has) else {
            return Task::none();
        };
        self.proj.landing_next = Some((
            abs.clone(),
            line - 1,
            crate::app::state::LandingAt::Name(place.name.to_string()),
        ));
        let opened = self.open_file(abs.clone(), Some(line), false);
        let loading = self.proj.link.pane_pending[self.proj.active].is_some();
        let then = match then {
            Some(msg) if loading => {
                self.tour_then = Some((abs, msg));
                Task::none()
            }
            Some(msg) => self.update(msg),
            None => Task::none(),
        };
        Task::batch([opened, then])
    }

    /// A file has loaded: what the tour's step waits to show of it, if this
    /// is the file it waits for.
    pub(crate) fn tour_file_loaded(&mut self, abs: &Path) -> Task<Message> {
        match self.tour_then.take() {
            Some((at, msg)) if at == abs => self.update(msg),
            other => {
                self.tour_then = other;
                Task::none()
            }
        }
    }
}

impl App {
    /// Handle a [`TutorialMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_tutorial(&mut self, message: TutorialMsg) -> Task<Message> {
        match message {
            TutorialMsg::Start => self.on_tour_prepare(),
            TutorialMsg::SampleWritten(written) => self.on_sample_written(written),
            TutorialMsg::RetrySample => {
                let Some(prep) = self.tour_prep.as_mut() else {
                    return Task::none();
                };
                if !matches!(prep.sample, Some(Err(_))) {
                    return Task::none();
                }
                prep.sample = None;
                self.write_sample()
            }
            TutorialMsg::SetUp(page) => Task::done(Message::Settings(SettingsMsg::OpenAt(page))),
            TutorialMsg::SkipEmbeddings => {
                if let Some(prep) = self.tour_prep.as_mut() {
                    prep.skip_embeddings = true;
                }
                Task::none()
            }
            TutorialMsg::PrepClose => {
                self.tour_prep = None;
                Task::none()
            }
            TutorialMsg::Go => self.on_tour_go(),
            // Taken by the shell, which opens the window it begins in; one
            // that reaches a window anyway (none to open it) begins in it.
            TutorialMsg::InNewWindow(start) | TutorialMsg::BeginIn(start) => {
                self.begin_tour_in(start)
            }
            TutorialMsg::Begin => self.on_tutorial_start(),
            TutorialMsg::Step(delta) => self.on_tutorial_step(delta),
            TutorialMsg::Exit => self.on_tutorial_exit(),
        }
    }
}
