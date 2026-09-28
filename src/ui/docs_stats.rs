//! Docs tab/page, overview home, stats page, and pane_area.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// The overview home's embedded module map. Never idly spinning: this is
/// the landing page, and a spinning map never settles — it re-tessellated
/// every frame for as long as the page was open. The spin belongs to the
/// graph modal, whose header toggles it. Nor does the wheel zoom it: the
/// page around it scrolls.
pub(crate) fn overview_map_canvas<'a>(
    app: &'a App,
    layout: &'a crate::graphlayout::Layout,
) -> GraphCanvas<'a> {
    GraphCanvas::new(
        layout,
        app.proj.overview.map_rev,
        crate::Overlay::ProjectImports,
        false,
        app.graph_3d,
        false,
    )
}

pub(crate) fn group_header(rel: &str) -> Element<'_, Message> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let (glyph, color) = crate::icons::file_icon(name);
    container(
        row![
            icon_text(glyph, color, 12.0),
            text(rel)
                .size(ts::SMALL)
                .color(theme::fg_muted())
                .wrapping(Wrapping::None),
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

/// What the pane area shows in place of the code panes. [`pane_cover`] picks
/// it, and both [`pane_area`] (to draw it) and [`time_travel_on_screen`] (a
/// session under it is not on screen) go through that choice.
pub(crate) enum PaneCover<'a> {
    /// The project is being scanned.
    Scanning,
    /// No project is open.
    Welcome,
    /// A documentation page.
    Docs(&'a crate::DocPage),
    /// The project overview.
    Overview,
    /// The code statistics.
    Stats,
}

/// What covers the code panes right now, if anything (see [`PaneCover`]).
pub(crate) fn pane_cover(app: &App) -> Option<PaneCover<'_>> {
    Some(if app.scanning {
        PaneCover::Scanning
    } else if app.proj.project.is_none() {
        PaneCover::Welcome
    } else if let Some(page) = &app.proj.docs.page {
        PaneCover::Docs(page)
    } else if app.proj.overview.showing {
        PaneCover::Overview
    } else if app.proj.stats.showing {
        PaneCover::Stats
    } else {
        return None;
    })
}

pub(crate) fn pane_area(app: &App) -> Element<'_, Message> {
    if let Some(cover) = pane_cover(app) {
        return editor_shell(match cover {
            PaneCover::Scanning => empty_state(
                Glyph::Search,
                "Scanning project…",
                "Indexing files so you can read and search them.",
                None,
            ),
            PaneCover::Welcome => welcome(app),
            PaneCover::Docs(page) => docs_page(page),
            PaneCover::Overview => overview_home(app),
            PaneCover::Stats => stats_home(app),
        });
    }
    // Always two slots, so toggling the split never moves pane 0's subtree
    // (and with it the state of its code view) to another place in the tree.
    let second = if app.proj.split {
        pane_view(app, 1)
    } else {
        slot()
    };
    row![pane_view(app, 0), second]
        .spacing(if app.proj.split { 1 } else { 0 })
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
    let is_dir_file = (lang == "rust"
        && matches!(
            segs.last().copied(),
            Some("mod") | Some("lib") | Some("main")
        ))
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
    if app.proj.docs.files.is_empty() {
        return if app.docs_loading() {
            empty_state(
                Glyph::Sparkle,
                "Building docs…",
                "Reading the project's public API.",
                None,
            )
        } else {
            empty_state(
                Glyph::Note,
                "No documentation",
                "No documented symbols found in this project.",
                Some(("Rebuild", Message::Docs(DocsMsg::Refresh))),
            )
        };
    }

    // Toolbar: a filter on top, then the grouping / visibility / rebuild
    // controls (two rows so they fit a narrow sidebar).
    let filter = text_input("Filter docs…", &app.proj.docs.filter)
        .on_input(|v| Message::Docs(DocsMsg::FilterChanged(v)))
        .size(ts::BODY)
        .padding(6)
        .width(Fill);
    let chip = |label: String, msg: Message| {
        button(text(label).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([4, 8])
            .on_press(msg)
    };
    let group_btn = chip(
        if app.docs_view.by_module {
            "Modules".into()
        } else {
            "Files".into()
        },
        Message::Docs(DocsMsg::ToggleGrouping),
    );
    let vis_btn = chip(
        if app.docs_view.show_all {
            "All".into()
        } else {
            "Public".into()
        },
        Message::Docs(DocsMsg::ToggleShowAll),
    );
    let refresh = chip("↻".into(), Message::Docs(DocsMsg::Refresh));
    let controls = row![group_btn, vis_btn, space().width(Fill), refresh]
        .spacing(4)
        .align_y(iced::Center);
    let toolbar = column![filter, controls].spacing(4);

    let query = app.proj.docs.filter.trim().to_lowercase();
    let selected_line = app
        .proj
        .docs
        .page
        .as_ref()
        .and_then(|p| p.entries.first().map(|e| (p.rel.as_str(), e.line)));

    // Grouping walks every documented item, so it is memoized per installed
    // index and view options (`ui::ViewMemo`) rather than redone per repaint.
    let key = DocsKey {
        generation: app.proj.docs.generation,
        by_module: app.docs_view.by_module,
        show_all: app.docs_view.show_all,
        query: query.clone(),
    };
    let groups = app.proj.view_memo.docs_groups.get_or(key, || {
        docs_groups(
            &app.proj.docs.files,
            app.docs_view.by_module,
            app.docs_view.show_all,
            &query,
        )
    });

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for group in groups.iter() {
        let items = group.items.iter().filter_map(|&(fi, ii)| {
            let file = app.proj.docs.files.get(fi)?;
            Some((file.rel.as_str(), file.items.get(ii)?))
        });
        let label = group.label.clone();
        let expanded = !query.is_empty() || app.proj.docs.expanded.contains(&label);
        let arrow = if expanded { "▾" } else { "▸" };
        rows.push(
            button(
                row![
                    text(arrow).size(ts::CAPTION).color(theme::dim()).width(10),
                    text(label.clone())
                        .size(ts::BODY)
                        .color(theme::fg_muted())
                        .wrapping(Wrapping::None),
                ]
                .spacing(4)
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([3, 8])
            .on_press(Message::Docs(DocsMsg::ToggleFile(label.clone())))
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
                                .size(ts::CAPTION)
                                .color(theme::accent())
                                .font(Font::MONOSPACE)
                                .width(42),
                            text(item.name.clone())
                                .size(ts::BASE)
                                .wrapping(Wrapping::None),
                        ]
                        .spacing(4)
                        .align_y(iced::Center),
                    )
                    .style(theme::list_row(is_sel))
                    .width(Fill)
                    .padding([3, 8])
                    .on_press(Message::Docs(DocsMsg::Select {
                        rel: rel.to_string(),
                        line: item.line,
                    }))
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

/// Group the documented items that pass the filters, by file or by module, in
/// label order. Each group lists its items as `(file index, item index)` into
/// `files`, so selection keeps working across merged files. `query` is the
/// trimmed, lowercased filter; it matches a symbol's name OR its file path /
/// module label, so a path fragment like "http.dart" finds that file's
/// symbols. Merged module groups are sorted by item name.
pub(crate) fn docs_groups(
    files: &[clew_protocol::DocFile],
    by_module: bool,
    show_all: bool,
    query: &str,
) -> Vec<DocsGroup> {
    let mut groups: std::collections::BTreeMap<String, Vec<(usize, usize)>> =
        std::collections::BTreeMap::new();
    for (fi, file) in files.iter().enumerate() {
        let label = if by_module {
            module_label(&file.rel)
        } else {
            file.rel.clone()
        };
        let path_matches = query.is_empty()
            || file.rel.to_lowercase().contains(query)
            || label.to_lowercase().contains(query);
        let mut visible: Vec<(usize, usize)> = file
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| show_all || item.public)
            .filter(|(_, item)| path_matches || item.name.to_lowercase().contains(query))
            .map(|(ii, _)| (fi, ii))
            .collect();
        if !visible.is_empty() {
            groups.entry(label).or_default().append(&mut visible);
        }
    }
    groups
        .into_iter()
        .map(|(label, mut items)| {
            // Merged module groups read better alphabetically.
            if by_module {
                items.sort_by(|a, b| files[a.0].items[a.1].name.cmp(&files[b.0].items[b.1].name));
            }
            DocsGroup { label, items }
        })
        .collect()
}

/// The main-pane doc page: the selected item followed by its members, each with
/// signature and rendered doc comment (like a rustdoc type page).
pub(crate) fn docs_page<'a>(page: &'a crate::DocPage) -> Element<'a, Message> {
    let top_line = page.entries.first().map(|e| e.line);
    let header = row![
        text(page.rel.clone())
            .size(ts::BODY)
            .color(theme::dim())
            .wrapping(Wrapping::None),
        space().width(Fill),
        button(text("Open source").size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::Editor(EditorMsg::OpenRel {
                rel: page.rel.clone(),
                line: top_line,
            })),
    ]
    .align_y(iced::Center);

    let mut blocks: Vec<Element<'a, Message>> = Vec::new();
    for (idx, e) in page.entries.iter().enumerate() {
        let title_size = if idx == 0 { ts::DISPLAY } else { ts::SUBTITLE };
        let title = row![
            text(kind_badge(&e.kind))
                .size(ts::SMALL)
                .color(theme::accent())
                .font(Font::MONOSPACE),
            text(e.name.clone()).size(title_size).color(theme::fg()),
        ]
        .spacing(8)
        .align_y(iced::Center);

        let signature = container(
            text(e.signature.clone())
                .size(ts::BODY)
                .font(Font::MONOSPACE)
                .color(theme::fg_muted()),
        )
        .padding([6, 10])
        .width(Fill)
        .style(theme::editor);

        let doc: Element<'a, Message> = if e.doc_items.is_empty() {
            text("No documentation.")
                .size(ts::BODY)
                .color(theme::dim())
                .into()
        } else {
            iced::widget::markdown::view(&e.doc_items, theme::markdown_settings())
                .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string())))
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

    let body = scrollable(
        Column::with_children(blocks)
            .spacing(4)
            .width(Fill)
            .padding(Padding {
                top: 6.0,
                right: 20.0,
                bottom: 24.0,
                left: 8.0,
            }),
    )
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
        button(text(label).size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([3, 12])
            .on_press(Message::Overview(OverviewMsg::Generate))
    };

    if app.proj.overview.generating {
        return center(
            text("Generating architecture overview…")
                .size(ts::EMPHASIS)
                .color(theme::dim()),
        )
        .into();
    }

    if app.proj.overview.markdown.is_some() {
        let header = row![
            text("Architecture Overview")
                .size(ts::HEADING)
                .color(theme::fg()),
            space().width(Fill),
            regen("Regenerate"),
        ]
        .align_y(iced::Center);
        // The module map, drawn natively (same engine as the Import Graph
        // overlay), sits at the top; the LLM prose follows.
        let mut items: Vec<Element<'_, Message>> = Vec::new();
        if let Some(layout) = app
            .proj
            .overview
            .map
            .as_ref()
            .filter(|l| !l.nodes.is_empty())
        {
            items.push(
                column![
                    text("Module map")
                        .size(ts::SUBTITLE)
                        .color(theme::fg_muted()),
                    container(
                        iced::widget::canvas::Canvas::new(overview_map_canvas(app, layout))
                            .width(Fill)
                            .height(iced::Length::Fixed(OVERVIEW_MAP_H)),
                    )
                    .width(Fill),
                    text(format!(
                        "size = how connected · {} · drag a node to move it · click to open",
                        super::map_nav_hint(app.graph_3d)
                    ))
                    .size(ts::CAPTION)
                    .color(theme::dim()),
                ]
                .spacing(6)
                .into(),
            );
        }
        items.extend(render_prepared(app, &app.proj.overview.prepared));
        return container(
            column![
                header,
                scrollable(
                    Column::with_children(items)
                        .spacing(10)
                        .width(Fill)
                        .max_width(860)
                )
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
            .size(ts::BODY)
            .color(theme::dim())
            .into()
    } else if app.proj.explain.cache.is_empty() {
        text("Run “Explain All” first — the overview is built from the explanations.")
            .size(ts::BODY)
            .color(theme::dim())
            .into()
    } else {
        regen("Generate overview").into()
    };
    center(
        column![
            text("Architecture Overview").size(ts::HEADING).color(theme::fg()),
            text("A generated tour of this codebase: what it does, core modules, entry points, and where to start.")
                .size(ts::BASE)
                .color(theme::dim()),
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
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A stable, readable color for the language at rank `i` in the bar/table.
pub(crate) fn lang_color(i: usize) -> iced::Color {
    theme::series_color(i)
}

/// A small filled square used as a color key next to a language row.
pub(crate) fn color_swatch(color: iced::Color) -> Element<'static, Message> {
    container(space())
        .width(10)
        .height(10)
        .style(move |_t| container::Style {
            background: Some(color.into()),
            border: iced::Border {
                radius: 2.0.into(),
                ..Default::default()
            },
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
            background: Some(theme::bg_panel().into()),
            border: iced::Border {
                radius: 3.0.into(),
                ..Default::default()
            },
            ..container::Style::default()
        })
        .into()
}

/// One headline number in the summary strip (a big value over a muted label).
pub(crate) fn stat_cell(label: &str, value: usize) -> Element<'_, Message> {
    column![
        text(fmt_thousands(value))
            .size(ts::DISPLAY)
            .color(theme::fg_bright()),
        text(label.to_string())
            .size(ts::SMALL)
            .color(theme::fg_muted()),
    ]
    .spacing(2)
    .into()
}

/// The code-statistics "home": totals, a language-proportion bar, a per-language
/// breakdown, and the largest files (each row opens the file).
pub(crate) fn stats_home(app: &App) -> Element<'_, Message> {
    let refresh = button(text("Refresh").size(ts::BODY))
        .style(theme::toolbar_button)
        .padding([3, 12])
        .on_press(Message::Overview(OverviewMsg::RefreshStats));

    // Nothing to show yet: computing, or a project with no counted code.
    let Some(report) = app.proj.stats.report.as_ref().filter(|r| !r.is_empty()) else {
        let msg = if app.proj.stats.building {
            "Computing code statistics…"
        } else {
            "No code files to count in this project."
        };
        return center(
            column![
                text("Code Statistics").size(ts::HEADING).color(theme::fg()),
                text(msg).size(ts::BASE).color(theme::dim()),
            ]
            .spacing(12)
            .align_x(iced::Center)
            .max_width(560),
        )
        .into();
    };

    // A recompute running over already-shown (stale) numbers.
    let updating: Element<'_, Message> = if app.proj.stats.building {
        text("updating…").size(ts::BODY).color(theme::dim()).into()
    } else {
        slot()
    };
    let header = row![
        text("Code Statistics").size(ts::HEADING).color(theme::fg()),
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
    let cell = |s: String, w: f32, color: iced::Color| {
        text(s).size(ts::BODY).color(color).width(Length::Fixed(w))
    };
    let head = |s: &'static str, w: f32| {
        text(s)
            .size(ts::SMALL)
            .color(theme::fg_muted())
            .width(Length::Fixed(w))
    };
    let table_header = row![
        // Match the color swatch's width so headers line up with the cells below.
        space().width(10),
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
                cell(l.name.clone(), 150.0, theme::fg()),
                cell(fmt_thousands(l.files), 70.0, theme::fg_muted()),
                cell(fmt_thousands(l.code), 90.0, theme::fg()),
                cell(fmt_thousands(l.comments), 90.0, theme::fg_muted()),
                cell(fmt_thousands(l.blanks), 80.0, theme::fg_muted()),
                cell(format!("{share:.2}%"), 70.0, theme::dim()),
            ]
            .spacing(8)
            .align_y(iced::Center),
        );
    }

    // Largest files: click a row to open it.
    let root = app.proj.project.as_ref().map(|p| p.root.clone());
    let mut files = Column::new().spacing(2);
    for f in &report.top_files {
        let inner = row![
            text(f.rel.clone())
                .size(ts::BODY)
                .color(theme::fg())
                .width(Fill)
                .wrapping(Wrapping::None),
            text(fmt_thousands(f.lines))
                .size(ts::BODY)
                .color(theme::fg_muted())
                .width(Length::Fixed(80.0)),
            text(f.lang.clone())
                .size(ts::SMALL)
                .color(theme::dim())
                .width(Length::Fixed(90.0)),
        ]
        .spacing(8)
        .align_y(iced::Center);
        let mut b = button(inner)
            .style(theme::list_row(false))
            .width(Fill)
            .padding(Padding {
                top: 2.0,
                right: 8.0,
                bottom: 2.0,
                left: 8.0,
            });
        if let Some(root) = &root {
            b = b.on_press(Message::Editor(EditorMsg::OpenAbs {
                abs: root.join(&f.rel),
                line: None,
                push: true,
            }));
        }
        files = files.push(b);
    }

    let section = |title: &'static str| text(title).size(ts::BASE).color(theme::fg_muted());
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
