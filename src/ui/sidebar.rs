//! Left sidebar and its tabs (files/search/trail/marks/notes/semantic/walk).

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

pub(crate) fn sidebar(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: SidebarTab| {
        button(text(label).size(11))
            .style(theme::tab_button(app.sidebar == this))
            .padding([5, 7])
            .on_press(Message::SidebarTabPicked(this))
    };
    // Seven tabs rarely all fit a narrow sidebar, so they keep their natural
    // width and scroll horizontally (a thin bar appears only when they overflow)
    // — widening the sidebar reveals them all. CALLS/IMPORTS are always present.
    let tabs = scrollable(
        row![
            tab("FILES", SidebarTab::Files),
            tab("SEARCH", SidebarTab::Search),
            tab("FIND", SidebarTab::Semantic),
            tab("MARKS", SidebarTab::Marks),
            tab("TRAIL", SidebarTab::Trail),
            tab("CALLS", SidebarTab::Calls),
            tab("IMPORTS", SidebarTab::Imports),
            tab("WALK", SidebarTab::Walk),
            tab("NOTES", SidebarTab::Notes),
            tab("DOCS", SidebarTab::Docs),
        ]
        .spacing(1),
    )
    // Scrollable but with no visible bar — it scrolls by trackpad/wheel and the
    // sidebar can be widened to reveal all tabs; a bar here just looks noisy.
    .direction(Direction::Horizontal(
        Scrollbar::new().width(0.0).scroller_width(0.0),
    ))
    .width(Fill);

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
        SidebarTab::Semantic => semantic_tab(app),
        SidebarTab::Marks => marks_tab(app),
        SidebarTab::Trail => trail_tab(app),
        SidebarTab::Calls => calls_tab(app),
        SidebarTab::Imports => imports_tab(app),
        SidebarTab::Walk => walk_tab(app),
        SidebarTab::Notes => notes_tab(app),
        SidebarTab::Docs => docs_tab(app),
    };

    container(column![tabs, content])
        .width(Length::Fixed(app.sidebar_width))
        .height(Fill)
        .style(theme::panel)
        .into()
}

/// The guided-walkthrough tab. The top bar toggles between *searching* the saved
/// library of tours and *walking* (generating) a new one. Opening a tour steps
/// through it, each step driving the editor to its anchor; regenerating a tour
/// lives next to its title.
pub(crate) fn walk_tab(app: &App) -> Element<'_, Message> {
    let header = walk_header(app);
    // A quick action to review the current branch/PR changes as a narrated tour.
    let review = container(
        button(text("\u{2387} Review branch changes").size(11))
            .style(theme::toolbar_button)
            .padding([4, 10])
            .width(Fill)
            .on_press(Message::GenerateDiffWalkthrough),
    )
    .padding(Padding { top: 0.0, right: 8.0, bottom: 6.0, left: 8.0 });

    // The library list is always shown under the input; selecting a tour expands
    // its steps inline (accordion) and its narration into the bottom pane — no
    // separate "back to library" navigation. Generation is shown per-row, so the
    // rest of the library stays usable while a tour is being built.
    let list = walk_library(app);
    let open = app.walkthrough_open.and_then(|o| app.walkthroughs.get(o).map(|w| (o, w)));

    let Some((idx, wt)) = open else {
        return column![header, review, hairline(), list].height(Fill).into();
    };

    let narration_block = walk_narration(app, idx, wt);
    column![
        header,
        review,
        hairline(),
        list,
        crate::resize::Divider::horizontal(Message::ResizeWalkNarration),
        narration_block,
    ]
    .height(Fill)
    .into()
}

/// A human label for a walkthrough's scope: the whole codebase, a change review,
/// or the user's feature prompt.
pub(crate) fn scope_label(scope: &str) -> String {
    if scope.is_empty() {
        "Whole codebase".to_string()
    } else if let Some(rest) = scope.strip_prefix("@diff") {
        format!("Change review{}", if rest.trim().is_empty() { String::new() } else { format!(" ({})", rest.trim()) })
    } else {
        scope.to_string()
    }
}

