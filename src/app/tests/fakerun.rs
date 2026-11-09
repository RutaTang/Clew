//! Fake run and fake debug (`app::fakerun`): the sheet opens on the
//! definition asked for, the run is an agent turn whose answer becomes a
//! trace, and the trace is stepped through as a debugger steps.

use super::*;
use crate::fakerun::Field;
use crate::{FakeMsg, SimCmd};

/// A project open on `src/lib.rs`, with a model configured.
fn fake_app(tag: &str) -> App {
    let mut app = scanned_app(tag);
    app.llm_config_cache = Some(Some(unreachable_llm()));
    open_synchronously(&mut app, "src/lib.rs", None);
    assert!(
        !app.active_viewer().unwrap().symbols.is_empty(),
        "the file's outline is known"
    );
    app
}

/// The context menu, opened on 0-based `line` of the active pane.
fn menu_at(app: &mut App, line: usize) {
    app.proj.context_menu = Some(crate::ContextMenu {
        pane: app.proj.active,
        line,
        col: 0,
        x: 0.0,
        y: 0.0,
        insight: None,
        served: true,
        by_pointer: true,
    });
}

/// What the app sent the server's agent: the question, and the tools it
/// was offered.
fn asked(
    rx: &mut tokio::sync::mpsc::Receiver<clew_protocol::ClientMessage>,
) -> Option<(u64, String, usize)> {
    std::iter::from_fn(|| rx.try_recv().ok()).find_map(|m| match m.request {
        clew_protocol::Request::AgentAsk {
            stream,
            question,
            tools,
            ..
        } => Some((stream, question, tools.len())),
        _ => None,
    })
}

/// A trace over `src/lib.rs`'s `origin` (lines 3–5), with one step on a
/// file the project does not have.
const TRACE: &str = r#"{"returns": "Point { x: 0.0 }", "output": [],
 "assumptions": ["nothing else"],
 "steps": [
  {"file": "src/lib.rs", "line": 3, "function": "origin", "depth": 0,
   "note": "origin is called", "vars": {}},
  {"file": "src/gone.rs", "line": 1, "depth": 0},
  {"file": "src/lib.rs", "line": 4, "function": "origin", "depth": 0,
   "note": "builds the point", "vars": {"x": "0.0"}}
 ]}"#;

/// The menu's Fake Run… opens the sheet on the innermost function there,
/// else the type: a function with no parameters asks only for context; a
/// struct for how it is made and what is called on it.
#[test]
fn the_sheet_opens_on_the_definition_asked_for() {
    let mut app = fake_app("fake-sheet");
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let sheet = app.proj.fake.sheet.as_ref().expect("the sheet is open");
    assert_eq!(sheet.target.name, "origin");
    assert_eq!(sheet.target.rel, "src/lib.rs");
    assert_eq!(sheet.target.line, 3);
    assert!(
        sheet
            .target
            .source
            .starts_with("pub fn origin() -> Point {")
    );
    assert_eq!(sheet.fields, [Field::Context]);
    assert!(app.proj.context_menu.is_none(), "the menu closed");

    let _ = app.update(Message::Fake(FakeMsg::CloseSheet));
    menu_at(&mut app, 0);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let sheet = app.proj.fake.sheet.as_ref().unwrap();
    assert_eq!(sheet.target.name, "Point");
    assert_eq!(
        sheet.fields,
        [Field::Construct, Field::Calls, Field::Context]
    );

    // Between definitions: said, and no sheet.
    let _ = app.update(Message::Fake(FakeMsg::CloseSheet));
    menu_at(&mut app, 1);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    assert!(app.proj.fake.sheet.is_none());
    assert!(app.status.contains("fake-run"), "{}", app.status);
}

