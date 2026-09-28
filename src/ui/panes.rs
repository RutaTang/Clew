//! Code/markdown panes, time-travel & diff views, outline, find bar.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

pub(crate) fn editor_shell(inner: Element<'_, Message>) -> Element<'_, Message> {
    container(inner)
        .width(Fill)
        .height(Fill)
        .style(theme::editor)
        .into()
}

/// One code pane.
///
/// Its skeleton is fixed — `column![header slot, stack![content, find slot]]`
/// — whatever is showing, so opening the find bar or splitting the view never
/// moves the content to another position in the widget tree. (Both used to:
/// the find bar wrapped the content in a new `Stack` and the split header
/// pushed it down a slot, and either rebuilt the code view's scrollable at the
/// top of the file.)
pub(crate) fn pane_view(app: &App, pane: usize) -> Element<'_, Message> {
    let time_travel = time_travel_on_screen(app).filter(|_| pane == app.proj.active);
    let content: Element<'_, Message> = match (time_travel, &app.proj.panes[pane]) {
        // Time Travel takes over the active pane (its own read-only view). The
        // live viewer is the fallback so the code stays visible while the first
        // historical revision loads (no blank "flash" on entry).
        (Some(tt), live) => time_travel_view(app, tt, live.as_ref()),
        (None, Some(v)) => {
            // The diff view replaces the code of the active pane's file.
            if pane == app.proj.active
                && let Some(d) = &app.proj.diff
                && d.abs == v.abs
            {
                diff_view(app, pane, d)
            } else if let Some(doc) = v.notebook.as_deref() {
                // A Jupyter notebook renders as its native cell view.
                notebook_pane(app, pane, v, doc)
            } else if let Some(md) = v.md.as_ref().filter(|_| !v.show_source) {
                // A markdown file renders as a document; a toggle in the
                // breadcrumb switches to the raw source.
                markdown_pane(app, pane, v, md)
            } else {
                code_pane(app, pane, v)
            }
        }
        (None, None) => mouse_area(empty_state(
            Glyph::Note,
            "No file open",
            "Pick a file from the tree, or press ⌘P.",
            None,
        ))
        .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
        .into(),
    };

    // The find bar floats over the top-right of the active pane.
    let find: Element<'_, Message> =
        if app.proj.find.open && pane == app.proj.active && time_travel.is_none() {
            find_bar(app)
        } else {
            slot()
        };
    let header: Element<'_, Message> = if app.proj.split {
        pane_header(app, pane)
    } else {
        slot()
    };
    column![header, stack![editor_shell(content), find]]
        .width(Fill)
        .height(Fill)
        .into()
}

/// What `pane`, showing `abs` — as code, a rendered document, a notebook or
/// a diff — reports of the reader's own scrolling ([`reader_scroll()`]):
/// `ReaderScrolled`, while a walkthrough step waits to settle in `abs` and
/// the scroll would still supersede it. That is all it is for, so the rest
/// of the time nothing is watched, and a scroll costs no message.
pub(crate) fn reader_scroll_report(
    app: &App,
    pane: usize,
    abs: &std::path::Path,
) -> Option<Message> {
    app.proj
        .walk
        .waits_in(abs)
        .then_some(Message::Editor(EditorMsg::ReaderScrolled(pane)))
}

// ------------------------------------------------------------- time travel

/// The time-travel session, when it is on screen: it takes over the active
/// pane while that pane shows the session's file, and nothing covers the
/// panes ([`pane_cover`]). A session outlives both — the reader focuses the
/// other half of a split, or opens the overview — and shows again once its
/// file does; meanwhile what the reader sees is something else. The view
/// ([`pane_view`]) and whatever acts for the reader on the view — the keys
/// (`App::handle_key`), the copy, the status bar — go through this one
/// choice, so a session off screen never takes a key meant for what is on it.
pub(crate) fn time_travel_on_screen(app: &App) -> Option<&TimeTravel> {
    let tt = app.proj.time_travel.as_ref()?;
    let shown = pane_cover(app).is_none() && app.active_viewer().is_some_and(|v| v.abs == tt.abs);
    shown.then_some(tt)
}

/// The pane the time-travel session is drawn in — the active one, while the
/// session is on screen ([`time_travel_on_screen`]) — and `None` while it is
/// off screen, where no pane shows it.
pub(crate) fn time_travel_pane(app: &App) -> Option<usize> {
    time_travel_on_screen(app).map(|_| app.proj.active)
}

