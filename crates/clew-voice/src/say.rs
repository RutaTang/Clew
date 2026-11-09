//! An answer made ready to be read aloud: its markdown as it would be said
//! — code blocks left out, a name in code said as its words (`cart.py` as
//! "cart dot py", `apply_discount` as "apply discount"), a link as its text
//! — cut into sentences, each with the language its script says it is in;
//! and, as the answer streams in, its sentences handed out once each is
//! finished ([`Reader`]).

/// What a text is said in, when a script besides Latin writes it — the
/// script its own words are written in, not one it borrows a word or a name
/// from: Hangul is Korean; kanji Japanese or Chinese, as the characters
/// say; Cyrillic Russian, Ukrainian or Bulgarian, as its letters and its
/// little words say; Greek is Greek,
/// Arabic Arabic, Devanagari Hindi. `None`: a Latin script, whose language
/// the caller knows better. `usual`: what is said where nothing tells.
pub fn script_language(text: &str, usual: Option<&str>) -> Option<&'static str> {
    let told = told_by(text);
    frame(&told).map(|_| decide(&told, &told, usual).0)
}

/// How much of a text each script writes: characters of Chinese, Japanese
/// and Korean one by one; letters of the other scripts by the word, and
/// only a word of two or more — one letter alone is a symbol.
struct Tally {
    chars: [usize; SCRIPTS],
    words: [usize; SCRIPTS],
}

impl Tally {
    fn of(text: &str) -> Tally {
        use Script::*;
        let mut tally = Tally {
            chars: [0; SCRIPTS],
            words: [0; SCRIPTS],
        };
        let mut run: Option<(Script, usize)> = None;
        for c in text.chars() {
            // A combining accent belongs to the letter before it.
            if matches!(c as u32, 0x0300..=0x036F) {
                continue;
            }
            let script = script_of(c);
            if let Some(script) = script {
                tally.chars[script as usize] += 1;
            }
            match (script, &mut run) {
                (Some(s @ (Latin | Cyrillic | Greek | Arabic | Devanagari)), Some((r, len)))
                    if *r == s =>
                {
                    *len += 1;
                }
                (Some(s @ (Latin | Cyrillic | Greek | Arabic | Devanagari)), _) => {
                    end_word(&mut run, &mut tally.words);
                    run = Some((s, 1));
                }
                _ => end_word(&mut run, &mut tally.words),
            }
        }
        end_word(&mut run, &mut tally.words);
        tally
    }

    fn cjk(&self) -> usize {
        use Script::*;
        self.chars[Hangul as usize] + self.chars[Kana as usize] + self.chars[Han as usize]
    }

    /// The script other than Latin that writes most of the text — Chinese,
    /// Japanese and Korean together as [`Script::Han`] — and how many words
    /// of it there are: for those three, a word to every two characters.
    /// `None` when there is none.
    fn other(&self) -> Option<(Script, usize)> {
        use Script::*;
        [
            (Han, self.cjk().div_ceil(2)),
            (Cyrillic, self.words[Cyrillic as usize]),
            (Greek, self.words[Greek as usize]),
            (Arabic, self.words[Arabic as usize]),
            (Devanagari, self.words[Devanagari as usize]),
        ]
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n > 0)
    }
}

/// Whether `lang` is written in Latin letters.
fn written_in_latin(lang: &str) -> bool {
    !matches!(
        lang,
        "zh" | "ja" | "ko" | "ru" | "uk" | "bg" | "el" | "ar" | "hi"
    )
}

/// The scripts told apart (see [`script_of`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Script {
    Latin,
    Hangul,
    Kana,
    Han,
    Cyrillic,
    Greek,
    Arabic,
    Devanagari,
}

const SCRIPTS: usize = Script::Devanagari as usize + 1;

/// The script `c` is a letter of — not a digit, nor punctuation, nor the
/// middle dot (・) or long-vowel mark (ー) Chinese text borrows from kana.
fn script_of(c: char) -> Option<Script> {
    use Script::*;
    if c.is_numeric() {
        return None;
    }
    Some(match c as u32 {
        0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => Hangul,
        0x30FB | 0x30FC | 0xFF70 => return None,
        0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => Kana,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => Han,
        0x0400..=0x052F => Cyrillic,
        0x037E | 0x0387 => return None,
        0x0370..=0x03FF | 0x1F00..=0x1FFF => Greek,
        0x060C | 0x061B | 0x061F | 0x066A..=0x066D | 0x06D4 => return None,
        0x0600..=0x06FF | 0x0750..=0x077F => Arabic,
        0x0964 | 0x0965 => return None,
        0x0900..=0x097F => Devanagari,
        0x00C0..=0x024F | 0x1E00..=0x1EFF if c.is_alphabetic() => Latin,
        _ if c.is_ascii_alphabetic() => Latin,
        _ => return None,
    })
}

/// The word being read ends: counted when it is one (two letters or more).
fn end_word(run: &mut Option<(Script, usize)>, words: &mut [usize; SCRIPTS]) {
    if let Some((script, len)) = run.take()
        && len >= 2
    {
        words[script as usize] += 1;
    }
}

/// The script a sentence's own words are written in, when it holds letters
/// of a script besides Latin — `None` for a Latin script. A sentence that
/// mixes the two is written in the one with more words of its own: the
/// Latin words that are names ([`latin_words`]) are not its own ("Используйте
/// Visual Studio Code", "推荐 Visual Studio Code"), and the other script's
/// words in a Latin sentence are what it is about ("In Japanese, 設定 means
/// settings"). As many of each: Latin when a little word of a language
/// written in it is among its Latin words ("Click 設定, then 保存"), else
/// the other script ("Это foo").
fn frame(sentence: &str) -> Option<Script> {
    let tally = Tally::of(sentence);
    let (script, theirs) = tally.other()?;
    let latin = latin_words(sentence);
    match latin.len().cmp(&theirs) {
        std::cmp::Ordering::Less => Some(script),
        std::cmp::Ordering::Greater => None,
        std::cmp::Ordering::Equal => (!has_little_words(&latin.join(" "))).then_some(script),
    }
}

/// `text`'s words in Latin letters (two or more) that are its own, not
/// names. A name is capitalised — the first word of the text only when
/// the Latin word after it is too ("Visual Studio Code 추천", not "Click
/// 設定") — or a short word in lower case between two words of a name ("Run
/// and Debug").
fn latin_words(text: &str) -> Vec<String> {
    // Each Latin word, and whether it is the first letters of the text.
    let mut words: Vec<(bool, String)> = Vec::new();
    let mut word = String::new();
    let mut first = false;
    let mut lettered = false;
    let flush = |word: &mut String, first: bool, words: &mut Vec<(bool, String)>| {
        if word.chars().count() >= 2 {
            words.push((first, std::mem::take(word)));
        }
        word.clear();
    };
    for c in text.chars() {
        match script_of(c) {
            Some(Script::Latin) => {
                if word.is_empty() {
                    first = !lettered;
                }
                word.push(c);
                lettered = true;
            }
            other => {
                flush(&mut word, first, &mut words);
                lettered |= other.is_some();
            }
        }
    }
    flush(&mut word, first, &mut words);
    let capital = |w: &str| w.chars().next().is_some_and(char::is_uppercase);
    let mut name: Vec<bool> = words
        .iter()
        .enumerate()
        .map(|(i, (first, w))| {
            capital(w) && (!first || words.get(i + 1).is_some_and(|(_, next)| capital(next)))
        })
        .collect();
    for i in 1..words.len().saturating_sub(1) {
        let w = &words[i].1;
        if !name[i]
            && name[i - 1]
            && name[i + 1]
            && w.chars().count() <= 3
            && w.chars().all(char::is_lowercase)
        {
            name[i] = true;
        }
    }
    words
        .into_iter()
        .zip(name)
        .filter(|(_, name)| !name)
        .map(|((_, word), _)| word)
        .collect()
}

/// Characters only Chinese writes, among its commonest: Simplified forms
/// where Japanese writes 這, 説, 対, 時, 設, 変, 関, 訳, 認…, and Traditional
/// forms where Japanese writes 説, 対, 会, 来, 国, 訳, 変, 関… — none of them
/// in Japanese use.
const CHINESE_ONLY: &str = "这们个么说对时还过让给从吗呢吧该现发实设变环关应码输调页种样总价进动开运门问题间长东车书语读为买卖购网络\
                            译认证务无处义乐习节级线组结统计记识询试话请选择单电脑软载显储库类档\
                            這們說對會來國學譯數據從讀與麼擇應舊號關變價總賣傳檔碼";

