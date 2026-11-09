//! Every setting with choices, as voice, the palette and the agent reach
//! them — the language Clew speaks, its theme, the provider, how voice
//! listens and speaks — each choice a palette command ("Language: French")
//! that does what its control in Settings does, and said in any of the
//! interface's languages: "change the app's language to French", "把语言
//! 改成英文", "テーマをダークに変更して", "읽어 주기 꺼 줘" ([`chosen`]).

use crate::app::prelude::*;
use crate::*;

/// A setting and its choices.
#[derive(Debug, Clone)]
pub(crate) struct Setting {
    /// Its name in English, as the catalogs know it — the palette's
    /// category for its choices.
    pub name: &'static str,
    /// Other names it goes by, in English, comma-separated, each language
    /// giving its own ([`crate::i18n::lookup_in`]); empty for none.
    pub aliases: &'static str,
    /// Whether a choice of it may be said without naming it ("switch to
    /// French", "use OpenAI"): not where its choices are words of every
    /// setting (on, off), nor where another setting has them too and is
    /// the one meant (the voice's language, beside the interface's).
    pub unnamed: bool,
    pub choices: Vec<Choice>,
}

/// One choice of a [`Setting`].
#[derive(Debug, Clone)]
pub(crate) struct Choice {
    /// The palette command that makes it, in English.
    pub title: String,
    /// The title as the reader's language shows it.
    pub shown: String,
    /// What it is called: English the catalogs translate, or a name said
    /// as it is written (a language's own name, a theme's); several
    /// comma-separated in one ([`crate::i18n::lookup_in`] of each).
    pub names: Vec<String>,
    pub message: Message,
}

/// What turns a switch on, and off: the English words, translated by the
/// catalogs.
const ON: &str = tr_noop!("on, enable, enabled, turn on, start");
const OFF: &str = tr_noop!("off, disable, disabled, turn off, stop");

/// A setting switched on or off: its two choices.
fn switch(name: &'static str, aliases: &'static str, message: fn(bool) -> Message) -> Setting {
    let choice = |on: bool| {
        let (key, words) = if on { ("On", ON) } else { ("Off", OFF) };
        Choice {
            title: format!("{name}: {key}"),
            shown: format!(
                "{}: {}",
                crate::i18n::lookup(name),
                crate::i18n::lookup(key)
            ),
            names: vec![key.to_string(), words.to_string()],
            message: message(on),
        }
    };
    Setting {
        name,
        aliases,
        unnamed: false,
        choices: vec![choice(true), choice(false)],
    }
}

/// A choice of setting `name` called `key` (English the catalogs
/// translate), and by `more`.
fn choice(name: &str, key: &str, more: &[&str], message: Message) -> Choice {
    Choice {
        title: format!("{name}: {key}"),
        shown: format!(
            "{}: {}",
            crate::i18n::lookup(name),
            crate::i18n::lookup(key)
        ),
        names: std::iter::once(key)
            .chain(more.iter().copied())
            .map(str::to_string)
            .collect(),
        message,
    }
}