/// The git time-travel view: a commit banner on top, the historical (read-only)
/// code in the middle, and a timeline scrubber at the bottom. `live` is the
/// pane's current viewer, shown until the first historical revision loads.
pub(crate) fn time_travel_view<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    live: Option<&'a Viewer>,
) -> Element<'a, Message> {
    let commit = tt.commits.get(tt.idx);
    // The historical viewer once ready; otherwise the live one so the code area
    // never goes blank on entry.
    let code: Element<'a, Message> = match tt.viewer.as_ref().or(live) {
        Some(hv) => time_travel_code(app, tt, hv),
        None => center(text("Loading revision…").size(ts::BASE).color(theme::dim())).into(),
    };
    // The story sits in a slot that is always there (zero-size until it is
    // asked for): pushed in ahead of the code, it moved the historical view's
    // scrollable down a position, iced rebuilt it at the top, and its scroll
    // report overwrote where the reader was.
    let story: Element<'a, Message> = match &tt.story {
        Some(story) => time_travel_story(app, tt, story),
        None => slot(),
    };
    column![
        time_travel_banner(tt, commit),
        story,
        container(code).width(Fill).height(Fill),
        time_travel_bar(tt),
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

/// The commit banner: sha · author · when, the subject, and the AI "what & why".
pub(crate) fn time_travel_banner<'a>(
    tt: &'a TimeTravel,
    commit: Option<&'a crate::git::HistCommit>,
) -> Element<'a, Message> {
    // A tidy "Exit  esc" — the little keycap reads as a control and teaches the
    // shortcut, instead of a bare ✕ glyph.
    let keycap = container(text("esc").size(ts::CAPTION).color(theme::fg_muted()))
        .padding(Padding {
            top: 1.0,
            right: 5.0,
            bottom: 1.0,
            left: 5.0,
        })
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::bg_active().into()),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: theme::hairline(),
            },
            ..Default::default()
        });
    let exit = button(
        row![
            text("Exit").size(ts::SMALL).color(theme::fg_muted()),
            keycap
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravel(TimeTravelMsg::Exit));

    let Some(c) = commit else {
        let head = row![
            glyph::icon(Glyph::TimeTravel, theme::accent(), 15.0),
            text("Time Travel").size(ts::BODY).color(theme::fg()),
            space().width(Fill),
            exit,
        ]
        .spacing(8)
        .align_y(iced::Center);
        return container(head)
            .padding([7, 12])
            .width(Fill)
            .style(theme::pane_header)
            .into();
    };

    let short: String = c.sha.chars().take(8).collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let head = row![
        glyph::icon(Glyph::TimeTravel, theme::accent(), 15.0),
        text(short)
            .size(ts::BODY)
            .color(theme::accent())
            .font(Font::MONOSPACE),
        text(format!(
            "{}  ·  {}",
            c.author,
            crate::git::relative_time(c.time, now)
        ))
        .size(ts::SMALL)
        .color(theme::dim()),
        space().width(Fill),
        exit,
    ]
    .spacing(10)
    .align_y(iced::Center);

    let subject = text(c.subject.clone())
        .size(ts::BODY)
        .color(theme::fg())
        .wrapping(Wrapping::Word);

    let why: Element<'a, Message> = if tt.why_loading {
        text("Summarizing…")
            .size(ts::SMALL)
            .color(theme::dim())
            .into()
    } else if let Some(w) = tt.why.get(&c.sha) {
        text(w.clone())
            .size(ts::SMALL)
            .color(theme::fg_muted())
            .wrapping(Wrapping::Word)
            .into()
    } else {
        button(text("What & why?").size(ts::SMALL).color(theme::accent()))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(Message::TimeTravel(TimeTravelMsg::Why))
            .into()
    };

    container(column![head, subject, why].spacing(5))
        .padding([7, 12])
        .width(Fill)
        .style(theme::pane_header)
        .into()
}

/// The historical code — a read-only code view of the file at this revision,
/// with the commit's added/changed lines marked in the gutter.
pub(crate) fn time_travel_code<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    hv: &'a Viewer,
) -> Element<'a, Message> {
    let lh = app.line_height();
    let row0 = (tt.scroll_y / lh) as usize;
    let sticky = crate::analyze::sticky_headers(&hv.folds, hv.line_at_row(row0), 5);
    // Read-only, but clicking still places a caret and dragging selects (for
    // reading / copying). Right-click has no menu in a historical view.
    let code = CodeView::new(
        &hv.lines,
        hv.max_cols,
        app.font_size,
        lh,
        theme::fg(),
        |(line, col)| Message::TimeTravel(TimeTravelMsg::SelectStart { line, col }),
        |(line, col)| Message::TimeTravel(TimeTravelMsg::SelectDrag { line, col }),
        |_, _| Message::Noop,
    )
    .cursor(hv.caret)
    .selection(hv.selection_ordered())
    .sticky(sticky)
    .folds(hv.visible_rows(), &hv.fold_header_set, &hv.collapsed)
    .indent_guides(true)
    .git_gutter(hv.git.as_deref());
    // The live pane's scroll id, so the scroll operations aimed at the active
    // pane reach this view too. An id does not carry widget state across the
    // enter/exit swap — this scrollable sits elsewhere in the tree than the
    // live one, so iced builds it fresh — which is why entering and leaving
    // re-issue `scroll_to` from `TimeTravel::scroll_y` / `Viewer::scroll_y`.
    scrollable(code)
        .id(code_scroll_id(app.proj.active))
        .on_scroll(|v| Message::TimeTravel(TimeTravelMsg::Scrolled(v)))
        .direction(both_scroll())
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill)
        .into()
}

