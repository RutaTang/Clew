//! Fake run and fake debug (see [`crate::fakerun`]): the inputs sheet, the
//! run worked out by the model — an agent turn on clew-server, which reads
//! the code the target calls before it answers, or one call when no turn
//! can run — and the trace stepped through in the bottom panel, the line
//! it is on lit in the code as a paused debugger's is.
//!
//! Its messages, [`FakeMsg`], arrive through `App::update_fake`.

use std::path::Path;

use crate::app::prelude::*;
use crate::fakerun::{self as fr, Field, Given, Target, Trace};
use crate::*;

/// The most of a definition the model is shown: a longer one is cut there,
/// and the question says which lines it has.
const MAX_SOURCE_LINES: usize = 600;

/// Tokens a one-call trace may take (it carries on by itself past them,
/// see `AiClient::complete`).
const ANSWER_TOKENS: u32 = 8_000;

/// A window's fake run: the sheet, the run being worked out, the trace
/// being stepped through.
#[derive(Debug, Default)]
pub struct FakeRun {
    /// The inputs sheet, while it is open.
    pub(crate) sheet: Option<Sheet>,
    /// The run the model is working out.
    pub(crate) working: Option<Working>,
    /// The trace, stepped through.
    pub(crate) sim: Option<Sim>,
    /// Why the last run brought no trace.
    pub(crate) failed: Option<String>,
    /// The inputs last run: the sheet Edit inputs and Try again open on.
    pub(crate) last: Option<Sheet>,
    /// Numbers each run and each suggestion: only the newest's answer is
    /// taken.
    seq: u64,
}

impl FakeRun {
    /// Whether the bottom panel has a fake run to show.
    pub(crate) fn shown(&self) -> bool {
        self.working.is_some() || self.sim.is_some() || self.failed.is_some()
    }
}

/// The inputs sheet: what is run, and what the reader gives it.
#[derive(Debug, Clone)]
pub struct Sheet {
    pub(crate) target: Target,
    pub(crate) fields: Vec<Field>,
    pub(crate) values: Vec<String>,
    /// A suggestion of inputs is being asked for: its number.
    pub(crate) suggesting: Option<u64>,
}

/// A run the model is working out.
#[derive(Debug)]
pub struct Working {
    pub(crate) seq: u64,
    /// The agent turn's stream, when it runs as one.
    pub(crate) stream: Option<u64>,
    pub(crate) target: Target,
    pub(crate) given: Vec<Given>,
    pub(crate) answer: fr::Answer,
    /// What the agent has done so far, as its steps say it ("Read
    /// pricing.py"), newest last.
    pub(crate) reading: Vec<String>,
}

/// A trace being stepped through.
#[derive(Debug, Clone)]
pub struct Sim {
    pub(crate) target: Target,
    pub(crate) given: Vec<Given>,
    pub(crate) trace: Trace,
    /// The step shown.
    pub(crate) at: usize,
    /// Stepped past the last line: the run is over, and what it returns
    /// is what is shown.
    pub(crate) ended: bool,
}

/// The widget id of the sheet's `i`th field.
pub(crate) fn field_id(i: usize) -> iced::widget::Id {
    iced::widget::Id::from(format!("fake-run-field-{i}"))
}

impl App {
    /// The definition a fake run at `line1` (1-based) of pane `pane`'s file
    /// would run: the innermost function or method there, else the
    /// innermost class, struct or other type.
    pub(crate) fn fake_target_at(&self, pane: usize, line1: usize) -> Option<Target> {
        let v = self.proj.panes.get(pane)?.as_ref()?;
        let sym = v
            .symbols
            .iter()
            .filter(|s| fr::runnable(&s.kind) && s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| {
                (
                    !clew_core::outline::is_callable(&s.kind),
                    s.end_line.saturating_sub(s.line),
                )
            })?;
        let lang = highlight::detect(&v.abs).unwrap_or("text");
        let lines: Vec<&str> = v.source.lines().collect();
        let from = sym.line.saturating_sub(1).min(lines.len());
        let to = sym.end_line.max(sym.line).min(lines.len());
        let to = to.min(from + MAX_SOURCE_LINES);
        let target = Target {
            name: sym.name.clone(),
            kind: sym.kind.clone(),
            rel: v.rel.clone(),
            line: from + 1,
            end_line: to.max(from + 1),
            lang: lang.to_string(),
            source: lines[from..to].join("\n"),
        };
        Some(target)
    }

