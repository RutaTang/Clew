//! What is said for what is on screen ([`crate::ui::heard`]): a button or
//! a choice by its name, the question on screen answered yes or no, a
//! list's row by its place in it ("open the third result", "第三个"), and
//! the words that put away what is on top ("close", "never mind") — so
//! that everything a click does, a word does.

use crate::app::prelude::*;
use crate::app::voice::{Heard, HeardOutcome, Waiting};
use crate::ui::heard::{Answer, Layer, Target};
use crate::*;

/// Why an answer is held ([`App::hold_answer`]): its question was not
/// read yet — not in view, covered, or just come — and its Allow cannot be
/// pressed.
pub(crate) const TOO_SOON: &str = tr_noop!(
    "Too soon to answer — say it again once the question has been in view, uncovered, for a moment, with the window in front"
);
/// … it allows, and was said as a question ("Allow?").
pub(crate) const ASKED: &str = tr_noop!("To answer, say it plainly — not as a question");
/// … more was said after a yes or a no ("yes, allow it", "okay, why that
/// file?"), which answers nothing: the question waits for a plain answer
/// before anything else is said.
pub(crate) const ALONE: &str =
    tr_noop!("A question is waiting — answer it first with a plain “yes” or “no”");
/// … a plain yes or no, where the question has none to give ("Always
/// reject" its only way to decline), or a word that also puts away what is
/// on top ("cancel"): its answer wants its own words.
pub(crate) const BY_NAME: &str = tr_noop!("Say the answer you mean by the words on its button");
/// … spoken commands are off: voice presses nothing.
pub(crate) const COMMANDS_OFF: &str =
    tr_noop!("Spoken commands are off — answer the question with its buttons");

/// What was said to a question waiting ([`App::said_to_question`]).
pub(crate) enum ToQuestion {
    /// A yes or a no, alone or opening more.
    YesOrNo,
    /// The name of something it offers.
    Answer(Box<Target>),
}

/// What is said before or after a thing's name to press it — "click on
/// Download", "the Retry button" — and means nothing more: the English
/// words, and each language's.
const PRESS: &str = tr_noop!("click, click on, press, tap, hit, select, choose, pick, the, button");

/// What puts away what is on top — a dialog, a menu, Time Travel, the find
/// bar, Ask — as Esc does: the English words, and each language's.
const DISMISS: &str = tr_noop!(
    "close, close it, close this, close that, cancel, never mind, dismiss, escape, exit, go away, hide it, hide this"
);

/// The words a list's place is said with, around its number: "open the
/// third result", "number 4", "第三个", "3番目", "세 번째".
const AROUND_A_ROW: &[&str] = &[
    // English.
    "open",
    "pick",
    "choose",
    "select",
    "click",
    "go",
    "to",
    "take",
    "show",
    "me",
    "jump",
    "the",
    "result",
    "item",
    "row",
    "entry",
    "option",
    "choice",
    "match",
    "file",
    "number",
    "no",
    "please",
    "on",
    // French.
    "ouvre",
    "ouvrir",
    "ouvrez",
    "choisis",
    "choisir",
    "choisissez",
    "sélectionne",
    "prends",
    "le",
    "la",
    "l",
    "résultat",
    "élément",
    "entrée",
    "numéro",
    "n",
    "fichier",
    // Chinese.
    "打开",
    "选",
    "选择",
    "点",
    "点击",
    "第",
    "个",
    "条",
    "项",
    "号",
    "结果",
    // Japanese.
    "番目",
    "目",
    "番",
    "の",
    "を",
    "開いて",
    "開く",
    "選んで",
    "選択",
    "結果",
    "項目",
    // Korean.
    "번째",
    "째",
    "번",
    "열어",
    "열어줘",
    "선택",
    "선택해",
    "줘",
    "결과",
    "항목",
    "를",
    "을",
];

/// Which row of a list is said.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RowAsked {
    /// The `n`th, from 1.
    Nth(usize),
    /// The last.
    Last,
}

