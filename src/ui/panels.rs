//! Bottom panels (debug/ask) and the calls/imports tabs.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

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

/// The collapsible bottom panel: a tab bar (Ask / Debug, like the left sidebar)
/// with a collapse control, above the selected tab's content.
pub(crate) fn bottom_panel(app: &App) -> Element<'_, Message> {
    use crate::BottomTab;
    let tab = |g: Glyph, label: &'static str, this: BottomTab| {
        let active = app.bottom_tab == this;
        let tint = if active {
            theme::fg_bright()
        } else {
            theme::fg_muted()
        };
        button(
            row![glyph::icon(g, tint, 16.0), text(label).size(ts::SMALL)]
                .spacing(6)
                .align_y(iced::Center),
        )
        .style(theme::tab_button(active))
        .padding([5, 12])
        .on_press(Message::Window(WindowMsg::BottomTabPicked(this)))
    };
    // A borderless "hide" affordance — no button box, just a chevron that
    // brightens on hover.
    let collapse = button(text("⌄").size(ts::BASE))
        .style(theme::list_row(false))
        .padding([3, 10])
        .on_press(Message::Window(WindowMsg::CollapseBottom));
    let tabs = row![
        tab(Glyph::Ask, "Ask", BottomTab::Ask),
        tab(Glyph::Debug, "Debug", BottomTab::Debug),
        space().width(Fill),
        collapse,
    ]
    .spacing(2)
    .align_y(iced::Center)
    .padding(Padding {
        top: 2.0,
        right: 6.0,
        bottom: 2.0,
        left: 6.0,
    });

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

