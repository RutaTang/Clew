//! The code panes: opening and loading files (local reads, server content,
//! notebooks), reloading them in place, highlighting, selection, split,
//! scrolling, cursor motion, folding, in-file find, the git gutter and the
//! diff view.
//!
//! Its messages, [`EditorMsg`], arrive through `App::update_editor`.

use crate::app::prelude::*;
use crate::*;

/// The status line for a file just opened: its path — and, when the
/// language's highlight query does not compile (a grammar bump broke it),
/// why the text shows uncolored, instead of leaving that to look like a file
/// without syntax.
pub(crate) fn open_status(rel: &str, lang_key: Option<&str>) -> String {
    // The sheet's header names the file: the status line says something
    // only when there is something to say about it.
    match lang_key.and_then(highlight::highlight_error) {
        Some(e) => tr!(
            "{file} — shown without highlighting: {error}",
            file = rel,
            error = e
        ),
        None => String::new(),
    }
}

impl App {
    /// Open a file into the active pane, optionally jumping to a 1-based line,
    /// and (with `push`) record the jump on the reading trail.
    pub(crate) fn open_file(
        &mut self,
        abs: PathBuf,
        line: Option<usize>,
        push: bool,
    ) -> Task<Message> {
        // A jump supersedes a walkthrough step still waiting for its file's
        // symbols (`PendingAnchor`): settled afterwards, the step moved the
        // cursor off where the reader went — even within the step's own
        // file, a jump that starts no load to mark it superseded. The step
        // still records its note, if its pane still shows the file when the
        // symbols land. The walkthrough's own jump sets its anchor after this
        // call.
        if let Some(anchor) = self.proj.walk.pending_anchor.as_mut() {
            anchor.superseded = true;
        }
        // Where this jump lands on its line: on a name only when the jump
        // to it asked so (`landing_next`), else at the line's start.
        self.proj.landing = self.proj.landing_next.take();
        // The Files view's keyboard rests on the file being read.
        self.proj.tree_cursor = None;
        // A package (a Go or Java import names its directory, a C# `using`
        // its namespace: the Imports tab's row, the map's) opens at its first
        // file: neither is a file. Looked up whatever the name: a namespace's
        // node (`namespace Acme.Core`) reads as though it had an extension.
        let abs = self.proj.import_graph.package_file(&abs).unwrap_or(abs);
        // Opening a file leaves the overview / stats / docs page for the code, and
        // ends any time-travel session (which would otherwise stay active-but-hidden
        // and keep capturing Esc/←/→ for a file that's no longer shown).
        self.proj.overview.showing = false;
        self.proj.stats.showing = false;
        self.proj.weak.showing = false;
        // The large Ask window steps aside to a card for the code it opens
        // (a citation clicked, a file the agent opened): the code is what
        // the reader goes there to see.
        if self.ask_view == AskView::Expanded {
            self.ask_view = AskView::Compact;
        }
        self.proj.glossary.showing = false;
        self.proj.docs.page = None;
        self.proj.time_travel = None;
        // (A revision still loading goes with the session: its load answers
        // to the session's own request.) A history still loading is given up
        // only if landing, it would undo this jump: in its own pane, to its
        // own file, or as a re-scope of the session just ended
        // (`jump_gives_up_time_start`). A jump to another file in the other
        // pane leaves it be: jumps used to give up every start, whatever its
        // pane, carrying out the open it held back over the history the
        // reader had asked for there.
        //
        // Given up, the start settles what its history was to: its "Loading
        // history…", and the open it superseded (`SupersededOpen`). In this
        // pane the jump is newer, and wins; in the other, nothing newer was
        // asked for, and the history will never land to carry the open out
        // after all — so that is done now, as a start in this pane does
        // (`on_time_travel_start`).
        let pane = self.proj.active;
        let carried = if self.jump_gives_up_time_start(pane, &abs) {
            match self.give_up_time_travel_start() {
                Some(s) if s.pane != pane => self.carry_out_superseded(s),
                _ => Task::none(),
            }
        } else {
            Task::none()
        };
        let saved = if push {
            // Remember the symbol at the target so the trail can re-anchor to it
            // after edits shift its line (see `reanchor` in FilesRehashed).
            let label = line.and_then(|l| self.symbol_name_at(&abs, l));
            self.proj.history.push(
                Loc {
                    path: abs.clone(),
                    line,
                },
                label,
            );
            self.save_history()
        } else {
            Task::none()
        };
        let opened = self.open_file_at(abs.clone(), line);
        // A jump into the step's own file, in its pane, supersedes the load
        // the step started with one of its own: the step waits for that one
        // instead, to record its note. Still bound to its superseded load, it
        // read as the reader having left for another file — the pane showed
        // one until the jump's load landed — and was dropped with its note.
        self.rebind_step_load(pane, &abs);
        Task::batch([saved, opened, carried])
    }

    /// Bind a walkthrough step waiting in `abs`, in `pane`, to the load of
    /// that file the pane now waits for (none: already on screen) — the one
    /// that shows the step's file there, whoever started it.
    fn rebind_step_load(&mut self, pane: usize, abs: &Path) {
        if let Some(anchor) = self.proj.walk.pending_anchor.as_mut()
            && anchor.pane == pane
            && anchor.abs == abs
        {
            anchor.load = self.proj.link.pane_pending[pane];
        }
    }

    /// The load `req` could not read its file, and `said` is what the status
    /// line says of that. When it is the open a time-travel start took over
    /// (`SupersededOpen`), that is kept with the open, unsaid: the history
    /// the reader asked for since is loading, and its "Loading history…"
    /// stays. Said once the open is carried out (`carry_out_superseded`) —
    /// where it asked for the file again, only to be refused again — and
    /// never, if a session starts.
    pub(crate) fn fail_superseded_open(&mut self, req: u64, said: String) {
        if let Some(held) = self
            .proj
            .link
            .superseded_open
            .as_mut()
            .filter(|superseded| superseded.open.req == req)
        {
            held.failed = Some(said);
        }
    }

    /// Cancel the load `pane` waits for, if any: its answer is dropped as it
    /// lands. What it said — "Loading B…" — goes with it, unless the status
    /// line says something else by now, or B is still on its way to the
    /// other pane: left up, it said B was coming, and stayed, as the dropped
    /// answer never replaced it.
    fn cancel_pane_load(&mut self, pane: usize) {
        self.proj.link.pane_pending[pane] = None;
        let Some(open) = self.proj.link.pane_opening[pane].take() else {
            return;
        };
        let link = &self.proj.link;
        let still_coming =
            link.pane_opening
                .iter()
                .zip(&link.pane_pending)
                .any(|(other, pending)| {
                    other
                        .as_ref()
                        .is_some_and(|other| other.abs == open.abs && *pending == Some(other.req))
                });
        if !still_coming && self.status == open.said {
            self.status.clear();
        }
    }

    /// Carry out an open that going into the history of the file on screen
    /// superseded, that history having brought no session (see
    /// `SupersededOpen`) — unless its pane has closed, or waits for another
    /// load, since. One whose load failed while it was held says so, as the
    /// load itself would have, and after what the caller says of the history
    /// — as a load's answer comes after (`EditorMsg::HeldOpenFailed`) — in
    /// place of a second read, refused again.
    pub(crate) fn carry_out_superseded(&mut self, superseded: SupersededOpen) -> Task<Message> {
        let SupersededOpen {
            pane, open, failed, ..
        } = superseded;
        if !(pane == 0 || self.proj.split) || self.proj.link.pane_pending[pane].is_some() {
            return Task::none();
        }
        if let Some(said) = failed {
            let stamp = self.stamp();
            return Task::done(Message::Editor(EditorMsg::HeldOpenFailed { stamp, said }));
        }
        let loading = self.load_into(pane, open.abs.clone(), open.target);
        self.rebind_step_load(pane, &open.abs);
        loading
    }

    /// The reader moved the caret, the selection or the view in `pane`
    /// themselves. A walkthrough step still waiting to settle in the file
    /// that pane shows (`PendingAnchor`) must not move them again once the
    /// file's symbols land: the reader's own move wins, as a jump does
    /// (`open_file`).
    ///
    /// Called from every way the reader moves them — a click or a drag, a
    /// cursor motion, a find jump, Esc, a fold that pulls the caret onto its
    /// header (`fold_in`), a scroll of the pane by wheel, scrollbar or
    /// minimap — and, for the file's history, from going into it and moving
    /// there (time travel). Never from the app's own moves: the jump itself,
    /// a load, a restore — which is why a scroll counts by what the reader
    /// did (`ReaderScrolled`, `MinimapScrolled`), not by the viewport
    /// changing (`Scrolled`).
    pub(crate) fn reader_moved_caret(&mut self, pane: usize) {
        if let Some(v) = self.proj.panes.get(pane).and_then(Option::as_ref) {
            self.proj.walk.reader_moved_in(&v.abs);
        }
    }

