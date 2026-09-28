//! Shared chrome: the layout metrics the views agree on, the one modal frame
//! every dialog and popover is built with, and the scrollbar geometry.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Thickness of every scrollbar (always paired with `theme::overlay_scrollbar`).
pub(crate) const SCROLLBAR_W: f32 = 6.0;
/// Thickness of the horizontal bar under code blocks and notebook outputs.
pub(crate) const SCROLLBAR_W_THIN: f32 = 4.0;

/// Space kept between a centered modal and the window edges.
pub(crate) const MODAL_MARGIN: f32 = 40.0;
/// Inner padding of a dialog panel.
pub(crate) const MODAL_PAD: f32 = 20.0;
/// Inner padding of a compact prompt panel (single-input dialogs).
pub(crate) const PROMPT_PAD: f32 = 16.0;
/// Inner padding of a confirmation panel (consent prompts, release notes).
pub(crate) const CONFIRM_PAD: f32 = 22.0;
/// Top offset of the prompts that sit near the top of the window.
pub(crate) const PROMPT_TOP: f32 = 120.0;
/// Top offset of the "why is this here?" popup.
pub(crate) const WHY_TOP: f32 = 110.0;
/// Top offset of the finder.
pub(crate) const FINDER_TOP: f32 = 80.0;

/// Dialog widths, narrowest first.
pub(crate) const PROMPT_W: f32 = 460.0;
pub(crate) const SETTINGS_W: f32 = 480.0;
pub(crate) const WHY_W: f32 = 480.0;
pub(crate) const SHORTCUTS_W: f32 = 540.0;
pub(crate) const DIALOG_W: f32 = 560.0;
pub(crate) const FINDER_W: f32 = 640.0;
pub(crate) const SERVER_PANEL_W: f32 = 720.0;
pub(crate) const GRAPH_MODAL_W: f32 = 760.0;
/// Width of the reading-target dropdown.
pub(crate) const TARGET_MENU_W: f32 = 172.0;
/// Width of the tutorial's callout card.
pub(crate) const TUTORIAL_CARD_W: f32 = 440.0;

/// Tallest each dialog grows before its body scrolls, beside the widths above.
pub(crate) const WHY_MAX_H: f32 = 440.0;
pub(crate) const FINDER_MAX_H: f32 = 520.0;
pub(crate) const SHORTCUTS_MAX_H: f32 = 600.0;
pub(crate) const SERVER_PANEL_MAX_H: f32 = 600.0;
pub(crate) const DIALOG_MAX_H: f32 = 620.0;
pub(crate) const GRAPH_MODAL_MAX_H: f32 = 640.0;

/// The hover peek: widest and tallest before its text wraps / scrolls.
pub(crate) const PEEK_MAX_W: f32 = 560.0;
pub(crate) const PEEK_MAX_H: f32 = 320.0;
/// Widest a small explanatory tooltip grows before it wraps.
pub(crate) const TIP_MAX_W: f32 = 320.0;
/// Widest the release notes' text runs before it wraps.
pub(crate) const NOTES_MAX_W: f32 = 520.0;
/// Height of the remote folder picker's listing.
pub(crate) const REMOTE_LIST_H: f32 = 300.0;
/// Height of the overview home's embedded module map.
pub(crate) const OVERVIEW_MAP_H: f32 = 320.0;

/// Where a modal's panel sits in the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Placement {
    /// Centered both ways, kept [`MODAL_MARGIN`] clear of the edges.
    Center,
    /// Horizontally centered, this far below the window's top edge.
    Top(f32),
    /// Pinned under a control in the top-right corner (a dropdown).
    TopRight { top: f32, right: f32 },
    /// Pinned above a control in the bottom-right corner (a drop-up).
    BottomRight { bottom: f32, right: f32 },
}

/// What surrounds a modal's panel.
#[derive(Debug, Clone)]
pub(crate) enum Backdrop {
    /// A dimmed veil. A click on it sends the message; `None` makes the choice
    /// required (the click does nothing — consent prompts).
    Dim(Option<Message>),
    /// No veil: a popover menu. A click anywhere outside it sends the message.
    Clear(Message),
}

