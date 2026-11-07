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
}

/// Read before the palette's titles: theirs would take these for a command
/// that asks for the value again ("Go to Line…" for "go to line 30").
const FIRST: &[(Kind, &str)] = &[
    (Kind::Breakpoint, tr_noop!("toggle breakpoint on line {n}")),
    (Kind::Breakpoint, tr_noop!("set a breakpoint on line {n}")),
    (Kind::Breakpoint, tr_noop!("breakpoint on line {n}")),
    (Kind::Line, tr_noop!("go to line {n}")),
    (Kind::Line, tr_noop!("line {n}")),
    (Kind::Search, tr_noop!("search the project for {text}")),
    (Kind::Search, tr_noop!("search for {text}")),
    (Kind::ScrollDown, tr_noop!("scroll down {n}")),
    (Kind::ScrollUp, tr_noop!("scroll up {n}")),
    (Kind::Unwatch, tr_noop!("stop watching {text}")),
    (Kind::Unwatch, tr_noop!("remove watch {text}")),
    (Kind::Watch, tr_noop!("add a watch for {text}")),
    (Kind::Watch, tr_noop!("add watch {text}")),
    (Kind::Watch, tr_noop!("watch {text}")),
];

/// What says yes to a command that waits for one, and what says no: the
/// English words, and the reader's.
const YES: &str = tr_noop!("yes, yeah, yep, sure, okay, ok, run, run it, do it, go ahead, confirm");
const NO: &str = tr_noop!("no, nope, cancel, don't, do not, never mind, not now");

/// Whether `said` answers a command waiting for a yes: `Some(true)` for a
/// yes, `Some(false)` for a no, `None` when it says something else.
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
    let (yes, no) = (phrases(YES), phrases(NO));
    if yes.contains(&said) {
        return Some(true);
    }
    if no.contains(&said) {
        return Some(false);
    }
    // Said together ("yes, run it", "no, cancel"): every word one side's,
    // and none the other's.
    let words = |phrases: &[String]| -> Vec<String> {
        phrases
            .iter()
            .flat_map(|p| p.split(' ').map(str::to_string))
            .collect()
    };
    let (yes_words, no_words) = (words(&yes), words(&no));
    let said: Vec<&str> = said.split(' ').collect();
    let all_of = |side: &[String], other: &[String]| {
        said.iter()
            .all(|w| side.iter().any(|s| s == w) && !other.iter().any(|o| o == w))
    };
    if all_of(&yes_words, &no_words) {
        Some(true)
    } else if all_of(&no_words, &yes_words) {
        Some(false)
    } else {
        None
    }
}

/// Read after them: a title wins ("find references", "open the call
/// graph", "show the sidebar").
const AFTER: &[(Kind, &str)] = &[
    (Kind::Find, tr_noop!("find {text}")),
    (Kind::Open, tr_noop!("open {name}")),
    (Kind::GoTo, tr_noop!("go to {name}")),
    (Kind::GoTo, tr_noop!("jump to {name}")),
    (Kind::GoTo, tr_noop!("show {name}")),
];

impl App {
    /// Do what `said` asks with a value in it, if it is a phrase read before
    /// the palette's titles ([`FIRST`]): what was done, said for the
    /// reader, and the task doing it. `None` leaves it to the titles.
    pub(crate) fn spoken_first(&mut self, said: &str) -> Option<(String, Task<Message>)> {
        self.spoken(said, FIRST)
    }

    /// [`Self::spoken_first`] for the phrases read after the titles
    /// ([`AFTER`]); `None` leaves it to Ask.
    pub(crate) fn spoken_after(&mut self, said: &str) -> Option<(String, Task<Message>)> {
        self.spoken(said, AFTER)
    }

    fn spoken(&mut self, said: &str, phrases: &[(Kind, &str)]) -> Option<(String, Task<Message>)> {
        phrases.iter().find_map(|&(kind, template)| {
            let translated = crate::i18n::lookup(template);
            [translated, template]
                .into_iter()
                .find_map(|t| value_in(t, said))
                .and_then(|value| self.do_spoken(kind, &value))
        })
    }

