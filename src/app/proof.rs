//! Step Through Proof (Lean): a declaration's tactic proof walked one
//! tactic at a time, as a debugger walks a program — the goals after each
//! tactic read from the Lean server (`$/lean/plainGoal`), next to the goals
//! before it, with what the tactic brought, changed and closed marked, and
//! its line lit in the code. Real answers, not the model's: the server
//! elaborates the proof as Lean checks it.
//!
//! Its messages, [`ProofMsg`], arrive through `App::update_proof`.

use std::path::{Path, PathBuf};

use clew_core::leanproof::{self, ProofStep};

use crate::app::prelude::*;
use crate::*;

/// A proof being stepped through.
#[derive(Debug, Clone)]
pub struct ProofWalk {
    /// The file it is in.
    pub(crate) abs: PathBuf,
    /// What is proved: the declaration's name.
    pub(crate) name: String,
    /// Its start, then each tactic.
    pub(crate) steps: Vec<ProofStep>,
    /// The goals after each step, once the Lean server has answered for
    /// all of them (`None` while it is asked); a step it has no tactic
    /// state at is `None` in it.
    pub(crate) goals: Option<Vec<Option<Vec<String>>>>,
    /// Why the server could not be asked, or failed to answer.
    pub(crate) failed: Option<String>,
    /// The step shown.
    pub(crate) at: usize,
    /// Which asking the answer must be for: a later one replaces it.
    pub(crate) token: u64,
}

impl ProofWalk {
    /// The goals after step `i`, when known.
    pub(crate) fn after(&self, i: usize) -> Option<&[String]> {
        self.goals.as_ref()?.get(i)?.as_deref()
    }

    /// The goals before step `i`: those after the one before it.
    pub(crate) fn before(&self, i: usize) -> Option<&[String]> {
        self.after(i.checked_sub(1)?)
    }
}

/// The steps of the innermost declaration of `source` around 0-based
/// `line` that is proved by tactics, with its name. Symbols' lines are
/// 1-based, as the outline gives them.
pub(crate) fn proof_at(
    source: &str,
    symbols: &[clew_core::outline::Symbol],
    line: usize,
) -> Option<(String, Vec<ProofStep>)> {
    symbols
        .iter()
        .filter(|s| s.line <= line + 1 && line < s.end_line.max(s.line))
        .filter_map(|s| {
            let steps = leanproof::proof_steps(source, s.line - 1, s.end_line.max(s.line) - 1);
            (!steps.is_empty()).then(|| (s.end_line.saturating_sub(s.line), s.name.clone(), steps))
        })
        .min_by_key(|(span, ..)| *span)
        .map(|(_, name, steps)| (name, steps))
}

