//! The bottom panel (the debugger), the Ask conversation the floating Ask
//! window holds (`ui::askfloat`), and the calls/imports tabs.

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

/// The collapsible bottom panel: the debugger's, under a title strip with a
/// collapse control. (Ask floats over the code instead: `ui::askfloat`.)
pub(crate) fn bottom_panel(app: &App) -> Element<'_, Message> {
    let title = container(
        row![
            glyph::icon(Glyph::Debug, theme::fg_bright(), 16.0),
            text(tr!("Debug")).size(ts::SMALL).color(theme::fg_bright()),
        ]
        .spacing(6)
        .align_y(iced::Center),
    )
    .padding([5, 12]);
    // A borderless "hide" affordance — no button box, just a chevron that
    // brightens on hover.
    let collapse = button(glyph::icon(Glyph::ChevronDown, theme::fg_muted(), 12.0))
        .style(theme::list_row(false))
        .padding([3, 10])
        .on_press(Message::Window(WindowMsg::CollapseBottom));
    let tabs = row![title, space().width(Fill), collapse,]
        .spacing(2)
        .align_y(iced::Center)
        .padding(Padding {
            top: 2.0,
            right: 6.0,
            bottom: 2.0,
            left: 6.0,
        });

    let content: Element<'_, Message> = if app.debug.session.is_some() {
        debug_panel(app)
    } else {
        empty_state(Glyph::Debug, tr!("No debug session"), "", None)
    };

    container(column![tabs, hairline(), content].height(Fill))
        .height(Fill)
        .style(theme::on_sheet)
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
        DebugStatus::Launching => (tr!("launching…"), theme::dim()),
        DebugStatus::Running => (tr!("running"), theme::success()),
        DebugStatus::Stopped => (tr!("stopped"), theme::warning()),
        DebugStatus::Terminated => (tr!("terminated"), theme::dim()),
    };
    let stopped = session.status == DebugStatus::Stopped;

    let ctrl = |label: &'static str, msg: Message, enabled: bool| {
        let mut b = button(text(label).size(ts::BODY).wrapping(Wrapping::None))
            .style(theme::toolbar_button)
            .padding([2, 8]);
        if enabled {
            b = b.on_press(msg);
        }
        b
    };
    let controls = row![
        ctrl(
            tr!("▶ Continue"),
            Message::Debug(DebugMsg::Control(DebugCmd::Continue)),
            stopped
        ),
        ctrl(
            tr!("⤼ Over"),
            Message::Debug(DebugMsg::Control(DebugCmd::StepOver)),
            stopped
        ),
        ctrl(
            tr!("⤓ In"),
            Message::Debug(DebugMsg::Control(DebugCmd::StepIn)),
            stopped
        ),
        ctrl(
            tr!("⤒ Out"),
            Message::Debug(DebugMsg::Control(DebugCmd::StepOut)),
            stopped
        ),
        ctrl(tr!("■ Stop"), Message::Debug(DebugMsg::Stop), true),
    ]
    .spacing(4);
    // The controls whole at the right; what is said before them takes the
    // room left, cut short where a language's words need more.
    let said = container(
        row![
            text(tr!("Debug"))
                .size(ts::BASE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
            text(status_txt)
                .size(ts::SMALL)
                .color(status_color)
                .wrapping(Wrapping::None),
            space().width(Fill),
            trace_control(app),
            hover_eval_control(app),
        ]
        .spacing(8)
        .align_y(iced::Center),
    )
    .width(Fill)
    .clip(true);
    let header = row![said, controls].spacing(8).align_y(iced::Center);

    // Call stack — click a frame to jump to its source.
    let mut stack_rows: Vec<Element<'_, Message>> = vec![
        text(tr!("CALL STACK"))
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
        text(tr!("VARIABLES"))
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
        push_var_rows(app, &sc.vars, 0, &mut var_rows);
    }

    // Program output.
    let mut out_rows: Vec<Element<'_, Message>> = vec![
        text(tr!("OUTPUT"))
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
    ];
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
    let mut watch_rows: Vec<Element<'_, Message>> = vec![
        text(tr!("WATCH"))
            .size(ts::CAPTION)
            .color(theme::dim())
            .into(),
    ];
    watch_rows.push(
        text_input(tr!("Add watch…"), &app.debug.watch_input)
            .style(theme::input)
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
                button(glyph::icon(Glyph::Close, theme::dim(), 10.0))
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
        .style(theme::on_sheet)
        .into()
}

/// The run's trace so far — how many stops it holds — and the button that
/// turns it into a walkthrough of the path the program took.
pub(crate) fn trace_control(app: &App) -> Element<'_, Message> {
    let n = app.debug.trace.len();
    if n == 0 {
        // "Yet" only while the program can still stop: a run that ended
        // without stopping has no stops, and never will.
        let ended = app
            .debug
            .session
            .as_ref()
            .is_some_and(|s| s.status == crate::DebugStatus::Terminated);
        return text(if ended {
            tr!("Trace: no stops")
        } else {
            tr!("Trace: no stops yet")
        })
        .size(ts::CAPTION)
        .color(theme::dim())
        .into();
    }
    let count = if app.debug.trace_cut {
        trn!(
            "Trace: 1 stop (cut short)",
            "Trace: {n} stops (cut short)",
            n
        )
    } else {
        trn!("Trace: 1 stop", "Trace: {n} stops", n)
    };
    row![
        text(count).size(ts::CAPTION).color(theme::dim()),
        button(text(tr!("Walk this run")).size(ts::SMALL))
            .style(theme::toolbar_button)
            .padding([2, 8])
            .on_press(Message::Walk(WalkMsg::GenerateTrace)),
    ]
    .spacing(6)
    .align_y(iced::Center)
    .into()
}

