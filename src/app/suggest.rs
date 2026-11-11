//! Ask's "Try asking": questions written for the project and what the reader
//! is on — the file, the function under the caret, what its notes say —
//! by the model, a few at random each new session. The same four templates
//! every time read as a form; without a key, or until the model's arrive,
//! those are what is offered.

use crate::app::prelude::*;
use crate::*;

/// How many questions one request asks the model for: more than are shown,
/// so the next new session has others to offer.
const POOL: usize = 8;
/// How many are offered at once.
pub(crate) const SHOWN: usize = 4;
/// The longest question offered, in characters: one line of the card.
const LONGEST: usize = 110;
/// How much of a note goes into the request, in characters.
const NOTE_CHARS: usize = 600;

/// What the model is asked to do.
pub(crate) const SUGGEST_SYSTEM: &str = "You suggest questions a developer reading a codebase \
     might ask an assistant about it. Write the questions only, one per line, with no \
     numbering, bullets or quotes. Each is short (under 12 words), specific to what you are \
     shown, and different from the others: what something is for, how it works, how data \
     flows, why it is written this way, edge cases, how to change it safely. Write names \
     from the code (functions, types, files) in backticks, like `total()`.";

/// What the questions were written for: the file the reader was on, and the
/// function under the caret. Questions for another place are not offered.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct SuggestKey {
    pub file: Option<String>,
    pub function: Option<String>,
}

/// The questions the model wrote, and for where.
#[derive(Debug, Clone, Default)]
pub struct Suggestions {
    pub key: SuggestKey,
    pub pool: Vec<String>,
}

/// The questions in what the model wrote: one a line, numbering, bullets
/// and quotes taken off; what is not a question, or too long for a line of
/// the card, left out; each once.
pub(crate) fn parse_questions(said: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in said.lines() {
        let line = line
            .trim()
            .trim_start_matches(|c: char| {
                c.is_ascii_digit() || matches!(c, '.' | ')' | '-' | '*' | '•')
            })
            .trim()
            .trim_matches(|c| matches!(c, '"' | '“' | '”' | '「' | '」'))
            .trim();
        let asks = line.ends_with('?') || line.ends_with('？');
        if !asks || line.chars().count() > LONGEST || out.iter().any(|q| q == line) {
            continue;
        }
        out.push(line.to_string());
        if out.len() == POOL {
            break;
        }
    }
    out
}

impl App {
    /// Where the reader is, as questions are written for it.
    pub(crate) fn suggest_key(&self) -> SuggestKey {
        let file = self.active_viewer().map(|v| v.rel.clone());
        let function = match self.cursor_target() {
            Some(explain::Node::Function { name, .. }) => Some(name),
            _ => None,
        };
        SuggestKey { file, function }
    }

    /// The model's questions for where the reader is, `SHOWN` of them picked
    /// by this session's draw — `None` while there are none for here.
    pub(crate) fn ai_suggestions(&self) -> Option<Vec<String>> {
        let s = self.proj.suggestions.as_ref()?;
        if s.key != self.suggest_key() || s.pool.is_empty() {
            return None;
        }
        let n = s.pool.len();
        let start = (self.proj.suggest_draw as usize) % n;
        Some(
            (0..SHOWN.min(n))
                .map(|i| s.pool[(start + i * 3) % n].clone())
                .fold(Vec::new(), |mut picked, q| {
                    if !picked.contains(&q) {
                        picked.push(q);
                    }
                    picked
                }),
        )
    }

    /// Whether the model is writing questions for where the reader is now:
    /// "Try asking" waits for them, rather than offering templates that
    /// change under the pointer when they come.
    pub(crate) fn suggestions_coming(&self) -> bool {
        self.proj.suggest_asked.as_ref() == Some(&self.suggest_key())
    }

