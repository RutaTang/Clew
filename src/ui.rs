//! All view code: toolbar, sidebar (files / search / marks), split code
//! panes, outline, status bar and the finder modal (files / symbols / :N).

use iced::widget::scrollable::{Direction, Scrollbar};
use iced::widget::text::Wrapping;
use iced::widget::{
    Column, button, center, column, container, mouse_area, opaque, row, scrollable, space, stack,
    text, text_input,
};
use iced::{Element, Fill, Font, Padding};

use crate::codeview::CodeView;
use crate::finder::FinderMode;
use crate::fs_scan::DirNode;
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

pub fn find_input_id() -> iced::widget::Id {
    iced::widget::Id::new("find-input")
}

pub fn view(app: &App) -> Element<'_, Message> {
    let mut main = row![sidebar(app), pane_area(app)];
    if let Some(outline) = outline_pane(app) {
        main = main.push(outline);
    }
    let base: Element<'_, Message> =
        column![toolbar(app), main.height(Fill), statusbar(app)].into();

    if let Some(root) = &app.pending_consent {
        stack![base, consent_modal(root)].into()
    } else if let Some(consent) = &app.pending_lsp_consent {
        stack![base, lsp_consent_modal(consent)].into()
    } else if app.server_panel {
        stack![base, server_panel_modal(app)].into()
    } else if app.finder.open {
        stack![base, finder_modal(app)].into()
    } else if let Some(menu) = &app.context_menu {
        stack![base, context_menu(menu)].into()
    } else if let Some(h) = app.hover.as_ref().filter(|h| h.text.is_some()) {
        stack![base, hover_tooltip(h)].into()
    } else {
        base
    }
}

// ---------------------------------------------------------------- hover tooltip

fn hover_tooltip(h: &crate::HoverState) -> Element<'_, Message> {
    let text_content = h.text.clone().unwrap_or_default();
    // Trim overly long hover text.
    let shown: String = if text_content.chars().count() > 1200 {
        text_content.chars().take(1200).collect::<String>() + "…"
    } else {
        text_content
    };

    let panel = container(
        scrollable(
            text(shown)
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG),
        )
        .height(iced::Length::Shrink),
    )
    .max_width(560)
    .max_height(320)
    .padding(8)
    .style(theme::modal_panel);

    // Position below-right of the hovered point.
    container(panel)
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top: h.y + 18.0,
            left: h.x,
            right: 0.0,
            bottom: 0.0,
        })
        .into()
}

// ---------------------------------------------------------------- context menu

fn context_menu(menu: &crate::ContextMenu) -> Element<'_, Message> {
    use crate::GotoKind;

    let item = |kind: GotoKind| {
        button(text(kind.label()).size(13))
            .style(theme::list_row(false))
            .width(Fill)
            .padding([5, 12])
            .on_press(Message::ContextGoto(kind))
    };

    let panel = container(
        column![
            item(GotoKind::Definition),
            item(GotoKind::References),
            item(GotoKind::Implementation),
            item(GotoKind::TypeDefinition),
        ]
        .spacing(1),
    )
    .width(210)
    .padding(4)
    .style(theme::modal_panel);

    // Place the menu's top-left at the click point via padding.
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .padding(Padding {
            top: menu.y,
            left: menu.x,
            right: 0.0,
            bottom: 0.0,
        });

    // A full-size backdrop closes the menu on any outside click.
    opaque(mouse_area(positioned).on_press(Message::ContextMenuClosed))
}

// ---------------------------------------------------------------- server panel

