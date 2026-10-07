//! Explanation rendering (markdown/SVG), consent modals, section header.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// Render prepared segments in order: markdown prose through the markdown widget,
/// math and mermaid as inline SVGs. Until a background render lands — and for
/// good when the renderer could not draw it — an equation or diagram shows as
/// its source, so nothing ever sits behind a "rendering…" that will not end.
/// Shared by the explanation panel and the architecture overview.
pub(crate) fn render_prepared<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
) -> Vec<Element<'a, Message>> {
    use crate::{PreparedInline, PreparedSeg};
    let mut out: Vec<Element<'_, Message>> = Vec::new();
    for seg in segments {
        match seg {
            PreparedSeg::Markdown(items) => out.push(
                iced::widget::markdown::view(items, theme::markdown_settings())
                    .map(|url| Message::Content(ContentMsg::OpenLink(url.to_string()))),
            ),
            PreparedSeg::DisplayMath(key, tex) => out.push(match app.proj.explain.svgs.get(key) {
                Some(sv) => container(svg_widget(sv))
                    .width(Fill)
                    .align_x(iced::Center)
                    .padding([6, 0])
                    .into(),
                None => source_block(
                    tex,
                    "equation",
                    app.proj.explain.svg_failed.contains_key(key),
                ),
            }),
            PreparedSeg::Mermaid(key, src) => out.push(match app.proj.explain.svgs.get(key) {
                Some(sv) => container(svg_widget(sv)).padding([8, 0]).into(),
                None => source_block(
                    src,
                    "diagram",
                    app.proj.explain.svg_failed.contains_key(key),
                ),
            }),
            PreparedSeg::Code(lines) => out.push(code_block(lines)),
            PreparedSeg::InlineLine(parts) => {
                let mut line: Vec<Element<'_, Message>> = Vec::new();
                for p in parts {
                    match p {
                        // The prose around an equation is markdown too
                        // (bold, code, `clew:` citation links), not raw text.
                        PreparedInline::Text(piece) => line.push(
                            iced::widget::rich_text(piece.spans(theme::markdown_settings().style))
                                .color(theme::fg())
                                .on_link_click(|v| Message::Content(ContentMsg::OpenLink(v)))
                                .into(),
                        ),
                        PreparedInline::Math(key, tex) => {
                            line.push(match app.proj.explain.svgs.get(key) {
                                Some(sv) => svg_widget(sv),
                                // The TeX itself, in the code face: readable
                                // while the render is on its way, and what
                                // stays when the renderer rejects it.
                                None => text(tex.as_str())
                                    .font(Font::MONOSPACE)
                                    .size(ts::SMALL)
                                    .color(if app.proj.explain.svg_failed.contains_key(key) {
                                        theme::warning()
                                    } else {
                                        theme::dim()
                                    })
                                    .into(),
                            })
                        }
                    }
                }
                out.push(
                    Row::with_children(line)
                        .align_y(iced::Center)
                        .spacing(1)
                        .into(),
                );
            }
        }
    }
    out
}

/// A fenced code block: per-line spans in the editor's own syntax palette, on
/// the editor background, scrolling horizontally rather than wrapping.
pub(crate) fn code_block<'a>(lines: &'a [crate::highlight::HlLine]) -> Element<'a, Message> {
    use crate::highlight::style_color;
    let rows: Vec<Element<'_, Message>> = lines
        .iter()
        .map(|l| {
            if l.spans.is_empty() {
                // Keep blank lines from collapsing to zero height.
                return text(" ").font(Font::MONOSPACE).size(ts::BODY).into();
            }
            let spans: Vec<iced::widget::text::Span<'_, Message>> = l
                .spans
                .iter()
                .map(|(t, style)| {
                    iced::widget::span(t.as_str())
                        .color(style.and_then(style_color).unwrap_or(theme::fg()))
                        .font(Font::MONOSPACE)
                        .size(ts::BODY)
                })
                .collect();
            iced::widget::rich_text(spans)
                .wrapping(Wrapping::None)
                .into()
        })
        .collect();
    container(
        scrollable(Column::with_children(rows).spacing(1))
            .direction(sideways_scroll())
            .style(theme::overlay_scrollbar)
            .width(Fill),
    )
    .width(Fill)
    .padding([8, 10])
    .style(theme::editor)
    .into()
}

