//! The floating Ask window (`ui::askfloat`, `AskView`): put away, a card in
//! the corner, or large and centred — opened, resized, put away from the
//! keyboard (Esc one step at a time) and by what happens in the window.

use super::*;

#[test]
fn ask_opens_as_a_card_grows_large_and_steps_back_with_esc() {
    let mut app = scanned_app("askfloat-steps");
    assert_eq!(app.ask_view, AskView::Closed);
    // ⇧⌘A: a card, its input taking the keyboard.
    let _ = app.update(key_press(
        keyboard::Key::Character("a".into()),
        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
    ));
    assert_eq!(app.ask_view, AskView::Compact);
    assert!(!app.code_focused, "the Ask input has the keyboard");
    let _ = app.update(Message::Ask(AskMsg::Expand));
    assert_eq!(app.ask_view, AskView::Expanded);
    // Esc: large to a card, a card put away — one step a press.
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(esc_press());
    assert_eq!(app.ask_view, AskView::Closed);
    assert!(app.code_focused, "the keyboard back with the code");
    // ⇧⌘A from large puts it away too; Shrink only makes it a card.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let _ = app.update(Message::Ask(AskMsg::Shrink));
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    assert_eq!(app.ask_view, AskView::Closed);
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
    assert_eq!(app.ask_view, AskView::Closed);
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

/// The card moves to the top of its corner when the caret's line would be
/// under it, and back when it would not.
#[test]
fn the_card_moves_out_of_the_carets_way() {
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
    // Near the top of the view: the card stays at the bottom.
    v.caret = Some((2, 0));
    assert_eq!(crate::ui::ask_place(&app).map(|p| p.at_top), Some(false));
    // Near the bottom: under the card, which moves up.
    let rows = (700.0 / lh) as usize;
    app.active_viewer_mut().unwrap().caret = Some((rows - 2, 0));
    assert_eq!(crate::ui::ask_place(&app).map(|p| p.at_top), Some(true));
    // Scrolled out of view: no caret to dodge.
    app.active_viewer_mut().unwrap().scroll_y = 300.0 * lh;
    assert_eq!(crate::ui::ask_place(&app).map(|p| p.at_top), Some(false));
    // Large: centred, wider than the card.
    let _ = app.update(Message::Ask(AskMsg::Expand));
    let place = crate::ui::ask_place(&app).unwrap();
    assert!(place.width > crate::ui::ASK_CARD_W, "{place:?}");
    assert!(!place.at_top);
}

/// The tour's Ask step shows the card; the steps after it put it away.
#[test]
fn the_tour_shows_the_ask_card() {
    let mut app = scanned_app("askfloat-tour");
    let _ = app.update(Message::Tutorial(TutorialMsg::Start));
    let ask_step = crate::app::tutorial::steps(&app)
        .iter()
        .position(|s| s.title == "Ask clew")
        .expect("an Ask step");
    while app.tutorial.is_some_and(|s| s < ask_step) {
        let _ = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    }
    assert_eq!(app.ask_view, AskView::Compact);
    let _ = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    assert_eq!(app.ask_view, AskView::Closed);
    assert!(app.show_bottom, "the Debug step shows the debugger's panel");
}
