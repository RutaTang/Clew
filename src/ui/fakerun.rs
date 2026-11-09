//! Fake run's inputs sheet and the fake debug panel (see `app::fakerun`).

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use crate::app::fakerun::{Sheet, Sim, field_id};
use crate::fakerun::Field;
use crate::{FakeMsg, SimCmd};

/// The sheet's width: room for a parameter's value written out.
const SHEET_W: f32 = 520.0;

/// What typing into the sheet's `i`th field sends, as a plain function so
/// voice can type into it too (`heard::field`); the sheet has at most this
/// many fields that can be typed into by voice — the rest by hand only.
const FIELD_INPUT: [fn(String) -> Message; 8] = [
    |v| Message::Fake(FakeMsg::Field(0, v)),
    |v| Message::Fake(FakeMsg::Field(1, v)),
    |v| Message::Fake(FakeMsg::Field(2, v)),
    |v| Message::Fake(FakeMsg::Field(3, v)),
    |v| Message::Fake(FakeMsg::Field(4, v)),
    |v| Message::Fake(FakeMsg::Field(5, v)),
    |v| Message::Fake(FakeMsg::Field(6, v)),
    |v| Message::Fake(FakeMsg::Field(7, v)),
];

/// A field's label and the hint in it when it is empty.
fn field_words(field: &Field) -> (String, String) {
    match field {
        Field::Param { name, hint } => (
            name.clone(),
            if hint.is_empty() {
                tr!("a value").to_string()
            } else {
                hint.clone()
            },
        ),
        Field::Inputs => (tr!("Inputs").to_string(), tr!("e.g. 2, 3").to_string()),
        Field::Construct => (
            tr!("Create it with").to_string(),
            tr!("e.g. Cart(items=[])").to_string(),
        ),
        Field::Calls => (
            tr!("Then call").to_string(),
            tr!("e.g. add(\"tea\", 2); total()").to_string(),
        ),
        Field::Context => (
            tr!("Anything else it depends on").to_string(),
            tr!("optional: a file's contents, what a service returns, the time…").to_string(),
        ),
    }
}

/// The inputs sheet: a field per parameter (or how a type is made and
/// used), what else the code depends on, and Run.
pub(crate) fn fake_sheet<'a>(sheet: &'a Sheet) -> Element<'a, Message> {
    let run = Message::Fake(FakeMsg::Run);
    let cancel = Message::Fake(FakeMsg::CloseSheet);
    let mut fields: Vec<Element<'a, Message>> = Vec::new();
    for (i, (field, value)) in sheet.fields.iter().zip(&sheet.values).enumerate() {
        let (label, hint) = field_words(field);
        let label = match field {
            Field::Param { .. } => text(label)
                .size(ts::SMALL)
                .font(crate::fonts::CODE)
                .color(theme::fg()),
            _ => text(label).size(ts::SMALL).color(theme::fg()),
        };
        let mut input = text_input(&hint, value)
            .style(theme::input)
            .id(field_id(i))
            .on_submit(run.clone())
            .size(ts::BASE)
            .padding(6);
        match FIELD_INPUT.get(i) {
            Some(&on_input) => {
                super::heard::field(value, on_input);
                input = input.on_input(on_input);
            }
            None => input = input.on_input(move |v| Message::Fake(FakeMsg::Field(i, v))),
        }
        if matches!(field, Field::Param { .. }) {
            input = input.font(crate::fonts::CODE);
        }
        fields.push(column![label, input].spacing(4).into());
    }
    let suggest_label = if sheet.suggesting.is_some() {
        tr!("Suggesting…")
    } else {
        tr!("Suggest inputs")
    };
    let suggest =
        button(text(suggest_label).size(ts::BODY))
            .style(theme::toolbar_button)
            .padding([4, 12])
            .on_press_maybe(sheet.suggesting.is_none().then(|| {
                super::heard::said(tr!("Suggest inputs"), Message::Fake(FakeMsg::Suggest))
            }));
    let cancel_b = button(text(tr!("Cancel")).size(ts::BODY))
        .style(theme::toolbar_button)
        .padding([4, 12])
        .on_press(super::heard::answer(
            tr!("Cancel"),
            super::heard::Answer::No,
            cancel.clone(),
        ));
    // Fake Debug steps through what Fake Run lands at the end of: the
    // sheet says which it is, and its button does that.
    let (heading, go) = if sheet.debug {
        (tr!("Fake debug"), tr!("Debug"))
    } else {
        (tr!("Fake run"), tr!("Run"))
    };
    let run_b = button(text(go).size(ts::BODY))
        .style(theme::primary_button)
        .padding([4, 12])
        .on_press(super::heard::answer(
            go,
            super::heard::Answer::Yes,
            run.clone(),
        ));
    let title = row![
        text(heading).size(ts::EMPHASIS).color(theme::fg()),
        text(sheet.target.name.clone())
            .size(ts::EMPHASIS)
            .font(crate::fonts::CODE)
            .color(theme::accent())
            .wrapping(Wrapping::None),
    ]
    .spacing(8)
    .align_y(iced::Center);
    let panel = container(
        column![
            title,
            text(tr!(
                "Clew's model works out what it would do with these inputs, line by line. Nothing is run."
            ))
            .size(ts::SMALL)
            .color(theme::dim()),
            Column::with_children(fields).spacing(10),
            row![suggest, space().width(Fill), cancel_b, run_b].spacing(8),
        ]
        .spacing(12),
    )
    .width(SHEET_W)
    .padding(PROMPT_PAD)
    .style(theme::modal_panel);
    modal(
        panel,
        Placement::Top(PROMPT_TOP),
        Backdrop::Dim(Some(cancel)),
    )
}