/// A fixed-size `svg` widget for a rendered math/mermaid block.
pub(crate) fn svg_widget<'a>(sv: &crate::ExplainSvg) -> Element<'a, Message> {
    iced::widget::svg(sv.handle.clone())
        .width(Length::Fixed(sv.width))
        .height(Length::Fixed(sv.height))
        .into()
}

/// A math/mermaid block shown as its source: while its render is on the way,
/// and in place of one the renderer could not draw (`failed`), which also
/// says so above the source.
pub(crate) fn source_block<'a>(source: &'a str, what: &str, failed: bool) -> Element<'a, Message> {
    let code = text(source)
        .font(Font::MONOSPACE)
        .size(ts::SMALL)
        .color(theme::dim());
    let body: Element<'a, Message> = if failed {
        column![
            text(format!("⚠ this {what} could not be rendered — its source:"))
                .size(ts::CAPTION)
                .color(theme::warning()),
            code,
        ]
        .spacing(4)
        .into()
    } else {
        code.into()
    };
    container(body)
        .padding(8)
        .width(Fill)
        .style(theme::editor)
        .into()
}

/// The "CALLED BY" / "CALLS" navigation strip for a focused function: its
/// one-hop callers and callees from the project call graph, each annotated with
/// its explanation summary. Clicking a link jumps there — the code view and this
/// panel both follow (via `OpenNode` → `open_file` → `follow_caret`), so you can
/// walk the call flow one hop at a time.
pub(crate) fn call_flow_rows<'a>(
    app: &'a App,
    node: &crate::explain::Node,
) -> Vec<Element<'a, Message>> {
    use crate::explain::Node;
    let mut out: Vec<Element<'a, Message>> = Vec::new();
    // The call graph keys a function by `(file, name, ordinal)` like the
    // explain node, so a file's second `new` shows its own call flow.
    let Node::Function {
        file,
        name,
        ordinal,
    } = node
    else {
        return out;
    };
    let g = &app.proj.project_calls.graph;
    let Some(id) = g.id_of_key(file, name, *ordinal) else {
        // No node for this function yet — show a hint only while the graph builds.
        if app.proj.project_calls.building {
            out.push(section_header("CALL FLOW"));
            out.push(
                container(
                    text("Building call graph…")
                        .size(ts::CAPTION)
                        .color(theme::dim()),
                )
                .padding([1, 8])
                .into(),
            );
        }
        return out;
    };
    // Debug overlay: the actual live caller (the frame below the current one on
    // the paused stack), so the static "CALLED BY" list marks who *really* called
    // this function in the running program.
    let live_parent: Option<String> = app
        .debug
        .session
        .as_ref()
        .filter(|s| s.status == crate::DebugStatus::Stopped)
        .and_then(|s| s.frames.get(1))
        .map(|f| crate::short_frame_name(&f.name));
    out.extend(reached_from_rows(app, file, name, id));
    // Split callers into tests and the rest, so the tests that exercise this
    // function read as its executable spec. Callees stay their own group.
    let sorted = |ids: &[usize]| {
        let mut v: Vec<&crate::projectcalls::SymNode> = ids.iter().map(|&i| g.node(i)).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    let (tests, callers): (Vec<_>, Vec<_>) = sorted(g.callers_of(id))
        .into_iter()
        .partition(|n| app.is_test_symbol(&n.file, &n.name));
    let callees = sorted(g.callees_of(id));
    let green = theme::success();

    // (header, arrow, nodes, jump-to-call-site, name colour). Tests and callees
    // open the symbol's definition; a plain caller jumps to the actual call line.
    for (label, arrow, items, to_call, name_color) in [
        ("TESTS", "←", tests, false, green),
        ("CALLED BY", "←", callers, true, theme::accent()),
        ("CALLS", "→", callees, false, theme::accent()),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push(section_header(label));
        for n in items {
            let target = Node::Function {
                file: n.file.clone(),
                name: n.name.clone(),
                ordinal: 0, // call-graph nodes are name-resolved
            };
            let summary = app
                .proj
                .explain
                .cache
                .get(&target)
                .map(crate::app::shown_summary)
                .unwrap_or_default();
            let is_live = label == "CALLED BY" && live_parent.as_deref() == Some(n.name.as_str());
            let live = theme::success();
            let mut r = row![
                text(arrow)
                    .size(ts::SMALL)
                    .color(if is_live { live } else { theme::dim() }),
                text(n.name.clone()).size(ts::BODY).color(name_color),
                text(rel_of(app, &n.file))
                    .size(ts::CAPTION)
                    .color(theme::dim()),
            ]
            .spacing(6);
            if is_live {
                r = r.push(text("● live").size(ts::CAPTION).color(live));
            }
            let mut col = column![r].spacing(1);
            if !summary.is_empty() {
                col = col.push(one_line_desc(&summary, 64));
            }
            let msg = if to_call {
                Message::Nav(NavMsg::JumpToCall {
                    caller_file: n.file.clone(),
                    caller: n.name.clone(),
                    callee: name.clone(),
                })
            } else {
                Message::Semantic(SemanticMsg::OpenNode(target))
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

/// How many "reached from" chains the panel shows, and how many calls deep
/// the walk looks for an entry point.
pub(crate) const REACHED_FROM_CHAINS: usize = 4;
pub(crate) const REACHED_FROM_DEPTH: usize = 16;

/// The REACHED FROM section: how execution gets to this function from the
/// project's entry points — one row per chain, entry first, each step a
/// button opening that function; or the fact that this function IS an entry
/// point; or that no entry point reaches it in the call graph (a library's
/// API, or a call the graph does not see: an interface, a callback). Nothing
/// at all for a test (an entry of its own kind, shown under TESTS where it
/// calls something), or in a project without entry points, where every
/// function would say the same.
fn reached_from_rows<'a>(
    app: &'a App,
    file: &std::path::Path,
    name: &str,
    id: usize,
) -> Vec<Element<'a, Message>> {
    use crate::explain::Node;
    let mut out: Vec<Element<'a, Message>> = Vec::new();
    let g = &app.proj.project_calls.graph;
    let caption = |t: String| -> Element<'a, Message> {
        container(text(t).size(ts::CAPTION).color(theme::dim()))
            .padding([1, 8])
            .into()
    };
    if let Some(kind) = app.entry_kind_of(file, name) {
        out.push(section_header("REACHED FROM"));
        out.push(
            container(
                row![
                    text("●").size(ts::SMALL).color(theme::success()),
                    text(format!("an entry point: {}", kind.label()))
                        .size(ts::SMALL)
                        .color(theme::success()),
                ]
                .spacing(6),
            )
            .padding([1, 8])
            .into(),
        );
        return out;
    }
    // A test is an entry of its own kind (the TESTS section is where tests
    // appear), and a project without entry points has no chains to show.
    if app.is_test_symbol(file, name) || !app.has_entry_points() {
        return out;
    }
    let node = Node::Function {
        file: file.to_path_buf(),
        name: name.to_string(),
        ordinal: 0,
    };
    // The walk covers the function's whole caller cone: memoized per graph
    // and index generation, like the graph overlay's rankings.
    let paths = app.proj.view_memo.entry_paths.get_or(
        (
            app.proj.project_calls.graph_rev,
            app.proj.symbol_index_rev,
            node,
        ),
        || {
            g.paths_from_entries(
                id,
                |n| app.entry_class_of(&n.file, &n.name),
                REACHED_FROM_CHAINS,
                REACHED_FROM_DEPTH,
            )
        },
    );
    out.push(section_header("REACHED FROM"));
    if paths.is_empty() {
        out.push(caption(
            "no entry point reaches this in the call graph (called from outside the \
             project, through an interface, or by a callback)"
                .into(),
        ));
        return out;
    }
    for chain in paths.iter() {
        let mut steps: Vec<Element<'a, Message>> = Vec::new();
        let last = chain.len().saturating_sub(1);
        for (i, &step) in chain.iter().enumerate() {
            let n = g.node(step);
            if i == last {
                steps.push(text("this").size(ts::SMALL).color(theme::dim()).into());
                break;
            }
            let entry = i == 0;
            let label: Element<'a, Message> = if entry {
                let kind = app
                    .entry_kind_of(&n.file, &n.name)
                    .map(|k| k.label())
                    .unwrap_or("test");
                row![
                    text(n.name.clone()).size(ts::SMALL).color(theme::success()),
                    text(kind).size(ts::CAPTION).color(theme::dim()),
                ]
                .spacing(4)
                .into()
            } else {
                text(n.name.clone())
                    .size(ts::SMALL)
                    .color(theme::accent())
                    .into()
            };
            steps.push(
                button(label)
                    .style(theme::list_row(false))
                    .padding([1, 4])
                    .on_press(Message::Semantic(SemanticMsg::OpenNode(Node::Function {
                        file: n.file.clone(),
                        name: n.name.clone(),
                        ordinal: 0, // call-graph nodes are name-resolved
                    })))
                    .into(),
            );
            steps.push(text("›").size(ts::SMALL).color(theme::dim()).into());
        }
        out.push(
            container(iced::widget::Row::with_children(steps).spacing(4).wrap())
                .padding([1, 6])
                .into(),
        );
    }
    out
}

