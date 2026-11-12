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

/// Whether `msg` uses the AI model: explaining, asking why, a fake run,
/// a written overview, story or walkthrough.
pub(crate) fn spends_ai(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Editor(EditorMsg::RunAction(
            keymap::Action::ExplainAll | keymap::Action::FakeRun | keymap::Action::FakeDebug
        )) | Message::Explain(
            ExplainMsg::FromMenu | ExplainMsg::Project | ExplainMsg::ReexplainNode
        ) | Message::TimeTravel(
            TimeTravelMsg::WhyIsThisHere | TimeTravelMsg::Why | TimeTravelMsg::Story
        ) | Message::Fake(FakeMsg::FromMenu { .. } | FakeMsg::AtCaret { .. })
            | Message::Overview(OverviewMsg::Generate)
            | Message::Walk(WalkMsg::Generate(_) | WalkMsg::GenerateTrace | WalkMsg::GenerateDiff)
            | Message::Ask(AskMsg::Suggested(_))
    )
}

/// The command bar's title for asking `question` of Clew: in English, as
/// every command's title is ([`PaletteCommand::title`]), and read back by
/// [`asked_in`].
fn ask_title(question: &str) -> String {
    format!("{ASK_PREFIX}{question}{ASK_SUFFIX}")
}

const ASK_PREFIX: &str = "Ask Clew: \u{201c}";
const ASK_SUFFIX: &str = "\u{201d}";
/// The end of the title of a row that only puts what was typed in Ask's
/// field (`draft_title`): the ellipsis of a command that asks for more.
const DRAFT_SUFFIX: &str = "\u{201d}\u{2026}";

/// The question a command bar row titled `title` asks, if it asks one.
pub(crate) fn asked_in(title: &str) -> Option<&str> {
    title.strip_prefix(ASK_PREFIX)?.strip_suffix(ASK_SUFFIX)
}

/// The command bar's title for opening Ask with `typed` in its field,
/// asking nothing yet: read back by [`drafted_in`].
fn draft_title(typed: &str) -> String {
    format!("{ASK_PREFIX}{typed}{DRAFT_SUFFIX}")
}

/// What a command bar row titled `title` puts in Ask's field, if it is
/// such a row.
pub(crate) fn drafted_in(title: &str) -> Option<&str> {
    title.strip_prefix(ASK_PREFIX)?.strip_suffix(DRAFT_SUFFIX)
}

/// The command bar's row that asks `question` of Clew, in the Ask window.
fn ask_command(question: &str) -> PaletteCommand {
    PaletteCommand {
        title: ask_title(question),
        keys: None,
        action: PaletteAction::Send(Message::Ask(AskMsg::Suggested(question.to_string()))),
        shown_as: Some(tr!("Ask Clew: “{question}”", question = question)),
    }
}

/// The command bar's row offered where nothing matched what was typed and
/// it reads as no question — a name mistyped, likely: it opens Ask with
/// the words in its field, to be asked once they are. Asked at once, as a
/// question's row is, Enter on "mian.rs" spent an AI call on a typo.
fn draft_command(typed: &str) -> PaletteCommand {
    PaletteCommand {
        title: draft_title(typed),
        keys: None,
        action: PaletteAction::Send(Message::Ask(AskMsg::Open)),
        shown_as: Some(tr!("Ask Clew about “{question}”…", question = typed)),
    }
}

/// Whether what was typed into the command bar reads as a question to ask:
/// it has a question mark, or three words or more — "how is tax
/// computed" — where a command's or a file's name is a word or two.
pub(crate) fn looks_like_a_question(typed: &str) -> bool {
    typed.contains(['?', '？']) || typed.split_whitespace().count() >= 3
}

/// One command in the palette.
#[derive(Debug, Clone)]
pub(crate) struct PaletteCommand {
    /// Its name, in English whatever the interface speaks: unique in the
    /// list, how a click, the voice and the agent find it, and what the
    /// recent commands keep. The row shows it translated
    /// ([`PaletteCommand::shown`]).
    pub(crate) title: String,
    /// Its key, when it has one: a chord (`⌘⇧C`) or a reading key (`gd`).
    pub(crate) keys: Option<String>,
    pub(crate) action: PaletteAction,
    /// What the row says, when its title is made of parts the catalogs
    /// translate one by one ("Language: French" — "Langue : Français").
    pub(crate) shown_as: Option<String>,
}

impl PaletteCommand {
    /// Whether running it uses the AI model (and so costs money): its row
    /// is marked so, as every control that does is.
    pub(crate) fn spends_ai(&self) -> bool {
        match &self.action {
            PaletteAction::Send(m) | PaletteAction::AtCaret(m) => spends_ai(m),
            PaletteAction::Goto(_) | PaletteAction::Insight => false,
        }
    }