/// Whether hovering a name while paused evaluates it (see
/// `dap::hover_eval_allowed`). An adapter that promised side-effect-free
/// hovers needs no switch, and nothing is shown. For any other adapter an
/// evaluation may run the program's own code, so it is a checkbox the
/// reader has to tick, with the risk named beside it.
pub(crate) fn hover_eval_control(app: &App) -> Element<'_, Message> {
    if app.debug.hover_safe {
        return space().into();
    }
    tooltip(
        iced::widget::checkbox(app.debug_hover_eval)
            .style(theme::check)
            .label(tr!("Evaluate on hover"))
            .on_toggle(|_| Message::Debug(DebugMsg::ToggleHoverEval))
            .size(14)
            .text_size(ts::SMALL)
            .spacing(6),
        container(
            text(tr!("May run the program's own code."))
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
/// The Ask conversation, filling the floating Ask window (`ui::askfloat`),
/// which is `height` tall: the pinned approval card, the turns, and the
/// compose area. The window's header names it and holds Clear.
pub(crate) fn ask_panel(app: &App, height: f32) -> Element<'_, Message> {
    ask_panel_in(app, height, false)
}

/// Room the Ask card keeps, when it fits its height to the conversation,
/// for what is not the conversation: its header and the voice line under
/// it, the input row, and the padding and spacing around them.
const CARD_CHROME: f32 = 180.0;
/// The gap over a pinned approval card, where the count of those waiting
/// after it shows.
const COUNT_SLOT_H: f32 = 14.0;
/// An approval's buttons: their height, and the gap between what is in it.
const APPROVE_BUTTON_H: f32 = 30.0;
const APPROVE_GAP: f32 = 12.0;
/// Size of what an approval says it does, under its question.
const APPROVE_DETAIL_SIZE: f32 = 12.0;
/// Size of the "Said" over a question said rather than typed, and of its
/// microphone: the design's 11.
const SAID_SIZE: f32 = 11.0;
/// Widest the question's bubble grows, of the conversation's text column:
/// the design's 82%.
const BUBBLE_SHARE: f32 = 0.82;

/// The width of the conversation's text column: the card's, inside its
/// 1 px edge, less the 16 either side.
fn conversation_width(app: &App) -> f32 {
    crate::ui::askfloat::ask_place(app).map_or(ASK_CARD_W, |p| p.width) - 2.0 - 32.0
}
/// Room for each of the composer's optional rows (the pinned code, an ACP
/// agent's settings) over the input.
const COMPOSER_ROW_ROOM: f32 = 36.0;

/// [`ask_panel`], fitted to its conversation: as tall as what is in it, up
/// to a card `height` tall, rather than always that tall — the floating
/// card, which hangs under the chat field and grows as the conversation
/// does.
pub(crate) fn ask_panel_fitted(app: &App, height: f32) -> Element<'_, Message> {
    ask_panel_in(app, height, true)
}

