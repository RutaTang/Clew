//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, Row, button, center, column, container, mouse_area, opaque, pick_list, progress_bar,
    row, scrollable, slider, space, stack, text, text_input, tooltip,
};
use iced::{Element, Fill, Font, Length, Padding};

use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::glyph::{self, Glyph};
use crate::viewer::Viewer;
use crate::{App, Message, SidebarTab, TimeScope, TimeTravel, theme};
mod dialogs;
pub(crate) use dialogs::*;
mod statusbar;
pub(crate) use statusbar::*;
mod panes;
pub(crate) use panes::*;
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
mod overlays;
pub(crate) use overlays::*;

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

pub fn bp_condition_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bp-condition-input")
}

pub fn note_input_id() -> iced::widget::Id {
    iced::widget::Id::new("bookmark-note-input")
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = Row::new();
    if app.show_left_sidebar {
        main = main.push(sidebar(app));
        main = main.push(crate::resize::Divider::vertical(Message::ResizeSidebar));
    }
    main = main.push(pane_area(app));
    // Right sidebar: the cursor-following reading-context panel.
    if let Some(rp) = right_panel(app) {
        main = main.push(crate::resize::Divider::vertical(Message::ResizeRight));
        main = main.push(rp);
    }
    // A bottom panel docks under the code, keeping it visible above. "Ask clew"
    // surfaces over the debugger when opened, so you can ask about the live state
    // while paused (the answer is grounded in the current stack + variables). Its
    // height is user-draggable via the divider between it and the code.
    let body: Element<'_, Message> = if app.show_bottom {
        column![
            main.height(Fill),
            crate::resize::Divider::horizontal(Message::ResizeBottom),
            container(bottom_panel(app)).height(Length::Fixed(app.bottom_height)),
        ]
        .height(Fill)
        .into()
    } else {
        main.height(Fill).into()
    };
    let base: Element<'_, Message> = column![toolbar(app), body, statusbar(app)].into();

    // Pick the single active overlay (if any).
    let overlay: Option<Element<'_, Message>> = if let Some(root) = &app.pending_consent {
        Some(consent_modal(root))
    } else if let Some(consent) = &app.pending_lsp_consent {
        Some(lsp_consent_modal(consent))
    } else if app.settings.open {
        Some(settings_modal(app))
    } else if app.connect.is_some() {
        Some(connect_modal(app))
    } else if app.show_shortcuts {
        Some(shortcuts_modal(app))
    } else if let Some(edit) = &app.debug.bp_cond_edit {
        Some(bp_condition_modal(app, edit))
    } else if let Some(edit) = &app.note_edit {
        Some(bookmark_note_modal(app, edit))
    } else if let Some(edit) = &app.reading_note_edit {
        Some(reading_note_modal(edit))
    } else if let Some(bw) = &app.blame_why {
        Some(why_modal(app, bw))
    } else if app.show_tools_menu {
        Some(tools_menu(app))
    } else if app.show_target_menu {
        Some(target_menu(app))
    } else if let Some(overlay) = app.overlay {
        Some(project_graph_modal(app, overlay))
    } else if app.server_panel {
        Some(server_panel_modal(app))
    } else if app.finder.open {
        Some(finder_modal(app))
    } else if let Some(menu) = &app.context_menu {
        Some(context_menu(app, menu))
    } else {
        app.hover
            .as_ref()
            .filter(|h| h.text.is_some() || h.summary.is_some() || h.diagnostic.is_some())
            .map(hover_tooltip)
    };

    // `base` is ALWAYS child 0 of a Stack — even with no overlay — so opening or
    // closing one never changes the base subtree's position in the widget tree.
    // Otherwise iced rebuilds it and resets the code view's scroll offset (a
    // right-click would snap the view back to the top).
    match overlay {
        Some(o) => stack![base, o].into(),
        None => stack![base].into(),
    }
}

// ---------------------------------------------------------------- hover tooltip
