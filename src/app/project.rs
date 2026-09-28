//! The project lifecycle: workspace-trust consent, scanning (server-side or
//! the local fallback), installing a scanned project, loading its persisted
//! state off the UI thread, splicing tree updates, leaving a project, and the
//! ownership check that tells a current result from a stale one.
//!
//! Its messages, [`ProjectMsg`], arrive through `App::update_project`.

use crate::app::prelude::*;
use crate::*;

/// The status line for a project just scanned: its size, and what the walk
/// skipped or added when it did (see `fs_scan::ScanReport::summary`).
pub(crate) fn scan_status(files: usize, truncated: bool, report: &fs_scan::ScanReport) -> String {
    let size = format!(
        "{files} files{}",
        if truncated { " (truncated)" } else { "" }
    );
    match report.summary() {
        Some(summary) => format!("{size} — {summary}"),
        None => size,
    }
}

/// The languages among `files` that clew ships a server for, sorted — which
/// rows the server panel shows (`ProjectSession::project_languages`). Taken
/// again whenever the file list is: set once, at open, a language whose
/// first file came later had no row, and one whose last file went kept it.
fn served_languages(files: &[fs_scan::FileEntry]) -> Vec<String> {
    let mut langs: Vec<String> = files
        .iter()
        .filter_map(|f| highlight::detect(&f.abs))
        .filter(|l| lsp::registry::default_for_language(l).is_some())
        .map(|l| l.to_string())
        .collect();
    langs.sort();
    langs.dedup();
    langs
}

/// Walk `root` on the blocking pool, WITH the report of what the walk skipped
/// or added (`fs_scan::scan_with_report`) — the plain `scan` only logs it,
/// where no reader sees it. A walk that panicked is an empty tree whose
/// report says so, never an empty project passing for a real one.
async fn scan_off_thread(root: PathBuf) -> (ScanResult, fs_scan::ScanReport) {
    let fallback = root.clone();
    tokio::task::spawn_blocking(move || fs_scan::scan_with_report(root))
        .await
        .unwrap_or_else(|e| {
            (
                ScanResult {
                    root: fallback,
                    tree: DirNode::default(),
                    files: Vec::new(),
                    truncated: false,
                },
                fs_scan::ScanReport {
                    walk_errors: 1,
                    first_error: Some(format!("the scan stopped unexpectedly: {e}")),
                    ..Default::default()
                },
            )
        })
}

impl App {
    /// Gate every project open behind consent recorded **outside** the project.
    ///
    /// Consent used to be "a `.clew/` directory exists", but that directory is
    /// part of the repository — a hostile repo could ship one and grant itself
    /// permission, along with the `lsp.toml` inside it. The record now lives in
    /// clew's global data directory, keyed by the canonical root.
    pub(crate) fn request_open(&mut self, root: PathBuf) -> Task<Message> {
        // Trust is host-scoped: the same absolute path on an SSH host is a
        // different project than the local one.
        if self
            .trust
            .is_root_trusted(self.connection.approval_host(), &root)
        {
            return self.start_scan(root);
        }
        // Otherwise ask via an in-app modal (see ui::consent_modal).
        self.pending_consent = Some(root);
        Task::none()
    }

    pub(crate) fn start_scan(&mut self, root: PathBuf) -> Task<Message> {
        self.scanning = true;
        self.status = format!("Scanning {}…", root.display());
        // Preferred: let clew-server scan and return the tree (its `Tree` reply
        // builds the project via `handle_server_reply`).
        if self.server.is_up() {
            if self.request_open_project(root.clone()) {
                return Task::none();
            }
            // Channel closed mid-session. For a REMOTE project there is
            // nothing to fall back TO: `root` names a path on the other host,
            // and scanning it here would open whatever this machine happens
            // to have there. Park the request for the reconnect instead.
            if self.connection.is_remote() {
                self.pending_scan_root = Some(root);
                self.status = "Lost the remote host — reopening once reconnected…".into();
                return Task::none();
            }
        } else {
            // Server not up yet: defer. `ServerMsg::Connected` sends the OpenProject
            // once it is; `ServerMsg::Unavailable` falls back to a local scan, and
            // so does `on_handshake_failed` when the server that came up
            // speaks another protocol — every path out of the wait releases
            // this root, or the window sits on "Scanning…" forever. This is
            // what removes the duplicate scan at startup.
            self.pending_scan_root = Some(root);
            return Task::none();
        }
        self.local_scan(root)
    }