/// The bottom debugger panel: status + step controls, and four columns —
/// call stack (click a frame to jump), variables, watches, and program output.
pub(crate) fn debug_panel(app: &App) -> Element<'_, Message> {
    use crate::{DebugCmd, DebugStatus};
    let Some(session) = app.debug.session.as_ref() else {
        return space().into();
    };
    let (status_txt, status_color) = match session.status {
        DebugStatus::Launching => ("launching…", theme::dim()),
        DebugStatus::Running => ("running", theme::success()),
        DebugStatus::Stopped => ("stopped", theme::warning()),
        DebugStatus::Terminated => ("terminated", theme::dim()),
    };
    let stopped = session.status == DebugStatus::Stopped;

    let ctrl = |label: &'static str, msg: Message, enabled: bool| {
        let mut b = button(text(label).size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([2, 8]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let controls = row![
        ctrl(
            "▶ Continue",
            Message::Debug(DebugMsg::Control(DebugCmd::Continue)),
            stopped
        ),
        ctrl(
            "⤼ Over",
            Message::Debug(DebugMsg::Control(DebugCmd::StepOver)),
            stopped
        ),
        ctrl(
            "⤓ In",
            Message::Debug(DebugMsg::Control(DebugCmd::StepIn)),
            stopped
        ),
        ctrl(
            "⤒ Out",
            Message::Debug(DebugMsg::Control(DebugCmd::StepOut)),
            stopped
        ),
        ctrl("■ Stop", Message::Debug(DebugMsg::Stop), true),
    ]
    .spacing(4);
    let header = row![
        text("Debug").size(ts::BASE).color(theme::fg()),
        text(status_txt).size(ts::SMALL).color(status_color),
        space().width(Fill),
        trace_control(app),
        hover_eval_control(app),
        controls,
    ]
    .spacing(8)
    .align_y(iced::Center);

    // Call stack — click a frame to jump to its source.
    let mut stack_rows: Vec<Element<'_, Message>> = vec![
        text("CALL STACK")
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
    ];
    for f in &session.frames {
        let loc = f
            .path
            .as_ref()
            .map(|p| format!("{}:{}", rel_of(app, p), f.line))
            .unwrap_or_default();
        let mut b = button(
            column![
                text(f.name.clone())
                    .size(ts::SMALL)
                    .color(theme::accent())
                    .wrapping(Wrapping::None),
                text(loc)
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .spacing(0),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 6]);
        if let Some(p) = f.path.clone() {
            b = b.on_press(Message::Graph(GraphMsg::OverlayOpenAt {
                abs: p,
                line: f.line,
            }));
        }
        stack_rows.push(b.into());
    }

    // Variables — each scope with its name = value rows.
    let mut var_rows: Vec<Element<'_, Message>> = vec![
        text("VARIABLES")
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
    ];
    for sc in &session.scopes {
        var_rows.push(
            text(sc.name.clone())
                .size(ts::CAPTION)
                .color(theme::dim())
                .into(),
        );
        for v in &sc.vars {
            var_rows.push(
                row![
                    text(v.name.clone()).size(ts::SMALL).color(theme::warning()),
                    text(" = ").size(ts::SMALL).color(theme::dim()),
                    text(v.value.clone())
                        .size(ts::SMALL)
                        .color(theme::fg())
                        .wrapping(Wrapping::None),
                ]
                .into(),
            );
        }
    }

    // Program output.
    let mut out_rows: Vec<Element<'_, Message>> =
        vec![text("OUTPUT").size(ts::CAPTION).color(theme::dim()).into()];
    for (cat, txt) in &session.output {
        let color = if cat == "stderr" {
            theme::danger()
        } else {
            theme::fg()
        };
        out_rows.push(
            text(txt.trim_end_matches('\n').to_string())
                .size(ts::SMALL)
                .color(color)
                .wrapping(Wrapping::None)
                .into(),
        );
    }

    // Watch — expressions re-evaluated each stop, with an add box + remove.
    let mut watch_rows: Vec<Element<'_, Message>> =
        vec![text("WATCH").size(ts::CAPTION).color(theme::dim()).into()];
    watch_rows.push(
        text_input("Add watch…", &app.debug.watch_input)
            .on_input(|v| Message::Debug(DebugMsg::WatchInput(v)))
            .on_submit(Message::Debug(DebugMsg::WatchAdd))
            .size(ts::SMALL)
            .padding(3)
            .into(),
    );
    for expr in &app.debug.watches {
        let val = session
            .watches
            .iter()
            .find(|(e, _)| e == expr)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "…".into());
        watch_rows.push(
            row![
                // By identity (the expression): an index into the list drawn
                // here can name another watch once the list changes.
                button(text("✕").size(ts::CAPTION).color(theme::dim()))
                    .style(theme::list_row(false))
                    .padding([0, 4])
                    .on_press(Message::Debug(DebugMsg::WatchRemoveExpr(expr.clone()))),
                text(expr.clone()).size(ts::SMALL).color(theme::accent()),
                text(" = ").size(ts::SMALL).color(theme::dim()),
                text(val)
                    .size(ts::SMALL)
                    .color(theme::fg())
                    .wrapping(Wrapping::None),
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

/// Whether hovering a name while paused evaluates it (see
/// `dap::hover_eval_allowed`). An adapter that promised side-effect-free
/// hovers needs no switch — the header just says hovers show values. For any
/// other adapter an evaluation may run the program's own code, so it is a
/// checkbox the reader has to tick, with the risk named beside it.
/// The run's trace so far — how many stops it holds — and the button that
/// turns it into a walkthrough of the path the program took.
pub(crate) fn trace_control(app: &App) -> Element<'_, Message> {
    let n = app.debug.trace.len();
    if n == 0 {
        return text("trace: no stops yet")
            .size(ts::CAPTION)
            .color(theme::dim())
            .into();
    }
    let count = format!(
        "trace: {n} {}{}",
        if n == 1 { "stop" } else { "stops" },
        if app.debug.trace_cut { " (cut)" } else { "" }
    );
    row![
        text(count).size(ts::CAPTION).color(theme::dim()),
        button(text("Walk this run").size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(Message::Walk(WalkMsg::GenerateTrace)),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .into()
}

pub(crate) fn hover_eval_control(app: &App) -> Element<'_, Message> {
    if app.debug.hover_safe {
        return text("hover shows values")
            .size(ts::CAPTION)
            .color(theme::dim())
            .into();
    }
    tooltip(
        iced::widget::checkbox(app.debug_hover_eval)
            .label("Evaluate on hover")
            .on_toggle(|_| Message::Debug(DebugMsg::ToggleHoverEval))
            .size(14)
            .text_size(ts::SMALL)
            .spacing(6),
        container(
            text(
                "This adapter does not promise side-effect-free hovers: evaluating a \
                 hovered name can run the program's own code (getters, Debug/toString).",
            )
            .size(ts::SMALL)
            .color(theme::fg()),
        )
        .max_width(TIP_MAX_W)
        .padding([4, 8])
        .style(theme::modal_panel),
        tooltip::Position::Bottom,
    )
    .into()
}

/// A dim note row in a lazily-expanded tree, indented under the node it is
/// about: what the tree's size cap left out ("12 more callers not shown").
/// Without it a node cut short reads as if it had no more callers.
fn tree_note_row<'a>(depth: usize, note: String) -> Element<'a, Message> {
    container(
        text(note)
            .size(ts::CAPTION)
            .color(theme::warning())
            .wrapping(Wrapping::None),
    )
    .padding(Padding {
        top: 0.0,
        right: 4.0,
        bottom: 2.0,
        // The note sits where the node's children start: past its indent,
        // its arrow slot and its badge.
        left: depth as f32 * 12.0 + 16.0 + 6.0,
    })
    .width(Fill)
    .into()
}

/// A one-line banner under a tree's header (cycles, what the index left out).
fn tree_banner<'a>(note: String, color: iced::Color) -> Element<'a, Message> {
    container(text(note).size(ts::CAPTION).color(color))
        .padding(Padding {
            top: 3.0,
            right: 8.0,
            bottom: 3.0,
            left: 10.0,
        })
        .width(Fill)
        .into()
}

/// What the symbol index left out of the open project (its file / size caps),
/// or `None` when it is complete. (A note from a project since left went with
/// that project's session.)
pub(crate) fn index_cap_note(app: &App) -> Option<&str> {
    app.proj.index_cap_note.as_deref()
}

/// The "Ask clew" bottom panel: a scrollable multi-turn Q&A over a question box.
/// Answers are grounded in retrieved code, cite it with jump links, and list
/// their retrieved sources as clickable chips.
pub(crate) fn ask_panel(app: &App) -> Element<'_, Message> {
    // The tab bar already names the panel and offers collapse, so the only header
    // control left is "Clear", and only once there's a conversation to clear.
    let header: Element<'_, Message> = if app.proj.ask_turns.is_empty() {
        space().height(0).into()
    } else {
        row![
            space().width(Fill),
            button(text("Clear").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Ask(AskMsg::Clear)),
        ]
        .align_y(iced::Center)
        .into()
    };

    let mut convo: Vec<Element<'_, Message>> = Vec::new();
    if app.proj.ask_turns.is_empty() && !app.proj.asking {
        convo.push(
            text(
                "Ask a question about this codebase. Answers cite the code and jump to it. \
                  Follow-ups keep the conversation. Select code and right-click → “Add to Ask” \
                  to attach snippets as context.",
            )
            .size(ts::BODY)
            .color(theme::dim())
            .into(),
        );
        // Agent mode explores the project itself, so it needs no semantic
        // index — the "build the index first" nudge only applies when Ask
        // would run in retrieval mode (no server channel to run an agent on).
        if !app.server.is_up()
            && app.proj.embed_index.entries.is_empty()
            && app.proj.ask_pins.is_empty()
        {
            convo.push(
                text(
                    "Run “Explain All” to ground answers in the code — or right-click code → \
                      “Add to Ask” to ground a single question now.",
                )
                .size(ts::SMALL)
                .color(theme::warn())
                .into(),
            );
        }
        // Context-aware starter questions — click one to ask it.
        let suggestions = app.suggested_questions();
        if !suggestions.is_empty() {
            convo.push(
                text("Try asking")
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .into(),
            );
            let chips: Vec<Element<'_, Message>> = suggestions
                .into_iter()
                .map(|q| {
                    button(
                        text(strip_backticks(&q))
                            .size(ts::SMALL)
                            .color(theme::accent()),
                    )
                    .style(theme::list_row(false))
                    .padding([3, 8])
                    .on_press(Message::Ask(AskMsg::Suggested(q)))
                    .into()
                })
                .collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    for turn in &app.proj.ask_turns {
        convo.push(
            text(format!("❯ {}", turn.question))
                .size(ts::BASE)
                .color(theme::accent())
                .into(),
        );
        // Agent exploration: one chip per tool call, clickable when the step
        // touched a code location (jump to the first ref).
        if !turn.steps.is_empty() {
            let root = app.proj.project.as_ref().map(|p| p.root.clone());
            let chips: Vec<Element<'_, Message>> = turn
                .steps
                .iter()
                .map(|s| agent_step_chip(s, root.as_deref()))
                .collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
        if turn.streaming {
            // Live answer: "Thinking…" until the first token, then the raw text
            // (with a cursor) as it streams; it's re-rendered richly when done.
            if turn.answer_md.trim().is_empty() {
                let label = if turn.steps.is_empty() {
                    "Thinking…"
                } else {
                    "Exploring…"
                };
                convo.push(text(label).size(ts::BODY).color(theme::dim()).into());
            } else {
                convo.push(
                    text(format!("{}▍", turn.answer_md))
                        .size(ts::BASE)
                        .color(theme::fg())
                        .into(),
                );
            }
        } else {
            convo.extend(render_prepared(app, &turn.answer));
        }
        if !turn.sources.is_empty() {
            convo.push(text("Sources").size(ts::CAPTION).color(theme::dim()).into());
            let chips: Vec<Element<'_, Message>> = turn
                .sources
                .iter()
                .map(|(n, s)| source_chip(n, *s))
                .collect();
            convo.push(Row::with_children(chips).spacing(4).wrap().into());
        }
    }
    // Retrieval phase (before the answer turn exists) shows a spinner line.
    if app.proj.asking {
        convo.push(text("Thinking…").size(ts::BODY).color(theme::dim()).into());
    }
    let conversation = scrollable(Column::with_children(convo).spacing(8).width(Fill))
        .id(ask_scroll_id())
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill);

    // Compose area: the pinned-selection chips (each a clickable jump + remove)
    // above the input row. Chips persist across turns and wrap when there are
    // several.
    let mut compose: Vec<Element<'_, Message>> = Vec::new();
    if !app.proj.ask_pins.is_empty() {
        let chips: Vec<Element<'_, Message>> = app
            .proj
            .ask_pins
            .iter()
            .map(|pin| {
                // By identity: removing one chip shifts every index after it.
                let key = pin.key();
                container(
                    row![
                        button(
                            text(format!("📎 {} · L{}", pin.rel, pin.line))
                                .size(ts::SMALL)
                                .color(theme::accent())
                        )
                        .style(theme::toolbar_button)
                        .padding([0, 4])
                        .on_press(Message::Ask(AskMsg::PinGoto(key))),
                        button(text("✕").size(ts::SMALL).color(theme::dim()))
                            .style(theme::toolbar_button)
                            .padding([0, 6])
                            .on_press(Message::Ask(AskMsg::Unpin(key))),
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
    let input = text_input("Ask about this codebase…", &app.proj.ask_input)
        .id(ask_input_id())
        .on_input(|v| Message::Ask(AskMsg::InputChanged(v)))
        .on_submit(Message::Ask(AskMsg::Submit))
        .size(ts::BASE)
        .padding(7);
    // Match the input's height (size 13 + 7 padding) so the row lines up. The
    // send button is the panel's primary action, so it gets accent emphasis
    // (dimmed to a plain style while a request is in flight / disabled).
    // Any answer still coming in — an agent turn, a server chat stream, a local
    // retrieval stream — can be stopped from here, not only agent turns.
    let streaming = app.ask_stream_active();
    let idle = !streaming;
    let mut ask_btn = button(text("Ask").size(ts::BASE))
        .style(if idle {
            theme::primary_button
        } else {
            theme::toolbar_button
        })
        .padding([7, 16]);
    if idle {
        ask_btn = ask_btn.on_press(Message::Ask(AskMsg::Submit));
    }
    let mut compose_row = row![input].spacing(6).align_y(iced::Center);
    if streaming {
        // An answer can run long — always give the user a way out.
        compose_row = compose_row.push(
            button(text("Stop").size(ts::BASE))
                .style(theme::toolbar_button)
                .padding([7, 12])
                .on_press(Message::Ask(AskMsg::Stop)),
        );
    }
    compose.push(compose_row.push(ask_btn).into());

    container(
        column![
            header,
            conversation,
            Column::with_children(compose).spacing(4)
        ]
        .spacing(8)
        .padding([8, 12]),
    )
    .width(Fill)
    .height(Fill)
    .style(theme::panel)
    .into()
}

/// One agent exploration step as a chip: a tool glyph + its one-line title.
/// Clickable when the step touched code (jumps to the first ref).
fn agent_step_chip<'a>(
    step: &crate::app::model::AgentStep,
    root: Option<&std::path::Path>,
) -> Element<'a, Message> {
    let icon = match step.tool.as_str() {
        "search" | "semantic_find" => "🔍",
        "read" => "📄",
        "outline" => "☰",
        "files" => "🗂",
        "history" => "🕘",
        "changes" => "±",
        "explanations" => "✦",
        _ => "⚙",
    };
    // `join` discards the root for an absolute argument, so a ref that is not
    // a plain in-project rel would make this chip open somewhere else entirely.
    // No current producer can emit one — the LSP-backed branch drops targets
    // whose path does not strip the project root (they are stdlib or dependency
    // sources, shown as text with no chip), and every other tool `confine`s the
    // rel it was given — so this is the lexical backstop for that invariant,
    // not a live escape. A ref that fails it leaves the chip unclickable.
    //
    // The leading `./` is stripped first because the two predicates on this
    // path disagree about it: the server's `confine` permits `Component::CurDir`
    // and so runs the tool and answers, while `safe_rel` requires every
    // component to be `Normal`. A tool arm stores the model's raw argument as
    // the ref, so `./src/lib.rs` reached here and was refused — a working jump
    // turned into a dead chip. `resolve_project_link` already normalizes the
    // same way for the same reason.
    let target = root.and_then(|root| {
        step.refs
            .first()
            .map(|(rel, line)| (rel.trim_start_matches("./"), line))
            .filter(|(rel, _)| clew_core::statefile::safe_rel(rel))
            .map(|(rel, line)| (root.join(rel), *line))
    });
    let clickable = target.is_some();
    let mut chip = button(
        text(format!("{icon} {}", step.title))
            .size(ts::CAPTION)
            .color(if clickable {
                theme::fg_muted()
            } else {
                theme::dim()
            }),
    )
    .style(theme::toolbar_button)
    .padding([1, 6]);
    if let Some((abs, line)) = target {
        chip = chip.on_press(Message::Editor(EditorMsg::OpenAbs {
            abs,
            line,
            push: true,
        }));
    }
    chip.into()
}

/// The call-hierarchy tree: a header with the root symbol + a callers/callees
/// toggle, then the lazily-expanded tree.
/// The FLOW tab: the traced identifier's occurrences under the role each
/// line gives it (declared, assigned, parameter, passed to, returned,
/// branched on, member access, read), each opening its line; a `Passed`
/// row unfolds into the callee's parameter and ITS occurrences.
pub(crate) fn flow_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.proj.flow else {
        return empty_state(
            Glyph::Search,
            "No value trace yet",
            "Right-click an identifier → Trace Value to see where it is set, passed and returned.",
            None,
        );
    };
    let pending = app.proj.flow_pending == Some(tree.token);
    let header = container(
        row![
            text(format!("`{}`", tree.symbol))
                .size(ts::BODY)
                .color(theme::accent())
                .wrapping(Wrapping::None),
            text(if pending {
                "tracing…".to_string()
            } else {
                format!("{} places", tree.node_count())
            })
            .size(ts::CAPTION)
            .color(theme::dim()),
            space().width(Fill),
            button(text("clear").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::Flow(crate::FlowMsg::Clear)),
        ]
        .spacing(6)
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
    if tree.stale {
        rows.push(
            container(
                row![
                    text("A traced file changed: rows marked changed are where their line was.")
                        .size(ts::CAPTION)
                        .color(theme::warning())
                        .width(Fill),
                    button(text("trace again").size(ts::SMALL))
                        .style(theme::toolbar_button)
                        .padding([2, 7])
                        .on_press(Message::Flow(crate::FlowMsg::Retrace)),
                ]
                .spacing(6)
                .align_y(iced::Center),
            )
            .padding([4, 10])
            .into(),
        );
    }
    if let Some(note) = &tree.note {
        rows.push(
            container(text(note).size(ts::CAPTION).color(theme::dim()))
                .padding([2, 10])
                .into(),
        );
    }
    let token = tree.token;
    for (role, ids) in tree.grouped_roots() {
        rows.push(
            container(
                text(role.heading())
                    .size(ts::CAPTION)
                    .color(theme::fg_muted()),
            )
            .padding(Padding {
                top: 8.0,
                right: 10.0,
                bottom: 2.0,
                left: 10.0,
            })
            .into(),
        );
        for root in ids {
            for id in tree.visible_under(root) {
                rows.push(flow_row(tree, id, token));
            }
        }
    }
    let list = scrollable(Column::with_children(rows).width(Fill))
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(Fill);
    column![header, list].height(Fill).into()
}

/// One occurrence: its role tag, file and line, the line's text; a
/// `Passed` row's unfold button follows the value into the callee.
fn flow_row(tree: &crate::app::flow::FlowTree, id: usize, token: u64) -> Element<'_, Message> {
    let node = tree.node(id);
    let indent = 10.0 + 14.0 * node.depth as f32;
    let unfold: Element<'_, Message> = if node.loading {
        text("…")
            .size(ts::SMALL)
            .color(theme::accent())
            .width(16)
            .into()
    } else if node.role == crate::app::flow::Role::Passed && node.callee.is_some() {
        button(
            text(if node.expanded { "▾" } else { "▸" })
                .size(ts::SMALL)
                .color(theme::dim()),
        )
        .style(theme::list_row(false))
        .padding([0, 3])
        .on_press(Message::Flow(if node.children.is_some() {
            crate::FlowMsg::Toggle { token, id }
        } else {
            crate::FlowMsg::Expand { token, id }
        }))
        .into()
    } else {
        space().width(16).into()
    };
    let label = match node.role {
        crate::app::flow::Role::Passed => format!("→ {}", node.detail),
        crate::app::flow::Role::Assigned if !node.detail.is_empty() => {
            format!("= {}", node.detail)
        }
        role => role.tag().to_string(),
    };
    let where_ = format!("{}:{}", node.rel, node.line + 1);
    let changed: Element<'_, Message> = if node.changed {
        text("changed")
            .size(ts::CAPTION)
            .color(theme::warning())
            .into()
    } else {
        space().width(0).into()
    };
    let body = column![
        row![
            text(label).size(ts::CAPTION).color(theme::accent()),
            text(where_).size(ts::CAPTION).color(theme::dim()),
            changed,
        ]
        .spacing(6),
        text(if node.classified {
            node.text.clone()
        } else {
            "(line not read)".to_string()
        })
        .size(ts::SMALL)
        .font(Font::MONOSPACE)
        .color(if node.classified {
            theme::fg()
        } else {
            theme::dim()
        })
        .wrapping(Wrapping::None),
    ]
    .spacing(1);
    let open = button(body)
        .style(theme::list_row(false))
        .width(Fill)
        .padding([3, 6])
        .on_press(Message::Editor(EditorMsg::OpenAbs {
            abs: node.abs.clone(),
            line: Some(node.line + 1),
            push: true,
        }));
    container(row![unfold, open].spacing(2).align_y(iced::Center))
        .padding(Padding {
            top: 0.0,
            right: 6.0,
            bottom: 0.0,
            left: indent,
        })
        .width(Fill)
        .into()
}

pub(crate) fn calls_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.proj.call_graph else {
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
                .size(ts::BODY)
                .color(theme::accent())
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::Calls(CallsMsg::ExpandAll)),
            button(text(tree.direction.label()).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Calls(CallsMsg::Direction)),
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
            text("…")
                .size(ts::SMALL)
                .color(theme::accent())
                .width(16)
                .into()
        } else if node.cyclic {
            text("↺")
                .size(ts::SMALL)
                .color(theme::dim())
                .width(16)
                .into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(
                text(if node.expanded { "▾" } else { "▸" })
                    .size(ts::SMALL)
                    .color(theme::dim()),
            )
            .style(theme::list_row(false))
            .padding([0, 3])
            // Names the tree it was drawn from: node ids are bare indices, so
            // a click that lands after a direction flip or a new hierarchy
            // must act on nothing rather than on the new tree's node `id`.
            .on_press(Message::Calls(CallsMsg::ExpandNode {
                token: tree.token,
                id,
            }))
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
                text(&node.item.name)
                    .size(ts::BODY)
                    .wrapping(Wrapping::None),
                space().width(6),
                text(format!("{fname}:{}", node.item.line + 1))
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .wrapping(Wrapping::None),
            ]
            .align_y(iced::Center),
        )
        .style(theme::list_row(false))
        .width(Fill)
        .padding([1, 4])
        .on_press(Message::Editor(EditorMsg::OpenAbs {
            abs: node.item.path.clone(),
            line: Some(node.item.line + 1),
            push: true,
        }));

        let badge = text(kind)
            .size(ts::CAPTION)
            .color(theme::dim())
            .width(if kind.is_empty() { 0.0 } else { 22.0 });

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
        // The tree's node cap cut this node short: say how many it holds back.
        if let Some(note) = tree.hidden_note(id) {
            rows.push(tree_note_row(node.depth + 1, note));
        }
    }

    // The banner sits in a slot that is always there (zero-size while there
    // is nothing to say): inserting it pushed the tree's scrollable down a
    // position, iced rebuilt it at the top, and the reader lost their place.
    let stale: Element<'_, Message> = if tree.stale {
        tree_banner(
            "⟳ code changed — press gc to refresh".into(),
            theme::warning(),
        )
    } else {
        slot()
    };
    column![
        header,
        stale,
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}