/// The top bar: a Search/Walk segmented toggle, the shared input, and (in Walk
/// mode) a Generate button.
pub(crate) fn walk_header(app: &App) -> Element<'_, Message> {
    let is_search = app.walkthrough_mode == crate::WalkMode::Search;
    // Two-segment control; only the inactive segment is pressable (it flips mode).
    let seg = |label: &str, active: bool| {
        let mut b =
            button(text(label.to_string()).size(11)).style(theme::tab_button(active)).padding([3, 10]);
        if !active {
            b = b.on_press(Message::WalkthroughToggleMode);
        }
        b
    };
    let toggle = row![seg("Search", is_search), seg("Walk", !is_search)].spacing(2);

    let placeholder = if is_search {
        "Search walkthroughs…"
    } else {
        "Walk a feature, or leave empty for the whole codebase…"
    };
    let mut input = text_input(placeholder, &app.walkthrough_input)
        .on_input(Message::WalkthroughInputChanged)
        .size(12)
        .padding(6);
    if !is_search {
        // Enter submits the same way the Generate button does ("" = whole codebase).
        input = input.on_submit(Message::GenerateWalkthrough(app.walkthrough_input.clone()));
    }
    let mut bar = row![toggle, input].spacing(6).align_y(iced::Center);
    if !is_search {
        bar = bar.push(
            button(text("Generate").size(11))
                .style(theme::toolbar_button)
                .padding([4, 12])
                .on_press(Message::GenerateWalkthrough(app.walkthrough_input.clone())),
        );
    }
    container(bar).padding(8).into()
}

/// The library list: every saved tour, filtered by the search query, each with a
/// per-tour Regenerate button on the right.
pub(crate) fn walk_library(app: &App) -> Element<'_, Message> {
    let query = if app.walkthrough_mode == crate::WalkMode::Search {
        app.walkthrough_input.trim().to_lowercase()
    } else {
        String::new()
    };
    let matches = |wt: &crate::walkthrough::Walkthrough| {
        query.is_empty()
            || wt.title.to_lowercase().contains(&query)
            || wt.scope.to_lowercase().contains(&query)
    };

    let visible: Vec<(usize, &crate::walkthrough::Walkthrough)> =
        app.walkthroughs.iter().enumerate().filter(|(_, wt)| matches(wt)).collect();

    // The scope currently generating, and — if it's a brand-new scope not yet in
    // the library — the label for a temporary "pending" row at the top.
    let gen_scope = app.generating_walkthrough.as_deref();
    let pending_new: Option<&str> =
        gen_scope.filter(|s| !app.walkthroughs.iter().any(|w| w.scope.as_str() == *s));

    if app.walkthroughs.is_empty() && pending_new.is_none() {
        return empty_state(
            Glyph::Compass,
            "No walkthroughs yet",
            "Switch to Walk mode and generate a guided tour of the codebase or a feature.",
            None,
        );
    }
    if visible.is_empty() && pending_new.is_none() {
        return empty_state(Glyph::Search, "No matches", "No saved walkthrough matches your search.", None);
    }

    // The current step of the open tour (for highlighting the expanded steps).
    let cur = app
        .walkthrough_open
        .and_then(|o| app.walkthroughs.get(o))
        .map(|w| app.walkthrough_step.min(w.steps.len().saturating_sub(1)));

    let mut list = Column::new().spacing(2).padding(8);

    // A new tour being generated shows a pending row until it lands in the library.
    if let Some(scope) = pending_new {
        let label = scope_label(scope);
        list = list.push(
            container(
                column![
                    text(label).size(13).color(theme::FG),
                    text("Generating…").size(10).color(theme::ACCENT),
                ]
                .spacing(1),
            )
            .width(Fill)
            .padding([6, 8]),
        );
    }

    for (i, wt) in visible {
        let is_open = app.walkthrough_open == Some(i);
        let busy = gen_scope == Some(wt.scope.as_str());
        let (subtitle, sub_color) = if busy {
            ("Generating…".to_string(), theme::ACCENT)
        } else {
            (scope_label(&wt.scope), theme::DIM)
        };
        // The tour row: a full-width clickable title (so its selected highlight
        // spans the whole row) with the regenerate/delete controls layered on top
        // at the right via a stack. Leave right padding for them so the title
        // text never runs under the controls.
        let title = button(
            column![
                text(wt.title.clone()).size(13).color(if is_open { theme::FG_BRIGHT } else { theme::FG }),
                text(subtitle).size(10).color(sub_color),
            ]
            .spacing(1),
        )
        .style(theme::list_row(is_open))
        .width(Fill)
        .padding(Padding { top: 6.0, right: if busy { 8.0 } else { 62.0 }, bottom: 6.0, left: 8.0 })
        .on_press(if is_open { Message::WalkthroughBack } else { Message::WalkthroughOpen(i) });
        let tour_row: Element<'_, Message> = if busy {
            title.into()
        } else {
            let controls = container(
                row![
                    button(text("↻").size(13))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(Message::WalkthroughRegenerate(i)),
                    button(text("✕").size(12))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(Message::WalkthroughDelete(i)),
                ]
                .spacing(2),
            )
            .width(Fill)
            .height(Fill)
            .align_x(iced::Right)
            .align_y(iced::Center)
            .padding(Padding { top: 0.0, right: 6.0, bottom: 0.0, left: 0.0 });
            stack![title, controls].into()
        };
        list = list.push(tour_row);

        // Expanded: the tour's steps, indented, current one highlighted.
        if is_open {
            for (si, step) in wt.steps.iter().enumerate() {
                let is_cur = cur == Some(si);
                list = list.push(
                    button(
                        row![
                            text(format!("{}", si + 1)).size(10).color(theme::DIM).width(18),
                            text(step.title.clone())
                                .size(12)
                                .color(if is_cur { theme::FG } else { theme::DIM }),
                        ]
                        .spacing(6)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_cur))
                    .width(Fill)
                    .padding(Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 22.0 })
                    .on_press(Message::WalkthroughGoto(si)),
                );
            }
        }
    }

    scrollable(list.width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill).into()
}