    /// Ask the model for questions for where the reader is, as an empty
    /// conversation is shown, when there are none for here yet: one small
    /// call a place, not one a session. A new session (`fresh`) draws
    /// another few of what came. Only with a key and the setting on (Settings
    /// → Usage and cost), once at a time, never with a conversation under way.
    pub(crate) fn ask_for_suggestions(&mut self, fresh: bool) -> Task<Message> {
        if fresh {
            // Another draw from what is here.
            self.proj.suggest_draw = self.proj.suggest_draw.wrapping_add(1 + rand_step());
        }
        if !self.llm_available
            || !self.suggest_questions
            || self.proj.project.is_none()
            || !self.proj.ask_turns.is_empty()
            || self.proj.asking
        {
            return Task::none();
        }
        let key = self.suggest_key();
        let have = self
            .proj
            .suggestions
            .as_ref()
            .is_some_and(|s| s.key == key && !s.pool.is_empty());
        if have || self.proj.suggest_asked.as_ref() == Some(&key) {
            return Task::none();
        }
        let Some(cfg) = self.llm_config() else {
            return Task::none();
        };
        self.proj.suggest_asked = Some(key.clone());
        let prompt = self.suggest_prompt();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            async move {
                ai.complete(cfg, crate::app::usage::SUGGEST, SUGGEST_SYSTEM, prompt, 300)
                    .await
            },
            move |result| {
                Message::Ask(AskMsg::SuggestionsCame {
                    stamp: stamp.clone(),
                    key: key.clone(),
                    result,
                })
            },
        )
    }

    /// The model's questions came, for `key`: kept, if any could be read —
    /// a failure leaves the templates offered, and says nothing.
    pub(crate) fn on_suggestions(&mut self, key: SuggestKey, result: Result<String, String>) {
        if self.proj.suggest_asked.as_ref() == Some(&key) {
            self.proj.suggest_asked = None;
        }
        let Ok(said) = result else {
            return;
        };
        let pool = parse_questions(&said);
        if !pool.is_empty() {
            self.proj.suggestions = Some(Suggestions { key, pool });
        }
    }

    /// What the model is shown: the project, the reader's file and its
    /// note, the function under the caret and its note, the overview.
    fn suggest_prompt(&self) -> String {
        let mut out = format!("{}\n\nWrite {POOL} questions.\n", explain::UNTRUSTED_NOTE);
        if let Some(project) = self.proj.project.as_ref() {
            let name = project
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let top: Vec<&str> = project
                .tree
                .dirs
                .iter()
                .map(|(d, _)| d.as_str())
                .chain(project.tree.files.iter().map(String::as_str))
                .take(30)
                .collect();
            out.push_str(&format!(
                "\nProject: {name}\nAt its top: {}\n",
                top.join(", ")
            ));
        }
        let note = |node: &explain::Node| {
            self.proj.explain.cache.get(node).map(|c| {
                crate::app::shown_summary(c)
                    .chars()
                    .take(NOTE_CHARS)
                    .collect::<String>()
            })
        };
        if let Some(v) = self.active_viewer() {
            out.push_str(&format!("\nThe file the reader has open: {}\n", v.rel));
            if let Some(n) = note(&explain::Node::File(v.abs.clone())) {
                out.push_str(&format!("Its note: {n}\n"));
            }
        }
        if let Some(node @ explain::Node::Function { .. }) = self.cursor_target() {
            if let explain::Node::Function { name, .. } = &node {
                out.push_str(&format!("The function under the caret: {name}\n"));
            }
            if let Some(n) = note(&node) {
                out.push_str(&format!("Its note: {n}\n"));
            }
        }
        if let Some(overview) = self.proj.overview.markdown.as_ref() {
            let short: String = overview.chars().take(NOTE_CHARS).collect();
            out.push_str(&format!("\nThe project's overview begins:\n{short}\n"));
        }
        out
    }
}

/// A step for the draw, from the clock: each new session's questions are
/// another few of what is here.
fn rand_step() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) % 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the model wrote is read as questions: numbering, bullets and
    /// quotes off; what is no question, or too long, left out; each once.
    #[test]
    fn questions_are_read_from_what_the_model_wrote() {
        let said = "1. What does `Store::path` return?\n\
                    - How are models verified?\n\
                    * \"Why is the download resumed?\"\n\
                    Here are some questions:\n\
                    How are models verified?\n\
                    模型是如何校验的？\n";
        assert_eq!(
            parse_questions(said),
            [
                "What does `Store::path` return?",
                "How are models verified?",
                "Why is the download resumed?",
                "模型是如何校验的？",
            ]
        );
        let long = format!("{}?", "word ".repeat(40));
        assert!(parse_questions(&long).is_empty());
        let many: String = (0..20).map(|i| format!("Question {i}?\n")).collect();
        assert_eq!(parse_questions(&many).len(), POOL);
    }
}