/// The place in a list `said` asks for, if that is all it says.
pub(crate) fn row_asked(said: &str) -> Option<RowAsked> {
    use crate::app::choices::units;
    let around: Vec<Vec<String>> = AROUND_A_ROW.iter().map(|w| units(w)).collect();
    let mut left: Vec<String> = Vec::new();
    let all = units(said);
    let mut i = 0;
    'units: while i < all.len() {
        // The longest of the words around a row that starts here.
        if let Some(w) = around
            .iter()
            .filter(|w| all[i..].starts_with(w))
            .max_by_key(|w| w.len())
        {
            i += w.len();
            continue 'units;
        }
        left.push(all[i].clone());
        i += 1;
    }
    // "the third one", "the last one": the one after a place said.
    if left.len() == 2 && left[1] == "one" && (ordinal(&left[0]).is_some() || left[0] == "last") {
        left.pop();
    }
    // Joined, as what is written without spaces is read a character at a
    // time (最后一个: 最, 后, 一, 个).
    let joined = left.concat();
    if [
        "last",
        "dernier",
        "dernière",
        "最后",
        "最后一",
        "最後",
        "마지막",
    ]
    .contains(&joined.as_str())
    {
        return Some(RowAsked::Last);
    }
    number_said(&left).map(RowAsked::Nth)
}

/// A place said as a word of its own: "first", "third", "troisième".
fn ordinal(word: &str) -> Option<usize> {
    const EN: [&str; 10] = [
        "first", "second", "third", "fourth", "fifth", "sixth", "seventh", "eighth", "ninth",
        "tenth",
    ];
    const FR: [&str; 10] = [
        "premier",
        "deuxième",
        "troisième",
        "quatrième",
        "cinquième",
        "sixième",
        "septième",
        "huitième",
        "neuvième",
        "dixième",
    ];
    EN.iter()
        .chain(FR.iter())
        .position(|w| *w == word)
        .map(|i| i % 10 + 1)
        .or(match word {
            "première" => Some(1),
            "seconde" => Some(2),
            _ => None,
        })
}

/// The number `units` say, and nothing else: in figures, an English place
/// or count ("third", "twenty one"), French, Chinese or Japanese numerals
/// (十二), or Korean's own counting words (세 — three).
fn number_said(units: &[String]) -> Option<usize> {
    if units.is_empty() {
        return None;
    }
    if units.len() == 1 {
        let w = units[0].trim_start_matches('#');
        if let Ok(n) = w.parse::<usize>() {
            return (n > 0).then_some(n);
        }
        if let Some(n) = ordinal(w) {
            return Some(n);
        }
    }
    // Korean counts its places with its own words: 첫 번째, 두 번째.
    const KO: [&str; 10] = [
        "첫", "두", "세", "네", "다섯", "여섯", "일곱", "여덟", "아홉", "열",
    ];
    if let Some(i) = KO.iter().position(|k| *k == units.concat()) {
        return Some(i + 1);
    }
    // Chinese and Japanese numerals, a character each.
    const HAN: [char; 10] = ['一', '二', '三', '四', '五', '六', '七', '八', '九', '十'];
    let han: Option<Vec<usize>> = units
        .iter()
        .map(|u| {
            let mut c = u.chars();
            match (c.next(), c.next()) {
                (Some('两'), None) => Some(2),
                (Some(ch), None) => HAN.iter().position(|h| *h == ch).map(|i| i + 1),
                _ => None,
            }
        })
        .collect();
    if let Some(digits) = han {
        // 十 is ten; 十二 twelve; 二十 twenty; 二十三 twenty-three.
        let n = match digits.as_slice() {
            [d] => *d,
            [10, d] => 10 + d,
            [d, 10] => d * 10,
            [d, 10, e] => d * 10 + e,
            _ => return None,
        };
        return (n > 0).then_some(n);
    }
    crate::app::spoken::number(&units.join(" "))
}

