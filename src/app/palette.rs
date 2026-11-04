//! The command palette (Cmd+Shift+P, or `>` in the finder): every action the
//! window offers, searchable in one place and run from the keyboard — the
//! rebindable commands, the toolbar's and the ⋯ menu's tools, the sidebar's
//! tabs and the panels, what the context menu does at the caret, the
//! debugger's controls, and the appearance.
//!
//! The list is built from the window's state each time the palette refreshes
//! ([`App::palette_commands`]): a command that cannot run here (no project,
//! no caret, no debug session) is not listed. Running one sends the very
//! message its button or menu row sends, so the two can never disagree.

use crate::app::prelude::*;
use crate::*;

/// Most commands the palette remembers as recently run.
const MAX_RECENT: usize = 8;

/// One command in the palette.
#[derive(Debug, Clone)]
pub(crate) struct PaletteCommand {
    /// What the row says; unique in the list, and how a click finds it.
    pub(crate) title: String,
    /// Its key, when it has one: a chord (`⌘⇧C`) or a reading key (`gd`).
    pub(crate) keys: Option<String>,
    pub(crate) action: PaletteAction,
}

impl PaletteCommand {
    /// Whether running it costs something beyond what the window shows —
    /// a paid pass over the project, the program started or stepped, a
    /// breakpoint set in it, another project opened, a file written — so
    /// that a command run for the user (by voice, by the agent) asks first.
    /// Everything else only changes what is on screen.
    pub(crate) fn costly(&self) -> bool {
        use keymap::Action;
        match &self.action {
            PaletteAction::Send(message) | PaletteAction::AtCaret(message) => matches!(
                message,
                Message::Editor(EditorMsg::RunAction(
                    Action::ExplainAll | Action::StartDebug
                )) | Message::Debug(_)
                    | Message::Explain(_)
                    | Message::Project(ProjectMsg::OpenFolderPressed)
                    | Message::Connect(ConnectMsg::Open)
                    | Message::Export(_)
            ),
            PaletteAction::Goto(_) | PaletteAction::Insight => false,
        }
    }
}

/// What running a command does.
#[derive(Debug, Clone)]
pub(crate) enum PaletteAction {
    /// Sends this message.
    Send(Message),
    /// Sends this context-menu message, the menu placed at the caret: what
    /// right-clicking there and picking the row does.
    AtCaret(Message),
    /// Goes to a definition (or references, …) at the caret, as `gd` does.
    Goto(GotoKind),
    /// The language's own action at the caret, as `ge` does.
    Insight,
}

impl App {
    /// Open the palette: the finder, its query `>`.
    pub(crate) fn on_palette_opened(&mut self) -> Task<Message> {
        self.proj.finder.open = true;
        self.proj.finder.mode = FinderMode::Files;
        self.proj.finder.query = ">".to_string();
        self.code_focused = false;
        self.refresh_finder();
        Task::batch([
            operation::focus(ui::finder_input_id()),
            operation::move_cursor_to_end(ui::finder_input_id()),
        ])
    }

    /// Rebuild the command list and rank it against the finder's query.
    pub(crate) fn refresh_palette(&mut self) {
        self.palette = self.palette_commands();
        let titles: Vec<&str> = self.palette.iter().map(|c| c.title.as_str()).collect();
        self.proj
            .finder
            .refresh_commands(&titles, &self.palette_recent);
    }

