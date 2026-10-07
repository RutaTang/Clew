//! Walkthroughs: generating a narrated tour for a scope or for the current
//! diff, the per-project library (save, delete, regenerate), and reading a tour
//! step by step.
//!
//! Its messages, [`WalkMsg`], arrive through `App::update_walk`.

use crate::app::prelude::*;
use crate::*;

/// The scope a change-review walkthrough is marked busy under until its review
/// base is resolved (inside the task); the finished tour is keyed
/// `@diff <label>`.
pub(crate) const DIFF_SCOPE_PENDING: &str = "@diff changes";

impl App {
    // ---- Walkthrough-domain handlers (extracted from `update`) ----------------

    /// Generate a scoped AI walkthrough (guided reading tour) of the project.
    pub(crate) fn on_generate_walkthrough(&mut self, scope: String) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        let context = self.gather_walkthrough_context();
        let overview = self.proj.overview.markdown.clone();
        let scope = scope.trim().to_string();
        let scope_opt = (!scope.is_empty()).then(|| scope.clone());
        let prompt = walkthrough::prompt(
            &project_name,
            overview.as_deref(),
            &context,
            scope_opt.as_deref(),
        );
        let seq = self.begin_walkthrough_generation(scope.clone());
        self.status = "Generating walkthrough…".into();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            async move {
                let resp = ai.complete(cfg, walkthrough::SYSTEM, prompt, 4096).await;
                resp.and_then(|r| walkthrough::parse(&r))
            },
            move |result| {
                Message::Walk(WalkMsg::Done {
                    stamp: stamp.clone(),
                    seq,
                    scope: scope.clone(),
                    result,
                })
            },
        )
    }

    /// A walkthrough of the last debug run, from its trace
    /// ([`walkthrough::from_trace`]): the plain tour is stored at once when
    /// no model is configured, and narrated by the model when one is — with
    /// the plain tour as the fallback, so the run is never lost to a model
    /// that would not answer.
    pub(crate) fn on_generate_trace_walkthrough(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let program = self
            .debug
            .trace_program
            .clone()
            .unwrap_or_else(|| "program".to_string());
        let Some(plain) = walkthrough::from_trace(&root, &program, &self.debug.trace) else {
            self.status = "The last run stopped nowhere in this project".into();
            return Task::none();
        };
        let scope = format!("@trace {program}");
        let seq = self.begin_walkthrough_generation(scope.clone());
        let Some(cfg) = self.llm_config() else {
            return self.on_walkthrough_done(seq, scope, Ok(plain));
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        // What clew already knows about the functions on the path.
        let mut summaries = String::new();
        for step in &plain.steps {
            let Some(symbol) = &step.symbol else { continue };
            let node = explain::Node::Function {
                file: root.join(&step.file),
                name: symbol.clone(),
                ordinal: 0,
            };
            if let Some(cached) = self.proj.explain.cache.get(&node) {
                summaries.push_str(&format!("{} ({}): {}\n", symbol, step.file, cached.summary));
            }
        }
        let prompt = walkthrough::trace_prompt(&project_name, &plain, &summaries);
        self.status = "Narrating the run…".into();
        let ai = self.ai_client();
        let stamp = self.stamp();
        Task::perform(
            async move {
                let narrated = ai
                    .complete(cfg, walkthrough::TRACE_SYSTEM, prompt, 4096)
                    .await
                    .and_then(|r| walkthrough::parse(&r));
                // The model's titles and narration on the run's own anchors
                // — a step's file, symbol and line are what happened, not
                // the model's to change — when it kept every step; else the
                // plain tour, which is the run as it happened.
                match narrated {
                    Ok(mut wt) if wt.steps.len() == plain.steps.len() => {
                        for (step, own) in wt.steps.iter_mut().zip(&plain.steps) {
                            step.file = own.file.clone();
                            step.symbol = own.symbol.clone();
                            step.line = own.line;
                        }
                        Ok(wt)
                    }
                    _ => Ok(plain),
                }
            },
            move |result| {
                Message::Walk(WalkMsg::Done {
                    stamp: stamp.clone(),
                    seq,
                    scope: scope.clone(),
                    result,
                })
            },
        )
    }

    /// Mark a walkthrough generation for `scope` as the one in flight and mint
    /// its request id: only that request's `WalkMsg::Done` may clear the busy
    /// row, retry, or open the tour.
    fn begin_walkthrough_generation(&mut self, scope: String) -> u64 {
        self.proj.walk.seq += 1;
        self.proj.walk.pending = Some(self.proj.walk.seq);
        self.proj.walk.generating = Some(scope);
        self.proj.walk.seq
    }

    /// Generate a "review my changes" walkthrough from the diff vs the review base.
    ///
    /// Everything git is asked — the review base included — runs inside the
    /// task, for a local repository exactly as for a remote one (`GitSource`):
    /// resolving the base spawns up to six git processes, which used to run on
    /// the UI thread for a local project. A git failure ends the generation
    /// with that error; it never reaches the (paid) prompt as an empty diff.
    pub(crate) fn on_generate_diff_walkthrough(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        let Some(git) = self.git_source() else {
            return Task::none();
        };
        let project_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string();
        // Symbols per file for the changed-file annotations (in memory — fed
        // by the server's snapshot on a remote project).
        let symbols_by_file = self.proj.symbol_index_by_file.clone();
        // The library row shown busy until the base (and so the tour's real
        // scope, `@diff <label>`) is known; the result names its own scope.
        let seq = self.begin_walkthrough_generation(DIFF_SCOPE_PENDING.to_string());
        self.status = "Reviewing changes…".into();
        let ai = self.ai_client();
        let task_root = root;
        let stamp = self.stamp();
        Task::perform(
            async move {
                let root = task_root;
                // A sentinel scope so the library shows it as a change review
                // and Regenerate re-runs the diff (not a normal scoped tour).
                let mut scope = DIFF_SCOPE_PENDING.to_string();
                let result = async {
                    let (base, label) = git
                        .run::<Option<(String, String)>>(clew_protocol::GitOp::ReviewBase)
                        .await?
                        .ok_or_else(|| {
                            "Nothing to review (need a branch vs main/master, or a prior commit)"
                                .to_string()
                        })?;
                    scope = format!("@diff {label}");
                    let commits = git
                        .run::<Vec<String>>(clew_protocol::GitOp::CommitSubjects {
                            base: base.clone(),
                        })
                        .await?;
                    let changed = git
                        .run::<Vec<(String, char)>>(clew_protocol::GitOp::ChangedFiles {
                            base: base.clone(),
                        })
                        .await?;
                    let patch = git
                        .run::<String>(clew_protocol::GitOp::RangePatch {
                            base: base.clone(),
                            max_bytes: 12000,
                        })
                        .await?;
                    let mut changed_text = String::new();
                    for (rel, ch) in changed {
                        changed_text.push_str(&format!("{ch} {rel}\n"));
                        if let Some(syms) = symbols_by_file.get(&root.join(&rel)) {
                            for s in syms.iter().filter(|s| {
                                matches!(
                                    s.kind.as_str(),
                                    "function" | "method" | "struct" | "class" | "enum" | "trait"
                                )
                            }) {
                                changed_text.push_str(&format!(
                                    "    {} {} @ L{}\n",
                                    s.kind, s.name, s.line
                                ));
                            }
                        }
                    }
                    // Read: the share goes now, not when the model has
                    // answered — a watcher batch landing while it is held
                    // copies the window's map before writing it.
                    drop(symbols_by_file);
                    let prompt = walkthrough::diff_prompt(
                        &project_name,
                        &label,
                        &commits,
                        &changed_text,
                        &patch,
                    );
                    let resp = ai
                        .complete(cfg, walkthrough::DIFF_SYSTEM, prompt, 4096)
                        .await?;
                    walkthrough::parse(&resp)
                }
                .await;
                (scope, result)
            },
            move |(scope, result)| {
                Message::Walk(WalkMsg::Done {
                    stamp: stamp.clone(),
                    seq,
                    scope,
                    result,
                })
            },
        )
    }

    /// Fold a finished walkthrough into the library and open it (retry once on a
    /// malformed-JSON parse error).
    pub(crate) fn on_walkthrough_done(
        &mut self,
        seq: u64,
        scope: String,
        result: Result<walkthrough::Walkthrough, String>,
    ) -> Task<Message> {
        // Supersession is decided BEFORE any flag is touched. A generation the
        // user has since replaced must not clear the busy row of the one now
        // running, nor spend its retry, nor pull the reader into its tour; a
        // finished one is still saved (it was paid for), just not opened.
        let awaited = self.proj.walk.pending == Some(seq);
        if awaited {
            self.proj.walk.pending = None;
            self.proj.walk.generating = None;
        } else if result.is_err() {
            return Task::none();
        }
        match result {
            Ok(mut wt) => {
                // Drop steps that don't resolve to a real project file.
                wt.steps
                    .retain(|s| self.resolve_walk_file(&s.file).is_some());
                if wt.steps.is_empty() {
                    if awaited {
                        self.status = "Walkthrough had no valid steps".into();
                    }
                    return Task::none();
                }
                wt.scope = scope.clone();
                // Upsert by scope: regenerating a tour replaces it in place, a
                // fresh scope is appended.
                let stored = wt.clone();
                match self.proj.walk.library.iter().position(|w| w.scope == scope) {
                    Some(i) => self.proj.walk.library[i] = wt,
                    None => self.proj.walk.library.push(wt),
                }
                let saved = if self.local_project_state()
                    && let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone())
                {
                    // Upserted into what is on disk RIGHT NOW, by scope: writing
                    // this window's whole library back erased every tour a
                    // second window on the same project had generated since it
                    // loaded (the same merge `on_walkthrough_delete` makes). The
                    // tour is already in this window's library, so a failed write
                    // still leaves it on screen — only unsaved.
                    let (merged, saved) =
                        walkthrough::edit_library(&root, &self.proj.walk.library, |lib| match lib
                            .iter()
                            .position(|w| w.scope == scope)
                        {
                            Some(i) => lib[i] = stored,
                            None => lib.push(stored),
                        });
                    // The merged library carries the new tour whether or not
                    // the write landed, so adopting it here is what keeps the
                    // generated tour on screen — the promise the comment above
                    // makes. Dropping it would throw away a tour that cost a
                    // full LLM pass.
                    self.proj.walk.library = merged;
                    // The open tour is held by scope, so the merged order
                    // cannot move the reader; one another window deleted
                    // meanwhile is simply no longer open.
                    self.proj.walk.forget_vanished_tour();
                    self.touch_project_state(walkthrough::LIBRARY_REL);
                    match saved {
                        Ok(()) => true,
                        Err(e) => {
                            self.status =
                                format!("Could not save walkthrough: {e} — shown but not saved");
                            false
                        }
                    }
                } else {
                    // Remotely the SAME upsert-by-scope, applied to the file
                    // by the server: shipping this window's whole library
                    // erased every tour another client had generated since.
                    // Not taken, it stays on screen, unsaved, and the status
                    // line says why.
                    self.save_walkthrough_scope(&scope, Some(&stored))
                };
                if !awaited {
                    // The reader has moved on to another tour: do not pull
                    // them into this one — and do not cover up that it was
                    // not saved.
                    if saved {
                        self.status = format!("Walkthrough “{scope}” finished in the background");
                    }
                    return Task::none();
                }
                // By scope: the merge re-reads a library another window may
                // have appended to, which moves every index.
                self.proj.walk.open = Some(scope);
                self.proj.walk.step = 0;
                self.sidebar = SidebarTab::Walk;
                self.show_left_sidebar = true;
                self.proj.walk.retried = false;
                // No second save here. The branch above already persisted, and
                // locally it did so by merging under the save lock; following
                // that with a second `save_walkthrough_scope()` writes the same
                // bytes again but outside the lock, reopening the unlocked
                // whole-file window `edit_library` exists to close. Nothing
                // between there and here touches the library — `open`, `step`
                // and `retried` are per-window view state.
                self.walkthrough_goto(0)
            }
            Err(e) => {
                // The model occasionally returns malformed JSON — retry the
                // generation once before surfacing the failure.
                if e.starts_with("parse") && !self.proj.walk.retried {
                    self.proj.walk.retried = true;
                    self.status = "Retrying walkthrough…".into();
                    return if scope.starts_with("@diff") {
                        Task::done(Message::Walk(WalkMsg::GenerateDiff))
                    } else {
                        Task::done(Message::Walk(WalkMsg::Generate(scope)))
                    };
                }
                self.proj.walk.retried = false;
                self.status = format!("Walkthrough failed: {e}");
                Task::none()
            }
        }
    }

    /// Persist ONE tour change of a REMOTE project's library, by scope:
    /// `Some(tour)` upserts it, `None` deletes it.
    ///
    /// Scope, not index, and one tour, not the library: writing this window's
    /// whole library back erased every tour another client had generated
    /// since it loaded. The local arms of both mutation paths (generate at
    /// `on_walkthrough_done`, delete at `on_walkthrough_delete`) merge through
    /// `walkthrough::edit_library`; this is the same merge for a REMOTE
    /// project, performed at the server. Remote only: a local caller must go
    /// through `edit_library`, or it reintroduces the lost update.
    ///
    /// Returns whether the change is on its way; when it is not, the status
    /// line says why (the journal refused it, or the tour cannot be
    /// serialized) — except with no remote project, where there is nothing
    /// to save it to.
    #[must_use = "a change that was not taken must not be applied to this window's library"]
    pub(crate) fn save_walkthrough_scope(
        &mut self,
        scope: &str,
        tour: Option<&walkthrough::Walkthrough>,
    ) -> bool {
        // With no project open there is nothing to save anywhere, and this
        // would otherwise write the window's leftover tour into whatever
        // project the server has next. A local project never reaches here.
        if self.proj.project.is_none() || self.local_project_state() {
            return false;
        }
        let merge = match tour {
            Some(tour) => walkthrough::merge_upsert(tour),
            None => Some(walkthrough::merge_remove(scope)),
        };
        match merge {
            Some(merge) => self.edit_remote_state(walkthrough::LIBRARY_REL, merge),
            // The tour could not be serialized, so there is nothing to send.
            // Saying so beats a silent no-op the user reads as saved.
            None => {
                self.status = "Could not save walkthrough: it is not serializable".into();
                false
            }
        }
    }

    pub(crate) fn on_walkthrough_delete(&mut self, scope: &str) -> Task<Message> {
        let Some(i) = self.proj.walk.library.iter().position(|w| w.scope == scope) else {
            return Task::none();
        };
        let gone = scope.to_string();
        if self.local_project_state()
            && let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone())
        {
            self.proj.walk.library.remove(i);
            // Delete by scope (the key tours are upserted on) against what is
            // on disk now: writing this window's whole library back erased
            // every tour a second window had generated since it loaded.
            let (merged, saved) =
                walkthrough::edit_library(&root, &self.proj.walk.library, |lib| {
                    lib.retain(|w| w.scope != gone);
                });
            // Adopted on failure too, so the deletion the user asked for holds
            // for the session instead of the tour reappearing in the sidebar.
            self.proj.walk.library = merged;
            self.touch_project_state(walkthrough::LIBRARY_REL);
            if let Err(e) = saved {
                self.status = format!("Could not save walkthrough: {e} — removed for this session");
            }
        } else if self.save_walkthrough_scope(&gone, None) {
            // Remotely: removed here once the change is on its way. Not
            // taken, the tour stays, and the status line says why.
            self.proj.walk.library.remove(i);
        }
        // The open tour is held by scope: deleting it closes it, deleting
        // another (or the merge reordering the rest) leaves it open.
        self.proj.walk.forget_vanished_tour();
        Task::none()
    }

    pub(crate) fn on_walkthrough_regenerate(&mut self, scope: String) -> Task<Message> {
        if !self.proj.walk.library.iter().any(|w| w.scope == scope) {
            return Task::none();
        }
        // A change-review tour re-runs the diff; a normal tour re-generates.
        if scope.starts_with("@diff") {
            return Task::done(Message::Walk(WalkMsg::GenerateDiff));
        }
        Task::done(Message::Walk(WalkMsg::Generate(scope)))
    }

    pub(crate) fn on_walkthrough_step(&mut self, delta: i32) -> Task<Message> {
        let n = self
            .proj
            .walk
            .open_tour()
            .map(|w| w.steps.len())
            .unwrap_or(0);
        if n == 0 {
            return Task::none();
        }
        let i = (self.proj.walk.step as i32 + delta).clamp(0, n as i32 - 1) as usize;
        self.walkthrough_goto(i)
    }

    /// Navigate to walkthrough step `i`: open its file and jump to the step's
    /// symbol, or its own line when the symbol is not there.
    ///
    /// Resolved against the file's OWN symbols — a pane already showing it
    /// (parsed from the text on screen), else, once the opened file's symbols
    /// land, through `settle_walk_anchor`; the project index only picks where
    /// to open meanwhile, since it may not have reached the file yet (still
    /// building) or skipped it (over a cap), and would call its symbol
    /// missing. Where the step did NOT land where it meant to is kept with the
    /// step (`WalkState::anchor_note`, shown in the WALK panel) as well as
    /// said in the status line — which the file load the step starts
    /// overwrites.
    pub(crate) fn walkthrough_goto(&mut self, i: usize) -> Task<Message> {
        let Some(step) = self
            .proj
            .walk
            .open_tour()
            .and_then(|w| w.steps.get(i))
            .cloned()
        else {
            return Task::none();
        };
        self.proj.walk.step = i;
        self.proj.walk.anchor_note = None;
        self.proj.walk.pending_anchor = None;
        // Prepare the narration (markdown + any mermaid/math → SVG).
        let (prepared, render) = self.prepare_segments(&step.narration);
        self.proj.walk.prepared = prepared;
        let Some(abs) = self.resolve_walk_file(&step.file) else {
            self.set_walk_anchor_note(Some(format!(
                "Walkthrough step {}: {} is not in this project",
                i + 1,
                step.file
            )));
            return render;
        };
        let shown = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == abs)
            .and_then(|v| walkthrough::resolve_in_viewer(&step, v));
        let (line, pending) = match shown {
            Some(anchor) => {
                self.set_walk_anchor_note(anchor.note(i + 1, &step));
                (anchor.line(), false)
            }
            None => {
                let line = walkthrough::provisional_line(
                    &step,
                    self.proj
                        .symbol_index_by_file
                        .get(&abs)
                        .map(|symbols| symbols.iter().map(|s| (s.name.as_str(), s.line))),
                );
                (line, true)
            }
        };
        let pane = self.proj.active;
        let open = self.open_file(abs.clone(), Some(line), true);
        if pending {
            // Settled once the opened file's own symbols land — for THIS
            // load (see `PendingAnchor`).
            self.proj.walk.pending_anchor = Some(PendingAnchor {
                abs,
                step: i,
                pane,
                load: self.proj.link.pane_pending[pane],
                superseded: false,
            });
        }
        Task::batch([open, render])
    }

    /// Keep `note` with the open step (the WALK panel shows it under the step)
    /// and say it in the status line; `None` clears the step's note.
    fn set_walk_anchor_note(&mut self, note: Option<String>) {
        if let Some(note) = &note {
            self.status = note.clone();
        }
        self.proj.walk.anchor_note = note;
    }

    /// Settle a walkthrough step that was opened before its file's own
    /// symbols were known (`walkthrough_goto`): once a pane shows that file
    /// with its symbols, resolve the step against them, keep (and say) where
    /// it landed, and — when the reader is still on that file in the active
    /// pane and the answer differs from the provisional line — jump there.
    ///
    /// The reader's own moves win over the jump. One made in that file
    /// meanwhile superseded the step (`reader_moved_caret`, `open_file`):
    /// nothing moves, and the status line — which would read as news about
    /// where the reader is — is left alone; the note is still kept with the
    /// step, since what the step points at is the step's to say. Nor does a
    /// step jump into a time-travel session: the jump is `open_file`, which
    /// ends one — nor where it would give up a time-travel start still
    /// loading.
    ///
    /// Run after every update (`App::update`), like the other re-derivations:
    /// symbols land through more than one path (a local highlight pass, a
    /// server's `FileContent`, a notebook's cells), and a hook at each of
    /// them is one easy to forget. Nothing to do while no step waits.
    pub(crate) fn settle_walk_anchor(&mut self) -> Option<Task<Message>> {
        let PendingAnchor {
            abs,
            step: i,
            pane,
            load,
            superseded,
        } = self.proj.walk.pending_anchor.clone()?;
        let step = self
            .proj
            .walk
            .open_tour()
            .and_then(|w| w.steps.get(i))
            .cloned();
        let Some(step) = step.filter(|_| self.proj.walk.step == i) else {
            // The reader moved to another step, or closed the tour.
            self.proj.walk.pending_anchor = None;
            return None;
        };
        // The load of its file it waits for still in flight — its own, or a
        // jump's into that file (`open_file`) — or held back by a time-travel
        // start in its pane, to be carried out should that history bring no
        // session (`SupersededOpen`, which rebinds the step to the load it
        // then issues): wait for it. Taken for gone, the step was dropped as
        // the start took the load off the pane, and the carried-out open
        // landed with no step to settle. Held only while that start is still
        // loading (`ProjectSession::time_start`): an open whose start is gone
        // is never carried out by it, and the step would wait on it for good.
        // (Defence in depth: every end of a start takes what it held back
        // along today — `give_up_time_travel_start`, its history landing,
        // the split closing, the project going.)
        let link = &self.proj.link;
        let loading = self.proj.time_start.as_ref().map(|start| start.generation);
        let held = link.superseded_open.as_ref().is_some_and(|s| {
            s.pane == pane && Some(s.open.req) == load && Some(s.generation) == loading
        });
        if load.is_some() && (link.pane_pending.get(pane).copied().flatten() == load || held) {
            return None;
        }
        // Landed, failed or superseded — and the pane shows another file: the
        // reader moved on, and a later, unrelated opening of the step's file
        // must not have its cursor moved by a step the reader left.
        let Some(viewer) = self
            .proj
            .panes
            .get(pane)
            .and_then(Option::as_ref)
            .filter(|v| v.abs == abs)
        else {
            self.proj.walk.pending_anchor = None;
            return None;
        };
        // `None` until the file's own symbols have landed.
        let anchor = walkthrough::resolve_in_viewer(&step, viewer)?;
        self.proj.walk.pending_anchor = None;
        let note = anchor.note(i + 1, &step);
        if superseded {
            self.proj.walk.anchor_note = note;
            return None;
        }
        self.set_walk_anchor_note(note);
        // A time-travel session belongs to the active pane — the only one
        // this jumps in — and `open_file` ends it, whichever file it is on.
        // Nor does the step jump where that would give up a time-travel
        // start still loading (`jump_gives_up_time_start`): the start is the
        // reader's own request, and newer — a step opened after it gave it
        // up with its own jump. None can be pending there today: a start in
        // this pane holds it on the start's file until it ends, and one on
        // the step's file superseded the step (`on_time_travel_start`); a
        // re-scope comes with a session. Should that change, the step still
        // stays off it, as it stays off a session. A start in the other pane,
        // on another file, is left be by the jump, which happens.
        if self.proj.time_travel.is_some() || self.jump_gives_up_time_start(self.proj.active, &abs)
        {
            return None;
        }
        let line = anchor.line();
        match self.active_viewer() {
            Some(v) if v.abs == abs && v.target_line != Some(line) => {
                Some(self.open_file(abs, Some(line), false))
            }
            _ => None,
        }
    }

    /// Context for the walkthrough planner: the structure + summaries (reused
    /// from the overview inputs) plus the real symbols per file, which the tour
    /// must anchor to (so it can't invent locations).
    pub(crate) fn gather_walkthrough_context(&self) -> String {
        let inputs = self.gather_overview_inputs();
        let mut c = String::new();
        c.push_str("Structure (files, each with a short summary of its role):\n");
        c.push_str(&inputs.structure);
        if !inputs.entry_points.is_empty() {
            c.push_str("\nEntry points:\n");
            for e in &inputs.entry_points {
                c.push_str(&format!("- {e}\n"));
            }
        }
        c.push_str("\nSymbols per file — anchor steps to these exact paths and names:\n");
        let mut by_file: Vec<&PathBuf> = self.proj.symbol_index_by_file.keys().collect();
        by_file.sort_by_key(|p| self.rel_of(p));
        for abs in by_file {
            let names: Vec<&str> = self.proj.symbol_index_by_file[abs]
                .iter()
                .filter(|s| {
                    matches!(
                        s.kind.as_str(),
                        "function" | "method" | "struct" | "enum" | "class" | "trait" | "interface"
                    )
                })
                .map(|s| s.name.as_str())
                .take(40)
                .collect();
            if !names.is_empty() {
                c.push_str(&format!("{}: {}\n", self.rel_of(abs), names.join(", ")));
            }
        }
        c
    }

    /// Resolve a walkthrough step's relative path to an absolute project file.
    pub(crate) fn resolve_walk_file(&self, rel: &str) -> Option<PathBuf> {
        let rel = rel.trim().trim_start_matches("./");
        self.proj
            .project
            .as_ref()?
            .files
            .iter()
            .find(|f| self.rel_of(&f.abs) == rel)
            .map(|f| f.abs.clone())
    }
}

