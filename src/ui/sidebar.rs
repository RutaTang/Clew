//! Left sidebar and its tabs (files/search/trail/marks/notes/semantic/walk).

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// The sidebar tabs in strip order — the one source of truth for both rendering
/// the strip and computing which one to scroll into view (`reveal_sidebar_tab`).
pub(crate) const SIDEBAR_TABS: [(&str, SidebarTab); 11] = [
    ("FILES", SidebarTab::Files),
    ("SEARCH", SidebarTab::Search),
    ("FIND", SidebarTab::Semantic),
    ("MARKS", SidebarTab::Marks),
    ("TRAIL", SidebarTab::Trail),
    ("CALLS", SidebarTab::Calls),
    ("FLOW", SidebarTab::Flow),
    ("IMPORTS", SidebarTab::Imports),
    ("WALK", SidebarTab::Walk),
    ("NOTES", SidebarTab::Notes),
    ("DOCS", SidebarTab::Docs),
];

/// How many of [`SIDEBAR_TABS`] stand on the strip itself: the reader's
/// everyday views. The rest open from what shows them — tracing a value,
/// the import graph, a walkthrough, a symbol's docs — and from the command
/// palette, each under a title of its own.
pub(crate) const STRIP_VIEWS: usize = 6;

/// A view's icon on its tab.
pub(crate) fn tab_glyph(tab: SidebarTab) -> Glyph {
    match tab {
        SidebarTab::Files => Glyph::Folder,
        SidebarTab::Search => Glyph::TextSearch,
        SidebarTab::Semantic => Glyph::FindMeaning,
        SidebarTab::Marks => Glyph::Bookmark,
        SidebarTab::Trail => Glyph::Route,
        SidebarTab::Calls => Glyph::Network,
        SidebarTab::Flow => Glyph::Flow,
        SidebarTab::Imports => Glyph::ImportGraph,
        SidebarTab::Walk => Glyph::Compass,
        SidebarTab::Notes => Glyph::Note,
        SidebarTab::Docs => Glyph::Book,
    }
}

/// A view's name, as its tab's tooltip says it.
pub(crate) fn tab_name(tab: SidebarTab) -> &'static str {
    match tab {
        SidebarTab::Files => tr!("Files"),
        SidebarTab::Search => tr!("Search text"),
        SidebarTab::Semantic => tr!("Find by meaning"),
        SidebarTab::Marks => tr!("Bookmarks"),
        SidebarTab::Trail => tr!("Reading trail"),
        SidebarTab::Calls => tr!("Calls"),
        SidebarTab::Flow => tr!("Value flow"),
        SidebarTab::Imports => tr!("Imports"),
        SidebarTab::Walk => tr!("Walkthroughs"),
        SidebarTab::Notes => tr!("Reading notes"),
        SidebarTab::Docs => tr!("API docs"),
    }
}

pub(crate) fn sidebar(app: &App) -> Element<'_, Message> {
    // The views as a segmented row of icons, each naming itself on hover;
    // the chosen one raised as a chip.
    let segment = |glyph: Glyph, chosen: bool, name: &'static str, msg: Message| {
        tip_at(
            button(
                container(glyph::icon(
                    glyph,
                    if chosen {
                        theme::fg_bright()
                    } else {
                        theme::fg_muted()
                    },
                    15.0,
                ))
                .width(Fill)
                .height(Fill)
                .align_x(iced::Center)
                .align_y(iced::Center),
            )
            .style(theme::segment(chosen))
            .padding(0)
            .width(Fill)
            .height(Length::Fixed(28.0))
            .on_press(msg),
            name,
            None,
            tooltip::Position::Bottom,
        )
    };
    let mut tabs_row = Row::new().spacing(2);
    for (_, this) in SIDEBAR_TABS.iter().take(STRIP_VIEWS) {
        tabs_row = tabs_row.push(segment(
            tab_glyph(*this),
            app.sidebar == *this,
            tab_name(*this),
            Message::Window(WindowMsg::SidebarTabPicked(*this)),
        ));
    }
    let tabs = container(tabs_row).padding(2).style(theme::trough);
    // A view not on the strip, opened by what shows it, is named over it,
    // with the way back to the files.
    let elsewhere = SIDEBAR_TABS
        .iter()
        .skip(STRIP_VIEWS)
        .any(|(_, t)| *t == app.sidebar);
    let titled: Element<'_, Message> = if elsewhere {
        container(
            row![
                glyph::icon(tab_glyph(app.sidebar), theme::fg_muted(), 14.0),
                text(tab_name(app.sidebar))
                    .size(ts::BASE)
                    .font(crate::fonts::UI_SEMIBOLD)
                    .color(theme::fg()),
                space().width(Fill),
                tip_at(
                    button(glyph::icon(Glyph::Close, theme::fg_muted(), 13.0))
                        .style(theme::icon_button(false))
                        .padding(5)
                        .on_press(Message::Window(WindowMsg::SidebarTabPicked(
                            SidebarTab::Files,
                        ))),
                    tr!("Back to the files"),
                    None,
                    tooltip::Position::Bottom,
                ),
            ]
            .spacing(8)
            .align_y(iced::Center),
        )
        .padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: SIDEBAR_GAP,
            left: 14.0,
        })
        .into()
    } else {
        slot()
    };

    // The Files view narrows to what its filter matches. Escape in the
    // field, or its ×, empties it: it stayed filled however often Escape
    // was pressed, and only deleting every letter brought the tree back.
    let filter: Element<'_, Message> =
        if app.sidebar == SidebarTab::Files && app.proj.project.is_some() {
            let cleared = Message::Window(WindowMsg::FileFilterChanged(String::new()));
            let mut field = row![
                glyph::icon(Glyph::Filter, theme::dim(), 13.0),
                ts::field_in(
                    text_input(tr!("Filter files"), &app.file_filter)
                        .id(super::file_filter_id())
                        .on_input(|s| Message::Window(WindowMsg::FileFilterChanged(s)))
                        .on_submit(Message::Window(WindowMsg::FileFilterSubmitted))
                        .style(theme::bare_input),
                    FILTER_H - 2.0,
                    ts::INTER,
                    ts::BODY,
                ),
            ]
            .spacing(8)
            .height(Fill)
            .align_y(iced::Center);
            if !app.file_filter.is_empty() {
                field = field.push(tip_at(
                    button(glyph::icon(Glyph::Close, theme::dim(), 12.0))
                        .style(theme::toolbar_button)
                        .padding(3)
                        .on_press(cleared.clone()),
                    tr!("Clear filter"),
                    None,
                    tooltip::Position::Bottom,
                ));
            }
            container(
                container(super::escape_clears::EscapeClears::new(
                    field,
                    Message::Window(WindowMsg::FileFilterEscaped),
                ))
                .height(Length::Fixed(FILTER_H))
                // The design's 10 inside its 1 px border, which here takes
                // no room of its own.
                .padding([1, 11])
                .style(theme::field(!app.file_filter.is_empty())),
            )
            .padding(Padding {
                top: 0.0,
                right: 10.0,
                bottom: SIDEBAR_GAP,
                left: 10.0,
            })
            .into()
        } else {
            slot()
        };

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
        SidebarTab::Semantic => semantic_tab(app),
        SidebarTab::Marks => marks_tab(app),
        SidebarTab::Trail => trail_tab(app),
        SidebarTab::Calls => calls_tab(app),
        SidebarTab::Flow => flow_tab(app),
        SidebarTab::Imports => imports_tab(app),
        SidebarTab::Walk => walk_tab(app),
        SidebarTab::Notes => notes_tab(app),
        SidebarTab::Docs => docs_tab(app),
    };

    // Each tab's own widgets: tabs of one shape (a list in a scrollable)
    // were handed each other's scroll offset as one replaced another.
    container(column![
        container(tabs).padding(Padding {
            top: 2.0,
            right: 10.0,
            bottom: SIDEBAR_GAP,
            left: 10.0,
        }),
        titled,
        filter,
        crate::ui::fresh::fresh_for(&app.sidebar, content)
    ])
    .width(Length::Fixed(app.sidebar_width))
    .height(Fill)
    .style(theme::chrome)
    .into()
}

