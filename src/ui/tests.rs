//! View-layer tests that drive the real widget tree the way the runtime does:
//! build the view, feed it events, carry its widget state (`Cache`) across
//! rebuilds, and read that state back through widget operations.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, DocsMsg, EditorMsg, ExplainMsg,
    GlossaryMsg, GraphMsg, HoverMsg, NavMsg, ProjectMsg, ReadingMsg, ServerMsg, Stamp, TutorialMsg,
    WalkMsg, WindowMsg,
};

use iced::advanced::clipboard;
use iced::advanced::widget::Operation;
use iced::advanced::widget::operation::focusable::find_focused;
use iced::advanced::widget::operation::scrollable::Scrollable;
use iced::advanced::widget::operation::{Outcome, black_box};
use iced::widget::{Id, container, scrollable, space};
use iced::{Element, Event, Font, Pixels, Point, Rectangle, Size, Vector, keyboard, mouse, window};
use iced_test::runtime::user_interface::{Cache, UserInterface};

use crate::viewer::Viewer;
use crate::{App, Message};

const WINDOW: Size = Size {
    width: 1280.0,
    height: 860.0,
};

/// A blank `App` whose data directory is the isolated one the app tests use,
/// so building it never reads the developer's real clew data.
pub(crate) use crate::app::tests::blank_app;

/// An app reading a 400-line file in pane 0, every side panel closed.
fn reader_app() -> App {
    let mut app = blank_app();
    let root = PathBuf::from("/nonexistent/clew-ui-test");
    let source: String = (0..400)
        .map(|i| format!("fn line_{i}() {{ let value = {i}; }}\n"))
        .collect();
    let lines = crate::highlight::plain_lines(&source);
    let viewer = Viewer::new(
        root.join("src/lib.rs"),
        "src/lib.rs".into(),
        Some("rust"),
        Arc::new(source),
        lines,
    );
    app.proj.project = Some(crate::Project {
        root,
        tree: Default::default(),
        files: Arc::new(Vec::new()),
        truncated: false,
    });
    app.proj.panes[0] = Some(viewer);
    app.proj.active = 0;
    // No repository at a path that is not there: no branch to read.
    app.proj.head_stale = false;
    app.proj.repo_tip_stale = false;
    app.show_left_sidebar = false;
    app.show_right_panel = false;
    app.show_bottom = false;
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    app
}

/// A headless renderer, made the way `iced_test`'s simulator makes one.
fn renderer() -> iced::Renderer {
    crate::fonts::load();
    iced_test::futures::futures::executor::block_on(
        <iced::Renderer as iced::advanced::renderer::Headless>::new(
            Font::with_name("Fira Sans"),
            Pixels(16.0),
            None,
        ),
    )
    .expect("a headless renderer")
}

/// Reads the scroll translation of the scrollable `target`.
struct ScrollProbe {
    target: Id,
    found: Option<Vector>,
}

impl Operation for ScrollProbe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if id == Some(&self.target) {
            self.found = Some(translation);
        }
    }
}

/// Run `op` over `ui` to completion (following chained operations) and return
/// its output.
fn run_operation<T: Send + 'static>(
    ui: &mut UserInterface<'_, Message, iced::Theme, iced::Renderer>,
    renderer: &iced::Renderer,
    op: impl Operation<T> + 'static,
) -> Option<T> {
    let mut op: Box<dyn Operation<T>> = Box::new(op);
    loop {
        ui.operate(renderer, &mut black_box(&mut *op));
        match op.finish() {
            Outcome::None => return None,
            Outcome::Some(value) => return Some(value),
            Outcome::Chain(next) => op = next,
        }
    }
}

/// One runtime turn: build `app`'s view over the carried widget state, deliver
/// `events`, and return the state, the published messages, and the scroll
/// offset of pane 0's code view.
fn turn(
    app: &App,
    cache: Cache,
    renderer: &mut iced::Renderer,
    events: &[Event],
    cursor: mouse::Cursor,
) -> (Cache, Vec<Message>, Option<Vector>) {
    let mut ui = UserInterface::build(super::view(app), WINDOW, cache, renderer);
    let mut messages = Vec::new();
    let _ = ui.update(
        events,
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let mut probe = ScrollProbe {
        target: super::code_scroll_id(0),
        found: None,
    };
    ui.operate(renderer, &mut probe);
    (ui.into_cache(), messages, probe.found)
}

/// A named change to the app's layout state.
type Toggle = (&'static str, Box<dyn Fn(&mut App)>);

/// Feed the scroll reports back the way the runtime would.
fn apply_scrolls(app: &mut App, messages: Vec<Message>) {
    for msg in messages {
        if matches!(msg, Message::Editor(EditorMsg::Scrolled(..))) {
            let _ = app.update(msg);
        }
    }
}

fn redraw() -> [Event; 1] {
    [Event::Window(window::Event::RedrawRequested(
        std::time::Instant::now(),
    ))]
}

/// E2-1: iced pairs widget state with widgets by position, so any panel that
/// inserted itself in front of the code view (or nested it deeper) rebuilt the
/// code view's scrollable at offset 0 — which it then reported, throwing the
/// reader back to the top of the file. Every region now has a fixed slot.
#[test]
fn panels_coming_and_going_never_reset_the_code_scroll() {
    let mut app = reader_app();
    let mut renderer = renderer();
    let (cache, _, _) = turn(
        &app,
        Cache::default(),
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    // Scroll the code 600 px down with the wheel, over the code.
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -600.0 },
    })];
    let over_code = mouse::Cursor::Available(Point::new(640.0, 400.0));
    let (mut cache, messages, offset) = turn(&app, cache, &mut renderer, &wheel, over_code);
    apply_scrolls(&mut app, messages);
    let scrolled = offset.expect("the code view is on screen").y;
    assert!(
        scrolled > 100.0,
        "the wheel did not scroll the code: {scrolled}"
    );
    assert_eq!(
        app.proj.panes[0].as_ref().map(|v| v.scroll_y),
        Some(scrolled)
    );

    let summary_arrives = |app: &mut App| {
        app.show_file_banner = true;
        let abs = app.proj.panes[0].as_ref().map(|v| v.abs.clone()).unwrap();
        app.proj.explain.cache.insert(
            crate::explain::Node::File(abs),
            crate::explain::Cached {
                summary: "Reads lines. More detail here.".into(),
                prompt_hash: crate::incremental::content_hash(b"test"),
                detail: None,
                basis: None,
            },
        );
    };
    let toggles: Vec<Toggle> = vec![
        (
            "opening the find bar",
            Box::new(|a| a.proj.find.open = true),
        ),
        ("the file summary arriving", Box::new(summary_arrives)),
        (
            "opening the left sidebar",
            Box::new(|a| a.show_left_sidebar = true),
        ),
        (
            "opening the bottom panel",
            Box::new(|a| a.show_bottom = true),
        ),
        (
            "opening the Ask card",
            Box::new(|a| a.ask_view = crate::AskView::Compact),
        ),
        (
            "making Ask large",
            Box::new(|a| a.ask_view = crate::AskView::Expanded),
        ),
        (
            "the update banner appearing",
            Box::new(|a| {
                a.update.available = Some(crate::AvailableUpdate {
                    version: clew_core::update::Version {
                        major: 9,
                        minor: 9,
                        patch: 9,
                    },
                    dmg_url: None,
                    notes: Vec::new(),
                })
            }),
        ),
        (
            "opening the right panel",
            Box::new(|a| a.show_right_panel = true),
        ),
        ("opening the ⋯ menu", Box::new(|a| a.show_tools_menu = true)),
        ("starting the tutorial", Box::new(|a| a.tutorial = Some(0))),
        ("splitting the view", Box::new(|a| a.proj.split = true)),
        (
            "closing everything again",
            Box::new(|a| {
                a.proj.find.open = false;
                a.show_file_banner = false;
                a.show_left_sidebar = false;
                a.show_bottom = false;
                a.ask_view = crate::AskView::Hidden;
                a.update.available = None;
                a.show_right_panel = false;
                a.show_tools_menu = false;
                a.tutorial = None;
                a.proj.split = false;
            }),
        ),
    ];
    for (what, toggle) in toggles {
        toggle(&mut app);
        let (next, messages, offset) = turn(
            &app,
            cache,
            &mut renderer,
            &redraw(),
            mouse::Cursor::Unavailable,
        );
        cache = next;
        assert_eq!(
            offset.map(|o| o.y),
            Some(scrolled),
            "{what} reset the code view's scroll"
        );
        apply_scrolls(&mut app, messages);
        assert_eq!(
            app.proj.panes[0].as_ref().map(|v| v.scroll_y),
            Some(scrolled),
            "{what}: the app was told the code view moved"
        );
    }
}

/// The widget operations a task carries (its `scroll_to`s), for the runtime
/// step that applies them to the rebuilt view.
fn widget_ops(task: iced::Task<Message>) -> Vec<Box<dyn Operation>> {
    use iced_test::futures::futures::StreamExt;
    let Some(stream) = iced_test::runtime::task::into_stream(task) else {
        return Vec::new();
    };
    iced_test::futures::futures::executor::block_on(stream.collect::<Vec<_>>())
        .into_iter()
        .filter_map(|action| match action {
            iced_test::runtime::Action::Widget(op) => Some(op),
            _ => None,
        })
        .collect()
}

/// One runtime turn after an update that returned `task`: rebuild the view,
/// apply the task's widget operations to it, then deliver a redraw — the order
/// the runtime runs them in. Returns the state, the scroll reports, and pane
/// 0's code-view offset.
fn turn_after(
    app: &App,
    task: iced::Task<Message>,
    cache: Cache,
    renderer: &mut iced::Renderer,
) -> (Cache, Vec<Message>, Option<Vector>) {
    let mut ui = UserInterface::build(super::view(app), WINDOW, cache, renderer);
    for mut op in widget_ops(task) {
        loop {
            ui.operate(renderer, &mut black_box(&mut *op));
            match op.finish() {
                Outcome::Chain(next) => op = next,
                _ => break,
            }
        }
    }
    let mut messages = Vec::new();
    let _ = ui.update(
        &redraw(),
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let mut probe = ScrollProbe {
        target: super::code_scroll_id(0),
        found: None,
    };
    ui.operate(renderer, &mut probe);
    (ui.into_cache(), messages, probe.found)
}

/// A click on the minimap moves the file down to it and leaves it where it
/// was across: a reader scrolled right was thrown back to the left edge.
#[test]
fn a_minimap_click_keeps_the_place_across() {
    let mut app = reader_app();
    let wide: String = (0..400)
        .map(|i| format!("fn line_{i}() {{}} // {}\n", "wide ".repeat(80)))
        .collect();
    app.proj.panes[0] = Some(Viewer::new(
        PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"),
        "src/lib.rs".into(),
        Some("rust"),
        Arc::new(wide.clone()),
        crate::highlight::plain_lines(&wide),
    ));
    let mut renderer = renderer();
    let (cache, _, _) = turn(
        &app,
        Cache::default(),
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: -400.0, y: 0.0 },
    })];
    let over_code = mouse::Cursor::Available(Point::new(400.0, 400.0));
    let (cache, messages, across) = turn(&app, cache, &mut renderer, &wheel, over_code);
    let across = across.expect("the code view is on screen").x;
    assert!(across > 100.0, "precondition: scrolled right ({across})");
    apply_scrolls(&mut app, messages);
    let task = app.update(Message::Editor(EditorMsg::MinimapScrolled {
        pane: 0,
        fraction: 0.5,
    }));
    let (_, _, at) = turn_after(&app, task, cache, &mut renderer);
    let at = at.expect("the code view is on screen");
    assert!(at.y > 100.0, "moved down: {at:?}");
    assert!(
        (at.x - across).abs() < 1.0,
        "kept across: {at:?} vs {across}"
    );
}

/// A minimap drag whose band goes away under it — the minimap turned off
/// while held — ends there: its release, unseen, no longer leaves it on, so
/// moves once the band is back do not scroll the file, no button held.
#[test]
fn a_minimap_drag_ends_with_its_band() {
    let mut app = reader_app();
    app.show_minimap = true;
    let mut renderer = renderer();
    let band_x = WINDOW.width - super::STRIP_W - super::SHEET_GAP - 28.0;
    let on_band = mouse::Cursor::Available(Point::new(band_x, 400.0));
    let scrolled = |messages: &[Message]| {
        messages
            .iter()
            .any(|m| matches!(m, Message::Editor(EditorMsg::MinimapScrolled { .. })))
    };
    let press = [Event::Mouse(mouse::Event::ButtonPressed(
        mouse::Button::Left,
    ))];
    let (cache, messages, _) = turn(&app, Cache::default(), &mut renderer, &press, on_band);
    assert!(scrolled(&messages), "precondition: the band is there");
    // Turned off while held; let go of meanwhile.
    app.show_minimap = false;
    let release = [Event::Mouse(mouse::Event::ButtonReleased(
        mouse::Button::Left,
    ))];
    let (cache, _, _) = turn(&app, cache, &mut renderer, &release, on_band);
    // Back, and the pointer moves over it.
    app.show_minimap = true;
    let elsewhere = Point::new(band_x, 500.0);
    let moved = [Event::Mouse(mouse::Event::CursorMoved {
        position: elsewhere,
    })];
    let (_, messages, _) = turn(
        &app,
        cache,
        &mut renderer,
        &moved,
        mouse::Cursor::Available(elsewhere),
    );
    assert!(!scrolled(&messages), "{messages:?}");
}

/// A11: scrubbing to another revision keeps the reader's place on BOTH axes —
/// on the historical view itself. The scroll the reader makes there is
/// recorded from the view's own reports, and the next revision's view,
/// mounted fresh (at the top-left), is scrolled back to it: a reader
/// scrolled right used to be thrown back to the left edge on every scrub.
#[test]
fn a_time_travel_scrub_keeps_the_place_on_both_axes() {
    let mut app = reader_app();
    let wide: String = (0..400)
        .map(|i| {
            format!(
                "fn line_{i}() {{ let value = {i}; }} // {}\n",
                "wide ".repeat(80)
            )
        })
        .collect();
    let v = app.proj.panes[0].as_ref().unwrap();
    let (abs, rel) = (v.abs.clone(), v.rel.clone());
    let historical = Viewer::new(
        abs.clone(),
        rel.clone(),
        Some("rust"),
        Arc::new(wide.clone()),
        crate::highlight::plain_lines(&wide),
    );
    let commit = |sha: &str| clew_protocol::HistCommit {
        sha: sha.into(),
        author: "a".into(),
        time: 0,
        subject: "s".into(),
        path: rel.clone(),
    };
    app.proj.time_travel = Some(crate::TimeTravel {
        abs,
        rel: rel.clone(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: vec![commit("b"), commit("a")],
        idx: 0,
        viewer: Some(historical),
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: false,
        generation: app.proj.time_gen,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    let mut renderer = renderer();
    let (cache, _, _) = turn(
        &app,
        Cache::default(),
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels {
            x: -400.0,
            y: -600.0,
        },
    })];
    let over_code = mouse::Cursor::Available(Point::new(640.0, 400.0));
    let (cache, messages, read_to) = turn(&app, cache, &mut renderer, &wheel, over_code);
    let read_to = read_to.expect("the historical view is on screen");
    assert!(read_to.y > 100.0 && read_to.x > 100.0, "{read_to:?}");
    // The view's reports reach the session, as the runtime delivers them.
    for msg in messages {
        if matches!(msg, Message::TimeTravel(crate::TimeTravelMsg::Scrolled(..))) {
            let _ = app.update(msg);
        }
    }
    // The next revision lands.
    let step = crate::TimeStep {
        lines: crate::highlight::plain_lines(&wide),
        content: wide.clone(),
        symbols: Vec::new(),
        added: std::collections::HashSet::new(),
        focus_line: None,
    };
    let task = app.update(Message::TimeTravel(crate::TimeTravelMsg::Step {
        stamp: app.stamp(),
        generation: app.proj.time_gen,
        idx: 1,
        step: Ok(Box::new(step)),
    }));
    let (_, _, offset) = turn_after(&app, task, cache, &mut renderer);
    assert_eq!(offset, Some(read_to), "the scrub lost the reader's place");
}

/// E2-1 / E2-4: a transition that puts ANOTHER widget where the code view was
/// — the diff view, the rendered markdown, time travel — rebuilds the code
/// view's scrollable when it comes back, at the top, and its report
/// overwrote where the reader was. Leaving each one puts it back, both
/// offsets; and the story panel appearing in time travel is a slot that was
/// always there, so it moves nothing.
#[test]
fn leaving_a_view_that_replaced_the_code_puts_the_reader_back() {
    let mut app = reader_app();
    // Lines wider than the window, so the reader can be scrolled right too:
    // both offsets must come back, not only the vertical one.
    {
        let wide: String = (0..400)
            .map(|i| {
                format!(
                    "fn line_{i}() {{ let value = {i}; }} // {}\n",
                    "wide ".repeat(80)
                )
            })
            .collect();
        let v = app.proj.panes[0].as_mut().unwrap();
        *v = Viewer::new(
            v.abs.clone(),
            v.rel.clone(),
            Some("rust"),
            Arc::new(wide.clone()),
            crate::highlight::plain_lines(&wide),
        );
    }
    let mut renderer = renderer();
    let (cache, _, _) = turn(
        &app,
        Cache::default(),
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels {
            x: -400.0,
            y: -600.0,
        },
    })];
    let over_code = mouse::Cursor::Available(Point::new(640.0, 400.0));
    let (mut cache, messages, offset) = turn(&app, cache, &mut renderer, &wheel, over_code);
    apply_scrolls(&mut app, messages);
    let scrolled = offset.expect("the code view is on screen");
    assert!(
        scrolled.y > 100.0 && scrolled.x > 100.0,
        "the wheel did not scroll the code both ways: {scrolled:?}"
    );
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    let kept = |app: &App| app.proj.panes[0].as_ref().map(|v| (v.scroll_x, v.scroll_y));
    let before = kept(&app);

    // The diff view, then its close.
    app.proj.diff = Some(crate::DiffState::new(
        abs.clone(),
        "src/lib.rs".into(),
        Vec::new(),
    ));
    let (next, messages, _) = turn(
        &app,
        cache,
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    apply_scrolls(&mut app, messages);
    let task = app.update(Message::Editor(EditorMsg::ToggleDiff));
    let (next, messages, offset) = turn_after(&app, task, next, &mut renderer);
    apply_scrolls(&mut app, messages);
    assert_eq!(offset, Some(scrolled), "closing the diff lost the place");
    assert_eq!(kept(&app), before, "the app was told the code view moved");
    cache = next;

    // Time travel over this file, with a story arriving mid-read, then Exit.
    let hv = app.proj.panes[0].clone().unwrap();
    app.proj.time_travel = Some(crate::TimeTravel {
        abs: abs.clone(),
        rel: "src/lib.rs".into(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: Vec::new(),
        idx: 0,
        viewer: Some(hv),
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: false,
        generation: 0,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    let (next, _, _) = turn(
        &app,
        cache,
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    let (next, _, historical) = turn(&app, next, &mut renderer, &wheel, over_code);
    let read_to = historical.expect("the historical view is on screen");
    assert!(read_to.y > 100.0 && read_to.x > 100.0, "{read_to:?}");
    app.proj.time_travel.as_mut().unwrap().story = Some(Vec::new());
    // The story of a block with no name of its own says so, not quoted as
    // if it were one.
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Story of this block"));
    assert!(!shows(&mut sim, "“this block”"));
    drop(sim);
    let (next, _, offset) = turn(
        &app,
        next,
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    assert_eq!(
        offset,
        Some(read_to),
        "the story appearing reset the historical view"
    );
    let task = app.update(Message::TimeTravel(crate::TimeTravelMsg::Exit));
    let (next, messages, offset) = turn_after(&app, task, next, &mut renderer);
    apply_scrolls(&mut app, messages);
    assert_eq!(offset, Some(scrolled), "leaving time travel lost the place");
    assert_eq!(kept(&app), before);
    cache = next;

    // A markdown file: rendered, then its source again.
    let md = "# Title\n\n".to_string() + &"a line of prose\n\n".repeat(200);
    let _ = cache;
    let v = app.proj.panes[0].as_mut().unwrap();
    *v = Viewer::new(
        PathBuf::from("/nonexistent/clew-ui-test/README.md"),
        "README.md".into(),
        Some("markdown"),
        Arc::new(md.clone()),
        crate::highlight::plain_lines(&md),
    );
    v.show_source = true;
    let (cache, _, _) = turn(
        &app,
        Cache::default(),
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    let (cache, messages, offset) = turn(&app, cache, &mut renderer, &wheel, over_code);
    apply_scrolls(&mut app, messages);
    let source_at = offset.expect("the source is on screen").y;
    assert!(source_at > 100.0);
    let _ = app.update(Message::Editor(EditorMsg::ToggleMarkdownSource(0)));
    let (cache, messages, _) = turn(
        &app,
        cache,
        &mut renderer,
        &redraw(),
        mouse::Cursor::Unavailable,
    );
    apply_scrolls(&mut app, messages);
    let task = app.update(Message::Editor(EditorMsg::ToggleMarkdownSource(0)));
    let (_, messages, offset) = turn_after(&app, task, cache, &mut renderer);
    apply_scrolls(&mut app, messages);
    assert_eq!(
        offset.map(|o| o.y),
        Some(source_at),
        "the source came back at the top"
    );
}

/// [`reader_app`], split: src/lib.rs in pane 0 and src/other.rs in pane 1,
/// each 400 lines wider than the window, and a time-travel session over
/// src/lib.rs with its revision loaded — on screen while pane 0 is focused,
/// which it is.
fn split_with_history() -> App {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let viewer = |rel: &str| {
        let source: String = (0..400)
            .map(|i| {
                format!(
                    "fn line_{i}() {{ let value = {i}; }} // {}\n",
                    "wide ".repeat(80)
                )
            })
            .collect();
        Viewer::new(
            root.join(rel),
            rel.into(),
            Some("rust"),
            Arc::new(source.clone()),
            crate::highlight::plain_lines(&source),
        )
    };
    app.proj.panes[0] = Some(viewer("src/lib.rs"));
    app.proj.panes[1] = Some(viewer("src/other.rs"));
    app.proj.split = true;
    app.proj.active = 0;
    let commit = |sha: &str| clew_protocol::HistCommit {
        sha: sha.into(),
        author: "a".into(),
        time: 0,
        subject: "s".into(),
        path: "src/lib.rs".into(),
    };
    app.proj.time_travel = Some(crate::TimeTravel {
        abs: root.join("src/lib.rs"),
        rel: "src/lib.rs".into(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: vec![commit("b"), commit("a")],
        idx: 0,
        viewer: Some(viewer("src/lib.rs")),
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: false,
        generation: app.proj.time_gen,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    app
}

/// One runtime turn over a split: rebuild `app`'s view, apply `task`'s
/// widget operations to it, deliver `events` with the cursor at `cursor` —
/// the order the runtime runs them in — and return the state, the published
/// messages, and each pane's code-view offset (a time-travel session's
/// included: its view takes the pane's scroll id).
fn split_turn(
    app: &App,
    task: iced::Task<Message>,
    cache: Cache,
    renderer: &mut iced::Renderer,
    events: &[Event],
    cursor: mouse::Cursor,
) -> (Cache, Vec<Message>, [Option<Vector>; 2]) {
    let mut ui = UserInterface::build(super::view(app), WINDOW, cache, renderer);
    for mut op in widget_ops(task) {
        loop {
            ui.operate(renderer, &mut black_box(&mut *op));
            match op.finish() {
                Outcome::Chain(next) => op = next,
                _ => break,
            }
        }
    }
    let mut messages = Vec::new();
    let _ = ui.update(
        events,
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let offsets = [0, 1].map(|pane| {
        let mut probe = ScrollProbe {
            target: super::code_scroll_id(pane),
            found: None,
        };
        ui.operate(renderer, &mut probe);
        probe.found
    });
    (ui.into_cache(), messages, offsets)
}

/// Feed the scroll reports of the live panes and of a time-travel session
/// back the way the runtime would.
fn apply_view_scrolls(app: &mut App, messages: Vec<Message>) {
    for msg in messages {
        if matches!(
            msg,
            Message::Editor(EditorMsg::Scrolled(..))
                | Message::TimeTravel(crate::TimeTravelMsg::Scrolled(..))
        ) {
            let _ = app.update(msg);
        }
    }
}

/// The wheel, scrolling right by `x` and down by `y` pixels.
fn wheel_by(x: f32, y: f32) -> [Event; 1] {
    [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: -x, y: -y },
    })]
}

/// A revision that lands while its session is off screen — the reader
/// focused the other half of a split before it came — moves no pane: not
/// the focused one, which shows another file, nor the live file under the
/// session. It is the session's own place that moves, and the session
/// shows there when its pane is focused again. The step used to scroll
/// whatever the focused pane showed to the session's place: the other file
/// jumped to where the session's block sat in its own.
#[test]
fn a_time_travel_revision_landing_off_screen_scrolls_no_pane() {
    let mut app = split_with_history();
    let mut renderer = renderer();
    let still = mouse::Cursor::Unavailable;
    let none = iced::Task::none;
    let (cache, _, _) = split_turn(
        &app,
        none(),
        Cache::default(),
        &mut renderer,
        &redraw(),
        still,
    );
    // The reader scrolls the other file, on the right...
    let over_other = mouse::Cursor::Available(Point::new(960.0, 400.0));
    let (cache, messages, offsets) = split_turn(
        &app,
        none(),
        cache,
        &mut renderer,
        &wheel_by(0.0, 600.0),
        over_other,
    );
    apply_view_scrolls(&mut app, messages);
    let other_at = offsets[1].expect("the other file is on screen");
    assert!(
        other_at.y > 100.0,
        "the wheel did not scroll it: {other_at:?}"
    );
    // ...and focuses it before the next revision of the session comes.
    let task = app.update(Message::Editor(EditorMsg::PaneFocused(1)));
    assert!(crate::ui::time_travel_on_screen(&app).is_none());
    let (cache, messages, offsets) = split_turn(&app, task, cache, &mut renderer, &redraw(), still);
    apply_view_scrolls(&mut app, messages);
    assert_eq!(offsets[1], Some(other_at));
    let live_at = offsets[0].expect("the live file shows in the session's place");

    // The revision lands, the block the session follows far down in it.
    let source = app.proj.panes[0].as_ref().unwrap().source.to_string();
    let task = app.update(Message::TimeTravel(crate::TimeTravelMsg::Step {
        stamp: app.stamp(),
        generation: app.proj.time_gen,
        idx: 1,
        step: Ok(Box::new(crate::TimeStep {
            lines: crate::highlight::plain_lines(&source),
            content: source,
            symbols: Vec::new(),
            added: std::collections::HashSet::new(),
            focus_line: Some(300),
        })),
    }));
    let (cache, messages, offsets) = split_turn(&app, task, cache, &mut renderer, &redraw(), still);
    apply_view_scrolls(&mut app, messages);
    assert_eq!(
        offsets[1],
        Some(other_at),
        "the revision scrolled the focused pane, on another file"
    );
    assert_eq!(
        offsets[0],
        Some(live_at),
        "the revision scrolled the live file under the session"
    );
    let tt = app.proj.time_travel.as_ref().expect("the session");
    assert_eq!(tt.idx, 1, "the revision was not taken");
    let stepped_to = Vector::new(tt.scroll_x, tt.scroll_y);
    assert!(
        stepped_to.y > 1000.0,
        "the block is far down: {stepped_to:?}"
    );

    // Focused again, the session shows where the revision put it.
    let task = app.update(Message::Editor(EditorMsg::PaneFocused(0)));
    let (_, _, offsets) = split_turn(&app, task, cache, &mut renderer, &redraw(), still);
    assert_eq!(
        offsets[0],
        Some(stepped_to),
        "the session showed again away from its block"
    );
}

/// A time-travel session hidden and shown again — the reader focused the
/// other half of a split, then came back — is where they left it, on both
/// axes, its caret included. And the live file it covers, shown in its
/// place meanwhile, is where the reader left that. The session's view and
/// the live one take turns in one place of the widget tree, where each is
/// built anew, at the top: its first report overwrote the place kept for
/// it, and the session came back at the top of its revision.
#[test]
fn a_time_travel_session_shown_again_keeps_its_place() {
    let mut app = split_with_history();
    // Where the reader was in the live file before going into its history.
    let kept = Vector::new(120.0, 800.0);
    let live = app.proj.panes[0].as_mut().unwrap();
    (live.scroll_x, live.scroll_y) = (kept.x, kept.y);
    let mut renderer = renderer();
    let still = mouse::Cursor::Unavailable;
    let none = iced::Task::none;
    let (cache, _, _) = split_turn(
        &app,
        none(),
        Cache::default(),
        &mut renderer,
        &redraw(),
        still,
    );
    // The reader scrolls the revision both ways, and clicks into it.
    let over_session = mouse::Cursor::Available(Point::new(320.0, 400.0));
    let (cache, messages, offsets) = split_turn(
        &app,
        none(),
        cache,
        &mut renderer,
        &wheel_by(400.0, 600.0),
        over_session,
    );
    apply_view_scrolls(&mut app, messages);
    let read_to = offsets[0].expect("the revision is on screen");
    assert!(read_to.x > 100.0 && read_to.y > 100.0, "{read_to:?}");
    let _ = app.update(Message::TimeTravel(crate::TimeTravelMsg::SelectStart {
        line: 250,
        col: 7,
    }));
    let _ = app.update(Message::Editor(EditorMsg::SelectEnd));

    // Hidden: the live file shows in its place, where the reader left it.
    let task = app.update(Message::Editor(EditorMsg::PaneFocused(1)));
    assert!(crate::ui::time_travel_on_screen(&app).is_none());
    let (cache, messages, offsets) = split_turn(&app, task, cache, &mut renderer, &redraw(), still);
    apply_view_scrolls(&mut app, messages);
    assert_eq!(offsets[0], Some(kept), "the live file came back at the top");
    let live = app.proj.panes[0].as_ref().unwrap();
    assert_eq!(Vector::new(live.scroll_x, live.scroll_y), kept);

    // Shown again: where the reader left it, caret and all.
    let task = app.update(Message::Editor(EditorMsg::PaneFocused(0)));
    assert!(crate::ui::time_travel_on_screen(&app).is_some());
    let (_, messages, offsets) = split_turn(&app, task, cache, &mut renderer, &redraw(), still);
    apply_view_scrolls(&mut app, messages);
    assert_eq!(
        offsets[0],
        Some(read_to),
        "the session came back at the top"
    );
    let tt = app.proj.time_travel.as_ref().unwrap();
    assert_eq!(Vector::new(tt.scroll_x, tt.scroll_y), read_to);
    assert_eq!(tt.caret, Some((250, 7)));
    assert_eq!(tt.viewer.as_ref().and_then(|v| v.caret), Some((250, 7)));
}

/// Every scrollable in traversal order: its bounds, its content's bounds and
/// its offset.
struct AllScrolls(Vec<(Rectangle, Rectangle, Vector)>);

impl Operation for AllScrolls {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        _id: Option<&Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        self.0.push((bounds, content_bounds, translation));
    }
}

/// Build `app`'s view in `cache`, deliver `events` with the cursor over the
/// sidebar tree, and return the cache with the tree's scroll offset — the
/// tree being the tallest scrollable inside the sidebar.
fn sidebar_tree_offset(
    app: &App,
    cache: Cache,
    renderer: &mut iced::Renderer,
    events: &[Event],
) -> (Cache, Option<iced::Vector>) {
    let over_tree = mouse::Cursor::Available(Point::new(app.sidebar_width / 2.0, 500.0));
    let mut ui = UserInterface::build(super::view(app), WINDOW, cache, renderer);
    let mut messages = Vec::new();
    let _ = ui.update(
        events,
        over_tree,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let mut all = AllScrolls(Vec::new());
    ui.operate(renderer, &mut all);
    let tree = all
        .0
        .into_iter()
        .filter(|(b, _, _)| b.x + b.width <= app.sidebar_width + 1.0)
        .max_by(|a, b| a.0.height.total_cmp(&b.0.height))
        .map(|(_, _, t)| t);
    (ui.into_cache(), tree)
}

/// A11 / E2-4: the sidebar tree's banners (import cycles, an incomplete
/// index) sit in slots that are always there. One appearing while the reader
/// is deep in the tree must not move the tree's scrollable to another
/// position — where iced rebuilds it at the top and the place is lost.
#[test]
fn a_sidebar_banner_appearing_keeps_the_trees_scroll() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Imports;
    app.import_dir = crate::imports::Dir::Importers;
    // src/lib.rs, on screen, imported by 300 modules: a long Importers tree.
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let mut files = vec![root.join("src/lib.rs")];
    let mut raw = std::collections::HashMap::new();
    for i in 0..300 {
        let module = root.join(format!("src/m{i}.rs"));
        files.push(module.clone());
        raw.insert(
            module,
            vec![crate::imports::RawImport {
                module: "crate::Thing".into(),
                line: 1,
                is_mod_decl: false,
            }],
        );
    }
    let resolver = crate::imports::Resolver::with_meta(&root, &files, None, None);
    app.proj.import_graph = Arc::new(crate::imports::ImportGraph::build(
        raw,
        &resolver,
        crate::highlight::detect,
    ));
    app.refresh_import_tree();
    let tree_offset = sidebar_tree_offset;
    let mut renderer = renderer();
    let (cache, _) = tree_offset(&app, Cache::default(), &mut renderer, &redraw());
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -900.0 },
    })];
    let (mut cache, deep) = tree_offset(&app, cache, &mut renderer, &wheel);
    let deep = deep.expect("the import tree is on screen");
    assert!(
        deep.y > 100.0,
        "the wheel did not scroll the tree: {deep:?}"
    );
    let banners: [Toggle; 4] = [
        (
            "the cycles note",
            Box::new(|app| {
                let lib = app.proj.import_tree.as_ref().unwrap().root_path.clone();
                app.proj.import_cycles = vec![vec![PathBuf::from("/x.rs"), lib]];
            }),
        ),
        (
            "the incomplete-index note",
            Box::new(|app| {
                app.proj.index_cap_note = Some("1 file over 4 MB not indexed".into());
                app.proj.index_too_large = vec![("big.c".into(), 5 << 20)];
            }),
        ),
        (
            "the cycles note opened",
            Box::new(|app| {
                app.proj
                    .import_notes_open
                    .push(crate::app::message::ImportNote::Cycles)
            }),
        ),
        (
            "the incomplete-index note opened",
            Box::new(|app| {
                app.proj
                    .import_notes_open
                    .push(crate::app::message::ImportNote::LeftOut)
            }),
        ),
    ];
    for (what, show) in banners {
        show(&mut app);
        let (next, offset) = tree_offset(&app, cache, &mut renderer, &redraw());
        assert_eq!(offset, Some(deep), "{what} reset the tree's scroll");
        cache = next;
    }
}

/// A11 / E2-4, the Calls tab: its "code changed" banner sits in a slot that
/// is always there too. Appearing while the reader is deep in a call tree
/// (a watched file changed), it must not move the tree's scrollable.
#[test]
fn the_call_trees_stale_banner_keeps_the_trees_scroll() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Calls;
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let item = |name: String| crate::lsp::client::CallItem {
        name,
        detail: String::new(),
        kind: 12,
        path: root.join("src/lib.rs"),
        line: 1,
        character: 0,
        raw: serde_json::json!({}),
    };
    let mut tree = crate::callgraph::CallTree::new(
        1,
        crate::callgraph::Direction::Incoming,
        "rust",
        vec![item("origin".into())],
    );
    let root_id = tree.roots()[0];
    tree.set_children(
        root_id,
        (0..300).map(|i| item(format!("caller{i}"))).collect(),
    );
    app.proj.call_graph = Some(tree);
    let mut renderer = renderer();
    let (cache, _) = sidebar_tree_offset(&app, Cache::default(), &mut renderer, &redraw());
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -900.0 },
    })];
    let (cache, deep) = sidebar_tree_offset(&app, cache, &mut renderer, &wheel);
    let deep = deep.expect("the call tree is on screen");
    assert!(
        deep.y > 100.0,
        "the wheel did not scroll the tree: {deep:?}"
    );
    app.proj.call_graph.as_mut().unwrap().stale = true;
    let (_, offset) = sidebar_tree_offset(&app, cache, &mut renderer, &redraw());
    assert_eq!(
        offset,
        Some(deep),
        "the stale banner reset the tree's scroll"
    );
    // It says what changed, and refreshes by a press — no key to be told of.
    let mut tab = sim_elem(super::calls_tab(&app));
    assert!(shows(&mut tab, "Code changed since this was traced"));
    assert!(!shows(&mut tab, "press"));
    let _ = tab.click("Refresh");
    let sent: Vec<Message> = tab.into_messages().collect();
    assert!(
        matches!(sent.as_slice(), [Message::Calls(CallsMsg::Refresh)]),
        "{sent:?}"
    );
}

/// A16 / E2-7: at the smallest window the app allows, the Settings modal
/// still fits — a page is a scrollable bounded by the window, taller in
/// content than on screen when it is a long one (Shortcuts), and the wheel
/// reaches the rest of it.
#[test]
fn the_settings_modal_fits_the_smallest_window_and_scrolls() {
    let min = crate::window_settings()
        .min_size
        .expect("a minimum window size");
    let mut app = reader_app();
    app.window_width = min.width;
    app.window_height = min.height;
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Shortcuts,
    )));
    let mut renderer = renderer();
    let over_form = mouse::Cursor::Available(Point::new(min.width / 2.0, min.height / 2.0));
    let form = |cache: Cache, renderer: &mut iced::Renderer, events: &[Event]| {
        let mut ui = UserInterface::build(super::view(&app), min, cache, renderer);
        let mut messages = Vec::new();
        let _ = ui.update(
            events,
            over_form,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let mut all = AllScrolls(Vec::new());
        ui.operate(renderer, &mut all);
        // The page: the topmost scrollable under the pointer (the modal is
        // drawn over the window, so it comes last).
        let form = all.0.into_iter().rev().find(|(b, _, _)| {
            b.contains(Point::new(min.width / 2.0, min.height / 2.0)) && b.x > 0.0
        });
        (ui.into_cache(), form)
    };
    let (cache, found) = form(Cache::default(), &mut renderer, &redraw());
    let (bounds, content, _) = found.expect("the settings form is on screen");
    assert!(
        bounds.y >= 0.0 && bounds.y + bounds.height <= min.height,
        "the form runs off a {min:?} window: {bounds:?}"
    );
    assert!(
        content.height > bounds.height,
        "at the minimum size the form must scroll: {content:?} in {bounds:?}"
    );
    let wheel = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -400.0 },
    })];
    let (_, scrolled) = form(cache, &mut renderer, &wheel);
    let (_, _, offset) = scrolled.expect("the settings form is on screen");
    assert!(
        offset.y > 0.0,
        "the wheel did not reach the rest of the form"
    );
}

/// Settings shows one feature's page at a time: its list names every page,
/// a click on one shows that page alone, and a flow that needs a page (voice
/// without a model) opens Settings on it.
#[test]
fn settings_shows_one_page_at_a_time() {
    use crate::SettingsPage;
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    {
        let mut sim = sim_of(&app);
        for page in SettingsPage::ALL {
            assert!(
                shows(&mut sim, page.title()),
                "the list misses {}",
                page.title()
            );
        }
        assert!(shows(&mut sim, "Light theme"));
        assert!(!shows(&mut sim, "Base URL"), "two pages at once");
    }
    let _ = app.update(Message::Settings(crate::SettingsMsg::Page(
        SettingsPage::LanguageModel,
    )));
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Base URL"));
        assert!(!shows(&mut sim, "Light theme"));
    }
    // Closed and opened again, it is where it was.
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    assert_eq!(app.settings.page, SettingsPage::LanguageModel);
    // Shortcuts are a page too, with the fixed reading motions on it.
    let _ = app.update(Message::Settings(crate::SettingsMsg::Page(
        SettingsPage::Shortcuts,
    )));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Click a shortcut, then press new keys"));
    assert!(shows(&mut sim, "Commands"));
}

/// Each Settings page opens at its top: the pages share one scroll, and
/// the Integrations page opened where the Voice page was left — at its
/// foot, mid-form. The page shown again, it stays where it is.
#[test]
fn another_settings_page_opens_at_its_top() {
    use crate::SettingsPage;
    use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, scroll_to};
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        SettingsPage::Voice,
    )));
    let mut renderer = renderer();
    let offset = |ui: &mut UserInterface<'_, Message, iced::Theme, iced::Renderer>,
                  renderer: &iced::Renderer| {
        let mut probe = ScrollProbe {
            target: super::settings_scroll_id(),
            found: None,
        };
        ui.operate(renderer, &mut black_box(&mut probe));
        probe.found.expect("the Settings page's scroll").y
    };
    let mut ui = UserInterface::build(super::view(&app), WINDOW, Cache::default(), &mut renderer);
    let end = AbsoluteOffset {
        x: Some(0.0),
        y: Some(f32::MAX),
    };
    let _ = run_operation::<()>(
        &mut ui,
        &renderer,
        scroll_to(super::settings_scroll_id(), end),
    );
    assert!(offset(&mut ui, &renderer) > 0.0, "the Voice page scrolls");
    let mut cache = ui.into_cache();
    for (page, top) in [
        (SettingsPage::Voice, false),
        (SettingsPage::Integrations, true),
    ] {
        let task = app.update(Message::Settings(crate::SettingsMsg::Page(page)));
        let mut ui = UserInterface::build(super::view(&app), WINDOW, cache, &mut renderer);
        for op in widget_ops(task) {
            let _ = run_operation::<()>(&mut ui, &renderer, op);
        }
        assert_eq!(offset(&mut ui, &renderer) == 0.0, top, "{page:?}");
        cache = ui.into_cache();
    }
}

/// A16 / E2-6: Enter in a field of the Settings or Connect form submits it,
/// like the button: Save, and Connect.
#[test]
fn enter_in_a_form_field_submits_it() {
    use iced_test::selector::Candidate;
    let enter_in = |app: &App, field: &str| -> Vec<Message> {
        let mut sim = sim_of(app);
        let wanted = field.to_string();
        let bounds = sim
            .find(move |c: Candidate<'_>| match c {
                Candidate::TextInput {
                    visible_bounds,
                    state,
                    ..
                } if state.text() == wanted => visible_bounds,
                _ => None,
            })
            .unwrap_or_else(|_| panic!("no field showing {field:?}"));
        sim.point_at(bounds.center());
        let _ = sim.simulate(iced_test::simulator::click());
        let _ = sim.tap_key(keyboard::Key::Named(keyboard::key::Named::Enter));
        sent_by(sim).into_iter().collect()
    };
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::LanguageModel,
    )));
    app.settings.model = "model-under-test".into();
    let sent = enter_in(&app, "model-under-test");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Settings(crate::SettingsMsg::Saved))),
        "{sent:?}"
    );
    let mut app = reader_app();
    let _ = app.update(Message::Connect(ConnectMsg::Open));
    app.connect.as_mut().unwrap().host = "example.com".into();
    let sent = enter_in(&app, "example.com");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Connect(ConnectMsg::Submit))),
        "{sent:?}"
    );
}

/// L9: while an AI field differs from what Settings opened on, its page
/// says that it is not saved yet and is saved as Settings closes — by ×,
/// Esc or a click outside — and offers to discard it; with none changed,
/// neither.
#[test]
fn settings_say_what_closing_keeps_and_offer_to_discard_it() {
    let mut app = reader_app();
    for page in [
        crate::SettingsPage::LanguageModel,
        crate::SettingsPage::Embeddings,
    ] {
        let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(page)));
        let _ = app.update(Message::Settings(crate::SettingsMsg::Discard));
        assert!(!shows(&mut sim_of(&app), "Not saved yet"), "{page:?}");
        assert!(!shows(&mut sim_of(&app), "Discard changes"), "{page:?}");
        app.settings.embed_key = "typed".into();
        app.settings.model = "typed".into();
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Not saved yet"), "{page:?}");
        assert!(shows(&mut sim, "Saved when Settings closes."), "{page:?}");
        assert!(
            matches!(
                click(sim_of(&app), "Discard changes").as_slice(),
                [Message::Settings(crate::SettingsMsg::Discard)]
            ),
            "{page:?}"
        );
    }
}

/// A16 / E2-11: an import map with nothing in it yet says the project is
/// being indexed, not that it has no imports; and a running Explain All
/// shows its progress in one place — the status bar's chip — with the
/// refresh chip out of the way.
#[test]
fn indexing_and_explain_progress_say_what_is_happening_once() {
    let imports_say = |app: &App, needle: &str| {
        let mut sim = sim_elem(super::project_imports_body(app));
        shows(&mut sim, needle)
    };
    let mut app = reader_app();
    app.proj.indexing = true;
    assert!(imports_say(&app, "Indexing the project…"));
    assert!(!imports_say(&app, "No imports found"));
    app.proj.indexing = false;
    // The index is in; the import job that resolves it has not landed.
    app.proj.import_work.running = true;
    assert!(imports_say(&app, "Resolving imports…"));
    assert!(!imports_say(&app, "No imports found"));
    app.proj.import_work.running = false;
    assert!(imports_say(&app, "No imports found in this project."));

    // A refresh chip would show here (a key, something explained) — but not
    // while the pass it would start is running.
    app.llm_available = true;
    app.proj.explain.cache.insert(
        crate::explain::Node::File(PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs")),
        crate::explain::Cached {
            summary: "s".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    {
        // Idle, it says what is current: the explanations — not "Up to date" a
        // second time beside the scan's own.
        let mut sim = sim_of(&app);
        assert!(
            shows(&mut sim, "No changes to explain"),
            "the refresh chip, idle"
        );
        assert!(
            text_at(&mut sim, |t| t.contains("Up to date"), 1).is_none(),
            "\"Up to date\" twice"
        );
        // A click asks first: the Explain sheet, with what it would cost,
        // not a pass of every function not yet explained.
        let sent = click(sim_of(&app), "↻ No changes to explain");
        assert!(
            sent.iter().any(|m| matches!(
                m,
                Message::Project(ProjectMsg::SetupShow(crate::app::setup::SetupMode::Explain))
            )),
            "the chip opens the Explain sheet: {sent:?}"
        );
        assert!(
            !sent
                .iter()
                .any(|m| matches!(m, Message::Explain(ExplainMsg::RefreshAll))),
            "the chip ran a pass with no word of its cost"
        );
    }
    {
        // Not kept fresh by itself, changed code is said to be out of date.
        app.keep_explanations_fresh = false;
        app.proj
            .explain
            .changed_sources
            .insert(PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"));
        {
            let mut sim = sim_of(&app);
            assert!(shows(&mut sim, "Explanations out of date"));
            assert!(!shows(&mut sim, "No changes to explain"));
        }
        app.keep_explanations_fresh = true;
        app.proj.explain.changed_sources.clear();
    }
    app.proj.explain.running = true;
    app.proj.explain.progress = Some((3, 9));
    let mut sim = sim_of(&app);
    assert!(text_at(&mut sim, |t| t.contains("3/9"), 0).is_some());
    assert!(
        text_at(&mut sim, |t| t.contains("3/9"), 1).is_none(),
        "the pass's progress is shown twice"
    );
    assert!(
        !shows(&mut sim, "↻"),
        "the refresh chip shows beside the pass"
    );
}

/// The same for an empty call graph: the local one is linked from the symbol
/// index, so an empty graph says the project is being indexed until the
/// index is in, and only then that there are no functions.
#[test]
fn an_empty_call_graph_says_what_is_happening() {
    let calls_say = |app: &App, needle: &str| {
        let mut sim = sim_elem(super::project_calls_body(app));
        shows(&mut sim, needle)
    };
    let mut app = reader_app();
    app.proj.indexing = true;
    assert!(calls_say(&app, "Indexing the project…"));
    assert!(!calls_say(&app, "No functions found"));
    app.proj.project_calls.building = true;
    assert!(calls_say(&app, "Building call graph…"));
    app.proj.project_calls.building = false;
    app.proj.indexing = false;
    assert!(calls_say(&app, "No functions found in this project."));
}

/// A17 / E2-9: the tutorial's spotlight regions start under what is DRAWN
/// above them — the toolbar and, when one shows, the update banner, which
/// pushes the body down — checked against where the sidebar's first tab and
/// the banner's text land on screen, not against the formula the regions are
/// computed with.
#[test]
fn the_tutorial_regions_start_under_what_is_drawn_above_them() {
    let mut app = reader_app();
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    app.show_left_sidebar = true;
    app.show_bottom = false;
    let check = |app: &App| {
        let region = super::region_rect(app, crate::app::tutorial::Anchor::FileTools)
            .expect("the sheet's header is shown");
        let mut sim = sim_of(app);
        let name = text_at(&mut sim, |t| t == "lib.rs", 0).expect("the sheet's header");
        assert!(
            name.y >= region.y && name.y - region.y <= super::SHEET_HEADER_ROW_H,
            "the region starts at {} but the header's name is drawn at {}",
            region.y,
            name.y
        );
        let banner = text_at(&mut sim, |t| t.contains("is available"), 0);
        (region, banner)
    };
    let (_, banner) = check(&app);
    assert!(banner.is_none());
    app.update.available = Some(crate::AvailableUpdate {
        version: clew_core::update::Version {
            major: 9,
            minor: 9,
            patch: 9,
        },
        dmg_url: None,
        notes: Vec::new(),
    });
    let (region, banner) = check(&app);
    let banner = banner.expect("the update banner is drawn");
    assert!(
        region.y >= banner.y + banner.height,
        "the spotlight region covers the update banner"
    );
}

/// A download in progress can be stopped from the banner that shows it: its
/// Cancel sits beside the progress, and a click on it is the app's Cancel.
/// Nothing could stop an update download while clew ran.
#[test]
fn a_download_in_progress_is_cancelled_from_its_banner() {
    let mut app = blank_app();
    app.update.available = Some(crate::AvailableUpdate {
        version: clew_core::update::Version {
            major: 9,
            minor: 9,
            patch: 9,
        },
        dmg_url: None,
        notes: Vec::new(),
    });
    app.update.phase = crate::UpdatePhase::Downloading;
    app.update.progress = Some((1 << 20, Some(4 << 20)));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Downloading Clew 9.9.9"));
    let sent = click(sim, "Cancel");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Updater(crate::UpdaterMsg::CancelDownload)]
        ),
        "{sent:?}"
    );
}

/// The Files view marks what the working tree changes: a file by git's
/// letter for it, a folder holding a change by a dot; an unchanged one by
/// nothing.
#[test]
fn the_files_view_marks_what_changed() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    let tree = crate::fs_scan::DirNode {
        files: vec!["README.md".into()],
        dirs: vec![(
            "src".into(),
            crate::fs_scan::DirNode {
                dirs: vec![],
                files: vec!["lib.rs".into(), "new.rs".into(), "same.rs".into()],
            },
        )],
    };
    app.proj.project.as_mut().unwrap().tree = tree;
    app.proj.expanded.insert("src".into());
    app.proj.git_status = [
        ("src/lib.rs".to_string(), 'M'),
        ("src/new.rs".to_string(), 'U'),
    ]
    .into_iter()
    .collect();
    let mut sim = sim_of(&app);
    let row_of = |sim: &mut Sim<'_>, name: &str| text_at(sim, |t| t == name, 0).expect(name);
    let lib = row_of(&mut sim, "lib.rs");
    let new = row_of(&mut sim, "new.rs");
    let m = text_at(&mut sim, |t| t == "M", 0).expect("a mark for the change");
    let u = text_at(&mut sim, |t| t == "U", 0).expect("a mark for the new file");
    assert!(
        (m.center_y() - lib.center_y()).abs() < 2.0,
        "on lib.rs's row"
    );
    assert!(
        (u.center_y() - new.center_y()).abs() < 2.0,
        "on new.rs's row"
    );
    let src = row_of(&mut sim, "src");
    let dot = text_at(&mut sim, |t| t == "●", 0).expect("the folder holding them");
    assert!((dot.center_y() - src.center_y()).abs() < 2.0);
    assert!(
        text_at(&mut sim, |t| t == "●", 1).is_none(),
        "the top holds no change"
    );
}

/// The Files view's tree takes the keyboard as a field does: a press on it
/// gives it the keys, and the keys that walk a tree are its moves, going no
/// further; a press elsewhere takes the keys away, and so does the code
/// having them (a file opened from the tree is read with `j`, as before).
/// ↓ in the filter above goes down to it.
#[test]
fn the_files_tree_takes_the_keyboard_as_a_field_does() {
    use crate::app::message::TreeKey;
    use iced::event::Status;
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.code_focused = false;
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        files: vec!["notes.txt".into()],
        dirs: vec![(
            "src".into(),
            crate::fs_scan::DirNode {
                dirs: vec![],
                files: vec!["lib.rs".into()],
            },
        )],
    };
    let down = || Key::Named(Named::ArrowDown);
    {
        let mut sim = sim_of(&app);
        assert_eq!(sim.tap_key(down()), Status::Ignored, "not the tree's yet");
        let src = text_at(&mut sim, |t| t == "src", 0).expect("the folder's row");
        sim.point_at(src.center());
        let _ = sim.simulate(iced_test::simulator::click());
        assert_eq!(sim.tap_key(down()), Status::Captured, "the tree's now");
        assert_eq!(sim.tap_key(Key::Character("p".into())), Status::Captured);
        // Away from the tree, the keys are no longer its.
        // The code, in the window's middle.
        sim.point_at(Point::new(WINDOW.width / 2.0, WINDOW.height / 2.0));
        let _ = sim.simulate(iced_test::simulator::click());
        assert_eq!(sim.tap_key(down()), Status::Ignored);
        let sent = sent_by(sim);
        let tree_keys: Vec<_> = sent
            .iter()
            .filter_map(|m| match m {
                Message::Window(WindowMsg::TreeKey(k)) => Some(*k),
                _ => None,
            })
            .collect();
        assert_eq!(tree_keys, [TreeKey::Down, TreeKey::Type('p')]);
        assert!(
            sent.iter()
                .any(|m| matches!(m, Message::Window(WindowMsg::TreeFocused(true))))
        );
        assert!(
            sent.iter()
                .any(|m| matches!(m, Message::Window(WindowMsg::TreeFocused(false))))
        );
    }
    // The code has the keys (a file opened from the tree): the tree lets
    // them go.
    app.code_focused = true;
    {
        let mut sim = sim_of(&app);
        let src = text_at(&mut sim, |t| t == "src", 0).expect("the folder's row");
        sim.point_at(src.center());
        let _ = sim.simulate(iced_test::simulator::click());
        assert_eq!(sim.tap_key(down()), Status::Ignored, "the code's");
    }
    // ↓ in the filter goes down to the tree.
    let mut sim = sim_of(&app);
    // The filter, above the tree's first row: a row's half, the gap, the
    // field's half.
    let src = text_at(&mut sim, |t| t == "src", 0).expect("the folder's row");
    sim.point_at(Point::new(
        src.x + 10.0,
        src.center_y() - super::TREE_ROW_H / 2.0 - 12.0 - 15.0,
    ));
    let _ = sim.simulate(iced_test::simulator::click());
    assert_eq!(sim.tap_key(down()), Status::Captured);
    assert!(
        sent_by(sim)
            .iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::FileFilterDown)))
    );
}

/// `app`'s window with the widget `id` holding the keys, then `events`:
/// how each event was taken, and what the window sent.
fn keys_held_by(app: &App, id: Id, events: &[Event]) -> (Vec<iced::event::Status>, Vec<Message>) {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(super::view(app), WINDOW, Cache::default(), &mut renderer);
    let _ = run_operation(
        &mut ui,
        &renderer,
        iced::advanced::widget::operation::focusable::focus::<()>(id),
    );
    let mut sent = Vec::new();
    let (_, statuses) = ui.update(
        events,
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut sent,
    );
    (statuses, sent)
}

/// A key pressed, as the window gets it: `key`, typing `typed`.
fn press(key: keyboard::Key, typed: keyboard::Key, modifiers: keyboard::Modifiers) -> Event {
    let text = match typed.as_ref() {
        keyboard::Key::Character(c) => Some(c.into()),
        _ => None,
    };
    Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modified_key: typed,
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers,
        text,
        repeat: false,
    })
}

/// A Files view with a folder and a file, for the tree's keys.
fn tree_app() -> App {
    let mut app = reader_app();
    // No repository at a path that is not there: no marks to read.
    app.proj.git_status_stale = false;
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.code_focused = false;
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        files: vec!["notes.txt".into()],
        dirs: vec![(
            "src".into(),
            crate::fs_scan::DirNode {
                dirs: vec![],
                files: vec!["lib.rs".into()],
            },
        )],
    };
    app
}

/// A dialog over the window holds the keys, though the tree had them: its
/// keys are not the tree's — an Esc closes the dialog, a letter goes
/// nowhere near the filter under it.
#[test]
fn the_tree_lets_the_keys_go_to_a_dialog_over_it() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    let none = keyboard::Modifiers::empty();
    let down = || {
        press(
            Key::Named(Named::ArrowDown),
            Key::Named(Named::ArrowDown),
            none,
        )
    };
    let (statuses, sent) = keys_held_by(&app, super::files_tree_keys_id(), &[down()]);
    assert_eq!(
        statuses,
        [iced::event::Status::Captured],
        "the tree's: {sent:?}"
    );
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let x = press(Key::Character("x".into()), Key::Character("x".into()), none);
    let (statuses, sent) = keys_held_by(&app, super::files_tree_keys_id(), &[down(), x]);
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::TreeKey(_)))),
        "{sent:?}"
    );
    assert!(statuses.iter().all(|s| *s == iced::event::Status::Ignored));
    // So Esc reaches the dialog, and closes it.
    type_key(&mut app, Key::Named(Named::Escape), none);
    assert!(!app.settings.open, "closed by the first Esc");
}

/// The tree's keys are the reading keys too — `j` `k` down and up, `h` out,
/// `l` in — and a character typed goes to the filter as typed, Shift and
/// all.
#[test]
fn the_tree_walks_by_the_reading_keys_and_types_as_typed() {
    use crate::app::message::TreeKey;
    use keyboard::Key;
    let none = keyboard::Modifiers::empty();
    let ch = |c: &str| Key::Character(c.into());
    let key = |k: &str, typed: &str, m| super::tree_keys::tree_key(&ch(k), &ch(typed), m);
    assert_eq!(key("j", "j", none), Some(TreeKey::Down));
    assert_eq!(key("k", "k", none), Some(TreeKey::Up));
    assert_eq!(key("h", "h", none), Some(TreeKey::Left));
    assert_eq!(key("l", "l", none), Some(TreeKey::Right));
    assert_eq!(
        key("j", "J", keyboard::Modifiers::SHIFT),
        Some(TreeKey::Type('J'))
    );
    assert_eq!(
        key("-", "_", keyboard::Modifiers::SHIFT),
        Some(TreeKey::Type('_')),
        "Shift and all"
    );
    assert_eq!(key("p", "p", none), Some(TreeKey::Type('p')));
    assert_eq!(
        key("j", "j", keyboard::Modifiers::COMMAND),
        None,
        "a shortcut"
    );
}

/// ↓ in a filter that lists nothing has no tree to go down to: the filter
/// keeps the keys, and F6 passes the tree by.
#[test]
fn an_empty_tree_is_not_gone_down_to() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    let down = press(
        Key::Named(Named::ArrowDown),
        Key::Named(Named::ArrowDown),
        keyboard::Modifiers::empty(),
    );
    let (_, sent) = keys_held_by(&app, super::file_filter_id(), std::slice::from_ref(&down));
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::FileFilterDown))),
        "a tree to go to: {sent:?}"
    );
    app.file_filter = "zzz".into();
    let (statuses, sent) = keys_held_by(&app, super::file_filter_id(), &[down]);
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::FileFilterDown))),
        "{sent:?}"
    );
    assert_eq!(statuses, [iced::event::Status::Ignored]);
    // F6 from the filter goes on to the chat.
    app.region = crate::app::keys::Region::Sidebar;
    type_key(
        &mut app,
        Key::Named(Named::F6),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.region, crate::app::keys::Region::Chat);
}

/// A window run as the runtime runs it: events through its widgets, what
/// they send — and the keys none took, as the window's key handler gets
/// them — through the app, the app's widget operations run on the window
/// built anew, the widgets' state kept from build to build.
struct Driven {
    renderer: iced::Renderer,
    cache: Option<Cache>,
    size: Size,
}

impl Driven {
    fn new(app: &App) -> Driven {
        Driven::sized(app, WINDOW)
    }

    fn sized(app: &App, size: Size) -> Driven {
        let mut driven = Driven {
            renderer: renderer(),
            cache: Some(Cache::default()),
            size,
        };
        driven.build(app);
        driven
    }

    /// How far the scrollable `id` is scrolled.
    fn scrolled(&mut self, app: &App, id: Id) -> Option<Vector> {
        let mut ui = UserInterface::build(
            super::view(app),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        let mut probe = ScrollProbe {
            target: id,
            found: None,
        };
        ui.operate(&self.renderer, &mut probe);
        self.cache = Some(ui.into_cache());
        probe.found
    }

    fn build(&mut self, app: &App) {
        let ui = UserInterface::build(
            super::view(app),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        self.cache = Some(ui.into_cache());
    }

    /// `events`, then a redraw; how each of `events` was taken.
    fn send(&mut self, app: &mut App, events: &[Event]) -> Vec<iced::event::Status> {
        let mut all = events.to_vec();
        all.push(Event::Window(window::Event::RedrawRequested(
            std::time::Instant::now(),
        )));
        let mut ui = UserInterface::build(
            super::view(app),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        let mut sent = Vec::new();
        let (_, statuses) = ui.update(
            &all,
            mouse::Cursor::Unavailable,
            &mut self.renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        let mut cache = ui.into_cache();
        for (event, status) in all.iter().zip(&statuses) {
            if let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
                let escape = matches!(
                    key.as_ref(),
                    keyboard::Key::Named(keyboard::key::Named::Escape)
                );
                if *status == iced::event::Status::Ignored || escape {
                    sent.push(Message::Editor(EditorMsg::KeyPressed(
                        key.clone(),
                        *modifiers,
                    )));
                }
            }
        }
        for message in sent {
            let ops = ready_ops(app.update(message));
            let mut ui =
                UserInterface::build(super::view(app), self.size, cache, &mut self.renderer);
            for mut op in ops {
                loop {
                    ui.operate(&self.renderer, &mut black_box(&mut *op));
                    match op.finish() {
                        Outcome::Chain(next) => op = next,
                        _ => break,
                    }
                }
            }
            cache = ui.into_cache();
        }
        self.cache = Some(cache);
        self.build(app);
        statuses[..events.len()].to_vec()
    }

    /// `key` pressed, plainly or with `modifiers`, then a redraw (which
    /// tells the app where the ring went).
    fn key(
        &mut self,
        app: &mut App,
        key: keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> iced::event::Status {
        let status = self.send(app, &[press(key.clone(), key, modifiers)])[0];
        let _ = self.send(app, &[]);
        status
    }

    fn tab(&mut self, app: &mut App, back: bool) -> iced::event::Status {
        let modifiers = if back {
            keyboard::Modifiers::SHIFT
        } else {
            keyboard::Modifiers::empty()
        };
        self.key(
            app,
            keyboard::Key::Named(keyboard::key::Named::Tab),
            modifiers,
        )
    }

    /// The widget holding the keys.
    fn focused(&mut self, app: &App) -> Option<Id> {
        let mut ui = UserInterface::build(
            super::view(app),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        let found = run_operation(&mut ui, &self.renderer, find_focused());
        self.cache = Some(ui.into_cache());
        found
    }

    /// `id` given the keys, as a click on it gives them.
    fn focus(&mut self, app: &App, id: Id) {
        let mut ui = UserInterface::build(
            super::view(app),
            self.size,
            self.cache.take().unwrap_or_default(),
            &mut self.renderer,
        );
        let _ = run_operation(
            &mut ui,
            &self.renderer,
            iced::advanced::widget::operation::focusable::focus::<()>(id),
        );
        self.cache = Some(ui.into_cache());
    }
}

/// The widget operations `task` asks for at once — what waits (a file
/// read, an operation's answer) is left, as a turn of the runtime leaves
/// it for later.
fn ready_ops(task: iced::Task<Message>) -> Vec<Box<dyn Operation>> {
    use iced_test::futures::futures::{FutureExt, StreamExt};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let _inside = runtime.enter();
    let Some(mut stream) = iced_test::runtime::task::into_stream(task) else {
        return Vec::new();
    };
    let mut ops = Vec::new();
    while let Some(Some(action)) = stream.next().now_or_never() {
        if let iced_test::runtime::Action::Widget(op) = action {
            ops.push(op);
        }
    }
    ops
}

/// Where the window's ring is: its place, how many stops, what it presses.
fn ring_at(app: &App) -> (Option<usize>, usize, Option<Message>) {
    crate::ui::jump::ring_placed(app.main_window).expect("the ring placed")
}

/// A control built as the window is laid out — in a `responsive`, as the
/// status bar's chips are — is a stop of its own: the ring on it presses
/// it, not the first control built with the view (whose number it took).
#[test]
fn a_control_built_as_laid_out_presses_itself() {
    use iced::widget::{button, column, responsive, text};
    let view = |ask: super::RingAsk| -> Element<'static, Message> {
        super::jump::begin(true);
        let early = Message::Nav(NavMsg::PaletteOpened);
        let first = super::jump::mark(button(text("early")).on_press(early.clone()), early);
        let second = responsive(|_| {
            let late = Message::Project(ProjectMsg::SetUpMissing);
            super::jump::mark(button(text("late")).on_press(late.clone()), late)
        });
        super::jump::root(
            column![first, second].into(),
            Some(super::jump::Up::Ring(ask)),
            false,
            None,
            true,
            super::jump::OnTop {
                layer: super::heard::Layer::Ask,
                scene: None,
            },
        )
    };
    let mut renderer = renderer();
    let size = Size::new(400.0, 300.0);
    super::jump::forget(None);
    // The first Tab puts the ring on the first control...
    let ui = UserInterface::build(
        view(super::RingAsk::Start { forward: true }),
        size,
        Cache::default(),
        &mut renderer,
    );
    let (at, count, pressed) = super::jump::ring_placed(None).expect("the ring placed");
    assert_eq!((at, count), (Some(0), 2), "both controls are stops");
    assert!(
        matches!(pressed, Some(Message::Nav(NavMsg::PaletteOpened))),
        "{pressed:?}"
    );
    let first = super::jump::ring_target(None);
    // ...and the next goes on to the one built as it was laid out.
    let mut ui = UserInterface::build(
        view(super::RingAsk::Kept),
        size,
        ui.into_cache(),
        &mut renderer,
    );
    let tab = keyboard::Key::Named(keyboard::key::Named::Tab);
    let mut sent = Vec::new();
    let _ = ui.update(
        &[press(tab.clone(), tab, keyboard::Modifiers::empty())],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut sent,
    );
    let (at, _, pressed) = super::jump::ring_placed(None).expect("the ring placed");
    assert_eq!(at, Some(1));
    assert!(
        matches!(pressed, Some(Message::Project(ProjectMsg::SetUpMissing))),
        "its own press, not the first control's: {pressed:?}"
    );
    // Another stop, by another number: the ring is drawn round it alone.
    assert_ne!(super::jump::ring_target(None), first);
    super::jump::forget(None);
}

/// Tab, no field holding the keys, rings the window's first stop — not its
/// title bar's, which come last — and goes from stop to stop: buttons and
/// fields alike, the filter field and the tree each one, Shift-Tab back.
/// A field reached holds the keys; a control reached is named in the status
/// bar, and Enter presses it as a click does, the ring staying.
#[test]
fn tab_walks_the_controls_and_fields_and_enter_presses() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    app.code_focused = true;
    let mut window = Driven::new(&app);
    assert_eq!(window.tab(&mut app, false), iced::event::Status::Captured);
    assert!(app.ring.is_some(), "up");
    let (at, count, first) = ring_at(&app);
    assert_eq!(at, Some(0));
    assert!(count > 10, "{count} stops");
    assert!(
        !matches!(first, Some(Message::Window(WindowMsg::ToggleAppMenu))),
        "the title bar's come last: {first:?}"
    );
    // Back from the first: the last, the title bar's.
    window.tab(&mut app, true);
    assert_eq!(ring_at(&app).0, Some(count - 1));
    window.tab(&mut app, false);
    // On, to the filter: it holds the keys; then the tree, one stop.
    let mut reached = Vec::new();
    for _ in 0..count {
        window.tab(&mut app, false);
        let held = window.focused(&app);
        if let Some(id) = held.clone() {
            reached.push(id);
        }
        if held == Some(super::files_tree_keys_id()) {
            break;
        }
    }
    assert_eq!(
        reached,
        [super::file_filter_id(), super::files_tree_keys_id()],
        "the filter, then the tree"
    );
    assert!(
        ring_at(&app).2.is_none(),
        "a field is not pressed by the ring"
    );
    // Past the tree: not one of its rows (its own keys walk them).
    window.tab(&mut app, false);
    let past = format!("{:?}", ring_at(&app).2);
    assert!(
        !past.contains("OpenRel") && !past.contains("ToggleDir"),
        "a row of the tree: {past}"
    );
    window.tab(&mut app, true);
    window.tab(&mut app, true);
    assert_eq!(window.focused(&app), Some(super::file_filter_id()), "back");
    // On to the strip's Weak Spots: named, then pressed.
    for _ in 0..count {
        if matches!(ring_at(&app).2, Some(Message::Weak(crate::WeakMsg::Show))) {
            break;
        }
        window.tab(&mut app, false);
    }
    assert_eq!(window.focused(&app), None, "no field holds the keys");
    assert!(!app.status.is_empty(), "named: {}", app.status);
    window.key(
        &mut app,
        Key::Named(Named::Enter),
        keyboard::Modifiers::empty(),
    );
    assert!(app.proj.weak.showing, "pressed");
    assert!(app.ring.is_some(), "the ring stays");
    assert!(
        matches!(ring_at(&app).2, Some(Message::Weak(crate::WeakMsg::Show))),
        "on what it pressed: {:?}",
        ring_at(&app).2
    );
}

/// The ring stays on the control it pressed, wherever the press moved it:
/// the sidebar's toggle, pressed, takes the sidebar's stops from before it,
/// and the ring is still on the toggle — not on the stop now where it was
/// (the window's Close, past the end), whose Enter would close the window.
#[test]
fn the_ring_stays_on_what_it_pressed() {
    use keyboard::Key;
    use keyboard::key::Named;
    let none = keyboard::Modifiers::empty();
    let mut app = tree_app();
    app.show_left_sidebar = true;
    app.code_focused = true;
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    let toggle = format!("{:?}", Some(Message::Window(WindowMsg::ToggleLeftSidebar)));
    for _ in 0..count {
        if format!("{:?}", ring_at(&app).2) == toggle {
            break;
        }
        window.tab(&mut app, false);
    }
    assert_eq!(format!("{:?}", ring_at(&app).2), toggle, "reached");
    window.key(&mut app, Key::Named(Named::Enter), none);
    assert!(!app.show_left_sidebar, "hidden");
    assert_eq!(format!("{:?}", ring_at(&app).2), toggle, "still on it");
    window.key(&mut app, Key::Named(Named::Enter), none);
    assert!(app.show_left_sidebar, "shown again, not the window closed");
    assert_eq!(format!("{:?}", ring_at(&app).2), toggle, "and still on it");
}

/// The messages `task` sends, run to its end: for a task that ends by
/// itself (a message sent on), never one that waits on the world.
fn task_sends(task: iced::Task<Message>) -> Vec<Message> {
    use iced_test::futures::futures::StreamExt;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let _inside = runtime.enter();
    let Some(stream) = iced_test::runtime::task::into_stream(task) else {
        return Vec::new();
    };
    let actions: Vec<_> = runtime.block_on(stream.collect());
    actions
        .into_iter()
        .filter_map(|action| match action {
            iced_test::runtime::Action::Output(message) => Some(message),
            _ => None,
        })
        .collect()
}

/// Enter on the window's Close, the ring on it, closes the window as a
/// click does: the press goes to the shell, which asks about unsaved edits
/// and lets the window go — not to the App, which takes no Close.
#[cfg(not(target_os = "macos"))]
#[test]
fn enter_on_the_rings_close_closes_the_window() {
    let none = keyboard::Modifiers::empty();
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.tab(&mut app, true);
    let (_, count, _) = ring_at(&app);
    for _ in 0..count {
        if matches!(ring_at(&app).2, Some(Message::Window(WindowMsg::Close))) {
            break;
        }
        window.tab(&mut app, true);
    }
    assert!(matches!(
        ring_at(&app).2,
        Some(Message::Window(WindowMsg::Close))
    ));
    let task = app.update(Message::Editor(EditorMsg::KeyPressed(
        keyboard::Key::Named(keyboard::key::Named::Enter),
        none,
    )));
    let sent = task_sends(task);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::Close))),
        "sent on, to the shell: {sent:?}"
    );
}

/// Esc puts the ring away, and the keys go back where they were — the
/// code, its reading keys at work again; so does any other key, doing what
/// it does.
#[test]
fn the_ring_put_away_gives_the_keys_back() {
    use keyboard::Key;
    use keyboard::key::Named;
    let none = keyboard::Modifiers::empty();
    let mut app = reader_app();
    app.code_focused = true;
    let line = |app: &App| app.active_viewer().unwrap().caret.map_or(0, |(l, _)| l);
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    assert!(!app.code_focused);
    window.key(&mut app, Key::Named(Named::Escape), none);
    assert_eq!(app.ring, None);
    assert!(app.code_focused, "the code's again");
    let before = line(&app);
    window.key(&mut app, Key::Character("j".into()), none);
    assert_eq!(line(&app), before + 1, "j reads on");
    // Another key: the ring goes, and the key does what it does.
    window.tab(&mut app, false);
    window.key(&mut app, Key::Character("j".into()), none);
    assert_eq!(app.ring, None);
    assert_eq!(line(&app), before + 2, "j read on, the ring gone");
}

/// A field taking the keys — a press that focuses one — puts the ring away:
/// its arrows are the field's, not the ring's.
#[test]
fn a_field_taking_the_keys_puts_the_ring_away() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    app.sidebar = crate::SidebarTab::Search;
    app.code_focused = true;
    let line = |app: &App| app.active_viewer().unwrap().caret.map_or(0, |(l, _)| l);
    let before = line(&app);
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    window.focus(&app, super::search_input_id());
    let status = window.key(
        &mut app,
        Key::Named(Named::ArrowDown),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.ring, None, "put away");
    // The keys are the field's: the arrow went nowhere near the code.
    assert!(!app.code_focused);
    assert_eq!(line(&app), before, "the code's caret stayed");
    assert_ne!(
        ring_at_or_none(&app),
        Some(1),
        "the arrow did not move it: {status:?}"
    );
}

fn ring_at_or_none(app: &App) -> Option<usize> {
    crate::ui::jump::ring_placed(app.main_window).and_then(|(at, _, _)| at)
}

/// Tab from a field the pointer gave the keys to goes on to the stop after
/// it — the filter's next is the tree.
#[test]
fn tab_from_a_field_goes_on_from_it() {
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.focus(&app, super::file_filter_id());
    window.tab(&mut app, false);
    assert_eq!(window.focused(&app), Some(super::files_tree_keys_id()));
    window.tab(&mut app, true);
    assert_eq!(
        window.focused(&app),
        Some(super::file_filter_id()),
        "and back"
    );
}

/// A dialog come up takes the ring to its own first stop, and only its
/// own; closed, the ring is back on what opened it.
#[test]
fn a_dialog_takes_the_ring_and_gives_it_back() {
    let mut app = reader_app();
    let mut window = Driven::new(&app);
    for _ in 0..4 {
        window.tab(&mut app, false);
    }
    let (under, window_count, press) = ring_at(&app);
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let _ = window.send(&mut app, &[]);
    let (at, count, _) = ring_at(&app);
    assert_eq!(at, Some(0), "the dialog's first");
    assert_ne!(count, window_count, "the dialog's own stops");
    window.tab(&mut app, false);
    assert_eq!(ring_at(&app).0, Some(1));
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));
    let _ = window.send(&mut app, &[]);
    let (back, _, back_press) = ring_at(&app);
    assert_eq!(back, under, "back on what it was on");
    assert_eq!(format!("{back_press:?}"), format!("{press:?}"));
}

/// Tab is each window's own: a field holding the keys in another window
/// does not keep this one from ringing its controls, and its ring going
/// round leaves the other's where it is.
#[test]
fn tab_is_each_windows_own() {
    let mut first = tree_app();
    first.main_window = Some(iced::window::Id::unique());
    let mut second = tree_app();
    second.main_window = Some(iced::window::Id::unique());
    let mut one = Driven::new(&first);
    let mut two = Driven::new(&second);
    one.focus(&first, super::file_filter_id());
    two.tab(&mut second, false);
    assert_eq!(
        second.ring,
        Some(crate::ui::RingAsk::Kept),
        "rung, from its own first stop"
    );
    assert_eq!(ring_at(&second).0, Some(0));
    // Both up: the second's Tab moves the second's alone.
    one.tab(&mut first, true);
    one.tab(&mut first, true);
    let (at, _, press) = ring_at(&first);
    for _ in 0..3 {
        two.tab(&mut second, false);
    }
    assert_eq!(ring_at(&second).0, Some(3));
    let (still, _, still_press) = ring_at(&first);
    assert_eq!(still, at, "the first window's ring stayed");
    assert_eq!(format!("{still_press:?}"), format!("{press:?}"));
}

/// A press of the pointer puts the ring away, the keys where it puts them.
#[test]
fn a_press_of_the_pointer_puts_the_ring_away() {
    let mut app = tree_app();
    app.code_focused = true;
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    assert!(app.ring.is_some());
    let _ = window.send(
        &mut app,
        &[Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))],
    );
    assert_eq!(app.ring, None, "put away");
}

/// The tutorial's keys are its own: Tab puts up no ring over it.
#[test]
fn the_tutorial_keeps_tab() {
    let mut app = reader_app();
    let _ = app.update(Message::Tutorial(crate::TutorialMsg::Begin));
    assert!(app.tutorial.is_some());
    let mut window = Driven::new(&app);
    assert_ne!(window.tab(&mut app, false), iced::event::Status::Captured);
    assert_eq!(app.ring, None);
}

/// Put away with Esc under a dialog the ring's control opened, the keys
/// are not the code's — its reading keys would act under the dialog — and
/// a control that gave the code the keys gives them for good: Esc then
/// leaves them there.
#[test]
fn the_keys_go_back_where_they_are_now() {
    use keyboard::Key;
    use keyboard::key::Named;
    let none = keyboard::Modifiers::empty();
    let line = |app: &App| app.active_viewer().unwrap().caret.map_or(0, |(l, _)| l);
    // A dialog came up over the code since the ring came up.
    let mut app = reader_app();
    app.code_focused = true;
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let _ = window.send(&mut app, &[]);
    window.key(&mut app, Key::Named(Named::Escape), none);
    assert_eq!(app.ring, None);
    assert!(app.settings.open, "the dialog stays");
    assert!(!app.code_focused, "not the code's, under the dialog");
    let before = line(&app);
    window.key(&mut app, Key::Character("j".into()), none);
    assert_eq!(line(&app), before, "j moved nothing under it");
    // The dialog's ×, pressed by the ring, gave the code the keys back.
    let mut app = reader_app();
    app.code_focused = true;
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    assert!(!app.code_focused);
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    for _ in 0..count {
        if matches!(
            ring_at(&app).2,
            Some(Message::Settings(crate::SettingsMsg::Close))
        ) {
            break;
        }
        window.tab(&mut app, false);
    }
    assert!(
        matches!(
            ring_at(&app).2,
            Some(Message::Settings(crate::SettingsMsg::Close))
        ),
        "on its ×: {:?}",
        ring_at(&app).2
    );
    window.key(&mut app, Key::Named(Named::Enter), none);
    assert!(!app.settings.open, "closed");
    window.key(&mut app, Key::Named(Named::Escape), none);
    assert_eq!(app.ring, None);
    assert!(app.code_focused, "the code's, as the × gave them");
    let before = line(&app);
    window.key(&mut app, Key::Character("j".into()), none);
    assert_eq!(line(&app), before + 1, "j reads on");
}

/// The ring come up from a field and put away by Esc gives the field the
/// keys back: they are not left nowhere.
#[test]
fn the_ring_put_away_gives_a_field_its_keys_back() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.focus(&app, super::file_filter_id());
    app.code_focused = false;
    window.tab(&mut app, true);
    assert_eq!(window.focused(&app), None, "back to a control: no field");
    window.key(
        &mut app,
        Key::Named(Named::Escape),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.ring, None);
    assert_eq!(
        window.focused(&app),
        Some(super::file_filter_id()),
        "the filter's again"
    );
}

/// The ring come up from a field, and a dialog over the field since: Esc
/// puts the ring away without giving the keys back to the field behind the
/// dialog — what was typed next went into it, unseen.
#[test]
fn esc_on_the_ring_gives_no_field_under_a_dialog_the_keys() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.focus(&app, super::file_filter_id());
    app.code_focused = false;
    window.tab(&mut app, true);
    assert_eq!(window.focused(&app), None, "on a control: no field");
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let _ = window.send(&mut app, &[]);
    window.key(
        &mut app,
        Key::Named(Named::Escape),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.ring, None);
    assert_ne!(
        window.focused(&app),
        Some(super::file_filter_id()),
        "the filter is under the dialog"
    );
}

/// The window changed under a dialog — the sidebar put away, its controls
/// with it — the ring is given back on the control it was on, found by
/// what it is, not on whatever came to stand at its old place.
#[test]
fn a_dialog_gone_gives_the_ring_back_on_the_same_control() {
    let mut app = tree_app();
    app.show_left_sidebar = true;
    let mut window = Driven::new(&app);
    // From the end: a control after the sidebar's.
    window.tab(&mut app, true);
    window.tab(&mut app, true);
    let (under, _, press) = ring_at(&app);
    assert!(press.is_some());
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let _ = window.send(&mut app, &[]);
    app.show_left_sidebar = false;
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));
    let _ = window.send(&mut app, &[]);
    let (back, _, back_press) = ring_at(&app);
    assert_ne!(back, under, "the sidebar's stops are gone: it moved up");
    assert_eq!(format!("{back_press:?}"), format!("{press:?}"));
}

/// A shortcut pressed with the ring up from a field puts the ring away and
/// does what it does, at once: Ctrl-P opened nothing, and had to be pressed
/// again.
#[test]
fn a_shortcut_with_the_ring_up_does_what_it_does() {
    use keyboard::Key;
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.focus(&app, super::file_filter_id());
    app.code_focused = false;
    window.tab(&mut app, true);
    assert!(app.ring.is_some());
    let p = Key::Character("p".into());
    let task = app.update(Message::Editor(EditorMsg::KeyPressed(
        p,
        keyboard::Modifiers::COMMAND,
    )));
    assert_eq!(app.ring, None);
    let next = task_sends(task);
    assert!(
        next.iter().any(|m| matches!(
            m,
            Message::Editor(EditorMsg::KeyPressed(Key::Character(c), mods))
                if c.as_str() == "p" && mods.command()
        )),
        "{next:?}"
    );
    for m in next {
        let _ = app.update(m);
    }
    assert!(app.proj.finder.open, "the command bar is up");
}

/// Page Up and Page Down, Home and End — and with Ctrl (Cmd) the file's
/// start and end — move in the code, as every editor reads by.
#[test]
fn page_home_and_end_keys_move_in_the_code() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = reader_app();
    app.code_focused = true;
    app.active_viewer_mut().unwrap().caret = Some((5, 3));
    let key = |app: &mut App, named: Named, modifiers: keyboard::Modifiers| {
        let _ = app.update(Message::Editor(EditorMsg::KeyPressed(
            Key::Named(named),
            modifiers,
        )));
    };
    let none = keyboard::Modifiers::empty();
    let caret = |app: &App| app.active_viewer().unwrap().caret.unwrap();
    let line_len = app.active_viewer().unwrap().line_len(5);
    key(&mut app, Named::End, none);
    assert_eq!(caret(&app), (5, line_len));
    key(&mut app, Named::Home, none);
    assert_eq!(caret(&app), (5, 0));
    let top = app.active_viewer().unwrap().scroll_y;
    key(&mut app, Named::PageDown, none);
    assert!(app.active_viewer().unwrap().scroll_y > top, "a page down");
    key(&mut app, Named::PageUp, none);
    assert_eq!(app.active_viewer().unwrap().scroll_y, top, "and back");
    key(&mut app, Named::End, keyboard::Modifiers::COMMAND);
    let last = app.active_viewer().unwrap().lines.len() - 1;
    assert_eq!(caret(&app).0, last, "the file's end");
    key(&mut app, Named::Home, keyboard::Modifiers::COMMAND);
    assert_eq!(caret(&app).0, 0, "the file's start");
    // Not while a field has the keys: they are its.
    app.code_focused = false;
    key(&mut app, Named::End, keyboard::Modifiers::COMMAND);
    assert_eq!(caret(&app).0, 0);
}

/// A fold key that folds nothing (`za` off any fold) leaves the view where
/// it was: it snapped the view to a row.
#[test]
fn a_fold_key_that_folds_nothing_leaves_the_view() {
    use keyboard::Key;
    let mut app = reader_app();
    app.code_focused = true;
    app.active_viewer_mut().unwrap().caret = Some((40, 0));
    app.active_viewer_mut().unwrap().scroll_y = 333.0;
    for c in ["z", "a"] {
        let _ = app.update(Message::Editor(EditorMsg::KeyPressed(
            Key::Character(c.into()),
            keyboard::Modifiers::empty(),
        )));
    }
    assert_eq!(app.active_viewer().unwrap().scroll_y, 333.0);
}

/// Ask made large holds the keys as a dialog does: the ring put away from
/// over it does not give them to the code under it.
#[test]
fn the_ring_put_away_over_a_large_ask_leaves_the_code_without_the_keys() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = reader_app();
    app.code_focused = true;
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    app.ask_view = crate::AskView::Expanded;
    let _ = window.send(&mut app, &[]);
    window.key(
        &mut app,
        Key::Named(Named::Escape),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.ring, None);
    assert!(!app.code_focused, "Ask, large, has them");
}

/// F6 opened the chat, then its × put it away: opened again by hand, F6
/// going on from it leaves it open — it put away the Ask the reader had
/// opened.
#[test]
fn f6_leaves_an_ask_opened_by_hand() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = reader_app();
    app.code_focused = true;
    let f6 = |app: &mut App| {
        let _ = app.update(Message::Editor(EditorMsg::KeyPressed(
            Key::Named(Named::F6),
            keyboard::Modifiers::empty(),
        )));
    };
    for _ in 0..6 {
        if app.ask_view.is_open() {
            break;
        }
        f6(&mut app);
    }
    assert!(app.ask_view.is_open(), "F6 came to the chat");
    let _ = app.update(Message::Ask(AskMsg::Hide));
    assert!(!app.ask_view.is_open());
    // Opened by hand, its field clicked into.
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    app.code_focused = false;
    assert!(app.ask_view.is_open());
    f6(&mut app);
    assert!(app.ask_view.is_open(), "the reader's Ask stays");
}

/// A control made an element after another was named keeps its own name:
/// the ring on it said none (the other's press was the one named last).
#[test]
fn a_control_made_an_element_late_keeps_its_own_name() {
    use iced::widget::{column, text};
    let view = |ask: super::RingAsk| -> Element<'static, Message> {
        super::jump::begin(true);
        let alpha = super::heard::said("Alpha", Message::Nav(NavMsg::PaletteOpened));
        let early = super::press::button(text("one")).on_press(alpha);
        let beta = super::heard::said("Beta", Message::Project(ProjectMsg::SetUpMissing));
        let second: Element<'static, Message> =
            super::press::button(text("two")).on_press(beta).into();
        let first: Element<'static, Message> = early.into();
        super::jump::root(
            column![first, second].into(),
            Some(super::jump::Up::Ring(ask)),
            false,
            None,
            true,
            super::jump::OnTop {
                layer: super::heard::Layer::Ask,
                scene: None,
            },
        )
    };
    let mut renderer = renderer();
    let size = Size::new(400.0, 300.0);
    super::jump::forget(None);
    let ui = UserInterface::build(
        view(super::RingAsk::Start { forward: false }),
        size,
        Cache::default(),
        &mut renderer,
    );
    let mut ui = UserInterface::build(
        view(super::RingAsk::Kept),
        size,
        ui.into_cache(),
        &mut renderer,
    );
    let tab = keyboard::Key::Named(keyboard::key::Named::Tab);
    let mut sent = Vec::new();
    let _ = ui.update(
        &[press(tab.clone(), tab, keyboard::Modifiers::empty())],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut sent,
    );
    assert_eq!(super::jump::ring_placed(None).unwrap().0, Some(0));
    let said: Vec<Option<String>> = sent
        .iter()
        .filter_map(|m| match m {
            Message::Window(WindowMsg::RingMoved(_, name)) => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(said, [Some("Alpha".to_string())]);
    super::jump::forget(None);
}

/// A dialog with no control of its own — the finder with nothing found —
/// keeps the ring in it, as one with controls does: Tab does not go on to
/// the window under it.
#[test]
fn a_dialog_with_no_controls_keeps_the_ring() {
    let mut app = reader_app();
    let _ = app.update(Message::Nav(NavMsg::FinderOpened(
        crate::session::finder::FinderMode::Files,
    )));
    let _ = app.update(Message::Nav(NavMsg::FinderQueryChanged(":".into())));
    let mut window = Driven::new(&app);
    window.focus(&app, super::finder_input_id());
    window.tab(&mut app, false);
    let (_, count, press) = ring_at(&app);
    assert_eq!(count, 1, "the finder's field alone");
    assert!(press.is_none(), "nothing under it: {press:?}");
    assert_eq!(window.focused(&app), Some(super::finder_input_id()));
}

/// Another dialog come up in the place of the one the ring was in takes
/// the ring to its own first stop.
#[test]
fn a_dialog_in_anothers_place_starts_the_ring_over() {
    let mut app = reader_app();
    app.show_app_menu = true;
    let mut window = Driven::new(&app);
    for _ in 0..4 {
        window.tab(&mut app, false);
    }
    assert_eq!(ring_at(&app).0, Some(3));
    // The menu's item opened a dialog in its place.
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
    let _ = window.send(&mut app, &[]);
    assert!(app.settings.open);
    let (at, _, press) = ring_at(&app);
    assert_eq!(at, Some(0), "the dialog's first stop: {press:?}");
}

/// The codes or the tour, begun while the ring is up — by a menu's
/// command, as the menu bar sends ⌘J on macOS — put the ring away: it does
/// not come back once they are done.
#[test]
fn the_codes_and_the_tour_put_the_ring_away() {
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let _ = app.on_menu_action(crate::keymap::Action::Jump);
    assert!(app.jump.is_some());
    assert_eq!(app.ring, None);
    // Any command of the menu bar's.
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let _ = app.on_menu_action(crate::keymap::Action::ZoomIn);
    assert_eq!(app.ring, None);
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let _ = app.update(Message::Tutorial(crate::TutorialMsg::Begin));
    assert_eq!(app.ring, None);
}

/// What else is said in the status bar while the ring is up stays when
/// the ring goes: only the name it said goes with it.
#[test]
fn the_ring_takes_away_only_what_it_said() {
    use keyboard::Key;
    use keyboard::key::Named;
    let none = keyboard::Modifiers::empty();
    let mut app = tree_app();
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    assert!(!app.status.is_empty(), "named");
    window.key(&mut app, Key::Named(Named::Escape), none);
    assert!(app.status.is_empty(), "its name went with it");
    window.tab(&mut app, false);
    app.status = "Could not save shortcuts".into();
    window.key(&mut app, Key::Named(Named::Escape), none);
    assert_eq!(app.status, "Could not save shortcuts");
}

/// Esc on a field the ring is on in a dialog puts the ring away, as it does
/// on a button there: it does not close the dialog as well.
#[test]
fn esc_on_a_ringed_field_puts_away_the_ring_alone() {
    use keyboard::Key;
    use keyboard::key::Named;
    let mut app = reader_app();
    let _ = app.update(Message::Connect(crate::ConnectMsg::Open));
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    // A field: a stop the ring presses nothing on.
    for _ in 0..count {
        if ring_at(&app).2.is_none() {
            break;
        }
        window.tab(&mut app, false);
    }
    assert!(ring_at(&app).2.is_none(), "on a field");
    window.key(
        &mut app,
        Key::Named(Named::Escape),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.ring, None, "put away");
    assert!(app.connect.is_some(), "the dialog stays");
}

/// Every control the ring reaches says its own name in the status bar —
/// a switch its name, not "On" or "Off"; one of two controls pressing the
/// same thing its own, not the other's.
#[test]
fn the_ring_says_each_controls_own_name() {
    let mut app = tree_app();
    app.code_focused = true;
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    let mut unnamed = Vec::new();
    for _ in 0..count {
        let (_, _, press) = ring_at(&app);
        if press.is_some() && app.status.is_empty() {
            unnamed.push(format!("{press:?}"));
        }
        window.tab(&mut app, false);
    }
    assert!(unnamed.is_empty(), "said no name: {unnamed:#?}");
    // A switch, by its name alone.
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Voice,
    )));
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    let mut said = Vec::new();
    for _ in 0..count {
        said.push(app.status.clone());
        window.tab(&mut app, false);
    }
    assert!(
        !said
            .iter()
            .any(|s| s.ends_with(": On") || s.ends_with(": Off")),
        "a switch said as one of its states: {said:#?}"
    );
    assert!(
        said.iter().any(|s| s == "Run spoken commands"),
        "a switch, by its name: {said:#?}"
    );
}

/// A control built as a dialog is laid out (a `responsive`'s) is in the
/// dialog: the ring there reaches it.
#[test]
fn a_control_laid_out_in_a_dialog_is_the_dialogs() {
    use iced::widget::{button, responsive, text};
    let view = |ask: super::RingAsk| -> Element<'static, Message> {
        super::jump::begin(true);
        let dialog = responsive(|_| {
            let late = Message::Project(ProjectMsg::SetUpMissing);
            super::jump::mark(button(text("late")).on_press(late.clone()), late)
        });
        super::jump::root(
            super::jump::scope(
                dialog,
                super::ring::ScopeTag::Layer(super::heard::Layer::Overlay),
            ),
            Some(super::jump::Up::Ring(ask)),
            false,
            None,
            true,
            super::jump::OnTop {
                layer: super::heard::Layer::Overlay,
                scene: Some("setup".into()),
            },
        )
    };
    let mut renderer = renderer();
    super::jump::forget(None);
    let _ui = UserInterface::build(
        view(super::RingAsk::Start { forward: true }),
        Size::new(400.0, 300.0),
        Cache::default(),
        &mut renderer,
    );
    let (at, count, pressed) = super::jump::ring_placed(None).expect("the ring placed");
    assert_eq!((at, count), (Some(0), 1), "the dialog's one control");
    assert!(matches!(
        pressed,
        Some(Message::Project(ProjectMsg::SetUpMissing))
    ));
    super::jump::forget(None);
}

/// Skim near a file's end leaves the code in view: the file folded short,
/// the view was left where it was, past its end — nothing but the last
/// line on screen. The function at the top stays at the top.
#[test]
fn skim_keeps_the_code_in_view() {
    let mut app = reader_app();
    let source: String = (0..120)
        .map(|i| format!("fn f{i}() {{\n    let a = {i};\n    let b = a;\n}}\n"))
        .collect();
    let lines = crate::highlight::plain_lines(&source);
    let abs = app.active_viewer().unwrap().abs.clone();
    app.proj.panes[0] = Some(Viewer::new(
        abs.clone(),
        "src/lib.rs".into(),
        Some("rust"),
        Arc::new(source),
        lines,
    ));
    app.proj.symbol_index_by_file.insert(
        abs.clone(),
        Arc::new(
            (0..120)
                .map(|i| crate::index::SymbolEntry {
                    name: format!("f{i}"),
                    kind: "function".into(),
                    rel: "src/lib.rs".into(),
                    abs: abs.clone(),
                    line: i * 4 + 1,
                    is_test: false,
                    entry: None,
                })
                .collect(),
        ),
    );
    let line_h = app.line_height();
    // Reading the last functions: f110 at the top.
    let v = app.active_viewer_mut().unwrap();
    v.scroll_y = 440.0 * line_h;
    v.caret = Some((441, 0));
    let _ = app.update(Message::Editor(EditorMsg::SkimFile));
    let v = app.active_viewer().unwrap();
    let rows = v.content_rows();
    assert!(rows < 300, "skimmed: {rows} rows");
    assert!(
        v.scroll_y <= (rows - 1) as f32 * line_h,
        "past the end: {} with {rows} rows",
        v.scroll_y
    );
    let top = v.line_at_row((v.scroll_y / line_h) as usize);
    assert_eq!(top, 440, "f110 at the top still");
}

/// A control scrolled out of sight is scrolled to when the ring reaches
/// it; a picker and a tick are stops, pressed by Enter as by a click.
#[test]
fn the_ring_reaches_what_is_scrolled_away_and_the_pickers() {
    use crate::SettingsPage;
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        SettingsPage::Appearance,
    )));
    let mut window = Driven::new(&app);
    window.tab(&mut app, false);
    let (_, count, _) = ring_at(&app);
    let mut presses = Vec::new();
    for _ in 0..count {
        presses.push(format!("{:?}", ring_at(&app).2));
        window.tab(&mut app, false);
    }
    assert!(
        presses.iter().any(|p| p.contains("SetLanguage")),
        "the display language's picker: {presses:#?}"
    );
    // A page longer than the window: its last stops are scrolled to.
    let mut app = reader_app();
    app.window_height = 520.0;
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        SettingsPage::Voice,
    )));
    let mut window = Driven::sized(&app, Size::new(1000.0, 520.0));
    window.tab(&mut app, false);
    let top = window
        .scrolled(&app, super::settings_scroll_id())
        .expect("the page");
    assert_eq!(top.y, 0.0);
    window.tab(&mut app, true);
    window.tab(&mut app, true);
    let low = window
        .scrolled(&app, super::settings_scroll_id())
        .expect("the page");
    assert!(low.y > 0.0, "scrolled to the page's last stops: {low:?}");
}

/// A file's row in the Files view's tree is counted as the tree lays its
/// rows out: each folder's row, its rows when open, then its files.
#[test]
fn a_files_row_is_where_the_tree_lays_it() {
    let tree = crate::fs_scan::DirNode {
        dirs: vec![(
            "a".into(),
            crate::fs_scan::DirNode {
                dirs: vec![(
                    "b".into(),
                    crate::fs_scan::DirNode {
                        dirs: vec![],
                        files: vec!["x.rs".into()],
                    },
                )],
                files: vec!["y.rs".into()],
            },
        )],
        files: vec!["z.rs".into()],
    };
    let open = |dirs: &[&str]| -> std::collections::HashSet<String> {
        dirs.iter().map(|d| d.to_string()).collect()
    };
    let at = |dirs: &[&str], rel: &str| super::tree_row_index(&tree, &open(dirs), rel);
    // a, (b closed), y.rs, z.rs
    assert_eq!(at(&["a"], "a/y.rs"), Some(2));
    assert_eq!(at(&["a"], "z.rs"), Some(3));
    assert_eq!(at(&["a"], "a/b/x.rs"), None, "behind a closed folder");
    // a, b, x.rs, y.rs, z.rs
    assert_eq!(at(&["a", "a/b"], "a/b/x.rs"), Some(2));
    assert_eq!(at(&["a", "a/b"], "z.rs"), Some(4));
    assert_eq!(at(&[], "z.rs"), Some(1));
}

/// The reading trail's row stands from the first file on, the file alone
/// until there is a way back: it came with the first jump, and moved the
/// code down a row.
#[test]
fn the_trail_row_does_not_move_the_code_when_the_trail_begins() {
    let mut app = reader_app();
    let abs = app.active_viewer().unwrap().abs.clone();
    assert!(app.proj.history.recent_path(2).is_empty());
    let before = super::sheet_header_height(&app);
    let mut sim = sim_of(&app);
    // Over the code as the title, and in the trail's row.
    assert!(
        text_at(&mut sim, |t| t == "lib.rs", 1).is_some(),
        "the file the trail starts at"
    );
    drop(sim);
    let loc = |line| crate::history::Loc {
        path: abs.clone(),
        line: Some(line),
    };
    app.proj.history.push(loc(1), Some("line_0".into()));
    app.proj.history.push(loc(40), Some("line_39".into()));
    assert_eq!(app.proj.history.recent_path(2).len(), 2, "a way back");
    assert_eq!(
        super::sheet_header_height(&app),
        before,
        "the trail's start moved the code"
    );
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "line_0"));
    assert!(shows(&mut sim, "line_39"));
}

/// A path too long for its row is cut at its start — "…" and its last
/// folders — not run past the row's end, under the finder's scrollbar, and
/// clipped mid-letter at the panel's edge.
#[test]
fn a_long_path_in_the_finder_keeps_its_end_in_view() {
    let mut app = reader_app();
    let deep =
        "crates/clew-core/src/analysis/languages/typescript/resolution/module_graph/fixtures";
    let mut rels: Vec<String> = vec![format!("{deep}/tsconfig_paths.rs")];
    // Enough rows that the list scrolls.
    rels.extend((0..40).map(|i| format!("src/m{i}.rs")));
    let files: Vec<crate::fs_scan::FileEntry> = rels
        .iter()
        .map(|rel| crate::fs_scan::FileEntry {
            abs: PathBuf::from("/nonexistent/clew-ui-test").join(rel),
            rel: rel.clone(),
        })
        .collect();
    app.proj.project.as_mut().unwrap().files = Arc::new(files);
    app.proj.finder.open = true;
    app.proj.finder.mode = crate::finder::FinderMode::Files;
    app.proj.finder.results = (0..rels.len()).collect();
    let mut sim = sim_of(&app);
    let dir = text_at(&mut sim, |t| t.ends_with("module_graph/fixtures"), 0)
        .expect("the folder's end is shown");
    assert!(shows(&mut sim, "…/"), "cut at its start, at a folder");
    assert!(!shows(&mut sim, "crates/clew-core/src"), "and not whole");
    let name = text_at(&mut sim, |t| t == "tsconfig_paths.rs", 0).expect("its name");
    assert!(
        dir.x > name.x + name.width,
        "beside its name: {name:?} {dir:?}"
    );
    // Inside its row, clear of the scrollbar, which floats in the list's
    // padding at the panel's right.
    let panel_right = (WINDOW.width + super::chrome::FINDER_W) / 2.0;
    let scrollbar_left = panel_right - 1.0 - 6.0 - super::chrome::SCROLLBAR_W;
    assert!(
        dir.x + dir.width <= scrollbar_left,
        "{dir:?} runs under the scrollbar (from {scrollbar_left})"
    );
}

/// `elide_start` keeps what fits whole, cuts at a folder where it can, and
/// at a character where no folder's cut fits.
#[test]
fn eliding_keeps_the_end() {
    use super::steady::{elide_start, width_of};
    let f = crate::fonts::UI;
    let path = "alpha/beta/gamma/delta";
    let whole = width_of(path, 12.0, f);
    assert_eq!(elide_start(path, 12.0, f, whole + 1.0), path);
    let cut = elide_start(path, 12.0, f, width_of("…/gamma/delta", 12.0, f) + 1.0);
    assert_eq!(cut, "…/gamma/delta");
    let tight = elide_start(
        "averyveryverylongname",
        12.0,
        f,
        width_of("…name", 12.0, f) + 0.5,
    );
    assert_eq!(tight, "…name");
    assert_eq!(elide_start(path, 12.0, f, 1.0), "…");
}

/// A19: finder row `i` is the `i`th result, a stale one included (drawn,
/// inert) — the scroll-into-view places row `i` at `i * FINDER_ROW_H`, and a
/// skipped row put it, and the highlight, one row off for every row after.
#[test]
fn finder_rows_stay_aligned_with_their_results() {
    let mut app = reader_app();
    let files: Vec<crate::fs_scan::FileEntry> = ["alpha.rs", "beta.rs"]
        .iter()
        .map(|rel| crate::fs_scan::FileEntry {
            abs: PathBuf::from("/nonexistent/clew-ui-test").join(rel),
            rel: rel.to_string(),
        })
        .collect();
    app.proj.project.as_mut().unwrap().files = Arc::new(files);
    app.proj.finder.open = true;
    app.proj.finder.mode = crate::finder::FinderMode::Files;
    // The middle result names an entry a rescan removed.
    app.proj.finder.results = vec![0, 99, 1];
    app.proj.finder.selected = 2;
    let mut rows = Vec::new();
    super::finder_file_rows(&app, &mut rows);
    assert_eq!(rows.len(), 3, "a stale result was skipped");
    let mut sim = sim_of(&app);
    let first = text_at(&mut sim, |t| t == "alpha.rs", 0).expect("row 0");
    let third = text_at(&mut sim, |t| t == "beta.rs", 0).expect("row 2");
    assert_eq!(
        third.y - first.y,
        2.0 * super::FINDER_ROW_H,
        "row 2 is not where the reveal scrolls to"
    );
}

/// The overview's module map stands without a caption telling how to use
/// it, drawn flat or in 3D.
#[test]
fn the_overview_map_has_no_caption() {
    let mut app = reader_app();
    app.proj.overview.showing = true;
    app.proj.overview.markdown = Some("## What it does\nA thing.".into());
    let node = |name: &str| crate::graphlayout::NodeInput {
        label: name.into(),
        file: PathBuf::from("/nonexistent/clew-ui-test").join(name),
        line: 1,
        weight: 1.0,
        cyclic: false,
    };
    app.proj.overview.map = Some(crate::graphlayout::layout(
        vec![node("a.rs"), node("b.rs")],
        vec![(0, 1)],
    ));
    for graph_3d in [false, true] {
        app.graph_3d = graph_3d;
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Module graph"), "3D {graph_3d}: no map");
        assert!(
            text_at(
                &mut sim,
                |t| t.contains("drag") || t.contains("click") || t.contains("size ="),
                0
            )
            .is_none(),
            "3D {graph_3d}: a caption says how to use the map"
        );
    }
}

/// E2-12: the chrome geometry the tutorial's spotlight places its holes with
/// is checked against the DRAWN chrome — a click at each computed strip
/// tool's centre reaches that tool's own action, one at the chat field's
/// centre opens the conversation — not re-derived from the formula it is
/// computed by.
#[test]
fn the_chrome_geometry_matches_what_is_drawn() {
    use super::StripTool;
    /// Whether a message is the one a tool sends.
    type Sends = fn(&Message) -> bool;
    let app = reader_app();
    let expected: [(StripTool, Sends); 14] = [
        (StripTool::Overview, |m| {
            matches!(m, Message::Overview(crate::OverviewMsg::Show))
        }),
        (StripTool::Stats, |m| {
            matches!(m, Message::Overview(crate::OverviewMsg::ShowStats))
        }),
        (StripTool::WeakSpots, |m| {
            matches!(m, Message::Weak(crate::WeakMsg::Show))
        }),
        (StripTool::Run, |m| {
            matches!(m, Message::Debug(DebugMsg::Run))
        }),
        (StripTool::FakeRun, |m| {
            matches!(m, Message::Fake(crate::FakeMsg::AtCaret { debug: false }))
        }),
        (StripTool::Debug, |m| {
            matches!(m, Message::Debug(DebugMsg::Start))
        }),
        (StripTool::FakeDebug, |m| {
            matches!(m, Message::Fake(crate::FakeMsg::AtCaret { debug: true }))
        }),
        (StripTool::RunMenu, |m| {
            matches!(m, Message::Window(WindowMsg::ToggleRunMenu))
        }),
        (StripTool::CallGraph, |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectCalls))
            )
        }),
        (StripTool::ImportGraph, |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectImports))
            )
        }),
        (StripTool::TypeMap, |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectTypes))
            )
        }),
        (StripTool::ProjectMaps, |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectMaps))
            )
        }),
        (StripTool::More, |m| {
            matches!(m, Message::Window(WindowMsg::ToggleToolsMenu))
        }),
        (StripTool::Settings, |m| {
            matches!(m, Message::Settings(crate::SettingsMsg::Open))
        }),
    ];
    let click_at = |at: Point| {
        let mut sim = sim_of(&app);
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim).into_iter().collect::<Vec<_>>()
    };
    for (tool, is_expected) in expected {
        // The ⋯ is not on a strip tall enough for the menu's own tools,
        // nor the bug on one tall enough for the four ways to run.
        let Some(at) = super::strip_slot_center(&app, super::StripSlot::Tool(tool)) else {
            assert!(
                matches!(tool, StripTool::More | StripTool::RunMenu),
                "{tool:?} is not on the strip"
            );
            continue;
        };
        let sent = click_at(at);
        assert!(
            sent.iter().any(is_expected),
            "{tool:?}'s computed centre clicked {sent:?}"
        );
    }
    // The menu's tools laid on the strip: each where it is computed to be.
    if super::tools_on_strip(&app) {
        for row in super::TOOLS_MENU_ROWS {
            let at = super::strip_slot_center(&app, super::StripSlot::Row(row)).unwrap();
            let sent = click_at(at);
            let expected = match row {
                super::ToolsRow::Tutorial => {
                    matches!(
                        sent.as_slice(),
                        [Message::Tutorial(crate::TutorialMsg::Start)]
                    )
                }
                super::ToolsRow::Glossary => matches!(
                    sent.as_slice(),
                    [Message::Glossary(crate::GlossaryMsg::Open)]
                ),
                // The rest: something, and only one thing, is pressed.
                _ => sent.len() <= 1,
            };
            assert!(expected, "{row:?}'s computed centre clicked {sent:?}");
        }
    }
    let field = super::search_field_rect(&app);
    let sent = click_at(field.center());
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Nav(crate::NavMsg::FinderOpened(_)))),
        "the search field's computed centre clicked {sent:?}"
    );

    // Its tools on the strip, no ⋯ menu is drawn, whatever asked for it:
    // there is no ⋯ to hang it from.
    let mut app = reader_app();
    assert!(super::tools_on_strip(&app));
    app.show_tools_menu = true;
    assert!(
        !shows(&mut sim_of(&app), "Keyboard Shortcuts"),
        "a menu of what the strip shows"
    );
    // The ⋯ menu's rows, in a window too short for them on the strip: each
    // computed row rect holds that row's label.
    app.window_height = TOOLS_IN_MENU_H;
    assert!(!super::tools_on_strip(&app));
    for (row, label) in [
        (super::ToolsRow::ExplainAll, "Explain All"),
        (super::ToolsRow::SetUp, "Set Up Project…"),
        (super::ToolsRow::Tutorial, "Tutorial"),
    ] {
        let rect = super::tools_menu_rows_rect(&app, row.index(), 1);
        let mut sim = sim_of(&app);
        let at = text_at(&mut sim, move |t| t == label, 0)
            .unwrap_or_else(|| panic!("{label} is not drawn"));
        assert!(
            rect.contains(at.center()),
            "{label} is drawn at {at:?}, outside its computed row {rect:?}"
        );
    }

    // The View menu: drawn where its spotlight says.
    let mut app = reader_app();
    app.show_view_menu = true;
    let rect = super::region_rect(&app, crate::app::tutorial::Anchor::ViewMenu).unwrap();
    let mut sim = sim_of(&app);
    for label in ["Outline summaries", "Minimap"] {
        let at = text_at(&mut sim, move |t| t == label, 0)
            .unwrap_or_else(|| panic!("{label} is not drawn"));
        assert!(
            rect.contains(at.center()),
            "{label} is drawn at {at:?}, outside the View menu's {rect:?}"
        );
    }
}

/// E2-9 / E2-12: the tutorial's spotlight frames what the view DRAWS — found
/// on screen — and never a panel the view has hidden: the right panel is not
/// drawn for a split, a narrow window, or no file, whatever
/// `show_right_panel` says. The sidebar's divider strip belongs to neither
/// it nor the reader; the margin's lies over the margin's edge, taking no
/// room of its own, so the margin starts where the reader ends.
#[test]
fn the_spotlight_frames_the_regions_the_view_draws() {
    use super::region_rect;
    use crate::app::tutorial::Anchor;
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.show_right_panel = true;
    // The file-summary banner: text the simulator can see in the reader (the
    // code view paints its own glyphs).
    app.show_file_banner = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.explain.cache.insert(
        crate::explain::Node::File(abs),
        crate::explain::Cached {
            summary: "Spotlight heading. More here.".into(),
            prompt_hash: crate::incremental::content_hash(b"test"),
            detail: None,
            basis: None,
        },
    );
    // Nothing explained under the caret: the margin says how to get a note.
    app.proj.explain.view = None;
    let margin_hint = |t: &str| t.contains("Move the cursor into a function");
    let mut sim = sim_of(&app);
    let hint = text_at(&mut sim, margin_hint, 0).expect("the margin is drawn");
    let margin = region_rect(&app, Anchor::RightTop).expect("a margin hole");
    assert!(
        margin.contains(hint.center()),
        "{hint:?} outside {margin:?}"
    );
    let code =
        text_at(&mut sim, |t| t.contains("Spotlight heading"), 0).expect("the document is drawn");
    let main = region_rect(&app, Anchor::Main).unwrap();
    assert!(main.contains(code.center()), "{code:?} outside {main:?}");
    let sidebar = region_rect(&app, Anchor::Sidebar).unwrap();
    // The divider lies over the sidebar's edge, as the design has it: the
    // sidebar and the reader meet.
    assert_eq!(
        main.x - (sidebar.x + sidebar.width),
        0.0,
        "the sidebar and the reader meet"
    );
    assert_eq!(margin.x - (main.x + main.width), 0.0);

    for (why, hide) in [
        (
            "a split",
            Box::new(|a: &mut App| a.proj.split = true) as Box<dyn Fn(&mut App)>,
        ),
        (
            "a narrow window",
            Box::new(|a: &mut App| a.window_width = 900.0),
        ),
        ("no file", Box::new(|a: &mut App| a.proj.panes[0] = None)),
        // A page over the panes hides the sheet's header, and with it the
        // View menu that shows and hides the margin: the margin stayed
        // beside the page, its notes of the file under it.
        (
            "the Overview",
            Box::new(|a: &mut App| a.proj.overview.showing = true),
        ),
        ("Stats", Box::new(|a: &mut App| a.proj.stats.showing = true)),
        (
            "the Glossary",
            Box::new(|a: &mut App| a.proj.glossary.showing = true),
        ),
    ] {
        let mut hidden = reader_app();
        hidden.show_right_panel = true;
        hidden.proj.explain.view = None;
        hide(&mut hidden);
        let mut sim = sim_of(&hidden);
        assert!(
            !shows(&mut sim, "Move the cursor into a function"),
            "{why}: the view hides the panel"
        );
        assert!(
            region_rect(&hidden, Anchor::RightTop).is_none(),
            "{why}: the spotlight framed a panel that is not drawn"
        );
    }
}

/// While Explain All runs, the ⋯ menu's row says how far it has got, and the
/// menu is as wide as that takes, in every language: it was measured for
/// "Cancel explaining" alone, and the count ran past its edge.
#[test]
fn the_tools_menu_holds_the_explain_all_count() {
    use crate::i18n::{Lang, with_lang};
    let mut app = reader_app();
    open_tools_menu(&mut app);
    app.proj.explain.running = true;
    app.proj.explain.progress = Some((1200, 12040));
    for lang in Lang::ALL {
        with_lang(lang, || {
            let label = crate::tr!(
                "Cancel explaining ({done}/{total})",
                done = 1200,
                total = 12040
            );
            let row = super::tools_menu_rows_rect(&app, super::ToolsRow::ExplainAll.index(), 1);
            let mut sim = sim_of(&app);
            let wanted = label.clone();
            let at = text_at(&mut sim, move |t| t == wanted, 0)
                .unwrap_or_else(|| panic!("{label} is not drawn"));
            let end = at.x + super::sheet::text_width(&label, super::ts::BASE, crate::fonts::UI);
            assert!(
                end <= row.x + row.width,
                "{label} runs to {end}, past the menu's edge at {}",
                row.x + row.width
            );
        });
    }
}

/// What clew-core said, drawn outside the status line, is said in the
/// reader's language as the status line says it: the search tab's error,
/// an MCP server's failure on Settings → Integrations. Both were drawn as
/// clew-core wrote them, in English.
#[test]
fn back_end_words_beside_the_status_line_are_said_in_the_readers_language() {
    use crate::i18n::{Lang, with_lang};
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Search;
    app.proj.search.error = Some("Invalid pattern: unclosed group".into());
    with_lang(Lang::Fr, || {
        let said = crate::i18n::message("Invalid pattern: unclosed group");
        assert_ne!(said, "Invalid pattern: unclosed group");
        assert!(shows(&mut sim_of(&app), &said), "{said} is not drawn");
    });

    let mut app = reader_app();
    app.settings.open = true;
    app.settings.page = crate::SettingsPage::Integrations;
    app.settings.integrations.mcp = vec![clew_core::mcp::ServerConfig {
        name: "docs".into(),
        transport: clew_core::mcp::Transport::Http {
            url: "https://mcp.example/".into(),
            headers: Vec::new(),
        },
        enabled: true,
        approve: clew_core::mcp::Approve::Ask,
        allow: Vec::new(),
        timeout: std::time::Duration::from_secs(10),
    }];
    let failed = "The MCP server failed: it hung up";
    app.settings.integrations.statuses.insert(
        "docs".into(),
        clew_core::mcp::hub::Status::Failed(failed.into()),
    );
    with_lang(Lang::Fr, || {
        let said = crate::i18n::message(failed);
        assert_ne!(said, failed);
        assert!(shows(&mut sim_of(&app), &said), "{said} is not drawn");
    });
}

/// E2-6: Escape dismisses exactly the overlay that is drawn on top.
#[test]
fn escape_names_the_topmost_overlay() {
    let mut app = reader_app();
    assert!(super::escape_message(&app).is_none());

    app.proj.hover = Some(crate::HoverState {
        text: Some("fn line_1()".into()),
        ..hover_at(10.0, 10.0)
    });
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Hover(HoverMsg::Pin(false)))
    ));

    app.proj.context_menu = Some(crate::ContextMenu {
        pane: 0,
        line: 1,
        col: 1,
        x: 20.0,
        y: 20.0,
        insight: None,
        served: true,
        by_pointer: true,
    });
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Hover(HoverMsg::ContextMenuClosed))
    ));

    app.proj.finder.open = true;
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Nav(NavMsg::FinderClosed))
    ));

    open_tools_menu(&mut app);
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Window(WindowMsg::ToggleToolsMenu))
    ));

    app.settings.open = true;
    app.settings.page = crate::SettingsPage::Shortcuts;
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Settings(crate::SettingsMsg::Close))
    ));

    // Settings opened from the project's sheet is over it, and goes first.
    app.pending_consent = Some(PathBuf::from("/nonexistent/clew-ui-test"));
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Settings(crate::SettingsMsg::Close))
    ));
    app.settings.open = false;
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Project(ProjectMsg::ConsentDenied(root))) if root == std::path::Path::new("/nonexistent/clew-ui-test")
    ));
}

fn hover_at(x: f32, y: f32) -> crate::HoverState {
    crate::HoverState {
        line: 0,
        col: 0,
        x,
        y,
        text: None,
        summary: None,
        diagnostic: None,
    }
}

/// E2-6, end to end through the key handler: each Escape closes one layer,
/// top first, and leaves the rest for the next press.
#[test]
fn escape_closes_overlays_one_at_a_time() {
    let mut app = reader_app();
    app.proj.context_menu = Some(crate::ContextMenu {
        pane: 0,
        line: 1,
        col: 1,
        x: 20.0,
        y: 20.0,
        insight: None,
        served: true,
        by_pointer: true,
    });
    open_tools_menu(&mut app);
    let escape = || {
        Message::Editor(EditorMsg::KeyPressed(
            keyboard::Key::Named(keyboard::key::Named::Escape),
            keyboard::Modifiers::default(),
        ))
    };
    let _ = app.update(escape());
    assert!(!app.show_tools_menu, "the ⋯ menu is on top");
    assert!(
        app.proj.context_menu.is_some(),
        "one press closes one layer"
    );
    let _ = app.update(escape());
    assert!(app.proj.context_menu.is_none());
}

/// E2-6: moving the finder's selection scrolls its list just enough to keep
/// the selected row in view, against a real scrollable.
#[test]
fn finder_rows_are_revealed_with_minimal_scrolling() {
    let mut renderer = renderer();
    let rows = |n: usize| -> Element<'static, Message> {
        let list = iced::widget::Column::with_children(
            (0..n).map(|_| space().height(super::FINDER_ROW_H).into()),
        );
        container(
            scrollable(list)
                .id(super::finder_list_id())
                .height(iced::Length::Fixed(300.0)),
        )
        .into()
    };
    let offset_after = |cache: Cache, selected: usize, renderer: &mut iced::Renderer| {
        let mut ui = UserInterface::build(rows(100), WINDOW, cache, renderer);
        let top = selected as f32 * super::FINDER_ROW_H;
        let reveal =
            super::reveal_operation(super::finder_list_id(), top, top + super::FINDER_ROW_H);
        let _ = run_operation(&mut ui, renderer, reveal);
        let mut probe = ScrollProbe {
            target: super::finder_list_id(),
            found: None,
        };
        ui.operate(renderer, &mut probe);
        (probe.found.map(|t| t.y), ui.into_cache())
    };
    let (y, cache) = offset_after(Cache::default(), 50, &mut renderer);
    assert_eq!(y, Some(51.0 * super::FINDER_ROW_H - 300.0));
    // Already visible: nothing moves.
    let (y, cache) = offset_after(cache, 45, &mut renderer);
    assert_eq!(y, Some(51.0 * super::FINDER_ROW_H - 300.0));
    // Above the viewport: its top aligns.
    let (y, _) = offset_after(cache, 2, &mut renderer);
    assert_eq!(y, Some(2.0 * super::FINDER_ROW_H));
}

// ------------------------------------------------------------------------
// Wave-2 integration (W2-I): end-to-end view checks through iced_test's
// simulator — what is on screen, and which message a click on it sends.
// ------------------------------------------------------------------------

type Sim<'a> = iced_test::Simulator<'a, Message, iced::Theme, iced::Renderer>;

/// The whole window, at the app's test size.
/// A window short enough that the ⋯ menu's tools are in its menu, not laid
/// on the strip ([`super::tools_on_strip`]): there is a ⋯ to open it from.
const TOOLS_IN_MENU_H: f32 = 520.0;

/// `app`'s ⋯ menu open — in a window short enough to have one.
fn open_tools_menu(app: &mut App) {
    app.window_height = TOOLS_IN_MENU_H;
    assert!(!super::tools_on_strip(app));
    app.show_tools_menu = true;
}

fn sim_of(app: &App) -> Sim<'_> {
    // The renderer keeps a shader cache under `$XDG_CACHE_HOME`: in the
    // suite's own directory, never the developer's home.
    crate::app::tests::isolated_data_dir();
    crate::fonts::load();
    iced_test::Simulator::with_size(Default::default(), WINDOW, super::view(app))
}

/// The whole window, at `size` (the app told it is that size too).
fn sim_sized(app: &App, size: Size) -> Sim<'_> {
    crate::app::tests::isolated_data_dir();
    crate::fonts::load();
    iced_test::Simulator::with_size(Default::default(), size, super::view(app))
}

/// One element on its own (default simulator size).
fn sim_elem(elem: Element<'_, Message>) -> Sim<'_> {
    crate::app::tests::isolated_data_dir();
    iced_test::simulator(elem)
}

/// [`sim_elem`] tall enough for a whole Settings section: what a test
/// clicks is on screen however long the sections above it grow.
fn sim_tall(elem: Element<'_, Message>) -> Sim<'_> {
    crate::app::tests::isolated_data_dir();
    crate::fonts::load();
    iced_test::Simulator::with_size(Default::default(), Size::new(1024.0, 3000.0), elem)
}

/// The visible bounds of the `nth` (0-based) text whose content satisfies
/// `pred`, if it is on screen.
fn text_at(sim: &mut Sim<'_>, pred: impl Fn(&str) -> bool + Send, nth: usize) -> Option<Rectangle> {
    use iced_test::selector::Candidate;
    let mut seen = 0usize;
    sim.find(move |c: Candidate<'_>| match c {
        Candidate::Text {
            content,
            visible_bounds,
            ..
        } if pred(content) => {
            if seen == nth {
                visible_bounds
            } else {
                seen += 1;
                None
            }
        }
        _ => None,
    })
    .ok()
}

/// Whether some text on screen contains `needle`.
fn shows(sim: &mut Sim<'_>, needle: &str) -> bool {
    let needle = needle.to_string();
    text_at(sim, move |t| t.contains(&needle), 0).is_some()
}

/// Click the icon button left of the `nth` text equal to `label` — a row's
/// chevron or remove mark, drawn as an icon, not text — stepping left
/// until a click sends what `wanted` looks for; what it sent. `view` draws
/// it afresh for each try.
fn click_left_of<'a>(
    view: impl Fn() -> Sim<'a>,
    label: &str,
    nth: usize,
    wanted: fn(&Message) -> bool,
) -> Vec<Message> {
    for dx in (1..=20).map(|i| i as f32 * 3.0) {
        let mut sim = view();
        let text = label.to_string();
        let bounds = text_at(&mut sim, move |t| t == text, nth)
            .unwrap_or_else(|| panic!("{label:?} (#{nth}) is not on screen"));
        sim.point_at(Point::new(bounds.x - dx, bounds.center_y()));
        let _ = sim.simulate(iced_test::simulator::click());
        let sent: Vec<Message> = sent_by(sim).into_iter().collect();
        if sent.iter().any(wanted) {
            return sent;
        }
    }
    Vec::new()
}

/// Click right of the text `label`, along its line, until a press sends a
/// `wanted` message: an icon button at the end of a title (a panel's ×)
/// has no text to click.
fn click_right_of<'a>(
    view: impl Fn() -> Sim<'a>,
    label: &str,
    wanted: fn(&Message) -> bool,
) -> Vec<Message> {
    let text = label.to_string();
    let start = {
        let mut sim = view();
        text_at(&mut sim, move |t| t == text, 0)
            .unwrap_or_else(|| panic!("{label:?} is not on screen"))
    };
    for dx in (1..=500).map(|i| i as f32 * 4.0) {
        let mut sim = view();
        sim.point_at(Point::new(start.x + start.width + dx, start.center_y()));
        let _ = sim.simulate(iced_test::simulator::click());
        let sent: Vec<Message> = sent_by(sim).into_iter().collect();
        if sent.iter().any(wanted) {
            return sent;
        }
    }
    Vec::new()
}

/// Click the `nth` text equal to `label` and return what the click sent.
fn click_nth(mut sim: Sim<'_>, label: &str, nth: usize) -> Vec<Message> {
    let wanted = label.to_string();
    let bounds = text_at(&mut sim, move |t| t == wanted, nth)
        .unwrap_or_else(|| panic!("{label:?} (#{nth}) is not on screen"));
    sim.point_at(bounds.center());
    let _ = sim.simulate(iced_test::simulator::click());
    sent_by(sim).into_iter().collect()
}

/// What `sim` sent, leaving out where its questions' answers were laid out
/// (`ui::placed`): the runtime's own bookkeeping, not a press's message.
fn sent_by(sim: Sim<'_>) -> Vec<Message> {
    sim.into_messages()
        .filter(|m| {
            !matches!(
                m,
                Message::Placed { .. } | Message::Window(WindowMsg::SoftwareRenderer(..))
            )
        })
        .collect()
}

fn click(sim: Sim<'_>, label: &str) -> Vec<Message> {
    click_nth(sim, label, 0)
}

/// I10: the tutorial walks every step on screen — each step's card shows its
/// counter and title, and its Next (Done on the last) sends the step that the
/// app then takes — until Done ends the tour.
#[test]
fn the_tutorial_walks_every_step_end_to_end() {
    let mut app = reader_app();
    let _ = app.update(Message::Tutorial(TutorialMsg::Begin));
    let total = crate::app::tutorial::steps(&app).len();
    assert!(total > 10, "the tour lost its steps: {total}");
    for i in 0..total {
        assert_eq!(app.tutorial, Some(i));
        let title = crate::app::tutorial::steps(&app)[i].title.clone();
        let mut sim = sim_of(&app);
        let counter = format!("{} of {total}", i + 1);
        assert!(sim.find(counter.as_str()).is_ok(), "no {counter:?}");
        assert!(
            sim.find(title.as_str()).is_ok(),
            "step {i}: {title:?} is not on screen"
        );
        let next = if i + 1 == total { "Done" } else { "Next" };
        let sent = click(sim, next);
        assert!(
            matches!(sent.as_slice(), [Message::Tutorial(TutorialMsg::Step(1))]),
            "step {i}: {next} sent {sent:?}"
        );
        for msg in sent {
            let _ = app.update(msg);
        }
    }
    assert_eq!(app.tutorial, None, "Done ends the tour");
}

/// A Rust file's context menu has its memory layout and its tests; a Lean
/// file's, Step Through Proof. Each sends its own message.
#[test]
fn the_context_menu_has_each_languages_own_rows() {
    let mut app = reader_app();
    app.proj.context_menu = Some(crate::ContextMenu {
        pane: 0,
        line: 3,
        col: 4,
        x: 200.0,
        y: 120.0,
        insight: crate::InsightKind::for_lang("rust"),
        served: true,
        by_pointer: true,
    });
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Memory Layout"));
    assert!(shows(&mut sim, "Related Tests"));
    assert!(!shows(&mut sim, "Step Through Proof"));
    drop(sim);
    let sent = click(sim_of(&app), "Related Tests");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Hover(HoverMsg::InsightOf(crate::InsightKind::RelatedTests))
        )),
        "{sent:?}"
    );
    // The same file as Lean.
    let v = app.proj.panes[0].as_mut().unwrap();
    v.lang_key = Some("lean");
    app.proj.context_menu.as_mut().unwrap().insight = crate::InsightKind::for_lang("lean");
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Step Through Proof"));
    assert!(!shows(&mut sim, "Memory Layout"));
    drop(sim);
    let sent = click(sim_of(&app), "Step Through Proof");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Proof(crate::ProofMsg::FromMenu))),
        "{sent:?}"
    );
}

/// A page over the code, scrolled to its end, has nothing under the Ask
/// pill floating at the window's foot: its last row stands clear of it
/// (the weak spots here; the glossary, the overview and a docs page alike).
#[test]
fn a_pages_last_row_scrolls_clear_of_the_ask_pill() {
    use clew_core::weakspots::{Reason, Spot};
    let mut app = reader_app();
    app.proj.weak.showing = true;
    app.proj.weak.files = 40;
    app.proj.weak.spots = Some(std::sync::Arc::new(
        (0..40)
            .map(|i| Spot {
                rel: format!("src/f{i:02}.rs"),
                name: format!("spot{i:02}"),
                line: 1,
                end_line: 90,
                score: 40 - i,
                reasons: vec![Reason::Long(87)],
            })
            .collect(),
    ));
    let pill = super::askfloat::ask_corner_rect(&app);
    let mut sim = sim_of(&app);
    sim.point_at(Point::new(WINDOW.width / 2.0, WINDOW.height / 2.0));
    let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels {
            x: 0.0,
            y: -20_000.0,
        },
    })]);
    let last = text_at(&mut sim, |t| t == "spot39", 0).expect("the last row");
    assert!(
        last.y + last.height <= pill.y,
        "{last:?} under the pill at {pill:?}"
    );
}

/// The weak spots page says what it is, then each spot: its name, where
/// it is, why it is listed; the row opens its code, Ask why asks.
#[test]
fn the_weak_spots_page_lists_each_spot_with_why() {
    use clew_core::weakspots::{Reason, Risk, Spot};
    let mut app = reader_app();
    app.proj.weak.showing = true;
    app.proj.weak.files = 12;
    app.proj.weak.spots = Some(std::sync::Arc::new(vec![
        Spot {
            rel: "src/load.rs".into(),
            name: "load".into(),
            line: 4,
            end_line: 90,
            score: 20,
            reasons: vec![
                Reason::Long(87),
                Reason::Deep(5),
                Reason::Risky(Risk::Unwrap, 3),
                Reason::Churn(24),
            ],
        },
        Spot {
            rel: "src/old.rs".into(),
            name: String::new(),
            line: 1,
            end_line: 1,
            score: 3,
            reasons: vec![Reason::Marked(1)],
        },
    ]));
    let mut sim = sim_of(&app);
    for words in [
        "Weak Spots",
        "2 spots, from 12 files measured",
        "load",
        "src/load.rs:4",
        "87 lines",
        "nested 5 deep",
        "3 unwraps",
        "changed in 24 commits",
        "notes in the file",
        "1 TODO or FIXME",
    ] {
        assert!(shows(&mut sim, words), "{words} is not shown");
    }
    assert!(
        !shows(&mut sim, "A guide to read by"),
        "no paragraph saying what the page is"
    );
    drop(sim);
    let sent = click(sim_of(&app), "load");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Weak(crate::WeakMsg::Open(0)))),
        "{sent:?}"
    );
    let sent = click_nth(sim_of(&app), "Ask why", 1);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Weak(crate::WeakMsg::AskWhy(1)))),
        "{sent:?}"
    );
}

/// The proof panel shows the step's tactic among the steps, the goals
/// before and after it, what the tactic brought marked `+`, changed `~`,
/// and the goals it closed; at the end, that it is proved.
#[test]
fn the_proof_panel_marks_what_each_tactic_did() {
    use clew_core::leanproof::ProofStep;
    let mut app = reader_app();
    let step = |first: usize, text: &str| ProofStep {
        first,
        last: first,
        end: 0,
        text: text.into(),
    };
    app.proj.proof = Some(crate::app::proof::ProofWalk {
        abs: PathBuf::from("/nonexistent/clew-ui-test/Swap.lean"),
        name: "and_swap".into(),
        steps: vec![step(0, "by"), step(1, "intro h"), step(2, "exact h.symm")],
        goals: Some(vec![
            Some(vec![
                "n : Nat\nk : Nat\n⊢ n = 1 → 1 = n".into(),
                "⊢ True".into(),
            ]),
            Some(vec!["n : Nat\nk : Int\nh : n = 1\n⊢ 1 = n".into()]),
            Some(Vec::new()),
        ]),
        failed: None,
        at: 1,
        token: 1,
    });
    app.show_bottom = true;
    let mut sim = sim_of(&app);
    for words in [
        "Proof",
        "and_swap",
        "step 2 of 3",
        "intro h",
        "GOALS BEFORE",
        "GOALS AFTER",
        "+ h : n = 1",
        "~ k : Int",
        "  n : Nat",
        "1 goal closed",
    ] {
        assert!(shows(&mut sim, words), "{words} is not shown");
    }
    drop(sim);
    let sent = click(sim_of(&app), "exact h.symm");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Proof(crate::ProofMsg::Goto(2)))),
        "{sent:?}"
    );
    app.proj.proof.as_mut().unwrap().at = 2;
    assert!(shows(&mut sim_of(&app), "No goals left: proved."));
}

/// The bottom panel has a tab for each run, named by what it runs — the
/// second of two Debugs of one program numbered — each pressed to show it
/// and closed by its ×; with none, it offers the four ways to run.
#[test]
fn the_panel_has_a_tab_for_each_run() {
    use crate::app::state::RunTab;
    let mut app = reader_app();
    app.show_bottom = true;
    assert!(shows(&mut sim_of(&app), "Nothing is running"));
    let sent = click(sim_of(&app), "Start Debugging");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Debug(DebugMsg::Start))),
        "{sent:?}"
    );
    let session = |status| crate::DebugSession {
        client: None,
        status,
        exit_code: None,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/p/main.py"),
        args: Vec::new(),
        cwd: PathBuf::from("/p"),
        addr: None,
    };
    app.debug.session = Some(session(crate::DebugStatus::Running));
    app.debug.tab = 1;
    app.park_debug();
    app.debug.session = Some(session(crate::DebugStatus::Stopped));
    app.debug.tab = 2;
    app.run_shown = Some(RunTab::Real(2));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "main.py (2)"), "two Debugs of one program");
    drop(sim);
    let sent = click(sim_of(&app), "main.py");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Debug(DebugMsg::ShowTab(RunTab::Real(1))))),
        "{sent:?}"
    );
    let (targets, _) = super::heard::gather_all(&app);
    for (shown, tab) in [("Close Debug main.py", 1), ("Close Debug main.py (2)", 2)] {
        assert!(
            targets.iter().any(|t| t.shown == shown
                && matches!(t.message, Message::Debug(DebugMsg::CloseTab(RunTab::Real(id))) if id == tab)),
            "{shown}: {:?}",
            targets.iter().map(|t| &t.shown).collect::<Vec<_>>()
        );
    }
    // Shown, a tab's run is the one on screen.
    for m in sent {
        let _ = app.update(m);
    }
    assert_eq!(app.debug.tab, 1);
    assert_eq!(
        app.debug.session.as_ref().unwrap().status,
        crate::DebugStatus::Running
    );
}

/// In a window too short for the four ways to run, they fold into the bug,
/// whose Run menu opens beside the strip, level with it: Run and Debug,
/// then Fake Run and Fake Debug, each named as the menu bar names it, with
/// its keys. A row sends its run, and picked, the menu goes; the bug is
/// lit while it is open.
#[test]
fn in_a_short_window_the_bug_opens_a_menu_of_the_four_runs() {
    use crate::keymap::Action;
    use crate::ui::StripTool;
    let short = Size {
        width: 1280.0,
        height: 540.0,
    };
    let mut app = reader_app();
    app.window_height = short.height;
    assert!(!super::runs_on_strip(&app));
    let (.., open) = super::strip_tool(&app, StripTool::RunMenu);
    assert!(matches!(open, Message::Window(WindowMsg::ToggleRunMenu)));
    let _ = app.update(open);
    assert!(app.show_run_menu);
    let bug = super::strip_tool_center(&app, StripTool::RunMenu);
    assert_eq!(
        super::toolbar::run_menu_top(&app),
        bug.y - super::ICON_BTN / 2.0,
        "level with the bug"
    );
    let mut sim = sim_sized(&app, short);
    for (label, action) in [
        ("Run Without Debugging", Action::Run),
        ("Start Debugging", Action::StartDebug),
        ("Fake Run…", Action::FakeRun),
        ("Fake Debug…", Action::FakeDebug),
    ] {
        let wanted = label.to_string();
        let row = text_at(&mut sim, move |t| t == wanted, 0)
            .unwrap_or_else(|| panic!("{label} is not in the menu"));
        assert!(row.x < bug.x, "{label} is beside the strip, left of it");
        let keys = app.keymap.chord(action).caps();
        let chord = text_at(&mut sim, move |t| t == keys, 0)
            .unwrap_or_else(|| panic!("{label}'s keys are not shown"));
        assert!(
            (chord.center().y - row.center().y).abs() < 2.0,
            "{label}'s keys are on its row"
        );
    }
    drop(sim);
    let sent = click(sim_sized(&app, short), "Run Without Debugging");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Debug(DebugMsg::Run))),
        "{sent:?}"
    );
    for m in sent {
        let _ = app.update(m);
    }
    assert!(!app.show_run_menu, "picked, it goes");
    let _ = app.update(Message::Window(WindowMsg::ToggleRunMenu));
    let sent = click(sim_sized(&app, short), "Fake Debug…");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::AtCaret { debug: true }))),
        "{sent:?}"
    );

    // The window made taller with the menu open: the four are buttons
    // again, and no menu hangs from a bug that is not there.
    assert!(app.show_run_menu);
    app.window_height = WINDOW.height;
    assert!(super::runs_on_strip(&app));
    assert!(
        !matches!(
            super::active_overlay(&app),
            Some(super::ActiveOverlay::RunMenu)
        ),
        "no menu hangs from a bug that is not there"
    );
}

/// In a window of the usual size, Run, Fake Run, Debug and Fake Debug are
/// buttons of their own on the strip, each beside its twin, and there is no
/// bug to open their menu from: its message opens nothing. Fake Run and
/// Fake Debug work out code in view: with no file open they are drawn off
/// and press nothing, as Run and Debug are with no project.
#[test]
fn the_four_ways_to_run_are_buttons_on_the_strip() {
    use crate::ui::StripTool;
    let mut app = reader_app();
    assert!(super::runs_on_strip(&app));
    assert!(super::strip_slot_center(&app, super::StripSlot::Tool(StripTool::RunMenu)).is_none());
    let _ = app.update(Message::Window(WindowMsg::ToggleRunMenu));
    assert!(!app.show_run_menu, "no bug, no menu");
    for tool in super::RUN_TOOLS {
        assert!(super::strip_tool_able(&app, tool), "{tool:?}");
    }
    let sent = {
        let mut sim = sim_of(&app);
        sim.point_at(super::strip_tool_center(&app, StripTool::FakeRun));
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::AtCaret { debug: false }))),
        "{sent:?}"
    );

    // No file open: the model has nothing to work out.
    app.proj.panes = Default::default();
    assert!(app.active_viewer().is_none());
    assert!(!super::strip_tool_able(&app, StripTool::FakeRun));
    assert!(!super::strip_tool_able(&app, StripTool::FakeDebug));
    assert!(super::strip_tool_able(&app, StripTool::Run));
    let sent = {
        let mut sim = sim_of(&app);
        sim.point_at(super::strip_tool_center(&app, StripTool::FakeDebug));
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::AtCaret { .. }))),
        "{sent:?}"
    );
}

/// Every tool on the strip — and every row of the ⋯ menu, laid on the strip
/// where there is room, and of the Run menu — has a shortcut, and its tip
/// says it: Overview, Stats and the menu's Glossary, Set Up Project… and
/// Tutorial had none. The ⋯ and the bug, menus, are the ones without. No
/// two share a chord, and none is one the menu bar keeps.
#[test]
fn every_tool_on_the_strip_has_a_shortcut() {
    use crate::ui::StripTool;
    let app = reader_app();
    for tool in super::STRIP_TOP
        .into_iter()
        .chain([StripTool::Settings])
        .filter(|t| !matches!(t, StripTool::More | StripTool::RunMenu))
    {
        let (_, label, keys, _) = super::strip_tool(&app, tool);
        assert!(keys.is_some(), "{label} has no shortcut");
    }
    for row in super::toolbar::RUN_MENU_ROWS {
        let action = super::toolbar::run_row_action(row);
        assert!(
            !app.keymap.chord(action).caps().is_empty(),
            "{row:?} has no chord"
        );
    }
    for row in super::TOOLS_MENU_ROWS {
        let action = super::tools_row_action(row);
        assert!(
            !app.keymap.chord(action).caps().is_empty(),
            "{row:?} has no chord"
        );
    }
    let all = crate::keymap::Action::ALL;
    for (i, a) in all.iter().enumerate() {
        let chord = app.keymap.chord(*a);
        for b in &all[i + 1..] {
            assert!(
                app.keymap.chord(*b) != chord,
                "{a:?} and {b:?} share a chord"
            );
        }
        assert!(
            !crate::keymap::MENU_RESERVED
                .iter()
                .any(|(c, _)| *c == chord),
            "{a:?} takes a chord the menu bar keeps"
        );
    }
}

/// Every tool on the strip for reading a project has its one tutorial
/// step, and none has two; and no text field holds the keyboard while the
/// tour shows its tab — the SEARCH box did, and took the → and Enter that
/// step the tour.
#[test]
fn the_tutorial_points_at_each_tool_and_leaves_no_field_holding_its_keys() {
    use crate::app::tutorial::{Anchor, steps};
    use crate::ui::StripTool;
    let mut app = reader_app();
    let _ = app.update(Message::Tutorial(TutorialMsg::Begin));
    let all = steps(&app);
    for tool in super::STRIP_TOP.into_iter().chain([StripTool::Settings]) {
        let on: Vec<&str> = all
            .iter()
            .filter(|s| s.anchor == Anchor::Strip(tool))
            .map(|s| s.title.as_str())
            .collect();
        let reads = matches!(
            tool,
            StripTool::Overview
                | StripTool::ProjectMaps
                | StripTool::CallGraph
                | StripTool::TypeMap
                | StripTool::ImportGraph
        );
        assert_eq!(on.len(), usize::from(reads), "{tool:?}: {on:?}");
    }
    // The four ways to run have one step, over the four of them.
    assert_eq!(
        all.iter().filter(|s| s.anchor == Anchor::RunTools).count(),
        1
    );
    let at = |title: &str| all.iter().position(|s| s.title == title).unwrap();
    assert!(all[at("Types and their kinds")].anchor == Anchor::Strip(StripTool::TypeMap));
    assert!(all.iter().any(|s| s.anchor == Anchor::AskCorner));
    assert!(all.iter().any(|s| s.anchor == Anchor::FileTools));
    assert!(all.iter().any(|s| s.anchor == Anchor::ViewMenu));
    let search = at("Search the project");

    // The search box holds the keys as the step before is left.
    app.sidebar = crate::SidebarTab::Search;
    let mut renderer = renderer();
    let mut ui = UserInterface::build(super::view(&app), WINDOW, Cache::default(), &mut renderer);
    let _ = run_operation(
        &mut ui,
        &renderer,
        iced::advanced::widget::operation::focusable::focus::<()>(super::search_input_id()),
    );
    assert_eq!(
        run_operation(&mut ui, &renderer, find_focused()),
        Some(super::search_input_id())
    );
    let cache = ui.into_cache();
    app.tutorial = Some(search - 1);
    let task = app.update(Message::Tutorial(TutorialMsg::Step(1)));
    assert_eq!(
        app.sidebar,
        crate::SidebarTab::Search,
        "the step shows the tab"
    );
    let mut ui = UserInterface::build(super::view(&app), WINDOW, cache, &mut renderer);
    for mut op in widget_ops(task) {
        loop {
            ui.operate(&renderer, &mut black_box(&mut *op));
            match op.finish() {
                Outcome::Chain(next) => op = next,
                _ => break,
            }
        }
    }
    assert_eq!(run_operation(&mut ui, &renderer, find_focused()), None);
}

/// The reading panels start from the top for each thing they show — the
/// node explained, the docs item, the tour's step, the sidebar's tab —
/// not where the last was scrolled to: each is drawn fresh for it
/// (`ui::fresh`), one key per thing shown.
#[test]
fn reading_panels_are_drawn_fresh_for_what_they_show() {
    use super::fresh::keys_in;
    // The docs page, by its item.
    let page = |line| crate::DocPage {
        rel: "src/lib.rs".into(),
        entries: vec![crate::DocEntryView {
            name: "a".into(),
            kind: "fn".into(),
            signature: String::new(),
            line,
            depth: 0,
            doc_items: Vec::new(),
        }],
    };
    assert_ne!(
        keys_in(&super::docs_page(&page(1))),
        keys_in(&super::docs_page(&page(9)))
    );
    assert_eq!(
        keys_in(&super::docs_page(&page(1))),
        keys_in(&super::docs_page(&page(1)))
    );
    // The sidebar, by its tab.
    let mut app = reader_app();
    let tab = |app: &mut App, tab| {
        app.sidebar = tab;
        keys_in(&super::sidebar::sidebar(app))
    };
    let marks = tab(&mut app, crate::SidebarTab::Marks);
    assert_ne!(marks, tab(&mut app, crate::SidebarTab::Notes));
    assert_eq!(marks, tab(&mut app, crate::SidebarTab::Marks));
    // The explain panel, by its node.
    let explained = |app: &mut App, name: &str| {
        app.proj.explain.view = Some(clew_core::explain::Node::Function {
            file: "src/lib.rs".into(),
            name: name.into(),
            ordinal: 0,
        });
        keys_in(&super::explain_content(app))
    };
    let a = explained(&mut app, "line_1");
    assert_ne!(a, explained(&mut app, "line_2"));
    assert_eq!(a, explained(&mut app, "line_1"));
    // The tour's narration, by its step.
    let step = |n: usize| crate::walkthrough::Step {
        title: format!("step {n}"),
        file: "src/lib.rs".into(),
        symbol: None,
        line: Some(n),
        narration: format!("narration {n}"),
        depth: 0,
    };
    let tour = crate::walkthrough::Walkthrough {
        title: "tour".into(),
        scope: String::new(),
        steps: vec![step(1), step(2)],
    };
    let at = |app: &mut App, n| {
        app.proj.walk.step = n;
        keys_in(&super::sidebar::walk_narration(app, &tour))
    };
    let first = at(&mut app, 0);
    assert_ne!(first, at(&mut app, 1));
    assert_eq!(first, at(&mut app, 0));
}

/// The find bar takes every press over it — on its count, its padding, the
/// gaps between its controls — none reaching the code drawn under it.
#[test]
fn a_press_on_the_find_bar_does_not_reach_the_code_under_it() {
    let mut app = reader_app();
    // Lines running under the bar, at the pane's top right.
    let source: String = (0..400)
        .map(|i| {
            format!(
                "fn line_{i}() {{ let value = {i}; }} // {}\n",
                "x".repeat(200)
            )
        })
        .collect();
    let lines = crate::highlight::plain_lines(&source);
    app.proj.panes[0] = Some(Viewer::new(
        PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"),
        "src/lib.rs".into(),
        Some("rust"),
        Arc::new(source),
        lines,
    ));
    app.proj.find.open = true;
    app.proj.find.query = "line_1".into();
    let _ = app.update(Message::Noop);
    assert!(!app.proj.find.matches.is_empty(), "precondition");
    let mut sim = sim_of(&app);
    let count = text_at(&mut sim, |t| t.contains('/'), 0).expect("its count is shown");
    // On the count, and on the panel's edge beside the input.
    for at in [count.center(), Point::new(count.x - 3.0, count.center_y())] {
        let mut sim = sim_of(&app);
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        let sent = sent_by(sim);
        assert!(
            !sent
                .iter()
                .any(|m| matches!(m, Message::Editor(EditorMsg::SelectStart { .. }))),
            "{at:?}: {sent:?}"
        );
    }
}

/// An overlay come up over a field holding the keyboard — opened by its
/// chord from inside the field — takes the keyboard from it, unless it
/// focuses a field of its own (the finder): left in the Ask input under a
/// graph or a menu, the Enter that seemed to go nowhere asked the hidden
/// question.
#[test]
fn an_overlay_come_up_over_a_field_takes_the_keyboard_from_it() {
    let mut renderer = renderer();
    // The search box holding the keys, then `message`: what has them after.
    let mut focused_after = |message: Message| -> Option<iced::widget::Id> {
        let mut app = reader_app();
        // Short enough for the ⋯ menu's tools to be in it, not on the strip.
        app.window_height = TOOLS_IN_MENU_H;
        app.show_left_sidebar = true;
        app.sidebar = crate::SidebarTab::Search;
        let mut ui =
            UserInterface::build(super::view(&app), WINDOW, Cache::default(), &mut renderer);
        let _ = run_operation(
            &mut ui,
            &renderer,
            iced::advanced::widget::operation::focusable::focus::<()>(super::search_input_id()),
        );
        assert_eq!(
            run_operation(&mut ui, &renderer, find_focused()),
            Some(super::search_input_id()),
            "precondition"
        );
        let cache = ui.into_cache();
        let task = app.update(message);
        let mut ui = UserInterface::build(super::view(&app), WINDOW, cache, &mut renderer);
        for mut op in widget_ops(task) {
            loop {
                ui.operate(&renderer, &mut black_box(&mut *op));
                match op.finish() {
                    Outcome::Chain(next) => op = next,
                    _ => break,
                }
            }
        }
        run_operation(&mut ui, &renderer, find_focused())
    };
    for message in [
        Message::Window(WindowMsg::OpenShortcuts),
        Message::Window(WindowMsg::ToggleToolsMenu),
    ] {
        let what = format!("{message:?}");
        assert_eq!(focused_after(message), None, "{what}");
    }
    // Not something that brings up no overlay, nor the finder: its own
    // input takes the keys.
    assert_eq!(focused_after(Message::Noop), Some(super::search_input_id()));
    assert_eq!(
        focused_after(Message::Nav(NavMsg::FinderOpened(
            crate::ui::FinderMode::Files
        ))),
        Some(super::finder_input_id())
    );
}

/// I10: the shortcuts — Settings' page of them, which the menu's Keyboard
/// Shortcuts opens — list every rebindable action, the ten added in wave 1
/// included, each with its chord.
#[test]
fn the_shortcuts_modal_lists_every_action_with_its_chord() {
    use crate::keymap::Action;
    let mut app = reader_app();
    let _ = app.update(Message::Window(WindowMsg::OpenShortcuts));
    assert!(app.settings.open && app.settings.page == crate::SettingsPage::Shortcuts);
    let mut sim = sim_of(&app);
    for action in [
        Action::ToggleAsk,
        Action::StartDebug,
        Action::CallGraph,
        Action::ImportGraph,
        Action::ExplainAll,
        Action::ToggleDiff,
        Action::TimeTravel,
        Action::Walkthrough,
        Action::LspServers,
        Action::Shortcuts,
    ] {
        assert!(
            sim.find(action.label()).is_ok(),
            "{:?} is missing from the shortcuts modal",
            action.label()
        );
    }
    for action in Action::ALL {
        let caps = app.keymap.chord(action).caps();
        assert!(
            sim.find(caps.as_str()).is_ok(),
            "{:?}'s chord {caps:?} is not shown",
            action.label()
        );
    }
}

fn scanned_key() -> crate::connect::ScannedHostKey {
    crate::connect::ScannedHostKey {
        host: "[example.com]:2222".into(),
        line: "[example.com]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEd25519KeyForTestsOnly0000"
            .into(),
        kind: "ED25519".into(),
        fingerprint: "SHA256:Ed25519PrintForTests".into(),
    }
}

fn port_2222_target() -> crate::connect::ConnTarget {
    crate::connect::SavedConnection {
        name: String::new(),
        host: "example.com".into(),
        user: "root".into(),
        port: 2222,
        identity: String::new(),
        send_ai_keys: false,
    }
    .target()
}

/// I10 / I6: the Connect modal's unknown-host prompt shows the key's type and
/// fingerprint with Trust / Cancel, and Trust names the fingerprint it shows.
#[test]
fn the_unknown_host_prompt_shows_the_fingerprint_and_trust_names_it() {
    let mut app = reader_app();
    app.connect = Some(crate::ConnectUi {
        stage: crate::ConnectStage::TrustHost {
            target: port_2222_target(),
            reason: "refused".into(),
            key: scanned_key(),
        },
        ..Default::default()
    });
    let mut sim = sim_of(&app);
    assert!(sim.find("Unknown host key").is_ok());
    assert!(sim.find("ED25519 key").is_ok());
    assert!(sim.find("SHA256:Ed25519PrintForTests").is_ok());
    assert!(shows(&mut sim, "Trust it only if this fingerprint matches"));
    let sent = click(sim, "Trust and connect");
    assert!(
        matches!(sent.as_slice(), [Message::Connect(ConnectMsg::TrustHost { fingerprint })]
            if fingerprint == "SHA256:Ed25519PrintForTests"),
        "{sent:?}"
    );
    let sent = click(sim_of(&app), "Cancel");
    assert!(
        matches!(sent.as_slice(), [Message::Connect(ConnectMsg::TrustCancel)]),
        "{sent:?}"
    );
}

/// I6: an unknown host is trusted only by a click on the key shown. The
/// prompt comes from the live transport only; a click naming another key
/// records nothing; Trust records exactly the key line in clew's own
/// known_hosts and reconnects; Cancel goes back to the form with the reason.
#[test]
fn an_unknown_host_is_trusted_only_by_a_click_on_the_key_shown() {
    let mut app = blank_app();
    app.connect = Some(crate::ConnectUi::default());
    let _ = app.connect_to(port_2222_target());
    let conn = app.conn_gen;

    // A late message from a replaced transport raises nothing.
    let _ = app.update(Message::Server(ServerMsg::HostKeyUnknown {
        conn: conn - 1,
        reason: "old".into(),
        key: scanned_key(),
    }));
    assert!(matches!(
        app.connect.as_ref().map(|u| &u.stage),
        Some(crate::ConnectStage::Connecting { .. })
    ));

    let _ = app.update(Message::Server(ServerMsg::HostKeyUnknown {
        conn,
        reason: "refused: unknown key".into(),
        key: scanned_key(),
    }));
    assert!(matches!(
        app.connect.as_ref().map(|u| &u.stage),
        Some(crate::ConnectStage::TrustHost { key, .. }) if *key == scanned_key()
    ));

    // Cancel: back to the form, saying why. A connect clew made itself
    // (not asked from the form) has nowhere else to go back to: the window
    // stays on the host, to trust it from there.
    let _ = app.update(Message::Connect(ConnectMsg::TrustCancel));
    assert!(matches!(
        app.connect.as_ref().map(|u| &u.stage),
        Some(crate::ConnectStage::Error(reason)) if reason == "refused: unknown key"
    ));
    assert_eq!(app.connection, port_2222_target());
    // Asked again, the key is shown again.
    let _ = app.connect_to(port_2222_target());
    let conn = app.conn_gen;
    let _ = app.update(Message::Server(ServerMsg::HostKeyUnknown {
        conn,
        reason: "refused: unknown key".into(),
        key: scanned_key(),
    }));

    // A data directory whose path has spaces in it.
    let data = crate::app::tests::test_dir("ui trust");
    std::fs::create_dir_all(&data).unwrap();
    let _env = crate::app::tests::data_dir_override(&data);

    let _ = app.update(Message::Connect(ConnectMsg::TrustHost {
        fingerprint: "SHA256:SomeOtherKey".into(),
    }));
    let file = data.join("known_hosts");
    assert!(!file.exists(), "a click for another key recorded something");
    assert_eq!(app.conn_gen, conn);

    // Not before it can have been read.
    app.asking.delay = std::time::Duration::from_secs(5);
    let _ = app.update(Message::Noop);
    let _ = app.update(Message::Connect(ConnectMsg::TrustHost {
        fingerprint: "SHA256:Ed25519PrintForTests".into(),
    }));
    assert!(!file.exists(), "trusted unread");
    let delay = app.asking.delay;
    crate::app::tests::read_for(&mut app, delay);

    let _ = app.update(Message::Connect(ConnectMsg::TrustHost {
        fingerprint: "SHA256:Ed25519PrintForTests".into(),
    }));
    let recorded = std::fs::read_to_string(&file);

    assert_eq!(recorded.unwrap(), format!("{}\n", scanned_key().line));
    assert_eq!(app.conn_gen, conn + 1, "trusting reconnects");
    assert_eq!(app.connection, port_2222_target());
    assert!(matches!(
        app.connect.as_ref().map(|u| &u.stage),
        Some(crate::ConnectStage::Connecting { .. })
    ));
}

/// The tap that raises the window — its focus, a press and a release in
/// one batch — landing on an Allow drawn enabled answers nothing: the focus
/// is told first (`ui::placed::watch_overlays`), and the question waits
/// from it. A window that opened without the focus is told none at start.
/// The same press and release, the window focused already, do answer.
#[test]
fn the_tap_that_raises_the_window_answers_nothing() {
    use crate::app::model::Question;
    use iced_test::selector::{Candidate, Selector};
    let data = crate::app::tests::test_dir("ui trust tap");
    std::fs::create_dir_all(&data).unwrap();
    let _env = crate::app::tests::data_dir_override(&data);
    let known_hosts = data.join("known_hosts");
    let delay = std::time::Duration::from_secs(5);
    // The key asked about, its Allow drawn where it was reported and read
    // there; then `events` on the Allow, and what the app made of them.
    let tapped = |events: &[Event]| -> (App, Vec<Message>) {
        let mut app = reader_app();
        app.asking.delay = delay;
        app.connection = port_2222_target();
        let key = scanned_key();
        app.connect = Some(crate::ConnectUi {
            stage: crate::ConnectStage::TrustHost {
                target: port_2222_target(),
                reason: "refused".into(),
                key: key.clone(),
            },
            ..Default::default()
        });
        let question = Question::TrustHost(key.fingerprint.clone());
        let _ = app.update(Message::Noop);
        let mut renderer = renderer();
        let mut ui =
            UserInterface::build(super::view(&app), WINDOW, Cache::default(), &mut renderer);
        let mut sent = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        let cache = ui.into_cache();
        for message in sent.drain(..) {
            let _ = app.update(message);
        }
        // Read where it is: the delay gone by, for it and its place.
        app.asking.since = app.asking.since.checked_sub(delay).unwrap();
        for p in &mut app.asking.places {
            p.since = p.since.checked_sub(delay).unwrap();
        }
        assert!(
            app.may_allow(&question),
            "precondition: its Allow is enabled"
        );

        let mut ui = UserInterface::build(super::view(&app), WINDOW, cache, &mut renderer);
        let mut find = (|c: Candidate<'_>| match c {
            Candidate::Text {
                content: "Trust and connect",
                visible_bounds,
                ..
            } => visible_bounds,
            _ => None,
        })
        .find();
        ui.operate(&renderer, &mut black_box(&mut find));
        let allow = match Operation::finish(&find) {
            Outcome::Some(Some(at)) => at,
            other => panic!("the Allow is not on screen: {other:?}"),
        };
        let _ = ui.update(
            events,
            mouse::Cursor::Available(allow.center()),
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        drop(ui);
        for message in sent.clone() {
            let _ = app.update(message);
        }
        (app, sent)
    };
    let click = [
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
    ];
    let tap: Vec<Event> = std::iter::once(Event::Window(window::Event::Focused))
        .chain(click.iter().cloned())
        .collect();

    let (app, sent) = tapped(&tap);
    let focus = sent
        .iter()
        .position(|m| matches!(m, Message::Window(crate::WindowMsg::FocusChanged(true))));
    let trust = sent
        .iter()
        .position(|m| matches!(m, Message::Connect(ConnectMsg::TrustHost { .. })));
    assert!(
        matches!((focus, trust), (Some(f), Some(t)) if f < t),
        "the focus told after the press it came with: {sent:?}"
    );
    assert!(
        matches!(
            app.connect.as_ref().map(|u| &u.stage),
            Some(crate::ConnectStage::TrustHost { .. })
        ),
        "the tap that raised the window trusted the key"
    );
    assert!(!known_hosts.exists());

    // The control: the window focused already, the same click trusts —
    // the key recorded, the prompt gone.
    let (app, _) = tapped(&click);
    assert_eq!(
        std::fs::read_to_string(&known_hosts).ok(),
        Some(format!("{}\n", scanned_key().line)),
        "a click on the enabled Allow trusts"
    );
    assert!(app.connect.is_none());
}

/// A CHANGED key clew itself recorded can be forgotten from the Connect
/// modal. The prompt comes from the live transport only; Forget names the host
/// it was drawn for, so a click naming another host forgets nothing; the right
/// one removes exactly that host's lines from clew's own known_hosts and
/// reconnects (the new key then meets the unknown-key prompt, to be checked
/// before it is trusted); Cancel goes back to the form with the reason.
#[test]
fn a_changed_key_clew_recorded_is_forgotten_only_by_a_click_on_the_prompt() {
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEd25519KeyForTestsOnly0000";
    let mut app = blank_app();
    app.connect = Some(crate::ConnectUi::default());
    let _ = app.connect_to(port_2222_target());
    let conn = app.conn_gen;
    let changed = |conn| {
        Message::Server(ServerMsg::HostKeyChanged {
            conn,
            reason: "refused: the host key changed".into(),
            forget_host: "[example.com]:2222".into(),
        })
    };
    fn stage(app: &App) -> Option<&crate::ConnectStage> {
        app.connect.as_ref().map(|u| &u.stage)
    }

    // A late message from a replaced transport raises nothing.
    let _ = app.update(changed(conn - 1));
    assert!(matches!(
        stage(&app),
        Some(crate::ConnectStage::Connecting { .. })
    ));
    let _ = app.update(changed(conn));
    assert!(matches!(
        stage(&app),
        Some(crate::ConnectStage::HostKeyChanged { forget_host, .. })
            if forget_host == "[example.com]:2222"
    ));
    let mut sim = sim_of(&app);
    assert!(sim.find("Host key changed").is_ok());
    assert!(shows(&mut sim, "refused: the host key changed"));
    let sent = click(sim, "Forget the old key");
    assert!(
        matches!(sent.as_slice(), [Message::Connect(ConnectMsg::ForgetHostKey { host })]
            if host == "[example.com]:2222"),
        "{sent:?}"
    );

    // Cancel: back to the form, saying why. A connect clew made itself
    // (not asked from the form) has nowhere else to go back to: the window
    // stays on the host.
    let _ = app.update(Message::Connect(ConnectMsg::TrustCancel));
    assert!(matches!(
        stage(&app),
        Some(crate::ConnectStage::Error(reason)) if reason == "refused: the host key changed"
    ));
    assert_eq!(app.connection, port_2222_target());
    // Asked again, the refusal is shown again.
    let _ = app.connect_to(port_2222_target());
    let conn = app.conn_gen;
    let _ = app.update(changed(conn));

    // A data directory whose path has spaces in it.
    let data = crate::app::tests::test_dir("ui forget");
    std::fs::create_dir_all(&data).unwrap();
    let _env = crate::app::tests::data_dir_override(&data);
    let file = data.join("known_hosts");
    let before = format!("[example.com]:2222 {KEY}\nother.example {KEY}\n");
    std::fs::write(&file, &before).unwrap();

    let _ = app.update(Message::Connect(ConnectMsg::ForgetHostKey {
        host: "other.example".into(),
    }));
    let untouched = std::fs::read_to_string(&file);
    let conn_after_stray = app.conn_gen;
    let _ = app.update(Message::Connect(ConnectMsg::ForgetHostKey {
        host: "[example.com]:2222".into(),
    }));
    let after = std::fs::read_to_string(&file);

    assert_eq!(untouched.unwrap(), before, "a stray click forgot a key");
    assert_eq!(conn_after_stray, conn, "a stray click reconnected");
    assert_eq!(after.unwrap(), format!("other.example {KEY}\n"));
    assert_eq!(app.conn_gen, conn + 1, "forgetting reconnects");
    assert!(matches!(
        stage(&app),
        Some(crate::ConnectStage::Connecting { .. })
    ));
    assert!(
        app.status.contains("old key — connecting"),
        "{}",
        app.status
    );
}

/// The forget-key reconnect drops the project as every switch does, and what
/// that discarded is still said: the reconnect's own line used to overwrite it.
#[test]
fn forgetting_a_key_keeps_the_discard_notice() {
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEd25519KeyForTestsOnly0000";
    let mut app = blank_app();
    app.connect = Some(crate::ConnectUi::default());
    let _ = app.connect_to(port_2222_target());
    let _ = app.update(Message::Server(ServerMsg::HostKeyChanged {
        conn: app.conn_gen,
        reason: "refused: the host key changed".into(),
        forget_host: "[example.com]:2222".into(),
    }));
    // Unsent remote state, and a typed note, held when the key is forgotten.
    app.proj.remote_state_dirty.insert("notes.json".into());
    app.proj.note_edit = Some(("src/lib.rs".into(), 1, "half a thought".into()));
    let data = crate::app::tests::test_dir("ui forget notice");
    std::fs::create_dir_all(&data).unwrap();
    let _env = crate::app::tests::data_dir_override(&data);
    std::fs::write(
        data.join("known_hosts"),
        format!("[example.com]:2222 {KEY}\n"),
    )
    .unwrap();
    let _ = app.update(Message::Connect(ConnectMsg::ForgetHostKey {
        host: "[example.com]:2222".into(),
    }));
    assert!(
        app.status.contains("old key — connecting"),
        "{}",
        app.status
    );
    assert!(
        app.status
            .contains("Changes not yet saved on the remote host were discarded: notes.json"),
        "{}",
        app.status
    );
    assert!(
        app.status.contains("Unsaved note was discarded"),
        "{}",
        app.status
    );
}

/// Leave to build is given to one project, and its Allow says which, by
/// the project's folder name.
#[test]
fn leave_to_build_names_the_project_it_is_for() {
    let consent = crate::LspConsent {
        on_path: None,
        language: "rust".into(),
        server_name: "rust-analyzer".into(),
        version: "1".into(),
        provision: crate::LspProvision::Build {
            runs: "build scripts".into(),
            root: PathBuf::from("/work/shop"),
            host: None,
        },
        dest_dir: PathBuf::new(),
    };
    let sim = sim_elem(super::lsp_consent_modal(&consent, 3, 0, true));
    assert!(matches!(
        click(sim, "Allow in shop").as_slice(),
        [Message::Lsp(crate::LspMsg::ConsentAllowed(3))]
    ));
}

/// L22: the download question offers the server's own program already on
/// this computer, saying where it is — and only when one was found. Its
/// button, like Allow, counts only once the question has been read.
#[test]
fn the_download_question_offers_the_program_already_here() {
    let mut consent = crate::LspConsent {
        on_path: None,
        language: "rust".into(),
        server_name: "rust-analyzer".into(),
        version: "1".into(),
        provision: crate::LspProvision::Download(clew_core::lsp::registry::Download {
            url: "https://example.invalid/rust-analyzer.gz",
            sha256: clew_core::lsp::registry::Sha256::of(b""),
            archive: clew_core::lsp::registry::Archive::Gzip,
            binary: "rust-analyzer",
        }),
        dest_dir: PathBuf::new(),
    };
    {
        let mut sim = sim_elem(super::lsp_consent_modal(&consent, 4, 0, true));
        assert!(shows(&mut sim, "Download"));
        assert!(!shows(&mut sim, "Use that one"));
        assert!(!shows(&mut sim, "already on this computer"));
    }

    consent.on_path = Some(PathBuf::from("/opt/tools/rust-analyzer"));
    let mut sim = sim_elem(super::lsp_consent_modal(&consent, 4, 0, true));
    assert!(shows(
        &mut sim,
        "rust-analyzer is already on this computer, at /opt/tools/rust-analyzer."
    ));
    assert!(matches!(
        click(sim, "Use that one").as_slice(),
        [Message::Lsp(crate::LspMsg::ConsentUsePath(4))]
    ));
    // The program already here is the primary answer: a spoken "yes" picks
    // it, and the download stays one press (or its name) away.
    let yes: Vec<Message> =
        crate::ui::heard::gather_of(|| super::lsp_consent_modal(&consent, 4, 0, true))
            .into_iter()
            .filter(|t| t.answer == Some(crate::ui::heard::Answer::Yes))
            .map(|t| t.message)
            .collect();
    assert!(
        matches!(
            yes.as_slice(),
            [Message::Lsp(crate::LspMsg::ConsentUsePath(4))]
        ),
        "{yes:?}"
    );
    let sim = sim_elem(super::lsp_consent_modal(&consent, 4, 0, true));
    assert!(matches!(
        click(sim, "Download").as_slice(),
        [Message::Lsp(crate::LspMsg::ConsentAllowed(4))]
    ));
    let sim = sim_elem(super::lsp_consent_modal(&consent, 4, 0, false));
    assert!(click(sim, "Use that one").is_empty(), "not yet read");
}

/// F#8: consent questions are asked one at a time, and the modal says when
/// more wait behind the one on screen.
#[test]
fn the_consent_modal_says_how_many_more_wait() {
    let consent = crate::LspConsent {
        on_path: None,
        language: "rust".into(),
        server_name: "rust-analyzer".into(),
        version: "1".into(),
        provision: crate::LspProvision::Remote {
            describe: "install it".into(),
            consent: "digest".into(),
        },
        dest_dir: PathBuf::new(),
    };
    let mut sim = sim_elem(super::lsp_consent_modal(&consent, 1, 2, true));
    assert!(shows(&mut sim, "2 more after this"));
    let mut sim = sim_elem(super::lsp_consent_modal(&consent, 1, 0, true));
    assert!(shows(&mut sim, "rust-analyzer 1"));
    assert!(!shows(&mut sim, "more after this"));

    // Its Allow is pressed only once it can have been read, and names the
    // question; its decline at once.
    let sim = sim_elem(super::lsp_consent_modal(&consent, 7, 0, false));
    assert!(click(sim, "Download").is_empty(), "allowed unread");
    let sim = sim_elem(super::lsp_consent_modal(&consent, 7, 0, true));
    assert!(matches!(
        click(sim, "Download").as_slice(),
        [Message::Lsp(crate::LspMsg::ConsentAllowed(7))]
    ));
    let sim = sim_elem(super::lsp_consent_modal(&consent, 7, 0, false));
    assert!(matches!(
        click(sim, "Not now").as_slice(),
        [Message::Lsp(crate::LspMsg::ConsentDismissed(7))]
    ));
}

/// A first open's setup sheet for `root`, before its look at the project
/// is in — the question that used to be the `.clew` consent alone.
fn open_sheet(
    root: &std::path::Path,
    asked: impl Into<super::placed::Asked>,
) -> Element<'static, Message> {
    let app: &'static crate::app::state::App = Box::leak(Box::new(reader_app()));
    let root: &'static std::path::Path = Box::leak(root.to_path_buf().into_boxed_path());
    super::setup::setup_sheet(app, root, None, asked)
}

/// The setup sheet says, in a line each, where clew keeps its data, what
/// its servers take and what explaining costs; its ticks and buttons send
/// what they say.
#[test]
fn the_setup_sheet_says_what_each_choice_takes() {
    use crate::app::setup::{Census, Fetch, ProjectSetup, SetupMode, SetupServer};
    let mut app = reader_app();
    app.llm_available = true;
    app.embed_available = true;
    let root = PathBuf::from("/nonexistent/iced");
    app.setup = Some(ProjectSetup {
        mode: SetupMode::Open,
        root: root.clone(),
        servers: true,
        servers_declined: false,
        explain: false,
        census: Some(Census {
            servers: vec![SetupServer {
                language: "rust".into(),
                name: "rust-analyzer".into(),
                fetch: Fetch::Download,
                builds: true,
            }],
            calls: Some(3617),
        }),
    });
    fn sheet<'a>(
        app: &'a crate::app::state::App,
        root: &'a std::path::Path,
    ) -> Element<'a, Message> {
        super::setup::setup_sheet(app, root, app.setup.as_ref(), true)
    }
    let mut sim = sim_elem(sheet(&app, &root));
    for said in [
        "Open iced",
        "/nonexistent/iced/.clew",
        "Language servers",
        "rust-analyzer: download, builds the project (runs its code)",
        "Explain the project",
        "Also builds the semantic index.",
        "Open without setup",
    ] {
        assert!(shows(&mut sim, said), "{said:?}");
    }
    // And what those calls are likely to cost, with their count, in one
    // sentence of one weight.
    let estimate = app.explain_estimate(3617).expect("an estimate");
    let said = estimate.said(3617);
    assert!(said.starts_with("About 3,600 AI calls: "), "{said:?}");
    assert!(shows(&mut sim, &said), "{said:?}");
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Language servers").as_slice(),
        [Message::Project(ProjectMsg::SetupServers(false))]
    ));
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Explain the project").as_slice(),
        [Message::Project(ProjectMsg::SetupExplain(true))]
    ));
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Open").as_slice(),
        [Message::Project(ProjectMsg::ConsentAllowed(r))] if *r == root
    ));
    // Explain All asking first: what it costs, and Explain.
    drop(sim);
    app.setup.as_mut().unwrap().mode = SetupMode::Explain;
    app.setup.as_mut().unwrap().explain = true;
    let mut sim = sim_elem(sheet(&app, &root));
    assert!(shows(&mut sim, "Explain iced?"));
    assert!(!shows(&mut sim, "Language servers"));
    // Asking first: the calls, their tokens and their money as three
    // figures of one weight, and where they come from.
    for said in [
        "3,600".to_string(),
        "AI calls".to_string(),
        crate::app::usage::tokens(estimate.tokens),
        "Tokens".to_string(),
        "Estimated cost".to_string(),
        estimate.basis(),
        "Also builds the semantic index.".to_string(),
    ] {
        assert!(shows(&mut sim, &said), "{said:?}");
    }
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Explain").as_slice(),
        [Message::Project(ProjectMsg::SetupApply)]
    ));
}

/// The first-open sheet holds still while it looks at the project: the
/// servers' row is there from the first frame, ticked as it will be, and
/// keeps its room, so the look landing — with a server to set up or none —
/// neither ticks anything nor moves Open; Open waits for the look (it sets
/// up what the look found), Open without setup does not; and its × says that it
/// does not open the project, beside Open without setup, which does.
#[test]
fn the_setup_sheet_holds_still_while_it_looks() {
    use crate::app::setup::{Census, Fetch, ProjectSetup, SetupMode, SetupServer};
    let mut app = reader_app();
    let root = PathBuf::from("/nonexistent/iced");
    app.setup = Some(ProjectSetup {
        mode: SetupMode::Open,
        root: root.clone(),
        servers: true,
        servers_declined: false,
        explain: false,
        census: None,
    });
    fn sheet<'a>(
        app: &'a crate::app::state::App,
        root: &'a std::path::Path,
    ) -> Element<'a, Message> {
        super::setup::setup_sheet(app, root, app.setup.as_ref(), true)
    }
    let open_at = |app: &crate::app::state::App| {
        let mut sim = sim_elem(sheet(app, &root));
        text_at(&mut sim, |t| t == "Open", 0).expect("Open")
    };
    let mut sim = sim_elem(sheet(&app, &root));
    assert!(shows(&mut sim, "Language servers"));
    assert!(shows(&mut sim, "Checking the project"));
    assert!(!shows(&mut sim, "Not now"));
    drop(sim);
    let row = |app: &crate::app::state::App| {
        super::setup::servers_row(app.setup.as_ref(), SetupMode::Open, false)
            .map(|(on, changeable, _)| (on, changeable))
    };
    assert_eq!(row(&app), Some((true, false)), "ticked as it will be");
    assert!(
        click(sim_elem(sheet(&app, &root)), "Open").is_empty(),
        "Open waits for the look"
    );
    assert!(
        click(sim_elem(sheet(&app, &root)), "Language servers").is_empty(),
        "nothing to choose yet"
    );
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Open without setup").as_slice(),
        [Message::Project(ProjectMsg::OpenWithoutSetup(r))] if *r == root
    ));
    let looking = open_at(&app);
    // Held, Open says why where the pointer rests (L24).
    let why = "Ready once the project is checked";
    let says_why = |app: &crate::app::state::App| {
        let at = open_at(app).center();
        super::tip::warm_now();
        let mut sim = sim_elem(sheet(app, &root));
        sim.point_at(at);
        let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
            position: at,
        })]);
        shows(&mut sim, why)
    };
    assert!(says_why(&app), "a held Open says nothing of why");

    // A server to set up: listed, still ticked, Open where it was.
    app.setup.as_mut().unwrap().census = Some(Census {
        servers: vec![SetupServer {
            language: "rust".into(),
            name: "rust-analyzer".into(),
            fetch: Fetch::Download,
            builds: false,
        }],
        calls: Some(40),
    });
    let mut sim = sim_elem(sheet(&app, &root));
    assert!(shows(&mut sim, "rust-analyzer: download"));
    drop(sim);
    assert_eq!(open_at(&app), looking, "the look landing moved Open");
    assert_eq!(row(&app), Some((true, true)));
    // Servers a line and more long: still in the room the line kept.
    let two = Census {
        servers: vec![
            SetupServer {
                language: "rust".into(),
                name: "rust-analyzer".into(),
                fetch: Fetch::Download,
                builds: true,
            },
            SetupServer {
                language: "c".into(),
                name: "clangd".into(),
                fetch: Fetch::Download,
                builds: false,
            },
            SetupServer {
                language: "zig".into(),
                name: "zls".into(),
                fetch: Fetch::Download,
                builds: true,
            },
        ],
        calls: Some(40),
    };
    let one = app.setup.as_mut().unwrap().census.replace(two);
    let lines = {
        let mut sim = sim_elem(sheet(&app, &root));
        text_at(&mut sim, |t| t.starts_with("rust-analyzer: download"), 0).expect("the list")
    };
    assert!(
        lines.height > 20.0 && lines.height < 40.0,
        "the list takes two lines: {lines:?}"
    );
    assert_eq!(open_at(&app), looking, "two lines of servers moved Open");
    app.setup.as_mut().unwrap().census = one;
    assert!(
        matches!(
            click(sim_elem(sheet(&app, &root)), "Language servers").as_slice(),
            [Message::Project(ProjectMsg::SetupServers(false))]
        ),
        "ticked as it was"
    );
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Open").as_slice(),
        [Message::Project(ProjectMsg::ConsentAllowed(r))] if *r == root
    ));
    assert!(!says_why(&app), "Open, ready, still says it is held");

    // None to set up: said, not taken away from under Open.
    app.setup.as_mut().unwrap().census = Some(Census {
        servers: Vec::new(),
        calls: Some(40),
    });
    app.setup.as_mut().unwrap().servers = false;
    let mut sim = sim_elem(sheet(&app, &root));
    assert!(shows(&mut sim, "Language servers"));
    assert!(shows(&mut sim, "Ready."));
    drop(sim);
    assert_eq!(open_at(&app), looking, "the row left from under Open");
    assert_eq!(row(&app), Some((true, false)), "done, not declined");
    // On another machine the look has nothing to say from the first frame:
    // no row, which is no change.
    assert_eq!(
        super::setup::servers_row(app.setup.as_ref(), SetupMode::Open, true),
        None
    );

    // The ×: closes without opening, and says so.
    let title = {
        let mut sim = sim_elem(sheet(&app, &root));
        text_at(&mut sim, |t| t == "Open iced", 0).expect("the title")
    };
    let mut said = false;
    for dx in (0..120).map(|i| i as f32 * 4.0) {
        let at = Point::new(title.x + title.width + dx, title.center_y());
        super::tip::warm_now();
        let mut sim = sim_elem(sheet(&app, &root));
        sim.point_at(at);
        let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
            position: at,
        })]);
        if shows(&mut sim, "Close without opening") {
            said = true;
            let _ = sim.simulate(iced_test::simulator::click());
            assert!(matches!(
                sent_by(sim).as_slice(),
                [Message::Project(ProjectMsg::ConsentDenied(r))] if *r == root
            ));
            break;
        }
    }
    assert!(said, "the × does not say it closes without opening");
}

/// Every question's Allow is drawn disabled until it can have been read:
/// a click on it sends nothing; its decline is pressed at once.
#[test]
fn an_unread_questions_allow_sends_nothing() {
    let root = PathBuf::from("/nonexistent/clew-ui-unread");
    let sim = sim_elem(open_sheet(&root, false));
    assert!(click(sim, "Open").is_empty());
    let sim = sim_elem(open_sheet(&root, true));
    assert!(matches!(
        click(sim, "Open").as_slice(),
        [Message::Project(ProjectMsg::ConsentAllowed(r))] if *r == root
    ));
    // A first open's Open without setup opens the project too, with nothing set
    // up: an answer as Open is, pressed only once it can have been read.
    let sim = sim_elem(open_sheet(&root, false));
    assert!(click(sim, "Open without setup").is_empty());
    let sim = sim_elem(open_sheet(&root, true));
    assert!(matches!(
        click(sim, "Open without setup").as_slice(),
        [Message::Project(ProjectMsg::OpenWithoutSetup(r))] if *r == root
    ));
    let pending = crate::PendingLspCommand {
        root: root.clone(),
        host: None,
        language: "rust".into(),
        command: Some(root.join("run.sh")),
        args: Vec::new(),
        server_name: "rust-analyzer".into(),
        version: "x".into(),
        fingerprint: "fp".into(),
        init_options: None,
    };
    let sim = sim_elem(super::lsp_command_modal(&pending, 4, 0, false));
    assert!(click(sim, "Run it").is_empty());
    let sim = sim_elem(super::lsp_command_modal(&pending, 4, 0, false));
    assert!(matches!(
        click(sim, "Don't run").as_slice(),
        [Message::Lsp(crate::LspMsg::CommandDismissed(4))]
    ));
    let target = port_2222_target();
    let key = scanned_key();
    let sim = sim_elem(super::connect::trust_host_prompt(&target, &key, false));
    assert!(click(sim, "Trust and connect").is_empty());
    let sim = sim_elem(super::connect::host_key_changed_prompt(
        &target, "changed", "host", false,
    ));
    assert!(click(sim, "Forget the old key").is_empty());
    let sim = sim_elem(super::connect::host_key_changed_prompt(
        &target, "changed", "host", true,
    ));
    assert!(matches!(
        click(sim, "Forget the old key").as_slice(),
        [Message::Connect(ConnectMsg::ForgetHostKey { host })] if host == "host"
    ));
    let approval = crate::app::model::Approval {
        key: crate::app::model::ApprovalKey::Mcp { call: 3 },
        mark: None,
        title: "run it".into(),
        detail: String::new(),
        options: vec![
            crate::app::model::ApprovalOption {
                id: "once".into(),
                label: "Allow once".into(),
                allows: true,
                always: false,
            },
            crate::app::model::ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                allows: false,
                always: false,
            },
        ],
    };
    let sim = sim_elem(super::panels::approval_card(9, &approval, false, 200.0, 0));
    assert!(click(sim, "Allow once").is_empty());
    let sim = sim_elem(super::panels::approval_card(9, &approval, false, 200.0, 0));
    assert!(matches!(
        click(sim, "Deny").as_slice(),
        [Message::Ask(AskMsg::Approve { stream: 9, option, .. })] if option == "deny"
    ));
}

/// A question said rather than typed is marked "Spoken" over its bubble; a
/// typed one is not.
#[test]
fn a_spoken_question_is_marked_said() {
    let mut app = reader_app();
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 1,
        question: "where is the origin?".into(),
        spoken: true,
        ..Default::default()
    });
    {
        let mut sim = sim_elem(super::ask_panel(&app, 560.0));
        assert!(shows(&mut sim, "where is the origin?"));
        assert!(shows(&mut sim, "Spoken"));
    }
    app.proj.ask_turns[0].spoken = false;
    let mut sim = sim_elem(super::ask_panel(&app, 560.0));
    assert!(!shows(&mut sim, "Spoken"));
}

/// The composer has one button, at its end: while an answer comes in, with
/// nothing typed it stops the answer, and with something typed it queues
/// that; at rest it asks. A Stop of its own beside the field said the same
/// thing in another place, and moved the field as it came and went.
#[test]
fn the_composers_one_button_stops_queues_or_asks() {
    let size = Size::new(800.0, 560.0);
    let press = |app: &App| -> Vec<Message> {
        crate::app::tests::isolated_data_dir();
        crate::fonts::load();
        let mut sim = iced_test::Simulator::with_size(
            Default::default(),
            size,
            super::panels::ask_panel(app, size.height),
        );
        // The composer's last button, at its foot.
        sim.point_at(Point::new(
            size.width - super::COMPOSER_PAD.right - super::ROUND_BUTTON / 2.0,
            size.height - super::COMPOSER_PAD.bottom - super::ROUND_BUTTON / 2.0,
        ));
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    let mut app = reader_app();
    app.proj.asking = true;
    assert!(
        !shows(
            &mut sim_elem(super::panels::ask_panel(&app, size.height)),
            "Stop"
        ),
        "no Stop of its own"
    );
    assert!(matches!(
        press(&app).as_slice(),
        [Message::Ask(AskMsg::Stop)]
    ));
    app.proj.ask_input = "and then?".into();
    assert!(matches!(
        press(&app).as_slice(),
        [Message::Ask(AskMsg::Submit)]
    ));
    app.proj.asking = false;
    assert!(matches!(
        press(&app).as_slice(),
        [Message::Ask(AskMsg::Submit)]
    ));
}

/// A field of the Connect form keeps the keyboard while what comes before
/// the form changes — an error shown by the Enter that tried to connect,
/// a host saved from another window: typed on, it is still that field.
#[test]
fn a_connect_field_keeps_the_keyboard_as_the_form_s_head_changes() {
    use iced_test::selector::{Candidate, Selector};
    let app = reader_app();
    let form = crate::ConnectUi::default();
    let size = Size::new(560.0, 700.0);
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        super::connect::connect_picker(&app, &form, None),
        size,
        Cache::default(),
        &mut renderer,
    );
    let mut find = (|c: Candidate<'_>| match c {
        Candidate::Text {
            content: "User",
            visible_bounds,
            ..
        } => visible_bounds,
        _ => None,
    })
    .find();
    ui.operate(&renderer, &mut black_box(&mut find));
    let user = match Operation::finish(&find) {
        Outcome::Some(Some(at)) => at,
        other => panic!("the User field is not on screen: {other:?}"),
    };
    // Its input, under its label.
    let at = mouse::Cursor::Available(Point::new(user.x + 20.0, user.y + user.height + 14.0));
    let mut sent = Vec::new();
    let _ = ui.update(
        &[
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        at,
        &mut renderer,
        &mut clipboard::Null,
        &mut sent,
    );
    let cache = ui.into_cache();
    // Enter showed an error by Connect; typed on.
    let mut ui = UserInterface::build(
        super::connect::connect_picker(&app, &form, Some("Host and user are required.")),
        size,
        cache,
        &mut renderer,
    );
    let typed = Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Character("r".into()),
        modified_key: keyboard::Key::Character("r".into()),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: Some("r".into()),
        repeat: false,
    });
    let mut sent = Vec::new();
    let _ = ui.update(
        &[typed],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut sent,
    );
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Connect(ConnectMsg::Field(crate::ConnectField::User, typed))] if typed == "r"
        ),
        "{sent:?}"
    );
}

/// One runtime turn of the Ask panel alone, at its default size, over
/// `cache`: `events` delivered with the pointer at `cursor`, then the
/// conversation scrolled to its end, as each answer token and tool call has
/// it (`ask.rs`), and what is found then — the visible bounds of the text
/// `label`, and the widget with the focus.
fn ask_panel_turn(
    app: &App,
    cache: Cache,
    renderer: &mut iced::Renderer,
    events: (&[Event], mouse::Cursor),
    label: &str,
) -> (Cache, Option<Rectangle>, Option<Id>) {
    panel_turn(
        super::panels::ask_panel,
        app,
        cache,
        renderer,
        events,
        label,
    )
}

/// [`ask_panel_turn`] with the panel `panel` builds — the large window's,
/// or the card's fitted to its conversation.
fn panel_turn(
    panel: for<'a> fn(&'a App, f32) -> Element<'a, Message>,
    app: &App,
    cache: Cache,
    renderer: &mut iced::Renderer,
    (events, cursor): (&[Event], mouse::Cursor),
    label: &str,
) -> (Cache, Option<Rectangle>, Option<Id>) {
    use iced::advanced::widget::operation::scrollable::{AbsoluteOffset, scroll_to};
    use iced_test::selector::{Candidate, Selector};
    let size = Size::new(800.0, app.bottom_height);
    let mut ui = UserInterface::build(panel(app, size.height), size, cache, renderer);
    let mut messages = Vec::new();
    let _ = ui.update(
        events,
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let end = AbsoluteOffset {
        x: Some(0.0),
        y: Some(f32::MAX),
    };
    let _ = run_operation::<()>(&mut ui, renderer, scroll_to(super::ask_scroll_id(), end));
    let wanted = label.to_string();
    let mut find = (move |c: Candidate<'_>| match c {
        Candidate::Text {
            content,
            visible_bounds,
            ..
        } if content == wanted => visible_bounds,
        _ => None,
    })
    .find();
    ui.operate(renderer, &mut black_box(&mut find));
    let found = match Operation::finish(&find) {
        Outcome::Some(found) => found,
        _ => None,
    };
    let focused = run_operation(&mut ui, renderer, find_focused());
    (ui.into_cache(), found, focused)
}

/// The approval card is asked above the conversation, from its own top,
/// its answers in view below what scrolls: its turn growing — steps, plan
/// entries, its answer, each scrolling the conversation —, more cards
/// coming and its arguments scrolled move none of its answers; the next
/// card starts from its own top; and nothing coming or going in the panel
/// takes the conversation's scroll or the input's focus.
#[test]
fn an_approval_card_stays_put_while_its_turn_grows() {
    use crate::app::model::{AgentStep, Approval, ApprovalKey, ApprovalOption, AskTurn, StepState};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    // Arguments longer than the card shows: they scroll.
    let card = |call| Approval {
        key: ApprovalKey::Mcp { call },
        mark: None,
        title: format!("run {call}"),
        detail: (0..30).map(|i| format!("arg {i}\n")).collect(),
        options: vec![ApprovalOption {
            id: "once".into(),
            label: "Allow once".into(),
            allows: true,
            always: false,
        }],
    };
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        answer_md: "so far ".repeat(2000),
        ..AskTurn::default()
    });
    let mut renderer = renderer();
    let mut cache = {
        let mut ui = UserInterface::build(
            super::panels::ask_panel(&app, app.bottom_height),
            Size::new(800.0, app.bottom_height),
            Cache::default(),
            &mut renderer,
        );
        let _ = run_operation::<()>(
            &mut ui,
            &renderer,
            iced::advanced::widget::operation::focusable::focus(super::ask_input_id()),
        );
        ui.into_cache()
    };
    let input = Some(super::ask_input_id());
    let still: (&[Event], mouse::Cursor) = (&[], mouse::Cursor::Unavailable);
    let mut turn = |app: &App, events: (&[Event], mouse::Cursor), label: &str| {
        let (next, found, focused) = ask_panel_turn(
            app,
            std::mem::take(&mut cache),
            &mut renderer,
            events,
            label,
        );
        cache = next;
        assert_eq!(focused, input, "the input lost the focus");
        found
    };
    // Read to the end of the answer so far.
    let _ = turn(&app, still, "Allow once");
    let _ = turn(&app, still, "Allow once");

    app.proj.ask_turns[0].extras.approvals.push(card(1));
    let title = turn(&app, still, "run 1").expect("the card shows from its top");
    let allow = turn(&app, still, "Allow once").expect("its answers show");
    assert_eq!(turn(&app, still, "run 1"), Some(title));

    // More cards, steps, plan entries and answer: the card stays put.
    app.proj.ask_turns[0].extras.approvals.push(card(2));
    assert_eq!(turn(&app, still, "Allow once"), Some(allow));
    assert!(turn(&app, still, "1 more waiting after this one").is_some());
    assert_eq!(turn(&app, still, "run 2"), None, "one card at a time");
    let grown = &mut app.proj.ask_turns[0];
    for i in 0..40 {
        grown.steps.push(AgentStep {
            tool: "read".into(),
            title: format!("Read file {i}.rs"),
            refs: Vec::new(),
            state: StepState::Done,
        });
        grown.extras.plan.push(clew_core::acp::PlanEntry {
            content: format!("step {i}"),
            status: "pending".into(),
        });
    }
    grown.answer_md.push_str(&"more ".repeat(400));
    assert_eq!(turn(&app, still, "Allow once"), Some(allow));
    assert_eq!(turn(&app, still, "run 1"), Some(title));

    // Its arguments read to their end: the answers stay where they were.
    let over_title = mouse::Cursor::Available(title.center());
    let _ = turn(&app, (&wheel_by(0.0, 2000.0), over_title), "Allow once");
    assert_eq!(turn(&app, still, "run 1"), None, "scrolled out of view");
    assert_eq!(turn(&app, still, "Allow once"), Some(allow));

    // The next card, when the first is answered, from its own top.
    app.proj.ask_turns[0].extras.approvals.remove(0);
    let next = turn(&app, still, "run 2").expect("the next card from its top");
    assert_eq!(next.position(), title.position());
    assert_eq!(turn(&app, still, "Allow once"), Some(allow));

    // Rows coming and going around the input keep its focus.
    app.proj.ask_pins.push(crate::app::model::AskPin {
        rel: "src/a.rs".into(),
        file: PathBuf::from("/nonexistent/src/a.rs"),
        line: 3,
        code: "fn a() {}".into(),
    });
    let _ = turn(&app, still, "Allow once");
    app.acp_agents = vec![clew_core::acp::AgentConfig {
        name: "fake".into(),
        launch: clew_core::linerpc::Launch::default(),
        share_mcp: false,
    }];
    let _ = turn(&app, still, "Allow once");
    // The ACP session's settings, once it offers some.
    let mut link = crate::app::integrations::AcpLink::new("fake".into(), "Fake".into());
    link.options = vec![clew_core::acp::ConfigOption {
        id: "mode".into(),
        name: "Mode".into(),
        category: Some("mode".into()),
        current: "ask".into(),
        choices: vec![("ask".into(), "Ask".into()), ("code".into(), "Code".into())],
    }];
    app.proj.acp = Some(link);
    app.ask_agent = Some("fake".into());
    assert!(turn(&app, still, "Mode:").is_some(), "its settings show");
    app.proj.acp = None;
    app.ask_agent = None;
    let _ = turn(&app, still, "Allow once");
    app.proj.ask_pins.clear();
    app.acp_agents.clear();
    let _ = turn(&app, still, "Allow once");
    app.proj.ask_turns[0].extras.approvals.clear();
    assert_eq!(turn(&app, still, "run 2"), None);
    app.proj.ask_turns.clear();
    let _ = turn(&app, still, "Allow once");
}

/// The card fitted to its conversation holds the conversation's height
/// while an approval is asked: steps and words arriving over it, and more
/// cards coming after it, move none of its answers — though fitted, the
/// card grows with what it holds.
#[test]
fn a_fitted_cards_approval_stays_put_while_its_turn_grows() {
    use crate::app::model::{AgentStep, Approval, ApprovalKey, ApprovalOption, AskTurn, StepState};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    app.bottom_height = 560.0;
    let card = |call| Approval {
        key: ApprovalKey::Mcp { call },
        mark: None,
        title: format!("run {call}"),
        detail: "x".into(),
        options: vec![ApprovalOption {
            id: "once".into(),
            label: "Allow once".into(),
            allows: true,
            always: false,
        }],
    };
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        answer_md: "so far".into(),
        ..AskTurn::default()
    });
    let mut renderer = renderer();
    let mut cache = Cache::default();
    let still: (&[Event], mouse::Cursor) = (&[], mouse::Cursor::Unavailable);
    let mut turn = |app: &App, label: &str| {
        let (next, found, _) = panel_turn(
            super::panels::ask_panel_fitted,
            app,
            std::mem::take(&mut cache),
            &mut renderer,
            still,
            label,
        );
        cache = next;
        found
    };
    app.proj.ask_turns[0].extras.approvals.push(card(1));
    let _ = turn(&app, "Allow once");
    let allow = turn(&app, "Allow once").expect("its answers show");

    // Steps and words arrive over it; one more card comes after it.
    let grown = &mut app.proj.ask_turns[0];
    for i in 0..30 {
        grown.steps.push(AgentStep {
            tool: "read".into(),
            title: format!("read src/{i}.rs"),
            refs: Vec::new(),
            state: StepState::Done,
        });
    }
    grown.answer_md.push_str(&" more".repeat(300));
    assert_eq!(turn(&app, "Allow once"), Some(allow));
    app.proj.ask_turns[0].extras.approvals.push(card(2));
    assert_eq!(turn(&app, "Allow once"), Some(allow));
    assert!(turn(&app, "1 more waiting after this one").is_some());

    // Answered, the card lets the conversation grow again.
    app.proj.ask_turns[0].extras.approvals.clear();
    assert_eq!(turn(&app, "Allow once"), None);
}

/// A question's Allow counts once its answers have been in view for the
/// delay, where they are now (`ui::placed`): the Ask card moved under them
/// (the window made shorter, its corner up) moves them, and a card too low
/// for them hides them — each waits again. And a hover's peek over the
/// window counts as over the card.
#[test]
fn a_question_moved_or_out_of_view_waits_again() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.ask_view = crate::AskView::Compact;
    shorter(&mut app, 100.0);
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        extras: TurnExtras {
            approvals: vec![Approval {
                key: ApprovalKey::Mcp { call: 1 },
                mark: None,
                title: "run it".into(),
                detail: "a: 1".into(),
                options: vec![ApprovalOption {
                    id: "once".into(),
                    label: "Allow once".into(),
                    allows: true,
                    always: false,
                }],
            }],
            ..TurnExtras::default()
        },
        ..AskTurn::default()
    });
    let card = Question::Approval {
        stream: 9,
        key: ApprovalKey::Mcp { call: 1 },
    };
    let mut renderer = renderer();
    let mut cache = Cache::default();
    // One frame of the window: the placements it reports, fed back as the
    // runtime does.
    let mut frame = |app: &mut App| -> Vec<Option<Rectangle>> {
        let mut ui = UserInterface::build(
            super::view(app),
            Size::new(app.window_width, app.window_height),
            std::mem::take(&mut cache),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        cache = ui.into_cache();
        let mut placed = Vec::new();
        for message in messages {
            if let Message::Placed { question, place } = &message {
                assert_eq!(question, &card);
                placed.push(*place);
                let _ = app.update(message);
            }
        }
        placed
    };
    // The delay gone by, for the question and where it is.
    let read = |app: &mut App| {
        app.asking.since = app.asking.since.checked_sub(delay).unwrap();
        for p in &mut app.asking.places {
            p.since = p.since.checked_sub(delay).unwrap();
        }
    };
    let _ = app.update(Message::Noop);
    let first = frame(&mut app);
    assert!(matches!(first.as_slice(), [Some(_)]), "{first:?}");
    assert!(!app.may_allow(&card), "unread");
    read(&mut app);
    assert!(app.may_allow(&card));
    assert!(frame(&mut app).is_empty(), "where it was: nothing to say");
    assert!(app.may_allow(&card));

    // The window shorter: the Ask card in its corner moves up, and with it
    // the approval card in it.
    shorter(&mut app, 140.0);
    let _ = app.update(Message::Noop);
    let moved = frame(&mut app);
    assert!(
        matches!(moved.as_slice(), [Some(b)] if Some(*b) != first[0]),
        "{moved:?}"
    );
    assert!(!app.may_allow(&card), "moved: unread where it is");
    read(&mut app);
    assert!(app.may_allow(&card));

    // An Ask card too low for the approval card (the window too short for
    // it): not all of it in view, however long.
    shorter(&mut app, SQUEEZING);
    assert_eq!(frame(&mut app), [None]);
    read(&mut app);
    assert!(!app.may_allow(&card), "out of view");
    shorter(&mut app, 340.0);
    assert!(matches!(frame(&mut app).as_slice(), [Some(_)]));
    assert!(!app.may_allow(&card), "back in view: unread");
    read(&mut app);
    assert!(app.may_allow(&card));

    // A hover's peek over the window: not within reach, until it goes.
    app.proj.hover = Some(crate::app::model::HoverState {
        line: 0,
        col: 0,
        x: 10.0,
        y: 10.0,
        text: Some("fn origin()".into()),
        summary: None,
        diagnostic: None,
    });
    let _ = app.update(Message::Noop);
    assert!(!app.may_allow(&card), "under the peek");
    app.proj.hover = None;
    let _ = app.update(Message::Noop);
    assert!(!app.may_allow(&card), "uncovered: unread");
    read(&mut app);
    assert!(app.may_allow(&card));

    // A picker's menu open over the window — opened by a press on it, as
    // the user does — hides any question: none is within reach until it
    // goes, read from what is drawn. Taken away with its menu open (the
    // panel collapsed, the agent gone), it holds nothing back.
    let mut link = crate::app::integrations::AcpLink::new("fake".into(), "Fake".into());
    link.options = vec![clew_core::acp::ConfigOption {
        id: "mode".into(),
        name: "Mode".into(),
        category: Some("mode".into()),
        current: "ask".into(),
        choices: vec![("ask".into(), "Ask".into()), ("code".into(), "Code".into())],
    }];
    app.proj.acp = Some(link);
    app.ask_agent = Some("fake".into());
    let mut renderer = self::renderer();
    let mut cache = Cache::default();
    // One frame of the window, `events` at `cursor`: what it reported, fed
    // back as the runtime does, and where `label` is.
    let mut step = |app: &mut App, events: &[Event], cursor: mouse::Cursor, label: &str| {
        use iced_test::selector::{Candidate, Selector};
        let mut ui = UserInterface::build(
            super::view(app),
            Size::new(app.window_width, app.window_height),
            std::mem::take(&mut cache),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            events,
            cursor,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let wanted = label.to_string();
        let mut find = (move |c: Candidate<'_>| match c {
            Candidate::Text {
                content,
                visible_bounds,
                ..
            } if content == wanted => visible_bounds,
            _ => None,
        })
        .find();
        ui.operate(&renderer, &mut black_box(&mut find));
        let found = match Operation::finish(&find) {
            Outcome::Some(found) => found,
            _ => None,
        };
        cache = ui.into_cache();
        let mut overlay = Vec::new();
        for message in messages {
            match message {
                Message::OverlayOpen(open) => {
                    overlay.push(open);
                    let _ = app.update(message);
                }
                Message::Placed { .. } => {
                    let _ = app.update(message);
                }
                _ => {}
            }
        }
        (found, overlay)
    };
    let nowhere = mouse::Cursor::Unavailable;
    let (label, overlay) = step(&mut app, &redraw(), nowhere, "Mode:");
    assert!(overlay.is_empty(), "no overlay yet: {overlay:?}");
    let label = label.expect("the settings show");
    read(&mut app);
    assert!(app.may_allow(&card));
    // The picker is right of its label.
    let picker = mouse::Cursor::Available(iced::Point::new(
        label.x + label.width + 30.0,
        label.center_y(),
    ));
    let press = [
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
    ];
    let _ = step(&mut app, &press, picker, "Mode:");
    let (_, overlay) = step(&mut app, &redraw(), picker, "Mode:");
    assert_eq!(overlay, [true], "the menu is open");
    assert!(!app.may_allow(&card), "under the menu");
    read(&mut app);
    assert!(!app.may_allow(&card), "under the menu, however long");

    // A modal question coming up over it — which takes every press, so no
    // click could close the menu — closes it: the modal is within reach,
    // and read in its own time.
    let folder = PathBuf::from("/nonexistent/clew-ui-consent");
    let consent = Question::Project(folder.clone());
    app.pending_consent = Some(folder);
    let _ = app.update(Message::Noop);
    let (_, overlay) = step(&mut app, &redraw(), picker, "Mode:");
    let (_, again) = step(&mut app, &redraw(), picker, "Mode:");
    assert_eq!([overlay, again].concat(), [false], "the menu went");
    assert!(!app.may_allow(&consent), "unread");
    read(&mut app);
    assert!(app.may_allow(&consent));
    app.pending_consent = None;
    let _ = app.update(Message::Noop);
    let _ = step(&mut app, &redraw(), picker, "Mode:");
    read(&mut app);
    assert!(app.may_allow(&card));
    let _ = step(&mut app, &press, picker, "Mode:");
    let (_, overlay) = step(&mut app, &redraw(), picker, "Mode:");
    assert_eq!(overlay, [true], "open again");
    // The picker gone with its menu open.
    app.proj.acp = None;
    app.ask_agent = None;
    let (_, overlay) = step(&mut app, &redraw(), picker, "Mode:");
    let (_, again) = step(&mut app, &redraw(), picker, "Mode:");
    assert_eq!([overlay, again].concat(), [false], "the menu went with it");
    assert!(!app.may_allow(&card), "uncovered: unread");
    read(&mut app);
    assert!(app.may_allow(&card));

    // The window moved on the screen, at the same size: where it is in the
    // window says nothing of where it is now. Reported anew, read anew.
    let _ = app.update(Message::Window(crate::WindowMsg::Moved));
    assert!(!app.may_allow(&card), "moved with the window");
    assert!(matches!(frame(&mut app).as_slice(), [Some(_)]));
    assert!(!app.may_allow(&card));
    read(&mut app);
    assert!(app.may_allow(&card));

    // Another card drawn where it was, alike, before the app had the last
    // one's report: it reports for itself.
    let mut renderer = self::renderer();
    let mut cache = Cache::default();
    let mut unheard = |app: &App| -> Vec<Message> {
        let mut ui = UserInterface::build(
            super::view(app),
            Size::new(app.window_width, app.window_height),
            std::mem::take(&mut cache),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        cache = ui.into_cache();
        messages
    };
    app.asking.places.clear();
    assert!(!unheard(&app).is_empty());
    let next = ApprovalKey::Mcp { call: 2 };
    app.proj.ask_turns[0].extras.approvals[0].key = next;
    let sent = unheard(&app);
    assert!(
        sent.iter().any(|m| matches!(m, Message::Placed { question: Question::Approval { key, .. }, place: Some(_) } if *key == next)),
        "{sent:?}"
    );
}

/// "Yes" said to an approval card allows this call, and "no" declines it:
/// never the answer that allows — or declines — every such call from now
/// on, however the agent orders its options; that one is pressed by its
/// own words alone.
#[test]
fn yes_said_to_an_approval_never_allows_always() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption};
    use crate::ui::heard::Answer;
    let option = |id: &str, label: &str, allows: bool, always: bool| ApprovalOption {
        id: id.into(),
        label: label.into(),
        allows,
        always,
    };
    let approval = Approval {
        key: ApprovalKey::Acp { id: 4 },
        mark: None,
        title: "edit main.rs".into(),
        detail: String::new(),
        // An agent that puts the lasting answers first.
        options: vec![
            option("allow_always", "Always allow", true, true),
            option("allow_once", "Allow", true, false),
            option("reject_always", "Always reject", false, true),
            option("reject_once", "Reject", false, false),
        ],
    };
    let targets =
        crate::ui::heard::gather_of(|| super::panels::approval_card(9, &approval, true, 400.0, 0));
    let answered = |answer: Answer| -> Vec<String> {
        targets
            .iter()
            .filter(|t| t.answer == Some(answer))
            .map(|t| match &t.message {
                Message::Ask(AskMsg::Approve { option, .. }) => option.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    assert_eq!(answered(Answer::Yes), ["allow_once"]);
    assert_eq!(answered(Answer::No), ["reject_once"]);
    assert!(
        targets
            .iter()
            .any(|t| t.title == "Always allow" && t.answer.is_none()),
        "the lasting answer is pressed by its words"
    );
}

/// An agent's answers, however many, stay within the card's reach: a few
/// of each kind at a time — a way to decline always among them — the rest
/// a page on, where the card is read anew. And a card coming up in an Ask
/// card too low for it opens the Ask window large, where it fits, once.
#[test]
fn many_answers_or_a_low_panel_leave_the_card_answerable() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    let option = |i: usize, allows: bool| ApprovalOption {
        id: format!("{}{i}", if allows { "yes" } else { "no" }),
        label: format!("{} {i}", if allows { "Allow" } else { "Deny" }),
        allows,
        always: false,
    };
    let page = |options: &[ApprovalOption], page| -> (Vec<String>, usize) {
        let (shown, pages) = super::panels::shown_answers(options, page);
        (shown.into_iter().map(|o| o.id.clone()).collect(), pages)
    };
    let mut options: Vec<ApprovalOption> = (0..8).map(|i| option(i, true)).collect();
    options.extend((0..8).map(|i| option(i, false)));
    assert_eq!(
        page(&options, 0),
        (
            ["yes0", "yes1", "yes2", "no0", "no1", "no2"]
                .map(String::from)
                .to_vec(),
            3
        )
    );
    assert_eq!(
        page(&options, 1).0,
        ["yes3", "yes4", "yes5", "no3", "no4", "no5"]
    );
    assert_eq!(page(&options, 2).0, ["yes6", "yes7", "no6", "no7"]);
    assert_eq!(page(&options, 3), page(&options, 0), "round again");
    // Fewer ways to decline than to allow: one on every page all the same.
    let mut few: Vec<ApprovalOption> = vec![option(0, true), option(0, false)];
    few.extend((1..7).map(|i| option(i, true)));
    assert_eq!(
        page(&few, 0),
        (
            ["yes0", "no0", "yes1", "yes2"].map(String::from).to_vec(),
            3
        )
    );
    assert_eq!(page(&few, 1).0, ["no0", "yes3", "yes4", "yes5"]);
    assert_eq!(page(&few, 2).0, ["no0", "yes6"]);
    // A few, all on one page.
    assert_eq!(
        page(&few[..3], 0),
        (["yes0", "no0", "yes1"].map(String::from).to_vec(), 1)
    );
    let approval = Approval {
        key: ApprovalKey::Mcp { call: 1 },
        mark: None,
        title: "run it".into(),
        detail: String::new(),
        options,
    };
    let mut sim = sim_elem(super::panels::approval_card(9, &approval, true, 200.0, 0));
    assert!(text_at(&mut sim, |t| t == "Allow 3", 0).is_none());
    assert!(text_at(&mut sim, |t| t == "Deny 2", 0).is_some());
    let sim = sim_elem(super::panels::approval_card(9, &approval, true, 200.0, 0));
    assert!(matches!(
        click(sim, "More answers (1 of 3)").as_slice(),
        [Message::Ask(AskMsg::AnswersPage {
            stream: 9,
            page: 1,
            ..
        })]
    ));
    let sim = sim_elem(super::panels::approval_card(9, &approval, true, 200.0, 2));
    assert!(matches!(
        click(sim, "More answers (3 of 3)").as_slice(),
        [Message::Ask(AskMsg::AnswersPage {
            stream: 9,
            page: 0,
            ..
        })]
    ));
    let mut sim = sim_elem(super::panels::approval_card(9, &approval, true, 200.0, 1));
    assert!(text_at(&mut sim, |t| t == "Allow 3", 0).is_some());
    assert!(text_at(&mut sim, |t| t == "Allow 0", 0).is_none());

    // In an Ask card too low for it — a window short for it — drawn, and
    // found squeezed, it opens the Ask window large, where it fits (wider,
    // its answers in fewer rows), and is answered there once read.
    let squeezing = squeezed_in_the_card_only(&approval);
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.ask_view = crate::AskView::Compact;
    shorter(&mut app, squeezing);
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        extras: TurnExtras {
            approvals: vec![approval],
            ..TurnExtras::default()
        },
        ..AskTurn::default()
    });
    let card = |call| Question::Approval {
        stream: 9,
        key: ApprovalKey::Mcp { call },
    };
    let mut renderer = renderer();
    let mut cache = Cache::default();
    let mut frame = |app: &mut App| -> Vec<Option<Rectangle>> {
        let mut ui = UserInterface::build(
            super::view(app),
            Size::new(app.window_width, app.window_height),
            std::mem::take(&mut cache),
            &mut renderer,
        );
        let mut sent = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        cache = ui.into_cache();
        let mut placed = Vec::new();
        for message in sent {
            if let Message::Placed { place, .. } = &message {
                placed.push(*place);
            }
            let _ = app.update(message);
        }
        placed
    };
    let read = |app: &mut App| {
        app.asking.since = app.asking.since.checked_sub(delay).unwrap();
        for p in &mut app.asking.places {
            p.since = p.since.checked_sub(delay).unwrap();
        }
    };
    let _ = app.update(Message::Noop);
    assert_eq!(frame(&mut app), [None], "squeezed in the card");
    assert_eq!(app.ask_view, crate::AskView::Expanded);
    assert!(
        matches!(frame(&mut app).as_slice(), [Some(_)]),
        "in view, large"
    );
    read(&mut app);
    assert!(app.may_allow(&card(1)), "answerable there");
    // Turned a page on: other answers under the pointer, read anew — and
    // there, the page it shows, answerable once read.
    let _ = app.update(Message::Ask(AskMsg::AnswersPage {
        stream: 9,
        key: ApprovalKey::Mcp { call: 1 },
        page: 1,
    }));
    assert!(!app.may_allow(&card(1)), "a page on: unread");
    {
        let mut sim = sim_of(&app);
        assert!(text_at(&mut sim, |t| t == "Allow 3", 0).is_some());
        assert!(text_at(&mut sim, |t| t == "Allow 0", 0).is_none());
    }
    assert!(
        matches!(frame(&mut app).as_slice(), [Some(_)]),
        "placed anew"
    );
    read(&mut app);
    assert!(app.may_allow(&card(1)), "read on its page");

    // Made small again by the user, it stays so.
    let _ = app.update(Message::Ask(AskMsg::Shrink));
    assert_eq!(frame(&mut app), [None]);
    assert_eq!(app.ask_view, crate::AskView::Compact);
    // Another card, squeezed when first drawn: made large for, once — and
    // on its own first page, not the one the last was turned to.
    app.proj.ask_turns[0].extras.approvals[0].key = ApprovalKey::Mcp { call: 2 };
    let _ = app.update(Message::Noop);
    assert_eq!(frame(&mut app), [None]);
    assert_eq!(app.ask_view, crate::AskView::Expanded);
    assert!(text_at(&mut sim_of(&app), |t| t == "Allow 0", 0).is_some());
    // One that fits when first drawn moves nothing — nor does the user's
    // own resize of the window squeezing it after.
    let _ = app.update(Message::Ask(AskMsg::Shrink));
    shorter(&mut app, 120.0);
    app.proj.ask_turns[0].extras.approvals[0].key = ApprovalKey::Mcp { call: 3 };
    let _ = app.update(Message::Noop);
    assert!(matches!(frame(&mut app).as_slice(), [Some(_)]));
    assert_eq!(app.ask_view, crate::AskView::Compact);
    shorter(&mut app, squeezing);
    assert_eq!(frame(&mut app), [None], "squeezed by the user");
    assert_eq!(app.ask_view, crate::AskView::Compact, "and left so");
    // Reported again from nothing (the window moved): still left so.
    let _ = app.update(Message::Window(crate::WindowMsg::Moved));
    assert!(app.asking.places.is_empty());
    assert_eq!(frame(&mut app), [None]);
    assert_eq!(app.ask_view, crate::AskView::Compact);
    let _ = card;
}

/// How much shorter than [`WINDOW`] a window is whose Ask card, in its
/// corner, is too low for an approval card's answers.
const SQUEEZING: f32 = 620.0;

/// The window `by` shorter than [`WINDOW`]: the Ask card in its corner
/// with that much less room.
fn shorter(app: &mut App, by: f32) {
    app.window_height = WINDOW.height - by;
}

/// How much shorter than [`WINDOW`] the window is when the Ask card in it
/// is too low for `approval`'s first page of answers and the large Ask
/// window — wider, its answers in fewer rows — is not.
fn squeezed_in_the_card_only(approval: &crate::app::model::Approval) -> f32 {
    use crate::app::model::{AskTurn, TurnExtras};
    let fits = |by: f32, view| {
        let mut app = reader_app();
        shorter(&mut app, by);
        app.ask_view = view;
        app.proj.ask_turns.push(AskTurn {
            stream: 9,
            question: "why?".into(),
            streaming: true,
            extras: TurnExtras {
                approvals: vec![approval.clone()],
                ..TurnExtras::default()
            },
            ..AskTurn::default()
        });
        let _ = app.update(Message::Noop);
        let mut renderer = renderer();
        let size = Size::new(app.window_width, app.window_height);
        let mut ui = UserInterface::build(super::view(&app), size, Cache::default(), &mut renderer);
        let mut sent = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        sent.iter()
            .any(|m| matches!(m, Message::Placed { place: Some(_), .. }))
    };
    (300..700)
        .step_by(4)
        .map(|by| by as f32)
        .find(|&by| !fits(by, crate::AskView::Compact) && fits(by, crate::AskView::Expanded))
        .expect("precondition: a window the card is too low in and the large window is not")
}

/// A page of answers taller than the first — its labels running over more
/// rows — in an Ask card the first page fitted: drawn, and found squeezed,
/// it opens the Ask window large, as a card come up does.
#[test]
fn a_page_of_answers_too_tall_for_the_panel_opens_it() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, TurnExtras};
    let option = |id: String, label: String, allows: bool| ApprovalOption {
        id,
        label,
        allows,
        always: false,
    };
    let mut options: Vec<ApprovalOption> = (0..3)
        .map(|i| option(format!("yes{i}"), format!("Allow {i}"), true))
        .collect();
    options.extend((3..6).map(|i| {
        let label = format!("Allow {i}, {}", "and so on and so forth ".repeat(8));
        option(format!("yes{i}"), label, true)
    }));
    options.push(option("no".into(), "Deny".into(), false));
    // In a window `height` shorter than the test's: where the card was
    // placed on its first page, then on its second, and the Ask window's
    // size after.
    let at = |height: f32| -> (Option<Rectangle>, Option<Rectangle>, crate::AskView) {
        let mut app = reader_app();
        app.ask_view = crate::AskView::Compact;
        shorter(&mut app, height);
        app.proj.ask_turns.push(AskTurn {
            stream: 9,
            question: "why?".into(),
            streaming: true,
            extras: TurnExtras {
                approvals: vec![Approval {
                    key: ApprovalKey::Mcp { call: 1 },
                    mark: None,
                    title: "run it".into(),
                    detail: String::new(),
                    options: options.clone(),
                }],
                ..TurnExtras::default()
            },
            ..AskTurn::default()
        });
        let _ = app.update(Message::Noop);
        let mut renderer = renderer();
        let mut cache = Cache::default();
        let mut frame = |app: &mut App| -> Option<Option<Rectangle>> {
            let mut ui = UserInterface::build(
                super::view(app),
                Size::new(app.window_width, app.window_height),
                std::mem::take(&mut cache),
                &mut renderer,
            );
            let mut sent = Vec::new();
            let _ = ui.update(
                &redraw(),
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut sent,
            );
            cache = ui.into_cache();
            let mut placed = None;
            for message in sent {
                if let Message::Placed { place, .. } = &message {
                    placed = Some(*place);
                }
                let _ = app.update(message);
            }
            placed
        };
        let first = frame(&mut app).flatten();
        let _ = app.update(Message::Ask(AskMsg::AnswersPage {
            stream: 9,
            key: ApprovalKey::Mcp { call: 1 },
            page: 1,
        }));
        let second = frame(&mut app).expect("placed anew");
        (first, second, app.ask_view)
    };
    let height = (300..680)
        .step_by(5)
        .map(|h| h as f32)
        .find(|&h| {
            let (first, second, _) = at(h);
            first.is_some() && second.is_none()
        })
        .expect("precondition: a card the first page fits and the second does not");
    assert_eq!(at(height).2, crate::AskView::Expanded);
}

/// The redraw that enables an Allow once its delay is over comes once for
/// a stream of pointer moves over its question, when the last is read
/// (`App::wake_after`): not once each.
#[test]
fn a_stream_of_moves_wakes_the_window_once() {
    use crate::app::model::{ApprovalKey, Placement, Question};
    let mut app = reader_app();
    app.asking.delay = std::time::Duration::from_millis(80);
    let card = Question::Approval {
        stream: 9,
        key: ApprovalKey::Mcp { call: 1 },
    };
    app.asking.places.push(Placement {
        question: card.clone(),
        place: Some(Rectangle::new(
            iced::Point::new(0.0, 0.0),
            Size::new(10.0, 10.0),
        )),
        since: std::time::Instant::now(),
    });
    let first = app.update(Message::Armed(card.clone()));
    assert!(app.asking.wake_waiting);
    let due = app.asking.wake_due.unwrap();
    // Moved again: the latest asked, and no second redraw waiting.
    let second = app.update(Message::Armed(card.clone()));
    assert!(crate::app::tests::run_task(second).is_empty(), "one wake");
    assert!(app.asking.wake_due.unwrap() >= due, "the latest asked");
    assert!(app.asking.wake_waiting);
    // As moves went on for a while: what is due comes after the first wake
    // (a task's wait starts when it is run, here when the test runs it).
    let later = std::time::Instant::now() + std::time::Duration::from_millis(250);
    app.asking.wake_due = Some(later);
    assert!(matches!(
        crate::app::tests::run_task(first).as_slice(),
        [Message::QuestionWake]
    ));
    let again = app.update(Message::QuestionWake);
    assert!(
        app.asking.wake_waiting,
        "waited for again, for what is left"
    );
    assert!(matches!(
        crate::app::tests::run_task(again).as_slice(),
        [Message::QuestionWake]
    ));
    let _ = app.update(Message::QuestionWake);
    assert!(std::time::Instant::now() >= later);
    assert!(!app.asking.wake_waiting && app.asking.wake_due.is_none());
}

/// Where nothing tells the window it moved (Wayland), the pointer moving
/// over a question — onto it, or within it — counts its delay anew, as a
/// move would: a window moved under a resting pointer is seen as no more
/// than that.
#[test]
fn where_moves_go_untold_the_pointer_moving_over_it_counts_anew() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.arm_on_enter = true;
    app.ask_view = crate::AskView::Compact;
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        extras: TurnExtras {
            approvals: vec![Approval {
                key: ApprovalKey::Mcp { call: 1 },
                mark: None,
                title: "run it".into(),
                detail: String::new(),
                options: vec![ApprovalOption {
                    id: "once".into(),
                    label: "Allow once".into(),
                    allows: true,
                    always: false,
                }],
            }],
            ..TurnExtras::default()
        },
        ..AskTurn::default()
    });
    let card = Question::Approval {
        stream: 9,
        key: ApprovalKey::Mcp { call: 1 },
    };
    let _ = app.update(Message::Noop);
    let mut renderer = renderer();
    let mut cache = Cache::default();
    // One frame: the mouse moved to `cursor`, what was reported fed back.
    fn moved_to(
        app: &mut App,
        cache: &mut Cache,
        renderer: &mut iced::Renderer,
        cursor: mouse::Cursor,
    ) -> Vec<Message> {
        let mut ui =
            UserInterface::build(super::view(app), WINDOW, std::mem::take(cache), renderer);
        let mut sent = Vec::new();
        let moved = [Event::Mouse(mouse::Event::CursorMoved {
            position: cursor.position().unwrap_or_default(),
        })];
        let _ = ui.update(&moved, cursor, renderer, &mut clipboard::Null, &mut sent);
        *cache = ui.into_cache();
        for message in sent.clone() {
            let _ = app.update(message);
        }
        sent
    }
    macro_rules! frame {
        ($app:expr, $cursor:expr) => {
            moved_to($app, &mut cache, &mut renderer, $cursor)
        };
    }
    let read = |app: &mut App| {
        app.asking.since = app.asking.since.checked_sub(delay).unwrap();
        for p in &mut app.asking.places {
            p.since = p.since.checked_sub(delay).unwrap();
        }
    };
    let away = mouse::Cursor::Available(iced::Point::new(5.0, 5.0));
    let _ = frame!(&mut app, away);
    let place = app
        .asking
        .places
        .iter()
        .find(|p| p.question == card)
        .and_then(|p| p.place)
        .expect("the card is placed");
    read(&mut app);
    assert!(app.may_allow(&card));
    // The pointer still where it was: nothing to say.
    assert!(
        !frame!(&mut app, away)
            .iter()
            .any(|m| matches!(m, Message::Armed(_)))
    );
    assert!(app.may_allow(&card));
    // Onto it — moved there, or the window moved under it.
    let title = mouse::Cursor::Available(iced::Point::new(place.x + 20.0, place.y + 10.0));
    let sent = frame!(&mut app, title);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Armed(q) if *q == card)),
        "{sent:?}"
    );
    assert!(!app.may_allow(&card), "come onto it: unread");
    // Resting on it: no more.
    read(&mut app);
    assert!(
        !frame!(&mut app, title)
            .iter()
            .any(|m| matches!(m, Message::Armed(_)))
    );
    assert!(app.may_allow(&card));
    // Moved within it — onto the Allow from its title, as a window moved
    // under the pointer resting there does: counted anew too.
    let allow = mouse::Cursor::Available(iced::Point::new(
        place.x + 30.0,
        place.y + place.height - 12.0,
    ));
    let sent = frame!(&mut app, allow);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Armed(q) if *q == card)),
        "{sent:?}"
    );
    assert!(!app.may_allow(&card), "moved within it: unread");
    read(&mut app);
    assert!(app.may_allow(&card));

    // The user's own presses: a finger's tap on the Allow answers, a click
    // on it answers however its pointer wobbles or is dragged (and the next
    // waits); a hold moved onto it, a press and release around a move onto
    // it in one frame, a click on it during a hold of another button begun
    // elsewhere answer nothing first and the next waits — and a hold gone
    // out of the window and back; a drag of the window by the toolbar
    // leaves the arming as it was.
    use iced_test::selector::{Candidate, Selector};
    let mut ui = UserInterface::build(
        super::view(&app),
        WINDOW,
        std::mem::take(&mut cache),
        &mut renderer,
    );
    let mut find = (|c: Candidate<'_>| match c {
        Candidate::Text {
            content: "Allow once",
            visible_bounds,
            ..
        } => visible_bounds,
        _ => None,
    })
    .find();
    ui.operate(&renderer, &mut black_box(&mut find));
    let on_allow = match Operation::finish(&find) {
        Outcome::Some(Some(at)) => at.center(),
        other => panic!("the Allow is not on screen: {other:?}"),
    };
    let on_title = iced::Point::new(place.x + 20.0, place.y + 10.0);
    let mut batch = |events: &[Event], at: iced::Point| -> Vec<Message> {
        let mut sent = Vec::new();
        let _ = ui.update(
            events,
            mouse::Cursor::Available(at),
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        sent
    };
    let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
    let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
    let moved = |to: iced::Point| Event::Mouse(mouse::Event::CursorMoved { position: to });
    let answered = |sent: &[Message]| {
        sent.iter().position(
            |m| matches!(m, Message::Ask(AskMsg::Approve { option, .. }) if option == "once"),
        )
    };
    let armed = |sent: &[Message]| sent.iter().position(|m| matches!(m, Message::Armed(_)));
    // A finger's tap, the pointer resting on the title: a touch takes the
    // pointer where it lands, and tells no move.
    let _ = batch(&[moved(on_title)], on_title);
    let finger = iced::touch::Finger(1);
    let sent = batch(
        &[
            Event::Touch(iced::touch::Event::FingerPressed {
                id: finger,
                position: on_allow,
            }),
            Event::Touch(iced::touch::Event::FingerLifted {
                id: finger,
                position: on_allow,
            }),
        ],
        on_allow,
    );
    assert!(
        answered(&sent).is_some() && armed(&sent).is_none(),
        "{sent:?}"
    );
    // A mouse click after the tap, the mouse not moved: iced's one cursor
    // is where the finger was, so the click is taken there — where the
    // pointer was not seen to go, counted anew first.
    let sent = batch(&[press.clone(), release.clone()], on_allow);
    assert!(
        matches!(
            (armed(&sent), answered(&sent)),
            (Some(r), a) if a.is_none_or(|a| r < a)
        ),
        "{sent:?}"
    );
    // A click, the pointer resting on the Allow, wobbling on it between
    // press and release.
    let wobble = iced::Point::new(on_allow.x + 2.0, on_allow.y);
    let _ = batch(&[moved(on_allow)], on_allow);
    let sent = batch(std::slice::from_ref(&press), on_allow);
    assert!(armed(&sent).is_none(), "pressed where it rests: {sent:?}");
    let sent = batch(&[moved(wobble), release.clone()], wobble);
    assert!(
        matches!((answered(&sent), armed(&sent)), (Some(a), Some(r)) if a < r),
        "the click answers, the next waits: {sent:?}"
    );
    // Pressed on the Allow, once it counted, and dragged along it — or
    // the window moved under the press — before it is let go of there: the
    // press was the user's own, on it, however far it went; the next waits.
    let dragged = iced::Point::new(on_allow.x + 12.0, on_allow.y);
    let _ = batch(&[moved(on_allow)], on_allow);
    let _ = batch(std::slice::from_ref(&press), on_allow);
    let sent = batch(&[moved(dragged), release.clone()], dragged);
    assert!(
        matches!((answered(&sent), armed(&sent)), (Some(a), Some(r)) if a < r),
        "{sent:?}"
    );
    // A press, a move onto the Allow and a release seen in one frame (the
    // window moved under the press while a frame was late), the pointer
    // where it ended for all of them: where it was pressed counts first.
    let _ = batch(&[moved(on_title)], on_title);
    let sent = batch(&[press.clone(), moved(on_allow), release.clone()], on_allow);
    assert!(
        matches!((armed(&sent), answered(&sent)), (Some(r), Some(a)) if r < a),
        "{sent:?}"
    );
    // Held on the title, moved onto the Allow — the window moving under
    // it alike — and let go: no answer, and the next waits.
    let _ = batch(&[moved(on_title)], on_title);
    let _ = batch(std::slice::from_ref(&press), on_title);
    let sent = batch(&[moved(on_allow)], on_allow);
    assert!(armed(&sent).is_none(), "while held: {sent:?}");
    let sent = batch(std::slice::from_ref(&release), on_allow);
    assert!(
        answered(&sent).is_none() && armed(&sent).is_some(),
        "{sent:?}"
    );
    // Held from the title by another button, and clicked on the Allow
    // there: the press, told where it is, is said first, so the click
    // waits.
    let right = |pressed: bool| {
        Event::Mouse(if pressed {
            mouse::Event::ButtonPressed(mouse::Button::Right)
        } else {
            mouse::Event::ButtonReleased(mouse::Button::Right)
        })
    };
    let _ = batch(&[moved(on_title)], on_title);
    let _ = batch(&[right(true)], on_title);
    let _ = batch(&[moved(on_allow)], on_allow);
    let sent = batch(&[press.clone(), release.clone()], on_allow);
    assert!(
        matches!(
            (armed(&sent), answered(&sent)),
            (Some(r), a) if a.is_none_or(|a| r < a)
        ),
        "{sent:?}"
    );
    let _ = batch(&[right(false)], on_allow);
    // Held on the Allow, the pointer gone from the window, and back onto
    // it — by a move alone, or entering first: counted anew, before the
    // release the Allow still holds for can answer.
    for back in [
        vec![moved(on_allow)],
        vec![Event::Mouse(mouse::Event::CursorEntered), moved(on_allow)],
    ] {
        let _ = batch(&[moved(on_allow)], on_allow);
        let _ = batch(std::slice::from_ref(&press), on_allow);
        let _ = batch(&[Event::Mouse(mouse::Event::CursorLeft)], on_allow);
        let sent = batch(&back, on_allow);
        assert!(armed(&sent).is_some(), "{back:?}: {sent:?}");
        let sent = batch(std::slice::from_ref(&release), on_allow);
        assert!(answered(&sent).is_some(), "the Allow held it: {sent:?}");
    }
    // The window dragged by its toolbar: the press elsewhere, the pointer
    // gone, its release never seen — and back over the card, arming.
    let toolbar = iced::Point::new(400.0, 12.0);
    let _ = batch(&[moved(toolbar)], toolbar);
    let _ = batch(std::slice::from_ref(&press), toolbar);
    let _ = batch(&[Event::Mouse(mouse::Event::CursorLeft)], toolbar);
    let sent = batch(
        &[Event::Mouse(mouse::Event::CursorEntered), moved(on_title)],
        on_title,
    );
    assert!(armed(&sent).is_some(), "arming after the drag: {sent:?}");
    let sent = batch(&[moved(on_allow)], on_allow);
    assert!(armed(&sent).is_some(), "{sent:?}");
    cache = ui.into_cache();

    // Where a move is told (macOS), the pointer moving over it is no news.
    app.arm_on_enter = false;
    let elsewhere = mouse::Cursor::Available(iced::Point::new(place.x + 60.0, place.y + 15.0));
    assert!(
        !frame!(&mut app, elsewhere)
            .iter()
            .any(|m| matches!(m, Message::Armed(_)))
    );
    assert!(app.may_allow(&card));
}

/// Where moves go untold (Wayland), a card that comes up under a pointer
/// resting where it is — the first of a turn, or the next one drawn where
/// the last was answered, its first event a redraw or a scroll — answers
/// its first click once its delay has run: the pointer resting there is
/// not a move onto it. The pointer moving over it after still is.
#[test]
fn a_card_come_up_under_a_resting_pointer_answers_its_first_click() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    use iced_test::selector::{Candidate, Selector};
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.arm_on_enter = true;
    app.ask_view = crate::AskView::Compact;
    let approval = |call: u64| Approval {
        key: ApprovalKey::Mcp { call },
        mark: None,
        title: "run it".into(),
        detail: String::new(),
        options: vec![ApprovalOption {
            id: "once".into(),
            label: "Allow once".into(),
            allows: true,
            always: false,
        }],
    };
    app.proj.ask_turns.push(AskTurn {
        stream: 9,
        question: "why?".into(),
        streaming: true,
        extras: TurnExtras {
            approvals: vec![approval(1)],
            ..TurnExtras::default()
        },
        ..AskTurn::default()
    });
    let _ = app.update(Message::Noop);
    let mut renderer = renderer();
    let on_allow = {
        let mut ui =
            UserInterface::build(super::view(&app), WINDOW, Cache::default(), &mut renderer);
        let mut find = (|c: Candidate<'_>| match c {
            Candidate::Text {
                content: "Allow once",
                visible_bounds,
                ..
            } => visible_bounds,
            _ => None,
        })
        .find();
        ui.operate(&renderer, &mut black_box(&mut find));
        match Operation::finish(&find) {
            Outcome::Some(Some(at)) => at.center(),
            other => panic!("the Allow is not on screen: {other:?}"),
        }
    };
    let mut cache = Cache::default();
    // One frame, the pointer at `at`, what was sent fed back.
    let mut frame_at = |app: &mut App, events: &[Event], at: Point| -> Vec<Message> {
        let mut ui = UserInterface::build(
            super::view(app),
            WINDOW,
            std::mem::take(&mut cache),
            &mut renderer,
        );
        let mut sent = Vec::new();
        let _ = ui.update(
            events,
            mouse::Cursor::Available(at),
            &mut renderer,
            &mut clipboard::Null,
            &mut sent,
        );
        cache = ui.into_cache();
        for message in sent.clone() {
            let _ = app.update(message);
        }
        sent
    };
    let read = |app: &mut App| {
        app.asking.since = app.asking.since.checked_sub(delay).unwrap();
        for p in &mut app.asking.places {
            p.since = p.since.checked_sub(delay).unwrap();
        }
    };
    let press = [Event::Mouse(mouse::Event::ButtonPressed(
        mouse::Button::Left,
    ))];
    let release = [Event::Mouse(mouse::Event::ButtonReleased(
        mouse::Button::Left,
    ))];
    let armed = |sent: &[Message]| sent.iter().any(|m| matches!(m, Message::Armed(_)));
    let scroll = [Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
    })];
    // Its first event a redraw (the first of a turn, and the next drawn
    // where it was answered), or a scroll that came first.
    for (call, first) in [(1, redraw()), (2, redraw()), (3, scroll)] {
        if call > 1 {
            // Answered, the slot empty a while (the tool running), and the
            // next drawn where it was.
            let _ = frame_at(&mut app, &redraw(), on_allow);
            let turn = app.proj.ask_turns.last_mut().unwrap();
            turn.extras.approvals.push(approval(call));
            let _ = app.update(Message::Noop);
        }
        let card = Question::Approval {
            stream: 9,
            key: ApprovalKey::Mcp { call },
        };
        let _ = frame_at(&mut app, &first, on_allow);
        let _ = frame_at(&mut app, &redraw(), on_allow);
        read(&mut app);
        assert!(app.may_allow(&card), "card {call}: read");
        let sent = frame_at(&mut app, &press, on_allow);
        assert!(!armed(&sent), "card {call}: {sent:?}");
        let sent = frame_at(&mut app, &release, on_allow);
        assert!(
            app.first_approval().is_none(),
            "card {call}: its first click answers: {sent:?}"
        );
    }
    // Come up so, it is still counted anew once the pointer moves over it
    // — the window moved under it.
    let _ = frame_at(&mut app, &redraw(), on_allow);
    let turn = app.proj.ask_turns.last_mut().unwrap();
    turn.extras.approvals.push(approval(4));
    let _ = app.update(Message::Noop);
    let card = Question::Approval {
        stream: 9,
        key: ApprovalKey::Mcp { call: 4 },
    };
    let _ = frame_at(&mut app, &redraw(), on_allow);
    let _ = frame_at(&mut app, &redraw(), on_allow);
    read(&mut app);
    assert!(app.may_allow(&card));
    let shifted = Point::new(on_allow.x + 3.0, on_allow.y);
    let sent = frame_at(
        &mut app,
        &[Event::Mouse(mouse::Event::CursorMoved {
            position: shifted,
        })],
        shifted,
    );
    assert!(armed(&sent), "{sent:?}");
    assert!(!app.may_allow(&card), "moved over: unread");
}

/// A press made on an Allow that counts, held while it stopped counting
/// (its question moved, say) and let go of once it counts again, answers
/// nothing: it is drawn anew as it comes to count (`fresh::allow`). Not
/// let go of in between, the same press answers; and a decline pressed
/// before the Allow counts and let go of after still declines. So for
/// every question's view.
#[test]
fn a_press_held_while_its_allow_stops_counting_answers_nothing() {
    use crate::LspMsg;
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption};
    use iced_test::selector::{Candidate, Selector};
    // `view`, its Allow counting or not, pressed and let go of on the
    // answers labelled `allow` and `decline`.
    /// An answer's label, and what it sends.
    type Answer<'s> = (&'s str, fn(&Message) -> bool);
    fn check<'a>(
        view: impl Fn(bool) -> Element<'a, Message>,
        (allow, allows): Answer<'_>,
        decline: Option<Answer<'_>>,
    ) {
        let mut renderer = renderer();
        let mut cache = Cache::default();
        // One frame, `events` on the answer labelled `label`.
        let mut frame = |ready: bool, label: &str, events: &[Event]| -> Vec<Message> {
            let mut ui = UserInterface::build(
                view(ready),
                WINDOW,
                std::mem::take(&mut cache),
                &mut renderer,
            );
            let mut find = (|c: Candidate<'_>| match c {
                Candidate::Text {
                    content,
                    visible_bounds,
                    ..
                } if content == label => visible_bounds,
                _ => None,
            })
            .find();
            ui.operate(&renderer, &mut black_box(&mut find));
            let at = match Operation::finish(&find) {
                Outcome::Some(Some(at)) => at.center(),
                other => panic!("{label} is not on screen: {other:?}"),
            };
            let mut sent = Vec::new();
            let _ = ui.update(
                events,
                mouse::Cursor::Available(at),
                &mut renderer,
                &mut clipboard::Null,
                &mut sent,
            );
            cache = ui.into_cache();
            sent
        };
        let press = [Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))];
        let release = [Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))];
        let _ = frame(true, allow, &press);
        let _ = frame(false, allow, &[]);
        let sent = frame(true, allow, &release);
        assert!(!sent.iter().any(allows), "{allow}: {sent:?}");
        let _ = frame(true, allow, &press);
        let sent = frame(true, allow, &release);
        assert!(sent.iter().any(allows), "{allow}: {sent:?}");
        if let Some((decline, declines)) = decline {
            let _ = frame(false, decline, &press);
            let sent = frame(true, decline, &release);
            assert!(sent.iter().any(declines), "{decline}: {sent:?}");
        }
    }

    let approval = Approval {
        key: ApprovalKey::Acp { id: 4 },
        mark: None,
        title: "run it".into(),
        detail: String::new(),
        options: vec![
            ApprovalOption {
                id: "once".into(),
                label: "Allow once".into(),
                allows: true,
                always: false,
            },
            ApprovalOption {
                id: "no".into(),
                label: "Deny".into(),
                allows: false,
                always: false,
            },
        ],
    };
    check(
        |ready| super::panels::approval_card(9, &approval, ready, 200.0, 0),
        (
            "Allow once",
            |m| matches!(m, Message::Ask(AskMsg::Approve { option, .. }) if option == "once"),
        ),
        Some((
            "Deny",
            |m| matches!(m, Message::Ask(AskMsg::Approve { option, .. }) if option == "no"),
        )),
    );
    let consent = crate::LspConsent {
        on_path: None,
        language: "rust".into(),
        server_name: "rust-analyzer".into(),
        version: "1".into(),
        provision: crate::LspProvision::Remote {
            describe: "install it".into(),
            consent: "digest".into(),
        },
        dest_dir: PathBuf::new(),
    };
    check(
        |ready| super::lsp_consent_modal(&consent, 7, 0, ready),
        ("Download", |m| {
            matches!(m, Message::Lsp(LspMsg::ConsentAllowed(7)))
        }),
        Some(("Not now", |m| {
            matches!(m, Message::Lsp(LspMsg::ConsentDismissed(7)))
        })),
    );
    let root = PathBuf::from("/nonexistent/clew-ui-allow");
    let pending = crate::PendingLspCommand {
        root: root.clone(),
        host: None,
        language: "rust".into(),
        command: None,
        args: Vec::new(),
        server_name: "rust-analyzer".into(),
        version: "x".into(),
        fingerprint: "fp".into(),
        init_options: None,
    };
    check(
        |ready| super::lsp_command_modal(&pending, 8, 0, ready),
        ("Allow", |m| {
            matches!(m, Message::Lsp(LspMsg::CommandAllowed(8)))
        }),
        Some(("Don't allow", |m| {
            matches!(m, Message::Lsp(LspMsg::CommandDismissed(8)))
        })),
    );
    // A first open's Open and Not now both open the project — Not now with
    // nothing set up — so both are held until the sheet can have been
    // read; not opening it is the ×, or Escape.
    check(
        |ready| open_sheet(&root, ready),
        ("Open", |m| {
            matches!(m, Message::Project(ProjectMsg::ConsentAllowed(_)))
        }),
        None,
    );
    check(
        |ready| open_sheet(&root, ready),
        ("Open without setup", |m| {
            matches!(m, Message::Project(ProjectMsg::OpenWithoutSetup(_)))
        }),
        None,
    );
    let target = port_2222_target();
    let key = scanned_key();
    check(
        |ready| super::connect::trust_host_prompt(&target, &key, ready),
        ("Trust and connect", |m| {
            matches!(m, Message::Connect(ConnectMsg::TrustHost { .. }))
        }),
        Some(("Cancel", |m| {
            matches!(m, Message::Connect(ConnectMsg::TrustCancel))
        })),
    );
    check(
        |ready| super::connect::host_key_changed_prompt(&target, "changed", "host", ready),
        ("Forget the old key", |m| {
            matches!(m, Message::Connect(ConnectMsg::ForgetHostKey { .. }))
        }),
        Some(("Cancel", |m| {
            matches!(m, Message::Connect(ConnectMsg::TrustCancel))
        })),
    );
}

/// Every question is drawn fresh for what it asks (`ui::fresh`): a press
/// held on one's button is not the next one's, wherever it comes up. Its
/// Allow is drawn fresh again as it comes to count or stops
/// (`fresh::allow`).
#[test]
fn every_question_is_drawn_fresh_for_what_it_asks() {
    use super::fresh::keys_in;
    let hash = |what: &dyn Fn(&mut std::collections::hash_map::DefaultHasher)| {
        use std::hash::Hasher;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        what(&mut hasher);
        hasher.finish()
    };
    use std::hash::Hash;
    let consent = crate::LspConsent {
        on_path: None,
        language: "rust".into(),
        server_name: "rust-analyzer".into(),
        version: "1".into(),
        provision: crate::LspProvision::Remote {
            describe: "install it".into(),
            consent: "digest".into(),
        },
        dest_dir: PathBuf::new(),
    };
    assert_eq!(
        keys_in(&super::lsp_consent_modal(&consent, 7, 0, true)),
        [7, 1]
    );
    let root = PathBuf::from("/nonexistent/clew-ui-fresh");
    let pending = crate::PendingLspCommand {
        root: root.clone(),
        host: None,
        language: "rust".into(),
        command: None,
        args: Vec::new(),
        server_name: "rust-analyzer".into(),
        version: "x".into(),
        fingerprint: "fp".into(),
        init_options: None,
    };
    assert_eq!(
        keys_in(&super::lsp_command_modal(&pending, 8, 0, true)),
        [8, 1]
    );
    // A first open's Not now opens the project too: held as Open is.
    assert_eq!(
        keys_in(&open_sheet(&root, true)),
        [hash(&|h| root.hash(h)), 1, 1]
    );
    let target = port_2222_target();
    let key = scanned_key();
    assert_eq!(
        keys_in(&super::connect::trust_host_prompt(&target, &key, true)),
        [hash(&|h| key.fingerprint.hash(h)), 1]
    );
    assert_eq!(
        keys_in(&super::connect::host_key_changed_prompt(
            &target, "changed", "host", true
        )),
        [hash(&|h| "host".hash(h)), 1]
    );
    let approval = crate::app::model::Approval {
        key: crate::app::model::ApprovalKey::Acp { id: 4 },
        mark: None,
        title: "run it".into(),
        detail: String::new(),
        options: vec![
            crate::app::model::ApprovalOption {
                id: "ok".into(),
                label: "Allow".into(),
                allows: true,
                always: false,
            },
            crate::app::model::ApprovalOption {
                id: "no".into(),
                label: "Deny".into(),
                allows: false,
                always: false,
            },
        ],
    };
    assert_eq!(
        keys_in(&super::panels::approval_card(9, &approval, true, 200.0, 0)),
        [hash(&|h| (9u64, approval.key).hash(h)), 0, 1]
    );
    // Its Allow anew too as it comes to count, its Deny as it is.
    assert_eq!(
        keys_in(&super::panels::approval_card(9, &approval, false, 200.0, 0)),
        [hash(&|h| (9u64, approval.key).hash(h)), 0, 0]
    );
    // And its answers anew for each page of them.
    let mut paged = approval.clone();
    paged
        .options
        .extend((1..4).map(|i| crate::app::model::ApprovalOption {
            id: format!("ok{i}"),
            label: format!("Allow {i}"),
            allows: true,
            always: false,
        }));
    assert_eq!(
        keys_in(&super::panels::approval_card(9, &paged, true, 200.0, 1)),
        [hash(&|h| (9u64, paged.key).hash(h)), 1, 1]
    );

    // And each reports where its answers are, all of them in view in a
    // window of the usual size (`ui::placed`) — or its Allow never counts.
    use crate::app::model::Question;
    let asked = |question: Question| crate::ui::placed::Asked {
        ready: false,
        report: Some((question, None)),
        arm: false,
    };
    let placed = |view: Element<'_, Message>| -> Vec<(Question, Option<Rectangle>)> {
        let mut renderer = renderer();
        let mut ui = UserInterface::build(view, WINDOW, Cache::default(), &mut renderer);
        let mut messages = Vec::new();
        let _ = ui.update(
            &redraw(),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
            .into_iter()
            .filter_map(|m| match m {
                Message::Placed { question, place } => Some((question, place)),
                _ => None,
            })
            .collect()
    };
    // Its own report, of all of it in view.
    let in_view = |places: Vec<(Question, Option<Rectangle>)>, what: Question| {
        assert!(
            matches!(places.as_slice(), [(question, Some(_))] if *question == what),
            "{what:?}: {places:?}"
        );
    };
    in_view(
        placed(super::lsp_consent_modal(
            &consent,
            7,
            0,
            asked(Question::LspConsent(7)),
        )),
        Question::LspConsent(7),
    );
    in_view(
        placed(super::lsp_command_modal(
            &pending,
            8,
            0,
            asked(Question::LspCommand(8)),
        )),
        Question::LspCommand(8),
    );
    in_view(
        placed(open_sheet(&root, asked(Question::Project(root.clone())))),
        Question::Project(root.clone()),
    );
    in_view(
        placed(super::connect::trust_host_prompt(
            &target,
            &key,
            asked(Question::TrustHost(key.fingerprint.clone())),
        )),
        Question::TrustHost(key.fingerprint.clone()),
    );
    // And in the Connect modal, as the window draws them.
    let mut app = reader_app();
    app.connect = Some(crate::ConnectUi {
        stage: crate::ConnectStage::TrustHost {
            target: target.clone(),
            reason: "refused".into(),
            key: key.clone(),
        },
        ..Default::default()
    });
    in_view(
        placed(super::view(&app)),
        Question::TrustHost(key.fingerprint.clone()),
    );
    app.connect = Some(crate::ConnectUi {
        stage: crate::ConnectStage::HostKeyChanged {
            target: target.clone(),
            reason: "changed".into(),
            forget_host: "host".into(),
        },
        ..Default::default()
    });
    in_view(
        placed(super::view(&app)),
        Question::ForgetHostKey("host".into()),
    );
    in_view(
        placed(super::connect::host_key_changed_prompt(
            &target,
            "changed",
            "host",
            asked(Question::ForgetHostKey("host".into())),
        )),
        Question::ForgetHostKey("host".into()),
    );
    in_view(
        placed(super::panels::approval_card(
            9,
            &approval,
            asked(Question::Approval {
                stream: 9,
                key: approval.key,
            }),
            200.0,
            0,
        )),
        Question::Approval {
            stream: 9,
            key: approval.key,
        },
    );
}

/// A14/A16: a file git tracks although `.gitignore` would hide it is listed
/// in the tree — and marked as such; other files are not.
#[test]
fn a_tracked_but_ignored_file_is_marked_in_the_tree() {
    let mut app = blank_app();
    let root = PathBuf::from("/nonexistent/clew-ui-tracked");
    app.proj.project = Some(crate::Project {
        root: root.clone(),
        tree: crate::fs_scan::DirNode {
            dirs: Vec::new(),
            files: vec!["payload.rs".into(), "plain.rs".into()],
        },
        files: Arc::new(Vec::new()),
        truncated: false,
    });
    app.proj.tracked_ignored.insert("payload.rs".into());
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "payload.rs"));
    let marks = {
        let mark = super::TRACKED_IGNORED_MARK.to_string();
        let mut n = 0;
        while text_at(&mut sim, |t| t == mark, n).is_some() {
            n += 1;
        }
        n
    };
    assert_eq!(marks, 1, "exactly the tracked-but-ignored file is marked");
}

/// A folder listing the server cut at its cap says what it left out where
/// the list ends — it was only ever said once, in the status line.
#[test]
fn a_capped_folder_listing_says_what_it_left_out() {
    let browser = |omitted| crate::RemoteBrowser {
        cwd: "/big".into(),
        parent: Some("/".into()),
        entries: vec![clew_protocol::DirEntry {
            name: "src".into(),
            is_dir: true,
        }],
        omitted,
        loading: false,
    };
    let capped = browser(41);
    let mut sim = sim_elem(super::remote_browser_view(&capped));
    assert!(shows(&mut sim, "41 more"));
    let complete = browser(0);
    let mut sim = sim_elem(super::remote_browser_view(&complete));
    assert!(shows(&mut sim, "src"));
    assert!(!shows(&mut sim, "more"));
    assert_eq!(super::omitted_note(1).as_deref(), Some("… and 1 more"));
}

/// I1: the Connect modal shows whether the LIVE host holds the AI keys, and
/// its Revoke withdraws them — the form's checkbox never did.
#[test]
fn the_connect_modal_can_revoke_the_live_hosts_ai_keys() {
    let mut app = reader_app();
    app.connection = port_2222_target();
    app.remote_ai_opt_in = true;
    app.settings.key = "sk-test".into();
    app.connect = Some(crate::ConnectUi::default());
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "holds your API keys"));
    let sent = click(sim, "Revoke");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Connect(ConnectMsg::RevokeAiKeys)]
        ),
        "{sent:?}"
    );
    for msg in sent {
        let _ = app.update(msg);
    }
    assert_eq!(app.live_ai_opt_in(), Some(false));
    assert!(shows(&mut sim_of(&app), "does not hold your API keys"));
    // With no key here, there is nothing for it to hold, or not.
    app.settings.key.clear();
    app.llm_available = false;
    assert!(!shows(&mut sim_of(&app), "does not hold your API keys"));
    // A key configured — in config.toml or the environment — with Settings
    // never opened (its draft empty) is one it does not hold.
    app.llm_available = true;
    assert!(shows(&mut sim_of(&app), "does not hold your API keys"));
    // Locally there is nothing to grant, so nothing is shown.
    app.connection = crate::connect::ConnTarget::Local;
    let mut local = sim_of(&app);
    assert!(!shows(&mut local, "AI API keys —"));
}

fn paused_debug_session() -> crate::DebugSession {
    crate::DebugSession {
        client: None,
        status: crate::DebugStatus::Stopped,
        exit_code: None,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: vec![("a".into(), "1".into()), ("b".into(), "2".into())],
        output: Vec::new(),
        current: None,
        program: PathBuf::from("/p/prog"),
        args: Vec::new(),
        cwd: PathBuf::from("/p"),
        addr: None,
    }
}

/// I1: the Debug panel's watch rows remove by identity, and hover evaluation
/// is a checkbox only where the adapter makes no side-effect promise.
#[test]
fn the_debug_panel_removes_watches_by_identity_and_offers_hover_evaluation() {
    let mut app = reader_app();
    app.debug.session = Some(paused_debug_session());
    app.debug.watches = vec!["a".into(), "b".into()];
    let sent = click_left_of(
        || sim_elem(super::debug_panel(&app)),
        "b",
        0,
        |m| matches!(m, Message::Debug(DebugMsg::WatchRemoveExpr(_))),
    );
    assert!(
        matches!(sent.as_slice(), [Message::Debug(DebugMsg::WatchRemoveExpr(e))] if e == "b"),
        "{sent:?}"
    );

    app.debug.hover_safe = false;
    let sent = click(sim_elem(super::debug_panel(&app)), "Evaluate on hover");
    assert!(
        matches!(sent.as_slice(), [Message::Debug(DebugMsg::ToggleHoverEval)]),
        "{sent:?}"
    );
    app.debug.hover_safe = true;
    let mut safe = sim_elem(super::debug_panel(&app));
    assert!(safe.find("hover shows values").is_err(), "no caption");
    assert!(
        safe.find("Evaluate on hover").is_err(),
        "no switch is needed"
    );
}

fn call_item(name: &str) -> crate::lsp::client::CallItem {
    crate::lsp::client::CallItem {
        name: name.into(),
        detail: String::new(),
        kind: 12,
        path: PathBuf::from("/p/x.rs"),
        line: 0,
        character: 0,
        raw: serde_json::json!({}),
    }
}

/// The empty call-hierarchy view says how to fill it: the tab alone gave no
/// way in, the menu item that does lives on the code's right-click menu.
#[test]
fn the_empty_call_hierarchy_says_how_to_fill_it() {
    let app = reader_app();
    assert!(app.proj.call_graph.is_none());
    let mut sim = sim_elem(super::calls_tab(&app));
    assert!(shows(&mut sim, "choose Call Hierarchy"));
    // Value flow says how to fill it too.
    assert!(app.proj.flow.is_none());
    let mut sim = sim_elem(super::panels::flow_tab(&app));
    assert!(shows(&mut sim, "choose Trace Value"));
}

/// A language server's list in the Search sidebar is a list of its own:
/// headed by what it lists, with a way back to the search, and not under
/// the search's own empty fields — "(references)" was the only word of it.
#[test]
fn a_reference_list_is_named_in_the_search_sidebar() {
    let mut app = reader_app();
    app.proj.search.ran = true;
    app.proj.search.listing = Some("References to “origin”".into());
    app.proj.search.hits = vec![crate::search::SearchHit {
        abs: PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"),
        rel: "src/lib.rs".into(),
        line: 3,
        preview: "pub fn origin()".into(),
    }];
    let view = || sim_elem(super::sidebar::search_tab(&app));
    assert!(shows(&mut view(), "References to “origin”"));
    assert!(shows(&mut view(), "1 match"));
    assert!(
        text_at(&mut view(), |t| t.contains("Search in project"), 0).is_none(),
        "the search's empty field under the list"
    );
    let sent = click_right_of(view, "References to “origin”", |m| {
        matches!(m, Message::Nav(NavMsg::ListingClosed))
    });
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Nav(NavMsg::ListingClosed))),
        "{sent:?}"
    );
    for m in sent {
        let _ = app.update(m);
    }
    assert!(app.proj.search.listing.is_none());
    assert!(app.proj.search.hits.is_empty());
}

/// One letter in the search field is searched only on Enter, and the
/// sidebar says so: it stayed blank, as if nothing matched.
#[test]
fn a_one_letter_search_says_it_waits_for_enter() {
    let mut app = reader_app();
    app.proj.search.query = "f".into();
    assert!(shows(
        &mut sim_elem(super::sidebar::search_tab(&app)),
        "Keep typing to search"
    ));
    app.proj.search.query = "fn".into();
    assert!(!shows(
        &mut sim_elem(super::sidebar::search_tab(&app)),
        "Keep typing to search"
    ));
}

/// A function with no callers (or callees) says so under its name: the
/// panel showed the name and nothing else, as if it were still loading.
#[test]
fn a_call_tree_with_nothing_under_its_root_says_so() {
    let mut app = reader_app();
    for (direction, said) in [
        (crate::callgraph::Direction::Incoming, "No callers found"),
        (crate::callgraph::Direction::Outgoing, "No calls found"),
    ] {
        let mut tree =
            crate::callgraph::CallTree::new(7, direction, "python", vec![call_item("root")]);
        tree.set_children(0, Vec::new());
        app.proj.call_graph = Some(tree);
        assert!(shows(&mut sim_elem(super::calls_tab(&app)), said), "{said}");
    }
    // Not before it has been asked.
    let tree = crate::callgraph::CallTree::new(
        8,
        crate::callgraph::Direction::Incoming,
        "python",
        vec![call_item("root")],
    );
    app.proj.call_graph = Some(tree);
    assert!(!shows(
        &mut sim_elem(super::calls_tab(&app)),
        "No callers found"
    ));
}

/// I1 / E1-11: a call-tree arrow names its tree (token) as well as the node,
/// and a node the size cap cut short says how many callers it holds back.
#[test]
fn the_call_tree_names_its_tree_and_shows_what_the_cap_hid() {
    let mut app = reader_app();
    let mut tree = crate::callgraph::CallTree::new(
        7,
        crate::callgraph::Direction::Incoming,
        "rust",
        vec![call_item("root")],
    );
    let many: Vec<_> = (0..crate::callgraph::MAX_NODES + 5)
        .map(|i| call_item(&format!("caller_{i}")))
        .collect();
    // The arena holds MAX_NODES: the root plus 799 callers, 6 held back.
    tree.set_children(0, many);
    app.proj.call_graph = Some(tree);
    let mut sim = sim_elem(super::calls_tab(&app));
    assert!(
        shows(&mut sim, "6 more callers not shown (tree limit 800)"),
        "the cap's note is not under its node"
    );
    drop(sim);
    // (The header names the root too: the row is the second "root".)
    let sent = click_left_of(
        || sim_elem(super::calls_tab(&app)),
        "root",
        1,
        |m| matches!(m, Message::Calls(CallsMsg::ExpandNode { .. })),
    );
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Calls(CallsMsg::ExpandNode { token: 7, id: 0 })]
        ),
        "{sent:?}"
    );
}

/// I1 / E1-11: what the symbol index left out shows where the trees built on
/// it render — the import tree and the symbol finder — and only for the
/// project it was built for.
#[test]
fn the_index_cap_is_shown_in_the_import_tree_and_the_symbol_finder() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.import_tree = Some(crate::imports::ImportTree::new(
        &crate::imports::ImportGraph::default(),
        &root,
        root.join("src/lib.rs"),
        crate::imports::Dir::Imports,
    ));
    app.proj.index_cap_note = Some("1 file over 4 MB not indexed".into());
    app.proj.index_too_large = vec![("vendor/big.c".into(), 9_200_000)];
    let mut tab = sim_elem(super::imports_tab(&app));
    assert!(shows(&mut tab, "1 file over 4 MB not indexed"));
    // Which file, and how large, once the note is opened.
    assert!(!shows(&mut tab, "vendor/big.c"));
    let _ = tab.click("1 file over 4 MB not indexed");
    let sent: Vec<Message> = tab.into_messages().collect();
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Graph(GraphMsg::ToggleImportNote(
                crate::app::message::ImportNote::LeftOut
            ))]
        ),
        "{sent:?}"
    );
    for message in sent {
        let _ = app.update(message);
    }
    let mut tab = sim_elem(super::imports_tab(&app));
    assert!(shows(&mut tab, "vendor/big.c"));
    assert!(shows(&mut tab, "8.8 MB"));
    drop(tab);
    app.proj.finder.open = true;
    app.proj.finder.mode = crate::finder::FinderMode::Symbols;
    assert!(shows(
        &mut sim_of(&app),
        "Index incomplete: 1 file over 4 MB not indexed"
    ));
    // A note belongs to its project's session, and leaves with it.
    app.forget_project_state();
    assert!(app.proj.index_cap_note.is_none());
    assert!(!shows(&mut sim_of(&app), "Index incomplete"));
}

/// The Imports tab's cycle note: of the cycle the tree's own file is in,
/// and only that one (a cycle elsewhere in the project is the import
/// graph's), said quietly — a cycle is no error in most languages — as how
/// many files it shares it with; opened, it names them, each opening its
/// file when pressed.
#[test]
fn the_files_import_cycle_opens_to_its_files_and_opens_each() {
    use crate::app::message::ImportNote;
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let lib = root.join("src/lib.rs");
    app.proj.import_tree = Some(crate::imports::ImportTree::new(
        &crate::imports::ImportGraph::default(),
        &root,
        lib.clone(),
        crate::imports::Dir::Imports,
    ));
    let elsewhere = vec![root.join("src/a.rs"), root.join("src/b.rs")];
    app.proj.import_cycles = vec![elsewhere.clone()];
    assert!(
        !shows(&mut sim_elem(super::imports_tab(&app)), "import cycle"),
        "a cycle lib.rs is not in is no note of lib.rs's"
    );
    let with_lib = vec![root.join("src/c.rs"), lib.clone(), root.join("src/d.rs")];
    app.proj.import_cycles = vec![elsewhere, with_lib];
    let mut tab = sim_elem(super::imports_tab(&app));
    assert!(shows(&mut tab, "In an import cycle with 2 other files"));
    assert!(!shows(&mut tab, "c.rs"), "shut, the note names no file");
    let _ = tab.click("In an import cycle with 2 other files");
    for message in tab.into_messages().collect::<Vec<_>>() {
        let _ = app.update(message);
    }
    assert_eq!(app.proj.import_notes_open, [ImportNote::Cycles]);

    let mut tab = sim_elem(super::imports_tab(&app));
    assert!(shows(&mut tab, "c.rs") && shows(&mut tab, "d.rs"));
    assert!(!shows(&mut tab, "a.rs"), "only lib.rs's cycle is listed");
    let _ = tab.click("d.rs");
    let sent: Vec<Message> = tab.into_messages().collect();
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Editor(EditorMsg::OpenAbs { abs, line: None, push: true })]
                if *abs == root.join("src/d.rs")
        ),
        "{sent:?}"
    );

    // Pressed again, the note shuts.
    let _ = app.update(Message::Graph(GraphMsg::ToggleImportNote(
        ImportNote::Cycles,
    )));
    assert!(app.proj.import_notes_open.is_empty());
    assert!(!shows(&mut sim_elem(super::imports_tab(&app)), "c.rs"));
}

/// I3 / E1-6: an equation or diagram the renderer could not draw shows its
/// source (with a note), one still rendering shows its source too — never a
/// "rendering…" that can outlive the render — and a failure is remembered so
/// the renderer is not run on it again this session.
#[test]
fn a_failed_render_shows_its_source_not_a_placeholder() {
    use crate::{PreparedInline, PreparedSeg};
    let mut app = reader_app();
    let segs = vec![
        PreparedSeg::DisplayMath(1, r"\frac{1}{".into()),
        PreparedSeg::Mermaid(2, "graph ?? broken".into()),
        PreparedSeg::InlineLine(vec![
            PreparedInline::Text(crate::richmd::InlinePiece::parse("see ")),
            PreparedInline::Math(3, r"\alpha".into()),
        ]),
    ];
    let failed = crate::app::tasks::SvgBatch {
        rendered: Default::default(),
        failed: vec![
            crate::app::tasks::SvgFailure {
                key: 1,
                reason: "could not parse this equation".into(),
            },
            crate::app::tasks::SvgFailure {
                key: 2,
                reason: "could not parse this diagram".into(),
            },
        ],
    };
    let generation = app.proj.explain.svg_gen;
    let _ = app.update(Message::Content(ContentMsg::SvgsGenerated {
        stamp: app.stamp(),
        generation,
        map: failed,
    }));
    assert!(
        app.proj.explain.svg_failed.contains_key(&1)
            && app.proj.explain.svg_failed.contains_key(&2)
    );
    assert!(
        app.proj.explain.svgs.is_empty(),
        "no placeholder SVG stands in for a failure"
    );

    {
        let column = iced::widget::Column::with_children(super::render_prepared(&app, &segs));
        let mut sim = sim_elem(column.into());
        assert!(sim.find(r"\frac{1}{").is_ok(), "the failed equation's TeX");
        assert!(
            sim.find("graph ?? broken").is_ok(),
            "the failed diagram's source"
        );
        assert!(sim.find(r"\alpha").is_ok(), "the pending inline math's TeX");
        assert!(
            sim.find("⚠ Could not render this equation. Its source:")
                .is_ok()
        );
        assert!(
            sim.find("⚠ Could not render this diagram. Its source:")
                .is_ok()
        );
        assert!(!shows(&mut sim, "rendering"), "a placeholder is on screen");
    }

    // A failure is not handed to the renderer again this session (the render
    // pass runs for an open project, which `reader_app` has).
    let before = app.proj.explain.svg_gen;
    let key = crate::richmd::math_key(r"\frac{1}{", true);
    app.proj
        .explain
        .svg_failed
        .insert(key, "could not parse".into());
    let (_, _task) = app.prepare_segments("$$\\frac{1}{$$");
    assert_eq!(
        app.proj.explain.svg_gen, before,
        "a failed source was queued again"
    );
}

/// A live language-server client over an in-memory stub that answers
/// `initialize`, then sends each of `notifications` (JSON-RPC bodies), then
/// listens (and says nothing more) until the test ends.
async fn stub_lsp_client_saying(
    root: &std::path::Path,
    notifications: Vec<String>,
) -> crate::lsp::client::LspClient {
    let (client_stdin, mut peer_rx) = tokio::io::duplex(64 * 1024);
    let (mut peer_tx, client_stdout) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut buf = [0u8; 4096];
        let _ = peer_rx.read(&mut buf).await;
        let frame = |body: &str| format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let answer = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let _ = peer_tx.write_all(frame(answer).as_bytes()).await;
        for body in notifications {
            let _ = peer_tx.write_all(frame(&body).as_bytes()).await;
        }
        // Drain whatever else the client says, holding the link open.
        while matches!(peer_rx.read(&mut buf).await, Ok(n) if n > 0) {}
        drop(peer_tx);
    });
    crate::lsp::client::LspClient::connect(client_stdin, client_stdout, root, None)
        .await
        .expect("stub handshake")
}

/// I5 / I4: every update takes ONE snapshot per ready server, and the view
/// reads the status bar, the underlines and the server panel from it. A
/// poisoned state shows as the problem it is — not as a healthy, quiet
/// server with no diagnostics.
#[tokio::test]
async fn the_view_reads_lsp_state_from_one_snapshot_and_reports_poisoning() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    // The stub reports an error on line 2 of the open file, so what a
    // poisoned snapshot must NOT show is shown by a healthy one.
    let diagnostic = format!(
        r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{}","diagnostics":[{{"range":{{"start":{{"line":1,"character":3}},"end":{{"line":1,"character":9}}}},"severity":1,"message":"boom"}}]}}}}"#,
        crate::lsp::client::path_to_uri(&root.join("src/lib.rs"))
    );
    let client = stub_lsp_client_saying(&root, vec![diagnostic]).await;
    for _ in 0..200 {
        if client.snapshot().is_ok_and(|s| s.diag_version > 0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    app.proj
        .link
        .lsp
        .insert("rust".into(), crate::LspSlot::Ready(client));
    assert!(app.proj.link.lsp_snapshots.is_empty());
    let _ = app.update(Message::Noop);
    assert!(
        matches!(app.proj.link.lsp_snapshots.get("rust"), Some(Ok(_))),
        "the update took no snapshot"
    );
    // The server at work is named.
    assert!(shows(&mut sim_of(&app), "rust-analyzer ready"));
    let underlined = |app: &App| {
        app.code_highlights(0, app.proj.panes[0].as_ref().unwrap())
            .iter()
            .any(|h| matches!(h.kind, crate::codeview::HlKind::DiagError) && h.line == 1)
    };
    assert!(
        underlined(&app),
        "the healthy snapshot's error is not underlined"
    );

    app.proj
        .link
        .lsp_snapshots
        .insert("rust".into(), Err(crate::lsp::client::StatePoisoned));
    assert!(shows(&mut sim_of(&app), "rust-analyzer ⚠ internal error"));
    assert!(
        !underlined(&app),
        "nothing is read past a poisoned snapshot"
    );
    // Settings' Languages page, where the servers are. (Opening it is an
    // update, which reads the live server again: poisoned once more.)
    app.proj.project_languages = vec!["rust".into()];
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();
    let _ = app.update(Message::Lsp(crate::LspMsg::TogglePanel));
    assert!(app.server_panel);
    app.proj
        .link
        .lsp_snapshots
        .insert("rust".into(), Err(crate::lsp::client::StatePoisoned));
    assert!(
        text_at(
            &mut sim_of(&app),
            |t| t.contains("internal error — restart this language server"),
            0
        )
        .is_some(),
        "the server's row says its state is unreadable"
    );
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));

    // A server that goes away takes its snapshot with it.
    app.proj.link.lsp.clear();
    let _ = app.update(Message::Noop);
    assert!(app.proj.link.lsp_snapshots.is_empty());
}

fn explain_file(root: &std::path::Path, rel: &str) -> crate::explain::Node {
    crate::explain::Node::File(root.join(rel))
}

/// I5: the Explain tab's CONTAINS list is memoized per cache generation — a
/// change to the cache (through `explain_cache_mut`, which bumps it) shows up
/// at the next view.
#[test]
fn the_explain_children_follow_the_cache_generation() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let cached = |s: &str| crate::explain::Cached {
        summary: s.into(),
        prompt_hash: crate::incremental::content_hash(s.as_bytes()),
        detail: None,
        basis: None,
    };
    app.explain_cache_mut()
        .insert(explain_file(&root, "b.rs"), cached("Second file."));
    app.proj.explain.view = Some(crate::explain::Node::Folder(root.clone()));
    let mut sim = sim_elem(super::explain_content(&app));
    assert!(sim.find("b.rs").is_ok());
    assert!(sim.find("a.rs").is_err());
    drop(sim);
    app.explain_cache_mut()
        .insert(explain_file(&root, "a.rs"), cached("First file."));
    let mut sim = sim_elem(super::explain_content(&app));
    assert!(sim.find("a.rs").is_ok(), "the new child is missing");
    assert!(
        shows(&mut sim, "First file"),
        "its summary is not beside it"
    );
    assert_eq!(
        super::explain_children(
            &app.proj.explain.cache,
            app.proj.explain.view.as_ref().unwrap()
        )
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>(),
        ["a.rs", "b.rs"]
    );
}

/// A summary kept from an earlier clew unchecked is marked wherever it is
/// shown — the file banner, a folder's CONTAINS and the sidebar's search
/// results here — as it is in the explanation overlay. They showed it as if
/// it were current.
#[test]
fn an_unchecked_summary_is_marked_where_it_is_shown() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let unchecked = crate::explain::Cached {
        summary: "Kept from before. More.".into(),
        prompt_hash: 1,
        detail: None,
        basis: Some(crate::explain::Basis {
            inputs: 2,
            source: None,
            unchecked: true,
            recipe: crate::explain::BASIS_RECIPE,
        }),
    };
    app.show_file_banner = true;
    app.explain_cache_mut()
        .insert(explain_file(&root, "src/lib.rs"), unchecked);
    let marked = format!("{} Kept from before", crate::app::UNCHECKED_MARK);
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, &marked), "the file banner");
    drop(sim);
    app.proj.explain.view = Some(crate::explain::Node::Folder(root.join("src")));
    let mut sim = sim_elem(super::explain_content(&app));
    assert!(shows(&mut sim, &marked), "CONTAINS");
    drop(sim);
    app.proj.semantic_results = vec![(explain_file(&root, "src/lib.rs"), 0.9)];
    let mut sim = sim_elem(super::semantic_tab(&app));
    assert!(shows(&mut sim, &marked), "the search results");
}

/// A click on the file banner shows the file's explanation in the margin, from
/// whichever of its pages the margin was on. Open on Calls (or History),
/// the margin stayed there: the note was chosen to be shown on Notes, the
/// banner stayed above the code, and the click seemed to do nothing.
#[test]
fn the_file_banner_shows_the_note_from_any_page_of_the_margin() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.explain_cache_mut().insert(
        explain_file(&root, "src/lib.rs"),
        crate::explain::Cached {
            summary: "Reads every line. More.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    app.show_file_banner = true;
    app.show_right_panel = true;
    app.margin_tab = crate::MarginTab::Calls;
    // The banner says the note's first sentence.
    let sent = click(sim_of(&app), "Reads every line");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Explain(crate::ExplainMsg::Show(..)))),
        "{sent:?}"
    );
    for message in sent {
        let _ = app.update(message);
    }
    assert_eq!(app.margin_tab, crate::MarginTab::Notes);
    assert_eq!(
        app.proj.explain.view,
        Some(explain_file(&root, "src/lib.rs"))
    );
    // The margin says it now, and the banner no longer says it twice.
    let mut sim = sim_of(&app);
    assert!(
        text_at(&mut sim, |t| t == "Reads every line", 0).is_none(),
        "the banner stayed"
    );
}

/// The same mark in the outline's one-line summaries and in the call flow's
/// lists: shown there without it, an unchecked summary read as current, and
/// no test said so.
#[test]
fn an_unchecked_summary_is_marked_in_the_outline_and_the_call_flow() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let lib = root.join("src/lib.rs");
    let node = |name: &str| crate::explain::Node::Function {
        file: lib.clone(),
        name: name.into(),
        ordinal: 0,
    };
    let unchecked = crate::explain::Cached {
        summary: "Kept from before. More.".into(),
        prompt_hash: 1,
        detail: None,
        basis: Some(crate::explain::Basis {
            inputs: 2,
            source: None,
            unchecked: true,
            recipe: crate::explain::BASIS_RECIPE,
        }),
    };
    app.explain_cache_mut()
        .insert(node("line_0"), unchecked.clone());
    app.explain_cache_mut().insert(node("line_1"), unchecked);
    let marked = format!("{} Kept from before", crate::app::UNCHECKED_MARK);

    app.show_inline_summaries = true;
    let symbol = |name: &str, line: usize| clew_protocol::Symbol {
        name: name.into(),
        kind: "function".into(),
        line,
        end_line: line,
    };
    app.proj.panes[0].as_mut().unwrap().symbols = vec![symbol("line_0", 1)];
    let mut sim = sim_elem(super::outline_content(&app));
    assert!(shows(&mut sim, &marked), "the outline");
    drop(sim);

    // `line_1` calls `line_0`.
    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 1,
            callers,
            callees,
        };
    app.proj.project_calls.graph = std::sync::Arc::new(
        crate::projectcalls::ProjectCallGraph::from_wire(
            clew_protocol::CallGraph {
                nodes: vec![
                    call("line_0", vec![1], vec![]),
                    call("line_1", vec![], vec![0]),
                ],
            },
            |rel| root.join(rel),
        )
        .expect("a call graph"),
    );
    let rows = super::call_flow_rows(&app, &node("line_1"));
    let mut sim = sim_elem(iced::widget::Column::with_children(rows).into());
    assert!(shows(&mut sim, &marked), "CALLS");
}

/// A feature is called the same wherever it is named: a sidebar view and
/// the palette's command for it, a shortcut's name and its menu title, a ⋯
/// row and the view or page it opens. And the reader's own notes and the
/// call hierarchy are not called what the margin's tabs are.
#[test]
fn a_feature_is_called_the_same_wherever_it_is_named() {
    use crate::SidebarTab as T;
    use crate::keymap::Action;
    let same = |a: &str, b: &str| {
        let bare = |s: &str| s.trim_end_matches('…').to_lowercase();
        bare(a) == bare(b)
    };
    let app = reader_app();
    let commands = app.palette_commands();
    for tab in [
        T::Files,
        T::Search,
        T::Semantic,
        T::Marks,
        T::Trail,
        T::Calls,
        T::Flow,
        T::Imports,
        T::Walk,
        T::Notes,
        T::Docs,
    ] {
        let title = format!("Sidebar: {}", super::sidebar::tab_name(tab));
        assert!(
            commands.iter().any(|c| same(&c.title, &title)),
            "no palette command is called {title:?}"
        );
    }
    for action in [
        Action::ProjectSearch,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ZoomReset,
        Action::Walkthrough,
    ] {
        assert!(
            same(action.label(), action.menu().title),
            "{action:?}: {:?} vs {:?}",
            action.label(),
            action.menu().title
        );
    }
    let tab = super::sidebar::tab_name;
    assert!(same(Action::ProjectSearch.label(), tab(T::Search)));
    assert!(same(Action::Walkthrough.label(), tab(T::Walk)));
    for margin in ["Explanations", "Calls"] {
        for t in [T::Notes, T::Calls] {
            assert!(
                !same(tab(t), margin),
                "{t:?} is called the margin's {margin}"
            );
        }
    }
}

/// An icon stands for one thing: what has one name has one icon wherever
/// it is (the strip, the ⋯ menu, the ☰ menu, the sidebar), and what is
/// another thing has another icon — the Overview, the glossary and the API
/// docs were all books, Open Folder and Open Remote each had two. A file's
/// view of the imports or the calls and the project's graph of them are one
/// thing, seen at two sizes.
#[test]
fn an_icon_stands_for_one_thing() {
    use super::toolbar::{
        STRIP_TOP, StripTool, TOOLS_MENU_ROWS, app_menu_head, strip_tool, tools_row,
    };
    use crate::SidebarTab as T;
    let app = reader_app();
    let mut named: Vec<(String, crate::glyph::Glyph)> = Vec::new();
    for tool in STRIP_TOP.into_iter().chain([StripTool::Settings]) {
        let (glyph, name, ..) = strip_tool(&app, tool);
        named.push((name.to_string(), glyph));
    }
    for row in TOOLS_MENU_ROWS {
        let (glyph, name, _) = tools_row(&app, row);
        named.push((name, glyph));
    }
    for row in super::toolbar::RUN_MENU_ROWS {
        let (glyph, name, _) = super::toolbar::run_row(&app, row);
        named.push((name.to_string(), glyph));
    }
    for (glyph, name, ..) in app_menu_head(&app) {
        named.push((name.to_string(), glyph));
    }
    for tab in [
        T::Files,
        T::Search,
        T::Semantic,
        T::Marks,
        T::Trail,
        T::Calls,
        T::Flow,
        T::Imports,
        T::Walk,
        T::Notes,
        T::Docs,
    ] {
        named.push((
            super::sidebar::tab_name(tab).to_string(),
            super::sidebar::tab_glyph(tab),
        ));
    }
    let bare = |s: &str| s.trim_end_matches('…').to_lowercase();
    let one_thing = [
        ("imports", "import graph"),
        ("call hierarchy", "call graph"),
        // The bug opens the Run menu, whose Debug is the bug's own.
        ("run and debug", "debug"),
    ];
    for (i, (a, glyph_a)) in named.iter().enumerate() {
        for (b, glyph_b) in &named[i + 1..] {
            let (a, b) = (bare(a), bare(b));
            if a == b
                || one_thing
                    .iter()
                    .any(|&(x, y)| (a == x && b == y) || (a == y && b == x))
            {
                assert_eq!(glyph_a, glyph_b, "{a:?} has two icons");
            } else {
                assert_ne!(glyph_a, glyph_b, "{a:?} and {b:?} share an icon");
            }
        }
    }
}

/// The map's header says its views in words, the one in effect lit and
/// pressing nothing: shapes for 2D and the map read as ◇ and ▽.
#[test]
fn the_maps_header_says_its_views_in_words() {
    let mut app = reader_app();
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    let header = |app: &App| {
        super::graph::graph_modal_frame(
            "Call Graph",
            true,
            app.graph_3d,
            false,
            false,
            None,
            None,
            None,
            WINDOW,
            iced::widget::space().into(),
        )
    };
    let mut sim = sim_elem(header(&app));
    for said in ["2D", "3D", "Graph", "List"] {
        assert!(shows(&mut sim, said), "{said}");
    }
    drop(sim);
    app.graph_3d = false;
    assert!(click(sim_elem(header(&app)), "2D").is_empty(), "in effect");
    assert!(matches!(
        click(sim_elem(header(&app)), "3D").as_slice(),
        [Message::Graph(crate::GraphMsg::Toggle3D)]
    ));
    assert!(
        click(sim_elem(header(&app)), "Graph").is_empty(),
        "in effect"
    );
    assert!(matches!(
        click(sim_elem(header(&app)), "List").as_slice(),
        [Message::Graph(crate::GraphMsg::OverlayViewToggle)]
    ));
}

/// The margin's History lists the file's commits (subject, then who and
/// when), each one opening Time Travel at it; and it says so while they
/// come, and when there are none — not links that do what the sheet's
/// header buttons already do.
#[test]
fn the_margin_history_lists_the_files_commits() {
    let mut app = reader_app();
    let abs = app.active_viewer().unwrap().abs.clone();
    app.show_right_panel = true;
    app.margin_tab = crate::MarginTab::History;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Reading the file's history"));
    assert!(shows(&mut sim, "Why is this line here?"));
    assert!(!shows(&mut sim, "Changes since the last commit"));
    assert!(!shows(&mut sim, "This file through time"));
    drop(sim);

    app.proj.margin_history = Some((abs.clone(), Some(Ok(vec![]))));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "No commit has touched this file yet."));
    drop(sim);

    let commit = |sha: &str, subject: &str| crate::git::HistCommit {
        sha: sha.into(),
        author: "Ada".into(),
        time: 0,
        subject: subject.into(),
        path: "src/lib.rs".into(),
    };
    app.proj.margin_history = Some((
        abs.clone(),
        Some(Ok(vec![
            commit("bbbbbbb2222", "Read lines lazily"),
            commit("aaaaaaa1111", "First reader"),
        ])),
    ));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Read lines lazily"));
    assert!(
        shows(&mut sim, "aaaaaaa by Ada"),
        "its short sha and author"
    );
    drop(sim);
    let sent = click(sim_of(&app), "First reader");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::TimeTravel(crate::TimeTravelMsg::StartAt { sha }) if sha == "aaaaaaa1111"
        )),
        "{sent:?}"
    );
    // Its commits are a list of their own, as voice counts them: "the
    // second one" is the second commit, whatever list was drawn before.
    let targets = crate::ui::heard::gather(&app);
    let place = |subject: &str| {
        targets
            .iter()
            .find(|t| t.title.contains(subject) && t.row.is_some())
            .and_then(|t| t.row)
            .unwrap_or_else(|| panic!("{subject} is no row"))
    };
    let (newest, oldest) = (place("Read lines lazily"), place("First reader"));
    assert_eq!((newest.1, oldest.1), (1, 2));
    assert_eq!(newest.0, oldest.0, "one list");
    assert!(
        targets
            .iter()
            .filter(|t| t.row.is_some_and(|(list, _)| list == newest.0))
            .count()
            == 2,
        "the list holds only the commits"
    );

    // Another file's commits are not this one's.
    app.proj.margin_history = Some((
        abs.with_file_name("other.rs"),
        Some(Ok(vec![commit("ccccccc3333", "Elsewhere")])),
    ));
    let mut sim = sim_of(&app);
    assert!(!shows(&mut sim, "Elsewhere"));
    assert!(shows(&mut sim, "Reading the file's history"));
}

/// A function no commit has yet — every line of it added since HEAD — has
/// no history of its own: the margin says so over the file's commits, which
/// are not its, and offers no "This function through time", which only
/// answered that git had none.
#[test]
fn the_margin_history_says_a_new_function_is_not_committed() {
    let mut app = reader_app();
    let abs = app.active_viewer().unwrap().abs.clone();
    app.show_right_panel = true;
    app.margin_tab = crate::MarginTab::History;
    {
        let v = app.proj.panes[0].as_mut().unwrap();
        v.symbols = vec![crate::outline::Symbol {
            name: "helper".into(),
            kind: "function".into(),
            line: 3,
            end_line: 5,
        }];
        v.caret = Some((3, 0));
    }
    app.proj.explain.view = Some(crate::explain::Node::Function {
        file: abs.clone(),
        name: "helper".into(),
        ordinal: 0,
    });
    app.proj.margin_history = Some((abs.clone(), Some(Ok(vec![]))));
    let git = |kind: clew_protocol::ChangeKind| {
        let mut status = vec![None; 400];
        for line in status.iter_mut().take(5).skip(2) {
            *line = Some(kind);
        }
        Some(Arc::new(clew_protocol::GitInfo {
            blame: Vec::new(),
            status,
            deleted_at: Default::default(),
        }))
    };

    // Committed and since changed: its history is there to read.
    app.proj.panes[0].as_mut().unwrap().git = git(clew_protocol::ChangeKind::Modified);
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "This function through time"));
    assert!(!shows(&mut sim, "is not in any commit yet"));
    drop(sim);

    // All of it new.
    app.proj.panes[0].as_mut().unwrap().git = git(clew_protocol::ChangeKind::Added);
    let mut sim = sim_of(&app);
    assert!(shows(
        &mut sim,
        "“helper” is not in any commit yet. The file's commits:"
    ));
    assert!(!shows(&mut sim, "This function through time"));
    assert!(shows(&mut sim, "Why is this line here?"));
}

/// A call row in a narrow margin puts its place under the name instead of
/// running it off the edge, where name and place read as one clipped word.
#[test]
fn a_call_row_wraps_its_place_in_a_narrow_margin() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let lib = root.join("src/lib.rs");
    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 12,
            callers,
            callees,
        };
    app.proj.project_calls.graph = std::sync::Arc::new(
        crate::projectcalls::ProjectCallGraph::from_wire(
            clew_protocol::CallGraph {
                nodes: vec![
                    call("work", vec![1], vec![]),
                    call("read_lines_lazily", vec![], vec![0]),
                ],
            },
            |rel| root.join(rel),
        )
        .expect("a call graph"),
    );
    let node = crate::explain::Node::Function {
        file: lib,
        name: "work".into(),
        ordinal: 0,
    };
    const NARROW: f32 = 160.0;
    let mut sim = sim_elem(
        container(iced::widget::Column::with_children(super::call_flow_rows(
            &app, &node,
        )))
        .width(NARROW)
        .into(),
    );
    let name = text_at(&mut sim, |t| t == "read_lines_lazily", 0).expect("the caller");
    let place = text_at(&mut sim, |t| t == "src/lib.rs", 0).expect("its place");
    assert!(
        place.x + place.width <= NARROW + 0.5,
        "inside the margin: {place:?}"
    );
    assert!(place.y > name.y, "under the name: {name:?} {place:?}");
}

/// The REACHED FROM section: the chain from an entry point down to the
/// explained function, entry first, each step a button; an entry point says
/// so instead; a function no entry reaches says that; and a project without
/// any entry point shows no section at all.
#[test]
fn the_call_flow_shows_how_an_entry_point_reaches_the_function() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let lib = root.join("src/lib.rs");
    let node = |name: &str| crate::explain::Node::Function {
        file: lib.clone(),
        name: name.into(),
        ordinal: 0,
    };
    // main → run → work; test_work → work, tested; alone is called by
    // nobody.
    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 1,
            callers,
            callees,
        };
    app.proj.project_calls.graph = std::sync::Arc::new(
        crate::projectcalls::ProjectCallGraph::from_wire(
            clew_protocol::CallGraph {
                nodes: vec![
                    call("main", vec![], vec![1]),
                    call("run", vec![0], vec![2]),
                    call("work", vec![1, 3], vec![]),
                    call("test_work", vec![], vec![2, 5]),
                    call("alone", vec![], vec![]),
                    call("tested", vec![3], vec![]),
                ],
            },
            |rel| root.join(rel),
        )
        .expect("a call graph"),
    );
    // No entry point in the index: no section.
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("work"))).into(),
    );
    assert!(
        !shows(&mut sim, "REACHED FROM"),
        "a library has no chains to show"
    );
    drop(sim);

    let symbol =
        |name: &str, line: usize, entry: Option<crate::index::EntryKind>, is_test: bool| {
            crate::index::SymbolEntry {
                name: name.into(),
                kind: "function".into(),
                rel: "src/lib.rs".into(),
                abs: lib.clone(),
                line,
                is_test,
                entry,
            }
        };
    app.proj.symbol_index_by_file.insert(
        lib.clone(),
        std::sync::Arc::new(vec![
            symbol("main", 1, Some(crate::index::EntryKind::Main), false),
            symbol("run", 2, None, false),
            symbol("work", 3, None, false),
            symbol("test_work", 4, None, true),
            symbol("alone", 5, None, false),
            symbol("tested", 6, None, false),
        ]),
    );
    app.proj.symbol_index_rev += 1;
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("work"))).into(),
    );
    assert!(shows(&mut sim, "REACHED FROM"));
    assert!(shows(&mut sim, "main"), "the entry heads the chain");
    assert!(shows(&mut sim, "run"), "the step between");
    assert!(shows(&mut sim, "test_work"), "the test that calls it");
    assert!(
        text_at(&mut sim, |t| t.contains("test_work"), 1).is_none(),
        "a test that calls it itself is said once, under TESTS: not again \
         as a one-step chain"
    );
    drop(sim);
    // Reached by a test alone: TESTS says so, and there is no chain to
    // show — nor a claim that nothing reaches it.
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("tested"))).into(),
    );
    assert!(shows(&mut sim, "test_work"));
    assert!(!shows(&mut sim, "REACHED FROM"));
    assert!(!shows(&mut sim, "No entry point reaches this"));
    drop(sim);
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("main"))).into(),
    );
    assert!(shows(&mut sim, "Entry point: main"));
    drop(sim);
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("alone"))).into(),
    );
    assert!(shows(&mut sim, "No entry point reaches this"));
    drop(sim);
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("test_work"))).into(),
    );
    assert!(
        !shows(&mut sim, "REACHED FROM"),
        "a test is an entry of its own kind"
    );
}

/// The graph overlays' MOST CHANGED section lists the hottest files with
/// their counts (a loading note before the history arrives, nothing for a
/// project without one), and the calls overlay files entry points under
/// their own heading, out of the "uncalled" list.
#[test]
fn the_overlays_list_the_most_changed_files_and_the_entry_points() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    assert!(super::churn_rows(&app).is_empty(), "no history: no section");
    app.proj.churn_loading = true;
    let mut sim = sim_elem(iced::widget::Column::with_children(super::churn_rows(&app)).into());
    assert!(shows(&mut sim, "Reading the change history"));
    drop(sim);
    app.proj.churn_loading = false;
    app.proj.churn = Some(std::sync::Arc::new(crate::Churn::from_files(
        &root,
        vec![
            clew_protocol::FileChurn {
                rel: "src/hot.rs".into(),
                commits: 7,
                last: 0,
            },
            clew_protocol::FileChurn {
                rel: "src/lib.rs".into(),
                commits: 1,
                last: 0,
            },
        ],
        300,
    )));
    let mut sim = sim_elem(iced::widget::Column::with_children(super::churn_rows(&app)).into());
    assert!(shows(&mut sim, "MOST CHANGED (LAST 300 COMMITS)"));
    assert!(shows(&mut sim, "hot.rs"));
    assert!(shows(&mut sim, "7 commits"));
    assert!(shows(&mut sim, "1 commit"));
    drop(sim);

    // main is an entry point; helper is uncalled; a test is neither.
    let lib = root.join("src/lib.rs");
    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 1,
            callers,
            callees,
        };
    let g = crate::projectcalls::ProjectCallGraph::from_wire(
        clew_protocol::CallGraph {
            nodes: vec![
                call("main", vec![], vec![]),
                call("helper", vec![], vec![]),
                call("test_it", vec![], vec![]),
            ],
        },
        |rel| root.join(rel),
    )
    .expect("a call graph");
    let symbol =
        |name: &str, line: usize, entry: Option<crate::index::EntryKind>, is_test: bool| {
            crate::index::SymbolEntry {
                name: name.into(),
                kind: "function".into(),
                rel: "src/lib.rs".into(),
                abs: lib.clone(),
                line,
                is_test,
                entry,
            }
        };
    app.proj.symbol_index_by_file.insert(
        lib.clone(),
        std::sync::Arc::new(vec![
            symbol("main", 1, Some(crate::index::EntryKind::Main), false),
            symbol("helper", 1, None, false),
            symbol("test_it", 1, None, true),
        ]),
    );
    let summary = super::calls_summary(&app, &g);
    let named = |ids: &[usize]| {
        ids.iter()
            .map(|&i| g.node(i).name.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(summary.entries, [(0, "main")]);
    assert_eq!(
        named(
            &summary
                .uncalled
                .iter()
                .map(|&(id, _)| id)
                .collect::<Vec<_>>()
        ),
        ["helper"]
    );
}

/// What the language reaches without a call naming it is not "uncalled":
/// a property, a special method, the `__init__` of a class that is called,
/// a subclass's override of a called method — as a language server's graph
/// leaves them, with edges to none. A class called nowhere keeps its
/// `__init__` listed, and a function called nowhere is listed.
#[test]
fn the_uncalled_list_leaves_out_what_python_reaches_by_itself() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let shop = root.join("shop.py");
    let src = "\
class Discount:
    def amount_off(self, order):
        return 0

class Bulk(Discount):
    def amount_off(self, order):
        return 1

class Inventory:
    def __init__(self):
        self.tins = {}

    @property
    def count(self):
        return len(self.tins)

    def __repr__(self):
        return 'Inventory'

class Ghost:
    def __init__(self):
        pass

def unused():
    return 1

def main(rule):
    inventory = Inventory()
    rule.amount_off(None)
";
    app.proj.calls_by_file.insert(
        shop.clone(),
        std::sync::Arc::new(crate::projectcalls::FileCalls::read(&shop, src).unwrap()),
    );
    let node = |name: &str, line: usize, callers: Vec<usize>| clew_protocol::CallGraphNode {
        name: name.into(),
        kind: "method".into(),
        file: "shop.py".into(),
        line,
        callers,
        callees: Vec::new(),
    };
    let g = crate::projectcalls::ProjectCallGraph::from_wire(
        clew_protocol::CallGraph {
            nodes: vec![
                node("amount_off", 2, vec![7]),
                node("amount_off", 6, vec![]),
                node("__init__", 10, vec![]),
                node("count", 14, vec![]),
                node("__repr__", 17, vec![]),
                node("__init__", 21, vec![]),
                node("unused", 24, vec![]),
                node("main", 27, vec![]),
            ],
        },
        |rel| root.join(rel),
    )
    .expect("a call graph");
    let summary = super::calls_summary(&app, &g);
    let uncalled: Vec<String> = summary
        .uncalled
        .iter()
        .map(|&(id, _)| format!("{}@{}", g.node(id).name, g.node(id).line))
        .collect();
    assert_eq!(uncalled, ["__init__@21", "main@27", "unused@24"]);
}

/// A project graph's English is said in the reader's language — its
/// words, and the shapes that hold a name — while code stays code; and a
/// section's heading is in capitals only in a language that has them.
#[test]
fn a_project_graphs_words_are_translated() {
    use crate::i18n::{Lang, with_lang};
    with_lang(Lang::Zh, || {
        assert_eq!(super::map_text("inherits"), "继承");
        assert_eq!(super::map_text("handled by"), "处理函数");
        assert_eq!(super::map_text("base 2"), "基类 2");
        assert_eq!(
            super::map_text("MRO: D → B → object"),
            "方法解析顺序：D → B → object"
        );
        assert_eq!(
            super::map_text("id serial (primary key) → users"),
            "id serial (主键) → users"
        );
        assert_eq!(super::map_text("1 lemmas"), "1 条引理");
        assert_eq!(super::map_text("pub use"), "pub use");
        assert_eq!(super::map_text("gen_server:call"), "gen_server:call");
        assert_eq!(
            crate::i18n::caps("入口点（main、路由）"),
            "入口点（main、路由）"
        );
    });
    with_lang(Lang::En, || {
        assert_eq!(super::map_text("inherits"), "inherits");
        assert_eq!(super::map_text("base 2"), "base 2");
        assert_eq!(
            crate::i18n::caps("Entry points (main)"),
            "ENTRY POINTS (MAIN)"
        );
    });
    with_lang(Lang::Fr, || {
        assert_eq!(super::map_text("delegate to profile"), "délègue à profile");
    });
}

/// The Maps overlay's list: the map's counts and its line on how to read
/// it, its nodes by kind with their detail and their arrows, each opening
/// where it is defined; nothing yet says so, a failed read says why; the
/// picker names each map with its size; a node defined nowhere opens
/// nothing from the graph.
#[test]
fn the_maps_overlay_lists_a_maps_nodes_by_kind() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(&mut sim, "No graphs for this project."));
    drop(sim);
    app.proj.maps_error = Some("the host refused".into());
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(
        &mut sim,
        "Couldn't read the project's graphs: the host refused"
    ));
    drop(sim);
    app.proj.maps_error = None;
    let node =
        |label: &str, kind: &str, file: &str, line: usize, detail: &str| clew_protocol::MapNode {
            label: label.into(),
            kind: kind.into(),
            file: file.into(),
            line,
            detail: detail.into(),
        };
    app.proj.maps = Arc::new(vec![
        clew_protocol::ProjectMap {
            id: "sql.schema".into(),
            title: "Database schema".into(),
            description: "an arrow is a foreign key".into(),
            nodes: vec![
                node(
                    "orders",
                    "table",
                    "db/schema.sql",
                    7,
                    "id int\nuser_id int → users",
                ),
                node("users", "table", "db/schema.sql", 1, "id int"),
            ],
            edges: vec![clew_protocol::MapEdge {
                from: 0,
                to: 1,
                label: "user_id".into(),
            }],
        },
        clew_protocol::ProjectMap {
            id: "routes".into(),
            title: "Routes".into(),
            description: "a route to its handler".into(),
            nodes: vec![node("GET /users", "route", "", 0, "")],
            edges: Vec::new(),
        },
    ]);
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(&mut sim, "Database schema, 2 nodes, 1 arrow"));
    assert!(shows(&mut sim, "an arrow is a foreign key"));
    assert!(shows(&mut sim, "TABLE"));
    assert!(shows(&mut sim, "db/schema.sql:7"));
    assert!(shows(&mut sim, "user_id int → users"));
    assert!(shows(&mut sim, "→ users (user_id)"));
    drop(sim);
    app.proj.maps_selected = Some("routes".into());
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(&mut sim, "Routes, 1 node, 0 arrows"));
    assert!(shows(&mut sim, "GET /users"));
    drop(sim);
    assert!(super::map_picker(&app).is_some());
    let choice = super::MapChoice {
        id: "routes".into(),
        title: "Routes".into(),
        nodes: 1,
    };
    assert_eq!(choice.to_string(), "Routes (1)");
    // From the graph: a node opens where it is defined; one defined nowhere,
    // nothing.
    let layout = crate::app::project_map_layout(&app.proj.maps[0], &root);
    let canvas =
        super::GraphCanvas::new(&layout, 1, crate::Overlay::ProjectMaps, true, true, false);
    let orders = layout
        .nodes
        .iter()
        .position(|n| n.label == "orders")
        .unwrap();
    assert!(matches!(
        canvas.open_message(orders),
        Some(Message::Graph(GraphMsg::OverlayOpenAt { abs, line: 7 })) if abs == root.join("db/schema.sql")
    ));
    let layout = crate::app::project_map_layout(&app.proj.maps[1], &root);
    let canvas =
        super::GraphCanvas::new(&layout, 1, crate::Overlay::ProjectMaps, true, true, false);
    assert!(canvas.open_message(0).is_none());
}

/// The type map's list: the counts, the most referenced types, the base
/// types and the most dependent ones, each row opening its definition;
/// an empty map says so, or that the index is being built.
#[test]
fn the_type_map_lists_the_most_referenced_and_base_types() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    // An index built, and no types in it.
    app.proj.docs.generation = 1;
    let mut sim = sim_elem(super::project_types_body(&app));
    assert!(shows(&mut sim, "No types found in this project."));
    drop(sim);
    let item = |name: &str, kind: &str, line: usize, signature: &str, refs: &[&str]| {
        clew_protocol::DocItem {
            name: name.into(),
            kind: kind.into(),
            signature: signature.into(),
            doc: String::new(),
            line,
            public: true,
            children: Vec::new(),
            refs: refs.iter().map(|r| r.to_string()).collect(),
        }
    };
    let files = vec![clew_protocol::DocFile {
        doc: String::new(),
        rel: "src/lib.rs".into(),
        items: vec![
            item(
                "Order",
                "struct",
                1,
                "pub struct Order {",
                &["Customer", "Priced"],
            ),
            item("Customer", "struct", 8, "pub struct Customer {", &[]),
            item("Priced", "trait", 12, "pub trait Priced {", &[]),
            item("Sale", "struct", 20, "pub struct Sale {", &["Customer"]),
        ],
    }];
    let mut structure = clew_protocol::StructureIndex::default();
    structure
        .by_type
        .entry("Sale".into())
        .or_default()
        .traits
        .push("Priced".into());
    app.proj.type_graph = std::sync::Arc::new(crate::typegraph::TypeGraph::build(
        &root, &files, &structure,
    ));
    let mut sim = sim_elem(super::project_types_body(&app));
    assert!(shows(&mut sim, "4 types, 4 relations, 1 inheritance link"));
    assert!(shows(&mut sim, "MOST REFERENCED (USED BY THE MOST TYPES)"));
    assert!(shows(&mut sim, "Customer"));
    assert!(shows(&mut sim, "2 ←"), "Customer is named twice");
    assert!(shows(&mut sim, "BASE TYPES (MOST SUBTYPES)"));
    assert!(shows(&mut sim, "1 subtype"));
    assert!(shows(&mut sim, "MOST DEPENDENCIES (USES THE MOST TYPES)"));
    assert!(shows(&mut sim, "→ 2"), "Order names two");
    drop(sim);
    // None inherits: the count says none — in French too, where nought takes
    // the singular.
    let structured = std::mem::replace(
        &mut app.proj.type_graph,
        std::sync::Arc::new(crate::typegraph::TypeGraph::build(
            &root,
            &files,
            &Default::default(),
        )),
    );
    let mut sim = sim_elem(super::project_types_body(&app));
    assert!(shows(&mut sim, ", 0 inheritance links"));
    drop(sim);
    crate::i18n::with_lang(crate::i18n::Lang::Fr, || {
        let mut sim = sim_elem(super::project_types_body(&app));
        assert!(shows(&mut sim, ", 0 lien d’héritage"));
    });
    app.proj.type_graph = structured;
    // A node of the type map opens its definition at its line.
    let layout = crate::graphlayout::layout(
        vec![crate::graphlayout::NodeInput {
            label: "Customer".into(),
            file: root.join("src/lib.rs"),
            line: 8,
            weight: 1.0,
            cyclic: false,
        }],
        Vec::new(),
    );
    let canvas =
        super::GraphCanvas::new(&layout, 1, crate::Overlay::ProjectTypes, true, true, false);
    assert!(matches!(
        canvas.open_message(0),
        Some(Message::Graph(GraphMsg::OverlayOpenAt { abs, line: 8 })) if abs == root.join("src/lib.rs")
    ));
}

/// The last run's trace in the views: the debug panel counts its stops and
/// offers the walkthrough, the WALK tab offers it too, a `@trace` scope has a
/// label, and the calls overlay lists the functions the run stopped in with
/// their counts.
/// A plain run's panel is its output and how it ended: titled Run, its exit
/// code said, no stack, variables or watches, nothing to step, and Stop
/// gone quiet once the program is. A debug run keeps its columns, and its
/// Stop goes quiet too when it ends.
#[test]
fn a_plain_runs_panel_is_its_output_and_how_it_ended() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let session = |status, exit_code| crate::DebugSession {
        client: None,
        status,
        exit_code,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: vec![("stdout".into(), "Sencha    37.50\n".into())],
        current: None,
        program: root.join("main.py"),
        args: Vec::new(),
        cwd: root.clone(),
        addr: None,
    };
    let stops = |app: &App| {
        let mut sim = sim_elem(super::debug_panel(app));
        let _ = sim.click("■ Stop");
        sent_by(sim)
            .into_iter()
            .any(|m| matches!(m, Message::Debug(DebugMsg::Stop)))
    };
    app.debug.no_debug = true;
    app.debug.session = Some(session(crate::DebugStatus::Running, None));
    {
        let mut sim = sim_elem(super::debug_panel(&app));
        assert!(shows(&mut sim, "Run"));
        assert!(shows(&mut sim, "Sencha    37.50"));
        assert!(!shows(&mut sim, "CALL STACK"));
        assert!(!shows(&mut sim, "▶ Continue"));
    }
    assert!(stops(&app), "a running program can be stopped");
    app.debug.session = Some(session(crate::DebugStatus::Terminated, Some(0)));
    {
        let mut sim = sim_elem(super::debug_panel(&app));
        assert!(shows(&mut sim, "exited with code 0"));
        assert!(!shows(&mut sim, "terminated"));
    }
    assert!(!stops(&app), "nothing is left to stop");
    app.debug.no_debug = false;
    app.debug.session = Some(session(crate::DebugStatus::Terminated, None));
    {
        // A debug run that ended without ever stopping: still a debug run,
        // but its output has the panel — its stack and variables were empty.
        let mut sim = sim_elem(super::debug_panel(&app));
        assert!(shows(&mut sim, "Debug"));
        assert!(shows(&mut sim, "terminated"));
        assert!(shows(&mut sim, "↻ Debug Again"));
        assert!(!shows(&mut sim, "CALL STACK"));
    }
    assert!(!stops(&app));
    // One that stopped keeps its columns.
    let mut stopped = session(crate::DebugStatus::Terminated, None);
    stopped.frames = vec![crate::dap::StackFrame {
        id: 1,
        name: "main".into(),
        path: Some(root.join("main.py")),
        line: 3,
        column: 1,
    }];
    app.debug.session = Some(stopped);
    let mut sim = sim_elem(super::debug_panel(&app));
    assert!(shows(&mut sim, "CALL STACK"));
}

#[test]
fn the_last_runs_trace_is_offered_as_a_walkthrough_and_listed_on_the_call_graph() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    assert_eq!(super::scope_label("@trace app"), "Debug run (app)");
    assert_eq!(super::scope_label("@trace"), "Debug run");
    let mut sim = sim_elem(super::trace_control(&app));
    assert!(shows(&mut sim, "Trace: no stops yet"));
    drop(sim);
    // A run that ended without stopping has no stops, not "yet".
    app.debug.session = Some(crate::DebugSession {
        client: None,
        status: crate::DebugStatus::Terminated,
        exit_code: None,
        thread_id: None,
        frames: Vec::new(),
        scopes: Vec::new(),
        watches: Vec::new(),
        output: Vec::new(),
        current: None,
        program: root.join("app"),
        args: Vec::new(),
        cwd: root.clone(),
        addr: None,
    });
    let mut sim = sim_elem(super::trace_control(&app));
    assert!(shows(&mut sim, "Trace: no stops"));
    assert!(!shows(&mut sim, "Trace: no stops yet"));
    drop(sim);
    app.debug.session = None;
    let frame = |name: &str, line: usize| crate::TraceFrame {
        name: name.into(),
        path: Some(root.join("src/lib.rs")),
        line,
    };
    app.debug.trace = vec![
        crate::TraceStop {
            reason: "breakpoint".into(),
            frames: vec![frame("line_0", 1)],
        },
        crate::TraceStop {
            reason: "step".into(),
            frames: vec![frame("line_1", 2), frame("line_0", 1)],
        },
        crate::TraceStop {
            reason: "step".into(),
            frames: vec![frame("line_1", 3)],
        },
    ];
    app.debug.trace_rev += 1;
    let mut sim = sim_elem(super::trace_control(&app));
    assert!(shows(&mut sim, "Trace: 3 stops"));
    let _ = sim.click("Walk this run");
    let sent: Vec<Message> = sent_by(sim).into_iter().collect();
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Walk(WalkMsg::GenerateTrace))),
        "{sent:?}"
    );
    app.sidebar = crate::SidebarTab::Walk;
    app.show_left_sidebar = true;
    let mut sim = sim_elem(super::walk_tab(&app));
    assert!(shows(&mut sim, "Walk the last run (3 stops)"));
    drop(sim);

    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 1,
            callers,
            callees,
        };
    let g = crate::projectcalls::ProjectCallGraph::from_wire(
        clew_protocol::CallGraph {
            nodes: vec![
                call("line_0", vec![], vec![1]),
                call("line_1", vec![0], vec![]),
            ],
        },
        |rel| root.join(rel),
    )
    .expect("a call graph");
    let visits = super::trace_visits(&app, &g);
    let named: Vec<(&str, usize)> = visits
        .iter()
        .map(|&(id, n)| (g.node(id).name.as_str(), n))
        .collect();
    assert_eq!(named, [("line_1", 2), ("line_0", 1)]);
    assert_eq!(super::trace_files(&app).len(), 1);
    app.proj.project_calls.graph = std::sync::Arc::new(g);
    let mut sim = sim_elem(super::project_calls_body(&app));
    assert!(shows(&mut sim, "WHERE THE LAST DEBUG RUN STOPPED"));
    assert!(shows(&mut sim, "2 stops"));
}

/// The FLOW tab: the traced identifier, its occurrences under their roles
/// with their lines, an unfold on a `Passed` row, and the context menu's
/// way in.
#[test]
fn the_flow_tab_lists_occurrences_under_their_roles() {
    use crate::app::flow::{FlowNode, FlowTree, Role};
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let mut sim = sim_elem(super::flow_tab(&app));
    assert!(shows(&mut sim, "No value trace yet"));
    drop(sim);
    let node = |role: Role, detail: &str, line: usize, text: &str| FlowNode {
        symbol: "total".into(),
        role,
        detail: detail.into(),
        argument: (role == Role::Passed).then_some(0),
        callee: (role == Role::Passed).then(|| (detail.to_string(), 4)),
        abs: root.join("src/lib.rs"),
        rel: "src/lib.rs".into(),
        line,
        character: 0,
        col: 0,
        text: text.into(),
        classified: !text.is_empty(),
        indent: 0,
        changed: false,
        depth: 0,
        parent: None,
        children: None,
        expanded: false,
        loading: false,
    };
    let mut tree = FlowTree::new(7, "total".into(), "rust", (root.join("src/lib.rs"), 0));
    tree.push_root(node(
        Role::Assigned,
        "compute()",
        0,
        "let total = compute();",
    ));
    tree.push_root(node(Role::Passed, "render", 3, "render(total)"));
    tree.push_root(node(Role::Read, "", 9, ""));
    app.proj.flow = Some(tree);
    app.sidebar = crate::SidebarTab::Flow;
    let mut sim = sim_elem(super::flow_tab(&app));
    assert!(shows(&mut sim, "total"));
    assert!(shows(&mut sim, "3 places"));
    assert!(shows(&mut sim, "ASSIGNED"));
    assert!(shows(&mut sim, "= compute()"));
    assert!(shows(&mut sim, "PASSED TO"));
    assert!(shows(&mut sim, "→ render"));
    assert!(shows(&mut sim, "src/lib.rs:4"));
    assert!(shows(&mut sim, "(line not read)"));
    drop(sim);
    let sent = click_left_of(
        || sim_elem(super::flow_tab(&app)),
        "→ render",
        0,
        |m| matches!(m, Message::Flow(_)),
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Flow(crate::FlowMsg::Expand { token: 7, id: 1 }))),
        "{sent:?}"
    );
    // The tab is in the strip, and the context menu offers the trace.
    assert!(
        super::SIDEBAR_TABS
            .iter()
            .any(|(name, tab)| *name == "FLOW" && *tab == crate::SidebarTab::Flow)
    );
    app.proj.context_menu = Some(crate::ContextMenu {
        pane: 0,
        line: 0,
        col: 0,
        x: 10.0,
        y: 10.0,
        insight: None,
        served: true,
        by_pointer: true,
    });
    assert!(shows(&mut sim_of(&app), "Trace Value"));
    app.proj.context_menu = None;

    // A traced file changed and a line is gone: the row says so, and the
    // tab offers to trace again.
    {
        let tree = app.proj.flow.as_mut().unwrap();
        tree.stale = true;
        tree.node_mut(1).unwrap().changed = true;
    }
    assert!(shows(&mut sim_elem(super::flow_tab(&app)), "changed"));
    let mut sim = sim_elem(super::flow_tab(&app));
    let _ = sim.click("Trace again");
    let sent: Vec<Message> = sent_by(sim).into_iter().collect();
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Flow(crate::FlowMsg::Retrace))),
        "{sent:?}"
    );
}

fn doc_file(rel: &str, items: &[(&str, bool)]) -> clew_protocol::DocFile {
    serde_json::from_value(serde_json::json!({
        "rel": rel,
        "doc": "",
        "items": items.iter().enumerate().map(|(i, (name, public))| serde_json::json!({
            "name": name, "kind": "fn", "line": i + 1, "public": public,
            "signature": "", "doc": "", "children": [],
        })).collect::<Vec<_>>(),
    }))
    .expect("a DocFile")
}

/// The API docs' grouping and visibility are pairs with the chosen half
/// raised: pressing the other half switches, pressing the chosen one does
/// nothing — a chip that named what was on and flipped read as two tabs
/// with neither picked.
#[test]
fn the_docs_toggles_show_which_is_chosen() {
    let mut app = reader_app();
    app.proj.docs.files = vec![doc_file("src/lib.rs", &[("open", true)])];
    let sent = |app: &App, label: &str| {
        click(sim_elem(super::docs_stats::docs_tab(app)), label)
            .into_iter()
            .filter(|m| matches!(m, Message::Docs(_)))
            .collect::<Vec<_>>()
    };
    for both in ["Files", "Modules", "Public", "All"] {
        assert!(
            shows(&mut sim_elem(super::docs_stats::docs_tab(&app)), both),
            "{both}"
        );
    }
    assert!(sent(&app, "Files").is_empty(), "Files is chosen");
    assert!(matches!(
        sent(&app, "Modules").as_slice(),
        [Message::Docs(DocsMsg::ToggleGrouping)]
    ));
    assert!(sent(&app, "Public").is_empty(), "Public is chosen");
    assert!(matches!(
        sent(&app, "All").as_slice(),
        [Message::Docs(DocsMsg::ToggleShowAll)]
    ));
    app.docs_view.by_module = true;
    assert!(sent(&app, "Modules").is_empty(), "now Modules is");
    assert!(matches!(
        sent(&app, "Files").as_slice(),
        [Message::Docs(DocsMsg::ToggleGrouping)]
    ));
}

/// I5: the DOCS grouping is computed per installed index and view options,
/// and a newly installed index is a new generation.
#[test]
fn the_docs_grouping_follows_the_installed_index() {
    // `src/lsp.rs` and `src/lsp/mod.rs` are both module `lsp`.
    let files = vec![
        doc_file(
            "src/lsp.rs",
            &[("zeta", true), ("alpha", true), ("hidden", false)],
        ),
        doc_file("src/lsp/mod.rs", &[("beta", true)]),
        doc_file("src/main.rs", &[("main", true)]),
    ];
    let by_file = super::docs_groups(&files, false, false, "");
    assert_eq!(
        by_file.iter().map(|g| g.label.as_str()).collect::<Vec<_>>(),
        ["src/lsp.rs", "src/lsp/mod.rs", "src/main.rs"]
    );
    assert_eq!(
        by_file[0].items,
        [(0, 0), (0, 1)],
        "public only, file order"
    );
    let by_module = super::docs_groups(&files, true, true, "");
    let lsp = by_module
        .iter()
        .find(|g| g.label == "lsp")
        .expect("merged module");
    let names: Vec<&str> = lsp
        .items
        .iter()
        .map(|&(f, i)| files[f].items[i].name.as_str())
        .collect();
    assert_eq!(
        names,
        ["alpha", "beta", "hidden", "zeta"],
        "merged, sorted by name, private items included when asked"
    );
    let filtered = super::docs_groups(&files, false, false, "main");
    assert_eq!(filtered.len(), 1);

    // Drawn BEFORE the replacement, so the memo holds the first index's
    // grouping when the second arrives — the replacement must not be served
    // the grouping it memoized for the first.
    let mut app = reader_app();
    app.sidebar = crate::SidebarTab::Docs;
    app.show_left_sidebar = true;
    let _ = app.apply_docs(files);
    let first = app.proj.docs.generation;
    assert!(shows(&mut sim_of(&app), "src/main.rs"));
    let _ = app.update(Message::Noop);
    assert_eq!(app.proj.docs.generation, first, "nothing changed");
    let _ = app.apply_docs(vec![doc_file("src/other.rs", &[("other", true)])]);
    assert!(app.proj.docs.generation > first, "a new index");
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "src/other.rs"));
    assert!(
        !shows(&mut sim, "src/main.rs"),
        "the first index's grouping was served"
    );
}

/// I8: the rows send what they show — identities, not positions.
#[test]
fn rows_send_the_identity_of_what_they_show() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    // Bookmarks: the ✕ of the second row names that bookmark.
    app.proj.bookmarks = vec![
        crate::Bookmark {
            rel: "a.rs".into(),
            line: 3,
            preview: "fn a()".into(),
            note: None,
        },
        crate::Bookmark {
            rel: "b.rs".into(),
            line: 7,
            preview: "fn b()".into(),
            note: None,
        },
    ];
    app.sidebar = crate::SidebarTab::Marks;
    app.show_left_sidebar = true;
    let mut sim = sim_of(&app);
    let row = text_at(&mut sim, |t| t == "fn b()", 0).expect("the bookmark rows are on screen");
    // The ✕ is the row's last control, at the sidebar's right edge (an SVG
    // glyph, so it is found by position rather than by text).
    let close = Point::new(app.sidebar_width - 14.0, row.center_y());
    sim.point_at(close);
    let _ = sim.simulate(iced_test::simulator::click());
    let sent: Vec<Message> = sent_by(sim).into_iter().collect();
    assert!(
        matches!(sent.as_slice(), [Message::Reading(ReadingMsg::BookmarkRemoved { rel, line: 7 })] if rel == "b.rs"),
        "{sent:?}"
    );
    app.show_left_sidebar = false;

    // The Ask chips.
    let pin = crate::AskPin {
        rel: "src/lib.rs".into(),
        file: root.join("src/lib.rs"),
        line: 4,
        code: "let x = 1;".into(),
    };
    app.proj.ask_pins = vec![pin.clone()];
    app.ask_view = crate::AskView::Compact;
    let sent = click(sim_of(&app), "📎 src/lib.rs:4");
    assert!(
        matches!(sent.as_slice(), [Message::Ask(AskMsg::PinGoto(k))] if *k == pin.key()),
        "{sent:?}"
    );

    // Saved hosts.
    app.show_bottom = false;
    let saved = crate::connect::SavedConnection {
        name: "prod".into(),
        host: "b.example".into(),
        user: "root".into(),
        port: 2222,
        identity: String::new(),
        send_ai_keys: false,
    };
    app.saved_connections = vec![saved.clone()];
    app.connect = Some(crate::ConnectUi::default());
    // The row names where it goes, the port with it: two hosts on one
    // machine read the same without it.
    assert!(shows(&mut sim_of(&app), "root@b.example:2222"));
    let sent = click(sim_of(&app), "prod");
    assert!(
        matches!(sent.as_slice(), [Message::Connect(ConnectMsg::ToSaved { user_host, port: 2222 })]
            if *user_host == saved.user_host()),
        "{sent:?}"
    );
    // An unnamed host on ssh's own port is said once, not twice.
    let plain = crate::connect::SavedConnection {
        name: String::new(),
        port: 22,
        ..saved.clone()
    };
    app.saved_connections = vec![plain];
    {
        let mut sim = sim_of(&app);
        assert!(text_at(&mut sim, |t| t == "root@b.example", 0).is_some());
        assert!(text_at(&mut sim, |t| t == "root@b.example", 1).is_none());
    }
    app.connect = None;

    // The symbol finder.
    app.proj.symbol_index = Arc::new(vec![crate::index::SymbolEntry {
        name: "origin".into(),
        kind: "function".into(),
        rel: "src/lib.rs".into(),
        abs: root.join("src/lib.rs"),
        line: 12,
        is_test: false,
        entry: None,
    }]);
    app.proj.finder.open = true;
    app.proj.finder.mode = crate::finder::FinderMode::Symbols;
    app.proj.finder.results = vec![0];
    let sent = click(sim_of(&app), "origin");
    assert!(
        matches!(sent.as_slice(), [Message::Nav(NavMsg::FinderPick { abs, line: Some(12), .. })]
            if *abs == root.join("src/lib.rs")),
        "{sent:?}"
    );
}

fn tour(scope: &str) -> crate::walkthrough::Walkthrough {
    crate::walkthrough::Walkthrough {
        title: format!("Tour of {scope}"),
        scope: scope.into(),
        steps: vec![
            crate::walkthrough::Step {
                title: format!("{scope} one"),
                file: "src/lib.rs".into(),
                symbol: None,
                line: Some(1),
                narration: "n".into(),
                depth: 0,
            },
            crate::walkthrough::Step {
                title: format!("{scope} two"),
                file: "src/lib.rs".into(),
                symbol: None,
                line: Some(2),
                narration: "n".into(),
                depth: 0,
            },
        ],
    }
}

/// I8: handlers resolve identities against the CURRENT lists, so an entry
/// that moved (another window's save, an earlier removal) is still the one
/// acted on, and an identity that is gone acts on nothing.
#[test]
fn identity_messages_resolve_against_the_current_lists() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();

    // Ask pins: the second pin, after the first is gone.
    let pin = |line: usize| crate::AskPin {
        rel: "src/lib.rs".into(),
        file: root.join("src/lib.rs"),
        line,
        code: format!("line {line}"),
    };
    app.proj.ask_pins = vec![pin(1), pin(2), pin(3)];
    let third = pin(3).key();
    let _ = app.update(Message::Ask(AskMsg::Unpin(pin(1).key())));
    let _ = app.update(Message::Ask(AskMsg::Unpin(third)));
    assert_eq!(
        app.proj.ask_pins.iter().map(|p| p.line).collect::<Vec<_>>(),
        [2],
        "the pins named were removed, whatever their position"
    );
    let _ = app.update(Message::Ask(AskMsg::Unpin(third)));
    assert_eq!(app.proj.ask_pins.len(), 1, "a gone pin names nothing");

    // Walkthroughs: opened and stepped by scope; a step for another tour is
    // ignored; the library order can change underneath.
    app.proj.walk.library = vec![tour("alpha"), tour("beta")];
    let _ = app.update(Message::Walk(WalkMsg::Open("beta".into())));
    assert_eq!(app.proj.walk.open.as_deref(), Some("beta"));
    // Another window's tour lands ahead of it: the open tour stays itself.
    app.proj.walk.library.insert(0, tour("new"));
    assert_eq!(
        app.proj.walk.open_tour().map(|w| w.scope.as_str()),
        Some("beta")
    );
    let _ = app.update(Message::Walk(WalkMsg::Goto {
        scope: "alpha".into(),
        step: 1,
    }));
    assert_eq!(
        app.proj.walk.step, 0,
        "a step of another tour moved this one"
    );
    let _ = app.update(Message::Walk(WalkMsg::Goto {
        scope: "beta".into(),
        step: 1,
    }));
    assert_eq!(app.proj.walk.step, 1);
    let _ = app.update(Message::Walk(WalkMsg::Open("gone".into())));
    assert_eq!(
        app.proj.walk.open.as_deref(),
        Some("beta"),
        "an unknown scope opens nothing"
    );

    // Saved hosts: connected to by identity.
    let host = |h: &str| crate::connect::SavedConnection {
        name: String::new(),
        host: h.into(),
        user: "root".into(),
        port: 22,
        identity: String::new(),
        send_ai_keys: false,
    };
    app.saved_connections = vec![host("a.example"), host("b.example")];
    let b = host("b.example");
    app.saved_connections.remove(0); // another window forgot a.example
    let _ = app.update(Message::Connect(ConnectMsg::ToSaved {
        user_host: b.user_host(),
        port: 22,
    }));
    assert_eq!(app.connection, b.target());
    let before = app.conn_gen;
    let _ = app.update(Message::Connect(ConnectMsg::ToSaved {
        user_host: "root@gone.example".into(),
        port: 22,
    }));
    assert_eq!(app.conn_gen, before, "an unknown host connects nowhere");
}

/// The welcome on a remote host says where the link stands: connected,
/// connecting, or not reached. A host that could not be reached read
/// "Connected to …" and offered its folders.
#[test]
fn the_remote_welcome_says_whether_the_host_was_reached() {
    let mut app = blank_app();
    let host = crate::connect::SavedConnection {
        name: String::new(),
        host: "h.example".into(),
        user: "root".into(),
        port: 22,
        identity: String::new(),
        send_ai_keys: false,
    };
    app.connection = host.target();
    assert!(shows(&mut sim_of(&app), "Connecting to root@h.example"));
    app.handshake_failure = Some("ssh is not installed here".into());
    assert!(shows(
        &mut sim_of(&app),
        "Could not connect to root@h.example"
    ));
    assert!(!shows(&mut sim_of(&app), "Connected to"));
    app.handshake_failure = None;
    app.server.ready = true;
    assert!(shows(&mut sim_of(&app), "Connected to root@h.example"));
    assert!(shows(&mut sim_of(&app), "Browse folders"));
}

/// I9: a remote project's tsconfig/jsconfig path mappings, once fetched from
/// the host, are what its resolver maps aliases through; a fetch that failed
/// says what that leaves (the aliases last read); a result for another
/// project instance is dropped.
#[test]
fn a_remote_projects_path_aliases_resolve_once_its_configs_arrive() {
    let mut app = blank_app();
    let root = PathBuf::from("/srv/remote-ts");
    let files: Vec<crate::fs_scan::FileEntry> =
        ["tsconfig.json", "src/app.ts", "src/components/Button.tsx"]
            .iter()
            .map(|rel| crate::fs_scan::FileEntry {
                abs: root.join(rel),
                rel: rel.to_string(),
            })
            .collect();
    app.connection = port_2222_target();
    app.proj.project = Some(crate::Project {
        root: root.clone(),
        tree: Default::default(),
        files: Arc::new(files),
        truncated: false,
    });
    let from = root.join("src/app.ts");
    let alias = |app: &App| {
        let raw = crate::imports::RawImport {
            // Only a config can say this: `@ui/…` otherwise reads as a
            // scoped package (unlike `@/…`, which defaults to `src/`).
            module: "@ui/Button".into(),
            line: 1,
            is_mod_decl: false,
        };
        app.import_resolver()
            .expect("a project is open")
            .resolve(&raw, &from, "typescript")
    };
    assert!(
        !matches!(alias(&app), crate::imports::Target::Internal(_)),
        "no aliases before the configs arrive"
    );

    let text = r#"{ "compilerOptions": { "baseUrl": "./src", "paths": { "@ui/*": ["./components/*"] } } }"#;
    let read = |p: &std::path::Path| (p == root.join("tsconfig.json")).then(|| text.to_string());
    let configs = crate::imports::TsConfigs::from(
        crate::imports::load_ts_config(&root, &root.join("tsconfig.json"), &read, 0)
            .into_iter()
            .collect::<Vec<_>>(),
    );
    // For another project instance: dropped.
    let _ = app.update(Message::Graph(GraphMsg::RemoteTsConfigsLoaded {
        stamp: Stamp {
            root: Some(root.clone()),
            epoch: app.project_epoch + 1,
            conn: None,
        },
        generation: app.proj.remote_ts_configs_gen,
        result: Ok(configs.clone()),
    }));
    assert!(!matches!(alias(&app), crate::imports::Target::Internal(_)));

    let _ = app.update(Message::Graph(GraphMsg::RemoteTsConfigsLoaded {
        stamp: Stamp {
            root: Some(root.clone()),
            epoch: app.project_epoch,
            conn: None,
        },
        generation: app.proj.remote_ts_configs_gen,
        result: Ok(configs),
    }));
    assert_eq!(
        alias(&app),
        crate::imports::Target::Internal(root.join("src/components/Button.tsx"))
    );

    let _ = app.update(Message::Graph(GraphMsg::RemoteTsConfigsLoaded {
        stamp: Stamp {
            root: Some(root.clone()),
            epoch: app.project_epoch,
            conn: None,
        },
        generation: app.proj.remote_ts_configs_gen,
        result: Err("the server is gone".into()),
    }));
    assert!(
        app.status.contains("resolve as last read"),
        "{}",
        app.status
    );
}

// ------------------------------------------------------------------------
// T3: smoke tests of the main views through the simulator — each state
// builds, and shows what a reader of it needs to see.
// ------------------------------------------------------------------------

/// A second file for the split, with its own name.
fn other_viewer(root: &std::path::Path) -> Viewer {
    let source = "pub fn other() {}\n".to_string();
    Viewer::new(
        root.join("src/other.rs"),
        "src/other.rs".into(),
        Some("rust"),
        Arc::new(source.clone()),
        crate::highlight::plain_lines(&source),
    )
}

/// A split shows both files, each under a header naming it; unsplit, the
/// second pane (and every header) is gone.
#[test]
fn a_split_shows_each_file_under_its_own_header() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.panes[1] = Some(other_viewer(&root));
    app.proj.split = true;
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "src/lib.rs"), "the first pane's header");
        assert!(shows(&mut sim, "src/other.rs"), "the second pane's header");
    }

    app.proj.split = false;
    let mut sim = sim_of(&app);
    assert!(
        !shows(&mut sim, "src/other.rs"),
        "the hidden pane still drew its header"
    );
}

/// A notebook renders as cells, through the path a server reply lands on
/// (`apply_notebook_content`): the shape strip, a code cell's execution
/// badge, a markdown cell as prose, and outputs folded until asked for —
/// the header's one click unfolds them.
#[test]
fn a_notebook_shows_its_cells_and_folds_its_outputs() {
    let mut app = reader_app();
    let code = "fit(data)\n";
    let cells = vec![
        clew_protocol::NotebookCell {
            kind: "markdown".into(),
            source: "The fit converges after twelve steps.\n".into(),
            lines: Vec::new(),
            proj_line: 1,
            outputs: Vec::new(),
            execution_count: None,
        },
        clew_protocol::NotebookCell {
            kind: "code".into(),
            source: code.into(),
            lines: crate::highlight::plain_lines(code),
            proj_line: 3,
            outputs: vec![clew_protocol::NotebookOutput::Text {
                spans: vec![("converged".into(), None)],
                stderr: false,
            }],
            execution_count: Some(3),
        },
    ];
    let projection = "# %% [markdown]\n# The fit converges after twelve steps.\n# %%\nfit(data)\n";
    let _ = app.apply_notebook_content(
        &[0],
        None,
        "analysis.ipynb".into(),
        "python".into(),
        cells,
        Vec::new(),
        projection.into(),
        false,
    );
    assert!(app.proj.panes[0].as_ref().unwrap().notebook.is_some());
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "2 cells, 1 with output, python"));
        assert!(shows(&mut sim, "In [3]"), "the code cell's execution badge");
        assert!(shows(&mut sim, "▸ output (1)"), "outputs start folded");
        assert!(shows(&mut sim, "Expand all outputs"));
    }
    // The markdown cell is prose (iced's markdown widget, whose text the
    // simulator's selectors do not reach): it was prepared for it.
    let doc = app.proj.panes[0]
        .as_ref()
        .unwrap()
        .notebook
        .clone()
        .unwrap();
    assert!(
        !doc.cells[0].segs.is_empty(),
        "the markdown cell was not prepared"
    );

    let expand = click(sim_of(&app), "Expand all outputs");
    for msg in expand {
        let _ = app.update(msg);
    }
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "▾ output (1)"), "the output unfolded");
    assert!(shows(&mut sim, "Collapse all outputs"));
}

/// Time travel takes over the active pane: the banner names the commit being
/// read (short sha, author, subject) with its exit, and the bar its scope;
/// before the first revision loads, the live file stays on screen rather than
/// a blank pane.
#[test]
fn time_travel_shows_the_commit_being_read() {
    let mut app = reader_app();
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.time_travel = Some(crate::TimeTravel {
        abs,
        rel: "src/lib.rs".into(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: vec![crate::git::HistCommit {
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            author: "Ada".into(),
            time: 0,
            subject: "Teach the parser about raw strings".into(),
            path: "src/lib.rs".into(),
        }],
        idx: 0,
        viewer: None,
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: true,
        generation: 0,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    let mut sim = sim_of(&app);
    assert!(
        shows(&mut sim, "0123456"),
        "the short sha, as git shortens it"
    );
    assert!(shows(&mut sim, "Ada"));
    assert!(shows(&mut sim, "Teach the parser about raw strings"));
    assert!(!shows(&mut sim, "Exit"), "the Time Travel button ends it");
    assert!(shows(&mut sim, "Scope: whole file"));
    assert!(
        !shows(&mut sim, "Loading revision…"),
        "the live file stays on screen until the revision loads"
    );
    let exit = click(sim_of(&app), "Time Travel");
    assert!(
        exit.iter()
            .any(|m| matches!(m, Message::TimeTravel(crate::TimeTravelMsg::Exit))),
        "{exit:?}"
    );
}

/// A remote window whose transport went down says so where the reader looks:
/// the connection chip still names the host, and the status bar says why a
/// file did not open — the file already on screen stays.
#[test]
fn a_disconnected_remote_window_says_so() {
    let mut app = reader_app();
    app.connection = crate::backend::connect::ConnTarget::Ssh {
        label: "user@host".into(),
        args: vec!["user@host".into()],
    };
    let conn = app.conn_gen;
    let _ = app.update(Message::Server(ServerMsg::Disconnected {
        conn,
        reason: None,
    }));
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let _ = app.open_file(root.join("src/other.rs"), None, true);
    assert_eq!(
        app.active_viewer().map(|v| v.rel.as_str()),
        Some("src/lib.rs")
    );
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "user@host"), "the chip names the host");
    assert!(
        shows(
            &mut sim,
            "Disconnected from the remote host — cannot open src/other.rs"
        ),
        "status: {}",
        app.status
    );
}

/// The Glossary page lists the project's terms under their kind, each a
/// button to its definition, filtered by the box on top; Close leaves it.
#[test]
fn the_glossary_page_lists_terms_by_kind() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let item = |name: &str, kind: &str, doc: &str, line: usize| clew_protocol::DocItem {
        name: name.into(),
        kind: kind.into(),
        signature: format!("{kind} {name}"),
        doc: doc.into(),
        line,
        public: true,
        children: Vec::new(),
        refs: Vec::new(),
    };
    app.proj.docs.files = vec![clew_protocol::DocFile {
        doc: String::new(),
        rel: "src/net.rs".into(),
        items: vec![
            item("Client", "struct", "A connection to one server.", 4),
            item(
                "frame",
                "function",
                "Frames a Remote Procedure Call (RPC) for the wire.",
                9,
            ),
        ],
    }];
    app.proj.docs.generation += 1;
    app.explain_cache_mut().insert(
        crate::explain::Node::File(root.join("src/net.rs")),
        crate::explain::Cached {
            summary: "Talks to the server.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    app.explain_cache_mut().insert(
        crate::explain::Node::Folder(root.join("src/wire")),
        crate::explain::Cached {
            summary: "The wire format.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let _ = app.update(Message::Glossary(GlossaryMsg::Open));
    assert!(app.proj.glossary.showing);

    let on_page = |app: &App, shown: &str| sim_of(app).find(shown).is_ok();
    for shown in [
        "src/wire/",
        "Glossary",
        "Types",
        "Modules",
        "Acronyms",
        "Client",
        "A connection to one server",
        "RPC",
        "Remote Procedure Call",
        "net",
        "Talks to the server",
        "src/net.rs:4",
    ] {
        assert!(on_page(&app, shown), "{shown:?} is not on the page");
    }
    // A term is a button to its definition.
    let sent = click(sim_of(&app), "Client");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Editor(EditorMsg::OpenRel { rel, line: Some(4) })] if rel == "src/net.rs"
        ),
        "{sent:?}"
    );
    // A folder is shown, with its explanation, not opened as a file.
    let sent = click(sim_of(&app), "src/wire");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Explain(crate::ExplainMsg::Show(crate::explain::Node::Folder(p)))]
                if p == &root.join("src/wire")
        ),
        "{sent:?}"
    );
    // The filter narrows the list, by name or definition.
    let _ = app.update(Message::Glossary(GlossaryMsg::FilterChanged(
        "procedure".into(),
    )));
    assert!(on_page(&app, "RPC"));
    assert!(!on_page(&app, "Client"), "Client survived the filter");
    assert!(!on_page(&app, "Types"), "an empty section kept its heading");
    let _ = app.update(Message::Glossary(GlossaryMsg::FilterChanged(
        "nothing-like-this".into(),
    )));
    assert!(on_page(
        &app,
        "No term matches \u{201c}nothing-like-this\u{201d}"
    ));
    // Its × leaves the page.
    let sent = click_right_of(
        || sim_of(&app),
        "Glossary",
        |m| matches!(m, Message::Glossary(GlossaryMsg::Close)),
    );
    // (The pointer over it opens its tip, which is said too.)
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Glossary(GlossaryMsg::Close))),
        "{sent:?}"
    );
    for msg in sent {
        let _ = app.update(msg);
    }
    assert!(!app.proj.glossary.showing);

    // With no terms the page says so and offers to look again: not to
    // rebuild what was never built (L17).
    app.proj.docs.files.clear();
    app.proj.docs.generation += 1;
    app.explain_cache_mut().clear();
    let _ = app.update(Message::Glossary(GlossaryMsg::Open));
    assert!(on_page(&app, "No terms found"));
    assert!(!on_page(&app, "Rebuild"));
    let sent = click(sim_of(&app), "Look again");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Docs(DocsMsg::Refresh))),
        "{sent:?}"
    );
}

/// The glossary caps each section on its own: a project with more types than
/// the page draws still shows its modules, and the long section says how
/// many it left out.
#[test]
fn a_long_glossary_section_does_not_hide_the_next() {
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let many = super::GLOSSARY_ROWS_SHOWN + 5;
    let items: Vec<clew_protocol::DocItem> = (0..many)
        .map(|i| clew_protocol::DocItem {
            name: format!("Type{i:03}"),
            kind: "struct".into(),
            signature: String::new(),
            doc: "A type.".into(),
            line: i + 1,
            public: true,
            children: Vec::new(),
            refs: Vec::new(),
        })
        .collect();
    app.proj.docs.files = vec![clew_protocol::DocFile {
        doc: String::new(),
        rel: "src/types.rs".into(),
        items,
    }];
    app.proj.docs.generation += 1;
    app.explain_cache_mut().insert(
        crate::explain::Node::File(root.join("src/net.rs")),
        crate::explain::Cached {
            summary: "Talks to the server.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let _ = app.update(Message::Glossary(GlossaryMsg::Open));
    // Tall enough that the whole list is on screen.
    let mut sim = iced_test::Simulator::with_size(
        Default::default(),
        Size::new(1000.0, 40_000.0),
        super::glossary_home(&app),
    );
    assert!(shows(&mut sim, "5 more — narrow the filter to see them"));
    assert!(
        shows(&mut sim, "Modules"),
        "the modules after a capped section"
    );
    assert!(shows(&mut sim, "Talks to the server"));
}

/// The settings' build choices, language by language: what each group
/// sets — and what the project's lsp.toml also sets — then what there is
/// to pick, each pick making the choice it names.
#[test]
fn the_languages_section_offers_each_languages_build_choices() {
    use clew_protocol::{
        COptions, CargoPackage, GoChoices, GoOptions, LanguageChoices, LanguageOptions,
        RustChoices, RustOptions, ToolchainCandidate,
    };
    let mut app = reader_app();
    app.proj.project_languages = vec!["c".into(), "go".into(), "lean".into(), "rust".into()];
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();
    app.proj.file_languages = vec!["lua".into()];
    app.proj.lang_choices = LanguageChoices {
        rust: Some(RustChoices {
            features: vec!["cli/color".into()],
            ..Default::default()
        }),
        go: Some(GoChoices {
            tags: vec!["e2e".into()],
            goos: Some("windows".into()),
            goarch: None,
        }),
        ..Default::default()
    };
    app.proj.lang_options = Some(Ok(LanguageOptions {
        rust: Some(RustOptions {
            packages: vec![
                CargoPackage {
                    name: "app".into(),
                    root: true,
                    features: vec!["serde".into(), "tls".into()],
                    default: vec!["tls".into()],
                },
                CargoPackage {
                    name: "cli".into(),
                    root: false,
                    features: vec!["color".into()],
                    default: vec![],
                },
            ],
            targets: vec!["thumbv7em-none-eabihf".into()],
            project_sets: vec!["cargo.features".into()],
        }),
        go: Some(GoOptions {
            tags: vec!["integration".into()],
            project_sets: vec![],
        }),
        c: Some(COptions {
            databases: vec!["/p/build".into()],
            project_sets: vec![],
        }),
        toolchains: [(
            "lean".to_string(),
            vec![ToolchainCandidate {
                command: "/h/.elan/bin/lake".into(),
                name: "elan".into(),
                project: true,
            }],
        )]
        .into(),
        python: None,
    }));
    let mut sim = sim_tall(super::connect::languages_section(&app));
    for shown in [
        "What rust-analyzer checks the project with. \
         Also set in .clew/lsp.toml: cargo.features. Settings here take precedence.",
        "Features",
        "Target",
        "cli:",
        "tls (default)",
        "The build tags and platform gopls uses for the project.",
        "e2e",
        "GOOS",
        "The compilation database and flags clangd compiles with.",
        "Automatic",
        "/p/build",
        "The toolchain the language server runs from.",
        "The lake found on the PATH",
        "elan (set by the project)",
        "Add a server program",
    ] {
        assert!(sim.find(shown).is_ok(), "{shown:?} is shown");
    }
    let _ = sim.click("serde");
    let _ = sim.click("integration");
    let chosen: Vec<LanguageChoices> = sim
        .into_messages()
        .filter_map(|m| match m {
            Message::Languages(crate::LanguagesMsg::Choose(next)) => Some(*next),
            _ => None,
        })
        .collect();
    assert_eq!(chosen.len(), 2, "{chosen:?}");
    assert_eq!(
        chosen[0].rust.as_ref().unwrap().features,
        ["cli/color", "serde"]
    );
    assert_eq!(chosen[1].go.as_ref().unwrap().tags, ["e2e", "integration"]);
    assert_eq!(
        chosen[1].go.as_ref().unwrap().goos.as_deref(),
        Some("windows")
    );
}

/// The TypeScript the TypeScript and JavaScript server runs, and the zig
/// zls uses: clew's own and the PATH's by default, the ones found to pick
/// from — the project's own TypeScript named as running its code, and said
/// so again once it is the one chosen.
#[test]
fn the_languages_section_picks_a_typescript_and_a_zig() {
    use clew_protocol::{LanguageChoices, LanguageOptions, ToolchainCandidate};
    let mut app = reader_app();
    app.proj.project_languages = vec!["typescript".into(), "zig".into()];
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();
    let projects = "/p/node_modules/typescript/lib/tsserver.js".to_string();
    app.proj.lang_options = Some(Ok(LanguageOptions {
        toolchains: [
            (
                "typescript".to_string(),
                vec![ToolchainCandidate {
                    command: projects.clone(),
                    name: "The project's TypeScript 5.4.5 — runs the project's own code".into(),
                    project: true,
                }],
            ),
            (
                "zig".to_string(),
                vec![ToolchainCandidate {
                    command: "/h/.zvm/0.13.0/zig".into(),
                    name: "Zig 0.13.0 (zvm)".into(),
                    project: true,
                }],
            ),
        ]
        .into(),
        ..Default::default()
    }));
    let warning = "Runs the project's own code. Only for projects you trust.";
    let mut sim = sim_tall(super::connect::languages_section(&app));
    for shown in [
        "TypeScript and JavaScript",
        "Clew's built-in TypeScript 5",
        "The project's TypeScript 5.4.5 — runs the project's own code",
        "Zig",
        "Zig 0.13.0 (zvm)",
    ] {
        assert!(sim.find(shown).is_ok(), "{shown:?} is shown");
    }
    assert!(sim.find(warning).is_err(), "not said before it is chosen");
    // The project's TypeScript, picked by its row.
    let _ = sim.click("The project's TypeScript 5.4.5 — runs the project's own code");
    let chosen: Vec<LanguageChoices> = sim
        .into_messages()
        .filter_map(|m| match m {
            Message::Languages(crate::LanguagesMsg::Choose(next)) => Some(*next),
            _ => None,
        })
        .collect();
    assert_eq!(chosen.len(), 1);
    assert_eq!(chosen[0].typescript.as_deref(), Some(projects.as_str()));

    app.proj.lang_choices = chosen[0].clone();
    let mut sim = sim_tall(super::connect::languages_section(&app));
    assert!(sim.find(warning).is_ok(), "said once it is the one chosen");
}

/// Java's JDK, PHP's version and C#'s solution are picked from the ones
/// found, the server's own pick by default; the Ruby whose Ruby LSP runs,
/// Erlang's ELP and Haskell's language server from the programs found.
#[test]
fn the_languages_section_picks_a_jdk_a_php_a_solution_and_programs() {
    use clew_protocol::{LanguageChoices, LanguageOptions, ToolchainCandidate};
    let mut app = reader_app();
    app.proj.project_languages = ["java", "php", "csharp", "ruby", "erlang", "haskell"]
        .map(String::from)
        .to_vec();
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();
    let one = |command: &str, name: &str| {
        vec![ToolchainCandidate {
            command: command.into(),
            name: name.into(),
            project: false,
        }]
    };
    app.proj.lang_options = Some(Ok(LanguageOptions {
        toolchains: [
            ("java".to_string(), one("/usr/lib/jvm/java-21", "JDK 21")),
            (
                "php".to_string(),
                one("8.2", "PHP 8.2 — composer.json's floor"),
            ),
            ("csharp".to_string(), one("/p/src/App.sln", "src/App.sln")),
            (
                "ruby".to_string(),
                one(
                    "/h/.rbenv/versions/3.3.0/bin/ruby-lsp",
                    "Ruby 3.3.0 (rbenv)",
                ),
            ),
            (
                "erlang".to_string(),
                one("/opt/elp/bin/elp", "elp in /opt/elp/bin"),
            ),
            (
                "haskell".to_string(),
                one("/h/.ghcup/bin/haskell-language-server-wrapper", "ghcup"),
            ),
        ]
        .into(),
        ..Default::default()
    }));
    let mut sim = sim_tall(super::connect::languages_section(&app));
    for shown in [
        "Java",
        "JDK 21",
        "PHP",
        "PHP 8.2 — composer.json's floor",
        "C#",
        "src/App.sln",
        "The Ruby that runs Ruby LSP.",
        "Ruby 3.3.0 (rbenv)",
        "The ruby-lsp found on the PATH",
        "The Erlang Language Platform build to run.",
        "The elp found on the PATH",
        "The Haskell language server to run.",
        "The haskell-language-server-wrapper found on the PATH",
    ] {
        assert!(sim.find(shown).is_ok(), "{shown:?} is shown");
    }
    let _ = sim.click("Ruby 3.3.0 (rbenv)");
    let chosen: Vec<LanguageChoices> = sim
        .into_messages()
        .filter_map(|m| match m {
            Message::Languages(crate::LanguagesMsg::Choose(next)) => Some(*next),
            _ => None,
        })
        .collect();
    assert_eq!(chosen.len(), 1);
    // The Ruby's row picks that Ruby's program.
    let ruby = chosen[0].servers.as_ref().and_then(|s| s.get("ruby"));
    assert_eq!(
        ruby.map(|r| (r.command.as_str(), r.args.is_none())),
        Some(("/h/.rbenv/versions/3.3.0/bin/ruby-lsp", true)),
        "a program found keeps its server's arguments"
    );
    // Each one's row chooses its own.
    type Pick = (
        &'static str,
        &'static str,
        fn(&LanguageChoices) -> Option<String>,
        &'static str,
    );
    let picks: [Pick; 5] = [
        ("java", "JDK 21", |c| c.java.clone(), "/usr/lib/jvm/java-21"),
        (
            "php",
            "PHP 8.2 — composer.json's floor",
            |c| c.php.clone(),
            "8.2",
        ),
        (
            "csharp",
            "src/App.sln",
            |c| c.csharp.clone(),
            "/p/src/App.sln",
        ),
        (
            "erlang",
            "elp in /opt/elp/bin",
            |c| Some(c.servers.as_ref()?.get("erlang")?.command.clone()),
            "/opt/elp/bin/elp",
        ),
        (
            "haskell",
            "ghcup",
            |c| Some(c.servers.as_ref()?.get("haskell")?.command.clone()),
            "/h/.ghcup/bin/haskell-language-server-wrapper",
        ),
    ];
    for (language, row_label, get, want) in picks {
        app.proj.project_languages = vec![language.to_string()];
        // The project's code is in them, as a scan finds it.
        app.proj.code_languages = app.proj.project_languages.clone();
        let mut sim = sim_tall(super::connect::languages_section(&app));
        let _ = sim.click(row_label);
        let chosen: Vec<LanguageChoices> = sim
            .into_messages()
            .filter_map(|m| match m {
                Message::Languages(crate::LanguagesMsg::Choose(next)) => Some(*next),
                _ => None,
            })
            .collect();
        assert_eq!(chosen.len(), 1, "{language}");
        assert_eq!(get(&chosen[0]).as_deref(), Some(want), "{language}");
    }
    app.proj.project_languages = ["java", "php", "csharp", "ruby", "erlang", "haskell"]
        .map(String::from)
        .to_vec();
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();

    app.proj.lang_choices = LanguageChoices {
        php: Some("8.2".into()),
        servers: Some(
            [(
                "erlang".to_string(),
                clew_protocol::ServerChoice {
                    command: "/opt/elp/bin/elp".into(),
                    args: None,
                },
            )]
            .into(),
        ),
        ..Default::default()
    };
    // The ELP chosen is one of those found, not a program of the user's
    // own: no row of its own for it.
    let mut sim = sim_tall(super::connect::languages_section(&app));
    assert!(sim.find("elp in /opt/elp/bin").is_ok());
    assert!(
        sim.find("Your own program").is_err(),
        "the chosen ELP is the one found"
    );
}

/// A server program of the user's own, with arguments of their own, is the
/// one chosen; pressing its row again keeps it as it is. It used to choose
/// the bare program, its arguments dropped for clew's defaults, and the
/// server restarted with those.
#[test]
fn pressing_your_own_program_again_keeps_its_arguments() {
    use clew_protocol::{LanguageChoices, LanguageOptions, ServerChoice, ToolchainCandidate};
    let mut app = reader_app();
    app.proj.project_languages = vec!["lean".into()];
    // The project's code is in them, as a scan finds it.
    app.proj.code_languages = app.proj.project_languages.clone();
    app.proj.lang_options = Some(Ok(LanguageOptions {
        toolchains: [(
            "lean".to_string(),
            vec![ToolchainCandidate {
                command: "/opt/lake".into(),
                name: "elan".into(),
                project: false,
            }],
        )]
        .into(),
        ..Default::default()
    }));
    let own = ServerChoice {
        command: "/opt/lake".into(),
        args: Some(vec!["serve".into(), "--foo".into()]),
    };
    app.proj.lang_choices = LanguageChoices {
        servers: Some([("lean".to_string(), own.clone())].into()),
        ..Default::default()
    };
    let chosen: Vec<LanguageChoices> = click(
        sim_elem(super::connect::languages_section(&app)),
        "Your own program",
    )
    .into_iter()
    .filter_map(|m| match m {
        Message::Languages(crate::LanguagesMsg::Choose(next)) => Some(*next),
        _ => None,
    })
    .collect();
    assert_eq!(chosen.len(), 1, "{chosen:?}");
    assert_eq!(
        chosen[0].servers.as_ref().and_then(|s| s.get("lean")),
        Some(&own),
        "the arguments were dropped"
    );
}

/// Explain All with no AI key opens Settings on the page that takes one —
/// from the ⋯ menu and from its shortcut alike — not on whichever page
/// Settings showed last.
#[test]
fn explain_all_without_a_key_opens_settings_where_the_key_goes() {
    let mut app = reader_app();
    app.llm_available = false;
    open_tools_menu(&mut app);
    let sent = click(sim_of(&app), "Explain All");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Settings(crate::SettingsMsg::NeedKey))),
        "{sent:?}"
    );
    let mut app = reader_app();
    app.llm_available = false;
    app.settings.page = crate::SettingsPage::Shortcuts;
    let _ = app.run_command_action(crate::keymap::Action::ExplainAll);
    assert!(app.settings.open);
    assert_eq!(app.settings.page, crate::SettingsPage::LanguageModel);
}

/// The status bar's target picker is there for a file whose dimming the
/// platform decides — Rust's, Go's, Haskell's — and not for one whose does
/// not (C#'s, Erlang's: their files' own symbols decide).
#[test]
fn the_target_picker_shows_for_languages_the_platform_dims() {
    let mut app = reader_app();
    let label = format!(
        "Reading for {}",
        super::statusbar::target_name(&app.proj.reading_target)
    );
    for (lang, shown) in [
        ("rust", true),
        ("go", true),
        ("zig", true),
        ("haskell", true),
        ("csharp", false),
        ("erlang", false),
        ("python", false),
    ] {
        app.proj.panes[0].as_mut().unwrap().lang_key = Some(lang);
        let mut sim = sim_elem(super::statusbar::statusbar(&app));
        assert_eq!(sim.find(label.as_str()).is_ok(), shown, "{lang}");
    }
}

/// A file whose language has its own action gets it in the context menu,
/// under the go-tos; one whose language has none does not.
#[test]
fn the_context_menu_offers_the_languages_own_action() {
    let mut app = reader_app();
    let menu = |insight| crate::ContextMenu {
        pane: 0,
        line: 0,
        col: 0,
        x: 10.0,
        y: 10.0,
        insight,
        served: true,
        by_pointer: true,
    };
    app.proj.context_menu = Some(menu(Some(crate::InsightKind::ExpandMacro)));
    assert!(shows(&mut sim_of(&app), "Expand Macro"));
    let sent = click(sim_of(&app), "Expand Macro");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Hover(HoverMsg::Insight))),
        "{sent:?}"
    );
    app.proj.context_menu = Some(menu(Some(crate::InsightKind::GoalState)));
    assert!(shows(&mut sim_of(&app), "Show Goal"));
    app.proj.context_menu = Some(menu(None));
    assert!(shows(&mut sim_of(&app), "Go to Definition"));
    for absent in ["Expand Macro", "Show Goal", "Show Type"] {
        assert!(!shows(&mut sim_of(&app), absent), "{absent}");
    }
}

/// The popup waits with "Asking…", then shows the answer; Open Definition
/// is there only when the answer points somewhere, and Escape closes it.
#[test]
fn the_insight_popup_shows_the_answer_and_where_it_points() {
    let mut app = reader_app();
    app.proj.insight = Some(crate::Insight {
        anchor: None,
        token: 1,
        kind: crate::InsightKind::ExpandMacro,
        title: "Expansion of MAX".into(),
        loading: true,
        prepared: Vec::new(),
        defined: None,
    });
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Expansion of MAX"));
        assert!(shows(&mut sim, "Asking…"));
    }
    assert!(!shows(&mut sim_of(&app), "Open Definition"));
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Hover(HoverMsg::InsightClose))
    ));
    let answer = crate::app::insight::c_answer(clew_protocol::MacroExpansion {
        name: "MAX".into(),
        invocation: "MAX(1, 2)".into(),
        expanded: "((1) > (2) ? (1) : (2))".into(),
        file: "include/util.h".into(),
        line: 1,
        definition: "#define MAX(a, b) ((a) > (b) ? (a) : (b))".into(),
        truncated: false,
    });
    let (prepared, _) = app.prepare_segments(&answer.markdown);
    // The answer's code blocks, highlighted as the editor highlights C:
    // each block's lines, spans joined.
    let blocks: Vec<String> = prepared
        .iter()
        .filter_map(|seg| match seg {
            crate::PreparedSeg::Code(lines) => Some(
                lines
                    .iter()
                    .map(|l| l.spans.iter().map(|(t, _)| t.as_str()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect();
    assert_eq!(
        blocks,
        [
            "MAX(1, 2)",
            "((1) > (2) ? (1) : (2))",
            "#define MAX(a, b) ((a) > (b) ? (a) : (b))"
        ]
    );
    if let Some(open) = app.proj.insight.as_mut() {
        open.loading = false;
        open.prepared = prepared;
        open.defined = answer.defined;
    }
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "Asking…"));
    }
    let sent = click(sim_of(&app), "Open Definition");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Hover(HoverMsg::InsightOpenDefinition))),
        "{sent:?}"
    );
    let sent = click(sim_of(&app), "Close");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Hover(HoverMsg::InsightClose))),
        "{sent:?}"
    );
}

/// The margin's notes sit level with the functions they are about: a
/// note's name is drawn on its function's first line, and moves with the
/// code as it scrolls.
#[test]
fn a_margin_note_sits_level_with_its_function() {
    let mut app = reader_app();
    app.show_right_panel = true;
    app.proj.explain.view = None;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.explain.cache.insert(
        crate::explain::Node::Function {
            file: abs,
            name: "origin".into(),
            ordinal: 0,
        },
        crate::explain::Cached {
            summary: "Makes the point at zero.".into(),
            prompt_hash: crate::incremental::content_hash(b"t"),
            detail: None,
            basis: None,
        },
    );
    // `origin` runs from the file's third line to its fifth.
    app.proj.panes[0].as_mut().unwrap().symbols = vec![crate::outline::Symbol {
        name: "origin".into(),
        kind: "function".into(),
        line: 3,
        end_line: 5,
    }];
    let lh = app.line_height();
    let line_top = |app: &App| {
        let v = app.active_viewer().unwrap();
        super::body_top(app) + super::sheet_header_height(app) + 1.0 + v.row_of(2) as f32 * lh
            - v.scroll_y
    };
    let note_y = |app: &App| {
        let mut sim = sim_of(app);
        assert!(
            shows(&mut sim, "Makes the point at zero."),
            "the note says it"
        );
        text_at(&mut sim, |t| t == "origin", 0)
            .expect("the note's name")
            .y
    };
    let y = note_y(&app);
    assert!(
        (y - line_top(&app)).abs() < lh,
        "{y} level with {}",
        line_top(&app)
    );
    // Scrolled a line, the note goes with its function.
    app.proj.panes[0].as_mut().unwrap().scroll_y = lh;
    let scrolled = note_y(&app);
    assert!(
        (scrolled - (y - lh)).abs() < 1.0,
        "{scrolled} moved a line from {y}"
    );
}

/// The raised note keeps clear of the Ask pill floating over the margin's
/// foot: low in the view, its references, date and Ask went under the pill.
#[test]
fn the_raised_note_keeps_clear_of_the_ask_pill() {
    let mut app = reader_app();
    app.show_right_panel = true;
    assert_eq!(app.ask_view, crate::AskView::Hidden, "the pill shows");
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.explain.view = Some(crate::explain::Node::Function {
        file: abs,
        name: "origin".into(),
        ordinal: 0,
    });
    let mut seen_low = false;
    for line in (4..60).step_by(2) {
        app.proj.panes[0].as_mut().unwrap().symbols = vec![crate::outline::Symbol {
            name: "origin".into(),
            kind: "function".into(),
            line,
            end_line: line + 2,
        }];
        let mut sim = sim_of(&app);
        let asks: Vec<iced::Rectangle> = (0..3)
            .filter_map(|n| text_at(&mut sim, |t| t == "Ask", n))
            .collect();
        if asks.len() < 2 {
            continue; // the note is out of view
        }
        let (note, pill) = (asks[0], asks[1]);
        seen_low |= note.y > app.window_height * 0.6;
        // The pill's box, around its label: the height it is drawn at.
        let pill_h = crate::ui::askfloat::PILL_H;
        let pill_top = pill.center_y() - pill_h / 2.0;
        assert!(
            note.y + note.height <= pill_top,
            "line {line}: the note's Ask {note:?} is under the pill from {pill_top}"
        );
    }
    assert!(seen_low, "the note was never brought low in the view");
}

/// While the diff or Time Travel stands in the code's place, the note the
/// caret is in heads the margin — where it is with its function at the top
/// of the code — and no other note is laid beside lines that are not on
/// screen. Each was placed by the hidden code's scroll: the raised note a
/// banner lower than without the diff, the rest beside whatever of the
/// diff was there.
#[test]
fn a_diff_in_the_codes_place_leaves_the_margin_its_raised_note_alone() {
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    let function = |name: &str, line: usize| crate::outline::Symbol {
        name: name.into(),
        kind: "function".into(),
        line,
        end_line: line + 2,
    };
    // `origin` heads the file and has the caret; `other`, further down,
    // is explained.
    app.proj.panes[0].as_mut().unwrap().symbols =
        vec![function("origin", 1), function("other", 10)];
    let node = |name: &str| crate::explain::Node::Function {
        file: abs.clone(),
        name: name.into(),
        ordinal: 0,
    };
    app.proj.explain.view = Some(node("origin"));
    app.proj.explain.cache.insert(
        node("other"),
        crate::explain::Cached {
            summary: "Does the other thing.".into(),
            prompt_hash: crate::incremental::content_hash(b"t"),
            detail: None,
            basis: None,
        },
    );
    let margin = |app: &App| {
        let mut sim = sim_of(app);
        let raised = text_at(&mut sim, |t| t == "origin", 0).map(|r| r.y);
        (raised, shows(&mut sim, "Does the other thing."))
    };
    let (beside_code, other_shown) = margin(&app);
    let beside_code = beside_code.expect("the raised note beside the code");
    assert!(other_shown, "beside the code, the other note shows");

    app.proj.diff = Some(crate::DiffState::new(
        abs.clone(),
        "src/lib.rs".into(),
        Vec::new(),
    ));
    let (beside_diff, other_shown) = margin(&app);
    let beside_diff = beside_diff.expect("the raised note beside the diff");
    assert!(
        (beside_diff - beside_code).abs() < 1.0,
        "beside the diff at {beside_diff}, beside the code at {beside_code}"
    );
    assert!(!other_shown, "a note beside the diff's lines");
    app.proj.diff = None;

    let hv = app.proj.panes[0].clone().unwrap();
    app.proj.time_travel = Some(crate::TimeTravel {
        abs: abs.clone(),
        rel: "src/lib.rs".into(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: Vec::new(),
        idx: 0,
        viewer: Some(hv),
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: false,
        generation: 0,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    let (in_time_travel, other_shown) = margin(&app);
    let in_time_travel = in_time_travel.expect("the raised note in Time Travel");
    assert!(
        (in_time_travel - beside_code).abs() < 1.0,
        "in Time Travel at {in_time_travel}, beside the code at {beside_code}"
    );
    assert!(!other_shown, "a note beside Time Travel's lines");
}

/// The status bar and the tool strip paint no fill of their own: drawn
/// after the sheet, theirs cut the sheet's shadow off in a hard line —
/// under the sheet's rounded bottom corners, and down its right edge. The
/// window's fill, under everything, is their background.
#[test]
fn the_chrome_drawn_after_the_sheet_leaves_its_shadow_whole() {
    let theme = crate::theme::app_theme();
    assert!(crate::theme::sheet(&theme).shadow.blur_radius > 0.0);
    assert!(crate::theme::statusbar(&theme).background.is_none());
    assert!(crate::theme::strip(&theme).background.is_none());
    assert!(crate::theme::chrome(&theme).background.is_some());
}

/// The explanation in blocks has the margin to be read in: the raised card
/// reaches down it. In the summary's 193 pixels it showed three lines of the
/// first block, the last cut through, the rest only by scrolling a window
/// that size.
#[test]
fn the_explanation_in_blocks_has_the_margin_to_be_read_in() {
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.panes[0].as_mut().unwrap().symbols = vec![crate::outline::Symbol {
        name: "origin".into(),
        kind: "function".into(),
        line: 1,
        end_line: 3,
    }];
    let node = crate::explain::Node::Function {
        file: abs,
        name: "origin".into(),
        ordinal: 0,
    };
    app.proj.explain.cache.insert(
        node.clone(),
        crate::explain::Cached {
            summary: "Makes the point at zero.".into(),
            prompt_hash: crate::incremental::content_hash(b"t"),
            detail: Some(
                "Names the point it makes.\n\n\
                 Sets its x to zero.\n\n\
                 Sets its y to zero.\n\n\
                 Checks the two agree.\n\n\
                 Hands the point back."
                    .into(),
            ),
            basis: None,
        },
    );
    // Where the card ends: its actions' row, the lowest "Calls" (the
    // other is the margin's tab).
    let card_foot = |app: &App| {
        let mut sim = sim_of(app);
        (0..)
            .map_while(|nth| text_at(&mut sim, |t| t == "Calls", nth))
            .map(|r| r.y)
            .fold(f32::MIN, f32::max)
    };
    let _ = app.show_explanation(node.clone());
    assert!(!app.proj.explain.showing_detail);
    let summary_foot = card_foot(&app);
    let _ = app.update(Message::Explain(crate::ExplainMsg::ToggleBlocks));
    assert!(app.proj.explain.showing_detail);
    let said = format!("{:?}", app.proj.explain.prepared);
    assert!(said.contains("Hands the point back."), "{said}");
    let blocks_foot = card_foot(&app);
    assert!(
        blocks_foot - summary_foot > 200.0,
        "the card in blocks ends at {blocks_foot}, in summary at {summary_foot}"
    );
    assert!(
        blocks_foot < app.window_height,
        "its actions off the window at {blocks_foot}"
    );
}

/// The raised card holds the whole of a function's explanation, where it
/// runs past four lines: those were shown, and the rest scrolled out of a
/// window that size, cut through a line. A short one keeps the card's own
/// height.
#[test]
fn the_raised_card_holds_a_long_explanation_whole() {
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.panes[0].as_mut().unwrap().symbols = vec![crate::outline::Symbol {
        name: "origin".into(),
        kind: "function".into(),
        line: 1,
        end_line: 3,
    }];
    let node = crate::explain::Node::Function {
        file: abs,
        name: "origin".into(),
        ordinal: 0,
    };
    let card_foot = |app: &mut App, summary: &str| {
        app.proj.explain.cache.insert(
            node.clone(),
            crate::explain::Cached {
                summary: summary.into(),
                prompt_hash: crate::incremental::content_hash(b"t"),
                detail: None,
                basis: None,
            },
        );
        let _ = app.show_explanation(node.clone());
        let mut sim = sim_of(app);
        (0..)
            .map_while(|nth| text_at(&mut sim, |t| t == "Calls", nth))
            .map(|r| r.y)
            .fold(f32::MIN, f32::max)
    };
    let short = card_foot(&mut app, "Makes the point at zero.");
    let shorter = card_foot(&mut app, "Zero.");
    assert!((short - shorter).abs() < 1.0, "{short} and {shorter}");
    let long = card_foot(
        &mut app,
        "Makes the point at zero, the origin every other point is measured \
         from. It builds the point from two zero coordinates rather than \
         asking for them, so a caller never passes the wrong ones. The \
         layout code calls it once per frame to anchor the grid, and the \
         tests call it to check that distances from it are the lengths \
         they expect. Nothing in it can fail, and it allocates nothing.",
    );
    assert!(
        long - short > 60.0,
        "the long card ends at {long}, the short one at {short}"
    );
    assert!(
        long < app.window_height,
        "its actions off the window at {long}"
    );
}

/// Escape in the Files view's filter empties it, as its × does: it stayed
/// filled however often Escape was pressed. Only while the filter has the
/// keyboard: Escape elsewhere leaves it be.
#[test]
fn escape_in_the_file_filter_clears_it() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.file_filter = "cart".into();
    let escaped = |m: &Message| matches!(m, Message::Window(WindowMsg::FileFilterEscaped));
    let cleared =
        |m: &Message| matches!(m, Message::Window(WindowMsg::FileFilterChanged(s)) if s.is_empty());
    let escape = Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(keyboard::key::Named::Escape),
        modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::default(),
        text: None,
        repeat: false,
    });
    let press = |focus: bool| {
        let mut renderer = renderer();
        let size = Size::new(300.0, 600.0);
        let mut ui = UserInterface::build(
            super::sidebar::sidebar(&app),
            size,
            Cache::default(),
            &mut renderer,
        );
        if focus {
            let _ = run_operation::<()>(
                &mut ui,
                &renderer,
                iced::advanced::widget::operation::focusable::focus(super::file_filter_id()),
            );
        }
        let mut messages = Vec::new();
        let _ = ui.update(
            std::slice::from_ref(&escape),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    };
    assert!(press(true).iter().any(escaped), "Escape in the filter");
    assert!(
        !press(false).iter().any(escaped),
        "Escape with it unfocused"
    );
    // Its ×, just right of the field, does the same.
    let mut sim = sim_elem(super::sidebar::sidebar(&app));
    let field = sim
        .find(super::file_filter_id())
        .expect("the filter is on screen")
        .bounds();
    sim.point_at(Point::new(field.x + field.width + 12.0, field.center_y()));
    let _ = sim.simulate(iced_test::simulator::click());
    let sent: Vec<Message> = sim.into_messages().collect();
    assert!(sent.iter().any(cleared), "{sent:?}");
}

/// The Files filter's well lights its edge while the filter has the
/// keyboard, as every field's does: reached by F6, it showed only a caret.
#[test]
fn the_files_filter_lights_its_edge_while_it_has_the_keyboard() {
    use iced::advanced::renderer::Headless as _;
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    let size = Size::new(300.0, 600.0);
    let field = sim_elem(super::sidebar::sidebar(&app))
        .find(super::file_filter_id())
        .expect("the filter is on screen")
        .bounds();
    let accent = crate::theme::accent();
    // How near the well's top edge, over the field's middle, comes to the
    // accent: the least distance of any of its pixels.
    let nearest = |focus: bool| {
        let mut renderer = renderer();
        let mut ui = UserInterface::build(
            super::sidebar::sidebar(&app),
            size,
            Cache::default(),
            &mut renderer,
        );
        if focus {
            let _ = run_operation::<()>(
                &mut ui,
                &renderer,
                iced::advanced::widget::operation::focusable::focus(super::file_filter_id()),
            );
        }
        let _ = ui.update(
            &[Event::Window(window::Event::RedrawRequested(
                std::time::Instant::now(),
            ))],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut Vec::new(),
        );
        ui.draw(
            &mut renderer,
            &crate::theme::app_theme(),
            &iced::advanced::renderer::Style {
                text_color: iced::Color::BLACK,
            },
            mouse::Cursor::Unavailable,
        );
        let rgba = renderer.screenshot(
            iced::Size::new(size.width as u32, size.height as u32),
            1.0,
            iced::Color::TRANSPARENT,
        );
        let x = field.center_x() as usize;
        (field.y as usize - 10..field.y as usize)
            .map(|y| {
                let i = (y * size.width as usize + x) * 4;
                let d = |c: u8, a: f32| (f32::from(c) / 255.0 - a).abs();
                d(rgba[i], accent.r) + d(rgba[i + 1], accent.g) + d(rgba[i + 2], accent.b)
            })
            .fold(f32::MAX, f32::min)
    };
    let (rest, lit) = (nearest(false), nearest(true));
    assert!(lit + 0.2 < rest, "lit {lit:.2}, at rest {rest:.2}");
}

/// A commit's day reads as a reader says it: the month and day, and the
/// year when it is not this one.
#[test]
fn a_commit_day_reads_as_month_and_day() {
    let now = 1_793_793_600; // 2026-11-04T12:00Z
    assert_eq!(super::short_date(now, now), "Nov 4");
    assert_eq!(super::short_date(1_767_225_600, now), "Jan 1"); // 2026-01-01
    assert_eq!(super::short_date(1_709_251_199, now), "Feb 29, 2024");
    assert_eq!(super::short_date(946_598_400, now), "Dec 31, 1999");
}

/// The note on what the caret is in says when its function last changed,
/// from the file's blame — the newest commit of its lines, or that one of
/// them is not committed yet — and how many functions call it.
#[test]
fn the_raised_note_says_when_its_function_changed() {
    use crate::git::{BlameLine, GitInfo};
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    let node = crate::explain::Node::Function {
        file: abs,
        name: "origin".into(),
        ordinal: 0,
    };
    app.proj.explain.view = Some(node);
    let blame = |time: i64, uncommitted: bool| BlameLine {
        commit: "c".into(),
        author: "a".into(),
        time,
        summary: "s".into(),
        uncommitted,
    };
    let set = |app: &mut App, lines: Vec<BlameLine>| {
        let v = app.proj.panes[0].as_mut().unwrap();
        v.symbols = vec![crate::outline::Symbol {
            name: "origin".into(),
            kind: "function".into(),
            line: 3,
            end_line: 5,
        }];
        v.git = Some(std::sync::Arc::new(GitInfo {
            blame: lines,
            status: Vec::new(),
            deleted_at: Default::default(),
        }));
    };
    // Lines 3–5 are `origin`; line 1's newer commit is not its.
    set(
        &mut app,
        vec![
            blame(1_793_793_600, false),
            blame(1, false),
            blame(946_598_400, false),
            blame(1_709_251_199, false),
            blame(946_598_400, false),
        ],
    );
    assert!(shows(&mut sim_of(&app), "Feb 29, 2024"));
    // A line of it edited since: its commits still date it — "Uncommitted"
    // read as if it were new.
    set(
        &mut app,
        vec![
            blame(1_793_793_600, false),
            blame(1, false),
            blame(946_598_400, true),
            blame(1_709_251_199, false),
            blame(946_598_400, false),
        ],
    );
    assert!(shows(&mut sim_of(&app), "Feb 29, 2024"));
    // None of it committed yet: said so.
    set(
        &mut app,
        vec![
            blame(1_793_793_600, false),
            blame(1, false),
            blame(946_598_400, true),
            blame(1_709_251_199, true),
            blame(946_598_400, true),
        ],
    );
    assert!(shows(&mut sim_of(&app), "Uncommitted"));
    // How many functions call it, from the project's call graph.
    assert!(shows(&mut sim_of(&app), "Calls"), "before the graph has it");
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let call =
        |name: &str, callers: Vec<usize>, callees: Vec<usize>| clew_protocol::CallGraphNode {
            name: name.into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 1,
            callers,
            callees,
        };
    app.proj.project_calls.graph = std::sync::Arc::new(
        crate::projectcalls::ProjectCallGraph::from_wire(
            clew_protocol::CallGraph {
                nodes: vec![
                    call("origin", vec![1, 2], vec![]),
                    call("a", vec![], vec![0]),
                    call("b", vec![], vec![0]),
                ],
            },
            |rel| root.join(rel),
        )
        .expect("a call graph"),
    );
    // Their call sites unknown (a remote project's graph comes without
    // them): the functions that call it.
    assert!(shows(&mut sim_of(&app), "2 callers"));
    // Known: each place that calls it — `a` twice, `b` once — and not a
    // call of another name.
    let site = |caller: &str, callee: &str, line| crate::projectcalls::CallSite {
        caller: Some(caller.into()),
        callee: callee.into(),
        method: false,
        receiver: None,
        line,
    };
    let lib = root.join("src/lib.rs");
    app.proj.calls_by_file.insert(
        lib.clone(),
        std::sync::Arc::new(crate::projectcalls::FileCalls::new(
            lib,
            crate::highlight::Lang::Rust,
            vec![
                site("a", "origin", 7),
                site("a", "origin", 8),
                site("b", "origin", 11),
                site("b", "other", 12),
            ],
            &[],
            Default::default(),
        )),
    );
    // Callers, as the Calls tab lists them, and the calls when one caller
    // makes more than one: `a` calls it twice.
    assert!(shows(&mut sim_of(&app), "2 callers, 3 calls"));
    // A call at the top of its file, in no function (Python's `if __name__
    // == "__main__": main()`), is a reference too: such a `main` read "0
    // references". A method call of that name is not one.
    let lib = root.join("src/lib.rs");
    let top = |callee: &str, method, line| crate::projectcalls::CallSite {
        caller: None,
        callee: callee.into(),
        method,
        receiver: None,
        line,
    };
    app.proj.calls_by_file.insert(
        lib.clone(),
        std::sync::Arc::new(crate::projectcalls::FileCalls::new(
            lib,
            crate::highlight::Lang::Rust,
            vec![
                site("a", "origin", 7),
                site("a", "origin", 8),
                site("b", "origin", 11),
                top("origin", false, 20),
                top("origin", true, 21),
            ],
            &[],
            Default::default(),
        )),
    );
    assert!(shows(&mut sim_of(&app), "2 callers, 4 calls"));
}

/// The margin, with no note to show, says why, centered under a muted icon
/// rather than as a line of text in a corner.
#[test]
fn the_right_panels_empty_sections_are_centered_notes() {
    let mut app = reader_app();
    app.show_right_panel = true;
    app.proj.explain.view = None;
    let mut sim = sim_of(&app);
    let hint = text_at(
        &mut sim,
        |t| t.contains("Move the cursor into a function"),
        0,
    )
    .expect("the explain hint");
    // Centered in the margin, well in from its left edge.
    let panel_left = WINDOW.width - super::STRIP_W - super::SHEET_GAP - app.right_width;
    let center = hint.x + hint.width / 2.0;
    let panel_center = panel_left + app.right_width / 2.0;
    assert!(
        (center - panel_center).abs() < 12.0,
        "{hint:?} centered in the panel"
    );
}

/// An empty Ask shows what to ask, not a paragraph about asking; the hint
/// that answers have nothing to stand on yet is a quiet line, and only for
/// a project open with no server to run the agent, no index, nothing pinned.
#[test]
fn an_empty_ask_shows_questions_not_instructions() {
    let mut app = reader_app();
    {
        let mut sim = sim_elem(super::ask_panel(&app, 560.0));
        assert!(!shows(&mut sim, "Ask a question about this codebase"));
        assert!(!shows(&mut sim, "Run “Explain All”"));
        assert!(shows(&mut sim, "Answers draw on the open file"));
    }
    // A question that could not be asked says why there.
    app.proj.ask_notice = Some("Open a file to ask about it".into());
    {
        let mut sim = sim_elem(super::ask_panel(&app, 560.0));
        assert!(shows(&mut sim, "Open a file to ask about it"));
        assert!(!shows(&mut sim, "Answers draw on the open file"));
    }
    app.proj.ask_notice = None;
    app.proj.project = None;
    let mut sim = sim_elem(super::ask_panel(&app, 560.0));
    assert!(!shows(&mut sim, "Answers draw on the open file"));
}

/// The palette's rows show each command and its keys, a cap per key; a
/// click sends the command it shows, by its title.
#[test]
fn the_palette_lists_commands_with_their_keys() {
    let mut app = reader_app();
    app.proj.finder.open = true;
    app.proj.finder.query = ">call".into();
    app.refresh_palette();
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Call Graph"));
        // Its keys, a cap each.
        let keys = super::split_caps(&crate::keymap::local_keys("⇧⌘C"));
        assert!(keys.len() > 1, "{keys:?}");
        for key in keys {
            assert!(shows(&mut sim, &key), "the {key} cap");
        }
    }
    let sent = click(sim_of(&app), "Call Graph");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Nav(NavMsg::PaletteRun(t)) if t == "Call Graph")),
        "{sent:?}"
    );
    app.proj.finder.query = ">nothing like this".into();
    app.refresh_palette();
    assert!(shows(&mut sim_of(&app), "No matching command"));
}

/// Where Ask's pill and card stand, in window coordinates: their right
/// edge, left of the tool strip, and their bottom edge, over the status bar.
fn ask_corner_edges() -> (f32, f32) {
    (
        WINDOW.width - super::STRIP_W - 12.0,
        WINDOW.height - super::STATUSBAR_H - super::askfloat::ASK_MARGIN,
    )
}

/// The floating Ask, on screen: the field at the top searches, and the
/// pill in the corner opens Ask; the card stands in that corner, its
/// conversation in it, the code above and beside it taking its own clicks;
/// Esc hides it to the pill, and put away the pill goes too; the large
/// window's dimmed rest makes it a card again when clicked.
/// A question to try names code as code (`total()` in the code font, not
/// "How does total handle…" run into the prose), and a click still asks
/// it, backticks and all, as the model wrote it.
#[test]
fn a_question_to_try_sets_its_names_as_code() {
    let mut app = reader_app();
    app.llm_available = false;
    let q = "What does `total()` return?".to_string();
    app.proj
        .suggestions
        .insert(app.suggest_key(), vec![q.clone()]);
    let _ = app.update(Message::Ask(AskMsg::Open));
    let mut sim = sim_of(&app);
    let caption = text_at(&mut sim, |t| t == "Try asking", 0).expect("Try asking");
    assert!(!shows(&mut sim, "`"), "a backtick is shown to the reader");
    // Its name in the code font, its backticks not drawn (rich text is not
    // seen by the simulator, so the pieces are checked as they are made); a
    // lone backtick sets nothing apart.
    assert_eq!(
        super::panels::question_pieces(&q),
        vec![
            ("What does ".to_string(), false),
            ("total()".to_string(), true),
            (" return?".to_string(), false),
        ]
    );
    assert_eq!(
        super::panels::question_pieces("Why `x?"),
        vec![("Why x?".to_string(), false)]
    );
    let at = Point::new(caption.x + 24.0, caption.y + caption.height + 14.0);
    let mut sim = sim_of(&app);
    sim.point_at(at);
    let _ = sim.simulate(iced_test::simulator::click());
    let sent = sent_by(sim);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Suggested(s)) if *s == q)),
        "the question is not asked: {sent:?}"
    );
}

#[test]
fn the_floating_ask_is_driven_from_the_screen() {
    let mut app = reader_app();
    assert_eq!(app.ask_view, crate::AskView::Hidden, "the pill, at first");
    // The field at the top is search: it opens the finder.
    assert!(matches!(
        click(sim_of(&app), "Search files, symbols and commands").as_slice(),
        [Message::Nav(crate::NavMsg::FinderOpened(
            crate::session::finder::FinderMode::Files
        ))]
    ));
    // The pill in the corner opens Ask.
    {
        let mut sim = sim_of(&app);
        let pill = text_at(&mut sim, |t| t == "Ask", 0).expect("the pill says Ask");
        let (right, bottom) = ask_corner_edges();
        assert!(
            pill.x > right - 200.0 && pill.x < right && pill.y > bottom - 60.0,
            "in the corner: {pill:?}"
        );
    }
    let sent = click(sim_of(&app), "Ask");
    assert!(
        sent.iter().any(|m| matches!(m, Message::Ask(AskMsg::Open))),
        "{sent:?}"
    );
    // A card in the corner, with a conversation in it.
    app.ask_view = crate::AskView::Compact;
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 1,
        question: "where is the origin?".into(),
        ..Default::default()
    });
    {
        let mut sim = sim_of(&app);
        let question = text_at(&mut sim, |t| t == "where is the origin?", 0)
            .expect("the conversation is on the card");
        let place = super::ask_place(&app).unwrap();
        let (right, _) = ask_corner_edges();
        assert!(
            (place.left + place.width - right).abs() < 1.0,
            "the card's right edge by the tool strip: {place:?}"
        );
        assert!(
            question.x >= place.left && question.x < place.left + place.width,
            "on the card: {question:?} in {place:?}"
        );
        assert!(
            !shows(&mut sim, "Keep in the margin"),
            "Ask is the whole project's: it is no tab of a file's margin"
        );
    }
    // Is Ask's pill in its corner?
    let pill_shown = |app: &App| {
        let (right, bottom) = ask_corner_edges();
        text_at(&mut sim_of(app), |t| t == "Ask", 0)
            .is_some_and(|at| at.x > right - 200.0 && at.x < right && at.y > bottom - 60.0)
    };
    assert!(!pill_shown(&app), "no pill while the card is open");
    // The code beside the card takes its own clicks.
    let mut sim = sim_of(&app);
    let beside = super::ask_place(&app).unwrap().left - 60.0;
    sim.point_at(iced::Point::new(beside, 300.0));
    let _ = sim.simulate(iced_test::simulator::click());
    let sent = sent_by(sim);
    assert!(
        !sent.iter().any(|m| matches!(m, Message::Ask(_))),
        "{sent:?}"
    );
    // Esc hides it to the pill — which says Ask, and no count of its
    // answers beside it.
    assert!(app.step_ask_down());
    assert_eq!(app.ask_view, crate::AskView::Hidden);
    assert!(super::ask_place(&app).is_none());
    assert!(pill_shown(&app));
    assert!(!shows(&mut sim_of(&app), "1 answer"));
    // Closed with its ×, it is back on its pill — still there to click.
    // Put away with nothing in the corner, Ask had gone, with no way back
    // to it on screen.
    let _ = app.update(Message::Ask(AskMsg::Open));
    let _ = app.update(Message::Ask(AskMsg::Close));
    assert_eq!(app.ask_view, crate::AskView::Hidden);
    assert!(pill_shown(&app));
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    assert_eq!(app.ask_view, crate::AskView::Compact);
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    assert_eq!(
        app.ask_view,
        crate::AskView::Hidden,
        "the shortcut hides it"
    );
    // Large: centred, and a click on the dimmed rest makes it a card.
    app.ask_view = crate::AskView::Expanded;
    {
        let mut sim = sim_of(&app);
        // The first is the card's name — the session is named for its
        // first question — the second the question's bubble.
        let question = text_at(&mut sim, |t| t == "where is the origin?", 1).unwrap();
        let place = super::ask_place(&app).unwrap();
        // The card is centred; the question, a bubble, sits at its right.
        let left = (WINDOW.width - place.width) / 2.0;
        assert!(
            (place.left - left).abs() < 1.0,
            "centred: {place:?}, from {left}"
        );
        assert!(
            question.x > left + place.width / 2.0
                && question.x + question.width < left + place.width,
            "the question at the card's right: {question:?} in {place:?}"
        );
    }
    let mut sim = sim_of(&app);
    sim.point_at(iced::Point::new(20.0, 400.0));
    let _ = sim.simulate(iced_test::simulator::click());
    assert!(matches!(
        sent_by(sim).as_slice(),
        [Message::Ask(AskMsg::Shrink)]
    ));
}

/// The card is named for its session; its name opens the menu of
/// sessions, which offers a new one and lists this project's, the one on
/// screen marked, each saying when it was last asked in; a pick goes back to
/// it, and + on the card starts a new one.
#[test]
fn the_card_names_its_session_and_lists_the_others() {
    use crate::app::ask_sessions::{AskSession, SavedTurn, now};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    let turn = || SavedTurn {
        question: "q".into(),
        spoken: false,
        answer: "a".into(),
    };
    app.proj.ask_sessions = vec![
        AskSession {
            id: 1,
            title: "Where is the origin?".into(),
            updated: now() - 120,
            turns: vec![turn()],
        },
        AskSession {
            id: 2,
            title: "How is a host connected?".into(),
            updated: now() - 7_200,
            turns: vec![turn(), turn()],
        },
    ];
    app.proj.ask_session = Some(1);
    let name_at = |app: &App| text_at(&mut sim_of(app), |t| t == "Where is the origin?", 0);
    let shut = name_at(&app);
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Where is the origin?"), "the card's name");
        assert!(
            !shows(&mut sim, "How is a host connected?"),
            "the menu is shut"
        );
    }
    let sent = click(sim_of(&app), "Where is the origin?");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::SessionsMenu(true)))),
        "{sent:?}"
    );
    app.ask_sessions_menu = true;
    // The menu drops over the window: the card, and its name, stay where
    // they were — the card grew to its full height to hold the menu, and
    // its header jumped up the window from under the pointer.
    assert!(shut.is_some());
    assert_eq!(name_at(&app), shut, "the card's header stays put");
    {
        let mut sim = sim_of(&app);
        // A new session is the header's +, beside the menu: not a second
        // time in it.
        assert!(!shows(&mut sim, "New session"));
        assert!(shows(&mut sim, "How is a host connected?"));
        assert!(shows(&mut sim, "2 min ago, 1 question"));
        assert!(shows(&mut sim, "2 h ago, 2 questions"));
    }
    let sent = click(sim_of(&app), "How is a host connected?");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::SwitchSession(2)))),
        "{sent:?}"
    );
    // Searched: only what has it, with where it is when it is past the name.
    app.proj.ask_sessions[1].turns[1].answer = "Over SSH, with the user's own keys.".into();
    app.ask_sessions_query = "ssh".into();
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "1 session found"));
        assert!(shows(&mut sim, "How is a host connected?"));
        assert!(shows(&mut sim, "Over SSH, with the user's own keys."));
        assert!(
            !shows(&mut sim, "2 min ago, 1 question"),
            "the other is not found"
        );
    }
    app.ask_sessions_query = "kubernetes".into();
    assert!(shows(&mut sim_of(&app), "No sessions match “kubernetes”."));
}

/// A session named in Chinese — about an em a character — is cut to the
/// room the card's header leaves it, as a name in Latin letters is: New,
/// Expand and Close after it all still answer a click. In the menu,
/// the name stops short of the × that forgets it.
#[test]
fn a_long_chinese_session_name_leaves_the_cards_buttons_in_reach() {
    use crate::app::ask_sessions::{AskSession, SavedTurn, now};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    let name = "服务器在哪里把改动过的文件变成它要发送的相对路径以及为什么";
    let session = |id| AskSession {
        id,
        title: name.into(),
        updated: now(),
        turns: vec![SavedTurn {
            question: name.into(),
            spoken: false,
            answer: "a".into(),
        }],
    };
    app.proj.ask_sessions = vec![session(1), session(2)];
    app.proj.ask_session = Some(1);
    let title = text_at(&mut sim_of(&app), |t| t.starts_with("服务器"), 0)
        .expect("the session's name on the card");
    type Sent = fn(&Message) -> bool;
    let wanted: [(&str, Sent); 3] = [
        ("New session", |m| {
            matches!(m, Message::Ask(AskMsg::NewSession))
        }),
        ("Expand", |m| matches!(m, Message::Ask(AskMsg::Expand))),
        ("Close", |m| matches!(m, Message::Ask(AskMsg::Close))),
    ];
    let mut sent = Vec::new();
    for dx in (1..=100).map(|i| i as f32 * 4.0) {
        let mut sim = sim_of(&app);
        sim.point_at(Point::new(title.x + title.width + dx, title.center_y()));
        let _ = sim.simulate(iced_test::simulator::click());
        sent.extend(sent_by(sim));
        if wanted.iter().all(|(_, is)| sent.iter().any(is)) {
            break;
        }
    }
    for (button, is) in wanted {
        assert!(sent.iter().any(is), "{button} is out of reach: {sent:?}");
    }
    // In the menu: cut short of the ×.
    app.ask_sessions_menu = true;
    // (The card is as tall as it may be with its menu open: its name is
    // higher.)
    let name = text_at(&mut sim_of(&app), |t| t.starts_with("服务器"), 0)
        .expect("the session's name on the card");
    let below = name.y + name.height;
    let shown = sim_of(&app)
        .find(move |c: iced_test::selector::Candidate<'_>| match c {
            iced_test::selector::Candidate::Text {
                content,
                visible_bounds: Some(bounds),
                ..
            } if content.starts_with("服务器") && bounds.y > below => Some(content.to_string()),
            _ => None,
        })
        .expect("the session's name in the menu");
    assert!(shown.ends_with('…'), "{shown}");
    assert!(
        super::sheet::text_width(&shown, super::ts::BASE, crate::fonts::UI)
            <= super::askfloat::SESSIONS_MENU_NAME_W,
        "{shown}"
    );
}

/// The menu's search stays while what it holds still filters the menu —
/// the session it found forgotten, the one left not found by it — so it
/// can be cleared.
#[test]
fn the_sessions_search_stays_while_it_filters_the_menu() {
    use crate::app::ask_sessions::{AskSession, SavedTurn, now};
    use iced_test::selector::Candidate;
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    let session = |id, title: &str| AskSession {
        id,
        title: title.into(),
        updated: now(),
        turns: vec![SavedTurn {
            question: title.into(),
            spoken: false,
            answer: "a".into(),
        }],
    };
    app.proj.ask_sessions = vec![
        session(1, "Where is the origin?"),
        session(2, "How is a host connected?"),
    ];
    app.proj.ask_session = Some(1);
    app.ask_sessions_menu = true;
    app.ask_sessions_query = "host".into();
    let _ = app.update(Message::Ask(AskMsg::DeleteSession(2)));
    assert_eq!(app.proj.ask_sessions.len(), 1);
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "No sessions match “host”."));
    let field = sim.find(|c: Candidate<'_>| match c {
        Candidate::TextInput {
            visible_bounds,
            state,
            ..
        } if state.text() == "host" => visible_bounds,
        _ => None,
    });
    assert!(field.is_ok(), "the search that filters the menu is gone");
}

/// The margin's "not explained yet" note speaks the language just chosen:
/// laid out once, it stayed in the one spoken when the file opened.
#[test]
fn the_unexplained_note_follows_a_language_switch() {
    use crate::i18n::{Lang, with_lang};
    let mut app = reader_app();
    let abs = app.active_viewer().unwrap().abs.clone();
    let said = |app: &App| format!("{:?}", app.proj.explain.prepared);
    let _ = app.show_explanation(crate::explain::Node::File(abs));
    assert!(said(&app).contains("Not explained yet"), "{}", said(&app));
    with_lang(Lang::Fr, || {
        let _ = app.respeak_explanation();
    });
    assert!(said(&app).contains("Pas encore expliqué"), "{}", said(&app));
}

/// Smart Turn in Voice settings: its download says its size; once it is
/// here, Remove; what went wrong with it is said under it — and that it
/// cannot run without `clew-speak` beside Clew.
#[test]
fn smart_turn_is_offered_in_voice_settings() {
    use crate::VoiceMsg;
    let mut app = reader_app();
    let dir = crate::app::tests::test_dir("ui-smart-turn");
    let _ = std::fs::remove_dir_all(&dir);
    let store = clew_voice::models::Store::new(dir);
    app.voice.store = Some(store.clone());
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(&mut sim, "Smart Turn"));
    assert!(shows(&mut sim, "a pause to think is waited through"));
    assert!(matches!(
        click(sim, "Download (9 MB)").as_slice(),
        [Message::Voice(VoiceMsg::DownloadTurn)]
    ));
    std::fs::create_dir_all(clew_voice::turn::dir(&store)).unwrap();
    std::fs::File::create(clew_voice::turn::path(&store))
        .unwrap()
        .set_len(clew_voice::turn::MODEL.bytes)
        .unwrap();
    app.voice.turn.error = Some("the model failed: it broke".into());
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(&mut sim, "the model failed: it broke"));
    assert!(!shows(&mut sim, "Download (9 MB)"));
    assert!(matches!(
        click(sim, "Remove").as_slice(),
        [Message::Voice(VoiceMsg::RemoveTurn)]
    ));
    app.voice.turn.judges = None;
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(
        &mut sim,
        "clew-speak, which runs it, is not installed beside Clew"
    ));
    let _ = std::fs::remove_dir_all(store.dir());
}

/// Waiting for "Hey, Clew" in Voice settings: its switch, and the keyword
/// spotter it needs — its download saying its size, once here Remove, what
/// went wrong under it, and that it cannot run without `clew-speak`. The
/// microphone, while it waits, is drawn in the accent, and says the phrase
/// starts it too.
#[test]
fn the_wake_phrase_is_offered_in_voice_settings() {
    use crate::VoiceMsg;
    let _turn = crate::app::wake::TEST_TURN
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut app = reader_app();
    let dir = crate::app::tests::test_dir("ui-wake-phrase");
    let _ = std::fs::remove_dir_all(&dir);
    let store = clew_voice::models::Store::new(dir);
    app.voice.store = Some(store.clone());
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(&mut sim, "Start listening when you say “Hey, Clew”"));
    assert!(shows(
        &mut sim,
        "goes only to a small model on this computer"
    ));
    assert!(shows(&mut sim, "Keyword Spotter"));
    assert!(matches!(
        click(sim, "Download (33 MB)").as_slice(),
        [Message::Voice(VoiceMsg::DownloadWake)]
    ));
    let wake = clew_voice::wake::dir(&store);
    std::fs::create_dir_all(&wake).unwrap();
    for file in clew_voice::wake::MODEL.files {
        std::fs::File::create(wake.join(file.name))
            .unwrap()
            .set_len(file.bytes)
            .unwrap();
    }
    app.voice.wake.error = Some("the microphone was unplugged".into());
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(&mut sim, "the microphone was unplugged"));
    assert!(!shows(&mut sim, "Download (33 MB)"));
    assert!(matches!(
        click(sim, "Remove").as_slice(),
        [Message::Voice(VoiceMsg::RemoveWake)]
    ));
    app.voice.wake.spotters = None;
    let mut sim = sim_elem(super::voice_section(&app));
    assert!(shows(
        &mut sim,
        "clew-speak, which runs it, is not installed beside Clew"
    ));
    drop(sim);
    // Waiting: the microphone at rest says so.
    let rest = crate::theme::fg_muted();
    assert_eq!(super::askfloat::talk_tip(&app), "Talk");
    assert_eq!(super::askfloat::talk_tint(&app, rest), rest);
    crate::app::wake::testing::ready(&mut app, store.clone());
    assert!(app.resume_waiting());
    assert_eq!(super::askfloat::talk_tip(&app), "Talk — or say “Hey, Clew”");
    assert_eq!(
        super::askfloat::talk_tint(&app, rest),
        crate::theme::accent()
    );
    app.stop_waiting();
    let _ = std::fs::remove_dir_all(store.dir());
}

/// What voice could not do is said in the reader's language: on the card,
/// on the pill, and under a model in Settings.
#[test]
fn voice_failures_are_said_in_the_readers_language() {
    use crate::app::voice::{Heard, HeardOutcome};
    use crate::i18n::{Lang, with_lang};
    let mut app = reader_app();
    app.voice.hear(Heard {
        text: String::new(),
        outcome: HeardOutcome::Failed("the microphone did not open in time".into()),
    });
    let dir = crate::app::tests::test_dir("ui-voice-failures");
    let _ = std::fs::remove_dir_all(&dir);
    app.voice.store = Some(clew_voice::models::Store::new(dir));
    app.voice.download_errors.insert(
        "base.en",
        "the download does not match the published ggml-base.en.bin (13 bytes, not 147951465) — \
         it was discarded"
            .into(),
    );
    with_lang(Lang::Fr, || {
        app.ask_view = crate::AskView::Compact;
        assert!(
            shows(&mut sim_of(&app), "le micro ne s’est pas ouvert à temps"),
            "on the card"
        );
        app.ask_view = crate::AskView::Hidden;
        assert!(
            shows(&mut sim_of(&app), "le micro ne s’est pas ouvert à temps"),
            "on the pill"
        );
        assert!(
            shows(
                &mut sim_elem(super::voice_section(&app)),
                "le téléchargement ne correspond pas au ggml-base.en.bin publié"
            ),
            "under the model"
        );
    });
}

/// Clew speaking shows as listening does: on the pill, even with Ask put
/// away, and in the card's composer — the speaker, how loud, and the
/// button lit to stop it.
#[test]
fn speaking_shows_with_a_stop() {
    struct Playing;
    impl crate::app::speech::Playing for Playing {
        fn play(&self, _: &[f32], _: u32) {}
        fn rate(&self) -> u32 {
            48_000
        }
        fn queued_seconds(&self) -> f32 {
            1.0
        }
        fn played(&self) -> u64 {
            0
        }
        fn take_rest(&self) -> (Vec<f32>, u32) {
            (Vec::new(), 1)
        }
        fn idle(&self) -> bool {
            false
        }
        fn level(&self) -> f32 {
            0.5
        }
        fn error(&self) -> Option<String> {
            None
        }
        fn pause(&self) {}
        fn resume(&self) {}
    }
    let mut app = reader_app();
    app.ask_view = crate::AskView::Hidden;
    app.voice.speech.speak_for_test(Box::new(Playing), 0.5);
    {
        let mut sim = sim_of(&app);
        let speaking =
            text_at(&mut sim, |t| t == "Speaking", 0).expect("the pill says Clew speaks");
        let (right, bottom) = ask_corner_edges();
        assert!(
            speaking.x > right - 300.0 && speaking.y > bottom - 60.0,
            "in the corner: {speaking:?}"
        );
    }
    // The pill opens the conversation, as it does at rest: the answer being
    // read is in it.
    assert!(
        click(sim_of(&app), "Speaking")
            .iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Open))),
        "the conversation is out of reach while Clew speaks"
    );
    // Its button, lit, stops it — and no key is named beside it, which
    // said the same thing twice.
    assert!(!shows(&mut sim_of(&app), "Esc"));
    let sent = click_right_of(
        || sim_of(&app),
        "Speaking",
        |m| matches!(m, Message::Voice(crate::VoiceMsg::StopSpeaking)),
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Voice(crate::VoiceMsg::StopSpeaking))),
        "{sent:?}"
    );
    // On the card, in the composer: the button lit to stop it, the field
    // left to type in, Esc left to the pill.
    app.ask_view = crate::AskView::Compact;
    assert!(!shows(&mut sim_of(&app), "Esc"));
    let lit = super::askfloat::voice_lit(&app, super::ROUND_BUTTON, 15.0).expect("lit");
    let mut sim = sim_elem(lit);
    sim.point_at(Point::new(
        super::ROUND_BUTTON / 2.0,
        super::ROUND_BUTTON / 2.0,
    ));
    let _ = sim.simulate(iced_test::simulator::click());
    assert!(
        sent_by(sim)
            .iter()
            .any(|m| matches!(m, Message::Voice(crate::VoiceMsg::StopSpeaking)))
    );
}

/// What voice is doing shows in Ask's corner, never in the field at the
/// top: on the pill while the card is hidden — even put away — and on the
/// card while it is open; with a way to stop, and for a command that asks
/// first, a way to say yes. A command it ran is said on the pill for a
/// moment.
#[test]
fn voice_shows_what_it_is_doing() {
    use crate::app::voice::{Heard, HeardOutcome, VoicePhase};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Hidden;
    app.voice.phase = VoicePhase::Listening {
        since: std::time::Instant::now(),
        level: 0.5,
    };
    {
        assert!(
            click(sim_of(&app), "Listening")
                .iter()
                .any(|m| matches!(m, Message::Ask(AskMsg::Open))),
            "the pill opens the conversation while it listens"
        );
        let mut sim = sim_of(&app);
        let listening = text_at(&mut sim, |t| t == "Listening", 0)
            .expect("the pill says it listens, even put away");
        let (right, bottom) = ask_corner_edges();
        assert!(
            listening.x > right - 300.0 && listening.y > bottom - 60.0,
            "in the corner, not the field at the top: {listening:?}"
        );
    }
    // The pill's microphone, lit while it listens, stops it.
    let (right, bottom) = ask_corner_edges();
    let mut sim = sim_of(&app);
    sim.point_at(Point::new(
        right - 7.0 - 1.0 - 13.0,
        bottom - super::askfloat::PILL_H / 2.0,
    ));
    let _ = sim.simulate(iced_test::simulator::click());
    // (Its tooltip opens too, under the pointer.)
    let voice: Vec<Message> = sent_by(sim)
        .into_iter()
        .filter(|m| matches!(m, Message::Voice(_)))
        .collect();
    assert!(matches!(
        voice.as_slice(),
        [Message::Voice(crate::VoiceMsg::Toggle)]
    ));
    // On the card, the field says it listens — and can still be typed in;
    // the microphone, lit, turns it off. What is said is worked out out of
    // sight: nothing says so, and there is no switch to keep it listening.
    app.voice.mic_on = true;
    app.ask_view = crate::AskView::Compact;
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "Keep listening"));
        assert!(!shows(&mut sim, "Transcribing"));
    }
    let lit = super::askfloat::voice_lit(&app, super::ROUND_BUTTON, 15.0).expect("lit while on");
    let mut sim = sim_elem(lit);
    sim.point_at(Point::new(
        super::ROUND_BUTTON / 2.0,
        super::ROUND_BUTTON / 2.0,
    ));
    let _ = sim.simulate(iced_test::simulator::click());
    assert!(
        sent_by(sim)
            .iter()
            .any(|m| matches!(m, Message::Voice(crate::VoiceMsg::Toggle))),
        "it turns the microphone off"
    );
    // A command that asks first, while it listens on for the yes.
    app.voice.confirm = Some(crate::app::voice::Waiting::Command("Explain All".into()));
    assert!(shows(&mut sim_of(&app), "Run “Explain All”?"));
    assert!(matches!(
        click(sim_of(&app), "Run").as_slice(),
        [Message::Voice(crate::VoiceMsg::Confirm)]
    ));
    // On the card: what was last heard, and what came of it — as it
    // listens on.
    app.voice.confirm = None;
    app.voice.hear(Heard {
        text: "open the call graph".into(),
        outcome: HeardOutcome::Ran("Call Graph".into()),
    });
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "“open the call graph”"));
        assert!(shows(&mut sim, "→ Call Graph"));
    }
    // Hidden, the pill says it for a moment, then is Ask's again.
    app.ask_view = crate::AskView::Hidden;
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "“open the call graph”"));
        assert!(shows(&mut sim, "Call Graph"));
    }
    app.voice.heard_at = Some(std::time::Instant::now() - crate::app::voice::HEARD_SHOWN);
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "“open the call graph”"));
        assert!(shows(&mut sim, "Listening"), "listening, as before");
    }
    app.voice.mic_on = false;
    app.voice.phase = VoicePhase::Idle;
    assert!(shows(&mut sim_of(&app), "Ask"));
}

/// The settings' Voice section: every model with its size, downloaded
/// only on the user's word; one downloading shows how far it got; one
/// downloaded can be removed; a multilingual one asks the language.
#[test]
fn the_voice_settings_offer_each_model_and_its_size() {
    let mut app = reader_app();
    let dir = crate::app::tests::test_dir("ui-voice-settings");
    let _ = std::fs::remove_dir_all(&dir);
    let store = clew_voice::models::Store::new(dir);
    app.voice.store = Some(store.clone());
    app.voice.prefs = crate::app::voice::VoicePrefs::default();
    {
        let mut sim = sim_elem(super::voice_section(&app));
        for model in clew_voice::models::MODELS {
            assert!(shows(&mut sim, model.name), "{}", model.name);
        }
        // No captions around the groups: not the voice key, nor where the
        // models are kept.
        assert!(!shows(&mut sim, "Speech stays on this computer"));
        assert!(!shows(&mut sim, "Models are stored in"));
        assert!(
            shows(&mut sim, "Language"),
            "Echo's Small hears any language"
        );
        assert!(
            shows(&mut sim, "Download (190 MB)"),
            "its size, before anything"
        );
        // The one chosen, not here yet, says what being chosen does.
        assert!(shows(
            &mut sim,
            "Voice uses this model once it is downloaded."
        ));
    }
    app.voice.prefs.model = "base.en".into();
    assert!(
        !shows(&mut sim_elem(super::voice_section(&app)), "Language"),
        "base.en hears English only"
    );
    assert!(matches!(
        click(sim_elem(super::voice_section(&app)), "Download (148 MB)").as_slice(),
        [Message::Voice(crate::VoiceMsg::Download("base.en"))]
    ));
    assert!(matches!(
        click(sim_elem(super::voice_section(&app)), "Large Turbo").as_slice(),
        [Message::Voice(crate::VoiceMsg::PickModel(
            "large-v3-turbo-q5_0"
        ))]
    ));
    // Downloaded: Remove. A multilingual one picked: its language.
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.path(app.voice.prefs.model()), b"x").unwrap();
    app.voice.prefs.model = "small-q5_1".into();
    {
        let mut sim = sim_elem(super::voice_section(&app));
        assert!(shows(&mut sim, "Downloaded"));
        assert!(shows(&mut sim, "Language"));
    }
    // The one chosen here too: nothing to say about it.
    std::fs::write(store.path(app.voice.prefs.model()), b"x").unwrap();
    assert!(
        !shows(
            &mut sim_elem(super::voice_section(&app)),
            "once it is downloaded"
        ),
        "the one chosen is here"
    );
    assert!(matches!(
        click(sim_elem(super::voice_section(&app)), "Remove").as_slice(),
        [Message::Voice(crate::VoiceMsg::RemoveModel("base.en"))]
    ));
    // Just downloaded, its Remove is not yet where its Cancel was: a click
    // meant for Cancel removed what had just arrived.
    app.voice
        .downloaded_at
        .insert("small-q5_1", std::time::Instant::now());
    app.voice
        .downloaded_at
        .insert("base.en", std::time::Instant::now());
    assert!(!shows(&mut sim_elem(super::voice_section(&app)), "Remove"));
    let _ = std::fs::remove_dir_all(store.dir());
}

/// The borderless window resizes from its edges and corners on Linux and
/// Windows: a press on one starts the system's resize toward it. Inside the
/// band nothing changes — a press there reaches what is drawn. (macOS keeps
/// the system's own frame, and no grip.)
#[test]
fn the_window_resizes_from_its_edges_and_corners() {
    let app = reader_app();
    let pressed_at = |at: Point| {
        let mut sim = sim_of(&app);
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    let resized = |sent: &[Message]| {
        sent.iter().find_map(|m| match m {
            Message::Window(crate::WindowMsg::ResizeDragged(edge)) => Some(format!("{edge:?}")),
            _ => None,
        })
    };
    let (w, h) = (WINDOW.width, WINDOW.height);
    for (at, edge) in [
        (Point::new(1.0, h / 2.0), "West"),
        (Point::new(w - 1.0, h / 2.0), "East"),
        (Point::new(w / 2.0, 1.0), "North"),
        (Point::new(w / 2.0, h - 1.0), "South"),
        (Point::new(1.0, 1.0), "NorthWest"),
        (Point::new(w - 1.0, 1.0), "NorthEast"),
        (Point::new(1.0, h - 1.0), "SouthWest"),
        (Point::new(w - 1.0, h - 1.0), "SouthEast"),
    ] {
        let sent = pressed_at(at);
        let expected = (!cfg!(target_os = "macos")).then(|| edge.to_string());
        assert_eq!(resized(&sent), expected, "{at:?}: {sent:?}");
    }
    for inside in [Point::new(w / 2.0, h / 2.0), Point::new(12.0, h / 2.0)] {
        assert_eq!(resized(&pressed_at(inside)), None, "{inside:?}");
    }
}

/// The sidebar's strip holds its six everyday views and nothing else; a
/// view opened by what shows it (a function's calls, a value traced, the
/// import graph, a symbol's docs) is named over it, with the way back to
/// the files.
#[test]
fn a_view_off_the_strip_is_named_over_it() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    assert_eq!(super::sidebar::STRIP_VIEWS, 6);
    assert!(!shows(&mut sim_of(&app), "Value flow"));
    for (tab, name) in [
        (crate::SidebarTab::Flow, "Value flow"),
        (crate::SidebarTab::Imports, "Imports"),
        (crate::SidebarTab::Calls, "Call hierarchy"),
        (crate::SidebarTab::Docs, "API docs"),
        (crate::SidebarTab::Notes, "Your notes"),
    ] {
        app.sidebar = tab;
        assert!(shows(&mut sim_of(&app), name), "{name} is named");
    }
    app.sidebar = crate::SidebarTab::Marks;
    assert!(
        !shows(&mut sim_of(&app), "Marks"),
        "a view on the strip needs no title"
    );
}

/// The raised note's actions are the three the design gives it — what
/// leans on it, when it changed, and Ask — and a function's explanation in
/// blocks is the View menu's (and the palette's): checked while shown,
/// inert with no function explained, and taken back to the summary there.
#[test]
fn blocks_are_the_view_menus_not_the_notes() {
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    let node = crate::explain::Node::Function {
        file: abs,
        name: "origin".into(),
        ordinal: 0,
    };
    app.proj.panes[0].as_mut().unwrap().symbols = vec![crate::outline::Symbol {
        name: "origin".into(),
        kind: "function".into(),
        line: 3,
        end_line: 5,
    }];
    app.proj.explain.cache.insert(
        node.clone(),
        crate::explain::Cached {
            summary: "Makes the point at zero.".into(),
            prompt_hash: crate::incremental::content_hash(b"t"),
            detail: Some("**Block 1.** Builds the point.".into()),
            basis: None,
        },
    );
    app.proj.explain.view = Some(node);
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Ask"), "the note's Ask");
        assert!(!shows(&mut sim, "Blocks"), "no Blocks on the note");
    }
    // The View menu: its row, enabled.
    app.show_view_menu = true;
    let sent = click(sim_of(&app), "Explanation in blocks");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Explain(crate::ExplainMsg::ToggleBlocks))),
        "{sent:?}"
    );
    app.show_view_menu = false;
    // Toggled: the cached blocks, then the summary again.
    let _ = app.update(Message::Explain(crate::ExplainMsg::ToggleBlocks));
    assert!(app.proj.explain.showing_detail, "shown in blocks");
    assert!(
        app.palette_commands()
            .iter()
            .any(|c| c.title == "Explain: Summary"),
        "and the palette offers the way back"
    );
    let _ = app.update(Message::Explain(crate::ExplainMsg::ToggleBlocks));
    assert!(!app.proj.explain.showing_detail, "the summary again");
    // No function explained: the row is inert.
    app.proj.explain.view = None;
    app.show_view_menu = true;
    let sent = click(sim_of(&app), "Explanation in blocks");
    assert!(
        !sent.iter().any(|m| matches!(m, Message::Explain(_))),
        "{sent:?}"
    );
}

/// The margin is shown and hidden from the View menu — the bars carry no
/// toggle for it — and the menu stays up for the next switch (L21).
#[test]
fn the_view_menu_shows_and_hides_the_margin() {
    let mut app = reader_app();
    app.show_view_menu = true;
    let shown = app.show_right_panel;
    let sent = click(sim_of(&app), "Margin");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::ToggleRightPanel))),
        "{sent:?}"
    );
    for m in sent {
        let _ = app.update(m);
    }
    assert_eq!(app.show_right_panel, !shown, "toggled");
    assert!(app.show_view_menu, "the switch put the menu away");
    let mut sim = sim_of(&app);
    assert!(!shows(&mut sim, "Toggle panel"), "no toggle in the bars");
}

/// On Linux and Windows the window is borderless and Clew draws the design's
/// 1 px border round it, the room inside it what everything is laid out in;
/// maximized, there is no frame, and restored, it is back. macOS keeps the
/// system's frame.
#[test]
fn the_window_is_framed_as_designed_unless_maximized() {
    let mut app = reader_app();
    let size = iced::Size::new(1216.0, 910.0);
    let _ = app.on_window_resized(size);
    let frame = if cfg!(target_os = "macos") { 0.0 } else { 1.0 };
    assert_eq!(app.window_frame, frame);
    assert_eq!(app.window_width, size.width - 2.0 * frame);
    assert_eq!(app.window_height, size.height - 2.0 * frame);
    let _ = app.update(Message::Window(WindowMsg::MaximizedIs(true)));
    assert_eq!(app.window_frame, 0.0);
    assert_eq!(app.window_width, size.width);
    let _ = app.update(Message::Window(WindowMsg::MaximizedIs(false)));
    assert_eq!(app.window_frame, frame);
    assert_eq!(app.window_width, size.width - 2.0 * frame);
}

/// The design rounds the window's corners (12) on every system. On Linux
/// and Windows, where Clew draws the window whole, it rounds them where
/// they can be left clear — the window system composites the window, and
/// iced draws it on the GPU — while the window does not fill the screen;
/// macOS rounds the window itself.
#[test]
fn the_window_rounds_its_corners_where_they_can_be_left_clear() {
    let mut app = reader_app();
    let _ = app.on_window_resized(iced::Size::new(1216.0, 912.0));
    let round = if cfg!(target_os = "macos") { 0.0 } else { 12.0 };
    let _ = app.update(Message::Window(WindowMsg::Composited(true)));
    assert_eq!(app.window_radius, round);
    // Filling the screen, square and unframed.
    let _ = app.update(Message::Window(WindowMsg::MaximizedIs(true)));
    assert_eq!(app.window_radius, 0.0);
    let _ = app.update(Message::Window(WindowMsg::MaximizedIs(false)));
    assert_eq!(app.window_radius, round);
    let _ = app.update(Message::Window(WindowMsg::ModeIs(window::Mode::Fullscreen)));
    assert_eq!((app.window_radius, app.window_frame), (0.0, 0.0));
    let _ = app.update(Message::Window(WindowMsg::ModeIs(window::Mode::Windowed)));
    assert_eq!(app.window_radius, round);
    // Drawn in software, whose surface has no alpha: square.
    let _ = app.update(Message::Window(WindowMsg::SoftwareRenderer(true)));
    assert_eq!(app.window_radius, 0.0);
    let _ = app.update(Message::Window(WindowMsg::SoftwareRenderer(false)));
    assert_eq!(app.window_radius, round);
    // Not composited (X with no compositing manager, Windows presenting
    // to the window directly): square.
    let _ = app.update(Message::Window(WindowMsg::Composited(false)));
    assert_eq!(app.window_radius, 0.0);
}

/// A diff's added lines and hunk headers are tinted across the pane, not
/// only as far as its longest line: a diff of short lines showed a strip of
/// green a third of the way across and nothing after it.
#[test]
fn a_diff_tints_its_rows_across_the_pane() {
    use crate::git::{DiffKind, DiffLine};
    use iced::advanced::renderer::Headless as _;
    let size = iced::Size::new(800.0, 200.0);
    let mut app = reader_app();
    let line = |kind, text: &str| DiffLine {
        kind,
        text: text.into(),
    };
    let diff = crate::DiffState::new(
        PathBuf::from("/p/src/main.py"),
        "src/main.py".into(),
        vec![
            line(DiffKind::Hunk, "@@ -4,1 +4,2 @@"),
            line(DiffKind::Context, " main()"),
            line(DiffKind::Add, "+# local change"),
        ],
    );
    app.proj.diff = Some(diff);
    let d = app.proj.diff.as_ref().unwrap();
    let mut renderer = renderer();
    let mut ui = UserInterface::build(
        super::panes::diff_view(&app, 0, d),
        size,
        Cache::default(),
        &mut renderer,
    );
    let _ = ui.update(
        &[Event::Window(window::Event::RedrawRequested(
            std::time::Instant::now(),
        ))],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    ui.draw(
        &mut renderer,
        &crate::theme::app_theme(),
        &iced::advanced::renderer::Style {
            text_color: iced::Color::BLACK,
        },
        mouse::Cursor::Unavailable,
    );
    let rgba = renderer.screenshot(
        iced::Size::new(size.width as u32, size.height as u32),
        1.0,
        iced::Color::TRANSPARENT,
    );
    // Far right of the pane, below its header: the tinted rows are there,
    // and the context line between them is not.
    let x = size.width as u32 - 20;
    let tinted = (40..size.height as u32)
        .filter(|&y| rgba[((y * size.width as u32 + x) * 4 + 3) as usize] > 0)
        .count();
    assert!(tinted >= 20, "{tinted} tinted pixels at x = {x}");
    assert!(tinted < 120, "the whole column is tinted: {tinted}");
}

/// Rounded, the window's corners are clear and what is inside them is not
/// — whatever is over the window: every layer reaching a corner (the
/// chrome, the status bar, a dialog's veil, the tutorial's) rounds its
/// own as the window's are.
#[test]
fn every_layer_leaves_the_rounded_corners_clear() {
    let size = iced::Size::new(640.0, 420.0);
    let pixels = |app: &App| {
        use iced::advanced::renderer::Headless as _;
        let mut renderer = renderer();
        let mut ui =
            UserInterface::build(crate::ui::view(app), size, Cache::default(), &mut renderer);
        let _ = ui.update(
            &[Event::Window(window::Event::RedrawRequested(
                std::time::Instant::now(),
            ))],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut Vec::new(),
        );
        let theme = crate::theme::app_theme();
        ui.draw(
            &mut renderer,
            &theme,
            &iced::advanced::renderer::Style {
                text_color: iced::Color::BLACK,
            },
            mouse::Cursor::Unavailable,
        );
        let physical = iced::Size::new(size.width as u32, size.height as u32);
        renderer.screenshot(physical, 1.0, iced::Color::TRANSPARENT)
    };
    let alpha = |rgba: &[u8], x: u32, y: u32| rgba[((y * size.width as u32 + x) * 4 + 3) as usize];
    let (w, h) = (size.width as u32, size.height as u32);
    for what in ["the window", "a dialog", "the tutorial"] {
        let mut app = reader_app();
        let _ = app.on_window_resized(size);
        app.window_frame = 1.0;
        app.window_radius = crate::ui::WINDOW_RADIUS;
        match what {
            "a dialog" => {
                let _ = app.update(Message::Window(WindowMsg::OpenShortcuts));
            }
            "the tutorial" => app.tutorial = Some(0),
            _ => {}
        }
        let rgba = pixels(&app);
        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(alpha(&rgba, x, y), 0, "{what}: corner ({x}, {y})");
        }
        // Just off each corner, outside its arc; and inside it.
        for (x, y) in [(2, 2), (w - 3, 2), (2, h - 3), (w - 3, h - 3)] {
            assert_eq!(alpha(&rgba, x, y), 0, "{what}: off the arc at ({x}, {y})");
        }
        // Inside the arcs; and the frame's edge, opaque over the desktop.
        for (x, y) in [(6, 6), (w - 7, 6), (6, h - 7), (w - 7, h - 7)] {
            assert_eq!(alpha(&rgba, x, y), 255, "{what}: inside at ({x}, {y})");
        }
        for (x, y) in [(w / 2, 0), (0, h / 2), (w / 2, h - 1), (w - 1, h / 2)] {
            assert_eq!(alpha(&rgba, x, y), 255, "{what}: the edge at ({x}, {y})");
        }
    }
}

/// What is missing to make embeddings on this computer — the model, then
/// the program that runs it — is said where search would be, and on the
/// Embeddings page once the model is here.
#[cfg(unix)]
#[test]
fn what_is_missing_to_make_embeddings_is_shown() {
    use clew_core::embed::local;
    let dir = crate::app::tests::test_dir("ui-embed-missing");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _data = crate::app::tests::data_dir_override(&dir);
    let old = std::env::var_os("PATH").unwrap_or_default();
    let kept: Vec<PathBuf> = std::env::split_paths(&old)
        .filter(|d| !d.join("clew-speak").exists())
        .collect();
    let _path =
        clew_core::testutil::EnvVars::new().set("PATH", std::env::join_paths(kept).unwrap());
    assert!(local::program().is_none(), "no clew-speak to be found");
    clew_core::embed::Config::local().save().unwrap();
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.explain_cache_mut().insert(
        explain_file(&root, "src/lib.rs"),
        crate::explain::Cached {
            summary: "Sums the cart.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    app.refresh_embeddings();
    assert!(!app.embed_available);
    let download = format!("Download {} in Settings → Embeddings", local::MODEL.name);
    let program = "clew-speak, which makes embeddings on this computer, is not installed";
    {
        let mut sim = sim_elem(super::semantic_tab(&app));
        assert!(shows(&mut sim, &download));
    }
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Embeddings,
    )));
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, program), "not before the model is here");
    }
    let model = local::dir().unwrap();
    for file in local::MODEL.files {
        let path = model.join(file.path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }
    // As a download that finished does.
    app.refresh_embeddings();
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, program), "the page says what is missing");
    }
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));
    {
        let mut sim = sim_elem(super::semantic_tab(&app));
        assert!(shows(&mut sim, program), "so does search");
    }
    // A service saved, this computer chosen on the page but not saved yet:
    // the page says what this computer is missing, not the service's.
    clew_core::embed::Config::from_parts("sk".into(), "m".into(), "http://emb.test/v1".into())
        .save()
        .unwrap();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Embeddings,
    )));
    let _ = app.update(Message::Settings(crate::SettingsMsg::EmbedHere(true)));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, program), "for the choice on the page");
}

/// The Stats page reads cleanly: the project's files counted beside its
/// code files (the status bar counts the first); a language past the
/// colours' series greyed as the rest, not a colour another one has; a
/// sliver of a share as under a tenth, not 0.00%; a long language name cut,
/// not wrapped; and room under the last row clear of the Ask pill.
#[test]
fn the_stats_page_reads_cleanly() {
    let mut app = reader_app();
    let files: Vec<crate::fs_scan::FileEntry> = (0..120)
        .map(|i| crate::fs_scan::FileEntry {
            abs: PathBuf::from(format!("/nonexistent/clew-ui-test/f{i}.rs")),
            rel: format!("f{i}.rs"),
        })
        .collect();
    app.proj.project.as_mut().unwrap().files = Arc::new(files);
    let lang = |name: &str, code: usize| crate::stats::LangStat {
        name: name.into(),
        files: 1,
        code,
        comments: 0,
        blanks: 0,
    };
    let mut langs: Vec<crate::stats::LangStat> = (0..9)
        .map(|i| lang(&format!("Lang{i}"), 10_000 - i * 100))
        .collect();
    langs.push(lang("Visual Basic for Applications Macro Language", 2));
    let top_files = (0..30)
        .map(|i| clew_protocol::FileStat {
            rel: format!("src/file{i:02}.rs"),
            lang: "Rust".into(),
            code: 800 - i,
            lines: 900 - i,
        })
        .collect();
    let totals = clew_protocol::Totals {
        files: 87,
        code: langs.iter().map(|l| l.code).sum(),
        ..Default::default()
    };
    app.proj.stats.report = Some(crate::stats::StatsReport {
        totals,
        langs,
        top_files,
        ..Default::default()
    });
    // The colours: each its own through the series, grey after.
    let n = crate::theme::series_distinct().len();
    assert!(n >= 5, "a palette of a few colours at least: {n}");
    for i in 0..n {
        for j in i + 1..n {
            assert_ne!(super::lang_color(i), super::lang_color(j), "{i} and {j}");
        }
    }
    assert_eq!(super::lang_color(n), crate::theme::dim());
    assert_eq!(super::lang_color(n + 1), super::lang_color(n));

    let size = Size::new(WINDOW.width, 600.0);
    let view =
        || iced_test::Simulator::with_size(Default::default(), size, super::stats_home(&app));
    let mut sim = view();
    assert!(shows(&mut sim, "120"), "the project's files");
    assert!(shows(&mut sim, "87"), "its code files");
    assert!(!shows(&mut sim, "0.00%"));
    assert!(shows(&mut sim, "<0.1%"), "a sliver said as one");
    let long = text_at(&mut sim, |t| t.starts_with("Visual Basic"), 0).expect("the long name");
    let short = text_at(&mut sim, |t| t == "Lang0", 0).expect("a short one");
    assert!(long.height <= short.height + 0.5, "on one line: {long:?}");
    assert!(shows(&mut sim, "…"), "cut, and said so");
    drop(sim);
    // Scrolled to its end: the last row stands clear of the pill's place.
    let mut sim = view();
    sim.point_at(Point::new(size.width / 2.0, size.height / 2.0));
    let _ = sim.simulate([Event::Mouse(mouse::Event::WheelScrolled {
        delta: mouse::ScrollDelta::Pixels {
            x: 0.0,
            y: -20_000.0,
        },
    })]);
    let last = text_at(&mut sim, |t| t == "src/file29.rs", 0).expect("the last row");
    let pill_top = size.height - super::askfloat::ASK_MARGIN - super::askfloat::PILL_H;
    assert!(
        last.y + last.height <= pill_top,
        "{last:?} under the pill from {pill_top}"
    );
}

/// Actions done again and again — refresh the statistics, rebuild the
/// semantic index, expand every branch of a tree, close the diff — are
/// icons named by their tooltips, not words; each does what its word did.
#[test]
fn repeated_actions_are_icons_that_do_what_they_did() {
    use crate::{CallsMsg, OverviewMsg, SemanticMsg};
    let mut app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let words_gone = |sim: &mut Sim<'_>, words: &[&str]| {
        for w in words {
            assert!(!shows(sim, w), "{w:?} is still a word");
        }
    };

    app.proj.stats.report = Some(crate::stats::StatsReport {
        langs: vec![crate::stats::LangStat {
            name: "Rust".into(),
            files: 1,
            code: 42,
            comments: 0,
            blanks: 0,
        }],
        ..Default::default()
    });
    words_gone(&mut sim_elem(super::stats_home(&app)), &["Refresh"]);
    let sent = click_right_of(
        || sim_elem(super::stats_home(&app)),
        "Code Statistics",
        |m| matches!(m, Message::Overview(OverviewMsg::RefreshStats)),
    );
    assert!(
        !sent.is_empty(),
        "the statistics are refreshed from their icon"
    );

    app.proj.embed_index = crate::embed::Index {
        model: "m".into(),
        base_url: "http://emb.test/v1".into(),
        entries: vec![crate::embed::Entry {
            node: crate::explain::Node::File(root.join("src/lib.rs")),
            hash: 1,
            vec: vec![1.0, 0.0],
        }],
    };
    words_gone(&mut sim_elem(super::semantic_tab(&app)), &["Rebuild"]);
    // At the start of the row the count ends.
    let count = text_at(
        &mut sim_elem(super::semantic_tab(&app)),
        |t| t == "1 indexed",
        0,
    )
    .expect("the count is on screen");
    let rebuilt = (0..count.x as u32).step_by(3).any(|x| {
        let mut sim = sim_elem(super::semantic_tab(&app));
        sim.point_at(Point::new(x as f32, count.center_y()));
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
            .into_iter()
            .any(|m| matches!(m, Message::Semantic(SemanticMsg::BuildIndex)))
    });
    assert!(rebuilt, "the index is rebuilt from its icon");
    // The first build is asked for in words (with a model to build with).
    app.proj.embed_index = crate::embed::Index::default();
    app.embed_available = true;
    assert!(shows(
        &mut sim_elem(super::semantic_tab(&app)),
        "Build index"
    ));

    let item = crate::lsp::client::CallItem {
        name: "origin".into(),
        detail: String::new(),
        kind: 12,
        path: root.join("src/lib.rs"),
        line: 1,
        character: 0,
        raw: serde_json::json!({}),
    };
    app.proj.call_graph = Some(crate::callgraph::CallTree::new(
        1,
        crate::callgraph::Direction::Incoming,
        "rust",
        vec![item],
    ));
    words_gone(&mut sim_elem(super::calls_tab(&app)), &["Expand all"]);
    let sent = click_right_of(
        || sim_elem(super::calls_tab(&app)),
        "origin",
        |m| matches!(m, Message::Calls(CallsMsg::ExpandAll)),
    );
    assert!(!sent.is_empty(), "the tree is expanded from its icon");

    let abs = root.join("src/lib.rs");
    app.proj.diff = Some(crate::DiffState::new(abs, "src/lib.rs".into(), Vec::new()));
    let head = "src/lib.rs: changes since the last commit";
    words_gone(&mut sim_of(&app), &["✕ Close"]);
    let sent = click_right_of(
        || sim_of(&app),
        head,
        |m| matches!(m, Message::Editor(EditorMsg::ToggleDiff)),
    );
    assert!(!sent.is_empty(), "the diff is closed from its icon");
}

/// Rebuilding the index says so by its button's icon, turning in its place:
/// the count it has stays said beside it, and no words come for the build
/// ("Building…", "Building the index…") to move what is there.
#[test]
fn rebuilding_the_index_turns_its_icon_and_says_nothing_new() {
    let mut app = reader_app();
    app.proj.embed_index.entries.push(crate::embed::Entry {
        node: crate::explain::Node::File(std::path::PathBuf::from("/p/a.rs")),
        hash: 0,
        vec: vec![1.0, 0.0],
    });
    for building in [false, true] {
        app.proj.building_embeddings = building;
        let mut sim = sim_elem(super::sidebar::semantic_tab(&app));
        assert!(shows(&mut sim, "1 indexed"), "building: {building}");
        assert!(!shows(&mut sim, "Building…"));
        assert!(!shows(&mut sim, "Building the index…"));
    }
}

/// A question answered with Ask put away to its pill (one said aloud is):
/// the pill says it is being answered — a click on it opens the card — and
/// once answered, says Ask again.
#[test]
fn the_pill_says_an_answer_is_coming() {
    let mut app = reader_app();
    app.ask_view = crate::AskView::Hidden;
    assert!(!shows(&mut sim_of(&app), "Answering"));
    app.proj.asking = true;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Answering"), "said on the pill");
    let at = text_at(&mut sim, |t| t == "Answering", 0).unwrap();
    sim.point_at(at.center());
    let _ = sim.simulate(iced_test::simulator::click());
    let sent = sent_by(sim);
    assert!(
        sent.iter().any(|m| matches!(m, Message::Ask(AskMsg::Open))),
        "{sent:?}"
    );
    // The card open, it says it there, not on a pill.
    app.ask_view = crate::AskView::Compact;
    assert!(!shows(&mut sim_of(&app), "Answering"));
    app.ask_view = crate::AskView::Hidden;
    app.proj.asking = false;
    assert!(!shows(&mut sim_of(&app), "Answering"));
}

/// The status bar is one line at any width and in any language: what Clew
/// says is cut short with an ellipsis (said whole where the pointer rests),
/// and where the chips and it do not fit, the chips are said short and it
/// gives way. A narrow window wrapped the counts onto two lines above the
/// bar, a French chip onto two, and cut what was said mid-word.
#[test]
fn the_status_bar_is_one_line_at_any_width() {
    use crate::i18n::{Lang, with_lang};
    let narrow = Size::new(860.0, 640.0);
    let mut app = reader_app();
    app.window_width = narrow.width;
    app.window_height = narrow.height;
    app.status = "Reading for Linux, Windows and macOS: the branches of every #[cfg] in \
                  src/ui/chrome.rs are read for the target picked below"
        .into();
    // A line of the bar: its top within the bar, and one line high.
    let one_line = |at: Rectangle| {
        assert!(
            at.y >= narrow.height - super::STATUSBAR_H - 1.0,
            "above the bar: {at:?}"
        );
        assert!(at.height < 20.0, "more than one line: {at:?}");
    };
    {
        let mut sim = sim_sized(&app, narrow);
        let at = text_at(&mut sim, |t| t.starts_with("Reading for Linux"), 0)
            .expect("what Clew says is shown");
        one_line(at);
        assert!(
            text_at(
                &mut sim,
                |t| t.starts_with("Reading for Linux") && t.ends_with('…'),
                0
            )
            .is_some(),
            "cut short with an ellipsis"
        );
    }
    // In French, a chip to set up servers and a long status: the chip on one
    // line, short if it must be.
    app.proj.setup_missing = vec![crate::app::setup::SetupServer {
        language: "rust".into(),
        name: "rust-analyzer".into(),
        fetch: crate::app::setup::Fetch::Download,
        builds: false,
    }];
    app.status = "x".repeat(200);
    with_lang(Lang::Fr, || {
        let mut sim = sim_sized(&app, narrow);
        let chip = text_at(&mut sim, |t| t.starts_with("Configurer") || t == "1", 0)
            .expect("the chip is shown");
        one_line(chip);
    });
}

/// In a window too narrow for the status bar's chip, the chip is left
/// out — not laid out clipped to nothing, an invisible control the ring
/// stopped on (its ring two brackets of a pixel).
#[test]
fn a_chip_with_no_room_is_no_stop() {
    let presses_setup = |width: f32| {
        let mut app = reader_app();
        app.window_width = width;
        app.window_height = 520.0;
        app.proj.setup_missing = vec![crate::app::setup::SetupServer {
            language: "rust".into(),
            name: "rust-analyzer".into(),
            fetch: crate::app::setup::Fetch::Download,
            builds: false,
        }];
        let mut window = Driven::sized(&app, Size::new(width, 520.0));
        window.tab(&mut app, false);
        let (_, count, _) = ring_at(&app);
        (0..count).any(|_| {
            let on = matches!(
                ring_at(&app).2,
                Some(Message::Project(ProjectMsg::SetupShow(_)))
            );
            window.tab(&mut app, false);
            on
        })
    };
    assert!(presses_setup(1280.0), "a stop where there is room");
    // Its right side's words — the language, the server, the caret — took
    // the room a window shows them in; here, the window's own narrowness.
    assert!(!presses_setup(400.0), "none where there is not");
}

/// What the status bar lays out for its room — the chip that sets up
/// missing language servers — is built only when laid out, and is pressed
/// by voice all the same: gathered as it is drawn. Pressed, it asks first —
/// the setup sheet, saying what each server takes — and fetches nothing,
/// runs no build, until that is agreed to.
#[test]
fn the_status_bars_chips_are_pressed_by_voice() {
    let mut app = reader_app();
    app.proj.setup_missing = vec![crate::app::setup::SetupServer {
        language: "rust".into(),
        name: "rust-analyzer".into(),
        fetch: crate::app::setup::Fetch::Download,
        builds: false,
    }];
    let targets = crate::ui::heard::gather(&app);
    let chip = targets
        .iter()
        .find(|t| t.title == "Set up missing language servers")
        .expect("the chip is gathered");
    assert!(matches!(
        chip.message,
        Message::Project(ProjectMsg::SetupShow(crate::app::setup::SetupMode::Manage))
    ));
    let _ = app.update(chip.message.clone());
    assert!(
        app.setup
            .as_ref()
            .is_some_and(|s| s.mode == crate::app::setup::SetupMode::Manage),
        "the setup sheet, asking"
    );
    assert!(app.proj.setting_up.is_empty(), "nothing set up yet");
}

/// The ⋯ menu fits the window: in one short enough for the strip to fall
/// back to ⋯, it is raised to end above the window's foot (its last row
/// shown, and clicked), and in one shorter than the menu, its rows scroll.
/// Hung from its button, its last rows ran past the foot, out of reach.
#[test]
fn the_tools_menu_fits_a_short_window() {
    for height in [460.0, 520.0] {
        let size = Size::new(1280.0, height);
        let mut app = reader_app();
        app.window_width = size.width;
        app.window_height = size.height;
        assert!(!super::tools_on_strip(&app));
        app.show_tools_menu = true;
        let top = super::tools_menu_top(&app);
        assert!(top >= super::TOOLBAR_H, "over the top bar: {top}");
        let mut sim = sim_sized(&app, size);
        let last = text_at(&mut sim, |t| t == "Tutorial", 0).expect("the last row is drawn");
        assert!(
            last.y + last.height <= height,
            "the last row runs past the foot at {height}: {last:?}"
        );
        assert!(
            text_at(&mut sim, |t| t == "Explain All", 0).is_some(),
            "and the first is there too"
        );
        sim.point_at(last.center());
        let _ = sim.simulate(iced_test::simulator::click());
        assert!(
            sent_by(sim)
                .iter()
                .any(|m| matches!(m, Message::Tutorial(crate::TutorialMsg::Start))),
            "its last row clicked at {height}"
        );
    }
    // Shorter than the menu: its rows scroll in what there is.
    let mut app = reader_app();
    app.window_height = 240.0;
    assert!(super::tools_menu_h() > super::tools_menu_room(&app));
}

/// The open file's name keeps its room in the sheet's header: where the
/// labelled tools would leave it less, they are said short (their glyphs,
/// their names in their tooltips) — in French in a narrow window, the
/// tools took the whole row and the name was cut to nothing.
#[test]
fn the_files_name_keeps_its_room_in_the_header() {
    use crate::i18n::{Lang, with_lang};
    let narrow = Size::new(860.0, 640.0);
    let mut app = reader_app();
    app.window_width = narrow.width;
    app.window_height = narrow.height;
    app.show_left_sidebar = true;
    with_lang(Lang::Fr, || {
        let mut sim = sim_sized(&app, narrow);
        let name = text_at(&mut sim, |t| t == "lib.rs", 0).expect("the name is drawn");
        assert!(name.width > 20.0, "the name cut to nothing: {name:?}");
        assert!(
            !shows(&mut sim, "Voyage dans le temps"),
            "the tools are said short"
        );
        // Short, they are where their rect says, and still do what they do.
        let tools = super::file_tools_rect(&app).unwrap();
        assert!(tools.x > name.x + name.width, "{tools:?} over {name:?}");
    });
    // With room, they are said whole.
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.window_width = WINDOW.width;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Time Travel"));
    let name = text_at(&mut sim, |t| t == "lib.rs", 0).unwrap();
    assert!(name.width > 20.0);
}

/// A note not explained yet offers what explains it, there in the note: it
/// named "the ⋯ menu", which was no menu on screen once its tools were laid
/// on the strip — and it explains this note's file or function, in one
/// call, not the whole project.
#[test]
fn an_unexplained_note_offers_explain_all() {
    let mut app = reader_app();
    app.show_right_panel = true;
    app.margin_tab = crate::MarginTab::Notes;
    let abs = app.active_viewer().unwrap().abs.clone();
    let _ = app.show_explanation(crate::explain::Node::File(abs));
    // The note says so (its words are markdown, rendered as rich text), and
    // names no menu.
    let said = format!("{:?}", app.proj.explain.prepared);
    assert!(said.contains("Not explained yet."), "{said}");
    assert!(!said.contains('⋯'), "{said}");
    // Every "Explain" on screen (the file's header has one too): the
    // note's is the rightmost, in the margin.
    let explains = |app: &App| -> Vec<Rectangle> {
        let mut sim = sim_of(app);
        (0..6)
            .map_while(|n| text_at(&mut sim, |t| t == "Explain", n))
            .collect()
    };
    let before = explains(&app);
    let note_explain = *before
        .iter()
        .max_by(|a, b| a.center_x().total_cmp(&b.center_x()))
        .expect("an Explain on the note");
    let mut sim = sim_of(&app);
    sim.point_at(note_explain.center());
    let _ = sim.simulate(iced_test::simulator::click());
    let sent: Vec<Message> = sent_by(sim);
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Explain(crate::ExplainMsg::Explain(crate::explain::Node::File(_)))
        )),
        "{sent:?}"
    );
    // Explained, it is not offered.
    let node = crate::explain::Node::File(app.active_viewer().unwrap().abs.clone());
    app.proj.explain.cache.insert(
        node.clone(),
        crate::explain::Cached {
            summary: "The file's note.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let _ = app.show_explanation(node);
    assert_eq!(
        explains(&app).len(),
        before.len() - 1,
        "not offered once explained"
    );
}

/// The Overview not generated yet says what it is written from and offers
/// the one step that gets there, as a button: it said "Run Explain All
/// first." in words alone, and the page was a dead end.
#[test]
fn the_overview_offers_its_next_step() {
    let mut app = reader_app();
    app.llm_available = false;
    let sent = click(sim_elem(super::overview_home(&app)), "Add an API Key…");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Settings(crate::SettingsMsg::OpenAt(
                crate::SettingsPage::LanguageModel
            ))]
        ),
        "{sent:?}"
    );
    app.llm_available = true;
    let sent = click(sim_elem(super::overview_home(&app)), "Explain All…");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Project(crate::ProjectMsg::SetupShow(_))]
        ),
        "{sent:?}"
    );
    let node = crate::explain::Node::File(app.active_viewer().unwrap().abs.clone());
    app.proj.explain.cache.insert(
        node,
        crate::explain::Cached {
            summary: "A note.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    // Ready: written by the icon beside the close button, not by a button
    // floated alone under the map.
    let mut sim = sim_elem(super::overview_home(&app));
    assert!(!shows(&mut sim, "Generate overview"), "a word, not an icon");
    drop(sim);
    let sent = click_right_of(
        || sim_elem(super::overview_home(&app)),
        "Architecture Overview",
        |m| matches!(m, Message::Overview(crate::OverviewMsg::Generate)),
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Overview(crate::OverviewMsg::Generate))),
        "{sent:?}"
    );
    // Writing it keeps the page and turns the icon: the page was swapped
    // for a line of words.
    app.proj.overview.generating = true;
    let mut sim = sim_elem(super::overview_home(&app));
    assert!(shows(&mut sim, "Architecture Overview"));
    assert!(shows(&mut sim, "Generating architecture overview…"));
}

/// An empty type map says why: an API index never built — and, without
/// Clew's server, that it cannot be — is no project without types. It
/// said "No types found" of a project of a thousand.
#[test]
fn an_empty_type_map_says_why() {
    let mut app = reader_app();
    assert_eq!(app.proj.docs.generation, 0);
    {
        let mut sim = sim_elem(super::project_types_body(&app));
        assert!(shows(&mut sim, "not running"));
        assert!(!shows(&mut sim, "No types found"));
    }
    // Built, and empty: then there are none.
    app.proj.docs.generation = 1;
    let mut sim = sim_elem(super::project_types_body(&app));
    assert!(shows(&mut sim, "No types found in this project."));
}

/// The Ask card's header has one way to put it away — Close, to the pill —
/// not a Hide beside it that did the same, its chevron the session menu's
/// again; and Esc is named on the one control it does.
#[test]
fn the_ask_header_has_one_way_away_and_names_esc_once() {
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "Hide (Esc)"));
    }
    // Every control along the header, right of the session's name.
    let name = app.ask_session_title();
    let title =
        text_at(&mut sim_of(&app), move |t| t == name, 0).expect("the session's name on the card");
    let place = super::ask_place(&app).unwrap();
    let sent = (1..)
        .map(|i| title.x + title.width + i as f32 * 4.0)
        .take_while(|&x| x < place.left + place.width)
        .flat_map(|x| {
            let mut sim = sim_of(&app);
            sim.point_at(Point::new(x, title.center_y()));
            let _ = sim.simulate(iced_test::simulator::click());
            sent_by(sim)
        })
        .collect::<Vec<_>>();
    assert!(
        !sent.iter().any(|m| matches!(m, Message::Ask(AskMsg::Hide))),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Close)))
    );
}

/// The Ask card keeps its height while an answer comes in: fitted to it,
/// the card grew up the window a line at a time, its header moving with
/// every line said.
#[test]
fn the_ask_card_holds_still_while_an_answer_comes_in() {
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 4,
        question: "What does it do?".into(),
        streaming: true,
        ..Default::default()
    });
    app.proj.link.agent_stream = Some(4);
    let _ = app.on_ask_delta(4, "It reads the file.\n\n".into());
    // Where the card's header is drawn: its session's name.
    let header_y = |app: &App| {
        let name = app.ask_session_title();
        text_at(&mut sim_of(app), move |t| t == name, 0)
            .expect("the card's header")
            .y
    };
    let before = header_y(&app);
    for n in 0..12 {
        let _ = app.on_ask_delta(4, format!("- line {n} of what it does\n"));
    }
    assert_eq!(
        header_y(&app),
        before,
        "the header moved as the answer came in"
    );
}

/// The strip's tools that read a project do nothing with none open, and are
/// drawn faded so: they looked as ready as the rest, and a press did
/// nothing. Settings and ⋯ still answer.
#[test]
fn the_strips_project_tools_wait_for_a_project() {
    let press = |app: &App, tool: super::StripTool| {
        let at = super::strip_tool_center(app, tool);
        let mut sim = sim_of(app);
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    let mut app = blank_app();
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    assert!(app.proj.project.is_none());
    assert!(press(&app, super::StripTool::CallGraph).is_empty());
    assert!(
        press(&app, super::StripTool::Settings)
            .iter()
            .any(|m| matches!(m, Message::Settings(crate::SettingsMsg::Open)))
    );
    let app = reader_app();
    assert!(
        press(&app, super::StripTool::CallGraph)
            .iter()
            .any(|m| matches!(m, Message::Graph(_))),
        "with a project it opens"
    );
}

/// The graph's Heat toggle is as wide on as off: a dot added after its word
/// when on widened it, and moved the controls beside it.
#[test]
fn the_heat_toggle_keeps_its_width() {
    let heat_at = |on: bool| {
        let frame = super::graph_modal_frame(
            "Import Graph",
            true,
            false,
            false,
            on,
            None,
            None,
            None,
            WINDOW,
            space().into(),
        );
        let mut sim = sim_elem(frame);
        text_at(&mut sim, |t| t.starts_with("Heat"), 0).expect("the toggle")
    };
    assert_eq!(heat_at(false), heat_at(true));
}

/// Settings, opened from a project's sheet — the first open's, or one
/// asked from the menu — comes up over it, and closes back to it: under
/// the sheet, "Add an API Key…" seemed to do nothing.
#[test]
fn settings_opened_from_the_projects_sheet_is_over_it() {
    use crate::app::setup::{ProjectSetup, SetupMode};
    let mut app = reader_app();
    let root = PathBuf::from("/nonexistent/clew-sheet");
    app.pending_consent = Some(root.clone());
    assert!(matches!(
        super::active_overlay(&app),
        Some(super::ActiveOverlay::Consent(_))
    ));
    app.settings.open = true;
    assert!(matches!(
        super::active_overlay(&app),
        Some(super::ActiveOverlay::Settings)
    ));
    app.settings.open = false;
    assert!(matches!(
        super::active_overlay(&app),
        Some(super::ActiveOverlay::Consent(_))
    ));
    app.pending_consent = None;
    app.setup = Some(ProjectSetup {
        root,
        mode: SetupMode::Manage,
        servers: true,
        servers_declined: false,
        explain: false,
        census: None,
    });
    assert!(matches!(
        super::active_overlay(&app),
        Some(super::ActiveOverlay::Setup(_))
    ));
    app.settings.open = true;
    assert!(matches!(
        super::active_overlay(&app),
        Some(super::ActiveOverlay::Settings)
    ));
}

/// The setup sheet with no AI key offers where one is added, a click away:
/// it said "Needs an API key in Settings." and left the reader to find it.
#[test]
fn the_setup_sheet_links_to_where_a_key_is_added() {
    let root = std::path::Path::new("/nonexistent/clew-sheet");
    let sent = click(sim_elem(open_sheet(root, true)), "Add an API Key…");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Settings(crate::SettingsMsg::OpenAt(
                crate::SettingsPage::LanguageModel
            ))
        )),
        "{sent:?}"
    );
}

/// Click about the top right of `view` — where a page's × is — and return
/// what the first click that sent a `wanted` message sent.
fn click_top_right<'a>(
    view: impl Fn() -> Element<'a, Message>,
    wanted: fn(&Message) -> bool,
) -> Vec<Message> {
    crate::app::tests::isolated_data_dir();
    // The page's padding in from its corner, then half the button.
    let side = super::chrome::CLOSE_SIDE;
    let at = Point::new(WINDOW.width - 28.0 - side / 2.0, 20.0 + side / 2.0);
    let near = [0.0f32, -6.0, 6.0, -12.0, 12.0];
    for dy in near {
        for dx in near {
            let mut sim = iced_test::Simulator::with_size(Default::default(), WINDOW, view());
            sim.point_at(Point::new(at.x + dx, at.y + dy));
            let _ = sim.simulate(iced_test::simulator::click());
            let sent = sent_by(sim);
            if sent.iter().any(wanted) {
                return sent;
            }
        }
    }
    Vec::new()
}

/// Every page over the code has the × the Glossary has, written or still
/// to be, counted or counting: the overview and the statistics had none,
/// and a doc page none either.
#[test]
fn every_page_over_the_code_has_its_close() {
    let mut app = reader_app();
    let closes_overview = |m: &Message| matches!(m, Message::Overview(crate::OverviewMsg::Close));
    // Still to be written: its step, and a ×.
    app.llm_available = false;
    let sent = click_top_right(|| super::overview_home(&app), closes_overview);
    assert!(sent.iter().any(closes_overview), "{sent:?}");
    app.proj.overview.generating = true;
    let sent = click_top_right(|| super::overview_home(&app), closes_overview);
    assert!(sent.iter().any(closes_overview), "{sent:?}");
    app.proj.overview.generating = false;
    // Written: on its title's line.
    app.proj.overview.markdown = Some("The parts.".into());
    let sent = click_right_of(
        || sim_elem(super::overview_home(&app)),
        "Architecture Overview",
        closes_overview,
    );
    assert!(sent.iter().any(closes_overview), "{sent:?}");
    // The statistics, counting.
    app.proj.stats.building = true;
    let sent = click_top_right(|| super::stats_home(&app), closes_overview);
    assert!(sent.iter().any(closes_overview), "{sent:?}");
    // A doc page.
    let page = crate::DocPage {
        rel: "src/lib.rs".into(),
        entries: Vec::new(),
    };
    let closes_page = |m: &Message| matches!(m, Message::Docs(crate::DocsMsg::ClosePage));
    let sent = click_right_of(
        || sim_elem(super::docs_page(&page)),
        "View source",
        closes_page,
    );
    assert!(sent.iter().any(closes_page), "{sent:?}");
    // And each closes.
    app.proj.overview.showing = true;
    let _ = app.update(Message::Overview(crate::OverviewMsg::Close));
    assert!(!app.proj.overview.showing);
    app.proj.docs.page = Some(page);
    let _ = app.update(Message::Docs(crate::DocsMsg::ClosePage));
    assert!(app.proj.docs.page.is_none());
}

/// Each tool is where what it opens is: nothing on the strip down the
/// right edge opens one of the left sidebar's views — walkthroughs are one
/// of its everyday views — opening another project is the app menu's, the
/// language servers are the status bar's and Settings', and the reader's
/// notes are exported from where they are listed.
#[test]
fn each_tool_is_on_the_side_it_opens() {
    use super::toolbar::{
        STRIP_TOP, StripTool, TOOLS_MENU_ROWS, app_menu_head, strip_tool, tools_row,
    };
    let app = reader_app();
    let elsewhere = |m: &Message| {
        matches!(
            m,
            Message::Window(WindowMsg::SidebarTabPicked(_))
                | Message::Lsp(_)
                | Message::Project(crate::ProjectMsg::OpenFolderPressed)
                | Message::Connect(_)
                | Message::Export(_)
        )
    };
    for tool in STRIP_TOP.into_iter().chain([StripTool::Settings]) {
        let (.., message) = strip_tool(&app, tool);
        assert!(!elsewhere(&message), "{tool:?}");
    }
    for row in TOOLS_MENU_ROWS {
        let (_, label, message) = tools_row(&app, row);
        assert!(!message.as_ref().is_some_and(elsewhere), "{label}");
    }
    assert!(
        super::SIDEBAR_TABS[..super::sidebar::STRIP_VIEWS]
            .iter()
            .any(|(_, tab)| *tab == crate::SidebarTab::Walk),
        "walkthroughs hidden in the sidebar's overflow"
    );
    assert!(
        app_menu_head(&app)
            .iter()
            .any(|(.., m)| matches!(m, Message::Project(crate::ProjectMsg::OpenFolderPressed)))
    );
    // Exported from the list of them, once there are any.
    let mut app = reader_app();
    app.sidebar = crate::SidebarTab::Notes;
    assert!(!shows(
        &mut sim_elem(super::sidebar::notes_tab(&app)),
        "Export…"
    ));
    app.proj.notes.push(crate::notes::Note {
        rel: "src/lib.rs".into(),
        symbol: "origin".into(),
        understood: true,
        text: "Where it all starts.".into(),
    });
    assert!(matches!(
        click(sim_elem(super::sidebar::notes_tab(&app)), "Export…").as_slice(),
        [Message::Export(crate::ExportMsg::Start)]
    ));
}

/// The ⋯ menu's rows name the keys that do what they do, on the row and,
/// where the rows are laid on the strip, in their tips — as every other tool
/// does: they named none, as though they had none.
#[test]
fn the_tools_menu_rows_name_their_keys() {
    use super::ToolsRow;
    let keyed = [
        (ToolsRow::ExplainAll, crate::keymap::Action::ExplainAll),
        (ToolsRow::Tutorial, crate::keymap::Action::Tutorial),
    ];
    let mut app = reader_app();
    open_tools_menu(&mut app);
    let size = Size::new(WINDOW.width, app.window_height);
    for (row, action) in keyed {
        let caps = app.keymap.chord(action).caps();
        let mut sim = sim_sized(&app, size);
        let shown = text_at(&mut sim, |t| t == caps, 0);
        assert!(shown.is_some(), "{row:?} does not say {caps}");
    }

    // On the strip: in the tip.
    app.show_tools_menu = false;
    app.window_height = 1000.0;
    assert!(super::tools_on_strip(&app));
    let size = Size::new(WINDOW.width, app.window_height);
    for (row, action) in keyed {
        let at = super::strip_slot_center(&app, super::StripSlot::Row(row)).unwrap();
        let last = super::split_caps(&app.keymap.chord(action).caps())
            .pop()
            .unwrap();
        super::tip::warm_now();
        let mut sim = sim_sized(&app, size);
        sim.point_at(at);
        let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
            position: at,
        })]);
        let key = text_at(&mut sim, |t| t == last, 0);
        assert!(key.is_some(), "{row:?}'s tip does not say {last}");
    }
}

/// What only the language server answers waits for it in the context menu:
/// drawn faded and inert while there is none ready — it was offered, and
/// answered with a line in the status bar — while what Clew answers itself
/// stays.
#[test]
fn the_context_menu_waits_for_the_language_server() {
    let mut app = reader_app();
    let menu = |served| crate::ContextMenu {
        pane: 0,
        line: 0,
        col: 0,
        x: 10.0,
        y: 10.0,
        insight: None,
        served,
        by_pointer: true,
    };
    app.proj.context_menu = Some(menu(false));
    for waits in ["Go to Definition", "Find References", "Call Hierarchy"] {
        let sent = click(sim_of(&app), waits);
        assert!(
            !sent.iter().any(|m| matches!(
                m,
                Message::Hover(HoverMsg::ContextGoto(_)) | Message::Calls(_)
            )),
            "{waits}: {sent:?}"
        );
    }
    let sent = click(sim_of(&app), "Trace Value");
    assert!(
        sent.iter().any(|m| matches!(m, Message::Flow(_))),
        "{sent:?}"
    );
    app.proj.context_menu = Some(menu(true));
    let sent = click(sim_of(&app), "Go to Definition");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Hover(HoverMsg::ContextGoto(_)))),
        "{sent:?}"
    );
    // Each row names the keys that do it in the code, on its own line.
    let trace = app.keymap.chord(crate::keymap::Action::TraceValue).caps();
    for (row, keys) in [("Go to Definition", "gd"), ("Trace Value", trace.as_str())] {
        let mut sim = sim_of(&app);
        let at = |sim: &mut Sim<'_>, t: &str| {
            let t = t.to_string();
            text_at(sim, move |s| s == t, 0)
        };
        let row_at = at(&mut sim, row).unwrap();
        let keys_at = at(&mut sim, keys).unwrap_or_else(|| panic!("{row} names no keys"));
        assert!(
            (row_at.center_y() - keys_at.center_y()).abs() < 2.0,
            "{row}"
        );
        assert!(keys_at.x > row_at.x + row_at.width, "{row}");
    }
}

/// The Set up sheet has its ×, as the first open's does: the same dialog
/// had one shown one way and none the other.
#[test]
fn the_set_up_sheet_has_its_close() {
    let mut app = reader_app();
    let _ = app.update(Message::Project(ProjectMsg::SetupShow(
        crate::app::setup::SetupMode::Manage,
    )));
    assert!(app.setup.is_some());
    let close = crate::ui::heard::gather(&app)
        .into_iter()
        .find(|t| t.title == "Close")
        .expect("its ×");
    assert!(matches!(
        close.message,
        Message::Project(ProjectMsg::SetupClose)
    ));
}

/// With no embedding model, Find by meaning says why it stays empty and
/// where one is set up: a search showed nothing, its reason flashed in the
/// status bar, and the link beside Build index was Explain All.
#[test]
fn find_by_meaning_with_no_model_says_where_to_set_one_up() {
    let mut app = reader_app();
    app.embed_available = false;
    app.embed_missing = "Configure an embedding endpoint in Settings".into();
    let mut sim = sim_elem(super::semantic_tab(&app));
    assert!(
        shows(
            &mut sim,
            "Find by meaning needs an embedding model, on this computer or from a service."
        ),
        "why, in the panel, in one sentence"
    );
    assert!(!shows(&mut sim, "Build index"), "nothing greyed to press");
    let sent = click(
        sim_elem(super::semantic_tab(&app)),
        "Set Up Find by Meaning…",
    );
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Settings(crate::SettingsMsg::OpenAt(crate::SettingsPage::Embeddings))
        )),
        "{sent:?}"
    );
}

/// Build index does nothing without notes to build from, and is drawn so,
/// with the step that gets there beside it: it looked ready, and answered
/// in the status bar. With notes and a model it builds.
#[test]
fn build_index_waits_for_notes_and_offers_explain_all() {
    let mut app = reader_app();
    app.llm_available = true;
    app.embed_available = true;
    assert!(app.proj.explain.cache.is_empty());
    let builds = |m: &Message| matches!(m, Message::Semantic(crate::SemanticMsg::BuildIndex));
    let sent = click(sim_elem(super::semantic_tab(&app)), "Build index");
    assert!(!sent.iter().any(builds), "{sent:?}");
    let sent = click(sim_elem(super::semantic_tab(&app)), "Explain All…");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Project(crate::ProjectMsg::SetupShow(_)))),
        "{sent:?}"
    );
    let node = crate::explain::Node::File(app.active_viewer().unwrap().abs.clone());
    app.proj.explain.cache.insert(
        node,
        crate::explain::Cached {
            summary: "A note.".into(),
            prompt_hash: 1,
            detail: None,
            basis: None,
        },
    );
    let sent = click(sim_elem(super::semantic_tab(&app)), "Build index");
    assert!(sent.iter().any(builds), "{sent:?}");
    // No model to embed with: nothing to build with, and the way to one
    // offered in its place.
    app.embed_available = false;
    let mut sim = sim_elem(super::semantic_tab(&app));
    assert!(!shows(&mut sim, "Build index"));
    assert!(shows(&mut sim, "Set Up Find by Meaning…"));
}

/// A map opened with no project says why it is empty, not "Nothing to
/// show." (a project with nothing in it), and has nothing to refine.
#[test]
fn a_map_with_no_project_says_why() {
    let mut app = blank_app();
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    app.graph_mode = true;
    let calls = crate::Overlay::ProjectCalls;
    assert!(shows(
        &mut sim_elem(super::project_graph_modal(&app, calls)),
        "Open a project to see its graph"
    ));
    let sent = click(
        sim_elem(super::project_graph_modal(&app, calls)),
        "Check calls with the language server",
    );
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Graph(GraphMsg::RefineProjectCalls))),
        "{sent:?}"
    );
}

/// The version is the app's name as written everywhere else: "Clew 0.2.1",
/// not "clew 0.2.1".
#[test]
fn the_version_line_names_clew() {
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Updates,
    )));
    let version = format!("Clew {}", crate::updater::current_version());
    assert!(shows(&mut sim_of(&app), &version), "{version}");
}

/// A streaming answer shows what has settled drawn, and only the paragraph
/// being written as its words: the whole of it was its raw text, `##` and
/// all. What grounded it is shown once it is whole.
#[test]
fn a_streaming_answer_shows_its_settled_blocks_drawn() {
    let mut app = reader_app();
    app.ask_view = crate::AskView::Expanded;
    let settled = "## How it works\n\n";
    let md = format!("{settled}It reads the fi");
    let (answer, _) = app.prepare_segments(settled);
    app.proj.ask_turns.push(crate::app::model::AskTurn {
        stream: 3,
        question: "how?".into(),
        answer_md: md,
        answer,
        settled: settled.len(),
        sources: vec![(
            crate::explain::Node::File(PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs")),
            0.87,
        )],
        streaming: true,
        ..Default::default()
    });
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "It reads the fi▍"));
        assert!(!shows(&mut sim, "## How"), "the heading is drawn as typed");
        assert!(!shows(&mut sim, "Sources"), "sources under half an answer");
    }
    app.proj.ask_turns[0].streaming = false;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Sources"));
    assert!(!shows(&mut sim, "87%"), "a score read as a grade");
}

/// The diff view shows where each change is and its numbers, not git's raw
/// headers.
#[test]
fn the_diff_view_shows_places_and_numbers_not_git_headers() {
    use crate::git::{DiffKind, DiffLine};
    let mut app = reader_app();
    let line = |kind, text: &str| DiffLine {
        kind,
        text: text.into(),
    };
    app.proj.diff = Some(crate::DiffState::new(
        PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"),
        "src/lib.rs".into(),
        vec![
            line(DiffKind::Header, "diff --git a/src/lib.rs b/src/lib.rs"),
            line(DiffKind::Header, "index 1a2b3c..4d5e6f 100644"),
            line(DiffKind::Hunk, "@@ -3,1 +3,2 @@ fn line_2() {"),
            line(DiffKind::Context, " fn line_3() {}"),
            line(DiffKind::Add, "+fn added() {}"),
        ],
    ));
    let d = app.proj.diff.as_ref().unwrap();
    let mut sim = sim_elem(super::panes::diff_view(&app, 0, d));
    assert!(shows(&mut sim, "⋯ 2 unchanged lines   fn line_2() {"));
    assert!(shows(&mut sim, "3 3"));
    assert!(!shows(&mut sim, "diff --git"));
    assert!(!shows(&mut sim, "@@"));
}

/// In Time Travel the status bar says only what the banner and the scrubber
/// do not — the revision's length — and the scrubber counts as its slider
/// runs, from the oldest: the hash and "1/3" were said twice, and the count
/// ran down as the slider went up.
#[test]
fn time_travel_says_each_thing_once_and_counts_as_its_slider_runs() {
    let mut app = reader_app();
    let v = app.proj.panes[0].as_ref().unwrap();
    let (abs, rel) = (v.abs.clone(), v.rel.clone());
    let source = "fn a() {}\nfn b() {}\n".to_string();
    let historical = Viewer::new(
        abs.clone(),
        rel.clone(),
        Some("rust"),
        Arc::new(source.clone()),
        crate::highlight::plain_lines(&source),
    );
    let commit = |sha: &str| clew_protocol::HistCommit {
        sha: sha.into(),
        author: "a".into(),
        time: 0,
        subject: "s".into(),
        path: rel.clone(),
    };
    app.proj.time_travel = Some(crate::TimeTravel {
        abs,
        rel: rel.clone(),
        lang: Some("rust"),
        scope: crate::TimeScope::File,
        commits: vec![
            commit("cccccccc1"),
            commit("bbbbbbbb1"),
            commit("aaaaaaaa1"),
        ],
        idx: 0,
        viewer: Some(historical),
        scroll_y: 0.0,
        scroll_x: 0.0,
        caret: None,
        focus_line: None,
        reveal_change: false,
        loading: false,
        generation: app.proj.time_gen,
        session: 0,
        scoped: 0,
        why: std::collections::HashMap::new(),
        why_loading: false,
        why_pending: std::collections::HashSet::new(),
        story: None,
        story_loading: false,
    });
    {
        let mut sim = sim_of(&app);
        // The newest, at the slider's right end: the last of three.
        assert!(shows(&mut sim, "3 / 3"));
        let hashes = {
            let mut n = 0;
            while text_at(&mut sim, |t| t.contains("ccccccc"), n).is_some() {
                n += 1;
            }
            n
        };
        assert_eq!(hashes, 1, "the commit is named once, in the banner");
        assert!(shows(&mut sim, "2 lines"), "the revision's length");
    }
    app.proj.time_travel.as_mut().unwrap().idx = 2;
    let mut sim = sim_of(&app);
    assert!(
        shows(&mut sim, "1 / 3"),
        "the oldest, at the slider's left end"
    );
}

/// The question's field wraps a long question and grows with it, a few
/// lines at most — it was one line that scrolled sideways, its start out of
/// sight — and Enter asks it while Shift+Enter starts a line.
#[test]
fn the_question_field_wraps_and_enter_asks() {
    use iced::keyboard::{self, Key, key::Named};
    let mut app = reader_app();
    app.ask_view = crate::AskView::Compact;
    let height_of = |app: &App| {
        let mut sim = sim_of(app);
        let target = sim
            .find(super::ask_input_id())
            .expect("the question's field");
        iced_test::selector::Bounded::visible_bounds(&target)
            .expect("on screen")
            .height
    };
    let one = height_of(&app);
    app.proj.ask_input = "how does the server decide which language server to start for a \
                          file that it has never seen before, and what happens when two match"
        .into();
    app.sync_ask_editor();
    let wrapped = height_of(&app);
    assert!(wrapped > one * 1.5, "one line {one}, wrapped {wrapped}");
    app.proj.ask_input = "line\n".repeat(40);
    app.sync_ask_editor();
    let longest = height_of(&app);
    assert!(longest < one * 8.0, "it grew to {longest}");

    // Enter asks; Shift+Enter starts a line.
    app.proj.ask_input = "why?".into();
    app.sync_ask_editor();
    let mut sim = sim_of(&app);
    sim.click(super::ask_input_id())
        .expect("the question's field");
    let _ = sim.tap_key(Key::Named(Named::Enter));
    let sent = sent_by(sim);
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Submit))),
        "{sent:?}"
    );
    let mut sim = sim_of(&app);
    sim.click(super::ask_input_id())
        .expect("the question's field");
    let shift = keyboard::Modifiers::SHIFT;
    let _ = sim.simulate([
        Event::Keyboard(keyboard::Event::ModifiersChanged(shift)),
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Enter),
            modified_key: Key::Named(Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: shift,
            text: Some("\n".into()),
            repeat: false,
        }),
    ]);
    let sent = sent_by(sim);
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Submit))),
        "{sent:?}"
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::Edit(_)))),
        "{sent:?}"
    );
}

/// The node picked on a map is said over it — its name, its place — with an
/// Open beside it; and the map is as large as the window allows, not 760
/// wide in any window.
#[test]
fn a_picked_node_is_said_over_the_map_which_fills_the_window() {
    let mut app = reader_app();
    let size = Size::new(1600.0, 1000.0);
    app.window_width = size.width;
    app.window_height = size.height;
    app.graph_mode = true;
    app.proj.overlay = Some(crate::Overlay::ProjectImports);
    let file = PathBuf::from("/nonexistent/clew-ui-test/src/app/mod.rs");
    app.proj.graph_pick = Some(crate::app::model::GraphPick {
        label: "app/mod.rs".into(),
        file: file.clone(),
        open: Box::new(Message::Graph(GraphMsg::OverlayOpenImports(file.clone()))),
    });
    let view = || sim_sized(&app, size);
    assert!(shows(&mut view(), "app/mod.rs"));
    assert!(shows(&mut view(), "src/app/mod.rs"), "its place");
    let open = text_at(&mut view(), |t| t == "Open", 0).expect("Open");
    assert!(
        open.x + open.width > size.width / 2.0 + 760.0 / 2.0,
        "the map is no wider than before: Open ends at {}",
        open.x + open.width
    );
    let sent = click(view(), "Open");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Graph(GraphMsg::OverlayOpenImports(f)) if *f == file)),
        "{sent:?}"
    );
}

/// L9: what a language's server says in the status bar opens the page that
/// lists the servers — where what it says to do ("…, or choose where it
/// is") is done — and says so where the pointer rests. With no server to
/// speak of, the language's name opens nothing.
#[test]
fn a_servers_state_in_the_status_bar_opens_its_settings_page() {
    let mut app = reader_app();
    let click_status = |app: &App| {
        let mut sim = sim_of(app);
        let bounds = text_at(&mut sim, |t| t.starts_with("Rust"), 0)
            .expect("the file's language is in the status bar");
        sim.point_at(bounds.center());
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
    };
    let opens = |sent: &[Message]| {
        sent.iter().any(|m| {
            matches!(
                m,
                Message::Settings(crate::SettingsMsg::OpenAt(crate::SettingsPage::Languages))
            )
        })
    };
    let sent = click_status(&app);
    assert!(!opens(&sent), "a language with no server opened: {sent:?}");
    app.proj.link.lsp.insert(
        "rust".into(),
        crate::LspSlot::Unsupported(
            "'rust-analyzer' not found on PATH — install its toolchain, or choose where it is"
                .into(),
        ),
    );
    let sent = click_status(&app);
    assert!(opens(&sent), "{sent:?}");
}

/// L9: a provider of the user's own has no defaults for its empty fields to
/// stand for: they say what goes in them. Another provider's say what a
/// blank one means.
#[test]
fn a_custom_providers_fields_say_what_goes_in_them() {
    use crate::llm::Provider;
    let (model, endpoint) = super::connect::model_field_hints(Provider::Custom);
    assert_eq!(model, "The model's name, e.g. llama3.1");
    assert_eq!(endpoint, "Its endpoint, e.g. http://localhost:11434/v1");
    for provider in [Provider::Anthropic, Provider::OpenAI] {
        assert_eq!(
            super::connect::model_field_hints(provider),
            (provider.default_model(), provider.default_base_url())
        );
    }
}

/// L11: a heading in an answer, an explanation or a note stands a notch
/// over the prose it heads, as a chat's do. At iced's document scale an
/// answer's "## How it is used" was set at nearly twice its text in a card;
/// a markdown file read as a document keeps that scale.
#[test]
fn a_heading_in_prose_stands_a_notch_over_its_text() {
    use iced::advanced::{layout, widget::Tree};
    crate::fonts::load();
    let renderer = renderer();
    // How tall `md`, one line of it, is laid out with `settings`.
    let height = |md: &str, settings: iced::widget::markdown::Settings| {
        let items: Vec<iced::widget::markdown::Item> = iced::widget::markdown::parse(md).collect();
        let mut element: Element<'_, Message> = iced::widget::markdown::view(&items, settings)
            .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string())));
        let mut tree = Tree::new(element.as_widget());
        let limits = layout::Limits::new(Size::ZERO, Size::new(800.0, 10_000.0));
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits)
            .size()
            .height
    };
    let ratio = |settings: fn() -> iced::widget::markdown::Settings| {
        height("## How it is used", settings()) / height("The toolbar draws the field.", settings())
    };
    let prose = ratio(crate::theme::markdown_settings);
    assert!(
        prose > 1.05 && prose < 1.3,
        "a prose heading is {prose}× its text"
    );
    let document =
        ratio(|| crate::theme::document_markdown_settings_at(crate::theme::DOCUMENT_SIZE));
    assert!(
        document > 1.5,
        "a document's heading is {document}× its text"
    );
}

/// L12: what the Connect form says is wrong stands beside Connect, after
/// the fields: above them, it pushed the whole form down as it came.
#[test]
fn the_connect_forms_error_stands_beside_connect() {
    let app = reader_app();
    let form = crate::ConnectUi::default();
    let place = |error: Option<&str>, label: &'static str| {
        let mut sim = sim_elem(super::connect::connect_picker(&app, &form, error));
        text_at(&mut sim, move |t| t == label, 0)
            .unwrap_or_else(|| panic!("{label:?} is not on screen"))
    };
    let why = "Host and user are required.";
    assert_eq!(
        place(None, "Name (optional)").y,
        place(Some(why), "Name (optional)").y,
        "the form moved down for the error"
    );
    let said = place(Some(why), "Host and user are required.");
    let connect = place(Some(why), "Connect");
    let mut sim = sim_elem(super::connect::connect_picker(&app, &form, Some(why)));
    assert!(
        text_at(&mut sim, move |t| t == why, 1).is_none(),
        "the error is said twice"
    );
    assert!(
        (said.center_y() - connect.center_y()).abs() < 2.0,
        "{said:?} is not beside {connect:?}"
    );
    assert!(
        said.x + said.width <= connect.x,
        "{said:?} runs into {connect:?}"
    );
}

/// Reads the bounds of the scrollable `target`.
struct ScrollBoundsProbe {
    target: Id,
    found: Option<Rectangle>,
}

impl Operation for ScrollBoundsProbe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _content_bounds: Rectangle,
        _translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if id == Some(&self.target) {
            self.found = Some(bounds);
        }
    }
}

/// L14: the find bar stands over the code in a row of its own, and the
/// code starts under it: floated over the code, it hid the lines under it
/// — the file's first ones, and a match scrolled to the top of the view.
/// Its options are there, each saying what it is.
#[test]
fn the_find_bar_hides_no_code() {
    let mut app = reader_app();
    let mut renderer = renderer();
    let mut code_top = |app: &App| {
        let mut ui =
            UserInterface::build(super::view(app), WINDOW, Cache::default(), &mut renderer);
        let mut probe = ScrollBoundsProbe {
            target: super::code_scroll_id(0),
            found: None,
        };
        ui.operate(&renderer, &mut black_box(&mut probe));
        probe.found.expect("the code view is on screen").y
    };
    let closed = code_top(&app);
    app.proj.find.open = true;
    let opened = code_top(&app);
    let mut sim = sim_of(&app);
    let bar = text_at(&mut sim, |t| t == "Aa", 0).expect("Match case is on the bar");
    assert!(
        bar.y + bar.height <= opened,
        "the bar {bar:?} is over the code, which starts at {opened}"
    );
    assert!(opened > closed, "the code did not make room for the bar");
    assert!(shows(&mut sim, "W"));
    assert!(shows(&mut sim, ".*"));
    // Each option a click away.
    let sent = click(sim_of(&app), ".*");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Editor(EditorMsg::FindToggle(crate::SearchOpt::Regex))
        )),
        "{sent:?}"
    );
}

/// L15: the top bar's search field says what it searches, cut to the room
/// it has with an ellipsis: clipped at its edge, it stopped mid-word.
#[test]
fn the_search_fields_words_end_in_an_ellipsis_when_cut() {
    let label = "Search files, symbols and commands";
    let shown = |width: f32| {
        let mut app = reader_app();
        app.window_width = width;
        let mut sim = sim_sized(&app, Size::new(width, WINDOW.height));
        let found = std::sync::Arc::new(std::sync::Mutex::new(None));
        let into = found.clone();
        let _ = sim.find(move |c: iced_test::selector::Candidate<'_>| match c {
            iced_test::selector::Candidate::Text { content, .. }
                if content.starts_with("Search") =>
            {
                *into.lock().unwrap() = Some(content.to_string());
                Some(())
            }
            _ => None,
        });
        let shown = found.lock().unwrap().clone();
        shown.expect("the search field says what it searches")
    };
    assert_eq!(shown(WINDOW.width), label, "whole, where it fits");
    // A window whose field is too narrow for the words — found from the
    // field's own geometry, which is the platform's: the macOS bar has its
    // window buttons at the left and nothing at the right, and at 640 its
    // field still held the whole sentence.
    let narrow_window = (300..=WINDOW.width as u32)
        .step_by(10)
        .map(|w| w as f32)
        .find(|&w| {
            let mut app = reader_app();
            app.window_width = w;
            super::toolbar::search_field_rect(&app).width >= 200.0
        })
        .expect("a window with a 200 px field");
    let narrow = shown(narrow_window);
    assert!(narrow.ends_with('…'), "{narrow:?}");
    assert!(
        label.starts_with(narrow.trim_end_matches('…')),
        "{narrow:?}"
    );
}

/// L17: the Docs view with nothing in it offers to look again, not to
/// rebuild what was never built.
#[test]
fn an_empty_docs_view_offers_to_look_again() {
    let mut app = reader_app();
    app.proj.docs.files.clear();
    app.proj.docs.generation = 0;
    assert!(!app.docs_loading(), "precondition");
    {
        let mut sim = sim_elem(super::docs_stats::docs_tab(&app));
        assert!(shows(&mut sim, "No documentation"));
        assert!(!shows(&mut sim, "Rebuild"));
    }
    assert!(matches!(
        click(sim_elem(super::docs_stats::docs_tab(&app)), "Look again").as_slice(),
        [Message::Docs(DocsMsg::Refresh)]
    ));
    // A build that came back empty says so, not "No documentation" again.
    app.proj.docs.generation = 1;
    let mut sim = sim_elem(super::docs_stats::docs_tab(&app));
    assert!(shows(&mut sim, "Nothing documented"));
    assert!(shows(&mut sim, "Look again"));
}

/// M10 (round 6): working without clew-server, the Docs view and the
/// Glossary said "No documentation" / "No terms yet" on a documented project
/// and offered a Look again that did nothing. They say what they need and
/// why it is missing.
#[test]
fn docs_without_their_server_say_what_they_need() {
    let mut app = reader_app();
    app.proj.docs.files.clear();
    app.explain_cache_mut().clear();
    let why = "No clew-server binary was found beside Clew or on PATH; working without it.";
    app.handshake_failure = Some(why.into());
    {
        let mut sim = sim_elem(super::docs_stats::docs_tab(&app));
        assert!(shows(&mut sim, "API docs need clew-server"));
        assert!(shows(&mut sim, why));
        assert!(!shows(&mut sim, "Look again"));
    }
    let _ = app.update(Message::Glossary(GlossaryMsg::Open));
    let on_page = |app: &App, shown: &str| sim_of(app).find(shown).is_ok();
    assert!(on_page(&app, "The glossary needs clew-server"));
    assert!(on_page(&app, why));
    assert!(!on_page(&app, "Look again"));
}

/// L21: the View menu's row that is inert — blocks, with no function
/// explained — says why where the pointer rests.
#[test]
fn the_view_menus_inert_row_says_why() {
    let mut app = reader_app();
    app.show_view_menu = true;
    let mut sim = sim_of(&app);
    let row = text_at(&mut sim, |t| t == "Explanation in blocks", 0).expect("the row");
    let why = "Explain a function to read its explanation in blocks";
    assert!(!shows(&mut sim, why), "said before the pointer rests on it");
    super::tip::warm_now();
    let mut sim = sim_of(&app);
    sim.point_at(row.center());
    let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
        position: row.center(),
    })]);
    assert!(shows(&mut sim, why));
    // A row that does something says nothing of the kind.
    let minimap = text_at(&mut sim_of(&app), |t| t == "Minimap", 0).expect("the row");
    super::tip::warm_now();
    let mut sim = sim_of(&app);
    sim.point_at(minimap.center());
    let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
        position: minimap.center(),
    })]);
    assert!(!shows(&mut sim, why));
}

/// The header's folder tag is cut short with "…" where the tools leave
/// it too little room — never clipped under them mid-word ("test" for
/// "tests", in French, whose tools are longer).
#[test]
fn the_headers_folder_is_cut_with_an_ellipsis_not_clipped() {
    use crate::i18n::{Lang, with_lang};
    with_lang(Lang::Fr, || {
        let mut app = reader_app();
        app.show_left_sidebar = true;
        app.show_right_panel = true;
        app.proj.panes[0].as_mut().unwrap().rel = "tests/integration/test_pricing.py".into();
        let mut cut = false;
        for width in (760..=1280).rev().step_by(20) {
            app.window_width = width as f32;
            let mut sim = sim_of(&app);
            let Some(tools) = text_at(&mut sim, |t| t == "Expliquer", 0) else {
                continue;
            };
            let full = text_at(&mut sim, |t| t == "tests/integration", 0);
            let elided = text_at(
                &mut sim,
                |t| {
                    t.strip_prefix('…')
                        .is_some_and(|rest| "tests/integration".ends_with(rest))
                },
                0,
            );
            cut |= elided.is_some();
            let Some(folder) = full.or(elided) else {
                continue;
            };
            assert!(
                folder.x + folder.width <= tools.x,
                "{width}: the folder {folder:?} runs under the tools at {}",
                tools.x
            );
        }
        assert!(cut, "some width cut the folder short");
    });
}

/// A file's card has no way to a function's calls: the margin's Calls tab
/// follows the function the caret is in, and from a file's card — a
/// README's, a schema's — it said only to move the cursor into one.
#[test]
fn a_files_card_offers_no_calls() {
    let mut app = reader_app();
    app.show_right_panel = true;
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    app.proj.explain.cache.insert(
        crate::explain::Node::File(abs.clone()),
        crate::explain::Cached {
            summary: "What the whole file is for.".into(),
            prompt_hash: crate::incremental::content_hash(b"test"),
            detail: None,
            basis: None,
        },
    );
    app.proj.explain.view = Some(crate::explain::Node::File(abs));
    let mut sim = sim_of(&app);
    // The card is up: its History beside the margin's own tab of the name.
    assert!(
        text_at(&mut sim, |t| t == "History", 1).is_some(),
        "the card's History, after the tab's"
    );
    assert!(
        text_at(&mut sim, |t| t == "Calls", 0).is_some(),
        "the margin's own tab"
    );
    assert!(
        text_at(&mut sim, |t| t == "Calls", 1).is_none(),
        "and no second Calls, on the card"
    );
}

/// Two classes' `__init__` are two rows with notes of their own: each
/// row's understood mark is kept under its class, not shared — marking one
/// marked both.
#[test]
fn each_init_in_the_outline_has_a_note_of_its_own() {
    let mut app = reader_app();
    let symbol = |name: &str, kind: &str, line: usize, end_line: usize| clew_protocol::Symbol {
        name: name.into(),
        kind: kind.into(),
        line,
        end_line,
    };
    app.proj.panes[0].as_mut().unwrap().symbols = vec![
        symbol("OutOfStock", "class", 1, 4),
        symbol("__init__", "method", 2, 4),
        symbol("Inventory", "class", 6, 12),
        symbol("__init__", "method", 7, 9),
    ];
    let toggled = |nth: usize| {
        click_left_of(
            || sim_elem(super::outline_content(&app)),
            "__init__",
            nth,
            |m| matches!(m, Message::Reading(ReadingMsg::NoteToggleUnderstood { .. })),
        )
        .into_iter()
        .find_map(|m| match m {
            Message::Reading(ReadingMsg::NoteToggleUnderstood { symbol, .. }) => Some(symbol),
            _ => None,
        })
    };
    assert_eq!(toggled(0).as_deref(), Some("OutOfStock.__init__"));
    assert_eq!(toggled(1).as_deref(), Some("Inventory.__init__"));
}

/// A setting whose value is words — the interpreter in use — has them on
/// its label's line, not 7 above it.
#[test]
fn a_fields_words_sit_on_its_labels_line() {
    let mut sim = sim_elem(super::form::field_text(
        "In use",
        None,
        iced::widget::text("Chosen by the language server").size(super::ts::BASE),
    ));
    let label = text_at(&mut sim, |t| t == "In use", 0).expect("the label");
    let value = text_at(&mut sim, |t| t == "Chosen by the language server", 0).expect("the value");
    assert!(
        (label.center_y() - value.center_y()).abs() <= 1.5,
        "label {label:?}, value {value:?}"
    );
}

/// A field whose control starts with a checkbox or a line of chips has
/// their words on its label's line: "Features" sat 7 below "All features
/// of all packages", "Build tags" 4 below its first tag.
#[test]
fn a_fields_checkbox_and_chips_sit_on_its_labels_line() {
    let app = reader_app();
    let level = |sim: &mut Sim<'_>, label: &str, value: &str| {
        let l = text_at(sim, |t| t == label, 0).expect("the label");
        let v = text_at(sim, |t| t == value, 0).expect("the value");
        assert!(
            (l.center_y() - v.center_y()).abs() <= 1.5,
            "{label} {l:?}, {value} {v:?}"
        );
    };
    let rust = clew_protocol::RustOptions {
        packages: vec![clew_protocol::CargoPackage {
            name: "salvo".into(),
            root: true,
            features: vec!["acme".into()],
            default: Vec::new(),
        }],
        targets: Vec::new(),
        project_sets: Vec::new(),
    };
    let mut sim = sim_elem(super::connect::rust_block(&app, Some(&rust)));
    level(&mut sim, "Features", "All features of all packages");
    let go = clew_protocol::GoOptions {
        tags: vec!["integration".into()],
        ..Default::default()
    };
    let mut sim = sim_elem(super::connect::go_block(&app, Some(&go)));
    level(&mut sim, "Build tags", "integration");
    let mut sim = sim_elem(super::connect::go_block(&app, None));
    level(&mut sim, "Build tags", "No build tags in this project.");
}

/// A file's card keeps its actions where they are while its history is
/// read: "History" turned into the date once it came, and the longer date
/// sent "Ask" on to a second line, and back on the next file: a flash at
/// every file opened.
#[test]
fn a_file_cards_actions_stay_put_when_its_date_comes() {
    let mut app = reader_app();
    let abs = app.active_viewer().unwrap().abs.clone();
    app.show_right_panel = true;
    app.margin_tab = crate::MarginTab::Notes;
    app.active_viewer_mut().unwrap().caret = Some((0, 0));
    app.proj.explain.view = Some(crate::explain::Node::File(abs));
    let ask_at = |app: &App| {
        let mut sim = sim_of(app);
        text_at(&mut sim, |t| t == "Ask", 0).expect("the card's Ask")
    };
    let before = ask_at(&app);
    let line = |time: i64| clew_protocol::BlameLine {
        commit: "a".repeat(40),
        author: "Ada".into(),
        time,
        summary: "Read lines lazily".into(),
        uncommitted: false,
    };
    // A date of the widest kind: another year, a two-figure day.
    app.proj.panes[0].as_mut().unwrap().git = Some(Arc::new(clew_protocol::GitInfo {
        blame: vec![line(1_701_302_400); 400],
        status: Vec::new(),
        deleted_at: Default::default(),
    }));
    let after = ask_at(&app);
    assert_eq!((before.x, before.y), (after.x, after.y));
}

/// The find bar's ‹ and › go to the matches while there are some, and do
/// nothing while there are none — in grey then, as they were always drawn,
/// so that with 16 matches they looked switched off.
#[test]
fn the_find_bars_arrows_step_only_through_matches() {
    let mut app = reader_app();
    app.proj.find.open = true;
    app.proj.find.query = "value".into();
    let click_prev = |app: &App| {
        let mut sim = sim_elem(super::panes::find_bar(app));
        let count = text_at(&mut sim, |t| t.contains('/'), 0).expect("the count");
        // Past the count's 46 and the row's 6: the first arrow.
        let at = Point::new(count.x + 46.0 + 6.0 + 13.0, count.center_y());
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
            .into_iter()
            .filter(|m| matches!(m, Message::Editor(EditorMsg::FindStep(_))))
            .count()
    };
    assert_eq!(click_prev(&app), 0, "no match to go to");
    app.proj.find.matches = vec![(0, 18, 23), (1, 18, 23)];
    assert_eq!(click_prev(&app), 1, "a match to go to");
}

/// An import list's row names the file's folder; a file at the project's
/// root has none to name, and shows none — not a stray ".".
#[test]
fn a_root_files_import_row_names_no_folder() {
    let app = reader_app();
    let root = app.proj.project.as_ref().unwrap().root.clone();
    let row = |rel: &str| super::memo::RankedFile {
        path: root.join(rel),
        fan_in: 1,
        fan_out: 2,
    };
    let mut sim = sim_elem(super::graph::import_file_row(&app, &row("main.py")));
    assert!(shows(&mut sim, "main.py"));
    assert!(
        text_at(&mut sim, |t| t.trim() == ".", 0).is_none(),
        "no folder for a root file"
    );
    let mut sim = sim_elem(super::graph::import_file_row(&app, &row("shop/cart.py")));
    assert!(shows(&mut sim, "shop"), "a nested file's folder is named");
}

/// The header's file tools say what they do where the pointer rests:
/// Explain that it asks the AI, Skim what it folds, Diff what it compares
/// with — the names the menu gives them.
#[test]
fn the_file_tools_say_what_they_do() {
    let app = reader_app();
    for (label, says) in [
        ("Explain", "Explain this file with AI"),
        ("Skim", "Skim: fold every body to its signature"),
        ("Diff", "Changes since the last commit"),
    ] {
        let at = text_at(&mut sim_of(&app), |t| t == label, 0).expect(label);
        assert!(!shows(&mut sim_of(&app), says), "{label}: said at rest");
        super::tip::warm_now();
        let mut sim = sim_of(&app);
        sim.point_at(at.center());
        let _ = sim.simulate([Event::Mouse(iced::mouse::Event::CursorMoved {
            position: at.center(),
        })]);
        assert!(shows(&mut sim, says), "{label}");
    }
}

/// L16: the ☰ menu keeps its tools under a Tools heading of their own,
/// between View and Go — under View they were mixed in with zoom and
/// split — and, when it runs past a short window, its scroller shows at
/// rest, the one cue that more rows are below.
#[test]
fn the_app_menu_keeps_its_tools_apart_and_shows_it_scrolls() {
    let mut app = reader_app();
    app.show_app_menu = true;
    // Tall enough for every row to be drawn.
    app.window_height = 2400.0;
    let mut sim = sim_sized(&app, Size::new(WINDOW.width, app.window_height));
    let y = |sim: &mut Sim<'_>, label: &'static str| {
        text_at(sim, move |t| t == label, 0)
            .unwrap_or_else(|| panic!("{label:?} is not in the menu"))
            .y
    };
    let (view, tools, go) = (y(&mut sim, "View"), y(&mut sim, "Tools"), y(&mut sim, "Go"));
    assert!(view < tools && tools < go, "{view} {tools} {go}");
    // Rows named nowhere else on screen (the file's header has its own
    // Time Travel).
    for tool in [
        "Call Graph",
        "Project Graphs",
        "Explain All",
        "Start Debugging",
    ] {
        let at = y(&mut sim, tool);
        assert!(tools < at && at < go, "{tool} is not under Tools");
    }
    for viewing in ["Split View", "Zoom In"] {
        let at = y(&mut sim, viewing);
        assert!(view < at && at < tools, "{viewing} is not under View");
    }
    // At rest, the cue's scroller is drawn; the panels' overlay one is not.
    let rest = iced::widget::scrollable::Status::Active {
        is_horizontal_scrollbar_disabled: true,
        is_vertical_scrollbar_disabled: false,
    };
    let theme = iced::Theme::Dark;
    let drawn = |style: iced::widget::scrollable::Style| {
        !matches!(
            style.vertical_rail.scroller.background,
            iced::Background::Color(c) if c.a == 0.0
        )
    };
    assert!(drawn(crate::theme::cue_scrollbar(&theme, rest)));
    assert!(!drawn(crate::theme::overlay_scrollbar(&theme, rest)));
}

/// L23: the status bar's place — "Local" — opens Connect, and says so
/// before the pointer rests on it: a chevron after it, as the target picker
/// has, a click on which opens Connect too. The scan's state stands apart,
/// after the button: "Local · Up to date ⌄" read as a branch's sync, and a
/// click on it opened Connect.
#[test]
fn the_status_bars_place_shows_it_opens_something() {
    let app = reader_app();
    let mut sim = sim_of(&app);
    let place = text_at(&mut sim, |t| t == "Local", 0).expect("the place");
    let state = text_at(&mut sim, |t| t == "Up to date", 0).expect("the scan's state");
    let opens = |at: Point| {
        let mut sim = sim_of(&app);
        sim.point_at(at);
        let _ = sim.simulate(iced_test::simulator::click());
        sent_by(sim)
            .iter()
            .any(|m| matches!(m, Message::Connect(ConnectMsg::Open)))
    };
    // Past the place's words, where only the chevron is.
    let room = super::steady::widest(&["Local"], super::ts::SMALL, crate::fonts::UI);
    assert!(
        opens(Point::new(place.x + room + 12.0, place.center_y())),
        "nothing past its words"
    );
    assert!(
        state.x > place.x + room + 12.0,
        "the state is after the chevron"
    );
    assert!(!opens(state.center()), "the scan's state is no connection");
}

/// The status bar names the branch checked out, after the scan's state —
/// a long one cut short, a detached HEAD by its commit's short name — and
/// nothing outside a repository.
#[test]
fn the_status_bar_names_the_branch_checked_out() {
    use clew_protocol::HeadName;
    let mut app = reader_app();
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "main"), "no repository, no branch");
    }
    app.proj.head = Some(HeadName::Branch("main".into()));
    {
        let mut sim = sim_of(&app);
        let state = text_at(&mut sim, |t| t == "Up to date", 0).expect("the scan's state");
        let branch = text_at(&mut sim, |t| t == "main", 0).expect("the branch");
        assert!(branch.x > state.x, "after the scan's state");
        assert!(
            (branch.center_y() - state.center_y()).abs() < 2.0,
            "on its line"
        );
    }
    let long = "feature/reading-guide-with-every-file-in-order";
    app.proj.head = Some(HeadName::Branch(long.into()));
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, long), "cut short");
        let start: String = long.chars().take(27).collect();
        assert!(
            text_at(&mut sim, |t| t == format!("{start}…"), 0).is_some(),
            "its start, and a mark that it goes on"
        );
    }
    app.proj.head = Some(HeadName::Detached(
        "0123456789abcdef0123456789abcdef01234567".into(),
    ));
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "0123456"), "the commit, as git shortens it");
    assert!(!shows(&mut sim, "01234567"));
}

/// What Clew says with a name in it — "Connecting to `prod`…", the MCP
/// server's, the host key's command — shows the name in the reader's
/// quotation marks: the backticks the message was written with stood in
/// plain text as marks of their own.
#[test]
fn a_said_name_is_shown_in_quotation_marks_not_backticks() {
    let mut app = reader_app();
    app.status = "Opened `src/lib.rs`".into();
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Opened \u{201c}src/lib.rs\u{201d}"));
    assert!(!shows(&mut sim, "`"), "a backtick is on screen");
    // A setting's detail line, as the Language servers page and the
    // integrations write theirs.
    let row = super::form::item(
        "Server",
        Some("Run `cargo` first".to_string()),
        super::slot(),
    );
    let mut sim = sim_elem(row);
    assert!(shows(&mut sim, "Run \u{201c}cargo\u{201d} first"));
    assert!(!shows(&mut sim, "`"));
    // Connect's error, beside its button.
    let form = crate::ConnectUi::default();
    let mut sim = sim_elem(super::connect::connect_picker(
        &app,
        &form,
        Some("Could not reach `prod`"),
    ));
    assert!(shows(&mut sim, "Could not reach \u{201c}prod\u{201d}"));
    assert!(!shows(&mut sim, "`"));
    // Why a host's key is asked about again.
    let target = port_2222_target();
    let mut sim = sim_elem(super::connect::host_key_changed_prompt(
        &target,
        "The key `prod` offers has changed",
        "host",
        true,
    ));
    assert!(shows(&mut sim, "The key \u{201c}prod\u{201d} offers"));
    assert!(!shows(&mut sim, "`"));
    // What the integrations say, and what went wrong with them.
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Integrations,
    )));
    app.settings.integrations.note = Some("Added `files`".into());
    app.settings.integrations.errors = vec!["Could not start `git`".into()];
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Added \u{201c}files\u{201d}"));
    assert!(shows(&mut sim, "Could not start \u{201c}git\u{201d}"));
    assert!(!shows(&mut sim, "`"));
}

/// L20: "Why line N exists" opens where it was asked — under the line
/// right-clicked, a line's height clear of it, or over it when there is too
/// little room under it — not pinned near the window's top, far from the
/// line it is about. So does the language's own answer (a macro's
/// expansion).
#[test]
fn the_why_popup_opens_at_the_line_it_is_about() {
    let mut app = reader_app();
    let at = |app: &App, title: &'static str| {
        text_at(&mut sim_of(app), move |t| t == title, 0)
            .unwrap_or_else(|| panic!("{title:?} is not on screen"))
    };
    let why = |anchor| crate::app::model::BlameWhy {
        token: 1,
        title: "Why line 8 exists".into(),
        commits: vec![("0ec3696".into(), "fix the findings".into())],
        loading: true,
        prepared: Vec::new(),
        anchor,
    };
    let lh = app.line_height();
    app.proj.blame_why = Some(why(Some(400.0)));
    let under = at(&app, "Why line 8 exists");
    assert!(under.y > 400.0 + lh, "{under:?} is not under the line");
    assert!(
        under.y < 400.0 + lh + 60.0,
        "{under:?} is far from the line"
    );
    // Low in the window: over the line, its whole panel above it.
    let low = WINDOW.height - 60.0;
    app.proj.blame_why = Some(why(Some(low)));
    let over = at(&app, "Why line 8 exists");
    assert!(
        over.y + over.height < low - lh,
        "{over:?} is not over the line"
    );
    // Asked from no point (the palette): near the top, as before.
    app.proj.blame_why = Some(why(None));
    assert!(at(&app, "Why line 8 exists").y < 200.0);
    app.proj.blame_why = None;
    app.proj.insight = Some(crate::Insight {
        anchor: Some(400.0),
        token: 1,
        kind: crate::InsightKind::ExpandMacro,
        title: "Expansion of MAX".into(),
        loading: true,
        prepared: Vec::new(),
        defined: None,
    });
    let insight = at(&app, "Expansion of MAX");
    assert!(insight.y > 400.0 + lh, "{insight:?} is not under the line");
}

/// A fake debug session in the bottom panel: said to be simulated, where
/// it is in the trace, the line's values with what it changed, what the
/// run returns — and its controls step it.
#[test]
fn the_fake_debug_panel_shows_the_trace_and_steps_it() {
    use crate::fakerun::{Step, Target, Trace};
    let mut app = reader_app();
    let step = |line: usize, vars: &[(&str, &str)], note: &str| Step {
        file: "src/lib.rs".into(),
        line,
        function: "line_0".into(),
        depth: 0,
        note: note.into(),
        vars: vars
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
    };
    app.proj.fake.sim = Some(crate::app::fakerun::Sim {
        target: Target {
            name: "line_0".into(),
            kind: "function".into(),
            rel: "src/lib.rs".into(),
            line: 1,
            end_line: 1,
            lang: "rust".into(),
            source: "fn line_0() { let value = 0; }".into(),
        },
        given: Vec::new(),
        trace: Trace {
            returns: "()".into(),
            assumptions: vec!["nothing is called".into()],
            steps: vec![
                step(1, &[("value", "0")], "the call begins"),
                step(1, &[("value", "7")], "value is set"),
            ],
            ..Trace::default()
        },
        at: 1,
        ended: false,
        debug: true,
    });
    app.show_bottom = true;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Fake debug"));
    assert!(
        !shows(&mut sim, "simulated"),
        "no caption saying what Fake debug is"
    );
    assert!(shows(&mut sim, "step 2 of 2"));
    assert!(shows(&mut sim, "value is set"));
    assert!(shows(&mut sim, "7"));
    assert!(shows(&mut sim, "nothing is called"));
    let over = click(sim_of(&app), "⤼ Over");
    assert!(
        over.iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::Step(crate::SimCmd::Over)))),
        "{over:?}"
    );
    let back = click(sim_of(&app), "◀ Back");
    assert!(
        back.iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::Step(crate::SimCmd::Back)))),
        "{back:?}"
    );
    // Its line lit in the code as the debugger's is.
    let abs = app.proj.panes[0].as_ref().unwrap().abs.clone();
    assert_eq!(app.fake_line(&abs), Some(1));
}

/// The inputs sheet names the target and each parameter, and Run runs it.
#[test]
fn the_fake_run_sheet_asks_for_each_parameter() {
    use crate::fakerun::{Field, Target};
    let mut app = reader_app();
    let target = Target {
        name: "total".into(),
        kind: "function".into(),
        rel: "src/lib.rs".into(),
        line: 1,
        end_line: 1,
        lang: "rust".into(),
        source: "fn total(cart: &Cart, coupon: Option<&str>) -> f64 { 0.0 }".into(),
    };
    let fields = crate::fakerun::fields(&target);
    assert_eq!(fields.len(), 3);
    assert!(matches!(&fields[0], Field::Param { name, .. } if name == "cart"));
    app.proj.fake.sheet = Some(crate::app::fakerun::Sheet {
        values: vec![String::new(); fields.len()],
        fields,
        target,
        suggesting: None,
        debug: false,
    });
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Fake run"));
    assert!(shows(&mut sim, "total"));
    assert!(shows(&mut sim, "cart"));
    assert!(shows(&mut sim, "coupon"));
    assert!(shows(&mut sim, "Anything else it depends on"));
    assert!(shows(&mut sim, "Suggest inputs"));
    let run = click(sim_of(&app), "Run");
    assert!(
        run.iter()
            .any(|m| matches!(m, Message::Fake(crate::FakeMsg::Run))),
        "{run:?}"
    );
}

/// The keyboard shortcuts are no tool of the strip's, nor of its ⋯ menu:
/// they are in the app menu, with the window's other pages, and on ⌘/.
#[test]
fn the_shortcuts_are_in_the_app_menu_not_on_the_strip() {
    let mut app = reader_app();
    open_tools_menu(&mut app);
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Tutorial"), "the ⋯ menu is open");
        assert!(!shows(&mut sim, "Keyboard Shortcuts"));
    }
    app.show_tools_menu = false;
    app.show_app_menu = true;
    let picked = click(sim_of(&app), "Keyboard Shortcuts…");
    assert!(
        picked.iter().any(|m| matches!(
            m,
            Message::Editor(EditorMsg::RunAction(crate::keymap::Action::Shortcuts))
        )),
        "{picked:?}"
    );
}

/// The reading guide is offered first in the Walkthroughs view — written
/// when there is none, opened when there is — and, open, its steps are a
/// tree: each under the one it is read from.
#[test]
fn the_reading_guide_is_a_tree_in_the_walkthroughs_view() {
    use crate::walkthrough::{GUIDE_SCOPE, Step, Walkthrough};
    let mut app = reader_app();
    app.sidebar = crate::SidebarTab::Walk;
    app.show_left_sidebar = true;
    let label = "Where to start reading";
    let write = click(sim_elem(super::walk_tab(&app)), label);
    assert!(
        write
            .iter()
            .any(|m| matches!(m, Message::Walk(WalkMsg::Generate(s)) if s == GUIDE_SCOPE)),
        "{write:?}"
    );
    let step = |title: &str, depth: usize| Step {
        title: title.into(),
        file: "src/lib.rs".into(),
        symbol: None,
        line: Some(1),
        narration: "n".into(),
        depth,
    };
    app.proj.walk.library = vec![Walkthrough {
        title: "Where to start".into(),
        scope: GUIDE_SCOPE.into(),
        steps: vec![
            step("The entry point", 0),
            step("The router", 1),
            step("One handler", 2),
            step("The store", 1),
        ],
    }];
    let open = click(sim_elem(super::walk_tab(&app)), label);
    assert!(
        open.iter()
            .any(|m| matches!(m, Message::Walk(WalkMsg::Open(s)) if s == GUIDE_SCOPE)),
        "{open:?}"
    );
    app.proj.walk.open = Some(GUIDE_SCOPE.into());
    let mut sim = sim_elem(super::walk_tab(&app));
    assert!(shows(&mut sim, "Reading guide"));
    let x = |sim: &mut Sim<'_>, title: &str| {
        text_at(sim, |t| t == title, 0)
            .unwrap_or_else(|| panic!("{title} is drawn"))
            .x
    };
    let root = x(&mut sim, "The entry point");
    let child = x(&mut sim, "The router");
    let grandchild = x(&mut sim, "One handler");
    let sibling = x(&mut sim, "The store");
    assert!(
        root < child && child < grandchild,
        "{root} {child} {grandchild}"
    );
    assert_eq!(child, sibling, "siblings line up");
}

/// The MCP registry in Settings: what the servers are for (Clew's own
/// agent, not an ACP agent's), each found server with how it runs and Use,
/// and — once used — what it reads that the form leaves to fill in.
#[test]
fn the_mcp_registry_is_searched_and_used_from_settings() {
    use crate::IntegrationsMsg as M;
    let mut app = reader_app();
    app.settings.open = true;
    app.settings.page = crate::SettingsPage::Integrations;
    let entries = clew_core::mcp::registry::parse(
        r#"{"servers": [
          {"server": {"name": "io.github.acme/notes-mcp", "description": "Your notes.",
            "version": "1.2.0", "packages": [{"registryType": "npm",
              "identifier": "@acme/notes", "version": "1.2.0",
              "environmentVariables": [{"name": "NOTES_TOKEN", "description": "An API token",
                "isRequired": true, "isSecret": true}]}]}},
          {"server": {"name": "com.example/bundle", "version": "1",
            "packages": [{"registryType": "mcpb", "identifier": "x"}]}}]}"#,
    )
    .unwrap();
    app.settings.integrations.registry.query = "notes".into();
    app.settings.integrations.registry.found = Some(Ok(entries));
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Find in the MCP registry"));
        assert!(
            !shows(&mut sim, "tools for Clew's own agent"),
            "no paragraph saying what the servers are for"
        );
        assert!(shows(&mut sim, "io.github.acme/notes-mcp"));
        assert!(shows(&mut sim, "npm 1.2.0"));
        assert!(shows(&mut sim, "Clew can't run it"));
    }
    let used = click(sim_of(&app), "Use");
    assert!(
        used.iter()
            .any(|m| matches!(m, Message::Integrations(M::RegistryUse(0)))),
        "{used:?}"
    );
    let _ = app.update(Message::Integrations(M::RegistryUse(0)));
    let draft = &app.settings.integrations;
    assert_eq!(draft.new_mcp_name, "notes");
    assert_eq!(draft.new_mcp_target, "npx -y @acme/notes@1.2.0");
    assert_eq!(draft.new_mcp_extra, "NOTES_TOKEN=");
    {
        // The results put away, the add form is in view.
        let found = app.settings.integrations.registry.found.take();
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "NOTES_TOKEN (required, secret)"));
        assert!(shows(&mut sim, "An API token"));
        drop(sim);
        app.settings.integrations.registry.found = found;
    }
    // An entry Clew cannot run fills nothing.
    let _ = app.update(Message::Integrations(M::RegistryUse(1)));
    assert_eq!(app.settings.integrations.new_mcp_name, "notes");

    // Add server asks for something to add, and says beside itself why an
    // Add did not add — not at the top of the page, 400 px away.
    app.settings.integrations.registry.found = None;
    app.settings.integrations.new_mcp_target.clear();
    assert!(click(sim_tall(super::view(&app)), "Add server").is_empty());
    app.settings.integrations.new_mcp_target = "npx -y x".into();
    assert!(matches!(
        click(sim_tall(super::view(&app)), "Add server").as_slice(),
        [Message::Integrations(M::McpAdd)]
    ));
    app.settings.integrations.mcp_add_error = Some("give the server a name".into());
    assert!(shows(
        &mut sim_tall(super::view(&app)),
        "Not added: give the server a name."
    ));
}

/// A search runs off the window's thread and only the newest answer is
/// taken; an empty query searches nothing.
#[test]
fn only_the_newest_registry_search_is_taken() {
    use crate::IntegrationsMsg as M;
    let mut app = reader_app();
    let _ = app.update(Message::Integrations(M::RegistrySearch));
    assert_eq!(app.settings.integrations.registry.searching, None);
    let _ = app.update(Message::Integrations(M::RegistryQuery("git".into())));
    let _ = app.update(Message::Integrations(M::RegistrySearch));
    let first = app
        .settings
        .integrations
        .registry
        .searching
        .expect("searching");
    let _ = app.update(Message::Integrations(M::RegistrySearch));
    let second = app.settings.integrations.registry.searching.unwrap();
    assert_ne!(first, second);
    let _ = app.update(Message::Integrations(M::RegistryFound {
        seq: first,
        result: Err("late".into()),
    }));
    assert!(
        app.settings.integrations.registry.found.is_none(),
        "the older one"
    );
    let _ = app.update(Message::Integrations(M::RegistryFound {
        seq: second,
        result: Ok(Vec::new()),
    }));
    assert_eq!(app.settings.integrations.registry.searching, None);
    assert!(matches!(
        app.settings.integrations.registry.found,
        Some(Ok(ref v)) if v.is_empty()
    ));
}

/// Every state of the window the audit below reads: the code with each of
/// the sidebar's views, the margin's tabs and the bottom panel, Ask open
/// and large, the menus, each page of Settings, and the pages over the
/// code.
fn audited_states() -> Vec<(String, App)> {
    let mut states = Vec::new();
    let base = reader_app();
    states.push(("the code".to_string(), base_clone(&base)));
    for (_, tab) in crate::ui::SIDEBAR_TABS {
        let mut app = base_clone(&base);
        app.show_left_sidebar = true;
        app.sidebar = tab;
        states.push((format!("the sidebar's {tab:?}"), app));
    }
    for tab in [
        crate::MarginTab::Notes,
        crate::MarginTab::Calls,
        crate::MarginTab::History,
    ] {
        let mut app = base_clone(&base);
        app.show_right_panel = true;
        app.margin_tab = tab;
        states.push((format!("the margin's {tab:?}"), app));
    }
    let mut app = base_clone(&base);
    app.show_bottom = true;
    states.push(("the bottom panel".into(), app));
    for (name, message) in [
        ("Ask", Message::Ask(AskMsg::Toggle)),
        (
            "the tools menu",
            Message::Window(WindowMsg::ToggleToolsMenu),
        ),
        ("the overview", Message::Overview(crate::OverviewMsg::Show)),
        (
            "the stats",
            Message::Overview(crate::OverviewMsg::ShowStats),
        ),
        ("the weak spots", Message::Weak(crate::WeakMsg::Show)),
        ("the command bar", Message::Nav(NavMsg::PaletteOpened)),
    ] {
        let mut app = base_clone(&base);
        let _ = app.update(message);
        states.push((name.to_string(), app));
    }
    // The bug's Run menu is there only where the strip is too short for
    // the four ways to run.
    let mut app = base_clone(&base);
    app.window_height = 540.0;
    let _ = app.update(Message::Window(WindowMsg::ToggleRunMenu));
    assert!(app.show_run_menu);
    states.push(("the run menu, the window short".into(), app));
    let mut app = base_clone(&base);
    let _ = app.update(Message::Ask(AskMsg::Toggle));
    app.ask_view = crate::AskView::Expanded;
    states.push(("Ask, large".into(), app));
    for page in crate::SettingsPage::ALL {
        let mut app = base_clone(&base);
        let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(page)));
        states.push((format!("Settings, {page:?}"), app));
    }
    // What each command opens — but those that start a program, the
    // microphone or a paid pass, or put codes on the window.
    use crate::keymap::Action;
    for action in Action::ALL {
        if matches!(
            action,
            Action::Run
                | Action::StartDebug
                | Action::ExplainAll
                | Action::Voice
                | Action::FakeRun
                | Action::FakeDebug
                | Action::Jump
        ) {
            continue;
        }
        let mut app = base_clone(&base);
        let _ = app.update(Message::Editor(EditorMsg::RunAction(action)));
        states.push((format!("{action:?}"), app));
    }
    // The sidebar's More views menu.
    let mut app = base_clone(&base);
    app.show_left_sidebar = true;
    let _ = app.update(Message::Window(WindowMsg::ToggleViewsMenu));
    states.push(("the views menu".into(), app));
    // The context menu over the code, and a tree with rows.
    let mut app = base_clone(&base);
    app.proj.context_menu = Some(crate::ContextMenu {
        pane: 0,
        line: 1,
        col: 1,
        x: 20.0,
        y: 20.0,
        insight: None,
        served: true,
        by_pointer: true,
    });
    states.push(("the context menu".into(), app));
    let mut app = base_clone(&base);
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        files: vec!["README.md".into()],
        dirs: vec![(
            "src".into(),
            crate::fs_scan::DirNode {
                dirs: vec![],
                files: vec!["lib.rs".into(), "main.rs".into()],
            },
        )],
    };
    app.proj.expanded.insert("src".into());
    states.push(("a tree".into(), app));
    states
}

/// [`reader_app`] again: an `App` is not `Clone`.
fn base_clone(_base: &App) -> App {
    reader_app()
}

/// Every button a click presses offers itself to voice and to the
/// command bar too (`heard`): in every state audited, each press a button
/// marks for jump mode is one voice can make by a word. A button built
/// without a word for it fails here.
#[test]
fn every_button_a_click_presses_a_word_presses_too() {
    let mut unsaid = Vec::new();
    for (name, app) in audited_states() {
        let (targets, marks) = crate::ui::heard::gather_all(&app);
        assert!(!marks.is_empty(), "{name}: nothing marked");
        let offered: std::collections::HashSet<String> =
            targets.iter().map(|t| format!("{:?}", t.message)).collect();
        for (mark, _) in marks {
            if !offered.contains(&mark) {
                unsaid.push(format!("{name}: {mark}"));
            }
        }
    }
    unsaid.dedup();
    assert!(
        unsaid.is_empty(),
        "pressed by a click, by no word:\n{}",
        unsaid.join("\n")
    );
}

/// macOS's window buttons — drawn only there, so the states above, built on
/// any system, show them only on a Mac — are pressed by a word too: "close",
/// "minimize", "full screen", or "exit full screen" once it is.
#[test]
fn the_window_buttons_of_macos_are_pressed_by_a_word_too() {
    let mut app = reader_app();
    for fullscreen in [false, true] {
        app.fullscreen = fullscreen;
        let (targets, marks) =
            crate::ui::heard::gather_all_of(|| crate::ui::toolbar::traffic_lights(&app));
        assert_eq!(marks.len(), 3, "{marks:?}");
        for (mark, _) in &marks {
            assert!(
                targets.iter().any(|t| &format!("{:?}", t.message) == mark),
                "{mark}: pressed by a click, by no word"
            );
        }
        let shown: Vec<&str> = targets.iter().map(|t| t.shown.as_str()).collect();
        let full = if fullscreen {
            "Exit Full Screen"
        } else {
            "Full Screen"
        };
        assert_eq!(shown, ["Close", "Minimize", full]);
    }
}

/// The window drawn once, as the runtime draws it: the codes jump mode
/// deals go out as it is.
fn draw_window(app: &App) {
    let mut renderer = renderer();
    let mut ui = UserInterface::build(super::view(app), WINDOW, Cache::default(), &mut renderer);
    let _ = ui.update(
        &redraw(),
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut Vec::new(),
    );
    ui.draw(
        &mut renderer,
        &crate::theme::app_theme(),
        &iced::advanced::renderer::Style {
            text_color: iced::Color::BLACK,
        },
        mouse::Cursor::Unavailable,
    );
}

/// A key typed as the window gets it.
fn type_key(app: &mut App, key: keyboard::Key, modifiers: keyboard::Modifiers) {
    let _ = app.handle_key(key, modifiers);
}

fn type_letters(app: &mut App, letters: &str) {
    for c in letters.chars() {
        type_key(
            app,
            keyboard::Key::Character(c.to_string().into()),
            keyboard::Modifiers::empty(),
        );
    }
}

/// ⌘J puts a code on every control on screen — the sidebar's rows, the
/// strip's tools, the bar's buttons — one each, all as long; typing a
/// tool's code presses it as a click does, and the codes go.
#[test]
fn jump_codes_go_on_every_control_and_a_code_presses_it() {
    let (_, mut app) = audited_states()
        .into_iter()
        .find(|(name, _)| name == "a tree")
        .expect("the tree state");
    let cmd_j = keyboard::Modifiers::COMMAND;
    type_key(&mut app, keyboard::Key::Character("j".into()), cmd_j);
    assert_eq!(app.jump.as_deref(), Some(""), "{}", app.status);
    draw_window(&app);
    let dealt = crate::ui::jump::dealt(app.main_window);
    assert!(dealt.len() > 26, "{} codes", dealt.len());
    let codes: std::collections::HashSet<&str> = dealt.iter().map(|(c, _)| c.as_str()).collect();
    assert_eq!(codes.len(), dealt.len(), "each code is another");
    assert!(dealt.iter().all(|(c, _)| c.len() == 2), "all two letters");
    // A file's row in the tree has one.
    assert!(
        dealt
            .iter()
            .any(|(_, m)| format!("{m:?}").contains("main.rs")),
        "the tree's rows"
    );
    let (code, _) = dealt
        .iter()
        .find(|(_, m)| matches!(m, Message::Weak(crate::WeakMsg::Show)))
        .expect("the strip's Weak Spots");
    let code = code.clone();
    type_letters(&mut app, &code[..1]);
    assert_eq!(app.jump.as_deref(), Some(&code[..1]), "waits for the rest");
    type_letters(&mut app, &code[1..]);
    assert!(app.proj.weak.showing, "pressed");
    assert!(app.jump.is_none(), "the codes go");
    // Drawn without them, none is dealt.
    draw_window(&app);
    assert!(crate::ui::jump::dealt(app.main_window).is_empty());
}

/// Each window keeps its own codes: another window drawn after it, without
/// codes or with its own, neither puts them away nor gives this one its.
#[test]
fn jump_codes_are_each_windows_own() {
    let mut first = reader_app();
    first.main_window = Some(iced::window::Id::unique());
    let mut second = reader_app();
    second.main_window = Some(iced::window::Id::unique());
    let cmd_j = keyboard::Modifiers::COMMAND;
    type_key(&mut first, keyboard::Key::Character("j".into()), cmd_j);
    draw_window(&first);
    // The codes and what each presses, as words to compare.
    let codes = |app: &App| -> Vec<String> {
        crate::ui::jump::dealt(app.main_window)
            .iter()
            .map(|(code, m)| format!("{code} {m:?}"))
            .collect()
    };
    let dealt = codes(&first);
    let (code, _) = crate::ui::jump::dealt(first.main_window)
        .into_iter()
        .find(|(_, m)| matches!(m, Message::Weak(crate::WeakMsg::Show)))
        .expect("the strip's Weak Spots");
    // The other window, drawn since without codes, leaves the first's.
    draw_window(&second);
    assert_eq!(codes(&first), dealt);
    // With codes of its own, it keeps them to itself.
    second.show_left_sidebar = true;
    type_key(&mut second, keyboard::Key::Character("j".into()), cmd_j);
    draw_window(&second);
    assert_ne!(codes(&second), dealt);
    assert_eq!(codes(&first), dealt);
    type_letters(&mut first, &code);
    assert!(first.proj.weak.showing, "pressed in the first window");
    assert!(!second.proj.weak.showing);
    assert!(first.jump.is_none(), "the codes go");
    draw_window(&first);
    assert!(crate::ui::jump::dealt(first.main_window).is_empty());
    assert!(!crate::ui::jump::dealt(second.main_window).is_empty());
}

/// Backspace takes a letter back; a letter no code goes on with, or Esc,
/// puts the codes away and presses nothing.
#[test]
fn jump_mode_takes_letters_back_and_ends_on_a_wrong_one() {
    let mut app = reader_app();
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::Jump,
    )));
    draw_window(&app);
    let dealt = crate::ui::jump::dealt(app.main_window);
    let first = dealt[0].0.clone();
    type_letters(&mut app, &first[..1]);
    type_key(
        &mut app,
        keyboard::Key::Named(keyboard::key::Named::Backspace),
        keyboard::Modifiers::empty(),
    );
    assert_eq!(app.jump.as_deref(), Some(""));
    // A letter no code begins with.
    let unused = ('a'..='z')
        .find(|c| !dealt.iter().any(|(code, _)| code.starts_with(*c)))
        .expect("a letter no code begins with");
    type_letters(&mut app, &unused.to_string());
    assert!(app.jump.is_none());
    assert!(
        app.status
            .contains(&unused.to_ascii_uppercase().to_string()),
        "{}",
        app.status
    );
    // Esc puts them away too; ⌘J again as well.
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::Jump,
    )));
    type_key(
        &mut app,
        keyboard::Key::Named(keyboard::key::Named::Escape),
        keyboard::Modifiers::empty(),
    );
    assert!(app.jump.is_none());
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::Jump,
    )));
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::Jump,
    )));
    assert!(app.jump.is_none());
}

/// A dialog over the window takes the codes as it takes the pointer:
/// only its own controls have one.
#[test]
fn a_dialog_takes_the_codes_from_what_is_under_it() {
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Shortcuts,
    )));
    let _ = app.update(Message::Editor(EditorMsg::RunAction(
        crate::keymap::Action::Jump,
    )));
    draw_window(&app);
    let dealt = crate::ui::jump::dealt(app.main_window);
    assert!(
        dealt
            .iter()
            .any(|(_, m)| matches!(m, Message::Window(WindowMsg::RebindStart(_)))),
        "the dialog's own"
    );
    assert!(
        !dealt
            .iter()
            .any(|(_, m)| matches!(m, Message::Weak(crate::WeakMsg::Show))),
        "nothing under it"
    );
    // ⌘J typed on that page, which keeps every other key from what is
    // under it, puts the codes up as well.
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Shortcuts,
    )));
    type_key(
        &mut app,
        keyboard::Key::Character("j".into()),
        keyboard::Modifiers::COMMAND,
    );
    assert!(app.jump.is_some());
}

/// The chat F6 opened on its way round is put away again as F6 goes on,
/// nothing typed in it — not left over the code — and kept, a question
/// begun in it. One open before F6 came is left open.
#[test]
fn f6_puts_away_the_chat_it_opened() {
    let mut app = reader_app();
    app.code_focused = true;
    let f6 = || keyboard::Key::Named(keyboard::key::Named::F6);
    let to_chat = |app: &mut App| {
        for _ in 0..6 {
            if app.region == crate::app::keys::Region::Chat {
                return;
            }
            type_key(app, f6(), keyboard::Modifiers::empty());
        }
        panic!("F6 never reached the chat");
    };
    to_chat(&mut app);
    assert!(app.ask_view.is_open(), "shown to type in");
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert!(!app.ask_view.is_open(), "put away, nothing typed");
    // A question begun: kept.
    to_chat(&mut app);
    app.proj.ask_input = "why".into();
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert!(app.ask_view.is_open(), "kept, a question begun in it");
    // Open before F6 came: left open.
    app.proj.ask_input.clear();
    to_chat(&mut app);
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert!(app.ask_view.is_open(), "it was open before");
}

/// F6 moves the keys from the code to the sidebar's field, to the chat
/// and back to the code; Shift-F6 the other way.
#[test]
fn f6_moves_the_keys_between_the_regions() {
    let mut app = reader_app();
    // A tree with a row to rest on: an empty one is passed over.
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        dirs: vec![],
        files: vec!["main.py".into()],
    };
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.code_focused = true;
    let f6 = || keyboard::Key::Named(keyboard::key::Named::F6);
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert_eq!(app.region, crate::app::keys::Region::Sidebar);
    assert!(!app.code_focused);
    // The Files view's tree, under its filter.
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert_eq!(app.region, crate::app::keys::Region::Tree);
    assert!(!app.code_focused);
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert_eq!(app.region, crate::app::keys::Region::Chat);
    assert!(app.ask_view.is_open(), "the chat shown to type in");
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert_eq!(app.region, crate::app::keys::Region::Code);
    assert!(app.code_focused);
    type_key(&mut app, f6(), keyboard::Modifiers::SHIFT);
    assert_eq!(app.region, crate::app::keys::Region::Chat);
    // A sidebar view with no field is passed over.
    app.sidebar = crate::SidebarTab::Marks;
    app.code_focused = true;
    type_key(&mut app, f6(), keyboard::Modifiers::empty());
    assert_eq!(app.region, crate::app::keys::Region::Chat);
}

/// The command bar lists what is on screen under it by name — a control
/// no command names — and runs it for the reader; the agent, running
/// commands by title, never reaches it.
#[test]
fn the_command_bar_lists_what_is_on_screen_for_the_reader_only() {
    let mut app = reader_app();
    app.show_bottom = true;
    let _ = app.update(Message::Nav(NavMsg::PaletteOpened));
    app.refresh_bar();
    let hide = app
        .palette
        .iter()
        .find(|c| c.title == "Hide panel")
        .expect("the panel's Hide is listed")
        .title
        .clone();
    assert!(
        !app.palette_commands().iter().any(|c| c.title == hide),
        "no command of the agent's"
    );
    let _ = app.run_palette(&hide);
    assert!(app.show_bottom, "the agent's way runs nothing");
    let _ = app.update(Message::Nav(NavMsg::PaletteOpened));
    let _ = app.update(Message::Nav(NavMsg::PaletteRun(hide)));
    assert!(!app.show_bottom, "the reader's does");
    assert!(!app.proj.finder.open);
}

/// A file the tree shows is listed once in the command bar — as the file —
/// not a second time above it as a control on screen.
#[test]
fn a_file_on_screen_is_listed_once_in_the_command_bar() {
    let mut app = reader_app();
    let project = app.proj.project.as_mut().unwrap();
    let root = project.root.clone();
    project.tree = crate::fs_scan::DirNode {
        dirs: vec![],
        files: vec!["main.py".into()],
    };
    project.files = Arc::new(vec![crate::fs_scan::FileEntry {
        abs: root.join("main.py"),
        rel: "main.py".into(),
    }]);
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    assert!(
        crate::ui::heard::gather(&app)
            .iter()
            .any(|t| t.title == "main.py"),
        "the tree's row is on screen"
    );
    let _ = app.update(Message::Nav(NavMsg::PaletteOpened));
    app.proj.finder.query = "main".into();
    app.refresh_bar();
    let n = app.palette.len();
    assert!(
        !app.palette.iter().any(|c| c.title == "main.py"),
        "not as a command"
    );
    let files: Vec<usize> = app
        .proj
        .finder
        .results
        .iter()
        .filter(|&&i| i >= n)
        .copied()
        .collect();
    assert_eq!(files, [n], "once, as the file");
}

/// Only what is on screen has a code: a long tree's rows scrolled away
/// below the window have none, and the codes stay two letters long.
#[test]
fn rows_scrolled_away_have_no_code() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::Files;
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        files: vec![],
        dirs: vec![(
            "src".into(),
            crate::fs_scan::DirNode {
                dirs: vec![],
                files: (0..800).map(|i| format!("f{i}.rs")).collect(),
            },
        )],
    };
    app.proj.expanded.insert("src".into());
    app.jump = Some(String::new());
    draw_window(&app);
    let dealt = crate::ui::jump::dealt(app.main_window);
    let rows = |name: &str| {
        dealt
            .iter()
            .any(|(_, m)| format!("{m:?}").contains(&format!("src/{name}\"")))
    };
    assert!(rows("f0.rs"), "the first row, on screen");
    assert!(!rows("f799.rs"), "the last, far below, has none");
    assert!(dealt.len() < 200, "{} codes", dealt.len());
    assert!(dealt.iter().all(|(c, _)| c.len() == 2));
}

/// The sidebar's ⋯ lists the views not on its strip — a function's calls
/// among them — and picking one shows it, the menu put away, the ⋯ raised
/// while it shows; Esc puts the menu away too.
#[test]
fn the_more_views_menu_lists_the_views_off_the_strip() {
    let mut app = reader_app();
    app.show_left_sidebar = true;
    let more = crate::ui::heard::gather(&app)
        .into_iter()
        .find(|t| t.title == "More views")
        .expect("the ⋯ is offered");
    let _ = app.update(more.message);
    assert!(app.show_views_menu);
    let mut sim = sim_of(&app);
    for name in [
        "Call hierarchy",
        "Value flow",
        "Imports",
        "Your notes",
        "API docs",
    ] {
        assert!(shows(&mut sim, name), "{name} listed");
    }
    assert!(!shows(&mut sim, "Bookmarks"), "a view on the strip is not");
    drop(sim);
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Window(WindowMsg::ToggleViewsMenu))
    ));
    let sent = click(sim_of(&app), "Call hierarchy");
    for message in sent {
        let _ = app.update(message);
    }
    assert_eq!(app.sidebar, crate::SidebarTab::Calls);
    assert!(!app.show_views_menu, "put away");
    // Its menu hangs under the ⋯, within the sidebar.
    let _ = app.update(Message::Window(WindowMsg::ToggleViewsMenu));
    let mut sim = sim_of(&app);
    let row = text_at(&mut sim, |t| t == "API docs", 0).expect("listed");
    assert!(row.x + row.width <= app.sidebar_width, "{row:?}");
    assert!(row.y > super::toolbar::TOOLBAR_H + 30.0, "{row:?}");
}

/// The window drawn at `size` with the pointer at `at` (moved there first),
/// as RGBA, and the messages the move gave.
fn window_with_pointer(
    app: &App,
    size: iced::Size,
    at: Option<iced::Point>,
    then: &[Event],
) -> (Vec<u8>, Vec<Message>) {
    use iced::advanced::renderer::Headless as _;
    let cursor = at.map_or(mouse::Cursor::Unavailable, mouse::Cursor::Available);
    let mut renderer = renderer();
    let mut ui = UserInterface::build(crate::ui::view(app), size, Cache::default(), &mut renderer);
    let mut events = vec![Event::Window(window::Event::RedrawRequested(
        std::time::Instant::now(),
    ))];
    if let Some(p) = at {
        events.push(Event::Mouse(mouse::Event::CursorMoved { position: p }));
    }
    events.extend_from_slice(then);
    let mut messages = Vec::new();
    let _ = ui.update(
        &events,
        cursor,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    ui.draw(
        &mut renderer,
        &crate::theme::app_theme(),
        &iced::advanced::renderer::Style {
            text_color: iced::Color::BLACK,
        },
        cursor,
    );
    let rgba = renderer.screenshot(
        iced::Size::new(size.width as u32, size.height as u32),
        1.0,
        iced::Color::TRANSPARENT,
    );
    (rgba, messages)
}

/// A reader with the sidebar open beside the sheet, in a window of `size`.
fn sidebar_app(size: iced::Size) -> App {
    let mut app = reader_app();
    let _ = app.on_window_resized(size);
    app.show_left_sidebar = true;
    app
}

/// The sheet's shadow fades out into the sidebar as it does along its other
/// edges, getting no darker away from the sheet: the sidebar's divider,
/// painted in the sidebar's colour at rest, stopped it short a few pixels
/// out, leaving a dark line down the sidebar's edge.
#[test]
fn the_sheets_shadow_fades_into_the_sidebar_without_a_line() {
    let size = iced::Size::new(1000.0, 640.0);
    let app = sidebar_app(size);
    let (rgba, _) = window_with_pointer(&app, size, None, &[]);
    let w = size.width as usize;
    let light = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        u32::from(rgba[i]) + u32::from(rgba[i + 1]) + u32::from(rgba[i + 2])
    };
    // The sheet's border: the first pixel past the sidebar's that is not
    // the chrome's.
    let edge = (app.sidebar_width + app.window_frame) as usize;
    for y in [250, 400, 550] {
        // Walking out from the sheet into the empty sidebar, each pixel is
        // as light as the one before it or lighter.
        let row: Vec<u32> = (edge - 30..edge).rev().map(|x| light(x, y)).collect();
        assert!(
            row.windows(2).all(|p| p[1] >= p[0]),
            "y = {y}: going out from the sheet: {row:?}"
        );
    }
}

/// The sidebar's lists keep their scrollbar: with the pointer over a long
/// file tree its scroller shows down the sidebar's edge, where the divider
/// lay over it and hid it.
#[test]
fn the_file_trees_scrollbar_shows_beside_the_sidebars_divider() {
    let size = iced::Size::new(1000.0, 640.0);
    let mut app = sidebar_app(size);
    app.proj.project.as_mut().unwrap().tree = crate::fs_scan::DirNode {
        dirs: Vec::new(),
        files: (0..80).map(|i| format!("file_{i:02}.rs")).collect(),
    };
    let w = size.width as usize;
    let edge = (app.sidebar_width + app.window_frame) as usize;
    let lightest = |rgba: &[u8]| {
        (edge - crate::ui::SCROLLBAR_W as usize..edge)
            .flat_map(|x| (150..600).map(move |y| (x, y)))
            .map(|(x, y)| u32::from(rgba[(y * w + x) * 4]))
            .max()
            .unwrap()
    };
    let (away, _) = window_with_pointer(&app, size, None, &[]);
    let over = iced::Point::new(app.window_frame + 120.0, 300.0);
    let (hovered, _) = window_with_pointer(&app, size, Some(over), &[]);
    // Unseen at rest; seen, well over the chrome, under the pointer.
    assert!(lightest(&away) < 30, "{}", lightest(&away));
    assert!(
        lightest(&hovered) > lightest(&away) + 15,
        "{}",
        lightest(&hovered)
    );
}

/// The sidebar's divider lies over the sheet's edge, but ends before the
/// code's gutter marks: a click anywhere on a breakpoint's dot clears it,
/// and doesn't grab the divider.
#[test]
fn a_click_on_a_breakpoints_dot_is_not_taken_by_the_sidebars_divider() {
    let size = iced::Size::new(1000.0, 640.0);
    let app = sidebar_app(size);
    let edge = app.window_frame + app.sidebar_width;
    // A dot starts past the sheet's 1px border and the bookmark ribbon's
    // room, 4.5px in at the least; its left side is the first place a
    // click aims.
    for x in [edge + 5.0, edge + 8.0] {
        let at = iced::Point::new(x, 300.0);
        let mut renderer = renderer();
        let mut ui =
            UserInterface::build(crate::ui::view(&app), size, Cache::default(), &mut renderer);
        let mut messages = Vec::new();
        for event in [
            mouse::Event::CursorMoved { position: at },
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            let _ = ui.update(
                &[Event::Mouse(event)],
                mouse::Cursor::Available(at),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
        }
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Debug(DebugMsg::BreakpointToggle { .. }))),
            "x = {x}: {messages:?}"
        );
    }
}

/// Dragging the divider beside the sidebar makes the sidebar reach the
/// pointer, less half the divider: grabbed by its middle, it stays put.
#[test]
fn dragging_the_sidebars_divider_moves_its_edge_with_the_pointer() {
    let size = iced::Size::new(1000.0, 640.0);
    let app = sidebar_app(size);
    let half = crate::resize::EDGE_GRIP / 2.0;
    let grab = iced::Point::new(app.window_frame + app.sidebar_width + half, 300.0);
    let to = iced::Point::new(grab.x + 40.0, 300.0);
    let mut renderer = renderer();
    let mut ui = UserInterface::build(crate::ui::view(&app), size, Cache::default(), &mut renderer);
    let mut messages = Vec::new();
    for (at, event) in [
        (grab, mouse::Event::CursorMoved { position: grab }),
        (grab, mouse::Event::ButtonPressed(mouse::Button::Left)),
        (to, mouse::Event::CursorMoved { position: to }),
    ] {
        let _ = ui.update(
            &[Event::Mouse(event)],
            mouse::Cursor::Available(at),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
    }
    let widths: Vec<f32> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Window(WindowMsg::ResizeSidebar(x)) => Some(*x),
            _ => None,
        })
        .collect();
    assert_eq!(widths, [app.sidebar_width + 40.0]);
}

/// A History view of two commits, the newer opened to its files: what the
/// History tests read.
fn history_app() -> (App, String, String) {
    use clew_protocol::{CommitDetail, CommitFile, LogCommit, LogRef, RefKind};
    let mut app = reader_app();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (new, old) = ("b".repeat(40), "a".repeat(40));
    let commit = |sha: &str, parents: &[&str], subject: &str| LogCommit {
        sha: sha.to_string(),
        parents: parents.iter().map(|p| p.to_string()).collect(),
        author: "Mira Chen".into(),
        time: now - 3 * 86_400,
        subject: subject.into(),
        refs: Vec::new(),
    };
    let mut commits = vec![
        commit(&new, &[&old], "Tax the total"),
        commit(&old, &[], "Start the shop"),
    ];
    commits[0].refs.push(LogRef {
        kind: RefKind::Head,
        name: "main".into(),
    });
    let h = &mut app.proj.git_log;
    h.asked = true;
    h.graph = crate::app::history::graph_rows(&commits);
    h.commits = commits;
    h.open = Some(new.clone());
    h.details.insert(
        new.clone(),
        Ok(Some(CommitDetail {
            sha: new.clone(),
            parents: vec![old.clone()],
            author: "Mira Chen".into(),
            time: now - 3 * 86_400,
            message: "Tax the total\n\nNot each line.".into(),
            files: vec![
                CommitFile {
                    rel: "teashop/pricing.py".into(),
                    old: None,
                    status: 'M',
                    added: Some(4),
                    removed: Some(2),
                },
                CommitFile {
                    rel: "teashop/legacy.py".into(),
                    old: None,
                    status: 'D',
                    added: Some(0),
                    removed: Some(9),
                },
            ],
            more_files: 0,
        })),
    );
    (app, new, old)
}

/// The History view lists the project's commits, newest first: the name
/// that points at one, its subject, its short id, who and when. Pressed, a
/// commit opens to its message and files, each file read as the commit
/// left it — not a deleted one, which it did not leave — and its whole id
/// is there to copy.
#[test]
fn the_history_view_lists_commits_and_opens_one_to_its_files() {
    use crate::HistoryMsg;
    let (app, new, old) = history_app();
    let view = || sim_elem(super::history::history_tab(&app));
    assert!(shows(&mut view(), "Tax the total"));
    // Whole in its row, the opened commit's subject is not said again
    // under it.
    assert!(
        text_at(&mut view(), |t| t == "Tax the total", 1).is_none(),
        "the subject said twice"
    );
    assert!(shows(&mut view(), "main"));
    assert!(shows(&mut view(), "bbbbbbb by Mira Chen, 3 days ago"));
    assert!(
        shows(&mut view(), "Not each line."),
        "the body of the opened one"
    );
    assert!(shows(&mut view(), "2 files changed"));

    let sent = click(view(), "Start the shop");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::History(HistoryMsg::Toggle(s)) if *s == old)),
        "{sent:?}"
    );
    let sent = click(view(), "pricing.py");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::History(HistoryMsg::OpenAt { sha, rel })
                if *sha == new && rel == "teashop/pricing.py"
        )),
        "{sent:?}"
    );
    let sent = click(view(), "legacy.py");
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::History(HistoryMsg::OpenAt { .. }))),
        "a file the commit deleted is not there to read: {sent:?}"
    );
    let sent = click(view(), "Copy id");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::History(HistoryMsg::CopyId(s)) if *s == new)),
        "{sent:?}"
    );
}

/// This branch / All branches spans the row, two equal halves, each label
/// centred in its own: one hugging its labels at the row's start, with
/// nothing after it, looked unfinished. The half not chosen switches; the
/// chosen one has nothing to do.
#[test]
fn the_branch_switch_spans_the_row() {
    use crate::HistoryMsg;
    let (app, ..) = history_app();
    let view = || sim_elem(super::history::history_tab(&app));
    let mut sim = view();
    let this = text_at(&mut sim, |t| t == "This branch", 0).expect("This branch");
    let all = text_at(&mut sim, |t| t == "All branches", 0).expect("All branches");
    // The simulator's 1024 wide, less the header's 10 a side: halves
    // centred near a quarter and three quarters across.
    let quarter = (1024.0 - 20.0) / 4.0 + 10.0;
    assert!(
        (this.center_x() - quarter).abs() < 8.0,
        "This branch centred in the left half: {this:?}"
    );
    assert!(
        (all.center_x() - 3.0 * quarter + 10.0).abs() < 16.0,
        "All branches centred in the right half: {all:?}"
    );
    let sent = click(view(), "All branches");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::History(HistoryMsg::AllBranches(true)))),
        "{sent:?}"
    );
    let sent = click(view(), "This branch");
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::History(HistoryMsg::AllBranches(_)))),
        "the chosen half: {sent:?}"
    );
}

/// A page read for a filter of words or an author is drawn as one dashed
/// lane: its commits are not each other's parents, and a second lane drawn
/// for one read as a branch that is not there. The whole history keeps its
/// branches.
#[test]
fn a_filtered_history_draws_no_branches() {
    use crate::HistoryMsg;
    use clew_protocol::{LogCommit, LogPage};
    let (mut app, new, old) = history_app();
    // Two matches with a commit between them the filter left out.
    let page = |app: &App| LogPage {
        commits: vec![
            LogCommit {
                parents: vec!["c".repeat(40)],
                ..app.proj.git_log.commits[0].clone()
            },
            app.proj.git_log.commits[1].clone(),
        ],
        more: false,
    };
    app.proj.git_log.query = "tax".into();
    let loaded = HistoryMsg::Loaded {
        stamp: app.stamp(),
        generation: app.proj.git_log.generation,
        skip: 0,
        result: Ok(page(&app)),
    };
    let _ = app.update(Message::History(loaded));
    let g = &app.proj.git_log.graph;
    assert_eq!(app.proj.git_log.commits[0].sha, new);
    assert!(g.iter().all(|r| r.dashed && r.col == 0), "{g:?}");

    app.proj.git_log.query.clear();
    let loaded = HistoryMsg::Loaded {
        stamp: app.stamp(),
        generation: app.proj.git_log.generation,
        skip: 0,
        result: Ok(page(&app)),
    };
    let _ = app.update(Message::History(loaded));
    let g = &app.proj.git_log.graph;
    assert_eq!(app.proj.git_log.commits[1].sha, old);
    assert!(g.iter().all(|r| !r.dashed), "{g:?}");
}

/// Updates is the last of Settings' pages: what a reader sets up first
/// comes first, Clew's own updates after.
#[test]
fn updates_is_the_last_settings_page() {
    assert_eq!(
        crate::SettingsPage::ALL.last(),
        Some(&crate::SettingsPage::Updates)
    );
    assert_eq!(crate::SettingsPage::ALL[0], crate::SettingsPage::Appearance);
}

/// Settings → Usage and cost: what the key spent today and this month, by
/// feature and by call, at the model's price — read from the ledger as the
/// page opens.
#[test]
fn the_usage_page_says_what_the_key_spent() {
    use crate::app::usage::{ASK, EXPLAIN, Entry};
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Usage,
    )));
    assert!(shows(&mut sim_of(&app), "Loading…"));
    let now = crate::app::usage::now_secs();
    let call = |purpose: &str, input: u64, output: u64| Entry {
        at: now,
        purpose: purpose.into(),
        model: "gpt-4o-mini".into(),
        input,
        cached: 0,
        written: 0,
        output,
    };
    let _ = app.update(Message::Usage(crate::UsageMsg::Read(vec![
        call(EXPLAIN, 1_000_000, 0),
        call(ASK, 0, 1_000_000),
    ])));
    let mut sim = sim_of(&app);
    for said in [
        "Today",
        "This month",
        "2 calls, 2M tokens",
        "$0.75",
        "This month by feature",
        "Explanations",
        "Latest calls",
    ] {
        assert!(shows(&mut sim, said), "{said:?}");
    }
    drop(sim);
    // Down the page, the prices the money is worked out at.
    let mut page = sim_tall(super::usage::usage_page(&app));
    assert!(shows(&mut page, "Prices"));
    // The chart says what a bar measures and when it runs; each call says
    // when, to the minute.
    for said in ["Cost per day", "Today, "] {
        assert!(shows(&mut page, said), "{said:?}");
    }
    drop(page);
    // Prices Clew knows are said in a line, not three boxes to fill in;
    // Edit shows the boxes.
    app.usage.model = "gpt-4o-mini".into();
    let mut page = sim_tall(super::usage::usage_page(&app));
    assert!(shows(&mut page, "$0.15 in, $0.60 out, per million tokens"));
    assert!(!shows(&mut page, "per 1M tokens"), "boxes for known prices");
    drop(page);
    assert!(matches!(
        click(sim_tall(super::usage::usage_page(&app)), "Edit").as_slice(),
        [Message::Usage(crate::UsageMsg::EditPrices(true))]
    ));
    let _ = app.update(Message::Usage(crate::UsageMsg::EditPrices(true)));
    let mut page = sim_tall(super::usage::usage_page(&app));
    assert!(shows(&mut page, "per 1M tokens"));
    assert!(shows(&mut page, "Done"));
    assert!(!shows(&mut page, "Enter a number"));
    drop(page);
    // A price that is no number is said to be one, not taken silently.
    let _ = app.update(Message::Usage(crate::UsageMsg::Price(
        crate::app::usage::PriceField::Input,
        "abc".into(),
    )));
    let mut page = sim_tall(super::usage::usage_page(&app));
    assert!(shows(&mut page, "Enter a number."));
    drop(page);
    // And Done keeps the editor until it is one: pressed, it closed and
    // dropped the edit without a word.
    assert!(click(sim_tall(super::usage::usage_page(&app)), "Done").is_empty());
    let _ = app.update(Message::Usage(crate::UsageMsg::Price(
        crate::app::usage::PriceField::Input,
        "0.2".into(),
    )));
    assert!(matches!(
        click(sim_tall(super::usage::usage_page(&app)), "Done").as_slice(),
        [Message::Usage(crate::UsageMsg::EditPrices(false))]
    ));
}

/// An empty call hierarchy, with the caret in a function, offers that
/// function's: it only said to right-click one (round-6 L17).
#[test]
fn an_empty_call_hierarchy_offers_the_carets_function() {
    let mut app = reader_app();
    app.proj.call_graph = None;
    app.proj.explain.view = None;
    assert!(!shows(
        &mut sim_elem(super::panels::calls_tab(&app)),
        "Show for total()"
    ));
    app.proj.explain.view = Some(crate::explain::Node::Function {
        file: "src/lib.rs".into(),
        name: "total".into(),
        ordinal: 0,
    });
    assert!(matches!(
        click(sim_elem(super::panels::calls_tab(&app)), "Show for total()").as_slice(),
        [Message::Calls(CallsMsg::OfFunction)]
    ));
}

/// The balance says what is left: an empty currency beside one with money
/// is left out; with none, the empty one is said.
#[test]
fn a_balance_leaves_out_an_empty_currency() {
    let said = |a: &[(&str, &str)]| {
        super::usage::balance_said(
            &a.iter()
                .map(|(c, v)| (c.to_string(), v.to_string()))
                .collect::<Vec<_>>(),
        )
    };
    assert_eq!(said(&[("USD", "0.00"), ("CNY", "72.01")]), "CN¥72.01");
    assert_eq!(
        said(&[("USD", "3.10"), ("CNY", "72.01")]),
        "$3.10, CN¥72.01"
    );
    assert_eq!(said(&[("USD", "0.00")]), "$0.00");
}

/// A model call the server made says what it used: kept, and counted to
/// the Ask answer it was for, whose cost is then said under it.
#[test]
fn a_reported_call_is_kept_and_counted_to_its_answer() {
    let mut app = reader_app();
    app.usage.entries = Some(Vec::new());
    for _ in 0..2 {
        let _ = app.handle_server_event(clew_protocol::Event::Usage {
            work: 42,
            purpose: crate::app::usage::ASK.into(),
            model: "gpt-4o-mini".into(),
            input: 1_000,
            cached: 0,
            written: 0,
            output: 500,
        });
    }
    assert_eq!(app.usage.entries.as_ref().map(Vec::len), Some(2));
    assert_eq!(app.answer_cost(42).as_deref(), Some("3k tokens, $0.0009"));
    assert!(app.answer_cost(7).is_none(), "another answer spent nothing");
    // A call that said nothing of what it used is no entry.
    let _ = app.handle_server_event(clew_protocol::Event::Usage {
        work: 43,
        purpose: crate::app::usage::EXPLAIN.into(),
        model: "gpt-4o-mini".into(),
        input: 0,
        cached: 0,
        written: 0,
        output: 0,
    });
    assert_eq!(app.usage.entries.as_ref().map(Vec::len), Some(2));
}

/// Explaining says what it is likely to cost before it is asked for: the
/// count of calls, and under it their tokens and money.
#[test]
fn explaining_says_what_it_will_likely_cost() {
    let mut app = reader_app();
    app.llm_available = true;
    let estimate = app.explain_estimate(45).expect("an estimate");
    let said = estimate.said(45);
    assert!(
        said.starts_with("About 45 AI calls: ") && said.contains("tokens"),
        "{said:?}"
    );
    assert!(!estimate.from_history, "no explanations in the ledger yet");
    assert_eq!(
        app.explain_estimate(0),
        None,
        "nothing to explain, nothing to say"
    );
    app.llm_available = false;
    assert_eq!(app.explain_estimate(45), None, "no key, no estimate");
}

/// "Check now" says what it is doing, on the page it was pressed on: its
/// answer went only to the status bar the Settings window covers, so it
/// seemed to do nothing. While a check runs the button says "Checking…"
/// and cannot be pressed again; after, the page says what it found.
#[test]
fn check_now_says_what_it_found_on_its_page() {
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Updates,
    )));
    assert!(
        !shows(&mut sim_of(&app), "Clew is up to date"),
        "before any check"
    );
    app.update.checking = true;
    assert!(shows(&mut sim_of(&app), "Checking…"));
    let sent = click(sim_of(&app), "Checking…");
    assert!(
        !sent
            .iter()
            .any(|m| matches!(m, Message::Updater(crate::UpdaterMsg::Check))),
        "a second check while one runs: {sent:?}"
    );
    let release = clew_core::update::Release {
        version: crate::updater::current_version(),
        notes: String::new(),
        dmg_url: None,
    };
    let _ = app.update(Message::Updater(crate::UpdaterMsg::Checked {
        manual: true,
        result: Ok(release),
    }));
    assert!(!app.update.checking);
    assert!(shows(&mut sim_of(&app), "Clew is up to date"));
    assert!(shows(&mut sim_of(&app), "Check now"));

    let _ = app.update(Message::Updater(crate::UpdaterMsg::Checked {
        manual: true,
        result: Err("no network".into()),
    }));
    assert!(shows(&mut sim_of(&app), "Update check failed: no network"));
}

/// A form's segmented control is as wide as the pickers above and below it,
/// in equal parts: Mode's System / Light / Dark hugged their words and sat
/// short of the theme pickers.
#[test]
fn a_forms_switch_is_as_wide_as_its_pickers() {
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::Appearance,
    )));
    let mut sim = sim_of(&app);
    let centre =
        |sim: &mut Sim<'_>, t: &'static str| text_at(sim, move |c| c == t, 0).expect(t).center_x();
    let (system, light, dark) = (
        centre(&mut sim, "System"),
        centre(&mut sim, "Light"),
        centre(&mut sim, "Dark"),
    );
    // Equal parts of exactly the picker's width (less the trough's 2 a
    // side and the 2 between parts): their centres a part and a gap apart.
    // At 8 more it started 8 px left of the pickers.
    let step = (super::form::PICK_W - 4.0 - 2.0 * 2.0) / 3.0 + 2.0;
    assert!(
        ((light - system) - step).abs() < 1.0,
        "{system} {light} {dark}"
    );
    assert!(
        ((dark - light) - step).abs() < 1.0,
        "{system} {light} {dark}"
    );
}

/// A page put away from over the code (Statistics here) brings the code
/// back where the reader left it: the code view, drawn afresh under it,
/// started at the top. Nothing is scrolled for a view at the top already,
/// nor while the page is still up.
#[test]
fn a_page_put_away_leaves_the_code_where_it_was() {
    let mut app = reader_app();
    app.proj.panes[0].as_mut().unwrap().scroll_y = 480.0;
    app.panes_covered = false;
    assert!(app.sync_panes_uncovered().is_none(), "nothing covered it");
    app.proj.stats.showing = true;
    assert!(app.sync_panes_uncovered().is_none(), "still covered");
    assert!(app.panes_covered);
    app.proj.stats.showing = false;
    assert!(app.sync_panes_uncovered().is_some(), "scrolled back");
    assert!(app.sync_panes_uncovered().is_none(), "once");
    app.proj.panes[0].as_mut().unwrap().scroll_y = 0.0;
    app.proj.stats.showing = true;
    let _ = app.sync_panes_uncovered();
    app.proj.stats.showing = false;
    assert!(app.sync_panes_uncovered().is_none(), "at the top already");
}

/// An AI action asked with no key opens the Language model page saying why,
/// the keyboard in its key field: it opened on the form with no word of it.
/// The word goes as Settings closes.
#[test]
fn an_ai_action_without_a_key_says_why_settings_opened() {
    let mut app = reader_app();
    app.settings.key.clear();
    let _ = app.update(Message::Settings(crate::SettingsMsg::NeedKey));
    assert!(app.settings.open);
    assert_eq!(app.settings.page, crate::SettingsPage::LanguageModel);
    assert!(shows(&mut sim_of(&app), "That needs an AI model"));
    let _ = app.update(Message::Settings(crate::SettingsMsg::Close));
    let _ = app.update(Message::Settings(crate::SettingsMsg::OpenAt(
        crate::SettingsPage::LanguageModel,
    )));
    assert!(
        !shows(&mut sim_of(&app), "That needs an AI model"),
        "opened by hand: nothing asked for it"
    );
}

/// Go to File (Ctrl+P, the key the field over the window shows) with no file
/// by the name typed offers the commands that have it: "settings" said only
/// "No matches", though that field says it searches commands too. A name
/// that is a file's lists the files as before.
#[test]
fn go_to_file_offers_the_commands_no_file_is_named_after() {
    let mut app = reader_app();
    let _ = app.update(Message::Nav(NavMsg::FinderOpened(
        crate::session::finder::FinderMode::Files,
    )));
    let _ = app.update(Message::Nav(NavMsg::FinderQueryChanged("settings".into())));
    assert!(
        app.proj.finder.bar_fallback,
        "nothing of the project by that name"
    );
    assert!(!app.proj.finder.results.is_empty());
    {
        let mut sim = sim_of(&app);
        assert!(!shows(&mut sim, "No matches"));
        assert!(shows(&mut sim, "Settings"), "a Settings command offered");
    }
    let _ = app.update(Message::Nav(NavMsg::FinderQueryChanged(":".into())));
    assert!(
        !app.proj.finder.bar_fallback,
        "a line asked for is not searched"
    );
}

/// The History view with the sidebar on it, nothing asked yet.
fn history_shown(mut app: App) -> App {
    app.show_left_sidebar = true;
    app.sidebar = crate::SidebarTab::History;
    app
}

/// A History page read for the view as it stands.
fn history_page(app: &App, result: Result<clew_protocol::LogPage, String>) -> Message {
    Message::History(crate::HistoryMsg::Loaded {
        stamp: app.stamp(),
        generation: app.proj.git_log.generation,
        skip: 0,
        result,
    })
}

/// The History view reads what it shows whenever it shows and has not
/// asked — after a new project, or a tutorial's sample put away, it stayed
/// on "Reading the history…": nothing read it again.
#[test]
fn the_history_view_reads_again_once_it_has_not_asked() {
    let mut app = history_shown(reader_app());
    app.proj.git_log.asked = false;
    let _ = app.update(Message::Noop);
    assert!(app.proj.git_log.asked, "read on the next update");
    // Not shown: nothing read.
    app.sidebar = crate::SidebarTab::Files;
    app.proj.git_log.asked = false;
    let _ = app.update(Message::Noop);
    assert!(!app.proj.git_log.asked);
}

/// A filter is read once its typing settles, not for each key: each key
/// walked the whole history.
#[test]
fn the_history_filter_is_read_once_it_settles() {
    use crate::HistoryMsg;
    let mut app = history_shown(reader_app());
    app.proj.git_log.asked = true;
    let _ = app.update(Message::History(HistoryMsg::Query("ta".into())));
    let first = app.proj.git_log.typed;
    let _ = app.update(Message::History(HistoryMsg::Query("tax".into())));
    assert!(app.proj.git_log.asked, "not read for a key");
    let _ = app.update(Message::History(HistoryMsg::QuerySettled(first)));
    assert!(
        !app.proj.git_log.loading,
        "an earlier key's settling reads nothing"
    );
    let last = app.proj.git_log.typed;
    let _ = app.update(Message::History(HistoryMsg::QuerySettled(last)));
    assert!(app.proj.git_log.loading, "the last key's settling reads it");
}

/// A first page that failed clears what the last filter showed — it
/// stayed under the new chip as if it matched; one file's commits, followed
/// across renames, are drawn apart.
#[test]
fn a_failed_or_followed_history_page_is_shown_for_what_it_is() {
    let (mut app, ..) = history_app();
    app.proj.git_log.path = Some("teashop/legacy.py".into());
    let failed = history_page(&app, Err("refused".into()));
    let _ = app.update(failed);
    assert!(app.proj.git_log.commits.is_empty() && app.proj.git_log.graph.is_empty());
    assert_eq!(app.proj.git_log.error.as_deref(), Some("refused"));

    let (mut app, new, old) = history_app();
    let page = clew_protocol::LogPage {
        commits: app.proj.git_log.commits.clone(),
        more: false,
    };
    let project = app.proj.project.as_mut().unwrap();
    project.files = std::sync::Arc::new(vec![crate::fs_scan::FileEntry {
        abs: project.root.join("teashop/pricing.py"),
        rel: "teashop/pricing.py".into(),
    }]);
    app.proj.git_log.path = Some("teashop/pricing.py".into());
    let loaded = history_page(&app, Ok(page));
    let _ = app.update(loaded);
    assert_eq!(app.proj.git_log.commits.len(), 2, "{new} {old}");
    assert!(app.proj.git_log.graph.iter().all(|r| r.dashed));
}

/// A changed file pressed while it is the one being read shows its Diff at
/// once — no load was coming to show it after, and the Diff came on at the
/// file's next opening, long after. What waits for a file is dropped when
/// another opens first, or it fails to.
#[test]
fn history_opens_answer_the_next_file_only() {
    use crate::HistoryMsg;
    let mut app = reader_app();
    let sent = crate::app::tests::run_task(app.update(Message::History(HistoryMsg::OpenChanged(
        "src/lib.rs".into(),
    ))));
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Editor(crate::EditorMsg::ToggleDiff))),
        "{sent:?}"
    );
    assert!(app.proj.diff_after_open.is_none(), "nothing left waiting");

    let root = app.proj.project.as_ref().unwrap().root.clone();
    app.proj.diff_after_open = Some(root.join("a.py"));
    app.proj.time_travel_after_open = Some((root.join("a.py"), "a".repeat(40)));
    let _ = app.after_history_open(&root.join("b.py"));
    assert!(app.proj.diff_after_open.is_none() && app.proj.time_travel_after_open.is_none());

    app.proj.diff_after_open = Some(root.join("a.py"));
    app.forget_history_open(&root.join("a.py"));
    assert!(app.proj.diff_after_open.is_none());
}

/// M4 (round 6): a compiled file among the uncommitted changes was pressed
/// like the rest — it closed what was open for "binary file" and became a
/// stop on the trail. It is listed, quiet and inert; a text file still
/// opens.
#[test]
fn a_binary_uncommitted_file_is_listed_but_not_opened() {
    use crate::HistoryMsg;
    assert!(super::history::binary_name(
        "teashop/__pycache__/a.cpython-313.pyc"
    ));
    assert!(super::history::binary_name("logo.PNG"));
    assert!(!super::history::binary_name("teashop/pricing.py"));
    assert!(!super::history::binary_name("Makefile"));
    let mut app = history_shown(reader_app());
    app.proj.git_status = [
        ("src/widget.rs".to_string(), 'M'),
        ("src/__pycache__/widget.cpython-313.pyc".to_string(), 'U'),
    ]
    .into_iter()
    .collect();
    app.proj.git_log.changes_open = true;
    assert!(click(sim_of(&app), "widget.cpython-313.pyc").is_empty());
    let sent = click(sim_of(&app), "widget.rs");
    assert!(
        sent.iter().any(
            |m| matches!(m, Message::History(HistoryMsg::OpenChanged(r)) if r == "src/widget.rs")
        ),
        "{sent:?}"
    );
}

/// A commit whose read failed is read again when opened again; it was
/// cached, failed, for good.
#[test]
fn a_commit_read_that_failed_is_read_again() {
    use crate::HistoryMsg;
    let (mut app, new, _) = history_app();
    app.proj
        .git_log
        .details
        .insert(new.clone(), Err("timed out".into()));
    app.proj.git_log.open = None;
    let _ = app.update(Message::History(HistoryMsg::Toggle(new.clone())));
    assert_eq!(app.proj.git_log.open.as_deref(), Some(new.as_str()));
    assert!(!app.proj.git_log.details.contains_key(&new), "asked again");
}

/// The check pressed in Settings ends its own "Checking…": the one at
/// startup answering meanwhile left it pressable again — and a startup
/// check that failed is no red line in Settings.
#[test]
fn only_the_pressed_check_ends_checking() {
    let mut app = reader_app();
    app.update.checking = true;
    let _ = app.update(Message::Updater(crate::UpdaterMsg::Checked {
        manual: false,
        result: Err("offline".into()),
    }));
    assert!(app.update.checking);
    assert!(app.update.last_check.is_none());
}

/// The Diff of a file git does not track is the file, every line added —
/// it said only "No diff … not tracked by git".
#[test]
fn an_untracked_files_diff_is_every_line_added() {
    let mut app = reader_app();
    let (abs, rel, total) = {
        let v = app.active_viewer().unwrap();
        (v.abs.clone(), v.rel.clone(), v.source.lines().count())
    };
    // One git ignores — or a project that is no repository — is answered
    // as an untracked file is: it was shown against a HEAD there is not.
    app.proj.diff_pending = Some((6, abs.clone()));
    let _ = app.on_diff_loaded(6, abs.clone(), rel.clone(), Ok(None));
    assert!(app.proj.diff.is_none(), "not shown as about to be added");
    assert!(app.status.contains("not tracked"), "{}", app.status);
    app.proj.git_status.insert(rel.clone(), 'U');
    app.proj.diff_pending = Some((7, abs.clone()));
    let _ = app.on_diff_loaded(7, abs.clone(), rel.clone(), Ok(None));
    let diff = app.proj.diff.as_ref().expect("a diff shown");
    assert_eq!(diff.abs, abs);
    let added = diff
        .lines
        .iter()
        .filter(|l| l.kind == crate::git::DiffKind::Add)
        .count();
    assert_eq!(added, total);
    // Drawn as the file reads: indented lines keep their indent.
    let source = app.active_viewer().unwrap().source.clone();
    let drawn: Vec<&str> = diff
        .runs
        .iter()
        .filter(|r| r.kind == crate::git::DiffKind::Add)
        .flat_map(|r| r.text.lines())
        .collect();
    let want: Vec<String> = source
        .lines()
        .map(super::panes::diff_display_text)
        .collect();
    assert_eq!(drawn, want);
    // Nothing is hidden before its first line, nor after its last.
    assert!(
        diff.runs
            .iter()
            .all(|r| r.kind != crate::git::DiffKind::Hunk)
    );
    assert!(!diff.hides_tail());
    // Read again after a save made elsewhere, it is still every line added:
    // it said "No uncommitted changes".
    app.proj.diff_refresh = Some(8);
    let _ = app.on_diff_loaded(8, abs.clone(), rel, Ok(None));
    let again = app.proj.diff.as_ref().expect("still shown");
    assert_eq!(
        again
            .lines
            .iter()
            .filter(|l| l.kind == crate::git::DiffKind::Add)
            .count(),
        total
    );
}

/// A search result longer than its row is cut with an ellipsis, each piece
/// keeping its colour and mark: it was clipped mid-letter at the panel's
/// edge ("Discounts an order can earn. Each or").
#[test]
fn a_long_search_result_ends_in_an_ellipsis() {
    crate::fonts::load();
    let shown = "Discounts an order can earn. Each one is checked in turn";
    let pieces = vec![
        ("Discounts".to_string(), Some(1), true),
        (
            " an order can earn. Each one is checked in turn".to_string(),
            None,
            false,
        ),
    ];
    let room = super::steady::width_of("Discounts an order", super::ts::BODY, crate::fonts::UI);
    let fit = super::sidebar::fit_pieces(pieces.clone(), shown, room);
    let joined: String = fit.iter().map(|(p, ..)| p.as_str()).collect();
    assert!(joined.ends_with('…'), "{joined}");
    assert!(joined.starts_with("Discounts an"), "{joined}");
    assert!(
        super::steady::width_of(&joined, super::ts::BODY, crate::fonts::UI) <= room + 0.5,
        "{joined}"
    );
    assert_eq!(
        fit[0],
        ("Discounts".to_string(), Some(1), true),
        "mark kept"
    );
    // What fits is left whole, with no ellipsis.
    assert_eq!(
        super::sidebar::fit_pieces(pieces.clone(), shown, 10_000.0),
        pieces
    );
}

/// The Overview's module map takes over half the window's height, within
/// bounds: at a fixed 320 it was a knot some 200 high across a page 900
/// wide, its labels running into one another.
#[test]
fn the_overview_map_takes_room_from_the_window() {
    use super::docs_stats::overview_map_h;
    assert!(overview_map_h(860.0) > 450.0, "{}", overview_map_h(860.0));
    assert_eq!(overview_map_h(400.0), super::chrome::OVERVIEW_MAP_H);
    assert_eq!(overview_map_h(4000.0), 640.0);
}

/// A server program of the reader's own is added from a form of its own,
/// headed as such, its Add on a row of its own — it sat in the chosen
/// list's card with no heading, its Use on the Arguments row.
#[test]
fn a_server_program_is_added_from_a_form_of_its_own() {
    let mut app = reader_app();
    app.proj.lang_drafts.server_language = Some("python".into());
    app.proj.lang_drafts.server_command = "pyright-langserver".into();
    let mut sim = sim_tall(super::connect::languages_section(&app));
    assert!(sim.find("Add a server program").is_ok());
    assert!(
        sim.find("Server programs").is_err(),
        "no list while none is chosen"
    );
    let _ = sim.click("Add");
    assert!(
        sim.into_messages().any(|m| matches!(
            m,
            Message::Languages(crate::LanguagesMsg::UseDraft(
                crate::LangField::ServerCommand
            ))
        )),
        "Add uses the draft"
    );
}

/// A call this window makes itself is counted where the window shows it —
/// the ledger as read, and the Ask answer it was for — not only kept in
/// the file: the Usage page and a local answer's cost knew nothing of it.
#[test]
fn a_local_call_is_counted_where_it_is_shown() {
    let mut app = reader_app();
    app.usage.entries = Some(Vec::new());
    let meter = crate::App::local_meter();
    meter(clew_core::llm::Spent {
        provider: clew_core::llm::Provider::OpenAI,
        model: "gpt-4o-mini".into(),
        usage: clew_core::llm::Usage {
            input: 1_000,
            cached: 0,
            written: 0,
            output: 500,
        },
        purpose: crate::app::usage::ASK.into(),
        work: 77,
    });
    app.take_local_spent();
    assert_eq!(app.usage.entries.as_ref().map(Vec::len), Some(1));
    assert!(
        app.answer_cost(77).is_some(),
        "the answer shows what it cost"
    );
}

/// A folder's change dot counts only what the tree shows inside it: a
/// dot for `__pycache__/*.pyc`, hidden by the tree, led nowhere.
#[test]
fn a_folder_marks_only_the_changes_the_tree_shows() {
    use super::sidebar::shown_in_tree;
    assert!(shown_in_tree("shop/cart.py"));
    assert!(!shown_in_tree("shop/__pycache__/cart.cpython-313.pyc"));
    assert!(!shown_in_tree("web/node_modules/x/index.js"));
    assert!(!shown_in_tree(".clew/bookmarks.json"));
    assert!(shown_in_tree(".clew/launch.json"));
    assert!(shown_in_tree("README.md"));
}
