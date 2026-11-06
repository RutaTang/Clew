//! The settings' Voice section: which speech model listens — each one
//! downloaded only when the user asks for it, its size said first — the
//! language spoken, and what is done with the words.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use super::form;
use crate::VoiceMsg;
use crate::app::voice::LANGUAGES;
use clew_voice::models::{MODELS, Model};

/// A language as the picker lists it: its English name (`LANGUAGES`),
/// shown in the interface's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LanguageChoice(Option<&'static str>, &'static str);

impl std::fmt::Display for LanguageChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(crate::i18n::lookup(self.1))
    }
}

/// One model's row: picked or not (a radio's mark), what it is, and at
/// its right whether it is here — its download, or getting it.
fn model_row<'a>(app: &'a App, model: &'static Model) -> Element<'a, Message> {
    let picked = app.voice.prefs.model == model.id;
    let installed = app.voice.installed(model);
    let state: Element<'a, Message> = if let Some(d) = app.voice.downloads.get(model.id) {
        let total = d
            .total
            .unwrap_or(u64::from(model.size_mb) * 1_000_000)
            .max(1);
        let share = (d.done as f32 / total as f32).clamp(0.0, 1.0);
        row![
            container(super::meter::eased_bar(
                share,
                70.0,
                4.0,
                theme::progress,
                super::meter::PROGRESS,
            ))
            .width(70),
            text(tr!(
                "{percent}%",
                percent = crate::i18n::decimal(f64::from(share) * 100.0, 0)
            ))
            .size(ts::SMALL)
            .color(theme::dim()),
            form::action(
                tr!("Cancel"),
                Some(Message::Voice(VoiceMsg::CancelDownload(model.id)))
            ),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into()
    } else if installed {
        row![
            text(tr!("Downloaded"))
                .size(ts::SMALL)
                .color(theme::success()),
            form::action(
                tr!("Remove"),
                Some(Message::Voice(VoiceMsg::RemoveModel(model.id)))
            ),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into()
    } else {
        let label = tr!("Download ({size} MB)", size = model.size_mb);
        let msg = Some(Message::Voice(VoiceMsg::Download(model.id)));
        if picked {
            form::primary(&label, msg).into()
        } else {
            form::action(&label, msg).into()
        }
    };
    let detail = column![
        text(crate::i18n::lookup(model.about))
            .size(ts::SMALL)
            .color(theme::dim())
            .wrapping(Wrapping::WordOrGlyph),
    ];
    let mut rows = column![form::choice_with(
        crate::i18n::lookup(model.name),
        Some(detail.into()),
        picked,
        Message::Voice(VoiceMsg::PickModel(model.id)),
        Some(state),
    )];
    if let Some(e) = app.voice.download_errors.get(model.id) {
        rows = rows.push(form::plain(
            text(crate::i18n::message(e))
                .size(ts::SMALL)
                .color(theme::warn())
                .wrapping(Wrapping::WordOrGlyph),
        ));
    }
    rows.into()
}

/// The settings' Voice section: which model listens, and how what it hears
/// is used. Its changes take effect, and are saved, at once.
pub(crate) fn voice_section(app: &App) -> Element<'_, Message> {
    let chord = app.keymap.chord(crate::keymap::Action::Voice).caps();
    let models = MODELS.iter().map(|model| model_row(app, model)).collect();
    let mut listening = Vec::new();
    if !app.voice.prefs.model().english_only {
        let choices: Vec<LanguageChoice> = LANGUAGES
            .iter()
            .map(|(code, name)| LanguageChoice(*code, name))
            .collect();
        let selected = choices
            .iter()
            .find(|c| c.0 == app.voice.prefs.language.as_deref())
            .copied();
        listening.push(form::item(
            tr!("Language"),
            None,
            form::pick(choices, selected, |c: LanguageChoice| {
                Message::Voice(VoiceMsg::SetLanguage(c.0))
            }),
        ));
    }
    listening.push(form::toggle(
        tr!("Run spoken commands"),
        None,
        app.voice.prefs.run_commands,
        |v| Message::Voice(VoiceMsg::SetRunCommands(v)),
    ));
    listening.push(form::toggle(
        tr!("Send spoken questions to Ask right away"),
        None,
        app.voice.prefs.send_questions,
        |v| Message::Voice(VoiceMsg::SetSendQuestions(v)),
    ));
    let mut parts = vec![
        form::note(tr!(
            "Press {chord} and speak. Speech stays on this computer.",
            chord = chord
        ))
        .into(),
        form::group(tr!("Model"), None, None, models),
        form::group(tr!("Listening"), None, None, listening),
    ];
    if let Some(store) = &app.voice.store {
        parts.push(
            form::note(tr!(
                "Models are stored in {path}",
                path = store.dir().display()
            ))
            .into(),
        );
    }
    form::page(parts)
}
