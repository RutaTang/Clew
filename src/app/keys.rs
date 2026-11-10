//! Keyboard input: key handling (Vim-style motions and prefixes, the finder
//! and find keys), the customizable command actions, and capturing a rebind.

use crate::app::prelude::*;
use crate::*;

/// The parts of the window F6 moves the keys between, in the order it
/// moves through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Region {
    /// The code: its motions and keys.
    Code,
    /// The sidebar's field: the file filter, or the text search.
    Sidebar,
    /// The Files view's tree, under its filter (`ui::tree_keys`).
    Tree,
    /// The chat with Clew.
    Chat,
}

/// An id no field has: focusing it takes the keys from every field.
pub(crate) fn nowhere() -> iced::widget::Id {
    iced::widget::Id::new("clew-keys-nowhere")
}

impl App {
    /// ⌘J: put a code on every control on screen (`ui::jump`), the keys
    /// taken from the fields to type one by — or put the codes away, if
    /// they are up.
    fn start_jump(&mut self) -> Task<Message> {
        if self.jump.take().is_some() {
            self.status.clear();
            return Task::none();
        }
        self.jump = Some(String::new());
        self.pending_g = false;
        self.pending_z = false;
        self.status = tr!("Type the code on a control to press it — Esc to stop").into();
        operation::focus(nowhere())
    }