/// The timeline scrubber: older/newer steps, a slider, position, scope toggle,
/// and (for a function scope) the "story of this function" narrative button.
pub(crate) fn time_travel_bar(tt: &TimeTravel) -> Element<'_, Message> {
    let n = tt.commits.len();
    let last = n.saturating_sub(1);
    // `then` (lazy) — not `then_some` — so `idx - 1` isn't evaluated (underflowing
    // usize) when idx is 0.
    let older = (tt.idx < last).then(|| Message::TimeTravel(TimeTravelMsg::Goto(tt.idx + 1)));
    let newer = (tt.idx > 0).then(|| Message::TimeTravel(TimeTravelMsg::Goto(tt.idx - 1)));
    let step = |g: Glyph, msg: Option<Message>| {
        let on = msg.is_some();
        let mut b = button(glyph::icon(
            g,
            if on { theme::fg() } else { theme::dim() },
            15.0,
        ))
        .style(theme::toolbar_button)
        .padding([2, 8]);
        if let Some(m) = msg {
            b = b.on_press(m);
        }
        b
    };
    // Slider: left = oldest, right = newest; position = last - idx.
    let sl = slider(0.0..=last.max(1) as f32, (last - tt.idx) as f32, move |v| {
        let p = (v.round() as usize).min(last);
        Message::TimeTravel(TimeTravelMsg::Goto(last - p))
    })
    .step(1.0)
    .width(Fill);

    let scope_label = match &tt.scope {
        TimeScope::Symbol { name, kind, .. } => format!("{} {name}", short_kind(kind)),
        TimeScope::File => "whole file".to_string(),
    };
    // Clicking toggles between the whole file and the block under the caret.
    let scope_btn = button(
        text(format!("scope: {scope_label}  ⇄"))
            .size(ts::SMALL)
            .color(theme::fg_muted()),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravel(TimeTravelMsg::ToggleScope));

    let story: Element<'_, Message> = if matches!(tt.scope, TimeScope::Symbol { .. }) {
        if tt.story_loading {
            text("Story…").size(ts::SMALL).color(theme::dim()).into()
        } else {
            let label = if tt.story.is_some() {
                "Hide story"
            } else {
                "Story"
            };
            let color = if tt.story.is_some() {
                theme::fg_muted()
            } else {
                theme::accent()
            };
            button(text(label).size(ts::SMALL).color(color))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::TimeTravel(TimeTravelMsg::Story))
                .into()
        }
    } else {
        space().into()
    };

    container(
        row![
            chrome_tip(
                step(Glyph::ArrowLeft, older),
                "Older commit",
                Some("⌘←".to_string())
            ),
            sl,
            chrome_tip(
                step(Glyph::ArrowRight, newer),
                "Newer commit",
                Some("⌘→".to_string())
            ),
            text(format!("{} / {}", tt.idx + 1, n))
                .size(ts::SMALL)
                .color(theme::dim()),
            space().width(16),
            scope_btn,
            story,
        ]
        .spacing(10)
        .align_y(iced::Center),
    )
    .padding([5, 12])
    .width(Fill)
    .style(theme::statusbar)
    .into()
}

/// The "story of this function" narrative panel (AI), shown above the code.
pub(crate) fn time_travel_story<'a>(
    app: &'a App,
    tt: &'a TimeTravel,
    story: &'a [crate::PreparedSeg],
) -> Element<'a, Message> {
    let name = tt.scope.symbol_name().unwrap_or("this block");
    let header = row![
        column![
            text(format!("Story of {name}"))
                .size(ts::BODY)
                .color(theme::accent()),
            // `git log -L` only follows the block's CURRENT lines, so earlier
            // rewrites may not be attributed — say so, so it's not read as a full
            // biography.
            text("from the commits that touched these lines")
                .size(ts::CAPTION)
                .color(theme::dim()),
        ]
        .spacing(1),
        space().width(Fill),
        button(text("✕").size(ts::SMALL).color(theme::dim()))
            .style(theme::toolbar_button)
            .padding([1, 6])
            .on_press(Message::TimeTravel(TimeTravelMsg::Story)),
    ]
    .align_y(iced::Center);
    let body = scrollable(
        Column::with_children(render_prepared(app, story))
            .spacing(8)
            .width(Fill),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Length::Fixed(200.0));
    container(column![header, body].spacing(6))
        .padding([8, 12])
        .width(Fill)
        .style(theme::modal_panel)
        .into()
}

/// The unified diff of the active file versus `HEAD`, colored by line kind,
/// in `pane` (the active one).
pub(crate) fn diff_view<'a>(
    app: &'a App,
    pane: usize,
    d: &'a crate::DiffState,
) -> Element<'a, Message> {
    use crate::git::DiffKind;

    let header = container(
        row![
            text(format!("{}  ·  vs HEAD", d.rel))
                .size(ts::BODY)
                .color(theme::accent()),
            space().width(Fill),
            button(text("✕ close").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Editor(EditorMsg::ToggleDiff)),
        ]
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 5.0,
        right: 8.0,
        bottom: 5.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    if d.lines.is_empty() {
        return column![
            header,
            center(
                text(format!("No uncommitted changes in {}", d.rel))
                    .size(ts::BASE)
                    .color(theme::dim()),
            )
        ]
        .width(Fill)
        .height(Fill)
        .into();
    }

    // Size every run to the widest line so the color tints span the full
    // content width and long lines become reachable via horizontal scroll —
    // measured, not guessed: the real monospace advance, and the columns the
    // widest line is drawn in (counted once, when the diff arrived — see
    // `DiffState::new` — not per repaint).
    let run_width = row_width(
        d.max_cols,
        mono_advance(app.font_size),
        DIFF_PAD_LEFT + DIFF_PAD_RIGHT,
    );
    // One text block per run of same-kind lines rather than a widget per line:
    // the same picture (runs stack with no gap) at a fraction of the layout
    // work on a long diff — prepared once, borrowed here.
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    for run in &d.runs {
        let (bg, fg) = match run.kind {
            DiffKind::Add => (
                Some(theme::with_alpha(theme::success(), 0.14)),
                theme::success(),
            ),
            DiffKind::Remove => (
                Some(theme::with_alpha(theme::danger(), 0.14)),
                theme::danger(),
            ),
            DiffKind::Hunk => (
                Some(theme::with_alpha(theme::accent(), 0.12)),
                theme::accent(),
            ),
            DiffKind::Header => (None, theme::dim()),
            DiffKind::Context => (None, theme::fg()),
        };
        let mut cell = container(
            text(run.text.as_str())
                .font(Font::MONOSPACE)
                .size(app.font_size)
                .color(fg)
                .wrapping(Wrapping::None),
        )
        .width(Length::Fixed(run_width))
        .padding(Padding {
            top: 0.0,
            right: DIFF_PAD_RIGHT,
            bottom: 0.0,
            left: DIFF_PAD_LEFT,
        });
        if let Some(bg) = bg {
            cell = cell.style(move |_: &iced::Theme| container::Style {
                background: Some(bg.into()),
                ..container::Style::default()
            });
        }
        rows.push(cell.into());
    }
    if d.lines.len() > MAX_DIFF_ROWS {
        rows.push(
            text(format!("… {} more lines", d.lines.len() - MAX_DIFF_ROWS))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
        );
    }

    // A walkthrough step can open its file here — the diff stays on for the
    // file it was asked for — so the reader's scroll counts here as in the
    // code this stands in for.
    let body = reader_scroll(
        scrollable(Column::with_children(rows).padding([4, 0]))
            .direction(both_scroll())
            .style(theme::overlay_scrollbar)
            .width(Fill)
            .height(Fill),
        reader_scroll_report(app, pane, &d.abs),
    );

    column![header, body].width(Fill).height(Fill).into()
}

