//! The command palette (`>` in the finder): every action the window offers,
//! searchable in one place and run from the keyboard — the rebindable
//! commands, the toolbar's and the ⋯ menu's tools, the sidebar's tabs and
//! the panels, what the context menu does at the caret, the debugger's
//! controls, and the appearance.
//!
//! Cmd+Shift+P opens it as the command bar: with nothing typed it suggests
//! what to do here ([`Suggestion`]: ask about what the caret is in, talk, go
//! to a file or to the symbols around the caret, the commands run lately);
//! a query matches the commands and then the files.
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

/// One of the command bar's suggestions, made with nothing typed.
#[derive(Debug, Clone, PartialEq)]
pub enum Suggestion {
    /// Ask Clew: the Ask window, its suggestions about what the caret is
    /// in. The row's words.
    Ask(String),
    /// Talk instead of typing: voice.
    Talk,
    /// The file finder.
    GoToFile,
    /// A symbol of the open file, from the one the caret is in on.
    Symbol {
        name: String,
        /// The file's name, for the row.
        file: String,
        /// 1-based.
        line: usize,
        abs: std::path::PathBuf,
    },
    /// A palette command, by its title: one that opens a project when none
    /// is open, else one run lately.
    Command { title: String, recent: bool },
}

impl Suggestion {
    /// The heading it is listed under.
    pub(crate) fn group(&self) -> &'static str {
        match self {
            Suggestion::Ask(_) | Suggestion::Talk => "Ask",
            Suggestion::Command { recent: true, .. } => "Recent",
            Suggestion::GoToFile | Suggestion::Symbol { .. } | Suggestion::Command { .. } => {
                "Go to"
            }
        }
    }
}

/// Most symbols of the open file the command bar suggests.
const SUGGESTED_SYMBOLS: usize = 3;
/// Most commands run lately the command bar suggests.
const SUGGESTED_RECENT: usize = 3;

