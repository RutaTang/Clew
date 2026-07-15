//! All view code: toolbar, sidebar (files / search), code pane, outline,
//! status bar and the fuzzy-finder modal.

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::{LineHeight, Wrapping};
use iced::widget::{
    Column, button, center, column, container, mouse_area, opaque, rich_text, row, scrollable,
    space, span, stack, text, text_input,
};
use iced::{Element, Fill, Font, Padding, Pixels};

use crate::fs_scan::DirNode;
use crate::highlight::style_color;
use crate::viewer::{FONT_SIZE, LINE_HEIGHT, Viewer};
use crate::{App, Message, SidebarTab, theme};

pub fn code_scroll_id() -> iced::widget::Id {
    iced::widget::Id::new("code-view")
}

pub fn finder_input_id() -> iced::widget::Id {
    iced::widget::Id::new("finder-input")
}

pub fn search_input_id() -> iced::widget::Id {
    iced::widget::Id::new("search-input")
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = row![sidebar(app), code_area(app)];
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

    let path_label: Element<'_, Message> = match &app.viewer {
        Some(v) => text(&v.rel).size(13).into(),
        None => text("").into(),
    };

    let bar = row![
        nav("←", app.history.can_back(), Message::GoBack),
        nav("→", app.history.can_forward(), Message::GoForward),
        path_label,
        space().width(Fill),
        text("⌘P files    ⌘⇧F search").size(11).color(theme::DIM),
        button(text("Open Folder…").size(12))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(Message::OpenFolderPressed),
        button(text("Outline").size(12))
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(Message::ToggleOutline),
    ]
    .spacing(8)
    .align_y(iced::Center)
    .padding([6, 10]);

    container(bar).width(Fill).style(theme::panel).into()
}

// ---------------------------------------------------------------- sidebar

fn sidebar(app: &App) -> Element<'_, Message> {
    let tab = |label: &'static str, this: SidebarTab| {
        button(text(label).size(12))
            .style(theme::tab_button(app.sidebar == this))
            .width(Fill)
            .padding([5, 0])
            .on_press(Message::SidebarTabPicked(this))
    };
    let tabs = row![
        tab("FILES", SidebarTab::Files),
        tab("SEARCH", SidebarTab::Search)
    ];

    let content: Element<'_, Message> = match app.sidebar {
        SidebarTab::Files => files_tab(app),
        SidebarTab::Search => search_tab(app),
    };

    container(column![tabs, content])
        .width(280)
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
        let is_current = app.viewer.as_ref().is_some_and(|v| v.rel == rel);
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
            rows.push(
                container(
                    text(&hit.rel)
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
                .into(),
            );
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

// ---------------------------------------------------------------- code pane

fn code_area(app: &App) -> Element<'_, Message> {
    let inner: Element<'_, Message> = if app.scanning {
        center(text("Scanning project…").color(theme::DIM)).into()
    } else if app.project.is_none() {
        welcome()
    } else if let Some(v) = &app.viewer {
        code_pane(v)
    } else {
        center(
            text("Pick a file from the tree, or press ⌘P")
                .size(14)
                .color(theme::DIM),
        )
        .into()
    };

    container(inner)
        .width(Fill)
        .height(Fill)
        .style(theme::editor)
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

fn code_pane(v: &Viewer) -> Element<'_, Message> {
    let (first, last) = v.visible_range();
    let total = v.lines.len();
    let top_pad = first as f32 * LINE_HEIGHT;
    let bottom_pad = (total - last) as f32 * LINE_HEIGHT;

    let mut children: Vec<Element<'_, Message>> = Vec::with_capacity(last - first + 2);
    children.push(space().height(top_pad).into());
    for i in first..last {
        children.push(code_line(v, i));
    }
    children.push(space().height(bottom_pad).into());

    scrollable(Column::with_children(children))
        .id(code_scroll_id())
        .on_scroll(Message::CodeScrolled)
        .direction(Direction::Both {
            vertical: Scrollbar::default(),
            horizontal: Scrollbar::default(),
        })
        .width(Fill)
        .height(Fill)
        .into()
}

fn code_line(v: &Viewer, i: usize) -> Element<'_, Message> {
    let line = &v.lines[i];
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(span(format!("{:>5}  ", i + 1)).color(theme::DIM));
    for (fragment, style) in &line.spans {
        let mut s = span(fragment.as_str());
        if let Some(color) = style.and_then(style_color) {
            s = s.color(color);
        }
        spans.push(s);
    }

    let rich = rich_text::<(), Message, iced::Theme, iced::Renderer>(spans)
        .size(FONT_SIZE)
        .line_height(LineHeight::Absolute(Pixels(LINE_HEIGHT)))
        .font(Font::MONOSPACE)
        .wrapping(Wrapping::None);

    if v.target_line == Some(i + 1) {
        container(rich).style(theme::target_line).into()
    } else {
        rich.into()
    }
}

// ---------------------------------------------------------------- outline

fn outline_pane(app: &App) -> Option<Element<'_, Message>> {
    if !app.show_outline || app.outline.is_empty() {
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
    for symbol in &app.outline {
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
    let right = match &app.viewer {
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
    let input = text_input("Type to search files by name…", &app.finder.query)
        .id(finder_input_id())
        .on_input(Message::FinderQueryChanged)
        .on_submit(Message::FinderConfirm)
        .size(14)
        .padding(10);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if let Some(project) = &app.project {
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
    if rows.is_empty() {
        rows.push(
            container(text("No matching files").size(12).color(theme::DIM))
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
