//! Explanations: the bottom-up explain pass (start, progress, cancel, finish,
//! persist), per-function block walkthroughs, the explanation overlay, and the
//! auto-refresh cooldown that re-runs the pass as files change.
//!
//! Its messages, [`ExplainMsg`], arrive through `App::update_explain`.

use crate::app::prelude::*;
use crate::*;

/// Which pass to start (see `explain::Reuse`).
enum PassKind {
    /// Explain All / Refresh All: every node whose prompt changed.
    Explicit,
    /// The refresh a source change started: code that changed.
    Automatic,
    /// A re-explain: this node, and what quotes it.
    Node(explain::Node),
}

impl App {
    /// Explain the whole project (bottom-up LLM pass), abortable from the UI.
    /// `automatic`: the refresh a source change started, which pays for code
    /// that changed and what quotes it, never for a new prompt wording (see
    /// `explain::Reuse::ChangedSources`); an explicit pass pays for every
    /// node whose prompt changed.
    pub(crate) fn on_explain_project(&mut self, automatic: bool) -> Task<Message> {
        self.start_explain_pass(if automatic {
            PassKind::Automatic
        } else {
            PassKind::Explicit
        })
    }

    fn start_explain_pass(&mut self, kind: PassKind) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        // One pass at a time. A second pass would bill every changed node
        // again beside the first, from a baseline that lacks what the first is
        // paying for — and overwriting `explain.abort` below would orphan the
        // first (an iced abort handle does not abort on drop), leaving it
        // running with nothing able to stop it. Every user path that can meet
        // a running pass — the chord, the menu item, the ⋯ row — cancels it
        // instead (`run_command_action`, the toolbar's Cancel row); what still
        // gets here is a refresh or a re-explain racing it, which the running
        // pass covers.
        if self.proj.explain.running {
            self.status = "Explain All is already running".into();
            return Task::none();
        }
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        let Some(project) = &self.proj.project else {
            return Task::none();
        };
        let root = project.root.clone();
        // The file list is shared, not walked here: listing its paths, and
        // picking out the files a pass explains, is work per file, done below
        // off the thread that serves every window.
        let files = project.files.clone();
        let remote = !self.local_project_state();
        // The pass's reuse baseline. Handed over as a shared pointer and
        // copied on the blocking pool below: a project's cache runs to
        // thousands of entries, and deep-copying it here stalled every window.
        let prev = self.proj.explain.cache.clone();
        // Which recorded summaries the pass may keep. The files the watcher
        // reported tell the automatic pass where code without a summary was
        // just written; an explicit pass explains all such code, so it takes
        // them too. Either keeps a copy, handed back if it does not complete.
        // A re-explain leaves them to the next refresh.
        let reported = match kind {
            PassKind::Node(_) => HashSet::new(),
            _ => std::mem::take(&mut self.proj.explain.changed_sources),
        };
        self.proj.explain.pass_sources = reported.clone();
        self.proj.explain.pass_automatic = matches!(kind, PassKind::Automatic);
        self.proj.explain.pass_target = None;
        let reuse = match kind {
            PassKind::Explicit => explain::Reuse::SamePrompt,
            PassKind::Automatic => explain::Reuse::ChangedSources(reported),
            PassKind::Node(node) => {
                self.proj.explain.pass_target = Some(node.clone());
                explain::Reuse::Node(node)
            }
        };
        // What waited in a pass this session waits no more (`explain::Pass`).
        let waited = self.proj.explain.waited.clone();
        let ai = self.ai_client();
        self.proj.explain.generation += 1;
        let generation = self.proj.explain.generation;
        let stamp = self.stamp();
        self.proj.explain.running = true;
        self.proj.explain.progress = Some((0, 0));
        self.proj.explain.failed = 0;
        self.status = "Explaining project…".into();
        let stream = iced::stream::channel(256, move |output| {
            let gather_root = root.clone();
            let fetch_ai = ai.clone();
            async move {
                let listed: Vec<PathBuf> = files.iter().map(|f| f.abs.clone()).collect();
                // For a remote project the sources come over the protocol —
                // the abs paths are identities only, never read from this
                // disk — and the same (pure) gather runs over them. Only the
                // files a pass explains are asked for: any other would come
                // back as one the host could not read.
                let fetched = if remote {
                    let rels: Vec<String> = files
                        .iter()
                        .filter(|f| highlight::detect(&f.abs).is_some())
                        .map(|f| f.rel.clone())
                        .collect();
                    Some(fetch_explain_sources(&fetch_ai, &gather_root, &rels).await)
                } else {
                    None
                };
                // The baseline is copied here, off the UI thread; the gather
                // reads it too, for what a file it could not read defines.
                let gathered = tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    gather_faults::hit(&gather_root);
                    let prev: explain::Cache = (*prev).clone();
                    let inputs = match fetched {
                        Some(mut sources) => {
                            sources.listed = listed;
                            Ok(gather_explain_inputs_from(sources, gather_root, &prev))
                        }
                        None => gather_explain_inputs(listed, gather_root, &prev),
                    };
                    inputs.map(|inputs| (inputs, prev))
                })
                .await
                .unwrap_or_else(|_| {
                    Err(ExplainFailure::Internal("while reading the project".into()))
                });
                explain_stream(output, gathered, reuse, waited, cfg, ai, stamp, generation).await;
            }
        });
        // Abortable so a long project pass (thousands of LLM calls on a big repo)
        // can be cancelled from the UI; the handle is dropped when the pass
        // finishes (ExplainMsg::Done) or is cancelled. A handle still held here
        // (none should be: no pass is running) is aborted, never just dropped.
        let (task, handle) = Task::run(stream, |m| m).abortable();
        if let Some(previous) = self.proj.explain.abort.replace(handle) {
            previous.abort();
        }
        task
    }

    /// Cancel the running explain pass (abort remaining calls, keep + save work).
    pub(crate) fn on_cancel_explain(&mut self) -> Task<Message> {
        // Stop the in-flight pass: abort the task (halts further LLM calls) and
        // bump the generation so any already-queued progress messages are
        // ignored. Cached explanations so far are kept.
        if let Some(handle) = self.proj.explain.abort.take() {
            handle.abort();
        }
        self.proj.explain.generation += 1;
        self.proj.explain.running = false;
        self.proj.explain.progress = None;
        self.return_pass_sources();
        self.proj.explain.pass_automatic = false;
        self.proj.explain.pass_target = None;
        self.status = "Explain cancelled".into();
        // Nothing the pass paid for reaches this window (its progress carries
        // no summaries), so this saves only what this window changed before —
        // merged into what is on disk, never written over it: the derived
        // store is shared by every window and every clew process on the
        // project, and cancelling here used to replace fifty summaries
        // another window had just paid for with this window's copy.
        self.persist_explain()
    }

    /// A pass that will not complete leaves the files it was started for to
    /// the next one: code written there may still have no summary, and the
    /// next automatic pass explains such code only where it is told to.
    fn return_pass_sources(&mut self) {
        let sources = std::mem::take(&mut self.proj.explain.pass_sources);
        self.proj.explain.changed_sources.extend(sources);
    }

    /// Note what a pass changed (`written`, each with what it replaced)
    /// among the changes waiting to be saved. One already waiting keeps its
    /// base: that names what the store holds, which the pass's base — taken
    /// from this window's copy since — need not. Folded in the smaller of the
    /// two maps: after an Explain All `written` is the whole project, and
    /// adding it entry by entry to an empty map was that much work on the
    /// thread that serves every window.
    fn note_written(&mut self, written: explain::Unsaved) {
        fold_changes(&mut self.proj.explain.unsaved, written);
    }

    /// Replace the in-memory explanation cache (a new identity for
    /// `ExplainMsg::Persisted`'s staleness check).
    pub(crate) fn set_explain_cache(&mut self, cache: explain::Cache) {
        self.proj.explain.cache = cache.into();
        self.proj.explain.cache_seq += 1;
    }

    /// The in-memory explanation cache, for a change (copy-on-write when a
    /// background merge still holds the previous version). A change made
    /// here is NOT saved: a save writes the entries in
    /// `ExplainState::unsaved`, and only those.
    pub(crate) fn explain_cache_mut(&mut self) -> &mut explain::Cache {
        self.proj.explain.cache_seq += 1;
        &mut self.proj.explain.cache
    }

    /// Save this window's changes — the entries in `ExplainState::unsaved`,
    /// and nothing else (`explain::merge_unsaved`) — into the project's
    /// shared derived store, on the blocking pool (the read-modify-write
    /// re-reads and re-serializes the whole store, which is megabytes on a
    /// big project). What is stored afterwards comes back as
    /// `ExplainMsg::Persisted`.
    ///
    /// One save at a time. Two out at once could each be adopted over the
    /// other's work, or hand back a base the other had just overtaken: memory
    /// went back to a stored summary the window had paid to replace, and
    /// later saves wrote the old one back. Asked for while one is out, a save
    /// joins it: the task running that one runs it next, with every change
    /// asked for since ([`run_saves`]). The queue is the task's, not the
    /// window's — a flag in the window was lost with it when the window
    /// closed, or its project or connection switched, while a save was out,
    /// and what it was to save with it.
    pub(crate) fn persist_explain(&mut self) -> Task<Message> {
        use crate::app::state::{QueuedSave, SaveChain};
        let (Some(store), Some(root)) = (
            self.proj.derived_dir.clone(),
            self.proj.project.as_ref().map(|p| p.root.clone()),
        ) else {
            // Nowhere to save them: they live as long as the session.
            self.proj.explain.unsaved.clear();
            return Task::none();
        };
        // Moved, never copied: the changes run to the whole project after an
        // Explain All, and a copy of them was made on the thread that serves
        // every window.
        let mut changes = std::mem::take(&mut self.proj.explain.unsaved);
        let mine = self.proj.explain.cache.clone();
        let seq = self.proj.explain.cache_seq;
        if let Some(chain) = &self.proj.explain.save {
            let mut queue = chain.lock();
            if queue.open {
                match &mut queue.next {
                    Some(next) => {
                        fold_changes(&mut next.changes, changes);
                        next.mine = mine;
                        next.seq = seq;
                    }
                    None => {
                        queue.next = Some(QueuedSave { changes, mine, seq });
                    }
                }
                return Task::none();
            }
            // Its task is done, and its end has not landed: what it could
            // not save goes with this one, each against what it replaced then.
            let mut left = std::mem::take(&mut queue.left);
            fold_changes(&mut left, changes);
            changes = left;
        }
        let chain = SaveChain::open();
        self.proj.explain.save = Some(chain.clone());
        let stamp = self.stamp();
        Task::perform(
            run_saves(
                store,
                root,
                QueuedSave { changes, mine, seq },
                chain.clone(),
            ),
            move |(seq, merged, saved)| {
                Message::Explain(ExplainMsg::Persisted {
                    stamp: stamp.clone(),
                    seq,
                    merged: Handoff::new(merged),
                    saved,
                    chain: chain.clone(),
                })
            },
        )
    }

    /// The saves of `chain` finished. The store as merged (this window's
    /// changes over every other window's summaries) is adopted only when the
    /// last of them saved, and only if the in-memory cache is still the one
    /// it was asked for from; a newer change stands, and is saved next. What
    /// the saves could not store is handed back to the next save — unless a
    /// save the window started since took it already.
    pub(crate) fn on_explain_persisted(
        &mut self,
        chain: crate::app::state::SaveChain,
        seq: u64,
        merged: Option<explain::Cache>,
        saved: Result<usize, String>,
    ) -> Task<Message> {
        if self
            .proj
            .explain
            .save
            .as_ref()
            .is_some_and(|current| current.is(&chain))
        {
            self.proj.explain.save = None;
            let left = std::mem::take(&mut chain.lock().left);
            // Each against what it replaced then — which the store still
            // holds, as nothing was written — over a later change's.
            let later = std::mem::take(&mut self.proj.explain.unsaved);
            let mut back = left;
            fold_changes(&mut back, later);
            self.proj.explain.unsaved = back;
        }
        self.adopt_save(seq, merged, saved);
        Task::none()
    }

    /// [`on_explain_persisted`](App::on_explain_persisted)'s outcome, once
    /// the window may save again.
    fn adopt_save(
        &mut self,
        seq: u64,
        merged: Option<explain::Cache>,
        saved: Result<usize, String>,
    ) {
        let left_out = match saved {
            Err(e) => {
                self.status = format!("Explanations kept for this session but not saved: {e}");
                // Nor is what came back adopted: it is not what the store
                // holds (from a store this build cannot read, it is this
                // window's changes alone).
                return;
            }
            Ok(left_out) => left_out,
        };
        // Said where the user sees it — it used to go to stderr, which a GUI
        // app's user never reads, while the next session re-billed the
        // summaries left out — but once a session, and beside whatever the
        // status says (a rejected key, say), never over it.
        if left_out > 0 && !self.proj.explain.cap_reported {
            self.proj.explain.cap_reported = true;
            let note = format!(
                "The explanation cache is over its size limit: {left_out} summaries are kept \
                 for this session but not saved (a later Explain All pays for them again)"
            );
            self.status = if self.status.is_empty() {
                note
            } else {
                format!("{} · {note}", self.status)
            };
        }
        if seq == self.proj.explain.cache_seq
            && let Some(merged) = merged
            && !merged.is_empty()
        {
            self.set_explain_cache(merged);
        }
    }

    /// Fold a finished project explain pass into state and fan out the downstream
    /// refresh (index / overview / open panel), reporting the outcome honestly.
    pub(crate) fn on_explain_done(
        &mut self,
        generation: u64,
        cache: explain::Cache,
        failures: PassFailures,
        tally: explain::Tally,
        written: explain::Unsaved,
    ) -> Task<Message> {
        // Only the latest pass (the project was checked in `dispatch`).
        if generation != self.proj.explain.generation {
            return Task::none();
        }
        self.proj.explain.running = false;
        self.proj.explain.progress = None;
        self.proj.explain.abort = None;
        self.proj.explain.failed = failures.failed;
        let automatic = std::mem::take(&mut self.proj.explain.pass_automatic);
        let reexplained = self.proj.explain.pass_target.take();
        // What the pass changed is this window's to save, and nothing else
        // (`explain::merge_unsaved`): every other stored entry is at least as
        // fresh as this window's copy — another window's Refresh All
        // included, which a save of the whole copy used to undo.
        //
        // A pass that RAN TO COMPLETION changed exactly `written`: summaries
        // it paid for, bases it stamped, and entries gone from the project.
        // What it could not explain keeps its entry (`explain::Pass`). Its
        // cache is the new truth, in memory at once and in the store once the
        // merge lands.
        //
        // A pass that STOPPED short — the provider rejected the key, its
        // calls were cancelled, it failed itself — holds only what it
        // reached. Only its new and re-stamped entries are taken, added to
        // this window's cache: replacing the cache with it would delete the
        // stored explanations of everything it never reached because a key
        // expired, thousands of billed calls lost to a recoverable error.
        let completed = failures.stopped.is_none();
        if completed {
            note_waited(
                &mut self.proj.explain.waited,
                written.keys(),
                &tally.waiting,
            );
            // A file the pass could not read was not explained, whatever it
            // holds now: what was recorded under it is kept, and the next
            // pass reads it again. Code there that has no summary is explained
            // by the next automatic pass wherever this one would have: in any
            // such file after Explain All, in one the watcher reported after a
            // refresh, in none after a re-explain. A file that cannot be
            // explained is not read again until it changes, which the watcher
            // reports.
            let explicit = !automatic && reexplained.is_none();
            for path in &tally.unread {
                if explicit || self.proj.explain.pass_sources.contains(path) {
                    self.proj.explain.changed_sources.insert(path.clone());
                }
            }
            self.proj.explain.pass_sources.clear();
            // What it could not explain is retried by the next automatic
            // pass, which explains code without a summary where it is told
            // to — whatever that code had recorded.
            self.proj
                .explain
                .changed_sources
                .extend(tally.retry.iter().cloned());
            self.set_explain_cache(cache);
            self.note_written(written);
        } else {
            self.return_pass_sources();
            let mut cache = cache;
            let mut taken = Vec::new();
            let memory = self.explain_cache_mut();
            for (node, base) in written {
                if let Some(entry) = cache.remove(&node) {
                    memory.insert(node.clone(), entry);
                    taken.push((node, base));
                }
            }
            let adopted = taken.iter().map(|(node, _)| node);
            note_waited(&mut self.proj.explain.waited, adopted, &tally.waiting);
            for (node, base) in taken {
                self.proj.explain.unsaved.entry(node).or_insert(base);
            }
        }
        let persist = self.persist_explain();
        // Report honestly: a pass that stopped short says why, in words that
        // tell the provider's refusal from its outage, the network, a
        // cancellation and clew's own failure; a partial run names how many
        // calls failed and why, and what it explained without them or left
        // waiting; only a clean pass claims unqualified success — and one
        // that kept or left summaries says which, and names the files it
        // could not read or can no longer explain.
        let n = self.proj.explain.cache.len();
        let mut status = if let Some(stopped) = &failures.stopped {
            format!("Explain stopped: {}", stopped.describe())
        } else if let Some(target) = reexplained.as_ref().filter(|_| failures.failed == 0) {
            let file = target.path();
            let not_read = tally.unread.iter().any(|p| p == file)
                || tally.dropped.iter().any(|(p, _)| p == file);
            match tally.regenerated {
                // Its file could not be read, or can no longer be explained:
                // the note below says so.
                0 if not_read => "Nothing re-explained".to_string(),
                0 => "Nothing to re-explain: it is no longer part of the project".to_string(),
                1 => "Re-explained".to_string(),
                paid => format!("Re-explained, and the {} summaries that quote it", paid - 1),
            }
        } else {
            let mut status = format!("Explained {n} functions/files/folders");
            if failures.failed > 0 {
                let why: Vec<String> = failures.calls.iter().map(|f| f.describe()).collect();
                status.push_str(&format!(
                    " · {} failed: {}",
                    failures.failed,
                    why.join("; ")
                ));
            }
            if tally.unquoted > 0 {
                status.push_str(&format!(
                    " · {} explained without the summaries that failed",
                    tally.unquoted
                ));
            }
            if !tally.waiting.is_empty() {
                status.push_str(&format!(
                    " · {} left for the next refresh",
                    tally.waiting.len()
                ));
            }
            let mut left = Vec::new();
            if tally.outdated > 0 {
                left.push(format!(
                    "{} written with an older prompt, kept",
                    tally.outdated
                ));
            }
            if tally.unverified > 0 {
                left.push(format!(
                    "{} from an earlier clew, kept unchecked",
                    tally.unverified
                ));
            }
            if tally.unexplained > 0 {
                left.push(format!("{} not explained yet", tally.unexplained));
            }
            if !left.is_empty() {
                status.push_str(&format!(
                    " · {} — Refresh All brings them up to date",
                    left.join(" · ")
                ));
            }
            status
        };
        // Named where they hold something: a file that could not be read and
        // holds no summary loses nothing by it.
        let held: Vec<String> = tally.held.iter().map(|p| self.shown_path(p)).collect();
        match held.len() {
            0 => {}
            1 => status.push_str(&format!(
                " · 1 file could not be read ({}) — its summaries are kept until it can be",
                held[0]
            )),
            files => status.push_str(&format!(
                " · {files} files could not be read ({}) — their summaries are kept until they \
                 can be",
                some_of(&held)
            )),
        }
        // Dropped only by a pass that ran to its end.
        if completed && let Some(note) = self.dropped_note(&tally.dropped) {
            status.push_str(&format!(" · {note}"));
        }
        self.status = status;

        // Propagate the refreshed summaries to the downstream artifacts already in
        // use, each guarded so an unchanged input stays cheap. These run in the
        // background — they never switch the user's view. After an automatic
        // pass that paid for nothing — every change was to how prompts are
        // rendered, not to what they describe — nothing downstream changed
        // either: the overview is not re-billed for a new prompt wording.
        let mut tasks = vec![persist];
        let downstream = !automatic || tally.regenerated > 0;
        if downstream && self.embed_available && !self.proj.explain.cache.is_empty() {
            tasks.push(Task::done(Message::Semantic(SemanticMsg::BuildIndex)));
        }
        if downstream && self.proj.overview.markdown.is_some() && self.overview_inputs_changed() {
            tasks.push(Task::done(Message::Overview(OverviewMsg::Generate)));
        }
        if self.proj.refresh_pending {
            tasks.push(self.request_auto_refresh());
        }
        if let Some(node) = self.proj.explain.view.clone() {
            let fresh_detail = self
                .proj
                .explain
                .showing_detail
                .then(|| {
                    self.proj
                        .explain
                        .cache
                        .get(&node)
                        .and_then(|c| c.detail.clone())
                })
                .flatten();
            tasks.push(match fresh_detail {
                Some(detail) => self.show_detail(node, detail),
                None => self.show_explanation(node),
            });
        }
        Task::batch(tasks)
    }

    /// What the status line says of the files whose summaries a pass drops
    /// because they can no longer be explained (`explain::Tally::dropped`):
    /// each by name — a few of several — with why.
    fn dropped_note(&self, dropped: &[(PathBuf, explain::Unexplainable)]) -> Option<String> {
        use explain::Unexplainable::{Refused, TooLarge};
        let size = |bytes: u64| crate::ui::human_size(bytes);
        match dropped {
            [] => None,
            [(path, TooLarge(bytes))] => Some(format!(
                "{} is too large to explain ({}) — its summaries are dropped",
                self.shown_path(path),
                size(*bytes)
            )),
            [(path, Refused(why))] => Some(format!(
                "{} {} — its summaries are dropped",
                self.shown_path(path),
                refusal_words(*why).0
            )),
            several => {
                let named: Vec<String> = several
                    .iter()
                    .map(|(path, why)| match why {
                        TooLarge(bytes) => {
                            format!("{} (too large, {})", self.shown_path(path), size(*bytes))
                        }
                        Refused(why) => {
                            format!("{} ({})", self.shown_path(path), refusal_words(*why).1)
                        }
                    })
                    .collect();
                Some(format!(
                    "{} files can no longer be explained ({}) — their summaries are dropped",
                    several.len(),
                    some_of(&named)
                ))
            }
        }
    }

    /// `path` as the status line names a file: relative to the project.
    fn shown_path(&self, path: &Path) -> String {
        let root = self.proj.project.as_ref().map(|p| p.root.as_path());
        root.and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    pub(crate) fn request_auto_refresh(&mut self) -> Task<Message> {
        if !self.llm_available || self.proj.explain.cache.is_empty() {
            return Task::none();
        }
        // Let any running pass finish, then re-check on completion / next tick.
        if self.proj.explain.running
            || self.proj.overview.generating
            || self.proj.building_embeddings
        {
            self.proj.refresh_pending = true;
            return Task::none();
        }
        let cooled = self
            .proj
            .last_auto_refresh
            .map(|t| t.elapsed() >= AUTO_REFRESH_MIN_INTERVAL)
            .unwrap_or(true);
        if cooled {
            self.begin_refresh(true)
        } else {
            self.proj.refresh_pending = true;
            Task::none()
        }
    }

    /// Begin a refresh pass now, resetting the cooldown. Shared by the auto path
    /// (`automatic`: a source change started it) and the manual force-refresh.
    /// The explain pass is cache-aware (only changed nodes hit the LLM — for
    /// an automatic one, only nodes whose code changed, and what quotes
    /// them); on completion it chains the semantic index and overview when
    /// those already exist (see `ExplainMsg::Done`).
    pub(crate) fn begin_refresh(&mut self, automatic: bool) -> Task<Message> {
        self.proj.last_auto_refresh = Some(std::time::Instant::now());
        self.proj.refresh_pending = false;
        Task::done(Message::Explain(if automatic {
            ExplainMsg::Refresh
        } else {
            ExplainMsg::Project
        }))
    }

    pub(crate) fn on_refresh_all(&mut self) -> Task<Message> {
        if let Err(ask_for_key) = self.require_llm() {
            return ask_for_key;
        }
        // Already refreshing — let it finish (the chip is disabled too).
        if self.proj.explain.running
            || self.proj.overview.generating
            || self.proj.building_embeddings
        {
            return Task::none();
        }
        // Manual: bypass the 30s cooldown entirely.
        self.status = "Refreshing…".into();
        self.begin_refresh(false)
    }

    /// The explanation target for the active pane's caret: the innermost
    /// function/method it sits in, or the file itself when it's between
    /// functions. Drives the always-on explanation panel.
    pub(crate) fn cursor_target(&self) -> Option<explain::Node> {
        let v = self.active_viewer()?;
        let line1 = v.caret.map(|(l, _)| l + 1)?;
        let sym = v
            .symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
            .filter(|s| s.line <= line1 && line1 <= s.end_line)
            .min_by_key(|s| s.end_line.saturating_sub(s.line));
        Some(match sym {
            Some(sym) => explain::Node::Function {
                file: v.abs.clone(),
                name: sym.name.clone(),
                ordinal: outline::fn_ordinal(&v.symbols, sym),
            },
            None => explain::Node::File(v.abs.clone()),
        })
    }

    /// Show the pre-built explanation for the innermost function/method whose
    /// span contains `line1` (1-based) in `file`. Used by the Outline Cmd+click
    /// and the code context menu. Everything is explained at project startup, so
    /// this is a pure show — no on-demand generation.
    pub(crate) fn explain_symbol_at(&mut self, file: PathBuf, line1: usize) -> Task<Message> {
        self.show_right_panel = true; // explicit action → reveal the panel
        let found = self
            .proj
            .panes
            .iter()
            .flatten()
            .find(|v| v.abs == file)
            .and_then(|v| {
                v.symbols
                    .iter()
                    .filter(|s| matches!(s.kind.as_str(), "function" | "method"))
                    .filter(|s| s.line <= line1 && line1 <= s.end_line)
                    .min_by_key(|s| s.end_line.saturating_sub(s.line)) // innermost span
                    .map(|s| (s.name.clone(), outline::fn_ordinal(&v.symbols, s)))
            });
        match found {
            Some((name, ordinal)) => {
                let node = explain::Node::Function {
                    file,
                    name,
                    ordinal,
                };
                // Reveal the panel now (a cached summary, or the placeholder),
                // then generate the block walkthrough. Without the second step a
                // menu / Cmd+click "Explain" just parked the panel on "Not
                // explained yet" and looked dead; ExplainMsg::Blocks shows a cached
                // walkthrough if present, else streams a fresh one.
                let show = self.show_explanation(node.clone());
                Task::batch([show, Task::done(Message::Explain(ExplainMsg::Blocks(node)))])
            }
            None => {
                self.status = "No function here to explain".into();
                Task::none()
            }
        }
    }

    /// Open the explanation overlay for `node`, showing its summary.
    pub(crate) fn show_explanation(&mut self, node: explain::Node) -> Task<Message> {
        let text = explanation_text(self.proj.explain.cache.get(&node));
        self.present(node, &text, false)
    }

    /// Show a function's block-by-block walkthrough (`detail`) in the overlay.
    pub(crate) fn show_detail(&mut self, node: explain::Node, detail: String) -> Task<Message> {
        self.present(node, &detail, true)
    }

    /// Prepare `content` (an LLM markdown string) into ordered segments — markdown
    /// pre-parsed, math/mermaid keyed — load any already-rendered SVGs from the
    /// session/disk cache, and kick off a background pass to render the rest.
    pub(crate) fn present(
        &mut self,
        node: explain::Node,
        content: &str,
        detail: bool,
    ) -> Task<Message> {
        let (prepared, task) = self.prepare_segments(content);
        self.proj.explain.prepared = prepared;
        self.proj.explain.view = Some(node);
        self.proj.explain.showing_detail = detail;
        // The call-flow strip needs the project call graph; build it lazily while
        // the reader is actually looking at a function in the context panel.
        let build = if self.show_right_panel
            && matches!(self.proj.explain.view, Some(explain::Node::Function { .. }))
        {
            self.ensure_call_graph()
        } else {
            Task::none()
        };
        Task::batch([task, build])
    }

    pub(crate) fn on_explain_from_menu(&mut self) -> Task<Message> {
        let Some(menu) = self.proj.context_menu.take() else {
            return Task::none();
        };
        let file = self
            .proj
            .panes
            .get(menu.pane)
            .and_then(Option::as_ref)
            .map(|v| v.abs.clone());
        match file {
            // menu.line is 0-based; explain_symbol_at wants 1-based.
            Some(file) => self.explain_symbol_at(file, menu.line + 1),
            None => Task::none(),
        }
    }

    /// Re-explain the node in the open panel: a pass that pays for this node
    /// and for what quotes its summary, and keeps every other entry as it is
    /// (`explain::Reuse::Node`) — it used to run Explain All, which re-billed
    /// every summary an older prompt wrote. Only when the node is already
    /// explained: point the user at the explicit Explain All otherwise.
    pub(crate) fn on_reexplain_node(&mut self) -> Task<Message> {
        let Some(node) = self.proj.explain.view.clone() else {
            return Task::none();
        };
        if let Err(ask_for_key) = self.require_llm() {
            return ask_for_key;
        }
        if !self.proj.explain.cache.contains_key(&node) {
            self.status =
                "Nothing to re-explain yet — run Explain in the toolbar to explain the project first.".into();
            return Task::none();
        }
        // A second pass is refused while one runs (`start_explain_pass`):
        // said in this flow's own words, to ask again afterwards.
        if self.proj.explain.running {
            self.status = "Explain All is running — re-explain once it finishes".into();
            return Task::none();
        }
        let pass = self.start_explain_pass(PassKind::Node(node));
        if self.proj.explain.running {
            self.status = "Re-explaining…".into();
        }
        pass
    }

    /// Generate (or show the cached) block-by-block walkthrough for a function.
    pub(crate) fn on_explain_blocks(&mut self, node: explain::Node) -> Task<Message> {
        let stamp = self.stamp();
        let explain::Node::Function {
            file,
            name,
            ordinal,
        } = node.clone()
        else {
            return Task::none(); // block detail only applies to functions
        };
        // Already generated? Show the cached walkthrough immediately.
        if let Some(detail) = self
            .proj
            .explain
            .cache
            .get(&node)
            .and_then(|c| c.detail.clone())
        {
            return self.show_detail(node, detail);
        }
        let cfg = match self.require_llm() {
            Ok(cfg) => cfg,
            Err(ask_for_key) => return ask_for_key,
        };
        // Unique-name → summary map so the off-thread gather can attach callee
        // context (ambiguous names resolve to None and are skipped).
        let mut summaries: HashMap<String, Option<String>> = HashMap::new();
        for (n, c) in self.proj.explain.cache.iter() {
            if let explain::Node::Function { name: fname, .. } = n {
                summaries
                    .entry(fname.clone())
                    .and_modify(|e| *e = None)
                    .or_insert_with(|| Some(c.summary.clone()));
            }
        }
        self.status = "Explaining blocks…".into();
        let ai = self.ai_client();
        // Remote: the function's source comes over the protocol, never from
        // a same-pathed local file.
        let remote_rel = (!self.local_project_state())
            .then(|| self.proj.project.as_ref().map(|p| p.root.clone()))
            .flatten()
            .and_then(|root| {
                file.strip_prefix(&root)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            });
        // The local gather reads this disk, so it needs the project root to
        // confine that read to.
        let root = self.proj.project.as_ref().map(|p| p.root.clone());
        let model = cfg.model.clone();
        Task::perform(
            async move {
                let prompt = match remote_rel {
                    Some(rel) => match ai
                        .request(clew_protocol::Request::ReadSources { rels: vec![rel] })
                        .await
                    {
                        Ok(clew_protocol::Event::Sources { files, .. }) if !files.is_empty() => {
                            let (_, content) = &files[0];
                            match gather_fn_detail_from(&file, content, &name, ordinal, &summaries)
                            {
                                Some((sig, body, callees)) => {
                                    Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                                }
                                None => Err("function body not found".to_string()),
                            }
                        }
                        _ => Err("could not read the remote source".to_string()),
                    },
                    None => tokio::task::spawn_blocking(move || {
                        let Some(root) = root else {
                            return Err::<String, String>("no project open".to_string());
                        };
                        let Some((sig, body, callees)) =
                            gather_fn_detail_input(&root, file, &name, ordinal, &summaries)
                        else {
                            return Err::<String, String>("function body not found".to_string());
                        };
                        Ok(explain::detail_prompt(&name, &sig, &body, &callees))
                    })
                    .await
                    .unwrap_or_else(|_| Err("task join failed".into())),
                };
                match prompt {
                    // A failed call says what failed and what to do, as a
                    // pass does (`ExplainFailure`).
                    Ok(p) => ai
                        .complete_typed(cfg, EXPLAIN_BLOCKS_SYSTEM, p, 1024)
                        .await
                        .map_err(|e| ExplainFailure::of_call(&e, &model).describe()),
                    Err(e) => Err(e),
                }
            },
            move |detail| {
                Message::Explain(ExplainMsg::BlocksExplained {
                    stamp: stamp.clone(),
                    node: node.clone(),
                    detail,
                })
            },
        )
    }

    /// Persist a generated block walkthrough and show it if still on that node.
    pub(crate) fn on_blocks_explained(
        &mut self,
        node: explain::Node,
        detail: Result<String, String>,
    ) -> Task<Message> {
        match detail {
            Ok(md) => {
                // Persist the walkthrough alongside the summary (dropped
                // automatically when the entry is regenerated).
                let persist = match self.proj.explain.cache.get(&node).map(|c| c.identity()) {
                    Some(identity) => {
                        if let Some(c) = self.explain_cache_mut().get_mut(&node) {
                            c.detail = Some(md.clone());
                        }
                        // Saved on its own, beside that same summary: writing
                        // this window's whole cache back to store ONE block
                        // walkthrough dropped every summary another window had
                        // added since this one loaded the file.
                        self.proj
                            .explain
                            .unsaved
                            .entry(node.clone())
                            .or_insert(Some(identity));
                        self.persist_explain()
                    }
                    None => Task::none(),
                };
                self.status = "Explained blocks".into();
                // Only swap the view if the user is still on this node.
                if self.proj.explain.view.as_ref() == Some(&node) {
                    return Task::batch([persist, self.show_detail(node, md)]);
                }
                persist
            }
            Err(e) => {
                self.status = format!("Block explanation failed: {e}");
                Task::none()
            }
        }
    }
}