/// The guided-walkthrough tab. The top bar toggles between *searching* the saved
/// library of tours and *walking* (generating) a new one. Opening a tour steps
/// through it, each step driving the editor to its anchor; regenerating a tour
/// lives next to its title.
pub(crate) fn walk_tab(app: &App) -> Element<'_, Message> {
    let header = walk_header(app);
    // A quick action to review the current branch/PR changes as a narrated tour.
    let mut actions = column![
        button(text(format!("\u{2387} {}", tr!("Review branch changes"))).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([4, 10])
            .width(Fill)
            .on_press(Message::Walk(WalkMsg::GenerateDiff)),
    ]
    .spacing(4);
    // The last debug run, once it stopped somewhere: a tour of the path it
    // actually took.
    if !app.debug.trace.is_empty() {
        actions = actions.push(
            button(
                text(format!(
                    "\u{25B6} {}",
                    trn!(
                        "Walk the last run (1 stop)",
                        "Walk the last run ({n} stops)",
                        app.debug.trace.len()
                    )
                ))
                .size(ts::SMALL),
            )
            .style(theme::toolbar_button)
            .padding([4, 10])
            .width(Fill)
            .on_press(Message::Walk(WalkMsg::GenerateTrace)),
        );
    }
    let review = container(actions).padding(Padding {
        top: 0.0,
        right: 8.0,
        bottom: 6.0,
        left: 8.0,
    });

    // The library list is always shown under the input; selecting a tour expands
    // its steps inline (accordion) and its narration into the bottom pane — no
    // separate "back to library" navigation. Generation is shown per-row, so the
    // rest of the library stays usable while a tour is being built.
    let list = walk_library(app);
    let Some(wt) = app.proj.walk.open_tour() else {
        return column![header, review, hairline(), list]
            .height(Fill)
            .into();
    };

    let narration_block = walk_narration(app, wt);
    // The divider says where the pointer is in the window; the narration
    // is inside its frame.
    let frame = app.window_frame;
    column![
        header,
        review,
        hairline(),
        list,
        crate::resize::Divider::horizontal(move |v| {
            Message::Walk(WalkMsg::ResizeNarration(v - frame))
        }),
        narration_block,
    ]
    .height(Fill)
    .into()
}

/// A human label for a walkthrough's scope: the whole codebase, a change review,
/// a debug run, or the user's feature prompt.
pub(crate) fn scope_label(scope: &str) -> String {
    if scope.is_empty() {
        tr!("Whole codebase").to_string()
    } else if let Some(rest) = scope.strip_prefix("@trace") {
        match rest.trim() {
            "" => tr!("Debug run").to_string(),
            program => tr!("Debug run ({program})", program = program),
        }
    } else if let Some(rest) = scope.strip_prefix("@diff") {
        match rest.trim() {
            "" => tr!("Change review").to_string(),
            label => tr!("Change review ({label})", label = label),
        }
    } else {
        scope.to_string()
    }
}

/// The top bar: a Search/Walk segmented toggle, the shared input, and (in Walk
/// mode) a Generate button.
pub(crate) fn walk_header(app: &App) -> Element<'_, Message> {
    let is_search = app.walk_ui.mode == crate::WalkMode::Search;
    // Two segments, the mode in use raised; only the other is pressable (it
    // flips mode). On a line of its own: beside the field it left the field
    // too narrow to say what it is for.
    let seg = |label: &str, active: bool| {
        button(text(label.to_string()).size(ts::SMALL))
            .style(theme::segment(active))
            .padding([2, 10])
            .on_press_maybe((!active).then_some(Message::Walk(WalkMsg::ToggleMode)))
    };
    let toggle =
        container(row![seg(tr!("Search"), is_search), seg(tr!("Walk"), !is_search)].spacing(1))
            .padding(2)
            .style(theme::trough);

    let placeholder = if is_search {
        tr!("Search walkthroughs…")
    } else {
        tr!("A feature, or leave empty for all")
    };
    let mut input = text_input(placeholder, &app.walk_ui.input)
        .style(theme::input)
        .on_input(|v| Message::Walk(WalkMsg::InputChanged(v)))
        .size(ts::BODY)
        .padding(6);
    if !is_search {
        // Enter submits the same way the Generate button does ("" = whole codebase).
        input = input.on_submit(Message::Walk(WalkMsg::Generate(app.walk_ui.input.clone())));
    }
    let mut field = row![input].spacing(6).align_y(iced::Center);
    if !is_search {
        field = field.push(
            button(text(tr!("Generate")).size(ts::SMALL))
                .style(theme::secondary_button)
                .padding([5, 10])
                .on_press(Message::Walk(WalkMsg::Generate(app.walk_ui.input.clone()))),
        );
    }
    container(column![toggle, field].spacing(8))
        .padding(8)
        .into()
}