    /// Open the inputs sheet on `target`: with the inputs last run on it,
    /// when it is the same definition.
    fn open_fake_sheet(&mut self, target: Target) -> Task<Message> {
        let same = |s: &&Sheet| {
            s.target.rel == target.rel
                && s.target.name == target.name
                && s.target.kind == target.kind
        };
        let fields = fr::fields(&target);
        let values = match self.proj.fake.last.as_ref().filter(same) {
            Some(last) if last.fields == fields => last.values.clone(),
            _ => vec![String::new(); fields.len()],
        };
        self.proj.fake.sheet = Some(Sheet {
            target,
            fields,
            values,
            suggesting: None,
        });
        operation::focus(field_id(0))
    }

    /// Fake Run… from the context menu: the definition it was opened on.
    fn on_fake_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        match self.fake_target_at(menu.pane, menu.line + 1) {
            Some(target) => self.open_fake_sheet(target),
            None => {
                self.status = tr!("No function, class or struct here to fake-run").into();
                Task::none()
            }
        }
    }

    /// Fake Run from the keyboard or the palette: the definition the caret
    /// is in.
    fn on_fake_at_caret(&mut self) -> Task<Message> {
        let pane = self.proj.active;
        let line = self
            .active_viewer()
            .and_then(|v| v.caret)
            .map(|(l, _)| l + 1);
        match line.and_then(|l| self.fake_target_at(pane, l)) {
            Some(target) => self.open_fake_sheet(target),
            None => {
                self.status =
                    tr!("Put the caret in a function, class or struct to fake-run it").into();
                Task::none()
            }
        }
    }

    /// Run the sheet's target with its inputs: an agent turn when the server
    /// can run one (it reads what the code calls), else one call.
    fn on_fake_run(&mut self) -> Task<Message> {
        if self.proj.fake.sheet.is_none() {
            return Task::none();
        }
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        let Some(mut sheet) = self.proj.fake.sheet.take() else {
            return Task::none();
        };
        sheet.suggesting = None;
        let stopped = self.stop_fake_work();
        let given = fr::given(&sheet.fields, &sheet.values);
        let target = sheet.target.clone();
        self.proj.fake.last = Some(sheet);
        self.proj.fake.failed = None;
        self.proj.fake.sim = None;
        self.proj.fake.seq += 1;
        let seq = self.proj.fake.seq;
        self.show_bottom = true;
        self.status = tr!("Working out what {name} does…", name = target.name.clone());
        let reply = clew_core::llm::reply_language();
        let mut working = Working {
            seq,
            stream: None,
            target: target.clone(),
            given: given.clone(),
            answer: fr::Answer::default(),
            reading: Vec::new(),
        };
        if self.server.is_up() && self.ai_on_server() {
            let question = fr::question(&target, &given, true, reply.as_deref());
            if let Some((stream, pump)) = self.start_fake_turn(question) {
                working.stream = Some(stream);
                self.proj.fake.working = Some(working);
                return Task::batch([stopped, pump]);
            }
        }
        // No agent turn: the model in one call, from the target's source.
        let prompt = fr::question(&target, &given, false, reply.as_deref());
        self.proj.fake.working = Some(working);
        let ai = self.ai_client();
        let stamp = self.stamp();
        let call = Task::perform(
            async move { ai.complete(cfg, fr::SYSTEM, prompt, ANSWER_TOKENS).await },
            move |result| {
                Message::Fake(FakeMsg::Answered {
                    stamp: stamp.clone(),
                    seq,
                    result,
                })
            },
        );
        Task::batch([stopped, call])
    }

    /// Send `question` as an agent turn with no tools but the server's own,
    /// and pump what comes back into [`FakeMsg`]s. `None` when it could
    /// not be sent.
    fn start_fake_turn(&mut self, question: String) -> Option<(u64, Task<Message>)> {
        use iced::futures::SinkExt;
        let stream_id = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<AgentPiece>();
        self.proj.link.agent_streams.insert(stream_id, tx);
        let sent = self.send_message(clew_protocol::ClientMessage {
            id: stream_id,
            request: clew_protocol::Request::AgentAsk {
                stream: stream_id,
                question,
                history: Vec::new(),
                context: String::new(),
                tools: Vec::new(),
                choices: self.proj.lang_choices.clone(),
            },
        });
        if !sent {
            self.proj.link.agent_streams.remove(&stream_id);
            return None;
        }
        let stamp = self.stamp();
        let stream = iced::stream::channel(
            256,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                let mut rx = rx;
                let mut ended = false;
                while let Some(piece) = rx.recv().await {
                    let stamp = stamp.clone();
                    let stream = stream_id;
                    let msg = match piece {
                        AgentPiece::Step(s) => FakeMsg::Read {
                            stamp,
                            stream,
                            title: s.title,
                        },
                        AgentPiece::ToolCall { call, .. } => FakeMsg::ToolCalled {
                            stamp,
                            stream,
                            call,
                        },
                        AgentPiece::Delta(text) => FakeMsg::Delta {
                            stamp,
                            stream,
                            text,
                        },
                        AgentPiece::Draft(text) => FakeMsg::Draft {
                            stamp,
                            stream,
                            text,
                        },
                        AgentPiece::DraftEnd(kept) => FakeMsg::DraftEnd {
                            stamp,
                            stream,
                            kept,
                        },
                        AgentPiece::Done(outcome) => {
                            ended = true;
                            FakeMsg::TurnEnded {
                                stamp,
                                stream,
                                outcome,
                            }
                        }
                    };
                    if output.send(Message::Fake(msg)).await.is_err() || ended {
                        break;
                    }
                }
                // The channel closed with no end said (the connection went):
                // the run ends here, rather than working on for good.
                if !ended {
                    let _ = output
                        .send(Message::Fake(FakeMsg::TurnEnded {
                            stamp,
                            stream: stream_id,
                            outcome: clew_protocol::StreamOutcome::Failed(
                                tr!("the connection to clew-server was lost").into(),
                            ),
                        }))
                        .await;
                }
            },
        );
        Some((stream_id, Task::run(stream, |m| m)))
    }

    /// Stop the run being worked out, if one is: its turn is cancelled on
    /// the server, and its answer no longer waited for.
    fn stop_fake_work(&mut self) -> Task<Message> {
        if let Some(stream) = self.proj.fake.working.take().and_then(|w| w.stream) {
            self.proj.link.agent_streams.remove(&stream);
            let _ = self.send_to_server(clew_protocol::Request::Cancel { id: stream });
        }
        Task::none()
    }

    /// The working run, when `stream` is its turn's.
    fn fake_turn(&mut self, stream: u64) -> Option<&mut Working> {
        self.proj
            .fake
            .working
            .as_mut()
            .filter(|w| w.stream == Some(stream))
    }

    /// The model's answer for the working run: parsed, checked against the
    /// project, and stepped into at its first line — or why it could not
    /// be.
    fn fake_answered(&mut self, answer: Result<String, String>) -> Task<Message> {
        let Some(working) = self.proj.fake.working.take() else {
            return Task::none();
        };
        let parsed = answer.and_then(|text| fr::parse(&text, |file| self.fake_resolve(file)));
        match parsed {
            Ok(trace) => {
                self.status = if trace.dropped > 0 {
                    trn!(
                        "Fake run ready — 1 step named no line of this project and was left out",
                        "Fake run ready — {n} steps named no line of this project and were left out",
                        trace.dropped
                    )
                } else {
                    tr!("Fake run ready — step through it below").into()
                };
                self.proj.fake.sim = Some(Sim {
                    target: working.target,
                    given: working.given,
                    trace,
                    at: 0,
                    ended: false,
                });
                self.show_bottom = true;
                self.fake_show_step()
            }
            Err(why) => {
                self.status = tr!("The fake run brought no trace").into();
                self.proj.fake.failed = Some(why);
                Task::none()
            }
        }
    }

    /// A step's file as the project knows it — its relative path and how
    /// many lines it has, when that is known (an open file's) — or `None`
    /// when the project has no such file.
    fn fake_resolve(&self, file: &str) -> Option<(String, usize)> {
        if !clew_core::statefile::safe_rel(file) {
            return None;
        }
        let project = self.proj.project.as_ref()?;
        let rel = project
            .files
            .iter()
            .find(|f| f.rel == file)
            .map(|f| f.rel.clone())?;
        let lines = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.rel == rel)
            .map_or(usize::MAX, |v| v.lines.len().max(1));
        Some((rel, lines))
    }

    /// Show the step the trace is at: its file, open at its line.
    fn fake_show_step(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(sim) = self.proj.fake.sim.as_ref() else {
            return Task::none();
        };
        let Some(step) = sim.trace.steps.get(sim.at) else {
            return Task::none();
        };
        let abs = root.join(&step.file);
        let line = step.line;
        self.open_file(abs, Some(line), false)
    }

    /// The line of `abs` the fake debug session is on, when it is on one of
    /// `abs`'s — lit in the code as a paused debugger's is — and no real
    /// session is running, whose line that is.
    pub(crate) fn fake_line(&self, abs: &Path) -> Option<usize> {
        if self.debug.session.is_some() {
            return None;
        }
        let sim = self.proj.fake.sim.as_ref().filter(|s| !s.ended)?;
        let step = sim.trace.steps.get(sim.at)?;
        let root = &self.proj.project.as_ref()?.root;
        (root.join(&step.file) == abs).then_some(step.line)
    }

    /// One of the debugger's step commands, on the trace.
    fn on_fake_step(&mut self, cmd: SimCmd) -> Task<Message> {
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        let breakpoints = &self.debug.breakpoints;
        let Some(sim) = self.proj.fake.sim.as_mut() else {
            return Task::none();
        };
        let next = match cmd {
            SimCmd::Restart => Some(0),
            SimCmd::Back => {
                if sim.ended {
                    Some(sim.at)
                } else {
                    sim.at.checked_sub(1)
                }
            }
            _ if sim.ended => return Task::none(),
            SimCmd::Into => sim.trace.step_in(sim.at),
            SimCmd::Over => sim.trace.step_over(sim.at),
            SimCmd::Out => sim.trace.step_out(sim.at),
            SimCmd::Continue => sim.trace.run_to(sim.at, |s| {
                root.as_ref().is_some_and(|root| {
                    breakpoints
                        .get(&root.join(&s.file))
                        .is_some_and(|lines| lines.contains_key(&s.line))
                })
            }),
        };
        match next {
            Some(at) => {
                sim.at = at;
                sim.ended = false;
                self.fake_show_step()
            }
            // Back from the first line: nowhere to go.
            None if cmd == SimCmd::Back => Task::none(),
            // Past the last line: the run is over.
            None => {
                sim.ended = true;
                sim.at = sim.trace.steps.len().saturating_sub(1);
                Task::none()
            }
        }
    }

    /// Go to step `at` (a frame of the stack).
    fn on_fake_goto(&mut self, at: usize) -> Task<Message> {
        let Some(sim) = self.proj.fake.sim.as_mut() else {
            return Task::none();
        };
        if at >= sim.trace.steps.len() {
            return Task::none();
        }
        sim.at = at;
        sim.ended = false;
        self.fake_show_step()
    }

    /// Ask about the step shown: the question, with the step and its
    /// values, goes to Ask.
    fn on_fake_ask_why(&mut self) -> Task<Message> {
        let Some(sim) = self.proj.fake.sim.as_ref() else {
            return Task::none();
        };
        let Some(step) = sim.trace.steps.get(sim.at) else {
            return Task::none();
        };
        let mut q = tr!(
            "In a fake run of {name}, why does {place} do this?",
            name = sim.target.name.clone(),
            place = format!("{}:{}", step.file, step.line)
        )
        .to_string();
        if !step.note.is_empty() {
            q.push_str(&format!("\n\n{}", step.note));
        }
        if !step.vars.is_empty() {
            let values: Vec<String> = step
                .vars
                .iter()
                .map(|(n, v)| format!("{n} = {v}"))
                .collect();
            q.push_str(&format!("\n{}", values.join(", ")));
        }
        let inputs: Vec<String> = sim
            .given
            .iter()
            .map(|g| format!("{}: {}", g.label, g.value))
            .collect();
        if !inputs.is_empty() {
            q.push_str(&format!("\n({})", inputs.join("; ")));
        }
        self.proj.ask_input = q;
        self.reveal_ask();
        self.on_ask_submit()
    }

    /// Ask the model for inputs to fill the sheet's empty fields with.
    fn on_fake_suggest(&mut self) -> Task<Message> {
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        self.proj.fake.seq += 1;
        let seq = self.proj.fake.seq;
        let Some(sheet) = self.proj.fake.sheet.as_mut() else {
            return Task::none();
        };
        sheet.suggesting = Some(seq);
        let prompt = suggest_prompt(&sheet.target, &sheet.fields);
        let n = sheet.fields.len();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            async move {
                let text = ai.complete(cfg, SUGGEST_SYSTEM, prompt, 1_500).await?;
                parse_suggestion(&text, n)
            },
            move |result| {
                Message::Fake(FakeMsg::Suggested {
                    stamp: stamp.clone(),
                    seq,
                    result,
                })
            },
        )
    }

    pub(crate) fn update_fake(&mut self, message: FakeMsg) -> Task<Message> {
        match message {
            FakeMsg::FromMenu => self.on_fake_from_menu(),
            FakeMsg::AtCaret => self.on_fake_at_caret(),
            FakeMsg::Field(i, value) => {
                if let Some(slot) = self
                    .proj
                    .fake
                    .sheet
                    .as_mut()
                    .and_then(|s| s.values.get_mut(i))
                {
                    *slot = value;
                }
                Task::none()
            }
            FakeMsg::Suggest => self.on_fake_suggest(),
            FakeMsg::Suggested { seq, result, .. } => {
                let Some(sheet) = self
                    .proj
                    .fake
                    .sheet
                    .as_mut()
                    .filter(|s| s.suggesting == Some(seq))
                else {
                    return Task::none();
                };
                sheet.suggesting = None;
                match result {
                    // What the reader typed stays; the rest is filled.
                    Ok(values) => {
                        for (slot, value) in sheet.values.iter_mut().zip(values) {
                            if slot.trim().is_empty() {
                                *slot = value;
                            }
                        }
                    }
                    Err(why) => {
                        self.status = tr!("No inputs suggested: {why}", why = why);
                    }
                }
                Task::none()
            }
            FakeMsg::Run => self.on_fake_run(),
            FakeMsg::CloseSheet => {
                self.proj.fake.sheet = None;
                Task::none()
            }
            FakeMsg::Read { stream, title, .. } => {
                if let Some(w) = self.fake_turn(stream)
                    && !title.trim().is_empty()
                {
                    w.reading.push(title);
                }
                Task::none()
            }
            FakeMsg::Delta { stream, text, .. } => {
                if let Some(w) = self.fake_turn(stream) {
                    w.answer.delta(&text);
                }
                Task::none()
            }
            FakeMsg::Draft { stream, text, .. } => {
                if let Some(w) = self.fake_turn(stream) {
                    w.answer.draft(&text);
                }
                Task::none()
            }
            FakeMsg::DraftEnd { stream, kept, .. } => {
                if let Some(w) = self.fake_turn(stream) {
                    w.answer.draft_end(kept);
                }
                Task::none()
            }
            // It was offered no tool of the window's: none is run.
            FakeMsg::ToolCalled { stream, call, .. } => {
                let _ = self.send_to_server(clew_protocol::Request::AgentToolResult {
                    stream,
                    call,
                    content: "This turn has no such tool.".into(),
                    is_error: true,
                });
                Task::none()
            }
            FakeMsg::TurnEnded {
                stream, outcome, ..
            } => {
                let Some(w) = self.fake_turn(stream) else {
                    return Task::none();
                };
                let text = std::mem::take(&mut w.answer.text);
                self.proj.link.agent_streams.remove(&stream);
                match outcome {
                    clew_protocol::StreamOutcome::Done => self.fake_answered(Ok(text)),
                    clew_protocol::StreamOutcome::Stopped => {
                        self.proj.fake.working = None;
                        Task::none()
                    }
                    clew_protocol::StreamOutcome::Failed(why) => self.fake_answered(Err(why)),
                }
            }
            FakeMsg::Answered { seq, result, .. } => {
                if self.proj.fake.working.as_ref().map(|w| (w.seq, w.stream)) != Some((seq, None)) {
                    return Task::none();
                }
                self.fake_answered(result)
            }
            FakeMsg::Stop => {
                let stopped = self.stop_fake_work();
                self.status = tr!("Fake run stopped").into();
                stopped
            }
            FakeMsg::Step(cmd) => self.on_fake_step(cmd),
            FakeMsg::Goto(at) => self.on_fake_goto(at),
            FakeMsg::AskWhy => self.on_fake_ask_why(),
            FakeMsg::EditInputs => {
                let Some(last) = self.proj.fake.last.clone() else {
                    return Task::none();
                };
                self.proj.fake.sheet = Some(last);
                operation::focus(field_id(0))
            }
            FakeMsg::Close => {
                let stopped = self.stop_fake_work();
                self.proj.fake.sim = None;
                self.proj.fake.failed = None;
                stopped
            }
        }
    }
}