    /// Ask the server to open `root` and return its tree. Records `root` as the
    /// pending scan so the `Tree` reply can build the project. Returns false if
    /// the request could not be sent (no server / channel closed).
    pub(crate) fn request_open_project(&mut self, root: PathBuf) -> bool {
        let request = clew_protocol::Request::OpenProject {
            root: root.to_string_lossy().into_owned(),
        };
        match self.send_to_server(request) {
            Some(id) => {
                self.pending_scan_root = Some(root);
                // Correlated: a failure is this open's answer, not a stray
                // status line (`on_open_project_failed`).
                self.server.pending_open = Some(id);
                true
            }
            None => false,
        }
    }

    /// The server could not open the project this window is opening (its
    /// scan or its commit failed — the server answers every `OpenProject`).
    /// The wait ends here either way: `scanning` and `pending_scan_root` are
    /// released, as on every other way out of an open.
    ///
    /// The server is left holding what it failed on: it tore the previous
    /// project down before scanning (its root, file list, watcher, processes
    /// and approvals), so it now holds the new root half-open — no files, no
    /// watcher — and a search sent to it waits out its scan and fails,
    /// minutes later. So:
    ///
    /// - a remote project has nothing to fall back to — the path names a
    ///   directory on the other host — so the window says why and keeps the
    ///   project it shows, which the server is told to open again: its reads,
    ///   blame and state requests name paths inside it;
    /// - a local project is scanned here, like one opened while no server was
    ///   up, and this window stops using that server — latched the way a
    ///   handshake refusal is, since opening the same root again would fail
    ///   the same way; the next project the user opens re-arms it.
    ///
    /// `Cancelled` means a newer open took over; that one owns the wait.
    pub(crate) fn on_open_project_failed(
        &mut self,
        code: clew_protocol::ErrorCode,
        message: String,
    ) -> Task<Message> {
        if code == clew_protocol::ErrorCode::Cancelled || !self.scanning {
            return Task::none();
        }
        let Some(root) = self.pending_scan_root.take() else {
            return Task::none();
        };
        if self.connection.is_remote() {
            self.scanning = false;
            self.status = format!(
                "Could not open {} on the remote host: {message}",
                root.display()
            );
            self.sync_project_to_server();
            return Task::none();
        }
        let why = format!(
            "clew-server could not open {} ({message}) — scanning it here",
            root.display()
        );
        self.server.close();
        self.handshake_failure = Some(why.clone());
        self.status = why;
        self.local_scan(root)
    }

    /// Scan the project on the client (fallback when the server is unavailable).
    pub(crate) fn local_scan(&mut self, root: PathBuf) -> Task<Message> {
        // `ScanDone` is accepted only for the root recorded here, so a slow
        // scan of a project the user has already left can't re-open it — and
        // only on the transport and project instance it was started under, so
        // it cannot install this machine's tree after a switch to a host
        // where the same path names another machine's project.
        self.pending_scan_root = Some(root.clone());
        let stamp = self.transport_stamp();
        Task::perform(scan_off_thread(root), move |(result, report)| {
            Message::Project(ProjectMsg::ScanDone {
                stamp: stamp.clone(),
                result,
                report,
            })
        })
    }

    /// Re-scan the tree off-thread after a structural change, delivering the
    /// result as `TreeUpdated` (a light swap, not a full project reopen).
    pub(crate) fn rescan_tree(&self, root: PathBuf) -> Task<Message> {
        let stamp = self.stamp();
        Task::perform(scan_off_thread(root), move |(result, report)| {
            Message::Project(ProjectMsg::TreeUpdated {
                stamp: stamp.clone(),
                result,
                report,
            })
        })
    }

    pub(crate) fn on_consent_allowed(&mut self) -> Task<Message> {
        let Some(root) = self.pending_consent.take() else {
            return Task::none();
        };
        // Consent is recorded outside the project (see `request_open`): a
        // repository must never be able to grant itself permission. It is
        // bound to the host it was granted for.
        let host = self.connection.approval_host().map(str::to_string);
        if let Err(e) = self.trust.update(|t| t.trust_root(host.as_deref(), &root)) {
            self.pending_open = None;
            self.status = format!("Cannot record consent: {e}");
            return Task::none();
        }
        // `.clew/` still holds this project's own state (bookmarks, caches);
        // create it now so the first save doesn't fail, but a failure here is
        // not fatal — a read-only project still opens, it just can't persist.
        // Never for a remote project: the root is a remote path, and creating
        // it HERE would plant directories on the local machine.
        if self.local_project_state() {
            let _ = std::fs::create_dir_all(root.join(".clew"));
        }
        // The user asking for a project is the retry a latched handshake
        // refusal does not take on its own; `start_scan` below would otherwise
        // park this root waiting for a server that can never arrive.
        self.retry_server_after_handshake_failure();
        self.start_scan(root)
    }