    /// Run the command titled `title` — looked up afresh, so a click on a
    /// row the window has since outgrown (the debug session ended) runs
    /// nothing rather than another command.
    pub(crate) fn run_palette(&mut self, title: &str) -> Task<Message> {
        self.proj.finder.open = false;
        self.code_focused = true;
        let Some(command) = self
            .palette_commands()
            .into_iter()
            .find(|c| c.title == title)
        else {
            return Task::none();
        };
        self.palette_recent.retain(|r| r != title);
        self.palette_recent.insert(0, title.to_string());
        self.palette_recent.truncate(MAX_RECENT);
        match command.action {
            PaletteAction::Send(message) => self.update(message),
            PaletteAction::AtCaret(message) => {
                let Some((pane, (line, col))) = self.caret() else {
                    return Task::none();
                };
                let insight = self
                    .active_viewer()
                    .and_then(|v| v.lang_key)
                    .and_then(InsightKind::for_lang);
                self.proj.context_menu = Some(ContextMenu {
                    pane,
                    line,
                    col,
                    x: 0.0,
                    y: 0.0,
                    insight,
                });
                self.update(message)
            }
            PaletteAction::Goto(kind) => self.goto_at_cursor(kind),
            PaletteAction::Insight => self.insight_at_cursor(),
        }
    }

    /// The active pane and its caret, when it has one.
    fn caret(&self) -> Option<(usize, (usize, usize))> {
        Some((self.proj.active, self.active_viewer()?.caret?))
    }