    /// What the row says: its title, in the reader's language.
    pub(crate) fn shown(&self) -> &str {
        self.shown_as
            .as_deref()
            .unwrap_or_else(|| crate::i18n::lookup(&self.title))
    }

    /// What a query is matched against: what the row says, and the other
    /// words it goes by ([`also_called`]).
    fn searched(&self) -> String {
        with_aliases(self.shown(), &self.title)
    }

    /// [`Self::searched`], in English.
    fn searched_in_english(&self) -> String {
        with_aliases(&self.title, &self.title)
    }

    /// Whether running it costs something beyond what the window shows —
    /// a paid pass over the project, the program started, stepped or ended,
    /// a breakpoint set in it, another project opened, a file written — so
    /// that a command the agent runs asks the user first. Everything else
    /// only changes what is on screen.
    pub(crate) fn costly(&self) -> bool {
        use keymap::Action;
        match &self.action {
            PaletteAction::Send(message) | PaletteAction::AtCaret(message) => match message {
                // What only shows, or stops spending, costs nothing: a
                // variable opened, a condition or watch being typed, an
                // explanation shown, an explain pass stopped.
                Message::Debug(
                    DebugMsg::ToggleVar(_)
                    | DebugMsg::WatchInput(_)
                    | DebugMsg::BpConditionInput(_)
                    | DebugMsg::BpConditionCancel,
                )
                | Message::Explain(ExplainMsg::Cancel | ExplainMsg::Show(_)) => false,
                _ => matches!(
                    message,
                    Message::Editor(EditorMsg::RunAction(
                        Action::ExplainAll | Action::StartDebug | Action::Run
                    )) | Message::Debug(_)
                        | Message::Explain(_)
                        | Message::Project(
                            ProjectMsg::OpenFolderPressed
                                | ProjectMsg::FolderPicked(_)
                                | ProjectMsg::CloseFolder
                        )
                        | Message::Connect(ConnectMsg::Open)
                        | Message::Export(_)
                ),
            },
            PaletteAction::Goto(_) | PaletteAction::Insight => false,
        }
    }

    /// Whether the agent, running it, asks the user first: what costs
    /// something ([`Self::costly`]), and a setting that changes what the
    /// microphone takes in and what is done with it, where the project's
    /// code is sent, or what is downloaded — the reader's to decide, never
    /// a model's, which could otherwise turn on a microphone that listens
    /// on and sends what it hears. The reader saying such a setting
    /// changes it, as clicking it does.
    pub(crate) fn asks_the_agent(&self) -> bool {
        self.costly() || self.private()
    }

    /// Whether it is one of the settings [`Self::asks_the_agent`] keeps to
    /// the reader.
    fn private(&self) -> bool {
        let (PaletteAction::Send(message) | PaletteAction::AtCaret(message)) = &self.action else {
            return false;
        };
        matches!(
            message,
            Message::Voice(
                VoiceMsg::SetSendQuestions(_)
                    | VoiceMsg::SetRunCommands(_)
                    | VoiceMsg::SetTalkOver(_)
                    | VoiceMsg::SetWake(_)
                    | VoiceMsg::PickModel(_)
            ) | Message::Editor(EditorMsg::RunAction(crate::keymap::Action::Voice))
                | Message::Settings(SettingsMsg::ProviderPicked(_) | SettingsMsg::EmbedHere(_))
                | Message::Updater(UpdaterMsg::SetAuto(_))
        )
    }

    /// Whether the reader, saying it, is asked first: what [`Self::costly`]
    /// says, but for stepping the program they are debugging and marking
    /// where it stops — what they are doing, said, as a click does it, with
    /// no yes for each step. Starting one, ending it, or evaluating in it as
    /// the mouse moves is asked.
    pub(crate) fn asks_when_said(&self) -> bool {
        let stepping = matches!(
            &self.action,
            PaletteAction::Send(Message::Debug(
                DebugMsg::Control(_)
                    | DebugMsg::BreakpointToggle { .. }
                    | DebugMsg::WatchAdd
                    | DebugMsg::WatchRemoveExpr(_),
            )) | PaletteAction::AtCaret(Message::Debug(
                DebugMsg::ToggleBreakpointFromMenu | DebugMsg::ConditionalBreakpointFromMenu,
            ))
        );
        self.costly() && !stepping
    }
}

/// One of the command bar's suggestions, made with nothing typed.
#[derive(Debug, Clone, PartialEq)]
pub enum Suggestion {
    /// Ask Clew: the Ask window, its suggestions about what the caret is
    /// in. The row's words.
    Ask(String),
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
    /// A palette command, by its title (in English, as
    /// [`PaletteCommand::title`]): one that opens a project when none is
    /// open, else one run lately.
    Command { title: String, recent: bool },
}

