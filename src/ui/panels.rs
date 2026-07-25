//! Bottom panels (debug/ask) and the calls/imports tabs.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

/// The "Ask clew" bottom panel: a scrollable multi-turn Q&A over a question box.
/// Answers are grounded in retrieved code, cite it with jump links, and list
/// their retrieved sources as clickable chips.
/// One scrollable column of the debug panel (call stack / variables / output).
pub(crate) fn debug_col(rows: Vec<Element<'_, Message>>) -> Element<'_, Message> {
    container(
        scrollable(Column::with_children(rows).spacing(1).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    )
        .width(Fill)
        .height(Fill)
        .padding([4, 6])
        .into()
}

/// The bottom debugger panel: status + step controls, and three columns —
/// call stack (click a frame to jump), variables, and program output.
/// The collapsible bottom panel: a tab bar (Ask / Debug, like the left sidebar)
/// with a collapse control, above the selected tab's content.
pub(crate) fn bottom_panel(app: &App) -> Element<'_, Message> {
    use crate::BottomTab;
    let tab = |g: Glyph, label: &'static str, this: BottomTab| {
        let active = app.bottom_tab == this;
        let tint = if active { theme::FG_BRIGHT } else { theme::FG_MUTED };
        button(
            row![glyph::icon(g, tint, 16.0), text(label).size(11)]
                .spacing(6)
                .align_y(iced::Center),
        )
        .style(theme::tab_button(active))
        .padding([5, 12])
        .on_press(Message::BottomTabPicked(this))
    };
    // A borderless "hide" affordance — no button box, just a chevron that
    // brightens on hover.
    let collapse = button(text("⌄").size(13))
        .style(theme::list_row(false))
        .padding([3, 10])
        .on_press(Message::CollapseBottom);
    let tabs = row![
        tab(Glyph::Ask, "Ask", BottomTab::Ask),
        tab(Glyph::Debug, "Debug", BottomTab::Debug),
        space().width(Fill),
        collapse,
    ]
    .spacing(2)
    .align_y(iced::Center)
    .padding(Padding { top: 2.0, right: 6.0, bottom: 2.0, left: 6.0 });

    let content: Element<'_, Message> = match app.bottom_tab {
        BottomTab::Ask => ask_panel(app),
        BottomTab::Debug if app.debug.session.is_some() => debug_panel(app),
        BottomTab::Debug => empty_state(
            Glyph::Debug,
            "No debug session",
            "Press Debug in the toolbar to start one (needs .clew/launch.json).",
            None,
        ),
    };

    container(column![tabs, hairline(), content].height(Fill))
        .height(Fill)
        .style(theme::panel)
        .into()
}

