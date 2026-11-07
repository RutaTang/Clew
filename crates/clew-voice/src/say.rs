//! An answer made ready to be read aloud: its markdown as it would be said
//! — code blocks left out, a name in code said as its words (`cart.py` as
//! "cart dot py", `apply_discount` as "apply discount"), a link as its text
//! — cut into sentences, each with the language its script says it is in;
//! and, as the answer streams in, its sentences handed out once each is
//! finished ([`Reader`]).

/// What a sentence is said in, when its script says so — the script most of
/// it is written in, not one a symbol borrows (`π`, `Θ(n)`, `Δt` in English;
/// a quoted word): Hangul is Korean, kana Japanese, Han alone Chinese — or
/// Japanese, where that is what is being spoken (`usual`: a line of kanji
/// alone); Greek is Greek, Arabic Arabic, Devanagari Hindi, and Cyrillic
/// Russian, Ukrainian or Bulgarian, as its letters say. `None`: a Latin
/// script, whose language the caller knows better.
pub fn script_language(text: &str, usual: Option<&str>) -> Option<&'static str> {
    Tally::of(text).language(text, usual)
}

/// How much of a text each script writes: characters of Chinese, Japanese
/// and Korean one by one, as each is a word or a syllable; letters of the
/// other scripts by the word, and only a word of two or more — one letter
/// alone is a symbol.
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

    /// The script other than Latin that writes most of the text, and how
    /// much of it: characters for Chinese, Japanese and Korean, words for
    /// the rest. `None` when there is none.
    fn other(&self) -> Option<(Script, usize)> {
        use Script::*;
        [
            (Han, self.cjk()),
            (Cyrillic, self.words[Cyrillic as usize]),
            (Greek, self.words[Greek as usize]),
            (Arabic, self.words[Arabic as usize]),
            (Devanagari, self.words[Devanagari as usize]),
        ]
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n > 0)
    }

    /// The language of `text`, of which this is the tally, when a script
    /// other than Latin writes most of it (see [`script_language`]).
    fn language(&self, text: &str, usual: Option<&str>) -> Option<&'static str> {
        let (script, most) = self.other()?;
        // Mostly Latin, it is in a Latin script. A tie is not: a word of
        // another script beside one of English is not a quoted one.
        if most < self.words[Script::Latin as usize] {
            return None;
        }
        Some(self.in_script(script, text, usual))
    }

    /// The language `text` is in, written in `script`.
    fn in_script(&self, script: Script, text: &str, usual: Option<&str>) -> &'static str {
        use Script::*;
        let usual = usual.and_then(known_language);
        match script {
            Han if self.chars[Hangul as usize] * 2 >= self.cjk() => "ko",
            Han if written_in_japanese(text) || usual == Some("ja") => "ja",
            Han => "zh",
            Cyrillic => cyrillic_language(text, usual),
            Greek => "el",
            Arabic => "ar",
            _ => "hi",
        }
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
#[derive(Clone, Copy, PartialEq, Eq)]
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

/// The kana and the Han characters in `text`.
fn kana_and_han(text: &str) -> (usize, usize) {
    let (mut kana, mut han) = (0usize, 0usize);
    for c in text.chars() {
        match script_of(c) {
            Some(Script::Kana) => kana += 1,
            Some(Script::Han) => han += 1,
            _ => {}
        }
    }
    (kana, han)
}

/// Whether `text` is written in Japanese: kana beside its kanji, as
/// Japanese is written — at least a fifth of them — not a word or two of it
/// quoted in Chinese.
fn written_in_japanese(text: &str) -> bool {
    let (kana, han) = kana_and_han(text);
    kana > 0 && kana * 5 >= kana + han
}

/// Characters only Chinese writes, among its commonest: Simplified forms
/// where Japanese writes 這, 説, 対, 時, 設, 変, 関…, and Traditional forms
/// where Japanese writes 説, 対, 会, 来, 国, 訳, 変, 関… — none of them in
/// Japanese use.
const CHINESE_ONLY: &str = "这们个么说对时还过让给从吗呢吧该现发实设变环关应码输调页种样总价进动开运门问题间长东车书语读为买卖购网络\
                            這們說對會來國學譯數據從讀與麼擇應舊號關變價總賣傳檔碼";

/// Whether kanji in `sentence` are Japanese, in an answer whose own words
/// are `context`: the sentence written in Japanese; or kana in it or about
/// it, where no character only Chinese writes is — a heading of kanji
/// alone, or one with a single particle (環境変数の設定), in an answer in
/// Japanese, but not a sentence of a Chinese answer that quotes Japanese.
fn japanese(sentence: &str, context: &str) -> bool {
    if written_in_japanese(sentence) {
        return true;
    }
    let chinese = |text: &str| text.chars().any(|c| CHINESE_ONLY.contains(c));
    if chinese(sentence) || chinese(context) {
        return false;
    }
    kana_and_han(sentence).0 > 0 || kana_and_han(context).0 > 0
}

