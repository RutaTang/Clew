//! The window's own tools for clew's Ask agent (`app::windowtools`):
//! offered with every turn, run here — opening a file, going to a symbol,
//! listing and running the window's commands, the costly ones only once
//! the user says so.

use super::*;
use crate::app::integrations::ToolMap;
use crate::app::windowtools::{GO_TO_SYMBOL, LIST_COMMANDS, OPEN_FILE, RUN_COMMAND};
use serde_json::{Value, json};

/// An app with a project open, a server attached, and an agent turn
/// running: its stream, and what the app sends the server.
fn turn_app(
    tag: &str,
) -> (
    App,
    u64,
    tokio::sync::mpsc::Receiver<clew_protocol::ClientMessage>,
) {
    let mut app = scanned_app(tag);
    let mut rx = attach_server(&mut app);
    let _pump = app.start_agent_turn("show me".into(), false, Vec::new(), ToolMap::new());
    let stream = app.proj.link.agent_stream.expect("an agent turn is open");
    // What the turn was offered.
    let tools = std::iter::from_fn(|| rx.try_recv().ok())
        .find_map(|m| match m.request {
            clew_protocol::Request::AgentAsk { tools, .. } => Some(tools),
            _ => None,
        })
        .expect("the turn was asked");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, [OPEN_FILE, GO_TO_SYMBOL, LIST_COMMANDS, RUN_COMMAND]);
    for tool in &tools {
        assert_eq!(tool.parameters["type"], "object", "{}", tool.name);
        assert!(tool.description.starts_with("[The user's Clew window]"));
    }
    (app, stream, rx)
}

/// The turn calls `tool` with `args`, as the server relays it.
fn call(app: &mut App, stream: u64, call: u64, tool: &str, args: Value) -> Task<Message> {
    app.update(Message::Ask(AskMsg::AgentToolCalled {
        stamp: app.stamp(),
        stream,
        call,
        tool: tool.into(),
        args,
    }))
}

