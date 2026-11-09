//! The settings' Voice section: which speech model listens — each one
//! downloaded only when the user asks for it, its size said first — the
//! language spoken, and what is done with the words; and how Clew speaks
//! its answers back: the voice models, the voice, the pace.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use super::form;
use crate::VoiceMsg;
use crate::app::speech::SPEEDS;
use crate::app::voice::LANGUAGES;
use clew_voice::models::{MODELS, Model};
use clew_voice::speech::{VOICE_MODELS, Voice, VoiceModel};

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
    // Its Download, Remove and Cancel said with its name, as every model
    // has them: "Download Whisper Small".
    super::heard::about(crate::i18n::lookup(model.name), || model_row_of(app, model))
}

fn model_row_of<'a>(app: &'a App, model: &'static Model) -> Element<'a, Message> {
    let picked = app.voice.prefs.model == model.id;
    let installed = app.voice.installed(model);
    let state: Element<'a, Message> = if let Some(d) = app.voice.downloads.get(model.id) {
        let total = d
            .total
            .unwrap_or(u64::from(model.size_mb) * 1_000_000)
            .max(1);
        getting(
            d.done,
            total,
            Message::Voice(VoiceMsg::CancelDownload(model.id)),
        )
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
    let mut detail = column![
        text(crate::i18n::lookup(model.about))
            .size(ts::SMALL)
            .color(theme::dim())
            .wrapping(Wrapping::WordOrGlyph),
    ];
    // The one chosen and not here yet — the first one chosen, Small, is
    // not: chosen, it says what being chosen does until it is here.
    if picked && !installed && !app.voice.downloads.contains_key(model.id) {
        detail = detail.push(
            text(tr!("Voice uses this model once it is downloaded."))
                .size(ts::SMALL)
                .color(theme::accent())
                .wrapping(Wrapping::WordOrGlyph),
        );
    }
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

/// A voice of a speech model as the picker lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VoiceChoice {
    model: &'static str,
    voice: Voice,
}

impl std::fmt::Display for VoiceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&voice_name(&self.voice))
    }
}

/// What a voice is called: Supertone's F1…M5 as a woman's or a man's
/// voice and its number; a named one by its name.
fn voice_name(voice: &Voice) -> String {
    crate::app::choices::voice_called(crate::i18n::current(), voice.name)
}

/// A pace as the picker lists it: "0.8×", "1×", "1.25×".
#[derive(Debug, Clone, Copy, PartialEq)]
struct SpeedChoice(f32);

impl std::fmt::Display for SpeedChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hundredths = (self.0 * 100.0).round() as i32;
        let places = if hundredths % 10 != 0 {
            2
        } else if hundredths % 100 != 0 {
            1
        } else {
            0
        };
        let pace = crate::i18n::decimal(f64::from(self.0), places);
        if hundredths == 100 {
            f.write_str(&tr!("{pace}× (natural)", pace = pace))
        } else {
            write!(f, "{pace}×")
        }
    }
}

/// One speech model's row: what it is and its license, and at its right
/// whether it is here — Try and Remove — its download, or getting it.
fn speech_row<'a>(app: &'a App, model: &'static VoiceModel) -> Vec<Element<'a, Message>> {
    super::heard::about(crate::i18n::lookup(model.name), || {
        speech_row_of(app, model)
    })
}

fn speech_row_of<'a>(app: &'a App, model: &'static VoiceModel) -> Vec<Element<'a, Message>> {
    let installed = app.speech_installed(model);
    let state: Element<'a, Message> = if let Some(d) = app.voice.speech.downloads.get(model.id) {
        getting(
            d.done,
            d.total.unwrap_or(model.bytes()),
            Message::Voice(VoiceMsg::CancelSpeechDownload(model.id)),
        )
    } else if installed {
        row![
            form::action(
                tr!("Try"),
                Some(Message::Voice(VoiceMsg::TrySpeech(model.id)))
            ),
            form::action(
                tr!("Remove"),
                Some(Message::Voice(VoiceMsg::RemoveSpeech(model.id)))
            ),
        ]
        .spacing(8)
        .align_y(iced::Center)
        .into()
    } else {
        let label = tr!("Download ({size} MB)", size = model.size_mb());
        let msg = Some(Message::Voice(VoiceMsg::DownloadSpeech(model.id)));
        // The voice of every language but one: the first to get.
        if model.speaks("en") {
            form::primary(&label, msg).into()
        } else {
            form::action(&label, msg).into()
        }
    };
    let mut rows = vec![form::item(
        crate::i18n::lookup(model.name),
        Some(crate::i18n::lookup(model.about).to_string()),
        state,
    )];
    if let Some(e) = app.voice.speech.download_errors.get(model.id) {
        rows.push(form::plain(
            text(crate::i18n::message(e))
                .size(ts::SMALL)
                .color(theme::warn())
                .wrapping(Wrapping::WordOrGlyph),
        ));
    }
    if installed && model.voices.len() > 1 {
        let choices: Vec<VoiceChoice> = model
            .voices
            .iter()
            .map(|voice| VoiceChoice {
                model: model.id,
                voice: *voice,
            })
            .collect();
        let picked = app.voice.prefs.voice_for(model);
        let selected = choices.iter().find(|c| c.voice.id == picked).copied();
        rows.push(form::item(
            tr!("Voice"),
            None,
            form::pick(choices, selected, |c: VoiceChoice| {
                Message::Voice(VoiceMsg::PickSpeechVoice {
                    model: c.model,
                    voice: c.voice.id,
                })
            }),
        ));
    }
    rows
}

