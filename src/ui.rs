//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, Row, center, column, container, mouse_area, opaque, pick_list, row, scrollable, slider,
    space, stack, text, text_input, tooltip,
};
use iced::{Element, Fill, Length, Padding, Task};

use crate::app::model::Question;
use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::glyph::{self, Glyph, WindowGlyph};
use crate::viewer::Viewer;
use crate::{App, Message, SidebarTab, TimeScope, TimeTravel, theme};
use crate::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, DocsMsg, EditorMsg, ExplainMsg, ExportMsg,
    GlossaryMsg, GraphMsg, HistoryMsg, HoverMsg, LspMsg, NavMsg, OverviewMsg, ProjectMsg,
    ReadingMsg, SemanticMsg, SettingsMsg, TimeTravelMsg, TutorialMsg, UpdaterMsg, WalkMsg,
    WindowMsg,
};
use crate::{tr, tr_noop, trn};
mod anchored;
// A line of text on the design's baseline wherever it lands (`ts::set_in`).
mod baseline;
mod clipped;
mod corner_safe;
mod escape_clears;
mod ring;
mod tree_keys;
pub(crate) use anchored::*;
mod chrome;
pub(crate) use chrome::*;
mod ops;
pub(crate) use ops::*;
pub(crate) use ring::RingAsk;
mod dialogs;
pub(crate) use dialogs::*;
mod fakerun;
pub(crate) mod jump;
mod press;
pub(crate) use press::{Press, button};
mod proof;
mod weakspots;
pub(crate) use fakerun::*;
pub(crate) use weakspots::reason_words;
mod slide;
mod tutorial;
pub(crate) use tutorial::*;
mod statusbar;
// Text that changes while it is watched, in a slot that does not.
pub(crate) mod steady;
// Tooltips that wait for the pointer to rest.
pub(crate) mod tip;
pub(crate) use statusbar::*;
mod panes;
pub(crate) use panes::*;
mod fresh;
// A height held while what is under it is asked about.
mod held;
pub(crate) mod placed;
mod reader_scroll;
pub(crate) use reader_scroll::*;
mod docs_stats;
pub(crate) use docs_stats::*;
mod panels;
pub(crate) use panels::*;
mod sidebar;
pub(crate) use sidebar::*;
pub(crate) use sidebar::{TREE_ROW_GAP, TREE_ROW_H, tree_row_index};
mod toolbar;
pub(crate) use toolbar::*;
mod sheet;
pub(crate) use sheet::*;
mod explain_view;
pub(crate) use explain_view::*;
mod connect;
pub(crate) use connect::*;
mod graph;
pub(crate) use graph::*;
mod graph_labels;
mod history;
mod memo;
pub use memo::ViewMemo;
pub(crate) use memo::{CallsSummary, DocsGroup, DocsKey, ImportRanks, RankedFile};
mod notebook;
pub(crate) use notebook::*;
mod overlays;
pub(crate) use overlays::*;
mod updater;
mod usage;
pub(crate) use updater::*;
mod askfloat;
mod form;
pub(crate) mod heard;
mod meter;
mod setup;
pub(crate) use askfloat::*;
mod voice;
pub(crate) use voice::*;
#[cfg(test)]
mod tests;

/// Text sizes by typographic role: the `theme::ui_size` table under short
/// names, so a view reads `.size(ts::SMALL)` and the scale lives in one place.
pub(crate) mod ts {
    use crate::theme::{TextRole, ui_size};

    pub(crate) const CAPTION: f32 = ui_size(TextRole::Caption);
    pub(crate) const SMALL: f32 = ui_size(TextRole::Small);
    pub(crate) const BODY: f32 = ui_size(TextRole::Body);
    pub(crate) const BASE: f32 = ui_size(TextRole::Base);
    pub(crate) const READING: f32 = ui_size(TextRole::Reading);
    pub(crate) const EMPHASIS: f32 = ui_size(TextRole::Emphasis);
    pub(crate) const SUBTITLE: f32 = ui_size(TextRole::Subtitle);
    pub(crate) const TITLE: f32 = ui_size(TextRole::Title);
    pub(crate) const HEADING: f32 = ui_size(TextRole::Heading);
    pub(crate) const DISPLAY: f32 = ui_size(TextRole::Display);
    pub(crate) const HERO: f32 = ui_size(TextRole::Hero);
    /// A face's ascent and descent, in em.
    #[derive(Clone, Copy)]
    pub(crate) struct Face {
        ascent: f32,
        descent: f32,
    }