impl App {
    /// Handle a [`WalkMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_walk(&mut self, message: WalkMsg) -> Task<Message> {
        match message {
            WalkMsg::Generate(scope) => self.on_generate_walkthrough(scope),
            WalkMsg::GenerateDiff => self.on_generate_diff_walkthrough(),
            WalkMsg::GenerateTrace => self.on_generate_trace_walkthrough(),
            WalkMsg::Regenerate(scope) => self.on_walkthrough_regenerate(scope),
            WalkMsg::Delete(scope) => self.on_walkthrough_delete(&scope),
            // (A tour generated for another project must not be saved into
            // this one's library — the save also writes to its disk: dropped
            // in `dispatch`.)
            WalkMsg::Done {
                seq, scope, result, ..
            } => self.on_walkthrough_done(seq, scope, result),
            WalkMsg::Open(scope) => {
                if !self.proj.walk.library.iter().any(|w| w.scope == scope) {
                    return Task::none();
                }
                self.proj.walk.open = Some(scope);
                self.proj.walk.step = 0;
                self.walkthrough_goto(0)
            }
            WalkMsg::Back => {
                self.proj.walk.open = None;
                self.proj.walk.forget_vanished_tour();
                Task::none()
            }
            WalkMsg::ToggleMode => {
                self.walk_ui.mode = match self.walk_ui.mode {
                    WalkMode::Search => WalkMode::Walk,
                    WalkMode::Walk => WalkMode::Search,
                };
                Task::none()
            }
            WalkMsg::Goto { scope, step } => {
                // A step index belongs to the tour it was drawn from.
                if self.proj.walk.open.as_deref() != Some(scope.as_str()) {
                    return Task::none();
                }
                self.walkthrough_goto(step)
            }
            WalkMsg::Step(delta) => self.on_walkthrough_step(delta),
            WalkMsg::InputChanged(s) => {
                self.walk_ui.input = s;
                Task::none()
            }
            WalkMsg::ResizeNarration(y) => {
                // Narration height = distance from the drag point to the window
                // bottom, clamped so neither block collapses.
                let max = (self.window_height - 160.0).max(120.0);
                self.walk_ui.narration_height = (self.window_height - y).clamp(90.0, max);
                Task::none()
            }
        }
    }
}
