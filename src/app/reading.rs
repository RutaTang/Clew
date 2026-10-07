//! The reading session a reader builds up in a project: the navigation trail,
//! bookmarks (with notes), per-symbol reading notes, and the `#[cfg]` reading
//! target — kept in memory and persisted locally or on the remote host.
//!
//! Its messages, [`ReadingMsg`], arrive through `App::update_reading`.

use crate::app::prelude::*;
use crate::*;

/// Write one trail on the blocking pool (`history::save_text`: under the
/// store's lock, after its replaceability check), reporting as
/// `ReadingMsg::TrailSaved`.
fn write_trail((root, text): TrailWrite) -> Task<Message> {
    Task::perform(
        async move {
            let task_root = root.clone();
            let result = tokio::task::spawn_blocking(move || {
                history::save_text(&task_root, text.as_deref()).map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(format!("the write failed unexpectedly: {e}")));
            (root, result)
        },
        |(root, result)| Message::Reading(ReadingMsg::TrailSaved { root, result }),
    )
}

impl App {
    /// Persist the navigation tree to the project's `.clew/` — on the local
    /// disk, or over the protocol for a remote project. A local write that
    /// fails is reported in the status line; the trail is kept for the
    /// session either way.
    ///
    /// Whole-tree write on purpose: with the project open in two windows the
    /// one that navigated last owns the stored trail (see
    /// `history::save_text` for why merging two readers' trees would be worse
    /// than that).
    ///
    /// Locally the trail is ENCODED here, where it lives — cheap — and
    /// WRITTEN on the blocking pool by the window's serial writer
    /// ([`TrailWriter`]): the write (a lock, the replaceability check, a
    /// sync and a rename) ran on this thread on every navigation. One write
    /// at a time, in order, and only the newest trail waits behind the one
    /// running. The returned task is that write, when one starts.
    pub(crate) fn save_history(&mut self) -> Task<Message> {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let text = history::to_text(&root, &self.proj.history);
        if !self.local_project_state() {
            self.write_remote_state("history.json", text);
            return Task::none();
        }
        // While the project's stored trail is still being read, what is in
        // memory is only the visits made since it opened: writing it would
        // replace the stored trail with them. They are replayed onto the
        // stored trail when it lands, and saved from there
        // (`on_project_state_loaded`).
        if self.proj.inflight.state_loading || self.proj.inflight.state_unread {
            return Task::none();
        }
        match self.trail_writer.submit((root, text)) {
            Some(write) => write_trail(write),
            None => Task::none(),
        }
    }

    /// A write of a trail finished: report a failure (an unreadable
    /// history.json is left alone, never overwritten — and said so), and
    /// start the next waiting write.
    pub(crate) fn on_trail_saved(
        &mut self,
        root: PathBuf,
        result: Result<(), String>,
    ) -> Task<Message> {
        if let Err(e) = result {
            let open = self.proj.project.as_ref().is_some_and(|p| p.root == root);
            self.status = if open {
                format!("Reading trail not saved: {e}")
            } else {
                format!("Reading trail of {} not saved: {e}", root.display())
            };
        }
        match self.trail_writer.finished() {
            Some(write) => write_trail(write),
            None => Task::none(),
        }
    }

    /// Drop the trail's collapsed-branch marks once the history's node ids no
    /// longer mean what they meant when the marks were made: the oldest
    /// visits evicted at the cap renumber every survivor, and a clear empties
    /// the tree. Checked here, after every message, because the eviction
    /// happens inside `History::push`, which every navigation reaches; left
    /// standing, the marks fold unrelated nodes of the trail on screen. (A
    /// trail REPLACED wholesale — loaded, adopted from the remote host —
    /// clears the marks where it is replaced.)
    pub(crate) fn sync_trail_collapsed(&mut self) {
        let stamp = self.proj.history.renumbering();
        if stamp != self.proj.trail_renumbering {
            self.proj.trail_renumbering = stamp;
            self.proj.trail_collapsed.clear();
        }
    }

    pub(crate) fn on_bookmark_toggled(&mut self) -> Task<Message> {
        let line_height = self.line_height();
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return Task::none();
        };
        let Some(v) = self.active_viewer() else {
            return Task::none();
        };
        let line = v.current_line(line_height);
        let mut preview = v.line_text(line).trim().to_string();
        if preview.chars().count() > 80 {
            preview = preview.chars().take(80).collect();
        }
        let rel = v.rel.clone();
        // A remote project's bookmarks persist over the protocol, where the
        // project lives — never in a same-pathed local .clew.
        let mut added = false;
        let saved = if self.local_project_state() {
            // Toggle against what is on disk now, not against this window's
            // open-time snapshot: a second window on the same project has been
            // writing the same file, and a wholesale write of our copy erased
            // everything it added. Adopt the merged list so this window stops
            // rendering a copy that disagrees with disk.
            // A store that cannot be read is never overwritten; the toggle is
            // then applied to this window's own list, so the other bookmarks
            // stay on screen instead of vanishing behind an empty one.
            let (merged, saved) =
                bookmarks::edit_with_fallback(&root, &self.proj.bookmarks, |list| {
                    added = bookmarks::toggle(list, &rel, line, preview)
                });
            // Adopted even when the write failed: the toggle is what the user
            // just did, and reverting to the pre-toggle list would make the
            // gutter disagree with the click as well as with disk.
            self.proj.bookmarks = merged;
            self.touch_project_state(bookmarks::REL);
            saved
        } else {
            // The same toggle, replayed by the SERVER on the remote file: this
            // window's list is its copy from project open, and shipping it
            // wholesale deleted every bookmark another client had added since.
            // Applied here as well so the gutter answers the click without
            // waiting for the round trip; the merged file replaces it when it
            // lands (`StateEdited`). A change the journal refused is not made
            // here either (the status line says why).
            let merge = bookmarks::merge_toggle(&rel, line, preview.clone());
            if !self.edit_remote_state(bookmarks::REL, merge) {
                return Task::none();
            }
            added = bookmarks::toggle(&mut self.proj.bookmarks, &rel, line, preview);
            Ok(())
        };
        self.status = match saved {
            Ok(()) if added => format!("Bookmarked {rel}:{line}"),
            Ok(()) => format!("Removed bookmark {rel}:{line}"),
            Err(e) => {
                format!("Cannot write .clew/bookmarks.json: {e} — kept for this session, not saved")
            }
        };
        Task::none()
    }

    pub(crate) fn on_bookmark_removed(&mut self, rel: &str, line: usize) -> Task<Message> {
        // Checked before the branch: with no project open the remote arm
        // would write this window's leftover list into whichever project the
        // server has, replacing its bookmarks with ours.
        if self.proj.project.is_none() {
            return Task::none();
        }
        let Some(idx) = self
            .proj
            .bookmarks
            .iter()
            .position(|b| b.rel == rel && b.line == line)
        else {
            return Task::none();
        };
        let gone = self.proj.bookmarks[idx].clone();
        if !self.local_project_state() {
            // Identity, not index, remotely too: an index points into this
            // window's snapshot, and the server removes from a file another
            // client may have inserted into (bookmarks are kept sorted), which
            // shifts every entry after the insertion point.
            let merge = bookmarks::merge_remove(&gone.rel, gone.line);
            if self.edit_remote_state(bookmarks::REL, merge) {
                self.proj.bookmarks.remove(idx);
            }
        } else if let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) {
            // Identity, not index: an index points into this window's
            // snapshot, and the merge below re-reads a list another window may
            // have inserted into (bookmarks are kept sorted), which shifts
            // every entry after the insertion point.
            let (merged, saved) =
                bookmarks::edit_with_fallback(&root, &self.proj.bookmarks, |list| {
                    list.retain(|b| !(b.rel == gone.rel && b.line == gone.line))
                });
            // Adopted on failure too: the removal is the user's own action, so
            // the list stays as they left it for the session. Only the write
            // is lost, and the status line says so rather than the entry
            // silently reappearing on the next render.
            self.proj.bookmarks = merged;
            self.touch_project_state(bookmarks::REL);
            if let Err(e) = saved {
                self.status =
                    format!("Cannot write .clew/bookmarks.json: {e} — removed for this session");
            }
        }
        Task::none()
    }

    pub(crate) fn on_bookmark_note_edit(&mut self, rel: String, line: usize) -> Task<Message> {
        let existing = self
            .proj
            .bookmarks
            .iter()
            .find(|b| b.rel == rel && b.line == line)
            .and_then(|b| b.note.clone())
            .unwrap_or_default();
        self.proj.note_edit = Some((rel, line, existing));
        // The note editor's input takes the keyboard.
        self.code_focused = false;
        operation::focus(ui::note_input_id())
    }

    pub(crate) fn on_bookmark_note_save(&mut self) -> Task<Message> {
        if self.proj.project.is_none() {
            return Task::none();
        }
        if let Some((rel, line, draft)) = self.proj.note_edit.take() {
            if !self.local_project_state() {
                // Only the note field travels, so a bookmark another client
                // added — and everything else in the file — survives the save.
                let merge = bookmarks::merge_note(&rel, line, Some(draft.clone()));
                if self.edit_remote_state(bookmarks::REL, merge) {
                    bookmarks::set_note(&mut self.proj.bookmarks, &rel, line, Some(draft));
                } else {
                    // Refused (the status line says why): the editor stays
                    // open on what was typed, to be saved again.
                    self.proj.note_edit = Some((rel, line, draft));
                }
            } else if let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) {
                // The note is attached on the list read here, so it survives
                // another window's edits instead of being written back as part
                // of this window's whole (stale) snapshot.
                let (merged, saved) =
                    bookmarks::edit_with_fallback(&root, &self.proj.bookmarks, |list| {
                        bookmarks::set_note(list, &rel, line, Some(draft))
                    });
                self.touch_project_state(bookmarks::REL);
                // Adopted whether or not the write landed: `note_edit` was
                // taken above, so the typed note lives ONLY in the merged
                // list. Dropping it on an unwritable `.clew/` deleted what the
                // user had just written, with the editor already closed.
                //
                // Not fully closed: the note is attached to the list just READ
                // from disk, so on a store that cannot be read either (`.clew`
                // shipped as a symlink, or replaced by a file) a bookmark that
                // only ever existed in this window's memory is not there to
                // attach it to, and the text is still lost. Fixing that means
                // merging in memory, which cannot tell a bookmark this window
                // deleted from one another window just added.
                self.proj.bookmarks = merged;
                if let Err(e) = saved {
                    self.status = format!(
                        "Cannot write .clew/bookmarks.json: {e} — kept for this session, not saved"
                    );
                }
            }
        }
        Task::none()
    }

    /// Apply one change to the reading notes and persist it.
    ///
    /// The change is handed down rather than applied to `self.notes` first,
    /// because locally it is replayed on the list read under the store lock:
    /// `self.notes` is this window's copy from project open, so writing it
    /// wholesale erased every note a second window on the same project had
    /// written since — invisibly, since each window kept rendering its own copy
    /// until the next launch (see `notes::edit`). The merged list is adopted so
    /// this window stops disagreeing with disk.
    ///
    /// Remotely the same change goes out as `merge`, which the SERVER replays
    /// on the file — the only place both clients' writes are visible. The
    /// local change still runs on `self.notes` there, so the UI does not wait
    /// for the round trip; the merged file that comes back replaces it.
    ///
    /// Returns false when the change was not made: no project, or a remote
    /// change the journal refused (`edit_remote_state`, whose status line
    /// says why).
    #[must_use = "a caller that took the user's draft must give it back when the change was not made"]
    pub(crate) fn edit_notes(
        &mut self,
        merge: clew_protocol::StateMerge,
        change: impl FnOnce(&mut Vec<notes::Note>),
    ) -> bool {
        let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
            return false;
        };
        if !self.local_project_state() {
            if !self.edit_remote_state(notes::REL, merge) {
                return false;
            }
            change(&mut self.proj.notes);
            return true;
        }
        // The merged list is adopted whether or not the write landed. The
        // caller has already taken the user's draft (`NoteEditSave` empties
        // `reading_note_edit` before getting here), so on an unwritable
        // `.clew/` — a read-only checkout, a full disk — dropping it deleted
        // prose that existed nowhere else, the moment the user pressed save.
        // Kept in memory it is still readable and re-savable this session;
        // only the persistence failed, and the status line says exactly that.
        // A store that cannot be read is never overwritten: the change is then
        // applied to this window's own list, so the other notes stay on screen.
        let (merged, saved) = notes::edit_with_fallback(&root, &self.proj.notes, change);
        self.proj.notes = merged;
        self.touch_project_state(notes::REL);
        if let Err(e) = saved {
            self.status =
                format!("Cannot write .clew/notes.json: {e} — kept for this session, not saved");
        }
        true
    }

    /// The live 1-based line of a noted symbol, resolved against the current
    /// index — `None` when the symbol no longer exists (an orphaned note).
    pub fn note_symbol_line(&self, rel: &str, symbol: &str) -> Option<usize> {
        let root = &self.proj.project.as_ref()?.root;
        let abs = root.join(rel);
        self.proj
            .symbol_index_by_file
            .get(&abs)?
            .iter()
            .find(|s| s.name == symbol)
            .map(|s| s.line)
    }

    pub(crate) fn on_target_selected(&mut self, target: inactive::Target) -> Task<Message> {
        if self.proj.project.is_some()
            && !self.local_project_state()
            && !self.edit_remote_state("reading.toml", reading::merge_target(&target))
        {
            return Task::none();
        }
        self.proj.reading_target = target;
        self.show_tools_menu = false;
        self.show_target_menu = false;
        // The reading target is per-project state; with none open there is
        // nothing to persist it to, and the remote arm below would otherwise
        // write it into whichever project the server currently has.
        if self.proj.project.is_none() {
            return Task::none();
        }
        // Re-evaluate the cfg dimming for every open file.
        let t = self.proj.reading_target.clone();
        for v in self.proj.panes.iter_mut().flatten() {
            if let Some(lang) = v.lang_key {
                let src = v.source.clone();
                v.set_inactive_lines(inactive::inactive_lines(&src, lang, &t));
            }
        }
        if !self.local_project_state() {
            return Task::none();
        }
        // The user's pick is newer than whatever the project-state load is
        // still reading.
        self.touch_project_state("reading.toml");
        if let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone())
            && let Err(e) = reading::save_target(&root, &self.proj.reading_target)
        {
            self.status = format!("Could not save target: {e}");
        }
        Task::none()
    }

    /// The current reading target in its protocol wire form.
    pub(crate) fn target_spec(&self) -> clew_protocol::TargetSpec {
        clew_protocol::TargetSpec {
            label: self.proj.reading_target.label.clone(),
            os: self.proj.reading_target.os.clone(),
            arch: self.proj.reading_target.arch.clone(),
            family: self.proj.reading_target.family.clone(),
        }
    }
}

