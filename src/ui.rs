//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::{LineHeight, Wrapping};
use iced::widget::{
    Column, button, center, column, container, mouse_area, opaque, rich_text, row, scrollable,
    space, span, stack, text, text_input,
};
use iced::{Element, Fill, Font, Padding, Pixels};

use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
use crate::highlight::style_color;
use crate::viewer::Viewer;
use crate::{App, Message, SidebarTab, theme};

pub fn code_scroll_id(pane: usize) -> iced::widget::Id {
    iced::widget::Id::new(if pane == 0 { "code-view-0" } else { "code-view-1" })
}

pub fn finder_input_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-input")
}

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = row![sidebar(app), pane_area(app)];
    if let Some(outline) = outline_pane(app) {
        main = main.push(outline);
    }
    let base: Element<'_, Message> =
        column![toolbar(app), main.height(Fill), statusbar(app)].into();

    if app.finder.open {
        stack![base, finder_modal(app)].into()
    } else {
        base
    }
}

// ---------------------------------------------------------------- toolbar

fn toolbar(app: &App) -> Element<'_, Message> {
    let nav = |label: &'static str, enabled: bool, msg: Message| {
        let mut b = button(text(label).size(14))
            .style(theme::toolbar_button)
            .padding([2, 9]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let tool = |label: &'static str, msg: Message| {
        button(text(label).size(12))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(msg)
    };

    let path_label: Element<'_, Message> = match app.active_viewer() {
        Some(v) => text(&v.rel).size(13).into(),
        None => text("").into(),
    };

    let mut bar = row![
        nav("←", app.history.can_back(), Message::GoBack),
        nav("→", app.history.can_forward(), Message::GoForward),
        path_label,
        space().width(Fill),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .padding([6, 10]);

    if app.window_width >= 1000.0 {
        bar = bar.push(
            text("⌘P files · ⌘T symbols · ⌘⇧F search · ⌘D mark")
                .size(11)
                .color(theme::DIM),
        );
    }
    bar = bar
        .push(tool("Open Folder…", Message::OpenFolderPressed))
        .push(tool("Split", Message::ToggleSplit))
        .push(tool("Outline", Message::ToggleOutline));

    container(bar).width(Fill).style(theme::panel).into()
}

// ---------------------------------------------------------------- sidebar

fn sidebar(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: SidebarTab| {
        button(text(label).size(11))
            .style(theme::tab_button(app.sidebar == this))
            .width(Fill)
            .padding([5, 0])
            .on_press(Message::SidebarTabPicked(this))
    };
    let tabs = row![
        tab("FILES", SidebarTab::Files),
        tab("SEARCH", SidebarTab::Search),
        tab("MARKS", SidebarTab::Marks),
    ];

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
        SidebarTab::Marks => marks_tab(app),
    };

    // Narrow windows get a slimmer sidebar so the code pane keeps room.
    let width = if app.window_width < 700.0 {
        180.0
    } else if app.window_width < 1000.0 {
        220.0
    } else {
        280.0
    };
    container(column![tabs, content])
        .width(width)
        .height(Fill)
        .style(theme::panel)
        .into()
}

fn files_tab(app: &App) -> Element<'_, Message> {
    let Some(project) = &app.project else {
        let hint = if app.scanning {
            "Scanning…"
        } else {
            "No folder open"
        };
        return container(text(hint).size(12).color(theme::DIM))
            .padding(12)
            .into();
    };

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    append_tree_rows(&mut rows, &project.tree, "", 0, app);
    scrollable(Column::with_children(rows).width(Fill))
        .height(Fill)
        .into()
}

