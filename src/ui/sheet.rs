//! The code's sheet: the reading surface, raised off the window's chrome.
//!
//! Everything about the open file lives on it — its header (the file's name
//! and folder, the tools that act on it, the reading trail that led here),
//! the panes, the reading panel beside them, and the debugger's panel under
//! them — so the chrome around it (the sidebar, the top bar, the tool strip,
//! the status bar) holds only what is about the whole project.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::keymap::Action;

/// Room kept between the sheet and the tool strip, and under it.
pub(crate) const SHEET_GAP: f32 = 6.0;
/// Height of the sheet header's first row (the file's name and its tools).
pub(crate) const SHEET_HEADER_ROW_H: f32 = 44.0;
/// Height of the header's trail row, when there is a trail to show.
pub(crate) const TRAIL_ROW_H: f32 = 22.0;
/// Width of the header's file tools (Explain, Skim, Diff, Time Travel and
/// View), for the tutorial's spotlight.
pub(crate) const FILE_TOOLS_W: f32 = 420.0;
/// How many stops of the reading trail the header shows, the current last.
const TRAIL_STOPS: usize = 4;

/// Height of the sheet's header as drawn now: its first row, the trail row
/// when there is a trail, and the rule under them — or nothing, with no file
/// on the sheet.
pub(crate) fn sheet_header_height(app: &App) -> f32 {
    if !header_shows(app) {
        return 0.0;
    }
    let trail = if trail_shows(app) { TRAIL_ROW_H } else { 0.0 };
    SHEET_HEADER_ROW_H + trail + 1.0
}

/// Whether the header shows: a file is open, and no page covers the panes.
fn header_shows(app: &App) -> bool {
    app.active_viewer().is_some() && pane_cover(app).is_none()
}

/// Whether the header's trail row shows: the trail has led somewhere.
fn trail_shows(app: &App) -> bool {
    app.proj.history.recent_path(2).len() > 1
}

/// The sheet, framed in the chrome: its header, the panes with the reading
/// panel beside them, and the bottom (debugger) panel under them.
///
/// Every region sits in a slot that is always present, so the widget tree
/// keeps one shape however the panels come and go (see [`super::view`]).
pub(crate) fn sheet(app: &App) -> Element<'_, Message> {
    let header = if header_shows(app) {
        sheet_header(app)
    } else {
        slot()
    };
    // The reading panel: the cursor-following context, beside the code.
    let right: Element<'_, Message> = match right_panel(app) {
        Some(panel) => row![
            crate::resize::Divider::vertical(|v| Message::Window(WindowMsg::ResizeRight(v)))
                .panel(crate::resize::Panel::Within),
            panel
        ]
        .into(),
        None => slot(),
    };
    // The debugger's panel docks under the code, keeping it visible above.
    let bottom: Element<'_, Message> = if app.show_bottom {
        column![
            crate::resize::Divider::horizontal(|v| Message::Window(WindowMsg::ResizeBottom(v)))
                .panel(crate::resize::Panel::Within),
            container(bottom_panel(app)).height(Length::Fixed(app.bottom_height)),
        ]
        .into()
    } else {
        slot()
    };
    let body = column![header, row![pane_area(app), right].height(Fill), bottom].height(Fill);
    container(
        container(body)
            .width(Fill)
            .height(Fill)
            .padding(1)
            .clip(true)
            .style(theme::sheet),
    )
    .padding(Padding {
        top: 0.0,
        right: SHEET_GAP,
        bottom: SHEET_GAP,
        left: 0.0,
    })
    .width(Fill)
    .height(Fill)
    .into()
}

/// A labelled tool of the sheet's header.
fn file_tool<'a>(
    glyph: Glyph,
    label: &'a str,
    on: bool,
    shortcut: Option<String>,
    msg: Message,
) -> Element<'a, Message> {
    let tint = if on { theme::fg() } else { theme::fg_muted() };
    let face = button(
        row![
            glyph::icon(glyph, tint, 14.0),
            text(label).size(ts::BODY).color(tint),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::icon_button(on))
    .padding([5, 9])
    .on_press(msg);
    match shortcut {
        Some(sc) => tip_at(face, label, Some(sc), tooltip::Position::Bottom),
        None => face.into(),
    }
}