fn ask_panel_in(app: &App, height: f32, fit: bool) -> Element<'_, Message> {
    // Every part of the panel keeps its place in the column, there or not
    // (an empty `space()`, not one of no height, which a column leaves out):
    // iced pairs widget state with widgets by position, and one coming or
    // going handed the conversation's scroll, or the input's focus, to
    // another widget.
    let header: Element<'_, Message> = space().into();

    let mut convo: Vec<Element<'_, Message>> = Vec::new();
    if app.proj.ask_turns.is_empty() && !app.proj.asking {
        // An empty conversation shows what to ask, not a paragraph about
        // asking: the box below says what it is for, and "Add to Ask" is in
        // the code's own context menu. The one thing worth saying is when
        // answers would have nothing to stand on — Ask in retrieval mode (no
        // server to run an agent on) with no index and nothing pinned — and
        // it is said quietly, as a hint, not as an error.
        if app.proj.project.is_some()
            && !app.server.is_up()
            && app.proj.embed_index.entries.is_empty()
            && app.proj.ask_pins.is_empty()
        {
            convo.push(
                text(tr!(
                    "For grounded answers, run “Explain All” or add code with “Add to Ask”."
                ))
                .size(ts::CAPTION)
                .color(theme::dim())
                .into(),
            );
        }
        // While voice listens: what can be said, and what comes of it.
        if app.voice.is_listening() {
            convo.push(
                text(tr!("Try saying"))
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .into(),
            );
            for (say, does) in crate::ui::askfloat::try_saying(app) {
                convo.push(
                    row![
                        // Wrapped, not run under its result: an example
                        // in some languages is longer than its row.
                        text(say)
                            .size(ts::SMALL)
                            .color(theme::fg())
                            .width(Fill)
                            .wrapping(Wrapping::WordOrGlyph),
                        container(text(does).size(ts::CAPTION).color(theme::fg_muted()))
                            .padding([2, 8])
                            .style(theme::chip),
                    ]
                    .spacing(8)
                    .align_y(iced::Center)
                    .into(),
                );
            }
        }
        // Context-aware starter questions — click one to ask it.
        let suggestions = app.suggested_questions();
        if !suggestions.is_empty() && !app.voice.is_listening() {
            convo.push(
                text(tr!("Try asking"))
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
    // The question's bubble grows to the design's 82% of the column it is
    // in: the card's, or the margin's.
    let bubble_max = BUBBLE_SHARE * conversation_width(app);
    for turn in &app.proj.ask_turns {
        // A question said rather than typed is marked so, over it (with
        // the design's 4 between them; a typed one has neither).
        let mut asked = Column::new()
            .spacing(4)
            .align_x(iced::alignment::Horizontal::Right);
        if turn.spoken {
            asked = asked.push(
                row![
                    glyph::icon(Glyph::Mic, theme::dim(), SAID_SIZE),
                    ts::set_in(
                        text(tr!("Said")).color(theme::dim()),
                        ts::INTER.normal(SAID_SIZE),
                        ts::INTER,
                        SAID_SIZE,
                    ),
                ]
                .spacing(5)
                .align_y(iced::Center),
            );
        }
        convo.push(
            container(
                asked.push(
                    container(
                        text(turn.question.as_str())
                            .size(ts::BASE)
                            .line_height(1.5)
                            .color(theme::fg()),
                    )
                    .padding([9, 12])
                    // As wide as its words up to the design's 82% of the
                    // text column; a question that runs on to a second
                    // line takes all of that, its lines ragged inside it,
                    // as the design sets it.
                    .width(
                        if super::sheet::text_width(&turn.question, ts::BASE, crate::fonts::UI)
                            > bubble_max - 24.0
                        {
                            Length::Fixed(bubble_max)
                        } else {
                            Length::Shrink
                        },
                    )
                    .max_width(bubble_max)
                    .style(theme::bubble),
                ),
            )
            .width(Fill)
            .align_x(iced::alignment::Horizontal::Right)
            .into(),
        );
        if let Some(agent) = &turn.extras.agent {
            convo.push(
                text(tr!("Asked {agent}", agent = agent))
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .into(),
            );
        }
        // An ACP agent's plan, as it last sent it.
        if !turn.extras.plan.is_empty() {
            let lines: Vec<Element<'_, Message>> = turn
                .extras
                .plan
                .iter()
                .map(|entry| {
                    let (mark, color) = match entry.status.as_str() {
                        "completed" => ("☑", theme::dim()),
                        "in_progress" => ("◐", theme::fg()),
                        _ => ("☐", theme::fg_muted()),
                    };
                    text(format!("{mark} {}", entry.content))
                        .size(ts::SMALL)
                        .color(color)
                        .into()
                })
                .collect();
            convo.push(Column::with_children(lines).spacing(2).into());
        }
        // Agent exploration: one chip per tool call, clickable when the step
        // touched a code location (jump to the first ref).
        if !turn.steps.is_empty() {
            let root = app.proj.project.as_ref().map(|p| p.root.clone());
            let chips: Vec<Element<'_, Message>> = turn
                .steps
                .iter()
                .map(|s| agent_step_chip(s, root.as_deref()))
                .collect();
            convo.push(Column::with_children(chips).spacing(2).into());
        }
        // Tool calls waiting for the user's say-so are asked under the
        // conversation, one at a time, the card in view there.
        if turn.streaming && !turn.extras.thought.trim().is_empty() {
            let thought = turn.extras.thought.trim();
            let tail: String = {
                let n = thought.chars().count();
                thought.chars().skip(n.saturating_sub(280)).collect()
            };
            convo.push(
                text(format!(
                    "💭 {}{tail}",
                    if tail.len() < thought.len() {
                        "…"
                    } else {
                        ""
                    }
                ))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
            );
        }
        if turn.streaming && !turn.extras.approvals.is_empty() && !turn.answer.is_empty() {
            // Paused on a question to the user: what it said so far, drawn
            // as an answer is (`App::settle_waiting_answers`).
            convo.extend(super::explain_view::render_answer(app, &turn.answer));
        } else if turn.streaming {
            // Live answer: "Thinking…" until the first token, then the raw text
            // (with a cursor) as it streams; it's re-rendered richly when done.
            if turn.answer_md.trim().is_empty() {
                let label = if !turn.extras.approvals.is_empty() {
                    tr!("Waiting for your answer…")
                } else if turn.steps.is_empty() {
                    tr!("Thinking…")
                } else {
                    tr!("Exploring…")
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
            convo.extend(super::explain_view::render_answer(app, &turn.answer));
        }
        if !turn.sources.is_empty() {
            convo.push(
                text(tr!("Sources"))
                    .size(ts::CAPTION)
                    .color(theme::dim())
                    .into(),
            );
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
        convo.push(
            text(tr!("Thinking…"))
                .size(ts::BODY)
                .color(theme::dim())
                .into(),
        );
    }
    let conversation = scrollable(Column::with_children(convo).spacing(14).width(Fill))
        .id(ask_scroll_id())
        .direction(thin_scroll())
        .style(theme::overlay_scrollbar)
        .height(if fit { Length::Shrink } else { Fill });
    // Fitted, the conversation is as tall as it is, up to what leaves the
    // composer its room: laid out before it, it would otherwise take all of
    // the card's height and push the input out of it.
    let extra_rows = [!app.proj.ask_pins.is_empty(), app.ask_agent.is_some()]
        .into_iter()
        .filter(|&row| row)
        .count() as f32;
    // An approval pinned under it: as tall as it is laid out across the
    // card, and the place for the count over it.
    let approval_room = match app.first_approval() {
        Some((_, approval)) if fit => {
            let width = crate::ui::askfloat::ask_place(app).map_or(ASK_CARD_W, |p| p.width);
            let head = (height * 0.5 - 60.0).max(40.0);
            COUNT_SLOT_H + approval_card_height(approval, width - 32.0, head)
        }
        _ => 0.0,
    };
    let conversation = container(conversation).max_height(if fit {
        (height - CARD_CHROME - COMPOSER_ROW_ROOM * extra_rows - approval_room).max(40.0)
    } else {
        f32::INFINITY
    });
    // Fitted, the card would grow with what arrives above an approval and
    // move its answers down: while one is asked, the conversation keeps the
    // height it had, and what arrives scrolls in it.
    let conversation = super::held::held(conversation, fit && app.first_approval().is_some());
    // The tool call waiting for the user's say-so, pinned under the
    // conversation, over the composer: drawn in it, a card moved down as its
    // turn grew above it — a step, a plan entry, each scrolling the panel —
    // its Allow with it, to under the pointer. Here the conversation grows
    // in its own scroll while it is asked, and the card stays put. Its Allow
    // is drawn enabled once it has been within reach long enough to be read
    // (`App::may_allow`); the cards are answered in turn.
    let pinned: Element<'_, Message> = match app.first_approval() {
        Some((turn, approval)) => {
            let asked = app.asked(crate::app::model::Question::Approval {
                stream: turn.stream,
                key: approval.key,
            });
            let more = app
                .proj
                .ask_turns
                .iter()
                .filter(|t| t.streaming)
                .map(|t| t.extras.approvals.len())
                .sum::<usize>()
                - 1;
            // About half the panel at most, the input kept in view: a card
            // whose arguments run longer scrolls them, above its answers.
            let head = (height * 0.5 - 60.0).max(40.0);
            let page = match &app.asking.answers_page {
                Some((crate::app::model::Question::Approval { stream, key }, page))
                    if *stream == turn.stream && *key == approval.key =>
                {
                    *page
                }
                _ => 0,
            };
            let card = approval_card(turn.stream, approval, asked, head, page);
            // The count goes over it, out of what scrolls, in a place of its
            // own that is there with no count to show — the gap over the
            // card — so one more coming moves nothing of it.
            let count: Element<'_, Message> = if more > 0 {
                text(trn!(
                    "1 more waiting after this one",
                    "{n} more waiting after this one",
                    more
                ))
                .size(ts::CAPTION)
                .line_height(ts::normal_line(ts::CAPTION))
                .color(theme::dim())
                .into()
            } else {
                space().into()
            };
            column![
                container(count)
                    .height(Length::Fixed(COUNT_SLOT_H))
                    .align_y(iced::alignment::Vertical::Center),
                card
            ]
            .into()
        }
        None => space().into(),
    };

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
                        button(glyph::icon(Glyph::Close, theme::dim(), 10.0))
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
        compose.push(compose_slot(Some(
            Row::with_children(chips).spacing(4).wrap().into(),
        )));
    } else {
        compose.push(compose_slot(None));
    }
    let placeholder = match app
        .ask_agent
        .as_deref()
        .and_then(|n| app.acp_agents.iter().find(|a| a.name == n))
    {
        Some(agent) => tr!(
            "Ask {agent} about this codebase…",
            agent = crate::app::integrations::acp_label(agent)
        ),
        // Once there is a conversation, what is typed is a reply — and, in
        // the large window, where there is room to say it, the microphone's
        // key as another way to give it.
        None if !app.proj.ask_turns.is_empty() && app.ask_view == crate::AskView::Expanded => {
            tr!(
                "Reply, or press {keys} to talk",
                keys = app.keymap.chord(crate::keymap::Action::Voice).caps()
            )
        }
        None if !app.proj.ask_turns.is_empty() => tr!("Reply…").to_string(),
        None => tr!("Ask about this codebase…").to_string(),
    };
    let input = ts::field_in(
        text_input(&placeholder, &app.proj.ask_input)
            .id(ask_input_id())
            .on_input(|v| Message::Ask(AskMsg::InputChanged(v)))
            .on_submit(Message::Ask(AskMsg::Submit))
            .style(theme::bare_input),
        ROUND_BUTTON,
        ts::INTER,
        ts::READING,
    );
    // The send button is the composer's primary action, a round button in
    // the accent (a quiet one while an answer is coming in, when it does
    // nothing). Any answer still coming in — an agent turn, a server chat
    // stream, a local retrieval stream — can be stopped from here, not only
    // agent turns.
    let streaming = app.ask_stream_active();
    let idle = !streaming;
    let send_tint = if idle {
        crate::theme::active().on_accent
    } else {
        theme::dim()
    };
    let round = |glyph: Glyph, tint: iced::Color| {
        button(
            container(glyph::icon(glyph, tint, 15.0))
                .width(Fill)
                .height(Fill)
                .align_x(iced::Center)
                .align_y(iced::Center),
        )
        .padding(0)
        .width(Length::Fixed(ROUND_BUTTON))
        .height(Length::Fixed(ROUND_BUTTON))
    };
    let mut ask_btn = round(Glyph::Send, send_tint).style(theme::round_button(idle));
    if idle {
        ask_btn = ask_btn.on_press(Message::Ask(AskMsg::Submit));
    }
    let ask_btn = super::toolbar::tip_at(ask_btn, tr!("Ask"), None, tooltip::Position::Top);
    // Or say it: the microphone, in the composer beside what is typed.
    let listening = app.voice.is_listening();
    let talk = super::toolbar::tip_at(
        round(
            Glyph::Mic,
            if listening {
                theme::danger()
            } else {
                theme::fg()
            },
        )
        .style(theme::round_button(false))
        .on_press(Message::Voice(crate::VoiceMsg::Toggle)),
        if listening {
            tr!("Stop listening")
        } else {
            tr!("Talk")
        },
        Some(app.keymap.chord(crate::keymap::Action::Voice).caps()),
        tooltip::Position::Top,
    );
    // The ACP session's own settings (its mode, its model…), when it has any.
    let acp_active = app
        .proj
        .acp
        .as_ref()
        .filter(|l| app.ask_agent.as_deref() == Some(l.name.as_str()));
    if let Some(link) = acp_active.filter(|l| !l.options.is_empty()) {
        let pickers: Vec<Element<'_, Message>> = link
            .options
            .iter()
            .map(|option| {
                let choices: Vec<OptionChoice> = option
                    .choices
                    .iter()
                    .map(|(value, name)| OptionChoice {
                        value: value.clone(),
                        label: name.clone(),
                    })
                    .collect();
                let selected = choices.iter().find(|c| c.value == option.current).cloned();
                let id = option.id.clone();
                row![
                    text(format!("{}:", option.name))
                        .size(ts::CAPTION)
                        .color(theme::dim()),
                    ask_picker(
                        app,
                        pick_list(choices, selected, move |c: OptionChoice| {
                            Message::Ask(AskMsg::AcpOption {
                                option: id.clone(),
                                value: c.value,
                            })
                        })
                        .style(theme::pick)
                        .menu_style(theme::pick_menu)
                        .text_size(ts::CAPTION)
                        .padding([2, 6]),
                    ),
                ]
                .spacing(4)
                .align_y(iced::Center)
                .into()
            })
            .collect();
        compose.push(compose_slot(Some(
            Row::with_children(pickers).spacing(10).wrap().into(),
        )));
    } else {
        compose.push(compose_slot(None));
    }
    // Who answers: clew's own agent, or an ACP agent the user configured —
    // in a place of its own, there or not, as the rows above: the input
    // after it keeps its state (`compose_slot`).
    let mut agent: Element<'_, Message> = space().into();
    if !app.acp_agents.is_empty() {
        let mut choices = vec![AgentChoice {
            name: None,
            label: "Clew".into(),
        }];
        choices.extend(app.acp_agents.iter().map(|a| AgentChoice {
            name: Some(a.name.clone()),
            label: crate::app::integrations::acp_label(a),
        }));
        let selected = choices
            .iter()
            .find(|c| c.name == app.ask_agent)
            .cloned()
            .or_else(|| choices.first().cloned());
        agent = container(ask_picker(
            app,
            pick_list(choices, selected, |c: AgentChoice| {
                Message::Ask(AskMsg::PickAgent(c.name))
            })
            .style(theme::pick)
            .menu_style(theme::pick_menu)
            .text_size(ts::SMALL)
            .padding([6, 8]),
        ))
        .padding(iced::Padding::default().right(6))
        .into();
    }
    // An answer can run long — always give the user a way out. In a place
    // of its own, there or not: Stop coming before Ask handed Ask its state
    // as it went, and a press held on Stop as the answer ended asked the
    // question typed meanwhile, let go of over Ask.
    let stop: Element<'_, Message> = if streaming {
        button(text(tr!("Stop")).size(ts::BASE))
            .style(theme::toolbar_button)
            .padding([5, 12])
            .on_press(Message::Ask(AskMsg::Stop))
            .into()
    } else {
        slot()
    };
    // A conversation by voice: once Clew has answered, listen again. The
    // design's switch — a track, its knob in from the end it is at — and
    // beside it what it does, which turns it as well.
    let on = app.voice.keep_listening;
    let turn = Message::Voice(crate::VoiceMsg::SetKeepListening(!on));
    let track = super::form::switch(on, turn.clone());
    let keep_listening = row![
        track,
        iced::widget::mouse_area(ts::set_in(
            text(tr!("Keep listening")).color(theme::fg_muted()),
            super::form::SWITCH_H,
            ts::INTER,
            KEEP_LISTENING_SIZE,
        ))
        .on_press(turn)
        .interaction(iced::mouse::Interaction::Pointer),
    ]
    .spacing(KEEP_LISTENING_GAP)
    .align_y(iced::Center);
    // The composer, under a rule across the card: the question as it is
    // typed, Stop while an answer comes in, Keep listening, the microphone
    // and send.
    let compose_row = row![input, stop, keep_listening, talk, ask_btn]
        .spacing(8)
        .align_y(iced::Center);
    compose.push(row![agent, compose_row].align_y(iced::Center).into());

    let height = if fit { Length::Shrink } else { Fill };
    container(column![
        // No spacing of the column's own: the header and the pinned card
        // are empty places most of the time, and would each add a gap.
        column![header]
            .push(conversation)
            .push(pinned)
            .padding(16)
            .height(height),
        hairline(),
        container(Column::with_children(compose)).padding(COMPOSER_PAD),
    ])
    .width(Fill)
    .height(height)
    .into()
}

/// Side of the composer's round buttons (the microphone, send).
pub(crate) const ROUND_BUTTON: f32 = 30.0;
/// Size of the composer's "Keep listening", as the design sets it.
const KEEP_LISTENING_SIZE: f32 = 12.0;
/// The design's gap between the switch and what it says.
const KEEP_LISTENING_GAP: f32 = 7.0;
/// Padding around the composer's row: a little more room at its start,
/// where the question begins.
pub(crate) const COMPOSER_PAD: Padding = Padding {
    top: 10.0,
    right: 10.0,
    bottom: 10.0,
    left: 14.0,
};

/// Most answers of each kind (allowing, declining) an approval card shows
/// at once.
pub(crate) const MAX_SHOWN_ANSWERS: usize = 3;

/// The answers of `options` an approval card shows on its `page` (counted
/// round), and how many pages there are: the `page`th [`MAX_SHOWN_ANSWERS`]
/// allowing ones and as many declining ones, in the agent's order — and,
/// past the declining ones, the first of them again: a way to decline on
/// every page.
pub(crate) fn shown_answers(
    options: &[crate::app::model::ApprovalOption],
    page: usize,
) -> (Vec<&crate::app::model::ApprovalOption>, usize) {
    let of_kind = |allows: bool| -> Vec<usize> {
        (0..options.len())
            .filter(|&i| options[i].allows == allows)
            .collect()
    };
    let (allowing, declining) = (of_kind(true), of_kind(false));
    let pages = allowing
        .len()
        .max(declining.len())
        .div_ceil(MAX_SHOWN_ANSWERS)
        .max(1);
    let page = page % pages;
    let on_page = |of: &[usize]| -> Vec<usize> {
        of.iter()
            .skip(page * MAX_SHOWN_ANSWERS)
            .take(MAX_SHOWN_ANSWERS)
            .copied()
            .collect()
    };
    let mut shown = on_page(&allowing);
    match on_page(&declining) {
        none if none.is_empty() => shown.extend(declining.first()),
        some => shown.extend(some),
    }
    shown.sort_unstable();
    (shown.into_iter().map(|i| &options[i]).collect(), pages)
}

/// A picker of the Ask panel, drawn afresh — its menu closed — while what
/// is over the panel takes every press (a modal, the context menu): a menu
/// left open under one could not be closed from it, and an open menu holds
/// back every question (`ui::placed::watch_overlays`), that modal's too.
fn ask_picker<'a>(app: &App, picker: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let covered = crate::ui::active_overlay(app).is_some_and(|o| o.holds_keys());
    crate::ui::fresh::fresh(u64::from(covered), picker)
}

