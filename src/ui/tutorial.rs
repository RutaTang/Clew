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

/// The screen rectangle a step points at, or `None` for a full-window dim (the
/// welcome / summary steps, or a step whose panel happens to be closed).
fn region_rect(app: &App, anchor: Anchor) -> Option<iced::Rectangle> {
    let ww = app.window_width;
    let wh = app.window_height;
    let sidebar = if app.show_left_sidebar { app.sidebar_width } else { 0.0 };
    let right = if app.show_right_panel { app.right_width } else { 0.0 };
    let bottom = if app.show_bottom { app.bottom_height } else { 0.0 };
    let top = TOOLBAR_H;
    let bot = wh - STATUS_H;
    let rect = |x: f32, y: f32, w: f32, h: f32| Some(iced::Rectangle { x, y, width: w, height: h });
    match anchor {
        Anchor::Center => None,
        Anchor::Toolbar => rect(0.0, 0.0, ww, top),
        Anchor::Sidebar if sidebar > 1.0 => rect(0.0, top, sidebar, bot - top),
        Anchor::RightPanel if right > 1.0 => rect(ww - right, top, right, bot - top),
        Anchor::Bottom if bottom > 1.0 => rect(sidebar, bot - bottom, ww - sidebar - right, bottom),
        Anchor::Main => rect(sidebar, top, ww - sidebar - right, (bot - top) - bottom),
        _ => None,
    }
}

/// Canvas that dims the window except `hole` (if any) and outlines it — the
/// spotlight. Drawn as four dim rectangles around the hole so the highlighted
/// region shows through at full brightness.
struct Spotlight {
    hole: Option<iced::Rectangle>,
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
        let dim = iced::Color { a: 0.6, ..iced::Color::BLACK };
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
                // Accent outline around the highlighted region.
                let inset = iced::Rectangle {
                    x: r.x + 1.0,
                    y: r.y + 1.0,
                    width: (r.width - 2.0).max(0.0),
                    height: (r.height - 2.0).max(0.0),
                };
                let outline = Path::rounded_rectangle(
                    pt(inset.x, inset.y),
                    sz(inset.width, inset.height),
                    6.0.into(),
                );
                frame.stroke(
                    &outline,
                    Stroke::default().with_color(theme::ACCENT).with_width(2.0),
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
        .color(theme::ACCENT);
    let title = text(step.title.clone()).size(17).color(theme::FG);
    let body = text(step.body.clone()).size(13).color(theme::FG_MUTED).wrapping(Wrapping::Word);

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
    let skip = button(text("Skip tour").size(12))
        .style(theme::toolbar_button)
        .padding([4, 10])
        .on_press(Message::TutorialExit);

    let card = container(
        column![
            progress,
            title,
            body,
            row![skip, space().width(Fill), back, next].spacing(8).align_y(iced::Center),
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
        Anchor::Center | Anchor::Main => {
            (Horizontal::Center, Vertical::Center, Padding::ZERO)
        }
        Anchor::Sidebar => (
            Horizontal::Left,
            Vertical::Center,
            Padding { left: app.sidebar_width + gap, ..Padding::ZERO },
        ),
        Anchor::Toolbar => (
            Horizontal::Right,
            Vertical::Top,
            Padding { top: 64.0, right: 40.0, ..Padding::ZERO },
        ),
        Anchor::RightPanel => (
            Horizontal::Right,
            Vertical::Center,
            Padding { right: app.right_width + gap, ..Padding::ZERO },
        ),
        Anchor::Bottom => (
            Horizontal::Left,
            Vertical::Bottom,
            Padding { bottom: app.bottom_height + gap, left: 40.0, ..Padding::ZERO },
        ),
    };

    let spotlight = iced::widget::canvas::Canvas::new(Spotlight {
        hole: region_rect(app, step.anchor),
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
