//! Auto-update UI: the inline "update available" banner (with download
//! progress, and Cancel) and the release-notes modal.

use iced::widget::{Row, button, column, container, progress_bar, row, scrollable, space, text};
use iced::{Element, Fill, Length};

use super::{Backdrop, CONFIRM_PAD, DIALOG_W, NOTES_MAX_W, Placement, human_size, modal, slot, ts};
use crate::{App, Message, UpdatePhase, theme};
use crate::{ContentMsg, UpdaterMsg};

/// Height of the update banner. Fixed, so the regions below it (and the
/// tutorial's spotlight over them) know how far down the body starts.
pub(crate) const UPDATE_BANNER_H: f32 = 34.0;

/// How much the update banner pushes the body down right now: its height while
/// it shows, else nothing.
pub(crate) fn update_banner_height(app: &App) -> f32 {
    if app.update.available.is_some() {
        UPDATE_BANNER_H
    } else {
        0.0
    }
}

/// The inline banner shown below the toolbar while an update is available or in
/// progress. `None` when there is nothing to show (keep in step with
/// [`update_banner_height`]).
pub(crate) fn update_banner(app: &App) -> Option<Element<'_, Message>> {
    let update = app.update.available.as_ref()?;
    let version = update.version;

    let bar: Row<'_, Message> = match &app.update.phase {
        UpdatePhase::Idle => row![
            dot(),
            text(format!("clew {version} is available"))
                .size(ts::BODY)
                .color(theme::fg_muted())
                .width(Fill),
            action_button("What's new", Message::Updater(UpdaterMsg::ShowNotes), false),
            action_button(
                "Update now",
                Message::Updater(UpdaterMsg::InstallStart),
                true
            ),
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
                _ => slot(),
            };
            row![
                dot(),
                text(label).size(ts::BODY).color(theme::fg_muted()),
                space().width(Fill),
                meter,
                action_button(
                    "Cancel",
                    Message::Updater(UpdaterMsg::CancelDownload),
                    false
                ),
            ]
        }
        UpdatePhase::Installing => row![
            dot(),
            text("Installing update… clew will relaunch")
                .size(ts::BODY)
                .color(theme::fg_muted())
                .width(Fill),
        ],
        UpdatePhase::Failed(e) => row![
            text("⚠").size(ts::BODY).color(theme::warn()),
            text(format!("Update failed: {e}"))
                .size(ts::BODY)
                .color(theme::fg_muted())
                .width(Fill),
            action_button("Retry", Message::Updater(UpdaterMsg::InstallStart), true),
            dismiss_button(),
        ],
    };

    Some(
        container(bar.spacing(10).align_y(iced::Center))
            .width(Fill)
            .height(Length::Fixed(UPDATE_BANNER_H))
            .align_y(iced::Center)
            .padding([0, 12])
            .style(theme::panel)
            .into(),
    )
}

/// The scrolling release-notes modal, opened from "What's new".
pub(crate) fn update_notes_modal(app: &App) -> Element<'_, Message> {
    let Some(update) = app.update.available.as_ref() else {
        return slot();
    };
    let version = update.version;

    let notes: Element<'_, Message> = if update.notes.is_empty() {
        text("No release notes.")
            .size(ts::BASE)
            .color(theme::fg_muted())
            .into()
    } else {
        iced::widget::markdown::view(&update.notes, theme::markdown_settings())
            .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string())))
    };
    let notes = scrollable(container(notes).padding([0, 6]).max_width(NOTES_MAX_W))
        .width(Fill)
        .height(Fill)
        .style(theme::overlay_scrollbar);

    // While downloading / installing the button is disabled (no `on_press`).
    let busy = matches!(
        app.update.phase,
        UpdatePhase::Downloading | UpdatePhase::Installing
    );
    let update_now = {
        let b = button(text("Update now").size(ts::BASE))
            .style(theme::primary_button)
            .padding([6, 16]);
        if busy {
            b
        } else {
            b.on_press(Message::Updater(UpdaterMsg::InstallStart))
        }
    };

    let panel = container(
        column![
            text(format!("clew {version}"))
                .size(ts::TITLE)
                .color(theme::fg_bright()),
            text("What's new").size(ts::BODY).color(theme::fg_muted()),
            container(notes).height(Length::Fixed(360.0)),
            row![
                space().width(Fill),
                button(text("Later").size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::Updater(UpdaterMsg::CloseNotes)),
                update_now,
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(12),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    modal(
        panel,
        Placement::Center,
        Backdrop::Dim(Some(Message::Updater(UpdaterMsg::CloseNotes))),
    )
}

/// The accent leading marker shared by the banner rows.
fn dot() -> Element<'static, Message> {
    text("›").size(ts::BASE).color(theme::accent()).into()
}

/// A small banner action button (primary = accent fill).
fn action_button(label: &'static str, msg: Message, primary: bool) -> Element<'static, Message> {
    let b = button(text(label).size(ts::BODY))
        .padding([4, 12])
        .on_press(msg);
    if primary {
        b.style(theme::primary_button).into()
    } else {
        b.style(theme::toolbar_button).into()
    }
}

/// The banner's dismiss (✕) button.
fn dismiss_button() -> Element<'static, Message> {
    button(text("✕").size(ts::SMALL).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([2, 6])
        .on_press(Message::Updater(UpdaterMsg::BannerDismissed))
        .into()
}
