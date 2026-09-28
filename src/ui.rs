//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, Row, button, center, column, container, mouse_area, opaque, pick_list, progress_bar,
    row, scrollable, slider, space, stack, text, text_input, tooltip,
};
use iced::{Element, Fill, Font, Length, Padding, Task};

use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::glyph::{self, Glyph};
use crate::viewer::Viewer;
use crate::{App, Message, SidebarTab, TimeScope, TimeTravel, theme};
use crate::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, DocsMsg, EditorMsg, ExplainMsg, GraphMsg,
    HoverMsg, LspMsg, NavMsg, OverviewMsg, ProjectMsg, ReadingMsg, SemanticMsg, SettingsMsg,
    TimeTravelMsg, TutorialMsg, UpdaterMsg, WalkMsg, WindowMsg,
};
mod anchored;
pub(crate) use anchored::*;
mod chrome;
pub(crate) use chrome::*;
mod ops;
pub(crate) use ops::*;
mod dialogs;
pub(crate) use dialogs::*;
mod tutorial;
pub(crate) use tutorial::*;
mod statusbar;
pub(crate) use statusbar::*;
mod panes;
pub(crate) use panes::*;
mod reader_scroll;
pub(crate) use reader_scroll::*;
mod docs_stats;
pub(crate) use docs_stats::*;
mod panels;
pub(crate) use panels::*;
mod sidebar;
pub(crate) use sidebar::*;
mod toolbar;
pub(crate) use toolbar::*;
mod explain_view;
pub(crate) use explain_view::*;
mod connect;
pub(crate) use connect::*;
mod graph;
pub(crate) use graph::*;
mod graph_labels;
mod memo;
pub use memo::ViewMemo;
pub(crate) use memo::{CallsSummary, DocsGroup, DocsKey, ImportRanks, RankedFile};
mod notebook;
pub(crate) use notebook::*;
mod overlays;
pub(crate) use overlays::*;
mod updater;
pub(crate) use updater::*;
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
    pub(crate) const EMPHASIS: f32 = ui_size(TextRole::Emphasis);
    pub(crate) const SUBTITLE: f32 = ui_size(TextRole::Subtitle);
    pub(crate) const TITLE: f32 = ui_size(TextRole::Title);
    pub(crate) const HEADING: f32 = ui_size(TextRole::Heading);
    pub(crate) const DISPLAY: f32 = ui_size(TextRole::Display);
    pub(crate) const HERO: f32 = ui_size(TextRole::Hero);
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

/// The finder's result list, so the selection can be scrolled into view.
pub fn finder_list_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-list")
}

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

pub fn find_input_id() -> iced::widget::Id {
    iced::widget::Id::new("find-input")
}

pub fn ask_input_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-input")
}

/// The Ask conversation scrollable, so a new answer can snap it to the bottom.
pub fn ask_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("ask-conversation")
}

/// The outline scrollable, so it can follow the caret's current symbol.
pub fn outline_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("outline-list")
}