/// Most nodes a window remembers as having waited ([`note_waited`]).
const MAX_WAITED: usize = 65_536;

/// Fold what a pass left waiting (`waiting`) into what has waited this
/// session (`ExplainState::waited`), which does not wait again: the next pass
/// pays for it without a summary that failed again (`explain::Pass`). What
/// the pass wrote (`written`) — paid for, stamped, or gone — is done
/// waiting, and may wait again for a failure to come. Kept across passes:
/// replaced by each
/// one that completed, it was emptied by a re-explain or an Explain All,
/// which leave nothing waiting, and what had waited for a call that kept
/// failing waited again. Bounded: past [`MAX_WAITED`] it starts over, and
/// what it forgot may wait once more.
fn note_waited<'a>(
    waited: &mut HashSet<explain::Node>,
    written: impl Iterator<Item = &'a explain::Node>,
    waiting: &[explain::Node],
) {
    for node in written {
        waited.remove(node);
    }
    if waited.len() + waiting.len() > MAX_WAITED {
        waited.clear();
    }
    waited.extend(waiting.iter().cloned());
}

/// Fold `newer` changes into `older` ones: a node in both keeps its base in
/// `older`, which names what the store held when it was first asked to be
/// saved. Folded in the smaller of the two maps: after an Explain All a
/// pass's changes are the whole project, and adding them entry by entry to
/// an empty map was that much work on the thread that serves every window.
fn fold_changes(older: &mut explain::Unsaved, mut newer: explain::Unsaved) {
    if older.len() >= newer.len() {
        for (node, base) in newer {
            older.entry(node).or_insert(base);
        }
    } else {
        newer.extend(std::mem::take(older));
        *older = newer;
    }
}

