//! What was said, read as one of the window's commands when it plainly is
//! one ("open the call graph", "hide the minimap", "dark mode") — and as
//! nothing when it is more than that ("explain the call graph"), so the
//! words go to the agent instead.
//!
//! A title is matched by its words, in any order, the way people say it:
//! verbs that only mean "do it" (open, go to, toggle) count for nothing on
//! either side; a title's category (`View:`, `Sidebar:`) and its
//! parenthetical may be said or left out; and "show" and "hide" must agree
//! with the title, so "close the call graph" never opens it. Every word the
//! speaker said must belong to the title — an extra word means a request,
//! not a command — and when two titles fit equally well, neither is run.

use std::cmp::Reverse;

/// Words that only say "do it", on either side.
const VERBS: &[&str] = &[
    "go",
    "to",
    "run",
    "toggle",
    "view",
    "switch",
    "bring",
    "up",
    "launch",
    "start",
    "add",
    "set",
    "put",
    "use",
    "turn",
    "change",
    "do",
    "execute",
    "jump",
    "navigate",
    "take",
    "me",
    "i",
    "want",
    "would",
    "like",
    "can",
    "could",
    "you",
    "please",
    "hey",
    "ok",
    "okay",
    "clew",
    "clue",
    "now",
    "just",
    "lets",
    "the",
    "a",
    "an",
    "my",
    "this",
    "that",
    "into",
    "for",
    // The app itself, which every command is of ("change the app's
    // language"), and pressing what is named.
    "app",
    "apps",
    "application",
    "applications",
    "clews",
    "click",
    "press",
    "tap",
];

/// Of [`VERBS`], the small words that only link the others.
const LINKING: &[&str] = &[
    "to", "the", "a", "an", "my", "this", "that", "into", "for", "me", "i", "up",
];

/// Words that ask for something to be shown.
const SHOW: &[&str] = &["show", "open", "display", "reveal", "enable", "on"];
/// Words that ask for something to be put away.
const HIDE: &[&str] = &["hide", "close", "disable", "off", "dismiss"];

/// Words said for another, as the titles spell it.
const SYNONYMS: &[(&str, &str)] = &[
    ("mode", "theme"),
    ("preferences", "settings"),
    ("prefs", "settings"),
    ("options", "settings"),
    // What the app calls the reader's ("Your Notes"), the reader calls
    // their own.
    ("my", "your"),
];

/// Which way a title or an utterance turns something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Polarity {
    Show,
    Hide,
    Neither,
}

/// A title, read for matching.
#[derive(Debug)]
struct Title {
    /// Words that must be said.
    required: Vec<String>,
    /// Words that may be said: its category, its parenthetical.
    optional: Vec<String>,
    polarity: Polarity,
    /// Whether what must be said is its category: the rest of it only says
    /// what it does ("Ask: Hide").
    by_category: bool,
}

/// How well what was said fits a title, better the greater: its words said
/// exactly, its optional words said, its optional words left unsaid (fewer
/// is better), its own "do it" words said, and its length (shorter is
/// better).
type Score = (usize, usize, Reverse<usize>, usize, Reverse<usize>);

/// The index in `titles` of the command `utterance` names, if it plainly
/// names exactly one.
pub fn command(utterance: &str, titles: &[&str]) -> Option<usize> {
    let (said, polarity) = utterance_words(utterance);
    if said.is_empty() {
        return None;
    }
    // Every word said, the ones that only say "do it" too: which of a
    // title's own such words were said tells "add to ask" from "ask".
    let all_said = words(utterance);
    let mut best: Option<(Score, usize)> = None;
    let mut tied = false;
    for (index, text) in titles.iter().enumerate() {
        let title = read_title(text);
        if title.required.is_empty() {
            continue;
        }
        let agrees = match (title.polarity, polarity) {
            (Polarity::Show, Polarity::Hide) | (Polarity::Hide, Polarity::Show) => false,
            // "Close the call graph" does not open it.
            (Polarity::Neither, Polarity::Hide) => false,
            // A title that is only its category and what it does ("Find:
            // Close"): what it does is said, or "find" would close it.
            (Polarity::Show | Polarity::Hide, said) if title.by_category => said == title.polarity,
            _ => true,
        };
        if !agrees {
            continue;
        }
        let Some((exact, optional)) = fit(&said, &title) else {
            continue;
        };
        // Equally well said, the title that says no more than was said wins:
        // "view docs" is "View Docs", not "Sidebar: Docs"; "ask" is "Ask",
        // not "Add to Ask".
        let own = words(text);
        // Its own verbs, not the small words between them: "go to ask" is
        // Ask, "add to ask" Add to Ask.
        let verbs_said = own
            .iter()
            .filter(|w| {
                VERBS.contains(&w.as_str())
                    && !LINKING.contains(&w.as_str())
                    && all_said.contains(w)
            })
            .count();
        let score = (
            exact,
            optional,
            Reverse(title.optional.len() - optional),
            verbs_said,
            Reverse(own.len()),
        );
        match best {
            Some((top, _)) if score < top => {}
            Some((top, _)) if score == top => tied = true,
            _ => {
                best = Some((score, index));
                tied = false;
            }
        }
    }
    if tied { None } else { best.map(|(_, i)| i) }
}

