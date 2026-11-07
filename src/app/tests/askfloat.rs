//! The floating Ask window (`ui::askfloat`, `AskView`): hidden to its pill,
//! put away, a card in the corner, or large and centred — opened, resized,
//! hidden from the keyboard (Esc one step at a time) and by what happens in
//! the window.

use super::*;

#[test]
fn ask_opens_as_a_card_grows_large_and_steps_back_with_esc() {
    let mut app = scanned_app("askfloat-steps");
    assert_eq!(app.ask_view, AskView::Hidden, "its pill, at first");
    // ⇧⌘A: a card, its input taking the keyboard.
    let _ = app.update(key_press(
        keyboard::Key::Character("a".into()),
        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
    ));
    assert_eq!(app.ask_view, AskView::Compact);
    assert!(!app.code_focused, "the Ask input has the keyboard");
    let _ = app.update(Message::Ask(AskMsg::Expand));
    assert_eq!(app.ask_view, AskView::Expanded);
    // Esc: large to a card, a card hidden to its pill — one step a press.
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Hidden);
    assert!(app.code_focused, "the keyboard back with the code");
    // ⇧⌘A from large hides it too; Shrink only makes it a card.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let _ = app.update(Message::Ask(AskMsg::Shrink));
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    assert_eq!(app.ask_view, AskView::Hidden);
    let _ = app.update(Message::Ask(AskMsg::Open));
    let _ = app.update(Message::Ask(AskMsg::Hide));
    assert_eq!(app.ask_view, AskView::Hidden);
    // Open, when open, leaves it as it is.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let _ = app.update(Message::Ask(AskMsg::Open));
    assert_eq!(app.ask_view, AskView::Expanded);
    let _ = app.update(Message::Ask(AskMsg::Close));
    assert_eq!(app.ask_view, AskView::Closed);
}

/// Esc puts away the nearest thing first: the find bar, then the
/// selection, and only then the Ask card. The large window, over them all,
/// goes first of all.
#[test]
fn esc_puts_away_the_nearest_thing_first() {
    let mut app = scanned_app("askfloat-esc-order");
    open_synchronously(&mut app, "src/lib.rs", Some(3));
    // The language server's consent prompt the open brought up is an
    // overlay: Esc's first.
    if crate::ui::active_overlay(&app).is_some() {
        let _ = app.update(esc_press());
    }
    assert!(crate::ui::active_overlay(&app).is_none());
    let _ = app.update(Message::Ask(AskMsg::Open));
    app.proj.find.open = true;
    let _ = app.update(esc_press());
    assert!(!app.proj.find.open);
    assert_eq!(app.ask_view, AskView::Compact);
    assert!(app.active_viewer().unwrap().target_line.is_some());
    let _ = app.update(esc_press());
    assert!(app.active_viewer().unwrap().target_line.is_none());
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Hidden);
    // Large: before the find bar.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    app.proj.find.open = true;
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Compact);
    assert!(app.proj.find.open);
}

/// A citation followed from the large window — any jump to code — makes it
/// a card, out of the way of the code it opened.
#[test]
fn opening_code_makes_the_large_window_a_card() {
    let mut app = scanned_app("askfloat-citation");
    let _ = app.update(Message::Ask(AskMsg::Expand));
    open_synchronously(&mut app, "src/lib.rs", Some(3));
    assert_eq!(app.ask_view, AskView::Compact);
    // A card stays a card.
    let _ = app.update(Message::Editor(EditorMsg::OpenRel {
        rel: "src/lib.rs".into(),
        line: Some(1),
    }));
    assert_eq!(app.ask_view, AskView::Compact);
}

