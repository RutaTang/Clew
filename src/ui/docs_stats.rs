//! Docs tab/page, overview home, stats page, and pane_area.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

pub(crate) fn group_header(rel: &str) -> Element<'_, Message> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let (glyph, color) = crate::icons::file_icon(name);
    container(
        row![
            icon_text(glyph, color, 12.0),
            text(rel).size(11).color(theme::FG_MUTED).wrapping(Wrapping::None),
        ]
        .spacing(5)
        .align_y(iced::Center),
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

pub(crate) fn pane_area(app: &App) -> Element<'_, Message> {
    if app.scanning {
        return editor_shell(empty_state(
            Glyph::Search,
            "Scanning project…",
            "Indexing files so you can read and search them.",
            None,
        ));
    }
    if app.project.is_none() {
        return editor_shell(welcome(app));
    }
    if let Some(page) = &app.docs.page {
        return editor_shell(docs_page(app, page));
    }
    if app.show_overview {
        return editor_shell(overview_home(app));
    }
    if app.show_stats {
        return editor_shell(stats_home(app));
    }
    if !app.split {
        return pane_view(app, 0);
    }
    row![pane_view(app, 0), pane_view(app, 1)]
        .spacing(1)
        .into()
}

/// Map a file rel to a module/package label for the "Modules" grouping — a
/// display heuristic per language (Rust `src/lsp/client.rs` -> `lsp::client`,
/// Python `foo/bar.py` -> `foo.bar`, Go by directory/package, etc.). Files that
/// map to the same label are merged into one module group.
pub(crate) fn module_label(rel: &str) -> String {
    let lang = match rel.rsplit('.').next() {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("go") => "go",
        Some("ts") | Some("tsx") => "ts",
        Some("js") | Some("jsx") => "js",
        Some("dart") => "dart",
        _ => "",
    };
    let no_ext = rel.rsplit_once('.').map(|(a, _)| a).unwrap_or(rel);
    let mut segs: Vec<&str> = no_ext.split('/').filter(|s| !s.is_empty()).collect();
    // Drop a conventional source root.
    if segs.len() > 1 && matches!(segs.first().copied(), Some("src") | Some("lib")) {
        segs.remove(0);
    }
    // A file that names its parent module collapses to the directory.
    let is_dir_file = (lang == "rust" && matches!(segs.last().copied(), Some("mod") | Some("lib") | Some("main")))
        || (lang == "python" && segs.last().copied() == Some("__init__"))
        || (matches!(lang, "ts" | "js") && segs.last().copied() == Some("index"));
    if is_dir_file {
        segs.pop();
    }
    // Go's unit is the package = the directory.
    if lang == "go" && !segs.is_empty() {
        segs.pop();
    }
    if segs.is_empty() {
        return "(root)".to_string();
    }
    let sep = match lang {
        "rust" => "::",
        "python" => ".",
        _ => "/",
    };
    segs.join(sep)
}

/// Short badge for a symbol kind, shown before the name in the Docs tree/page.
pub(crate) fn kind_badge(kind: &str) -> &str {
    match kind {
        "function" | "fn" | "func" => "fn",
        "method" => "fn",
        "struct" => "struct",
        "enum" => "enum",
        "trait" | "interface" => "trait",
        "class" => "class",
        "constant" | "const" => "const",
        "module" | "mod" | "namespace" => "mod",
        "type" | "typealias" | "type_alias" => "type",
        "impl" => "impl",
        "property" | "prop" => "prop",
        "field" => "field",
        _ => kind,
    }
}