/// Lowercase words, apostrophes dropped (`let's` → `lets`).
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace(['\'', '’'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The words of what was said that must find a place in a title, and which
/// way it asks to turn things.
fn utterance_words(utterance: &str) -> (Vec<String>, Polarity) {
    let mut polarity = Polarity::Neither;
    let mut out = Vec::new();
    for w in words(utterance) {
        let w = SYNONYMS
            .iter()
            .find(|(said, _)| *said == w)
            .map_or(w, |(_, meant)| meant.to_string());
        if SHOW.contains(&w.as_str()) {
            polarity = Polarity::Show;
        } else if HIDE.contains(&w.as_str()) {
            polarity = Polarity::Hide;
        } else if !VERBS.contains(&w.as_str()) {
            out.push(w);
        }
    }
    (out, polarity)
}

fn read_title(title: &str) -> Title {
    let title = title.trim_end_matches(['…', '.', '?', '!']);
    let (category, rest) = match title.split_once(':') {
        Some((category, rest)) => (category, rest),
        None => ("", title),
    };
    let (main, aside) = match rest.split_once('(') {
        Some((main, aside)) => (main, aside.trim_end_matches(')')),
        None => (rest, ""),
    };
    let mut polarity = Polarity::Neither;
    let mut required = Vec::new();
    // A title's "of" may go unsaid, or be heard as another small word
    // ("go to the top on the file"); said where a title has none, it is a
    // word of a request ("explain all of this").
    let mut main_optional = Vec::new();
    for w in words(main) {
        if w == "show" {
            polarity = Polarity::Show;
        } else if HIDE.contains(&w.as_str()) {
            polarity = Polarity::Hide;
        } else if w == "of" {
            main_optional.push(w);
        } else if !VERBS.contains(&w.as_str()) && !SHOW.contains(&w.as_str()) {
            required.push(w);
        }
    }
    let category: Vec<String> = words(category)
        .into_iter()
        .filter(|w| !VERBS.contains(&w.as_str()))
        .collect();
    let aside = words(aside)
        .into_iter()
        .filter(|w| !VERBS.contains(&w.as_str()));
    // Nothing but a verb after the category ("Ask: Hide", "Find: Close"):
    // the category is what is named, and must be said.
    let by_category = required.is_empty();
    let optional = if by_category {
        required = category;
        main_optional.into_iter().chain(aside).collect()
    } else {
        category
            .into_iter()
            .chain(main_optional)
            .chain(aside)
            .collect()
    };
    Title {
        required,
        optional,
        polarity,
        by_category,
    }
}

/// How well the words said fit `title`: `(exact, optional)` — how many of
/// its words were said exactly as spelled, and how many of its optional
/// ones were said — or `None` if one of its words was not said, or a word
/// was said that is not its.
fn fit(said: &[String], title: &Title) -> Option<(usize, usize)> {
    let mut used = vec![false; said.len()];
    let mut exact = 0;
    for w in &title.required {
        if place(said, &mut used, w)? {
            exact += 1;
        }
    }
    let mut optional = 0;
    for w in &title.optional {
        if place(said, &mut used, w).is_some() {
            optional += 1;
        }
    }
    used.iter().all(|u| *u).then_some((exact, optional))
}

/// Find `word` among the words said not yet placed — as spelled, then
/// close to it, then as two words run together ("mini map") — and mark it
/// placed. `Some(true)` when it was said exactly.
fn place(said: &[String], used: &mut [bool], word: &str) -> Option<bool> {
    if let Some(i) = (0..said.len()).find(|i| !used[*i] && said[*i] == word) {
        used[i] = true;
        return Some(true);
    }
    if let Some(i) = (0..said.len()).find(|i| !used[*i] && near(&said[*i], word)) {
        used[i] = true;
        return Some(false);
    }
    let pair = (0..said.len().saturating_sub(1)).find(|i| {
        !used[*i] && !used[i + 1] && {
            let joined = format!("{}{}", said[*i], said[i + 1]);
            joined == word || near(&joined, word)
        }
    })?;
    used[pair] = true;
    used[pair + 1] = true;
    Some(format!("{}{}", said[pair], said[pair + 1]) == word)
}

/// Whether `a` is `b` as a speech model might spell it: one plural from
/// the other, or one letter off in a long word.
fn near(a: &str, b: &str) -> bool {
    if inflected(a, b) || inflected(b, a) {
        return true;
    }
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    a.len() >= 5 && b.len() >= 5 && edits_at_most_one(&a, &b)
}

/// Whether `x` is `y` inflected: a plural (`files`, `hierarchies`), or a
/// verb's `-ing` or `-ed` (`debugging`, `zoomed`).
fn inflected(x: &str, y: &str) -> bool {
    let doubled = |stem: &str| {
        let mut chars = stem.chars().rev();
        matches!((chars.next(), chars.next()), (Some(a), Some(b)) if a == b)
            && stem[..stem.len() - 1] == *y
    };
    x.strip_suffix("es") == Some(y)
        || x.strip_suffix('s') == Some(y)
        || x.strip_suffix("ies")
            .is_some_and(|stem| y.strip_suffix('y') == Some(stem))
        || ["ing", "ed"].iter().any(|suffix| {
            x.strip_suffix(suffix)
                .is_some_and(|stem| stem == y || doubled(stem))
        })
}

fn edits_at_most_one(a: &[char], b: &[char]) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let Some(i) = short.iter().zip(long).position(|(x, y)| x != y) else {
        return true;
    };
    if short.len() == long.len() {
        short[i + 1..] == long[i + 1..]
    } else {
        short[i..] == long[i + 1..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window's worth of titles, as the palette lists them.
    const TITLES: &[&str] = &[
        "Call Graph",
        "Go to File…",
        "Go to Symbol…",
        "Find in Files",
        "Back",
        "Forward",
        "Zoom In",
        "Zoom Out",
        "Go to Definition",
        "Find References",
        "Go to Type Definition",
        "Call Hierarchy",
        "Expand Macro",
        "Explain",
        "Add to Ask",
        "Why Is This Here?",
        "Toggle Breakpoint",
        "Conditional Breakpoint…",
        "Debug: Continue",
        "Debug: Step Over",
        "Debug: Stop",
        "Overview",
        "Stats",
        "Settings",
        "Sidebar: Files",
        "Sidebar: Bookmarks",
        "View: Hide Sidebar",
        "View: Show Context Panel",
        "View: Debug Panel",
        "View: Hide Minimap",
        "View: Show Inlay Hints",
        "Glossary",
        "Skim (Fold Bodies)",
        "Export Notes…",
        "Open Folder…",
        "Theme: Light",
        "Theme: Dark",
        "Theme: Match System",
        "Ask: Hide",
        "Find: Close",
        "Debug: Turn Off Hover Evaluation",
        "Find…",
        "Sidebar: Find by Meaning",
        "Ask",
        "View Docs",
        "Sidebar: Docs",
        "Go to Top of File",
    ];

    fn said(utterance: &str) -> Option<&'static str> {
        command(utterance, TITLES).map(|i| TITLES[i])
    }

    #[test]
    fn commands_are_found_however_they_are_said() {
        for (utterance, title) in [
            ("Call graph.", "Call Graph"),
            ("Open the call graph", "Call Graph"),
            ("Hey Clew, show me the call graph please.", "Call Graph"),
            ("go to file", "Go to File…"),
            ("Go to definition.", "Go to Definition"),
            ("go to type definition", "Go to Type Definition"),
            ("find references", "Find References"),
            ("Zoom in.", "Zoom In"),
            ("zoom out", "Zoom Out"),
            ("go back", "Back"),
            ("Toggle breakpoint", "Toggle Breakpoint"),
            ("add a breakpoint", "Toggle Breakpoint"),
            ("conditional breakpoint", "Conditional Breakpoint…"),
            ("step over", "Debug: Step Over"),
            ("Continue.", "Debug: Continue"),
            ("stop debugging", "Debug: Stop"),
            ("open settings", "Settings"),
            ("open preferences", "Settings"),
            ("files sidebar", "Sidebar: Files"),
            ("show the bookmarks", "Sidebar: Bookmarks"),
            ("hide the sidebar", "View: Hide Sidebar"),
            ("show the context panel", "View: Show Context Panel"),
            ("Hide the mini map.", "View: Hide Minimap"),
            ("turn on inlay hints", "View: Show Inlay Hints"),
            ("debug panel", "View: Debug Panel"),
            ("skim", "Skim (Fold Bodies)"),
            ("skim, fold bodies", "Skim (Fold Bodies)"),
            ("export notes", "Export Notes…"),
            ("Dark mode.", "Theme: Dark"),
            ("switch to the light theme", "Theme: Light"),
            ("match system theme", "Theme: Match System"),
            ("Why is this here?", "Why Is This Here?"),
            ("open folder", "Open Folder…"),
            ("expand the macro", "Expand Macro"),
            // A title that is only its category and a verb: the category said.
            ("hide ask", "Ask: Hide"),
            ("Close Ask.", "Ask: Hide"),
            ("close find", "Find: Close"),
            (
                "turn off hover evaluation",
                "Debug: Turn Off Hover Evaluation",
            ),
            // Equally well said: the title that says no more than was said.
            ("find", "Find…"),
            ("find by meaning", "Sidebar: Find by Meaning"),
            ("ask", "Ask"),
            ("add to ask", "Add to Ask"),
            ("view docs", "View Docs"),
            ("docs sidebar", "Sidebar: Docs"),
            // "of" heard as another small word, or not said.
            ("to, go to the top on the file,", "Go to Top of File"),
            ("go to the top of the file", "Go to Top of File"),
            ("go to the top file", "Go to Top of File"),
            ("go to ask", "Ask"),
            ("switch to ask", "Ask"),
        ] {
            assert_eq!(said(utterance), Some(title), "{utterance:?}");
        }
    }

    /// The app itself is no word of a command: "change the app's theme to
    /// dark" is the theme; "click" before a name presses it.
    #[test]
    fn the_app_and_a_click_say_nothing_more() {
        assert_eq!(said("change the app's theme to dark"), Some("Theme: Dark"));
        assert_eq!(
            said("set the application theme to light"),
            Some("Theme: Light")
        );
        assert_eq!(said("click glossary"), Some("Glossary"));
        assert_eq!(said("press the call graph"), Some("Call Graph"));
    }

    /// "debugging" is "debug" to the speaker, and so to the matcher.
    #[test]
    fn a_word_is_matched_by_its_stem_or_one_slip() {
        assert!(near("files", "file"));
        assert!(near("hierarchies", "hierarchy"));
        assert!(near("minimab", "minimap"));
        assert!(!near("map", "mat"), "too short to guess");
        assert!(near("debugging", "debug"));
        assert!(near("zoomed", "zoom"));
        assert!(!near("glossary", "gloss"));
        assert!(edits_at_most_one(
            &"definition".chars().collect::<Vec<_>>(),
            &"defnition".chars().collect::<Vec<_>>()
        ));
    }

    /// "All" is a word of some titles: "explain all" is not "Explain".
    #[test]
    fn all_is_a_word_of_its_own() {
        let titles = ["Explain", "Explain All"];
        assert_eq!(command("explain all", &titles), Some(1));
        assert_eq!(command("explain", &titles), Some(0));
        assert_eq!(command("find all references", TITLES), None);
    }

    #[test]
    fn requests_are_left_for_the_agent() {
        // "of" where a title has none is a request's word.
        assert_eq!(command("explain all of this", &["Explain All"]), None);
        assert_eq!(command("explain all", &["Explain All"]), Some(0));
        assert_eq!(
            command("summary of this file", &["View: Hide File Summary"]),
            None
        );
        for utterance in [
            "explain the call graph",
            "what does this function do",
            "find where the config is loaded",
            "open the file that defines the parser",
            "zoom",
            "show",
            "",
            "um",
        ] {
            assert_eq!(said(utterance), None, "{utterance:?}");
        }
    }

    /// "close" and "hide" never run a command that shows, nor "show" one
    /// that hides.
    #[test]
    fn show_and_hide_must_agree() {
        assert_eq!(said("close the call graph"), None);
        assert_eq!(said("show the minimap"), None, "only Hide is listed");
        assert_eq!(said("hide the context panel"), None, "only Show is listed");
        assert_eq!(said("turn off inlay hints"), None);
        // "Close" in a title is a hide, as said.
        assert_eq!(said("open find"), Some("Find…"), "opened, not closed");
        assert_eq!(said("turn on hover evaluation"), None);
    }

    /// Two titles that fit equally well run neither.
    #[test]
    fn a_tie_runs_nothing() {
        let titles = ["Sidebar: Files", "Find: Files"];
        assert_eq!(command("files", &titles), None);
        // Said with its category, one wins.
        assert_eq!(command("sidebar files", &titles), Some(0));
        // An exact word beats a near one.
        assert_eq!(
            command("files", &["Go to File…", "Sidebar: Files"]),
            Some(1)
        );
        assert_eq!(command("file", &["Go to File…", "Sidebar: Files"]), Some(0));
    }
}
