//! Keyboard input: key handling (Vim-style motions and prefixes, the finder
//! and find keys), the customizable command actions, and capturing a rebind.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn handle_key(
        &mut self,
        key: keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        use keyboard::Key;
        use keyboard::key::Named;

        let cmd = modifiers.command();

        // Rebinding capture takes priority: the next chord becomes the binding.
        if let Some(action) = self.rebinding {
            return self.capture_rebind(action, &key, modifiers);
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
        // While the shortcuts panel is open (and not capturing), swallow every
        // key so nothing acts behind the modal. (Esc never gets here: the
        // panel is an overlay, so `escape_message` above closed it.)
        if self.show_shortcuts {
            return Task::none();
        }
        // Tab / Shift-Tab move keyboard focus between fields (inside the open
        // modal, if any). Focus leaves the code view, so its motions stop.
        if matches!(key.as_ref(), Key::Named(Named::Tab))
            && !(cmd || modifiers.alt() || modifiers.control())
        {
            self.code_focused = false;
            return ui::cycle_focus(!modifiers.shift());
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
            // Only what is NOT an overlay is left for Esc here — the find bar,
            // then the selection. Every overlay (the context menu and the
            // finder included) was dismissed through `escape_message` above.
            Key::Named(Named::Escape) => {
                self.pending_g = false;
                self.pending_z = false;
                if self.proj.find.open {
                    return self.update(Message::Editor(EditorMsg::FindClosed));
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
            // Two-key `g` prefix: gg / gd / gr / gi / gy / gc.
            _ if self.pending_g => {
                self.pending_g = false;
                match key.as_ref() {
                    Key::Character("g") => self.move_cursor(viewer::Motion::FileStart),
                    Key::Character("d") => self.goto_at_cursor(GotoKind::Definition),
                    Key::Character("r") => self.goto_at_cursor(GotoKind::References),
                    Key::Character("i") => self.goto_at_cursor(GotoKind::Implementation),
                    Key::Character("y") => self.goto_at_cursor(GotoKind::TypeDefinition),
                    Key::Character("c") => self.update(Message::Calls(CallsMsg::Requested)),
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
        Some(ui::reveal_finder_selection(self.proj.finder.selected))
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
            ToggleAsk => self.update(Message::Ask(AskMsg::Toggle)),
            StartDebug => self.update(Message::Debug(DebugMsg::Start)),
            CallGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectCalls))),
            TypeGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(Overlay::ProjectTypes))),
            ImportGraph => self.update(Message::Graph(GraphMsg::OpenOverlay(
                Overlay::ProjectImports,
            ))),
            // The same as the ⋯ menu's row: cancel a running pass, else start
            // one (or ask for a key first).
            ExplainAll => self.update(if self.proj.explain.running {
                Message::Explain(ExplainMsg::Cancel)
            } else if self.llm_available {
                Message::Explain(ExplainMsg::Project)
            } else {
                Message::Settings(SettingsMsg::Open)
            }),
            ToggleDiff => self.update(Message::Editor(EditorMsg::ToggleDiff)),
            TimeTravel => self.update(Message::TimeTravel(TimeTravelMsg::Start { symbol: false })),
            Walkthrough => self.update(Message::Window(WindowMsg::SidebarTabPicked(
                SidebarTab::Walk,
            ))),
            LspServers => self.update(Message::Lsp(LspMsg::TogglePanel)),
            Shortcuts => self.update(Message::Window(WindowMsg::OpenShortcuts)),
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
        if self.tutorial.is_some() || self.show_shortcuts {
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
            self.keymap_notice = Some("Unsupported key".into());
            return Task::none();
        };
        self.capture_chord(action, chord)
    }

    /// Bind `chord` to `action` (the rebind capture), unless it is not a
    /// command chord or another action holds it.
    fn capture_chord(&mut self, action: keymap::Action, chord: keymap::Chord) -> Task<Message> {
        if !chord.is_command() {
            self.keymap_notice = Some("Shortcut must include ⌘, ⌥, or ⌃".into());
            return Task::none();
        }
        if let Some(other) = self.keymap.conflict(&chord, action) {
            self.keymap_notice = Some(format!("Already used by “{}”", other.label()));
            return Task::none();
        }
        self.keymap.rebind(action, chord);
        self.rebinding = None;
        self.keymap_notice = None;
        if let Err(e) = self.keymap.save() {
            self.status = format!("Could not save shortcuts: {e}");
        }
        Task::none()
    }
}
