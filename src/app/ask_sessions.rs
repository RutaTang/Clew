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
