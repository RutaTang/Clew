//! The interactive-tutorial overlay: a spotlight that dims everything except the
//! region a step is about, plus a callout card placed next to it.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::app::tutorial::{Anchor, steps};

// The chrome heights, used to carve the body region out of the window. Fixed by
// the layout, so no widget measurement is needed.
const TOOLBAR_H: f32 = 46.0;
const STATUS_H: f32 = 27.0;

// The opened ⋯ dropdown geometry (see `ui::toolbar::tools_menu`): a fixed panel
// at the top right. Its rows share a uniform pitch, with one wider gap where the
// separator sits after the four toggle rows (row 3). Used both to spotlight a
// single row (or the toggles group) and to sit the callout right beside it.
const MENU_LEFT: f32 = 276.0; // the item column's left edge = ww - MENU_LEFT
const MENU_WIDTH: f32 = 216.0; // item column width
const MENU_C0: f32 = 60.0; // vertical centre of the first row's text
const MENU_PITCH: f32 = 28.0; // row-to-row distance
const MENU_ROW_H: f32 = 26.0; // highlight-box height, centred on the row's text
const MENU_SEP: f32 = 9.5; // extra gap before the first action row (row 4)

/// The vertical centre of menu row `i` (window-y), accounting for the one
/// separator after the four toggle rows. The highlight box is centred on this,
/// so the row's text sits in the middle of the box rather than crowding an edge.
fn menu_row_center(i: usize) -> f32 {
    MENU_C0 + i as f32 * MENU_PITCH + if i >= 4 { MENU_SEP } else { 0.0 }
}