/// The bottom pane for the open tour: a compact nav row (file + step counter +
/// prev/next) over the current step's rendered narration.
pub(crate) fn walk_narration<'a>(
    app: &'a App,
    _idx: usize,
    wt: &'a crate::walkthrough::Walkthrough,
) -> Element<'a, Message> {
    let n = wt.steps.len();
    let cur = app.walkthrough_step.min(n.saturating_sub(1));
    let Some(step) = wt.steps.get(cur) else {
        return space().into();
    };

    let nav = row![
        text(step.file.clone()).size(10).color(theme::ACCENT),
        space().width(Fill),
        button(text("‹").size(14))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::WalkthroughStep(-1)),
        text(format!("{}/{}", cur + 1, n)).size(11).color(theme::DIM),
        button(text("›").size(14))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::WalkthroughStep(1)),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .padding([4, 8]);

    let body: Element<'_, Message> = if app.walkthrough_prepared.is_empty() {
        text(step.narration.clone()).size(12).color(theme::FG).width(Fill).into()
    } else {
        Column::with_children(render_prepared(app, &app.walkthrough_prepared)).spacing(8).width(Fill).into()
    };
    let narration = scrollable(container(body).padding(Padding {
        top: 0.0,
        right: 8.0,
        bottom: 8.0,
        left: 8.0,
    }))
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);

    container(column![nav, narration]).height(Length::Fixed(app.walkthrough_narration_height)).into()
}

