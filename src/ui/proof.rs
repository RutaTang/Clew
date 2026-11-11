//! Step Through Proof's panel (see `app::proof`): the tactic shown, the
//! goals before and after it — what it brought, changed and closed marked —
//! and the proof's steps to go to.

use super::*;
// Explicit macro imports shadow the glob from `super`, disambiguating
// iced's column!/row! from the prelude macros of the same name.
use iced::widget::{column, row};

use clew_core::leanproof::{self, Change};

use super::fakerun::{control, heading};
use crate::ProofMsg;
use crate::app::proof::ProofWalk;

/// The bottom panel's proof: being asked, why it could not be, or its
/// steps.
pub(crate) fn proof_panel(app: &App) -> Element<'_, Message> {
    let Some(walk) = app.proj.proof.as_ref() else {
        return space().into();
    };
    let ready = walk.goals.is_some();
    let total = walk.steps.len();
    let status = if walk.failed.is_some() {
        tr!("no answer").to_string()
    } else if !ready {
        tr!("asking the Lean server…").to_string()
    } else {
        tr!("step {n} of {total}", n = walk.at + 1, total = total).to_string()
    };
    let lead = row![
        text(tr!("Proof"))
            .size(ts::BASE)
            .color(theme::fg())
            .wrapping(Wrapping::None),
        text(walk.name.clone())
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
    .align_y(iced::Center);
    let last = total.saturating_sub(1);
    let controls = row![
        control(
            tr!("⟲ Start"),
            Message::Proof(ProofMsg::Goto(0)),
            ready && walk.at > 0
        ),
        control(
            tr!("◀ Back"),
            Message::Proof(ProofMsg::Step(false)),
            ready && walk.at > 0
        ),
        control(
            tr!("▶ Next"),
            Message::Proof(ProofMsg::Step(true)),
            ready && walk.at < last
        ),
        control(
            tr!("⏭ End"),
            Message::Proof(ProofMsg::Goto(last)),
            ready && walk.at < last
        ),
        control(tr!("■ Close"), Message::Proof(ProofMsg::Close), true),
    ]
    .spacing(4);
    let header = row![container(lead).width(Fill).clip(true), controls]
        .spacing(8)
        .align_y(iced::Center);
    let body: Element<'_, Message> = if let Some(why) = walk.failed.as_ref() {
        super::panels::debug_col(vec![
            text(tr!(
                "The Lean server gave no goals: {why}",
                why = why.clone()
            ))
            .size(ts::SMALL)
            .color(theme::danger())
            .into(),
        ])
    } else if !ready {
        // The header says it is asking the Lean server.
        super::panels::debug_col(Vec::new())
    } else {
        proof_columns(walk)
    };
    container(column![header, body].spacing(6).padding([8, 12]))
        .width(Fill)
        .height(Fill)
        .style(theme::on_sheet)
        .into()
}

/// The steps, the goals before the one shown, and the goals after it.
fn proof_columns(walk: &ProofWalk) -> Element<'_, Message> {
    let mut steps: Vec<Element<'_, Message>> = vec![heading(tr!("STEPS"))];
    for (i, step) in walk.steps.iter().enumerate() {
        let words = if i == 0 {
            tr!("(start)").to_string()
        } else {
            step.text.clone()
        };
        steps.push(
            button(
                text(words.clone())
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(if i == walk.at {
                        theme::fg()
                    } else {
                        theme::fg_muted()
                    })
                    .wrapping(Wrapping::None),
            )
            .style(theme::list_row(i == walk.at))
            .width(Fill)
            .padding([1, 6])
            .on_press(super::heard::said(
                &words,
                Message::Proof(ProofMsg::Goto(i)),
            ))
            .into(),
        );
    }
    let before = walk.before(walk.at);
    let after = walk.after(walk.at);
    let mut was: Vec<Element<'_, Message>> = vec![heading(tr!("GOALS BEFORE"))];
    match before {
        Some(goals) => was.extend(goal_rows(goals, None)),
        None => was.push(
            text(tr!("The proof starts here."))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
        ),
    }
    let mut now: Vec<Element<'_, Message>> = vec![heading(tr!("GOALS AFTER"))];
    match after {
        Some([]) => now.push(
            text(tr!("No goals left: proved."))
                .size(ts::SMALL)
                .color(theme::success())
                .into(),
        ),
        Some(goals) => {
            if let Some(closed) = before
                .map(<[String]>::len)
                .and_then(|b| b.checked_sub(goals.len()))
                .filter(|&n| n > 0)
            {
                now.push(
                    text(trn!("1 goal closed", "{n} goals closed", closed))
                        .size(ts::SMALL)
                        .color(theme::success())
                        .into(),
                );
            }
            now.extend(goal_rows(goals, before));
        }
        None => now.push(
            text(tr!("The Lean server has no tactic state here."))
                .size(ts::SMALL)
                .color(theme::dim())
                .into(),
        ),
    }
    row![
        super::panels::debug_col(steps),
        super::panels::debug_col(was),
        super::panels::debug_col(now)
    ]
    .spacing(6)
    .height(Fill)
    .into()
}

/// Each goal of `goals`: its case, its hypotheses and what is to be shown;
/// next to `before`, what is new marked `+` and what changed `~`, in their
/// colours, and a target that changed in the accent.
fn goal_rows<'a>(goals: &[String], before: Option<&[String]>) -> Vec<Element<'a, Message>> {
    let prior = before
        .and_then(|b| b.first())
        .map(|g| leanproof::parse_goal(g));
    let mut rows: Vec<Element<'a, Message>> = Vec::new();
    for (i, text_of) in goals.iter().enumerate() {
        let goal = leanproof::parse_goal(text_of);
        // The goal a tactic works on is the first; the rest wait their
        // turn, compared with nothing.
        let against = if i == 0 { prior.as_ref() } else { None };
        if let Some(case) = &goal.case {
            rows.push(
                text(format!("case {case}"))
                    .size(ts::CAPTION)
                    .font(crate::fonts::CODE)
                    .color(theme::dim())
                    .into(),
            );
        }
        let changes = leanproof::hyp_changes(against, &goal);
        for ((names, ty), change) in goal.hyps.iter().zip(changes) {
            let (mark, color) = match change {
                Change::New => ("+ ", theme::success()),
                Change::Changed => ("~ ", theme::warning()),
                Change::Same => ("  ", theme::fg()),
            };
            rows.push(
                text(format!("{mark}{} : {ty}", names.join(" ")))
                    .size(ts::SMALL)
                    .font(crate::fonts::CODE)
                    .color(color)
                    .into(),
            );
        }
        let target_changed = against.is_some_and(|b| b.target != goal.target);
        rows.push(
            text(format!("⊢ {}", goal.target))
                .size(ts::SMALL)
                .font(crate::fonts::CODE)
                .color(if target_changed {
                    theme::accent()
                } else {
                    theme::fg()
                })
                .into(),
        );
        if i + 1 < goals.len() {
            rows.push(space().height(6).into());
        }
    }
    rows
}