/// The library list: every saved tour, filtered by the search query, each with a
/// per-tour Regenerate button on the right.
pub(crate) fn walk_library(app: &App) -> Element<'_, Message> {
    let query = if app.walk_ui.mode == crate::WalkMode::Search {
        app.walk_ui.input.trim().to_lowercase()
    } else {
        String::new()
    };
    let matches = |wt: &crate::walkthrough::Walkthrough| {
        query.is_empty()
            || wt.title.to_lowercase().contains(&query)
            || wt.scope.to_lowercase().contains(&query)
    };

    let visible: Vec<(usize, &crate::walkthrough::Walkthrough)> = app
        .proj
        .walk
        .library
        .iter()
        .enumerate()
        .filter(|(_, wt)| matches(wt))
        .collect();

    // The scope currently generating, and — if it's a brand-new scope not yet in
    // the library — the label for a temporary "pending" row at the top.
    let gen_scope = app.proj.walk.generating.as_deref();
    let pending_new: Option<&str> =
        gen_scope.filter(|s| !app.proj.walk.library.iter().any(|w| w.scope.as_str() == *s));

    if app.proj.walk.library.is_empty() && pending_new.is_none() {
        return empty_state(Glyph::Compass, tr!("No walkthroughs yet"), "", None);
    }
    if visible.is_empty() && pending_new.is_none() {
        return empty_state(Glyph::Search, tr!("No matches"), "", None);
    }

    // The current step of the open tour (for highlighting the expanded steps).
    let cur = app
        .proj
        .walk
        .open_tour()
        .map(|w| app.proj.walk.step.min(w.steps.len().saturating_sub(1)));

    let mut list = Column::new().spacing(2).padding(8);

    // A new tour being generated shows a pending row until it lands in the library.
    if let Some(scope) = pending_new {
        let label = scope_label(scope);
        list = list.push(
            container(
                column![
                    text(label).size(ts::BASE).color(theme::fg()),
                    text(tr!("Generating…"))
                        .size(ts::CAPTION)
                        .color(theme::accent()),
                ]
                .spacing(1),
            )
            .width(Fill)
            .padding([6, 8]),
        );
    }

    for (_, wt) in visible {
        let is_open = app.proj.walk.open.as_deref() == Some(wt.scope.as_str());
        let busy = gen_scope == Some(wt.scope.as_str());
        let (subtitle, sub_color) = if busy {
            (tr!("Generating…").to_string(), theme::accent())
        } else {
            (scope_label(&wt.scope), theme::dim())
        };
        // The tour row: a full-width clickable title (so its selected highlight
        // spans the whole row) with the regenerate/delete controls layered on top
        // at the right via a stack. Leave right padding for them so the title
        // text never runs under the controls.
        let title = button(
            column![
                text(wt.title.clone()).size(ts::BASE).color(if is_open {
                    theme::fg_bright()
                } else {
                    theme::fg()
                }),
                text(subtitle).size(ts::CAPTION).color(sub_color),
            ]
            .spacing(1),
        )
        .style(theme::list_row(is_open))
        .width(Fill)
        .padding(Padding {
            top: 6.0,
            right: if busy { 8.0 } else { 62.0 },
            bottom: 6.0,
            left: 8.0,
        })
        .on_press(if is_open {
            Message::Walk(WalkMsg::Back)
        } else {
            Message::Walk(WalkMsg::Open(wt.scope.clone()))
        });
        let tour_row: Element<'_, Message> = if busy {
            title.into()
        } else {
            let controls = container(
                row![
                    button(text("↻").size(ts::BASE))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(Message::Walk(WalkMsg::Regenerate(wt.scope.clone()))),
                    button(glyph::icon(Glyph::Close, theme::fg_muted(), 12.0))
                        .style(theme::toolbar_button)
                        .padding([7, 9])
                        .on_press(Message::Walk(WalkMsg::Delete(wt.scope.clone()))),
                ]
                .spacing(2),
            )
            .width(Fill)
            .height(Fill)
            .align_x(iced::Right)
            .align_y(iced::Center)
            .padding(Padding {
                top: 0.0,
                right: 6.0,
                bottom: 0.0,
                left: 0.0,
            });
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
                            text(format!("{}", si + 1))
                                .size(ts::CAPTION)
                                .color(theme::dim())
                                .width(18),
                            text(step.title.clone()).size(ts::BODY).color(if is_cur {
                                theme::fg()
                            } else {
                                theme::dim()
                            }),
                        ]
                        .spacing(6)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_cur))
                    .width(Fill)
                    .padding(Padding {
                        top: 4.0,
                        right: 8.0,
                        bottom: 4.0,
                        left: 22.0,
                    })
                    .on_press(Message::Walk(WalkMsg::Goto {
                        scope: wt.scope.clone(),
                        step: si,
                    })),
                );
            }
        }
    }

    scrollable(list.width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill)
        .into()
}