    /// [`Self::open_file`] once the trail is recorded: show `abs` at `line`
    /// in the active pane — moving the cursor when it is already there, else
    /// loading it.
    fn open_file_at(&mut self, abs: PathBuf, line: Option<usize>) -> Task<Message> {
        // A jump lands the reader in the code view.
        self.code_focused = true;
        let landing = self.proj.landing.clone();
        let pane = self.proj.active;
        let line_height = self.line_height();
        // Same file already in the active pane: move the cursor and scroll.
        if let Some(v) = self.active_viewer_mut()
            && v.abs == abs
        {
            // Cancel any load still in flight for this pane (A → B → A: B's
            // reply would otherwise land and replace the A the user is
            // looking at). The token is what makes it a no-op on arrival.
            self.cancel_pane_load(pane);
            let Some(v) = self.active_viewer_mut() else {
                return Task::none();
            };
            v.target_line = line;
            if let Some(l) = line {
                let l0 = l.saturating_sub(1);
                v.reveal(l0); // expand any fold hiding the jump target
                let col = landing_col(&landing, v, l0);
                v.caret = Some((l0, col));
            }
            // Notebook cells have variable height, so their goto scrolls to an
            // estimated cell offset (the target ring points precisely).
            let y = match (&v.notebook, line) {
                (Some(doc), Some(l)) => {
                    Self::estimate_notebook_offset(doc, &v.nb_expanded, l, line_height)
                }
                _ => v.scroll_offset_for(line, line_height),
            };
            v.scroll_y = y;
            let scroll =
                operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
            return self.follow_caret(scroll);
        }
        self.load_into(pane, abs, line)
    }

    /// Load `abs` into `pane`, to show it at `line`: the pane's one pending
    /// open from now on, superseding any earlier one.
    fn load_into(&mut self, pane: usize, abs: PathBuf, line: Option<usize>) -> Task<Message> {
        let rel = self.rel_of(&abs);
        // A go-to-def target outside the project (a dependency or stdlib source
        // the LSP resolved) would be refused by the server, whose ReadFile
        // enforces the project boundary. For a LOCAL server, read it directly,
        // read-only — the client already resolved it via the LSP, so reading a
        // dep's source is safe and is core to a code reader. Keep routing
        // through the server for a REMOTE connection, where the boundary is a
        // real security guard and the file lives on the remote host anyway.
        let external_local = self
            .proj
            .project
            .as_ref()
            .is_some_and(|p| !abs.starts_with(&p.root))
            && !self.connection.is_remote();
        // Preferred: fetch the file from clew-server — it reads, highlights, and
        // extracts symbols/docs/inactive server-side. The reply arrives as
        // Event::FileContent and lands via `apply_file_content`.
        if !external_local && self.server.is_up() {
            let request = clew_protocol::Request::ReadFile {
                rel: rel.clone(),
                target: self.target_spec(),
            };
            if let Some(id) = self.send_to_server(request) {
                self.proj
                    .link
                    .pending_reads
                    .insert(id, ReadKind::Open { pane, target: line });
                // This is now the one load the pane is waiting for; any
                // earlier in-flight load for it is superseded.
                self.proj.link.pane_pending[pane] = Some(id);
                self.status = tr!("Loading {file}…", file = rel);
                self.proj.link.pane_opening[pane] = Some(PaneOpen {
                    req: id,
                    abs,
                    target: line,
                    said: self.status.clone(),
                });
                return Task::none();
            }
        }
        // Remote project with the transport down: fail closed. The path names
        // a file on the remote host; a local file at the same absolute path
        // is a different machine's data, so reading it here would silently
        // show (and index) the wrong project.
        if self.connection.is_remote() {
            self.proj.link.pane_pending[pane] = None;
            self.status = tr!(
                "Disconnected from the remote host — cannot open {file}",
                file = rel
            );
            return Task::none();
        }
        // Fallback: server not up — read + highlight locally. The token comes
        // from the same id space as server reads, so the pane guard is uniform.
        let req = self
            .next_req_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.proj.link.pane_pending[pane] = Some(req);
        self.status = tr!("Loading {file}…", file = rel);
        self.proj.link.pane_opening[pane] = Some(PaneOpen {
            req,
            abs: abs.clone(),
            target: line,
            said: self.status.clone(),
        });
        let stamp = self.stamp();
        // A file of the project is read only if it really is one (see
        // `load_file`); one outside it by its path is a dependency's source.
        let confine = self
            .proj
            .project
            .as_ref()
            .filter(|_| !external_local)
            .map(|p| p.root.clone());
        Task::perform(
            load_file(pane, abs, line, confine),
            move |(pane, abs, target, result)| {
                Message::Editor(EditorMsg::FileLoaded {
                    stamp: stamp.clone(),
                    req,
                    pane,
                    abs,
                    target,
                    result,
                })
            },
        )
    }

    pub(crate) fn on_file_loaded(
        &mut self,
        req: u64,
        pane: usize,
        abs: PathBuf,
        target: Option<usize>,
        result: Result<String, String>,
    ) -> Task<Message> {
        // Only the load the pane is still waiting for may land: a slower
        // earlier open must not overwrite a faster later one, and a load
        // issued before a project switch must not resurrect into it.
        if self.proj.link.pane_pending.get(pane).copied().flatten() != Some(req) {
            if let Err(e) = &result {
                let said = format!("{}: {e}", self.rel_of(&abs));
                self.fail_superseded_open(req, said);
            }
            return Task::none();
        }
        self.proj.link.pane_pending[pane] = None;
        let rel = self.rel_of(&abs);
        let content = match result {
            Err(e) => {
                self.status = format!("{rel}: {e}");
                return Task::none();
            }
            Ok(content) => content,
        };

        let lang_key = highlight::detect(&abs);
        let source = Arc::new(content);
        let lines = highlight::plain_lines(&source);
        let line_height = self.line_height();
        let old_viewport = self.proj.panes[pane]
            .as_ref()
            .map(|v| (v.viewport_h, v.viewport_measured));
        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        if let Some((h, measured)) = old_viewport {
            v.viewport_h = h;
            v.viewport_measured = measured;
        }
        v.target_line = target;
        // Put the block cursor on the jump target (or the top of the file).
        let l0 = target.map(|t| t.saturating_sub(1)).unwrap_or(0);
        v.caret = Some((l0, landing_col(&self.proj.landing, &v, l0)));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        // Just the path here; the right status segment already reports line
        // count — unless the language's highlighting is broken, which reads
        // as uncolored text and is said instead.
        self.status = open_status(&v.rel, lang_key);
        self.proj.panes[pane] = Some(v);
        // The peek open over the file it replaces is of that file's symbol
        // (and an answer still on its way for it is too), as on every other
        // way a pane is replaced.
        self.invalidate_hover();
        // Seed the content hash so the watcher can tell real edits from noise
        // — for a file of the project: one outside it (a dependency, the
        // standard library, gone to by a definition) is in no index, and
        // seeding it moved the registry on, every view built from it read
        // stale and was built again for nothing.
        let in_project = self
            .proj
            .project
            .as_ref()
            .is_some_and(|p| abs.starts_with(&p.root));
        if in_project {
            self.proj
                .registry
                .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        }
        // Point the Imports tab at the newly focused file.
        if pane == self.proj.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        // Start (or reuse) a language server for this file and open the doc.
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        // What the tour's step waits to show of this file, if it does.
        let tour = self.tour_file_loaded(&abs);
        // What the History view waits for this file to open for.
        let after = self.after_history_open(&abs);
        let content = self.content_tasks(abs, source, lang_key);
        // Symbols arrive later via `Highlighted`; follow_caret there resolves the
        // enclosing function. Here it shows the file until then.
        self.follow_caret(Task::batch([scroll, lsp_task, content, tour, after]))
    }