/// The DOCS sidebar tab: a filterable tree of files → public API items. Clicking
/// an item opens its doc page in the main pane.
pub(crate) fn docs_tab(app: &App) -> Element<'_, Message> {
    if app.docs.files.is_empty() {
        return if app.docs.loading {
            empty_state(Glyph::Sparkle, "Building docs…", "Reading the project's public API.", None)
        } else {
            empty_state(
                Glyph::Note,
                "No documentation",
                "No documented symbols found in this project.",
                Some(("Rebuild", Message::DocsRefresh)),
            )
        };
    }

    // Toolbar: a filter on top, then the grouping / visibility / rebuild
    // controls (two rows so they fit a narrow sidebar).
    let filter = text_input("Filter docs…", &app.docs.filter)
        .on_input(Message::DocsFilterChanged)
        .size(12)
        .padding(6)
        .width(Fill);
    let chip = |label: String, msg: Message| {
        button(text(label).size(11))
            .style(theme::toolbar_button)
            .padding([4, 8])
            .on_press(msg)
    };
    let group_btn = chip(
        if app.docs.by_module { "Modules".into() } else { "Files".into() },
        Message::DocsToggleGrouping,
    );
    let vis_btn = chip(
        if app.docs.show_all { "All".into() } else { "Public".into() },
        Message::DocsToggleShowAll,
    );
    let refresh = chip("↻".into(), Message::DocsRefresh);
    let controls = row![group_btn, vis_btn, space().width(Fill), refresh]
        .spacing(4)
        .align_y(iced::Center);
    let toolbar = column![filter, controls].spacing(4);

    let query = app.docs.filter.trim().to_lowercase();
    let selected_line = app
        .docs
        .page
        .as_ref()
        .and_then(|p| p.entries.first().map(|e| (p.rel.as_str(), e.line)));

    // Group the visible items by file or by module. Each group carries its items
    // as (source rel, item) so selection keeps working across merged files.
    let mut groups: std::collections::BTreeMap<String, Vec<(&str, &clew_protocol::DocItem)>> =
        std::collections::BTreeMap::new();
    for file in &app.docs.files {
        let label = if app.docs.by_module {
            module_label(&file.rel)
        } else {
            file.rel.clone()
        };
        // Match the filter against the symbol name OR the file path / module
        // label, so a path fragment like "http.dart" finds that file's symbols
        // (previously only the symbol name was matched, so paths found nothing).
        let path_matches = query.is_empty()
            || file.rel.to_lowercase().contains(&query)
            || label.to_lowercase().contains(&query);
        for item in &file.items {
            let matches = path_matches || item.name.to_lowercase().contains(&query);
            if (app.docs.show_all || item.public) && matches {
                groups.entry(label.clone()).or_default().push((&file.rel, item));
            }
        }
    }

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for (label, mut items) in groups {
        if items.is_empty() {
            continue;
        }
        // Merged module groups read better alphabetically.
        if app.docs.by_module {
            items.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        }
        let expanded = !query.is_empty() || app.docs.expanded.contains(&label);
        let arrow = if expanded { "▾" } else { "▸" };
        rows.push(
            button(
                row![
                    text(arrow).size(10).color(theme::DIM).width(10),
                    text(label.clone()).size(12).color(theme::FG_MUTED).wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::DocsToggleFile(label.clone()))
            .into(),
        );
        if expanded {
            for (rel, item) in items {
                let is_sel = selected_line == Some((rel, item.line));
                rows.push(
                    button(
                        row![
                            space().width(14),
                            text(kind_badge(&item.kind))
                                .size(10)
                                .color(theme::ACCENT)
                                .font(Font::MONOSPACE)
                                .width(42),
                            text(item.name.clone()).size(13).wrapping(Wrapping::None),
                        ]
                        .spacing(4)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_sel))
                    .width(Fill)
                    .padding([3, 8])
                    .on_press(Message::DocsSelect {
                        rel: rel.to_string(),
                        line: item.line,
                    })
                    .into(),
                );
            }
        }
    }

    column![
        container(toolbar).padding([6, 6]),
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

/// The main-pane doc page: the selected item followed by its members, each with
/// signature and rendered doc comment (like a rustdoc type page).
pub(crate) fn docs_page<'a>(app: &'a App, page: &'a crate::DocPage) -> Element<'a, Message> {
    let _ = app;
    let top_line = page.entries.first().map(|e| e.line);
    let header = row![
        text(page.rel.clone()).size(12).color(theme::DIM).wrapping(Wrapping::None),
        space().width(Fill),
        button(text("Open source").size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::OpenRel {
                rel: page.rel.clone(),
                line: top_line,
            }),
    ]
    .align_y(iced::Center);

    let mut blocks: Vec<Element<'a, Message>> = Vec::new();
    for (idx, e) in page.entries.iter().enumerate() {
        let title_size = if idx == 0 { 22 } else { 15 };
        let title = row![
            text(kind_badge(&e.kind)).size(11).color(theme::ACCENT).font(Font::MONOSPACE),
            text(e.name.clone()).size(title_size).color(theme::FG),
        ]
        .spacing(8)
        .align_y(iced::Center);

        let signature = container(
            text(e.signature.clone())
                .size(12)
                .font(Font::MONOSPACE)
                .color(theme::FG_MUTED),
        )
        .padding([6, 10])
        .width(Fill)
        .style(theme::editor);

        let doc: Element<'a, Message> = if e.doc_items.is_empty() {
            text("No documentation.").size(12).color(theme::DIM).into()
        } else {
            iced::widget::markdown::view(&e.doc_items, iced::Theme::Dark)
                .map(|url| Message::OpenLink(url.to_string()))
        };

        let block = column![title, signature, doc].spacing(8);
        // Indent members under their type.
        let indent = e.depth as f32 * 18.0;
        blocks.push(
            container(block)
                .padding(Padding {
                    top: if idx == 0 { 0.0 } else { 14.0 },
                    right: 0.0,
                    bottom: 0.0,
                    left: indent,
                })
                .width(Fill)
                .into(),
        );
    }

    let body = scrollable(Column::with_children(blocks).spacing(4).width(Fill).padding(Padding {
        top: 6.0,
        right: 20.0,
        bottom: 24.0,
        left: 8.0,
    }))
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .height(Fill);

    container(column![container(header).padding([10, 16]), body])
        .width(Fill)
        .height(Fill)
        .into()
}

