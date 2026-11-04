//! The interactive-tutorial overlay: a spotlight that dims everything except the
//! region a step is about, plus a callout card placed next to it.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::app::tutorial::{Anchor, steps};
use crate::ui::StripTool;

/// Half the side of the spotlight hole over a single strip tool.
const TOOL_HOLE_HALF: f32 = 19.0;
/// Gap between the callout card and the region it points at.
const CARD_GAP: f32 = 24.0;

/// The screen rectangle a step points at, or `None` for a full-window dim (the
/// welcome / summary steps, or a step whose panel happens to be closed).
///
/// Every number comes from the chrome that is actually drawn: the top bar's
/// height and the chat field's place (`ui::toolbar`), the strip's slots and
/// its menus, the update banner's height when one shows (it pushes the body
/// down), the sheet's margins and header (`ui::sheet`), the panels the view
/// actually shows (`right_panel_shown`) with the divider strip beside each,
/// and the status bar's height.
pub(crate) fn region_rect(app: &App, anchor: Anchor) -> Option<iced::Rectangle> {
    let ww = app.window_width;
    let wh = app.window_height;
    let divider = crate::resize::THICKNESS;
    let sidebar = if app.show_left_sidebar {
        app.sidebar_width
    } else {
        0.0
    };
    let sidebar_edge = if app.show_left_sidebar {
        sidebar + divider
    } else {
        0.0
    };
    let right = if right_panel_shown(app) {
        app.right_width
    } else {
        0.0
    };
    let right_edge = if right > 0.0 { right + divider } else { 0.0 };
    let bottom = if app.show_bottom {
        app.bottom_height + divider
    } else {
        0.0
    };
    let top = TOOLBAR_H + update_banner_height(app);
    let bot = wh - STATUSBAR_H;
    // The sheet: between the sidebar and the strip, its gap kept to the
    // strip and above the status bar.
    let sheet_right = ww - STRIP_W - SHEET_GAP;
    let sheet_bottom = bot - SHEET_GAP;
    // Its panes start under its header and end above the bottom panel.
    let main_top = top + sheet_header_height(app);
    let main_bot = sheet_bottom - bottom;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        Some(iced::Rectangle {
            x,
            y,
            width: w,
            height: h,
        })
    };
    match anchor {
        Anchor::Center => None,
        // The top bar's left cluster: the window buttons (or the app menu),
        // the sidebar toggle, back / forward.
        Anchor::ToolbarLeft => rect(0.0, 0.0, left_cluster_w(app) + NAV_W, TOOLBAR_H),
        Anchor::ChatField => {
            let f = chat_field_rect(app);
            rect(f.x, f.y, f.width, f.height)
        }
        Anchor::Strip(tool) => {
            let c = strip_tool_center(app, tool);
            rect(
                c.x - TOOL_HOLE_HALF,
                c.y - TOOL_HOLE_HALF,
                2.0 * TOOL_HOLE_HALF,
                2.0 * TOOL_HOLE_HALF,
            )
        }
        // A row (or run of rows) in the opened ⋯ menu. The hole hugs the item
        // column exactly (no bleed past the panel edge), so the inset outline
        // frames it evenly without catching a neighbour.
        Anchor::ToolbarMenu { first, count } => Some(tools_menu_rows_rect(app, first, count)),
        Anchor::FileTools => file_tools_rect(app),
        Anchor::ViewMenu => rect(
            view_menu_left(app),
            view_menu_top(app),
            VIEW_MENU_W,
            view_menu_height(),
        ),
        Anchor::Sidebar if sidebar > 1.0 => rect(0.0, top, sidebar, bot - top),
        // The margin: its notes, beside the code.
        Anchor::RightTop if right > 1.0 => {
            rect(sheet_right - right, main_top, right, main_bot - main_top)
        }
        Anchor::Main => rect(
            sidebar_edge,
            main_top,
            sheet_right - sidebar_edge - right_edge,
            main_bot - main_top,
        ),
        _ => None,
    }
}

/// Canvas that dims the window except `hole` (if any) and outlines it — the
/// spotlight. Drawn as four dim rectangles around the hole so the highlighted
/// region shows through at full brightness.
struct Spotlight {
    hole: Option<iced::Rectangle>,
    /// How far the accent outline sits from the hole edge. Positive = just
    /// outside (frames without covering the region's own content); negative =
    /// just inside (for tightly-packed rows like the ⋯ menu, so the frame never
    /// spills onto the panel edge or the next row).
    outline_pad: f32,
}

