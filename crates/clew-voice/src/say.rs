//! An answer made ready to be read aloud: its markdown as it would be said
//! — code blocks left out, a name in code said as its words (`cart.py` as
//! "cart dot py", `apply_discount` as "apply discount"), a link as its text
//! — cut into sentences, each with the language its script says it is in;
//! and, as the answer streams in, its sentences handed out once each is
//! finished ([`Reader`]).

/// What a sentence is said in, when its script says so: Hangul is Korean,
/// kana Japanese, Han alone Chinese — or Japanese, where that is what is
/// being spoken (`usual`: a line of kanji alone). `None`: a Latin, Cyrillic
/// … script, whose language the caller knows better.
pub fn script_language(text: &str, usual: Option<&str>) -> Option<&'static str> {
    let (mut hangul, mut kana, mut han) = (false, false, false);
    for c in text.chars() {
        match c as u32 {
            0xAC00..=0xD7AF | 0x1100..=0x11FF | 0x3130..=0x318F => hangul = true,
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => kana = true,
            0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0xF900..=0xFAFF => han = true,
            _ => {}
        }
    }
    if hangul {
        Some("ko")
    } else if kana || (han && usual.is_some_and(|l| l.starts_with("ja"))) {
        Some("ja")
    } else if han {
        Some("zh")
    } else {
        None
    }
}

/// `markdown` as it would be said, one paragraph, heading or list item a
/// line: what is only drawn or only read — code blocks, markup, an address
/// — left out or said in words.
pub fn speakable(markdown: &str) -> String {
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

/// One line's inline markdown as said: code as its words, a link as its
/// text, an address as "a link", emphasis and emoji dropped.
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
                out.push_str(&code_words(body[..end].trim()));
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
        // A word: said as one, or as the words of a name in code.
        if c.is_alphanumeric() || c == '_' {
            let end = rest
                .char_indices()
                .find(|&(i, ch)| {
                    !(ch.is_alphanumeric()
                        || ch == '_'
                        || ((ch == '.' || ch == '/' || ch == ':')
                            && rest[i + ch.len_utf8()..]
                                .starts_with(|n: char| n.is_alphanumeric() || n == '_')))
                })
                .map_or(rest.len(), |(i, _)| i);
            let word = &rest[..end];
            out.push_str(&if is_code_name(word) {
                code_words(word)
            } else {
                word.to_string()
            });
            rest = &rest[end..];
            continue;
        }
        out.push(c);
        rest = &rest[len..];
    }
    // One space where markup left several.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `word` is a name in code rather than a word: it has an
/// underscore, a dot or slash inside it, or capitals within (`cartTotal`).
fn is_code_name(word: &str) -> bool {
    let inner = word.trim_matches(|c: char| !c.is_alphanumeric());
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
    for part in sentence.split_inclusive([',', ';', '，', '；', '、']) {
        if !piece.is_empty() && piece.chars().count() + part.chars().count() > LONG {
            out.push(piece.trim().to_string());
            piece.clear();
        }
        piece.push_str(part);
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

/// The sentences of an answer as it streams in, each handed out once: the
/// last one only when the answer is over, as the words still coming may be
/// its own.
#[derive(Debug, Default)]
pub struct Reader {
    given: usize,
}

impl Reader {
    /// The sentences of `answer` (its markdown so far) not handed out yet;
    /// `done`: no more is coming.
    pub fn next(&mut self, answer: &str, done: bool) -> Vec<String> {
        let all = sentences(&speakable(answer));
        let ready = if done {
            all.len()
        } else {
            all.len().saturating_sub(1)
        };
        if ready <= self.given {
            return Vec::new();
        }
        let fresh = all[self.given..ready].to_vec();
        self.given = ready;
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
        let mut reader = Reader::default();
        assert!(reader.next("The total", false).is_empty());
        assert!(reader.next("The total adds them.", false).is_empty());
        assert_eq!(
            reader.next("The total adds them. Then it", false),
            ["The total adds them."]
        );
        assert!(
            reader
                .next("The total adds them. Then it", false)
                .is_empty()
        );
        assert_eq!(
            reader.next("The total adds them. Then it rounds.", true),
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
            reader.next("Like so:\n```rust\nlet x = 1;\nlet y", false),
            Vec::<String>::new()
        );
        assert_eq!(
            reader.next("Like so:\n```rust\nlet x = 1;\n```\nDone.", true),
            ["Like so:", "Done."]
        );
    }
}
