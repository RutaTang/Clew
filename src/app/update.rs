//! The update loop. [`App::update`] runs every message through `dispatch` —
//! the one ownership check every async result and transport event passes
//! (their `Stamp` / transport instance, see `Message::origin`), then the hand-
//! off of each feature's sub-enum to that feature's `update_<feature>` in its
//! own module — and afterwards the post-update hooks that re-derive what the
//! view reads, and the explicit release of a server nothing needs any more.

use crate::app::prelude::*;
use crate::*;

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        // Whether the server subscription runs as this message arrives, and
        // over which transport instance (see the end of this function).
        let (served, conn) = (self.wants_server(), self.conn_gen);
        // The pane the time-travel session is drawn in as this message
        // arrives, if any (see the end of this function).
        let time_travel = ui::time_travel_pane(self);
        let task = self.dispatch(message);
        // Derived from the active pane's document, so it is re-anchored HERE
        // rather than at each of the many handlers that can replace what a
        // pane shows (open, server content, watcher reload, notebook rebuild,
        // split, pane focus). Stamped with the document's identity, so this is
        // a comparison and nothing else when the pane did not change.
        self.sync_find_matches();
        self.sync_trail_collapsed();
        self.sync_lsp_snapshots();
        // A walkthrough step waiting for its file's own symbols (see
        // `settle_walk_anchor`), whichever path this message landed them by.
        let task = match self.settle_walk_anchor() {
            Some(jump) => Task::batch([task, jump]),
            None => task,
        };
        // A call-graph refinement holding files — or a full pass held — for
        // a language server starting or loading the project (see
        // `settle_refine_wait`), whichever message readied it.
        let task = match self.settle_refine_wait() {
            Some(refine) => Task::batch([task, refine]),
            None => task,
        };
        // The server's lifetime, made explicit. The subscription runs only
        // while `wants_server` holds; when this message made it stop holding —
        // on the same transport instance (a message that switched or dropped
        // the transport has already torn it down and moved `conn_gen` on) —
        // iced drops the stream after this update, and a dropped stream sends
        // nothing: no `ServerMsg::Disconnected` would ever arrive, and the link
        // would keep naming a writer that is gone. So the app emits that
        // disconnect itself, through the one handler every disconnect takes.
        let task = if served && !self.wants_server() && self.conn_gen == conn {
            let released = self.dispatch(Message::Server(ServerMsg::Disconnected {
                conn,
                reason: Some(crate::app::server::SERVER_RELEASED.into()),
            }));
            Task::batch([task, released])
        } else {
            task
        };
        // Moving the time-travel session to another pane, off the screen or
        // back onto it — which a focus switch, a page over the panes or the
        // split can do, whatever the message — rebuilds the views it swaps
        // at the top. Here, once, rather than at each of those handlers:
        // they are put back where their readers left them.
        let task = match self.reseat_time_travel(time_travel) {
            Some(reseat) => Task::batch([task, reseat]),
            None => task,
        };
        // The question within reach, whatever brought it there: one asked,
        // replaced, or uncovered — by an answer to the modal over it, the
        // tour ending, the window coming back.
        match self.track_question() {
            Some(enable) => Task::batch([task, enable]),
            None => task,
        }
    }

    /// The question whose Allow the user can reach now (see
    /// [`crate::app::model::Asking`]): the overlay on top, with no tour over
    /// it, in the window that has the focus.
    pub(crate) fn question_on_screen(&self) -> Option<crate::app::model::Question> {
        use crate::app::model::Question;
        use crate::ui::ActiveOverlay as O;
        // A widget's own overlay — a picker's menu, a tooltip — is drawn
        // over everything, modals and the tour included, and may hide any
        // question: none is within reach until it goes, and each waits anew.
        if self.tutorial.is_some() || !self.window_focused || self.overlay_open {
            return None;
        }
        match ui::active_overlay(self) {
            Some(O::Consent(root)) => Some(Question::Project(root.to_path_buf())),
            Some(O::LspCommand(_, number, _)) => Some(Question::LspCommand(number)),
            Some(O::LspConsent(_, number, _)) => Some(Question::LspConsent(number)),
            Some(O::Connect) => match self.connect.as_ref().map(|c| &c.stage) {
                Some(ConnectStage::TrustHost { key, .. }) => {
                    Some(Question::TrustHost(key.fingerprint.clone()))
                }
                Some(ConnectStage::HostKeyChanged { forget_host, .. }) => {
                    Some(Question::ForgetHostKey(forget_host.clone()))
                }
                _ => None,
            },
            // Nothing over the window: the approval card pinned in the Ask
            // panel, while the panel shows. A hover's peek counts as over
            // it — one opened over the panel hides it, and Esc took it away
            // from under a pointer resting on an Allow — and its delay
            // starts when the peek goes. The cards are answered in turn, so
            // the next one coming up where the last was answered waits too.
            None => (self.show_bottom && self.bottom_tab == BottomTab::Ask)
                .then(|| self.first_approval())
                .flatten()
                .map(|(turn, approval)| Question::Approval {
                    stream: turn.stream,
                    key: approval.key,
                }),
            Some(_) => None,
        }
    }

    /// The tool call the Ask panel asks about — the first a running turn
    /// waits on (a turn that ended waits for no answer) — and its turn.
    pub(crate) fn first_approval(
        &self,
    ) -> Option<(&crate::app::model::AskTurn, &crate::app::model::Approval)> {
        self.proj
            .ask_turns
            .iter()
            .filter(|t| t.streaming)
            .find_map(|turn| Some((turn, turn.extras.approvals.first()?)))
    }

    /// Whether an Allow given to `question` counts — and is drawn enabled:
    /// it is the question within reach, and has been for the delay, all of
    /// its answers in view where they are for as long (`ui::placed`).
    pub(crate) fn may_allow(&self, question: &crate::app::model::Question) -> bool {
        let asking = &self.asking;
        self.question_on_screen().as_ref() == Some(question)
            && (asking.delay.is_zero()
                || (asking.question.as_ref() == Some(question)
                    && asking.since.elapsed() >= asking.delay
                    && asking.places.iter().any(|p| {
                        &p.question == question
                            && p.place.is_some()
                            && p.since.elapsed() >= asking.delay
                    })))
    }

    /// What `question`'s view needs of the gate ([`crate::ui::placed::Asked`]).
    pub(crate) fn asked(&self, question: crate::app::model::Question) -> crate::ui::placed::Asked {
        let known = self
            .asking
            .places
            .iter()
            .find(|p| p.question == question)
            .map(|p| p.place);
        crate::ui::placed::Asked {
            ready: self.may_allow(&question),
            report: Some((question, known)),
            arm: self.arm_on_enter,
        }
    }

    /// The pointer came onto `question`'s answers where nothing tells the
    /// window it moved (`App::arm_on_enter`): its delay counts from now, as
    /// a move's would, and the redraw that enables its Allow once it is
    /// over is returned.
    fn on_armed(&mut self, question: crate::app::model::Question) -> Task<Message> {
        let Some(placement) = self
            .asking
            .places
            .iter_mut()
            .find(|p| p.question == question)
        else {
            return Task::none();
        };
        placement.since = std::time::Instant::now();
        let delay = self.asking.delay;
        if delay.is_zero() {
            return Task::none();
        }
        Task::perform(async move { tokio::time::sleep(delay).await }, |()| {
            Message::Noop
        })
    }

    /// `question`'s answers laid out at `place` now: a move counts its
    /// delay from now, and the redraw that enables its Allow once the delay
    /// is over is returned.
    fn on_placed(
        &mut self,
        question: crate::app::model::Question,
        place: Option<iced::Rectangle>,
    ) -> Task<Message> {
        use crate::app::model::{MAX_PLACEMENTS, Placement};
        let places = &mut self.asking.places;
        if places
            .iter()
            .any(|p| p.question == question && p.place == place)
        {
            return Task::none();
        }
        places.retain(|p| p.question != question);
        places.push(Placement {
            question,
            place,
            since: std::time::Instant::now(),
        });
        // The oldest go: only the few drawn now report, and one gone that
        // is drawn again reports anew.
        if places.len() > MAX_PLACEMENTS {
            places.remove(0);
        }
        let delay = self.asking.delay;
        if place.is_none() || delay.is_zero() {
            return Task::none();
        }
        Task::perform(async move { tokio::time::sleep(delay).await }, |()| {
            Message::Noop
        })
    }

    /// Note the question within reach now: one that was not counts its delay
    /// from now, and the redraw that enables its Allow once the delay is over
    /// is returned.
    fn track_question(&mut self) -> Option<Task<Message>> {
        let on_screen = self.question_on_screen();
        if on_screen == self.asking.question {
            return None;
        }
        // An approval card come up in a bottom panel too low to show all of
        // it — its answers out of reach — opens the panel to where it fits
        // (once: a panel the user then makes lower stays so).
        if matches!(on_screen, Some(Question::Approval { .. }))
            && self.bottom_height < crate::app::model::CARD_PANEL_HEIGHT
        {
            self.bottom_height = crate::app::model::CARD_PANEL_HEIGHT;
            self.clamp_panel_sizes();
        }
        self.asking.question = on_screen;
        self.asking.since = std::time::Instant::now();
        let delay = self.asking.delay;
        (self.asking.question.is_some() && !delay.is_zero()).then(|| {
            // Built inside the future: a timer needs the runtime, which the
            // update loop is not running on.
            Task::perform(async move { tokio::time::sleep(delay).await }, |()| {
                Message::Noop
            })
        })
    }

    fn dispatch(&mut self, message: Message) -> Task<Message> {
        // The ONE ownership check every asynchronous message passes before
        // any handler sees it: an async result from a project instance (or,
        // when transport-bound, a transport) this window has since left, or a
        // transport event from a dead or replaced transport, is dropped here
        // (see `Stamp`). A late Connected would install the old host's request
        // channel, a late Disconnected would tear down a healthy transport,
        // and a late result would apply another project's state — the
        // handlers below keep only their feature-local sequence checks.
        if let Some(origin) = message.origin()
            && !self.owns_origin(origin)
        {
            #[cfg(test)]
            {
                self.stale_dropped += 1;
            }
            return Task::none();
        }
        // Any action picked from the toolbar "More" menu dismisses it — every
        // row's action, from the one list of what the rows emit (see
        // `tutorial::closes_tools_menu`), not a partial whitelist here.
        if self.show_tools_menu && crate::app::tutorial::closes_tools_menu(&message) {
            self.show_tools_menu = false;
        }
        match message {
            Message::Server(message) => self.update_server(message),
            Message::Connect(message) => self.update_connect(message),
            Message::Project(message) => self.update_project(message),
            Message::Watch(message) => self.update_watch(message),
            Message::Editor(message) => self.update_editor(message),
            Message::Hover(message) => self.update_hover(message),
            Message::Nav(message) => self.update_nav(message),
            Message::Reading(message) => self.update_reading(message),
            Message::Calls(message) => self.update_calls(message),
            Message::Flow(message) => self.update_flow(message),
            Message::Graph(message) => self.update_graph(message),
            Message::Explain(message) => self.update_explain(message),
            Message::Content(message) => self.update_content(message),
            Message::Overview(message) => self.update_overview(message),
            Message::Walk(message) => self.update_walk(message),
            Message::Semantic(message) => self.update_semantic(message),
            Message::Ask(message) => self.update_ask(message),
            Message::Integrations(message) => self.update_integrations(message),
            Message::TimeTravel(message) => self.update_time_travel(message),
            Message::Debug(message) => self.update_debug(message),
            Message::Lsp(message) => self.update_lsp(message),
            Message::Docs(message) => self.update_docs(message),
            Message::Glossary(message) => self.update_glossary(message),
            Message::Export(message) => self.update_export(message),
            Message::Settings(message) => self.update_settings(message),
            Message::Updater(message) => self.update_updater(message),
            Message::Window(message) => self.update_window(message),
            Message::Tutorial(message) => self.update_tutorial(message),
            Message::Tick => self.on_tick(),
            Message::Noop => Task::none(),
            Message::Placed { question, place } => self.on_placed(question, place),
            Message::Armed(question) => self.on_armed(question),
            Message::OverlayOpen(open) => {
                self.overlay_open = open;
                Task::none()
            }
        }
    }
}