/// The sheet's header: the open file's name and folder, the tools that act
/// on it — Explain, Skim, Diff, Time Travel, and the View menu of reading
/// aids — and, under them, the reading trail that led here.
fn sheet_header(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return slot();
    };
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    let (folder, name) = match v.rel.rsplit_once('/') {
        Some((folder, name)) => (folder.to_string(), name.to_string()),
        None => (String::new(), v.rel.clone()),
    };
    let mut title = row![
        text(crate::highlight::in_order(&name).into_owned())
            .size(ts::EMPHASIS)
            .font(crate::fonts::UI_SEMIBOLD)
            .color(theme::fg_bright())
            .wrapping(Wrapping::None),
    ]
    .spacing(8)
    .align_y(iced::Center);
    if !folder.is_empty() {
        title = title.push(
            text(crate::highlight::in_order(&folder).into_owned())
                .size(ts::BODY)
                .color(theme::dim())
                .wrapping(Wrapping::None),
        );
    }
    // A markdown file reads rendered or as its source.
    if v.md.is_some() {
        let (glyph, label) = if v.show_source {
            (Glyph::Overview, "Rendered")
        } else {
            (Glyph::Note, "Source")
        };
        title = title.push(file_tool(
            glyph,
            label,
            false,
            None,
            Message::Editor(EditorMsg::ToggleMarkdownSource(app.proj.active)),
        ));
    }
    let time_travel = time_travel_on_screen(app).is_some();
    let diffing = app.proj.diff.as_ref().is_some_and(|d| d.abs == v.abs);
    let rule = container(
        container(space())
            .width(Length::Fixed(1.0))
            .height(Length::Fixed(16.0))
            .style(|_: &iced::Theme| container::Style {
                background: Some(theme::hairline().into()),
                ..container::Style::default()
            }),
    )
    .padding([0, 4]);
    let view_menu = button(
        row![
            glyph::icon(Glyph::Eye, theme::fg_muted(), 14.0),
            text("View").size(ts::BODY).color(theme::fg_muted()),
            glyph::icon(Glyph::ChevronDown, theme::dim(), 11.0),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::icon_button(app.show_view_menu))
    .padding([5, 8])
    .on_press(Message::Window(WindowMsg::ToggleViewMenu));
    let tools = row![
        file_tool(
            Glyph::Sparkle,
            "Explain",
            false,
            None,
            Message::Explain(ExplainMsg::Show(crate::explain::Node::File(v.abs.clone()))),
        ),
        file_tool(
            Glyph::Skim,
            "Skim",
            false,
            None,
            Message::Editor(EditorMsg::SkimFile),
        ),
        file_tool(
            Glyph::Diff,
            "Diff",
            diffing,
            caps(Action::ToggleDiff),
            Message::Editor(EditorMsg::ToggleDiff),
        ),
        file_tool(
            Glyph::TimeTravel,
            "Time Travel",
            time_travel,
            caps(Action::TimeTravel),
            Message::TimeTravel(TimeTravelMsg::Start { symbol: false }),
        ),
        rule,
        view_menu,
    ]
    .spacing(2)
    .align_y(iced::Center);
    let first = container(
        row![title, space().width(Fill), tools]
            .spacing(12)
            .align_y(iced::Center),
    )
    .height(Length::Fixed(SHEET_HEADER_ROW_H))
    .align_y(iced::Center)
    .padding([0.0, 12.0]);
    let trail: Element<'_, Message> = if trail_shows(app) {
        trail_row(app)
    } else {
        slot()
    };
    column![first, trail, hairline()].into()
}

/// The reading trail's last stops, oldest first, the current last: where
/// the reader came from, each a click back to it.
fn trail_row(app: &App) -> Element<'_, Message> {
    let stops = app.proj.history.recent_path(TRAIL_STOPS);
    let last = stops.len().saturating_sub(1);
    let mut line = row![glyph::icon(Glyph::Route, theme::dim(), 13.0)]
        .spacing(4)
        .align_y(iced::Center);
    for (i, (id, loc, label)) in stops.into_iter().enumerate() {
        if i > 0 {
            line = line.push(glyph::icon(Glyph::ChevronRight, theme::dim(), 10.0));
        }
        let name = match label {
            Some(label) => label.to_string(),
            None => loc
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let current = i == last;
        line = line.push(
            button(
                text(name)
                    .size(ts::SMALL)
                    .color(if current {
                        theme::fg()
                    } else {
                        theme::fg_muted()
                    })
                    .wrapping(Wrapping::None),
            )
            .style(theme::icon_button(false))
            .padding([1, 5])
            .on_press(Message::Reading(ReadingMsg::HistoryJump {
                id,
                loc: loc.clone(),
            })),
        );
    }
    container(line)
        .height(Length::Fixed(TRAIL_ROW_H))
        .align_y(iced::Center)
        .padding(Padding {
            top: 0.0,
            right: 12.0,
            bottom: 4.0,
            left: 14.0,
        })
        .into()
}
