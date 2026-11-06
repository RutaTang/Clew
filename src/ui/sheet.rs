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

/// Room kept between the sheet and the tool strip. Under it there is none:
/// it runs down to the status bar.
pub(crate) const SHEET_GAP: f32 = 8.0;
/// Height of the sheet header's first row (the file's name and its tools):
/// its controls 28 high, with 14 over them and 6 under.
pub(crate) const SHEET_HEADER_ROW_H: f32 = 48.0;
const HEADER_ROW_PAD: Padding = Padding {
    top: 14.0,
    right: 0.0,
    bottom: 6.0,
    left: 0.0,
};
/// Height of the header's trail row, when there is a trail to show: the
/// trail 22 high, with 12 under it.
pub(crate) const TRAIL_ROW_H: f32 = 34.0;
/// Height of a stop of the trail, a button.
const TRAIL_STOP_H: f32 = 22.0;
/// Height of the controls of the header's first row: the title's line and
/// the tools, each a button.
const HEADER_CONTROL_H: f32 = 28.0;
/// Height of a margin tab.
const MARGIN_TAB_H: f32 = 26.0;
/// Room under the first row when there is no trail under it.
const NO_TRAIL_PAD: f32 = 6.0;
/// How many stops of the reading trail the header shows, the current last.
const TRAIL_STOPS: usize = 4;

/// Height of the sheet's header as drawn now: its first row, the trail row
/// when there is a trail, and the rule under them — or nothing, with no file
/// on the sheet.
pub(crate) fn sheet_header_height(app: &App) -> f32 {
    if !header_shows(app) {
        return 0.0;
    }
    let trail = if trail_shows(app) {
        TRAIL_ROW_H
    } else {
        NO_TRAIL_PAD
    };
    SHEET_HEADER_ROW_H + trail + 1.0
}

/// Width of `content` set at `size` in `font`, as a text widget lays it
/// out — so what follows a run of text can be placed exactly.
pub(crate) fn text_width(content: &str, size: f32, font: iced::Font) -> f32 {
    use iced::advanced::text::{self, paragraph::Plain};
    let paragraph = Plain::<<iced::Renderer as text::Renderer>::Paragraph>::new(text::Text {
        content: content.to_string(),
        bounds: iced::Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::default(),
        wrapping: text::Wrapping::None,
    });
    paragraph.min_bounds().width
}

/// How tall `content` is set at `size` in `font`, wrapped at words to
/// `width`.
pub(crate) fn text_height(content: &str, size: f32, font: iced::Font, width: f32) -> f32 {
    use iced::advanced::text::{self, paragraph::Plain};
    let paragraph = Plain::<<iced::Renderer as text::Renderer>::Paragraph>::new(text::Text {
        content: content.to_string(),
        bounds: iced::Size::new(width.max(1.0), f32::INFINITY),
        size: size.into(),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::default(),
        wrapping: text::Wrapping::Word,
    });
    paragraph.min_bounds().height
}

/// Right edge of the sheet's content, in window coordinates: its border is
/// inside the gap before the tool strip.
fn sheet_content_right(window_width: f32) -> f32 {
    window_width - STRIP_W - SHEET_GAP - 1.0
}

/// How wide the margin is with the middle of its divider at `x` (the
/// divider lies over the margin's edge).
pub(crate) fn margin_width_at(window_width: f32, x: f32) -> f32 {
    sheet_content_right(window_width) - (x - crate::resize::THICKNESS / 2.0)
}

/// Right edge of the header's file row, in window coordinates: the sheet's
/// content, less the margin's tabs beside it.
fn header_right(app: &App) -> f32 {
    let tabs = if super::panes::right_panel_shown(app) {
        app.right_width
    } else {
        0.0
    };
    sheet_content_right(app.window_width) - tabs
}

/// Left edge of the sheet's content, in window coordinates.
fn sheet_content_left(app: &App) -> f32 {
    // The sidebar's divider lies over it, taking no room of its own.
    let sidebar = if app.show_left_sidebar {
        app.sidebar_width
    } else {
        0.0
    };
    sidebar + 1.0
}

