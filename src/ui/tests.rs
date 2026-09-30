//! View-layer tests that drive the real widget tree the way the runtime does:
//! build the view, feed it events, carry its widget state (`Cache`) across
//! rebuilds, and read that state back through widget operations.

use std::path::PathBuf;
use std::sync::Arc;

use crate::{
    AskMsg, CallsMsg, ConnectMsg, ContentMsg, DebugMsg, EditorMsg, GraphMsg, HoverMsg, NavMsg,
    ProjectMsg, ReadingMsg, ServerMsg, Stamp, TutorialMsg, WalkMsg, WindowMsg,
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
/// still fits — its form is a scrollable bounded by the window, taller in
/// content than on screen, and the wheel reaches the rest of it.
#[test]
fn the_settings_modal_fits_the_smallest_window_and_scrolls() {
    let min = crate::window_settings()
        .min_size
        .expect("a minimum window size");
    let mut app = reader_app();
    app.window_width = min.width;
    app.window_height = min.height;
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
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
        // The form: the scrollable inside the (narrower) modal panel.
        let form = all
            .0
            .into_iter()
            .find(|(b, _, _)| b.width <= super::SETTINGS_W + 1.0 && b.x > 0.0);
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
        sim.into_messages().collect()
    };
    let mut app = reader_app();
    let _ = app.update(Message::Settings(crate::SettingsMsg::Open));
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
    let mut app = blank_app();
    app.window_width = WINDOW.width;
    app.window_height = WINDOW.height;
    app.show_left_sidebar = true;
    app.show_bottom = false;
    let first_tab = super::SIDEBAR_TABS[0].0.to_string();
    let check = |app: &App| {
        let region = super::region_rect(app, crate::app::tutorial::Anchor::Sidebar)
            .expect("the sidebar is shown");
        let mut sim = sim_of(app);
        let wanted = first_tab.clone();
        let tab = text_at(&mut sim, move |t| t == wanted, 0).expect("the sidebar's tabs");
        assert!(
            tab.y >= region.y && tab.y - region.y <= 10.0,
            "the region starts at {} but the sidebar's first tab is drawn at {}",
            region.y,
            tab.y
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
    assert!(shows(&mut sim, "Downloading clew 9.9.9"));
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

/// A19: the overview's module map says what dragging it does in the mode it
/// is drawn in — it said "orbit" while drawn flat, where a drag pans.
#[test]
fn the_overview_map_caption_follows_its_mode() {
    let mut app = reader_app();
    app.proj.overview.showing = true;
    app.proj.overview.markdown = Some("## What it does\nA thing.".into());
    let node = |name: &str| crate::graphlayout::NodeInput {
        label: name.into(),
        file: PathBuf::from("/nonexistent/clew-ui-test").join(name),
        weight: 1.0,
        cyclic: false,
    };
    app.proj.overview.map = Some(crate::graphlayout::layout(
        vec![node("a.rs"), node("b.rs")],
        vec![(0, 1)],
    ));
    for (graph_3d, says, never) in [
        (false, "drag to pan", "orbit"),
        (true, "drag to orbit", "pan"),
    ] {
        app.graph_3d = graph_3d;
        let mut sim = sim_of(&app);
        assert!(shows(&mut sim, says), "3D {graph_3d}: no {says:?}");
        assert!(
            text_at(
                &mut sim,
                |t| t.starts_with("size = how connected") && t.contains(never),
                0
            )
            .is_none(),
            "3D {graph_3d}: the caption says {never:?}"
        );
    }
}

/// E2-12: the toolbar geometry the tutorial's spotlight places its holes with
/// is checked against the DRAWN toolbar — a click at each computed icon
/// center reaches that icon's own action — not re-derived from the formula it
/// is computed by.
#[test]
fn the_toolbar_geometry_matches_what_is_drawn() {
    use super::{CORE_TOOLS, TOOLBAR_H, toolbar_icon_center_x, toolbar_more_center_x};
    let app = reader_app();
    let expected: [fn(&Message) -> bool; CORE_TOOLS] = [
        |m| matches!(m, Message::Overview(crate::OverviewMsg::Show)),
        |m| matches!(m, Message::Overview(crate::OverviewMsg::ShowStats)),
        |m| matches!(m, Message::Ask(AskMsg::Toggle)),
        |m| matches!(m, Message::Debug(DebugMsg::Start)),
        |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectCalls))
            )
        },
        |m| {
            matches!(
                m,
                Message::Graph(GraphMsg::OpenOverlay(crate::Overlay::ProjectImports))
            )
        },
        |m| matches!(m, Message::Settings(crate::SettingsMsg::Open)),
    ];
    let click_at = |x: f32| {
        let mut sim = sim_of(&app);
        sim.point_at(Point::new(x, TOOLBAR_H / 2.0));
        let _ = sim.simulate(iced_test::simulator::click());
        sim.into_messages().collect::<Vec<_>>()
    };
    for (i, is_expected) in expected.iter().enumerate() {
        let sent = click_at(toolbar_icon_center_x(WINDOW.width, i));
        assert!(
            sent.iter().any(is_expected),
            "icon {i}'s computed center clicked {sent:?}"
        );
    }
    let sent = click_at(toolbar_more_center_x(WINDOW.width));
    assert!(
        sent.iter()
            .any(|m| matches!(m, Message::Window(WindowMsg::ToggleToolsMenu))),
        "the ⋯ button's computed center clicked {sent:?}"
    );

    // The ⋯ menu's rows: each computed row rect holds that row's label.
    let mut app = reader_app();
    app.show_tools_menu = true;
    for (row, label) in [
        (super::ToolsRow::OutlineSummaries, "Outline summaries"),
        (super::ToolsRow::Minimap, "Minimap"),
        (super::ToolsRow::ExplainAll, "Explain All"),
    ] {
        let rect = super::tools_menu_rows_rect(WINDOW.width, row.index(), 1);
        let mut sim = sim_of(&app);
        let at = text_at(&mut sim, move |t| t == label, 0)
            .unwrap_or_else(|| panic!("{label} is not drawn"));
        assert!(
            rect.contains(at.center()),
            "{label} is drawn at {at:?}, outside its computed row {rect:?}"
        );
    }
}