/// The most diff lines the view renders (the rest are summarized).
pub(crate) const MAX_DIFF_ROWS: usize = 8000;
/// Horizontal padding of a diff row.
const DIFF_PAD_LEFT: f32 = 10.0;
const DIFF_PAD_RIGHT: f32 = 8.0;
/// Columns a tab advances in the reader (the highlighter expands tabs to this
/// many spaces, so the diff draws them the same way).
const TAB_COLS: usize = 4;

/// A diff line as the view draws it: tabs expanded like the code view's, and a
/// blank line kept one space wide so it does not collapse.
pub(crate) fn diff_display_text(line: &str) -> String {
    if line.is_empty() {
        " ".to_string()
    } else {
        line.replace('\t', &" ".repeat(TAB_COLS))
    }
}

/// Monospace columns `line` occupies once drawn: tabs as [`TAB_COLS`], wide
/// glyphs (CJK, emoji) as two, zero-width marks as none.
pub(crate) fn display_cols(line: &str) -> usize {
    use unicode_width::UnicodeWidthChar;
    line.chars()
        .map(|c| match c {
            '\t' => TAB_COLS,
            c => c.width().unwrap_or(0),
        })
        .sum()
}

/// Width of a diff row wide enough for every line: the widest line's `cols`
/// (plus one of slack) at `advance` px each, and the row's padding.
pub(crate) fn row_width(cols: usize, advance: f32, padding: f32) -> f32 {
    (cols as f32 + 1.0) * advance + padding
}

/// Advance of one monospace glyph at `size`, measured by the same text engine
/// the renderer shapes with (the diff used to assume 0.6em). Memoized per size,
/// as the view asks on every repaint.
pub(crate) fn mono_advance(size: f32) -> f32 {
    thread_local! {
        static MEMO: std::cell::Cell<Option<(u32, f32)>> = const { std::cell::Cell::new(None) };
    }
    if let Some((bits, w)) = MEMO.get()
        && bits == size.to_bits()
    {
        return w;
    }
    let w = measured_mono_advance(size).unwrap_or(size * 0.6);
    MEMO.set(Some((size.to_bits(), w)));
    w
}

/// One monospace glyph's advance at `size` as the text engine shapes it, or
/// `None` when it measured nothing (no monospace face at all) — the case
/// [`mono_advance`] falls back to 0.6 em for.
pub(crate) fn measured_mono_advance(size: f32) -> Option<f32> {
    use iced::advanced::text::{self, Paragraph as _};
    let sample = <iced::Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: "0",
        bounds: iced::Size::INFINITE,
        size: size.into(),
        line_height: text::LineHeight::Absolute(size.into()),
        font: Font::MONOSPACE,
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    });
    Some(sample.min_bounds().width).filter(|w| *w > 0.0)
}