fn append_tree_rows<'a>(
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
        let arrow = if expanded { "▾ " } else { "▸ " };
        rows.push(
            button(
                text(format!("{arrow}{name}"))
                    .size(13)
                    .wrapping(Wrapping::None),
            )
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
        rows.push(
            button(
                text(format!("  {name}"))
                    .size(13)
                    .wrapping(Wrapping::None),
            )
            .style(theme::list_row(is_current))
            .width(Fill)
            .padding(pad)
            .on_press(Message::OpenRel { rel, line: None })
            .into(),
        );
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn search_tab(app: &App) -> Element<'_, Message> {
    let input = text_input("Search in project…", &app.search.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchSubmitted)
        .size(13)
        .padding(7);

    let status_line = if app.search.running {
        Some("Searching…".to_string())
    } else if app.search.ran {
        let n = app.search.hits.len();
        Some(if n >= crate::search::MAX_HITS {
            format!("{n}+ matches (capped)")
        } else {
            format!("{n} matches")
        })
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

    let mut col = column![input].spacing(6).padding(8);
    if let Some(status) = status_line {
        col = col.push(text(status).size(11).color(theme::DIM));
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
        .into()
}

fn marks_tab(app: &App) -> Element<'_, Message> {
    if app.bookmarks.is_empty() {
        return container(
            text("No bookmarks yet.\n⌘D marks the current line.")
                .size(12)
                .color(theme::DIM),
        )
        .padding(12)
        .into();
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    let mut last_rel: Option<&str> = None;
    for (idx, bm) in app.bookmarks.iter().enumerate() {
        if last_rel != Some(bm.rel.as_str()) {
            last_rel = Some(bm.rel.as_str());
            rows.push(group_header(&bm.rel));
        }
        rows.push(
            row![
                button(
                    row![
                        text(bm.line.to_string()).size(11).color(theme::DIM).width(36),
                        text(&bm.preview).size(12).wrapping(Wrapping::None),
                    ]
                    .spacing(4),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding(Padding {
                    top: 1.0,
                    right: 2.0,
                    bottom: 1.0,
                    left: 8.0,
                })
                .on_press(Message::OpenRel {
                    rel: bm.rel.clone(),
                    line: Some(bm.line),
                }),
                button(text("✕").size(10).color(theme::DIM))
                    .style(theme::list_row(false))
                    .padding([3, 6])
                    .on_press(Message::BookmarkRemoved(idx)),
            ]
            .align_y(iced::Center)
            .into(),
        );
    }

    column![scrollable(Column::with_children(rows).width(Fill)).height(Fill)]
        .padding(Padding {
            top: 6.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .into()
}

fn group_header(rel: &str) -> Element<'_, Message> {
    container(
        text(rel)
            .size(11)
            .color(theme::ACCENT)
            .wrapping(Wrapping::None),
    )
    .padding(Padding {
        top: 8.0,
        right: 6.0,
        bottom: 2.0,
        left: 8.0,
    })
    .into()
}

// ---------------------------------------------------------------- code panes

fn pane_area(app: &App) -> Element<'_, Message> {
    if app.scanning {
        return editor_shell(center(text("Scanning project…").color(theme::DIM)).into());
    }
    if app.project.is_none() {
        return editor_shell(welcome());
    }
    if !app.split {
        return pane_view(app, 0);
    }
    row![pane_view(app, 0), pane_view(app, 1)]
        .spacing(1)
        .into()
}

fn editor_shell(inner: Element<'_, Message>) -> Element<'_, Message> {
    container(inner)
        .width(Fill)
        .height(Fill)
        .style(theme::editor)
        .into()
}

fn pane_view(app: &App, pane: usize) -> Element<'_, Message> {
    let inner: Element<'_, Message> = match &app.panes[pane] {
        Some(v) => code_pane(app, pane, v),
        None => mouse_area(center(
            text("Pick a file from the tree, or press ⌘P")
                .size(14)
                .color(theme::DIM),
        ))
        .on_press(Message::PaneFocused(pane))
        .into(),
    };

    let mut col = column![];
    if app.split {
        col = col.push(pane_header(app, pane));
    }
    col.push(editor_shell(inner)).width(Fill).height(Fill).into()
}

fn pane_header(app: &App, pane: usize) -> Element<'_, Message> {
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

fn welcome() -> Element<'static, Message> {
    center(
        column![
            text("clew").size(46).color(theme::ACCENT),
            text("a reader for code").size(15).color(theme::DIM),
            space().height(14),
            button(text("Open Folder…").size(14))
                .style(theme::toolbar_button)
                .padding([8, 18])
                .on_press(Message::OpenFolderPressed),
            space().height(6),
            text("tip: `clew <path>` opens a project directly")
                .size(12)
                .color(theme::DIM),
        ]
        .spacing(6)
        .align_x(iced::Center),
    )
    .into()
}

fn code_pane<'a>(app: &'a App, pane: usize, v: &'a Viewer) -> Element<'a, Message> {
    let line_height = app.line_height();
    let (first, last) = v.visible_range(line_height);
    let total = v.lines.len();
    let top_pad = first as f32 * line_height;
    let bottom_pad = (total - last) as f32 * line_height;

    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    let mut children: Vec<Element<'a, Message>> = Vec::with_capacity(last - first + 2);
    children.push(space().height(top_pad).into());
    for i in first..last {
        children.push(code_line(app, pane, v, i, marked.contains(&(i + 1))));
    }
    children.push(space().height(bottom_pad).into());

    scrollable(Column::with_children(children))
        .id(code_scroll_id(pane))
        .on_scroll(move |viewport| Message::CodeScrolled(pane, viewport))
        .direction(Direction::Both {
            vertical: Scrollbar::default(),
            horizontal: Scrollbar::default(),
        })
        .width(Fill)
        .height(Fill)
        .into()
}

fn code_line<'a>(
    app: &'a App,
    pane: usize,
    v: &'a Viewer,
    i: usize,
    bookmarked: bool,
) -> Element<'a, Message> {
    let line = &v.lines[i];
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    let number = span(format!("{:>5}  ", i + 1)).color(if bookmarked {
        theme::ACCENT
    } else {
        theme::DIM
    });
    spans.push(number);
    for (fragment, style) in &line.spans {
        let mut s = span(fragment.as_str());
        if let Some(color) = style.and_then(style_color) {
            s = s.color(color);
        }
        spans.push(s);
    }

    let rich = rich_text::<(), Message, iced::Theme, iced::Renderer>(spans)
        .size(app.font_size)
        .line_height(LineHeight::Absolute(Pixels(app.line_height())))
        .font(Font::MONOSPACE)
        .wrapping(Wrapping::None);

    let selected = v
        .selection_bounds()
        .is_some_and(|(a, b)| i >= a && i <= b);
    let row_el: Element<'a, Message> = if selected {
        container(rich).style(theme::selected_line).into()
    } else if v.target_line == Some(i + 1) {
        container(rich).style(theme::target_line).into()
    } else {
        rich.into()
    };

    mouse_area(row_el)
        .on_press(Message::SelectStart { pane, line: i })
        .on_enter(Message::SelectDrag { pane, line: i })
        .into()
}