/// E2-9 / E2-12: the tutorial's spotlight frames what the view DRAWS — found
/// on screen — and never a panel the view has hidden: the right panel is not
/// drawn for a split, a narrow window, or no file, whatever
/// `show_right_panel` says, and the divider strips beside the side panels
/// belong to neither the panel nor the reader.
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
    let mut sim = sim_of(&app);
    let outline = text_at(&mut sim, |t| t == "OUTLINE", 0).expect("the right panel is drawn");
    let bottom = region_rect(&app, Anchor::RightBottom).expect("a right-panel hole");
    assert!(
        bottom.contains(outline.center()),
        "{outline:?} outside {bottom:?}"
    );
    let code =
        text_at(&mut sim, |t| t.contains("Spotlight heading"), 0).expect("the document is drawn");
    let main = region_rect(&app, Anchor::Main).unwrap();
    assert!(main.contains(code.center()), "{code:?} outside {main:?}");
    let sidebar = region_rect(&app, Anchor::Sidebar).unwrap();
    assert_eq!(
        main.x - (sidebar.x + sidebar.width),
        crate::resize::THICKNESS,
        "the divider is neither sidebar nor reader"
    );
    assert_eq!(bottom.x - (main.x + main.width), crate::resize::THICKNESS);

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
    ] {
        let mut hidden = reader_app();
        hidden.show_right_panel = true;
        hide(&mut hidden);
        let mut sim = sim_of(&hidden);
        assert!(
            !shows(&mut sim, "OUTLINE"),
            "{why}: the view hides the panel"
        );
        assert!(
            region_rect(&hidden, Anchor::RightTop).is_none()
                && region_rect(&hidden, Anchor::RightBottom).is_none(),
            "{why}: the spotlight framed a panel that is not drawn"
        );
    }
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

    app.show_tools_menu = true;
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Window(WindowMsg::ToggleToolsMenu))
    ));

    app.show_shortcuts = true;
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Window(WindowMsg::CloseShortcuts))
    ));

    app.pending_consent = Some(PathBuf::from("/nonexistent/clew-ui-test"));
    assert!(matches!(
        super::escape_message(&app),
        Some(Message::Project(ProjectMsg::ConsentDenied))
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
    });
    app.show_tools_menu = true;
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
fn sim_of(app: &App) -> Sim<'_> {
    iced_test::Simulator::with_size(Default::default(), WINDOW, super::view(app))
}