pub(crate) fn debug_panel(app: &App) -> Element<'_, Message> {
    use crate::{DebugCmd, DebugStatus};
    let Some(session) = app.debug.session.as_ref() else {
        return space().into();
    };
    let (status_txt, status_color) = match session.status {
        DebugStatus::Launching => ("launching…", theme::DIM),
        DebugStatus::Running => ("running", theme::rgb(0x98c379)),
        DebugStatus::Stopped => ("stopped", theme::rgb(0xe5c07b)),
        DebugStatus::Terminated => ("terminated", theme::DIM),
    };
    let stopped = session.status == DebugStatus::Stopped;

    let ctrl = |label: &'static str, msg: Message, enabled: bool| {
        let mut b = button(text(label).size(12)).style(theme::toolbar_button).padding([2, 8]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let controls = row![
        ctrl("▶ Continue", Message::DebugControl(DebugCmd::Continue), stopped),
        ctrl("⤼ Over", Message::DebugControl(DebugCmd::StepOver), stopped),
        ctrl("⤓ In", Message::DebugControl(DebugCmd::StepIn), stopped),
        ctrl("⤒ Out", Message::DebugControl(DebugCmd::StepOut), stopped),
        ctrl("■ Stop", Message::DebugStop, true),
    ]
    .spacing(4);
    let header = row![
        text("Debug").size(13).color(theme::FG),
        text(status_txt).size(11).color(status_color),
        space().width(Fill),
        controls,
    ]
    .spacing(8)
    .align_y(iced::Center);

    // Call stack — click a frame to jump to its source.
    let mut stack_rows: Vec<Element<'_, Message>> =
        vec![text("CALL STACK").size(10).color(theme::DIM).into()];
    for f in &session.frames {
        let loc = f
            .path
            .as_ref()
            .map(|p| format!("{}:{}", rel_of(app, p), f.line))
            .unwrap_or_default();
        let mut b = button(
            column![
                text(f.name.clone()).size(11).color(theme::ACCENT).wrapping(Wrapping::None),
                text(loc).size(9).color(theme::DIM).wrapping(Wrapping::None),
            ]
            .spacing(0),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 6]);
        if let Some(p) = f.path.clone() {
            b = b.on_press(Message::OverlayOpenAt { abs: p, line: f.line });
        }
        stack_rows.push(b.into());
    }

    // Variables — each scope with its name = value rows.
    let mut var_rows: Vec<Element<'_, Message>> =
        vec![text("VARIABLES").size(10).color(theme::DIM).into()];
    for sc in &session.scopes {
        var_rows.push(text(sc.name.clone()).size(10).color(theme::DIM).into());
        for v in &sc.vars {
            var_rows.push(
                row![
                    text(v.name.clone()).size(11).color(theme::rgb(0xe5c07b)),
                    text(" = ").size(11).color(theme::DIM),
                    text(v.value.clone()).size(11).color(theme::FG).wrapping(Wrapping::None),
                ]
                .into(),
            );
        }
    }

    // Program output.
    let mut out_rows: Vec<Element<'_, Message>> =
        vec![text("OUTPUT").size(10).color(theme::DIM).into()];
    for (cat, txt) in &session.output {
        let color = if cat == "stderr" { theme::rgb(0xe06c75) } else { theme::FG };
        out_rows.push(
            text(txt.trim_end_matches('\n').to_string())
                .size(11)
                .color(color)
                .wrapping(Wrapping::None)
                .into(),
        );
    }

    // Watch — expressions re-evaluated each stop, with an add box + remove.
    let mut watch_rows: Vec<Element<'_, Message>> =
        vec![text("WATCH").size(10).color(theme::DIM).into()];
    watch_rows.push(
        text_input("Add watch…", &app.debug.watch_input)
            .on_input(Message::DebugWatchInput)
            .on_submit(Message::DebugWatchAdd)
            .size(11)
            .padding(3)
            .into(),
    );
    for (i, expr) in app.debug.watches.iter().enumerate() {
        let val = session
            .watches
            .iter()
            .find(|(e, _)| e == expr)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "…".into());
        watch_rows.push(
            row![
                button(text("✕").size(9).color(theme::DIM))
                    .style(theme::list_row(false))
                    .padding([0, 4])
                    .on_press(Message::DebugWatchRemove(i)),
                text(expr.clone()).size(11).color(theme::ACCENT),
                text(" = ").size(11).color(theme::DIM),
                text(val).size(11).color(theme::FG).wrapping(Wrapping::None),
            ]
            .spacing(2)
            .align_y(iced::Center)
            .into(),
        );
    }

    let panels = row![
        debug_col(stack_rows),
        debug_col(var_rows),
        debug_col(watch_rows),
        debug_col(out_rows)
    ]
    .spacing(6)
    .height(Fill);

    container(column![header, panels].spacing(6).padding([8, 12]))
        .width(Fill)
        .height(Fill)
        .style(theme::panel)
        .into()
}