/// Characters only Japanese writes, among its commonest: its own forms of
/// characters Chinese writes otherwise (証 for 证 and 證, 変 for 变 and 變),
/// and characters of its own (込, 働).
const JAPANESE_ONLY: &str = "証済権変関説読図広応発気楽価経続総単実処様検験録覧拡転込働円両帰戦鉄駅歴隠険仮栄営継剤雑従縦恵軽齢譲穏浄獣渋粋駆辺対庁歯塩県売\
                             択訳伝拠覚観歓黒悪薬戻払抜";

/// The language of a sentence written in Chinese, Japanese or Korean
/// characters, in an answer whose own words so far are `context`, and
/// whether it is told: Korean where Hangul writes most of it; Chinese where
/// a character only Chinese writes is in it; Japanese where kana is, or a
/// character only Japanese writes. A heading of kanji alone is told by the
/// answer about it; until it is, Chinese — or Japanese, where that is what
/// is usually spoken.
fn han(sentence: &str, context: &str, usual: Option<&'static str>) -> (&'static str, bool) {
    let tally = Tally::of(sentence);
    if tally.chars[Script::Hangul as usize] * 2 >= tally.cjk() {
        return ("ko", true);
    }
    let tells = |text: &str| {
        if text.chars().any(|c| CHINESE_ONLY.contains(c)) {
            Some("zh")
        } else if text
            .chars()
            .any(|c| script_of(c) == Some(Script::Kana) || JAPANESE_ONLY.contains(c))
        {
            Some("ja")
        } else {
            None
        }
    };
    match tells(sentence).or_else(|| tells(context)) {
        Some(lang) => (lang, true),
        None => (
            usual.filter(|l| matches!(*l, "zh" | "ja")).unwrap_or("zh"),
            false,
        ),
    }
}

/// `text` without what it quotes — «…», „…“, “…”, "…", 「…」, 『…』: words a
/// sentence names, which may be another language's, not its own. A mark
/// that no other closes quotes nothing. Apostrophes are left, as Ukrainian
/// writes them within words (м'який).
fn unquoted(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let closers: &[char] = match chars[i] {
            '«' => &['»'],
            '„' => &['“', '”'],
            '“' => &['”'],
            '"' => &['"'],
            '「' => &['」'],
            '『' => &['』'],
            _ => &[],
        };
        if let Some(len) = chars[i + 1..].iter().position(|c| closers.contains(c)) {
            out.push(' ');
            i += len + 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// What tells a text's language: its own words ([`own_words`]), what they
/// quote set aside ([`unquoted`]) — unless all of them are quoted.
fn told_by(text: &str) -> String {
    let own = own_words(text);
    let unquoted = unquoted(&own);
    if unquoted.chars().any(|c| script_of(c).is_some()) {
        unquoted
    } else {
        own
    }
}

/// The letters that tell Cyrillic languages apart, counted: Ukrainian's
/// і ї є ґ; Russian's ы э ё; Bulgarian's ъ said as a vowel (път,
/// България), where Russian writes it only as the hard sign before е ё ю я.
fn cyrillic_letters(text: &str) -> [usize; 3] {
    let letters: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut counts = [0usize; 3];
    for c in &letters {
        match c {
            'і' | 'ї' | 'є' | 'ґ' => counts[0] += 1,
            'ы' | 'э' | 'ё' => counts[1] += 1,
            _ => {}
        }
    }
    counts[2] = letters
        .windows(2)
        .filter(|w| w[0] == 'ъ' && matches!(w[1], 'а'..='я') && !matches!(w[1], 'е' | 'ю' | 'я'))
        .count();
    counts
}

/// The little words each of Ukrainian, Russian and Bulgarian writes, and
/// neither of the others: "що", "это", "това".
const CYRILLIC_WORDS: [&[&str]; 3] = [
    &[
        "що",
        "це",
        "цей",
        "ця",
        "ці",
        "він",
        "вона",
        "вони",
        "воно",
        "потім",
        "був",
        "була",
        "було",
        "були",
        "бути",
        "який",
        "яка",
        "яке",
        "які",
        "також",
        "якщо",
        "або",
        "лише",
        "тільки",
        "вже",
        "щоб",
        "ми",
        "ви",
        "треба",
        "можна",
        "його",
        "її",
        "їх",
        "де",
        "коли",
        "зараз",
        "тут",
        "ніж",
        "від",
        "з",
        "теж",
        "дуже",
        "ні",
        "ось",
        "як",
    ],
    &[
        "что",
        "это",
        "этот",
        "эта",
        "эти",
        "он",
        "она",
        "они",
        "оно",
        "затем",
        "был",
        "была",
        "было",
        "были",
        "быть",
        "есть",
        "который",
        "которая",
        "которое",
        "которые",
        "также",
        "если",
        "только",
        "уже",
        "ещё",
        "еще",
        "чтобы",
        "мы",
        "вы",
        "нужно",
        "можно",
        "его",
        "её",
        "ее",
        "их",
        "где",
        "когда",
        "потом",
        "сейчас",
        "здесь",
        "чем",
        "вот",
        "тоже",
        "очень",
        "нет",
    ],
    &[
        "че",
        "това",
        "този",
        "тази",
        "тези",
        "той",
        "тя",
        "са",
        "е",
        "съм",
        "сме",
        "сте",
        "бил",
        "била",
        "било",
        "били",
        "който",
        "която",
        "което",
        "които",
        "също",
        "ако",
        "само",
        "вече",
        "още",
        "към",
        "със",
        "във",
        "ние",
        "вие",
        "трябва",
        "къде",
        "кога",
        "сега",
        "тук",
        "няма",
        "има",
        "защото",
        "след",
        "преди",
        "чрез",
        "така",
        "както",
        "нещо",
        "всички",
    ],
];

/// What says a text in Cyrillic is Ukrainian, Russian or Bulgarian: the
/// letters only it writes ([`cyrillic_letters`]) and its little words
/// ([`CYRILLIC_WORDS`]), one each.
fn cyrillic_evidence(text: &str) -> [usize; 3] {
    let mut counts = cyrillic_letters(text);
    for word in words_of(text) {
        let word = word.to_lowercase();
        for (count, words) in counts.iter_mut().zip(CYRILLIC_WORDS) {
            if words.contains(&word.as_str()) {
                *count += 1;
            }
        }
    }
    counts
}

/// The words of `text`: letters, with an apostrophe within (м'який) and the
/// dots of an abbreviation (т.е, a.m) — whose letters are no words alone.
fn words_of(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let next = chars.get(i + 1).copied();
        let joins = matches!(c, '.' | '\'' | '’')
            && !word.is_empty()
            && next.is_some_and(char::is_alphabetic);
        if c.is_alphabetic() || joins {
            word.push(c);
        } else if !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// The language of a sentence in Cyrillic, in an answer whose own words so
/// far are `context`, and whether it is told: by the sentence when it
/// clearly says — two more of one language's letters and little words than
/// of any other's — else by the answer when it does; else, not yet told, the
/// one the answer leans to, or, where nothing leans, the one usually spoken
/// if it is written in Cyrillic, else Russian.
fn cyrillic(sentence: &str, context: &str, usual: Option<&'static str>) -> (&'static str, bool) {
    const LANGS: [&str; 3] = ["uk", "ru", "bg"];
    let ranked = |counts: [usize; 3]| {
        let mut order = [0usize, 1, 2];
        // Russian first among equals: the one most written.
        order.sort_by_key(|&i| (std::cmp::Reverse(counts[i]), i != 1));
        (LANGS[order[0]], counts[order[0]], counts[order[1]])
    };
    let (lang, best, second) = ranked(cyrillic_evidence(sentence));
    if best >= second + 2 {
        return (lang, true);
    }
    let (lang, best, second) = ranked(cyrillic_evidence(context));
    if best >= second + 2 {
        return (lang, true);
    }
    if best > second {
        return (lang, false);
    }
    (
        usual
            .filter(|l| matches!(*l, "ru" | "uk" | "bg"))
            .unwrap_or("ru"),
        false,
    )
}

/// Every language a voice here speaks, as its code.
const KNOWN: &[&str] = &[
    "en", "ko", "ja", "zh", "ar", "bg", "cs", "da", "de", "el", "es", "et", "fi", "fr", "hi", "hr",
    "hu", "id", "it", "lt", "lv", "nl", "pl", "pt", "ro", "ru", "sk", "sl", "sv", "tr", "uk", "vi",
];

/// `lang` (`fr`, `fr-CA`, `pt_BR`) as the code of a language a voice here
/// speaks (`fr`, `pt`), when one does.
pub fn known_language(lang: &str) -> Option<&'static str> {
    let base = lang
        .split(['-', '_'])
        .next()
        .unwrap_or(lang)
        .to_ascii_lowercase();
    KNOWN.iter().copied().find(|code| *code == base)
}

/// The little words each language in a Latin script uses most, and other
/// languages rarely: what tells an English answer from a French one.
const LITTLE_WORDS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "the", "and", "is", "of", "to", "that", "this", "with", "for", "are", "it", "you",
            "be", "by", "from", "which", "or", "an", "was", "has", "its", "when", "each", "then",
            "here", "there", "what", "how", "if", "not", "can", "your", "they", "these", "those",
            "into", "at", "every", "all", "about", "after", "before", "first", "only", "just",
            "more", "most", "some", "any", "other", "than", "but", "have", "had", "been", "were",
            "would", "should", "could", "may", "might", "must", "their", "them", "who", "where",
            "why", "while", "because", "through", "over", "out", "up", "same", "both", "such",
            "returns", "calls", "adds", "uses", "takes", "checks", "sets", "gets", "makes", "runs",
            "does",
        ],
    ),
    (
        "fr",
        &[
            "le", "les", "et", "est", "des", "une", "du", "que", "qui", "dans", "pour", "pas",
            "sur", "ce", "cette", "avec", "sont", "au", "aux", "il", "elle", "vous", "nous", "ou",
            "chaque", "puis",
        ],
    ),
    (
        "es",
        &[
            "el", "los", "las", "y", "es", "del", "por", "para", "con", "una", "lo", "como", "más",
            "está", "pero", "cada", "su", "sus", "este", "esta",
        ],
    ),
    (
        "de",
        &[
            "der", "die", "das", "und", "ist", "nicht", "ein", "eine", "mit", "zu", "den", "dem",
            "von", "auf", "für", "sich", "auch", "wird", "sind", "jede", "jeden",
        ],
    ),
    (
        "it",
        &[
            "il", "gli", "è", "di", "che", "per", "non", "della", "delle", "sono", "nel", "questo",
            "questa", "ogni", "anche", "come", "alla",
        ],
    ),
    (
        "pt",
        &[
            "o", "os", "é", "um", "uma", "não", "do", "da", "dos", "das", "em", "na", "no", "com",
            "cada", "também", "isso", "são",
        ],
    ),
    (
        "nl",
        &[
            "het", "een", "en", "is", "van", "dat", "niet", "op", "te", "met", "voor", "zijn",
            "er", "ook", "wordt", "aan", "elke",
        ],
    ),
];

