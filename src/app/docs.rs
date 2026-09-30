//! The DOCS tab: requesting the project's API documentation from the server,
//! its freshness, the doc page shown in the main pane, and "View docs" for the
//! symbol under the cursor.
//!
//! Its messages, [`DocsMsg`], arrive through `App::update_docs`.

use crate::app::prelude::*;
use crate::*;

/// The same "not a real registry revision" stamp for `docs.rev`, written when a
/// `BuildDocs` is abandoned without an index (the transport died, the server
/// refused or failed). `request_docs` stamps the revision it asked at BEFORE
/// the answer exists, so leaving that stamp behind on an abandoned build would
/// leave the PREVIOUS index labelled with the CURRENT revision — the one
/// over-claim this freshness key must never make.
pub(crate) const DOCS_REV_STALE: u64 = u64::MAX;

impl App {
    /// Ask the server to (re)build the project's API docs. The `Docs` reply
    /// lands in `handle_server_reply`, correlated by its request id
    /// (`pending_docs`), so a build abandoned by a project switch or a
    /// reconnect can never land on the next one.
    ///
    /// Stamps `docs.rev` with the registry revision this build reads, so a later
    /// change marks the result stale — the same stamp-at-request-time discipline
    /// `start_stats` uses. Without it the index had no freshness key at all and
    /// a non-empty one survived every edit made from another sidebar tab.
    pub(crate) fn request_docs(&mut self) {
        // Single-flight: a build reads every file, and the one in flight
        // already answers this request. A refresh pressed during a build is
        // therefore dropped rather than queued. That is safe, not silent data
        // loss: this build's stamp is the revision it asked at, so any change
        // made since leaves the result reading stale and the next read
        // rebuilds.
        if self.docs_loading() {
            return;
        }
        // Re-sent (bounded) should the server refuse it while its scan of a
        // just-opened project runs (see `send_retrying`).
        if let Some(id) = self.send_retrying(clew_protocol::Request::BuildDocs) {
            self.proj.docs.rev = self.proj.registry.revision();
            self.proj.link.pending_docs = Some(id);
        }
    }

    /// Install a freshly built API docs index (the `Docs` reply to this
    /// window's `BuildDocs`; the caller has checked it is that reply).
    pub(crate) fn apply_docs(&mut self, files: Vec<clew_protocol::DocFile>) -> Task<Message> {
        self.proj.docs.files = files;
        // A new index: the DOCS tree's memoized grouping is recomputed.
        self.proj.docs.generation += 1;
        self.proj.link.pending_docs = None;
        // The open doc page was flattened from the PREVIOUS index, so a
        // rebuild would leave it presenting pre-edit signatures and doc text,
        // with an "Open source" button carrying the pre-edit line. Re-resolve
        // it against the index that just arrived, matching the item by NAME
        // within the same file — its line is exactly what an edit above it
        // moves, so the line cannot be the key here.
        if let Some((rel, name)) = self
            .proj
            .docs
            .page
            .as_ref()
            .and_then(|p| Some((p.rel.clone(), p.entries.first()?.name.clone())))
        {
            match self
                .proj
                .docs
                .files
                .iter()
                .find(|f| f.rel == rel)
                .and_then(|f| find_doc_by_name(std::slice::from_ref(f), &name))
            {
                Some((_, line)) => self.open_doc_page(&rel, line),
                // The item is gone from the file (deleted, renamed, or no
                // longer parsed). There is nothing to re-resolve to, and
                // leaving the page up would present a symbol this project no
                // longer documents as current.
                None => {
                    self.proj.docs.page = None;
                    self.status = format!("“{name}” is no longer in {rel}");
                }
            }
        }
        // Resolve a "View docs" that was waiting on the index.
        if let Some(name) = self.proj.link.pending_docs_view.take() {
            match find_doc_by_name(&self.proj.docs.files, &name) {
                Some((rel, line)) => self.open_doc_page(&rel, line),
                None => self.status = format!("No docs for “{name}”"),
            }
        }
        // An open type map is drawn from this index: rebuild and redraw it.
        if self.proj.overlay == Some(Overlay::ProjectTypes) {
            self.rebuild_type_graph();
            return self.refresh_graph_layout();
        }
        Task::none()
    }

    /// A `BuildDocs` is in flight: exactly while its reply is awaited (the
    /// request id lives on the project's link, so a dead transport takes the
    /// spinner with it).
    pub(crate) fn docs_loading(&self) -> bool {
        self.proj.link.pending_docs.is_some()
    }