/// A row of the Ask panel's compose area that may be there or not, in a
/// place of its own either way: iced pairs widget state with widgets by
/// position, and a row coming or going above the input handed its focus and
/// cursor to another widget. An empty `space()`, not one of no height, which
/// a column leaves out; the gap below a row is its own, so an absent one
/// takes no room.
fn compose_slot(row: Option<Element<'_, Message>>) -> Element<'_, Message> {
    match row {
        Some(row) => container(row)
            .padding(iced::Padding::default().bottom(4))
            .into(),
        None => space().into(),
    }
}

/// One value of an ACP session setting, as its picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionChoice {
    value: String,
    label: String,
}

impl std::fmt::Display for OptionChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Who answers the Ask panel, as its picker lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentChoice {
    /// The ACP agent's name; `None` for clew's own.
    name: Option<String>,
    label: String,
}

impl std::fmt::Display for AgentChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// A tool call waiting for the user: who asks for what, the arguments, and
/// the answers on offer, those on `page` (counted round; see
/// [`shown_answers`]) — those that let it run only when `ready`. Who asks
/// and the arguments scroll, no taller than `head`; the answers stay below
/// them, where they are: scrolled with them, an Allow out of view while its
/// delay ran came up under the pointer enabled.
pub(crate) fn approval_card<'a>(
    stream: u64,
    approval: &crate::app::model::Approval,
    asked: impl Into<crate::ui::placed::Asked>,
    head: f32,
    page: usize,
) -> Element<'a, Message> {
    let asked = asked.into();
    let ready = asked.ready;
    // A few of each kind at a time, in the agent's order, the rest a page
    // further: an agent's list of answers is its own to make as long as it
    // likes, and answers running over more rows than the card can show
    // could not be reached. A way to decline is always among them.
    let (shown, pages) = shown_answers(&approval.options, page);
    let page = page % pages;
    let mut buttons: Vec<Element<'a, Message>> = shown
        .into_iter()
        .map(|option| {
            let answer = Message::Ask(AskMsg::Approve {
                stream,
                key: approval.key,
                option: option.id.clone(),
            });
            // The design's 30 px buttons, the one that allows a little wider.
            let shown = button(
                container(text(option.label.clone()).size(ts::BODY).font(
                    crate::fonts::in_control(if option.allows {
                        crate::fonts::UI_SEMIBOLD
                    } else {
                        crate::fonts::UI_MEDIUM
                    }),
                ))
                .height(Fill)
                .align_y(iced::Center),
            )
            .style(if option.allows {
                theme::primary_button
            } else {
                theme::toolbar_button
            })
            .height(Length::Fixed(APPROVE_BUTTON_H))
            .padding([0.0, if option.allows { 12.0 } else { 10.0 }])
            .on_press_maybe((ready || !option.allows).then_some(answer));
            if option.allows {
                crate::ui::fresh::allow(ready, shown)
            } else {
                shown.into()
            }
        })
        .collect();
    use crate::app::model::ApprovalKey;
    // What asks: a command of the window's that costs something, a tool of
    // an MCP server, or the ACP agent itself. A tool's detail is its
    // arguments, set as code; the others say it in words.
    let (glyph, code) = match approval.key {
        ApprovalKey::Window { .. } => (Glyph::Play, false),
        ApprovalKey::Mcp { .. } => (Glyph::Plug, true),
        ApprovalKey::Acp { .. } => (Glyph::Ask, true),
    };
    let glyph = approval.mark.unwrap_or(glyph);
    let mut asks = column![
        text(approval.title.clone())
            .size(ts::BASE)
            .line_height(ts::normal_line(ts::BASE))
            .font(crate::fonts::UI_SEMIBOLD)
            .color(theme::fg())
    ]
    .spacing(2);
    if !approval.detail.is_empty() {
        asks = asks.push(
            text(approval.detail.clone())
                .size(APPROVE_DETAIL_SIZE)
                .line_height(ts::normal_line(APPROVE_DETAIL_SIZE))
                .font(if code {
                    crate::fonts::CODE
                } else {
                    crate::fonts::UI
                })
                .color(theme::fg_muted()),
        );
    }
    // All of it can be read: long arguments scroll, across the card.
    let asks = container(
        scrollable(asks)
            .width(Fill)
            .direction(thin_scroll())
            .style(theme::overlay_scrollbar),
    )
    .max_height(head);
    // The rest of its answers, a page on: the card counts its delay anew
    // for them (`App::on_answers_page`), and its buttons are drawn anew.
    if pages > 1 {
        buttons.push(
            button(
                text(tr!(
                    "More answers ({page} of {pages})",
                    page = page + 1,
                    pages = pages
                ))
                .size(ts::SMALL),
            )
            .style(theme::toolbar_button)
            .padding([3, 10])
            .on_press(Message::Ask(AskMsg::AnswersPage {
                stream,
                key: approval.key,
                page: (page + 1) % pages,
            }))
            .into(),
        );
    }
    // A yes and a no sit beside the question; more answers than that, a
    // row under it.
    let beside = buttons.len() <= 2;
    let answers = crate::ui::fresh::fresh(
        page as u64,
        Row::with_children(buttons).spacing(APPROVE_GAP).wrap(),
    );
    let mark = container(glyph::icon(glyph, theme::accent(), 18.0)).padding([2, 0]);
    let body: Element<'a, Message> = if beside {
        row![mark, container(asks).width(Fill), answers]
            .spacing(12)
            .align_y(iced::Center)
            .into()
    } else {
        column![
            row![mark, container(asks).width(Fill)]
                .spacing(12)
                .align_y(iced::Center),
            answers
        ]
        .spacing(10)
        .into()
    };
    // Fresh for each card: one that comes up where another was answered is
    // not handed a press held on that one's buttons, nor where it was
    // scrolled to (`fresh`).
    // Where its answers are (`placed`).
    crate::ui::placed::placed(
        &asked,
        crate::ui::fresh::fresh_for(
            &(stream, approval.key),
            // The design's padding, inside the border drawn over its edge.
            container(body)
                .padding(Padding {
                    top: 13.0,
                    right: 13.0,
                    bottom: 13.0,
                    left: 15.0,
                })
                .width(Fill)
                .style(theme::approve_card),
        ),
    )
}