/// `said` without what is said around a name to press it ([`PRESS`]) —
/// and a name, so: "Select All" is "all" said bare, as "select all" is.
fn bare(said: &str) -> Vec<String> {
    use crate::app::choices::units;
    let mut words = units(said);
    let around: Vec<Vec<String>> = crate::i18n::Lang::ALL
        .into_iter()
        .flat_map(|l| crate::i18n::lookup_in(l, PRESS).split([',', '，', '、']))
        .map(units)
        .filter(|u| !u.is_empty())
        .collect();
    loop {
        let before = words.len();
        if let Some(w) = around
            .iter()
            .filter(|w| words.starts_with(w) && words.len() > w.len())
            .max_by_key(|w| w.len())
        {
            words.drain(..w.len());
        }
        if let Some(w) = around
            .iter()
            .filter(|w| words.ends_with(w) && words.len() > w.len())
            .max_by_key(|w| w.len())
        {
            words.truncate(words.len() - w.len());
        }
        if words.len() == before {
            return words;
        }
    }
}

/// Whether `said` is only a word that puts away what is on top.
pub(crate) fn dismisses(said: &str) -> bool {
    use crate::app::choices::units;
    let said = units(said);
    !said.is_empty()
        && crate::i18n::Lang::ALL.into_iter().any(|l| {
            crate::i18n::lookup_in(l, DISMISS)
                .split([',', '，', '、'])
                .any(|w| units(w) == said)
        })
}

/// Whether `said` puts away what is on top by its `name` (an English key,
/// [`crate::ui::ActiveOverlay::name`]), said in English or any language's
/// words for it: "close the call graph", "hide Settings", "关闭调用图".
/// What is left with the name taken out must put away on its own
/// ([`dismisses`]), the small words between them aside — "explain the call
/// graph" is a question, and Ask's.
pub(crate) fn dismisses_named(said: &str, name: &str) -> bool {
    use crate::app::choices::units;
    let said = units(said);
    crate::i18n::Lang::ALL.into_iter().any(|lang| {
        let name = units(crate::i18n::lookup_in(lang, name));
        if name.is_empty() || said.len() <= name.len() {
            return false;
        }
        (0..=said.len() - name.len())
            .filter(|&at| said[at..at + name.len()] == name[..])
            .any(|at| {
                // Matched as units again, a word a character where words
                // are written unspaced: joined by spaces either way.
                let rest: Vec<&str> = said[..at]
                    .iter()
                    .chain(&said[at + name.len()..])
                    .map(String::as_str)
                    .filter(|w| !matches!(*w, "the" | "this" | "that" | "my"))
                    .collect();
                dismisses(&rest.join(" "))
            })
    })
}

/// The one target `said` names: its name said as it is shown or in
/// English ("Download", "Download Whisper Small"), "click" or "the
/// button" around it or not. `None` when nothing is named — or two
/// different things are, alike.
pub(crate) fn named(said: &str, targets: &[Target]) -> Option<Target> {
    let words = bare(said);
    if words.is_empty() {
        return None;
    }
    let squeeze = |w: &[String]| w.concat();
    let said_squeezed = squeeze(&words);
    // A name as it is, or bare of what is said around one, as what was
    // said is: "select all" names "Select All" both ways.
    let exact: Vec<&Target> = targets
        .iter()
        .filter(|t| {
            [&t.title, &t.shown].iter().any(|name| {
                let u = crate::app::choices::units(name);
                !u.is_empty()
                    && (squeeze(&u) == said_squeezed || squeeze(&bare(name)) == said_squeezed)
            })
        })
        .collect();
    if let Some(one) = only_one(&exact) {
        return Some(one.clone());
    }
    if !exact.is_empty() {
        return None;
    }
    // Said as a command is: its words in any order, its aside left out.
    let said = words.join(" ");
    let titles: Vec<&str> = targets.iter().map(|t| t.title.as_str()).collect();
    let found = clew_voice::intent::command(&said, &titles).or_else(|| {
        let shown: Vec<&str> = targets.iter().map(|t| t.shown.as_str()).collect();
        crate::app::voice::said_as_shown(crate::i18n::current(), &said, &shown)
    })?;
    Some(targets[found].clone())
}

/// The one of `found` — or of several that do the same, offered twice (a
/// button and its menu row) — or none.
fn only_one<'a>(found: &[&'a Target]) -> Option<&'a Target> {
    let first = found.first()?;
    let does = format!("{:?}", first.message);
    found
        .iter()
        .all(|t| format!("{:?}", t.message) == does)
        .then_some(*first)
}