/// The import tree: a header with the focus file + an Imports/Importers toggle,
/// a cycles banner, then the lazily-expanded (but synchronous) tree.
pub(crate) fn imports_tab(app: &App) -> Element<'_, Message> {
    use crate::imports::Target;

    let Some(tree) = &app.proj.import_tree else {
        return container(
            column![
                text("No file focused.").size(ts::BODY).color(theme::dim()),
                space().height(6),
                text("Open a source file to see what it")
                    .size(ts::SMALL)
                    .color(theme::dim()),
                text("imports and what imports it.")
                    .size(ts::SMALL)
                    .color(theme::dim()),
            ]
            .spacing(2),
        )
        .padding(12)
        .into();
    };

    let header = container(
        row![
            text(&tree.root_name)
                .size(ts::BODY)
                .color(theme::accent())
                .wrapping(Wrapping::None),
            space().width(Fill),
            button(text("⇊ all").size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::Graph(GraphMsg::ImportExpandAll)),
            button(text(tree.direction.label()).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 8])
                .on_press(Message::Graph(GraphMsg::ImportDirection)),
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
            text("↺")
                .size(ts::SMALL)
                .color(theme::dim())
                .width(16)
                .into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(16).into()
        } else {
            button(
                text(if node.expanded { "▾" } else { "▸" })
                    .size(ts::SMALL)
                    .color(theme::dim()),
            )
            .style(theme::list_row(false))
            .padding([0, 3])
            .on_press(Message::Graph(GraphMsg::ImportExpand {
                token: app.proj.import_tree_token,
                id,
            }))
            .into()
        };

        // Internal files open on click; external/unresolved are dim leaves.
        let name: Element<'_, Message> = match &node.target {
            Target::Internal(path) => button(
                row![
                    text(&node.label).size(ts::BODY).wrapping(Wrapping::None),
                    space().width(6),
                    text(&node.detail)
                        .size(ts::CAPTION)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .align_y(iced::Center),
            )
            .style(theme::list_row(false))
            .width(Fill)
            .padding([1, 4])
            .on_press(Message::Editor(EditorMsg::OpenAbs {
                abs: path.clone(),
                line: None,
                push: true,
            }))
            .into(),
            Target::External(_) => container(
                row![
                    text(&node.label)
                        .size(ts::BODY)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                    space().width(6),
                    text("ext").size(ts::CAPTION).color(theme::dim()),
                ]
                .align_y(iced::Center),
            )
            .padding([1, 4])
            .width(Fill)
            .into(),
            Target::Unresolved(_) => container(
                row![
                    text(&node.label)
                        .size(ts::BODY)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                    space().width(6),
                    text("?").size(ts::CAPTION).color(theme::dim()),
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
        // The tree's node cap cut this node short: say how many it holds back.
        if let Some(note) = tree.hidden_note(id) {
            rows.push(tree_note_row(node.depth + 1, note));
        }
    }

    // Both banners sit in slots that are always there (zero-size while there
    // is nothing to say), so one appearing never moves the tree's scrollable
    // to another position — where iced would rebuild it at the top.
    let cycles: Element<'_, Message> = match app.proj.import_cycles.len() {
        0 => slot(),
        n => tree_banner(
            format!("⚠ {n} import cycle{}", if n == 1 { "" } else { "s" }),
            theme::warning(),
        ),
    };
    // The graph under this tree is built from the symbol index; files the
    // index had to leave out have no edges here, so say which were left out.
    let capped: Element<'_, Message> = match index_cap_note(app) {
        Some(note) => tree_banner(format!("Index incomplete: {note}"), theme::dim()),
        None => slot(),
    };
    column![
        header,
        cycles,
        capped,
        scrollable(Column::with_children(rows).width(Fill))
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar)
            .height(Fill),
    ]
    .into()
}
