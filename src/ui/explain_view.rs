//! Explanation rendering (markdown/SVG), consent modals, section header.

use super::heard;
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
    render_segments(app, segments, None)
}

/// [`render_prepared`] in a margin note's type: the design's 13 px, its
/// lines set 1.55 apart.
pub(crate) fn render_note<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
) -> Vec<Element<'a, Message>> {
    render_segments(
        app,
        segments,
        Some(Prose {
            size: NOTE_SIZE,
            line_height: NOTE_LINE_HEIGHT,
        }),
    )
}

/// [`render_prepared`] in an answer's type: the design's 13.5 px, its lines
/// set 1.6 apart.
pub(crate) fn render_answer<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
) -> Vec<Element<'a, Message>> {
    render_segments(
        app,
        segments,
        Some(Prose {
            size: theme::PROSE_SIZE,
            line_height: 1.6,
        }),
    )
}

/// A margin note's text size.
pub(crate) const NOTE_SIZE: f32 = 13.0;
/// A margin note's line height, relative to its size.
pub(crate) const NOTE_LINE_HEIGHT: f32 = 1.55;
/// Code inside prose, as the design sets it: a size of its own, under the
/// words around it.
const PROSE_CODE_SIZE: f32 = 12.0;

/// Room either side of code in prose, inside its chip: the design's 5 px,
/// as an en space (half the size it is set at).
const PROSE_CODE_ROOM: f32 = 10.0;

/// The room itself: an en space, joined to the code by a word joiner. The
/// design's chip is one box, its padding laid out with its code and broken
/// before or after, never inside; a bare space at a line's end hangs past
/// it, uncounted (cosmic-text's way with trailing white space), so the
/// line took up to 5 px more than the design's and wrapped a word later.
const PROSE_CODE_ROOM_TEXT: &str = "\u{2002}\u{2060}";

/// The design's padding of a chip of code in prose, above and below it.
const PROSE_CODE_PAD_Y: f32 = 1.0;

/// How far inside its line a chip of code in prose (`size`, lines `line`
/// tall) is drawn, at its top and at its bottom. The design's renderer
/// draws an inline code's background around the code face's ascent and
/// descent from the line's baseline (each rounded, as it takes them, and
/// the leading over the line's text floored), its padding out; a span's
/// background here spans its line.
fn chip_inset(size: f32, line: f32) -> (f32, f32) {
    let (ascent, descent) = ts::INTER.rounded(size);
    let baseline = ((line - (ascent + descent)) / 2.0).floor() + ascent;
    let (code_ascent, code_descent) = ts::MONO.rounded(PROSE_CODE_SIZE);
    (
        baseline - code_ascent - PROSE_CODE_PAD_Y,
        line - (baseline + code_descent + PROSE_CODE_PAD_Y),
    )
}

/// How the markdown widget sets prose of a type of its own (a note, an
/// answer): its paragraphs at their size and line height, the code in them
/// at [`PROSE_CODE_SIZE`] on a chip with room of its own either side (a
/// span's padding is drawn, not laid out, so the room is an en space);
/// everything else as in any explanation.
#[derive(Clone, Copy)]
struct Prose {
    size: f32,
    line_height: f32,
}

impl<'a> iced::widget::markdown::Viewer<'a, Message> for Prose {
    fn on_link_click(url: iced::widget::markdown::Uri) -> Message {
        Message::Content(ContentMsg::OpenLink(url))
    }