impl iced::widget::canvas::Program<Message> for Spotlight {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &iced::Renderer,
        _theme: &iced::Theme,
        bounds: iced::Rectangle,
        _cursor: iced::advanced::mouse::Cursor,
    ) -> Vec<iced::widget::canvas::Geometry> {
        use iced::widget::canvas::{Frame, Path, Stroke};
        let mut frame = Frame::new(renderer, bounds.size());
        let dim = theme::scrim(0.6);
        let pt = iced::Point::new;
        let sz = iced::Size::new;
        let w = bounds.width;
        let h = bounds.height;

        match self.hole {
            None => frame.fill_rectangle(pt(0.0, 0.0), sz(w, h), dim),
            Some(r) => {
                // Dim everything around the hole (top / bottom / left / right).
                frame.fill_rectangle(pt(0.0, 0.0), sz(w, r.y), dim);
                frame.fill_rectangle(pt(0.0, r.y + r.height), sz(w, h - (r.y + r.height)), dim);
                frame.fill_rectangle(pt(0.0, r.y), sz(r.x, r.height), dim);
                frame.fill_rectangle(
                    pt(r.x + r.width, r.y),
                    sz(w - (r.x + r.width), r.height),
                    dim,
                );
                // Accent outline offset from the region by `outline_pad`:
                // positive frames just outside (so it never covers the region's
                // own content, e.g. the breadcrumb or first code line); negative
                // frames just inside (for tight ⋯-menu rows, so it hugs the row
                // without spilling onto the panel edge or the neighbouring row).
                let pad = self.outline_pad;
                let ox = (r.x - pad).max(0.0);
                let oy = (r.y - pad).max(0.0);
                let ow = ((r.x + r.width + pad).min(w) - ox).max(0.0);
                let oh = ((r.y + r.height + pad).min(h) - oy).max(0.0);
                let outline = Path::rounded_rectangle(pt(ox, oy), sz(ow, oh), 6.0.into());
                frame.stroke(
                    &outline,
                    Stroke::default()
                        .with_color(theme::accent())
                        .with_width(2.0),
                );
            }
        }
        vec![frame.into_geometry()]
    }
}