/// The bottom pane for the open tour: a compact nav row (file + step counter +
/// prev/next) over the current step's rendered narration.
pub(crate) fn walk_narration<'a>(
    app: &'a App,
    wt: &'a crate::walkthrough::Walkthrough,
) -> Element<'a, Message> {
    let n = wt.steps.len();
    let cur = app.proj.walk.step.min(n.saturating_sub(1));
    let Some(step) = wt.steps.get(cur) else {
        return space().into();
    };

    let nav = row![
        text(step.file.clone())
            .size(ts::CAPTION)
            .color(theme::accent()),
        space().width(Fill),
        button(text("‹").size(ts::EMPHASIS))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::Walk(WalkMsg::Step(-1))),
        text(format!("{}/{}", cur + 1, n))
            .size(ts::SMALL)
            .color(theme::dim()),
        button(text("›").size(ts::EMPHASIS))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(Message::Walk(WalkMsg::Step(1))),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .padding([4, 8]);

    // Where the step landed when that is not where it meant to (its symbol
    // is missing): kept with the step, in a slot that is always present so
    // the narration's scroll state survives the note coming and going.
    let note: Element<'_, Message> = match &app.proj.walk.anchor_note {
        Some(note) => container(
            text(note.clone())
                .size(ts::SMALL)
                .color(theme::warning())
                .wrapping(iced::widget::text::Wrapping::Word),
        )
        .padding([0, 8])
        .width(Fill)
        .into(),
        None => slot(),
    };

    let body: Element<'_, Message> = if app.proj.walk.prepared.is_empty() {
        text(step.narration.clone())
            .size(ts::BODY)
            .color(theme::fg())
            .width(Fill)
            .into()
    } else {
        Column::with_children(render_prepared(app, &app.proj.walk.prepared))
            .spacing(8)
            .width(Fill)
            .into()
    };
    // From its top for each step and tour (`fresh`), not where the last
    // step's narration was scrolled to.
    let narration = crate::ui::fresh::fresh_for(
        &(&wt.title, &wt.scope, cur),
        scrollable(container(body).padding(Padding {
            top: 0.0,
            right: 8.0,
            bottom: 8.0,
            left: 8.0,
        }))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill),
    );

    container(column![nav, note, narration])
        .height(Length::Fixed(app.walk_ui.narration_height))
        .into()
}

pub(crate) fn files_tab(app: &App) -> Element<'_, Message> {
    let Some(project) = &app.proj.project else {
        // Same centered empty-state pattern as the other tabs (Trail, Marks, …).
        // No action button here: the open/connect actions live in the centered
        // welcome hero, so the sidebar just states what's going on.
        return if app.scanning {
            empty_state(Glyph::Search, tr!("Scanning…"), "", None)
        } else if app.connection.is_remote() {
            empty_state(
                Glyph::Remote,
                tr!("No folder open"),
                tr!("Browse the host to open a folder."),
                None,
            )
        } else {
            empty_state(
                Glyph::Folder,
                tr!("No folder open"),
                tr!("Open a folder to start reading."),
                None,
            )
        };
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let filter = app.file_filter.trim();
    if filter.is_empty() {
        append_tree_rows(&mut rows, &project.tree, "", 0, app);
    } else {
        filtered_file_rows(&mut rows, project, filter, app);
        if rows.is_empty() {
            return empty_state(Glyph::Filter, tr!("No matching files"), "", None);
        }
    }
    scrollable(
        container(
            Column::with_children(rows)
                .spacing(TREE_ROW_GAP)
                .width(Fill),
        )
        .padding(Padding {
            top: 0.0,
            right: 10.0,
            bottom: 12.0,
            left: 10.0,
        }),
    )
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill)
    .into()
}

/// Most files the filter lists: a filter as short as one letter matches
/// most of a large project, and the list is for picking, not browsing.
const FILTER_LIMIT: usize = 400;

/// The files the Files view's filter lists, in its order: every file whose
/// path holds the filter (any case, its ends trimmed), at most
/// [`FILTER_LIMIT`]. Enter in the filter opens the first.
pub(crate) fn filtered_files<'a>(
    project: &'a crate::Project,
    filter: &str,
) -> impl Iterator<Item = &'a crate::fs_scan::FileEntry> {
    let needle = filter.trim().to_lowercase();
    project
        .files
        .iter()
        .filter(move |f| !needle.is_empty() && f.rel.to_lowercase().contains(&needle))
        .take(FILTER_LIMIT)
}

/// The Files view narrowed by its filter: every file whose path holds it
/// (any case), as its name with its folder beside it.
fn filtered_file_rows<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    project: &'a crate::Project,
    filter: &str,
    app: &'a App,
) {
    for f in filtered_files(project, filter) {
        let (folder, name) = f.rel.rsplit_once('/').unwrap_or(("", f.rel.as_str()));
        let is_current = app.active_viewer().is_some_and(|v| v.rel == f.rel);
        rows.push(
            button(
                row![
                    quiet_icon(Glyph::File, theme::dim()),
                    text(crate::highlight::in_order(name))
                        .size(ts::BASE)
                        .color(if is_current {
                            theme::fg_bright()
                        } else {
                            theme::fg()
                        })
                        .wrapping(Wrapping::None),
                    text(crate::highlight::in_order(folder))
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .spacing(6)
                .align_y(iced::Center),
            )
            .style(theme::list_row(is_current))
            .width(Fill)
            .padding([4, 8])
            .on_press(Message::Editor(EditorMsg::OpenRel {
                rel: f.rel.clone(),
                line: None,
            }))
            .into(),
        );
    }
}

/// A tree row's icon: the chrome's own line glyph in one quiet ink, in the
/// tree's fixed icon column.
fn quiet_icon(glyph: Glyph, color: iced::Color) -> Element<'static, Message> {
    container(glyph::icon(glyph, color, 14.0)).width(14).into()
}

/// Height of a row of the file tree, and the gap between rows.
const TREE_ROW_H: f32 = 28.0;
const TREE_ROW_GAP: f32 = 1.0;
/// Height of the Files view's filter field.
const FILTER_H: f32 = 30.0;
/// The room between the sidebar's views, its filter and its list.
const SIDEBAR_GAP: f32 = 12.0;

/// The file-tree mark of a file git tracks although `.gitignore` (or a
/// build-directory name) would hide it (see `fs_scan::ScanReport`).
pub(crate) const TRACKED_IGNORED_MARK: &str = tr_noop!("tracked · ignored");

