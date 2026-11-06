//! Ask's sessions: each conversation with Clew about a project, kept so the
//! reader can start a new one and go back to an earlier one — with its
//! answers as the context of what is asked next.
//!
//! They are kept in clew's own data for the project (the derived-artifact
//! directory, `clew_core::derived::dir`), never in the repository: what the
//! reader asked about a codebase is theirs, not the codebase's. A remote
//! project's are kept on this machine, keyed by its host, as its other
//! derived files are.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use iced::Task;
use serde::{Deserialize, Serialize};

use crate::app::model::AskTurn;
use crate::{App, Message};

/// The file the sessions are kept in, in the project's derived directory.
const FILE: &str = "ask-sessions.json";
/// Its format: a file of another is read as none.
const VERSION: u32 = 1;
/// The most sessions kept: the oldest go past it.
pub(crate) const MAX_SESSIONS: usize = 50;
/// The longest a session's name grows, in characters.
const TITLE_CHARS: usize = 48;

/// One question of a session and its answer, as kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedTurn {
    pub question: String,
    /// Said rather than typed.
    #[serde(default)]
    pub spoken: bool,
    /// The answer, as the markdown it was written in.
    pub answer: String,
}

/// A conversation with Clew about the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AskSession {
    pub id: u64,
    /// Its name: its first question, cut short.
    pub title: String,
    /// When it was last asked in, in seconds since the epoch.
    pub updated: i64,
    pub turns: Vec<SavedTurn>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionsFile {
    version: u32,
    sessions: Vec<AskSession>,
}

/// A session's name for its first question: its first line, cut short.
pub(crate) fn title_of(question: &str) -> String {
    let line = question.lines().map(str::trim).find(|l| !l.is_empty());
    let line = line.unwrap_or("Untitled");
    if line.chars().count() <= TITLE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Now, in seconds since the epoch.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// When a session was last asked in, as a reader says it: "just now", "5
/// min ago", "3 h ago" today, else its day ("Nov 3").
pub(crate) fn when(updated: i64, now: i64) -> String {
    let ago = (now - updated).max(0);
    match ago {
        0..60 => "just now".to_string(),
        60..3_600 => format!("{} min ago", ago / 60),
        3_600..86_400 => format!("{} h ago", ago / 3_600),
        _ => crate::ui::short_date(updated, now),
    }
}

/// How a session answers a search: found, and — when what was looked for
/// is in its questions or answers rather than its name — the passage it is
/// in.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Found {
    pub passage: Option<String>,
}

/// How much of a question or answer is shown around a word found in it.
const PASSAGE_CHARS: usize = 64;

/// Whether `session` has every word of `query` in it — its name, its
/// questions or its answers, in any case — and where. An empty query finds
/// every session, no passage shown.
pub(crate) fn search(session: &AskSession, query: &str) -> Option<Found> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if words.is_empty() {
        return Some(Found { passage: None });
    }
    let title = session.title.to_lowercase();
    let texts: Vec<&str> = session
        .turns
        .iter()
        .flat_map(|t| [t.question.as_str(), t.answer.as_str()])
        .collect();
    let lowered: Vec<String> = texts.iter().map(|t| t.to_lowercase()).collect();
    let everywhere =
        |word: &String| title.contains(word) || lowered.iter().any(|t| t.contains(word));
    if !words.iter().all(everywhere) {
        return None;
    }
    if words.iter().all(|w| title.contains(w)) {
        return Some(Found { passage: None });
    }
    // The first passage holding a word the name does not.
    let word = words.iter().find(|w| !title.contains(w.as_str()))?;
    let passage = texts.iter().zip(&lowered).find_map(|(text, low)| {
        low.find(word.as_str())
            .map(|at| passage_around(text, low, at))
    });
    Some(Found { passage })
}