/// The tool results the app sent: `(call, content, is_error)`.
fn results(
    rx: &mut tokio::sync::mpsc::Receiver<clew_protocol::ClientMessage>,
) -> Vec<(u64, String, bool)> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|m| match m.request {
            clew_protocol::Request::AgentToolResult {
                call,
                content,
                is_error,
                ..
            } => Some((call, content, is_error)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_agent_opens_a_file_at_a_line() {
    let (mut app, stream, mut rx) = turn_app("wt-open");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _ = call(
        &mut app,
        stream,
        1,
        OPEN_FILE,
        json!({"path": "./src/lib.rs", "line": 3}),
    );
    land_active_load(&mut app, root.join("src/lib.rs"), Some(3));
    let v = app.active_viewer().expect("a file is open");
    assert_eq!(v.rel, "src/lib.rs");
    assert_eq!(v.target_line, Some(3));
    let sent = results(&mut rx);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, 1);
    assert!(!sent[0].2, "{}", sent[0].1);
    assert!(sent[0].1.contains("src/lib.rs:3"), "{}", sent[0].1);
    // A step chip says so, and points there.
    let step = app.proj.ask_turns.last().unwrap().steps.last().unwrap();
    assert_eq!(step.title, "Opened `src/lib.rs:3`");
    assert_eq!(step.refs, [("src/lib.rs".to_string(), Some(3))]);
    assert_eq!(step.state, StepState::Done);
}

/// Only the project's own files: a path out of it, or one it does not
/// list, opens nothing and says why.
#[test]
fn the_agent_opens_only_the_projects_files() {
    let (mut app, stream, mut rx) = turn_app("wt-open-outside");
    for (n, path) in ["../../etc/passwd", "/etc/passwd", "src/missing.rs", ""]
        .into_iter()
        .enumerate()
    {
        let _ = call(&mut app, stream, n as u64, OPEN_FILE, json!({"path": path}));
        let sent = results(&mut rx);
        assert_eq!(sent.len(), 1, "{path}");
        assert!(sent[0].2, "{path}: {}", sent[0].1);
    }
    assert!(app.active_viewer().is_none(), "nothing was opened");
}

#[test]
fn the_agent_goes_to_a_symbol_by_name() {
    let (mut app, stream, mut rx) = turn_app("wt-symbol");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    if app.fresh_symbol_index().is_empty() {
        let abs = root.join("src/lib.rs");
        app.proj.symbol_index_by_file.insert(
            abs.clone(),
            Arc::new(vec![crate::index::SymbolEntry {
                name: "origin".into(),
                kind: "function".into(),
                rel: "src/lib.rs".into(),
                abs,
                line: 3,
                is_test: false,
                entry: None,
            }]),
        );
        app.proj.symbol_index_stale = true;
    }
    let _ = call(&mut app, stream, 1, GO_TO_SYMBOL, json!({"name": "origin"}));
    land_active_load(&mut app, root.join("src/lib.rs"), Some(3));
    let sent = results(&mut rx);
    assert!(!sent[0].2, "{}", sent[0].1);
    assert!(sent[0].1.contains("src/lib.rs:3"), "{}", sent[0].1);
    assert_eq!(
        app.active_viewer().map(|v| v.rel.as_str()),
        Some("src/lib.rs")
    );
    // Regardless of case, when no name matches exactly.
    let _ = call(&mut app, stream, 2, GO_TO_SYMBOL, json!({"name": "ORIGIN"}));
    assert!(!results(&mut rx)[0].2);
    // None by that name: said, with what to do instead.
    let _ = call(
        &mut app,
        stream,
        3,
        GO_TO_SYMBOL,
        json!({"name": "nowhere"}),
    );
    let sent = results(&mut rx);
    assert!(sent[0].2);
    assert!(sent[0].1.contains("search"), "{}", sent[0].1);
}

#[test]
fn the_agent_lists_and_runs_the_windows_commands() {
    let (mut app, stream, mut rx) = turn_app("wt-commands");
    let _ = call(&mut app, stream, 1, LIST_COMMANDS, json!({}));
    let listed = results(&mut rx).remove(0).1;
    let keys = crate::keymap::local_keys;
    assert!(
        listed.contains(&format!("- Call Graph ({})", keys("⇧⌘C"))),
        "{listed}"
    );
    assert!(listed.contains(&format!(
        "- Explain All ({}) — asks the user first",
        keys("⇧⌘E")
    )));
    assert!(
        listed.contains(&format!("- Overview ({})\n", keys("⇧⌘O"))),
        "{listed}"
    );
    // One with no key: its title alone.
    assert!(listed.contains("- Window: Minimize\n"), "{listed}");
    // Something that only shows: run at once.
    let _ = call(
        &mut app,
        stream,
        2,
        RUN_COMMAND,
        json!({"title": "Call Graph"}),
    );
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls));
    let sent = results(&mut rx);
    assert_eq!(sent, [(2, "Ran “Call Graph”.".to_string(), false)]);
    let _ = app.update(Message::Graph(GraphMsg::CloseOverlay));
    // Its words rather than its title still find it.
    let _ = call(
        &mut app,
        stream,
        3,
        RUN_COMMAND,
        json!({"title": "show the stats"}),
    );
    assert!(app.proj.stats.showing);
    assert!(!results(&mut rx)[0].2);
    // No such command: said, and how to find one.
    let _ = call(
        &mut app,
        stream,
        4,
        RUN_COMMAND,
        json!({"title": "Make Coffee"}),
    );
    let sent = results(&mut rx);
    assert!(sent[0].2);
    assert!(sent[0].1.contains(LIST_COMMANDS), "{}", sent[0].1);
}