    fn paragraph(
        &self,
        settings: iced::widget::markdown::Settings,
        text: &iced::widget::markdown::Text,
    ) -> Element<'a, Message> {
        use iced::widget::text::{LineHeight, Span};
        // Every span its line height in pixels: a line takes its height from
        // the spans that state one, so code (sized apart) would otherwise
        // set the lines it is on closer than the rest.
        // In the design's renderer's unit of layout, a 64th of a pixel,
        // rounded down: line after line, a paragraph's lines fall where
        // the design's do.
        let line_px = (self.size * self.line_height * 64.0).floor() / 64.0;
        let line = LineHeight::Absolute(line_px.into());
        let (inset_top, inset_bottom) = chip_inset(self.size, line_px);
        let room = || {
            Span::new(PROSE_CODE_ROOM_TEXT)
                .size(PROSE_CODE_ROOM)
                .line_height(line)
        };
        let mut spans = Vec::new();
        for span in text.spans(settings.style).iter().cloned() {
            if span.font == Some(settings.style.inline_code_font) {
                spans.push(room());
                spans.push(
                    span.size(PROSE_CODE_SIZE)
                        .line_height(line)
                        .padding(iced::Padding {
                            top: -inset_top,
                            bottom: -inset_bottom,
                            left: PROSE_CODE_ROOM / 2.0,
                            right: PROSE_CODE_ROOM / 2.0,
                        }),
                );
                spans.push(room());
            } else {
                spans.push(span.line_height(line));
            }
        }
        iced::widget::rich_text(spans)
            .size(settings.text_size)
            .line_height(line)
            .on_link_click(Self::on_link_click)
            .into()
    }
}