fn server_panel_modal(app: &App) -> Element<'_, Message> {
    use crate::LspSlot;

    // Languages relevant to this project (present in it, or installed/running).
    let languages = app.managed_languages();

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    rows.push(section_header("SERVERS FOR THIS PROJECT"));
    if languages.is_empty() {
        rows.push(
            container(
                text("No supported languages detected in this project.")
                    .size(11)
                    .color(theme::DIM),
            )
            .padding([2, 8])
            .into(),
        );
    }
    for lang in &languages {
        let (status, action) = app.lsp_row(lang);
        let server_name = crate::lsp::registry::default_for_language(lang)
            .map(|s| s.name.to_string())
            .unwrap_or_else(|| "custom".into());

        let action_el: Element<'_, Message> = match action {
            Some((label, msg)) => button(text(label).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(msg)
                .into(),
            None => space().width(0).into(),
        };

        rows.push(
            row![
                text(lang.clone()).size(12).width(70),
                text(server_name).size(12).color(theme::ACCENT).width(140),
                text(status).size(11).color(theme::DIM).width(Fill),
                action_el,
            ]
            .spacing(8)
            .align_y(iced::Center)
            .padding([3, 8])
            .into(),
        );
    }

    rows.push(section_header("INSTALLED (global, shared across projects)"));
    if app.installed_servers.is_empty() {
        rows.push(
            container(text("Nothing downloaded yet.").size(11).color(theme::DIM))
                .padding([2, 8])
                .into(),
        );
    }
    for srv in &app.installed_servers {
        rows.push(
            row![
                text(&srv.name).size(12).width(150),
                text(&srv.version).size(11).color(theme::DIM).width(120),
                text(human_size(srv.bytes)).size(11).color(theme::DIM).width(Fill),
                button(text("Remove").size(11))
                    .style(theme::toolbar_button)
                    .padding([2, 8])
                    .on_press(Message::LspRemove {
                        name: srv.name.clone(),
                        version: srv.version.clone(),
                    }),
            ]
            .spacing(8)
            .align_y(iced::Center)
            .padding([3, 8])
            .into(),
        );
    }

    // Log of the active file's language server, if it is running.
    let logs = app
        .active_viewer()
        .and_then(|v| v.lang_key)
        .and_then(|l| app.lsp.get(l))
        .and_then(|s| match s {
            LspSlot::Ready(c) => Some(c.logs()),
            _ => None,
        })
        .unwrap_or_default();
    rows.push(section_header("SERVER LOG"));
    let log_lines: Vec<Element<'_, Message>> = logs
        .iter()
        .rev()
        .take(200)
        .map(|line| {
            text(line.clone())
                .size(11)
                .font(Font::MONOSPACE)
                .color(theme::DIM)
                .wrapping(Wrapping::None)
                .into()
        })
        .collect();
    let log_view = if log_lines.is_empty() {
        container(text("No output.").size(11).color(theme::DIM)).padding([2, 8])
    } else {
        container(scrollable(Column::with_children(log_lines).spacing(1)).height(160)).padding([2, 8])
    };
    rows.push(log_view.into());

    let panel = container(
        column![
            row![
                text("Language Servers").size(17).color(theme::FG),
                space().width(Fill),
                button(text("Close").size(12))
                    .style(theme::toolbar_button)
                    .padding([3, 12])
                    .on_press(Message::ToggleServerPanel),
            ]
            .align_y(iced::Center),
            scrollable(Column::with_children(rows).spacing(2).width(Fill)).height(iced::Length::Fill),
        ]
        .spacing(12),
    )
    .width(720)
    .max_height(600)
    .padding(20)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .padding(40)
        .style(theme::backdrop);

    opaque(mouse_area(positioned).on_press(Message::ToggleServerPanel))
}

fn section_header(label: &str) -> Element<'_, Message> {
    container(text(label.to_string()).size(11).color(theme::DIM))
        .padding(Padding {
            top: 10.0,
            right: 8.0,
            bottom: 2.0,
            left: 8.0,
        })
        .into()
}

fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------- LSP consent

fn lsp_consent_modal(consent: &crate::LspConsent) -> Element<'_, Message> {
    use crate::LspProvision;
    let is_install = matches!(consent.provision, LspProvision::Install(_));
    let (title, action) = if is_install {
        ("Install a language server?", "Install")
    } else {
        ("Download a language server?", "Download")
    };

    let panel = container(
        column![
            text(title).size(17).color(theme::FG),
            text(
                "clew manages its own “go to definition” server for this \
                 language, separate from anything on your system:",
            )
            .size(13)
            .color(theme::FG),
            container(
                text(format!("{} {}", consent.server_name, consent.version))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::None),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text(consent.describe()).size(12).color(theme::DIM).wrapping(Wrapping::None),
            row![
                space().width(Fill),
                button(text("Not now").size(13))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::LspConsentDismissed),
                button(text(action).size(13))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::LspConsentAllowed),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(560)
    .padding(22)
    .style(theme::modal_panel);

    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .style(theme::backdrop);

    opaque(positioned)
}