    /// Whether the loaded API-docs index describes the files as they are NOW.
    /// Keyed on the change registry, the freshness key its peers already use
    /// (`stats.rev`, `project_calls.rev`); on a REMOTE project that revision is
    /// advanced by the server's `ProjectSymbols` publications, so this reads a
    /// remote edit exactly as it reads a local one.
    ///
    /// Deliberately conservative in one direction: `request_docs` stamps the
    /// revision it asked at, so a change landing WHILE a build is in flight
    /// leaves the (genuinely current) result reading as stale and costs one
    /// extra rebuild the next time the tab is opened. Under-claiming freshness
    /// only wastes a build; over-claiming is what put a pre-edit API surface on
    /// screen and is the reason this key exists.
    pub(crate) fn docs_fresh(&self) -> bool {
        !self.proj.docs.files.is_empty() && self.proj.docs.rev == self.proj.registry.revision()
    }

    /// Rebuild the API docs if what we hold is missing or stale. Cheap to call
    /// from any place that is about to READ the index. Single-flight, though
    /// the guard that matters lives in `request_docs` — every caller needs it,
    /// not just this one.
    pub(crate) fn ensure_docs(&mut self) {
        if !self.docs_loading() && !self.docs_fresh() {
            self.request_docs();
        }
    }

    /// Build the main-pane doc page for the item at (`rel`, `line`): the item
    /// itself plus its members (public unless "show all"), each with its doc
    /// comment parsed to markdown. Switches the main pane to the page.
    pub(crate) fn open_doc_page(&mut self, rel: &str, line: usize) {
        let Some(file) = self.proj.docs.files.iter().find(|f| f.rel == rel) else {
            return;
        };
        let Some(item) = find_doc_item(&file.items, line) else {
            return;
        };
        let mut entries = Vec::new();
        flatten_doc(item, 0, self.docs_view.show_all, &mut entries);
        self.proj.docs.page = Some(DocPage {
            rel: rel.to_string(),
            entries,
        });
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.glossary.showing = false;
    }

    /// Open the doc page for the symbol named `name` (from "View docs"). Switches
    /// to the DOCS tab. If the index isn't built yet, build it and resolve the
    /// name when it arrives.
    pub(crate) fn view_docs_for(&mut self, name: &str) {
        self.sidebar = SidebarTab::Docs;
        self.show_left_sidebar = true;
        // A STALE index is not an answer about this symbol: resolving against it
        // opens the page at a line the edits have since moved, or reports "no
        // docs" for something added since the build. Rebuild and let the reply
        // resolve the name — the same waiting path a never-built index takes.
        if !self.docs_fresh() {
            self.proj.link.pending_docs_view = Some(name.to_string());
            self.ensure_docs();
            // No build in flight afterwards means none could be sent (no
            // transport), so nothing will ever resolve the pending name —
            // answer now instead of leaving the request outstanding forever.
            if !self.docs_loading() {
                self.proj.link.pending_docs_view = None;
                self.status = format!("No docs for “{name}”");
            }
            return;
        }
        if let Some((rel, line)) = find_doc_by_name(&self.proj.docs.files, name) {
            self.open_doc_page(&rel, line);
        } else {
            self.status = format!("No docs for “{name}”");
        }
    }

    pub(crate) fn on_view_docs_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let Some(word) = self
            .proj
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .and_then(|v| analyze::word_at(&v.lines, menu.line, menu.col))
        else {
            return Task::none();
        };
        // Docs are built AND current but hold no entry for this symbol (e.g. an
        // undocumented private item): rather than silently doing nothing,
        // fall back to its definition so "View docs" always lands the
        // reader somewhere useful. A stale index cannot support that verdict —
        // it reported "No doc entry for X" for every symbol added since it was
        // built — so it is sent to `view_docs_for`, which rebuilds and resolves
        // the name against the answer.
        if self.docs_fresh() && find_doc_by_name(&self.proj.docs.files, &word).is_none() {
            self.status = format!("No doc entry for “{word}” — showing its definition");
            return self.goto_definition(menu.pane, menu.line, menu.col);
        }
        self.view_docs_for(&word);
        Task::none()
    }
}

impl App {
    /// Handle a [`DocsMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_docs(&mut self, message: DocsMsg) -> Task<Message> {
        match message {
            DocsMsg::Refresh => {
                self.request_docs();
                Task::none()
            }
            DocsMsg::ToggleFile(rel) => {
                if !self.proj.docs.expanded.remove(&rel) {
                    self.proj.docs.expanded.insert(rel);
                }
                Task::none()
            }
            DocsMsg::FilterChanged(s) => {
                self.proj.docs.filter = s;
                Task::none()
            }
            DocsMsg::ToggleShowAll => {
                self.docs_view.show_all = !self.docs_view.show_all;
                Task::none()
            }
            DocsMsg::ToggleGrouping => {
                self.docs_view.by_module = !self.docs_view.by_module;
                Task::none()
            }
            DocsMsg::Select { rel, line } => {
                self.open_doc_page(&rel, line);
                Task::none()
            }
            DocsMsg::ViewFromMenu => self.on_view_docs_from_menu(),
        }
    }
}