/// The line of `text` around byte `at` of its lowercase `low`, cut to
/// [`PASSAGE_CHARS`], with an ellipsis where it was cut.
fn passage_around(text: &str, low: &str, at: usize) -> String {
    // The lowercase text may differ in length from the text: count in
    // characters, which lowercasing keeps for all but a few scripts.
    let char_at = low[..at].chars().count();
    let chars: Vec<char> = text.chars().collect();
    let char_at = char_at.min(chars.len());
    let line_start = chars[..char_at]
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(0, |i| i + 1);
    let line_end = chars[char_at..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |i| char_at + i);
    // A line that fits is shown whole; a longer one, around the word.
    let (mut start, mut end) = if line_end - line_start <= PASSAGE_CHARS {
        (line_start, line_end)
    } else {
        let start = char_at.saturating_sub(PASSAGE_CHARS / 3).max(line_start);
        (start, (start + PASSAGE_CHARS).min(line_end))
    };
    // Cut between words, never through one: forward to the next word's
    // start, back to the last word's end (but never past the word found).
    if start > line_start
        && let Some(gap) = chars[start..char_at].iter().position(|c| c.is_whitespace())
    {
        start += gap + 1;
    }
    if end < line_end
        && let Some(gap) = chars[char_at..end].iter().rposition(|c| c.is_whitespace())
    {
        end = char_at + gap;
    }
    let body: String = chars[start..end].iter().collect();
    let body = body.trim();
    format!(
        "{}{body}{}",
        if start > line_start { "…" } else { "" },
        if end < line_end { "…" } else { "" }
    )
}

/// The sessions kept in `dir`, newest first; none when there are none, or
/// the file is not one this version of clew wrote.
pub(crate) fn load(dir: &Path) -> Vec<AskSession> {
    let Ok(bytes) = std::fs::read(dir.join(FILE)) else {
        return Vec::new();
    };
    match serde_json::from_slice::<SessionsFile>(&bytes) {
        Ok(file) if file.version == VERSION => {
            let mut sessions = file.sessions;
            sessions.sort_by_key(|s| std::cmp::Reverse(s.updated));
            sessions.truncate(MAX_SESSIONS);
            sessions
        }
        _ => Vec::new(),
    }
}