    /// Off-thread re-highlight + git-info tasks for a file's current source,
    /// shared by initial load and live refresh. Both deliver `Highlighted` /
    /// `GitInfoLoaded` keyed by `abs`, so they route to whatever pane shows it.
    pub(crate) fn content_tasks(
        &mut self,
        abs: PathBuf,
        source: Arc<String>,
        lang_key: Option<&'static str>,
    ) -> Task<Message> {
        let hl_abs = abs.clone();
        let inactive_abs = abs.clone();
        let hl_source = source.clone();
        let target = self.proj.reading_target.clone();
        let hl_target = target.clone();
        let facts = self.proj.build_facts.clone();
        let hl_facts = facts.clone();
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        // Stamped from the bytes being highlighted, so a result that arrives
        // after a newer pass can be recognized as stale and dropped.
        let hl_hash = incremental::content_hash(source.as_bytes());
        let stamp = self.stamp();
        let hl_stamp = stamp.clone();
        let highlight_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let lines = highlight::highlight_lines(&hl_source, lang_key);
                    // The outline, and the author's doc comments for it, from
                    // one parse.
                    let (symbols, docs) = lang_key
                        .map(|key| docs::outline_and_peek(&hl_source, key))
                        .unwrap_or_default();
                    // The lines the build leaves out for the reading target
                    // (dimmed).
                    let inactive = lang_key
                        .map(|key| {
                            crate::app::languages::inactive_of(
                                &hl_source,
                                key,
                                &inactive_abs,
                                root.as_deref(),
                                &target,
                                &facts,
                            )
                        })
                        .unwrap_or_default();
                    (lines, symbols, docs, inactive)
                })
                .await
                .unwrap_or_default()
            },
            move |(lines, symbols, docs, inactive)| {
                Message::Editor(EditorMsg::Highlighted {
                    stamp: hl_stamp.clone(),
                    abs: hl_abs.clone(),
                    src_hash: hl_hash,
                    lines,
                    symbols,
                    docs,
                    inactive,
                    target: hl_target.clone(),
                    facts: hl_facts.clone(),
                })
            },
        );

        // The project's git, for a file of it: asked about a file outside it
        // (a dependency gone to by a definition), git refuses — "outside
        // repository" — and that refusal took the status line at every such
        // jump.
        let git_task = self.local_git_info(abs, stamp);
        Task::batch([highlight_task, git_task])
    }

    /// The blame and change status of `abs`, a file of the project, read
    /// here (the local path's `GitInfo`); none for a file outside it.
    fn local_git_info(&mut self, abs: PathBuf, stamp: Stamp) -> Task<Message> {
        match self
            .proj
            .project
            .as_ref()
            .map(|p| p.root.clone())
            .filter(|root| abs.starts_with(root))
        {
            Some(root) => {
                // This pass is now the newest blame request for the file (see
                // `request_git_info`): only its answer may paint the gutter.
                let req = self.mint_request_id();
                self.proj.git_latest.insert(abs.clone(), req);
                let file = abs.clone();
                Task::perform(
                    async move {
                        // `try_info`, not `info`: a git that could not answer
                        // is reported (`on_git_info_loaded`), not painted as an
                        // empty gutter that reads as "untracked".
                        tokio::task::spawn_blocking(move || {
                            git::try_info(&root, &file)
                                .map(|info| info.map(Arc::new))
                                .map_err(|e| e.to_string())
                        })
                        .await
                        .unwrap_or_else(|e| Err(tr!("the git task failed: {error}", error = e)))
                    },
                    move |info| {
                        Message::Editor(EditorMsg::GitInfoLoaded {
                            stamp: stamp.clone(),
                            abs: abs.clone(),
                            req,
                            info,
                        })
                    },
                )
            }
            None => Task::none(),
        }
    }

    /// The blame and change status of every file open, read again: the
    /// project's history moved (`App::repo_moved`), and what the gutter
    /// showed was of the commit before. Through the server when it is up,
    /// as a file's content came; here otherwise.
    pub(crate) fn refresh_git_of_open_files(&mut self) -> Task<Message> {
        let mut open: Vec<(PathBuf, String)> = self
            .proj
            .panes
            .iter()
            .flatten()
            .map(|v| (v.abs.clone(), v.rel.clone()))
            .collect();
        open.sort();
        open.dedup();
        let mut tasks = Vec::new();
        for (abs, rel) in open {
            if self.server.is_up() {
                self.request_git_info(rel, abs);
            } else {
                let stamp = self.stamp();
                tasks.push(self.local_git_info(abs, stamp));
            }
        }
        Task::batch(tasks)
    }

    pub(crate) fn on_open_rel(&mut self, rel: String, line: Option<usize>) -> Task<Message> {
        let Some(project) = &self.proj.project else {
            return Task::none();
        };
        // Defence in depth, not a live escape from repository content: every
        // caller today passes a rel that is already safe — the sidebar builds
        // its rels by walking the scanned tree, and bookmarks, notes and trail
        // history each `safe_rel`-filter their entries at load. What this
        // guards is the one source that is neither scan-built nor load-filtered
        // on the way in: on a REMOTE project the tree and the Docs index arrive
        // over the wire, so an absolute rel from a hostile or wrong server
        // would reach `join`, which DISCARDS the root for an absolute argument.
        // Purely lexical, so it is identical for local and remote and never
        // probes this machine's disk at a remote path.
        if !clew_core::statefile::safe_rel(&rel) {
            self.status = tr!("Refused a path outside the project: {file}", file = rel);
            return Task::none();
        }
        let abs = project.root.join(&rel);
        // Cmd+click a file shows its explanation instead of opening it.
        if self.modifiers.command() {
            self.show_right_panel = true;
            return self.show_explanation(explain::Node::File(abs));
        }
        self.open_file(abs, line, true)
    }

    #[allow(clippy::too_many_arguments)] // one highlight pass's whole result
    pub(crate) fn on_highlighted(
        &mut self,
        abs: PathBuf,
        src_hash: incremental::Version,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: HashMap<usize, String>,
        inactive: HashSet<usize>,
        target: inactive::Target,
        facts: Arc<clew_protocol::BuildFacts>,
    ) -> Task<Message> {
        let lines = Arc::new(lines);
        // Lines, symbols and docs follow from the bytes alone, so the hash
        // below vouches for them. `inactive` does not: it also depends on the
        // reading target, and accepting a pass that ran against the PREVIOUS
        // target put the old dimming back over unchanged source. When the
        // target has moved on, keep what the pane already has —
        // `on_target_selected` recomputed it for every open pane at the moment
        // of the change, so it is current by construction.
        let dimming_is_current =
            target == self.proj.reading_target && Arc::ptr_eq(&facts, &self.proj.build_facts);
        for slot in &mut self.proj.panes {
            // Applied only to a pane still showing the exact bytes this pass
            // ran over. Two passes for one file can be in flight and finish
            // in either order, and the old test — same path, same line COUNT
            // — happily accepted a stale pass whenever an edit left the line
            // count alone. The view then showed the previous highlighting,
            // symbols and doc comments over source the rest of the app (LSP,
            // index, search) already treated as current.
            if let Some(v) = slot
                && v.abs == abs
                && incremental::content_hash(v.source.as_bytes()) == src_hash
            {
                v.set_lines(lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                if dimming_is_current {
                    v.set_inactive_lines(inactive.clone());
                }
                v.highlighted = true;
            }
        }
        // Symbols just landed — resolve the function under the caret.
        self.follow_caret(Task::none())
    }

    /// Build the viewer from a clew-server `FileContent` reply — the server-side
    /// equivalent of `on_file_loaded` + `Highlighted` in one step (content
    /// arrives already highlighted, so there is no plain phase or flash).
    #[allow(clippy::too_many_arguments)] // mirrors the FileContent event's fields
    pub(crate) fn apply_file_content(
        &mut self,
        pane: usize,
        target: Option<usize>,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        // Opening a file leaves the doc page (and the overview/stats homes).
        self.proj.docs.page = None;
        let abs = root.join(&rel);
        let git_rel = rel.clone();
        let lang_key = highlight::detect(&abs);
        let source = Arc::new(source);
        let line_height = self.line_height();
        let old_viewport = self
            .proj
            .panes
            .get(pane)
            .and_then(|s| s.as_ref())
            .map(|v| (v.viewport_h, v.viewport_measured));

        let mut v = Viewer::new(abs.clone(), rel, lang_key, source.clone(), lines);
        v.symbols = symbols;
        v.docs = docs.into_iter().collect();
        // As in `apply_file_refresh`: the reading target is the client's, and
        // this pane did not exist when `on_target_selected` last refreshed the
        // open ones, so it has no later chance to be corrected.
        let dimmed = match lang_key {
            Some(lang) => self.inactive_for(&source, lang, &abs),
            None => inactive.into_iter().collect(),
        };
        v.set_inactive_lines(dimmed);
        v.highlighted = true;
        if let Some((h, measured)) = old_viewport {
            v.viewport_h = h;
            v.viewport_measured = measured;
        }
        v.target_line = target;
        let l0 = target.map(|t| t.saturating_sub(1)).unwrap_or(0);
        v.caret = Some((l0, landing_col(&self.proj.landing, &v, l0)));
        let y = v.scroll_offset_for(target, line_height);
        v.scroll_y = y;
        self.status = open_status(&v.rel, lang_key);
        // The pane's document is being replaced: any hover in flight is
        // about the file that was there.
        self.invalidate_hover();
        self.proj.panes[pane] = Some(v);
        // Seed the content hash so the watcher can tell real edits from noise.
        // Local projects only: a remote project's versions come from the index
        // publications, which are the one writer that sees EVERY file (see
        // `ProjectSymbols`). Hashing bytes in here as well would count a single
        // remote edit twice — once when the publication lands, once when this
        // re-read does — and make merely opening an untouched file look like a
        // change, rebuilding Stats and the project call graph for nothing.
        if self.local_project_state() {
            self.proj
                .registry
                .set(abs.clone(), incremental::content_hash(source.as_bytes()));
        }
        if pane == self.proj.active {
            self.refresh_import_tree();
        }

        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let lsp_task = match lang_key {
            Some(lang) => self.ensure_lsp(lang),
            None => Task::none(),
        };
        self.request_git_info(git_rel, abs.clone());
        // What the History view waits for this file to open for.
        let after = self.after_history_open(&abs);
        self.follow_caret(Task::batch([scroll, lsp_task, after]))
    }

    /// Rough pixel height of one rendered notebook cell, for scroll estimation.
    /// (Cells have variable height; goto scrolls near the cell and the target
    /// ring points precisely.)
    fn estimate_nb_cell_height(cell: &NbCell, expanded: bool, line_height: f32) -> f32 {
        let body = match cell.kind.as_str() {
            "code" => cell.lines.len().max(1) as f32 * (line_height + 1.0) + 34.0,
            _ => cell.source.lines().count().max(1) as f32 * 22.0 + 16.0,
        };
        let outputs = if cell.outputs.is_empty() {
            0.0
        } else if expanded {
            cell.outputs
                .iter()
                .map(|o| match o {
                    NbOutput::Text { spans, .. } => {
                        let lines: usize = spans
                            .iter()
                            .map(|(t, _)| t.matches('\n').count())
                            .sum::<usize>()
                            + 1;
                        lines.min(1_000) as f32 * 17.0 + 12.0
                    }
                    NbOutput::Image(_) | NbOutput::Svg(_) => 332.0,
                    NbOutput::Placeholder(_) => 24.0,
                })
                .sum::<f32>()
                + 26.0
        } else {
            26.0
        };
        body + outputs + 14.0
    }

    /// Estimated scroll offset that brings the cell containing projection
    /// `line` near the top of the notebook view.
    pub(crate) fn estimate_notebook_offset(
        doc: &NotebookDoc,
        expanded: &std::collections::HashSet<usize>,
        line: usize,
        line_height: f32,
    ) -> f32 {
        let target = doc
            .cells
            .iter()
            .rposition(|c| c.proj_line <= line)
            .unwrap_or(0);
        let y: f32 = doc
            .cells
            .iter()
            .enumerate()
            .take(target)
            .map(|(i, c)| Self::estimate_nb_cell_height(c, expanded.contains(&i), line_height))
            .sum();
        (y - 40.0).max(0.0)
    }

    /// Apply a `NotebookContent` reply: build the render-ready cell doc (markdown
    /// through the richmd pipeline, outputs into image/svg handles) and mount a
    /// viewer whose text is the script projection — so search hits, the outline,
    /// and goto all speak projection lines.
    ///
    /// `panes` is every pane the doc must be mounted into: an open names one, a
    /// refresh names all the panes showing the file (a split shows the same
    /// notebook twice). The cells are parsed once and shared; only the
    /// scroll/expanded state kept across a refresh is per pane.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_notebook_content(
        &mut self,
        panes: &[usize],
        target: Option<usize>,
        rel: String,
        language: String,
        cells: Vec<clew_protocol::NotebookCell>,
        symbols: Vec<Symbol>,
        projection: String,
        // refresh = a watcher-triggered reload of the open notebook: keep
        // scroll and expanded outputs, and skip the open-time side effects
        // (status, leaving the Docs page).
        refresh: bool,
    ) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        if !refresh {
            self.proj.docs.page = None;
        }
        let abs = root.join(&rel);
        // Prepare markdown cells first (this may spawn math/mermaid renders).
        let mut tasks: Vec<Task<Message>> = Vec::new();
        let mut prepared: Vec<NbCell> = Vec::new();
        for c in cells {
            let (segs, lines) = match c.kind.as_str() {
                "code" => (Vec::new(), c.lines),
                _ => {
                    let (segs, task) = self.prepare_segments(&c.source);
                    tasks.push(task);
                    (segs, Vec::new())
                }
            };
            let outputs = c
                .outputs
                .into_iter()
                .map(|o| match o {
                    clew_protocol::NotebookOutput::Text { spans, stderr } => {
                        NbOutput::Text { spans, stderr }
                    }
                    clew_protocol::NotebookOutput::Image { data } => {
                        NbOutput::Image(iced::widget::image::Handle::from_bytes(data))
                    }
                    clew_protocol::NotebookOutput::Svg(svg) => {
                        NbOutput::Svg(iced::widget::svg::Handle::from_memory(svg.into_bytes()))
                    }
                    clew_protocol::NotebookOutput::Placeholder(label) => {
                        NbOutput::Placeholder(label)
                    }
                })
                .collect();
            prepared.push(NbCell {
                kind: c.kind,
                source: c.source,
                segs,
                lines,
                proj_line: c.proj_line,
                outputs,
                execution_count: c.execution_count,
            });
        }
        // Single-threaded UI state; Arc only for cheap clones into iced
        // widgets (the prepared markdown items are not Sync).
        #[allow(clippy::arc_with_non_send_sync)]
        let doc = std::sync::Arc::new(NotebookDoc {
            language,
            cells: prepared,
        });

        let source = Arc::new(projection);
        let lines = highlight::plain_lines(&source);
        if !refresh {
            self.status.clear();
        }
        // The pane's document is being replaced: any hover in flight is
        // about the file that was there.
        self.invalidate_hover();
        let mut active_mounted = false;
        for &pane in panes {
            // Each pane keeps ITS own scroll and expanded outputs across a
            // refresh; the two halves of a split are read at different places.
            let Some(slot) = self.proj.panes.get(pane) else {
                continue;
            };
            let old = slot.as_ref().map(|v| {
                (
                    v.viewport_h,
                    v.viewport_measured,
                    v.scroll_y,
                    v.nb_expanded.clone(),
                )
            });
            let mut v = Viewer::new(
                abs.clone(),
                rel.clone(),
                None,
                source.clone(),
                lines.clone(),
            );
            v.symbols = symbols.clone();
            v.highlighted = true;
            v.notebook = Some(doc.clone());
            if let Some((h, measured, old_scroll, old_expanded)) = old {
                v.viewport_h = h;
                v.viewport_measured = measured;
                if refresh {
                    v.nb_expanded = old_expanded;
                    v.scroll_y = old_scroll;
                }
            }
            v.target_line = target;
            // The cell view has variable-height cells, so a goto scrolls to an
            // estimate of the target cell's offset; the highlight ring on the
            // cell (drawn by the view for `target_line`) does the precise
            // pointing. A refresh keeps the reader where they were instead.
            let y = if refresh {
                v.scroll_y
            } else {
                target
                    .map(|line| {
                        Self::estimate_notebook_offset(
                            &doc,
                            &v.nb_expanded,
                            line,
                            self.line_height(),
                        )
                    })
                    .unwrap_or(0.0)
            };
            v.scroll_y = y;
            self.proj.panes[pane] = Some(v);
            active_mounted |= pane == self.proj.active;
            tasks.push(operation::scroll_to(
                ui::code_scroll_id(pane),
                AbsoluteOffset { x: 0.0, y },
            ));
        }
        // The projection's hash stands in for the notebook's bytes — local
        // only, for the reason given in `apply_file_content`.
        if self.local_project_state() {
            self.proj
                .registry
                .set(abs, incremental::content_hash(source.as_bytes()));
        }
        if active_mounted {
            self.refresh_import_tree();
        }
        Task::batch(tasks)
    }

    /// Ask the server to read `rel` again so the panes showing it can reload in
    /// place. The reply lands as `ReadKind::Refresh`, which rebuilds a plain
    /// file through `apply_file_refresh` and a notebook through
    /// `apply_notebook_content` — the only way to rebuild a notebook pane, whose
    /// text is the parsed script projection rather than the file's bytes.
    pub(crate) fn request_file_refresh(&mut self, rel: &str) {
        let request = clew_protocol::Request::ReadFile {
            rel: rel.to_string(),
            target: self.target_spec(),
        };
        if let Some(id) = self.send_to_server(request) {
            // Retire any earlier refresh still in flight for this file: only
            // the newest may apply, and replies for one rel are not ordered
            // (the server reads off its request loop).
            for kind in self.proj.link.pending_reads.values_mut() {
                if matches!(kind, ReadKind::Refresh { rel: r } if r == rel) {
                    *kind = ReadKind::Retired;
                }
            }
            self.proj.link.pending_reads.insert(
                id,
                ReadKind::Refresh {
                    rel: rel.to_string(),
                },
            );
        }
    }

    /// Reload every pane showing `rel` in place after an on-disk change, keeping
    /// scroll / caret / folds (unlike opening, which jumps to a target line).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_file_refresh(
        &mut self,
        rel: String,
        source: String,
        lines: Vec<HlLine>,
        symbols: Vec<Symbol>,
        docs: Vec<(usize, String)>,
        inactive: Vec<usize>,
    ) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let abs = root.join(&rel);
        let source = Arc::new(source);
        let docs: HashMap<usize, String> = docs.into_iter().collect();
        // Evaluate the cfg dimming against the client's CURRENT reading
        // target rather than trusting the server's answer. The two agree
        // normally, but the server evaluated whatever `reading.toml` said when
        // this read started, and a target the user picked meanwhile would
        // otherwise be undone by the reply.
        let inactive: HashSet<usize> = match highlight::detect(&abs) {
            Some(lang) => self.inactive_for(&source, lang, &abs),
            None => inactive.into_iter().collect(),
        };
        let mut on_screen = false;
        for slot in &mut self.proj.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                // Keeps scroll / caret / collapsed folds; then restore the
                // highlighting bundle the reload cleared.
                v.reload(source.clone(), lines.clone());
                v.symbols = symbols.clone();
                v.docs = docs.clone();
                v.set_inactive_lines(inactive.clone());
                v.highlighted = true;
                on_screen = true;
                // `reload` clears the derived per-line state it cannot trust
                // across a content change, but not `git`, whose `blame` and
                // `status` vectors are indexed by 0-based line and describe
                // bytes that are now gone. Drop it here for the same reason,
                // and re-request below: a single insertion above shifts every
                // gutter bar, and the caret-line blame then names a plausible
                // but wrong commit, which "Explain why this line exists" would
                // hand to the LLM as fact. Cleared BEFORE the request so the
                // round trip shows nothing rather than something wrong, and so
                // a refresh with no transport leaves it empty, not lying.
                v.git = None;
            }
        }
        // Only the panes' gutters consume blame, so a file refreshed purely to
        // resync the language server (off screen) needs no git pass.
        if on_screen {
            self.request_git_info(rel, abs.clone());
        }
        // The server's copy of an open document is not refreshed by this
        // reload either — on a remote project this reply is the only carrier
        // of the new bytes the client ever sees (see `resync_open_doc`).
        self.resync_open_doc(&abs, &source);
        // Track the new bytes so the next change is detected against them —
        // local only, for the reason given in `apply_file_content`: remotely,
        // the publication that reported this change already bumped the version,
        // and hashing here would bump it a second time for the same edit.
        if self.local_project_state() {
            self.proj
                .registry
                .set(abs, incremental::content_hash(source.as_bytes()));
        }
        self.follow_caret(Task::none())
    }

    /// Ask the server for `rel`'s per-line blame + change status. It fills in
    /// asynchronously via `Event::GitInfo`, routed back to this file by the
    /// recorded `abs` rather than by re-deriving it from the current root (a
    /// project switch would otherwise paint another project's same-named file).
    ///
    /// Ordering: every blame request — this one, and a local `git::try_info`
    /// pass (`content_tasks`) — takes an id from the one request-id space, and
    /// `git_latest` records the newest per file. `on_git_info_loaded` paints
    /// only that one, so an older pass that lands last (the same file opened
    /// in a second pane mid-pass) is dropped instead of repainting the gutter
    /// from bytes the pane no longer shows.
    pub(crate) fn request_git_info(&mut self, rel: String, abs: PathBuf) {
        let request = clew_protocol::Request::GitInfo { rel };
        if let Some(id) = self.send_to_server(request) {
            // Retire any earlier blame still in flight for this file: only the
            // newest may paint, and replies for one file are not ordered (the
            // server reads off its request loop), so an older one landing last
            // would describe bytes the pane no longer shows. Same reasoning as
            // `request_file_refresh`.
            self.retire_git_requests_for(&abs);
            self.proj.git_latest.insert(abs.clone(), id);
            self.proj.link.pending_git.insert(id, abs);
        }
    }

    /// Retire every `GitInfo` request still in flight for `abs` (see
    /// `ProjectLink::retired_git`): its reply must neither paint nor report.
    pub(crate) fn retire_git_requests_for(&mut self, abs: &Path) {
        let link = &mut self.proj.link;
        let retired: Vec<u64> = link
            .pending_git
            .iter()
            .filter(|(_, p)| p.as_path() == abs)
            .map(|(id, _)| *id)
            .collect();
        for id in retired {
            link.pending_git.remove(&id);
            link.retired_git.insert(id);
        }
    }

    /// Blame + change status for `abs` arrived, from request `req` (a local
    /// `git::try_info` pass or a server `GitInfo` — one id space). Painted
    /// only when it is still the newest request for that file: two passes for
    /// one file are not ordered against each other, and the older one landing
    /// last repainted the gutter from bytes the pane no longer shows.
    ///
    /// `Err` is a git that could not answer (timed out, not installed, an
    /// object it could not read): the reason goes to the status bar — worded
    /// like the server's refusal of a remote `GitInfo` — and the gutter is
    /// cleared, since what it showed describes bytes this pass replaced. It
    /// used to arrive as `None`, an empty gutter that reads as "untracked".
    pub(crate) fn on_git_info_loaded(
        &mut self,
        abs: PathBuf,
        req: u64,
        info: Result<Option<Arc<git::GitInfo>>, String>,
    ) -> Task<Message> {
        if self.proj.git_latest.get(&abs) != Some(&req) {
            return Task::none();
        }
        self.proj.git_latest.remove(&abs);
        let info = match info {
            Ok(info) => info,
            Err(e) => {
                self.status = tr!(
                    "Couldn't read git blame for {file}: {error}",
                    file = self.rel_of(&abs),
                    error = e
                );
                None
            }
        };
        for slot in &mut self.proj.panes {
            if let Some(v) = slot
                && v.abs == abs
            {
                v.git = info.clone();
            }
        }
        Task::none()
    }

    /// Put `pane`'s code view back where its reader left it — both offsets —
    /// after a transition that rebuilt its scrollable in another widget's
    /// place (the diff view closing, the markdown source shown again, time
    /// travel left): a fresh scrollable starts at the top and reports it,
    /// which would overwrite the offsets the viewer keeps. Captured here,
    /// before that report can arrive.
    pub(crate) fn restore_code_scroll(&self, pane: usize) -> Task<Message> {
        let Some(v) = self.proj.panes.get(pane).and_then(Option::as_ref) else {
            return Task::none();
        };
        operation::scroll_to(
            ui::code_scroll_id(pane),
            AbsoluteOffset {
                x: v.scroll_x,
                y: v.scroll_y,
            },
        )
    }

    pub(crate) fn on_toggle_diff(&mut self) -> Task<Message> {
        // A second press while the diff is still being computed cancels it:
        // its reply finds no pending request and is dropped, instead of
        // opening the view the user just asked to turn off.
        if self.proj.diff_pending.take().is_some() {
            self.status = tr!("Diff cancelled").into();
            return Task::none();
        }
        // Toggle off if already showing this file's diff.
        let active_abs = self.active_viewer().map(|v| v.abs.clone());
        if let (Some(d), Some(abs)) = (&self.proj.diff, &active_abs)
            && d.abs == *abs
        {
            self.proj.diff = None;
            self.proj.diff_refresh = None;
            // The code view is built anew in the diff's place: put it back
            // where the reader left it.
            return self.restore_code_scroll(self.proj.active);
        }
        let Some(abs) = active_abs else {
            return Task::none();
        };
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        let others = self.one_file_view(FileView::Diff);
        let req = self.mint_request_id();
        self.proj.diff_pending = Some((req, abs.clone()));
        self.status = tr!("Computing the diff of {file}…", file = self.rel_of(&abs));
        Task::batch([others, self.load_diff(git, req, abs)])
    }

    /// The file's other views put away for `keep`. Skim, Diff and Time
    /// Travel each show the file another way, and only one shows at a time:
    /// a diff asked for while Time Travel was on never showed, its button
    /// lit all the same, and Skim folded a view no one could see.
    pub(crate) fn one_file_view(&mut self, keep: FileView) -> Task<Message> {
        let mut tasks = Vec::new();
        if keep != FileView::TimeTravel && ui::time_travel_pane(self) == Some(self.proj.active) {
            tasks.push(self.update(Message::TimeTravel(TimeTravelMsg::Exit)));
        }
        if keep != FileView::Diff {
            self.proj.diff_pending = None;
            if self.proj.diff.take().is_some() {
                self.proj.diff_refresh = None;
                tasks.push(self.restore_code_scroll(self.proj.active));
            }
        }
        if keep != FileView::Skim
            && let Some(abs) = self.active_viewer().map(|v| v.abs.clone())
        {
            let sig_lines = self.skim_lines(&abs);
            if self.active_viewer().is_some_and(|v| v.skimmed(&sig_lines)) {
                tasks.push(self.fold_in(self.proj.active, |v| v.skim_bodies(&sig_lines)));
            }
        }
        Task::batch(tasks)
    }

    /// The diff on screen again, when `path` — just changed or deleted on
    /// disk — is its file: it showed the file as it was when Diff was
    /// pressed, and said "no uncommitted changes" over a file just edited.
    pub(crate) fn refresh_open_diff(&mut self, path: &Path) -> Task<Message> {
        if self.proj.diff.as_ref().is_none_or(|d| d.abs != path) {
            return Task::none();
        }
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        let req = self.mint_request_id();
        self.proj.diff_refresh = Some(req);
        self.load_diff(git, req, path.to_path_buf())
    }

    /// Compute `abs`'s diff against `HEAD`, arriving as `DiffLoaded`.
    fn load_diff(&self, git: crate::app::rpc::GitSource, req: u64, abs: PathBuf) -> Task<Message> {
        let rel = self.rel_of(&abs);
        let stamp = self.stamp();
        Task::perform(
            async move {
                let result = git
                    .run::<Option<Vec<git::DiffLine>>>(clew_protocol::GitOp::DiffLines {
                        rel: rel.clone(),
                    })
                    .await;
                (abs, rel, result)
            },
            move |(abs, rel, result)| {
                Message::Editor(EditorMsg::DiffLoaded {
                    stamp: stamp.clone(),
                    req,
                    abs,
                    rel,
                    result,
                })
            },
        )
    }

    /// A diff load finished. Applied only while it is still the load the user
    /// is waiting for, and only onto the file the active pane still shows. A
    /// failure is reported as a failure — it used to become an empty diff,
    /// which reads as "no changes".
    pub(crate) fn on_diff_loaded(
        &mut self,
        req: u64,
        abs: PathBuf,
        rel: String,
        result: Result<Option<Vec<git::DiffLine>>, String>,
    ) -> Task<Message> {
        if self.proj.diff_refresh == Some(req) {
            self.proj.diff_refresh = None;
            // The view takes the new lines while it still shows that file;
            // a failure leaves what it showed (the edit's own status stands).
            if let Ok(lines) = result
                && self.proj.diff.as_ref().is_some_and(|d| d.abs == abs)
            {
                self.proj.diff = Some(DiffState::new(abs, rel, lines.unwrap_or_default()));
            }
            return Task::none();
        }
        if self.proj.diff_pending.as_ref().map(|(r, _)| *r) != Some(req) {
            return Task::none();
        }
        self.proj.diff_pending = None;
        match result {
            Err(e) => {
                self.status = tr!(
                    "Couldn't compute the diff of {file}: {error}",
                    file = rel,
                    error = e
                )
            }
            Ok(None) => {
                self.status = tr!("No diff for {file} — it is not tracked by git", file = rel);
            }
            Ok(Some(lines)) => {
                // Show the diff only if the active pane still shows that file —
                // a slow diff must not display against another file.
                if self.active_viewer().is_some_and(|v| v.abs == abs) {
                    // No changes is said by the diff's own pane; the status
                    // line said it again under it.
                    self.status.clear();
                    self.proj.diff = Some(DiffState::new(abs, rel, lines));
                }
            }
        }
        Task::none()
    }

    pub(crate) fn on_toggle_split(&mut self) -> Task<Message> {
        // Panes are about to move or close under the cursor.
        self.invalidate_hover();
        if self.proj.split {
            self.proj.split = false;
            self.proj.panes[1] = None;
            // A load still in flight for the closed pane must not resurrect
            // it as an invisible viewer — nor one a time-travel start there
            // superseded, carried out once its history came to nothing: into
            // a new second pane, should the split be open again by then.
            self.cancel_pane_load(1);
            self.proj.link.superseded_open.take_if(|s| s.pane == 1);
            // A time-travel start made there goes on in the pane left, should
            // that show its file — its history shows there, and a jump there
            // gives it up (`jump_gives_up_time_start`) — and is given up
            // otherwise: its history would bring a session no pane shows,
            // replacing the one there is, which opening its file ends.
            let shown = self.proj.panes[0].as_ref().map(|v| v.abs.clone());
            if let Some(start) = self.proj.time_start.as_mut().filter(|s| s.pane == 1) {
                if shown.as_ref() == Some(&start.abs) {
                    start.pane = 0;
                } else {
                    // What it held back went with the pane, above.
                    let _ = self.give_up_time_travel_start();
                }
            }
            self.proj.active = 0;
        } else {
            self.proj.split = true;
            // Duplicate the current file for side-by-side reading.
            self.proj.panes[1] = self.proj.panes[0].clone();
            self.proj.active = 1;
        }
        Task::none()
    }

    pub(crate) fn on_select_start(
        &mut self,
        pane: usize,
        line: usize,
        col: usize,
    ) -> Task<Message> {
        if pane == 0 || self.proj.split {
            self.proj.active = pane;
        }
        // Clicking the code gives it keyboard focus for cursor motion.
        self.code_focused = true;
        // Cmd/Ctrl-click is go-to-definition, not selection.
        if self.modifiers.command() && !self.modifiers.shift() {
            return self.goto_definition(pane, line, col);
        }
        let extend = self.modifiers.shift();
        if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) {
            let head = (line, col);
            match (extend, v.selection) {
                // Shift-click keeps the existing anchor and moves the head.
                (true, Some((anchor, _))) => v.selection = Some((anchor, head)),
                _ => v.selection = Some((head, head)),
            }
            v.caret = Some(head);
            self.selecting = true;
            self.reader_moved_caret(pane);
        }
        let follow = self.follow_caret(Task::none());
        Task::batch([follow, self.sync_reading_context()])
    }

    pub(crate) fn on_select_drag(&mut self, pane: usize, line: usize, col: usize) -> Task<Message> {
        if self.selecting
            && pane == self.proj.active
            && let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut)
            && let Some((anchor, _)) = v.selection
        {
            let head = (line, col);
            v.selection = Some((anchor, head));
            v.caret = Some(head);
            self.reader_moved_caret(pane);
        }
        Task::none()
    }

    pub(crate) fn on_copy_selection(&mut self) -> Task<Message> {
        // In time travel, copy the historical selection, not the live one —
        // while the session is on screen: off it, the reader is copying from
        // the file that is.
        let viewer = ui::time_travel_on_screen(self)
            .and_then(|t| t.viewer.as_ref())
            .or_else(|| self.active_viewer());
        let Some(text) = viewer.and_then(Viewer::selected_text) else {
            return Task::none();
        };
        let n = text.lines().count();
        self.status = trn!("Copied 1 line", "Copied {n} lines", n);
        iced::clipboard::write(text)
    }

    pub(crate) fn on_minimap_scrolled(&mut self, pane: usize, fraction: f32) -> Task<Message> {
        let lh = self.line_height();
        if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) {
            let total = v.content_rows() as f32 * lh;
            let max_y = (total - v.viewport_h).max(0.0);
            // Center the clicked fraction in the viewport.
            let y = (fraction * total - v.viewport_h / 2.0).clamp(0.0, max_y);
            v.scroll_y = y;
            // Across, it stays where it was: the minimap moves the file down.
            let x = v.scroll_x;
            self.reader_moved_caret(pane);
            return operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x, y });
        }
        Task::none()
    }

    /// The code panes back from under a page that covered them (Statistics,
    /// the Overview, Weak spots, the Glossary, a docs page): drawn afresh,
    /// their code views start at the top, so each is scrolled back to where
    /// the reader left it — the caret's line still on screen. `None` while
    /// nothing was put away.
    pub(crate) fn sync_panes_uncovered(&mut self) -> Option<Task<Message>> {
        let covered = ui::pane_cover(self).is_some();
        let was = std::mem::replace(&mut self.panes_covered, covered);
        if !was || covered {
            return None;
        }
        let scrolls: Vec<Task<Message>> = self
            .proj
            .panes
            .iter()
            .enumerate()
            .filter_map(|(pane, v)| {
                let v = v.as_ref()?;
                let (x, y) = (v.scroll_x, v.scroll_y);
                (x > 0.0 || y > 0.0).then(|| {
                    operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x, y })
                })
            })
            .collect();
        (!scrolls.is_empty()).then(|| Task::batch(scrolls))
    }

    pub(crate) fn on_outline_jump(&mut self, line: usize) -> Task<Message> {
        let Some(abs) = self.active_viewer().map(|v| v.abs.clone()) else {
            return Task::none();
        };
        // Cmd+click an outline symbol explains it; a plain click jumps.
        if self.modifiers.command() {
            self.explain_symbol_at(abs, line)
        } else {
            self.open_file(abs, Some(line), true)
        }
    }

    /// Keep the top visible line stable across a line-height change.
    pub(crate) fn rescale_scroll(&mut self, old_line_height: f32) -> Task<Message> {
        let new_line_height = self.line_height();
        if (new_line_height - old_line_height).abs() < f32::EPSILON {
            return Task::none();
        }
        // Both ways: a character is as much wider as a line is taller, so the
        // column at the left edge stays there — set to 0, it threw the reader
        // back to the start of every line.
        let scale = new_line_height / old_line_height;
        // The past revision on screen scrolls apart from the pane's live file:
        // scaled too, and the one the pane's scrollable is told.
        let shown_tt = ui::time_travel_pane(self);
        let tt = self.proj.time_travel.as_mut().map(|tt| {
            tt.scroll_x *= scale;
            tt.scroll_y *= scale;
            (tt.scroll_x, tt.scroll_y)
        });
        let mut tasks = Vec::new();
        for (pane, slot) in self.proj.panes.iter_mut().enumerate() {
            if let Some(v) = slot {
                v.scroll_x *= scale;
                v.scroll_y *= scale;
                let (x, y) = match tt {
                    Some(offset) if shown_tt == Some(pane) => offset,
                    _ => (v.scroll_x, v.scroll_y),
                };
                tasks.push(operation::scroll_to(
                    ui::code_scroll_id(pane),
                    AbsoluteOffset { x, y },
                ));
            }
        }
        Task::batch(tasks)
    }

    /// How much of `pane`'s code view is seen: its height, less the foot of
    /// it the Ask pill floats over ([`ui::pill_over_code`]). The caret kept
    /// in view is kept in this — moved down a line at a time in a narrow
    /// window, it went on under the pill — and a page is as many lines.
    pub(crate) fn code_seen_h(&self, pane: usize) -> f32 {
        let line_height = self.line_height();
        self.proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .map_or(0.0, |v| {
                (v.viewport_h - ui::pill_over_code(self, pane)).max(line_height)
            })
    }

    /// Move the active pane's block cursor and scroll it into view.
    pub(crate) fn move_cursor(&mut self, motion: viewer::Motion) -> Task<Message> {
        let pane = self.proj.active;
        let line_height = self.line_height();
        let seen_h = self.code_seen_h(pane);
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.move_caret(motion);
        let (line, _) = v.caret.unwrap_or((0, 0));
        // Keep the cursor line within the viewport (in display rows, so folds
        // above it are accounted for) — above the Ask pill, where it floats
        // over the code's foot.
        let top = v.row_of(line) as f32 * line_height;
        let bottom = top + line_height;
        if top < v.scroll_y {
            v.scroll_y = top;
        } else if bottom > v.scroll_y + seen_h {
            v.scroll_y = bottom - seen_h;
        }
        let y = v.scroll_y;
        self.reader_moved_caret(pane);
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let follow = self.follow_caret(scroll);
        Task::batch([follow, self.sync_reading_context()])
    }

    /// Scroll the active pane by `pages` screens — down when positive — the
    /// caret going as far, so it stays where it was on the screen; at the
    /// end it goes to the last line (the first, scrolling up).
    pub(crate) fn scroll_pages(&mut self, pages: i32) -> Task<Message> {
        let line_height = self.line_height();
        if self.active_viewer().is_none() {
            return Task::none();
        }
        let seen_h = self.code_seen_h(self.proj.active);
        let page = ((seen_h / line_height).floor() as i64 - 1).max(1);
        self.scroll_rows(page * i64::from(pages))
    }

    /// [`Self::scroll_pages`] by `rows` lines of the screen.
    pub(crate) fn scroll_rows(&mut self, by: i64) -> Task<Message> {
        let pane = self.proj.active;
        let line_height = self.line_height();
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        let rows = v.content_rows() as i64;
        // As far as the view scrolls: on past the end until the last line
        // is the first in view (`ui::panes`' room after the code). Capped a
        // screen short of that, "scroll down" from near the end moved up.
        let most = ((rows - 1).max(0)) as f32 * line_height;
        let before = v.scroll_y;
        v.scroll_y = (before + by as f32 * line_height).clamp(0.0, most);
        let moved = ((v.scroll_y - before) / line_height).round() as i64;
        let row = match v.caret {
            _ if moved == 0 => {
                if by > 0 {
                    rows - 1
                } else {
                    0
                }
            }
            Some((line, _)) => v.row_of(line) as i64 + moved,
            None => (v.scroll_y / line_height) as i64,
        };
        let line = v.line_at_row(row.clamp(0, (rows - 1).max(0)) as usize);
        v.caret = Some((line, 0));
        let y = v.scroll_y;
        self.reader_moved_caret(pane);
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        let follow = self.follow_caret(scroll);
        Task::batch([follow, self.sync_reading_context()])
    }

    /// Change `pane`'s folds with `fold`. A fold that hides the caret pulls
    /// it onto its header, which is the reader moving it as much as a click
    /// is (`reader_moved_caret`). The line at the top of the view stays at
    /// the top — its fold's header, folded away — and the view no further
    /// down than the code now runs: left where it was, a file folded short
    /// (Skim near its end) showed nothing but its last line.
    fn fold_in(&mut self, pane: usize, fold: impl FnOnce(&mut Viewer)) -> Task<Message> {
        let line_height = self.line_height();
        // Time Travel shows its own view in this pane's place, by this
        // pane's scroll id: the live file's rows are not its.
        let in_time_travel = ui::time_travel_pane(self) == Some(pane);
        let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) else {
            return Task::none();
        };
        let caret = v.caret;
        let rows = v.content_rows();
        let top = v.line_at_row((v.scroll_y / line_height.max(1.0)) as usize);
        fold(v);
        // Scrolled by the code's rows only where the view is them, and only
        // when the fold changed them: a notebook's cells and a rendered
        // document scroll by their own measure, and a fold that changed
        // nothing (`za` off a fold, `zR` with none closed) snapped the view
        // to a row.
        let by_rows = !in_time_travel
            && v.notebook.is_none()
            && (v.md.is_none() || v.show_source)
            && v.content_rows() != rows;
        if !by_rows {
            if v.caret != caret {
                self.reader_moved_caret(pane);
            }
            return Task::none();
        }
        let row = v.row_of(top);
        // Folded away: its fold's header, the visible line before it.
        let row = if v.line_at_row(row) == top {
            row
        } else {
            row.saturating_sub(1)
        };
        let most = v.content_rows().saturating_sub(1) as f32 * line_height;
        let y = (row as f32 * line_height).min(most);
        let scroll = if (y - v.scroll_y).abs() >= 0.5 {
            v.scroll_y = y;
            operation::scroll_to(
                ui::code_scroll_id(pane),
                AbsoluteOffset { x: v.scroll_x, y },
            )
        } else {
            Task::none()
        };
        if v.caret != caret {
            self.reader_moved_caret(pane);
        }
        scroll
    }

    /// Toggle the fold enclosing the caret (`za`).
    pub(crate) fn fold_toggle_at_cursor(&mut self) -> Task<Message> {
        self.fold_in(self.proj.active, |v| {
            let line = v.caret.map(|(l, _)| l).unwrap_or(0);
            if let Some(header) = v.fold_header_for(line) {
                v.toggle_fold(header);
            }
        })
    }

    /// Collapse (`zM`) or expand (`zR`) every fold in the active pane.
    pub(crate) fn fold_all(&mut self, collapse: bool) -> Task<Message> {
        self.fold_in(self.proj.active, |v| {
            if collapse {
                v.collapse_all();
            } else {
                v.expand_all();
            }
        })
    }

    /// Toggle "skim" for the active file: fold every function/method body down
    /// to its signature (which still shows its inline summary), so the file
    /// reads as an annotated table of contents. Uses the symbol index to fold
    /// only bodies, leaving impl/mod blocks open so every signature stays shown.
    pub(crate) fn skim_active_file(&mut self) -> Task<Message> {
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let functions = self.skim_lines(&v.abs);
        self.fold_in(self.proj.active, |v| v.skim_bodies(&functions))
    }

    /// Where `abs`'s functions and methods start and end (0-based; the end
    /// `usize::MAX` when not known): what Skim folds. From the file's own
    /// outline when it is open — which knows where each ends — else from
    /// the project's index, which knows where each starts.
    pub(crate) fn skim_lines(&self, abs: &Path) -> Vec<(usize, usize)> {
        let open = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == abs && !v.symbols.is_empty());
        if let Some(v) = open {
            return v
                .symbols
                .iter()
                .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                .map(|s| (s.line.saturating_sub(1), s.end_line.saturating_sub(1)))
                .collect();
        }
        self.proj
            .symbol_index_by_file
            .get(abs)
            .map(|syms| {
                syms.iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .map(|s| (s.line.saturating_sub(1), usize::MAX)) // 1-based → 0-based
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn on_find_opened(&mut self) -> Task<Message> {
        if self.active_viewer().is_none() {
            return Task::none();
        }
        self.proj.find.open = true;
        self.code_focused = false; // the find input takes focus
        // Reveal everything so matches inside collapsed folds are shown.
        if let Some(v) = self.active_viewer_mut() {
            v.expand_all();
        }
        self.recompute_find();
        Task::batch([
            operation::focus(ui::find_input_id()),
            operation::select_all(ui::find_input_id()),
        ])
    }

    /// The document the active pane is showing right now, as the identity a
    /// find-match list is stamped with. `None` when no pane holds a file.
    pub(crate) fn active_find_doc(&self) -> Option<find::DocId> {
        self.active_viewer()
            .map(|v| (v.abs.clone(), v.lines_generation()))
    }

    /// Recompute the find matches over the active pane's current document.
    pub(crate) fn recompute_find(&mut self) {
        let Some(doc) = self.active_find_doc() else {
            // Nothing on screen to match against, so nothing may be painted:
            // keeping the previous file's triples here is what let them be
            // drawn over the next document that arrives.
            self.proj.find.matches.clear();
            self.proj.find.current = 0;
            self.proj.find.doc = None;
            return;
        };
        let Some(lines) = self.active_viewer().map(|v| v.lines.clone()) else {
            return;
        };
        self.proj.find.recompute(doc, &lines);
    }

    /// Keep the find matches describing the document actually on screen.
    ///
    /// `find.matches` are raw (line, col0, col1) triples in ONE document's
    /// coordinates, and every consumer — the painted highlight rectangles, the
    /// `n/m` counter, the caret jump — uses them without re-checking which
    /// file that was. Opening another file into the pane, reloading the same
    /// file after an on-disk change, and clicking into the other half of a
    /// split all leave the bar open with the previous document's triples, so
    /// the highlights land on unrelated substrings and Enter parks the caret
    /// where the query does not occur. Recompute (rather than clear) so the
    /// user's query survives the move and `current` re-anchors near the same
    /// line. Called once per update, so a new way to swap a pane's document
    /// cannot forget it — the identity stamp makes the check a no-op when
    /// nothing changed.
    pub(crate) fn sync_find_matches(&mut self) {
        if !self.proj.find.open {
            return;
        }
        if self.proj.find.doc == self.active_find_doc() {
            return;
        }
        self.recompute_find();
    }

    /// Move the cursor to the current find match and scroll it into view.
    pub(crate) fn jump_to_find_match(&mut self) -> Task<Message> {
        let Some((line, col, _)) = self.proj.find.current_match() else {
            return Task::none();
        };
        let pane = self.proj.active;
        let line_height = self.line_height();
        let seen_h = self.code_seen_h(pane);
        let Some(v) = self.active_viewer_mut() else {
            return Task::none();
        };
        v.caret = Some((line, col));
        if let Some(doc) = v.notebook.clone() {
            // Notebook: the view can't paint per-match highlights, so point at
            // the owning cell instead — estimated scroll plus the target ring.
            v.target_line = Some(line + 1);
            v.scroll_y =
                Self::estimate_notebook_offset(&doc, &v.nb_expanded, line + 1, line_height);
        } else {
            // Center-ish the match line (display rows account for folds),
            // unless it is in view already — clear of the Ask pill.
            let top = v.row_of(line) as f32 * line_height;
            if top < v.scroll_y || top + line_height > v.scroll_y + seen_h {
                v.scroll_y = (top - v.viewport_h / 3.0).max(0.0);
            }
        }
        let y = v.scroll_y;
        self.reader_moved_caret(pane);
        let scroll = operation::scroll_to(ui::code_scroll_id(pane), AbsoluteOffset { x: 0.0, y });
        self.follow_caret(scroll)
    }

    /// Extra span highlights for the code view of `pane`: find matches, or
    /// (when not finding) the occurrences of the identifier under the cursor
    /// and the matching bracket.
    pub fn code_highlights(&self, pane: usize, v: &Viewer) -> Vec<codeview::Hl> {
        use codeview::{Hl, HlKind};
        let mut out = Vec::new();
        if pane != self.proj.active {
            return out;
        }

        // Diagnostic underlines (always shown, from the LSP server), read
        // from this update's snapshot rather than a locked copy per view.
        if let Some(lang) = v.lang_key
            && let Some(LspSlot::Ready(client)) = self.proj.link.lsp.get(lang)
            && let Some(Ok(snapshot)) = self.proj.link.lsp_snapshots.get(lang)
        {
            for d in snapshot.diagnostics(&v.abs) {
                let (c0, c1) = v.lsp_span(d.line, d.char_start, d.char_end, client.encoding);
                out.push(Hl {
                    line: d.line,
                    col0: c0.into(),
                    col1: c1.into(),
                    kind: match d.severity {
                        1 => HlKind::DiagError,
                        2 => HlKind::DiagWarn,
                        _ => HlKind::DiagHint,
                    },
                });
            }
        }

        // While finding, the matches themselves are painted by the code view
        // straight from `self.find` (`CodeView::find_matches`); copying every
        // match in here cost O(matches) per view for a one-letter query.
        if self.proj.find.open {
            return out;
        }

        // Cursor-derived aids, only while reading (code has focus).
        if !self.code_focused {
            return out;
        }
        let Some((line, col)) = v.caret else {
            return out;
        };

        // Occurrences of the identifier under the cursor (2+ to be useful).
        if let Some(word) = analyze::word_at(&v.lines, line, col) {
            let occ = v.occurrences(&word, 500);
            if occ.len() > 1 {
                for (l, c0, c1) in occ {
                    out.push(Hl {
                        line: l,
                        col0: c0,
                        col1: c1,
                        kind: HlKind::Occurrence,
                    });
                }
            }
        }

        // Matching bracket pair.
        if let Some((ml, mc)) = v.matching_bracket(line, col) {
            out.push(Hl {
                line,
                col0: col,
                col1: col + 1,
                kind: HlKind::Bracket,
            });
            out.push(Hl {
                line: ml,
                col0: mc,
                col1: mc + 1,
                kind: HlKind::Bracket,
            });
        }
        out
    }

    /// Inline blame annotation for the caret line: `author, when · summary`.
    pub fn blame_annotation(&self, v: &Viewer) -> Option<(usize, String)> {
        let git = v.git.as_ref()?;
        let (line, _) = v.caret?;
        let b = git.blame_for(line)?;
        if b.commit.is_empty() {
            return None;
        }
        let text = if b.uncommitted {
            tr!("Uncommitted change").to_string()
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(b.time);
            let mut summary = b.summary.clone();
            if summary.chars().count() > 60 {
                summary = summary.chars().take(59).collect::<String>() + "…";
            }
            // What the commit said, then who and when.
            format!(
                "{summary} ({}, {})",
                b.author,
                crate::i18n::ago(b.time, now)
            )
        };
        Some((line, text))
    }

    /// Sticky-scroll header lines for a viewer at its current scroll position.
    pub fn sticky_headers(&self, v: &Viewer) -> Vec<usize> {
        let row = (v.scroll_y / self.line_height()) as usize;
        // Read enclosing headers off the precomputed fold ranges — cheap enough
        // to recompute each frame, so sticky scroll stays smooth in huge files.
        v.sticky_at_row(row)
    }

    pub(crate) fn rel_of(&self, abs: &Path) -> String {
        self.proj
            .project
            .as_ref()
            .and_then(|p| abs.strip_prefix(&p.root).ok())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|| abs.display().to_string())
    }

    /// Whether a pane shows the project's file `rel`.
    pub(crate) fn shows_rel(&self, rel: &str) -> bool {
        let Some(root) = self.proj.project.as_ref().map(|p| &p.root) else {
            return false;
        };
        let abs = root.join(rel);
        self.proj.panes.iter().flatten().any(|v| v.abs == abs)
    }
}