impl PaletteCommand {
    /// What running it costs, for the card that asks first: the mark the
    /// card carries and what it does, in a sentence.
    pub(crate) fn cost(&self) -> (crate::glyph::Glyph, &'static str) {
        use crate::glyph::Glyph;
        use keymap::Action;
        let message = match &self.action {
            PaletteAction::Send(message) | PaletteAction::AtCaret(message) => message,
            PaletteAction::Goto(_) | PaletteAction::Insight => {
                return (Glyph::Play, "This only changes what the window shows.");
            }
        };
        match message {
            Message::Editor(EditorMsg::RunAction(Action::StartDebug)) | Message::Debug(_) => {
                (Glyph::Debug, "This builds the program and runs it.")
            }
            Message::Editor(EditorMsg::RunAction(Action::ExplainAll)) | Message::Explain(_) => (
                Glyph::Sparkle,
                "This asks the language model about the code, which costs money.",
            ),
            Message::Project(ProjectMsg::OpenFolderPressed) => {
                (Glyph::Folder, "This opens another folder in this window.")
            }
            Message::Connect(ConnectMsg::Open) => {
                (Glyph::Remote, "This connects to another computer.")
            }
            Message::Export(_) => (Glyph::Export, "This writes a file."),
            _ => (Glyph::Play, "This runs a command of the window."),
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
    /// Open the command bar: the finder, nothing typed, its suggestions
    /// showing.
    pub(crate) fn on_palette_opened(&mut self) -> Task<Message> {
        self.proj.finder.open = true;
        self.proj.finder.bar = true;
        self.proj.finder.mode = FinderMode::Files;
        self.proj.finder.query.clear();
        self.code_focused = false;
        self.refresh_finder();
        operation::focus(ui::finder_input_id())
    }

    /// Rebuild the command bar's suggestions; they are its results.
    pub(crate) fn refresh_suggestions(&mut self) {
        self.palette = self.palette_commands();
        self.palette_home = self.suggestions();
        self.proj.finder.selected = 0;
        self.proj.finder.results = (0..self.palette_home.len()).collect();
    }

    /// Rank the commands and then the files against what the command bar
    /// has typed.
    pub(crate) fn refresh_bar(&mut self) {
        self.palette = self.palette_commands();
        let titles: Vec<&str> = self.palette.iter().map(|c| c.title.as_str()).collect();
        let files = self
            .proj
            .project
            .as_ref()
            .map(|p| p.files.clone())
            .unwrap_or_default();
        self.proj.finder.refresh_bar(&titles, &files);
    }

    /// What the command bar suggests here, in the order it lists them.
    pub(crate) fn suggestions(&self) -> Vec<Suggestion> {
        let mut out = Vec::new();
        let about = match self.cursor_target() {
            Some(crate::explain::Node::Function { name, .. }) => Some(name),
            Some(crate::explain::Node::File(p)) => {
                p.file_name().and_then(|n| n.to_str()).map(str::to_string)
            }
            _ => None,
        };
        out.push(Suggestion::Ask(match about {
            Some(about) => format!("Ask Clew about {about}"),
            None => "Ask Clew a question".to_string(),
        }));
        out.push(Suggestion::Talk);
        let listed = |title: &str| self.palette.iter().any(|c| c.title == title);
        if self.proj.project.is_some() {
            out.push(Suggestion::GoToFile);
            if let Some(v) = self.active_viewer() {
                let file = v
                    .abs
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                // From the innermost symbol the caret is in, else the first
                // after it, else the top.
                let at = v.caret.map(|(l, _)| l + 1);
                let start = at
                    .and_then(|line| {
                        v.symbols
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| s.line <= line && line <= s.end_line)
                            .min_by_key(|(_, s)| s.end_line.saturating_sub(s.line))
                            .map(|(i, _)| i)
                            .or_else(|| v.symbols.iter().position(|s| s.line > line))
                    })
                    .unwrap_or(0);
                for s in v.symbols.iter().skip(start).take(SUGGESTED_SYMBOLS) {
                    out.push(Suggestion::Symbol {
                        name: s.name.clone(),
                        file: file.clone(),
                        line: s.line,
                        abs: v.abs.clone(),
                    });
                }
            }
        } else {
            for title in ["Open Folder…", "Open Remote…"] {
                if listed(title) {
                    out.push(Suggestion::Command {
                        title: title.to_string(),
                        recent: false,
                    });
                }
            }
        }
        let shown: Vec<String> = out
            .iter()
            .filter_map(|s| match s {
                Suggestion::Command { title, .. } => Some(title.clone()),
                _ => None,
            })
            .collect();
        for title in self
            .palette_recent
            .iter()
            .filter(|t| listed(t) && !shown.contains(t))
            .take(SUGGESTED_RECENT)
        {
            out.push(Suggestion::Command {
                title: title.clone(),
                recent: true,
            });
        }
        out
    }

    /// How many of the command bar's headings sit over its selected row, the
    /// one under which it is listed among them; none outside its suggestions.
    pub(crate) fn headings_over_selection(&self) -> usize {
        if !self.proj.finder.is_home() {
            return 0;
        }
        let mut group = "";
        let mut headings = 0;
        for suggestion in self.palette_home.iter().take(self.proj.finder.selected + 1) {
            if suggestion.group() != group {
                group = suggestion.group();
                headings += 1;
            }
        }
        headings
    }

    /// Do what a suggestion offers; the command bar closes.
    pub(crate) fn run_suggestion(&mut self, suggestion: Suggestion) -> Task<Message> {
        if let Suggestion::Command { title, .. } = &suggestion {
            return self.run_palette(title);
        }
        self.proj.finder.open = false;
        self.code_focused = true;
        match suggestion {
            Suggestion::Ask(_) => self.update(Message::Ask(AskMsg::Open)),
            Suggestion::Talk => self.update(Message::Voice(VoiceMsg::Toggle)),
            Suggestion::GoToFile => self.on_finder_opened(FinderMode::Files),
            Suggestion::Symbol { abs, line, .. } => self.open_file(abs, Some(line), true),
            Suggestion::Command { .. } => Task::none(),
        }
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
            Some(crate::keymap::local_keys("⌘,")),
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
                (
                    self.show_inline_blame,
                    "Inline Blame",
                    Message::Window(WindowMsg::ToggleInlineBlame),
                ),
                (
                    self.show_indent_guides,
                    "Indent Guides",
                    Message::Window(WindowMsg::ToggleIndentGuides),
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
            // The note on the function the caret is in, block by block or
            // summed up.
            if matches!(
                self.proj.explain.view,
                Some(crate::explain::Node::Function { .. })
            ) {
                let title = if self.proj.explain.showing_detail {
                    "Explain: Summary"
                } else {
                    "Explain: In Blocks"
                };
                add(
                    title.into(),
                    None,
                    send(Message::Explain(ExplainMsg::ToggleBlocks)),
                );
            }
            // Where the conversation is, its sessions, and clearing it.
            add(
                "Ask: New Session".into(),
                None,
                send(Message::Ask(AskMsg::NewSession)),
            );
            if self.ask_view.is_open() {
                add("Ask: Hide".into(), None, send(Message::Ask(AskMsg::Hide)));
            }
            if self.ask_view != AskView::Expanded {
                add(
                    "Ask: Make Large".into(),
                    None,
                    send(Message::Ask(AskMsg::Expand)),
                );
            }
            if !self.proj.ask_turns.is_empty() {
                add(
                    "Ask: Clear Conversation".into(),
                    None,
                    send(Message::Ask(AskMsg::Clear)),
                );
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
