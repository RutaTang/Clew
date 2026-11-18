//! What is said with a value in it — a line, a file, a symbol, words to
//! look for — which no command title can name: "go to line 30", "breakpoint
//! on line 12", "search for quantity", "open cart.py", "go to total". Read
//! in the reader's language and in English, as the palette's commands are
//! (`voice::said_as_shown`).

use crate::app::prelude::*;
use crate::*;

/// What a phrase with a value asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// Go to a line of the open file.
    Line,
    /// Set or clear a breakpoint on a line of the open file.
    Breakpoint,
    /// Search the project.
    Search,
    /// Find in the open file.
    Find,
    /// Open a file by its name.
    Open,
    /// Go to a symbol by its name (or a file, when no symbol has it).
    GoTo,
    /// Scroll the open file down by lines.
    ScrollDown,
    /// Scroll the open file up by lines.
    ScrollUp,
    /// Watch an expression in the debugger.
    Watch,
    /// Stop watching one.
    Unwatch,
    /// Search the project by meaning.
    Meaning,
    /// Filter the files the sidebar lists.
    Filter,
    /// Write a walkthrough of a part of the project.
    Walk,
}

/// What says yes to a command that waits for one, and what says no: the
/// English words, and the reader's.
const YES: &str =
    tr_noop!("yes, yeah, yep, sure, okay, ok, run, run it, do it, go ahead, confirm, allow");
const NO: &str = tr_noop!("no, nope, cancel, don't, do not, never mind, not now");

/// Whether `said` answers a command waiting for a yes: `Some(true)` for a
/// yes, `Some(false)` for a no, `None` when it says something else. What it
/// opens with answers ("Yes, do it", "No, thank you", "Run it now"): the
/// longest of the yes and the no it begins with, as words of their own —
/// "not now" is not "no", and "nothing" not "no".
pub(crate) fn yes_or_no(said: &str) -> Option<bool> {
    let said = plain(said);
    let phrases = |list: &'static str| -> Vec<String> {
        [list, crate::i18n::lookup(list)]
            .into_iter()
            .flat_map(|l| l.split([',', '，', '、']))
            .map(plain)
            .filter(|p| !p.is_empty())
            .collect()
    };
    let opens = |phrase: &String| {
        said.strip_prefix(phrase.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    let longest = |list: &'static str| {
        phrases(list)
            .into_iter()
            .filter(opens)
            .map(|p| p.len())
            .max()
    };
    match (longest(YES), longest(NO)) {
        (Some(yes), Some(no)) => Some(yes > no),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}

/// Whether `said` is nothing but a yes, or nothing but a no ("yes", "yes,
/// do it", "no thanks" is not): how a question on screen is answered,
/// where what only opens with a yes ("run the tests") must not allow
/// what it asks.
pub(crate) fn only_yes_or_no(said: &str) -> Option<bool> {
    let said: Vec<String> = plain(said).split(' ').map(str::to_string).collect();
    if said.iter().all(|w| w.is_empty()) {
        return None;
    }
    let made_of = |list: &'static str| -> bool {
        let phrases: Vec<Vec<String>> = [list, crate::i18n::lookup(list)]
            .into_iter()
            .flat_map(|l| l.split([',', '，', '、']))
            .map(plain)
            .filter(|p| !p.is_empty())
            .map(|p| p.split(' ').map(str::to_string).collect())
            .collect();
        // Whether the first `i` words are made of the list's phrases.
        let mut made = vec![false; said.len() + 1];
        made[0] = true;
        for i in 0..said.len() {
            if made[i] {
                for p in &phrases {
                    if said[i..].starts_with(p) {
                        made[i + p.len()] = true;
                    }
                }
            }
        }
        made[said.len()]
    };
    match (made_of(YES), made_of(NO)) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}

/// Clew's name as whisper writes it: "Clew", the words it is heard as
/// ("clue", French "clou"), and how it is written in Chinese, Korean and
/// Japanese. "Hey, Clew" and "Bye, Clew" are said as they are in every
/// language — these are only how they come out written.
const NAMES: &[&str] = &[
    "clew",
    "clue",
    "clou",
    "克鲁",
    "克卢",
    "克露",
    "클루",
    "클류",
    "클로",
    "クルー",
    "クリュー",
];
/// Clew's name as whisper writes it in Latin letters when it is said in
/// a Japanese sentence — "Crew 設定を": Japanese says "crew" as it says
/// "Clew", "クルー". Taken for the name only in such a sentence
/// ([`in_japanese`]): "Hey crew, let's go", said to a team, is no call,
/// nor is Korean, which tells "크루" (crew) from "클루".
const NAMES_BEFORE_JAPANESE: &[&str] = &["crew"];
/// What Japanese says straight after a name it says something of —
/// "Clewのグラフ", "Clew って何", "Clew だよ", "Clew とは": not a call.
const JAPANESE_PARTICLES: &[&str] = &[
    "の",
    "が",
    "は",
    "を",
    "に",
    "へ",
    "で",
    "って",
    "から",
    "まで",
    "より",
    "だ",
    "じゃ",
    "とは",
    "とか",
    "として",
];
/// Particles that begin as many words said to Clew ("もう一回", "やめて",
/// "とめて"): particles only with no hiragana after them ("Clew も落ちた",
/// "Clew と比べて", "Clew や VS Code").
const JAPANESE_SHORT_PARTICLES: &[char] = &['と', 'も', 'や'];
/// What a Japanese sentence says to many by: after the name, it was said
/// to a crew, not to Clew, who is one ("Crew 全員集まって", "ヘイクルー
/// みんな聞いて") — but not with a particle after it that makes the many
/// what the sentence is about rather than whom it is said to ("Clew
/// みんなのファイルを開いて", "Hey Clew、みんなに共有して"
/// [`JAPANESE_ABOUT_MANY`]). Not "皆" alone, which begins other words
/// ("皆既日食").
const JAPANESE_TO_MANY: &[&str] = &[
    "全員",
    "みんな",
    "皆さん",
    "皆様",
    "みなさん",
    "各自",
    "お前ら",
    "君たち",
    "あなたたち",
];
/// The particles after a word for many ([`JAPANESE_TO_MANY`]) that make
/// it what is spoken of — whose, who does, to whom, what, where, with,
/// from or than whom — not whom the sentence is said to, as "で", "は" and
/// "も" leave it ("Crew みんなで行こう").
const JAPANESE_ABOUT_MANY: &[&str] = &["の", "が", "に", "を", "へ", "と", "から", "より"];
/// Words said to many that begin as a particle does: after a word for
/// many, they are no particle ("みんながんばって", "みんなとりあえず集合").
const JAPANESE_WORDS_AFTER_MANY: &[&str] = &[
    "がんば",
    "頑張",
    "がっかり",
    "とりあえず",
    "とにかく",
    "ともかく",
    "とても",
    "ところで",
];
/// How a Japanese request ends: "開いて", "移動して", "ください".
const JAPANESE_REQUEST_ENDS: &[&str] = &["て", "で", "ください", "下さい"];
/// The "Hey" of "Hey, Clew", as whisper writes it.
const HEY: &[&str] = &["hey", "hi", "hé", "嘿", "嗨", "헤이", "ヘイ"];
/// The "Hey" of "Hey, Clew" as whisper writes it when it hears another
/// word in it, said with what follows in another language — French "Et
/// Clew, ouvre…", Japanese "ネークルー" — or said in English: a word of its
/// own elsewhere, it is the call only first in its sentence
/// ([`named_in`]).
const HEY_HEARD_AS: &[&str] = &["et", "eh", "hay", "hei", "ネー", "ねえ", "ネイ"];
/// What is said around "Bye, Clew" to be polite, or added by whisper
/// ("Bye, Clew. Thank you."): "Okay, bye, Clew" ends listening too.
const COURTESY: &[&str] = &[
    "ok",
    "okay",
    "alright",
    "all right",
    "thanks",
    "thank you",
    "cheers",
    "好",
    "好的",
    "谢谢",
    "고마워",
    "고마워요",
    "감사합니다",
    "じゃあ",
    "ありがとう",
    "ありがとうございます",
    "ありがとうございました",
    "merci",
    "d’accord",
];
/// The "Bye" of "Bye, Clew", as whisper writes it.
const BYE: &[&str] = &[
    "bye",
    "bye bye",
    "goodbye",
    "good bye",
    "拜",
    "拜拜",
    "바이",
    "바이바이",
    "バイ",
    "バイバイ",
];

/// Whether `c` is of a script written without spaces between its words —
/// Chinese, Japanese — or with its syllables run together, as Korean is.
pub(crate) fn cjk(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF)
}

/// Whether `c` is a Chinese character, as Japanese writes them too.
fn han(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
}