/// Asking opens the card; the debugger opens its own panel, and Ask is
/// not in it.
#[test]
fn asking_opens_the_card_and_debug_keeps_the_bottom_panel() {
    let mut app = scanned_app("askfloat-debug");
    let mut rx = attach_server(&mut app);
    let _ = app.start_agent_turn(
        "why?".into(),
        false,
        Vec::new(),
        crate::app::integrations::ToolMap::new(),
    );
    assert!(rx.try_recv().is_ok());
    assert_eq!(app.ask_view, AskView::Compact);
    assert!(!app.show_bottom, "the bottom panel is the debugger's");
    let _ = app.update(Message::Window(WindowMsg::ShowDebugPanel));
    assert!(app.show_bottom);
    assert_eq!(app.ask_view, AskView::Compact, "both, side by side");
    let titles: Vec<String> = app
        .palette_commands()
        .into_iter()
        .map(|c| c.title)
        .collect();
    assert!(
        titles.iter().any(|t| t == "View: Hide Debug Panel"),
        "{titles:?}"
    );
    let _ = app.update(Message::Window(WindowMsg::CollapseBottom));
    let titles: Vec<String> = app
        .palette_commands()
        .into_iter()
        .map(|c| c.title)
        .collect();
    assert!(titles.iter().any(|t| t == "View: Show Debug Panel"));
}

/// The card stands in the window's bottom-right corner wherever the caret
/// is — left of the tool strip, over the status bar — and on the debugger's
/// panel when it shows; the large window is centred, wider than the card.
#[test]
fn the_card_stands_in_the_corner() {
    let mut app = scanned_app("askfloat-dodge");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let body: String = (0..400).map(|i| format!("// line {i}\n")).collect();
    std::fs::write(root.join("src/long.rs"), body).unwrap();
    open_synchronously(&mut app, "src/long.rs", None);
    let _ = app.update(Message::Ask(AskMsg::Open));
    let lh = app.line_height();
    let v = app.active_viewer_mut().unwrap();
    v.viewport_h = 700.0;
    v.scroll_y = 0.0;
    let card = crate::ui::ask_place(&app).unwrap();
    // Near the top of the view, near its foot, scrolled away: the card
    // stays where it is, in the corner.
    let rows = (700.0 / lh) as usize;
    for (caret, scroll) in [(2, 0.0), (rows - 2, 0.0), (2, 300.0 * lh)] {
        let v = app.active_viewer_mut().unwrap();
        v.caret = Some((caret, 0));
        v.scroll_y = scroll;
        assert_eq!(crate::ui::ask_place(&app), Some(card), "caret {caret}");
    }
    let middle = |left: f32, width: f32| left + width / 2.0;
    let right = app.window_width - crate::ui::STRIP_W - 12.0;
    assert!(
        (card.left + card.width - right).abs() < 1.0,
        "{card:?} by the tool strip"
    );
    let corner = crate::ui::ask_corner_rect(&app);
    assert!(
        (corner.y + corner.height
            - (app.window_height - crate::ui::STATUSBAR_H - crate::ui::ASK_MARGIN))
            .abs()
            < 1.0,
        "{corner:?} over the status bar"
    );
    // The debugger's panel up: the card stands on it.
    app.show_bottom = true;
    app.bottom_height = 200.0;
    let raised = crate::ui::ask_corner_rect(&app);
    assert!(
        raised.y + raised.height < corner.y + corner.height - 199.0,
        "{raised:?}"
    );
    app.show_bottom = false;
    // Large: centred, wider than the card.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let place = crate::ui::ask_place(&app).unwrap();
    assert!(place.width > crate::ui::ASK_CARD_W, "{place:?}");
    assert!((middle(place.left, place.width) - app.window_width / 2.0).abs() < 1.0);
}

/// The tour's Ask step shows the card; the steps after it put it away.
#[test]
fn the_tour_shows_the_ask_card() {
    let mut app = scanned_app("askfloat-tour");
    let _ = app.update(Message::Tutorial(TutorialMsg::Start));
    let ask_step = crate::app::tutorial::steps(&app)
        .iter()
        .position(|s| s.title == "Ask Clew, or tell it what to do")
        .expect("an Ask step");
    while app.tutorial.is_some_and(|s| s < ask_step) {
        let _ = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    }
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    assert_eq!(app.ask_view, AskView::Hidden);
    let debug_step = crate::app::tutorial::steps(&app)
        .iter()
        .position(|s| s.title == "Debug")
        .expect("a Debug step");
    while app.tutorial.is_some_and(|s| s < debug_step) {
        let _ = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    }
    assert_eq!(app.ask_view, AskView::Hidden);
    assert!(app.show_bottom, "the Debug step shows the debugger's panel");
}