pub(crate) fn append_tree_rows<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    node: &'a DirNode,
    prefix: &str,
    depth: u16,
    app: &'a App,
) {
    let indent = 6.0 + depth as f32 * 14.0;
    let pad = Padding {
        top: 0.0,
        right: 8.0,
        bottom: 0.0,
        left: indent,
    };

    for (name, child) in &node.dirs {
        let rel = join_rel(prefix, name);
        let expanded = app.proj.expanded.contains(&rel);
        let content = row![
            container(glyph::icon(
                if expanded {
                    Glyph::ChevronDown
                } else {
                    Glyph::ChevronRight
                },
                theme::dim(),
                10.0,
            ))
            .width(12)
            .align_x(iced::Center),
            quiet_icon(Glyph::Folder, theme::fg_muted()),
            // A name in the order it is spelled (`highlight::in_order`).
            ts::set_in(
                text(crate::highlight::in_order(name))
                    .color(theme::fg_muted())
                    .wrapping(Wrapping::None),
                TREE_ROW_H,
                ts::INTER,
                ts::BASE,
            ),
        ]
        .spacing(6)
        .height(Fill)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(false))
                .width(Fill)
                .height(Length::Fixed(TREE_ROW_H))
                .padding(pad)
                .on_press(Message::Project(ProjectMsg::ToggleDir(rel.clone())))
                .into(),
        );
        if expanded {
            append_tree_rows(rows, child, &rel, depth + 1, app);
        }
    }

    for name in &node.files {
        let rel = join_rel(prefix, name);
        let is_current = app.active_viewer().is_some_and(|v| v.rel == rel);
        // A file git tracks although the ignore rules would hide it: listed
        // (what the repository contains is never invisible), and said so.
        let mark: Element<'_, Message> = if app.proj.tracked_ignored.contains(&rel) {
            text(crate::i18n::lookup(TRACKED_IGNORED_MARK))
                .size(ts::CAPTION)
                .color(theme::dim())
                .wrapping(Wrapping::None)
                .into()
        } else {
            space().width(0).into()
        };
        let content = row![
            space().width(12), // align names under the folders' chevron column
            quiet_icon(
                Glyph::File,
                if is_current {
                    theme::fg()
                } else {
                    theme::dim()
                }
            ),
            ts::set_in(
                text(crate::highlight::in_order(name))
                    .font(if is_current {
                        crate::fonts::UI_MEDIUM
                    } else {
                        crate::fonts::UI
                    })
                    .color(if is_current {
                        theme::fg()
                    } else {
                        theme::fg_muted()
                    })
                    .wrapping(Wrapping::None),
                TREE_ROW_H,
                ts::INTER,
                ts::BASE,
            ),
            mark,
        ]
        .spacing(6)
        .height(Fill)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::list_row(is_current))
                .width(Fill)
                .height(Length::Fixed(TREE_ROW_H))
                .padding(pad)
                .on_press(Message::Editor(EditorMsg::OpenRel { rel, line: None }))
                .into(),
        );
    }
}

/// Room a tree row keeps for its chevron, so a row without one lines up.
pub(crate) const CHEVRON_W: f32 = 18.0;

/// A tree row's expand / collapse control: its chevron centred in a
/// square of [`CHEVRON_W`], lit under the pointer — a target a pointer
/// finds, not a glyph's width of one.
pub(crate) fn chevron<'a>(expanded: bool, msg: Message) -> Element<'a, Message> {
    button(
        container(glyph::icon(
            if expanded {
                Glyph::ChevronDown
            } else {
                Glyph::ChevronRight
            },
            theme::dim(),
            10.0,
        ))
        .center(Fill),
    )
    .style(theme::list_row(false))
    .width(Length::Fixed(CHEVRON_W))
    .height(Length::Fixed(CHEVRON_W))
    .padding(0)
    .on_press(msg)
    .into()
}

/// A file-type glyph in the embedded icon font, for inline use (breadcrumb,
/// finder rows, …).
pub(crate) fn icon_text(glyph: char, color: iced::Color, size: f32) -> iced::widget::Text<'static> {
    text(glyph.to_string())
        .font(crate::icons::ICON_FONT)
        .size(size)
        .color(color)
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
        glyph::icon(g, theme::empty_state_icon(), 42.0),
        space().height(6),
        text(title.to_string())
            .size(ts::EMPHASIS)
            .color(theme::fg()),
    ]
    .spacing(4)
    .align_x(iced::Center);
    if !subtitle.is_empty() {
        col = col.push(
            container(
                text(subtitle.to_string())
                    .size(ts::BODY)
                    .color(theme::dim())
                    .align_x(iced::Center),
            )
            .max_width(260),
        );
    }
    if let Some((label, msg)) = action {
        col = col.push(space().height(10));
        col = col.push(
            button(text(label.to_string()).size(ts::BASE))
                .style(theme::toolbar_button)
                .padding([7, 16])
                .on_press(msg),
        );
    }
    center(col).padding(20).into()
}