    /// Inter, the interface's face (1984 and 494 of 2048 units).
    pub(crate) const INTER: Face = Face {
        ascent: 0.968_75,
        descent: 0.241_211,
    };
    /// JetBrains Mono, the code's (1020 and 300 of 1000).
    pub(crate) const MONO: Face = Face {
        ascent: 1.02,
        descent: 0.3,
    };

    impl Face {
        /// Its ascent and descent at `size`, each rounded to a whole
        /// pixel, as the design's renderer takes them (at every scale).
        pub(crate) fn rounded(self, size: f32) -> (f32, f32) {
            ((self.ascent * size).round(), (self.descent * size).round())
        }

        /// Its `line-height: normal` at `size`: the two rounded together.
        pub(crate) fn normal(self, size: f32) -> f32 {
            let (ascent, descent) = self.rounded(size);
            ascent + descent
        }
    }

    /// The design's `line-height: normal` for Inter at `size`: the face's
    /// rounded ascent and descent (see [`Face`]). For text stacked in a box
    /// its lines size: iced's default of 1.3 sets each such line a pixel
    /// or so off the design.
    pub(crate) fn normal_line(size: f32) -> iced::widget::text::LineHeight {
        normal_of(INTER, size)
    }

    /// [`normal_line`] for JetBrains Mono.
    pub(crate) fn normal_line_code(size: f32) -> iced::widget::text::LineHeight {
        normal_of(MONO, size)
    }

    fn normal_of(face: Face, size: f32) -> iced::widget::text::LineHeight {
        iced::widget::text::LineHeight::Absolute(face.normal(size).into())
    }

    /// Where to draw a line of `face` at `size`, laid out
    /// [`Face::normal`] tall (its baseline then its rounded ascent down, as
    /// the vendored text engine sets a line), for its text to sit where
    /// the design's renderer paints it in a line `line` tall from `top`: a
    /// `normal` box centred in the line, its baseline that box's rounded
    /// ascent down, then snapped to a whole pixel.
    pub(crate) fn baseline_top(top: f32, line: f32, face: Face, size: f32) -> f32 {
        let (ascent, descent) = face.rounded(size);
        (top + (line - (ascent + descent)) / 2.0 + ascent).round() - ascent
    }

    /// Where to draw a line of `face` at `size`, laid out
    /// [`Face::normal`] tall, whose own line box is `own` tall (the
    /// design's `line-height`) and centred in a box `height` tall from
    /// `top`: the design's renderer puts that line box on a whole pixel,
    /// and the text in it a whole number of pixels down, the leading over
    /// it floored. With a `normal` line box, as [`baseline_top`].
    pub(crate) fn line_top(top: f32, height: f32, own: f32, face: Face, size: f32) -> f32 {
        (top + (height - own) / 2.0).round() + ((own - face.normal(size)) / 2.0).floor()
    }