/// An answered question as the stream that carried it ends: the turn on
/// the card, finished.
fn answered(app: &mut App, question: &str, answer: &str) {
    let stream = app
        .next_req_id
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream,
        question: question.into(),
        answer_md: answer.into(),
        streaming: true,
        ..Default::default()
    });
    app.proj.asking = true;
    let _ = app.on_ask_stream_ended(stream, clew_protocol::StreamOutcome::Done);
}

/// Ask's sessions: an answer keeps the conversation as a session, named
/// for its first question; a new session is an empty card, the last one in
/// the menu; going back to one puts its questions and answers on the card;
/// one forgotten, or cleared, goes from the menu; and they are kept in the
/// project's own data, there when the project is opened again.
#[test]
fn ask_keeps_its_sessions() {
    let mut app = scanned_app("ask-sessions");
    let derived = app.proj.derived_dir.clone().expect("a derived dir");
    let _ = std::fs::remove_file(derived.join("ask-sessions.json"));
    app.proj.ask_sessions.clear();
    assert_eq!(app.ask_session_title(), "New session");
    answered(&mut app, "Where is the origin?", "In `origin.rs`.");
    assert_eq!(app.proj.ask_sessions.len(), 1);
    let first = app.proj.ask_session.expect("a session once answered");
    assert_eq!(app.ask_session_title(), "Where is the origin?");
    answered(&mut app, "And its tests?", "In `tests/`.");
    assert_eq!(
        app.proj.ask_sessions[0].turns.len(),
        2,
        "one session, two turns"
    );

    // A new session: an empty card, the last one in the menu.
    let _ = app.update(Message::Ask(AskMsg::NewSession));
    assert!(app.proj.ask_turns.is_empty());
    assert_eq!(app.proj.ask_session, None);
    assert_eq!(app.ask_view, AskView::Compact, "the card, open on it");
    assert_eq!(app.ask_session_title(), "New session");
    answered(&mut app, "How is a host connected?", "Over SSH.");
    assert_eq!(app.proj.ask_sessions.len(), 2);
    assert_eq!(
        app.proj.ask_sessions[0].title, "How is a host connected?",
        "newest first"
    );

    // Back to the first: its questions and answers on the card.
    let _ = app.update(Message::Ask(AskMsg::SessionsMenu(true)));
    assert!(app.ask_sessions_menu);
    let _ = app.update(Message::Ask(AskMsg::SwitchSession(first)));
    assert!(!app.ask_sessions_menu, "the menu closes on a pick");
    assert_eq!(app.proj.ask_session, Some(first));
    let questions: Vec<&str> = app
        .proj
        .ask_turns
        .iter()
        .map(|t| t.question.as_str())
        .collect();
    assert_eq!(questions, ["Where is the origin?", "And its tests?"]);
    assert_eq!(app.proj.ask_turns[0].answer_md, "In `origin.rs`.");
    assert!(app.proj.ask_turns.iter().all(|t| !t.streaming));

    // The menu's search: what is found, and Enter goes back to the first.
    let _ = app.update(Message::Ask(AskMsg::SessionsMenu(true)));
    let _ = app.update(Message::Ask(AskMsg::SessionsQuery("ssh".into())));
    let found: Vec<String> = app
        .found_ask_sessions()
        .iter()
        .map(|(s, _)| s.title.clone())
        .collect();
    assert_eq!(found, ["How is a host connected?"], "by its answer");
    let host = app
        .proj
        .ask_sessions
        .iter()
        .find(|s| s.title == "How is a host connected?")
        .map(|s| s.id);
    let before: Vec<u64> = app.proj.ask_sessions.iter().map(|s| s.id).collect();
    let _ = app.update(Message::Ask(AskMsg::OpenFirstFound));
    assert_eq!(app.proj.ask_session, host);
    assert_eq!(app.proj.ask_turns[0].question, "How is a host connected?");
    assert_eq!(
        app.proj
            .ask_sessions
            .iter()
            .map(|s| s.id)
            .collect::<Vec<_>>(),
        before,
        "only looked at, the one left keeps its place"
    );
    assert!(!app.ask_sessions_menu && app.ask_sessions_query.is_empty());
    let _ = app.update(Message::Ask(AskMsg::SessionsMenu(true)));
    let _ = app.update(Message::Ask(AskMsg::SessionsQuery(
        "nothing like it".into(),
    )));
    assert!(app.found_ask_sessions().is_empty());
    let _ = app.update(Message::Ask(AskMsg::OpenFirstFound));
    assert!(app.ask_sessions_menu, "nothing found: nothing to go to");
    assert!(app.step_ask_down());
    assert!(
        app.ask_sessions_query.is_empty(),
        "closed, the search is cleared"
    );
    let _ = app.update(Message::Ask(AskMsg::SwitchSession(first)));

    // Kept in the project's own data: opened again, they are there.
    let mut again = scanned_app("ask-sessions");
    assert_eq!(again.proj.derived_dir, Some(derived.clone()));
    assert_eq!(again.proj.ask_sessions.len(), 2);
    assert_eq!(again.proj.ask_session, None, "the card starts on a new one");

    // Forgotten from the menu; the one on screen cleared goes too.
    let other = again.proj.ask_sessions[0].id;
    let _ = again.update(Message::Ask(AskMsg::DeleteSession(other)));
    assert_eq!(again.proj.ask_sessions.len(), 1);
    let _ = again.update(Message::Ask(AskMsg::SwitchSession(first)));
    let _ = again.update(Message::Ask(AskMsg::Clear));
    assert!(again.proj.ask_sessions.is_empty());
    assert!(again.proj.ask_turns.is_empty());
    assert!(
        crate::app::ask_sessions::load(&derived).is_empty(),
        "forgotten on disk too"
    );

    // Esc closes the menu before it hides the card.
    again.ask_view = AskView::Compact;
    again.ask_sessions_menu = true;
    assert!(again.step_ask_down());
    assert!(!again.ask_sessions_menu);
    assert_eq!(again.ask_view, AskView::Compact);
}