fn render_segments<'a>(
    app: &'a App,
    segments: &'a [crate::PreparedSeg],
    prose: Option<Prose>,
) -> Vec<Element<'a, Message>> {
    use crate::{PreparedInline, PreparedSeg};
    let mut out: Vec<Element<'_, Message>> = Vec::new();
    for seg in segments {
        match seg {
            PreparedSeg::Markdown(items) if let Some(prose) = prose => {
                out.push(iced::widget::markdown::view_with(
                    items,
                    theme::markdown_settings_at(prose.size),
                    &prose,
                ))
            }
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
                    tr!("Could not render this equation. Its source:"),
                    app.proj.explain.svg_failed.contains_key(key),
                ),
            }),
            PreparedSeg::Mermaid(key, src) => out.push(match app.proj.explain.svgs.get(key) {
                Some(sv) => container(svg_widget(sv)).padding([8, 0]).into(),
                None => source_block(
                    src,
                    tr!("Could not render this diagram. Its source:"),
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
                                    .font(crate::fonts::CODE)
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
                return text(" ").font(crate::fonts::CODE).size(ts::BODY).into();
            }
            let spans: Vec<iced::widget::text::Span<'_, Message>> = l
                .spans
                .iter()
                .map(|(t, style)| {
                    iced::widget::span(t.as_str())
                        .color(style.and_then(style_color).unwrap_or(theme::code_fg()))
                        .font(crate::fonts::CODE)
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
/// says so (`failure`) above the source.
pub(crate) fn source_block<'a>(
    source: &'a str,
    failure: &str,
    failed: bool,
) -> Element<'a, Message> {
    let code = text(source)
        .font(crate::fonts::CODE)
        .size(ts::SMALL)
        .color(theme::dim());
    let body: Element<'a, Message> = if failed {
        column![
            text(format!("⚠ {failure}"))
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
            out.push(section_header(tr!("Call flow")));
            out.push(
                container(
                    text(tr!("Building call graph…"))
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
        let mut v: Vec<(usize, &crate::projectcalls::SymNode)> =
            ids.iter().map(|&i| (i, g.node(i))).collect();
        v.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name).then(a.line.cmp(&b.line)));
        v
    };
    let (tests, callers): (Vec<_>, Vec<_>) = sorted(g.callers_of(id))
        .into_iter()
        .partition(|(_, n)| app.is_test_symbol(&n.file, &n.name));
    let callees = sorted(g.callees_of(id));
    let green = theme::success();

    // (header, arrow, nodes, jump-to-call-site, name colour). Tests and callees
    // open the symbol's definition; a plain caller jumps to the actual call line.
    for (label, arrow, items, to_call, name_color) in [
        (tr!("Tests"), "←", tests, false, green),
        (tr!("Called by"), "←", callers, true, theme::accent()),
        (tr!("Calls"), "→", callees, false, theme::accent()),
    ] {
        if items.is_empty() {
            continue;
        }
        out.push(section_header(label));
        for (k, &(node_id, n)) in items.iter().enumerate() {
            // Each row is its own function: a file's second `total` opens
            // that one, not the first.
            let ordinal = g.ordinals().get(node_id).copied().unwrap_or(0);
            let target = Node::Function {
                file: n.file.clone(),
                name: n.name.clone(),
                ordinal,
            };
            // Two rows that would read the same (`Item.total`, `Cart.total`)
            // say where each is.
            let twin = |j: usize| {
                items
                    .get(j)
                    .is_some_and(|(_, m)| m.name == n.name && m.file == n.file)
            };
            let place = if (k > 0 && twin(k - 1)) || twin(k + 1) {
                format!("{}:{}", rel_of(app, &n.file), n.line)
            } else {
                rel_of(app, &n.file)
            };
            let summary = app
                .proj
                .explain
                .cache
                .get(&target)
                .map(crate::app::shown_summary)
                .unwrap_or_default();
            // Only the callers (the rows that jump to their call) can be the
            // live one.
            let is_live = to_call && live_parent.as_deref() == Some(n.name.as_str());
            let live = theme::success();
            // Wrapped, the place under the name where the margin is narrow:
            // cut at its edge, the two read as one clipped word.
            let mut r = row![
                text(arrow)
                    .size(ts::SMALL)
                    .color(if is_live { live } else { theme::dim() }),
                text(n.name.clone()).size(ts::BODY).color(name_color),
                text(place).size(ts::CAPTION).color(theme::dim()),
            ]
            .spacing(6);
            if is_live {
                r = r.push(text(tr!("● live")).size(ts::CAPTION).color(live));
            }
            let mut col = column![r.wrap()].spacing(1);
            if !summary.is_empty() {
                col = col.push(one_line_desc(&summary, 64));
            }
            let msg = if to_call {
                Message::Nav(NavMsg::JumpToCall {
                    caller_file: n.file.clone(),
                    caller: n.name.clone(),
                    caller_line: n.line,
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
                    .on_press(heard::said(&n.name, msg))
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
        out.push(section_header(tr!("Reached from")));
        out.push(
            container(
                row![
                    text("●").size(ts::SMALL).color(theme::success()),
                    text(tr!(
                        "Entry point: {kind}",
                        kind = crate::i18n::lookup(kind.label())
                    ))
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
    // The walk covers the function's whole caller cone: memoized per graph
    // and index generation, like the graph overlay's rankings, and per node
    // id — keyed by file and name alone, a file's two `new`s shared one
    // entry, and the second showed the first's chains.
    let paths = app.proj.view_memo.entry_paths.get_or(
        (
            app.proj.project_calls.graph_rev,
            app.proj.symbol_index_rev,
            id,
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
    // A test that calls this itself is under Tests: not again here, as the
    // one step of a chain.
    let chains: Vec<&Vec<usize>> = paths
        .iter()
        .filter(|chain| {
            !(chain.len() == 2 && {
                let first = g.node(chain[0]);
                app.is_test_symbol(&first.file, &first.name)
            })
        })
        .collect();
    if chains.is_empty() && !paths.is_empty() {
        return out;
    }
    out.push(section_header(tr!("Reached from")));
    if chains.is_empty() {
        out.push(caption(tr!("No entry point reaches this").into()));
        return out;
    }
    for chain in chains {
        let mut steps: Vec<Element<'a, Message>> = Vec::new();
        let last = chain.len().saturating_sub(1);
        for (i, &step) in chain.iter().enumerate() {
            let n = g.node(step);
            if i == last {
                steps.push(text(tr!("this")).size(ts::SMALL).color(theme::dim()).into());
                break;
            }
            let entry = i == 0;
            let label: Element<'a, Message> = if entry {
                let kind = app
                    .entry_kind_of(&n.file, &n.name)
                    .map(|k| crate::i18n::lookup(k.label()))
                    .unwrap_or(tr!("test"));
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
            // A step and the arrow after it, one piece: wrapped apart, a
            // line began with the arrow.
            steps.push(
                row![
                    button(label)
                        .style(theme::list_row(false))
                        .padding([1, 4])
                        .on_press(heard::said(
                            &n.name,
                            Message::Semantic(SemanticMsg::OpenNode(Node::Function {
                                file: n.file.clone(),
                                name: n.name.clone(),
                                ordinal: g.ordinals().get(step).copied().unwrap_or(0),
                            })),
                        )),
                    text("›").size(ts::SMALL).color(theme::dim()),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into(),
            );
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
        return compact_empty_state(Glyph::Lightbulb, tr!("Move the cursor into a function."));
    };
    // The note's head: what it is about, in the face it is written in (a
    // function's name as code), and what kind of thing that is.
    let (title, meta, code) = match node {
        Node::Folder(p) => (rel_of(app, p), tr!("folder").to_string(), false),
        Node::File(p) => (rel_of(app, p), tr!("file").to_string(), false),
        Node::Function { file, name, .. } => (
            name.clone(),
            match file.file_name() {
                Some(f) => tr!("function in {file}", file = f.to_string_lossy()),
                None => "fn".to_string(),
            },
            true,
        ),
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
        rows.push(section_header(tr!("Contains")));
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
                .on_press(heard::said(
                    label,
                    Message::Explain(ExplainMsg::Show(n.clone())),
                ))
                .into(),
            );
        }
    }

    let act = |label: &str, msg: Message| {
        button(text(label.to_string()).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(heard::said(label, msg))
    };
    let mut actions: Vec<Element<'_, Message>> = Vec::new();
    // Functions get a summary ⇄ per-block-detail toggle.
    if matches!(node, Node::Function { .. }) {
        actions.push(if app.proj.explain.showing_detail {
            act(
                tr!("Summary"),
                Message::Explain(ExplainMsg::Show(node.clone())),
            )
            .into()
        } else {
            act(
                tr!("Explain blocks"),
                Message::Explain(ExplainMsg::Blocks(node.clone())),
            )
            .into()
        });
    }
    // Not explained yet: what explains it, where the note says so.
    if !app.proj.explain.running && !app.proj.explain.cache.contains_key(node) {
        actions.push(act(tr!("Explain All"), super::toolbar::explain_all_message(app)).into());
    } else if app.llm_available {
        actions.push(
            act(
                tr!("Re-explain"),
                Message::Explain(ExplainMsg::ReexplainNode),
            )
            .into(),
        );
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
            container(
                row![
                    text(title)
                        .size(if code { ts::BODY } else { ts::BASE })
                        .font(if code {
                            crate::fonts::CODE_BOLD
                        } else {
                            crate::fonts::UI_SEMIBOLD
                        })
                        .color(theme::fg_bright())
                        .wrapping(Wrapping::None),
                    space().width(Fill),
                    text(meta)
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .spacing(8)
                .align_y(iced::Center)
            )
            .padding(Padding {
                top: 12.0,
                right: 14.0,
                bottom: 0.0,
                left: 14.0,
            }),
            // left 4 so the button's own inner padding lands its text at ~12,
            // aligned with the title above (not indented past it).
            container(Row::with_children(actions).spacing(4)).padding(pad(4, 12)),
            // From its top for each node it follows: one widget for all of
            // them kept where the last was scrolled to, and the next one's
            // text, when it came, opened partway down.
            crate::ui::fresh::fresh_for(
                node,
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
                .height(iced::Length::Shrink),
            ),
        ]
        .spacing(8),
    )
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
        text(crate::i18n::caps(label))
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
        tr!("{size} MB", size = crate::i18n::decimal(b / MB, 1))
    } else if b >= KB {
        tr!("{size} KB", size = crate::i18n::decimal(b / KB, 0))
    } else {
        tr!("{size} B", size = bytes)
    }
}

// ---------------------------------------------------------------- LSP consent

pub(crate) fn lsp_consent_modal(
    consent: &crate::LspConsent,
    number: u64,
    waiting: usize,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'_, Message> {
    let asked = asked.into();
    let ready = asked.ready;
    use crate::LspProvision;
    let server_blurb = tr!("Clew installs it in its own folder, not system-wide:");
    let (title, blurb, action) = match consent.provision {
        LspProvision::DebugAdapter { .. } => (
            tr!("Install a debug adapter?"),
            tr!("This will run:"),
            tr!("Install"),
        ),
        LspProvision::Install(_) => (
            tr!("Install a language server?"),
            server_blurb,
            tr!("Install"),
        ),
        LspProvision::Build { .. } => (
            tr!("Let the language server build this project?"),
            tr!("Building runs the project's own code. Allow only if you trust it."),
            tr!("Allow"),
        ),
        _ => (
            tr!("Download a language server?"),
            server_blurb,
            tr!("Download"),
        ),
    };
    // Leave to build is given to one project, and the button says which.
    let action = match &consent.provision {
        LspProvision::Build { root, .. } => match root.file_name() {
            Some(name) => tr!("Allow in {folder}", folder = name.to_string_lossy()),
            None => action.to_string(),
        },
        _ => action.to_string(),
    };

    let panel = container(
        column![
            text(title).size(ts::TITLE).color(theme::fg()),
            text(blurb).size(ts::BASE).color(theme::fg()),
            container(
                // Leave to build is not about a version: any version builds.
                text(match consent.provision {
                    LspProvision::Build { .. } => {
                        crate::app::model::shown(&consent.server_name).into_owned()
                    }
                    _ => format!(
                        "{} {}",
                        crate::app::model::shown(&consent.server_name),
                        crate::app::model::shown(&consent.version)
                    ),
                })
                .size(ts::BODY)
                .color(theme::accent())
                .font(crate::fonts::CODE)
                .wrapping(Wrapping::None),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor),
        ]
        // Wrapped: what it runs is the point of the question, and a line
        // cut at the dialog's edge hid the most of it.
        .push({
            let said = consent.describe();
            (!said.is_empty()).then(|| {
                text(said)
                    .size(ts::BODY)
                    .color(theme::dim())
                    .wrapping(Wrapping::WordOrGlyph)
            })
        })
        // The server's own program already on this computer: said, and
        // offered in place of the download.
        .push(consent.on_path.as_ref().map(|program| {
            text(tr!(
                "{server} is already on this computer, at {path}.",
                server = crate::app::model::shown(&consent.server_name),
                path = program.display()
            ))
            .size(ts::BODY)
            .color(theme::fg_muted())
            .wrapping(Wrapping::WordOrGlyph)
        }))
        // Asked one at a time: say that more follow, so answering this one
        // and seeing another does not read as the same question again.
        .push(more_questions(waiting))
        .push(
            row![
                space().width(Fill),
                button(text(tr!("Not now")).size(ts::BASE))
                    .style(theme::toolbar_button)
                    .padding([6, 16])
                    .on_press(heard::answer(
                        tr!("Not now"),
                        heard::Answer::No,
                        Message::Lsp(LspMsg::ConsentDismissed(number))
                    )),
                // Each Allow offered to the voice only once it can be
                // pressed: read first, said yes to after.
                consent.on_path.as_ref().map(|_| crate::ui::fresh::allow(
                    ready,
                    button(text(tr!("Use that one")).size(ts::BASE))
                        .style(theme::toolbar_button)
                        .padding([6, 16])
                        .on_press_maybe(ready.then(|| heard::said(
                            tr!("Use that one"),
                            Message::Lsp(LspMsg::ConsentUsePath(number))
                        )))
                )),
                crate::ui::fresh::allow(
                    ready,
                    button(text(action.clone()).size(ts::BASE))
                        .style(theme::primary_button)
                        .padding([6, 16])
                        .on_press_maybe(ready.then(|| heard::answer(
                            &action,
                            heard::Answer::Yes,
                            Message::Lsp(LspMsg::ConsentAllowed(number))
                        )))
                ),
            ]
            .spacing(10)
            .align_y(iced::Center),
        )
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    // The choice is required: the backdrop ignores clicks (Escape declines).
    // Fresh for each question: one replaced in place is drawn with buttons
    // of its own — a press held on the last one's Allow is not this one's.
    let panel = crate::ui::placed::placed(&asked, crate::ui::fresh::fresh(number, panel));
    modal(panel, Placement::Center, Backdrop::Dim(None))
}

/// "N more questions are waiting…", under a question `waiting` more follow;
/// under the last, its line held empty. The dialog is centred: a line
/// coming as more questions queued moved its buttons under the hand on its
/// way to them.
fn more_questions<'a>(waiting: usize) -> Element<'a, Message> {
    let said = match waiting {
        0 => String::new(),
        n => trn!("1 more after this", "{n} more after this", n),
    };
    text(said)
        .size(ts::SMALL)
        .line_height(ts::normal_line(ts::SMALL))
        .color(theme::dim())
        .into()
}

/// Confirm what the project's own `lsp.toml` asks clew to do for a language:
/// run a command, hand the server `init_options`, or both. That file ships
/// with the repository, so each part is shown in full and must be approved
/// before it takes effect — options included, because a server will run a
/// program named in them (rust-analyzer's `overrideCommand`, pyright's
/// `pythonPath`) just as readily as clew would run a `command`.
///
/// `waiting` more questions follow it, which it says, as the install consent
/// does: answering this one and seeing another must not read as the same
/// question again.
pub(crate) fn lsp_command_modal(
    pending: &crate::PendingLspCommand,
    number: u64,
    waiting: usize,
    asked: impl Into<crate::ui::placed::Asked>,
) -> Element<'_, Message> {
    let asked = asked.into();
    let ready = asked.ready;
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
                    .font(crate::fonts::CODE),
            )
            .padding(8)
            .width(Fill)
            .style(theme::editor)
            .into(),
        );
    }
    if let Some(options) = &pending.init_options {
        blocks.push(
            text(tr!("Initialization options from .clew/lsp.toml:"))
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
                        .font(crate::fonts::CODE),
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
        true => (tr!("Don't run"), tr!("Run it")),
        false => (tr!("Don't allow"), tr!("Allow")),
    };
    let panel = container(
        column![
            text(tr!("Let this project configure its language server?"))
                .size(ts::TITLE)
                .color(theme::fg()),
            text(tr!(
                "Its .clew/lsp.toml chooses what runs. Allow only if you trust this project."
            ))
            .size(ts::BASE)
            .color(theme::fg()),
            column(blocks).spacing(8),
            text(tr!(
                "Language: {language}, server: {server} {version}",
                language = crate::app::languages::language_name(&pending.language),
                server = pending.server_name,
                version = pending.version
            ))
            .size(ts::BODY)
            .color(theme::dim())
            .wrapping(Wrapping::None),
        ]
        .push(more_questions(waiting))
        .push(
            row![
                space().width(Fill),
                button(text(decline).size(ts::BASE))
                    .style(theme::primary_button)
                    .padding([6, 16])
                    .on_press(heard::answer(
                        decline,
                        heard::Answer::No,
                        Message::Lsp(LspMsg::CommandDismissed(number))
                    )),
                crate::ui::fresh::allow(
                    ready,
                    button(text(accept).size(ts::BASE))
                        .style(theme::toolbar_button)
                        .padding([6, 16])
                        .on_press_maybe(ready.then(|| heard::answer(
                            accept,
                            heard::Answer::Yes,
                            Message::Lsp(LspMsg::CommandAllowed(number))
                        )))
                ),
            ]
            .spacing(10)
            .align_y(iced::Center),
        )
        .spacing(14),
    )
    .width(DIALOG_W)
    .padding(CONFIRM_PAD)
    .style(theme::modal_panel);

    // The choice is required: the backdrop ignores clicks (Escape declines).
    // Fresh for each question: one replaced in place is drawn with buttons
    // of its own — a press held on the last one's Allow is not this one's.
    let panel = crate::ui::placed::placed(&asked, crate::ui::fresh::fresh(number, panel));
    modal(panel, Placement::Center, Backdrop::Dim(None))
}
