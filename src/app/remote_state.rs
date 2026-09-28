//! A remote project's `.clew/` session state over the protocol: fetching it,
//! adopting what arrives, and writing changes back with the dirty / unsent /
//! in-flight / rescue bookkeeping that keeps a change from being lost across a
//! dead transport.

use crate::app::prelude::*;
use crate::*;

/// How often a journaled edit the server could not apply is tried in all
/// before it is given up (see [`App::retry_remote_edit`]).
pub(crate) const EDIT_ATTEMPTS: u32 = 3;

/// The pause before a journaled edit held back goes again — multiplied by
/// its failures so far.
pub(crate) const EDIT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

/// The most edits of ONE store the journal holds — sent and unanswered, or
/// waiting their turn. A further edit of that store is refused, saying why
/// on the status line, until the server has answered some of them
/// ([`App::edit_remote_state`]).
///
/// The bound is what keeps de-duplication from being outrun. The server
/// knows a replayed edit by its id, and keeps the last
/// [`clew_core::statefile::EDIT_LEDGER_CAP`] ids of each store; a journal
/// could otherwise grow past that while the link is down, and replay an edit
/// whose id the ledger had forgotten — which the server then applies again
/// (a toggle undoing itself). Far below the ledger's cap, because the ledger
/// is shared: every window and machine editing the store adds its ids to
/// the same list.
pub(crate) const EDIT_JOURNAL_CAP: usize = 64;

const _: () = assert!(
    EDIT_JOURNAL_CAP * 16 <= clew_core::statefile::EDIT_LEDGER_CAP,
    "the journal must stay far below the server's ledger"
);

/// The per-project state files a REMOTE project loads over the protocol.
/// Order does not matter; each is requested and applied independently.
pub(crate) const REMOTE_STATE_FILES: &[&str] = &[
    "history.json",
    "bookmarks.json",
    "notes.json",
    "reading.toml",
    walkthrough::LIBRARY_REL,
];

/// Journaled edits closing a window would leave unsaved, and why each is:
/// no transport took it, the host has not confirmed it, the host could not
/// save it — or could not, and it was given up. What the shell asks the user
/// about before it lets the window go (`crate::shell`), and what stderr says
/// was lost when it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnsavedEdits {
    /// How many, for every reason.
    pub(crate) count: usize,
    /// Which, by their ids (`RemoteEdit::id`): what the user agreed to
    /// lose, so an edit made while they were being asked is not lost with
    /// the ones they were told about.
    pub(crate) ids: Vec<String>,
    /// The project they were made in, as the user knows it:
    /// `user@host:/path`.
    pub(crate) project: String,
    /// What the ones still in the journal changed, by store, as the user
    /// knows it: `bookmarks`, `notes`, `walkthroughs`.
    pub(crate) stores: Vec<&'static str>,
    /// How many of those no transport took: there was none, or its queue
    /// was full.
    pub(crate) unsent: usize,
    /// How many of those went to the host, which has not confirmed them:
    /// over a link that died without anyone noticing, they may never have
    /// arrived.
    pub(crate) unconfirmed: usize,
    /// How many of those the host could not save — an I/O error, a lock —
    /// and are tried again while the window stays open.
    pub(crate) failed: usize,
    /// The ones the host could not save that were given up — out of tries,
    /// refused, or taken over by a later change to the same entry — each as
    /// the user knows it ([`entry_name`]): lost, whatever the answer.
    pub(crate) lost: Vec<String>,
}

/// Why a journaled edit is not saved yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// No transport took it.
    Unsent,
    /// The host has not answered it.
    Unconfirmed,
    /// The host could not save it, and it is tried again.
    Failed,
}

impl UnsavedEdits {
    /// How many are still in the journal: not saved yet, and not given up.
    fn pending(&self) -> usize {
        self.unsent + self.unconfirmed + self.failed
    }

    /// The reason every pending edit shares; `None` when they have several,
    /// or there are none.
    fn why(&self) -> Option<Why> {
        let mut reasons = [
            (self.unsent, Why::Unsent),
            (self.unconfirmed, Why::Unconfirmed),
            (self.failed, Why::Failed),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0);
        match (reasons.next(), reasons.next()) {
            (Some((_, why)), None) => Some(why),
            _ => None,
        }
    }

    /// Whether every one of them was sent, and waits for the host's answer.
    fn all_unconfirmed(&self) -> bool {
        self.why() == Some(Why::Unconfirmed) && self.lost.is_empty()
    }

    /// "3 changes to bookmarks and notes in me@host:/srv/app": the pending
    /// ones.
    fn describe(&self) -> String {
        let stores = match self.stores.as_slice() {
            [] => "project state".to_string(),
            stores => and_list(&stores.iter().map(|s| s.to_string()).collect::<Vec<_>>()),
        };
        format!(
            "{} to {stores} in {}",
            changes(self.pending()),
            self.project
        )
    }

    /// Why the pending ones are not saved, after [`Self::describe`]: "could
    /// not be sent to the host" — or, when they are not all alike, how many
    /// for each reason.
    fn fate(&self) -> String {
        let n = self.pending();
        match self.why() {
            Some(Why::Unsent) => "could not be sent to the host".to_string(),
            Some(Why::Unconfirmed) => format!(
                "{} sent to the host, but it has not confirmed {}: the connection may be down",
                were(n),
                them(n)
            ),
            Some(Why::Failed) => "could not be saved by the host".to_string(),
            None => format!("are not saved to the host: {}", self.breakdown()),
        }
    }