    /// Install a freshly scanned project. `report` says what the walk skipped
    /// or added — a client-side scan's own; the server's scans say theirs in
    /// an `Event::Status` of their own, so theirs arrive empty here.
    pub(crate) fn on_scan_done(
        &mut self,
        result: ScanResult,
        report: fs_scan::ScanReport,
    ) -> Task<Message> {
        // Stop the OLD project's in-flight work before installing the new one,
        // while the server link, `agent_stream` and the debug session still name
        // it.
        let stop_old = self.drop_project_work();
        self.scanning = false;
        // A new project instance: results of tasks spawned under the old one
        // carry the old epoch in their stamps and are dropped in `dispatch`.
        self.project_epoch += 1;
        self.status = scan_status(result.files.len(), result.truncated, &report);
        // Everything anchored to the project being left goes with it — its
        // whole session, the same reset a transport switch performs
        // (`forget_project_state`). That includes any tree resync still awaited
        // for it (this tree IS the project's file list, and answers it better).
        self.forget_project_state();
        // Tracked files the ignore rules hid, listed anyway: marked in the tree.
        self.proj.tracked_ignored = report.tracked_ignored.into_iter().collect();
        // Persisted project state (`.clew/`) lives WITH the project. For a
        // remote project those files are on the remote host — the same paths
        // on this machine belong to a different (or no) project — so nothing
        // is loaded from (or later saved to) the local disk; per-project
        // state starts fresh until it arrives over the protocol.
        let local_state = !self.connection.is_remote();
        // This project's derived-artifact store, in clew's OWN data dir and
        // keyed by host+root. It is not the project's `.clew/`, whose bytes
        // the repository controls — a cache shipped there was accepted as
        // clew's own index, which is a way to forge navigation, the graphs,
        // and the source handed to the model. Because the store is local, a
        // remote project caches exactly like a local one.
        self.proj.derived_dir =
            clew_core::derived::dir(self.connection.approval_host(), &result.root);
        let store = self.proj.derived_dir.clone();
        // Land on the architecture-overview home (filled from the cache when
        // the project's state has loaded).
        self.proj.overview.showing = true;
        self.embed_available = embed::Config::available();
        // A project open re-reads the AI config (another window may have
        // saved Settings).
        self.invalidate_llm_config();
        self.proj.project_languages = served_languages(&result.files);
        let files = Arc::new(result.files);
        self.proj.project = Some(Project {
            root: result.root,
            tree: result.tree,
            files: files.clone(),
            truncated: result.truncated,
        });
        // The server cleared its per-project LSP approvals on OpenProject;
        // push this project's recorded ones so its spawn paths (SpawnLsp, the
        // Ask agent) honor them.
        self.send_lsp_approvals();
        // A remote project's session state (history, bookmarks, notes,
        // reading target) lives in the REMOTE `.clew/`; fetch it over the
        // protocol.
        if !local_state {
            self.request_remote_state();
        }
        // The server already knows this project: either it produced this tree
        // (server-scan path in `start_scan`), or — if this came from the local
        // fallback — the `ServerMsg::Connected` handler syncs it when the server is up.

        // The project's persisted state — reading trail, bookmarks, notes,
        // `lsp.toml`, reading target, tours, and the derived caches (the
        // explanations, overview, stats and semantic index, the last of which
        // runs to tens of megabytes of JSON) — is read on the blocking pool.
        // It used to be read right here, on the thread that serves every
        // window. Until it lands (`on_project_state_loaded`) the stores hold
        // empty placeholders, and nothing writes one of them back over its
        // file (see `InFlight::state_loading`).
        let state_task = self.load_project_state(local_state, store.clone());

        // Build the project-wide symbol index in the background, warm-starting
        // from the persistent cache (only files changed while clew was closed
        // are re-read/re-parsed), and persist the refreshed cache.
        //
        // This runs for as long as the project is big, and the watcher stays
        // live throughout: by the time the result lands, some of the files it
        // hashed may already have been re-read at a newer version. The result
        // carries no clock of its own, so `on_symbol_index_done` merges it
        // against the registry — which is cleared just above, making every
        // entry it holds at that point the watcher's newer work.
        //
        // NEVER for a remote project: the file list's absolute paths name
        // files on the remote host, and reading them here would index
        // whatever this machine has at those paths — empty results at best,
        // another project's source (fed onward to AI features) at worst.
        // The server pushes a `ProjectSymbols` snapshot instead.
        self.proj.indexing = true;
        let index_task = if local_state {
            let index_root = self.proj.project.as_ref().unwrap().root.clone();
            let stamp = self.stamp();
            let index_store = store.clone();
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        index::build_indexed_warm(&index_root, index_store.as_deref(), files)
                    })
                    .await
                    .unwrap_or_default()
                },
                move |indexed| {
                    Message::Watch(WatchMsg::SymbolIndexDone {
                        stamp: stamp.clone(),
                        indexed,
                    })
                },
            )
        } else {
            Task::none()
        };

        let open_task = match self.pending_open.take() {
            Some(file) => self.open_file(file, None, true), // clears show_overview
            None => Task::none(),
        };
        // An open Language Servers panel lists THIS project's languages now
        // (its lsp.toml re-lists it again when it loads).
        let panel_task = if self.server_panel {
            self.refresh_server_panel()
        } else {
            Task::none()
        };
        // No auto-explain on startup: warm-start from the persisted cache and
        // show what's there. Explanations (re)generate only on an explicit
        // request (whole project / one function) or when a file's hash changes.
        Task::batch([stop_old, state_task, index_task, open_task, panel_task])
    }

    /// Forget everything anchored to the open project: its panes, editors and
    /// popups, its reading state, every derived index and cache, and every
    /// request still in flight for it — by dropping its [`ProjectSession`]
    /// whole. The ONE reset both ways of leaving a project share: opening
    /// another (`on_scan_done`) and switching transport (`connect_to`), which
    /// used to keep the old project's time travel, diff, notes, bookmarks,
    /// tours and caches on screen until the next scan happened to replace
    /// them. There is no list of fields here to keep complete: a field added
    /// to the session is forgotten with it.
    ///
    /// Dropping the session also drops what it owns that runs: its language
    /// servers (their children are killed with their clients) and its
    /// abort-on-drop task handles. Work that must be told to stop elsewhere is
    /// `drop_project_work`'s, which runs first.
    ///
    /// Two drafts the user typed exist nowhere else, so a discarded one is
    /// reported (as is unsaved remote state) rather than left to vanish.
    pub(crate) fn forget_project_state(&mut self) {
        let left = std::mem::take(&mut self.proj);
        // The Language Servers panel's per-language rows were resolved against
        // the project being left (its lsp.toml, its host): they go with it,
        // and a listing still in flight for it is dropped by its stamp.
        self.lsp_located.clear();
        // The two note editors carry a project-RELATIVE anchor plus a draft
        // (which is why neither may outlive its project: Save re-resolves the
        // root when it runs); what the user typed into them is lost with the
        // project, so say so.
        let dropped_drafts = [
            left.reading_note_edit.map(|(_, _, draft)| draft),
            left.note_edit.map(|(_, _, draft)| draft),
        ];
        if dropped_drafts
            .iter()
            .flatten()
            .any(|draft| !draft.trim().is_empty())
        {
            self.status = "Unsaved note was discarded with the previous project".into();
        }
        // Anything still marked dirty is a change this client holds and the
        // remote never received. Leaving the project discards it, so say so
        // rather than letting a session's bookmarks or notes disappear without
        // a word.
        if !left.remote_state_dirty.is_empty() {
            let mut lost: Vec<&str> = left.remote_state_dirty.iter().map(String::as_str).collect();
            lost.sort_unstable();
            self.status = format!("Unsaved remote state was discarded: {}", lost.join(", "));
        }
    }

    /// Stop everything running FOR the project being left — the work itself,
    /// not just its results.
    ///
    /// Forgetting the project (`forget_project_state`, which drops its whole
    /// [`ProjectSession`]) makes its results ignorable — their stamps no
    /// longer match — and drops what it owns. What it cannot do is reach work
    /// that runs somewhere else: an Explain pass keeps issuing LLM calls, a
    /// server-side agent keeps calling tools against the OLD root, an LSP
    /// refine keeps querying servers through clones it already holds, and a
    /// debuggee keeps running. All of it is billed, and some of it feeds the
    /// next project's context. So this is not a reset: it is the list of
    /// things to TELL to stop.
    ///
    /// Runs wherever the project's work has to end, before the session is
    /// dropped (the handles and stream ids are in it): at both entries to a
    /// different project — opening one (`on_scan_done`) and switching
    /// transport (`connect_to`) — and when the window closes, which quitting
    /// does to every window (`on_window_closed`).
    /// Distinct from [`Self::drop_connection_state`], which handles the
    /// TRANSPORT dying, and must run before it — the `Cancel` has to reach the
    /// server over the channel the turn started on.
    pub(crate) fn drop_project_work(&mut self) -> Task<Message> {
        // The bottom-up Explain pass is thousands of LLM calls, and an iced
        // `Handle` does NOT abort when dropped: without this the calls keep
        // going (and keep being billed). Aborting stops scheduling; for the
        // client AI endpoint the calls already handed to `spawn_blocking`
        // still finish.
        if let Some(handle) = self.proj.explain.abort.take() {
            handle.abort();
        }
        // The LSP refine holds CLONES of the old project's language-server
        // clients, so dropping them with the session does not stop it.
        self.abort_refine();
        // The agent runs ON THE SERVER against the old root. Its cancel flag
        // lives in the server's map keyed by the stream id, so forgetting the
        // id locally would leave it looping with no way to stop it. (The
        // server also drains its own agent map on `OpenProject`, which covers
        // a lost frame or a client that never sends this.) A plain streamed
        // answer is cancelled there the same way, and one streaming from the
        // provider on THIS machine is aborted — `stop_ask_streams` covers all
        // three, plus a question still being retrieved.
        let stop_ask = self.stop_ask_streams();
        // The debuggee belongs to the old project: left running, its variables
        // would keep feeding the new project's Ask context. Literally the same
        // teardown the Stop button performs, rather than a copy of it that
        // reads the same: the copy was where the breakpoint-verdict reset would
        // have gone missing.
        Task::batch([stop_ask, self.stop_debug_session()])
    }

    /// Read the open project's persisted state on the blocking pool (see
    /// [`LoadedProjectState::read`]); it lands as `ProjectMsg::StateLoaded`.
    fn load_project_state(&mut self, local: bool, store: Option<PathBuf>) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        self.proj.inflight.state_loading = true;
        let stamp = self.stamp();
        let read_root = root;
        Task::perform(
            async move {
                let read = tokio::task::spawn_blocking(move || {
                    LoadedProjectState::read(&read_root, store.as_deref(), local)
                })
                .await;
                read_or_unread(read)
            },
            move |state| {
                Message::Project(ProjectMsg::StateLoaded {
                    stamp: stamp.clone(),
                    state: Handoff::new(state),
                })
            },
        )
    }

    /// Record that the user changed per-project store `rel` while the project
    /// state was still loading: the in-memory copy is then newer than what the
    /// load read, and `on_project_state_loaded` keeps it.
    pub(crate) fn touch_project_state(&mut self, rel: &'static str) {
        if self.proj.inflight.state_loading {
            self.proj.inflight.state_touched.insert(rel);
        }
    }

    /// The project's persisted state arrived (`ProjectMsg::StateLoaded`). Adopt each
    /// store the user has not changed meanwhile, report any that could not be
    /// read (they are left untouched on disk), replay trail visits made while
    /// loading onto the loaded trail, fill the derived caches where nothing
    /// newer exists, and start the language servers that waited for
    /// `lsp.toml`.
    pub(crate) fn on_project_state_loaded(&mut self, state: LoadedProjectState) -> Task<Message> {
        self.proj.inflight.state_loading = false;
        let touched = std::mem::take(&mut self.proj.inflight.state_touched);
        let mut problems: Vec<String> = Vec::new();
        let mut tasks: Vec<Task<Message>> = Vec::new();
        if let Some(why) = &state.unread {
            // Nothing was read: the stored trail is still on disk, and what
            // is in memory is only this session's visits.
            self.proj.inflight.state_unread = true;
            problems.push(format!("the project's saved state ({why})"));
        }

        if let Some(loaded) = state.history {
            match loaded {
                Ok(loaded) => {
                    // Visits made while the trail was loading (a file opened
                    // at startup, a quick click) are replayed onto it, in
                    // order, instead of either copy losing to the other.
                    let during = std::mem::replace(&mut self.proj.history, loaded);
                    let visits = during.flatten();
                    let replayed = !visits.is_empty();
                    for v in visits {
                        self.proj.history.push(v.loc, v.label);
                    }
                    self.proj.trail_collapsed.clear();
                    if replayed {
                        tasks.push(self.save_history());
                    }
                }
                Err(e) => problems.push(format!("history.json {e}")),
            }
        }
        if let Some(loaded) = state.bookmarks
            && !touched.contains(bookmarks::REL)
        {
            match loaded {
                Ok(list) => self.proj.bookmarks = list,
                Err(e) => problems.push(format!("{} {e}", bookmarks::REL)),
            }
        }
        if let Some(loaded) = state.notes
            && !touched.contains(notes::REL)
        {
            match loaded {
                Ok(list) => self.proj.notes = list,
                Err(e) => problems.push(format!("{} {e}", notes::REL)),
            }
        }
        if let Some(loaded) = state.walk_library
            && !touched.contains(walkthrough::LIBRARY_REL)
        {
            match loaded {
                Ok(library) => self.proj.walk.library = library,
                Err(e) => problems.push(e),
            }
        }
        if let Some(loaded) = state.reading_target
            && !touched.contains("reading.toml")
        {
            match loaded {
                Ok(target) => {
                    let target = target.unwrap_or_else(inactive::Target::host);
                    if target != self.proj.reading_target {
                        self.proj.reading_target = target;
                        // Re-evaluate the cfg dimming of anything already open.
                        let t = self.proj.reading_target.clone();
                        for v in self.proj.panes.iter_mut().flatten() {
                            if let Some(lang) = v.lang_key {
                                let src = v.source.clone();
                                v.set_inactive_lines(inactive::inactive_lines(&src, lang, &t));
                            }
                        }
                    }
                }
                Err(e) => problems.push(format!("reading.toml {e}")),
            }
        }
        // A malformed lsp.toml is surfaced, not silently replaced by defaults
        // (which could resolve a different server than configured).
        if let Some(loaded) = state.lsp_config {
            match loaded {
                Ok(config) => {
                    self.proj.lsp_config = config;
                    // The panel's rows resolve through lsp.toml.
                    if self.server_panel {
                        tasks.push(self.refresh_server_panel());
                    }
                }
                Err(e) => problems.push(e),
            }
        }
        // Language servers asked for before `lsp.toml` was known.
        for language in std::mem::take(&mut self.proj.inflight.deferred_lsp) {
            tasks.push(self.ensure_lsp(&language));
        }

        // Derived caches: whatever this session already produced is newer.
        if let Some(problem) = state.explain_problem {
            problems.push(problem);
        }
        if !state.explain.is_empty() {
            if self.proj.explain.cache.is_empty() {
                self.set_explain_cache(state.explain);
            } else {
                let cache = self.explain_cache_mut();
                for (node, cached) in state.explain {
                    cache.entry(node).or_insert(cached);
                }
            }
        }
        if let Some(cached) = state.overview
            && self.proj.overview.markdown.is_none()
            && !self.proj.overview.generating
        {
            self.proj.overview.prompt_hash = Some(cached.prompt_hash);
            let display = self.overview_display(&cached.markdown);
            self.proj.overview.markdown = Some(cached.markdown);
            // The module map lays out from the import graph; if imports aren't
            // resolved yet, it fills in when indexing completes
            // (`refresh_overview_map`).
            let (prepared, task) = self.prepare_segments(&display);
            self.proj.overview.prepared = prepared;
            self.set_overview_map();
            tasks.push(task);
        }
        if let Some(report) = state.stats
            && self.proj.stats.report.is_none()
        {
            self.proj.stats.report = Some(report);
        }
        if !state.embed.entries.is_empty()
            && self.proj.embed_index.entries.is_empty()
            && !self.proj.building_embeddings
        {
            self.proj.embed_index = state.embed;
        }
        if let Some(problem) = state.embed_problem {
            problems.push(problem);
        }

        if !problems.is_empty() {
            self.status = format!(
                "Left untouched, could not read: {} — fix or remove the file to use it",
                problems.join("; ")
            );
        }
        Task::batch(tasks)
    }

    pub(crate) fn on_tree_updated(
        &mut self,
        result: ScanResult,
        report: fs_scan::ScanReport,
    ) -> Task<Message> {
        // (Applied only to the project instance this rescan was started for —
        // checked in `dispatch` from its stamp. The root alone could not say
        // that: a local project and a remote one can share an absolute path
        // while being different machines' code, and the file list drives the
        // index, the graphs and the AI context.)
        if let Some(p) = &mut self.proj.project {
            p.tree = result.tree;
            p.files = Arc::new(result.files);
            p.truncated = result.truncated;
            self.proj.project_languages = served_languages(&p.files);
        }
        self.proj.tracked_ignored = report.tracked_ignored.iter().cloned().collect();
        self.refresh_finder();
        // The file set changed, so imports that were unresolved (or
        // resolved to a since-moved file) may now resolve differently.
        let cycles = self.reresolve_import_graph();
        if let Some(p) = &self.proj.project {
            self.status = match report.summary() {
                Some(summary) => format!(
                    "{} files · {} symbols — {summary}",
                    p.files.len(),
                    self.symbol_count()
                ),
                None => format!("{} files · {} symbols", p.files.len(), self.symbol_count()),
            };
        }
        // This is the first point at which a created or deleted Rust file
        // is part of the file set, so it is where the structure index can
        // learn its `impl` blocks — the watcher batch that saw the creation
        // ran against the tree as it was before it (see `on_files_rehashed`).
        Task::batch([cycles, self.request_structure_build()])
    }

    /// Swap a server-sent file list into the project that is already open,
    /// keeping panes, scroll and every derived artifact. Shared by the
    /// watcher's structural notification and the resync `OpenProject` reply, so
    /// the same snapshot lands the same way whichever way it arrives. The
    /// snapshot names the project the server holds — one from a project we've
    /// already left must not splice its file list under the current root —
    /// and its scan order: a tree whose scan started before the one on screen
    /// saw less than it, and is dropped.
    ///
    /// Everything that holds POSITIONS into the old file list is refreshed
    /// with it, as `on_tree_updated` does for a local rescan: the finder's
    /// results are indices into `project.files`, so a finder left open would
    /// otherwise open whatever file now sits at a stale index. Imports that
    /// were unresolved (or resolved to a since-moved file) are re-resolved.
    pub(crate) fn splice_tree(
        &mut self,
        tree_root: &str,
        seq: u64,
        tree: clew_protocol::DirNode,
        files: Vec<clew_protocol::Rel>,
        tracked_ignored: Vec<clew_protocol::Rel>,
    ) -> Task<Message> {
        if !self.owns_server_event(tree_root) || seq <= self.proj.link.server_tree_seq {
            return Task::none();
        }
        let Some(project) = &mut self.proj.project else {
            return Task::none();
        };
        self.proj.link.server_tree_seq = seq;
        let root = project.root.clone();
        // The path-mapping configs the file list names, before and after,
        // with the bases the fetched ones extend: a tsconfig — or a base —
        // created or deleted on the host changes what is fetched.
        let fetched = self.proj.remote_ts_configs.clone();
        let configs = |files: &[fs_scan::FileEntry]| -> Vec<String> {
            let mut rels: Vec<String> = files
                .iter()
                .filter(|f| {
                    imports::is_resolution_metadata(Path::new(&f.rel))
                        || fetched.as_ref().is_some_and(|c| c.reads(&f.abs))
                })
                .map(|f| f.rel.clone())
                .collect();
            rels.sort_unstable();
            rels
        };
        let configs_before = configs(&project.files);
        project.tree = tree;
        project.files = Arc::new(
            files
                .into_iter()
                .map(|rel| fs_scan::FileEntry {
                    abs: root.join(&rel),
                    rel,
                })
                .collect(),
        );
        let configs_changed = configs(&project.files) != configs_before;
        self.proj.project_languages = served_languages(&project.files);
        // Marked in the tree, as a local rescan marks them.
        self.proj.tracked_ignored = tracked_ignored.into_iter().collect();
        self.refresh_finder();
        if configs_changed {
            // The configs are settled before the re-resolve starts, so one
            // job runs, against the right ones.
            return self.queue_imports_after_ts_configs(imports::ImportBatch::reresolve);
        }
        self.reresolve_import_graph()
    }

    pub(crate) fn on_toggle_dir(&mut self, rel: String) -> Task<Message> {
        // Cmd+click a folder shows its architectural explanation instead
        // of expanding it.
        if self.modifiers.command()
            && let Some(project) = &self.proj.project
        {
            let node = explain::Node::Folder(project.root.join(&rel));
            self.show_right_panel = true;
            return self.show_explanation(node);
        }
        if !self.proj.expanded.remove(&rel) {
            self.proj.expanded.insert(rel);
        }
        Task::none()
    }

    /// Whether this project's `.clew/` state may touch the LOCAL filesystem.
    /// A remote project's root is a remote path: reading or creating a
    /// same-pathed `.clew/` on this machine would mix two hosts' data. Its
    /// state stays in memory until persistence migrates over the protocol.
    pub(crate) fn local_project_state(&self) -> bool {
        !self.connection.is_remote()
    }

    /// The stamp for project-scoped work spawned now (see [`Stamp`]): its
    /// result is accepted while this project instance stays open, across
    /// reconnects of the transport.
    pub(crate) fn stamp(&self) -> Stamp {
        Stamp {
            root: self.proj.project.as_ref().map(|p| p.root.clone()),
            epoch: self.project_epoch,
            conn: None,
        }
    }

    /// The stamp for work that runs over the current transport and dies with
    /// it (a proxied process, a language server, a parked scan): its result
    /// is accepted only while this project instance is open over this very
    /// transport instance.
    pub(crate) fn transport_stamp(&self) -> Stamp {
        Stamp {
            conn: Some(self.conn_gen),
            ..self.stamp()
        }
    }

    /// Whether a result stamped `stamp` still belongs here. `epoch` is the
    /// authority — it tells apart two projects that share an absolute path on
    /// different hosts, and a project from its own reopening, which a `root`
    /// comparison cannot — and `root` is kept as a consistency check on the
    /// same instance.
    pub(crate) fn owns(&self, stamp: &Stamp) -> bool {
        stamp.epoch == self.project_epoch
            && stamp.root.as_deref() == self.proj.project.as_ref().map(|p| p.root.as_path())
            && stamp.conn.is_none_or(|conn| conn == self.conn_gen)
    }

    /// The one ownership check `dispatch` runs on every async message (see
    /// [`Message::origin`]).
    pub(crate) fn owns_origin(&self, origin: Origin<'_>) -> bool {
        match origin {
            Origin::Stamped(stamp) => self.owns(stamp),
            Origin::Transport(conn) => conn == self.conn_gen,
        }
    }

    /// Tell the clew-server which project to scan, so its file-backed flows
    /// (search today) have a file list. Sent on project open and on (re)connect,
    /// whichever happens second; a no-op until both a project and the server are
    /// present.
    pub(crate) fn sync_project_to_server(&mut self) {
        if let (true, Some(project)) = (self.server.is_up(), &self.proj.project) {
            let request = clew_protocol::Request::OpenProject {
                root: project.root.to_string_lossy().into_owned(),
            };
            let _ = self.send_to_server(request);
            // The server now serves THIS project instance: its notifications
            // (watcher changes, symbol publications, state reads) apply to it.
            self.server_epoch = self.project_epoch;
            // This OpenProject answers with a `Tree` for a project that is
            // already on screen, so mark it as a resync: the reply arm splices
            // the file list in place instead of discarding it. On a reconnect
            // that reply is the ONLY report of everything created while the
            // link was down; the watcher restarts from the current state and
            // reports only later changes.
            self.proj.link.pending_tree_resync = true;
            // OpenProject clears the server's approvals; re-push ours (the
            // serial request loop guarantees ordering).
            self.send_lsp_approvals();
        }
    }
}