/// Whether `text` has a little word of a language written in Latin letters
/// ([`LITTLE_WORDS`]).
fn has_little_words(text: &str) -> bool {
    text.split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .any(|word| {
            let word = word.to_lowercase();
            LITTLE_WORDS
                .iter()
                .any(|(_, words)| words.contains(&word.as_str()))
        })
}

/// The language `text` (in a Latin script) is written in, by its little
/// words — when there are enough of them to tell, and one language clearly
/// has the most.
pub fn latin_language(text: &str) -> Option<&'static str> {
    let mut counts = [0usize; LITTLE_WORDS.len()];
    for word in text
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_lowercase();
        for (count, (_, words)) in counts.iter_mut().zip(LITTLE_WORDS) {
            if words.contains(&word.as_str()) {
                *count += 1;
            }
        }
    }
    let mut ranked: Vec<(usize, &'static str)> = counts
        .iter()
        .zip(LITTLE_WORDS)
        .map(|(count, (lang, _))| (*count, *lang))
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
    let (best, lang) = ranked[0];
    let second = ranked.get(1).map_or(0, |r| r.0);
    // Two of its words at least — one may be another language's too ("en"
    // is French as well as Dutch) — and more than any other language's by a
    // half: "de la" is French and Spanish both.
    (best >= 2 && best * 2 > second * 3).then_some(lang)
}

/// The language to say `sentence` in, in an answer whose sentences up to
/// this one, and this one, are `context` — each told by its own words, code
/// and quotes set aside. The script the sentence's own words are written in
/// says ([`script_language`]); for a Latin
/// script, the language its little words say ([`latin_language`]), else the
/// answer's, else `usual` when it is written in Latin letters, else English.
pub fn language_for(sentence: &str, context: &str, usual: Option<&str>) -> &'static str {
    decide(&told_by(sentence), &told_by(context), usual).0
}

/// [`language_for`], of words as they tell ([`told_by`]), and whether the
/// sentence, or the answer so far, told it: `false` where neither does yet
/// and what follows may ([`Reader::next`]).
fn decide(sentence: &str, context: &str, usual: Option<&str>) -> (&'static str, bool) {
    let usual = usual.and_then(known_language);
    match frame(sentence) {
        None => match latin_language(sentence).or_else(|| latin_language(context)) {
            Some(lang) => (lang, true),
            None => (usual.filter(|l| written_in_latin(l)).unwrap_or("en"), false),
        },
        Some(Script::Cyrillic) => cyrillic(sentence, context, usual),
        Some(Script::Greek) => ("el", true),
        Some(Script::Arabic) => ("ar", true),
        Some(Script::Devanagari) => ("hi", true),
        Some(_) => han(sentence, context, usual),
    }
}

/// What marks, in the text [`marked`] makes, where a name in code said as
/// its words begins and ends: characters of Unicode's private use, which no
/// text holds — taken out before anything is said ([`unmarked`]), and with
/// what they enclose before a sentence's language is told ([`own_words`]).
const CODE_OPEN: char = '\u{E000}';
const CODE_CLOSE: char = '\u{E001}';

/// `text` as it is said: the marks taken out.
fn unmarked(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(*c, CODE_OPEN | CODE_CLOSE))
        .collect()
}

/// `text`'s own words: what is marked as code left out.
fn own_words(text: &str) -> String {
    let mut out = String::new();
    let mut code = false;
    for c in text.chars() {
        match c {
            CODE_OPEN => code = true,
            CODE_CLOSE => {
                code = false;
                out.push(' ');
            }
            _ if !code => out.push(c),
            _ => {}
        }
    }
    out
}

/// `markdown` as it would be said, one paragraph, heading or list item a
/// line: what is only drawn or only read — code blocks, markup, an address
/// — left out or said in words.
pub fn speakable(markdown: &str) -> String {
    unmarked(&marked(markdown))
}