pub(crate) fn files_tab(app: &App) -> Element<'_, Message> {
    let Some(project) = &app.project else {
        // Same centered empty-state pattern as the other tabs (Trail, Marks, …).
        // No action button here: the open/connect actions live in the centered
        // welcome hero, so the sidebar just states what's going on.
        return if app.scanning {
            empty_state(Glyph::Search, "Scanning…", "Reading the project's files.", None)
        } else if app.connection.is_remote() {
            empty_state(
                Glyph::Remote,
                "No folder open",
                "Browse the host to open a folder.",
                None,
            )
        } else {
            empty_state(
                Glyph::Folder,
                "No folder open",
                "Open a folder to start reading.",
                None,
            )
        };
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    append_tree_rows(&mut rows, &project.tree, "", 0, app);
    scrollable(Column::with_children(rows).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill)
        .into()
}

pub(crate) fn append_tree_rows<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    node: &'a DirNode,
    prefix: &str,
    depth: u16,
    app: &'a App,
) {
    let indent = 10.0 + depth as f32 * 14.0;
    let pad = Padding {
        top: 2.0,
        right: 6.0,
        bottom: 2.0,
        left: indent,
    };

    for (name, child) in &node.dirs {
        let rel = join_rel(prefix, name);
        let expanded = app.expanded.contains(&rel);
        let arrow = if expanded { "▾" } else { "▸" };
        let (glyph, color) = crate::icons::folder_icon(expanded);
        let content = row![
            text(arrow).size(10).color(theme::DIM).width(10),
            tree_icon(glyph, color),
            text(name.as_str()).size(13).wrapping(Wrapping::None),
        ]
        .spacing(3)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(false))
                .width(Fill)
                .padding(pad)
                .on_press(Message::ToggleDir(rel.clone()))
                .into(),
        );
        if expanded {
            append_tree_rows(rows, child, &rel, depth + 1, app);
        }
    }

    for name in &node.files {
        let rel = join_rel(prefix, name);
        let is_current = app.active_viewer().is_some_and(|v| v.rel == rel);
        let (glyph, color) = crate::icons::file_icon(name);
        let content = row![
            space().width(10), // align names under the folders' arrow column
            tree_icon(glyph, color),
            text(name.as_str()).size(13).wrapping(Wrapping::None),
        ]
        .spacing(3)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(is_current))
                .width(Fill)
                .padding(pad)
                .on_press(Message::OpenRel { rel, line: None })
                .into(),
        );
    }
}

/// A file-type glyph in the embedded icon font, for inline use (breadcrumb,
/// finder rows, …).
pub(crate) fn icon_text(glyph: char, color: iced::Color, size: f32) -> iced::widget::Text<'static> {
    text(glyph.to_string()).font(crate::icons::ICON_FONT).size(size).color(color)
}

/// A fixed-width, centered file-type icon for the tree's icon column.
pub(crate) fn tree_icon(glyph: char, color: iced::Color) -> Element<'static, Message> {
    container(icon_text(glyph, color, 14.0))
        .width(18)
        .align_x(iced::alignment::Horizontal::Center)
        .into()
}

/// A centered empty / loading state: a large muted icon, a title, a subtitle,
/// and an optional action button — so every "nothing here yet" screen matches.
pub(crate) fn empty_state<'a>(
    g: Glyph,
    title: &'a str,
    subtitle: &'a str,
    action: Option<(&'a str, Message)>,
) -> Element<'a, Message> {
    let mut col = column![
        glyph::icon(g, theme::rgb(0x434b57), 42.0),
        space().height(6),
        text(title.to_string()).size(14).color(theme::FG),
        container(
            text(subtitle.to_string())
                .size(12)
                .color(theme::DIM)
                .align_x(iced::Center)
        )
        .max_width(260),
    ]
    .spacing(4)
    .align_x(iced::Center);
    if let Some((label, msg)) = action {
        col = col.push(space().height(10));
        col = col.push(
            button(text(label.to_string()).size(13))
                .style(theme::toolbar_button)
                .padding([7, 16])
                .on_press(msg),
        );
    }
    center(col).padding(20).into()
}

pub(crate) fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