/// Run sends an agent turn — the target's numbered source and the inputs
/// in its question, no tool of the window's offered — and its answer,
/// once the turn ends, is a trace: the remark before a tool call left
/// out, a step on no file of the project dropped and counted, the first
/// line opened and lit.
#[test]
fn a_run_is_an_agent_turn_whose_answer_is_stepped_through() {
    let mut app = fake_app("fake-turn");
    let mut rx = attach_server(&mut app);
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let _ = app.update(Message::Fake(FakeMsg::Field(
        0,
        "the clock reads noon".into(),
    )));
    let _ = app.update(Message::Fake(FakeMsg::Run));
    assert!(app.proj.fake.sheet.is_none());
    let (stream, question, tools) = asked(&mut rx).expect("an agent turn was asked");
    assert_eq!(tools, 0, "no tool of the window's");
    assert!(
        question.contains("    3| pub fn origin() -> Point {"),
        "{question}"
    );
    assert!(question.contains("the clock reads noon"), "{question}");
    assert!(question.contains("read the code it calls"), "{question}");
    assert_eq!(
        app.proj.fake.working.as_ref().and_then(|w| w.stream),
        Some(stream)
    );
    assert!(app.show_bottom, "the panel shows the run being worked out");
    assert!(app.proj.link.agent_streams.contains_key(&stream));
    assert!(app.proj.link.agent_stream.is_none(), "Ask is left free");

    let stamp = app.stamp();
    for msg in [
        FakeMsg::Read {
            stamp: stamp.clone(),
            stream,
            title: "Read src/lib.rs".into(),
        },
        FakeMsg::Draft {
            stamp: stamp.clone(),
            stream,
            text: "Let me read it first.".into(),
        },
        FakeMsg::DraftEnd {
            stamp: stamp.clone(),
            stream,
            kept: false,
        },
        FakeMsg::Delta {
            stamp: stamp.clone(),
            stream,
            text: TRACE.into(),
        },
    ] {
        let _ = app.update(Message::Fake(msg));
    }
    assert_eq!(
        app.proj.fake.working.as_ref().unwrap().reading,
        ["Read src/lib.rs"]
    );
    let _ = app.update(Message::Fake(FakeMsg::TurnEnded {
        stamp,
        stream,
        outcome: clew_protocol::StreamOutcome::Done,
    }));
    assert!(app.proj.fake.working.is_none());
    assert!(!app.proj.link.agent_streams.contains_key(&stream));
    let sim = app.proj.fake.sim.as_ref().expect("a trace");
    assert_eq!(sim.trace.steps.len(), 2);
    assert_eq!(sim.trace.dropped, 1);
    assert_eq!(sim.trace.returns, "Point { x: 0.0 }");
    assert!(app.status.contains("left out"), "{}", app.status);
    let abs = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    assert_eq!(app.fake_line(&abs), Some(3), "its first line lit");
}

/// What the server is sent, held so the link stays up.
type Sent = tokio::sync::mpsc::Receiver<clew_protocol::ClientMessage>;

/// A trace, made from [`TRACE`] as the turn would bring it.
fn traced(tag: &str) -> (App, Sent) {
    let mut app = fake_app(tag);
    let mut rx = attach_server(&mut app);
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let _ = app.update(Message::Fake(FakeMsg::Run));
    let (stream, ..) = asked(&mut rx).unwrap();
    let stamp = app.stamp();
    let _ = app.update(Message::Fake(FakeMsg::Delta {
        stamp: stamp.clone(),
        stream,
        text: TRACE.into(),
    }));
    let _ = app.update(Message::Fake(FakeMsg::TurnEnded {
        stamp,
        stream,
        outcome: clew_protocol::StreamOutcome::Done,
    }));
    assert!(app.proj.fake.sim.is_some());
    (app, rx)
}

fn at(app: &App) -> (usize, bool) {
    let sim = app.proj.fake.sim.as_ref().unwrap();
    (sim.at, sim.ended)
}

