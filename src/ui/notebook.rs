//! The native Jupyter notebook view: a vertical list of cells — markdown
//! rendered through the richmd pipeline (math/mermaid included), code cells
//! highlighted like the editor, outputs (text/images/SVG) collapsed by default.
//! Read-only by design: clew renders what the notebook saved; it never
//! executes anything.

use super::*;

/// The notebook pane: all cells in one scrollable, whose scroll id matches the
/// code view's so scroll tracking (`CodeScrolled`) keeps working.
pub(crate) fn notebook_pane<'a>(
    app: &'a App,
    pane: usize,
    v: &'a Viewer,
    doc: &'a crate::NotebookDoc,
) -> Element<'a, Message> {
    // The goto target (search hit / outline click) gets a highlight ring.
    let target_cell = v.target_line.map(|line| {
        doc.cells
            .iter()
            .rposition(|c| c.proj_line <= line)
            .unwrap_or(0)
    });
    let cells: Vec<Element<'_, Message>> = doc
        .cells
        .iter()
        .enumerate()
        .map(|(i, cell)| cell_view(app, pane, v, i, cell, target_cell == Some(i)))
        .collect();
    scrollable(
        container(
            Column::with_children(cells)
                .spacing(14)
                .width(Fill)
                .padding(Padding {
                    top: 14.0,
                    right: 22.0,
                    bottom: 40.0,
                    left: 18.0,
                }),
        )
        .width(Fill),
    )
    .id(code_scroll_id(pane))
    .on_scroll(move |viewport| Message::CodeScrolled(pane, viewport))
    .direction(thin_scroll())
    .style(theme::overlay_scrollbar)
    .width(Fill)
    .height(Fill)
    .into()
}

fn cell_view<'a>(
    app: &'a App,
    pane: usize,
    v: &'a Viewer,
    index: usize,
    cell: &'a crate::NbCell,
    targeted: bool,
) -> Element<'a, Message> {
    let mut parts: Vec<Element<'_, Message>> = Vec::new();
    match cell.kind.as_str() {
        "code" => {
            // "In [n]" badge above the code, like Jupyter's gutter but compact.
            let badge = match cell.execution_count {
                Some(n) => format!("In [{n}]"),
                None => "In [ ]".into(),
            };
            parts.push(
                text(badge)
                    .size(10)
                    .font(Font::MONOSPACE)
                    .color(theme::dim())
                    .into(),
            );
            parts.push(explain_view::code_block(&cell.lines));
        }
        _ => {
            // Markdown (and raw) cells render as a document.
            parts.push(
                Column::with_children(explain_view::render_prepared(app, &cell.segs))
                    .spacing(6)
                    .width(Fill)
                    .into(),
            );
        }
    }
    if !cell.outputs.is_empty() {
        let expanded = v.nb_expanded.contains(&index);
        let label = if expanded {
            format!("▾ output ({})", cell.outputs.len())
        } else {
            format!("▸ output ({})", cell.outputs.len())
        };
        parts.push(
            button(text(label).size(11).color(theme::fg_muted()))
                .style(theme::toolbar_button)
                .padding([1, 6])
                .on_press(Message::NbToggleOutputs { pane, cell: index })
                .into(),
        );
        if expanded {
            for o in &cell.outputs {
                parts.push(output_view(o));
            }
        }
    }
    // A hairline ring marks the goto target cell (scroll is an estimate; the
    // ring is the precise pointer). Every cell carries the same padding so the
    // ring never shifts content — only the border color changes.
    let styled = container(Column::with_children(parts).spacing(6).width(Fill))
        .width(Fill)
        .padding(6)
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            border: iced::Border {
                color: if targeted {
                    theme::accent()
                } else {
                    iced::Color::TRANSPARENT
                },
                width: 1.0,
                radius: 6.0.into(),
            },
            ..iced::widget::container::Style::default()
        });
    styled.into()
}

fn output_view(output: &crate::NbOutput) -> Element<'_, Message> {
    match output {
        crate::NbOutput::Text { spans, stderr } => {
            let rows: Vec<Element<'_, Message>> = spans_to_lines(spans)
                .into_iter()
                .map(|line| {
                    if line.is_empty() {
                        return text(" ").font(Font::MONOSPACE).size(11).into();
                    }
                    let spans: Vec<iced::widget::text::Span<'_, Message>> = line
                        .into_iter()
                        .map(|(t, color)| {
                            iced::widget::span(t)
                                .color(match color {
                                    Some(idx) => ansi_color(idx),
                                    None if *stderr => theme::warn(),
                                    None => theme::fg(),
                                })
                                .font(Font::MONOSPACE)
                                .size(11)
                        })
                        .collect();
                    iced::widget::rich_text(spans)
                        .wrapping(Wrapping::None)
                        .into()
                })
                .collect();
            container(
                scrollable(Column::with_children(rows).spacing(1))
                    .direction(Direction::Horizontal(
                        Scrollbar::new().width(4.0).scroller_width(4.0),
                    ))
                    .style(theme::overlay_scrollbar)
                    .width(Fill),
            )
            .width(Fill)
            .padding([6, 10])
            .style(theme::editor)
            .into()
        }
        crate::NbOutput::Image(handle) => container(
            iced::widget::image(handle.clone())
                .width(Fill)
                .height(Length::Fixed(360.0)),
        )
        .width(Fill)
        .into(),
        crate::NbOutput::Svg(handle) => container(
            iced::widget::svg(handle.clone())
                .width(Fill)
                .height(Length::Fixed(360.0)),
        )
        .width(Fill)
        .into(),
        crate::NbOutput::Placeholder(label) => text(format!("⧉ {label} (not rendered)"))
            .size(11)
            .color(theme::dim())
            .into(),
    }
}

/// Split colored spans on newlines into per-line span runs (rich_text rows).
fn spans_to_lines(spans: &[(String, Option<u8>)]) -> Vec<Vec<(String, Option<u8>)>> {
    let mut lines: Vec<Vec<(String, Option<u8>)>> = vec![Vec::new()];
    for (text, color) in spans {
        let mut first = true;
        for piece in text.split('\n') {
            if !first {
                lines.push(Vec::new());
            }
            first = false;
            if !piece.is_empty() {
                lines
                    .last_mut()
                    .expect("never empty")
                    .push((piece.to_string(), *color));
            }
        }
    }
    lines
}

/// The 16-color ANSI palette, tuned to read on the editor background in both
/// themes (mid-luminance one-dark-ish values).
fn ansi_color(idx: u8) -> iced::Color {
    const P: [u32; 16] = [
        0x5c6370, // black → dim gray so it stays visible
        0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf, // normal
        0x7f848e, // bright black
        0xef7783, 0xa9d47f, 0xf0ca85, 0x74bdf5, 0xd48ce8, 0x67c5d1, 0xcfd6e0, // bright
    ];
    theme::rgb(P[(idx as usize) % 16])
}
