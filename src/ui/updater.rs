//! Auto-update UI: the inline "update available" banner (with download
//! progress) and the release-notes modal.

use iced::widget::{
    Row, button, column, container, mouse_area, opaque, progress_bar, row, scrollable, space, text,
};
use iced::{Element, Fill, Length};

use super::human_size;
use crate::{App, Message, UpdatePhase, theme};

/// The inline banner shown below the toolbar while an update is available or in
/// progress. `None` when there is nothing to show.
pub(crate) fn update_banner(app: &App) -> Option<Element<'_, Message>> {
    let update = app.update.available.as_ref()?;
    let version = update.version;

    let bar: Row<'_, Message> = match &app.update.phase {
        UpdatePhase::Idle => row![
            dot(),
            text(format!("clew {version} is available"))
                .size(12)
                .color(theme::fg_muted())
                .width(Fill),
            action_button("What's new", Message::UpdateShowNotes, false),
            action_button("Update now", Message::UpdateInstallStart, true),
            dismiss_button(),
        ],
        UpdatePhase::Downloading => {
            let (done, total) = app.update.progress.unwrap_or((0, None));
            let label = match total {
                Some(t) if t > 0 => format!(
                    "Downloading clew {version}…  {} / {}",
                    human_size(done),
                    human_size(t)
                ),
                _ => format!("Downloading clew {version}…  {}", human_size(done)),
            };
            let meter: Element<'_, Message> = match total {
                Some(t) if t > 0 => progress_bar(0.0..=t as f32, done as f32)
                    .length(160.0)
                    .girth(4.0)
                    .style(theme::progress)
                    .into(),
                _ => space().width(0.0).into(),
            };
            row![
                dot(),
                text(label).size(12).color(theme::fg_muted()),
                space().width(Fill),
                meter,
            ]
        }
        UpdatePhase::Installing => row![
            dot(),
            text("Installing update… clew will relaunch")
                .size(12)
                .color(theme::fg_muted())
                .width(Fill),
        ],
        UpdatePhase::Failed(e) => row![
            text("⚠").size(12).color(theme::warn()),
            text(format!("Update failed: {e}"))
                .size(12)
                .color(theme::fg_muted())
                .width(Fill),
            action_button("Retry", Message::UpdateInstallStart, true),
            dismiss_button(),
        ],
    };

    Some(
        container(bar.spacing(10).align_y(iced::Center))
            .width(Fill)
            .padding([5, 12])
            .style(theme::panel)
            .into(),
    )
}

/// The scrolling release-notes modal, opened from "What's new".
pub(crate) fn update_notes_modal(app: &App) -> Element<'_, Message> {
    let Some(update) = app.update.available.as_ref() else {
        return space().width(0.0).into();
    };
    let version = update.version;

    let notes: Element<'_, Message> = if update.notes.is_empty() {
        text("No release notes.")
            .size(13)
            .color(theme::fg_muted())
            .into()
    } else {
        iced::widget::markdown::view(&update.notes, theme::markdown_settings())
            .map(|url| Message::OpenLink(url.to_string()))
    };
    let notes = scrollable(container(notes).padding([0, 6]).max_width(520))
        .width(Fill)
        .height(Fill)
        .style(theme::overlay_scrollbar);

    // While downloading / installing the button is disabled (no `on_press`).
    let busy = matches!(
        app.update.phase,
        UpdatePhase::Downloading | UpdatePhase::Installing
    );
    let update_now = {
        let b = button(text("Update now").size(13))
            .style(theme::primary_button)
            .padding([6, 16]);
        if busy {
            b
        } else {
            b.on_press(Message::UpdateInstallStart)
        }
    };

    let panel = container(
        column![
            text(format!("clew {version}"))
                .size(17)
                .color(theme::fg_bright()),
            text("What's new").size(12).color(theme::fg_muted()),
            container(notes).height(Length::Fixed(360.0)),
            row![
                space().width(Fill),
                button(text("Later").size(13))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::UpdateCloseNotes),
                update_now,
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(12),
    )
    .width(560)
    .padding(22)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .style(theme::backdrop);
    opaque(mouse_area(positioned).on_press(Message::UpdateCloseNotes))
}

/// The accent leading marker shared by the banner rows.
fn dot() -> Element<'static, Message> {
    text("›").size(13).color(theme::accent()).into()
}

/// A small banner action button (primary = accent fill).
fn action_button(label: &'static str, msg: Message, primary: bool) -> Element<'static, Message> {
    let b = button(text(label).size(12)).padding([4, 12]).on_press(msg);
    if primary {
        b.style(theme::primary_button).into()
    } else {
        b.style(theme::toolbar_button).into()
    }
}

/// The banner's dismiss (✕) button.
fn dismiss_button() -> Element<'static, Message> {
    button(text("✕").size(11).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([2, 6])
        .on_press(Message::UpdateBannerDismissed)
        .into()
}