// ---------------------------------------------------------------- outline

fn outline_pane(app: &App) -> Option<Element<'_, Message>> {
    // Hide the outline on narrow windows or when split: code panes first.
    if !app.show_outline || app.split || app.window_width < 950.0 {
        return None;
    }
    let symbols = &app.active_viewer()?.symbols;
    if symbols.is_empty() {
        return None;
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(
        container(text("OUTLINE").size(11).color(theme::DIM))
            .padding(Padding {
                top: 8.0,
                right: 8.0,
                bottom: 4.0,
                left: 10.0,
            })
            .into(),
    );
    for symbol in symbols {
        rows.push(
            button(
                row![
                    text(short_kind(&symbol.kind))
                        .size(10)
                        .color(kind_color(&symbol.kind))
                        .width(40),
                    text(&symbol.name).size(12).wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 2.0,
                right: 6.0,
                bottom: 2.0,
                left: 10.0,
            })
            .on_press(Message::OutlineJump(symbol.line))
            .into(),
        );
    }

    Some(
        container(scrollable(Column::with_children(rows).width(Fill)).height(Fill))
            .width(230)
            .height(Fill)
            .style(theme::panel)
            .into(),
    )
}

fn short_kind(kind: &str) -> &'static str {
    match kind {
        "function" => "fn",
        "method" => "meth",
        "class" => "class",
        "struct" => "struct",
        "enum" => "enum",
        "union" => "union",
        "trait" => "trait",
        "interface" => "iface",
        "implementation" => "impl",
        "module" => "mod",
        "macro" => "macro",
        "constant" => "const",
        "type" => "type",
        _ => "sym",
    }
}