/// Whether `c` is Japanese kana: hiragana or katakana.
fn kana(c: char) -> bool {
    matches!(c as u32, 0x3040..=0x30FF)
}

/// Whether `c` is hiragana.
fn hiragana(c: char) -> bool {
    matches!(c as u32, 0x3041..=0x309F)
}

/// Whether `rest`, what follows a name, is a Japanese sentence: kana in
/// it before it ends, past the marks and spaces at its start
/// ("Crew、設定を開いて", not "Clew 又卡住了。ちょっと見て").
fn japanese_follows(rest: &str) -> bool {
    first_sentence(unmarked(rest)).chars().any(kana)
}

/// The first sentence of `said`, up to its end — not a dot in a name
/// ("main.rs を開いて", "v1.2"), one a Latin letter or a number follows
/// straight after.
fn first_sentence(said: &str) -> &str {
    let mut chars = said.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let in_name = c == '.'
            && chars
                .peek()
                .is_some_and(|&(_, next)| next.is_ascii_alphanumeric());
        if sentence_end(c) && !in_name {
            return &said[..i];
        }
    }
    said
}

/// Whether a name said after `before` and before `rest` is said in a
/// Japanese sentence — one with kana, Japanese's own letters, in it:
/// before the name ("ヘイ、Crew 設定"), or after it, past the marks and
/// spaces after it but not past a sentence's end in Latin letters ("Crew
/// 設定を", not "Hey crew! 準備はいい？"); or Chinese characters after a
/// Japanese comma ("Hey Crew、設定"), or after a space with the window
/// shown in Japanese ("Hey Crew 続行"). Shown in another language, Chinese
/// characters after a space may as well be Chinese ("Hey crew 开会了"),
/// which does not write Clew so: they are not taken.
fn in_japanese(before: &str, rest: &str) -> bool {
    if last_sentence(before).chars().any(kana) {
        return true;
    }
    let after = unmarked(rest);
    let between = &rest[..rest.len() - after.len()];
    let japanese_window = crate::i18n::current() == crate::i18n::Lang::Ja;
    !between.contains(['.', '!', '?'])
        && (japanese_follows(after)
            || ((between.contains('、') || japanese_window) && after.starts_with(han)))
}

/// Where Clew's name ends, said from the start of `at`, after `before`
/// ([`name_at`]) — or written "crew" in a Japanese sentence
/// ([`NAMES_BEFORE_JAPANESE`], [`in_japanese`]) — not said to many
/// ([`JAPANESE_TO_MANY`]).
fn called_name_at(at: &str, before: &str) -> Option<usize> {
    let end = name_at(at).or_else(|| {
        let end = NAMES_BEFORE_JAPANESE
            .iter()
            .find_map(|name| begins(at, name))?;
        in_japanese(before, &at[end..]).then_some(end)
    })?;
    let next = unmarked(&at[end..]);
    let to_many = JAPANESE_TO_MANY.iter().any(|many| {
        next.strip_prefix(many).is_some_and(|rest| {
            let about = JAPANESE_ABOUT_MANY
                .iter()
                .any(|particle| rest.starts_with(particle))
                && !JAPANESE_WORDS_AFTER_MANY
                    .iter()
                    .any(|word| rest.starts_with(word));
            !about
        })
    });
    (!to_many).then_some(end)
}

/// Whether Clew's name, `name` as written, is set apart from `rest`, what
/// follows it, as a call sets it: by a mark ("Clew, …") or nothing at all
/// — or, written in Latin letters, by a pause and then a Japanese
/// sentence ("Crew 設定を", "Crew もう一回"), not one that goes on from the
/// name ("Clewのグラフ", "Clew のグラフ", "Clew だよ", "Clew も落ちた"); one
/// that begins in Latin letters or numbers, only as a request ("Crew
/// README を開いて", not "Clew VS Code より速い？"). Not by a pause alone
/// before Chinese or Korean, where it may as well be said of Clew ("Clew
/// 又卡住了"): a call there is written with a mark or a "Hey" — or in
/// Korean, "야" or "아" after the name, which a pause after it sets apart
/// ("클루야 설정 열어줘"). A pause is no sure sign: "Crew 集合して"
/// ("crew, gather"), said to others, is taken for a call too — no words
/// tell it from one; only the many it is said to, named after it, do
/// ([`JAPANESE_TO_MANY`]).
fn set_apart(name: &str, rest: &str) -> bool {
    let after = rest.trim_start();
    // Korean calls with "야" or "아" after the name ([`name_at`]): a pause
    // after it sets it apart ("클루야 설정 열어줘").
    let called = name.ends_with(['야', '아']) && rest.starts_with(char::is_whitespace);
    if called || after.chars().next().is_none_or(mark) {
        return true;
    }
    if name.chars().any(cjk) || !rest.starts_with(char::is_whitespace) || !japanese_follows(rest) {
        return false;
    }
    let particle = JAPANESE_PARTICLES.iter().any(|p| after.starts_with(p))
        || after
            .strip_prefix(JAPANESE_SHORT_PARTICLES)
            .is_some_and(|word| !word.starts_with(hiragana));
    !particle && (after.starts_with(cjk) || japanese_request(after))
}

/// Whether the sentence `said` begins with is a Japanese request —
/// "…を開いて", "…移動して", "…ください" — or one of the phrases Clew is
/// told a value with, as the window says them ("{text}を検索",
/// [`PHRASES`]), the value a name or a number — katakana in it too
/// ("READMEファイル") — not a Japanese clause, which hiragana tell
/// ("2 回同じファイルを開く", "go to definition が効かない").
fn japanese_request(said: &str) -> bool {
    let sentence = first_sentence(said).trim_end_matches(|c: char| c.is_whitespace() || mark(c));
    JAPANESE_REQUEST_ENDS
        .iter()
        .any(|end| sentence.ends_with(end))
        || PHRASES.iter().any(|&(_, phrase)| {
            value_in(crate::i18n::lookup(phrase), sentence)
                .is_some_and(|value| !value.chars().any(hiragana))
        })
}

/// What sets one part of what was said from the next.
fn mark(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | '!'
            | '?'
            | ':'
            | ';'
            | '-'
            | '—'
            | '…'
            | '，'
            | '。'
            | '！'
            | '？'
            | '、'
            | '：'
            | '・'
    )
}

/// What ends a sentence.
fn sentence_end(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '。' | '！' | '？')
}

/// Sounds a Chinese transliteration goes on with after 克鲁 — "克鲁斯"
/// (Cruz), "克鲁兹", "克鲁格" (Kruger), "克鲁尼" (Clooney): another name.
const NAME_GOES_ON_IN_CHINESE: &str = "斯兹泽格尼姆";

/// Where Clew's name ends, said from the start of `at`, if it is said
/// there as a word of its own: not the start of a longer word — "clueless",
/// Japanese "クルーズ" (cruise), Korean "클로이" (Chloe), Chinese "克鲁斯"
/// (Cruz). Korean may call it with "야" or "아" after it ("클루야", or
/// "Clew야" as whisper may write it, told the name): the name, called
/// so, ends after it. Whisper writes a call with a mark or a
/// space after the name ("ネークルー、", "嘿，克鲁，", "헤이클루 호출"):
/// one run straight on into more of its script is taken for a longer word
/// — the cost, a call written "嘿克鲁格式化…", is missed, and said again.
fn name_at(at: &str) -> Option<usize> {
    NAMES.iter().find_map(|name| {
        let end = begins(at, name)?;
        let last = name.chars().last()?;
        let next = at[end..].chars().next();
        if let Some(called @ ('야' | '아')) = next
            && (matches!(last as u32, 0xAC00..=0xD7AF) || last.is_ascii_alphabetic())
        {
            return Some(end + called.len_utf8());
        }
        // "Clew's graph", "Clew’s slow": said of Clew, not to it.
        if last.is_ascii_alphabetic()
            && at[end..]
                .strip_prefix(['\'', '’'])
                .is_some_and(|rest| rest.starts_with(char::is_alphabetic))
        {
            return None;
        }
        let goes_on = next.is_some_and(|c| match last as u32 {
            // Katakana, and its long vowel mark: "クルーズ" (cruise),
            // "クルーネック" (crew-neck).
            0x30A1..=0x30FC => matches!(c as u32, 0x30A1..=0x30FA | 0x30FC),
            0xAC00..=0xD7AF => matches!(c as u32, 0xAC00..=0xD7AF),
            0x4E00..=0x9FFF => NAME_GOES_ON_IN_CHINESE.contains(c),
            _ => false,
        });
        (!goes_on).then_some(end)
    })
}

/// `said` without the marks and spaces at its start.
fn unmarked(said: &str) -> &str {
    said.trim_start_matches(|c: char| c.is_whitespace() || mark(c))
}