    /// A key while jump mode's codes are up: a letter goes on with a code,
    /// and a whole code presses its control, as a click does; Backspace
    /// takes a letter back. Any other key — Esc, ⌘J — puts the codes away.
    fn jump_key(&mut self, key: &keyboard::Key, modifiers: keyboard::Modifiers) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;
        let mut typed = self.jump.take().unwrap_or_default();
        let plain = !(modifiers.command() || modifiers.control() || modifiers.alt());
        match key.as_ref() {
            Key::Named(Named::Backspace) if plain && !typed.is_empty() => {
                typed.pop();
                self.jump = Some(typed);
                Task::none()
            }
            Key::Character(c) if plain => {
                let Some(letter) = c.chars().next().filter(char::is_ascii_alphabetic) else {
                    self.status.clear();
                    return Task::none();
                };
                typed.push(letter.to_ascii_lowercase());
                match ui::jump::pressed(self.main_window, &typed) {
                    ui::jump::Typed::Press(message) => {
                        self.status.clear();
                        self.update(*message)
                    }
                    ui::jump::Typed::More => {
                        self.jump = Some(typed);
                        Task::none()
                    }
                    ui::jump::Typed::Nothing => {
                        self.status = tr!(
                            "No control has the code {code}",
                            code = typed.to_uppercase()
                        );
                        Task::none()
                    }
                }
            }
            _ => {
                self.status.clear();
                Task::none()
            }
        }
    }

    /// Tab or Shift-Tab, a field holding the keys (`field`) or not: the
    /// next field, as before; else the keyboard's ring, on the first
    /// control (the last, going back), the code giving up the keys.
    pub(crate) fn on_tab(&mut self, forward: bool, field: bool) -> Task<Message> {
        self.code_focused = false;
        if field {
            return ui::cycle_focus(forward);
        }
        self.ring = Some(if forward { 0 } else { -1 });
        self.pending_g = false;
        self.pending_z = false;
        // The Files view's tree gives up the keys too: its arrows are the
        // ring's now.
        operation::focus(nowhere())
    }

    /// A key while the keyboard's ring is up: Tab and the arrows move it,
    /// Enter and Space press what it is on (as a click does, the ring
    /// staying), Esc puts it away. Any other key puts it away and is left
    /// to do what it does (`None`).
    fn ring_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        use keyboard::Key;
        use keyboard::key::Named;
        let plain = !(modifiers.command() || modifiers.control() || modifiers.alt());
        let step = match key.as_ref() {
            Key::Named(Named::Tab) if plain => Some(!modifiers.shift()),
            Key::Named(Named::ArrowDown | Named::ArrowRight) if plain => Some(true),
            Key::Named(Named::ArrowUp | Named::ArrowLeft) if plain => Some(false),
            _ => None,
        };
        if let Some(forward) = step {
            // From where it is: on the first control of a layer come up on
            // top since, if one has.
            let asked = self.ring.unwrap_or(0);
            let at = ui::jump::ring_on(self.main_window, asked).map_or(asked, |at| at as isize);
            self.ring = Some(if forward { at + 1 } else { at - 1 });
            return Some(Task::none());
        }
        match key.as_ref() {
            Key::Named(Named::Enter | Named::Space) if plain => {
                let asked = self.ring.unwrap_or(0);
                let message = ui::jump::ring_press(self.main_window, asked)?;
                // Where it is, so a press that changes nothing on screen
                // leaves it there.
                if let Some(at) = ui::jump::ring_on(self.main_window, asked) {
                    self.ring = Some(at as isize);
                }
                Some(self.update(message))
            }
            Key::Named(Named::Escape) => {
                self.ring = None;
                Some(Task::none())
            }
            _ => {
                self.ring = None;
                None
            }
        }
    }

    /// The regions F6 moves through now: the code while a file is open,
    /// the sidebar while it shows a field, and the chat.
    fn regions(&self) -> Vec<Region> {
        let mut regions = Vec::new();
        if self.active_viewer().is_some() {
            regions.push(Region::Code);
        }
        if self.sidebar_field().is_some() {
            regions.push(Region::Sidebar);
        }
        if self.sidebar_field().is_some() && self.sidebar == SidebarTab::Files {
            regions.push(Region::Tree);
        }
        regions.push(Region::Chat);
        regions
    }

    /// The sidebar's field, while it is on screen.
    fn sidebar_field(&self) -> Option<iced::widget::Id> {
        if !self.show_left_sidebar || self.proj.project.is_none() {
            return None;
        }
        match self.sidebar {
            SidebarTab::Files => Some(ui::file_filter_id()),
            SidebarTab::Search => Some(ui::search_input_id()),
            _ => None,
        }
    }

    /// F6 (`forward`) or Shift-F6: the keys to the next region of the
    /// window, or the one before — from the code, wherever they are.
    pub(crate) fn cycle_region(&mut self, forward: bool) -> Task<Message> {
        let regions = self.regions();
        let now = if self.code_focused {
            Region::Code
        } else if self.tree_focused {
            Region::Tree
        } else {
            self.region
        };
        let n = regions.len();
        let next = match regions.iter().position(|&r| r == now) {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        };
        let region = regions[next];
        self.region = region;
        match region {
            Region::Code => {
                self.code_focused = true;
                operation::focus(nowhere())
            }
            Region::Sidebar => {
                self.code_focused = false;
                self.sidebar_field()
                    .map_or_else(Task::none, operation::focus)
            }
            Region::Tree => {
                self.code_focused = false;
                Task::batch([
                    operation::focus(ui::files_tree_keys_id()),
                    self.reveal_tree_cursor(),
                ])
            }
            Region::Chat => {
                self.reveal_ask();
                self.code_focused = false;
                Task::batch([
                    operation::focus(ui::ask_input_id()),
                    operation::move_cursor_to_end(ui::ask_input_id()),
                ])
            }
        }
    }

    pub(crate) fn handle_key(
        &mut self,
        key: keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();

        // An Escape a field used already (see `escape_taken`); read on every
        // press, so it never holds over to a later one.
        if std::mem::take(&mut self.escape_taken)
            && matches!(key.as_ref(), Key::Named(Named::Escape))
        {
            return Task::none();
        }

        // Rebinding capture takes priority: the next chord becomes the binding.
        if let Some(action) = self.rebinding {
            return self.capture_rebind(action, &key, modifiers);
        }
        // A modifier pressed on its own is no key of its own: the Shift
        // before `R` in `zR` ended the `z` it was typed after.
        if matches!(
            key.as_ref(),
            Key::Named(
                Named::Shift
                    | Named::Control
                    | Named::Alt
                    | Named::AltGraph
                    | Named::Super
                    | Named::Meta
                    | Named::Hyper
                    | Named::CapsLock
                    | Named::NumLock
                    | Named::ScrollLock
                    | Named::Fn
                    | Named::FnLock
                    | Named::Symbol
                    | Named::SymbolLock
            )
        ) {
            return Task::none();
        }
        // Jump mode's codes are up: every key is theirs (`jump_key`).
        if self.jump.is_some() {
            return self.jump_key(&key, modifiers);
        }
        // The keyboard's ring is up: Tab, the arrows, Enter, Space and Esc
        // are its (`ring_key`); any other key puts it away and does what it
        // does.
        if self.ring.is_some()
            && let Some(task) = self.ring_key(&key, modifiers)
        {
            return task;
        }
        // The tutorial is modal: → / Enter advance, ← goes back, Esc leaves, and
        // every other key is swallowed so nothing acts behind the overlay.
        if self.tutorial.is_some() {
            return match key.as_ref() {
                Key::Named(Named::ArrowRight | Named::Enter | Named::Space) => {
                    self.on_tutorial_step(1)
                }
                Key::Named(Named::ArrowLeft) => self.on_tutorial_step(-1),
                Key::Named(Named::Escape) => self.on_tutorial_exit(),
                _ => Task::none(),
            };
        }
        // Escape dismisses the overlay on top — the same one the view draws
        // (`ui::active_overlay`), with the message its own Close/Cancel sends.
        if matches!(key.as_ref(), Key::Named(Named::Escape))
            && let Some(msg) = ui::escape_message(self)
        {
            self.pending_g = false;
            self.pending_z = false;
            return self.update(msg);
        }
        // The large Ask window is over everything but the overlays: Esc
        // makes it a card first, before it reaches what is under it.
        if matches!(key.as_ref(), Key::Named(Named::Escape)) && self.ask_view == AskView::Expanded {
            self.step_ask_down();
            return Task::none();
        }
        // While Settings shows the shortcuts (and is not capturing), swallow
        // every key so nothing acts behind it. (Esc never gets here: Settings
        // is an overlay, so `escape_message` above closed it.)
        if self.shortcuts_shown() {
            // But ⌘J: its codes go on the page's own controls only.
            if keymap::Chord::from_event(&key, modifiers)
                .is_some_and(|chord| chord == self.keymap.chord(keymap::Action::Jump))
            {
                return self.start_jump();
            }
            return Task::none();
        }
        // Tab / Shift-Tab move keyboard focus between fields (inside the open
        // modal, if any) while a field holds the keys; else the keyboard's
        // ring among the controls on screen (`WindowMsg::TabPressed`).
        if matches!(key.as_ref(), Key::Named(Named::Tab))
            && !(cmd || modifiers.alt() || modifiers.control())
        {
            let forward = !modifiers.shift();
            return ui::field_focused()
                .map(move |field| Message::Window(WindowMsg::TabPressed { forward, field }));
        }
        // F6 / Shift-F6 move the keys between the window's regions — the
        // code, the sidebar's field, the chat — but not out of a dialog
        // holding them.
        if matches!(key.as_ref(), Key::Named(Named::F6))
            && !ui::active_overlay(self).is_some_and(|o| o.holds_keys())
        {
            return self.cycle_region(!modifiers.shift());
        }
        // Keys go to whatever is on top first: the overlay drawn over the app
        // takes its own before anything under it — a time-travel session
        // included, whose swallow below used to drop the finder's ↑/↓. (Esc,
        // which every overlay takes, went to the one on top above.)
        if let Some(task) = self.overlay_key(&key) {
            return task;
        }
        // Time Travel history navigation uses COMMAND chords — ⌘← steps to an
        // older commit, ⌘→ to a newer one — so plain ←/→/h/l stay free for
        // reading. Esc exits. Handled before the keymap so these chords drive
        // time travel (over any action rebound to them) while a session is on.
        // Not ⌘H / ⌘L: the menu bar owns both (Hide clew, Go to Line), so
        // AppKit takes them before this handler could ever see them.
        //
        // Only the session on screen (`ui::time_travel_on_screen`): one the
        // reader cannot see — its pane shows another file, a page covers the
        // panes — takes no key here or below. Esc used to end it unseen, and
        // every reading motion meant for the view on screen was swallowed.
        //
        // Nor does one under an overlay take its chords: a menu, a panel, a
        // modal, the finder hold the keys (`ActiveOverlay::holds_keys`), and
        // ⌘←/⌘→ used to step the history under the tools menu, the graph or
        // the server panel. Its swallow below still keeps the reading
        // motions off the live file hidden behind it.
        let time_travel = ui::time_travel_on_screen(self).map(|tt| (tt.idx, tt.commits.len()));
        let covered = ui::active_overlay(self).is_some_and(|o| o.holds_keys());
        if let Some((idx, n)) = time_travel.filter(|_| !covered) {
            match key.as_ref() {
                Key::Named(Named::Escape) => {
                    return self.update(Message::TimeTravel(TimeTravelMsg::Exit));
                }
                Key::Named(Named::ArrowLeft) if cmd && idx + 1 < n => {
                    return self.update(Message::TimeTravel(TimeTravelMsg::Goto(idx + 1)));
                }
                Key::Named(Named::ArrowRight) if cmd && idx > 0 => {
                    return self.update(Message::TimeTravel(TimeTravelMsg::Goto(idx - 1)));
                }
                _ => {}
            }
        }
        // Command chords (those carrying ⌘/⌥/⌃) are dispatched through the
        // customizable keymap. Only modifier-carrying chords are eligible, so
        // the single-key reading motions and text input below stay untouched.
        if (cmd || modifiers.alt() || modifiers.control())
            && let Some(chord) = keymap::Chord::from_event(&key, modifiers)
            && let Some(action) = self.keymap.action_for(&chord)
            && let Some(task) = self.run_command_action(action)
        {
            return task;
        }

        // Settings: ⌘, — the menu bar's on macOS, where AppKit takes it before
        // this handler; Ctrl+, on Windows and Linux, which have no menu bar
        // to own it.
        if cmd
            && !modifiers.shift()
            && !modifiers.alt()
            && matches!(key.as_ref(), Key::Character(","))
        {
            return self.update(Message::Settings(SettingsMsg::Open));
        }

        // While time-travelling, swallow any remaining (non-command) keys so
        // plain reading motions don't act on the live file hidden behind the view.
        if time_travel.is_some() {
            return Task::none();
        }

        match key.as_ref() {
            // In-file find bar: Enter next, Shift+Enter prev.
            Key::Named(Named::Enter) if self.proj.find.open => {
                self.update(Message::Editor(EditorMsg::FindStep(if modifiers.shift() {
                    -1
                } else {
                    1
                })))
            }
            // Only what is NOT an overlay is left for Esc here — listening,
            // the find bar, the selection, then the Ask card: each press puts
            // away the nearest. Every overlay (the context menu and the
            // finder included) was dismissed through `escape_message` above.
            Key::Named(Named::Escape) => {
                self.pending_g = false;
                self.pending_z = false;
                if self.voice.takes_esc() {
                    return self.update(Message::Voice(VoiceMsg::Cancel));
                }
                if self.proj.find.open {
                    return self.update(Message::Editor(EditorMsg::FindClosed));
                }
                // Ask holding the keys (its input, not the code, has them),
                // or its menu of sessions open over it, is put away first:
                // what the reader is looking at — a press went instead to a
                // selection, or a jumped-to line's mark, in the code behind.
                let ask_first =
                    self.ask_view.is_open() && (!self.code_focused || self.ask_sessions_menu);
                // A page over the code (the overview, a doc page) hides its
                // selection: the card over the page goes, then the page.
                let covered = ui::pane_cover(self).is_some_and(|c| c.is_page());
                let marked = !covered
                    && self
                        .active_viewer()
                        .is_some_and(|v| v.selection.is_some() || v.target_line.is_some());
                if (ask_first || !marked) && self.step_ask_down() {
                    return Task::none();
                }
                if covered && self.put_away_page() {
                    return Task::none();
                }
                if let Some(v) = self.active_viewer_mut() {
                    v.selection = None;
                    v.target_line = None;
                }
                self.reader_moved_caret(self.proj.active);
                Task::none()
            }
            // -------- Vim-style read-only cursor (only when the code view has
            // focus, so it never steals keys from a text input) --------
            _ if cmd
                || self.proj.finder.open
                || self.proj.context_menu.is_some()
                || !self.code_focused
                || self.active_viewer().is_none() =>
            {
                Task::none()
            }
            // Two-key `g` prefix: gg / gd / gr / gi / gy / gc / ge.
            _ if self.pending_g => {
                self.pending_g = false;
                match key.as_ref() {
                    Key::Character("g") => self.move_cursor(viewer::Motion::FileStart),
                    Key::Character("d") => self.goto_at_cursor(GotoKind::Definition),
                    Key::Character("r") => self.goto_at_cursor(GotoKind::References),
                    Key::Character("i") => self.goto_at_cursor(GotoKind::Implementation),
                    Key::Character("y") => self.goto_at_cursor(GotoKind::TypeDefinition),
                    Key::Character("c") => self.update(Message::Calls(CallsMsg::Requested)),
                    Key::Character("e") => self.insight_at_cursor(),
                    _ => Task::none(),
                }
            }
            Key::Character("g") => {
                self.pending_g = true;
                Task::none()
            }
            // Two-key `z` prefix for folding: za toggle, zR open all, zM close all.
            _ if self.pending_z => {
                self.pending_z = false;
                match key.as_ref() {
                    Key::Character("a") => self.fold_toggle_at_cursor(),
                    Key::Character("R") => self.fold_all(false),
                    Key::Character("M") => self.fold_all(true),
                    _ => {}
                }
                Task::none()
            }
            Key::Character("z") => {
                self.pending_z = true;
                Task::none()
            }
            Key::Character("h") | Key::Named(Named::ArrowLeft) => {
                self.move_cursor(viewer::Motion::Left)
            }
            Key::Character("l") | Key::Named(Named::ArrowRight) => {
                self.move_cursor(viewer::Motion::Right)
            }
            Key::Character("k") | Key::Named(Named::ArrowUp) => {
                self.move_cursor(viewer::Motion::Up)
            }
            Key::Character("j") | Key::Named(Named::ArrowDown) => {
                self.move_cursor(viewer::Motion::Down)
            }
            Key::Character("w") => self.move_cursor(viewer::Motion::WordForward),
            Key::Character("b") => self.move_cursor(viewer::Motion::WordBack),
            Key::Character("0") => self.move_cursor(viewer::Motion::LineStart),
            Key::Character("$") => self.move_cursor(viewer::Motion::LineEnd),
            Key::Character("G") => self.move_cursor(viewer::Motion::FileEnd),
            _ => Task::none(),
        }
    }

    /// A key the overlay on top (`ui::active_overlay`) takes for itself,
    /// besides the Esc every overlay takes: the finder's ↑/↓, which move its
    /// selection. `None` when the key is not the overlay's to take.
    fn overlay_key(&mut self, key: &keyboard::Key) -> Option<Task<Message>> {
        use keyboard::Key;
        use keyboard::key::Named;
        let delta = match (ui::active_overlay(self)?, key.as_ref()) {
            (ui::ActiveOverlay::Finder, Key::Named(Named::ArrowDown)) => 1,
            (ui::ActiveOverlay::Finder, Key::Named(Named::ArrowUp)) => -1,
            _ => return None,
        };
        self.proj.finder.move_selection(delta);
        Some(ui::reveal_finder_selection(
            self.proj.finder.selected,
            self.headings_over_selection(),
        ))
    }

    /// Run a rebindable command action. Returns `None` when the action declines
    /// in the current context (so the key falls through — e.g. ⌘C inside the
    /// finder input should copy text, not the code selection).
    pub(crate) fn run_command_action(&mut self, action: keymap::Action) -> Option<Task<Message>> {
        use keymap::Action::*;
        Some(match action {
            OpenFile => self.update(Message::Nav(NavMsg::FinderOpened(FinderMode::Files))),
            OpenSymbol => self.update(Message::Nav(NavMsg::FinderOpened(FinderMode::Symbols))),
            ProjectSearch => self.update(Message::Window(WindowMsg::SidebarTabPicked(
                SidebarTab::Search,
            ))),
            // Over a map, Find finds in the map.
            FindInFile
                if self.graph_mode
                    && matches!(ui::active_overlay(self), Some(ui::ActiveOverlay::Graph(_))) =>
            {
                operation::focus(ui::graph_search_id())
            }
            FindInFile => self.update(Message::Editor(EditorMsg::FindOpened)),
            CopySelection => {
                if self.proj.finder.open {
                    return None;
                }
                self.update(Message::Editor(EditorMsg::CopySelection))
            }
            ToggleBookmark => self.update(Message::Reading(ReadingMsg::BookmarkToggled)),
            GotoLine => self.update(Message::Nav(NavMsg::GotoLineRequested)),
            ToggleSplit => self.update(Message::Editor(EditorMsg::ToggleSplit)),
            ZoomIn => self.update(Message::Editor(EditorMsg::FontSizeDelta(1.0))),
            ZoomOut => self.update(Message::Editor(EditorMsg::FontSizeDelta(-1.0))),
            ZoomReset => self.update(Message::Editor(EditorMsg::FontSizeReset)),
            GoBack => self.update(Message::Reading(ReadingMsg::GoBack)),
            GoForward => self.update(Message::Reading(ReadingMsg::GoForward)),
            TraceValue => self.update(Message::Flow(crate::FlowMsg::AtCursor)),
            FakeRun => self.update(Message::Fake(FakeMsg::AtCaret { debug: false })),
            FakeDebug => self.update(Message::Fake(FakeMsg::AtCaret { debug: true })),
            ToggleAsk => self.update(Message::Ask(AskMsg::Toggle)),
            Run => self.update(Message::Debug(DebugMsg::Run)),
            StartDebug => self.update(Message::Debug(DebugMsg::Start)),
            WeakSpots => self.update(Message::Weak(WeakMsg::Show)),
            Jump => self.start_jump(),
            CallGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectCalls))),
            TypeGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectTypes))),
            ProjectMaps => self.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectMaps))),
            ImportGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(
                Overlay::ProjectImports,
            ))),
            // The same as the ⋯ menu's row: cancel a running pass, else ask
            // first what one costs (or ask for a key first, on the page of
            // Settings that takes it).
            ExplainAll => self.update(if self.proj.explain.running {
                Message::Explain(ExplainMsg::Cancel)
            } else if self.llm_available {
                Message::Project(ProjectMsg::SetupShow(crate::app::setup::SetupMode::Explain))
            } else {
                Message::Settings(SettingsMsg::OpenAt(SettingsPage::LanguageModel))
            }),
            ToggleDiff => self.update(Message::Editor(EditorMsg::ToggleDiff)),
            TimeTravel => self.update(Message::TimeTravel(TimeTravelMsg::Start { symbol: false })),
            Walkthrough => self.update(Message::Window(WindowMsg::SidebarTabPicked(
                SidebarTab::Walk,
            ))),
            LspServers => self.update(Message::Lsp(LspMsg::TogglePanel)),
            Shortcuts => self.update(Message::Window(WindowMsg::OpenShortcuts)),
            Overview => self.update(Message::Overview(OverviewMsg::Show)),
            Stats => self.update(Message::Overview(OverviewMsg::ShowStats)),
            Glossary => self.update(Message::Glossary(GlossaryMsg::Open)),
            SetUpProject => self.update(Message::Project(ProjectMsg::SetupShow(
                crate::app::setup::SetupMode::Manage,
            ))),
            Tutorial => self.update(Message::Tutorial(TutorialMsg::Start)),
            CommandPalette => self.update(Message::Nav(NavMsg::PaletteOpened)),
            Voice => self.update(Message::Voice(VoiceMsg::Toggle)),
        })
    }

    /// A menu item's action — clicked, or its chord matched by AppKit as the
    /// item's key equivalent, which happens BEFORE any key event reaches the
    /// app: the same keystroke `handle_key` would have seen, so it answers to
    /// the same modal states. During a rebind capture the chord is what the
    /// user pressed to bind (it is captured, or refused as taken — never run:
    /// ⌘⇧E there used to start a paid Explain All); the tutorial and the
    /// shortcuts panel swallow it like every other key. An action that
    /// declines in this context (⌘C while the finder holds the text) does
    /// nothing — there is no key event left to fall through to.
    pub(crate) fn on_menu_action(&mut self, action: keymap::Action) -> Task<Message> {
        if let Some(target) = self.rebinding {
            let chord = self.keymap.chord(action);
            return self.capture_chord(target, chord);
        }
        let jump_on_shortcuts = self.shortcuts_shown() && action == keymap::Action::Jump;
        if self.tutorial.is_some() || (self.shortcuts_shown() && !jump_on_shortcuts) {
            return Task::none();
        }
        self.run_command_action(action).unwrap_or_else(Task::none)
    }

    /// Capture a keypress as the new binding for `action`. Esc cancels; keys
    /// without a ⌘/⌥/⌃ modifier or that collide with another action are
    /// rejected with an inline notice (capture stays active so the user can
    /// try again). A successful bind is persisted immediately.
    pub(crate) fn capture_rebind(
        &mut self,
        action: keymap::Action,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::key::Named;
        if matches!(key.as_ref(), keyboard::Key::Named(Named::Escape)) {
            self.rebinding = None;
            self.keymap_notice = None;
            return Task::none();
        }
        let Some(chord) = keymap::Chord::from_event(key, modifiers) else {
            self.keymap_notice = Some(tr!("This key can't be used in a shortcut").into());
            return Task::none();
        };
        self.capture_chord(action, chord)
    }

    /// Bind `chord` to `action` (the rebind capture), unless it is not a
    /// command chord or another action holds it.
    fn capture_chord(&mut self, action: keymap::Action, chord: keymap::Chord) -> Task<Message> {
        if !chord.is_command() {
            self.keymap_notice = Some(if keymap::MAC_KEYS {
                tr!("A shortcut must include ⌘, ⌥ or ⌃").into()
            } else {
                tr!("A shortcut must include Ctrl or Alt").into()
            });
            return Task::none();
        }
        if let Some(other) = self.keymap.conflict(&chord, action) {
            self.keymap_notice = Some(tr!("Already used by “{name}”", name = other.label()));
            return Task::none();
        }
        self.keymap.rebind(action, chord);
        self.rebinding = None;
        self.keymap_notice = None;
        if let Err(e) = self.keymap.save() {
            self.status = tr!("Could not save shortcuts: {error}", error = e);
        }
        Task::none()
    }
}