// ---------------------------------------------------------------- consent modal

fn consent_modal(root: &std::path::Path) -> Element<'_, Message> {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    let panel = container(
        column![
            text("Allow clew to use this project?").size(17).color(theme::FG),
            text(
                "clew stores bookmarks and reading data in a “.clew” folder \
                 inside the project:",
            )
            .size(13)
            .color(theme::FG),
            container(
                text(format!("{}/.clew", root.display()))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::None),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text("Without it the project can't be opened. You can delete .clew any time.")
                .size(12)
                .color(theme::DIM),
            row![
                space().width(Fill),
                button(text("Not now").size(13))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::ConsentDenied),
                button(text(format!("Allow in {name}")).size(13))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::ConsentAllowed),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(560)
    .padding(22)
    .style(theme::modal_panel);

    // A plain backdrop; clicking it does nothing (the choice is required).
    let positioned = container(opaque(panel))
        .width(Fill)
        .height(Fill)
        .align_x(iced::Center)
        .align_y(iced::Center)
        .style(theme::backdrop);

    opaque(positioned)
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
        .push(tool("Servers", Message::ToggleServerPanel))
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
    use crate::SearchOpt;

    let input = text_input("Search in project…", &app.search.query)
        .id(search_input_id())
        .on_input(Message::SearchQueryChanged)
        .on_submit(Message::SearchSubmitted)
        .size(13)
        .padding(7);

    // Match-option chips: case-sensitive, whole-word, regex.
    let chip = |label: &'static str, active: bool, opt: SearchOpt| -> Element<'_, Message> {
        button(text(label).size(12).font(Font::MONOSPACE))
            .style(theme::tab_button(active))
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
    let include = text_input("files to include (e.g. *.rs)", &app.search.include)
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

fn find_bar(app: &App) -> Element<'_, Message> {
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
    // Bookmarked lines of this file, for the gutter marker.
    let marked: std::collections::HashSet<usize> = app
        .bookmarks
        .iter()
        .filter(|b| b.rel == v.rel)
        .map(|b| b.line)
        .collect();

    // The block cursor shows only on the active pane while the code view has
    // keyboard focus.
    let cursor = if pane == app.active && app.code_focused {
        v.caret
    } else {
        None
    };

    let code = CodeView::new(
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
    .folds(v.visible_rows(), &v.fold_header_set, &v.collapsed)
    .on_fold(move |line| Message::FoldToggle { pane, line })
    .indent_guides(true)
    .on_minimap(move |fraction| Message::MinimapScrolled { pane, fraction })
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
    });

    scrollable(code)
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
            // 1-based line/column of the last click, when there is one.
            let pos = v
                .caret
                .map(|(l, c)| format!("Ln {}, Col {}  ·  ", l + 1, c + 1))
                .unwrap_or_default();
            // Language-server status for this file's language, when relevant.
            let lsp = v
                .lang_key
                .and_then(|k| app.lsp.get(k))
                .map(|slot| format!("  ·  LSP {}", slot.label()))
                .unwrap_or_default();
            // Diagnostic counts for this file.
            let diags = v
                .lang_key
                .and_then(|k| match app.lsp.get(k) {
                    Some(crate::LspSlot::Ready(c)) => Some(c.diagnostics(&v.abs)),
                    _ => None,
                })
                .map(|ds| {
                    let errs = ds.iter().filter(|d| d.severity == 1).count();
                    let warns = ds.iter().filter(|d| d.severity == 2).count();
                    if errs + warns == 0 {
                        String::new()
                    } else {
                        format!("  ·  ✘ {errs}  ⚠ {warns}")
                    }
                })
                .unwrap_or_default();
            format!("{}{}  ·  {} lines{}{}", pos, lang, v.lines.len(), diags, lsp)
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
