//! The command palette (`app::palette`): opened by ⌘⇧P or `>` in the finder,
//! it lists what the window can do now, ranks it as you type, runs the pick
//! with Enter, and leads with what was run last.

use super::*;
use crate::app::palette::PaletteAction;

fn titles(app: &App) -> Vec<String> {
    app.proj
        .finder
        .results
        .iter()
        .filter_map(|i| app.palette.get(*i))
        .map(|c| c.title.clone())
        .collect()
}

fn typed(app: &mut App, query: &str) {
    let _ = app.update(Message::Nav(NavMsg::FinderQueryChanged(query.into())));
}

fn open_palette(app: &mut App) {
    let _ = app.update(key_press(
        keyboard::Key::Character("p".into()),
        keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
    ));
}

/// ⌘⇧P opens the finder as the palette; typing narrows it; Enter runs the
/// top match and closes it; and that command leads the list next time.
#[test]
fn the_palette_opens_ranks_runs_and_remembers() {
    let mut app = scanned_app("palette-run");
    open_palette(&mut app);
    assert!(app.proj.finder.open);
    assert_eq!(app.proj.finder.query, ">");
    let all = titles(&app);
    for expected in [
        "Call Graph",
        "Go to File…",
        "Overview",
        "Settings",
        "Sidebar: Files",
        "View: Hide Minimap",
        "Theme: Dark",
        "Open Folder…",
    ] {
        assert!(all.iter().any(|t| t == expected), "{expected} in {all:?}");
    }
    assert!(
        !all.iter().any(|t| t == "Command Palette…"),
        "the palette does not list itself"
    );
    // Rows show their keys.
    let call_graph = app
        .palette
        .iter()
        .find(|c| c.title == "Call Graph")
        .unwrap();
    assert_eq!(call_graph.keys.as_deref(), Some("⇧⌘C"));

    typed(&mut app, "> call gra");
    assert_eq!(titles(&app).first().map(String::as_str), Some("Call Graph"));
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
    assert!(
        !app.proj.finder.open,
        "running a command closes the palette"
    );
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls));
    let _ = app.update(Message::Graph(GraphMsg::CloseOverlay));

    // Next time, the command run last leads.
    open_palette(&mut app);
    assert_eq!(titles(&app).first().map(String::as_str), Some("Call Graph"));
    // A click runs the row it is on, by its title.
    let _ = app.update(Message::Nav(NavMsg::PaletteRun("Sidebar: Marks".into())));
    assert_eq!(app.sidebar, SidebarTab::Marks);
    assert!(!app.proj.finder.open);
    open_palette(&mut app);
    assert_eq!(
        titles(&app)[..2],
        ["Sidebar: Marks".to_string(), "Call Graph".to_string()]
    );

    // Deleting the `>` gives the files back; typing it in the file finder
    // gives the commands.
    typed(&mut app, "lib");
    assert!(!app.proj.finder.is_commands());
    let files = &app.proj.project.as_ref().unwrap().files;
    let first = app.proj.finder.results.first().and_then(|i| files.get(*i));
    assert_eq!(first.map(|f| f.rel.as_str()), Some("src/lib.rs"));
    let _ = app.update(Message::Nav(NavMsg::FinderClosed));
    let _ = app.update(Message::Nav(NavMsg::FinderOpened(FinderMode::Files)));
    typed(&mut app, ">stats");
    assert_eq!(titles(&app).first().map(String::as_str), Some("Stats"));
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
    assert!(!app.proj.finder.open);
}

/// The context menu's actions are listed while there is a caret, and run at
/// it: the breakpoint lands on the caret's line.
#[test]
fn caret_commands_run_where_the_caret_is() {
    let mut app = scanned_app("palette-caret");
    open_synchronously(&mut app, "src/lib.rs", None);
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = None;
    }
    open_palette(&mut app);
    assert!(
        !titles(&app).iter().any(|t| t == "Toggle Breakpoint"),
        "no caret, no caret commands"
    );
    let _ = app.update(Message::Nav(NavMsg::FinderClosed));
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = Some((2, 4));
    }
    open_palette(&mut app);
    let all = titles(&app);
    for expected in [
        "Go to Definition",
        "Find References",
        "Call Hierarchy",
        "Expand Macro",
        "Explain",
        "Toggle Breakpoint",
    ] {
        assert!(all.iter().any(|t| t == expected), "{expected} in {all:?}");
    }
    let goto = app
        .palette
        .iter()
        .find(|c| c.title == "Go to Definition")
        .unwrap();
    assert_eq!(goto.keys.as_deref(), Some("gd"));
    assert!(matches!(
        goto.action,
        PaletteAction::Goto(GotoKind::Definition)
    ));
    typed(&mut app, ">toggle breakpoint");
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
    let lib = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    let lines: Vec<usize> = app
        .debug
        .breakpoints
        .get(&lib)
        .map(|b| b.keys().copied().collect())
        .unwrap_or_default();
    assert_eq!(lines, [3], "the caret's line, 1-based");
    assert!(app.proj.context_menu.is_none(), "no menu left open");
}

/// With no project open, the palette still opens, with what needs none.
#[test]
fn without_a_project_only_what_needs_none_is_listed() {
    let mut app = blank_app();
    open_palette(&mut app);
    assert!(app.proj.finder.open);
    let all = titles(&app);
    for expected in [
        "Open Folder…",
        "Open Remote…",
        "Settings",
        "Zoom In",
        "Theme: Light",
    ] {
        assert!(all.iter().any(|t| t == expected), "{expected} in {all:?}");
    }
    for absent in ["Call Graph", "Go to File…", "Sidebar: Files", "Overview"] {
        assert!(!all.iter().any(|t| t == absent), "{absent} in {all:?}");
    }
    // The window's own preference: the palette in effect is the process's,
    // which another test may be previewing at the same moment.
    typed(&mut app, ">theme light");
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
    assert_eq!(app.theme_pref, crate::theme::ThemePref::Light);
}

/// Every title is listed once — a click finds its command by its title —
/// whatever the window shows.
#[test]
fn every_command_title_is_unique() {
    let mut app = scanned_app("palette-unique");
    open_synchronously(&mut app, "src/lib.rs", None);
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = Some((0, 0));
    }
    app.show_bottom = true;
    let commands = app.palette_commands();
    assert!(commands.len() > 50, "{}", commands.len());
    let mut seen = std::collections::HashSet::new();
    for c in &commands {
        assert!(seen.insert(c.title.as_str()), "{} listed twice", c.title);
    }
}