impl App {
    /// Handle a [`ReadingMsg`]: this feature's share of what `dispatch` routes
    /// (after its one ownership check and the menu bookkeeping).
    pub(crate) fn update_reading(&mut self, message: ReadingMsg) -> Task<Message> {
        match message {
            ReadingMsg::BookmarkToggled => self.on_bookmark_toggled(),
            ReadingMsg::BookmarkRemoved { rel, line } => self.on_bookmark_removed(&rel, line),
            ReadingMsg::BookmarkNoteEdit(rel, line) => self.on_bookmark_note_edit(rel, line),
            ReadingMsg::BookmarkNoteInput(s) => {
                if let Some((_, _, draft)) = &mut self.proj.note_edit {
                    *draft = s;
                }
                Task::none()
            }
            ReadingMsg::BookmarkNoteSave => self.on_bookmark_note_save(),
            ReadingMsg::BookmarkNoteCancel => {
                self.proj.note_edit = None;
                Task::none()
            }
            ReadingMsg::NoteToggleUnderstood { rel, symbol } => {
                // The flag the user is asking for is the opposite of the one
                // they can see, resolved HERE so BOTH arms carry that value
                // rather than replaying a flip against a file they may find in
                // a different state. The local arm is not the safe one: its
                // change is replayed inside `notes::edit` on the list read from
                // disk under the lock, so a flip there lands inverted whenever
                // a second window has already marked the same symbol.
                let want =
                    !notes::find(&self.proj.notes, &rel, &symbol).is_some_and(|n| n.understood);
                // Refused, nothing changed, and the status line says why.
                let _ = self.edit_notes(notes::merge_understood(&rel, &symbol, want), |list| {
                    notes::set_understood(list, &rel, &symbol, want);
                });
                Task::none()
            }
            ReadingMsg::NoteEditStart { rel, symbol } => {
                let existing = notes::find(&self.proj.notes, &rel, &symbol)
                    .map(|n| n.text.clone())
                    .unwrap_or_default();
                self.proj.reading_note_edit = Some((rel, symbol, existing));
                // The note editor's input takes the keyboard: motion keys must
                // type into it, not move the code cursor behind the modal.
                self.code_focused = false;
                operation::focus(ui::note_input_id())
            }
            ReadingMsg::NoteEditInput(s) => {
                if let Some((_, _, draft)) = &mut self.proj.reading_note_edit {
                    *draft = s;
                }
                Task::none()
            }
            ReadingMsg::NoteEditSave => {
                if let Some((rel, symbol, draft)) = self.proj.reading_note_edit.take() {
                    let saved = self.edit_notes(notes::merge_text(&rel, &symbol, &draft), |list| {
                        notes::set_text(list, &rel, &symbol, &draft)
                    });
                    if !saved {
                        // Not made (the status line says why): the editor
                        // stays open on what was typed, to be saved again.
                        self.proj.reading_note_edit = Some((rel, symbol, draft));
                    }
                }
                Task::none()
            }
            ReadingMsg::NoteEditCancel => {
                self.proj.reading_note_edit = None;
                Task::none()
            }
            ReadingMsg::NoteRemove { rel, symbol } => {
                // Refused, nothing changed, and the status line says why.
                let _ = self.edit_notes(notes::merge_remove(&rel, &symbol), |list| {
                    notes::remove(list, &rel, &symbol)
                });
                Task::none()
            }
            ReadingMsg::NoteJump { rel, symbol } => {
                let Some(root) = self.proj.project.as_ref().map(|p| p.root.clone()) else {
                    return Task::none();
                };
                let line = self.note_symbol_line(&rel, &symbol);
                self.open_file(root.join(&rel), line, true)
            }
            ReadingMsg::GoBack => match self.proj.history.back() {
                Some(loc) => {
                    let saved = self.save_history();
                    Task::batch([saved, self.open_file(loc.path, loc.line, false)])
                }
                None => Task::none(),
            },
            ReadingMsg::GoForward => match self.proj.history.forward() {
                Some(loc) => {
                    let saved = self.save_history();
                    Task::batch([saved, self.open_file(loc.path, loc.line, false)])
                }
                None => Task::none(),
            },
            // A click names the visit it drew: an id that no longer points
            // there (renumbered by an eviction, or a trail replaced since)
            // is dropped rather than jumping to whatever holds it now.
            ReadingMsg::HistoryJump { id, loc } => {
                if self.proj.history.loc(id) != Some(&loc) {
                    return Task::none();
                }
                match self.proj.history.goto(id) {
                    Some(loc) => {
                        let saved = self.save_history();
                        Task::batch([saved, self.open_file(loc.path, loc.line, false)])
                    }
                    None => Task::none(),
                }
            }
            ReadingMsg::TrailToggleCollapse { id, loc } => {
                if self.proj.history.loc(id) != Some(&loc) {
                    return Task::none();
                }
                if !self.proj.trail_collapsed.remove(&id) {
                    self.proj.trail_collapsed.insert(id);
                }
                Task::none()
            }
            ReadingMsg::HistoryClear => {
                self.proj.history.clear();
                // Collapse marks are node ids of the tree just cleared; left
                // standing they would fold unrelated nodes of the new trail.
                self.proj.trail_collapsed.clear();
                self.save_history()
            }
            ReadingMsg::TrailSaved { root, result } => self.on_trail_saved(root, result),
            ReadingMsg::TargetSelected(target) => self.on_target_selected(target),
        }
    }
}