/// The screen rectangle a step points at, or `None` for a full-window dim (the
/// welcome / summary steps, or a step whose panel happens to be closed).
fn region_rect(app: &App, anchor: Anchor) -> Option<iced::Rectangle> {
    let ww = app.window_width;
    let wh = app.window_height;
    let sidebar = if app.show_left_sidebar {
        app.sidebar_width
    } else {
        0.0
    };
    let right = if app.show_right_panel {
        app.right_width
    } else {
        0.0
    };
    let bottom = if app.show_bottom {
        app.bottom_height
    } else {
        0.0
    };
    let top = TOOLBAR_H;
    let bot = wh - STATUS_H;
    // The main row (sidebar / reader / right panel) ends above the bottom panel
    // when it is open, so the side regions must stop there rather than at `bot`.
    let main_bot = bot - bottom;
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
        // The top bar's left cluster (window controls · back/forward · breadcrumb).
        // Width is approximate — the breadcrumb grows with the path — but always
        // covers the controls without spilling into the right cluster.
        Anchor::ToolbarLeft => rect(0.0, 0.0, (ww - 440.0).clamp(220.0, 560.0), top),
        // A single tool icon in the right cluster. The cluster is right-aligned
        // with a fixed layout, so each icon sits a fixed distance in from the
        // window's right edge (the 7 core icons on a ~40px pitch, then the ⋯).
        Anchor::ToolbarIcon(i) => {
            let cx = ww - (356.0 - 40.3 * i as f32);
            rect(cx - 22.0, 0.0, 44.0, top)
        }
        Anchor::ToolbarMore => {
            let cx = ww - 64.0;
            rect(cx - 22.0, 0.0, 44.0, top)
        }
        // A row (or run of rows) in the opened ⋯ menu. The hole hugs the item
        // column exactly (no bleed past the panel edge) and is centred on the
        // row's text, so the inset outline frames it evenly without crowding the
        // text or catching a neighbour.
        Anchor::ToolbarMenu { first, count } => {
            let top = menu_row_center(first) - MENU_ROW_H / 2.0;
            let bottom = menu_row_center(first + count.max(1) - 1) + MENU_ROW_H / 2.0;
            rect(ww - MENU_LEFT, top, MENU_WIDTH, bottom - top)
        }
        Anchor::Sidebar if sidebar > 1.0 => rect(0.0, top, sidebar, main_bot - top),
        // The right panel is an equal top/bottom split: Explain over Outline.
        Anchor::RightTop if right > 1.0 => rect(ww - right, top, right, (main_bot - top) / 2.0),
        Anchor::RightBottom if right > 1.0 => {
            let h = (main_bot - top) / 2.0;
            rect(ww - right, top + h, right, h)
        }
        Anchor::Main => rect(sidebar, top, ww - sidebar - right, main_bot - top),
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
        let dim = iced::Color {
            a: 0.6,
            ..iced::Color::BLACK
        };
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
        return space().into();
    };
    let script = steps(app);
    let total = script.len();
    let Some(step) = script.get(step_i) else {
        return space().into();
    };

    // The callout card: progress · title · body · controls.
    let progress = text(format!("Step {} of {}", step_i + 1, total))
        .size(11)
        .color(theme::accent());
    let title = text(step.title.clone()).size(17).color(theme::fg());
    let body = text(step.body.clone())
        .size(13)
        .color(theme::fg_muted())
        .wrapping(Wrapping::Word);

    let back: Element<'_, Message> = if step_i > 0 {
        button(text("Back").size(12))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press(Message::TutorialStep(-1))
            .into()
    } else {
        space().into()
    };
    let last = step_i + 1 == total;
    let next = button(text(if last { "Done" } else { "Next" }).size(12))
        .style(theme::primary_button)
        .padding([4, 16])
        .on_press(Message::TutorialStep(1));
    // "Skip tour" is the subtle exit, kept left and muted. Small horizontal
    // padding so its text lines up with the title/body above it, rather than
    // sitting indented by a wider button's internal padding.
    let skip = button(text("Skip tour").size(12).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([4, 6])
        .on_press(Message::TutorialExit);

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
    .width(440)
    .padding(18)
    .style(theme::modal_panel);

    // Place the card next to the region the step is about, using the known
    // layout sizes — no widget measurement needed since clew's layout is fixed.
    use iced::alignment::{Horizontal, Vertical};
    let gap = 24.0;
    let (ax, ay, pad) = match step.anchor {
        Anchor::Center | Anchor::Main => (Horizontal::Center, Vertical::Center, Padding::ZERO),
        Anchor::Sidebar => (
            Horizontal::Left,
            Vertical::Center,
            Padding {
                left: app.sidebar_width + gap,
                ..Padding::ZERO
            },
        ),
        Anchor::ToolbarLeft => (
            Horizontal::Left,
            Vertical::Top,
            Padding {
                top: 64.0,
                left: 24.0,
                ..Padding::ZERO
            },
        ),
        // The tool icons sit at the top right, so drop the card just below them
        // on the right, clear of the main area a live demo fills.
        Anchor::ToolbarIcon(_) => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: 64.0,
                right: 40.0,
                ..Padding::ZERO
            },
        ),
        // The ⋯ button and its opened menu sit at the top right. Put the callout
        // immediately to their left (its right edge a `gap` from the dropdown's
        // left edge) so the two read as one unit instead of being stranded across
        // the window. `304 = 280 (menu offset from the right) + gap`.
        Anchor::ToolbarMore => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: 60.0,
                right: 280.0 + gap,
                ..Padding::ZERO
            },
        ),
        // For a menu row, also drop the card so its middle is level with the row
        // it points at, tracking the row down the menu.
        Anchor::ToolbarMenu { first, count } => {
            let center = (menu_row_center(first) + menu_row_center(first + count.max(1) - 1)) / 2.0;
            let lo = TOOLBAR_H + 14.0;
            let top = (center - 88.0).clamp(lo, (app.window_height - 200.0).max(lo));
            (
                Horizontal::Right,
                Vertical::Top,
                Padding {
                    top,
                    right: 280.0 + gap,
                    ..Padding::ZERO
                },
            )
        }
        Anchor::RightTop => (
            Horizontal::Right,
            Vertical::Top,
            Padding {
                top: 72.0,
                right: app.right_width + gap,
                ..Padding::ZERO
            },
        ),
        Anchor::RightBottom => (
            Horizontal::Right,
            Vertical::Bottom,
            Padding {
                bottom: 72.0,
                right: app.right_width + gap,
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