/// [`empty_state`] for a small region — a panel section — where a title and
/// a 42px icon would crowd it: a small muted icon over one centered line.
pub(crate) fn compact_empty_state<'a>(g: Glyph, note: impl Into<String>) -> Element<'a, Message> {
    let col = column![
        glyph::icon(g, theme::empty_state_icon(), 22.0),
        container(
            text(note.into())
                .size(ts::SMALL)
                .color(theme::dim())
                .align_x(iced::Center)
        )
        .max_width(240),
    ]
    .spacing(8)
    .align_x(iced::Center);
    center(col).padding(16).into()
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

    let input = text_input(tr!("Search in project…"), &app.proj.search.query)
        .style(theme::input)
        .id(search_input_id())
        .on_input(|v| Message::Nav(NavMsg::SearchQueryChanged(v)))
        .on_submit(Message::Nav(NavMsg::SearchSubmitted))
        .size(ts::BASE)
        .padding(7);

    // Match-option toggles: case-sensitive, whole-word, regex — as round as
    // the fields, outlined while off, filled in the accent while on.
    let chip = |label: &'static str, active: bool, opt: SearchOpt| -> Element<'_, Message> {
        button(text(label).size(ts::BODY).font(crate::fonts::CODE))
            .style(theme::toggle(active))
            .padding([3, 8])
            .on_press(Message::Nav(NavMsg::SearchToggle(opt)))
            .into()
    };
    let options = row![
        chip("Aa", app.proj.search.case_sensitive, SearchOpt::Case),
        chip("W", app.proj.search.whole_word, SearchOpt::WholeWord),
        chip(".*", app.proj.search.regex, SearchOpt::Regex),
    ]
    .spacing(4);

    // Include / exclude glob filters.
    let include = text_input(
        tr!("files to include (e.g. src/**)"),
        &app.proj.search.include,
    )
    .style(theme::input)
    .on_input(|v| Message::Nav(NavMsg::SearchIncludeChanged(v)))
    .on_submit(Message::Nav(NavMsg::SearchSubmitted))
    .size(ts::BODY)
    .padding(5);
    let exclude = text_input(tr!("files to exclude"), &app.proj.search.exclude)
        .style(theme::input)
        .on_input(|v| Message::Nav(NavMsg::SearchExcludeChanged(v)))
        .on_submit(Message::Nav(NavMsg::SearchSubmitted))
        .size(ts::BODY)
        .padding(5);

    let status_line = if let Some(err) = &app.proj.search.error {
        // As clew-core, or the host, said it: in the reader's language.
        Some((crate::i18n::message(err), theme::danger()))
    } else if app.proj.search.running {
        Some((tr!("Searching…").to_string(), theme::dim()))
    } else if app.proj.search.ran {
        let n = app.proj.search.hits.len();
        let msg = if n >= crate::search::MAX_HITS {
            tr!("{n}+ matches (capped)", n = n)
        } else {
            crate::app::count_matches(n)
        };
        // A language server's list in place of the search's results says
        // which list it is.
        let msg = match &app.proj.search.listing {
            Some(list) => format!("{list} · {msg}"),
            None => msg,
        };
        Some((msg, theme::dim()))
    } else if app.proj.search.query.trim().chars().count() == 1 {
        // One character, which matches most of a project, is searched for
        // only when asked (see `search_when_typing_pauses`).
        Some((tr!("Press Enter to search").to_string(), theme::dim()))
    } else {
        None
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for (i, hit) in app.proj.search.hits.iter().enumerate() {
        if last_rel != Some(hit.rel.as_str()) {
            last_rel = Some(hit.rel.as_str());
            rows.push(group_header(&hit.rel));
        }
        rows.push(
            button(
                row![
                    text(hit.line.to_string())
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .width(36),
                    search_preview(app, i, &hit.preview),
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
            .on_press(Message::Editor(EditorMsg::OpenAbs {
                abs: hit.abs.clone(),
                line: Some(hit.line),
                push: true,
            }))
            .into(),
        );
    }

    let mut col = column![input, options, include, exclude]
        .spacing(6)
        .padding(8);
    if let Some((status, color)) = status_line {
        col = col.push(text(status).size(ts::SMALL).color(color));
    }
    col.push(
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    )
    .into()
}

/// A result's preview: in its language's colours once they have come
/// (`SearchState::styled`), what the search matched — or the symbol a
/// reference list is of — marked as the code view marks a find. It was
/// plain text, and the reader had to look for the match along the line.
fn search_preview<'a>(app: &'a App, i: usize, preview: &'a str) -> Element<'a, Message> {
    // As the code view draws the line, wherever the preview came from — the
    // server's search, a reference read from disk or from an open pane: a
    // direction control in it reordered it here.
    let shown = crate::highlight::line_in_order(preview);
    let styled = app
        .proj
        .search
        .styled
        .get(i)
        // Only colours of the text shown: a preview read again since keeps
        // its plain look until its own arrive.
        .filter(|spans| spans.iter().map(|(t, _)| t.as_str()).collect::<String>() == *shown);
    let plain = [(shown.to_string(), None)];
    let spans: &[(String, Option<u8>)] = match styled {
        Some(spans) => spans,
        None => &plain,
    };
    // Found in the whole line where it could be read; else as matched in
    // the preview itself.
    let in_line = app
        .proj
        .search
        .marks
        .get(i)
        .and_then(Option::as_ref)
        .filter(|_| *shown == *preview);
    let marks = match in_line {
        Some(marks) => marks.clone(),
        None => app
            .proj
            .search
            .matcher
            .as_ref()
            .map(|m| m.ranges_in_preview(&shown))
            .unwrap_or_default(),
    };
    let pieces = mark_spans(spans, &marks);
    let mark = theme::with_alpha(theme::find(), 0.35);
    let spans: Vec<iced::widget::text::Span<'static, Message>> = pieces
        .into_iter()
        .map(|(piece, style, marked)| {
            let span = iced::widget::span(piece)
                .color(
                    style
                        .and_then(crate::highlight::style_color)
                        .unwrap_or(theme::code_fg()),
                )
                .size(ts::BODY);
            if marked {
                span.background(mark).border(iced::border::rounded(2))
            } else {
                span
            }
        })
        .collect();
    iced::widget::rich_text(spans)
        .wrapping(Wrapping::None)
        .into()
}

/// `spans` cut where `marks` (byte ranges of their joined text) begin and
/// end: each piece with its style, and whether a mark covers it.
pub(crate) fn mark_spans(
    spans: &[(String, Option<u8>)],
    marks: &[(usize, usize)],
) -> Vec<(String, Option<u8>, bool)> {
    let mut out = Vec::new();
    let mut at = 0;
    for (text, style) in spans {
        let end = at + text.len();
        let mut cuts: Vec<usize> = marks
            .iter()
            .flat_map(|&(s, e)| [s, e])
            .filter(|&c| c > at && c < end && text.is_char_boundary(c - at))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut from = at;
        for c in cuts.into_iter().chain(std::iter::once(end)) {
            if c > from {
                let marked = marks.iter().any(|&(s, e)| s <= from && c <= e);
                out.push((text[from - at..c - at].to_string(), *style, marked));
                from = c;
            }
        }
        at = end;
    }
    out
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
/// click a node to jump. Rows are the panel's width, as the file tree's are;
/// a name too long for it is cut at the edge.
pub(crate) fn trail_tab(app: &App) -> Element<'_, Message> {
    let visits = app.proj.history.flatten_with(&app.proj.trail_collapsed);
    if visits.is_empty() {
        return empty_state(Glyph::Minimap, tr!("No reading trail yet"), "", None);
    }

    let header = row![
        text(tr!("Reading trail"))
            .size(ts::SMALL)
            .color(theme::dim()),
        space().width(Fill),
        button(text(tr!("Clear")).size(ts::CAPTION).color(theme::dim()))
            .style(theme::list_row(false))
            .padding([1, 6])
            .on_press(Message::Reading(ReadingMsg::HistoryClear)),
    ]
    .align_y(iced::Center)
    .padding(Padding {
        top: 4.0,
        right: 8.0,
        bottom: 2.0,
        left: 8.0,
    });

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for v in &visits {
        // Indent by tree depth, but cap it: past ~8 levels a deep branch would
        // otherwise push the node (including the current one) off the panel's
        // right edge. Beyond the cap, depth stops adding indent.
        let indent = (v.depth.min(8) as f32) * 14.0;

        // A collapse chevron for nodes with children (forks stand out in accent);
        // leaves get a fixed-width spacer so names still line up.
        let toggle: Element<'_, Message> = if v.has_children {
            chevron(
                !v.collapsed,
                Message::Reading(ReadingMsg::TrailToggleCollapse {
                    id: v.id,
                    loc: v.loc.clone(),
                }),
            )
        } else {
            space().width(CHEVRON_W).into()
        };
        // Status marker: current dot (accent), fork ⋔ (accent), other visited
        // nodes a grey dot so the trail reads as a string of nodes. The dots
        // are drawn shapes rather than a tiny "●" glyph (text stays at or
        // above the type scale's floor); the fork glyph stays readable.
        let marker: Element<'_, Message> = if v.forks && !v.is_current {
            text("⋔")
                .size(ts::SMALL)
                .color(theme::accent())
                .width(10)
                .into()
        } else {
            let color = if v.is_current {
                theme::accent()
            } else {
                theme::dim()
            };
            container(trail_dot(color))
                .width(10)
                .align_x(iced::Center)
                .into()
        };
        // The row is the whole width, as the file tree's are — its depth
        // indents what is in it, not the row — and the place being read is
        // the row lit as the open file is, its name in the medium weight.
        let jump = button(
            row![
                space().width(indent),
                toggle,
                marker,
                container(glyph::icon(Glyph::File, theme::dim(), 12.0)).width(12),
                container(column![
                    text(loc_label(&v.loc, v.label.as_deref()))
                        .size(ts::BODY)
                        .font(if v.is_current {
                            crate::fonts::UI_MEDIUM
                        } else {
                            crate::fonts::UI
                        })
                        .color(theme::fg())
                        .wrapping(Wrapping::None),
                    text(rel_of(app, &v.loc.path))
                        .size(ts::CAPTION)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ])
                .width(Fill)
                .clip(true),
            ]
            .spacing(6)
            .align_y(iced::Center),
        )
        .style(theme::list_row(v.is_current))
        .width(Fill)
        .padding(Padding {
            top: 3.0,
            right: 8.0,
            bottom: 3.0,
            left: 6.0,
        })
        .on_press(Message::Reading(ReadingMsg::HistoryJump {
            id: v.id,
            loc: v.loc.clone(),
        }));

        rows.push(jump.into());
    }
    column![
        header,
        scrollable(Column::with_children(rows).spacing(1))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

pub(crate) fn marks_tab(app: &App) -> Element<'_, Message> {
    if app.proj.bookmarks.is_empty() {
        return empty_state(
            Glyph::Bookmark,
            tr!("No bookmarks yet"),
            if crate::keymap::MAC_KEYS {
                tr!("Press ⌘D to bookmark the current line.")
            } else {
                tr!("Press Ctrl+D to bookmark the current line.")
            },
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for bm in &app.proj.bookmarks {
        if last_rel != Some(bm.rel.as_str()) {
            last_rel = Some(bm.rel.as_str());
            rows.push(group_header(&bm.rel));
        }
        // Clip the preview to its own column so a long line never draws over the
        // trailing pencil/✕ icons; truncate with an ellipsis for the cut affordance.
        let top = row![
            text(bm.line.to_string())
                .size(ts::SMALL)
                .color(theme::dim())
                .width(36),
            container(
                text(truncate_ellipsis(
                    &crate::highlight::line_in_order(&bm.preview),
                    48
                ))
                .size(ts::BODY)
                .wrapping(Wrapping::None)
            )
            .clip(true)
            .width(Fill),
        ]
        .spacing(4)
        .width(Fill);
        // A saved note shows as a wrapped dim line under the preview.
        let main: Element<'_, Message> = match &bm.note {
            Some(note) => column![
                top,
                container(
                    text(note)
                        .size(ts::CAPTION)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::Word)
                )
                .padding(Padding {
                    top: 0.0,
                    right: 4.0,
                    bottom: 0.0,
                    left: 40.0
                }),
            ]
            .spacing(1)
            .width(Fill)
            .into(),
            None => top.into(),
        };
        let note_color = if bm.note.is_some() {
            theme::accent()
        } else {
            theme::dim()
        };
        // The whole row is one full-width button (jump); the pencil/✕ are inner
        // buttons that capture their own clicks, so the highlight spans the row.
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::Reading(ReadingMsg::BookmarkNoteEdit(
                bm.rel.clone(),
                bm.line,
            )));
        let close = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::Reading(ReadingMsg::BookmarkRemoved {
                rel: bm.rel.clone(),
                line: bm.line,
            }));
        rows.push(
            button(row![main, pencil, close].spacing(2).align_y(iced::Center))
                .style(theme::list_row(false))
                .width(Fill)
                .padding(Padding {
                    top: 2.0,
                    right: 4.0,
                    bottom: 2.0,
                    left: 8.0,
                })
                .on_press(Message::Editor(EditorMsg::OpenRel {
                    rel: bm.rel.clone(),
                    line: Some(bm.line),
                }))
                .into(),
        );
    }

    column![
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill)
    ]
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
/// The Reading notes view: the outline of the open file — where a symbol
/// is marked understood or given a note — over every note in the project.
pub(crate) fn notes_tab(app: &App) -> Element<'_, Message> {
    if app.active_viewer().is_none() {
        return notes_list(app);
    }
    let label = |s: &'static str| {
        container(
            text(s)
                .size(ts::SMALL)
                .font(crate::fonts::UI_SEMIBOLD)
                .color(theme::dim()),
        )
        .padding(Padding {
            top: 8.0,
            right: 14.0,
            bottom: 2.0,
            left: 14.0,
        })
    };
    column![
        label(tr!("In this file")),
        container(outline_content(app)).height(Length::FillPortion(1)),
        hairline(),
        label(tr!("All notes")),
        container(notes_list(app)).height(Length::FillPortion(1)),
    ]
    .into()
}