/// The model's instructions for suggesting inputs.
const SUGGEST_SYSTEM: &str = "You suggest inputs for a function or a type, so a reader can see \
how it behaves on a typical case. You answer with one JSON object and nothing else.";

/// Ask for one typical value per field of `fields`, for `target`.
fn suggest_prompt(target: &Target, fields: &[Field]) -> String {
    use clew_core::explain::{fenced, prompt_label};
    let mut p = format!(
        "Suggest inputs for the {} `{}` ({}), so that running it shows what it is for:\n",
        prompt_label(&target.kind),
        prompt_label(&target.name),
        prompt_label(&target.lang),
    );
    p.push_str(&fenced(&target.lang, &target.source));
    p.push_str("\nThe fields, in order:\n");
    for (i, field) in fields.iter().enumerate() {
        let what = match field {
            Field::Param { name, hint } if hint.is_empty() => format!("the parameter `{name}`"),
            Field::Param { name, hint } => format!("the parameter `{name}` ({hint})"),
            Field::Inputs => "its inputs".to_string(),
            Field::Construct => "how it is made (a constructor call or a literal)".to_string(),
            Field::Calls => "the calls made on it, in order".to_string(),
            Field::Context => {
                "anything else it depends on (files, services, the time) — empty if nothing"
                    .to_string()
            }
        };
        p.push_str(&format!("{}. {what}\n", i + 1));
    }
    p.push_str(
        "\nAnswer with ONLY {\"values\": [\"…\", …]}, one string per field in that order, each \
         written as code in the language would write the value.",
    );
    p
}