/// Run the saves of `chain` one at a time — `first`, then each asked for
/// while one ran — into the derived store at `store`, off the UI thread. A
/// save that failed goes on with the next one, each change against what it
/// replaced then; with none after it, it waits in `chain` for the window
/// (`SaveQueue::left`). The last save's sequence number, the store as it
/// merged, and whether it was saved: what `ExplainMsg::Persisted` reports.
///
/// Every save asked for runs, whatever becomes of the window that asked:
/// the queue is this task's, and a window gone — closed, its project or
/// connection switched — only no longer hears how it ended.
async fn run_saves(
    store: PathBuf,
    root: PathBuf,
    first: crate::app::state::QueuedSave,
    chain: crate::app::state::SaveChain,
) -> (u64, explain::Cache, Result<usize, String>) {
    let mut save = first;
    loop {
        let crate::app::state::QueuedSave { changes, mine, seq } = save;
        // Shared with the save rather than moved into it: the changes come
        // back whole, returned or unwound, for a save that failed.
        let changes = Arc::new(changes);
        let (store_now, root_now, for_save) = (store.clone(), root.clone(), changes.clone());
        let (merged, saved) = tokio::task::spawn_blocking(move || {
            let (merged, saved) = explain::edit(&store_now, &root_now, |disk| {
                explain::merge_unsaved(disk, &mine, &for_save);
            });
            (merged, saved.map_err(|e| e.to_string()))
        })
        .await
        .unwrap_or_else(|e| (explain::Cache::new(), Err(e.to_string())));
        let changes = Arc::try_unwrap(changes).unwrap_or_else(|shared| (*shared).clone());
        let mut queue = chain.lock();
        if saved.is_err() {
            let mut carried = changes;
            match &mut queue.next {
                Some(next) => {
                    fold_changes(&mut carried, std::mem::take(&mut next.changes));
                    next.changes = carried;
                }
                None => {
                    fold_changes(&mut carried, std::mem::take(&mut queue.left));
                    queue.left = carried;
                }
            }
        }
        match queue.next.take() {
            Some(next) => save = next,
            None => {
                queue.open = false;
                return (seq, merged, saved);
            }
        }
    }
}