/// Keep `sessions` in `dir`: written whole beside the file, then moved over
/// it, so a crash leaves the old file or the new, never half of one.
pub(crate) fn save(dir: &Path, sessions: &[AskSession]) -> std::io::Result<()> {
    clew_core::derived::ensure_private_dir(dir)?;
    let file = SessionsFile {
        version: VERSION,
        sessions: sessions.to_vec(),
    };
    let bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!("{FILE}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(FILE))
}

impl App {
    /// Keep the conversation on screen as its session, newest first, and
    /// write the sessions out. A conversation with nothing answered yet is
    /// no session.
    pub(crate) fn keep_ask_session(&mut self) {
        let turns: Vec<SavedTurn> = self
            .proj
            .ask_turns
            .iter()
            .filter(|t| !t.streaming)
            .map(|t| SavedTurn {
                question: t.question.clone(),
                spoken: t.spoken,
                answer: t.answer_md.clone(),
            })
            .collect();
        let Some(first) = turns.first() else {
            return;
        };
        // Only looked at, not asked in: it keeps its place, and the file is
        // left as it is.
        if let Some(id) = self.proj.ask_session
            && self
                .proj
                .ask_sessions
                .iter()
                .any(|s| s.id == id && s.turns == turns)
        {
            return;
        }
        let id = match self.proj.ask_session {
            Some(id) => id,
            None => {
                let id = self
                    .proj
                    .ask_sessions
                    .iter()
                    .map(|s| s.id)
                    .max()
                    .unwrap_or(0)
                    + 1;
                self.proj.ask_session = Some(id);
                id
            }
        };
        let title = self
            .proj
            .ask_sessions
            .iter()
            .find(|s| s.id == id)
            .map_or_else(|| title_of(&first.question), |s| s.title.clone());
        self.proj.ask_sessions.retain(|s| s.id != id);
        self.proj.ask_sessions.insert(
            0,
            AskSession {
                id,
                title,
                updated: now(),
                turns,
            },
        );
        self.proj.ask_sessions.truncate(MAX_SESSIONS);
        self.write_ask_sessions();
    }

    /// Write the sessions to the project's derived directory; with none (no
    /// data directory), they are kept for this run only.
    fn write_ask_sessions(&mut self) {
        if let Some(dir) = self.proj.derived_dir.clone()
            && let Err(e) = save(&dir, &self.proj.ask_sessions)
        {
            self.status = format!("Could not keep Ask's sessions: {e}");
        }
    }

    /// The conversation on screen let go — its streams stopped, its pinned
    /// code and the ACP agent's session with it — for another to take its
    /// place.
    fn leave_ask_session(&mut self) -> Task<Message> {
        let stop = self.stop_ask_streams();
        self.keep_ask_session();
        self.proj.ask_turns.clear();
        self.proj.ask_pins.clear();
        self.proj.asking = false;
        if let Some(link) = self.proj.acp.as_mut() {
            link.session = None;
            link.options.clear();
        }
        self.ask_sessions_menu = false;
        self.ask_sessions_query.clear();
        stop
    }

    /// Start a new session: an empty card, the last one kept in the menu.
    pub(crate) fn new_ask_session(&mut self) -> Task<Message> {
        let stop = self.leave_ask_session();
        self.proj.ask_session = None;
        self.reveal_ask();
        Task::batch([stop, self.on_open_ask()])
    }

    /// Go back to session `id`: its questions and answers on the card, the
    /// context of what is asked next.
    pub(crate) fn switch_ask_session(&mut self, id: u64) -> Task<Message> {
        if self.proj.ask_session == Some(id) {
            self.ask_sessions_menu = false;
            self.ask_sessions_query.clear();
            return Task::none();
        }
        let Some(session) = self.proj.ask_sessions.iter().find(|s| s.id == id).cloned() else {
            return Task::none();
        };
        let stop = self.leave_ask_session();
        self.proj.ask_session = Some(id);
        let mut tasks = vec![stop];
        for turn in session.turns {
            let stream = self
                .next_req_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (answer, task) = self.prepare_segments(&turn.answer);
            tasks.push(task);
            self.proj.ask_turns.push(AskTurn {
                stream,
                question: turn.question,
                spoken: turn.spoken,
                answer_md: turn.answer,
                answer,
                ..AskTurn::default()
            });
        }
        self.reveal_ask();
        Task::batch(tasks)
    }

    /// Forget session `id`; the one on screen, forgotten, leaves an empty
    /// card in its place.
    pub(crate) fn delete_ask_session(&mut self, id: u64) -> Task<Message> {
        let stop = if self.proj.ask_session == Some(id) {
            let stop = self.stop_ask_streams();
            self.proj.ask_turns.clear();
            self.proj.ask_pins.clear();
            self.proj.asking = false;
            self.proj.ask_session = None;
            stop
        } else {
            Task::none()
        };
        self.proj.ask_sessions.retain(|s| s.id != id);
        self.write_ask_sessions();
        stop
    }

    /// The sessions the menu's search finds, newest first, each with how it
    /// was found.
    pub(crate) fn found_ask_sessions(&self) -> Vec<(&AskSession, Found)> {
        self.proj
            .ask_sessions
            .iter()
            .filter_map(|s| search(s, &self.ask_sessions_query).map(|found| (s, found)))
            .collect()
    }

    /// The name of the session on screen: "New session" until something
    /// has been asked in it.
    pub(crate) fn ask_session_title(&self) -> String {
        self.proj
            .ask_session
            .and_then(|id| self.proj.ask_sessions.iter().find(|s| s.id == id))
            .map(|s| s.title.clone())
            .or_else(|| self.proj.ask_turns.first().map(|t| title_of(&t.question)))
            .unwrap_or_else(|| "New session".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session is named for its first question's first line, cut short.
    #[test]
    fn a_session_is_named_for_its_first_question() {
        assert_eq!(title_of("Where is auth done?"), "Where is auth done?");
        assert_eq!(title_of("\n  Why this?\nand more"), "Why this?");
        let long = "Where does the server turn the files that changed into rels it sends?";
        let title = title_of(long);
        assert!(
            title.ends_with('…') && title.chars().count() <= TITLE_CHARS,
            "{title}"
        );
        assert_eq!(title_of("   "), "Untitled");
    }

    /// When a session was last asked in, as a reader says it.
    #[test]
    fn when_reads_as_a_reader_says_it() {
        let now = 1_793_000_000;
        assert_eq!(when(now - 5, now), "just now");
        assert_eq!(when(now - 300, now), "5 min ago");
        assert_eq!(when(now - 3 * 3_600, now), "3 h ago");
        assert_eq!(
            when(now - 3 * 86_400, now),
            crate::ui::short_date(now - 3 * 86_400, now)
        );
    }

    /// A search finds a session with every word of it in its name, its
    /// questions or its answers, in any case; found past its name, it says
    /// the passage, cut short around the word.
    #[test]
    fn a_search_finds_sessions_by_what_was_asked_and_answered() {
        let session = AskSession {
            id: 1,
            title: "Where is the origin?".into(),
            updated: 0,
            turns: vec![
                SavedTurn {
                    question: "Where is the origin?".into(),
                    spoken: false,
                    answer: "It is set in `origin.rs`, by `set_origin`.\nThe server reads it \
                             when a project opens, then keeps it for the whole session it \
                             serves, so a later change waits for a reopen."
                        .into(),
                },
                SavedTurn {
                    question: "And the WATCHER?".into(),
                    spoken: true,
                    answer: "The watcher batches changes.".into(),
                },
            ],
        };
        assert_eq!(search(&session, ""), Some(Found { passage: None }));
        assert_eq!(
            search(&session, "ORIGIN"),
            Some(Found { passage: None }),
            "its name"
        );
        let found = search(&session, "origin set_origin").expect("both words");
        assert_eq!(
            found.passage.as_deref(),
            Some("It is set in `origin.rs`, by `set_origin`.")
        );
        let found = search(&session, "reopen").expect("in an answer");
        let passage = found.passage.unwrap();
        assert!(
            passage.starts_with('…') && passage.contains("reopen"),
            "{passage}"
        );
        assert!(
            passage.starts_with("…when")
                || passage.starts_with("…then")
                || passage[3..].starts_with(char::is_alphabetic),
            "starts on a word: {passage}"
        );
        assert!(
            !passage.contains(" a…") && !passage.ends_with(" …"),
            "{passage}"
        );
        assert!(passage.chars().count() <= PASSAGE_CHARS + 2, "{passage}");
        assert_eq!(
            search(&session, "watcher").and_then(|f| f.passage),
            Some("And the WATCHER?".into()),
            "in any case, the first passage holding it"
        );
        assert_eq!(search(&session, "origin nowhere"), None, "every word");
    }

    /// Kept and read back, newest first; a file of another format, or none,
    /// is no sessions.
    #[test]
    fn sessions_are_kept_and_read_back() {
        let dir = crate::app::tests::test_dir("ask-sessions-file");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&dir).is_empty());
        let session = |id, updated| AskSession {
            id,
            title: format!("s{id}"),
            updated,
            turns: vec![SavedTurn {
                question: "q".into(),
                spoken: id == 2,
                answer: "a".into(),
            }],
        };
        save(&dir, &[session(1, 10), session(2, 30)]).unwrap();
        let back = load(&dir);
        assert_eq!(back, vec![session(2, 30), session(1, 10)]);
        std::fs::write(dir.join(FILE), r#"{"version":99,"sessions":[]}"#).unwrap();
        assert!(load(&dir).is_empty());
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert!(load(&dir).is_empty());
    }
}