/// The suggested values, one per field (`n`); missing ones empty.
fn parse_suggestion(text: &str, n: usize) -> Result<Vec<String>, String> {
    let start = text.find('{').ok_or("no JSON object in the answer")?;
    let end = text.rfind('}').ok_or("no JSON object in the answer")?;
    if end < start {
        return Err("malformed JSON in the answer".into());
    }
    let v: serde_json::Value =
        serde_json::from_str(&text[start..=end]).map_err(|e| e.to_string())?;
    let values = v
        .get("values")
        .and_then(|v| v.as_array())
        .ok_or("the answer has no values")?;
    let mut out: Vec<String> = values
        .iter()
        .map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    out.resize(n, String::new());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A suggestion gives one value per field, strings as themselves and
    /// anything else as its JSON; too few are padded, too many cut.
    #[test]
    fn a_suggestion_fills_each_field() {
        assert_eq!(
            parse_suggestion("```json\n{\"values\": [\"[1, 2]\", 3]}\n```", 3).unwrap(),
            ["[1, 2]", "3", ""]
        );
        assert_eq!(
            parse_suggestion("{\"values\": [\"a\", \"b\"]}", 1).unwrap(),
            ["a"]
        );
        assert!(parse_suggestion("no", 1).is_err());
        assert!(parse_suggestion("{\"other\": 1}", 1).is_err());
    }
}
