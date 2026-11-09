//! What on screen a word can press: the buttons of the dialog on top, a
//! setting's switch and its choices, the rows of a list — gathered by
//! building the view as it is drawn ([`gather`]), so that voice reaches
//! exactly what the reader sees, and nothing they cannot.
//!
//! A control offers itself where it is built — [`said`] around the message
//! its press sends, [`row`] for a list's row, [`answer`] for the yes or the
//! no of a question — and the offer is kept only while [`gather`] builds
//! the view: drawing a frame costs no more than a look at a thread-local.
//!
//! What is offered is the reader's to press by voice, never the agent's:
//! a question's Allow, a host key's Trust, an approval's Run it are among
//! them, and only the person at the window may answer those.

use std::cell::RefCell;

use crate::{App, Message};

/// Where on screen a target is: what is over what. A dialog over the
/// window takes the voice as it takes the pointer; the tutorial, over
/// everything, takes it from all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Layer {
    /// The window: its bars, the sidebar, the code, the margin.
    Window,
    /// Ask, floating over the window.
    Ask,
    /// A dialog, a menu, a panel over everything else, holding the keys.
    Overlay,
    /// The tutorial's callout, over all.
    Tutorial,
}

/// How a target answers the question it is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// It allows, accepts, goes ahead: what "yes" says.
    Yes,
    /// It declines, cancels: what "no" says.
    No,
}

/// One thing on screen a word can press.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    /// Its name in English — the catalogs' text it is shown from — or its
    /// own name (a file, a server, a model) when that is all it has.
    pub title: String,
    /// As the reader sees it.
    pub shown: String,
    /// What pressing it sends: what its click sends.
    pub message: Message,
    pub layer: Layer,
    /// The list it is a row of, and its place there (from 1).
    pub row: Option<(usize, usize)>,
    pub answer: Option<Answer>,
    /// A field what is said is typed into: what it holds, and what typing
    /// in it sends.
    pub typing: Option<Typing>,
}

/// A field's text, and what typing in it sends.
pub(crate) type Typing = (String, fn(String) -> Message);

/// What is being gathered: the targets so far, and where the view is.
struct Gathering {
    targets: Vec<Target>,
    layer: Layer,
    /// The list rows are numbered in now, and how many it has so far.
    list: usize,
    rows: usize,
    /// What the controls built now belong to ([`about`]), innermost last.
    about: Vec<String>,
}

thread_local! {
    static GATHERING: RefCell<Option<Gathering>> = const { RefCell::new(None) };
}

/// Whether the view is being built to be gathered: what a label costs to
/// make is spent only then.
pub(crate) fn gathering() -> bool {
    GATHERING.with(|g| g.borrow().is_some())
}

fn push(title: String, shown: String, message: &Message, answer: Option<Answer>, row: bool) {
    GATHERING.with(|g| {
        if let Some(g) = g.borrow_mut().as_mut() {
            let row = row.then(|| {
                g.rows += 1;
                (g.list, g.rows)
            });
            // Also by what it belongs to: "Remove: rust-analyzer", where
            // every server has its Remove.
            if let Some(about) = g.about.last() {
                g.targets.push(Target {
                    title: format!("{title}: {}", title_of(about)),
                    shown: format!("{shown}: {about}"),
                    message: message.clone(),
                    layer: g.layer,
                    row: None,
                    answer,
                    typing: None,
                });
            }
            g.targets.push(Target {
                title,
                shown,
                message: message.clone(),
                layer: g.layer,
                row,
                answer,
                typing: None,
            });
        }
    });
}

/// The English a label `shown` in the reader's language is the
/// translation of — or the label itself.
fn title_of(shown: &str) -> String {
    crate::i18n::english_of(shown).unwrap_or(shown).to_string()
}

/// Offer `message` under the label it is `shown` with.
pub(crate) fn offer(shown: &str, message: &Message) {
    if gathering() {
        push(title_of(shown), shown.to_string(), message, None, false);
    }
}

/// `message`, offered under the label it is `shown` with: what a button's
/// press is wrapped in, where it is built.
pub(crate) fn said(shown: &str, message: Message) -> Message {
    offer(shown, &message);
    message
}