impl Suggestion {
    /// The heading it is listed under, in the reader's language.
    pub(crate) fn group(&self) -> &'static str {
        match self {
            Suggestion::Ask(_) => tr!("Ask"),
            Suggestion::Command { recent: true, .. } => tr!("Recent"),
            Suggestion::GoToFile | Suggestion::Symbol { .. } | Suggestion::Command { .. } => {
                tr!("Go to")
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
                return (Glyph::Play, tr!("This only changes what the window shows."));
            }
        };
        match message {
            Message::Editor(EditorMsg::RunAction(Action::StartDebug))
            | Message::Debug(DebugMsg::Start) => {
                (Glyph::Debug, tr!("This builds the program and runs it."))
            }
            Message::Editor(EditorMsg::RunAction(Action::Run)) | Message::Debug(DebugMsg::Run) => (
                Glyph::Play,
                tr!("This builds the program and runs it, without the debugger."),
            ),
            Message::Debug(DebugMsg::Control(_)) => (
                Glyph::Debug,
                tr!("This resumes the program from where it stopped."),
            ),
            Message::Debug(DebugMsg::Stop) => {
                (Glyph::Debug, tr!("This ends the program being debugged."))
            }
            Message::Debug(DebugMsg::ToggleHoverEval) => (
                Glyph::Debug,
                tr!("This runs code in the program for each name the mouse rests on."),
            ),
            Message::Debug(_) => (Glyph::Debug, tr!("This changes where the program stops.")),
            Message::Editor(EditorMsg::RunAction(Action::ExplainAll)) | Message::Explain(_) => (
                Glyph::Sparkle,
                tr!("This uses the AI model, which costs money."),
            ),
            Message::Project(ProjectMsg::OpenFolderPressed | ProjectMsg::FolderPicked(_)) => (
                Glyph::FolderOpen,
                tr!("This opens another folder in this window."),
            ),
            Message::Project(ProjectMsg::CloseFolder) => (
                Glyph::Close,
                tr!("This closes the folder open in this window."),
            ),
            Message::Connect(ConnectMsg::Open) => {
                (Glyph::Remote, tr!("This connects to another computer."))
            }
            Message::Export(_) => (Glyph::Export, tr!("This writes a file.")),
            Message::Voice(VoiceMsg::PickModel(_)) => {
                (Glyph::Mic, tr!("This can download a voice model."))
            }
            Message::Voice(_) => (
                Glyph::Mic,
                tr!("This changes what the microphone hears, and what is done with it."),
            ),
            Message::Settings(SettingsMsg::ProviderPicked(_) | SettingsMsg::EmbedHere(_)) => {
                (Glyph::Network, tr!("This changes where your code is sent."))
            }
            Message::Updater(UpdaterMsg::SetAuto(_)) => (
                Glyph::Network,
                tr!("This changes whether Clew checks online for updates."),
            ),
            _ => (Glyph::Play, tr!("This runs one of the window's commands.")),
        }
    }
}

/// The other words a command titled `title` goes by — a name it had, the
/// short word people use — that a query finds it by, though its row does
/// not show them: "map" finds Project Graphs, once Project Maps, and
/// "stats" Code Statistics.
fn also_called(title: &str) -> &'static str {
    match title {
        "Project Graphs" => "map maps schema routes",
        "Type Graph" => "type map",
        "Code Statistics" => "stats",
        "Architecture Overview" => "overview",
        "Talk" => "voice microphone listen",
        "Diff with Last Commit" => "head diff vs head",
        _ => "",
    }
}

/// The name the command titled `title` had before it took that title,
/// heard as a title of its own when the words said name no command by its
/// title: "show the stats" still opens Code Statistics.
fn formerly(title: &str) -> Option<&'static str> {
    match title {
        "Project Graphs" => Some("Project Maps"),
        "Type Graph" => Some("Type Map"),
        "Code Statistics" => Some("Stats"),
        "Architecture Overview" => Some("Overview"),
        "Diff with Last Commit" => Some("Diff vs HEAD"),
        _ => None,
    }
}

/// The index in `commands` of the command `said` plainly names: by its
/// title, or, when no title is named, by the name it had ([`formerly`]).
pub(crate) fn command_said(said: &str, commands: &[PaletteCommand]) -> Option<usize> {
    let titles: Vec<&str> = commands.iter().map(|c| c.title.as_str()).collect();
    clew_voice::intent::command(said, &titles).or_else(|| {
        let (names, at): (Vec<&str>, Vec<usize>) = commands
            .iter()
            .enumerate()
            .filter_map(|(i, c)| formerly(&c.title).map(|name| (name, i)))
            .unzip();
        clew_voice::intent::command(said, &names).map(|k| at[k])
    })
}