/// The open file's name and folder, as the header shows them.
fn title_parts(v: &Viewer) -> (String, String) {
    match v.rel.rsplit_once('/') {
        Some((folder, name)) => (
            crate::highlight::in_order(name).into_owned(),
            crate::highlight::in_order(folder).into_owned(),
        ),
        None => (
            crate::highlight::in_order(&v.rel).into_owned(),
            String::new(),
        ),
    }
}

/// Width of a labelled tool of the header ([`file_tool`]).
fn file_tool_w(label: &str) -> f32 {
    FILE_TOOL_PAD_X * 2.0
        + FILE_TOOL_GLYPH
        + FILE_TOOL_SPACING
        + text_width(label, ts::BODY, FILE_TOOL_FONT)
}

/// The face of the file's name over the sheet: a title's.
const TITLE_FONT: iced::Font = crate::fonts::titled(crate::fonts::UI_SEMIBOLD);
/// The face of what the header's tools say: in a control.
const FILE_TOOL_FONT: iced::Font = crate::fonts::in_control(crate::fonts::UI_MEDIUM);

/// Inner padding across a header tool, its glyph's size, and the gap
/// between glyph and label.
const FILE_TOOL_PAD_X: f32 = 9.0;
const FILE_TOOL_GLYPH: f32 = 14.0;
const FILE_TOOL_SPACING: f32 = 6.0;
/// Gaps between the header's tools, and between its title and them.
const TOOLS_SPACING: f32 = 2.0;
const TITLE_GAP: f32 = 10.0;
/// The rule before the View menu: a hairline with room either side.
const TOOLS_RULE_W: f32 = 9.0;
/// Padding along the header's row: 20 before the title, 16 after the
/// tools, as the design has it.
const HEADER_PAD_X: f32 = 20.0;
const HEADER_PAD_RIGHT: f32 = 16.0;

/// Width of the header's title: the file's name, its folder, and, for a
/// markdown file, the switch between it rendered and its source.
fn title_w(app: &App, v: &Viewer) -> f32 {
    let (name, folder) = title_parts(v);
    let mut w = text_width(&name, ts::EMPHASIS, TITLE_FONT);
    if !folder.is_empty() {
        w += 8.0 + text_width(&folder, ts::BODY, crate::fonts::UI);
    }
    if v.md.is_some() {
        let label = if v.show_source { "Rendered" } else { "Source" };
        w += 8.0 + file_tool_w(label);
    }
    let _ = app;
    w
}

/// Width of the View menu's button.
fn view_button_w() -> f32 {
    VIEW_PAD.x() + 14.0 + 6.0 + text_width("View", ts::BODY, FILE_TOOL_FONT) + 6.0 + VIEW_CHEVRON
}

/// The View menu's button: the design's 9 before what it holds, 8 after,
/// and the chevron it ends with.
const VIEW_PAD: Padding = Padding {
    top: 0.0,
    right: 8.0,
    bottom: 0.0,
    left: 9.0,
};
const VIEW_CHEVRON: f32 = 10.0;

/// Where the header's file tools are drawn (Explain through View), in
/// window coordinates — at the header's right end, as the header lays them
/// out, or just after the file's name when it is too long for that; `None`
/// while the header is not drawn.
pub(crate) fn file_tools_rect(app: &App) -> Option<iced::Rectangle> {
    if !header_shows(app) {
        return None;
    }
    let v = app.active_viewer()?;
    let after_title = sheet_content_left(app) + HEADER_PAD_X + title_w(app, v) + TITLE_GAP;
    let tools = ["Explain", "Skim", "Diff", "Time Travel"]
        .iter()
        .map(|l| file_tool_w(l))
        .sum::<f32>();
    let width = tools + TOOLS_RULE_W + view_button_w() + 5.0 * TOOLS_SPACING;
    let x = (header_right(app) - HEADER_PAD_RIGHT - width).max(after_title);
    Some(iced::Rectangle {
        x,
        y: body_top(app) + 1.0,
        width,
        height: SHEET_HEADER_ROW_H,
    })
}