    /// `content`, a line of `face` at `size`, set in a box `height` tall as
    /// the design centres a line in one (a button, a row of the chrome, a
    /// menu's): its baseline where the design's renderer paints it, by
    /// [`baseline_top`], wherever the box lands (`ui::baseline`). Left to
    /// centre itself, a text widget lands up to a pixel higher.
    pub(crate) fn set_in<'a, M: 'a>(
        content: iced::widget::Text<'a>,
        height: f32,
        face: Face,
        size: f32,
    ) -> iced::Element<'a, M> {
        set_in_line(content, height, face.normal(size), face, size)
    }

    /// [`set_in`], for a line the design gives a `line-height` of its own:
    /// its line box `own` tall ([`line_top`]).
    pub(crate) fn set_in_line<'a, M: 'a>(
        content: iced::widget::Text<'a>,
        height: f32,
        own: f32,
        face: Face,
        size: f32,
    ) -> iced::Element<'a, M> {
        let line = iced::widget::text::LineHeight::Absolute(face.normal(size).into());
        let laid = iced::widget::container(content.size(size).line_height(line))
            .height(iced::Length::Fixed(height))
            .padding(iced::Padding {
                top: line_top(0.0, height, own, face, size),
                ..iced::Padding::ZERO
            });
        super::baseline::Snapped::new(laid.into(), height, own, face, size).into()
    }

    /// The room a browser leaves either side of what is typed into an
    /// input — its own padding, which the design's inputs keep.
    pub(crate) const FIELD_INSET: f32 = 2.0;

    /// `field`, one line of the interface's face at `size` (`face`'s
    /// metrics), set in a box `height` tall as the design's renderer sets an
    /// input in one: what is typed [`FIELD_INSET`] in from either side, in
    /// the face's own spacing (`fonts::in_control`), its baseline as
    /// [`set_in`]'s.
    pub(crate) fn field_in<'a, M: Clone + 'a>(
        field: iced::widget::TextInput<'a, M>,
        height: f32,
        face: Face,
        size: f32,
    ) -> iced::Element<'a, M> {
        let line = iced::widget::text::LineHeight::Absolute(face.normal(size).into());
        let laid = iced::widget::container(
            field
                .font(crate::fonts::in_control(crate::fonts::UI))
                .size(size)
                .line_height(line)
                .padding(0),
        )
        .height(iced::Length::Fixed(height))
        .padding(iced::Padding {
            top: baseline_top(0.0, height, face, size),
            right: FIELD_INSET,
            bottom: 0.0,
            left: FIELD_INSET,
        });
        super::baseline::Snapped::new(laid.into(), height, face.normal(size), face, size).into()
    }
}

pub fn code_scroll_id(pane: usize) -> iced::widget::Id {
    iced::widget::Id::new(if pane == 0 {
        "code-view-0"
    } else {
        "code-view-1"
    })
}

pub fn finder_input_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-input")
}

/// The Files view's tree, scrolled to the file being read.
pub fn files_tree_id() -> iced::widget::Id {
    iced::widget::Id::new("files-tree")
}

/// The finder's result list, so the selection can be scrolled into view.
pub fn finder_list_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-list")
}

/// The map's search field (see `graph::graph_search_field`).
pub fn graph_search_id() -> iced::widget::Id {
    iced::widget::Id::new("graph-search")
}

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

pub fn find_input_id() -> iced::widget::Id {
    iced::widget::Id::new("find-input")
}

/// The Language model page's API key field: where `SettingsMsg::NeedKey`
/// puts the keyboard.
pub fn settings_key_id() -> iced::widget::Id {
    iced::widget::Id::new("settings-key")
}

pub fn ask_input_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-input")
}

/// The Files view's filter field.
/// The Files view's tree as one stop for the keyboard (`tree_keys`).
pub fn files_tree_keys_id() -> iced::widget::Id {
    iced::widget::Id::new("files-tree-keys")
}

pub fn file_filter_id() -> iced::widget::Id {
    iced::widget::Id::new("file-filter")
}

/// The search field of Ask's menu of sessions, focused as the menu opens.
pub fn ask_sessions_search_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-sessions-search")
}

/// The Ask conversation scrollable, so a new answer can snap it to the bottom.
pub fn ask_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-conversation")
}

/// The Reading trail's list, so it opens on the place being read.
pub fn trail_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("trail-list")
}

/// The outline scrollable, so it can follow the caret's current symbol.
pub fn outline_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("outline-list")
}

/// The Settings page's scroll: each page opens at its top.
pub fn settings_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("settings-page")
}

pub fn bp_condition_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bp-condition-input")
}

pub fn note_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bookmark-note-input")
}

/// The panel of whichever modal is open (every modal is built by
/// [`modal`]), so Tab can keep focus inside it.
pub fn modal_scope_id() -> iced::widget::Id {
    iced::widget::Id::new("modal-scope")
}

