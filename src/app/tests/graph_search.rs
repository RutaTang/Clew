//! Finding a node on a map: the camera sent to it, picked, stepped through.

use super::*;

/// A map open, laid out with the nodes `labels`.
fn map_with(labels: &[&str]) -> App {
    map_over(blank_app(), labels)
}

/// A map open over `app`, laid out with the nodes `labels`.
fn map_over(mut app: App, labels: &[&str]) -> App {
    app.proj.overlay = Some(Overlay::ProjectCalls);
    app.proj.graph_layout_for = Some(Overlay::ProjectCalls);
    app.graph_mode = true;
    app.proj.graph_layout = Some(crate::graphlayout::Layout {
        nodes: labels
            .iter()
            .enumerate()
            .map(|(i, label)| crate::graphlayout::LNode {
                label: label.to_string(),
                file: PathBuf::from(format!("/p/{label}")),
                line: 1,
                x: i as f32 / labels.len() as f32,
                y: 0.5,
                weight: 1.0,
                cyclic: false,
                depth: 0.5,
            })
            .collect(),
        edges: (1..labels.len()).map(|i| (i - 1, i)).collect(),
        total: labels.len(),
    });
    app
}

fn picked(app: &App) -> Option<String> {
    app.proj.graph_pick.as_ref().map(|p| p.label.clone())
}

/// Typing finds: the best match is picked — what it is, and Open — and
/// the camera is sent to it. Enter goes on to the next, round; Shift with
/// it, or the button, back. A search that finds nothing picks nothing.
#[test]
fn a_search_picks_its_best_match_and_steps_through_the_rest() {
    let mut app = map_with(&[
        "src/checkout.py",
        "src/pricing.py",
        "tests/test_pricing.py",
        "src/pricing_rules.py",
    ]);
    let rev = app.graph_focus_rev;
    let _ = app.update(Message::Graph(GraphMsg::Search("pricing".into())));
    assert_eq!(picked(&app).as_deref(), Some("src/pricing.py"));
    assert!(app.graph_focus_rev > rev, "the camera was not sent");
    assert_eq!(app.graph_matches().len(), 3);

    let step = |app: &mut App, forward: bool| {
        let _ = app.update(Message::Graph(GraphMsg::SearchStep(forward)));
        picked(app)
    };
    assert_eq!(
        step(&mut app, true).as_deref(),
        Some("src/pricing_rules.py")
    );
    assert_eq!(
        step(&mut app, true).as_deref(),
        Some("tests/test_pricing.py")
    );
    assert_eq!(
        step(&mut app, true).as_deref(),
        Some("src/pricing.py"),
        "round"
    );
    // Shift with Enter, and the button, go back.
    app.modifiers = keyboard::Modifiers::SHIFT;
    assert_eq!(
        step(&mut app, true).as_deref(),
        Some("tests/test_pricing.py")
    );
    app.modifiers = keyboard::Modifiers::default();
    assert_eq!(
        step(&mut app, false).as_deref(),
        Some("src/pricing_rules.py")
    );

    let _ = app.update(Message::Graph(GraphMsg::Search("nothing".into())));
    assert_eq!(picked(&app), None);
    let rev = app.graph_focus_rev;
    let _ = app.update(Message::Graph(GraphMsg::SearchStep(true)));
    assert_eq!(app.graph_focus_rev, rev, "stepped through nothing");
}

/// Esc clears a search before it closes the map; a search typed over one
/// map is not another's, nor the same map's opened again.
#[test]
fn escape_clears_the_search_first_and_a_map_opens_without_one() {
    let mut app = map_with(&["a.py", "b.py"]);
    let _ = app.update(Message::Graph(GraphMsg::Search("a".into())));
    assert_eq!(
        crate::ui::escape_message(&app).map(|m| format!("{m:?}")),
        Some(format!(
            "{:?}",
            Message::Graph(GraphMsg::Search(String::new()))
        ))
    );
    let _ = app.update(crate::ui::escape_message(&app).unwrap());
    assert!(app.graph_search.is_empty());
    assert!(app.proj.overlay.is_some(), "closed with the search");
    let _ = app.update(crate::ui::escape_message(&app).unwrap());
    assert!(app.proj.overlay.is_none());

    let mut app = map_with(&["a.py", "b.py"]);
    let _ = app.update(Message::Graph(GraphMsg::Search("a".into())));
    let _ = app.update(Message::Graph(GraphMsg::OpenOverlay(
        Overlay::ProjectImports,
    )));
    assert!(app.graph_search.is_empty(), "carried to another map");
    let mut app = map_with(&["a.py", "b.py"]);
    let _ = app.update(Message::Graph(GraphMsg::Search("a".into())));
    let _ = app.update(Message::Graph(GraphMsg::CloseOverlay));
    assert!(app.graph_search.is_empty(), "kept for the next time");
}

/// The map's header has the search: the field, and which match of how
/// many the camera is on.
#[test]
fn the_maps_header_says_which_match_it_is_on() {
    let mut app = map_with(&["src/pricing.py", "tests/test_pricing.py", "c.py"]);
    let _ = app.update(Message::Graph(GraphMsg::Search("pricing".into())));
    let _ = app.update(Message::Graph(GraphMsg::SearchStep(true)));
    {
        let mut sim = iced_test::simulator(crate::ui::graph_search_field(&app));
        assert!(sim.find("2 of 2").is_ok());
    }
    let _ = app.update(Message::Graph(GraphMsg::Search("zzz".into())));
    let mut sim = iced_test::simulator(crate::ui::graph_search_field(&app));
    assert!(sim.find("No matches").is_ok());
}

/// Find, over a map, finds in the map: it does not open the find bar of
/// the file under it.
#[test]
fn find_over_a_map_finds_in_the_map() {
    let mut reading = scanned_app("graph-find");
    open_synchronously(&mut reading, "src/lib.rs", None);
    dismiss_overlays(&mut reading);
    let mut app = map_over(reading, &["a.py", "b.py"]);
    assert!(matches!(
        crate::ui::active_overlay(&app),
        Some(crate::ui::ActiveOverlay::Graph(_))
    ));
    let task = app.run_command_action(crate::keymap::Action::FindInFile);
    assert!(task.is_some());
    assert!(!app.proj.find.open, "the find bar under the map opened");
}