/// The row of a list on screen `said` asks for by its place: of the first
/// list drawn (the sidebar's before the margin's). Never for a yes or a no
/// said alone: Korean's 네 is "yes" before it is "four".
pub(crate) fn row(said: &str, targets: &[Target]) -> Option<Target> {
    if crate::app::spoken::only_yes_or_no(said).is_some() {
        return None;
    }
    let asked = row_asked(said)?;
    let list = targets.iter().find_map(|t| t.row.map(|(list, _)| list))?;
    let rows: Vec<&Target> = targets
        .iter()
        .filter(|t| t.row.is_some_and(|(l, _)| l == list))
        .collect();
    let at = match asked {
        RowAsked::Nth(n) => rows.iter().find(|t| t.row.is_some_and(|(_, i)| i == n)),
        RowAsked::Last => rows.last(),
    }?;
    Some((*at).clone())
}

/// Whether pressing a target by a word, which may have been misheard,
/// should wait for a yes: what removes, forgets or resets something, and
/// what costs something as a command said does (a paid pass, the program
/// started) — never the answer to a question, which was asked already.
fn asks_first(target: &Target) -> bool {
    target.answer.is_none() && (undoes(target) || costs(&target.message))
}

/// Whether `message`, said as a command, would wait for a yes
/// ([`crate::app::palette::PaletteCommand::asks_when_said`]).
fn costs(message: &Message) -> bool {
    crate::app::palette::PaletteCommand {
        title: String::new(),
        keys: None,
        action: crate::app::palette::PaletteAction::Send(message.clone()),
        shown_as: None,
    }
    .asks_when_said()
}

/// Whether pressing `target` removes, forgets or resets something.
fn undoes(target: &Target) -> bool {
    const WORDS: [&str; 9] = [
        "Remove",
        "Delete",
        "Forget",
        "Revoke",
        "Disconnect",
        "Clear",
        "Reset",
        "Discard",
        "Uninstall",
    ];
    WORDS.iter().any(|w| target.title.starts_with(w))
}

impl App {
    /// Do what `said` asks of what is on screen, if it asks anything of
    /// it: the question's answer, a dialog's button, the dismissal of what
    /// is on top (`first`: before the palette is heard, as a dialog takes
    /// the voice as it takes the pointer); else a button, a choice, or a
    /// list's row by name or place (after the palette and the settings).
    pub(crate) fn heard_on_screen(
        &mut self,
        said: &str,
        targets: &[Target],
        first: bool,
    ) -> Option<Task<Message>> {
        if first {
            // A drop-down open, or jump mode's codes up: what Esc puts away
            // before anything else — a dialog's Close or Cancel under the
            // drop-down is not pressed for it.
            if dismisses(said) && (self.picker.is_some() || self.jump.is_some()) {
                return self.dismiss_heard(said, targets);
            }
            // A question on screen answered. A word that also puts away
            // what is on top ("cancel", "never mind") answers only a
            // question that is on top: said with a card in Ask, it closes
            // what it closes, and declines nothing it was not looking at.
            if let Some(yes) = crate::app::spoken::only_yes_or_no(said) {
                let answer = if yes { Answer::Yes } else { Answer::No };
                let dismissing = !yes && dismisses(said);
                if let Some(t) = targets.iter().find(|t| {
                    t.answer == Some(answer) && (!dismissing || t.layer >= Layer::Overlay)
                }) {
                    return Some(self.press_heard(said, t.clone()));
                }
            }
            // A dialog's button, or its list's row by its place — before
            // what is said is typed into its field: "the third one", said
            // to the command bar, opens its third result.
            if targets.iter().any(|t| t.layer >= Layer::Overlay)
                && let Some(t) = named(said, targets).or_else(|| row(said, targets))
            {
                return Some(self.press_heard(said, t));
            }
            if dismisses(said) {
                return self.dismiss_heard(said, targets);
            }
            // What is on top put away by its name: "close the call graph"
            // went to Ask as a question about the call graph.
            if let Some(message) = ui::active_overlay(self)
                .filter(|o| o.name().is_some_and(|name| dismisses_named(said, name)))
                .map(|o| o.dismiss_message())
            {
                let task = self.update(message);
                return Some(self.closed_as_said(said, task));
            }
            // A field on top — a note, a condition, the command bar's
            // query — takes what else is said, as its words.
            if let Some((value, on_input)) = targets
                .iter()
                .filter(|t| t.layer >= Layer::Overlay)
                .find_map(|t| t.typing.clone())
            {
                return Some(self.type_heard(said, &value, on_input));
            }
            return None;
        }
        let t = named(said, targets).or_else(|| row(said, targets))?;
        Some(self.press_heard(said, t))
    }