/// A project's sessions, read afresh from its data, their names sorted.
fn kept_titles(derived: &std::path::Path) -> Vec<String> {
    let mut titles: Vec<String> = crate::app::ask_sessions::load(derived)
        .into_iter()
        .map(|s| s.title)
        .collect();
    titles.sort();
    titles
}

/// Two windows on one project: each keeps its session in the file as it
/// stands, so neither writes over what the other kept since it opened the
/// project, nor names its session with the other's id; one forgotten in
/// one window leaves the other's.
#[test]
fn two_windows_keep_each_others_sessions() {
    let mut one = scanned_app("ask-sessions-two-windows");
    let derived = one.proj.derived_dir.clone().expect("a derived dir");
    let _ = std::fs::remove_file(derived.join("ask-sessions.json"));
    one.proj.ask_sessions.clear();
    let mut two = scanned_app("ask-sessions-two-windows");
    assert_eq!(two.proj.derived_dir, Some(derived.clone()));
    assert!(two.proj.ask_sessions.is_empty());

    answered(&mut one, "Where is the origin?", "In `origin.rs`.");
    answered(&mut two, "How is a host connected?", "Over SSH.");
    assert_eq!(
        kept_titles(&derived),
        ["How is a host connected?", "Where is the origin?"],
        "the second window's session did not write over the first's"
    );
    assert_ne!(one.proj.ask_session, two.proj.ask_session, "two ids");
    assert_eq!(
        two.proj.ask_sessions,
        crate::app::ask_sessions::load(&derived),
        "the window's menu is what is kept"
    );

    // The first window forgets its session: the second's stays.
    let first = one.proj.ask_session.expect("kept");
    let _ = one.update(Message::Ask(AskMsg::DeleteSession(first)));
    assert_eq!(kept_titles(&derived), ["How is a host connected?"]);
    // And the second, asked in again, keeps the first's forgotten.
    answered(&mut two, "And its keys?", "The user's own.");
    assert_eq!(kept_titles(&derived), ["How is a host connected?"]);
    assert_eq!(
        crate::app::ask_sessions::load(&derived)[0].turns.len(),
        2,
        "one session, two turns"
    );
}