impl App {
    /// Every setting with choices the window offers now, in the order the
    /// palette lists them; the first of two that offer a choice said alone
    /// is the one meant.
    pub(crate) fn settings_choices(&self) -> Vec<Setting> {
        use crate::i18n::{Lang, LangPref};
        let mut out = Vec::new();

        // The language Clew speaks: by its English name, its own, and as
        // each language calls it.
        let mut languages = vec![choice(
            tr_noop!("Language"),
            tr_noop!("Match System"),
            &[tr_noop!("System"), tr_noop!("system language, automatic")],
            Message::Settings(SettingsMsg::SetLanguage(LangPref::System)),
        )];
        for lang in Lang::ALL {
            let (key, more): (&str, &[&str]) = match lang {
                Lang::En => (tr_noop!("English"), &["英文"]),
                Lang::Fr => (tr_noop!("French"), &["法文"]),
                Lang::Zh => (
                    tr_noop!("Chinese"),
                    &["Simplified Chinese", "汉语", "普通话", "中文", "华语"],
                ),
                Lang::Ko => (tr_noop!("Korean"), &["韩文", "朝鲜语"]),
                Lang::Ja => (tr_noop!("Japanese"), &["日文"]),
            };
            let mut c = choice(
                tr_noop!("Language"),
                key,
                more,
                Message::Settings(SettingsMsg::SetLanguage(LangPref::Chosen(lang))),
            );
            c.names.push(lang.native_name().to_string());
            languages.push(c);
        }
        out.push(Setting {
            name: tr_noop!("Language"),
            aliases: tr_noop!("display language, interface language, app language"),
            unnamed: true,
            choices: languages,
        });

        // The theme: light, dark, or as the system is.
        out.push(Setting {
            name: tr_noop!("Theme"),
            aliases: tr_noop!("mode, appearance, color scheme, colour scheme"),
            unnamed: true,
            choices: [
                (theme::ThemePref::Light, tr_noop!("Light")),
                (theme::ThemePref::Dark, tr_noop!("Dark")),
                (theme::ThemePref::System, tr_noop!("Match System")),
            ]
            .into_iter()
            .map(|(pref, key)| {
                let more: &[&str] = if pref == theme::ThemePref::System {
                    &[tr_noop!("System")]
                } else {
                    &[]
                };
                choice(
                    tr_noop!("Theme"),
                    key,
                    more,
                    Message::Settings(SettingsMsg::SetThemePref(pref)),
                )
            })
            .collect(),
        });
        // Which theme is the light one, and the dark: by their names.
        for (is_light, name, themes) in [
            (true, tr_noop!("Light theme"), theme::light_choices()),
            (false, tr_noop!("Dark theme"), theme::dark_choices()),
        ] {
            out.push(Setting {
                name,
                aliases: "",
                unnamed: true,
                choices: themes
                    .into_iter()
                    .map(|t| {
                        choice(
                            name,
                            t.0.name,
                            &[],
                            Message::Settings(SettingsMsg::SetThemeVariant {
                                id: t.0.id,
                                is_light,
                            }),
                        )
                    })
                    .collect(),
            });
        }

        out.push(switch(
            tr_noop!("Check for updates automatically"),
            tr_noop!("automatic updates, update checks, automatic update checks"),
            |on| Message::Updater(UpdaterMsg::SetAuto(on)),
        ));

        // The model that answers: whose.
        out.push(Setting {
            name: tr_noop!("Provider"),
            aliases: tr_noop!("AI provider, model provider, language model provider"),
            unnamed: true,
            choices: crate::llm::Provider::ALL
                .into_iter()
                .map(|p| {
                    let more: &[&str] = match p {
                        crate::llm::Provider::Anthropic => &["Claude"],
                        crate::llm::Provider::OpenAI => &["GPT", "ChatGPT", "Open AI"],
                        crate::llm::Provider::DeepSeek => &["Deep Seek"],
                        crate::llm::Provider::Custom => &[tr_noop!("Custom")],
                    };
                    choice(
                        tr_noop!("Provider"),
                        p.label(),
                        more,
                        Message::Settings(SettingsMsg::ProviderPicked(p)),
                    )
                })
                .collect(),
        });

        // Where embeddings are made.
        out.push(Setting {
            name: tr_noop!("Embeddings"),
            aliases: tr_noop!("where embeddings are made, embedding source"),
            unnamed: false,
            choices: vec![
                choice(
                    tr_noop!("Embeddings"),
                    tr_noop!("On this computer"),
                    &[tr_noop!("this computer, local, locally, here")],
                    Message::Settings(SettingsMsg::EmbedHere(true)),
                ),
                choice(
                    tr_noop!("Embeddings"),
                    tr_noop!("A service"),
                    &[tr_noop!("service, API, online, the cloud")],
                    Message::Settings(SettingsMsg::EmbedHere(false)),
                ),
            ],
        });

        // How voice listens.
        out.push(Setting {
            name: tr_noop!("Voice model"),
            aliases: tr_noop!("listening model, speech recognition model, whisper model"),
            unnamed: true,
            choices: clew_voice::models::MODELS
                .iter()
                .map(|m| {
                    choice(
                        tr_noop!("Voice model"),
                        m.name,
                        &[],
                        Message::Voice(VoiceMsg::PickModel(m.id)),
                    )
                })
                .collect(),
        });
        if !self.voice.prefs.model().english_only {
            out.push(Setting {
                name: tr_noop!("Voice language"),
                aliases: tr_noop!("spoken language, listening language, dictation language"),
                unnamed: false,
                choices: crate::app::voice::LANGUAGES
                    .iter()
                    .map(|(code, name)| {
                        choice(
                            tr_noop!("Voice language"),
                            name,
                            &[],
                            Message::Voice(VoiceMsg::SetLanguage(*code)),
                        )
                    })
                    .collect(),
            });
        }
        out.push(switch(
            tr_noop!("Run spoken commands"),
            tr_noop!("spoken commands, voice commands"),
            |on| Message::Voice(VoiceMsg::SetRunCommands(on)),
        ));
        out.push(switch(
            tr_noop!("Send spoken questions to Ask right away"),
            tr_noop!("send questions right away, send spoken questions"),
            |on| Message::Voice(VoiceMsg::SetSendQuestions(on)),
        ));
        out.push(switch(
            tr_noop!("Talk over Clew while it speaks"),
            tr_noop!("talk over Clew, interrupt Clew, barge in"),
            |on| Message::Voice(VoiceMsg::SetTalkOver(on)),
        ));

        // How Clew speaks.
        out.push(Setting {
            name: tr_noop!("Read answers aloud"),
            aliases: tr_noop!("speak answers, read aloud, text to speech"),
            unnamed: false,
            choices: crate::app::voice::ReadAloud::ALL
                .into_iter()
                .map(|read_aloud| {
                    use crate::app::voice::ReadAloud;
                    let name = read_aloud.name();
                    // Turned on, every answer is read; off, none.
                    let words = match read_aloud {
                        ReadAloud::Never => OFF,
                        ReadAloud::Spoken => tr_noop!("spoken questions, voice questions"),
                        ReadAloud::Always => ON,
                    };
                    Choice {
                        title: format!("Read answers aloud: {name}"),
                        shown: format!(
                            "{}: {}",
                            tr!("Read answers aloud"),
                            crate::i18n::lookup(name)
                        ),
                        names: vec![name.to_string(), words.to_string()],
                        message: Message::Voice(VoiceMsg::SetSpeak(read_aloud)),
                    }
                })
                .collect(),
        });
        out.push(Setting {
            name: tr_noop!("Speed"),
            aliases: tr_noop!("speaking speed, reading speed, speech speed"),
            unnamed: false,
            choices: crate::app::speech::SPEEDS
                .iter()
                .map(|&speed| {
                    let pace = crate::i18n::decimal(f64::from(speed), 2);
                    let pace = pace.trim_end_matches('0').trim_end_matches(['.', ',']);
                    let plain = format!("{speed}");
                    let mut names = vec![
                        format!("{plain}×"),
                        format!("{plain}x"),
                        plain.clone(),
                        pace.to_string(),
                    ];
                    if (speed - 1.0).abs() < 0.001 {
                        names.push(tr_noop!("natural, normal").to_string());
                    }
                    Choice {
                        title: format!("Speed: {plain}×"),
                        shown: format!("{}: {pace}×", tr!("Speed")),
                        names,
                        message: Message::Voice(VoiceMsg::SetSpeed(speed)),
                    }
                })
                .collect(),
        });
        // The voice of each speech model here that has more than one.
        for model in clew_voice::speech::VOICE_MODELS {
            if model.voices.len() < 2 || !self.speech_installed(model) {
                continue;
            }
            out.push(Setting {
                name: tr_noop!("Voice"),
                aliases: tr_noop!("speaking voice"),
                unnamed: true,
                choices: model
                    .voices
                    .iter()
                    .map(|voice| {
                        use crate::i18n::Lang;
                        let called = voice_called(crate::i18n::current(), voice.name);
                        Choice {
                            title: format!("Voice: {}", voice_called(Lang::En, voice.name)),
                            shown: format!("{}: {called}", tr!("Voice")),
                            names: std::iter::once(voice.name.to_string())
                                .chain(Lang::ALL.into_iter().map(|l| voice_called(l, voice.name)))
                                .collect(),
                            message: Message::Voice(VoiceMsg::PickSpeechVoice {
                                model: model.id,
                                voice: voice.id,
                            }),
                        }
                    })
                    .collect(),
            });
        }
        out
    }
}