    /// Type `said` into a field that holds `value`: after what is there,
    /// a space between.
    fn type_heard(
        &mut self,
        said: &str,
        value: &str,
        on_input: fn(String) -> Message,
    ) -> Task<Message> {
        let words = said.trim().trim_end_matches(['.', '。']).to_string();
        let typed = if value.trim().is_empty() {
            words
        } else {
            format!("{} {words}", value.trim_end())
        };
        self.status = tr!("Typed “{text}”", text = said);
        self.voice.hear(Heard {
            text: said.to_string(),
            outcome: HeardOutcome::Ran(tr!("Typed").to_string()),
        });
        self.update(on_input(typed))
    }

    /// Press `target`, as `said` asked — or, when it undoes or costs
    /// something, ask first ([`asks_first`]).
    fn press_heard(&mut self, said: &str, target: Target) -> Task<Message> {
        let text = said.to_string();
        // An answer whose question was not read yet, or a button waiting
        // on something ([`Target::waiting`]): what was said for it waits,
        // told why — sent on to Ask, a "yes" would read to the agent asking
        // as consent.
        if let Some(why) = target.waiting {
            return self.hold_answer(text, why);
        }
        // "Allow?", "Always allow read_file?": asked, not answered — what
        // allows wants to be said plainly.
        if target.allows && crate::app::spoken::asked(said) && self.question_pending().is_some() {
            return self.hold_answer(text, ASKED);
        }
        if asks_first(&target) {
            self.status = tr!(
                "{command}? Say yes or no — “{text}”",
                text = text,
                command = target.shown
            );
            self.voice.hear(Heard {
                text,
                outcome: HeardOutcome::Confirm(target.shown.clone()),
            });
            self.voice.confirm = Some(Waiting::Press {
                shown: target.shown,
                message: Box::new(target.message),
            });
            return Task::none();
        }
        self.status = tr!("“{text}” → {command}", text = text, command = target.shown);
        self.voice.hear(Heard {
            text,
            outcome: HeardOutcome::Ran(target.shown),
        });
        self.press(target.message)
    }

    /// What `said` says to a question waiting ([`App::question_pending`])
    /// that nothing within reach took — the name of something it offers
    /// ([`crate::ui::answers_waiting`]: "Trust and connect", "Always reject"),
    /// or anything that opens with a yes or a no ("yes", "allow it", "okay,
    /// why that file?", "don't run it") — wherever that question is:
    /// covered, not yet ready to answer, in an Ask put away, a page further
    /// on its card. Held ([`App::hold_for_question`]), it goes nowhere else:
    /// the question is answered first, plainly, and what else is to be said
    /// is said after.
    pub(crate) fn said_to_question(&self, said: &str) -> Option<ToQuestion> {
        self.question_pending()?;
        if let Some(target) = named(said, &crate::ui::answers_waiting(self)) {
            return Some(ToQuestion::Answer(Box::new(target)));
        }
        crate::app::spoken::yes_or_no(said).map(|_| ToQuestion::YesOrNo)
    }

