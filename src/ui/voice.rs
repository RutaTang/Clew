//! The settings' Voice section: which speech model listens — each one
//! downloaded only when the user asks for it, its size said first — the
//! language spoken, and what is done with the words.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::VoiceMsg;
use crate::app::voice::LANGUAGES;
use clew_voice::models::{MODELS, Model};

/// A language as the picker lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LanguageChoice(Option<&'static str>, &'static str);

impl std::fmt::Display for LanguageChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}

/// One model's row: picked or not, what it is, and whether it is here.
fn model_row<'a>(app: &'a App, model: &'static Model) -> Element<'a, Message> {
    let picked = app.voice.prefs.model == model.id;
    let installed = app.voice.installed(model);
    let pick = button(
        column![
            row![
                text(if picked { "●" } else { "○" })
                    .size(ts::SMALL)
                    .color(if picked {
                        theme::accent()
                    } else {
                        theme::dim()
                    }),
                text(model.name).size(ts::BODY).color(theme::fg()),
                text(format!("{} MB", model.size_mb))
                    .size(ts::CAPTION)
                    .color(theme::dim()),
            ]
            .spacing(6)
            .align_y(iced::Center),
            text(model.about).size(ts::CAPTION).color(theme::dim()),
        ]
        .spacing(2),
    )
    .style(theme::list_row(picked))
    .padding([4, 6])
    .width(Fill)
    .on_press(Message::Voice(VoiceMsg::PickModel(model.id)));
    let small = |label: String, msg: Message, primary: bool| {
        button(text(label).size(ts::SMALL))
            .style(if primary {
                theme::primary_button
            } else {
                theme::secondary_button
            })
            .padding([2, 8])
            .on_press(msg)
    };
    let state: Element<'a, Message> = if let Some(d) = app.voice.downloads.get(model.id) {
        let total = d
            .total
            .unwrap_or(u64::from(model.size_mb) * 1_000_000)
            .max(1);
        let share = (d.done as f32 / total as f32).clamp(0.0, 1.0);
        row![
            container(
                progress_bar(0.0..=1.0, share)
                    .length(70.0)
                    .girth(4.0)
                    .style(theme::progress)
            )
            .width(70),
            text(format!("{:.0}%", share * 100.0))
                .size(ts::CAPTION)
                .color(theme::dim()),
            small(
                "Cancel".into(),
                Message::Voice(VoiceMsg::CancelDownload(model.id)),
                false
            ),
        ]
        .spacing(6)
        .align_y(iced::Center)
        .into()
    } else if installed {
        row![
            text("Downloaded").size(ts::CAPTION).color(theme::success()),
            small(
                "Remove".into(),
                Message::Voice(VoiceMsg::RemoveModel(model.id)),
                false
            ),
        ]
        .spacing(6)
        .align_y(iced::Center)
        .into()
    } else {
        small(
            format!("Download ({} MB)", model.size_mb),
            Message::Voice(VoiceMsg::Download(model.id)),
            picked,
        )
        .into()
    };
    let mut rows = column![row![pick, state].spacing(8).align_y(iced::Center)].spacing(2);
    if let Some(e) = app.voice.download_errors.get(model.id) {
        rows = rows.push(
            text(e.clone())
                .size(ts::CAPTION)
                .color(theme::warn())
                .wrapping(Wrapping::WordOrGlyph),
        );
    }
    rows.into()
}

/// The settings' Voice section. Its changes take effect, and are saved, at
/// once, like the integrations' below it.
pub(crate) fn voice_section(app: &App) -> Element<'_, Message> {
    let chord = app.keymap.chord(crate::keymap::Action::Voice).caps();
    let mut col = column![
        text(format!(
            "Press {chord} and speak. Speech stays on this computer."
        ))
        .size(ts::CAPTION)
        .color(theme::dim()),
    ]
    .spacing(6);
    for model in MODELS {
        col = col.push(model_row(app, model));
    }
    if !app.voice.prefs.model().english_only {
        let choices: Vec<LanguageChoice> = LANGUAGES
            .iter()
            .map(|(code, name)| LanguageChoice(*code, name))
            .collect();
        let selected = choices
            .iter()
            .find(|c| c.0 == app.voice.prefs.language.as_deref())
            .copied();
        col = col.push(
            row![
                text("Language").size(ts::SMALL).color(theme::fg_muted()),
                pick_list(choices, selected, |c: LanguageChoice| {
                    Message::Voice(VoiceMsg::SetLanguage(c.0))
                })
                .text_size(ts::SMALL)
                .padding([3, 8]),
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
    }
    let check = |label: &'static str, on: bool, msg: fn(bool) -> VoiceMsg| {
        iced::widget::checkbox(on)
            .label(label)
            .on_toggle(move |v| Message::Voice(msg(v)))
            .text_size(ts::BODY)
            .size(16)
            .spacing(8)
    };
    col = col
        .push(check(
            "Run commands it hears",
            app.voice.prefs.run_commands,
            VoiceMsg::SetRunCommands,
        ))
        .push(check(
            "Ask questions as soon as they are heard",
            app.voice.prefs.send_questions,
            VoiceMsg::SetSendQuestions,
        ));
    if let Some(store) = &app.voice.store {
        col = col.push(
            text(format!("Models are kept in {}", store.dir().display()))
                .size(ts::CAPTION)
                .color(theme::dim())
                .wrapping(Wrapping::Glyph),
        );
    }
    col.into()
}