/// The one overlay drawn over the app. [`active_overlay`] picks it — the first
/// variant that applies wins, so the order is the stacking priority — and both
/// [`view`] (to draw it) and [`escape_message`] (to dismiss it) go through that
/// choice, so Escape always closes exactly what is on top.
pub(crate) enum ActiveOverlay<'a> {
    Consent(&'a std::path::Path),
    /// The question on screen, its number, and how many more wait behind it.
    LspCommand(&'a crate::PendingLspCommand, u64, usize),
    /// The question on screen, its number, and how many more wait behind it.
    LspConsent(&'a crate::LspConsent, u64, usize),
    /// "Set up project…", or Explain All asking first (a first open's
    /// sheet is `Consent`).
    Setup(&'a crate::app::setup::ProjectSetup),
    UpdateNotes,
    Settings,
    Connect,
    /// The sheet before the tour (under Settings, which its steps open).
    TourPrep(&'a crate::app::tutorial::TourPrep),
    BpCondition(&'a (std::path::PathBuf, usize, String)),
    /// Fake run's inputs sheet.
    FakeRun(&'a crate::app::fakerun::Sheet),
    BookmarkNote(&'a (String, usize, String)),
    ReadingNote(&'a (String, String, String)),
    Why(&'a crate::BlameWhy),
    Insight(&'a crate::Insight),
    ToolsMenu,
    /// The strip's Run menu (Run, Debug, Fake Run, Fake Debug).
    RunMenu,
    TargetMenu,
    AppMenu,
    ViewMenu,
    /// The sidebar's More views menu.
    ViewsMenu,
    Graph(crate::Overlay),
    Finder,
    ContextMenu(&'a crate::ContextMenu),
    Hover(&'a crate::HoverState),
}

/// The overlay on top right now, if any (see [`ActiveOverlay`]).
pub(crate) fn active_overlay(app: &App) -> Option<ActiveOverlay<'_>> {
    use ActiveOverlay as O;
    // Settings opened from the project's sheet — its "Add an API Key…" —
    // comes up over it, and closes back to it: under the sheet, the click
    // seemed to do nothing.
    let sheet_up = !app.settings.open;
    let overlay = if let Some(root) = app.pending_consent.as_ref().filter(|_| sheet_up) {
        O::Consent(root)
    } else if let Some((number, pending)) = app.proj.pending_lsp_command.on_screen() {
        O::LspCommand(
            pending,
            number,
            app.proj.pending_lsp_command.len() - 1 + app.proj.pending_lsp_consent.len(),
        )
    } else if let Some((number, consent)) = app.proj.pending_lsp_consent.on_screen() {
        O::LspConsent(consent, number, app.proj.pending_lsp_consent.len() - 1)
    } else if let Some(setup) = app
        .setup
        .as_ref()
        .filter(|s| sheet_up && s.mode != crate::app::setup::SetupMode::Open)
    {
        O::Setup(setup)
    } else if app.update.show_notes {
        O::UpdateNotes
    } else if app.settings.open {
        O::Settings
    } else if app.connect.is_some() {
        O::Connect
    } else if let Some(prep) = &app.tour_prep {
        O::TourPrep(prep)
    } else if let Some(edit) = &app.proj.bp_cond_edit {
        O::BpCondition(edit)
    } else if let Some(sheet) = &app.proj.fake.sheet {
        O::FakeRun(sheet)
    } else if let Some(edit) = &app.proj.note_edit {
        O::BookmarkNote(edit)
    } else if let Some(edit) = &app.proj.reading_note_edit {
        O::ReadingNote(edit)
    } else if let Some(bw) = &app.proj.blame_why {
        O::Why(bw)
    } else if let Some(insight) = &app.proj.insight {
        O::Insight(insight)
    } else if app.show_tools_menu && !tools_on_strip(app) {
        O::ToolsMenu
    } else if app.show_run_menu && !runs_on_strip(app) {
        O::RunMenu
    } else if app.show_target_menu {
        O::TargetMenu
    } else if app.show_app_menu {
        O::AppMenu
    } else if app.show_view_menu {
        O::ViewMenu
    } else if app.show_views_menu {
        O::ViewsMenu
    } else if let Some(overlay) = app.proj.overlay {
        O::Graph(overlay)
    } else if app.proj.finder.open {
        O::Finder
    } else if let Some(menu) = &app.proj.context_menu {
        O::ContextMenu(menu)
    } else if let Some(hover) = app
        .proj
        .hover
        .as_ref()
        .filter(|h| h.text.is_some() || h.summary.is_some() || h.diagnostic.is_some())
    {
        O::Hover(hover)
    } else {
        return None;
    };
    Some(overlay)
}

impl<'a> ActiveOverlay<'a> {
    /// Which overlay it is, when it holds the keys yet takes none to a
    /// field of its own: every one but the hover peek (no keys), the
    /// finder and the note and condition editors (each focuses its input).
    /// Opened, it leaves the keyboard where it was — in a field under it
    /// (Ask, Search, Find), whose Enter would act behind it — unless that
    /// is taken away (`App::update`).
    pub(crate) fn leaves_focus(&self) -> Option<&'static str> {
        Some(match self {
            Self::Consent(_) => "consent",
            Self::LspCommand(..) => "lsp-command",
            Self::LspConsent(..) => "lsp-consent",
            Self::Setup(_) => "setup",
            Self::UpdateNotes => "update-notes",
            Self::Settings => "settings",
            Self::Connect => "connect",
            Self::TourPrep(_) => "tour-prep",
            Self::Why(_) => "why",
            Self::Insight(_) => "insight",
            Self::ToolsMenu => "tools-menu",
            Self::RunMenu => "run-menu",
            Self::TargetMenu => "target-menu",
            Self::AppMenu => "app-menu",
            Self::ViewMenu => "view-menu",
            Self::ViewsMenu => "views-menu",
            Self::Graph(_) => "graph",
            Self::ContextMenu(_) => "context-menu",
            Self::BpCondition(_)
            | Self::FakeRun(_)
            | Self::BookmarkNote(_)
            | Self::ReadingNote(_)
            | Self::Finder
            | Self::Hover(_) => return None,
        })
    }

    /// Which dialog it is, told apart from another in its place — the next
    /// of several questions asked in turn too: the keyboard's ring goes to
    /// the first stop of one come up in another's place.
    pub(crate) fn scene(&self) -> String {
        match self {
            Self::LspCommand(_, number, _) => format!("lsp-command#{number}"),
            Self::LspConsent(_, number, _) => format!("lsp-consent#{number}"),
            Self::Graph(overlay) => format!("graph:{overlay:?}"),
            Self::BpCondition(_) => "bp-condition".into(),
            Self::FakeRun(_) => "fake-run".into(),
            Self::BookmarkNote(_) => "bookmark-note".into(),
            Self::ReadingNote(_) => "reading-note".into(),
            Self::Finder => "finder".into(),
            Self::Hover(_) => "hover".into(),
            other => other.leaves_focus().unwrap_or("overlay").into(),
        }
    }

    /// Whether this overlay holds the keys of what is under it: every one
    /// but the hover peek, which asks for none (the Esc every overlay takes
    /// aside, `escape_message`). A time-travel session under one takes none
    /// of its chords (`App::handle_key`).
    pub(crate) fn holds_keys(&self) -> bool {
        !matches!(self, Self::Hover(_))
    }

    fn view(self, app: &'a App) -> Element<'a, Message> {
        match self {
            // Each Allow is drawn enabled once its question has been within
            // reach long enough to be read, where it is (`App::asked`).
            Self::Consent(root) => setup::setup_sheet(
                app,
                root,
                app.setup
                    .as_ref()
                    .filter(|s| s.root == root && s.mode == crate::app::setup::SetupMode::Open),
                app.asked(Question::Project(root.to_path_buf())),
            ),
            Self::Setup(s) => setup::setup_sheet(app, &s.root, Some(s), true),
            Self::LspCommand(pending, number, waiting) => lsp_command_modal(
                pending,
                number,
                waiting,
                app.asked(Question::LspCommand(number)),
            ),
            Self::LspConsent(consent, number, waiting) => lsp_consent_modal(
                consent,
                number,
                waiting,
                app.asked(Question::LspConsent(number)),
            ),
            Self::UpdateNotes => update_notes_modal(app),
            Self::Settings => settings_modal(app),
            Self::Connect => connect_modal(app),
            Self::TourPrep(prep) => tutorial::tour_prep_sheet(app, prep),
            Self::BpCondition(edit) => bp_condition_modal(app, edit),
            Self::FakeRun(sheet) => fake_sheet(sheet),
            Self::BookmarkNote(edit) => bookmark_note_modal(edit),
            Self::ReadingNote(edit) => reading_note_modal(edit),
            Self::Why(bw) => why_modal(app, bw),
            Self::Insight(insight) => insight_modal(app, insight),
            Self::ToolsMenu => tools_menu(app),
            Self::RunMenu => toolbar::run_menu(app),
            Self::TargetMenu => target_menu(app),
            Self::AppMenu => app_menu(app),
            Self::ViewMenu => view_menu(app),
            Self::ViewsMenu => toolbar::views_menu(app),
            Self::Graph(overlay) => project_graph_modal(app, overlay),
            Self::Finder => finder_modal(app),
            Self::ContextMenu(menu) => context_menu(app, menu),
            Self::Hover(hover) => hover_tooltip(hover),
        }
    }

    /// What the overlay is called where it is opened, in English — as it is
    /// put away by voice: "close the call graph", "hide Settings". `None`
    /// for what goes by no name of its own (a menu, a prompt).
    pub(crate) fn name(&self) -> Option<&'static str> {
        match self {
            Self::Graph(overlay) => Some(graph::graph_title(*overlay)),
            Self::Settings => Some(tr_noop!("Settings")),
            Self::FakeRun(sheet) => Some(if sheet.debug {
                tr_noop!("Fake debug")
            } else {
                tr_noop!("Fake run")
            }),
            _ => None,
        }
    }

    /// The message that dismisses this overlay — the same one its Cancel /
    /// Close control or a backdrop click sends. For the consent prompts that
    /// is the decline, as Escape means cancel: a first open's × (the project
    /// not opened — its Not now opens it, set up with nothing), a command's
    /// "Don't run".
    pub(crate) fn dismiss_message(&self) -> Message {
        match self {
            Self::Consent(root) => Message::Project(ProjectMsg::ConsentDenied(root.to_path_buf())),
            Self::LspCommand(_, number, _) => Message::Lsp(LspMsg::CommandDismissed(*number)),
            Self::LspConsent(_, number, _) => Message::Lsp(LspMsg::ConsentDismissed(*number)),
            Self::Setup(_) => Message::Project(ProjectMsg::SetupClose),
            Self::UpdateNotes => Message::Updater(UpdaterMsg::CloseNotes),
            Self::Settings => Message::Settings(SettingsMsg::Close),
            Self::Connect => Message::Connect(ConnectMsg::Close),
            Self::TourPrep(_) => Message::Tutorial(TutorialMsg::PrepClose),
            Self::BpCondition(_) => Message::Debug(DebugMsg::BpConditionCancel),
            Self::FakeRun(_) => Message::Fake(crate::FakeMsg::CloseSheet),
            Self::BookmarkNote(_) => Message::Reading(ReadingMsg::BookmarkNoteCancel),
            Self::ReadingNote(_) => Message::Reading(ReadingMsg::NoteEditCancel),
            Self::Why(_) => Message::TimeTravel(TimeTravelMsg::BlameWhyClose),
            Self::Insight(_) => Message::Hover(HoverMsg::InsightClose),
            Self::ToolsMenu => Message::Window(WindowMsg::ToggleToolsMenu),
            Self::RunMenu => Message::Window(WindowMsg::ToggleRunMenu),
            Self::TargetMenu => Message::Window(WindowMsg::ToggleTargetMenu),
            Self::AppMenu => Message::Window(WindowMsg::ToggleAppMenu),
            Self::ViewMenu => Message::Window(WindowMsg::ToggleViewMenu),
            Self::ViewsMenu => Message::Window(WindowMsg::ToggleViewsMenu),
            Self::Graph(_) => Message::Graph(GraphMsg::CloseOverlay),
            Self::Finder => Message::Nav(NavMsg::FinderClosed),
            Self::ContextMenu(_) => Message::Hover(HoverMsg::ContextMenuClosed),
            // Not `HoverMsg::Cleared`: that one keeps a peek the pointer rests in.
            // Unpinning drops the peek wherever the pointer is.
            Self::Hover(_) => Message::Hover(HoverMsg::Pin(false)),
        }
    }
}

/// What Escape does while an overlay is up: dismiss the topmost one. `None`
/// when nothing is layered over the app (Escape then falls through to the
/// find bar / selection handling).
pub(crate) fn escape_message(app: &App) -> Option<Message> {
    match active_overlay(app) {
        // A map's search is cleared before the map is closed.
        Some(ActiveOverlay::Graph(_)) if !app.graph_search.is_empty() => {
            Some(Message::Graph(GraphMsg::Search(String::new())))
        }
        overlay => overlay.map(|o| o.dismiss_message()),
    }
}

/// The window's view.
///
/// Every region sits in a slot that is always present — a zero-size [`slot`]
/// while the region is hidden — so the widget tree keeps one shape however
/// panels come and go. iced pairs widget state with widgets by position: when
/// opening the sidebar, the bottom panel, the update banner or the tutorial
/// inserted a sibling in front of the code view (or nested it one level
/// deeper), its scrollable was rebuilt at offset 0 and reported that offset,
/// throwing the reader back to the top of the file.
pub fn view(app: &App) -> Element<'_, Message> {
    // Jump mode's codes, when they are up, go on what is built now.
    jump::begin(app.jump.is_some() || app.ring.is_some());
    heard::layer(heard::Layer::Window);
    // The divider says where the pointer is in the window; the sidebar is
    // inside its frame.
    let frame = app.window_frame;
    let left: Element<'_, Message> = if app.show_left_sidebar {
        sidebar(app)
    } else {
        slot()
    };
    // The sidebar's divider lies just past its edge, over the sheet's own,
    // in a layer of its own: not over the sidebar's padding, where its lists'
    // scrollbars run, nor drawn over the sheet's shadow at rest, and thin
    // enough to end before the gutter's breakpoint dots. The sidebar's edge
    // follows the divider's middle less half its width: the edge is the
    // divider's near side.
    let grip: Element<'_, Message> = if app.show_left_sidebar {
        let half = crate::resize::EDGE_GRIP / 2.0;
        row![
            space().width(Length::Fixed(app.sidebar_width)),
            crate::resize::Divider::vertical(move |v| {
                Message::Window(WindowMsg::ResizeSidebar(v - frame - half))
            })
            .panel(crate::resize::Panel::Before)
            .thickness(crate::resize::EDGE_GRIP),
        ]
        .height(Fill)
        .into()
    } else {
        slot()
    };
    // The sidebar on the chrome, the code's sheet raised beside it, and the
    // tool strip down the right edge; over them, the sidebar's divider.
    let main = stack![row![left, sheet(app), tool_strip(app)].height(Fill), grip,].height(Fill);
    // A slim "update available" / download-progress banner sits between the
    // toolbar and the content, reading as an extension of the toolbar chrome.
    let banner = update_banner(app).unwrap_or_else(slot);
    // The title bar's controls are the keyboard ring's last stops.
    let bar = jump::scope(toolbar(app), ring::ScopeTag::Chrome);
    let base = container(column![bar, banner, main, statusbar(app)])
        .width(Fill)
        .height(Fill)
        .style(theme::chrome);
    let base = jump::scope(base, ring::ScopeTag::Layer(heard::Layer::Window));

    // The floating Ask window (or its launcher) over the window; over it,
    // the overlay layer (at most one overlay), so the palette or a menu
    // opens above Ask too; and, above everything, the tutorial — whose
    // callout stays on top while it drives the UI underneath.
    // Each layer said as it is built, for what voice may press
    // (`heard::gather`): a dialog holding the keys leaves nothing under it.
    heard::layer(heard::Layer::Ask);
    let ask = jump::scope(ask_layer(app), ring::ScopeTag::Layer(heard::Layer::Ask));
    let overlay = active_overlay(app).map_or_else(slot, |o| {
        let layer = if o.holds_keys() {
            heard::Layer::Overlay
        } else {
            heard::Layer::Window
        };
        heard::layer(layer);
        jump::scope(o.view(app), ring::ScopeTag::Layer(layer))
    });
    heard::layer(heard::Layer::Tutorial);
    let tutorial = if app.tutorial.is_some() {
        jump::scope(
            tutorial_overlay(app),
            ring::ScopeTag::Layer(heard::Layer::Tutorial),
        )
    } else {
        slot()
    };
    // Inside the frame Clew draws round its borderless window (none on macOS
    // or maximized: padding 0, the same tree), every layer, so all of them
    // share one origin; the resize grips at the window's very edge.
    let framed = container(stack![base, ask, overlay, tutorial])
        .padding(app.window_frame)
        .style(theme::window_frame(app.window_radius));
    // Whether a widget's own overlay is drawn over it all, and which
    // renderer draws the window (`watch_overlays`); inside the frame, every
    // layer reaching the window's corners rounds its own as they are.
    let inner = (app.window_radius - app.window_frame).max(0.0);
    let window = placed::watch_overlays(
        placed::Known {
            overlay_open: app.overlay_open,
            software_renderer: app.software_renderer,
            corners: inner,
        },
        stack![framed, resize_grips()],
    );
    let up = match (&app.jump, app.ring) {
        (Some(typed), _) => Some(jump::Up::Codes(typed)),
        (None, Some(ask)) => Some(jump::Up::Ring(ask)),
        (None, None) => None,
    };
    // The tutorial, a shortcut being typed, jump mode's codes: their keys
    // are their own, Tab too.
    let tab = app.tutorial.is_none() && app.rebinding.is_none() && app.jump.is_none();
    // What is on top, whether or not it has a control of its own: a dialog
    // with none (the finder with no match) still has the window under it.
    let dialog = active_overlay(app).filter(ActiveOverlay::holds_keys);
    let top = if app.tutorial.is_some() {
        jump::OnTop {
            layer: heard::Layer::Tutorial,
            scene: None,
        }
    } else if let Some(dialog) = dialog {
        jump::OnTop {
            layer: heard::Layer::Overlay,
            scene: Some(dialog.scene()),
        }
    } else {
        jump::OnTop {
            layer: heard::Layer::Ask,
            scene: None,
        }
    };
    jump::root(
        window,
        up,
        app.ask_view == crate::AskView::Expanded,
        app.main_window,
        tab,
        top,
    )
}

/// The width of the frame drawn round the window: the design's 1 px border
/// on Linux and Windows, whose window is borderless; none on macOS, whose
/// window keeps the system's frame, nor on a window filling the screen.
pub(crate) fn window_frame_for(full: bool) -> f32 {
    if cfg!(target_os = "macos") || full {
        0.0
    } else {
        WINDOW_BORDER
    }
}

/// The design's window border.
const WINDOW_BORDER: f32 = 1.0;

/// The radius Clew rounds its window's corners to: the design's 12 on
/// Linux and Windows, where Clew draws the window whole, while the window
/// does not fill the screen and its corners can be left clear (`clear`:
/// the window system composites it, and the renderer leaves them so; see
/// `app::corners`). None elsewhere: macOS rounds the window itself
/// (`macos::WINDOW_RADIUS`), and so does Windows 11 a window whose corners
/// cannot be left clear (`window_settings`).
pub(crate) fn window_radius_for(full: bool, clear: bool) -> f32 {
    if !cfg!(target_os = "macos") && clear && !full {
        WINDOW_RADIUS
    } else {
        0.0
    }
}

/// The design's window corner.
pub(crate) const WINDOW_RADIUS: f32 = 12.0;

/// How wide the band along each edge is where a drag resizes the window.
const GRIP: f32 = 4.0;

/// The window's edges and corners, where a drag resizes it. On Linux and
/// Windows the window is borderless — clew draws its own controls — and a
/// borderless window has no frame of the system's to be resized by: these
/// thin bands, over everything, are that frame. Between them is empty space,
/// which takes no event and shows no cursor, so everything under it works as
/// before. On macOS the window keeps the system's frame (`window_settings`).
fn resize_grips<'a>() -> Element<'a, Message> {
    if cfg!(target_os = "macos") {
        return slot();
    }
    use iced::mouse::Interaction as Cursor;
    use iced::window::Direction as Edge;
    let grip =
        |edge: Edge, cursor: Cursor, width: Length, height: Length| -> Element<'a, Message> {
            mouse_area(space().width(width).height(height))
                .on_press(Message::Window(WindowMsg::ResizeDragged(edge)))
                .interaction(cursor)
                .into()
        };
    let (band, corner) = (Length::Fixed(GRIP), Length::Fixed(GRIP * 2.0));
    let top = row![
        grip(
            Edge::NorthWest,
            Cursor::ResizingDiagonallyDown,
            corner,
            band
        ),
        grip(Edge::North, Cursor::ResizingVertically, Fill, band),
        grip(Edge::NorthEast, Cursor::ResizingDiagonallyUp, corner, band),
    ];
    let sides = row![
        grip(Edge::West, Cursor::ResizingHorizontally, band, Fill),
        space().width(Fill).height(Fill),
        grip(Edge::East, Cursor::ResizingHorizontally, band, Fill),
    ]
    .height(Fill);
    let bottom = row![
        grip(Edge::SouthWest, Cursor::ResizingDiagonallyUp, corner, band),
        grip(Edge::South, Cursor::ResizingVertically, Fill, band),
        grip(
            Edge::SouthEast,
            Cursor::ResizingDiagonallyDown,
            corner,
            band
        ),
    ];
    column![top, sides, bottom].width(Fill).height(Fill).into()
}

// ---------------------------------------------------------------- hover tooltip