pub(crate) fn search_tab(app: &App) -> Element<'_, Message> {
    use crate::SearchOpt;

    let input = text_input("Search in project…", &app.search.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchSubmitted)
        .size(13)
        .padding(7);

    // Match-option toggles: case-sensitive, whole-word, regex. Each carries a
    // border so it reads as a clickable control even when off, and fills with the
    // accent (like VS Code's search toggles) when on.
    let chip = |label: &'static str, active: bool, opt: SearchOpt| -> Element<'_, Message> {
        let style = move |_t: &iced::Theme, status: button::Status| {
            let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
            let bg = if active {
                theme::ACCENT
            } else if hovered {
                theme::BG_HOVER
            } else {
                theme::BG
            };
            button::Style {
                background: Some(bg.into()),
                text_color: if active { theme::rgb(0x1b1d23) } else { theme::FG_MUTED },
                border: iced::Border {
                    radius: 4.0.into(),
                    width: 1.0,
                    color: if active { theme::ACCENT } else { theme::HAIRLINE },
                },
                ..button::Style::default()
            }
        };
        button(text(label).size(12).font(Font::MONOSPACE))
            .style(style)
            .padding([2, 7])
            .on_press(Message::SearchToggle(opt))
            .into()
    };
    let options = row![
        chip("Aa", app.search.case_sensitive, SearchOpt::Case),
        chip("W", app.search.whole_word, SearchOpt::WholeWord),
        chip(".*", app.search.regex, SearchOpt::Regex),
    ]
    .spacing(4);

    // Include / exclude glob filters.
    let include = text_input("files to include (e.g. src/**)", &app.search.include)
        .on_input(Message::SearchIncludeChanged)
        .on_submit(Message::SearchSubmitted)
        .size(12)
        .padding(5);
    let exclude = text_input("files to exclude", &app.search.exclude)
        .on_input(Message::SearchExcludeChanged)
        .on_submit(Message::SearchSubmitted)
        .size(12)
        .padding(5);

    let status_line = if let Some(err) = &app.search.error {
        Some((err.clone(), theme::rgb(0xe06c75)))
    } else if app.search.running {
        Some(("Searching…".to_string(), theme::DIM))
    } else if app.search.ran {
        let n = app.search.hits.len();
        let msg = if n >= crate::search::MAX_HITS {
            format!("{n}+ matches (capped)")
        } else {
            format!("{n} matches")
        };
        Some((msg, theme::DIM))
    } else {
        None
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for hit in &app.search.hits {
        if last_rel != Some(hit.rel.as_str()) {
            last_rel = Some(hit.rel.as_str());
            rows.push(group_header(&hit.rel));
        }
        rows.push(
            button(
                row![
                    text(hit.line.to_string()).size(11).color(theme::DIM).width(36),
                    text(&hit.preview).size(12).wrapping(Wrapping::None),
                ]
                .spacing(4),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 1.0,
                right: 6.0,
                bottom: 1.0,
                left: 8.0,
            })
            .on_press(Message::OpenAbs {
                abs: hit.abs.clone(),
                line: Some(hit.line),
                push: true,
            })
            .into(),
        );
    }

    let mut col = column![input, options, include, exclude]
        .spacing(6)
        .padding(8);
    if let Some((status, color)) = status_line {
        col = col.push(text(status).size(11).color(color));
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

/// Label a history entry: the symbol name recorded at nav time (stable as lines
/// shift), else `file:line`. Jumps land on symbol lines, so most read as names.
pub(crate) fn loc_label(loc: &crate::history::Loc, label: Option<&str>) -> String {
    if let Some(name) = label {
        return name.to_string();
    }
    let base = loc.path.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    match loc.line {
        Some(l) => format!("{base}:{l}"),
        None => base.to_string(),
    }
}

/// The TRAIL tab: the navigation history as a tree. Backtracking then exploring
/// elsewhere branches (the old path is kept), so this is the full reading trail.
/// Indentation follows the tree depth; nodes with children can be collapsed;
/// click a node to jump. Scrolls both ways for deep/wide trees.
pub(crate) fn trail_tab(app: &App) -> Element<'_, Message> {
    let visits = app.history.flatten_with(&app.trail_collapsed);
    if visits.is_empty() {
        return empty_state(
            Glyph::Minimap,
            "No reading trail yet",
            "Jump around the code and your trail builds here.",
            None,
        );
    }

    let header = row![
        text("Reading trail").size(11).color(theme::DIM),
        space().width(Fill),
        button(text("Clear").size(10).color(theme::DIM))
            .style(theme::list_row(false))
            .padding([1, 6])
            .on_press(Message::HistoryClear),
    ]
    .align_y(iced::Center)
    .padding(Padding { top: 4.0, right: 8.0, bottom: 2.0, left: 8.0 });

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for v in &visits {
        // Indent by tree depth, but cap it: past ~8 levels a deep branch would
        // otherwise push the node (including the current one) off the panel's
        // right edge. Beyond the cap, depth stops adding indent.
        let indent = 4.0 + (v.depth.min(8) as f32) * 10.0;
        let name_color = if v.is_current { theme::ACCENT } else { theme::FG };
        let fname = v.loc.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let (glyph, gcolor) = crate::icons::file_icon(fname);

        // A collapse chevron for nodes with children (forks stand out in accent);
        // leaves get a fixed-width spacer so names still line up.
        let toggle: Element<'_, Message> = if v.has_children {
            let ar = if v.collapsed { "▸" } else { "▾" };
            button(text(ar).size(10).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([2, 3])
                .on_press(Message::TrailToggleCollapse(v.id))
                .into()
        } else {
            space().width(12).into()
        };
        // Status marker: current ● (accent), fork ⋔ (accent), other visited
        // nodes a grey ● so the trail reads as a string of nodes. The dots are
        // small; the fork glyph stays readable.
        let (marker, mcolor, msize) = if v.is_current {
            ("●", theme::ACCENT, 7.0)
        } else if v.forks {
            ("⋔", theme::ACCENT, 11.0)
        } else {
            ("●", theme::DIM, 7.0)
        };
        let jump = button(
            row![
                text(marker).size(msize).color(mcolor).width(10),
                icon_text(glyph, gcolor, 12.0),
                column![
                    text(loc_label(&v.loc, v.label.as_deref())).size(12).color(name_color),
                    text(rel_of(app, &v.loc.path)).size(9).color(theme::DIM).wrapping(Wrapping::None),
                ],
            ]
            .spacing(5)
            .align_y(iced::Center),
        )
        .style(theme::list_row(v.is_current))
        .padding(Padding { top: 2.0, right: 8.0, bottom: 2.0, left: 4.0 })
        .on_press(Message::HistoryJump(v.id));

        rows.push(
            row![space().width(indent), toggle, jump]
                .spacing(1)
                .align_y(iced::Center)
                .into(),
        );
    }
    column![
        header,
        scrollable(Column::with_children(rows).spacing(1))
            .direction(Direction::Both {
                vertical: Scrollbar::new().width(6.0).scroller_width(6.0),
                horizontal: Scrollbar::new().width(6.0).scroller_width(6.0),
            })
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

pub(crate) fn marks_tab(app: &App) -> Element<'_, Message> {
    if app.bookmarks.is_empty() {
        return empty_state(
            Glyph::Bookmark,
            "No bookmarks yet",
            "Press ⌘D to mark the current line.",
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for (idx, bm) in app.bookmarks.iter().enumerate() {
        if last_rel != Some(bm.rel.as_str()) {
            last_rel = Some(bm.rel.as_str());
            rows.push(group_header(&bm.rel));
        }
        // Clip the preview to its own column so a long line never draws over the
        // trailing pencil/✕ icons; truncate with an ellipsis for the cut affordance.
        let top = row![
            text(bm.line.to_string()).size(11).color(theme::DIM).width(36),
            container(text(truncate_ellipsis(&bm.preview, 48)).size(12).wrapping(Wrapping::None))
                .clip(true)
                .width(Fill),
        ]
        .spacing(4)
        .width(Fill);
        // A saved note shows as a wrapped dim line under the preview.
        let main: Element<'_, Message> = match &bm.note {
            Some(note) => column![
                top,
                container(text(note).size(10).color(theme::FG_MUTED).wrapping(Wrapping::Word))
                    .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 40.0 }),
            ]
            .spacing(1)
            .width(Fill)
            .into(),
            None => top.into(),
        };
        let note_color = if bm.note.is_some() { theme::ACCENT } else { theme::DIM };
        // The whole row is one full-width button (jump); the pencil/✕ are inner
        // buttons that capture their own clicks, so the highlight spans the row.
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::BookmarkNoteEdit(bm.rel.clone(), bm.line));
        let close = button(glyph::icon(Glyph::Close, theme::DIM, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::BookmarkRemoved(idx));
        rows.push(
            button(row![main, pencil, close].spacing(2).align_y(iced::Center))
                .style(theme::list_row(false))
                .width(Fill)
                .padding(Padding { top: 2.0, right: 4.0, bottom: 2.0, left: 8.0 })
                .on_press(Message::OpenRel {
                    rel: bm.rel.clone(),
                    line: Some(bm.line),
                })
                .into(),
        );
    }

    column![scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill)]
        .padding(Padding {
            top: 6.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

/// The NOTES tab: every reading note grouped by file, with progress. Each note
/// jumps to its symbol's live line; a note whose symbol has vanished is flagged
/// "detached" (it opens the file top) rather than pointing at the wrong code.
pub(crate) fn notes_tab(app: &App) -> Element<'_, Message> {
    if app.notes.is_empty() {
        return empty_state(
            Glyph::Note,
            "No reading notes yet",
            "In the OUTLINE, click ○ to mark a symbol understood, or ✎ to add a note.",
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for n in &app.notes {
        if last_rel != Some(n.rel.as_str()) {
            last_rel = Some(n.rel.as_str());
            rows.push(group_header(&n.rel));
        }
        let line = app.note_symbol_line(&n.rel, &n.symbol);
        // Leading understood toggle.
        let (cg, gcolor) =
            if n.understood { (Glyph::CheckCircle, theme::ACCENT) } else { (Glyph::Circle, theme::DIM) };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteToggleUnderstood { rel: n.rel.clone(), symbol: n.symbol.clone() });

        // Symbol name + its live location (or a "detached" flag when orphaned).
        let loc: Element<'_, Message> = match line {
            Some(l) => text(format!("L{l}")).size(10).color(theme::DIM).into(),
            None => text("detached").size(10).color(theme::rgb(0xe5c07b)).into(),
        };
        let head = row![
            text(&n.symbol)
                .size(12)
                .color(if n.understood { theme::DIM } else { theme::FG })
                .wrapping(Wrapping::None),
            loc,
        ]
        .spacing(6)
        .width(Fill)
        .align_y(iced::Center);
        let main: Element<'_, Message> = if n.text.is_empty() {
            head.into()
        } else {
            column![
                head,
                container(text(&n.text).size(10).color(theme::FG_MUTED).wrapping(Wrapping::Word))
                    .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 0.0 }),
            ]
            .spacing(1)
            .width(Fill)
            .into()
        };

        let note_color = if n.text.is_empty() { theme::DIM } else { theme::ACCENT };
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteEditStart { rel: n.rel.clone(), symbol: n.symbol.clone() });
        let close = button(glyph::icon(Glyph::Close, theme::DIM, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::NoteRemove { rel: n.rel.clone(), symbol: n.symbol.clone() });
        // The name area jumps; the toggle/pencil/✕ capture their own clicks.
        let jump = button(main)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding { top: 2.0, right: 4.0, bottom: 2.0, left: 4.0 })
            .on_press(Message::NoteJump { rel: n.rel.clone(), symbol: n.symbol.clone() });
        rows.push(row![toggle, jump, pencil, close].spacing(1).align_y(iced::Center).into());
    }

    column![
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill)
    ]
    .padding(Padding { top: 6.0, right: 0.0, bottom: 0.0, left: 0.0 })
    .into()
}

