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
        match self.reseat_time_travel(time_travel) {
            Some(reseat) => Task::batch([task, reseat]),
            None => task,
        }
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
        }
    }
}