/// About how tall [`approval_card`] is laid out `width` wide, its words at
/// most `head` tall: measured, so that a card fitted to its conversation
/// gives the conversation over it what is left, and no more. A little over
/// rather than under: under, the input below it would be cut off.
fn approval_card_height(approval: &crate::app::model::Approval, width: f32, head: f32) -> f32 {
    use crate::app::model::ApprovalKey;
    use crate::ui::sheet::{text_height, text_width};
    let (shown, pages) = shown_answers(&approval.options, 0);
    let count = shown.len() + usize::from(pages > 1);
    let beside = count <= 2;
    let answers: f32 = shown
        .iter()
        .map(|o| {
            if o.allows {
                text_width(
                    &o.label,
                    ts::BODY,
                    crate::fonts::in_control(crate::fonts::UI_SEMIBOLD),
                ) + 24.0
            } else {
                text_width(
                    &o.label,
                    ts::BODY,
                    crate::fonts::in_control(crate::fonts::UI_MEDIUM),
                ) + 20.0
            }
        })
        .sum::<f32>()
        + APPROVE_GAP * count.saturating_sub(1) as f32;
    // Its padding, the mark and the gaps beside the words, its answers
    // when they sit beside them.
    let words =
        width - 28.0 - 18.0 - APPROVE_GAP - if beside { answers + APPROVE_GAP } else { 0.0 };
    let code = !matches!(approval.key, ApprovalKey::Window { .. });
    let mut asks = text_height(&approval.title, ts::BASE, crate::fonts::UI_SEMIBOLD, words);
    if !approval.detail.is_empty() {
        let font = if code {
            crate::fonts::CODE
        } else {
            crate::fonts::UI
        };
        asks += 2.0 + text_height(&approval.detail, APPROVE_DETAIL_SIZE, font, words);
    }
    let asks = asks.min(head);
    let button = APPROVE_BUTTON_H;
    let body = if beside {
        asks.max(button)
    } else {
        asks + 10.0 + button
    };
    body + 26.0 + 4.0
}