/// The architecture-overview "home": the generated overview, a prompt to
/// generate it, or a generation-in-progress note.
pub(crate) fn overview_home(app: &App) -> Element<'_, Message> {
    let regen = |label: &'static str| {
        button(text(label).size(12))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::GenerateOverview)
    };

    if app.generating_overview {
        return center(text("Generating architecture overview…").size(14).color(theme::DIM)).into();
    }

    if app.overview.is_some() {
        let header = row![
            text("Architecture Overview").size(18).color(theme::FG),
            space().width(Fill),
            regen("Regenerate"),
        ]
        .align_y(iced::Center);
        // The module map, drawn natively (same engine as the Import Graph
        // overlay), sits at the top; the LLM prose follows.
        let mut items: Vec<Element<'_, Message>> = Vec::new();
        if let Some(layout) = app.overview_map.as_ref().filter(|l| !l.nodes.is_empty()) {
            items.push(
                column![
                    text("Module map").size(15).color(theme::FG_MUTED),
                    container(
                        iced::widget::canvas::Canvas::new(GraphCanvas {
                            layout,
                            kind: crate::Overlay::ProjectImports,
                            scroll_zooms: false,
                        })
                        .width(Fill)
                        .height(iced::Length::Fixed(320.0)),
                    )
                    .width(Fill),
                    text("size = how connected · drag to pan · click a node to open it")
                        .size(10)
                        .color(theme::DIM),
                ]
                .spacing(6)
                .into(),
            );
        }
        items.extend(render_prepared(app, &app.overview_prepared));
        return container(
            column![
                header,
                scrollable(Column::with_children(items).spacing(10).width(Fill).max_width(860))
                    .direction(thin_scroll())
                    .style(theme::overlay_scrollbar)
                    .height(Fill),
            ]
            .spacing(14),
        )
        .width(Fill)
        .height(Fill)
        .padding([20, 28])
        .into();
    }

    // Not generated yet.
    let action: Element<'_, Message> = if !app.llm_available {
        text("Configure an LLM key in Settings to generate the overview.")
            .size(12)
            .color(theme::DIM)
            .into()
    } else if app.explanations.is_empty() {
        text("Run “Explain All” first — the overview is built from the explanations.")
            .size(12)
            .color(theme::DIM)
            .into()
    } else {
        regen("Generate overview").into()
    };
    center(
        column![
            text("Architecture Overview").size(18).color(theme::FG),
            text("A generated tour of this codebase: what it does, core modules, entry points, and where to start.")
                .size(13)
                .color(theme::DIM),
            action,
        ]
        .spacing(12)
        .align_x(iced::Center)
        .max_width(560),
    )
    .into()
}