/// How much of the start of `said` is `phrase`, case aside — a phrase of
/// Latin letters only as whole words: "clue" does not begin "clueless".
fn begins(said: &str, phrase: &str) -> Option<usize> {
    let mut chars = said.char_indices();
    let mut end = 0;
    for p in phrase.chars() {
        let (i, c) = chars.next()?;
        if !c.to_lowercase().eq(p.to_lowercase()) {
            return None;
        }
        end = i + c.len_utf8();
    }
    let ends_word = phrase.chars().last().is_some_and(|c| !cjk(c));
    let next = said[end..].chars().next();
    if ends_word && next.is_some_and(|c| c.is_alphanumeric() && !cjk(c)) {
        return None;
    }
    Some(end)
}

/// Whether `said` is nothing but "Bye, Clew" — once or more, "Bye-bye,
/// Clew", "Goodbye, Clew", with an "okay" or "thank you" around it
/// ([`COURTESY`]) — not a "bye" said to someone else in the room, nor "say
/// bye to Clew", nor "Thanks, Clew". Matched with its spaces and marks
/// left out, so that Chinese and Japanese, written without them, are heard
/// too ("拜拜克鲁").
pub(crate) fn stops_listening(said: &str) -> bool {
    let compact = |text: &str| -> String {
        let mut text: String = text
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        // "Bye, Crew" too, as whisper may write it said by a Japanese
        // speaker ([`NAMES_BEFORE_JAPANESE`]): said to a team, it only
        // ends listening.
        for name in NAMES.iter().chain(NAMES_BEFORE_JAPANESE) {
            text = text.replace(name, "clew");
        }
        // Korean's calling "야" or "아" after it ("바이 클루야").
        text.replace("clew야", "clew").replace("clew아", "clew")
    };
    let said: Vec<char> = compact(said).chars().collect();
    if said.is_empty() {
        return false;
    }
    let byes: Vec<Vec<char>> = BYE
        .iter()
        .map(|bye| compact(&format!("{bye} clew")).chars().collect())
        .collect();
    let courtesies: Vec<Vec<char>> = COURTESY
        .iter()
        .map(|c| compact(c).chars().collect())
        .collect();
    // Made, from its start, of nothing but those phrases: up to each
    // place, whether it can be (`Some`), and whether with a goodbye.
    let mut made: Vec<Option<bool>> = vec![None; said.len() + 1];
    made[0] = Some(false);
    for i in 0..said.len() {
        let Some(bye) = made[i] else { continue };
        for (phrases, is_bye) in [(&byes, true), (&courtesies, false)] {
            for p in phrases {
                if said[i..].starts_with(p) {
                    let next = &mut made[i + p.len()];
                    *next = Some(next.unwrap_or(false) || bye || is_bye);
                }
            }
        }
    }
    made[said.len()] == Some(true)
}

/// What words heard say to Clew: those after its name, when they `call` it
/// — what was said before the call is not what was asked
/// ([`after_name`]) — else the words without its name before them
/// ([`without_name`]).
pub(crate) fn what_is_said(text: &str, call: bool) -> &str {
    if call {
        after_name(text)
    } else {
        without_name(text)
    }
}

/// Whether words heard say "Bye, Clew" ([`stops_listening`]): all of
/// them, or all that is said to Clew ([`what_is_said`]) — "Hey, Clew.
/// Bye, Clew." What they do to Clew's speech and to the microphone is
/// decided by this alone, wherever they are heard.
pub(crate) fn says_bye(text: &str, call: bool) -> bool {
    stops_listening(text) || stops_listening(what_is_said(text, call))
}

/// Whether words heard, said over Clew, leave it speaking: "Bye, Clew",
/// which ends listening, not what Clew says — or the microphone's label,
/// which only says what does ([`says_stop_listening`]).
pub(crate) fn leaves_clew_speaking(text: &str, call: bool) -> bool {
    says_bye(text, call) || says_stop_listening(what_is_said(text, call))
}

/// The microphone button's label while it listens.
const STOP_LISTENING: &str = tr_noop!("Stop listening");

/// Whether `said` is nothing but the microphone button's label while it
/// listens — "Stop listening", in English or as the window shows it —
/// which is not how listening is stopped: "Bye, Clew" is.
pub(crate) fn says_stop_listening(said: &str) -> bool {
    // Its name said after it too: "Stop listening, Clew."
    let said = plain(said);
    let said = NAMES
        .iter()
        .find_map(|name| {
            said.strip_suffix(name)
                .filter(|rest| rest.ends_with(' ') || rest.chars().last().is_some_and(cjk))
        })
        .map_or(said.as_str(), str::trim_end);
    !said.is_empty()
        && [STOP_LISTENING, crate::i18n::lookup(STOP_LISTENING)]
            .iter()
            .any(|label| plain(label) == said)
}

/// `said` without Clew called by its name before it: "Hey, Clew, open
/// Settings" is "open Settings", "Clew, what does this do?" is "what does
/// this do?" — what was said as it was said, its case and marks kept. The
/// name alone is a call only set apart from what follows as a call is
/// ([`set_apart`]): with a mark after it, or a pause before a Japanese
/// sentence — "Clue me in" is not one.
pub(crate) fn without_name(said: &str) -> &str {
    let start = said.trim_start();
    // Called, then named — the "Hey" as whisper may have heard it too
    // ("Et Clew, ouvre…").
    for hey in HEY.iter().chain(HEY_HEARD_AS) {
        if let Some(end) = begins(start, hey) {
            let at = unmarked(&start[end..]);
            if let Some(name) = called_name_at(at, &start[..end]) {
                return unmarked(&at[name..]);
            }
        }
    }
    // Named alone, set apart from what follows.
    if let Some(end) = called_name_at(start, "")
        && set_apart(&start[..end], &start[end..])
    {
        return unmarked(&start[end..]);
    }
    said
}

/// What was said after Clew's name, wherever it was said in `said` —
/// what was heard just before the call is not what was asked; `said` as
/// [`without_name`] leaves it when the name is not in it.
pub(crate) fn after_name(said: &str) -> &str {
    named_in(said).unwrap_or_else(|| without_name(said))
}

/// What follows Clew's name in `said`, if Clew is called by it there —
/// the name a word of its own ([`name_at`]: "clueless" holds no "clue",
/// nor "クルーズ" "クルー"), said as a call is: after the "Hey" of "Hey,
/// Clew" in its sentence ("hey", "嘿"), or first in its sentence with a
/// mark after it ("Clew, …", not "Clew keeps crashing"), or as a Japanese
/// sentence may write it ([`set_apart`], [`called_name_at`]: "Crew
/// 設定を"); else first in a sentence after a "Hey" whisper wrote as
/// another word ("Et Clew, ouvre…" for "Hey,
/// Clew" said in French, "ネークルー") — a call the first way, later in
/// `said`, wins over one heard this way ("Et clou. Hey, Clew, run it").
/// Not after other words: "the clue is", "this is Clew's graph", "a
/// clue", nor after a "hi" that ended the sentence before ("I said hi.
/// Clew crashed again").
pub(crate) fn named_in(said: &str) -> Option<&str> {
    let mut heard_as_another: Option<&str> = None;
    let mut last: Option<char> = None;
    for (i, c) in said.char_indices() {
        let at_word = last.is_none_or(|l| !l.is_alphanumeric() || cjk(l) || cjk(c));
        last = Some(c);
        if !at_word {
            continue;
        }
        let Some(end) = called_name_at(&said[i..], &said[..i]) else {
            continue;
        };
        let rest = &said[i + end..];
        let apart = set_apart(&said[i..i + end], rest);
        // What was said before it in its sentence: a sentence ended just
        // before it is not its call — "I said hi. Clew crashed" — unless
        // the call was all that sentence said ("Hey. Clew, …").
        let ended = said[..i].trim_end().ends_with(sentence_end);
        let before = said[..i].trim_end_matches(|c: char| c.is_whitespace() || mark(c));
        let sentence = last_sentence(before).to_lowercase();
        let first = before.is_empty() || ended;
        let after_hey = HEY.iter().any(|hey| {
            if ended {
                sentence == *hey
            } else {
                ends_with_word(before, hey)
            }
        });
        if (first && apart) || after_hey {
            return Some(unmarked(rest));
        }
        if heard_as_another.is_none() && call_word_heard_as_another(before) {
            heard_as_another = Some(unmarked(rest));
        }
    }
    heard_as_another
}

/// The last sentence of `text`, without the marks and spaces around it.
fn last_sentence(text: &str) -> &str {
    text.rsplit(sentence_end)
        .next()
        .unwrap_or(text)
        .trim_matches(|c: char| c.is_whitespace() || mark(c))
}