    /// Every command the window offers now, in the order the palette lists
    /// them with nothing typed.
    pub(crate) fn palette_commands(&self) -> Vec<PaletteCommand> {
        let mut out = Vec::new();
        let project = self.proj.project.is_some();
        let mut add = |title: String, keys: Option<String>, action: PaletteAction| {
            out.push(PaletteCommand {
                title,
                keys,
                action,
            });
        };
        let send = PaletteAction::Send;

        // The rebindable commands, by their menu titles, with their chords.
        for action in keymap::Action::ALL {
            if action == keymap::Action::CommandPalette {
                continue;
            }
            let global = matches!(
                action,
                keymap::Action::ZoomIn
                    | keymap::Action::ZoomOut
                    | keymap::Action::ZoomReset
                    | keymap::Action::Shortcuts
            );
            if !(project || global) {
                continue;
            }
            add(
                action.menu().title.to_string(),
                Some(self.keymap.chord(action).caps()),
                send(Message::Editor(EditorMsg::RunAction(action))),
            );
        }

        // At the caret: what the context menu and the reading keys do there.
        if project && self.caret().is_some() {
            for (kind, keys) in [
                (GotoKind::Definition, "gd"),
                (GotoKind::References, "gr"),
                (GotoKind::Implementation, "gi"),
                (GotoKind::TypeDefinition, "gy"),
            ] {
                add(
                    kind.label().to_string(),
                    Some(keys.into()),
                    PaletteAction::Goto(kind),
                );
            }
            if let Some(kind) = self
                .active_viewer()
                .and_then(|v| v.lang_key)
                .and_then(InsightKind::for_lang)
            {
                add(
                    kind.label().to_string(),
                    Some("ge".into()),
                    PaletteAction::Insight,
                );
            }
            let at_caret = |m| PaletteAction::AtCaret(m);
            for (title, keys, message) in [
                (
                    "Call Hierarchy",
                    Some("gc"),
                    Message::Calls(CallsMsg::FromMenu),
                ),
                ("View Docs", None, Message::Docs(DocsMsg::ViewFromMenu)),
                ("Explain", None, Message::Explain(ExplainMsg::FromMenu)),
                ("Add to Ask", None, Message::Ask(AskMsg::AboutSelection)),
                (
                    "Why Is This Here?",
                    None,
                    Message::TimeTravel(TimeTravelMsg::WhyIsThisHere),
                ),
                (
                    "Toggle Breakpoint",
                    None,
                    Message::Debug(DebugMsg::ToggleBreakpointFromMenu),
                ),
                (
                    "Conditional Breakpoint…",
                    None,
                    Message::Debug(DebugMsg::ConditionalBreakpointFromMenu),
                ),
            ] {
                add(
                    title.to_string(),
                    keys.map(str::to_string),
                    at_caret(message),
                );
            }
        }

        // The debugger, while a session runs.
        if let Some(session) = &self.debug.session {
            if session.status == DebugStatus::Stopped {
                for (title, cmd) in [
                    ("Debug: Continue", DebugCmd::Continue),
                    ("Debug: Step Over", DebugCmd::StepOver),
                    ("Debug: Step Into", DebugCmd::StepIn),
                    ("Debug: Step Out", DebugCmd::StepOut),
                ] {
                    add(
                        title.into(),
                        None,
                        send(Message::Debug(DebugMsg::Control(cmd))),
                    );
                }
            }
            add(
                "Debug: Stop".into(),
                None,
                send(Message::Debug(DebugMsg::Stop)),
            );
        }

        // The toolbar's own tools.
        if project {
            add(
                "Overview".into(),
                None,
                send(Message::Overview(OverviewMsg::Show)),
            );
            add(
                "Stats".into(),
                None,
                send(Message::Overview(OverviewMsg::ShowStats)),
            );
        }
        add(
            "Settings".into(),
            Some("⌘,".into()),
            send(Message::Settings(SettingsMsg::Open)),
        );

        // The sidebar's tabs and the panels.
        if project {
            for (label, tab) in ui::SIDEBAR_TABS {
                add(
                    format!("Sidebar: {}", title_case(label)),
                    None,
                    send(Message::Window(WindowMsg::SidebarTabPicked(tab))),
                );
            }
            let toggle = |shown: bool, what: &str| {
                format!("View: {} {what}", if shown { "Hide" } else { "Show" })
            };
            add(
                toggle(self.show_left_sidebar, "Sidebar"),
                None,
                send(Message::Window(WindowMsg::ToggleLeftSidebar)),
            );
            add(
                toggle(self.show_right_panel, "Context Panel"),
                None,
                send(Message::Window(WindowMsg::ToggleRightPanel)),
            );
            add(
                toggle(self.show_bottom, "Debug Panel"),
                None,
                send(Message::Window(if self.show_bottom {
                    WindowMsg::CollapseBottom
                } else {
                    WindowMsg::ShowDebugPanel
                })),
            );
            // The ⋯ menu's view toggles.
            for (shown, what, message) in [
                (
                    self.show_inline_summaries,
                    "Outline Summaries",
                    Message::Window(WindowMsg::ToggleInlineSummaries),
                ),
                (
                    self.show_file_banner,
                    "File Summary",
                    Message::Window(WindowMsg::ToggleFileBanner),
                ),
                (
                    self.show_inlay_hints,
                    "Inlay Hints",
                    Message::Lsp(LspMsg::ToggleInlayHints),
                ),
                (
                    self.show_minimap,
                    "Minimap",
                    Message::Window(WindowMsg::ToggleMinimap),
                ),
            ] {
                add(toggle(shown, what), None, send(message));
            }
            // The ⋯ menu's tools that have no chord of their own.
            for (title, message) in [
                ("Glossary", Message::Glossary(GlossaryMsg::Open)),
                ("Skim (Fold Bodies)", Message::Editor(EditorMsg::SkimFile)),
                ("Export Notes…", Message::Export(ExportMsg::Start)),
                ("Tutorial", Message::Tutorial(TutorialMsg::Start)),
            ] {
                add(title.into(), None, send(message));
            }
        }
        add(
            "Open Folder…".into(),
            None,
            send(Message::Project(ProjectMsg::OpenFolderPressed)),
        );
        add(
            "Open Remote…".into(),
            None,
            send(Message::Connect(ConnectMsg::Open)),
        );

        // The appearance.
        for (pref, name) in [
            (theme::ThemePref::Light, "Light"),
            (theme::ThemePref::Dark, "Dark"),
            (theme::ThemePref::System, "Match System"),
        ] {
            add(
                format!("Theme: {name}"),
                None,
                send(Message::Settings(SettingsMsg::SetThemePref(pref))),
            );
        }
        out
    }
}

/// `FILES` → `Files`.
fn title_case(label: &str) -> String {
    let mut chars = label.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect(),
        None => String::new(),
    }
}