    /// Hold `text`, said to a question waiting (`to`, from
    /// [`App::said_to_question`]), saying why — spoken commands off, a yes
    /// or a no the question has no answer for, more said than a yes or a
    /// no, asked rather than answered, or said before the question could
    /// be read — and bring that question into view where it can be: an
    /// approval's Ask put away comes back, on the page of the answer meant.
    pub(crate) fn hold_for_question(&mut self, text: String, to: ToQuestion) -> Task<Message> {
        let more = matches!(to, ToQuestion::YesOrNo)
            && crate::app::spoken::only_yes_or_no(&text).is_none();
        // The answer meant: the one named, or the question's own yes or
        // no — not for a word that also puts away what is on top
        // ("cancel"), which, said again, would close the question instead.
        let meant = match to {
            ToQuestion::Answer(target) => Some(*target),
            ToQuestion::YesOrNo => crate::app::spoken::yes_or_no(&text)
                .filter(|_| !dismisses(&text))
                .and_then(|yes| {
                    let answer = Some(if yes { Answer::Yes } else { Answer::No });
                    crate::ui::answers_waiting(self)
                        .into_iter()
                        .find(|t| t.answer == answer)
                }),
        };
        match meant.as_ref().map(|t| &t.message) {
            Some(Message::Ask(AskMsg::Approve { option, .. })) => {
                let option = option.clone();
                self.show_approval_answer(&option);
            }
            None if matches!(
                self.question_pending(),
                Some(crate::app::model::Question::Approval { .. })
            ) =>
            {
                self.reveal_ask();
            }
            _ => {}
        }
        let why = if !self.voice.prefs.run_commands {
            COMMANDS_OFF
        } else if meant.is_none() {
            BY_NAME
        } else if more {
            ALONE
        } else if crate::app::spoken::asked(&text) && meant.as_ref().is_some_and(|t| t.allows) {
            // Only what allows wants it said plainly: a decline is taken
            // asked too, once in view.
            ASKED
        } else {
            TOO_SOON
        };
        self.hold_answer(text, why)
    }

    /// Bring the approval waiting into view on the page of its answer
    /// `option`: Ask, put away, comes back, and the card turns to it.
    fn show_approval_answer(&mut self, option: &str) {
        let Some((stream, key, page)) = self.first_approval().and_then(|(turn, approval)| {
            let (_, pages) = crate::ui::shown_answers(&approval.options, 0);
            (0..pages)
                .find(|&page| {
                    crate::ui::shown_answers(&approval.options, page)
                        .0
                        .iter()
                        .any(|o| o.id == option)
                })
                .map(|page| (turn.stream, approval.key, page))
        }) else {
            return;
        };
        self.reveal_ask();
        let question = crate::app::model::Question::Approval { stream, key };
        let shown = match &self.asking.answers_page {
            Some((on, page)) if *on == question => *page,
            _ => 0,
        };
        if shown != page {
            let _ = self.on_answers_page(stream, key, page);
        }
    }

    /// An answer said (`text`), held, saying `why` ([`TOO_SOON`], [`ASKED`],
    /// [`ALONE`], [`BY_NAME`], [`COMMANDS_OFF`], or a button's own reason): it presses
    /// nothing, and goes nowhere else — sent on to Ask, a "yes" would read
    /// to the agent asking as consent.
    pub(crate) fn hold_answer(&mut self, text: String, why: &'static str) -> Task<Message> {
        self.status = crate::i18n::lookup(why).into();
        self.voice.hear(Heard {
            text,
            outcome: HeardOutcome::Failed(why.into()),
        });
        Task::none()
    }

    /// Put away what is on top, in the order Esc does, for `said`: jump
    /// mode's codes, a drop-down, the tutorial, a dialog or menu, the large Ask, Time
    /// Travel, the find bar, Ask's menu of sessions, the Ask card, a page
    /// over the code. `None` when nothing is. Where it would put Ask itself
    /// away, an answer of the agent's card called by the very words said
    /// ("Cancel", an agent's own name for declining) is given instead —
    /// put away, the card came back held, and went again said again —
    /// while anything Esc takes first still goes first, the card's answer
    /// left alone.
    fn dismiss_heard(&mut self, said: &str, targets: &[Target]) -> Option<Task<Message>> {
        let task = if self.jump.take().is_some() {
            Task::none()
        } else if self.picker.is_some() {
            self.picker = None;
            Task::none()
        } else if self.tutorial.is_some() {
            self.on_tutorial_exit()
        } else if let Some(message) = ui::escape_message(self) {
            self.update(message)
        } else if self.ask_view == AskView::Expanded
            && let Some(answer) = self.card_answer_named(said, targets)
        {
            return Some(self.give_card_answer(said, answer));
        } else if self.ask_view == AskView::Expanded && self.step_ask_down() {
            Task::none()
        } else if ui::time_travel_on_screen(self).is_some() {
            self.update(Message::TimeTravel(TimeTravelMsg::Exit))
        } else if self.proj.find.open {
            self.update(Message::Editor(EditorMsg::FindClosed))
        } else if self.ask_view.is_open()
            && let Some(answer) = self.card_answer_named(said, targets)
        {
            return Some(self.give_card_answer(said, answer));
        } else if (self.ask_view.is_open() && self.step_ask_down())
            || (ui::pane_cover(self).is_some_and(|c| c.is_page()) && self.put_away_page())
        {
            Task::none()
        } else {
            return None;
        };
        Some(self.closed_as_said(said, task))
    }

