//! The interactive-tutorial overlay: a callout card that steps through clew's
//! features, positioned next to the region each step describes.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::app::tutorial::{Anchor, steps};

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

    // Place the card next to the region the step is about, using the (known)
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

    let positioned = container(opaque(card))
        .width(Fill)
        .height(Fill)
        .align_x(ax)
        .align_y(ay)
        .padding(pad)
        .style(theme::backdrop);

    // Block clicks to the app while the tour runs; clicking the dimmed backdrop
    // does nothing (leaving is via Skip / Done), so a stray click can't drop it.
    opaque(mouse_area(positioned).on_press(Message::Noop))
}