fn kind_color(kind: &str) -> iced::Color {
    match kind {
        "function" | "method" | "macro" => theme::rgb(0x61afef),
        "class" | "struct" | "enum" | "union" | "trait" | "interface" | "type" => {
            theme::rgb(0xe5c07b)
        }
        "module" | "implementation" => theme::rgb(0xc678dd),
        "constant" => theme::rgb(0xd19a66),
        _ => theme::DIM,
    }
}

// ---------------------------------------------------------------- status bar

fn statusbar(app: &App) -> Element<'_, Message> {
    let right = match app.active_viewer() {
        Some(v) => {
            let lang = v
                .lang_key
                .and_then(crate::highlight::lang_name)
                .unwrap_or("Plain text");
            format!("{}  ·  {} lines", lang, v.lines.len())
        }
        None => String::new(),
    };

    container(
        row![
            text(&app.status).size(11),
            space().width(Fill),
            text(right).size(11),
        ]
        .padding([3, 10]),
    )
    .width(Fill)
    .style(theme::statusbar)
    .into()
}

// ---------------------------------------------------------------- finder modal

fn finder_modal(app: &App) -> Element<'_, Message> {
    let placeholder = match app.finder.mode {
        FinderMode::Files => "File name…  (:123 jumps to a line)",
        FinderMode::Symbols => "Symbol name…",
    };
    let input = text_input(placeholder, &app.finder.query)
        .id(finder_input_id())
        .on_input(Message::FinderQueryChanged)
        .on_submit(Message::FinderConfirm)
        .size(14)
        .padding(10);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if let Some(n) = app.finder.goto_line() {
        rows.push(
            container(
                text(format!("↵  Go to line {n}"))
                    .size(13)
                    .color(theme::ACCENT),
            )
            .padding(8)
            .into(),
        );
    } else {
        match app.finder.mode {
            FinderMode::Files => finder_file_rows(app, &mut rows),
            FinderMode::Symbols => finder_symbol_rows(app, &mut rows),
        }
    }
    if rows.is_empty() {
        let hint = if app.finder.mode == FinderMode::Symbols && app.indexing {
            "Indexing symbols…"
        } else {
            "No matches"
        };
        rows.push(
            container(text(hint).size(12).color(theme::DIM))
                .padding(8)
                .into(),
        );
    }

    let panel = container(
        column![
            input,
            scrollable(Column::with_children(rows).width(Fill)).height(iced::Length::Shrink),
        ]
        .spacing(8),
    )
    .width(640)
    .max_height(520)
    .padding(10)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .padding(Padding {
            top: 80.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
        })
        .style(theme::backdrop);

    opaque(mouse_area(positioned).on_press(Message::FinderClosed))
}

fn finder_file_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    let Some(project) = &app.project else {
        return;
    };
    for (pos, &idx) in app.finder.results.iter().enumerate() {
        let Some(entry) = project.files.get(idx) else {
            continue;
        };
        let (dir, name) = match entry.rel.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", entry.rel.as_str()),
        };
        rows.push(
            button(
                row![
                    text(name).size(13),
                    text(dir).size(11).color(theme::DIM).wrapping(Wrapping::None),
                ]
                .spacing(10)
                .align_y(iced::Center),
            )
            .style(theme::list_row(pos == app.finder.selected))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::FinderPick(idx))
            .into(),
        );
    }
}

fn finder_symbol_rows<'a>(app: &'a App, rows: &mut Vec<Element<'a, Message>>) {
    for (pos, &idx) in app.finder.results.iter().enumerate() {
        let Some(entry) = app.symbol_index.get(idx) else {
            continue;
        };
        rows.push(
            button(
                row![
                    text(short_kind(&entry.kind))
                        .size(10)
                        .color(kind_color(&entry.kind))
                        .width(40),
                    text(&entry.name).size(13),
                    text(format!("{}:{}", entry.rel, entry.line))
                        .size(11)
                        .color(theme::DIM)
                        .wrapping(Wrapping::None),
                ]
                .spacing(10)
                .align_y(iced::Center),
            )
            .style(theme::list_row(pos == app.finder.selected))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::FinderPick(idx))
            .into(),
        );
    }
}