/// A setting that turns the microphone on and keeps it on, sends what it
/// hears, or sends the project's code somewhere else is the reader's to
/// change: the agent asking for it waits for their yes, on a card that
/// says why. One that only changes how the window looks runs.
#[test]
fn the_agent_asks_before_a_setting_that_listens_or_sends() {
    let (mut app, stream, mut rx) = turn_app("wt-private");
    app.asking.delay = std::time::Duration::ZERO;
    app.voice.prefs.wake = false;
    let listening = app.voice.mic_on;
    for (call_id, title) in [
        (1, "Talk"),
        (2, "Send spoken questions to Ask right away: On"),
        (3, "Embeddings: A service"),
        (4, "Start listening when you say “Hey, Clew”: On"),
    ] {
        let _ = call(
            &mut app,
            stream,
            call_id,
            RUN_COMMAND,
            json!({ "title": title }),
        );
        assert!(results(&mut rx).is_empty(), "{title} ran without asking");
        let turn = app.proj.ask_turns.last().unwrap();
        let card = turn
            .extras
            .approvals
            .iter()
            .find(|a| a.key == ApprovalKey::Window { call: call_id })
            .unwrap_or_else(|| panic!("no card for {title}"));
        assert!(card.title.contains(title), "{}", card.title);
    }
    assert_eq!(app.voice.mic_on, listening, "it changed unasked");
    assert!(!app.voice.prefs.wake, "it changed unasked");
    // How the window looks is the agent's to change.
    let _ = call(
        &mut app,
        stream,
        5,
        RUN_COMMAND,
        json!({"title": "Theme: Dark"}),
    );
    assert_eq!(
        results(&mut rx),
        [(5, "Ran “Theme: Dark”.".to_string(), false)]
    );
}

/// A command that costs something waits for the user's answer on a card;
/// a no is told to the turn; a turn that ended is answered nothing more.
#[test]
fn a_costly_command_waits_for_the_users_yes() {
    let (mut app, stream, mut rx) = turn_app("wt-costly");
    app.asking.delay = std::time::Duration::ZERO;
    app.ask_view = AskView::Hidden;
    let _ = call(
        &mut app,
        stream,
        1,
        RUN_COMMAND,
        json!({"title": "Explain All"}),
    );
    assert!(results(&mut rx).is_empty(), "nothing yet");
    assert!(!app.settings.open, "nothing ran");
    assert_eq!(app.ask_view, AskView::Compact, "the card is shown");
    let turn = app.proj.ask_turns.last().unwrap();
    assert_eq!(turn.extras.approvals.len(), 1);
    assert_eq!(
        turn.extras.approvals[0].key,
        ApprovalKey::Window { call: 1 }
    );
    assert!(turn.extras.approvals[0].title.contains("Explain All"));
    assert_eq!(turn.steps.last().unwrap().state, StepState::Waiting);
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Window { call: 1 },
        option: "once".into(),
    }));
    assert!(
        app.settings.open,
        "it ran: with no key, Settings asks for one"
    );
    assert_eq!(
        results(&mut rx),
        [(1, "Ran “Explain All”.".to_string(), false)]
    );
    assert_eq!(
        app.proj
            .ask_turns
            .last()
            .unwrap()
            .steps
            .last()
            .unwrap()
            .state,
        StepState::Done
    );
    let _ = app.update(Message::Settings(SettingsMsg::Close));
    // No.
    let _ = call(
        &mut app,
        stream,
        2,
        RUN_COMMAND,
        json!({"title": "Explain All"}),
    );
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Window { call: 2 },
        option: "deny".into(),
    }));
    assert!(!app.settings.open);
    let sent = results(&mut rx);
    assert!(sent[0].2 && sent[0].1.contains("declined"), "{sent:?}");
    // Asked, then the turn ended: its card goes, and nothing is run.
    let _ = call(
        &mut app,
        stream,
        3,
        RUN_COMMAND,
        json!({"title": "Explain All"}),
    );
    app.drop_turn_tools(stream);
    assert!(app.proj.link.window_pending.is_empty());
    let _ = app.update(Message::Ask(AskMsg::Approve {
        stream,
        key: ApprovalKey::Window { call: 3 },
        option: "once".into(),
    }));
    assert!(!app.settings.open);
}

/// A call of a turn that is no longer running is not run.
#[test]
fn a_stopped_turns_calls_are_not_run() {
    let (mut app, stream, mut rx) = turn_app("wt-stopped");
    let _ = app.update(Message::Ask(AskMsg::Stop));
    let _ = call(
        &mut app,
        stream,
        1,
        RUN_COMMAND,
        json!({"title": "Call Graph"}),
    );
    assert_eq!(app.proj.overlay, None);
    let sent = results(&mut rx);
    assert!(sent.iter().any(|(c, _, err)| *c == 1 && *err), "{sent:?}");
}
