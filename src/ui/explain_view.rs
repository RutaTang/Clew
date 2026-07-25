//! Explanation rendering (markdown/SVG), consent modals, section header.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Render prepared segments in order: markdown prose through the markdown widget,
/// math and mermaid as inline SVGs (with a placeholder until a background render
/// lands). Shared by the explanation panel and the architecture overview.
pub(crate) fn render_prepared<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
) -> Vec<Element<'a, Message>> {
    use crate::{PreparedInline, PreparedSeg};
    let mut out: Vec<Element<'_, Message>> = Vec::new();
    for seg in segments {
        match seg {
            PreparedSeg::Markdown(items) => out.push(
                iced::widget::markdown::view(items, iced::Theme::Dark)
                    .map(|url| Message::OpenLink(url.to_string())),
            ),
            PreparedSeg::DisplayMath(key) => out.push(match app.explain.svgs.get(key) {
                Some(sv) => container(svg_widget(sv))
                    .width(Fill)
                    .align_x(iced::Center)
                    .padding([6, 0])
                    .into(),
                None => svg_placeholder("equation"),
            }),
            PreparedSeg::Mermaid(key, src) => out.push(match app.explain.svgs.get(key) {
                Some(sv) => container(svg_widget(sv)).padding([8, 0]).into(),
                // No SVG yet (still rendering, or the diagram failed to render):
                // show the raw mermaid source rather than a perpetual spinner.
                None => container(
                    text(src.clone()).font(Font::MONOSPACE).size(11).color(theme::DIM),
                )
                .padding(8)
                .width(Fill)
                .style(theme::editor)
                .into(),
            }),
            PreparedSeg::InlineLine(parts) => {
                let mut line: Vec<Element<'_, Message>> = Vec::new();
                for p in parts {
                    match p {
                        PreparedInline::Text(t) => {
                            line.push(text(t.clone()).color(theme::FG).into());
                        }
                        PreparedInline::Math(key) => line.push(match app.explain.svgs.get(key) {
                            Some(sv) => svg_widget(sv),
                            None => text("…").color(theme::DIM).into(),
                        }),
                    }
                }
                out.push(Row::with_children(line).align_y(iced::Center).spacing(1).into());
            }
        }
    }
    out
}

/// A fixed-size `svg` widget for a rendered math/mermaid block.
pub(crate) fn svg_widget<'a>(sv: &crate::ExplainSvg) -> Element<'a, Message> {
    iced::widget::svg(sv.handle.clone())
        .width(Length::Fixed(sv.width))
        .height(Length::Fixed(sv.height))
        .into()
}

/// Shown in place of a math/mermaid block until its background render arrives.
pub(crate) fn svg_placeholder<'a>(what: &str) -> Element<'a, Message> {
    text(format!("rendering {what}…")).size(11).color(theme::DIM).into()
}

