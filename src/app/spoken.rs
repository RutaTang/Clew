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
const YES: &str = tr_noop!("yes, yeah, yep, sure, okay, ok, run, run it, do it, go ahead, confirm");
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
        PHRASES.iter().find_map(|&(kind, template)| {
            let translated = crate::i18n::lookup(template);
            [translated, template]
                .into_iter()
                .find_map(|t| value_in(t, said))
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
            if matches!(c, ',' | '，' | '、') {
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
    let said: Vec<&str> = text.trim_matches(ends).split_whitespace().collect();
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

    /// A question on screen is answered only by what is all a yes or all
    /// a no: what merely opens with one ("run the tests") is not.
    #[test]
    fn a_question_is_answered_by_a_yes_alone() {
        for said in ["yes", "Yes, do it.", "ok", "go ahead please", "run it"] {
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