/// Every reading note in the project, file by file.
fn notes_list(app: &App) -> Element<'_, Message> {
    if app.proj.notes.is_empty() {
        return empty_state(Glyph::Note, tr!("No reading notes yet"), "", None);
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for n in &app.proj.notes {
        if last_rel != Some(n.rel.as_str()) {
            last_rel = Some(n.rel.as_str());
            rows.push(group_header(&n.rel));
        }
        let line = app.note_symbol_line(&n.rel, &n.symbol);
        // Leading understood toggle.
        let (cg, gcolor) = if n.understood {
            (Glyph::CheckCircle, theme::accent())
        } else {
            (Glyph::Circle, theme::dim())
        };
        let toggle = button(glyph::icon(cg, gcolor, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::Reading(ReadingMsg::NoteToggleUnderstood {
                rel: n.rel.clone(),
                symbol: n.symbol.clone(),
            }));

        // Symbol name + its live location (or a "detached" flag when orphaned).
        let loc: Element<'_, Message> = match line {
            Some(l) => text(format!("L{l}"))
                .size(ts::CAPTION)
                .color(theme::dim())
                .into(),
            None => text(tr!("detached"))
                .size(ts::CAPTION)
                .color(theme::warning())
                .into(),
        };
        let head = row![
            text(&n.symbol)
                .size(ts::BODY)
                .color(if n.understood {
                    theme::dim()
                } else {
                    theme::fg()
                })
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
                container(
                    text(&n.text)
                        .size(ts::CAPTION)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::Word)
                )
                .padding(Padding {
                    top: 0.0,
                    right: 4.0,
                    bottom: 0.0,
                    left: 0.0
                }),
            ]
            .spacing(1)
            .width(Fill)
            .into()
        };

        let note_color = if n.text.is_empty() {
            theme::dim()
        } else {
            theme::accent()
        };
        let pencil = button(glyph::icon(Glyph::Edit, note_color, 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::Reading(ReadingMsg::NoteEditStart {
                rel: n.rel.clone(),
                symbol: n.symbol.clone(),
            }));
        let close = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(Message::Reading(ReadingMsg::NoteRemove {
                rel: n.rel.clone(),
                symbol: n.symbol.clone(),
            }));
        // The name area jumps; the toggle/pencil/✕ capture their own clicks.
        let jump = button(main)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 2.0,
                right: 4.0,
                bottom: 2.0,
                left: 4.0,
            })
            .on_press(Message::Reading(ReadingMsg::NoteJump {
                rel: n.rel.clone(),
                symbol: n.symbol.clone(),
            }));
        rows.push(
            row![toggle, jump, pencil, close]
                .spacing(1)
                .align_y(iced::Center)
                .into(),
        );
    }

    column![
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill)
    ]
    .padding(Padding {
        top: 6.0,
        right: 0.0,
        bottom: 0.0,
        left: 0.0,
    })
    .into()
}

