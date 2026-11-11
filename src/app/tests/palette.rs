//! The command palette (`app::palette`): `>` in the finder, or typed in the
//! command bar ⌘⇧P opens, it lists what the window can do now, ranks it as
//! you type, runs the pick with Enter, and leads with what was run last. With
//! nothing typed, the command bar suggests what to do here.

use super::*;
use crate::app::palette::{PaletteAction, Suggestion};

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

/// The command bar, `>` typed in it: every command.
fn open_commands(app: &mut App) {
    open_palette(app);
    typed(app, ">");
}

fn enter(app: &mut App) {
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
}

fn down(app: &mut App) {
    let _ = app.update(key_press(
        keyboard::Key::Named(keyboard::key::Named::ArrowDown),
        keyboard::Modifiers::empty(),
    ));
}

/// ⌘⇧P opens the command bar, nothing typed; `>` lists the commands; typing
/// narrows them; Enter runs the top match and closes it; and that command
/// leads the list next time.
#[test]
fn the_palette_opens_ranks_runs_and_remembers() {
    let mut app = scanned_app("palette-run");
    open_palette(&mut app);
    assert!(app.proj.finder.open && app.proj.finder.bar);
    assert_eq!(app.proj.finder.query, "");
    assert!(app.proj.finder.is_home());
    typed(&mut app, ">");
    let all = titles(&app);
    for expected in [
        "Call Graph",
        "Go to File…",
        "Architecture Overview",
        "Settings",
        "Sidebar: Files",
        "View: Show Minimap",
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
    assert_eq!(call_graph.keys, Some(crate::keymap::local_keys("⇧⌘C")));

    typed(&mut app, "> call gra");
    assert_eq!(titles(&app).first().map(String::as_str), Some("Call Graph"));
    let _ = app.update(Message::Nav(NavMsg::FinderConfirm));
    assert!(
        !app.proj.finder.open,
        "running a command closes the palette"
    );
    assert_eq!(app.proj.overlay, Some(Overlay::ProjectCalls));
    let _ = app.update(Message::Graph(GraphMsg::CloseOverlay));

    // Next time, the command run last leads, and the bar suggests it.
    open_palette(&mut app);
    assert!(app.palette_home.contains(&Suggestion::Command {
        title: "Call Graph".into(),
        recent: true
    }));
    typed(&mut app, ">");
    assert_eq!(titles(&app).first().map(String::as_str), Some("Call Graph"));
    // A click runs the row it is on, by its title.
    let _ = app.update(Message::Nav(NavMsg::PaletteRun(
        "Sidebar: Bookmarks".into(),
    )));
    assert_eq!(app.sidebar, SidebarTab::Marks);
    assert!(!app.proj.finder.open);
    open_commands(&mut app);
    assert_eq!(
        titles(&app)[..2],
        ["Sidebar: Bookmarks".to_string(), "Call Graph".to_string()]
    );

    // Without the `>`, the bar matches the commands and then the files: a
    // command or a file, whichever is picked.
    typed(&mut app, "lib");
    assert!(!app.proj.finder.is_commands() && app.proj.finder.is_bar_search());
    let files = &app.proj.project.as_ref().unwrap().files;
    let n = app.palette.len();
    let file = app
        .proj
        .finder
        .results
        .iter()
        .position(|&i| i >= n && files.get(i - n).is_some_and(|f| f.rel == "src/lib.rs"))
        .expect("src/lib.rs among the results");
    assert!(
        app.proj.finder.results[..file].iter().all(|&i| i < n),
        "the commands lead"
    );
    app.proj.finder.selected = file;
    enter(&mut app);
    assert!(!app.proj.finder.open);
    let lib = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    land_active_load(&mut app, lib.clone(), None);
    assert_eq!(app.active_viewer().map(|v| v.abs.clone()), Some(lib));
    // Typing `>` in the file finder gives the commands.
    let _ = app.update(Message::Nav(NavMsg::FinderOpened(FinderMode::Files)));
    typed(&mut app, ">stats");
    assert_eq!(
        titles(&app).first().map(String::as_str),
        Some("Code Statistics"),
        "found by the word people use"
    );
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
    open_commands(&mut app);
    assert!(
        !titles(&app).iter().any(|t| t == "Toggle Breakpoint"),
        "no caret, no caret commands"
    );
    let _ = app.update(Message::Nav(NavMsg::FinderClosed));
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = Some((2, 4));
    }
    open_commands(&mut app);
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
    // The rows that call the AI model are marked (a sparkle), the rest not.
    let spends = |title: &str| {
        app.palette
            .iter()
            .find(|c| c.title == title)
            .unwrap_or_else(|| panic!("{title} listed"))
            .spends_ai()
    };
    assert!(spends("Explain"));
    assert!(!spends("Go to Definition") && !spends("Toggle Breakpoint"));
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
    // The bar suggests asking, and opening a project.
    assert_eq!(
        app.palette_home,
        [
            Suggestion::Ask("Ask Clew a question".into()),
            Suggestion::Command {
                title: "Open Folder…".into(),
                recent: false
            },
            Suggestion::Command {
                title: "Open Remote…".into(),
                recent: false
            },
        ]
    );
    typed(&mut app, ">");
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
    for absent in [
        "Call Graph",
        "Go to File…",
        "Sidebar: Files",
        "Architecture Overview",
    ] {
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

/// With nothing typed, the command bar suggests what to do where the caret
/// is: ask about the function it is in, talk, go to a file or to the file's
/// symbols from that function on. Each does what it says, and the arrows
/// move through them under their headings.
#[test]
fn the_command_bar_suggests_what_to_do_here() {
    let mut app = scanned_app("palette-home");
    open_synchronously(&mut app, "src/lib.rs", None);
    // The language server's download prompt, over everything, would take
    // the arrows.
    app.proj.pending_lsp_consent = Default::default();
    let lib = app.proj.project.as_ref().unwrap().root.join("src/lib.rs");
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.symbols = vec![
            crate::outline::Symbol {
                name: "Point".into(),
                kind: "struct".into(),
                line: 1,
                end_line: 1,
            },
            crate::outline::Symbol {
                name: "origin".into(),
                kind: "function".into(),
                line: 3,
                end_line: 5,
            },
        ];
        v.caret = Some((3, 4));
    }
    open_palette(&mut app);
    let symbol = |name: &str, line| Suggestion::Symbol {
        name: name.into(),
        file: "lib.rs".into(),
        line,
        abs: lib.clone(),
    };
    assert_eq!(
        app.palette_home,
        [
            Suggestion::Ask("Ask Clew about “origin”".into()),
            Suggestion::GoToFile,
            symbol("origin", 3),
        ]
    );
    assert_eq!(
        app.palette_home
            .iter()
            .map(Suggestion::group)
            .collect::<Vec<_>>(),
        ["Ask", "Go to", "Go to"]
    );
    // The results are the suggestions, the first selected; the arrows keep
    // count of the headings over the selection, for scrolling it into view.
    assert_eq!(app.proj.finder.results, [0, 1, 2]);
    assert_eq!(app.headings_over_selection(), 1);
    down(&mut app);
    assert_eq!(app.proj.finder.selected, 1);
    assert_eq!(app.headings_over_selection(), 2);

    // Go to File… opens the file finder in its place.
    enter(&mut app);
    assert!(app.proj.finder.open && !app.proj.finder.bar);
    assert_eq!(app.proj.finder.mode, FinderMode::Files);
    let _ = app.update(Message::Nav(NavMsg::FinderClosed));

    // A symbol opens at its line.
    if let Some(v) = app.proj.panes[app.proj.active].as_mut() {
        v.caret = Some((0, 0));
    }
    open_palette(&mut app);
    assert_eq!(app.palette_home[2], symbol("Point", 1));
    assert_eq!(app.palette_home[3], symbol("origin", 3));
    let _ = app.update(Message::Nav(NavMsg::PaletteSuggestion(symbol("origin", 3))));
    assert!(!app.proj.finder.open);
    assert_eq!(
        app.active_viewer().and_then(|v| v.caret).map(|(l, _)| l),
        Some(2),
        "the caret on its line"
    );

    // Asking opens the Ask window, its suggestions about the caret's symbol.
    open_palette(&mut app);
    enter(&mut app);
    assert!(!app.proj.finder.open);
    assert!(app.ask_view.is_open());
}

/// A command is found by the other words it goes by, though its row does
/// not show them: "map" finds the two graphs once called maps — Project
/// Graphs (Project Maps) and Type Graph (Type Map) — first, in English and
/// in another language alike.
#[test]
fn a_command_is_found_by_the_words_it_also_goes_by() {
    use crate::i18n::{Lang, with_lang};
    let mut app = scanned_app("palette-aliases");
    open_commands(&mut app);
    typed(&mut app, ">map");
    let found = titles(&app);
    let mut first: Vec<&str> = found.iter().take(2).map(String::as_str).collect();
    first.sort_unstable();
    assert_eq!(first, ["Project Graphs", "Type Graph"], "{found:?}");
    with_lang(Lang::Fr, || {
        open_commands(&mut app);
        typed(&mut app, ">map");
        let found = titles(&app);
        assert!(found.iter().any(|t| t == "Project Graphs"), "{found:?}");
    });
}

/// In another language too, the command bar lists the commands that say
/// what was typed — in that language or in English — and not those that
/// only hold its letters: "ligne" brought back "Thème clair : One Light",
/// and "go to line" typed in French came after six loose matches.
#[test]
fn the_command_bar_keeps_to_worded_titles_in_another_language() {
    use crate::i18n::{Lang, with_lang};
    let mut app = scanned_app("palette-worded-fr");
    open_synchronously(&mut app, "src/lib.rs", None);
    with_lang(Lang::Fr, || {
        for query in ["ligne", "go to line"] {
            open_palette(&mut app);
            typed(&mut app, query);
            let found = titles(&app);
            assert!(
                found.iter().any(|t| t == "Go to Line…"),
                "{query}: {found:?}"
            );
            assert!(
                !found.iter().any(|t| t.to_lowercase().contains("theme")),
                "{query}: {found:?}"
            );
            let _ = app.update(Message::Nav(NavMsg::FinderClosed));
        }
    });
}
