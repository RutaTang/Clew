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

pub(crate) fn pane_view(app: &App, pane: usize) -> Element<'_, Message> {
    // Time travel takes over the active pane entirely (its own read-only view).
    if pane == app.active
        && let Some(tt) = &app.time_travel
        && app.panes[pane].as_ref().is_some_and(|v| v.abs == tt.abs)
    {
        // Pass the live viewer as a fallback so the code stays visible while the
        // first historical revision loads (no blank "flash" on entry).
        return editor_shell(time_travel_view(app, tt, app.panes[pane].as_ref()));
    }
    let inner: Element<'_, Message> = match &app.panes[pane] {
        Some(v) => {
            // The diff view replaces the code of the active pane's file.
            if pane == app.active
                && let Some(d) = &app.diff
                && d.abs == v.abs
            {
                diff_view(app, d)
            } else if let Some(md) = v.md.as_ref().filter(|_| !v.show_source) {
                // A markdown file renders as a document; a toggle in the
                // breadcrumb switches to the raw source.
                markdown_pane(pane, md)
            } else {
                code_pane(app, pane, v)
            }
        }
        None => mouse_area(empty_state(
            Glyph::Note,
            "No file open",
            "Pick a file from the tree, or press ⌘P.",
            None,
        ))
        .on_press(Message::PaneFocused(pane))
        .into(),
    };

    // Find bar floats over the top-right of the active pane.
    let body: Element<'_, Message> = if app.find.open && pane == app.active {
        stack![editor_shell(inner), find_bar(app)].into()
    } else {
        editor_shell(inner)
    };

    let mut col = column![];
    if app.split {
        col = col.push(pane_header(app, pane));
    }
    col.push(body).width(Fill).height(Fill).into()
}