/// The step commands move as a debugger's: over, back, on to a
/// breakpoint, past the end (nothing lit, what it returns shown), back
/// from the end, and to the start again; a stack frame goes to its line.
#[test]
fn the_trace_steps_as_a_debugger_does() {
    let (mut app, _rx) = traced("fake-steps");
    let abs = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    let step = |app: &mut App, cmd| {
        let _ = app.update(Message::Fake(FakeMsg::Step(cmd)));
    };
    step(&mut app, SimCmd::Over);
    assert_eq!(at(&app), (1, false));
    assert_eq!(app.fake_line(&abs), Some(4));
    step(&mut app, SimCmd::Back);
    assert_eq!(at(&app), (0, false));
    step(&mut app, SimCmd::Back);
    assert_eq!(at(&app), (0, false), "nothing before the first line");

    // On to a breakpoint.
    app.debug
        .breakpoints
        .entry(abs.clone())
        .or_default()
        .insert(4, Default::default());
    step(&mut app, SimCmd::Continue);
    assert_eq!(at(&app), (1, false), "stopped at the breakpoint");
    // None after it: to the end.
    step(&mut app, SimCmd::Continue);
    assert_eq!(at(&app), (1, true));
    assert_eq!(app.fake_line(&abs), None, "finished: nothing lit");
    step(&mut app, SimCmd::Over);
    assert_eq!(at(&app), (1, true), "nothing past the end");
    step(&mut app, SimCmd::Back);
    assert_eq!(at(&app), (1, false), "back to the last line");
    step(&mut app, SimCmd::Restart);
    assert_eq!(at(&app), (0, false));
    let _ = app.update(Message::Fake(FakeMsg::Goto(1)));
    assert_eq!(at(&app), (1, false));
    let _ = app.update(Message::Fake(FakeMsg::Goto(9)));
    assert_eq!(at(&app), (1, false), "no such step");

    let _ = app.update(Message::Fake(FakeMsg::Close));
    assert!(!app.proj.fake.shown());
    assert_eq!(app.fake_line(&abs), None);
}

/// With no agent turn to be had (no server), the run is one call; only
/// the newest call's answer is taken.
#[test]
fn without_a_server_the_run_is_one_call() {
    let mut app = fake_app("fake-one-call");
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let _ = app.update(Message::Fake(FakeMsg::Run));
    let seq = {
        let w = app.proj.fake.working.as_ref().expect("working");
        assert_eq!(w.stream, None);
        w.seq
    };
    let _ = app.update(Message::Fake(FakeMsg::Answered {
        stamp: app.stamp(),
        seq: seq + 7,
        result: Ok(TRACE.into()),
    }));
    assert!(app.proj.fake.sim.is_none(), "another run's answer");
    let _ = app.update(Message::Fake(FakeMsg::Answered {
        stamp: app.stamp(),
        seq,
        result: Ok(TRACE.into()),
    }));
    assert_eq!(app.proj.fake.sim.as_ref().unwrap().trace.steps.len(), 2);
}

/// A run that fails says why, and Try again opens the sheet with the
/// inputs it ran on; Stop cancels the turn on the server.
#[test]
fn a_failed_run_says_why_and_can_be_tried_again() {
    let mut app = fake_app("fake-failed");
    let mut rx = attach_server(&mut app);
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let _ = app.update(Message::Fake(FakeMsg::Field(0, "noon".into())));
    let _ = app.update(Message::Fake(FakeMsg::Run));
    let (stream, ..) = asked(&mut rx).unwrap();
    let _ = app.update(Message::Fake(FakeMsg::Delta {
        stamp: app.stamp(),
        stream,
        text: "I cannot do that.".into(),
    }));
    let _ = app.update(Message::Fake(FakeMsg::TurnEnded {
        stamp: app.stamp(),
        stream,
        outcome: clew_protocol::StreamOutcome::Done,
    }));
    assert!(app.proj.fake.sim.is_none());
    assert!(
        app.proj
            .fake
            .failed
            .as_deref()
            .is_some_and(|w| w.contains("no JSON")),
        "{:?}",
        app.proj.fake.failed
    );
    assert!(app.proj.fake.shown(), "the panel says why");
    let _ = app.update(Message::Fake(FakeMsg::EditInputs));
    assert_eq!(app.proj.fake.sheet.as_ref().unwrap().values, ["noon"]);

    // Run again, then Stop: the turn is cancelled.
    let _ = app.update(Message::Fake(FakeMsg::Run));
    let (stream, ..) = asked(&mut rx).unwrap();
    let _ = app.update(Message::Fake(FakeMsg::Stop));
    assert!(app.proj.fake.working.is_none());
    assert!(!app.proj.link.agent_streams.contains_key(&stream));
    let cancelled = std::iter::from_fn(|| rx.try_recv().ok())
        .any(|m| matches!(m.request, clew_protocol::Request::Cancel { id } if id == stream));
    assert!(cancelled, "the turn was cancelled on the server");
    // Its late end changes nothing.
    let _ = app.update(Message::Fake(FakeMsg::TurnEnded {
        stamp: app.stamp(),
        stream,
        outcome: clew_protocol::StreamOutcome::Done,
    }));
    assert!(app.proj.fake.sim.is_none() && app.proj.fake.failed.is_none());
}