/// The Explain tab's content: the explanation of the node under the caret (or
/// the Cmd+clicked file/folder) — its summary or block detail, the action
/// buttons, and a drill-down into the summaries it contains.
pub(crate) fn explain_content(app: &App) -> Element<'_, Message> {
    use crate::explain::Node;
    let Some(node) = app.proj.explain.view.as_ref() else {
        return container(
            text("Move the cursor into a function, or Cmd+click a file/folder.")
                .size(ts::SMALL)
                .color(theme::dim()),
        )
        .padding(10)
        .into();
    };
    let title = match node {
        Node::Folder(p) => format!("📁 {}", rel_of(app, p)),
        Node::File(p) => rel_of(app, p),
        Node::Function { file, name, .. } => format!("{name} · {}", rel_of(app, file)),
    };

    // Call-flow navigation first (callers/callees), then the explanation prose.
    let mut rows: Vec<Element<'_, Message>> = call_flow_rows(app, node);
    rows.extend(render_prepared(app, &app.proj.explain.prepared));

    // The children come from a scan of the WHOLE cache, sorted by label —
    // memoized per cache generation (`cache_seq`, bumped by every change to
    // the cache) and node, not redone on every repaint of this always-visible
    // panel.
    let children = app
        .proj
        .view_memo
        .explain_children
        .get_or((app.proj.explain.cache_seq, node.clone()), || {
            explain_children(&app.proj.explain.cache, node)
        });
    if !children.is_empty() {
        rows.push(section_header("CONTAINS"));
        for (label, n) in children.iter() {
            let summary = app
                .proj
                .explain
                .cache
                .get(n)
                .map(crate::app::shown_summary)
                .unwrap_or_default();
            rows.push(
                button(
                    column![
                        text(label.clone()).size(ts::BODY).color(theme::accent()),
                        one_line_desc(&summary, 64),
                    ]
                    .spacing(1),
                )
                .style(theme::list_row(false))
                .width(Fill)
                .padding([3, 6])
                .on_press(Message::Explain(ExplainMsg::Show(n.clone())))
                .into(),
            );
        }
    }

    let act = |label: &str, msg: Message| {
        button(text(label.to_string()).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(msg)
    };
    let mut actions: Vec<Element<'_, Message>> = Vec::new();
    // Functions get a summary ⇄ per-block-detail toggle.
    if matches!(node, Node::Function { .. }) {
        actions.push(if app.proj.explain.showing_detail {
            act("Summary", Message::Explain(ExplainMsg::Show(node.clone()))).into()
        } else {
            act(
                "Explain blocks",
                Message::Explain(ExplainMsg::Blocks(node.clone())),
            )
            .into()
        });
    }
    if app.llm_available {
        actions.push(act("Re-explain", Message::Explain(ExplainMsg::ReexplainNode)).into());
    }

    // The header is padded, but the scrollable itself reaches the panel's right
    // edge so its scrollbar lines up with the outline's below (content is padded
    // inside instead). A thin bar keeps both looking tidy.
    let pad = |l, r| Padding {
        top: 0.0,
        right: r as f32,
        bottom: 0.0,
        left: l as f32,
    };
    container(
        column![
            container(text(title).size(ts::BASE).color(theme::fg())).padding(Padding {
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
                    .padding(Padding {
                        top: 2.0,
                        right: 10.0,
                        bottom: 8.0,
                        left: 12.0
                    }),
            )
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(iced::Length::Fill),
        ]
        .spacing(8),
    )
    .height(Fill)
    .into()
}

/// The explained nodes directly inside `node` (a folder's files and folders,
/// a file's functions) as `(label, node)`, sorted by label. The label is both
/// the sort key and what the row shows, so it is built once per child rather
/// than once per comparison.
pub(crate) fn explain_children(
    cache: &crate::explain::Cache,
    node: &crate::explain::Node,
) -> Vec<(String, crate::explain::Node)> {
    let mut children: Vec<(String, crate::explain::Node)> = cache
        .keys()
        .filter(|n| explain_is_child(node, n))
        .map(|n| (explain_child_label(n), n.clone()))
        .collect();
    children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    children
}

/// A small uppercase section label (OUTLINE / CONTAINS / CALLED BY …) — a
/// single consistent style for every panel sub-heading.
/// A section's heading. The label is copied (uppercased), so the element
/// borrows nothing from it: a `format!`ed label may be a temporary.
pub(crate) fn section_header<'a>(label: &str) -> Element<'a, Message> {
    container(
        text(label.to_uppercase())
            .size(ts::CAPTION)
            .color(theme::fg_muted()),
    )
    .padding(Padding {
        top: 12.0,
        right: 10.0,
        bottom: 4.0,
        left: 10.0,
    })
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

pub(crate) fn lsp_consent_modal(
    consent: &crate::LspConsent,
    waiting: usize,
) -> Element<'_, Message> {
    use crate::LspProvision;
    const SERVER_BLURB: &str = "clew manages its own “go to definition” server for this \
                                language, separate from anything on your system:";
    let (title, blurb, action) = match consent.provision {
        LspProvision::DebugAdapter { .. } => (
            "Install a debug adapter?",
            "Debugging this program needs a debug adapter that is not installed yet. \
             This will run:",
            "Install",
        ),
        LspProvision::Install(_) => ("Install a language server?", SERVER_BLURB, "Install"),
        _ => ("Download a language server?", SERVER_BLURB, "Download"),
    };

    let panel = container(
        column![
            text(title).size(ts::TITLE).color(theme::fg()),
            text(blurb).size(ts::BASE).color(theme::fg()),
            container(
                text(format!("{} {}", consent.server_name, consent.version))
                    .size(ts::BODY)
                    .color(theme::accent())
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::None),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text(consent.describe())
                .size(ts::BODY)
                .color(theme::dim())
                .wrapping(Wrapping::None),
            // Asked one at a time: say that more follow, so answering this
            // one and seeing another does not read as the same question again.
            text(match waiting {
                0 => String::new(),
                1 => "1 more install is waiting for your answer after this one.".to_string(),
                n => format!("{n} more installs are waiting for your answer after this one."),
            })
            .size(ts::SMALL)
            .color(theme::dim()),
            row![
                space().width(Fill),
                button(text("Not now").size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::Lsp(LspMsg::ConsentDismissed)),
                button(text(action).size(ts::BASE))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::Lsp(LspMsg::ConsentAllowed)),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    // The choice is required: the backdrop ignores clicks (Escape declines).
    modal(panel, Placement::Center, Backdrop::Dim(None))
}

/// Confirm what the project's own `lsp.toml` asks clew to do for a language:
/// run a command, hand the server `init_options`, or both. That file ships
/// with the repository, so each part is shown in full and must be approved
/// before it takes effect — options included, because a server will run a
/// program named in them (rust-analyzer's `overrideCommand`, pyright's
/// `pythonPath`) just as readily as clew would run a `command`.
pub(crate) fn lsp_command_modal(pending: &crate::PendingLspCommand) -> Element<'_, Message> {
    // One block per thing being approved, each only when it exists: a modal
    // that showed an empty command box for an options-only config would be
    // asking about something the repository did not ask for.
    let mut blocks: Vec<Element<'_, Message>> = Vec::new();
    if let Some(line) = pending.command_line() {
        blocks.push(
            container(
                text(line)
                    .size(ts::BODY)
                    .color(theme::warn())
                    .font(Font::MONOSPACE),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor)
            .into(),
        );
    }
    if let Some(options) = &pending.init_options {
        blocks.push(
            text("initialize options from .clew/lsp.toml:")
                .size(ts::BODY)
                .color(theme::dim())
                .into(),
        );
        blocks.push(
            container(
                scrollable(
                    text(options)
                        .size(ts::BODY)
                        .color(theme::warn())
                        .font(Font::MONOSPACE),
                )
                .height(Length::Fixed(160.0)),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor)
            .into(),
        );
    }
    // Name the actual decision: "Run it" is a lie for a config that only sends
    // options, and a button whose label does not match what happens is how a
    // consent prompt stops being consent.
    let (decline, accept) = match pending.command.is_some() {
        true => ("Don't run", "Run it"),
        false => ("Don't allow", "Allow"),
    };
    let panel = container(
        column![
            text("Let this project configure its language server?")
                .size(ts::TITLE)
                .color(theme::fg()),
            text(
                "This project's .clew/lsp.toml decides what clew runs for its \
                 own “go to definition”, and with which options. It is part of \
                 the repository — allow it only if you trust this project.",
            )
            .size(ts::BASE)
            .color(theme::fg()),
            column(blocks).spacing(8),
            text(format!(
                "language: {} · server: {} {}",
                pending.language, pending.server_name, pending.version
            ))
            .size(ts::BODY)
            .color(theme::dim())
            .wrapping(Wrapping::None),
            row![
                space().width(Fill),
                button(text(decline).size(ts::BASE))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::Lsp(LspMsg::CommandDismissed)),
                button(text(accept).size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::Lsp(LspMsg::CommandAllowed)),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    // The choice is required: the backdrop ignores clicks (Escape declines).
    modal(panel, Placement::Center, Backdrop::Dim(None))
}

// ---------------------------------------------------------------- consent modal

pub(crate) fn consent_modal(root: &std::path::Path) -> Element<'_, Message> {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());

    let panel = container(
        column![
            text("Allow clew to use this project?")
                .size(ts::TITLE)
                .color(theme::fg()),
            text(
                "clew stores bookmarks and reading data in a “.clew” folder \
                 inside the project:",
            )
            .size(ts::BASE)
            .color(theme::fg()),
            container(
                // Paths have no spaces to break on, so glyph-wrap to keep a long
                // path inside the box instead of overflowing off the panel.
                text(format!("{}/.clew", root.display()))
                    .size(ts::BODY)
                    .color(theme::accent())
                    .font(Font::MONOSPACE)
                    .wrapping(Wrapping::Glyph),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
            text("Without it the project can't be opened. You can delete .clew any time.")
                .size(ts::BODY)
                .color(theme::dim()),
            row![
                space().width(Fill),
                button(text("Not now").size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(Message::Project(ProjectMsg::ConsentDenied)),
                button(text(format!("Allow in {name}")).size(ts::BASE))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(Message::Project(ProjectMsg::ConsentAllowed)),
            ]
            .spacing(10)
            .align_y(iced::Center),
        ]
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    // The choice is required: the backdrop ignores clicks (Escape declines).
    modal(panel, Placement::Center, Backdrop::Dim(None))
}
