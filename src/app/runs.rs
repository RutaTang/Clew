//! The bottom panel's tabs: every run of the program (Run, Debug) and every
//! run the model works out (Fake Run, Fake Debug), open at once, one tab
//! each.
//!
//! The run on screen is held where one run always was — the real one in
//! [`App::debug`] and the `debug_run*` counters, the fake one in
//! `proj.fake` — and every other is parked beside it ([`ParkedDebug`],
//! [`ParkedFake`]) with all that is its own: its session, its children, its
//! trace, its run id and the token that cancels it. Shown, a tab's run is
//! swapped in; a message of a parked run's is handled with it swapped in,
//! and then out again ([`App::with_parked_debug`]), so each run is driven
//! by the one code that drives the run on screen.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::app::fakerun::ParkedFake;
use crate::app::prelude::*;
use crate::app::state::{ParkedDebug, RunTab};
use crate::*;

/// What a tab shows as its state: how its run is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabState {
    /// Starting, or the model working it out.
    Starting,
    Running,
    /// Paused at a stop: something to read, or to step on from.
    Paused,
    /// Over, and well.
    Done,
    /// Over, and not well: a non-zero exit, a failed start, a fake run
    /// that brought no trace.
    Failed,
}

/// What a tab is: its kind, its name and its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabFace {
    pub tab: RunTab,
    /// Run, not Debug (a plain run of the program); for a fake one, Fake
    /// Run, not Fake Debug.
    pub plain: bool,
    /// What it runs: the program's file name, or the function's name.
    pub name: String,
    pub state: TabState,
}

impl ParkedDebug {
    /// A run with nothing in it, under run id `run`: what the one on screen
    /// becomes as it is parked.
    fn empty(run: u64) -> ParkedDebug {
        ParkedDebug {
            tab: 0,
            run,
            live: Arc::new(AtomicU64::new(run)),
            stop: 0,
            session: None,
            no_debug: false,
            trace: Vec::new(),
            trace_cut: false,
            trace_program: None,
            pending_reason: String::new(),
            said: None,
            ran_for: None,
            last_fn: None,
            hover_safe: false,
            child_started: None,
            children: Default::default(),
            proxied_run: None,
            open_vars: Default::default(),
            var_children: Default::default(),
        }
    }
}

impl App {
    /// A new tab id: above every one handed out.
    pub(crate) fn next_run_tab(&mut self) -> u64 {
        self.run_tab_last += 1;
        self.run_tab_last
    }

    /// A new run id: above the run on screen's and every parked one's (each
    /// keeps its own), so no message can be taken for another run's.
    pub(crate) fn next_debug_run(&mut self) -> u64 {
        self.debug_run_high = self.debug_run_high.max(self.debug_run) + 1;
        self.debug_run_high
    }

    /// Swap the run on screen's own state with `p`'s.
    fn swap_debug(&mut self, p: &mut ParkedDebug) {
        use std::mem::swap;
        swap(&mut self.debug.tab, &mut p.tab);
        swap(&mut self.debug_run, &mut p.run);
        swap(&mut self.debug_run_live, &mut p.live);
        swap(&mut self.debug_stop, &mut p.stop);
        swap(&mut self.debug.session, &mut p.session);
        swap(&mut self.debug.no_debug, &mut p.no_debug);
        swap(&mut self.debug.trace, &mut p.trace);
        swap(&mut self.debug.trace_cut, &mut p.trace_cut);
        swap(&mut self.debug.trace_program, &mut p.trace_program);
        swap(&mut self.debug.pending_reason, &mut p.pending_reason);
        swap(&mut self.debug.said, &mut p.said);
        swap(&mut self.debug.ran_for, &mut p.ran_for);
        swap(&mut self.debug.last_fn, &mut p.last_fn);
        swap(&mut self.debug.hover_safe, &mut p.hover_safe);
        swap(&mut self.debug.child_started, &mut p.child_started);
        swap(&mut self.debug.children, &mut p.children);
        swap(&mut self.debug.proxied_run, &mut p.proxied_run);
        swap(&mut self.debug.open_vars, &mut p.open_vars);
        swap(&mut self.debug.var_children, &mut p.var_children);
        // What is drawn from the trace (the graphs' rings) is another run's.
        self.debug.trace_rev += 1;
    }