/// [`speakable`], with each name in code said as words marked
/// ([`CODE_OPEN`], [`CODE_CLOSE`]).
fn marked(markdown: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    // The fence a code block is open with: its mark, and how many.
    let mut fence: Option<(char, usize)> = None;
    let fence_of = |line: &str| {
        let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let run = line.chars().take_while(|c| *c == mark).count();
        (run >= 3).then_some((mark, run))
    };
    for line in markdown.lines() {
        let trimmed = line.trim();
        // A fenced code block: shown, not said. It is closed by a fence of
        // its own mark, as long as it or longer, with nothing after it —
        // so one block can show another (````markdown … ```bash … ``` …
        // ````).
        if let Some((mark, run)) = fence {
            if let Some((closing, length)) = fence_of(trimmed)
                && closing == mark
                && length >= run
                && trimmed.trim_start_matches(mark).trim().is_empty()
            {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_of(trimmed) {
            fence = Some(open);
            continue;
        }
        // A table's rule; its rows, cell by cell.
        if trimmed.starts_with('|') {
            if trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
                continue;
            }
            let cells: Vec<String> = trimmed
                .trim_matches('|')
                .split('|')
                .map(|c| inline(c.trim()))
                .filter(|c| !c.is_empty())
                .collect();
            if !cells.is_empty() {
                out.push(ended(cells.join(", ")));
            }
            continue;
        }
        // A rule, an image alone, an HTML line: nothing to say.
        if trimmed
            .chars()
            .all(|c| matches!(c, '-' | '*' | '_' | '=' | ' '))
            || trimmed.starts_with('<')
        {
            if !trimmed.is_empty() && out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            if trimmed.is_empty() {
                out.push(String::new());
            }
            continue;
        }
        let text = trimmed
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start();
        let heading = trimmed.starts_with('#');
        // A list's marker; what it marks is a sentence of its own.
        let (item, text) = list_item(text);
        let said = inline(text);
        if said.is_empty() {
            continue;
        }
        out.push(if heading || item { ended(said) } else { said });
    }
    // Lines of one paragraph run on; paragraphs, headings and items stand
    // apart.
    let mut said = String::new();
    let mut apart = true;
    for line in out {
        if line.is_empty() {
            apart = true;
            continue;
        }
        if !said.is_empty() {
            said.push(if apart || ends_sentence(&said) {
                '\n'
            } else {
                ' '
            });
        }
        said.push_str(&line);
        apart = false;
    }
    said
}

/// A list item's text, without its marker (`- `, `* `, `1. `), and whether
/// it was one.
fn list_item(text: &str) -> (bool, &str) {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(marker) {
            return (true, rest);
        }
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = text[digits..]
            .strip_prefix(". ")
            .or_else(|| text[digits..].strip_prefix(") "))
    {
        return (true, rest);
    }
    (false, text)
}

/// `text` ending as a sentence ends: a full stop added where it has none.
fn ended(mut text: String) -> String {
    if !ends_sentence(&text) {
        text.push('.');
    }
    text
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .ends_with(['.', '!', '?', ':', ';', '。', '！', '？', '：', '；'])
}

/// One line's inline markdown as said: code as its words, marked
/// ([`marked`]), a link as its text, an address as "a link", emphasis and
/// emoji dropped.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        // Code: what is between the backticks, said as its words.
        if let Some(after) = rest.strip_prefix('`') {
            let ticks = 1 + after.bytes().take_while(|b| *b == b'`').count();
            let fence = &rest[..ticks];
            let body = &rest[ticks..];
            if let Some(end) = body.find(fence) {
                out.push(CODE_OPEN);
                out.push_str(&code_words(body[..end].trim()));
                out.push(CODE_CLOSE);
                rest = &body[end + ticks..];
                continue;
            }
            rest = &rest[ticks..];
            continue;
        }
        // A link or an image: its text.
        if rest.starts_with('[') || rest.starts_with("![") {
            let open = rest.find('[').unwrap_or(0);
            // Its text ends at the first `]`, and an address follows it at
            // once; a `[` that no such `](` closes is said as it is ([1]).
            if let Some(close) = rest[open..].find(']').map(|i| open + i)
                && rest[close..].starts_with("](")
                && let Some(end) = rest[close..].find(')').map(|i| close + i)
            {
                out.push_str(&inline(&rest[open + 1..close]));
                rest = &rest[end + 1..];
                continue;
            }
        }
        // An address: said as one.
        if rest.starts_with("https://") || rest.starts_with("http://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let trailing = rest[..end]
                .chars()
                .rev()
                .take_while(|c| matches!(c, '.' | ',' | ')' | ';' | ':'))
                .count();
            out.push_str("a link");
            rest = &rest[end - trailing..];
            continue;
        }
        let c = rest.chars().next().unwrap_or(' ');
        let len = c.len_utf8();
        // Emphasis and strike-through marks; emoji and pictographs.
        if matches!(c, '*' | '~') || is_pictograph(c) {
            rest = &rest[len..];
            continue;
        }
        // A word: said as one, or as the words of a name in code. Chinese,
        // Japanese and Korean, written with no space before a name in code
        // (调用getUserById), are words of their own beside it.
        if c.is_alphanumeric() || c == '_' {
            let cjk = is_cjk(c);
            let end = rest
                .char_indices()
                .find(|&(i, ch)| {
                    is_cjk(ch) != cjk
                        || !(ch.is_alphanumeric()
                            || ch == '_'
                            || ((ch == '.' || ch == '/' || ch == ':')
                                && rest[i + ch.len_utf8()..]
                                    .starts_with(|n: char| n.is_alphanumeric() || n == '_')))
                })
                .map_or(rest.len(), |(i, _)| i);
            let word = &rest[..end];
            // Two words with a slash between (and/or, и/или): both said.
            let mut halves = word.split('/');
            if let (Some(a), Some(b), None) = (halves.next(), halves.next(), halves.next())
                && !a.is_empty()
                && !b.is_empty()
                && a.chars().chain(b.chars()).all(char::is_alphabetic)
            {
                out.push_str(a);
                out.push(' ');
                out.push_str(b);
                rest = &rest[end..];
                continue;
            }
            if is_code_name(word, rest[end..].starts_with('.')) {
                out.push(CODE_OPEN);
                out.push_str(&code_words(word));
                out.push(CODE_CLOSE);
            } else {
                out.push_str(word);
            }
            rest = &rest[end..];
            continue;
        }
        out.push(c);
        rest = &rest[len..];
    }
    // One space where markup left several.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `c` is a character of Chinese, Japanese or Korean.
fn is_cjk(c: char) -> bool {
    matches!(
        script_of(c),
        Some(Script::Han | Script::Kana | Script::Hangul)
    )
}

/// Whether `word` is a name in code rather than a word: it has an
/// underscore, a dot or slash inside it, or capitals within (`cartTotal`).
fn is_code_name(word: &str, then_dot: bool) -> bool {
    let inner = word.trim_matches(|c: char| !c.is_alphanumeric());
    // An abbreviation: letters between dots (e.g, т.е), one listed (p.ej),
    // or a few capitalised at a time with a full stop after them (Bs.As.).
    let lower = inner.to_lowercase();
    if single_letters(&lower)
        || ABBREVIATIONS.contains(&lower.as_str())
        || (then_dot && capitals_dotted(inner))
    {
        return false;
    }
    // A version (v1.2.3): said as it is written.
    let version = inner.strip_prefix(['v', 'V']).unwrap_or(inner);
    if version.contains('.') && version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return false;
    }
    inner.contains(['_', '/'])
        || (inner.contains('.') && !inner.chars().all(|c| c.is_ascii_digit() || c == '.'))
        || inner.chars().skip(1).any(|c| c.is_uppercase())
            && inner.chars().any(|c| c.is_lowercase())
}

/// A name in code said as its words: `src/cart.py:12` as "src slash cart
/// dot py line 12", `apply_discount` as "apply discount", `cartTotal` as
/// "cart total", `Cart::total()` as "Cart total".
pub fn code_words(code: &str) -> String {
    let code = code.trim_end_matches("()");
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let flush = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '.' if next.is_some_and(char::is_alphanumeric)
                && !(word.chars().all(|w| w.is_ascii_digit())
                    && next.is_some_and(|n| n.is_ascii_digit())) =>
            {
                flush(&mut word, &mut words);
                words.push("dot".into());
            }
            '/' => {
                flush(&mut word, &mut words);
                if !words.is_empty() && next.is_some() {
                    words.push("slash".into());
                }
            }
            ':' if next == Some(':') => {
                flush(&mut word, &mut words);
                i += 1;
            }
            // A line of a file (cart.py:12); after a name alone, a number
            // (localhost:3000).
            ':' if next.is_some_and(|n| n.is_ascii_digit()) => {
                flush(&mut word, &mut words);
                if words.iter().any(|w| w == "dot" || w == "slash") {
                    words.push("line".into());
                }
            }
            '_' | '-' | '>' | '(' | ')' | '<' | '[' | ']' | '{' | '}' | ',' | ' ' | ':' | '!'
            | '?' => {
                flush(&mut word, &mut words);
            }
            c if c.is_uppercase() && word.chars().last().is_some_and(char::is_lowercase) => {
                flush(&mut word, &mut words);
                word.push(c);
            }
            c => word.push(c),
        }
        i += 1;
    }
    flush(&mut word, &mut words);
    words.join(" ")
}

fn is_pictograph(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F | 0x200D)
}

/// `text` (as [`speakable`] says it) cut into sentences: at a full stop, a
/// question or an exclamation mark that ends one, at the end of a line, and
/// a long one at its commas, so the first is heard soon.
pub fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut start = 0;
        let mut in_code = false;
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (k, &(i, c)) in chars.iter().enumerate() {
            // Not within a name in code ([`marked`]): its words are said
            // whole, whatever marks they hold.
            match c {
                CODE_OPEN => in_code = true,
                CODE_CLOSE => in_code = false,
                _ => {}
            }
            if in_code {
                continue;
            }
            let end = i + c.len_utf8();
            let cjk_stop = matches!(c, '。' | '！' | '？');
            let stop = matches!(c, '.' | '!' | '?')
                && chars.get(k + 1).is_none_or(|&(_, n)| n.is_whitespace())
                && !abbreviation(&line[start..end]);
            if cjk_stop || stop {
                push_sentence(&mut out, &line[start..end]);
                start = end;
            }
        }
        push_sentence(&mut out, &line[start..]);
    }
    out
}

/// A sentence said whole, or at its commas when it is long.
fn push_sentence(out: &mut Vec<String>, sentence: &str) {
    const LONG: usize = 220;
    let sentence = sentence.trim();
    if sentence.is_empty() || !sentence.chars().any(char::is_alphanumeric) {
        return;
    }
    if sentence.chars().count() <= LONG {
        out.push(sentence.to_string());
        return;
    }
    let mut piece = String::new();
    let mut in_code = false;
    for part in sentence.split_inclusive([',', ';', '，', '；', '、']) {
        // Cut only where no name in code is open ([`marked`]).
        if !in_code && !piece.is_empty() && piece.chars().count() + part.chars().count() > LONG {
            out.push(piece.trim().to_string());
            piece.clear();
        }
        piece.push_str(part);
        for c in part.chars() {
            match c {
                CODE_OPEN => in_code = true,
                CODE_CLOSE => in_code = false,
                _ => {}
            }
        }
    }
    if !piece.trim().is_empty() {
        out.push(piece.trim().to_string());
    }
}