/// One agent exploration step as a chip: a tool glyph + its one-line title.
/// Clickable when the step touched code (jumps to the first ref).
/// Height of one step of an agent's turn.
const STEP_ROW_H: f32 = 28.0;
/// The design's gap between what a step's row holds: its mark, its
/// words, its code.
const STEP_GAP: f32 = 9.0;
/// Size of what a step was done to, as code: the design's 12.
const STEP_CODE_SIZE: f32 = 12.0;

/// A step's title as the reader takes it in: what was done, in words, then
/// what it was done to, as code — `read src/lib.rs:1-50` reads "Read
/// `src/lib.rs:1-50`", and a title in words marks its code with backticks
/// ("Went to `on_watch_batch`"). A title of another shape is shown as it is.
fn step_title<'a>(title: &str) -> Element<'a, Message> {
    // Each centred in the step's row as the design centres it (its words
    // at 12.5, its code at 12).
    let word = |s: &str| -> Element<'a, Message> {
        ts::set_in(
            text(s.to_string())
                .color(theme::fg_muted())
                .wrapping(Wrapping::None),
            STEP_ROW_H,
            ts::INTER,
            ts::BODY,
        )
    };
    let code = |s: &str| -> Element<'a, Message> {
        ts::set_in(
            text(s.to_string())
                .font(crate::fonts::CODE)
                .color(theme::fg())
                .wrapping(Wrapping::None),
            STEP_ROW_H,
            ts::MONO,
            STEP_CODE_SIZE,
        )
    };
    // Words with their code in backticks: every other part is code.
    if title.matches('`').count() >= 2 {
        let parts = title
            .split('`')
            .enumerate()
            .filter(|(_, part)| !part.trim().is_empty())
            .map(|(i, part)| {
                if i % 2 == 1 {
                    code(part)
                } else {
                    word(part.trim())
                }
            });
        return Row::with_children(parts)
            .spacing(STEP_GAP)
            .align_y(iced::Center)
            .into();
    }
    let verb = title.split_once(' ').filter(|(verb, rest)| {
        !verb.is_empty() && !rest.trim().is_empty() && verb.chars().all(|c| c.is_ascii_lowercase())
    });
    match verb {
        Some((verb, rest)) => {
            let mut said = verb.to_string();
            said[..1].make_ascii_uppercase();
            // The server's verbs, in the reader's language where known.
            let said = if crate::miscellaneous::extern_text::STEP_VERBS.contains(&said.as_str()) {
                crate::i18n::lookup(&said).to_string()
            } else {
                said
            };
            row![word(&said), code(rest.trim())]
                .spacing(STEP_GAP)
                .align_y(iced::Center)
                .into()
        }
        None => word(title),
    }
}