/// What a speech model's voice is called in `lang`: Supertone's F1…M5 a
/// woman's or a man's voice and its number, as Settings lists them; a
/// named one by its name.
pub(crate) fn voice_called(lang: crate::i18n::Lang, name: &str) -> String {
    let mut chars = name.chars();
    let kind = chars.next();
    let number: String = chars.collect();
    let template = match kind {
        _ if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) => None,
        Some('F') => Some(tr_noop!("Female voice {n}")),
        Some('M') => Some(tr_noop!("Male voice {n}")),
        _ => None,
    };
    match template {
        Some(t) => crate::i18n::format(crate::i18n::lookup_in(lang, t), &[("n", &number)]),
        None => name.to_string(),
    }
}

// ---- hearing a choice -------------------------------------------------------

/// What is said around a setting and its choice, in any of the interface's
/// languages — "change … to", "把…改成", "…を…に変更して" — and counts for
/// nothing: what is left once the setting and the choice are found must be
/// only these. Each is read as the words said are ([`units`]).
const AROUND: &[&str] = &[
    // English.
    "change",
    "set",
    "switch",
    "make",
    "use",
    "turn",
    "put",
    "select",
    "choose",
    "pick",
    "to",
    "the",
    "a",
    "an",
    "my",
    "app",
    "apps",
    "application",
    "applications",
    "s",
    "clew",
    "clews",
    "clue",
    "interface",
    "please",
    "can",
    "could",
    "you",
    "would",
    "will",
    "i",
    "want",
    "wanna",
    "like",
    "it",
    "into",
    "in",
    "as",
    "of",
    "for",
    "now",
    "just",
    "lets",
    "let",
    "us",
    "me",
    "go",
    "ok",
    "okay",
    "hey",
    "be",
    "is",
    "should",
    "so",
    "with",
    "and",
    "then",
    "back",
    "instead",
    "its",
    "using",
    "setting",
    "settings",
    "option",
    // French.
    "changer",
    "changez",
    "changes",
    "mets",
    "mettre",
    "mettez",
    "passe",
    "passer",
    "passez",
    "règle",
    "régler",
    "réglez",
    "utilise",
    "utiliser",
    "utilisez",
    "choisis",
    "choisir",
    "choisissez",
    "sélectionne",
    "bascule",
    "basculer",
    "la",
    "le",
    "les",
    "l",
    "de",
    "du",
    "des",
    "d",
    "en",
    "à",
    "au",
    "aux",
    "sur",
    "pour",
    "par",
    "il",
    "vous",
    "te",
    "plaît",
    "plait",
    "merci",
    "appli",
    "peux",
    "tu",
    "pouvez",
    "je",
    "veux",
    "voudrais",
    "moi",
    "svp",
    "un",
    "une",
    "mon",
    "ma",
    "mes",
    "cette",
    "ce",
    "maintenant",
    "et",
    "réglage",
    // Chinese.
    "把",
    "将",
    "改成",
    "改为",
    "改到",
    "换成",
    "换为",
    "换到",
    "设为",
    "设置为",
    "设成",
    "设置成",
    "设定为",
    "切换到",
    "切换成",
    "切换为",
    "切到",
    "调成",
    "调到",
    "调为",
    "使用",
    "用",
    "请",
    "帮我",
    "给我",
    "我想",
    "我要",
    "一下",
    "吧",
    "谢谢",
    "的",
    "应用",
    "软件",
    "界面",
    "程序",
    "成",
    "为",
    "到",
    "改",
    "换",
    "设置",
    "切换",
    "麻烦",
    "你",
    "能",
    "可以",
    "要",
    "想",
    "我",
    "让",
    "变成",
    "变为",
    "选择",
    "选",
    // Japanese.
    "を",
    "に",
    "へ",
    "は",
    "の",
    "で",
    "が",
    "変更",
    "変えて",
    "変える",
    "変更して",
    "切り替え",
    "切り替えて",
    "切り替える",
    "切替",
    "設定",
    "設定して",
    "して",
    "ください",
    "下さい",
    "お願い",
    "お願いします",
    "します",
    "アプリ",
    "使う",
    "使って",
    "使用",
    "する",
    "にして",
    "選んで",
    "選択",
    // Korean.
    "를",
    "을",
    "로",
    "으로",
    "은",
    "는",
    "의",
    "바꿔",
    "바꿔줘",
    "바꿔주세요",
    "변경",
    "변경해",
    "변경해줘",
    "변경해주세요",
    "설정",
    "설정해",
    "설정해줘",
    "해줘",
    "해주세요",
    "해",
    "줘",
    "주세요",
    "앱",
    "사용",
    "사용해",
    "사용해줘",
    "전환",
    "전환해",
    "전환해줘",
    "좀",
    "선택",
    "선택해",
];