pub(crate) fn ask_panel(app: &App) -> Element<'_, Message> {
    // The tab bar already names the panel and offers collapse, so the only header
    // control left is "Clear", and only once there's a conversation to clear.
    let header: Element<'_, Message> = if app.ask_turns.is_empty() {
        space().height(0).into()
    } else {
        row![
            space().width(Fill),
            button(text("Clear").size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::AskClear),
        ]
        .align_y(iced::Center)
        .into()
    };

    let mut convo: Vec<Element<'_, Message>> = Vec::new();
    if app.ask_turns.is_empty() && !app.asking {
        convo.push(
            text("Ask a question about this codebase. Answers cite the code and jump to it. \
                  Follow-ups keep the conversation. Select code and right-click → “Add to Ask” \
                  to attach snippets as context.")
                .size(12)
                .color(theme::DIM)
                .into(),
        );
        // Answers are grounded in the semantic index (or pinned snippets). Say so
        // upfront when neither exists, rather than only rejecting on submit — so
        // this matches how Overview/FIND show their "run Explain All first" state.
        if app.embed_index.entries.is_empty() && app.ask_pins.is_empty() {
            convo.push(
                text("Run “Explain All” to ground answers in the code — or right-click code → \
                      “Add to Ask” to ground a single question now.")
                    .size(11)
                    .color(theme::WARN)
                    .into(),
            );
        }
        // Context-aware starter questions — click one to ask it.
        let suggestions = app.suggested_questions();
        if !suggestions.is_empty() {
            convo.push(text("Try asking").size(10).color(theme::DIM).into());
            let chips: Vec<Element<'_, Message>> = suggestions
                .into_iter()
                .map(|q| {
                    button(text(strip_backticks(&q)).size(11).color(theme::ACCENT))
                        .style(theme::list_row(false))
                        .padding([3, 8])
                        .on_press(Message::AskSuggested(q))
                        .into()
                })
                .collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    for turn in &app.ask_turns {
        convo.push(text(format!("❯ {}", turn.question)).size(13).color(theme::ACCENT).into());
        if turn.streaming {
            // Live answer: "Thinking…" until the first token, then the raw text
            // (with a cursor) as it streams; it's re-rendered richly when done.
            if turn.answer_md.trim().is_empty() {
                convo.push(text("Thinking…").size(12).color(theme::DIM).into());
            } else {
                convo.push(
                    text(format!("{}▍", turn.answer_md))
                        .size(13)
                        .color(theme::FG)
                        .into(),
                );
            }
        } else {
            convo.extend(render_prepared(app, &turn.answer));
        }
        if !turn.sources.is_empty() {
            convo.push(text("Sources").size(10).color(theme::DIM).into());
            let chips: Vec<Element<'_, Message>> =
                turn.sources.iter().map(|(n, s)| source_chip(n, *s)).collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    // Retrieval phase (before the answer turn exists) shows a spinner line.
    if app.asking {
        convo.push(text("Thinking…").size(12).color(theme::DIM).into());
    }
    let conversation =
        scrollable(Column::with_children(convo).spacing(8).width(Fill))
            .id(ask_scroll_id())
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill);

    // Compose area: the pinned-selection chips (each a clickable jump + remove)
    // above the input row. Chips persist across turns and wrap when there are
    // several.
    let mut compose: Vec<Element<'_, Message>> = Vec::new();
    if !app.ask_pins.is_empty() {
        let chips: Vec<Element<'_, Message>> = app
            .ask_pins
            .iter()
            .enumerate()
            .map(|(i, pin)| {
                container(
                    row![
                        button(text(format!("📎 {} · L{}", pin.rel, pin.line)).size(11).color(theme::ACCENT))
                            .style(theme::toolbar_button)
                            .padding([0, 4])
                            .on_press(Message::AskPinGoto(i)),
                        button(text("✕").size(11).color(theme::DIM))
                            .style(theme::toolbar_button)
                            .padding([0, 6])
                            .on_press(Message::AskUnpin(i)),
                    ]
                    .spacing(2)
                    .align_y(iced::Center),
                )
                .padding([1, 2])
                .style(theme::panel)
                .into()
            })
            .collect();
        compose.push(Row::with_children(chips).spacing(4).wrap().into());
    }
    let input = text_input("Ask about this codebase…", &app.ask_input)
        .id(ask_input_id())
        .on_input(Message::AskInputChanged)
        .on_submit(Message::AskSubmit)
        .size(13)
        .padding(7);
    // Match the input's height (size 13 + 7 padding) so the row lines up. The
    // send button is the panel's primary action, so it gets accent emphasis
    // (dimmed to a plain style while a request is in flight / disabled).
    let idle = !app.asking;
    let mut ask_btn = button(text("Ask").size(13))
        .style(if idle { theme::primary_button } else { theme::toolbar_button })
        .padding([7, 16]);
    if idle {
        ask_btn = ask_btn.on_press(Message::AskSubmit);
    }
    compose.push(row![input, ask_btn].spacing(6).align_y(iced::Center).into());

    container(
        column![header, conversation, Column::with_children(compose).spacing(4)]
            .spacing(8)
            .padding([8, 12]),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::panel)
    .into()
}

/// The call-hierarchy tree: a header with the root symbol + a callers/callees
/// toggle, then the lazily-expanded tree.
pub(crate) fn calls_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.call_graph else {
        return empty_state(
            Glyph::CallGraph,
            "No call hierarchy yet",
            "Put the cursor on a function and press gc, or right-click it → Call Hierarchy.",
            None,
        );
    };

    let header = container(
        row![
            text(&tree.root_name)
                .size(12)
                .color(theme::ACCENT)
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(11))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::CallHierarchyExpandAll),
            button(text(tree.direction.label()).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::CallHierarchyDirection),
        ]
        .spacing(4)
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 6.0,
        right: 8.0,
        bottom: 6.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for id in tree.visible() {
        let node = tree.node(id);
        // Expansion affordance: an arrow for fetchable nodes, a loop glyph for
        // recursion, blank for a leaf with no further calls.
        let arrow: Element<'_, Message> = if node.loading {
            text("…").size(11).color(theme::ACCENT).width(16).into()
        } else if node.cyclic {
            text("↺").size(11).color(theme::DIM).width(16).into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(text(if node.expanded { "▾" } else { "▸" }).size(11).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([0, 3])
                .on_press(Message::CallHierarchyExpand(id))
                .into()
        };

        let kind = kind_short(node.item.kind);
        let fname = node
            .item
            .path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let name_btn = button(
            row![
                text(&node.item.name).size(12).wrapping(Wrapping::None),
                space().width(6),
                text(format!("{fname}:{}", node.item.line + 1))
                    .size(10)
                    .color(theme::DIM)
                    .wrapping(Wrapping::None),
            ]
            .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 4])
        .on_press(Message::OpenAbs {
            abs: node.item.path.clone(),
            line: Some(node.item.line + 1),
            push: true,
        });

        let badge = text(kind).size(9).color(theme::DIM).width(if kind.is_empty() {
            0.0
        } else {
            22.0
        });

        rows.push(
            row![
                space().width(node.depth as f32 * 12.0),
                arrow,
                badge,
                name_btn,
            ]
            .spacing(2)
            .align_y(iced::Center)
            .into(),
        );
    }

    let mut col = column![header];
    if tree.stale {
        col = col.push(
            container(
                text("⟳ code changed — press gc to refresh")
                    .size(10)
                    .color(theme::rgb(0xe5c07b)),
            )
            .padding(Padding {
                top: 3.0,
                right: 8.0,
                bottom: 3.0,
                left: 10.0,
            })
            .width(Fill),
        );
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

/// The import tree: a header with the focus file + an Imports/Importers toggle,
/// a cycles banner, then the lazily-expanded (but synchronous) tree.
pub(crate) fn imports_tab(app: &App) -> Element<'_, Message> {
    use crate::imports::Target;

    let Some(tree) = &app.import_tree else {
        return container(
            column![
                text("No file focused.").size(12).color(theme::DIM),
                space().height(6),
                text("Open a source file to see what it")
                    .size(11)
                    .color(theme::DIM),
                text("imports and what imports it.")
                    .size(11)
                    .color(theme::DIM),
            ]
            .spacing(2),
        )
        .padding(12)
        .into();
    };

    let header = container(
        row![
            text(&tree.root_name)
                .size(12)
                .color(theme::ACCENT)
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(11))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::ImportExpandAll),
            button(text(tree.direction.label()).size(11))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::ImportDirection),
        ]
        .spacing(4)
        .align_y(iced::Center),
    )
    .padding(Padding {
        top: 6.0,
        right: 8.0,
        bottom: 6.0,
        left: 10.0,
    })
    .style(theme::pane_header)
    .width(Fill);

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for id in tree.visible() {
        let node = tree.node(id);
        // Expansion affordance: a loop glyph for a cycle, blank for a leaf
        // (external/unresolved, or an already-expanded internal with no edges),
        // an arrow otherwise.
        let arrow: Element<'_, Message> = if node.cyclic {
            text("↺").size(11).color(theme::DIM).width(16).into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(text(if node.expanded { "▾" } else { "▸" }).size(11).color(theme::DIM))
                .style(theme::list_row(false))
                .padding([0, 3])
                .on_press(Message::ImportExpand(id))
                .into()
        };

        // Internal files open on click; external/unresolved are dim leaves.
        let name: Element<'_, Message> = match &node.target {
            Target::Internal(path) => button(
                row![
                    text(&node.label).size(12).wrapping(Wrapping::None),
                    space().width(6),
                    text(&node.detail).size(10).color(theme::DIM).wrapping(Wrapping::None),
                ]
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([1, 4])
            .on_press(Message::OpenAbs {
                abs: path.clone(),
                line: None,
                push: true,
            })
            .into(),
            Target::External(_) => container(
                row![
                    text(&node.label).size(12).color(theme::DIM).wrapping(Wrapping::None),
                    space().width(6),
                    text("ext").size(9).color(theme::DIM),
                ]
                .align_y(iced::Center),
            )
            .padding([1, 4])
            .width(Fill)
            .into(),
            Target::Unresolved(_) => container(
                row![
                    text(&node.label).size(12).color(theme::DIM).wrapping(Wrapping::None),
                    space().width(6),
                    text("?").size(10).color(theme::DIM),
                ]
                .align_y(iced::Center),
            )
            .padding([1, 4])
            .width(Fill)
            .into(),
        };

        rows.push(
            row![space().width(node.depth as f32 * 12.0), arrow, name]
                .spacing(2)
                .align_y(iced::Center)
                .into(),
        );
    }

    let mut col = column![header];
    if !app.import_cycles.is_empty() {
        let n = app.import_cycles.len();
        col = col.push(
            container(
                text(format!(
                    "⚠ {n} import cycle{}",
                    if n == 1 { "" } else { "s" }
                ))
                .size(10)
                .color(theme::rgb(0xe5c07b)),
            )
            .padding(Padding {
                top: 3.0,
                right: 8.0,
                bottom: 3.0,
                left: 10.0,
            })
            .width(Fill),
        );
    }
    col.push(scrollable(Column::with_children(rows).width(Fill)).direction(thin_scroll()).style(theme::overlay_scrollbar).height(Fill))
        .into()
}