/// Whether `text` ends with an abbreviation's full stop, not a sentence's.
fn abbreviation(text: &str) -> bool {
    let last = text
        .trim_end_matches('.')
        .rsplit(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("");
    let lower = last.to_lowercase();
    ABBREVIATIONS.contains(&lower.as_str()) || single_letters(&lower) || capitals_dotted(last)
}

/// Whether `word` is an abbreviation of a few letters at a time between
/// dots, each part capitalised (Ph.D, EE.UU, Bs.As) — not a file's name
/// (app.js).
fn capitals_dotted(word: &str) -> bool {
    word.contains('.')
        && word.split('.').all(|part| {
            (1..=3).contains(&part.chars().count())
                && part.chars().all(char::is_alphabetic)
                && part.chars().next().is_some_and(char::is_uppercase)
        })
}

/// Whether `word` is letters one by one, between dots or alone: an initial
/// (J.), or an abbreviation of them (e.g, a.m, z.B, т.е).
fn single_letters(word: &str) -> bool {
    !word.is_empty()
        && word
            .split('.')
            .all(|part| part.chars().count() == 1 && part.chars().all(char::is_alphabetic))
}

/// Abbreviations a full stop ends that do not end a sentence — those of
/// more than one letter at a time (`single_letters` are all).
const ABBREVIATIONS: &[&str] = &[
    "etc",
    "vs",
    "mr",
    "mrs",
    "ms",
    "dr",
    "st",
    "no",
    "fig",
    "cf",
    "approx",
    "incl",
    "prof",
    "jr",
    "sr",
    "inc",
    "ltd",
    "co",
    "corp",
    "dept",
    "ca",
    "vgl",
    "bzw",
    "usw",
    "ggf",
    "evtl",
    "inkl",
    "nr",
    "ph.d",
    "p.ej",
    "ee.uu",
    "sra",
    "sres",
    "т.нар",
    "напр",
    "см",
    "др",
    "проф",
    "стр",
    "рис",
    "табл",
    "тыс",
    "млн",
    "млрд",
    "руб",
    "гг",
    "вв",
];

/// One sentence to say, and the language to say it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    pub text: String,
    pub lang: &'static str,
}

/// How many sentences after it a sentence that does not tell its language
/// waits for, at most: told by them, or by the answer as far as they go.
const LOOKAHEAD: usize = 8;

/// `answer`, as it streams in, as far as it reads as it will once it is
/// whole: up to a name in code being written on its last line, whose
/// closing backtick, not come yet, makes what is before it code — and the
/// sentences cut where it will not be cut.
fn settled(answer: &str) -> &str {
    let start = answer.rfind('\n').map_or(0, |i| i + 1);
    let line = &answer.as_bytes()[start..];
    let mut open: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < line.len() {
        if line[i] == b'`' {
            let ticks = line[i..].iter().take_while(|b| **b == b'`').count();
            match open {
                Some((_, t)) if t == ticks => open = None,
                None => open = Some((i, ticks)),
                _ => {}
            }
            i += ticks;
        } else {
            i += 1;
        }
    }
    match open {
        Some((at, _)) => &answer[..start + at],
        None => answer,
    }
}

/// The sentences of an answer as it streams in, each handed out once: the
/// last one only when the answer is over, as the words still coming may be
/// its own.
#[derive(Debug, Default)]
pub struct Reader {
    given: usize,
    /// The language usually spoken: what a sentence whose words do not
    /// tell is said in ([`language_for`]).
    usual: Option<&'static str>,
    /// The own words ([`own_words`]) of the sentences handed out: what
    /// tells the language of a sentence whose words do not — the same
    /// however the answer comes in, as it holds finished sentences alone.
    context: String,
}

impl Reader {
    /// A reader for an answer in a conversation usually held in `usual`.
    pub fn new(usual: Option<&str>) -> Reader {
        Reader {
            given: 0,
            usual: usual.and_then(known_language),
            context: String::new(),
        }
    }

    /// The answer cut back to `answer` — what came after it taken out, a
    /// remark made before the answer was: its sentences, all of them handed
    /// out already, are where reading goes on from.
    pub fn cut_back(&mut self, answer: &str) {
        self.given = sentences(&marked(answer)).len();
    }

    /// The sentences of `answer` (its markdown so far) not handed out yet;
    /// `done`: no more is coming.
    pub fn next(&mut self, answer: &str, done: bool) -> Vec<Sentence> {
        let answer = if done { answer } else { settled(answer) };
        let all = sentences(&marked(answer));
        let ready = if done {
            all.len()
        } else {
            all.len().saturating_sub(1)
        };
        if ready <= self.given {
            return Vec::new();
        }
        let mut fresh = Vec::with_capacity(ready - self.given);
        while self.given < ready {
            let text = &all[self.given];
            let own = told_by(text);
            let context = format!("{}{own}\n", self.context);
            let (mut lang, sure) = decide(&own, &context, self.usual);
            if !sure {
                // Not told by it, nor by what came before: by the sentences
                // after it, as each is finished, until one tells — or as
                // far as [`LOOKAHEAD`] goes, or the answer is over.
                let mut ahead = context.clone();
                let mut next = self.given + 1;
                loop {
                    if next > self.given + LOOKAHEAD || (next >= ready && done) {
                        lang = decide(&own, &ahead, self.usual).0;
                        break;
                    }
                    if next >= ready {
                        return fresh;
                    }
                    ahead.push_str(&told_by(&all[next]));
                    ahead.push('\n');
                    let (told, sure) = decide(&own, &ahead, self.usual);
                    if sure {
                        lang = told;
                        break;
                    }
                    next += 1;
                }
            }
            self.context = context;
            fresh.push(Sentence {
                lang,
                text: unmarked(text),
            });
            self.given += 1;
        }
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markdown is said as it reads: code blocks left out, a heading and a
    /// list item each a sentence, a link as its text, an address as one.
    #[test]
    fn markdown_is_said_as_it_reads() {
        let said = speakable(
            "## How it works\n\
             `Cart.total` sums each **item's** price, see [the docs](https://x.y/z).\n\
             \n\
             ```python\nprint('not said')\n```\n\
             - first, the subtotal\n\
             - then `apply_discount` 🎉\n\
             \n\
             | Name | Line |\n|---|---|\n| total | 34 |\n\
             More at https://example.com/a.",
        );
        assert_eq!(
            said,
            "How it works.\n\
             Cart dot total sums each item's price, see the docs.\n\
             first, the subtotal.\n\
             then apply discount.\n\
             Name, Line.\n\
             total, 34.\n\
             More at a link."
        );
        assert!(!said.contains("not said"));
    }

    /// The sentences an answer's markdown is read in, and their languages.
    fn read(markdown: &str, usual: Option<&str>, done: bool) -> Vec<(String, &'static str)> {
        Reader::new(usual)
            .next(markdown, done)
            .into_iter()
            .map(|s| (s.text, s.lang))
            .collect()
    }

    fn langs(markdown: &str, usual: Option<&str>) -> Vec<&'static str> {
        read(markdown, usual, true)
            .into_iter()
            .map(|(_, l)| l)
            .collect()
    }

    /// A sentence mostly in Latin letters with words of another script is
    /// in that script's language when its Latin words are names, and in a
    /// Latin language when they are a sentence's; names in code are no
    /// words of either, also with no space before them.
    #[test]
    fn mixed_sentences_follow_their_own_words() {
        assert_eq!(langs("Используйте Visual Studio Code.", Some("ru")), ["ru"]);
        assert_eq!(langs("Используйте Visual Studio Code.", Some("en")), ["ru"]);
        assert_eq!(langs("推荐 Visual Studio Code。", Some("zh")), ["zh"]);
        assert_eq!(langs("推荐 Visual Studio Code。", None), ["zh"]);
        assert_eq!(langs("Visual Studio Code 추천.", None), ["ko"]);
        assert_eq!(
            read("调用getUserById。", Some("en"), true),
            [("调用get User By Id。".to_string(), "zh")]
        );
        // A word of another script in a sentence of English is a quoted one.
        assert_eq!(langs("The word 你好 means hello.", None), ["en"]);
        assert_eq!(langs("The prefix Это means this.", Some("ru")), ["en"]);
    }