/// A small filled dot marking a node of the reading trail.
fn trail_dot<'a>(color: iced::Color) -> Element<'a, Message> {
    container(space())
        .width(6)
        .height(6)
        .style(move |_: &iced::Theme| container::Style {
            background: Some(color.into()),
            border: iced::Border {
                radius: 3.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// Short badge for an LSP SymbolKind number.
pub(crate) fn kind_short(kind: u8) -> &'static str {
    match kind {
        12 | 6 | 9 => "fn", // Function / Method / Constructor
        5 | 23 => "type",   // Class / Struct
        11 | 10 => "trait", // Interface / Enum
        2 => "mod",         // Module
        _ => "",
    }
}

/// The "Ask" (semantic search) tab: a natural-language query over the embedding
/// index, with a build/refresh control and ranked results that jump to the code.
pub(crate) fn semantic_tab(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let n = app.proj.embed_index.entries.len();

    let input = text_input(tr!("Search by meaning…"), &app.proj.semantic_query)
        .style(theme::input)
        .on_input(|v| Message::Semantic(SemanticMsg::QueryChanged(v)))
        .on_submit(Message::Semantic(SemanticMsg::Search))
        .size(ts::BASE)
        .padding(7);

    let build_label = if app.proj.building_embeddings {
        tr!("Building…")
    } else if n == 0 {
        tr!("Build index")
    } else {
        tr!("Rebuild")
    };
    let mut build = button(text(build_label).size(ts::SMALL))
        .style(theme::toolbar_button)
        .padding([2, 8]);
    if !app.proj.building_embeddings {
        build = build.on_press(Message::Semantic(SemanticMsg::BuildIndex));
    }
    // The index builds itself from explanation summaries (automatically, after
    // Explain All) — so the hint only reports state, never asks for a manual step.
    let info = text(if app.proj.building_embeddings {
        tr!("Building the index…").to_string()
    } else if n > 0 {
        trn!("1 indexed", "{n} indexed", n)
    } else if app.proj.explain.cache.is_empty() {
        tr!("Run Explain All to enable semantic search.").to_string()
    } else if !app.embed_available {
        crate::app::embedding::why_no_embeddings()
    } else {
        tr!("Preparing the index…").to_string()
    })
    .size(ts::CAPTION)
    .color(theme::dim());

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        row![build, space().width(Fill), info]
            .align_y(iced::Center)
            .into(),
    );
    if app.proj.searching_semantic {
        rows.push(
            text(tr!("Searching…"))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
        );
    }
    for (node, score) in &app.proj.semantic_results {
        let label = match node {
            Node::Function { file, name, .. } => format!("{name} · {}", rel_of(app, file)),
            Node::File(p) => rel_of(app, p),
            Node::Folder(p) => rel_of(app, p),
        };
        let sum = app
            .proj
            .explain
            .cache
            .get(node)
            .map(crate::app::shown_summary)
            .unwrap_or_default();
        let short: String = sum.chars().take(96).collect();
        rows.push(
            button(
                column![
                    row![
                        text(label)
                            .size(ts::BODY)
                            .color(theme::accent())
                            .wrapping(Wrapping::None),
                        space().width(Fill),
                        text(tr!(
                            "{percent}%",
                            percent = crate::i18n::decimal(f64::from(*score) * 100.0, 0)
                        ))
                        .size(ts::CAPTION)
                        .color(theme::dim()),
                    ]
                    .align_y(iced::Center),
                    text(short).size(ts::CAPTION).color(theme::dim()),
                ]
                .spacing(1),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 6])
            .on_press(Message::Semantic(SemanticMsg::OpenNode(node.clone())))
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
        Node::File(p) | Node::Folder(p) => p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    };
    let pct = if score > 0.0 {
        format!("  {}%", (score * 100.0).round() as i32)
    } else {
        String::new()
    };
    button(
        text(format!("{label}{pct}"))
            .size(ts::CAPTION)
            .color(theme::dim()),
    )
    .style(theme::toolbar_button)
    .padding([1, 6])
    .on_press(Message::Semantic(SemanticMsg::OpenNode(node.clone())))
    .into()
}