/// What the status line says a file is that is not a plain UTF-8 text file
/// of the project: in a sentence, and in a list. Each as what it is: a
/// Latin-1 file and a link out of the project were both "not a text file".
fn refusal_words(why: explain::Refusal) -> (&'static str, &'static str) {
    match why {
        explain::Refusal::NotUtf8 => ("is not UTF-8 text", "not UTF-8"),
        explain::Refusal::NotPlainFile => ("is not a plain file", "not a plain file"),
        explain::Refusal::OutsideProject => ("resolves outside the project", "outside the project"),
    }
}

/// Most files the status line names in one note; the rest are counted.
const NAMED_FILES: usize = 3;

/// `names` as the status line lists them: the first few, and how many more.
fn some_of(names: &[String]) -> String {
    let shown = names
        .iter()
        .take(NAMED_FILES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match names.len().saturating_sub(NAMED_FILES) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// What the explanation overlay shows for a node's entry: its summary — with
/// a word before it when it was kept from an earlier clew unchecked
/// (`explain::Basis::unchecked`), so it never reads as current — or how to
/// get one.
pub(crate) fn explanation_text(cached: Option<&explain::Cached>) -> String {
    match cached {
        Some(c) if is_unchecked(c) => format!(
            "*Written by an earlier version of clew, and not checked against the code \
             since — Refresh All explains it again.*\n\n{}",
            c.summary
        ),
        Some(c) => c.summary.clone(),
        None => "Not explained yet — press Explain in the toolbar.".to_string(),
    }
}

/// Whether `cached` was kept from an earlier clew without being checked
/// against the code (`explain::Basis::unchecked`).
pub(crate) fn is_unchecked(cached: &explain::Cached) -> bool {
    cached.basis.is_some_and(|b| b.unchecked)
}

/// What marks such a summary wherever a short view shows it.
pub(crate) const UNCHECKED_MARK: &str = "(unchecked)";

/// A summary as a short view shows it — the hover, the file banner, the
/// outline, a list of results, callers or contents — trimmed, and marked
/// when it was kept unchecked ([`is_unchecked`]): it must not read as
/// current there any more than in the overlay ([`explanation_text`]).
pub(crate) fn shown_summary(cached: &explain::Cached) -> std::borrow::Cow<'_, str> {
    let summary = cached.summary.trim();
    if is_unchecked(cached) {
        format!("{UNCHECKED_MARK} {summary}").into()
    } else {
        summary.into()
    }
}

impl App {
    /// Handle a [`ExplainMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_explain(&mut self, message: ExplainMsg) -> Task<Message> {
        match message {
            ExplainMsg::FromMenu => self.on_explain_from_menu(),
            ExplainMsg::Project => self.on_explain_project(false),
            ExplainMsg::Refresh => self.on_explain_project(true),
            ExplainMsg::Cancel => self.on_cancel_explain(),
            ExplainMsg::Progress {
                generation,
                done,
                total,
                failed,
                ..
            } => {
                if generation == self.proj.explain.generation {
                    self.proj.explain.progress = Some((done, total));
                    self.proj.explain.failed = failed;
                }
                Task::none()
            }
            ExplainMsg::Done {
                generation,
                cache,
                failures,
                tally,
                written,
                ..
            } => self.on_explain_done(generation, cache, failures, *tally, written),
            ExplainMsg::RefreshAll => self.on_refresh_all(),
            ExplainMsg::Show(node) => {
                self.show_right_panel = true;
                self.show_explanation(node)
            }
            ExplainMsg::ReexplainNode => self.on_reexplain_node(),
            ExplainMsg::Blocks(node) => self.on_explain_blocks(node),
            ExplainMsg::BlocksExplained { node, detail, .. } => {
                self.on_blocks_explained(node, detail)
            }
            ExplainMsg::Persisted {
                seq,
                merged,
                saved,
                chain,
                ..
            } => self.on_explain_persisted(chain, seq, merged.take(), saved),
        }
    }
}

/// A fault a unit test injects into the start of a pass: the task reading
/// the project panics, as a bug would make it. Keyed by root, so tests
/// running in parallel never trip each other's.
#[cfg(test)]
pub(crate) mod gather_faults {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static ARMED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    /// Make the next pass over `root` fail to read it.
    pub(crate) fn arm(root: &Path) {
        ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(root.to_path_buf());
    }

    /// Panic when armed for `root` (the lock is released first).
    pub(crate) fn hit(root: &Path) {
        let armed = ARMED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|r| r == root);
        if armed {
            panic!("injected fault reading {}", root.display());
        }
    }
}