/// What a project-state read on the blocking pool came back as. A read that
/// panicked is a read that read nothing ([`LoadedProjectState::unread`]) —
/// never a load of empty stores, which lifted the guard on the wholesale
/// trail write and let the visits made since the open replace the stored
/// trail.
pub(crate) fn read_or_unread(
    read: Result<LoadedProjectState, tokio::task::JoinError>,
) -> LoadedProjectState {
    read.unwrap_or_else(|e| {
        LoadedProjectState::unread(format!("the read failed unexpectedly: {e}"))
    })
}

impl App {
    /// Handle a [`ProjectMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_project(&mut self, message: ProjectMsg) -> Task<Message> {
        match message {
            // The picker walks THIS machine, so on a remote session it would
            // hand a local path to the remote server as the project root.
            // Route to the remote browser instead — the same thing the
            // Connect entry point does, including its transport check.
            ProjectMsg::OpenFolderPressed if self.connection.is_remote() => {
                self.connect = Some(ConnectUi::default());
                if self.server.is_up() {
                    self.enter_remote_browser(None);
                }
                Task::none()
            }
            ProjectMsg::OpenFolderPressed => {
                pick_folder(self.main_window).map(|v| Message::Project(ProjectMsg::FolderPicked(v)))
            }
            ProjectMsg::FolderPicked(None) => Task::none(),
            // A picker opened before a connect can still answer after it: the
            // path is this machine's, so it is not a remote project's root.
            ProjectMsg::FolderPicked(Some(_)) if self.connection.is_remote() => Task::none(),
            ProjectMsg::FolderPicked(Some(root)) => {
                // The user asking for a project is the retry a latched
                // handshake refusal does not take on its own; without it
                // `start_scan` would park this root waiting for a server that
                // can never arrive.
                self.retry_server_after_handshake_failure();
                self.request_open(root)
            }
            ProjectMsg::ConsentDenied => {
                self.pending_consent = None;
                self.pending_open = None;
                self.status = "Project not opened: creating .clew was not allowed".to_string();
                Task::none()
            }
            ProjectMsg::ConsentAllowed => self.on_consent_allowed(),
            ProjectMsg::ScanDone { result, report, .. } => {
                // Accept only the scan we are waiting for: a slow scan of a
                // project the user has already left must not re-open it. The
                // root is not enough to say so — after a switch to a remote
                // host the same path names the OTHER machine's project, so a
                // local scan started before the switch must not install this
                // machine's tree as that project's (its transport-bound stamp
                // was checked in `dispatch`); within one instance, only the root still
                // being waited for opens.
                if !self.scanning || self.pending_scan_root.as_ref() != Some(&result.root) {
                    return Task::none();
                }
                self.pending_scan_root = None;
                self.on_scan_done(result, report)
            }
            ProjectMsg::StateLoaded { state, .. } => match state.take() {
                Some(state) => self.on_project_state_loaded(state),
                None => Task::none(),
            },
            ProjectMsg::TreeUpdated { result, report, .. } => self.on_tree_updated(result, report),
            ProjectMsg::ToggleDir(rel) => self.on_toggle_dir(rel),
        }
    }
}