/// `row`, and the words the command titled `title` also goes by.
fn with_aliases(row: &str, title: &str) -> String {
    match also_called(title) {
        "" => row.to_string(),
        also => format!("{row} {also}"),
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
        // What is on screen under it, for the bar to list by name.
        self.palette_screen = crate::ui::heard::gather(self);
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
    /// has typed: the commands by the titles their rows show, then any
    /// more that their English titles match.
    pub(crate) fn refresh_bar(&mut self) {
        self.palette = self.palette_commands();
        let on_screen = self.screen_commands(&self.palette);
        self.palette.extend(on_screen);
        let files = self
            .proj
            .project
            .as_ref()
            .map(|p| p.files.clone())
            .unwrap_or_default();
        let n = self.palette.len();
        let english = self.ranked_in_english(|finder, titles| {
            finder.refresh_bar(titles, &files);
            finder.bar_worded
        });
        let searched: Vec<String> = self.palette.iter().map(PaletteCommand::searched).collect();
        let searched: Vec<&str> = searched.iter().map(String::as_str).collect();
        self.proj.finder.refresh_bar(&searched, &files);
        if let Some((english, english_worded)) = english {
            let worded = self.proj.finder.bar_worded;
            let results = &mut self.proj.finder.results;
            let (mut commands, files): (Vec<usize>, Vec<usize>) =
                results.iter().partition(|&&i| i < n);
            // Titles that say what was typed, in either language, before
            // any that only hold its letters: the English merge brought
            // back "Thème clair : One Light" for "ligne", and an English
            // phrase typed in French came after six loose matches.
            let english = match (worded, english_worded) {
                (false, true) => {
                    commands.clear();
                    english
                }
                (true, false) => Vec::new(),
                _ => english,
            };
            let most = commands
                .len()
                .max(english.iter().filter(|&&i| i < n).count());
            for i in english.into_iter().filter(|&i| i < n) {
                if !commands.contains(&i) {
                    commands.push(i);
                }
            }
            commands.truncate(most);
            commands.extend(files);
            *results = commands;
        }
        // What was typed, asked of Clew: a row of its own, listed last.
        // Only the empty bar offered Ask, so a question typed into it found
        // commands and files and no way to ask it. Not ranked with the
        // commands — its title holds every word typed, and would have
        // been the one "worded" match that put the rest out — but added
        // after them, the files' places moved one on.
        // In the bar, and in the file picker searching as the bar does
        // (`bar_fallback`): the field over the window, and Ctrl+P.
        let question = self.proj.finder.query.trim().to_string();
        if (self.proj.finder.bar || self.proj.finder.bar_fallback) && !question.is_empty() {
            let ask_at = self.palette.len();
            let asks = looks_like_a_question(&question);
            self.palette.push(if asks {
                ask_command(&question)
            } else {
                draft_command(&question)
            });
            let results = &mut self.proj.finder.results;
            for i in results.iter_mut().filter(|i| **i >= ask_at) {
                *i += 1;
            }
            // Offered where it reads as a question — several words, or a
            // question mark — or, to be asked once written, where nothing
            // else was found.
            if asks || results.is_empty() {
                results.push(ask_at);
            }
        }
    }

    /// The finder's results ranked, by `rank`, against the commands'
    /// English titles — `None` while the interface speaks English, where
    /// those are the titles shown.
    fn ranked_in_english<T>(
        &mut self,
        rank: impl FnOnce(&mut crate::finder::Finder, &[&str]) -> T,
    ) -> Option<(Vec<usize>, T)> {
        if crate::i18n::current() == crate::i18n::Lang::En {
            return None;
        }
        let titles: Vec<String> = self
            .palette
            .iter()
            .map(PaletteCommand::searched_in_english)
            .collect();
        let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
        let said = rank(&mut self.proj.finder, &titles);
        Some((std::mem::take(&mut self.proj.finder.results), said))
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
            Some(about) => tr!("Ask Clew about “{name}”", name = about),
            None => tr!("Ask Clew a question").to_string(),
        }));
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
            for title in [tr_noop!("Open Folder…"), tr_noop!("Open Remote…")] {
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
            Suggestion::GoToFile => self.on_finder_opened(FinderMode::Files),
            Suggestion::Symbol { abs, line, .. } => self.open_file(abs, Some(line), true),
            Suggestion::Command { .. } => Task::none(),
        }
    }

    /// Rebuild the command list and rank it against the finder's query: by
    /// the titles their rows show, then any more that their English titles
    /// match.
    pub(crate) fn refresh_palette(&mut self) {
        self.palette = self.palette_commands();
        let on_screen = self.screen_commands(&self.palette);
        self.palette.extend(on_screen);
        // The recent commands, as the ranking finds them: by the words
        // each is matched against, in English and as shown.
        let recent: Vec<&PaletteCommand> = self
            .palette_recent
            .iter()
            .filter_map(|t| self.palette.iter().find(|c| &c.title == t))
            .collect();
        let recent_english: Vec<String> = recent.iter().map(|c| c.searched_in_english()).collect();
        let recent: Vec<String> = recent.iter().map(|c| c.searched()).collect();
        let english = self
            .ranked_in_english(|finder, titles| finder.refresh_commands(titles, &recent_english));
        let searched: Vec<String> = self.palette.iter().map(PaletteCommand::searched).collect();
        let searched: Vec<&str> = searched.iter().map(String::as_str).collect();
        self.proj.finder.refresh_commands(&searched, &recent);
        if let Some((english, ())) = english {
            let results = &mut self.proj.finder.results;
            for i in english {
                if !results.contains(&i) {
                    results.push(i);
                }
            }
        }
    }

    /// The controls on screen when the command bar opened that no command
    /// in `commands` already names, each a command of its own: what it
    /// says, and the press its click sends. Only the bar lists them, and
    /// only the reader runs them ([`App::run_from_bar`]).
    fn screen_commands(&self, commands: &[PaletteCommand]) -> Vec<PaletteCommand> {
        if !self.proj.finder.bar {
            return Vec::new();
        }
        let mut seen: std::collections::HashSet<&str> =
            commands.iter().map(|c| c.title.as_str()).collect();
        // A control named for one of the project's files — a tree row, a
        // crumb — is that file, which the bar lists as a file: listed again
        // as a command, it sat above itself.
        let files: std::collections::HashSet<&str> = self
            .proj
            .project
            .iter()
            .flat_map(|p| p.files.iter())
            .flat_map(|f| [f.rel.as_str(), f.rel.rsplit('/').next().unwrap_or(&f.rel)])
            .collect();
        self.palette_screen
            .iter()
            .filter(|t| t.typing.is_none() && !t.title.trim().is_empty())
            .filter(|t| !files.contains(t.title.as_str()))
            .filter(|t| seen.insert(t.title.as_str()))
            .map(|t| PaletteCommand {
                title: t.title.clone(),
                keys: None,
                action: PaletteAction::Send(t.message.clone()),
                shown_as: (t.shown != crate::i18n::lookup(&t.title)).then(|| t.shown.clone()),
            })
            .collect()
    }

    /// Run what the reader picked in the command bar: the command titled
    /// `title`, or else the control on screen it names ([`Self::screen_commands`]),
    /// pressed as a click presses it. The agent runs commands by
    /// [`Self::run_palette`], which never reaches what is on screen.
    pub(crate) fn run_from_bar(&mut self, title: &str) -> Task<Message> {
        // What was typed, asked: the Ask window opened on its answer.
        // Asked first, the window opened after: opened first, on an empty
        // conversation, it asked the model for questions to suggest — a
        // call paid for, its answer never shown.
        if let Some(question) = asked_in(title).map(str::to_string) {
            self.proj.finder.open = false;
            let asked = self.ask_typed(question);
            let open = self.update(Message::Ask(AskMsg::Open));
            return Task::batch([asked, open]);
        }
        // What was typed, put in Ask's field to be asked once written: after
        // a draft there, which it keeps.
        if let Some(typed) = drafted_in(title).map(str::to_string) {
            self.proj.finder.open = false;
            let draft = self.proj.ask_input.trim_end();
            self.proj.ask_input = if draft.is_empty() {
                typed
            } else {
                format!("{draft}\n\n{typed}")
            };
            self.proj.ask_spoken = false;
            return self.update(Message::Ask(AskMsg::Open));
        }
        if self.palette_commands().iter().any(|c| c.title == title) {
            return self.run_palette(title);
        }
        let Some(message) = self
            .palette_screen
            .iter()
            .find(|t| t.typing.is_none() && t.title == title)
            .map(|t| t.message.clone())
        else {
            return Task::none();
        };
        self.proj.finder.open = false;
        self.code_focused = true;
        self.update(message)
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
                let served = self.served(pane);
                self.proj.context_menu = Some(ContextMenu {
                    pane,
                    line,
                    col,
                    x: 0.0,
                    y: 0.0,
                    insight,
                    served,
                    by_pointer: false,
                    line_top: None,
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
    /// them with nothing typed. Their titles are in English (each marked for
    /// the catalogs where it is written); the rows show them translated.
    pub(crate) fn palette_commands(&self) -> Vec<PaletteCommand> {
        let mut out = Vec::new();
        let project = self.proj.project.is_some();
        let mut add = |title: String, keys: Option<String>, action: PaletteAction| {
            out.push(PaletteCommand {
                title,
                keys,
                action,
                shown_as: None,
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
                    | keymap::Action::Tutorial
            );
            if !(project || global) {
                continue;
            }
            add(
                action.menu().title.to_string(),
                Some(self.keymap.shown(action)),
                send(Message::Editor(EditorMsg::RunAction(action))),
            );
        }

        // At the caret: what the context menu and the reading keys do there.
        if project && self.caret().is_some() {
            for (kind, keys) in [
                (GotoKind::Definition, "F12".to_string()),
                (GotoKind::References, crate::keymap::local_keys("⇧F12")),
                (GotoKind::Implementation, "gi".to_string()),
                (GotoKind::TypeDefinition, "gy".to_string()),
            ] {
                add(
                    kind.name().to_string(),
                    Some(keys),
                    PaletteAction::Goto(kind),
                );
            }
            if let Some(kind) = self
                .active_viewer()
                .and_then(|v| v.lang_key)
                .and_then(InsightKind::for_lang)
            {
                add(
                    kind.name().to_string(),
                    Some("ge".into()),
                    PaletteAction::Insight,
                );
            }
            let lang = self.active_viewer().and_then(|v| v.lang_key);
            for &kind in lang.map_or(&[][..], InsightKind::more_for_lang) {
                add(
                    kind.name().to_string(),
                    None,
                    PaletteAction::AtCaret(Message::Hover(HoverMsg::InsightOf(kind))),
                );
            }
            if lang == Some("lean") {
                add(
                    tr_noop!("Step Through Proof").to_string(),
                    None,
                    PaletteAction::AtCaret(Message::Proof(crate::ProofMsg::FromMenu)),
                );
            }
            let at_caret = |m| PaletteAction::AtCaret(m);
            for (title, keys, message) in [
                (
                    tr_noop!("Call Hierarchy"),
                    Some("gc"),
                    Message::Calls(CallsMsg::FromMenu),
                ),
                (
                    tr_noop!("View Docs"),
                    None,
                    Message::Docs(DocsMsg::ViewFromMenu),
                ),
                (
                    tr_noop!("Explain"),
                    None,
                    Message::Explain(ExplainMsg::FromMenu),
                ),
                (
                    tr_noop!("Add to Ask"),
                    None,
                    Message::Ask(AskMsg::AboutSelection),
                ),
                (
                    tr_noop!("Why Is This Here?"),
                    None,
                    Message::TimeTravel(TimeTravelMsg::WhyIsThisHere),
                ),
                (
                    tr_noop!("Toggle Breakpoint"),
                    None,
                    Message::Debug(DebugMsg::ToggleBreakpointFromMenu),
                ),
                (
                    tr_noop!("Conditional Breakpoint…"),
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

        // The debugger, while the run whose tab is shown goes on.
        if let Some(session) = self
            .debug
            .session
            .as_ref()
            .filter(|s| s.status != DebugStatus::Terminated && self.real_run_shown())
        {
            if session.status == DebugStatus::Stopped {
                for (title, cmd) in [
                    (tr_noop!("Debug: Continue"), DebugCmd::Continue),
                    (tr_noop!("Debug: Step Over"), DebugCmd::StepOver),
                    (tr_noop!("Debug: Step Into"), DebugCmd::StepIn),
                    (tr_noop!("Debug: Step Out"), DebugCmd::StepOut),
                ] {
                    add(
                        title.into(),
                        None,
                        send(Message::Debug(DebugMsg::Control(cmd))),
                    );
                }
            }
            add(
                tr_noop!("Debug: Stop").into(),
                None,
                send(Message::Debug(DebugMsg::Stop)),
            );
            add(
                if self.debug_hover_eval {
                    tr_noop!("Debug: Turn Off Hover Evaluation")
                } else {
                    tr_noop!("Debug: Turn On Hover Evaluation")
                }
                .into(),
                None,
                send(Message::Debug(DebugMsg::ToggleHoverEval)),
            );
        }

        // The find bar, while it is open.
        if project && self.proj.find.open {
            for (title, keys, message) in [
                (
                    tr_noop!("Find Next"),
                    Some(crate::keymap::local_keys("↩")),
                    EditorMsg::FindStep(1),
                ),
                (
                    tr_noop!("Find Previous"),
                    Some(crate::keymap::local_keys("⇧↩")),
                    EditorMsg::FindStep(-1),
                ),
                (tr_noop!("Find: Close"), None, EditorMsg::FindClosed),
            ] {
                add(title.into(), keys, send(Message::Editor(message)));
            }
        }

        // Moving through the open file, as its keys do: by a screen, to
        // either end, folding — for voice above all, which has no keys.
        if project && self.active_viewer().is_some() {
            for (title, keys, message) in [
                (tr_noop!("Scroll Down"), None, EditorMsg::ScrollPages(1)),
                (tr_noop!("Scroll Up"), None, EditorMsg::ScrollPages(-1)),
                (
                    tr_noop!("Go to Top of File"),
                    Some("gg"),
                    EditorMsg::MoveCaret(viewer::Motion::FileStart),
                ),
                (
                    tr_noop!("Go to End of File"),
                    Some("G"),
                    EditorMsg::MoveCaret(viewer::Motion::LastLine),
                ),
                (tr_noop!("Fold All"), Some("zM"), EditorMsg::FoldAll(true)),
                (
                    tr_noop!("Unfold All"),
                    Some("zR"),
                    EditorMsg::FoldAll(false),
                ),
                (
                    tr_noop!("Toggle Fold Here"),
                    Some("za"),
                    EditorMsg::FoldHere,
                ),
            ] {
                add(
                    title.into(),
                    keys.map(String::from),
                    send(Message::Editor(message)),
                );
            }
            for (title, tab) in [
                (tr_noop!("Margin: Explanations"), MarginTab::Notes),
                (tr_noop!("Margin: Calls"), MarginTab::Calls),
                (tr_noop!("Margin: History"), MarginTab::History),
            ] {
                add(
                    title.into(),
                    None,
                    send(Message::Window(WindowMsg::MarginTabPicked(tab))),
                );
            }
        }
        // Each page of Settings, by its name.
        for (title, page) in [
            (tr_noop!("Settings: Appearance"), SettingsPage::Appearance),
            (tr_noop!("Settings: Usage and cost"), SettingsPage::Usage),
            (tr_noop!("Settings: Updates"), SettingsPage::Updates),
            (
                tr_noop!("Settings: Language Model"),
                SettingsPage::LanguageModel,
            ),
            (tr_noop!("Settings: Embeddings"), SettingsPage::Embeddings),
            (tr_noop!("Settings: Voice"), SettingsPage::Voice),
            (
                tr_noop!("Settings: Integrations"),
                SettingsPage::Integrations,
            ),
            (
                tr_noop!("Settings: Language Servers"),
                SettingsPage::Languages,
            ),
            (
                tr_noop!("Settings: Keyboard Shortcuts"),
                SettingsPage::Shortcuts,
            ),
        ] {
            add(
                title.into(),
                None,
                send(Message::Settings(SettingsMsg::OpenAt(page))),
            );
        }
        // The window itself.
        add(
            tr_noop!("Window: Minimize").into(),
            None,
            send(Message::Window(WindowMsg::Minimize)),
        );
        add(
            tr_noop!("Window: Toggle Maximize").into(),
            None,
            send(Message::Window(WindowMsg::ToggleMaximize)),
        );
        add(
            tr_noop!("Window: Full Screen").into(),
            None,
            send(Message::Window(WindowMsg::ToggleFullscreen)),
        );

        // The toolbar's own tools (Overview and Stats are keymap actions,
        // listed with the rest above).
        add(
            tr_noop!("Settings").into(),
            Some(crate::keymap::local_keys("⌘,")),
            send(Message::Settings(SettingsMsg::Open)),
        );

        // The sidebar's tabs and the panels.
        if project {
            for (_, tab) in ui::SIDEBAR_TABS {
                add(
                    sidebar_title(tab).into(),
                    None,
                    send(Message::Window(WindowMsg::SidebarTabPicked(tab))),
                );
            }
            // Each title whole, as it reads: "View: Hide Sidebar" while the
            // sidebar shows, "View: Show Sidebar" while it does not.
            let toggle =
                |shown: bool, hide: &str, show: &str| if shown { hide } else { show }.to_string();
            add(
                toggle(
                    self.show_left_sidebar,
                    tr_noop!("View: Hide Sidebar"),
                    tr_noop!("View: Show Sidebar"),
                ),
                None,
                send(Message::Window(WindowMsg::ToggleLeftSidebar)),
            );
            add(
                toggle(
                    self.show_right_panel,
                    tr_noop!("View: Hide Margin"),
                    tr_noop!("View: Show Margin"),
                ),
                None,
                send(Message::Window(WindowMsg::ToggleRightPanel)),
            );
            add(
                toggle(
                    self.show_bottom,
                    tr_noop!("View: Hide Debug Panel"),
                    tr_noop!("View: Show Debug Panel"),
                ),
                None,
                send(Message::Window(if self.show_bottom {
                    WindowMsg::CollapseBottom
                } else {
                    WindowMsg::ShowDebugPanel
                })),
            );
            // The ⋯ menu's view toggles.
            for (shown, hide, show, message) in [
                (
                    self.show_inline_summaries,
                    tr_noop!("View: Hide Outline Summaries"),
                    tr_noop!("View: Show Outline Summaries"),
                    Message::Window(WindowMsg::ToggleInlineSummaries),
                ),
                (
                    self.show_file_banner,
                    tr_noop!("View: Hide File Summary"),
                    tr_noop!("View: Show File Summary"),
                    Message::Window(WindowMsg::ToggleFileBanner),
                ),
                (
                    self.show_inlay_hints,
                    tr_noop!("View: Hide Inlay Hints"),
                    tr_noop!("View: Show Inlay Hints"),
                    Message::Lsp(LspMsg::ToggleInlayHints),
                ),
                (
                    self.show_minimap,
                    tr_noop!("View: Hide Minimap"),
                    tr_noop!("View: Show Minimap"),
                    Message::Window(WindowMsg::ToggleMinimap),
                ),
                (
                    self.show_inline_blame,
                    tr_noop!("View: Hide Inline Blame"),
                    tr_noop!("View: Show Inline Blame"),
                    Message::Window(WindowMsg::ToggleInlineBlame),
                ),
                (
                    self.show_indent_guides,
                    tr_noop!("View: Hide Indent Guides"),
                    tr_noop!("View: Show Indent Guides"),
                    Message::Window(WindowMsg::ToggleIndentGuides),
                ),
            ] {
                add(toggle(shown, hide, show), None, send(message));
            }
            // The tools that have no chord of their own.
            for (title, message) in [
                (
                    tr_noop!("Skim (Fold Bodies)"),
                    Message::Editor(EditorMsg::SkimFile),
                ),
                (tr_noop!("Export Notes…"), Message::Export(ExportMsg::Start)),
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
                    tr_noop!("Explain: Summary")
                } else {
                    tr_noop!("Explain: In Blocks")
                };
                add(
                    title.into(),
                    None,
                    send(Message::Explain(ExplainMsg::ToggleBlocks)),
                );
            }
            // Where the conversation is, its sessions, and clearing it.
            add(
                tr_noop!("Ask: New Session").into(),
                None,
                send(Message::Ask(AskMsg::NewSession)),
            );
            if self.ask_view.is_open() {
                add(
                    tr_noop!("Ask: Hide").into(),
                    None,
                    send(Message::Ask(AskMsg::Hide)),
                );
            }
            if self.ask_view != AskView::Expanded {
                add(
                    tr_noop!("Ask: Expand").into(),
                    None,
                    send(Message::Ask(AskMsg::Expand)),
                );
            }
            if !self.proj.ask_turns.is_empty() {
                add(
                    tr_noop!("Ask: Clear Conversation").into(),
                    None,
                    send(Message::Ask(AskMsg::Clear)),
                );
            }
        }
        add(
            tr_noop!("Open Folder…").into(),
            None,
            send(Message::Project(ProjectMsg::OpenFolderPressed)),
        );
        add(
            tr_noop!("Open Remote…").into(),
            None,
            send(Message::Connect(ConnectMsg::Open)),
        );
        // The file being read's commits, in the History view.
        if let Some(rel) = self.active_viewer().map(|v| v.rel.clone()) {
            add(
                tr_noop!("History of This File").into(),
                None,
                send(Message::History(crate::HistoryMsg::Show {
                    path: Some(rel),
                })),
            );
        }

        // Every setting's every choice: the language, the theme, how voice
        // listens and speaks ("Language: French", "Read answers aloud: Never").
        for setting in self.settings_choices() {
            for choice in setting.choices {
                out.push(PaletteCommand {
                    title: choice.title,
                    keys: None,
                    action: PaletteAction::Send(choice.message),
                    shown_as: Some(choice.shown),
                });
            }
        }
        // The folders opened before, by their names ("Open Recent:
        // teashop"), as the app menu lists them; and the way back to the
        // welcome screen.
        for (name, place, message) in crate::ui::app_menu_recent(self) {
            out.push(PaletteCommand {
                title: format!("Open Recent: {name} ({place})"),
                keys: None,
                action: PaletteAction::Send(message),
                shown_as: Some(tr!("Open Recent: {name}", name = name)),
            });
        }
        if self.proj.project.is_some() {
            out.push(PaletteCommand {
                title: tr_noop!("Close Folder").into(),
                keys: None,
                action: PaletteAction::Send(Message::Project(ProjectMsg::CloseFolder)),
                shown_as: None,
            });
        }
        out
    }
}

/// The palette's title for showing sidebar view `tab`, in English (see
/// [`PaletteCommand::title`]).
fn sidebar_title(tab: SidebarTab) -> &'static str {
    match tab {
        SidebarTab::Files => tr_noop!("Sidebar: Files"),
        SidebarTab::Search => tr_noop!("Sidebar: Search Text"),
        SidebarTab::Semantic => tr_noop!("Sidebar: Find by Meaning"),
        SidebarTab::Marks => tr_noop!("Sidebar: Bookmarks"),
        SidebarTab::Trail => tr_noop!("Sidebar: Reading Trail"),
        SidebarTab::Calls => tr_noop!("Sidebar: Call Hierarchy"),
        SidebarTab::Flow => tr_noop!("Sidebar: Value Flow"),
        SidebarTab::Imports => tr_noop!("Sidebar: Imports"),
        SidebarTab::Walk => tr_noop!("Sidebar: Walkthroughs"),
        SidebarTab::Notes => tr_noop!("Sidebar: Your Notes"),
        SidebarTab::Docs => tr_noop!("Sidebar: API Docs"),
        SidebarTab::History => tr_noop!("Sidebar: History"),
    }
}