    /// An answer is read the same however it comes in: a character at a
    /// time, the sentences and their languages are those of it whole.
    #[test]
    fn an_answer_is_read_the_same_however_it_comes() {
        let answers = [
            "推荐 Visual Studio Code。它很快。",
            "## 概要\n価格は据え置きです。それから変わりません。",
            "## 概要\n这个函数读取设置。然后返回。",
            "Слово «їжак» значит еж. Это пример.",
            "Используйте Visual Studio Code. Слово «їжак» значит еж. Это пример.",
            "Добре. Функція повертає список.",
            "It adds, i.e. sums them. Then `Cart.total` returns it.",
            "Use `getUserById` here.\n\n```rust\nlet x = 1;\n```\n- first, the subtotal\n- then the tax",
            "Le résultat est prêt. 結果は準備できました。The end.",
            "调用getUserById。然后返回。",
            "运行 `echo \"Build is done. Exiting\"` 查看结果。然后继续。",
            "Выполните `git commit -m \"Fix the typo. Update docs\"` сейчас. Готово.",
            "This is documented [1]. See [the guide](https://example.com/guide) for more.",
            "Use `vec![1, 2, 3]` to make a vector. It is quick.",
            "Run `echo \"The build is done. Exiting\"` now. Then it stops.",
            "## 概要\n\n### 目的\n\n本ツールは設定を管理します。",
            "Bien sûr ! Voici comment corriger l'erreur. La fonction lit le fichier et les données.",
        ];
        for answer in answers {
            for usual in [
                None,
                Some("en"),
                Some("zh"),
                Some("ja"),
                Some("ru"),
                Some("fr"),
            ] {
                let whole = read(answer, usual, true);
                let mut reader = Reader::new(usual);
                let mut came = Vec::new();
                let mut so_far = String::new();
                for c in answer.chars() {
                    so_far.push(c);
                    came.extend(
                        reader
                            .next(&so_far, false)
                            .into_iter()
                            .map(|s| (s.text, s.lang)),
                    );
                }
                came.extend(
                    reader
                        .next(answer, true)
                        .into_iter()
                        .map(|s| (s.text, s.lang)),
                );
                assert_eq!(came, whole, "{answer:?} with {usual:?}");
            }
        }
    }

    /// Kanji are Chinese where a character only Chinese writes is about —
    /// Simplified or Traditional — and Japanese where kana is and none is;
    /// a heading of kanji alone waits for the next sentence to tell.
    #[test]
    fn kanji_are_told_by_what_is_about() {
        let traditional = "日文的「こんにちは」是你好的意思。這個函數會讀取設定檔，\
                           然後根據語言代碼選擇對應的翻譯表。如果找不到翻譯，就回傳原文。";
        assert_eq!(langs(traditional, None).last(), Some(&"zh"));
        // 据 is Japanese too (据え置き).
        assert_eq!(langs("## 概要\n価格は据え置きです。", None), ["ja", "ja"]);
        // The heading waits until the sentence after it is finished.
        assert!(read("## 概要\n価格は", None, false).is_empty());
        assert_eq!(
            read("## 概要\n価格は据え置きです。それから", None, false)
                .into_iter()
                .map(|(_, l)| l)
                .collect::<Vec<_>>(),
            ["ja", "ja"]
        );
        // The answer over, it is told as it is.
        assert_eq!(langs("## 概要", None), ["zh"]);
    }

    /// One letter of a word quoted from Ukrainian does not make a sentence
    /// of Russian Ukrainian; a sentence that clearly is one stays one.
    #[test]
    fn cyrillic_is_told_by_its_letters_and_the_answers() {
        assert_eq!(
            langs("Слово «їжак» значит еж. Это пример.", None),
            ["ru", "ru"]
        );
        assert_eq!(
            langs("Это пример. Слово «їжак» значит еж.", None),
            ["ru", "ru"]
        );
        assert_eq!(
            langs("Функція повертає список. Ось приклад коду.", None),
            ["uk", "uk"]
        );
        // Unquoted, one letter is not enough either, where the answer's
        // letters say otherwise.
        assert_eq!(
            langs("Слово їжак значит еж. Это пример.", None),
            ["ru", "ru"]
        );
        // Nor does a quoted one tell the sentence before it.
        assert_eq!(
            langs(
                "Используйте Visual Studio Code. Слово «їжак» значит еж. Это пример.",
                None
            ),
            ["ru", "ru", "ru"]
        );
        assert_eq!(
            langs(
                "Используйте Visual Studio Code. Слово «їжак» значит еж.",
                None
            ),
            ["ru", "ru"]
        );
        // All of it quoted, its letters are its own.
        assert_eq!(langs("«Підсумок.»", None), ["uk"]);
        // Where no letter tells, it is the language usually spoken.
        assert_eq!(langs("Добре.", Some("uk")), ["uk"]);
        assert_eq!(langs("Добре.", Some("en")), ["ru"]);
        assert_eq!(langs("Добре.", None), ["ru"]);
    }

    /// A sentence in Latin letters is told by its own little words first:
    /// an answer quoting or translating another language reads each
    /// sentence in its own, whatever is usually spoken; an opening that does
    /// not tell waits for a sentence that does.
    #[test]
    fn latin_sentences_are_told_by_their_own_words() {
        assert_eq!(
            langs(
                "Here is the French version of the error messages:\n\n\
                 > Le fichier est introuvable. Vérifiez que le chemin est correct et que vous avez \
                 les droits de lecture. Si le problème persiste, contactez le support.\n\n\
                 Note that this is the formal version of the message. You can use it as is.",
                Some("fr")
            ),
            ["en", "fr", "fr", "fr", "en", "en"]
        );
        assert_eq!(
            langs(
                "Voici la traduction en anglais :\n\n\
                 > The file could not be found. Check that the path is correct and that you have \
                 read access.\n\n\
                 Cette version est plus courte que l'originale.",
                Some("fr")
            )
            .last(),
            Some(&"fr")
        );
        assert_eq!(
            langs(
                "Bien sûr ! Voici comment corriger l'erreur. La fonction `load` lit le fichier et \
                 renvoie les données.",
                Some("en")
            ),
            ["fr", "fr", "fr"]
        );
        assert_eq!(
            langs(
                "Gute Frage! Der Fehler liegt in `parse_config`. Die Funktion liest die Datei und \
                 gibt sie zurück.",
                Some("en")
            ),
            ["de", "de", "de"]
        );
        assert_eq!(
            langs(
                "Great question!\n\n## Overview\n\n1. **Install** the package and run it.",
                Some("fr")
            ),
            ["en", "en", "en"]
        );
    }

    /// A sentence mixing scripts is in the one with more of its own words:
    /// names are not its own, nor what it quotes, nor code; as many of each,
    /// Latin when one of its own Latin words is a little word.
    #[test]
    fn a_mixed_sentence_is_in_the_script_of_its_own_words() {
        for english in [
            "In Japanese, 設定 means settings.",
            "In Russian, «Привет» means hello.",
            "Click 設定, then 保存.",
            "Set `title` to 用户设置 here.",
            "Use 你好世界 as the greeting.",
            "Replace «Сохранить» with «Сохранить изменения».",
        ] {
            assert_eq!(langs(english, Some("zh")), ["en"], "{english}");
            assert_eq!(langs(english, Some("ru")), ["en"], "{english}");
        }
        assert_eq!(
            langs(
                "Откройте панель Run and Debug. Затем нажмите F5.",
                Some("ru")
            ),
            ["ru", "ru"]
        );
        assert_eq!(langs("用 git 提交代码。", None), ["zh"]);
        assert_eq!(langs("Python 很好用。", None), ["zh"]);
        assert_eq!(langs("Это foo.", None), ["ru"]);
    }

    /// Cyrillic is told by the letters and the little words only one of
    /// Russian, Ukrainian and Bulgarian writes — the sentence's, else the
    /// answer's — and what is usually spoken only where nothing tells.
    #[test]
    fn cyrillic_is_told_by_letters_and_little_words() {
        let russian = "## Обзор\n\nМодуль отвечает за настройки. Он читает файл `config.toml` и \
                       возвращает структуру. Затем она проверяется.";
        for usual in [None, Some("uk"), Some("bg"), Some("ru")] {
            assert_eq!(langs(russian, usual), ["ru"; 4], "{usual:?}");
        }
        assert_eq!(
            langs(
                "Звичайно! Ось як це виправити. Функція `check` перевіряє, чи існує файл.",
                Some("ru")
            ),
            ["uk"; 3]
        );
        let bulgarian = "## Преглед\n\nТози модул отговаря за настройките. Той чете файла \
                         `config.toml` и връща структура.";
        assert_eq!(langs(bulgarian, None), ["bg"; 3]);
        assert_eq!(langs(bulgarian, Some("uk")), ["bg"; 3]);
        // The letters of an abbreviation are no words: т.е. is not "е".
        assert_eq!(langs("Сумма, т.е. итог.", None), ["ru"]);
        assert_eq!(
            langs("Сумма, т.е. итог. The job runs at 9 a.m. every day.", None),
            ["ru", "en"]
        );
    }