/// Whether `c` is written without spaces between its words — Chinese,
/// Japanese, and the syllables Korean joins its particles to — so that each
/// is a unit of its own ([`units`]).
fn unspaced(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}' // kana
        | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' // han
        | '\u{AC00}'..='\u{D7AF}' // hangul syllables
        | '\u{F900}'..='\u{FAFF}')
}

/// `text` as the units it is matched by: its words, lowercase, broken at
/// spaces, punctuation and apostrophes ("app's" — app, s; "l'appli" — l,
/// appli), a figure's decimal point kept ("1.25"); and each character of
/// what is written without spaces (中文 — 中, 文), as nothing says where its
/// words end.
pub(crate) fn units(text: &str) -> Vec<String> {
    let text = text.to_lowercase();
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let decimal = c == '.'
            && word.chars().last().is_some_and(|p| p.is_ascii_digit())
            && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit());
        if unspaced(c) {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            out.push(c.to_string());
        } else if c.is_alphanumeric() || decimal {
            word.push(c);
        } else if !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// Every name `key` goes by: itself and each language's own for it, each
/// of their comma-separated parts, as [`units`].
fn named(key: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    for lang in crate::i18n::Lang::ALL {
        for part in crate::i18n::lookup_in(lang, key).split([',', '，', '、']) {
            let u = units(part);
            if !u.is_empty() && !out.contains(&u) {
                out.push(u);
            }
        }
    }
    out
}