/// `text` without what it quotes — «…», „…“, “…”, "…": words a sentence
/// names, which may be another language's, not its own. Apostrophes are
/// left, as Ukrainian writes them within words (м'який).
fn unquoted(text: &str) -> String {
    let mut out = String::new();
    let mut closers: Vec<char> = Vec::new();
    for c in text.chars() {
        let closes = closers
            .last()
            .is_some_and(|&close| c == close || (close == '“' && c == '”'));
        match c {
            _ if closes => {
                closers.pop();
            }
            '«' => closers.push('»'),
            '„' => closers.push('“'),
            '“' => closers.push('”'),
            '"' => closers.push('"'),
            _ if closers.is_empty() => out.push(c),
            _ => {}
        }
    }
    out
}

/// The letters that tell Cyrillic languages apart, counted: Ukrainian's
/// і ї є ґ; Russian's ы э ё; Bulgarian's ъ said as a vowel (път,
/// България), where Russian writes it only as the hard sign before е ё ю я.
/// What `text` quotes is not counted — a word quoted from Ukrainian does
/// not make a sentence of Russian Ukrainian — unless all its Cyrillic is.
fn cyrillic_letters(text: &str) -> [usize; 3] {
    let own = unquoted(text);
    let text = if own.chars().any(|c| script_of(c) == Some(Script::Cyrillic)) {
        own.as_str()
    } else {
        text
    };
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

/// The language a text in Cyrillic is in, as most of its telling letters
/// say ([`cyrillic_letters`]); where none tells, the one usually spoken if
/// it is written in Cyrillic, else Russian.
fn cyrillic_language(text: &str, usual: Option<&'static str>) -> &'static str {
    let [uk, ru, bg] = cyrillic_letters(text);
    if uk + ru + bg == 0 {
        return usual
            .filter(|lang| matches!(*lang, "ru" | "uk" | "bg"))
            .unwrap_or("ru");
    }
    if uk > ru && uk >= bg {
        "uk"
    } else if bg > ru && bg > uk {
        "bg"
    } else {
        "ru"
    }
}

/// The language a sentence in Cyrillic is in, in an answer whose own words
/// up to it are `context`, and whether it is told: by its own letters
/// when they clearly do — two more of one language's than of any other's;
/// else by the answer's, so that one letter of a word a Russian sentence
/// borrows does not make it Ukrainian. Not told while neither does.
fn cyrillic_sentence(
    sentence: &str,
    context: &str,
    usual: Option<&'static str>,
) -> (&'static str, bool) {
    let clear = |counts: [usize; 3]| {
        let mut sorted = counts;
        sorted.sort_unstable();
        sorted[2] >= sorted[1] + 2
    };
    if clear(cyrillic_letters(sentence)) {
        (cyrillic_language(sentence, usual), true)
    } else {
        (
            cyrillic_language(context, usual),
            clear(cyrillic_letters(context)),
        )
    }
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
            "be", "by", "from", "which", "or", "an", "was", "has", "its", "when", "each",
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

/// The language `text` (in a Latin script) is written in, by its little
/// words — when there are enough of them to tell, and one language clearly
/// has the most.
/// Whether `text` has a little word of a Latin language ([`LITTLE_WORDS`]):
/// its Latin words are a sentence's, not names alone ("Visual Studio Code").
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
    // Two of its words at least, and more than any other language's by a
    // half: "de la" is French and Spanish both.
    (best >= 2 && best * 2 > second * 3).then_some(lang)
}

/// The language to say `sentence` in — its own words, code left out — in
/// an answer whose own words so far, its sentences up to this one and this
/// one with them, are `context`: the script most of the sentence is written in
/// ([`script_language`]); else, for one mostly in Latin letters with words
/// of another script, that script's language when the answer is written in
/// it, or when the Latin words are names (no little word of a Latin
/// language among them: "Используйте Visual Studio Code"), and the Latin
/// language they are in when they are not ("The prefix Это means this");
/// kanji are Japanese where the answer is — kana about, and no character
/// only Chinese writes — and Cyrillic the language its letters say, or the
/// answer's, what it quotes aside (`usual`, where none tells, if that is
/// written in Cyrillic). Else, for a Latin
/// script, the language the answer's words are in, or `usual` when it is
/// written in one; else English.
pub fn language_for(sentence: &str, context: &str, usual: Option<&str>) -> &'static str {
    decide(sentence, context, usual).0
}

/// [`language_for`], and whether what told it was there: `false` for kanji
/// with no kana and no character only Chinese writes, in the sentence or
/// about it — Japanese or Chinese alike — and for Cyrillic with no letter
/// telling Russian from Ukrainian or Bulgarian. Such a sentence is better
/// told with the next sentence in view ([`Reader::next`]).
fn decide(sentence: &str, context: &str, usual: Option<&str>) -> (&'static str, bool) {
    let tally = Tally::of(sentence);
    let lang = tally.language(sentence, usual).or_else(|| {
        let (script, _) = tally.other()?;
        let around = Tally::of(context);
        let answer_in_it = around
            .other()
            .is_some_and(|(s, n)| s == script && n >= around.words[Script::Latin as usize]);
        (answer_in_it || !has_little_words(sentence))
            .then(|| tally.in_script(script, sentence, usual))
    });
    let usual = usual.and_then(known_language);
    let told = |text: &str| {
        text.chars()
            .any(|c| CHINESE_ONLY.contains(c) || script_of(c) == Some(Script::Kana))
    };
    match lang {
        Some(lang @ ("zh" | "ja")) => {
            let sure = lang == "ja" || told(sentence) || told(context);
            let lang = if lang == "zh" && japanese(sentence, context) {
                "ja"
            } else {
                lang
            };
            (lang, sure)
        }
        Some("ru" | "uk" | "bg") => cyrillic_sentence(sentence, context, usual),
        Some(lang) => (lang, true),
        None => (
            latin_language(context)
                .or_else(|| usual.filter(|lang| written_in_latin(lang)))
                .unwrap_or("en"),
            true,
        ),
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
    let mut fence: Option<&str> = None;
    for line in markdown.lines() {
        let trimmed = line.trim();
        // A fenced code block: shown, not said.
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = ["```", "~~~"].into_iter().find(|f| trimmed.starts_with(f)) {
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
            if let Some(close) = rest[open..].find("](").map(|i| open + i)
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
            if is_code_name(word) {
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
fn is_code_name(word: &str) -> bool {
    let inner = word.trim_matches(|c: char| !c.is_alphanumeric());
    // Letters between dots are an abbreviation: e.g, i.e, т.е.
    if inner.contains('.') && inner.split('.').all(|part| part.chars().count() == 1) {
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
            ':' if next.is_some_and(|n| n.is_ascii_digit()) => {
                flush(&mut word, &mut words);
                words.push("line".into());
            }
            '_' | '-' | '>' | '(' | ')' | '<' | '[' | ']' | '{' | '}' | ',' | ' ' | ':' => {
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
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        for (k, &(i, c)) in chars.iter().enumerate() {
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
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        last.as_str(),
        "e.g" | "i.e" | "etc" | "vs" | "mr" | "mrs" | "ms" | "dr" | "st" | "no" | "fig" | "cf"
    ) || (last.len() == 1 && last.chars().all(|c| c.is_ascii_alphabetic()))
}

/// One sentence to say, and the language to say it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    pub text: String,
    pub lang: &'static str,
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

    /// The sentences of `answer` (its markdown so far) not handed out yet;
    /// `done`: no more is coming.
    pub fn next(&mut self, answer: &str, done: bool) -> Vec<Sentence> {
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
            let own = own_words(text);
            let context = format!("{}{own}\n", self.context);
            let (mut lang, sure) = decide(&own, &context, self.usual);
            if !sure {
                // Not told by what came before: by the next sentence too,
                // once it is finished — or as it is, the answer over.
                match all.get(self.given + 1).filter(|_| self.given + 1 < ready) {
                    Some(next) => {
                        let ahead = format!("{context}{}\n", own_words(next));
                        lang = decide(&own, &ahead, self.usual).0;
                    }
                    None if !done => break,
                    None => {}
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
        ];
        for answer in answers {
            for usual in [None, Some("en"), Some("zh"), Some("ja")] {
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

    /// What a sentence quotes is set aside, however it is quoted.
    #[test]
    fn quotes_are_set_aside() {
        assert_eq!(unquoted("Слово «їжак» значит"), "Слово  значит");
        assert_eq!(unquoted("Слово „їжак“ и “ще” и \"є\"."), "Слово  и  и .");
        assert_eq!(
            unquoted("Ukrainian: м'який, м’який."),
            "Ukrainian: м'який, м’який."
        );
        assert_eq!(unquoted("„one”, then"), ", then");
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
        assert!(reader.next("The total adds them.", false).is_empty());
        assert_eq!(
            texts(reader.next("The total adds them. Then it", false)),
            ["The total adds them."]
        );
        assert!(
            reader
                .next("The total adds them. Then it", false)
                .is_empty()
        );
        assert_eq!(
            texts(reader.next("The total adds them. Then it rounds.", true)),
            ["Then it rounds."]
        );
        assert!(
            reader
                .next("The total adds them. Then it rounds.", true)
                .is_empty()
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
}