    /// Do `kind` with `value`; `None` when it cannot be done here (no file
    /// open, no line number, nothing of the name).
    fn do_spoken(&mut self, kind: Kind, value: &str) -> Option<(String, Task<Message>)> {
        match kind {
            Kind::Line => {
                let line = number(value)?;
                self.active_viewer()?;
                let task = self.update(Message::Editor(EditorMsg::GotoLineNumber(line)));
                Some((tr!("Line {n}", n = line), task))
            }
            Kind::Breakpoint => {
                let line = number(value)?;
                let path = self.active_viewer()?.abs.clone();
                let task = self.update(Message::Debug(DebugMsg::BreakpointToggle { path, line }));
                Some((tr!("Breakpoint on line {n}", n = line), task))
            }
            Kind::Search => {
                self.proj.project.as_ref()?;
                self.proj.search.query = value.to_string();
                let shown = self.update(Message::Window(WindowMsg::SidebarTabPicked(
                    SidebarTab::Search,
                )));
                let run = self.update(Message::Nav(NavMsg::SearchSubmitted));
                Some((
                    tr!("Search for “{text}”", text = value),
                    Task::batch([shown, run]),
                ))
            }
            Kind::Find => {
                self.active_viewer()?;
                let open = self.update(Message::Editor(EditorMsg::FindOpened));
                let find = self.update(Message::Editor(EditorMsg::FindQueryChanged(
                    value.to_string(),
                )));
                Some((
                    tr!("Find “{text}” in this file", text = value),
                    Task::batch([open, find]),
                ))
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
                Some((crate::i18n::lookup(title).to_string(), task))
            }
            Kind::Watch => {
                self.proj.project.as_ref()?;
                self.debug.watch_input = value.to_string();
                let task = self.update(Message::Debug(DebugMsg::WatchAdd));
                Some((tr!("Watching “{text}”", text = value), task))
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
                Some((tr!("Stopped watching “{text}”", text = expr), task))
            }
        }
    }

    /// Open the project's file `name` says: the best of those whose path
    /// holds every word said ("cart dot py", "the cart file" — `cart.py`).
    fn open_named(&mut self, name: &str) -> Option<(String, Task<Message>)> {
        let words = file_words(name);
        if words.is_empty() {
            return None;
        }
        let files = self.proj.project.as_ref()?.files.clone();
        // The best-ranked file whose path holds every word, or — `by_name` —
        // whose name does, its extension aside.
        let find = |words: &[String], by_name: bool| {
            let query = words.join(" ");
            crate::session::finder::rank(&query, files.iter().map(|f| f.rel.as_str()), 8, true)
                .into_iter()
                .find(|&i| {
                    let rel = files[i].rel.to_lowercase();
                    let name = rel.rsplit('/').next().unwrap_or(&rel);
                    let held = if by_name {
                        name.split('.').next().unwrap_or(name)
                    } else {
                        rel.as_str()
                    };
                    words.iter().all(|w| held.contains(w.as_str()))
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
        Some((rel, task))
    }

    /// Go to the symbol `name` says, spelled as said or run together
    /// ("apply discount" — `apply_discount`, `applyDiscount`).
    fn go_to_named(&mut self, name: &str) -> Option<(String, Task<Message>)> {
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
        Some((entry.name, task))
    }
}

/// The value `said` gives `template`'s placeholder (`{n}`, `{text}`,
/// `{name}`), when it is the template said with a value: the words before
/// and after it as they are, ends of sentences and commas aside, in either
/// case.
fn value_in(template: &str, said: &str) -> Option<String> {
    let (head, rest) = template.split_once('{')?;
    let (_, tail) = rest.split_once('}')?;
    let said = plain(said);
    let (head, tail) = (plain(head), plain(tail));
    let rest = said.strip_prefix(head.as_str())?;
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
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `text` lowercase, without what ends or opens a sentence, its commas as
/// spaces, its spaces single, "please" said around it dropped.
fn plain(text: &str) -> String {
    let lower: String = text
        .to_lowercase()
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
    // Words a recognizer runs together or splits that the phrases spell
    // otherwise: "breakpoint online 6" is "breakpoint on line 6".
    let words: Vec<&str> = lower
        .trim_matches(ends)
        .split_whitespace()
        .flat_map(|w| match w {
            "online" => vec!["on", "line"],
            w => vec![w],
        })
        .collect();
    let words = words.join(" ").replace("break point", "breakpoint");
    let words = words.strip_prefix("please ").unwrap_or(&words);
    let words = words.strip_suffix(" please").unwrap_or(words);
    words.to_string()
}

/// A line number said: in figures ("30", "#30") or in English words ("thirty
/// five").
fn number(text: &str) -> Option<usize> {
    let text = text.trim().trim_start_matches('#');
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
        .filter(|w| !matches!(*w, "the" | "a" | "file" | "this" | "that"))
        .map(str::to_string)
        .collect()
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
        assert_eq!(number("zero"), None);
        assert_eq!(number("quantity"), None);
    }

    /// A file said: its dot said, what only says it is a file left out.
    #[test]
    fn a_file_is_said_as_its_name() {
        assert_eq!(file_words("cart dot py"), ["cart.py"]);
        assert_eq!(file_words("the cart file"), ["cart"]);
        assert_eq!(squeezed("apply discount"), squeezed("apply_discount"));
        assert_eq!(squeezed("Apply Discount"), squeezed("applyDiscount"));
    }
}