    /// Swap the run on screen with the parked one at `i`.
    fn swap_parked_debug(&mut self, i: usize) {
        let mut p = self.debug_parked.remove(i);
        self.swap_debug(&mut p);
        self.debug_parked.insert(i, p);
    }

    /// Put the run on screen aside, if there is one: a new one is to be
    /// started in its place.
    pub(crate) fn park_debug(&mut self) {
        if self.debug.session.is_none() {
            return;
        }
        let run = self.next_debug_run();
        let mut p = ParkedDebug::empty(run);
        self.swap_debug(&mut p);
        self.debug_parked.push(p);
    }

    /// The parked run `message` belongs to, by its place among them: none
    /// for the run on screen's, or none's.
    pub(crate) fn parked_owner(&self, message: &DebugMsg) -> Option<usize> {
        let run = message.run().filter(|&run| run != self.debug_run)?;
        self.debug_parked.iter().position(|p| p.run == run)
    }

    /// Handle `message` for the parked run at `i`, whose it is: swapped in,
    /// handled as the run on screen's are, and swapped out again — what it
    /// would show (the file it stopped in) not shown. A stop brings it on
    /// screen instead, unless the run on screen is paused too, being read:
    /// then it is only said.
    pub(crate) fn with_parked_debug(&mut self, i: usize, message: DebugMsg) -> Task<Message> {
        let reading = self.shown_run_tab() == Some(RunTab::Real(self.debug.tab))
            && self
                .debug
                .session
                .as_ref()
                .is_some_and(|s| s.status == DebugStatus::Stopped);
        if message.is_stop() && !reading {
            let tab = self.debug_parked[i].tab;
            let shown = self.show_run_tab(RunTab::Real(tab));
            return Task::batch([shown, self.update_debug_here(message)]);
        }
        let stop = message.is_stop();
        self.swap_parked_debug(i);
        let status = self.status.clone();
        self.debug_off_screen = true;
        let task = self.update_debug_here(message);
        self.debug_off_screen = false;
        let name = self.debug_tab_name();
        let gone = self.debug.session.is_none();
        // Said paused only when it is: a stop of a run still starting is
        // not one.
        let paused = self
            .debug
            .session
            .as_ref()
            .is_some_and(|s| s.status == DebugStatus::Stopped);
        // What it said is its own, said when it comes on screen — but its
        // start failing, its tab going with it, is said now.
        if !gone {
            self.status = status;
        }
        self.swap_parked_debug(i);
        if gone {
            self.debug_parked.remove(i);
        } else if stop && paused {
            self.status = tr!("{name} paused — its tab shows where", name = name);
        }
        // Ended in a tab not on screen: it stays, to be read.
        task
    }

    /// Do `f` for each run in a tab not on screen, as if it were on screen.
    pub(crate) fn for_each_parked_debug(&mut self, mut f: impl FnMut(&mut App)) {
        for i in 0..self.debug_parked.len() {
            self.swap_parked_debug(i);
            self.debug_off_screen = true;
            f(self);
            self.debug_off_screen = false;
            self.swap_parked_debug(i);
        }
        self.debug_parked.retain(|p| p.session.is_some());
    }