/// Whether `said` is `name` as a speech model may spell it: the same, a
/// plural of it, or one letter off in a long word ("frensh").
fn sounds_like(said: &str, name: &str) -> bool {
    if said == name {
        return true;
    }
    if said.strip_suffix('s') == Some(name) || name.strip_suffix('s') == Some(said) {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (said.chars().collect(), name.chars().collect());
    if a.len() < 5 || b.len() < 5 || a.iter().any(|c| unspaced(*c)) {
        return false;
    }
    let (short, long) = if a.len() <= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    if long.len() - short.len() > 1 {
        return false;
    }
    let Some(i) = short.iter().zip(long.iter()).position(|(x, y)| x != y) else {
        return true;
    };
    if short.len() == long.len() {
        short[i + 1..] == long[i + 1..]
    } else {
        short[i..] == long[i + 1..]
    }
}

/// Where in `said` the units of `name` are, one after another.
fn spans(said: &[String], name: &[String]) -> Vec<std::ops::Range<usize>> {
    if name.is_empty() || name.len() > said.len() {
        return Vec::new();
    }
    (0..=said.len() - name.len())
        .filter(|&at| name.iter().zip(&said[at..]).all(|(n, s)| sounds_like(s, n)))
        .map(|at| at..at + name.len())
        .collect()
}

/// Whether `units` is nothing but what is said [`AROUND`] a choice.
fn only_around(units: &[String]) -> bool {
    static AROUND_UNITS: std::sync::OnceLock<Vec<Vec<String>>> = std::sync::OnceLock::new();
    let around = AROUND_UNITS.get_or_init(|| AROUND.iter().map(|w| self::units(w)).collect());
    // Whether the first `i` units are made of them.
    let mut made = vec![false; units.len() + 1];
    made[0] = true;
    for i in 0..units.len() {
        if !made[i] {
            continue;
        }
        for w in around {
            if units[i..].starts_with(w) {
                made[i + w.len()] = true;
            }
        }
    }
    made[units.len()]
}

/// Whether `said` (its [`units`]) opens as a question does — "is it in
/// French", "what is the theme" — which asks what a setting is, not for
/// it to change: some of a question's words are what a request is said
/// around ("is", "it", "in": [`AROUND`]). "Can you switch to French",
/// asked as a question is, is a request. (Another language's question
/// has words of its own no request is said with — 吗, か, est-ce — and is
/// no choice already.)
fn asks_a_question(said: &[String]) -> bool {
    const OPENS: [&str; 18] = [
        "is", "are", "was", "were", "am", "does", "do", "did", "what", "whats", "which", "why",
        "how", "where", "when", "who", "should", "shall",
    ];
    const ASKING: [&str; 4] = ["can", "could", "would", "will"];
    const OF_THE_READER: [&str; 4] = ["you", "we", "i", "u"];
    match said {
        [first, ..] if OPENS.contains(&first.as_str()) => true,
        [first, second, ..] if ASKING.contains(&first.as_str()) => {
            !OF_THE_READER.contains(&second.as_str())
        }
        _ => false,
    }
}

/// Which of `settings`' choices `said` asks for, as (setting, choice): its
/// choice named, and its setting named or — where it may be ([`Setting::
/// unnamed`]) — not, with nothing else said but what is said around a
/// choice ([`AROUND`]). A choice with its setting named comes before one
/// without; the more of what was said they name, the better; and two
/// settings' choices that fit as well are no answer, but where neither
/// was named: the first setting is the one meant ("switch to French" —
/// the interface's language, not the voice's).
pub(crate) fn chosen(said: &str, settings: &[Setting]) -> Option<(usize, usize)> {
    let said = units(said);
    if said.is_empty() || asks_a_question(&said) {
        return None;
    }
    // (named, units named), and where.
    let mut best: Option<((bool, usize), (usize, usize))> = None;
    let mut tied = false;
    for (s, setting) in settings.iter().enumerate() {
        let setting_names: Vec<Vec<String>> = [setting.name, setting.aliases]
            .into_iter()
            .filter(|key| !key.is_empty())
            .flat_map(named)
            .collect();
        for (c, choice) in setting.choices.iter().enumerate() {
            let choice_names: Vec<Vec<String>> =
                choice.names.iter().flat_map(|n| named(n)).collect();
            let mut fit: Option<(bool, usize)> = None;
            for name in &choice_names {
                for at in spans(&said, name) {
                    let rest_without = |skip: &[&std::ops::Range<usize>]| -> Vec<String> {
                        said.iter()
                            .enumerate()
                            .filter(|(i, _)| !skip.iter().any(|r| r.contains(i)))
                            .map(|(_, u)| u.clone())
                            .collect()
                    };
                    // With its setting named.
                    for setting_name in &setting_names {
                        for named_at in spans(&said, setting_name) {
                            if named_at.start < at.end && at.start < named_at.end {
                                continue;
                            }
                            if only_around(&rest_without(&[&at, &named_at])) {
                                let score = (true, at.len() + named_at.len());
                                fit = fit.max(Some(score));
                            }
                        }
                    }
                    // Alone.
                    if setting.unnamed && only_around(&rest_without(&[&at])) {
                        fit = fit.max(Some((false, at.len())));
                    }
                }
            }
            let Some(score) = fit else {
                continue;
            };
            match best {
                Some((top, _)) if score < top => {}
                Some((top, (bs, _))) if score == top => {
                    // Unnamed, the first setting is the one meant; the same
                    // setting's two choices, or two settings named as well,
                    // are no answer.
                    if score.0 || bs == s {
                        tied = true;
                    }
                }
                _ => {
                    best = Some((score, (s, c)));
                    tied = false;
                }
            }
        }
    }
    if tied { None } else { best.map(|(_, at)| at) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(app: &App) -> Vec<Setting> {
        app.settings_choices()
    }

    fn heard(settings: &[Setting], said: &str) -> Option<String> {
        chosen(said, settings).map(|(s, c)| settings[s].choices[c].title.clone())
    }

    /// The words of what is said, and of what is written without spaces
    /// each character.
    /// A question about a setting asks what it is: "is it in French?"
    /// changes nothing, where "set it in French" — and "can you switch to
    /// French?", asked as a question is — does.
    #[test]
    fn a_question_about_a_setting_changes_nothing() {
        let app = crate::app::tests::blank_app();
        let settings = app.settings_choices();
        let picks = |said: &str| chosen(said, &settings).is_some();
        for said in [
            "Is it in French?",
            "what is the language",
            "is the theme dark",
            "does it use French",
            "will it be dark",
        ] {
            assert!(!picks(said), "{said:?}");
        }
        for said in [
            "set it in French",
            "Can you switch to French?",
            "could you make it dark",
            "change the app's language to French",
        ] {
            assert!(picks(said), "{said:?}");
        }
    }

    #[test]
    fn what_is_said_is_read_as_its_units() {
        assert_eq!(
            units("Change the app's language to French."),
            ["change", "the", "app", "s", "language", "to", "french"]
        );
        assert_eq!(
            units("把语言改成英文"),
            ["把", "语", "言", "改", "成", "英", "文"]
        );
        assert_eq!(units("언어를 영어로"), ["언", "어", "를", "영", "어", "로"]);
        assert_eq!(units("speed 1.25×"), ["speed", "1.25"]);
        assert_eq!(units("l'appli"), ["l", "appli"]);
    }

    /// The sentence that found nothing: the interface's language, said as
    /// anyone says it, its name misheard included — and in each of the
    /// interface's languages.
    #[test]
    fn a_setting_is_changed_however_it_is_said() {
        let _env = crate::app::tests::env_lock();
        let app = crate::app::tests::blank_app();
        let s = titles(&app);
        for said in [
            "change the app's language to frensh",
            "Change the app's language to French.",
            "set the language to French please",
            "switch to French",
            "French",
            "use Français",
            "change the display language to français",
            "changer la langue de l'application en français",
            "把语言改成法语",
            "言語をフランス語に変更してください",
            "언어를 프랑스어로 바꿔 줘",
        ] {
            assert_eq!(
                heard(&s, said).as_deref(),
                Some("Language: French"),
                "{said:?}"
            );
        }
        assert_eq!(
            heard(&s, "把应用的语言改成英文").as_deref(),
            Some("Language: English")
        );
        assert_eq!(
            heard(&s, "set the theme to dark").as_deref(),
            Some("Theme: Dark")
        );
        assert_eq!(heard(&s, "dark mode").as_deref(), Some("Theme: Dark"));
        assert_eq!(
            heard(&s, "use one dark").as_deref(),
            Some("Dark theme: One Dark")
        );
        assert_eq!(
            heard(&s, "turn off read answers aloud").as_deref(),
            Some("Read answers aloud: Never")
        );
        assert_eq!(
            heard(&s, "turn read answers aloud on").as_deref(),
            Some("Read answers aloud: Always")
        );
        assert_eq!(heard(&s, "use OpenAI").as_deref(), Some("Provider: OpenAI"));
        assert_eq!(
            heard(&s, "set the voice language to french").as_deref(),
            Some("Voice language: French")
        );
        assert_eq!(
            heard(&s, "set the speed to 1.25").as_deref(),
            Some("Speed: 1.25×")
        );
    }

    /// What is more than a setting and its choice is a request, Ask's; a
    /// switch's on or off says nothing without the switch; and two
    /// settings named alike are none.
    #[test]
    fn what_is_more_than_a_choice_is_not_one() {
        let _env = crate::app::tests::env_lock();
        let app = crate::app::tests::blank_app();
        let s = titles(&app);
        for said in [
            "why is the language French",
            "translate this to French",
            "on",
            "turn it off",
            "open the light theme file",
            "",
        ] {
            assert_eq!(heard(&s, said), None, "{said:?}");
        }
    }
}