/// A step control: its words, what it does, and whether it can now.
fn control<'a>(label: &'static str, msg: Message, enabled: bool) -> Element<'a, Message> {
    let mut b = button(text(label).size(ts::BODY).wrapping(Wrapping::None))
        .style(theme::toolbar_button)
        .padding([2, 8]);
    if enabled {
        b = b.on_press(super::heard::said(label, msg));
    }
    b.into()
}

/// A column's heading.
fn heading<'a>(words: &'a str) -> Element<'a, Message> {
    text(words).size(ts::CAPTION).color(theme::dim()).into()
}

/// The bottom panel's fake debug session: the run being worked out, why
/// it brought no trace, or the trace stepped through — its stack, its
/// values (what the line changed marked), and what the model says of it.
pub(crate) fn fake_panel(app: &App) -> Element<'_, Message> {
    let fake = &app.proj.fake;
    let chip = || {
        text(tr!("simulated · not run"))
            .size(ts::CAPTION)
            .color(theme::accent())
            .wrapping(Wrapping::None)
    };
    // Which it was asked as: the run being worked out's, else the trace's,
    // else the last sheet's.
    let debug = fake
        .working
        .as_ref()
        .map(|w| w.debug)
        .or(fake.sim.as_ref().map(|s| s.debug))
        .or(fake.last.as_ref().map(|s| s.debug))
        .unwrap_or(true);
    let lead = |name: &str, status: String| {
        row![
            text(if debug {
                tr!("Fake debug")
            } else {
                tr!("Fake run")
            })
            .size(ts::BASE)
            .color(theme::fg())
            .wrapping(Wrapping::None),
            chip(),
            text(name.to_string())
                .size(ts::SMALL)
                .font(crate::fonts::CODE)
                .color(theme::accent())
                .wrapping(Wrapping::None),
            text(status)
                .size(ts::SMALL)
                .color(theme::dim())
                .wrapping(Wrapping::None),
        ]
        .spacing(8)
        .align_y(iced::Center)
    };
    let close = || control(tr!("■ Close"), Message::Fake(FakeMsg::Close), true);
    let edit = || control(tr!("Edit inputs"), Message::Fake(FakeMsg::EditInputs), true);

    let (header, body): (Element<'_, Message>, Element<'_, Message>) =
        if let Some(w) = fake.working.as_ref() {
            let header = row![
                container(lead(&w.target.name, tr!("working it out…").to_string()))
                    .width(Fill)
                    .clip(true),
                control(tr!("■ Stop"), Message::Fake(FakeMsg::Stop), true),
            ]
            .spacing(8)
            .align_y(iced::Center);
            let mut rows = vec![
                text(tr!(
                    "Clew's model is reading the code it calls, then tracing it line by line."
                ))
                .size(ts::SMALL)
                .color(theme::fg())
                .into(),
            ];
            for read in &w.reading {
                rows.push(
                    text(read.clone())
                        .size(ts::SMALL)
                        .color(theme::dim())
                        .wrapping(Wrapping::None)
                        .into(),
                );
            }
            (header.into(), debug_col(rows))
        } else if let Some(why) = fake.failed.as_ref() {
            let name = fake.last.as_ref().map_or("", |s| s.target.name.as_str());
            let header = row![
                container(lead(name, tr!("no trace").to_string()))
                    .width(Fill)
                    .clip(true),
                control(tr!("Try again"), Message::Fake(FakeMsg::EditInputs), true),
                close(),
            ]
            .spacing(4)
            .align_y(iced::Center);
            let rows = vec![
                text(tr!(
                    "The fake run brought no trace: {why}",
                    why = why.clone()
                ))
                .size(ts::SMALL)
                .color(theme::danger())
                .into(),
            ];
            (header.into(), debug_col(rows))
        } else if let Some(sim) = fake.sim.as_ref() {
            let total = sim.trace.steps.len();
            let status = if sim.ended {
                tr!("finished").to_string()
            } else {
                tr!("step {n} of {total}", n = sim.at + 1, total = total).to_string()
            };
            let live = !sim.ended;
            let controls = row![
                control(
                    tr!("⟲ Restart"),
                    Message::Fake(FakeMsg::Step(SimCmd::Restart)),
                    true
                ),
                control(
                    tr!("◀ Back"),
                    Message::Fake(FakeMsg::Step(SimCmd::Back)),
                    sim.ended || sim.at > 0
                ),
                control(
                    tr!("⤼ Over"),
                    Message::Fake(FakeMsg::Step(SimCmd::Over)),
                    live
                ),
                control(
                    tr!("⤓ In"),
                    Message::Fake(FakeMsg::Step(SimCmd::Into)),
                    live
                ),
                control(
                    tr!("⤒ Out"),
                    Message::Fake(FakeMsg::Step(SimCmd::Out)),
                    live
                ),
                control(
                    tr!("▶ Continue"),
                    Message::Fake(FakeMsg::Step(SimCmd::Continue)),
                    live
                ),
                control(tr!("Ask why"), Message::Fake(FakeMsg::AskWhy), live),
                edit(),
                close(),
            ]
            .spacing(4);
            let header = row![
                container(lead(&sim.target.name, status))
                    .width(Fill)
                    .clip(true),
                controls
            ]
            .spacing(8)
            .align_y(iced::Center);
            (header.into(), sim_columns(sim))
        } else {
            (space().into(), space().into())
        };
    container(column![header, body].spacing(6).padding([8, 12]))
        .width(Fill)
        .height(Fill)
        .style(theme::on_sheet)
        .into()
}