pub(crate) fn tutorial_overlay(app: &App) -> Element<'_, Message> {
    let Some(step_i) = app.tutorial else {
        return slot();
    };
    let script = steps(app);
    let total = script.len();
    let Some(step) = script.get(step_i) else {
        return slot();
    };

    // The callout card: progress · title · body · controls.
    let progress = text(format!("Step {} of {}", step_i + 1, total))
        .size(ts::SMALL)
        .color(theme::accent());
    let title = text(step.title.clone()).size(ts::TITLE).color(theme::fg());
    let body = text(step.body.clone())
        .size(ts::BASE)
        .color(theme::fg_muted())
        .wrapping(Wrapping::Word);

    let back: Element<'_, Message> = if step_i > 0 {
        button(text("Back").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press(Message::Tutorial(TutorialMsg::Step(-1)))
            .into()
    } else {
        slot()
    };
    let last = step_i + 1 == total;
    let next = button(text(if last { "Done" } else { "Next" }).size(ts::BODY))
        .style(theme::primary_button)
        .padding([4, 16])
        .on_press(Message::Tutorial(TutorialMsg::Step(1)));
    // "Skip tour" is the subtle exit, kept left and muted. Small horizontal
    // padding so its text lines up with the title/body above it, rather than
    // sitting indented by a wider button's internal padding.
    let skip = button(text("Skip tour").size(ts::BODY).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([4, 6])
        .on_press(Message::Tutorial(TutorialMsg::Exit));

    // Back and Next form the primary control group on the right; Skip sits on the
    // left. On the last step Next reads "Done".
    let controls = row![back, next].spacing(8).align_y(iced::Center);
    let card = container(
        column![
            progress,
            title,
            body,
            row![skip, space().width(Fill), controls].align_y(iced::Center),
        ]
        .spacing(12),
    )
    .width(TUTORIAL_CARD_W)
    .padding(18)
    .style(theme::modal_panel);

    // Place the card next to the region the step is about, from the same
    // chrome metrics the spotlight uses (see `region_rect`).
    use iced::alignment::{Horizontal, Vertical};
    let gap = CARD_GAP;
    // The top of the body: under the toolbar and, when one shows, the update
    // banner.
    let body_top = TOOLBAR_H + update_banner_height(app);
    // The ⋯ dropdown's left edge, measured from the window's right edge.
    let menu_left = TOOLS_MENU_RIGHT + TOOLS_MENU_W;
    let lo = body_top + 14.0;
    let hi = (app.window_height - 260.0).max(lo);
    let (ax, ay, pad) = match step.anchor {
        Anchor::Center | Anchor::Main => (Horizontal::Center, Vertical::Center, Padding::ZERO),
        Anchor::Sidebar => (
            Horizontal::Left,
            Vertical::Center,
            Padding {
                left: app.sidebar_width + crate::resize::THICKNESS + gap,
                ..Padding::ZERO
            },
        ),
        Anchor::ToolbarLeft => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: body_top + gap,
                left: gap,
                ..Padding::ZERO
            },
        ),
        // Ask hangs under the field in this step: the card goes to the
        // bottom left, clear of it.
        Anchor::ChatField => (
            Horizontal::Left,
            Vertical::Bottom,
            Padding {
                bottom: STATUSBAR_H + gap,
                left: gap,
                ..Padding::ZERO
            },
        ),
        // The ⋯ button with its menu open: the card to the menu's left, so
        // the two read as one.
        Anchor::Strip(StripTool::More) => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: (tools_menu_top(app) + 16.0).clamp(lo, hi),
                right: menu_left + gap,
                ..Padding::ZERO
            },
        ),
        // A tool on the strip: the card just left of it, level with it.
        Anchor::Strip(tool) => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: (strip_tool_center(app, tool).y - 60.0).clamp(lo, hi),
                right: STRIP_W + gap,
                ..Padding::ZERO
            },
        ),
        // For a menu row, the card's middle level with the row it points
        // at, tracking the row down the menu.
        Anchor::ToolbarMenu { first, count } => {
            let rows = tools_menu_rows_rect(app, first, count);
            let center = rows.y + rows.height / 2.0;
            (
                Horizontal::Right,
                Vertical::Top,
                Padding {
                    top: (center - 88.0).clamp(lo, hi),
                    right: menu_left + gap,
                    ..Padding::ZERO
                },
            )
        }
        // Under the tools, from where they start.
        Anchor::FileTools => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: body_top + SHEET_HEADER_ROW_H + gap,
                left: file_tools_rect(app)
                    .map_or(gap, |r| r.x)
                    .min(app.window_width - TUTORIAL_CARD_W - STRIP_W - gap),
                ..Padding::ZERO
            },
        ),
        // Beside the open menu.
        Anchor::ViewMenu => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: view_menu_top(app),
                left: (view_menu_left(app) + VIEW_MENU_W + gap)
                    .min(app.window_width - TUTORIAL_CARD_W - STRIP_W - gap),
                ..Padding::ZERO
            },
        ),
        Anchor::RightTop => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: body_top + 34.0,
                right: STRIP_W + SHEET_GAP + app.right_width + crate::resize::THICKNESS + gap,
                ..Padding::ZERO
            },
        ),
    };

    // Menu rows sit shoulder-to-shoulder, so their frame is drawn just INSIDE
    // the row; every other region frames just outside itself.
    let outline_pad = if matches!(step.anchor, Anchor::ToolbarMenu { .. }) {
        -2.0
    } else {
        2.5
    };
    let spotlight = iced::widget::canvas::Canvas::new(Spotlight {
        hole: region_rect(app, step.anchor),
        outline_pad,
    })
    .width(Fill)
    .height(Fill);

    let card_layer = container(opaque(card))
        .width(Fill)
        .height(Fill)
        .align_x(ax)
        .align_y(ay)
        .padding(pad);

    // Block clicks to the app while the tour runs; clicking the dimmed backdrop
    // does nothing (leaving is via Skip / Done), so a stray click can't drop it.
    opaque(mouse_area(stack![spotlight, card_layer]).on_press(Message::Noop))
}