impl App {
    /// Handle a [`EditorMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_editor(&mut self, message: EditorMsg) -> Task<Message> {
        match message {
            EditorMsg::OpenRel { rel, line } => self.on_open_rel(rel, line),
            EditorMsg::OpenAbs { abs, line, push } => self.open_file(abs, line, push),
            EditorMsg::FileLoaded {
                req,
                pane,
                abs,
                target,
                result,
                ..
            } => self.on_file_loaded(req, pane, abs, target, result),
            EditorMsg::HeldOpenFailed { said, .. } => {
                self.status = said;
                Task::none()
            }
            EditorMsg::Highlighted {
                abs,
                src_hash,
                lines,
                symbols,
                docs,
                inactive,
                target,
                facts,
                ..
            } => self.on_highlighted(abs, src_hash, lines, symbols, docs, inactive, target, facts),
            EditorMsg::GitInfoLoaded { abs, req, info, .. } => {
                self.on_git_info_loaded(abs, req, info)
            }
            EditorMsg::Scrolled(pane, viewport) => {
                if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) {
                    v.scroll_y = viewport.absolute_offset().y;
                    v.scroll_x = viewport.absolute_offset().x;
                    v.viewport_h = viewport.bounds().height;
                    v.viewport_measured = true;
                }
                Task::none()
            }
            EditorMsg::PaneFocused(pane) => {
                if pane == 0 || self.proj.split {
                    self.proj.active = pane;
                    // The Imports tab follows the focused pane's file.
                    self.refresh_import_tree();
                }
                Task::none()
            }
            EditorMsg::ToggleSplit => self.on_toggle_split(),
            EditorMsg::SelectStart { pane, line, col } => self.on_select_start(pane, line, col),
            EditorMsg::SelectDrag { pane, line, col } => self.on_select_drag(pane, line, col),
            EditorMsg::SelectEnd => {
                self.selecting = false;
                Task::none()
            }
            EditorMsg::FoldToggle { pane, line } => self.fold_in(pane, |v| v.toggle_fold(line)),
            EditorMsg::MinimapScrolled { pane, fraction } => {
                self.on_minimap_scrolled(pane, fraction)
            }
            EditorMsg::ReaderScrolled(pane) => {
                self.reader_moved_caret(pane);
                Task::none()
            }
            EditorMsg::CopySelection => self.on_copy_selection(),
            EditorMsg::ToggleDiff => self.on_toggle_diff(),
            EditorMsg::DiffLoaded {
                req,
                abs,
                rel,
                result,
                ..
            } => self.on_diff_loaded(req, abs, rel, result),
            EditorMsg::OutlineJump(line) => self.on_outline_jump(line),
            EditorMsg::FontSizeDelta(delta) => {
                let old = self.line_height();
                // Never below the smallest text the UI draws anywhere.
                self.font_size = (self.font_size + delta).clamp(theme::MIN_TEXT_SIZE, 22.0);
                self.rescale_scroll(old)
            }
            EditorMsg::FontSizeReset => {
                let old = self.line_height();
                self.font_size = DEFAULT_FONT_SIZE;
                self.rescale_scroll(old)
            }
            EditorMsg::KeyPressed(key, modifiers) => self.handle_key(key, modifiers),
            // A menu click (or a chord the menu owns): the same table the key
            // handler runs, behind the same modal states (`on_menu_action`).
            EditorMsg::RunAction(action) => self.on_menu_action(action),
            EditorMsg::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers;
                if !modifiers.command() {
                    self.invalidate_hover(); // hover is a Cmd-hover affordance
                }
                Task::none()
            }
            EditorMsg::FindOpened => self.on_find_opened(),
            EditorMsg::FindQueryChanged(q) => {
                self.proj.find.query = q;
                self.recompute_find();
                self.jump_to_find_match()
            }
            EditorMsg::FindToggle(opt) => {
                let find = &mut self.proj.find;
                let on = match opt {
                    crate::SearchOpt::Case => &mut find.case_sensitive,
                    crate::SearchOpt::WholeWord => &mut find.whole_word,
                    crate::SearchOpt::Regex => &mut find.regex,
                };
                *on = !*on;
                self.recompute_find();
                self.jump_to_find_match()
            }
            EditorMsg::FindStep(delta) => {
                self.proj.find.step(delta);
                self.jump_to_find_match()
            }
            EditorMsg::FindClosed => {
                self.proj.find.open = false;
                self.code_focused = true;
                Task::none()
            }
            EditorMsg::SkimFile => {
                // Skimming puts the file's other views away; unskimming
                // leaves them be.
                let skimmed = self
                    .active_viewer()
                    .is_some_and(|v| v.skimmed(&self.skim_lines(&v.abs)));
                let others = if skimmed {
                    Task::none()
                } else {
                    self.one_file_view(FileView::Skim)
                };
                let fold = self.skim_active_file();
                Task::batch([others, fold])
            }
            EditorMsg::ScrollPages(pages) => self.scroll_pages(pages),
            EditorMsg::ScrollLines(lines) => self.scroll_rows(i64::from(lines)),
            EditorMsg::MoveCaret(motion) => self.move_cursor(motion),
            EditorMsg::FoldAll(collapse) => self.fold_all(collapse),
            EditorMsg::FoldHere => self.fold_toggle_at_cursor(),
            EditorMsg::GotoLineNumber(line) => {
                let Some(v) = self.active_viewer() else {
                    return Task::none();
                };
                let line = line.clamp(1, v.lines.len().max(1));
                let abs = v.abs.clone();
                self.open_file(abs, Some(line), true)
            }
            EditorMsg::NbToggleOutputs { pane, cell } => {
                if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut)
                    && !v.nb_expanded.remove(&cell)
                {
                    v.nb_expanded.insert(cell);
                }
                Task::none()
            }
            EditorMsg::NbExpandAll { pane, expand } => {
                // Batch writes to the same per-cell set the individual toggles
                // use, so the two compose without extra mode state.
                if let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) {
                    if !expand {
                        v.nb_expanded.clear();
                    } else if let Some(doc) = &v.notebook {
                        v.nb_expanded = doc
                            .cells
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| !c.outputs.is_empty())
                            .map(|(i, _)| i)
                            .collect();
                    }
                }
                Task::none()
            }
            EditorMsg::ToggleMarkdownSource(pane) => {
                let Some(v) = self.proj.panes.get_mut(pane).and_then(Option::as_mut) else {
                    return Task::none();
                };
                v.show_source = !v.show_source;
                // Back to the source: its code view is built anew in the
                // rendered document's place, so it is put back where it was.
                if v.show_source {
                    self.restore_code_scroll(pane)
                } else {
                    Task::none()
                }
            }
        }
    }
}