    /// How many pending ones there are for each reason: "1 could not be
    /// sent, 2 were sent but not confirmed and 1 could not be saved by the
    /// host".
    fn breakdown(&self) -> String {
        let mut parts = Vec::new();
        if self.unsent > 0 {
            parts.push(format!("{} could not be sent", self.unsent));
        }
        if self.unconfirmed > 0 {
            let n = self.unconfirmed;
            parts.push(format!("{n} {} sent but not confirmed", were(n)));
        }
        if self.failed > 0 {
            parts.push(format!("{} could not be saved by the host", self.failed));
        }
        and_list(&parts)
    }

    /// One line of a quit's list for this window's pending edits: "2
    /// changes to notes in me@host:/srv/app could not be sent" — or, when
    /// they are not all alike, how many for each reason.
    fn line(&self) -> String {
        let n = self.pending();
        match self.why() {
            Some(Why::Unsent) => format!("{} could not be sent", self.describe()),
            Some(Why::Unconfirmed) => {
                format!("{} {} sent but not confirmed", self.describe(), were(n))
            }
            Some(Why::Failed) => format!("{} could not be saved by the host", self.describe()),
            None => format!("{}: {}", self.describe(), self.breakdown()),
        }
    }

    /// The sentence naming the edits given up: "The host could not save
    /// the change to the bookmark at src/lib.rs:12: it is lost." — with
    /// the project when nothing else names it.
    fn lost_sentence(&self, with_project: bool) -> String {
        let n = self.lost.len();
        let project = if with_project {
            format!(" in {}", self.project)
        } else {
            String::new()
        };
        format!(
            "The host could not save {}{project}: {} lost.",
            lost_list(&self.lost),
            they_are(n)
        )
    }

    /// Whether some of them may yet be saved, sent before the host went
    /// quiet: closing loses them only perhaps.
    fn may_be_lost(&self) -> &'static str {
        if self.unconfirmed > 0 {
            "may be"
        } else {
            "are"
        }
    }
}

/// "1 change", "3 changes".
fn changes(n: usize) -> String {
    match n {
        1 => "1 change".to_string(),
        n => format!("{n} changes"),
    }
}

/// "a", "a and b", "a, b and c".
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// How many entries given up a question or a status line names, before it
/// says how many more there are.
const LOST_NAMED: usize = 3;

/// Edits given up, by the entries they change (see [`entry_name`]): "the
/// change to the bookmark at a.rs:1", "the changes to the note on f in
/// lib.rs, the walkthrough “parsing” and 2 more".
fn lost_list(names: &[String]) -> String {
    let (named, more) = names.split_at(names.len().min(LOST_NAMED));
    let mut items = named.to_vec();
    if !more.is_empty() {
        items.push(format!("{} more", more.len()));
    }
    let changes = if names.len() == 1 {
        "change"
    } else {
        "changes"
    };
    format!("the {changes} to {}", and_list(&items))
}

/// The entry journaled edit `edit` changes, as the user knows it: "the
/// bookmark at src/lib.rs:12", "the note on parse in src/lib.rs", "the
/// walkthrough “how parsing works”". Read from the fields that identify the
/// entry (`StateMerge::key`), which each store sets.
pub(crate) fn entry_name(edit: &RemoteEdit) -> String {
    let merge = &edit.merge;
    let field = |name: &str| {
        let at = merge.key_fields.iter().position(|f| f == name)?;
        merge.key.get(at).map(|value| match value {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    };
    match edit.rel.as_str() {
        rel if rel == bookmarks::REL => match (field("rel"), field("line")) {
            (Some(file), Some(line)) => format!("the bookmark at {file}:{line}"),
            _ => "a bookmark".to_string(),
        },
        rel if rel == notes::REL => match (field("rel"), field("symbol")) {
            (Some(file), Some(symbol)) => format!("the note on {symbol} in {file}"),
            _ => "a note".to_string(),
        },
        rel if rel == walkthrough::LIBRARY_REL => match field("scope") {
            Some(scope) => format!("the walkthrough “{}”", shortened(&scope)),
            None => "a walkthrough".to_string(),
        },
        rel => format!("an entry of .clew/{rel}"),
    }
}

/// `text` cut to a length a sentence can carry: a walkthrough's scope is
/// the prompt it was made for.
fn shortened(text: &str) -> String {
    const MOST: usize = 40;
    if text.chars().count() <= MOST {
        return text.to_string();
    }
    let cut: String = text.chars().take(MOST - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Whether journaled edits `a` and `b` change the same entry of the same
/// store. Changes to different entries land the same in either order;
/// changes to one entry do not.
fn same_entry(a: &RemoteEdit, b: &RemoteEdit) -> bool {
    a.rel == b.rel && a.merge.key_fields == b.merge.key_fields && a.merge.key == b.merge.key
}

/// The prefix of the status line while a close waits for the host.
const CLOSING_WAITS: &str = "Closing — waiting for the host to confirm ";

/// The prefix of the status line while a quit waits for the hosts.
const QUITTING_WAITS: &str = "Quitting — waiting for the host to confirm ";

/// A journaled edit the server answered without applying it
/// ([`App::retry_remote_edit`], [`App::give_up_remote_edit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unapplied {
    /// Its store, under `.clew/`.
    pub(crate) rel: String,
    /// The entry it changes, as the user knows it ([`entry_name`]).
    pub(crate) what: String,
    /// It left the journal: given up. `false` while it waits to be tried
    /// again.
    pub(crate) given_up: bool,
    /// Given up, it was the last edit of its store in the journal: the
    /// store is read again, so the screen matches the disk.
    pub(crate) last: bool,
}

/// "it is" for one change, "they are" for more.
fn they_are(count: usize) -> &'static str {
    if count == 1 { "it is" } else { "they are" }
}

/// "it" for one change, "they" for more.
fn they(count: usize) -> &'static str {
    if count == 1 { "it" } else { "they" }
}

/// "it" for one change, "them" for more.
fn them(count: usize) -> &'static str {
    if count == 1 { "it" } else { "them" }
}