/// The Explain tab's content: the explanation of the node under the caret (or
/// the Cmd+clicked file/folder) — its summary or block detail, the action
/// buttons, and a drill-down into the summaries it contains.
/// The "CALLED BY" / "CALLS" navigation strip for a focused function: its
/// one-hop callers and callees from the project call graph, each annotated with
/// its explanation summary. Clicking a link jumps there — the code view and this
/// panel both follow (via `OpenNode` → `open_file` → `follow_caret`), so you can
/// walk the call flow one hop at a time.
pub(crate) fn call_flow_rows<'a>(app: &'a App, node: &crate::explain::Node) -> Vec<Element<'a, Message>> {
    use crate::explain::Node;
    let mut out: Vec<Element<'a, Message>> = Vec::new();
    let Node::Function { file, name } = node else {
        return out;
    };
    let g = &app.project_calls.graph;
    let Some(id) = g.id_of(file, name) else {
        // No node for this function yet — show a hint only while the graph builds.
        if app.project_calls.building {
            out.push(section_header("CALL FLOW"));
            out.push(
                container(text("Building call graph…").size(10).color(theme::DIM))
                    .padding([1, 8])
                    .into(),
            );
        }
        return out;
    };
    // Debug overlay: the actual live caller (the frame below the current one on
    // the paused stack), so the static "CALLED BY" list marks who *really* called
    // this function in the running program.
    let live_parent: Option<String> = app.debug.session
        .as_ref()
        .filter(|s| s.status == crate::DebugStatus::Stopped)
        .and_then(|s| s.frames.get(1))
        .map(|f| crate::short_frame_name(&f.name));
    // Split callers into tests and the rest, so the tests that exercise this
    // function read as its executable spec. Callees stay their own group.
    let sorted = |ids: &[usize]| {
        let mut v: Vec<&crate::projectcalls::SymNode> = ids.iter().map(|&i| g.node(i)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    let (tests, callers): (Vec<_>, Vec<_>) =
        sorted(g.callers_of(id)).into_iter().partition(|n| app.is_test_symbol(&n.file, &n.name));
    let callees = sorted(g.callees_of(id));
    let green = theme::rgb(0x98c379);

    // (header, arrow, nodes, jump-to-call-site, name colour). Tests and callees
    // open the symbol's definition; a plain caller jumps to the actual call line.
    for (label, arrow, items, to_call, name_color) in [
        ("TESTS", "←", tests, false, green),
        ("CALLED BY", "←", callers, true, theme::ACCENT),
        ("CALLS", "→", callees, false, theme::ACCENT),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push(section_header(label));
        for n in items {
            let target = Node::Function { file: n.file.clone(), name: n.name.clone() };
            let summary = app.explain.cache.get(&target).map(|c| c.summary.as_str()).unwrap_or("");
            let is_live = label == "CALLED BY" && live_parent.as_deref() == Some(n.name.as_str());
            let live = theme::rgb(0x98c379);
            let mut r = row![
                text(arrow).size(11).color(if is_live { live } else { theme::DIM }),
                text(n.name.clone()).size(12).color(name_color),
                text(rel_of(app, &n.file)).size(10).color(theme::DIM),
            ]
            .spacing(6);
            if is_live {
                r = r.push(text("● live").size(9).color(live));
            }
            let mut col = column![r].spacing(1);
            if !summary.is_empty() {
                col = col.push(one_line_desc(summary, 64));
            }
            let msg = if to_call {
                Message::JumpToCall {
                    caller_file: n.file.clone(),
                    caller: n.name.clone(),
                    callee: name.clone(),
                }
            } else {
                Message::OpenNode(target)
            };
            out.push(
                button(col)
                    .style(theme::list_row(false))
                    .width(Fill)
                    .padding([3, 6])
                    .on_press(msg)
                    .into(),
            );
        }
    }
    out
}

pub(crate) fn explain_content(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let Some(node) = app.explain.view.as_ref() else {
        return container(
            text("Move the cursor into a function, or Cmd+click a file/folder.")
                .size(11)
                .color(theme::DIM),
        )
        .padding(10)
        .into();
    };
    let title = match node {
        Node::Folder(p) => format!("📁 {}", rel_of(app, p)),
        Node::File(p) => rel_of(app, p),
        Node::Function { file, name } => format!("{name} · {}", rel_of(app, file)),
    };

    // Call-flow navigation first (callers/callees), then the explanation prose.
    let mut rows: Vec<Element<'_, Message>> = call_flow_rows(app, node);
    rows.extend(render_prepared(app, &app.explain.prepared));

    let mut children: Vec<(&Node, &str)> = app.explain.cache
        .iter()
        .filter(|(n, _)| explain_is_child(node, n))
        .map(|(n, c)| (n, c.summary.as_str()))
        .collect();
    children.sort_by_key(|(n, _)| explain_child_label(n));
    if !children.is_empty() {
        rows.push(section_header("CONTAINS"));
        for (n, sum) in children {
            rows.push(
                button(
                    column![
                        text(explain_child_label(n)).size(12).color(theme::ACCENT),
                        one_line_desc(sum, 64),
                    ]
                    .spacing(1),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([3, 6])
                .on_press(Message::ShowExplanation(n.clone()))
                .into(),
            );
        }
    }

    let act = |label: &str, msg: Message| {
        button(text(label.to_string()).size(11))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };
    let mut actions: Vec<Element<'_, Message>> = Vec::new();
    // Functions get a summary ⇄ per-block-detail toggle.
    if matches!(node, Node::Function { .. }) {
        actions.push(if app.explain.showing_detail {
            act("Summary", Message::ShowExplanation(node.clone())).into()
        } else {
            act("Explain blocks", Message::ExplainBlocks(node.clone())).into()
        });
    }
    if app.llm_available {
        actions.push(act("Re-explain", Message::ReexplainNode).into());
    }

    // The header is padded, but the scrollable itself reaches the panel's right
    // edge so its scrollbar lines up with the outline's below (content is padded
    // inside instead). A thin bar keeps both looking tidy.
    let pad = |l, r| Padding { top: 0.0, right: r as f32, bottom: 0.0, left: l as f32 };
    container(
        column![
            container(text(title).size(13).color(theme::FG)).padding(Padding {
                top: 10.0,
                right: 12.0,
                bottom: 0.0,
                left: 12.0,
            }),
            // left 4 so the button's own inner padding lands its text at ~12,
            // aligned with the title above (not indented past it).
            container(Row::with_children(actions).spacing(4)).padding(pad(4, 12)),
            scrollable(
                Column::with_children(rows)
                    .spacing(8)
                    .width(Fill)
                    .padding(Padding { top: 2.0, right: 10.0, bottom: 8.0, left: 12.0 }),
            )
            .direction(Direction::Vertical(Scrollbar::new().width(6.0).scroller_width(6.0)))
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Fill),
        ]
        .spacing(8),
    )
    .height(Fill)
    .into()
}

/// A small uppercase section label (OUTLINE / CONTAINS / CALLED BY …) — a
/// single consistent style for every panel sub-heading.
pub(crate) fn section_header(label: &str) -> Element<'_, Message> {
    container(text(label.to_uppercase()).size(10).color(theme::FG_MUTED))
        .padding(Padding { top: 12.0, right: 10.0, bottom: 4.0, left: 10.0 })
        .into()
}

pub(crate) fn human_size(bytes: u64) -> String {
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

pub(crate) fn lsp_consent_modal(consent: &crate::LspConsent) -> Element<'_, Message> {
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

pub(crate) fn consent_modal(root: &std::path::Path) -> Element<'_, Message> {
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
                // Paths have no spaces to break on, so glyph-wrap to keep a long
                // path inside the box instead of overflowing off the panel.
                text(format!("{}/.clew", root.display()))
                    .size(12)
                    .color(theme::ACCENT)
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::Glyph),
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