/// One element on its own (default simulator size).
fn sim_elem(elem: Element<'_, Message>) -> Sim<'_> {
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

/// Click the `nth` text equal to `label` and return what the click sent.
fn click_nth(mut sim: Sim<'_>, label: &str, nth: usize) -> Vec<Message> {
    let wanted = label.to_string();
    let bounds = text_at(&mut sim, move |t| t == wanted, nth)
        .unwrap_or_else(|| panic!("{label:?} (#{nth}) is not on screen"));
    sim.point_at(bounds.center());
    let _ = sim.simulate(iced_test::simulator::click());
    sim.into_messages().collect()
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

/// I10: the shortcuts modal lists every rebindable action — the ten added in
/// wave 1 included — each with its chord.
#[test]
fn the_shortcuts_modal_lists_every_action_with_its_chord() {
    use crate::keymap::Action;
    let mut app = reader_app();
    app.show_shortcuts = true;
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
    assert!(shows(&mut sim, "~/.ssh/known_hosts is not changed"));
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
    assert!(app.status.contains("Forgot 1 key"), "{}", app.status);
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
    let mut sim = sim_elem(super::lsp_consent_modal(&consent, 2));
    assert!(shows(&mut sim, "2 more installs are waiting"));
    let mut sim = sim_elem(super::lsp_consent_modal(&consent, 0));
    assert!(shows(&mut sim, "rust-analyzer 1"));
    assert!(!shows(&mut sim, "waiting for your answer"));
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
    assert!(shows(&mut sim, "41 more entries"));
    let complete = browser(0);
    let mut sim = sim_elem(super::remote_browser_view(&complete));
    assert!(shows(&mut sim, "src"));
    assert!(!shows(&mut sim, "more entr"));
    assert_eq!(
        super::omitted_note(1).as_deref(),
        Some("… and 1 more entry, past the listing's cap")
    );
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
    assert!(shows(&mut sim, "holds your AI API keys"));
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
    assert!(shows(&mut sim_of(&app), "does not hold your AI API keys"));
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
    let sent = click_nth(sim_elem(super::debug_panel(&app)), "✕", 1);
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
    assert!(safe.find("hover shows values").is_ok());
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
    let sent = click(sim, "▾");
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
            sim.find("⚠ this equation could not be rendered — its source:")
                .is_ok()
        );
        assert!(
            sim.find("⚠ this diagram could not be rendered — its source:")
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
        r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"file://{}","diagnostics":[{{"range":{{"start":{{"line":1,"character":3}},"end":{{"line":1,"character":9}}}},"severity":1,"message":"boom"}}]}}}}"#,
        root.join("src/lib.rs").display()
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
    assert!(shows(&mut sim_of(&app), "LSP ready"));
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
    assert!(shows(&mut sim_of(&app), "LSP ⚠ state unreadable"));
    assert!(
        !underlined(&app),
        "nothing is read past a poisoned snapshot"
    );
    app.server_panel = true;
    app.proj.project_languages = vec!["rust".into()];
    assert!(shows(
        &mut sim_of(&app),
        "state unreadable after an internal error"
    ));

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
    assert!(shows(&mut sim, "an entry point: main"));
    drop(sim);
    let mut sim = sim_elem(
        iced::widget::Column::with_children(super::call_flow_rows(&app, &node("alone"))).into(),
    );
    assert!(shows(&mut sim, "no entry point reaches this"));
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

fn doc_file(rel: &str, items: &[(&str, bool)]) -> clew_protocol::DocFile {
    serde_json::from_value(serde_json::json!({
        "rel": rel,
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
    app.apply_docs(files);
    let first = app.proj.docs.generation;
    assert!(shows(&mut sim_of(&app), "src/main.rs"));
    let _ = app.update(Message::Noop);
    assert_eq!(app.proj.docs.generation, first, "nothing changed");
    app.apply_docs(vec![doc_file("src/other.rs", &[("other", true)])]);
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
    let sent: Vec<Message> = sim.into_messages().collect();
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
    app.show_bottom = true;
    app.bottom_tab = crate::BottomTab::Ask;
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
    let sent = click(sim_of(&app), "prod");
    assert!(
        matches!(sent.as_slice(), [Message::Connect(ConnectMsg::ToSaved { user_host, port: 2222 })]
            if *user_host == saved.user_host()),
        "{sent:?}"
    );
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
        matches!(sent.as_slice(), [Message::Nav(NavMsg::FinderPick { abs, line: Some(12) })]
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
    assert!(shows(&mut sim, "scope: whole file"));
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