/// [`said`], for a label made to be said (`shown`, a path and a line, a
/// question without its marks): made only while gathering, never for a
/// frame that is only drawn.
pub(crate) fn said_with(shown: impl FnOnce() -> String, message: Message) -> Message {
    if gathering() {
        offer(&shown(), &message);
    }
    message
}

/// [`said_to`], the item's name made only while gathering ([`said_with`]).
pub(crate) fn said_to_with(
    action: &str,
    item: impl FnOnce() -> String,
    message: Message,
) -> Message {
    if gathering() {
        return said_to(action, &item(), message);
    }
    message
}

/// `message`, offered as `action` done to `item` ("Remove: Whisper Small"):
/// a button one of several rows has, each named by what it is done to.
pub(crate) fn said_to(action: &str, item: &str, message: Message) -> Message {
    if gathering() {
        push(
            format!("{}: {}", title_of(action), title_of(item)),
            format!("{action}: {item}"),
            &message,
            None,
            false,
        );
    }
    message
}

/// A switch called `name`, `on` now, that `set` turns on or off: offered as
/// "{name}: On" and "{name}: Off" — each setting it so, whatever it is
/// now — and as its name alone, which turns it over.
pub(crate) fn switch(name: &str, on: bool, set: impl Fn(bool) -> Message) {
    if gathering() {
        said_to(name, crate::tr!("On"), set(true));
        said_to(name, crate::tr!("Off"), set(false));
        offer(name, &set(!on));
    }
}

/// `message`, offered under `shown` as the `answer` it gives the question
/// on screen: "yes" and "no" press it too.
pub(crate) fn answer(shown: &str, answer: Answer, message: Message) -> Message {
    if gathering() {
        push(
            title_of(shown),
            shown.to_string(),
            &message,
            Some(answer),
            false,
        );
    }
    message
}

/// A field that holds `value`, `on_input` sending what it is changed to:
/// what is said, when nothing else is meant, is typed into it while it is
/// on top (a note, a breakpoint's condition, the command bar's query).
pub(crate) fn field(value: &str, on_input: fn(String) -> Message) {
    GATHERING.with(|g| {
        if let Some(g) = g.borrow_mut().as_mut() {
            g.targets.push(Target {
                title: String::new(),
                shown: String::new(),
                message: on_input(value.to_string()),
                layer: g.layer,
                row: None,
                answer: None,
                typing: Some((value.to_string(), on_input)),
            });
        }
    });
}

/// `build`, whose controls belong to `item` — a server, a model, a saved
/// host — each offered also as done to it ("Remove: rust-analyzer"), so
/// that one of several rows' alike buttons can be named.
pub(crate) fn about<T>(item: &str, build: impl FnOnce() -> T) -> T {
    let pushed = GATHERING.with(|g| {
        g.borrow_mut()
            .as_mut()
            .map(|g| g.about.push(item.to_string()))
            .is_some()
    });
    let built = build();
    if pushed {
        GATHERING.with(|g| {
            if let Some(g) = g.borrow_mut().as_mut() {
                g.about.pop();
            }
        });
    }
    built
}

/// [`about`], the item's name made only while gathering ([`said_with`]).
pub(crate) fn about_with<T>(item: impl FnOnce() -> String, build: impl FnOnce() -> T) -> T {
    if gathering() {
        about(&item(), build)
    } else {
        build()
    }
}

/// A list begins: the rows offered after it are numbered from 1, as the
/// reader counts them down the list.
pub(crate) fn list() {
    GATHERING.with(|g| {
        if let Some(g) = g.borrow_mut().as_mut() {
            g.list += 1;
            g.rows = 0;
        }
    });
}

/// `message`, offered as the next row of the list begun last
/// ([`list`]), under what the row says.
pub(crate) fn row(shown: impl FnOnce() -> String, message: Message) -> Message {
    if gathering() {
        let shown = shown();
        push(shown.clone(), shown, &message, None, true);
    }
    message
}

/// What is built from here on is in `layer`.
pub(crate) fn layer(layer: Layer) {
    GATHERING.with(|g| {
        if let Some(g) = g.borrow_mut().as_mut() {
            g.layer = layer;
        }
    });
}

