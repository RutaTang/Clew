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

/// The most edits given up that a closing window keeps by name for its
/// question (`ProjectSession::remote_edits_lost`); past these it keeps
/// their ids alone, which the user's agreement to lose them names, and
/// counts them. A question names a few and says how many more, and the
/// record must not keep a name — a tour's, cut to fit — for every edit a
/// host refuses while a close waits.
pub(crate) const LOST_KEPT: usize = 64;

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
/// save it, it waits for an earlier change it must land after — or the host
/// could not save it, and it was given up. What the shell asks the user
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
    /// How many of those were not sent for an earlier change they must land
    /// after — to the same entry, or to the same store where its readers go
    /// by the order of its file ([`keeps_order`]) — which reached the host
    /// and is not saved yet (on the wire, or failed there and to be tried
    /// again), or which waits its turn in turn ([`held_for_turn`]). Each
    /// goes once the one before it is saved ([`App::send_remote_edits_due`]).
    /// With a link up, none of them "could not be sent".
    pub(crate) waiting: usize,
    /// The ones the host could not save that were given up — out of tries,
    /// or refused — each as the user knows it ([`entry_name`]): lost,
    /// whatever the answer.
    pub(crate) lost: Vec<String>,
    /// How many more were given up than [`Self::lost`] names: past
    /// [`LOST_KEPT`], they are only counted.
    pub(crate) lost_more: usize,
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
    /// It waits, not sent, for the host to save an earlier change it must
    /// land after ([`must_follow`]) first.
    Waiting,
}

impl UnsavedEdits {
    /// How many are still in the journal: not saved yet, and not given up.
    fn pending(&self) -> usize {
        self.unsent + self.unconfirmed + self.failed + self.waiting
    }

    /// The reason every pending edit shares; `None` when they have several,
    /// or there are none.
    fn why(&self) -> Option<Why> {
        let mut reasons = [
            (self.unsent, Why::Unsent),
            (self.unconfirmed, Why::Unconfirmed),
            (self.failed, Why::Failed),
            (self.waiting, Why::Waiting),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0);
        match (reasons.next(), reasons.next()) {
            (Some((_, why)), None) => Some(why),
            _ => None,
        }
    }

    /// How many were given up: named, and only counted.
    pub(crate) fn given_up(&self) -> usize {
        self.lost.len() + self.lost_more
    }

    /// Whether every one of them was sent, and waits for the host's answer.
    fn all_unconfirmed(&self) -> bool {
        self.why() == Some(Why::Unconfirmed) && self.given_up() == 0
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
            Some(Why::Waiting) => format!(
                "{} not sent yet: {} for the host to save an earlier change first",
                is(n),
                if n == 1 { "it waits" } else { "each waits" }
            ),
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
        if self.waiting > 0 {
            let n = self.waiting;
            let waits = if n == 1 { "waits" } else { "wait" };
            parts.push(format!("{n} {waits} for an earlier change to be saved"));
        }
        and_list(&parts)
    }

    /// One line of a quit's list for this window's pending edits: "2
    /// changes to notes in me@host:/srv/app could not be sent" — or, when
    /// they are not all alike, how many for each reason.
    pub(crate) fn line(&self) -> String {
        let n = self.pending();
        match self.why() {
            Some(Why::Unsent) => format!("{} could not be sent", self.describe()),
            Some(Why::Unconfirmed) => {
                format!("{} {} sent but not confirmed", self.describe(), were(n))
            }
            Some(Why::Failed) => format!("{} could not be saved by the host", self.describe()),
            Some(Why::Waiting) => format!("{} {} not sent yet", self.describe(), is(n)),
            None => format!("{}: {}", self.describe(), self.breakdown()),
        }
    }