/// Whether `before`, what was said before Clew's name, is nothing but the
/// "Hey" of "Hey, Clew" as whisper writes it when it hears another word in
/// it ([`HEY_HEARD_AS`]), first in its sentence — "Et" said in a sentence,
/// as in "Paul et Clew", is no call.
fn call_word_heard_as_another(before: &str) -> bool {
    HEY_HEARD_AS.contains(&last_sentence(before).to_lowercase().as_str())
}

/// Whether `text` ends with `word` (lower-cased), case aside, as a word of
/// its own.
fn ends_with_word(text: &str, word: &str) -> bool {
    let text = text.to_lowercase();
    let Some(start) = text.len().checked_sub(word.len()) else {
        return false;
    };
    if !text.is_char_boundary(start) || text[start..] != *word {
        return false;
    }
    let first = word.chars().next().is_some_and(cjk);
    text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| first || cjk(c) || !c.is_alphanumeric())
}

/// The phrases said with a value, in the order they are tried — after the
/// palette's titles, so that a title wins: said with its value, a phrase
/// is no title's words (the extra word is the value), and a translation
/// beginning as a title does ("rechercher {text}" and "Rechercher les
/// références") does not take the title's words for its value.
const PHRASES: &[(Kind, &str)] = &[
    (Kind::Breakpoint, tr_noop!("toggle breakpoint on line {n}")),
    (Kind::Breakpoint, tr_noop!("set a breakpoint on line {n}")),
    (Kind::Breakpoint, tr_noop!("breakpoint on line {n}")),
    (Kind::Line, tr_noop!("go to line {n}")),
    (Kind::Line, tr_noop!("line {n}")),
    (Kind::Meaning, tr_noop!("search by meaning for {text}")),
    (Kind::Meaning, tr_noop!("find code that {text}")),
    (Kind::Filter, tr_noop!("filter the files by {text}")),
    (Kind::Filter, tr_noop!("filter files by {text}")),
    (Kind::Walk, tr_noop!("walk me through {text}")),
    (Kind::Walk, tr_noop!("write a walkthrough of {text}")),
    (Kind::Search, tr_noop!("search the project for {text}")),
    (Kind::Search, tr_noop!("search for {text}")),
    (Kind::ScrollDown, tr_noop!("scroll down {n}")),
    (Kind::ScrollUp, tr_noop!("scroll up {n}")),
    (Kind::Unwatch, tr_noop!("stop watching {text}")),
    (Kind::Unwatch, tr_noop!("remove watch {text}")),
    (Kind::Watch, tr_noop!("add a watch for {text}")),
    (Kind::Watch, tr_noop!("add watch {text}")),
    (Kind::Watch, tr_noop!("watch {text}")),
    (Kind::Find, tr_noop!("find {text}")),
    (Kind::Open, tr_noop!("open {name}")),
    (Kind::GoTo, tr_noop!("go to {name}")),
    (Kind::GoTo, tr_noop!("jump to {name}")),
    (Kind::GoTo, tr_noop!("show {name}")),
];

/// What a phrase came to, said for the reader.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Done {
    /// It was done.
    Did(String),
    /// It could not be, and why ("This file has no line 90").
    Cannot(String),
}

/// `said` done, by `task`.
fn did(said: impl Into<String>, task: Task<Message>) -> Option<(Done, Task<Message>)> {
    Some((Done::Did(said.into()), task))
}

impl App {
    /// Do what `said` asks with a value in it, if it is one of the
    /// [`PHRASES`]: what it came to, said for the reader, and the task doing
    /// it. `None` leaves it to Ask.
    pub(crate) fn spoken(&mut self, said: &str) -> Option<(Done, Task<Message>)> {
        PHRASES
            .iter()
            .find_map(|&(kind, template)| {
                let translated = crate::i18n::lookup(template);
                [translated, template]
                    .into_iter()
                    .find_map(|t| value_in(t, said))
                    .and_then(|value| self.do_spoken(kind, &value))
            })
            .or_else(|| self.spoken_glued(said))
    }

    /// "openpricing.py": a name the recognizer ran into the word before it
    /// — as whisper does with a file's name — opens it as "open pricing.py"
    /// would. Only a name to open or go to, and only one the project has:
    /// "opener" is still Ask's.
    fn spoken_glued(&mut self, said: &str) -> Option<(Done, Task<Message>)> {
        let kept = said_form(said, true);
        let lower = kept.to_lowercase();
        PHRASES
            .iter()
            .filter(|(kind, _)| matches!(kind, Kind::Open | Kind::GoTo))
            .find_map(|&(kind, template)| {
                let translated = crate::i18n::lookup(template);
                [translated, template]
                    .into_iter()
                    .find_map(|t| {
                        let (head, rest) = t.split_once('{')?;
                        // A template that ends at its name, and a head that
                        // ends in a space for the name to run into.
                        if !rest.split_once('}')?.1.trim().is_empty() || !head.ends_with(' ') {
                            return None;
                        }
                        let head = plain(head);
                        let rest = lower.strip_prefix(head.as_str())?;
                        if rest.is_empty() || rest.starts_with(' ') {
                            return None;
                        }
                        let value =
                            if kept.len() == lower.len() && kept.is_char_boundary(head.len()) {
                                &kept[head.len()..]
                            } else {
                                rest
                            };
                        Some(value.trim().to_string())
                    })
                    .and_then(|value| self.do_spoken(kind, &value))
            })
    }

    /// Do `kind` with `value`; `None` when it cannot be done here (no file
    /// open, no line number, nothing of the name).
    fn do_spoken(&mut self, kind: Kind, value: &str) -> Option<(Done, Task<Message>)> {
        match kind {
            Kind::Line => {
                let line = number(value)?;
                self.active_viewer()?;
                let task = self.update(Message::Editor(EditorMsg::GotoLineNumber(line)));
                did(tr!("Line {n}", n = line), task)
            }
            Kind::Breakpoint => {
                let line = number(value)?;
                let v = self.active_viewer()?;
                // Past the end — a number misheard, as often as not — no
                // breakpoint where no line is: said so instead.
                if line > v.lines.len() {
                    return Some((
                        Done::Cannot(tr!("This file has no line {n}", n = line)),
                        Task::none(),
                    ));
                }
                let path = v.abs.clone();
                let task = self.update(Message::Debug(DebugMsg::BreakpointToggle { path, line }));
                did(tr!("Breakpoint on line {n}", n = line), task)
            }
            Kind::Search => {
                self.proj.project.as_ref()?;
                self.proj.search.query = value.to_string();
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Search,
                )));
                let run = self.update(Message::Nav(NavMsg::SearchSubmitted));
                did(
                    tr!("Search for “{text}”", text = value),
                    Task::batch([shown, run]),
                )
            }
            Kind::Find => {
                // Words to find, not a request: "find where the total is
                // worked out" is Ask's.
                if value.split_whitespace().count() > 3 {
                    return None;
                }
                self.active_viewer()?;
                let open = self.update(Message::Editor(EditorMsg::FindOpened));
                let find = self.update(Message::Editor(EditorMsg::FindQueryChanged(
                    value.to_string(),
                )));
                did(
                    tr!("Find “{text}” in this file", text = value),
                    Task::batch([open, find]),
                )
            }
            Kind::Open => self.open_named(value),
            Kind::GoTo => self.go_to_named(value).or_else(|| self.open_named(value)),
            Kind::ScrollDown | Kind::ScrollUp => {
                let lines = i32::try_from(number(value)?).ok()?;
                self.active_viewer()?;
                let (title, by) = if kind == Kind::ScrollDown {
                    (tr_noop!("Scroll Down"), lines)
                } else {
                    (tr_noop!("Scroll Up"), -lines)
                };
                let task = self.update(Message::Editor(EditorMsg::ScrollLines(by)));
                did(crate::i18n::lookup(title), task)
            }
            Kind::Watch => {
                self.proj.project.as_ref()?;
                // What was being typed in the watch field stays there.
                let typed = std::mem::replace(&mut self.debug.watch_input, value.to_string());
                let task = self.update(Message::Debug(DebugMsg::WatchAdd));
                self.debug.watch_input = typed;
                did(tr!("Watching “{text}”", text = value), task)
            }
            Kind::Meaning => {
                self.proj.project.as_ref()?;
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Semantic,
                )));
                let typed = self.update(Message::Semantic(SemanticMsg::QueryChanged(
                    value.to_string(),
                )));
                let run = self.update(Message::Semantic(SemanticMsg::Search));
                did(
                    tr!("Search by meaning for “{text}”", text = value),
                    Task::batch([shown, typed, run]),
                )
            }
            Kind::Filter => {
                self.proj.project.as_ref()?;
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Files,
                )));
                let typed = self.update(Message::Window(WindowMsg::FileFilterChanged(
                    value.to_string(),
                )));
                did(
                    tr!("Files filtered by “{text}”", text = value),
                    Task::batch([shown, typed]),
                )
            }
            Kind::Walk => {
                self.proj.project.as_ref()?;
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Walk,
                )));
                let run = self.update(Message::Walk(WalkMsg::Generate(value.to_string())));
                did(
                    tr!("Walkthrough of “{text}”", text = value),
                    Task::batch([shown, run]),
                )
            }
            Kind::Unwatch => {
                let wanted = squeezed(value);
                let expr = self
                    .debug
                    .watches
                    .iter()
                    .find(|w| squeezed(w) == wanted)?
                    .clone();
                let task = self.update(Message::Debug(DebugMsg::WatchRemoveExpr(expr.clone())));
                did(tr!("Stopped watching “{text}”", text = expr), task)
            }
        }
    }

    /// Open the project's file `name` says: the best of those whose path
    /// holds every word said ("cart dot py", "the cart file" — `cart.py`).
    fn open_named(&mut self, name: &str) -> Option<(Done, Task<Message>)> {
        let words = file_words(name);
        if words.is_empty() {
            return None;
        }
        let files = self.proj.project.as_ref()?.files.clone();
        // The best-ranked file whose path has every word said as one of its
        // own — "it" is not a word of `gitops.rs`, nor "end" of `backend.rs`
        // — or, `by_name`, whose name does, its extension aside.
        let find = |words: &[String], by_name: bool| {
            let said: Vec<String> = words.iter().flat_map(|w| tokens(w)).collect();
            if said.is_empty() {
                return None;
            }
            crate::session::finder::rank(
                &said.join(" "),
                files.iter().map(|f| f.rel.as_str()),
                8,
                true,
            )
            .into_iter()
            .find(|&i| {
                let rel = files[i].rel.as_str();
                let name = rel.rsplit('/').next().unwrap_or(rel);
                let own = if by_name {
                    tokens(name.split('.').next().unwrap_or(name))
                } else {
                    tokens(rel)
                };
                said.iter().all(|w| own.contains(w))
            })
        };
        // An extension misheard ("cart dot pie" heard as `cart.ide`): the
        // file of that name.
        let stems: Vec<String> = words
            .iter()
            .filter_map(|w| w.split('.').next().filter(|s| !s.is_empty()))
            .map(str::to_string)
            .collect();
        let best = find(&words, false).or_else(|| {
            (stems != words && !stems.is_empty())
                .then(|| find(&stems, true))
                .flatten()
        })?;
        let rel = files[best].rel.clone();
        let task = self.update(Message::Editor(EditorMsg::OpenRel {
            rel: rel.clone(),
            line: None,
        }));
        did(rel, task)
    }

    /// Go to the symbol `name` says, spelled as said or run together
    /// ("apply discount" — `apply_discount`, `applyDiscount`).
    fn go_to_named(&mut self, name: &str) -> Option<(Done, Task<Message>)> {
        let wanted = squeezed(name);
        if wanted.is_empty() {
            return None;
        }
        let symbols = self.fresh_symbol_index();
        let entry = symbols
            .iter()
            .filter(|s| squeezed(&s.name) == wanted)
            // A definition over a test of the same name, then the shortest path.
            .min_by_key(|s| (s.is_test, s.rel.len()))?
            .clone();
        let task = self.update(Message::Nav(NavMsg::FinderPick {
            abs: entry.abs.clone(),
            line: Some(entry.line),
            name: Some(entry.name.clone()),
        }));
        did(entry.name, task)
    }
}