/// Every target on screen in `app`'s window now, in the order it is drawn
/// (top to bottom, as a list reads) — of the layer on top only, as a
/// dialog over the window leaves nothing under it to press.
pub(crate) fn gather(app: &App) -> Vec<Target> {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            GATHERING.with(|g| *g.borrow_mut() = None);
        }
    }
    GATHERING.with(|g| {
        *g.borrow_mut() = Some(Gathering {
            targets: Vec::new(),
            layer: Layer::Window,
            list: 0,
            rows: 0,
            about: Vec::new(),
        })
    });
    let stop = Stop;
    drop(crate::ui::view(app));
    let targets = GATHERING
        .with(|g| g.borrow_mut().take())
        .map(|g| g.targets)
        .unwrap_or_default();
    drop(stop);
    on_top(targets, app.ask_view == crate::AskView::Expanded)
}

/// What building `build` offers, all of it — one part of the view,
/// gathered as [`gather`] gathers the whole.
#[cfg(test)]
pub(crate) fn gather_of<T>(build: impl FnOnce() -> T) -> Vec<Target> {
    GATHERING.with(|g| {
        *g.borrow_mut() = Some(Gathering {
            targets: Vec::new(),
            layer: Layer::Window,
            list: 0,
            rows: 0,
            about: Vec::new(),
        })
    });
    drop(build());
    GATHERING
        .with(|g| g.borrow_mut().take())
        .map(|g| g.targets)
        .unwrap_or_default()
}

/// Of `targets`, those a press can reach: the tutorial's while it runs, a
/// dialog's while one is up, Ask's while it is large over the window
/// (`ask_large`), else the window's and Ask's.
fn on_top(targets: Vec<Target>, ask_large: bool) -> Vec<Target> {
    let top = targets
        .iter()
        .map(|t| t.layer)
        .max()
        .unwrap_or(Layer::Window);
    if top >= Layer::Overlay || (ask_large && top == Layer::Ask) {
        targets.into_iter().filter(|t| t.layer == top).collect()
    } else {
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(title: &str, layer: Layer) -> Target {
        Target {
            title: title.into(),
            shown: title.into(),
            message: Message::Noop,
            layer,
            row: None,
            answer: None,
            typing: None,
        }
    }

    /// A dialog over the window leaves nothing under it to press; with
    /// none, the window and Ask both can be.
    #[test]
    fn only_what_is_on_top_can_be_pressed() {
        let titles = |targets: Vec<Target>| -> Vec<String> {
            on_top(targets, false)
                .into_iter()
                .map(|t| t.title)
                .collect()
        };
        assert_eq!(
            titles(vec![
                target("Diff", Layer::Window),
                target("Stop", Layer::Ask),
            ]),
            ["Diff", "Stop"]
        );
        assert_eq!(
            titles(vec![
                target("Diff", Layer::Window),
                target("Download", Layer::Overlay),
                target("Stop", Layer::Ask),
            ]),
            ["Download"]
        );
        assert_eq!(
            titles(vec![
                target("Download", Layer::Overlay),
                target("Next", Layer::Tutorial),
            ]),
            ["Next"]
        );
        // Ask, large over the window, leaves nothing under it to press.
        let large: Vec<String> = on_top(
            vec![target("Diff", Layer::Window), target("Stop", Layer::Ask)],
            true,
        )
        .into_iter()
        .map(|t| t.title)
        .collect();
        assert_eq!(large, ["Stop"]);
    }

    /// A label made to be said is made only while gathering: a frame that
    /// is only drawn spends nothing on it.
    #[test]
    fn a_label_is_made_only_while_gathering() {
        let made = std::cell::Cell::new(0);
        let label = || {
            made.set(made.get() + 1);
            "src/lib.rs 3".to_string()
        };
        let _ = said_with(label, Message::Noop);
        let _ = said_to_with("Remove", label, Message::Noop);
        about_with(label, || ());
        assert_eq!(made.get(), 0, "made for a drawn frame");
        let gathered = gather_of(|| {
            let _ = said_with(label, Message::Noop);
            let _ = said_to_with("Remove", label, Message::Noop);
        });
        assert_eq!(made.get(), 2);
        let titles: Vec<&str> = gathered.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["src/lib.rs 3", "Remove: src/lib.rs 3"]);
    }

    /// Nothing is kept while the window is only drawn: an offer outside a
    /// gathering is dropped.
    #[test]
    fn an_offer_is_kept_only_while_gathering() {
        assert!(!gathering());
        let _ = said("Download", Message::Noop);
        assert!(GATHERING.with(|g| g.borrow().is_none()));
    }
}