// ------------------------------------------------------------- time travel

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
        None => center(text("Loading revision…").size(13).color(theme::DIM)).into(),
    };
    let mut col = Column::new().push(time_travel_banner(tt, commit));
    if let Some(story) = &tt.story {
        col = col.push(time_travel_story(app, tt, story));
    }
    col.push(container(code).width(Fill).height(Fill))
        .push(time_travel_bar(tt))
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
    let keycap = container(text("esc").size(9).color(theme::FG_MUTED))
        .padding(Padding {
            top: 1.0,
            right: 5.0,
            bottom: 1.0,
            left: 5.0,
        })
        .style(|_: &iced::Theme| iced::widget::container::Style {
            background: Some(theme::BG_ACTIVE.into()),
            border: iced::Border {
                radius: 3.0.into(),
                width: 1.0,
                color: theme::HAIRLINE,
            },
            ..Default::default()
        });
    let exit = button(
        row![text("Exit").size(11).color(theme::FG_MUTED), keycap]
            .spacing(6)
            .align_y(iced::Center),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravelExit);

    let Some(c) = commit else {
        let head = row![
            glyph::icon(Glyph::TimeTravel, theme::ACCENT, 15.0),
            text("Time travel").size(12).color(theme::FG),
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
        glyph::icon(Glyph::TimeTravel, theme::ACCENT, 15.0),
        text(short)
            .size(12)
            .color(theme::ACCENT)
            .font(Font::MONOSPACE),
        text(format!(
            "{}  ·  {}",
            c.author,
            crate::git::relative_time(c.time, now)
        ))
        .size(11)
        .color(theme::DIM),
        space().width(Fill),
        exit,
    ]
    .spacing(10)
    .align_y(iced::Center);

    let subject = text(c.subject.clone())
        .size(12)
        .color(theme::FG)
        .wrapping(Wrapping::Word);

    let why: Element<'a, Message> = if tt.why_loading {
        text("Summarizing…").size(11).color(theme::DIM).into()
    } else if let Some(w) = tt.why.get(&c.sha) {
        text(w.clone())
            .size(11)
            .color(theme::FG_MUTED)
            .wrapping(Wrapping::Word)
            .into()
    } else {
        button(text("What & why?").size(11).color(theme::ACCENT))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(Message::TimeTravelWhy)
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
        theme::FG,
        |(line, col)| Message::TimeTravelSelectStart { line, col },
        |(line, col)| Message::TimeTravelSelectDrag { line, col },
        |_, _| Message::Noop,
    )
    .cursor(hv.caret)
    .selection(hv.selection_ordered())
    .sticky(sticky)
    .folds(hv.visible_rows(), &hv.fold_header_set, &hv.collapsed)
    .indent_guides(true)
    .git_gutter(hv.git.as_deref());
    // Reuse the live pane's scroll id so iced keeps the scroll position across
    // the enter/exit swap (the historical file is ~the same length), instead of
    // remounting a fresh scrollable at the top.
    scrollable(code)
        .id(code_scroll_id(app.active))
        .on_scroll(Message::TimeTravelScrolled)
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
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
    let older = (tt.idx < last).then(|| Message::TimeTravelGoto(tt.idx + 1));
    let newer = (tt.idx > 0).then(|| Message::TimeTravelGoto(tt.idx - 1));
    let step = |g: Glyph, msg: Option<Message>| {
        let on = msg.is_some();
        let mut b = button(glyph::icon(
            g,
            if on { theme::FG } else { theme::DIM },
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
        Message::TimeTravelGoto(last - p)
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
            .size(11)
            .color(theme::FG_MUTED),
    )
    .style(theme::toolbar_button)
    .padding([2, 8])
    .on_press(Message::TimeTravelToggleScope);

    let story: Element<'_, Message> = if matches!(tt.scope, TimeScope::Symbol { .. }) {
        if tt.story_loading {
            text("Story…").size(11).color(theme::DIM).into()
        } else {
            let label = if tt.story.is_some() {
                "Hide story"
            } else {
                "Story"
            };
            let color = if tt.story.is_some() {
                theme::FG_MUTED
            } else {
                theme::ACCENT
            };
            button(text(label).size(11).color(color))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::TimeTravelStory)
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
                .size(11)
                .color(theme::DIM),
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
                .size(12)
                .color(theme::ACCENT),
            // `git log -L` only follows the block's CURRENT lines, so earlier
            // rewrites may not be attributed — say so, so it's not read as a full
            // biography.
            text("from the commits that touched these lines")
                .size(9)
                .color(theme::DIM),
        ]
        .spacing(1),
        space().width(Fill),
        button(text("✕").size(11).color(theme::DIM))
            .style(theme::toolbar_button)
            .padding([1, 6])
            .on_press(Message::TimeTravelStory),
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

/// The unified diff of the active file versus `HEAD`, colored by line kind.
pub(crate) fn diff_view<'a>(app: &'a App, d: &'a crate::DiffState) -> Element<'a, Message> {
    use crate::git::DiffKind;

    let header = container(
        row![
            text(format!("{}  ·  vs HEAD", d.rel))
                .size(12)
                .color(theme::ACCENT),
            space().width(Fill),
            button(text("✕ close").size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::ToggleDiff),
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
                    .size(13)
                    .color(theme::DIM),
            )
        ]
        .width(Fill)
        .height(Fill)
        .into();
    }

    const MAX_DIFF_ROWS: usize = 8000;
    // Size every row to the longest line so the color tints span the full
    // content width and long lines become reachable via horizontal scroll.
    let max_cols = d
        .lines
        .iter()
        .take(MAX_DIFF_ROWS)
        .map(|l| l.text.chars().count())
        .max()
        .unwrap_or(0);
    let row_width = (max_cols as f32 + 1.0) * app.font_size * 0.6 + 18.0;
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    for dl in d.lines.iter().take(MAX_DIFF_ROWS) {
        let (bg, fg) = match dl.kind {
            DiffKind::Add => (
                Some(theme::with_alpha(theme::rgb(0x98c379), 0.14)),
                theme::rgb(0x98c379),
            ),
            DiffKind::Remove => (
                Some(theme::with_alpha(theme::rgb(0xe06c75), 0.14)),
                theme::rgb(0xe06c75),
            ),
            DiffKind::Hunk => (Some(theme::with_alpha(theme::ACCENT, 0.12)), theme::ACCENT),
            DiffKind::Header => (None, theme::DIM),
            DiffKind::Context => (None, theme::FG),
        };
        // A space keeps empty lines from collapsing to zero height.
        let content = if dl.text.is_empty() {
            " "
        } else {
            dl.text.as_str()
        };
        let mut cell = container(
            text(content)
                .font(Font::MONOSPACE)
                .size(app.font_size)
                .color(fg)
                .wrapping(Wrapping::None),
        )
        .width(Length::Fixed(row_width))
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 0.0,
            left: 10.0,
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
                .size(11)
                .color(theme::DIM)
                .into(),
        );
    }

    let body = scrollable(Column::with_children(rows).padding([4, 0]))
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill);

    column![header, body].width(Fill).height(Fill).into()
}