impl App {
    /// Step Through Proof from the context menu: the proof of the
    /// declaration it was opened on, asked of the Lean server step by step.
    fn on_proof_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let Some(v) = self.proj.panes.get(menu.pane).and_then(Option::as_ref) else {
            return Task::none();
        };
        if v.lang_key != Some("lean") {
            return Task::none();
        }
        let Some((name, steps)) = proof_at(&v.source, &v.symbols, menu.line) else {
            self.status =
                tr!("No tactic proof here: put the cursor in a theorem proved `by` tactics").into();
            return Task::none();
        };
        let abs = v.abs.clone();
        let client = match self.proj.link.lsp.get("lean") {
            Some(LspSlot::Ready(c)) => c.clone(),
            Some(LspSlot::Starting | LspSlot::AwaitingConsent) => {
                self.status =
                    tr!("The Lean server is still starting — ask again once it is ready").into();
                return Task::none();
            }
            _ => {
                self.status = tr!(
                    "The Lean server is not running — start it from the language servers panel"
                )
                .into();
                return Task::none();
            }
        };
        // Where each step's goals are read: the end of its text, in the
        // server's units.
        let positions: Vec<(usize, usize)> = steps
            .iter()
            .map(|s| {
                let text = v.source_line(s.last).unwrap_or("");
                let end = s.end.min(text.len());
                let end = (0..=end)
                    .rev()
                    .find(|&i| text.is_char_boundary(i))
                    .unwrap_or(0);
                (s.last, client.encoding.units_in(&text[..end]))
            })
            .collect();
        self.proof_seq += 1;
        let token = self.proof_seq;
        self.proj.proof = Some(ProofWalk {
            abs: abs.clone(),
            name: name.clone(),
            steps,
            goals: None,
            failed: None,
            at: 0,
            token,
        });
        self.show_bottom = true;
        self.status = tr!(
            "Asking the Lean server for each step of {name}…",
            name = name
        );
        let stamp = self.stamp();
        Task::perform(
            async move {
                let mut goals = Vec::with_capacity(positions.len());
                for (line, character) in positions {
                    goals.push(client.lean_tactic_goals(&abs, line, character).await?);
                }
                Ok(goals)
            },
            move |result| {
                Message::Proof(ProofMsg::Answered {
                    stamp: stamp.clone(),
                    token,
                    result,
                })
            },
        )
    }

    /// The Lean server's goals for each step of the proof asked
    /// (`token`): shown from its start.
    fn on_proof_answered(
        &mut self,
        token: u64,
        result: Result<Vec<Option<Vec<String>>>, String>,
    ) -> Task<Message> {
        let Some(walk) = self.proj.proof.as_mut().filter(|w| w.token == token) else {
            return Task::none();
        };
        match result {
            Ok(goals) => {
                walk.goals = Some(goals);
                walk.at = 0;
                self.status = tr!("Proof ready — step through it below").into();
                self.proof_show_step()
            }
            Err(why) => {
                walk.failed = Some(why);
                self.status = tr!("The Lean server gave no goals for this proof").into();
                Task::none()
            }
        }
    }

    /// Show the step the walk is at: its line, in view.
    fn proof_show_step(&mut self) -> Task<Message> {
        let Some(walk) = self.proj.proof.as_ref() else {
            return Task::none();
        };
        let Some(step) = walk.steps.get(walk.at) else {
            return Task::none();
        };
        let (abs, line) = (walk.abs.clone(), step.first + 1);
        self.open_file(abs, Some(line), false)
    }

    /// The 1-based line of `abs` the proof walk is on, lit in the code as
    /// a paused debugger's is; none while a run's tab is shown.
    pub(crate) fn proof_line(&self, abs: &Path) -> Option<usize> {
        if self.shown_run_tab().is_some() {
            return None;
        }
        let walk = self.proj.proof.as_ref().filter(|w| w.goals.is_some())?;
        (walk.abs == abs).then(|| walk.steps.get(walk.at).map(|s| s.first + 1))?
    }

    /// Handle a [`ProofMsg`].
    pub(crate) fn update_proof(&mut self, message: ProofMsg) -> Task<Message> {
        match message {
            ProofMsg::FromMenu => self.on_proof_from_menu(),
            ProofMsg::Answered { token, result, .. } => self.on_proof_answered(token, result),
            ProofMsg::Step(forward) => {
                let Some(walk) = self.proj.proof.as_mut().filter(|w| w.goals.is_some()) else {
                    return Task::none();
                };
                let last = walk.steps.len().saturating_sub(1);
                let at = if forward {
                    (walk.at + 1).min(last)
                } else {
                    walk.at.saturating_sub(1)
                };
                if at == walk.at {
                    return Task::none();
                }
                walk.at = at;
                self.proof_show_step()
            }
            ProofMsg::Goto(at) => {
                let Some(walk) = self
                    .proj
                    .proof
                    .as_mut()
                    .filter(|w| w.goals.is_some() && at < w.steps.len())
                else {
                    return Task::none();
                };
                walk.at = at;
                self.proof_show_step()
            }
            ProofMsg::Close => {
                self.proj.proof = None;
                Task::none()
            }
        }
    }
}