/// A download under way: how far it is, and its Cancel. The share is said
/// in a slot as wide as "100%" reads, so its bar does not move as it grows.
pub(crate) fn getting<'a>(done: u64, total: u64, cancel: Message) -> Element<'a, Message> {
    let share = (done as f32 / total.max(1) as f32).clamp(0.0, 1.0);
    let percent = |share: f64| tr!("{percent}%", percent = crate::i18n::decimal(share, 0));
    let said = percent(f64::from(share) * 100.0);
    row![
        container(super::meter::eased_bar(
            share,
            70.0,
            4.0,
            theme::progress,
            super::meter::PROGRESS,
        ))
        .width(70),
        super::steady::slot(
            text(said.clone()).size(ts::SMALL).color(theme::dim()),
            &said,
            &[&percent(100.0)],
            ts::SMALL,
        ),
        form::action(tr!("Cancel"), Some(cancel)),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .into()
}

/// Smart Turn's rows: what it does and its license, and at its right
/// whether it is here — Remove — its download, or getting it; and what
/// last went wrong with it.
fn turn_rows(app: &App) -> Vec<Element<'_, Message>> {
    super::heard::about(clew_voice::turn::MODEL.name, || turn_rows_of(app))
}

fn turn_rows_of(app: &App) -> Vec<Element<'_, Message>> {
    use clew_voice::turn::{self as smart, MODEL};
    let installed = app.voice.store.as_ref().is_some_and(smart::installed);
    let state: Element<'_, Message> = if let Some(d) = &app.voice.turn.download {
        getting(
            d.done,
            d.total.unwrap_or(MODEL.bytes),
            Message::Voice(VoiceMsg::CancelTurnDownload),
        )
    } else if installed {
        form::action(tr!("Remove"), Some(Message::Voice(VoiceMsg::RemoveTurn))).into()
    } else {
        form::action(
            &tr!("Download ({size} MB)", size = MODEL.size_mb()),
            Some(Message::Voice(VoiceMsg::DownloadTurn)),
        )
        .into()
    };
    let about = tr!(
        "Hears when you have finished speaking: a finished sentence is acted on at once, and a pause to think is waited through."
    );
    let mut rows = vec![form::item(MODEL.name, Some(about.to_string()), state)];
    let warning = |said: String| {
        form::plain(
            text(said)
                .size(ts::SMALL)
                .color(theme::warn())
                .wrapping(Wrapping::WordOrGlyph),
        )
    };
    if installed && app.voice.turn.judges.is_none() {
        rows.push(warning(
            tr!("clew-speak, which runs it, is not installed beside Clew").into(),
        ));
    }
    if let Some(e) = &app.voice.turn.error {
        rows.push(warning(crate::i18n::message(e)));
    }
    rows
}

/// How Clew answers aloud: whether it does, how fast, and the models it
/// speaks with.
fn speaking_rows(app: &App) -> Vec<Element<'_, Message>> {
    let mut rows = vec![form::item(
        tr!("Read answers aloud"),
        Some(
            tr!(
                "Read on this computer: the answers to spoken questions only, or to every question."
            )
            .into(),
        ),
        form::pick(
            &crate::app::voice::ReadAloud::ALL[..],
            Some(app.voice.prefs.read_aloud),
            |read_aloud| Message::Voice(VoiceMsg::SetSpeak(read_aloud)),
        ),
    )];
    let speeds: Vec<SpeedChoice> = SPEEDS.iter().map(|s| SpeedChoice(*s)).collect();
    let selected = speeds
        .iter()
        .find(|s| (s.0 - app.voice.prefs.speed).abs() < 0.001)
        .copied();
    rows.push(form::item(
        tr!("Speed"),
        None,
        form::pick(speeds, selected, |s: SpeedChoice| {
            Message::Voice(VoiceMsg::SetSpeed(s.0))
        }),
    ));
    for model in VOICE_MODELS {
        rows.extend(speech_row(app, model));
    }
    rows
}

/// The settings' Voice section: which model listens, and how what it hears
/// is used. Its changes take effect, and are saved, at once.
pub(crate) fn voice_section(app: &App) -> Element<'_, Message> {
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
    listening.push(form::toggle(
        tr!("Talk over Clew while it speaks"),
        None,
        app.voice.prefs.talk_over,
        |v| Message::Voice(VoiceMsg::SetTalkOver(v)),
    ));
    listening.extend(turn_rows(app));
    // Groups, and no captions around them: the voice key is on the button
    // it starts, and where models are kept is no setting.
    form::page(vec![
        form::group(tr!("Model"), None, None, models),
        form::group(tr!("Listening"), None, None, listening),
        form::group(tr!("Speaking"), None, None, speaking_rows(app)),
    ])
}