    /// The answer of the agent's card `said` names, and whether it is
    /// drawn (`targets`) or a page further: any of its answers, those
    /// given by their words alone ("Always reject") too. None while Ask's
    /// menu of sessions is open over the card: Esc takes that first.
    fn card_answer_named(&self, said: &str, targets: &[Target]) -> Option<(Target, bool)> {
        if self.ask_sessions_menu {
            return None;
        }
        let approves = |t: &Target| matches!(t.message, Message::Ask(AskMsg::Approve { .. }));
        // Named among the card's own answers: another "Cancel" on screen —
        // an update's download — made the name stand for neither.
        let drawn: Vec<Target> = targets
            .iter()
            .filter(|t| t.layer == Layer::Ask && approves(t))
            .cloned()
            .collect();
        if let Some(answer) = named(said, &drawn) {
            return Some((answer, true));
        }
        let waiting: Vec<Target> = crate::ui::answers_waiting(self)
            .into_iter()
            .filter(approves)
            .collect();
        let answer = named(said, &waiting)?;
        let does = format!("{:?}", answer.message);
        Some(
            match drawn
                .into_iter()
                .find(|t| format!("{:?}", t.message) == does)
            {
                Some(drawn) => (drawn, true),
                None => (answer, false),
            },
        )
    }

    /// Give the card's answer `said` named: pressed where it is drawn,
    /// else held and the card turned to its page
    /// ([`App::hold_for_question`]).
    fn give_card_answer(&mut self, said: &str, (answer, drawn): (Target, bool)) -> Task<Message> {
        if drawn {
            self.press_heard(said, answer)
        } else {
            self.hold_for_question(said.to_string(), ToQuestion::Answer(Box::new(answer)))
        }
    }

