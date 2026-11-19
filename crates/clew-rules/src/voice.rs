//! Where heard words go, once they are classified.

/// What [`route`] decides from: read off the words and the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteFacts {
    /// "Bye, Clew".
    pub bye: bool,
    /// The microphone's label, "Stop listening", said.
    pub stop_listening_said: bool,
    /// Nothing said past Clew's name.
    pub empty: bool,
    /// A command waits for a yes.
    pub confirm_waiting: bool,
    /// Only a word that stops Clew ("stop", "okay, be quiet").
    pub hush: bool,
    /// What opens with a yes or a no.
    pub yes_or_no: Option<bool>,
    /// What it is to a question waiting: one of its answers, or a yes or a no.
    pub to_question: Option<QuestionWord>,
    /// An answer is being written in Ask.
    pub answer_being_written: bool,
    /// Spoken commands are on.
    pub run_commands: bool,
}

/// What words are to a question waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionWord {
    YesOrNo,
    Answer,
}

/// One step of where heard words go. Every step acts but the two `Try`s,
/// which act only on words that are a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Bye,
    SayHowToStop,
    Nothing,
    RunWaiting,
    DeclineWaiting,
    Hush,
    TryCommand,
    TryCommandsBackOn,
    Hold,
    Ask,
}

/// One step.
#[expect(
    clippy::vec_init_then_push,
    reason = "Aeneas translates `Vec::new` and `push`; `vec![]` is a box it does not"
)]
fn only(step: Step) -> Vec<Step> {
    let mut steps = Vec::new();
    steps.push(step);
    steps
}

/// Where heard words go: the steps to take, in order, until one acts.
///
/// The rules the voice reviews kept returning to are theorems about this
/// function (`verification/Clew/VoiceRoute.lean`, and the proof that the
/// Lean Aeneas makes of this source is that model): a word that stops Clew
/// is never sent to Ask, nor taken for a yes; what is said to a question
/// waiting is never asked; "Bye, Clew" is always heard first.
pub fn route(f: &RouteFacts) -> Vec<Step> {
    if f.bye {
        return only(Step::Bye);
    }
    if f.stop_listening_said {
        return only(Step::SayHowToStop);
    }
    if f.empty {
        return only(Step::Nothing);
    }
    if f.confirm_waiting {
        // "Stop" is a no too — "Okay, stop" as well, though it opens with a
        // yes. Anything else said puts the command away, and is read as
        // itself.
        let answer = if f.hush { Some(false) } else { f.yes_or_no };
        match answer {
            Some(true) => return only(Step::RunWaiting),
            Some(false) => return only(Step::DeclineWaiting),
            None => {}
        }
    }
    // An answer of a stop word's name the question offers is that answer.
    let offers_answer = match f.to_question {
        Some(QuestionWord::Answer) => true,
        Some(QuestionWord::YesOrNo) | None => false,
    };
    let hush = f.hush && !offers_answer;
    // While an answer is being written, a stop word is that answer's —
    // before any command of its name (the debugger's Stop).
    if hush && f.answer_being_written {
        return only(Step::Hush);
    }
    let mut steps = only(if f.run_commands {
        Step::TryCommand
    } else {
        Step::TryCommandsBackOn
    });
    #[expect(
        clippy::redundant_pattern_matching,
        reason = "Aeneas's library models no `Option::is_some`"
    )]
    let waiting = match f.to_question {
        Some(_) => true,
        None => false,
    };
    steps.push(if hush {
        Step::Hush
    } else if waiting {
        Step::Hold
    } else {
        Step::Ask
    });
    steps
}
