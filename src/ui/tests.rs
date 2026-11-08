//! View-layer tests that drive the real widget tree the way the runtime does:
//! build the view, feed it events, carry its widget state (`Cache`) across
//! rebuilds, and read that state back through widget operations.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, DocsMsg, EditorMsg, GlossaryMsg, GraphMsg,
    HoverMsg, NavMsg, ProjectMsg, ReadingMsg, ServerMsg, Stamp, TutorialMsg, WalkMsg, WindowMsg,
};

use iced::advanced::clipboard;
use iced::advanced::widget::Operation;
use iced::advanced::widget::operation::focusable::find_focused;
use iced::advanced::widget::operation::scrollable::Scrollable;
use iced::advanced::widget::operation::{Outcome, black_box};
use iced::widget::{Id, column, container, scrollable, space, text_input};
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
    let banners: [Toggle; 2] = [
        (
            "the cycles banner",
            Box::new(|app| app.proj.import_cycles = vec![vec![PathBuf::from("/x.rs")]]),
        ),
        (
            "the incomplete-index banner",
            Box::new(|app| app.proj.index_cap_note = Some("3 too large".into())),
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
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Up to date"), "the refresh chip, idle");
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
        assert!(shows(&mut sim, "Module map"), "3D {graph_3d}: no map");
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
    let expected: [(StripTool, Sends); 9] = [
        (StripTool::Overview, |m| {
            matches!(m, Message::Overview(crate::OverviewMsg::Show))
        }),
        (StripTool::Stats, |m| {
            matches!(m, Message::Overview(crate::OverviewMsg::ShowStats))
        }),
        (StripTool::Debug, |m| {
            matches!(m, Message::Debug(DebugMsg::Start))
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
        // The ⋯ is not on a strip tall enough for the menu's own tools.
        let Some(at) = super::strip_slot_center(&app, super::StripSlot::Tool(tool)) else {
            assert_eq!(tool, StripTool::More, "{tool:?} is not on the strip");
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
                super::ToolsRow::Shortcuts => {
                    matches!(sent.as_slice(), [Message::Window(WindowMsg::OpenShortcuts)])
                }
                super::ToolsRow::OpenFolder => matches!(
                    sent.as_slice(),
                    [Message::Project(crate::ProjectMsg::OpenFolderPressed)]
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
        !shows(&mut sim_of(&app), "Open Remote…"),
        "a menu of what the strip shows"
    );
    // The ⋯ menu's rows, in a window too short for them on the strip: each
    // computed row rect holds that row's label.
    app.window_height = TOOLS_IN_MENU_H;
    assert!(!super::tools_on_strip(&app));
    for (row, label) in [
        (super::ToolsRow::ExplainAll, "Explain All"),
        (super::ToolsRow::OpenRemote, "Open Remote…"),
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

    app.pending_consent = Some(PathBuf::from("/nonexistent/clew-ui-test"));
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

/// E2-6: Tab cycles focus through the open modal's fields only, wrapping, and
/// clears any focus left behind it; with no modal it covers every field.
#[test]
fn tab_focus_cycles_inside_the_open_modal() {
    let mut renderer = renderer();
    let input = |id: &'static str| {
        text_input::<Message, iced::Theme, iced::Renderer>("", "")
            .id(Id::new(id))
            .on_input(|_| Message::Noop)
    };
    // The modal is framed by the real `modal()` every dialog is built with
    // — the scope is whatever that frame marks, not a container this test
    // labelled itself.
    let view = |with_modal: bool| -> Element<'static, Message> {
        let modal: Element<'static, Message> = if with_modal {
            super::modal(
                column![input("modal-a"), input("modal-b")],
                super::Placement::Center,
                super::Backdrop::Dim(None),
            )
        } else {
            space().into()
        };
        iced::widget::stack![column![input("behind-a"), input("behind-b")], modal].into()
    };
    let mut focused_after = |with_modal: bool, cache: Cache, forward: bool| {
        let mut ui = UserInterface::build(view(with_modal), WINDOW, cache, &mut renderer);
        let _ = run_operation(&mut ui, &renderer, super::focus_cycle(forward));
        let focused = run_operation(&mut ui, &renderer, find_focused());
        (focused, ui.into_cache())
    };

    let (first, cache) = focused_after(true, Cache::default(), true);
    assert_eq!(first, Some(Id::new("modal-a")));
    let (second, cache) = focused_after(true, cache, true);
    assert_eq!(second, Some(Id::new("modal-b")));
    let (wrapped, cache) = focused_after(true, cache, true);
    assert_eq!(wrapped, Some(Id::new("modal-a")), "the cycle wraps");
    let (back, cache) = focused_after(true, cache, false);
    assert_eq!(back, Some(Id::new("modal-b")), "Shift-Tab goes backwards");

    // No modal: every field takes part (the first after the last).
    let (open, _) = focused_after(false, cache, true);
    assert_eq!(open, Some(Id::new("behind-a")));
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
const TOOLS_IN_MENU_H: f32 = 600.0;

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
    let _ = app.update(Message::Tutorial(TutorialMsg::Start));
    let total = crate::app::tutorial::steps(&app).len();
    assert!(total > 10, "the tour lost its steps: {total}");
    for i in 0..total {
        assert_eq!(app.tutorial, Some(i));
        let title = crate::app::tutorial::steps(&app)[i].title.clone();
        let mut sim = sim_of(&app);
        let counter = format!("Step {} of {total}", i + 1);
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

/// Every tool on the strip has its one tutorial step, Settings the last
/// (the type map's came in before it); and no text field holds the keyboard
/// while the tour shows its tab — the SEARCH box did, and took the → and
/// Enter that step the tour.
#[test]
fn the_tutorial_points_at_each_tool_and_leaves_no_field_holding_its_keys() {
    use crate::app::tutorial::{Anchor, steps};
    use crate::ui::StripTool;
    let mut app = reader_app();
    let _ = app.update(Message::Tutorial(TutorialMsg::Start));
    let all = steps(&app);
    for tool in super::STRIP_TOP.into_iter().chain([StripTool::Settings]) {
        let on: Vec<&str> = all
            .iter()
            .filter(|s| s.anchor == Anchor::Strip(tool))
            .map(|s| s.title.as_str())
            .collect();
        assert_eq!(on.len(), 1, "{tool:?}: {on:?}");
    }
    let at = |title: &str| all.iter().position(|s| s.title == title).unwrap();
    assert!(all[at("Settings")].anchor == Anchor::Strip(StripTool::Settings));
    assert!(all[at("The type map")].anchor == Anchor::Strip(StripTool::TypeMap));
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

    // Cancel: back to the form, saying why.
    let _ = app.update(Message::Connect(ConnectMsg::TrustCancel));
    assert!(matches!(
        app.connect.as_ref().map(|u| &u.stage),
        Some(crate::ConnectStage::Error(reason)) if reason == "refused: unknown key"
    ));
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

    // Cancel: back to the form, saying why.
    let _ = app.update(Message::Connect(ConnectMsg::TrustCancel));
    assert!(matches!(
        stage(&app),
        Some(crate::ConnectStage::Error(reason)) if reason == "refused: the host key changed"
    ));
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

/// F#8: consent questions are asked one at a time, and the modal says when
/// more wait behind the one on screen.
#[test]
fn the_consent_modal_says_how_many_more_wait() {
    let consent = crate::LspConsent {
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
        "About 3,600 AI calls. Also builds the semantic index.",
        "Not now",
    ] {
        assert!(shows(&mut sim, said), "{said:?}");
    }
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
    assert!(matches!(
        click(sim_elem(sheet(&app, &root)), "Explain").as_slice(),
        [Message::Project(ProjectMsg::SetupApply)]
    ));
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
    // A first open's Not now opens the project too, with nothing set up:
    // an answer as Open is, pressed only once it can have been read.
    let sim = sim_elem(open_sheet(&root, false));
    assert!(click(sim, "Not now").is_empty());
    let sim = sim_elem(open_sheet(&root, true));
    assert!(matches!(
        click(sim, "Not now").as_slice(),
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
            },
            crate::app::model::ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                allows: false,
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
        let row = text_at(&mut sim, |t| t == "Keep listening", 0).expect("the composer");
        sim.point_at(Point::new(
            size.width - super::COMPOSER_PAD.right - super::ROUND_BUTTON / 2.0,
            row.center_y(),
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
    // Enter showed an error above the form; typed on.
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
/// moves them, and a card too low for them hides them — each waits again.
/// And a hover's peek over the window counts as over the card.
#[test]
fn a_question_moved_or_out_of_view_waits_again() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.ask_view = crate::AskView::Compact;
    // The debugger's panel under the card, low: its height squeezes it.
    app.show_bottom = true;
    app.bottom_height = 100.0;
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
            WINDOW,
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

    // The debugger's panel taller: the Ask card standing on it moves up,
    // and with it the approval card in it.
    app.bottom_height = 140.0;
    let _ = app.update(Message::Noop);
    let moved = frame(&mut app);
    assert!(
        matches!(moved.as_slice(), [Some(b)] if Some(*b) != first[0]),
        "{moved:?}"
    );
    assert!(!app.may_allow(&card), "moved: unread where it is");
    read(&mut app);
    assert!(app.may_allow(&card));

    // An Ask card too low for the approval card (the debugger's panel tall
    // under it): not all of it in view, however long.
    app.bottom_height = SQUEEZING;
    assert_eq!(frame(&mut app), [None]);
    read(&mut app);
    assert!(!app.may_allow(&card), "out of view");
    app.bottom_height = 340.0;
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
            WINDOW,
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
            WINDOW,
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

/// An agent's answers, however many, stay within the card's reach: a few
/// of each kind at a time — a way to decline always among them — the rest
/// a page on, where the card is read anew. And a card coming up in a
/// bottom panel too low for it opens the panel to where it fits, once.
#[test]
fn many_answers_or_a_low_panel_leave_the_card_answerable() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, Question, TurnExtras};
    let option = |i: usize, allows: bool| ApprovalOption {
        id: format!("{}{i}", if allows { "yes" } else { "no" }),
        label: format!("{} {i}", if allows { "Allow" } else { "Deny" }),
        allows,
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

    // In an Ask card too low for it — the debugger's panel tall under it —
    // drawn, and found squeezed, it opens the Ask window large, where it
    // fits, and is answered there once read.
    let mut app = reader_app();
    let delay = std::time::Duration::from_secs(5);
    app.asking.delay = delay;
    app.ask_view = crate::AskView::Compact;
    app.show_bottom = true;
    app.bottom_height = SQUEEZING;
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
            WINDOW,
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
    // own drag of the debugger's panel squeezing it after.
    let _ = app.update(Message::Ask(AskMsg::Shrink));
    app.bottom_height = 120.0;
    app.proj.ask_turns[0].extras.approvals[0].key = ApprovalKey::Mcp { call: 3 };
    let _ = app.update(Message::Noop);
    assert!(matches!(frame(&mut app).as_slice(), [Some(_)]));
    assert_eq!(app.ask_view, crate::AskView::Compact);
    app.bottom_height = SQUEEZING;
    assert_eq!(frame(&mut app), [None], "squeezed by the user");
    assert_eq!(app.ask_view, crate::AskView::Compact, "and left so");
    // Reported again from nothing (the window moved): still left so.
    let _ = app.update(Message::Window(crate::WindowMsg::Moved));
    assert!(app.asking.places.is_empty());
    assert_eq!(frame(&mut app), [None]);
    assert_eq!(app.ask_view, crate::AskView::Compact);
    let _ = card;
}

/// A debugger's panel tall enough that the Ask card above it is too low
/// for an approval card's answers.
const SQUEEZING: f32 = 660.0;

/// A page of answers taller than the first — its labels running over more
/// rows — in an Ask card the first page fitted: drawn, and found squeezed,
/// it opens the Ask window large, as a card come up does.
#[test]
fn a_page_of_answers_too_tall_for_the_panel_opens_it() {
    use crate::app::model::{Approval, ApprovalKey, ApprovalOption, AskTurn, TurnExtras};
    let option = |id: String, label: String, allows: bool| ApprovalOption { id, label, allows };
    let mut options: Vec<ApprovalOption> = (0..3)
        .map(|i| option(format!("yes{i}"), format!("Allow {i}"), true))
        .collect();
    options.extend((3..6).map(|i| {
        let label = format!("Allow {i}, {}", "and so on and so forth ".repeat(8));
        option(format!("yes{i}"), label, true)
    }));
    options.push(option("no".into(), "Deny".into(), false));
    // With the debugger's panel `height` tall under the Ask card: where the
    // card was placed on its first page, then on its second, and the Ask
    // window's size after.
    let at = |height: f32| -> (Option<Rectangle>, Option<Rectangle>, crate::AskView) {
        let mut app = reader_app();
        app.ask_view = crate::AskView::Compact;
        app.show_bottom = true;
        app.bottom_height = height;
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
                WINDOW,
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
            },
            ApprovalOption {
                id: "no".into(),
                label: "Deny".into(),
                allows: false,
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
        ("Not now", |m| {
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
            },
            crate::app::model::ApprovalOption {
                id: "no".into(),
                label: "Deny".into(),
                allows: false,
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
    // Locally there is nothing to grant, so nothing is shown.
    app.connection = crate::connect::ConnTarget::Local;
    let mut local = sim_of(&app);
    assert!(!shows(&mut local, "AI API keys —"));
}

fn paused_debug_session() -> crate::DebugSession {
    crate::DebugSession {
        client: None,
        status: crate::DebugStatus::Stopped,
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
}

/// A language server's list in the Search sidebar says which list it is.
#[test]
fn a_reference_list_is_named_in_the_search_sidebar() {
    let mut app = reader_app();
    app.proj.search.ran = true;
    app.proj.search.listing = Some("(references)".into());
    app.proj.search.hits = vec![crate::search::SearchHit {
        abs: PathBuf::from("/nonexistent/clew-ui-test/src/lib.rs"),
        rel: "src/lib.rs".into(),
        line: 3,
        preview: "pub fn origin()".into(),
    }];
    assert!(shows(
        &mut sim_elem(super::sidebar::search_tab(&app)),
        "(references) · 1 match"
    ));
}

/// One letter in the search field is searched only on Enter, and the
/// sidebar says so: it stayed blank, as if nothing matched.
#[test]
fn a_one_letter_search_says_it_waits_for_enter() {
    let mut app = reader_app();
    app.proj.search.query = "f".into();
    assert!(shows(
        &mut sim_elem(super::sidebar::search_tab(&app)),
        "Press Enter to search"
    ));
    app.proj.search.query = "fn".into();
    assert!(!shows(
        &mut sim_elem(super::sidebar::search_tab(&app)),
        "Press Enter to search"
    ));
}

/// A function with no callers (or callees) says so under its name: the
/// panel showed the name and nothing else, as if it were still loading.
#[test]
fn a_call_tree_with_nothing_under_its_root_says_so() {
    let mut app = reader_app();
    for (direction, said) in [
        (crate::callgraph::Direction::Incoming, "No callers found"),
        (crate::callgraph::Direction::Outgoing, "No callees found"),
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
    app.proj.index_cap_note = Some("3 too large (over 512 KiB)".into());
    assert!(shows(
        &mut sim_elem(super::imports_tab(&app)),
        "Index incomplete: 3 too large (over 512 KiB)"
    ));
    app.proj.finder.open = true;
    app.proj.finder.mode = crate::finder::FinderMode::Symbols;
    assert!(shows(&mut sim_of(&app), "Index incomplete: 3 too large"));
    // A note belongs to its project's session, and leaves with it.
    app.forget_project_state();
    assert!(app.proj.index_cap_note.is_none());
    assert!(!shows(&mut sim_of(&app), "Index incomplete"));
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

/// A click on the file banner shows the file's note in the margin, from
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
    // main → run → work; test_work → work; alone is called by nobody.
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
                    call("test_work", vec![], vec![2]),
                    call("alone", vec![], vec![]),
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
        ]),
    );
    app.proj.symbol_index_rev += 1;
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("work"))).into(),
    );
    assert!(shows(&mut sim, "REACHED FROM"));
    assert!(shows(&mut sim, "main"), "the entry heads the chain");
    assert!(shows(&mut sim, "run"), "the step between");
    assert!(
        shows(&mut sim, "test_work"),
        "a test is a chain of its own, after the main"
    );
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
    assert!(shows(&mut sim, "No maps for this project."));
    drop(sim);
    app.proj.maps_error = Some("the host refused".into());
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(
        &mut sim,
        "Couldn't read the project's maps: the host refused"
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
    assert!(shows(&mut sim, "Database schema · 2 nodes · 1 arrow"));
    assert!(shows(&mut sim, "an arrow is a foreign key"));
    assert!(shows(&mut sim, "TABLE"));
    assert!(shows(&mut sim, "db/schema.sql:7"));
    assert!(shows(&mut sim, "user_id int → users"));
    assert!(shows(&mut sim, "→ users (user_id)"));
    drop(sim);
    app.proj.maps_selected = Some("routes".into());
    let mut sim = sim_elem(super::project_maps_body(&app));
    assert!(shows(&mut sim, "Routes · 1 node · 0 arrows"));
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
    assert!(shows(
        &mut sim,
        "4 types · 4 relations · 1 inheritance link"
    ));
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
    assert!(shows(&mut sim, "· 0 inheritance links"));
    drop(sim);
    crate::i18n::with_lang(crate::i18n::Lang::Fr, || {
        let mut sim = sim_elem(super::project_types_body(&app));
        assert!(shows(&mut sim, "· 0 lien d’héritage"));
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
    let sent = click(sim_of(&app), "📎 src/lib.rs · L4");
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
            },
            crate::walkthrough::Step {
                title: format!("{scope} two"),
                file: "src/lib.rs".into(),
                symbol: None,
                line: Some(2),
                narration: "n".into(),
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
        assert!(shows(&mut sim, "2 cells · 1 with output · python"));
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
    assert!(shows(&mut sim, "01234567"), "the short sha");
    assert!(shows(&mut sim, "Ada"));
    assert!(shows(&mut sim, "Teach the parser about raw strings"));
    assert!(shows(&mut sim, "Exit"));
    assert!(shows(&mut sim, "Scope: whole file"));
    assert!(
        !shows(&mut sim, "Loading revision…"),
        "the live file stays on screen until the revision loads"
    );
    let exit = click(sim_of(&app), "Exit");
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

    // With no terms the page says what a term is and offers a rebuild.
    app.proj.docs.files.clear();
    app.proj.docs.generation += 1;
    app.explain_cache_mut().clear();
    let _ = app.update(Message::Glossary(GlossaryMsg::Open));
    assert!(on_page(&app, "No terms yet"));
    let sent = click(sim_of(&app), "Rebuild");
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
    let mut sim = sim_elem(super::connect::languages_section(&app));
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
        "Server programs",
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
                    name: "zvm · Zig 0.13.0".into(),
                    project: true,
                }],
            ),
        ]
        .into(),
        ..Default::default()
    }));
    let warning = "Runs the project's own code. Only for projects you trust.";
    let mut sim = sim_elem(super::connect::languages_section(&app));
    for shown in [
        "TypeScript and JavaScript",
        "Clew's built-in TypeScript 5",
        "The project's TypeScript 5.4.5 — runs the project's own code",
        "Zig",
        "zvm · Zig 0.13.0",
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
    let mut sim = sim_elem(super::connect::languages_section(&app));
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
                    "rbenv · Ruby 3.3.0",
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
    let mut sim = sim_elem(super::connect::languages_section(&app));
    for shown in [
        "Java",
        "JDK 21",
        "PHP",
        "PHP 8.2 — composer.json's floor",
        "C#",
        "src/App.sln",
        "The Ruby that runs Ruby LSP.",
        "rbenv · Ruby 3.3.0",
        "The ruby-lsp found on the PATH",
        "The Erlang Language Platform build to run.",
        "The elp found on the PATH",
        "The Haskell language server to run.",
        "The haskell-language-server-wrapper found on the PATH",
    ] {
        assert!(sim.find(shown).is_ok(), "{shown:?} is shown");
    }
    let _ = sim.click("rbenv · Ruby 3.3.0");
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
        let mut sim = sim_elem(super::connect::languages_section(&app));
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
    let mut sim = sim_elem(super::connect::languages_section(&app));
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
        sent.iter().any(|m| matches!(
            m,
            Message::Settings(crate::SettingsMsg::OpenAt(
                crate::SettingsPage::LanguageModel
            ))
        )),
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
    // A line of it not committed: said so.
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
    assert!(shows(&mut sim_of(&app), "3 references"));
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
    assert!(shows(&mut sim_of(&app), "4 references"));
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
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "New session"));
        assert!(shows(&mut sim, "How is a host connected?"));
        assert!(shows(&mut sim, "2 min ago · 1 question"));
        assert!(shows(&mut sim, "2 h ago · 2 questions"));
    }
    let sent = click(sim_of(&app), "How is a host connected?");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::SwitchSession(2)))),
        "{sent:?}"
    );
    let sent = click(sim_of(&app), "New session");
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Ask(AskMsg::NewSession))),
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
            !shows(&mut sim, "2 min ago · 1 question"),
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
    // On the card, in the composer: the same, Esc left to the pill.
    app.ask_view = crate::AskView::Compact;
    let mut sim = sim_of(&app);
    assert!(shows(&mut sim, "Speaking"), "on the card too");
    assert!(!shows(&mut sim, "Esc"));
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
    app.ask_view = crate::AskView::Compact;
    assert!(shows(&mut sim_of(&app), "Listening"), "on the card too");
    app.voice.phase = VoicePhase::Transcribing { token: 1 };
    assert!(shows(&mut sim_of(&app), "Transcribing"));
    // On the card, the microphone's place holds the way to cancel; on the
    // pill, as while listening, Esc.
    let sent = click_right_of(
        || sim_of(&app),
        "Transcribing",
        |m| matches!(m, Message::Voice(crate::VoiceMsg::Cancel)),
    );
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Voice(crate::VoiceMsg::Cancel))),
        "{sent:?}"
    );
    app.ask_view = crate::AskView::Hidden;
    {
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, "Transcribing"));
        assert!(!shows(&mut sim, "Esc"));
    }
    app.ask_view = crate::AskView::Compact;
    app.voice.phase = VoicePhase::Idle;
    app.voice.confirm = Some("Explain All".into());
    assert!(shows(&mut sim_of(&app), "Run “Explain All”?"));
    assert!(matches!(
        click(sim_of(&app), "Run").as_slice(),
        [Message::Voice(crate::VoiceMsg::Confirm)]
    ));
    // On the card: what was last heard, and what came of it.
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
    let mut sim = sim_of(&app);
    assert!(!shows(&mut sim, "“open the call graph”"));
    assert!(shows(&mut sim, "Ask"));
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
        click(
            sim_elem(super::voice_section(&app)),
            "Medium (Large v3 Turbo)"
        )
        .as_slice(),
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
    assert!(matches!(
        click(sim_elem(super::voice_section(&app)), "Remove").as_slice(),
        [Message::Voice(crate::VoiceMsg::RemoveModel("base.en"))]
    ));
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
/// view opened by what shows it (a value traced, the import graph, a
/// walkthrough, a symbol's docs) is named over it, with the way back to
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
        (crate::SidebarTab::Walk, "Walkthroughs"),
        (crate::SidebarTab::Docs, "API docs"),
        (crate::SidebarTab::Notes, "Reading notes"),
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
/// toggle for it — and a pick closes the menu.
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
    assert!(!app.show_view_menu, "the pick closed the menu");
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
            line(DiffKind::Hunk, "@@ -1,1 +1,2 @@"),
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
    // The first build is asked for in words.
    app.proj.embed_index = crate::embed::Index::default();
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
    let head = "src/lib.rs  ·  vs HEAD";
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