/// "was" for one change, "were" for more.
fn were(count: usize) -> &'static str {
    if count == 1 { "was" } else { "were" }
}

/// A question's title: how many changes are not saved to `hosts` — or,
/// when every one was sent and waits for an answer, not confirmed.
fn question_title(count: usize, confirming: bool, hosts: &str) -> String {
    match (count, confirming) {
        (1, true) => "A change is not confirmed by the host".to_string(),
        (n, true) => format!("{n} changes are not confirmed by {hosts}"),
        (1, false) => "A change is not saved to the host".to_string(),
        (n, false) => format!("{n} changes are not saved to {hosts}"),
    }
}

/// What keeping the pending edits of `lost` does for them — `connected`
/// says what connects again: "it is", "the window is" — and what giving
/// them up does.
fn keep_or_lose(lost: &UnsavedEdits, connected: &str) -> (String, String) {
    let n = lost.pending();
    match lost.why() {
        Some(Why::Unsent) => (
            format!("{} sent once {connected} connected again", they_are(n)),
            format!("{} lost", they_are(n)),
        ),
        Some(Why::Unconfirmed) => (
            format!(
                "{} sent again once {connected} connected again",
                they_are(n)
            ),
            format!("{} may be lost", they(n)),
        ),
        Some(Why::Failed) => (
            format!("{} tried again", they_are(n)),
            format!("{} lost", they_are(n)),
        ),
        None => (
            "clew keeps trying to save them".to_string(),
            format!("they {} lost", lost.may_be_lost()),
        ),
    }
}

/// The title and message of the question a window asks before it closes
/// with edits it would leave unsaved (`lost`): every one, whatever the
/// reason, in one question — how many for each reason when they differ,
/// and the ones given up by what they changed.
pub(crate) fn close_question(lost: &UnsavedEdits) -> (String, String) {
    let title = question_title(lost.count, lost.all_unconfirmed(), "the host");
    let mut message = Vec::new();
    if lost.pending() > 0 {
        let (keep, close) = keep_or_lose(lost, "it is");
        message.push(format!(
            "{} {}. Keep the window open, and {keep}. Close it, and {close}.",
            lost.describe(),
            lost.fate()
        ));
    }
    if !lost.lost.is_empty() {
        message.push(lost.lost_sentence(lost.pending() == 0));
        if lost.pending() == 0 {
            message.push(format!(
                "Keep the window open to make {} again, or close it.",
                them(lost.lost.len())
            ));
        }
    }
    (title, message.join(" "))
}