/// Group a large integer with thousands separators, e.g. `12345` → `12,345`.
pub(crate) fn fmt_thousands(n: usize) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + len / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A stable, readable color for the language at rank `i` in the bar/table.
pub(crate) fn lang_color(i: usize) -> iced::Color {
    const PALETTE: [u32; 8] = [
        0x61afef, // blue
        0x98c379, // green
        0xe5c07b, // yellow
        0xe06c75, // red
        0xc678dd, // purple
        0x56b6c2, // cyan
        0xd19a66, // orange
        0x828b9c, // grey
    ];
    theme::rgb(PALETTE[i % PALETTE.len()])
}

/// A small filled square used as a color key next to a language row.
pub(crate) fn color_swatch(color: iced::Color) -> Element<'static, Message> {
    container(space())
        .width(10)
        .height(10)
        .style(move |_t| container::Style {
            background: Some(color.into()),
            border: iced::Border { radius: 2.0.into(), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

/// FillPortion factor per language for the proportion bar. Scaled by the *total*
/// code (not the max) into a small fixed budget, so the factors — which a `Row`
/// sums into a u16 — can never overflow: with several large languages, a
/// max-based scale summed past `u16::MAX`, panicking the flex layout in debug and
/// wrapping (wrong bar) in release. Ratios are preserved; every non-empty set of
/// languages yields at least a 1-wide sliver.
pub(crate) fn bar_portions(langs: &[crate::stats::LangStat]) -> Vec<u16> {
    const BUDGET: f64 = 10_000.0; // sum stays ~BUDGET + langs.len(), well under u16
    let total: u64 = langs.iter().map(|l| l.code as u64).sum();
    if total == 0 {
        return Vec::new();
    }
    langs
        .iter()
        .map(|l| ((l.code as f64 / total as f64) * BUDGET).round().max(1.0) as u16)
        .collect()
}

/// A GitHub-style proportion bar: one colored segment per language, its width
/// proportional to that language's code lines.
pub(crate) fn language_bar(report: &crate::stats::StatsReport) -> Element<'_, Message> {
    let portions = bar_portions(&report.langs);
    if portions.is_empty() {
        return space().height(12).into();
    }
    let mut bar = Row::new();
    for (i, portion) in portions.into_iter().enumerate() {
        let color = lang_color(i);
        bar = bar.push(
            container(space())
                .width(Length::FillPortion(portion))
                .height(Fill)
                .style(move |_t| container::Style {
                    background: Some(color.into()),
                    ..container::Style::default()
                }),
        );
    }
    container(bar)
        .width(Fill)
        .height(12)
        .style(|_t| container::Style {
            background: Some(theme::BG_PANEL.into()),
            border: iced::Border { radius: 3.0.into(), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

/// One headline number in the summary strip (a big value over a muted label).
pub(crate) fn stat_cell(label: &str, value: usize) -> Element<'_, Message> {
    column![
        text(fmt_thousands(value)).size(22).color(theme::FG_BRIGHT),
        text(label.to_string()).size(11).color(theme::FG_MUTED),
    ]
    .spacing(2)
    .into()
}

/// The code-statistics "home": totals, a language-proportion bar, a per-language
/// breakdown, and the largest files (each row opens the file).
pub(crate) fn stats_home(app: &App) -> Element<'_, Message> {
    let refresh = button(text("Refresh").size(12))
        .style(theme::toolbar_button)
        .padding([3, 12])
        .on_press(Message::RefreshStats);

    // Nothing to show yet: computing, or a project with no counted code.
    let Some(report) = app.stats.as_ref().filter(|r| !r.is_empty()) else {
        let msg = if app.building_stats {
            "Computing code statistics…"
        } else {
            "No code files to count in this project."
        };
        return center(
            column![
                text("Code Statistics").size(18).color(theme::FG),
                text(msg).size(13).color(theme::DIM),
            ]
            .spacing(12)
            .align_x(iced::Center)
            .max_width(560),
        )
        .into();
    };

    // A recompute running over already-shown (stale) numbers.
    let updating: Element<'_, Message> = if app.building_stats {
        text("updating…").size(12).color(theme::DIM).into()
    } else {
        space().width(0).into()
    };
    let header = row![
        text("Code Statistics").size(18).color(theme::FG),
        space().width(Fill),
        updating,
        space().width(10),
        refresh,
    ]
    .align_y(iced::Center);

    let t = &report.totals;
    // "Code files" (tokei-counted source files), not the tree's total file count —
    // labelled explicitly so the two numbers don't read as a contradiction.
    let summary = row![
        stat_cell("Code files", t.files),
        stat_cell("Lines", t.lines()),
        stat_cell("Code", t.code),
        stat_cell("Comments", t.comments),
        stat_cell("Blanks", t.blanks),
    ]
    .spacing(36);

    // Per-language table: a color key, name, and counts, ranked by code lines.
    let total_code = report.totals.code.max(1);
    let cell = |s: String, w: f32, color: iced::Color| text(s).size(12).color(color).width(Length::Fixed(w));
    let head = |s: &'static str, w: f32| text(s).size(11).color(theme::FG_MUTED).width(Length::Fixed(w));
    let table_header = row![
        space().width(16),
        head("Language", 150.0),
        head("Files", 70.0),
        head("Code", 90.0),
        head("Comments", 90.0),
        head("Blanks", 80.0),
        head("Share", 70.0),
    ]
    .spacing(8)
    .align_y(iced::Center);
    let mut table = Column::new().spacing(6).push(table_header);
    for (i, l) in report.langs.iter().enumerate() {
        let share = l.code as f64 / total_code as f64 * 100.0;
        table = table.push(
            row![
                color_swatch(lang_color(i)),
                cell(l.name.clone(), 150.0, theme::FG),
                cell(fmt_thousands(l.files), 70.0, theme::FG_MUTED),
                cell(fmt_thousands(l.code), 90.0, theme::FG),
                cell(fmt_thousands(l.comments), 90.0, theme::FG_MUTED),
                cell(fmt_thousands(l.blanks), 80.0, theme::FG_MUTED),
                cell(format!("{share:.2}%"), 70.0, theme::DIM),
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
    }

    // Largest files: click a row to open it.
    let root = app.project.as_ref().map(|p| p.root.clone());
    let mut files = Column::new().spacing(2);
    for f in &report.top_files {
        let inner = row![
            text(f.rel.to_string_lossy().into_owned())
                .size(12)
                .color(theme::FG)
                .width(Fill)
                .wrapping(Wrapping::None),
            text(fmt_thousands(f.lines)).size(12).color(theme::FG_MUTED).width(Length::Fixed(80.0)),
            text(f.lang.clone()).size(11).color(theme::DIM).width(Length::Fixed(90.0)),
        ]
        .spacing(8)
        .align_y(iced::Center);
        let mut b = button(inner)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding { top: 2.0, right: 8.0, bottom: 2.0, left: 8.0 });
        if let Some(root) = &root {
            b = b.on_press(Message::OpenAbs { abs: root.join(&f.rel), line: None, push: true });
        }
        files = files.push(b);
    }

    let section = |title: &'static str| text(title).size(13).color(theme::FG_MUTED);
    let body = column![
        summary,
        space().height(4),
        language_bar(report),
        space().height(10),
        section("By language"),
        table,
        space().height(14),
        section("Largest files"),
        files,
    ]
    .spacing(8)
    .width(Fill)
    .max_width(860);

    container(
        column![
            header,
            scrollable(body)
                .direction(thin_scroll())
                .style(theme::overlay_scrollbar)
                .height(Fill),
        ]
        .spacing(14),
    )
    .width(Fill)
    .height(Fill)
    .padding([20, 28])
    .into()
}