/// The sidebar's horizontally-scrolling tab strip, so the active tab can be
/// scrolled into view when it sits off-screen.
pub fn sidebar_tabs_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("sidebar-tabs")
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
    LspCommand(&'a crate::PendingLspCommand),
    /// The question on screen, and how many more wait behind it.
    LspConsent(&'a crate::LspConsent, usize),
    UpdateNotes,
    Settings,
    Connect,
    Shortcuts,
    BpCondition(&'a (std::path::PathBuf, usize, String)),
    BookmarkNote(&'a (String, usize, String)),
    ReadingNote(&'a (String, String, String)),
    Why(&'a crate::BlameWhy),
    ToolsMenu,
    TargetMenu,
    Graph(crate::Overlay),
    ServerPanel,
    Finder,
    ContextMenu(&'a crate::ContextMenu),
    Hover(&'a crate::HoverState),
}

/// The overlay on top right now, if any (see [`ActiveOverlay`]).
pub(crate) fn active_overlay(app: &App) -> Option<ActiveOverlay<'_>> {
    use ActiveOverlay as O;
    let overlay = if let Some(root) = &app.pending_consent {
        O::Consent(root)
    } else if let Some(pending) = &app.proj.pending_lsp_command {
        O::LspCommand(pending)
    } else if let Some(consent) = app.proj.pending_lsp_consent.front() {
        O::LspConsent(consent, app.proj.pending_lsp_consent.len() - 1)
    } else if app.update.show_notes {
        O::UpdateNotes
    } else if app.settings.open {
        O::Settings
    } else if app.connect.is_some() {
        O::Connect
    } else if app.show_shortcuts {
        O::Shortcuts
    } else if let Some(edit) = &app.proj.bp_cond_edit {
        O::BpCondition(edit)
    } else if let Some(edit) = &app.proj.note_edit {
        O::BookmarkNote(edit)
    } else if let Some(edit) = &app.proj.reading_note_edit {
        O::ReadingNote(edit)
    } else if let Some(bw) = &app.proj.blame_why {
        O::Why(bw)
    } else if app.show_tools_menu {
        O::ToolsMenu
    } else if app.show_target_menu {
        O::TargetMenu
    } else if let Some(overlay) = app.proj.overlay {
        O::Graph(overlay)
    } else if app.server_panel {
        O::ServerPanel
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
    /// Whether this overlay holds the keys of what is under it: every one
    /// but the hover peek, which asks for none (the Esc every overlay takes
    /// aside, `escape_message`). A time-travel session under one takes none
    /// of its chords (`App::handle_key`).
    pub(crate) fn holds_keys(&self) -> bool {
        !matches!(self, Self::Hover(_))
    }

    fn view(self, app: &'a App) -> Element<'a, Message> {
        match self {
            Self::Consent(root) => consent_modal(root),
            Self::LspCommand(pending) => lsp_command_modal(pending),
            Self::LspConsent(consent, waiting) => lsp_consent_modal(consent, waiting),
            Self::UpdateNotes => update_notes_modal(app),
            Self::Settings => settings_modal(app),
            Self::Connect => connect_modal(app),
            Self::Shortcuts => shortcuts_modal(app),
            Self::BpCondition(edit) => bp_condition_modal(app, edit),
            Self::BookmarkNote(edit) => bookmark_note_modal(edit),
            Self::ReadingNote(edit) => reading_note_modal(edit),
            Self::Why(bw) => why_modal(app, bw),
            Self::ToolsMenu => tools_menu(app),
            Self::TargetMenu => target_menu(app),
            Self::Graph(overlay) => project_graph_modal(app, overlay),
            Self::ServerPanel => server_panel_modal(app),
            Self::Finder => finder_modal(app),
            Self::ContextMenu(menu) => context_menu(menu),
            Self::Hover(hover) => hover_tooltip(hover),
        }
    }

    /// The message that dismisses this overlay — the same one its Cancel /
    /// Close control or a backdrop click sends. For the consent prompts that
    /// is the decline ("Not now" / "Don't run"), as Escape means cancel.
    pub(crate) fn dismiss_message(&self) -> Message {
        match self {
            Self::Consent(_) => Message::Project(ProjectMsg::ConsentDenied),
            Self::LspCommand(_) => Message::Lsp(LspMsg::CommandDismissed),
            Self::LspConsent(..) => Message::Lsp(LspMsg::ConsentDismissed),
            Self::UpdateNotes => Message::Updater(UpdaterMsg::CloseNotes),
            Self::Settings => Message::Settings(SettingsMsg::Close),
            Self::Connect => Message::Connect(ConnectMsg::Close),
            Self::Shortcuts => Message::Window(WindowMsg::CloseShortcuts),
            Self::BpCondition(_) => Message::Debug(DebugMsg::BpConditionCancel),
            Self::BookmarkNote(_) => Message::Reading(ReadingMsg::BookmarkNoteCancel),
            Self::ReadingNote(_) => Message::Reading(ReadingMsg::NoteEditCancel),
            Self::Why(_) => Message::TimeTravel(TimeTravelMsg::BlameWhyClose),
            Self::ToolsMenu => Message::Window(WindowMsg::ToggleToolsMenu),
            Self::TargetMenu => Message::Window(WindowMsg::ToggleTargetMenu),
            Self::Graph(_) => Message::Graph(GraphMsg::CloseOverlay),
            Self::ServerPanel => Message::Lsp(LspMsg::TogglePanel),
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
    active_overlay(app).map(|o| o.dismiss_message())
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
    let left: Element<'_, Message> = if app.show_left_sidebar {
        row![
            sidebar(app),
            crate::resize::Divider::vertical(|v| Message::Window(WindowMsg::ResizeSidebar(v))),
        ]
        .into()
    } else {
        slot()
    };
    // Right sidebar: the cursor-following reading-context panel.
    let right: Element<'_, Message> = match right_panel(app) {
        Some(panel) => row![
            crate::resize::Divider::vertical(|v| Message::Window(WindowMsg::ResizeRight(v))),
            panel
        ]
        .into(),
        None => slot(),
    };
    let main = row![left, pane_area(app), right].height(Fill);
    // A bottom panel docks under the code, keeping it visible above. "Ask clew"
    // surfaces over the debugger when opened, so you can ask about the live state
    // while paused (the answer is grounded in the current stack + variables). Its
    // height is user-draggable via the divider between it and the code.
    let bottom: Element<'_, Message> = if app.show_bottom {
        column![
            crate::resize::Divider::horizontal(|v| Message::Window(WindowMsg::ResizeBottom(v))),
            container(bottom_panel(app)).height(Length::Fixed(app.bottom_height)),
        ]
        .into()
    } else {
        slot()
    };
    let body = column![main, bottom].height(Fill);
    // A slim "update available" / download-progress banner sits between the
    // toolbar and the content, reading as an extension of the toolbar chrome.
    let banner = update_banner(app).unwrap_or_else(slot);
    let base = column![toolbar(app), banner, body, statusbar(app)];

    // The overlay layer (at most one overlay) and, above everything, the
    // tutorial — whose callout stays on top while it drives the UI underneath.
    let overlay = active_overlay(app).map_or_else(slot, |o| o.view(app));
    let tutorial = if app.tutorial.is_some() {
        tutorial_overlay(app)
    } else {
        slot()
    };
    stack![base, overlay, tutorial].into()
}

// ---------------------------------------------------------------- hover tooltip