/// Frame `panel` as a modal: positioned per `placement`, over `backdrop`.
///
/// Every dialog and every menu that owns the keyboard goes through here, so
/// they share one behaviour: the panel swallows its own clicks, the backdrop
/// dismisses (or deliberately does not), and the panel carries
/// [`modal_scope_id`] so Tab cycles focus among the modal's own fields rather
/// than the hidden ones behind it. Two overlays deliberately do not: the
/// code's right-click menu and the hover peek are anchored at the pointer
/// (`anchored::Anchored`), not placed, and neither holds a field to focus —
/// the menu has a click-catcher of its own that closes it, and the peek goes
/// when the pointer leaves it. Escape reaches all of them the same way
/// (`escape_message`).
pub(crate) fn modal<'a>(
    panel: impl Into<Element<'a, Message>>,
    placement: Placement,
    backdrop: Backdrop,
) -> Element<'a, Message> {
    let panel = container(opaque(panel)).id(modal_scope_id());
    let frame = container(panel).width(Fill).height(Fill);
    let frame = match placement {
        Placement::Center => frame
            .align_x(iced::Center)
            .align_y(iced::Center)
            .padding(MODAL_MARGIN),
        Placement::Top(top) => frame.align_x(iced::Center).padding(Padding {
            top,
            ..Padding::ZERO
        }),
        Placement::TopRight { top, right } => frame
            .align_x(iced::alignment::Horizontal::Right)
            .padding(Padding {
                top,
                right,
                ..Padding::ZERO
            }),
        Placement::BottomRight { bottom, right } => frame
            .align_x(iced::alignment::Horizontal::Right)
            .align_y(iced::alignment::Vertical::Bottom)
            .padding(Padding {
                bottom,
                right,
                ..Padding::ZERO
            }),
    };
    match backdrop {
        Backdrop::Dim(dismiss) => {
            let dimmed = frame.style(theme::backdrop);
            match dismiss {
                Some(msg) => opaque(mouse_area(dimmed).on_press(msg)),
                None => opaque(dimmed),
            }
        }
        Backdrop::Clear(dismiss) => opaque(mouse_area(frame).on_press(dismiss)),
    }
}

/// The fields of a single-line prompt dialog (breakpoint condition, bookmark
/// note, reading note): a title, a hint, one input, Cancel and a submit.
pub(crate) struct Prompt<'a> {
    pub(crate) title: String,
    pub(crate) hint: &'a str,
    pub(crate) placeholder: &'a str,
    pub(crate) value: &'a str,
    pub(crate) input_id: iced::widget::Id,
    pub(crate) on_input: fn(String) -> Message,
    pub(crate) submit: Message,
    pub(crate) submit_label: &'a str,
    pub(crate) cancel: Message,
}

/// Build a [`Prompt`] dialog. Enter submits, Esc / a backdrop click cancels.
pub(crate) fn prompt_modal(p: Prompt<'_>) -> Element<'_, Message> {
    let panel = container(
        column![
            text(p.title).size(ts::EMPHASIS).color(theme::fg()),
            text(p.hint).size(ts::SMALL).color(theme::dim()),
            text_input(p.placeholder, p.value)
                .id(p.input_id)
                .on_input(p.on_input)
                .on_submit(p.submit.clone())
                .size(ts::BASE)
                .padding(8),
            row![
                space().width(Fill),
                button(text("Cancel").size(ts::BODY))
                    .style(theme::toolbar_button)
                    .padding([4, 12])
                    .on_press(p.cancel.clone()),
                button(text(p.submit_label).size(ts::BODY))
                    .style(theme::primary_button)
                    .padding([4, 12])
                    .on_press(p.submit),
            ]
            .spacing(8),
        ]
        .spacing(10),
    )
    .width(PROMPT_W)
    .padding(PROMPT_PAD)
    .style(theme::modal_panel);
    modal(
        panel,
        Placement::Top(PROMPT_TOP),
        Backdrop::Dim(Some(p.cancel)),
    )
}

/// A thin vertical scrollbar geometry, paired with [`theme::overlay_scrollbar`]
/// so panels get a slim, auto-hiding bar instead of the chunky default.
pub(crate) fn thin_scroll() -> Direction {
    Direction::Vertical(
        Scrollbar::new()
            .width(SCROLLBAR_W)
            .scroller_width(SCROLLBAR_W),
    )
}

/// Both scrollbars, for content that can outgrow the pane either way (code,
/// diffs, the reading trail).
pub(crate) fn both_scroll() -> Direction {
    Direction::Both {
        vertical: Scrollbar::new()
            .width(SCROLLBAR_W)
            .scroller_width(SCROLLBAR_W),
        horizontal: Scrollbar::new()
            .width(SCROLLBAR_W)
            .scroller_width(SCROLLBAR_W),
    }
}

/// A slim horizontal-only bar, for code blocks that scroll sideways inside a
/// vertically scrolling page.
pub(crate) fn sideways_scroll() -> Direction {
    Direction::Horizontal(
        Scrollbar::new()
            .width(SCROLLBAR_W_THIN)
            .scroller_width(SCROLLBAR_W_THIN),
    )
}

/// An empty slot: a zero-size placeholder that keeps a container's child
/// positions stable while the thing it stands for is hidden (see `ui::view`).
pub(crate) fn slot<'a>() -> Element<'a, Message> {
    space().into()
}