    /// What the real run on screen runs, for a tab and the status line.
    fn debug_tab_name(&self) -> String {
        self.debug
            .session
            .as_ref()
            .and_then(|s| s.program.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Bring the parked run of tab `tab` on screen, the run there parked in
    /// its place. `false` when no parked run has that tab.
    fn unpark_debug(&mut self, tab: u64) -> bool {
        let Some(i) = self.debug_parked.iter().position(|p| p.tab == tab) else {
            return false;
        };
        self.swap_parked_debug(i);
        // The run swapped out had nothing in it: no tab to keep.
        if self.debug_parked[i].session.is_none() {
            self.debug_parked.remove(i);
        }
        // The gutter: this run's answers about the breakpoints.
        self.forget_adapter_verdicts();
        self.show_own_debug_verdicts();
        true
    }

    /// Put fake run `tab` aside, the fake one on screen parked in its place
    /// — or, `tab` 0, park the one on screen for a new one.
    fn swap_fake(&mut self, p: &mut ParkedFake) {
        use std::mem::swap;
        let fake = &mut self.proj.fake;
        swap(&mut fake.tab, &mut p.tab);
        swap(&mut fake.working, &mut p.working);
        swap(&mut fake.sim, &mut p.sim);
        swap(&mut fake.failed, &mut p.failed);
        swap(&mut fake.last, &mut p.last);
    }

    /// Put the fake run on screen aside, if there is one.
    pub(crate) fn park_fake(&mut self) {
        if !self.proj.fake.shown() {
            return;
        }
        let mut p = ParkedFake::default();
        self.swap_fake(&mut p);
        self.proj.fake.parked.push(p);
    }

    /// Bring the parked fake run of tab `tab` on screen. `false` when none
    /// has that tab.
    fn unpark_fake(&mut self, tab: u64) -> bool {
        let Some(i) = self.proj.fake.parked.iter().position(|p| p.tab == tab) else {
            return false;
        };
        let mut p = self.proj.fake.parked.remove(i);
        self.swap_fake(&mut p);
        if p.shown() {
            self.proj.fake.parked.insert(i, p);
        }
        true
    }

    /// Handle a fake-run message for the parked run it belongs to: its
    /// answer taken in its own tab, nothing of it shown. `matches` says
    /// whether a run is the message's.
    pub(crate) fn with_parked_fake(
        &mut self,
        matches: impl Fn(&crate::app::fakerun::Working) -> bool,
        handle: impl FnOnce(&mut App) -> Task<Message>,
    ) -> Option<Task<Message>> {
        if self.proj.fake.working.as_ref().is_some_and(&matches) {
            return None;
        }
        let i = self
            .proj
            .fake
            .parked
            .iter()
            .position(|p| p.working.as_ref().is_some_and(&matches))?;
        let mut p = self.proj.fake.parked.remove(i);
        self.swap_fake(&mut p);
        let status = self.status.clone();
        let bottom = self.show_bottom;
        self.debug_off_screen = true;
        let task = handle(self);
        self.debug_off_screen = false;
        self.show_bottom = bottom;
        let name = self.proj.fake.sim.as_ref().map(|s| s.target.name.clone());
        let failed = self.proj.fake.failed.is_some();
        self.status = status;
        self.swap_fake(&mut p);
        self.proj.fake.parked.insert(i, p);
        if let Some(name) = name {
            self.status = tr!("The fake run of {name} is ready in its tab", name = name);
        } else if failed {
            self.status = tr!("A fake run failed — its tab says why").into();
        }
        Some(task)
    }

    /// The bottom panel's tabs, in the order they were opened.
    pub(crate) fn run_tabs(&self) -> Vec<RunTab> {
        let mut tabs: Vec<RunTab> = Vec::new();
        if self.debug.session.is_some() {
            tabs.push(RunTab::Real(self.debug.tab));
        }
        tabs.extend(
            self.debug_parked
                .iter()
                .filter(|p| p.session.is_some())
                .map(|p| RunTab::Real(p.tab)),
        );
        if self.proj.fake.shown() {
            tabs.push(RunTab::Fake(self.proj.fake.tab));
        }
        tabs.extend(
            self.proj
                .fake
                .parked
                .iter()
                .filter(|p| p.shown())
                .map(|p| RunTab::Fake(p.tab)),
        );
        tabs.sort_by_key(|t| t.id());
        tabs
    }

    /// The tab on screen: the one shown last while it is there, else the
    /// newest of the runs on screen.
    pub(crate) fn shown_run_tab(&self) -> Option<RunTab> {
        let tabs = self.run_tabs();
        self.run_shown.filter(|t| tabs.contains(t)).or_else(|| {
            let real = self
                .debug
                .session
                .is_some()
                .then_some(RunTab::Real(self.debug.tab));
            let fake = self
                .proj
                .fake
                .shown()
                .then_some(RunTab::Fake(self.proj.fake.tab));
            real.into_iter().chain(fake).max_by_key(|t| t.id())
        })
    }

    /// Whether the real run on screen is the tab shown: what it shows in the
    /// code (its line, a hovered value) is shown only then.
    pub(crate) fn real_run_shown(&self) -> bool {
        self.debug.session.is_some() && self.shown_run_tab() == Some(RunTab::Real(self.debug.tab))
    }

    /// Whether the fake run on screen is the tab shown.
    pub(crate) fn fake_run_shown(&self) -> bool {
        self.proj.fake.shown() && self.shown_run_tab() == Some(RunTab::Fake(self.proj.fake.tab))
    }

    /// How each tab is: its kind, its name, its state.
    pub(crate) fn run_tab_faces(&self) -> Vec<TabFace> {
        let real = |tab: u64, session: &DebugSession, plain: bool| TabFace {
            tab: RunTab::Real(tab),
            plain,
            name: session
                .program
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            state: match (session.status, session.exit_code) {
                (DebugStatus::Launching, _) => TabState::Starting,
                (DebugStatus::Running, _) => TabState::Running,
                (DebugStatus::Stopped, _) => TabState::Paused,
                (DebugStatus::Terminated, Some(code)) if code != 0 => TabState::Failed,
                (DebugStatus::Terminated, _) => TabState::Done,
            },
        };
        let fake = |tab: u64,
                    working: Option<&crate::app::fakerun::Working>,
                    sim: Option<&crate::app::fakerun::Sim>,
                    failed: bool,
                    last: Option<&crate::app::fakerun::Sheet>| {
            let (name, debug) = match (working, sim, last) {
                (Some(w), _, _) => (w.target.name.clone(), w.debug),
                (_, Some(s), _) => (s.target.name.clone(), s.debug),
                (_, _, Some(l)) => (l.target.name.clone(), l.debug),
                _ => (String::new(), false),
            };
            TabFace {
                tab: RunTab::Fake(tab),
                plain: !debug,
                name,
                state: if working.is_some() {
                    TabState::Starting
                } else if failed {
                    TabState::Failed
                } else if sim.is_some_and(|s| s.debug && !s.ended) {
                    TabState::Paused
                } else {
                    TabState::Done
                },
            }
        };
        let mut faces = Vec::new();
        if let Some(session) = self.debug.session.as_ref() {
            faces.push(real(self.debug.tab, session, self.debug.no_debug));
        }
        for p in &self.debug_parked {
            if let Some(session) = &p.session {
                faces.push(real(p.tab, session, p.no_debug));
            }
        }
        let f = &self.proj.fake;
        if f.shown() {
            faces.push(fake(
                f.tab,
                f.working.as_ref(),
                f.sim.as_ref(),
                f.failed.is_some(),
                f.last.as_ref(),
            ));
        }
        for p in f.parked.iter().filter(|p| p.shown()) {
            faces.push(fake(
                p.tab,
                p.working.as_ref(),
                p.sim.as_ref(),
                p.failed.is_some(),
                p.last.as_ref(),
            ));
        }
        faces.sort_by_key(|f| f.tab.id());
        faces
    }

    /// Show tab `tab`: its run comes on screen, and where it is in the code
    /// — a paused run's line, a fake run's step — is shown.
    pub(crate) fn show_run_tab(&mut self, tab: RunTab) -> Task<Message> {
        if !self.run_tabs().contains(&tab) {
            return Task::none();
        }
        self.run_shown = Some(tab);
        self.show_bottom = true;
        match tab {
            RunTab::Real(id) => {
                if self.debug.tab != id && !self.unpark_debug(id) {
                    return Task::none();
                }
                if let Some(said) = self.debug.said.clone() {
                    self.status = said;
                }
                match self.debug.session.as_ref().and_then(|s| s.current.clone()) {
                    Some((path, line)) => self.open_file(path, Some(line), false),
                    None => Task::none(),
                }
            }
            RunTab::Fake(id) => {
                if self.proj.fake.tab != id {
                    let mut empty = ParkedFake::default();
                    if !self.proj.fake.parked.iter().any(|p| p.tab == id) {
                        return Task::none();
                    }
                    // The one on screen aside, then this one in.
                    self.swap_fake(&mut empty);
                    if empty.shown() {
                        self.proj.fake.parked.push(empty);
                    }
                    self.unpark_fake(id);
                }
                self.fake_show_step()
            }
        }
    }

    /// Close tab `tab`: its run ended if it still runs, and the tab next to
    /// it shown in its place.
    pub(crate) fn close_run_tab(&mut self, tab: RunTab) -> Task<Message> {
        let tabs = self.run_tabs();
        let Some(at) = tabs.iter().position(|&t| t == tab) else {
            return Task::none();
        };
        let was_shown = self.shown_run_tab() == Some(tab);
        let ended = match tab {
            RunTab::Real(id) if self.debug.tab == id => {
                let ended = self.stop_debug_session();
                self.debug.tab = 0;
                self.unsay_debug_tab();
                ended
            }
            RunTab::Real(id) => {
                let Some(i) = self.debug_parked.iter().position(|p| p.tab == id) else {
                    return Task::none();
                };
                self.swap_parked_debug(i);
                let status = self.status.clone();
                let ended = self.stop_debug_session();
                self.status = status;
                self.swap_parked_debug(i);
                self.debug_parked.remove(i);
                ended
            }
            RunTab::Fake(id) if self.proj.fake.tab == id => {
                let ended = self.stop_fake_work();
                let f = &mut self.proj.fake;
                f.sim = None;
                f.failed = None;
                f.last = None;
                f.tab = 0;
                self.forget_failure_of(crate::app::state::FailureOf::FakeRun);
                ended
            }
            RunTab::Fake(id) => {
                let Some(i) = self.proj.fake.parked.iter().position(|p| p.tab == id) else {
                    return Task::none();
                };
                let mut p = self.proj.fake.parked.remove(i);
                self.swap_fake(&mut p);
                let ended = self.stop_fake_work();
                self.swap_fake(&mut p);
                ended
            }
        };
        if !was_shown {
            return ended;
        }
        // The tab after it, else the one before.
        let next = tabs
            .get(at + 1)
            .or_else(|| at.checked_sub(1).and_then(|i| tabs.get(i)))
            .copied();
        self.run_shown = next;
        match next {
            Some(next) => Task::batch([ended, self.show_run_tab(next)]),
            None => ended,
        }
    }

    /// The run on screen's words taken off the status line: its tab is
    /// closed.
    fn unsay_debug_tab(&mut self) {
        if let Some(said) = self.debug.said.take() {
            self.settle_status(&[&said]);
        }
    }

    /// End every real run, on screen and parked: the project they run in is
    /// left, or the window goes.
    pub(crate) fn stop_every_debug_run(&mut self) -> Task<Message> {
        let mut ended = vec![self.stop_debug_session()];
        self.debug.tab = 0;
        while !self.debug_parked.is_empty() {
            self.swap_parked_debug(0);
            ended.push(self.stop_debug_session());
            self.swap_parked_debug(0);
            self.debug_parked.remove(0);
        }
        self.run_shown = None;
        Task::batch(ended)
    }

    /// Where a new real run goes: in the tab of an ended run of the same
    /// program the same way (Run, Debug) — it takes its place, as running
    /// it again — else a new tab. The run on screen is parked first.
    pub(crate) fn tab_for_new_run(
        &mut self,
        program: &Path,
        no_debug: bool,
    ) -> (u64, Task<Message>) {
        let ended_same = |session: &Option<DebugSession>, plain: bool| {
            session.as_ref().is_some_and(|s| {
                s.status == DebugStatus::Terminated && s.program == program && plain == no_debug
            })
        };
        if ended_same(&self.debug.session, self.debug.no_debug) {
            let tab = self.debug.tab;
            let ended = self.stop_debug_session();
            return (if tab == 0 { self.next_run_tab() } else { tab }, ended);
        }
        if let Some(i) = self
            .debug_parked
            .iter()
            .position(|p| ended_same(&p.session, p.no_debug))
        {
            self.park_debug();
            let mut p = self.debug_parked.remove(i);
            self.swap_debug(&mut p);
            let tab = self.debug.tab;
            let ended = self.stop_debug_session();
            return (tab, ended);
        }
        self.park_debug();
        (self.next_run_tab(), Task::none())
    }
}