/// Suggested inputs fill the fields left empty, not what was typed; a
/// suggestion asked before the newest is dropped.
#[test]
fn suggested_inputs_fill_only_the_empty_fields() {
    let mut app = fake_app("fake-suggest");
    menu_at(&mut app, 0);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug: true }));
    let _ = app.update(Message::Fake(FakeMsg::Field(0, "Point { x: 2.0 }".into())));
    let _ = app.update(Message::Fake(FakeMsg::Suggest));
    let seq = app.proj.fake.sheet.as_ref().unwrap().suggesting.unwrap();
    let _ = app.update(Message::Fake(FakeMsg::Suggested {
        stamp: app.stamp(),
        seq: seq - 1,
        result: Ok(vec!["no".into(), "no".into(), "no".into()]),
    }));
    assert_eq!(app.proj.fake.sheet.as_ref().unwrap().values[1], "");
    let _ = app.update(Message::Fake(FakeMsg::Suggested {
        stamp: app.stamp(),
        seq,
        result: Ok(vec!["Point { x: 1.0 }".into(), "p.x".into(), String::new()]),
    }));
    let sheet = app.proj.fake.sheet.as_ref().unwrap();
    assert_eq!(sheet.values, ["Point { x: 2.0 }", "p.x", ""]);
    assert_eq!(sheet.suggesting, None);
}

/// Ask why sends the step — where it is, what the model said of it, its
/// values — to Ask as a question.
#[test]
fn ask_why_asks_about_the_step_shown() {
    let (mut app, _rx) = traced("fake-ask-why");
    let _ = app.update(Message::Fake(FakeMsg::Step(SimCmd::Over)));
    let _ = app.update(Message::Fake(FakeMsg::AskWhy));
    let turn = app.proj.ask_turns.last().expect("a question was asked");
    assert!(turn.question.contains("src/lib.rs:4"), "{}", turn.question);
    assert!(
        turn.question.contains("builds the point"),
        "{}",
        turn.question
    );
    assert!(turn.question.contains("x = 0.0"), "{}", turn.question);
}

/// The shortcut fake-runs the definition the caret is in.
#[test]
fn the_shortcut_runs_where_the_caret_is() {
    let mut app = fake_app("fake-caret");
    app.active_viewer_mut().unwrap().caret = Some((4, 0));
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::FakeRun,
    )));
    assert_eq!(
        app.proj.fake.sheet.as_ref().map(|s| s.target.name.as_str()),
        Some("origin")
    );
}

/// Inside a method, the method is what runs, not the class around it;
/// on the class's own lines, the class.
#[test]
fn the_innermost_function_wins_over_its_type() {
    let mut app = fake_app("fake-innermost");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    std::fs::write(
        root.join("src/cart.py"),
        "class Cart:\n    def __init__(self):\n        self.items = []\n\n    def add(self, sku, qty=1):\n        self.items.append((sku, qty))\n",
    )
    .unwrap();
    open_synchronously(&mut app, "src/cart.py", None);
    let pane = app.proj.active;
    let add = app.fake_target_at(pane, 6).expect("a target");
    assert_eq!(add.name, "add");
    assert_eq!(
        crate::fakerun::fields(&add),
        [
            Field::Param {
                name: "sku".into(),
                hint: String::new()
            },
            Field::Param {
                name: "qty".into(),
                hint: "1".into()
            },
            Field::Context
        ],
        "self is no input"
    );
    let cart = app.fake_target_at(pane, 1).expect("the class");
    assert_eq!(cart.name, "Cart");
    assert!(cart.is_type());
    assert_eq!(
        crate::fakerun::fields(&cart),
        [Field::Construct, Field::Calls, Field::Context]
    );
}