    /// Kanji are Chinese where a character only Chinese writes is, Japanese
    /// where kana or a character only Japanese writes is — quoted words set
    /// aside — and a heading of kanji alone is told by what follows it.
    #[test]
    fn kanji_are_told_by_characters_only_one_language_writes() {
        assert_eq!(
            langs(
                "## 概要\n\n### 目的\n\n本ツールは設定を管理します。",
                Some("en")
            ),
            ["ja"; 3]
        );
        assert_eq!(
            langs(
                "## 概要\n\n### 目的\n\n本ツールは設定を管理します。",
                Some("zh")
            ),
            ["ja"; 3]
        );
        assert_eq!(
            langs(
                "## 主要機能\n\n- 認証\n- 決済\n- 在庫管理\n\n以上の機能があります。",
                Some("en")
            ),
            ["ja"; 5]
        );
        assert_eq!(
            langs(
                "中文翻译如下：\n\n这个函数读取配置文件，然后返回结果。",
                Some("ja")
            ),
            ["zh", "zh"]
        );
        assert_eq!(
            langs("把这个字符串翻译成「ファイルを開く」。", Some("zh")),
            ["zh"]
        );
        // Kanji alone, with no kana anywhere: told by the characters only
        // one language writes.
        assert_eq!(langs("## 認証の設定", None).len(), 1);
        assert_eq!(langs("## 認証設定\n\n## 決済", None), ["ja", "ja"]);
        assert_eq!(langs("## 认证设置\n\n## 支付", None), ["zh", "zh"]);
        // Both, unquoted: Chinese, which Japanese never writes so.
        assert_eq!(langs("把这个字符串翻译成ファイルを開く。", None), ["zh"]);
        assert_eq!(
            langs("日文的「こんにちは」是你好的意思。可以在早上使用。", None),
            ["zh", "zh"]
        );
    }

    /// Markdown and code are said as they read: a sentence never cut within
    /// a name in code, abbreviations ending none, a block shown within a
    /// block left out, a bracket that is no link said as it is, a port, a
    /// version and "and/or" as words.
    #[test]
    fn what_is_written_is_said_as_it_reads() {
        let said = |markdown: &str| -> Vec<String> {
            read(markdown, None, true)
                .into_iter()
                .map(|(t, _)| t)
                .collect()
        };
        assert_eq!(
            said("Use `vec![1, 2, 3]` to make a vector."),
            ["Use vec 1 2 3 to make a vector."]
        );
        assert_eq!(
            read(
                "用 `isReady ? toTheList() : hideIt()` 吧。",
                Some("zh"),
                true
            ),
            [("用 is Ready to The List hide It 吧。".to_string(), "zh")]
        );
        assert_eq!(said("Сумма, т.е. итог."), ["Сумма, т.е. итог."]);
        assert_eq!(
            said("It runs at 9 a.m. every day. Then it stops."),
            ["It runs at 9 a.m. every day.", "Then it stops."]
        );
        assert_eq!(said("He has a Ph.D. in it."), ["He has a Ph.D. in it."]);
        assert_eq!(
            said("Vive en Bs.As. desde 2010."),
            ["Vive en Bs.As. desde 2010."]
        );
        assert_eq!(
            said("See app.js. Then run it."),
            ["See app dot js.", "Then run it."]
        );
        // A full stop within a name in code ends no sentence.
        assert_eq!(
            said("Run `echo \"Build is done. Exiting\"` now. Then stop."),
            ["Run echo \"Build is done. Exiting\" now.", "Then stop."]
        );
        assert_eq!(
            said("Viven en EE.UU. desde hace años."),
            ["Viven en EE.UU. desde hace años."]
        );
        assert_eq!(
            said(
                "Put this in README.md:\n\n````markdown\n# Setup\n\n```bash\nnpm install --save-dev typescript\n```\n````\n\nThen commit it."
            ),
            ["Put this in README dot md:", "Then commit it."]
        );
        assert_eq!(
            said("This is documented [1]. See [the guide](https://example.com/guide) for more."),
            ["This is documented [1].", "See the guide for more."]
        );
        assert_eq!(
            speakable("Open `localhost:3000` after upgrading to v1.2.3, and/or restart."),
            "Open localhost 3000 after upgrading to v1.2.3, and or restart."
        );
        assert_eq!(
            speakable("Сохраните и/или закройте."),
            "Сохраните и или закройте."
        );
        assert_eq!(
            speakable("See `src/cart.py:12`."),
            "See src slash cart dot py line 12."
        );
    }

