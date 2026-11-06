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
        .position(|s| s.title == "Ask clew, or tell it what to do")
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
