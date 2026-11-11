//! Left sidebar and its tabs (files/search/trail/marks/notes/semantic/walk).

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// The sidebar tabs in strip order — the one source of truth for both rendering
/// the strip and computing which one to scroll into view (`reveal_sidebar_tab`).
pub(crate) const SIDEBAR_TABS: [(&str, SidebarTab); 12] = [
    ("FILES", SidebarTab::Files),
    ("SEARCH", SidebarTab::Search),
    ("FIND", SidebarTab::Semantic),
    ("WALK", SidebarTab::Walk),
    ("MARKS", SidebarTab::Marks),
    ("TRAIL", SidebarTab::Trail),
    ("HISTORY", SidebarTab::History),
    ("CALLS", SidebarTab::Calls),
    ("FLOW", SidebarTab::Flow),
    ("IMPORTS", SidebarTab::Imports),
    ("NOTES", SidebarTab::Notes),
    ("DOCS", SidebarTab::Docs),
];

/// How many of [`SIDEBAR_TABS`] stand on the strip itself: the reader's
/// everyday views, walkthroughs among them. The rest open from what shows
/// them — a function's calls, tracing a value, the import graph, a symbol's
/// docs — from the overflow, and from the command palette, each under a
/// title of its own.
pub(crate) const STRIP_VIEWS: usize = 6;

/// The width of the ⋯ after the strip's views, which lists the rest.
pub(crate) const MORE_VIEWS_W: f32 = 28.0;

/// Where the strip's views stand: their top edge below the top bar, and
/// their row's right edge (the ⋯'s) from the sidebar's left — what the
/// More views menu hangs from.
pub(crate) const VIEWS_ROW_TOP: f32 = 2.0;
pub(crate) const VIEWS_ROW_RIGHT_PAD: f32 = 10.0 + 2.0;
pub(crate) const VIEWS_ROW_H: f32 = 28.0 + 2.0 * 2.0;

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
        SidebarTab::History => Glyph::Branch,
    }
}

/// The command that opens a view, when one does: its shortcut is said in
/// the view's tooltip.
pub(crate) fn tab_action(tab: SidebarTab) -> Option<crate::keymap::Action> {
    match tab {
        SidebarTab::Search => Some(crate::keymap::Action::ProjectSearch),
        SidebarTab::Walk => Some(crate::keymap::Action::Walkthrough),
        _ => None,
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
        SidebarTab::Calls => tr!("Call hierarchy"),
        SidebarTab::Flow => tr!("Value flow"),
        SidebarTab::Imports => tr!("Imports"),
        SidebarTab::Walk => tr!("Walkthroughs"),
        SidebarTab::Notes => tr!("Your notes"),
        SidebarTab::Docs => tr!("API docs"),
        SidebarTab::History => tr!("History"),
    }
}