pub(crate) fn find_bar(app: &App) -> Element<'_, Message> {
    let count = if app.proj.find.query.is_empty() {
        String::new()
    } else if app.proj.find.matches.is_empty() {
        "0/0".to_string()
    } else {
        format!(
            "{}/{}",
            app.proj.find.current + 1,
            app.proj.find.matches.len()
        )
    };

    let input = text_input("Find in file…", &app.proj.find.query)
        .id(find_input_id())
        .on_input(|v| Message::Editor(EditorMsg::FindQueryChanged(v)))
        .on_submit(Message::Editor(EditorMsg::FindStep(1)))
        .size(ts::BASE)
        .padding([4, 8])
        .width(190);

    let btn = |label: &'static str, msg: Message| {
        button(text(label).size(ts::BASE))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };

    let bar = container(
        row![
            input,
            text(count).size(ts::SMALL).color(theme::dim()).width(46),
            btn("‹", Message::Editor(EditorMsg::FindStep(-1))),
            btn("›", Message::Editor(EditorMsg::FindStep(1))),
            btn("✕", Message::Editor(EditorMsg::FindClosed)),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .padding(6)
    .style(theme::modal_panel);

    // Pin to the top-right of the pane.
    container(bar)
        .width(Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .padding(Padding {
            top: 6.0,
            right: 16.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

pub(crate) fn pane_header(app: &App, pane: usize) -> Element<'_, Message> {
    let active = app.proj.active == pane;
    let title = app.proj.panes[pane]
        .as_ref()
        .map(|v| v.rel.as_str())
        .unwrap_or("—");
    mouse_area(
        container(
            text(title)
                .size(ts::SMALL)
                .color(if active {
                    theme::accent()
                } else {
                    theme::dim()
                })
                .wrapping(Wrapping::None),
        )
        .width(Fill)
        .padding([3, 8])
        .style(theme::pane_header),
    )
    .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
    .into()
}

pub(crate) fn welcome(app: &App) -> Element<'_, Message> {
    // On a live remote the hero invites browsing that host; otherwise it offers
    // opening local code or connecting out over SSH.
    let (subtitle, primary, secondary): (String, (&str, Message), (&str, Message)) =
        if app.connection.is_remote() {
            (
                format!("connected to {}", app.connection.label()),
                ("Browse folders…", Message::Connect(ConnectMsg::Open)),
                ("Disconnect", Message::Connect(ConnectMsg::Disconnect)),
            )
        } else {
            (
                // "clew" = the thread that guides you out of the labyrinth.
                "Find the thread through your codebase".to_string(),
                (
                    "Open Folder…",
                    Message::Project(ProjectMsg::OpenFolderPressed),
                ),
                ("Open Remote…", Message::Connect(ConnectMsg::Open)),
            )
        };

    let actions = row![
        button(text(primary.0.to_string()).size(ts::EMPHASIS))
            .style(theme::primary_button)
            .padding([8, 20])
            .on_press(primary.1),
        button(text(secondary.0.to_string()).size(ts::EMPHASIS))
            .style(theme::secondary_button)
            .padding([8, 20])
            .on_press(secondary.1),
    ]
    .spacing(10);

    // Brand lockup: the "C" mark on the left, the name on the right — same mark
    // as the app icon (minus the square), so the welcome reads as part of the app.
    // Tint the mark to a soft foreground tone so it reads on every theme — the
    // asset's own light-grey stroke washes out on light backgrounds.
    let mark = iced::widget::svg(iced::widget::svg::Handle::from_memory(MARK_SVG))
        .width(Length::Fixed(52.0))
        .height(Length::Fixed(52.0))
        .style(|_theme, _status| iced::widget::svg::Style {
            color: Some(theme::fg_muted()),
        });
    let brand = row![mark, text("Clew").size(ts::HERO).color(theme::fg_bright())]
        .spacing(14)
        .align_y(iced::Center);

    center(
        column![
            brand,
            space().height(4),
            text(subtitle).size(ts::BASE).color(theme::fg_muted()),
            space().height(22),
            actions,
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

/// The Clew "C" mark (monochrome arcs, transparent background) for the welcome
/// screen. Rendered with a theme-adaptive tint (see `welcome`), so the asset's
/// own stroke colour doesn't matter.
const MARK_SVG: &[u8] = include_bytes!("../../assets/icon/mark.svg");

/// Render a markdown file (`v`'s `items`) as a document — readmes,
/// changelogs — instead of raw source. Links open via the normal `OpenLink`
/// path; the `PaneFocused` mouse area keeps click-to-focus working like the
/// code view, and the reader's scroll counts as it does there
/// (`reader_scroll_report`).
pub(crate) fn markdown_pane<'a>(
    app: &App,
    pane: usize,
    v: &Viewer,
    items: &'a [iced::widget::markdown::Item],
) -> Element<'a, Message> {
    let doc = iced::widget::markdown::view(items, theme::markdown_settings())
        .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string())));
    let body = container(doc).padding([16, 28]).max_width(920);
    mouse_area(reader_scroll(
        scrollable(body)
            .width(Fill)
            .height(Fill)
            .style(theme::overlay_scrollbar),
        reader_scroll_report(app, pane, &v.abs),
    ))
    .on_press(Message::Editor(EditorMsg::PaneFocused(pane)))
    .into()
}

pub(crate) fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .proj
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // Debug: this file's breakpoints (and which are conditional), plus the
    // current stopped line (if here).
    let file_bps = app.debug.breakpoints.get(&v.abs);
    let breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| m.keys().copied().collect())
        .unwrap_or_default();
    let cond_breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| {
            m.iter()
                .filter(|(_, bp)| bp.condition.is_some())
                .map(|(l, _)| *l)
                .collect()
        })
        .unwrap_or_default();
    // Only an explicit refusal, never `None`: a breakpoint the adapter has not
    // answered about (no session yet, reply in flight) is not known to be dead.
    let unverified_breakpoints: std::collections::HashSet<usize> = file_bps
        .map(|m| {
            m.iter()
                .filter(|(_, bp)| bp.verified == Some(false))
                .map(|(l, _)| *l)
                .collect()
        })
        .unwrap_or_default();
    let debug_current = app
        .debug
        .session
        .as_ref()
        .and_then(|d| d.current.as_ref())
        .filter(|(p, _)| *p == v.abs)
        .map(|(_, line)| *line);

    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.proj.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let mut code = CodeView::new(
        &v.lines,
        v.max_cols,
        app.font_size,
        app.line_height(),
        theme::fg(),
        move |(line, col)| Message::Editor(EditorMsg::SelectStart { pane, line, col }),
        move |(line, col)| Message::Editor(EditorMsg::SelectDrag { pane, line, col }),
        move |(line, col), at| {
            Message::Hover(HoverMsg::ContextMenuOpened {
                pane,
                line,
                col,
                x: at.x,
                y: at.y,
            })
        },
    )
    .selection(v.selection_ordered())
    .cursor(cursor)
    .highlights(app.code_highlights(pane, v))
    .sticky(app.sticky_headers(v))
    .bookmarks(marked)
    .breakpoints(breakpoints)
    .cond_breakpoints(cond_breakpoints)
    .unverified_breakpoints(unverified_breakpoints)
    .debug_current(debug_current)
    .inlay_hints(v.inlay_hints(), theme::dim())
    .inactive(v.inactive_lines())
    // The summary of those two and the folds, kept current by the viewer's
    // setters — else the view re-derives it, hashing every chip, per build.
    .annotations(v.annotations())
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::Editor(EditorMsg::FoldToggle { pane, line }))
    .on_breakpoint(move |line| {
        Message::Debug(DebugMsg::BreakpointToggle {
            path: v.abs.clone(),
            line,
        })
    })
    .indent_guides(true)
    .git_gutter(v.git.as_deref())
    .blame(if pane == app.proj.active && app.code_focused {
        app.blame_annotation(v)
    } else {
        None
    })
    .on_hover(move |(line, col), at| {
        Message::Hover(HoverMsg::Requested {
            pane,
            line,
            col,
            x: at.x,
            y: at.y,
        })
    })
    .on_hover_end(|| Message::Hover(HoverMsg::Cleared));
    // The minimap is opt-in (toggle in the "More" menu); without the callback
    // the widget draws no minimap band at all.
    if app.show_minimap {
        code = code.on_minimap(move |fraction| {
            Message::Editor(EditorMsg::MinimapScrolled { pane, fraction })
        });
    }
    // Find matches are painted from the find state itself (sorted, borrowed),
    // not copied into the highlight list per view — see `code_highlights`.
    if pane == app.proj.active && app.proj.find.open {
        code = code.find_matches(&app.proj.find.matches, app.proj.find.current);
    }

    let scroller = reader_scroll(
        scrollable(code)
            .id(code_scroll_id(pane))
            .on_scroll(move |viewport| Message::Editor(EditorMsg::Scrolled(pane, viewport)))
            .direction(both_scroll())
            .style(theme::overlay_scrollbar)
            .width(Fill)
            .height(Fill),
        reader_scroll_report(app, pane, &v.abs),
    );

    // File TL;DR banner: a one-line "what is this file" from the explain cache,
    // pinned above the code. Dismissable (toggle back via the More menu). Its
    // slot is always there, so the summary arriving mid-read (Explain All
    // fills the cache as you scroll) does not rebuild the scroller below it.
    let banner = if app.show_file_banner {
        app.proj
            .explain
            .cache
            .get(&crate::explain::Node::File(v.abs.clone()))
            .filter(|c| !crate::explain::is_error_summary(&c.summary))
            .map(|c| file_banner(first_sentence(&crate::app::shown_summary(c))))
    } else {
        None
    };
    column![banner.unwrap_or_else(slot), scroller].into()
}