fn agent_step_chip<'a>(
    step: &crate::app::model::AgentStep,
    root: Option<&std::path::Path>,
) -> Element<'a, Message> {
    use crate::app::model::StepState;
    // Its state, as the mark that leads it: ticked when done.
    let (mark, tint) = match step.state {
        StepState::Done => (Glyph::Check, theme::success()),
        StepState::Running => (Glyph::More, theme::fg_muted()),
        StepState::Waiting => (Glyph::Pause, theme::warning()),
        StepState::Failed => (Glyph::Close, theme::danger()),
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
    let show: Element<'a, Message> = if clickable {
        ts::set_in(
            text(tr!("Show")).color(theme::accent()),
            STEP_ROW_H,
            ts::INTER,
            ts::SMALL,
        )
    } else {
        space().into()
    };
    let mut chip = button(
        row![
            glyph::icon(mark, tint, 14.0),
            container(step_title(&step.title)).width(Fill).clip(true),
            show,
        ]
        .spacing(STEP_GAP)
        .height(Fill)
        .align_y(iced::Center),
    )
    .style(theme::menu_row)
    .width(Fill)
    .height(Length::Fixed(STEP_ROW_H))
    // Its mark and words in line with the answer under it, as the design
    // has them.
    .padding(0);
    if let Some((abs, line)) = target {
        chip = chip.on_press(Message::Editor(EditorMsg::OpenAbs {
            abs,
            line,
            push: true,
        }));
    }
    chip.into()
}

/// The head of a tree in the sidebar (Calls, Imports, Value flow): what it
/// is rooted at and `detail` beside it, then its controls, and under them
/// the way it reads (`switch`) — over a hairline, with no band of its own
/// (the sidebar is the band).
fn tree_head<'a>(
    root: &'a str,
    detail: Option<String>,
    controls: Vec<Element<'a, Message>>,
    switch: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let mut head = row![
        text(root)
            .size(ts::BODY)
            .font(crate::fonts::UI_MEDIUM)
            .color(theme::fg())
            .wrapping(Wrapping::None),
    ]
    .spacing(6)
    .align_y(iced::Center);
    if let Some(detail) = detail {
        head = head.push(text(detail).size(ts::CAPTION).color(theme::dim()));
    }
    head = head.push(space().width(Fill));
    for control in controls {
        head = head.push(control);
    }
    let head: Element<'a, Message> = match switch {
        Some(switch) => column![head, switch].spacing(6).into(),
        None => head.into(),
    };
    column![
        container(head)
            .padding(Padding {
                top: 4.0,
                right: 8.0,
                bottom: 6.0,
                left: 10.0,
            })
            .width(Fill),
        super::panes::hairline(),
    ]
    .into()
}

/// Expand every node of a tree.
fn expand_all<'a>(msg: Message) -> Element<'a, Message> {
    button(text(tr!("Expand all")).size(ts::SMALL))
        .style(theme::toolbar_button)
        .padding([2, 7])
        .on_press(msg)
        .into()
}

/// Which way a tree reads (callers or callees, imports or importers): a
/// small segmented control, the way it reads raised; the other switches.
fn direction_switch<'a>(sides: [(&'static str, bool, Message); 2]) -> Element<'a, Message> {
    container(
        Row::with_children(sides.into_iter().map(|(label, chosen, msg)| {
            button(text(label).size(ts::SMALL))
                .style(theme::segment(chosen))
                .padding([1, 7])
                .on_press_maybe((!chosen).then_some(msg))
                .into()
        }))
        .spacing(1),
    )
    .padding(2)
    .style(theme::trough)
    .into()
}