    /// The sentence naming the edits given up: "The host could not save
    /// the change to the bookmark at src/lib.rs:12: it is lost." — with
    /// the project when nothing else names it.
    pub(crate) fn lost_sentence(&self, with_project: bool) -> String {
        let n = self.given_up();
        let project = if with_project {
            format!(" in {}", self.project)
        } else {
            String::new()
        };
        format!(
            "The host could not save {}{project}: {} lost.",
            lost_list(&self.lost, self.lost_more),
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

/// Edits given up, by the entries they change (see [`entry_name`]), and
/// `uncounted` more given up past those names: "the change to the bookmark
/// at a.rs:1", "the changes to the note on f in lib.rs, the walkthrough
/// “parsing” and 2 more" — "3 changes" when none is named.
fn lost_list(names: &[String], uncounted: usize) -> String {
    if names.is_empty() {
        return changes(uncounted);
    }
    let (named, more) = names.split_at(names.len().min(LOST_NAMED));
    let more = more.len() + uncounted;
    let mut items = named.to_vec();
    if more > 0 {
        items.push(format!("{more} more"));
    }
    let changes = if names.len() + uncounted == 1 {
        "change"
    } else {
        "changes"
    };
    format!("the {changes} to {}", and_list(&items))
}

/// The entry journaled edit `edit` changes, as the user knows it: "the
/// bookmark at src/lib.rs:12", "the note on parse in src/lib.rs", "the
/// walkthrough “how parsing works”". Read from the fields that identify the
/// entry (`StateMerge::key`), or the store name for a TOML key.
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
        "reading.toml" => "the reading target".to_string(),
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
/// store. Changes to different entries land the same in either order, as
/// far as the store's readers go ([`keeps_order`]); changes to one entry
/// only when they commute ([`clew_core::statefile::commutes`]).
fn same_entry(a: &RemoteEdit, b: &RemoteEdit) -> bool {
    a.rel == b.rel && a.merge.key_fields == b.merge.key_fields && a.merge.key == b.merge.key
}

/// Whether the store `rel`'s readers go by the order of its file: the
/// walkthrough library is shown in that order, while bookmarks and notes are
/// sorted as they are read ([`App::adopt_remote_state`]). There, edits of
/// different entries do not land the same in either order — one that
/// appends a tour puts it before or after another — so its edits keep the
/// order they were made in, every one of them ([`must_follow`]).
fn keeps_order(rel: &str) -> bool {
    rel == walkthrough::LIBRARY_REL
}

/// Whether journaled edit `later` must land after `earlier`, made before it,
/// or the store would not end as they made it: they change the same entry
/// and do not commute ([`clew_core::statefile::commutes`]) — or any two
/// entries of a store that keeps its order ([`keeps_order`]).
fn must_follow(later: &RemoteEdit, earlier: &RemoteEdit) -> bool {
    later.rel == earlier.rel
        && (keeps_order(&later.rel)
            || (same_entry(earlier, later)
                && !clew_core::statefile::commutes(&earlier.merge, &later.merge)))
}

/// What a journaled edit held for its turn waits behind, directly or
/// through edits held in turn ([`held_for_turn`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Held {
    /// The edits on the wire, by their place in the journal, whose answers
    /// are on their way.
    wire: Vec<usize>,
    /// An edit the host failed, held back to be tried again: a close asks
    /// about it, rather than tries it again while it waits.
    failed: bool,
}

impl Held {
    /// Whether answers on their way let it go: it waits behind an edit on
    /// the wire, and behind none the host failed — whose try comes only
    /// after a close has stopped waiting.
    fn freed_by_answers(&self) -> bool {
        !self.wire.is_empty() && !self.failed
    }
}

/// For each journaled edit, in order: whether it is held for its turn — not
/// sent, for an earlier edit it must land after ([`must_follow`]) that
/// reached the host (on the wire, or failed there and held back to be tried
/// again) or that is held in turn — and, if it is, what it waits behind
/// ([`Held`]). A close's sending holds such an edit
/// ([`App::send_remote_edits_due`]), which a link that is up gives its turn:
/// it waits, where one that could go before every such edit and did not
/// could not be sent. Only one held right behind an edit that reached the
/// host used to be counted as waiting: one held behind it in turn was said
/// not to have been sent — with the link up.
fn held_for_turn(journal: &[RemoteEdit]) -> Vec<Option<Held>> {
    let mut held: Vec<Option<Held>> = Vec::with_capacity(journal.len());
    for (at, edit) in journal.iter().enumerate() {
        let mut turn: Option<Held> = None;
        if edit.request.is_none() && edit.failures == 0 {
            for (before, earlier) in journal[..at].iter().enumerate() {
                if !must_follow(edit, earlier) {
                    continue;
                }
                let behind = if earlier.request.is_some() {
                    Some(Held {
                        wire: vec![before],
                        failed: false,
                    })
                } else if earlier.failures > 0 {
                    Some(Held {
                        wire: Vec::new(),
                        failed: true,
                    })
                } else {
                    held[before].clone()
                };
                if let Some(behind) = behind {
                    let turn = turn.get_or_insert_with(Held::default);
                    for on_wire in behind.wire {
                        if !turn.wire.contains(&on_wire) {
                            turn.wire.push(on_wire);
                        }
                    }
                    turn.failed |= behind.failed;
                }
            }
        }
        held.push(turn);
    }
    held
}

/// Where the first journaled edit is that no host may have applied and
/// that the next edit of its entry supersedes
/// ([`clew_core::statefile::supersedes`]) — in a store that keeps its order
/// ([`keeps_order`]), only when that is the next edit of the store; see
/// [`App::drop_superseded`].
fn superseded_at(journal: &[RemoteEdit]) -> Option<usize> {
    journal.iter().enumerate().position(|(i, edit)| {
        !edit.may_have_landed
            && journal[i + 1..]
                .iter()
                .find(|later| {
                    if keeps_order(&edit.rel) {
                        later.rel == edit.rel
                    } else {
                        same_entry(edit, later)
                    }
                })
                .is_some_and(|next| {
                    same_entry(edit, next)
                        && clew_core::statefile::supersedes(&next.merge, &edit.merge)
                })
    })
}

/// The status line while `what`, an edit of `.clew/<rel>` the host could not
/// save this time (`message`), waits to be tried again.
pub(crate) fn retrying_status(rel: &str, what: &str, message: &str) -> String {
    format!("{NOT_SAVED}{rel} yet: the change to {what} ({message}) — retrying")
}

/// How the status line begins when it says an edit of the journal is not
/// saved: tried again ([`retrying_status`]), or lost (`App::edit_not_saved`).
pub(crate) const NOT_SAVED: &str = "Could not save .clew/";

/// The prefix of the status line while a close waits for the host.
const CLOSING_WAITS: &str = "Closing — waiting for the host to confirm ";

/// The prefix of the status line while a quit waits for the hosts.
const QUITTING_WAITS: &str = "Quitting — waiting for the host to confirm ";

/// A journaled edit the server answered without applying it
/// ([`App::retry_remote_edit`], [`App::give_up_remote_edit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unapplied {
    /// Its id (`RemoteEdit::id`).
    pub(crate) id: String,
    /// Its store, under `.clew/`.
    pub(crate) rel: String,
    /// The entry it changes, as the user knows it ([`entry_name`]).
    pub(crate) what: String,
    /// What became of it.
    pub(crate) fate: Fate,
    /// Given up, it was the last edit of its store in the journal: the
    /// store is read again, so the screen matches the disk.
    pub(crate) last: bool,
}

/// What became of a journaled edit the server did not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fate {
    /// It stays in the journal, to be tried again.
    Retried,
    /// It left the journal, and nothing is lost: a later change to its
    /// entry, still to be sent, supersedes it
    /// ([`clew_core::statefile::supersedes`]).
    Superseded,
    /// It left the journal, lost: refused, or out of tries.
    GivenUp,
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

/// "is" for one change, "are" for more.
fn is(count: usize) -> &'static str {
    if count == 1 { "is" } else { "are" }
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
        Some(Why::Waiting) => (
            if n == 1 {
                "it is sent once the change before it is saved".to_string()
            } else {
                "they are sent once the changes before them are saved".to_string()
            },
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
    if lost.given_up() > 0 {
        message.push(lost.lost_sentence(lost.pending() == 0));
        if lost.pending() == 0 {
            message.push(format!(
                "Keep the window open to make {} again, or close it.",
                them(lost.given_up())
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
        if one.given_up() > 0 {
            message.push(one.lost_sentence(one.pending() == 0));
            if one.pending() == 0 {
                message.push(format!(
                    "Cancel to keep clew open and make {} again, or quit.",
                    them(one.given_up())
                ));
            }
        }
        return (title, message.join(" "));
    }
    let title = question_title(total, confirming, "their hosts");
    let given_up = lost.iter().any(|l| l.given_up() > 0);
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
    lines.extend(lost.iter().filter(|l| l.given_up() > 0).map(|l| {
        let n = l.given_up();
        format!(
            "• {} in {}: the host could not save {}, and {} lost",
            lost_list(&l.lost, l.lost_more),
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
        // entry or TOML key and carries no baseline, so it is safe to send straight away
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
                if let Ok(target) = reading::try_target_from_text(text) {
                    self.proj.reading_target = target.unwrap_or_else(inactive::Target::host);
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

    /// Adopt an authoritative state read or merge, including absence. This
    /// is called only after the dirty/journal guards decided it may replace
    /// the window's copy. An absent file means the store's empty/default
    /// value, even when this window loaded an older version before reconnect.
    pub(crate) fn adopt_remote_state_content(
        &mut self,
        root: &Path,
        rel: &str,
        text: Option<&str>,
    ) {
        let empty = if rel == "reading.toml" { "" } else { "[]" };
        self.adopt_remote_state(root, rel, text.unwrap_or(empty));
    }

    /// Re-send the state file `rel` from what this client now holds. Used when
    /// the user changed it while its load was still outstanding, or while
    /// there was no transport: the load's arrival keeps the user's version,
    /// and this is what persists it.
    ///
    /// Whole-snapshot, so only for the stores ONE client owns outright —
    /// `history.json` (deliberately last-writer-wins). The
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
            _ => return,
        };
        self.write_remote_state(rel, text);
    }

    /// Apply ONE entry or TOML-key change to a REMOTE project's `.clew/<rel>`
    /// at the server, and adopt the merged file it replies with.
    ///
    /// This is the remote half of the same rule the local stores follow: apply
    /// the change to the CONTENT THAT IS AUTHORITATIVE RIGHT NOW, never to a
    /// window's copy of it. Locally that is a read-modify-write under
    /// `bookmarks::edit` / `notes::edit` / `walkthrough::edit_library` /
    /// `reading::save_target`; here no
    /// client can hold that lock — two windows on one remote project each open
    /// their own SSH session and their own remote clew-server — so the change
    /// travels as data and the server performs the read-modify-write. The
    /// merged file comes back as `StateEdited` and replaces this window's copy,
    /// exactly as the local callers adopt the merged state.
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
    /// baseline — it names one entry or key and what to do with it — so it is safe
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
            may_have_landed: false,
            adrift: false,
            failures: 0,
            retry_at: None,
        });
        // The edit of the same entry before it may be one it takes the
        // place of: a note saved again before the first save was sent.
        self.drop_superseded();
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
    /// what it can without waiting for answers, so that the close waits for
    /// as few round trips as it can.
    ///
    /// The one-at-a-time rule is there so that no edit lands after the ones
    /// made after it. A close's sending keeps it where it matters
    /// ([`must_follow`]): an edit goes while an earlier edit of its entry is
    /// still in the journal — on the wire, held back after a failure, or
    /// waiting — only when the two land the same in either order
    /// ([`clew_core::statefile::commutes`]): a note's text and its flag, a
    /// bookmark toggled twice. Any other pair a failure, or a link that
    /// died, could land the wrong way round: the older text of a note over
    /// the newer, a toggle after the note that was set on what it added.
    /// Edits of other entries go at once — they land the same in either
    /// order — but not in a store whose readers go by the order of its file
    /// ([`keeps_order`]), where a tour appended after another would stand
    /// before it, and where no edit goes past another. An edit the host
    /// failed goes too, once it is due: an edit sent past it did not have
    /// to follow it.
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
        // The stores whose next edit waits: one is on the wire, or held back.
        let mut waiting: HashSet<String> = HashSet::new();
        for i in 0..self.proj.remote_edits.len() {
            let journal = &self.proj.remote_edits;
            let edit = &journal[i];
            if waiting.contains(&edit.rel) {
                continue;
            }
            // Sent now, it could land before an earlier edit it must land
            // after.
            let out_of_turn = closing
                && journal[..i]
                    .iter()
                    .any(|earlier| must_follow(edit, earlier));
            if edit.request.is_some() || edit.retry_at.is_some_and(|at| at > now) || out_of_turn {
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
            let Some(sent) = self.offer_to_server(request) else {
                // No transport, or its queue is full. The first leaves the
                // edit to the handshake of the next one; the second would
                // leave it to the next edit, which may never come — so it is
                // held back, for the tick to send it again. A full queue is
                // this client's own, and drains in moments: the edit is due
                // again at once, and goes with the next tick, or answer, that
                // finds room — within a close's short grace too, when the
                // close waits for an edit on the wire. It was held for a
                // failure's pause, and from the clock a flush sets ahead: it
                // went again only after the grace. (A close with nothing on
                // the wire does not wait: it asks at once, the edit counted
                // as one that could not be sent.)
                if self.server.is_up() {
                    self.proj.remote_edits[i].retry_at = Some(std::time::Instant::now());
                }
                return;
            };
            let edit = &mut self.proj.remote_edits[i];
            edit.request = Some(sent);
            edit.may_have_landed = true;
            edit.retry_at = None;
            if !closing {
                waiting.insert(edit.rel.clone());
            }
        }
    }

    /// Send what the journal holds that the transport can take, held back
    /// or not, and without waiting for answers — but for an edit that must
    /// land after an earlier one still unanswered ([`must_follow`]), which
    /// waits for that one ([`Self::send_remote_edits_due`]): what a window
    /// sends once it may close (`crate::shell`).
    ///
    /// From here on the window may go at any moment — or be kept open with
    /// edits sent out of their turn — so an edit given up from now on is
    /// kept to be named in the question its close or a quit asks
    /// ([`Self::unsaved_edits`]), rather than left to a status line nobody
    /// may read before the window is gone.
    pub(crate) fn flush_remote_edits(&mut self) {
        self.proj
            .remote_edits_lost
            .get_or_insert_with(LostEdits::default);
        let every_hold = EDIT_RETRY_DELAY * EDIT_ATTEMPTS;
        self.send_remote_edits_due(std::time::Instant::now() + every_hold, true);
    }

    /// For a window whose close, or a quit, waits for the host: send the
    /// edits that are due without waiting for answers — one made while it
    /// waits goes at once, as the flush sent the rest, unless it must land
    /// after an earlier edit still unanswered ([`must_follow`]). One the
    /// host failed stays held back: it is asked about, not tried again in a
    /// burst while the grace runs.
    pub(crate) fn send_closing_edits(&mut self) {
        self.send_remote_edits_due(std::time::Instant::now(), true);
    }

    /// Everything closing the window now would leave unsaved: every edit
    /// the journal holds — not sent, sent and not confirmed, or failed by
    /// the host and held back to be tried again — and the ones given up
    /// since it began to close ([`Self::stop_keeping_lost_edits`]). `None`
    /// when there is nothing.
    ///
    /// Sent is not saved: a transport that died without anyone noticing
    /// takes frames into a pipe that goes nowhere, and SSH notices a dead
    /// link only after about 45 s. So a close waits for what it sent to be
    /// answered ([`Self::awaits_host`]), and then asks about all of this at
    /// once (`crate::shell`).
    pub(crate) fn unsaved_edits(&self) -> Option<UnsavedEdits> {
        let none = LostEdits::default();
        let lost = self.proj.remote_edits_lost.as_ref().unwrap_or(&none);
        self.unsaved(self.proj.remote_edits.iter(), lost)
    }

    /// The journaled edits no transport has taken — after a flush, the ones
    /// it could not send, the ones that wait for an earlier edit they must
    /// land after, and the ones the host failed and that wait to be sent
    /// again; `None` when there are none.
    pub(crate) fn unsent_edits(&self) -> Option<UnsavedEdits> {
        let unsent = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.request.is_none());
        self.unsaved(unsent, &LostEdits::default())
    }

    /// The journaled edits sent to the host and not answered yet; `None`
    /// when there are none.
    pub(crate) fn unconfirmed_edits(&self) -> Option<UnsavedEdits> {
        let sent = self
            .proj
            .remote_edits
            .iter()
            .filter(|e| e.request.is_some());
        self.unsaved(sent, &LostEdits::default())
    }

    /// Whether an edit is on the wire — sent to the host and not answered —
    /// other than the ones in `agreed`, which the user agreed to lose: what
    /// a close, or a quit, waits for. An agreed one is waited for too while
    /// an edit the user did not agree to lose is held for its turn behind
    /// it ([`held_for_turn`]) — right behind it, or behind edits held in
    /// turn — and behind none the host failed: its answer then lets that
    /// one go ([`Held::freed_by_answers`]). Not waited for, the close asked
    /// at once about a tour made while its question was up, as one that
    /// waits for an earlier change — while the answer that would send it was
    /// on its way. One that could not be sent, that the host failed, or that
    /// is held behind an edit the host failed, is not waited for: no answer
    /// coming lets it go.
    pub(crate) fn awaits_host(&self, agreed: &HashSet<String>) -> bool {
        !self.awaited(agreed).is_empty()
    }

    /// The edits on the wire, by their place in the journal, whose answers
    /// a close, or a quit, losing `agreed` waits for ([`Self::awaits_host`]):
    /// the ones the user did not agree to lose, and the agreed ones that an
    /// edit they did not agree to lose waits behind, freed by answers.
    fn awaited(&self, agreed: &HashSet<String>) -> std::collections::BTreeSet<usize> {
        let journal = &self.proj.remote_edits;
        let mut awaited = std::collections::BTreeSet::new();
        for (at, (edit, held)) in journal.iter().zip(held_for_turn(journal)).enumerate() {
            if agreed.contains(&edit.id) {
                continue;
            }
            if edit.request.is_some() {
                awaited.insert(at);
            }
            if let Some(held) = held.filter(Held::freed_by_answers) {
                awaited.extend(held.wire);
            }
        }
        awaited
    }

    /// The window is in use again: the user kept it open, or cancelled the
    /// quit. What was given up is not kept for a question any more — the
    /// one answered named what had been, and the status line says what is
    /// given up from now on, in a window the user is in — until the window
    /// begins to close again ([`Self::flush_remote_edits`]). Kept, the
    /// record grew with every edit given up for as long as the window
    /// stayed open, and the next close named again what the user had been
    /// told of.
    pub(crate) fn stop_keeping_lost_edits(&mut self) {
        self.proj.remote_edits_lost = None;
    }

    /// For a window about to close: send what the journal holds
    /// ([`Self::flush_remote_edits`]), and take out the edits that did not go
    /// — no transport, a full queue, an earlier edit they must land after
    /// not answered — which are lost with the window. `None` when every edit
    /// went. Taken out, so they are not lost twice.
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
        lost: &LostEdits,
    ) -> Option<UnsavedEdits> {
        let edits: Vec<&RemoteEdit> = edits.collect();
        if edits.is_empty() && lost.kept.is_empty() && lost.unnamed.is_empty() {
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
        let journal = &self.proj.remote_edits;
        let held: HashSet<&str> = journal
            .iter()
            .zip(held_for_turn(journal))
            .filter(|(_, held)| held.is_some())
            .map(|(edit, _)| edit.id.as_str())
            .collect();
        let waiting = edits
            .iter()
            .filter(|e| held.contains(e.id.as_str()))
            .count();
        Some(UnsavedEdits {
            count: edits.len() + lost.kept.len() + lost.unnamed.len(),
            ids: edits
                .iter()
                .map(|e| e.id.clone())
                .chain(lost.kept.iter().map(|(id, _)| id.clone()))
                .chain(lost.unnamed.iter().cloned())
                .collect(),
            project: self.project_label(),
            stores,
            unsent: edits.len() - unconfirmed - failed - waiting,
            unconfirmed,
            failed,
            waiting,
            lost: lost.kept.iter().map(|(_, what)| what.clone()).collect(),
            lost_more: lost.unnamed.len(),
        })
    }

    /// The status line while a close — or a quit, when `quitting` — waits
    /// for the host to confirm what it sent (`crate::shell`): the window is
    /// still open and working, and says why it has not gone — counting the
    /// answers it waits for, not an edit on the wire the user agreed to
    /// lose (`agreed`) that nothing waits behind.
    pub(crate) fn show_waiting(&mut self, quitting: bool, agreed: &HashSet<String>) {
        self.waiting_said = true;
        let sent = self.awaited(agreed).len();
        let waits = if quitting {
            QUITTING_WAITS
        } else {
            CLOSING_WAITS
        };
        self.status = format!("{waits}{}…", changes(sent));
    }

    /// The wait goes on, and the host has answered some of it: the status
    /// line counts what is left — unless it says something else by now, a
    /// failure the host reported, which it goes on saying. It says the wait
    /// once to a window that begins to wait only now — an edit made in it
    /// while a quit waits for another — but not over what it says of its
    /// edits not saved (a "— retrying", an "… is lost"), which is still so;
    /// and again once what it said since is taken back: silent, it gave no
    /// reason why clew had not gone.
    pub(crate) fn update_waiting(&mut self, quitting: bool, agreed: &HashSet<String>) {
        let first = !self.waiting_said && !self.status.starts_with(NOT_SAVED);
        if self.shows_waiting() || first || self.status.is_empty() {
            self.show_waiting(quitting, agreed);
        }
    }

    /// The wait for the host is over: the status line stops saying it
    /// waits, unless it says something else by now.
    pub(crate) fn end_waiting(&mut self) {
        self.waiting_said = false;
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
    /// ([`Self::give_up_remote_edit`]), or a later change to its entry
    /// supersedes it, when it leaves the journal and nothing is lost
    /// ([`Self::drop_superseded`]): tried again, the older text of a note
    /// would have landed over the newer; given up, it was named as lost
    /// when the entry held what the user meant. `None` when `request`
    /// carried no journaled edit.
    ///
    /// Edits of its store made after it may be on the wire too — a close
    /// sent them without waiting ([`Self::flush_remote_edits`]) — and the
    /// server may have applied them already. Each changes another entry, or
    /// commutes with this one ([`Self::send_remote_edits_due`]), so this one
    /// lands the same after them as before them, as far as the store's
    /// readers go: a store that keeps its order sends none past it
    /// ([`keeps_order`]).
    pub(crate) fn retry_remote_edit(&mut self, request: u64) -> Option<Unapplied> {
        let at = self
            .proj
            .remote_edits
            .iter()
            .position(|e| e.request == Some(request))?;
        let edit = &mut self.proj.remote_edits[at];
        edit.failures += 1;
        // Answered: this host did not apply it — but one that took it over a
        // transport that died may still.
        edit.may_have_landed = edit.adrift;
        if edit.failures < EDIT_ATTEMPTS {
            edit.request = None;
            edit.retry_at = Some(std::time::Instant::now() + EDIT_RETRY_DELAY * edit.failures);
        }
        let (id, rel, what) = (edit.id.clone(), edit.rel.clone(), entry_name(edit));
        if self.drop_superseded() && !self.proj.remote_edits.iter().any(|e| e.id == id) {
            let last = self.settle_store(&rel);
            return Some(Unapplied {
                id,
                rel,
                what,
                fate: Fate::Superseded,
                last,
            });
        }
        if self
            .proj
            .remote_edits
            .iter()
            .any(|e| e.request == Some(request))
        {
            return self.give_up_remote_edit(request);
        }
        Some(Unapplied {
            id,
            rel,
            what,
            fate: Fate::Retried,
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
        self.keep_lost(edit.id.clone(), what.clone());
        let last = self.settle_store(&rel);
        Some(Unapplied {
            id: edit.id,
            rel,
            what,
            fate: Fate::GivenUp,
            last,
        })
    }

    /// Leave out the journaled edits a later one takes the place of: an edit
    /// no host may have applied — not sent yet, or answered that it could
    /// not be — whose next edit of the same entry supersedes it
    /// ([`clew_core::statefile::supersedes`]) is redundant once that edit
    /// lands, as it will, or is given up and said to be lost in its turn.
    /// A note saved twice before the first save went sends only the second;
    /// one the host failed is not tried again over the newer text, nor
    /// named as lost. In a store that keeps its order ([`keeps_order`]), not
    /// past an edit of another entry: left out, a regenerated tour stood
    /// after one generated between the two. An edit that may have landed is
    /// never left out ([`RemoteEdit::may_have_landed`]): one on the wire may
    /// land yet, and one that was on the wire when its link died may land
    /// late, whatever the next host answers — left out, it would land after
    /// the edit that took its place, over it. Returns whether any was left
    /// out.
    fn drop_superseded(&mut self) -> bool {
        let mut dropped = false;
        while let Some(at) = superseded_at(&self.proj.remote_edits) {
            let edit = self.proj.remote_edits.remove(at);
            self.take_back_retrying(&edit);
            dropped = true;
        }
        dropped
    }

    /// Journaled edit `edit` left the journal — saved, or left out for a
    /// later change: the status line stops saying it is tried again
    /// ([`retrying_status`]) — unless it says something else by now. While
    /// another edit of the same entry is still tried again, the same words
    /// speak for that one, and go when it leaves in turn: still kept for
    /// the edit that had left, they stayed up once every edit of the entry
    /// was saved.
    fn take_back_retrying(&mut self, edit: &RemoteEdit) {
        let Some((id, said)) = self.proj.remote_edit_retrying.clone() else {
            return;
        };
        if id != edit.id || self.status != said {
            return;
        }
        let still_tried = self
            .proj
            .remote_edits
            .iter()
            .find(|e| same_entry(e, edit) && e.failures > 0)
            .map(|e| e.id.clone());
        match still_tried {
            Some(other) => self.proj.remote_edit_retrying = Some((other, said)),
            None => {
                self.proj.remote_edit_retrying = None;
                self.status.clear();
            }
        }
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
    /// An earlier edit of its entry still in the journal — one the server
    /// failed while this one was on the wire — commutes with it, as the
    /// close that sent them both made sure ([`Self::send_remote_edits_due`]):
    /// it is tried again in its turn, and lands the same after this one.
    pub(crate) fn settle_remote_edit(&mut self, request: u64) -> Option<(String, bool)> {
        let at = self
            .proj
            .remote_edits
            .iter()
            .position(|e| e.request == Some(request))?;
        let landed = self.proj.remote_edits.remove(at);
        self.take_back_retrying(&landed);
        let last = self.settle_store(&landed.rel);
        Some((landed.rel, last))
    }

    /// An edit of `rel` left the journal: whether it was the last one, which
    /// leaves the store clean. What it stood between may be redundant now
    /// ([`Self::drop_superseded`]); the next edit of the store goes out.
    fn settle_store(&mut self, rel: &str) -> bool {
        self.drop_superseded();
        let last = !self.remote_edits_pending(rel);
        if last {
            self.proj.remote_state_dirty.remove(rel);
        }
        self.send_remote_edits();
        last
    }

    /// The edit `id`, which changed `what` ([`entry_name`]), was given up:
    /// kept to be named in a question, once the window has begun to close
    /// ([`Self::flush_remote_edits`]) — past [`LOST_KEPT`] of them, by its
    /// id alone, and counted.
    fn keep_lost(&mut self, id: String, what: String) {
        if let Some(lost) = &mut self.proj.remote_edits_lost {
            if lost.kept.len() < LOST_KEPT {
                lost.kept.push((id, what));
            } else {
                lost.unnamed.push(id);
            }
        }
    }

    /// The transport died: every journaled edit it carried is unanswered for
    /// good on it, and goes out again on the next (under its same id) — and
    /// may yet land where it went ([`RemoteEdit::adrift`]).
    pub(crate) fn unsend_remote_edits(&mut self) {
        for edit in &mut self.proj.remote_edits {
            if edit.request.take().is_some() {
                edit.adrift = true;
            }
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
    /// `history.json` (deliberately last-writer-wins, see [`Self::save_history`]).
    /// For the stores two clients can edit, use [`Self::edit_remote_state`]: a snapshot written
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
        "reading.toml" => "reading preferences",
        _ => "project state",
    }
}