/// A one-line file summary pinned at the top of the code view.
pub(crate) fn file_banner<'a>(summary: String) -> Element<'a, Message> {
    container(
        row![
            text("›").size(ts::BODY).color(theme::accent()),
            text(summary)
                .size(ts::BODY)
                .color(theme::fg_muted())
                .width(Fill),
            button(text("✕").size(ts::SMALL).color(theme::dim()))
                .style(theme::toolbar_button)
                .padding([0, 6])
                .on_press(Message::Window(WindowMsg::ToggleFileBanner)),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .width(Fill)
    .padding([4, 10])
    .style(theme::panel)
    .into()
}

// ---------------------------------------------------------------- outline

/// Narrowest window the right panel is drawn in: below it, the reader gets
/// the width.
pub(crate) const RIGHT_PANEL_MIN_WINDOW_W: f32 = 950.0;

/// Whether the right panel is drawn: asked for, and room and a file for it.
/// The one test the view and the tutorial's spotlight both use — the
/// spotlight used to take `show_right_panel` alone and framed a panel the
/// view had hidden (a split, a narrow window, no file).
pub(crate) fn right_panel_shown(app: &App) -> bool {
    app.show_right_panel
        && !app.proj.split
        && app.window_width >= RIGHT_PANEL_MIN_WINDOW_W
        && app.active_viewer().is_some()
}

/// The right sidebar: a tabbed panel with an Outline tab and an Explain tab
/// (mirrors the left sidebar's tabs). Hidden on narrow/split windows or with no
/// file open.
pub(crate) fn right_panel(app: &App) -> Option<Element<'_, Message>> {
    if !right_panel_shown(app) {
        return None;
    }

    // One cursor-following reading-context panel — no tab-dance. The top follows
    // the caret: the current function's summary, call-flow and quick actions.
    // The bottom is the file's outline, an annotated table of contents with the
    // current symbol highlighted, so "where am I" and "what's around me" sit
    // together.
    // Equal split: the explanation (which can stream several blocks) gets as much
    // room as the outline, rather than being squeezed into the smaller share.
    let context = container(explain_content(app)).height(iced::Length::FillPortion(1));
    let outline = column![section_header("OUTLINE"), outline_content(app)]
        .height(iced::Length::FillPortion(1));

    Some(
        container(column![context, hairline(), outline])
            .width(Length::Fixed(app.right_width))
            .height(Fill)
            .style(theme::panel)
            .into(),
    )
}

/// A 1px horizontal divider spanning the panel width.
pub(crate) fn hairline() -> Element<'static, Message> {
    container(space().width(Fill).height(1))
        .width(Fill)
        .height(1)
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::hairline().into()),
            ..Default::default()
        })
        .into()
}