/// The ⋯ menu fits the window: in one short enough for the strip to fall
/// back to ⋯, it is raised to end above the window's foot (its last row
/// shown, and clicked), and in one shorter than the menu, its rows scroll.
/// Hung from its button, its last rows ran past the foot, out of reach.
#[test]
fn the_tools_menu_fits_a_short_window() {
    for height in [520.0, 600.0] {
        let size = Size::new(1280.0, height);
        let mut app = reader_app();
        app.window_width = size.width;
        app.window_height = size.height;
        assert!(!super::tools_on_strip(&app));
        app.show_tools_menu = true;
        let top = super::tools_menu_top(&app);
        assert!(top >= super::TOOLBAR_H, "over the top bar: {top}");
        let mut sim = sim_sized(&app, size);
        let last =
            text_at(&mut sim, |t| t == "Keyboard Shortcuts", 0).expect("the last row is drawn");
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
                .any(|m| matches!(m, Message::Window(WindowMsg::OpenShortcuts))),
            "its last row clicked at {height}"
        );
    }
    // Shorter than the menu: its rows scroll in what there is.
    let mut app = reader_app();
    app.window_height = 360.0;
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
/// on the strip.
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
    let sent = click(sim_of(&app), "Explain All");
    assert!(
        sent.iter().any(|m| matches!(
            m,
            Message::Settings(crate::SettingsMsg::OpenAt(
                crate::SettingsPage::LanguageModel
            )) | Message::Project(crate::ProjectMsg::SetupShow(_))
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
    assert!(!shows(&mut sim_of(&app), "Explain All"));
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
    let sent = click(sim_elem(super::overview_home(&app)), "Generate overview");
    assert!(
        matches!(
            sent.as_slice(),
            [Message::Overview(crate::OverviewMsg::Generate)]
        ),
        "{sent:?}"
    );
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