pub(crate) fn sidebar(app: &App) -> Element<'_, Message> {
    // The view's rows a list voice counts down: "open the third one".
    super::heard::list();
    // The views as a segmented row of icons, each naming itself on hover;
    // the chosen one raised as a chip.
    let segment =
        |glyph: Glyph, chosen: bool, name: &'static str, keys: Option<String>, msg: Message| {
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
                .on_press(super::heard::said(name, msg)),
                name,
                keys,
                tooltip::Position::Bottom,
            )
        };
    // A view not on the strip, opened by what shows it or from the ⋯.
    let elsewhere = SIDEBAR_TABS
        .iter()
        .skip(STRIP_VIEWS)
        .any(|(_, t)| *t == app.sidebar);
    let mut tabs_row = Row::new().spacing(2);
    for (_, this) in SIDEBAR_TABS.iter().take(STRIP_VIEWS) {
        tabs_row = tabs_row.push(segment(
            tab_glyph(*this),
            app.sidebar == *this,
            tab_name(*this),
            tab_action(*this).map(|action| app.keymap.chord(action).caps()),
            Message::Window(WindowMsg::SidebarTabPicked(*this)),
        ));
    }
    // The views not on the strip, from a menu: raised while one of them
    // shows, or the menu is open.
    let more = tip_at(
        button(
            container(glyph::icon(
                Glyph::More,
                if elsewhere || app.show_views_menu {
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
        .style(theme::segment(elsewhere || app.show_views_menu))
        .padding(0)
        .width(Length::Fixed(MORE_VIEWS_W))
        .height(Length::Fixed(28.0))
        .on_press(super::heard::said(
            tr!("More views"),
            Message::Window(WindowMsg::ToggleViewsMenu),
        )),
        tr!("More views"),
        None,
        tooltip::Position::Bottom,
    );
    let tabs = container(tabs_row.push(more))
        .padding(2)
        .style(theme::trough);
    // A view not on the strip, opened by what shows it, is named over it,
    // with the way back to the files.
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
                        .on_press(super::heard::said(
                            tr!("Close"),
                            Message::Window(WindowMsg::SidebarTabPicked(SidebarTab::Files,))
                        )),
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
            filter_field(
                tr!("Filter files"),
                &app.file_filter,
                Some(super::file_filter_id()),
                |s| Message::Window(WindowMsg::FileFilterChanged(s)),
                Some(Message::Window(WindowMsg::FileFilterSubmitted)),
                Message::Window(WindowMsg::FileFilterEscaped),
                // ↓ goes down to the tree under it — when it shows a row.
                tree_has_rows(app).then_some(Message::Window(WindowMsg::FileFilterDown)),
            )
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
        SidebarTab::History => super::history::history_tab(app),
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
    // Where to start reading: the guide, opened when there is one, else
    // written.
    let guide = crate::walkthrough::GUIDE_SCOPE;
    let has_guide = app.proj.walk.library.iter().any(|w| w.scope == guide);
    let guide_msg = if has_guide {
        Message::Walk(WalkMsg::Open(guide.to_string()))
    } else {
        Message::Walk(WalkMsg::Generate(guide.to_string()))
    };
    let mut actions = column![
        walk_action(
            Glyph::Compass,
            tr!("Where to start reading").to_string(),
            None,
            tr!("The AI writes a tour of where to start, what to read next, and where to end"),
            (app.proj.walk.generating.as_deref() != Some(guide)).then_some(guide_msg),
        ),
        walk_action(
            Glyph::Diff,
            tr!("Review branch changes").to_string(),
            None,
            tr!("The AI writes a tour of this branch's changes"),
            Some(Message::Walk(WalkMsg::GenerateDiff)),
        ),
    ]
    .spacing(4);
    // The last debug run, once it stopped somewhere: a tour of the path it
    // actually took.
    if !app.debug.trace.is_empty() {
        actions = actions.push(walk_action(
            Glyph::Play,
            trn!(
                "Walk the last run (1 stop)",
                "Walk the last run ({n} stops)",
                app.debug.trace.len()
            )
            .to_string(),
            Some(tr!("Walk the last run").to_string()),
            tr!("The AI writes a tour of the path the last run took"),
            Some(Message::Walk(WalkMsg::GenerateTrace)),
        ));
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
    } else if scope == crate::walkthrough::GUIDE_SCOPE {
        tr!("Reading guide").to_string()
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
    // flips mode). On a line of its own — beside the field it left the field
    // too narrow to say what it is for — and across it, in halves, as tall
    // as a field: two small chips at its start sat cramped against its edge
    // with the line's room empty beside them.
    let seg = |label: &str, active: bool| {
        button(
            container(text(label.to_string()).size(ts::BODY))
                .width(Fill)
                .align_x(iced::Center),
        )
        .style(theme::segment(active))
        .padding([5, 0])
        .width(Fill)
        .on_press_maybe(
            (!active).then(|| super::heard::said(label, Message::Walk(WalkMsg::ToggleMode))),
        )
    };
    let toggle =
        container(row![seg(tr!("Search"), is_search), seg(tr!("New"), !is_search)].spacing(2))
            .padding(2)
            .width(Fill)
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
                .on_press(super::heard::said(
                    tr!("Generate"),
                    Message::Walk(WalkMsg::Generate(app.walk_ui.input.clone())),
                )),
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
        .on_press(super::heard::row(
            || scope_label(&wt.scope),
            if is_open {
                Message::Walk(WalkMsg::Back)
            } else {
                Message::Walk(WalkMsg::Open(wt.scope.clone()))
            },
        ));
        let tour_row: Element<'_, Message> = if busy {
            title.into()
        } else {
            let controls = container(
                row![
                    button(text("↻").size(ts::BASE))
                        .style(theme::toolbar_button)
                        .padding([6, 9])
                        .on_press(super::heard::said_to(
                            tr!("Regenerate"),
                            &scope_label(&wt.scope),
                            Message::Walk(WalkMsg::Regenerate(wt.scope.clone()))
                        )),
                    button(glyph::icon(Glyph::Close, theme::fg_muted(), 12.0))
                        .style(theme::toolbar_button)
                        .padding([7, 9])
                        .on_press(super::heard::said_to(
                            tr!("Delete"),
                            &scope_label(&wt.scope),
                            Message::Walk(WalkMsg::Delete(wt.scope.clone()))
                        )),
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
            // A reading guide is a tree: each step under the one it is read
            // from, by its depth, with a branch mark where a tour numbers it.
            let tree = crate::walkthrough::is_guide(wt);
            for (si, step) in wt.steps.iter().enumerate() {
                let is_cur = cur == Some(si);
                let (mark, indent) = if tree {
                    let mark = if step.depth == 0 {
                        "\u{25C9}"
                    } else {
                        "\u{2514}"
                    };
                    (mark.to_string(), 14.0 * step.depth as f32)
                } else {
                    (format!("{}", si + 1), 0.0)
                };
                list = list.push(
                    button(
                        row![
                            text(mark).size(ts::CAPTION).color(theme::dim()).width(18),
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
                        left: 22.0 + indent,
                    })
                    .on_press(super::heard::row(
                        || step.title.clone(),
                        Message::Walk(WalkMsg::Goto {
                            scope: wt.scope.clone(),
                            step: si,
                        }),
                    )),
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
            .on_press(super::heard::said(
                tr!("Previous step"),
                Message::Walk(WalkMsg::Step(-1))
            )),
        // As wide as it reads at the last step: the ‹ before it moved as
        // the count gained a digit, under the pointer stepping through.
        {
            let said = format!("{}/{}", cur + 1, n);
            super::steady::slot(
                text(said.clone()).size(ts::SMALL).color(theme::dim()),
                &said,
                &[&format!("{n}/{n}")],
                ts::SMALL,
            )
        },
        button(text("›").size(ts::EMPHASIS))
            .style(theme::toolbar_button)
            .padding([1, 8])
            .on_press(super::heard::said(
                tr!("Next step"),
                Message::Walk(WalkMsg::Step(1))
            )),
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
    // The row the keyboard rests on, drawn so while the tree has it.
    let cursor = app.tree_cursor_rel();
    let cursor = cursor.as_deref();
    if filter.is_empty() {
        append_tree_rows(&mut rows, &project.tree, "", 0, app, cursor);
    } else {
        filtered_file_rows(&mut rows, project, filter, app, cursor);
        if rows.is_empty() {
            return empty_state(Glyph::Filter, tr!("No matching files"), "", None);
        }
    }
    let tree = scrollable(
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
    .id(crate::ui::files_tree_id())
    .on_scroll(|v| {
        Message::Window(WindowMsg::FilesTreeScrolled {
            offset: v.absolute_offset().y,
            height: v.bounds().height,
        })
    })
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);
    // Something over the tree holding the keys takes them from it.
    let held_above =
        super::active_overlay(app).is_some_and(|o| o.holds_keys()) || app.tutorial.is_some();
    super::tree_keys::TreeKeys::new(tree, super::files_tree_keys_id())
        .release(app.code_focused || held_above)
        .into()
}

/// What the working tree changes in the file at `rel`, as git letters it —
/// `M`odified, `A`dded, `U`ntracked, `R`enamed, `D`eleted, `!` in conflict —
/// in a colour of its own, the letter saying it as well as the colour.
pub(crate) fn file_change_mark<'a>(app: &App, rel: &str) -> Element<'a, Message> {
    match app.proj.git_status.get(rel) {
        Some(&letter) => text(letter.to_string())
            .size(ts::SMALL)
            .font(crate::fonts::CODE)
            .color(change_color(letter))
            .into(),
        None => space().width(0).into(),
    }
}

/// A folder holding a change: a dot, as editors mark one.
fn dir_change_mark<'a>(app: &App, rel: &str) -> Element<'a, Message> {
    let inside = format!("{rel}/");
    if app
        .proj
        .git_status
        .keys()
        .any(|changed| changed.starts_with(&inside))
    {
        text("●")
            .size(ts::CAPTION)
            .color(theme::with_alpha(theme::warning(), 0.8))
            .into()
    } else {
        space().width(0).into()
    }
}

/// The colour of a change's letter.
fn change_color(letter: char) -> iced::Color {
    match letter {
        'A' | 'U' => theme::success(),
        'D' | '!' => theme::danger(),
        'R' => theme::info(),
        _ => theme::warning(),
    }
}

/// A row of the Files view as the keyboard walks it: its path, and whether
/// it is a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeRow {
    pub rel: String,
    pub dir: bool,
}

/// The rows the Files view shows, in its order: the tree with its open
/// folders' rows (as [`append_tree_rows`] lays them out), or, filtered, the
/// files the filter lists.
pub(crate) fn tree_rows(app: &App) -> Vec<TreeRow> {
    let Some(project) = &app.proj.project else {
        return Vec::new();
    };
    let filter = app.file_filter.trim();
    if !filter.is_empty() {
        return filtered_files(project, filter)
            .map(|f| TreeRow {
                rel: f.rel.clone(),
                dir: false,
            })
            .collect();
    }
    fn walk(
        node: &DirNode,
        prefix: &str,
        expanded: &std::collections::HashSet<String>,
        rows: &mut Vec<TreeRow>,
    ) {
        for (name, child) in &node.dirs {
            let rel = join_rel(prefix, name);
            let open = expanded.contains(&rel);
            rows.push(TreeRow {
                rel: rel.clone(),
                dir: true,
            });
            if open {
                walk(child, &rel, expanded, rows);
            }
        }
        for name in &node.files {
            rows.push(TreeRow {
                rel: join_rel(prefix, name),
                dir: false,
            });
        }
    }
    let mut rows = Vec::new();
    walk(&project.tree, "", &app.proj.expanded, &mut rows);
    rows
}

/// Whether the Files view shows any row for the keyboard to rest on: a
/// file the filter lists, or, unfiltered, anything in the project.
pub(crate) fn tree_has_rows(app: &App) -> bool {
    let Some(project) = &app.proj.project else {
        return false;
    };
    let filter = app.file_filter.trim();
    if filter.is_empty() {
        !project.tree.dirs.is_empty() || !project.tree.files.is_empty()
    } else {
        filtered_files(project, filter).next().is_some()
    }
}

/// The row `rel` is drawn at in the Files view's tree with `expanded`
/// folders open — counted as [`append_tree_rows`] lays them out, each
/// folder's own row, then its rows when open, then its files; `None` while a
/// closed folder holds it.
pub(crate) fn tree_row_index(
    tree: &DirNode,
    expanded: &std::collections::HashSet<String>,
    rel: &str,
) -> Option<usize> {
    fn walk(
        node: &DirNode,
        prefix: &str,
        expanded: &std::collections::HashSet<String>,
        rel: &str,
        row: &mut usize,
    ) -> Option<usize> {
        for (name, child) in &node.dirs {
            let dir = join_rel(prefix, name);
            *row += 1;
            if expanded.contains(&dir)
                && let Some(found) = walk(child, &dir, expanded, rel, row)
            {
                return Some(found);
            }
        }
        for name in &node.files {
            if join_rel(prefix, name) == rel {
                return Some(*row);
            }
            *row += 1;
        }
        None
    }
    walk(tree, "", expanded, rel, &mut 0)
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
    cursor: Option<&str>,
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
                    space().width(Fill),
                    file_change_mark(app, &f.rel),
                ]
                .spacing(6)
                .align_y(iced::Center),
            )
            .style(theme::tree_row(is_current, cursor == Some(f.rel.as_str())))
            .width(Fill)
            .height(Length::Fixed(TREE_ROW_H))
            .padding([0, 8])
            .on_press(super::heard::row(
                || f.rel.clone(),
                Message::Editor(EditorMsg::OpenRel {
                    rel: f.rel.clone(),
                    line: None,
                }),
            ))
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
pub(crate) const TREE_ROW_H: f32 = 28.0;
pub(crate) const TREE_ROW_GAP: f32 = 1.0;
/// Height of the Files view's filter field.
const FILTER_H: f32 = 30.0;
/// The room between the sidebar's views, its filter and its list.
const SIDEBAR_GAP: f32 = 12.0;

/// The file-tree mark of a file git tracks although `.gitignore` (or a
/// build-directory name) would hide it (see `fs_scan::ScanReport`).
pub(crate) const TRACKED_IGNORED_MARK: &str = tr_noop!("tracked, though ignored");

pub(crate) fn append_tree_rows<'a>(
    rows: &mut Vec<Element<'a, Message>>,
    node: &'a DirNode,
    prefix: &str,
    depth: u16,
    app: &'a App,
    cursor: Option<&str>,
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
            space().width(Fill),
            dir_change_mark(app, &rel),
        ]
        .spacing(6)
        .height(Fill)
        .align_y(iced::Center);
        rows.push(
            button(content)
                .style(theme::tree_row(false, cursor == Some(rel.as_str())))
                .width(Fill)
                .height(Length::Fixed(TREE_ROW_H))
                .padding(pad)
                .on_press(super::heard::row(
                    || name.to_string(),
                    Message::Project(ProjectMsg::ToggleDir(rel.clone())),
                ))
                .into(),
        );
        if expanded {
            append_tree_rows(rows, child, &rel, depth + 1, app, cursor);
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
            space().width(Fill),
            file_change_mark(app, &rel),
        ]
        .spacing(6)
        .height(Fill)
        .align_y(iced::Center);
        let style = theme::tree_row(is_current, cursor == Some(rel.as_str()));
        rows.push(
            button(content)
                .style(style)
                .width(Fill)
                .height(Length::Fixed(TREE_ROW_H))
                .padding(pad)
                .on_press(super::heard::row(
                    || name.to_string(),
                    Message::Editor(EditorMsg::OpenRel { rel, line: None }),
                ))
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
    empty_state_saying(g, title, subtitle, action.map(|(l, m)| (l.to_string(), m)))
}

/// [`empty_state`] whose action's label is made as it is drawn ("Show for
/// total()").
pub(crate) fn empty_state_saying<'a>(
    g: Glyph,
    title: &'a str,
    subtitle: &'a str,
    action: Option<(String, Message)>,
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
            button(text(label.clone()).size(ts::BASE))
                .style(theme::toolbar_button)
                .padding([7, 16])
                .on_press(super::heard::said(&label, msg)),
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

/// The side of a match-option toggle beside the search field.
const OPTION_SIDE: f32 = 30.0;

pub(crate) fn search_tab(app: &App) -> Element<'_, Message> {
    use crate::SearchOpt;

    let input = text_input(tr!("Search text…"), &app.proj.search.query)
        .style(theme::input)
        .id(search_input_id())
        .on_input(|v| Message::Nav(NavMsg::SearchQueryChanged(v)))
        .on_submit(Message::Nav(NavMsg::SearchSubmitted))
        .size(ts::BASE)
        .padding(7);

    // Match-option toggles: case-sensitive, whole-word, regex — as round as
    // the fields, outlined while off, filled in the accent while on. Beside
    // the field they qualify, each a square of one size, its mark centred:
    // on a row of their own under it, each as wide as its mark, they read
    // as three odd chips apart from the search.
    let chip = |label: &'static str, tip: &'static str, active: bool, opt: SearchOpt| {
        super::toolbar::tip_at(
            button(container(text(label).size(ts::BODY).font(crate::fonts::CODE)).center(Fill))
                .style(theme::toggle(active))
                .padding(0)
                .width(Length::Fixed(OPTION_SIDE))
                .height(Length::Fixed(OPTION_SIDE))
                .on_press(super::heard::said(
                    tip,
                    Message::Nav(NavMsg::SearchToggle(opt)),
                )),
            tip,
            None,
            tooltip::Position::Bottom,
        )
    };
    let options = row![
        chip(
            "Aa",
            tr!("Match case"),
            app.proj.search.case_sensitive,
            SearchOpt::Case
        ),
        chip(
            "W",
            tr!("Whole word"),
            app.proj.search.whole_word,
            SearchOpt::WholeWord
        ),
        chip(
            ".*",
            tr!("Regular expression"),
            app.proj.search.regex,
            SearchOpt::Regex
        ),
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
        Some((msg, theme::dim()))
    } else if app.proj.search.query.trim().chars().count() == 1 {
        // One character, which matches most of a project, is searched for
        // only when asked (see `search_when_typing_pauses`).
        Some((tr!("Keep typing to search").to_string(), theme::dim()))
    } else {
        None
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    // What a preview has beside its line number, inside the view's and the
    // row's padding and clear of the scrollbar: a longer line is cut there
    // with an ellipsis, not clipped mid-letter at the panel's edge.
    let preview_room =
        (app.sidebar_width - 2.0 * 8.0 - (8.0 + 6.0) - (36.0 + 4.0) - crate::ui::SCROLLBAR_W - 2.0)
            .max(0.0);
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
                    search_preview(app, i, &hit.preview, preview_room),
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
            .on_press(super::heard::row(
                || format!("{} {}", hit.rel, hit.line),
                Message::Editor(EditorMsg::OpenAbs {
                    abs: hit.abs.clone(),
                    line: Some(hit.line),
                    push: true,
                }),
            ))
            .into(),
        );
    }

    // A language server's list (the references to a name, its several
    // definitions) is a list of its own, headed by what it lists, with a way
    // back to the search: it took the search's place under the search's
    // own empty fields, "(references)" the only word of what it was.
    let mut col = match &app.proj.search.listing {
        Some(list) => column![
            row![
                text(list.clone())
                    .size(ts::BASE)
                    .color(theme::fg())
                    .wrapping(Wrapping::WordOrGlyph)
                    .width(Fill),
                super::chrome::close_button(Message::Nav(NavMsg::ListingClosed)),
            ]
            .spacing(6)
            .align_y(iced::Center)
        ],
        None => column![
            row![input, options].spacing(6).align_y(iced::Center),
            include,
            exclude
        ],
    }
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
fn search_preview<'a>(app: &'a App, i: usize, preview: &'a str, room: f32) -> Element<'a, Message> {
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
    let pieces = fit_pieces(mark_spans(spans, &marks), &shown, room);
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

/// `pieces` of `shown` as they fit in `room`: whole, or as many of their
/// characters as `steady::cut` keeps, an ellipsis after them — each piece
/// keeping its colour and its mark.
pub(crate) fn fit_pieces(
    pieces: Vec<(String, Option<u8>, bool)>,
    shown: &str,
    room: f32,
) -> Vec<(String, Option<u8>, bool)> {
    // One look-up a row once cut: the cut is kept (`steady::cut`).
    let cut = super::steady::cut(shown, room, ts::BODY, crate::fonts::UI);
    if cut == shown {
        return pieces;
    }
    let mut keep = cut.chars().count().saturating_sub(1);
    let mut out = Vec::new();
    for (piece, style, marked) in pieces {
        if keep == 0 {
            break;
        }
        let n = piece.chars().count();
        if n <= keep {
            keep -= n;
            out.push((piece, style, marked));
        } else {
            out.push((piece.chars().take(keep).collect(), style, marked));
            keep = 0;
        }
    }
    out.push(("…".to_string(), None, false));
    out
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
            .on_press(super::heard::said(
                tr!("Clear the trail"),
                Message::Reading(ReadingMsg::HistoryClear)
            )),
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
        // Indented under each fork above it — a hop that just followed the
        // one before stays in line — and capped: past ~8 branches a deep one
        // would push the node off the panel's right edge.
        let indent = (v.indent.min(8) as f32) * 14.0;
        // What is left for its name and file after the row's indent, chevron,
        // mark and icon: cut with an ellipsis, where they were cut bare.
        let room = (app.sidebar_width
            - indent
            - CHEVRON_W
            - 10.0
            - 12.0
            - 3.0 * 6.0
            - 14.0
            - crate::ui::SCROLLBAR_W)
            .max(0.0);

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
        let name_font = if v.is_current {
            crate::fonts::UI_MEDIUM
        } else {
            crate::fonts::UI
        };
        let jump = button(
            row![
                space().width(indent),
                toggle,
                marker,
                container(glyph::icon(Glyph::File, theme::dim(), 12.0)).width(12),
                container(column![
                    text(super::steady::elide_end(
                        &loc_label(&v.loc, v.label.as_deref()),
                        ts::BODY,
                        name_font,
                        room,
                    ))
                    .size(ts::BODY)
                    .font(name_font)
                    .color(theme::fg())
                    .wrapping(Wrapping::None),
                    text(super::steady::elide_start(
                        &rel_of(app, &v.loc.path),
                        ts::CAPTION,
                        crate::fonts::UI,
                        room,
                    ))
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
        .on_press(super::heard::row(
            || loc_label(&v.loc, v.label.as_deref()),
            Message::Reading(ReadingMsg::HistoryJump {
                id: v.id,
                loc: v.loc.clone(),
            }),
        ));

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
            tr!("The lines you bookmark are listed here."),
            None,
        );
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    // What a preview has beside its line number and the row's pencil and ✕:
    // cut to it with an ellipsis — cut at 48 characters, a narrower panel
    // clipped it mid-letter.
    let preview_room = (app.sidebar_width
        - 8.0
        - 4.0
        - 36.0
        - 4.0
        - 2.0 * 2.0
        - 2.0 * (13.0 + 12.0)
        - crate::ui::SCROLLBAR_W
        - 4.0)
        .max(0.0);
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
                text(super::steady::cut(
                    &crate::highlight::line_in_order(&bm.preview),
                    preview_room,
                    ts::BODY,
                    crate::fonts::UI,
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
            .on_press(super::heard::said_to_with(
                tr!("Edit note"),
                || format!("{} {}", bm.rel, bm.line),
                Message::Reading(ReadingMsg::BookmarkNoteEdit(bm.rel.clone(), bm.line)),
            ));
        let close = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(super::heard::said_to_with(
                tr!("Remove"),
                || format!("{} {}", bm.rel, bm.line),
                Message::Reading(ReadingMsg::BookmarkRemoved {
                    rel: bm.rel.clone(),
                    line: bm.line,
                }),
            ));
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
                .on_press(super::heard::row(
                    || format!("{} {}", bm.rel, bm.line),
                    Message::Editor(EditorMsg::OpenRel {
                        rel: bm.rel.clone(),
                        line: Some(bm.line),
                    }),
                ))
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
        return column![
            row![space().width(Fill), notes_export(app)],
            notes_list(app)
        ]
        .into();
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
        row![
            label(tr!("All notes")),
            space().width(Fill),
            notes_export(app)
        ]
        .align_y(iced::Bottom),
        container(notes_list(app)).height(Length::FillPortion(1)),
    ]
    .into()
}

/// The notes exported, as one Markdown file — here, where they are: none
/// yet, nothing to export.
fn notes_export(app: &App) -> Element<'_, Message> {
    if app.proj.notes.is_empty() {
        return space().into();
    }
    container(
        button(
            text(tr!("Export…"))
                .size(ts::SMALL)
                .color(theme::fg_muted()),
        )
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press(super::heard::said(
            tr!("Export notes"),
            Message::Export(ExportMsg::Start),
        )),
    )
    .padding(Padding {
        right: 8.0,
        ..Padding::ZERO
    })
    .into()
}

/// Every reading note in the project, file by file.
fn notes_list(app: &App) -> Element<'_, Message> {
    if app.proj.notes.is_empty() {
        return empty_state(Glyph::Note, tr!("No notes of your own yet"), "", None);
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
            .on_press(super::heard::said_to(
                tr!("Understood"),
                &n.symbol,
                Message::Reading(ReadingMsg::NoteToggleUnderstood {
                    rel: n.rel.clone(),
                    symbol: n.symbol.clone(),
                }),
            ));

        // Symbol name + its live location (or a "detached" flag when orphaned).
        let loc: Element<'_, Message> = match line {
            Some(l) => text(format!("L{l}"))
                .size(ts::CAPTION)
                .color(theme::dim())
                .into(),
            None => text(tr!("symbol not found"))
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
            .on_press(super::heard::said_to(
                tr!("Edit note"),
                &n.symbol,
                Message::Reading(ReadingMsg::NoteEditStart {
                    rel: n.rel.clone(),
                    symbol: n.symbol.clone(),
                }),
            ));
        let close = button(glyph::icon(Glyph::Close, theme::dim(), 13.0))
            .style(theme::list_row(false))
            .padding([2, 6])
            .on_press(super::heard::said_to(
                tr!("Remove"),
                &n.symbol,
                Message::Reading(ReadingMsg::NoteRemove {
                    rel: n.rel.clone(),
                    symbol: n.symbol.clone(),
                }),
            ));
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
            .on_press(super::heard::row(
                || n.symbol.clone(),
                Message::Reading(ReadingMsg::NoteJump {
                    rel: n.rel.clone(),
                    symbol: n.symbol.clone(),
                }),
            ));
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

    let building = app.proj.building_embeddings;
    // It is built from the notes Explain All writes, by an embedding model:
    // with neither, there is nothing it can do, and it is drawn so — it
    // looked ready, and answered in the status bar.
    let can_build = !app.proj.explain.cache.is_empty() && app.embed_available;
    let press = (!building && can_build).then_some(Message::Semantic(SemanticMsg::BuildIndex));
    let can_press = press.is_some();
    // While it builds, the button's own icon turns, in its place: words
    // that came in for the build ("Building…", "Building the index…") moved
    // what stood beside them, and the count went for as long as it ran.
    let icon = |size: f32| -> Element<'_, Message> {
        if building {
            super::meter::spinning(crate::glyph::Glyph::Refresh, theme::accent(), size)
        } else {
            // Dimmed with its words when there is nothing it can do: its
            // icon at full strength, it looked ready.
            let tint = if can_press {
                theme::fg_muted()
            } else {
                theme::dim()
            };
            crate::glyph::icon(crate::glyph::Glyph::Refresh, tint, size)
        }
    };
    // The first build is asked for in words; building again, as the other
    // rebuilds are, by its icon.
    let build: Element<'_, Message> = if n == 0 {
        button(
            row![icon(13.0), text(tr!("Build index")).size(ts::SMALL)]
                .spacing(6)
                .align_y(iced::Center),
        )
        .style(theme::toolbar_button)
        .padding([2, 8])
        .on_press_maybe(press.map(|m| super::heard::said(tr!("Build index"), m)))
        .into()
    } else {
        super::toolbar::busy_action(
            crate::glyph::Glyph::Refresh,
            tr!("Rebuild"),
            Some(Message::Semantic(SemanticMsg::BuildIndex)),
            building,
        )
    };
    // The index builds itself from explanation summaries (automatically, after
    // Explain All) — so the hint only reports state, never asks for a manual step.
    let info: Element<'_, Message> = if !app.embed_available && n == 0 {
        // No model to search with, and no index: where one is set up, as a
        // link — what stood here was Explain All, which needs one as much.
        // An index built before keeps its count here.
        button(
            text(tr!("Set Up…"))
                .size(ts::CAPTION)
                .color(theme::accent()),
        )
        .style(theme::icon_button(false))
        .padding([2, 6])
        .on_press(super::heard::said(
            tr!("Set Up…"),
            Message::Settings(crate::SettingsMsg::OpenAt(crate::SettingsPage::Embeddings)),
        ))
        .into()
    } else if n == 0 && app.proj.explain.cache.is_empty() {
        // The step that gets there, as a link.
        button(
            text(tr!("Explain All…"))
                .size(ts::CAPTION)
                .color(theme::accent()),
        )
        .style(theme::icon_button(false))
        .padding([2, 6])
        .on_press(super::heard::said(
            tr!("Explain All…"),
            super::toolbar::explain_all_message(app),
        ))
        .into()
    } else {
        text(if n > 0 {
            trn!("1 indexed", "{n} indexed", n)
        } else if !app.embed_available {
            app.embed_missing.clone()
        } else {
            tr!("Preparing the index…").to_string()
        })
        .size(ts::CAPTION)
        .color(theme::dim())
        .into()
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        row![build, space().width(Fill), info]
            .align_y(iced::Center)
            .into(),
    );
    // And why the panel stays empty, said in it: a search pressed showed
    // nothing, and said why only for a moment in the status bar.
    if !app.embed_available && !app.embed_missing.is_empty() {
        rows.push(
            text(app.embed_missing.clone())
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
        );
    }
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
            Node::Function { file, name, .. } => {
                tr!("{name} in {file}", name = name, file = rel_of(app, file))
            }
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
                        text(label.clone())
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
            .on_press(super::heard::row(
                || label.clone(),
                Message::Semantic(SemanticMsg::OpenNode(node.clone())),
            ))
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
/// A chip naming what grounded an answer, opening it. Its name only: a
/// score beside it ("87%") read as a grade of the answer.
pub(crate) fn source_chip<'a>(node: &crate::explain::Node) -> Element<'a, Message> {
    use crate::explain::Node;
    let label = match node {
        Node::Function { name, .. } => name.clone(),
        Node::File(p) | Node::Folder(p) => p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string(),
    };
    button(text(label).size(ts::CAPTION).color(theme::dim()))
        .style(theme::toolbar_button)
        .padding([1, 6])
        .on_press(Message::Semantic(SemanticMsg::OpenNode(node.clone())))
        .into()
}

/// A quick action of the Walkthroughs tab: its icon and words (`spoken`,
/// when a voice says it otherwise), and on the pointer what it does — each
/// has the AI write a tour, which a plain button did not say.
fn walk_action<'a>(
    glyph: Glyph,
    label: String,
    spoken: Option<String>,
    does: impl Into<String>,
    press: Option<Message>,
) -> Element<'a, Message> {
    let press = press.map(|msg| super::heard::said(spoken.as_deref().unwrap_or(&label), msg));
    let face = row![
        crate::glyph::icon(glyph, theme::fg_muted(), 13.0),
        text(label).size(ts::SMALL),
    ]
    .spacing(6)
    .align_y(iced::Center);
    let control = button(face)
        .style(theme::toolbar_button)
        .padding([4, 10])
        .width(Fill)
        .on_press_maybe(press);
    let does: String = does.into();
    super::toolbar::tip_at(control, does, None, iced::widget::tooltip::Position::Bottom)
}

/// A sidebar view's filter: the filter mark, the field, and — once there is
/// something in it — a × that empties it; Escape in it does too
/// (`escaped`). The Files view's and the API docs': the docs' was a plain
/// field of another style.
pub(crate) fn filter_field<'a>(
    placeholder: &'a str,
    value: &'a str,
    id: Option<iced::widget::Id>,
    on_input: fn(String) -> Message,
    on_submit: Option<Message>,
    escaped: Message,
    down: Option<Message>,
) -> Element<'a, Message> {
    let mut input = text_input(placeholder, value)
        .on_input(on_input)
        .style(theme::bare_input);
    if let Some(id) = id {
        input = input.id(id);
    }
    if let Some(submit) = on_submit {
        input = input.on_submit(submit);
    }
    let mut field = row![
        glyph::icon(Glyph::Filter, theme::dim(), 13.0),
        ts::field_in(input, FILTER_H - 2.0, ts::INTER, ts::BODY),
    ]
    .spacing(8)
    .height(Fill)
    .align_y(iced::Center);
    if !value.is_empty() {
        field = field.push(tip_at(
            button(glyph::icon(Glyph::Close, theme::dim(), 12.0))
                .style(theme::toolbar_button)
                .padding(3)
                .on_press(super::heard::said(
                    tr!("Clear filter"),
                    on_input(String::new()),
                )),
            tr!("Clear filter"),
            None,
            tooltip::Position::Bottom,
        ));
    }
    container(
        super::escape_clears::EscapeClears::new(
            container(field)
                .height(Length::Fixed(FILTER_H))
                // The design's 10 inside its 1 px border, which here
                // takes no room of its own.
                .padding([1, 11])
                .style(theme::field(!value.is_empty())),
            escaped,
        )
        .ringed(9.0)
        .on_down(down),
    )
    .padding(Padding {
        top: 0.0,
        right: 10.0,
        bottom: SIDEBAR_GAP,
        left: 10.0,
    })
    .into()
}