    /// What a sentence quotes is set aside, however it is quoted; a mark no
    /// other closes quotes nothing.
    #[test]
    fn quotes_are_set_aside() {
        let words = |text: &str| {
            unquoted(text)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(words("Слово «їжак» значит"), "Слово значит");
        assert_eq!(words("Слово „їжак“ и “ще” и \"є\"."), "Слово и и .");
        assert_eq!(words("按下「保存」按钮，『设置』。"), "按下 按钮， 。");
        assert_eq!(
            words("Ukrainian: м'який, м’який."),
            "Ukrainian: м'який, м’який."
        );
        assert_eq!(words("„one”, then"), ", then");
        // Never closed: nothing is quoted.
        assert_eq!(words("A 12\" pipe, «unclosed"), "A 12\" pipe, «unclosed");
    }

    /// An abbreviation is said as it is written, not as a name in code.
    #[test]
    fn abbreviations_are_not_code() {
        assert_eq!(
            speakable("It adds, i.e. sums them."),
            "It adds, i.e. sums them."
        );
        assert_eq!(speakable("Сумма, т.е. итог."), "Сумма, т.е. итог.");
        assert_eq!(speakable("Open os.path now."), "Open os dot path now.");
    }

    /// A name in code is said as its words.
    #[test]
    fn code_is_said_as_its_words() {
        assert_eq!(
            code_words("src/cart.py:12"),
            "src slash cart dot py line 12"
        );
        assert_eq!(code_words("apply_discount"), "apply discount");
        assert_eq!(code_words("cartTotal"), "cart Total");
        assert_eq!(code_words("Cart::total()"), "Cart total");
        assert_eq!(code_words("3.14"), "3.14");
        assert_eq!(code_words("/usr/bin"), "usr slash bin");
        // Outside backticks too, where a word is plainly a name in code.
        assert_eq!(
            speakable("Call apply_discount from cart.py, version 1.2."),
            "Call apply discount from cart dot py, version 1.2."
        );
    }

    /// Sentences end where they end — not at an abbreviation, a decimal, a
    /// file's dot — and a long one is cut at its commas.
    #[test]
    fn sentences_end_where_they_end() {
        assert_eq!(
            sentences("It adds them, e.g. 3.5 apples. Then? Done!"),
            ["It adds them, e.g. 3.5 apples.", "Then?", "Done!"]
        );
        assert_eq!(sentences("合计。然后打折！"), ["合计。", "然后打折！"]);
        assert_eq!(sentences("one\ntwo"), ["one", "two"]);
        let long = "word, ".repeat(80);
        assert!(sentences(&long).iter().all(|s| s.chars().count() <= 230));
        assert!(sentences("```").is_empty());
    }

    /// A script says its language; a Latin one leaves it to the caller.
    #[test]
    fn a_script_says_its_language() {
        assert_eq!(script_language("안녕하세요", None), Some("ko"));
        assert_eq!(script_language("合計は、", None), Some("ja"));
        assert_eq!(script_language("合计", None), Some("zh"));
        assert_eq!(script_language("合計", Some("ja")), Some("ja"));
        assert_eq!(script_language("The total", Some("fr")), None);
    }

    /// As an answer streams in, each finished sentence is handed out once;
    /// the last only when the answer is over.
    #[test]
    fn a_streamed_answer_is_read_once() {
        let texts = |sentences: Vec<Sentence>| -> Vec<String> {
            sentences.into_iter().map(|s| s.text).collect()
        };
        let mut reader = Reader::default();
        assert!(reader.next("The total", false).is_empty());
        assert!(reader.next("The total is the sum.", false).is_empty());
        assert_eq!(
            texts(reader.next("The total is the sum. Then it", false)),
            ["The total is the sum."]
        );
        assert!(
            reader
                .next("The total is the sum. Then it", false)
                .is_empty()
        );
        assert_eq!(
            texts(reader.next("The total is the sum. Then it rounds.", true)),
            ["Then it rounds."]
        );
        assert!(
            reader
                .next("The total is the sum. Then it rounds.", true)
                .is_empty()
        );
        // A sentence that does not tell its language waits for one that
        // does — "Sure!" is English as much as anything — and is then said
        // in that one's.
        let mut reader = Reader::new(Some("fr"));
        assert!(reader.next("Sure! The total is", false).is_empty());
        assert_eq!(
            reader
                .next("Sure! The total is the sum of the prices. And", false)
                .into_iter()
                .map(|s| (s.text, s.lang))
                .collect::<Vec<_>>(),
            [
                ("Sure!".to_string(), "en"),
                ("The total is the sum of the prices.".to_string(), "en")
            ]
        );
        // A code block still open says nothing of what is in it.
        let mut reader = Reader::default();
        assert_eq!(
            texts(reader.next("Like so:\n```rust\nlet x = 1;\nlet y", false)),
            Vec::<String>::new()
        );
        assert_eq!(
            texts(reader.next("Like so:\n```rust\nlet x = 1;\n```\nDone.", true)),
            ["Like so:", "Done."]
        );
    }

    /// A sentence in a Latin script is said in the language the answer's
    /// little words are in — French, Spanish, German, as well as English —
    /// else in the one usually spoken, else in English; a script that tells
    /// its own language is said in it, whatever the rest.
    #[test]
    fn each_sentence_is_said_in_its_language() {
        assert_eq!(
            latin_language("La fonction calcule le total de chaque article et applique la remise."),
            Some("fr")
        );
        assert_eq!(
            latin_language("The function adds up the price of each item and applies the discount."),
            Some("en")
        );
        assert_eq!(
            latin_language("La función suma el precio de cada artículo y aplica el descuento."),
            Some("es")
        );
        assert_eq!(
            latin_language("Die Funktion addiert den Preis und ist nicht teuer."),
            Some("de")
        );
        // Too few to tell.
        assert_eq!(latin_language("apply discount"), None);
        assert_eq!(latin_language("total"), None);
        // An answer in French, a sentence of it with no little word.
        let answer = "La fonction calcule le total de chaque article. Ensuite, apply discount.";
        assert_eq!(language_for("Ensuite, apply discount.", answer, None), "fr");
        // Nothing tells: the usual language when it is written in Latin
        // letters, else English.
        assert_eq!(
            language_for("apply discount.", "apply discount.", Some("fr")),
            "fr"
        );
        assert_eq!(
            language_for("apply discount.", "apply discount.", Some("zh")),
            "en"
        );
        assert_eq!(
            language_for("apply discount.", "apply discount.", Some("ru")),
            "en"
        );
        assert_eq!(
            language_for("apply discount.", "apply discount.", Some("xx")),
            "en"
        );
        assert_eq!(
            language_for("apply discount.", "apply discount.", None),
            "en"
        );
        // A script that tells its own.
        assert_eq!(language_for("합계를 구합니다.", answer, Some("fr")), "ko");
        assert_eq!(language_for("合計を計算します。", answer, None), "ja");
        assert_eq!(language_for("计算总价。", answer, None), "zh");
        // A heading of kanji alone, in an answer written in Japanese.
        let japanese = "概要。合計を計算します。";
        assert_eq!(language_for("概要。", japanese, Some("en")), "ja");
        assert_eq!(language_for("概要。", "概要。总价。", Some("en")), "zh");
        // Other scripts the voices speak.
        assert_eq!(language_for("Итог считается так.", answer, None), "ru");
        // (An answer of one sentence: the context is the sentence.)
        let alone = |sentence: &str| language_for(sentence, sentence, None);
        assert_eq!(alone("Підсумок."), "uk");
        assert_eq!(alone("Функцията връща списък."), "bg");
        assert_eq!(alone("Объект создан."), "ru");
        assert_eq!(language_for("Το σύνολο.", answer, None), "el");
        assert_eq!(language_for("المجموع.", answer, None), "ar");
        assert_eq!(language_for("कुल योग।", answer, None), "hi");
        // A script a symbol borrows, or a word quotes, is not the
        // sentence's: most of its words are.
        let english = "The function returns the area of the circle.";
        for sentence in [
            "Multiply the radius by π.",
            "The search is Θ(n log n).",
            "The latency is 5 μs.",
            "Δt is the step between frames.",
            "The prefix Это means this.",
        ] {
            assert_eq!(language_for(sentence, english, None), "en", "{sentence}");
        }
        assert_eq!(
            language_for(
                "Функция getUserById возвращает пользователя.",
                english,
                None
            ),
            "ru"
        );
        assert_eq!(language_for("Это foo.", english, None), "ru");
        // A name in code is said as its words, which can outnumber the
        // sentence's own — and the answer's: code never says the language.
        // As the reader hands sentences out, from an answer's markdown:
        let read = |markdown: &str, usual: Option<&str>| -> Vec<&'static str> {
            Reader::new(usual)
                .next(markdown, true)
                .into_iter()
                .map(|s| s.lang)
                .collect()
        };
        assert_eq!(
            read(
                "Функция `getUserById` вызывает `fetchUserFromDatabase`. \
                 Затем `validateUserPermissions` проверяет права.",
                None
            ),
            ["ru", "ru"]
        );
        assert_eq!(
            read("- `src/app/speech.rs` — озвучивание ответов.", Some("en")),
            ["ru"]
        );
        assert_eq!(read("Это функция `getUserById`.", None), ["ru"]);
        assert_eq!(read("Это функция getUserById.", None), ["ru"]);
        assert_eq!(
            read("调用 `getUserById`。然后返回用户。", Some("en")),
            ["zh", "zh"]
        );
        assert_eq!(read("`getUserById` を呼ぶ。", Some("en")), ["ja"]);
        assert_eq!(read("`getUserById` 호출.", None), ["ko"]);
        // A word of another script quoted in English stays English.
        assert_eq!(
            read("The word 你好 means hello. It is a greeting.", None),
            ["en", "en"]
        );
        assert_eq!(
            language_for("The character 中 means middle.", english, None),
            "en"
        );
        // A Japanese heading of kanji and one particle, opening an answer
        // still streaming in, is Japanese.
        assert_eq!(
            Reader::new(Some("en"))
                .next("## 環境変数の設定\n環境変数は", false)
                .into_iter()
                .map(|s| s.lang)
                .collect::<Vec<_>>(),
            ["ja"]
        );
        // Ukrainian and Bulgarian sentences with no letter of their own are
        // in the answer's language, not Russian.
        assert_eq!(
            read("Функція повертає список. Ось приклад коду.", None),
            ["uk", "uk"]
        );
        assert_eq!(
            read("Функцията връща списък. Това е пример.", None),
            ["bg", "bg"]
        );
        // A Russian answer quoting a word of Ukrainian is still Russian.
        assert_eq!(
            read("Слово «їжак» значит ёж. Это пример.", None),
            ["ru", "ru"]
        );
        let greek = "Η συνάρτηση get User By Id. Επιστρέφει τον χρήστη.";
        assert_eq!(
            language_for("Η συνάρτηση get User By Id.", greek, None),
            "el"
        );
        // A word of Russian quoted in English stays English, the
        // conversation's language Russian or not.
        let quoting = "The function returns the area. The prefix Это means this.";
        assert_eq!(
            language_for("The prefix Это means this.", quoting, Some("ru")),
            "en"
        );
        assert_eq!(language_for("调用 getUserById 函数。", english, None), "zh");
        assert_eq!(
            language_for("韩语的“안녕”是你好的意思。", english, None),
            "zh"
        );
        assert_eq!(
            language_for("이 함수는 价格 를 구합니다.", english, None),
            "ko"
        );
        // Kana Chinese text borrows, or quotes, does not make it Japanese.
        assert_eq!(
            language_for("迈克尔・乔丹是篮球运动员。", english, None),
            "zh"
        );
        assert_eq!(language_for("阿・乔丹。", english, None), "zh");
        assert_eq!(language_for("乔丹ー。", english, None), "zh");
        let chinese = "这个函数把问候语翻译成日语，比如“こんにちは”。它先读取配置文件，\
                       然后根据语言代码选择对应的翻译表。如果找不到翻译，就返回原文。计算总价。";
        assert_eq!(language_for("计算总价。", chinese, None), "zh");
        // A list of kanji alone, in an answer written in Japanese.
        let japanese = "主な機能は次のとおりです。\n認証\n決済\n在庫管理";
        assert_eq!(language_for("在庫管理", japanese, None), "ja");
        assert_eq!(known_language("pt_BR"), Some("pt"));
        assert_eq!(known_language("FR-ca"), Some("fr"));
        assert_eq!(known_language("klingon"), None);
        // Handed out with it.
        let mut reader = Reader::new(Some("ko"));
        assert_eq!(
            reader.next("Le total est calculé. 합계입니다.", true),
            [
                Sentence {
                    text: "Le total est calculé.".into(),
                    lang: "fr"
                },
                Sentence {
                    text: "합계입니다.".into(),
                    lang: "ko"
                }
            ]
        );
    }

    /// A remark made before the answer, said and then cut back out: reading
    /// goes on with the answer that follows, none of it skipped, none of
    /// the remark said again.
    #[test]
    fn a_reader_goes_on_after_a_remark_is_cut_back() {
        let mut reader = Reader::new(Some("en"));
        let remark = "Let me look at the pricing code. Then";
        let said: Vec<String> = reader
            .next(remark, true)
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(said, ["Let me look at the pricing code.", "Then"]);
        reader.cut_back("");
        let answer = "The total is worked out in total(). It adds tax last. More";
        let said: Vec<String> = reader
            .next(answer, false)
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(
            said,
            ["The total is worked out in total().", "It adds tax last."]
        );
    }
}