/// Where the View menu's button is drawn, in window coordinates: the last
/// of the file tools.
pub(crate) fn view_button_rect(app: &App) -> Option<iced::Rectangle> {
    let tools = file_tools_rect(app)?;
    let w = view_button_w();
    Some(iced::Rectangle {
        x: tools.x + tools.width - w,
        width: w,
        ..tools
    })
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
    // The dividers say where the pointer is in the window; the panels are
    // inside its frame.
    let frame = app.window_frame;
    // The reading panel: the cursor-following context, beside the code. The
    // two read as one surface, so the divider between them takes no room of
    // its own: it lies over the margin's edge, where nothing is written.
    let right: Element<'_, Message> = match right_panel(app) {
        Some(panel) => stack![
            panel,
            crate::resize::Divider::vertical(move |v| {
                Message::Window(WindowMsg::ResizeRight(v - frame))
            })
            .panel(crate::resize::Panel::Seamless),
        ]
        .into(),
        None => slot(),
    };
    // The debugger's panel docks under the code, keeping it visible above.
    let bottom: Element<'_, Message> = if app.show_bottom {
        column![
            crate::resize::Divider::horizontal(move |v| {
                Message::Window(WindowMsg::ResizeBottom(v - frame))
            })
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
        bottom: 0.0,
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
            glyph::icon(glyph, tint, FILE_TOOL_GLYPH),
            ts::set_in(
                text(label).font(FILE_TOOL_FONT).color(tint),
                HEADER_CONTROL_H,
                ts::INTER,
                ts::BODY,
            ),
        ]
        .spacing(FILE_TOOL_SPACING)
        .align_y(iced::Center),
    )
    .style(theme::icon_button(on))
    .padding([0.0, FILE_TOOL_PAD_X]);
    let face = face.on_press(msg);
    match shortcut {
        Some(sc) => tip_at(face, label, Some(sc), tooltip::Position::Bottom),
        None => face.into(),
    }
}

/// The margin's tabs — Notes, Calls, History — in the header, over the
/// margin they switch.
fn margin_tabs(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: crate::MarginTab| {
        let chosen = app.margin_tab == this;
        button(ts::set_in(
            text(label).font(FILE_TOOL_FONT).color(if chosen {
                theme::fg()
            } else {
                theme::fg_muted()
            }),
            MARGIN_TAB_H,
            ts::INTER,
            ts::BODY,
        ))
        .style(theme::segment(chosen))
        .padding([0, 10])
        .on_press(Message::Window(WindowMsg::MarginTabPicked(this)))
    };
    row![
        tab("Notes", crate::MarginTab::Notes),
        tab("Calls", crate::MarginTab::Calls),
        tab("History", crate::MarginTab::History),
    ]
    .spacing(2)
    .into()
}