/// The value `said` gives `template`'s placeholder (`{n}`, `{text}`,
/// `{name}`), when it is the template said with a value: the words before
/// and after it as they are, ends of sentences and commas aside, in either
/// case. The value is as it was said, its case kept (`watch Cart`); words a
/// recognizer runs together or splits are mended only when the phrase is
/// not read without it ("breakpoint online 6"), never in a value read so
/// ("search for online orders").
fn value_in(template: &str, said: &str) -> Option<String> {
    value_read(template, said, false).or_else(|| value_read(template, said, true))
}

/// [`value_in`], `said`'s words `mend`ed or not.
fn value_read(template: &str, said: &str, mend: bool) -> Option<String> {
    let (head, rest) = template.split_once('{')?;
    let (_, tail) = rest.split_once('}')?;
    let kept = said_form(said, mend);
    let lower = kept.to_lowercase();
    let (head, tail) = (plain(head), plain(tail));
    let rest = lower.strip_prefix(head.as_str())?;
    // A word of its own: "open" is not the start of "opener".
    let joined = |a: Option<char>, b: Option<char>| {
        a.zip(b)
            .is_some_and(|(a, b)| a.is_ascii_alphanumeric() && b.is_ascii_alphanumeric())
    };
    if joined(head.chars().last(), rest.chars().next()) {
        return None;
    }
    let value = rest.strip_suffix(tail.as_str())?;
    if joined(value.chars().last(), tail.chars().next()) {
        return None;
    }
    // Where it is in what was said, its case kept — when lowercasing kept
    // every letter's length, as it does but for a few.
    let (start, end) = (head.len(), head.len() + value.len());
    let value = if kept.len() == lower.len()
        && kept.is_char_boundary(start)
        && kept.is_char_boundary(end)
    {
        &kept[start..end]
    } else {
        value
    };
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `text` lowercase, without what ends or opens a sentence, its commas as
/// spaces, its spaces single, "please" said around it dropped, and the
/// words a recognizer runs together or splits mended.
fn plain(text: &str) -> String {
    said_form(text, true).to_lowercase()
}

/// [`plain`] with the case kept, mended (`mend`) or not.
fn said_form(text: &str, mend: bool) -> String {
    let text: String = text
        .replace('’', "'")
        .chars()
        .map(|c| {
            // A comma, or a sentence's end where no space follows it, as
            // Chinese and Japanese write it ("はい。お願いします").
            if matches!(c, ',' | '，' | '、' | '。' | '！' | '？') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let ends = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '.' | '!' | '?' | '。' | '！' | '？' | '"' | '“' | '”' | '…'
            )
    };
    // A sentence's end before a space ends its word too ("Yes. Go
    // ahead."): not a dot in a name ("main.rs", "v1.2").
    let said: Vec<&str> = text
        .trim_matches(ends)
        .split_whitespace()
        .map(|word| match word.trim_end_matches(['.', '!', '?']) {
            "" => word,
            trimmed => trimmed,
        })
        .collect();
    let mut words: Vec<&str> = Vec::with_capacity(said.len());
    let mut i = 0;
    while i < said.len() {
        let word = said[i];
        // "breakpoint online 6" is "breakpoint on line 6"; "break point",
        // "breakpoint".
        if mend && word.eq_ignore_ascii_case("online") {
            words.extend(["on", "line"]);
        } else if mend
            && word.eq_ignore_ascii_case("break")
            && said
                .get(i + 1)
                .is_some_and(|w| w.eq_ignore_ascii_case("point"))
        {
            words.push("breakpoint");
            i += 1;
        } else {
            words.push(word);
        }
        i += 1;
    }
    // "Please" around it.
    if words
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("please"))
        && words.len() > 1
    {
        words.remove(0);
    }
    if words
        .last()
        .is_some_and(|w| w.eq_ignore_ascii_case("please"))
        && words.len() > 1
    {
        words.pop();
    }
    words.join(" ")
}

/// A line number said: in figures ("30", "#30") or in English words ("thirty
/// five").
pub(crate) fn number(text: &str) -> Option<usize> {
    let text = text.trim().trim_start_matches('#').to_lowercase();
    let text = text.as_str();
    // "10 lines": the count said with what it counts.
    let text = ["lines", "line"]
        .iter()
        .find_map(|unit| text.strip_suffix(unit))
        .map_or(text, str::trim_end);
    if let Ok(n) = text.parse::<usize>() {
        return (n > 0).then_some(n);
    }
    const ONES: [&str; 20] = [
        "zero",
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = [
        "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
    ];
    let mut total = 0;
    let mut current = 0;
    let mut any = false;
    for word in text
        .split([' ', '-'])
        .filter(|w| !w.is_empty() && *w != "and")
    {
        if let Some(n) = ONES.iter().position(|w| *w == word) {
            current += n;
        } else if let Some(n) = TENS.iter().position(|w| *w == word) {
            current += (n + 2) * 10;
        } else if word == "hundred" {
            current = current.max(1) * 100;
        } else if word == "thousand" {
            total += current.max(1) * 1000;
            current = 0;
        } else if word == "a" {
            continue;
        } else {
            return None;
        }
        any = true;
    }
    let n = total + current;
    (any && n > 0).then_some(n)
}

/// The words of a file's name as said: "dot" said as the dot it is, and
/// what only says it is a file left out ("the cart file" — `cart`).
fn file_words(name: &str) -> Vec<String> {
    let name = name.to_lowercase();
    let name = name.replace(" dot ", ".").replace(" point ", ".");
    name.split_whitespace()
        .filter(|w| {
            !matches!(
                *w,
                "the" | "a" | "an" | "file" | "this" | "that" | "it" | "me" | "them" | "one"
            )
        })
        .map(str::to_string)
        .collect()
}

/// The words a path or a name is made of, lowercase: split at whatever is
/// not a letter or a figure, and where a lowercase letter meets a capital
/// (`src/CartView.tsx` — src, cart, view, tsx).
fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut last_lower = false;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            last_lower = false;
            continue;
        }
        if c.is_uppercase() && last_lower && !word.is_empty() {
            out.push(std::mem::take(&mut word));
        }
        last_lower = c.is_lowercase();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// `name` run together, lowercase, with nothing but letters and figures:
/// how a symbol said and a symbol written are told to be the same.
fn squeezed(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Bye, Clew" stops it, once or twice, and Clew heard as "Clue"; a
    /// "bye" without its name, "Stop listening", or the words in something
    /// else said, do not.
    #[test]
    fn stopping_listening_is_said() {
        for said in [
            "Bye, Clew!",
            "Bye, Clew! Bye, Clew.",
            "goodbye clue",
            "Bye-bye, Clew.",
            "Good bye, Clew",
            "Bye, Clou.",
            "Okay, bye, Clew.",
            "Thanks, bye Clew",
            "Bye, Clew. Thank you.",
            "All right. Bye-bye, Clew!",
            "好的，拜拜 Clew",
            "バイバイ、クルー。ありがとう",
            "バイバイ、クルー。ありがとうございました。",
            "Bye, Clew. Merci.",
            "D’accord, bye Clew !",
        ] {
            assert!(stops_listening(said), "{said}");
        }
        for said in [
            "",
            "bye",
            "Stop listening.",
            "Bye, Clew! Stop listening.",
            "Stop listening to the test output",
            "say bye to Clew",
            "Clew",
            "Hey, Clew",
            "stop",
            "Thanks, Clew",
            "Okay",
            "Thank you.",
            "Okay, bye.",
            "Bye, Clew. Thank you so much.",
        ] {
            assert!(!stops_listening(said), "{said}");
        }
    }

    /// "Bye, Clew" said after the call, in one breath, is a goodbye too.
    #[test]
    fn a_goodbye_after_the_call_is_one() {
        assert!(says_bye("Hey, Clew. Bye, Clew.", true));
        assert!(says_bye("Bye, Clew.", false));
        assert!(!says_bye("Hey, Clew, open settings.", true));
        assert_eq!(what_is_said("Hey, Clew, open it", true), "open it");
        assert_eq!(what_is_said("Clew, open it", false), "open it");
    }

    /// The microphone's label said, alone, in English or as the window
    /// shows it — not words it is part of.
    #[test]
    fn the_microphones_label_is_heard_as_itself() {
        use crate::i18n::{Lang, with_lang};
        assert!(says_stop_listening("Stop listening."));
        assert!(says_stop_listening("stop listening"));
        assert!(says_stop_listening("Stop listening, Clew."));
        assert!(!says_stop_listening("Clew"));
        assert!(!says_stop_listening("Stop listening to the test output"));
        assert!(!says_stop_listening("Bye, Clew"));
        assert!(!says_stop_listening(""));
        with_lang(Lang::Zh, || {
            assert!(says_stop_listening("停止聆听。"));
            assert!(says_stop_listening("Stop listening"));
        });
    }

    /// "Bye, Clew" is said as it is in every language, whichever Clew is
    /// shown in: as whisper writes it in each, it stops listening — and a
    /// goodbye in the language's own words does not.
    #[test]
    fn bye_clew_stops_listening_in_each_language() {
        use crate::i18n::{Lang, with_lang};
        for lang in Lang::ALL {
            with_lang(lang, || {
                for said in [
                    "Bye, Clew.",
                    "拜拜，克鲁。",
                    "拜拜Clew！",
                    "バイバイ、クルー。",
                    "バイ、Clew。",
                    "바이 클루.",
                    "바이 클루야.",
                    "Bye, Clew야.",
                    "Bye, Crew.",
                ] {
                    assert!(stops_listening(said), "{lang:?}: {said}");
                }
                for said in [
                    "再见，Clew。",
                    "停止聆听。",
                    "Au revoir, Clew.",
                    "잘 가, Clew.",
                    "さようなら、クルー。",
                    "拜拜",
                    "Bye, Clew. 아, 잠깐",
                    "바이 클루 아니야",
                ] {
                    assert!(!stops_listening(said), "{lang:?}: {said}");
                }
            });
        }
    }

    /// Clew called by its name before what is said to it: the name, and
    /// the call before it, are not what was asked.
    #[test]
    fn what_is_said_to_clew_is_without_its_name() {
        assert_eq!(without_name("Hey, Clew, open Settings."), "open Settings.");
        assert_eq!(
            without_name("Hey Clue. What does total do?"),
            "What does total do?"
        );
        assert_eq!(without_name("Clew, go to line 30"), "go to line 30");
        assert_eq!(without_name("Hey, Clew."), "");
        assert_eq!(without_name("Hi Clew — next"), "next");
        // A "Hey" whisper heard as another word.
        assert_eq!(
            without_name("Et Clew, ouvre les paramètres"),
            "ouvre les paramètres"
        );
        assert_eq!(without_name("Hay Clew, open settings"), "open settings");
        assert_eq!(without_name("Paul et Clew"), "Paul et Clew");
        // Set apart as a call to it is, in a Japanese sentence too.
        assert_eq!(without_name("Crew 設定を開いて"), "設定を開いて");
        assert_eq!(without_name("Clew、設定を開いて"), "設定を開いて");
        assert_eq!(without_name("Clewのグラフが遅い"), "Clewのグラフが遅い");
        assert_eq!(without_name("Clew 又卡住了"), "Clew 又卡住了");
        assert_eq!(without_name("ヘイ、Crew 設定"), "設定");
        assert_eq!(without_name("ヘイ、Crew。"), "");
        assert_eq!(without_name("Clew とは何ですか？"), "Clew とは何ですか？");
        assert_eq!(without_name("Hey crew, let's go"), "Hey crew, let's go");
        // Not called: left as it is.
        assert_eq!(without_name("Hey, look at this"), "Hey, look at this");
        assert_eq!(without_name("Open the clew file"), "Open the clew file");
        assert_eq!(without_name("A clue is in the log"), "A clue is in the log");
        assert_eq!(without_name("Clue me in on this"), "Clue me in on this");
        assert_eq!(without_name("Clew!"), "");
        // Chinese and Japanese, written without spaces: the name heard in
        // them, call and all.
        assert_eq!(without_name("嘿克鲁，打开设置"), "打开设置");
        assert_eq!(without_name("嘿 Clew 打开设置"), "打开设置");
        assert_eq!(without_name("克鲁是什么"), "克鲁是什么", "no mark: no call");
        assert_eq!(without_name("ヘイクルー、設定を開いて"), "設定を開いて");
        assert!(stops_listening("拜拜克鲁"));
        assert!(stops_listening("バイバイクルー"));
    }

    /// What was said once Clew was called is what follows its name, wherever
    /// the name was heard: what was heard just before the call is not asked.
    #[test]
    fn what_is_asked_once_called_follows_the_name() {
        assert_eq!(
            after_name("So anyway. Hey, Clew, open Settings."),
            "open Settings."
        );
        assert_eq!(after_name("Hi clue, go to line 30"), "go to line 30");
        // A "Hey" whisper wrote as another word.
        assert_eq!(after_name("Hay clue, go to line 30"), "go to line 30");
        // "Clue" said as anything but a call is no call.
        assert_eq!(after_name("A clue, go to line 30"), "A clue, go to line 30");
        assert_eq!(after_name("Hey Clew"), "");
        // Not in a word, and nothing to cut without it.
        assert_eq!(
            after_name("Open the clueless file"),
            "Open the clueless file"
        );
        assert_eq!(after_name("Clew, next"), "next");
        assert_eq!(after_name("Maclew, next"), "Maclew, next", "not in a word");
    }

    /// Whether Clew's name was heard at all — what the spotter took for the
    /// call was it, or only something like it ("Hey, cool").
    #[test]
    fn the_name_is_heard_or_not() {
        use crate::i18n::{Lang, with_lang};
        assert_eq!(
            named_in("Hey, Clew, open Settings."),
            Some("open Settings.")
        );
        assert_eq!(named_in("Hey, Clew."), Some(""));
        assert_eq!(named_in("Hey clue."), Some(""));
        assert_eq!(named_in("Clue, open settings"), Some("open settings"));
        // A call word whisper wrote as another, first in the sentence.
        assert_eq!(
            named_in("Et Clew ouvre les paramètres."),
            Some("ouvre les paramètres.")
        );
        assert_eq!(named_in("Eh Clew, go to line 30"), Some("go to line 30"));
        assert_eq!(
            named_in("ネークルー、呼び出しグラフを開いて"),
            Some("呼び出しグラフを開いて")
        );
        assert_eq!(named_in("So that's it. Hay Clew, run it"), Some("run it"));
        // Written in Latin letters, a pause, then a Japanese sentence —
        // and "crew", as Japanese says "Clew", only before one.
        assert_eq!(named_in("Crew 設定を"), Some("設定を"));
        assert_eq!(named_in("Clew 設定を開いて"), Some("設定を開いて"));
        assert_eq!(named_in("ヘイ、Crew 設定を開いて"), Some("設定を開いて"));
        assert_eq!(named_in("ヘイ、Crew、設定を開いて"), Some("設定を開いて"));
        assert_eq!(named_in("Crew、設定を開いて"), Some("設定を開いて"));
        // A Japanese sentence before it, or Chinese characters after it.
        assert_eq!(named_in("ヘイ、Crew 設定"), Some("設定"));
        assert_eq!(named_in("ヘイ、Crew、検索"), Some("検索"));
        assert_eq!(named_in("ヘイ、Crew。"), Some(""));
        assert_eq!(named_in("ヘイ、Crew。設定を開いて"), Some("設定を開いて"));
        // Words that begin as particles do.
        assert_eq!(named_in("Crew やめて"), Some("やめて"));
        assert_eq!(named_in("Crew もう一回"), Some("もう一回"));
        assert_eq!(named_in("Crew とめて"), Some("とめて"));
        // A Japanese sentence beginning with numbers or Latin letters.
        assert_eq!(named_in("Crew 30行目に移動して"), Some("30行目に移動して"));
        assert_eq!(named_in("Crew README を開いて"), Some("README を開いて"));
        assert_eq!(named_in("Crew README を読んで"), Some("README を読んで"));
        assert_eq!(
            named_in("Crew README を開いてください"),
            Some("README を開いてください")
        );
        assert_eq!(
            named_in("Crew README を開いて下さい"),
            Some("README を開いて下さい")
        );
        assert_eq!(named_in("Crew README を開いて…"), Some("README を開いて…"));
        // A dot in a name ends no sentence.
        assert_eq!(
            named_in("Hey Crew main.rs を開いて"),
            Some("main.rs を開いて")
        );
        // A Japanese comma after it.
        assert_eq!(named_in("Hey Crew、設定"), Some("設定"));
        // Everyone's, to everyone, what everyone uses: said to Clew.
        assert_eq!(
            named_in("Hey Clew、みんなのファイルを開いて"),
            Some("みんなのファイルを開いて")
        );
        assert_eq!(
            named_in("Hey Clew、みんなに共有して"),
            Some("みんなに共有して")
        );
        assert_eq!(
            named_in("Clew、みんなが使うファイルを開いて"),
            Some("みんなが使うファイルを開いて")
        );
        // All together, said to a crew — and words that begin with a
        // particle's kana.
        assert_eq!(named_in("Crew、みんなで行こう"), None);
        assert_eq!(named_in("Crew、みんながんばって"), None);
        assert_eq!(named_in("ヘイクルー みんなとりあえず集合"), None);
        assert_eq!(
            named_in("Hey Clew、みんなと共有して"),
            Some("みんなと共有して")
        );
        // Particles with kana after them are particles still.
        assert_eq!(
            named_in("Clew、みんながよく使うファイルを開いて"),
            Some("みんながよく使うファイルを開いて")
        );
        assert_eq!(
            named_in("Hey Clew、みんなとのチャットを開いて"),
            Some("みんなとのチャットを開いて")
        );
        // Or as Clew's own Japanese phrases say a value, shown in Japanese.
        with_lang(Lang::Ja, || {
            assert_eq!(named_in("Clew 30行目へ移動"), Some("30行目へ移動"));
            assert_eq!(named_in("Clew README を開く"), Some("README を開く"));
            assert_eq!(named_in("Clew TODO を検索"), Some("TODO を検索"));
            assert_eq!(without_name("Clew TODO を検索"), "TODO を検索");
            assert_eq!(named_in("Clew VS Code より速い？"), None);
            // Said of Clew, ending as a phrase of its own would.
            assert_eq!(named_in("Clew 2 回同じファイルを開く"), None);
            assert_eq!(named_in("Clew 2 回落ちた件を検索"), None);
            // Shown in Japanese, Chinese characters after "crew" are
            // Japanese: a command's name alone.
            assert_eq!(named_in("Hey Crew 続行"), Some("続行"));
            assert_eq!(named_in("Hey Crew 設定"), Some("設定"));
            // Said to many, it is no call.
            assert_eq!(named_in("Hey Crew 全員集合"), None);
            // Korean after it is not Japanese.
            assert_eq!(named_in("Hey Crew 가자"), None);
            // Names with katakana in them are names.
            assert_eq!(
                named_in("Clew READMEファイルを開く"),
                Some("READMEファイルを開く")
            );
            assert_eq!(
                named_in("Clew TODOコメントを検索"),
                Some("TODOコメントを検索")
            );
        });
        // Korean's calling "야", then a pause.
        assert_eq!(named_in("클루야 설정 열어줘"), Some("설정 열어줘"));
        assert_eq!(without_name("클루야 설정 열어줘"), "설정 열어줘");
        assert_eq!(named_in("클루야말로 최고야"), None);
        // The name in Latin letters, as whisper may write it.
        assert_eq!(named_in("Clew야 설정 열어줘"), Some("설정 열어줘"));
        assert_eq!(named_in("Clew야, 설정 열어줘"), Some("설정 열어줘"));
        assert_eq!(named_in("헤이 Clew야 설정 열어줘"), Some("설정 열어줘"));
        assert_eq!(without_name("헤이 Clew야 설정 열어줘"), "설정 열어줘");
        assert_eq!(named_in("Clew야말로 최고야"), None);
        assert_eq!(named_in("그게 Clew야 맞아"), None);
        assert_eq!(named_in("Clew go to definition が効かない"), None);
        assert_eq!(named_in("Clew crashed. ちょっと見て"), None);
        assert_eq!(named_in("Clew crashed.ちょっと見て"), None);
        assert_eq!(named_in("Hey crew、let's go"), None);
        for said in [
            "Crew, get ready.",
            "Crew members are here",
            "Hey Crew 가자",
            "Crew 설정 열어줘",
            "Crew 打开设置",
            "The crew 設定を",
            // Said of Clew in Japanese, Chinese or Korean: the name goes
            // on into its sentence — or, before Chinese or Korean, only a
            // pause sets it apart, which a call is not written with.
            "Clewのグラフが遅い",
            "Clewが落ちた",
            "Clew設定が開かない",
            "Clew って何？",
            "Clew だよ",
            "Clew 又卡住了。ちょっと見て",
            "Hey crew! 準備はいい？",
            "準備できた？ Hey crew, let's go",
            // "crew" before Chinese, which does not write Clew so — nor,
            // shown in another language than Japanese, Japanese kanji alone.
            "Hey crew 打开设置",
            "Hey Crew 続行",
            "嘿 crew 开会了",
            "Crew，大家过来一下",
            // Particles, and words that go on from the name.
            "Clew とは何ですか",
            "Clew とか使ってる？",
            "Clew として使う",
            "Clew も落ちた",
            "Clew と比べて遅い",
            "Clew や VS Code みたいなツール",
            "Clew もバグってる",
            // Latin letters or numbers after the pause: only a request.
            "Clew VS Code より速い？",
            "Clew 2 回落ちたけど、なぜ？",
            "Clew 2 回落ちた。直して",
            "Clew のグラフが遅い",
            "Clew 的图很慢",
            "Clew 又卡住了",
            "Clew가 또 멈췄어",
            "Clew 설정 열어줘",
            // Written in its own script, the name is set apart only by a
            // mark: "crew, everyone gather".
            "クルー 全員集合",
            "クルー 全員集まって",
            // Said to many: to a crew, not to Clew.
            "Crew 全員集まって",
            "Hey Crew 全員集合",
            "ヘイ、クルー、全員集合",
            "ヘイクルー みんな聞いて",
            "Hey Clew、皆さん、こちらへ",
        ] {
            assert_eq!(named_in(said), None, "{said:?}");
        }
        // Said of Clew, not to it.
        for said in [
            "Clew keeps crashing on my machine.",
            "Clew's graph is slow",
            "Hey, Clew's graph is slow",
            "Hey Clew’s not answering",
            "this is Clew's graph",
            "the clue is here",
            "The clue, I think",
            "No clue why it broke",
            "A clue is in the log",
            "Paul et Clew, ouvre les paramètres",
            "Hey crew, let's go",
        ] {
            assert_eq!(named_in(said), None, "{said:?}");
        }
        // What was said before the call was not said to Clew.
        assert_eq!(
            named_in("and then I said hey Clew, open settings"),
            Some("open settings")
        );
        assert_eq!(named_in("No clue why. Hey Clew, run it"), Some("run it"));
        // A call heard plainly wins over one heard in another word before it.
        assert_eq!(named_in("Et clou. Hey Clew, run it"), Some("run it"));
        // A sentence of its own: the call, or the name with a mark after it.
        assert_eq!(named_in("Hey. Clew, open settings"), Some("open settings"));
        assert_eq!(
            named_in("Right. Clew, open settings"),
            Some("open settings")
        );
        // The start of a longer word, in any script: no name.
        for said in [
            "I said hi. Clew crashed again.",
            "헤이 클로이, 뭐 해?",
            "헤이 크루, 가자",
            "ねえ、クルーズ行こう",
            "嘿，克鲁斯来了",
            "Hey, Clewless people",
        ] {
            assert_eq!(named_in(said), None, "{said:?}");
        }
        for said in [
            "Hey cool it works now.",
            "Hey Kyle, look at this.",
            "Open the clueless file",
            "Hey, cool — no clue why it broke",
            "Hey, cool — no Clew why it broke",
            "and then I said Clew, open settings",
            "Clue me in",
        ] {
            assert_eq!(named_in(said), None, "{said:?}");
        }
        // In the language it is said in, whichever Clew is shown in.
        for lang in Lang::ALL {
            with_lang(lang, || {
                assert_eq!(named_in("嘿，克鲁，打开设置"), Some("打开设置"));
                assert_eq!(
                    named_in("Hé, Clou, ouvre les paramètres"),
                    Some("ouvre les paramètres")
                );
                assert_eq!(named_in("헤이 클루, 설정 열어"), Some("설정 열어"));
                assert_eq!(named_in("클루야, 설정 열어"), Some("설정 열어"));
                assert_eq!(named_in("ヘイ・クルー、設定を開いて"), Some("設定を開いて"));
                assert_eq!(named_in("嘿，克鲁，索引一下"), Some("索引一下"));
                assert_eq!(named_in("嘿克鲁打开设置"), Some("打开设置"));
                assert_eq!(
                    named_in("ヘイ、クルー、ファイルを開いて"),
                    Some("ファイルを開いて")
                );
                // Longer words the name begins.
                assert_eq!(named_in("ねえ、クルーネックどこ？"), None);
                assert_eq!(named_in("嗨，克鲁格先生"), None);
                assert_eq!(named_in("헤이클로 설정 열어"), Some("설정 열어"));
                assert_eq!(named_in("ヘイ、クルー、設定を開いて"), Some("設定を開いて"));
                assert_eq!(named_in("这条线索是克鲁斯给的"), None, "{lang:?}");
                assert_eq!(named_in("ヘイ、Clew、設定を開いて"), Some("設定を開いて"));
                assert_eq!(named_in("嗨，克鲁，打开设置"), Some("打开设置"));
            });
        }
    }

    /// A question on screen is answered only by what is all a yes or all
    /// a no: what merely opens with one ("run the tests") is not.
    #[test]
    fn a_question_is_answered_by_a_yes_alone() {
        for said in [
            "yes",
            "Yes, do it.",
            "ok",
            "go ahead please",
            "run it",
            "Yes. Go ahead.",
            "Allow.",
        ] {
            assert_eq!(only_yes_or_no(said), Some(true), "{said:?}");
        }
        for said in ["no", "No.", "cancel", "not now", "never mind"] {
            assert_eq!(only_yes_or_no(said), Some(false), "{said:?}");
        }
        for said in [
            "run the tests",
            "yes and explain it",
            "no idea why",
            "nothing",
            "",
        ] {
            assert_eq!(only_yes_or_no(said), None, "{said:?}");
        }
        assert_eq!(
            yes_or_no("run the tests"),
            Some(true),
            "where opening counts"
        );
    }

    /// A phrase gives its value however it is said around it: in either
    /// case, with a full stop, with "please" — and only as a word of its
    /// own.
    #[test]
    fn a_phrase_gives_its_value() {
        assert_eq!(
            value_in("go to line {n}", "Go to line 30.").as_deref(),
            Some("30")
        );
        assert_eq!(
            value_in("go to line {n}", "please go to line, 30").as_deref(),
            Some("30")
        );
        assert_eq!(
            value_in("open {name}", "Open cart.py!").as_deref(),
            Some("cart.py")
        );
        assert_eq!(value_in("open {name}", "opener"), None);
        // The value as said, its case kept, its words not mended.
        assert_eq!(
            value_in("watch {text}", "Watch Cart.total.").as_deref(),
            Some("Cart.total")
        );
        assert_eq!(
            value_in("search for {text}", "search for online orders").as_deref(),
            Some("online orders")
        );
        // As a recognizer may hear it.
        assert_eq!(
            value_in("breakpoint on line {n}", "Breakpoint Online 6.").as_deref(),
            Some("6")
        );
        assert_eq!(
            value_in("breakpoint on line {n}", "break point on line six").as_deref(),
            Some("six")
        );
        assert_eq!(value_in("open {name}", "open"), None);
        assert_eq!(
            value_in("跳到第{n}行", "跳到第30行。").as_deref(),
            Some("30")
        );
        assert_eq!(
            value_in("{name}を開く", "cart.pyを開く").as_deref(),
            Some("cart.py")
        );
    }

    /// A line number said in figures or in English words.
    #[test]
    fn a_number_is_read_in_figures_or_words() {
        assert_eq!(number("30"), Some(30));
        assert_eq!(number("#12"), Some(12));
        assert_eq!(number("thirty five"), Some(35));
        assert_eq!(number("forty-two"), Some(42));
        assert_eq!(number("a hundred and two"), Some(102));
        assert_eq!(number("five"), Some(5));
        assert_eq!(number("Thirty-Two"), Some(32));
        assert_eq!(number("zero"), None);
        assert_eq!(number("quantity"), None);
    }

    /// An answer is what it opens with — the longest yes or no it begins
    /// with, as words of their own.
    #[test]
    fn an_answer_is_what_it_opens_with() {
        for yes in [
            "Yes.",
            "Yes, do it.",
            "Okay, do it.",
            "Sure, do it",
            "Run it now",
            "go ahead",
        ] {
            assert_eq!(yes_or_no(yes), Some(true), "{yes}");
        }
        for no in [
            "No.",
            "No, do not.",
            "No, thank you.",
            "Not now",
            "Cancel that",
            "Don't",
        ] {
            assert_eq!(yes_or_no(no), Some(false), "{no}");
        }
        for neither in [
            "nothing",
            "notes",
            "okayish",
            "show the call graph",
            "Open cart.py",
        ] {
            assert_eq!(yes_or_no(neither), None, "{neither}");
        }
        crate::i18n::with_lang(crate::i18n::Lang::Fr, || {
            assert_eq!(yes_or_no("Non, merci."), Some(false));
            assert_eq!(yes_or_no("Oui, vas-y"), Some(true));
        });
        crate::i18n::with_lang(crate::i18n::Lang::Ja, || {
            assert_eq!(yes_or_no("はい、お願いします。"), Some(true));
            assert_eq!(only_yes_or_no("はい。お願いします。"), Some(true));
            assert_eq!(only_yes_or_no("実行する"), Some(true));
        });
        crate::i18n::with_lang(crate::i18n::Lang::Zh, || {
            assert_eq!(only_yes_or_no("对，允许。"), Some(true));
        });
        crate::i18n::with_lang(crate::i18n::Lang::Ko, || {
            assert_eq!(only_yes_or_no("좋아요"), Some(true));
        });
    }

    /// A path's words, as a file is said.
    #[test]
    fn a_path_is_read_as_its_words() {
        assert_eq!(tokens("src/CartView.tsx"), ["src", "cart", "view", "tsx"]);
        assert_eq!(
            tokens("tests/test_cart.py"),
            ["tests", "test", "cart", "py"]
        );
        assert_eq!(tokens("src/gitops.rs"), ["src", "gitops", "rs"]);
        assert_eq!(tokens("README.md"), ["readme", "md"]);
    }

    /// A file said: its dot said, what only says it is a file left out.
    #[test]
    fn a_file_is_said_as_its_name() {
        assert_eq!(file_words("cart dot py"), ["cart.py"]);
        assert_eq!(file_words("the cart file"), ["cart"]);
        assert!(file_words("it").is_empty());
        assert_eq!(squeezed("apply discount"), squeezed("apply_discount"));
        assert_eq!(squeezed("Apply Discount"), squeezed("applyDiscount"));
    }
}