/// The FLOW tab: the traced identifier's occurrences under the role each
/// line gives it (declared, assigned, parameter, passed to, returned,
/// branched on, member access, read), each opening its line; a `Passed`
/// row unfolds into the callee's parameter and ITS occurrences.
pub(crate) fn flow_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.proj.flow else {
        return empty_state(Glyph::Search, tr!("No value trace yet"), "", None);
    };
    let pending = app.proj.flow_pending == Some(tree.token);
    let header = tree_head(
        &tree.symbol,
        Some(if pending {
            tr!("tracing…").to_string()
        } else {
            trn!("1 place", "{n} places", tree.node_count())
        }),
        vec![
            button(text(tr!("Clear")).size(ts::SMALL))
                .style(theme::toolbar_button)
                .padding([2, 7])
                .on_press(Message::Flow(crate::FlowMsg::Clear))
                .into(),
        ],
        None,
    );

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if tree.stale {
        rows.push(
            container(
                row![
                    text(tr!("A traced file changed."))
                        .size(ts::CAPTION)
                        .color(theme::warning())
                        .width(Fill),
                    button(text(tr!("Trace again")).size(ts::SMALL))
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
            .width(super::sidebar::CHEVRON_W)
            .align_x(iced::Center)
            .into()
    } else if node.role == crate::app::flow::Role::Passed && node.callee.is_some() {
        super::sidebar::chevron(
            node.expanded,
            Message::Flow(if node.children.is_some() {
                crate::FlowMsg::Toggle { token, id }
            } else {
                crate::FlowMsg::Expand { token, id }
            }),
        )
    } else {
        space().width(super::sidebar::CHEVRON_W).into()
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
        text(tr!("changed"))
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
            tr!("(line not read)").to_string()
        })
        .size(ts::SMALL)
        .font(crate::fonts::CODE)
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

/// The call-hierarchy tree: a header with the root symbol + a callers/callees
/// toggle, then the lazily-expanded tree.
pub(crate) fn calls_tab(app: &App) -> Element<'_, Message> {
    let Some(tree) = &app.proj.call_graph else {
        return empty_state(Glyph::CallGraph, tr!("No call hierarchy yet"), "", None);
    };

    use crate::graph::callgraph::Direction;
    let header = tree_head(
        &tree.root_name,
        None,
        vec![expand_all(Message::Calls(CallsMsg::ExpandAll))],
        Some(direction_switch(
            [Direction::Incoming, Direction::Outgoing].map(|d| {
                (
                    crate::i18n::lookup(d.label()),
                    tree.direction == d,
                    Message::Calls(CallsMsg::Direction),
                )
            }),
        )),
    );

    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    for id in tree.visible() {
        let node = tree.node(id);
        // Expansion affordance: an arrow for fetchable nodes, a loop glyph for
        // recursion, blank for a leaf with no further calls.
        let arrow: Element<'_, Message> = if node.loading {
            text("…")
                .size(ts::SMALL)
                .color(theme::accent())
                .width(super::sidebar::CHEVRON_W)
                .align_x(iced::Center)
                .into()
        } else if node.cyclic {
            text("↺")
                .size(ts::SMALL)
                .color(theme::dim())
                .width(super::sidebar::CHEVRON_W)
                .align_x(iced::Center)
                .into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(super::sidebar::CHEVRON_W).into()
        } else {
            // Names the tree it was drawn from: node ids are bare indices, so
            // a click that lands after a direction flip or a new hierarchy
            // must act on nothing rather than on the new tree's node `id`.
            super::sidebar::chevron(
                node.expanded,
                Message::Calls(CallsMsg::ExpandNode {
                    token: tree.token,
                    id,
                }),
            )
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
            format!(
                "⟳ {}",
                tr!("Code changed — press {keys} to refresh", keys = "gc")
            ),
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
                text(tr!("No file focused."))
                    .size(ts::BODY)
                    .color(theme::dim()),
                space().height(6),
                text(tr!(
                    "Open a source file to see what it imports and what imports it."
                ))
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::Word),
            ]
            .spacing(2),
        )
        .padding(12)
        .into();
    };

    use crate::graph::imports::Dir;
    let header = tree_head(
        &tree.root_name,
        None,
        vec![expand_all(Message::Graph(GraphMsg::ImportExpandAll))],
        Some(direction_switch([Dir::Imports, Dir::Importers].map(|d| {
            (
                crate::i18n::lookup(d.label()),
                tree.direction == d,
                Message::Graph(GraphMsg::ImportDirection),
            )
        }))),
    );

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
                .width(super::sidebar::CHEVRON_W)
                .align_x(iced::Center)
                .into()
        } else if node.children.as_ref().is_some_and(|c| c.is_empty()) {
            space().width(super::sidebar::CHEVRON_W).into()
        } else {
            super::sidebar::chevron(
                node.expanded,
                Message::Graph(GraphMsg::ImportExpand {
                    token: app.proj.import_tree_token,
                    id,
                }),
            )
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
                    text(tr!("ext")).size(ts::CAPTION).color(theme::dim()),
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
            format!("⚠ {}", trn!("1 import cycle", "{n} import cycles", n)),
            theme::warning(),
        ),
    };
    // The graph under this tree is built from the symbol index; files the
    // index had to leave out have no edges here, so say which were left out.
    let capped: Element<'_, Message> = match index_cap_note(app) {
        Some(note) => tree_banner(tr!("Index incomplete: {note}", note = note), theme::dim()),
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

/// How deep the variables panel opens values into values.
const MAX_VAR_DEPTH: usize = 12;

/// `vars` as the panel's rows, `depth` levels in: `name = value`, a value
/// with children opening and closing on a click, its children under it
/// once read. A group with no value of its own (debugpy's "class
/// variables") says only its name: it was drawn "class variables =".
fn push_var_rows<'a>(
    app: &'a App,
    vars: &'a [crate::dap::Variable],
    depth: usize,
    out: &mut Vec<Element<'a, Message>>,
) {
    for v in vars {
        let reference = v.variables_reference;
        let opens = reference > 0 && depth < MAX_VAR_DEPTH;
        let open = opens && app.debug.open_vars.contains(&reference);
        let marker: Element<'a, Message> = if opens {
            glyph::icon(
                if open {
                    Glyph::ChevronDown
                } else {
                    Glyph::ChevronRight
                },
                theme::dim(),
                10.0,
            )
        } else {
            space().width(10).into()
        };
        let mut line = row![
            space().width(depth as f32 * 12.0),
            marker,
            text(v.name.clone()).size(ts::SMALL).color(theme::warning()),
        ]
        .spacing(3)
        .align_y(iced::Center);
        if !v.value.is_empty() {
            line = line
                .push(text("=").size(ts::SMALL).color(theme::dim()))
                .push(
                    text(v.value.clone())
                        .size(ts::SMALL)
                        .color(theme::fg())
                        .wrapping(Wrapping::None),
                );
        }
        out.push(if opens {
            button(line)
                .style(theme::list_row(false))
                .padding(0)
                .on_press(Message::Debug(DebugMsg::ToggleVar(reference)))
                .into()
        } else {
            line.into()
        });
        if open {
            match app.debug.var_children.get(&reference) {
                Some(children) => push_var_rows(app, children, depth + 1, out),
                None => out.push(
                    row![
                        space().width((depth + 1) as f32 * 12.0 + 13.0),
                        text("…").size(ts::SMALL).color(theme::dim()),
                    ]
                    .into(),
                ),
            }
        }
    }
}