    /// `said` put something away, by `task`: said so on the status line and
    /// the pill.
    fn closed_as_said(&mut self, said: &str, task: Task<Message>) -> Task<Message> {
        let text = said.to_string();
        let done = tr!("Closed").to_string();
        self.status = tr!("“{text}” → {command}", text = text, command = done);
        self.voice.hear(Heard {
            text,
            outcome: HeardOutcome::Ran(done),
        });
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What is on top is put away by its name, however the putting away
    /// is said, in English or in the reader's language — and only by its
    /// own name: a question about it is no command.
    #[test]
    fn what_is_on_top_is_put_away_by_its_name() {
        for said in [
            "close the call graph",
            "Close the Call Graph.",
            "call graph, close it",
            "dismiss the call graph",
            "关闭调用图",
        ] {
            assert!(dismisses_named(said, "Call Graph"), "{said}");
        }
        for said in [
            "explain the call graph",
            "close the type graph",
            "call graph",
            "close",
            "what does the call graph show",
        ] {
            assert!(!dismisses_named(said, "Call Graph"), "{said}");
        }
        assert!(dismisses_named("close settings", "Settings"));
    }

    /// A list's place, however it is said, in each language.
    #[test]
    fn a_rows_place_is_heard_however_it_is_said() {
        for (said, n) in [
            ("open the third result", 3),
            ("the second one", 2),
            ("number 4", 4),
            ("result 12", 12),
            ("pick 5", 5),
            ("#7", 7),
            ("first", 1),
            ("twenty one", 21),
            ("ouvre le troisième résultat", 3),
            ("numéro 2", 2),
            ("第三个", 3),
            ("打开第12个", 12),
            ("第十二个", 12),
            ("3番目を開いて", 3),
            ("三番目", 3),
            ("세 번째", 3),
            ("3번", 3),
            ("첫 번째 결과", 1),
        ] {
            assert_eq!(row_asked(said), Some(RowAsked::Nth(n)), "{said:?}");
        }
        for said in ["the last one", "last", "最后一个", "마지막"] {
            assert_eq!(row_asked(said), Some(RowAsked::Last), "{said:?}");
        }
        for said in ["open the file", "line 3", "go to total", "three files", ""] {
            assert_eq!(row_asked(said), None, "{said:?}");
        }
    }

    /// A yes or a no said alone is no row's place: Korean's 네 is "yes"
    /// before it is "four" — 네 번째 is the fourth.
    #[test]
    fn a_yes_said_alone_picks_no_row() {
        use crate::i18n::{Lang, with_lang};
        let rows: Vec<Target> = (1..=5)
            .map(|i| Target {
                row: Some((0, i)),
                ..target(&format!("file {i}"), Message::Noop)
            })
            .collect();
        with_lang(Lang::Ko, || {
            for said in ["네", "네.", "네, 네"] {
                assert!(row(said, &rows).is_none(), "{said:?}");
            }
            assert_eq!(
                row("네 번째", &rows).map(|t| t.title),
                Some("file 4".to_string())
            );
            assert_eq!(
                row("세 번째", &rows).map(|t| t.title),
                Some("file 3".to_string())
            );
        });
    }

    fn target(title: &str, message: Message) -> Target {
        Target {
            title: title.into(),
            shown: title.into(),
            message,
            layer: Layer::Overlay,
            row: None,
            answer: None,
            typing: None,
            waiting: None,
            allows: false,
        }
    }

    /// A button is pressed by its name, "click" and "the button" around it
    /// or not; two buttons of one name that do different things are
    /// neither, and one offered twice is one.
    #[test]
    fn a_button_is_pressed_by_its_name() {
        let targets = [
            target("Download (120 MB)", Message::Noop),
            target("Not now", Message::Tick),
            target("Use that one", Message::QuestionWake),
        ];
        let pressed = |said: &str| named(said, &targets).map(|t| t.title);
        assert_eq!(pressed("not now").as_deref(), Some("Not now"));
        assert_eq!(pressed("Click on Not Now.").as_deref(), Some("Not now"));
        assert_eq!(
            pressed("press the use that one button").as_deref(),
            Some("Use that one")
        );
        assert_eq!(pressed("download").as_deref(), Some("Download (120 MB)"));
        assert_eq!(pressed("why is it downloading"), None);

        let twice = [
            target("Remove", Message::Noop),
            target("Remove", Message::Tick),
        ];
        assert!(named("remove", &twice).is_none());
        let same = [
            target("Settings", Message::Tick),
            target("Settings", Message::Tick),
        ];
        assert_eq!(
            named("settings", &same).map(|t| t.title).as_deref(),
            Some("Settings")
        );
    }

    /// What is said around a name to press it is said around one that
    /// holds those words itself: "select all" names "Select All".
    #[test]
    fn a_name_with_a_press_word_in_it_is_pressed_by_it() {
        let targets = [
            target("Select All", Message::Noop),
            target("Open Folder…", Message::Tick),
        ];
        let pressed = |said: &str| named(said, &targets).map(|t| t.title);
        assert_eq!(pressed("select all").as_deref(), Some("Select All"));
        assert_eq!(pressed("click select all").as_deref(), Some("Select All"));
    }

    /// What puts away what is on top is said alone: "close the minimap"
    /// is a command of its own.
    #[test]
    fn close_alone_puts_away_what_is_on_top() {
        for said in ["close", "Close it.", "never mind", "escape", "cancel"] {
            assert!(dismisses(said), "{said:?}");
        }
        for said in ["close the minimap", "close the file", ""] {
            assert!(!dismisses(said), "{said:?}");
        }
    }
}