/// What an empty outline says: why there is nothing to list. A language
/// without an outline query, and one whose query does not compile (a grammar
/// bump broke it), are not files without symbols — the last used to read as
/// one, silently.
pub(crate) fn empty_outline_note(lang_key: Option<&str>) -> String {
    use crate::highlight::{Lang, tags_query};
    outline_note_for(
        lang_key
            .and_then(Lang::from_key)
            .map(|lang| tags_query(lang).map(|_| ())),
    )
}

/// [`empty_outline_note`] given how the language's outline query came out
/// (`None`: no language at all).
pub(crate) fn outline_note_for(query: Option<Result<(), crate::highlight::QueryError>>) -> String {
    use crate::highlight::QueryError;
    match query {
        Some(Err(QueryError::Invalid(e))) => {
            format!("Outline unavailable: the outline query does not compile ({e}).")
        }
        Some(Err(QueryError::Unsupported)) | None => "No outline for this language.".into(),
        Some(Ok(())) => "No symbols in this file.".into(),
    }
}

/// The Outline tab's content: the active file's symbols, click to jump.
pub(crate) fn outline_content(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return space().into();
    };
    if v.symbols.is_empty() {
        return container(
            text(empty_outline_note(v.lang_key))
                .size(ts::SMALL)
                .color(theme::dim()),
        )
        .padding(10)
        .into();
    }
    // The symbol the reading cursor is currently inside, to highlight its row.
    let current = match &app.proj.explain.view {
        Some(crate::explain::Node::Function {
            file,
            name,
            ordinal,
        }) if *file == v.abs => Some((name.as_str(), *ordinal)),
        _ => None,
    };
    // Rebuilt on every repaint, so nothing below may scan a whole list per
    // symbol (that was quadratic: ~100 ms a frame on a file with thousands of
    // symbols). The same-name ordinals come from one pass, and this file's
    // notes are indexed by symbol once — the first note for a name wins, as
    // with `notes::find`.
    let ordinals = crate::outline::fn_ordinals(&v.symbols);
    let mut notes: std::collections::HashMap<&str, &crate::notes::Note> =
        std::collections::HashMap::new();
    for n in app.proj.notes.iter().filter(|n| n.rel == v.rel) {
        notes.entry(n.symbol.as_str()).or_insert(n);
    }
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (symbol, &ordinal) in v.symbols.iter().zip(&ordinals) {
        let is_fn = matches!(symbol.kind.as_str(), "function" | "method");
        let is_current = is_fn && current == Some((symbol.name.as_str(), ordinal));
        // The reader's note/progress on this symbol (anchored by name, so it
        // follows the symbol across edits/re-scans).
        let note = notes.get(symbol.name.as_str()).copied();
        let understood = note.is_some_and(|n| n.understood);
        let has_text = note.is_some_and(|n| !n.text.is_empty());

        let label = row![
            text(short_kind(&symbol.kind))
                .size(ts::CAPTION)
                .color(theme::kind_color(&symbol.kind))
                .width(40),
            // Understood symbols dim, so the outline shows at a glance what's left.
            text(&symbol.name)
                .size(ts::BODY)
                .color(if understood {
                    theme::dim()
                } else {
                    theme::fg()
                })
                .wrapping(Wrapping::None),
        ]
        .spacing(4)
        .align_y(iced::Center);

        // Annotate each function/method with its one-line explanation, turning
        // the outline into a table of contents that says what each symbol does.
        // Same toggle and error-filter as the inline code summaries.
        let summary = if app.show_inline_summaries && is_fn {
            let node = crate::explain::Node::Function {
                file: v.abs.clone(),
                name: symbol.name.clone(),
                ordinal,
            };
            app.proj
                .explain
                .cache
                .get(&node)
                .filter(|c| !crate::explain::is_error_summary(&c.summary))
                .map(|c| crate::app::shown_summary(c).into_owned())
        } else {
            None
        };

        let mut col = Column::new().spacing(1).push(label);
        if let Some(full) = summary {
            // A one-line table-of-contents entry: the first sentence, truncated
            // with an ellipsis and clipped so it never wraps or overflows the
            // panel. The complete explanation shows in a bubble on hover.
            let clean = strip_backticks(&full);
            let one_line = truncate_ellipsis(&first_sentence(&clean), 52);
            let line = container(
                text(one_line)
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            )
            .clip(true)
            .width(Fill)
            .padding(Padding {
                top: 0.0,
                right: 6.0,
                bottom: 0.0,
                left: 44.0,
            });
            let bubble = container(text(clean).size(ts::SMALL).color(theme::fg()))
                .padding(Padding {
                    top: 6.0,
                    right: 9.0,
                    bottom: 6.0,
                    left: 9.0,
                })
                .max_width(TIP_MAX_W)
                .style(theme::modal_panel);
            col = col.push(tooltip(line, bubble, tooltip::Position::Bottom).gap(4));
        }
        if let Some(n) = note.filter(|n| !n.text.is_empty()) {
            // The reader's own note, in accent so it's distinct from the summary.
            col = col.push(
                container(
                    text(format!("\u{270e} {}", n.text))
                        .size(ts::CAPTION)
                        .color(theme::accent())
                        .wrapping(Wrapping::Word),
                )
                .padding(Padding {
                    top: 0.0,
                    right: 4.0,
                    bottom: 0.0,
                    left: 44.0,
                }),
            );
        }

        let jump = button(col)
            .style(theme::list_row(is_current))
            .width(Fill)
            .padding(Padding {
                top: 4.0,
                right: 4.0,
                bottom: 4.0,
                left: 4.0,
            })
            .on_press(Message::Editor(EditorMsg::OutlineJump(symbol.line)));
        // Leading "understood" toggle and trailing note pencil sit outside the
        // jump button so each captures its own click.
        let (cg, gcolor) = if understood {
            (Glyph::CheckCircle, theme::accent())
        } else {
            (Glyph::Circle, theme::dim())
        };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([5, 5])
            .on_press(Message::Reading(ReadingMsg::NoteToggleUnderstood {
                rel: v.rel.clone(),
                symbol: symbol.name.clone(),
            }));
        let pencil = button(glyph::icon(
            Glyph::Edit,
            if has_text {
                theme::accent()
            } else {
                theme::dim()
            },
            12.0,
        ))
        .style(theme::list_row(false))
        .padding([5, 5])
        .on_press(Message::Reading(ReadingMsg::NoteEditStart {
            rel: v.rel.clone(),
            symbol: symbol.name.clone(),
        }));
        // Top-align so the toggle circle and pencil sit on the kind-badge/name
        // line rather than floating in the middle of the multi-line row.
        rows.push(
            row![toggle, jump, pencil]
                .spacing(1)
                .align_y(iced::alignment::Vertical::Top)
                .into(),
        );
    }

    // Sub-label under "OUTLINE": the reader's manual "understood" coverage for
    // this file. Plain text, left-aligned with the section header — no decorative
    // leading circle (that read as a stray, non-clickable control). Explain-All
    // progress lives only in the status bar, so it isn't duplicated here.
    let names: Vec<String> = v.symbols.iter().map(|s| s.name.clone()).collect();
    let (done, total) = crate::notes::coverage(&app.proj.notes, &v.rel, &names);
    let header_content: Element<'_, Message> = text(format!("{done}/{total} understood"))
        .size(ts::SMALL)
        .color(theme::fg_muted())
        .into();
    let header = container(header_content).padding(Padding {
        top: 2.0,
        right: 10.0,
        bottom: 4.0,
        left: 10.0,
    });

    // The wrapping column must be Fill so the scrollable has a bounded height to
    // scroll within — otherwise it grows to its content and never scrolls (which
    // made long outlines like main.rs's 224 symbols un-navigable).
    column![
        header,
        scrollable(Column::with_children(rows).width(Fill))
            .id(outline_scroll_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .height(Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E2-10: a diff line's width counts what is drawn — tabs at the reader's
    /// tab width, CJK and emoji as two columns, combining marks as none.
    #[test]
    fn display_columns_follow_the_drawn_text() {
        assert_eq!(display_cols("abc"), 3);
        assert_eq!(display_cols("\tx"), TAB_COLS + 1);
        assert_eq!(display_cols("数据"), 4);
        assert_eq!(display_cols("😀"), 2);
        assert_eq!(display_cols("e\u{301}"), 1);
        assert_eq!(display_cols(""), 0);
    }

    /// E2-5: a diff is prepared for the view once, when it arrives: its
    /// runs of one kind joined (tabs expanded, blank lines kept a space
    /// wide), its widest drawn line counted — the view used to redo both for
    /// up to 8000 lines on every repaint — and at most `MAX_DIFF_ROWS` lines.
    #[test]
    fn diff_rows_are_as_wide_as_the_widest_drawn_line() {
        use crate::git::{DiffKind, DiffLine};
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
        };
        let d = crate::DiffState::new(
            std::path::PathBuf::from("/p/a.rs"),
            "a.rs".into(),
            vec![
                line(DiffKind::Context, "ab"),
                line(DiffKind::Context, ""),
                line(DiffKind::Add, "数据库"),
                line(DiffKind::Add, "\tx"),
                line(DiffKind::Remove, "gone"),
            ],
        );
        // Widest: 3 CJK = 6 columns.
        assert_eq!(d.max_cols, 6);
        let runs: Vec<(DiffKind, &str)> =
            d.runs.iter().map(|r| (r.kind, r.text.as_str())).collect();
        assert_eq!(
            runs,
            [
                (DiffKind::Context, "ab\n "),
                (DiffKind::Add, "数据库\n    x"),
                (DiffKind::Remove, "gone"),
            ]
        );
        // + 1 of slack, at 7 px, + 18 px padding.
        assert_eq!(row_width(d.max_cols, 7.0, 18.0), 7.0 * 7.0 + 18.0);
        assert_eq!(row_width(0, 7.0, 18.0), 7.0 + 18.0);
        let long = crate::DiffState::new(
            std::path::PathBuf::from("/p/a.rs"),
            "a.rs".into(),
            (0..MAX_DIFF_ROWS + 5)
                .map(|_| line(DiffKind::Add, "+"))
                .collect(),
        );
        assert_eq!(long.runs[0].text.lines().count(), MAX_DIFF_ROWS);
    }

    #[test]
    fn diff_text_expands_tabs_and_keeps_blank_lines_tall() {
        assert_eq!(diff_display_text(""), " ");
        assert_eq!(
            diff_display_text("\tx"),
            format!("{}x", " ".repeat(TAB_COLS))
        );
        assert_eq!(diff_display_text("+ok"), "+ok");
    }

    /// The advance is measured by the text engine, not assumed at 0.6 em —
    /// the engine answers, and what the diff uses is that answer (a range
    /// check alone admitted the 0.6 em fallback) — and repeated asks for one
    /// size agree.
    #[test]
    fn the_monospace_advance_is_measured() {
        let measured =
            measured_mono_advance(13.0).expect("the text engine measures the monospace face");
        assert_eq!(mono_advance(13.0), measured);
        assert!(
            measured > 13.0 * 0.3 && measured < 13.0,
            "implausible advance {measured}"
        );
        assert_eq!(mono_advance(13.0), measured);
        assert!(mono_advance(26.0) > measured);
    }
}
