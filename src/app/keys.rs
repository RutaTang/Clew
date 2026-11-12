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
        // The codes put the ring away: a press after them is a code's.
        let _ = self.end_ring(false);
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
                        self.press(*message)
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

    /// Tab or Shift-Tab, the ring not up: the window puts it up — on the
    /// first stop (the last, going back), or, a field holding the keys
    /// (`field`), on the stop after it. The keys it takes are given back
    /// when the ring is put away by a key.
    pub(crate) fn on_tab(&mut self, forward: bool, field: bool) -> Task<Message> {
        if self.tutorial.is_some() {
            return Task::none();
        }
        if self.ring.is_none() {
            self.ring_code = self.code_focused;
            self.ring_menu = ui::active_overlay(self).is_some_and(|o| o.is_menu());
        }
        self.ring = Some(if field {
            ui::RingAsk::AfterFocused { forward }
        } else {
            ui::RingAsk::Start { forward }
        });
        self.code_focused = false;
        self.pending_g = false;
        self.pending_z = false;
        Task::none()
    }

    /// The ring is on another stop: what it is called — as its control
    /// said when built, else as what it presses is offered — said in the
    /// status bar (a strip's tool is an icon alone); nothing, on a field.
    pub(crate) fn on_ring_moved(&mut self, press: Option<Message>, name: Option<String>) {
        // Said late, of a ring put away since: it stays away.
        if self.ring.is_none() {
            return;
        }
        self.ring = Some(ui::RingAsk::Kept);
        self.status = name
            .or_else(|| press.and_then(|press| ring_name(self, &press)))
            .unwrap_or_default();
        self.ring_said = self.status.clone();
    }

    /// The ring put away: by a key (`give_back`), the keys go back where
    /// they were when it came up — to the code, unless a dialog or the
    /// tutorial holds them now; to the field it came up from, which the
    /// task returned gives them back — and by the pointer or a field taking
    /// them, they are where those put them. What the ring said in the
    /// status bar goes with it.
    pub(crate) fn end_ring(&mut self, give_back: bool) -> Option<Task<Message>> {
        self.ring.take()?;
        self.ring_menu = false;
        if self.status == self.ring_said {
            self.status.clear();
        }
        self.ring_said.clear();
        if !give_back {
            return None;
        }
        // Ask made large holds them too, as a dialog does. A menu still up
        // does not: it lets keys through to nothing and goes with Esc, and
        // counted, the keys were nowhere once it had gone.
        let held_above = ui::active_overlay(self).is_some_and(|o| o.holds_keys() && !o.is_menu())
            || self.tutorial.is_some()
            || self.ask_view == crate::AskView::Expanded;
        self.code_focused = self.ring_code && !held_above;
        if self.ring_code {
            return None;
        }
        ui::jump::ring_from(self.main_window).map(operation::focus)
    }

    /// A menu come up or gone, as the last message left it. Gone: the ring
    /// that came up on it goes with it, the keys back where they were —
    /// left up, it went on to a control of the window, whose name took
    /// the status line from what the menu's row had said there — and the
    /// tree, which gives up the keys while its menu is up, has them again,
    /// unless what was picked gave them to the code (a file opened).
    /// Before, the keys were the tree's when the menu went, and nothing's.
    pub(crate) fn sync_menu_keys(&mut self) -> Option<Task<Message>> {
        let up = ui::active_overlay(self).is_some_and(|o| o.is_menu());
        let was = std::mem::replace(&mut self.menu_up, up);
        if up && !was {
            self.menu_tree_keys = self.tree_focused;
            // Come up under the ring (Enter on the View button): the ring's
            // menu as one Tab opened on is — Esc closed the ring alone and
            // left the menu holding the keys, and a pick left the ring on a
            // control of the window.
            if self.ring.is_some() {
                self.ring_menu = true;
            }
        }
        if up || !was {
            return None;
        }
        let held = ui::active_overlay(self).is_some_and(|o| o.holds_keys())
            || self.tutorial.is_some()
            || self.ask_view == crate::AskView::Expanded;
        // A dialog its row opened in its place: the ring goes on there, on
        // the dialog's first stop.
        let ring = if self.ring_menu && !held {
            // What was picked gave the code the keys (Open, from the tree's
            // menu): theirs once the ring is put away — it gave them back
            // to the tree the ring came up from.
            if self.code_focused {
                self.ring_code = true;
            }
            self.end_ring(true)
        } else {
            self.ring_menu = false;
            None
        };
        let tree = std::mem::take(&mut self.menu_tree_keys)
            && !self.code_focused
            && !held
            && self.show_left_sidebar
            && self.sidebar == crate::SidebarTab::Files
            && ui::tree_has_rows(self);
        let tree = tree.then(|| operation::focus(ui::files_tree_keys_id()));
        match (ring, tree) {
            (None, None) => None,
            (ring, tree) => Some(Task::batch(ring.into_iter().chain(tree))),
        }
    }

    /// What pressing a control sends, sent as its click sends it: through
    /// the window's shell when that takes it before the App does — closing
    /// the window (which asks about unsaved edits first), a tour begun in a
    /// window of its own — else to the App now.
    pub(crate) fn press(&mut self, message: Message) -> Task<Message> {
        let shells = matches!(
            message,
            Message::Window(WindowMsg::Close) | Message::Tutorial(TutorialMsg::InNewWindow(_))
        );
        if shells {
            Task::done(message)
        } else {
            self.update(message)
        }
    }

    /// A key while the keyboard's ring is up (the window took Tab and the
    /// arrows already): Enter and Space press what it is on, as a click
    /// does, the ring staying; Esc puts it away. Any other key puts it away
    /// and is left to do what it does (`None`).
    fn ring_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        use keyboard::Key;
        use keyboard::key::Named;
        let plain = !(modifiers.command() || modifiers.control() || modifiers.alt());
        match key.as_ref() {
            Key::Named(Named::Enter | Named::Space) if plain => {
                let message = ui::jump::ring_press(self.main_window)?;
                let task = self.press(message);
                // The press gave the code the keys (a file opened from a
                // row): they are its once the ring is put away.
                if self.ring.is_some() && self.code_focused {
                    self.ring_code = true;
                    self.code_focused = false;
                }
                Some(task)
            }
            // On a menu, the menu goes with it, and the keys go back where
            // they were: put away alone, it left the menu up, holding them,
            // and the next Esc closed the menu with the keys nowhere.
            Key::Named(Named::Escape) if self.ring_menu => {
                let closed = ui::escape_message(self).map(|close| self.update(close));
                // Closing it put the ring away (`sync_menu_keys`) unless it
                // did not close.
                let ended = self.end_ring(true);
                Some(Task::batch(closed.into_iter().chain(ended)))
            }
            Key::Named(Named::Escape) => Some(self.end_ring(true).unwrap_or_else(Task::none)),
            // Any other key puts the ring away and does what it does. Given
            // back to the field the ring came up from, what is typed is the
            // field's to have, and went to no field; a shortcut (with a
            // modifier, or a function key) does what it does once the field
            // has the keys again — it only put the ring away, and had to be
            // pressed twice.
            _ => {
                let give_back = self.end_ring(true)?;
                let shortcut =
                    !plain || matches!(key.as_ref(), Key::Named(named) if is_function_key(named));
                Some(if shortcut {
                    give_back.chain(Task::done(Message::Editor(EditorMsg::KeyPressed(
                        key.clone(),
                        modifiers,
                    ))))
                } else {
                    give_back
                })
            }
        }
    }

    /// What a function key does for the debugger now, if anything: F9 sets
    /// or clears a breakpoint on the caret's line; F5 continues a paused
    /// program, or starts debugging when none runs; ⇧F5 stops it; F10, F11
    /// and ⇧F11 step over, in and out of a paused one.
    pub(crate) fn debug_key(
        &self,
        key: &iced::keyboard::Key,
        modifiers: iced::keyboard::Modifiers,
    ) -> Option<Message> {
        use iced::keyboard::{Key, key::Named};
        let Key::Named(named) = key.as_ref() else {
            return None;
        };
        if modifiers.command() || modifiers.alt() {
            return None;
        }
        let status = self.debug.session.as_ref().map(|s| s.status);
        let paused = status == Some(crate::DebugStatus::Stopped);
        let live = status.is_some_and(|s| s != crate::DebugStatus::Terminated);
        let shift = modifiers.shift();
        let step = |cmd| Message::Debug(DebugMsg::Control(cmd));
        Some(match named {
            Named::F9 if !shift => {
                let v = self.active_viewer()?;
                let (line, _) = v.caret?;
                Message::Debug(DebugMsg::BreakpointToggle {
                    path: v.abs.clone(),
                    line: line + 1,
                })
            }
            Named::F5 if shift && live => Message::Debug(DebugMsg::Stop),
            Named::F5 if !shift && paused => step(crate::DebugCmd::Continue),
            Named::F5 if !shift && !live => {
                Message::Editor(EditorMsg::RunAction(keymap::Action::StartDebug))
            }
            Named::F10 if !shift && paused => step(crate::DebugCmd::StepOver),
            Named::F11 if !shift && paused => step(crate::DebugCmd::StepIn),
            Named::F11 if shift && paused => step(crate::DebugCmd::StepOut),
            _ => return None,
        })
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
        if self.sidebar_field().is_some()
            && self.sidebar == SidebarTab::Files
            && ui::tree_has_rows(self)
        {
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
        // Going on from the chat F6 opened, nothing typed or asked there:
        // put away again, not left over the code.
        if now == Region::Chat
            && region != Region::Chat
            && let Some(turns) = self.ask_by_f6.take()
            && self.proj.ask_input.trim().is_empty()
            && self.proj.ask_turns.len() == turns
        {
            // As its × puts it away: its menu of sessions with it.
            self.put_ask_away();
        }
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
                self.ask_by_f6 = (!self.ask_view.is_open()).then_some(self.proj.ask_turns.len());
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
            // It put the ring away too: the field gave the keys where they
            // go.
            let _ = self.end_ring(false);
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
        // The tutorial holds the keys: no ring over it.
        if self.tutorial.is_some() {
            let _ = self.end_ring(false);
        }
        // The keyboard's ring is up: Enter, Space and Esc are its
        // (`ring_key`); any other key puts it away and does what it does.
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
        // Tab is the window's: it moves the keyboard's ring among the
        // controls and fields (`ui::ring`) before a key reaches here.
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
        if let Some(task) = self.overlay_key(&key, modifiers) {
            return task;
        }
        // The debugger's function keys, as editors have them — F9 a
        // breakpoint, F5 continue (or start), ⇧F5 stop, F10 over, F11 in,
        // ⇧F11 out — unless a dialog holds the keys.
        // ⇧F10, or the Menu key: the code's menu at the caret, as a
        // right-click opens it there (the tree's opens from the tree).
        if self.code_focused
            && ui::active_overlay(self).is_none()
            && !(cmd || modifiers.alt() || modifiers.control())
            && match key.as_ref() {
                Key::Named(Named::F10) => modifiers.shift(),
                Key::Named(Named::ContextMenu) => true,
                _ => false,
            }
        {
            return self.open_context_menu_at_caret();
        }
        if !ui::active_overlay(self).is_some_and(|o| o.holds_keys())
            && let Some(msg) = self.debug_key(&key, modifiers)
        {
            return self.update(msg);
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
            && let Some(action) = self
                .keymap
                .action_for(&chord)
                .or_else(|| keymap::alias_for(&chord))
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
            // A page, the line's ends and — with ⌘ (Ctrl) — the file's: the
            // keys every editor reads by, which did nothing here. Only while
            // the code has the keys, as the motions below.
            Key::Named(named @ (Named::PageUp | Named::PageDown | Named::Home | Named::End))
                if self.code_focused
                    && !self.proj.finder.open
                    && self.proj.context_menu.is_none()
                    && self.active_viewer().is_some() =>
            {
                self.pending_g = false;
                self.pending_z = false;
                match (named, cmd) {
                    (Named::PageDown, _) => self.scroll_pages(1),
                    (Named::PageUp, _) => self.scroll_pages(-1),
                    (Named::Home, true) => self.move_cursor(viewer::Motion::FileStart),
                    (Named::End, true) => self.move_cursor(viewer::Motion::FileEnd),
                    (Named::Home, false) => self.move_cursor(viewer::Motion::LineStart),
                    _ => self.move_cursor(viewer::Motion::LineEnd),
                }
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
                    _ => Task::none(),
                }
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
            Key::Character("G") => self.move_cursor(viewer::Motion::LastLine),
            _ => Task::none(),
        }
    }

    /// A key the overlay on top (`ui::active_overlay`) takes for itself,
    /// besides the Esc every overlay takes: the finder's ↑/↓, which move its
    /// selection; a menu's, which put the keyboard's ring on its first row
    /// (↓) or its last (↑), from where the arrows walk its rows and Enter
    /// presses one — they did nothing, and a menu was the mouse's alone.
    /// Any other plain key, a menu up, is its to swallow: ←, →, j, k, Enter
    /// moved and stepped the code under the menu — and once the ring was
    /// up, ← and → walked the menu instead. A function key or a chord does
    /// what it does. `None` when the key is not the overlay's to take.
    fn overlay_key(
        &mut self,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        use keyboard::Key;
        use keyboard::key::Named;
        let overlay = ui::active_overlay(self)?;
        if overlay.is_menu() && self.ring.is_none() {
            let chord = modifiers.command() || modifiers.control() || modifiers.alt();
            return match key.as_ref() {
                Key::Named(Named::ArrowDown) => Some(self.on_tab(true, false)),
                Key::Named(Named::ArrowUp) => Some(self.on_tab(false, false)),
                Key::Named(named) if chord || is_function_key(named) => None,
                _ if chord => None,
                _ => Some(Task::none()),
            };
        }
        let delta = match (overlay, key.as_ref()) {
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
            // Searching text: a list of references (or definitions) in the
            // search's place is put away first, as its × does, for the
            // field to be there — it stayed, with no field, and what was
            // typed went nowhere.
            ProjectSearch => {
                let left = if self.proj.search.listing.is_some() {
                    self.update(Message::Nav(NavMsg::ListingClosed))
                } else {
                    Task::none()
                };
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Search,
                )));
                Task::batch([left, shown])
            }
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
                Message::Settings(SettingsMsg::NeedKey)
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
        // A menu's command (⌘J and every key the menu bar takes on macOS)
        // puts the ring away, as any other key does.
        let back = self.end_ring(true);
        let run = self.run_command_action(action).unwrap_or_else(Task::none);
        match back {
            Some(back) => Task::batch([back, run]),
            None => run,
        }
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

/// What a control pressing `press` is called on screen: the words a voice
/// would press it by.
fn ring_name(app: &App, press: &Message) -> Option<String> {
    let wanted = format!("{press:?}");
    ui::heard::gather(app)
        .into_iter()
        .find(|t| format!("{:?}", t.message) == wanted)
        .map(|t| t.shown)
}

/// Whether `named` is a function key (F1–F12): a shortcut of its own, as
/// a chord is, never typing.
fn is_function_key(named: keyboard::key::Named) -> bool {
    use keyboard::key::Named;
    matches!(
        named,
        Named::F1
            | Named::F2
            | Named::F3
            | Named::F4
            | Named::F5
            | Named::F6
            | Named::F7
            | Named::F8
            | Named::F9
            | Named::F10
            | Named::F11
            | Named::F12
    )
}