/// The column a jump to `line0` of `v`'s file lands on: the name a
/// definition or symbol jumped to there (`landing`), else the line's start.
fn landing_col(
    landing: &Option<(PathBuf, usize, crate::app::state::LandingAt)>,
    v: &Viewer,
    line0: usize,
) -> usize {
    use crate::app::state::LandingAt;
    use crate::viewer::{Col, PositionEncoding};
    let Some((abs, line, at)) = landing else {
        return 0;
    };
    if *abs != v.abs || *line != line0 {
        return 0;
    }
    let Some(raw) = v.source_line(line0) else {
        return 0;
    };
    match at {
        LandingAt::Char(character, encoding) => Col::from_offset(raw, *character, *encoding).0,
        LandingAt::Name(name) => {
            word_at(raw, name).map_or(0, |at| Col::from_offset(raw, at, PositionEncoding::Utf8).0)
        }
    }
}

/// Where `raw` first spells `name` as a whole word, in bytes.
fn word_at(raw: &str, name: &str) -> Option<usize> {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    raw.match_indices(name).map(|(at, _)| at).find(|&at| {
        !word(raw[..at].chars().next_back()) && !word(raw[at + name.len()..].chars().next())
    })
}

/// One way of showing the file on the sheet, of the ones that exclude each
/// other (see [`App::one_file_view`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileView {
    Skim,
    Diff,
    TimeTravel,
}