/// The title and message of the question a quit asks when windows hold
/// edits it would leave unsaved (`lost`, one entry per window) — every one
/// of them, whatever the reason, in one question.
pub(crate) fn quit_question(lost: &[UnsavedEdits]) -> (String, String) {
    let total: usize = lost.iter().map(|l| l.count).sum();
    let confirming = !lost.is_empty() && lost.iter().all(UnsavedEdits::all_unconfirmed);
    if let [one] = lost {
        let title = question_title(total, confirming, "the host");
        let mut message = Vec::new();
        if one.pending() > 0 {
            let (keep, quit) = keep_or_lose(one, "the window is");
            message.push(format!(
                "{} {}. Cancel to keep clew open, and {keep}. Quit, and {quit}.",
                one.describe(),
                one.fate()
            ));
        }
        if !one.lost.is_empty() {
            message.push(one.lost_sentence(one.pending() == 0));
            if one.pending() == 0 {
                message.push(format!(
                    "Cancel to keep clew open and make {} again, or quit.",
                    them(one.lost.len())
                ));
            }
        }
        return (title, message.join(" "));
    }
    let title = question_title(total, confirming, "their hosts");
    let given_up = lost.iter().any(|l| !l.lost.is_empty());
    let all = |why: Why| !given_up && lost.iter().all(|l| l.why() == Some(why));
    let list = || {
        lost.iter()
            .map(|l| format!("• {}", l.describe()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if all(Why::Unconfirmed) {
        let message = format!(
            "These changes were sent to their hosts, which have not confirmed them:\n\n{}\n\n\
             The connections may be down. Cancel to keep clew open, and they are sent again \
             once the windows are connected again. Quit, and they may be lost.",
            list()
        );
        return (title, message);
    }
    if all(Why::Unsent) {
        let they_are = they_are(total);
        let message = format!(
            "These changes could not be sent to their hosts:\n\n{}\n\nCancel to keep clew \
             open, and {they_are} sent once the windows are connected again. Quit, and \
             {they_are} lost.",
            list()
        );
        return (title, message);
    }
    // For several reasons, or with edits given up: a line for each.
    let mut lines: Vec<String> = lost
        .iter()
        .filter(|l| l.pending() > 0)
        .map(|l| format!("• {}", l.line()))
        .collect();
    lines.extend(lost.iter().filter(|l| !l.lost.is_empty()).map(|l| {
        let n = l.lost.len();
        format!(
            "• {} in {}: the host could not save {}, and {} lost",
            lost_list(&l.lost),
            l.project,
            them(n),
            they_are(n)
        )
    }));
    let end = if lost.iter().all(|l| l.pending() == 0) {
        "Cancel to keep clew open and make them again, or quit.".to_string()
    } else {
        let others = if given_up { "the others" } else { "them" };
        let may_be = if lost.iter().any(|l| l.unconfirmed > 0) {
            "may be"
        } else {
            "are"
        };
        format!(
            "Cancel to keep clew open, and clew keeps trying to save {others}. Quit, and they \
             {may_be} lost."
        )
    };
    let message = format!(
        "These changes are not saved to their hosts:\n\n{}\n\n{end}",
        lines.join("\n")
    );
    (title, message)
}

impl App {
    /// Ask the server for the `.clew/` session-state files of a REMOTE
    /// project ([`REMOTE_STATE_FILES`]: history, bookmarks, notes, reading
    /// target, the walkthrough library) — they live where
    /// the project lives; a same-pathed local file is another machine's
    /// data. The replies land as `StateContent` notifications.
    pub(crate) fn request_remote_state(&mut self) {
        let (true, Some(root)) = (
            self.server.is_up(),
            self.proj
                .project
                .as_ref()
                .map(|p| p.root.to_string_lossy().into_owned()),
        ) else {
            return;
        };
        // Until each file's real content arrives, what this client holds for
        // it is an EMPTY baseline (`on_scan_done` starts every remote project
        // that way). A save in that window would push the baseline back and
        // wipe the remote file — these writes replace it wholesale, and an
        // empty list serializes to `None`, which DELETES it. So each rel is
        // marked outstanding here and only becomes WHOLESALE-writable when it
        // loads. It bounds `write_remote_state` only: an `EditState` names one
        // entry and carries no baseline, so it is safe to send straight away
        // (see `edit_remote_state`).
        //
        // The DIRTY set is deliberately kept: this also runs on a reconnect,
        // where it holds changes the user made while the link was down, and
        // dropping them here would lose exactly the edits this re-read exists
        // to rescue. A fresh project clears both in `on_scan_done`.
        self.proj.remote_state_pending.clear();
        for rel in REMOTE_STATE_FILES {
            self.proj.remote_state_pending.insert((*rel).to_string());
            let _ = self.send_to_server(clew_protocol::Request::ReadState {
                root: root.clone(),
                rel: (*rel).into(),
            });
        }
        // Then every edit a dead transport left unanswered, as itself: the
        // server applies each id once, so one that did land before the link
        // died is answered without being applied again. After the reads, so
        // the ordered worker answers them first — and a read of a store with
        // edits still waiting is not adopted (`remote_edits_pending`).
        self.send_remote_edits();
    }

    /// Take one remote `.clew/<rel>` file's text as this window's copy of that
    /// store — from a read (`StateContent`) or from the merged file a
    /// `StateEdited` carries back.
    ///
    /// The two mergeable list stores are re-sorted here. A remote merge is
    /// applied by the server, which is told the fields that IDENTIFY an entry
    /// and not the ones the store displays by, so it appends where the local
    /// path would have inserted in order.
    ///
    /// Any view state that ADDRESSES one of these stores by position or id has
    /// to be rebased when the store is replaced. The trail's `trail_collapsed`
    /// does (node ids of the history being replaced, which would otherwise
    /// collapse unrelated nodes of the adopted trail — cleared). The
    /// walkthrough library's open tour is held by scope, so only a tour that
    /// is gone from the adopted file closes. Bookmarks and notes need
    /// nothing: their rows' messages carry identities — `(rel, line)`,
    /// `(rel, symbol)` — resolved against the list when the click lands, and
    /// the only selection either keeps across a round trip (`note_edit`) is
    /// keyed by `(rel, line)`.
    pub(crate) fn adopt_remote_state(&mut self, root: &Path, rel: &str, text: &str) {
        match rel {
            "history.json" => {
                self.proj.history = history::from_text(root, text);
                self.proj.trail_collapsed.clear();
            }
            _ if rel == bookmarks::REL => {
                let mut list = bookmarks::from_text(text);
                bookmarks::sort(&mut list);
                self.proj.bookmarks = list;
            }
            _ if rel == notes::REL => {
                let mut list = notes::from_text(text);
                notes::sort(&mut list);
                self.proj.notes = list;
            }
            "reading.toml" => {
                if let Some(target) = reading::target_from_text(text) {
                    self.proj.reading_target = target;
                    // Re-evaluate the cfg dimming for anything open.
                    let t = self.proj.reading_target.clone();
                    for v in self.proj.panes.iter_mut().flatten() {
                        if let Some(lang) = v.lang_key {
                            let src = v.source.clone();
                            v.set_inactive_lines(inactive::inactive_lines(&src, lang, &t));
                        }
                    }
                }
            }
            _ if rel == walkthrough::LIBRARY_REL => {
                if let Some(library) = walkthrough::from_text(text) {
                    // The open tour is held by SCOPE (`WalkState::open`): the
                    // list arriving here is the FILE's, in the file's order,
                    // and a tour another client appended before this window's
                    // — or removed — would have moved an index, switching the
                    // WALK pane to somebody else's tour while next/prev
                    // navigated the editor into its files.
                    self.proj.walk.library = library;
                    // The tour is gone from the file: it closes, and its
                    // narration goes too, the same way the local delete path
                    // does, instead of prose on screen for a tour nothing
                    // selects.
                    self.proj.walk.forget_vanished_tour();
                    //
                    // NOT closed: when the scope survives, `prepared` is left
                    // as it is. It still holds the narration of the version
                    // this window rendered, so a tour another client
                    // REGENERATED under the same scope shows its old prose
                    // until the user steps or reopens it. Re-preparing here
                    // means `walkthrough_goto`, which opens files and moves
                    // the editor — surprise navigation from a background state
                    // adoption is the worse failure.
                }
            }
            // An unknown rel: nothing here holds it.
            _ => {}
        }
    }

    /// Re-send the state file `rel` from what this client now holds. Used when
    /// the user changed it while its load was still outstanding, or while
    /// there was no transport: the load's arrival keeps the user's version,
    /// and this is what persists it.
    ///
    /// Whole-snapshot, so only for the stores ONE client owns outright —
    /// `history.json` (deliberately last-writer-wins) and `reading.toml`. The
    /// mergeable stores never come here: a change to one waits in
    /// `ProjectSession::remote_edits` and is sent again, as itself
    /// ([`Self::send_remote_edits`]); writing this window's copy over the file
    /// would replace whatever another client saved since.
    pub(crate) fn flush_remote_state(&mut self, rel: &str) {
        let text = match rel {
            "history.json" => self
                .proj
                .project
                .as_ref()
                .and_then(|p| history::to_text(&p.root, &self.proj.history)),
            "reading.toml" => reading::target_to_text(&self.proj.reading_target),
            _ => return,
        };
        self.write_remote_state(rel, text);
    }

    /// Apply ONE entry-level change to a REMOTE project's `.clew/<rel>` at the
    /// server, and adopt the merged file it replies with.
    ///
    /// This is the remote half of the same rule the local stores follow: apply
    /// the change to the CONTENT THAT IS AUTHORITATIVE RIGHT NOW, never to a
    /// window's copy of it. Locally that is a read-modify-write under
    /// `bookmarks::edit` / `notes::edit` / `walkthrough::edit_library`; here no
    /// client can hold that lock — two windows on one remote project each open
    /// their own SSH session and their own remote clew-server — so the change
    /// travels as data and the server performs the read-modify-write. The
    /// merged file comes back as `StateEdited` and replaces this window's copy,
    /// exactly as the local callers adopt the merged list.
    ///
    /// The change is journaled first (`ProjectSession::remote_edits`) under an
    /// id of its own, and leaves the journal only when the server answers it.
    /// A transport that dies first takes nothing with it: the next one sends
    /// the change again, under the same id, and the server — which records the
    /// ids it applied — applies it once. (A queued frame proves nothing: a
    /// transport that has died without being detected accepts frames into a
    /// pipe that goes nowhere.)
    ///
    /// Deliberately NOT gated on `remote_state_pending`, which
    /// [`Self::write_remote_state`] must be: that gate exists because a
    /// wholesale write from a client that has not loaded the file yet pushes
    /// its empty baseline over the remote's content. A merge carries no
    /// baseline — it names one entry and what to do with it — so it is safe
    /// the moment the user makes it, even before the initial read lands.
    ///
    /// Returns whether the change was taken. It is not while the store has
    /// [`EDIT_JOURNAL_CAP`] edits waiting for the server (the link is down,
    /// or the server slow): the status line then says so, and the caller
    /// leaves this window's copy as it is — the change was not made.
    #[must_use = "a refused change must not be applied to this window's copy"]
    pub(crate) fn edit_remote_state(
        &mut self,
        rel: &str,
        merge: clew_protocol::StateMerge,
    ) -> bool {
        if self.proj.project.is_none() {
            return false;
        }
        let waiting = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.rel == rel)
            .count();
        if waiting >= EDIT_JOURNAL_CAP {
            self.status = format!(
                "Not changed: {waiting} earlier changes to .clew/{rel} are still waiting for \
                 the server to confirm them — try again once they are saved"
            );
            return false;
        }
        let id = self.proj.remote_edit_ids.mint();
        self.proj.remote_edits.push(RemoteEdit {
            rel: rel.to_string(),
            id,
            merge,
            request: None,
            failures: 0,
            retry_at: None,
        });
        // Unsaved until the server answers (what leaving the project warns
        // about, and what keeps a re-read from replacing this window's copy).
        self.proj.remote_state_dirty.insert(rel.to_string());
        self.send_remote_edits();
        true
    }

    /// Send the journaled edits whose turn it is, in the order they were
    /// made — the store's order of truth, which the server's ordered state
    /// worker keeps.
    ///
    /// ONE edit of a store is on the wire at a time: the next goes once the
    /// server has answered it ([`Self::settle_remote_edit`]). Sent all at
    /// once, an edit the server could not apply this time (`Failed`) was
    /// tried again after the ones behind it had landed, and landed on top of
    /// them: two quick saves of a note ended as the first, with no error; a
    /// removal after an upsert brought the note back. So an edit waits
    /// behind its store's edit on the wire, and behind one held back after a
    /// transient failure ([`RemoteEdit::retry_at`]), which the tick sends
    /// again once due. Stores do not wait for each other: they are separate
    /// files.
    ///
    /// Stops at the first edit that cannot go at all — no transport, or this
    /// client's request queue full, which holds it back like a failure.
    /// Nothing goes before the transport's handshake, which is where the
    /// project is opened on it (`ServerLink::ready`); the handshake sends
    /// the edits itself (`request_remote_state`).
    pub(crate) fn send_remote_edits(&mut self) {
        self.send_remote_edits_due(std::time::Instant::now(), false);
    }

    /// [`Self::send_remote_edits`], counting as due every edit held back
    /// until `now` or earlier — and, for a window that is `closing`, sending
    /// every edit it can without waiting for any answer, so that the close
    /// waits for one round trip at most.
    ///
    /// The one-at-a-time rule is there so that no edit lands after the ones
    /// made after it. A close's sending breaks it, so one more rule holds
    /// either way: no edit goes while an edit of its store made after it is
    /// on the wire — sent now, it would land after that one. One the server
    /// failed waits for those answers ([`Self::retry_remote_edit`]).
    fn send_remote_edits_due(&mut self, now: std::time::Instant, closing: bool) {
        if !self.server.ready {
            return;
        }
        let Some(root) = self
            .proj
            .project
            .as_ref()
            .map(|p| p.root.to_string_lossy().into_owned())
        else {
            return;
        };
        // Where each store's last edit on the wire is.
        let mut last_sent: HashMap<String, usize> = HashMap::new();
        for (i, edit) in self.proj.remote_edits.iter().enumerate() {
            if edit.request.is_some() {
                last_sent.insert(edit.rel.clone(), i);
            }
        }
        // The stores whose next edit waits: one is on the wire, or held back.
        let mut waiting: HashSet<String> = HashSet::new();
        for i in 0..self.proj.remote_edits.len() {
            let edit = &self.proj.remote_edits[i];
            if waiting.contains(&edit.rel) {
                continue;
            }
            let overtaken = last_sent.get(&edit.rel).is_some_and(|last| *last > i);
            if edit.request.is_some() || edit.retry_at.is_some_and(|at| at > now) || overtaken {
                if !closing {
                    waiting.insert(edit.rel.clone());
                }
                continue;
            }
            let request = clew_protocol::Request::EditState {
                root: root.clone(),
                rel: edit.rel.clone(),
                merge: edit.merge.clone(),
                edit_id: edit.id.clone(),
            };
            let Some(sent) = self.send_to_server(request) else {
                // No transport, or its queue is full. The first leaves the
                // edit to the handshake of the next one; the second would
                // leave it to the next edit, which may never come — so it is
                // held back and the tick sends it again.
                if self.server.is_up() {
                    self.proj.remote_edits[i].retry_at = Some(now + EDIT_RETRY_DELAY);
                }
                return;
            };
            let edit = &mut self.proj.remote_edits[i];
            edit.request = Some(sent);
            edit.retry_at = None;
            if !closing {
                waiting.insert(edit.rel.clone());
            }
        }
    }

    /// Send everything the journal holds that the transport can take, held
    /// back or not, and without waiting for answers: what a window sends
    /// once it may close (`crate::shell`).
    ///
    /// From here on the window may go at any moment — or be kept open with
    /// edits sent out of their turn — so an edit given up from now on is
    /// kept to be named in the question its close or a quit asks
    /// ([`Self::unsaved_edits`]), rather than left to a status line nobody
    /// may read before the window is gone.
    pub(crate) fn flush_remote_edits(&mut self) {
        self.proj.remote_edits_lost.get_or_insert_with(Vec::new);
        let every_hold = EDIT_RETRY_DELAY * EDIT_ATTEMPTS;
        self.send_remote_edits_due(std::time::Instant::now() + every_hold, true);
    }

    /// For a window whose close, or a quit, waits for the host: send the
    /// edits that are due without waiting for answers — one made while it
    /// waits goes at once, as the flush sent the rest, not behind one on the
    /// wire. One the host failed stays held back: it is asked about, not
    /// tried again in a burst while the grace runs.
    pub(crate) fn send_closing_edits(&mut self) {
        self.send_remote_edits_due(std::time::Instant::now(), true);
    }

    /// Everything closing the window now would leave unsaved: every edit
    /// the journal holds — not sent, sent and not confirmed, or failed by
    /// the host and held back to be tried again — and the ones given up
    /// since it began to close that no question has named yet
    /// ([`Self::forget_lost_edits`]). `None` when there is nothing.
    ///
    /// Sent is not saved: a transport that died without anyone noticing
    /// takes frames into a pipe that goes nowhere, and SSH notices a dead
    /// link only after about 45 s. So a close waits for what it sent to be
    /// answered ([`Self::awaits_host`]), and then asks about all of this at
    /// once (`crate::shell`).
    pub(crate) fn unsaved_edits(&self) -> Option<UnsavedEdits> {
        let lost = self.proj.remote_edits_lost.as_deref().unwrap_or_default();
        self.unsaved(self.proj.remote_edits.iter(), lost)
    }

    /// The journaled edits no transport has taken — after a flush, the ones
    /// it could not send, and the ones the host failed and that wait to be
    /// sent again; `None` when there are none.
    pub(crate) fn unsent_edits(&self) -> Option<UnsavedEdits> {
        let unsent = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.request.is_none());
        self.unsaved(unsent, &[])
    }

    /// The journaled edits sent to the host and not answered yet; `None`
    /// when there are none.
    pub(crate) fn unconfirmed_edits(&self) -> Option<UnsavedEdits> {
        let sent = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.request.is_some());
        self.unsaved(sent, &[])
    }

    /// Whether an edit is on the wire — sent to the host and not answered —
    /// other than the ones in `agreed`, which the user agreed to lose: what
    /// a close, or a quit, waits for. One that could not be sent, or that
    /// the host failed, is not waited for: no answer is coming for it.
    pub(crate) fn awaits_host(&self, agreed: &HashSet<String>) -> bool {
        self.proj
            .remote_edits
            .iter()
            .any(|e| e.request.is_some() && !agreed.contains(&e.id))
    }

    /// The user was told of the edits given up that `told` names — a
    /// question named them, and was answered: no later one names them again.
    pub(crate) fn forget_lost_edits(&mut self, told: &HashSet<String>) {
        if let Some(lost) = &mut self.proj.remote_edits_lost {
            lost.retain(|e| !told.contains(&e.id));
        }
    }

    /// For a window about to close: send what the journal holds
    /// ([`Self::flush_remote_edits`]), and take out the edits that still
    /// could not go — no transport, or a full queue — which are lost with the
    /// window. `None` when every edit went. Taken out, so they are not lost
    /// twice.
    pub(crate) fn take_unsendable_edits(&mut self) -> Option<UnsavedEdits> {
        self.flush_remote_edits();
        let lost = self.unsent_edits();
        self.proj.remote_edits.retain(|e| e.request.is_some());
        lost
    }

    /// `edits`, from the journal, and `lost`, given up, as the user is told
    /// about them: how many, why, and which; `None` when there are none.
    fn unsaved<'a>(
        &self,
        edits: impl Iterator<Item = &'a RemoteEdit>,
        lost: &[RemoteEdit],
    ) -> Option<UnsavedEdits> {
        let edits: Vec<&RemoteEdit> = edits.collect();
        if edits.is_empty() && lost.is_empty() {
            return None;
        }
        let mut stores: Vec<&'static str> = edits.iter().map(|e| store_name(&e.rel)).collect();
        stores.sort_unstable();
        stores.dedup();
        let unconfirmed = edits.iter().filter(|e| e.request.is_some()).count();
        // `failures` counts the host's failures alone: a full queue holds an
        // edit back without one.
        let failed = edits
            .iter()
            .filter(|e| e.request.is_none() && e.failures > 0)
            .count();
        Some(UnsavedEdits {
            count: edits.len() + lost.len(),
            ids: edits
                .iter()
                .copied()
                .chain(lost)
                .map(|e| e.id.clone())
                .collect(),
            project: self.project_label(),
            stores,
            unsent: edits.len() - unconfirmed - failed,
            unconfirmed,
            failed,
            lost: lost.iter().map(entry_name).collect(),
        })
    }

    /// The status line while a close — or a quit, when `quitting` — waits
    /// for the host to confirm what it sent (`crate::shell`): the window is
    /// still open and working, and says why it has not gone.
    pub(crate) fn show_waiting(&mut self, quitting: bool) {
        let sent = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.request.is_some())
            .count();
        let waits = if quitting {
            QUITTING_WAITS
        } else {
            CLOSING_WAITS
        };
        self.status = format!("{waits}{}…", changes(sent));
    }

    /// The wait goes on, and the host has answered some of it: the status
    /// line counts what is left — unless it says something else by now, a
    /// failure the host reported, which it goes on saying.
    pub(crate) fn update_waiting(&mut self, quitting: bool) {
        if self.shows_waiting() {
            self.show_waiting(quitting);
        }
    }

    /// The wait for the host is over: the status line stops saying it
    /// waits, unless it says something else by now.
    pub(crate) fn end_waiting(&mut self) {
        if self.shows_waiting() {
            self.status.clear();
        }
    }

    /// Whether the status line says a close, or a quit, waits for the host.
    fn shows_waiting(&self) -> bool {
        self.status.starts_with(CLOSING_WAITS) || self.status.starts_with(QUITTING_WAITS)
    }

    /// The open project as the user knows it: its path, after its host's
    /// `user@host` when it is remote.
    fn project_label(&self) -> String {
        let root = self
            .proj
            .project
            .as_ref()
            .map(|p| p.root.display().to_string())
            .unwrap_or_default();
        match &self.connection {
            crate::backend::connect::ConnTarget::Ssh { label, .. } => format!("{label}:{root}"),
            crate::backend::connect::ConnTarget::Local => root,
        }
    }

    /// Journaled edits wait to be sent again (see [`RemoteEdit::retry_at`]):
    /// what keeps the tick running. (An edit waiting behind its store's edit
    /// on the wire needs no tick: that edit's answer sends it.)
    pub(crate) fn remote_edits_waiting(&self) -> bool {
        self.server.ready
            && self
                .proj
                .remote_edits
                .iter()
                .any(|e| e.request.is_none() && e.retry_at.is_some())
    }

    /// The server could not apply journaled edit request `request` this time
    /// (it answered `Failed`: an I/O error, a lock, a panic). The edit stays
    /// in the journal and goes again after a pause, in its place — unless
    /// that was its [`EDIT_ATTEMPTS`]th try, when it is given up
    /// ([`Self::give_up_remote_edit`]). `None` when `request` carried no
    /// journaled edit.
    ///
    /// Edits of its store made after it may be on the wire too — a close
    /// sent them without waiting ([`Self::flush_remote_edits`]) — and the
    /// server may have applied them already: sent again now, it would land
    /// on top of them, the older text of a note over the newer, a removal
    /// undone by the add it followed. So it is not sent again while they are
    /// on the wire ([`Self::send_remote_edits_due`]), and their answers
    /// decide: when one that changes the same entry lands, it has taken this
    /// one's place, which is given up ([`Self::settle_remote_edit`]); when
    /// they fail too, they are all tried again, in order. Changes to other
    /// entries decide nothing: they land the same in either order.
    pub(crate) fn retry_remote_edit(&mut self, request: u64) -> Option<Unapplied> {
        let at = self
            .proj
            .remote_edits
            .iter()
            .position(|e| e.request == Some(request))?;
        let edit = &mut self.proj.remote_edits[at];
        edit.failures += 1;
        if edit.failures >= EDIT_ATTEMPTS {
            return self.give_up_remote_edit(request);
        }
        edit.request = None;
        edit.retry_at = Some(std::time::Instant::now() + EDIT_RETRY_DELAY * edit.failures);
        Some(Unapplied {
            rel: edit.rel.clone(),
            what: entry_name(edit),
            given_up: false,
            last: false,
        })
    }

    /// The server will not apply journaled edit request `request`: it
    /// refused it (a store it cannot understand), or it was out of tries.
    /// It leaves the journal, lost — kept to be named in a question once the
    /// window has begun to close ([`Self::flush_remote_edits`]) — and the
    /// next edit of its store goes out. `None` when `request` carried no
    /// journaled edit.
    pub(crate) fn give_up_remote_edit(&mut self, request: u64) -> Option<Unapplied> {
        let at = self
            .proj
            .remote_edits
            .iter()
            .position(|e| e.request == Some(request))?;
        let edit = self.proj.remote_edits.remove(at);
        let (rel, what) = (edit.rel.clone(), entry_name(&edit));
        self.keep_lost(edit);
        let last = self.settle_store(&rel);
        Some(Unapplied {
            rel,
            what,
            given_up: true,
            last,
        })
    }

    /// Whether a journaled edit of `rel` is still unanswered. A read of `rel`
    /// that arrives meanwhile is not adopted: it may predate the edits, whose
    /// replies bring the merged truth.
    pub(crate) fn remote_edits_pending(&self, rel: &str) -> bool {
        self.proj.remote_edits.iter().any(|e| e.rel == rel)
    }

    /// The server applied journaled edit request `request`: it leaves the
    /// journal, and the next edit of its store goes out (one at a time, see
    /// [`Self::send_remote_edits`]). Returns its store's rel — and whether it
    /// was the LAST edit of that store still waiting, whose answer is then
    /// the store's truth: an earlier one describes the file before the edits
    /// after it, and adopting it would roll this window back below its own
    /// changes. `None` when `request` carried no journaled edit.
    ///
    /// An edit of the same entry made before it, which the server failed
    /// and which waited for this answer ([`Self::retry_remote_edit`]), is
    /// given up: this one has taken its place, and sent again it would land
    /// on top of it. The status line says which.
    pub(crate) fn settle_remote_edit(&mut self, request: u64) -> Option<(String, bool)> {
        let at = self
            .proj
            .remote_edits
            .iter()
            .position(|e| e.request == Some(request))?;
        let landed = self.proj.remote_edits.remove(at);
        let taken_over: Vec<RemoteEdit> = self
            .proj
            .remote_edits
            .extract_if(..at, |e| e.request.is_none() && same_entry(e, &landed))
            .collect();
        if !taken_over.is_empty() {
            let names: Vec<String> = taken_over.iter().map(entry_name).collect();
            self.status = format!(
                "Could not save .clew/{}: {} {} lost — a later change to the same entry was \
                 saved first",
                landed.rel,
                lost_list(&names),
                if names.len() == 1 { "is" } else { "are" }
            );
            for edit in taken_over {
                self.keep_lost(edit);
            }
        }
        let last = self.settle_store(&landed.rel);
        Some((landed.rel, last))
    }

    /// An edit of `rel` left the journal: whether it was the last one, which
    /// leaves the store clean. The next edit of the store goes out.
    fn settle_store(&mut self, rel: &str) -> bool {
        let last = !self.remote_edits_pending(rel);
        if last {
            self.proj.remote_state_dirty.remove(rel);
        }
        self.send_remote_edits();
        last
    }

    /// `edit` was given up: kept to be named in a question, once the window
    /// has begun to close ([`Self::flush_remote_edits`]).
    fn keep_lost(&mut self, edit: RemoteEdit) {
        if let Some(lost) = &mut self.proj.remote_edits_lost {
            lost.push(edit);
        }
    }

    /// The transport died: every journaled edit it carried is unanswered for
    /// good on it, and goes out again on the next (under its same id).
    pub(crate) fn unsend_remote_edits(&mut self) {
        for edit in &mut self.proj.remote_edits {
            edit.request = None;
        }
    }

    /// Whether a wholesale write of `rel` is on its way to the server and has
    /// not been acknowledged. A `StateContent` that arrives while one is
    /// outstanding describes the file BEFORE that write: adopting it would
    /// revert what the user just did. (The mergeable stores' edits are
    /// tracked by the journal instead — [`Self::remote_edits_pending`].)
    pub(crate) fn remote_state_edit_inflight(&self, rel: &str) -> bool {
        self.proj
            .link
            .remote_state_inflight
            .values()
            .any(|r| r == rel)
    }

    /// Replace one `.clew/<rel>` of a REMOTE project wholesale (`None`
    /// deletes). Fire-and-forget: a failure comes back as an Error event and
    /// lands in the status bar.
    ///
    /// Correct only for a store this client alone owns the whole content of —
    /// `history.json` (deliberately last-writer-wins, see [`Self::save_history`])
    /// and `reading.toml` (a single scalar). For the stores two clients can
    /// both add entries to, use [`Self::edit_remote_state`]: a snapshot written
    /// from a copy loaded at project open deletes everything the other client
    /// has written since.
    pub(crate) fn write_remote_state(&mut self, rel: &str, text: Option<String>) {
        let Some(root) = self
            .proj
            .project
            .as_ref()
            .map(|p| p.root.to_string_lossy().into_owned())
        else {
            return;
        };
        // Dirty FIRST, and cleared only by the server's `StateWritten`. A
        // successful `send` proves nothing: it queues a frame, and a transport
        // that has died without being detected — a laptop changing networks,
        // the ordinary case — accepts frames into a pipe that goes nowhere.
        // Clearing the mark here lost every change made in that window, and
        // the reconnect's re-read then replaced them with the stale remote
        // copy (see `remote_state_inflight`).
        self.proj.remote_state_dirty.insert(rel.to_string());
        // Two reasons to hold a change back rather than send it, and both end
        // the same way: it stays dirty and `flush_remote_state` sends it once
        // the (re-)read lands.
        //
        // * the file's real content has not arrived yet, so writing now would
        //   push this client's empty baseline over it;
        // * there is no transport at all.
        if !self.server.is_up() || self.proj.remote_state_pending.contains(rel) {
            return;
        }
        // A transport that is gone (or not taking requests): stay dirty; the
        // reconnect flushes.
        let Some(id) = self.send_to_server(clew_protocol::Request::WriteState {
            root,
            rel: rel.into(),
            text,
        }) else {
            return;
        };
        // Supersede any earlier write of the same rel: its acknowledgement
        // must not clear a mark this newer one owns.
        self.proj
            .link
            .remote_state_inflight
            .retain(|_, pending| pending != rel);
        self.proj
            .link
            .remote_state_inflight
            .insert(id, rel.to_string());
        // These bytes are this window's whole copy, so if a change the server
        // never received is in that copy, this request is what carries it back.
        // Recorded separately from the id above because a later edit takes the
        // dirty mark's ownership away from this id while leaving that fact
        // true — and then only this record can retire the unsent mark.
        if self.proj.remote_state_unsent.contains(rel) {
            self.proj
                .link
                .remote_state_rescue
                .insert(id, rel.to_string());
        }
    }
}

/// How a mergeable store is named to the user.
fn store_name(rel: &str) -> &'static str {
    match rel {
        _ if rel == bookmarks::REL => "bookmarks",
        _ if rel == notes::REL => "notes",
        _ if rel == walkthrough::LIBRARY_REL => "walkthroughs",
        _ => "project state",
    }
}
