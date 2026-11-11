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
/// Height of the header's trail row: the trail 22 high, with 12 under it.
pub(crate) const TRAIL_ROW_H: f32 = 34.0;
/// Height of a stop of the trail, a button.
const TRAIL_STOP_H: f32 = 22.0;
/// Height of the controls of the header's first row: the title's line and
/// the tools, each a button.
const HEADER_CONTROL_H: f32 = 28.0;
/// Height of a margin tab.
const MARGIN_TAB_H: f32 = 26.0;
/// How many stops of the reading trail the header shows, the current last.
const TRAIL_STOPS: usize = 4;

/// Height of the sheet's header as drawn now: its first row, the trail row
/// and the rule under them — or nothing, with no file on the sheet. The
/// trail row stands from the first file on, one stop long until there is a
/// way back: it came with the first jump, and moved the code down a row.
pub(crate) fn sheet_header_height(app: &App) -> f32 {
    if !header_shows(app) {
        return 0.0;
    }
    SHEET_HEADER_ROW_H + TRAIL_ROW_H + 1.0
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
    text_height_spaced(
        content,
        size,
        iced::widget::text::LineHeight::default(),
        font,
        width,
    )
}

/// [`text_height`] with its lines set `line_height` apart.
pub(crate) fn text_height_spaced(
    content: &str,
    size: f32,
    line_height: iced::widget::text::LineHeight,
    font: iced::Font,
    width: f32,
) -> f32 {
    use iced::advanced::text::{self, paragraph::Plain};
    let paragraph = Plain::<<iced::Renderer as text::Renderer>::Paragraph>::new(text::Text {
        content: content.to_string(),
        bounds: iced::Size::new(width.max(1.0), f32::INFINITY),
        size: size.into(),
        line_height,
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

/// Width of a tool of the header ([`file_tool`]): labelled, or `short`, its
/// glyph alone.
fn file_tool_w(label: &str, short: bool) -> f32 {
    if short {
        return FILE_TOOL_PAD_X * 2.0 + FILE_TOOL_GLYPH;
    }
    FILE_TOOL_PAD_X * 2.0
        + FILE_TOOL_GLYPH
        + FILE_TOOL_SPACING
        + text_width(label, ts::BODY, FILE_TOOL_FONT)
}

/// The least room the file's name keeps beside the header's tools: its
/// width, up to this.
const TITLE_MIN_W: f32 = 160.0;

/// The least room the file's folder keeps beside its name before the
/// header's tools give up their words: its width, up to this.
const FOLDER_MIN_W: f32 = 96.0;

/// Width of the header's tools, Explain through View, labelled or `short`.
fn file_tools_w(short: bool) -> f32 {
    let tools = [tr!("Explain"), tr!("Skim"), tr!("Diff"), tr!("Time Travel")]
        .iter()
        .map(|l| file_tool_w(l, short))
        .sum::<f32>();
    tools + TOOLS_RULE_W + view_button_w(short) + 5.0 * TOOLS_SPACING
}

/// Whether the header's tools are said short — their glyphs alone, their
/// names in their tooltips: where, labelled, they would leave the file's
/// name less than its room. In a narrow window, or a language of longer
/// words, the labelled tools took the whole row and the name was cut to
/// nothing.
fn file_tools_short(app: &App) -> bool {
    let Some(v) = app.active_viewer() else {
        return false;
    };
    let (name, folder) = title_parts(v);
    // The name, and enough of its folder to say where it is: the tools'
    // words go first. In French, "Voyage dans le temps" and "Affichage"
    // left the folder "…op".
    let folder = if folder.is_empty() {
        0.0
    } else {
        TITLE_SPACING + text_width(&folder, ts::BODY, crate::fonts::UI).min(FOLDER_MIN_W)
    };
    let title = text_width(&name, ts::EMPHASIS, TITLE_FONT).min(TITLE_MIN_W) + folder;
    let room = header_right(app) - HEADER_PAD_RIGHT - (sheet_content_left(app) + HEADER_PAD_X);
    room - TITLE_GAP / 2.0 - file_tools_w(false) < title
}

/// The open file's `folder` as the header has room to show it beside
/// its `name` and the tools: whole, cut short from its start ("…/tests"),
/// or not at all when even "…" has no room.
fn folder_shown(app: &App, name: &str, folder: &str) -> String {
    if folder.is_empty() {
        return String::new();
    }
    let short = file_tools_short(app);
    let markdown = app.active_viewer().is_some_and(|v| v.md.is_some());
    let toggle = if markdown {
        file_tool_w(tr!("Rendered"), false).max(file_tool_w(tr!("Source"), false)) + TITLE_SPACING
    } else {
        0.0
    };
    let room = header_right(app)
        - HEADER_PAD_RIGHT
        - (sheet_content_left(app) + HEADER_PAD_X)
        - TITLE_GAP
        - file_tools_w(short)
        - text_width(name, ts::EMPHASIS, TITLE_FONT)
        - TITLE_SPACING
        - toggle;
    if room < text_width("…", ts::BODY, crate::fonts::UI) {
        return String::new();
    }
    super::steady::elide_start(folder, ts::BODY, crate::fonts::UI, room)
}

/// The room between the name, its folder and the Markdown toggle.
const TITLE_SPACING: f32 = 8.0;

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

/// Width of the View menu's button: its eye, its name unless `short`, and
/// its chevron.
fn view_button_w(short: bool) -> f32 {
    let named = if short {
        0.0
    } else {
        6.0 + text_width(tr!("View"), ts::BODY, FILE_TOOL_FONT)
    };
    VIEW_PAD.x() + 14.0 + named + 6.0 + VIEW_CHEVRON
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
/// out: a title too long for the room left is cut, not the tools (whose
/// words are longer in some languages than in English); `None` while the
/// header is not drawn.
pub(crate) fn file_tools_rect(app: &App) -> Option<iced::Rectangle> {
    if !header_shows(app) {
        return None;
    }
    let row_left = sheet_content_left(app) + HEADER_PAD_X;
    let width = file_tools_w(file_tools_short(app));
    let x = (header_right(app) - HEADER_PAD_RIGHT - width).max(row_left);
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
    let w = view_button_w(file_tools_short(app));
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
            container(bottom_panel(app)).height(Length::Fixed(app.bottom_shown_height())),
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
    about: Option<&'a str>,
    on: bool,
    shortcut: Option<String>,
    msg: Message,
    short: bool,
) -> Element<'a, Message> {
    let tint = if on { theme::fg() } else { theme::fg_muted() };
    let mut face = row![glyph::icon(glyph, tint, FILE_TOOL_GLYPH)]
        .spacing(FILE_TOOL_SPACING)
        .align_y(iced::Center);
    if !short {
        face = face.push(ts::set_in(
            text(label)
                .font(FILE_TOOL_FONT)
                .color(tint)
                .wrapping(Wrapping::None),
            HEADER_CONTROL_H,
            ts::INTER,
            ts::BODY,
        ));
    }
    let face = button(
        container(face)
            .height(Length::Fixed(HEADER_CONTROL_H))
            .align_y(iced::Center),
    )
    .style(theme::icon_button(on))
    .padding([0.0, FILE_TOOL_PAD_X])
    .on_press(super::heard::said(label, msg));
    // Its tooltip says what it does (`about`) where its name alone does
    // not, with its shortcut; said short, the tooltip is where its name is.
    let tip = about.unwrap_or(label);
    match shortcut {
        Some(sc) => tip_at(face, tip, Some(sc), tooltip::Position::Bottom),
        None if short || about.is_some() => tip_at(face, tip, None, tooltip::Position::Bottom),
        None => face.into(),
    }
}

/// The margin's tabs — Notes, Calls, History — in the header, over the
/// margin they switch.
fn margin_tabs(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: crate::MarginTab| {
        let chosen = app.margin_tab == this;
        button(ts::set_in(
            text(label)
                .font(FILE_TOOL_FONT)
                .color(if chosen {
                    theme::fg()
                } else {
                    theme::fg_muted()
                })
                .wrapping(Wrapping::None),
            MARGIN_TAB_H,
            ts::INTER,
            ts::BODY,
        ))
        .style(theme::segment(chosen))
        .padding([0, 10])
        .on_press(super::heard::said(
            label,
            Message::Window(WindowMsg::MarginTabPicked(this)),
        ))
    };
    row![
        tab(tr!("Explanations"), crate::MarginTab::Notes),
        tab(tr!("Calls"), crate::MarginTab::Calls),
        tab(tr!("History"), crate::MarginTab::History),
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
    // The folder, in the room the name and the tools leave it — cut short
    // with "…" where it does not fit: the row clipped it mid-word ("test"
    // for "tests", in French, whose tools take more room).
    let folder = folder_shown(app, &name, &folder);
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
            (Glyph::OpenBook, tr!("Rendered"))
        } else {
            (Glyph::Note, tr!("Source"))
        };
        title = title.push(file_tool(
            glyph,
            label,
            None,
            false,
            None,
            Message::Editor(EditorMsg::ToggleMarkdownSource(app.proj.active)),
            false,
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
    // Said short where the file's name would otherwise lose its room.
    let short = file_tools_short(app);
    let mut view_face = row![glyph::icon(Glyph::Eye, theme::fg_muted(), 14.0)]
        .spacing(6)
        .align_y(iced::Center);
    if !short {
        view_face = view_face.push(ts::set_in(
            text(tr!("View"))
                .font(FILE_TOOL_FONT)
                .color(theme::fg_muted())
                .wrapping(Wrapping::None),
            HEADER_CONTROL_H,
            ts::INTER,
            ts::BODY,
        ));
    }
    let view_face = view_face.push(glyph::icon(Glyph::ChevronDown, theme::dim(), VIEW_CHEVRON));
    let view_menu = button(
        container(view_face)
            .height(Length::Fixed(HEADER_CONTROL_H))
            .align_y(iced::Center),
    )
    .style(theme::icon_button(app.show_view_menu))
    .padding(VIEW_PAD)
    .on_press(super::heard::said(
        tr!("View"),
        Message::Window(WindowMsg::ToggleViewMenu),
    ));
    let view_menu: Element<'_, Message> = if short {
        tip_at(view_menu, tr!("View"), None, tooltip::Position::Bottom)
    } else {
        view_menu.into()
    };
    let unchanged = !diffing
        && v.git
            .as_ref()
            .is_some_and(|g| g.status.iter().all(Option::is_none) && g.deleted_at.is_empty());
    let tools = row![
        file_tool(
            Glyph::TextLines,
            tr!("Explain"),
            Some(tr!(
                "Explain this file with AI (an AI call for each part not yet explained)"
            )),
            false,
            None,
            Message::Explain(ExplainMsg::Explain(crate::explain::Node::File(
                v.abs.clone(),
            ))),
            short,
        ),
        file_tool(
            Glyph::Skim,
            tr!("Skim"),
            Some(tr!("Skim: fold every body to its signature")),
            v.skimmed(&app.skim_lines(&v.abs)),
            None,
            Message::Editor(EditorMsg::SkimFile),
            short,
        ),
        // Said before it is pressed when there is nothing to show: on an
        // unchanged file, Diff replaced the code with one line saying so.
        file_tool(
            Glyph::Diff,
            tr!("Diff"),
            Some(if unchanged {
                tr!("No changes since the last commit")
            } else {
                tr!("Changes since the last commit")
            }),
            diffing,
            caps(Action::ToggleDiff),
            Message::Editor(EditorMsg::ToggleDiff),
            short,
        ),
        // A toggle, as Diff is: pressed again while it is on, it leaves —
        // where only the Exit at the scrubber's end did.
        file_tool(
            Glyph::TimeTravel,
            tr!("Time Travel"),
            None,
            time_travel,
            caps(Action::TimeTravel),
            if time_travel {
                Message::TimeTravel(TimeTravelMsg::Exit)
            } else {
                Message::TimeTravel(TimeTravelMsg::Start { symbol: false })
            },
            short,
        ),
        rule,
        view_menu,
    ]
    .spacing(TOOLS_SPACING)
    .align_y(iced::Center);
    // The tools at the row's right end, as the design has them: the room
    // the title leaves goes between the two.
    let first = container(
        row![container(title).width(Fill).clip(true), tools]
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
    let trail = trail_row(app);
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
/// the reader came from, each a click back to it; the file on the sheet
/// alone before there is a way back.
fn trail_row(app: &App) -> Element<'_, Message> {
    let stops = merged_stops(app.proj.history.recent_path(TRAIL_STOPS), app.first_rows());
    let last = stops.len().saturating_sub(1);
    // Its icon says what the row is where the pointer rests: read as a
    // breadcrumb, "lib.rs › Frame" over another file was taken for that
    // file's path.
    let route = super::toolbar::tip_at(
        glyph::icon(Glyph::Route, theme::dim(), 13.0),
        tr!("Reading trail"),
        None,
        iced::widget::tooltip::Position::Bottom,
    );
    let mut line = row![route].spacing(6).align_y(iced::Center);
    // No visit recorded yet (a file restored, not gone to): the file on the
    // sheet is where the trail starts.
    if stops.is_empty()
        && let Some(v) = app.active_viewer()
    {
        let name = v
            .abs
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        line = line.push(
            container(ts::set_in(
                text(name)
                    .font(crate::fonts::in_control(crate::fonts::UI_MEDIUM))
                    .color(theme::fg())
                    .wrapping(Wrapping::None),
                TRAIL_STOP_H,
                ts::INTER,
                ts::BODY,
            ))
            .padding([0, 5]),
        );
    }
    // Where the reader is now: the function the caret is in, when it is in
    // one of the current stop's file's.
    let here = match (app.cursor_target(), stops.last()) {
        (Some(crate::explain::Node::Function { file, name, .. }), Some((_, loc, _)))
            if file == loc.path =>
        {
            Some(name)
        }
        _ => None,
    };
    // A function's stop reads as one — "discounts()" — so it is not taken
    // for the module of the same name ("discounts.py") beside it.
    let is_function = |path: &std::path::Path, name: &str| {
        app.proj
            .symbol_index_by_file
            .get(path)
            .is_some_and(|symbols| {
                symbols
                    .iter()
                    .any(|s| s.name == name && matches!(s.kind.as_str(), "function" | "method"))
            })
    };
    // A stop recorded by its line, not at a definition, named by the
    // function it was in, where that is known: "discounts()" became
    // "pricing.py:20" once the reader left it.
    let around: Vec<Option<String>> = stops
        .iter()
        .map(|(_, loc, label)| match (label, loc.line) {
            (None, Some(line)) => app.function_around(&loc.path, line),
            _ => None,
        })
        .collect();
    let stops: Vec<_> = stops
        .into_iter()
        .zip(&around)
        .map(|((id, loc, label), around)| (id, loc, label.or(around.as_deref())))
        .collect();
    let names = trail_names(&stops, here, is_function);
    for (i, ((id, loc, _), name)) in stops.into_iter().zip(names).enumerate() {
        if i > 0 {
            line = line.push(glyph::icon(Glyph::ChevronRight, theme::dim(), 11.0));
        }
        let current = i == last;
        line = line.push(
            button(ts::set_in(
                text(name.clone())
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
            .on_press(super::heard::said(
                &name.to_string(),
                Message::Reading(ReadingMsg::HistoryJump {
                    id,
                    loc: loc.clone(),
                }),
            )),
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

/// The trail's stops as the row shows them: a stop Back passes over
/// ([`crate::history::History::changes_nothing`], for a view `first_rows`
/// lines tall) is not one — "main.py › main" was one place said twice, and
/// Back from the second seemed to do nothing.
fn merged_stops<'a>(
    stops: Vec<(usize, &'a crate::history::Loc, Option<&'a str>)>,
    first_rows: usize,
) -> Vec<(usize, &'a crate::history::Loc, Option<&'a str>)> {
    let keep: Vec<bool> = (0..stops.len())
        .map(|i| {
            let (_, loc, _) = stops[i];
            !stops.get(i + 1).is_some_and(|(_, next, _)| {
                crate::history::History::changes_nothing(loc, next, first_rows)
            })
        })
        .collect();
    stops
        .into_iter()
        .zip(keep)
        .filter_map(|(stop, keep)| keep.then_some(stop))
        .collect()
}

/// What each trail stop is called: the function it was in, else its file's
/// name — with its line when a stop beside it has the same name, which two
/// visits to one file outside any function otherwise read as ("cart.py ›
/// cart.py"). `here` names the current (last) stop when the caret is in a
/// function of its file. A function's name is said with `()`
/// (`is_function` says which names in a file are functions).
fn trail_names(
    stops: &[(usize, &crate::history::Loc, Option<&str>)],
    here: Option<String>,
    is_function: impl Fn(&std::path::Path, &str) -> bool,
) -> Vec<String> {
    let last = stops.len().saturating_sub(1);
    let mut here = here;
    let mut names: Vec<(String, bool)> = stops
        .iter()
        .enumerate()
        .map(
            |(i, (_, loc, label))| match (here.take_if(|_| i == last), label) {
                (Some(here), _) => (format!("{here}()"), true),
                (None, Some(label)) if is_function(&loc.path, label) => {
                    (format!("{label}()"), true)
                }
                (None, Some(label)) => (label.to_string(), true),
                (None, None) => (
                    loc.path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    false,
                ),
            },
        )
        .collect();
    // Beside a stop of the same name, a stop says its line: a file's or a
    // function's twice in a row read as a loop ("main() › main()").
    let twins: Vec<bool> = (0..names.len())
        .map(|i| {
            let same = |j: usize| names.get(j).is_some_and(|(n, _)| *n == names[i].0);
            (i > 0 && same(i - 1)) || same(i + 1)
        })
        .collect();
    for (i, twin) in twins.into_iter().enumerate() {
        if twin && let Some(line) = stops[i].1.line {
            names[i].0 = format!("{}:{line}", names[i].0);
        }
    }
    names.into_iter().map(|(n, _)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two stops beside each other in one file, outside any function, are
    /// told apart by their lines; a named stop and a lone file are not.
    #[test]
    fn neighbouring_stops_in_one_file_show_their_lines() {
        let loc = |p: &str, line| crate::history::Loc {
            path: std::path::PathBuf::from(p),
            line,
        };
        let (a18, a1, b) = (
            loc("src/cart.py", Some(18)),
            loc("src/cart.py", Some(1)),
            loc("src/main.py", Some(4)),
        );
        let stops = vec![
            (0, &b, None),
            (1, &a18, Some("subtotal")),
            (2, &a18, None),
            (3, &a1, None),
        ];
        let function = |_: &std::path::Path, name: &str| name == "subtotal";
        assert_eq!(
            trail_names(&stops, None, function),
            ["main.py", "subtotal()", "cart.py:18", "cart.py:1"]
        );
        // The caret in a function names the current stop.
        assert_eq!(
            trail_names(&stops, Some("total".into()), function),
            ["main.py", "subtotal()", "cart.py", "total()"]
        );
        // A name that is no function (a class) is said as it is.
        assert_eq!(trail_names(&stops, None, |_, _| false)[1], "subtotal");
        // One function twice in a row, at two places in it: its lines,
        // not "subtotal() › subtotal()".
        let a24 = loc("src/cart.py", Some(24));
        let twice = vec![(0, &a18, Some("subtotal")), (1, &a24, Some("subtotal"))];
        assert_eq!(
            trail_names(&twice, None, function),
            ["subtotal():18", "subtotal():24"]
        );
    }

    /// A file's stop followed by one near its top is shown as that one:
    /// "main.py › main" was one place said twice. Down the file, Back stops
    /// at the file's top, and so does the trail.
    #[test]
    fn a_files_stop_merges_into_its_functions() {
        let loc = |p: &str, line| crate::history::Loc {
            path: std::path::PathBuf::from(p),
            line,
        };
        let (main_top, main_fn, cart) = (
            loc("main.py", None),
            loc("main.py", Some(12)),
            loc("cart.py", Some(3)),
        );
        let stops = || {
            vec![
                (0, &main_top, None),
                (1, &main_fn, Some("main")),
                (2, &cart, None),
                (3, &cart, None),
            ]
        };
        let ids: Vec<usize> = merged_stops(stops(), 40).iter().map(|s| s.0).collect();
        assert_eq!(ids, [1, 3], "the same place twice is one stop");
        let ids: Vec<usize> = merged_stops(stops(), 10).iter().map(|s| s.0).collect();
        assert_eq!(ids, [0, 1, 3], "line 12 is off a 10-line screen");
    }

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