pub(crate) fn find_bar(app: &App) -> Element<'_, Message> {
    let count = if app.find.query.is_empty() {
        String::new()
    } else if app.find.matches.is_empty() {
        "0/0".to_string()
    } else {
        format!("{}/{}", app.find.current + 1, app.find.matches.len())
    };

    let input = text_input("Find in file…", &app.find.query)
        .id(find_input_id())
        .on_input(Message::FindQueryChanged)
        .on_submit(Message::FindStep(1))
        .size(13)
        .padding([4, 8])
        .width(190);

    let btn = |label: &'static str, msg: Message| {
        button(text(label).size(13))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };

    let bar = container(
        row![
            input,
            text(count).size(11).color(theme::DIM).width(46),
            btn("‹", Message::FindStep(-1)),
            btn("›", Message::FindStep(1)),
            btn("✕", Message::FindClosed),
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
    let active = app.active == pane;
    let title = app.panes[pane]
        .as_ref()
        .map(|v| v.rel.as_str())
        .unwrap_or("—");
    mouse_area(
        container(
            text(title)
                .size(11)
                .color(if active { theme::ACCENT } else { theme::DIM })
                .wrapping(Wrapping::None),
        )
        .width(Fill)
        .padding([3, 8])
        .style(theme::pane_header),
    )
    .on_press(Message::PaneFocused(pane))
    .into()
}

pub(crate) fn welcome(app: &App) -> Element<'_, Message> {
    // On a live remote the hero invites browsing that host; otherwise it offers
    // opening local code or connecting out over SSH.
    let (subtitle, primary, secondary): (String, (&str, Message), (&str, Message)) =
        if app.connection.is_remote() {
            (
                format!("connected to {}", app.connection.label()),
                ("Browse folders…", Message::OpenConnect),
                ("Disconnect", Message::ConnectDisconnect),
            )
        } else {
            (
                // "clew" = the thread that guides you out of the labyrinth.
                "Find the thread through your codebase".to_string(),
                ("Open Folder…", Message::OpenFolderPressed),
                ("Open Remote…", Message::OpenConnect),
            )
        };

    let actions = row![
        button(text(primary.0.to_string()).size(14))
            .style(theme::primary_button)
            .padding([8, 20])
            .on_press(primary.1),
        button(text(secondary.0.to_string()).size(14))
            .style(theme::secondary_button)
            .padding([8, 20])
            .on_press(secondary.1),
    ]
    .spacing(10);

    // Brand lockup: the "C" mark on the left, the name on the right — same mark
    // as the app icon (minus the square), so the welcome reads as part of the app.
    let mark = iced::widget::svg(iced::widget::svg::Handle::from_memory(MARK_SVG))
        .width(Length::Fixed(52.0))
        .height(Length::Fixed(52.0));
    let brand = row![mark, text("Clew").size(34).color(theme::FG_BRIGHT)]
        .spacing(14)
        .align_y(iced::Center);

    center(
        column![
            brand,
            space().height(4),
            text(subtitle).size(13).color(theme::FG_MUTED),
            space().height(22),
            actions,
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

/// The Clew "C" mark (white arcs, transparent background) for the welcome screen.
const MARK_SVG: &[u8] = include_bytes!("../../assets/icon/mark.svg");

/// Render a markdown file as a document (readmes, changelogs) instead of raw
/// source. Links open via the normal `OpenLink` path; the `PaneFocused` mouse
/// area keeps click-to-focus working like the code view.
pub(crate) fn markdown_pane<'a>(
    pane: usize,
    items: &'a [iced::widget::markdown::Item],
) -> Element<'a, Message> {
    let doc = iced::widget::markdown::view(items, iced::Theme::Dark)
        .map(|url| Message::OpenLink(url.to_string()));
    let body = container(doc).padding([16, 28]).max_width(920);
    mouse_area(
        scrollable(body)
            .width(Fill)
            .height(Fill)
            .style(theme::overlay_scrollbar),
    )
    .on_press(Message::PaneFocused(pane))
    .into()
}

pub(crate) fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // Inline summaries: each function's one-line explanation, shown past its
    // signature so you read the code together with what it does.
    let summaries: std::collections::HashMap<usize, String> = if app.show_inline_summaries {
        app.symbol_index_by_file
            .get(&v.abs)
            .map(|syms| {
                syms.iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .filter_map(|s| {
                        let node = crate::explain::Node::Function {
                            file: v.abs.clone(),
                            name: s.name.clone(),
                        };
                        app.explain
                            .cache
                            .get(&node)
                            .filter(|c| !crate::explain::is_error_summary(&c.summary))
                            .map(|c| (s.line, first_sentence(&c.summary)))
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };

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
    let debug_current = app
        .debug
        .session
        .as_ref()
        .and_then(|d| d.current.as_ref())
        .filter(|(p, _)| *p == v.abs)
        .map(|(_, line)| *line);

    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let mut code = CodeView::new(
        &v.lines,
        v.max_cols,
        app.font_size,
        app.line_height(),
        theme::FG,
        move |(line, col)| Message::SelectStart { pane, line, col },
        move |(line, col)| Message::SelectDrag { pane, line, col },
        move |(line, col), at| Message::ContextMenuOpened {
            pane,
            line,
            col,
            x: at.x,
            y: at.y,
        },
    )
    .selection(v.selection_ordered())
    .cursor(cursor)
    .highlights(app.code_highlights(pane, v))
    .sticky(app.sticky_headers(v))
    .bookmarks(marked)
    .breakpoints(breakpoints)
    .cond_breakpoints(cond_breakpoints)
    .debug_current(debug_current)
    .summaries(summaries)
    .inlay_hints(v.inlay_hints.clone(), theme::DIM)
    .inactive(v.inactive_lines.clone())
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::FoldToggle { pane, line })
    .on_breakpoint(move |line| Message::BreakpointToggle {
        path: v.abs.clone(),
        line,
    })
    .indent_guides(true)
    .git_gutter(v.git.as_deref())
    .blame(if pane == app.active && app.code_focused {
        app.blame_annotation(v)
    } else {
        None
    })
    .on_hover(move |(line, col), at| Message::HoverRequested {
        pane,
        line,
        col,
        x: at.x,
        y: at.y,
    })
    .on_hover_end(|| Message::HoverCleared);
    // The minimap is opt-in (toggle in the "More" menu); without the callback
    // the widget draws no minimap band at all.
    if app.show_minimap {
        code = code.on_minimap(move |fraction| Message::MinimapScrolled { pane, fraction });
    }

    let scroller = scrollable(code)
        .id(code_scroll_id(pane))
        .on_scroll(move |viewport| Message::CodeScrolled(pane, viewport))
        .direction(Direction::Both {
            vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
            horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
        })
        .style(theme::overlay_scrollbar)
        .width(Fill)
        .height(Fill);

    // File TL;DR banner: a one-line "what is this file" from the explain cache,
    // pinned above the code. Dismissable (toggle back via the More menu).
    let banner: Option<Element<'_, Message>> = if app.show_file_banner {
        app.explain
            .cache
            .get(&crate::explain::Node::File(v.abs.clone()))
            .filter(|c| !crate::explain::is_error_summary(&c.summary))
            .map(|c| file_banner(first_sentence(&c.summary)))
    } else {
        None
    };
    match banner {
        Some(b) => column![b, scroller].into(),
        None => scroller.into(),
    }
}

/// A one-line file summary pinned at the top of the code view.
pub(crate) fn file_banner<'a>(summary: String) -> Element<'a, Message> {
    container(
        row![
            text("›").size(12).color(theme::ACCENT),
            text(summary).size(12).color(theme::FG_MUTED).width(Fill),
            button(text("✕").size(11).color(theme::DIM))
                .style(theme::toolbar_button)
                .padding([0, 6])
                .on_press(Message::ToggleFileBanner),
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

/// The right sidebar: a tabbed panel with an Outline tab and an Explain tab
/// (mirrors the left sidebar's tabs). Hidden on narrow/split windows or with no
/// file open.
pub(crate) fn right_panel(app: &App) -> Option<Element<'_, Message>> {
    if !app.show_right_panel || app.split || app.window_width < 950.0 {
        return None;
    }
    app.active_viewer()?; // a file must be open

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
            background: Some(theme::HAIRLINE.into()),
            ..Default::default()
        })
        .into()
}

/// The Outline tab's content: the active file's symbols, click to jump.
pub(crate) fn outline_content(app: &App) -> Element<'_, Message> {
    let Some(v) = app.active_viewer() else {
        return space().into();
    };
    if v.symbols.is_empty() {
        return container(text("No symbols in this file.").size(11).color(theme::DIM))
            .padding(10)
            .into();
    }
    // The symbol the reading cursor is currently inside, to highlight its row.
    let current = match &app.explain.view {
        Some(crate::explain::Node::Function { file, name }) if *file == v.abs => {
            Some(name.as_str())
        }
        _ => None,
    };
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for symbol in &v.symbols {
        let is_current = matches!(symbol.kind.as_str(), "function" | "method")
            && current == Some(symbol.name.as_str());
        // The reader's note/progress on this symbol (anchored by name, so it
        // follows the symbol across edits/re-scans).
        let note = crate::notes::find(&app.notes, &v.rel, &symbol.name);
        let understood = note.is_some_and(|n| n.understood);
        let has_text = note.is_some_and(|n| !n.text.is_empty());

        let label = row![
            text(short_kind(&symbol.kind))
                .size(10)
                .color(kind_color(&symbol.kind))
                .width(40),
            // Understood symbols dim, so the outline shows at a glance what's left.
            text(&symbol.name)
                .size(12)
                .color(if understood { theme::DIM } else { theme::FG })
                .wrapping(Wrapping::None),
        ]
        .spacing(4)
        .align_y(iced::Center);

        // Annotate each function/method with its one-line explanation, turning
        // the outline into a table of contents that says what each symbol does.
        // Same toggle and error-filter as the inline code summaries.
        let summary =
            if app.show_inline_summaries && matches!(symbol.kind.as_str(), "function" | "method") {
                let node = crate::explain::Node::Function {
                    file: v.abs.clone(),
                    name: symbol.name.clone(),
                };
                app.explain
                    .cache
                    .get(&node)
                    .filter(|c| !crate::explain::is_error_summary(&c.summary))
                    .map(|c| c.summary.trim().to_string())
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
                    .size(10)
                    .color(theme::DIM)
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
            let bubble = container(text(clean).size(11).color(theme::FG))
                .padding(Padding {
                    top: 6.0,
                    right: 9.0,
                    bottom: 6.0,
                    left: 9.0,
                })
                .max_width(320)
                .style(theme::modal_panel);
            col = col.push(tooltip(line, bubble, tooltip::Position::Bottom).gap(4));
        }
        if let Some(n) = note.filter(|n| !n.text.is_empty()) {
            // The reader's own note, in accent so it's distinct from the summary.
            col = col.push(
                container(
                    text(format!("\u{270e} {}", n.text))
                        .size(10)
                        .color(theme::ACCENT)
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
            .on_press(Message::OutlineJump(symbol.line));
        // Leading "understood" toggle and trailing note pencil sit outside the
        // jump button so each captures its own click.
        let (cg, gcolor) = if understood {
            (Glyph::CheckCircle, theme::ACCENT)
        } else {
            (Glyph::Circle, theme::DIM)
        };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([5, 5])
            .on_press(Message::NoteToggleUnderstood {
                rel: v.rel.clone(),
                symbol: symbol.name.clone(),
            });
        let pencil = button(glyph::icon(
            Glyph::Edit,
            if has_text { theme::ACCENT } else { theme::DIM },
            12.0,
        ))
        .style(theme::list_row(false))
        .padding([5, 5])
        .on_press(Message::NoteEditStart {
            rel: v.rel.clone(),
            symbol: symbol.name.clone(),
        });
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
    let (done, total) = crate::notes::coverage(&app.notes, &v.rel, &names);
    let header_content: Element<'_, Message> = text(format!("{done}/{total} understood"))
        .size(11)
        .color(theme::FG_MUTED)
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
            .direction(Direction::Vertical(
                Scrollbar::new().width(6.0).scroller_width(6.0)
            ))
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .height(Fill)
    .into()
}