/// A trace, made from [`TRACE`] as the turn brings it, of a run asked for
/// as Fake Run (`debug` false) or Fake Debug.
fn traced_as(tag: &str, debug: bool) -> (App, Sent) {
    let mut app = fake_app(tag);
    let mut rx = attach_server(&mut app);
    menu_at(&mut app, 3);
    let _ = app.update(Message::Fake(FakeMsg::FromMenu { debug }));
    assert_eq!(app.proj.fake.sheet.as_ref().map(|s| s.debug), Some(debug));
    let _ = app.update(Message::Fake(FakeMsg::Run));
    assert_eq!(app.proj.fake.working.as_ref().map(|w| w.debug), Some(debug));
    let (stream, ..) = asked(&mut rx).unwrap();
    let stamp = app.stamp();
    let _ = app.update(Message::Fake(FakeMsg::Delta {
        stamp: stamp.clone(),
        stream,
        text: TRACE.into(),
    }));
    let _ = app.update(Message::Fake(FakeMsg::TurnEnded {
        stamp,
        stream,
        outcome: clew_protocol::StreamOutcome::Done,
    }));
    (app, rx)
}

/// Fake Run lands on the run's end — what it returns shown, no line lit —
/// and its steps are there to go back through; Fake Debug starts on its
/// first line, lit, as a debugger stops at a breakpoint.
#[test]
fn a_fake_run_lands_on_its_end_and_a_fake_debug_on_its_first_line() {
    let (mut app, _rx) = traced_as("fake-run-end", false);
    let abs = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    let sim = app.proj.fake.sim.as_ref().expect("a trace");
    assert!(!sim.debug);
    assert_eq!(at(&app), (1, true), "at the end");
    assert_eq!(app.fake_line(&abs), None, "nothing lit");
    assert!(app.status.contains("Fake run done"), "{}", app.status);
    let _ = app.update(Message::Fake(FakeMsg::Step(SimCmd::Restart)));
    assert_eq!(at(&app), (0, false), "stepped through from the start");
    assert_eq!(app.fake_line(&abs), Some(3));

    let (app, _rx) = traced_as("fake-debug-start", true);
    assert!(app.proj.fake.sim.as_ref().unwrap().debug);
    assert_eq!(at(&app), (0, false), "on its first line");
    assert_eq!(app.fake_line(&abs_of(&app)), Some(3), "lit");
    assert!(app.status.contains("Fake debug ready"), "{}", app.status);
}

fn abs_of(app: &App) -> std::path::PathBuf {
    app.proj.project.as_ref().unwrap().root.join("src/lib.rs")
}

/// From the strip's Run menu no caret was placed: Fake Run takes the
/// definition where the reader is — the one in view, else the first under
/// it — and says which in its sheet. A caret placed between definitions
/// is still said to be in none.
#[test]
fn fake_run_from_the_menu_takes_the_definition_in_view() {
    let mut app = fake_app("fake-in-view");
    {
        let v = app.active_viewer_mut().unwrap();
        v.caret = None;
        v.target_line = None;
        v.scroll_y = 0.0;
    }
    let _ = app.update(Message::Fake(FakeMsg::AtCaret { debug: true }));
    let sheet = app.proj.fake.sheet.as_ref().expect("a sheet");
    assert_eq!(sheet.target.name, "Point", "the definition at the top");
    assert!(sheet.debug);
    let _ = app.update(Message::Fake(FakeMsg::CloseSheet));

    // The view's top on the blank line under Point: the first one below.
    let line_height = app.line_height();
    app.active_viewer_mut().unwrap().scroll_y = line_height;
    let _ = app.update(Message::Fake(FakeMsg::AtCaret { debug: false }));
    let sheet = app.proj.fake.sheet.as_ref().expect("a sheet");
    assert_eq!(sheet.target.name, "origin", "the first one under it");
    assert!(!sheet.debug);
    let _ = app.update(Message::Fake(FakeMsg::CloseSheet));

    // A caret placed there is the reader's: no definition, said.
    app.active_viewer_mut().unwrap().caret = Some((1, 0));
    let _ = app.update(Message::Fake(FakeMsg::AtCaret { debug: false }));
    assert!(app.proj.fake.sheet.is_none());
    assert!(app.status.contains("caret"), "{}", app.status);
}