/// Short badge for an LSP SymbolKind number.
pub(crate) fn kind_short(kind: u8) -> &'static str {
    match kind {
        12 | 6 | 9 => "fn",  // Function / Method / Constructor
        5 | 23 => "type",    // Class / Struct
        11 | 10 => "trait",  // Interface / Enum
        2 => "mod",          // Module
        _ => "",
    }
}

/// The "Ask" (semantic search) tab: a natural-language query over the embedding
/// index, with a build/refresh control and ranked results that jump to the code.
pub(crate) fn semantic_tab(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let n = app.embed_index.entries.len();

    let input = text_input("Ask by meaning…", &app.semantic_query)
        .on_input(Message::SemanticQueryChanged)
        .on_submit(Message::SemanticSearch)
        .size(13)
        .padding(7);

    let build_label = if app.building_embeddings {
        "Building…"
    } else if n == 0 {
        "Build index"
    } else {
        "Rebuild"
    };
    let mut build = button(text(build_label).size(11)).style(theme::toolbar_button).padding([2, 8]);
    if !app.building_embeddings {
        build = build.on_press(Message::BuildEmbeddings);
    }
    // The index builds itself from explanation summaries (automatically, after
    // Explain All) — so the hint only reports state, never asks for a manual step.
    let info = text(if app.building_embeddings {
        "Building the index…".to_string()
    } else if n > 0 {
        format!("{n} indexed")
    } else if app.explanations.is_empty() {
        "Run Explain All to enable semantic search.".to_string()
    } else if !app.embed_available {
        "Set an embedding provider in Settings.".to_string()
    } else {
        "Preparing the index…".to_string()
    })
    .size(10)
    .color(theme::DIM);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(row![build, space().width(Fill), info].align_y(iced::Center).into());
    if app.searching_semantic {
        rows.push(text("Searching…").size(11).color(theme::DIM).into());
    }
    for (node, score) in &app.semantic_results {
        let label = match node {
            Node::Function { file, name } => format!("{name} · {}", rel_of(app, file)),
            Node::File(p) => rel_of(app, p),
            Node::Folder(p) => rel_of(app, p),
        };
        let sum = app.explanations.get(node).map(|c| c.summary.as_str()).unwrap_or("");
        let short: String = sum.chars().take(96).collect();
        rows.push(
            button(
                column![
                    row![
                        text(label).size(12).color(theme::ACCENT).wrapping(Wrapping::None),
                        space().width(Fill),
                        text(format!("{:.0}%", score * 100.0)).size(9).color(theme::DIM),
                    ]
                    .align_y(iced::Center),
                    text(short).size(10).color(theme::DIM),
                ]
                .spacing(1),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 6])
            .on_press(Message::OpenNode(node.clone()))
            .into(),
        );
    }

    container(
        column![
            input,
            scrollable(Column::with_children(rows).spacing(4).width(Fill))
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Fill),
        ]
        .spacing(8)
        .padding([8, 8]),
    )
    .height(Fill)
    .into()
}

/// A clickable chip for a retrieved source node — jumps to the code on press,
/// showing the similarity score when it came from the ranked retrieval.
pub(crate) fn source_chip<'a>(node: &crate::explain::Node, score: f32) -> Element<'a, Message> {
    use crate::explain::Node;
    let label = match node {
        Node::Function { name, .. } => name.clone(),
        Node::File(p) | Node::Folder(p) => {
            p.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string()
        }
    };
    let pct = if score > 0.0 {
        format!("  {}%", (score * 100.0).round() as i32)
    } else {
        String::new()
    };
    button(text(format!("{label}{pct}")).size(10).color(theme::DIM))
        .style(theme::toolbar_button)
        .padding([1, 6])
        .on_press(Message::OpenNode(node.clone()))
        .into()
}