/// Leaving a session while clew's agent answers on the server keeps the
/// question and what was written of its answer — for a session's first
/// question, the session itself. The server's end of the stopped turn,
/// coming later, changes nothing.
#[test]
fn leaving_a_session_keeps_the_question_the_server_is_answering() {
    let mut app = scanned_app("ask-sessions-leave-agent");
    let derived = app.proj.derived_dir.clone().expect("a derived dir");
    let _ = std::fs::remove_file(derived.join("ask-sessions.json"));
    app.proj.ask_sessions.clear();
    // A turn of clew's agent, answering on the server.
    let stream = app.mint_request_id();
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream,
        question: "Where is the origin?".into(),
        answer_md: "It is set in".into(),
        streaming: true,
        ..Default::default()
    });
    app.proj.link.agent_stream = Some(stream);

    let _ = app.update(Message::Ask(AskMsg::NewSession));
    assert!(app.proj.ask_turns.is_empty(), "a new session's empty card");
    assert!(app.proj.link.agent_stream.is_none(), "the turn stopped");
    let kept = crate::app::ask_sessions::load(&derived);
    assert_eq!(kept.len(), 1, "the session of its first question: {kept:?}");
    assert_eq!(kept[0].turns[0].question, "Where is the origin?");
    assert!(
        kept[0].turns[0].answer.starts_with("It is set in"),
        "{:?}",
        kept[0].turns[0].answer
    );
    assert_eq!(app.proj.ask_sessions, kept, "in the menu");

    let stamp = app.stamp();
    let _ = app.update(Message::Ask(AskMsg::AgentTurnEnded {
        stamp,
        stream,
        outcome: clew_protocol::StreamOutcome::Stopped,
    }));
    assert_eq!(crate::app::ask_sessions::load(&derived), kept);
    assert!(app.proj.ask_turns.is_empty());
}

/// Going back to the oldest session of a full menu while an answer streams
/// on this machine: the conversation left is kept, and the oldest of the
/// others makes room for it — not the one gone back to, which is on screen.
#[test]
fn going_back_to_the_oldest_session_keeps_it() {
    use crate::app::ask_sessions::{AskSession, MAX_SESSIONS, SavedTurn, load, save};
    let mut app = scanned_app("ask-sessions-oldest");
    let derived = app.proj.derived_dir.clone().expect("a derived dir");
    let full: Vec<AskSession> = (1..=MAX_SESSIONS as u64)
        .map(|id| AskSession {
            id,
            title: format!("s{id}"),
            updated: 1_000 + id as i64,
            turns: vec![SavedTurn {
                question: format!("q{id}"),
                spoken: false,
                answer: "a".into(),
            }],
        })
        .collect();
    save(&derived, &full).unwrap();
    app.proj.ask_sessions = load(&derived);
    app.proj.ask_session = None;
    // A new session's first answer, streaming from the provider here.
    let stream = app.mint_request_id();
    let _ = app.start_ask_stream(
        crate::app::state::PendingAsk {
            stream,
            question: "why?".into(),
            sources: Vec::new(),
            spoken: false,
        },
        unreachable_llm(),
        "system".into(),
        vec![llm::ChatMsg::user("why?")],
    );
    assert!(app.proj.inflight.ask_local.is_some());

    let _ = app.update(Message::Ask(AskMsg::SwitchSession(1)));
    assert_eq!(app.proj.ask_session, Some(1));
    assert_eq!(app.proj.ask_turns[0].question, "q1");
    let kept = load(&derived);
    assert_eq!(kept.len(), MAX_SESSIONS);
    assert!(kept.iter().any(|s| s.id == 1), "the one on screen is kept");
    assert!(
        kept.iter().any(|s| s.turns[0].question == "why?"),
        "and the one left"
    );
    assert!(
        !kept.iter().any(|s| s.id == 2),
        "the oldest of the others made room"
    );
    assert_eq!(app.proj.ask_sessions, kept, "in the menu");
}