/// The trace's columns: the stack, the values, and the notes.
fn sim_columns(sim: &Sim) -> Element<'_, Message> {
    let trace = &sim.trace;
    let step = trace.steps.get(sim.at);

    // The stack: each frame's line, innermost first; a click goes there.
    let mut stack: Vec<Element<'_, Message>> = vec![heading(tr!("CALL STACK"))];
    for at in trace.stack(sim.at) {
        let s = &trace.steps[at];
        let name = if s.function.is_empty() {
            sim.target.name.clone()
        } else {
            s.function.clone()
        };
        stack.push(
            button(
                column![
                    text(name.clone())
                        .size(ts::SMALL)
                        .color(theme::accent())
                        .wrapping(Wrapping::None),
                    text(format!("{}:{}", s.file, s.line))
                        .size(ts::CAPTION)
                        .color(theme::dim())
                        .wrapping(Wrapping::None),
                ]
                .spacing(0),
            )
            .style(theme::list_row(at == sim.at))
            .width(Fill)
            .padding([1, 6])
            .on_press(super::heard::said(&name, Message::Fake(FakeMsg::Goto(at))))
            .into(),
        );
    }

    // The values once the line has run; what it changed in the warning
    // colour.
    let mut vars: Vec<Element<'_, Message>> = vec![heading(tr!("VARIABLES"))];
    if let Some(step) = step {
        let changed = trace.changed(sim.at);
        for (name, value) in &step.vars {
            let color = if changed.contains(name.as_str()) {
                theme::warning()
            } else {
                theme::fg()
            };
            vars.push(
                row![
                    text(name.clone())
                        .size(ts::SMALL)
                        .font(crate::fonts::CODE)
                        .color(theme::accent())
                        .wrapping(Wrapping::None),
                    text(" = ").size(ts::SMALL).color(theme::dim()),
                    text(value.clone())
                        .size(ts::SMALL)
                        .font(crate::fonts::CODE)
                        .color(color)
                        .wrapping(Wrapping::None),
                ]
                .into(),
            );
        }
    }

    // What the model says of the line, and of the whole run.
    let mut notes: Vec<Element<'_, Message>> = Vec::new();
    if !sim.ended
        && let Some(step) = step
    {
        notes.push(heading(tr!("THIS LINE")));
        if !step.note.is_empty() {
            notes.push(
                text(step.note.clone())
                    .size(ts::SMALL)
                    .color(theme::fg())
                    .into(),
            );
        }
    }
    notes.push(heading(tr!("RETURNS")));
    notes.push(
        text(if trace.returns.is_empty() {
            "—".to_string()
        } else {
            trace.returns.clone()
        })
        .size(ts::SMALL)
        .font(crate::fonts::CODE)
        .color(if sim.ended {
            theme::success()
        } else {
            theme::fg()
        })
        .into(),
    );
    if !trace.output.is_empty() {
        notes.push(heading(tr!("OUTPUT")));
        for line in &trace.output {
            notes.push(
                text(line.clone())
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(theme::fg())
                    .wrapping(Wrapping::None)
                    .into(),
            );
        }
    }
    if !trace.assumptions.is_empty() {
        notes.push(heading(tr!("ASSUMED")));
        for a in &trace.assumptions {
            notes.push(
                text(format!("• {a}"))
                    .size(ts::SMALL)
                    .color(theme::dim())
                    .into(),
            );
        }
    }
    if trace.cut {
        notes.push(
            text(trn!(
                "Cut short at 1 step",
                "Cut short at {n} steps",
                trace.steps.len()
            ))
            .size(ts::CAPTION)
            .color(theme::warning())
            .into(),
        );
    }
    row![debug_col(stack), debug_col(vars), debug_col(notes)]
        .spacing(6)
        .height(Fill)
        .into()
}