/// The sheet's header: the open file's name and folder, the tools that act
/// on it — Explain, Skim, Diff, Time Travel, and the View menu of reading
/// aids — following them, and under them the reading trail that led here;
/// over the margin, its tabs.
fn sheet_header(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return slot();
    };
    let caps = |action: Action| Some(app.keymap.chord(action).caps());
    let (name, folder) = title_parts(v);
    // The design sets the name and its folder a line high (`line-height:
    // 1`).
    let mut title = row![ts::set_in_line(
        text(name)
            .font(TITLE_FONT)
            .color(theme::fg_bright())
            .wrapping(Wrapping::None),
        HEADER_CONTROL_H,
        ts::EMPHASIS,
        ts::INTER,
        ts::EMPHASIS,
    )]
    .spacing(8)
    .align_y(iced::Center);
    if !folder.is_empty() {
        title = title.push(ts::set_in_line(
            text(folder).color(theme::dim()).wrapping(Wrapping::None),
            HEADER_CONTROL_H,
            ts::BODY,
            ts::INTER,
            ts::BODY,
        ));
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
    .padding([0.0, (TOOLS_RULE_W - 1.0) / 2.0]);
    let view_menu = button(
        row![
            glyph::icon(Glyph::Eye, theme::fg_muted(), 14.0),
            ts::set_in(
                text("View").font(FILE_TOOL_FONT).color(theme::fg_muted()),
                HEADER_CONTROL_H,
                ts::INTER,
                ts::BODY,
            ),
            glyph::icon(Glyph::ChevronDown, theme::dim(), VIEW_CHEVRON),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::icon_button(app.show_view_menu))
    .padding(VIEW_PAD)
    .on_press(Message::Window(WindowMsg::ToggleViewMenu));
    let tools = row![
        file_tool(
            Glyph::TextLines,
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
    .spacing(TOOLS_SPACING)
    .align_y(iced::Center);
    // The tools at the row's right end, as the design has them: the room
    // the title leaves goes between the two.
    let first = container(
        row![title, space().width(Fill), tools]
            .spacing(TITLE_GAP / 2.0)
            .align_y(iced::Center),
    )
    .width(Fill)
    .height(Length::Fixed(SHEET_HEADER_ROW_H))
    .align_y(iced::Center)
    .clip(true)
    .padding(Padding {
        left: HEADER_PAD_X,
        right: HEADER_PAD_RIGHT,
        ..HEADER_ROW_PAD
    });
    let trail: Element<'_, Message> = if trail_shows(app) {
        trail_row(app)
    } else {
        space().height(Length::Fixed(NO_TRAIL_PAD)).into()
    };
    // Over the margin, beside the divider, its tabs.
    let tabs: Element<'_, Message> = if right_panel(app).is_some() {
        // At the row's top, as the design has them: 14 down.
        container(margin_tabs(app))
            .width(Length::Fixed(app.right_width))
            .height(Length::Fixed(SHEET_HEADER_ROW_H))
            .align_y(iced::Top)
            .padding(Padding {
                right: 16.0,
                left: 12.0,
                ..HEADER_ROW_PAD
            })
            .into()
    } else {
        slot()
    };
    column![row![column![first, trail].width(Fill), tabs], hairline()].into()
}

/// The reading trail's last stops, oldest first, the current last: where
/// the reader came from, each a click back to it.
fn trail_row(app: &App) -> Element<'_, Message> {
    let stops = app.proj.history.recent_path(TRAIL_STOPS);
    let last = stops.len().saturating_sub(1);
    let mut line = row![glyph::icon(Glyph::Route, theme::dim(), 13.0)]
        .spacing(6)
        .align_y(iced::Center);
    for (i, (id, loc, label)) in stops.into_iter().enumerate() {
        if i > 0 {
            line = line.push(glyph::icon(Glyph::ChevronRight, theme::dim(), 11.0));
        }
        let current = i == last;
        // Where the reader is now: the function the caret is in, when it is
        // in one of this file's.
        let here = match app.cursor_target() {
            Some(crate::explain::Node::Function { file, name, .. })
                if current && file == loc.path =>
            {
                Some(name)
            }
            _ => None,
        };
        let name = match (here, label) {
            (Some(here), _) => here,
            (None, Some(label)) => label.to_string(),
            (None, None) => loc
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        line = line.push(
            button(ts::set_in(
                text(name)
                    .font(crate::fonts::in_control(if current {
                        crate::fonts::UI_MEDIUM
                    } else {
                        crate::fonts::UI
                    }))
                    .color(if current {
                        theme::fg()
                    } else {
                        theme::fg_muted()
                    })
                    .wrapping(Wrapping::None),
                TRAIL_STOP_H,
                ts::INTER,
                ts::BODY,
            ))
            .style(theme::icon_button(false))
            .padding([0, 5])
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
            bottom: 12.0,
            left: HEADER_PAD_X,
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divider over the margin's edge, held by its middle and not yet
    /// moved, leaves the margin as wide as it was; moved, the margin follows.
    #[test]
    fn the_margin_follows_its_divider() {
        let (window, margin) = (1216.0, 272.0);
        let edge = sheet_content_right(window) - margin;
        let middle = edge + crate::resize::THICKNESS / 2.0;
        assert_eq!(margin_width_at(window, middle), margin);
        assert_eq!(margin_width_at(window, middle - 30.0), margin + 30.0);
    }
}
